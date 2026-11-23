//! `instance_settings` rows (spec 04): one text value per key.

use sqlx::{SqliteConnection, SqlitePool};

use super::MetaError;

/// Key of the encryption check value (D-I-2).
pub(crate) const ENCRYPTION_CHECK: &str = "encryption.check";

/// Key of the licence key an instance admin installed (D-P-75). A signed token, stored in plain text.
pub(crate) const LICENSE_KEY: &str = "license.key";

/// Key of this instance's random id for the heartbeat and crash reports (D-TL-9). A UUIDv4.
pub(crate) const TELEMETRY_INSTANCE_ID: &str = "telemetry.instance_id";

/// Key of the time of the last heartbeat the relay accepted (D-TL-8): i64 nanoseconds since
/// the Unix epoch, as decimal text.
pub(crate) const TELEMETRY_LAST_HEARTBEAT_AT: &str = "telemetry.last_heartbeat_at";

/// Key of the feature counts not yet sent in a heartbeat (track TL M6): a JSON object of
/// catalog event name to count.
pub(crate) const TELEMETRY_USAGE_COUNTS: &str = "telemetry.usage_counts";

/// The value of `key`, or `None`.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the query fails.
pub(crate) async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, MetaError> {
    sqlx::query_scalar::<_, String>("SELECT value FROM instance_settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
        .map_err(MetaError::from)
}

/// The same read on the writer, inside a transaction.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the query fails.
pub(crate) async fn get_setting_tx(
    conn: &mut SqliteConnection,
    key: &str,
) -> Result<Option<String>, MetaError> {
    sqlx::query_scalar::<_, String>("SELECT value FROM instance_settings WHERE key = ?")
        .bind(key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(MetaError::from)
}

/// Inserts or replaces `key`.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the write fails.
pub(crate) async fn put_setting(
    conn: &mut SqliteConnection,
    key: &str,
    value: &str,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO instance_settings (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Inserts `key` with `value` unless the key exists; an existing value is kept (D-TL-9).
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the write fails.
pub(crate) async fn insert_setting_if_absent(
    conn: &mut SqliteConnection,
    key: &str,
    value: &str,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO instance_settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO NOTHING",
    )
    .bind(key)
    .bind(value)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_on_an_empty_table_is_none() {
        let (_dir, db) = crate::meta::test_db().await;
        assert_eq!(
            get_setting(db.reader(), ENCRYPTION_CHECK).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let (_dir, db) = crate::meta::test_db().await;
        db.write(|conn| Box::pin(async move { put_setting(conn, ENCRYPTION_CHECK, "abc").await }))
            .await
            .unwrap();
        assert_eq!(
            get_setting(db.reader(), ENCRYPTION_CHECK).await.unwrap(),
            Some("abc".to_owned())
        );
    }

    #[tokio::test]
    async fn put_twice_keeps_the_second_value() {
        let (_dir, db) = crate::meta::test_db().await;
        db.write(|conn| {
            Box::pin(async move {
                put_setting(conn, "k", "first").await?;
                put_setting(conn, "k", "second").await
            })
        })
        .await
        .unwrap();
        assert_eq!(
            get_setting(db.reader(), "k").await.unwrap(),
            Some("second".to_owned())
        );
    }

    #[tokio::test]
    async fn insert_if_absent_writes_a_new_key() {
        let (_dir, db) = crate::meta::test_db().await;
        db.write(|conn| {
            Box::pin(async move { insert_setting_if_absent(conn, "k", "first").await })
        })
        .await
        .unwrap();
        assert_eq!(
            get_setting(db.reader(), "k").await.unwrap(),
            Some("first".to_owned())
        );
    }

    #[tokio::test]
    async fn insert_if_absent_keeps_an_existing_value() {
        let (_dir, db) = crate::meta::test_db().await;
        db.write(|conn| {
            Box::pin(async move {
                put_setting(conn, "k", "first").await?;
                insert_setting_if_absent(conn, "k", "second").await
            })
        })
        .await
        .unwrap();
        assert_eq!(
            get_setting(db.reader(), "k").await.unwrap(),
            Some("first".to_owned())
        );
    }
}
