//! Dataset request and response types (spec 09 § Datasets & experiments, D-J-2 to D-J-5).

use std::fmt;

use llmobs_core::ids::{SpanId, TraceId};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::{Limit, any_json};
use crate::datasets::json_bytes;

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
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
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

// `Debug` is hand-written: ids, versions and sizes only, never content (AGENTS.md § Privacy).
impl fmt::Debug for DatasetItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = json_bytes(&self.input, self.expected_output.as_ref(), &self.metadata);
        f.debug_struct("DatasetItem")
            .field("id", &self.id)
            .field("revision_id", &self.revision_id)
            .field("version", &self.version)
            .field("bytes", &bytes)
            .finish_non_exhaustive()
    }
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
        let metadata = self.metadata.clone().unwrap_or_default();
        let bytes = json_bytes(&self.input, self.expected_output.as_ref(), &metadata);
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

/// `PATCH …/items/{item_id}` body. An absent field is unchanged; `expected_output: null` clears
/// it. `input` and `metadata` must not be null (400).
#[derive(Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateItemRequest {
    /// New input, when present. Must not be null.
    #[serde(default, deserialize_with = "present")]
    #[schema(schema_with = any_json_not_null)]
    pub input: Option<Option<Value>>,
    /// New expected output, when present. `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Option<Value>>,
    /// New metadata, when present. A JSON object; must not be null.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Option<Map<String, Value>>>,
}

// `Debug` is hand-written: presence flags and sizes only, never content (AGENTS.md § Privacy).
impl fmt::Debug for UpdateItemRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let input = self.input.clone().flatten().unwrap_or(Value::Null);
        let expected = self.expected_output.clone().flatten();
        let metadata = self.metadata.clone().flatten().unwrap_or_default();
        let bytes = json_bytes(&input, expected.as_ref(), &metadata);
        f.debug_struct("UpdateItemRequest")
            .field("input_present", &self.input.is_some())
            .field("expected_output_present", &self.expected_output.is_some())
            .field("metadata_present", &self.metadata.is_some())
            .field("bytes", &bytes)
            .finish_non_exhaustive()
    }
}

/// `POST …/items/from-trace` body (D-J-8). Fields left out take the span's values.
#[derive(Clone, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FromTraceRequest {
    /// 32 hex digits, either case.
    pub trace_id: String,
    /// 16 hex digits. Absent: the trace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Replaces the span's input. Any JSON; `null` keeps the span's input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(schema_with = any_json)]
    pub input: Option<Value>,
    /// Replaces the span's output. Any JSON; `null` stores no expected output. Absent keeps
    /// the span's output.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(schema_with = any_json)]
    pub expected_output: Option<Option<Value>>,
    /// The item's metadata, a JSON object. Absent: `{}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub metadata: Option<Map<String, Value>>,
}

// `Debug` is hand-written: presence flags and sizes only, never content (AGENTS.md § Privacy).
impl fmt::Debug for FromTraceRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let input = self.input.clone().unwrap_or(Value::Null);
        let expected = self.expected_output.clone().flatten();
        let metadata = self.metadata.clone().unwrap_or_default();
        let bytes = json_bytes(&input, expected.as_ref(), &metadata);
        f.debug_struct("FromTraceRequest")
            .field("span_id_present", &self.span_id.is_some())
            .field("input_present", &self.input.is_some())
            .field("expected_output_present", &self.expected_output.is_some())
            .field("metadata_present", &self.metadata.is_some())
            .field("bytes", &bytes)
            .finish_non_exhaustive()
    }
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImportRowError {
    /// CSV: the 1-based data record (the header is not counted). JSONL: the 1-based line.
    pub row: i64,
    /// Why the row was rejected.
    pub message: String,
}

// `Debug` is hand-written: the row only, never the message (AGENTS.md § Privacy).
impl fmt::Debug for ImportRowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportRowError")
            .field("row", &self.row)
            .finish_non_exhaustive()
    }
}

/// `?format=&partial=` of the preview.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct PreviewParams {
    /// The file's format.
    pub format: ImportFormat,
    /// The body is the first part of a larger file (the browser sends at most 1 MiB).
    #[serde(default)]
    pub partial: bool,
}

/// `?format=` of the import.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ImportParams {
    /// The file's format.
    pub format: ImportFormat,
}

/// The format of an export (D-J-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    /// One JSON object per line.
    Jsonl,
    /// Comma-separated values with a header record (RFC 4180).
    Csv,
}

/// `?version=&format=` of the export.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ExportParams {
    /// The version to export; default the latest.
    pub version: Option<i64>,
    /// Default `jsonl`.
    pub format: Option<ExportFormat>,
}

/// The head of a file as the import wizard shows it (D-J-6).
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ImportPreview {
    pub format: ImportFormat,
    /// CSV: the header. JSONL: the top-level keys of the shown rows, in first-seen order.
    pub columns: Vec<String>,
    /// At most 20. CSV: arrays of cell strings. JSONL: the parsed lines.
    #[schema(schema_with = any_json_array)]
    pub rows: Vec<Value>,
    /// Lines of the head that could not be read.
    pub errors: Vec<ImportRowError>,
    /// More rows follow, or the head was cut.
    pub truncated: bool,
}

// `Debug` is hand-written: counts and flags only, never the rows (AGENTS.md § Privacy).
impl fmt::Debug for ImportPreview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportPreview")
            .field("format", &self.format)
            .field("columns", &self.columns.len())
            .field("rows", &self.rows.len())
            .field("errors", &self.errors.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// The outcome of an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ImportResult {
    /// The version the import made.
    pub version: i64,
    /// Items imported.
    pub imported: i64,
    /// Rows rejected, all of them.
    pub error_count: i64,
    /// The first 100 rejected rows.
    pub errors: Vec<ImportRowError>,
}

/// OpenAPI schema of `UpdateItemRequest.input`: any JSON value but null.
fn any_json_not_null() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
    utoipa::openapi::schema::ObjectBuilder::new()
        .schema_type(utoipa::openapi::schema::SchemaType::AnyValue)
        .description(Some("Any JSON value but null."))
        .into()
}

/// OpenAPI schema of `ImportPreview.rows`: an array of any JSON values.
fn any_json_array() -> utoipa::openapi::schema::Array {
    utoipa::openapi::schema::ArrayBuilder::new()
        .items(any_json())
        .build()
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
    fn item_debug_prints_no_content() {
        let item = NewDatasetItem {
            input: json!("canary-debug"),
            expected_output: None,
            metadata: None,
        };
        let text = format!("{item:?}");
        assert!(!text.contains("canary-debug"));

        let edit = UpdateItemRequest {
            input: Some(Some(json!("canary-patch"))),
            expected_output: Some(Some(json!("canary-patch-expected"))),
            metadata: Some(Some(Map::from_iter([(
                "k".to_owned(),
                json!("canary-patch-meta"),
            )]))),
        };
        let text = format!("{edit:?}");
        assert!(text.contains("bytes"), "{text}");
        for canary in ["canary-patch", "canary-patch-expected", "canary-patch-meta"] {
            assert!(!text.contains(canary), "{text}");
        }

        let stored = DatasetItem {
            id: Uuid::nil(),
            revision_id: Uuid::nil(),
            version: 3,
            input: json!("canary-stored"),
            expected_output: Some(json!("canary-stored-expected")),
            metadata: Map::new(),
            source_trace_id: None,
            source_span_id: None,
            created_at: 0,
        };
        let text = format!("{stored:?}");
        assert!(text.contains("version: 3"), "{text}");
        assert!(!text.contains("canary-stored"), "{text}");
    }

    #[test]
    fn import_debug_prints_no_content() {
        let preview = ImportPreview {
            format: ImportFormat::Jsonl,
            columns: vec!["canary-column".to_owned()],
            rows: vec![json!({"input": "canary-row"})],
            errors: vec![ImportRowError {
                row: 7,
                message: "canary-message".to_owned(),
            }],
            truncated: true,
        };
        let text = format!("{preview:?}");
        assert!(text.contains("truncated: true"), "{text}");
        assert!(text.contains("rows: 1"), "{text}");
        for canary in ["canary-column", "canary-row", "canary-message"] {
            assert!(!text.contains(canary), "{text}");
        }
        let error = format!(
            "{:?}",
            ImportRowError {
                row: 3,
                message: "canary-error".to_owned(),
            }
        );
        assert!(error.contains("row: 3"), "{error}");
        assert!(!error.contains("canary-error"), "{error}");
    }
}
