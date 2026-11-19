//! Score definition request and response types.
//!
//! Spec 09 § Evaluation & alerts and spec 04 § Scores definitions. A definition names the
//! shape of a score; score writes and evaluators must fit the live definition of their name.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use cetra_core::enums::ScoreDataType;

use super::common::Limit;

/// One label of a categorical score definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ScoreDefinitionCategory {
    /// The label, 1 to 255 bytes.
    pub label: String,
    /// Numeric value of the label. Omitted when the label has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// Which way a score should move to be good. Drives the colour of its value in the UI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScoreDirection {
    /// A higher value is better.
    Higher,
    /// A lower value is better.
    Lower,
    /// No direction: the score is neutral.
    #[default]
    None,
}

impl ScoreDirection {
    /// The stored and JSON text: `higher`, `lower` or `none`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Higher => "higher",
            Self::Lower => "lower",
            Self::None => "none",
        }
    }

    /// Parses the stored text. `None` for any other text.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "higher" => Some(Self::Higher),
            "lower" => Some(Self::Lower),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// A named score shape. Archived definitions are kept but not checked against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ScoreDefinition {
    /// Definition id.
    pub id: Uuid,
    /// Score name, unique among live definitions of the project.
    pub name: String,
    /// Score data type. Immutable.
    pub data_type: ScoreDataType,
    /// Labels of a categorical score. Empty otherwise.
    pub categories: Vec<ScoreDefinitionCategory>,
    /// Lowest allowed value of a numeric score. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    /// Highest allowed value of a numeric score. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    /// What the score means. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which way the score should move to be good. `none` when unset.
    pub direction: ScoreDirection,
    /// When the definition was archived. Omitted while it is live. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub archived_at: Option<i64>,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last change time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// `POST /api/v1/projects/{project}/score-definitions` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateScoreDefinitionRequest {
    /// Score name, 1 to 255 bytes after trimming.
    pub name: String,
    /// Score data type. Immutable once created.
    pub data_type: ScoreDataType,
    /// Labels of a categorical score: 1 to 100, unique. Only for categorical scores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<ScoreDefinitionCategory>>,
    /// Lowest allowed value. Only for numeric scores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    /// Highest allowed value. Only for numeric scores, above `min_value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    /// What the score means, at most 1 000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which way the score should move to be good. Omitted means `none`. `higher` and `lower`
    /// need a numeric range or a value on every category, and are refused on text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<ScoreDirection>,
}

/// `PATCH /api/v1/projects/{project}/score-definitions/{id}` body. Absent means unchanged.
///
/// `min_value` and `max_value` cannot be cleared once set. Archive the definition instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateScoreDefinitionRequest {
    /// Score name, 1 to 255 bytes after trimming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Labels of a categorical score: 1 to 100, unique.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub categories: Option<Vec<ScoreDefinitionCategory>>,
    /// Lowest allowed value. Only for numeric scores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    /// Highest allowed value. Only for numeric scores, above `min_value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    /// What the score means. An empty string clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which way the score should move to be good. Same rules as on create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<ScoreDirection>,
    /// `true` archives the definition; `false` restores it, unless another live one has its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

/// Query parameters of `GET /api/v1/projects/{project}/score-definitions`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ScoreDefinitionListParams {
    /// Include archived definitions. Default false.
    #[serde(default)]
    pub include_archived: bool,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}
