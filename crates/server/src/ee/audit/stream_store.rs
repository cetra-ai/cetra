// SPDX-License-Identifier: Elastic-2.0
//! The audit stream's destinations (track EA M4, D-EA-11): one HTTPS destination per org, and one
//! for the instance (`org_id` null). The row keeps the URL host and the bookkeeping in clear; the
//! URL, the signing secret and the optional header are sealed in `config_enc` (D-I-2), bound to
//! the row id.
#![cfg_attr(not(test), allow(dead_code))] // the delivery task and the API use it from E2 and E3 on

use std::fmt;

use cetra_core::ids::OrgId;
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use url::Url;
use uuid::Uuid;

use crate::crypto::{Aad, CryptoError, SecretBox};
use crate::meta::{MetaError, unique_or};

/// AAD table and column of a destination's sealed config.
pub(crate) const TABLE: &str = "audit_stream_destinations";
pub(crate) const COLUMN: &str = "config_enc";

/// The sealed part of a destination. `Debug` prints the host only.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StreamConfig {
    /// The endpoint, as given. Never logged or returned.
    pub(crate) url: String,
    /// The signing secret, as cetra generated it. Never returned.
    pub(crate) secret: String,
    /// The optional header: name and value. The value is never returned.
    pub(crate) header: Option<(String, String)>,
}

impl fmt::Debug for StreamConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = Url::parse(&self.url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned))
            .unwrap_or_default();
        formatter
            .debug_struct("StreamConfig")
            .field("url", &host)
            .finish_non_exhaustive()
    }
}

/// One `audit_stream_destinations` row. It holds only the sealed blob, never the config text.
#[derive(Debug)]
pub(crate) struct DestinationRow {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// The org, or `None` for the instance destination.
    pub(crate) org_id: Option<OrgId>,
    /// The endpoint's host, for status pages.
    pub(crate) url_host: String,
    /// The sealed [`StreamConfig`].
    pub(crate) config_enc: Vec<u8>,
    pub(crate) enabled: bool,
    /// The last `audit_events.seq` the destination has accepted.
    pub(crate) cursor_seq: i64,
    /// Failed attempts since the last success.
    pub(crate) attempts: i64,
    /// Nanoseconds; the task skips the destination until then.
    pub(crate) next_attempt_at: i64,
    pub(crate) last_success_at: Option<i64>,
    pub(crate) last_error: Option<String>,
    /// When the current run of failures began.
    pub(crate) failing_since: Option<i64>,
    /// When the owners (or instance admins) were emailed about the current run of failures.
    pub(crate) alerted_at: Option<i64>,
    /// Records above this `seq` were deleted by retention before they were sent (P-5).
    pub(crate) lost_before_seq: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// Why a stored destination config could not be read. No variant holds config text.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    /// The value was sealed with another key, or is corrupt.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// The plaintext is not a `StreamConfig`.
    #[error("the stream config is not valid JSON")]
    Json,
}

/// Seals `config` for the destination row `id`.
pub(crate) fn seal(
    secrets: &SecretBox,
    id: &Uuid,
    config: &StreamConfig,
) -> Result<Vec<u8>, CryptoError> {
    // Serializing a StreamConfig cannot fail: it holds only strings and an optional string pair.
    #[allow(clippy::expect_used)]
    let json = serde_json::to_vec(config).expect("stream config serializes");
    secrets.seal(
        &json,
        Aad {
            table: TABLE,
            column: COLUMN,
            row_id: id,
        },
    )
}

/// Opens the sealed config of the destination row `id`.
pub(crate) fn open(
    secrets: &SecretBox,
    id: &Uuid,
    sealed: &[u8],
) -> Result<StreamConfig, OpenError> {
    let json = secrets.open(
        sealed,
        Aad {
            table: TABLE,
            column: COLUMN,
            row_id: id,
        },
    )?;
    serde_json::from_slice(&json).map_err(|_| OpenError::Json)
}

const COLUMNS: &str = "id, org_id, url_host, config_enc, enabled, cursor_seq, attempts, \
    next_attempt_at, last_success_at, last_error, failing_since, alerted_at, lost_before_seq, \
    created_at, updated_at";

/// `SELECT` of every column where `filter` holds.
fn select(filter: &str) -> String {
    format!("SELECT {COLUMNS} FROM {TABLE} WHERE {filter}")
}

#[derive(Debug, sqlx::FromRow)]
struct DestinationRecord {
    id: String,
    org_id: Option<String>,
    url_host: String,
    config_enc: Vec<u8>,
    enabled: i64,
    cursor_seq: i64,
    attempts: i64,
    next_attempt_at: i64,
    last_success_at: Option<i64>,
    last_error: Option<String>,
    failing_since: Option<i64>,
    alerted_at: Option<i64>,
    lost_before_seq: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

fn corrupt(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: TABLE,
        column,
    }
}

fn row_from(record: DestinationRecord) -> Result<DestinationRow, MetaError> {
    let id: Uuid = record.id.parse().map_err(|_| corrupt("id"))?;
    let org_id = match record.org_id {
        Some(text) => {
            let uuid: Uuid = text.parse().map_err(|_| corrupt("org_id"))?;
            Some(OrgId::from_uuid(uuid))
        }
        None => None,
    };
    Ok(DestinationRow {
        id,
        org_id,
        url_host: record.url_host,
        config_enc: record.config_enc,
        enabled: record.enabled != 0,
        cursor_seq: record.cursor_seq,
        attempts: record.attempts,
        next_attempt_at: record.next_attempt_at,
        last_success_at: record.last_success_at,
        last_error: record.last_error,
        failing_since: record.failing_since,
        alerted_at: record.alerted_at,
        lost_before_seq: record.lost_before_seq,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

/// The org's text, or NULL for the instance. The scope is `org_id IS ?`: with a bound text it is
/// `org_id = ?`, and with a bound NULL it is `org_id IS NULL`.
fn scope_of(org: Option<OrgId>) -> Option<String> {
    org.map(|org| org.to_string())
}

/// The destination of `org` (`None`: the instance), on the reader pool.
pub(crate) async fn destination(
    pool: &SqlitePool,
    org: Option<OrgId>,
) -> Result<Option<DestinationRow>, MetaError> {
    let sql = select("org_id IS ?");
    let record: Option<DestinationRecord> = sqlx::query_as(&sql)
        .bind(scope_of(org))
        .fetch_optional(pool)
        .await?;
    record.map(row_from).transpose()
}

/// [`destination`] in the caller's transaction.
pub(crate) async fn destination_tx(
    conn: &mut SqliteConnection,
    org: Option<OrgId>,
) -> Result<Option<DestinationRow>, MetaError> {
    let sql = select("org_id IS ?");
    let record: Option<DestinationRecord> = sqlx::query_as(&sql)
        .bind(scope_of(org))
        .fetch_optional(&mut *conn)
        .await?;
    record.map(row_from).transpose()
}

/// Creates the destination of `org` (`None`: the instance), enabled. Its cursor starts at the
/// table's current highest `seq` (0 when empty): history is not replayed (the export covers it).
///
/// # Errors
/// [`MetaError::Conflict`] when the scope already has a destination.
pub(crate) async fn insert(
    conn: &mut SqliteConnection,
    id: Uuid,
    org: Option<OrgId>,
    url_host: &str,
    sealed: &[u8],
    now: i64,
) -> Result<DestinationRow, MetaError> {
    let cursor_seq: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM audit_events")
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO audit_stream_destinations \
         (id, org_id, url_host, config_enc, enabled, cursor_seq, attempts, next_attempt_at, \
         created_at, updated_at) \
         VALUES (?, ?, ?, ?, 1, ?, 0, 0, ?, ?)",
    )
    .bind(id.to_string())
    .bind(scope_of(org))
    .bind(url_host)
    .bind(sealed)
    .bind(cursor_seq)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| {
        unique_or(
            error,
            "an audit stream destination already exists for this scope",
        )
    })?;
    Ok(DestinationRow {
        id,
        org_id: org,
        url_host: url_host.to_owned(),
        config_enc: sealed.to_vec(),
        enabled: true,
        cursor_seq,
        attempts: 0,
        next_attempt_at: 0,
        last_success_at: None,
        last_error: None,
        failing_since: None,
        alerted_at: None,
        lost_before_seq: None,
        created_at: now,
        updated_at: now,
    })
}

/// Replaces the config of destination `id` and clears its failure state, so the next pass sends
/// at once. `updated_at` moves past the value the task may have read (plan P-6), even within one
/// clock tick. The cursor is kept.
pub(crate) async fn update_config(
    conn: &mut SqliteConnection,
    id: Uuid,
    url_host: &str,
    sealed: &[u8],
    enabled: bool,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE audit_stream_destinations SET url_host = ?, config_enc = ?, enabled = ?, \
         attempts = 0, next_attempt_at = 0, last_error = NULL, failing_since = NULL, \
         alerted_at = NULL, lost_before_seq = NULL, updated_at = max(?, updated_at + 1) \
         WHERE id = ?",
    )
    .bind(url_host)
    .bind(sealed)
    .bind(enabled)
    .bind(now)
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Removes the destination of `org` (`None`: the instance). `false` when there was none.
pub(crate) async fn delete(
    conn: &mut SqliteConnection,
    org: Option<OrgId>,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM audit_stream_destinations WHERE org_id IS ?")
        .bind(scope_of(org))
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Enabled destinations whose next attempt is due.
pub(crate) async fn due(pool: &SqlitePool, now: i64) -> Result<Vec<DestinationRow>, MetaError> {
    let sql = select("enabled = 1 AND next_attempt_at <= ?");
    let records: Vec<DestinationRecord> = sqlx::query_as(&sql).bind(now).fetch_all(pool).await?;
    records.into_iter().map(row_from).collect()
}

/// Records a 2xx: the cursor moves to `cursor_seq` and the failure state clears. Updates only
/// the row the task read (`updated_at` = `read_updated_at`, plan P-6); `false` when it changed.
pub(crate) async fn mark_sent(
    conn: &mut SqliteConnection,
    id: Uuid,
    read_updated_at: i64,
    cursor_seq: i64,
    now: i64,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "UPDATE audit_stream_destinations SET cursor_seq = ?, attempts = 0, next_attempt_at = 0, \
         last_success_at = ?, last_error = NULL, failing_since = NULL, alerted_at = NULL \
         WHERE id = ? AND updated_at = ?",
    )
    .bind(cursor_seq)
    .bind(now)
    .bind(id.to_string())
    .bind(read_updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Records a failed attempt. The first failure of a run sets `failing_since`; later ones keep it.
/// Updates only the row the task read (plan P-6).
///
/// Returns the `failing_since` now stored, or `None` when the row changed meanwhile.
pub(crate) async fn mark_failed(
    conn: &mut SqliteConnection,
    id: Uuid,
    read_updated_at: i64,
    attempts: i64,
    next_attempt_at: i64,
    error: &str,
    now: i64,
) -> Result<Option<i64>, MetaError> {
    let failing_since: Option<i64> = sqlx::query_scalar(
        "UPDATE audit_stream_destinations SET attempts = ?, next_attempt_at = ?, last_error = ?, \
         failing_since = COALESCE(failing_since, ?) \
         WHERE id = ? AND updated_at = ? \
         RETURNING failing_since",
    )
    .bind(attempts)
    .bind(next_attempt_at)
    .bind(error)
    .bind(now)
    .bind(id.to_string())
    .bind(read_updated_at)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(failing_since)
}

/// Records that the owners (or instance admins) were emailed about this run of failures.
/// Updates only the row the task read (plan P-6); `false` when it changed.
pub(crate) async fn mark_alerted(
    conn: &mut SqliteConnection,
    id: Uuid,
    read_updated_at: i64,
    now: i64,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "UPDATE audit_stream_destinations SET alerted_at = ? WHERE id = ? AND updated_at = ?",
    )
    .bind(now)
    .bind(id.to_string())
    .bind(read_updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Retention deleted records up to `max_deleted_seq` in `org`'s scope (`None`: the instance).
/// A destination whose cursor is below it has lost undelivered records, so its `lost_before_seq`
/// is set (plan P-5). Other destinations are untouched.
pub(crate) async fn flag_lost(
    conn: &mut SqliteConnection,
    org: Option<OrgId>,
    max_deleted_seq: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE audit_stream_destinations \
         SET lost_before_seq = max(coalesce(lost_before_seq, 0), ?) \
         WHERE org_id IS ? AND cursor_seq < ?",
    )
    .bind(max_deleted_seq)
    .bind(scope_of(org))
    .bind(max_deleted_seq)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::OrgId;
    use uuid::Uuid;

    use super::{
        DestinationRow, OpenError, StreamConfig, delete, destination, flag_lost, insert, mark_alerted,
        mark_failed, mark_sent, open, seal, update_config,
    };
    use crate::audit::catalog::AuthAction;
    use crate::audit::{Ctx, Event, record_alone};
    use crate::crypto::SecretBox;
    use crate::meta::orgs::insert_org;
    use crate::meta::{MetaDb, MetaError};

    fn config(url: &str) -> StreamConfig {
        StreamConfig {
            url: url.to_owned(),
            secret: "signing-secret-value".to_owned(),
            header: None,
        }
    }

    fn secrets() -> SecretBox {
        SecretBox::new(&[7; 32])
    }

    async fn seed_org(meta: &MetaDb) -> OrgId {
        meta.write(|conn| {
            Box::pin(async move { Ok(insert_org(conn, OrgId::new_v7(), "Acme", "acme", 1).await?.id) })
        })
        .await
        .expect("the org is written")
    }

    /// A destination of `org` (`None`: the instance) at `now`, with a sealed config for `id`.
    async fn insert_destination(
        meta: &MetaDb,
        org: Option<OrgId>,
        id: Uuid,
        now: i64,
    ) -> Result<DestinationRow, MetaError> {
        let sealed = seal(
            &secrets(),
            &id,
            &config("https://siem.example.com/ingest"),
        )
        .expect("seals");
        meta.write(move |conn| {
            Box::pin(async move {
                insert(conn, id, org, "siem.example.com", &sealed, now).await
            })
        })
        .await
    }

    async fn read(meta: &MetaDb, org: Option<OrgId>) -> DestinationRow {
        destination(meta.reader(), org)
            .await
            .expect("read")
            .expect("the destination exists")
    }

    #[test]
    fn seal_and_open_round_trip() {
        let id = Uuid::now_v7();
        let stored = StreamConfig {
            url: "https://siem.example.com/ingest?route=audit".to_owned(),
            secret: "secret-value".to_owned(),
            header: Some(("Authorization".to_owned(), "Splunk token-value".to_owned())),
        };
        let sealed = seal(&secrets(), &id, &stored).expect("seals");
        assert_eq!(open(&secrets(), &id, &sealed).expect("opens"), stored);

        // The row id is bound: another row cannot open it.
        assert!(matches!(
            open(&secrets(), &Uuid::now_v7(), &sealed),
            Err(OpenError::Crypto(_))
        ));
    }

    #[test]
    fn debug_hides_the_url_path_and_secrets() {
        let stored = StreamConfig {
            url: "https://siem.example.com/path-marker?token=query-marker".to_owned(),
            secret: "secret-marker".to_owned(),
            header: Some(("Authorization".to_owned(), "header-marker".to_owned())),
        };
        assert_eq!(
            format!("{stored:?}"),
            r#"StreamConfig { url: "siem.example.com", .. }"#
        );
    }

    #[tokio::test]
    async fn one_destination_per_scope() {
        let (_dir, meta) = crate::meta::test_db().await;
        let org = seed_org(&meta).await;
        insert_destination(&meta, Some(org), Uuid::now_v7(), 1)
            .await
            .expect("the org's first destination");
        let second = insert_destination(&meta, Some(org), Uuid::now_v7(), 2).await;
        assert!(matches!(second, Err(MetaError::Conflict(_))));

        insert_destination(&meta, None, Uuid::now_v7(), 3)
            .await
            .expect("the instance's first destination");
        let second = insert_destination(&meta, None, Uuid::now_v7(), 4).await;
        assert!(matches!(second, Err(MetaError::Conflict(_))));
    }

    #[tokio::test]
    async fn insert_starts_at_the_global_max_seq() {
        let (_dir, meta) = crate::meta::test_db().await;
        let org = seed_org(&meta).await;

        // No records yet: the cursor starts at 0.
        let instance = insert_destination(&meta, None, Uuid::now_v7(), 1)
            .await
            .expect("inserted");
        assert_eq!(instance.cursor_seq, 0);

        for at in 1..=3 {
            record_alone(&meta, &Ctx::system(), Event::new(AuthAction::Logout), at)
                .await
                .expect("written");
        }
        let row = insert_destination(&meta, Some(org), Uuid::now_v7(), 4)
            .await
            .expect("inserted");
        assert_eq!(row.cursor_seq, 3);
        assert_eq!(read(&meta, Some(org)).await.cursor_seq, 3);
    }

    #[tokio::test]
    async fn marks_are_ignored_after_an_edit() {
        let (_dir, meta) = crate::meta::test_db().await;
        let org = seed_org(&meta).await;
        let id = Uuid::now_v7();
        insert_destination(&meta, Some(org), id, 10)
            .await
            .expect("inserted");
        let read_updated_at = read(&meta, Some(org)).await.updated_at;

        // An edit in the same clock tick still moves `updated_at`.
        let sealed = seal(&secrets(), &id, &config("https://siem.example.com/other")).expect("seals");
        meta.write(move |conn| {
            Box::pin(async move {
                update_config(conn, id, "siem.example.com", &sealed, true, 10).await
            })
        })
        .await
        .expect("edited");

        let sent = meta
            .write(move |conn| {
                Box::pin(async move { mark_sent(conn, id, read_updated_at, 5, 11).await })
            })
            .await
            .expect("ran");
        assert!(!sent);
        let failing = meta
            .write(move |conn| {
                Box::pin(async move {
                    mark_failed(conn, id, read_updated_at, 1, 100, "the endpoint answered 500", 11)
                        .await
                })
            })
            .await
            .expect("ran");
        assert_eq!(failing, None);
        let stored = read(&meta, Some(org)).await;
        assert_eq!(stored.cursor_seq, 0);
        assert_eq!(stored.attempts, 0);
        assert_eq!(stored.failing_since, None);
    }

    #[tokio::test]
    async fn mark_failed_keeps_the_first_failing_since() {
        let (_dir, meta) = crate::meta::test_db().await;
        let org = seed_org(&meta).await;
        let id = Uuid::now_v7();
        insert_destination(&meta, Some(org), id, 1)
            .await
            .expect("inserted");
        let read_updated_at = read(&meta, Some(org)).await.updated_at;

        let first = meta
            .write(move |conn| {
                Box::pin(async move {
                    mark_failed(conn, id, read_updated_at, 1, 100, "the endpoint answered 500", 50)
                        .await
                })
            })
            .await
            .expect("ran");
        assert_eq!(first, Some(50));

        let second = meta
            .write(move |conn| {
                Box::pin(async move {
                    mark_failed(conn, id, read_updated_at, 2, 200, "the endpoint answered 503", 80)
                        .await
                })
            })
            .await
            .expect("ran");
        assert_eq!(second, Some(50));

        let stored = read(&meta, Some(org)).await;
        assert_eq!(stored.attempts, 2);
        assert_eq!(stored.next_attempt_at, 200);
        assert_eq!(stored.last_error.as_deref(), Some("the endpoint answered 503"));
        assert_eq!(stored.failing_since, Some(50));
    }

    #[tokio::test]
    async fn mark_sent_clears_the_failure_state() {
        let (_dir, meta) = crate::meta::test_db().await;
        let org = seed_org(&meta).await;
        let id = Uuid::now_v7();
        insert_destination(&meta, Some(org), id, 1)
            .await
            .expect("inserted");
        let read_updated_at = read(&meta, Some(org)).await.updated_at;

        meta.write(move |conn| {
            Box::pin(async move {
                mark_failed(conn, id, read_updated_at, 3, 99, "the endpoint answered 500", 5).await?;
                mark_alerted(conn, id, read_updated_at, 6).await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .expect("ran");
        let failing = read(&meta, Some(org)).await;
        assert_eq!(failing.alerted_at, Some(6));

        let sent = meta
            .write(move |conn| {
                Box::pin(async move { mark_sent(conn, id, read_updated_at, 7, 9).await })
            })
            .await
            .expect("ran");
        assert!(sent);

        let stored = read(&meta, Some(org)).await;
        assert_eq!(stored.cursor_seq, 7);
        assert_eq!(stored.attempts, 0);
        assert_eq!(stored.next_attempt_at, 0);
        assert_eq!(stored.last_success_at, Some(9));
        assert_eq!(stored.last_error, None);
        assert_eq!(stored.failing_since, None);
        assert_eq!(stored.alerted_at, None);
    }

    #[tokio::test]
    async fn flag_lost_only_when_undelivered_records_were_deleted() {
        let (_dir, meta) = crate::meta::test_db().await;
        let org = seed_org(&meta).await;
        let id = Uuid::now_v7();
        insert_destination(&meta, Some(org), id, 1)
            .await
            .expect("inserted");
        let read_updated_at = read(&meta, Some(org)).await.updated_at;
        meta.write(move |conn| {
            Box::pin(async move { mark_sent(conn, id, read_updated_at, 5, 2).await })
        })
        .await
        .expect("sent");
        let instance_id = Uuid::now_v7();
        insert_destination(&meta, None, instance_id, 3)
            .await
            .expect("inserted");

        // Records up to 5 were deleted: all of them were delivered (cursor 5).
        meta.write(|conn| Box::pin(async move { flag_lost(conn, Some(org), 5).await }))
            .await
            .expect("flagged");
        assert_eq!(read(&meta, Some(org)).await.lost_before_seq, None);

        // Record 6 was deleted before it was sent: the org's destination is flagged.
        meta.write(|conn| Box::pin(async move { flag_lost(conn, Some(org), 6).await }))
            .await
            .expect("flagged");
        assert_eq!(read(&meta, Some(org)).await.lost_before_seq, Some(6));
        // The instance's records were not deleted.
        assert_eq!(read(&meta, None).await.lost_before_seq, None);
    }

    #[tokio::test]
    async fn org_delete_cascades() {
        let (_dir, meta) = crate::meta::test_db().await;
        let org = seed_org(&meta).await;
        insert_destination(&meta, Some(org), Uuid::now_v7(), 1)
            .await
            .expect("inserted");
        meta.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM orgs WHERE id = ?")
                    .bind(org.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .expect("the org is deleted");
        assert!(
            destination(meta.reader(), Some(org))
                .await
                .expect("read")
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_reports_whether_a_destination_existed() {
        let (_dir, meta) = crate::meta::test_db().await;
        insert_destination(&meta, None, Uuid::now_v7(), 1)
            .await
            .expect("inserted");
        let removed = meta
            .write(|conn| Box::pin(async move { delete(conn, None).await }))
            .await
            .expect("deleted");
        assert!(removed);
        let removed_again = meta
            .write(|conn| Box::pin(async move { delete(conn, None).await }))
            .await
            .expect("deleted");
        assert!(!removed_again);
    }
}
