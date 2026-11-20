//! Error issues: the error spans of a batch become issues, routed and regressed (D-IC-2).

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, ListArray, RecordBatch, StringArray, StructArray, TimestampNanosecondArray,
};
use arrow::compute::cast;
use arrow::datatypes::{DataType, TimeUnit};
use cetra_core::ids::{ProjectId, SpanId, TraceId};
use cetra_core::schema::{event_field, spans};
use cetra_core::score::{ScoreTarget, ScoreValue};
use cetra_eval::issue::fingerprint::{ErrorFields, fingerprint_hex, normalize_message};
use cetra_storage::{ErrorSpansRequest, QueryContext, RunError, error_spans};
use serde_json::Value;
use uuid::Uuid;

use crate::api::convert::ConvertError;
use crate::api::scores::ScoreCreate;
use crate::entitlements::{Visibility, query_options};
use crate::eval::budget::DAY_NS;
use crate::eval::execute::{after_commit, borrowed_users};
use crate::eval::pending::Completed;
use crate::eval::trigger::{READ_TIMEOUT, SweepCtx};
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::issues::{self, EventKind, EventRow, IssueRow, IssueState};
use crate::meta::projects::{self, ProjectRow};
use crate::scores::{self, Writer, occurrence_id};

/// Longest issue title, in characters.
const TITLE_CHARS: usize = 200;
/// Most occurrence scores one write takes (the API's per-request cap).
const OCCURRENCE_CHUNK: usize = 1_000;
/// Stands in for a missing message in a title.
const NO_MESSAGE: &str = "(no message)";
/// The event name whose attributes carry the exception (OpenTelemetry semantic conventions).
const EXCEPTION_EVENT: &str = "exception";

/// Error issues for one batch (D-IC-2). Every error source of the project reads the batch's
/// error spans; a source whose filter does not parse is skipped. Its issues are written in one
/// transaction per source.
///
/// # Errors
///
/// `Err(())` when a read or a write fails; the batch is then retried, and nothing a retry would
/// repeat has been counted.
pub(crate) async fn sweep_batch(
    ctx: &SweepCtx<'_>,
    around: i64,
    batch: &[Completed],
) -> Result<(), ()> {
    let opened = ctx
        .state
        .storage
        .query(
            ctx.project,
            query_options(Visibility::window(ctx.state, ctx.org), Some(around)),
        )
        .await
        .ok()
        .and_then(|source| QueryContext::new(source, None).ok());
    let Some(query) = opened else {
        tracing::warn!(project_id = %ctx.project, "an error read could not start");
        return Err(());
    };
    let project = match projects::project_by_id(ctx.state.meta.reader(), ctx.project).await {
        Ok(Some(project)) => project,
        Ok(None) => {
            tracing::warn!(project_id = %ctx.project, "an error sweep found no project");
            return Err(());
        }
        Err(error) => {
            tracing::warn!(project_id = %ctx.project, error = %error, "the project could not be read");
            return Err(());
        }
    };
    let ids: Vec<String> = batch
        .iter()
        .map(|entry| entry.trace_id.to_string())
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut created = 0;
    let mut error_span_count = 0;
    let mut skipped = 0;
    let mut total = Outcome::default();
    for source in ctx.sources {
        let read = error_spans(
            &query,
            ErrorSpansRequest {
                filter: source.filter.as_deref(),
                trace_ids: &refs,
                timeout: READ_TIMEOUT,
            },
        )
        .await;
        #[cfg(test)]
        let read = crate::eval::trigger::faults::take_read().map_or(read, Err);
        let batches = match read {
            Ok(batches) => batches,
            // The filter does not parse: this source is skipped, the others go on. The warning is
            // logged at most once per source per hour.
            Err(RunError::Invalid(_) | RunError::InvalidRequest(_)) => {
                if ctx.state.eval.filter_warning_due(source.id, ctx.now) {
                    tracing::warn!(
                        project_id = %ctx.project,
                        source_id = %source.id,
                        "an issue source's filter could not be read; the source was skipped"
                    );
                }
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %ctx.project,
                    source_id = %source.id,
                    kind = error.kind(),
                    "an error read failed"
                );
                return Err(());
            }
        };
        let decoded = decode(&batches);
        skipped += decoded.skipped;
        error_span_count += decoded.spans.len();
        let groups = group_spans(decoded.spans, ctx.now);
        if groups.is_empty() {
            continue;
        }
        let (issues_created, found) =
            write_groups(ctx.state, ctx.project, source.id, groups, ctx.now)
                .await
                .map_err(|error| {
                    tracing::warn!(
                        project_id = %ctx.project,
                        source_id = %source.id,
                        error = %error,
                        "error issues could not be written"
                    );
                })?;
        created += issues_created;
        let outcome = write_occurrences(ctx, &project, source.id, found).await?;
        total.add(&outcome);
    }
    tracing::debug!(
        project_id = %ctx.project,
        sources = ctx.sources.len(),
        error_spans = error_span_count,
        skipped,
        issues_created = created,
        occurrences = total.written,
        occurrences_blocked = total.blocked,
        occurrences_too_old = total.too_old,
        occurrences_blocked_after_write = total.blocked_after_write,
        "error issues swept"
    );
    Ok(())
}

/// What became of the occurrences of a batch, for the debug log.
#[derive(Default)]
struct Outcome {
    /// Written as occurrence scores.
    written: usize,
    /// Dropped before the write: a deletion blocks their trace or user (D-M-6).
    blocked: usize,
    /// Dropped before the write: older than the project's retention.
    too_old: usize,
    /// Written, then blocked by a deletion that came meanwhile; a follow-up deletion was filed.
    blocked_after_write: usize,
}

impl Outcome {
    fn add(&mut self, other: &Self) {
        self.written += other.written;
        self.blocked += other.blocked;
        self.too_old += other.too_old;
        self.blocked_after_write += other.blocked_after_write;
    }
}

/// Writes the occurrences of one source as occurrence scores (D-IC-11, D-IC-18). An occurrence
/// whose trace or user a deletion blocks is dropped (D-M-6), and so is one older than the project's
/// retention: `scores::write` would refuse its whole chunk. The rest are written in chunks of
/// [`OCCURRENCE_CHUNK`]. After each chunk, the after-commit check files a follow-up deletion for a
/// trace that a deletion blocked meanwhile.
///
/// # Errors
///
/// `Err(())` when a write fails. The batch is then retried; the occurrence ids are stable, so a
/// rewrite replaces the same rows.
async fn write_occurrences(
    ctx: &SweepCtx<'_>,
    project: &ProjectRow,
    source: Uuid,
    occurrences: Vec<Occurrence>,
) -> Result<Outcome, ()> {
    let mut outcome = Outcome::default();
    let cutoff = project.retention_days.map(|days| {
        ctx.now
            .saturating_sub(i64::from(days).saturating_mul(DAY_NS))
    });
    let mut kept = Vec::with_capacity(occurrences.len());
    for occurrence in occurrences {
        let users = occurrence_users(&occurrence);
        let blocked = ctx
            .state
            .deletions
            .guard
            .blocking(
                ctx.project,
                occurrence.trace_id,
                &borrowed_users(&users),
                ctx.now,
            )
            .is_some();
        if blocked {
            outcome.blocked += 1;
        } else if cutoff.is_some_and(|cutoff| id_time(occurrence.at) < cutoff) {
            outcome.too_old += 1;
        } else {
            kept.push(occurrence);
        }
    }
    for chunk in kept.chunks(OCCURRENCE_CHUNK) {
        let items: Vec<ScoreCreate> = chunk
            .iter()
            .map(|occurrence| occurrence_score(project.id, occurrence))
            .collect();
        if let Err(error) = scores::write(
            ctx.state,
            project,
            Writer::Issue { source_id: source },
            items,
        )
        .await
        {
            tracing::warn!(
                project_id = %ctx.project,
                source_id = %source,
                count = chunk.len(),
                error = %error,
                "issue occurrences could not be written"
            );
            return Err(());
        }
        outcome.written += chunk.len();
        for occurrence in chunk {
            let users = occurrence_users(occurrence);
            if after_commit(
                ctx.state,
                ctx.project,
                occurrence.trace_id,
                &borrowed_users(&users),
                occurrence.trace_id,
            )
            .await
            {
                outcome.blocked_after_write += 1;
            }
        }
    }
    Ok(outcome)
}

/// The time a UUIDv7 of `at` carries: milliseconds, as `scores::write` reads it back (D-G-26).
fn id_time(at: i64) -> i64 {
    at.max(0) / 1_000_000 * 1_000_000
}

/// The occurrence's end user, as the deletion guard takes it. Error spans do not carry their
/// ingest time here, so the occurrence time stands in for it. Ingest comes after a span starts, so
/// this is the earlier of the two, and the guard blocks at least as often as with the real time.
fn occurrence_users(occurrence: &Occurrence) -> Vec<(String, i64)> {
    occurrence
        .user_id
        .iter()
        .map(|user| (user.clone(), occurrence.at))
        .collect()
}

/// The score of an occurrence (D-IC-18): named for its issue, stamped with its time through the
/// id, on its trace, with the trace's user and session and the span it came from.
fn occurrence_score(project: ProjectId, occurrence: &Occurrence) -> ScoreCreate {
    ScoreCreate {
        score_id: Some(occurrence_id(
            project,
            occurrence.trace_id,
            occurrence.issue_id,
            occurrence.at,
        )),
        target: ScoreTarget::Trace {
            trace_id: occurrence.trace_id,
        },
        session_id: occurrence.session_id.clone(),
        user_id: occurrence.user_id.clone(),
        name: format!("issue:{}", occurrence.issue_id),
        value: ScoreValue::Boolean(true),
        reason: occurrence.reason.clone(),
        source: None,
        metadata: BTreeMap::from([
            (
                "cetra.issue.source_id".to_owned(),
                occurrence.source_id.to_string(),
            ),
            (
                "cetra.issue.span_id".to_owned(),
                occurrence.span_id.to_string(),
            ),
        ]),
        experiment_id: None,
        dataset_item_id: None,
    }
}

/// One error span of a batch, decoded. Its text is payload: it is never logged.
struct ErrorSpan {
    trace_id: TraceId,
    span_id: SpanId,
    start_time: i64,
    service_name: Option<String>,
    name: String,
    exception_type: Option<String>,
    message: Option<String>,
    user_id: Option<String>,
    session_id: Option<String>,
}

/// The rows of a read that decoded, and how many did not.
struct Decoded {
    spans: Vec<ErrorSpan>,
    skipped: usize,
}

/// Decodes the rows of `batches`. A row that cannot be decoded is skipped; a batch whose columns
/// do not have the expected types skips all of its rows.
fn decode(batches: &[RecordBatch]) -> Decoded {
    let mut out = Decoded {
        spans: Vec::new(),
        skipped: 0,
    };
    for batch in batches {
        match decode_batch(batch, &mut out.spans) {
            Ok(skipped) => out.skipped += skipped,
            Err(error) => {
                tracing::warn!(error = %error, "error spans of a batch do not convert");
                out.skipped += batch.num_rows();
            }
        }
    }
    out
}

/// Appends the decodable rows of `batch` to `out`; returns how many rows were skipped.
fn decode_batch(batch: &RecordBatch, out: &mut Vec<ErrorSpan>) -> Result<usize, ConvertError> {
    let columns = Columns::of(batch)?;
    let mut skipped = 0;
    for row in 0..batch.num_rows() {
        match columns.row(row) {
            Ok(Some(span)) => out.push(span),
            Ok(None) | Err(_) => skipped += 1,
        }
    }
    Ok(skipped)
}

/// The columns [`decode_batch`] reads, extracted once per batch.
struct Columns {
    trace_id: StringArray,
    span_id: StringArray,
    start_time: TimestampNanosecondArray,
    service_name: StringArray,
    name: StringArray,
    status_message: StringArray,
    events: ListArray,
    user_id: StringArray,
    session_id: StringArray,
}

impl Columns {
    fn of(batch: &RecordBatch) -> Result<Self, ConvertError> {
        Ok(Self {
            trace_id: strings(batch, spans::TRACE_ID)?,
            span_id: strings(batch, spans::SPAN_ID)?,
            start_time: times(batch, spans::START_TIME)?,
            service_name: strings(batch, spans::SERVICE_NAME)?,
            name: strings(batch, spans::NAME)?,
            status_message: strings(batch, spans::STATUS_MESSAGE)?,
            events: lists(batch, spans::EVENTS)?,
            user_id: strings(batch, spans::USER_ID)?,
            session_id: strings(batch, spans::SESSION_ID)?,
        })
    }

    /// Row `row`. `None` when a required value is null or does not parse.
    fn row(&self, row: usize) -> Result<Option<ErrorSpan>, ConvertError> {
        let Some(trace_id) =
            text(&self.trace_id, row).and_then(|value| TraceId::from_str(&value).ok())
        else {
            return Ok(None);
        };
        let Some(span_id) =
            text(&self.span_id, row).and_then(|value| SpanId::from_str(&value).ok())
        else {
            return Ok(None);
        };
        if self.start_time.is_null(row) {
            return Ok(None);
        }
        let Some(name) = text(&self.name, row) else {
            return Ok(None);
        };
        let (exception_type, exception_message) = exception(&self.events, row)?;
        Ok(Some(ErrorSpan {
            trace_id,
            span_id,
            start_time: self.start_time.value(row),
            service_name: text(&self.service_name, row),
            name,
            exception_type,
            message: exception_message.or_else(|| text(&self.status_message, row)),
            user_id: text(&self.user_id, row),
            session_id: text(&self.session_id, row),
        }))
    }
}

/// The exception of a span: `exception.type` and `exception.message` of its last `exception`
/// event (D-IC-2). Missing, non-string or unparsable values are absent.
fn exception(
    events: &ListArray,
    row: usize,
) -> Result<(Option<String>, Option<String>), ConvertError> {
    if events.is_null(row) {
        return Ok((None, None));
    }
    let values = events.value(row);
    let items = values
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or(ConvertError::Type(spans::EVENTS))?;
    let names = child_strings(items, event_field::NAME)?;
    let attributes = child_strings(items, event_field::ATTRIBUTES)?;
    let found = (0..items.len())
        .rev()
        .find(|&index| text(&names, index).as_deref() == Some(EXCEPTION_EVENT));
    let Some(index) = found else {
        return Ok((None, None));
    };
    let parsed: Option<Value> =
        text(&attributes, index).and_then(|json| serde_json::from_str(&json).ok());
    let field = |key: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok((field("exception.type"), field("exception.message")))
}

/// A nullable string as text. Empty is absent.
fn text(array: &StringArray, row: usize) -> Option<String> {
    (!array.is_null(row))
        .then(|| array.value(row))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The batch column `name`, cast to `data_type` when its type differs.
fn column(
    batch: &RecordBatch,
    name: &'static str,
    data_type: &DataType,
) -> Result<ArrayRef, ConvertError> {
    let array = batch
        .column_by_name(name)
        .ok_or(ConvertError::Missing(name))?;
    if array.data_type() == data_type {
        return Ok(Arc::clone(array));
    }
    cast(array, data_type).map_err(|_| ConvertError::Type(name))
}

fn strings(batch: &RecordBatch, name: &'static str) -> Result<StringArray, ConvertError> {
    column(batch, name, &DataType::Utf8)?
        .as_any()
        .downcast_ref::<StringArray>()
        .cloned()
        .ok_or(ConvertError::Type(name))
}

fn times(
    batch: &RecordBatch,
    name: &'static str,
) -> Result<TimestampNanosecondArray, ConvertError> {
    let data_type = DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()));
    column(batch, name, &data_type)?
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .cloned()
        .ok_or(ConvertError::Type(name))
}

/// A list column as it is stored: its items are cast when read.
fn lists(batch: &RecordBatch, name: &'static str) -> Result<ListArray, ConvertError> {
    batch
        .column_by_name(name)
        .ok_or(ConvertError::Missing(name))?
        .as_any()
        .downcast_ref::<ListArray>()
        .cloned()
        .ok_or(ConvertError::Type(name))
}

/// A string child of a struct item, cast to Utf8 when its type differs.
fn child_strings(items: &StructArray, name: &'static str) -> Result<StringArray, ConvertError> {
    let child = items
        .column_by_name(name)
        .ok_or(ConvertError::Missing(name))?;
    let child: ArrayRef = if child.data_type() == &DataType::Utf8 {
        Arc::clone(child)
    } else {
        cast(child, &DataType::Utf8).map_err(|_| ConvertError::Type(name))?
    };
    child
        .as_any()
        .downcast_ref::<StringArray>()
        .cloned()
        .ok_or(ConvertError::Type(name))
}

/// One error group of a source: its title, its normalized message, and one hit per trace.
struct Group {
    title: String,
    reason: Option<String>,
    traces: BTreeMap<TraceId, Hit>,
}

/// The occurrence span of a group in one trace: its earliest span, and the trace's first user and
/// session among the group's spans.
struct Hit {
    span_id: SpanId,
    /// The span's start, clamped to the sweep's clock (D-IC-18).
    at: i64,
    user_id: Option<String>,
    session_id: Option<String>,
}

/// One occurrence of an issue in a trace, written as an occurrence score by [`write_occurrences`].
struct Occurrence {
    source_id: Uuid,
    issue_id: Uuid,
    trace_id: TraceId,
    span_id: SpanId,
    /// Trace start, clamped to the sweep's clock.
    at: i64,
    user_id: Option<String>,
    session_id: Option<String>,
    reason: Option<String>,
}

/// Groups the spans by fingerprint (D-IC-2, D-IC-17). Spans are visited in `(start_time,
/// span_id)` order, so each trace's hit is its earliest span.
fn group_spans(mut spans: Vec<ErrorSpan>, now: i64) -> BTreeMap<String, Group> {
    spans.sort_by_key(|span| (span.start_time, span.span_id));
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for span in spans {
        let normalized = span.message.as_deref().map(normalize_message);
        let fingerprint = fingerprint_hex(&ErrorFields {
            service_name: span.service_name.as_deref(),
            span_name: &span.name,
            exception_type: span.exception_type.as_deref(),
            message: normalized.as_deref(),
        });
        let group = groups.entry(fingerprint).or_insert_with(|| Group {
            title: title(
                span.exception_type.as_deref(),
                &span.name,
                normalized.as_deref(),
            ),
            reason: normalized.clone(),
            traces: BTreeMap::new(),
        });
        let hit = group.traces.entry(span.trace_id).or_insert_with(|| Hit {
            span_id: span.span_id,
            at: span.start_time.min(now),
            user_id: None,
            session_id: None,
        });
        if hit.user_id.is_none() {
            hit.user_id = span.user_id;
        }
        if hit.session_id.is_none() {
            hit.session_id = span.session_id;
        }
    }
    groups
}

/// `exception type: message`, or the span name when there is no exception type, cut to 200
/// characters (D-IC-2).
fn title(exception_type: Option<&str>, name: &str, message: Option<&str>) -> String {
    format!(
        "{}: {}",
        exception_type.unwrap_or(name),
        message.unwrap_or(NO_MESSAGE)
    )
    .chars()
    .take(TITLE_CHARS)
    .collect()
}

/// An event with no actor (the system).
fn event(issue: Uuid, kind: EventKind, from: Option<&str>, to: Option<&str>, at: i64) -> EventRow {
    EventRow {
        id: Uuid::now_v7(),
        issue_id: issue,
        kind,
        actor_user_id: None,
        from_value: from.map(str::to_owned),
        to_value: to.map(str::to_owned),
        at,
    }
}

/// Creates, routes and regresses the issues of one source's groups in one transaction (D-IC-2,
/// D-IC-9, D-IC-10). Returns the number of issues created and the occurrences of the groups.
async fn write_groups(
    state: &AppState,
    project: ProjectId,
    source: Uuid,
    groups: BTreeMap<String, Group>,
    now: i64,
) -> Result<(usize, Vec<Occurrence>), MetaError> {
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let mut created = 0;
                let mut occurrences = Vec::new();
                for (fingerprint, group) in groups {
                    let Group {
                        title,
                        reason,
                        traces,
                    } = group;
                    let first = traces.values().map(|hit| hit.at).min().unwrap_or(now);
                    let last = traces.values().map(|hit| hit.at).max().unwrap_or(now);
                    let existing = issues::issue_by_fingerprint(conn, source, &fingerprint).await?;
                    let mut issue = match existing {
                        Some(issue) => issue,
                        None => {
                            let issue = IssueRow {
                                id: Uuid::now_v7(),
                                project_id: project,
                                source_id: source,
                                fingerprint: Some(fingerprint),
                                title,
                                description: None,
                                state: IssueState::Open,
                                regressed: false,
                                merged_into: None,
                                first_seen: first,
                                last_seen: last,
                                resolved_at: None,
                                resolved_by: None,
                                created_at: now,
                                updated_at: now,
                            };
                            issues::insert_issue(conn, &issue).await?;
                            issues::insert_event(
                                conn,
                                &event(
                                    issue.id,
                                    EventKind::Created,
                                    None,
                                    Some(IssueState::Open.as_str()),
                                    now,
                                ),
                            )
                            .await?;
                            created += 1;
                            issue
                        }
                    };
                    // A merged issue's occurrences count on its target. The chain is one level deep
                    // (D-IC-9), so the target is taken as it is.
                    if let (IssueState::Merged, Some(target_id)) = (issue.state, issue.merged_into)
                    {
                        match issues::get_issue_tx(conn, project, target_id).await? {
                            Some(target) => issue = target,
                            None => tracing::warn!(
                                project_id = %project,
                                issue_id = %issue.id,
                                target_id = %target_id,
                                "a merged issue's target is missing; its occurrences stay on the merged issue"
                            ),
                        }
                    }
                    let mut changed = false;
                    // A resolved issue recurs when a trace of the group started after it was
                    // resolved (D-IC-10). Trace times decide, not completion times.
                    if issue.state == IssueState::Resolved
                        && issue.resolved_at.is_none_or(|resolved| last > resolved)
                    {
                        issue.state = IssueState::Open;
                        issue.regressed = true;
                        issue.resolved_at = None;
                        issue.resolved_by = None;
                        issues::insert_event(
                            conn,
                            &event(
                                issue.id,
                                EventKind::Regressed,
                                Some(IssueState::Resolved.as_str()),
                                Some(IssueState::Open.as_str()),
                                now,
                            ),
                        )
                        .await?;
                        changed = true;
                    }
                    let first_seen = issue.first_seen.min(first);
                    let last_seen = issue.last_seen.max(last);
                    if first_seen != issue.first_seen || last_seen != issue.last_seen {
                        issue.first_seen = first_seen;
                        issue.last_seen = last_seen;
                        changed = true;
                    }
                    if changed {
                        issue.updated_at = now;
                        issues::update_issue(conn, &issue).await?;
                    }
                    for (trace_id, hit) in traces {
                        occurrences.push(Occurrence {
                            source_id: source,
                            issue_id: issue.id,
                            trace_id,
                            span_id: hit.span_id,
                            at: hit.at,
                            user_id: hit.user_id,
                            session_id: hit.session_id,
                            reason: reason.clone(),
                        });
                    }
                }
                Ok((created, occurrences))
            })
        })
        .await
}
