//! The instance id (D-TL-9): a random UUIDv4 in `instance_settings`, not a UUIDv7 (its timestamp
//! would reveal the install time). A restored backup keeps it. It is used for nothing but what
//! goes to us and is never joined with a licence.

use uuid::Uuid;

use crate::meta::instance_settings::{
    TELEMETRY_INSTANCE_ID, get_setting, get_setting_tx, insert_setting_if_absent,
};
use crate::meta::{MetaDb, MetaError};

/// This instance's id, created on first use. Two callers at once get the same id: the insert
/// keeps an existing row, and the read is in the same transaction.
///
/// # Errors
///
/// [`MetaError`] when the write fails, or [`MetaError::Corrupt`] when the stored value is not
/// a UUID.
pub(crate) async fn instance_id(meta: &MetaDb) -> Result<Uuid, MetaError> {
    let candidate = Uuid::new_v4().to_string();
    let stored = meta
        .write(move |conn| {
            Box::pin(async move {
                insert_setting_if_absent(conn, TELEMETRY_INSTANCE_ID, &candidate).await?;
                get_setting_tx(conn, TELEMETRY_INSTANCE_ID).await
            })
        })
        .await?;
    stored
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(MetaError::Corrupt {
            table: "instance_settings",
            column: "value",
        })
}

/// This instance's id when one was created, without creating it: a read-only command (the
/// debug bundle) must not create the id (D-TL-9).
///
/// # Errors
///
/// [`MetaError`] when the read fails, or [`MetaError::Corrupt`] when the stored value is not
/// a UUID.
pub(crate) async fn existing_instance_id(meta: &MetaDb) -> Result<Option<Uuid>, MetaError> {
    match get_setting(meta.reader(), TELEMETRY_INSTANCE_ID).await? {
        None => Ok(None),
        Some(value) => Uuid::parse_str(&value).map(Some).map_err(|_| MetaError::Corrupt {
            table: "instance_settings",
            column: "value",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::instance_settings::{get_setting, put_setting};

    #[tokio::test]
    async fn the_first_call_creates_a_v4_id() {
        let (_dir, db) = crate::meta::test_db().await;
        let id = instance_id(&db).await.unwrap();
        assert_eq!(id.get_version_num(), 4);
        let stored = get_setting(db.reader(), TELEMETRY_INSTANCE_ID)
            .await
            .unwrap();
        assert_eq!(stored, Some(id.hyphenated().to_string()));
    }

    #[tokio::test]
    async fn later_calls_return_the_same_id() {
        let (_dir, db) = crate::meta::test_db().await;
        let first = instance_id(&db).await.unwrap();
        let second = instance_id(&db).await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn an_existing_id_is_kept() {
        let (_dir, db) = crate::meta::test_db().await;
        let existing = "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d";
        db.write(move |conn| {
            Box::pin(async move { put_setting(conn, TELEMETRY_INSTANCE_ID, existing).await })
        })
        .await
        .unwrap();
        let id = instance_id(&db).await.unwrap();
        assert_eq!(id.to_string(), existing);
    }

    #[tokio::test]
    async fn concurrent_calls_agree() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, b) = tokio::join!(instance_id(&db), instance_id(&db));
        assert_eq!(a.unwrap(), b.unwrap());
    }

    #[tokio::test]
    async fn existing_instance_id_is_none_and_creates_nothing() {
        let (_dir, db) = crate::meta::test_db().await;
        assert_eq!(existing_instance_id(&db).await.unwrap(), None);
        let stored = get_setting(db.reader(), TELEMETRY_INSTANCE_ID)
            .await
            .unwrap();
        assert_eq!(stored, None);
    }

    #[tokio::test]
    async fn existing_instance_id_reads_a_created_id() {
        let (_dir, db) = crate::meta::test_db().await;
        let id = instance_id(&db).await.unwrap();
        assert_eq!(existing_instance_id(&db).await.unwrap(), Some(id));
    }

    #[tokio::test]
    async fn existing_instance_id_refuses_a_corrupt_value() {
        let (_dir, db) = crate::meta::test_db().await;
        db.write(|conn| {
            Box::pin(async move { put_setting(conn, TELEMETRY_INSTANCE_ID, "not-a-uuid").await })
        })
        .await
        .unwrap();
        let result = existing_instance_id(&db).await;
        assert!(matches!(result, Err(MetaError::Corrupt { .. })));
    }

    #[tokio::test]
    async fn a_corrupt_value_is_an_error() {
        let (_dir, db) = crate::meta::test_db().await;
        db.write(|conn| {
            Box::pin(async move { put_setting(conn, TELEMETRY_INSTANCE_ID, "not-a-uuid").await })
        })
        .await
        .unwrap();
        let result = instance_id(&db).await;
        assert!(matches!(
            result,
            Err(MetaError::Corrupt {
                table: "instance_settings",
                column: "value"
            })
        ));
        let stored = get_setting(db.reader(), TELEMETRY_INSTANCE_ID)
            .await
            .unwrap();
        assert_eq!(stored, Some("not-a-uuid".to_owned()));
    }
}
