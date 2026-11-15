//! `pending_traces` rows: the persisted copy of the trigger's in-memory map (D-I-8).

use llmobs_core::ids::{ProjectId, TraceId};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;

/// One pending trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingRow {
    pub(crate) project_id: ProjectId,
    pub(crate) trace_id: TraceId,
    pub(crate) first_seen: i64,
    pub(crate) last_seen: i64,
    pub(crate) min_start: i64,
    pub(crate) root_ended: bool,
}

/// Every row of an active project of an active org, ordered by `project_id, trace_id`.
///
/// # Errors
///
/// [`MetaError`] when the query fails, or `MetaError::Corrupt` for a `trace_id` blob that is not 16 bytes.
pub(crate) async fn load_pending(pool: &SqlitePool) -> Result<Vec<PendingRow>, MetaError> {
    let records: Vec<(String, Vec<u8>, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT t.project_id, t.trace_id, t.first_seen, t.last_seen, t.min_start, t.root_ended \
         FROM pending_traces t \
         JOIN projects p ON p.id = t.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE p.state = 'active' AND o.state = 'active' \
         ORDER BY t.project_id, t.trace_id",
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(
            |(project, trace, first_seen, last_seen, min_start, root_ended)| {
                Ok(PendingRow {
                    project_id: parse_project(&project)?,
                    trace_id: TraceId::from_slice(&trace).map_err(|_| corrupt())?,
                    first_seen,
                    last_seen,
                    min_start,
                    root_ended: root_ended != 0,
                })
            },
        )
        .collect()
}

/// Deletes `deletes`, then upserts `upserts`, on the caller's connection (the caller's
/// `MetaDb::write` is the transaction). An upsert for a project that no longer exists writes nothing.
///
/// # Errors
///
/// [`MetaError`] when a statement fails.
pub(crate) async fn write_pending(
    conn: &mut SqliteConnection,
    deletes: &[(ProjectId, TraceId)],
    upserts: &[PendingRow],
) -> Result<(), MetaError> {
    for (project, trace) in deletes {
        sqlx::query("DELETE FROM pending_traces WHERE project_id = ? AND trace_id = ?")
            .bind(project.to_string())
            .bind(trace.as_bytes().to_vec())
            .execute(&mut *conn)
            .await?;
    }
    for row in upserts {
        sqlx::query(
            "INSERT INTO pending_traces (project_id, trace_id, first_seen, last_seen, min_start, root_ended) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6 WHERE EXISTS (SELECT 1 FROM projects WHERE id = ?1) \
             ON CONFLICT(project_id, trace_id) DO UPDATE SET \
             first_seen = excluded.first_seen, last_seen = excluded.last_seen, \
             min_start = excluded.min_start, root_ended = excluded.root_ended",
        )
        .bind(row.project_id.to_string())
        .bind(row.trace_id.as_bytes().to_vec())
        .bind(row.first_seen)
        .bind(row.last_seen)
        .bind(row.min_start)
        .bind(i64::from(row.root_ended))
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn corrupt() -> MetaError {
    MetaError::Corrupt {
        table: "pending_traces",
        column: "trace_id",
    }
}

fn parse_project(value: &str) -> Result<ProjectId, MetaError> {
    let id: Uuid = value.parse().map_err(|_| MetaError::Corrupt {
        table: "pending_traces",
        column: "project_id",
    })?;
    Ok(ProjectId::from_uuid(id))
}

#[cfg(test)]
mod tests {
    use llmobs_core::ids::{OrgId, ProjectId, TraceId};

    use super::{PendingRow, load_pending, write_pending};
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000;

    fn trace(byte: u8) -> TraceId {
        TraceId::from_bytes([byte; 16]).unwrap()
    }

    fn row(project: ProjectId, byte: u8, last_seen: i64) -> PendingRow {
        PendingRow {
            project_id: project,
            trace_id: trace(byte),
            first_seen: 10,
            last_seen,
            min_start: 5,
            root_ended: false,
        }
    }

    /// Two projects of one org.
    async fn seed(db: &MetaDb) -> (ProjectId, ProjectId) {
        db.write(|conn| {
            Box::pin(async move {
                let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", NOW)
                    .await?
                    .id;
                let a = insert_project(conn, org, "A", "a", None, NOW).await?.id;
                let b = insert_project(conn, org, "B", "b", None, NOW).await?.id;
                Ok((a, b))
            })
        })
        .await
        .unwrap()
    }

    async fn write(db: &MetaDb, deletes: Vec<(ProjectId, TraceId)>, upserts: Vec<PendingRow>) {
        db.write(move |conn| {
            Box::pin(async move { write_pending(conn, &deletes, &upserts).await })
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn write_then_load_round_trips() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, b) = seed(&db).await;
        write(
            &db,
            Vec::new(),
            vec![row(b, 2, NOW), row(a, 9, NOW), row(a, 1, NOW)],
        )
        .await;
        let loaded = load_pending(db.reader()).await.unwrap();
        let mut expected = vec![row(a, 1, NOW), row(a, 9, NOW), row(b, 2, NOW)];
        expected.sort_by_key(|r| (r.project_id.to_string(), r.trace_id.as_bytes().to_vec()));
        assert_eq!(loaded, expected);
    }

    #[tokio::test]
    async fn an_upsert_replaces_the_columns() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        write(&db, Vec::new(), vec![row(a, 1, NOW)]).await;
        let mut later = row(a, 1, NOW + 50);
        later.root_ended = true;
        write(&db, Vec::new(), vec![later]).await;
        let loaded = load_pending(db.reader()).await.unwrap();
        assert_eq!(loaded, vec![later]);
    }

    #[tokio::test]
    async fn deletes_run_before_upserts() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        write(&db, vec![(a, trace(1))], vec![row(a, 1, NOW)]).await;
        assert_eq!(
            load_pending(db.reader()).await.unwrap(),
            vec![row(a, 1, NOW)]
        );
    }

    #[tokio::test]
    async fn an_upsert_for_a_missing_project_writes_nothing() {
        let (_dir, db) = crate::meta::test_db().await;
        seed(&db).await;
        let missing = ProjectId::new_v7();
        write(&db, Vec::new(), vec![row(missing, 1, NOW)]).await;
        assert!(load_pending(db.reader()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn load_skips_inactive_projects_and_orgs() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, b) = seed(&db).await;
        write(&db, Vec::new(), vec![row(a, 1, NOW), row(b, 2, NOW)]).await;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE projects SET state = 'deleting' WHERE id = ?")
                    .bind(b.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(
            load_pending(db.reader()).await.unwrap(),
            vec![row(a, 1, NOW)]
        );
    }

    #[tokio::test]
    async fn deleting_the_project_cascades() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        write(&db, Vec::new(), vec![row(a, 1, NOW)]).await;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM projects WHERE id = ?")
                    .bind(a.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending_traces")
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn a_bad_trace_id_blob_is_corrupt() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO pending_traces (project_id, trace_id, first_seen, last_seen, min_start) \
                     VALUES (?, X'010203', 1, 1, 1)",
                )
                .bind(a.to_string())
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        let error = load_pending(db.reader()).await.unwrap_err();
        assert!(matches!(error, MetaError::Corrupt { .. }), "{error}");
    }
}
