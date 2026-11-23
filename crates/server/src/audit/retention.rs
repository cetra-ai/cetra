//! Retention of audit records (D-EA-7): the hourly purge of records older than
//! `audit.retention_days`, and the deletion of an org's records when its drop is applied.

use cetra_core::ids::OrgId;

use crate::http::AppState;
use crate::meta::{MetaDb, MetaError};

/// Rows deleted per statement, so the writer is never held long.
pub(crate) const PURGE_BATCH: i64 = 5_000;

/// The shortest retention an org limit can set (D-EA-7, plan P-4).
const MIN_RETENTION_DAYS: u64 = 30;

const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// Purges the instance log at `audit.retention_days` (0 = keep) and each org's records at
/// the org's own limit (`None` = the instance's days, `0` = keep, otherwise at least 30
/// days; D-EA-7, plan P-4). Returns the count of deleted records.
///
/// # Errors
///
/// The [`MetaError`] of the first batch that fails; the batches before it stay deleted.
pub(crate) async fn purge(state: &AppState, now: i64) -> Result<u64, MetaError> {
    let meta = &state.meta;
    let instance_days = state.config.audit.retention_days;
    let mut removed = purge_scope(meta, None, instance_days, now).await?;
    // Every org, active or not: a dropped org's records were already deleted (D-EA-7).
    let org_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM orgs")
        .fetch_all(meta.reader())
        .await
        .map_err(MetaError::from)?;
    for id in org_ids {
        // `orgs.id` is written from an `OrgId`, so this parses; a row that does not is skipped.
        let Ok(org) = id.parse::<OrgId>() else {
            continue;
        };
        let days = match state.entitlements.resolve(org, now).limits.audit_retention_days {
            None => instance_days,
            Some(0) => 0,
            Some(days) => u32::try_from(days.max(MIN_RETENTION_DAYS)).unwrap_or(u32::MAX),
        };
        removed += purge_scope(meta, Some(org), days, now).await?;
    }
    Ok(removed)
}

/// Deletes records of `org` (`None`: the instance log) older than `days` (0 = keep all), in
/// batches; returns the count. Each batch is its own write, so the writer is released between
/// batches.
async fn purge_scope(
    meta: &MetaDb,
    org: Option<OrgId>,
    days: u32,
    now: i64,
) -> Result<u64, MetaError> {
    if days == 0 {
        return Ok(0);
    }
    let cutoff = now.saturating_sub(i64::from(days).saturating_mul(DAY_NS));
    let org = org.map(|org| org.to_string());
    let mut removed = 0;
    loop {
        let org = org.clone();
        let deleted = meta
            .write(move |conn| {
                Box::pin(async move {
                    let result = sqlx::query(
                        "DELETE FROM audit_events WHERE seq IN \
                         (SELECT seq FROM audit_events WHERE org_id IS ? AND at < ? LIMIT ?)",
                    )
                    .bind(org.as_deref())
                    .bind(cutoff)
                    .bind(PURGE_BATCH)
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                    Ok(result.rows_affected())
                })
            })
            .await?;
        removed += deleted;
        // A batch that deleted fewer than PURGE_BATCH rows was the last one.
        if i64::try_from(deleted).is_ok_and(|deleted| deleted < PURGE_BATCH) {
            return Ok(removed);
        }
    }
}

/// Deletes every record of `org` (an applied org drop, D-EA-7). Records without an org
/// (the instance log) stay. Runs in the caller's transaction.
///
/// # Errors
///
/// The [`MetaError`] of the delete.
pub(crate) async fn delete_org_records(
    conn: &mut sqlx::SqliteConnection,
    org: uuid::Uuid,
) -> Result<u64, MetaError> {
    let result = sqlx::query("DELETE FROM audit_events WHERE org_id = ?")
        .bind(org.to_string())
        .execute(&mut *conn)
        .await
        .map_err(MetaError::from)?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::orgs::insert_org;
    use crate::test_support::{TestApp, set_limits};
    use serde_json::json;

    const DAY: i64 = DAY_NS;

    /// Inserts `count` records at `at`, for `org` (or none), in one transaction.
    async fn insert_rows(meta: &MetaDb, count: i64, at: i64, org: Option<uuid::Uuid>) {
        let org = org.map(|org| org.to_string());
        meta.write(move |conn| {
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
        .unwrap();
    }

    async fn count(meta: &MetaDb) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_events")
            .fetch_one(meta.reader())
            .await
            .unwrap()
    }

    async fn count_org(meta: &MetaDb, org: uuid::Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_events WHERE org_id = ?")
            .bind(org.to_string())
            .fetch_one(meta.reader())
            .await
            .unwrap()
    }

    /// An app whose instance log keeps `days` days (0 = forever).
    async fn app_with_instance_days(days: u32) -> TestApp {
        TestApp::with_config(move |config| config.audit.retention_days = days).await
    }

    /// Inserts an org row and returns its id, so the purge loop sees it.
    async fn new_org(app: &TestApp) -> uuid::Uuid {
        let org = OrgId::new_v7();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { insert_org(conn, org, "Acme", "acme", now).await })
            })
            .await
            .unwrap();
        org.as_uuid()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_keeps_recent_and_removes_old() {
        let app = app_with_instance_days(30).await;
        let meta = &app.state.meta;
        let now = 1000 * DAY;
        insert_rows(meta, 3, now - 31 * DAY, None).await;
        insert_rows(meta, 2, now - DAY, None).await;

        let removed = purge(&app.state, now).await.unwrap();

        assert_eq!(removed, 3);
        assert_eq!(count(meta).await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_with_zero_keeps_everything() {
        let app = app_with_instance_days(0).await;
        let meta = &app.state.meta;
        insert_rows(meta, 3, 0, None).await;

        let removed = purge(&app.state, 1000 * DAY).await.unwrap();

        assert_eq!(removed, 0);
        assert_eq!(count(meta).await, 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_spans_batches() {
        let app = app_with_instance_days(30).await;
        let meta = &app.state.meta;
        insert_rows(meta, 12_001, 0, None).await;

        let removed = purge(&app.state, 1000 * DAY).await.unwrap();

        assert_eq!(removed, 12_001);
        assert_eq!(count(meta).await, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_orgs_limit_overrides_the_instance_default() {
        // The instance keeps 30 days; the org's limit keeps 90.
        let app = app_with_instance_days(30).await;
        let meta = &app.state.meta;
        let org = new_org(&app).await;
        set_limits(
            &app,
            OrgId::from_uuid(org),
            "2025-12-01T00:00:00Z",
            &json!({"audit_retention_days": 90}),
        );
        let now = 1000 * DAY;
        insert_rows(meta, 2, now - 60 * DAY, Some(org)).await;
        insert_rows(meta, 3, now - 60 * DAY, None).await;

        let removed = purge(&app.state, now).await.unwrap();

        assert_eq!(removed, 3);
        assert_eq!(count_org(meta, org).await, 2);
        assert_eq!(count(meta).await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_limit_below_30_counts_as_30() {
        let app = app_with_instance_days(365).await;
        let meta = &app.state.meta;
        let org = new_org(&app).await;
        set_limits(
            &app,
            OrgId::from_uuid(org),
            "2025-12-01T00:00:00Z",
            &json!({"audit_retention_days": 7}),
        );
        let now = 1000 * DAY;
        insert_rows(meta, 1, now - 29 * DAY, Some(org)).await;
        insert_rows(meta, 1, now - 31 * DAY, Some(org)).await;

        let removed = purge(&app.state, now).await.unwrap();

        assert_eq!(removed, 1);
        assert_eq!(count_org(meta, org).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_null_limit_uses_the_instance_default() {
        // `null` sets no limit (plan P-4): the instance's 30 days apply to the org.
        let app = app_with_instance_days(30).await;
        let meta = &app.state.meta;
        let org = new_org(&app).await;
        set_limits(
            &app,
            OrgId::from_uuid(org),
            "2025-12-01T00:00:00Z",
            &json!({"audit_retention_days": null}),
        );
        let now = 1000 * DAY;
        insert_rows(meta, 1, now - 31 * DAY, Some(org)).await;
        insert_rows(meta, 1, now - 29 * DAY, Some(org)).await;

        let removed = purge(&app.state, now).await.unwrap();

        assert_eq!(removed, 1);
        assert_eq!(count_org(meta, org).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_orgs_zero_limit_keeps_its_records_forever() {
        let app = app_with_instance_days(30).await;
        let meta = &app.state.meta;
        let org = new_org(&app).await;
        set_limits(
            &app,
            OrgId::from_uuid(org),
            "2025-12-01T00:00:00Z",
            &json!({"audit_retention_days": 0}),
        );
        let now = 1000 * DAY;
        insert_rows(meta, 2, 0, Some(org)).await;

        let removed = purge(&app.state, now).await.unwrap();

        assert_eq!(removed, 0);
        assert_eq!(count_org(meta, org).await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_instance_log_uses_the_config() {
        let app = app_with_instance_days(30).await;
        let meta = &app.state.meta;
        let now = 1000 * DAY;
        insert_rows(meta, 1, now - 31 * DAY, None).await;
        insert_rows(meta, 1, now - 29 * DAY, None).await;

        let removed = purge(&app.state, now).await.unwrap();

        assert_eq!(removed, 1);
        assert_eq!(count(meta).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_org_records_leaves_other_orgs_and_the_instance_log() {
        let app = TestApp::new().await;
        let meta = &app.state.meta;
        let (org, other) = (uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        insert_rows(meta, 2, 1, Some(org)).await;
        insert_rows(meta, 1, 1, Some(other)).await;
        insert_rows(meta, 1, 1, None).await;

        let removed = meta
            .write(move |conn| Box::pin(async move { delete_org_records(conn, org).await }))
            .await
            .unwrap();

        assert_eq!(removed, 2);
        assert_eq!(count_org(meta, org).await, 0);
        assert_eq!(count_org(meta, other).await, 1);
        assert_eq!(count(meta).await, 2);
    }
}
