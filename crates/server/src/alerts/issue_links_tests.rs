//! Issue alerts and notifications, end to end in process (IC M4.15, D-IC-20 to D-IC-25): the
//! `issue_occurrences` condition through the rule routes and the evaluation pass, and the digest of
//! new and regressed issues through the source routes, the notification pass and the delivery pass,
//! with the manual clock. The passes are called directly; no loop runs.

use axum::http::Method;
use cetra_core::ids::ProjectId;
use cetra_core::names::PRODUCT;
use cetra_eval::{SIGNATURE_HEADER, Secret, sign_payload};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::alerts::deliver::{DELIVERY_HEADER, DeliverReport, backoff_ns, deliver_once};
use crate::alerts::evaluate::{EvaluateReport, evaluate_once};
use crate::alerts::issue_notify::{NotifyReport, notify_issues};
use crate::api::alerts::AlertState;
use crate::meta::alerts::{RuleRow, rule_by_id};
use crate::meta::issue_links::{ISSUE_NOTIFY_SETTLE_NS, THROTTLE_NS};
use crate::meta::issues::{EventKind, IssueState, SourceKind};
use crate::test_support::hook_receiver::{HookReceiver, Received};
use crate::test_support::issues::{
    issue_row, occurrence, put_event, put_issue, put_source, source_row,
};
use crate::test_support::{Seeded, TestApp};

const SECOND: i64 = 1_000_000_000;
const MINUTE: i64 = 60 * SECOND;
const SECRET: &str = "0123456789abcdef";

/// A distinct 32-hex trace id per `index`.
fn trace_hex(index: usize) -> String {
    format!("{index:032x}")
}

fn uuid_of(value: &Value) -> Uuid {
    value.as_str().expect("an id").parse().expect("a uuid")
}

fn project_path(seeded: &Seeded, rest: &str) -> String {
    format!("/api/v1/projects/{}/{rest}", seeded.project.id)
}

/// One session request. Returns the status and the JSON body (`Null` when there is none).
async fn send(
    app: &TestApp,
    seeded: &Seeded,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let request = app.request(method, path).header("cookie", &seeded.cookie);
    let response = match body {
        Some(body) => request.json(&body).send().await,
        None => request.send().await,
    };
    let json = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    (response.status.as_u16(), json)
}

/// A webhook channel that posts to `url`, signed with [`SECRET`].
async fn create_webhook(app: &TestApp, seeded: &Seeded, url: &str) -> Uuid {
    let body = json!({"name": "Hook", "kind": "webhook", "url": url, "secret": SECRET});
    let (status, json) = send(
        app,
        seeded,
        Method::POST,
        &project_path(seeded, "channels"),
        Some(body),
    )
    .await;
    assert_eq!(status, 201, "{json}");
    uuid_of(&json["id"])
}

/// An email channel that mails `recipients`.
async fn create_email(app: &TestApp, seeded: &Seeded, recipients: &[&str]) -> Uuid {
    let body = json!({"name": "Mail", "kind": "email", "recipients": recipients});
    let (status, json) = send(
        app,
        seeded,
        Method::POST,
        &project_path(seeded, "channels"),
        Some(body),
    )
    .await;
    assert_eq!(status, 201, "{json}");
    uuid_of(&json["id"])
}

/// An error source created through the route. Enabled sources start their notification clock now.
async fn route_source(
    app: &TestApp,
    seeded: &Seeded,
    name: &str,
    enabled: bool,
    channels: &[Uuid],
) -> Uuid {
    let body = json!({
        "name": name,
        "kind": "error",
        "enabled": enabled,
        "channel_ids": channels,
    });
    let (status, json) = send(
        app,
        seeded,
        Method::POST,
        &project_path(seeded, "issue-sources"),
        Some(body),
    )
    .await;
    assert_eq!(status, 201, "{json}");
    uuid_of(&json["id"])
}

/// An error source stored directly, enabled, with no channels.
async fn stored_source(app: &TestApp, project: ProjectId) -> Uuid {
    let now = app.state.clock.now_ns();
    let row = source_row(project, "Errors", SourceKind::Error, true, now);
    let id = row.id;
    put_source(app, row).await;
    id
}

/// An open issue of `source` seen at `at`.
async fn open_issue(app: &TestApp, project: ProjectId, source: Uuid, title: &str, at: i64) -> Uuid {
    let id = Uuid::now_v7();
    put_issue(
        app,
        issue_row(project, source, id, title, IssueState::Open, at),
    )
    .await;
    id
}

/// An `issue_occurrences` condition over `window` 15m: `scope` is `issue_id` or `source_id`.
fn rule_condition(scope: &str, id: Uuid, comparator: &str, threshold: f64) -> Value {
    let mut condition = json!({
        "metric": "issue_occurrences",
        "window": "15m",
        "comparator": comparator,
        "threshold": threshold,
    });
    condition[scope] = json!(id);
    condition
}

/// An alert rule created through the route.
async fn create_rule(app: &TestApp, seeded: &Seeded, condition: Value, channels: &[Uuid]) -> Uuid {
    let body = json!({
        "name": "Issue rule",
        "condition": condition,
        "channel_ids": channels,
    });
    let (status, json) = send(
        app,
        seeded,
        Method::POST,
        &project_path(seeded, "alert-rules"),
        Some(body),
    )
    .await;
    assert_eq!(status, 201, "{json}");
    uuid_of(&json["id"])
}

async fn rule_row(app: &TestApp, project: ProjectId, id: Uuid) -> RuleRow {
    rule_by_id(app.state.meta.reader(), project, id)
        .await
        .expect("query")
        .expect("the rule")
}

/// Sets the clock to `at`.
fn at(app: &TestApp, at: i64) -> i64 {
    app.state.clock.set(at);
    at
}

/// Runs one SQL statement that binds one id.
async fn exec(app: &TestApp, sql: &'static str, id: Uuid) {
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query(sql)
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .expect("the statement");
}

async fn notification_count(pool: &SqlitePool, source: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM issue_notifications WHERE source_id = ?")
        .bind(source.to_string())
        .fetch_one(pool)
        .await
        .expect("count")
}

async fn delivery_count(pool: &SqlitePool, source: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM notification_deliveries d \
         JOIN issue_notifications n ON n.id = d.issue_notification_id \
         WHERE n.source_id = ?",
    )
    .bind(source.to_string())
    .fetch_one(pool)
    .await
    .expect("count")
}

/// The only delivery of `source`'s digest: its status, attempts and next attempt.
async fn the_delivery(pool: &SqlitePool, source: Uuid) -> (String, i64, Option<i64>) {
    sqlx::query_as(
        "SELECT d.status, d.attempts, d.next_attempt_at FROM notification_deliveries d \
         JOIN issue_notifications n ON n.id = d.issue_notification_id \
         WHERE n.source_id = ?",
    )
    .bind(source.to_string())
    .fetch_one(pool)
    .await
    .expect("the delivery")
}

/// The JSON bodies the receiver got, oldest first.
fn bodies(receiver: &HookReceiver) -> Vec<Value> {
    receiver
        .received()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("a json body"))
        .collect()
}

/// Checks the request's signature (D-K-7) and returns its delivery id.
fn assert_signed(request: &Received) -> Uuid {
    let signature = request
        .headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .expect("signature header")
        .to_owned();
    let timestamp: i64 = signature
        .split(',')
        .find_map(|part| part.strip_prefix("t="))
        .expect("timestamp")
        .parse()
        .expect("timestamp is a number");
    let expected = sign_payload(&Secret::new(SECRET.to_owned()), timestamp, &request.body);
    assert_eq!(Some(signature.as_str()), expected.as_deref());
    request
        .headers
        .get(DELIVERY_HEADER)
        .and_then(|value| value.to_str().ok())
        .expect("delivery header")
        .parse()
        .expect("a delivery id")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_issue_rule_fires_a_webhook_and_resolves() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;

    let now = app.state.clock.now_ns();
    let source = stored_source(&app, project).await;
    let issue = open_issue(&app, project, source, "Boom", now).await;
    for (index, minutes) in [1_i64, 2, 3].into_iter().enumerate() {
        occurrence(
            &app,
            &seeded,
            source,
            issue,
            &trace_hex(index + 1),
            now - minutes * MINUTE,
            None,
            None,
        )
        .await;
    }
    let rule = create_rule(
        &app,
        &seeded,
        rule_condition("issue_id", issue, "gt", 2.0),
        &[channel],
    )
    .await;

    assert_eq!(
        evaluate_once(&app.state, now).await.unwrap(),
        EvaluateReport {
            evaluated: 1,
            events: 1,
            ..EvaluateReport::default()
        }
    );
    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let fired = bodies(&receiver);
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0]["type"], "alert.fired");
    assert_eq!(fired[0]["rule"]["metric"], "issue_occurrences");
    assert_eq!(fired[0]["value"].as_f64(), Some(3.0));
    assert_eq!(fired[0]["count"].as_i64(), Some(3));

    // The occurrences leave the 15 minute window: the first clear reading keeps the rule firing.
    let later = at(&app, now + 16 * MINUTE);
    assert_eq!(
        evaluate_once(&app.state, later).await.unwrap(),
        EvaluateReport {
            evaluated: 1,
            ..EvaluateReport::default()
        }
    );
    let row = rule_row(&app, project, rule).await;
    assert_eq!(row.state, AlertState::Firing);
    assert_eq!(row.clear_streak, 1);

    // The second clear reading resolves it (D-K-4).
    let resolved = at(&app, later + MINUTE);
    assert_eq!(
        evaluate_once(&app.state, resolved).await.unwrap(),
        EvaluateReport {
            evaluated: 1,
            events: 1,
            ..EvaluateReport::default()
        }
    );
    assert_eq!(rule_row(&app, project, rule).await.state, AlertState::Ok);
    assert_eq!(
        deliver_once(&app.state, resolved).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let all = bodies(&receiver);
    assert_eq!(all.len(), 2);
    assert_eq!(all[1]["type"], "alert.resolved");
    assert_eq!(all[1]["rule"]["metric"], "issue_occurrences");
}

#[tokio::test]
async fn a_source_rule_counts_every_live_issue_and_sees_a_new_issue() {
    let app = TestApp::new().await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let now = app.state.clock.now_ns();
    let source = stored_source(&app, project).await;
    let first = open_issue(&app, project, source, "First", now).await;
    let second = open_issue(&app, project, source, "Second", now).await;
    occurrence(
        &app,
        &seeded,
        source,
        first,
        &trace_hex(1),
        now - MINUTE,
        None,
        None,
    )
    .await;
    occurrence(
        &app,
        &seeded,
        source,
        second,
        &trace_hex(2),
        now - 2 * MINUTE,
        None,
        None,
    )
    .await;
    let rule = create_rule(
        &app,
        &seeded,
        rule_condition("source_id", source, "gt", 0.0),
        &[],
    )
    .await;

    assert_eq!(
        evaluate_once(&app.state, now).await.unwrap(),
        EvaluateReport {
            evaluated: 1,
            events: 1,
            ..EvaluateReport::default()
        }
    );
    assert_eq!(rule_row(&app, project, rule).await.last_count, Some(2));

    // A third issue appears between two passes: the count follows the source.
    let later = at(&app, now + MINUTE);
    let third = open_issue(&app, project, source, "Third", later).await;
    occurrence(
        &app,
        &seeded,
        source,
        third,
        &trace_hex(3),
        later - 30 * SECOND,
        None,
        None,
    )
    .await;
    assert_eq!(
        evaluate_once(&app.state, later).await.unwrap(),
        EvaluateReport {
            evaluated: 1,
            ..EvaluateReport::default()
        }
    );
    assert_eq!(rule_row(&app, project, rule).await.last_count, Some(3));
}

#[tokio::test]
async fn an_issue_rule_survives_a_merge() {
    let app = TestApp::new().await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let now = app.state.clock.now_ns();
    let source = stored_source(&app, project).await;
    let target = open_issue(&app, project, source, "Target", now).await;
    let merged = open_issue(&app, project, source, "Merged", now).await;
    occurrence(
        &app,
        &seeded,
        source,
        merged,
        &trace_hex(1),
        now - MINUTE,
        None,
        None,
    )
    .await;
    occurrence(
        &app,
        &seeded,
        source,
        merged,
        &trace_hex(2),
        now - 2 * MINUTE,
        None,
        None,
    )
    .await;
    occurrence(
        &app,
        &seeded,
        source,
        target,
        &trace_hex(3),
        now - 3 * MINUTE,
        None,
        None,
    )
    .await;
    let rule = create_rule(
        &app,
        &seeded,
        rule_condition("issue_id", merged, "gt", 0.0),
        &[],
    )
    .await;
    evaluate_once(&app.state, now).await.unwrap();
    assert_eq!(rule_row(&app, project, rule).await.last_count, Some(2));

    let (status, json) = send(
        &app,
        &seeded,
        Method::POST,
        &project_path(&seeded, &format!("issues/{merged}/merge")),
        Some(json!({"into": target})),
    )
    .await;
    assert_eq!(status, 200, "{json}");

    // The merged issue's occurrences count on its target, with the target's own.
    let later = at(&app, now + MINUTE);
    assert_eq!(
        evaluate_once(&app.state, later).await.unwrap(),
        EvaluateReport {
            evaluated: 1,
            ..EvaluateReport::default()
        }
    );
    assert_eq!(rule_row(&app, project, rule).await.last_count, Some(3));
}

#[tokio::test]
async fn a_deleted_source_makes_the_rule_fail_with_a_fixed_reason() {
    let app = TestApp::new().await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let now = app.state.clock.now_ns();
    let source = stored_source(&app, project).await;
    open_issue(&app, project, source, "Boom", now).await;
    let rule = create_rule(
        &app,
        &seeded,
        rule_condition("source_id", source, "gt", 0.0),
        &[],
    )
    .await;

    let (status, json) = send(
        &app,
        &seeded,
        Method::DELETE,
        &project_path(&seeded, &format!("issue-sources/{source}")),
        None,
    )
    .await;
    assert_eq!(status, 204, "{json}");

    assert_eq!(
        evaluate_once(&app.state, now).await.unwrap(),
        EvaluateReport {
            evaluated: 1,
            failed: 1,
            ..EvaluateReport::default()
        }
    );
    let (status, json) = send(
        &app,
        &seeded,
        Method::GET,
        &project_path(&seeded, &format!("alert-rules/{rule}")),
        None,
    )
    .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["last_error"], "issue source not found");
}

#[tokio::test(flavor = "multi_thread")]
async fn new_issues_are_notified_in_one_digest() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let pool = app.state.meta.reader().clone();
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", true, &[channel]).await;
    let older = open_issue(&app, project, source, "Boom", start).await;
    let newer = open_issue(&app, project, source, "Bang", start).await;
    put_event(&app, older, EventKind::Created, start + MINUTE).await;
    put_event(&app, newer, EventKind::Created, start + 2 * MINUTE).await;

    let now = at(&app, start + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    assert_eq!(notification_count(&pool, source).await, 1);

    let requests = receiver.received();
    assert_eq!(requests.len(), 1);
    assert_signed(&requests[0]);
    let body: Value = serde_json::from_slice(&requests[0].body).expect("json body");
    assert_eq!(body["type"], "issues.detected");
    assert_eq!(body["source_id"], json!(source));
    assert_eq!(body["more"], json!(0));
    // Newest first.
    assert_eq!(
        body["items"],
        json!([
            {"issue_id": newer, "kind": "created", "title": "Bang"},
            {"issue_id": older, "kind": "created", "title": "Boom"},
        ])
    );
    assert!(
        body["issues_url"]
            .as_str()
            .is_some_and(|url| url.ends_with(&format!("/issues?source={source}")))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn regressions_are_notified() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", true, &[channel]).await;
    let issue = open_issue(&app, project, source, "Back", start).await;

    // Resolving writes a state change, which is never notified.
    let (status, json) = send(
        &app,
        &seeded,
        Method::PATCH,
        &project_path(&seeded, &format!("issues/{issue}")),
        Some(json!({"state": "resolved"})),
    )
    .await;
    assert_eq!(status, 200, "{json}");
    put_event(&app, issue, EventKind::Regressed, start + MINUTE).await;

    let now = at(&app, start + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let sent = bodies(&receiver);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["type"], "issues.detected");
    assert_eq!(
        sent[0]["items"],
        json!([{"issue_id": issue, "kind": "regressed", "title": "Back"}])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_inside_the_settle_margin_waits_for_the_next_pass() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let pool = app.state.meta.reader().clone();
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", true, &[channel]).await;
    let issue = open_issue(&app, project, source, "Boom", start).await;

    let now = at(&app, start + 11 * MINUTE);
    // Newer than the settle margin at `now`: its write may still be in flight.
    put_event(
        &app,
        issue,
        EventKind::Created,
        now - ISSUE_NOTIFY_SETTLE_NS / 2,
    )
    .await;
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 0,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport::default()
    );
    assert_eq!(notification_count(&pool, source).await, 0);

    let later = at(&app, now + MINUTE);
    assert_eq!(
        notify_issues(&app.state, later).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, later).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let sent = bodies(&receiver);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["items"][0]["issue_id"], json!(issue));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_throttle_holds_a_second_digest_for_ten_minutes() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", true, &[channel]).await;
    let first = open_issue(&app, project, source, "First", start).await;
    let second = open_issue(&app, project, source, "Second", start).await;

    // The first digest's cut-off is this event's time, ten minutes before the tick that sends it.
    let cutoff = start + 10 * MINUTE;
    put_event(&app, first, EventKind::Created, cutoff).await;
    let now = at(&app, start + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );

    // One minute after the cut-off: held until the throttle has passed.
    put_event(&app, second, EventKind::Created, cutoff + MINUTE).await;
    let early = at(&app, cutoff + THROTTLE_NS - MINUTE);
    assert_eq!(
        notify_issues(&app.state, early).await.unwrap(),
        NotifyReport::default()
    );
    assert_eq!(
        deliver_once(&app.state, early).await.unwrap(),
        DeliverReport::default()
    );
    assert_eq!(bodies(&receiver).len(), 1);

    let due = at(&app, cutoff + THROTTLE_NS);
    assert_eq!(
        notify_issues(&app.state, due).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, due).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let sent = bodies(&receiver);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[1]["items"],
        json!([{"issue_id": second, "kind": "created", "title": "Second"}])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn enabling_a_source_does_not_send_its_backlog() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let pool = app.state.meta.reader().clone();
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", false, &[channel]).await;
    let backlog = open_issue(&app, project, source, "Backlog", start).await;
    put_event(&app, backlog, EventKind::Created, start).await;

    // Enabling the source sets its cut-off to now (D-IC-21).
    let enabled_at = at(&app, start + MINUTE);
    let (status, json) = send(
        &app,
        &seeded,
        Method::PATCH,
        &project_path(&seeded, &format!("issue-sources/{source}")),
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, 200, "{json}");

    let quiet = at(&app, enabled_at + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, quiet).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 0,
            failed: 0,
        }
    );
    assert_eq!(notification_count(&pool, source).await, 0);

    let fresh = open_issue(&app, project, source, "Fresh", quiet - 2 * MINUTE).await;
    put_event(&app, fresh, EventKind::Created, quiet - 2 * MINUTE).await;
    assert_eq!(
        notify_issues(&app.state, quiet).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, quiet).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let sent = bodies(&receiver);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["items"],
        json!([{"issue_id": fresh, "kind": "created", "title": "Fresh"}])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_source_does_not_notify_and_a_paused_one_does() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let pool = app.state.meta.reader().clone();
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let disabled = route_source(&app, &seeded, "Disabled", true, &[channel]).await;
    let paused = route_source(&app, &seeded, "Paused", true, &[channel]).await;

    let (status, json) = send(
        &app,
        &seeded,
        Method::PATCH,
        &project_path(&seeded, &format!("issue-sources/{disabled}")),
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, 200, "{json}");
    exec(
        &app,
        "UPDATE issue_sources SET paused_reason = 'budget' WHERE id = ?",
        paused,
    )
    .await;

    for source in [disabled, paused] {
        let issue = open_issue(&app, project, source, "Boom", start).await;
        put_event(&app, issue, EventKind::Created, start + MINUTE).await;
    }
    let now = at(&app, start + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(notification_count(&pool, disabled).await, 0);
    assert_eq!(notification_count(&pool, paused).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_receiver_is_retried_with_backoff_and_not_renotified() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let pool = app.state.meta.reader().clone();
    let receiver = HookReceiver::start(vec![500, 200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", true, &[channel]).await;
    let issue = open_issue(&app, project, source, "Boom", start).await;
    put_event(&app, issue, EventKind::Created, start + MINUTE).await;

    let now = at(&app, start + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport {
            retried: 1,
            ..DeliverReport::default()
        }
    );
    let retry_at = now + backoff_ns(1);
    assert_eq!(
        the_delivery(&pool, source).await,
        ("pending".to_owned(), 1, Some(retry_at))
    );

    // Not before the backoff has passed.
    at(&app, retry_at - 1);
    assert_eq!(
        deliver_once(&app.state, retry_at - 1).await.unwrap(),
        DeliverReport::default()
    );
    assert_eq!(receiver.received().len(), 1);

    at(&app, retry_at);
    assert_eq!(
        deliver_once(&app.state, retry_at).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let requests = receiver.received();
    assert_eq!(requests.len(), 2);
    assert_eq!(assert_signed(&requests[0]), assert_signed(&requests[1]));
    assert_eq!(
        the_delivery(&pool, source).await,
        ("sent".to_owned(), 2, None)
    );

    // One digest, however many attempts it took.
    assert_eq!(notification_count(&pool, source).await, 1);
    assert_eq!(delivery_count(&pool, source).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_channel_stops_notifications() {
    let app =
        TestApp::with_config(|config| config.security.allow_private_network_egress = true).await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let pool = app.state.meta.reader().clone();
    let receiver = HookReceiver::start(vec![200]).await;
    let channel = create_webhook(&app, &seeded, &receiver.url("/hook")).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", true, &[channel]).await;
    let issue = open_issue(&app, project, source, "Boom", start).await;
    put_event(&app, issue, EventKind::Created, start + MINUTE).await;

    // A digest written but not yet delivered when the channel goes.
    let now = at(&app, start + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap().notifications,
        1
    );
    assert_eq!(delivery_count(&pool, source).await, 1);

    let (status, json) = send(
        &app,
        &seeded,
        Method::DELETE,
        &project_path(&seeded, &format!("channels/{channel}")),
        None,
    )
    .await;
    assert_eq!(status, 204, "{json}");
    assert_eq!(delivery_count(&pool, source).await, 0);

    let (status, json) = send(
        &app,
        &seeded,
        Method::GET,
        &project_path(&seeded, &format!("issue-sources/{source}")),
        None,
    )
    .await;
    assert_eq!(status, 200, "{json}");
    assert_eq!(json["channel_ids"], json!([]));

    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport::default()
    );
    assert!(receiver.received().is_empty());

    // A later event is not notified: the source has no channel left.
    let later = at(&app, now + 11 * MINUTE);
    put_event(&app, issue, EventKind::Regressed, later - 2 * MINUTE).await;
    assert_eq!(
        notify_issues(&app.state, later).await.unwrap(),
        NotifyReport::default()
    );
    assert_eq!(notification_count(&pool, source).await, 1);
}

#[tokio::test]
async fn the_email_channel_gets_the_digest_subject() {
    let app = TestApp::with_mailer().await;
    let seeded = app.seed().await;
    let project = seeded.project.id;
    let channel = create_email(&app, &seeded, &["ada@example.com"]).await;
    let start = app.state.clock.now_ns();
    let source = route_source(&app, &seeded, "Errors", true, &[channel]).await;
    for (index, minutes) in [1_i64, 2].into_iter().enumerate() {
        let issue = open_issue(&app, project, source, &format!("Issue {index}"), start).await;
        put_event(&app, issue, EventKind::Created, start + minutes * MINUTE).await;
    }

    let now = at(&app, start + 11 * MINUTE);
    assert_eq!(
        notify_issues(&app.state, now).await.unwrap(),
        NotifyReport {
            due: 1,
            notifications: 1,
            failed: 0,
        }
    );
    assert_eq!(
        deliver_once(&app.state, now).await.unwrap(),
        DeliverReport {
            sent: 1,
            ..DeliverReport::default()
        }
    );
    let mail = app.sent();
    assert_eq!(mail.len(), 1);
    assert_eq!(mail[0].to, "ada@example.com");
    assert_eq!(
        mail[0].subject,
        format!("[{PRODUCT}] 2 new or regressed issues: Errors")
    );
}
