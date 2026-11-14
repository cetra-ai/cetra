# R · M1 — Model-based correctness

The first milestone of track R (`00-design.md` § M1, decisions D-R-3, D-R-4, D-R-14). A proptest runs random operations against the real `Storage` and against a small in-memory model. After every operation, the test reads storage back through the same `reads` functions the server calls and compares the result with what the model predicts. It replaces `compaction/model.rs`.

**Why it matters.** Wrong results matter more than rare crashes (D-R-1). Today's model test checks one table's raw rows. This one checks every read the server makes, across both projects, through restarts and crashes. That covers spans, traces, scores, sessions, users, metrics and deletion status.

Sub-phases R1.1–R1.8, one commit each. R1.8 can produce several commits, one per bug found.

## Conventions for every sub-phase

- **`just check` passes before each commit.** Node comes from nvm and is not on the non-interactive PATH: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first (if that version is gone, `ls ~/.local/share/nvm/` for the current one). One commit per sub-phase, with the subject given.
- **Scope.** Edit only:
  - `crates/storage/src/model_tests/` (new) and `crates/storage/src/lib.rs` (the `mod` line);
  - `crates/storage/proptest-regressions/`;
  - `justfile` and `AGENTS.md` (R1.7);
  - `docs/`.

  R1.7 also deletes `crates/storage/src/compaction/model.rs`, its `mod` line in `compaction/mod.rs`, and `crates/storage/proptest-regressions/compaction/model.txt`.

  R1.8 may fix bugs anywhere in `crates/storage/src/` (D-R-2). It stops and reports instead when a fix would change a file format, a public API or a spec rule.
- **No new dependency.** Everything needed is already a dependency or dev-dependency of `llmobs-storage`: `proptest`, `tokio`, `tempfile`, `uuid`, `chrono`, `arrow`, `datafusion`. Don't add `proptest-state-machine`; ops are a plain `Vec<Op>` resolved against the model at run time, as `compaction/model.rs` does (D-R-3).
- **All new code is test code.** `lib.rs` gets `#[cfg(test)] mod model_tests;` next to `mod crash_tests;`. Nothing changes outside `cfg(test)` until R1.8. `test_support.rs` is not edited: what it keeps private (`trace_id_of`) is re-implemented in `shape.rs`. Private `#[cfg(test)]` methods of `Storage` such as `staged_paths` are callable from `model_tests`, a child module of the crate root, as they are.
- **Dead code between sub-phases.** `just check-rust` runs clippy with `-D warnings`, and R1.1–R1.6 add items that a later sub-phase calls first. R1.1 puts `#![allow(dead_code)] // removed in R1.7, once every item is used` as the first line of `model_tests/mod.rs`. R1.7 removes it and deletes whatever is still unused.
- **The oracle is independent.** `model.rs` re-implements only the spec rules it needs, in plain Rust over plain structs. It must not call `partials.rs`, `TraceMerger`, `test_support::trace_batch`, `test_support::live_scores`, any `views`/`reads`/`query` function, or DataFusion. A model that reuses the engine's code checks nothing.
- **Generated data stays inside what the spec guarantees.** The shapes in R1.2 avoid every documented known limit:
  - ties on `start_time`;
  - several roots;
  - context fields that differ between spans;
  - partials more than `trace_lookback` apart;
  - duplicates split across partitions.

  If a check fails, first make sure the generator didn't step outside those limits.
- **Panics.** Test code may `unwrap`/`expect`. Every `expect` message says what was expected (e.g. `"trace_list on project A"`).
- **Read timeouts.** Every read request uses `timeout: Duration::from_secs(30)`.
- **Report at the end of each sub-phase:**
  - what was done;
  - any deviation from this plan, and why;
  - how long `cargo test -p llmobs-storage model_tests` took in debug;
  - anything left undone or unverified.

### Names used throughout

- Projects:
  - `PROJECT_A` = `"018f6b4e-7c1a-7c1a-8c1a-0000000000a1"`
  - `PROJECT_B` = `"018f6b4e-7c1a-7c1a-8c1a-0000000000b2"`

  Both parse as `ProjectId`. B is the only project that can be dropped. **The two projects use the same trace numbers and therefore the same trace ids**, with different content (§ Shapes), so a read that leaks across projects fails.
- **Day `D`** = 2026-03-05 (UTC). `midnight(date) -> i64` and `DAY_NS`, `HOUR_NS`, `MINUTE_NS`, `SECOND_NS` are constants in `shape.rs`. Copy `midnight` from `compaction/model.rs`.
- **The clock.** `clock: i64` (ns) starts at `midnight(D) + 12 * HOUR_NS`. It only rises:
  - `Compact` and `ApplyDeletions` add hours;
  - `Settle` jumps it (R1.3);
  - every op adds 1 µs (`op_tick`), so ingest times are strictly increasing across ops.

  Every row an op writes gets `ingest_time = clock` (after the op's tick). Within one op, every row shares that value.

---

## R1.1 — Harness: one runtime per life, crash without shutdown

Commit: `storage: add the model test harness with in-process crashes (R1.1)`

### What it does (D-R-4)

A *life* is one opened `Storage` on a tokio runtime of its own. Ending a life:
- **Restart:** `storage.shutdown().await`, drop the storage, then `runtime.shutdown_timeout(10 s)`. Shutdown flushes both memtables.
- **Crash:** drop the storage **without** `shutdown`, then `runtime.shutdown_timeout(10 s)`. `Storage::drop` aborts the writer, flush, compaction, GC and disk tasks (`lib.rs`, `impl Drop for Storage`). Shutting the runtime down cancels every task and waits for blocking threads, so no task of the old life can still touch the directory when the next life opens it. The memtables are lost; the WAL and the OS page cache are kept, exactly like the `abort()` of `crash_tests.rs`. Unsynced data is not lost here; that is M2's lossy crash.

Each op is `harness.block_on(future)` on the current life's runtime. The proptest body is synchronous, so it can own runtimes.

### Files

`crates/storage/src/model_tests/mod.rs`:

```rust
//! Track R M1: random operations against `Storage` and an in-memory model of
//! every read (docs/plans/r-hardening/01-m1-model.md).

mod harness;
```

R1.2–R1.6 add the other modules here as they land.

`crates/storage/src/model_tests/harness.rs`:

```rust
/// Field order matters only for an implicit drop; `restart` and `crash`
/// always drop `storage` explicitly before shutting `runtime` down.
pub(super) struct Life {
    storage: Storage,
    runtime: tokio::runtime::Runtime,
}

pub(super) struct Harness {
    /// Kept so the directory lives as long as the harness.
    dir: tempfile::TempDir,
    config: StorageConfig,
    life: Option<Life>,
}

impl Harness {
    /// A fresh temp dir and `model_config(dir)`, opened.
    pub(super) fn new() -> Self;
    pub(super) fn dir(&self) -> &std::path::Path;
    pub(super) fn config(&self) -> &StorageConfig;
    pub(super) fn storage(&self) -> &Storage;
    /// Runs `future` on the current life's runtime.
    pub(super) fn block_on<F: std::future::Future>(&self, future: F) -> F::Output;
    /// Clean shutdown, then a new life.
    pub(super) fn restart(&mut self);
    /// Drop without shutdown, then a new life.
    pub(super) fn crash(&mut self);
}

/// The storage config every model case uses.
pub(super) fn model_config(dir: &std::path::Path) -> StorageConfig;

fn open_life(config: &StorageConfig) -> Life;
```

- `open_life` builds `tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()`, then `runtime.block_on(Storage::open(config.clone()))`. It `expect`s both (`"open storage for a new life"`).
- `restart`: `let Life { storage, runtime } = self.life.take().expect("a life")`, then `runtime.block_on(storage.shutdown())` (expect `"clean shutdown"`; `self.block_on` can't be used, the life is taken), `drop(storage)`, `runtime.shutdown_timeout(Duration::from_secs(10))`, then `self.life = Some(open_life(&self.config))`.
- `crash`: take the life the same way, `drop(storage)` (no shutdown), `runtime.shutdown_timeout(Duration::from_secs(10))`, then open a new life. The storage is always dropped **before** the runtime is shut down.
- `impl Drop for Harness`: if a life is left, shut it down cleanly when possible and ignore the error (`let _ = …`). That avoids a noisy panic on an already failing case. Then `shutdown_timeout`.

`model_config(dir)`. Every background task is effectively off: the ops drive flush (through restarts), compaction, deletion passes and GC themselves, so cases are deterministic and shrink well. Background work runs in M6.

```rust
StorageConfig {
    data_dir: dir.to_path_buf(),
    min_free_bytes: ByteSize::b(0),
    object_store: None,
    s3: S3Config::default(),
    wal: WalConfig::default(),                     // sync = "always"
    flush: FlushConfig {
        max_age: Duration::from_secs(3_600),       // the validator's maximum
        ..FlushConfig::default()                   // max_bytes 256 MiB: never reached
    },
    memtable: MemtableConfig::default(),
    query: QueryConfig::default(),
    anchor: Default::default(),
    gc: Default::default(),
    compaction: CompactionConfig {
        interval: Duration::from_secs(3_600),      // the validator's maximum
        l0_files: 2,
        ..CompactionConfig::default()
    },
    deletion: Default::default(),
}
```

If `StorageConfig` has fields this literal misses, the compiler says so. Add them with `Default::default()` and report it.

### Tests (in `harness.rs`, `#[cfg(test)] mod tests` is not needed: the whole module is test-only)

Use `crate::test_support::{span_row, span_rows}` and `project()` = `PROJECT_A` parsed here (R1.2 moves it to `shape.rs`). `span_row` hardcodes the project `…000000000001`, and `append_spans` rejects a row of another project with `InvalidBatch`: set `row.project_id = project()` on every row before `span_rows`.

1. `crash_keeps_acknowledged_appends`:
   - In the first life, append 3 spans in one batch (`span_row(1, s, start, ingest)` for `s` in `0..3`, project set as above, with `start = midnight(D) + 10 * HOUR_NS`) to `PROJECT_A`; each `append_spans` returns `Ok`.
   - `crash()`.
   - In the new life, `storage.query(PROJECT_A, Default::default())`, register `query.spans` in a `SessionContext` built with `SessionContext::new_with_config_rt(SessionConfig::new(), Arc::clone(&query.runtime))` (as `compaction/model.rs::stored_spans` does), and count the rows: 3.
2. `crash_then_restart_flushes_the_replayed_rows`:
   - As 1, then `restart()`.
   - Check that `storage.catalog_files(Table::Spans)` is not empty and that their `rows` sum to 3.
   - Check that `storage.published_rows(Table::Spans)` is 0.
3. `restart_after_crash_twice`:
   - append, `crash()`, `crash()`, append 2 more, `restart()`.
   - 5 rows are readable, and opening never failed. A `Locked` open error means a task of the old life survived, or a concurrently spawned test child holds a copy of the `LOCK` descriptor until it execs (see the comment above `every_flush_hook_aborts_safely` in `crash_tests.rs`). Only `#[ignore]`d tests spawn children, and `just test` runs them on their own. Fix the harness, not the storage.

### Commands

```bash
cargo test -p llmobs-storage --all-features model_tests::harness
```

Expected: 3 passed.

---

## R1.2 — Shapes and the model

Commit: `storage: add the model and the trace shapes of the model test (R1.2)`

### `model_tests/shape.rs`: what every row looks like

Everything about a trace's content is a pure function of `(project, trace number, span number)`. Ops then only choose *which* rows to write and *when*. Trace numbers are `0..12`, span numbers `0..12`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum P { A, B }

impl P {
    pub(super) fn id(self) -> ProjectId;
    /// 0 for A, 1 for B. Mixed into the content so the projects differ.
    pub(super) fn salt(self) -> u8;
}

/// Per-trace constants.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TraceShape {
    pub(super) trace: u8,
    /// The trace's partition day for retention: `D + day_offset`.
    pub(super) day_offset: u8,
    /// Start of span 0.
    pub(super) base: i64,
    pub(super) has_root: bool,
    pub(super) user: Option<String>,
    pub(super) session: Option<String>,
    pub(super) environment: Option<String>,
}

pub(super) fn trace_shape(project: P, trace: u8) -> TraceShape;
/// The 2 big-endian bytes of `trace + 1`, repeated 8 times: the id `span_row(u16::from(trace), ..)` builds.
pub(super) fn trace_id(trace: u8) -> TraceId;
pub(super) fn span_id(span: u8) -> SpanId;      // 8 bytes of `span + 1`, as span_row
pub(super) fn hex(bytes: &[u8]) -> String;      // lowercase hex
/// The full `Span` of `(project, trace, span)` with `ingest_time = ingest`.
pub(super) fn span(project: P, trace: u8, span: u8, ingest: i64) -> Span;
```

`trace_shape(project, t)`, with `k = t + project.salt()`:

| Field | Rule |
|---|---|
| class | `t % 4` |
| `day_offset`, `base` | class 0: day `D`, `base = midnight(D) + 10 h`. Class 1: `D + 1`, `midnight(D+1) + 10 h`. Class 2 (crosses midnight): `D + 1`, `base = midnight(D+2) − 10 min`. Class 3: `D + 3`, `midnight(D+3) + 10 h`. |
| `has_root` | `t % 3 != 2` |
| `user` | `["u0", "u1", none][(k / 2) % 3]` as `Option<String>` |
| `session` | `Some(format!("s{}", k % 4))` when `k % 5 != 4`, else `None` |
| `environment` | `[None, Some("production"), Some(environment::EVAL)][(k / 3) % 3]`, with `environment` from `llmobs_core::names` |

`span(project, t, s, ingest)`. Start from `test_support::span_row(u16::from(t), s, start, ingest)` (it sets every field), then override:
- `project_id = project.id()`;
- `start_time = base + s × 60 s`, `end_time = start_time + 30 s + s × 1 s`. Twelve spans stay within 12 minutes, so far inside `trace_lookback`. For class 2, spans `s ≥ 10` start after midnight;
- `parent_span_id`:
  - span 0 has `None` when `has_root`;
  - otherwise span 0 has `Some(SpanId::from_bytes([0xEE; 8]).unwrap())`, a parent that is never stored (`from_bytes` returns a `Result`);
  - every other span has `Some(span_id(0))`;
- `name`: `"root"` for span 0 when `has_root`, else `format!("op-{s}")`;
- `span_kind`: `SpanKind::Llm` when `s` is odd, else `SpanKind::Span`;
- for LLM spans:
  - `response_model = Some(["m1", "m2"][((s / 2) + project.salt()) % 2])`, with `request_model` left `None`. `Span` has no `model` field: a partial's `models` come from `response_model`, else `request_model` (`partials.rs`), and the spans view derives `model`;
  - `total_tokens = Some(10 × s + project.salt())`;
  - `cost_total = Some(0.125 × s)` (binary-exact, so sums are exact);

  other spans leave all three `None`;
- `status = SpanStatus::Error` when `t % 5 == 1 && s == 2`, else `Ok`;
- `tags = vec!["even"]` for even `s`, `vec!["odd"]` for odd `s`;
- `session_id`, `user_id`, `environment` from the shape (the same on every span: no known limit);
- every other field stays as `span_row` set it.

**Score shapes.** A score is identified by `ScoreKey { project: P, target: Target, name: u8 }`. `Target` is the model's own enum, `Trace(u8)` or `Session(u8)`, named so that it doesn't clash with `llmobs_core::score::ScoreTarget`. A `Trace(t)` score copies the trace's `session` and `user`. A `Session(n)` score has session `format!("s{n}")`, no trace and no user.

```rust
pub(super) fn score_id(key: &ScoreKey) -> Uuid;  // Uuid::from_u128 of a packed (salt, kind, target, name)
pub(super) fn score_created_at(key: &ScoreKey) -> i64;  // Trace(t): trace_shape(...).base; Session(n): midnight(D + 3) + 11 h
/// One version. `version` starts at 1; `updated_at = created_at + version`.
pub(super) fn score(key: &ScoreKey, version: u32, deleted: bool, ingest: i64) -> Score;
```

Build `score` from `test_support::score_row(...)` and override `project_id`, `target`, `session_id`, `user_id` and `name` (`format!("n{name}")`). Use `value = ScoreValue::Numeric(f64::from(version))`.

### `model_tests/model.rs`: state and expected reads

```rust
pub(super) struct SpanCopy { pub(super) ingest: i64 }

pub(super) struct ScoreVersion { pub(super) version: u32, pub(super) deleted: bool, pub(super) ingest: i64 }

pub(super) struct ProjectModel {
    /// Which project: `trace_shape(project, t)` needs it for user, session and environment.
    pub(super) project: P,
    /// Every stored copy of every span key; an empty Vec is never kept.
    pub(super) spans: BTreeMap<(u8, u8), Vec<SpanCopy>>,
    /// Next span number to append, per trace (spans are appended in order).
    pub(super) next_span: BTreeMap<u8, u8>,
    /// Every stored score version: key → versions (version, deleted, ingest).
    pub(super) scores: BTreeMap<ScoreKey, Vec<ScoreVersion>>,
    /// Deletions registered for this project, in order.
    pub(super) deletions: Vec<ModelDeletion>,
    pub(super) keep_from: Option<NaiveDate>,
    pub(super) dropped: bool,
}

pub(super) struct ModelDeletion {
    pub(super) id: Uuid,
    pub(super) cutoff: i64,
    pub(super) traces: BTreeSet<u8>,
    /// `User` scope only.
    pub(super) user: Option<String>,
    pub(super) sessions: BTreeSet<String>,
}

pub(super) struct Model { pub(super) a: ProjectModel, pub(super) b: ProjectModel }

impl Model {
    /// `a.project = P::A`, `b.project = P::B`, everything else empty.
    pub(super) fn new() -> Self;
    pub(super) fn project(&self, p: P) -> &ProjectModel;
    pub(super) fn project_mut(&mut self, p: P) -> &mut ProjectModel;
}
```

**Rows the model keeps.**
- **A written span copy** is kept unless the project is dropped or its trace's day (`D + day_offset`) is before `keep_from`. Storage hides such a row and never flushes it (spec 03 §8).
- **A score version** follows the same rule, using the UTC day of `score_created_at`.
- **A deletion** removes nothing from the state. Coverage is computed at read time, because a copy written after the deletion (ingest above its cutoff) stays visible.
- **A retention** sets `keep_from = max(old, keep)`: the watermark never moves back (spec 03 §8, as the old `compaction/model.rs` did). It then removes the copies and versions of days before the new `keep_from`.
- **A drop** sets `dropped` and clears B's spans and scores. B's `deletions` stay, so their status checks go on (the drop marks them applied, spec 03 §7).

**Coverage** (spec 03 §7). A span copy of trace `t` is covered by deletion `d` when `copy.ingest ≤ d.cutoff` and either:
- `t ∈ d.traces`, or
- `d.user` is `Some(u)` and the trace's user is `Some(u)`.

A score version is covered when `ingest ≤ cutoff` and either:
- its trace `∈ d.traces`, or
- `d.user` is `Some(u)` and the score's user is `Some(u)`, or
- its session `∈ d.sessions`.

A `Traces`-scope deletion has `user = None` and no sessions, so it never covers by user or session. Never compare the two `Option`s with `==`: `None == None` would cover every row without a user.

**Pending reads the same as applied.** A covered copy is invisible in every read. A deletion still pending hides the partials of the older `traces` files whole (`trace_id IN …`). Rows flushed after its registration are written without covered rows, and memtable batches are filtered before their partials are computed (spec 03 §7, `query/traces.rs`). So in this single-threaded harness a trace reads as the merge of its uncovered copies in both states. The model therefore has no notion of pending. `deletion_status` is only checked for its own rules (R1.5).

**Expected reads** are functions of `&ProjectModel`.

```rust
/// Visible copies per key: not covered. Keys with no visible copy are absent.
pub(super) fn visible_spans(&self) -> BTreeMap<(u8, u8), Vec<i64>>;

/// One expected `traces` view row per trace with a visible key, sorted by trace number.
pub(super) fn expected_traces(&self) -> Vec<ExpectedTrace>;

/// True when some key has two or more visible copies (unmerged re-sends).
pub(super) fn unsettled(&self, trace: u8) -> bool;

/// Live scores: per key, the visible version with the greatest
/// `(updated_at, ingest)`, dropped when it is `deleted`.
pub(super) fn expected_scores(&self) -> Vec<ExpectedScore>;
```

`ExpectedTrace` holds:
- `trace_id: String` (hex);
- `start_time`, `end_time`;
- `name`, `root_span_id: Option<String>`, `has_root`;
- `status: &'static str` (`"error"`, `"ok"`, `"unset"`);
- `session_id`, `user_id`, `environment`;
- `tags: Vec<String>` and `models: Vec<String>`, sorted and distinct;
- `span_count`, `error_count`, `llm_count`;
- `total_tokens: Option<i64>`, `cost_total: Option<f64>`.

It is computed from the trace's visible keys, using **one row per key** (its newest copy; every copy has the same content), by the spec 01 merge rules applied to all spans at once. That equals the merge of partials for these shapes (spec 01 § Order):
- `start_time` = min, `end_time` = max;
- `has_root` is derived from the **visible** keys: true when the shape has a root **and** span 0 is visible. A deletion can hide span 0 while later spans stay.
- With a root: `name`, `root_span_id` and the root's status come from span 0. Without one: `name` = the name of the visible span with the smallest `(start_time, span_id)`, and `root_span_id = None`;
- `status` = `"error"` if any visible span has `Error`; else, with a root, the root's status (`"ok"`); else `"unset"`;
- the context fields come from the shape;
- `tags` and `models`: distinct union;
- counts: `span_count` = number of keys, `error_count` = spans with `Error`, `llm_count` = spans with `SpanKind::Llm`;
- `total_tokens`, `cost_total`: sum over spans that have a value, `None` when none has one. `partials.rs` does the same: all-null input gives `None`.

`ExpectedScore` holds `score_id: String` (lowercase hyphenated UUID), `name`, `target_kind` (`"trace"`/`"session"`), `trace_id: Option<String>`, `session_id: Option<String>`, `value_num: f64` and `created_at`.

The model never looks at levels, files or partitions.

### Tests (`model.rs`)

Add `ProjectModel::new(project: P)` (everything empty) for these tests and for `Model::new`. Derive `Debug, Clone, PartialEq` on `ExpectedTrace` and `ExpectedScore`, and `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord` on `Target` and `ScoreKey` (a `BTreeMap` key). Each test builds a `ProjectModel` with `new` and fills it by hand to check one rule:
- `expected_trace_without_root_takes_the_earliest_name`;
- `expected_trace_error_wins_over_root_status`;
- `expected_trace_sums_none_when_no_span_has_tokens` (spans 0 and 2 only);
- `deleted_copies_vanish_and_late_copies_stay`: span 0 copied at ingest 5, span 1 at ingest 20, a `Traces` deletion with cutoff 10. One trace, with only span 1, `has_root = false`, named `"op-1"`;
- `traces_deletion_does_not_cover_rows_without_a_user`: a `Traces` deletion of trace 1 leaves a user-less trace 3 visible;
- `user_deletion_covers_scores_by_session`;
- `live_score_is_the_greatest_updated_at_and_drops_deleted`;
- `retention_drops_the_day_and_hides_later_writes_to_it`.

---

## R1.3 — Ops, the runner, spans and traces

Commit: `storage: run spans and traces ops against the model (R1.3)`

### `model_tests/ops.rs`

```rust
#[derive(Debug, Clone)]
pub(super) enum Op {
    /// Appends the next `count` spans (1..=6) of `trace`, in one batch, or in
    /// two batches when `split`. Spans past 11 are not appended.
    Append { project: P, trace: u8, count: u8, split: bool },
    /// Appends again one span of `trace` (picked by `pick % len`) with a new
    /// ingest time. The candidates are `0..next_span[trace]`, whether covered
    /// by a deletion or not. A class-2 trace (`trace % 4 == 2`) is never
    /// re-sent: its anchor can move between flushes (`anchor.rs`), so a copy
    /// could land in another partition and never be merged (known limit).
    Resend { project: P, trace: u8, pick: u8 },
    Restart,
    Crash,
    /// Advances the clock by `hours` (0..72), then one compaction pass.
    Compact { hours: u8 },
    /// `collect_obsolete(Duration::ZERO)`.
    Gc,
    /// Everything flushed, every deletion applied, every partition compacted,
    /// nothing obsolete (below).
    Settle,
}

pub(super) fn ops() -> impl Strategy<Value = Vec<Op>>;
```

`ops()` in R1.3 has these weights: `Append 30`, `Resend 4`, `Restart 3`, `Crash 3`, `Compact 3`, `Gc 1`, `Settle 1`. The project is A with weight 3 and B with weight 1. Use `proptest::collection::vec(op, 1..=24)`. R1.4 and R1.5 add variants and weights; keep the weights in one `prop_oneof!`.

`pub(super) fn apply(op: &Op, harness: &mut Harness, model: &mut Model, clock: &mut i64)`:
- First `*clock += MICRO_NS` (1,000).
- `Append`:
  - A dropped project: the append returns `Err(AppendError::ProjectDropped)`, and the model does not change.
  - Otherwise, take `n = next_span[trace]` and build `span(project, trace, s, *clock)` for `s ∈ n..min(n + count, 12)`.
  - With `split` and at least 2 spans, append the first `len / 2` and then the rest, in two `append_spans` calls. Otherwise append them all in one call. Expect `Ok`. An empty batch is rejected as `InvalidBatch`, so never send one; when `n` is already 12 the op appends nothing and calls nothing.
  - The model pushes `SpanCopy { ingest: *clock }` for each key the keep rule keeps, and advances `next_span` either way.
- `Resend`: picks among the candidates above. With no candidate it is a no-op without a call. A dropped project behaves as in `Append`. The model pushes the new copy when the keep rule keeps it.
- `Restart` / `Crash`: `harness.restart()` / `harness.crash()`. The model does not change; acknowledged data survives both.
- `Compact`: `*clock += hours × HOUR_NS`, then `storage.compact(*clock)`; assert `report.failed == 0`.
- `Gc`: `storage.collect_obsolete(Duration::ZERO)`.
- `Settle`: see below.

**`settle(harness, model, clock)`** follows the quiesce steps of `compaction/model.rs::run_case` 1–5:
1. `harness.restart()`.
2. Advance the clock to `max(clock, midnight(D + 6)) + compaction.close_delay + 1 ms`.
3. Up to 10 times: `apply_deletions(clock)` (assert `failed == 0`) until every registered deletion of both projects is `Applied`. If one is still pending after 10 passes, the case fails.
4. Up to 10 times: `compact(clock)` (assert `failed == 0`) until `report.jobs == 0`. If jobs remain after 10 passes, the case fails.
5. `collect_obsolete(Duration::ZERO)`.
6. In the model, keep only the newest copy of each key. Compaction collapsed them, since every partition is now one fully merged file. The generator keeps duplicates in one partition, which is what makes this true.
7. Run the physical checks (R1.7 adds them; R1.3 leaves a `// R1.7` call site out, not a stub).

### `model_tests/observe.rs`: reading storage

Each function opens `storage.query(project.id(), QueryOptions::default())` and builds `QueryContext::new(query, None)`. Each one pages through `next_cursor` until it is `None`, with `limit: MAX_PAGE`. The model never has more than about 300 rows, so one page is the norm, but paging must work.

```rust
/// `(trace_id hex, span_id hex)` of every `span_list` row, duplicates kept.
pub(super) fn observe_spans(h: &Harness, project: P) -> Vec<(String, String)>;
pub(super) fn observe_traces(h: &Harness, project: P, scope: ListScope) -> Vec<ExpectedTrace>;
pub(super) fn observe_trace_detail(h: &Harness, project: P, trace: u8) -> Option<(ExpectedTrace, Vec<String>)>;
pub(super) fn observe_span_detail(h: &Harness, project: P, trace: u8, span: u8) -> Option<i64>;
```

- **`observe_spans`.** `span_list` returns `SPAN_LIST_COLUMNS` (`reads/traces.rs`), which has no `ingest_time`. Read its `trace_id` and `span_id` (hex `Utf8`). The ingest checks use `observe_span_detail`, which returns every column. Use `ListScope { window: None, include_eval: true, include_experiments: true }`, `sort: SpanSort::StartTime`, `order: SortOrder::Asc`, no filter, no search.
- **`observe_traces`.** Calls `trace_list` with `sort: TraceSort::StartTime`, `order: SortOrder::Desc`, no filter or search, and converts each row into an `ExpectedTrace`. Read the columns by name with `RecordBatch::column_by_name` and `arrow::array::AsArray`; the names are the constants in `llmobs_core::schema::traces`, and the types are those of spec 01 with hex `Utf8` ids (`views/traces.rs`). Sort `tags` and `models` before returning.
- **`observe_trace_detail`.** Calls `trace_detail(ctx, &hex(trace_id), timeout)`. It returns the trace row (same conversion) and the `span_id`s of its spans, in the returned order.
- **`observe_span_detail`.** Calls `span_detail` and returns its `ingest_time`.

### `model_tests/check.rs`

```rust
/// After an op that touched one project: that project's spans and trace list.
pub(super) fn check_quick(h: &Harness, model: &Model, project: P, index: usize);
/// After Restart, Crash and Settle, and at the end: both projects, `check_quick`'s
/// reads plus the sampled details below.
pub(super) fn check_full(h: &Harness, model: &Model, index: usize);
```

Ops without a project (`Compact`, `Gc`; later `ApplyDeletions`, `DropB`) run `check_quick` on both projects. Keep the number of queries small: debug-mode DataFusion queries cost milliseconds each, and the budget below is per case.

**Spans**, per project:
- the set of `(trace, span)` keys from `observe_spans` equals the keys of `visible_spans()`;
- each key appears at least once and at most as many times as it has visible copies;
- in `check_full`, for **3 keys** per project (the first, middle and last visible key in key order), `observe_span_detail` returns the newest visible copy's ingest. `span_detail` keeps the greatest `ingest_time` among duplicates (`reads/traces.rs`). One key that has no visible copy (when there is one) returns `None`.

**Traces**, per project, with `scope = ListScope { window: None, include_eval: true, include_experiments: true }`:
- the trace ids equal those of `expected_traces()`;
- for a trace where `unsettled(t)` is false, the observed row equals the expected one, field by field. Floats compare with `(a − b).abs() ≤ 1e-9`;
- for an unsettled trace, every field except the counts, `total_tokens` and `cost_total` is equal. `span_count` is between the expected value and the expected value plus the extra copies, because unmerged re-sends count twice (spec 03 §6 *Duplicates*);
- the rows come back in `start_time DESC, trace_id DESC` order.

**Details** (`check_full` only): for **3 traces** per project (the first, middle and last expected trace), `observe_trace_detail` returns `Some` with the same row as the list, and its span ids equal the trace's visible keys (as a set, duplicates allowed as above). One trace number in `0..12` that is not expected returns `None`.

**Failure messages** print:
- the op index;
- the project;
- the expected and observed values.

proptest prints the shrunk op list.

### The proptest (`model_tests/mod.rs`)

```rust
fn cases() -> u32 {
    std::env::var("LLMOBS_MODEL_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(16)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Random ops on two projects read back exactly as the model predicts.
    #[test]
    fn storage_matches_the_model(ops in ops::ops()) {
        run_case(&ops);
    }
}

fn run_case(ops: &[Op]) {
    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(D) + 12 * HOUR_NS;
    for (index, op) in ops.iter().enumerate() {
        apply(op, &mut harness, &mut model, &mut clock);
        match op {
            Op::Restart | Op::Crash | Op::Settle => check_full(&harness, &model, index),
            _ => match op.project() {
                Some(project) => check_quick(&harness, &model, project, index),
                None => {
                    check_quick(&harness, &model, P::A, index);
                    check_quick(&harness, &model, P::B, index);
                }
            },
        }
    }
    settle(&mut harness, &mut model, &mut clock);
    check_full(&harness, &model, ops.len());
}
```

`Op::project(&self) -> Option<P>` returns the op's project, `None` for ops without one.

**Budget (D-R-14).** Measure first: run `cargo test -p llmobs-storage --all-features model_tests::storage_matches_the_model` in debug with `LLMOBS_MODEL_CASES=4`, and time it. Set the default in `cases()` to the largest of 16, 8 or 4 that keeps the test under 60 s, and report the time and the value chosen. R1.4–R1.6 add reads: re-measure at the end of each, lower the default when needed, and report. Below 4 cases, stop and report: the checks then need to be cut further.

### Commands

```bash
cargo test -p llmobs-storage --all-features model_tests
LLMOBS_MODEL_CASES=200 cargo test --release -p llmobs-storage --all-features model_tests::storage_matches_the_model
```

Both pass. A failure in the 200-case run is a finding for R1.8: write it down in the report with the shrunk ops, but don't fix it in R1.3. proptest writes its seed to `crates/storage/proptest-regressions/model_tests/mod.txt`, and a file left there is replayed by every later `just check`. Move it out of the tree (to the scratchpad) and name its path in the report; R1.8 brings it back.

---

## R1.4 — Scores

Commit: `storage: add score ops and checks to the model test (R1.4)`

### Ops

```rust
/// Writes the next version of the score `(project, target, name)`: version 1
/// when it doesn't exist yet. `target` < 12 is Trace(target), else Session(target - 12) with target in 12..16.
Score { project: P, target: u8, name: u8 },   // target 0..16, name 0..3
/// Writes version `last + 1` with `deleted = true` of one score the model
/// has written (`pick % len`, in key order). A no-op without a call when the
/// project has none. A later `Score` of that key writes `last + 1` again,
/// live: the model's LWW rule already brings it back.
DeleteScore { project: P, pick: u8 },
```

Weights: `Score 8`, `DeleteScore 2`. `Op::project()` returns `Some(project)` for both.
- `Score` on a trace that has no appended span is allowed: scores don't need their target to exist.
- Each version gets `updated_at = created_at + version` and `ingest_time = clock`.
- `append_scores` with one batch (`scores_to_record_batch(&[score])`).
- A dropped project behaves as in `Append`: `Err(AppendError::ProjectDropped)` and no model change.
- The keep rule uses the score's `created_at` day.

### Observe and check

- `observe_scores(h, project)` pages through `score_list` (`ScoreListRequest` with every filter `None`, `window: None`) and converts each row into an `ExpectedScore`.
- `observe_scores_by_id(h, project, ids)` calls `scores_by_id`.
- `observe_latest(h, project, targets)` calls `latest_scores` with `ScoreTargets::Traces` (hex ids of traces `0..12`) and `ScoreTargets::Sessions` (`"s0".."s3"`).

Checks:
- `check_quick`: `observe_scores` equals `expected_scores()` as a set, compared by `score_id` with every field equal.
- `check_full` adds:
  - `scores_by_id` over every `score_id` the model ever wrote (live or not) returns exactly the live ones;
  - `latest_scores` returns, per `(target_key, name)`, the live score. It picks the live score with the greatest `updated_at`, then `score_id` (`reads/scores.rs`, module doc). The model has one score id per `(target, name)`, so there is exactly one candidate and the check is exact.

### Settle

Settling also drops deleted score versions from files (a `scores` day close drops `deleted` rows, spec 03 §5). Reads are unchanged by that, so the model needs nothing new. R1.7 checks it physically.

---

## R1.5 — Deletions, retention, project drop

Commit: `storage: add deletions, retention and drop to the model test (R1.5)`

### Ops

```rust
/// `register_deletion` of 1–3 traces (`picks`, each `% 12`) plus their
/// evaluation traces, cut off at the op's ingest time.
DeleteTraces { project: P, picks: Vec<u8> },
/// A user deletion as the server resolves it (D-M-3), through
/// `user_deletion_scope` and `eval_trace_ids`.
DeleteUser { project: P, user: u8 },     // user 0..2 → "u0" | "u1"
/// Advances the clock by `hours` (0..24), then one deletion pass.
ApplyDeletions { hours: u8 },
/// `apply_retention(project, keep)`, keep ∈ {D+1, D+3, D+4} (index 0..3).
Retention { project: P, keep: u8 },
/// `drop_project(B)`.
DropB,
```

Weights: `DeleteTraces 3`, `DeleteUser 2`, `ApplyDeletions 2`, `Retention 1`, `DropB 1`. `Op::project()` returns `Some(project)` for `DeleteTraces`, `DeleteUser` and `Retention`, `Some(P::B)` for `DropB`, and `None` for `ApplyDeletions` (both projects are checked).

- **`keep` never equals `D + 2`.** Class-2 traces have rows on days `D + 1` and `D + 2` whose partition depends on the anchor (spec 03 § Trace anchor). Every allowed `keep` treats both days alike, so the model needs no anchor logic (D-R-3).
- **Deletion ids** are `Uuid::now_v7()`; the cutoff is `clock` after the op's tick.
- **Evaluation traces (shape change in this sub-phase).** In `shape.rs::span`, every span of a trace whose environment is `environment::EVAL` gets `metadata = {metadata_key::EVAL_TARGET_TRACE_ID: hex(trace_id((t + 1) % 12))}` (`llmobs_core::names::metadata_key`). The value is the same on every span of the trace, so no known limit is hit. The model doesn't compare `metadata`.
- **Resolution goes through the real reads (D-M-3, D-M-6).** The server resolves a deletion's scope with `crate::reads::{user_deletion_scope, eval_trace_ids}` before it calls `register_deletion`. The op does the same, on a `QueryContext` of the project, and first checks each read against the oracle:
  - `DeleteUser`: `user_deletion_scope(ctx, "u<n>", cutoff, timeout)` must return:
    - as `trace_ids`, the traces whose shape has `user == Some(u)` and that have at least one **visible** copy (all copies are ≤ the cutoff, since the cutoff is the op's ingest time), sorted by bytes;
    - as `session_ids`, those traces' `Some` sessions, sorted and distinct.

    Copies already covered by an earlier deletion are not visible, so a trace with no visible copy is not in the scope. The coverage is the same either way.
  - Both ops then call `eval_trace_ids(ctx, &targets, timeout)` with the resolved trace ids as `targets`. It must return the traces whose environment is `EVAL`, that have a visible copy, whose target (`(t + 1) % 12`) is in `targets`, and that are not in `targets` themselves (one level, not transitive), sorted by bytes.
  - On a mismatch, the case fails with both sets printed. That is a finding about M's resolution reads.
  - The deletion is registered with the real reads' results: `trace_ids` = targets ∪ eval traces, and for `DeleteUser`, `user_id` and the returned `session_ids`. The model's `ModelDeletion` stores the same sets.
  - If no trace matches a user, register the deletion anyway, with empty `trace_ids` and `session_ids`: it still covers rows by `user_id`.
- **A dropped project** skips `register_deletion` and `apply_retention`. The model ignores the op, and nothing is called.
- **`DropB`** expects `Ok` the first time; later times are a no-op without a call. The model applies the drop rule of R1.2.

### Checks

`check_quick` and `check_full` already read through the model's coverage. Add:
- **Deletion status** in `check_full`: every registered deletion's status is `Pending` or `Applied`, and once seen `Applied` it never reads `Pending` again. Add `pub(super) applied_seen: bool` to `ModelDeletion`; `check_full` takes `model: &mut Model` from R1.5 on and sets it. Update the R1.3 call sites in the same commit.
- **After `Settle`:** every deletion is `Applied`.
- **A dropped B:** every read of B is empty, and `register_deletion` for B is not called.
- **Unit test** `drop_keeps_project_a`: append the same trace numbers to A and B, `DropB`, `Settle`. All of A's reads match the model. B's are empty.

---

## R1.6 — Sessions, users, metrics, windows, exclusions

Commit: `storage: check sessions, users, metrics and windows in the model test (R1.6)`

All of these are in `check_full`, per project. Each one is compared **only when no visible trace of the project is unsettled**: unmerged re-sends count twice in the traces view, so the aggregates would only have bounds. The read still runs in that case, and it must return `Ok`.

- **Windowed trace list.** `observe_traces` with `window = TimeWindow::new(midnight(D+1), midnight(D+2)).unwrap()` (`new` returns a `Result`). Expected: the traces whose merged `start_time` lies in `[from, to)`. That is class 1 and class 2, since class 2 starts at 23:50 on `D + 1`. Their partials sit within minutes of each other, so `trace_lookback` doesn't matter.
- **Eval exclusion.** `observe_traces` with `include_eval: false`. Expected: the traces whose environment is not `environment::EVAL`.
- **Sessions.** `session_list` (`ListScope` with no window, `include_eval: true`, `include_experiments: true`, `sort: SessionSort::StartTime`, `order: SortOrder::Desc`). Compare per `session_id`: `trace_count`, `error_count` (sum of the traces' `error_count`), `total_tokens` and `cost_total` (sums, `None` when no trace has one). The expected rows come from `expected_traces` grouped by `session_id`, with `None` sessions excluded (`views/sessions.rs` filters them).
- **Users.** `user_list` with the same scope, `sort: UserSort::TraceCount`, `order: SortOrder::Desc`. Compare per `user_id`: the trace count (column `trace_count`). The expected rows are the `expected_traces` grouped by `user_id`, with `None` excluded: `user_list` drops null users (`reads/users.rs`).
- **Metrics.** `metrics` with:
  - `window = TimeWindow::new(midnight(D), midnight(D+4)).unwrap()`;
  - `bucket: Bucket::Fixed(BucketWidth::D1)`;
  - `filter: None`, `group_by: None`;
  - `include_eval: false`, `include_experiments: true`.

  Compare the KPIs `Traces`, `ErrorRate`, `CostTotal` and `TotalTokens`. Their `value` comes from the expected traces with `start_time` in the window and a non-eval environment:
  - the count;
  - the share with `error_count > 0`;
  - the cost sum;
  - the token sum.

  All four are `None` when there is no such trace. `previous` covers `[midnight(D−4), midnight(D))` and is always `None`. `LatencyP95` is not compared (approximate percentile); it only has to be present. Floats compare with `1e-9`.
- **Filter-deletion scope** (track M's resolve read, D-M-2). `filter_trace_ids` with:
  - `filter: None`, `search: None`;
  - `window = [midnight(D), midnight(D+4))`;
  - `include_eval: false`, `include_experiments: true`;
  - `limit: MAX_FILTER_DELETION_TRACES + 1`.

  Expected: the trace ids, sorted by bytes, of the expected traces in that window whose environment is not `EVAL`, which are exactly the traces the trace list shows with the same scope.
- **Alert values** (track K's read, `reads/alerts.rs`). `alert_value` with `window = [midnight(D), midnight(D+4))`, `filter: None`:
  - `AlertMeasure::ErrorRate`: `count` is the number of expected traces in the window with a non-`EVAL` environment, and `value` is the share of them with `error_count > 0` (`None` when `count` is 0). Experiment traces are always excluded; the shapes have none.
  - `AlertMeasure::Cost`: the same count; `value` is their cost sum, always `Some` (`0.0` with no trace or no cost).
  - `AlertMeasure::ScoreAvg { name: "n<k>" }` for `k` in `0..3`. The rows are the live **trace** scores (session scores never count) with that name and `created_at` in the window, whose trace passes the semi-join of `alerts.rs::trace_id_plan`. That means the trace is an expected, non-`EVAL` trace with `end_time ≥ from` and `start_time ∈ [from − 1 day, to)`. `count` is those rows, and `value` is the mean of their `value_num` (`None` when `count` is 0).

  `TraceLatencyP95`, `LlmLatencyP95` and `ScorePassRate` are not compared: the percentiles are approximate, and the shapes write no boolean scores.

---

## R1.7 — Physical checks, the old model removed, the recipe

Commit: `storage: check settled files physically and replace the compaction model test (R1.7)`

### Physical checks after every `Settle`

`check_settled_files(h, model)` in `check.rs` runs at the end of `settle`. It reads every active file with `storage.catalog_files(FileTable::…)` and `test_support::object_rows(h.dir(), &file)`, and checks:
1. **`spans`.**
   - No row is covered by any registered deletion of its project. Use the model's coverage on `(trace_id, ingest_time)` (and `user_id`); the file's project is `file.project`.
   - No file's `day` is before its project's `keep_from`.
   - No file belongs to a dropped project.
   - Within one file, no `(trace_id, span_id)` appears twice.
2. **`scores`.** Same coverage rule; no `deleted = true` row is left in any file; each file is `fully_merged`.
3. **`traces`.**
   - Query the `traces` provider (`query.traces`, the merging provider) as `compaction/model.rs::stored_partials` does: `SELECT trace_id, partial_count FROM traces_t`. It returns one merged row per trace, and `partial_count` is the sum of its partials (spec 01).
   - The trace ids returned are exactly those of `expected_traces()` for that project. Run it with the project's id; nothing else may appear.
   - For classes 0, 1 and 3, `partial_count == 1`.
   - For class 2, `partial_count ≤ 2`. When its anchor entry has expired, a late span starting after midnight goes to its own day, so the trace has one partial per day (spec 03 § Trace anchor, known limit).
4. **Staged objects.** `storage.staged_paths().await` is `Ok` and empty after the GC. The method is private and `#[cfg(test)]` in `lib.rs`; `model_tests` is a child of the crate root, so it can call it unchanged.

### Remove the old model test

The new test covers all of its ops (append, restart, compact, GC, delete, apply, retention) and its final physical checks (covered rows, one partial per trace). Delete:
- `crates/storage/src/compaction/model.rs`;
- its `#[cfg(test)] mod model;` line in `crates/storage/src/compaction/mod.rs`;
- `crates/storage/proptest-regressions/compaction/model.txt`.

Before deleting it, port its first regression case to the new op set as the named unit test `regression_compaction_restart_resend` in `model_tests/mod.rs`. It calls `run_case(&[...])` with:
1. `Append { project: P::A, trace: 1, count: 1, split: false }`;
2. `Restart`;
3. `Compact { hours: 41 }`;
4. `Compact { hours: 66 }`;
5. `Compact { hours: 34 }`;
6. `Resend { project: P::A, trace: 1, pick: 0 }`.

The old case re-appended the same key after its day was compacted, which is a `Resend` here, not another `Append`. The second old case is not ported. It addressed spans by number in the old day scheme (`trace % 3`), which the new shapes don't have, and the random ops cover the same mix (appends, restarts, compactions, GC) with re-sends.

Remove `#![allow(dead_code)]` from `model_tests/mod.rs` in this sub-phase (§ Conventions). Delete any item clippy then reports as unused.

### `justfile`

Add, after `test:`:

```just
# The storage model test with more cases (D-R-14), in release mode.
model cases="500":
    LLMOBS_MODEL_CASES={{cases}} cargo test --release --workspace --all-features -- model_tests::storage_matches_the_model
```

### Docs

- **`AGENTS.md`.**
  - § Commands: add a row for `just model [cases]`: "the storage model test with more cases, in release mode (D-R-14)".
  - § Tests: add a bullet: "`LLMOBS_MODEL_CASES=N` sets the case count of the storage model test (`model_tests::storage_matches_the_model`, default set in R1.3); `just model N` runs it in release mode. It is separate from `LLMOBS_PROPTEST_CASES`, which sets the count of the cheap property tests of otel and eval: one model case costs about a second, so one variable for both would make a high count unusable."
- **`docs/research/storage/hardening.md`** (new). Title `# Storage hardening — findings`, then a one-paragraph intro: track R, `docs/plans/r-hardening/`. Then `## M1 — Model-based correctness` with:
  - what the test covers, in one list;
  - `### Runs`: a table of date, commit, cases, mode, duration and result, with R1.7's own `just model 500` run as the first row;
  - `### Findings`: empty until R1.8, with the line "None yet."
- **`docs/spec/03-storage.md` § 10 Failure handling** gets one sentence after the table: "The model test (`crates/storage/src/model_tests/`, track R M1) checks every read against an in-memory model through restarts and crashes."

### Commands

```bash
just check-rust
just model 500
```

`just check` passes. Record the `just model 500` run in `hardening.md`. If it fails, record the shrunk case as a finding in `hardening.md`, move the seed file out of the tree as in R1.3, commit R1.7 anyway, and leave the fix to R1.8.

---

## R1.8 — Long runs and fixes

Commit per bug: `storage: <fix in imperative> (R1.8)`. A final docs commit: `docs: record the M1 model runs (R1.8)`.

1. Run `just model 5000`, three times, each time with a fresh `PROPTEST_RNG_SEED` (proptest reads this env var; print it in the report). One run takes hours: start it in the background with its output in a log file (`just model 5000 > <scratchpad>/model-1.log 2>&1`), not as a foreground command, which times out after 10 minutes.
2. **For each failure:**
   1. Save the shrunk op list. proptest writes it to `crates/storage/proptest-regressions/model_tests/mod.txt`; keep that file.
   2. Add it as a named unit test in `model_tests/mod.rs` (`regression_<what>`) calling `run_case(&[...])`.
   3. Decide whose bug it is:
      - **The generator stepped outside a known limit** (§ Conventions): fix the generator and say which limit.
      - **The model disagrees with spec 03/01:** fix the model and cite the spec line.
      - **Storage disagrees with the spec:** find the cause in `crates/storage/src`. If the fix is local (one module, no format, public API or spec change), fix it in the same commit as the regression test. Otherwise stop and report the shrunk case, the cause you found, and the change it needs.
   4. Add a row to `hardening.md` § Findings:
      - id `M1-<n>`;
      - the shrunk ops (or the test name);
      - the cause;
      - the classification (storage, model or generator);
      - the fix commit.
3. When three runs of 5,000 cases pass, record them in § Runs and update the roadmap row (R1.8's docs commit): "M1 done".

If a storage cause is not found within the code paths the failing read touches, stop and report. Don't guess.

---

## What not to build in M1

- **Lossy crashes, the `fs` seam, fsync fixes:** M2.
- **Background tasks during ops, concurrent readers:** M6.
- **S3 mode:** M4 adds it with `ChaosStore`.
- **Media, `trace_spans`, `trace_refresh`, `session_detail`, `user_detail`, the SQL console, filters and search text:** the read functions M1 checks are the list, lookup and aggregate paths that every page uses, plus the scope reads of deletion (R1.5, R1.6) and the alert measures (R1.6).
- **P's `QueryOptions::visible_from`** (D-P-21, P's M4): not merged yet. When it lands, an R follow-up sub-phase adds it to the model's read set (`00-design.md` § Other tracks in `crates/storage`). Adding filters means modelling the filter language; that is a later extension once M1's findings are in.
- **A nightly workflow:** M3, together with fuzzing.
