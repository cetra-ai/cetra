//! Light lookups of annotation-queue targets by id (D-AQ-8): which traces, spans and sessions exist and are visible, with their target time.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use arrow::array::{ArrayRef, AsArray, PrimitiveArray};
use arrow::datatypes::TimestampNanosecondType;
use cetra_core::ids::{SpanId, TraceId};
use cetra_core::schema::{spans, traces};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{Expr, col, lit};

use super::deletions::{bad_id, utf8};
use super::{ListScope, scoped_traces};
use crate::query_error::{RunError, classify};
use crate::views::QueryContext;

/// Most ids one lookup takes (D-AQ-8: an add takes 1–1 000 targets).
pub const MAX_TARGET_IDS: usize = 1_000;

/// A visible trace target and its start time (its target time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceTarget {
    /// The trace id.
    pub trace_id: TraceId,
    /// The trace's merged `start_time`, nanoseconds since the Unix epoch.
    pub start_time: i64,
}

/// A visible span target: its pair and its trace's start time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanTarget {
    /// The trace id.
    pub trace_id: TraceId,
    /// The span id.
    pub span_id: SpanId,
    /// The trace's `start_time`, nanoseconds since the Unix epoch.
    pub trace_start: i64,
}

/// A visible session target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTarget {
    /// The session id.
    pub session_id: String,
    /// Start of the session's earliest visible trace.
    pub start_time: i64,
    /// The session's visible trace ids, sorted, for the deletion-guard check.
    pub trace_ids: Vec<TraceId>,
}

/// The visible, non-evaluation traces among `ids`, by trace id. Experiment traces are included.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for more than [`MAX_TARGET_IDS`] ids; [`RunError::Timeout`];
/// the query's other errors as `ctx.fail` classifies them.
pub async fn trace_targets(
    ctx: &QueryContext,
    ids: &[TraceId],
    timeout: Duration,
) -> Result<Vec<TraceTarget>, RunError> {
    check_len(ids.len())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    bounded(timeout, trace_scan(ctx, ids)).await
}

/// The visible spans among `ids` (trace id, span id) whose trace is a target of
/// [`trace_targets`], by pair, each with its trace's start time.
///
/// # Errors
///
/// As [`trace_targets`].
pub async fn span_targets(
    ctx: &QueryContext,
    ids: &[(TraceId, SpanId)],
    timeout: Duration,
) -> Result<Vec<SpanTarget>, RunError> {
    check_len(ids.len())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    bounded(timeout, span_scan(ctx, ids)).await
}

/// The visible sessions among `ids` with their earliest visible trace's start and their
/// visible trace ids. Evaluation traces are left out of both.
///
/// # Errors
///
/// As [`trace_targets`].
pub async fn session_targets(
    ctx: &QueryContext,
    ids: &[&str],
    timeout: Duration,
) -> Result<Vec<SessionTarget>, RunError> {
    check_len(ids.len())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    bounded(timeout, session_scan(ctx, ids)).await
}

async fn bounded<T>(
    timeout: Duration,
    scan: impl Future<Output = Result<T, RunError>>,
) -> Result<T, RunError> {
    match tokio::time::timeout(timeout, scan).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

fn check_len(len: usize) -> Result<(), RunError> {
    if len > MAX_TARGET_IDS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_TARGET_IDS} target ids"
        )));
    }
    Ok(())
}

/// Distinct literals of `values`, in sorted order.
fn literals(values: impl Iterator<Item = String>) -> Vec<Expr> {
    values
        .collect::<BTreeSet<String>>()
        .into_iter()
        .map(lit)
        .collect()
}

/// The traces-view scope of a target lookup: no window, no evaluation traces, experiments kept.
async fn target_traces(ctx: &QueryContext) -> Result<datafusion::prelude::DataFrame, RunError> {
    scoped_traces(
        ctx,
        ListScope {
            window: None,
            include_eval: false,
            include_experiments: true,
        },
        false,
    )
    .await
}

async fn trace_scan(ctx: &QueryContext, ids: &[TraceId]) -> Result<Vec<TraceTarget>, RunError> {
    let frame = target_traces(ctx)
        .await?
        .filter(col(traces::TRACE_ID).in_list(literals(ids.iter().map(TraceId::to_hex)), false))
        .map_err(classify)?
        .select(vec![col(traces::TRACE_ID), col(traces::START_TIME)])
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    let mut found = BTreeMap::new();
    for batch in &batches {
        let trace_ids = utf8(batch.column(0))?;
        let trace_ids = trace_ids.as_string::<i32>();
        let starts = start_column(batch.column(1))?;
        for row in 0..batch.num_rows() {
            let trace_id = parse_trace(trace_ids.value(row))?;
            found.insert(trace_id, starts.value(row));
        }
    }
    Ok(found
        .into_iter()
        .map(|(trace_id, start_time)| TraceTarget {
            trace_id,
            start_time,
        })
        .collect())
}

async fn span_scan(
    ctx: &QueryContext,
    ids: &[(TraceId, SpanId)],
) -> Result<Vec<SpanTarget>, RunError> {
    let requested: HashSet<(TraceId, SpanId)> = ids.iter().copied().collect();
    let trace_ids: Vec<TraceId> = ids
        .iter()
        .map(|(trace_id, _)| *trace_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let targets = trace_scan(ctx, &trace_ids).await?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let starts: BTreeMap<TraceId, i64> = targets
        .iter()
        .map(|target| (target.trace_id, target.start_time))
        .collect();
    let frame = ctx
        .session()
        .table("spans")
        .await
        .map_err(classify)?
        .filter(col(spans::TRACE_ID).in_list(
            literals(targets.iter().map(|target| target.trace_id.to_hex())),
            false,
        ))
        .map_err(classify)?
        .select(vec![col(spans::TRACE_ID), col(spans::SPAN_ID)])
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    let mut found = BTreeMap::new();
    for batch in &batches {
        let trace_column = utf8(batch.column(0))?;
        let trace_column = trace_column.as_string::<i32>();
        let span_column = utf8(batch.column(1))?;
        let span_column = span_column.as_string::<i32>();
        for row in 0..batch.num_rows() {
            let trace_id = parse_trace(trace_column.value(row))?;
            let span_id = parse_span(span_column.value(row))?;
            if !requested.contains(&(trace_id, span_id)) {
                continue;
            }
            let Some(&trace_start) = starts.get(&trace_id) else {
                continue;
            };
            found.insert((trace_id, span_id), trace_start);
        }
    }
    Ok(found
        .into_iter()
        .map(|((trace_id, span_id), trace_start)| SpanTarget {
            trace_id,
            span_id,
            trace_start,
        })
        .collect())
}

async fn session_scan(ctx: &QueryContext, ids: &[&str]) -> Result<Vec<SessionTarget>, RunError> {
    let frame = target_traces(ctx)
        .await?
        .filter(
            col(traces::SESSION_ID).in_list(literals(ids.iter().map(|id| (*id).to_owned())), false),
        )
        .map_err(classify)?
        .select(vec![
            col(traces::SESSION_ID),
            col(traces::TRACE_ID),
            col(traces::START_TIME),
        ])
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    let mut found: BTreeMap<String, (i64, BTreeSet<TraceId>)> = BTreeMap::new();
    for batch in &batches {
        let sessions = utf8(batch.column(0))?;
        let sessions = sessions.as_string::<i32>();
        let trace_ids = utf8(batch.column(1))?;
        let trace_ids = trace_ids.as_string::<i32>();
        let starts = start_column(batch.column(2))?;
        for row in 0..batch.num_rows() {
            let trace_id = parse_trace(trace_ids.value(row))?;
            let start = starts.value(row);
            found
                .entry(sessions.value(row).to_owned())
                .and_modify(|(earliest, members)| {
                    *earliest = (*earliest).min(start);
                    members.insert(trace_id);
                })
                .or_insert_with(|| (start, BTreeSet::from([trace_id])));
        }
    }
    Ok(found
        .into_iter()
        .map(|(session_id, (start_time, trace_ids))| SessionTarget {
            session_id,
            start_time,
            trace_ids: trace_ids.into_iter().collect(),
        })
        .collect())
}

fn start_column(column: &ArrayRef) -> Result<&PrimitiveArray<TimestampNanosecondType>, RunError> {
    column
        .as_primitive_opt::<TimestampNanosecondType>()
        .ok_or_else(|| {
            RunError::Internal(DataFusionError::Internal(
                "start_time is not a timestamp".to_owned(),
            ))
        })
}

fn parse_trace(text: &str) -> Result<TraceId, RunError> {
    TraceId::from_str(text).map_err(|_| bad_id())
}

fn parse_span(text: &str) -> Result<SpanId, RunError> {
    SpanId::from_str(text).map_err(|_| bad_id())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::span::Span;

    use super::{
        MAX_TARGET_IDS, SessionTarget, SpanTarget, TraceTarget, session_targets, span_targets,
        trace_targets,
    };
    use crate::query_error::RunError;
    use crate::test_support::{example_query, span_row, spans_table, traces_table};
    use crate::views::{ProjectQuery, QueryContext};

    const TIMEOUT: Duration = Duration::from_secs(30);

    /// A context over `spans`; rows before `visible_from` are hidden from the views.
    fn context_of(spans: &[Span], visible_from: Option<i64>) -> QueryContext {
        let query = ProjectQuery {
            spans: spans_table(spans),
            traces: traces_table(spans),
            visible_from,
            ..example_query()
        };
        QueryContext::new(query, None).expect("context")
    }

    fn id(trace: u16) -> TraceId {
        span_row(trace, 0, 0, 0).trace_id
    }

    fn span_id(trace: u16, span: u8) -> SpanId {
        span_row(trace, span, 0, 0).span_id
    }

    #[tokio::test]
    async fn trace_targets_finds_visible_traces() {
        let ctx = context_of(&[span_row(1, 0, 1_000, 0), span_row(2, 0, 2_000, 0)], None);
        let mut found = trace_targets(&ctx, &[id(2), id(9), id(1)], TIMEOUT)
            .await
            .expect("targets");
        let mut expected = vec![
            TraceTarget {
                trace_id: id(1),
                start_time: 1_000,
            },
            TraceTarget {
                trace_id: id(2),
                start_time: 2_000,
            },
        ];
        expected.sort_by_key(|target| target.trace_id);
        found.sort_by_key(|target| target.trace_id);
        assert_eq!(found, expected);
    }

    #[tokio::test]
    async fn trace_targets_leave_out_evaluation_traces() {
        let mut eval = span_row(3, 0, 3_000, 0);
        eval.environment = Some("cetra-eval".to_owned());
        let mut experiment = span_row(4, 0, 4_000, 0);
        experiment
            .metadata
            .insert("cetra.experiment_id".to_owned(), "e1".to_owned());
        let ctx = context_of(&[eval, experiment], None);
        let found = trace_targets(&ctx, &[id(3), id(4)], TIMEOUT)
            .await
            .expect("targets");
        assert_eq!(
            found,
            [TraceTarget {
                trace_id: id(4),
                start_time: 4_000,
            }]
        );
    }

    #[tokio::test]
    async fn trace_targets_respect_the_window() {
        let ctx = context_of(
            &[span_row(1, 0, 1_000, 0), span_row(2, 0, 2_000, 0)],
            Some(1_500),
        );
        let found = trace_targets(&ctx, &[id(1), id(2)], TIMEOUT)
            .await
            .expect("targets");
        assert_eq!(
            found,
            [TraceTarget {
                trace_id: id(2),
                start_time: 2_000,
            }]
        );
    }

    #[tokio::test]
    async fn trace_targets_dedup_and_empty() {
        let ctx = context_of(&[span_row(1, 0, 1_000, 0)], None);
        let found = trace_targets(&ctx, &[id(1), id(1)], TIMEOUT)
            .await
            .expect("dedup");
        assert_eq!(
            found,
            [TraceTarget {
                trace_id: id(1),
                start_time: 1_000,
            }]
        );
        let found = trace_targets(&ctx, &[], TIMEOUT).await.expect("empty");
        assert_eq!(found, []);
    }

    #[tokio::test]
    async fn too_many_ids_are_refused() {
        let ctx = context_of(&[span_row(1, 0, 1_000, 0)], None);
        let ids = vec![id(1); MAX_TARGET_IDS + 1];
        let result = trace_targets(&ctx, &ids, TIMEOUT).await;
        assert!(
            matches!(result, Err(RunError::InvalidRequest(_))),
            "{result:?}"
        );
        let pairs = vec![(id(1), span_id(1, 0)); MAX_TARGET_IDS + 1];
        let result = span_targets(&ctx, &pairs, TIMEOUT).await;
        assert!(
            matches!(result, Err(RunError::InvalidRequest(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn span_targets_match_pairs() {
        let ctx = context_of(&[span_row(1, 0, 1_000, 0), span_row(2, 0, 2_000, 0)], None);
        let found = span_targets(
            &ctx,
            &[
                (id(1), span_id(1, 0)),
                (id(1), span_id(1, 9)),
                (id(9), span_id(9, 0)),
            ],
            TIMEOUT,
        )
        .await
        .expect("spans");
        assert_eq!(
            found,
            [SpanTarget {
                trace_id: id(1),
                span_id: span_id(1, 0),
                trace_start: 1_000,
            }]
        );
    }

    #[tokio::test]
    async fn session_targets_group_traces() {
        let mut spans = Vec::new();
        for (trace, start) in [(1, 1_000), (2, 2_000), (3, 3_000)] {
            let mut span = span_row(trace, 0, start, 0);
            span.session_id = Some("s1".to_owned());
            spans.push(span);
        }
        let mut eval = span_row(4, 0, 500, 0);
        eval.session_id = Some("s1".to_owned());
        eval.environment = Some("cetra-eval".to_owned());
        spans.push(eval);
        let ctx = context_of(&spans, None);

        let found = session_targets(&ctx, &["s1", "nope"], TIMEOUT)
            .await
            .expect("sessions");
        let mut trace_ids = vec![id(1), id(2), id(3)];
        trace_ids.sort();
        assert_eq!(
            found,
            [SessionTarget {
                session_id: "s1".to_owned(),
                start_time: 1_000,
                trace_ids,
            }]
        );
    }
}
