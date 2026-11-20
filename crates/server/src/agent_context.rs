//! The in-memory session contexts of coding agents (D-CA-9). `normalize` reads one project's
//! map; the route (S3) and the ingest stamp (S4) write it. Each change is written to `meta.db`
//! first, then to the map, under `writes`.
// Until S3 (the route) and S4 (the stamp) call these, only the tests do.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use cetra_core::ids::ProjectId;
use cetra_otel::{AgentContext, AgentContexts};

use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::agent_contexts::{AgentContextRow, delete_expired};

/// Most contexts kept per project (D-CA-9).
pub(crate) const MAX_PER_PROJECT: usize = 10_000;
/// Longest life of a context (D-CA-9); a shorter project retention wins.
pub(crate) const MAX_AGE_DAYS: u32 = 30;
/// A post that changes nothing is written again only after this long.
pub(crate) const REWRITE_AFTER_NS: i64 = 3_600 * 1_000_000_000;
/// How often the sweep runs.
pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(3_600);

/// Per-project contexts. `normalize` reads one project's map under its read lock.
#[derive(Debug, Default)]
pub(crate) struct AgentContextStore {
    projects: RwLock<HashMap<ProjectId, Arc<RwLock<AgentContexts>>>>,
    /// Serializes writers (route, sweep, deletions), so the meta write and the map change stay in step.
    writes: tokio::sync::Mutex<()>,
}

impl AgentContextStore {
    /// The project's map, if it has one. Ingest clones the `Arc` into its blocking task.
    pub(crate) fn project(&self, project: ProjectId) -> Option<Arc<RwLock<AgentContexts>>> {
        self.projects
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&project)
            .cloned()
    }

    /// Replaces the whole content with `rows` (startup).
    pub(crate) fn load(&self, rows: Vec<AgentContextRow>) {
        let mut projects: HashMap<ProjectId, AgentContexts> = HashMap::new();
        for row in rows {
            projects.entry(row.project_id).or_default().insert(
                &row.agent,
                &row.session_id,
                AgentContext {
                    repository: row.repository,
                    branch: row.branch,
                    user: row.user_id,
                    updated_at: row.updated_at,
                },
            );
        }
        let maps = projects
            .into_iter()
            .map(|(project, contexts)| (project, Arc::new(RwLock::new(contexts))))
            .collect();
        *self
            .projects
            .write()
            .unwrap_or_else(PoisonError::into_inner) = maps;
    }

    /// Drops a project's map (project or org drop).
    pub(crate) fn forget_project(&self, project: ProjectId) {
        self.projects
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&project);
    }

    pub(crate) fn writes(&self) -> &tokio::sync::Mutex<()> {
        &self.writes
    }

    /// The project's map, created empty if absent.
    pub(crate) fn project_or_insert(&self, project: ProjectId) -> Arc<RwLock<AgentContexts>> {
        let mut projects = self
            .projects
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            projects
                .entry(project)
                .or_insert_with(|| Arc::new(RwLock::new(AgentContexts::default()))),
        )
    }
}

/// Deletes the contexts past their life, in `meta.db` and in memory, and returns how many rows
/// were deleted. Each project keeps a context for `min(MAX_AGE_DAYS, its retention)` days,
/// where no retention means `MAX_AGE_DAYS`.
///
/// # Errors
///
/// [`MetaError`] when the projects cannot be listed or the delete fails. Then no map changes.
pub(crate) async fn sweep_once(state: &AppState, now_ns: i64) -> Result<u64, MetaError> {
    // Retention is read before the writes lock is taken, so no lock is held across these reads.
    let (projects, _cap_failures) = crate::retention::effective_retention(state, now_ns).await?;
    let store = state.ingest.contexts();
    let _writes = store.writes().lock().await;
    let mut targets = Vec::new();
    for (project, retention) in projects {
        let Some(map) = store.project(project) else {
            continue;
        };
        let days = retention.map_or(MAX_AGE_DAYS, |days| days.min(MAX_AGE_DAYS));
        let before = now_ns.saturating_sub(i64::from(days) * 86_400 * 1_000_000_000);
        targets.push((project, before, map));
    }
    let pairs: Vec<(ProjectId, i64)> = targets
        .iter()
        .map(|(project, before, _)| (*project, *before))
        .collect();
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let mut total: u64 = 0;
                for (project, before) in pairs {
                    total += delete_expired(conn, project, before).await?;
                }
                Ok(total)
            })
        })
        .await?;
    for (_, before, map) in &targets {
        let mut map = map.write().unwrap_or_else(PoisonError::into_inner);
        map.retain(|_, _, context| context.updated_at >= *before);
    }
    Ok(deleted)
}

/// The hourly sweep. A failed run is logged; the next tick tries again.
pub(crate) fn spawn_agent_context_sweep(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            if let Err(error) = sweep_once(&state, now).await {
                tracing::warn!(error = %error, "agent contexts could not be swept");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::PoisonError;

    use cetra_core::ids::ProjectId;
    use cetra_otel::AgentContext;

    use super::{AgentContextStore, sweep_once};
    use crate::meta::agent_contexts::{AgentContextRow, load_contexts, upsert_context};
    use crate::test_support::TestApp;

    /// 2026-01-01T00:00:00Z, the manual clock's start.
    const NOW: i64 = 1_767_225_600_000_000_000;
    const DAY: i64 = 86_400 * 1_000_000_000;

    fn row(project: ProjectId, session: &str, updated_at: i64) -> AgentContextRow {
        AgentContextRow {
            project_id: project,
            agent: "codex".to_owned(),
            session_id: session.to_owned(),
            repository: Some("github.com/acme/app".to_owned()),
            branch: Some("main".to_owned()),
            user_id: Some("alice".to_owned()),
            updated_at,
        }
    }

    fn entry(store: &AgentContextStore, project: ProjectId, session: &str) -> Option<AgentContext> {
        let contexts = store.project(project)?;
        let guard = contexts.read().unwrap_or_else(PoisonError::into_inner);
        guard.get("codex", session).cloned()
    }

    /// Writes the rows to `meta.db` and loads the store from it, as startup does.
    async fn persist(app: &TestApp, rows: Vec<AgentContextRow>) {
        let for_db = rows;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for row in &for_db {
                        upsert_context(conn, row).await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
        let loaded = load_contexts(app.state.meta.reader()).await.unwrap();
        app.state.ingest.contexts().load(loaded);
    }

    #[test]
    fn load_then_project_returns_the_entries() {
        let store = AgentContextStore::default();
        let (a, b) = (ProjectId::new_v7(), ProjectId::new_v7());
        store.load(vec![row(a, "s1", NOW), row(b, "s2", NOW)]);
        assert_eq!(
            entry(&store, a, "s1"),
            Some(AgentContext {
                repository: Some("github.com/acme/app".to_owned()),
                branch: Some("main".to_owned()),
                user: Some("alice".to_owned()),
                updated_at: NOW,
            })
        );
        assert_eq!(entry(&store, b, "s2").map(|c| c.updated_at), Some(NOW));
        assert!(entry(&store, a, "s2").is_none());
        assert!(store.project(ProjectId::new_v7()).is_none());
    }

    #[test]
    fn forget_project_drops_the_map() {
        let store = AgentContextStore::default();
        let (a, b) = (ProjectId::new_v7(), ProjectId::new_v7());
        store.load(vec![row(a, "s1", NOW), row(b, "s2", NOW)]);
        store.forget_project(a);
        assert!(store.project(a).is_none());
        assert!(entry(&store, b, "s2").is_some());
    }

    #[tokio::test]
    async fn sweep_removes_entries_older_than_30_days() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let project = seeded.project.id;
        persist(
            &app,
            vec![
                row(project, "old", NOW - 31 * DAY),
                row(project, "new", NOW - DAY),
            ],
        )
        .await;

        let deleted = sweep_once(&app.state, NOW).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(entry(app.state.ingest.contexts(), project, "old").is_none());
        assert!(entry(app.state.ingest.contexts(), project, "new").is_some());
    }

    #[tokio::test]
    async fn sweep_uses_a_shorter_project_retention() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let (short, _) = app.add_project(&seeded, "short", Some(7)).await;
        persist(
            &app,
            vec![
                row(short.id, "eight", NOW - 8 * DAY),
                row(short.id, "six", NOW - 6 * DAY),
            ],
        )
        .await;

        let deleted = sweep_once(&app.state, NOW).await.unwrap();
        assert_eq!(deleted, 1);
        let store = app.state.ingest.contexts();
        assert!(entry(store, short.id, "eight").is_none());
        assert!(entry(store, short.id, "six").is_some());
    }

    #[tokio::test]
    async fn sweep_deletes_the_rows_too() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let project = seeded.project.id;
        persist(
            &app,
            vec![
                row(project, "old", NOW - 31 * DAY),
                row(project, "new", NOW - DAY),
            ],
        )
        .await;

        sweep_once(&app.state, NOW).await.unwrap();
        let left = load_contexts(app.state.meta.reader()).await.unwrap();
        assert_eq!(left, vec![row(project, "new", NOW - DAY)]);
    }
}
