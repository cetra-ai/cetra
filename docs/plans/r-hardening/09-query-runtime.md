# R · Query runtime — DataFusion work off the ingest runtime (M7-5)

The soak `20261125-vm-q4-12h` (`docs/research/storage/soak.md`) found M7-5 (`docs/research/storage/hardening.md`): queries delay ingest as the retained data grows. The append p50 stayed at 11–12 ms, but the append p99 grew from 38 to 307 ms and the ingest ticks' lateness p99 from 25 to 453 ms. The cause is that the server has one Tokio runtime (`crates/cli/src/main.rs`, `main`), and DataFusion runs every query's and every compaction job's plan on it. A query poll that decodes a large Parquet batch holds a worker for tens to hundreds of milliseconds, and the ingest tasks (OTLP handlers, WAL writers, flushes) wait behind it. This plan gives storage a second runtime, the **query runtime**, that runs all DataFusion execution: queries and compaction jobs. On Linux its threads run at nice + 10. Decision D-R-39.

Implementer: the `glm-5-3` agent (Haiku when GLM has no credit). Four phases, run in order, one commit each.

**Prerequisites.** #248 (merged: the soak's clock pacing and `appends` samples) and #257 (the M7-5 write-up). The plan's branch, `claude/r-query-runtime`, is cut from #257's branch `claude/r-soak-q4-writeup`, so `hardening.md` already has M7-5 and `roadmap.md` mentions it.

**Read first.**
- `docs/research/storage/hardening.md` § M7, M7-5.
- `docs/spec/03-storage.md` § 6 (`Storage::query`, `ProjectQuery`, Resource limits, Parallelism), § 5 (compaction).
- `docs/spec/05-query.md` § the session config list (item 6: timeout and cancellation).
- `crates/storage/src/views/mod.rs`: `ProjectQuery`, `QueryContext::new`, `QueryContext::fail`.
- `crates/storage/src/lib.rs`: `Storage` (fields), `Storage::open` (the compactor's construction, around `compaction::job::Compactor::new`), `Storage::query`, `project_query`, `impl Drop for Storage`.
- `crates/storage/src/compaction/job.rs`: `Compactor::new`, `Compactor::pass`.
- `crates/storage/src/config.rs`: `QueryConfig`, `StorageConfig::validate`, `default_target_partitions`.
- `crates/storage/src/hooks.rs`: the test `Hooks` (`mod fault`) and `Hooks::pause`.

## What it is, and what it is not

**When it is done:**
- `Storage::open` builds a multi-thread Tokio runtime of `storage.query.threads` workers (default: the CPU count), named `cetra-query`. On Linux every thread of it lowers its own priority by 10 nice levels at start.
- Every DataFusion execution in `crates/storage/src` outside tests runs on it: each `DataFrame::collect` and `DataFrame::execute_stream` of the read and SQL paths goes through `QueryContext::collect` / `QueryContext::execute_stream`, and each compaction job is spawned on it.
- Planning (building the `DataFrame`, `ctx.sql`, view registration) stays on the caller's runtime; it is cheap.
- Dropping the caller's future or stream cancels the work on the query runtime, as before (spec 05: "dropping the stream cancels it").
- A panic in the work reaches the caller as before (`std::panic::resume_unwind`).

**Not in this plan:**
- Object-store I/O stays on whichever runtime polls it (the query runtime for queries). Routing S3 I/O back to the main runtime, as InfluxDB's `DedicatedExecutor` does, is left until a measurement asks for it.
- No hard CPU pinning, no `SCHED_IDLE`. No change to `target_partitions`, batch sizes or the memory pools.
- No server or CLI change: the server keeps its one runtime; storage owns the second.
- Parquet decoding outside DataFusion stays on the caller's runtime: the anchor rebuild (`anchor.rs`), the verify pass's checks (`verify.rs`, `compaction/verify_pass.rs` apart from its traces-file rebuild, which P3 moves) and `compaction/slice.rs` `time_values`. The verification soak may still show tail latency from them; they move only if it does.
- A dropped query is aborted, but the abort lands at the task's next poll, so its memory reservation and spill files are released a moment later than when the caller polled it itself. Accepted. This was already partly true before: DataFusion's own `spawn_buffered` tasks (a sort's inputs, `datafusion-physical-plan` 55.2 `sorts/sort.rs`) release their reservations a few milliseconds after the stream is dropped, so `sql/run.rs` `sql_json`, which drops its stream and then reserves the JSON encoding from the same pool, could already see the memory briefly held. A `close()` that awaits the bridged task was tried after the code review and did not make the release synchronous (the residual came from those inner tasks), so it was not kept. Likewise, compaction jobs hold the runtime through the `Compactor`, so after a `Storage` is dropped an aborted job can live until its next poll (and its `spawn_blocking` work to its end). Accepted; the P3 report says whether a test that reopens a data directory right away flaked.
- The 12 h soak that verifies the fix is run by the coordinating session after this plan, on the VM.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-R-39 (new, P4 writes it) | Storage runs all DataFusion execution (queries and compaction jobs) on a runtime of its own, `cetra-query`, of `storage.query.threads` workers (default the CPU count, 1–1024), at nice + 10 on Linux. The main runtime keeps ingest, HTTP, the WAL writers, flushes and timers. |
| D56 | mimalloc is the binary's allocator (unchanged). |
| D91 | `QueryContext::new` builds the session from the `ProjectQuery` `Storage::query` returns. |

**The user's answers (2026-11-25):**
- Compaction's DataFusion work moves to the new runtime too, not only queries.
- Default thread count: all cores (`std::thread::available_parallelism`), like the main runtime; the OS shares the cores.
- On Linux the query threads run at nice + 10, so ingest wins when cores are busy; no effect elsewhere.

## Versions

No new crate. Two new features on an existing pin:
- `rustix` 1.1.6 (workspace pin, `Cargo.toml` `[workspace.dependencies]`, features `std`, `fs`). `crates/storage/Cargo.toml` `[dependencies]` changes `rustix = { workspace = true }` to `rustix = { workspace = true, features = ["process", "thread"] }`: `process` for `rustix::process::{getpriority_process, setpriority_process}` (`rustix-1.1.6/src/process/priority.rs`), `thread` for `rustix::thread::gettid` (`src/thread/id.rs`). Re-check with `cargo info rustix@1.1.6` and report the version used.
- `tokio` 1.54 (workspace pin) already has `runtime::Builder::{thread_name, on_thread_start, worker_threads, thread_stack_size}`, `Runtime::shutdown_background` and `task::JoinSet::spawn_on` (`tokio-1.54.1/src/task/join_set.rs:161`).

## Phases and dependencies

```
P1 query_runtime.rs, config  ──►  P2 reads and SQL through QueryContext  ──►  P3 compaction on the query runtime  ──►  P4 docs
```

Sequential: P2 and P3 both edit `lib.rs`, and P4 documents what P1–P3 built.

| Phase | Files |
|---|---|
| P1 | `crates/storage/src/query_runtime.rs` (new), `crates/storage/src/lib.rs`, `crates/storage/src/config.rs`, `crates/storage/Cargo.toml`, `Cargo.lock`, `crates/storage/src/model_tests/harness.rs`, `crates/storage/src/crash_tests.rs`, `crates/storage/src/model_tests/resources/memory.rs` |
| P2 | `crates/storage/src/views/{mod,hex,sessions}.rs`, `crates/storage/src/lib.rs`, `crates/storage/src/media.rs`, `crates/storage/src/test_support.rs`, `crates/storage/src/reads/{deletions,experiments,scores,sessions,targets,traces,users,widgets,alerts,facets,prompts}.rs`, `crates/storage/src/sql/run.rs`, `crates/storage/src/fuzz_entry.rs`, `crates/storage/src/filter/mod.rs`, `crates/storage/src/query/traces.rs` |
| P3 | `crates/storage/src/compaction/job.rs`, `crates/storage/src/compaction/rewrite.rs`, `crates/storage/src/compaction/verify_pass.rs`, `crates/storage/src/lib.rs`, `crates/storage/src/hooks.rs` |
| P4 | `docs/mvp-decisions.md`, `docs/spec/03-storage.md`, `docs/spec/05-query.md`, `docs/spec/12-operations.md`, `website/src/content/docs/docs/self-hosting/configuration-reference.md`, `CHANGELOG.md`, `docs/research/storage/hardening.md`, `docs/roadmap.md` |

## Conventions for every phase

- **Do not compile, test, lint or run `just` while implementing.** Write the code, then commit with the phase's subject. The coordinating session runs the full checks once after P4 (user rule: the machine is loaded).
- **Never guess.** When a step is unclear, contradicts the code, or cannot be done as written, stop and report the question instead of improvising. Names below are the code's at the time of writing; if one moved, follow it and say so in the report.
- Edit only the phase's files. Report any change needed elsewhere.
- No `unwrap()`/`expect()` outside tests unless a comment next to it states the invariant. No `unsafe`. Errors and logs never carry payload content.
- Report: what was done, deviations and why, anything not done.

---

## P1 — The query runtime type and its config key

**Goal:** a `QueryRuntime` that owns a niced multi-thread Tokio runtime and runs futures and DataFusion streams on it, built by `Storage::open` from `storage.query.threads`; nothing uses it yet.

**Edit:** `crates/storage/src/query_runtime.rs` (new), `crates/storage/src/lib.rs` (the `mod` line, the `Storage` field, its construction in `Storage::open`, the `pub use`), `crates/storage/src/config.rs`, `crates/storage/Cargo.toml`, `Cargo.lock`, and the three test configs of step 4 (`model_tests/harness.rs`, `crash_tests.rs`, `model_tests/resources/memory.rs`).

**Steps:**

1. `crates/storage/Cargo.toml` `[dependencies]`: `rustix = { workspace = true, features = ["process", "thread"] }`. `Cargo.lock` changes only if cargo rewrites it at the coordinating session's first build; do not edit it by hand.

2. `config.rs`, `QueryConfig`: add the field after `target_partitions`:
   ```rust
   /// Worker threads of the query runtime, which runs every query's and every compaction
   /// job's DataFusion work (D-R-39). Defaults to the process parallelism.
   #[serde(default = "default_query_threads")]
   pub threads: usize,
   ```
   `impl Default for QueryConfig`: `threads: default_query_threads(),`. Add next to `default_target_partitions`:
   ```rust
   fn default_query_threads() -> usize {
       // `available_parallelism` fails only when the OS cannot report a count.
       std::thread::available_parallelism()
           .map(|n| n.get())
           .unwrap_or(1)
   }
   ```
   `StorageConfig::validate`, right after the `target_partitions` check:
   ```rust
   if !(1..=1024).contains(&self.query.threads) {
       return Err(ConfigError::invalid(
           "storage.query.threads",
           "must be from 1 to 1024",
       ));
   }
   ```

3. New file `crates/storage/src/query_runtime.rs`. Its full public surface:
   ```rust
   //! The query runtime (D-R-39): a Tokio runtime of its own for DataFusion execution, so a
   //! query or a compaction job never holds a worker of the runtime ingest runs on.

   use std::future::Future;
   use std::num::NonZeroUsize;
   use std::pin::Pin;
   use std::sync::Arc;
   use std::task::{Context, Poll};

   use datafusion::arrow::datatypes::SchemaRef;
   use datafusion::arrow::record_batch::RecordBatch;
   use datafusion::dataframe::DataFrame;
   use datafusion::error::DataFusionError;
   use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
   use datafusion::physical_plan::SendableRecordBatchStream;
   use futures::{Stream, StreamExt};
   use tokio::runtime::{Handle, Runtime};
   use tokio::sync::{mpsc, oneshot};
   use tokio::task::AbortHandle;

   /// Name of every thread of the query runtime.
   pub(crate) const THREAD_NAME: &str = "cetra-query";
   /// Nice levels the query threads add to the process's on Linux (D-R-39).
   #[cfg(target_os = "linux")]
   pub(crate) const NICE_INCREMENT: i32 = 10;
   /// Stack of each query thread: DataFusion plans recursively (the main runtime uses the
   /// same 8 MiB, `crates/cli/src/main.rs`).
   const STACK_BYTES: usize = 8 * 1024 * 1024;
   /// Batches a bridged stream buffers ahead of its reader. They sit outside DataFusion's
   /// memory pool: two batches, at most 2 × 8,192 rows.
   const STREAM_BUFFER: usize = 2;
   /// The error a caller gets when the query runtime has shut down.
   pub(crate) const STOPPED: &str = "the query runtime has stopped";

   /// A handle on the query runtime. Cloning shares it; the runtime shuts down when the
   /// last clone is dropped, without waiting for its tasks (`Runtime::shutdown_background`),
   /// so dropping it inside an async context is allowed.
   #[derive(Clone)]
   pub struct QueryRuntime {
       inner: Arc<Owned>,
   }

   struct Owned {
       /// `Some` until `Drop` takes it.
       runtime: Option<Runtime>,
       handle: Handle,
   }

   impl Drop for Owned {
       fn drop(&mut self) {
           if let Some(runtime) = self.runtime.take() {
               runtime.shutdown_background();
           }
       }
   }

   impl std::fmt::Debug for QueryRuntime {
       fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
           formatter.debug_struct("QueryRuntime").finish_non_exhaustive()
       }
   }
   ```
   with these functions (bodies are the implementer's, following the numbered rules):
   ```rust
   impl QueryRuntime {
       /// Builds the runtime: `threads` workers named [`THREAD_NAME`], 8 MiB stacks, I/O and
       /// time drivers on, and [`lower_priority`] as `on_thread_start`.
       pub(crate) fn new(threads: NonZeroUsize) -> std::io::Result<Self>;

       /// The runtime's handle, for `JoinSet::spawn_on`.
       pub(crate) fn handle(&self) -> &Handle;

       /// Runs `future` on the query runtime and returns its output.
       pub(crate) async fn run<F, T>(&self, future: F) -> Result<T, Stopped>
       where
           F: Future<Output = T> + Send + 'static,
           T: Send + 'static;

       /// Runs `frame.collect()` on the query runtime.
       pub(crate) async fn collect(&self, frame: DataFrame) -> Result<Vec<RecordBatch>, DataFusionError>;

       /// Runs `frame.execute_stream()` on the query runtime and returns a stream that
       /// reads the batches it produces there.
       pub(crate) async fn execute_stream(
           &self,
           frame: DataFrame,
       ) -> Result<SendableRecordBatchStream, DataFusionError>;
   }

   /// The query runtime shut down before the work finished.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
   #[error("the query runtime has stopped")]
   pub(crate) struct Stopped;

   /// `on_thread_start` of the query runtime: on Linux, sets the calling thread's nice
   /// value to `base + NICE_INCREMENT` (at most 19), where `base` is the nice value of the
   /// thread that built the runtime. Absolute, not relative: blocking-pool threads are
   /// spawned from query threads and inherit their nice value, so `current + 10` would
   /// stack. Elsewhere does nothing. A failure is logged at debug and ignored: the thread
   /// then runs at the priority it inherited.
   pub(crate) fn lower_priority(base: i32);
   ```
   Rules for the bodies:
   1. `new`: on Linux read `let base = rustix::process::getpriority_process(None).unwrap_or(0);` (elsewhere `let base = 0;`). Then `tokio::runtime::Builder::new_multi_thread()` with `.worker_threads(threads.get())`, `.thread_name(THREAD_NAME)`, `.thread_stack_size(STACK_BYTES)`, `.enable_all()`, `.on_thread_start(move || lower_priority(base))`, `.build()?`. Store `runtime.handle().clone()` as `handle` and `Some(runtime)`.
   2. `lower_priority(base)`, under `#[cfg(target_os = "linux")]`: `rustix::process::setpriority_process(Some(rustix::thread::gettid()), (base + NICE_INCREMENT).min(19))`. On Linux `PRIO_PROCESS` with a thread id changes that thread only. An error goes to `tracing::debug!(%error, "lowering the query thread priority")`. Under `#[cfg(not(target_os = "linux"))]` the body is `let _ = base;`.
   3. `run`: `let task = self.inner.handle.spawn(future);` Keep `task.abort_handle()` in a guard whose `Drop` calls `abort()`, so dropping the `run` future aborts the task. Await the `JoinHandle`: `Ok(value)` returns `Ok(value)`; a `JoinError` with `is_panic()` calls `std::panic::resume_unwind(error.into_panic())`; any other `JoinError` (cancelled: the runtime shut down, or aborted) returns `Err(Stopped)`. Spawning on a handle whose runtime has shut down gives a cancelled `JoinError`, so that case is also `Err(Stopped)`.
   4. `collect`: `self.run(async move { frame.collect().await }).await`, mapping `Err(Stopped)` to `DataFusionError::Execution(STOPPED.to_owned())`.
   5. `execute_stream`: create `let (schema_tx, schema_rx) = oneshot::channel::<Result<SchemaRef, DataFusionError>>();` and `let (batch_tx, batch_rx) = mpsc::channel::<Result<RecordBatch, DataFusionError>>(STREAM_BUFFER);`. Spawn on `self.inner.handle` a task that: (a) calls `frame.execute_stream().await`; on `Err(error)` sends `Err(error)` on `schema_tx` and returns; (b) on `Ok(stream)` sends `Ok(stream.schema())` on `schema_tx` (if the receiver is gone, return); (c) loops `while let Some(item) = stream.next().await`, sending each item on `batch_tx`; when a send fails (the reader dropped the stream) it returns, which drops the DataFusion stream and cancels the plan. The whole of `execute_stream()` runs in that task, because some operators (for example `CoalescePartitionsExec`) spawn their partition tasks inside `execute`, on the runtime that calls it.
      Then `await` `schema_rx`: `Ok(Ok(schema))` goes on; `Ok(Err(error))` returns `Err(error)`; a `RecvError` (the task ended without sending) awaits the task's `JoinHandle`: a panic → `std::panic::resume_unwind(error.into_panic())`; otherwise `DataFusionError::Execution(STOPPED.to_owned())`. The abort guard stays alive across that await. Abort the task if this function's future is dropped before the schema arrives (same guard as in `run`).
      Return `Box::pin(RecordBatchStreamAdapter::new(schema, Bridged { receiver: batch_rx, task: Some(join_handle), _abort: guard }))`. `Bridged` is a private struct (all its fields are `Unpin`, so `impl Stream` uses `self.get_mut()`) implementing `Stream<Item = Result<RecordBatch, DataFusionError>>`:
      - poll `receiver.poll_recv(cx)`; `Ready(Some(item))` → `Ready(Some(item))`;
      - `Ready(None)` (the task dropped its sender) → if `task` is `Some`, poll it (`Pin::new(task).poll(cx)`): `Pending` → `Pending`; `Ready(Ok(()))` → set `task = None`, `Ready(None)`; `Ready(Err(error))` with `error.is_panic()` → `std::panic::resume_unwind(error.into_panic())`, so a panic in the query reaches the reader as it did before; any other `Err` (cancelled) → set `task = None`, `Ready(Some(Err(DataFusionError::Execution(STOPPED.to_owned()))))`. If `task` is already `None`, `Ready(None)`.
      Without this, a panic mid-stream would end the stream early and a reader such as `sql/run.rs` `take_rows` would return a short result as if it were complete. The guard aborts the task when `Bridged` is dropped.
   6. The abort guard is one private type used by both: `struct AbortOnDrop(AbortHandle); impl Drop for AbortOnDrop { fn drop(&mut self) { self.0.abort(); } }`. Aborting a finished task is a no-op.

4. Test configs that open many storages get 2 query threads, so a model or crash suite does not start one thread per core for every storage it opens: in `model_tests/harness.rs` `model_config` (line ~43) and `crash_tests.rs` `quiet_config` (line ~109), `query: QueryConfig::default()` becomes `query: QueryConfig { threads: 2, ..QueryConfig::default() }`; in `model_tests/resources/memory.rs` `small_config` (the small-machine profile, which P4 documents; its `QueryConfig { … ..QueryConfig::default() }` literal at line ~149), add `threads: 2,`. `crash_tests.rs` `compaction_config` calls `quiet_config` and needs nothing. Leave every other test config at the default.
5. `lib.rs`: add `mod query_runtime;` next to `mod query_text;`, and `pub use query_runtime::QueryRuntime;` with the other `pub use` lines (the type is public because P2 puts it in the public `ProjectQuery`). Add the field to `Storage`, after `runtime: Arc<RuntimeEnv>,`:
   ```rust
   /// The query runtime: every query's and compaction job's DataFusion work (D-R-39).
   query_runtime: QueryRuntime,
   ```
   In `Storage::open`, build it before the compactor block: `let query_runtime = QueryRuntime::new(NonZeroUsize::new(config.query.threads).unwrap_or(NonZeroUsize::MIN)).map_err(|error| { tracing::error!(%error, "building the query runtime"); OpenError::Runtime })?;` (validate has already refused 0; the `unwrap_or` keeps it infallible without a panic). Put `query_runtime` in the `Self { … }` literal. Nothing reads it yet: add `#[allow(dead_code)] // P2 and P3 use it` on the field.

**Tests** (in a `#[cfg(test)] mod tests` at the end of `query_runtime.rs`, each `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` unless said):
- `run_returns_the_output_from_a_query_thread`: `QueryRuntime::new(NonZeroUsize::new(2).unwrap())`; `run(async { std::thread::current().name().map(str::to_owned) })` returns `Ok(Some("cetra-query"))`.
- `dropping_run_aborts_the_task`: the future holds a guard (a struct whose `Drop` sends on a `std::sync::mpsc` channel) and then awaits `std::future::pending::<()>()`. Start `run` with `tokio::time::timeout(Duration::from_millis(50), …)`, which drops it; the guard's message arrives within 2 s (`recv_timeout`).
- `a_panic_reaches_the_caller`: `#[should_panic(expected = "boom")]`; `run(async { panic!("boom") })`.
- `run_after_the_runtime_is_gone_is_stopped`: build a plain `tokio::runtime::Builder::new_multi_thread().worker_threads(1).build()`, keep `runtime.handle().clone()`, then drop the runtime inside `tokio::task::spawn_blocking(move || drop(runtime)).await` (a plain drop shuts it down synchronously, which is not allowed on an async thread); a `QueryRuntime { inner: Arc::new(Owned { runtime: None, handle }) }` (test-only construction) gives `run(async {}).await == Err(Stopped)`.
- `collect_and_execute_stream_return_the_rows`: a `SessionContext::new()`; `ctx.sql("SELECT * FROM (VALUES (1), (2), (3)) AS t(x)")` → frame; `collect` gives 3 rows; a second identical frame through `execute_stream` gives the same 3 rows when read to the end, and `stream.schema()` has the field `x`.
- `execute_stream_reports_an_execution_error`: a frame of `SELECT CAST('a' AS INT)`; `execute_stream` either returns `Err` or a stream whose first item is `Err` (DataFusion may fail at either point); assert one of the two, never `Ok` rows.
- `a_panic_mid_stream_reaches_the_reader`: `#[should_panic(expected = "udf boom")]`. A `SessionContext::new()` with a scalar UDF `boom(x)` (`datafusion::logical_expr::create_udf`, `Int64` → `Int64`, `Volatility::Volatile`) whose body panics with `"udf boom"`; `execute_stream` of `SELECT boom(value) FROM generate_series(1, 10)` and read the stream to the end. If it does not panic (DataFusion turned the panic into an error item instead), stop and report what the stream returned.
- `dropping_the_stream_stops_the_producer`: a frame over `generate_series(1, 100000000)` (`SELECT * FROM generate_series(1, 100000000)`); `execute_stream`, read one batch, drop the stream; then `tokio::time::timeout(Duration::from_secs(5), …)` around a loop that waits until `handle().metrics().num_alive_tasks() == 0` (poll every 10 ms) succeeds.
- `query_threads_run_at_a_lower_priority` (Linux only, `#[cfg(target_os = "linux")]`): `let base = rustix::process::getpriority_process(None).unwrap();` `run(async { rustix::process::getpriority_process(Some(rustix::thread::gettid())).unwrap() })` returns `(base + 10).min(19)`.
- `config.rs` tests (next to `"storage.query.target_partitions: must be at least 1"`): `{"data_dir":"/data","query":{"threads":0}}` and `{"data_dir":"/data","query":{"threads":1025}}` both fail with `storage.query.threads: must be from 1 to 1024`; `{"threads":1024}` passes; the default equals `std::thread::available_parallelism()` (or 1).

**Done when:** committed (no checks; see Conventions).

**Commit:** `storage: add the query runtime (D-R-39)`

---

## P2 — Queries execute on the query runtime

**Goal:** every read and SQL execution goes through `QueryContext::collect` / `QueryContext::execute_stream`, which run on the query runtime carried by `ProjectQuery`.

**Edit:** `crates/storage/src/views/mod.rs`, `crates/storage/src/lib.rs`, `crates/storage/src/media.rs`, `crates/storage/src/test_support.rs`, `crates/storage/src/fuzz_entry.rs`, `crates/storage/src/sql/run.rs`, the `reads/` files of step 4 (`alerts`, `deletions`, `experiments`, `facets`, `prompts`, `scores`, `sessions`, `targets`, `traces`, `users`, `widgets`), and every file with a `ProjectQuery { … }` literal (step 5's list).

**Steps:**

1. `views/mod.rs`, `ProjectQuery`: add the last field
   ```rust
   /// Where this query's DataFusion work runs (D-R-39).
   pub executor: QueryRuntime,
   ```
   (`use crate::query_runtime::QueryRuntime;`).
2. `QueryContext`: add a field `executor: QueryRuntime`, set from `source.executor` in `QueryContext::new`, and two methods next to `fail`:
   ```rust
   /// `frame.collect()`, run on the query runtime (D-R-39).
   pub(crate) async fn collect(&self, frame: DataFrame) -> Result<Vec<RecordBatch>, DataFusionError> {
       self.executor.collect(frame).await
   }

   /// `frame.execute_stream()`, run on the query runtime (D-R-39). Dropping the stream
   /// cancels the query.
   pub(crate) async fn execute_stream(
       &self,
       frame: DataFrame,
   ) -> Result<SendableRecordBatchStream, DataFusionError> {
       self.executor.execute_stream(frame).await
   }
   ```
3. `lib.rs`: `project_query` takes one more parameter `executor: &QueryRuntime` (last) and sets `executor: executor.clone()` in its two `ProjectQuery` literals (`lib.rs` 1450 and 1541). Its callers:
   - `Storage::query` passes `&self.query_runtime`.
   - `media.rs` `MediaGc::live_set` (line ~227): add the field `pub(crate) query_runtime: QueryRuntime,` to `MediaGc` (doc: `/// Where the live-set scan runs (D-R-39).`), set it in its one literal (`lib.rs` ~605) to `query_runtime.clone()`, pass `&self.query_runtime` to `project_query`, and run the scan there: build the `DataFrame` as now (`context.read_table(…)?.select_columns(&["media_refs"])?`), then `self.query_runtime.collect(frame).await` with the same `.map_err(|error| { query.suspects.report(&error); MediaError::from(error) })?`.
   - `fuzz_entry.rs` 365 and 572: `fuzz_entry` is compiled under `cfg(any(test, fuzzing))`, where `test_support` may not exist. Add next to its `runtime()` (line ~270):
     ```rust
     /// The query runtime the entries' queries run on (D-R-39).
     fn query_runtime() -> &'static QueryRuntime {
         static RUNTIME: OnceLock<QueryRuntime> = OnceLock::new();
         RUNTIME.get_or_init(|| {
             // Harness setup, not input-dependent.
             QueryRuntime::new(NonZeroUsize::new(2).expect("2 is not zero"))
                 .expect("build the fuzz query runtime")
         })
     }
     ```
     and pass `query_runtime()` at both calls.
   - Any other caller `rtk proxy grep -rn "project_query(" crates/storage/src` shows: stop and report it.
   Remove the `#[allow(dead_code)]` P1 put on the `Storage` field.
4. **Rule:** every `DataFrame` `.collect()` and `.execute_stream()` outside `#[cfg(test)]` goes through the context. `X.collect().await` becomes `ctx.collect(X).await` and `X.execute_stream().await` becomes `ctx.execute_stream(X).await`, where `X` is the whole `DataFrame` expression before `.collect()`; this includes chains written over several lines, for example
   ```rust
   let rows = frame
       .aggregate(…)
       .map_err(classify)?
       .collect()
       .await
       .map_err(|error| ctx.fail(error))?;
   ```
   becomes
   ```rust
   let rows = ctx
       .collect(frame.aggregate(…).map_err(classify)?)
       .await
       .map_err(|error| ctx.fail(error))?;
   ```
   Keep every `.map_err(…)` and `?` as it is. In `sql/run.rs` the receiver is `self` (a `QueryContext` method): `self.collect(…)` / `self.execute_stream(…)`. Where the function names its context differently than `ctx`, use that name. `futures` streams' `.collect()` (on a `Stream`, not a `DataFrame`) stay. The sites, from the branch at the time of writing (non-test code only; single-line and multi-line):
   - `reads/alerts.rs`: 210, 262, 304, 403 (multi-line);
   - `reads/deletions.rs`: 112 (`frame.collect`), 183, 256, 321 (`execute_stream`);
   - `reads/experiments.rs`: 194 (multi-line), 263, 339 (`execute_stream`);
   - `reads/facets.rs`: 560, 608, 678, 760 (multi-line; 414 is a `futures` stream and stays);
   - `reads/prompts.rs`: 198, 301 (multi-line);
   - `reads/scores.rs`: 130, 358, 510 (multi-line), 416;
   - `reads/sessions.rs`: 236, 295 (`execute_stream`), 342, 372, 387 (multi-line, `DataFrame::new(state…, plan)…`), 446 (`joined.collect`);
   - `reads/targets.rs`: 252 (`execute_stream`), 353, 404, 446;
   - `reads/traces.rs`: 462 (`required`), 478 (`earliest`), 567, 650, 736, 895, 1136 (`trace`), 1157 (`spans`), 1185;
   - `reads/users.rs`: 212, 371, 432, 455, 507, 523 (the last two multi-line);
   - `reads/widgets.rs`: 1061;
   - `sql/run.rs`: 184, 320 (`execute_stream`);
   - `fuzz_entry.rs`: 596 (`frame.collect`, result ignored: `let _ = ctx.collect(frame).await;`);
   - `media.rs` ~244: step 3.
   After the edits, `rg -U -n '\.collect\(\)\s*\.await' crates/storage/src` and `rg -n 'execute_stream\(\)' crates/storage/src` must show only lines inside `#[cfg(test)]` modules, on `futures` streams (for example `sql/run.rs` `stream.collect().await`, `reads/facets.rs` 414), or inside `query_runtime.rs` itself. List any other hit in the report and stop. If you find a `DataFrame` method that executes and is not in the list (`count`, `show`, `cache`, `write_*`, `collect_partitioned`) outside tests, stop and report it.
5. `test_support.rs` (add `use crate::query_runtime::QueryRuntime;` and `std::num::NonZeroUsize` if missing): add
   ```rust
   /// One query runtime for every test that builds a `ProjectQuery` by hand.
   pub(crate) fn query_runtime() -> QueryRuntime {
       static RUNTIME: std::sync::OnceLock<QueryRuntime> = std::sync::OnceLock::new();
       RUNTIME
           .get_or_init(|| QueryRuntime::new(NonZeroUsize::new(2).expect("2 is not zero")).expect("the test query runtime builds"))
           .clone()
   }
   ```
   and add `executor: crate::test_support::query_runtime(),` to every `ProjectQuery { … }` literal in test code. At the time of writing: `test_support.rs` 319 (`example_query`, inside the same file: `query_runtime()`), `filter/mod.rs` 781, 972; `query/traces.rs` 2830; `reads/alerts.rs` 460; `reads/experiments.rs` 443; `reads/facets.rs` 851; `reads/prompts.rs` 384; `reads/scores.rs` 1014, 1047, 1105, 1132, 1303; `reads/targets.rs` 523; `reads/traces.rs` 1847; `reads/users.rs` 1104; `reads/widgets.rs` 1769; `views/hex.rs` 203; `views/mod.rs` 833, 915, 952; `views/sessions.rs` 357. Re-run `rtk proxy grep -rn "ProjectQuery {" crates` after the edits: every hit but the struct definition and `lib.rs`'s two must have the field. If one of them is in code compiled outside `cfg(test)`, stop and report.

**Tests:**
- `views/mod.rs` tests: `a_query_executes_on_the_query_runtime`: open a storage with the existing test helper the module's other tests use, append one span, `QueryContext::new(storage.query(project, QueryOptions::new(None, None)).await?, None)`; register on `ctx.session` a scalar UDF `thread_name()` (`datafusion::logical_expr::create_udf`, `Volatility::Volatile`, returning `Utf8` of `std::thread::current().name()`, `"none"` when unnamed); `ctx.collect(ctx.session.sql("SELECT thread_name() AS t FROM spans").await?)` gives `"cetra-query"` in every row, and the same through `ctx.execute_stream`. If the module has no storage helper, use `crate::test_support`'s, and name the one used in the report.
- The existing read, SQL, filter and fuzz-entry tests are the regression suite: they must pass unchanged after the coordinating session's checks.

**Done when:** committed.

**Commit:** `storage: run queries on the query runtime`

---

## P3 — Compaction jobs on the query runtime

**Goal:** `Compactor::pass` spawns each job on the query runtime.

**Edit:** `crates/storage/src/compaction/job.rs`, `crates/storage/src/compaction/rewrite.rs`, `crates/storage/src/compaction/verify_pass.rs`, `crates/storage/src/lib.rs`, `crates/storage/src/hooks.rs`.

**Steps:**
1. `job.rs`, `Compactor`: add the field `executor: QueryRuntime` (`use crate::query_runtime::QueryRuntime;`) with the doc `/// Where the jobs run (D-R-39).` `Compactor::new` takes it as a new last parameter `executor: QueryRuntime` and stores it.
2. `Compactor::pass`: `set.spawn(async move { … })` becomes `set.spawn_on(async move { … }, self.executor.handle())`, the body unchanged. The rest of `pass` (the semaphore, joining the set) stays as it is. A job aborted because the runtime shut down comes back as a cancelled `JoinError`; keep whatever the loop that joins the set already does with a `JoinError` (read it and say in the report what it does).
3. `compaction/rewrite.rs`, the deletion-rewrite pass (`impl Compactor`, the `JoinSet` of `check_and_rewrite` tasks, line ~199): `set.spawn(async move { … })` becomes `set.spawn_on(async move { … }, self.executor.handle())`, body unchanged. Report what its join loop does with a `JoinError`.
3b. `compaction/verify_pass.rs`, the rebuild of a corrupt traces file (line ~266, `if let Err(error) = self.run(job, policy).await {`): it runs a `JobKind::Rewrite` job, a DataFusion merge, outside `pass`. Run it on the query runtime: `let compactor = Arc::clone(self); let outcome = self.executor.run(async move { compactor.run(job, policy).await }).await;` then `match outcome { Ok(Ok(_)) => {}, Ok(Err(error)) => { /* the existing failure branch, unchanged, with `error` */ }, Err(Stopped) => { /* the same branch: report.failed += 1, a warn without an error field ("rebuilding a corrupt traces file failed: the query runtime stopped"), requeue, return */ } }`. If `self` there is not an `Arc<Compactor>` receiver (`self: &Arc<Self>`), stop and report.
4. `lib.rs`, `Storage::open`: pass `query_runtime.clone()` as the new last argument of `compaction::job::Compactor::new`. If any test builds a `Compactor` directly, pass `crate::test_support::query_runtime()` there.
5. `hooks.rs`, test `Hooks` (`mod fault`): add to `State` a field `thread_names: Mutex<Vec<(HookPoint, Option<String>)>>` (initialised empty in `Default`), push `(point, std::thread::current().name().map(str::to_owned))` at the start of `pause`, next to the `passed` push, and add
   ```rust
   /// The name of the thread each passage of `point` ran on, in order.
   pub(crate) fn thread_names(&self, point: HookPoint) -> Vec<Option<String>> {
       lock(&self.inner.thread_names)
           .iter()
           .filter(|(passed, _)| *passed == point)
           .map(|(_, name)| name.clone())
           .collect()
   }
   ```
   The production `Hooks` (`#[cfg(not(test))]`) gets nothing.

**Tests:**
- `compaction::job` tests: `a_compaction_job_runs_on_the_query_runtime`: use the module's existing setup that makes `storage.compact(now)` run at least one job (copy the setup of an existing test in the module that asserts a job ran, and name it in the report); after `compact` returns, `hooks.thread_names(HookPoint::CompactionAfterWrite)` is not empty and every entry is `Some("cetra-query")`.
- Held compaction hooks do not deadlock: the test `Hooks::pause` uses `block_in_place` on a multi-thread runtime, and the query runtime is one.

**Done when:** committed.

**Commit:** `storage: run compaction jobs on the query runtime`

---

## P4 — Decision, spec, docs

**Goal:** D-R-39 and the new key are written down everywhere they belong.

**Edit:** the P4 files in the table above.

**Steps:**
1. `docs/mvp-decisions.md`, track R's table, after D-R-38, one row:
   `| D-R-39 | Query runtime | **Storage runs all DataFusion execution, every query's and every compaction job's, on a Tokio runtime of its own (\`cetra-query\`, \`storage.query.threads\` workers, default the CPU count, 1–1024), whose threads run at nice + 10 on Linux. Planning stays on the caller's runtime; the main runtime keeps ingest, HTTP, the WAL writers, flushes and timers.** One runtime let a query's or a merge's CPU-bound polls hold the workers ingest needs: in the soak \`20261125-vm-q4-12h\` the append p99 grew from 38 to 307 ms and the ingest ticks' lateness p99 from 25 to 453 ms as the retained data grew, with 8 of 12 cores idle (M7-5). Rejected: lowering \`target_partitions\` or capping concurrent queries (slower queries, and one long poll still blocks a worker); hard CPU pinning (fights container CPU quotas and wastes a core when ingest is idle); a separate query process (breaks the single binary). Object-store I/O of a query runs on the query runtime; routing it to the main runtime waits for a measurement. |`
2. `docs/spec/03-storage.md`:
   - § 6, the `Storage::query` paragraph: after "…`target_partitions`, and the physical optimizer rules the session appends (the trace list fast path, D153)", add ", and the query runtime its execution runs on (D-R-39)".
   - The **Parallelism** bullet (`target_partitions = cores`): append "; every query's and compaction job's DataFusion execution runs on the query runtime, `storage.query.threads` workers (default the CPU count) at nice + 10 on Linux, not on the runtime ingest uses (D-R-39)."
   - § 5, the **Resources:** list (line ~280): add a bullet "- Compaction jobs and deletion rewrites run on the query runtime, not on the runtime ingest uses (D-R-39)."
   - The small-machine profile sentence (line ~498, "The small-machine profile is `memtable.hard_limit` 128 MiB; …"): after "`query.target_partitions` 2" add ", `query.threads` 2".
3. `docs/spec/05-query.md`, the session config item 6: after "dropping the stream cancels it." add " `QueryContext::collect` and `QueryContext::execute_stream` run the execution on the query runtime (D-R-39); dropping the future or the stream aborts that work."
4. `docs/spec/12-operations.md`, the config table row `| \`storage.query.target_partitions\` / \`storage.query.trace_lookback\` / \`storage.query.admission_wait\` | | the CPU count / "1h" / "5s" |` (line ~64): add a separate row after it, `| \`storage.query.threads\` | | the CPU count |`.
5. `website/src/content/docs/docs/self-hosting/configuration-reference.md`, after the `storage.query.target_partitions` row: `| \`storage.query.threads\` | | integer | the number of CPU cores | Threads that run queries and compaction, apart from ingest. From 1 to 1024. On Linux they run at a lower priority (nice + 10). |` (same columns as the row above it; the server's `the_configuration_reference_matches_the_loader` test checks it).
6. `CHANGELOG.md` § `[Unreleased]` `### Changed`: `- Queries and compaction run on threads of their own (\`storage.query.threads\`, default the CPU count), at a lower priority on Linux, so busy queries no longer slow down ingestion.`
7. `docs/research/storage/hardening.md`, M7-5: change "(open)" to "(fix in review)" and append: "Fixed by D-R-39 (`docs/plans/r-hardening/09-query-runtime.md`): queries and compaction jobs run on a runtime of their own. A 12 h soak at 4 queries/s verifies it." 
8. `docs/roadmap.md`, track R's row: after "ingest latency under queries (M7-5) is open" write "ingest latency under queries (M7-5) has a fix in review (D-R-39, the query runtime)". If the row's words differ, keep its sentence structure and report the change.

**Done when:** committed.

**Commit:** `docs: record the query runtime (D-R-39)`

---

## After P4 (coordinating session)

1. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` once; fix what fails in a fix phase.
2. A Sonnet code review of the four commits.
3. PR.
4. On the VM: the soak `CETRA_SOAK_HOURS=12 CETRA_SOAK_RATE=1000 CETRA_SOAK_QPS=4 CETRA_SOAK_QUERY_TASKS=4` on this branch. Expected: tick lateness p99 flat (tens of ms, not hundreds), append p99 flat, query p95s no worse than `20261125-vm-q4-12h`, all 28 rules pass.
