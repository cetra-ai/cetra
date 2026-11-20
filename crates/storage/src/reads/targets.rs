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
use futures::StreamExt;

use super::deletions::{bad_id, utf8};
use super::sessions::filtered_session_ids;
use super::traces::scope_frame;
use super::{ListScope, present, scoped_traces};
use crate::filter::{Filter, FilterView};
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow};

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

/// A span query for [`filter_span_targets`]: the spans list's filter, search and window.
#[derive(Debug, Clone, Copy)]
pub struct SpanTargetFilter<'a> {
    /// Filter text over `spans`. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Full-text search over the spans. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// `start_time ∈ [from, to)` of the span itself.
    pub window: TimeWindow,
    /// Keep spans of experiment traces, as the spans list does.
    pub include_experiments: bool,
    /// Most targets returned. The server passes its cap; one more match is reported as `capped`.
    pub limit: usize,
    /// Time allowed for the whole call.
    pub timeout: Duration,
}

/// A session query for [`filter_session_targets`]: the sessions list's filter and window. The
/// sessions list has no search, so this request has none.
#[derive(Debug, Clone, Copy)]
pub struct SessionTargetFilter<'a> {
    /// Filter text over `sessions`. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// The sessions of traces that start in `[from, to)`, as the sessions list selects them.
    pub window: TimeWindow,
    /// Keep sessions of experiment traces, as the sessions list does.
    pub include_experiments: bool,
    /// Most targets returned. The server passes its cap; one more match is reported as `capped`.
    pub limit: usize,
    /// Time allowed for the whole call.
    pub timeout: Duration,
}

/// Targets found, and whether more than `limit` matched before any target was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredTargets<T> {
    /// At most `limit` targets, sorted by id.
    pub targets: Vec<T>,
    /// True when more than `limit` rows matched the filter.
    pub capped: bool,
}

/// The visible spans the spans list selects for `request`, each with its trace's start time
/// (the trace start, even when it is before the window). Distinct `(trace, span)` pairs, sorted.
/// A span matched more than once (copies) is one target. Evaluation spans are never returned.
/// The scan stops once `limit + 1` distinct pairs are seen; `capped` says so, and the first
/// `limit` pairs by id are kept.
///
/// # Errors
///
/// [`RunError::Invalid`] for filter text; [`RunError::InvalidRequest`] for a bad search;
/// [`RunError::Timeout`]; the query's other errors as `ctx.fail` classifies them.
pub async fn filter_span_targets(
    ctx: &QueryContext,
    request: SpanTargetFilter<'_>,
) -> Result<FilteredTargets<SpanTarget>, RunError> {
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(FilterView::Spans, text)?),
        None => None,
    };
    bounded(
        request.timeout,
        span_filter_scan(
            ctx,
            filter,
            request.search,
            request.window,
            request.include_experiments,
            request.limit,
        ),
    )
    .await
}

/// The visible sessions the sessions list selects for `request`, each with its earliest visible
/// trace's start and its visible trace ids, as [`session_targets`] answers them (no window, no
/// evaluation traces). Sorted by session id. A session the scan finds but the lookup does not
/// answer (only evaluation traces left) is dropped. The scan stops at `limit + 1` sessions;
/// `capped` says so, and the first `limit` are kept.
///
/// # Errors
///
/// As [`filter_span_targets`].
pub async fn filter_session_targets(
    ctx: &QueryContext,
    request: SessionTargetFilter<'_>,
) -> Result<FilteredTargets<SessionTarget>, RunError> {
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(FilterView::Sessions, text)?),
        None => None,
    };
    bounded(
        request.timeout,
        session_filter_scan(
            ctx,
            filter,
            request.window,
            request.include_experiments,
            request.limit,
        ),
    )
    .await
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

async fn span_filter_scan(
    ctx: &QueryContext,
    filter: Option<Filter>,
    search_text: Option<&str>,
    window: TimeWindow,
    include_experiments: bool,
    limit: usize,
) -> Result<FilteredTargets<SpanTarget>, RunError> {
    let scope = ListScope {
        window: Some(window),
        include_eval: false,
        include_experiments,
    };
    let frame = scope_frame(
        ctx,
        FilterView::Spans,
        "spans",
        filter,
        search_text,
        scope,
        false,
    )
    .await?
    .select(vec![col(spans::TRACE_ID), col(spans::SPAN_ID)])
    .map_err(classify)?;
    let mut stream = frame
        .execute_stream()
        .await
        .map_err(|error| ctx.fail(error))?;
    let mut pairs = BTreeSet::new();
    'read: while let Some(batch) = stream.next().await {
        let batch = batch.map_err(|error| ctx.fail(error))?;
        let trace_column = utf8(batch.column(0))?;
        let trace_column = trace_column.as_string::<i32>();
        let span_column = utf8(batch.column(1))?;
        let span_column = span_column.as_string::<i32>();
        for row in 0..batch.num_rows() {
            pairs.insert((
                parse_trace(trace_column.value(row))?,
                parse_span(span_column.value(row))?,
            ));
            if pairs.len() > limit {
                break 'read;
            }
        }
    }
    let capped = pairs.len() > limit;
    let kept: Vec<(TraceId, SpanId)> = pairs.into_iter().take(limit).collect();
    let trace_ids: BTreeSet<TraceId> = kept.iter().map(|(trace_id, _)| *trace_id).collect();
    let trace_ids: Vec<TraceId> = trace_ids.into_iter().collect();
    let mut starts = BTreeMap::new();
    for chunk in trace_ids.chunks(MAX_TARGET_IDS) {
        for target in trace_scan(ctx, chunk).await? {
            starts.insert(target.trace_id, target.start_time);
        }
    }
    let targets = kept
        .into_iter()
        .filter_map(|(trace_id, span_id)| {
            starts.get(&trace_id).map(|&trace_start| SpanTarget {
                trace_id,
                span_id,
                trace_start,
            })
        })
        .collect();
    Ok(FilteredTargets { targets, capped })
}

async fn session_filter_scan(
    ctx: &QueryContext,
    filter: Option<Filter>,
    window: TimeWindow,
    include_experiments: bool,
    limit: usize,
) -> Result<FilteredTargets<SessionTarget>, RunError> {
    let mut ids = filtered_session_ids(ctx, filter, window, include_experiments, limit + 1).await?;
    let capped = ids.len() > limit;
    ids.truncate(limit);
    let mut targets = Vec::new();
    for chunk in ids.chunks(MAX_TARGET_IDS) {
        let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
        targets.extend(session_scan(ctx, &refs).await?);
    }
    targets.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(FilteredTargets { targets, capped })
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

pub(super) fn start_column(
    column: &ArrayRef,
) -> Result<&PrimitiveArray<TimestampNanosecondType>, RunError> {
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

    use std::collections::BTreeSet;
    use std::str::FromStr;

    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::schema::spans;
    use cetra_core::span::Span;

    use super::{
        FilteredTargets, MAX_TARGET_IDS, SessionTarget, SessionTargetFilter, SpanTarget,
        SpanTargetFilter, TraceTarget, filter_session_targets, filter_span_targets,
        session_targets, span_targets, trace_targets,
    };
    use crate::query_error::RunError;
    use crate::reads::deletions::utf8;
    use crate::reads::{ListScope, Page, SortOrder, SpanListRequest, SpanSort, span_list};
    use crate::test_support::{example_query, span_row, spans_table, traces_table};
    use crate::views::{ProjectQuery, QueryContext, TimeWindow};
    use arrow::array::AsArray;

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

    fn wide() -> TimeWindow {
        TimeWindow::new(0, 1_000_000).expect("window")
    }

    fn span_filter<'a>(
        filter: Option<&'a str>,
        search: Option<&'a str>,
        limit: usize,
    ) -> SpanTargetFilter<'a> {
        SpanTargetFilter {
            filter,
            search,
            window: wide(),
            include_experiments: false,
            limit,
            timeout: TIMEOUT,
        }
    }

    /// The `(trace, span)` pairs of a span list page.
    fn listed_pairs(page: &Page) -> BTreeSet<(TraceId, SpanId)> {
        let mut pairs = BTreeSet::new();
        for batch in &page.batches {
            let traces =
                utf8(batch.column_by_name(spans::TRACE_ID).expect("trace column")).expect("cast");
            let span_ids =
                utf8(batch.column_by_name(spans::SPAN_ID).expect("span column")).expect("cast");
            let (traces, span_ids) = (traces.as_string::<i32>(), span_ids.as_string::<i32>());
            for row in 0..batch.num_rows() {
                pairs.insert((
                    TraceId::from_str(traces.value(row)).expect("trace id"),
                    SpanId::from_str(span_ids.value(row)).expect("span id"),
                ));
            }
        }
        pairs
    }

    fn pairs_of(targets: &FilteredTargets<SpanTarget>) -> BTreeSet<(TraceId, SpanId)> {
        targets
            .targets
            .iter()
            .map(|target| (target.trace_id, target.span_id))
            .collect()
    }

    #[tokio::test]
    async fn filter_span_targets_follow_the_span_list() {
        let mut chat = span_row(1, 1, 1_500, 0);
        chat.name = "chat".to_owned();
        let mut tool = span_row(2, 0, 2_000, 0);
        tool.name = "tool".to_owned();
        let mut late = span_row(3, 0, 9_000, 0);
        late.name = "chat".to_owned();
        let ctx = context_of(&[span_row(1, 0, 1_000, 0), chat, tool, late], None);
        let window = TimeWindow::new(1_200, 5_000).expect("window");
        let page = span_list(
            &ctx,
            SpanListRequest {
                filter: Some("name = 'chat'"),
                search: None,
                scope: ListScope {
                    window: Some(window),
                    include_eval: false,
                    include_experiments: false,
                },
                strict_exclusions: false,
                sort: SpanSort::StartTime,
                order: SortOrder::Asc,
                limit: 100,
                cursor: None,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("list");
        let listed = listed_pairs(&page);
        let found = filter_span_targets(
            &ctx,
            SpanTargetFilter {
                filter: Some("name = 'chat'"),
                search: None,
                window,
                include_experiments: false,
                limit: 100,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("targets");
        assert!(!found.capped);
        assert_eq!(pairs_of(&found), listed);
        assert_eq!(listed, BTreeSet::from([(id(1), span_id(1, 1))]));
        // The trace starts before the window; the target keeps the trace's own start.
        assert_eq!(found.targets[0].trace_start, 1_000);
    }

    #[tokio::test]
    async fn filter_span_targets_stop_at_the_limit() {
        let spans: Vec<Span> = (1..=5).map(|trace| span_row(trace, 0, 1_000, 0)).collect();
        let ctx = context_of(&spans, None);
        let found = filter_span_targets(&ctx, span_filter(None, None, 2))
            .await
            .expect("targets");
        assert_eq!(found.targets.len(), 2);
        assert!(found.capped);
        let exact = filter_span_targets(&ctx, span_filter(None, None, 5))
            .await
            .expect("exact");
        assert_eq!(exact.targets.len(), 5);
        assert!(!exact.capped);
    }

    #[tokio::test]
    async fn filter_span_targets_dedup_copies() {
        // The same span stored twice (a re-send not yet compacted).
        let spans = [span_row(1, 0, 1_000, 10), span_row(1, 0, 1_000, 20)];
        let ctx = context_of(&spans, None);
        let found = filter_span_targets(&ctx, span_filter(None, None, 10))
            .await
            .expect("targets");
        assert_eq!(found.targets.len(), 1);
        assert!(!found.capped);
    }

    #[tokio::test]
    async fn filter_span_targets_exclude_eval_and_honour_experiments() {
        let mut eval = span_row(3, 0, 3_000, 0);
        eval.environment = Some("cetra-eval".to_owned());
        let mut experiment = span_row(4, 0, 4_000, 0);
        experiment
            .metadata
            .insert("cetra.experiment_id".to_owned(), "e1".to_owned());
        let ctx = context_of(&[span_row(1, 0, 1_000, 0), eval, experiment], None);
        let found = filter_span_targets(&ctx, span_filter(None, None, 10))
            .await
            .expect("default");
        assert_eq!(pairs_of(&found), BTreeSet::from([(id(1), span_id(1, 0))]));
        let mut request = span_filter(None, None, 10);
        request.include_experiments = true;
        let found = filter_span_targets(&ctx, request)
            .await
            .expect("experiments");
        assert_eq!(
            pairs_of(&found),
            BTreeSet::from([(id(1), span_id(1, 0)), (id(4), span_id(4, 0))])
        );
    }

    #[tokio::test]
    async fn filter_span_targets_use_the_trace_start() {
        let root = span_row(1, 0, 1_000, 0);
        let child = span_row(1, 1, 1_600, 0);
        let ctx = context_of(&[root, child], None);
        let window = TimeWindow::new(1_500, 3_000).expect("window");
        let mut request = span_filter(None, None, 10);
        request.window = window;
        let found = filter_span_targets(&ctx, request).await.expect("targets");
        assert_eq!(found.targets.len(), 1);
        let trace = trace_targets(&ctx, &[id(1)], TIMEOUT).await.expect("trace");
        assert_eq!(found.targets[0].trace_start, trace[0].start_time);
        assert!(found.targets[0].trace_start < 1_500);
    }

    #[tokio::test]
    async fn filter_span_targets_search_narrows() {
        let mut hit = span_row(1, 0, 1_000, 0);
        hit.input = Some("refund please".to_owned());
        let ctx = context_of(&[hit, span_row(2, 0, 2_000, 0)], None);
        let found = filter_span_targets(&ctx, span_filter(None, Some("refund"), 10))
            .await
            .expect("search");
        assert_eq!(pairs_of(&found), BTreeSet::from([(id(1), span_id(1, 0))]));
    }

    #[tokio::test]
    async fn filter_session_targets_follow_the_session_list() {
        let mut spans = Vec::new();
        for (trace, start, session) in [(1, 1_000, "s1"), (2, 2_000, "s2"), (3, 3_000, "s1")] {
            let mut span = span_row(trace, 0, start, 0);
            span.session_id = Some(session.to_owned());
            spans.push(span);
        }
        let ctx = context_of(&spans, None);
        let found = filter_session_targets(
            &ctx,
            SessionTargetFilter {
                filter: None,
                window: wide(),
                include_experiments: false,
                limit: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("sessions");
        assert!(!found.capped);
        let ids: Vec<&str> = found
            .targets
            .iter()
            .map(|target| target.session_id.as_str())
            .collect();
        assert_eq!(ids, ["s1", "s2"]);
    }

    #[tokio::test]
    async fn filter_session_targets_use_the_earliest_visible_trace() {
        let mut early = span_row(1, 0, 1_000, 0);
        early.session_id = Some("s1".to_owned());
        let mut inside = span_row(2, 0, 2_000, 0);
        inside.session_id = Some("s1".to_owned());
        let ctx = context_of(&[early, inside], None);
        let found = filter_session_targets(
            &ctx,
            SessionTargetFilter {
                filter: None,
                window: TimeWindow::new(1_500, 3_000).expect("window"),
                include_experiments: false,
                limit: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("sessions");
        let mut trace_ids = vec![id(1), id(2)];
        trace_ids.sort();
        assert_eq!(
            found.targets,
            [SessionTarget {
                session_id: "s1".to_owned(),
                start_time: 1_000,
                trace_ids,
            }]
        );
    }

    #[tokio::test]
    async fn filter_session_targets_stop_at_the_limit() {
        let mut spans = Vec::new();
        for trace in 1..=3 {
            let mut span = span_row(trace, 0, 1_000, 0);
            span.session_id = Some(format!("s{trace}"));
            spans.push(span);
        }
        let ctx = context_of(&spans, None);
        let found = filter_session_targets(
            &ctx,
            SessionTargetFilter {
                filter: None,
                window: wide(),
                include_experiments: false,
                limit: 2,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("sessions");
        assert_eq!(found.targets.len(), 2);
        assert!(found.capped);
    }

    #[tokio::test]
    async fn filter_session_targets_leave_out_evaluation_sessions() {
        // The sessions view has no `environment` column, so a filter cannot keep evaluation
        // traces in; the scan leaves an evaluation-only session out before the lookup.
        let mut eval = span_row(1, 0, 1_000, 0);
        eval.session_id = Some("eval-only".to_owned());
        eval.environment = Some("cetra-eval".to_owned());
        let mut plain = span_row(2, 0, 2_000, 0);
        plain.session_id = Some("plain".to_owned());
        let ctx = context_of(&[eval, plain], None);
        let found = filter_session_targets(
            &ctx,
            SessionTargetFilter {
                filter: None,
                window: wide(),
                include_experiments: false,
                limit: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("sessions");
        let ids: Vec<&str> = found
            .targets
            .iter()
            .map(|target| target.session_id.as_str())
            .collect();
        assert_eq!(ids, ["plain"]);
        assert!(!found.capped);
    }

    #[tokio::test]
    async fn filter_targets_refuse_a_bad_filter() {
        let ctx = context_of(&[span_row(1, 0, 1_000, 0)], None);
        let result = filter_span_targets(&ctx, span_filter(Some("name = "), None, 10)).await;
        assert!(matches!(result, Err(RunError::Invalid(_))), "{result:?}");
    }
}
