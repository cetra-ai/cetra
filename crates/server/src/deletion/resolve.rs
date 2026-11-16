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

    let id = row.id;
    let session_count = (row.kind == DeletionKind::User).then(|| count(sessions.len()));
    let trace_count = count(scope.len());
    // J's cascade (D-J-9) reads the scope from `deletion_traces` in this transaction, so an item
    // added from a trace before it is never missed; add-from-trace checks the guard in its own
    // write, so one added after the guard is filled is refused.
    let (stored, cascade) = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                insert_traces(conn, id, &scope).await?;
                insert_sessions(conn, id, &sessions).await?;
                let stored = store_counts(conn, id, trace_count, session_count).await?;
                if stored {
                    // Running jobs are kept: the guard stops them (D-M-6).
                    crate::meta::eval_jobs::delete_jobs(conn, &jobs).await?;
                    let cascade =
                        crate::meta::experiments::delete_copies_of_tx(conn, id, project).await?;
                    return Ok((true, cascade));
                }
                Ok((false, Cascade::default()))
            })
        })
        .await
        .map_err(|error| super::from_meta(&error))?;
    if cascade.dataset_item_revisions > 0 || cascade.experiment_items > 0 {
        tracing::info!(
            target: "audit",
            deletion_id = %row.id,
            project_id = %project,
            dataset_item_revisions = cascade.dataset_item_revisions,
            experiment_items = cascade.experiment_items,
            "deletion removed dataset and experiment copies"
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
    let ids = filter_trace_ids(
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
    if ids.len() > cap {
        return Err(Step::Fail(FailReason::TooManyTraces));
    }
    Ok(ids)
}

/// A count as the `u64` the store takes. Saturates only where `usize` exceeds `u64`.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
