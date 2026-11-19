//! Trace ids and end users of recent deletions, so a judge run that started before a
//! deletion does not write a copy of deleted data after its cut-off (D-M-6).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use cetra_core::ids::{ProjectId, TraceId};
use uuid::Uuid;

use crate::meta::{MetaDb, MetaError};

/// How long an entry is kept after its deletion's `created_at` (24 h).
pub(crate) const GUARD_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

#[derive(Debug, Default)]
pub(crate) struct DeletionGuard {
    inner: Mutex<GuardState>,
}

/// The deletion an entry comes from. When two deletions name the same trace or user, the
/// later one is kept.
#[derive(Debug, Clone, Copy)]
struct Entry {
    deletion: Uuid,
    created_at: i64,
}

impl Entry {
    fn keep_later(&mut self, other: Self) {
        if other.created_at > self.created_at {
            *self = other;
        }
    }
}

#[derive(Debug, Default)]
struct GuardState {
    traces: HashMap<ProjectId, HashMap<TraceId, Entry>>,
    users: HashMap<ProjectId, HashMap<String, Entry>>,
}

impl DeletionGuard {
    fn lock(&self) -> std::sync::MutexGuard<'_, GuardState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `traces` of `project` from `deletion`, created at `created_at`.
    pub(crate) fn add_traces(
        &self,
        project: ProjectId,
        traces: &[TraceId],
        deletion: Uuid,
        created_at: i64,
    ) {
        let entry = Entry {
            deletion,
            created_at,
        };
        let mut state = self.lock();
        let map = state.traces.entry(project).or_default();
        for trace in traces {
            map.entry(*trace).or_insert(entry).keep_later(entry);
        }
    }

    /// Records an end user of `project` from `deletion`, created at `created_at`.
    pub(crate) fn add_user(
        &self,
        project: ProjectId,
        user_id: &str,
        deletion: Uuid,
        created_at: i64,
    ) {
        let entry = Entry {
            deletion,
            created_at,
        };
        let mut state = self.lock();
        let map = state.users.entry(project).or_default();
        map.entry(user_id.to_owned())
            .or_insert(entry)
            .keep_later(entry);
    }

    /// The deletion that forbids a judge run about `trace` to go on, if any (D-M-6). Only
    /// entries with `created_at > now - GUARD_NS` count:
    /// - a trace-id entry for `trace` blocks;
    /// - a user entry blocks when `users` holds that user with an oldest ingest time at or
    ///   before the entry's `created_at`: the trace holds data of that user from before the
    ///   deletion. A trace with only newer data of the user is not covered (spec S5).
    ///
    /// `users` are the distinct user ids of the trace's loaded spans, each with the smallest
    /// `ingest_time` of its spans. Callers that have not loaded the trace pass `&[]`.
    /// The returned id lets a run that already committed a copy ask for its follow-up
    /// deletion (D-M-6).
    pub(crate) fn blocking(
        &self,
        project: ProjectId,
        trace: TraceId,
        users: &[(&str, i64)],
        now: i64,
    ) -> Option<Uuid> {
        let floor = now.saturating_sub(GUARD_NS);
        let state = self.lock();
        if let Some(entry) = state
            .traces
            .get(&project)
            .and_then(|map| map.get(&trace))
            .filter(|entry| entry.created_at > floor)
        {
            return Some(entry.deletion);
        }
        let map = state.users.get(&project)?;
        users.iter().find_map(|(user, oldest_ingest)| {
            map.get(*user)
                .filter(|entry| entry.created_at > floor && *oldest_ingest <= entry.created_at)
                .map(|entry| entry.deletion)
        })
    }

    /// Removes entries with `created_at <= now - GUARD_NS`, and empty project maps.
    pub(crate) fn prune(&self, now: i64) {
        let floor = now.saturating_sub(GUARD_NS);
        let mut state = self.lock();
        for map in state.traces.values_mut() {
            map.retain(|_, entry| entry.created_at > floor);
        }
        state.traces.retain(|_, map| !map.is_empty());
        for map in state.users.values_mut() {
            map.retain(|_, entry| entry.created_at > floor);
        }
        state.users.retain(|_, map| !map.is_empty());
    }

    /// Adds `meta::deletions::guard_entries(now - GUARD_NS)`.
    ///
    /// # Errors
    ///
    /// Returns [`MetaError`] when the entries cannot be read.
    pub(crate) async fn load(&self, meta: &MetaDb, now: i64) -> Result<(), MetaError> {
        let entries =
            crate::meta::deletions::guard_entries(meta.reader(), now.saturating_sub(GUARD_NS))
                .await?;
        for (project, trace, deletion, created_at) in entries.traces {
            self.add_traces(project, &[trace], deletion, created_at);
        }
        for (project, user, deletion, created_at) in entries.users {
            self.add_user(project, &user, deletion, created_at);
        }
        Ok(())
    }

    /// Total entries held, for tests.
    #[cfg(test)]
    fn len(&self) -> usize {
        let state = self.lock();
        let traces: usize = state.traces.values().map(HashMap::len).sum();
        let users: usize = state.users.values().map(HashMap::len).sum();
        traces + users
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::deletions::{
        ActorKind, DeletionKind, DeletionState, NewDeletion, RequestDetails, insert_deletion,
        insert_traces,
    };
    use crate::test_support::TestApp;
    use cetra_core::ids::OrgId;
    use uuid::Uuid;

    fn trace(n: u128) -> TraceId {
        TraceId::from_bytes((n + 1).to_be_bytes()).unwrap()
    }

    #[test]
    fn blocks_known_traces_and_users_for_a_day() {
        let guard = DeletionGuard::default();
        let p = ProjectId::new_v7();
        let p2 = ProjectId::new_v7();
        let (d1, d2) = (Uuid::from_u128(1), Uuid::from_u128(2));
        guard.add_traces(p, &[trace(1)], d1, 100);
        guard.add_user(p, "u", d2, 100);
        let (t, t2) = (trace(1), trace(2));

        assert_eq!(guard.blocking(p, t, &[], 101), Some(d1));
        // The user's data ingested at or before the cut-off is covered, even when the
        // trace also holds newer data; a trace with only newer data of the user is not.
        assert_eq!(guard.blocking(p, t2, &[("u", 100)], 101), Some(d2));
        assert_eq!(
            guard.blocking(p, t2, &[("v", 50), ("u", 99)], 101),
            Some(d2)
        );
        assert_eq!(guard.blocking(p, t2, &[("u", 101)], 102), None);
        assert_eq!(guard.blocking(p2, t, &[], 101), None);
        assert_eq!(guard.blocking(p, t2, &[], 101), None);
        // Expired after a day.
        assert_eq!(guard.blocking(p, t, &[], 100 + GUARD_NS), None);
    }

    #[test]
    fn the_later_deletion_wins() {
        let guard = DeletionGuard::default();
        let p = ProjectId::new_v7();
        let (d1, d2) = (Uuid::from_u128(1), Uuid::from_u128(2));
        guard.add_traces(p, &[trace(1)], d2, 200);
        guard.add_traces(p, &[trace(1)], d1, 100);
        assert_eq!(guard.blocking(p, trace(1), &[], 201), Some(d2));
    }

    #[test]
    fn prune_drops_expired_entries() {
        let guard = DeletionGuard::default();
        let p = ProjectId::new_v7();
        let d = Uuid::from_u128(1);
        guard.add_traces(p, &[trace(1), trace(2)], d, 100);
        guard.add_user(p, "u", d, 100);
        assert_eq!(guard.len(), 3);

        guard.prune(100 + GUARD_NS);
        assert_eq!(guard.len(), 0);
        assert_eq!(
            guard.blocking(p, trace(1), &[("u", 0)], 100 + GUARD_NS),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn load_reads_recent_deletions() {
        let app = TestApp::new().await;
        let now = app.state.clock.now_ns();
        let p = ProjectId::new_v7();
        let org: OrgId = OrgId::new_v7();
        let (t, t2, x) = (trace(1), trace(2), trace(3));

        let user_id = Uuid::now_v7();
        let user_row = NewDeletion {
            id: user_id,
            org_id: org,
            project_id: Some(p),
            kind: DeletionKind::User,
            user_id: Some("user-1".to_owned()),
            request: RequestDetails::default(),
            actor_kind: ActorKind::User,
            actor_id: Uuid::now_v7(),
            state: DeletionState::Resolving,
            created_at: now,
        };
        let old_row = NewDeletion {
            id: Uuid::now_v7(),
            kind: DeletionKind::TraceIds,
            user_id: None,
            created_at: now - GUARD_NS - 1,
            ..user_row.clone()
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_deletion(conn, &user_row).await?;
                    insert_traces(conn, user_row.id, &[t]).await?;
                    insert_deletion(conn, &old_row).await?;
                    insert_traces(conn, old_row.id, &[t2]).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let guard = DeletionGuard::default();
        guard.load(&app.state.meta, now).await.unwrap();
        assert_eq!(guard.blocking(p, t, &[], now), Some(user_id));
        assert_eq!(
            guard.blocking(p, x, &[("user-1", now - 1)], now),
            Some(user_id)
        );
        assert_eq!(guard.blocking(p, t2, &[], now), None);
    }
}
