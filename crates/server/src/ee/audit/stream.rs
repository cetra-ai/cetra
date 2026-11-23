// SPDX-License-Identifier: Elastic-2.0
//! The audit stream's delivery (track EA M4, D-EA-11, D-EA-19, D-EA-20).
//!
//! Every 10 s, each due destination's records after its `cursor_seq` go to its HTTPS endpoint
//! as one signed NDJSON batch (at most 500 records or 1 MiB). A 2xx moves the cursor to the
//! batch's last `seq`; any other outcome backs off ([`backoff_ns`]). After 24 h of failure the
//! owners (or the instance admins) are emailed once per run of failures (plan P-3).
//!
//! Logs carry the destination id and the URL's host only, never the URL, a header value or the
//! signing secret.

use cetra_eval::{SIGNATURE_HEADER, Secret, sign_payload};
use chrono::{DateTime, SecondsFormat, Utc};
use futures::StreamExt;
use reqwest::header::{HeaderName, HeaderValue};
use serde::Serialize;
use serde_json::Value;

use super::stream_store::{self, DestinationRow, StreamConfig};
use crate::alerts::deliver::{self, DELIVERY_HEADER, backoff_ns};
use crate::api::entitlements::Feature;
use crate::auth::OrgRole;
use crate::email::Email;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::members::org_members;

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const HOUR_NS: i64 = 3_600 * NANOS_PER_SECOND;
/// The failure run after which the owners (or instance admins) are emailed (D-EA-11).
const ALERT_AFTER_NS: i64 = 24 * HOUR_NS;
/// Records per batch (plan P-4).
const BATCH_RECORDS: i64 = 500;
/// Bytes per batch (plan P-4). A batch always holds at least one record.
const BATCH_BYTES: usize = 1024 * 1024;

/// Headers the optional header may not use (they are set per delivery).
const RESERVED_HEADERS: &[&str] = &[
    "content-type",
    "content-length",
    "host",
    "user-agent",
    SIGNATURE_HEADER,
    DELIVERY_HEADER,
];

const UNREADABLE_RECORD: &str = "an audit record could not be read";
const UNSIGNABLE: &str = "the signing secret is unusable";
const INVALID_HEADER: &str = "the optional header is not valid";
const RESERVED_HEADER: &str = "the optional header name is reserved";
const UNREADABLE_SETTINGS: &str = "the destination's settings cannot be read";
const UNREADABLE_CURSOR: &str = "the audit records could not be read";

/// What one pass did (plan E2 step 2).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Report {
    /// Destinations whose cursor moved.
    pub(crate) sent: u32,
    /// Destinations whose attempt failed.
    pub(crate) failed: u32,
    /// Due destinations whose scope lacks the `audit_log` feature (or the instance gate).
    pub(crate) skipped: u32,
}

/// What one destination's pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Sent,
    Failed,
    Idle,
}

/// One stream line: the record as the stream sends it (plan E2 step 1). Ids only, no names.
#[derive(Debug, Serialize)]
struct StreamLine {
    id: String,
    seq: i64,
    at: String,
    org_id: Option<String>,
    project_id: Option<String>,
    action: String,
    outcome: String,
    actor: Actor,
    target: Option<Target>,
    details: Value,
    ip: Option<String>,
    user_agent: Option<String>,
}

#[derive(Debug, Serialize)]
struct Actor {
    kind: String,
    id: Option<String>,
    via: Option<String>,
    credential_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct Target {
    kind: String,
    id: Option<String>,
}

/// One `audit_events` row, every column the stream sends.
#[derive(Debug, sqlx::FromRow)]
struct Record {
    id: String,
    seq: i64,
    at: i64,
    org_id: Option<String>,
    project_id: Option<String>,
    action: String,
    outcome: String,
    actor_kind: String,
    actor_id: Option<String>,
    via: Option<String>,
    credential_id: Option<String>,
    target_kind: Option<String>,
    target_id: Option<String>,
    details: String,
    ip: Option<String>,
    user_agent: Option<String>,
}

/// One batch ready to send: its body, the first and last `seq`, and its record count.
struct Batch {
    body: Vec<u8>,
    first: i64,
    last: i64,
    records: usize,
}

/// Delivers every due destination's records once (plan E2 step 2). Destinations are sent at
/// most [`deliver::CONCURRENCY`] at a time.
///
/// # Errors
///
/// [`MetaError`] only when the due destinations cannot be read. Per-destination failures are
/// recorded on the destination and logged.
pub(crate) async fn deliver_once(state: &AppState, now: i64) -> Result<Report, MetaError> {
    let due = stream_store::due(state.meta.reader(), now).await?;
    let mut report = Report::default();
    let mut work = Vec::new();
    for row in due {
        if entitled(state, &row, now) {
            work.push(row);
        } else {
            report.skipped = report.skipped.saturating_add(1);
        }
    }
    let outcomes = futures::stream::iter(work)
        .map(|row| async move { deliver_destination(state, &row, now).await })
        .buffer_unordered(deliver::CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    for outcome in outcomes {
        match outcome {
            Outcome::Sent => report.sent = report.sent.saturating_add(1),
            Outcome::Failed => report.failed = report.failed.saturating_add(1),
            Outcome::Idle => {}
        }
    }
    if report != Report::default() {
        tracing::debug!(
            sent = report.sent,
            failed = report.failed,
            skipped = report.skipped,
            "audit stream pass"
        );
    }
    Ok(report)
}

/// Whether the destination's scope may stream now: the org's `audit_log` feature, or the
/// instance gate for the instance destination (plan E2 step 2.2).
fn entitled(state: &AppState, row: &DestinationRow, now: i64) -> bool {
    match row.org_id {
        Some(org) => crate::entitlements::of(state, org).allows(Feature::AuditLog),
        None => super::instance_gate(state, now),
    }
}

/// One destination's pass. A write that fails is logged and counted as idle: the row stays as
/// it was, and the next pass reads it again.
async fn deliver_destination(state: &AppState, row: &DestinationRow, now: i64) -> Outcome {
    match send(state, row, now).await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::error!(
                destination_id = %row.id,
                url_host = %row.url_host,
                error = %error,
                "audit stream write failed"
            );
            Outcome::Idle
        }
    }
}

/// Reads, sends and records one destination's batch (plan E2 steps 2.3.1 to 2.3.8).
async fn send(state: &AppState, row: &DestinationRow, now: i64) -> Result<Outcome, MetaError> {
    let config = match stream_store::open(&state.secrets, &row.id, &row.config_enc) {
        Ok(config) => config,
        Err(_) => return fail(state, row, UNREADABLE_SETTINGS, now).await,
    };
    let records = match read_after(state, row).await {
        Ok(records) => records,
        Err(_) => return fail(state, row, UNREADABLE_CURSOR, now).await,
    };
    let batch = match build_batch(records) {
        Ok(Some(batch)) => batch,
        Ok(None) => return Ok(Outcome::Idle),
        Err(message) => return fail(state, row, message, now).await,
    };
    let headers = match headers_of(&config, row, &batch, now) {
        Ok(headers) => headers,
        Err(message) => return fail(state, row, message, now).await,
    };
    let (records, last) = (batch.records, batch.last);
    match deliver::post(state, &config.url, headers, batch.body).await {
        Ok(()) => {
            let (id, read) = (row.id, row.updated_at);
            let moved = state
                .meta
                .write(move |conn| {
                    Box::pin(
                        async move { stream_store::mark_sent(conn, id, read, last, now).await },
                    )
                })
                .await?;
            if !moved {
                // The destination changed during the send: its new state wins (plan P-6).
                return Ok(Outcome::Idle);
            }
            tracing::debug!(
                destination_id = %row.id,
                url_host = %row.url_host,
                records,
                last_seq = last,
                "audit stream batch sent"
            );
            Ok(Outcome::Sent)
        }
        Err(error) => fail(state, row, &error.message, now).await,
    }
}

/// Records a failed attempt with backoff, and emails the owners once when the run of failures
/// is 24 h old (plan E2 step 2.3.8).
async fn fail(
    state: &AppState,
    row: &DestinationRow,
    message: &str,
    now: i64,
) -> Result<Outcome, MetaError> {
    let attempts = row.attempts.saturating_add(1);
    let next_attempt_at = now.saturating_add(backoff_ns(attempts));
    let (id, read) = (row.id, row.updated_at);
    let stored = message.to_owned();
    let failing_since = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                stream_store::mark_failed(conn, id, read, attempts, next_attempt_at, &stored, now)
                    .await
            })
        })
        .await?;
    // `None`: the destination changed during the send, and its new state wins (plan P-6).
    let Some(since) = failing_since else {
        return Ok(Outcome::Idle);
    };
    if row.alerted_at.is_none() && since <= now.saturating_sub(ALERT_AFTER_NS) {
        let marked = state
            .meta
            .write(move |conn| {
                Box::pin(async move { stream_store::mark_alerted(conn, id, read, now).await })
            })
            .await?;
        if marked {
            alert_owners(state, row, since, now).await?;
        }
    }
    tracing::info!(
        destination_id = %row.id,
        url_host = %row.url_host,
        attempts,
        error = %message,
        "audit stream send failed"
    );
    Ok(Outcome::Failed)
}

/// The records after the destination's cursor, oldest first, at most [`BATCH_RECORDS`]. The
/// scope is the destination's (`org_id IS NULL` for the instance).
async fn read_after(state: &AppState, row: &DestinationRow) -> Result<Vec<Record>, MetaError> {
    let scope = row.org_id.map(|org| org.to_string());
    let records: Vec<Record> = sqlx::query_as(
        "SELECT id, seq, at, org_id, project_id, action, outcome, actor_kind, actor_id, via, \
         credential_id, target_kind, target_id, details, ip, user_agent \
         FROM audit_events WHERE org_id IS ? AND seq > ? ORDER BY seq ASC LIMIT ?",
    )
    .bind(scope)
    .bind(row.cursor_seq)
    .bind(BATCH_RECORDS)
    .fetch_all(state.meta.reader())
    .await?;
    Ok(records)
}

/// The batch of `records` (already oldest first): lines until the record or byte limit, always
/// at least one. `None` when there are no records.
fn build_batch(records: Vec<Record>) -> Result<Option<Batch>, &'static str> {
    let mut body = Vec::new();
    let mut first = None;
    let mut last = 0;
    let mut count = 0;
    for record in records {
        let seq = record.seq;
        let line = line_of(record)?;
        let mut encoded = serde_json::to_vec(&line).map_err(|_| UNREADABLE_RECORD)?;
        encoded.push(b'\n');
        if count > 0 && body.len().saturating_add(encoded.len()) > BATCH_BYTES {
            break;
        }
        if first.is_none() {
            first = Some(seq);
        }
        body.extend_from_slice(&encoded);
        last = seq;
        count += 1;
    }
    let Some(first) = first else {
        return Ok(None);
    };
    Ok(Some(Batch {
        body,
        first,
        last,
        records: count,
    }))
}

/// The stream line of one record (plan E2 step 1).
fn line_of(record: Record) -> Result<StreamLine, &'static str> {
    let Record {
        id,
        seq,
        at,
        org_id,
        project_id,
        action,
        outcome,
        actor_kind,
        actor_id,
        via,
        credential_id,
        target_kind,
        target_id,
        details,
        ip,
        user_agent,
    } = record;
    let details = serde_json::from_str(&details).map_err(|_| UNREADABLE_RECORD)?;
    let at = DateTime::<Utc>::from_timestamp_nanos(at).to_rfc3339_opts(SecondsFormat::Millis, true);
    Ok(StreamLine {
        id,
        seq,
        at,
        org_id,
        project_id,
        action,
        outcome,
        actor: Actor {
            kind: actor_kind,
            id: actor_id,
            via,
            credential_id,
        },
        target: target_kind.map(|kind| Target {
            kind,
            id: target_id,
        }),
        details,
        ip,
        user_agent,
    })
}

/// The request headers of one batch (plan E2 step 2.3.5). The signature is over the body with
/// the send time in seconds.
fn headers_of(
    config: &StreamConfig,
    row: &DestinationRow,
    batch: &Batch,
    now: i64,
) -> Result<Vec<(HeaderName, HeaderValue)>, &'static str> {
    let signature = sign_payload(
        &Secret::new(config.secret.clone()),
        now / NANOS_PER_SECOND,
        &batch.body,
    )
    .ok_or(UNSIGNABLE)?;
    let delivery = format!("{}:{}-{}", row.id, batch.first, batch.last);
    let mut headers = vec![
        (
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/x-ndjson"),
        ),
        (
            HeaderName::from_static(SIGNATURE_HEADER),
            header_value(&signature)?,
        ),
        (
            HeaderName::from_static(DELIVERY_HEADER),
            header_value(&delivery)?,
        ),
    ];
    if let Some((name, value)) = &config.header {
        if RESERVED_HEADERS
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(name))
        {
            return Err(RESERVED_HEADER);
        }
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| INVALID_HEADER)?;
        headers.push((name, header_value(value)?));
    }
    Ok(headers)
}

fn header_value(text: &str) -> Result<HeaderValue, &'static str> {
    HeaderValue::from_str(text).map_err(|_| INVALID_HEADER)
}

/// Emails the owners of the org (or the instance admins, plan P-3) that the destination has
/// failed for 24 h. With no mailer nothing is sent, and the episode is still marked alerted.
async fn alert_owners(
    state: &AppState,
    row: &DestinationRow,
    since: i64,
    now: i64,
) -> Result<(), MetaError> {
    let (scope_name, url, recipients) = match row.org_id {
        Some(org) => {
            let found: Option<(String, String)> =
                sqlx::query_as("SELECT name, slug FROM orgs WHERE id = ?")
                    .bind(org.to_string())
                    .fetch_optional(state.meta.reader())
                    .await?;
            let Some((name, slug)) = found else {
                return Ok(());
            };
            let owners: Vec<String> = org_members(state.meta.reader(), org)
                .await?
                .into_iter()
                .filter(|member| member.role == OrgRole::Owner && member.user.disabled_at.is_none())
                .map(|member| member.user.email)
                .collect();
            let url = state
                .config
                .public_url
                .join(&format!("/{slug}/settings/audit-log"));
            (name, url, owners)
        }
        None => {
            let admins: Vec<String> = sqlx::query_scalar(
                "SELECT email FROM users WHERE is_instance_admin = 1 AND disabled_at IS NULL \
                 ORDER BY email",
            )
            .fetch_all(state.meta.reader())
            .await?;
            let url = state.config.public_url.join("/admin/audit-log");
            ("this cetra instance".to_owned(), url, admins)
        }
    };
    let since_hours = u64::try_from(now.saturating_sub(since) / HOUR_NS).unwrap_or(0);
    if let Some(mailer) = &state.mailer {
        tracing::info!(
            destination_id = %row.id,
            url_host = %row.url_host,
            recipients = recipients.len(),
            "audit stream failing: emailing"
        );
        for to in recipients {
            mailer.send_in_background(
                to,
                Email::AuditStreamFailing {
                    scope_name: scope_name.clone(),
                    url_host: row.url_host.clone(),
                    since_hours,
                    url: url.clone(),
                },
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::OrgId;
    use cetra_eval::{Secret, sign_payload};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::{SIGNATURE_HEADER, deliver_once};
    use crate::alerts::deliver::DELIVERY_HEADER;
    use crate::config::ServerConfig;
    use crate::ee::audit::stream_store::{self, StreamConfig};
    use crate::meta::MetaError;
    use crate::test_support::hook_receiver::HookReceiver;
    use crate::test_support::{TestApp, capture_logs, license_key, org_token};

    const SECRET: &str = "signing-secret-value-0123";
    const SECOND: i64 = 1_000_000_000;
    const HOUR: i64 = 3_600 * SECOND;
    const DAY: i64 = 24 * HOUR;

    /// Gives the instance and every org the `audit_log` feature through a licence.
    fn entitled(config: &mut ServerConfig) {
        config.license_key = Some(crate::config::Secret::new(license_key(&["audit_log"])));
        config.security.allow_private_network_egress = true;
    }

    fn config(url: &str) -> StreamConfig {
        StreamConfig {
            url: url.to_owned(),
            secret: SECRET.to_owned(),
            header: None,
        }
    }

    /// Creates the destination of `org` (`None`: the instance) for `config`, with no records yet
    /// (its cursor starts at the table's highest seq). Returns its id.
    async fn destination(app: &TestApp, org: Option<OrgId>, config: StreamConfig) -> Uuid {
        let id = Uuid::now_v7();
        let sealed = stream_store::seal(&app.state.secrets, &id, &config).expect("seals");
        let host = url::Url::parse(&config.url)
            .expect("a test URL")
            .host_str()
            .unwrap_or_default()
            .to_owned();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    stream_store::insert(conn, id, org, &host, &sealed, now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .expect("the destination is written");
        id
    }

    /// Inserts `count` audit records at `at` for `org` (`None`: the instance log).
    async fn insert_records(app: &TestApp, count: i64, org: Option<OrgId>, at: i64) {
        let org = org.map(|org| org.to_string());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
                         INSERT INTO audit_events (id, at, org_id, action, outcome, actor_kind) \
                         SELECT lower(hex(randomblob(16))), ?, ?, 'instance.init_applied', \
                         'success', 'system' FROM n",
                    )
                    .bind(count)
                    .bind(at)
                    .bind(org.as_deref())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                    Ok(())
                })
            })
            .await
            .expect("the records are written");
    }

    async fn cursor(app: &TestApp, org: Option<OrgId>) -> i64 {
        stream_store::destination(app.state.meta.reader(), org)
            .await
            .expect("the destination reads")
            .expect("the destination exists")
            .cursor_seq
    }

    /// The NDJSON lines of a request body.
    fn lines(body: &[u8]) -> Vec<Value> {
        body.split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("a JSON line"))
            .collect()
    }

    /// Waits up to a second for `count` background sends.
    async fn wait_for_sent(app: &TestApp, count: usize) -> Vec<crate::email::SentMail> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while app.sent().len() < count && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        app.sent()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_records_are_sent_in_order_and_signed() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![200]).await;
        let id = destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 3, Some(seeded.org_id), 1000).await;
        let now = app.state.clock.now_ns();

        let report = deliver_once(&app.state, now).await.unwrap();

        assert_eq!(report.sent, 1);
        let received = receiver.received();
        assert_eq!(received.len(), 1);
        let request = &received[0];
        let body = lines(&request.body);
        let seqs: Vec<i64> = body
            .iter()
            .map(|line| line["seq"].as_i64().unwrap())
            .collect();
        assert_eq!(seqs.len(), 3);
        assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
        assert_eq!(body[0]["org_id"], json!(seeded.org_id.to_string()));
        assert_eq!(body[0]["at"], "1970-01-01T00:00:00.000Z");
        assert_eq!(
            request
                .headers
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/x-ndjson"
        );
        let signature = request
            .headers
            .get(SIGNATURE_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        let expected =
            sign_payload(&Secret::new(SECRET.to_owned()), now / SECOND, &request.body).unwrap();
        assert_eq!(signature, expected);
        let delivery = request
            .headers
            .get(DELIVERY_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(delivery, format!("{id}:{}-{}", seqs[0], seqs[2]));
        assert_eq!(cursor(&app, Some(seeded.org_id)).await, seqs[2]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_cursor_moves_only_on_success() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![500, 200]).await;
        destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 2, Some(seeded.org_id), 1000).await;
        let now = app.state.clock.now_ns();
        // The cursor starts at the table's highest `seq` at creation (the seeded login is seq 1).
        let start = cursor(&app, Some(seeded.org_id)).await;

        let report = deliver_once(&app.state, now).await.unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(cursor(&app, Some(seeded.org_id)).await, start);

        // The first backoff is 30 s: a pass before it sends nothing, one after it succeeds.
        deliver_once(&app.state, now + SECOND).await.unwrap();
        assert_eq!(receiver.received().len(), 1);
        let report = deliver_once(&app.state, now + 31 * SECOND).await.unwrap();
        assert_eq!(report.sent, 1);
        assert_eq!(receiver.received().len(), 2);
        assert!(cursor(&app, Some(seeded.org_id)).await > start);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failure_backs_off_and_retries() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![503, 503, 200]).await;
        destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 1, Some(seeded.org_id), 1000).await;
        let now = app.state.clock.now_ns();

        deliver_once(&app.state, now).await.unwrap();
        assert_eq!(receiver.received().len(), 1);
        // After one failure the next attempt is 30 s away.
        deliver_once(&app.state, now + 10 * SECOND).await.unwrap();
        assert_eq!(receiver.received().len(), 1);
        // After two failures it is 60 s further on.
        deliver_once(&app.state, now + 31 * SECOND).await.unwrap();
        assert_eq!(receiver.received().len(), 2);
        deliver_once(&app.state, now + 60 * SECOND).await.unwrap();
        assert_eq!(receiver.received().len(), 2);
        deliver_once(&app.state, now + 92 * SECOND).await.unwrap();
        assert_eq!(receiver.received().len(), 3);
        assert!(cursor(&app, Some(seeded.org_id)).await > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batches_stop_at_500_records() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![200]).await;
        destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 501, Some(seeded.org_id), 1000).await;
        let now = app.state.clock.now_ns();

        deliver_once(&app.state, now).await.unwrap();
        deliver_once(&app.state, now).await.unwrap();

        let received = receiver.received();
        assert_eq!(received.len(), 2);
        assert_eq!(lines(&received[0].body).len(), 500);
        assert_eq!(lines(&received[1].body).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lapsed_org_is_not_streamed() {
        // No licence: the org lacks `audit_log`.
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![200]).await;
        destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 2, Some(seeded.org_id), 1000).await;
        let now = app.state.clock.now_ns();
        let start = cursor(&app, Some(seeded.org_id)).await;

        let report = deliver_once(&app.state, now).await.unwrap();

        assert_eq!(report.skipped, 1);
        assert_eq!(report.sent, 0);
        assert!(receiver.received().is_empty());
        assert_eq!(cursor(&app, Some(seeded.org_id)).await, start);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_mode_sends_nothing() {
        let app = TestApp::with_config(|config| {
            entitled(config);
            config.restore_mode = true;
        })
        .await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![200]).await;
        destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 2, Some(seeded.org_id), 1000).await;
        let now = app.state.clock.now_ns();
        let start = cursor(&app, Some(seeded.org_id)).await;

        let report = deliver_once(&app.state, now).await.unwrap();

        // The egress client refuses in restore mode, so the attempt fails and nothing is sent.
        assert_eq!(report.failed, 1);
        assert!(receiver.received().is_empty());
        assert_eq!(cursor(&app, Some(seeded.org_id)).await, start);
    }

    /// Grants `org` `features` and `limits` the way a licence token does (`set_limits`).
    fn grant(app: &TestApp, org: OrgId, features: &[&str], limits: &Value) {
        let token = org_token(
            org,
            "2025-12-01T00:00:00Z",
            "2099-01-01T00:00:00Z",
            "2099-02-01T00:00:00Z",
            features,
            limits,
        );
        let defaults = app.state.entitlements.defaults().limits;
        let (org, grant) = crate::entitlements::tokens::grant_from_token(
            &token,
            app.state.config.public_url.origin(),
            &defaults,
        )
        .expect("the test token verifies");
        app.state.entitlements.put_grant(org, grant);
    }

    async fn max_seq(app: &TestApp) -> i64 {
        sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM audit_events")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purging_undelivered_records_flags_the_destination() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        grant(
            &app,
            seeded.org_id,
            &["audit_log"],
            &json!({"audit_retention_days": 30}),
        );
        destination(
            &app,
            Some(seeded.org_id),
            config("https://siem.example.com/ingest"),
        )
        .await;
        insert_records(&app, 2, Some(seeded.org_id), 0).await;
        let last = max_seq(&app).await;

        let removed = crate::audit::retention::purge(&app.state, 1000 * DAY)
            .await
            .unwrap();

        assert_eq!(removed, 2);
        let row = stream_store::destination(app.state.meta.reader(), Some(seeded.org_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.lost_before_seq, Some(last));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purging_delivered_records_does_not() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        grant(
            &app,
            seeded.org_id,
            &["audit_log"],
            &json!({"audit_retention_days": 30}),
        );
        let receiver = HookReceiver::start(vec![200]).await;
        destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 2, Some(seeded.org_id), 0).await;
        let now = app.state.clock.now_ns();
        assert_eq!(deliver_once(&app.state, now).await.unwrap().sent, 1);

        let removed = crate::audit::retention::purge(&app.state, 1000 * DAY)
            .await
            .unwrap();

        assert_eq!(removed, 2);
        let row = stream_store::destination(app.state.meta.reader(), Some(seeded.org_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.lost_before_seq, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owners_are_emailed_once_after_24_hours() {
        let app = TestApp::with_mailer_config(entitled).await;
        let seeded = app.seed().await;
        // The receiver answers 500 to every request.
        let receiver = HookReceiver::start(vec![500]).await;
        destination(&app, Some(seeded.org_id), config(&receiver.url("/ingest"))).await;
        insert_records(&app, 1, Some(seeded.org_id), 1000).await;
        let start = app.state.clock.now_ns();

        deliver_once(&app.state, start).await.unwrap();
        assert!(app.sent().is_empty());

        // 25 h later the run is older than 24 h: the owner is emailed once.
        deliver_once(&app.state, start + 25 * HOUR).await.unwrap();
        let sent = wait_for_sent(&app, 1).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "ada@example.com");
        assert_eq!(sent[0].subject, "Audit stream failing for Acme");

        // Still failing 50 h in, but the episode is already alerted.
        deliver_once(&app.state, start + 50 * HOUR).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(app.sent().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_instance_destination_streams_the_instance_log_only() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![200]).await;
        destination(&app, None, config(&receiver.url("/instance"))).await;
        insert_records(&app, 2, Some(seeded.org_id), 1000).await;
        insert_records(&app, 1, None, 1000).await;
        let now = app.state.clock.now_ns();

        deliver_once(&app.state, now).await.unwrap();

        let received = receiver.received();
        assert_eq!(received.len(), 1);
        let body = lines(&received[0].body);
        assert_eq!(body.len(), 1);
        assert!(body[0]["org_id"].is_null());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_url_or_secret_in_logs() {
        let app = TestApp::with_config(entitled).await;
        let seeded = app.seed().await;
        let receiver = HookReceiver::start(vec![500]).await;
        let mut settings = config(&receiver.url("/ingest/tok-7f3a9c"));
        settings.header = Some(("Authorization".to_owned(), "Splunk hec-9d2e".to_owned()));
        let id = destination(&app, Some(seeded.org_id), settings).await;
        insert_records(&app, 1, Some(seeded.org_id), 1000).await;
        let now = app.state.clock.now_ns();

        let (result, logs) = capture_logs(deliver_once(&app.state, now)).await;

        result.unwrap();
        assert!(logs.contains(&id.to_string()), "{logs}");
        assert!(!logs.contains("tok-7f3a9c"), "{logs}");
        assert!(!logs.contains("/ingest"), "{logs}");
        assert!(!logs.contains(SECRET), "{logs}");
        assert!(!logs.contains("hec-9d2e"), "{logs}");
    }
}
