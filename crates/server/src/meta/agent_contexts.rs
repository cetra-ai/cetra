//! `agent_contexts` rows: the persisted copy of the session contexts a coding-agent hook
//! reported (D-CA-9). Only the store in `agent_context` reads them back at startup.

use cetra_core::ids::ProjectId;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;

/// One session context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentContextRow {
    pub(crate) project_id: ProjectId,
    pub(crate) agent: String,
    pub(crate) session_id: String,
    pub(crate) repository: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) user_id: Option<String>,
    pub(crate) updated_at: i64,
}

/// One `agent_contexts` record as selected: project, agent, session, repository, branch, user, time.
type ContextRecord = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

/// Every row of an active project of an active org, ordered by `project_id, agent, session_id`.
///
/// # Errors
///
/// [`MetaError`] when the query fails, or `MetaError::Corrupt` for a `project_id` that is not a UUID.
pub(crate) async fn load_contexts(pool: &SqlitePool) -> Result<Vec<AgentContextRow>, MetaError> {
    let records: Vec<ContextRecord> = sqlx::query_as(
        "SELECT c.project_id, c.agent, c.session_id, c.repository, c.branch, c.user_id, c.updated_at \
         FROM agent_contexts c \
         JOIN projects p ON p.id = c.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE p.state = 'active' AND o.state = 'active' \
         ORDER BY c.project_id, c.agent, c.session_id",
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(
            |(project, agent, session_id, repository, branch, user_id, updated_at)| {
                Ok(AgentContextRow {
                    project_id: parse_project(&project)?,
                    agent,
                    session_id,
                    repository,
                    branch,
                    user_id,
                    updated_at,
                })
            },
        )
        .collect()
}

/// Inserts the row, or replaces the repository, branch, user and `updated_at` of the row with
/// the same key. Writes nothing when the project no longer exists.
/// Returns whether a row was written.
///
/// # Errors
///
/// [`MetaError`] when the statement fails.
pub(crate) async fn upsert_context(
    conn: &mut SqliteConnection,
    row: &AgentContextRow,
) -> Result<bool, MetaError> {
    let written = sqlx::query(
        "INSERT INTO agent_contexts (project_id, agent, session_id, repository, branch, user_id, updated_at) \
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7 WHERE EXISTS (SELECT 1 FROM projects WHERE id = ?1) \
         ON CONFLICT(project_id, agent, session_id) DO UPDATE SET \
         repository = excluded.repository, branch = excluded.branch, \
         user_id = excluded.user_id, updated_at = excluded.updated_at",
    )
    .bind(row.project_id.to_string())
    .bind(&row.agent)
    .bind(&row.session_id)
    .bind(&row.repository)
    .bind(&row.branch)
    .bind(&row.user_id)
    .bind(row.updated_at)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(written > 0)
}

/// When `key` is not stored yet and the project has at least `cap` rows, deletes the row with the
/// smallest `(updated_at, agent, session_id)` and returns its `(agent, session_id)`. Under `cap`,
/// or when the `key` row already exists (an update, not an insert), deletes nothing and returns
/// `None`.
///
/// # Errors
///
/// [`MetaError`] when a statement fails.
pub(crate) async fn delete_oldest(
    conn: &mut SqliteConnection,
    project: ProjectId,
    cap: usize,
    key: (&str, &str),
) -> Result<Option<(String, String)>, MetaError> {
    let project = project.to_string();
    let (agent, session_id) = key;
    let present: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM agent_contexts WHERE project_id = ?1 AND agent = ?2 AND session_id = ?3",
    )
    .bind(&project)
    .bind(agent)
    .bind(session_id)
    .fetch_optional(&mut *conn)
    .await?;
    if present.is_some() {
        return Ok(None);
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_contexts WHERE project_id = ?1")
            .bind(&project)
            .fetch_one(&mut *conn)
            .await?;
    // `cap` is a small constant; a value beyond i64 can never be reached by `count`.
    let cap = i64::try_from(cap).unwrap_or(i64::MAX);
    if count < cap {
        return Ok(None);
    }
    let oldest: Option<(String, String)> = sqlx::query_as(
        "SELECT agent, session_id FROM agent_contexts WHERE project_id = ?1 \
         ORDER BY updated_at, agent, session_id LIMIT 1",
    )
    .bind(&project)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((agent, session_id)) = oldest else {
        return Ok(None);
    };
    sqlx::query(
        "DELETE FROM agent_contexts WHERE project_id = ?1 AND agent = ?2 AND session_id = ?3",
    )
    .bind(&project)
    .bind(&agent)
    .bind(&session_id)
    .execute(&mut *conn)
    .await?;
    Ok(Some((agent, session_id)))
}

/// Deletes the project's rows whose `updated_at` is before `before`. Returns how many.
///
/// # Errors
///
/// [`MetaError`] when the statement fails.
pub(crate) async fn delete_expired(
    conn: &mut SqliteConnection,
    project: ProjectId,
    before: i64,
) -> Result<u64, MetaError> {
    let deleted =
        sqlx::query("DELETE FROM agent_contexts WHERE project_id = ?1 AND updated_at < ?2")
            .bind(project.to_string())
            .bind(before)
            .execute(&mut *conn)
            .await?
            .rows_affected();
    Ok(deleted)
}

/// Deletes the project's rows of `user_id`. Returns how many.
///
/// # Errors
///
/// [`MetaError`] when the statement fails.
pub(crate) async fn delete_user_contexts(
    conn: &mut SqliteConnection,
    project: ProjectId,
    user_id: &str,
) -> Result<u64, MetaError> {
    let deleted = sqlx::query("DELETE FROM agent_contexts WHERE project_id = ?1 AND user_id = ?2")
        .bind(project.to_string())
        .bind(user_id)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(deleted)
}

fn parse_project(value: &str) -> Result<ProjectId, MetaError> {
    let id: Uuid = value.parse().map_err(|_| MetaError::Corrupt {
        table: "agent_contexts",
        column: "project_id",
    })?;
    Ok(ProjectId::from_uuid(id))
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::{OrgId, ProjectId};

    use super::{
        AgentContextRow, delete_expired, delete_oldest, delete_user_contexts, load_contexts,
        upsert_context,
    };
    use crate::meta::MetaDb;
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;

    const NOW: i64 = 1_000;

    fn row(project: ProjectId, agent: &str, session: &str, updated_at: i64) -> AgentContextRow {
        AgentContextRow {
            project_id: project,
            agent: agent.to_owned(),
            session_id: session.to_owned(),
            repository: Some("github.com/acme/app".to_owned()),
            branch: Some("main".to_owned()),
            user_id: Some("alice".to_owned()),
            updated_at,
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

    async fn upsert(db: &MetaDb, row: AgentContextRow) -> bool {
        db.write(move |conn| Box::pin(async move { upsert_context(conn, &row).await }))
            .await
            .unwrap()
    }

    async fn count(db: &MetaDb) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_contexts")
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn upsert_then_load_round_trips() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, b) = seed(&db).await;
        assert!(upsert(&db, row(b, "codex", "s2", NOW)).await);
        assert!(upsert(&db, row(a, "claude-code", "s1", NOW)).await);
        let loaded = load_contexts(db.reader()).await.unwrap();
        let mut expected = vec![row(a, "claude-code", "s1", NOW), row(b, "codex", "s2", NOW)];
        expected.sort_by_key(|r| {
            (
                r.project_id.to_string(),
                r.agent.clone(),
                r.session_id.clone(),
            )
        });
        assert_eq!(loaded, expected);
    }

    #[tokio::test]
    async fn an_empty_context_round_trips_with_absent_fields() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        let empty = AgentContextRow {
            project_id: a,
            agent: "copilot".to_owned(),
            session_id: "s9".to_owned(),
            repository: None,
            branch: None,
            user_id: None,
            updated_at: NOW,
        };
        assert!(upsert(&db, empty.clone()).await);
        assert_eq!(load_contexts(db.reader()).await.unwrap(), vec![empty]);
    }

    #[tokio::test]
    async fn upsert_replaces_the_same_key() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        assert!(upsert(&db, row(a, "codex", "s1", NOW)).await);
        let mut later = row(a, "codex", "s1", NOW + 50);
        later.branch = Some("feature".to_owned());
        later.repository = None;
        later.user_id = None;
        assert!(upsert(&db, later.clone()).await);
        assert_eq!(count(&db).await, 1);
        assert_eq!(load_contexts(db.reader()).await.unwrap(), vec![later]);
    }

    #[tokio::test]
    async fn upsert_for_a_dropped_project_writes_nothing() {
        let (_dir, db) = crate::meta::test_db().await;
        seed(&db).await;
        let missing = ProjectId::new_v7();
        assert!(!upsert(&db, row(missing, "codex", "s1", NOW)).await);
        assert_eq!(count(&db).await, 0);
    }

    #[tokio::test]
    async fn delete_oldest_picks_the_smallest_updated_at_then_key() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        upsert(&db, row(a, "codex", "s0", 9)).await;
        upsert(&db, row(a, "claude-code", "s2", 5)).await;
        upsert(&db, row(a, "claude-code", "s1", 5)).await;

        // Under the cap: nothing is deleted.
        let under = db
            .write(move |conn| {
                Box::pin(async move { delete_oldest(conn, a, 4, ("codex", "new")).await })
            })
            .await
            .unwrap();
        assert_eq!(under, None);
        assert_eq!(count(&db).await, 3);

        // At the cap: the tie on updated_at goes to the smaller (agent, session_id).
        let deleted = db
            .write(move |conn| {
                Box::pin(async move { delete_oldest(conn, a, 3, ("codex", "new")).await })
            })
            .await
            .unwrap();
        assert_eq!(deleted, Some(("claude-code".to_owned(), "s1".to_owned())));
        let left: Vec<(String, String)> = sqlx::query_as(
            "SELECT agent, session_id FROM agent_contexts ORDER BY agent, session_id",
        )
        .fetch_all(db.reader())
        .await
        .unwrap();
        assert_eq!(
            left,
            vec![
                ("claude-code".to_owned(), "s2".to_owned()),
                ("codex".to_owned(), "s0".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn delete_oldest_skips_an_existing_key() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        upsert(&db, row(a, "codex", "s0", 9)).await;
        upsert(&db, row(a, "claude-code", "s2", 5)).await;
        upsert(&db, row(a, "claude-code", "s1", 5)).await;

        // At the cap, but the key is already stored: an update evicts nothing.
        let deleted = db
            .write(move |conn| {
                Box::pin(async move { delete_oldest(conn, a, 3, ("codex", "s0")).await })
            })
            .await
            .unwrap();
        assert_eq!(deleted, None);
        assert_eq!(count(&db).await, 3);
        let left: Vec<(String, String)> = sqlx::query_as(
            "SELECT agent, session_id FROM agent_contexts ORDER BY agent, session_id",
        )
        .fetch_all(db.reader())
        .await
        .unwrap();
        assert_eq!(
            left,
            vec![
                ("claude-code".to_owned(), "s1".to_owned()),
                ("claude-code".to_owned(), "s2".to_owned()),
                ("codex".to_owned(), "s0".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn load_skips_inactive_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, b) = seed(&db).await;
        upsert(&db, row(a, "codex", "s1", NOW)).await;
        upsert(&db, row(b, "codex", "s2", NOW)).await;
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
            load_contexts(db.reader()).await.unwrap(),
            vec![row(a, "codex", "s1", NOW)]
        );
    }

    #[tokio::test]
    async fn delete_expired_removes_only_older_rows() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, b) = seed(&db).await;
        upsert(&db, row(a, "codex", "old", 10)).await;
        upsert(&db, row(a, "codex", "edge", 20)).await;
        upsert(&db, row(a, "codex", "new", 30)).await;
        upsert(&db, row(b, "codex", "old", 10)).await;
        let deleted = db
            .write(move |conn| Box::pin(async move { delete_expired(conn, a, 20).await }))
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        let left: Vec<String> = sqlx::query_scalar(
            "SELECT session_id FROM agent_contexts ORDER BY project_id, session_id",
        )
        .fetch_all(db.reader())
        .await
        .unwrap();
        assert_eq!(left.len(), 3);
        assert!(left.contains(&"edge".to_owned()));
        assert!(left.contains(&"new".to_owned()));
    }

    #[tokio::test]
    async fn delete_user_contexts_removes_only_that_user_in_that_project() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, b) = seed(&db).await;
        upsert(&db, row(a, "codex", "s1", NOW)).await;
        let mut other = row(a, "codex", "s2", NOW);
        other.user_id = Some("bob".to_owned());
        upsert(&db, other).await;
        upsert(&db, row(b, "codex", "s3", NOW)).await;
        let deleted = db
            .write(move |conn| {
                Box::pin(async move { delete_user_contexts(conn, a, "alice").await })
            })
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(count(&db).await, 2);
    }

    #[tokio::test]
    async fn project_delete_cascades() {
        let (_dir, db) = crate::meta::test_db().await;
        let (a, _) = seed(&db).await;
        upsert(&db, row(a, "codex", "s1", NOW)).await;
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
        assert_eq!(count(&db).await, 0);
    }
}
