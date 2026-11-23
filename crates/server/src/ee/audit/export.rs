// SPDX-License-Identifier: Elastic-2.0
//! The audit exports (track EA M3, plan step 6): CSV and NDJSON. The export is recorded before
//! its first byte is read (`audit.export_started`), and it streams in pages of [`EXPORT_PAGE`]
//! records, newest first, up to [`MAX_EXPORT_ROWS`].

use std::fmt;
use std::io;

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::SqlitePool;

use super::read::{self, Filters, Scope};
use super::require_instance;
use crate::api::audit::{
    AuditEvent, AuditExportFormat, AuditExportParams, AuditOutcome, AuditRefStatus,
};
use crate::api::entitlements::Feature;
use crate::audit::catalog::AuditAction;
use crate::audit::{Ctx, Event, record_alone};
use crate::entitlements::require;
use crate::http::AppState;
use crate::http::access::OrgAccess;
use crate::http::error::{self, ApiError};

/// Records read per page of an export (reader pool).
const EXPORT_PAGE: u32 = 1_000;
/// Most records one export writes (plan P-7).
#[cfg(not(test))]
const MAX_EXPORT_ROWS: u64 = 1_000_000;
/// The cap in tests: small enough to reach with a few pages.
#[cfg(test)]
const MAX_EXPORT_ROWS: u64 = 1_500;

const CSV_HEADER: [&str; 20] = [
    "at",
    "action",
    "outcome",
    "actor_kind",
    "actor_id",
    "actor_name",
    "actor_email",
    "actor_status",
    "via",
    "credential_id",
    "target_kind",
    "target_id",
    "target_name",
    "target_email",
    "target_status",
    "project_id",
    "project_name",
    "ip",
    "user_agent",
    "details",
];

/// `GET /api/v1/orgs/{org}/audit-events/export`. Needs `audit_log` (plan step 6).
pub(crate) async fn export_org(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    ctx: Ctx,
    error::Query(params): error::Query<AuditExportParams>,
) -> Result<Response, ApiError> {
    let org = access.org.id;
    require(&state, org, Feature::AuditLog)?;
    export(&state, &ctx, Scope::Org(org), &params).await
}

/// `GET /api/v1/admin/audit-events/export`: the instance log (plan P-5).
pub(crate) async fn export_instance(
    State(state): State<AppState>,
    ctx: Ctx,
    error::Query(params): error::Query<AuditExportParams>,
) -> Result<Response, ApiError> {
    require_instance(&state)?;
    export(&state, &ctx, Scope::Instance, &params).await
}

/// Validates the filters (400 on bad input), records the export, then answers with the stream.
/// A failed record refuses the export, so a refused or invalid request writes nothing.
async fn export(
    state: &AppState,
    ctx: &Ctx,
    scope: Scope,
    params: &AuditExportParams,
) -> Result<Response, ApiError> {
    let filters = read::filters(
        params.from.as_ref(),
        params.to.as_ref(),
        params.action.as_deref(),
        params.actor_id.as_deref(),
        params.target_id.as_deref(),
        params.project_id,
        params.outcome,
    )?;
    let now = state.clock.now_ns();
    let mut event = Event::new(AuditAction::ExportStarted {
        format: format_name(params.format),
        filters: filter_names(params),
    });
    if let Scope::Org(org) = &scope {
        event = event.org(*org);
    }
    record_alone(&state.meta, ctx, event, now).await?;
    stream(state, scope, filters, params.format, now)
}

fn format_name(format: AuditExportFormat) -> &'static str {
    match format {
        AuditExportFormat::Csv => "csv",
        AuditExportFormat::Ndjson => "ndjson",
    }
}

/// The names of the filters that were set, never their values (`audit.export_started`).
fn filter_names(params: &AuditExportParams) -> Vec<&'static str> {
    [
        ("from", params.from.is_some()),
        ("to", params.to.is_some()),
        ("action", params.action.is_some()),
        ("actor_id", params.actor_id.is_some()),
        ("target_id", params.target_id.is_some()),
        ("project_id", params.project_id.is_some()),
        ("outcome", params.outcome.is_some()),
    ]
    .into_iter()
    .filter(|(_, set)| *set)
    .map(|(name, _)| name)
    .collect()
}

/// The streaming answer: headers, then the body pages.
fn stream(
    state: &AppState,
    scope: Scope,
    filters: Filters,
    format: AuditExportFormat,
    now: i64,
) -> Result<Response, ApiError> {
    let (content_type, extension) = match format {
        AuditExportFormat::Csv => ("text/csv; charset=utf-8", "csv"),
        AuditExportFormat::Ndjson => ("application/x-ndjson", "ndjson"),
    };
    let stamp = DateTime::<Utc>::from_timestamp_nanos(now).format("%Y%m%dT%H%M%SZ");
    let disposition = format!("attachment; filename=\"audit-events-{stamp}.{extension}\"");
    let disposition =
        HeaderValue::from_str(&disposition).map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    let cursor = ExportCursor {
        before_seq: None,
        written: 0,
        header_due: format == AuditExportFormat::Csv,
        done: false,
    };
    let pool = state.meta.reader().clone();
    let pages = futures::stream::try_unfold(cursor, move |cursor| {
        export_page(
            pool.clone(),
            scope.clone(),
            filters.clone(),
            format,
            cursor,
        )
    });
    Ok((StatusCode::OK, headers, Body::from_stream(pages)).into_response())
}

/// Where an export's stream is: the `seq` to read before, the records written so far, whether
/// the CSV header is still due, and whether the stream is over.
struct ExportCursor {
    before_seq: Option<i64>,
    written: u64,
    header_due: bool,
    done: bool,
}

/// One page of an export as its bytes, with the cursor of the next one. `None` ends the
/// stream. A failure logs its error (no payload) and ends the body with an I/O error.
async fn export_page(
    pool: SqlitePool,
    scope: Scope,
    filters: Filters,
    format: AuditExportFormat,
    cursor: ExportCursor,
) -> Result<Option<(Bytes, ExportCursor)>, io::Error> {
    if cursor.done {
        return Ok(None);
    }
    let fail = |error: &dyn fmt::Display| {
        tracing::error!(error = %error, "audit export failed");
        io::Error::other("audit export failed")
    };
    let mut rows = match read::page(&pool, &scope, &filters, cursor.before_seq, EXPORT_PAGE).await
    {
        Ok(rows) => rows,
        Err(error) => return Err(fail(&error)),
    };
    let last_page = rows.len() < EXPORT_PAGE as usize;
    let room = MAX_EXPORT_ROWS.saturating_sub(cursor.written);
    rows.truncate(usize::try_from(room).unwrap_or(usize::MAX));
    let written = cursor.written + rows.len() as u64;
    let events = match read::resolve(&pool, &scope, &rows).await {
        Ok(events) => events,
        Err(error) => return Err(fail(&error)),
    };
    let bytes = match encode(&events, format, cursor.header_due) {
        Ok(bytes) => bytes,
        Err(error) => return Err(fail(&error)),
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let next = ExportCursor {
        before_seq: rows.last().map(|row| row.seq).or(cursor.before_seq),
        written,
        header_due: false,
        done: last_page || written >= MAX_EXPORT_ROWS,
    };
    Ok(Some((Bytes::from(bytes), next)))
}

/// The bytes of one page: NDJSON lines, or CSV records preceded by the header when asked.
fn encode(events: &[AuditEvent], format: AuditExportFormat, with_header: bool) -> io::Result<Vec<u8>> {
    match format {
        AuditExportFormat::Csv => encode_csv(events, with_header),
        AuditExportFormat::Ndjson => {
            let mut bytes = Vec::new();
            for event in events {
                serde_json::to_writer(&mut bytes, event).map_err(io::Error::other)?;
                bytes.push(b'\n');
            }
            Ok(bytes)
        }
    }
}

fn encode_csv(events: &[AuditEvent], with_header: bool) -> io::Result<Vec<u8>> {
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(Vec::new());
    if with_header {
        writer.write_record(CSV_HEADER).map_err(io::Error::other)?;
    }
    for event in events {
        writer
            .write_record(csv_row(event).iter().map(String::as_bytes))
            .map_err(io::Error::other)?;
    }
    writer
        .into_inner()
        .map_err(csv::IntoInnerError::into_error)
}

/// The cells of one record, in [`CSV_HEADER`]'s order, each through [`safe_cell`].
fn csv_row(event: &AuditEvent) -> Vec<String> {
    let text = |value: Option<&str>| value.unwrap_or("").to_owned();
    let actor = &event.actor;
    let target = event.target.as_ref();
    let project = event.project.as_ref();
    let cells = vec![
        DateTime::<Utc>::from_timestamp_nanos(event.at)
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        event.action.clone(),
        outcome_name(event.outcome).to_owned(),
        actor.kind.clone(),
        text(actor.id.as_deref()),
        text(actor.name.as_deref()),
        text(actor.email.as_deref()),
        status_name(actor.status).to_owned(),
        text(actor.via.as_deref()),
        text(actor.credential_id.as_deref()),
        target.map_or_else(String::new, |target| target.kind.clone()),
        target.map_or_else(String::new, |target| target.id.clone()),
        text(target.and_then(|target| target.name.as_deref())),
        text(target.and_then(|target| target.email.as_deref())),
        target.map_or_else(String::new, |target| status_name(target.status).to_owned()),
        text(project.map(|project| project.id.as_str())),
        text(project.and_then(|project| project.name.as_deref())),
        text(event.ip.as_deref()),
        text(event.user_agent.as_deref()),
        event.details.to_string(),
    ];
    cells
        .into_iter()
        .map(|cell| safe_cell(&cell).into_owned())
        .collect()
}

/// A cell that a spreadsheet would read as a formula, or that starts a control sequence,
/// gets a leading `'` (plan P-6). User agents and names are attacker-chosen.
fn safe_cell(value: &str) -> std::borrow::Cow<'_, str> {
    if matches!(
        value.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    ) {
        std::borrow::Cow::Owned(format!("'{value}"))
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

fn outcome_name(outcome: AuditOutcome) -> &'static str {
    match outcome {
        AuditOutcome::Success => "success",
        AuditOutcome::Failure => "failure",
    }
}

fn status_name(status: AuditRefStatus) -> &'static str {
    match status {
        AuditRefStatus::Present => "present",
        AuditRefStatus::FormerMember => "former_member",
        AuditRefStatus::Deleted => "deleted",
        AuditRefStatus::Unresolved => "unresolved",
    }
}
