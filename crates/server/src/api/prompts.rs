//! Prompt registry request and response types (spec 09 § Prompts, D-V-2 to D-V-13).

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::{Limit, TimeParam, any_json, open_object};

/// `text` or `chat` (D-V-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// One template text.
    Text,
    /// A list of messages and placeholders.
    Chat,
}

/// A label and the version it points at (D-V-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PromptLabel {
    /// The label, `[a-z0-9][a-z0-9_.-]{0,63}`.
    pub label: String,
    /// The version the label points at.
    pub version: i64,
    /// When the label last moved.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// A prompt (D-V-2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Prompt {
    /// Prompt id.
    pub id: Uuid,
    /// Unique in the project, 1 to 200 characters. Immutable.
    pub name: String,
    /// `text` or `chat`. Immutable.
    #[serde(rename = "type")]
    pub kind: PromptKind,
    /// Free text, up to 2 000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Tags, in the order they were given.
    pub tags: Vec<String>,
    /// The highest version. `latest` always resolves to it (D-V-8).
    pub latest_version: i64,
    /// Stored labels, ordered by label. `latest` is not listed.
    pub labels: Vec<PromptLabel>,
    /// A user id, or `key:<key id>` for an API key. Absent when the user was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// When the prompt was created.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// When the prompt was last edited or versioned.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// One version, as the fetch and the version routes return it (D-V-7, D-V-12).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PromptVersion {
    /// The prompt's name.
    pub name: String,
    /// `text` or `chat`.
    #[serde(rename = "type")]
    pub kind: PromptKind,
    /// The version number, 1, 2, …
    pub version: i64,
    /// The template: a string for `text`, an array of messages for `chat`.
    #[schema(schema_with = any_json)]
    pub template: Value,
    /// The model configuration (D-V-6): `model`, `connection_id`, `params`, `tools`,
    /// `tool_choice`, `response_format`, and any other keys as given.
    #[schema(schema_with = open_object)]
    pub config: Value,
    /// Labels that point at this version, ordered. Includes `latest` when this is the latest
    /// version.
    pub labels: Vec<String>,
    /// Variable names, in order of first appearance.
    pub variables: Vec<String>,
    /// Placeholder names (chat), in order of first appearance.
    pub placeholders: Vec<String>,
    /// The commit message, up to 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    /// A user id, or `key:<key id>` for an API key. Absent when the user was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// When the version was created.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
}

/// `POST /api/v1/projects/{project}/prompts` body (D-V-2, D-V-5 to D-V-7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreatePromptRequest {
    /// Unique in the project, 1 to 200 characters: letters, digits, `_`, `.`, `-`, starting with
    /// a letter or digit.
    pub name: String,
    /// `text` or `chat`.
    #[serde(rename = "type")]
    pub kind: PromptKind,
    /// The first version's template: a string for `text`, an array of messages for `chat`.
    #[schema(schema_with = any_json)]
    pub template: Value,
    /// The first version's model configuration (D-V-6). Absent: an empty object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(schema_with = open_object)]
    pub config: Option<Value>,
    /// Labels to point at the first version. The API never adds `production` by itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// The first version's commit message, up to 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    /// Free text, up to 2 000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Tags: at most 20, each 1 to 64 characters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// `POST /api/v1/projects/{project}/prompts/{prompt}/versions` body (D-V-7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateVersionRequest {
    /// The new version's template: a string for `text`, an array of messages for `chat`.
    #[schema(schema_with = any_json)]
    pub template: Value,
    /// The new version's model configuration (D-V-6). Absent: an empty object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(schema_with = open_object)]
    pub config: Option<Value>,
    /// Labels to move to the new version. Protected labels need a project admin or a `write`
    /// key (D-V-10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// The commit message, up to 500 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
}

/// `PATCH /api/v1/projects/{project}/prompts/{prompt}` body. An absent field is unchanged.
/// The name, the type and the versions never change (D-V-2, D-V-7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePromptRequest {
    /// New description, when present. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    /// New tags, when present. Replaces the whole list. `null` is refused.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Vec<String>)]
    pub tags: Option<Option<Vec<String>>>,
}

/// `PUT /api/v1/projects/{project}/prompts/{prompt}/labels/{label}` body (D-V-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetLabelRequest {
    /// The version the label moves to.
    pub version: i64,
}

/// One entry of a label's history (D-V-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PromptLabelEvent {
    /// The label that moved.
    pub label: String,
    /// The version it pointed at before. Absent when the label was new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_version: Option<i64>,
    /// The version it points at now. Absent when the label was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_version: Option<i64>,
    /// A user id, or `key:<key id>` for an API key. Absent when the user was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// When the label moved.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub at: i64,
}

/// The prompt settings of a project, read and written by the settings routes (D-V-10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PromptSettings {
    /// Labels only project admins and `write` keys may move. At most 20 (D-V-10).
    pub protected_labels: Vec<String>,
}

/// `GET /api/v1/projects/{project}/prompts` query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct PromptListParams {
    /// Name contains this text, ignoring case.
    #[serde(default)]
    pub q: Option<String>,
    /// Only prompts with this label.
    #[serde(default)]
    pub label: Option<String>,
    /// Only prompts with this tag.
    #[serde(default)]
    pub tag: Option<String>,
    /// Maximum number of prompts to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET …/prompts/{prompt}/resolve` query (D-V-9, D-V-12).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ResolveParams {
    /// A label. Default `production`. `latest` resolves the highest version.
    #[serde(default)]
    pub label: Option<String>,
    /// A version number. Not together with `label`.
    #[serde(default)]
    pub version: Option<i64>,
}

/// `GET …/prompts/{prompt}/metrics` query (D-V-17). The window is at most 90 days.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct PromptMetricsParams {
    /// Window start, RFC 3339 or epoch milliseconds. Default: `to` minus 7 days.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Window end, RFC 3339 or epoch milliseconds. Default: now.
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Comma-separated version numbers, 1 to 50 distinct. Default: every version.
    #[serde(default)]
    pub versions: Option<String>,
}

/// The per-version metrics of a prompt over a window (D-V-17). Versions come newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PromptMetrics {
    /// Window start.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    /// Window end (exclusive).
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub to: i64,
    /// One entry per version that has linked spans in the window.
    pub versions: Vec<PromptVersionMetrics>,
}

/// One version's numbers over the window. Span scores count for their span's version; a trace
/// score counts for every version its linked spans carry (D-V-17).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PromptVersionMetrics {
    /// The version, 1 or more.
    pub version: i64,
    /// Linked `llm` spans.
    pub span_count: i64,
    /// Distinct traces of those spans.
    pub trace_count: i64,
    /// Linked spans with status `error`.
    pub error_count: i64,
    /// Sum of span costs, USD.
    pub cost_total: f64,
    /// Average span cost, USD. Omitted when no span has a cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_avg: Option<f64>,
    /// Median span duration, milliseconds (approximate). Omitted when there are no spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_p50_ms: Option<f64>,
    /// 95th percentile span duration, milliseconds (approximate). Omitted when there are no spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_p95_ms: Option<f64>,
    /// Average input tokens. Omitted when no span has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_avg: Option<f64>,
    /// Average output tokens. Omitted when no span has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_avg: Option<f64>,
    /// Numeric and boolean score means, by name.
    pub scores: Vec<PromptScoreMean>,
}

/// The mean of one score name for one version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PromptScoreMean {
    /// Score name.
    pub name: String,
    /// Average of the numeric and boolean score values.
    pub mean: f64,
    /// Number of scores averaged.
    pub count: i64,
}

/// Present JSON is `Some(value)`, including null. An absent field stays the default `None`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_refuse_unknown_fields() {
        let with_extra = json!({"name": "a", "type": "text", "template": "x", "foo": 1});
        assert!(serde_json::from_value::<CreatePromptRequest>(with_extra).is_err());
        let ok = json!({"name": "a", "type": "text", "template": "x"});
        assert!(serde_json::from_value::<CreatePromptRequest>(ok).is_ok());
        assert!(
            serde_json::from_value::<CreateVersionRequest>(json!({"template": "x", "foo": 1}))
                .is_err()
        );
        assert!(serde_json::from_value::<UpdatePromptRequest>(json!({"name": "renamed"})).is_err());
    }

    #[test]
    fn update_tri_state() {
        let absent: UpdatePromptRequest = serde_json::from_value(json!({})).expect("parses");
        assert_eq!(absent.description, None);
        let cleared: UpdatePromptRequest =
            serde_json::from_value(json!({"description": null})).expect("parses");
        assert_eq!(cleared.description, Some(None));
        let set: UpdatePromptRequest =
            serde_json::from_value(json!({"description": "x"})).expect("parses");
        assert_eq!(set.description, Some(Some("x".to_owned())));
    }

    #[test]
    fn tags_tri_state() {
        let absent: UpdatePromptRequest = serde_json::from_value(json!({})).expect("parses");
        assert_eq!(absent.tags, None);
        let null: UpdatePromptRequest =
            serde_json::from_value(json!({"tags": null})).expect("parses");
        assert_eq!(null.tags, Some(None));
        let set: UpdatePromptRequest =
            serde_json::from_value(json!({"tags": ["a"]})).expect("parses");
        assert_eq!(set.tags, Some(Some(vec!["a".to_owned()])));
    }

    #[test]
    fn kind_spelling() {
        assert_eq!(
            serde_json::from_value::<PromptKind>(json!("text")).expect("text"),
            PromptKind::Text
        );
        assert_eq!(
            serde_json::from_value::<PromptKind>(json!("chat")).expect("chat"),
            PromptKind::Chat
        );
        assert!(serde_json::from_value::<PromptKind>(json!("Text")).is_err());
    }
}
