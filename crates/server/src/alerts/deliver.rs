//! Delivery of alert notifications (D-K-7): one send, the retry policy, the due pass, pruning.

use std::time::Duration;

use cetra_core::ids::OrgId;
use cetra_eval::{EgressError, EgressRequest, SIGNATURE_HEADER, Secret, sign_payload};
use futures::StreamExt;
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue};
use tracing::Instrument as _;
use uuid::Uuid;

use crate::alerts::channel::{ChannelConfig, open_config};
use crate::alerts::message::{self, IssuesNotice, MessageKind, Notice};
use crate::config::EmailRecipients;
use crate::email::{Email, MailError};
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::alerts::{
    Attempt, DueBody, DueDelivery, DueIssues, EventRow, due_deliveries, finish_delivery, prune,
};
use crate::meta::members::verified_member_emails;
use crate::observability::spans;

/// Deliveries sent at once (D-K-7).
pub(crate) const CONCURRENCY: usize = 4;
/// Deliveries taken per pass.
pub(crate) const BATCH: i64 = 50;
/// Time allowed for one send (D-K-7).
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Attempts before a delivery fails (D-K-7).
pub(crate) const MAX_ATTEMPTS: i64 = 8;
/// Response bodies read at most. Only the status matters, but the egress client fails a
/// larger body before it exposes the status, so the cap is far above any acknowledgement
/// page a receiver sends back.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Events and finished deliveries kept (D-K-10).
pub(crate) const EVENT_RETENTION_NS: i64 = 90 * 86_400_000_000_000;
pub(crate) const DELIVERY_RETENTION_NS: i64 = 30 * 86_400_000_000_000;
/// Delivery id header, lower case. A rename of the product changes it.
pub(crate) const DELIVERY_HEADER: &str = "x-cetra-delivery";

const INVALID_HEADER: &str = "invalid header";

/// Wait before attempt `attempts + 1`, after `attempts` failed: `min(30 s · 2^(attempts − 1), 1 h)`.
pub(crate) fn backoff_ns(attempts: i64) -> i64 {
    const BASE: i64 = 30_000_000_000;
    const CAP: i64 = 3_600_000_000_000;
    let shift = u32::try_from(attempts.saturating_sub(1).clamp(0, 20)).unwrap_or(20);
    BASE.saturating_mul(1_i64 << shift).min(CAP)
}

/// Why a send failed. The message never holds a URL, a header value, a secret or an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SendError {
    pub(crate) retryable: bool,
    pub(crate) message: String,
}

fn permanent(message: &str) -> SendError {
    SendError {
        retryable: false,
        message: message.to_owned(),
    }
}

fn retryable(message: &str) -> SendError {
    SendError {
        retryable: true,
        message: message.to_owned(),
    }
}

/// What one send carries: a rule event's message or an issue notification's.
#[derive(Clone, Copy)]
pub(crate) enum Outgoing<'a> {
    Alert(&'a Notice),
    Issues(&'a IssuesNotice),
}

/// Sends `outgoing` once to the channel `config` of org `org_id`. `delivery_id` goes into
/// `X-Cetra-Delivery`.
pub(crate) async fn send_to_channel(
    state: &AppState,
    org_id: OrgId,
    config: &ChannelConfig,
    outgoing: Outgoing<'_>,
    delivery_id: Uuid,
    now: i64,
) -> Result<(), SendError> {
    match config {
        ChannelConfig::Slack { url } => {
            // A `Value` always serializes.
            let value = match outgoing {
                Outgoing::Alert(notice) => message::slack_body(notice),
                Outgoing::Issues(notice) => message::issues_slack_body(notice),
            };
            let body = serde_json::to_vec(&value).unwrap_or_default();
            let headers = vec![(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            )];
            post(state, url, headers, body).await
        }
        ChannelConfig::Webhook {
            url,
            secret,
            headers,
        } => {
            let body = match outgoing {
                Outgoing::Alert(notice) => message::webhook_body(notice),
                Outgoing::Issues(notice) => message::issues_webhook_body(notice),
            };
            let signature = sign_payload(&Secret::new(secret.clone()), now / 1_000_000_000, &body)
                .ok_or_else(|| permanent("could not sign the payload"))?;
            let mut extra = vec![
                (
                    HeaderName::from_static("content-type"),
                    HeaderValue::from_static("application/json"),
                ),
                (
                    HeaderName::from_static(SIGNATURE_HEADER),
                    header_value(&signature)?,
                ),
                (
                    HeaderName::from_static(DELIVERY_HEADER),
                    header_value(&delivery_id.to_string())?,
                ),
            ];
            for (name, value) in headers {
                let name =
                    HeaderName::try_from(name.as_str()).map_err(|_| permanent(INVALID_HEADER))?;
                extra.push((name, header_value(value)?));
            }
            post(state, url, extra, body).await
        }
        ChannelConfig::Email { recipients } => {
            let email = match outgoing {
                Outgoing::Alert(notice) => message::email(notice),
                Outgoing::Issues(notice) => message::issues_email(notice),
            };
            send_email(state, org_id, recipients, &email, now).await
        }
    }
}

fn header_value(text: &str) -> Result<HeaderValue, SendError> {
    HeaderValue::from_str(text).map_err(|_| permanent(INVALID_HEADER))
}

/// One POST through the egress client. A 2xx is a success.
pub(crate) async fn post(
    state: &AppState,
    url: &str,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Vec<u8>,
) -> Result<(), SendError> {
    let url = Url::parse(url).map_err(|_| permanent("invalid URL"))?;
    let response = state
        .eval_runner
        .egress()
        .post(EgressRequest {
            url: &url,
            headers,
            body,
            timeout: SEND_TIMEOUT,
            max_response_bytes: MAX_RESPONSE_BYTES,
        })
        .await
        .map_err(|error| egress_error(&error))?;
    if response.status.is_success() {
        return Ok(());
    }
    let code = response.status.as_u16();
    Err(SendError {
        retryable: matches!(code, 408 | 429) || (500..=599).contains(&code),
        message: format!("the endpoint answered {code}"),
    })
}

pub(crate) fn egress_error(error: &EgressError) -> SendError {
    let (retry, message) = match error {
        EgressError::Timeout => (true, "the request timed out"),
        EgressError::Connect => (true, "could not connect"),
        EgressError::Request => (true, "the request failed"),
        EgressError::Denied { .. } => (
            false,
            "the address is not allowed (security.allow_private_network_egress)",
        ),
        EgressError::Scheme => (false, "unsupported URL scheme"),
        EgressError::TooLarge { .. } => (false, "the response was too large"),
        EgressError::RestoreMode => (false, "outbound calls are off: restore mode"),
    };
    SendError {
        retryable: retry,
        message: message.to_owned(),
    }
}

/// One email to each recipient, in order. SMTP connection, TLS and authentication failures
/// are retryable (an admin may fix the credentials within the retry span, D-K-7).
///
/// When `alerts.email_recipients` is `members`, only verified members of the org get mail (D-K-19,
/// D-K-21): the other recipients are skipped, and the delivery then fails with a count. Every
/// message counts against the org's daily cap, reserved just before its send.
async fn send_email(
    state: &AppState,
    org_id: OrgId,
    recipients: &[String],
    email: &Email,
    now: i64,
) -> Result<(), SendError> {
    let Some(mailer) = state.mailer.as_ref() else {
        return Err(permanent("email is not configured"));
    };
    let (sendable, skipped) = if state.config.email_recipients() == EmailRecipients::Members {
        let members = verified_member_emails(state.meta.reader(), org_id)
            .await
            .map_err(|_| retryable("could not read the org members"))?;
        let (members_ok, others): (Vec<&String>, Vec<&String>) = recipients
            .iter()
            .partition(|r| members.iter().any(|member| member.eq_ignore_ascii_case(r)));
        (members_ok, others.len())
    } else {
        (recipients.iter().collect(), 0)
    };
    if sendable.is_empty() && skipped > 0 {
        return Err(permanent(
            "no recipient is a verified member of the organization",
        ));
    }
    // Each message reserves one unit of the daily cap just before it is sent, so a retry counts
    // only the messages it attempts: recipients a failed send never reached are not counted twice
    // (D-K-19).
    let mut capped = false;
    let sent = tokio::time::timeout(SEND_TIMEOUT, async {
        for recipient in &sendable {
            if !state.alert_email_count.reserve(org_id, 1, now) {
                capped = true;
                break;
            }
            mailer.send_now(recipient, email).await?;
        }
        Ok::<(), MailError>(())
    })
    .await;
    match sent {
        Err(_elapsed) => Err(retryable("the request timed out")),
        Ok(Ok(())) if capped => Err(permanent("daily alert email limit reached")),
        Ok(Ok(())) if skipped > 0 => Err(permanent(&format!(
            "{skipped} recipient(s) skipped: not verified members of the organization"
        ))),
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(SendError {
            retryable: matches!(
                error,
                MailError::Connect(_) | MailError::Tls(_) | MailError::Auth(_)
            ),
            message: error.to_string(),
        }),
    }
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DeliverReport {
    pub(crate) sent: usize,
    pub(crate) retried: usize,
    pub(crate) failed: usize,
}

/// Sends every due delivery once, at most [`CONCURRENCY`] at a time.
///
/// # Errors
///
/// Returns [`MetaError`] only when the due deliveries cannot be read.
pub(crate) async fn deliver_once(state: &AppState, now: i64) -> Result<DeliverReport, MetaError> {
    let batch = due_deliveries(state.meta.reader(), now, BATCH).await?;
    // A row that cannot be decoded is failed, so it is not read again at every tick.
    for id in batch.unreadable {
        let failed = Attempt::Failed {
            error: "the delivery row could not be decoded".to_owned(),
        };
        state
            .meta
            .write(move |conn| {
                Box::pin(async move { finish_delivery(conn, id, failed, now).await })
            })
            .await?;
    }
    let outcomes = futures::stream::iter(batch.deliveries)
        .map(|due| async move { deliver_one(state, &due, now).await })
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut report = DeliverReport::default();
    for outcome in outcomes.into_iter().flatten() {
        match outcome {
            Outcome::Sent => report.sent += 1,
            Outcome::Retried => report.retried += 1,
            Outcome::Failed => report.failed += 1,
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Sent,
    Retried,
    Failed,
}

/// [`deliver_one_once`] in an `alerts.deliver` span with its outcome.
async fn deliver_one(state: &AppState, due: &DueDelivery, now: i64) -> Option<Outcome> {
    let span = spans::alert_deliver(
        due.id,
        due.channel_id,
        due.channel_kind.as_str(),
        due.attempts.saturating_add(1),
    );
    let outcome = deliver_one_once(state, due, now)
        .instrument(span.clone())
        .await;
    let label = match outcome {
        Some(Outcome::Sent) => "sent",
        Some(Outcome::Retried) => "retry",
        Some(Outcome::Failed) => "failed",
        None => "write_failed",
    };
    span.record(spans::OUTCOME, label);
    outcome
}

/// Makes one attempt and records it. Runs inside its span (see [`deliver_one`]). `None` when the
/// final write fails; the row stays pending and the next pass retries it.
async fn deliver_one_once(state: &AppState, due: &DueDelivery, now: i64) -> Option<Outcome> {
    let attempt = attempt_of(state, due, now).await;
    let (outcome, error) = match &attempt {
        Attempt::Sent => (Outcome::Sent, None),
        Attempt::Retry { error, .. } => (Outcome::Retried, Some(error.clone())),
        Attempt::Failed { error } => (Outcome::Failed, Some(error.clone())),
    };
    let id = due.id;
    let written = state
        .meta
        .write(move |conn| Box::pin(async move { finish_delivery(conn, id, attempt, now).await }))
        .await;
    if let Err(error) = written {
        tracing::error!(delivery_id = %due.id, error = %error, "alert delivery write failed");
        return None;
    }
    let label = match outcome {
        Outcome::Sent => "sent",
        Outcome::Retried => "retry",
        Outcome::Failed => "failed",
    };
    match &error {
        None => tracing::info!(
            delivery_id = %due.id,
            channel_id = %due.channel_id,
            kind = due.channel_kind.as_str(),
            attempt = due.attempts + 1,
            outcome = label,
            "alert delivery"
        ),
        Some(message) => tracing::info!(
            delivery_id = %due.id,
            channel_id = %due.channel_id,
            kind = due.channel_kind.as_str(),
            attempt = due.attempts + 1,
            outcome = label,
            error = %message,
            "alert delivery"
        ),
    }
    Some(outcome)
}

/// The result of one send, as the store records it.
async fn attempt_of(state: &AppState, due: &DueDelivery, now: i64) -> Attempt {
    let Ok(config) = open_config(&state.secrets, &due.channel_id, &due.config_enc) else {
        return Attempt::Failed {
            error: "the channel config could not be read".to_owned(),
        };
    };
    let result = match &due.body {
        DueBody::Rule(event) => {
            let notice = notice_of(state, due, event);
            Box::pin(send_to_channel(
                state,
                due.org_id,
                &config,
                Outgoing::Alert(&notice),
                due.id,
                now,
            ))
            .await
        }
        DueBody::Issues(issues) => {
            let notice = issues_notice_of(state, due, issues);
            Box::pin(send_to_channel(
                state,
                due.org_id,
                &config,
                Outgoing::Issues(&notice),
                due.id,
                now,
            ))
            .await
        }
    };
    state
        .telemetry
        .alert_delivered(due.channel_kind, result.is_ok());
    match result {
        Ok(()) => Attempt::Sent,
        Err(error) if error.retryable && due.attempts + 1 < MAX_ATTEMPTS => Attempt::Retry {
            error: error.message,
            next_attempt_at: now.saturating_add(backoff_ns(due.attempts + 1)),
        },
        Err(error) => Attempt::Failed {
            error: error.message,
        },
    }
}

/// The message of a due rule delivery, built from its event snapshot.
fn notice_of(state: &AppState, due: &DueDelivery, event: &EventRow) -> Notice {
    let kind = match event.kind {
        crate::api::alerts::AlertEventKind::Fired => MessageKind::Fired,
        crate::api::alerts::AlertEventKind::Resolved => MessageKind::Resolved,
        crate::api::alerts::AlertEventKind::NoData => MessageKind::NoData,
        crate::api::alerts::AlertEventKind::Reminder => MessageKind::Reminder,
    };
    let public_url = &state.config.public_url;
    Notice {
        kind,
        event_id: event.id,
        rule_id: event.rule_id,
        rule_name: event.details.rule_name.clone(),
        metric: event.details.metric,
        comparator: event.details.comparator,
        threshold: event.threshold,
        window: event.details.window,
        value: event.value,
        count: event.count,
        window_from: event.window_from,
        window_to: event.window_to,
        occurred_at: event.created_at,
        project_id: due.project_id,
        project_name: due.project_name.clone(),
        rule_url: message::rule_url(public_url, &due.org_slug, &due.project_slug, event.rule_id),
        traces_url: message::traces_url(
            public_url,
            &due.org_slug,
            &due.project_slug,
            event.window_from,
            event.window_to,
            event.details.filter.as_deref(),
        ),
    }
}

/// The message of a due issue delivery: the digest's items, each with its issue page URL.
fn issues_notice_of(state: &AppState, due: &DueDelivery, issues: &DueIssues) -> IssuesNotice {
    let public_url = &state.config.public_url;
    IssuesNotice {
        notification_id: issues.notification_id,
        source_id: issues.source_id,
        source_name: issues.source_name.clone(),
        project_id: due.project_id,
        project_name: due.project_name.clone(),
        items: issues.digest.items.clone(),
        more: issues.digest.more,
        occurred_at: issues.created_at,
        issues_url: message::issues_url(
            public_url,
            &due.org_slug,
            &due.project_slug,
            issues.source_id,
        ),
        issue_urls: issues
            .digest
            .items
            .iter()
            .map(|item| {
                message::issue_url(public_url, &due.org_slug, &due.project_slug, item.issue_id)
            })
            .collect(),
    }
}

/// Deletes events and issue notifications older than 90 days and finished deliveries older than
/// 30 days (D-K-10, D-IC-21). Returns `(events, notifications, deliveries)` deleted.
///
/// # Errors
///
/// Returns [`MetaError`] when the delete fails.
pub(crate) async fn prune_once(state: &AppState, now: i64) -> Result<(u64, u64, u64), MetaError> {
    let events_before = now.saturating_sub(EVENT_RETENTION_NS);
    let deliveries_before = now.saturating_sub(DELIVERY_RETENTION_NS);
    let (events, notifications, deliveries) = state
        .meta
        .write(move |conn| {
            Box::pin(async move { prune(conn, events_before, deliveries_before).await })
        })
        .await?;
    if events > 0 || notifications > 0 || deliveries > 0 {
        tracing::info!(events, notifications, deliveries, "alert history pruned");
    }
    Ok((events, notifications, deliveries))
}

#[cfg(test)]
mod tests;
