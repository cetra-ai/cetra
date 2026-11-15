//! Dataset request and response types (spec 09 § Datasets & experiments, D-J-2 to D-J-5).

use std::fmt;

use llmobs_core::ids::{SpanId, TraceId};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::{Limit, any_json};

/// A dataset (D-J-2, D-J-3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Dataset {
    /// Dataset id.
    pub id: Uuid,
    /// Unique in the project, 1 to 200 characters, never a UUID.
    pub name: String,
    /// What the dataset holds. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema (Draft 2020-12) every item input must fit. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Value>,
    /// JSON Schema every present expected output must fit. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub expected_schema: Option<Value>,
    /// Latest version; 0 for a dataset with no change yet.
    pub current_version: i64,
    /// Live items at the latest version.
    pub item_count: i64,
    /// Experiments run on this dataset.
    pub experiment_count: i64,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last change time. JSON is RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

/// `POST /api/v1/projects/{project}/datasets` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateDatasetRequest {
    /// Unique in the project, 1 to 200 characters, never a UUID.
    pub name: String,
    /// Free text, at most 2 000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema (Draft 2020-12) every item input must fit, at most 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Value>,
    /// JSON Schema every present expected output must fit, at most 64 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub expected_schema: Option<Value>,
}

/// `PATCH /api/v1/projects/{project}/datasets/{dataset}` body. An absent field is unchanged;
/// `null` clears `description` or a schema.
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateDatasetRequest {
    /// New name, when present. Same rules as on create.
    #[serde(default)]
    pub name: Option<String>,
    /// New description, when present. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    /// New input schema, when present. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Object>)]
    pub input_schema: Option<Option<Value>>,
    /// New expected-output schema, when present. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Object>)]
    pub expected_schema: Option<Option<Value>>,
}

/// One version of a dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DatasetVersion {
    /// Version number. Versions start at 1.
    pub version: i64,
    /// What changed, e.g. `add 12 items`.
    pub description: String,
    /// The user id, or `key:<key id>` for an API key. Omitted when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// Creation time. JSON is RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
}

/// One item as it is in a version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DatasetItem {
    /// The item id, stable across versions.
    pub id: Uuid,
    /// This revision.
    pub revision_id: Uuid,
    /// The version that introduced this revision.
    pub version: i64,
    /// Any JSON value.
    #[schema(schema_with = any_json)]
    pub input: Value,
    /// Any JSON value. Omitted when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Value>,
    /// A JSON object. Empty when the item has no metadata.
    #[schema(value_type = Object)]
    pub metadata: Map<String, Value>,
    /// The trace the item was copied from. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_trace_id: Option<TraceId>,
    /// The span the item was copied from. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_span_id: Option<SpanId>,
    /// Creation time of this revision. JSON is RFC 3339.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
}

/// One item to add.
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewDatasetItem {
    /// Any JSON value but null.
    #[schema(schema_with = any_json)]
    pub input: Value,
    /// Any JSON value. Omitted when there is no expected output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Value>,
    /// A JSON object. Empty when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
}

impl fmt::Debug for NewDatasetItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = json_len(&self.input)
            + self.expected_output.as_ref().map_or(0, json_len)
            + self.metadata.as_ref().map_or(0, json_len);
        f.debug_struct("NewDatasetItem")
            .field("bytes", &bytes)
            .finish_non_exhaustive()
    }
}

/// `POST …/datasets/{dataset}/items` body: 1 to 1 000 items, one new version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddItemsRequest {
    /// The items, 1 to 1 000 of them. Their ids come back in the same order.
    pub items: Vec<NewDatasetItem>,
}

/// The new version and the new item ids, in request order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AddItemsResponse {
    /// The version the items were added in.
    pub version: i64,
    /// The new item ids, in request order.
    pub item_ids: Vec<Uuid>,
}

/// `PATCH …/items/{item_id}` body. Absent keeps; `expected_output: null` clears it.
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateItemRequest {
    /// New input, when present. Never null.
    #[serde(default)]
    #[schema(schema_with = any_json)]
    pub input: Option<Value>,
    /// New expected output, when present. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Option<Value>>,
    /// New metadata, when present. A JSON object.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
}

/// `GET /datasets` query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DatasetListParams {
    /// Case-insensitive substring of the name.
    #[serde(default)]
    pub q: Option<String>,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /datasets/{dataset}/versions` query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DatasetVersionListParams {
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /datasets/{dataset}/items` query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DatasetItemListParams {
    /// Version to read, 0 to the current version. Default: the current version.
    #[serde(default)]
    pub version: Option<i64>,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// `GET /datasets/{dataset}/items/{item_id}` query.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct DatasetItemParams {
    /// Version to read, 0 to the current version. Default: the current version.
    #[serde(default)]
    pub version: Option<i64>,
}

/// Present JSON is `Some(value)`, including null. An absent field stays the default `None`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::<T>::deserialize(deserializer)?))
}

/// Serialized size of one JSON value, in bytes.
fn json_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

/// The format of an import or a preview (D-J-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImportFormat {
    /// Comma-separated values with a header record (RFC 4180).
    Csv,
    /// One JSON object per line.
    Jsonl,
}

/// Where one CSV column goes (D-J-6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportColumn {
    /// The header of the column, exactly as in the file.
    pub column: String,
    /// `input`, `input.<key>`, `expected`, `expected.<key>`, `metadata.<key>` or `ignore`.
    pub target: String,
}

/// The column mapping of a CSV import. Columns the mapping does not list are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportMapping {
    /// One entry per mapped column, at most 1,000.
    pub columns: Vec<ImportColumn>,
}

/// One rejected row of an import or a preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImportRowError {
    /// CSV: the 1-based data record (the header is not counted). JSONL: the 1-based line.
    pub row: i64,
    /// Why the row was rejected.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use utoipa::PartialSchema;

    use super::*;

    #[test]
    fn any_json_fields_accept_any_value() {
        let item: NewDatasetItem = serde_json::from_value(json!({
            "input": "a string",
            "expected_output": [1, 2]
        }))
        .expect("any JSON");
        assert_eq!(item.input, json!("a string"));

        let schema = serde_json::to_value(<NewDatasetItem as PartialSchema>::schema())
            .expect("schema serializes");
        for field in ["input", "expected_output"] {
            let property = &schema["properties"][field];
            assert!(property.is_object(), "{field} is in the schema");
            assert_ne!(
                property["type"],
                json!("object"),
                "{field} is not an object"
            );
        }
    }

    #[test]
    fn update_requests_tell_absent_from_null() {
        let cleared: UpdateDatasetRequest =
            serde_json::from_value(json!({"description": null})).expect("null");
        assert_eq!(cleared.description, Some(None));
        let untouched: UpdateDatasetRequest = serde_json::from_value(json!({})).expect("empty");
        assert_eq!(untouched.description, None);
        let schema: UpdateDatasetRequest =
            serde_json::from_value(json!({"input_schema": {"type": "object"}})).expect("schema");
        assert!(matches!(schema.input_schema, Some(Some(_))));
        assert!(serde_json::from_value::<UpdateDatasetRequest>(json!({"bogus": 1})).is_err());

        let cleared: UpdateItemRequest =
            serde_json::from_value(json!({"expected_output": null})).expect("null");
        assert_eq!(cleared.expected_output, Some(None));
        let untouched: UpdateItemRequest = serde_json::from_value(json!({})).expect("empty");
        assert_eq!(untouched.expected_output, None);
        assert!(serde_json::from_value::<UpdateItemRequest>(json!({"bogus": 1})).is_err());
    }

    #[test]
    fn new_item_debug_prints_no_content() {
        let item = NewDatasetItem {
            input: json!("canary-debug"),
            expected_output: None,
            metadata: None,
        };
        let text = format!("{item:?}");
        assert!(!text.contains("canary-debug"));
    }
}
