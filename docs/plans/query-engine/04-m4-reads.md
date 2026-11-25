# Track QE — Milestone 4 Implementation Plan (`reads/` Product Reads & Benchmark Verification)

**Status:** Approved (Neon R2: 0 Blockers, 0 Majors)
**Track / Milestone:** QE — M4 (`docs/plans/query-engine/design.md` § Milestones / M4)
**Depends on:** M1 (`cetra-column`), M2 (`cetra-query` exprs + Filter DSL + tombstone), M3 (`cetra-query::exec` + `cetra-storage::engine` including QE3.7 `TracesSource` & `ScoresSource` merged on `qe-query-engine`)
**Leaves out:** SQL console (`POST /sql`, `sql_check`, MCP `query_sql` — M5), storage internals migration (compaction/WAL/memtable/deletion rewrite/media GC — M6), public API cut-over and deleting DataFusion (`crates/storage/src/reads/` stays untouched as the differential oracle until M7).

---

## Goal

Implement every structured read and validation entry point of `cetra-storage::reads` (**39 public functions** total: `mod`: 1, `traces`: 10, `scores`: 4, `sessions`: 3, `users`: 3, `targets`: 5, `deletions`: 4, `prompts`: 1, `experiments`: 3, `facets`: 1, `widgets`: 3, `alerts`: 1) on top of `cetra-query` and `cetra-storage::engine` under `crates/storage/src/reads_qe/`, verify full behavioral parity against the existing DataFusion `crates/storage/src/reads/` oracle across all 39 functions, and prove via a direct `cetra-column` `trace_bench` path (zero Arrow bridge copy in timed paths) that the new engine meets or beats DataFusion on latency and peak RSS.

---

## Cross-Cutting Invariants (Apply to Every Phase)

1. **Prerequisite gate:** M4 starts ONLY after M3 (including QE3.6 fixes + QE3.7 `TracesSource` and `ScoresSource`) is merged and green on `qe-query-engine`.
2. **Zero edits to `crates/storage/src/reads/` and `crates/storage/src/views/`** during M4 — they remain the live DataFusion differential oracle and keep all public `cetra-storage` APIs and `just check` green.
3. **Structured list ordering (`NULLS LAST` in BOTH directions):** every structured list (`trace_list`, `span_list`, `session_list`, `user_list`, `score_list`) uses `SortKey { col, descending, nulls_first: false }` (`crates/query/src/exec/sort_key.rs`) for the primary sort column and non-null tie columns sorted in the **same** direction (`descending`). This differs from SQL's PostgreSQL default (`DESC -> NULLS FIRST`).
4. **Exact keyset cursor semantics (`MAX_CURSOR_BYTES = 4096`):**
   - Lowercase hex of compact JSON `{"l":list,"s":sort,"o":"asc"|"desc","v":null|i64|f64,"t":[tie...]}`.
   - Float sort values (`cost_total`, `error_rate`, `traces_per_session`) MUST parse raw JSON tokens via `str::parse::<f64>()` (never `serde_json` float parsing, which can lose 1 ULP and cause infinite paging loops). Non-finite floats are rejected with `RunError::InvalidRequest`.
   - Timestamp sort values (`start_time`, `end_time`, `first_seen`, `last_seen`, `created_at`) serialize as `i64` UTC nanoseconds.
   - `after` predicate: when `v` is non-null: `(col <op> v) OR (col = v AND tie_after) OR col IS NULL`; when `v` is `Null`: `col IS NULL AND tie_after`. Multi-column lexicographic tie comparison supports `&["span_id", "trace_id"]`.
   - `Cursor` holds `tie: Vec<String>` (which may contain `user_id`/`session_id`) -> MUST implement manual privacy-safe `fmt::Debug` logging only `tie_len`.
5. **Deterministic `Decimal128(38, 18)` cost sums (D-A2-4):** `sessions`, `user_list`, `user_detail`, `widgets` (`cost_input`, `cost_output`, `cost_total`), `alerts` (`Cost`), and `prompt_metrics` sum costs using `AggFunc::SumCostDecimal` (which outputs `Decimal128(38, 18)`) and explicitly cast/convert the result to `F64`. Non-finite or out-of-range cost returns `RunError::Failed` (`400 query_failed`).
6. **`score()` / `score_label()` / `has_score()` filter joins vs `source != 'issue'` (D97, D-IC-18):**
   - Filter score joins (`score()`, `score_label()`, `has_score()`): max 8 distinct score names, unavailable on `FilterView::Scores` and `widgets` `measures[i].where`; left-join latest live score per target key ordered by `(updated_at DESC, score_id DESC)` (distinct from `ScoresSource` per-`score_id` LWW which orders `(updated_at DESC, ingest_time DESC)`). **Does NOT filter `source != 'issue'`.**
   - `latest_scores`, `user_extras.scores`, and `widgets` (`WidgetSource::Scores`) **DO filter `source != 'issue'`** (D-IC-18).
7. **Exclusions, search, `visible_from` & `around`:**
   - `visible_from` applies on every base scan (`time_col >= visible_from`).
   - `around: Option<i64>` comes strictly from the caller's `QueryOptions::around()` and flows into `ScanSpec.time_range.around_ns` — NEVER synthesized from `trace.start_time` inside `trace_detail`.
   - Default exclusions: `environment IS DISTINCT FROM r` for each `r` in `environment::RESERVED` (skipped if `include_eval || references_environment`), AND `metadata['cetra.experiment_id'] IS NULL` (skipped if `include_experiments`).
   - Strict exclusions (D-I-9): always excludes reserved environments + experiments; on `Spans`, anti-joins `TracesSource` on `trace_id` for non-clean traces, accepting `trace_ids: Option<&[[u8; 16]]>` so `span_refresh` and `error_spans` restrict the anti-join scan to only their `<= 500` target traces.
   - Search: requires `TimeWindow` (else `RunError::InvalidRequest("search needs a time range")`); case-insensitive escaped `ILIKE '%escaped%' ESCAPE '\'` across `(input, output, name, system_instructions)` executed via `spec.filters` (note `ScanSpec.substrings` is populated as a forward-compatible hint for future formats; `ParquetSource` currently evaluates `ILIKE` via `spec.filters`). On `Traces`, semi-joins `spans` restricted to `window`.
8. **Persistent QE Runtime on `Storage` & Timeout Drain (`AGENTS.md`):**
   - `Storage` owns a persistent `QeRuntime` (`Arc<AsyncAdmissionPool>`, `Arc<WorkerPool>`, `Arc<MetadataCache>`, `Arc<SpillManager>` wired to `disk.set_engine_spilled` / `disk.engine_spill_cap()`), initialized once in `Storage::open_inner` — NEVER recreated per query (critical for Q4a and warm metadata cache).
   - `run_pipeline` spawns on `tokio::task::spawn_blocking` with a `Drop` guard that fires `cancel.cancel()`. On `tokio::time::timeout` expiry, it **explicitly fires `cancel.cancel()` AND `.await`s the `JoinHandle` to drain `WorkerPool::run_scoped`** before returning `Err(RunError::Timeout)`, preventing any leaked `ScopedPool` reservations.
   - `QueryError` -> `RunError` mapping:
     - `QueryError::TooLarge` -> `RunError::TooLarge`
     - `QueryError::Cancelled` -> `RunError::Timeout`
     - `QueryError::Invalid(InvalidQuery { message, position })` -> `RunError::Invalid(crate::query_error::InvalidQuery { message, position: position.map(|p| crate::query_error::Position { line: p.line, column: p.column }) })`
     - `QueryError::Failed(msg)` -> `RunError::Failed(msg)`
     - `QueryError::Column(e)` -> `RunError::Internal(DataFusionError::External(Box::new(e)))`
     - `QueryError::Internal(msg)` -> `RunError::Internal(DataFusionError::External(msg.into()))`
     - `QueryError::Busy`: handled at admission (`AsyncAdmissionPool::admit` -> `crate::error::QueryError::Busy` / HTTP 503), NEVER mapped to `RunError::TooLarge` (422); if unexpectedly encountered inside execution, maps to `RunError::Internal(...)`.
9. **Safety & privacy (`AGENTS.md`):**
   - `#![forbid(unsafe_code)]`, zero `unwrap()`/`expect()` outside tests, no `#![allow(...)]`.
   - Manual privacy-safe `fmt::Debug` on `Cursor`, `EngineContext`, and any struct holding filter text, search strings, user/session IDs, or batches.
10. **Differential oracle tolerances (`design.md` § 6):**
    - Exact row-for-row, column-for-column, cursor-for-cursor, and error-variant parity on all deterministic reads.
    - `<= 1%` relative error tolerance on sketch-based `DdSketch` percentiles (`p50`/`p75`/`p90`/`p95`/`p99` in `facets`, `widgets`, `alerts`, `prompts`) and resulting `[min, p99)` histogram bucket counts (D-QE-13).

---

## Phased Breakdown & Parallelism DAG

```text
QE4.1 Foundation:
      - Small `cetra-query` additions (`AggFunc::FirstByOrder` NULL-preserving,
        `HashAgg::merge_sinks`, `ProjectOp::with_schema`)
      - Persistent `QeRuntime` on `Storage` (`lib.rs`)
      - `reads_qe/{mod,context,columns,scope,cursor,buckets,scores_join}.rs`
  ├── QE4.2 Traces, Spans & Scores (`reads_qe/{traces,scores}.rs`)                     ┐
  ├── QE4.3 Sessions, Users, Targets, Deletions, Prompts & Experiments                 ├── 3 PARALLEL PHASES
  │         (`reads_qe/{sessions,users,targets,deletions,prompts,experiments}.rs`)     │   (only mechanical
  └── QE4.4 Shared-Scan Facets, Widgets & Alerts (`reads_qe/{facets,widgets,alerts}.rs`)┘    `reads_qe/mod.rs`
        └── QE4.5 Full 39-Function Differential Oracle Suite, Direct `cetra-column`         1-line mod adds)
                  `trace_bench` (`#[doc(hidden)] pub` hook + peak RSS), Neon Review & Closeout
```

Note on parallelism: QE4.2, QE4.3, and QE4.4 are file-disjoint except for their 1-line `pub(crate) mod ...;` additions in `crates/storage/src/reads_qe/mod.rs` (mechanical non-overlapping merge, same pattern as `03-m3-exec-scan.md` § `exec/mod.rs`).

---

### Phase QE4.1 — Foundation: Small `cetra-query` Additions, `QeRuntime` on `Storage`, `EngineContext`, Views, Scope, Columns, Cursor, Buckets & Score Joins

**Files:**
- `crates/query/src/exec/hash_agg.rs` (add NULL-preserving `AggFunc::FirstByOrder { value: Expr, order_by: Vec<SortKey> }` matching `FirstNonNullByOrder`'s shape for `session_extras` + `pub fn merge_sinks(sinks: Vec<HashAgg>, spec: &HashAggSpec, input_schema: &Schema, ctx: &ExecContext) -> Result<Vec<Batch>, QueryError>` so multi-worker operator chains and shared-scan `PushSink` fan-outs combine per-worker `HashAgg` sinks cleanly including empty-partition global aggs)
- `crates/query/src/exec/streaming.rs` (add `ProjectOp::with_schema(...)` so projected output `Schema` nullability matches expected view schemas for `TopKSink::merge` and `BatchSource::new`)
- `crates/storage/src/lib.rs` (add persistent `QeRuntime` field on `Storage` initialized once in `Storage::open_inner` with `AsyncAdmissionPool`, `WorkerPool`, `MetadataCache`, `SpillManager` wired to `disk.set_engine_spilled`/`disk.engine_spill_cap()`; expose `pub(crate) mod reads_qe;` and `#[doc(hidden)] pub mod qe_bench` or `Storage::qe_context` hook in QE4.5)
- `crates/storage/src/reads_qe/mod.rs`
- `crates/storage/src/reads_qe/context.rs`
- `crates/storage/src/reads_qe/columns.rs`
- `crates/storage/src/reads_qe/scope.rs`
- `crates/storage/src/reads_qe/cursor.rs`
- `crates/storage/src/reads_qe/buckets.rs`
- `crates/storage/src/reads_qe/scores_join.rs`

**Deliverables:**
1. **`cetra-query` targeted additions (`hash_agg.rs`, `streaming.rs`):**
   - `AggFunc::FirstByOrder { value: Expr, order_by: Vec<SortKey> }`: tracks minimum/maximum `(order_by)` row and keeps its `value` **even if `value` is `NULL`** (matching `ROW_NUMBER() = 1` semantics in `reads/sessions.rs:200-257`, reusing `FirstNonNullByOrder`'s `Expr` + `SortKey` eval and spill serialization code directly).
   - `HashAgg::merge_sinks(sinks: Vec<HashAgg>, spec: &HashAggSpec, input_schema: &Schema, ctx: &ExecContext) -> Result<Vec<Batch>, QueryError>`: public wrapper exposing multi-worker sink combine (both in-memory and spilled partial-state combine, plus emitting the 1-row empty global-agg result when `sinks.is_empty()` and `spec.group_by.is_empty()`).
   - `ProjectOp::with_schema(exprs, schema)` preserving explicit field nullability.
2. **`QeRuntime` (`lib.rs`) & `EngineContext` (`context.rs`):**
   - `Storage` owns `qe_runtime: Arc<QeRuntime>` (`admission: Arc<AsyncAdmissionPool>`, lazy `OnceLock<Arc<WorkerPool>>` so non-QE `Storage::open` tests don't eagerly spawn OS threads while keeping `WorkerPool` 100% persistent and warm across all QE queries & `Q4a`, `metadata_cache: Arc<MetadataCache>`, `spill: Arc<SpillManager>`).
   - `EngineContext`: wraps `spans: Arc<dyn ScanSource>`, `traces: Arc<TracesSource>`, `scores: Arc<ScoresSource>`, `runtime: Arc<QeRuntime>`, `visible_from: Option<i64>`, `around: Option<i64>` (from `QueryOptions::around()`), `force_range_mode: bool` (default `false`; set `true` by `trace_bench` to verify `differing == 0`), `suspects: Suspects` (note `Suspects` is `Clone`).
   - `run_pipeline` / `collect_batches` / `run_with_sink` with `CancelToken` + `Drop` guard + timeout-drain (`cancel.cancel()` then `.await` `JoinHandle` before returning `RunError::Timeout`), returning `(Vec<Batch>, ExecMetrics)`.
   - Base view schemas & projection builders matching `views/{spans,traces,scores,sessions}.rs` (`spans`: 56 cols; `traces`: 29 cols; `scores`: 23 cols; `sessions`: 11 cols using `SumCostDecimal` cast to `F64`, `FirstNonNullByOrder` for `user_id`, `TagUnion` for `tags`).
   - Binary hex-ID predicate rewrite helper populating `ScanSpec.id_lookups` (`Fixed16`/`Fixed8`) for bloom/sort-key pruning.
3. **Shared column readers (`columns.rs`):**
   - Typed `cetra-column::Batch` cell extractors mirroring `reads/columns.rs` (`i64_at`, `f64_at`, `time_at`, `string_at`, `decimal_to_f64_at`, `require_col`, handling `Flat`/`Const`/`Dict`/`Rle` + `Selection`).
4. **Scope, exclusions, search, `trace_id_semi_join` & sessions (`scope.rs`):**
   - `scoped_traces_spec` / `scoped_spans_spec`: applies `TimeWindow`, `visible_from`, `around`, default exclusions, and `strict_exclusions(trace_ids: Option<&[[u8; 16]]>)` (restricting `Spans` anti-join scan when `trace_ids` is provided).
   - `trace_id_semi_join(ctx, lookback_ns, ends_after_from, scope, dashboard_filter)`: shared parameterized helper used by `widgets` (QE4.4), `alerts` (QE4.4), and `prompts` (QE4.3), mapping `from - lookback` underflow to `RunError::InvalidRequest("from is out of range")`.
   - `search_predicate`: validates `window.is_some()`, builds escaped case-insensitive `ILIKE` over `(input, output, name, system_instructions)` (via `spec.filters`, also recording `ScanSpec.substrings`), and on `Traces` semi-joins windowed `spans`.
   - `sessions_from_traces`: reusable push-pipeline builder producing the 11-column `sessions` stream.
5. **Score joins & filter compilation (`scores_join.rs`):**
   - `validate_filter(view: ViewName, text: &str) -> Result<(), RunError>`: accepts `Traces|Spans|Sessions|Scores` only (same error message as `reads::validate_filter`), blank -> `Ok(())`, calls **`cetra_query::compile_filter`** (full syntax + column + type + boolean-return validation, rejecting non-boolean filters with `"the filter must be a true/false expression"`).
   - `apply_compiled_filter`: up to 8 `Left` hash joins on latest live scores per `(target_key, name)` ordered `(updated_at DESC, score_id DESC)` (NO `source != 'issue'` filter), strips `__score_{i}_*` helper columns after filtering.
6. **Native `cetra-column` keyset cursor (`cursor.rs`) & `buckets.rs`:**
   - `QePage { pub batches: Vec<Batch>, pub next_cursor: Option<String> }`, manual privacy-safe `fmt::Debug` on `Cursor`, raw-token `str::parse::<f64>()` float parsing, `SortKey { col, descending, nulls_first: false }`, `after_expr`, `assemble`.
   - `buckets.rs` wrapping `reads::buckets::{Bucket, BucketWidth, resolve}` + epoch-aligned UTC `date_bin` `Expr`.

**Tests & Done Criteria:**
- Unit tests in `cetra-query` for `AggFunc::FirstByOrder` (NULL winning row returns NULL, spilled combine preserves NULL), `HashAgg::merge_sinks`, and `ProjectOp::with_schema`.
- Unit tests in `cetra-storage` for `validate_filter` (rejecting non-boolean like `"1 + 2"`), `cursor` (1-ULP float round-trip, privacy `Debug`, multi-tie), `scope` (`strict_exclusions` with `trace_ids`, `trace_id_semi_join` underflow), `scores_join`, and timeout cancellation drain (verifying `ScopedPool::reserved() == 0` after `RunError::Timeout`).
- `cargo test -p cetra-query && cargo test -p cetra-storage --lib reads_qe:: && cargo clippy -p cetra-query -p cetra-storage --lib -- -D warnings`.

---

### Phase QE4.2 — Traces, Spans & Scores Reads *(parallel with QE4.3, QE4.4)*

**Files:**
- `crates/storage/src/reads_qe/traces.rs`
- `crates/storage/src/reads_qe/scores.rs`
- `crates/storage/src/reads_qe/mod.rs` (add `pub(crate) mod traces; pub(crate) mod scores;`)

**Deliverables:**
1. **`traces.rs` (all 10 entry points matching `reads/traces.rs`):**
   - `trace_list`:
     - When `TraceSort::StartTime` `Desc` and `!ctx.force_range_mode` (and no conflicting non-pushable predicates): wires `TracesSource` widening-window `mode=list`.
     - For `CostTotal`, `Duration`, `TotalTokens`: **two-phase rank -> fetch** via `TracesSource` (Phase 1 projects rank/filter/cursor columns + `trace_id`, applies any `score()` left-join / `search` semi-join on Phase 1's narrow projection, runs `TopK(limit + 1)`; Phase 2 fetches full trace rows for the `<= limit + 1` winning `trace_id`s with bloom/sort-key pruning and restores exact rank order).
   - `trace_list_plan`: returns `TracesSource::explain_summary(...)` containing `"mode=list"` or `"mode=range"`.
   - `span_list`: projects `SPAN_LIST_COLUMNS`, applies exclusions/filter/search BEFORE dedup, sorts with trailing `ingest_time DESC NULLS LAST`, runs adjacent-copy dedup on `(start_time, span_id, trace_id)` with doubling refetch loop (`fetch = min(2 * (limit + 1), MAX_FETCH)`) and `capped_page` `next_cursor`.
   - `trace_detail`: validates 32-hex, no exclusions, 1 merged trace row + spans (`SPAN_LIST_COLUMNS`, `start_time ASC, span_id ASC`, `MAX_TRACE_SPANS = 20_000`, `truncated` via `cap + 1`). Uses ONLY caller's `ctx.around` (never synthesizes `around` from `trace.start_time`).
   - `span_detail`: validates 32-hex & 16-hex, all 56 `spans` cols, `ORDER BY ingest_time DESC LIMIT 1`.
   - `trace_spans`: `max_spans >= 1 && max_required >= 1`; `required` (`LIMIT max_required + 1`) + `earliest` (capped at `max_spans` with `truncated`).
   - `trace_refresh`: parses filter even on empty `trace_ids`; `<= 500` 32-hex IDs; optional `search: Option<(&str, TimeWindow)>`; projects `TRACE_REFRESH_STRICT_COLUMNS` when `strict_exclusions` else all `traces` cols.
   - `span_refresh`: parses filter even on empty `trace_ids`; `per_trace in 1..=200`; passes `Some(ids)` to `strict_exclusions`; `row_number() OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC) <= per_trace`, ordered `(trace_id, start_time, span_id)` ASC, projects `SPAN_REFRESH_COLUMNS`.
   - `error_spans`: parses filter even on empty `trace_ids`; passes `Some(ids)` to `strict_exclusions` + `status = 'error'`; first 50 per trace, projects `ERROR_SPAN_COLUMNS`.
   - `match_count`: `Traces | Spans` only, `cap in 1..=10_000`, early stop at `Limit(cap + 1)`.
2. **`scores.rs` (all 4 entry points matching `reads/scores.rs`):**
   - `score_list`, `latest_scores` (`source != 'issue'`, `(updated_at DESC, score_id DESC)`), `scores_by_id`, `score_name_counts`.

**Tests & Done Criteria:**
- Unit + differential tests comparing all 14 entry points row-for-row and cursor-for-cursor against `reads::{traces, scores}`, including `span_list` doubling refetch & `MAX_FETCH` cap, two-phase `trace_list` (with and without `score()`/`search`), `trace_detail` on multi-day trace with `around = None`, `trace_list_plan` `"mode=list"`/`"mode=range"`, and `latest_scores` `source != 'issue'`.

---

### Phase QE4.3 — Sessions, Users, Targets, Deletions, Prompts & Experiments *(parallel with QE4.2, QE4.4)*

**Files:**
- `crates/storage/src/reads_qe/sessions.rs`
- `crates/storage/src/reads_qe/users.rs`
- `crates/storage/src/reads_qe/targets.rs`
- `crates/storage/src/reads_qe/deletions.rs`
- `crates/storage/src/reads_qe/prompts.rs`
- `crates/storage/src/reads_qe/experiments.rs`
- `crates/storage/src/reads_qe/mod.rs` (add the 6 `pub(crate) mod ...;` lines)

**Deliverables:**
1. **`sessions.rs` (3 entry points):**
   - `session_list`: `references_environment = false`, `sessions_from_traces`, `FilterView::Sessions` filter (incl. `score()`), cursor `list = "sessions"`, tie `&["session_id"]`.
   - `session_detail`: no window, 1 session row (`None` if empty), up to `MAX_SESSION_TURNS + 1` turns ordered `(start_time ASC, trace_id ASC)`, inner-joins turns with `spans` pushing the `<= 1,000` turn `trace_id`/`span_id` values as `IdLookupHint`s for bloom/sort-key pruning.
   - `session_extras`: `<= MAX_PAGE` session IDs, computed in 1 pass via `HashAgg` using NULL-preserving **`AggFunc::FirstByOrder`** for `first_input_preview` (`(start_time ASC, trace_id ASC)`) and `last_output_preview` (`(start_time DESC, trace_id DESC)`) + `error_trace_count`.
2. **`users.rs` (3 entry points):**
   - `user_list`: filters `traces` BEFORE grouping (honoring `references_environment`), drops `user_id IS NULL`, all 11 cols (`SumCostDecimal` -> `F64`), all 9 `UserSort` variants, cursor `list = "users"`, tie `&["user_id"]`.
   - `user_detail`: `references_environment = false`, 1 user row + whole sessions via semi-join -> `sessions_from_traces`, `LIMIT 100`.
   - `user_extras`: empty `user_ids` returns `Default` **before** parsing `filter`; `activity` uses signed truncating `(start_time - from) / width` (NOT `div_euclid`); `scores` filters `source != 'issue'`; `metadata` latest per `user_id` by `(start_time DESC, trace_id DESC)`.
3. **`targets.rs` (5 entry points):**
   - `trace_targets` (`include_eval = false, include_experiments = true`), `span_targets`, `session_targets`, `filter_span_targets` (second-stage `trace_scan` always forces `include_eval = false, include_experiments = true` even if span filter named `environment`), `filter_session_targets`.
4. **`deletions.rs` (4 entry points):**
   - `filter_trace_ids`, `user_deletion_scope`, `eval_trace_ids`, `session_ids_of_traces`.
5. **`prompts.rs` (1 entry point):**
   - `prompt_metrics`: uses shared `scope::trace_id_semi_join` (1-day lookback, `ends_after_from = true`), span aggs (`SumCostDecimal` defaulting to `0.0`, `DdSketch` `p50`/`p95` rounded to `i64`), deduplicates `(trace_id, span_id, prompt_version)` and `(trace_id, prompt_version)` before joining `scores` (`created_at >= window.from()`, `UNION ALL` across span/trace branches), sorted `version DESC`, scores `name ASC`.
6. **`experiments.rs` (3 entry points):**
   - `experiment_scores` (`seen > MAX_EXPERIMENT_SCORES` checked **before** skipping `trace_id IS NULL`), `experiment_traces`, `experiment_score_means`.

**Tests & Done Criteria:**
- Unit + differential tests comparing all 19 entry points against `reads::{sessions, users, targets, deletions, prompts, experiments}` (including `session_extras` with NULL preview on earliest/latest trace, `user_extras` empty `user_ids` with invalid filter, signed `activity` bucket, `filter_span_targets` with `environment = 'cetra-eval'` filter, and `prompt_metrics` `<= 1%` p50/p95 tolerance).

---

### Phase QE4.4 — Shared-Scan `facets`, Shared-Scan `widgets` & `alerts` *(parallel with QE4.2, QE4.3)*

**Files:**
- `crates/storage/src/reads_qe/facets.rs`
- `crates/storage/src/reads_qe/widgets.rs`
- `crates/storage/src/reads_qe/alerts.rs`
- `crates/storage/src/reads_qe/mod.rs` (add `pub(crate) mod facets; pub(crate) mod widgets; pub(crate) mod alerts;`)

**Deliverables:**
1. **`facets.rs` (`facets` — shared-scan Pass 1 + narrow Pass 2 + per-field isolation):**
   - Exact validation order: `fields.len()` -> `Sessions && search` -> `compile_filter` -> per-field `(limit, q, parse_field)` -> all-invalid early return (BEFORE `search` window check!) -> `search` window check (outer error).
   - `Spans` facets do NOT deduplicate span copies; `total` and `missing` are computed **before** `q` filtering; `List` deduplicates per row; `Keys` counts `missing` on null/empty map BEFORE dropping `cetra.*` keys.
   - **Shared-scan Pass 1** (custom `PushSink` fan-out fed by **1 single scan**, combining per-worker `HashAgg` states via `HashAgg::merge_sinks`):
     - High-cardinality value fields use spillable `HashAgg` with per-field `Reservation`s on the shared `ScopedPool` (with a per-field soft spill/drop cap so one runaway field spills or drops cleanly on `QueryError::TooLarge` without starving sibling unspillable reservations!). On `TooLarge`, drop only that field's state and record `Err(RunError::TooLarge)` in its slot while siblings finish.
     - Simultaneously accumulates histogram `total`, `present`, exact `min`, exact `max`, and `DdSketch` (`p50`, `p95`, `p99`), counting non-finite values in `missing`.
   - **Shared narrow Pass 2** (at most **1** narrow scan over only histogram columns with `present > 0 && p99 > min`): exact 20 equal-width buckets + optional 21st bucket `[p99, max]` iff `max >= p99`.
   - Pass 1 / Pass 2 timeout isolation: Pass 1 timeout marks all unfinished valid fields `Err(Timeout)` (preserving pre-validated `Err(InvalidRequest)`); Pass 2 timeout preserves completed Pass 1 `Values`/`Keys` as `Ok(...)` and marks only unfinished Pass 2 histograms `Err(Timeout)`.
2. **`widgets.rs` (`validate_shape`, `validate_expressions`, `widget_query`):**
   - `validate_shape`: collects all field errors in path order; rejects `'` inside map keys (`!key.contains('\'')`); `CountDistinct` allows `IDENTITY_FIELDS` + non-list breakdown fields.
   - `validate_expressions`: uses `compile_filter` for `dashboard_filter` (`Traces`), `filter` (`source`), `measures[i].where` (`source`, forbidding score functions).
   - `widget_query`:
     - Uses shared `scope::trace_id_semi_join` (1-day lookback, `ends_after_from = true`; overflow -> `InvalidRequest("from is out of range")`, same for `previous_window` underflow).
     - `Scores` filters `target IN ('trace','span') AND source != 'issue'`.
     - List breakdown (`tags`, `models`): evaluates measure expressions & `where` BEFORE unnesting, drops null elements.
     - **NULL breakdown rows dropped BEFORE `Top`/`Rest`** (`group IS NOT NULL`), NEVER classified into `other`!
     - **Zero-row `Shape::Total`**: when `rows == 0`, returns `vec![None; measures.len()]` even for `Count`/`CountDistinct` (whereas `rows > 0` with 0 `where` matches returns `Some(0.0)`).
     - Omit `other` when it has 0 rows or `top_names.is_empty()`.
     - 1-scan non-breakdown; 2-scan breakdown (Pass 1 spillable top-`<= 20` by first measure; Pass 2 **1 unified scan** over non-null groups classifying `group in top_set ? idx : OTHER`); `compare: true` previous-period scan; `SumCostDecimal` -> `F64` for costs; `DdSketch` percentiles.
3. **`alerts.rs` (`alert_value`):**
   - All 7 `AlertMeasure` variants using shared `scope::trace_id_semi_join` (1-day `ends_after_from = true` for `LlmLatencyP95`; 7-day `ends_after_from = false` for `ScoreAvg`/`ScorePassRate`); `Cost` uses `SumCostDecimal` -> `F64` and returns `Some(0.0)` when `count == 0` (other non-`ScoreCount` measures return `None` when `count == 0`); `ScoreCount` forbids filter.

**Tests & Done Criteria:**
- Unit + differential tests comparing `reads_qe::{facets, widgets, alerts}` against `reads::{facets, widgets, alerts}`, plus dedicated tests for:
  - `facets` scan count `<= 2` across 10 mixed fields, per-field `TooLarge` isolation, Pass 1/Pass 2 timeout isolation, all-invalid fields skipping `search` window check,
  - `widgets` NULL breakdown exclusion from `other`, `rows == 0` -> `None` vs `rows > 0 && matched == 0` -> `Some(0.0)`, empty `other` omission, `from` underflow, unified Pass 2 `top + other`, `compare: true`,
  - `alerts` all 7 measures + `count == 0` edge cases.

---

### Phase QE4.5 — Full 39-Function Differential Oracle Suite, Direct `cetra-column` `trace_bench` Verification & Closeout

**Files:**
- `crates/storage/src/reads_qe/diff_tests.rs`
- `crates/storage/src/lib.rs` (expose `#[doc(hidden)] pub mod qe_bench` re-exporting `EngineContext` + `reads_qe` benchmark entry points for the external `tests/trace_bench.rs` integration crate)
- `crates/storage/tests/trace_bench.rs` (add `CETRA_TRACE_BENCH_ENGINE=qe` mode measuring directly on `qe_bench` / `cetra-column::Batch`es with `ExecMetrics` diagnostics on missed targets, peak RSS measurement via `VmHWM`/`ru_maxrss`, and zero Arrow bridge copy in timed paths; default mode remains unchanged until M7)
- `docs/roadmap.md` & `docs/plans/query-engine/design.md` (status update)

**Deliverables:**
1. Comprehensive `reads_qe/diff_tests.rs` verifying **all 39 public entry points** against `reads::*` over a realistic dataset (L0/L1 Parquet + memtable + split traces + duplicate span copies + tombstones + scores + experiments + reserved environments + `visible_from` + `around`).
2. `trace_bench` direct `cetra-column` execution (`CETRA_TRACE_BENCH_ENGINE=qe`) verifying `Q1`, `Q2`, `Q2r-2h`, `Q2r-15h`, `Q3`, `Q6`, `Q4a`, `Q4b`, and `widget_query` (with `default_plan.contains("mode=list")`, `differing == 0` via `force_range_mode: true`, `ExecMetrics` profile output on missed targets, and peak RSS comparison).
3. Full Neon cold code review of all `reads_qe/` files + fixes for every Blocker/Major/Minor.
4. Full `just check-rust` + `just check-workflows` green, and `docs/roadmap.md` updated for M4 complete.
