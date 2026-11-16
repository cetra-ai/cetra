# B2 — `Storage::flush_now`

Read [00-design.md](00-design.md) first, especially § What exists › Flush and § Design ›
Consistency (D-N-18).

**Goal:** `Storage::flush_now(timeout)` makes both flush tasks flush everything acknowledged before the
call, and returns once the catalog's `flushed_seq` covers it. Ingestion continues meanwhile; rows
appended after the call are not waited for.

**Edit only:** `crates/storage/src/memtable.rs`, `crates/storage/src/wal/writer.rs`,
`crates/storage/src/flush.rs`, `crates/storage/src/lib.rs`, `crates/storage/src/error.rs`.

**Commit:** `Add Storage::flush_now`

## Read first

- `crates/storage/src/flush.rs`: `run`, `wait_until_due`, `flush_due`, `flush_wait`, `flush_once`.
- `crates/storage/src/wal/writer.rs`: `WriterShared` (`failed`, `memtable()`, `flush_notify`), and
  the order "insert into the memtable, then acknowledge".
- `crates/storage/src/memtable.rs`: `Active`, `Frozen`, `active_max_seq`, `has_frozen`, `freeze`.
- `crates/storage/src/catalog/committer.rs`: `Committer::flushed_seq`.
- `crates/storage/src/lib.rs`: `Storage` (`spans`, `scores`, `committer`), `shutdown`,
  `finish_flushes`.

## Changes

### `memtable.rs`

Add:

```rust
/// Highest WAL seq held, active or frozen. `0` when the memtable holds nothing.
pub(crate) fn max_seq(&self) -> u64
```

It returns the larger of the active `max_seq` and the frozen one (when a frozen memtable exists).

### `wal/writer.rs`

Add a field to `WriterShared`, and initialise it to `false` wherever `WriterShared` is built:

```rust
/// Set by [`crate::Storage::flush_now`]: the flush task flushes whatever the
/// memtable holds, regardless of `flush.max_bytes` and `flush.max_age`.
force_flush: AtomicBool,
```

Add the methods `pub(crate) fn set_force_flush(&self, on: bool)` (Release) and
`pub(crate) fn force_flush(&self) -> bool` (Acquire).

### `flush.rs`

A forced flush is due only when there is something to flush. That keeps the task from spinning on
an empty memtable.

- In `flush_due`, also return `true` when
  `params.shared.force_flush() && (memtable.has_frozen() || memtable.active_max_seq() > 0)`.
- In `flush_wait`, include the same condition in `ready`.

### `flush.rs`, error backoff

`sleep_backoff` waits on the sleep and on `stop.changed()` only. Add a third `select!` arm on
`params.notify.notified()`, so a forced flush arriving during an error backoff retries at once
instead of after up to 60 s. Pass `notify` in, or change the function to take `&FlushParams`.

### `error.rs`

```rust
/// Why [`crate::Storage::flush_now`] failed.
#[derive(Debug, thiserror::Error)]
pub enum FlushNowError {
    /// A WAL writer has stopped on an I/O error; nothing more will flush.
    #[error("the {table} WAL writer has failed")]
    WriterFailed { table: &'static str },
    /// The flush did not commit within the timeout.
    #[error("the {table} flush did not commit within {timeout:?}")]
    Timeout { table: &'static str, timeout: std::time::Duration },
    /// The storage is shutting down.
    #[error("storage is shutting down")]
    ShuttingDown,
}
```

Re-export it with `pub use crate::error::FlushNowError;` in `lib.rs`. Use the same table names as the existing
logs, `"spans"` and `"scores"`.

### `lib.rs`

Add this method to `impl Storage`, after `health`:

```rust
/// Flushes both tables up to what was acknowledged before this call and
/// waits for the catalog commit (N M2, D-N-18). Appends continue meanwhile;
/// rows acknowledged after the call are not waited for.
///
/// One caller at a time: the server's backup slot serialises calls, and
/// returning clears both tables' force flags.
///
/// # Errors
///
/// [`FlushNowError`] when a WAL writer has failed, when the storage is
/// shutting down, or when a table's flush has not committed within `timeout`.
pub async fn flush_now(&self, timeout: std::time::Duration) -> Result<(), FlushNowError>
```

Algorithm:

1. If `shutting_down`, return `ShuttingDown`.
2. For each table (spans, then scores):
   1. Lock its memtable (`shared().memtable().lock().unwrap_or_else(PoisonError::into_inner)`), read
   `target = memtable.max_seq()`, and release the lock. `target` is a one-shot snapshot, the
   backup's point in time; rows appended later are never waited for.
   2. If `target == 0`, there is nothing to wait for.
   3. Otherwise call `set_force_flush(true)`, then `flush_notify.notify_one()`. Use `notify_one`, not
      `notify_waiters`, so a task between two waits keeps the permit.
3. Poll every 20 ms, until the deadline `Instant::now() + timeout`. For each table with a target:
   - if `self.committer.flushed_seq(table).await >= target`, the table is done: call
     `set_force_flush(false)`;
   - if the writer's `shared().failed()` is true, return `WriterFailed`;
   - if `shutting_down` became true, return `ShuttingDown`;
   - past the deadline, return `Timeout`.
4. On every return path, clear `force_flush` on both tables. Use a small guard struct whose `Drop`
   clears them, so a cancelled future also clears them.
5. Remove the `#[cfg_attr(not(test), allow(dead_code))]` on the `committer` field: it is used now.

## Tests

In `lib.rs`'s test module, using the existing helpers that open a storage and append spans and
scores. Configure `flush.max_age = "1h"` and `flush.max_bytes = "1MiB"`, so nothing flushes on its
own. `max_bytes` must stay below `memtable.hard_limit`, which defaults to 1 GiB; the existing tests
use `1MiB`.

- `flush_now_commits_everything_acknowledged`:
  1. Append spans and scores; keep the receipts.
  2. Call `flush_now(10 s)`.
  3. Assert `flushed_seq(Spans) >= spans_receipt.seq`, and the same for scores.
  4. Assert both memtables are empty (`memtable_bytes() == 0`).
  5. A query returns the rows.
- `flush_now_with_nothing_to_flush_returns_at_once`: on a fresh storage, read both `flushed_seq`
  values, call `flush_now(10 s)`; it returns `Ok` within 2 s, and both values are unchanged.
- `flush_now_does_not_wait_for_later_appends`:
  1. Append once.
  2. Spawn a task that appends in a loop.
  3. `flush_now(10 s)` returns `Ok`, and `flushed_seq >= ` the first receipt's seq. The loop ignores
     `AppendError::Backpressure`.
  4. Stop the loop.
- `flush_now_clears_the_force_flag`: after `flush_now` returns, both `force_flush()` are `false`, and
  an append right after it stays in the memtable for 200 ms (`memtable_bytes() > 0`).
- `flush_now_after_shutdown_is_refused`: after `shutdown`, `flush_now` returns `ShuttingDown`.
- `flush_now_reports_a_failed_writer`:
  1. Append once.
  2. Arm `hooks.arm_fail(HookPoint::WalWrite)` (see the writer tests).
  3. Append until an append fails.
  4. `flush_now(10 s)` returns `WriterFailed`.
- `flush_now_times_out`: hold the flush at a pause point (the `FlushAfterFreeze` hook the flush
  tests use), then `flush_now(100 ms)` returns `Timeout`. Release the hook.

Running a query: use the same query plumbing the existing `lib.rs` tests use to read spans back.

## Verify

```bash
cargo test -p cetra-storage --all-features flush_now
just check
```

## Done when

- [ ] `Storage::flush_now` exists with the seven tests.
- [ ] No busy loop: a forced flag with an empty memtable is not "due".
- [ ] `just check` passes. One commit: `Add Storage::flush_now`.
