//! Error issues: the error spans of a batch become issues, routed and regressed (D-IC-2).

use crate::eval::pending::Completed;
use crate::eval::trigger::SweepCtx;

/// Error issues for one batch (D-IC-2). Filled in by M1.6b.
pub(crate) async fn sweep_batch(
    _ctx: &SweepCtx<'_>,
    _around: i64,
    _batch: &[Completed],
) -> Result<(), ()> {
    Ok(())
}
