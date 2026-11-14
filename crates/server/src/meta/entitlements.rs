//! `org_entitlements` and `org_window_ends` (D-P-11, D-P-30).

use std::fmt;

use llmobs_core::ids::OrgId;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;

/// Window ends older than this, relative to the newest one, are pruned (D-P-30).
const WINDOW_RETENTION_NS: i64 = 30 * 86_400 * 1_000_000_000;

/// A stored org token. `Debug` is written by hand and omits the token.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StoredToken {
    /// Owning org.
    pub(crate) org_id: OrgId,
    /// The token as received. Never logged.
    pub(crate) token: String,
    /// The token's `jti`.
    pub(crate) token_id: Uuid,
    /// The token's issue time, nanoseconds.
    pub(crate) issued_at: i64,
    /// When this cell accepted the token, nanoseconds.
    pub(crate) received_at: i64,
}

impl fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredToken")
            .field("org_id", &self.org_id)
            .field("token", &format_args!("<{} bytes>", self.token.len()))
            .field("token_id", &self.token_id)
            .field("issued_at", &self.issued_at)
            .field("received_at", &self.received_at)
            .finish()
    }
}

#[derive(Debug, sqlx::FromRow)]
struct TokenRecord {
    org_id: String,
    token: String,
    token_id: String,
    issued_at: i64,
    received_at: i64,
}

/// Every stored token, ordered by org id. Used at startup.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a row does not decode.
pub(crate) async fn all_tokens(pool: &SqlitePool) -> Result<Vec<StoredToken>, MetaError> {
    let records: Vec<TokenRecord> = sqlx::query_as(
        "SELECT org_id, token, token_id, issued_at, received_at \
         FROM org_entitlements ORDER BY org_id",
    )
    .fetch_all(pool)
    .await?;
    records.into_iter().map(token_row).collect()
}

/// The org's stored token, in the write transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or the row does not decode.
pub(crate) async fn token_of_tx(
    conn: &mut SqliteConnection,
    org_id: OrgId,
) -> Result<Option<StoredToken>, MetaError> {
    let record: Option<TokenRecord> = sqlx::query_as(
        "SELECT org_id, token, token_id, issued_at, received_at \
         FROM org_entitlements WHERE org_id = ?",
    )
    .bind(org_id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    record.map(token_row).transpose()
}

/// Inserts or replaces the org's token.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn put_token(
    conn: &mut SqliteConnection,
    token: &StoredToken,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO org_entitlements (org_id, token, token_id, issued_at, received_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (org_id) DO UPDATE SET \
         token = excluded.token, token_id = excluded.token_id, \
         issued_at = excluded.issued_at, received_at = excluded.received_at",
    )
    .bind(token.org_id.to_string())
    .bind(&token.token)
    .bind(token.token_id.to_string())
    .bind(token.issued_at)
    .bind(token.received_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Records that `days` (`None` = unlimited) stopped applying to the org at `ended_at`, and
/// deletes the org's rows that ended before `ended_at` minus 30 days. A row with the same
/// `(org_id, ended_at)` is replaced.
///
/// # Errors
///
/// Returns [`MetaError`] when the write fails.
pub(crate) async fn record_window_end(
    conn: &mut SqliteConnection,
    org_id: OrgId,
    ended_at: i64,
    days: Option<u64>,
) -> Result<(), MetaError> {
    let days = days.map(|days| i64::try_from(days).unwrap_or(i64::MAX));
    sqlx::query(
        "INSERT OR REPLACE INTO org_window_ends (org_id, ended_at, data_access_days) \
         VALUES (?, ?, ?)",
    )
    .bind(org_id.to_string())
    .bind(ended_at)
    .bind(days)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM org_window_ends WHERE org_id = ? AND ended_at < ?")
        .bind(org_id.to_string())
        .bind(ended_at.saturating_sub(WINDOW_RETENTION_NS))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Windows of the org that ended at or after `since`, newest first. `None` days = unlimited.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a row does not decode.
pub(crate) async fn window_ends_since(
    pool: &SqlitePool,
    org_id: OrgId,
    since: i64,
) -> Result<Vec<(i64, Option<u64>)>, MetaError> {
    let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT ended_at, data_access_days FROM org_window_ends \
         WHERE org_id = ? AND ended_at >= ? ORDER BY ended_at DESC",
    )
    .bind(org_id.to_string())
    .bind(since)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(ended_at, days)| {
            let days = days
                .map(|days| {
                    u64::try_from(days).map_err(|_| MetaError::Corrupt {
                        table: "org_window_ends",
                        column: "data_access_days",
                    })
                })
                .transpose()?;
            Ok((ended_at, days))
        })
        .collect()
}

fn token_row(record: TokenRecord) -> Result<StoredToken, MetaError> {
    let org_id = record.org_id.parse().map_err(|_| MetaError::Corrupt {
        table: "org_entitlements",
        column: "org_id",
    })?;
    let token_id = Uuid::parse_str(&record.token_id).map_err(|_| MetaError::Corrupt {
        table: "org_entitlements",
        column: "token_id",
    })?;
    Ok(StoredToken {
        org_id,
        token: record.token,
        token_id,
        issued_at: record.issued_at,
        received_at: record.received_at,
    })
}

#[cfg(test)]
mod tests {
    use llmobs_core::ids::OrgId;
    use uuid::Uuid;

    use super::{
        StoredToken, all_tokens, put_token, record_window_end, token_of_tx, window_ends_since,
    };
    use crate::meta::MetaDb;
    use crate::meta::orgs::insert_org;

    const DAY: i64 = 86_400 * 1_000_000_000;

    async fn org(db: &MetaDb, slug: &str) -> OrgId {
        let slug = slug.to_owned();
        db.write(move |conn| Box::pin(async move { insert_org(conn, "Acme", &slug, 0).await }))
            .await
            .unwrap()
            .id
    }

    fn token(org_id: OrgId, token: &str, received_at: i64) -> StoredToken {
        StoredToken {
            org_id,
            token: token.to_owned(),
            token_id: Uuid::now_v7(),
            issued_at: received_at - 1,
            received_at,
        }
    }

    #[tokio::test]
    async fn put_and_read_back() {
        let (_dir, db) = crate::meta::test_db().await;
        let acme = org(&db, "acme").await;
        let first = token(acme, "first-token", 10);
        let stored = first.clone();
        db.write(move |conn| Box::pin(async move { put_token(conn, &stored).await }))
            .await
            .unwrap();

        let read = db
            .write(move |conn| Box::pin(async move { token_of_tx(conn, acme).await }))
            .await
            .unwrap();
        assert_eq!(read, Some(first.clone()));
        assert_eq!(all_tokens(db.reader()).await.unwrap(), vec![first]);

        let second = token(acme, "second-token", 20);
        let stored = second.clone();
        db.write(move |conn| Box::pin(async move { put_token(conn, &stored).await }))
            .await
            .unwrap();
        assert_eq!(all_tokens(db.reader()).await.unwrap(), vec![second]);
    }

    #[tokio::test]
    async fn cascade_on_org_delete() {
        let (_dir, db) = crate::meta::test_db().await;
        let acme = org(&db, "acme").await;
        let stored = token(acme, "token", 10);
        db.write(move |conn| {
            Box::pin(async move {
                put_token(conn, &stored).await?;
                record_window_end(conn, acme, 10, Some(30)).await
            })
        })
        .await
        .unwrap();

        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM orgs WHERE id = ?")
                    .bind(acme.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_entitlements")
            .fetch_one(db.reader())
            .await
            .unwrap();
        let windows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_window_ends")
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(tokens, 0);
        assert_eq!(windows, 0);
    }

    #[tokio::test]
    async fn window_ends_prune_after_30_days() {
        let (_dir, db) = crate::meta::test_db().await;
        let acme = org(&db, "acme").await;
        db.write(move |conn| {
            Box::pin(async move {
                record_window_end(conn, acme, 0, Some(30)).await?;
                record_window_end(conn, acme, 20 * DAY, None).await?;
                record_window_end(conn, acme, 45 * DAY, Some(120)).await
            })
        })
        .await
        .unwrap();

        let recent = window_ends_since(db.reader(), acme, 45 * DAY - 30 * DAY)
            .await
            .unwrap();
        assert_eq!(recent, vec![(45 * DAY, Some(120)), (20 * DAY, None)]);

        let all = window_ends_since(db.reader(), acme, 0).await.unwrap();
        assert_eq!(all.len(), 2, "the day-0 row was pruned");
    }

    #[tokio::test]
    async fn orgs_limits_is_gone() {
        let (_dir, db) = crate::meta::test_db().await;
        let result = sqlx::query("SELECT limits FROM orgs")
            .fetch_all(db.reader())
            .await;
        assert!(result.is_err());
    }
}
