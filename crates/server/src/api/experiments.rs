//! Experiments API types (spec 09 § Datasets & experiments, D-J-10 to D-J-13).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use llmobs_core::ids::TraceId;

use super::common::{Limit, any_json};
use super::datasets::DatasetItem;

/// Repetitions of an experiment (D-J-10).
pub const MAX_REPETITIONS: i64 = 20;
/// Live items × repetitions of an experiment (D-J-10, CG-J2).
pub const MAX_EXPERIMENT_ROWS: i64 = 20_000;
/// Results in one `POST …/items` (D-J-10).
pub const MAX_RESULTS_PER_REQUEST: usize = 500;
/// Serialized size of one result's `output` (D-J-10).
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
/// Serialized size of all the outputs of one `POST …/items` (D-J-10).
pub const MAX_OUTPUTS_PER_REQUEST: usize = 32 * 1024 * 1024;
/// Body limit of `POST …/items`: the outputs' cap plus room for the rest (D-J-10).
pub const RESULTS_BODY_LIMIT: usize = 40 * 1024 * 1024;
/// Characters of one result's `error`, and of `finish`'s `error` (D-J-10).
pub const MAX_ERROR_CHARS: usize = 4_096;
/// Serialized size of an experiment's `metadata` (D-J-10).
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
/// Characters of a name (D-J-10).
pub const MAX_NAME_CHARS: usize = 200;
/// Characters of a description (D-J-4's limit, reused).
pub const MAX_DESCRIPTION_CHARS: usize = 2_000;
/// Items in `CreateExperimentResponse` (the first page of the pinned version).
pub const CREATE_ITEMS_PAGE: u32 = 1_000;
/// How far before an experiment's `created_at` its traces and scores are read (client clocks
/// stamp span start times and UUIDv7 score ids), in nanoseconds: one hour.
pub const CLOCK_SKEW_NS: i64 = 3_600_000_000_000;

/// `running`, `completed` or `failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    /// Results may still be posted.
    Running,
    /// Finished without an error.
    Completed,
    /// Finished with an error.
    Failed,
}

/// One experiment: a run of a task over a pinned dataset version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Experiment {
    /// Experiment id.
    pub id: Uuid,
    /// Not unique. 1 to 200 characters.
    pub name: String,
    /// Free text. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The dataset. Omitted once the dataset is deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<Uuid>,
    /// The dataset's name when the experiment was created.
    pub dataset_name: String,
    /// The dataset version the experiment is pinned to.
    pub dataset_version: i64,
    /// A JSON object.
    #[schema(value_type = Object)]
    pub metadata: Map<String, Value>,
    /// Repetitions of every item, 1 to 20.
    pub repetitions: i64,
    /// Live items of the pinned version.
    pub item_count: i64,
    /// Results stored so far.
    pub result_count: i64,
    /// `running`, `completed` or `failed`.
    pub status: ExperimentStatus,
    /// The error the experiment finished with. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// When the experiment finished. Omitted while it runs. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub finished_at: Option<i64>,
}

/// `POST /api/v1/projects/{project}/experiments` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateExperimentRequest {
    /// Experiment name, 1 to 200 characters after trimming.
    pub name: String,
    /// The dataset, by id or by name.
    pub dataset: String,
    /// The version to pin. Defaults to the dataset's current version. 0 is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_version: Option<i64>,
    /// Free text, at most 2 000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A JSON object of at most 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
    /// Repetitions of every item, 1 to 20. Defaults to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repetitions: Option<i64>,
}

/// The created experiment and the first page of its pinned items. The SDK pages the rest with
/// `GET …/datasets/{dataset}/items?version=` from `next_cursor`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CreateExperimentResponse {
    /// The experiment, `running`.
    pub experiment: Experiment,
    /// The first 1 000 items of the pinned version, by item id.
    pub items: Vec<DatasetItem>,
    /// The item id to pass as `cursor` of `GET …/items` for the next page, or null on the last
    /// page. Always present.
    #[schema(required)]
    pub next_cursor: Option<String>,
}

/// One result: what the task answered for one item and repetition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperimentResult {
    /// The dataset item. Its input and expected output are copied from the pinned version.
    pub dataset_item_id: Uuid,
    /// 0 to `repetitions` − 1.
    pub repetition: i64,
    /// The trace of the task run. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<TraceId>,
    /// Any JSON value of at most 256 KiB. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(schema_with = any_json)]
    pub output: Option<Value>,
    /// What went wrong, at most 4 096 characters. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Time the task took, in nanoseconds. Omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ns: Option<i64>,
}

/// `POST …/experiments/{id}/items` body: 1 to 500 results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExperimentResultsRequest {
    /// The results, 1 to 500 of them. One row per item and repetition: a later post replaces it.
    pub results: Vec<ExperimentResult>,
}

/// What `POST …/items` stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ExperimentResultsResponse {
    /// Rows written.
    pub stored: i64,
    /// Rows not stored because their trace is being deleted (D-J-9).
    pub skipped: i64,
}

/// `POST …/experiments/{id}/finish` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FinishExperimentRequest {
    /// Why the run failed, at most 4 096 characters. Omitted for `completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `GET /experiments` query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ExperimentListParams {
    /// Keep one dataset's experiments: its id or name.
    #[serde(default)]
    pub dataset: Option<String>,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}
