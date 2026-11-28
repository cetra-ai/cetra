//! The server's spans (track TL M2, D-TL-4). Every span is built here, by one constructor,
//! so the names and attribute keys are constants and reviewable in one place. Values are ids,
//! counts, fixed strings and durations: never a URL, a query string, a body, SQL text, a name,
//! an email or an error's text. A field declared `Empty` is recorded later with
//! `span.record(KEY, value)`; counts go through [`count`], because `tracing-opentelemetry`
//! records a `u64` as a string. Spans cost little when no layer records them (no endpoint).

use cetra_core::ids::ProjectId;
use tracing::Span;
use tracing::field::Empty;
use uuid::Uuid;

/// `otel.kind`: the OTel span kind (`tracing-opentelemetry` special field).
pub(crate) const OTEL_KIND: &str = "otel.kind";
/// `otel.status_code`: `"error"` marks the span failed (`tracing-opentelemetry` special field).
pub(crate) const OTEL_STATUS: &str = "otel.status_code";
/// The request method, `GET` … `OPTIONS` or `other` (`telemetry::Method`).
pub(crate) const METHOD: &str = "http.request.method";
/// The matched route template (D150), or `<unmatched>`.
pub(crate) const ROUTE: &str = "http.route";
/// The response status, an integer.
pub(crate) const STATUS_CODE: &str = "http.response.status_code";
/// `http` or `grpc`.
pub(crate) const PROTOCOL: &str = "protocol";
pub(crate) const PROJECT_ID: &str = "project_id";
/// Spans accepted by an export.
pub(crate) const SPANS: &str = "spans";
/// Spans rejected by an export.
pub(crate) const REJECTED: &str = "rejected";
pub(crate) const BYTES: &str = "bytes";
/// A fixed word per span: `ok` or why not.
pub(crate) const OUTCOME: &str = "outcome";
pub(crate) const KIND: &str = "kind";
/// An MCP tool name, from the tool list (never the caller's string).
pub(crate) const TOOL: &str = "tool";
pub(crate) const ROWS: &str = "rows";
pub(crate) const JOB_ID: &str = "job_id";
pub(crate) const EVALUATOR_ID: &str = "evaluator_id";
pub(crate) const RULE_ID: &str = "rule_id";
pub(crate) const DELIVERY_ID: &str = "delivery_id";
pub(crate) const CHANNEL_ID: &str = "channel_id";
pub(crate) const ATTEMPT: &str = "attempt";
pub(crate) const DELETION_ID: &str = "deletion_id";
pub(crate) const PROJECTS: &str = "projects";
pub(crate) const FILES: &str = "files";
pub(crate) const FAILURES: &str = "failures";
pub(crate) const CANDIDATE_ID: &str = "candidate_id";
pub(crate) const SOURCE_ID: &str = "source_id";
pub(crate) const MODE: &str = "mode";
pub(crate) const OBJECTS: &str = "objects";

/// [`PROTOCOL`] of an OTLP/HTTP export.
pub(crate) const PROTOCOL_HTTP: &str = "http";
/// [`PROTOCOL`] of an OTLP/gRPC export.
pub(crate) const PROTOCOL_GRPC: &str = "grpc";
/// [`KIND`] of a SQL console query answered as JSON.
pub(crate) const QUERY_SQL: &str = "sql";
/// [`KIND`] of a SQL console export (the span covers its start, to the first rows).
pub(crate) const QUERY_SQL_EXPORT: &str = "sql_export";
/// [`KIND`] of an MCP tool call.
pub(crate) const QUERY_MCP: &str = "mcp";

/// `value` as an `i64` attribute, `i64::MAX` when it does not fit.
pub(crate) fn count<T: TryInto<i64>>(value: T) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

/// An eval job's `kind` column as a fixed word: `rule`, `manual`, `backfill`, else `other`.
pub(crate) fn job_kind(kind: &str) -> &'static str {
    match kind {
        "rule" => "rule",
        "manual" => "manual",
        "backfill" => "backfill",
        _ => "other",
    }
}

/// One HTTP request, from the request log middleware (`http::log_request`).
pub(crate) fn http_request(method: &axum::http::Method, route: &str) -> Span {
    tracing::info_span!(
        "http.request",
        { OTEL_KIND } = "server",
        { METHOD } = ?crate::telemetry::Method::of(method),
        { ROUTE } = route,
        { STATUS_CODE } = Empty,
        { OTEL_STATUS } = Empty
    )
}

/// One OTLP trace export (`protocol` is [`PROTOCOL_HTTP`] or [`PROTOCOL_GRPC`]). Over gRPC it
/// is the request's root, so its kind is `server`; over HTTP it is a child of `http.request`.
pub(crate) fn otlp_export(protocol: &'static str) -> Span {
    let kind = if protocol == PROTOCOL_GRPC {
        "server"
    } else {
        "internal"
    };
    tracing::info_span!(
        "otlp.export",
        { OTEL_KIND } = kind,
        { PROTOCOL } = protocol,
        { PROJECT_ID } = Empty,
        { SPANS } = Empty,
        { REJECTED } = Empty,
        { BYTES } = Empty,
        { OUTCOME } = Empty
    )
}

/// One query of `kind` ([`QUERY_SQL`], [`QUERY_SQL_EXPORT`], [`QUERY_MCP`]), child of
/// `parent`. Never carries the SQL text.
pub(crate) fn query(parent: &Span, kind: &'static str) -> Span {
    tracing::info_span!(
        parent: parent,
        "query",
        { KIND } = kind,
        { PROJECT_ID } = Empty,
        { TOOL } = Empty,
        { ROWS } = Empty,
        { OUTCOME } = Empty
    )
}

/// One eval job run (`eval::dispatch::run_claimed`).
pub(crate) fn eval_job(job_id: Uuid, project: ProjectId, evaluator_id: Uuid, kind: &str) -> Span {
    tracing::info_span!(
        "eval.job",
        { JOB_ID } = %job_id,
        { PROJECT_ID } = %project,
        { EVALUATOR_ID } = %evaluator_id,
        { KIND } = job_kind(kind),
        { OUTCOME } = Empty
    )
}

/// One alert rule's evaluation (`alerts::evaluate::evaluate_rule`).
pub(crate) fn alert_evaluate(rule_id: Uuid, project: ProjectId) -> Span {
    tracing::info_span!(
        "alerts.evaluate",
        { RULE_ID } = %rule_id,
        { PROJECT_ID } = %project,
        { OUTCOME } = Empty
    )
}

/// One alert delivery attempt (`alerts::deliver::deliver_one`). `kind` is the channel kind.
pub(crate) fn alert_deliver(
    delivery_id: Uuid,
    channel_id: Uuid,
    kind: &'static str,
    attempt: i64,
) -> Span {
    tracing::info_span!(
        "alerts.deliver",
        { DELIVERY_ID } = %delivery_id,
        { CHANNEL_ID } = %channel_id,
        { KIND } = kind,
        { ATTEMPT } = attempt,
        { OUTCOME } = Empty
    )
}

/// One step of one deletion row (`deletion::run_once`). `kind` is the row's kind.
pub(crate) fn deletion_apply(deletion_id: Uuid, kind: &'static str) -> Span {
    tracing::info_span!(
        "deletion.apply",
        { DELETION_ID } = %deletion_id,
        { KIND } = kind,
        { OUTCOME } = Empty
    )
}

/// One retention pass (`retention::retention_once`), hourly.
pub(crate) fn retention_pass() -> Span {
    tracing::info_span!(
        "retention.pass",
        { PROJECTS } = Empty,
        { FILES } = Empty,
        { FAILURES } = Empty,
        { OUTCOME } = Empty
    )
}

/// One issue candidate run (`issues::worker::run_candidate`).
pub(crate) fn issues_candidate(candidate_id: Uuid, source_id: Uuid) -> Span {
    tracing::info_span!(
        "issues.candidate",
        { CANDIDATE_ID } = %candidate_id,
        { SOURCE_ID } = %source_id,
        { OUTCOME } = Empty
    )
}

/// One online backup (`backup::run`).
pub(crate) fn backup() -> Span {
    tracing::info_span!(
        "backup",
        { MODE } = Empty,
        { OBJECTS } = Empty,
        { BYTES } = Empty,
        { OUTCOME } = Empty
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name, target-independent shape and field names of a span built with no subscriber.
    fn shape(span: &Span) -> (&'static str, Vec<&'static str>) {
        let meta = span.metadata().expect("a span keeps its metadata");
        (
            meta.name(),
            meta.fields().iter().map(|field| field.name()).collect(),
        )
    }

    /// Every span of the catalog, built with fixed arguments.
    fn all_spans() -> Vec<Span> {
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap();
        vec![
            http_request(&axum::http::Method::GET, "/x"),
            otlp_export(PROTOCOL_HTTP),
            query(&Span::none(), QUERY_MCP),
            eval_job(Uuid::nil(), project, Uuid::nil(), "rule"),
            alert_evaluate(Uuid::nil(), project),
            alert_deliver(Uuid::nil(), Uuid::nil(), "email", 1),
            deletion_apply(Uuid::nil(), "project"),
            retention_pass(),
            issues_candidate(Uuid::nil(), Uuid::nil()),
            backup(),
        ]
    }

    #[test]
    fn the_span_catalog_is_fixed() {
        let shapes: Vec<_> = all_spans().iter().map(shape).collect();
        assert_eq!(
            shapes,
            vec![
                (
                    "http.request",
                    vec![
                        "otel.kind",
                        "http.request.method",
                        "http.route",
                        "http.response.status_code",
                        "otel.status_code",
                    ],
                ),
                (
                    "otlp.export",
                    vec![
                        "otel.kind",
                        "protocol",
                        "project_id",
                        "spans",
                        "rejected",
                        "bytes",
                        "outcome",
                    ],
                ),
                (
                    "query",
                    vec!["kind", "project_id", "tool", "rows", "outcome"]
                ),
                (
                    "eval.job",
                    vec!["job_id", "project_id", "evaluator_id", "kind", "outcome"],
                ),
                ("alerts.evaluate", vec!["rule_id", "project_id", "outcome"]),
                (
                    "alerts.deliver",
                    vec!["delivery_id", "channel_id", "kind", "attempt", "outcome"],
                ),
                ("deletion.apply", vec!["deletion_id", "kind", "outcome"]),
                (
                    "retention.pass",
                    vec!["projects", "files", "failures", "outcome"],
                ),
                (
                    "issues.candidate",
                    vec!["candidate_id", "source_id", "outcome"],
                ),
                ("backup", vec!["mode", "objects", "bytes", "outcome"]),
            ]
        );
    }

    #[test]
    fn spans_have_this_module_as_target() {
        for span in all_spans() {
            let meta = span.metadata().expect("a span keeps its metadata");
            assert_eq!(meta.target(), "cetra_server::observability::spans");
        }
    }

    #[test]
    fn spans_are_info() {
        for span in all_spans() {
            let meta = span.metadata().expect("a span keeps its metadata");
            assert_eq!(*meta.level(), tracing::Level::INFO);
        }
    }

    #[test]
    fn count_saturates() {
        assert_eq!(count(3_usize), 3);
        assert_eq!(count(u64::MAX), i64::MAX);
        assert_eq!(count(7_u16), 7);
    }

    #[test]
    fn job_kind_maps_unknown_values_to_other() {
        assert_eq!(job_kind("rule"), "rule");
        assert_eq!(job_kind("manual"), "manual");
        assert_eq!(job_kind("backfill"), "backfill");
        assert_eq!(job_kind(""), "other");
        assert_eq!(job_kind("Rule"), "other");
    }
}
