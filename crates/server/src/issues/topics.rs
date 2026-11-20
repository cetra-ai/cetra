//! Topic candidates (D-IC-4): a completed production trace of a `topic` source becomes a
//! candidate, when the source's filter and sampling keep it.

use cetra_storage::{QueryContext, RunError, TraceRefreshRequest, trace_refresh};

use super::triggers::{Place, queued};
use crate::api::convert::{MatchRow, match_rows};
use crate::entitlements::{Visibility, query_options};
use crate::eval::budget::day_start;
use crate::eval::pending::Completed;
use crate::eval::sampling;
use crate::eval::trigger::{READ_TIMEOUT, SweepCtx};
use crate::meta::issues::{CandidateRow, admit_daily, insert_candidate};

/// Topic candidates of one batch of completed traces, for every topic source of the project, in
/// id order. A source whose filter does not parse is skipped, with the hourly warning the rules
/// use. Each source's candidates are written in one transaction.
///
/// # Errors
///
/// `Err(())` when a read or a write fails. Its caller retries the batch when no rule runs with it
/// (`eval::trigger`). A retry re-admits the traces of the sources that were already written: the
/// dedup keys keep them from being queued twice, but they count again against their daily caps.
pub(crate) async fn sweep_batch(
    ctx: &SweepCtx<'_>,
    around: i64,
    batch: &[Completed],
) -> Result<(), ()> {
    let opened = ctx
        .state
        .storage
        .query(
            ctx.project,
            query_options(Visibility::window(ctx.state, ctx.org), Some(around)),
        )
        .await
        .ok()
        .and_then(|source| QueryContext::new(source, None).ok());
    let Some(query) = opened else {
        tracing::warn!(project_id = %ctx.project, "a topic read could not start");
        return Err(());
    };
    let ids: Vec<String> = batch
        .iter()
        .map(|entry| entry.trace_id.to_string())
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    for source in ctx.topic_sources {
        // Both are set for a topic source (D-IC-1); a source without them takes nothing.
        let (Some(rate), Some(cap)) = (source.sample_rate, source.daily_cap) else {
            continue;
        };
        let read = trace_refresh(
            &query,
            TraceRefreshRequest {
                filter: source.filter.as_deref(),
                search: None,
                trace_ids: &refs,
                include_eval: false,
                include_experiments: false,
                strict_exclusions: true,
                timeout: READ_TIMEOUT,
            },
        )
        .await;
        #[cfg(test)]
        let read = crate::eval::trigger::faults::take_read().map_or(read, Err);
        let batches = match read {
            Ok(batches) => batches,
            // The filter does not parse: this source is skipped, the others go on. The warning is
            // logged at most once per source per hour.
            Err(RunError::Invalid(_) | RunError::InvalidRequest(_)) => {
                if ctx.state.eval.filter_warning_due(source.id, ctx.now) {
                    tracing::warn!(
                        project_id = %ctx.project,
                        source_id = %source.id,
                        "an issue source's filter could not be read; the source was skipped"
                    );
                }
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    project_id = %ctx.project,
                    source_id = %source.id,
                    kind = error.kind(),
                    "a topic read failed"
                );
                return Err(());
            }
        };
        let rows = match match_rows(&batches, false) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(
                    project_id = %ctx.project,
                    source_id = %source.id,
                    error = %error,
                    "a topic read returned a row that does not convert"
                );
                return Err(());
            }
        };
        let kept: Vec<MatchRow> = rows
            .into_iter()
            .filter(|row| sampling::sampled(rate, source.id, row.trace_id, None))
            .collect();
        if kept.is_empty() {
            continue;
        }
        let candidates: Vec<CandidateRow> = kept
            .into_iter()
            .map(|row| {
                queued(
                    ctx.now,
                    ctx.project,
                    source.id,
                    Place::Trace {
                        trace: row.trace_id,
                        session: None,
                    },
                    None,
                    Some(row.start_time),
                    None,
                )
            })
            .collect();
        let (source_id, now, count) = (source.id, ctx.now, candidates.len());
        let outcome = ctx
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for row in candidates {
                        if admit_daily(conn, source_id, day_start(now), cap).await? {
                            insert_candidate(conn, &row).await?;
                        }
                    }
                    Ok(())
                })
            })
            .await;
        if let Err(error) = outcome {
            tracing::warn!(
                project_id = %ctx.project,
                source_id = %source.id,
                count,
                error = %error,
                "topic candidates could not be queued"
            );
            return Err(());
        }
        ctx.state.issues_wake();
    }
    Ok(())
}
