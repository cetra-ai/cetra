# QE · M2 — `cetra-query` vectorized expressions (`expr/`), Filter DSL, and tombstone predicate

Milestone 2 of track QE ([`design.md`](design.md)). It builds `crates/query` (`cetra-query`), a pure-Rust, zero-`unsafe`, `tokio`-free, Arrow-free, DataFusion-free crate implementing the expression IR, binder, vectorized batch evaluator over `cetra-column`, all curated Shape B scalar functions (~50 functions across string/binary, time/interval, math/nulls, array/map, and JSON extraction), the Filter DSL compiler (exact parity with `crates/storage/src/filter/`), and the unified tombstone predicate builder (D-A1-3). It verifies parity differentially against DataFusion inside `crates/storage` using M1's `column_bridge`.

When M2 is done:
- `crates/query` exists depending only on `cetra-column`, `sqlparser`, `thiserror`, `regex`, `chrono`, `serde_json`, `serde_json_path`, and `hex` — zero `tokio`, `arrow`, `parquet`, `datafusion`, or `object_store`;
- `crates/query` evaluates typed `Expr` IR vectorized over `cetra_column::Batch`, supporting all 4 `Column` encodings (`Flat`, `Const`, `Dict`, `Rle`), respecting `Selection` via active-row masking (never faulting on inactive rows), checking `CancelToken`, and enforcing a single exact `Reservation` postcondition across every evaluator;
- All ~50 curated scalar functions & operators work with exact spec 05 and DataFusion parity semantics (division by zero, integer overflow, total-order `F64` with `-0.0 == +0.0` and `NaN == NaN`, regex size limit, `LIKE`/`ILIKE ... ESCAPE`, calendar `Interval`, JSON `->`/`->>`/`json_extract*`, `CAST`/`TRY_CAST`);
- `bind::compile_filter` and `filter::parse_filter_ast` match `crates/storage/src/filter/` on grammar, error messages, `Position`s, `TRY_CAST` rewrite, `references_environment`, and `score()`/`score_label()`/`has_score()` metadata (`MAX_SCORE_NAMES = 8`);
- `tombstone::covered_predicate` builds the null-safe, PII-redacted (`<{n} redacted>`) tombstone predicate for `Spans`, `Scores`, and `Traces`;
- Differential tests in `crates/storage` prove row-for-row and error-code parity against DataFusion across the Filter DSL and tombstone predicates, and `just check` is green.

## Prerequisites

- Milestone 1 (`cetra-column` + `crates/storage/src/column_bridge.rs`) is merged on branch `qe-query-engine`.
- Track QE design doc ([`design.md`](design.md)) is approved on `qe-query-engine`.

## Read first

1. `AGENTS.md`: § Rules (no `unsafe`, no `unwrap`/`expect` outside tests, privacy, dependencies), § Layout, § Crate dependency graph.
2. [`design.md`](design.md): § Contracts → Contracts 1, 2 (tombstones), 4 (Filter DSL & Shape B scalar functions), 6 (differential oracle); decisions `D-QE-1`, `D-QE-2`, `D-QE-4`, `D-QE-10`; § Milestones → M2.
3. `crates/column/src/{lib.rs,error.rs,types.rs,bitmap.rs,selection.rs,memory.rs,cancel.rs,column.rs,batch.rs}`.
4. `crates/storage/src/filter/{mod.rs,validate.rs,scores.rs,scope.rs}` — exact Filter DSL grammar, error messages, spans, `TRY_CAST` rewrite, and score rewrite rules.
5. `crates/storage/src/query_text.rs` and `crates/storage/src/query_error.rs` — `GenericDialect`, `FILTER_LIMITS`, `SQL_LIMITS`, 45 symbol + 23 keyword operator tokens, `PARSER_RECURSION_LIMIT = 50`, `MESSAGE_CAP = 2048`, `split_appended_location`, and `tokenizer_failure`.
6. `crates/storage/src/deletion.rs` — `key_columns`, `covered_predicate`, `drop_covered`, and `Redacted` display formatting (`"{col} IN (<{n} redacted>)"`, D-A1-3).
7. `crates/storage/src/views/{guard.rs,hex.rs,session.rs}` — `DENIED_FUNCTIONS` (20 names), `concat_ws` / `array_to_string` amplification reservation, `||` 2 GiB cap, and `encode(col, 'hex')`.

## What it is, and what it is not

**It is:**
- the `cetra-query` crate's text tokenizer/limits, expression IR, core vectorized evaluator, ~50 curated scalar functions, Filter DSL compiler, AST binder, and unified tombstone predicate builder;
- differential parity tests in `crates/storage` comparing `cetra-query` against DataFusion;
- a `query_expr` fuzz target in `fuzz/` with corpus replay under `just test`.

**It is not:**
- push-pipeline operators (`Scan`, `Sort`, `HashAgg`, `HashJoin`, `Window`, `TopK`) or `ScanSource` / `ParquetSource` (M3);
- porting `crates/storage/src/reads/` (M4);
- SQL statement (`SELECT`/`WITH`/`JOIN`/`GROUP BY`) planning or JSON/CSV/NDJSON/Parquet encoders (M5);
- changing any production caller in `cetra-storage` or `cetra-server` (M6–M7).

**Note on `cetra-core` dependency:** `design.md` shows `cetra-query` depending on `cetra-core` after M7, when `cetra-core` drops `arrow`. During M2–M6, `cetra-core` still depends on `arrow`, so `cetra-query` intentionally does **not** depend on `cetra-core` (keeping `cetra-query` 100% Arrow-free from Day 1). `tombstone.rs` uses the string column names `"trace_id"`, `"user_id"`, `"session_id"`, `"ingest_time"` directly until M7.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-QE-1 | Own pure-Rust query engine keeping `sqlparser` (`GenericDialect`). |
| D-QE-2 | Built directly on `cetra-column` (`Batch`, `Column`, `Selection`, `Reservation`, `CancelToken`); zero Arrow/Tokio. |
| D-QE-4 | Curated ~50 Shape B scalar functions with explicit allow-list and 20 denied OOM/internal functions. |
| D-QE-9 | Differential verification against DataFusion during M2–M6 via `crates/storage/src/column_bridge.rs`. |
| D-QE-10 | Dedicated `crates/query` (`cetra-query`) crate. |
| D95, D97, D100, D-A1-3, D-A2-1, D-A2-2 | Token/operator/depth limits, score functions (max 8), DSL `TRY_CAST`, unified tombstone predicate, function memory guards. |

## Versions

M2 promotes **one** already-resolved transitive dependency (`sqlparser 0.62.0`, already in `Cargo.lock` via `datafusion 55.2.0` with features `visitor` and `recursive-protection`) to root `[workspace.dependencies]`, adding **zero** new transitive crates.

| Crate | Version (root `Cargo.toml`) | Where | Why |
|---|---|---|---|
| `sqlparser` | `{ version = "0.62.0", features = ["visitor", "recursive-protection"] }` | root `[workspace.dependencies]`, `crates/query` | Matches DataFusion 55.2's exact resolved version (`0.62.0`) so `cargo deny` sees one version. |
| `cetra-query` | `{ path = "crates/query" }` | root `[workspace.dependencies]`, `crates/storage` (`[dev-dependencies]` for M2 diff tests), `fuzz/Cargo.toml` | The new crate built in M2. |
| `cetra-column`, `thiserror`, `regex`, `chrono`, `serde_json`, `serde_json_path`, `hex` | workspace | `crates/query` | Already pinned in root `Cargo.toml`. (`chrono` is already used across the workspace and covered by `deny.toml` `RUSTSEC-2026-0543`.) |
| `proptest` | workspace (`1.12.0`) | `crates/query` `[dev-dependencies]` | Property tests for expressions and functions. |

## Phases and dependency graph

```text
QE2.1 (crate scaffold, error, text limits/tokenizer, Expr IR, shared reservation/column helpers, core eval)
  ├── QE2.2 (fn_string: string/binary/LIKE/ILIKE/regex/encode)          ┐
  ├── QE2.3 (fn_time: now/date_trunc/date_bin/extract/Interval/ts cast) ├── 4 PARALLEL PHASES
  ├── QE2.4 (fn_misc: math/nulls/array/list_concat/map/JSON)            │
  └── QE2.5 (filter: Filter DSL validator + tombstone predicate)        ┘
        └── QE2.6 (merge lib.rs + binder + wire dispatch + DataFusion differential tests)
              └── QE2.7 (query_expr fuzz target + replay, Neon code review, closeout)
```

`QE2.2`, `QE2.3`, `QE2.4`, and `QE2.5` each create disjoint new modules inside `crates/query/src/` and add their own `pub mod ...;` line in `crates/query/src/lib.rs` so each parallel worktree runs `cargo test -p cetra-query` cleanly. When merging `QE2.2`–`QE2.5` into `qe-query-engine`, the coordinator resolves the mechanical 1-line `lib.rs` merge conflict before launching `QE2.6`.

| Phase | Edits | Depends on |
|---|---|---|
| QE2.1 Crate scaffold, errors, text tokenizer, `Expr` IR, core evaluator | root `Cargo.toml`, `Cargo.lock`, `AGENTS.md`, `crates/query/{Cargo.toml,src/{lib.rs,error.rs,text.rs,ir.rs,eval.rs}}` | M1 |
| QE2.2 String, binary, `LIKE`/`ILIKE`, regex, `encode` functions | `crates/query/src/{lib.rs,fn_string.rs}` | QE2.1 |
| QE2.3 Time, `Interval`, `date_trunc`, `date_bin`, `extract`, timestamp casts | `crates/query/src/{lib.rs,fn_time.rs}` | QE2.1 |
| QE2.4 Math, null helpers, array, list concat, map, JSON extraction functions | `crates/query/src/{lib.rs,fn_misc.rs}` | QE2.1 |
| QE2.5 Filter DSL validator and unified tombstone predicate | `crates/query/src/{lib.rs,filter.rs,tombstone.rs}` | QE2.1 |
| QE2.6 AST binder, function dispatch wiring, and DataFusion differential tests | `crates/query/src/{lib.rs,bind.rs,eval.rs}`, `crates/storage/{Cargo.toml,src/{lib.rs,query_diff_tests.rs}}`, `Cargo.lock` | QE2.2, QE2.3, QE2.4, QE2.5 |
| QE2.7 Fuzz target `query_expr`, Neon code review, and M2 closeout | `crates/query/src/{lib.rs,fuzz_entry.rs}`, `fuzz/Cargo.toml`, `fuzz/fuzz_targets/query_expr.rs`, `fuzz/corpus/query_expr/*`, `justfile`, `.github/workflows/nightly.yml`, `docs/roadmap.md` | QE2.6 |

## Cross-cutting invariants for every phase

1. **Safe Rust & lints:** `#![forbid(unsafe_code)]`, `#![warn(missing_docs)]`, no `unwrap()`/`expect()` outside tests (unless an adjacent invariant comment proves infallibility), zero `#![allow(...)]` lint suppressions on library code.
2. **Zero banned dependencies:** zero `tokio`, `arrow`, `parquet`, `datafusion`, or `object_store` in `crates/query`.
3. **Privacy (`AGENTS.md` § Privacy):** Error messages, `Display`, and `Debug` impls never print row payload values, prompt/completion text, or erased user/session IDs. Redacted tombstone `IN` lists format as `"{col} IN (<{n} redacted>)"` (single angle brackets, matching `crates/storage/src/deletion.rs:445`) in both `Display`/`fmt_plan()` and `Debug`. Caller-facing `InvalidQuery`/`Failed` messages are capped at `MESSAGE_CAP = 2048` bytes on a UTF-8 char boundary.
4. **Row length & active-row masking:**
   - `eval_expr` **always** returns a `Column` of length `batch.raw_len()` so it remains index-aligned with `batch.selection()`.
   - `eval_expr` derives `active: Option<Bitmap>` of length `batch.raw_len()` from `batch.selection()`:
     - `Selection::All` → `None` (every row `0..raw_len` is active);
     - `Selection::Mask(bm)` → borrows `Some(bm)`;
     - `Selection::Indices(idx)` → builds a `Bitmap` of length `raw_len` with bit `1` for every index in `idx`.
   - Every evaluator (`eval.rs`, `fn_string.rs`, `fn_time.rs`, `fn_misc.rs`) accepts `active: Option<&Bitmap>` and **MUST skip inactive rows** (bit `0` in `active`): never error (no div-by-zero, overflow, bad cast, bad regex, bad JSONPath, bad interval, or OOM amplification on inactive rows), never reserve amplification bytes for inactive rows, and write canonical null (validity bit `0`, dummy zero/empty value) into inactive output slots.
   - `CASE` branches evaluate lazily under shrinking `remaining: Bitmap` masks so `CASE WHEN x != 0 THEN 10 / x ELSE 0 END` never faults when `x == 0`. Similarly, `AND` evaluates `right` only on active rows where `left` is not `Some(false)`, and `OR` evaluates `right` only on active rows where `left` is not `Some(true)`.
5. **Unified `Reservation` accounting postcondition (enforced via `pub(crate)` helpers in QE2.1):**
   - Let `before = pool.as_deref().map_or(0, Reservation::size)` on entry to any `eval_*` function (`eval_expr`, `eval_filter`, `eval_like`, `eval_string_concat`, `eval_list_concat`, `eval_string_fn`, `eval_time_fn`, `eval_misc_fn`).
   - **Pre-allocation:** Before allocating an output buffer or amplification buffer, reserve `estimated_bytes` via `res.try_grow(estimated_bytes)?` so OOM fails *before* heap allocation with `QueryError::TooLarge`.
   - **On `Ok(col)`:** Reconcile `pool` to `before + col.memory_bytes()` via `eval::reconcile_output(pool, before, col.memory_bytes())?` (for `eval_filter`, `before + bitmap.memory_bytes()`). If reconciling upward fails with `MemoryLimit`, rollback to `before` and return `Err(QueryError::TooLarge)`.
   - **On `Err(_)`:** Rollback `pool` to `before` via `eval::rollback_to(pool, before)` — zero leaked bytes.
   - **Intermediate children in `eval_expr`:** While a parent holds child `Column`s, child reservations stay accounted in `pool`; once the parent builds its output and drops child `Column`s, the parent reconciles `pool` to `before + parent_out.memory_bytes()` (or rolls back to `before` on error).
6. **All 4 `Column` encodings:** Every evaluator MUST accept `Column::Flat`, `Column::Const`, `Column::Dict`, and `Column::Rle` — and for `FlatData::List { values, .. }`, the nested child `values: &Column` may also be any of the 4 encodings. Never reject non-`Flat` columns and never materialize a `Column::Const` into a full `raw_len` flat buffer when a scalar read suffices. QE2.1 provides lightweight `pub(crate)` per-row accessor helpers in `eval.rs` so QE2.2–QE2.4 handle all 4 encodings uniformly.
7. **3-valued SQL NULL logic & total-order `F64`:**
   - `AND`, `OR`, `NOT`, comparisons, `IN`, `BETWEEN`, `LIKE`/`ILIKE` follow exact SQL 3-valued logic (`NULL AND false = false`, `NULL OR true = true`, `IS [NOT] DISTINCT FROM` never yields `NULL`).
   - Floating-point comparisons and equality use Arrow/DataFusion total order (`eval::f64_cmp` / `eval::f64_key`): **`-0.0 == +0.0`**, **`NaN == NaN`**, and **`NaN > +inf`** (do **NOT** use raw `f64::total_cmp`, where `-0.0 < +0.0`).
8. **Cancellation & regex safety:**
   - Every evaluation checks `ctx.cancel.is_cancelled()` on entry and every `1024` rows inside loops, rolling back `pool` and returning `QueryError::Cancelled`.
   - Compiled regexes use `regex::RegexBuilder::new(pat).size_limit(10 * 1024 * 1024).dfa_size_limit(10 * 1024 * 1024).build()`; compile/size errors return `QueryError::Failed`.
9. **Machine load:** While parallel agents run, each runs only `cargo test -p cetra-query` + `cargo clippy -p cetra-query --all-targets -- -D warnings` + `cargo doc -p cetra-query --no-deps` + `cargo fmt --all -- --check` once right before committing.

---

## QE2.1 — Crate scaffold, errors, text tokenizer, `Expr` IR, and core evaluator

**Goal:** Create `crates/query` (`cetra-query`) with error types, `sqlparser` tokenizer/limit checker (exact parity with `query_text.rs`), typed `Expr` IR (covering all ~50 `ScalarFn` variants with redacted `Debug` & `Display`), shared `pub(crate)` reservation/column/float helpers, and the core vectorized batch evaluator for columns, literals, comparisons, boolean logic, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, `BETWEEN`, `IN`, checked arithmetic, `CASE`, `MapIndex`, and core `CAST`/`TRY_CAST`.

**Edit:**
- `Cargo.toml` (root): add `cetra-query = { path = "crates/query" }` and `sqlparser = { version = "0.62.0", features = ["visitor", "recursive-protection"] }` under `[workspace.dependencies]`.
- `Cargo.lock` (updated via `cargo metadata --format-version 1 > /dev/null`).
- `AGENTS.md`: add `query/` (`cetra-query`) to § Layout and § Crate dependency graph (`query` depends only on `column`).
- `crates/query/Cargo.toml` (new).
- `crates/query/src/{lib.rs,error.rs,text.rs,ir.rs,eval.rs}` (new).

### Contracts

`crates/query/Cargo.toml`:
```toml
[package]
name = "cetra-query"
description = "Pure-Rust vectorized expressions, Filter DSL, Shape B SQL planner, and push-pipeline executor"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true
license.workspace = true

[lints]
workspace = true

[dependencies]
cetra-column.workspace = true
chrono.workspace = true
hex.workspace = true
regex.workspace = true
serde_json.workspace = true
serde_json_path.workspace = true
sqlparser.workspace = true
thiserror.workspace = true

[dev-dependencies]
proptest.workspace = true
```

`crates/query/src/lib.rs` (exact exports in QE2.1 so QE2.2–QE2.5 compile cleanly in parallel):
```rust
#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Pure-Rust vectorized expressions, Filter DSL, and tombstone predicates for Cetra.

pub mod error;
pub mod eval;
pub mod ir;
pub mod text;

pub use error::{InvalidQuery, MESSAGE_CAP, Position, QueryError, cap_message};
pub use eval::{eval_expr, eval_filter};
pub use ir::{BinaryOp, CastMode, EvalContext, Expr, ScalarFn, UnaryOp};
pub use text::{
    DENIED_FUNCTIONS, FILTER_LIMITS, PARSER_RECURSION_LIMIT, SQL_LIMITS, TextKind, TextLimits,
};
```

`crates/query/src/error.rs`:
```rust
use cetra_column::ColumnError;
use thiserror::Error;

/// Maximum UTF-8 byte length of caller-facing query error messages (D94).
pub const MESSAGE_CAP: usize = 2048;

/// 1-based line and character column inside a filter or SQL string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// 1-based line number.
    pub line: u32,
    /// 1-based character column.
    pub column: u32,
}

/// Caller-facing parse, validation, or binding error (HTTP 400 `invalid_filter`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct InvalidQuery {
    /// Capped message (`<= MESSAGE_CAP` bytes). Never logged with user data.
    pub message: String,
    /// Optional 1-based source location.
    pub position: Option<Position>,
}

impl InvalidQuery {
    /// Builds an [`InvalidQuery`], capping `message` at [`MESSAGE_CAP`] on a UTF-8 char boundary.
    pub fn new(message: impl AsRef<str>, position: Option<Position>) -> Self;
}

/// Errors returned by `cetra-query`.
#[derive(Debug, Error)]
pub enum QueryError {
    /// Parse, grammar, schema, or type error (400 `invalid_filter`).
    #[error("{0}")]
    Invalid(#[from] InvalidQuery),
    /// Runtime execution error: division by zero, overflow, bad cast, regex too big, non-finite cost (400 `query_failed`).
    #[error("{0}")]
    Failed(String),
    /// Memory pool cap exceeded (422 `query_too_large`).
    #[error("query memory limit exceeded")]
    TooLarge,
    /// Query was cancelled via `CancelToken` (maps to timeout/abort in server).
    #[error("query cancelled")]
    Cancelled,
    /// Column invariant or internal error (never `ColumnError::MemoryLimit`, which converts to `TooLarge`).
    #[error("column error: {0}")]
    Column(ColumnError),
}

impl From<ColumnError> for QueryError {
    fn from(err: ColumnError) -> Self {
        match err {
            ColumnError::MemoryLimit { .. } => Self::TooLarge,
            other => Self::Column(other),
        }
    }
}

impl QueryError {
    /// Builds `QueryError::Failed`, capping `message` at [`MESSAGE_CAP`].
    pub fn failed(message: impl AsRef<str>) -> Self;
}

/// Truncates `text` to at most [`MESSAGE_CAP`] UTF-8 bytes on a char boundary.
pub fn cap_message(text: &str) -> String;
```

`crates/query/src/text.rs`:
- Port exact tokenization, operator counting (45 symbols + 23 keywords, `is_operator`), `PARSER_RECURSION_LIMIT = 50`, `FILTER_LIMITS { max_bytes: 4096, max_operators: 128 }`, `SQL_LIMITS { max_bytes: 65536, max_operators: 512 }`, `TextKind::{Filter, Sql}` (deriving `Debug, Clone, Copy, PartialEq, Eq`), `TextLimits` (deriving `Debug, Clone, Copy, PartialEq, Eq` — required by workspace `missing_debug_implementations = "warn"`), `tokenize`, `parser`, `parse_failure`, `expect_end`, `end_of`, `split_appended_location`, and `tokenizer_failure` verbatim from `crates/storage/src/query_text.rs:1-313` using `sqlparser::dialect::GenericDialect`.
- Export `pub const DENIED_FUNCTIONS: &[&str]` containing all 20 lowercase names from `crates/storage/src/views/guard.rs:38-59` (`range`, `generate_series`, `array_repeat`, `list_repeat`, `repeat`, `lpad`, `rpad`, `array_resize`, `list_resize`, `replace`, `regexp_replace`, `array_replace`, `array_replace_n`, `array_replace_all`, `list_replace`, `list_replace_n`, `list_replace_all`, `levenshtein`, `input_file_name`, `file_row_index`).

`crates/query/src/ir.rs`:
```rust
use std::fmt;

use cetra_column::{DataType, ScalarValue};

/// Binary operators in [`Expr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// Logical AND (3-valued).
    And,
    /// Logical OR (3-valued).
    Or,
    /// `=`
    Eq,
    /// `!=` / `<>`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `IS DISTINCT FROM` (null-safe `!=`, never returns NULL).
    IsDistinctFrom,
    /// `IS NOT DISTINCT FROM` (null-safe `=`, never returns NULL).
    IsNotDistinctFrom,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `||` (string or list concatenation).
    Concat,
    /// `@>` (`array_has_all`).
    ArrayContains,
}

/// Unary operators in [`Expr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Logical `NOT` (3-valued).
    Not,
    /// Arithmetic negation `-x`.
    Neg,
    /// `IS NULL` (never returns NULL).
    IsNull,
    /// `IS NOT NULL` (never returns NULL).
    IsNotNull,
    /// `IS TRUE` (`true` → `true`, `false`/`NULL` → `false`).
    IsTrue,
    /// `IS FALSE` (`false` → `true`, `true`/`NULL` → `false`).
    IsFalse,
    /// `IS NOT TRUE` (`true` → `false`, `false`/`NULL` → `true`).
    IsNotTrue,
    /// `IS NOT FALSE` (`false` → `false`, `true`/`NULL` → `true`).
    IsNotFalse,
}

/// Cast failure handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastMode {
    /// Strict SQL `CAST`: invalid conversion returns `QueryError::Failed`.
    Strict,
    /// Safe `TRY_CAST` (Filter DSL, D100): invalid conversion produces `NULL`.
    Try,
}

/// Curated Shape B scalar function identifier (all ~50 functions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarFn {
    // String & binary (QE2.2)
    Lower, Upper, Length, Substr, Trim, Ltrim, Rtrim, Left, Right, SplitPart,
    Concat, ConcatWs, StartsWith, EndsWith, Contains, Strpos, RegexpLike, RegexpExtract, Encode,
    // Time & interval (QE2.3)
    Now, DateTrunc, DateBin, Extract, ToTimestamp,
    // Math & null handling (QE2.4)
    Abs, Round, Floor, Ceil, Sqrt, Ln, Log10, Power, Coalesce, Nullif, Greatest, Least, IsNan, IsFinite,
    // Array & map (QE2.4)
    ArrayHas, ArrayHasAll, ArrayLength, ArrayElement, ArrayToString, ArrayDistinct, MapKeys, MapValues,
    // JSON (QE2.4)
    JsonGet, JsonGetText, JsonExtract, JsonExtractText,
}

/// Typed expression tree evaluated vectorized over a `cetra_column::Batch`.
///
/// Implements manual `fmt::Debug` and `fmt::Display` so `InList { redacted: true }`
/// never exposes erased user/session IDs in logs or `EXPLAIN` output.
#[derive(Clone, PartialEq)]
pub enum Expr {
    /// Reference to column at `index` in input batch schema.
    Column {
        /// Column index in the input `Schema`.
        index: usize,
        /// Column name (diagnostics / display only).
        name: String,
        /// Resolved data type.
        data_type: DataType,
    },
    /// Constant scalar value.
    Literal(ScalarValue),
    /// Unary operation.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        expr: Box<Expr>,
        /// Result type.
        data_type: DataType,
    },
    /// Binary operation.
    Binary {
        /// Left operand.
        left: Box<Expr>,
        /// Operator.
        op: BinaryOp,
        /// Right operand.
        right: Box<Expr>,
        /// Result type.
        data_type: DataType,
    },
    /// `CAST` or `TRY_CAST`.
    Cast {
        /// Source expression.
        expr: Box<Expr>,
        /// Target type.
        target: DataType,
        /// Strict or try-cast mode.
        mode: CastMode,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// Tested value.
        expr: Box<Expr>,
        /// Lower bound (inclusive).
        low: Box<Expr>,
        /// Upper bound (inclusive).
        high: Box<Expr>,
        /// True for `NOT BETWEEN`.
        negated: bool,
    },
    /// `expr [NOT] IN (list...)`.
    InList {
        /// Tested value.
        expr: Box<Expr>,
        /// Candidate values.
        list: Vec<Expr>,
        /// True for `NOT IN`.
        negated: bool,
        /// When true, `Display`, `fmt_plan`, and `Debug` render `<col> IN (<{n} redacted>)` (GDPR redaction, D96).
        redacted: bool,
    },
    /// `expr [NOT] LIKE / ILIKE pattern [ESCAPE escape]`.
    Like {
        /// Input string expression.
        expr: Box<Expr>,
        /// Pattern string expression.
        pattern: Box<Expr>,
        /// Optional single-char escape (`Some(c)` escapes with `c`, `None` disables escaping; SQL default is `Some('\\')`).
        escape: Option<char>,
        /// True for `ILIKE` (Unicode case-insensitive).
        case_insensitive: bool,
        /// True for `NOT LIKE` / `NOT ILIKE`.
        negated: bool,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    Case {
        /// Optional base operand for simple `CASE x WHEN ...`.
        operand: Option<Box<Expr>>,
        /// `(condition_or_match, result)` branches.
        when_then: Vec<(Expr, Expr)>,
        /// Optional `ELSE` branch (defaults to `NULL` of `data_type`).
        else_expr: Option<Box<Expr>>,
        /// Resolved result type.
        data_type: DataType,
    },
    /// Map lookup `map_col['key']` (returns nullable `Utf8`).
    MapIndex {
        /// Map column expression (`DataType::MapUtf8`).
        map: Box<Expr>,
        /// Key expression (`DataType::Utf8`).
        key: Box<Expr>,
    },
    /// Dynamic array construction `[expr1, expr2, ...]` (constant arrays fold to `Literal(ScalarValue::List { .. })`).
    MakeArray {
        /// Element expressions (all coerced to `item_type`).
        elems: Vec<Expr>,
        /// Element `DataType` (the expression's own type is `DataType::List(Box::new(item_type))`).
        item_type: DataType,
    },
    /// Curated scalar function call.
    Call {
        /// Function identifier.
        func: ScalarFn,
        /// Evaluated arguments.
        args: Vec<Expr>,
        /// Resolved return type.
        data_type: DataType,
    },
}

impl Expr {
    /// Concrete [`DataType`] produced by evaluating this expression.
    pub fn data_type(&self) -> DataType;
    /// Formats a privacy-safe plan summary (respecting `redacted: true` on `InList`, formatting `"{expr} IN (<{n} redacted>)"`).
    pub fn fmt_plan(&self) -> String;
}
// Also implement `fmt::Display` (delegating to `fmt_plan`) and manual `fmt::Debug`
// (formatting `InList { redacted: true }` with `list: "<{n} redacted>"` instead of its element values).

/// Evaluation runtime context passed to [`crate::eval::eval_expr`] and [`crate::eval::eval_filter`].
#[derive(Debug, Clone)]
pub struct EvalContext {
    /// Query start instant in UTC nanoseconds since Unix epoch (used by `now()`).
    pub now_ns: i64,
    /// Cooperative cancellation flag.
    pub cancel: cetra_column::CancelToken,
}
```

`crates/query/src/eval.rs`:
- **Public API:**
  - `pub fn eval_expr(expr: &Expr, batch: &Batch, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Column, QueryError>`
  - `pub fn eval_filter(expr: &Expr, batch: &Batch, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Bitmap, QueryError>`
    - Rejects any expression whose `data_type()` is neither `DataType::Bool` nor `DataType::Null` with `QueryError::failed("filter predicate must evaluate to boolean")`.
    - Returns a `Bitmap` of length `batch.raw_len()` where bit `1` = active AND `Some(true)`, bit `0` = inactive OR `Some(false)` OR `NULL`.
- **Shared `pub(crate)` helpers exported for QE2.2–QE2.4:**
  - `pub(crate) fn pool_size(pool: &Option<&mut Reservation>) -> usize`
  - `pub(crate) fn try_reserve(pool: &mut Option<&mut Reservation>, bytes: usize) -> Result<(), QueryError>`
  - `pub(crate) fn rollback_to(pool: &mut Option<&mut Reservation>, before: usize)`
  - `pub(crate) fn reconcile_output(pool: &mut Option<&mut Reservation>, before: usize, actual_bytes: usize) -> Result<(), QueryError>`
  - `pub(crate) fn is_active(active: Option<&Bitmap>, row: usize) -> bool`
  - `pub(crate) fn active_count(active: Option<&Bitmap>, len: usize) -> usize`
  - `pub(crate) fn check_cancel(ctx: &EvalContext, pool: &mut Option<&mut Reservation>, before: usize) -> Result<(), QueryError>`
  - `pub(crate) fn f64_cmp(a: f64, b: f64) -> std::cmp::Ordering` and `pub(crate) fn f64_canonical_bits(v: f64) -> u64`:
    - Canonicalizes `-0.0` → `+0.0`, canonicalizes all `NaN`s to a single bit pattern, and orders `NaN > +inf` (`-0.0 == +0.0`, `NaN == NaN`, matching Arrow/DataFusion total order).
  - Per-row encoding-agnostic readers over `&Column` (supporting `Flat`, `Const`, `Dict`, `Rle` without materializing `Const` or cloning large buffers): `read_bool(col, row)`, `read_i64(col, row)`, `read_f64(col, row)`, `read_decimal(col, row)`, `read_timestamp(col, row)`, `read_date(col, row)`, `read_interval(col, row)`, `read_str(col, row) -> Option<&str>`, `read_bytes(col, row) -> Option<&[u8]>`, `read_scalar(col, row) -> ScalarValue`, plus zero-copy borrowed enums `pub(crate) enum ListRef<'a> { Flat { values: &'a Column, start: usize, end: usize }, Const(&'a [ScalarValue]) }` (`list_ref_at(col, row) -> Option<ListRef<'_>>`) and `pub(crate) enum MapRef<'a> { Flat { data: &'a MapData, start: usize, end: usize }, Const(&'a [(String, Option<String>)]) }` (`map_ref_at(col, row) -> Option<MapRef<'_>>`).
- **Core evaluation semantics in QE2.1:**
  - `Column`, `Literal` (returns `Column::Const { value, len: raw_len }`), `MakeArray` (evaluates each element expression under `active` and builds a `FlatData::List { item_type, offsets, values }` column of length `raw_len` with null validity for inactive rows).
  - `Unary`: `Not` (3-valued), `Neg` (checked `-i64::MIN` → `Failed`; `F64`; `Decimal128` checked `-i128::MIN` → `Failed`; `Interval` negates `months`/`days`/`nanos` checked), `IsNull`, `IsNotNull`, `IsTrue`, `IsFalse`, `IsNotTrue`, `IsNotFalse` (all `IS *` ops never return `NULL` on active rows!).
  - `Binary`:
    - `And` / `Or`: 3-valued SQL logic with masked short-circuiting of `right`.
    - `Eq`, `NotEq`, `Lt`, `LtEq`, `Gt`, `GtEq`, `IsDistinctFrom`, `IsNotDistinctFrom`: over all scalar types (`Bool`, `I64`, `F64` via `f64_cmp`, `Decimal128`, `Utf8`, `Binary`, `FixedBytes8`, `FixedBytes16`, `TimestampNs`, `Date`, `Interval`, `List` lexicographic, `Null`).
    - `Add`, `Sub`, `Mul`, `Div`, `Mod`:
      - `I64`: checked overflow (`i64::MAX + 1`, `i64::MIN / -1`, `i64::MIN % -1` → `Failed`), division/modulo by `0` → `Failed`.
      - `F64`: `Div` and `Mod` by `0.0` (including `-0.0`) return `QueryError::failed("division by zero")` (matching DataFusion `div_checked`); `Add`/`Sub`/`Mul` follow IEEE 754.
      - `Decimal128 { precision, scale }`: operands are already aligned to `(p, s)` by `bind.rs`; `Add`/`Sub` checked `i128`; `Mul` `(a * b) / 10^s` rounded half-away-from-zero with checked `i128` overflow against `10^p`; `Div` `(a * 10^s) / b` with div-by-zero → `Failed`; `Mod` `a % b` with div-by-zero → `Failed`.
  - `Between`: evaluates `expr`, `low`, `high` and combines via exact SQL 3-valued `(expr >= low) AND (expr <= high)` (so `5 BETWEEN 10 AND NULL` is `false`, `5 BETWEEN 1 AND NULL` is `NULL`); applies 3-valued `NOT` if `negated`.
  - `InList`:
    - Empty `list` → `false` (or `true` if `negated`) even when LHS is `NULL`.
    - When every item in `list` is `Expr::Literal(_)`, build a typed `HashSet` (plus `has_null: bool`) supporting `Bool`, `I64`, canonical `F64`, `Decimal128`, `Utf8`, `Binary`, `FixedBytes8`, `FixedBytes16`, `TimestampNs`, `Date`: match → `true` (`false` if negated); no match & `has_null` → `NULL`; no match & `!has_null` → `false` (`true` if negated); `NULL` LHS → `NULL`.
    - When `list` contains non-literal expressions, evaluate each item and apply exact SQL 3-valued `OR` of `lhs = item_k` (negated if `NOT IN`).
  - `Case`: masked short-circuit evaluation under shrinking `remaining: Bitmap`; for simple `CASE operand WHEN w THEN t`, compare `operand = w` (note `NULL = NULL` is `NULL`, not matched — matching SQL standard).
  - `MapIndex`: supports `Flat`/`Const`/`Dict`/`Rle` map and key columns; returns `NULL` when map is `NULL`, key is `NULL`, key is absent, or matched entry has `value_nulls = 0`; duplicate-key tie-breaker returns the first matching entry.
  - `Cast` / `TryCast` matrix (core types in QE2.1; `TimestampNs`/`Date` string parsing wired in QE2.6):
    - Identity cast (same `DataType`, or `TimestampNs { utc: a }` → `TimestampNs { utc: b }` which preserves `i64` ns) is zero-copy/metadata-only.
    - `Null` → any target `T`: `Column::Const { value: ScalarValue::Null(T), len }`.
    - `I64 ↔ F64`: `I64 -> F64` is `v as f64`; `F64 -> I64` truncates toward zero, rejecting `NaN`, `±inf`, or values outside `[i64::MIN as f64, 9223372036854775808.0)` (`Strict` → `Failed`, `Try` → `NULL`; never saturates!).
    - `I64`/`F64` ↔ `Decimal128 { p, s }`: scales by `10^s` checking bounds `|v| < 10^p`; `Decimal128 -> Decimal128` rescales with rounding/overflow check.
    - `Bool` ↔ `I64`: `true -> 1`, `false -> 0`; `0 -> false`, non-zero -> `true`.
    - `Utf8 -> Bool`: trims ASCII whitespace, lowercases; accepts `"true"|"t"|"yes"|"y"|"1"|"on"` → `true`, `"false"|"f"|"no"|"n"|"0"|"off"` → `false`; anything else fails (`Strict` → `Failed`, `Try` → `NULL`).
    - `Utf8 -> I64`/`F64`/`Decimal128`: trims ASCII whitespace, parses; `Utf8 -> F64` accepts `NaN`/`inf`/`-inf`? Match Rust `str::parse::<f64>()` (rejects invalid strings).
    - Any scalar → `Utf8`: `Bool` `"true"`/`"false"`, `I64` decimal, `F64` standard decimal, `Decimal128` fixed-scale decimal, `Binary`/`FixedBytes8`/`FixedBytes16` lowercase hex, `Date` `"YYYY-MM-DD"`, `TimestampNs` RFC 3339 UTC (wired via `fn_time` in QE2.6).
    - `Utf8 -> Binary`/`FixedBytes8`/`FixedBytes16`: `Utf8 -> Binary` takes UTF-8 bytes; `Utf8 -> FixedBytes8`/`FixedBytes16` decodes exact 16/32-char hex (case-insensitive) else fails (`Strict` → `Failed`, `Try` → `NULL`).
    - `Binary -> Utf8`: `std::str::from_utf8` (invalid UTF-8 → `Failed`/`NULL`).
    - `I64 -> TimestampNs`: treats `I64` as epoch nanoseconds directly (matching Arrow/DataFusion `CAST(i64 AS TIMESTAMP)`). `TimestampNs -> I64`: returns `i64` nanoseconds.
    - `Date <-> TimestampNs`: `Date -> TimestampNs` = `(days as i64).checked_mul(86_400_000_000_000)` (`Strict` → `Failed`, `Try` → `NULL` on overflow — never panics in debug builds); `TimestampNs -> Date` = `ns.div_euclid(86_400_000_000_000)` checked into `i32`.
  - **Temporary QE2.1 arms (removed in QE2.6):** `Expr::Like`, `Expr::Call`, `BinaryOp::Concat`, `BinaryOp::ArrayContains`, `Utf8 -> TimestampNs`/`Date` `Cast`, `TimestampNs -> Utf8` `Cast`, and `TimestampNs ± Interval` return `Err(QueryError::failed("not yet wired"))` until QE2.6 connects `fn_string`/`fn_time`/`fn_misc` (zero `todo!()` macros). QE2.6 asserts zero `"not yet wired"` arms remain.

### Steps

1. Update root `Cargo.toml` (`cetra-query`, `sqlparser = { version = "0.62.0", features = ["visitor", "recursive-protection"] }`), run `cargo metadata --format-version 1 > /dev/null`, and update `AGENTS.md`.
2. Create `crates/query/Cargo.toml`, `lib.rs`, `error.rs`, `text.rs`, `ir.rs`, `eval.rs`.
3. Implement `eval_expr` and `eval_filter` thoroughly with active-row masking, `CASE`/`AND`/`OR` short-circuiting, 3-valued logic, `f64_cmp` (`-0.0 == +0.0`, `NaN == NaN`), checked overflow, div-by-zero → `Failed`, `InList` literal hash-set optimization, `MapIndex` across all 4 encodings, `Cast`/`TryCast`, `CancelToken`, and exact `Reservation` reconcile/rollback.

### Tests

- `text::tests::limits_operators_and_positions` — verifies `FILTER_LIMITS` (4 KiB / 128 ops), `SQL_LIMITS` (64 KiB / 512 ops), quoted keyword `"union"` = 0 ops, recursion limit 50, `expect_end`, `split_appended_location`, `tokenizer_failure`, and 1-based `Position`.
- `eval::tests::three_valued_boolean_and_null_safe_distinct` — full truth table for `AND`, `OR`, `NOT`, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, `IS [NOT] TRUE/FALSE`, `IN`/`NOT IN` with `NULL` and empty `IN ()`, `5 BETWEEN 10 AND NULL == false`.
- `eval::tests::arithmetic_overflow_div_zero_f64_total_order_and_casts` — div-by-zero (`I64` and `F64`) → `Failed`, `i64::MAX + 1` / `i64::MIN / -1` / `-i64::MIN` → `Failed`, `-0.0 == +0.0`, `NaN == NaN`, `NaN > +inf`, `CAST('abc' AS DOUBLE)` → `Failed`, `TRY_CAST('abc' AS DOUBLE)` → `NULL`, `CAST(NaN AS BIGINT)` → `Failed`, `TRY_CAST(NaN AS BIGINT)` → `NULL`.
- `eval::tests::case_short_circuit_selection_masking_map_index_and_cancellation` — `CASE WHEN x != 0 THEN 10 / x ELSE 0 END` with `x = 0` row; masked-out `Selection::Mask` and `Selection::Indices` rows with `x = 0` never fault; `MapIndex` on `Flat`/`Const`/`Dict`/`Rle` maps (present, missing, null value, duplicate keys); `CancelToken` abort; `Reservation` OOM with `pool.size() == before` on `Err` and `== before + col.memory_bytes()` on `Ok`.
- `eval::tests::in_list_redacted_display_and_debug_hide_values` — asserts both `fmt_plan()`, `Display`, and `format!("{expr:?}")` contain `<2 redacted>` (single brackets) and never contain the erased user IDs.

### Done when

- `cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.
- `cargo tree -p cetra-query` shows zero `tokio`, `arrow`, `parquet`, `datafusion`, `object_store`.

**Commit:** `query: scaffold cetra-query with text limits, Expr IR, and core evaluator`

---

## QE2.2 — String, binary, `LIKE`/`ILIKE`, regex, and `encode` functions (`fn_string.rs`) *(parallel with QE2.3, QE2.4, QE2.5)*

**Goal:** Implement all 19 string & binary scalar functions, `||` string concatenation (with 2 GiB cap), `LIKE`/`ILIKE ... ESCAPE` (Unicode case-folding), and size-capped `regexp_like`/`regexp_extract`, honoring `active: Option<&Bitmap>`, all 4 `Column` encodings, and the exact `Reservation` postcondition (including `concat_ws` amplification guard).

**Edit:**
- `crates/query/src/fn_string.rs` (new).
- `crates/query/src/lib.rs`: add `pub mod fn_string;`.

### Contracts

`crates/query/src/fn_string.rs`:
- `pub fn eval_like(input: &Column, pattern: &Column, escape: Option<char>, case_insensitive: bool, negated: bool, active: Option<&Bitmap>, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Column, QueryError>`
  - `escape: Some(c)` escapes the next character with `c`; `escape: None` disables escaping. A trailing lone escape character at the end of `pattern` matches literal `c` (matching DataFusion / `arrow-string`).
  - Compiles constant `pattern` once (exact/prefix/suffix/contains fast paths when no interior wildcards, else compiled wildcard/regex matcher with Unicode case-folding for `ILIKE`). `%` matches any sequence including newlines (`\n`); `_` matches any single Unicode scalar value (including `\n`).
- `pub fn eval_string_concat(left: &Column, right: &Column, active: Option<&Bitmap>, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Column, QueryError>`
  - SQL `||` on strings: either operand `NULL` → `NULL`. Enforces 2 GiB cap (`i32::MAX as usize`, returning `QueryError::failed("string concatenation result is larger than 2 GiB")`), pre-reserves exact output bytes for active rows, and reconciles to `before + out.memory_bytes()`.
- `pub fn eval_string_fn(func: ScalarFn, args: &[Column], len: usize, active: Option<&Bitmap>, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Column, QueryError>` covering:
  - `Lower`, `Upper`: Unicode `.to_lowercase()` / `.to_uppercase()`.
  - `Length`: Unicode scalar value count (`.chars().count() as i64`).
  - `Substr(s, start [, count])`: Postgres/DataFusion 1-based char semantics — window `[start, start + count)` clamped to `>= 1` (`substr('abcde', 0, 3) == "ab"`, `substr('abcde', -1, 4) == "ab"` — negative `start` does **NOT** count from the end!). Negative `count` → `QueryError::failed("negative substring length not allowed")`.
  - `Trim` / `Ltrim` / `Rtrim`: 1-arg form trims **ASCII space `' '` only** (matching Postgres/DataFusion — NOT Rust `str::trim()`!). 2-arg form `trim(s, chars)` trims any char in `chars`.
  - `Left(s, n)` / `Right(s, n)`: positive `n` takes first/last `n` chars; negative `n` drops `|n|` chars from the opposite end (`left('abcde', -2) == "abc"`, `right('abcde', -2) == "cde"`); `|n|` exceeding length returns `""` (negative) or whole string (positive).
  - `SplitPart(s, delim, n)`: 1-based part index; negative `n` counts from end (`-1` = last); `n == 0` → `QueryError::failed("field position must not be zero")`; out-of-range → `""`; empty `delim == ""` returns `s` for `n == 1` or `n == -1`, else `""`.
  - `Concat(a1, ...)`: 1+ args (non-`Utf8` args already cast by binder; NULL treated as `""`); enforces 2 GiB cap and reserves exact output bytes.
  - `ConcatWs(sep, a1, ...)`: `sep` `NULL` → `NULL`; NULL value args skipped; pre-reserves separator amplification bytes `longest_string(sep) * args.len().saturating_sub(2) * active_rows` (matching `views/guard.rs:435-439`) + payload bytes, enforces 2 GiB cap, reconciles on `Ok` / rolls back on `Err`.
  - `StartsWith`, `EndsWith`, `Contains`: exact UTF-8 byte/substring match, SQL `NULL` propagation.
  - `Strpos(s, sub)`: 1-based Unicode char position, `0` when absent, `strpos(s, "") == 1` for any non-null `s`.
  - `RegexpLike(s, pat [, flags])`: optional flags string supporting `'i'`, `'m'`, `'s'` (invalid flag → `QueryError::Failed`); caches compiled `Regex` when `pat` & `flags` are `Column::Const`; enforces 10 MiB regex size/DFA limit → `QueryError::Failed`.
  - `RegexpExtract(s, pat [, flags])`: if compiled regex has capture groups (`captures_len() > 1`), returns capture group 1 (`NULL` if group 1 did not participate in the match, e.g. `(a)|(b)` matching `"b"`); if 0 capture groups, returns full match (`group 0`); no match → `NULL`; enforces 10 MiB limit → `QueryError::Failed`.
  - `Encode(val, encoding)`: accepts `Binary`, `FixedBytes8`, `FixedBytes16`, **and `Utf8`** (matching `views/session.rs:127` `encode(name, 'hex')`); `'hex'` (case-insensitive) produces lowercase hex; any other encoding name → `QueryError::Failed`.

### Tests

- `fn_string::tests::like_and_ilike_with_escape_and_unicode` — `%`, `_` (including `\n`), `Some('\\')`, `None`, trailing lone escape, literal `\%`/`\_`, accents (`Café` ILIKE `%café%`), CJK, emoji, `NULL` propagation, `NOT ILIKE`, inactive rows skipped.
- `fn_string::tests::string_functions_edge_cases` — `substr('abcde', 0, 3) == "ab"`, `substr('abcde', -1, 4) == "ab"`, negative length → `Failed`, `trim("  \ta \n ") == "\ta \n"` (ASCII space only!), `split_part` positive/negative/0/empty-delim, `strpos('abc', '') == 1`, `concat` vs `concat_ws` null semantics, `left`/`right` negative `n`, `encode` hex on `FixedBytes16`/`FixedBytes8`/`Binary`/`Utf8`, `regexp_extract` with/without capture groups.
- `fn_string::tests::regex_size_limit_and_concat_memory_cap` — `(((a{100}){100}){100})` → `QueryError::Failed`; tight `Reservation` on `concat`/`concat_ws` → `QueryError::TooLarge` with `pool.size() == before`; `Ok` reconciles to `before + col.memory_bytes()`; masked-out row with invalid regex or `split_part(..., 0)` does not fault.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: implement string, binary, LIKE/ILIKE, regex, and encode functions`

---

## QE2.3 — Time, `Interval`, `date_trunc`, `date_bin`, `extract`, and timestamp casts (`fn_time.rs`) *(parallel with QE2.2, QE2.4, QE2.5)*

**Goal:** Implement all calendar-aware time/interval parsing, formatting, arithmetic, timestamp casts, and time functions (`now`, `date_trunc`, `date_bin`, `extract`/`date_part`, `to_timestamp`) with UTC nanosecond precision, honoring `active: Option<&Bitmap>`, all 4 `Column` encodings, and exact `Reservation` accounting.

**Edit:**
- `crates/query/src/fn_time.rs` (new).
- `crates/query/src/lib.rs`: add `pub mod fn_time;`.

### Contracts

`crates/query/src/fn_time.rs`:
- `pub fn parse_interval(text: &str) -> Result<Interval, QueryError>` — parses PostgreSQL/DataFusion interval strings (`'1 hour'`, `'30 minutes'`, `'7 days'`, `'1 month'`, `'2 years 3 days 4 hours 5 seconds'`, `'500 milliseconds'`, `'100 microseconds'`, `'10 nanoseconds'`, `'1 week'`, negative signs, singular/plural/abbreviations `ns`, `us`, `ms`, `s`/`sec`, `m`/`min`, `h`/`hr`, `d`, `w`, `mon`, `y`/`yr`, and bare numeric values when paired with a `leading_field` in `bind.rs`).
- `pub fn parse_timestamp_ns(text: &str, utc: bool) -> Result<i64, QueryError>` — trims ASCII whitespace and parses:
  - RFC 3339 with offset or `Z` (`'2026-01-01T10:00:03.123Z'`, `'2026-01-01T12:00:03+02:00'`): converted to UTC epoch nanoseconds (both `utc = true` and `utc = false` convert an explicit offset to UTC instants, matching DataFusion);
  - Offset-less `'YYYY-MM-DD[T| ]HH:MM:SS[.f{1..9}]'` and date-only `'YYYY-MM-DD'`: interpreted as UTC (midnight UTC for date-only) into `i64` nanoseconds since epoch, checked against `i64` overflow.
- `pub fn format_timestamp_ns(ns: i64, utc: bool) -> String` — formats `i64` UTC ns as RFC 3339 (`YYYY-MM-DDTHH:MM:SS[.fff...]Z` when `utc == true`, or `YYYY-MM-DDTHH:MM:SS[.fff...]` when `utc == false`, stripping trailing fractional zeros).
- `pub fn parse_date_days(text: &str) -> Result<i32, QueryError>` — parses `'YYYY-MM-DD'` into days since 1970-01-01.
- `pub fn format_date_days(days: i32) -> String` — formats days since 1970-01-01 as `'YYYY-MM-DD'`.
- `pub fn add_interval(ts_ns: i64, interval: Interval, subtract: bool) -> Result<i64, QueryError>` — exact calendar month addition (clamping end-of-month, e.g. Jan 31 + 1 month → Feb 28/29 via `chrono::Months`), then `+ days * 86_400_000_000_000 + nanos` with checked arithmetic (`Failed` on overflow).
- `pub fn extract_return_type(field: &str) -> Result<DataType, QueryError>` — canonical field validation & static return type (called by both `bind.rs` and `eval_time_fn`, case-insensitive, matching DataFusion 55.2 `date_part.rs`):
  - `year`, `quarter`, `month`, `week`, `day`, `dow` (0=Sun..6=Sat), `doy` (1..366), `hour`, `minute`, `second` (integer `0..59`), `millisecond` (integer `sec * 1_000 + nanos / 1_000_000`), `microsecond` (integer `sec * 1_000_000 + nanos / 1_000`), `nanosecond` (integer `sec * 1_000_000_000 + nanos`) → `DataType::I64`.
  - `epoch` (fractional seconds since Unix epoch) → `DataType::F64`.
  - Unknown field → `QueryError::failed(...)`.
- `pub fn eval_time_fn(func: ScalarFn, args: &[Column], len: usize, active: Option<&Bitmap>, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Column, QueryError>` covering:
  - `Now`: `args` is `&[]`; returns `Column::Const { value: ScalarValue::TimestampNs { value: ctx.now_ns, utc: true }, len }`.
  - `DateTrunc(unit, ts)`: units `nanosecond`, `microsecond`, `millisecond`, `second`, `minute`, `hour`, `day`, `week` (Monday 00:00 UTC, ISO-8601 matching Postgres/DataFusion), `month`, `quarter`, `year` (case-insensitive); preserves `ts`'s `utc` flag; unknown unit → `Failed`.
  - `DateBin(stride, source [, origin])`: `stride: Interval`, `source: TimestampNs`, optional `origin: TimestampNs` (default `0`, epoch-aligned matching spec 05 § Buckets). Rejects zero, negative, or month-bearing (`months != 0`) stride with `QueryError::Failed`. Uses Euclidean floor division (`div_euclid`) so pre-1970 timestamps bin cleanly.
  - `Extract(field_const_utf8, source)`: accepts `TimestampNs`, `Date` (midnight UTC), and `Interval`; produces `I64` or `F64` per `extract_return_type` (non-constant `field` rejected with `QueryError::Failed`).
  - `ToTimestamp(val)`: converts `I64` (epoch seconds, checked `* 1_000_000_000`), `F64` (fractional epoch seconds, rejecting `NaN`/`±inf`/overflow), or `Utf8` (`parse_timestamp_ns(s, true)`) to `TimestampNs { utc: true }`.

### Tests

- `fn_time::tests::parse_interval_and_calendar_arithmetic` — leap years, Jan 31 + 1 month, negative intervals, compound intervals, overflow → `Failed`.
- `fn_time::tests::date_trunc_and_date_bin_epoch_alignment` — every unit including `week` (Monday UTC), `quarter`, `date_bin` with explicit & default origin, pre-epoch timestamps, zero/negative/month stride → `Failed`.
- `fn_time::tests::timestamp_parsing_and_formatting` — `'2026-01-01T10:00:03Z'`, `'2026-01-01T12:00:03+02:00'`, `'2026-01-01 10:00:03'`, `'2026-01-01'`, invalid string → `Failed`, round-trip `format_timestamp_ns`.
- `fn_time::tests::extract_fields_types_and_now_uses_ctx` — integer fields (including `second`, `millisecond`, `microsecond`, `nanosecond`) return `I64`, `epoch` returns `F64`, `TimestampNs` + `Date` + `Interval` sources, `Now` with `args = &[]` and `len = 5`, inactive row with invalid stride/timestamp does not fault, `Reservation` postcondition holds.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: implement time, calendar interval, date_trunc, date_bin, and extract functions`

---

## QE2.4 — Math, null helpers, array, list concat, map, and JSON extraction functions (`fn_misc.rs`) *(parallel with QE2.2, QE2.3, QE2.5)*

**Goal:** Implement all math, null-handling, array (including `||` list concatenation), map, and JSON extraction scalar functions, honoring `active: Option<&Bitmap>`, all 4 `Column` encodings (including nested `FlatData::List { values: Box<Column> }`), and exact `Reservation` accounting (including `array_to_string` amplification guard).

**Edit:**
- `crates/query/src/fn_misc.rs` (new).
- `crates/query/src/lib.rs`: add `pub mod fn_misc;`.

### Contracts

`crates/query/src/fn_misc.rs`:
- `pub fn eval_list_concat(left: &Column, right: &Column, active: Option<&Bitmap>, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Column, QueryError>`
  - Concatenates two `List(T)` columns (matching DataFusion `array_concat` / `||` on lists, used by `views/guard.rs:770`): `NULL || [1] -> [1]`, `[1] || NULL -> [1]`, `NULL || NULL -> NULL`. Pre-reserves total output list bytes and enforces `Reservation` postcondition.
- `pub fn eval_misc_fn(func: ScalarFn, args: &[Column], len: usize, active: Option<&Bitmap>, ctx: &EvalContext, pool: Option<&mut Reservation>) -> Result<Column, QueryError>` covering:
  - **Math:**
    - `Abs`: `I64` (`i64::MIN` → `Failed`), `F64` (`v.abs()`), `Decimal128` (`i128::MIN` → `Failed`).
    - `Round(x [, dp])`: `F64` (half-away-from-zero matching Postgres/DataFusion; supports negative `dp: I64`, e.g. `round(1234.0, -2) == 1200.0`; `NaN`/`±inf` pass through) and `I64` (returns `I64` or `F64` matching input type).
    - `Floor`, `Ceil`: `F64` → `F64` (or `I64` → `I64` identity).
    - `Sqrt`, `Ln`, `Log10`: `F64` → `F64` following IEEE 754 / DataFusion (`sqrt(-1.0) = NaN`, `ln(0.0) = -inf`, `ln(-1.0) = NaN`).
    - `Power(base, exp)`: `F64` → `F64` (`base.powf(exp)`).
    - `IsNan(x)` / `IsFinite(x)`: `F64` → `Bool`; returns `NULL` on `NULL` input (matching DataFusion).
  - **Null helpers:**
    - `Coalesce(a1, ...)`: first non-null across `args` (all coerced to common `DataType` by binder).
    - `Nullif(a, b)`: if `a` is `NULL` → `NULL`; if `b` is non-null and `a == b` (using `f64_cmp` for `F64`) → `NULL`; else `a`.
    - `Greatest(a1, ...)` / `Least(a1, ...)`: ignores `NULL` args when any non-null arg exists, returns `NULL` only if all args are `NULL` (uses `f64_cmp` for `F64`, matching DataFusion).
  - **Array:**
    - `ArrayHas(list, elem)`: `NULL` list **or `NULL` elem** → `NULL` (matching DataFusion 55.2 `array_has.rs`); otherwise returns `true` iff `list` contains a non-null element equal to `elem`.
    - `ArrayHasAll(list, sub)` (powers `@>`): `NULL` list or `NULL` sub → `NULL`; empty `sub` → `true`; otherwise `true` iff every element in `sub` (including `NULL` if `sub` contains `NULL`, matching DataFusion 55.2!) is present in `list`.
    - `ArrayLength(list [, dim])`: `I64`; `NULL` list → `NULL`; empty `[]` → `0` (matching DataFusion); optional 2nd `dim: I64` returns length for `dim == 1` and `NULL` otherwise.
    - `ArrayElement(list, idx)`: 1-based SQL indexing; positive `idx` returns `1`-based element; **negative `idx` counts from the end (`-1` = last element, matching DataFusion)**; `idx == 0` or out-of-bounds or `NULL` list/idx → `NULL`.
    - `ArrayToString(list, delim [, null_str])`: `NULL` list or `NULL` delim → `NULL`; when `null_str` is omitted or `NULL`, `NULL` list elements are skipped; when `null_str` is `Some(s)`, `NULL` elements render as `s`; formats non-`Utf8` and nested list elements cleanly; pre-reserves `(delim.len() + null_str.len()) * active_leaf_count` amplification bytes up front (matching `views/guard.rs:475-495`) and enforces `Reservation` postcondition.
    - `ArrayDistinct(list)`: preserves first-seen order; deduplicates multiple `NULL` elements into a single `NULL` at its first-seen position.
  - **Map:**
    - `MapKeys(map)` → `List(Utf8)` (nullable `NULL` map → `NULL`).
    - `MapValues(map)` → `List(Utf8)` (nullable `NULL` map → `NULL`; preserves per-entry null values in child `Utf8` column!).
  - **JSON** (note: `->`, `->>`, `json_extract`, `json_extract_text` are Cetra-curated functions not built into DataFusion, verified by thorough unit tests here):
    - `JsonGet(json_str, key_or_idx)` (`->`): parses `Utf8` JSON (returns `NULL` on `NULL` input, invalid JSON, missing key, or negative/out-of-bounds index); extracts object field (`Utf8` key) or 0-based array element (`I64` index), returning JSON-serialized `Utf8` (JSON `null` returns `Some("null")`).
    - `JsonGetText(json_str, key_or_idx)` (`->>`): same lookup, unquoting JSON strings to plain text, formatting numbers/bools/arrays/objects via JSON text, and returning SQL `NULL` for JSON `null`.
    - `JsonExtract(json_str, path)` & `JsonExtractText(json_str, path)`: compiles RFC 9535 JSONPath (`serde_json_path::JsonPath::parse`, cached when `path` is `Column::Const`; invalid JSONPath → `QueryError::Failed`); no match → `NULL`; single match → value (`JsonExtract` serializes JSON incl. JSON `null` → `Some("null")`; `JsonExtractText` unquotes strings, returns SQL `NULL` for JSON `null`, formats numbers/bools/arrays/objects as JSON text); multiple matches → JSON array string.

### Tests

- `fn_misc::tests::math_and_null_helpers` — `abs(i64::MIN)` → `Failed`, `round(3.14159, 2)`, `round(1234.0, -2)`, `sqrt(-1.0).is_nan()`, `ln(0.0) == -inf`, `coalesce`, `nullif`, `greatest`/`least` with nulls and NaNs, `isnan`/`isfinite` returning `NULL` on `NULL`.
- `fn_misc::tests::array_list_concat_and_map_functions` — `eval_list_concat` (`NULL || [1]`, `[1] || [2]`, `NULL || NULL`), `array_has(list, NULL) == NULL` & `array_has_all` with `NULL` elements & empty `sub`, `array_length([]) == 0`, `array_element` positive/`-1`/`0`/out-of-bounds, `array_to_string` (with/without `null_str`, non-`Utf8` elems, tight `Reservation` → `TooLarge` with zero leaked bytes), `array_distinct` with multiple `NULL`s, `map_keys`/`map_values` with null map values and `Dict`/`Rle` encodings.
- `fn_misc::tests::json_arrow_and_jsonpath_extraction` — `->`, `->>` (including JSON `null` → `Some("null")` vs SQL `NULL`, negative array index → `NULL`), `json_extract`, `json_extract_text` on nested objects/arrays, missing keys, invalid JSON (`NULL`), invalid JSONPath (`Failed`), inactive row with invalid JSONPath does not fault.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: implement math, null, array, list concat, map, and JSON scalar functions`

---

## QE2.5 — Filter DSL validator and unified tombstone predicate (`filter.rs`, `tombstone.rs`) *(parallel with QE2.2, QE2.3, QE2.4)*

**Goal:** Implement the Filter DSL AST validator (exact parity with `crates/storage/src/filter/` including error messages, `Position`s, `TRY_CAST` rewrite, `references_environment`, and `score()`/`score_label()`/`has_score()` metadata) and the unified tombstone predicate builder (`covered_predicate`, `filter_uncovered`, GDPR `<{n} redacted>` redaction).

**Edit:**
- `crates/query/src/filter.rs` (new).
- `crates/query/src/tombstone.rs` (new).
- `crates/query/src/lib.rs`: add `pub mod filter;` and `pub mod tombstone;`.

### Contracts

`crates/query/src/filter.rs`:
```rust
use cetra_column::{DataType, Field, Schema};

use crate::InvalidQuery;

/// Maximum distinct score names referenced in one filter (D97).
pub const MAX_SCORE_NAMES: usize = 8;

/// Target view of a Filter DSL expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterView {
    /// `traces` view.
    Traces,
    /// `spans` view.
    Spans,
    /// `sessions` view.
    Sessions,
    /// `scores` view.
    Scores,
}

/// Validated & parsed Filter DSL ready for binding/planning.
#[derive(Debug, Clone)]
pub struct ParsedFilter {
    /// Target view.
    pub view: FilterView,
    /// Validated & rewritten `sqlparser` AST (casts rewritten to `TryCast`, score calls to `__score_{i}_*` / `IS TRUE`).
    pub ast: sqlparser::ast::Expr,
    /// True iff any column or map root resolved to `"environment"`.
    pub references_environment: bool,
    /// Distinct score names in first-appearance order (`<= MAX_SCORE_NAMES`).
    pub scores: Vec<String>,
}

/// Parses and validates a Filter DSL string against `view`'s base `schema`.
/// Exact parity with `crates/storage/src/filter/{mod.rs,validate.rs}` on every error message and `Position`.
pub fn parse_filter_ast(
    view: FilterView,
    schema: &Schema,
    text: &str,
) -> Result<ParsedFilter, InvalidQuery>;

/// Returns `base` extended with the 3 nullable score columns (`__score_{i}_num: F64`,
/// `__score_{i}_str: Utf8`, `__score_{i}_present: Bool`) for each name in `scores`.
pub fn filter_planning_schema(base: &Schema, scores: &[String]) -> Result<Schema, InvalidQuery>;

/// Escapes `\`, `%`, `_` with `\` and wraps `%<escaped>%` (spec 05 § Full-text search).
pub fn like_pattern(query: &str) -> String;
```

`crates/query/src/tombstone.rs`:
```rust
use cetra_column::{Batch, Reservation, Schema};

use crate::{EvalContext, Expr, QueryError};

/// Physical table kind for tombstone matching (mirrors `cetra_storage::catalog::FileTable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneTable {
    /// `spans`: matches `trace_id` and `user_id`, gated by `ingest_time <= cutoff`.
    Spans,
    /// `scores`: matches `trace_id`, `user_id`, and `session_id`, gated by `ingest_time <= cutoff`.
    Scores,
    /// `traces`: matches `trace_id` only (no `ingest_time` gate).
    Traces,
}

/// Minimal view of one pending deletion needed to build the unified tombstone predicate (D-A1-3).
#[derive(Debug, Clone)]
pub struct TombstoneSpec {
    /// Inclusive `ingest_time` cutoff (UTC ns).
    pub cutoff: i64,
    /// Erased 16-byte trace IDs (sorted and deduplicated by `covered_predicate`).
    pub trace_ids: Vec<[u8; 16]>,
    /// Erased user IDs (sorted, deduplicated, and redacted in `Display`/`Debug`).
    pub users: Vec<String>,
    /// Erased session IDs (matched on `Scores` only; sorted, deduplicated, and redacted in `Display`/`Debug`).
    pub sessions: Vec<String>,
}

/// Required columns for evaluating tombstones on `table` (stored schemas).
pub fn key_columns(table: TombstoneTable) -> &'static [&'static str];

/// Builds one null-safe boolean [`Expr`] matching rows covered by any deletion in `deletions`.
///
/// Runs against **stored** table schemas (`trace_id: FixedBytes16`, `user_id: Utf8`,
/// `session_id: Utf8`, `ingest_time: TimestampNs` — preserving `ingest_time`'s `utc` flag from `schema`).
/// Returns `Err(QueryError::Failed(_))` if a required column is missing or has an unexpected `DataType`.
/// Every `IN` list is sorted & deduplicated and guarded by `col IS NOT NULL AND col IN (...)`.
/// `user_id` and `session_id` `InList` nodes carry `redacted: true` so `fmt_plan()` and `Debug`
/// print `"{col} IN (<{n} redacted>)"`.
/// Returns `Ok(None)` when no deletion produces an active term for `table`.
pub fn covered_predicate(
    table: TombstoneTable,
    schema: &Schema,
    deletions: &[TombstoneSpec],
) -> Result<Option<Expr>, QueryError>;

/// Drops rows matched by `predicate` from `batch` (updating `batch`'s `Selection` mask without copying payloads).
///
/// `Reservation` postcondition: rolls back to `before` on `Err`, and on `Ok(out)` reconciles `pool`
/// to `before + out.selection().memory_bytes()` (after dropping temporary `covered` / inverted masks).
pub fn filter_uncovered(
    batch: &Batch,
    predicate: &Expr,
    ctx: &EvalContext,
    pool: Option<&mut Reservation>,
) -> Result<Batch, QueryError>;
```

### Steps

1. Implement `parse_filter_ast` in `crates/query/src/filter.rs` matching `crates/storage/src/filter/validate.rs` line-for-line on allowed AST nodes, operator allow-list (including unary `+`/`-`), 5 cast targets + `TryCast` rewrite, 9 scalar functions, `score`/`score_label`/`has_score` validation/dedup/`MAX_SCORE_NAMES = 8`/AST rewrite (note `has_score` rewrites to `IsTrue(Identifier("__score_{i}_present"))`), `references_environment`, and every exact error message & `Position`.
2. Implement `filter_planning_schema` (returning `Result<Schema, InvalidQuery>`) and `like_pattern`.
3. Implement `crates/query/src/tombstone.rs` matching `crates/storage/src/deletion.rs` (`key_columns`, `covered_predicate` with schema type validation, `sort_unstable` + `dedup`, `IS NOT NULL` guards, `ingest_time <= cutoff` preserving `schema`'s `utc` flag on `Spans`/`Scores` only, `redacted: true` on `user_id`/`session_id`, and `filter_uncovered` intersecting `!covered` into `batch.filter_mask` while respecting `Reservation`).
4. Export `pub mod filter;` and `pub mod tombstone;` in `crates/query/src/lib.rs`.

### Tests

- `filter::tests::valid_and_invalid_filters_match_spec_05` — tests all allowed operators, casts (`CAST`/`::`, disallowed targets, `TRY_CAST` rejected directly), `map['k']` vs non-map vs non-string key, `ARRAY[...]` rejected vs `[...]` allowed, 9 functions, `now()` with args rejected, `score`/`score_label`/`has_score` on `Traces`/`Spans`/`Sessions` vs rejected on `Scores`, 8 allowed vs 9th rejected with position, `references_environment`, empty filter, operator cap 128, byte cap 4096.
- `tombstone::tests::covered_predicate_null_safety_dedup_and_redaction` — tests `Spans`, `Scores`, `Traces` (nullable `trace_id`/`user_id`/`session_id`), duplicate users deduplicated so count is exact, missing/wrong-type column → `Err(QueryError::Failed(_))`, cutoff boundary (`<=` covered, `>` kept), empty terms → `None`, `fmt_plan()` and `Debug` contain `<{n} redacted>` and never erased user/session IDs, `filter_uncovered` preserves non-deleted rows with `Selection`.

### Done when

- `cargo fmt --all && cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: add Filter DSL validator and unified tombstone predicate`

---

## QE2.6 — AST binder, function dispatch wiring, and DataFusion differential parity tests

**Goal:** Wire `fn_string`, `fn_time`, `fn_misc`, `filter`, and `tombstone` into `crates/query/src/{bind.rs,eval.rs,lib.rs}`, and add a comprehensive differential parity test suite in `crates/storage/src/query_diff_tests.rs` comparing `cetra-query` row-for-row and error-for-error against DataFusion.

**Edit:**
- `Cargo.lock` (updated by `cargo metadata --format-version 1 > /dev/null`).
- `crates/query/src/bind.rs` (new).
- `crates/query/src/eval.rs`: wire all remaining arms (and remove every temporary `"not yet wired"` arm).
- `crates/query/src/lib.rs`: unify all `pub mod`s and top-level `pub use`s.
- `crates/storage/Cargo.toml`: add `cetra-query.workspace = true` under `[dev-dependencies]`.
- `crates/storage/src/query_diff_tests.rs` (new, `#[cfg(test)]`).
- `crates/storage/src/lib.rs`: add `#[cfg(test)] mod query_diff_tests;`.

### Contracts

`crates/query/src/bind.rs`:
```rust
use cetra_column::Schema;

use crate::filter::FilterView;
use crate::{Expr, InvalidQuery};

/// Compiled Filter DSL ready for evaluation over batches matching `planning_schema`.
#[derive(Debug, Clone)]
pub struct CompiledFilter {
    /// Target view.
    pub view: FilterView,
    /// Bound boolean/null expression against `planning_schema`.
    pub expr: Expr,
    /// Schema extended with any `__score_{i}_*` columns referenced by `scores`.
    pub planning_schema: Schema,
    /// True iff any column or map root resolved to `"environment"`.
    pub references_environment: bool,
    /// Distinct score names in first-appearance order (`<= MAX_SCORE_NAMES`).
    pub scores: Vec<String>,
}

/// Binds a `sqlparser::ast::Expr` against `schema` into a typed [`Expr`] IR.
pub fn bind_expr(ast: &sqlparser::ast::Expr, schema: &Schema) -> Result<Expr, InvalidQuery>;

/// Parses, validates, and binds a Filter DSL string end-to-end against `view`'s base `schema`.
/// Rejects expressions whose bound `data_type()` is neither `Bool` nor `Null` with exact message
/// `"the filter must be a true/false expression"` at `Some(Position { line: 1, column: 1 })`
/// (matching `crates/storage/src/filter/mod.rs:120-123`).
pub fn compile_filter(
    view: FilterView,
    schema: &Schema,
    text: &str,
) -> Result<CompiledFilter, InvalidQuery>;
```

**Binding, AST forms, aliases, and implicit type coercion rules (`bind.rs`):**
1. **Supported `sqlparser` 0.62 AST forms (all must be handled, not just `Expr::Function`):**
   - `Identifier` / `CompoundIdentifier`: quoted (`ident.quote_style.is_some()`) uses exact case-sensitive match against `schema`; unquoted uses ASCII case-insensitive match (matching `filter/validate.rs:493-499` and DataFusion SQL); error if unknown.
   - `Value`: `Number` (parses as `I64` if integer in range, else `F64` — scientific notation like `1e8` binds as `F64`), `SingleQuotedString`, `Boolean`, `Null`, `HexStringLiteral` (`X'deadbeef'` → `Binary`).
   - `Expr::TypedString(TypedString { data_type, value, .. })` (tuple variant in `sqlparser 0.62`, e.g. `TIMESTAMP '1970-01-01T00:00:00Z'`, `DATE '2026-01-01'`): parses literal via `fn_time::parse_timestamp_ns` / `parse_date_days`.
   - `Interval(Interval { value, leading_field, .. })`: supports both `INTERVAL '1 hour'` and `INTERVAL '1' HOUR` (combines `leading_field` with `value` before calling `fn_time::parse_interval`).
   - `UnaryOp`: `Not`, `Minus` (`Neg`), `Plus` (no-op on numeric types, error on non-numeric).
   - `BinaryOp`: `And`, `Or`, `Eq`, `NotEq`, `Lt`, `LtEq`, `Gt`, `GtEq`, `Plus`, `Minus`, `Multiply`, `Divide`, `Modulo`, `StringConcat` (`||` → `Concat` for `Utf8` or `List`), `AtArrow` (`@>` → `ArrayContains`), `Arrow` (`->` → `JsonGet`), `LongArrow` (`->>` → `JsonGetText`).
   - `IsNull`, `IsNotNull`, `IsDistinctFrom`, `IsNotDistinctFrom`, `IsTrue`, `IsFalse`, `IsNotTrue`, `IsNotFalse`, `IsUnknown` (`IsNull`), `IsNotUnknown` (`IsNotNull`).
   - `Between`, `InList`, `Like`, `ILike` (`escape_char: None` → `Some('\\')`; `ESCAPE ''` → `None`; single-char `ESCAPE '!'` → `Some('!')`; multi-char `ESCAPE` → `InvalidQuery`).
   - `Cast { kind: CastKind::Cast | CastKind::DoubleColon, .. }` → `CastMode::Strict`; `CastKind::TryCast | CastKind::SafeCast` → `CastMode::Try`.
   - `Case`, `Nested`, `Array(Array { elem, .. })`: infers `item_type` from first non-null element (or `DataType::Null` if empty/all-null). When all elements are literals, folds to `Expr::Literal(ScalarValue::List { item_type, values })` (with null elements typed as `ScalarValue::Null(item_type.clone())`, required by `ScalarValue::validate`); when any element is non-literal (e.g. `tags @> [lower('BETA')]`), emits `Expr::MakeArray { elems, item_type }`.
   - `CompoundFieldAccess { root, access_chain }` / `JsonAccess`: on `MapUtf8` with `Utf8` subscript → `Expr::MapIndex`; on `List(_)` with `I64` subscript → `Expr::Call { func: ScalarFn::ArrayElement, .. }`.
   - Special SQL syntax nodes: `Expr::Ceil { expr, field }` → `ScalarFn::Ceil` and `Expr::Floor { expr, field }` → `ScalarFn::Floor` (`sqlparser 0.62` parses `CEIL(x)`/`FLOOR(x)` as dedicated AST variants, not `Expr::Function`!); `Expr::Substring { expr, substring_from, substring_for, .. }` → `ScalarFn::Substr`; `Expr::Trim { expr, trim_where, trim_what, trim_characters }` → `Trim`/`Ltrim`/`Rtrim`; `Expr::Position { expr: sub, r#in: s }` → `ScalarFn::Strpos(s, sub)` (**note swapped argument order!**); `Expr::Extract { field, expr, .. }` → `ScalarFn::Extract(Literal(Utf8(field)), expr)`.
2. **Function allow-list, aliases, and deny-list:**
   - Every function name is lowercased before dispatch.
   - If in `DENIED_FUNCTIONS`, return `InvalidQuery::new(format!("function `{name}` is not available"), pos)`.
   - Aliases mapped to canonical `ScalarFn`: `char_length`/`character_length` → `Length`, `substring` → `Substr`, `position` → `Strpos`, `date_part` → `Extract`, `ceiling` → `Ceil`, `pow` → `Power`, `array_contains` → `ArrayHas`.
   - Unknown function name → `InvalidQuery`.
3. **Implicit type coercion (inserts `Expr::Cast { mode: CastMode::Strict }` where needed):**
   - `Null` coerces to any peer type `T` across binary ops, `Between`, `InList`, `Case`, `Coalesce`, `Greatest`, `Least`, `Nullif`, and `Array` literals (`List(Null)` empty `[]` coerces to `List(T)` on `tags @> []` and `tags || []`).
   - `I64` + `F64` → `F64` (covers `duration_ns BETWEEN 1e8 AND 5e9` in `filter/mod.rs:334`).
   - `Decimal128(p, s)` + `I64` → `Decimal128(p, s)`; `Decimal128` + `F64` → `F64`; `Decimal128(p1, s1)` + `Decimal128(p2, s2)` → `Decimal128(max(p1,p2).min(38), max(s1,s2))`.
   - `TimestampNs { utc: a }` vs `TimestampNs { utc: b }` → `TimestampNs { utc: a || b }`.
   - `TimestampNs` vs `Utf8` literal (`start_time >= '2026-01-01T10:05:00Z'` in `filter/mod.rs:399`): coerces `Utf8` to `TimestampNs` via `Cast` (or parses at bind time). Same for `Date` vs `Utf8` literal.
   - `TimestampNs ± Interval` and `Interval + TimestampNs` → `TimestampNs` (preserving `utc` flag).
   - `Concat` / `ConcatWs`: non-`Utf8` arguments coerce to `Utf8` via `Cast`.
   - `Round(x, dp)` / `Sqrt` / `Ln` / `Log10` / `Power`: integer arguments coerce to `F64` when needed.
   - `Extract(field, source)`: requires `field` to be a string literal and resolves return `DataType` via `fn_time::extract_return_type`.

`crates/query/src/eval.rs` wiring:
- Wire `Expr::Like` → `fn_string::eval_like`.
- Wire `BinaryOp::Concat` → `fn_string::eval_string_concat` (for `Utf8`) and `fn_misc::eval_list_concat` (for `List`).
- Wire `BinaryOp::ArrayContains` → `fn_misc::eval_misc_fn(ScalarFn::ArrayHasAll, ...)`.
- Wire `Cast` for `Utf8 -> TimestampNs`/`Date` and `TimestampNs -> Utf8` → `fn_time`.
- Wire `BinaryOp::Add`/`Sub` for `TimestampNs ± Interval` and `Interval + TimestampNs` → `fn_time::add_interval`.
- Wire `Expr::Call` → `fn_string::eval_string_fn`, `fn_time::eval_time_fn`, `fn_misc::eval_misc_fn`.
- **Zero `"not yet wired"` arms remain anywhere in `crates/query`.**

### Steps

1. Ensure `QE2.2`–`QE2.5` are merged and `crates/query/src/lib.rs` exports all modules and top-level symbols (`bind`, `compile_filter`, `CompiledFilter`, `filter`, `tombstone`, etc.).
2. Implement `crates/query/src/bind.rs` per the contracts above.
3. Wire all remaining arms in `crates/query/src/eval.rs` and confirm zero `"not yet wired"` strings remain.
4. Add `cetra-query.workspace = true` to `crates/storage/Cargo.toml` `[dev-dependencies]` and write `crates/storage/src/query_diff_tests.rs`:
   - **Filter DSL differential suite (40+ expressions):** executes against **view** RecordBatches (`ctx.session().table("spans" | "traces" | "scores")` where `trace_id` is `Utf8` hex and `model`/`duration_ms` exist), converted via `column_bridge::record_batch_to_column`. Covers equality, ranges, scientific notation (`duration_ns BETWEEN 1e8 AND 5e9`), `TimestampNs >= '2026-01-01T10:05:00Z'`, `IN`, `NOT IN` with `NULL`, `BETWEEN`, `IS [NOT] NULL`, `IS [NOT] DISTINCT FROM`, `LIKE`/`ILIKE` (default & `ESCAPE '\\'`; note DataFusion 55.2 rejects custom `ESCAPE '!'`/`ESCAPE ''`, so custom escapes are tested in `cetra-query` unit tests), `map['k']`, `@>` (incl. `tags @> []` and dynamic `tags @> [lower('BETA')]`), `CAST`/`::` valid & invalid `TRY_CAST`, `now() - INTERVAL '3650 days'` (wide window + synced `ctx.now_ns` to avoid clock flake), `lower`, `upper`, `length`, `coalesce`, `array_has`, `array_length`, `date_trunc`, `abs`, compound `AND`/`OR`/`NOT`, plus pre-joined `__score_0_*` batches verifying `score()`/`score_label()`/`has_score()` evaluation. Asserts **identical matching row masks** against DataFusion!
   - **Invalid Filter DSL parity suite (20+ cases):** asserts both engines reject with `Invalid` and matching `Position`.
   - **Tombstone differential suite:** runs against **stored** batches (`spans_schema()`/`traces_schema()`/`scores_schema()` where `trace_id` is `FixedBytes16`) comparing `cetra_query::tombstone::covered_predicate` + `filter_uncovered` against `crates/storage/src/deletion.rs::covered_predicate` + `drop_covered` across `Spans`, `Scores`, `Traces` with nullable columns, duplicate entries, and cutoff boundaries.

### Tests

- `bind::tests::all_50_scalar_functions_aliases_coercions_and_denied_functions` — verifies every `ScalarFn` and alias binds with exact return type, all 20 `DENIED_FUNCTIONS` are rejected with `"function \`{name}\` is not available"`, and `TypedString`/`Substring`/`Trim`/`Position`/`Extract`/`Interval`/`CompoundFieldAccess`/`IsTrue`/`Arrow`/`LongArrow` bind cleanly.
- `query_diff_tests::filter_dsl_matches_datafusion_row_for_row` — 40+ filter expressions on view `spans`, `traces`, `scores` batches (including pre-joined `__score_0_*`).
- `query_diff_tests::invalid_filters_match_datafusion_errors` — 20+ invalid filters matching error & `Position`.
- `query_diff_tests::tombstone_predicate_matches_datafusion_across_all_tables` — stored `spans`, `traces`, `scores`.

### Done when

- Zero `"not yet wired"` occurrences in `crates/query/src/`.
- `cargo test -p cetra-query && cargo test -p cetra-storage --lib query_diff_tests:: && cargo clippy -p cetra-query -p cetra-storage --lib --tests -- -D warnings && cargo doc -p cetra-query --no-deps && cargo fmt --all -- --check` passes.

**Commit:** `query: wire scalar function dispatch, AST binder, and DataFusion differential tests`

---

## QE2.7 — `query_expr` fuzz target, Neon code review, and M2 closeout

**Goal:** Add the `query_expr` fuzz target + seed corpus + stable corpus replay test, run a Neon code review over all M2 code, fix every finding, update `docs/roadmap.md`, and pass full `just check`.

**Edit:**
- `crates/query/src/{lib.rs,fuzz_entry.rs}` (new `#[cfg(any(test, fuzzing))] #[doc(hidden)] pub mod fuzz_entry;`):
  - `pub fn fuzz_query_expr(data: &[u8])`: exercises Filter DSL parsing (`parse_filter_ast` + `compile_filter`), arbitrary SQL expression parsing + binding (`bind_expr`), `eval_expr` / `eval_filter` against a small multi-type `Batch` under a bounded `Reservation` (asserting `pool.size() == before` on `Err`, `== before + out.memory_bytes()` on `Ok`, and never panicking), plus `parse_interval` and `parse_timestamp_ns`.
  - `#[cfg(test)] mod tests`: contains `#[test] fn replay_query_expr_corpus()` (replays every file in `fuzz/corpus/query_expr/` and `fuzz/regressions/query_expr/` on stable under `just test`, per `AGENTS.md`) and `#[test] #[ignore] fn write_seed_corpus()` (matches `just fuzz-seed`'s `--exact fuzz_entry::tests::write_seed_corpus` filter; writes 6+ committed seeds into `fuzz/corpus/query_expr/seed-*`).
- `fuzz/Cargo.toml` (add `cetra-query = { path = "../crates/query" }` and `[[bin]] name = "query_expr"`), `fuzz/fuzz_targets/query_expr.rs`, `fuzz/corpus/query_expr/seed-*`, `justfile` (add `query_expr` to `fuzz_targets`), `.github/workflows/nightly.yml` (add `query_expr` to fuzz matrix).
- `docs/roadmap.md`: update track `QE` status to M1–M2 done.
- Any Neon review fixes.

### Done when

- `just check` passes.
- `just check-workflows` passes.
- `just assert-clean` passes.
- `cargo tree -p cetra-query` confirms zero `tokio`, `arrow`, `parquet`, `datafusion`, `object_store`.

**Commit:** `query: add query_expr fuzz target and close out M2`
