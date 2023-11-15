// Copyright 2022 The Blaze Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::common::onheap_spill::Spill;
use crate::common::output::output_with_sender;
use arrow::datatypes::SchemaRef;
use arrow::error::Result as ArrowResult;
use arrow::record_batch::RecordBatch;
use async_trait::async_trait;
use datafusion::common::Result;
use datafusion::error::DataFusionError;
use datafusion::execution::context::TaskContext;
use datafusion::physical_plan::metrics::BaselineMetrics;
use datafusion::physical_plan::{Partitioning, SendableRecordBatchStream};
use datafusion_ext_commons::array_builder::has_array_builder_supported;
use datafusion_ext_commons::spark_hash::{create_hashes, pmod};
use datafusion_ext_commons::streams::coalesce_stream::CoalesceStream;
use futures::StreamExt;
use std::sync::Arc;

pub mod bucket_repartitioner;
pub mod single_repartitioner;
pub mod sort_repartitioner;

mod rss;
pub mod rss_bucket_repartitioner;
pub mod rss_single_repartitioner;
pub mod rss_sort_repartitioner;

pub fn can_use_bucket_repartitioner(schema: &SchemaRef) -> bool {
    schema
        .fields()
        .iter()
        .all(|field| has_array_builder_supported(field.data_type()))
}

#[async_trait]
pub trait ShuffleRepartitioner: Send + Sync {
    async fn insert_batch(&self, input: RecordBatch) -> Result<()>;
    async fn shuffle_write(&self) -> Result<()>;
}

impl dyn ShuffleRepartitioner {
    pub async fn execute(
        self: Arc<Self>,
        context: Arc<TaskContext>,
        input: SendableRecordBatchStream,
        batch_size: usize,
        metrics: BaselineMetrics,
    ) -> Result<SendableRecordBatchStream> {
        let input_schema = input.schema();

        // coalesce input
        let mut coalesced = Box::pin(CoalesceStream::new(
            input,
            batch_size,
            metrics.elapsed_compute().clone(),
        ));

        // process all input batches
        output_with_sender("Shuffle", context, input_schema, |_| async move {
            while let Some(batch) = coalesced.next().await.transpose()? {
                let _timer = metrics.elapsed_compute().timer();
                metrics.record_output(batch.num_rows());
                self.insert_batch(batch)
                    .await
                    .map_err(|err| err.context("shuffle: executing insert_batch() error"))?;
            }
            let _timer = metrics.elapsed_compute().timer();
            self.shuffle_write()
                .await
                .map_err(|err| err.context("shuffle: executing shuffle_write() error"))?;
            Ok::<_, DataFusionError>(())
        })
    }
}

struct ShuffleSpill {
    spill: Box<dyn Spill>,
    offsets: Vec<u64>,
}

fn evaluate_hashes(partitioning: &Partitioning, batch: &RecordBatch) -> ArrowResult<Vec<u32>> {
    match partitioning {
        Partitioning::Hash(exprs, _) => {
            let mut hashes_buf = vec![];
            let arrays = exprs
                .iter()
                .map(|expr| Ok(expr.evaluate(batch)?.into_array(batch.num_rows())))
                .collect::<Result<Vec<_>>>()?;

            // use identical seed as spark hash partition
            hashes_buf.resize(arrays[0].len(), 42);

            // compute hash array
            create_hashes(&arrays, &mut hashes_buf)?;
            Ok(hashes_buf)
        }
        _ => unreachable!("unsupported partitioning: {:?}", partitioning),
    }
}

fn evaluate_partition_ids(hashes: &[u32], num_partitions: usize) -> Vec<u32> {
    hashes
        .iter()
        .map(|hash| pmod(*hash, num_partitions) as u32)
        .collect()
}
