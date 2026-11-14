# R · M6 — Concurrency

The sixth milestone of track R (`00-design.md` § M6, decision D-R-10). M1's model test runs one op at a time with every background task effectively off. M6 adds a **concurrent mode**: flush, compaction, deletion passes and obsolete-file GC run for real, reader threads run M1's reads continuously while the ops proceed, and every read must equal the model in some state between the one before and the one after the step in flight. A release-mode **stress test** repeats this for minutes, with several writer lanes on disjoint projects, more readers and more tokio workers, and checks that the engine returns to a clean state at the end. Targeted deterministic tests, built on the `hooks.rs` pause points, cover the races the review below found plausible and not yet tested, starting with a confirmed GC bug.

**Why it matters.** Reads take one published `ReadState` (spec 03 §4), and every commit publishes under the committer mutex. A race in that machinery (a lost publish, a snapshot that mixes two commits, GC deleting a file a reader still uses) gives wrong or failing reads only under load, which no single-threaded test can show. This milestone looks for exactly that, with the M1 oracle.

Sub-phases R6.1–R6.9, one commit each. R6.8 and R6.9 can produce several commits, one per bug found (D-R-2).

**Decisions applied** (user, 2026-11-15): obsolete-file GC in the tests is driven by a test thread, with no new config key; the GC-after-raise fix keeps a `Weak` list of superseded `FileMeta` versions in the committer; the nightly workflow (M3) runs `just stress 1800` locally and a 10-minute remote-mode run.

## Conventions for every sub-phase

- **`just check` passes before each commit.** Node comes from nvm and is not on the non-interactive PATH: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first (if that version is gone, `ls ~/.local/share/nvm/` for the current one). One commit per sub-phase, with the subject given.
- **Prerequisite.** M1 is merged, through R1.7: `crates/storage/src/model_tests/` has `harness.rs`, `shape.rs`, `model.rs`, `ops.rs`, `observe.rs`, `check.rs`; `run_case`, `settle`, `check_quick`, `check_full`, `check_settled_files`; the ops and reads of R1.3–R1.6 (spans, traces, scores, deletions resolved through `user_deletion_scope`/`eval_trace_ids`, retention, drop, sessions, users, metrics, windows, `filter_trace_ids`, `alert_value`); and `just model`. M1's `#![allow(dead_code)]` is gone. This plan names M1's items as `01-m1-model.md` and the merged code define them (for example, the merged observe functions take a page `limit`, and `check_full` has a `check_paging` step). Where the merged code differs from a name here, follow the code and say so in the report. Don't change M1's behaviour except where a sub-phase says so.
- **No dead code, no `#[allow(dead_code)]`.** `just check-rust` runs clippy with `-D warnings`. Each item is added in the sub-phase that first uses it; an item only a unit test uses is fine (the whole module is test code). If clippy reports an unused item, move it to the sub-phase that uses it, don't silence it.
- **Scope.** Edit only:
  - `crates/storage/src/model_tests/` (all sub-phases);
  - `crates/storage/proptest-regressions/model_tests/`;
  - `crates/storage/src/test_support.rs`: `parquet_rows` (R6.6);
  - `crates/storage/src/disk.rs`: one `#[cfg(test)]` accessor (R6.6);
  - `crates/storage/src/catalog/committer.rs`, `crates/storage/src/catalog/mod.rs` and `crates/storage/src/gc.rs` (R6.4: the fix and its test);
  - the test modules named in R6.8: `read_cache.rs` (its `mod tests`), `drop_tests.rs`, `retention.rs`, and, if N's B2/B3 have merged, the test modules of `snapshot.rs` and `lib.rs`;
  - `justfile`, `AGENTS.md` and, if it exists, `.github/workflows/nightly.yml` (R6.7);
  - `CHANGELOG.md` (R6.4, and any user-visible fix of R6.8–R6.9);
  - `docs/`.

  R6.8 and R6.9 may fix bugs anywhere in `crates/storage/src/` (D-R-2). Stop and report instead when a fix would change a file format, a public API or a spec rule beyond the spec sentence a sub-phase names.
- **No new dependency.** Everything needed is already a dependency or dev-dependency of `llmobs-storage`: `proptest` (1.12.0; `TestRunner`, `TestRng` in R6.7), `tokio`, `tempfile`, `object_store` (its `memory::InMemory`), `futures`, `bytes`, `sqlx`. Random choices inside reader threads use a 10-line xorshift written in `concurrent.rs`, not the `rand` crate, which `llmobs-storage` does not depend on.
- **Test code**, except R6.4's fix and R6.8–R6.9's fixes. New modules are declared in `model_tests/mod.rs`; the `mod model_tests` line in `lib.rs` is M1's.
- **Threads, not tasks, drive the test.** Lane drivers, readers, the GC thread and the watchdog are `std::thread::scope` threads that call `Harness::block_on` (M1). `Runtime::block_on` takes `&self` and may be called from several non-runtime threads at once on a multi-thread runtime; each call runs its future on the calling thread, and the futures' own spawns (DataFusion partitions, `spawn_blocking`) go to the runtime's workers. Scoped threads borrow `&Harness`, so a restart or a crash (`&mut Harness`) can only happen between scopes: that is how readers are kept out of a life that is ending.
- **The oracle stays independent** (M1 § Conventions): `model.rs` doesn't call engine code. The concurrent mode adds no model logic, only *which* model states a read may match.
- **Generated data stays inside the spec's guarantees** (M1 § Conventions, D-R-3): no re-send of a class-2 trace, no retention at the anchor-dependent day `D + 2`. M6 changes neither the shapes nor the generators' limits. If a concurrent check fails, first rerun the same ops with M1's sequential `run_case`: a sequential failure is an M1 finding, not a race.
- **Panics.** Test code may `unwrap`/`expect`, with messages that say what was expected.
- **Read timeouts.** Every read request uses `timeout: Duration::from_secs(30)`, as in M1.
- **Logging and privacy.** Test output prints op indexes, lane numbers, read kinds, project numbers, trace/span numbers, and expected vs observed values: all synthetic. Fixes in `src/` follow `AGENTS.md` § Privacy: log ids and sizes, never payload or attribute values.
- **Report at the end of each sub-phase:**
  - what was done;
  - any deviation from this plan, and why;
  - how long `cargo test --workspace --all-features` and `cargo test --workspace --all-features model_tests` took in debug;
  - anything left undone or unverified.

### Names used throughout

- **Lane.** A pair of projects with its own `Model`, its own clock and its own op list. Lane `n` uses projects `P(2n)` and `P(2n + 1)`; lane 0 is M1's `P::A`, `P::B`. Only lane `n`'s ops write to its projects, so each project has one linear history even when lanes run at once.
- **Step.** One storage call that changes what a read can see: one `append_spans`/`append_scores` batch, one `register_deletion`, one `apply_retention`, one `drop_project`. A split `Append` is two steps. Ops that change nothing visible (`Compact`, `Gc`, `ApplyDeletions`, an op that calls nothing) are zero steps.
- **History.** Per lane, the model after each step: `S_0` is the empty model, `S_k` the model after `k` steps.
- **Published counter.** Per lane, `published: AtomicUsize`: the number of steps whose storage call has **returned** and whose `S_k` is in the history.
- **In-flight flag.** Per lane, `in_flight: AtomicBool`: set by the driver just before a step's storage call, cleared just after the step's `S_k` is published.
- **Observation.** One read by a reader thread: the lane, the read, `lo` (the published counter before the read's `storage.query`), the observed value, and `hi` (computed after the read returned, § The rule).
- **Background.** The flush tasks, the compaction task (compaction pass, deletion pass with its media collection, verify pass, every `compaction.interval`; `compaction/task.rs`), and the GC thread of R6.5.

---

## The rule: what a concurrent read may return

A read takes **one** `ReadState` (`Storage::query` → `Published::load`, spec 03 §4). Every step publishes once before it returns: `append` acknowledges after `HookPoint::AfterPublish` (the writer publishes under the memtable lock, `wal/writer.rs::publish`); `register_deletion`, `apply_retention` and `drop_project` publish inside their commit, under the committer mutex. So for its lane's projects, a read's snapshot is exactly one `S_j`, plus any amount of background work.

**Which `j`.** `j ∈ [lo, hi]`, where the reader computes, after the read returned and in this order:

```rust
let flying = lane.in_flight.load(SeqCst);
let done = lane.published.load(SeqCst);
let hi = done + usize::from(flying);
```

- `lo`: the driver stores `published = k` only after step `k`'s call returned, and the step published before returning, so a snapshot loaded after the reader saw `lo` includes at least `S_lo`.
- `hi`: the driver stores `published` before it clears `in_flight`, and sets `in_flight` before it starts a call. If the reader saw `in_flight == false`, no call had started since the last `published` store it can see, so the snapshot (taken earlier) is at most `S_done`. If it saw `true`, the call in flight may already have published: `S_done + 1`.

Every load and store of these two atomics uses `Ordering::SeqCst`.

**Background work changes no read** for a fixed `S_j`, with the exceptions M1's rules already handle:

| Background effect | Why the read is unchanged | Covered by |
|---|---|---|
| Flush | Memtable rows and the new files are swapped in one publish (`flush.rs::publish_flush`, under the committer mutex and the memtable lock). | exact |
| Compaction of distinct rows | Inputs and outputs are swapped in one commit. | exact |
| Compaction merging re-sent duplicates | Counts of an unsettled trace fall toward the expected value; `span_detail` keeps the newest copy, as the merge does. | M1's bounds for unsettled traces; `span_detail` exact |
| Deletion pass (rewrite, raise, mark applied) | Pending reads the same as applied (M1 R1.2); a file flushed before a registration is gated by its `deletions_applied_seq`. | exact |
| Media collection after a deletion | No checked read touches media. | not read-visible |
| Day close dropping `deleted` score versions | The live-score rule gives the same result with or without them. | exact |
| Verify pass | Runs only on suspect files; no file is corrupt in this test. | a quarantine during a run is a finding (R6.6 check 8) |
| Obsolete-file GC | Only files no snapshot references (spec 03 §5; R6.4 fixes the raise case). | exact; a failed read is a finding |
| Trace anchors | The anchor state is global and advances with every flush of every lane, so another lane's activity can expire a class-2 trace's anchor entry (`anchor.rs`). That only moves where a later span of a class-2 trace lands; reads merge partials across days. M6 inherits M1's limits: class-2 traces are never re-sent, and retention never uses `D + 2`. The settled physical check keeps `partial_count ≤ 2` for class 2. | exact |
| Retention, drop | Explicit steps, not background. | exact |

**Per read**, compare the observation with each `S_j`, `j ∈ [lo, hi]`, using M1's comparison. The observation passes when **any** `S_j` matches:

| Read (M1) | Compared against `S_j` |
|---|---|
| spans list | keys equal `visible_spans()` keys; each key's multiplicity in `[1, copies]` |
| trace list (unwindowed) | settled traces exact; unsettled ones: every field but counts, `total_tokens`, `cost_total`; `span_count` in `[expected, expected + extra copies]`; the order |
| windowed and no-eval trace lists | the trace id **set** always exact; rows as for the unwindowed list |
| trace detail `t` | `Some` iff `t` is expected; row as in the list; span ids as a set, duplicates bounded as for spans |
| span detail `(t, s)` | the newest visible copy's ingest, or `None` |
| scores, scores by id, latest scores | exact |
| `filter_trace_ids` | always exact: it is a set of trace ids, and duplicates don't change sets |
| sessions, users | the session ids, user ids and per-row `trace_count` always exact (a trace is one row of the traces view whatever its duplicates); `error_count`, `total_tokens`, `cost_total` only when no visible trace of the project is unsettled in `S_j` |
| metrics, `alert_value` | `Traces` and the counts always exact; `ErrorRate` always exact (duplicates never change whether a trace has `error_count > 0`); `CostTotal`, `TotalTokens`, cost and `ScoreAvg` values only when no visible trace of the project is unsettled in `S_j` |
| `deletion_status` | below |

The "always exact" parts are a strengthening of M1's R1.6 rule, which skipped the whole read while a trace was unsettled; R6.5 applies it in both modes.

`user_deletion_scope` and `eval_trace_ids` are not reader reads: they run inside the `DeleteUser`/`DeleteTraces` op (M1 R1.5), on the op's own lane, where no other step of those projects can be in flight. They keep M1's exact check against the lane's current model.

**`deletion_status`.** It reads `catalog.db`, not a snapshot. For an id the reader picks among the deletions registered in `S_lo`:
- the status is `Pending` or `Applied`, never `NotFound`;
- once any thread has seen `Applied`, a read that **started** after that thread's read returned never sees `Pending`. Each lane keeps `applied_seen: Mutex<HashSet<Uuid>>`. The reader copies the set before the call; `Pending` for an id in the copy fails the case; `Applied` inserts the id after the call.

**Pages.** Every page of one reader read comes from the same `QueryContext` (one `storage.query`, one snapshot); R6.3 makes sure of it. M1's `check_paging` (pages of 7 vs one page, two separate reads) is sequential only and never runs in a reader.

**Reads that may fail.** `QueryError::Busy` (admission, spec 03 §6) is not a mismatch: the reader counts it and moves on. Any other error fails the case; an object-not-found error in particular means GC deleted a file a snapshot still used.

---

## R6.1 — Lanes: projects beyond A and B

Commit: `storage: let the model test run on any pair of projects (R6.1)`

No behaviour change: every M1 test passes with the same default case count.

### `shape.rs`

Replace the enum with a newtype:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct P(pub(super) u8);

impl P {
    pub(super) const A: P = P(0);
    pub(super) const B: P = P(1);
    /// `PROJECT_A` for 0, `PROJECT_B` for 1, else
    /// `format!("018f6b4e-7c1a-7c1a-8c1a-{:012x}", 0xc00 + u64::from(self.0))`.
    pub(super) fn id(self) -> ProjectId;
    /// `self.0`: 0 for A, 1 for B, as before.
    pub(super) fn salt(self) -> u8;
}
```

- Every `match` on `P` becomes a comparison or uses the model's projects (below).
- Check every use of `salt()` for `salt ≤ 31` (16 lanes): the shape formulas (`k = t + salt`, `(s / 2 + salt) % 2`, `10 × s + salt`) are plain arithmetic; `score_id` packs the salt into a `u128` and must give it at least 8 bits. Widen it there if it has fewer.
- Unit test `p_ids_are_distinct_and_a_b_unchanged`: `P::A.id()` and `P::B.id()` equal `PROJECT_A`/`PROJECT_B` parsed; `P(0)..P(32)` give 32 distinct ids.

### `model.rs`

- Add `Model::for_pair(a: P, b: P) -> Model`; `Model::new()` returns `Model::for_pair(P::A, P::B)`.
- Add `Model::projects(&self) -> [P; 2]` returning `[self.a.project, self.b.project]`.
- `Model::project(p)` / `project_mut(p)` return `a` when `p == self.a.project`, `b` when `p == self.b.project`, and panic with `"project {p:?} is not in this lane"` otherwise.
- Derive `Clone` on `Model` and what it holds, if M1 did not.

### `ops.rs`

- `ops()` becomes `ops_for(a: P, b: P) -> impl Strategy<Value = Vec<Op>>`, which draws every `project` field from `{a, b}` with M1's weights (`a` 3, `b` 1). `ops()` is removed; `storage_matches_the_model` uses `ops_for(P::A, P::B)`.
- `Op::DropB` drops `model.b.project` and `Op::project()` returns `None` for it; the call sites below check `model.b.project` instead.

### `check.rs` and `mod.rs`: iterate the model's projects

- `check_full` loops over `model.projects()` instead of `[P::A, P::B]` (the merged code has the literal at the top of `check_full`). Do the same everywhere else a literal `[P::A, P::B]` or `P::A`/`P::B` names "both projects" (`check_settled_files`, `settle`'s "every deletion applied" check, `run_case`).
- `run_case`'s per-op dispatch: `Op::DropB => check_quick(&harness, &model, model.b.project, index)`; ops with no project check both of `model.projects()`.

### Test

`a_lane_one_case_runs`: `run_case` on lane 1 needs a model for `P(2)`, `P(3)`. Give `run_case` a `pair: (P, P)` parameter (`storage_matches_the_model` passes `(P::A, P::B)`), and run it on `(P(2), P(3))` with the ops of M1's `regression_compaction_restart_resend` mapped to `P(2)`, plus `DropB` and `Settle`. It passes. Update every existing caller of `run_case` (M1's `regression_*` tests and any R1.8 regression) to pass `(P::A, P::B)`.

### Commands

```bash
cargo test --workspace --all-features model_tests
```

---

## R6.2 — A storage-only apply with a step callback

Commit: `storage: split the model test's apply into steps (R6.2)`

No behaviour change.

### `ops.rs`

```rust
/// What `apply_storage` reports around each storage call that changes what a
/// read sees.
pub(super) enum StepEvent<'a> {
    /// The call is about to start.
    Begin,
    /// The call returned; the model is already updated for it.
    Done(&'a Model),
}

/// Every op except `Restart`, `Crash` and `Settle`, which need `&mut Harness`.
/// Ticks the clock first.
pub(super) fn apply_storage(
    op: &Op,
    h: &Harness,
    model: &mut Model,
    clock: &mut i64,
    step: &mut dyn FnMut(StepEvent<'_>),
);
```

- `apply_storage` on `Restart`, `Crash` or `Settle` is `unreachable!("{op:?} needs &mut Harness: use apply")`; its callers never pass them.
- **Only `apply_storage` ticks** for the ops it handles (`*clock += MICRO_NS`, first thing). M1's `apply` ticks only for `Restart`, `Crash` and `Settle`, then handles them; for every other op it calls `apply_storage(op, harness, model, clock, &mut |_| {})` without ticking. Each op ticks exactly once, as before.
- The model is updated **per call**. A split `Append`: `step(Begin)`, first batch, push the first half's copies, `step(Done(model))`, `step(Begin)`, second batch, push its copies, `step(Done(model))`. `Score`, `DeleteScore`, `DeleteTraces`, `DeleteUser`, `Retention`, `DropB`: `step(Begin)` just before the publishing call (for the deletion ops, after the resolution reads), `step(Done(model))` after it returned and the model was updated. `Compact`, `Gc`, `ApplyDeletions` and ops that call nothing never call `step`.
- **A call that is refused changes nothing, so it is not a step.** An append, score write or deletion op on a dropped project still calls storage and asserts the refusal (`Err(AppendError::ProjectDropped)`, as M1 does), but calls neither `Begin` nor `Done`: the model knows the project is dropped before the call (`ProjectModel::dropped`), so the op checks that first. Never call `Begin` without a matching `Done`; a `Begin` left unmatched would keep the lane's in-flight flag set (R6.5).

### Test

`apply_storage_reports_one_done_per_publishing_call`: a fresh `Harness::new()` and `Model::new()`; collect the events of `Append { A, 0, 4, true }` (Begin, Done, Begin, Done), `Append { A, 0, 4, false }` (Begin, Done), `Compact { hours: 1 }` (none), `DeleteTraces { A, [0] }` (Begin, Done). Each `Done` model has the copies of the batches before it. The clock rose by exactly 4 × `MICRO_NS` plus the compact's hour.

### Commands

```bash
cargo test --workspace --all-features model_tests
```

---

## R6.3 — Reads over `&Storage`, one snapshot each, comparisons that return

Commit: `storage: make the model test's reads comparable off the driver thread (R6.3)`

No behaviour change.

### `observe.rs`

Every observe function becomes an `async fn` whose first parameter is `storage: &Storage` instead of `harness: &Harness` (the page `limit` parameters stay), and returns `Result<_, ReadFailure>` instead of panicking on a read error:

```rust
#[derive(Debug)]
pub(super) enum ReadFailure {
    /// `QueryError::Busy` from `storage.query`.
    Busy,
    /// Any other error, as its `Display` text.
    Error(String),
}
```

Each function calls `storage.query` **once** and pages through `next_cursor` on that same `QueryContext` (the merged `context` helper builds one; check that no function calls it per page, and fix it if one does).

### `check.rs`

```rust
/// One read a check or a reader thread runs.
#[derive(Debug, Clone)]
pub(super) enum Read {
    Spans(P),
    Traces(P),
    Windowed(P),
    NoEval(P),
    TraceDetail(P, u8),
    SpanDetail(P, u8, u8),
    Scores(P),
    /// The score ids to ask for: every id the model had written when the
    /// read was chosen (M1 asks for all of them).
    ScoresById(P, Vec<String>),
    Latest(P),
    Sessions(P),
    Users(P),
    Metrics(P),
    FilterTraceIds(P),
    AlertValues(P),
}

/// What a read returned: one variant per `Read`, holding that observe
/// function's value.
#[derive(Debug, Clone)]
pub(super) enum Observed { /* … */ }

/// Runs one read on one snapshot, with one page of `MAX_PAGE`.
pub(super) async fn observe(storage: &Storage, read: &Read) -> Result<Observed, ReadFailure>;

/// Compares an observation with one model state. `Err` holds a printable
/// description of the first difference (expected and observed).
pub(super) fn compare(model: &Model, read: &Read, observed: &Observed) -> Result<(), String>;
```

- `compare` holds M1's rules unchanged in this sub-phase (R6.5 strengthens the R1.6 reads).
- `check_quick` and `check_full` keep their signatures and their reads; `check_paging` keeps its two calls per table. Each other read becomes:

  ```rust
  let observed = h.block_on(observe(h.storage(), &read)).expect("…");
  compare(model, &read, &observed).unwrap_or_else(|diff| panic!("op {index}, {read:?}: {diff}"));
  ```
- `deletion_status` stays in `check_full` as M1 wrote it.

`ScoresById` carries the ids it asks for: M1's check builds them from the model it checks (every id it ever wrote); a reader builds them from `S_lo`. `compare` against a state expects exactly that state's live scores **restricted to the requested ids**, so a score written after `S_lo` (not requested) never counts as missing. Add a helper `Model::score_ids(&self, p: P) -> Vec<String>` (every written id of `p`, live or not) for both callers.

### Commands

```bash
cargo test --workspace --all-features model_tests
```

Every M1 test passes, with the same counts as before.

---

## R6.4 — GC keeps files a snapshot from before a raise still reads (bug fix)

Commit: `storage: keep raised files a pre-raise snapshot reads out of GC (R6.4)`

This comes before the concurrent runner: its zero-grace GC thread would trip on this bug first.

### The bug (confirmed)

A deletion pass *raises* a clean file: `Catalog::apply` replaces its `FileMeta` with a new `Arc` (`catalog/mod.rs`, the `edit.raise` loop and `replace_active`: `files[position] = Arc::new(f(&files[position]))`). Snapshots taken before the raise hold the **old** `Arc`. When the file is later removed (compaction, retention), the commit path (`committer.rs`, after `transaction.commit()`) puts only the **new** `Arc` in `retired`, and `collect_obsolete_file` checks `Arc::strong_count` of that one. A reader on a pre-raise snapshot is not counted, so GC deletes the object under it once the grace has passed. In production the 5-minute grace hides it; spec 03 §5 says no `ReadState` of the process may still reference a collected file.

### Test

`gc.rs::tests::gc_keeps_a_file_a_snapshot_from_before_its_raise_reads` (it needs the private `committer` field, as the other tests there do):
1. Set up as `compaction/rewrite.rs::a_clean_candidate_is_raised_without_a_rewrite`: `config_in(dir, quiet_compaction(8), 1 h)`, `flush_cycle` with `span_row(2, 0, time, time + 1)`. `quiet_compaction`, `flush_cycle`, `config_in` and `at` are `pub(crate)` in `compaction::job::tests`; copy `traces_deletion` from `rewrite.rs`'s test module.
2. `let held = storage.query(project(), Default::default()).await.unwrap();` (the pre-raise snapshot).
3. Register `traces_deletion(&[1], at(11, 0, 0))` and run `apply_deletions(at(11, 0, 0))`: `(rewritten, raised, applied, failed) == (0, 1, 1, 0)`; the spans file keeps its id and has `deletions_applied_seq == 1`.
4. Remove it: `apply_retention(project(), <the day after the file's day>)`; the report counts it.
5. `storage.collect_obsolete(Duration::ZERO)`: `deleted == 0` and `in_use ≥ 1`.
6. Read through `held` (`SELECT count(*) FROM spans_t` over `held.spans`, as `flush.rs::a_flush_prepared_before_a_registration_keeps_its_rows` does): `Ok`, 1 row.
7. `drop(held)`; `collect_obsolete(Duration::ZERO)` deletes every obsolete file; the store has none of their objects left.

Run it before the fix: step 5 deletes the spans file. Then fix.

### Fix (`catalog/committer.rs`)

- Add to the committer's inner state, next to `retired`:
  ```rust
  /// Earlier versions of files a raise replaced, still held by older
  /// snapshots: GC treats the file as in use while any is alive (spec 03 §5).
  superseded: HashMap<FileId, Vec<Weak<FileMeta>>>,
  ```
  initialised empty where `retired` is.
- `catalog/mod.rs`: `Catalog::apply` returns the replaced versions too: `Result<(Self, Vec<Arc<FileMeta>>, Vec<Arc<FileMeta>>), CommitError>`, the third element being, for each raised file, the `Arc` that `replace_active` replaced (make `replace_active` return it as `Option<Arc<FileMeta>>` instead of `bool`). Update its callers.
- In the commit path, take `(id, Arc::downgrade(&old))` for each replaced version `apply` returned. Push them into `superseded` **only after `transaction.commit()` succeeded**, in the same place `removed` goes into `retired`. A rolled-back commit leaves `superseded` unchanged.
- After each successful commit, drop from `superseded` every entry whose `Weak`s all have `strong_count() == 0`, so the map is bounded by the live snapshots.
- In `collect_obsolete_file`, return `ObsoleteRemoval::InUse` also when any `superseded[&file.id]` entry has `Weak::strong_count(w) > 0`. Wherever `retired.remove(&file.id)` runs, also `superseded.remove(&file.id)`.

### Spec, changelog, findings

- `docs/spec/03-storage.md` §5 *GC of obsolete files*, after "(reference counts on `FileMeta`)": "A raise replaces a file's `FileMeta`; the count covers the versions it replaced too."
- `CHANGELOG.md` § `[Unreleased]` › `### Fixed` (create the heading if absent): "Storage GC no longer deletes a data file that a query started before a deletion pass still reads (the deletion's raise of a clean file hid the query's reference)."
- `docs/research/storage/hardening.md`: create § `## M6 — Concurrency` with `### Findings` and the row `M6-1`: the test name, the cause above, class storage, this commit.

### Commands

```bash
cargo test --workspace --all-features gc::tests
for i in $(seq 20); do cargo test --workspace --all-features gc::tests::gc_keeps_a_file_a_snapshot_from_before_its_raise_reads || break; done
```

---

## R6.5 — The concurrent runner and its proptest

Commit: `storage: run the model test with background tasks and concurrent readers (R6.5)`

### `ops.rs`: the concurrent mode

Add the mode to `apply_storage` (its first non-sequential caller is this sub-phase):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// M1: background tasks off; every pass must report `failed == 0`.
    Sequential,
    /// M6: background tasks run; jobs may lose a conflict, appends may meet backpressure.
    Concurrent,
}
```

`apply_storage` gets a `mode: Mode` parameter; M1's `apply` passes `Mode::Sequential`. **`Mode::Concurrent` differences, nothing else:**
- `Compact` and `ApplyDeletions` don't assert `report.failed == 0`: a background job, or another lane's retention or drop, can make a job lose a conflict (`CompactionReport::failed` counts "jobs that failed or lost a conflict"; `retention.rs::a_compaction_started_before_the_retention_fails_and_is_collected` shows one). The `Result` itself must still be `Ok`.
- An append that returns `Err(AppendError::Backpressure)` is retried after `std::thread::sleep(10 ms)`, up to 500 times, then the case fails with `"backpressure for 5 s"`. A refused append writes nothing (`lib.rs::tests::backpressure_writes_nothing`), so the model is unchanged until the retry succeeds. `step(Begin)` is called once, before the first attempt.

### `check.rs`: set-valued parts always exact

Change `compare` for the R1.6 reads as § The rule's table says: the trace id sets of the windowed and no-eval lists, `filter_trace_ids`, the session and user ids with their `trace_count`, metrics' `Traces` and `ErrorRate`, and `alert_value`'s counts and `ErrorRate` are compared exactly even when a trace is unsettled; the sums stay conditional. This applies to M1's sequential checks too (a stronger check, same spec rules).

### `harness.rs`: options and the concurrent config

```rust
pub(super) struct HarnessOptions {
    pub(super) config: fn(&std::path::Path) -> StorageConfig,
    /// Tokio worker threads per life.
    pub(super) workers: usize,
}

impl Harness {
    pub(super) fn with_options(options: HarnessOptions) -> Self;
}

/// `model_config` with every background task running often.
pub(super) fn concurrent_config(dir: &std::path::Path) -> StorageConfig;
```

`Harness::new()` is `with_options(HarnessOptions { config: model_config, workers: 2 })`; `open_life` takes the worker count. Add a test `harness_is_sync` calling `fn assert_sync<T: Sync>() {}` on `Harness`.

`concurrent_config`: `model_config(dir)` with (minimums from `StorageConfig::validate`, `config.rs`):

| Field | Value | Why |
|---|---|---|
| `flush.max_age` | `100 ms` | the minimum: flushes all the time |
| `compaction.interval` | `100 ms` | the minimum: a compaction, deletion and verify pass every 100 ms |
| `compaction.l0_files` | `2` | as M1: a merge as soon as two L0 files exist |
| `compaction.l0_age` | `1 s` | the minimum |
| `compaction.threads` | `2` | two jobs at once |
| `gc.grace` | `1 s` | the minimum; only the collection at open uses it |

Everything else as `model_config`, `close_delay` included.

The background passes run at the wall clock (`catalog::now_ns()`), while explicit ops pass the model clock (March 2026). Every model day is long past at wall-clock time, so background passes close every day they find; explicit passes find less. No read depends on when a day closes.

**GC is driven by a test thread** (decided). `gc::INTERVAL` is a 10-minute constant (`gc.rs`), so the GC task never ticks during a test, and no config key is added. The race worth testing is inside `collect_obsolete` (`Committer::collect_obsolete_file`, the reference-count check under the committer lock), which the GC thread calls through `Storage::collect_obsolete` with `Duration::ZERO`: with no grace, only the reference counts protect readers, which is spec 03 §5's rule. Staged-object collection is **not** run during a segment: a zero age would delete the outputs of a flush between stage and commit, which its 1-hour age prevents. `settle_concurrent` runs it once everything is quiet.

### `concurrent.rs` (new module)

```rust
//! Track R M6: the model test with background tasks running and reader
//! threads checking every read against the model states it may have seen
//! (docs/plans/r-hardening/06-m6-concurrency.md § The rule).

pub(super) struct ConcurrentOptions {
    /// Reader threads per segment.
    pub(super) readers: usize,
    /// Pause after each op of a lane, so background work overlaps the ops.
    pub(super) pace: Duration,
    /// Sleep between two collections of the GC thread.
    pub(super) gc_every: Duration,
    /// States kept per lane; older ones are dropped (stress runs).
    pub(super) history_keep: usize,
}

/// What a lane shares with the readers.
pub(super) struct Lane {
    log: Mutex<LaneLog>,
    grew: Condvar,
    published: AtomicUsize,
    in_flight: AtomicBool,
    /// Index of the op the driver is running, for the watchdog.
    op: AtomicUsize,
    applied_seen: Mutex<HashSet<Uuid>>,
}

struct LaneLog {
    /// `states[i]` is `S_{base + i}`.
    states: VecDeque<Arc<Model>>,
    base: usize,
    /// No step is in flight and none will come in this segment.
    idle: bool,
}

/// What a lane's driver owns.
pub(super) struct LaneState {
    pub(super) shared: Arc<Lane>,
    pub(super) model: Model,
    pub(super) clock: i64,
}

/// Counters printed at the end of a case or a stress run.
#[derive(Debug, Default)]
pub(super) struct Stats {
    pub(super) steps: AtomicU64,
    /// Matched reads per read kind (index: the reader's mix order, below).
    pub(super) reads: [AtomicU64; 15],
    pub(super) busy: AtomicU64,
    /// Reads whose `lo` was below the kept history (stress only).
    pub(super) stale: AtomicU64,
    /// `states` calls that hit the 1 s wait limit.
    pub(super) wait_limit: AtomicU64,
    /// Reads that matched a state other than `S_lo`.
    pub(super) matched_later: AtomicU64,
    pub(super) gc_deleted: AtomicU64,
    pub(super) gc_in_use: AtomicU64,
}
```

The lane's number is its index in the `lanes` slice; it is not stored. A step pushes `Arc::new(model.clone())`; a lane's model is two projects of at most 144 span keys each.

#### Lane methods

```rust
impl Lane {
    /// `states = [model]`, base 0, not idle.
    pub(super) fn new(model: &Model) -> Arc<Lane>;
    /// After a step's call returned and the model was updated.
    pub(super) fn push(&self, model: &Model, keep: usize);
    /// The states a read with this `lo`/`hi` may have seen; waits while `hi`
    /// is not in the history and the lane is not idle. `None` when `lo` is
    /// below `base`, or when `stop` was set while waiting.
    pub(super) fn states(&self, lo: usize, hi: usize, stop: &AtomicBool) -> Option<Vec<Arc<Model>>>;
    /// Notifies `grew`.
    pub(super) fn set_idle(&self, idle: bool);
}
```

- `push`: lock the log; push the clone; while `states.len() > keep`, pop the front and raise `base`; `published.store(base + states.len() - 1, SeqCst)`; then `in_flight.store(false, SeqCst)`; unlock; `grew.notify_all()`.
- The driver's step callback: `Begin` → `in_flight.store(true, SeqCst)`; `Done(m)` → `push(m, keep)` and `stats.steps += 1`.
- `states`: lock; `while !log.idle && log.base + log.states.len() <= hi`: `grew.wait_timeout(log, 100 ms)`; return `None` if `stop` is set, and stop waiting after 1 s in total. Then return `None` if `lo < log.base`, else the `Arc`s of `lo..=min(hi, base + len - 1)`. After the 1 s limit it returns `None` and the reader skips that read, counting a `wait_limit` hit in `Stats`: the step in flight has not returned, so its state is not known, and comparing against the other states could fail a correct read. Report the count; more than a handful means a step is slow, look at which. The driver never leaves `in_flight` set across a refused call (R6.2), so the limit is a safety net, not the normal path.

#### Readers

```rust
/// A 64-bit xorshift, seeded per reader so a run's read mix is reproducible.
struct XorShift(u64);
impl XorShift { fn next(&mut self) -> u64; fn below(&mut self, n: u64) -> u64; }
```

`fn reader(h: &Harness, lanes: &[Arc<Lane>], pairs: &[(P, P)], seed: u64, stop: &AtomicBool, stats: &Stats)` loops until `stop`:
1. Pick a lane, one of its projects (the first with weight 3, the second with weight 1, as the ops), and a read from this mix, in this order (the index of `Stats::reads`): `Spans 3`, `Traces 3`, `TraceDetail 2` (trace `below(12)`), `SpanDetail 2` (trace and span `below(12)`), `Scores 2`, `ScoresById 1`, `Latest 1`, `Windowed 1`, `NoEval 1`, `Sessions 1`, `Users 1`, `Metrics 1`, `FilterTraceIds 1`, `AlertValues 1`, `DeletionStatus 1`.
2. `let lo = lane.published.load(SeqCst)`; `S_lo` = `lane.states(lo, lo, stop)`; `None` → count `stale` (unless `stop`), continue. Build the read's inputs from `S_lo` (`ScoresById`'s ids: `s_lo.score_ids(p)`).
3. `h.block_on(observe(h.storage(), &read))`. `Err(Busy)`: count `busy`, continue. `Err(Error(text))`: panic with the lane, the read, `lo` and the text.
4. Compute `hi` (§ The rule); `lane.states(lo, hi, stop)`; `None` → count `stale`, continue.
5. Compare with each state in order. On the first `Ok`, count the read (and `matched_later` if it was not the first state). If none matches, panic with: lane, read, `lo`, `hi`, the observed value, and each state's `Err(diff)`.

`DeletionStatus` picks an id among the chosen project's deletions in `S_lo` (none: skip), calls `storage.deletion_status(id)`, and applies § The rule's check.

#### The GC thread

`fn gc_thread(h: &Harness, every: Duration, stop: &AtomicBool, stats: &Stats)`: until `stop`, `h.block_on(h.storage().collect_obsolete(Duration::ZERO))`, expected `Ok` (`"obsolete-file collection"`); add the report's `deleted` and `in_use` to the stats; `std::thread::sleep(every)`.

#### A segment, and how it never hangs

```rust
/// Runs every lane's ops at once, each lane on its own thread, with the
/// readers, the GC thread and a watchdog, until every lane is done. No op
/// may be `Restart`, `Crash` or `Settle`.
pub(super) fn run_segment(
    h: &Harness,
    lanes: &mut [LaneState],
    ops: &[&[Op]],
    options: &ConcurrentOptions,
    seed: u64,
    stats: &Stats,
);
```

One `stop: AtomicBool` per segment. Inside one `std::thread::scope`:
- set every lane not idle;
- spawn `options.readers` readers (seeds `seed + i`) and the GC thread;
- spawn one driver thread per lane. Its body runs inside `std::panic::catch_unwind(AssertUnwindSafe(|| …))`, and a drop guard created first sets the lane idle and, if the thread is panicking (`std::thread::panicking()`), sets `stop`. For each op `i`: stop early if `stop` is set (another thread failed); `lane.op.store(i, SeqCst)`; `apply_storage(op, h, &mut lane.model, &mut lane.clock, Mode::Concurrent, &mut callback)`; `std::thread::sleep(options.pace)`. On `Err(payload)` from `catch_unwind`: set `stop`, then `std::panic::resume_unwind(payload)`;
- readers and the GC thread use the same pattern: a guard that sets `stop` when they panic, so the drivers' readers and the others wind down;
- spawn a **watchdog**, armed until the scope ends (it watches the readers and the GC thread too, not only the drivers). It sleeps in 100 ms ticks until every other thread of the scope has finished. Each driver, reader and the GC thread record what they are doing in a per-thread `Mutex<String>` (the driver: lane, op index, `published`; a reader: lane and read; the GC thread: "collecting") and the time it started. If any thread's current item has run for more than 30 s, the watchdog writes every thread's record with `writeln!(std::io::stderr(), …)` (not `eprintln!`: libtest captures `eprintln!` output and drops it if the process aborts), sets `stop`, and waits up to 10 s for the threads. If they finish, it panics with the stuck thread's record. If not (a storage call is hung and cannot be interrupted), it writes `"watchdog: a storage call hung; aborting"` the same way and calls `std::process::abort()`, with `#[allow(clippy::exit)]` and a comment saying why: a hung test must fail, not block CI. The abort kills every test running in that libtest process, so the other tests of the run are reported as lost, not failed; the stderr lines are the evidence;
- drivers, readers and the GC thread check `stop` between items, so all of them wind down after a failure;
- the scope's own thread joins the driver handles (`join()` returns `Err` for a panicked one: set `stop`, then `resume_unwind` after the scope), then sets `stop`; the scope joins the rest when it ends.

#### The concurrent settle

```rust
/// M1's settle, safe with background tasks running, for every lane.
pub(super) fn settle_concurrent(h: &mut Harness, lanes: &mut [LaneState]);
```

It runs between segments (no reader or GC thread alive):
1. `h.restart()`.
2. `clock` = the max of the lanes' clocks, advanced as M1's settle step 2; set every lane's clock to it.
3. Up to 10 `apply_deletions(clock)` passes, until every deletion of every lane is `Applied`. A pass may report `failed > 0` (a background job planned before the restart cannot exist, but a background deletion pass of this life may hold a file the explicit one wants: it then loses or skips); keep looping. Fail after 10 passes with deletions still pending.
4. Up to 10 `compact(clock)` passes until `jobs == 0 && failed == 0`, tolerating `failed > 0` in between. Then poll `h.storage().compaction_claims() == 0` every 10 ms, failing after 30 s (a background job outlived the passes). Then one final `compact(clock)`, which is strict: `jobs == 0 && failed == 0`. Once a pass at `max(clock, D + 6) + close_delay` finds nothing, every day is closed and every partition merged, so a background pass at any `now` finds nothing either.
5. `collect_staged(Duration::ZERO)` (`Ok`), then `collect_obsolete(Duration::ZERO)` (`Ok`). Staged rows here come from jobs that lost a conflict and from flushes cut by a crash; spec 03 §5 leaves both to GC.
6. Per lane, collapse the model's duplicates as M1's settle step 6, and `push` the collapsed model.
7. `check_settled_files(h, &models)` with every lane's model. In this sub-phase change its signature from M1's `(h, model: &Model)` to `check_settled_files(h: &Harness, models: &[&Model])`: for each active file, find the model owning `file.project` (`models.iter().find(|m| m.projects().contains(&p))`, comparing ids); if none owns it, fail with `"file of project {id} belongs to no lane"`; run the per-file and per-project checks (coverage, `keep_from`, dropped, duplicates, `deleted` scores, the `traces` provider query) with the owner's `ProjectModel`; run the global `staged_paths` check once. M1's `settle` calls it with `&[model]`.

M1's `settle` stays as it is for M1.

#### The case

```rust
/// Lane 0 (A, B); its ops are cut into segments at `Restart`, `Crash` and
/// `Settle`, which run between segments, each followed by `check_full`.
pub(super) fn run_concurrent_case(ops: &[Op], options: &ConcurrentOptions);
```

1. `Harness::with_options(HarnessOptions { config: concurrent_config, workers: 4 })`; one `LaneState` (`Model::new()`, M1's start clock).
2. Walk the ops, collecting them into the current segment until a barrier op; run the segment; then the barrier: tick the clock, then `h.restart()`, `h.crash()` or `settle_concurrent`, then `check_full(&h, &mut lane.model, index)`.
3. After the last op: the last segment, `settle_concurrent`, `check_full`.
4. `eprintln!` the stats (shown on failure, or with `--nocapture`).

`check_full` between segments is M1's own, sequential; nothing of the test is in flight, but the new life's background tasks are running, which M1 never had. A failure there is a race too.

### The proptest (`concurrent.rs`)

```rust
fn concurrent_cases() -> u32 {
    std::env::var("LLMOBS_CONCURRENT_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(4)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: concurrent_cases(),
        // A failure may not reproduce on the smaller cases shrinking tries;
        // the first failure's message carries every detail.
        max_shrink_iters: 32,
        ..ProptestConfig::default()
    })]

    /// Every read made while ops and background tasks run equals the model
    /// in a state the read may have seen.
    #[test]
    fn concurrent_matches_the_model(ops in ops::ops_for(P::A, P::B)) {
        run_concurrent_case(&ops, &ConcurrentOptions {
            readers: 2,
            pace: Duration::from_millis(20),
            gc_every: Duration::from_millis(50),
            history_keep: usize::MAX,
        });
    }
}
```

The name does not contain `storage_matches_the_model`, so `just model`'s filter (`model_tests::storage_matches_the_model`) does not select it.

**Budget (D-R-14).** Measure in debug, the way `just test` runs it: `time cargo test --workspace --all-features` with `LLMOBS_CONCURRENT_CASES=2`, and the filtered `cargo test --workspace --all-features model_tests::concurrent`. Other tests run in parallel with it there, which slows it. Set the default to the largest of 4, 2 or 1 that keeps the concurrent test under 60 s in the workspace run, and report both times and the value.

**Seed files.** A failing concurrent case writes `crates/storage/proptest-regressions/model_tests/concurrent.txt`. Keep it only when the case fails again on at least 3 of 5 reruns (R6.9 § 3); otherwise move it to the scratchpad and name its path in the report, so `just check` doesn't become flaky.

### Tests (`concurrent.rs`)

1. `lane_states_wait_for_the_step_in_flight`: a `Lane` with `S_0`; a thread calls `states(0, 1, &stop)` and then sets an `AtomicBool returned`. The main thread sleeps 50 ms, asserts `returned` is still `false`, `push`es `S_1`, joins the thread: it returned 2 states. Then `set_idle(true)`; `states(1, 5, &stop)` returns 1 state at once.
2. `states_return_none_on_stop`: a thread waits in `states(0, 3, &stop)`; the main thread sets `stop`; the thread returns `None` within 1 s.
3. `history_keep_drops_old_states`: push 10 states with `keep = 4`; `states(2, 3, …)` is `None`, `states(7, 9, …)` has 3 states, `published` is 10.
4. `background_flush_runs_without_ops`: open `concurrent_config`; append spans 0..3 of trace 0 of `P::A` (`span(P::A, 0, s, clock)`); within 30 s (poll every 20 ms) the `Table::Spans` catalog files hold 3 rows and `published_rows(Table::Spans)` is 0.
5. `regression_concurrent_smoke`: `run_concurrent_case` on `Append { A, 0, 6, true }`, `Append { A, 1, 6, false }`, `Resend { A, 1, 3 }`, `Score { A, 0, 0 }`, `DeleteTraces { A, [1] }`, `Append { B, 0, 6, false }`, `Retention { B, 2 }`, `Restart`, `Append { A, 0, 6, false }`, `DropB`, `Crash`, `Settle`: every kind of step and barrier. It passes.

### Commands

```bash
cargo test --workspace --all-features model_tests
LLMOBS_CONCURRENT_CASES=50 cargo test --release --workspace --all-features model_tests::concurrent::concurrent_matches_the_model
```

Both pass. A failure in the 50-case run is a finding for R6.9: record the message and ops in the report; don't fix it here.

---

## R6.6 — End-state checks and the remote mode

Commit: `storage: check the end state and a remote store in the concurrent model test (R6.6)`

### Reading objects from either store

M1's `check_settled_files` reads files with `test_support::object_rows(h.dir(), &file)`, which reads `<dir>/objects/<path>` from disk and would panic on an in-memory store. Replace it (the same names as M4's plan, so both milestones share them):
- `test_support.rs`: add `pub(crate) fn parquet_rows(bytes: Bytes) -> RecordBatch` with the body of `object_rows` after its `fs::read`; `object_rows(dir, file)` becomes `parquet_rows(Bytes::from(fs::read(…).unwrap()))`.
- `harness.rs`: `pub(super) fn object_bytes(&self, file: &FileMeta) -> Bytes`: with no store, `fs::read(self.dir().join("objects").join(&file.path))`; with a store, `self.block_on(store.get(&object_store::path::Path::from(file.path.as_str())))` then `.bytes()`, both `expect`ed with the path.
- `check_settled_files` uses `parquet_rows(h.object_bytes(&file))`.

If M4 has merged first with these names, reuse them as they are.

### `harness.rs`: the remote store

`HarnessOptions` gets `pub(super) store: Option<Arc<dyn object_store::ObjectStore>>`, kept across lives; `open_life` calls `Storage::open_with_store(config.clone(), Arc::clone(store))` (`#[cfg(test)] pub(crate)` in `lib.rs`) when it is `Some`. Every existing `HarnessOptions` literal gets `store: None`.

`concurrent_remote_config(dir)`: `concurrent_config(dir)` with `query.read_cache_bytes = Some(ByteSize::kib(64))`, plus whatever `object_store` URL a remote store needs: copy what `lib.rs::tests::cached_config` sets. Model files are a few KiB and a cache block is up to 1 MiB (`read_cache::BLOCK`), so each file is one block and 64 KiB holds a handful: readers evict each other's blocks constantly.

`ConcurrentOptions` gets `pub(super) remote: bool`; `run_concurrent_case` builds the harness with `store: Some(Arc::new(object_store::memory::InMemory::new()))` and `concurrent_remote_config` when it is set.

### End-state checks (`check.rs`)

```rust
/// After `settle_concurrent`: nothing in flight, nothing leaked.
pub(super) fn check_quiet_state(h: &mut Harness, models: &[&Model]);
```

Called at the end of `settle_concurrent`, after `check_settled_files`. Each failure names what was found. Checks 3 and 5 poll every 10 ms for up to 5 s (a dropped `QueryContext`'s pool and a background task's last step may land just after the passes); the others are immediate.

1. **Memtables empty.** `storage.health().memtable_bytes == 0`; `published_rows(Table::Spans) == 0`; `published_rows(Table::Scores) == 0`.
2. **No claims.** `storage.compaction_claims() == 0`.
3. **No query memory held.** `storage.memory.used() == 0`. `Storage::memory` is a private field of the crate root and `SharedPool::used` is `pub(crate)`; `model_tests` descends from the crate root, so both are visible. Each `QueryPool` returns its bytes on drop; a value that stays non-zero is a leaked pool.
4. **No disk reservation.** Add to `disk.rs`:
   ```rust
   /// Bytes held by running compaction jobs.
   #[cfg(test)]
   pub(crate) fn reserved(&self) -> u64
   ```
   (locks `reserved`, returns it). Assert `storage.disk().reserved() == 0`.
5. **No staged rows.** `storage.staged_paths().await` is `Ok` and empty.
6. **No orphan objects.** The data objects in the store are exactly the active catalog files:
   - local store: walk `<data_dir>/objects/tables/` recursively; assert no name starts with `.` (temp files are `.…tmp-…`); map each file to its path relative to `<data_dir>/objects/`;
   - remote store: `store.list(Some(&object_store::path::Path::from("tables")))` collected with `futures::TryStreamExt::try_collect`, keys as strings;
   - compare, as sets, with the paths of `catalog_files(FileTable::Spans)`, `FileTable::Traces` and `FileTable::Scores`. `collect_obsolete(ZERO)` ran with no reader alive, so no obsolete object may remain.
7. **WAL fully flushed.** `h.restart()` once more; then `storage.replayed(Table::Spans) == 0` and `storage.replayed(Table::Scores) == 0` (`replayed` is a private `#[cfg(test)]` method of `Storage`, visible here). Each `wal/<table>/` directory has at most 2 segment files (names `crate::wal::parse_segment_name` accepts, as `lib.rs::tests::span_segments` counts them): the newest segment truncation keeps (spec 03 §1) and the one this life writes to. If there are more, read `recover_tables` (`lib.rs`) and `wal/writer.rs` to see whether open starts a new segment, and stop and report with the count and names.
8. **No quarantine.** `storage.verify_queue()` is empty, and `SELECT COUNT(*) FROM data_files WHERE state = 'quarantined'` on `catalog.db` (a read-only connection as `drop_tests.rs::connect` opens) is 0. Nothing is corrupt in this test, so a quarantine means a read or the verify pass misjudged a healthy file: a finding.

`check_quiet_state` takes `&mut Harness` for the restart in 7.

### Tests

- The proptest `concurrent_matches_the_model_remote`: the same as `concurrent_matches_the_model` with `remote: true`. Both share `concurrent_cases()`; re-measure the budget (R6.5) with both and lower the default if needed (report it).
- `regression_concurrent_smoke_remote`: R6.5 test 5 with `remote: true`.
- `object_bytes_reads_both_stores`: append and settle one trace on a local and on a remote harness; `parquet_rows(h.object_bytes(&file))` returns the same row count as the file's `rows` for each.

### Commands

```bash
cargo test --workspace --all-features model_tests
```

All pass.

---

## R6.7 — Stress: many lanes, readers and workers, for minutes

Commit: `storage: add the concurrent stress test and just stress (R6.7)`

### `shape.rs`

Add `P::pair(lane: u8) -> (P, P)` returning `(P(2 * lane), P(2 * lane + 1))` (first used here).

### `model_tests/stress.rs` (new module)

```rust
/// Several lanes write at once while readers check every read. Release
/// mode, minutes long: `just stress [secs]`.
#[test]
#[ignore = "minutes long; run with just stress"]
fn concurrent_lanes_match_the_model();
```

Parameters, from the environment, each printed at the start with `eprintln!`:

| Variable | Default | Meaning |
|---|---|---|
| `LLMOBS_STRESS_SECS` | 300 | no new round starts after this many seconds |
| `LLMOBS_STRESS_LANES` | 6 | lanes (projects `P(0)…P(2 × lanes − 1)`), at most 16 |
| `LLMOBS_STRESS_READERS` | 8 | reader threads |
| `LLMOBS_STRESS_WORKERS` | 8 | tokio worker threads per life |
| `LLMOBS_STRESS_REMOTE` | `0` | `1` runs on the in-memory store with the 64 KiB read cache |
| `LLMOBS_STRESS_SEED` | wall-clock nanoseconds | seeds the op lists and the readers' read mix; always printed. The same seed repeats the op lists and read choices, not the thread interleavings |

**Ops per lane.** Each lane has its own `TestRunner::new_with_rng(Config::default(), TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes(seed, lane)))` (`proptest::test_runner`); `seed_bytes` is the 8 little-endian bytes of `seed ^ (u64::from(lane) * 0x9E37_79B9_7F4A_7C15)` repeated 4 times (32 bytes). Each round draws one op list with `ops::ops_for(a, b).new_tree(&mut runner).expect("op tree").current()` (`proptest::strategy::{Strategy, ValueTree}`) and removes `Restart`, `Crash` and `Settle`. Lists have 1..=24 ops, so rounds are short and many.

**Epochs.** A lane saturates quickly: each trace has at most 12 spans and `next_span` never goes back, and one `Retention` at `D + 4` empties a project for good. So the run is cut into **epochs** of 5 rounds. Each epoch starts from scratch: a new `TempDir` and `Harness` (`HarnessOptions { config: concurrent_config or concurrent_remote_config, workers, store: a new InMemory when remote }`), new lanes, new `Model`s (`Model::for_pair(a, b)` with `(a, b) = P::pair(lane)`) and M1's start clock. The op runners are **not** reset, so each epoch draws new op lists. One `Stats` value is kept across epochs. Every progress line prints the epoch, the round and the seed.

**Rounds.** Until `LLMOBS_STRESS_SECS` have passed, epoch after epoch:
1. `run_segment` with every lane's list, `readers` readers, `pace: Duration::ZERO`, `gc_every: 20 ms`, `history_keep: 512`.
2. The barrier, by round number `r` within the epoch (from 1): `r == 5` → `settle_concurrent`, which ends the epoch; else `r == 4` → `h.crash()`; else `r == 2` → `h.restart()`; else none. After any barrier, `check_full` per lane. An epoch that runs out of time ends with `settle_concurrent` too.
3. After each epoch, `eprintln!` the epoch, the elapsed time and the stats, then drop the harness (its `Drop` shuts the life down) before the next epoch opens its own.

A lane whose B was dropped keeps writing to A until the epoch ends (B answers `ProjectDropped`); `drop_project`'s write lock on `append_gate` then races every other lane's appends.

**The end.** Every epoch already ended with `settle_concurrent` (with `check_settled_files` and `check_quiet_state`) and `check_full` per lane. After the last one:
- on runs of at least 60 s: every entry of `stats.reads` is `> 0` (a read kind that never ran means the mix is broken);
- `stats.stale * 100 <= total reads` (at most 1 % fell out of the kept history; otherwise raise `history_keep` and report);
- `stats.busy * 100 <= total reads` (admission rarely refused; otherwise report the query memory figures);
- on runs of at least 60 s: `stats.gc_deleted > 0` (GC ran against live readers) and `stats.matched_later > 0` (reads overlapped steps). Neither proves anything alone; zero means the test did not do what it claims.

Print the final stats, the seed and the duration.

### `justfile`

After `model`:

```just
# The concurrent model stress test (track R M6), release mode. `secs` is the
# run time; LLMOBS_STRESS_* variables tune it (docs/plans/r-hardening/06-m6-concurrency.md).
stress secs="300":
    LLMOBS_STRESS_SECS={{secs}} cargo test --release --workspace --all-features -- --ignored --exact --nocapture model_tests::stress::concurrent_lanes_match_the_model
```

`just test` runs only the `#[ignore]`d crash tests it names with `--exact`, so the stress test never runs there.

### `AGENTS.md`

- § Commands, after `just eval-live`: `` | `just stress [secs]` | the storage's concurrent model stress test, release mode (track R M6) | ``.
- § Rules › **Tests**, after the `LLMOBS_CRASH_ITERATIONS` line: "`LLMOBS_CONCURRENT_CASES=N just test` runs more cases of the storage's concurrent model test (default 4 or less, set by measurement). `just stress [secs]` runs its stress test; `LLMOBS_STRESS_LANES`, `_READERS`, `_WORKERS`, `_REMOTE` and `_SEED` tune it, and the seed is printed so a run's op lists can be repeated (not its interleavings)."

### Nightly (decided)

The nightly workflow of M3 (`.github/workflows/nightly.yml`, outside `ci-ok`) runs `just stress 1800` and `LLMOBS_STRESS_REMOTE=1 just stress 600`, and opens an issue on failure like its other jobs, with the printed seed in the log.
- If `nightly.yml` exists when R6.7 lands: add the two steps there as `run:` steps calling the recipes (`AGENTS.md` § CI), with a job timeout of 60 minutes; run `just check-workflows`.
- If it doesn't: write in the report and in `hardening.md` § M6 "nightly stress steps pending M3"; M3's plan carries the job.

### Commands

```bash
just stress 60
LLMOBS_STRESS_REMOTE=1 just stress 60
```

Both pass. Report their final stats lines. A failure is a finding for R6.9.

---

## R6.8 — Targeted race tests

One commit per group below, each with its fix when the test fails (D-R-2): `storage: <test or fix in imperative> (R6.8)`. A fix also adds a `CHANGELOG.md` `### Fixed` line and a `hardening.md` § M6 finding.

### What exists already

Read these first so nothing is duplicated; they pin the races in their names with hook points:
- writer and snapshot: `lib.rs::tests::a_query_during_append_stays_on_its_snapshot`, `query/spans.rs::a_flush_never_shows_a_row_twice`, `query/traces.rs::a_query_during_a_flush_sees_each_trace_once`;
- compaction: `compaction/job.rs::{a_job_that_loses_its_inputs_fails_cleanly, two_passes_share_one_job, a_query_pins_the_files_it_reads, a_flush_during_a_job_commits_alongside_it}`;
- deletion: `flush.rs::a_flush_prepared_before_a_registration_keeps_its_rows`, `compaction/rewrite.rs::{a_compaction_started_before_a_deletion_is_rewritten_afterwards, a_deletion_pass_leaves_claimed_files_alone, a_flush_in_flight_does_not_make_a_deletion_applied}`;
- retention: `retention.rs::{a_flush_prepared_before_the_retention_conflicts_and_drops_the_partition, a_compaction_started_before_the_retention_fails_and_is_collected}`;
- drop: `drop_tests.rs::{the_drop_waits_for_an_append_in_flight, a_flush_prepared_before_the_drop_conflicts, a_media_put_racing_the_drop_never_lands_its_row}`;
- GC: `gc.rs::tests::{collection_during_a_flush_keeps_the_fresh_staged_path, an_older_catalog_keeps_its_files}`, and R6.4's test.

Helpers to reuse: `compaction::job::tests::{quiet_compaction, flush_cycle, wait_at}` are `pub(crate)`.

### 1. Compaction and deletion rewrite racing a drop or a retention

- `drop_tests.rs::a_compaction_started_before_the_drop_fails_and_is_collected`: the drop counterpart of `retention.rs::a_compaction_started_before_the_retention_fails_and_is_collected`. Two flushed L0 files of the project to drop, `quiet_compaction(2)`; hold `HookPoint::CompactionAfterWrite`; spawn `compact`; `wait_at` the hook; `drop_project`; release. The pass reports `(jobs, failed) == (0, 1)`; no catalog file of the dropped project remains; `collect_staged(ZERO)` leaves `staged_paths()` empty. Then the same setup with a second project's two files flushed too, and only that project's rows checked: they read back unchanged.
- `retention.rs::a_rewrite_started_before_the_retention_fails_and_the_deletion_applies`: a flushed file with a covered row and a registered deletion; hold `CompactionAfterWrite`; spawn `apply_deletions`; `wait_at`; `apply_retention` past the file's day; release. The pass reports `failed == 1`; a second `apply_deletions` marks the deletion applied (its candidate files are gone); `deletion_status` is `Applied`; `collect_staged(ZERO)` leaves no staged path. If the second pass leaves it pending, stop and report: spec 03 §7 applies a deletion with no file left to rewrite.

### 2. The read cache under concurrent reads, writes and evictions

`read_cache.rs::tests::concurrent_use_never_returns_wrong_bytes`, a plain `#[test]` (no runtime: `ReadCache`'s methods are blocking). Build the cache with the module's own `fixture(dir.path(), capacity, 0)` (`capacity = 8 × 4,100` bytes: 8 blocks of 4 KiB plus their 4-byte CRC; `min_free = 0`, so `writable()` is always true) and use `fixture.cache`. In `std::thread::scope`, 8 threads, 2,000 iterations each, over 32 objects `"obj-{n}"`, each 4,096 bytes that are a pure function of `n` (byte `i` = `(n * 31 + i) as u8`). Each iteration picks (xorshift seeded per thread) one of: `insert_bytes(name, &bytes)`; `read_blocks(name, &[0])`, asserting any returned block equals the object's bytes; `evict_file(name)`; `shed(4_100)`. After the scope:
- `stats().bytes <= capacity`;
- every block file on disk (walk `fixture.cache_dir` recursively: blocks sit in one directory per object; skip `size` files and temp names) is in the index: the sum of their sizes is `<= stats().bytes`. Both sides count the 4-byte CRC: `write_blocks` indexes `len_u64(file.len())` where `file` is the data plus its CRC, and the file on disk is the same bytes. Confirm that in `read_cache.rs` before relying on it.

The reverse (every indexed block has a file) can be false and is not asserted: an eviction drops the index entry under the lock and deletes the file after it, so a writer can re-index the block in between and see its file deleted. A later read finds the file missing, drops the entry and fetches again. Note it in `hardening.md` § M6 as known, benign behaviour.

### 3. N's B2 (`flush_now`) and B3 (catalog snapshot, `object_gc_gate`), when merged

At the start of R6.8, run `grep -n "fn flush_now\|fn snapshot_catalog\|object_gc_gate" crates/storage/src/lib.rs crates/storage/src/catalog/committer.rs`:
- **Neither merged:** skip this group. Write "B2/B3 race tests pending" in the report and in `hardening.md` § M6. The coordinator schedules them as an R follow-up sub-phase with this section as its brief, once B2/B3 merge.
- **One merged:** write that one's tests only, and note the other as pending.

B3's plan already tests a held snapshot against obsolete-file GC (`snapshot_holds_obsolete_gc`), against `drop_project` (`snapshot_defers_dropped_project_media`) and against appends and flushes. These cover what it leaves open, in `snapshot.rs`'s test module unless a test needs `lib.rs` privates:

- `the_gate_holds_media_collection` (B3): a media blob of a project, unreferenced (no span names it); take a snapshot; spawn `storage.collect_media(now)` on another task; after 200 ms the task is pending and the blob's object exists; drop the snapshot; the task finishes and the object is gone. Never collect on the task that holds the snapshot: it deadlocks (B3 § Lock order).
- `the_gate_holds_a_quarantined_file` (B3): quarantine doesn't go through `retired` (`catalog/mod.rs::Catalog::apply`, the comment on `edit.quarantine`); only the gate protects it during a backup. Corrupt one flushed file's bytes, queue it as `quarantine_tests.rs` does, take a snapshot, `storage.verify_pass()` (it quarantines), then spawn `collect_obsolete(ZERO)`; after 200 ms every key of the snapshot's `objects` still exists. Read `quarantine_tests.rs` first: if a quarantined object moves, assert on the key the snapshot lists, and report what you found.
- `a_snapshot_matches_its_copy_under_concurrent_commits` (B3): flush and compaction every 100 ms with `l0_files` 2; a task appends a batch every 5 ms. On the test task, take 20 snapshots 50 ms apart, each to a new path. For each: open the copy read-only and assert `SELECT path FROM data_files WHERE state = 'active'` equals, as a set, the snapshot's `objects` keys under `tables/`; every key exists while the snapshot is alive; the copy's `flushed_seq` is at most the committer's current one. Drop it before the next. Stop the appender; `shutdown`.
- `drop_project_during_a_snapshot_returns_and_finishes_later` (B3): with a snapshot held and media stored for the project, `drop_project` returns `Ok` within 1 s; `append_spans` for it answers `ProjectDropped`; the media object still exists; after the snapshot is dropped, `storage.media.finish_drops().await` (the GC task's entry point; the private field is visible from `lib.rs`'s test module) deletes it.
- `flush_now_covers_every_acknowledged_append` (B2): `flush.max_age = 1 h` (only `flush_now` flushes); a task appends one batch every 2 ms and records each receipt's `seq` once `append_spans` returned. On the test task, 20 times: take the greatest `seq` recorded so far, `flush_now(5 s)` → `Ok`, then `storage.flushed_seq(Table::Spans) >= that seq`. Stop the appender; `shutdown`.
- `flush_now_racing_shutdown_fails_cleanly` (B2): `flush_now(5 s)` spawned while `shutdown` runs returns `Ok` or `FlushNowError::ShuttingDown` within its timeout, and `shutdown` returns `Ok`.

If B2 has merged, the M1 model has a `Flush` op (`00-design.md` § Other tracks). In `Mode::Concurrent` it is a zero-step op like `Compact`: check that `apply_storage` treats it so, and add `Flush` to R6.5's smoke case.

### Commands

```bash
cargo test --workspace --all-features drop_tests retention read_cache::tests::concurrent_use
cargo test --workspace --all-features snapshot flush_now     # when B2/B3 merged
```

Each new test passes (after its fix). Run each new hook-based test 20 times in a loop (`for i in $(seq 20); do cargo test … || break; done`) to show it is not flaky; report the count.

---

## R6.9 — Long runs and fixes

Commit per bug: `storage: <fix in imperative> (R6.9)`. A final docs commit: `docs: record the M6 concurrency runs (R6.9)`.

1. **Runs**, each in the background with its output in a log file in the scratchpad (a foreground command times out after 10 minutes):
   - `LLMOBS_CONCURRENT_CASES=1000 cargo test --release --workspace --all-features model_tests::concurrent` (the local and remote proptests);
   - `just stress 3600`, three times, each with a fresh `LLMOBS_STRESS_SEED` (printed);
   - `LLMOBS_STRESS_REMOTE=1 just stress 3600`, once;
   - `LLMOBS_STRESS_WORKERS=2 LLMOBS_STRESS_READERS=16 just stress 1800`, once: few workers and many readers make tasks queue behind each other, which changes the interleavings.

   Record the machine, command, seed, duration and final stats line of each in `hardening.md` § M6 § Runs. These are the local counterparts of the nightly jobs (R6.7), which run `just stress 1800` and a 600 s remote run every night from then on.
2. **For each failure:**
   1. Save the failure message (lane, read, `lo`/`hi`, observed value, each state's difference) and, for the proptest, the op list.
   2. **Rerun.** Proptest: a named test `regression_<what>` calling `run_concurrent_case(&[...])`, run 5 times. Stress: the same seed and duration, 3 times.
   3. **Rerun sequentially.** Feed the proptest's ops to M1's `run_case`. A sequential failure is an M1 finding: handle it with M1's R1.8 procedure.
   4. **Classify** as M1's R1.8 does (generator outside a known limit, model wrong, storage wrong), plus two M6 classes:
      - **The rule is wrong:** an effect § The rule says changes no read does change one. Check spec 03 before blaming storage. If the spec allows the effect (a documented limit), extend § The rule's table here and the comparison in the code, citing the spec line.
      - **Old life after a crash:** an unexplained duplicate or extra row right after a `Crash` barrier may be a commit (or a `spawn_blocking` write) of the old life landing after the new life's WAL replay, if a blocking task outlived the harness's `shutdown_timeout`. Check the timing in the log before blaming the merge.
      - **Storage race:** find the cause in `crates/storage/src`, then write a deterministic test that holds a background task at the hook point the failure suggests (R6.4 and R6.8 show the pattern). With that test failing, fix as M1's R1.8 does: a local fix in the same commit, with a `CHANGELOG.md` `### Fixed` line; stop and report for a format, public API or spec change.
   5. Add a row to `hardening.md` § M6 § Findings: id `M6-<n>`, the test or seed, the cause, the class, the fix commit.
3. **Non-reproducible failures: the shuttle trigger (D-R-10).** A failure looks like this: a read matches **none** of `S_lo … S_hi`, the same ops pass sequentially, and the reruns of 2.2 pass. Its message shows which states were compared and how each differed: one flush's or one compaction's worth of rows duplicated or missing points at a publish that was not atomic; a read error naming a missing object points at GC; a `Pending` after an `Applied` points at the deletion status path; a watchdog abort points at a deadlock (its output names the lane and op). If no hook-based test reproduces it after a focused attempt of a working day:
   - record it as `M6-<n> (not reproduced)` with the full message, seed and stats;
   - don't keep its proptest seed file in the tree (a seed that fails one run in ten makes `just check` flaky);
   - stop and report to the coordinator: the component the failure points at, and the proposal to model that one component under `shuttle` (a randomized scheduler that replays a failing schedule from its seed), with only that component's `std::sync` and tokio primitives swapped for shuttle's in a test-only build. That adds a dev-dependency and a seam: a new decision, recorded by the coordinator before any code.
4. **When the runs of 1 pass**, record them and update the roadmap row (this docs commit): "M6 done".

### Docs

- **`docs/research/storage/hardening.md`** § M6 (created by R6.4) gets:
  - what the concurrent mode and the stress test check, in one list (§ The rule in short, and R6.6's end-state checks);
  - the race review: the existing hook-based tests (R6.8 § What exists already), the new ones, and the read-cache behaviour of R6.8 § 2;
  - `### Runs`, and `### Findings` with R6.4's `M6-1` first.
- **`docs/spec/03-storage.md` § 10 Failure handling**: after M1's sentence about the model test, add: "Its concurrent mode and a release-mode stress test (`just stress`) run the background tasks and concurrent readers, and check each read against the model states it may have seen (track R M6)."

---

## What not to build in M6

- **loom or shuttle** (D-R-10). R6.9 § 3 says when to propose shuttle for one component; nothing is added here.
- **A configurable GC interval** (decided). The GC thread of R6.5 runs the same collection; `gc::INTERVAL` stays a constant.
- **Staged-object collection during a segment.** Its 1-hour age is what protects in-flight flushes; collecting at age zero mid-run would test a misuse.
- **Writes to one project from several lanes.** One writer lane per project gives each project a linear history; several writers would need a linearizability checker over interleavings, which this oracle is not.
- **More reads in the readers.** Media, `trace_spans`, `trace_refresh`, `session_detail`, `user_detail` and the SQL console stay out, as in M1.
- **Lossy crashes under concurrency** (M2), **S3 faults under concurrency** (M4; its `ChaosStore` can reuse `HarnessOptions::store` and `object_bytes`), **resource limits** (M5), **soak** (M7).
- **The nightly workflow itself.** M3 owns it; R6.7 only adds the two stress steps when the file exists.
