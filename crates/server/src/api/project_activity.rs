//! Project activity API types (spec 09 § Orgs, projects, members, keys; D-C-27).

use cetra_core::ids::ProjectId;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters of `GET /api/v1/orgs/{org}/projects/activity`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ProjectActivityParams {
    /// One to 50 distinct project ids, comma-separated, in the order of the answer.
    pub ids: String,
}

/// One hour of a project's traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ActivityBucket {
    /// Traces that started in the hour.
    pub traces: u64,
    /// Of those, the traces whose status is `error`.
    pub errors: u64,
}

/// One project's traces over the activity window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProjectActivity {
    /// The project.
    pub project_id: ProjectId,
    /// Traces that started in the window, with the default exclusions (spec 05).
    pub traces: u64,
    /// Of those, the traces whose status is `error`.
    pub errors: u64,
    /// Sum of the traces' `cost_total`, USD.
    pub cost_usd: f64,
    /// 24 hourly buckets, oldest first. Bucket `i` starts `i` hours after `from`.
    pub buckets: Vec<ActivityBucket>,
    /// When the server last committed spans sent over OTLP to this project (D-C-20). Kept in
    /// memory: absent until the first batch after the server starts.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_ingest_at: Option<i64>,
}

/// `GET /api/v1/orgs/{org}/projects/activity`: the requested projects' last 24 hours (D-C-27).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProjectsActivity {
    /// Window start: the start of the hour 23 hours before the current one. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    /// Window end, exclusive: one nanosecond after the request time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub to: i64,
    /// One entry per requested project the caller can see, in request order.
    pub data: Vec<ProjectActivity>,
    /// Requested projects whose query failed or ran out of time, in request order.
    pub unavailable: Vec<ProjectId>,
}
