//! Scope resolution of `trace_ids`, `filter` and `user` deletions over the storage reads
//! (D-M-2, D-M-6). Called by the worker for a row in `resolving` with no stored scope.

use std::collections::HashSet;

use cetra_core::ids::{ProjectId, TraceId};
use cetra_storage::{
    FilterDeletionRequest, QueryContext, TimeWindow, eval_trace_ids, filter_trace_ids,
    user_deletion_scope,
};

use super::{RESOLVE_TIMEOUT, Step};
use crate::entitlements::{Visibility, query_options};
use crate::http::AppState;
use crate::meta::deletions::{
    DeletionKind, DeletionRow, FailReason, insert_sessions, insert_traces, scope_traces,
    store_counts,
};
use crate::meta::experiments::Cascade;

/// Resolves the scope of `row` and stores it with its counts in one transaction. Returns
/// `Ok(false)` when the row is no longer `resolving`, in which case nothing is handed over.
///
/// # Errors
///
/// Returns the [`Step`] to take when a read or the write fails.
pub(super) async fn resolve(
    state: &AppState,
    row: &DeletionRow,
    project: ProjectId,
) -> Result<bool, Step> {
    let source = state
        .storage
        .query(project, query_options(Visibility::exempt(), None))
        .await
        .map_err(|error| super::from_query(&error))?;
    let ctx = QueryContext::new(source, None).map_err(|error| super::from_run(&error, row.kind))?;

    let (targets, sessions) = match row.kind {
        DeletionKind::TraceIds => {
            let traces = scope_traces(state.meta.reader(), row.id)
                .await
                .map_err(|error| super::from_meta(&error))?;
            (traces, Vec::new())
        }
        DeletionKind::Filter => (filter_targets(state, row, &ctx).await?, Vec::new()),
        DeletionKind::User => {
            let Some(user_id) = row.user_id.as_deref() else {
                return Err(Step::Fail(FailReason::ResolutionFailed));
            };
            let scope = user_deletion_scope(&ctx, user_id, row.created_at, RESOLVE_TIMEOUT)
                .await
                .map_err(|error| super::from_run(&error, row.kind))?;
            (scope.trace_ids, scope.session_ids)
        }
        DeletionKind::Project | DeletionKind::Org => {
            return Err(Step::Fail(FailReason::ResolutionFailed));
        }
    };

    let target_set: HashSet<TraceId> = targets.iter().copied().collect();
    let evaluations = eval_trace_ids(&ctx, &target_set, RESOLVE_TIMEOUT)
        .await
        .map_err(|error| super::from_run(&error, row.kind))?;
    let mut scope = targets;
    scope.extend(evaluations);
    scope.sort();
    scope.dedup();
    // D-M-6: the guard holds the scope from now on, so a judge run about one of its traces that
    // starts or commits from here on is stopped (M3 registers the same entries again).
    state
        .deletions
        .guard
        .add_traces(project, &scope, row.id, row.created_at);
    // The jobs are read on the reader pool, outside the write transaction; a job queued or started
    // in between is stopped by the guard (D-M-6).
    let jobs = crate::meta::eval_jobs::jobs_for_traces(state.meta.reader(), project, &scope)
        .await
        .map_err(|error| super::from_meta(&error))?;
    // Annotation queue items of a session in the scope go too (D-AQ-9). The scan is skipped when
    // no queue holds a session item. The visibility is exempt, so old sessions are found.
    let queue_sessions: Vec<String> =
        if crate::meta::annotation_queues::has_session_items(state.meta.reader(), project)
            .await
            .map_err(|error| super::from_meta(&error))?
        {
            let traces: HashSet<TraceId> = scope.iter().copied().collect();
            let mut found = cetra_storage::session_ids_of_traces(&ctx, &traces, RESOLVE_TIMEOUT)
                .await
                .map_err(|error| super::from_run(&error, row.kind))?;
            found.extend(sessions.iter().cloned());
            found.sort();
            found.dedup();
            found
        } else {
            Vec::new()
        };

    let id = row.id;
    let session_count = (row.kind == DeletionKind::User).then(|| count(sessions.len()));
    let trace_count = count(scope.len());
    // D-CA-9: a user deletion also removes the user's coding-agent contexts. The context key is
    // the form the post stored (trimmed, lowercased). The writes lock keeps a post from
    // re-inserting one between the meta write and the map change; only a user deletion takes it.
    let context_user: Option<String> = match row.kind {
        DeletionKind::User => row
            .user_id
            .as_deref()
            .map(|user| user.trim().to_lowercase()),
        _ => None,
    };
    let contexts = state.ingest.contexts();
    let _writes = match context_user {
        Some(_) => Some(contexts.writes().lock().await),
        None => None,
    };
    let write_user = context_user.clone();
    // J's cascade (D-J-9) reads the scope from `deletion_traces` in this transaction, so an item
    // added from a trace before it is never missed; add-from-trace checks the guard in its own
    // write, so one added after the guard is filled is refused.
    let (stored, cascade, queue_items) = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                insert_traces(conn, id, &scope).await?;
                insert_sessions(conn, id, &sessions).await?;
                let stored = store_counts(conn, id, trace_count, session_count).await?;
                if stored {
                    // Running jobs are kept: the guard stops them (D-M-6).
                    crate::meta::eval_jobs::delete_jobs(conn, &jobs).await?;
                    crate::meta::issues::delete_candidates_of_deletion(conn, project, id).await?;
                    let cascade =
                        crate::meta::experiments::delete_copies_of_tx(conn, id, project).await?;
                    let queue_items = crate::meta::annotation_queues::delete_items_of_deletion_tx(
                        conn,
                        id,
                        project,
                        &queue_sessions,
                    )
                    .await?;
                    if let Some(user) = write_user.as_deref() {
                        crate::meta::agent_contexts::delete_user_contexts(conn, project, user)
                            .await?;
                    }
                    return Ok((true, cascade, queue_items));
                }
                Ok((false, Cascade::default(), 0))
            })
        })
        .await
        .map_err(|error| super::from_meta(&error))?;
    if let (true, Some(user), Some(map)) =
        (stored, context_user.as_deref(), contexts.project(project))
    {
        map.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, _, context| context.user.as_deref() != Some(user));
    }
    if cascade.dataset_item_revisions > 0 || cascade.experiment_items > 0 || queue_items > 0 {
        tracing::info!(
            target: "audit",
            deletion_id = %row.id,
            project_id = %project,
            dataset_item_revisions = cascade.dataset_item_revisions,
            experiment_items = cascade.experiment_items,
            queue_items = queue_items,
            "deletion removed derived copies"
        );
    }
    Ok(stored)
}

/// The traces matching a filter deletion's request, at most the cap.
async fn filter_targets(
    state: &AppState,
    row: &DeletionRow,
    ctx: &QueryContext,
) -> Result<Vec<TraceId>, Step> {
    let request = &row.request;
    let (Some(from), Some(to)) = (request.from, request.to) else {
        return Err(Step::Fail(FailReason::ResolutionFailed));
    };
    let window = TimeWindow::new(from, to).map_err(|error| super::from_run(&error, row.kind))?;
    let cap = state.deletions.filter_cap();
    let targets = filter_trace_ids(
        ctx,
        FilterDeletionRequest {
            filter: request.filter.as_deref(),
            search: request.search.as_deref(),
            window,
            include_eval: request.include_eval.unwrap_or(false),
            include_experiments: request.include_experiments.unwrap_or(false),
            limit: cap.saturating_add(1),
            timeout: RESOLVE_TIMEOUT,
        },
    )
    .await
    .map_err(|error| super::from_run(&error, row.kind))?;
    if targets.len() > cap {
        return Err(Step::Fail(FailReason::TooManyTraces));
    }
    Ok(targets.into_iter().map(|target| target.trace_id).collect())
}

/// A count as the `u64` the store takes. Saturates only where `usize` exceeds `u64`.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
