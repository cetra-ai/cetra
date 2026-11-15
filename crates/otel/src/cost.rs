//! Price table and span cost.
//!
//! Spec 02 § Cost computation, D82 and D109. Reported cost wins outright.
//! Otherwise an `llm` or `embedding` span is priced from the matching row.

use std::sync::Arc;

use llmobs_core::enums::{CostSource, SpanKind};
use llmobs_core::names::attr;
use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use serde::{Deserialize, Serialize};

use crate::bag::AttrBag;
use crate::error::PriceError;
use crate::usage::Usage;
use crate::value::{InvalidNumber, as_f64};
use crate::warn::{Warning, Warnings};

/// Compiled patterns may use this many bytes in total.
const PATTERN_SIZE_LIMIT: usize = 16 * 1024 * 1024;

const TOTAL_KEYS: &[&str] = &[
    attr::COST_TOTAL,
    "llm.cost.total",
    "gen_ai.usage.cost",
    "gen_ai.usage.total_cost",
];
const INPUT_KEYS: &[&str] = &["llm.cost.prompt", "gen_ai.usage.input_cost"];
const OUTPUT_KEYS: &[&str] = &["llm.cost.completion", "gen_ai.usage.output_cost"];
const COST_DETAILS: &str = "langfuse.observation.cost_details";
/// Semconv provider for any Google GenAI endpoint. See [`same_provider`].
const GCP_GEN_AI: &str = "gcp.gen_ai";

/// Who owns a price row. Project beats org, which beats built-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceScope {
    /// A row from the embedded snapshot.
    #[default]
    Builtin,
    /// A row stored for one organization.
    Org,
    /// A row stored for one project.
    Project,
}

/// One long-context price band. Null prices use the row's base price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceTier {
    /// This band applies when `input_tokens` is at least this count.
    pub min_input_tokens: i64,
    /// USD per 1M uncached input tokens.
    pub price_input: Option<f64>,
    /// USD per 1M output tokens.
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens. Null uses the row's base reasoning price.
    pub price_reasoning: Option<f64>,
}

/// One price-table row. `scope` defaults to [`PriceScope::Builtin`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceRow {
    /// Who owns the row.
    #[serde(default)]
    pub scope: PriceScope,
    /// Display name.
    pub model_name: String,
    /// Regex matched against the span model. Anchored and case-insensitive
    /// when the table is built.
    pub match_pattern: String,
    /// When set, the row matches this provider, and also a span that names none.
    pub provider: Option<String>,
    /// Row applies at span start times at or after this instant.
    #[serde(with = "llmobs_core::time_serde")]
    pub valid_from: i64,
    /// USD per 1M uncached input tokens.
    pub price_input: Option<f64>,
    /// USD per 1M output tokens.
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens. Null means use `price_output` at compute time.
    pub price_reasoning: Option<f64>,
    /// Long-context bands. Null when the row has none.
    pub tiers: Option<Vec<PriceTier>>,
}

/// Compiled price rows. Patterns are `^(?i:pattern)$`.
///
/// A table built by [`PriceTable::layered`] also prices with the rows of its base table: the
/// lookup picks the same row a single table holding the base rows followed by these rows would
/// pick (spec 02 § Cost computation, rule 2).
#[derive(Debug, Clone)]
pub struct PriceTable {
    rows: Vec<PriceRow>,
    set: RegexSet,
    base: Option<Arc<PriceTable>>,
}

impl PriceTable {
    /// Compile `rows`. An invalid pattern or a set over 16 MiB is an error.
    ///
    /// # Errors
    ///
    /// [`PriceError::InvalidPattern`] carries the row index and no pattern text.
    /// [`PriceError::TooLarge`] is the compiled-size limit.
    pub fn new(rows: Vec<PriceRow>) -> Result<Self, PriceError> {
        Self::compile(rows, PATTERN_SIZE_LIMIT, None)
    }

    /// Compile `rows` on top of `base`. Lookups consider the rows of both, as one table holding
    /// `base`'s rows followed by `rows` would (spec 02 rule 2). `base` is shared, not copied.
    ///
    /// # Errors
    ///
    /// As [`PriceTable::new`]. An index in [`PriceError::InvalidPattern`] is a position in `rows`.
    pub fn layered(base: Arc<PriceTable>, rows: Vec<PriceRow>) -> Result<Self, PriceError> {
        Self::compile(rows, PATTERN_SIZE_LIMIT, Some(base))
    }

    /// This table's own rows, in the order they were given. The base table's rows are not included.
    #[must_use]
    pub fn rows(&self) -> &[PriceRow] {
        &self.rows
    }

    /// The table this one was layered on, if any.
    #[must_use]
    pub fn base(&self) -> Option<&Arc<PriceTable>> {
        self.base.as_ref()
    }

    /// Rows of this table and of every table below it.
    fn total_rows(&self) -> usize {
        self.rows.len() + self.base.as_ref().map_or(0, |base| base.total_rows())
    }

    fn compile(
        rows: Vec<PriceRow>,
        size_limit: usize,
        base: Option<Arc<PriceTable>>,
    ) -> Result<Self, PriceError> {
        let mut patterns = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            // The pattern must be a complete regex on its own. Checking only the
            // wrapped form would accept `x)|(.*`, which wraps to `^(?i:x)|(.*)$`
            // and matches every model.
            match RegexBuilder::new(&row.match_pattern)
                .size_limit(size_limit)
                .build()
            {
                Ok(_) => {}
                Err(regex::Error::CompiledTooBig(_)) => return Err(PriceError::TooLarge),
                Err(_) => return Err(PriceError::InvalidPattern { index }),
            }
            let pattern = format!("^(?i:{})$", row.match_pattern);
            if Regex::new(&pattern).is_err() {
                return Err(PriceError::InvalidPattern { index });
            }
            patterns.push(pattern);
        }
        let set = RegexSetBuilder::new(patterns)
            .size_limit(size_limit)
            .build()
            .map_err(|err| match err {
                regex::Error::CompiledTooBig(_) => PriceError::TooLarge,
                _ => PriceError::InvalidPattern { index: 0 },
            })?;
        Ok(Self { rows, set, base })
    }

    /// The row that prices `model` for `provider` at `at`, as span costing would pick it.
    #[must_use]
    pub fn price_for(&self, model: &str, provider: Option<&str>, at: i64) -> Option<&PriceRow> {
        lookup(self, model, provider, at)
    }
}

/// Built-in price rows from the committed LiteLLM snapshot.
///
/// Every row has [`PriceScope::Builtin`]. `just prices-update` regenerates the
/// file. `tests/prices.rs` checks that it parses, so a failure here is a bug
/// in that snapshot.
#[allow(clippy::expect_used)]
pub fn builtin_price_rows() -> Vec<PriceRow> {
    // The snapshot is ours and `tests/prices.rs` checks it. A parse failure is a bug.
    serde_json::from_str(include_str!("../prices/builtin.json"))
        .expect("builtin.json parses; tests/prices.rs checks the snapshot")
}

/// The pattern that matches exactly `model_name` (case-insensitive once compiled): every regex
/// metacharacter escaped.
#[must_use]
pub fn exact_pattern(model_name: &str) -> String {
    regex::escape(model_name)
}

/// Cost columns for one span.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Priced {
    /// Input cost, USD.
    pub input: Option<f64>,
    /// Output cost, USD. Null on an embedding span.
    pub output: Option<f64>,
    /// Total cost, USD.
    pub total: Option<f64>,
    /// Where the columns came from. Null when the kind does not carry cost.
    pub source: Option<CostSource>,
    /// Provider taken from the matched row when the span named none.
    pub provider: Option<String>,
}

/// Span facts cost needs, apart from the attribute bag.
pub(crate) struct Charge<'a> {
    /// Span kind.
    pub kind: SpanKind,
    /// Provider already read from attributes.
    pub provider: Option<&'a str>,
    /// `response_model`, or `request_model` when the response names none.
    pub model: Option<&'a str>,
    /// Span start, nanoseconds.
    pub start_time: i64,
    /// Token columns.
    pub usage: &'a Usage,
}

/// Fill cost for one span. On any other kind the cost keys stay unread.
pub(crate) fn apply(
    bag: &mut AttrBag<'_>,
    charge: &Charge<'_>,
    prices: &PriceTable,
    warnings: &mut Warnings,
) -> Priced {
    if !matches!(charge.kind, SpanKind::Llm | SpanKind::Embedding) {
        return Priced::default();
    }
    if let Some(reported) = reported_cost(bag, warnings) {
        return Priced {
            input: reported.input,
            output: reported.output,
            total: reported.total,
            source: Some(CostSource::Reported),
            provider: None,
        };
    }
    let Some(model) = charge.model else {
        return none();
    };
    let Some(matched) = lookup(prices, model, charge.provider, charge.start_time) else {
        return none();
    };
    let inferred = match charge.provider {
        None => matched.provider.clone(),
        Some(_) => None,
    };
    // No token count at all is unknown usage, not a free call (D-B1-7).
    if charge.usage.input_tokens.is_none() && charge.usage.output_tokens.is_none() {
        return Priced {
            provider: inferred,
            ..none()
        };
    }
    let units = unit_prices(matched, charge.usage.input_tokens);
    let (input, output, total) = formula(charge.kind, charge.usage, &units);
    Priced {
        input,
        output,
        total,
        source: Some(CostSource::Computed),
        provider: inferred,
    }
}

fn none() -> Priced {
    Priced {
        source: Some(CostSource::None),
        ..Priced::default()
    }
}

struct Reported {
    input: Option<f64>,
    output: Option<f64>,
    total: Option<f64>,
}

/// `None` when no reported-cost key is present. A present key sets the source
/// even when its value is not a number.
fn reported_cost(bag: &mut AttrBag<'_>, warnings: &mut Warnings) -> Option<Reported> {
    let (total_present, total) = first_number(bag, TOTAL_KEYS, warnings);
    let (input_present, input) = first_number(bag, INPUT_KEYS, warnings);
    let (output_present, output) = first_number(bag, OUTPUT_KEYS, warnings);
    if !total_present && !input_present && !output_present {
        return langfuse_cost(bag, warnings);
    }
    Some(Reported {
        input,
        output,
        total: reported_total(input, output, total),
    })
}

/// `cost_details` when no attribute cost key is present.
///
/// A value that is not a JSON object still sets the source and stays unread.
fn langfuse_cost(bag: &mut AttrBag<'_>, warnings: &mut Warnings) -> Option<Reported> {
    let value = bag.get(COST_DETAILS)?;
    let Some(json) = crate::payload::decode_json_text(value, warnings) else {
        return Some(Reported {
            input: None,
            output: None,
            total: None,
        });
    };
    let Some(object) = json.as_object() else {
        return Some(Reported {
            input: None,
            output: None,
            total: None,
        });
    };
    let input = cost_number(object.get("input"), warnings);
    let output = cost_number(object.get("output"), warnings);
    let total = cost_number(object.get("total"), warnings);
    bag.take(COST_DETAILS);
    Some(Reported {
        input,
        output,
        total: reported_total(input, output, total),
    })
}

fn reported_total(input: Option<f64>, output: Option<f64>, total: Option<f64>) -> Option<f64> {
    total.or_else(|| match (input, output) {
        (Some(input), Some(output)) => (input + output).is_finite().then_some(input + output),
        _ => None,
    })
}

fn cost_number(value: Option<&serde_json::Value>, warnings: &mut Warnings) -> Option<f64> {
    let value = value?;
    let number = match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    };
    match number {
        Some(number) if number.is_finite() => Some(number),
        Some(_) => {
            warnings.add(Warning::InvalidNumber);
            None
        }
        None if value.is_null() => None,
        None => {
            warnings.add(Warning::InvalidNumber);
            None
        }
    }
}

/// The first finite number, and whether any key in `keys` exists.
fn first_number(
    bag: &mut AttrBag<'_>,
    keys: &[&str],
    warnings: &mut Warnings,
) -> (bool, Option<f64>) {
    let mut present = false;
    let mut found = None;
    for key in keys {
        if bag.get(key).is_none() {
            continue;
        }
        present = true;
        if found.is_some() {
            continue;
        }
        let parsed = bag.get(key).map(as_f64);
        match parsed {
            Some(Ok(Some(number))) => {
                bag.take(key);
                found = Some(number);
            }
            Some(Err(InvalidNumber)) => warnings.add(Warning::InvalidNumber),
            _ => {}
        }
    }
    (present, found)
}

fn lookup<'a>(
    prices: &'a PriceTable,
    model: &str,
    provider: Option<&str>,
    start_time: i64,
) -> Option<&'a PriceRow> {
    ranked_lookup(prices, model, provider, start_time).map(|(_, row)| row)
}

/// The winning row and its index in the flat order: the base's rows, then this table's rows.
fn ranked_lookup<'a>(
    prices: &'a PriceTable,
    model: &str,
    provider: Option<&str>,
    start_time: i64,
) -> Option<(usize, &'a PriceRow)> {
    let (offset, mut best) = match &prices.base {
        Some(base) => (
            base.total_rows(),
            ranked_lookup(base, model, provider, start_time),
        ),
        None => (0, None),
    };
    for index in prices.set.matches(model) {
        let Some(row) = prices.rows.get(index) else {
            continue;
        };
        if !row_matches(row, provider, start_time) {
            continue;
        }
        let ranked = offset + index;
        let wins =
            best.is_none_or(|(best_index, best_row)| row_beats(row, ranked, best_row, best_index));
        if wins {
            best = Some((ranked, row));
        }
    }
    best
}

fn row_matches(row: &PriceRow, provider: Option<&str>, start_time: i64) -> bool {
    if row.valid_from > start_time {
        return false;
    }
    match (&row.provider, provider) {
        (Some(row_provider), Some(span_provider)) => same_provider(row_provider, span_provider),
        (Some(_), None) | (None, _) => true,
    }
}

/// `gcp.gen_ai` is any Google GenAI endpoint (Gemini API or Vertex AI), so it prices with
/// either platform's rows. Every other provider must match exactly, with no fallback.
fn same_provider(row_provider: &str, span_provider: &str) -> bool {
    row_provider == span_provider
        || (span_provider == GCP_GEN_AI && matches!(row_provider, "gcp.gemini" | "gcp.vertex_ai"))
}

/// Scope, then latest `valid_from`, then the longest pattern, then the lowest index.
fn row_beats(row: &PriceRow, index: usize, best: &PriceRow, best_index: usize) -> bool {
    row.scope > best.scope
        || (row.scope == best.scope && row.valid_from > best.valid_from)
        || (row.scope == best.scope
            && row.valid_from == best.valid_from
            && row.match_pattern.len() > best.match_pattern.len())
        || (row.scope == best.scope
            && row.valid_from == best.valid_from
            && row.match_pattern.len() == best.match_pattern.len()
            && index < best_index)
}

struct Units {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    reasoning: Option<f64>,
}

fn unit_prices(row: &PriceRow, input_tokens: Option<i64>) -> Units {
    let tokens = input_tokens.unwrap_or(0).max(0);
    let tier = row.tiers.as_ref().and_then(|tiers| {
        tiers.iter().fold(None, |best: Option<&PriceTier>, tier| {
            if tier.min_input_tokens > tokens {
                return best;
            }
            match best {
                Some(current) if current.min_input_tokens >= tier.min_input_tokens => Some(current),
                _ => Some(tier),
            }
        })
    });
    Units {
        input: tier.and_then(|tier| tier.price_input).or(row.price_input),
        output: tier.and_then(|tier| tier.price_output).or(row.price_output),
        cache_read: tier
            .and_then(|tier| tier.price_cache_read)
            .or(row.price_cache_read),
        cache_write: tier
            .and_then(|tier| tier.price_cache_write)
            .or(row.price_cache_write),
        reasoning: tier
            .and_then(|tier| tier.price_reasoning)
            .or(row.price_reasoning),
    }
}

fn formula(
    kind: SpanKind,
    usage: &Usage,
    units: &Units,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let input_tokens = count(usage.input_tokens);
    let output_tokens = count(usage.output_tokens);
    let cache_read = count(usage.cache_read_tokens);
    let cache_write = count(usage.cache_write_tokens);
    let reasoning = count(usage.reasoning_tokens);
    let uncached = input_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    let non_reasoning = output_tokens.saturating_sub(reasoning);
    let cost_input = part(&[
        charge(uncached, units.input),
        charge(cache_read, units.cache_read),
        charge(cache_write, units.cache_write),
    ]);
    if kind == SpanKind::Embedding {
        return (cost_input, None, cost_input);
    }
    let reasoning_price = units.reasoning.or(units.output);
    let cost_output = part(&[
        charge(non_reasoning, units.output),
        charge(reasoning, reasoning_price),
    ]);
    let cost_total = match (cost_input, cost_output) {
        (Some(input), Some(output)) => (input + output).is_finite().then_some(input + output),
        _ => None,
    };
    (cost_input, cost_output, cost_total)
}

fn count(value: Option<i64>) -> i64 {
    value.unwrap_or(0).max(0)
}

fn part(terms: &[Option<f64>]) -> Option<f64> {
    let mut total = 0.0;
    for term in terms {
        total += (*term)?;
    }
    (total / 1e6).is_finite().then_some(total / 1e6)
}

fn charge(tokens: i64, price: Option<f64>) -> Option<f64> {
    if tokens == 0 {
        return Some(0.0);
    }
    let cost = (tokens as f64) * price?;
    cost.is_finite().then_some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        pattern: &str,
        scope: PriceScope,
        provider: Option<&str>,
        valid_from: i64,
        price_input: f64,
    ) -> PriceRow {
        PriceRow {
            scope,
            model_name: pattern.to_owned(),
            match_pattern: pattern.to_owned(),
            provider: provider.map(str::to_owned),
            valid_from,
            price_input: Some(price_input),
            price_output: Some(1.0),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        }
    }

    fn usage_input(tokens: i64) -> Usage {
        Usage {
            input_tokens: Some(tokens),
            ..Usage::default()
        }
    }

    fn table(rows: Vec<PriceRow>) -> PriceTable {
        PriceTable::new(rows).expect("patterns")
    }

    #[test]
    fn project_beats_org_and_builtin_then_date_then_pattern_then_index() {
        let prices = table(vec![
            row("gpt", PriceScope::Builtin, None, 0, 1.0),
            row("gpt", PriceScope::Org, None, 0, 2.0),
            row("gpt-4o", PriceScope::Project, None, 10, 3.0),
            row("gpt", PriceScope::Project, None, 10, 4.0),
            row("gpt", PriceScope::Project, None, 5, 5.0),
        ]);
        let matched = lookup(&prices, "gpt", None, 10).expect("row");
        assert_eq!(matched.price_input, Some(4.0));
        let earlier = lookup(&prices, "gpt", None, 6).expect("row");
        assert_eq!(earlier.price_input, Some(5.0));
        let longer = table(vec![
            row("gpt", PriceScope::Builtin, None, 0, 1.0),
            row("gpt.*", PriceScope::Builtin, None, 0, 8.0),
        ]);
        let matched = lookup(&longer, "gpt", None, 0).expect("longer");
        assert_eq!(matched.price_input, Some(8.0));
    }

    #[test]
    fn the_pattern_is_anchored_and_case_insensitive() {
        let prices = table(vec![row("gpt-4o", PriceScope::Builtin, None, 0, 1.0)]);
        assert!(lookup(&prices, "gpt-4o", None, 0).is_some());
        assert!(lookup(&prices, "GPT-4O", None, 0).is_some());
        assert!(lookup(&prices, "gpt-4o-mini", None, 0).is_none());
        assert!(lookup(&prices, "xgpt-4o", None, 0).is_none());
    }

    #[test]
    fn a_gen_ai_span_prices_with_either_google_platform_and_nothing_else() {
        let prices = table(vec![
            row(
                "gemini-2.5-flash",
                PriceScope::Builtin,
                Some("gcp.gemini"),
                0,
                1.0,
            ),
            row(
                "gemini-2.5-flash",
                PriceScope::Builtin,
                Some("gcp.vertex_ai"),
                0,
                2.0,
            ),
            row("gpt-4o", PriceScope::Builtin, Some("openai"), 0, 3.0),
        ]);
        // Both platform rows match; the lower row index (gcp.gemini) wins.
        let matched = lookup(&prices, "gemini-2.5-flash", Some("gcp.gen_ai"), 0).expect("row");
        assert_eq!(matched.provider.as_deref(), Some("gcp.gemini"));
        let vertex_only = table(vec![row(
            "gemini-2.5-flash",
            PriceScope::Builtin,
            Some("gcp.vertex_ai"),
            0,
            2.0,
        )]);
        let matched = lookup(&vertex_only, "gemini-2.5-flash", Some("gcp.gen_ai"), 0);
        assert_eq!(matched.and_then(|row| row.price_input), Some(2.0));
        // No fallback: other providers and a no-row model stay unmatched.
        assert!(lookup(&prices, "gpt-4o", Some("gcp.gen_ai"), 0).is_none());
        assert!(lookup(&prices, "gpt-4o", Some("azure.ai.openai"), 0).is_none());
        assert!(lookup(&prices, "gemini-2.5-flash", Some("aws.bedrock"), 0).is_none());
        assert!(lookup(&prices, "gemini-2.5-flash", Some("gcp.gemini"), 0).is_some());
    }

    #[test]
    fn an_invalid_pattern_names_only_its_index() {
        let err = PriceTable::new(vec![row("(", PriceScope::Builtin, None, 0, 1.0)])
            .expect_err("invalid");
        assert_eq!(err, PriceError::InvalidPattern { index: 0 });
        let text = err.to_string();
        assert!(!text.contains('('));
        assert!(text.contains("index 0"));
    }

    #[test]
    fn a_pattern_must_be_a_complete_regex_on_its_own() {
        // Wrapped, `x)|(.*` would be `^(?i:x)|(.*)$`, a valid regex that matches every model.
        for pattern in ["x)|(.*", "foo)|(.*"] {
            let rows = vec![
                row("gpt-4o", PriceScope::Builtin, None, 0, 1.0),
                row(pattern, PriceScope::Builtin, None, 0, 1.0),
            ];
            let err = PriceTable::new(rows).expect_err("unbalanced pattern");
            assert_eq!(err, PriceError::InvalidPattern { index: 1 });
        }
        let ok = table(vec![row("gpt-4o", PriceScope::Builtin, None, 0, 1.0)]);
        assert!(lookup(&ok, "gpt-4o", None, 0).is_some());
        assert!(lookup(&ok, "claude-3", None, 0).is_none());
    }

    #[test]
    fn the_compiled_set_has_a_size_limit() {
        let rows = vec![row("abcdef", PriceScope::Builtin, None, 0, 1.0)];
        let err = PriceTable::compile(rows, 1, None).expect_err("too large");
        assert_eq!(err, PriceError::TooLarge);
    }

    fn layered(base: Vec<PriceRow>, rows: Vec<PriceRow>) -> PriceTable {
        PriceTable::layered(Arc::new(table(base)), rows).expect("patterns")
    }

    fn input_price(
        prices: &PriceTable,
        model: &str,
        provider: Option<&str>,
        at: i64,
    ) -> Option<f64> {
        prices
            .price_for(model, provider, at)
            .and_then(|row| row.price_input)
    }

    #[test]
    fn a_custom_row_overrides_the_base_row_of_the_same_model() {
        let prices = layered(
            vec![row("gpt-4o", PriceScope::Builtin, Some("openai"), 0, 2.5)],
            vec![row("gpt-4o", PriceScope::Project, None, 0, 1.0)],
        );
        assert_eq!(
            input_price(&prices, "gpt-4o", Some("openai"), 10),
            Some(1.0)
        );
    }

    #[test]
    fn the_base_prices_models_no_custom_row_matches() {
        let without_base = layered(
            vec![row("gpt-4o", PriceScope::Builtin, Some("openai"), 0, 2.5)],
            vec![row("gpt-4o", PriceScope::Project, None, 0, 1.0)],
        );
        assert_eq!(input_price(&without_base, "gpt-4o-mini", None, 0), None);
        let with_base = layered(
            vec![
                row("gpt-4o", PriceScope::Builtin, Some("openai"), 0, 2.5),
                row("gpt-4o-mini", PriceScope::Builtin, Some("openai"), 0, 0.15),
            ],
            vec![row("gpt-4o", PriceScope::Project, None, 0, 1.0)],
        );
        assert_eq!(
            input_price(&with_base, "gpt-4o-mini", Some("openai"), 0),
            Some(0.15)
        );
    }

    #[test]
    fn scope_beats_a_later_valid_from_across_layers() {
        let prices = layered(
            vec![row("m", PriceScope::Builtin, None, 100, 9.0)],
            vec![row("m", PriceScope::Org, None, 0, 1.0)],
        );
        assert_eq!(input_price(&prices, "m", None, 200), Some(1.0));
    }

    #[test]
    fn project_beats_org_in_one_layer() {
        let forward = layered(
            vec![],
            vec![
                row("m", PriceScope::Org, None, 0, 1.0),
                row("m", PriceScope::Project, None, 0, 2.0),
            ],
        );
        assert_eq!(input_price(&forward, "m", None, 0), Some(2.0));
        let reversed = layered(
            vec![],
            vec![
                row("m", PriceScope::Project, None, 0, 2.0),
                row("m", PriceScope::Org, None, 0, 1.0),
            ],
        );
        assert_eq!(input_price(&reversed, "m", None, 0), Some(2.0));
    }

    #[test]
    fn a_custom_row_with_a_provider_leaves_other_providers_to_the_base() {
        let prices = layered(
            vec![row("llama3", PriceScope::Builtin, Some("ollama"), 0, 0.5)],
            vec![row("llama3", PriceScope::Project, Some("openai"), 0, 0.0)],
        );
        assert_eq!(input_price(&prices, "llama3", Some("openai"), 0), Some(0.0));
        assert_eq!(input_price(&prices, "llama3", Some("ollama"), 0), Some(0.5));
        assert_eq!(input_price(&prices, "llama3", None, 0), Some(0.0));
    }

    #[test]
    fn a_custom_row_without_a_provider_prices_every_provider() {
        let prices = layered(vec![], vec![row("llama3", PriceScope::Org, None, 0, 0.0)]);
        assert_eq!(input_price(&prices, "llama3", Some("openai"), 0), Some(0.0));
        assert_eq!(input_price(&prices, "llama3", Some("ollama"), 0), Some(0.0));
        assert_eq!(input_price(&prices, "llama3", None, 0), Some(0.0));
    }

    #[test]
    fn valid_from_still_applies_to_custom_rows() {
        let prices = layered(
            vec![row("m", PriceScope::Builtin, None, 0, 5.0)],
            vec![row("m", PriceScope::Project, None, 100, 1.0)],
        );
        assert_eq!(input_price(&prices, "m", None, 50), Some(5.0));
        assert_eq!(input_price(&prices, "m", None, 100), Some(1.0));
    }

    #[test]
    fn ties_across_layers_prefer_the_base_row() {
        let prices = layered(
            vec![row("m", PriceScope::Org, None, 0, 1.0)],
            vec![row("m", PriceScope::Org, None, 0, 2.0)],
        );
        assert_eq!(input_price(&prices, "m", None, 0), Some(1.0));
    }

    #[test]
    fn layered_equals_one_flat_table() {
        let base = vec![
            row("gpt-4o", PriceScope::Builtin, Some("openai"), 0, 2.5),
            row(
                "gpt-4o",
                PriceScope::Builtin,
                Some("azure.ai.openai"),
                0,
                2.6,
            ),
            row("gpt-.*", PriceScope::Builtin, None, 0, 9.0),
            row("claude-.*", PriceScope::Builtin, Some("anthropic"), 0, 3.0),
        ];
        let custom = vec![
            row("gpt-4o", PriceScope::Org, Some("openai"), 50, 1.0),
            row("gpt-4o.*", PriceScope::Project, None, 0, 1.5),
            row("claude-3", PriceScope::Org, None, 0, 0.5),
        ];
        let mut flat_rows = base.clone();
        flat_rows.extend(custom.clone());
        let flat = table(flat_rows);
        let stacked = layered(base, custom);
        let models = [
            "gpt-4o",
            "GPT-4O",
            "gpt-4o-mini",
            "gpt-5",
            "claude-3",
            "claude-3-opus",
            "llama3",
        ];
        let providers = [
            None,
            Some("openai"),
            Some("azure.ai.openai"),
            Some("anthropic"),
            Some("gcp.gen_ai"),
        ];
        for model in models {
            for provider in providers {
                for at in [0, 49, 50, 1_000] {
                    assert_eq!(
                        stacked.price_for(model, provider, at),
                        flat.price_for(model, provider, at),
                        "model {model}, provider {provider:?}, at {at}"
                    );
                }
            }
        }
    }

    #[test]
    fn rows_and_base_describe_the_layers() {
        let a = row("a", PriceScope::Builtin, None, 0, 1.0);
        let b = row("b", PriceScope::Org, None, 0, 2.0);
        let c = row("c", PriceScope::Project, None, 0, 3.0);
        let stacked = layered(vec![a.clone()], vec![b.clone(), c.clone()]);
        assert_eq!(stacked.rows(), &[b, c][..]);
        let base = stacked.base().expect("base");
        assert_eq!(base.rows(), &[a][..]);
        assert!(
            table(vec![row("a", PriceScope::Builtin, None, 0, 1.0)])
                .base()
                .is_none()
        );
    }

    #[test]
    fn an_invalid_custom_pattern_names_its_index_in_the_layer() {
        let err = PriceTable::layered(
            Arc::new(table(vec![row("ok", PriceScope::Builtin, None, 0, 1.0)])),
            vec![
                row("fine", PriceScope::Org, None, 0, 1.0),
                row("(", PriceScope::Org, None, 0, 1.0),
            ],
        )
        .expect_err("invalid");
        assert_eq!(err, PriceError::InvalidPattern { index: 1 });
    }

    #[test]
    fn apply_prices_a_span_with_a_layered_table() {
        let prices = layered(
            vec![row("gpt-4o", PriceScope::Builtin, Some("openai"), 0, 2.5)],
            vec![row("gpt-4o", PriceScope::Project, None, 0, 1.0)],
        );
        let priced = priced_of(&prices, "gpt-4o", 1_000_000);
        assert_eq!(priced.input, Some(1.0));
        assert_eq!(priced.source, Some(CostSource::Computed));
    }

    #[test]
    fn exact_pattern_matches_only_that_name() {
        let prices = table(vec![row(
            &exact_pattern("llama3.1:8b"),
            PriceScope::Project,
            None,
            0,
            1.0,
        )]);
        assert!(lookup(&prices, "llama3.1:8b", None, 0).is_some());
        assert!(lookup(&prices, "LLAMA3.1:8B", None, 0).is_some());
        assert!(lookup(&prices, "llama3x1:8b", None, 0).is_none());
        assert!(lookup(&prices, "llama3.1:8b-q4", None, 0).is_none());
    }

    fn priced_of(prices: &PriceTable, model: &str, tokens: i64) -> Priced {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&[], &mut warnings);
        apply(
            &mut bag,
            &Charge {
                kind: SpanKind::Llm,
                provider: None,
                model: Some(model),
                start_time: 0,
                usage: &usage_input(tokens),
            },
            prices,
            &mut warnings,
        )
    }

    #[test]
    fn a_token_count_near_the_integer_limit_stays_finite() {
        let prices = table(vec![row(
            "huge",
            PriceScope::Builtin,
            Some("openai"),
            0,
            1.0,
        )]);
        let priced = priced_of(&prices, "huge", i64::MAX);
        let input = priced.input.expect("input");
        assert!(input.is_finite());
        assert!(priced.output.is_some_and(f64::is_finite));
        assert!(priced.total.is_some_and(f64::is_finite));
        assert_eq!(priced.provider.as_deref(), Some("openai"));
        assert_eq!(priced.source, Some(CostSource::Computed));
    }

    #[test]
    fn a_matched_row_with_a_null_price_still_sets_the_provider() {
        let mut priced_row = row("bare", PriceScope::Builtin, Some("openai"), 0, 1.0);
        priced_row.price_input = None;
        priced_row.price_output = None;
        let prices = table(vec![priced_row]);
        let priced = priced_of(&prices, "bare", 10);
        assert_eq!(priced.input, None);
        assert_eq!(priced.source, Some(CostSource::Computed));
        assert_eq!(priced.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn a_span_without_token_counts_has_no_cost_but_gets_the_provider() {
        let prices = table(vec![row(
            "known",
            PriceScope::Builtin,
            Some("openai"),
            0,
            1.0,
        )]);
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&[], &mut warnings);
        let priced = apply(
            &mut bag,
            &Charge {
                kind: SpanKind::Llm,
                provider: None,
                model: Some("known"),
                start_time: 0,
                usage: &Usage::default(),
            },
            &prices,
            &mut warnings,
        );
        assert_eq!(priced.total, None);
        assert_eq!(priced.source, Some(CostSource::None));
        assert_eq!(priced.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn the_lowest_index_wins_a_complete_tie() {
        let prices = table(vec![
            row("same", PriceScope::Builtin, None, 0, 1.0),
            row("same", PriceScope::Builtin, None, 0, 9.0),
        ]);
        let matched = lookup(&prices, "same", None, 0).expect("row");
        assert_eq!(matched.price_input, Some(1.0));
    }

    #[test]
    fn price_for_finds_a_builtin_row_and_no_row_for_an_unknown_model() {
        let prices = PriceTable::new(builtin_price_rows()).expect("builtin patterns");
        let matched = prices
            .price_for("Codestral-2501", None, 0)
            .expect("the built-in table prices Codestral-2501");
        assert!(matched.price_input.is_some());
        assert!(
            prices
                .price_for("no-such-model-anywhere", None, 0)
                .is_none()
        );
    }

    #[test]
    fn langfuse_cost_details_is_reported_and_loses_to_an_attribute() {
        use crate::bag::Use;
        use opentelemetry_proto::tonic::common::v1::any_value::Value;
        use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};

        fn string_attr(key: &str, text: &str) -> KeyValue {
            KeyValue {
                key: key.to_owned(),
                value: Some(AnyValue {
                    value: Some(Value::StringValue(text.to_owned())),
                }),
                key_strindex: 0,
            }
        }

        let prices = table(vec![row("gpt-4o-mini", PriceScope::Builtin, None, 0, 9.0)]);
        let usage = Usage::default();
        let charge = Charge {
            kind: SpanKind::Llm,
            provider: None,
            model: Some("gpt-4o-mini"),
            start_time: 0,
            usage: &usage,
        };

        let attrs = [string_attr(
            "langfuse.observation.cost_details",
            r#"{"input":0.25,"output":0.5}"#,
        )];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let priced = apply(&mut bag, &charge, &prices, &mut warnings);
        assert_eq!(priced.source, Some(CostSource::Reported));
        assert_eq!(priced.input, Some(0.25));
        assert_eq!(priced.output, Some(0.5));
        assert_eq!(priced.total, Some(0.75));
        assert_eq!(
            bag.state("langfuse.observation.cost_details"),
            Some(Use::Consumed)
        );

        let attrs = [string_attr("langfuse.observation.cost_details", "not-json")];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let priced = apply(&mut bag, &charge, &prices, &mut warnings);
        assert_eq!(priced.source, Some(CostSource::Reported));
        assert_eq!(priced.input, None);
        assert_eq!(priced.total, None);
        assert_eq!(
            bag.state("langfuse.observation.cost_details"),
            Some(Use::Unread)
        );

        let attrs = [
            string_attr("llm.cost.total", "1.5"),
            string_attr(
                "langfuse.observation.cost_details",
                r#"{"input":0.25,"output":0.5}"#,
            ),
        ];
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(&attrs, &mut warnings);
        let priced = apply(&mut bag, &charge, &prices, &mut warnings);
        assert_eq!(priced.total, Some(1.5));
        assert_eq!(priced.source, Some(CostSource::Reported));
        assert_eq!(
            bag.state("langfuse.observation.cost_details"),
            Some(Use::Unread)
        );
    }
}
