use std::collections::BTreeMap;

use cetra_core::ids::{OrgId, ProjectId};
use cetra_core::names::PRODUCT;
use cetra_eval::{Secret, sign_payload};
use serde_json::Value;
use uuid::Uuid;

use super::*;
use crate::alerts::channel::seal_config;
use crate::alerts::evaluate::Step;
use crate::alerts::message::{MessageKind, sample_notice};
use crate::alerts::quota::DAILY_EMAIL_LIMIT;
use crate::api::account::SignupPolicy;
use crate::api::alerts::{
    AlertCondition, AlertEventKind, AlertMetric, AlertState, AlertWindow, Comparator,
};
use crate::auth::roles::OrgRole;
use crate::meta::alerts::{
    Evaluation, EventDetails, NewChannel, NewRule, Recorded, delete_channel, insert_channel,
    insert_rule, record_evaluation,
};
use crate::meta::issue_links::{Digest, DigestItem, DigestKind};
use crate::meta::issues::SourceKind;
use crate::test_support::TestApp;
use crate::test_support::capture_logs;
use crate::test_support::hook_receiver::{HookReceiver, Received};
use crate::test_support::issues::{put_source, source_row};

const SECRET: &str = "0123456789abcdef";
const SECOND: i64 = 1_000_000_000;
const DAY: i64 = 86_400 * SECOND;
/// The clock of every test: a fixed instant, so offsets are exact.
const NOW: i64 = 1_800_000_000_000_000_000;

fn rule_condition() -> AlertCondition {
    AlertCondition {
        metric: AlertMetric::ErrorRate,
        window: AlertWindow::M5,
        filter: None,
        comparator: Comparator::Gt,
        threshold: 0.05,
        min_count: Some(20),
        target: None,
        score_name: None,
        aggregation: None,
        issue_id: None,
        source_id: None,
    }
}

fn webhook(url: &str) -> ChannelConfig {
    ChannelConfig::Webhook {
        url: url.to_owned(),
        secret: SECRET.to_owned(),
        headers: BTreeMap::from([("X-Team".to_owned(), "core".to_owned())]),
    }
}

fn email(recipients: &[&str]) -> ChannelConfig {
    ChannelConfig::Email {
        recipients: recipients.iter().map(|text| (*text).to_owned()).collect(),
    }
}

/// An app that may send to 127.0.0.1, the address of the test receivers.
async fn private_app() -> TestApp {
    TestApp::with_config(|config| config.security.allow_private_network_egress = true).await
}

/// Seeds a project, a channel with `config` sealed, a rule using it, and a `Fired` evaluation at
/// `now`, so one delivery is `pending` with `next_attempt_at = now`.
/// Returns (delivery id, channel id, event id).
async fn seed_delivery(app: &TestApp, config: ChannelConfig, now: i64) -> (Uuid, Uuid, Uuid) {
    let project_id = app.seed().await.project.id;
    let channel_id = Uuid::now_v7();
    let sealed = seal_config(&app.state.secrets, &channel_id, &config).expect("seals");
    let kind = config.kind();
    let rule = app
        .state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let channel = insert_channel(
                    conn,
                    NewChannel {
                        id: channel_id,
                        project_id,
                        kind,
                        name: "Channel".to_owned(),
                        config_enc: sealed,
                        target_hint: "hint".to_owned(),
                    },
                    now,
                )
                .await?;
                insert_rule(
                    conn,
                    NewRule {
                        project_id,
                        name: "High errors".to_owned(),
                        condition: rule_condition(),
                        channel_ids: vec![channel.id],
                        enabled: true,
                        cooldown_s: 3600,
                        notify_while_firing: false,
                        created_by: None,
                    },
                    now,
                )
                .await
            })
        })
        .await
        .expect("rule");
    let rule_id = rule.id;
    let expected = rule.updated_at;
    let recorded = app
        .state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                record_evaluation(
                    conn,
                    rule_id,
                    expected,
                    Evaluation::Measured {
                        step: Step {
                            state: AlertState::Firing,
                            clear_streak: 0,
                            event: Some(AlertEventKind::Fired),
                            fired: true,
                            notified: true,
                        },
                        count: 48,
                        value: Some(0.125),
                        threshold: 0.05,
                        window_from: now - 300 * SECOND,
                        window_to: now,
                        details: EventDetails {
                            rule_name: "High errors".to_owned(),
                            metric: AlertMetric::ErrorRate,
                            comparator: Comparator::Gt,
                            window: AlertWindow::M5,
                            filter: None,
                        },
                    },
                    now,
                )
                .await
            })
        })
        .await
        .expect("evaluation");
    let Recorded::Event { event_id, .. } = recorded else {
        panic!("the evaluation recorded an event");
    };
    let delivery: String =
        sqlx::query_scalar("SELECT id FROM notification_deliveries WHERE event_id = ?")
            .bind(event_id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .expect("delivery");
    (delivery.parse().expect("delivery id"), channel_id, event_id)
}

/// The rule that an event belongs to.
async fn rule_of(app: &TestApp, event: Uuid) -> String {
    sqlx::query_scalar("SELECT rule_id FROM alert_events WHERE id = ?")
        .bind(event.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("rule of the event")
}

/// Inserts an event of `rule` at `created_at` by SQL.
async fn insert_event(app: &TestApp, rule: &str, created_at: i64) -> Uuid {
    let id = Uuid::now_v7();
    let rule = rule.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO alert_events (id, rule_id, kind, threshold, window_from, window_to, \
                     details, created_at) VALUES (?, ?, 'fired', 0.05, 0, 1, \
                     '{\"rule_name\":\"r\",\"metric\":\"error_rate\",\"comparator\":\"gt\",\"window\":\"5m\"}', ?)",
                )
                .bind(id.to_string())
                .bind(rule)
                .bind(created_at)
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
        .expect("event");
    id
}

/// Inserts a delivery of `event` to `channel` by SQL.
async fn insert_delivery(
    app: &TestApp,
    event: Uuid,
    channel: Uuid,
    status: &str,
    next_attempt_at: Option<i64>,
    updated_at: i64,
) -> Uuid {
    let id = Uuid::now_v7();
    let status = status.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO notification_deliveries (id, event_id, channel_id, status, attempts, \
                     next_attempt_at, last_error, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, 0, ?, NULL, ?, ?)",
                )
                .bind(id.to_string())
                .bind(event.to_string())
                .bind(channel.to_string())
                .bind(status)
                .bind(next_attempt_at)
                .bind(updated_at)
                .bind(updated_at)
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
        .expect("delivery");
    id
}

/// `(status, attempts, next_attempt_at, last_error)` of a delivery.
async fn row(app: &TestApp, id: Uuid) -> (String, i64, Option<i64>, Option<String>) {
    sqlx::query_as(
        "SELECT status, attempts, next_attempt_at, last_error FROM notification_deliveries WHERE id = ?",
    )
    .bind(id.to_string())
    .fetch_one(app.state.meta.reader())
    .await
    .expect("delivery row")
}

async fn exists(app: &TestApp, sql: &'static str, id: Uuid) -> bool {
    let count: i64 = sqlx::query_scalar(sql)
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .expect("count");
    count > 0
}

/// Ids of the due deliveries at `at`, oldest first, at most `limit`.
async fn due_ids(app: &TestApp, at: i64, limit: i64) -> Vec<Uuid> {
    due_deliveries(app.state.meta.reader(), at, limit)
        .await
        .expect("due")
        .deliveries
        .iter()
        .map(|due| due.id)
        .collect()
}

/// The project of the test app, which seeds one.
async fn project_of(app: &TestApp) -> ProjectId {
    let id: String = sqlx::query_scalar("SELECT id FROM projects")
        .fetch_one(app.state.meta.reader())
        .await
        .expect("project");
    id.parse().expect("project id")
}

/// Stores a channel named `name` in `project`, with `config` sealed. Returns its id.
async fn channel_for(
    app: &TestApp,
    project: ProjectId,
    name: &str,
    config: &ChannelConfig,
) -> Uuid {
    let channel_id = Uuid::now_v7();
    let sealed = seal_config(&app.state.secrets, &channel_id, config).expect("seals");
    let kind = config.kind();
    let name = name.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                insert_channel(
                    conn,
                    NewChannel {
                        id: channel_id,
                        project_id: project,
                        kind,
                        name,
                        config_enc: sealed,
                        target_hint: "hint".to_owned(),
                    },
                    NOW,
                )
                .await
            })
        })
        .await
        .expect("channel");
    channel_id
}

/// The rows an issue delivery test starts from: a project, a channel and a source "Errors".
struct IssueSeed {
    org: OrgId,
    project: ProjectId,
    channel: Uuid,
    source: Uuid,
}

/// Seeds the test app's project, a channel "Issues" of `config`, and a source.
async fn seed_issue(app: &TestApp, config: &ChannelConfig) -> IssueSeed {
    let seeded = app.seed().await;
    let channel = channel_for(app, seeded.project.id, "Issues", config).await;
    let source = source_row(seeded.project.id, "Errors", SourceKind::Error, true, NOW);
    let source_id = source.id;
    put_source(app, source).await;
    IssueSeed {
        org: seeded.org_id,
        project: seeded.project.id,
        channel,
        source: source_id,
    }
}

/// A digest of one item of `kind` titled `title`.
fn digest(kind: DigestKind, title: &str) -> Digest {
    Digest {
        items: vec![DigestItem {
            issue_id: Uuid::now_v7(),
            kind,
            title: title.to_owned(),
        }],
        more: 0,
    }
}

/// Inserts a notification of `source` with `digest` at `created_at` by SQL. Returns its id.
async fn insert_notification(
    app: &TestApp,
    source: Uuid,
    digest: &Digest,
    created_at: i64,
) -> Uuid {
    let id = Uuid::now_v7();
    let digest = serde_json::to_string(digest).expect("digest json");
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO issue_notifications (id, source_id, digest, created_at) \
                     VALUES (?, ?, ?, ?)",
                )
                .bind(id.to_string())
                .bind(source.to_string())
                .bind(digest)
                .bind(created_at)
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
        .expect("notification");
    id
}

/// Inserts a delivery of issue notification `notification` to `channel` by SQL.
async fn insert_issue_delivery(
    app: &TestApp,
    notification: Uuid,
    channel: Uuid,
    status: &str,
    next_attempt_at: Option<i64>,
    updated_at: i64,
) -> Uuid {
    let id = Uuid::now_v7();
    let status = status.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO notification_deliveries (id, event_id, issue_notification_id, \
                     channel_id, status, attempts, next_attempt_at, last_error, created_at, \
                     updated_at) VALUES (?, NULL, ?, ?, ?, 0, ?, NULL, ?, ?)",
                )
                .bind(id.to_string())
                .bind(notification.to_string())
                .bind(channel.to_string())
                .bind(status)
                .bind(next_attempt_at)
                .bind(updated_at)
                .bind(updated_at)
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
        .expect("issue delivery");
    id
}

/// Seeds an issue notification of "Boom" with one delivery to a channel of `config`, the
/// delivery `status` with `next_attempt_at` and `updated_at`. The notification is recorded at
/// `updated_at`. Returns the seed, the notification id and the delivery id.
async fn seed_issue_delivery(
    app: &TestApp,
    config: &ChannelConfig,
    status: &str,
    next_attempt_at: Option<i64>,
    updated_at: i64,
) -> (IssueSeed, Uuid, Uuid) {
    let seed = seed_issue(app, config).await;
    let notification = insert_notification(
        app,
        seed.source,
        &digest(DigestKind::Created, "Boom"),
        updated_at,
    )
    .await;
    let delivery = insert_issue_delivery(
        app,
        notification,
        seed.channel,
        status,
        next_attempt_at,
        updated_at,
    )
    .await;
    (seed, notification, delivery)
}

/// Seeds two rule deliveries and two issue deliveries, all `pending` to one channel, with
/// `next_attempt_at` NOW, NOW + 10 s, NOW + 20 s and NOW + 30 s in that order. Returns their
/// ids in that order.
async fn seed_interleaved(app: &TestApp) -> [Uuid; 4] {
    let (first, channel, event) =
        seed_delivery(app, webhook("https://hooks.example.com/x"), NOW).await;
    let rule = rule_of(app, event).await;
    let project = project_of(app).await;
    let source = source_row(project, "Errors", SourceKind::Error, true, NOW);
    let source_id = source.id;
    put_source(app, source).await;
    let issue =
        insert_notification(app, source_id, &digest(DigestKind::Created, "Boom"), NOW).await;
    let second =
        insert_issue_delivery(app, issue, channel, "pending", Some(NOW + 10 * SECOND), NOW).await;
    let third = insert_delivery(
        app,
        insert_event(app, &rule, NOW).await,
        channel,
        "pending",
        Some(NOW + 20 * SECOND),
        NOW,
    )
    .await;
    let issue =
        insert_notification(app, source_id, &digest(DigestKind::Regressed, "Again"), NOW).await;
    let fourth =
        insert_issue_delivery(app, issue, channel, "pending", Some(NOW + 30 * SECOND), NOW).await;
    [first, second, third, fourth]
}

/// One send of the sample notice.
async fn send(app: &TestApp, config: &ChannelConfig, now: i64) -> Result<(), SendError> {
    // The org only matters under open signup, which these tests do not set.
    send_to_channel(
        &app.state,
        OrgId::new_v7(),
        config,
        Outgoing::Alert(&sample_notice(MessageKind::Fired, now)),
        Uuid::now_v7(),
        now,
    )
    .await
}

fn header<'a>(request: &'a Received, name: &str) -> Option<&'a str> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
}

#[test]
fn backoff_doubles_and_caps() {
    assert_eq!(backoff_ns(1), 30 * SECOND);
    assert_eq!(backoff_ns(2), 60 * SECOND);
    assert_eq!(backoff_ns(7), 1_920 * SECOND);
    assert_eq!(backoff_ns(8), 3_600 * SECOND);
    assert_eq!(backoff_ns(100), 3_600 * SECOND);
}

#[tokio::test]
async fn webhook_is_signed_and_identified() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![200]).await;
    let delivery = Uuid::now_v7();
    let notice = sample_notice(MessageKind::Fired, NOW);
    send_to_channel(
        &app.state,
        OrgId::new_v7(),
        &webhook(&receiver.url("/hook")),
        Outgoing::Alert(&notice),
        delivery,
        NOW,
    )
    .await
    .expect("sent");

    let requests = receiver.received();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/hook");
    assert_eq!(header(request, "content-type"), Some("application/json"));
    assert_eq!(header(request, "x-team"), Some("core"));
    let delivery_text = delivery.to_string();
    assert_eq!(
        header(request, "x-cetra-delivery"),
        Some(delivery_text.as_str())
    );
    let expected = sign_payload(&Secret::new(SECRET.to_owned()), NOW / SECOND, &request.body);
    assert_eq!(header(request, "x-cetra-signature"), expected.as_deref());
}

#[tokio::test]
async fn slack_posts_block_kit() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![200]).await;
    let config = ChannelConfig::Slack {
        url: receiver.url("/services/T/B/abcd"),
    };
    send(&app, &config, NOW).await.expect("sent");
    let requests = receiver.received();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        header(&requests[0], "content-type"),
        Some("application/json")
    );
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json");
    assert!(body["blocks"].is_array());
}

#[tokio::test]
async fn statuses_are_classified() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![503, 404, 429]).await;
    let config = webhook(&receiver.url("/hook"));
    assert_eq!(
        send(&app, &config, NOW).await,
        Err(SendError {
            retryable: true,
            message: "the endpoint answered 503".to_owned(),
        })
    );
    assert_eq!(
        send(&app, &config, NOW).await,
        Err(SendError {
            retryable: false,
            message: "the endpoint answered 404".to_owned(),
        })
    );
    assert_eq!(
        send(&app, &config, NOW).await,
        Err(SendError {
            retryable: true,
            message: "the endpoint answered 429".to_owned(),
        })
    );
}

#[tokio::test]
async fn private_addresses_are_denied_by_default() {
    let app = TestApp::new().await;
    let receiver = HookReceiver::start(vec![200]).await;
    let error = send(&app, &webhook(&receiver.url("/hook")), NOW)
        .await
        .expect_err("denied");
    assert_eq!(
        error,
        SendError {
            retryable: false,
            message: "the address is not allowed (security.allow_private_network_egress)"
                .to_owned(),
        }
    );
    assert!(receiver.received().is_empty());
}

#[tokio::test]
async fn deliver_once_retries_then_fails() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![500]).await;
    let (id, _, _) = seed_delivery(&app, webhook(&receiver.url("/hook")), NOW).await;

    let mut at = NOW;
    for pass in 1..=MAX_ATTEMPTS {
        let report = deliver_once(&app.state, at).await.expect("pass");
        let (status, attempts, next, error) = row(&app, id).await;
        assert_eq!(attempts, pass);
        assert_eq!(error.as_deref(), Some("the endpoint answered 500"));
        if pass < MAX_ATTEMPTS {
            assert_eq!(
                report,
                DeliverReport {
                    sent: 0,
                    retried: 1,
                    failed: 0
                }
            );
            assert_eq!(status, "pending");
            assert_eq!(next, Some(at + backoff_ns(pass)));
            at = next.expect("a retry is scheduled");
            if pass == 1 {
                assert_eq!(
                    deliver_once(&app.state, NOW + 29 * SECOND)
                        .await
                        .expect("pass"),
                    DeliverReport::default()
                );
            }
        } else {
            assert_eq!(
                report,
                DeliverReport {
                    sent: 0,
                    retried: 0,
                    failed: 1
                }
            );
            assert_eq!(status, "failed");
            assert_eq!(next, None);
        }
    }
    assert_eq!(receiver.received().len(), 8);
    let text = app
        .state
        .telemetry
        .render(&app.state)
        .await
        .expect("metrics render");
    let failed = format!(
        "cetra_alert_deliveries_total{{channel=\"webhook\",outcome=\"failed\"}} {MAX_ATTEMPTS}"
    );
    assert!(text.lines().any(|line| line == failed), "{text}");
}

#[tokio::test]
async fn deliver_once_sends() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![200]).await;
    let (id, _, _) = seed_delivery(&app, webhook(&receiver.url("/hook")), NOW).await;
    let report = deliver_once(&app.state, NOW).await.expect("pass");
    assert_eq!(
        report,
        DeliverReport {
            sent: 1,
            retried: 0,
            failed: 0
        }
    );
    let (status, attempts, _, error) = row(&app, id).await;
    assert_eq!((status.as_str(), attempts, error), ("sent", 1, None));
    let text = app
        .state
        .telemetry
        .render(&app.state)
        .await
        .expect("metrics render");
    let sent = "cetra_alert_deliveries_total{channel=\"webhook\",outcome=\"sent\"} 1";
    assert!(text.lines().any(|line| line == sent), "{text}");
    assert_eq!(
        deliver_once(&app.state, NOW + SECOND).await.expect("pass"),
        DeliverReport::default()
    );
}

#[tokio::test]
async fn email_delivery_sends_to_each_recipient() {
    let app = TestApp::with_mailer().await;
    seed_delivery(&app, email(&["a@x.io", "b@x.io"]), NOW).await;
    let report = deliver_once(&app.state, NOW).await.expect("pass");
    assert_eq!(
        report,
        DeliverReport {
            sent: 1,
            retried: 0,
            failed: 0
        }
    );
    let sent = app.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].to, "a@x.io");
    assert_eq!(sent[1].to, "b@x.io");
    let subject = format!("[{PRODUCT}] Firing: High errors");
    assert!(sent.iter().all(|mail| mail.subject == subject));
    let text = app
        .state
        .telemetry
        .render(&app.state)
        .await
        .expect("metrics render");
    for line in [
        "cetra_emails_total{outcome=\"sent\"} 2",
        "cetra_alert_deliveries_total{channel=\"email\",outcome=\"sent\"} 1",
    ] {
        assert!(
            text.lines().any(|candidate| candidate == line),
            "{line}\n{text}"
        );
    }
}

#[tokio::test]
async fn daily_cap_refuses_the_send_before_it_starts() {
    let app = TestApp::with_mailer().await;
    let org = OrgId::new_v7();
    assert!(
        app.state
            .alert_email_count
            .reserve(org, DAILY_EMAIL_LIMIT, NOW)
    );
    let outcome = send_to_channel(
        &app.state,
        org,
        &email(&["a@x.io"]),
        Outgoing::Alert(&sample_notice(MessageKind::Fired, NOW)),
        Uuid::now_v7(),
        NOW,
    )
    .await;
    assert_eq!(outcome, Err(permanent("daily alert email limit reached")));
    assert!(app.sent().is_empty());
}

#[tokio::test]
async fn open_signup_skips_recipients_that_are_not_verified_members() {
    let app = TestApp::with_mailer_config(|config| config.auth.signup = SignupPolicy::Open).await;
    let seeded = app.seed().await;
    app.verify_email(seeded.user_id).await;
    let ben = app
        .add_member(&seeded, "ben@example.com", OrgRole::Member)
        .await;
    app.verify_email(ben.id).await;
    app.add_member(&seeded, "cy@example.com", OrgRole::Member)
        .await;
    let config = email(&[
        "ada@example.com",
        "ben@example.com",
        "cy@example.com",
        "eve@example.com",
    ]);
    let outcome = send_to_channel(
        &app.state,
        seeded.org_id,
        &config,
        Outgoing::Alert(&sample_notice(MessageKind::Fired, NOW)),
        Uuid::now_v7(),
        NOW,
    )
    .await;
    assert_eq!(
        outcome,
        Err(permanent(
            "2 recipient(s) skipped: not verified members of the organization"
        ))
    );
    let sent: Vec<String> = app.sent().into_iter().map(|mail| mail.to).collect();
    assert_eq!(sent, ["ada@example.com", "ben@example.com"]);
}

#[tokio::test]
async fn email_retry_after_a_failure() {
    let app = TestApp::with_mailer().await;
    app.state.mailer.as_ref().expect("stub mailer").fail_next(1);
    let (id, _, _) = seed_delivery(&app, email(&["a@x.io"]), NOW).await;
    let report = deliver_once(&app.state, NOW).await.expect("pass");
    assert_eq!(
        report,
        DeliverReport {
            sent: 0,
            retried: 1,
            failed: 0
        }
    );
    let (status, attempts, next, _) = row(&app, id).await;
    assert_eq!((status.as_str(), attempts), ("pending", 1));
    assert_eq!(next, Some(NOW + backoff_ns(1)));
    assert!(app.sent().is_empty());
}

#[tokio::test]
async fn email_without_smtp_fails_at_once() {
    let app = TestApp::new().await;
    let (id, _, _) = seed_delivery(&app, email(&["a@x.io"]), NOW).await;
    deliver_once(&app.state, NOW).await.expect("pass");
    let (status, attempts, next, error) = row(&app, id).await;
    assert_eq!((status.as_str(), attempts, next), ("failed", 1, None));
    assert_eq!(error.as_deref(), Some("email is not configured"));
}

#[tokio::test]
async fn prune_keeps_recent_history() {
    let app = TestApp::new().await;
    let (_, channel, event) =
        seed_delivery(&app, webhook("https://hooks.example.com/x"), NOW).await;
    let rule = rule_of(&app, event).await;

    let old = insert_event(&app, &rule, NOW - 91 * DAY).await;
    let old_delivery = insert_delivery(&app, old, channel, "sent", None, NOW - 91 * DAY).await;
    let recent = insert_event(&app, &rule, NOW - DAY).await;
    let finished_long_ago =
        insert_delivery(&app, recent, channel, "sent", None, NOW - 31 * DAY).await;
    let failed = insert_delivery(&app, recent, channel, "failed", None, NOW - DAY).await;
    let pending = insert_delivery(
        &app,
        recent,
        channel,
        "pending",
        Some(NOW - 40 * DAY),
        NOW - 40 * DAY,
    )
    .await;

    assert_eq!(prune_once(&app.state, NOW).await.expect("prune"), (1, 0, 1));
    assert!(!exists(&app, "SELECT COUNT(*) FROM alert_events WHERE id = ?", old).await);
    assert!(
        !exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            old_delivery
        )
        .await
    );
    assert!(
        !exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            finished_long_ago
        )
        .await
    );
    assert!(
        exists(
            &app,
            "SELECT COUNT(*) FROM alert_events WHERE id = ?",
            recent
        )
        .await
    );
    assert!(
        exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            failed
        )
        .await
    );
    assert!(
        exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            pending
        )
        .await
    );
}

#[tokio::test]
async fn finish_delivery_is_fenced() {
    let app = TestApp::new().await;
    let (id, _, _) = seed_delivery(&app, webhook("https://hooks.example.com/x"), NOW).await;
    let first = app
        .state
        .meta
        .write(move |conn| {
            Box::pin(async move { finish_delivery(conn, id, Attempt::Sent, NOW).await })
        })
        .await
        .expect("write");
    assert!(first);
    let second = app
        .state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                finish_delivery(
                    conn,
                    id,
                    Attempt::Failed {
                        error: "late".to_owned(),
                    },
                    NOW,
                )
                .await
            })
        })
        .await
        .expect("write");
    assert!(!second);
    let (status, attempts, _, _) = row(&app, id).await;
    assert_eq!((status.as_str(), attempts), ("sent", 1));
}

#[tokio::test]
async fn due_deliveries_order_and_filter() {
    let app = TestApp::new().await;
    let (seeded, channel, event) =
        seed_delivery(&app, webhook("https://hooks.example.com/x"), NOW).await;
    let rule = rule_of(&app, event).await;
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE notification_deliveries SET status = 'sent' WHERE id = ?")
                    .bind(seeded.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .expect("update");

    let late = insert_delivery(
        &app,
        insert_event(&app, &rule, NOW).await,
        channel,
        "pending",
        Some(NOW + 30 * SECOND),
        NOW,
    )
    .await;
    let early = insert_delivery(
        &app,
        insert_event(&app, &rule, NOW).await,
        channel,
        "pending",
        Some(NOW + 10 * SECOND),
        NOW,
    )
    .await;
    let middle = insert_delivery(
        &app,
        insert_event(&app, &rule, NOW).await,
        channel,
        "pending",
        Some(NOW + 20 * SECOND),
        NOW,
    )
    .await;
    let at = NOW + 25 * SECOND;
    let pool = app.state.meta.reader();
    let ids = |dues: Vec<DueDelivery>| dues.iter().map(|due| due.id).collect::<Vec<Uuid>>();
    assert_eq!(
        ids(due_deliveries(pool, at, 10).await.expect("due").deliveries),
        [early, middle]
    );
    assert_eq!(
        ids(due_deliveries(pool, at, 1).await.expect("due").deliveries),
        [early]
    );
    let later = due_deliveries(pool, NOW + 40 * SECOND, 10)
        .await
        .expect("due")
        .deliveries;
    assert_eq!(ids(later), [early, middle, late]);

    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE projects SET state = 'deleting'")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .expect("update");
    assert!(
        due_deliveries(pool, at, 10)
            .await
            .expect("due")
            .deliveries
            .is_empty()
    );
}

#[tokio::test]
async fn delivery_logs_hold_no_secrets() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![500]).await;
    seed_delivery(&app, webhook(&receiver.url("/hook")), NOW).await;

    let (_, logs) = capture_logs(deliver_once(&app.state, NOW)).await;
    assert!(logs.contains("alert delivery"), "{logs}");
    for secret in [SECRET, "\"core\"", "/hook"] {
        assert!(!logs.contains(secret), "the logs hold {secret}");
    }

    let (_, logs) = capture_logs(deliver_once(&app.state, NOW + 31 * SECOND)).await;
    assert!(logs.contains("alert delivery"), "{logs}");
    for secret in [SECRET, "\"core\"", "/hook"] {
        assert!(!logs.contains(secret), "the logs hold {secret}");
    }
}

#[tokio::test]
async fn permanent_failure_and_unreadable_config() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![404]).await;
    let (id, _, _) = seed_delivery(&app, webhook(&receiver.url("/hook")), NOW).await;
    deliver_once(&app.state, NOW).await.expect("pass");
    let (status, attempts, next, error) = row(&app, id).await;
    assert_eq!((status.as_str(), attempts, next), ("failed", 1, None));
    assert_eq!(error.as_deref(), Some("the endpoint answered 404"));

    // A config sealed for another channel id does not open.
    let other = TestApp::new().await;
    let (id, channel, _) = seed_delivery(&other, webhook("https://hooks.example.com/x"), NOW).await;
    let foreign = seal_config(
        &other.state.secrets,
        &Uuid::now_v7(),
        &webhook("https://hooks.example.com/x"),
    )
    .expect("seals");
    other
        .state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE notification_channels SET config_enc = ? WHERE id = ?")
                    .bind(foreign)
                    .bind(channel.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .expect("update");
    deliver_once(&other.state, NOW).await.expect("pass");
    let (status, attempts, next, error) = row(&other, id).await;
    assert_eq!((status.as_str(), attempts, next), ("failed", 1, None));
    assert_eq!(
        error.as_deref(),
        Some("the channel config could not be read")
    );
}

#[tokio::test]
async fn due_deliveries_returns_rule_and_issue_deliveries_ordered_together() {
    let app = TestApp::new().await;
    let [first, second, third, fourth] = seed_interleaved(&app).await;
    let dues = due_deliveries(app.state.meta.reader(), NOW + 40 * SECOND, 10)
        .await
        .expect("due")
        .deliveries;
    let ids: Vec<Uuid> = dues.iter().map(|due| due.id).collect();
    assert_eq!(ids, [first, second, third, fourth]);
    let issues: Vec<bool> = dues
        .iter()
        .map(|due| matches!(due.body, DueBody::Issues(_)))
        .collect();
    assert_eq!(issues, [false, true, false, true]);
}

#[tokio::test]
async fn due_deliveries_limit_applies_to_the_union() {
    let app = TestApp::new().await;
    let [first, second, third, _] = seed_interleaved(&app).await;
    let at = NOW + 40 * SECOND;
    assert_eq!(
        due_ids(&app, at, 3).await,
        [first, second, third],
        "the oldest three of both kinds"
    );
    assert_eq!(due_ids(&app, at, 1).await, [first]);
}

#[tokio::test]
async fn due_deliveries_skips_inactive_projects_for_issue_deliveries() {
    let app = TestApp::new().await;
    let (_, _, delivery) = seed_issue_delivery(
        &app,
        &webhook("https://hooks.example.com/x"),
        "pending",
        Some(NOW),
        NOW,
    )
    .await;
    assert_eq!(due_ids(&app, NOW, 10).await, [delivery]);
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE projects SET state = 'deleting'")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .expect("update");
    assert!(due_ids(&app, NOW, 10).await.is_empty());
}

#[tokio::test]
async fn due_deliveries_skips_inactive_orgs_for_issue_deliveries() {
    let app = TestApp::new().await;
    let (seed, _, delivery) = seed_issue_delivery(
        &app,
        &webhook("https://hooks.example.com/x"),
        "pending",
        Some(NOW),
        NOW,
    )
    .await;
    assert_eq!(due_ids(&app, NOW, 10).await, [delivery]);
    let org = seed.org;
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE orgs SET state = 'deleting' WHERE id = ?")
                    .bind(org.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .expect("update");
    assert!(due_ids(&app, NOW, 10).await.is_empty());
}

#[tokio::test]
async fn due_deliveries_skips_sent_and_future_issue_deliveries() {
    let app = TestApp::new().await;
    let seed = seed_issue(&app, &webhook("https://hooks.example.com/x")).await;
    let notification =
        insert_notification(&app, seed.source, &digest(DigestKind::Created, "Boom"), NOW).await;
    let sent = insert_issue_delivery(&app, notification, seed.channel, "sent", None, NOW).await;
    let pending =
        insert_issue_delivery(&app, notification, seed.channel, "pending", Some(NOW), NOW).await;
    let later = insert_issue_delivery(
        &app,
        notification,
        seed.channel,
        "pending",
        Some(NOW + 60 * SECOND),
        NOW,
    )
    .await;
    assert_eq!(due_ids(&app, NOW, 10).await, [pending]);
    let due = due_ids(&app, NOW + 60 * SECOND, 10).await;
    assert_eq!(due, [pending, later]);
    assert!(!due.contains(&sent));
}

#[tokio::test]
async fn an_undecodable_digest_is_failed_and_does_not_block_the_others() {
    let app = TestApp::new().await;
    let seed = seed_issue(&app, &webhook("https://hooks.example.com/x")).await;
    let good =
        insert_notification(&app, seed.source, &digest(DigestKind::Created, "Boom"), NOW).await;
    let bad = Uuid::now_v7();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO issue_notifications (id, source_id, digest, created_at) \
                     VALUES (?, (SELECT id FROM issue_sources LIMIT 1), 'not json', ?)",
                )
                .bind(bad.to_string())
                .bind(NOW)
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
        .expect("bad notification");
    let broken = insert_issue_delivery(&app, bad, seed.channel, "pending", Some(NOW), NOW).await;
    let fine = insert_issue_delivery(&app, good, seed.channel, "pending", Some(NOW + 1), NOW).await;

    let batch = due_deliveries(app.state.meta.reader(), NOW + SECOND, 10)
        .await
        .expect("due");
    assert_eq!(
        batch
            .deliveries
            .iter()
            .map(|due| due.id)
            .collect::<Vec<_>>(),
        [fine]
    );
    assert_eq!(batch.unreadable, [broken]);

    deliver_once(&app.state, NOW + SECOND).await.expect("pass");
    let status: String =
        sqlx::query_scalar("SELECT status FROM notification_deliveries WHERE id = ?")
            .bind(broken.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .expect("status");
    assert_eq!(status, "failed");
    let batch = due_deliveries(app.state.meta.reader(), NOW + SECOND, 10)
        .await
        .expect("due");
    assert!(batch.unreadable.is_empty());
}

#[tokio::test]
async fn an_issue_delivery_is_sent_to_a_webhook() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![200]).await;
    let (seed, notification, delivery) = seed_issue_delivery(
        &app,
        &webhook(&receiver.url("/hook")),
        "pending",
        Some(NOW),
        NOW,
    )
    .await;
    let report = deliver_once(&app.state, NOW).await.expect("pass");
    assert_eq!(
        report,
        DeliverReport {
            sent: 1,
            retried: 0,
            failed: 0
        }
    );

    let requests = receiver.received();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path, "/hook");
    let delivery_text = delivery.to_string();
    assert_eq!(
        header(request, "x-cetra-delivery"),
        Some(delivery_text.as_str())
    );
    let expected = sign_payload(&Secret::new(SECRET.to_owned()), NOW / SECOND, &request.body);
    assert_eq!(header(request, "x-cetra-signature"), expected.as_deref());
    let body: Value = serde_json::from_slice(&request.body).expect("json");
    assert_eq!(body["type"], "issues.detected");
    assert_eq!(body["notification_id"], notification.to_string());
    assert_eq!(body["source_id"], seed.source.to_string());
    assert_eq!(body["source_name"], "Errors");
    assert_eq!(body["items"][0]["kind"], "created");
    assert_eq!(body["items"][0]["title"], "Boom");
    assert_eq!(body["more"], 0);
    let issues_url = body["issues_url"].as_str().expect("issues url");
    assert!(
        issues_url.ends_with(&format!("/issues?source={}", seed.source)),
        "{issues_url}"
    );
    let (status, _, _, error) = row(&app, delivery).await;
    assert_eq!((status.as_str(), error), ("sent", None));
}

#[tokio::test]
async fn an_issue_delivery_is_sent_to_slack_and_email() {
    let app =
        TestApp::with_mailer_config(|config| config.security.allow_private_network_egress = true)
            .await;
    let receiver = HookReceiver::start(vec![200]).await;
    let seed = seed_issue(
        &app,
        &ChannelConfig::Slack {
            url: receiver.url("/services/T/B/abcd"),
        },
    )
    .await;
    let mail = channel_for(&app, seed.project, "Mail", &email(&["a@x.io"])).await;
    let notification =
        insert_notification(&app, seed.source, &digest(DigestKind::Created, "Boom"), NOW).await;
    insert_issue_delivery(&app, notification, seed.channel, "pending", Some(NOW), NOW).await;
    insert_issue_delivery(&app, notification, mail, "pending", Some(NOW), NOW).await;

    let report = deliver_once(&app.state, NOW).await.expect("pass");
    assert_eq!(
        report,
        DeliverReport {
            sent: 2,
            retried: 0,
            failed: 0
        }
    );
    let requests = receiver.received();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json");
    assert_eq!(body["text"], "1 new or regressed issue in Errors");
    let sent = app.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "a@x.io");
    assert_eq!(
        sent[0].subject,
        format!("[{PRODUCT}] 1 new or regressed issue: Errors")
    );
}

#[tokio::test]
async fn an_issue_delivery_retries_and_fails_like_a_rule_delivery() {
    let app = private_app().await;
    let receiver = HookReceiver::start(vec![500]).await;
    let (_, _, id) = seed_issue_delivery(
        &app,
        &webhook(&receiver.url("/hook")),
        "pending",
        Some(NOW),
        NOW,
    )
    .await;

    let mut at = NOW;
    for pass in 1..MAX_ATTEMPTS {
        let report = deliver_once(&app.state, at).await.expect("pass");
        assert_eq!(
            report,
            DeliverReport {
                sent: 0,
                retried: 1,
                failed: 0
            }
        );
        let (status, attempts, next, error) = row(&app, id).await;
        assert_eq!((status.as_str(), attempts), ("pending", pass));
        assert_eq!(error.as_deref(), Some("the endpoint answered 500"));
        assert_eq!(next, Some(at + backoff_ns(pass)));
        at = next.expect("a retry is scheduled");
    }
    let report = deliver_once(&app.state, at).await.expect("pass");
    assert_eq!(
        report,
        DeliverReport {
            sent: 0,
            retried: 0,
            failed: 1
        }
    );
    let (status, attempts, next, _) = row(&app, id).await;
    assert_eq!(
        (status.as_str(), attempts, next),
        ("failed", MAX_ATTEMPTS, None)
    );
    assert_eq!(receiver.received().len(), 8);
}

#[tokio::test]
async fn an_issue_email_counts_against_the_daily_quota() {
    let app = TestApp::with_mailer().await;
    let (seed, _, id) = seed_issue_delivery(
        &app,
        &email(&["a@x.io", "b@x.io"]),
        "pending",
        Some(NOW),
        NOW,
    )
    .await;
    // One message of the day's cap is left: the first recipient gets it, the second is refused.
    assert!(
        app.state
            .alert_email_count
            .reserve(seed.org, DAILY_EMAIL_LIMIT - 1, NOW)
    );
    let report = deliver_once(&app.state, NOW).await.expect("pass");
    assert_eq!(
        report,
        DeliverReport {
            sent: 0,
            retried: 0,
            failed: 1
        }
    );
    let sent: Vec<String> = app.sent().into_iter().map(|mail| mail.to).collect();
    assert_eq!(sent, ["a@x.io"]);
    let (status, attempts, next, error) = row(&app, id).await;
    assert_eq!((status.as_str(), attempts, next), ("failed", 1, None));
    assert_eq!(error.as_deref(), Some("daily alert email limit reached"));
}

#[tokio::test]
async fn prune_removes_old_issue_notifications_and_their_deliveries() {
    let app = TestApp::new().await;
    let (seed, old, old_delivery) = seed_issue_delivery(
        &app,
        &webhook("https://hooks.example.com/x"),
        "sent",
        None,
        NOW - 91 * DAY,
    )
    .await;
    let recent = insert_notification(
        &app,
        seed.source,
        &digest(DigestKind::Created, "Boom"),
        NOW - DAY,
    )
    .await;
    let recent_delivery =
        insert_issue_delivery(&app, recent, seed.channel, "sent", None, NOW - DAY).await;
    let finished_long_ago =
        insert_issue_delivery(&app, recent, seed.channel, "sent", None, NOW - 31 * DAY).await;

    // One notification and one old delivery; the notification's own delivery cascades and is
    // not counted.
    assert_eq!(prune_once(&app.state, NOW).await.expect("prune"), (0, 1, 1));
    assert!(
        !exists(
            &app,
            "SELECT COUNT(*) FROM issue_notifications WHERE id = ?",
            old
        )
        .await
    );
    assert!(
        !exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            old_delivery
        )
        .await
    );
    assert!(
        exists(
            &app,
            "SELECT COUNT(*) FROM issue_notifications WHERE id = ?",
            recent
        )
        .await
    );
    assert!(
        exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            recent_delivery
        )
        .await
    );
    assert!(
        !exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            finished_long_ago
        )
        .await
    );
}

#[tokio::test]
async fn deleting_a_channel_removes_its_issue_deliveries() {
    let app = TestApp::new().await;
    let (seed, _, delivery) = seed_issue_delivery(
        &app,
        &webhook("https://hooks.example.com/x"),
        "pending",
        Some(NOW),
        NOW,
    )
    .await;
    let project = seed.project;
    let channel = seed.channel;
    let removed = app
        .state
        .meta
        .write(move |conn| Box::pin(async move { delete_channel(conn, project, channel).await }))
        .await
        .expect("delete");
    assert!(removed);
    assert!(
        !exists(
            &app,
            "SELECT COUNT(*) FROM notification_deliveries WHERE id = ?",
            delivery
        )
        .await
    );
}

#[tokio::test]
async fn the_debug_of_a_due_issue_delivery_hides_titles() {
    let app = TestApp::new().await;
    let seed = seed_issue(&app, &webhook("https://hooks.example.com/x")).await;
    let notification = insert_notification(
        &app,
        seed.source,
        &digest(DigestKind::Created, "Secret title"),
        NOW,
    )
    .await;
    insert_issue_delivery(&app, notification, seed.channel, "pending", Some(NOW), NOW).await;
    let dues = due_deliveries(app.state.meta.reader(), NOW, 10)
        .await
        .expect("due")
        .deliveries;
    let text = format!("{:?}", dues.first().expect("one due delivery"));
    assert!(text.contains("DueDelivery"), "{text}");
    assert!(!text.contains("Secret"), "{text}");
}
