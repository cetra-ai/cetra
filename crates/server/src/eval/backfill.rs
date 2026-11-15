//! Online rule backfill (D-I-25, D-I-38 to D-I-42): validation, the paged reads, sampling, the
//! already-scored check, the estimate and the runner that enqueues the jobs.

use std::collections::HashSet;
use std::time::Duration;

use llmobs_core::ids::{OrgId, ProjectId};
use llmobs_core::score::ScoreTarget;
use llmobs_eval::OutputScore;
use llmobs_otel::PriceRow;
use llmobs_storage::error::QueryError;
use llmobs_storage::{
    ListScope, MAX_PAGE, QueryContext, RunError, SortOrder, SpanListRequest, SpanSort, TimeWindow,
    TraceListRequest, TraceSort, scores_by_id, span_list, trace_list,
};
use uuid::Uuid;

use crate::api::common::FieldError;
use crate::api::convert::{self, ConvertError, MatchRow, match_rows};
use crate::api::evaluators::EvaluatorKind;
use crate::api::online_rules::RuleTarget;
use crate::entitlements::{Visibility, query_options};
use crate::eval::budget::{self, DAY_NS};
use crate::eval::rule_check::field_error;
use crate::eval::sampling;
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::meta::backfills::{
    self, BackfillEnd, BackfillRow, PageWrite, STATUS_DONE, STATUS_FAILED,
};
use crate::meta::eval_jobs::{KIND_BACKFILL, NewJob};
use crate::meta::evaluators::EvaluatorRow;
use crate::meta::online_rules::RuleRow;
use crate::meta::{MetaError, evaluators, online_rules, projects};
use crate::scores;

/// Most rows one page reads (storage's `MAX_PAGE`).
const PAGE: usize = MAX_PAGE;
/// Time allowed for one page read or one scored-ids read.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Most targets one backfill reads (D-I-25).
pub(crate) const MAX_TARGETS: u32 = 10_000;
/// Widest window (D-I-25).
pub(crate) const MAX_WINDOW_NS: i64 = 30 * DAY_NS;
/// `to` must be at least this far in the past (D-I-25).
pub(crate) const SETTLE_NS: i64 = 5 * 60 * 1_000_000_000;
/// Characters the estimate counts for each mapped variable (D-I-25).
const CHARS_PER_VARIABLE: usize = 2_000;

/// A rule was disabled while its backfill ran (D-I-41).
pub(crate) const ERROR_RULE_DISABLED: &str = "rule disabled";
/// Storage was busy or a read timed out (D-I-41).
pub(crate) const ERROR_STORAGE_UNAVAILABLE: &str = "storage unavailable";
/// The rule's stored filter no longer parses (D-I-41).
pub(crate) const ERROR_INVALID_FILTER: &str = "invalid filter";
/// Any other read or conversion failure (D-I-41).
pub(crate) const ERROR_STORAGE_FAILED: &str = "storage read failed";
/// The stored rule or its project does not decode (D-I-41).
pub(crate) const ERROR_RULE: &str = "rule could not be read";
/// The rule's evaluator row is missing or does not decode (D-I-41).
pub(crate) const ERROR_EVALUATOR: &str = "evaluator could not be read";

/// A checked backfill window and its limits (D-I-42).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Window {
    pub(crate) from: i64,
    pub(crate) to: i64,
    pub(crate) max_targets: u32,
    pub(crate) sample_rate: f64,
}

/// Checks a request at `now` (§ Validation). An absent `sample_rate` takes `rule_rate`.
///
/// # Errors
///
/// Every failing field, in the order of the validation table.
pub(crate) fn validate(
    from: i64,
    to: i64,
    max_targets: u32,
    sample_rate: Option<f64>,
    rule_rate: f64,
    now: i64,
) -> Result<Window, Vec<FieldError>> {
    let mut errors = Vec::new();
    if to > now.saturating_sub(SETTLE_NS) {
        errors.push(field_error("to", "to must be at least 5 minutes ago"));
    }
    if from >= to {
        errors.push(field_error("from", "from must be before to"));
    } else if to.saturating_sub(from) > MAX_WINDOW_NS {
        errors.push(field_error("from", "the window is at most 30 days"));
    }
    if !(1..=MAX_TARGETS).contains(&max_targets) {
        errors.push(field_error("max_targets", "max_targets must be 1 to 10000"));
    }
    let sample_rate = sample_rate.unwrap_or(rule_rate);
    if !(sample_rate.is_finite() && sample_rate > 0.0 && sample_rate <= 1.0) {
        errors.push(field_error(
            "sample_rate",
            "sample_rate must be above 0 and at most 1",
        ));
    }
    if errors.is_empty() {
        Ok(Window {
            from,
            to,
            max_targets,
            sample_rate,
        })
    } else {
        Err(errors)
    }
}

/// What a backfill needs of its rule.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BackfillRule {
    pub(crate) id: Uuid,
    pub(crate) project: ProjectId,
    pub(crate) org: OrgId,
    pub(crate) target: RuleTarget,
    /// The rule's filter, as stored.
    pub(crate) filter: Option<String>,
}

impl BackfillRule {
    /// From a stored rule and its project's org.
    ///
    /// # Errors
    ///
    /// `MetaError::Corrupt { table: "online_rules", column: "target" }` for a stored target
    /// that `RuleTarget::from_name` refuses.
    pub(crate) fn new(rule: &RuleRow, org: OrgId) -> Result<Self, MetaError> {
        let target = RuleTarget::from_name(&rule.target).ok_or(MetaError::Corrupt {
            table: "online_rules",
            column: "target",
        })?;
        Ok(Self {
            id: rule.id,
            project: rule.project_id,
            org,
            target,
            filter: rule.filter.clone(),
        })
    }
}

/// A backfill read that failed (D-I-41).
#[derive(Debug, thiserror::Error)]
pub(crate) enum BackfillReadError {
    #[error("storage query could not start: {0}")]
    Query(#[from] QueryError),
    #[error("storage read failed: {0}")]
    Run(#[from] RunError),
    #[error("stored rows could not be converted: {0}")]
    Convert(#[from] ConvertError),
}

impl BackfillReadError {
    /// The error a failed backfill stores (§ Stored error strings).
    pub(crate) fn stored(&self) -> &'static str {
        match self {
            Self::Query(QueryError::Busy) | Self::Run(RunError::Timeout) => {
                ERROR_STORAGE_UNAVAILABLE
            }
            Self::Run(RunError::Invalid(_)) => ERROR_INVALID_FILTER,
            Self::Query(_) | Self::Run(_) | Self::Convert(_) => ERROR_STORAGE_FAILED,
        }
    }
}

impl From<BackfillReadError> for ApiError {
    fn from(error: BackfillReadError) -> Self {
        match error {
            BackfillReadError::Query(error) => Self::from(error),
            BackfillReadError::Run(error) => Self::from(error),
            BackfillReadError::Convert(error) => Self::from(error),
        }
    }
}

/// One page of matches, newest first, and the query context it was read with (the scored
/// check reuses it).
pub(crate) struct TargetPage {
    pub(crate) ctx: QueryContext,
    pub(crate) rows: Vec<MatchRow>,
}

/// Walks a rule's matches in a window, newest first, page by page, up to `limit` rows
/// (D-I-40). Each page opens its own query context, so a long run never holds one storage
/// snapshot.
pub(crate) struct Pager<'a> {
    state: &'a AppState,
    rule: &'a BackfillRule,
    window: TimeWindow,
    limit: usize,
    page: usize,
    read: usize,
    cursor: Option<String>,
    done: bool,
}

impl<'a> Pager<'a> {
    pub(crate) fn new(
        state: &'a AppState,
        rule: &'a BackfillRule,
        window: TimeWindow,
        limit: usize,
    ) -> Self {
        Self {
            state,
            rule,
            window,
            limit,
            page: PAGE,
            read: 0,
            cursor: None,
            done: false,
        }
    }

    /// A smaller page, so tests walk several pages over a few traces.
    #[cfg(test)]
    fn with_page(mut self, page: usize) -> Self {
        self.page = page;
        self
    }

    /// The next page, or `None` once `limit` rows were read or the list ended.
    ///
    /// # Errors
    ///
    /// [`BackfillReadError`] when the query cannot start, the read fails or a row does not
    /// convert.
    pub(crate) async fn next_page(&mut self) -> Result<Option<TargetPage>, BackfillReadError> {
        if self.done || self.read >= self.limit {
            return Ok(None);
        }
        let ask = self.page.min(self.limit - self.read);
        #[cfg(test)]
        if let Some(error) = faults::take_read() {
            return Err(BackfillReadError::Run(error));
        }
        let source = self
            .state
            .storage
            .query(
                self.rule.project,
                query_options(Visibility::window(self.state, self.rule.org), None),
            )
            .await?;
        let ctx = QueryContext::new(source, None)?;
        let scope = ListScope {
            window: Some(self.window),
            include_eval: false,
            include_experiments: false,
        };
        let filter = self.rule.filter.as_deref();
        let cursor = self.cursor.as_deref();
        let page = match self.rule.target {
            RuleTarget::Trace => {
                trace_list(
                    &ctx,
                    TraceListRequest {
                        filter,
                        search: None,
                        scope,
                        strict_exclusions: true,
                        sort: TraceSort::StartTime,
                        order: SortOrder::Desc,
                        limit: ask,
                        cursor,
                        timeout: READ_TIMEOUT,
                    },
                )
                .await?
            }
            RuleTarget::Span => {
                span_list(
                    &ctx,
                    SpanListRequest {
                        filter,
                        search: None,
                        scope,
                        strict_exclusions: true,
                        sort: SpanSort::StartTime,
                        order: SortOrder::Desc,
                        limit: ask,
                        cursor,
                        timeout: READ_TIMEOUT,
                    },
                )
                .await?
            }
        };
        let rows = match_rows(&page.batches, self.rule.target == RuleTarget::Span)?;
        self.read += rows.len();
        self.done = page.next_cursor.is_none() || rows.is_empty();
        self.cursor = page.next_cursor;
        Ok(Some(TargetPage { ctx, rows }))
    }
}

/// The rows `sampling::sampled` keeps for `rule` at `rate`, in order (D-I-10, D-I-40).
pub(crate) fn sampled_rows(rule: Uuid, rate: f64, rows: Vec<MatchRow>) -> Vec<MatchRow> {
    rows.into_iter()
        .filter(|row| sampling::sampled(rate, rule, row.trace_id, row.span_id))
        .collect()
}

/// What the scored check needs of the rule's evaluator (D-I-40).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scoring {
    /// The evaluator's current output score name.
    pub(crate) name: String,
    pub(crate) kind: EvaluatorKind,
}

impl Scoring {
    /// From the stored evaluator, without opening a webhook's sealed values. `None` when the
    /// kind or the output score does not decode.
    pub(crate) fn from_row(row: &EvaluatorRow) -> Option<Self> {
        let kind = EvaluatorKind::from_name(&row.kind)?;
        let output: OutputScore = serde_json::from_str(&row.output_score).ok()?;
        Some(Self {
            name: output.name,
            kind,
        })
    }
}

/// `rows` without the targets that already have this rule's score (one `scores_by_id` read).
/// No rows, no read.
///
/// # Errors
///
/// [`BackfillReadError`] when the read fails or a score row does not convert.
pub(crate) async fn unscored(
    ctx: &QueryContext,
    rule: &BackfillRule,
    scoring: &Scoring,
    rows: Vec<MatchRow>,
) -> Result<Vec<MatchRow>, BackfillReadError> {
    if rows.is_empty() {
        return Ok(rows);
    }
    let ids: Vec<Uuid> = rows
        .iter()
        .map(|row| {
            let target = match row.span_id {
                Some(span_id) => ScoreTarget::Span {
                    trace_id: row.trace_id,
                    span_id,
                },
                None => ScoreTarget::Trace {
                    trace_id: row.trace_id,
                },
            };
            scores::rule_score_id(rule.project, &target, &scoring.name, scoring.kind, rule.id)
        })
        .collect();
    let texts: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    let batches = scores_by_id(ctx, &refs, READ_TIMEOUT).await?;
    let scored: HashSet<Uuid> = convert::scores(&batches, rule.project)?
        .into_iter()
        .map(|score| score.score_id)
        .collect();
    Ok(rows
        .into_iter()
        .zip(ids)
        .filter(|(_, id)| !scored.contains(id))
        .map(|(row, _)| row)
        .collect())
}

/// What the estimate needs of a judge evaluator (D-I-42).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JudgeCost {
    /// `chars().count()` of the prompt template.
    pub(crate) template_chars: usize,
    /// Mapped variables of the rule.
    pub(crate) variables: usize,
    pub(crate) max_output_tokens: u32,
    /// The model's price (`budget::model_price`), `None` when it has none.
    pub(crate) price: Option<PriceRow>,
}

/// Tokens and cost of the sampled targets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CostEstimate {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cost_usd: Option<f64>,
}

/// What a backfill would do (D-I-38, D-I-42).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Estimate {
    pub(crate) matched: u64,
    pub(crate) capped: bool,
    pub(crate) sampled: u64,
    /// Judges only.
    pub(crate) cost: Option<CostEstimate>,
}

/// D-I-25's estimate for `sampled` targets.
fn estimate_cost(sampled: u64, judge: &JudgeCost) -> CostEstimate {
    let chars = judge
        .template_chars
        .saturating_add(CHARS_PER_VARIABLE.saturating_mul(judge.variables));
    let per_input = u64::try_from(chars.div_ceil(4)).unwrap_or(u64::MAX);
    let cost_usd = judge
        .price
        .as_ref()
        .map(|price| sampled as f64 * budget::estimate_usd(chars, judge.max_output_tokens, price))
        .filter(|cost| cost.is_finite());
    CostEstimate {
        input_tokens: sampled.saturating_mul(per_input),
        output_tokens: sampled.saturating_mul(u64::from(judge.max_output_tokens)),
        cost_usd,
    }
}

/// Reads up to `max_targets + 1` rows; `matched` is at most `max_targets`, `capped` says more
/// matched, and `sampled` counts the kept ones among the first `max_targets`.
///
/// # Errors
///
/// [`BackfillReadError`] from a page read.
pub(crate) async fn estimate(
    state: &AppState,
    rule: &BackfillRule,
    window: Window,
    judge: Option<&JudgeCost>,
) -> Result<Estimate, BackfillReadError> {
    let time = TimeWindow::new(window.from, window.to)?;
    let max = usize::try_from(window.max_targets).unwrap_or(usize::MAX);
    let mut pager = Pager::new(state, rule, time, max.saturating_add(1));
    let mut rows = Vec::new();
    while let Some(page) = pager.next_page().await? {
        rows.extend(page.rows);
    }
    let capped = rows.len() > max;
    rows.truncate(max);
    let matched = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let sampled =
        u64::try_from(sampled_rows(rule.id, window.sample_rate, rows).len()).unwrap_or(u64::MAX);
    Ok(Estimate {
        matched,
        capped,
        sampled,
        cost: judge.map(|judge| estimate_cost(sampled, judge)),
    })
}

/// How one run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// Every page was read; the backfill is `done`.
    Done,
    /// Stopped early; the backfill is `failed`.
    Failed,
    /// The rule or its project is gone; nothing left to update.
    Gone,
    /// The server is stopping; the backfill stays `running` (D-I-39).
    Stopped,
}

/// Runs every `running` backfill of an active project, oldest first, one at a time (D-I-39).
/// Returns how many ended `Done` or `Failed`. One backfill's `meta.db` error is logged and the
/// pass goes on to the next (D-I-41).
///
/// # Errors
///
/// [`MetaError`] only when the list of running backfills cannot be read.
pub(crate) async fn backfill_once(state: &AppState) -> Result<usize, MetaError> {
    let rows = backfills::running_backfills(state.meta.reader()).await?;
    let mut finished = 0;
    for row in &rows {
        if state.eval.is_stopped() {
            break;
        }
        match run_backfill(state, row).await {
            Ok(RunEnd::Done | RunEnd::Failed) => finished += 1,
            Ok(RunEnd::Gone) => {
                tracing::debug!(
                    backfill_id = %row.id,
                    rule_id = %row.rule_id,
                    "a backfill's rule is gone"
                );
            }
            Ok(RunEnd::Stopped) => {}
            Err(error) => tracing::error!(
                backfill_id = %row.id,
                error = %error,
                "a backfill pass failed; it runs again from the start next pass"
            ),
        }
    }
    Ok(finished)
}

/// Splits a read of a stored row into its value, or the error text a backfill fails with when the
/// row does not decode (D-I-41). Any other `meta.db` error is returned as the pass error.
fn decoded<T>(
    read: Result<T, MetaError>,
    text: &'static str,
) -> Result<Result<T, &'static str>, MetaError> {
    match read {
        Ok(value) => Ok(Ok(value)),
        Err(MetaError::Corrupt { .. }) => Ok(Err(text)),
        Err(error) => Err(error),
    }
}

/// Runs one backfill to its end (D-I-40, D-I-41). Every page is one `meta.db` write.
async fn run_backfill(state: &AppState, row: &BackfillRow) -> Result<RunEnd, MetaError> {
    let reader = state.meta.reader();
    let project = match decoded(
        projects::project_by_id(reader, row.project_id).await,
        ERROR_RULE,
    )? {
        Ok(Some(project)) => project,
        Ok(None) => return Ok(RunEnd::Gone),
        Err(text) => return finish(state, row, BackfillEnd::Failed(text), 0, 0).await,
    };
    let rule_row = match decoded(
        online_rules::get_rule(reader, row.project_id, row.rule_id).await,
        ERROR_RULE,
    )? {
        Ok(Some(rule)) => rule,
        Ok(None) => return Ok(RunEnd::Gone),
        Err(text) => return finish(state, row, BackfillEnd::Failed(text), 0, 0).await,
    };
    let rule = match decoded(BackfillRule::new(&rule_row, project.org_id), ERROR_RULE)? {
        Ok(rule) => rule,
        Err(text) => return finish(state, row, BackfillEnd::Failed(text), 0, 0).await,
    };
    let scoring = match decoded(
        evaluators::get_evaluator(reader, row.project_id, rule_row.evaluator_id).await,
        ERROR_EVALUATOR,
    )? {
        Ok(Some(evaluator)) => Scoring::from_row(&evaluator),
        Ok(None) => None,
        Err(text) => return finish(state, row, BackfillEnd::Failed(text), 0, 0).await,
    };
    let Some(scoring) = scoring else {
        return finish(state, row, BackfillEnd::Failed(ERROR_EVALUATOR), 0, 0).await;
    };
    let Ok(time) = TimeWindow::new(row.from_ns, row.to_ns) else {
        return finish(state, row, BackfillEnd::Failed(ERROR_STORAGE_FAILED), 0, 0).await;
    };

    let mut pager = Pager::new(
        state,
        &rule,
        time,
        usize::try_from(row.max_targets).unwrap_or(usize::MAX),
    );
    let mut matched: u64 = 0;
    let mut enqueued: u64 = 0;
    loop {
        if state.eval.is_stopped() {
            return Ok(RunEnd::Stopped);
        }
        let page = match pager.next_page().await {
            Ok(Some(page)) => page,
            Ok(None) => return finish(state, row, BackfillEnd::Done, matched, enqueued).await,
            Err(error) => return fail_read(state, row, &error, matched, enqueued).await,
        };
        matched = matched.saturating_add(u64::try_from(page.rows.len()).unwrap_or(u64::MAX));
        let kept = sampled_rows(rule.id, row.sample_rate, page.rows);
        let kept = match unscored(&page.ctx, &rule, &scoring, kept).await {
            Ok(kept) => kept,
            Err(error) => return fail_read(state, row, &error, matched, enqueued).await,
        };
        let jobs: Vec<NewJob> = kept
            .into_iter()
            .map(|target| NewJob {
                id: Uuid::now_v7(),
                project_id: row.project_id,
                kind: KIND_BACKFILL,
                rule_id: Some(rule.id),
                evaluator_id: rule_row.evaluator_id,
                variable_mapping: None,
                trace_id: target.trace_id,
                span_id: target.span_id,
                trace_start_ns: target.start_time,
            })
            .collect();
        let (backfill, rule_id, now) = (row.id, rule.id, state.clock.now_ns());
        let written = state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    backfills::write_page(conn, backfill, rule_id, &jobs, matched, now).await
                })
            })
            .await?;
        match written {
            PageWrite::Written { enqueued: added } => {
                enqueued = enqueued.saturating_add(added);
                if added > 0 {
                    state.eval.wake();
                }
            }
            PageWrite::RuleGone => return Ok(RunEnd::Gone),
            PageWrite::RuleDisabled => {
                return finish(
                    state,
                    row,
                    BackfillEnd::Failed(ERROR_RULE_DISABLED),
                    matched,
                    enqueued,
                )
                .await;
            }
        }
    }
}

/// A failed read ends the backfill with the error's stored text (D-I-41).
async fn fail_read(
    state: &AppState,
    row: &BackfillRow,
    error: &BackfillReadError,
    matched: u64,
    enqueued: u64,
) -> Result<RunEnd, MetaError> {
    tracing::warn!(
        backfill_id = %row.id,
        rule_id = %row.rule_id,
        error = error.stored(),
        "a backfill read failed"
    );
    finish(
        state,
        row,
        BackfillEnd::Failed(error.stored()),
        matched,
        enqueued,
    )
    .await
}

/// Ends a backfill with `end` and logs its counts. A row that already ended stays as it is.
async fn finish(
    state: &AppState,
    row: &BackfillRow,
    end: BackfillEnd,
    matched: u64,
    enqueued: u64,
) -> Result<RunEnd, MetaError> {
    let (id, now) = (row.id, state.clock.now_ns());
    state
        .meta
        .write(move |conn| {
            Box::pin(async move { backfills::finish_backfill(conn, id, end, now).await })
        })
        .await?;
    let (status, run_end) = match end {
        BackfillEnd::Done => (STATUS_DONE, RunEnd::Done),
        BackfillEnd::Failed(_) => (STATUS_FAILED, RunEnd::Failed),
    };
    tracing::info!(
        backfill_id = %row.id,
        rule_id = %row.rule_id,
        project_id = %row.project_id,
        matched,
        enqueued,
        status,
        "backfill finished"
    );
    Ok(run_end)
}

/// Test seam: the next candidate read fails with a chosen error.
#[cfg(test)]
mod faults {
    use std::cell::RefCell;

    use llmobs_storage::RunError;

    thread_local! {
        static READ: RefCell<Option<RunError>> = const { RefCell::new(None) };
    }

    /// The next page read fails with `error`.
    pub(super) fn fail_next_read(error: RunError) {
        READ.with(|slot| *slot.borrow_mut() = Some(error));
    }

    pub(super) fn take_read() -> Option<RunError> {
        READ.with(|slot| slot.borrow_mut().take())
    }
}

#[cfg(test)]
mod tests {
    use llmobs_core::enums::{SpanKind, SpanStatus};
    use llmobs_core::ids::{SpanId, TraceId};
    use llmobs_core::score::ScoreValue;
    use llmobs_core::span::Span;
    use llmobs_otel::PriceScope;

    use super::*;
    use crate::api::scores::ScoreCreate;
    use crate::eval::EvalTiming;
    use crate::meta::backfills::{NewBackfill, STATUS_RUNNING, insert_backfill, list_backfills};
    use crate::meta::eval_jobs::{KIND_RULE, insert_rule_job};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
    use crate::scores::{Writer, write};
    use crate::test_support::{TestApp, dataset_span};

    const SECOND: i64 = 1_000_000_000;
    const NOW: i64 = 1_767_225_600_000_000_000;

    fn trace_id(n: u128) -> TraceId {
        TraceId::from_bytes(*Uuid::from_u128(n).as_bytes()).expect("not all zero")
    }

    fn span_id(n: u64) -> SpanId {
        SpanId::from_bytes(n.to_be_bytes()).expect("not all zero")
    }

    /// A root span of `trace` at `start`, with the given status.
    fn span(trace: TraceId, id: u64, start: i64, status: SpanStatus) -> Span {
        let mut row = dataset_span(0);
        row.trace_id = trace;
        row.span_id = span_id(id);
        row.parent_span_id = if id == 1 { None } else { Some(span_id(1)) };
        row.start_time = start;
        row.end_time = start + 10_000_000;
        row.environment = None;
        row.metadata.clear();
        row.status = status;
        row.span_kind = SpanKind::Chain;
        row
    }

    fn rule_of(
        project: ProjectId,
        org: OrgId,
        target: RuleTarget,
        filter: Option<&str>,
    ) -> BackfillRule {
        BackfillRule {
            id: Uuid::from_u128(0x700),
            project,
            org,
            target,
            filter: filter.map(str::to_owned),
        }
    }

    fn window(from: i64, to: i64) -> TimeWindow {
        TimeWindow::new(from, to).expect("from before to")
    }

    /// Every page of a pager, until it ends.
    async fn read_all(pager: &mut Pager<'_>) -> Result<Vec<MatchRow>, BackfillReadError> {
        let mut rows = Vec::new();
        while let Some(page) = pager.next_page().await? {
            rows.extend(page.rows);
        }
        Ok(rows)
    }

    fn trace_ids(rows: &[MatchRow]) -> Vec<TraceId> {
        rows.iter().map(|row| row.trace_id).collect()
    }

    fn price(input: Option<f64>, output: Option<f64>) -> PriceRow {
        PriceRow {
            scope: PriceScope::Builtin,
            model_name: "m".to_owned(),
            match_pattern: "m".to_owned(),
            provider: None,
            valid_from: 0,
            price_input: input,
            price_output: output,
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        }
    }

    #[test]
    fn validate_accepts_a_window_and_defaults_the_rate() {
        let to = NOW - 10 * 60 * SECOND;
        let from = to - 7 * DAY_NS;
        let checked = validate(from, to, 1000, None, 0.25, NOW).expect("valid");
        assert_eq!(
            checked,
            Window {
                from,
                to,
                max_targets: 1000,
                sample_rate: 0.25,
            }
        );
    }

    #[test]
    fn validate_reports_every_field() {
        let to = NOW - 60 * SECOND;
        let from = to + SECOND;
        let errors = validate(from, to, 0, Some(1.5), 0.25, NOW).expect_err("invalid");
        let fields: Vec<&str> = errors.iter().map(|error| error.field.as_str()).collect();
        assert_eq!(fields, ["to", "from", "max_targets", "sample_rate"]);
    }

    #[test]
    fn validate_caps_the_window_targets_and_rate() {
        let to = NOW - 5 * 60 * SECOND;
        let exact = to - MAX_WINDOW_NS;
        assert!(validate(exact, to, MAX_TARGETS, Some(1.0), 0.25, NOW).is_ok());
        assert!(validate(exact, to, 1, Some(0.0), 0.25, NOW).is_err());

        let too_wide = validate(exact - 1, to, 1, None, 0.25, NOW).expect_err("too wide");
        assert_eq!(too_wide[0].field, "from");
        assert_eq!(too_wide[0].message, "the window is at most 30 days");

        let too_many = validate(exact, to, MAX_TARGETS + 1, None, 0.25, NOW).expect_err("many");
        assert_eq!(too_many[0].field, "max_targets");

        for rate in [0.0, -1.0, f64::NAN] {
            let errors = validate(exact, to, 1, Some(rate), 0.25, NOW).expect_err("rate");
            assert_eq!(errors[0].field, "sample_rate", "rate {rate}");
        }
    }

    #[test]
    fn estimate_cost_counts_tokens_and_cost() {
        let judge = JudgeCost {
            template_chars: 100,
            variables: 2,
            max_output_tokens: 50,
            price: Some(price(Some(2.0), Some(8.0))),
        };
        let cost = estimate_cost(4, &judge);
        assert_eq!(cost.input_tokens, 4 * 1025);
        assert_eq!(cost.output_tokens, 200);
        let row = judge.price.as_ref().expect("priced");
        assert_eq!(
            cost.cost_usd,
            Some(4.0 * budget::estimate_usd(4100, 50, row))
        );
    }

    #[test]
    fn estimate_cost_without_a_price_has_no_cost() {
        let judge = JudgeCost {
            template_chars: 10,
            variables: 0,
            max_output_tokens: 1,
            price: None,
        };
        assert_eq!(estimate_cost(3, &judge).cost_usd, None);
    }

    #[test]
    fn stored_errors_follow_the_kind() {
        let stored = |error: BackfillReadError| error.stored();
        assert_eq!(
            stored(BackfillReadError::Query(QueryError::Busy)),
            ERROR_STORAGE_UNAVAILABLE
        );
        assert_eq!(
            stored(BackfillReadError::Run(RunError::Timeout)),
            ERROR_STORAGE_UNAVAILABLE
        );
        assert_eq!(
            stored(BackfillReadError::Run(RunError::Failed("x".to_owned()))),
            ERROR_STORAGE_FAILED
        );
        assert_eq!(
            stored(BackfillReadError::Run(RunError::InvalidRequest(
                "x".to_owned()
            ))),
            ERROR_STORAGE_FAILED
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pages_walk_newest_first_until_the_limit() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let spans: Vec<Span> = (1..=5_u128)
            .map(|n| {
                span(
                    trace_id(n),
                    1,
                    NOW - 60 * SECOND + n as i64 * SECOND,
                    SpanStatus::Ok,
                )
            })
            .collect();
        app.commit_spans(&seeded.project, spans).await;
        let rule = rule_of(seeded.project.id, seeded.org_id, RuleTarget::Trace, None);
        let time = window(NOW - 2 * 60 * SECOND, NOW);

        let mut limited = Pager::new(&app.state, &rule, time, 4).with_page(2);
        let mut pages = Vec::new();
        while let Some(page) = limited.next_page().await.unwrap() {
            pages.push(trace_ids(&page.rows));
        }
        assert_eq!(
            pages,
            vec![
                vec![trace_id(5), trace_id(4)],
                vec![trace_id(3), trace_id(2)],
            ]
        );

        let mut whole = Pager::new(&app.state, &rule, time, 10).with_page(2);
        let mut sizes = Vec::new();
        while let Some(page) = whole.next_page().await.unwrap() {
            sizes.push(page.rows.len());
        }
        assert_eq!(sizes, vec![2, 2, 1]);
        assert!(whole.next_page().await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_window_is_half_open() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let from = NOW - 60 * SECOND;
        let to = NOW - 30 * SECOND;
        app.commit_spans(
            &seeded.project,
            vec![
                span(trace_id(1), 1, from, SpanStatus::Ok),
                span(trace_id(2), 1, to, SpanStatus::Ok),
            ],
        )
        .await;
        let rule = rule_of(seeded.project.id, seeded.org_id, RuleTarget::Trace, None);
        let mut pager = Pager::new(&app.state, &rule, window(from, to), 10);
        let rows = read_all(&mut pager).await.unwrap();
        assert_eq!(trace_ids(&rows), vec![trace_id(1)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pages_use_the_rule_filter_and_strict_exclusions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let start = NOW - 30 * SECOND;
        let mut judged = span(trace_id(3), 1, start, SpanStatus::Error);
        judged.environment = Some(llmobs_core::names::environment::EVAL.to_owned());
        app.commit_spans(
            &seeded.project,
            vec![
                span(trace_id(1), 1, start, SpanStatus::Ok),
                span(trace_id(2), 1, start, SpanStatus::Error),
                judged,
            ],
        )
        .await;
        let time = window(NOW - 60 * SECOND, NOW);

        let errors = rule_of(
            seeded.project.id,
            seeded.org_id,
            RuleTarget::Trace,
            Some("status = 'error'"),
        );
        let rows = read_all(&mut Pager::new(&app.state, &errors, time, 10))
            .await
            .unwrap();
        assert_eq!(trace_ids(&rows), vec![trace_id(2)]);

        let evaluations = rule_of(
            seeded.project.id,
            seeded.org_id,
            RuleTarget::Trace,
            Some("environment = 'llmobs-eval'"),
        );
        let rows = read_all(&mut Pager::new(&app.state, &evaluations, time, 10))
            .await
            .unwrap();
        assert!(rows.is_empty(), "strict exclusions hide evaluation traces");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_span_rule_pages_spans() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let start = NOW - 30 * SECOND;
        app.commit_spans(
            &seeded.project,
            vec![
                span(trace_id(1), 1, start, SpanStatus::Ok),
                span(trace_id(1), 2, start + 1, SpanStatus::Error),
                span(trace_id(1), 3, start + 2, SpanStatus::Error),
            ],
        )
        .await;
        let rule = rule_of(
            seeded.project.id,
            seeded.org_id,
            RuleTarget::Span,
            Some("status = 'error'"),
        );
        let rows = read_all(&mut Pager::new(
            &app.state,
            &rule,
            window(NOW - 60 * SECOND, NOW),
            10,
        ))
        .await
        .unwrap();
        let spans: Vec<Option<SpanId>> = rows.iter().map(|row| row.span_id).collect();
        assert_eq!(spans, vec![Some(span_id(3)), Some(span_id(2))]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_stored_filter_is_an_invalid_filter() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![span(trace_id(1), 1, NOW - 30 * SECOND, SpanStatus::Ok)],
        )
        .await;
        let rule = rule_of(
            seeded.project.id,
            seeded.org_id,
            RuleTarget::Trace,
            Some("status = "),
        );
        let mut pager = Pager::new(&app.state, &rule, window(NOW - 60 * SECOND, NOW), 10);
        let error = pager.next_page().await.err().expect("a bad filter fails");
        assert_eq!(error.stored(), ERROR_INVALID_FILTER);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_read_is_a_read_error() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let rule = rule_of(seeded.project.id, seeded.org_id, RuleTarget::Trace, None);
        faults::fail_next_read(RunError::Timeout);
        let mut pager = Pager::new(&app.state, &rule, window(NOW - 60 * SECOND, NOW), 10);
        let error = pager.next_page().await.err().expect("the read fails");
        assert_eq!(error.stored(), ERROR_STORAGE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sampled_rows_match_the_live_sampler() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let spans: Vec<Span> = (1..=20_u128)
            .map(|n| span(trace_id(n), 1, NOW - 30 * SECOND, SpanStatus::Ok))
            .collect();
        app.commit_spans(&seeded.project, spans).await;
        let rule = rule_of(seeded.project.id, seeded.org_id, RuleTarget::Trace, None);
        let rows = read_all(&mut Pager::new(
            &app.state,
            &rule,
            window(NOW - 60 * SECOND, NOW),
            100,
        ))
        .await
        .unwrap();
        assert_eq!(rows.len(), 20);

        let kept = sampled_rows(rule.id, 0.5, rows.clone());
        let expected: Vec<TraceId> = rows
            .iter()
            .filter(|row| sampling::sampled(0.5, rule.id, row.trace_id, None))
            .map(|row| row.trace_id)
            .collect();
        assert_eq!(trace_ids(&kept), expected);
        assert!(!kept.is_empty() && kept.len() < rows.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unscored_drops_targets_the_rule_scored() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            (1..=3_u128)
                .map(|n| {
                    span(
                        trace_id(n),
                        1,
                        NOW - 30 * SECOND + n as i64 * SECOND,
                        SpanStatus::Ok,
                    )
                })
                .collect(),
        )
        .await;
        let rule = rule_of(seeded.project.id, seeded.org_id, RuleTarget::Trace, None);
        let evaluator: Uuid = Uuid::from_u128(0x500);
        let scored = |trace: u128| ScoreCreate {
            score_id: None,
            target: ScoreTarget::Trace {
                trace_id: trace_id(trace),
            },
            session_id: None,
            user_id: None,
            name: "same".to_owned(),
            value: ScoreValue::Numeric(1.0),
            reason: None,
            source: None,
            metadata: std::collections::BTreeMap::new(),
        };
        let writer = |rule_id: Uuid| Writer::Evaluator {
            kind: EvaluatorKind::Check,
            evaluator_id: evaluator,
            rule_id: Some(rule_id),
        };
        // The rule scored trace 2; another rule scored trace 3.
        write(
            &app.state,
            &seeded.project,
            writer(rule.id),
            vec![scored(2)],
        )
        .await
        .unwrap();
        write(
            &app.state,
            &seeded.project,
            writer(Uuid::from_u128(0x701)),
            vec![scored(3)],
        )
        .await
        .unwrap();

        let scoring = Scoring {
            name: "same".to_owned(),
            kind: EvaluatorKind::Check,
        };
        let mut pager = Pager::new(&app.state, &rule, window(NOW - 60 * SECOND, NOW), 10);
        let page = pager.next_page().await.unwrap().expect("a page");
        let kept = unscored(&page.ctx, &rule, &scoring, page.rows)
            .await
            .unwrap();
        // Newest first: trace 3 starts last.
        assert_eq!(trace_ids(&kept), vec![trace_id(3), trace_id(1)]);

        let empty = unscored(&page_ctx(&app, &rule).await, &rule, &scoring, Vec::new())
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    /// A query context over the whole project, for the empty-read case.
    async fn page_ctx(app: &TestApp, rule: &BackfillRule) -> QueryContext {
        let source = app
            .state
            .storage
            .query(
                rule.project,
                query_options(Visibility::window(&app.state, rule.org), None),
            )
            .await
            .expect("query");
        QueryContext::new(source, None).expect("context")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn estimate_counts_matched_capped_and_sampled() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            (1..=5_u128)
                .map(|n| span(trace_id(n), 1, NOW - 30 * SECOND, SpanStatus::Ok))
                .collect(),
        )
        .await;
        let rule = rule_of(seeded.project.id, seeded.org_id, RuleTarget::Trace, None);
        let checked = |max_targets: u32| Window {
            from: NOW - 60 * SECOND,
            to: NOW,
            max_targets,
            sample_rate: 1.0,
        };

        let capped = estimate(&app.state, &rule, checked(3), None).await.unwrap();
        assert_eq!(
            capped,
            Estimate {
                matched: 3,
                capped: true,
                sampled: 3,
                cost: None,
            }
        );
        let whole = estimate(&app.state, &rule, checked(10), None)
            .await
            .unwrap();
        assert_eq!(whole.matched, 5);
        assert!(!whole.capped);
        assert_eq!(whole.sampled, 5);
    }

    /// A check rule over `target` in `project`, with `filter` and `rate`. Returns the rule's id
    /// and its evaluator's id.
    async fn online_rule(
        app: &TestApp,
        project: ProjectId,
        target: &'static str,
        filter: Option<&str>,
        rate: f64,
    ) -> (Uuid, Uuid) {
        let now = app.state.clock.now_ns();
        let filter = filter.map(str::to_owned);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let evaluator = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        evaluator,
                        project,
                        "check",
                        EvaluatorFields {
                            name: format!("Same {evaluator}"),
                            config: "{}".to_owned(),
                            output_score: r#"{"name":"same","data_type":"boolean"}"#.to_owned(),
                            connection_id: None,
                            secret_enc: None,
                            version: 1,
                        },
                        None,
                        now,
                    )
                    .await?;
                    let rule = Uuid::now_v7();
                    insert_rule(
                        conn,
                        NewRule {
                            id: rule,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: target.to_owned(),
                            created_by: None,
                        },
                        RuleFields {
                            name: format!("Rule {rule}"),
                            filter,
                            variable_mapping: "{}".to_owned(),
                            sample_rate: rate,
                            budget_usd: None,
                            budget_period: None,
                            enabled: true,
                        },
                        now,
                    )
                    .await?;
                    Ok::<_, MetaError>((rule, evaluator))
                })
            })
            .await
            .expect("online rule")
    }

    /// A `running` backfill of `rule` over `[from, to)`, inserted now. Returns its id.
    async fn backfill(
        app: &TestApp,
        rule: Uuid,
        from: i64,
        to: i64,
        max_targets: u32,
        rate: f64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_backfill(
                        conn,
                        NewBackfill {
                            id,
                            rule_id: rule,
                            from_ns: from,
                            to_ns: to,
                            max_targets,
                            sample_rate: rate,
                            created_by: None,
                        },
                        now,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .expect("insert backfill");
        id
    }

    async fn backfill_row(app: &TestApp, rule: Uuid, id: Uuid) -> BackfillRow {
        list_backfills(app.state.meta.reader(), rule)
            .await
            .expect("list backfills")
            .into_iter()
            .find(|row| row.id == id)
            .expect("the backfill is listed")
    }

    /// The rule's jobs by dedup key: `(kind, priority, status, trace_start_ns)`.
    async fn jobs(app: &TestApp, rule: Uuid) -> std::collections::BTreeMap<String, JobSummary> {
        let rows: Vec<(String, String, i64, String, i64)> = sqlx::query_as(
            "SELECT dedup_key, kind, priority, status, trace_start_ns FROM eval_jobs \
             WHERE rule_id = ?",
        )
        .bind(rule.to_string())
        .fetch_all(app.state.meta.reader())
        .await
        .expect("jobs");
        rows.into_iter()
            .map(|(key, kind, priority, status, start)| (key, (kind, priority, status, start)))
            .collect()
    }

    /// `(kind, priority, status, trace_start_ns)` of one job.
    type JobSummary = (String, i64, String, i64);

    /// Runs one SQL statement on `meta.db`; every parameter is text.
    async fn execute(app: &TestApp, sql: &'static str, binds: Vec<String>) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let mut query = sqlx::query(sql);
                    for bind in binds {
                        query = query.bind(bind);
                    }
                    query
                        .execute(&mut *conn)
                        .await
                        .map(|_| ())
                        .map_err(MetaError::from)
                })
            })
            .await
            .expect("statement");
    }

    /// Queues a rule job of `trace` (no span) for `rule`.
    async fn rule_job(
        app: &TestApp,
        project: ProjectId,
        rule: Uuid,
        evaluator: Uuid,
        trace: TraceId,
        start: i64,
    ) {
        let now = app.state.clock.now_ns();
        let job = NewJob {
            id: Uuid::now_v7(),
            project_id: project,
            kind: KIND_RULE,
            rule_id: Some(rule),
            evaluator_id: evaluator,
            variable_mapping: None,
            trace_id: trace,
            span_id: None,
            trace_start_ns: start,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { insert_rule_job(conn, job, now).await.map(|_| ()) })
            })
            .await
            .expect("rule job");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_backfill_queues_its_window() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![
                span(trace_id(1), 1, NOW - 50 * SECOND, SpanStatus::Ok),
                span(trace_id(2), 1, NOW - 40 * SECOND, SpanStatus::Ok),
                span(trace_id(3), 1, NOW - 10 * SECOND, SpanStatus::Ok),
            ],
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;

        assert_eq!(backfill_once(&app.state).await.expect("pass"), 1);
        let row = backfill_row(&app, rule, id).await;
        assert_eq!(row.status, STATUS_DONE);
        assert_eq!((row.matched, row.enqueued), (2, 2));
        assert!(row.finished_at.is_some());

        let queued = jobs(&app, rule).await;
        let expected: std::collections::BTreeMap<String, JobSummary> = [
            (
                format!("rule:{rule}:{}:-", trace_id(1)),
                (
                    "backfill".to_owned(),
                    2,
                    "queued".to_owned(),
                    NOW - 50 * SECOND,
                ),
            ),
            (
                format!("rule:{rule}:{}:-", trace_id(2)),
                (
                    "backfill".to_owned(),
                    2,
                    "queued".to_owned(),
                    NOW - 40 * SECOND,
                ),
            ),
        ]
        .into_iter()
        .collect();
        assert_eq!(queued, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn max_targets_keeps_the_newest() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            (1..=5_u128)
                .map(|n| {
                    span(
                        trace_id(n),
                        1,
                        NOW - 50 * SECOND + n as i64 * SECOND,
                        SpanStatus::Ok,
                    )
                })
                .collect(),
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 2, 1.0).await;

        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, id).await;
        assert_eq!((row.status.as_str(), row.matched), (STATUS_DONE, 2));
        let keys: Vec<String> = jobs(&app, rule).await.into_keys().collect();
        let mut expected = vec![
            format!("rule:{rule}:{}:-", trace_id(5)),
            format!("rule:{rule}:{}:-", trace_id(4)),
        ];
        expected.sort();
        assert_eq!(keys, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_span_rule_queues_spans() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut first = span(trace_id(1), 2, NOW - 40 * SECOND, SpanStatus::Ok);
        first.span_kind = SpanKind::Llm;
        let mut second = span(trace_id(1), 3, NOW - 39 * SECOND, SpanStatus::Ok);
        second.span_kind = SpanKind::Llm;
        app.commit_spans(
            &seeded.project,
            vec![
                span(trace_id(1), 1, NOW - 41 * SECOND, SpanStatus::Ok),
                first,
                second,
            ],
        )
        .await;
        let (rule, _) = online_rule(
            &app,
            seeded.project.id,
            "span",
            Some("span_kind = 'llm'"),
            1.0,
        )
        .await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;

        backfill_once(&app.state).await.expect("pass");
        assert_eq!(backfill_row(&app, rule, id).await.enqueued, 2);
        let keys: Vec<String> = jobs(&app, rule).await.into_keys().collect();
        let mut expected = vec![
            format!("rule:{rule}:{}:{}", trace_id(1), span_id(2)),
            format!("rule:{rule}:{}:{}", trace_id(1), span_id(3)),
        ];
        expected.sort();
        assert_eq!(keys, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evaluation_traces_are_never_targets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut eval = span(trace_id(1), 1, NOW - 40 * SECOND, SpanStatus::Ok);
        eval.environment = Some("llmobs-eval".to_owned());
        app.commit_spans(&seeded.project, vec![eval]).await;
        let (rule, _) = online_rule(
            &app,
            seeded.project.id,
            "trace",
            Some("environment = 'llmobs-eval'"),
            1.0,
        )
        .await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;

        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, id).await;
        assert_eq!((row.status.as_str(), row.matched), (STATUS_DONE, 0));
        assert!(jobs(&app, rule).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sampling_matches_the_live_sampler() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            (1..=20_u128)
                .map(|n| {
                    span(
                        trace_id(n),
                        1,
                        NOW - 30 * SECOND + n as i64 * SECOND / 4,
                        SpanStatus::Ok,
                    )
                })
                .collect(),
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 0.5).await;
        backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 0.5).await;

        backfill_once(&app.state).await.expect("pass");
        let expected: Vec<String> = (1..=20_u128)
            .filter(|&n| sampling::sampled(0.5, rule, trace_id(n), None))
            .map(|n| format!("rule:{rule}:{}:-", trace_id(n)))
            .collect();
        assert!(
            !expected.is_empty() && expected.len() < 20,
            "a mixed sample"
        );
        let keys: Vec<String> = jobs(&app, rule).await.into_keys().collect();
        assert_eq!(keys.len(), expected.len());
        for key in expected {
            assert!(keys.contains(&key), "{key} is queued");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scored_targets_are_skipped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            (1..=3_u128)
                .map(|n| {
                    span(
                        trace_id(n),
                        1,
                        NOW - 30 * SECOND + n as i64 * SECOND,
                        SpanStatus::Ok,
                    )
                })
                .collect(),
        )
        .await;
        let (rule, evaluator) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let score = ScoreCreate {
            score_id: None,
            target: ScoreTarget::Trace {
                trace_id: trace_id(2),
            },
            session_id: None,
            user_id: None,
            name: "same".to_owned(),
            value: ScoreValue::Numeric(1.0),
            reason: None,
            source: None,
            metadata: std::collections::BTreeMap::new(),
        };
        write(
            &app.state,
            &seeded.project,
            Writer::Evaluator {
                kind: EvaluatorKind::Check,
                evaluator_id: evaluator,
                rule_id: Some(rule),
            },
            vec![score],
        )
        .await
        .expect("score");
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;

        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, id).await;
        assert_eq!((row.matched, row.enqueued), (3, 2));
        let keys: Vec<String> = jobs(&app, rule).await.into_keys().collect();
        assert!(!keys.contains(&format!("rule:{rule}:{}:-", trace_id(2))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_job_is_requeued_and_a_done_one_is_not() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![
                span(trace_id(1), 1, NOW - 50 * SECOND, SpanStatus::Ok),
                span(trace_id(2), 1, NOW - 40 * SECOND, SpanStatus::Ok),
            ],
        )
        .await;
        let (rule, evaluator) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        rule_job(
            &app,
            seeded.project.id,
            rule,
            evaluator,
            trace_id(1),
            NOW - 50 * SECOND,
        )
        .await;
        rule_job(
            &app,
            seeded.project.id,
            rule,
            evaluator,
            trace_id(2),
            NOW - 40 * SECOND,
        )
        .await;
        let failed_key = format!("rule:{rule}:{}:-", trace_id(1));
        let done_key = format!("rule:{rule}:{}:-", trace_id(2));
        execute(
            &app,
            "UPDATE eval_jobs SET status = 'failed' WHERE dedup_key = ?",
            vec![failed_key.clone()],
        )
        .await;
        execute(
            &app,
            "UPDATE eval_jobs SET status = 'done' WHERE dedup_key = ?",
            vec![done_key.clone()],
        )
        .await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;

        backfill_once(&app.state).await.expect("pass");
        assert_eq!(backfill_row(&app, rule, id).await.enqueued, 1);
        let queued = jobs(&app, rule).await;
        assert_eq!(queued[&failed_key].0, "backfill");
        assert_eq!(queued[&failed_key].2, "queued");
        assert_eq!(queued[&done_key].0, "rule");
        assert_eq!(queued[&done_key].2, "done");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_run_queues_nothing_new() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![
                span(trace_id(1), 1, NOW - 50 * SECOND, SpanStatus::Ok),
                span(trace_id(2), 1, NOW - 40 * SECOND, SpanStatus::Ok),
            ],
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let first = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        backfill_once(&app.state).await.expect("first pass");
        assert_eq!(backfill_row(&app, rule, first).await.enqueued, 2);

        app.state.clock.advance(Duration::from_secs(1));
        let second = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        backfill_once(&app.state).await.expect("second pass");
        let row = backfill_row(&app, rule, second).await;
        assert_eq!((row.matched, row.enqueued), (2, 0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disabled_rule_fails_the_backfill() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![span(trace_id(1), 1, NOW - 40 * SECOND, SpanStatus::Ok)],
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        execute(
            &app,
            "UPDATE online_rules SET enabled = 0 WHERE id = ?",
            vec![rule.to_string()],
        )
        .await;

        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, id).await;
        assert_eq!(row.status, STATUS_FAILED);
        assert_eq!(row.error.as_deref(), Some("rule disabled"));
        assert!(jobs(&app, rule).await.is_empty());
    }

    #[tokio::test]
    async fn a_failed_read_fails_the_backfill() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![span(trace_id(1), 1, NOW - 40 * SECOND, SpanStatus::Ok)],
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;

        let busy = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        faults::fail_next_read(RunError::Timeout);
        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, busy).await;
        assert_eq!(row.status, STATUS_FAILED);
        assert_eq!(row.error.as_deref(), Some("storage unavailable"));

        app.state.clock.advance(Duration::from_secs(1));
        let other = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        faults::fail_next_read(RunError::Failed("x".to_owned()));
        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, other).await;
        assert_eq!(row.error.as_deref(), Some("storage read failed"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_rule_takes_its_backfill() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        execute(
            &app,
            "DELETE FROM online_rules WHERE id = ?",
            vec![rule.to_string()],
        )
        .await;

        assert_eq!(backfill_once(&app.state).await.expect("pass"), 0);
        assert!(
            list_backfills(app.state.meta.reader(), rule)
                .await
                .expect("list")
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_rule_fails_its_backfill_and_the_next_one_runs() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![span(trace_id(1), 1, NOW - 40 * SECOND, SpanStatus::Ok)],
        )
        .await;
        let (broken, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let first = backfill(
            &app,
            broken,
            NOW - 60 * SECOND,
            NOW - 20 * SECOND,
            1000,
            1.0,
        )
        .await;
        app.state.clock.advance(Duration::from_secs(1));
        let (sound, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let second = backfill(&app, sound, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        execute(
            &app,
            "UPDATE online_rules SET target = 'bogus' WHERE id = ?",
            vec![broken.to_string()],
        )
        .await;

        assert_eq!(backfill_once(&app.state).await.expect("pass"), 2);
        let row = backfill_row(&app, broken, first).await;
        assert_eq!(row.status, STATUS_FAILED);
        assert_eq!(row.error.as_deref(), Some("rule could not be read"));
        let row = backfill_row(&app, sound, second).await;
        assert_eq!(row.status, STATUS_DONE);
        assert_eq!(row.enqueued, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_stored_filter_fails_the_backfill() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![span(trace_id(1), 1, NOW - 40 * SECOND, SpanStatus::Ok)],
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        execute(
            &app,
            "UPDATE online_rules SET filter = 'status = = 1' WHERE id = ?",
            vec![rule.to_string()],
        )
        .await;

        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, id).await;
        assert_eq!(row.status, STATUS_FAILED);
        assert_eq!(row.error.as_deref(), Some("invalid filter"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_runner_leaves_the_backfill_running() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;

        app.state.eval.stop();
        assert_eq!(backfill_once(&app.state).await.expect("pass"), 0);
        assert_eq!(backfill_row(&app, rule, id).await.status, STATUS_RUNNING);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pages_record_progress() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            (1..=1_200_u128)
                .map(|n| {
                    span(
                        trace_id(n),
                        1,
                        NOW - 30 * SECOND + n as i64 * 1_000_000,
                        SpanStatus::Ok,
                    )
                })
                .collect(),
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let id = backfill(
            &app,
            rule,
            NOW - 60 * SECOND,
            NOW - 20 * SECOND,
            10_000,
            1.0,
        )
        .await;

        backfill_once(&app.state).await.expect("pass");
        let row = backfill_row(&app, rule, id).await;
        assert_eq!(
            (row.status.as_str(), row.matched, row.enqueued),
            (STATUS_DONE, 1_200, 1_200)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_loop_runs_a_backfill_when_woken() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![span(trace_id(1), 1, NOW - 40 * SECOND, SpanStatus::Ok)],
        )
        .await;
        let (rule, _) = online_rule(&app, seeded.project.id, "trace", None, 1.0).await;
        let tasks = crate::eval::spawn(
            &app.state,
            EvalTiming {
                backfill_every: Duration::from_secs(3_600),
                ..EvalTiming::TEST
            },
        );
        tokio::time::sleep(Duration::from_millis(200)).await;

        let id = backfill(&app, rule, NOW - 60 * SECOND, NOW - 20 * SECOND, 1000, 1.0).await;
        app.state.eval.wake_backfills();
        let mut status = String::new();
        for _ in 0..250 {
            status = backfill_row(&app, rule, id).await.status;
            if status == STATUS_DONE {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(status, STATUS_DONE);
        tasks.shutdown(&app.state).await;
    }
}
