//! List-query clauses added beside the caller's filter: exclusions, the time
//! window, and full-text search (spec 05).

use std::ops::Not;
use std::sync::Arc;

use cetra_core::names::{environment, metadata_key};
use cetra_core::schema::{spans, traces};
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{
    Expr, Like, Operator, binary_expr, col, in_subquery, lit, not_in_subquery,
};

use super::FilterView;
use crate::query_error::RunError;
use crate::views::{QueryContext, TimeWindow, ViewName};

/// `start_time >= from AND start_time < to`.
pub(crate) fn time_window(window: TimeWindow) -> Expr {
    window.predicate(traces::START_TIME)
}

/// Default list exclusions.
///
/// The eval clause is omitted when the caller asked for eval rows or the
/// filter already names `environment`. The experiment clause is omitted when
/// the caller asked for experiment rows. `None` means both clauses are off.
///
/// The metadata lookup is planned from fixed text, so the key comes from
/// [`metadata_key`] and is not interpolated from a caller.
///
/// # Errors
///
/// Planning the metadata lookup is internal: the text is not caller input.
pub(crate) fn exclusions(
    ctx: &QueryContext,
    view: FilterView,
    include_eval: bool,
    include_experiments: bool,
    filter_references_environment: bool,
) -> Result<Option<Expr>, RunError> {
    let mut clauses = Vec::new();
    if !(include_eval || filter_references_environment) {
        clauses.push(binary_expr(
            col(traces::ENVIRONMENT),
            Operator::IsDistinctFrom,
            lit(environment::EVAL),
        ));
    }
    if !include_experiments {
        let key = metadata_key::EXPERIMENT_ID.replace('\'', "''");
        let schema = ctx.view_schema(view.view_name());
        let access = ctx
            .session()
            .state()
            .create_logical_expr(&format!("metadata['{key}']"), schema)
            .map_err(RunError::Internal)?;
        clauses.push(access.is_null());
    }
    Ok(clauses.into_iter().reduce(Expr::and))
}

/// Exclusions that no request flag and no filter can turn off (D-I-9): evaluation traces and
/// experiment traces. On `spans`, the test is on the span's trace, not on the span. `trace_ids`,
/// when given, are the only traces the caller reads: the `spans` subquery is limited to them.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for a view other than traces or spans. A failure while building
/// the subquery is [`RunError::Internal`].
pub(crate) async fn strict_exclusions(
    ctx: &QueryContext,
    view: FilterView,
    trace_ids: Option<&[String]>,
) -> Result<Expr, RunError> {
    match view {
        FilterView::Traces => traces_clean(ctx),
        FilterView::Spans => {
            let clean = traces_clean(ctx)?;
            let mut subquery = ctx
                .session()
                .table(ViewName::Traces.as_str())
                .await
                .map_err(RunError::Internal)?;
            if let Some(ids) = trace_ids {
                let literals = ids
                    .iter()
                    .map(|id| lit(id.to_ascii_lowercase()))
                    .collect::<Vec<_>>();
                subquery = subquery
                    .filter(col(traces::TRACE_ID).in_list(literals, false))
                    .map_err(RunError::Internal)?;
            }
            let subquery = subquery
                .filter(clean.not())
                .map_err(RunError::Internal)?
                .select(vec![col(traces::TRACE_ID)])
                .map_err(RunError::Internal)?;
            Ok(not_in_subquery(
                col(spans::TRACE_ID),
                Arc::new(subquery.into_unoptimized_plan()),
            ))
        }
        _ => Err(RunError::InvalidRequest(
            "strict exclusions apply to traces and spans".to_owned(),
        )),
    }
}

/// The traces conjunction of the default exclusions with both include flags off, so both
/// clauses are present.
fn traces_clean(ctx: &QueryContext) -> Result<Expr, RunError> {
    exclusions(ctx, FilterView::Traces, false, false, false)?.ok_or_else(|| {
        RunError::Internal(DataFusionError::Internal(
            "strict exclusions of traces are empty".to_owned(),
        ))
    })
}

/// Full-text search over span text.
///
/// An empty or all-whitespace query adds nothing and does not require a
/// window. Any other query needs one. On spans the expression is the `ILIKE`
/// disjunction; the caller adds [`time_window`] itself. On traces it is
/// `trace_id IN (SELECT trace_id FROM spans WHERE <search> AND <window>)`.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] when a non-empty search has no window, or the
/// view is not spans or traces. A failure while building the spans scan is
/// [`RunError::Internal`].
pub(crate) async fn search(
    view: FilterView,
    query: &str,
    window: Option<TimeWindow>,
    ctx: &QueryContext,
) -> Result<Option<Expr>, RunError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(None);
    }
    if !matches!(view, FilterView::Spans | FilterView::Traces) {
        return Err(RunError::InvalidRequest(
            "search is not available on this list".to_owned(),
        ));
    }
    let Some(window) = window else {
        return Err(RunError::InvalidRequest(
            "search needs a time range".to_owned(),
        ));
    };
    let pattern = like_pattern(query);
    let predicate = ilike(spans::INPUT, &pattern)
        .or(ilike(spans::OUTPUT, &pattern))
        .or(ilike(spans::NAME, &pattern))
        .or(ilike(spans::SYSTEM_INSTRUCTIONS, &pattern));
    if view == FilterView::Spans {
        return Ok(Some(predicate));
    }
    let spans = ctx
        .session()
        .table(FilterView::Spans.as_str())
        .await
        .map_err(RunError::Internal)?
        .filter(predicate.and(time_window(window)))
        .map_err(RunError::Internal)?
        .select(vec![col(spans::TRACE_ID)])
        .map_err(RunError::Internal)?;
    Ok(Some(in_subquery(
        col(traces::TRACE_ID),
        Arc::new(spans.into_unoptimized_plan()),
    )))
}

/// `%<escaped>%`, with `\`, `%` and `_` escaped for `ESCAPE '\'`.
fn like_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for character in query.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    format!("%{escaped}%")
}

fn ilike(column: &str, pattern: &str) -> Expr {
    Expr::Like(Like::new(
        false,
        Box::new(col(column)),
        Box::new(lit(pattern)),
        Some('\\'),
        true,
    ))
}
