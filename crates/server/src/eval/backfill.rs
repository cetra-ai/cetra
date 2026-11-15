//! Online rule backfill (D-I-25, D-I-38 to D-I-42): validation, the paged reads, sampling, the
//! already-scored check and the estimate. The runner that enqueues the jobs is added later.

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
use crate::meta::MetaError;
use crate::meta::evaluators::EvaluatorRow;
use crate::meta::online_rules::RuleRow;
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
// I6.4 calls it.
#[allow(dead_code)]
pub(crate) const ERROR_RULE_DISABLED: &str = "rule disabled";
/// Storage was busy or a read timed out (D-I-41).
pub(crate) const ERROR_STORAGE_UNAVAILABLE: &str = "storage unavailable";
/// The rule's stored filter no longer parses (D-I-41).
pub(crate) const ERROR_INVALID_FILTER: &str = "invalid filter";
/// Any other read or conversion failure (D-I-41).
pub(crate) const ERROR_STORAGE_FAILED: &str = "storage read failed";
/// The stored rule or its project does not decode (D-I-41).
// I6.4 calls it.
#[allow(dead_code)]
pub(crate) const ERROR_RULE: &str = "rule could not be read";
/// The rule's evaluator row is missing or does not decode (D-I-41).
// I6.4 calls it.
#[allow(dead_code)]
pub(crate) const ERROR_EVALUATOR: &str = "evaluator could not be read";

/// A checked backfill window and its limits (D-I-42).
#[derive(Debug, Clone, Copy, PartialEq)]
// I6.5 reads it.
#[allow(dead_code)]
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
// I6.5 calls it.
#[allow(dead_code)]
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
// I6.4 calls it.
#[allow(dead_code)]
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
    // I6.4 calls it.
    #[allow(dead_code)]
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
    // I6.4 calls it.
    #[allow(dead_code)]
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
// I6.4 reads it.
#[allow(dead_code)]
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
    // I6.4 calls it.
    #[allow(dead_code)]
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
    // I6.4 calls it.
    #[allow(dead_code)]
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
// I6.4 calls it.
#[allow(dead_code)]
pub(crate) fn sampled_rows(rule: Uuid, rate: f64, rows: Vec<MatchRow>) -> Vec<MatchRow> {
    rows.into_iter()
        .filter(|row| sampling::sampled(rate, rule, row.trace_id, row.span_id))
        .collect()
}

/// What the scored check needs of the rule's evaluator (D-I-40).
#[derive(Debug, Clone, PartialEq, Eq)]
// I6.4 reads it.
#[allow(dead_code)]
pub(crate) struct Scoring {
    /// The evaluator's current output score name.
    pub(crate) name: String,
    pub(crate) kind: EvaluatorKind,
}

impl Scoring {
    /// From the stored evaluator, without opening a webhook's sealed values. `None` when the
    /// kind or the output score does not decode.
    // I6.4 calls it.
    #[allow(dead_code)]
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
// I6.4 calls it.
#[allow(dead_code)]
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
// I6.5 reads it.
#[allow(dead_code)]
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
// I6.5 reads it.
#[allow(dead_code)]
pub(crate) struct CostEstimate {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cost_usd: Option<f64>,
}

/// What a backfill would do (D-I-38, D-I-42).
#[derive(Debug, Clone, Copy, PartialEq)]
// I6.5 reads it.
#[allow(dead_code)]
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
// I6.5 calls it.
#[allow(dead_code)]
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
}
