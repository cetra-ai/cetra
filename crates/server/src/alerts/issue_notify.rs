//! Issue notifications: once per alert tick, the sources that may notify get one digest each (D-IC-21).

use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::issue_links::{Notified, due_sources, has_new_events, notify_source};

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct NotifyReport {
    /// Sources that were due.
    pub(crate) due: usize,
    /// Notifications written.
    pub(crate) notifications: usize,
    /// Sources whose write failed.
    pub(crate) failed: usize,
}

/// Writes a digest for every enabled source of an active project and org with at least one
/// channel whose `last_notified_at` is at least 10 minutes old and that has `created` or
/// `regressed` issue events in `(last_notified_at, now]`.
///
/// # Errors
///
/// Returns [`MetaError`] only when the due sources cannot be read. A failed read or write of one
/// source is logged (source id only) and counted.
pub(crate) async fn notify_issues(state: &AppState, now: i64) -> Result<NotifyReport, MetaError> {
    let due = due_sources(state.meta.reader(), now).await?;
    let mut report = NotifyReport {
        due: due.len(),
        ..NotifyReport::default()
    };
    for source in due {
        let id = source.id;
        match has_new_events(state.meta.reader(), &source, now).await {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => {
                tracing::error!(source_id = %id, error = %error, "issue notification check failed");
                report.failed += 1;
                continue;
            }
        }
        let written = state
            .meta
            .write(move |conn| Box::pin(async move { notify_source(conn, &source, now).await }))
            .await;
        match written {
            Ok(Notified::Sent {
                notification_id,
                items,
                more,
                deliveries,
            }) => {
                report.notifications += 1;
                tracing::info!(
                    source_id = %id,
                    notification_id = %notification_id,
                    items,
                    more,
                    deliveries,
                    "issue notification"
                );
            }
            // An event can vanish between the read and the write: nothing to send.
            Ok(Notified::NoEvents | Notified::Changed) => {}
            Err(error) => {
                tracing::error!(source_id = %id, error = %error, "issue notification write failed");
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::ProjectId;
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use super::*;
    use crate::meta::issue_links::{
        DIGEST_ITEMS, Digest, DigestKind, set_last_notified_at, set_source_channels,
    };
    use crate::meta::issues::{EventKind, IssueState, SourceKind};
    use crate::test_support::TestApp;
    use crate::test_support::issues::{
        issue_row, put_channel, put_event, put_issue, put_source, source_row,
    };

    const SECOND: i64 = 1_000_000_000;
    const MINUTE: i64 = 60 * SECOND;
    const HOUR: i64 = 60 * MINUTE;

    /// An enabled source of `project` with the given kind; `paused` sets a pause reason.
    async fn source(
        app: &TestApp,
        project: ProjectId,
        name: &str,
        kind: SourceKind,
        enabled: bool,
        paused: bool,
    ) -> Uuid {
        let mut row = source_row(project, name, kind, enabled, app.state.clock.now_ns());
        if paused {
            row.paused_reason = Some("budget".to_owned());
        }
        let id = row.id;
        put_source(app, row).await;
        id
    }

    /// An open issue of `source` seen at `at`.
    async fn issue(app: &TestApp, project: ProjectId, source: Uuid, title: &str, at: i64) -> Uuid {
        let id = Uuid::now_v7();
        put_issue(
            app,
            issue_row(project, source, id, title, IssueState::Open, at),
        )
        .await;
        id
    }

    /// Makes `source` notify `channels`, with its cut-off at `last`, as the source route does when
    /// it links the first channel or enables the source (D-IC-21).
    async fn link(app: &TestApp, source: Uuid, channels: Vec<Uuid>, last: i64) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_source_channels(conn, source, &channels).await?;
                    set_last_notified_at(conn, source, last).await
                })
            })
            .await
            .unwrap();
    }

    /// Runs one UPDATE on a project or org by id.
    async fn set_state(app: &TestApp, sql: &'static str, id: String) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(sql).bind(id).execute(&mut *conn).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    async fn last_notified(pool: &SqlitePool, source: Uuid) -> Option<i64> {
        sqlx::query_scalar("SELECT last_notified_at FROM issue_sources WHERE id = ?")
            .bind(source.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn notification_count(pool: &SqlitePool, source: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM issue_notifications WHERE source_id = ?")
            .bind(source.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
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
        .unwrap()
    }

    /// The digests written for `source`, oldest first.
    async fn digests(pool: &SqlitePool, source: Uuid) -> Vec<Digest> {
        let rows: Vec<String> = sqlx::query_scalar(
            "SELECT digest FROM issue_notifications WHERE source_id = ? ORDER BY created_at, id",
        )
        .bind(source.to_string())
        .fetch_all(pool)
        .await
        .unwrap();
        rows.iter()
            .map(|json| serde_json::from_str(json).unwrap())
            .collect()
    }

    async fn tick(app: &TestApp, now: i64) -> NotifyReport {
        notify_issues(&app.state, now).await.unwrap()
    }

    #[tokio::test]
    async fn a_source_with_events_and_channels_notifies_once() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let first = put_channel(&app, seeded.project.id, "first").await;
        let second = put_channel(&app, seeded.project.id, "second").await;
        link(&app, src, vec![first, second], now - HOUR).await;
        let one = issue(&app, seeded.project.id, src, "Boom", now - HOUR).await;
        let two = issue(&app, seeded.project.id, src, "Bang", now - HOUR).await;
        put_event(&app, one, EventKind::Created, now - 10 * MINUTE).await;
        put_event(&app, two, EventKind::Regressed, now - 5 * MINUTE).await;

        assert_eq!(
            tick(&app, now).await,
            NotifyReport {
                due: 1,
                notifications: 1,
                failed: 0,
            }
        );
        assert_eq!(notification_count(&pool, src).await, 1);
        assert_eq!(delivery_count(&pool, src).await, 2);
        assert_eq!(last_notified(&pool, src).await, Some(now - 5 * MINUTE));

        // The cut-off moved to the newest event, and the source is not due again yet.
        assert_eq!(tick(&app, now).await, NotifyReport::default());
        assert_eq!(notification_count(&pool, src).await, 1);
    }

    #[tokio::test]
    async fn throttle_is_ten_minutes() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - 2 * HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - 2 * HOUR).await;
        put_event(&app, target, EventKind::Created, now - 90 * MINUTE).await;

        // The first notification moves the cut-off to L = now - 90 min.
        let first = now - 90 * MINUTE;
        assert_eq!(tick(&app, now).await.notifications, 1);
        assert_eq!(last_notified(&pool, src).await, Some(first));

        // An event one minute after L waits until ten minutes after L.
        put_event(&app, target, EventKind::Regressed, first + MINUTE).await;
        assert_eq!(
            tick(&app, first + 10 * MINUTE - 1).await,
            NotifyReport::default()
        );
        assert_eq!(notification_count(&pool, src).await, 1);
        assert_eq!(
            tick(&app, first + 10 * MINUTE).await,
            NotifyReport {
                due: 1,
                notifications: 1,
                failed: 0,
            }
        );
        assert_eq!(notification_count(&pool, src).await, 2);
    }

    #[tokio::test]
    async fn an_event_inside_the_settle_margin_waits_for_the_next_tick() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - HOUR).await;
        // Newer than the settle margin at `now`: it may still be uncommitted.
        put_event(&app, target, EventKind::Created, now - 30 * SECOND).await;

        assert_eq!(
            tick(&app, now).await,
            NotifyReport {
                due: 1,
                notifications: 0,
                failed: 0
            }
        );
        assert_eq!(notification_count(&pool, src).await, 0);
        assert_eq!(last_notified(&pool, src).await, Some(now - HOUR));

        let later = now + MINUTE;
        assert_eq!(tick(&app, later).await.notifications, 1);
        assert_eq!(notification_count(&pool, src).await, 1);
        assert_eq!(last_notified(&pool, src).await, Some(now - 30 * SECOND));
    }

    #[tokio::test]
    async fn no_events_means_no_notification() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - 2 * HOUR).await;
        // Before the cut-off: not sent.
        put_event(&app, target, EventKind::Created, now - 2 * HOUR).await;

        assert_eq!(
            tick(&app, now).await,
            NotifyReport {
                due: 1,
                notifications: 0,
                failed: 0,
            }
        );
        assert_eq!(notification_count(&pool, src).await, 0);
        assert_eq!(last_notified(&pool, src).await, Some(now - HOUR));
    }

    #[tokio::test]
    async fn a_source_without_channels_never_notifies() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        link(&app, src, Vec::new(), now - HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - HOUR).await;
        put_event(&app, target, EventKind::Created, now - 10 * MINUTE).await;

        assert_eq!(tick(&app, now).await, NotifyReport::default());
        assert_eq!(notification_count(&pool, src).await, 0);
    }

    #[tokio::test]
    async fn a_disabled_source_never_notifies() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            false,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - HOUR).await;
        put_event(&app, target, EventKind::Created, now - 10 * MINUTE).await;

        assert_eq!(tick(&app, now).await, NotifyReport::default());
        assert_eq!(notification_count(&pool, src).await, 0);
    }

    #[tokio::test]
    async fn a_paused_source_still_notifies() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            true,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - HOUR).await;
        put_event(&app, target, EventKind::Created, now - 10 * MINUTE).await;

        assert_eq!(tick(&app, now).await.notifications, 1);
        assert_eq!(notification_count(&pool, src).await, 1);
    }

    #[tokio::test]
    async fn an_inactive_project_or_org_never_notifies() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - HOUR).await;
        put_event(&app, target, EventKind::Created, now - 10 * MINUTE).await;

        set_state(
            &app,
            "UPDATE projects SET state = 'deleting' WHERE id = ?",
            seeded.project.id.to_string(),
        )
        .await;
        assert_eq!(tick(&app, now).await, NotifyReport::default());

        set_state(
            &app,
            "UPDATE projects SET state = 'active' WHERE id = ?",
            seeded.project.id.to_string(),
        )
        .await;
        set_state(
            &app,
            "UPDATE orgs SET state = 'deleting' WHERE id = ?",
            seeded.org_id.to_string(),
        )
        .await;
        assert_eq!(tick(&app, now).await, NotifyReport::default());
        assert_eq!(notification_count(&pool, src).await, 0);
    }

    #[tokio::test]
    async fn enabling_a_source_does_not_send_its_backlog() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        let target = issue(&app, seeded.project.id, src, "Old", now - 3 * 24 * HOUR).await;
        put_event(&app, target, EventKind::Created, now - 3 * 24 * HOUR).await;
        // The enabling route sets the cut-off to now.
        link(&app, src, vec![channel], now).await;

        let later = now + 20 * MINUTE;
        assert_eq!(
            tick(&app, later).await,
            NotifyReport {
                due: 1,
                notifications: 0,
                failed: 0
            }
        );

        let fresh = issue(&app, seeded.project.id, src, "Fresh", now + 5 * MINUTE).await;
        put_event(&app, fresh, EventKind::Created, now + 5 * MINUTE).await;
        let after = now + 30 * MINUTE;
        assert_eq!(tick(&app, after).await.notifications, 1);
        let sent = digests(&pool, src).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].items.len(), 1);
        assert_eq!(sent[0].items[0].issue_id, fresh);
    }

    #[tokio::test]
    async fn adding_the_first_channel_does_not_send_the_backlog() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let target = issue(&app, seeded.project.id, src, "Old", now - 3 * 24 * HOUR).await;
        put_event(&app, target, EventKind::Created, now - 3 * 24 * HOUR).await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        // The source route sets the cut-off to now when the first channel is linked (Q12).
        link(&app, src, vec![channel], now).await;

        assert_eq!(
            tick(&app, now + 11 * MINUTE).await,
            NotifyReport {
                due: 1,
                notifications: 0,
                failed: 0
            }
        );

        // An event written after the route call is sent.
        let fresh = issue(&app, seeded.project.id, src, "Fresh", now + 5 * MINUTE).await;
        put_event(&app, fresh, EventKind::Created, now + 5 * MINUTE).await;
        assert_eq!(tick(&app, now + 16 * MINUTE).await.notifications, 1);
        let sent = digests(&pool, src).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].items.len(), 1);
        assert_eq!(sent[0].items[0].issue_id, fresh);
    }

    #[tokio::test]
    async fn regressed_events_are_sent_with_their_kind() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let created = issue(&app, seeded.project.id, src, "New", now - HOUR).await;
        let regressed = issue(&app, seeded.project.id, src, "Back", now - HOUR).await;
        put_event(&app, created, EventKind::Created, now - 20 * MINUTE).await;
        put_event(&app, regressed, EventKind::Regressed, now - 15 * MINUTE).await;

        assert_eq!(tick(&app, now).await.notifications, 1);
        let sent = digests(&pool, src).await;
        assert_eq!(sent.len(), 1);
        // Newest first.
        assert_eq!(sent[0].items.len(), 2);
        assert_eq!(sent[0].items[0].issue_id, regressed);
        assert_eq!(sent[0].items[0].kind, DigestKind::Regressed);
        assert_eq!(sent[0].items[1].issue_id, created);
        assert_eq!(sent[0].items[1].kind, DigestKind::Created);
        assert_eq!(sent[0].more, 0);
    }

    #[tokio::test]
    async fn the_digest_names_up_to_twenty_and_counts_the_rest() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        for index in 0..25i64 {
            let target = issue(
                &app,
                seeded.project.id,
                src,
                &format!("Issue {index}"),
                now - HOUR,
            )
            .await;
            put_event(
                &app,
                target,
                EventKind::Created,
                now - 30 * MINUTE + index * SECOND,
            )
            .await;
        }

        assert_eq!(tick(&app, now).await.notifications, 1);
        let sent = digests(&pool, src).await;
        assert_eq!(sent[0].items.len(), DIGEST_ITEMS);
        assert_eq!(sent[0].more, 5);
        assert_eq!(sent[0].items[0].title, "Issue 24");
        assert_eq!(delivery_count(&pool, src).await, 1);
    }

    #[tokio::test]
    async fn user_events_never_notify() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let src = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let target = issue(&app, seeded.project.id, src, "Boom", now - HOUR).await;
        put_event(&app, target, EventKind::StateChanged, now - 10 * MINUTE).await;
        put_event(&app, target, EventKind::Renamed, now - 9 * MINUTE).await;
        put_event(&app, target, EventKind::Merged, now - 8 * MINUTE).await;

        assert_eq!(
            tick(&app, now).await,
            NotifyReport {
                due: 1,
                notifications: 0,
                failed: 0,
            }
        );
        assert_eq!(notification_count(&pool, src).await, 0);
    }

    #[tokio::test]
    async fn two_sources_notify_independently() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        let errors = source(
            &app,
            seeded.project.id,
            "errors",
            SourceKind::Error,
            true,
            false,
        )
        .await;
        let topics = source(
            &app,
            seeded.project.id,
            "topics",
            SourceKind::Topic,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, errors, vec![channel], now - HOUR).await;
        link(&app, topics, vec![channel], now - HOUR).await;
        let error_issue = issue(&app, seeded.project.id, errors, "Boom", now - HOUR).await;
        let topic_issue = issue(&app, seeded.project.id, topics, "Topic", now - HOUR).await;
        put_event(&app, error_issue, EventKind::Created, now - 10 * MINUTE).await;
        put_event(&app, topic_issue, EventKind::Created, now - 10 * MINUTE).await;

        assert_eq!(
            tick(&app, now).await,
            NotifyReport {
                due: 2,
                notifications: 2,
                failed: 0,
            }
        );
        assert_eq!(notification_count(&pool, errors).await, 1);
        assert_eq!(notification_count(&pool, topics).await, 1);
    }

    #[tokio::test]
    async fn m3_assign_events_are_covered() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let pool = app.state.meta.reader().clone();
        // An LLM source (M3): its candidates become issues whose `created` event carries no actor.
        let src = source(
            &app,
            seeded.project.id,
            "topics",
            SourceKind::Topic,
            true,
            false,
        )
        .await;
        let channel = put_channel(&app, seeded.project.id, "chat").await;
        link(&app, src, vec![channel], now - HOUR).await;
        let target = issue(
            &app,
            seeded.project.id,
            src,
            "Refund questions",
            now - 10 * MINUTE,
        )
        .await;
        put_event(&app, target, EventKind::Created, now - 10 * MINUTE).await;

        assert_eq!(tick(&app, now).await.notifications, 1);
        let sent = digests(&pool, src).await;
        assert_eq!(sent[0].items[0].issue_id, target);
        assert_eq!(sent[0].items[0].kind, DigestKind::Created);
    }
}
