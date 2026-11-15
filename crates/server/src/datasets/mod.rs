//! Dataset validation: names, sizes, JSON Schemas (D-J-2, D-J-4). No database access.

pub(crate) mod sql_views;

use std::fmt;

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::api::common::FieldError;
use crate::meta::MetaError;
use crate::meta::datasets::DatasetRow;

/// Longest name, in characters, after trimming.
pub(crate) const MAX_NAME_CHARS: usize = 200;
/// Longest description, in characters.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 2_000;
/// Longest JSON Schema, serialized, in bytes.
pub(crate) const MAX_SCHEMA_BYTES: usize = 64 * 1024;
/// Largest item: `input` + `expected_output` + `metadata`, serialized, in bytes.
pub(crate) const MAX_ITEM_BYTES: usize = 1024 * 1024;
/// Items in one batch add.
pub(crate) const MAX_BATCH_ITEMS: usize = 1_000;
/// Schema failures listed when a schema change is refused.
pub(crate) const MAX_SCHEMA_FAILURES: usize = 20;
/// Item JSON read when a new schema is checked against the live items (D-J-2).
pub(crate) const MAX_SCHEMA_CHECK_BYTES: usize = 64 * 1024 * 1024;

/// An item's values, before or after storage. `Debug` is hand-written: sizes only.
#[derive(Clone, PartialEq)]
pub(crate) struct ItemValues {
    pub(crate) input: Value,
    pub(crate) expected_output: Option<Value>,
    pub(crate) metadata: Map<String, Value>,
}

impl fmt::Debug for ItemValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemValues")
            .field(
                "bytes",
                &json_bytes(&self.input, self.expected_output.as_ref(), &self.metadata),
            )
            .finish_non_exhaustive()
    }
}

/// The compiled schemas of one dataset.
pub(crate) struct Schemas {
    input: Option<jsonschema::Validator>,
    expected: Option<jsonschema::Validator>,
}

/// Trims; 1–200 characters; must not parse as a UUID. Field `name`.
pub(crate) fn check_name(name: &str) -> Result<String, FieldError> {
    let name = name.trim();
    let count = name.chars().count();
    if count == 0 || count > MAX_NAME_CHARS {
        return Err(field_error("name", "must be 1 to 200 characters"));
    }
    if name.parse::<Uuid>().is_ok() {
        return Err(field_error("name", "must not be a UUID"));
    }
    Ok(name.to_owned())
}

/// At most 2 000 characters. An empty string is `None`. Field `description`.
pub(crate) fn check_description(text: &str) -> Result<Option<String>, FieldError> {
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(field_error(
            "description",
            "must be at most 2000 characters",
        ));
    }
    Ok(Some(text.to_owned()))
}

/// Serializes `schema`, checks its size, compiles it (Draft 2020-12, offline). Returns the text
/// to store. Field `field` (`input_schema` or `expected_schema`).
pub(crate) fn check_schema(field: &str, schema: &Value) -> Result<String, FieldError> {
    if !schema.is_object() {
        return Err(field_error(field, "must be a JSON object"));
    }
    let text = serde_json::to_string(schema)
        .map_err(|_| field_error(field, "is not a valid JSON Schema"))?;
    if text.len() > MAX_SCHEMA_BYTES {
        return Err(field_error(field, "must be at most 65536 bytes"));
    }
    if compile(schema).is_none() {
        return Err(field_error(field, "is not a valid JSON Schema"));
    }
    Ok(text)
}

impl Schemas {
    /// Compiles the stored schema texts. A stored text that no longer parses or compiles is
    /// `Err(column)` (`"input_schema"` or `"expected_schema"`).
    pub(crate) fn compile(
        input: Option<&str>,
        expected: Option<&str>,
    ) -> Result<Self, &'static str> {
        Ok(Self {
            input: compile_text(input).map_err(|()| "input_schema")?,
            expected: compile_text(expected).map_err(|()| "expected_schema")?,
        })
    }

    /// [`Self::compile`] of a stored dataset; an `Err(column)` becomes
    /// `MetaError::Corrupt { table: "datasets", column }`.
    // Used by M2's import; no J1 caller.
    #[allow(dead_code)]
    pub(crate) fn of(dataset: &DatasetRow) -> Result<Self, MetaError> {
        Self::compile(
            dataset.input_schema.as_deref(),
            dataset.expected_schema.as_deref(),
        )
        .map_err(|column| MetaError::Corrupt {
            table: "datasets",
            column,
        })
    }
}

/// Every problem with one item, fields prefixed by `prefix` (e.g. `items[3]` gives
/// `items[3].input`; an empty prefix gives `input`).
pub(crate) fn check_item(prefix: &str, item: &ItemValues, schemas: &Schemas) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let input_field = field_path(prefix, "input");
    if item.input.is_null() {
        errors.push(field_error(&input_field, "must not be null"));
    }
    let bytes = json_bytes(&item.input, item.expected_output.as_ref(), &item.metadata);
    if bytes > MAX_ITEM_BYTES {
        let field = if prefix.is_empty() { "item" } else { prefix };
        errors.push(field_error(field, "is larger than 1 MiB"));
    }
    let input_schema = schemas.input.as_ref().filter(|_| !item.input.is_null());
    if let Some(message) = input_schema.and_then(|v| first_failure(v, &item.input)) {
        errors.push(field_error(&input_field, &message));
    }
    if let (Some(expected), Some(validator)) = (&item.expected_output, &schemas.expected)
        && let Some(message) = first_failure(validator, expected)
    {
        errors.push(field_error(
            &field_path(prefix, "expected_output"),
            &message,
        ));
    }
    errors
}

/// Compiles one schema. The compiler's error is dropped: its text can quote the schema.
fn compile(schema: &Value) -> Option<jsonschema::Validator> {
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .offline()
        .build(schema)
        .ok()
}

/// A stored schema text compiled; `Ok(None)` when the column is NULL.
fn compile_text(text: Option<&str>) -> Result<Option<jsonschema::Validator>, ()> {
    let Some(text) = text else {
        return Ok(None);
    };
    let schema = serde_json::from_str::<Value>(text).map_err(|_| ())?;
    compile(&schema).map(Some).ok_or(())
}

/// `"<instance path>: <keyword>"` of the first error, the path `/` at the root.
fn first_failure(validator: &jsonschema::Validator, value: &Value) -> Option<String> {
    let error = validator.iter_errors(value).next()?;
    let path = error.instance_path().to_string();
    let path = if path.is_empty() {
        "/".to_owned()
    } else {
        path
    };
    Some(format!("{path}: {}", error.kind().keyword()))
}

fn field_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

/// The serialized size of an item's three values, in bytes.
pub(crate) fn json_bytes(
    input: &Value,
    expected: Option<&Value>,
    metadata: &Map<String, Value>,
) -> usize {
    let size = |text: Result<String, serde_json::Error>| text.map_or(0, |s| s.len());
    size(serde_json::to_string(input))
        + expected.map_or(0, |value| size(serde_json::to_string(value)))
        + size(serde_json::to_string(metadata))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn faq_schemas() -> Schemas {
        let input = r#"{"type":"object","required":["question"],"properties":{"question":{"type":"string"}}}"#;
        Schemas::compile(Some(input), None).expect("compiles")
    }

    fn item(input: Value, expected: Option<Value>) -> ItemValues {
        ItemValues {
            input,
            expected_output: expected,
            metadata: Map::new(),
        }
    }

    #[test]
    fn names() {
        assert_eq!(check_name("  faq  ").expect("trimmed"), "faq");
        for bad in [
            String::new(),
            "a".repeat(201),
            "01900000-0000-7000-8000-000000000c01".to_owned(),
        ] {
            assert!(check_name(&bad).is_err(), "{bad:?} must be refused");
        }
        let message = check_name("").expect_err("empty").message;
        assert_eq!(message, "must be 1 to 200 characters");
        let message = check_name("01900000-0000-7000-8000-000000000c01")
            .expect_err("uuid")
            .message;
        assert_eq!(message, "must not be a UUID");
        assert_eq!(
            check_name(&"é".repeat(200))
                .expect("200 chars")
                .chars()
                .count(),
            200
        );
    }

    #[test]
    fn descriptions() {
        assert_eq!(check_description("").expect("empty"), None);
        assert!(
            check_description(&"x".repeat(2_000))
                .expect("2000")
                .is_some()
        );
        let error = check_description(&"x".repeat(2_001)).expect_err("2001");
        assert_eq!(error.message, "must be at most 2000 characters");
    }

    #[test]
    fn schemas() {
        let text = check_schema("input_schema", &json!({"type": "object"})).expect("valid");
        assert_eq!(text, r#"{"type":"object"}"#);
        let error = check_schema("input_schema", &json!([])).expect_err("array");
        assert_eq!(error.message, "must be a JSON object");
        let error = check_schema("expected_schema", &json!({"type": 5})).expect_err("type");
        assert_eq!(error.message, "is not a valid JSON Schema");
        assert_eq!(error.field, "expected_schema");
        let big = json!({"description": "x".repeat(MAX_SCHEMA_BYTES)});
        let error = check_schema("input_schema", &big).expect_err("too big");
        assert_eq!(error.message, "must be at most 65536 bytes");
        let remote = json!({"$ref": "https://example.com/s.json"});
        assert!(check_schema("input_schema", &remote).is_err());
    }

    #[test]
    fn items_against_schemas() {
        let schemas = faq_schemas();
        assert!(check_item("items[2]", &item(json!({"question": "x"}), None), &schemas).is_empty());

        let errors = check_item("items[2]", &item(json!({"q": "x"}), None), &schemas);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "items[2].input");
        assert_eq!(errors[0].message, "/: required");
        assert!(!errors[0].message.contains('x'));

        let errors = check_item("items[2]", &item(json!({"question": 5}), None), &schemas);
        assert_eq!(errors[0].message, "/question: type");

        let errors = check_item("items[2]", &item(Value::Null, None), &schemas);
        assert_eq!(errors[0].message, "must not be null");

        let expected_schemas = Schemas::compile(None, Some(r#"{"type":"string"}"#)).expect("ok");
        let errors = check_item(
            "items[2]",
            &item(json!("q"), Some(json!(1))),
            &expected_schemas,
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "items[2].expected_output");
        assert_eq!(errors[0].message, "/: type");

        let none = check_item("items[2]", &item(json!("q"), None), &expected_schemas);
        assert!(none.is_empty());
    }

    #[test]
    fn item_size() {
        let big = json!("x".repeat(1_048_577));
        let no_schemas = Schemas::compile(None, None).expect("no schemas compile");
        let errors = check_item("items[0]", &item(big.clone(), None), &no_schemas);
        assert_eq!(errors[0].field, "items[0]");
        assert_eq!(errors[0].message, "is larger than 1 MiB");
        let errors = check_item(
            "",
            &item(big, None),
            &Schemas::compile(None, None).expect("no schemas compile"),
        );
        assert_eq!(errors[0].field, "item");
    }

    #[test]
    fn item_values_debug_prints_no_content() {
        let values = item(json!("canary-debug"), None);
        let text = format!("{values:?}");
        assert!(!text.contains("canary-debug"));
        assert!(text.contains("bytes"));
    }
}
