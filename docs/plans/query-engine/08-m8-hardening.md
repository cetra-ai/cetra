# Track QE — Milestone 8 Implementation Plan (Post-Review Hardening & Verification Gates)

**Status:** Done (2026-11-26) — All waves H1–H4b implemented, cold-reviewed by Neon High, and verified across H5–H6.
**Track / Milestone:** QE — M8 (Hardening follow-up to M1–M7 on branch `qe-query-engine`)
**Source:** Clean-room blind design review (`qe_plan_blind_review.md`) + read-only code triage (`qe_code_triage.md`) against PR #267 (`e63bd7c`)
**Scope:** Close **all 9 confirmed Blockers**, **Seam #3**, **all 26 confirmed Majors**, **12 remaining Minors** (`QE-m13` slim `arrow` umbrella swap explicitly deferred until Statam replaces Parquet), **1 newly discovered `ChunkFlusher` reservation leak** in `reads/facets.rs`, and **CodeQL alerts `#152` & `#153`** across `H1`–`H4b` + full verification (`H5`) + Neon High cold code review (`H6`).

---

## Cross-Cutting Invariants

1. **Workspace & ownership:** All edits happen exclusively inside `/home/sebastien/travaux/llm-observer/.claude/worktrees/qe-query-engine` (`qe-query-engine`). Never edit `/home/sebastien/travaux/cetra` (`main`).
2. **Code rules (`AGENTS.md`):** `#![forbid(unsafe_code)]`, zero `unwrap()`/`expect()` outside tests (unless an adjacent comment proves the invariant), zero compatibility shims/wrappers (update all callers in the same commit), privacy-safe `Debug`/`Display`/errors (zero row payloads or PII), `thiserror` per crate.
3. **Crate dependency graph preserved:**
   - `cetra-column` depends on no workspace crate and no `tokio`/`arrow`/`parquet`.
   - `cetra-query` depends ONLY on `cetra-column` (no `cetra-core`, `tokio`, `arrow`, `parquet`).
   - `cetra-storage` isolates all `arrow`/`parquet` usage behind `file_format/` + `engine/{parquet_source,parquet_export}.rs`.
4. **Every commit leaves the entire workspace compiling cleanly** (`cargo test` on affected crates + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --all -- --check`).
5. **Sequential commits, zero build races:** `H1` (done @ `d904084`) → `H2` → `H3a` → `H3b` → `H4a` → `H4b` → `H5` → `H6`.

---

## Phase H1 — Foundation (`cetra-column`, `cetra-core`, `README.md`) *(DONE @ `d904084`)*

Closed: `QE-B1` (column side), `QE-B2`, `QE-M3`, `QE-M4` (types side), `QE-M35`, `QE-m2` (column/frame side), **Seam #3**, CodeQL `#152` & `#153`.

---

## Phase H2 — `cetra-query` Expressions, Error Typing & `ScanSpec` Contracts

**Depends on:** H1 (`d904084`)
**Findings closed:** `QE-B1` (eval/tombstone side), `QE-B13` (decimal coercion), `QE-B15`, `QE-B19` (contract), `QE-M1`, `QE-M2`, `QE-M4` (expr side), `QE-M5`, `QE-M7`, `QE-M8`, `QE-M14` (error variants), `QE-M17` (unified `f64_to_decimal`), `QE-M27` (`ir.rs` iterative walkers).

**Files:**
- `crates/query/src/{error.rs, ir.rs, bind.rs, eval.rs, fn_misc.rs, fn_string.rs, fn_time.rs, tombstone.rs, fuzz_entry.rs, exec/source.rs}`
- Call-site updates in `crates/query/` & `crates/storage/` for `EvalContext` (including all 6 hot per-batch sites per R1-M1), `eval_expr` `Arc<Column>` return type, `QueryError` match arms, and `ScanSpec`.

**Exact Contracts:**
1. **`QueryError` (`crates/query/src/error.rs` — `QE-M8`, `QE-M14`):**
   - Add typed variants:
     - `#[error("corrupt data: {0}")] CorruptData(String)`
     - `#[error("storage read error: {0}")] StorageIo(String)`
   - In `From<ColumnError> for QueryError`: map `ColumnError::Overflow { reason } => Self::Failed(reason.to_owned())` (400 `query_failed` instead of 500 `QueryError::Column`).
2. **Zero-copy `eval_expr` & `filter_uncovered` (`QE-B1`, R1-B8):**
   - Change `eval_expr` / `eval_expr_masked` to return `Result<Arc<Column>, QueryError>`:
     - `Expr::Column { index, .. }` returns `Ok(Arc::clone(batch.column_arc(*index)?))` with **0 newly allocated bytes**.
     - **Explicit reservation postcondition (R1-B8):** if the returned `Arc<Column>` is `Arc::ptr_eq` to a column in `batch`, `pool.size()` is restored to `before` (0 new bytes charged); otherwise `pool.size() == before + col.memory_bytes()`. Skip `reconcile_output` (shrink scratch back to `before`) in `eval.rs:36, 297` when `batch.columns().iter().any(|c| Arc::ptr_eq(c, &col))`.
     - Update `crates/query/src/fuzz_entry.rs:165` to check `expected = if batch.columns().iter().any(|c| Arc::ptr_eq(c, &col)) { 0 } else { col.memory_bytes() }`, and update `eval.rs:5625-5654` (`col_clone_oom` / `retag_oom`) to assert zero-copy / test real allocating expressions cleanly.
     - Update `ProjectOp` (`exec/streaming.rs:241-254`) to build `Batch::try_new_shared(out_schema, out_cols, batch.raw_len(), batch.selection().clone())`.
   - In `tombstone::filter_uncovered`: pre-reserve `bitmap_mem_bytes(batch.raw_len())` for the combined output selection mask before `batch.filter_mask(&keep)` (rolling back on error), then reconcile to `out.selection().memory_bytes()` after dropping `keep`.
3. **Per-query thread-safe `EvalCache` (`QE-M2`, R1-M1, R1-m3):**
   - Add `pub struct EvalCache` (`Default`, opaque redacted `Debug` printing counts/bytes only) held as `cache: Arc<EvalCache>` on `EvalContext`, with `EvalContext::new(now_ns, cancel)` and `EvalContext::with_cancel(&self, cancel: CancelToken) -> Self` (sharing `Arc<EvalCache>`!).
   - **Store `eval: ctx.eval.clone()` (not bare `now_ns`) in ALL 7 hot scan/exec paths (R1-M1):** `BatchPartitionStream` (`exec/source.rs:218`), `ParquetPartitionStream` (`parquet_source.rs:4635`), `MemPartitionStream` (`mem_source.rs:184`), `ScoresDedupStream` (`scores_source.rs:380`), `MemPartialsStream` & `NativeTraceMergeStream` (`traces_source.rs:1152,1338`), `HashJoin` (`hash_join.rs:1365`), and `ExternalSort`/`WindowStream` (`sort_window.rs:662,725,874,1260,1773,1810,1887`).
   - Inside `EvalCache`, enforce BOTH `MAX_CACHE_ENTRIES = 256` per sub-cache AND a global `MAX_CACHE_BYTES = 4 * 1024 * 1024` (`AtomicUsize`) cap across all 4 sub-caches (`like`, `regex`, `json_path`, content-verified ABA-free `in_list` for `list.len() >= 4`) so dynamic patterns can never grow unboundedly outside `ScopedPool` (R1-m3). On `Column::Const`, look up once outside the row loop; on dynamic columns, check a local `last_dyn: Option<(&str, Arc<T>)>` before hitting `ctx.cache`.
4. **Short-circuit `COALESCE` (`QE-M1`):**
   - Intercept `ScalarFn::Coalesce` at the top of `eval_call` BEFORE evaluating `args`: maintain `remaining: Bitmap`, evaluate each `arg` under `Some(&remaining)`, fill non-null active winners, clear those bits in `remaining`, break early if `remaining.all_zeros()`, and build the flat output column.
5. **Exact `f64_to_decimal` & Decimal coercion (`QE-B13`, `QE-M7`, `QE-M17`, R1-M2):**
   - Implement ONE `pub(crate) fn f64_to_decimal(v: f64, precision: u8, scale: i8) -> Result<i128, QueryError>` in `eval.rs` (delete duplicate in `exec/hash_agg.rs:951` and call `crate::eval::f64_to_decimal`):
     - Reject `!v.is_finite()` up front; fast-path `v == 0.0 => Ok(0)`.
     - Fast-path magnitude guard: if `v.abs() >= 10f64.powi(i32::from(precision) - i32::from(scale) + 1)` return `Err(decimal overflow)`; if `v.abs() < 10f64.powi(-i32::from(scale) - 2)` return `Ok(0)`.
     - Use Rust's shortest round-trip `Display` (`format!("{v}")`, **NEVER** `format!("{v:.*}"` — avoids banker's rounding, negative-scale panic, and IEEE-754 binary ULP noise!), then parse via `parse_decimal_str(&s, precision, scale)` (which uses half-away-from-zero rounding, handles negative scales, and validates `check_decimal_bounds`)!
   - Fix `coerce_common_types` (`bind.rs:2255-2274`) & `coerce_numeric_pair` (`bind.rs:2321-2340`):
     - `(Decimal128(p1, s1), Decimal128(p2, s2))`: `int_digits = (p1 - s1).max(p2 - s2)`, `scale = s1.max(s2)`, `precision = (int_digits + scale).clamp(1, 38) as u8` (clamping `scale <= precision as i8`).
     - `(Decimal128(p, s), I64)` (and symmetric): treat `I64` as `Decimal128(19, 0)` using the exact formula above.
     - For comparisons (`=`, `!=`, `<`, `<=`, `>`, `>=`): when `int_digits + scale > 38` (precision would clamp), leave the operands un-narrowed AND add `(Decimal128 { scale, .. }, I64)` / `(I64, Decimal128 { scale, .. })` arms to `eval.rs:1613` `compare_row` calling `compare_decimals(a, s, i128::from(b), 0)` directly (R1-M2)!
6. **JSON DOM reserve & `query(root).all()` bound (`QE-B15`):**
   - Increase DOM pre-reservation in `fn_misc.rs` (`eval_json_get`, `eval_json_extract`) from `1×` to `const JSON_DOM_MULTIPLIER: usize = 6;` (`s.len().saturating_mul(6)`), released right after `root` drops.
   - In `eval_json_extract`: reserve `nodes.len().saturating_mul(std::mem::size_of::<&JsonValue>())` before `JsonBudgetWriter` serialization and shrink immediately after.
7. **Canonical `Interval` & temporal arithmetic (`QE-M4`, `QE-M5`, R1-m4):**
   - Use `Interval::canonical_key()` in `eval.rs` comparisons & `LiteralSet`.
   - Wire in `bind_arith_binary` (`bind.rs:781`) & `eval_binary` (`eval.rs:1356`) — including `(Date, Null)`, `(Null, Date)`, `(Interval, Null)`, `(Null, Interval)` NULL-coercion arms (R1-m4):
     1. `TimestampNs - TimestampNs -> Interval` (`Interval { months: 0, days: 0, nanos: a.checked_sub(b)? }`).
     2. `Date ± Interval -> TimestampNs { utc: false }` (and `Interval + Date`, rejecting `Interval - Date`), using `i64::from(days).checked_mul(NS_PER_DAY)` -> `QueryError::Failed` (400) for out-of-range dates + `fn_time::add_interval`.
     3. `Date - Date -> I64` (`i64::from(a) - i64::from(b)` days).
     4. `Interval ± Interval -> Interval` (component-wise checked) + `UnaryOp::Neg` on `Interval` (checked).
8. **`ScanSpec` Strata contracts (`QE-B19`, `crates/query/src/exec/source.rs`):**
   - Define `pub enum MapProjection { KeysOnly, Keys(BTreeSet<String>), Full }` with `pub fn merge(&mut self, other: MapProjection)` (`Full` absorbs all; `KeysOnly + Keys(_)` → `Full`; `Keys + Keys` unions).
   - Replace `ScanSpec.map_keys: Vec<(String, String)>` with `pub map_projections: BTreeMap<String, MapProjection>`.
   - Replace `ScanSpec.substrings: Vec<SubstringHint>` with CNF `pub substrings: Vec<Vec<SubstringHint>>` (outer AND, inner OR).
   - Add a shared `pub fn collect_expr_map_projections(expr: &Expr, schema: &Schema, out: &mut BTreeMap<String, MapProjection>)` helper (reused by both SQL Pass 5 and `reads/facets.rs` per R1-B6!).
   - Update all `ScanSpec` construction sites cleanly so the workspace compiles.
9. **Iterative `Expr` walkers in `crates/query/src/ir.rs` (`QE-M27`):**
   - Make `Expr::fmt_plan`, `Expr::referenced_columns`, `Expr::remap_columns`, and `impl fmt::Debug for Expr` iterative over `Expr::Binary` spines.

**Commit:** `query: zero-copy eval_expr, EvalCache, COALESCE short-circuit, decimal/temporal fixes, typed errors, ScanSpec CNF/MapProjection`

---

## Phase H3a — Execution Engine Hardening (`crates/query/src/exec/*`)

**Depends on:** H2
**Findings closed:** `QE-B4`, `QE-B6`, `QE-B7` (pipeline), `QE-B8`, `QE-B9`, `QE-M4` (exec), `QE-M11`, `QE-m3`, `QE-m5`.

**Files:**
- `crates/query/src/exec/{pool,pipeline,spill,sort_key,sort_window,hash_agg,hash_join,streaming}.rs`
- Mechanical `PushSink::consume` signature updates in `crates/storage/src/{media.rs, engine/mod.rs, reads/{deletions,facets,prompts,targets,traces,users}.rs}`.

**Exact Contracts:**
1. **Reentrant `WorkerPool::run_scoped` & race-free `CancelToken` wakeup (`QE-B4`, R1-B4):**
   - Track `thread_local! { static IN_WORKER_POOL: Cell<bool> = const { Cell::new(false) }; }` via RAII guard in `worker_loop`. When `true`, execute `tasks` sequentially inline on the 8 MiB worker thread (`catch_unwind`, checking `cancel.is_cancelled()`, calling `cancel.cancel()` on error/panic).
   - **Race-free condvar wakeup (R1-B4):** Register `cancel.register_waker({ let s = Arc::clone(&scope); move || { let _g = s.lock(); s.condvar.notify_all(); } })` — acquiring `s.lock()` BEFORE `notify_all()` eliminates lost-wakeup deadlocks!
2. **Zero-double-count `run_pipeline` & `PushOperator` reservation threading (`QE-B7`, R1-B8):**
   - Add `pub(crate) fn transfer_or_grow(src: &mut Reservation, dst: &mut Reservation, bytes: usize) -> Result<(), ColumnError>`: when `src.size() >= bytes`, calls `src.transfer_to(dst, bytes)`; otherwise transfers `t = src.size()` into `dst` and calls `dst.try_grow(bytes - t)` (rolling back `dst.transfer_to(src, t)` on error!).
   - Update BOTH `PushSink::consume(&mut self, batch: Batch, batch_res: &mut Reservation) -> Result<ControlFlow, QueryError>` AND `PushOperator::push(&mut self, batch: Batch, batch_res: &mut Reservation, emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>)` + `PushOperator::finish(&mut self, emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>)`:
     - `FilterOp` / `LimitOp`: pass `batch_res` through zero-copy.
     - `ProjectOp`: transfers `batch_res.size().min(out_bytes.saturating_sub(self.scratch.size()))` from `batch_res` to `self.scratch`, frees `batch_res`, reconciles `self.scratch` to `out_bytes`, and emits `(out, &mut self.scratch)`.
     - `UnnestOp`: keeps `batch_res` alive while iterating `batch` and emits `(out_chunk, &mut self.scratch)`.
     - Buffered sinks (`CollectSink`, `ExternalSort`, `TopKSink`, `HashAgg`) use `transfer_or_grow(batch_res, &mut self.res, bytes)` (spilling and retrying on `SpillShare` for spillable sinks).
   - Update all 9 storage `PushSink` impls cleanly (`_batch_res: &mut Reservation`).
3. **`Arc<SpillFileInner>` (`AtomicU64` `byte_len`) & zero-idle-FD `SpillAppender` (`QE-B8`, `QE-B9`, R1-M5):**
   - `SpillFileInner` holds `path: PathBuf`, `byte_len: AtomicU64`, `spilled_bytes: Arc<AtomicU64>` (with `Drop` unlinking `path` and subtracting `byte_len.load(Relaxed)` from `spilled_bytes`).
   - Both `SpillFile` and `SpillReader` hold `Arc<SpillFileInner>`, with `SpillReader`'s `reader: BufReader<File>` declared **before** `_inner: Arc<SpillFileInner>` (FD closes before unlink & byte decrement).
   - Add `pub struct SpillAppender` (tracking `created: bool`, `poisoned: bool`, `inner: Option<Arc<SpillFileInner>>`, `cap: SpillCapFn`, `scratch_buf: Vec<u8>`):
     - Opens with `create_new(true)` (looping on `AlreadyExists` via `SpillManager::next_id`) on FIRST `append_batch` (zero empty files created for unused partitions, collision-safe on reused PIDs!), `OpenOptions::new().append(true)` on subsequent writes, updates `inner.byte_len` atomically after CAS-reserving against `cap`, and marks `poisoned = true` on partial write error (returning `Err` on subsequent calls). Closes `File` before returning → **0 idle FDs**!
   - Switch `HashAgg` and `HashJoin` Grace spill partition writers to `SpillAppender` → peak open FDs across `P` workers + nested joins stays `<= MAX_MERGE_WAY + 1 = 33`.
4. **Spillable global & skewed grouped `COUNT(DISTINCT)` (`QE-B9`, R1-B5):**
   - Set `can_spill = is_grouped || has_count_distinct` in `HashAgg`.
   - When memory pressure hits on a `CountDistinct` slot `d`, spill `(group_cols..., distinct_val)` (for non-null, `FILTER`-passing values) into `distinct_spills[d]` via `SpillAppender` and clear ONLY that slot's in-memory `HashSet<KeyAtom>` (keeping all groups and other accumulators intact so NULL-only/filtered-out groups are NEVER dropped!). Collect all workers' `distinct_spills[d]` in `combine_sinks_into`.
   - **Exact O(1)-RAM merge protocol (R1-B5):**
     - **Global mode:** For each spilled slot `d`, sort `(distinct_val ASC)` via `ExternalSort`, stream counting unique transitions in O(1) RAM, store into `AccState::CountDistinctFinal(count)`.
     - **Grouped mode:** If any `CountDistinct` slot spilled, finalize the non-distinct group table/partitions stream (left outer side — contains EVERY group including NULL-only/filtered-out groups) into an `ExternalSort` on `(group_cols ASC)`, wrap each spilled slot `d`'s `ExternalSort` on `(group_cols ASC, distinct_val ASC)` in an O(1)-RAM `DistinctCountPerGroupStream`, and left-outer merge-join them on `(group_cols ASC)` (missing key => `0`)!
5. **Zero-lookahead streaming `WindowStream` (`QE-B6`, R1-M6):**
   - Stream directly off `ExternalSort` in O(1)/bounded memory whenever EVERY spec in `self.specs` is zero-lookahead streamable:
     - `RowNumber`, `Rank`, `DenseRank` (O(1) state; handle `order_by.is_empty()` emitting `1` for every row!)
     - `Lag { offset, .. }` with `0 <= offset <= 4096` (ring buffer of the single evaluated `expr` column, tracked in `self.res` — zero full-row lookahead!)
     - `FirstValue`, `LastValue`, `Sum`, `Count`, `Avg`, `Min`, `Max` with explicit `ROWS` frame whose `end_bound <= CurrentRow` (`UNBOUNDED PRECEDING AND CURRENT ROW`, `m PRECEDING AND CURRENT ROW`, or `m PRECEDING AND n PRECEDING` with `m <= 4096`, buffering only evaluated arg values tracked in `self.res`).
   - Non-streamable specs (positive `Lead`, negative `Lag`, `FOLLOWING`, `UNBOUNDED FOLLOWING`, `RANGE` peer-group lookahead, or whole-partition `order_by.is_empty()` aggregates) use the existing partition-buffered path.
   - Guarantees `SPILL_SQL` (`row_number() OVER (ORDER BY input, span_id)`) runs in O(1) RAM after `ExternalSort` under a 32 MiB cap!
6. **Two-token `LimitOp` cancellation (`QE-m5`, R1-B3), `Interval`, `effective_merge_way`, `TopKSink` (`QE-M4`, `QE-M11`, `QE-m3`):**
   - **Two-token `LimitOp` early-stop (R1-B3):** `run_pipeline` creates a pipeline-local `stop_cancel = CancelToken::new()` linked to `ctx.eval.cancel` via `_g = ctx.eval.cancel.register_waker({ let sc = stop_cancel.clone(); move || sc.cancel() })` (or accepts an optional shared `stop_cancel` passed to `LimitOp`). `run_pipeline` passes `&ctx.eval.cancel` to `WorkerPool::run_scoped` and `&stop_cancel` to `stream.next_batch`. `LimitOp` calls `stop_cancel.cancel()` **only after** `emit(out, ...)?` succeeds when `shared_remaining` hits `0`. In `run_pipeline`, if `stream.next_batch` returns `Err(QueryError::Cancelled)` while `!ctx.eval.cancel.is_cancelled() && stop_cancel.is_cancelled()`, break normally (`Ok(sink)` — never `Err(Cancelled)`!).
   - Use `Interval::canonical_key()` in `sort_key.rs`, `hash_agg.rs`, `hash_join.rs`.
   - Dynamic `effective_merge_way = (pool.spill_share() / 4096).saturating_sub(2).clamp(2, self.max_merge_way)` in `ExternalSort`.
   - In `TopKSink`: cap initial `Vec::with_capacity(keep.min(ctx.batch_size))` and track heap/winner index bytes in `self.res`.

**Commit:** `query/exec: reentrant WorkerPool, streaming WindowOp, spillable COUNT(DISTINCT) + SpillAppender FD cap, transfer_to, SpillReader lifetime`

---

## Phase H3b — SQL Frontend & Optimizer Hardening (`crates/query/src/sql/*`)

**Depends on:** H3a
**Findings closed:** `QE-B16`, `QE-B17`, `QE-B19` (optimizer), `QE-M18`, `QE-M27`, `QE-m7`, `QE-m11`.

**Files:** `crates/query/src/sql/{validate,binder,optimize,logical,compile,explain,run}.rs`.

**Exact Contracts:**
1. **Pre-pool DoS guards & O(N) `ConstFolder` (`QE-B16`):**
   - `ConstFolder` passes a standalone `const FOLD_MEM_CAP: usize = 64 * 1024;` `ScopedPool` + `Reservation` to `eval_expr` (leaving expr unfolded on `Err(_)` or `ScalarValue::heap_bytes() > FOLD_MEM_CAP`), and returns bottom-up `(Expr, FoldFlags)` in **O(N)**.
   - Pass-2 `Project` substitution enforces `MAX_SUBSTITUTED_EXPR_NODES = 2_048` and refuses to push a conjunct through `Project` if any referenced non-trivial projection expr is referenced `> 1` time.
2. **Sound `is_infallible` (preserving base-view pushdown!) & Pass-2 pushdown (`QE-B17`, R1-B1):**
   - In `is_infallible` (`optimize.rs:557-663`):
     - Return `false` for `BinaryOp::Concat`, `Expr::Like`, `ScalarFn::Concat | ConcatWs | ArrayDistinct | MapKeys | MapValues`, and `Floor | Ceil` on `Decimal128`.
     - **Explicitly return `true` (preserving base-view bloom/range pushdown per R1-B1!)** for:
       1. `ScalarFn::Encode` when `args[1]` is literal `"hex"` (case-insensitive), `args[0].data_type()` is `FixedBytes8 | FixedBytes16 | Null`, and `is_infallible(&args[0])`;
       2. Lossless/total `Expr::Cast` regardless of `CastMode`: `(I64 -> F64)`, `(TimestampNs { .. } -> I64)`, `(I64 -> TimestampNs { .. })`, `(TimestampNs { .. } -> TimestampNs { .. })`, `(FixedBytes8 | FixedBytes16 -> Binary)`, `(Decimal128 { .. } -> F64)` when `is_infallible(inner)`;
       3. `BinaryOp::Div` on `DataType::F64` when `right` is a finite non-zero `ScalarValue::F64` literal (e.g. `1_000_000.0`) and `is_infallible(left)`.
   - Gate `Project` filter pushdown on every referenced `project_exprs[idx]` being `is_infallible`, and gate `Join` filter pushdown (including `cols.is_empty()` constants) on `is_infallible(&conj)`.
   - Add unit tests proving `WHERE trace_id = '...'` on `spans`, `traces`, and `scores` STILL pushes `IdLookupHint` into `Scan` while `SELECT 100 / a.x AS r FROM a INNER JOIN b ON a.id = b.id AND a.x != 0` with `WHERE r > 10` does NOT push `100 / a.x > 10` below `Join`!
3. **Pass 4 CNF `substrings` & Pass 5 `MapProjection` (`QE-B19`, `QE-m7`, R1-B6):**
   - Pass 4: emit CNF `Vec<Vec<SubstringHint>>` (pure-substring `OR` → single clause; `AND` concatenates clauses) and fix `longest_like_literal` trailing lone escape char (`else { cur.push(ch); }`).
   - Pass 5: use `collect_expr_map_projections` across BOTH projected expressions AND `Scan.filters` / `Filter.predicate` (R1-B6) so any whole-map use or `KeysOnly + filter_key` merges cleanly to `MapProjection::Full` (never dropping filter keys!).
4. **Unclamped SQL `sessions.duration_ns` (`QE-M18`):**
   - Remove `ScalarFn::Greatest(diff_ns, 0)` in `binder.rs:2565-2582` (`CAST(end_time AS I64) - CAST(start_time AS I64)`, matching `reads/scope.rs`, `traces`, `main`, `spec 05`).
5. **Exhaustive iterative 512-deep AST & IR walkers (`QE-M27`, R1-M7):**
   - In `sql/validate.rs`: replace recursive `sqlparser::ast::Visit` with an explicit worklist-stack walker using **exhaustive destructuring** (no `_ => {}` wildcards on `Query`, `SetExpr`, `TableFactor`, `JoinConstraint`) so no sandbox check (`DENIED_FUNCTIONS`, `MAX_TABLES`, `score*`, `RANGE`/`GROUPS`, unknown tables) can ever be bypassed inside CTEs, subqueries, `CASE`, `FILTER (WHERE ...)`, or `OVER (...)`!
   - In `sql/optimize.rs`: make all binary-spine walkers iterative over `Expr::Binary`.
6. **`TableFactor::Table` modifiers & qualified functions (`QE-m11`):**
   - Exhaustively destructure `TableFactor::Table` in `validate.rs:314-322` rejecting non-empty/`Some`/`true` `args`, `with_hints`, `version`, `with_ordinality`, `partitions`, `json_path`, `sample`, `index_hints`, and reject multi-part qualified function names early.

**Commit:** `query/sql: bound ConstFolder & Pass-2 substitution, infallible pushdown, iterative walkers, unclamped sessions duration, CNF/MapProjection`

---

## Phase H4a — Storage Engine, Parquet, WAL, Exports & Encapsulation (`crates/storage/`)

**Depends on:** H3b
**Findings closed:** `QE-B3`, `QE-B5`, `QE-B7` (Parquet), `QE-B11`, `QE-B14` (part 3), `QE-M9`, `QE-M10`, `QE-M12`, `QE-M14`, `QE-M15`, `QE-M16`, `QE-M26`, `QE-M31`, `QE-M33` (storage), `QE-m6`.

**Files:** `crates/storage/src/{file_format/{mod,arrow_write}.rs, engine/{mod,parquet_source,parquet_export,traces_source,scores_source,mem_source}.rs, wal/record.rs, error.rs, verify.rs, lib.rs, sql/mod.rs, compaction/{merge,rewrite}.rs, media.rs, deletion.rs, deletion_tests.rs, fuzz_entry.rs}`.

**Exact Contracts:**
1. **Parquet continuation pages (`QE-B11`, R1-B2):**
   - `validate_page_locations(locs, num_rows, file_size, is_repeated)`: allow `loc.first_row_index >= prev_row` when `is_repeated == true` (`max_rep_level > 0`), keeping strict `>` for scalar columns.
   - `page_row_ranges(locs, num_rows)`: extend continuation pages sharing `first_row_index == r` to `r..next_distinct_first_row_gt`.
   - **Full-chunk safety on duplicate `first_row_index` (R1-B2):** In `page_ranges_for_leaf`, whenever `locs` contains ANY duplicate `first_row_index`, return `(vec![chunk_start..chunk_end], None)` — fetching the full column chunk so `SerializedPageReader` with `page_locations: None` has every byte from row 0 and `ColumnReaderImpl` skips/decodes multi-page records natively without ever failing on an unfetched leading page! (Leaves without duplicate `first_row_index` keep full page-level pruning.)
   - In `DictUtf8LeafDecoder` (`ListUtf8Dict`), fall back to `DictUtf8Mode::Flat(ColumnReaderImpl<ByteArrayType>)` whenever `max_rep_level > 0` encounters a continuation/V1 split page.
2. **`ParquetSource` stats, wakeup, coalescing, reservations, safe async prefetch, `KeysOnly`, & phase-2 narrowing (`QE-B7`, `QE-M9`, `QE-M12`, `QE-m6`, R1-M4, R1-M8, R1-m2):**
   - In `stats_bounds`: gate `ByteArray` / `FixedLenByteArray` `min` on `stats.min_is_exact()` and `max` on `stats.max_is_exact()`.
   - In `fetch_ranges`: replace 1 ms `CANCEL_POLL_INTERVAL` with `CancelToken::register_waker` calling **`notify.notify_one()`** (permit-storing, race-free per R1-M8!) registered BEFORE checking `cancel.is_cancelled()`. Delete `CANCEL_POLL_INTERVAL`.
   - In `coalesce_ranges`: `COALESCE_GAP_BYTES = 64 * 1024`, `MAX_COALESCED_RANGE_BYTES = 8 * 1024 * 1024`.
   - Pass `pool: Arc<ScopedPool>` into `ParquetPartitionStream`; add `phase1_io_res`/`phase2_io_res` reserving `coalesced_bytes` before `fetch_ranges`, plus per-RG byte-capped `chunk_rows` from `rg.column(leaf).uncompressed_size()`.
   - **Safe 1-RG speculative prefetch (R1-M4):** Only spawn next-RG prefetch when `coalesced_bytes <= pool.cap() / 4` and `remaining != Some(0)`. If current RG's phase-2 fetch or decode hits `MemoryLimit` while a prefetch is active, abort & drop the prefetch (freeing `prefetch_io_res`) and retry once before returning `TooLarge`!
   - When `MapProjection::KeysOnly` is set for a `MapUtf8` column, skip fetching/decoding `val_leaf` and construct `MapData` with all-null values of matching length.
   - `QE-m6` + R1-m2: narrow phase-2 `fetch_leaf_readers` to `remaining_kept_ranges` starting at `chunk_ranges[0].start`, and on sparse chunks coalesce surviving row ranges separated by `gap < 64` (avoiding 1-row `skip_records` thrashing).
3. **Typed corrupt-vs-transient errors & local truncation (`QE-M14`, R1-M8):**
   - Emit `QueryError::CorruptData` / `QueryError::StorageIo` in `parquet_source.rs` and match directly in `error.rs::EngineError::from_query` (zero string matching).
   - Pure-sync `pub(crate) fn is_corrupt_store_error(err: &object_store::Error) -> bool` shared with `verify.rs` (`NotFound` + `.source()` chain `InvalidGetRange` / `io::ErrorKind::UnexpectedEof`), plus inside `fetch_ranges`'s `async move` block: on `LocalFileSystem` `Error::Generic`, call `store.head(&path).await` and classify as `FetchError::Corrupt` if `meta.size < expected_file_size`!
4. **Parallel `TracesSource` & `ScoresSource` with bounded `FairSpill`/FD budget, bloom pushdown, & `ProjectMemSnapshot` (`QE-M10`, `QE-M15`, `QE-M16`, R1-M3):**
   - Implement `HashPartitionedSortSink` (`engine/mod.rs`) holding **ONE** spillable `Reservation` per worker (total `W` spillable consumers, NOT `W * P`! per R1-M3) shared across `P = ctx.target_partitions.max(1)` in-memory partition buffers + `P` lazy `SpillAppender`s (0 idle FDs), spilling the largest bucket when the worker's single reservation hits `SpillShare`.
   - Cap per-partition `max_merge_way = (MAX_MERGE_WAY / P).max(2)` (so `P` concurrent merge streams keep total open FDs `<= 33`!), and share `spec.limit` via `Arc<AtomicUsize>` across parallel `ScoresDedupStream`s / `NativeTraceMergeStream`s.
   - `ScoresSource`: push immutable `score_id` and `trace_id` filters AND `IdLookupHint`s below LWW dedup (`QE-M15`).
   - `deletion.rs`: `pub(crate) struct ProjectMemSnapshot(Arc<MemSnapshot>)` required by `TracesSource::new` (`QE-M16`).
5. **WAL `encode_within` `is_all_flat` (`QE-B3`)**, **shared `coordinator_slots` + dedicated `bg_workers` on `QeRuntime` (`QE-B4`, `QE-B5`)**, **`ParquetBatchExporter` `transfer_or_grow` for `pull_res` + output reserve + `chunk_bytes` slicing on BOTH `flush_row_group()` and `finish()` (`QE-M26`, R1-m1)**, **redacted `deletions={}` in `explain_summary` + `EXPLAIN` PII test in `deletion_tests.rs` (`QE-B14`)**, **`file_format/` encapsulation (`open_table_source`/`open_file_scan`/`merge_files_source`) + 109-column Arrow↔core parity test (`QE-M31`)**, and **`OnceLock<Arc<QeRuntime>>` (`MetadataCache::new(0)`) in `fuzz_entry.rs` (`QE-M33`)**.

**Commit:** `storage: Parquet continuation pages, safe async prefetch & coalescing, parallel Traces/Scores, typed corruption, file_format encapsulation, shared coordinator_slots`

---

## Phase H4b — Reads, Server, CLI & Hygiene

**Depends on:** H4a
**Findings closed:** `QE-B19` (reads), `QE-M20` + **bonus `ChunkFlusher` leak**, `QE-M21`, `QE-M33` (server/CLI), `QE-M34`, `QE-m12`.

**Files:** `crates/storage/src/{reads/{facets,scope,context}.rs, query_error.rs, views/catalog.rs, lib.rs, model_tests/{model.rs, resources/{descriptors.rs, disk_full.rs}}}`, `crates/storage/tests/ingest_bench.rs`, `crates/server/src/{http/error.rs, datasets/sql_views.rs, routes/sql.rs}`, `crates/cli/src/query.rs`, `web/src/lib/sql/format-sql.test.ts`.

**Exact Contracts:**
1. **`facets` per-field hard cap + `ChunkFlusher` leak fix (`QE-M20`, R1-B7):**
   - Fix `ChunkFlusher` (`reads/facets.rs:1535, 1551, 1576, 1662`): pass delta (`release`, `bytes`, `*buf_bytes`, `dedup_bytes`) to `self.scratch.shrink(...)`.
   - **Shared across all `P` partitions (R1-B7):** Allocate **one** `field_pool = GlobalPool::standalone(per_field_cap)` + **one** `parent_res` on `exec_ctx.pool` **per value field** *before* `exec_run_pipeline` (shared across all `P` partition sinks and `combine_pass1_sinks`), using `per_field_cap = ((exec_ctx.pool.cap() * 3 / 4) / num_value_fields.max(1)).max(1)` (never exceeding `3/4` of `pool.cap()` regardless of `P` or `num_value_fields`!). Drop a field's `parent_res` immediately when that field trips `TooLarge` or finishes.
2. **`ScanSpec` reads wiring (`QE-B19`, R1-B6):**
   - In `reads/facets.rs:1116-1174`: populate `spec.map_projections` from fields AND merge `collect_expr_map_projections` over ALL `spec.filters` (so `ParsedField::Keys("metadata")` + `WHERE metadata['env'] = 'prod'` upgrades to `MapProjection::Full` instead of losing values!).
   - In `reads/scope.rs:555-630`: emit a single 4-column CNF `OR` clause into `spec.substrings`.
3. **`RunError::Busy` (503) & non-optional `EngineContext.pool` (`QE-M21`):**
   - Add `RunError::Busy` (`kind() == "busy"`, mapped to 503 `ApiError::Unavailable` in `server/src/http/error.rs`) and make `EngineContext.pool: Arc<ScopedPool>` non-optional.
4. **Chunked `dataset_items`/`experiment_items` (`VIEW_BATCH_ROWS = 8_192`, `8 MiB` text cap) (`QE-M34`)**.
5. **Full cell-value decoding in HTTP & CLI Parquet export tests (`QE-M33`)**.
6. **Hygiene (`QE-m12`):** `views/catalog.rs:887` fixture `in_sql` assertion + 5 stale Arrow/DataFusion comments. (`QE-m13` slim `arrow-array`/`arrow-schema` swap is explicitly deferred until Statam removes Arrow completely.)

**Commit:** `storage,server,cli: facets per-field isolation & ChunkFlusher fix, RunError::Busy, chunked SQLite views, Parquet test decoding, hygiene`

---

## Phases H5 & H6 — Verification Gates (`QE-B20`) & Cold Neon High Review *(Done)*

### H5 Verification Results (2026-11-26, Linux 7.3.4, Ryzen 9 3950X 16c/32t, 62 GiB RAM)

- **`just check`:** PASS (Rust fmt/clippy/unit/integration/crash/doctests, `cargo-deny` policy, EE headers, golden dirs, web, Python SDK, TypeScript SDK, `node-cli`, 545-page docs site + link validator).
- **`just e2e`:** PASS (embedded UI + Rust e2e + Python SDK + TypeScript SDK + `node-cli` against live `cetra serve`).
- **`just resources` (`small_fs`, `disk_full`, `descriptors`):** PASS (15/15 tests, including `SPILL_SQL` + `a_spilling_query_stops_at_min_free` + low `RLIMIT_NOFILE`).
- **`just resources-memory` (with `mimalloc` in `cetra-storage` lib tests matching `cetra-cli` / `tests/it/main.rs`, M5-8):**
  - `child_sees_the_scope_limit`: **PASS**
  - `defaults_at_the_documented_minimum` (10.5 GiB cgroup limit): **PASS** (`peak_rss = 8.85 GiB`, `6,695` queries ok, `0` `too_large`, `0` `busy`).
  - `small_profile_at_its_minimum` (provisional 1.25 GiB limit): killed at `1.25 GiB` under `mimalloc` on 32 cores (measured `1.51 GiB` peak RSS under 4 GiB scope — **>2.1× lower** than DataFusion + `mimalloc` on `main`'s `3.21 GiB`; with `MALLOC_ARENA_MAX=2` glibc, passes at `1.25 GiB` with `1.11 GiB` peak RSS). `MIN_MEMORY_SMALL` remains untouched per `hardening.md` M5-7/M5-8.
- **`just model 100`:** PASS (3/3 tests, 15.74s).
- **`just model-s3 50`:** PASS (1/1 test, 87.99s).
- **`just stress 60`:** PASS (19 epochs, 95 rounds, 5,492 steps, 409,439 matched reads, 0 busy/stale/wait_limit, 712 compaction passes, 4,968 GC deletions, 60.13s).
- **ASan libFuzzer (`CC=clang` `just fuzz-ci <target> 20`, all 7 targets):** PASS — **1,500,753 total runs, 0 crashes, 0 OOMs, 0 timeouts** (`column_frame`: 62,186; `query_expr`: 8,676; `sql`: 61,242; `wal_record`: 1,229,970; `wal_replay`: 76,184; `parquet_file`: 6,736; `filter_dsl`: 55,759).
- **`ingest_bench` (15s, oasst2 corpus):** PASS — **34,339 spans/s** (`166.4 MiB/s`), `5.60×` compression, `28.8 MiB/s` compaction (0 failed), `1.99` write amplification.
- **`trace_bench` (100k traces / 1M spans, oasst2 corpus, 122 spans files, 2.5 GiB on disk):** **9/9 PASS, 0 differing IDs** — Q1 `2.9ms` p50 (`3.7ms` p95), Q2 `9.5ms` p50, Q2r-2h `10.2ms`, Q2r-15h `22.6ms`, Q3 `2.9ms` p50 (`3.1ms` p95), Q6 `12.0ms`, Q4a `15.5ms`, Q4b `15.5ms`, Q5 SQL `93.8ms` p50 (`99.4ms` p95), 5-measure widget series `86.3ms` p50; peak RSS `3.6 GiB`.
- **Skipped due to missing host binaries:** `just lazyfs` (`lazyfs` binary not installed) and `just s3-chaos` (`versitygw`/`toxiproxy-server` not installed; `s3_chaos` & `s3_probe` targets verified compiling cleanly).

### H6 Cold Reviews & Final Sync

- Every wave (`H1`, `H2`, `H3a`, `H3b`, `H4a`, `H4b`) received an independent cold Neon High code review and every finding was fixed and verified in follow-up commits (`e2af788`, `fb9268b`, `43386a3`, `9eec660`, `e131209`).
- `design.md` synced with hardened contracts (`Arc<Column>`, `EvalCache`, `MapProjection`, CNF `substrings`, `SpillAppender`, `ScopedPool::child`, `bg_workers`, `ScoresSource` pre-LWW pushdown).
