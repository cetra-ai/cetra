# T1 — `PriceTable::layered` in `cetra-otel`

**Commit subject:** `otel: layer custom price rows over a shared table`

Adds a way to build a price table whose rows sit on top of another, already compiled, table, so the server can give each project its org's and its own rows without copying or recompiling the ~1 900 built-in rows (README § Model, D-T-3, D-T-9). The lookup over a layered table must give exactly what one flat table with the same rows would give: spec 02 rule 2 does not change. No server change yet.

**Prerequisite:** README § Prerequisite. Run both commands; each must print one line, else STOP.

## Read first

- README § Design → Model, D-T-3.
- `docs/spec/02-ingestion.md` § Cost computation, rule 2 (the order: scope, then latest `valid_from`, then longest pattern, then lowest row index).
- `crates/otel/src/cost.rs`, all of it: `PriceScope`, `PriceRow`, `PriceTable` (`new`, `compile`, `price_for`), `lookup`, `row_matches`, `same_provider`, `row_beats`, and the `tests` module (`row`, `table`, `priced_of`).
- `crates/otel/src/error.rs`: `PriceError`.
- `crates/otel/src/lib.rs`: the `pub use cost::{…}` line.

## Files

| File | Change |
|---|---|
| `crates/otel/src/cost.rs` | `base` field, `layered`, `rows`, `base`, `total_rows`, `exact_pattern`; the lookup walks the base; tests |
| `crates/otel/src/lib.rs` | re-export `exact_pattern` |

Nothing else changes. `normalize` keeps taking `&PriceTable`.

## 1. The struct

Add `use std::sync::Arc;` to the imports. Change the struct and its doc:

```rust
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
```

## 2. Constructors and accessors

`compile` gains the base as its last argument and stores it; its body is otherwise unchanged:

```rust
fn compile(
    rows: Vec<PriceRow>,
    size_limit: usize,
    base: Option<Arc<PriceTable>>,
) -> Result<Self, PriceError> {
    // … unchanged checks …
    Ok(Self { rows, set, base })
}
```

`new` calls `Self::compile(rows, PATTERN_SIZE_LIMIT, None)`. The existing test that calls `PriceTable::compile(rows, 1)` becomes `PriceTable::compile(rows, 1, None)`.

Add to `impl PriceTable`, after `new`:

```rust
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
```

`price_for` is unchanged (it calls `lookup`).

Add a free function after `builtin_price_rows`, for the server's "exact name" rows (D-T-4); the server has no `regex` dependency:

```rust
/// The pattern that matches exactly `model_name` (case-insensitive once compiled): every regex
/// metacharacter escaped.
#[must_use]
pub fn exact_pattern(model_name: &str) -> String {
    regex::escape(model_name)
}
```

In `crates/otel/src/lib.rs`, add `exact_pattern` to the `pub use cost::{…}` list (keep it sorted as rustfmt orders it).

## 3. The lookup

Replace `lookup` with a wrapper over a ranked lookup. A row's rank index is its position in the flat order "base rows first, then own rows", so `row_beats`'s last tie-break (lowest index) prefers the base row, as a flat table would.

```rust
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
```

`row_matches`, `same_provider`, `row_beats`, `apply` and everything else stay as they are. The recursion depth is the number of layers (the server uses one).

## 4. Tests (`cost.rs` `mod tests`)

Use the existing `row(pattern, scope, provider, valid_from, price_input)` helper and `table(rows)`. Add a helper:

```rust
fn layered(base: Vec<PriceRow>, rows: Vec<PriceRow>) -> PriceTable {
    PriceTable::layered(Arc::new(table(base)), rows).expect("patterns")
}

fn input_price(prices: &PriceTable, model: &str, provider: Option<&str>, at: i64) -> Option<f64> {
    prices.price_for(model, provider, at).and_then(|row| row.price_input)
}
```

Named cases:

1. `a_custom_row_overrides_the_base_row_of_the_same_model`: base `row("gpt-4o", Builtin, Some("openai"), 0, 2.5)`; layer `row("gpt-4o", Project, None, 0, 1.0)`. `input_price(…, "gpt-4o", Some("openai"), 10)` is `Some(1.0)`.
2. `the_base_prices_models_no_custom_row_matches`: same tables; `"gpt-4o-mini"` is not matched by either pattern → `None`; add base `row("gpt-4o-mini", Builtin, Some("openai"), 0, 0.15)` → `Some(0.15)`.
3. `scope_beats_a_later_valid_from_across_layers`: base `row("m", Builtin, None, 100, 9.0)`, layer `row("m", Org, None, 0, 1.0)`; at 200 → `Some(1.0)`.
4. `project_beats_org_in_one_layer`: layer `[row("m", Org, None, 0, 1.0), row("m", Project, None, 0, 2.0)]` over an empty base → `Some(2.0)`; reverse the order of the two rows → still `Some(2.0)`.
5. `a_custom_row_with_a_provider_leaves_other_providers_to_the_base`: base `row("llama3", Builtin, Some("ollama"), 0, 0.5)`; layer `row("llama3", Project, Some("openai"), 0, 0.0)`. Provider `openai` → `Some(0.0)`; provider `ollama` → `Some(0.5)`; provider `None` → `Some(0.0)` (project scope wins, both rows match a span without a provider).
6. `a_custom_row_without_a_provider_prices_every_provider`: layer `row("llama3", Org, None, 0, 0.0)` over an empty base; providers `Some("openai")`, `Some("ollama")`, `None` all give `Some(0.0)`.
7. `valid_from_still_applies_to_custom_rows`: layer `row("m", Project, None, 100, 1.0)` over base `row("m", Builtin, None, 0, 5.0)`; at 50 → `Some(5.0)`; at 100 → `Some(1.0)`.
8. `ties_across_layers_prefer_the_base_row`: base `row("m", Org, None, 0, 1.0)`, layer `row("m", Org, None, 0, 2.0)` (same scope, `valid_from`, pattern length) → `Some(1.0)`, as one table `[base row, layer row]` gives.
9. `layered_equals_one_flat_table`: base rows `[row("gpt-4o", Builtin, Some("openai"), 0, 2.5), row("gpt-4o", Builtin, Some("azure.ai.openai"), 0, 2.6), row("gpt-.*", Builtin, None, 0, 9.0), row("claude-.*", Builtin, Some("anthropic"), 0, 3.0)]`; layer rows `[row("gpt-4o", Org, Some("openai"), 50, 1.0), row("gpt-4o.*", Project, None, 0, 1.5), row("claude-3", Org, None, 0, 0.5)]`. Build `layered(base.clone(), custom.clone())` and `table(base ++ custom)`. For every model in `["gpt-4o", "GPT-4O", "gpt-4o-mini", "gpt-5", "claude-3", "claude-3-opus", "llama3"]`, every provider in `[None, Some("openai"), Some("azure.ai.openai"), Some("anthropic"), Some("gcp.gen_ai")]` and every time in `[0, 49, 50, 1_000]`, `price_for` returns equal rows (`assert_eq!` on the `Option<&PriceRow>`, which compares the rows' fields).
10. `rows_and_base_describe_the_layers`: `layered(vec![a], vec![b, c])`: `rows()` is `[b, c]` (equal rows), `base()` is `Some`, and the base's `rows()` is `[a]`; `table(vec![a]).base()` is `None`.
11. `an_invalid_custom_pattern_names_its_index_in_the_layer`: `PriceTable::layered(Arc::new(table(vec![row("ok", Builtin, None, 0, 1.0)])), vec![row("fine", Org, None, 0, 1.0), row("(", Org, None, 0, 1.0)])` is `Err(PriceError::InvalidPattern { index: 1 })`.
12. `apply_prices_a_span_with_a_layered_table`: build the layered table of case 1 and compute a span's cost the way the existing `priced_of(prices, model, tokens)` helper does; the cost uses the custom row's input price (`1.0` per 1M: 1 000 000 input tokens → `cost_input` `Some(1.0)`). If `priced_of` does not take a provider, a span without a provider is fine: the project row still wins.
13. `exact_pattern_matches_only_that_name`: `table(vec![row(&exact_pattern("llama3.1:8b"), Project, None, 0, 1.0)])` prices `"llama3.1:8b"` and `"LLAMA3.1:8B"`, not `"llama3x1:8b"` nor `"llama3.1:8b-q4"`.

## Commands

```bash
cargo test -p cetra-otel cost
```

```bash
just check
```

## Done when

- The 13 tests pass, and every existing `cetra-otel` test passes unchanged (golden fixtures included).
- `PriceTable::layered`, `rows` and `base` are public and documented; `total_rows` and `ranked_lookup` are private.
- `just check` is green.

**Commit:** `otel: layer custom price rows over a shared table`
