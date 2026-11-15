//! Usage API types (spec 09, D-P-16, D-P-42).

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters of `GET /api/v1/orgs/{org}/usage`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct UsageParams {
    /// First day, inclusive, `YYYY-MM-DD` (UTC). Default: the 1st of this month.
    pub from: Option<String>,
    /// Last day, inclusive, `YYYY-MM-DD` (UTC). Default: today. `to - from` at most 365 days.
    pub to: Option<String>,
}

/// One UTC day of an org's usage, summed over its projects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UsageDay {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub spans: u64,
    pub scores: u64,
    /// `spans + scores`.
    pub units: u64,
    pub bytes: u64,
    pub judge_input_tokens: u64,
    pub judge_output_tokens: u64,
    pub judge_cost_usd: f64,
}

/// `GET /api/v1/orgs/{org}/usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OrgUsage {
    pub from: String,
    pub to: String,
    /// Days with usage, oldest first. Days without usage are absent.
    pub days: Vec<UsageDay>,
}
