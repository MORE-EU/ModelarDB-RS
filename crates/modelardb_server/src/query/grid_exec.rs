/* Copyright 2023 The ModelarDB Contributors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Implementation of the Apache Arrow DataFusion execution plan [`GridExec`] and its corresponding
//! stream [`GridStream`] which reconstructs the data points for a specific column from the
//! compressed segments containing metadata and models.

use std::any::Any;
use std::fmt::{self, Formatter};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as StdTaskContext, Poll};

use async_trait::async_trait;
use datafusion::arrow::array::{
    Array, ArrayRef, BinaryArray, Float32Array, UInt64Array, UInt64Builder, UInt8Array,
};
use datafusion::arrow::compute::filter_record_batch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::cast::as_boolean_array;
use datafusion::error::{DataFusionError, Result};
use datafusion::execution::context::TaskContext;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalSortRequirement};
use datafusion::physical_plan::expressions::PhysicalSortExpr;
use datafusion::physical_plan::metrics::{BaselineMetrics, ExecutionPlanMetricsSet, MetricsSet};
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, Distribution, ExecutionPlan, Partitioning, PhysicalExpr,
    RecordBatchStream, SendableRecordBatchStream, Statistics,
};
use futures::stream::{Stream, StreamExt};
use modelardb_common::schemas::QUERY_SCHEMA;
use modelardb_common::types::{TimestampArray, TimestampBuilder, ValueArray, ValueBuilder};
use modelardb_compression;

use super::{QUERY_ORDER_DATA_POINT, QUERY_ORDER_SEGMENT};

/// An execution plan that reconstructs the data points stored as compressed segments containing
/// metadata and models. It is public so the additional rules added to Apache Arrow DataFusion's
/// physical optimizer can pattern match on it.
#[derive(Debug, Clone)]
pub struct GridExec {
    /// Schema of the execution plan.
    schema: SchemaRef,
    /// Predicate to filter data points by.
    maybe_predicate: Option<Arc<dyn PhysicalExpr>>,
    /// Number of data points requested by the query.
    limit: Option<usize>,
    /// Execution plan to read batches of segments from.
    input: Arc<dyn ExecutionPlan>,
    /// Metrics collected during execution for use by EXPLAIN ANALYZE.
    metrics: ExecutionPlanMetricsSet,
}

impl GridExec {
    pub fn new(
        maybe_predicate: Option<Arc<dyn PhysicalExpr>>,
        limit: Option<usize>,
        input: Arc<dyn ExecutionPlan>,
    ) -> Arc<Self> {
        let schema = QUERY_SCHEMA.0.clone();

        Arc::new(GridExec {
            maybe_predicate,
            schema,
            limit,
            input,
            metrics: ExecutionPlanMetricsSet::new(),
        })
    }
}

#[async_trait]
impl ExecutionPlan for GridExec {
    /// Return `self` as [`Any`] so it can be downcast.
    fn as_any(&self) -> &dyn Any {
        self
    }

    /// Return the schema of the plan.
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Return the partitioning of the single execution plan batches of segments are read from.
    fn output_partitioning(&self) -> Partitioning {
        self.input.output_partitioning()
    }

    /// Specify that the global order for the data points produced by all [`GridExec`] will be the
    /// same. This is needed because [`crate::query::sorted_join_exec::SortedJoinExec`] assumes the
    /// data it receives from all of its inputs uses the same global sort order.
    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        Some(&QUERY_ORDER_DATA_POINT)
    }

    /// Return the single execution plan batches of rows are read from.
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }

    /// Return a new [`GridExec`] with the execution plan to read batches of compressed segments
    /// from replaced. [`DataFusionError::Plan`] is returned if `children` does not contain a single
    /// element.
    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.len() == 1 {
            Ok(GridExec::new(
                self.maybe_predicate.clone(),
                self.limit,
                children[0].clone(),
            ))
        } else {
            Err(DataFusionError::Plan(format!(
                "Exactly one child must be provided {self:?}.",
            )))
        }
    }

    /// Create a stream that reads batches of compressed segments from the child stream,
    /// reconstructs the data points from the metadata and models in the segments, and returns
    /// batches of rows with data points.
    fn execute(
        &self,
        partition: usize,
        task_context: Arc<TaskContext>,
    ) -> Result<SendableRecordBatchStream> {
        // Must be read before GridStream as task_context are moved into input.
        let batch_size = task_context.session_config().batch_size();

        Ok(Box::pin(GridStream::new(
            self.schema.clone(),
            self.maybe_predicate.clone(),
            self.limit,
            self.input.execute(partition, task_context)?,
            batch_size,
            BaselineMetrics::new(&self.metrics, partition),
        )))
    }

    /// Specify that [`GridExec`] knows nothing about the data it will output.
    fn statistics(&self) -> Result<Statistics, DataFusionError> {
        Ok(Statistics::new_unknown(&self.schema))
    }

    /// Specify that [`GridExec`] requires one partition for each input as it assumes that the
    /// global sort order are the same for its input and Apache Arrow DataFusion only guarantees the
    /// sort order within each partition rather than the input's global sort order.
    fn required_input_distribution(&self) -> Vec<Distribution> {
        vec![Distribution::SinglePartition]
    }

    /// Specify that [`GridExec`] requires that its input provides data that is sorted by
    /// [`QUERY_ORDER_SEGMENT`].
    fn required_input_ordering(&self) -> Vec<Option<Vec<PhysicalSortRequirement>>> {
        let physical_sort_requirements =
            PhysicalSortRequirement::from_sort_exprs(QUERY_ORDER_SEGMENT.iter());
        vec![Some(physical_sort_requirements)]
    }

    /// Return an [`EquivalenceProperties`] to specify how the output of [`GridExec`] is ordered.
    /// This is required in addition to [`ExecutionPlan::output_partitioning()`] and
    /// [`ExecutionPlan::output_ordering()`] as it is used by some physical optimizer rules included
    /// with Apache Arrow DataFusion to check the correct sort order is preserved.
    fn equivalence_properties(&self) -> EquivalenceProperties {
        EquivalenceProperties::new_with_orderings(self.schema(), &[QUERY_ORDER_DATA_POINT.clone()])
    }

    /// Return a snapshot of the set of metrics being collected by the execution plain.
    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
    }
}

impl DisplayAs for GridExec {
    /// Write a string-based representation of the operator to `f`. Returns
    /// `Err` if `std::write` cannot format the string and write it to `f`.
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "GridExec: limit={:?}", self.limit)
    }
}

/// A stream that read batches of rows with segments from the input stream, reconstructs the data
/// points from the metadata and models in the segments, and returns batches of data points.
struct GridStream {
    /// Schema of the stream.
    schema: SchemaRef,
    /// Predicate to filter data points by.
    maybe_predicate: Option<Arc<dyn PhysicalExpr>>,
    /// Stream to read batches of compressed segments from.
    input: SendableRecordBatchStream,
    /// Size of the batches returned when this stream is pooled.
    batch_size: usize,
    /// Current batch of data points to return data points from when the stream is pooled.
    current_batch: RecordBatch,
    /// Next data point in the current batch of data points to return when the stream is pooled.
    current_batch_offset: usize,
    /// Metrics collected during execution for use by EXPLAIN ANALYZE.
    baseline_metrics: BaselineMetrics,
}

impl GridStream {
    fn new(
        schema: SchemaRef,
        maybe_predicate: Option<Arc<dyn PhysicalExpr>>,
        limit: Option<usize>,
        input: SendableRecordBatchStream,
        batch_size: usize,
        baseline_metrics: BaselineMetrics,
    ) -> Self {
        // Assumes limit is mostly used to request less than batch_size rows so one batch is enough.
        // If it is a bit larger than batch_size the second batch will contain too many data points.
        // Also limit is not simply used as batch size to prevent OOM issues with a very big limit.
        let batch_size = if let Some(limit) = limit {
            usize::min(limit, batch_size)
        } else {
            batch_size
        };

        GridStream {
            schema: schema.clone(),
            maybe_predicate,
            input,
            baseline_metrics,
            batch_size,
            current_batch: RecordBatch::new_empty(schema),
            current_batch_offset: 0,
        }
    }

    /// Replace the current batch with a sorted [`RecordBatch`] that contains the remaining data
    /// points in the current batch and those reconstructed from the compressed segments in `batch`.
    fn grid_and_append_to_leftovers_in_current_batch(&mut self, batch: &RecordBatch) {
        // Record the time elapsed from the timer is created to it is dropped.
        let _timer = self.baseline_metrics.elapsed_compute().timer();

        // Retrieve the arrays from batch and cast them to their concrete type.
        modelardb_common::arrays!(
            batch,
            univariate_ids,
            model_type_ids,
            start_times,
            end_times,
            timestamps,
            min_values,
            max_values,
            values,
            residuals,
            _error_array
        );

        // Allocate builders with approximately enough capacity. The builders are allocated with
        // enough capacity for the remaining data points in the current batch and one data point
        // from each segment in the new batch as each segment contains at least one data point.
        let current_rows = self.current_batch.num_rows() - self.current_batch_offset;
        let new_rows = batch.num_rows();
        let mut univariate_id_builder = UInt64Builder::with_capacity(current_rows + new_rows);
        let mut timestamp_builder = TimestampBuilder::with_capacity(current_rows + new_rows);
        let mut value_builder = ValueBuilder::with_capacity(current_rows + new_rows);

        // Copy over the data points from the current batch to keep the resulting batch sorted.
        let current_batch = &self.current_batch; // Required as self cannot be passed to array!.
        univariate_id_builder.append_slice(
            &modelardb_common::array!(current_batch, 0, UInt64Array).values()
                [self.current_batch_offset..],
        );
        timestamp_builder.append_slice(
            &modelardb_common::array!(current_batch, 1, TimestampArray).values()
                [self.current_batch_offset..],
        );
        value_builder.append_slice(
            &modelardb_common::array!(current_batch, 2, ValueArray).values()
                [self.current_batch_offset..],
        );

        // Reconstruct the data points from the compressed segments.
        for row_index in 0..new_rows {
            modelardb_compression::grid(
                univariate_ids.value(row_index),
                model_type_ids.value(row_index),
                start_times.value(row_index),
                end_times.value(row_index),
                timestamps.value(row_index),
                min_values.value(row_index),
                max_values.value(row_index),
                values.value(row_index),
                residuals.value(row_index),
                &mut univariate_id_builder,
                &mut timestamp_builder,
                &mut value_builder,
            );
        }

        let columns: Vec<ArrayRef> = vec![
            Arc::new(univariate_id_builder.finish()),
            Arc::new(timestamp_builder.finish()),
            Arc::new(value_builder.finish()),
        ];

        // Update the current batch, unwrap() is safe as GridStream uses a static schema.
        // For simplicity, all data points are reconstructed and then pruned by time.
        let current_batch = RecordBatch::try_new(self.schema.clone(), columns).unwrap();

        self.current_batch = if let Some(predicate) = &self.maybe_predicate {
            // unwrap() is safe as the predicate has been written for the schema.
            let column_value = predicate.evaluate(&current_batch).unwrap();
            let array = column_value.into_array(current_batch.num_rows()).unwrap();
            let boolean_array = as_boolean_array(&array).unwrap();
            filter_record_batch(&current_batch, boolean_array).unwrap()
        } else {
            current_batch
        };

        // As a new batch have been created the offset into this batch must be set to zero.
        self.current_batch_offset = 0;
    }
}

impl Stream for GridStream {
    /// Specify that [`GridStream`] returns [`Result<RecordBatch>`] when polled.
    type Item = Result<RecordBatch>;

    /// Try to poll the next batch of data points from the [`GridStream`] and returns:
    /// * `Poll::Pending` if the next batch is not yet ready.
    /// * `Poll::Ready(Some(Ok(batch)))` if the next batch is ready.
    /// * `Poll::Ready(None)` if the stream is empty.
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut StdTaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        // Try to ensure there are enough data points in the current batch to match batch size.
        if (self.current_batch.num_rows() - self.current_batch_offset) < self.batch_size {
            match self.input.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(batch))) => {
                    self.grid_and_append_to_leftovers_in_current_batch(&batch);
                }
                Poll::Ready(None) if self.current_batch_offset < self.current_batch.num_rows() => {
                    // Ignore Poll::Ready(None) as there are data points in the current buffer.
                }
                other => return self.baseline_metrics.record_poll(other),
            }
        }

        // While input uses the same batch size as self and each compressed segment is guaranteed to
        // represent one data point, the current batch may not contain enough data points, e.g., if
        // the query contains a very specific predicate that filter out all but a very few segments.
        let remaining_data_points = self.current_batch.num_rows() - self.current_batch_offset;
        let length = usize::min(self.batch_size, remaining_data_points);
        let batch = self.current_batch.slice(self.current_batch_offset, length);
        self.current_batch_offset += batch.num_rows();
        self.baseline_metrics
            .record_poll(Poll::Ready(Some(Ok(batch))))
    }
}

impl RecordBatchStream for GridStream {
    /// Return the schema of the stream.
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}
