# Track QE — Milestone 9 Implementation Plan (Query Engine Verification)

**Status:** Approved for execution (cold-reviewed; all blockers, majors, and refinements folded in)  
**Branch:** `qe-verification` (branched from `main`, runs in parallel with `statam`)  
**Scope:** Format-agnostic verification of `cetra-column` and `cetra-query` across three mutually reinforcing suites — **zero Parquet/Arrow dependency** so every suite stays 100% valid before, during, and after a native file format (`statam`) replaces Parquet:

1. **Phase 1 — Curated `sqllogictest` Corpus** (`crates/query/tests/verify/` + `crates/query/tests/slt/01..09.slt`)
2. **Phase 2 — Metamorphic Equivalence & SQLancer TLP Harness** (`crates/query/tests/verify/metamorphic.rs`)
3. **Phase 3 — DuckDB Differential Testing Tool & Bug-Fix Campaign** (`tools/sql-diff/` + `just sql-diff`)

*(On-disk `cetra-storage` metamorphic testing over `StatamSource` is explicitly deferred until `statam` lands in `cetra-storage`, at which point it plugs directly into these same `.slt` and metamorphic query suites.)*

---

## Cross-Cutting Invariants & Repo Conventions

1. **Zero format coupling:** All three suites load tables via `cetra_query::exec::BatchSource` + `SqlCatalog::register_batch_source` + `SqlSources::register_catalog_batches`. Neither `crates/query/tests/` nor `tools/sql-diff/` depends on `parquet`, `arrow`, or `cetra-storage`.
2. **Zero new Rust workspace dependencies:**
   - The `.slt` parser/runner in `crates/query/tests/verify/` is a self-contained Rust module (~280 lines) using existing `sqlparser` + `cetra_query` public APIs (`bind_expr`, `eval_expr`, `parse_and_validate_sql`, `bind_statement`, `optimize`, `compile_plan`, `sql::run::{prepare_sql_plan_with_limit, execute_plan_json, sql_json}`) — no external `sqllogictest` crate.
   - `tools/sql-diff` (`cetra-sql-diff`, MIT, added to root `[workspace] members`) depends ONLY on existing workspace crates (`cetra-column`, `cetra-query`, `serde`, `serde_json`, `chrono`, `hex`, `tempfile`) — zero `Cargo.lock` additions, clean for `check-ee`, `deny-policy`, and `fuzz-lock`.
   - DuckDB runs strictly in Python (`tools/sql-diff/py/`, `uv`, `requires-python = "==3.13.*"`, `[tool.uv] package = false`, `exclude-newer = "7 days"`, exact-pinned `duckdb`, `ruff==0.17.3`, `mypy==2.4.1`, strict mypy) — **never** `duckdb-rs` in Rust.
3. **Single integration test binary in `cetra-query`:** Put `slt` and `metamorphic` as modules of one integration test binary `crates/query/tests/verify/main.rs` (single relink per edit, matching `crates/storage/tests/it/`).
4. **Fast default `just check`, scalable deep sweeps:**
   - Default `just check` runs all 9 curated `.slt` files (with 4-mode metamorphic cross-checks) + `CETRA_METAMORPHIC_CASES` (default `64`, `< 3s` in debug).
   - `just metamorphic [cases="1000"]` runs release-mode deep sweeps (`CETRA_METAMORPHIC_CASES=N`).
   - `just sql-diff [cases="2000"] [seed="0"]` builds `cetra-sql-diff` in release mode and runs the DuckDB differential campaign (not part of `just check`).
   - `just check-sql-diff` builds `cetra-sql-diff`, runs `ruff format --check`, `ruff check`, `mypy` (strict), Python unit tests, and a 50-query DuckDB smoke diff (mirrors `just check-soak-plot`).
5. **Universal post-query invariants (asserted after *every* query, `Ok` or `Err`):**
   - Zero panics; stress queries run on an explicit `2 MiB` stack thread (`std::thread::Builder::new().stack_size(2 * 1024 * 1024)`).
   - `ctx.pool.reserved() == 0` and `ctx.spill.spilled_bytes() == 0` after execution drops.
   - Never returns `QueryError::Internal` (HTTP 500) on valid or invalid SQL input.
6. **Workspace lints (`AGENTS.md`):** `#![forbid(unsafe_code)]`, zero non-test `unwrap()`/`expect()` in library or `tools/sql-diff` binary code, `BufWriter::new(io::stdout().lock())` + `writeln!` in `tools/sql-diff` (clean under `clippy::print_stdout`), privacy-safe logs/errors, no compatibility shims.

---

## Phase 1 — Curated `sqllogictest` Corpus (`crates/query/tests/verify/` + `crates/query/tests/slt/`)

### 1.1 Integration Binary Layout
```
crates/query/tests/
  verify/
    main.rs          # mod common; mod slt; mod metamorphic;
    common.rs        # Batch builders, raw ScalarValue canonicalization, encoding/selection mutators, counting SpillManager
    slt.rs           # lightweight .slt parser & runner + 4-mode metamorphic execution hook
    metamorphic.rs   # seeded Shape B SQL + Filter DSL generator, 5-axis equivalence & SQLancer TLP engine
  slt/
    01_literals_nulls_3vl.slt
    02_numeric_decimal_temporal.slt
    03_strings_regex_json.slt
    04_arrays_maps_unnest.slt
    05_joins_subqueries.slt
    06_aggregates_groupby_having.slt
    07_setops_distinct_order.slt
    08_windows.slt
    09_sandbox_errors.slt
```

### 1.2 Lightweight `.slt` Grammar & Semantics (`verify/slt.rs`)
Standard SQLite/DuckDB `sqllogictest` subset so `.slt` files are readable and directly cross-runnable by `tools/sql-diff` against DuckDB:

- **Comments & directives:**
  - `# ...` comments;
  - `# metamorphic: off` (skip extra metamorphic axes on the immediately following query, e.g., `now()` or intentional `TooLarge`);
  - `# duckdb: skip <reason>` (parsed by `tools/sql-diff` for cetra-only functions/sandbox tests);
  - `# unordered_arrays` (canonicalize `ARRAY_AGG` list element order when comparing across modes/DuckDB).
- **Mutable table state & catalog rebuild:**
  - Because `SqlCatalog::register_batch_source` rejects duplicate names, has no `remove`, and pre-registers `spans`, `traces`, `scores`, `sessions`, the `.slt` runner stores mutable `BTreeMap<String, TableState>` (forbidding those 4 reserved names for `CREATE TABLE`) and rebuilds a fresh `SqlCatalog::new(None, None)` + `SqlSources::new()` after each `CREATE TABLE` / `INSERT INTO` / `DROP TABLE`.
- **DDL & `INSERT` evaluation (`statement ok`):**
  - `CREATE TABLE <name> (<col> <type> [NOT NULL], ...)` parsed directly by `slt.rs` for all `cetra-column` types: `BOOL`/`BOOLEAN`, `BIGINT`/`I64`, `DOUBLE`/`F64`, `DECIMAL(p, s)`, `VARCHAR`/`TEXT`, `BYTEA`/`BINARY`, `FIXEDBYTES8`, `FIXEDBYTES16`, `TIMESTAMP`, `TIMESTAMPTZ`, `DATE`, `INTERVAL`, `T[]` (lists), `MAP(VARCHAR, VARCHAR)`.
  - `INSERT INTO <name> VALUES (...), (...)`:
    - Intercepts `MAP('k1', 'v1', ...)` / `MAP()` and builds `ScalarValue::MapUtf8` directly.
    - Evaluates all other cells via public `cetra_query::bind::bind_expr` + `cetra_query::eval::eval_expr` on an empty 1-row batch (using `CAST('NaN' AS DOUBLE)` / `CAST('Infinity' AS DOUBLE)` for special floats), then **coerces** the resulting `ScalarValue` to the target column `DataType` (`Null` → typed `Null`, `Binary` → `FixedBytes8`/`16`, empty `List(Null)` → `List(T)`, `I64` → `F64`/`Decimal128`).
  - `DROP TABLE <name>` removes the table from `BTreeMap<String, TableState>`.
- **Error assertions (`statement error <expected_substr>`):**
  - Runs `sql_json` and asserts `Err(e)` matching `<expected_substr>`, `!matches!(e, QueryError::Internal(_))`, `pool.reserved() == 0`, and `spill.spilled_bytes() == 0`.
- **Query assertions (`query <type-sig> [nosort|rowsort]`):**
  - `<type-sig>`: one char per column (`I`, `R`, `T`, `B`, `?`).
  - Tab-separated rows (`NULL` for null, `(empty)` for `""`, finite floats compared within `1e-9` tolerance on `R` columns; `NaN`/`+Inf`/`-Inf` distinguished explicitly when comparing raw `ScalarValue`s across metamorphic modes).
  - **4-Mode Metamorphic Cross-Check (on every `query` unless `# metamorphic: off`):**
    1. **Optimized + `FlatAll` + 1 partition (`32 MiB`)** (checked against `.slt` `----` expected rows)
    2. **Unoptimized** (`bind_statement` → `compile_plan` directly, bypassing `optimize`)
    3. **Encoded + Fragmented** (`Dict`/`Rle`/`Const` + 1–3 row chunks + `Selection::Mask`/`Indices` with poison masked-out decoy rows)
    4. **Parallel + Tight-Pool** (`target_partitions = 4`, `64 KiB` pool, `batch_size = 4`, splitting every table with ≥ 4 rows into ≥ 4 batches so `BatchSource::open_partitions` genuinely spawns 4 parallel partitions). Allows clean `QueryError::TooLarge`, otherwise asserts exact `ScalarValue` match with Mode 1.

### 1.3 Curated `.slt` Corpus Coverage (`01`–`09`, ~350–450 queries)
All queries assign unique top-level output aliases (`unique_names` requirement), tie-break `ORDER BY` when paired with `LIMIT` or row-order windows, and use explicit `NULLS FIRST`/`NULLS LAST` and `ESCAPE '\'` where cross-checked against DuckDB:

1. `01_literals_nulls_3vl.slt`: 3VL truth tables, `IS [NOT] TRUE/FALSE/UNKNOWN/NULL`, `IS [NOT] DISTINCT FROM`, `CASE` (simple/searched/short-circuit), `COALESCE` short-circuit, `NULLIF` output typing, `GREATEST`/`LEAST`, `CAST`/`TRY_CAST`.
2. `02_numeric_decimal_temporal.slt`: `I64` overflow & div-by-zero, `F64` `-0.0`/`NaN`/`±Inf` & float div-by-zero (`query_failed`), `Decimal128` widening/negative scales/precision-38/scientific casts, `Date`/`Timestamp`/`Interval` arithmetic & canonical equality (`1 day = 24 hours`), `date_trunc` (incl. `NULL` unit), `date_bin`, `extract`, `to_timestamp`.
3. `03_strings_regex_json.slt`: `LIKE`/`ILIKE`/`ESCAPE` (incl. empty & trailing escape, Unicode), string scalar functions, `regexp_like`/`regexp_extract`, `encode` (`hex`/`base64`, incl. uppercase-hex pushdown regression), `json_get`/`json_get_text`/`json_extract`/`json_extract_text` + `->`/`->>`.
4. `04_arrays_maps_unnest.slt`: `ARRAY[...]`, 1-based subscripts, `array_has`/`array_has_all`/`array_length` (1D & 2D)/`array_element`/`array_to_string`/`array_distinct`, `map['k']`/`map_keys`/`map_values`, `UNNEST` in `FROM` and `SELECT`.
5. `05_joins_subqueries.slt`: `INNER`/`LEFT`/`RIGHT`/`FULL`/`CROSS`, `USING` (incl. `FULL JOIN ... USING` coalescing), NULL join keys, skewed duplicates, outer-join residuals, `IN`/`NOT IN` (with NULLs on left & right), `EXISTS`/`NOT EXISTS`, empty tables, CTEs, derived tables.
6. `06_aggregates_groupby_having.slt`: `COUNT(*)`/`COUNT(col)`/`COUNT(DISTINCT)`, `SUM`/`AVG`/`MIN`/`MAX`/`BOOL_OR`/`BOOL_AND`/`ARRAY_AGG([DISTINCT])`/`percentile_cont`/`approx_percentile_cont`, `FILTER (WHERE ...)`, 0-row global vs grouped agg, `GROUP BY` aliases/ordinals/expressions/qualified subscripts, `HAVING` with subqueries.
7. `07_setops_distinct_order.slt`: `UNION`/`UNION ALL`/`INTERSECT`/`EXCEPT`, `SELECT DISTINCT`, `ORDER BY ... NULLS FIRST/LAST`, `SELECT DISTINCT ... ORDER BY` quoted case-sensitive aliases, `LIMIT`/`OFFSET` + TopK.
8. `08_windows.slt`: `ROW_NUMBER`/`RANK`/`DENSE_RANK`/`LAG`/`LEAD`/`FIRST_VALUE`/`LAST_VALUE`, running aggregates, `PARTITION BY` NULLs, `RANGE` peers vs `ROWS`, `ROWS BETWEEN` bounded/unbounded frames, multi-group windows, named `WINDOW` inheritance.
9. `09_sandbox_errors.slt`: every Shape B refusal on `sqlparser 0.62` (correlated subqueries, `WITH RECURSIVE`, `LATERAL`, `ROLLUP`/`CUBE`/`GROUPING SETS`, `GROUP BY ALL`, `ORDER BY ALL`, `ORDER BY ... WITH FILL`, `ORDER BY ... INTERPOLATE`, `BY NAME`, `INTERSECT ALL`/`EXCEPT ALL`, `GROUPS`/offset `RANGE`, denied functions, score functions in SQL, empty/NUL identifiers, `SELECT INTO`, `TABLESAMPLE`, pipe `|>`, `NATURAL JOIN`, non-equi `OUTER JOIN`, raw `$1`, invalid `ESCAPE`).

---

## Phase 2 — Metamorphic Equivalence & SQLancer TLP Harness (`crates/query/tests/verify/metamorphic.rs`)

### 2.1 Adversarial Multi-Table Dataset (`verify/common.rs`)
Fixed deterministic multi-table dataset covering every `cetra-column` `DataType` and edge value:
- `t_main` (64 rows): `id: I64 NOT NULL` (unique `0..64`), `grp: Utf8`, `k_small: I64` (bounded `-1000..=1000` for overflow-free `SUM` & Aggregate TLP), `k_edge: I64` (`i64::MIN`/`MAX`/`0`/`±1`), `x: F64` (`-0.0`/`0.0`/`±1.5`/`NaN`/`±Inf`), `dec: Decimal128(10, 2)`, `flag: Bool`, `ts: TimestampNs(UTC)`, `d: Date`, `iv: Interval` (`1 day` and `24 hours`), `tags: List(Utf8)`, `meta: MapUtf8`, `tid: FixedBytes16`.
- `t_right` (32 rows): overlapping/non-overlapping `k_small` & `grp` keys, NULL join keys, duplicates.
- `t_empty` (0 rows, same schema as `t_main`) and `t_one` (1 row, all-NULL & non-NULL variants).

### 2.2 Encoding & Layout Mutator (`verify/common.rs`)
1. **`Variant::FlatAll`:** 1 single `Flat` batch with `Selection::All`.
2. **`Variant::MultiChunkFlat`:** Split into `>= 8` small `Flat` batches (ensures Axis C spawns `target_partitions = 4` real parallel workers).
3. **`Variant::FragmentedMasked`:** Split into tiny variable-size chunks (1, 2, 3, 5, 7 rows + empty 0-row batches), alternating `Selection::Mask` and `Selection::Indices` with poison/decoy masked-out rows constructed directly via `Batch::try_new(schema, cols, raw_len, sel)`.
4. **`Variant::EncodedMixed`:** Re-encode columns per chunk (`>= 8` chunks) using `Column::Dict` (with decoy dictionary entries), `Column::Rle` (empty chunks with `values.len() == 0, run_ends = []`), and `Column::Const`, combined with `Selection::Mask`/`Indices`.

### 2.3 Raw `ScalarValue` Comparison & Five Metamorphic Axes
Axes A–D compare materialized `Batch` → `ScalarValue` rows directly (strictly distinguishing `Null`, `NaN`, `+Inf`, `-Inf`), plus run `execute_plan_json` on baseline to verify JSON encoding & reservation cleanup. Finite `F64` cells use `1e-9` rel / `1e-12` abs tolerance; `ARRAY_AGG` lists are sorted before comparing.

1. **Axis A — Optimizer Equivalence:** `bind_statement` → `compile_plan` vs `optimize(bound)?` → `compile_plan`, plus `prepare_sql_plan_with_limit(..., Some(limit + 1))` vs unoptimized `Limit(bound)` for TopK.
2. **Axis B — Encoding & Selection Equivalence:** `FlatAll` vs `FragmentedMasked` vs `EncodedMixed`.
3. **Axis C — Parallelism & Forced-Spill Equivalence:** `FlatAll` (`target_partitions = 1, cap = 32 MiB`) vs `MultiChunkFlat` and `EncodedMixed` (`target_partitions = 4, cap = 64 KiB, batch_size = 8`), counting actual spill writes via `Arc<AtomicU64>` in `SpillCapFn` and asserting `spill_events > 0`.
4. **Axis D — SQLancer TLP (Ternary Logic Partitioning):** Row TLP, Aggregate TLP (`COUNT`/`SUM(k_small)`/`MIN`/`MAX`), and Grouped `HAVING` TLP over generated infallible predicates `P`.
5. **Axis E — Filter DSL Metamorphic & TLP:** `compile_filter` + `eval_filter` across `FlatAll`/`FragmentedMasked`/`EncodedMixed`, applying `batch.filter_mask(&mask)` to compare active row IDs + verifying TLP partition completeness.

### 2.4 Seeded Shape B SQL Generator
Deterministic `SplitMix64` PRNG (`CETRA_METAMORPHIC_CASES`, default `64`, optional `CETRA_METAMORPHIC_SEED`, prints seed + exact SQL on failure). Always assigns unique aliases (`AS c0, ...`), tie-breaks `ORDER BY ..., id ASC` for `LIMIT` and row-order windows, excludes `approx_percentile_cont` from bit-exact 1-vs-4-partition equality, and runs deep-expression stress on a `2 MiB` stack thread.

---

## Phase 3 — DuckDB Differential Testing Tool (`tools/sql-diff/`) & Bug-Fix Campaign

### 3.1 Architecture (`tools/sql-diff/`)
```
tools/sql-diff/
  Cargo.toml         # workspace member `cetra-sql-diff` (MIT, [lints] workspace = true)
  src/
    main.rs          # CLI binary reading JSON dataset+queries bundle, executing via BatchSource + sql_json, writing JSONL via BufWriter(stdout.lock())
  py/
    pyproject.toml   # uv package (requires-python = "==3.13.*", package = false, exclude-newer = "7 days", exact duckdb/ruff/mypy pins, strict mypy)
    uv.lock          # committed lockfile
    diff_runner.py   # loads curated .slt files + generates seeded Shape B SQL & datasets, runs DuckDB (UTC) + cetra-sql-diff, diffs & minimizes
    dialect.py       # explicit documented normalization & DuckDB↔cetra dialect rules
    test_dialect.py  # unit tests for normalization, DDL mapping & equivalence comparator
  README.md          # usage & intentional-difference reference
```

### 3.2 Format-Agnostic JSON Wire Protocol (`cetra-sql-diff`)
- Input JSON bundle: `{tables: [{name, columns: [{name, type, nullable}], rows}], queries: [{id, sql, limit, memory_cap_bytes, target_partitions}]}`.
- `cetra-sql-diff` builds `BatchSource` tables (splitting into multiple batches when `target_partitions > 1`), runs every query through `sql_json`, asserts `pool.reserved() == 0 && spill.spilled_bytes() == 0`, and writes JSONL via `BufWriter::new(io::stdout().lock())`.

### 3.3 Explicit DuckDB vs `cetra-query` Dialect Rules (`tools/sql-diff/py/dialect.py`)
1. **Timezone & timestamps:** `SET TimeZone = 'UTC'` on every DuckDB connection; `TIMESTAMP` → `TIMESTAMP_NS`; microsecond granularity on `TIMESTAMPTZ` inputs.
2. **Explicit `NULLS FIRST` / `NULLS LAST`** on every `ORDER BY` term (cetra uses Postgres `DESC → NULLS FIRST`; DuckDB defaults to `NULLS LAST` for both).
3. **Explicit `ESCAPE '\'`** on `LIKE`/`ILIKE` patterns containing `\` (cetra defaults to `\`; DuckDB has no default escape).
4. **Division & modulo:** `F64` division for `/` (or `//` translation on DuckDB for `I64 / I64`); guard zero denominators (`NULLIF(denom, 0)`) since cetra returns `Err(query_failed)` for both `I64` and `F64` div/mod by zero.
5. **`CAST(F64 AS BIGINT)`:** use `CAST(trunc(x) AS BIGINT)` in cross-engine queries (cetra truncates; DuckDB rounds).
6. **`SUM(BIGINT)`:** use bounded `k_small` (cetra uses checked `I64`; DuckDB widens to `HUGEINT`).
7. **Unordered queries, `ARRAY_AGG`, & aliases:** multiset comparison without top-level `ORDER BY`, `, id` tie-breaking with `ORDER BY`, sorted `ARRAY_AGG` lists, unique `AS c0, ...` aliases.
8. **Floats & Decimals:** normalize DuckDB `NaN`/`±Inf` → `None` (matches cetra JSON `D98`), finite `F64` `rel_tol=1e-9`/`abs_tol=1e-12`, exact `decimal.Decimal` (`min(cetra_scale, duckdb_scale)` digits for `Decimal / Decimal`).
9. **Cetra-only / divergent functions & DDL translation:** map `I64`→`BIGINT`, `F64`→`DOUBLE`, `FIXEDBYTES8`/`16`→`BLOB` when loading `.slt` into DuckDB; tag cetra-only functions (`encode`, `regexp_extract` flags, `substr` neg start, `array_has`/`array_has_all`/`array_element`, `json_get`/`json_extract_text`, sorted `map_keys`/`map_values`, `approx_percentile_cont`, sandbox refusals) with `# duckdb: skip <reason>`.

### 3.4 `justfile` & Docs Wiring
- `just metamorphic cases="1000"`
- `just sql-diff cases="2000" seed="0"`
- `just check-sql-diff`
- Update `AGENTS.md` and `tools/README.md`.

### 3.5 Campaign & Bug-Fix Gate
1. Run `just metamorphic 1000` (release mode) and `just sql-diff 5000` (eligible `.slt` + 5,000 generated Shape B queries) on `macbook-m4-vm`.
2. Fix every genuine `cetra-column` / `cetra-query` bug uncovered (including verifying `optimize.rs:2109` uppercase-hex `pass3_hex_id_rewrite` vs lowercase `encode(tid, 'hex')`) and promote each into a permanent `.slt` or unit regression test.
3. Cold-review all diffs with Gemini 4 Neon, run full `just check-rust` + `just check-sql-diff` on `macbook-m4-vm`, push `qe-verification`, and verify PR CI green.

---

## Commit Plan
1. **Commit 1:** `query: add curated sqllogictest runner and 9-file Shape B corpus` (Phase 1)
2. **Commit 2:** `query: add metamorphic equivalence and SQLancer TLP harness` (Phase 2)
3. **Commit 3:** `tools: add format-agnostic DuckDB differential SQL harness (tools/sql-diff)` (Phase 3)
4. **Commit 4:** `query,column: fix bugs uncovered by metamorphic and DuckDB differential verification` (+ promoted `.slt` tests and docs sync)
