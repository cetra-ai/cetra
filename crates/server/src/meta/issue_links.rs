//! Issue links: source scopes, source channels and issue notifications (D-IC-20, D-IC-21).
//! A source notifies its channels of its new and regressed issues in one digest per
//! [`THROTTLE_NS`]. `kind` and `state` are stored as text; a stored value outside its list is
//! corrupt.

use std::collections::HashMap;
use std::fmt;

use cetra_core::ids::ProjectId;
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;
use super::issues::merged_ids;

const SOURCES: &str = "issue_sources";
const ISSUES: &str = "issues";
const EVENTS: &str = "issue_events";
const CHANNEL_LINKS: &str = "issue_source_channels";
const NOTIFICATIONS: &str = "issue_notifications";

/// Most items one digest holds (D-IC-21).
pub(crate) const DIGEST_ITEMS: usize = 20;
/// Minimum time between two notifications of one source: 10 minutes (D-IC-21).
pub(crate) const THROTTLE_NS: i64 = 600_000_000_000;
/// Most channels one source notifies (D-IC-21).
pub(crate) const MAX_SOURCE_CHANNELS: usize = 10;
/// A tick collects only events at least this old: 60 seconds. An event's `at` is the time its
/// writer read the clock, and its transaction can commit a little later; without the margin a tick
/// between the two would advance `last_notified_at` past an event it never saw, and the event
/// would never be sent (D-IC-21).
pub(crate) const ISSUE_NOTIFY_SETTLE_NS: i64 = 60_000_000_000;

/// What one issue notification shows (D-IC-21).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Digest {
    /// At most [`DIGEST_ITEMS`], newest first.
    pub(crate) items: Vec<DigestItem>,
    /// Events that did not fit in `items`.
    pub(crate) more: u64,
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Digest")
            .field("items", &self.items.len())
            .field("more", &self.more)
            .finish()
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DigestItem {
    pub(crate) issue_id: Uuid,
    pub(crate) kind: DigestKind,
    pub(crate) title: String,
}

impl fmt::Debug for DigestItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DigestItem")
            .field("kind", &self.kind)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DigestKind {
    Created,
    Regressed,
}

/// An enabled source of an active project in an active org that can notify now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NotifySource {
    pub(crate) id: Uuid,
    pub(crate) last_notified_at: i64,
}

/// What one notify attempt did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Notified {
    /// A notification row and one pending delivery per channel were written.
    Sent {
        notification_id: Uuid,
        items: usize,
        more: u64,
        deliveries: usize,
    },
    /// No `created` or `regressed` event since `last_notified_at`; nothing was written.
    NoEvents,
    /// The source changed (or lost its channels) since `due_sources` read it; nothing was written.
    Changed,
}

/// The issue `issue` of `project` and the issues that count with it: its merge target when it is
/// merged, then the issues merged into that target (D-IC-9). `None` when the issue does not exist
/// in `project`.
pub(crate) async fn issue_scope_ids(
    pool: &SqlitePool,
    project: ProjectId,
    issue: Uuid,
) -> Result<Option<Vec<Uuid>>, MetaError> {
    let merged_into: Option<Option<String>> =
        sqlx::query_scalar("SELECT merged_into FROM issues WHERE project_id = ? AND id = ?")
            .bind(project.to_string())
            .bind(issue.to_string())
            .fetch_optional(pool)
            .await?;
    let Some(merged_into) = merged_into else {
        return Ok(None);
    };
    let target = match merged_into {
        Some(text) => parse_uuid(&text, ISSUES, "merged_into")?,
        None => issue,
    };
    let mut ids = vec![target];
    ids.extend(merged_ids(pool, project, target).await?);
    Ok(Some(ids))
}

/// The issues of `source` that count for it: not ignored or merged, and the issues merged into
/// a counted issue. `None` when the source does not exist in `project`.
pub(crate) async fn source_scope_ids(
    pool: &SqlitePool,
    project: ProjectId,
    source: Uuid,
) -> Result<Option<Vec<Uuid>>, MetaError> {
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM issue_sources WHERE project_id = ? AND id = ?")
            .bind(project.to_string())
            .bind(source.to_string())
            .fetch_optional(pool)
            .await?;
    if exists.is_none() {
        return Ok(None);
    }
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM issues
          WHERE project_id = ?1 AND source_id = ?2 AND state NOT IN ('ignored', 'merged')
         UNION
         SELECT m.id FROM issues m JOIN issues t ON m.merged_into = t.id
          WHERE t.project_id = ?1 AND t.source_id = ?2 AND t.state NOT IN ('ignored', 'merged')
         ORDER BY id",
    )
    .bind(project.to_string())
    .bind(source.to_string())
    .fetch_all(pool)
    .await?;
    parse_uuids(ids, ISSUES, "id").map(Some)
}

/// The channels of `source`, ordered by id.
pub(crate) async fn source_channel_ids(
    pool: &SqlitePool,
    source: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM issue_source_channels WHERE source_id = ? ORDER BY channel_id",
    )
    .bind(source.to_string())
    .fetch_all(pool)
    .await?;
    parse_uuids(ids, CHANNEL_LINKS, "channel_id")
}

/// The channels of every source of `project` that has any, ordered by id.
pub(crate) async fn source_channel_ids_by_source(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<HashMap<Uuid, Vec<Uuid>>, MetaError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.source_id, c.channel_id FROM issue_source_channels c \
         JOIN issue_sources s ON s.id = c.source_id \
         WHERE s.project_id = ? ORDER BY c.source_id, c.channel_id",
    )
    .bind(project.to_string())
    .fetch_all(pool)
    .await?;
    let mut channels: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (source, channel) in rows {
        let source = parse_uuid(&source, CHANNEL_LINKS, "source_id")?;
        let channel = parse_uuid(&channel, CHANNEL_LINKS, "channel_id")?;
        channels.entry(source).or_default().push(channel);
    }
    Ok(channels)
}

/// Replaces the channels of `source`. The caller validated the ids (M4.6).
pub(crate) async fn set_source_channels(
    conn: &mut SqliteConnection,
    source: Uuid,
    channels: &[Uuid],
) -> Result<(), MetaError> {
    sqlx::query("DELETE FROM issue_source_channels WHERE source_id = ?")
        .bind(source.to_string())
        .execute(&mut *conn)
        .await?;
    for channel in channels {
        sqlx::query("INSERT INTO issue_source_channels (source_id, channel_id) VALUES (?, ?)")
            .bind(source.to_string())
            .bind(channel.to_string())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// Whether the source has at least one channel (M4.6 uses it inside the update transaction).
pub(crate) async fn source_has_channels(
    conn: &mut SqliteConnection,
    source: Uuid,
) -> Result<bool, MetaError> {
    let has: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM issue_source_channels WHERE source_id = ?)",
    )
    .bind(source.to_string())
    .fetch_one(&mut *conn)
    .await?;
    Ok(has)
}

/// Sets the notification cut-off of `source` to `at` (nanoseconds).
pub(crate) async fn set_last_notified_at(
    conn: &mut SqliteConnection,
    source: Uuid,
    at: i64,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE issue_sources SET last_notified_at = ? WHERE id = ?")
        .bind(at)
        .bind(source.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The sources that may notify at `now`: enabled, paused or not, with at least one channel, a
/// cut-off at least [`THROTTLE_NS`] old, in an active project of an active org. Oldest cut-off
/// first.
pub(crate) async fn due_sources(
    pool: &SqlitePool,
    now: i64,
) -> Result<Vec<NotifySource>, MetaError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT s.id, s.last_notified_at
           FROM issue_sources s
           JOIN projects p ON p.id = s.project_id
           JOIN orgs o ON o.id = p.org_id
          WHERE s.enabled = 1 AND p.state = 'active' AND o.state = 'active'
            AND s.last_notified_at IS NOT NULL
            AND s.last_notified_at <= ?1
            AND EXISTS (SELECT 1 FROM issue_source_channels c WHERE c.source_id = s.id)
          ORDER BY s.last_notified_at, s.id",
    )
    .bind(now.saturating_sub(THROTTLE_NS))
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(id, last_notified_at)| {
            Ok(NotifySource {
                id: parse_uuid(&id, SOURCES, "id")?,
                last_notified_at,
            })
        })
        .collect()
}

/// Whether [`notify_source`] would find at least one event now: the count of its first step,
/// read on the pool, so a tick does not take the writer for a source with nothing to send.
pub(crate) async fn has_new_events(
    pool: &SqlitePool,
    source: &NotifySource,
    now: i64,
) -> Result<bool, MetaError> {
    let count: i64 = sqlx::query_scalar(EVENT_COUNT_SQL)
        .bind(source.id.to_string())
        .bind(source.last_notified_at)
        .bind(settle_cutoff(now))
        .fetch_one(pool)
        .await?;
    Ok(count > 0)
}

/// Writes the notification of `source` at `now`, if it has events to send. Runs inside one
/// writer transaction. The events it sends are those after the source's cut-off and no newer
/// than the settle margin; the cut-off then moves to the newest of them.
pub(crate) async fn notify_source(
    conn: &mut SqliteConnection,
    source: &NotifySource,
    now: i64,
) -> Result<Notified, MetaError> {
    let current: Option<Option<i64>> =
        sqlx::query_scalar("SELECT last_notified_at FROM issue_sources WHERE id = ?")
            .bind(source.id.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    if current != Some(Some(source.last_notified_at)) {
        return Ok(Notified::Changed);
    }

    let cutoff = settle_cutoff(now);
    let count: i64 = sqlx::query_scalar(EVENT_COUNT_SQL)
        .bind(source.id.to_string())
        .bind(source.last_notified_at)
        .bind(cutoff)
        .fetch_one(&mut *conn)
        .await?;
    if count == 0 {
        return Ok(Notified::NoEvents);
    }
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT e.at, e.kind, i.id AS issue_id, i.title
           FROM issue_events e JOIN issues i ON i.id = e.issue_id
          WHERE i.source_id = ?1 AND e.kind IN ('created', 'regressed') AND e.at > ?2 AND e.at <= ?3
          ORDER BY e.at DESC, e.id DESC LIMIT ?4",
    )
    .bind(source.id.to_string())
    .bind(source.last_notified_at)
    .bind(cutoff)
    .bind(i64::try_from(DIGEST_ITEMS).unwrap_or(i64::MAX))
    .fetch_all(&mut *conn)
    .await?;
    let Some(newest) = rows.first().map(|row| row.0) else {
        return Ok(Notified::NoEvents);
    };
    let items = rows
        .iter()
        .map(|(_, kind, issue_id, title)| {
            Ok(DigestItem {
                issue_id: parse_uuid(issue_id, ISSUES, "id")?,
                kind: digest_kind(kind)?,
                title: title.clone(),
            })
        })
        .collect::<Result<Vec<_>, MetaError>>()?;
    let total = u64::try_from(count).unwrap_or(0);
    let shown = u64::try_from(items.len()).unwrap_or(u64::MAX);
    let digest = Digest {
        items,
        more: total.saturating_sub(shown),
    };

    let channels: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM issue_source_channels WHERE source_id = ? ORDER BY channel_id",
    )
    .bind(source.id.to_string())
    .fetch_all(&mut *conn)
    .await?;
    if channels.is_empty() {
        return Ok(Notified::Changed);
    }
    let channels = parse_uuids(channels, CHANNEL_LINKS, "channel_id")?;

    let advanced = sqlx::query(
        "UPDATE issue_sources SET last_notified_at = ?1 WHERE id = ?2 AND last_notified_at = ?3",
    )
    .bind(newest)
    .bind(source.id.to_string())
    .bind(source.last_notified_at)
    .execute(&mut *conn)
    .await?;
    if advanced.rows_affected() == 0 {
        return Ok(Notified::Changed);
    }

    let notification_id = Uuid::now_v7();
    let json = serde_json::to_string(&digest).map_err(|_| corrupt(NOTIFICATIONS, "digest"))?;
    sqlx::query(
        "INSERT INTO issue_notifications (id, source_id, digest, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(notification_id.to_string())
    .bind(source.id.to_string())
    .bind(json)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    for channel in &channels {
        sqlx::query(
            "INSERT INTO notification_deliveries
               (id, event_id, issue_notification_id, channel_id, status, attempts, next_attempt_at, last_error, created_at, updated_at)
             VALUES (?, NULL, ?, ?, 'pending', 0, ?, NULL, ?, ?)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(notification_id.to_string())
        .bind(channel.to_string())
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(Notified::Sent {
        notification_id,
        items: digest.items.len(),
        more: digest.more,
        deliveries: channels.len(),
    })
}

/// The events of a source that a notification may send: `created` and `regressed` after the
/// cut-off (`?2`) and no newer than the settle margin (`?3`), whatever the issue's state (Q10).
const EVENT_COUNT_SQL: &str = "SELECT COUNT(*) FROM issue_events e JOIN issues i ON i.id = e.issue_id \
     WHERE i.source_id = ?1 AND e.kind IN ('created', 'regressed') AND e.at > ?2 AND e.at <= ?3";

/// The newest event that a tick collects: `now` less the settle margin (Q8).
fn settle_cutoff(now: i64) -> i64 {
    now.saturating_sub(ISSUE_NOTIFY_SETTLE_NS)
}

fn digest_kind(text: &str) -> Result<DigestKind, MetaError> {
    match text {
        "created" => Ok(DigestKind::Created),
        "regressed" => Ok(DigestKind::Regressed),
        _ => Err(corrupt(EVENTS, "kind")),
    }
}

fn corrupt(table: &'static str, column: &'static str) -> MetaError {
    MetaError::Corrupt { table, column }
}

fn parse_uuid(value: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    value.parse().map_err(|_| corrupt(table, column))
}

fn parse_uuids(
    values: Vec<String>,
    table: &'static str,
    column: &'static str,
) -> Result<Vec<Uuid>, MetaError> {
    values
        .iter()
        .map(|value| parse_uuid(value, table, column))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::issues::{self as store, EventKind, IssueState, SourceKind};
    use crate::test_support::issues::{
        issue_row, put_channel, put_event, put_issue, put_source, source_row,
    };
    use crate::test_support::{Seeded, TestApp};

    const SECOND_NS: i64 = 1_000_000_000;
    const MINUTE_NS: i64 = 60 * SECOND_NS;
    const HOUR_NS: i64 = 60 * MINUTE_NS;

    fn reader(app: &TestApp) -> &SqlitePool {
        app.state.meta.reader()
    }

    async fn count(app: &TestApp, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    async fn last_notified(app: &TestApp, source: Uuid) -> Option<i64> {
        sqlx::query_scalar("SELECT last_notified_at FROM issue_sources WHERE id = ?")
            .bind(source.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    /// Runs one UPDATE by id.
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

    async fn source(app: &TestApp, seeded: &Seeded, name: &str, enabled: bool) -> Uuid {
        let row = source_row(
            seeded.project.id,
            name,
            SourceKind::Error,
            enabled,
            app.state.clock.now_ns(),
        );
        let id = row.id;
        put_source(app, row).await;
        id
    }

    async fn issue(
        app: &TestApp,
        seeded: &Seeded,
        source: Uuid,
        title: &str,
        state: IssueState,
        seen: i64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        put_issue(
            app,
            issue_row(seeded.project.id, source, id, title, state, seen),
        )
        .await;
        id
    }

    /// An issue of `source` merged into `target`.
    async fn merged_into(app: &TestApp, seeded: &Seeded, source: Uuid, target: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        let mut row = issue_row(
            seeded.project.id,
            source,
            id,
            "merged",
            IssueState::Merged,
            0,
        );
        row.merged_into = Some(target);
        put_issue(app, row).await;
        id
    }

    async fn set_channels(app: &TestApp, source: Uuid, channels: Vec<Uuid>) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { set_source_channels(conn, source, &channels).await })
            })
            .await
            .unwrap();
    }

    async fn has_channels(app: &TestApp, source: Uuid) -> bool {
        app.state
            .meta
            .write(move |conn| Box::pin(async move { source_has_channels(conn, source).await }))
            .await
            .unwrap()
    }

    async fn set_last(app: &TestApp, source: Uuid, at: i64) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { set_last_notified_at(conn, source, at).await })
            })
            .await
            .unwrap();
    }

    /// An enabled source linked to `channels` whose cut-off is `last`, as the notify tests start.
    async fn linked(
        app: &TestApp,
        seeded: &Seeded,
        name: &str,
        channels: Vec<Uuid>,
        last: i64,
    ) -> NotifySource {
        let id = source(app, seeded, name, true).await;
        set_channels(app, id, channels).await;
        set_last(app, id, last).await;
        NotifySource {
            id,
            last_notified_at: last,
        }
    }

    async fn notify(app: &TestApp, source: &NotifySource, now: i64) -> Notified {
        let source = source.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { notify_source(conn, &source, now).await }))
            .await
            .unwrap()
    }

    async fn due_ids(app: &TestApp, now: i64) -> Vec<Uuid> {
        due_sources(reader(app), now)
            .await
            .unwrap()
            .into_iter()
            .map(|source| source.id)
            .collect()
    }

    #[tokio::test]
    async fn issue_scope_includes_the_merged_set() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let src = source(&app, &seeded, "errors", true).await;
        let target = issue(&app, &seeded, src, "Boom", IssueState::Open, now).await;
        let a = merged_into(&app, &seeded, src, target).await;
        let b = merged_into(&app, &seeded, src, target).await;
        let mut merged = vec![a, b];
        merged.sort();
        let mut expected = vec![target];
        expected.extend(merged);
        assert_eq!(
            issue_scope_ids(reader(&app), seeded.project.id, target)
                .await
                .unwrap(),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn issue_scope_of_a_merged_issue_is_its_targets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let src = source(&app, &seeded, "errors", true).await;
        let target = issue(&app, &seeded, src, "Boom", IssueState::Open, now).await;
        let a = merged_into(&app, &seeded, src, target).await;
        let b = merged_into(&app, &seeded, src, target).await;
        let mut merged = vec![a, b];
        merged.sort();
        let mut expected = vec![target];
        expected.extend(merged);
        assert_eq!(
            issue_scope_ids(reader(&app), seeded.project.id, a)
                .await
                .unwrap(),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn issue_scope_of_a_missing_issue_is_none_and_is_project_scoped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let other_source = source_row(other.id, "other", SourceKind::Error, true, now);
        let other_source_id = other_source.id;
        put_source(&app, other_source).await;
        let foreign = issue(
            &app,
            &seeded,
            other_source_id,
            "foreign",
            IssueState::Open,
            now,
        )
        .await;
        assert_eq!(
            issue_scope_ids(reader(&app), seeded.project.id, Uuid::now_v7())
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            issue_scope_ids(reader(&app), seeded.project.id, foreign)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            issue_scope_ids(reader(&app), other.id, foreign)
                .await
                .unwrap(),
            Some(vec![foreign])
        );
    }

    #[tokio::test]
    async fn source_scope_skips_ignored_and_merged_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let src = source(&app, &seeded, "errors", true).await;
        let open = issue(&app, &seeded, src, "open", IssueState::Open, now).await;
        let resolved = issue(&app, &seeded, src, "resolved", IssueState::Resolved, now).await;
        let ignored = issue(&app, &seeded, src, "ignored", IssueState::Ignored, now).await;
        let _under_ignored = merged_into(&app, &seeded, src, ignored).await;
        let under_open = merged_into(&app, &seeded, src, open).await;
        let mut expected = vec![open, resolved, under_open];
        expected.sort();
        assert_eq!(
            source_scope_ids(reader(&app), seeded.project.id, src)
                .await
                .unwrap(),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn source_scope_of_a_source_without_issues_is_empty_not_none() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let empty = source(&app, &seeded, "empty", true).await;
        assert_eq!(
            source_scope_ids(reader(&app), seeded.project.id, empty)
                .await
                .unwrap(),
            Some(Vec::new())
        );
        assert_eq!(
            source_scope_ids(reader(&app), seeded.project.id, Uuid::now_v7())
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn source_channels_round_trip_sorted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let c1 = put_channel(&app, project, "ops").await;
        let c2 = put_channel(&app, project, "email").await;
        let src = source(&app, &seeded, "errors", true).await;
        set_channels(&app, src, vec![c1, c2]).await;
        let mut expected = vec![c1, c2];
        expected.sort();
        assert_eq!(
            source_channel_ids(reader(&app), src).await.unwrap(),
            expected
        );
        let by_source = source_channel_ids_by_source(reader(&app), project)
            .await
            .unwrap();
        assert_eq!(by_source.len(), 1);
        assert_eq!(by_source.get(&src), Some(&expected));
    }

    #[tokio::test]
    async fn set_source_channels_replaces() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let c1 = put_channel(&app, project, "ops").await;
        let c2 = put_channel(&app, project, "email").await;
        let c3 = put_channel(&app, project, "chat").await;
        let src = source(&app, &seeded, "errors", true).await;
        set_channels(&app, src, vec![c1, c2]).await;
        set_channels(&app, src, vec![c3]).await;
        assert_eq!(
            source_channel_ids(reader(&app), src).await.unwrap(),
            vec![c3]
        );
    }

    #[tokio::test]
    async fn source_has_channels_follows_the_links() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let src = source(&app, &seeded, "errors", true).await;
        assert!(!has_channels(&app, src).await);
        set_channels(&app, src, vec![c1]).await;
        assert!(has_channels(&app, src).await);
        set_channels(&app, src, Vec::new()).await;
        assert!(!has_channels(&app, src).await);
    }

    #[tokio::test]
    async fn deleting_a_channel_removes_it_from_a_source() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let c1 = put_channel(&app, project, "ops").await;
        let c2 = put_channel(&app, project, "email").await;
        let src = source(&app, &seeded, "errors", true).await;
        set_channels(&app, src, vec![c1, c2]).await;
        set_state(
            &app,
            "DELETE FROM notification_channels WHERE id = ?",
            c1.to_string(),
        )
        .await;
        assert_eq!(
            source_channel_ids(reader(&app), src).await.unwrap(),
            vec![c2]
        );
    }

    #[tokio::test]
    async fn deleting_a_source_removes_its_links_and_notifications() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let src = linked(&app, &seeded, "errors", vec![c1], now - HOUR_NS).await;
        let id = src.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO issue_notifications (id, source_id, digest, created_at) \
                         VALUES (?, ?, '{\"items\":[],\"more\":0}', 0)",
                    )
                    .bind(Uuid::now_v7().to_string())
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let links = format!("SELECT COUNT(*) FROM issue_source_channels WHERE source_id = '{id}'");
        let notes = format!("SELECT COUNT(*) FROM issue_notifications WHERE source_id = '{id}'");
        assert_eq!(count(&app, &links).await, 1);
        assert_eq!(count(&app, &notes).await, 1);
        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { store::delete_source(conn, project, id).await })
            })
            .await
            .unwrap();
        assert_eq!(count(&app, &links).await, 0);
        assert_eq!(count(&app, &notes).await, 0);
    }

    #[tokio::test]
    async fn due_sources_requires_enabled_channels_and_the_throttle() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let t0 = now;
        let channel = put_channel(&app, project, "ops").await;

        let due = source(&app, &seeded, "due", true).await;
        set_channels(&app, due, vec![channel]).await;
        set_last(&app, due, t0).await;

        let disabled = source(&app, &seeded, "disabled", false).await;
        set_channels(&app, disabled, vec![channel]).await;
        set_last(&app, disabled, t0).await;

        let bare = source(&app, &seeded, "bare", true).await;
        set_last(&app, bare, t0).await;

        let fresh = source(&app, &seeded, "fresh", true).await;
        set_channels(&app, fresh, vec![channel]).await;

        let paused = {
            let mut row = source_row(project, "paused", SourceKind::Error, true, now);
            row.paused_reason = Some("budget".to_owned());
            row.paused_at = Some(now);
            let id = row.id;
            put_source(&app, row).await;
            id
        };
        set_channels(&app, paused, vec![channel]).await;
        set_last(&app, paused, t0).await;

        let (other, _) = app.add_project(&seeded, "other", None).await;
        let other_channel = put_channel(&app, other.id, "ops").await;
        let other_source = {
            let row = source_row(other.id, "other", SourceKind::Error, true, now);
            let id = row.id;
            put_source(&app, row).await;
            id
        };
        set_channels(&app, other_source, vec![other_channel]).await;
        set_last(&app, other_source, t0).await;

        let throttle_end = t0 + THROTTLE_NS;
        assert!(due_ids(&app, throttle_end - 1).await.is_empty());
        let ids = due_ids(&app, throttle_end).await;
        assert_eq!(ids.len(), 3, "due, paused and the other project's source");
        for id in [due, paused, other_source] {
            assert!(ids.contains(&id));
        }

        set_state(
            &app,
            "UPDATE projects SET state = 'deleting' WHERE id = ?",
            other.id.to_string(),
        )
        .await;
        let ids = due_ids(&app, throttle_end).await;
        assert_eq!(ids.len(), 2);
        assert!(!ids.contains(&other_source));

        set_state(
            &app,
            "UPDATE orgs SET state = 'deleting' WHERE id = ?",
            seeded.org_id.to_string(),
        )
        .await;
        assert!(due_ids(&app, throttle_end).await.is_empty());
    }

    #[tokio::test]
    async fn notify_source_writes_one_notification_per_channel() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let c2 = put_channel(&app, seeded.project.id, "email-ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1, c2], last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        put_event(&app, open, EventKind::Created, now - 10 * MINUTE_NS).await;
        put_event(&app, open, EventKind::Regressed, now - 5 * MINUTE_NS).await;

        let Notified::Sent {
            notification_id,
            items,
            more,
            deliveries,
        } = notify(&app, &src, now).await
        else {
            panic!("the source had two events to send");
        };
        assert_eq!((items, more, deliveries), (2, 0, 2));
        assert_eq!(
            count(&app, "SELECT COUNT(*) FROM issue_notifications").await,
            1
        );
        let pending = format!(
            "SELECT COUNT(*) FROM notification_deliveries \
             WHERE event_id IS NULL AND status = 'pending' \
               AND issue_notification_id = '{notification_id}'"
        );
        assert_eq!(count(&app, &pending).await, 2);
        assert_eq!(last_notified(&app, src.id).await, Some(now - 5 * MINUTE_NS));
    }

    #[tokio::test]
    async fn notify_source_digest_is_newest_first_and_capped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;
        let base = now - 30 * MINUTE_NS;
        for i in 0..25 {
            let id = issue(
                &app,
                &seeded,
                src.id,
                &format!("issue {i}"),
                IssueState::Open,
                base,
            )
            .await;
            put_event(&app, id, EventKind::Created, base + i * SECOND_NS).await;
        }

        let Notified::Sent {
            notification_id,
            items,
            more,
            deliveries,
        } = notify(&app, &src, now).await
        else {
            panic!("the source had events to send");
        };
        assert_eq!((items, more, deliveries), (20, 5, 1));
        let text: String =
            sqlx::query_scalar("SELECT digest FROM issue_notifications WHERE id = ?")
                .bind(notification_id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .unwrap();
        let digest: Digest = serde_json::from_str(&text).unwrap();
        let titles: Vec<String> = digest.items.iter().map(|item| item.title.clone()).collect();
        let expected: Vec<String> = (5..25).rev().map(|i| format!("issue {i}")).collect();
        assert_eq!(titles, expected);
        assert_eq!(digest.more, 5);
    }

    #[tokio::test]
    async fn notify_source_counts_only_created_and_regressed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        put_event(&app, open, EventKind::StateChanged, now - 10 * MINUTE_NS).await;
        put_event(&app, open, EventKind::Renamed, now - 9 * MINUTE_NS).await;
        put_event(&app, open, EventKind::Merged, now - 8 * MINUTE_NS).await;

        assert_eq!(notify(&app, &src, now).await, Notified::NoEvents);
        assert_eq!(
            count(&app, "SELECT COUNT(*) FROM issue_notifications").await,
            0
        );
        assert_eq!(last_notified(&app, src.id).await, Some(last));
    }

    #[tokio::test]
    async fn notify_source_ignores_events_at_or_before_the_cutoff() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        put_event(&app, open, EventKind::Created, last).await;
        let settled = now - ISSUE_NOTIFY_SETTLE_NS;
        put_event(&app, open, EventKind::Created, settled).await;

        let Notified::Sent { items, .. } = notify(&app, &src, now).await else {
            panic!("the event at the margin is sent");
        };
        assert_eq!(items, 1);
        assert_eq!(last_notified(&app, src.id).await, Some(settled));
    }

    #[tokio::test]
    async fn an_event_inside_the_settle_margin_waits_for_the_next_tick() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        put_event(
            &app,
            open,
            EventKind::Created,
            now - ISSUE_NOTIFY_SETTLE_NS + 1,
        )
        .await;

        assert_eq!(notify(&app, &src, now).await, Notified::NoEvents);
        assert_eq!(last_notified(&app, src.id).await, Some(last));

        let next = notify(&app, &src, now + SECOND_NS).await;
        assert!(matches!(
            next,
            Notified::Sent {
                items: 1,
                more: 0,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn only_the_events_outside_the_settle_margin_are_sent_first() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        let outside = now - ISSUE_NOTIFY_SETTLE_NS - SECOND_NS;
        let inside = now - ISSUE_NOTIFY_SETTLE_NS + 1;
        put_event(&app, open, EventKind::Created, outside).await;
        put_event(&app, open, EventKind::Created, inside).await;

        let Notified::Sent { items, .. } = notify(&app, &src, now).await else {
            panic!("the event outside the margin is sent");
        };
        assert_eq!(items, 1);
        assert_eq!(last_notified(&app, src.id).await, Some(outside));

        let next = NotifySource {
            id: src.id,
            last_notified_at: outside,
        };
        assert_eq!(notify(&app, &next, now).await, Notified::NoEvents);
        let later = notify(&app, &next, now + SECOND_NS).await;
        assert!(matches!(later, Notified::Sent { items: 1, .. }));
        assert_eq!(last_notified(&app, src.id).await, Some(inside));
    }

    #[tokio::test]
    async fn has_new_events_matches_notify_source() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        assert!(!has_new_events(reader(&app), &src, now).await.unwrap());
        put_event(&app, open, EventKind::Created, last).await;
        assert!(!has_new_events(reader(&app), &src, now).await.unwrap());
        put_event(
            &app,
            open,
            EventKind::Created,
            now - ISSUE_NOTIFY_SETTLE_NS + 1,
        )
        .await;
        assert!(!has_new_events(reader(&app), &src, now).await.unwrap());
        put_event(&app, open, EventKind::Created, now - ISSUE_NOTIFY_SETTLE_NS).await;
        assert!(has_new_events(reader(&app), &src, now).await.unwrap());
    }

    #[tokio::test]
    async fn notify_source_with_no_events_changes_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;

        assert_eq!(notify(&app, &src, now).await, Notified::NoEvents);
        assert_eq!(last_notified(&app, src.id).await, Some(last));
        assert_eq!(
            count(&app, "SELECT COUNT(*) FROM issue_notifications").await,
            0
        );
        assert_eq!(
            count(&app, "SELECT COUNT(*) FROM notification_deliveries").await,
            0
        );
    }

    #[tokio::test]
    async fn notify_source_returns_changed_when_the_cutoff_moved() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let c1 = put_channel(&app, seeded.project.id, "ops").await;
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", vec![c1], last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        put_event(&app, open, EventKind::Created, now - 10 * MINUTE_NS).await;
        let moved = now - 30 * MINUTE_NS;
        set_last(&app, src.id, moved).await;

        assert_eq!(notify(&app, &src, now).await, Notified::Changed);
        assert_eq!(last_notified(&app, src.id).await, Some(moved));
        assert_eq!(
            count(&app, "SELECT COUNT(*) FROM issue_notifications").await,
            0
        );
    }

    #[tokio::test]
    async fn notify_source_without_channels_is_changed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let last = now - HOUR_NS;
        let src = linked(&app, &seeded, "errors", Vec::new(), last).await;
        let open = issue(&app, &seeded, src.id, "Boom", IssueState::Open, last).await;
        put_event(&app, open, EventKind::Created, now - 10 * MINUTE_NS).await;

        assert_eq!(notify(&app, &src, now).await, Notified::Changed);
        assert_eq!(last_notified(&app, src.id).await, Some(last));
        assert_eq!(
            count(&app, "SELECT COUNT(*) FROM issue_notifications").await,
            0
        );
    }

    #[tokio::test]
    async fn the_delivery_check_refuses_both_or_neither() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let channel = put_channel(&app, seeded.project.id, "ops").await;
        for (event, notification) in [(Some(Uuid::now_v7()), Some(Uuid::now_v7())), (None, None)] {
            let result = app
                .state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        sqlx::query(
                            "INSERT INTO notification_deliveries \
                             (id, event_id, issue_notification_id, channel_id, status, attempts, \
                              next_attempt_at, last_error, created_at, updated_at) \
                             VALUES (?, ?, ?, ?, 'pending', 0, NULL, NULL, 0, 0)",
                        )
                        .bind(Uuid::now_v7().to_string())
                        .bind(event.map(|id| id.to_string()))
                        .bind(notification.map(|id| id.to_string()))
                        .bind(channel.to_string())
                        .execute(&mut *conn)
                        .await?;
                        Ok(())
                    })
                })
                .await;
            let error = result.expect_err("the CHECK refuses the row");
            assert!(
                error.to_string().contains("CHECK constraint failed"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn the_rebuilt_deliveries_table_keeps_its_indexes() {
        let app = TestApp::new().await;
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM pragma_index_list('notification_deliveries') \
             WHERE origin = 'c' ORDER BY name",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(
            names,
            [
                "notification_deliveries_channel",
                "notification_deliveries_due",
                "notification_deliveries_event",
                "notification_deliveries_issue",
            ]
        );
    }

    #[tokio::test]
    async fn digest_debug_hides_titles() {
        let digest = Digest {
            items: vec![DigestItem {
                issue_id: Uuid::nil(),
                kind: DigestKind::Created,
                title: "A secret title".to_owned(),
            }],
            more: 2,
        };
        assert_eq!(format!("{digest:?}"), "Digest { items: 1, more: 2 }");
        assert_eq!(
            format!("{:?}", digest.items[0]),
            "DigestItem { kind: Created }"
        );
    }
}
