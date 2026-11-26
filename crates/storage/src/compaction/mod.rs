//! Compaction: picking jobs, merging their inputs slice by slice, and running
//! them from a background task. The policy is spec 03 §5.

pub(crate) mod job;
pub(crate) mod merge;
pub(crate) mod plan;
pub(crate) mod rewrite;
pub(crate) mod slice;
pub(crate) mod task;
pub(crate) mod verify_pass;

/// Cancels an in-flight `WorkerPool` pull when the owning compaction future is dropped.
pub(super) struct CancelOnDrop(pub(super) cetra_column::CancelToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
