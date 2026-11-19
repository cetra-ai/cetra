//! The part of an OTLP/JSON `ExportTraceServiceRequest` the generator reads and
//! changes. Every other field is kept in `rest`, so a built trace carries the
//! fixture's own fields unchanged (spec 02, OTLP/JSON).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One `resourceSpans` entry. A generated trace is a list of these.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ResourceSpans {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resource: Option<Resource>,
    #[serde(rename = "scopeSpans", default)]
    pub(crate) scope_spans: Vec<ScopeSpans>,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Resource {
    #[serde(default)]
    pub(crate) attributes: Vec<KeyValue>,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ScopeSpans {
    #[serde(default)]
    pub(crate) spans: Vec<Span>,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Span {
    #[serde(rename = "traceId", default)]
    pub(crate) trace_id: String,
    #[serde(rename = "spanId", default)]
    pub(crate) span_id: String,
    #[serde(rename = "parentSpanId", default)]
    pub(crate) parent_span_id: String,
    /// `startTimeUnixNano`, a decimal string in OTLP/JSON.
    #[serde(rename = "startTimeUnixNano", default)]
    pub(crate) start: String,
    /// `endTimeUnixNano`, a decimal string in OTLP/JSON.
    #[serde(rename = "endTimeUnixNano", default)]
    pub(crate) end: String,
    #[serde(default)]
    pub(crate) attributes: Vec<KeyValue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) links: Vec<Link>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<Status>,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Event {
    #[serde(rename = "timeUnixNano", default)]
    pub(crate) time: String,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Link {
    #[serde(rename = "traceId", default)]
    pub(crate) trace_id: String,
    #[serde(rename = "spanId", default)]
    pub(crate) span_id: String,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Status {
    /// The OTLP status code as an integer: 0 unset, 1 ok, 2 error.
    #[serde(default)]
    pub(crate) code: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

/// An OTLP `KeyValue`. `value` is the `AnyValue` object, kept as JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct KeyValue {
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) value: Value,
    #[serde(flatten)]
    pub(crate) rest: Map<String, Value>,
}

/// The body of `POST /v1/traces`. Borrows the traces it sends.
#[derive(Debug, Serialize)]
pub(crate) struct ExportRequest<'a> {
    #[serde(rename = "resourceSpans")]
    pub(crate) resource_spans: Vec<&'a ResourceSpans>,
}

/// A value of an attribute, when it is a string.
pub(crate) fn string_value(value: &str) -> Value {
    serde_json::json!({ "stringValue": value })
}

/// Sets `key` to `value`, replacing an existing entry with the same key.
pub(crate) fn set_attr(attrs: &mut Vec<KeyValue>, key: &str, value: Value) {
    match attrs.iter_mut().find(|kv| kv.key == key) {
        Some(kv) => kv.value = value,
        None => attrs.push(KeyValue {
            key: key.to_owned(),
            value,
            rest: Map::new(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let input = json!({
            "resource": {"attributes": [], "droppedAttributesCount": 3},
            "scopeSpans": [{"scope": {"name": "x"}, "spans": [{
                "traceId": "00000000000000000000000000000001",
                "spanId": "0000000000000001",
                "startTimeUnixNano": "10",
                "endTimeUnixNano": "20",
                "kind": 2,
                "flags": 1
            }]}],
            "schemaUrl": "https://example.test/schema"
        });
        let parsed: ResourceSpans = serde_json::from_value(input).expect("parses");
        let back = serde_json::to_value(&parsed).expect("serializes");
        assert_eq!(back["schemaUrl"], "https://example.test/schema");
        assert_eq!(back["resource"]["droppedAttributesCount"], 3);
        assert_eq!(back["scopeSpans"][0]["scope"]["name"], "x");
        assert_eq!(back["scopeSpans"][0]["spans"][0]["kind"], 2);
        assert_eq!(back["scopeSpans"][0]["spans"][0]["flags"], 1);
    }

    #[test]
    fn set_attr_replaces_or_appends() {
        let mut attrs = vec![KeyValue {
            key: "a".to_owned(),
            value: string_value("old"),
            rest: Map::new(),
        }];
        set_attr(&mut attrs, "a", string_value("new"));
        set_attr(&mut attrs, "b", string_value("x"));
        assert_eq!(attrs.len(), 2);
        assert_eq!(attrs[0].value["stringValue"], "new");
        assert_eq!(attrs[1].key, "b");
    }
}
