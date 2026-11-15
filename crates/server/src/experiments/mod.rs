//! Experiments: the plan window on experiments (D-J-14). Plain functions, no traits.

use llmobs_core::ids::OrgId;

use crate::entitlements::Visibility;
use crate::http::AppState;

/// The org's window bound now (`created_at` before it is hidden), or `None` without a window
/// (D-J-14).
pub(crate) fn visible_from(state: &AppState, org: OrgId) -> Option<i64> {
    Visibility::window(state, org).visible_from()
}
