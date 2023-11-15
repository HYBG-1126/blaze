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

use crate::common::batch_statisitcs::{stat_input, InputBatchStatistics};
use crate::common::cached_exprs_evaluator::CachedExprsEvaluator;
use crate::common::column_pruning::ExecuteWithColumnPruning;
use crate::common::output::output_with_sender;
use crate::project_exec::ProjectExec;
use arrow::datatypes::{DataType, SchemaRef};
use datafusion::common::Statistics;
use datafusion::common::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::expressions::Column;
use datafusion::physical_expr::{PhysicalExprRef, PhysicalSortExpr};
use datafusion::physical_plan::metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning, SendableRecordBatchStream,
};
use datafusion_ext_commons::streams::coalesce_stream::CoalesceStream;
use futures::stream::once;
use futures::{StreamExt, TryStreamExt};
use itertools::Itertools;
use std::any::Any;
use std::fmt::Formatter;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct FilterExec {
    input: Arc<dyn ExecutionPlan>,
    predicates: Vec<PhysicalExprRef>,
    metrics: ExecutionPlanMetricsSet,
}

impl FilterExec {
    pub fn try_new(
        predicates: Vec<PhysicalExprRef>,
        input: Arc<dyn ExecutionPlan>,
    ) -> Result<Self> {
        let schema = input.schema();

        if predicates.is_empty() {
            return Err(DataFusionError::Plan(format!(
                "Filter requires at least one predicate"
            )));
        }
        if !predicates
            .iter()
            .all(|pred| matches!(pred.data_type(&schema), Ok(DataType::Boolean)))
        {
            return Err(DataFusionError::Plan(format!(
                "Filter predicate must return boolean values"
            )));
        }
        Ok(Self {
            input,
            predicates,
            metrics: ExecutionPlanMetricsSet::new(),
        })
    }

    pub fn predicates(&self) -> &[PhysicalExprRef] {
        &self.predicates
    }
}

impl DisplayAs for FilterExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        write!(
            f,
            "FilterExec [{}]",
            self.predicates.iter().map(|e| format!("{e}")).join(", ")
        )
    }
}

impl ExecutionPlan for FilterExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }

    fn output_partitioning(&self) -> Partitioning {
        self.input.output_partitioning()
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        self.input.output_ordering()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(Self::try_new(
            self.predicates.clone(),
            children[0].clone(),
        )?))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        let batch_size = context.session_config().batch_size();
        let predicates = self.predicates.clone();
        let metrics = BaselineMetrics::new(&self.metrics, partition);
        let elapsed_compute = metrics.elapsed_compute().clone();

        let input = stat_input(
            InputBatchStatistics::from_metrics_set_and_blaze_conf(&self.metrics, partition)?,
            self.input.execute(partition, context.clone())?,
        )?;
        let filtered = Box::pin(RecordBatchStreamAdapter::new(
            self.schema(),
            once(execute_filter(input, context, predicates, metrics)).try_flatten(),
        ));
        let coalesced = Box::pin(CoalesceStream::new(filtered, batch_size, elapsed_compute));
        Ok(coalesced)
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }

    fn statistics(&self) -> Statistics {
        todo!()
    }
}

impl ExecuteWithColumnPruning for FilterExec {
    fn execute_projected(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
        projection: &[usize],
    ) -> Result<SendableRecordBatchStream> {
        let schema = self.schema();
        let project = Arc::new(ProjectExec::try_new(
            schema
                .fields()
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    let name = field.name().to_owned();
                    let col: PhysicalExprRef = Arc::new(Column::new(&name, i));
                    (col, name)
                })
                .collect(),
            Arc::new(self.clone()),
        )?);
        project.execute_projected(partition, context, projection)
    }
}

async fn execute_filter(
    mut input: SendableRecordBatchStream,
    context: Arc<TaskContext>,
    predicates: Vec<PhysicalExprRef>,
    metrics: BaselineMetrics,
) -> Result<SendableRecordBatchStream> {
    let cached_exprs_evaluator = CachedExprsEvaluator::try_new(predicates, vec![])?;

    output_with_sender(
        "Filter",
        context,
        input.schema(),
        move |sender| async move {
            while let Some(batch) = input.next().await.transpose()? {
                let mut timer = metrics.elapsed_compute().timer();
                let filtered_batch = cached_exprs_evaluator.filter(&batch)?;
                metrics.record_output(filtered_batch.num_rows());
                sender.send(Ok(filtered_batch), Some(&mut timer)).await;
            }
            Ok(())
        },
    )
}
