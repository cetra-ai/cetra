//! OTLP logs: decoding and the count of GenAI content records (spec 02 § Logs and metrics,
//! D-G-40).
//!
//! Logs are not stored. Some GenAI instrumentations send prompts and completions as log
//! events instead of span attributes; the server counts those records per project so the UI
//! can say that the content was dropped.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use prost::Message;

use crate::error::DecodeError;

/// Prefix of the GenAI semantic-convention event names and attribute keys.
const GENAI_PREFIX: &str = "gen_ai.";
/// The attribute that carried the event name before `LogRecord.event_name` existed.
const EVENT_NAME_ATTRIBUTE: &str = "event.name";

/// Decode a protobuf `ExportLogsServiceRequest`.
///
/// # Errors
///
/// Returns [`DecodeError`] when `body` is not an OTLP logs request.
pub fn decode_logs_protobuf(body: &[u8]) -> Result<ExportLogsServiceRequest, DecodeError> {
    ExportLogsServiceRequest::decode(body).map_err(DecodeError::Protobuf)
}

/// Decode an OTLP/JSON `ExportLogsServiceRequest`.
///
/// # Errors
///
/// Returns [`DecodeError`] when `body` is not an OTLP logs request.
pub fn decode_logs_json(body: &[u8]) -> Result<ExportLogsServiceRequest, DecodeError> {
    serde_json::from_slice(body).map_err(|err| DecodeError::from_json(&err))
}

/// The records of `request` that look like GenAI content events: an event name that starts
/// with `gen_ai.` (the field, or the older `event.name` attribute), or any attribute whose
/// key starts with `gen_ai.`.
#[must_use]
pub fn count_genai_log_records(request: &ExportLogsServiceRequest) -> u64 {
    let count = request
        .resource_logs
        .iter()
        .flat_map(|resource| resource.scope_logs.iter())
        .flat_map(|scope| scope.log_records.iter())
        .filter(|record| is_genai_record(record))
        .count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

fn is_genai_record(record: &LogRecord) -> bool {
    record.event_name.starts_with(GENAI_PREFIX) || record.attributes.iter().any(is_genai_attribute)
}

fn is_genai_attribute(attribute: &KeyValue) -> bool {
    attribute.key.starts_with(GENAI_PREFIX)
        || (attribute.key == EVENT_NAME_ATTRIBUTE
            && matches!(
                attribute.value.as_ref().and_then(|value| value.value.as_ref()),
                Some(Value::StringValue(name)) if name.starts_with(GENAI_PREFIX)
            ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_json(body: &str) -> u64 {
        count_genai_log_records(&decode_logs_json(body.as_bytes()).expect("valid OTLP/JSON"))
    }

    #[test]
    fn counts_event_names_and_attributes() {
        let body = r#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[
            {"eventName":"gen_ai.user.message","body":{"stringValue":"hi"}},
            {"attributes":[{"key":"event.name","value":{"stringValue":"gen_ai.choice"}}]},
            {"attributes":[{"key":"gen_ai.system","value":{"stringValue":"openai"}}]},
            {"eventName":"http.request","attributes":[{"key":"event.name","value":{"stringValue":"http"}}]},
            {"body":{"stringValue":"plain log"}}
        ]}]}]}"#;
        assert_eq!(count_json(body), 3);
    }

    #[test]
    fn counts_across_resources_and_scopes() {
        let body = r#"{"resourceLogs":[
            {"scopeLogs":[{"logRecords":[{"eventName":"gen_ai.choice"}]},{"logRecords":[{"eventName":"gen_ai.choice"}]}]},
            {"scopeLogs":[{"logRecords":[{"eventName":"gen_ai.system.message"}]}]}
        ]}"#;
        assert_eq!(count_json(body), 3);
        assert_eq!(count_json("{}"), 0);
    }

    #[test]
    fn protobuf_and_json_agree() {
        let request = decode_logs_json(br#"{"resourceLogs":[{"scopeLogs":[{"logRecords":[{"eventName":"gen_ai.choice"},{"body":{"stringValue":"x"}}]}]}]}"#)
            .expect("valid OTLP/JSON");
        let decoded = decode_logs_protobuf(&request.encode_to_vec()).expect("valid protobuf");
        assert_eq!(count_genai_log_records(&decoded), 1);
    }

    #[test]
    fn malformed_bodies_are_decode_errors() {
        assert!(decode_logs_json(b"{").is_err());
        assert!(decode_logs_protobuf(&[0xff, 0xff, 0xff]).is_err());
    }
}
