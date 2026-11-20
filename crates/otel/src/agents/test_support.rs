//! Builders shared by the coding-agent tests. Span ids are the same for every
//! span built here, so a test that sends two spans sets the second id itself.

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::span::Event;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span as OtlpSpan};

use crate::agents::AgentContexts;
use crate::cost::{PriceRow, PriceTable};
use crate::{IngestContext, IngestLimits, Normalized};

/// Inside the test context's window (see [`context`]).
const START_NS: u64 = 1_767_225_590_000_000_000;

pub(crate) fn kv(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(value),
        ..KeyValue::default()
    }
}

pub(crate) fn s(text: &str) -> AnyValue {
    AnyValue {
        value: Some(Value::StringValue(text.to_owned())),
    }
}

pub(crate) fn i(number: i64) -> AnyValue {
    AnyValue {
        value: Some(Value::IntValue(number)),
    }
}

pub(crate) fn event(name: &str, attributes: Vec<KeyValue>) -> Event {
    Event {
        time_unix_nano: START_NS,
        name: name.to_owned(),
        attributes,
        ..Event::default()
    }
}

/// A valid span with a one-second duration, inside the test context's window.
pub(crate) fn span(name: &str, attributes: Vec<KeyValue>, events: Vec<Event>) -> OtlpSpan {
    OtlpSpan {
        trace_id: vec![0x11; 16],
        span_id: vec![0x22; 8],
        name: name.to_owned(),
        start_time_unix_nano: START_NS,
        end_time_unix_nano: START_NS + 1_000_000_000,
        attributes,
        events,
        ..OtlpSpan::default()
    }
}

/// One resource (`service.name`) and one scope (`scope_name`, empty for none).
pub(crate) fn request(
    service_name: &str,
    scope_name: &str,
    spans: Vec<OtlpSpan>,
) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", s(service_name))],
                ..Resource::default()
            }),
            scope_spans: vec![ScopeSpans {
                scope: Some(InstrumentationScope {
                    name: scope_name.to_owned(),
                    ..InstrumentationScope::default()
                }),
                spans,
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    }
}

fn context() -> IngestContext {
    IngestContext {
        project_id: "01900000-0000-7000-8000-000000000001"
            .parse()
            .expect("project id"),
        ingest_time: 1_767_225_600_000_000_000,
        retention_days: Some(7),
        limits: IngestLimits::default(),
    }
}

/// Normalize with the test context, an empty price table and `agents`.
pub(crate) fn run(request: &ExportTraceServiceRequest, agents: &AgentContexts) -> Normalized {
    let prices = PriceTable::new(Vec::new()).expect("empty price table");
    crate::normalize(request, &context(), &prices, agents)
}

/// Normalize with the test context, `rows` as the price table and no agent contexts.
pub(crate) fn run_priced(request: &ExportTraceServiceRequest, rows: Vec<PriceRow>) -> Normalized {
    let prices = PriceTable::new(rows).expect("price table");
    crate::normalize(request, &context(), &prices, &AgentContexts::default())
}
