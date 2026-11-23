//! Retention of audit records (D-EA-7): the hourly purge of records older than
//! `audit.retention_days`, and the deletion of an org's records when its drop is applied.

use crate::meta::{MetaDb, MetaError};

/// Rows deleted per statement, so the writer is never held long.
pub(crate) const PURGE_BATCH: i64 = 5_000;

const DAY_NS: i64 = 86_400 * 1_000_000_000;

/// Deletes records older than `retention_days` (0 = keep all), in batches; returns the count.
/// Each batch is its own write, so the writer is released between batches.
///
/// # Errors
///
/// The [`MetaError`] of the first batch that fails; the batches before it stay deleted.
pub(crate) async fn purge(meta: &MetaDb, retention_days: u32, now: i64) -> Result<u64, MetaError> {
    if retention_days == 0 {
        return Ok(0);
    }
    let cutoff = now.saturating_sub(i64::from(retention_days).saturating_mul(DAY_NS));
    let mut removed = 0;
    loop {
        let deleted = meta
            .write(move |conn| {
                Box::pin(async move {
                    let result = sqlx::query(
                        "DELETE FROM audit_events WHERE seq IN \
                         (SELECT seq FROM audit_events WHERE at < ? LIMIT ?)",
                    )
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
    use crate::test_support::TestApp;

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

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_keeps_recent_and_removes_old() {
        let app = TestApp::new().await;
        let meta = &app.state.meta;
        let now = 1000 * DAY;
        insert_rows(meta, 3, now - 31 * DAY, None).await;
        insert_rows(meta, 2, now - DAY, None).await;

        let removed = purge(meta, 30, now).await.unwrap();

        assert_eq!(removed, 3);
        assert_eq!(count(meta).await, 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_with_zero_keeps_everything() {
        let app = TestApp::new().await;
        let meta = &app.state.meta;
        insert_rows(meta, 3, 0, None).await;

        let removed = purge(meta, 0, 1000 * DAY).await.unwrap();

        assert_eq!(removed, 0);
        assert_eq!(count(meta).await, 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_spans_batches() {
        let app = TestApp::new().await;
        let meta = &app.state.meta;
        insert_rows(meta, 12_001, 0, None).await;

        let removed = purge(meta, 30, 1000 * DAY).await.unwrap();

        assert_eq!(removed, 12_001);
        assert_eq!(count(meta).await, 0);
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
