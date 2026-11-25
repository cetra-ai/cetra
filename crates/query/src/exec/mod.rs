//! Synchronous push-pipeline query execution engine (`D-QE-5`).

pub mod hash_agg;
pub mod pipeline;
pub mod pool;
pub mod sort_key;
pub mod source;
pub mod spill;

pub use hash_agg::{AggExpr, AggFunc, DdSketch, HashAgg, HashAggSpec, execute_hash_agg};
pub use pipeline::{
    CollectSink, ControlFlow, DEFAULT_BATCH_SIZE, ExecContext, ExecMetrics, OperatorProfile,
    PushOperator, PushSink, run_pipeline,
};
pub use pool::WorkerPool;
pub use sort_key::{SortKey, compare_rows, compare_rows_across};
pub use source::{
    BatchSource, IdLookupHint, PartitionStream, ScanSource, ScanSpec, SortHint, SubstringHint,
    TimeRangeHint,
};
pub use spill::{MAX_MERGE_WAY, SpillCapFn, SpillFile, SpillManager, SpillReader, SpillWriter};
