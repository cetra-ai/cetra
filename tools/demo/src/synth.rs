//! Builds one generated trace from a fixture template (DO5 step 3).
//!
//! The template's ids are replaced by new random ids, consistently, so parent and
//! link references still resolve. Times are moved so the root starts at the plan's
//! start, and every duration is scaled by one factor. The user, session, environment
//! and release are set, token counts are scaled by one factor, and some roots are
//! marked as errors.

use std::collections::HashMap;

use rand::RngExt;
use rand::rngs::StdRng;
use serde_json::{Value, json};

use crate::otlp::{KeyValue, Resource, ResourceSpans, Span, Status, set_attr, string_value};
use crate::templates::Trace;

/// The per-trace choices the generator draws (see `generate.rs`).
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    /// Where the trace's root starts, in nanoseconds since the Unix epoch.
    pub(crate) start_ns: i64,
    pub(crate) user_id: String,
    pub(crate) session_id: String,
    pub(crate) environment: &'static str,
    pub(crate) release: String,
    pub(crate) error: bool,
    /// Scales every duration and offset of the trace, in 0.5 to 2.0.
    pub(crate) duration_factor: f64,
    /// Scales every token count of the trace, in 0.7 to 1.3.
    pub(crate) token_factor: f64,
}

/// The fixed message of a generated error.
const ERROR_MESSAGE: &str = "simulated upstream failure";
/// OTLP status code for an error.
const STATUS_ERROR: i64 = 2;
/// Attribute keys whose integer values are token counts (spec 02, Model, provider, usage).
const TOKEN_PREFIXES: &[&str] = &["gen_ai.usage.", "llm.token_count.", "ai.usage."];
/// The keys of a span's total tokens (spec 02, `total_tokens`).
const TOTAL_KEYS: &[&str] = &[
    "llm.token_count.total",
    "gen_ai.usage.total_tokens",
    "ai.usage.totalTokens",
];
/// The keys of a span's input tokens, in precedence order (spec 02, `input_tokens`).
const INPUT_KEYS: &[&str] = &[
    "gen_ai.usage.input_tokens",
    "llm.token_count.prompt",
    "gen_ai.usage.prompt_tokens",
    "ai.usage.inputTokens",
    "ai.usage.promptTokens",
];
/// The keys of a span's output tokens, in precedence order (spec 02, `output_tokens`).
const OUTPUT_KEYS: &[&str] = &[
    "gen_ai.usage.output_tokens",
    "llm.token_count.completion",
    "gen_ai.usage.completion_tokens",
    "ai.usage.outputTokens",
    "ai.usage.completionTokens",
];

/// Builds the trace for `plan` from `template`.
pub(crate) fn synthesize(template: &Trace, plan: &Plan, rng: &mut StdRng) -> Trace {
    let mut trace = template.clone();
    let base = root_start(template).unwrap_or(plan.start_ns);
    let mut ids = Ids::default();
    for rs in &mut trace {
        set_resource(rs, plan);
        for scope in &mut rs.scope_spans {
            for span in &mut scope.spans {
                let root = is_root(span);
                retime(span, base, plan);
                remap_ids(span, &mut ids, rng);
                set_attr(&mut span.attributes, "user.id", string_value(&plan.user_id));
                set_attr(
                    &mut span.attributes,
                    "session.id",
                    string_value(&plan.session_id),
                );
                scale_tokens(&mut span.attributes, plan.token_factor);
                if plan.error && root {
                    let status = span.status.get_or_insert_with(Status::default);
                    status.code = STATUS_ERROR;
                    status.message = Some(ERROR_MESSAGE.to_owned());
                }
            }
        }
    }
    trace
}

fn set_resource(rs: &mut ResourceSpans, plan: &Plan) {
    let resource = rs.resource.get_or_insert_with(Resource::default);
    set_attr(
        &mut resource.attributes,
        "deployment.environment",
        string_value(plan.environment),
    );
    set_attr(
        &mut resource.attributes,
        "service.version",
        string_value(&plan.release),
    );
}

/// Moves a span and its events: `base` (the root's start) goes to the plan's start.
fn retime(span: &mut Span, base: i64, plan: &Plan) {
    let factor = plan.duration_factor;
    if let (Ok(start), Ok(end)) = (span.start.parse::<i64>(), span.end.parse::<i64>()) {
        let new_start = shift(start, base, plan.start_ns, factor);
        let duration = scale(end.saturating_sub(start), factor);
        span.start = new_start.to_string();
        span.end = new_start.saturating_add(duration).to_string();
    }
    for event in &mut span.events {
        if let Ok(time) = event.time.parse::<i64>() {
            event.time = shift(time, base, plan.start_ns, factor).to_string();
        }
    }
}

fn shift(at: i64, base: i64, target: i64, factor: f64) -> i64 {
    target.saturating_add(scale(at.saturating_sub(base), factor))
}

fn scale(value: i64, factor: f64) -> i64 {
    (value as f64 * factor).round() as i64
}

/// Replaces the span's and its links' ids. An empty or all-zero id stays as it is.
fn remap_ids(span: &mut Span, ids: &mut Ids, rng: &mut StdRng) {
    span.trace_id = ids.trace(&span.trace_id, rng);
    span.span_id = ids.span(&span.span_id, rng);
    span.parent_span_id = ids.span(&span.parent_span_id, rng);
    for link in &mut span.links {
        link.trace_id = ids.trace(&link.trace_id, rng);
        link.span_id = ids.span(&link.span_id, rng);
    }
}

/// One new id per original id, so every reference to an id gets the same new one.
#[derive(Default)]
struct Ids {
    traces: HashMap<String, String>,
    spans: HashMap<String, String>,
}

impl Ids {
    fn trace(&mut self, original: &str, rng: &mut StdRng) -> String {
        if is_zero(original) {
            return original.to_owned();
        }
        self.traces
            .entry(original.to_owned())
            .or_insert_with(|| fresh_trace_id(rng))
            .clone()
    }

    fn span(&mut self, original: &str, rng: &mut StdRng) -> String {
        if is_zero(original) {
            return original.to_owned();
        }
        self.spans
            .entry(original.to_owned())
            .or_insert_with(|| fresh_span_id(rng))
            .clone()
    }
}

/// A 16-byte trace id as 32 lowercase hex digits, never all zero.
fn fresh_trace_id(rng: &mut StdRng) -> String {
    loop {
        let id = format!("{:016x}{:016x}", rng.random::<u64>(), rng.random::<u64>());
        if !is_zero(&id) {
            return id;
        }
    }
}

/// An 8-byte span id as 16 lowercase hex digits, never all zero.
fn fresh_span_id(rng: &mut StdRng) -> String {
    loop {
        let id = format!("{:016x}", rng.random::<u64>());
        if !is_zero(&id) {
            return id;
        }
    }
}

/// An empty id or one of only zeros: OTLP's "no parent" and "no id".
fn is_zero(id: &str) -> bool {
    id.bytes().all(|byte| byte == b'0')
}

fn is_root(span: &Span) -> bool {
    is_zero(&span.parent_span_id)
}

/// The earliest start of the roots, or of every span when there is no root.
fn root_start(trace: &Trace) -> Option<i64> {
    let spans: Vec<(bool, i64)> = trace
        .iter()
        .flat_map(|rs| rs.scope_spans.iter())
        .flat_map(|scope| scope.spans.iter())
        .filter_map(|span| Some((is_root(span), span.start.parse::<i64>().ok()?)))
        .collect();
    spans
        .iter()
        .filter(|(root, _)| *root)
        .map(|(_, start)| *start)
        .min()
        .or_else(|| spans.iter().map(|(_, start)| *start).min())
}

/// Scales the integer value of every token count. One factor per trace keeps the
/// parts ordered (a cache read never exceeds its input) because rounding is monotone.
/// A total is then the sum of the scaled input and output when the span has both
/// (spec 02 `total_tokens`: cache is inside input and reasoning inside output, so
/// neither is added again). A total on a span with only one of them just scales.
fn scale_tokens(attrs: &mut [KeyValue], factor: f64) {
    for kv in attrs.iter_mut().filter(|kv| {
        TOKEN_PREFIXES
            .iter()
            .any(|prefix| kv.key.starts_with(prefix))
    }) {
        if let Some(count) = int_value(&kv.value) {
            set_int_value(&mut kv.value, scale(count, factor).max(0));
        }
    }
    if let (Some(input), Some(output)) = (
        first_count(attrs, INPUT_KEYS),
        first_count(attrs, OUTPUT_KEYS),
    ) {
        let total = input.saturating_add(output);
        for kv in attrs
            .iter_mut()
            .filter(|kv| TOTAL_KEYS.contains(&kv.key.as_str()))
        {
            set_int_value(&mut kv.value, total);
        }
    }
}

/// The first of `keys` that is on the span with an integer value (spec 02 precedence).
fn first_count(attrs: &[KeyValue], keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        attrs
            .iter()
            .find(|kv| kv.key == *key)
            .and_then(|kv| int_value(&kv.value))
    })
}

/// The integer of an attribute's `intValue`, which OTLP/JSON writes as a string or a number.
fn int_value(value: &Value) -> Option<i64> {
    match value.get("intValue")? {
        Value::String(text) => text.parse().ok(),
        other => other.as_i64(),
    }
}

/// Sets the `intValue`, keeping its JSON type (string or number).
fn set_int_value(value: &mut Value, count: i64) {
    if let Some(int) = value.get_mut("intValue") {
        *int = if int.is_string() {
            Value::String(count.to_string())
        } else {
            json!(count)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::templates::fixture_templates;
    use rand::SeedableRng;
    use std::collections::HashSet;

    fn plan(start_ns: i64, duration_factor: f64, token_factor: f64, error: bool) -> Plan {
        Plan {
            start_ns,
            user_id: "user-0007".to_owned(),
            session_id: "sess-0000000000000001".to_owned(),
            environment: "staging",
            release: "1.9.0".to_owned(),
            error,
            duration_factor,
            token_factor,
        }
    }

    fn all_spans(trace: &Trace) -> Vec<&Span> {
        trace
            .iter()
            .flat_map(|rs| rs.scope_spans.iter())
            .flat_map(|scope| scope.spans.iter())
            .collect()
    }

    #[test]
    fn ids_are_new_and_references_stay_consistent() {
        let mut rng = StdRng::seed_from_u64(1);
        for template in fixture_templates() {
            let plan = plan(1_800_000_000_000_000_000, 1.0, 1.0, false);
            let built = synthesize(&template, &plan, &mut rng);
            let original = all_spans(&template);
            let new = all_spans(&built);
            assert_eq!(original.len(), new.len());
            let old_ids: HashSet<&str> = original.iter().map(|s| s.span_id.as_str()).collect();
            let new_ids: HashSet<&str> = new.iter().map(|s| s.span_id.as_str()).collect();
            let old_traces: HashSet<&str> = original.iter().map(|s| s.trace_id.as_str()).collect();
            for (before, after) in original.iter().zip(&new) {
                assert!(
                    !old_traces.contains(after.trace_id.as_str()),
                    "trace id kept"
                );
                assert!(!old_ids.contains(after.span_id.as_str()), "span id kept");
                if is_zero(&before.parent_span_id) {
                    assert!(is_zero(&after.parent_span_id));
                } else {
                    assert_eq!(
                        old_ids.contains(before.parent_span_id.as_str()),
                        new_ids.contains(after.parent_span_id.as_str()),
                        "parent link broke"
                    );
                }
                for (link_before, link_after) in before.links.iter().zip(&after.links) {
                    assert_eq!(
                        old_ids.contains(link_before.span_id.as_str()),
                        new_ids.contains(link_after.span_id.as_str()),
                        "link broke"
                    );
                }
            }
        }
    }

    #[test]
    fn every_span_carries_the_context_and_the_resource_carries_the_environment() {
        let mut rng = StdRng::seed_from_u64(2);
        let template = fixture_templates().swap_remove(0);
        let plan = plan(1_800_000_000_000_000_000, 1.0, 1.0, false);
        let built = synthesize(&template, &plan, &mut rng);
        for rs in &built {
            let resource = rs.resource.as_ref().expect("resource set");
            let env = resource
                .attributes
                .iter()
                .find(|kv| kv.key == "deployment.environment")
                .expect("environment set");
            assert_eq!(env.value["stringValue"], "staging");
        }
        for span in all_spans(&built) {
            let user = span.attributes.iter().find(|kv| kv.key == "user.id");
            assert_eq!(user.expect("user.id set").value["stringValue"], "user-0007");
        }
    }

    #[test]
    fn the_root_starts_at_the_target_and_durations_scale() {
        let mut rng = StdRng::seed_from_u64(3);
        let template = fixture_templates().swap_remove(1);
        let target = 1_800_000_000_000_000_000_i64;
        let factor = 1.7;
        let built = synthesize(&template, &plan(target, factor, 1.0, false), &mut rng);
        let original = all_spans(&template);
        let new = all_spans(&built);
        let base = root_start(&template).expect("template has spans");
        let new_root = new
            .iter()
            .zip(&original)
            .filter(|(_, before)| is_zero(&before.parent_span_id))
            .map(|(after, _)| after.start.parse::<i64>().expect("start"))
            .min()
            .expect("a root");
        assert_eq!(new_root, target);
        for (before, after) in original.iter().zip(&new) {
            let start = before.start.parse::<i64>().expect("start");
            let end = before.end.parse::<i64>().expect("end");
            let expected_start = target + scale(start - base, factor);
            assert_eq!(after.start.parse::<i64>().expect("start"), expected_start);
            let duration = after.end.parse::<i64>().expect("end") - expected_start;
            assert_eq!(duration, scale(end - start, factor));
        }
    }

    #[test]
    fn token_counts_scale_stay_integers_and_keep_their_order() {
        let template: Trace = serde_json::from_value(json!([{
            "scopeSpans": [{"spans": [{
                "traceId": "00000000000000000000000000000001",
                "spanId": "0000000000000001",
                "startTimeUnixNano": "1000",
                "endTimeUnixNano": "2000",
                "attributes": [
                    {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "100"}},
                    {"key": "gen_ai.usage.cache_read.input_tokens", "value": {"intValue": 80}},
                    {"key": "gen_ai.usage.output_tokens", "value": {"intValue": "10"}},
                    {"key": "gen_ai.request.model", "value": {"stringValue": "m"}}
                ]
            }]}]
        }]))
        .expect("a valid trace");
        let mut rng = StdRng::seed_from_u64(4);
        let built = synthesize(&template, &plan(5000, 1.0, 0.7, false), &mut rng);
        let span = all_spans(&built)[0];
        let value = |key: &str| {
            span.attributes
                .iter()
                .find(|kv| kv.key == key)
                .expect("present")
                .value
                .clone()
        };
        assert_eq!(value("gen_ai.usage.input_tokens")["intValue"], "70");
        assert_eq!(
            value("gen_ai.usage.cache_read.input_tokens")["intValue"],
            56
        );
        assert_eq!(value("gen_ai.usage.output_tokens")["intValue"], "7");
        assert_eq!(value("gen_ai.request.model")["stringValue"], "m");
    }

    #[test]
    fn an_error_marks_only_the_root() {
        let template: Trace = serde_json::from_value(json!([{
            "scopeSpans": [{"spans": [
                {"traceId": "a1", "spanId": "b1", "parentSpanId": "",
                 "startTimeUnixNano": "1000", "endTimeUnixNano": "9000"},
                {"traceId": "a1", "spanId": "b2", "parentSpanId": "b1",
                 "startTimeUnixNano": "2000", "endTimeUnixNano": "3000"}
            ]}]
        }]))
        .expect("a valid trace");
        let mut rng = StdRng::seed_from_u64(5);
        let built = synthesize(&template, &plan(10_000, 1.0, 1.0, true), &mut rng);
        let spans = all_spans(&built);
        let root = spans[0].status.as_ref().expect("root has a status");
        assert_eq!(root.code, STATUS_ERROR);
        assert_eq!(root.message.as_deref(), Some(ERROR_MESSAGE));
        assert!(spans[1].status.is_none());
    }

    fn span_with(attributes: &Value) -> Trace {
        serde_json::from_value(json!([{"scopeSpans": [{"spans": [{
            "traceId": "00000000000000000000000000000001",
            "spanId": "0000000000000001",
            "startTimeUnixNano": "1000",
            "endTimeUnixNano": "2000",
            "attributes": attributes
        }]}]}]))
        .expect("a valid trace")
    }

    #[test]
    fn the_total_is_the_sum_of_the_scaled_input_and_output() {
        let template = span_with(&json!([
            {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "101"}},
            {"key": "gen_ai.usage.cache_read.input_tokens", "value": {"intValue": "80"}},
            {"key": "gen_ai.usage.output_tokens", "value": {"intValue": 11}},
            {"key": "gen_ai.usage.total_tokens", "value": {"intValue": "112"}},
            {"key": "llm.token_count.total", "value": {"intValue": 112}}
        ]));
        for factor in [0.7, 1.0, 1.3] {
            let mut rng = StdRng::seed_from_u64(6);
            let built = synthesize(&template, &plan(5000, 1.0, factor, false), &mut rng);
            let span = all_spans(&built)[0];
            let int = |key: &str| {
                let attr = span
                    .attributes
                    .iter()
                    .find(|kv| kv.key == key)
                    .expect("present");
                int_value(&attr.value).expect("an integer")
            };
            let sum = int("gen_ai.usage.input_tokens") + int("gen_ai.usage.output_tokens");
            assert_eq!(int("gen_ai.usage.total_tokens"), sum, "factor {factor}");
            assert_eq!(int("llm.token_count.total"), sum, "factor {factor}");
        }
    }

    #[test]
    fn a_total_without_both_parts_only_scales() {
        let template = span_with(&json!([
            {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "100"}},
            {"key": "gen_ai.usage.total_tokens", "value": {"intValue": "110"}}
        ]));
        let mut rng = StdRng::seed_from_u64(7);
        let built = synthesize(&template, &plan(5000, 1.0, 0.7, false), &mut rng);
        let span = all_spans(&built)[0];
        let total = span
            .attributes
            .iter()
            .find(|kv| kv.key == "gen_ai.usage.total_tokens")
            .expect("present");
        assert_eq!(total.value["intValue"], "77");
    }
}
