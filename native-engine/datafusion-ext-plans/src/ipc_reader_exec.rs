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

use std::{
    any::Any,
    fmt::{Debug, Formatter},
    sync::Arc,
};

use arrow::datatypes::SchemaRef;
use async_trait::async_trait;
use blaze_jni_bridge::{jni_call, jni_call_static, jni_new_global_ref, jni_new_string};
use datafusion::{
    error::Result,
    execution::context::TaskContext,
    physical_plan::{
        expressions::PhysicalSortExpr,
        metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricBuilder, MetricsSet},
        DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning,
        Partitioning::UnknownPartitioning,
        SendableRecordBatchStream, Statistics,
    },
};
use datafusion_ext_commons::streams::{
    coalesce_stream::CoalesceInput,
    ipc_stream::{IpcReadMode, IpcReaderStream},
};
use jni::objects::JObject;

#[derive(Debug, Clone)]
pub struct IpcReaderExec {
    pub num_partitions: usize,
    pub ipc_provider_resource_id: String,
    pub schema: SchemaRef,
    pub mode: IpcReadMode,
    pub metrics: ExecutionPlanMetricsSet,
}
impl IpcReaderExec {
    pub fn new(
        num_partitions: usize,
        ipc_provider_resource_id: String,
        schema: SchemaRef,
        mode: IpcReadMode,
    ) -> IpcReaderExec {
        IpcReaderExec {
            num_partitions,
            ipc_provider_resource_id,
            schema,
            mode,
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }
}

impl DisplayAs for IpcReaderExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "IpcReader: [{:?}]", &self.schema)
    }
}

#[async_trait]
impl ExecutionPlan for IpcReaderExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn output_partitioning(&self) -> Partitioning {
        UnknownPartitioning(self.num_partitions)
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(Self::new(
            self.num_partitions,
            self.ipc_provider_resource_id.clone(),
            self.schema.clone(),
            self.mode,
        )))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let baseline_metrics = BaselineMetrics::new(&self.metrics, partition);
        let size_counter = MetricBuilder::new(&self.metrics).counter("size", partition);

        let elapsed_compute = baseline_metrics.elapsed_compute().clone();
        let _timer = elapsed_compute.timer();

        let segments_provider = jni_call_static!(
            JniBridge.getResource(
                jni_new_string!(&self.ipc_provider_resource_id)?.as_obj()
            ) -> JObject
        )?;
        let segments_local =
            jni_call!(ScalaFunction0(segments_provider.as_obj()).apply() -> JObject)?;
        let segments = jni_new_global_ref!(segments_local.as_obj())?;

        let schema = self.schema.clone();
        let mode = self.mode;
        let ipc_stream = Box::pin(IpcReaderStream::new(
            schema,
            segments,
            mode,
            baseline_metrics,
            size_counter,
        ));
        Ok(context.coalesce_with_default_batch_size(
            ipc_stream,
            &BaselineMetrics::new(&self.metrics, partition),
        )?)
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Statistics {
        Statistics::default()
    }
}
