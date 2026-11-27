//! Synchronous push-pipeline query execution engine (`D-QE-5`).

pub mod hash_agg;
pub mod hash_join;
pub mod pipeline;
pub mod pool;
pub mod sort_key;
pub mod sort_window;
pub mod source;
pub mod spill;
pub mod streaming;

pub use hash_agg::{AggExpr, AggFunc, DdSketch, HashAgg, HashAggSpec, execute_hash_agg};
pub use pipeline::{
    CollectSink, ControlFlow, DEFAULT_BATCH_SIZE, ExecContext, ExecMetrics, OperatorProfile,
    PushOperator, PushSink, run_pipeline, run_pipeline_with_stop,
};
pub use pool::WorkerPool;
pub use sort_key::{SortKey, compare_rows, compare_rows_across};
pub use source::{
    BatchSource, IdLookupHint, MapProjection, PartitionStream, ScanSource, ScanSpec, SortHint,
    SubstringHint, TimeRangeHint, collect_expr_map_projections,
};
pub use spill::{
    MAX_MERGE_WAY, SpillAppender, SpillCapFn, SpillFile, SpillManager, SpillReader, SpillWriter,
};
