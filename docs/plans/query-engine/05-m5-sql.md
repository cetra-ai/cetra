# Track QE — Milestone 5 Implementation Plan (Shape B SQL Frontend & Exports)

**Status:** Approved (Neon R2: 0 Blockers; R2 M1/M2 & Minors resolved)
**Track / Milestone:** QE — M5 (`docs/plans/query-engine/design.md` § Milestones / M5)
**Depends on:** M1 (`cetra-column`), M2 (`cetra-query` exprs), M3 (including QE3.6 fixes + QE3.7 `TracesSource`/`ScoresSource`). Note on parallelism with M4: QE5.2–QE5.5 are 100% file-disjoint from M4 (`reads_qe/`), with only a mechanical additive merge on `crates/query/src/exec/hash_agg.rs` between QE4.1 and QE5.1 (and `engine/mod.rs` for `parquet_export.rs`). QE5.6 (`engine/sql.rs`) uses a self-contained `SqlEngineContext` over M3 primitives (`spans`, `TracesSource`, `ScoresSource`, `AsyncAdmissionPool`, `WorkerPool`, `SpillManager`, `visible_from`, `window`, `extra_tables`, `suspects`) with a convenience constructor from M4's `EngineContext` if present.
**Leaves out:** storage maintenance migration (M6), switching `cetra-storage::function_catalog()` / `schema.json` / `web/src/lib/sql/types.ts` / deleting DataFusion (M7).

---

## Goal

Build the curated analytical SQL ("Shape B") frontend and direct `cetra-column` JSON / CSV / NDJSON encoders in `crates/query`, build the zero-Arrow streaming `.parquet` export writer (`parquet` low-level `SerializedFileWriter`, D-QE-12) in `crates/storage/src/engine/parquet_export.rs`, build and unit-test `cetra_query::sql::function_catalog()`, verify full differential and sandbox-security parity against DataFusion across all 5 shipped web UI examples, all 3 `eval/stats.rs` queries, MCP queries, `EXPLAIN` GDPR redaction, and exports, and add the new `sql` cargo-fuzz target.

---

## Cross-Cutting Invariants (Apply to Every Phase)

1. **Zero Parquet/Arrow/Tokio in `cetra-query`:** `crates/query` NEVER imports `parquet`, `arrow`, `datafusion`, or `tokio`. All `.parquet` streaming export logic lives in `crates/storage/src/engine/parquet_export.rs` using `parquet::file::writer::SerializedFileWriter<Vec<u8>>` (`default-features = false` + `zstd`, zero `parquet::arrow`).
2. **Existing `crates/storage/src/sql/` and `cetra-storage::function_catalog()` stay untouched** during M5 as the differential oracle and to keep `fixtures/api/static/schema.json` and `just check-docs` green until M7.
3. **Sandbox limits & exact error classification:**
   - `QueryError::InvalidRequest(String)` (capped with `cap_message`, mapped to `RunError::InvalidRequest` / HTTP `400 bad_request`) is used for missing/extra/wrong-type/out-of-range/usage-mismatch `{name:Type}` parameter values and invalid `limit`/`timeout`, distinct from `QueryError::Invalid(InvalidQuery { message, position })` (`400 invalid_filter`).
   - `SQL_LIMITS`: 65,536 bytes max (`"the query is longer than 65536 bytes"`), 512 operator tokens max (`"the query has more than 512 operators"`), `PARSER_RECURSION_LIMIT = 50`, `MESSAGE_CAP = 2048` UTF-8 boundary truncation.
   - Reject pipe syntax (`|>`), multiple statements (`"only one statement is allowed"`), non-SELECT statements (`"only SELECT queries are allowed"`), non-plain `EXPLAIN` (`"only plain EXPLAIN of a query is allowed"`), `WITH RECURSIVE`, `SELECT INTO`, qualified table names (`a.b`), table functions, all 20 `DENIED_FUNCTIONS`, filter-only score functions (`score`, `score_label`, `has_score`), and Shape B out-of-scope constructs (correlated subqueries, `LATERAL`, `ROLLUP`/`CUBE`/`GROUPING SETS`, `GROUPS`/non-row `RANGE` window frames, `INTERSECT ALL`, `EXCEPT ALL`, unknown functions) with `QueryError::Invalid(InvalidQuery)` (`400 invalid_filter`) and position.
   - `MAX_TABLES = 32`: every `TableFactor` except `NestedJoin` counts; each `INTERSECT`/`EXCEPT`/`MINUS` increments table count (`UNION`/`UNION ALL` do not); iterative left-deep traversal (zero recursion risk).
   - Output column names must be unique (`"column name \`{name}\` appears twice; rename one with AS"`).
4. **PostgreSQL SQL ordering & binder ergonomics:**
   - Default SQL `ORDER BY` null ordering: `ASC -> NULLS LAST`, `DESC -> NULLS FIRST` (unless explicit `NULLS FIRST`/`NULLS LAST`).
   - `GROUP BY` and `ORDER BY` MUST resolve:
     - **SELECT output aliases** (`GROUP BY day`, `ORDER BY trace_count DESC` — `web/src/lib/sql/examples.ts`),
     - **1-based ordinals** (`GROUP BY 1`, `ORDER BY 2 DESC`),
     - **Identical repeated expressions** between `SELECT` and `GROUP BY` (`date_trunc('day', created_at)` — `eval/stats.rs:180-183`),
     - **Table-qualified `ORDER BY` after `GROUP BY`** (`GROUP BY t.release, s.name ORDER BY t.release, s.name` — `examples.ts:46-47`), and
     - **`ORDER BY` columns omitted from `SELECT`** (retained through `Sort`, then projected away).
   - Derived tables in `FROM (SELECT ...)` without an explicit alias MUST be accepted (`eval/stats.rs:232-238`).
   - CTEs may shadow base view names (`WITH spans AS (...)`).
   - Drop `sqlparser::Parser` (non-`Send` `Rc` state) before any `.await` boundary in `cetra-storage`.
5. **View isolation (`window` + `visible_from`):**
   - `window` (`col >= from AND col < to`) and `visible_from` (`col >= visible_from`) apply directly at every base storage view scan (`spans.start_time`, `traces.start_time`, `scores.created_at`, `sessions` expanded over windowed+visible `traces`) BEFORE projection/CTEs/joins/subqueries so user SQL cannot bypass them. No default `environment`/experiment exclusions in SQL console (spec 05).
   - Server-owned views (`dataset_items`, `experiment_items`) register cleanly as generic in-memory `BatchSource` tables.
6. **Exact result formatting, caps & D-QE-15:**
   - `columns[].type` uses `cetra_column::DataType::display_name()` (D-QE-15).
   - **Explicit nulls parity (`with_explicit_nulls(true)`):** null columns, null `MapUtf8` values, and `EventItem.attributes = None` / `LinkItem.attributes = None` serialize as explicit JSON `null` (never omitted).
   - **Fixed-9-digit RFC 3339 timestamps** in JSON/CSV/NDJSON: `%Y-%m-%dT%H:%M:%S%.9fZ` (UTC) / `%Y-%m-%dT%H:%M:%S%.9f` (non-UTC) — do NOT use `fn_time::format_timestamp_ns` (which trims trailing zeros).
   - JSON console: `limit in 1..=10_000`, 32 MiB byte cap, pre-row exact size estimation (including `+ QUOTES (2)` for quoted `Decimal128` strings so `estimate >= actual_json_bytes` holds!), `CappedBuffer` + `Reservation::try_grow`, parsed into `Vec<serde_json::Map<String, Value>>` via `serde_json::from_slice` with `2 * encoded.len()` extra (`3x` total) reservation while held; `NaN`/`±inf` -> `null`, `Decimal128` -> exact decimal string (note intentional diff vs Arrow JSON number in `sql_diff_tests.rs`), `Binary`/`FixedBytes8`/`FixedBytes16` -> lowercase hex, `Events`/`Links` -> JSON arrays of objects.
   - Streaming CSV/NDJSON: `EXPORT_MAX_ROWS = 10_000_000`, `EXPORT_PIECE_BYTES = 8 MiB`, `EXPORT_MAX_CHUNK_BYTES = 64 MiB`; CSV uses `\r\n`, RFC 4180 quoting, and `guard_formula` (`=`, `+`, `-`, `@`, `\t`, `\r` -> prepend `'`) on header names and `Utf8` / `TimestampNs` / `Date` / `Interval` string cells (matching `sql/encode.rs:646`, while keeping `I64`, `F64`, `Decimal128` numeric values unguarded, and `Bool`, `Null` -> `""`, and JSON arrays/objects unguarded).
   - `EXPLAIN`: 2 columns `["plan_type", "plan"]` (`Utf8`, `Utf8`), formatting all predicates via `Expr::fmt_plan()` so GDPR erased `user_id`/`session_id` render as `IN (<n redacted>)`.
7. **Safety & privacy (`AGENTS.md`):**
   - `#![forbid(unsafe_code)]`, zero `unwrap()`/`expect()` outside tests, no `#![allow(...)]`, privacy-safe `Debug` (never log parameter values, SQL literals, or row payloads).

---

## Phased Breakdown & Parallelism DAG

```text
QE5.1 Foundation:
      - `crates/query/src/error.rs` (`QueryError::InvalidRequest(String)`)
      - `crates/query/src/bind.rs` (expose internal column/placeholder resolver hook for `sql/binder.rs`
        to reuse all ~1,000 lines of scalar coercion & operator/function binding without duplication)
      - `crates/query/src/exec/hash_agg.rs` (`execute_hash_agg_streams` / `combine_sinks_to_stream`
        for parallel non-`ScanSource` children & streaming finalized groups for 10M-row exports)
      - `crates/query/src/exec/hash_join.rs` (`JoinType::LeftAntiNullAware` for SQL `NOT IN (SELECT ...)` NULL semantics)
      - `crates/query/src/lib.rs` (add `pub mod sql;` ONLY — QE5.4 adds `pub mod encode;`)
      - `crates/query/src/sql/{mod,validate,params,catalog,logical}.rs`
  ├── QE5.2 Binder & Typechecker: `crates/query/src/sql/binder.rs` (+ 1-line `sql/mod.rs`)              ┐
  ├── QE5.3 Optimizer & EXPLAIN: `crates/query/src/sql/{optimize,explain}.rs` (+ 1-line `sql/mod.rs`)   ├── 4 PARALLEL PHASES
  ├── QE5.4 JSON/CSV/NDJSON Encoders: `crates/query/src/encode/{mod,size,json,csv,ndjson}.rs` (+ `lib.rs` `pub mod encode;`) │
  └── QE5.5 Low-level Parquet Export: `crates/storage/src/engine/parquet_export.rs` (+ `engine/mod.rs`) ┘
        └── QE5.6 Pipeline Compiler, Execution Runner, Differential Oracle & `sql` Fuzz Target
                  (`crates/query/src/sql/{compile,run}.rs`, `crates/query/src/fuzz_entry.rs`,
                   `crates/storage/src/engine/{sql,sql_diff_tests}.rs`,
                   `fuzz/fuzz_targets/sql.rs`, `fuzz/Cargo.toml`, `justfile`, `.github/workflows/nightly.yml`)
              └── QE5.7 Neon Code Review, Fixes & M5 Closeout
```

Note on parallelism: QE5.2, QE5.3, QE5.4, and QE5.5 are completely file-disjoint except for the mechanical 1-line `pub mod ...;` additions in `crates/query/src/sql/mod.rs` between QE5.2 and QE5.3 (and `engine/mod.rs` if QE5.5 runs alongside QE3.7).

---

### Phase QE5.1 — Foundation: Engine Hooks, `InvalidRequest`, Validator, Parameters, Catalog & `LogicalPlan` IR

**Files:**
- `crates/query/src/error.rs` (add `QueryError::InvalidRequest(String)` capped with `cap_message`)
- `crates/query/src/bind.rs` (expose `pub(crate)` resolver trait/closure hook for table-qualified column lookup & `$k` placeholder resolution so `sql/binder.rs` reuses all scalar binding & coercion logic cleanly)
- `crates/query/src/exec/hash_agg.rs` (add `execute_hash_agg_streams` / `HashAgg::combine_sinks_to_stream` supporting non-`ScanSource` children and streaming finalized groups into a `Box<dyn PartitionStream>` without materializing 10M rows in RAM)
- `crates/query/src/exec/hash_join.rs` (add `JoinType::LeftAntiNullAware` implementing SQL `NOT IN (SELECT ...)` NULL semantics: if RHS contains any NULL key -> emit 0 rows; if RHS is non-empty -> drop left rows with NULL key; if RHS is empty -> emit all left rows)
- `crates/query/src/lib.rs` (add `pub mod sql;` ONLY — do NOT declare `pub mod encode;` until QE5.4!)
- `crates/query/src/sql/mod.rs`
- `crates/query/src/sql/validate.rs`
- `crates/query/src/sql/params.rs`
- `crates/query/src/sql/catalog.rs`
- `crates/query/src/sql/logical.rs`

**Deliverables:**
1. **`params.rs`:**
   - 5-token `{name:Type}` placeholder scanner & rewriter to `$k` (`String`, `Int`, `Float`, `Bool`, `Timestamp` case-insensitive), rejecting raw `$1`/`?`, unknown types, and conflicting redeclarations with exact token positions (`QueryError::Invalid(InvalidQuery)`).
   - JSON value parser validating missing/extra params, `i64` range, `f64` finite check, `Bool`, strict RFC 3339 (`chrono::DateTime::parse_from_rfc3339` + `timestamp_nanos_opt()`, NOT `fn_time::parse_timestamp_ns`!) or epoch-ms `Timestamp`, and usage-site type coercion (`QueryError::InvalidRequest`).
2. **`validate.rs`:**
   - Single-statement parser (`GenericDialect`, `SQL_LIMITS`, `PARSER_RECURSION_LIMIT = 50`), rejecting `|>`, extra statements, DDL/DML, non-plain `EXPLAIN`, `WITH RECURSIVE`, `SELECT INTO`, >32 tables (counting `INTERSECT`/`EXCEPT`/`MINUS` iteratively), qualified table names, table functions, all 20 `DENIED_FUNCTIONS`, filter-only score functions, `LATERAL`, `ROLLUP`/`CUBE`/`GROUPING SETS`, `GROUPS`/non-row `RANGE` frames, `INTERSECT ALL`, `EXCEPT ALL`.
3. **`catalog.rs`:**
   - `SqlTableDef` & `SqlCatalog` (the 4 base storage views `spans`, `traces`, `scores`, `sessions` + registered in-memory `BatchSource` tables like `dataset_items`/`experiment_items`).
   - `FunctionInfo` & `pub fn function_catalog() -> &'static [FunctionInfo]` covering all Shape B scalars (`~50`), aggregates (10), windows (7), and the 3 filter-only score functions with accurate `in_filter` / `in_sql` flags, verified by unit tests against `ir::ScalarFn` and `DENIED_FUNCTIONS`.
4. **`logical.rs`:**
   - `LogicalPlan`: `Scan { table, projection, filters, time_range, id_lookups, substrings, map_keys, limit }`, `Values { schema, rows }`, `EmptyRow`, `Filter`, `Project`, `Aggregate { group_by, aggs: Vec<exec::hash_agg::AggExpr>, schema }` (reusing `exec::hash_agg::AggExpr` so `sessions` view expands directly with `SumCostDecimal`, `FirstNonNullByOrder`, `TagUnion`!), `Window`, `Sort`, `Limit { skip, fetch }`, `TopK { keys, limit, offset }`, `Distinct`, `Join { kind: Inner|Left|Right|Full|Semi|Anti|AntiNullAware|Cross, left_keys, right_keys, null_equals_null, residual }`, `Union { all }`, `Unnest { columns, preserve_nulls }`, `Explain { inner }`.
   - `SqlColumn { pub name: String, pub data_type: DataType }` with `type_name(&self) -> &'static str` delegating to `DataType::display_name()`.
   - `unique_names` checker.

**Tests & Done Criteria:**
- Unit tests in `cetra-query` for `LeftAntiNullAware` (empty RHS, RHS with NULL, LHS with NULL, normal match/miss, spilled), `combine_sinks_to_stream`, `InvalidRequest`, sandbox rejections, 32-table `INTERSECT`/`EXCEPT` counting, `{name:Type}` parsing & coercion, and `function_catalog` completeness.
- `cargo test -p cetra-query && cargo clippy -p cetra-query --all-targets -- -D warnings`.

---

### Phase QE5.2 — Shape B Binder & Typechecker *(parallel with QE5.3, QE5.4, QE5.5)*

**Files:**
- `crates/query/src/sql/binder.rs`
- `crates/query/src/sql/mod.rs` (add `pub mod binder;`)

**Deliverables:**
- Binds validated `sqlparser::ast::Statement` (`Query` or `Explain`) reusing `bind.rs`'s column/placeholder resolver hook into a typed `LogicalPlan`:
  - `WITH` non-recursive CTEs (shadowing base views), `SELECT` (with/without `FROM`), `SELECT DISTINCT`, `*` and `alias.*`, aliases, `VALUES (...) [AS t(c1, c2)]`.
  - `FROM` joins (`INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`, `ON` equi + residual split with `null_equals_null: false`, `USING`), derived tables (**both aliased AND unaliased `FROM (SELECT ...)`**), `NestedJoin`, `UNNEST(list_expr)`.
  - `WHERE` and `HAVING`, including non-correlated `IN (SELECT ...)` -> `SemiJoin`, `NOT IN (SELECT ...)` -> **`AntiNullAware`** (exact SQL NULL semantics!), and `EXISTS` / `NOT EXISTS` -> `SemiJoin`/`AntiJoin` over `Limit(1)` with constant `Literal(Bool(true))` join key (rejecting correlated outer refs cleanly with `InvalidQuery`, and rejecting `IN`/`EXISTS` subqueries inside `OR` or `SELECT` list cleanly unless constant-folded via `Limit(1)`).
  - `GROUP BY` resolving expressions, **identical repeated `SELECT` expressions** (`date_trunc('day', created_at)` in `eval/stats.rs`), **SELECT output aliases** (`GROUP BY day` in `examples.ts`), and **1-based ordinals**; aggregates: `COUNT(*)`, `COUNT(expr)`, `COUNT(DISTINCT expr)`, `SUM`, `AVG`, `MIN`, `MAX`, `approx_percentile_cont(expr, p)`, `percentile_cont(p) WITHIN GROUP (ORDER BY expr [ASC|DESC])` (mapping `DESC` to `1.0 - p`), `BOOL_OR`, `BOOL_AND`, `ARRAY_AGG`, plus `FILTER (WHERE ...)`.
  - `WINDOW` (`ROW_NUMBER`, `RANK`, `DENSE_RANK`, `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, running `SUM`/`COUNT`/`AVG`/`MIN`/`MAX`) with `OVER ([PARTITION BY ...] [ORDER BY ...] [ROWS BETWEEN ...])`.
  - Set operations (`UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`/`MINUS`): `INTERSECT`/`EXCEPT` lowered to `Distinct` + `SemiJoin`/`AntiJoin` with **`null_equals_null: true`** (D-QE-14); `INTERSECT ALL`/`EXCEPT ALL` rejected cleanly.
  - `ORDER BY` (PostgreSQL default null order, supporting SELECT aliases, 1-based ordinals, **table-qualified `ORDER BY` after `GROUP BY`** like `t.release, s.name`, and `ORDER BY` columns omitted from `SELECT` retained through `Sort` then projected away), `LIMIT` / `OFFSET` (rejecting negative values).
  - View expansion matching `crates/storage/src/views/{spans,traces,scores,sessions}.rs`:
    - Every base storage scan attaches `TimeWindow` (`>= from AND < to`) + `visible_from` (`>= visible_from`) directly on the raw `Scan`.
    - `spans` `Project`: drops `project_id`, hex-encodes `trace_id`/`span_id`/`parent_span_id` (`encode(..., 'hex')`), inserts `duration_ms = CAST(duration_ns AS F64) / 1e6` after `duration_ns`.
    - `traces` `Project`: drops `project_id`/`last_ingest_time`/`partial_count`, hex-encodes `trace_id`/`root_span_id`, inserts `duration_ns = CAST(end_time AS I64) - CAST(start_time AS I64)` and `duration_ms = CAST(duration_ns AS F64) / 1e6` after `end_time`.
    - `scores` `Project`: drops `project_id`, hex-encodes `trace_id`/`span_id`.
    - `sessions`: expands over windowed+visible `traces` -> `Filter(session_id IS NOT NULL)` -> `Aggregate` (`min(start_time)`, `max(end_time)`, `count(1)`, `coalesce(sum(error_count), 0)`, `sum(total_tokens)`, `SumCostDecimal(cost_total)`, `FirstNonNullByOrder(user_id)` by `(start_time ASC, trace_id ASC)`, `TagUnion(tags)`) -> post-agg `Project` casting `SumCostDecimal(cost_total)` **back to `F64`** (`exact_cost_sum`) and projecting derived `duration_ns = greatest(end_time - start_time, 0)` and `duration_ms = CAST(duration_ns AS F64) / 1e6`!
  - `unique_names` check on final output schema and usage-site parameter type coercion (`QueryError::InvalidRequest` on mismatch).

**Tests & Done Criteria:**
- Unit tests binding all 5 `web/src/lib/sql/examples.ts` queries, all 3 `eval/stats.rs` queries (including unaliased nested derived tables + `row_number()`), `VALUES`, `UNNEST`, `UNION`/`INTERSECT`/`EXCEPT`, `IN`/`NOT IN`/`EXISTS`/`NOT EXISTS`, CTE shadowing, `GROUP BY` alias/ordinal/repeated-expr, table-qualified `ORDER BY` after `GROUP BY`, `ORDER BY` non-selected column, `percentile_cont WITHIN GROUP (ORDER BY x DESC)`, and rejecting correlated subqueries / `INTERSECT ALL` / `EXCEPT ALL` / duplicate output names.

---

### Phase QE5.3 — Rule-Based Optimizer & GDPR-Redacted `EXPLAIN` *(parallel with QE5.2, QE5.4, QE5.5)*

**Files:**
- `crates/query/src/sql/optimize.rs`
- `crates/query/src/sql/explain.rs`
- `crates/query/src/sql/mod.rs` (add `pub mod optimize; pub mod explain;`)

**Deliverables:**
1. **`optimize.rs` (deterministic fixed-order pass list — exact order matters!):**
   - Pass 1 — **Constant folding & boolean simplification:** folds literal subexpressions, simplifies `AND true`/`OR false`, detects statically-false filters.
   - Pass 2 — **Filter pushdown through `Project` and safe `Join` sides (MUST run BEFORE Hex-ID rewrite!):** substitutes `Project` output expressions into `Filter` predicates so `WHERE trace_id = '<32-hex>'` becomes `encode(raw.trace_id, 'hex') = '<32-hex>'` on the raw scan! NEVER pushes predicates referencing window output columns below `Window` (e.g. `WHERE rn = 1` in `eval/stats.rs`).
   - Pass 3 — **Hex-literal ID rewrite (D89):** rewrites `encode(id, 'hex') =/<> '<hex>'` and `IN`/`NOT IN` (when ALL items are exact `2 * width` ASCII hex, case-insensitive) into binary `FixedBytes8`/`FixedBytes16` comparisons, and same-width `encode(a, 'hex') = encode(b, 'hex')` to binary equality; invalid-length/mixed lists stay string comparisons.
   - Pass 4 — **Scan pushdown & hint extraction:** pushes storage-safe conjunctive predicates into `Scan.filters` (respecting `TracesSource` merge & `ScoresSource` LWW safety boundaries), AND extracts `ScanSpec.time_range`, `ScanSpec.id_lookups`, and `ScanSpec.substrings` hints while **always retaining the full pushed predicates in `Scan.filters`** (hints are pruning accelerators only).
   - Pass 5 — **Projection & map-key pushdown:** prunes unreferenced columns top-down through all operators and records referenced `map['k']` keys into `Scan.map_keys`.
   - Pass 6 — **Bounded `ORDER BY ... LIMIT k [OFFSET n]` -> `TopK` rewrite:** converts `Limit(skip, Some(fetch)) + Sort` into `TopK { keys, limit: fetch, offset: skip }` (using `TopKSink`'s native `offset`!) **only when `skip.saturating_add(fetch) <= 65_536`**; above that cap, keeps spillable `ExternalSort` + `LimitOp` so large `OFFSET`/`LIMIT` never OOMs unspillable `TopKSink`.
2. **`explain.rs`:**
   - Renders `LogicalPlan` and compiled physical plan into the 2-column `["plan_type", "plan"]` (`Utf8`, `Utf8`) `Batch` (`("logical_plan", ...)`, `("physical_plan", ...)`).
   - Formats ALL expressions strictly through `Expr::fmt_plan()` so redacted `InList` / tombstone predicates render as `user_id IN (<n redacted>)` / `session_id IN (<n redacted>)`.

**Tests & Done Criteria:**
- Unit tests verifying: `Filter` over `Project(encode(trace_id, 'hex'))` pushes down AND rewrites to `FixedBytes16` + populates `Scan.id_lookups` (while retaining `Scan.filters`), `WHERE rn = 1` does NOT push below `Window`, `LIMIT 100000` stays `Sort + Limit` (not `TopK`), `LIMIT 50 OFFSET 10` rewrites to `TopK(limit=50, offset=10)`, projection/map-key pruning, and `EXPLAIN` 2-col schema + GDPR `<n redacted>` redaction.

---

### Phase QE5.4 — Direct `cetra-column` JSON, CSV & NDJSON Encoders *(parallel with QE5.2, QE5.3, QE5.5)*

**Files:**
- `crates/query/src/lib.rs` (add `pub mod encode;`)
- `crates/query/src/encode/mod.rs`
- `crates/query/src/encode/size.rs`
- `crates/query/src/encode/json.rs`
- `crates/query/src/encode/csv.rs`
- `crates/query/src/encode/ndjson.rs`

**Deliverables:**
1. **Shared formatters (`encode/mod.rs`):**
   - Fixed-9-digit RFC 3339 timestamp formatter (`%Y-%m-%dT%H:%M:%S%.9fZ` UTC, `%Y-%m-%dT%H:%M:%S%.9f` non-UTC).
   - Exact formatters for `Date`, `Interval` (canonical string <= 96 bytes), `Decimal128` (`eval::format_decimal`), `Binary`/`FixedBytes8`/`FixedBytes16` (lowercase hex), `F64` (`NaN`/`±inf` -> `null`), `List`, `MapUtf8`, `Events`, `Links`, `Null`, with **explicit nulls** (`with_explicit_nulls(true)` parity) across all 4 `Column` encodings (`Flat`/`Const`/`Dict`/`Rle`) and `Selection` vectors.
2. **Pre-row size estimator & `CappedBuffer` (`encode/size.rs`):**
   - Exact pre-row JSON byte estimator (`JSON_ARRAY_BYTES = 2`, `BRACKETS = 2`, `MEMBER_BYTES = 4`, `null = 4`, scalar = 40, `Interval = 96`, **`Decimal128 = decimal_digits + QUOTES (2)`** so `estimate >= actual_json_bytes` always holds!, `Utf8 = len + 2`, `Binary = len * 2 + 2`, reachable `List`/`MapUtf8`/`Events`/`Links`).
   - `CappedBuffer`: enforces `max_bytes` + `Reservation::try_grow` -> `QueryError::TooLarge`.
   - Piece slicer (`EXPORT_PIECE_BYTES = 8 MiB`, `EXPORT_MAX_CHUNK_BYTES = 64 MiB`, `>= 1` row/piece, single row `> 64 MiB` -> `TooLarge`).
3. **JSON console encoder (`encode/json.rs`):**
   - Encodes up to `limit` (`<= 10_000`) rows within 32 MiB via `CappedBuffer`, sets `truncated = true` if row limit or byte cap trims rows (0 rows fit -> `QueryError::TooLarge`), parses UTF-8 JSON bytes into `Vec<serde_json::Map<String, Value>>` via `serde_json::from_slice` while holding `2 * encoded.len()` extra (`3x` total) reservation, and returns `{ columns: Vec<SqlColumn>, rows: Vec<serde_json::Map<String, Value>>, truncated: bool }`.
4. **CSV encoder (`encode/csv.rs`):**
   - Header (`\r\n`, `guard_formula` on header names) and data chunks (`\r\n`, RFC 4180 quoting, `guard_formula` on `Utf8`/`TimestampNs`/`Date`/`Interval` string cells matching `sql/encode.rs:646`; numeric `I64`/`F64`/`Decimal128`, `Bool`, `Null` -> `""`, and JSON arrays/objects unguarded).
5. **NDJSON encoder (`encode/ndjson.rs`):**
   - One compact JSON object per line + `\n`, explicit nulls, zero formula guarding.

**Tests & Done Criteria:**
- Unit tests across all 15 `DataType`s + all 4 `Column` encodings + `Selection`, asserting `estimate >= actual_json_bytes` (including `Decimal128` quoted strings), fixed-9-digit timestamps (UTC & non-UTC), explicit nulls in `MapUtf8`/`Events`/`Links`, `NaN`/`±inf` -> `null`, JSON 32 MiB truncation vs 0-row `TooLarge`, CSV `guard_formula` parity, and 8 MiB / 64 MiB export chunking.

---

### Phase QE5.5 — Zero-Arrow Low-Level `.parquet` Export Writer *(parallel with QE5.2, QE5.3, QE5.4)*

**Files:**
- `crates/storage/src/engine/parquet_export.rs`
- `crates/storage/src/engine/mod.rs` (add `pub(crate) mod parquet_export;`)

**Deliverables:**
- `ParquetBatchExporter` wrapping `parquet::file::writer::SerializedFileWriter<Vec<u8>>` directly (using `.flush()`, `.inner_mut()`, `.into_inner()` — zero `Mutex` wrapper needed, ZERO `parquet::arrow`, ZERO `arrow` imports):
  - Compression: `Compression::ZSTD(ZstdLevel::default())` (level 1).
  - Rejects 0-column schemas cleanly with `QueryError::Failed`.
  - Exact Parquet schema builder (`design.md:287`):
    - `Bool` -> `BOOLEAN`; `I64` -> `INT64`; `F64` -> `DOUBLE`;
    - `Decimal128(p, s)`: for `s >= 0`, `FIXED_LEN_BYTE_ARRAY(16)` with BOTH `.with_precision(p).with_scale(s)` AND `.with_logical_type(Some(LogicalType::Decimal { scale: s, precision: p }))` (big-endian `i128::to_be_bytes()`); for `s < 0`, rescale to `s = 0` when `p + (-s) <= 38`, else encode as `BYTE_ARRAY (STRING)`;
    - `TimestampNs { utc }` -> `INT64` with `LogicalType::Timestamp { is_adjusted_to_u_t_c: utc, unit: TimeUnit::NANOS }`;
    - `Date` -> `INT32` (`LogicalType::Date`);
    - `Utf8` -> `BYTE_ARRAY` (`LogicalType::String`); `Binary` -> `BYTE_ARRAY`;
    - `FixedBytes8` / `FixedBytes16` -> `FIXED_LEN_BYTE_ARRAY(8 | 16)`;
    - `List(T)` -> standard 3-level Parquet `LIST` (including nested `List(List(T))` with exact def/rep levels);
    - `MapUtf8` -> standard Parquet `MAP<STRING, STRING>`;
    - `Events` -> `LIST` of `GROUP { required INT64 time (TIMESTAMP_NANOS UTC); required BYTE_ARRAY name (STRING); optional BYTE_ARRAY attributes (STRING) }`;
    - `Links` -> `LIST` of `GROUP { required FIXED_LEN_BYTE_ARRAY(16) trace_id; required FIXED_LEN_BYTE_ARRAY(8) span_id; optional BYTE_ARRAY attributes (STRING) }`;
    - `Interval` -> `BYTE_ARRAY` (`LogicalType::String`);
    - `Null` -> optional `BOOLEAN` column of all nulls.
  - Row-group buffering gated by `Reservation` from `ScopedPool`: flush whenever buffered rows >= `1_048_576` OR estimated bytes >= `32 MiB`, drain completed bytes from `writer.inner_mut()`, write footer on `finish()`, and handle 0-row exports cleanly.

**Tests & Done Criteria:**
- Round-trip tests writing EVERY `cetra_column::DataType` (including `List(List(Utf8))`, empty/null lists, empty/null maps, `Events`, `Links`, `Decimal128` positive/negative/negative-scale, `FixedBytes8/16`, `TimestampNs` UTC & non-UTC, `Date`, `Interval`, `Null`, `Dict`/`Rle`/`Const` columns, `Selection`, 0-row export, multi-row-group streaming flush) and reading back via BOTH low-level `parquet` reader AND test-only `parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder`.

---

### Phase QE5.6 — Pipeline Compiler, Execution Runner, Differential Oracle & `sql` Fuzz Target

**Files:**
- `crates/query/src/sql/compile.rs`
- `crates/query/src/sql/run.rs`
- `crates/query/src/sql/mod.rs` (add `pub mod compile; pub mod run;`)
- `crates/query/src/fuzz_entry.rs` (add `#[cfg(any(test, fuzzing))] pub fn fuzz_sql(data: &[u8])` + wire `sql` seed generation into `fuzz_entry::tests::write_seed_corpus` so `just fuzz-seed` `--exact` runs it!)
- `crates/storage/src/engine/sql.rs`
- `crates/storage/src/engine/sql_diff_tests.rs`
- `crates/storage/src/engine/mod.rs` (add `pub(crate) mod sql;` and `#[cfg(test)] mod sql_diff_tests;`)
- `fuzz/fuzz_targets/sql.rs`, `fuzz/Cargo.toml`, `fuzz/corpus/sql/seed-01..10`, `justfile` (incl. adding `sql` to the 300-file tier in `_fuzz-max-files`), `.github/workflows/nightly.yml`

**Deliverables:**
1. **`compile.rs` & `run.rs` (`crates/query`):**
   - Compiles optimized `LogicalPlan` into M3 push pipelines; uses `execute_hash_agg_streams` for streaming `Aggregate`/`Distinct`, coalesces to 1 stream before `LimitOp` whenever `offset > 0` (avoiding per-partition `offset_remaining` bug!), and compiles `Explain` directly via `explain.rs`.
   - `sql_check(catalog, sql, params) -> Result<Vec<SqlColumn>, QueryError>`.
   - `sql_json(catalog, sources, ctx, sql, params, limit) -> Result<SqlJsonResult, QueryError>` (`limit + 1`, drops pipeline before encoding, 32 MiB cap, 3x reservation).
   - `sql_export_batches` (streaming up to `EXPORT_MAX_ROWS = 10_000_000`) + CSV / NDJSON streaming drivers.
2. **`crates/storage/src/engine/sql.rs`:**
   - Completes parse/validate/bind synchronously BEFORE any `.await` (no `sqlparser::Parser` `Rc` across `.await`!).
   - Defines self-contained `SqlEngineContext` over M3 primitives (`spans: Arc<dyn ScanSource>`, `traces: Arc<TracesSource>`, `scores: Arc<ScoresSource>`, `admission: Arc<AsyncAdmissionPool>`, `workers: Arc<WorkerPool>`, `spill: Arc<SpillManager>`, `visible_from: Option<i64>`, `window: Option<TimeWindow>`, `extra_tables: HashMap<String, Arc<BatchSource>>` for `dataset_items`/`experiment_items`, `suspects: Suspects`), wired to `cetra_query::sql::*` and `ParquetBatchExporter`, with `CancelToken` timeout-drain (`cancel.cancel()` + `.await` `JoinHandle`) and exact `QueryError -> RunError` mapping (`InvalidRequest -> InvalidRequest`, `Invalid(e) -> Invalid(...)`, etc.).
3. **`crates/storage/src/engine/sql_diff_tests.rs`:**
   - Full differential suite against `crates/storage/src/sql/` (DataFusion oracle) across:
     - All **5** shipped web UI SQL examples (`web/src/lib/sql/examples.ts`),
     - All **3** `eval/stats.rs` queries,
     - MCP `query_sql` patterns (`CROSS JOIN`, CTEs, `dataset_items`/`experiment_items`, `{name:Type}` params),
     - `UNNEST`, `VALUES`, `UNION [ALL]`, `INTERSECT`, `EXCEPT`, `IN`/`NOT IN` (with NULLs!), `EXISTS`/`NOT EXISTS`, `approx_percentile_cont` (`<= 1%` tolerance), exact `percentile_cont WITHIN GROUP (ORDER BY ... ASC|DESC)`, window functions, `FILTER (WHERE ...)`, hex-ID pruning,
     - JSON / CSV / NDJSON / Parquet export parity (documenting intentional D-QE-15 `columns[].type` vocabulary, `Decimal128` JSON string vs Arrow JSON number, and `EXPLAIN` 2-col schema + GDPR `<n redacted>` redaction),
     - Every sandbox rejection & parameter error (`InvalidRequest` vs `Invalid`).
4. **`sql` fuzz target (`fuzz/fuzz_targets/sql.rs`):**
   - Fuzzes tokenize -> param bind -> parse -> validate -> bind -> optimize -> compile -> execute on small synthetic `BatchSource` tables under a tight 1 MiB `ScopedPool`, asserting zero panics.

---

### Phase QE5.7 — Neon Code Review, Fixes & M5 Closeout

**Deliverables:**
- Full Neon cold code review of `crates/query/src/{sql,encode}/`, `crates/query/src/{bind,error,exec/hash_agg,exec/hash_join}.rs` additions, and `crates/storage/src/engine/{parquet_export,sql,sql_diff_tests}.rs`.
- Fix every Blocker, Major, and Minor.
- Verify `cargo tree -p cetra-query` has zero `arrow`/`parquet`/`datafusion`/`tokio` dependencies.
- Run full `just check-rust` + `just check-workflows` (green!), update `docs/roadmap.md` and `docs/plans/query-engine/design.md` for M5 complete.
