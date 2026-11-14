# R · M4 — S3 fault injection

The fourth milestone of track R (`00-design.md` § M4, decision D-R-8). It shows that a failing S3 never loses acknowledged data, never produces a wrong read, and never leaves garbage behind. Garbage here means an uncollected staged object, an orphan object or an open multipart upload. It does this at three levels:
- **in process:** a test-only `ChaosStore` (replacing `test_support::FaultStore`) fails calls the way `object_store`'s S3 client fails them once its own retries are exhausted. The M1 model test gets an *S3 mode* that runs on it;
- **over TCP:** toxiproxy in front of Versity Gateway resets, stalls, slices and throttles connections, so the S3 client's retry loop runs for real;
- **against a real provider:** one run against a Hetzner Object Storage bucket the user provides, plus probes of what our code relies on.

Sub-phases R4.1–R4.10, one commit each. R4.6 and R4.10 can produce several commits, one per bug found.

**What we already know from reading the code.** Each item is confirmed by a test before it is fixed (D-R-2):
1. `objects.rs::put_multi` aborts the upload when a part fails, but not when `complete` fails, and not when the part source (`next()`) fails. Those uploads stay open on the bucket.
2. `CacheStore` (`read_cache.rs`) caches whatever the inner store returned. Verification of a suspect file reads the raw store (spec 03 §10), but a `Healthy` verdict does not drop the file's cached blocks. A bad cached block therefore keeps failing queries until LRU evicts it.
3. `Anchors::rebuild` (`anchor.rs`) fails `Storage::open` on a store error. That is intended ("A store failure still fails the open"), but spec 03 does not say so.
4. `Objects::publish_output` in remote mode reads the spill file (up to 64 MiB) with blocking `std::fs` calls (`metadata`, `seek`, `read_exact`) inside an `async` block on a tokio worker.
5. `object_store` 0.13.2 sends every delete, even a single one, as a bulk `POST ?delete` (`aws/mod.rs::delete_stream` → `aws/client.rs::bulk_delete_request`). That request is not marked idempotent, so the client does not retry it on a timeout or an interrupted connection (`retry.rs`, the `HttpErrorKind::Timeout | Interrupted => is_idempotent` arm). GC then sees the error and retries on its next pass. That is harmless, but R4.7 records it as finding `M4-n`.

**Decisions taken with the user for M4** (2026-11-15; they are proposed for recording in § Decisions and design text for the coordinator):
- **Single-put size:** decided after R4.7 measures the slow-link case. If the finding is confirmed, R4.10 lowers `PutSizes::S3.single_max` to 16 MiB (one part's size); otherwise nothing changes.
- **`docs/self-hosting/s3.md`:** R edits it (R4.9).
- **Store errors at open:** `open` fails on a store error during the anchor rebuild, and spec 03 §10 documents it (R4.6).
- **The lifecycle rule on the Hetzner bucket:** the user applies it once with the AWS CLI before R4.8.

## Where faults appear: above the S3 client's retries

`object_store`'s S3 client (`AmazonS3`, `object_store` 0.13.2, `src/client/retry.rs`) retries inside its HTTP layer:
- **What it retries:**
  - 5xx statuses, 429 and 408;
  - a 200 whose body contains `InternalError` or `SlowDown` (only on requests marked `retry_error_body`: `complete` and copy);
  - connect and request errors (the request was not sent);
  - timeouts and interrupted connections, only for requests marked idempotent.
- **How long:** at most `max_retries` (10) times, stopping once `retry_timeout` (3 min) has passed. The backoff starts at 100 ms, doubles, and is capped at 15 s.
- **Which of our requests are idempotent:**
  - **Idempotent**, and so retried on timeouts too:
    - a plain `PutObject` (`PutMode::Overwrite`, `aws/mod.rs::put_opts`);
    - `CreateMultipartUpload`, `UploadPart` and `CompleteMultipartUpload` (`aws/client.rs`, `.idempotent(true)`);
    - `AbortMultipartUpload` through the upload handle we use (`aws/mod.rs`, `S3MultiPartUpload::abort`, `.idempotent(true)`). The other abort, `aws/client.rs::abort_multipart`, is not marked idempotent, but it is only reached through the `MultipartStore` trait, which we don't use;
    - `GET` and `HEAD`: their idempotency is unset, so it falls back to the HTTP method being safe.
  - **Not idempotent:** the delete, which is a bulk `POST ?delete` even for one key. It is retried on 5xx and on connect or request errors, but not on a timeout or an interrupted connection (§ What we already know, item 5).
- **Timeouts:** each request has its own `timeout` (30 s by default) and `connect_timeout` (5 s) (`ClientOptions` defaults, `src/client/mod.rs`).
- **The error left after the retries:** `RetryError::error` (`retry.rs`) maps the final status:
  - 404 → `Error::NotFound`;
  - 304 → `NotModified`;
  - 412 → `Precondition`;
  - 409 → `AlreadyExists`;
  - 403 → `PermissionDenied`;
  - 401 → `Unauthenticated`;
  - everything else, including 500, 503 and every transport error, → `Error::Generic { store: "S3", source }`.

`ChaosStore` wraps an `Arc<dyn ObjectStore>`, so it sits **above** that loop and never triggers it. A fault it injects is therefore what our code sees **after the client has given up**: a 503 that lasted for three minutes, or a reset on every attempt. That is the right level for testing *our* handling (flush retry, staging, GC, verification), and it is why toxiproxy (R4.7) is needed to exercise the client's loop itself.

Every `ChaosStore` error is therefore `object_store::Error::Generic { store: "S3", source }`, with `source` a small error type whose text names the simulated status (`503 SlowDown`, `500 InternalError`). Our code distinguishes only:
- `NotFound`, in `Objects::delete`, `Objects::delete_len`, `verify_file` and `is_store_error`;
- `AlreadyExists`, which local writes return.

So `Generic` is the variant that reaches the same code paths a real exhausted 5xx or reset reaches.

**What `ChaosStore` never injects, and why.** This deviates from D-R-8; the new decision text is in § Decisions and design text for the coordinator.
- **`NotFound` for an object that exists.** S3 has been strongly consistent for reads after writes, and for listings, since December 2020, and our code relies on it: `verify_file` treats a missing object as corrupt and quarantines it (spec 03 §10). A gateway that answers 404 for an existing key would make us drop data. R4.8 probes this on Hetzner, and the finding goes into `hardening.md` and `docs/self-hosting/s3.md`.
- **Listing faults** (omitted or repeated entries). Storage never lists the bucket (spec 03 §5: "the bucket is never listed"). The one exception is N's operator command `collect_orphans`, which M4 uses only as an audit. Instead of a fault nobody would see, `ChaosStore` counts `list` calls, and the model asserts that none happens outside the audit. That pins the spec rule with a test.
- **`Ok` with wrong bytes, in the model.** TLS, and S3's own integrity checks, make silently corrupted bytes on a GET unrealistic, and Parquet pages carry no checksum in our files, so the model could not tell a wrong value from a right one. Wrong bytes are injected only in the focused tests of R4.6, which check what can be checked: no healthy file is quarantined, and the read cache recovers. A *truncated* body is realistic (a connection cut mid-body on every retry). It is modelled as a stream that yields part of the body and then an error, which is what `AmazonS3` returns after its body retries.

## Conventions for every sub-phase

- **`just check` passes before each commit.** Node comes from nvm and is not on the non-interactive PATH: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first (if that version is gone, `ls ~/.local/share/nvm/` for the current one). One commit per sub-phase, with the subject given.
- **Prerequisite.** M1 is merged (R1.1–R1.8): `crates/storage/src/model_tests/` with `Harness`, `Model`, `Op`, `apply`, `settle`, `check_quick`, `check_full`, `check_settled_files`, `run_case`, and `just model`. If a name below differs from M1's code, follow the code and report the difference.
- **Other tracks in the same crate** (`00-design.md` § Other tracks in `crates/storage`):
  - **N's M2 B5, `Storage::collect_orphans(min_age, dry_run)`.** The orphan audit is that function in dry-run mode, not a second lister. If B5 has not merged when R4.4 starts, R4.4 writes `test_support::collect_orphans_dry_run`, a test helper with B5's exact semantics (spec in `docs/plans/n-packaging/m2/05-orphans.md`, on `origin/main`), and says in its report that B5 replaces it. B5's commit then deletes the helper and points its callers at the real method; tell the coordinating session so B5's plan says so.
  - **N's M2 B1, `gc.grace` up to 30 days.** M4 never relies on the default grace: every GC call passes an explicit grace (`collect_obsolete(Duration::ZERO)` in the model, explicit values in the integration tests).
  - **N's M2 B2, `Storage::flush_now`.** If it has merged and M1 got its `Flush` op, that op runs under faults in S3 mode like any other op. Nothing else changes.
- **Scope.** Edit only:
  - `crates/storage/src/test_support.rs` and the new `crates/storage/src/test_support/chaos.rs`;
  - the test modules that use `FaultStore` today, for the migration (R4.1): `objects.rs` (`mod tests`), `read_cache.rs` (`mod tests`), `lib.rs` (`mod tests`), `quarantine_tests.rs`;
  - `crates/storage/src/objects.rs` (R4.3: the part-size seam and the abort fix);
  - `crates/storage/src/lib.rs`: the `open_with_store` signature (R4.3) and the `mod s3_fault_tests;` line (R4.6);
  - `crates/storage/src/s3_fault_tests.rs` (new, R4.6);
  - `crates/storage/src/model_tests/` and `crates/storage/proptest-regressions/`;
  - `crates/storage/tests/s3.rs`, and the new `crates/storage/tests/s3_chaos.rs` and `crates/storage/tests/s3_probe.rs`;
  - `crates/storage/Cargo.toml`: one dev-dependency (R4.7);
  - `justfile`, `AGENTS.md` (§ Commands, § Tests), `docs/`;
  - `CHANGELOG.md` § `[Unreleased]`, for user-visible changes. The sub-phase that makes each change adds its line:
    - R4.3, the multipart abort;
    - R4.6, the cache eviction;
    - R4.9, the lifecycle-rule docs;
    - R4.10, the single-put size, if it changes.

  R4.6 and R4.10 may fix bugs anywhere in `crates/storage/src/` (D-R-2). Each fix stops and is reported instead when it would change a file format, a public API or a spec rule.
- **Dependencies.**
  - **Rust:** one new dev-dependency, `reqwest = { workspace = true }` in `crates/storage/Cargo.toml` `[dev-dependencies]` (R4.7). The workspace pins `reqwest = "0.12.28"` with `rustls-tls` and `json` (root `Cargo.toml`), and the lockfile already has it through `llmobs-server`, so no version is added.
  - **No random-number crate.** `chaos.rs` has its own 64-bit SplitMix generator (ten lines), seeded by proptest.
  - **External tools** (not in `just check`):
    - **Toxiproxy 2.13.0** (released 2026-10-14), either as the image `ghcr.io/shopify/toxiproxy:2.13.0` (the tag exists on GHCR as of 2026-11-15) or as the release binary `toxiproxy-server-linux-amd64` from `https://github.com/Shopify/toxiproxy/releases/tag/v2.13.0`;
    - **Versity Gateway `versity/versitygw:v1.11.0`**, as `tests/s3.rs` already documents.

    **Verify both tags when M4 runs** (`docker pull`), and if a newer release exists, use it and record the versions in `hardening.md`.
    - **`curl` with `--aws-sigv4`** (curl 7.75 or later).
- **Logging and privacy.** Test output may print object keys and counts; they hold only test project ids. Never print or write S3 credentials. The real-S3 tests read `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` from the environment only, never write them to a file, a log or a command line (§ R4.7 shows how `curl` gets them through stdin), and `hardening.md` records the provider and region but not the bucket name or the endpoint host.
- **Panics.** Test code may `unwrap`/`expect`, with an `expect` message that says what was expected. `ChaosStore` itself never panics on a call: a fault is an `Err`, and its own bookkeeping uses `lock().unwrap_or_else(PoisonError::into_inner)` as `FaultStore` does.
- **Real time.** The flush backoff (1 s doubling to 60 s, spec 03 §2) is wall time, so a flush that fails under faults costs at least a second. Fault rates in the model are set so that a case stays within its budget (R4.5).
- **Report at the end of each sub-phase:**
  - what was done;
  - any deviation from this plan, and why;
  - the run time of the tests the sub-phase added (`cargo test` in debug);
  - anything left undone or unverified.

---

## R4.1 — `ChaosStore` replaces `FaultStore`

Commit: `storage: replace FaultStore with ChaosStore (R4.1)`

### Files

- `crates/storage/src/test_support.rs`: delete `mod fault_store { … }` and its `pub(crate) use fault_store::{FaultKind, FaultStore};`. Add `mod chaos;` and `pub(crate) use chaos::{Call, ChaosStore, Fault, Status};`. `test_support.rs` stays a file; the child module lives in `src/test_support/chaos.rs`, the non-`mod.rs` layout.
- `crates/storage/src/test_support/chaos.rs`: new.

### Types (`chaos.rs`)

```rust
/// Which kind of store call a fault applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Call {
    Put,            // put_opts
    MultipartStart, // put_multipart_opts
    PutPart,        // MultipartUpload::put_part
    Complete,       // MultipartUpload::complete
    Abort,          // MultipartUpload::abort
    Get,            // get_opts without `head`
    Head,           // get_opts with `head` (what `ObjectStoreExt::head` sends)
    Delete,         // each path of delete_stream
    List,           // list and list_with_delimiter
    Copy,           // copy_opts
}

/// The S3 status a fault simulates, after the client's retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// 503 SlowDown.
    SlowDown,
    /// 500 InternalError.
    Internal,
}

/// One injected fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fault {
    /// Waits, then runs the call normally.
    Delay(Duration),
    /// Fails without reaching the inner store.
    Before(Status),
    /// Runs the call on the inner store, then fails: the effect happened
    /// (the ambiguous upload of spec 03 §2).
    After(Status),
    /// `Get` only: the body yields its first half, then an error.
    Truncated,
    /// `Get` only: `Ok`, with one byte flipped. Never scheduled, only pushed.
    Corrupt,
}

/// A store that injects faults into the calls it forwards to `inner`.
/// Clones share their state.
#[derive(Debug, Clone)]
pub(crate) struct ChaosStore {
    inner: Arc<dyn ObjectStore>,
    state: Arc<Mutex<ChaosState>>,
}
```

`ChaosState` (private) holds:
- `scripted: BTreeMap<Call, VecDeque<Fault>>`;
- `held: BTreeSet<Call>`;
- `calls: BTreeMap<Call, u64>`;
- `injected: u64`;
- R4.2 adds the schedule, the upload ledger and the journal.

### Behaviour

`fn next_fault(&self, call: Call) -> Option<Fault>`, called once per call:
1. Increment `calls[call]`.
2. If `held` contains `call`, return `Some(Fault::Before(Status::SlowDown))`.
3. Otherwise pop the front of `scripted[call]`.

Every `Some` other than `Delay` increments `injected`. `Delay` is not a failure.

The error is built by `fn injected(status: Status, call: Call) -> object_store::Error`:
```rust
object_store::Error::Generic { store: "S3", source: Box::new(Injected { status, call }) }
```
`Injected` is a private `thiserror` struct, with the message `"injected {code} on {call:?} (after the client's retries)"`, where `code` is `"503 SlowDown"` or `"500 InternalError"`.

Per method of `impl ObjectStore for ChaosStore` (`#[async_trait]`, same imports as the old `FaultStore`):
- **`put_opts`** (`Call::Put`):
  - `Delay(d)`: `tokio::time::sleep(d)`, then forward;
  - `Before`: return the error;
  - `After`: forward, then return the error (if the inner call failed, return its error);
  - `None`: forward.
- **`put_multipart_opts`** (`Call::MultipartStart`): the same, around `inner.put_multipart_opts`. On success it wraps the inner upload in `ChaosUpload { inner, store: self.clone() }`. With `After`, the inner upload was created and is then dropped unreturned, so nobody can complete or abort it: an *orphaned* upload (R4.2 records it).
- **`ChaosUpload: MultipartUpload`:**
  - `put_part` (`Call::PutPart`) is a synchronous method that returns a boxed `'static` future (`UploadPart`). It decides the fault synchronously, then:
    - **`Before`:** does **not** call `inner.put_part`, and returns `Box::pin(async move { Err(error) })`;
    - **`After` and `Delay`:** call `let part = self.inner.put_part(data);` **synchronously**, before returning, so the inner upload sees its parts in call order. Then return `Box::pin(async move { …; part.await?; … })`: `Delay` sleeps before `part.await`, and `After` returns the error after it;
    - **`None`:** returns `self.inner.put_part(data)` unchanged.
  - `complete` (`Call::Complete`) and `abort` (`Call::Abort`) follow `put_opts`. `After` on `Abort` means the abort happened and reported an error. The ledger (R4.2) records it as `AbortFailed`, because that is what our code sees.
  - `ChaosUpload` derives no `Debug` (the inner box is `dyn`); implement `Debug` by hand as `FaultMultipart` does.
- **`get_opts`** (`Call::Head` when `options.head`, else `Call::Get`):
  - `Before`: the error. `Delay`: sleep, then forward. `After`: same as `Before` (a read has no effect).
  - `Truncated` (`Get` only): forward, then replace the payload with a stream that yields the first `len / 2` bytes of the body and then `Err(injected(Status::Internal, Call::Get))`. Collect the inner body with `GetResult::bytes()` first, then build `GetResultPayload::Stream`, keeping `meta`, `range` and `attributes`.
  - `Corrupt` (`Get` only): forward, collect, flip byte `len / 2` (`^= 0xFF`; leave an empty body unchanged), and return `Ok` with the same metadata.
- **`delete_stream`** (`Call::Delete`): map each incoming `Result<Path>`:
  - `Before`: yield `Err` without deleting;
  - `After`: delete through `inner.delete(&path)`, then yield `Err`;
  - `None`: delete and yield `Ok(path)`.

  Build it with `futures::StreamExt::then` over `locations`, cloning `self` into the closure. This is per path, so a batch delete can partly fail, as S3's `DeleteObjects` can.
- **`list`, `list_with_delimiter`** (`Call::List`) and **`copy_opts`** (`Call::Copy`): count the call, then forward unchanged.
- **`Display`:** `ChaosStore(<inner>)`.

Public methods:

```rust
impl ChaosStore {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>) -> Self;
    /// The store faults are injected in front of (tests read and audit through it).
    pub(crate) fn inner(&self) -> &Arc<dyn ObjectStore>;
    /// Queues `fault` for the next call of kind `call`.
    pub(crate) fn push(&self, call: Call, fault: Fault);
    /// Fails every call of these kinds before it reaches the store, until `release`.
    pub(crate) fn hold(&self, calls: &[Call]);
    /// Ends every `hold`, and drops every scripted fault still queued
    /// (`push`, and R4.2's `push_at`), so a test starts its next step clean.
    pub(crate) fn release(&self);
    /// Calls of this kind so far, failed or not.
    pub(crate) fn count(&self, call: Call) -> u64;
    /// Faults that failed a call so far (not delays).
    pub(crate) fn injected(&self) -> u64;
}
```

### Migration of every `FaultStore` user

Find them with `rg -n 'FaultStore|FaultKind' crates/storage/src`. Today they are:
- `lib.rs` `mod tests`: four tests, around the `open_with_store` calls;
- `objects.rs` `mod tests`: `remote_put_round_trips_and_reports_faults`, `a_failed_remote_output_upload_removes_the_spill_file`;
- `read_cache.rs` `mod tests`: the `Fixture` with `fault: FaultStore`;
- `quarantine_tests.rs`: two tests.

Map each old call as follows:

| Old | New |
|---|---|
| `FaultStore::new(inner)` | `ChaosStore::new(inner)` |
| `enqueue(FaultKind::FailBefore)` / `arm(…)` before a small upload | `push(Call::Put, Fault::Before(Status::SlowDown))` |
| the same before an upload larger than the single-put limit (multipart) | `push(Call::MultipartStart, Fault::Before(Status::SlowDown))` |
| `FaultKind::FailAfter` on a small upload | `push(Call::Put, Fault::After(Status::SlowDown))` |
| `FaultKind::FailAfter` on a multipart upload (the old `FaultMultipart` failed `complete` after it succeeded) | `push(Call::Complete, Fault::After(Status::SlowDown))` |
| `FaultKind::Delay(d)` | `push(Call::Put, Fault::Delay(d))` |
| `hold_failures()` / `release()` | `hold(&[Call::Put, Call::MultipartStart])` / `release()` |
| `fail_reads(true)` / `fail_reads(false)` | `hold(&[Call::Get, Call::Head])` / `release()` |
| `gets()` | `count(Call::Get) + count(Call::Head)` |

`arm` replaced the queue, and `push` appends to it. Each test that used `arm` armed an empty queue, so `push` is equivalent there. If one did not, stop and report. The old queue was shared between `put` and `put_multipart`. Check for each test which call its uploads make (a size above `SINGLE_PUT_MAX` is multipart) and push on that call.

Every migrated test must still pass unchanged in what it asserts.

### Tests (`chaos.rs`, `#[cfg(test)] mod tests` is not needed: the module is test-only)

Each test runs on `InMemory`:
1. `before_fails_without_writing`: `push(Put, Before)`, `put` → `Err(Generic { store: "S3", .. })`, `inner.head` → `NotFound`.
2. `after_writes_then_fails`: `push(Put, After)`, `put` → `Err`, `inner.get` returns the bytes, and `chaos.injected() == 1`.
3. `held_calls_fail_until_released`: also checks that `release` drops queued faults: `push(Put, Before)` twice, `release()`, then a `put` → `Ok`.
4. `truncated_get_errors_after_half_the_body`: put 1,000 bytes, `push(Get, Truncated)`, `get(...).bytes()` → `Err`; the next `get` → the 1,000 bytes.
5. `corrupt_get_flips_one_byte`.
6. `delete_after_deletes_and_fails`: `push(Delete, After)`, `delete` → `Err`, `inner.head` → `NotFound`.
7. `part_and_complete_faults_reach_the_upload`: a multipart upload with `push(PutPart, Before)`. The first `put_part` → `Err`, the second → `Ok`. Then `push(Complete, After)`: `complete` → `Err`, and the object exists.
8. `counts_every_call_kind`: one call of each kind (the `Head` comes from `ObjectStoreExt::head`), then `count` is 1 for each.

### Commands

```bash
cargo test -p llmobs-storage --all-features test_support::chaos
cargo test -p llmobs-storage --all-features
rg -n 'FaultStore|FaultKind' crates/ docs/   # nothing left, outside docs that describe history
```

Fix doc comments that name `FaultStore` (e.g. `Objects::remote_for_test`).

---

## R4.2 — Seeded schedule, upload ledger, journal

Commit: `storage: add a seeded fault schedule and an upload ledger to ChaosStore (R4.2)`

### Schedule

```rust
/// Probabilities, per call, of a random fault while the schedule is active.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rates {
    /// Any call: a `Delay` of up to `max_delay`.
    pub(crate) delay: f64,
    pub(crate) max_delay: Duration,
    /// Put, MultipartStart, PutPart, Complete, Delete.
    pub(crate) before: f64,
    pub(crate) after: f64,
    /// Abort: `Before`, so the upload stays open.
    pub(crate) abort: f64,
    /// Get and Head: `Before`.
    pub(crate) read_error: f64,
    /// Get: `Truncated`.
    pub(crate) truncated: f64,
}

impl Rates {
    /// The model's S3 mode (R4.5 may tune these by measurement).
    pub(crate) const MODEL: Rates = Rates {
        delay: 0.2, max_delay: Duration::from_millis(5),
        before: 0.06, after: 0.03, abort: 0.3,
        read_error: 0.04, truncated: 0.04,
    };
}

impl ChaosStore {
    /// Random faults from `seed` at `rates`. Off until `set_active(true)`.
    pub(crate) fn set_schedule(&self, seed: u64, rates: Rates);
    /// Turns the random schedule on or off. Scripted faults and holds are not affected.
    pub(crate) fn set_active(&self, on: bool);
    pub(crate) fn is_active(&self) -> bool;
}
```

`next_fault` order:
1. `held`;
2. `scripted`;
3. then, if the schedule is active, the checks of the call's row below, in order. Each check takes one uniform draw from the RNG and compares it with its rate; the first that hits wins, and no later check draws.

| Call | Checks, in order |
|---|---|
| `Put`, `MultipartStart`, `PutPart`, `Complete`, `Delete` | `before` → `Before`, `after` → `After`, `delay` → `Delay` |
| `Get` | `read_error` → `Before`, `truncated` → `Truncated`, `delay` → `Delay` |
| `Head` | `read_error` → `Before`, `delay` → `Delay` |
| `Abort` | `abort` → `Before`, `delay` → `Delay` |
| `List`, `Copy` | none (no draw) |

The status of a drawn `Before`/`After` is `SlowDown` or `Internal` from one more draw (< 0.5 is `SlowDown`). `Corrupt` is never drawn. A delay lasts `max_delay × draw`, with one more draw.

**The RNG.** A private SplitMix64:

```rust
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 { (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64 }
}
```

`as` casts between integer and float are allowed here; add `#[allow(clippy::cast_precision_loss)]` on `unit` if clippy asks.

**Reproducibility, and its limits.** The draws are consumed in call order. The model's S3 config makes most calls sequential (R4.4):
- `flush.upload_concurrency = 1`;
- `compaction.threads = 1`;
- `query.target_partitions = 1`, so a query reads its files one at a time.

Two sources of concurrency remain:
- **At shutdown**, the `spans` and `scores` flush tasks run together (`Storage::finish_flushes`), so their uploads can interleave in either order.
- **Read-cache write-through** runs in `spawn_blocking`, but it does no store calls.

So one seed with one op list *usually* replays the same faults. A case with a restart that flushes both tables may not. Object names hold UUIDv7s, so the journal's paths differ between replays even when its `(call, fault)` sequence matches.

**The default form of a regression test is therefore scripted:** R4.10 turns the journal of a failing case into `push(call, fault)` calls in order, run with the schedule off. The seed is kept only as a note.

### Upload ledger

Each `ChaosUpload` gets an id (a counter) and a ledger entry:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UploadState {
    /// Started; neither completed nor aborted yet.
    Open,
    /// `complete` returned Ok.
    Completed,
    /// `complete` returned an error and nothing aborted the upload since.
    CompleteFailed,
    /// `abort` returned Ok.
    Aborted,
    /// `abort` returned an error: the upload stays on the bucket until a lifecycle rule removes it.
    AbortFailed,
    /// `CreateMultipartUpload` took effect but its answer was lost (`After` on
    /// `MultipartStart`): no caller holds it.
    Orphaned,
    /// Dropped while `Open`: the future that owned it went away.
    DroppedOpen,
}

#[derive(Debug, Clone)]
pub(crate) struct UploadRecord {
    pub(crate) id: u64,
    pub(crate) path: String,
    pub(crate) state: UploadState,
    /// `DroppedOpen` only: dropped while a life was ending (`set_ending(true)`).
    pub(crate) cancelled: bool,
}

impl ChaosStore {
    /// Marks the window in which a life ends. The model harness sets it just
    /// before `shutdown` or the drop of `Storage`, and clears it once the old
    /// runtime has shut down, before the next open.
    pub(crate) fn set_ending(&self, on: bool);
    pub(crate) fn uploads(&self) -> Vec<UploadRecord>;
    /// Uploads our code left open although it could have closed them:
    /// `Open`, `CompleteFailed`, and `DroppedOpen` with `cancelled == false`.
    pub(crate) fn leaked_uploads(&self) -> Vec<UploadRecord>;
    /// Uploads only a lifecycle rule can remove: `AbortFailed`, `Orphaned`,
    /// and `DroppedOpen` with `cancelled == true`.
    pub(crate) fn abandoned_uploads(&self) -> Vec<UploadRecord>;
}
```

- **Transitions.** `complete` moves the state to `Completed` or `CompleteFailed`, and `abort` to `Aborted` or `AbortFailed`, whatever the previous state.
- **`impl Drop for ChaosUpload`:** an `Open` entry becomes `DroppedOpen`, with `cancelled` set to whether the ending window is open at that moment.
- **What counts as leaked.** Cancellation while a life ends is expected:
  - a `Crash` drops `Storage` and aborts its tasks;
  - a `Restart` cancels a flush still running after 30 s (`Storage::finish_flushes`, `lib.rs`). That includes uploads the shutdown flush itself started.

  Every such drop happens inside the ending window: during `shutdown`, the drop of `Storage`, or the runtime's shutdown. So the upload is recorded as `cancelled` and counts as **abandoned** (the lifecycle rule's job). An upload dropped open outside that window was dropped by our own code without an abort, and that is a **leak**. `leaked_uploads` is what the model asserts empty (R4.5).

**Scripted replay.** Add `pub(crate) fn push_at(&self, call: Call, ordinal: u64, fault: Fault)`: inject `fault` on the `ordinal`-th call of that kind (1-based, the `calls[call]` count after incrementing). `next_fault` checks it before `scripted`. `release()` clears these too (R4.1). This is how R4.10 replays a failing case without the RNG.

### Journal

`journal: VecDeque<String>`, the last 200 entries of `"{call:?}#{ordinal} {path} {fault:?}"`, with `pub(crate) fn journal(&self) -> Vec<String>`. `ordinal` is the per-kind call count that `push_at` takes. Record only calls that got a fault or a delay, plus every `MultipartStart`, `Complete` and `Abort`. The model prints it when a case fails (R4.5).

### Tests (`chaos.rs`)

1. `the_same_seed_gives_the_same_faults`: two stores with seed 7 and `Rates::MODEL`, 500 `put`s each of distinct keys, active. Then `injected()` is equal, and the `(call, fault)` sequences, taken from the journals with paths stripped, are equal.
2. `an_inactive_schedule_injects_nothing`: `is_active()` is false after `set_schedule`, 100 puts inject nothing; after `set_active(true)`, `is_active()` is true.
3. `the_ledger_tracks_every_outcome`:
   - one upload completed;
   - one aborted;
   - one whose `complete` fails (scripted `Before`) → `CompleteFailed`, then `abort` → `Aborted`;
   - one dropped while open → `DroppedOpen`, `cancelled == false`, leaked;
   - one dropped while open after `set_ending(true)` (then `set_ending(false)`) → `DroppedOpen`, `cancelled == true`, abandoned;
   - one created with `push(MultipartStart, After)` → `Orphaned`.

   Assert each record's `id` (distinct, increasing), `path` (the key passed in) and `state`. Then `leaked_uploads` is exactly the first dropped one, and `abandoned_uploads` exactly the second dropped one and the orphaned one.
4. `push_at_hits_the_given_call`: `push_at(Put, 3, Before)`; puts 1, 2 and 4 succeed, put 3 fails.

---

## R4.3 — Part sizes carried by `Objects`; abort every failed multipart upload

Commit: `storage: abort a multipart upload on every failure after it starts (R4.3)`

### Part sizes

Multipart starts above 64 MiB (`SINGLE_PUT_MAX`), so the model's files (a few KiB) never take that path. Make the two sizes values carried by `Objects`. This is production code: `Objects::new` uses `PutSizes::S3`, and only tests pass other sizes. It is a plain struct and a field, not a seam that compiles away; its cost is two words per `Objects`.

```rust
/// When an upload switches to multipart, and the part size (spec 03 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PutSizes {
    /// `put` up to this size, inclusive; multipart above.
    pub(crate) single_max: usize,
    /// Size of every part except the last.
    pub(crate) part: usize,
}

impl PutSizes {
    /// What S3 gets: 64 MiB single puts, 16 MiB parts (S3's minimum part is 5 MiB).
    pub(crate) const S3: PutSizes = PutSizes { single_max: 64 * 1024 * 1024, part: 16 * 1024 * 1024 };
}
```

- Delete the constants `SINGLE_PUT_MAX` and `MULTIPART_PART`. `Objects` gets a field `put_sizes: PutSizes`, which `Objects::new` sets to `PutSizes::S3`.
- `Objects::remote_for_test(store, data_dir, hooks)` gains a fourth parameter, `put_sizes: PutSizes`.
- `put_remote`, `put_multi` and the remote branch of `publish_output` read `self.put_sizes`, or take it as a parameter, instead of the constants. Inside `publish_output`'s part closure, `vec![0_u8; MULTIPART_PART]` becomes `vec![0_u8; sizes.part]`, with `sizes` copied into the closure.
- `Storage::open_with_store(config, store)` (`lib.rs`, `#[cfg(test)]`) becomes `open_with_store(config, store, put_sizes: PutSizes)`.
  - **Don't add a separate parameter to `open_inner`/`open_objects`.** `open_objects` reads `remote` only under `cfg(test)` and discards it otherwise (`#[cfg(not(test))] let _ = remote;`), so a second parameter would be unused in non-test builds and fail clippy.
  - Change the existing parameter's type instead, from `Option<Arc<dyn ObjectStore>>` to `Option<(Arc<dyn ObjectStore>, PutSizes)>`, in `open_inner` and `open_objects`. Keep the same `cfg` handling, and pass the sizes to `Objects::remote_for_test`. `Storage::open` passes `None` as today.
  - Update every caller in the same commit. Existing callers pass `PutSizes::S3`; `rg -n 'open_with_store|remote_for_test' crates/storage` lists them.
- Tests that used `SINGLE_PUT_MAX + 1`-byte buffers (`objects.rs` tests) switch to a small `PutSizes { single_max: 1024, part: 512 }` and buffers of a few KiB. They then stop allocating 64 MiB. Keep each assertion's meaning (for example, "the large object took the multipart path" becomes `count(Call::MultipartStart) == 1`).

Production always gets `PutSizes::S3` from `Objects::new`, and nothing in the config changes. The `PutSizes` type and the `put_sizes` field are used in every build, so no `dead_code` allowance is needed.

### The bug, confirmed first

`put_multi` (`objects.rs`):

```rust
while let Some(part) = next().await? {          // a failed read here returns without abort
    if let Err(source) = upload.put_part(part).await {
        let _ = upload.abort().await;
        return Err(ObjectError::upload(path, source));
    }
}
upload.complete().await.map_err(...)?;          // a failed complete is never aborted
```

Write the five tests below first and run them against the current code. Expected before the fix:
- test 2 fails, with a `CompleteFailed` record;
- test 3 fails, with a `CompleteFailed` record;
- test 4 fails, with a `DroppedOpen` record: the upload is dropped when `put_multi` returns.

Put the observed failures in the report.

**Tests** (`objects.rs` `mod tests`; `Objects::remote_for_test(Arc::new(chaos.clone()), dir, Hooks::default(), PutSizes { single_max: 1024, part: 512 })`, and a 3,000-byte object written with `put_durable`):
1. `a_failed_part_aborts_the_upload`: `push(PutPart, Before)` → `Err(ObjectError::Upload)`, and the ledger's only record is `Aborted`.
2. `a_failed_complete_aborts_the_upload`: `push(Complete, Before)` → `Err(Upload)`, record `Aborted`, no object on `inner`.
3. `an_ambiguous_complete_aborts_and_keeps_the_object`: `push(Complete, After)` → `Err(Upload)`, record `Aborted`, and **the object still exists on `inner`**. Abort cannot remove a completed object. With `InMemory`, abort after complete is a no-op. On S3, `AbortMultipartUpload` on a completed upload answers `NoSuchUpload`, which R4.8 probes. The name stays staged for GC (spec 03 §2).
4. `a_failed_part_read_aborts_the_upload`: call `put_multi` directly with a `next` closure that returns `Ok(Some(part))` once and then `Err(ObjectError::InvalidPath)` → `Err`, record `Aborted`. Driving this through `publish_output` would need a spill file that fails to read, which is hard to provoke.
5. `a_failed_abort_is_reported_as_abandoned`: `push(PutPart, Before)` and `push(Abort, Before)` → `Err(Upload)`; `abandoned_uploads()` has the record, `leaked_uploads()` is empty.

### The fix

Restructure `put_multi` so that every error after `put_multipart` succeeded aborts, best effort, then returns the original error:

```rust
let mut upload = store.put_multipart(&location).await.map_err(|source| ObjectError::upload(path, source))?;
let outcome = send_parts(upload.as_mut(), path, &mut next).await;
if outcome.is_err() {
    // A failed `complete` may still have created the object. Aborting
    // cannot remove a completed object (S3 answers NoSuchUpload); it only
    // frees the parts when the upload really failed. The name stays
    // staged either way, and GC removes it (spec 03 §2).
    let _ = upload.abort().await;
}
outcome
```

`send_parts(upload: &mut dyn MultipartUpload, path, next) -> Result<(), ObjectError>` sends every part, then `complete`s.

### Spec

Spec 03 §2 *Flush*, step 4, after the "An upload that fails ambiguously…" sentence, add:

> A multipart upload that fails after it started (a part, the part source or `complete`) is aborted, best effort. An abort cannot remove an object that `complete` did create, so this is safe after an ambiguous `complete`. Uploads that no abort reached remain on the bucket: those whose abort failed, those whose `CreateMultipartUpload` answer was lost, and those cut by a crash or a cancelled task. The bucket's lifecycle rule removes them (`docs/self-hosting/s3.md`).

Spec 03 §5 *Resources* (S3 outputs) gets: "A failed upload is aborted as at flush (§2)."

Spec 03 §10, row *Ambiguous upload*, append: "; a failed multipart upload is aborted (§2)".

### Changelog

`CHANGELOG.md` § `[Unreleased]` § `Fixed`: "With S3, a multipart upload that fails is now aborted, so its parts no longer stay on the bucket (track R)."

---

## R4.4 — The model's S3 mode, without faults

Commit: `storage: run the model test against a remote store with the read cache (R4.4)`

### Harness (`model_tests/harness.rs`)

```rust
/// The remote store of an S3-mode harness.
pub(super) struct Remote {
    /// What storage writes to: faults are injected here.
    pub(super) chaos: ChaosStore,
    /// Passed to `Storage::open_with_store` at every open.
    pub(super) put_sizes: PutSizes,
}

pub(super) struct Harness {
    dir: tempfile::TempDir,
    config: StorageConfig,
    /// `None` in local mode.
    remote: Option<Remote>,
    life: Option<Life>,
}

impl Harness {
    /// Local mode, as in M1.
    pub(super) fn new() -> Self;
    /// S3 mode: `inner` behind a `ChaosStore`, the read cache on.
    /// `put_sizes`: `MODEL_PUT_SIZES` over `InMemory`, `REMOTE_PUT_SIZES` over a real bucket (R4.8).
    pub(super) fn new_s3(inner: Arc<dyn ObjectStore>, put_sizes: PutSizes) -> Self;
    pub(super) fn chaos(&self) -> Option<&ChaosStore>;
    /// The bytes of object `key`, read past the chaos store and the read cache:
    /// `<dir>/objects/<key>` in local mode, `chaos.inner()` in S3 mode.
    pub(super) fn object_bytes(&self, key: &str) -> Bytes;
}

/// `model_config` plus the remote settings.
pub(super) fn model_config_s3(dir: &std::path::Path) -> StorageConfig;

/// Sizes that make the model's files take both upload paths.
pub(super) const MODEL_PUT_SIZES: PutSizes = PutSizes { single_max: 16 * 1024, part: 8 * 1024 };
```

R4.8 adds `REMOTE_PUT_SIZES` next to it, for runs against a real bucket.

- **`open_life`** takes the remote: `open_life(config: &StorageConfig, remote: Option<&Remote>) -> Life`. With a remote, it opens with `Storage::open_with_store(config.clone(), Arc::new(remote.chaos.clone()), remote.put_sizes)`. The same `ChaosStore`, and so the same `InMemory`, survives every restart and crash. That is the bucket.
- **The ending window.** `restart` and `crash` call `chaos.set_ending(true)` before they end the old life (before `shutdown` or the drop of `Storage`). They call `chaos.set_ending(false)` after `runtime.shutdown_timeout(…)` returns and before `open_life`. An upload cancelled anywhere in between therefore counts as abandoned, not leaked (R4.2). That includes one the shutdown flush started and the 30 s cut-off cancelled.
- **Opening runs calm.** Before `Storage::open*`, `open_life` calls `chaos.set_active(false)` and restores the previous value (`is_active()`) after. A store error during the anchor rebuild fails the open by design (§ What we already know, item 3). R4.6 tests that case on its own, so the model never has to retry an open.
- **`model_config_s3(dir)`** is `model_config(dir)` with:
  - `query.read_cache_bytes = Some(ByteSize::mib(64))`, so the read cache is on, every data-file read goes through `CacheStore`, and flush and compaction write through to it;
  - `flush.upload_concurrency = 1`, `compaction.threads = 1` and `query.target_partitions = 1`, so most calls are sequential and a seed usually replays (R4.2 lists what remains concurrent).

  `object_store` stays `None`: `open_with_store` ignores it.

### Reading files in S3 mode

- **`test_support::object_rows(dir, file)`** reads `<dir>/objects/<path>` from disk. Split it: add `pub(crate) fn parquet_rows(bytes: Bytes) -> RecordBatch` with the decoding part, and make `object_rows` read the file and call it. Every existing caller keeps `object_rows`.
- **`check_settled_files`** (`model_tests/check.rs`) calls `parquet_rows(h.object_bytes(&file.path))` instead of `object_rows(h.dir(), &file)`. In S3 mode, `object_bytes` is `h.block_on(chaos.inner().get(&ObjectPath::from(key)))` followed by `.bytes()`. It reads `inner` directly, so neither faults nor the cache can change what the check sees.
- **The `traces` check** goes through `query.traces`, as in M1, and so through the read cache in S3 mode. That is the point: the cached path is checked.

### Changes to `settle`

M1's `settle` steps are:
1. restart;
2. advance the clock;
3. apply deletions;
4. compact;
5. `collect_obsolete(ZERO)`;
6. collapse the model;
7. physical checks (`check_settled_files`).

R4.4 changes them in two places:
- **Step 5 becomes two calls:** first `storage.collect_staged(Duration::ZERO)` (expect `Ok`), then M1's `collect_obsolete(Duration::ZERO)`. This applies in both modes. In local mode nothing is staged at that point. In S3 mode with faults, failed flushes and compactions leave staged names, and collecting them before the obsolete files keeps M1's `staged_paths()` empty check true. Nothing else runs during `settle`, so a zero age cannot catch a flush in progress.
- **After step 7, in S3 mode only,** run the two checks below. They stay S3-only until both M2 (which fixes the local GC-unlink leak, `00-design.md` § Audit, item 2) and N's B5 have merged. Enabling the audit in local mode is then a one-line follow-up that R4.10 does if both have merged, or leaves listed in § M4 § Open.

The two S3-mode checks:
1. **Orphan audit:**
   - Call `storage.collect_orphans(Duration::ZERO, true)` (N's B5, dry run), or the R4.4 helper if B5 has not merged (§ Conventions).
   - Assert `orphans == 0`, `recent_unknown == 0`, `missing == 0` and `missing_media == 0`.
   - Also assert there is no `obsolete` row left. `collect_obsolete(Duration::ZERO)` deleted every one; read `SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'` from `catalog.db` through a read-only sqlx connection (as `tests/s3.rs::largest_object` opens one).
   - `missing > 0` is lost data. `orphans > 0` is a leak: a staged name that was neither committed nor collected, or an obsolete object whose row is gone.
   - Once enabled in local mode (after M2), the audit there lists `<dir>/objects` through the storage's own local store.
2. **Read cache equals the store.** For every active file of the three tables:
   - `storage.objects.get_range(&file.path, Some(0..file.size))` (through `CacheStore`) equals `h.object_bytes(&file.path)`;
   - `storage.objects.read_cache()` is `Some`.

   The range must be bounded, with the size taken from the catalog's `FileMeta::size`: `CacheStore::get_opts` serves only bounded ranges of `tables/` keys from the cache, and passes an unbounded read straight to the inner store (`read_cache.rs`, the `(Some(name), Some(range), true)` arm). `get_range(path, None)` would therefore compare the store with itself.

   `objects` is a private field of `Storage`. `model_tests` is a child of the crate root, where `Storage` is defined, so it may read it.

**The fallback audit helper**, only when B5 has not merged: `test_support::collect_orphans_dry_run(data_dir: &Path, store: &dyn ObjectStore) -> OrphanCounts`, with B5's semantics:
- "needed": the `active` `data_files` paths, plus the media keys of projects not `dropping`/`dropped`;
- "known": `data_files` paths in any state, every media key and every `staged_objects` path;
- list `tables/` and `media/`, skip temp names;
- `orphans` = listed keys that are not known; `missing` / `missing_media` = needed keys that were not listed.

`OrphanCounts` has the four fields named as in B5's `OrphanReport`. The helper lists the store it is given. The model passes `chaos.inner()`, **not** the `ChaosStore`, so the audit sees neither faults nor list counts.

### Never listed

In S3 mode, `check_full` asserts `chaos.count(Call::List) == lists_by_audits`, and `chaos.count(Call::Copy) == 0`. `lists_by_audits` is a counter in the harness: `settle` reads `count(Call::List)` before and after the audit and adds the difference, because B5 lists through `raw_store()`, which is the `ChaosStore`. The fallback helper lists `chaos.inner()` directly, so it adds 0. The test pins spec 03 §5 ("the bucket is never listed") for everything except the operator's audit.

### The test (`model_tests/mod.rs`)

```rust
fn s3_cases() -> u32 {
    std::env::var("LLMOBS_MODEL_S3_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(4)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(s3_cases()))]

    /// The model test on a remote store behind the read cache.
    #[test]
    fn storage_matches_the_model_on_s3(ops in ops::ops()) {
        run_case_s3(&ops, None);
    }
}

/// `faults`: `Some((seed, rates))` sets the fault schedule on the harness's
/// `ChaosStore` (`set_schedule(seed, rates)`, left inactive). R4.5 activates it
/// around each op.
fn run_case_s3(ops: &[Op], faults: Option<(u64, Rates)>);
```

R4.4 already reads `faults`: when it is `Some`, it calls `chaos.set_schedule(seed, rates)`. The schedule stays inactive until R4.5, so no fault fires yet. Add the unit test `a_schedule_without_activation_changes_nothing`: run a fixed op list with `Some((1, Rates::MODEL))`, and assert `chaos.injected() == 0`. That gives the parameter a caller in R4.4, so clippy has nothing to flag.

`run_case_s3` is `run_case` with `Harness::new_s3(Arc::new(InMemory::new()), MODEL_PUT_SIZES)`. Factor the shared loop into `fn run_with(harness: Harness, ops: &[Op])`, called by both, so the two modes cannot drift.

Add `s3_mode_uses_both_upload_paths`, a unit test with a fixed op list: append 12 spans to each of 6 traces of A, `Restart`, `Compact { hours: 60 }`, `Settle`. It asserts `chaos.count(Call::Put) > 0` and `chaos.count(Call::Complete) > 0`. If either is 0, lower or raise `MODEL_PUT_SIZES.single_max` until both are positive, and report the value and the file sizes seen (`catalog_files(...).size`).

### Commands

```bash
cargo test -p llmobs-storage --all-features model_tests
LLMOBS_MODEL_S3_CASES=100 cargo test --release -p llmobs-storage --all-features model_tests::storage_matches_the_model_on_s3
```

Both pass. A failure without faults is a bug of the S3 path itself (cache, write-through, multipart). Record it with its shrunk ops for R4.10, and move the proptest seed file out of the tree as in M1's R1.3.

---

## R4.5 — The model's S3 mode with faults

Commit: `storage: inject S3 faults into the model test (R4.5)`

### Faults during ops, calm during checks

In `run_with`, when the harness has a `ChaosStore` with a schedule:
- `chaos.set_active(true)` just before `apply(op, …)`, and `set_active(false)` right after, **except for `Op::Settle`**, which runs with the schedule off from start to end. Settle must converge: its loops of deletion and compaction passes assert they finish within 10 passes;
- every `check_quick`/`check_full`, and the final `settle` after the op list, run calm (inactive). Checks compare reads with the model; a read that fails there would be noise. Reads *under* faults are an op of their own (`FaultyRead`, below);
- the schedule is off before the `Harness` drops. Its `Drop` shuts the storage down cleanly, and that must not meet faults. `run_with` calls `set_active(false)` after the loop and in the panic path (§ On failure) before the harness goes out of scope.

`proptest!` gains the seed: `fn storage_matches_the_model_on_s3(ops in ops::ops_s3(), seed in any::<u64>().no_shrink())`, calling `run_case_s3(&ops, Some((seed, Rates::MODEL)))`.

### What an op may do under faults

M1's `apply` asserts success. In S3 mode with faults, use this rule instead: **an op that touches the store may fail only if the chaos store injected a fault during it** (`chaos.injected()` before and after differ). Then:
- **`Compact`:** `report.failed > 0` or `Err` is allowed under that rule. Otherwise assert `failed == 0` as in M1. Write the rule as one helper, `fn allow_failure(h: &Harness, injected_before: u64) -> bool`, and use it at every assert below.
- **`ApplyDeletions`:** the same for `report.failed` and `Err`.
- **`Gc`:** `collect_obsolete` → `Err` allowed under the rule.
- **`Restart`:** `shutdown` must return `Ok` whatever the store does. A flush that cannot finish in 30 s is cancelled, and its rows stay in the WAL (spec 03 §2). The model does not change, since acknowledged rows survive in the WAL.
- **`Crash`:** unchanged.
- **Appends, scores, deletion registration, retention, `DropB`** don't touch the object store (the WAL and `catalog.db` only), except `DropB`'s media step, which has no media in the model. They must succeed as in M1.

The model never changes on a store failure. Every model rule is about acknowledged writes, and none of them depends on a flush, compaction or GC succeeding.

### New ops (S3 mode only)

Add three variants to `Op`. Zero weights can't be used to keep them out of local mode: `prop_oneof!` builds a `Union::new_weighted`, which panics on a weight of 0. So R4.5 replaces M1's `prop_oneof!` in `ops.rs` with one weighted table and a mode:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode { Local, S3 }

/// Every op with its weight; the S3-only entries only in `Mode::S3`.
fn op_strategy(mode: Mode) -> impl Strategy<Value = Op> {
    let mut table: Vec<(u32, BoxedStrategy<Op>)> = vec![ /* M1's entries, same weights */ ];
    if mode == Mode::S3 {
        table.push((2, any::<u8>().prop_map(|pick| Op::Verify { pick }).boxed()));
        table.push((2, Just(Op::EvictCache).boxed()));
        table.push((4, project().prop_map(|project| Op::FaultyRead { project }).boxed()));
    }
    proptest::strategy::Union::new_weighted(table)
}

pub(super) fn ops() -> impl Strategy<Value = Vec<Op>> { proptest::collection::vec(op_strategy(Mode::Local), 1..=24) }
pub(super) fn ops_s3() -> impl Strategy<Value = Vec<Op>> { proptest::collection::vec(op_strategy(Mode::S3), 1..=24) }
```

`project()` is the A-weighted 3, B-weighted 1 strategy of M1, factored out if it is inline today. M1's weights and its op list must not change in local mode: check that `ops()` generates the same variants with the same weights as before. The strategy's internal shape does change, so seeds saved in `crates/storage/proptest-regressions/model_tests/mod.txt` before this switch may not replay the same ops. Each saved regression that matters is already a named `regression_*` unit test (M1's R1.8), so that is acceptable. Say in the report whether old seeds still reproduce.

`apply` with one of the three new ops on a local-mode harness (`h.chaos()` is `None`): `panic!("S3-only op {op:?} in local mode")`. Neither strategy generates that, so the panic only fires on a hand-written op list that is wrong.

```rust
/// Puts one active file (`pick % len`, in path order over the three tables)
/// in the verify queue, then runs one verify pass under faults.
Verify { pick: u8 },
/// Drops every block of the read cache, so the next reads go to the store.
EvictCache,
/// One trace list of `project` under faults.
FaultyRead { project: P },
```

`Op::project()` returns `Some(project)` for `FaultyRead` and `None` for the other two.
- **`Verify`:**
  - `storage.verify_queue().suspect([file.id], catalog::now_ns())`, then `storage.verify_pass().await`, with faults active.
  - Assert `report.quarantined == 0` and `report.repaired == 0`: no fault the schedule injects may make a healthy file look corrupt. Store errors must come out as `unavailable` (spec 03 §10).
  - Afterwards the file is still in `catalog_files` (calm read).
  - A file verified healthy within the hour is not queued again (`HEALTHY_FOR`); `suspect` then returns 0, which is fine.
- **`EvictCache`:** for every active file, `cache.evict_file(&cached_name(&ObjectPath::from(path)).expect(…))` on `storage.objects.read_cache()`. Calm, since it is local.
- **`FaultyRead`:**
  - Call `try_observe_traces(h, project, scope)` with faults active.
  - The result must be either `Err` with `allow_failure` true, or `Ok(rows)` equal to what `check_quick` expects, compared by the same function.
  - Refactor M1's `observe_traces` into `try_observe_traces -> Result<Vec<ExpectedTrace>, String>` plus `observe_traces`, which `expect`s it. Refactor `check_quick`'s trace comparison into `fn compare_traces(model: &ProjectModel, observed: &[ExpectedTrace], index: usize, project: P)`, so both callers share it.

### Checks added to `check_full` (S3 mode)

- **`chaos.leaked_uploads()` is empty.** After R4.3, at a calm point, no multipart upload is `Open` or `CompleteFailed`, and none was dropped open outside a life's ending window (R4.2).
- **`chaos.abandoned_uploads()`** may be non-empty. That covers a failed abort, a lost create answer, and an upload cancelled by a `Crash` or by the 30 s flush cut-off of a `Restart`. Cancellation is expected behaviour: the lifecycle rule handles it. Print the count in the failure message only, and add it to the run statistics below.
- **Never listed:** as in R4.4.

### On failure

`run_with` wraps the loop in `std::panic::catch_unwind(AssertUnwindSafe(...))`. On a panic it:
1. calls `chaos.set_active(false)`;
2. prints `chaos.journal()`, the seed and the op index;
3. resumes the panic with `std::panic::resume_unwind`.

proptest then shrinks the ops, and the seed stays fixed because of `no_shrink()`. Because a seed may not replay exactly (R4.2), the journal is what R4.10 turns into a scripted regression.

### Statistics

At the end of each case, add to a process-wide `static STATS: Mutex<S3Stats>`:
- cases;
- faults injected;
- puts, multipart completes and aborts;
- abandoned uploads;
- `FaultyRead` errors.

When `LLMOBS_MODEL_S3_STATS=1`, `run_case_s3` prints `STATS` every 50 cases. It is for `hardening.md`; don't build more than that.

### Budget

Measure as M1 did: time `storage_matches_the_model_on_s3` in debug with `LLMOBS_MODEL_S3_CASES=2`. Then set the default in `s3_cases()` to the largest of 8, 4 or 2 that keeps the test under 60 s, and report the time and the value. Each failed flush costs at least 1 s of real backoff. If the budget is missed at 2 cases:
1. first halve `Rates::MODEL.before` and `after`;
2. then stop and report.

### `justfile` and `AGENTS.md`

```just
# The storage model test on a remote store with S3 faults, in release mode (R4.5).
model-s3 cases="200":
    LLMOBS_MODEL_S3_CASES={{cases}} LLMOBS_MODEL_S3_STATS=1 cargo test --release --workspace --all-features -- model_tests::storage_matches_the_model_on_s3
```

In `AGENTS.md`:
- § Commands, table row: `just model-s3 [cases]` | the storage model test on a remote store with injected S3 faults, release mode (R M4);
- § Tests, one bullet: `LLMOBS_MODEL_S3_CASES=N` sets the S3-mode model test's cases (default by measurement, R4.5).

### Docs

`docs/research/storage/hardening.md`, new section `## M4 — S3 fault injection`:
- **What the S3 mode covers:** `ChaosStore` faults, calm checks, `FaultyRead`/`Verify`/`EvictCache`, the upload ledger, the audit, never listed;
- **`### Runs`:** the same table as M1, with this sub-phase's `just model-s3 200` as the first row, plus the statistics;
- **`### Findings`:** "None yet." Or, for R4.3's confirmed multipart leak, row `M4-1` (test names, cause, fix commit).

### Commands

```bash
cargo test -p llmobs-storage --all-features model_tests
just model-s3 200
```

---

## R4.6 — Focused fault tests

Commit per test group: `storage: test <what> under S3 faults (R4.6)`. A fix gets its own commit with its regression test (D-R-2).

These are scenarios the model cannot reach, or reaches too rarely to rely on. All of them are in `crates/storage/src/s3_fault_tests.rs`, a new `#[cfg(test)] mod s3_fault_tests;` in `lib.rs` next to `quarantine_tests`. They run on `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` with `Storage::open_with_store(config, Arc::new(chaos.clone()), PutSizes { single_max: 16 * 1024, part: 8 * 1024 })` over `InMemory`. The config is `lib.rs`'s `cached_config` pattern: `config_in`, plus `read_cache_bytes = Some(64 MiB)`. Copy it, since it is private to `lib.rs`'s tests, or move it to `test_support` if both need it.

1. **Open under store errors.** `open_fails_on_a_store_error_and_a_later_open_succeeds`:
   - append, `shutdown` (files exist);
   - **delete the read cache:** `std::fs::remove_dir_all(dir.join("cache"))`. The anchor rebuild reads through `CacheStore` (`anchor.rs::rebuild_file` uses `objects.store()`), and flush wrote the files through to the cache, which survives the restart. With the cache in place, the open would never reach the `ChaosStore`;
   - `chaos.hold(&[Call::Get, Call::Head])`, then `Storage::open_with_store` → `Err`. Assert that `chaos.count(Call::Get) + chaos.count(Call::Head) > 0`, which proves the open reached the store. Find the variant (`OpenError::…` from the anchor rebuild) in `error.rs` and assert it. If the open panics instead, that is a finding: fix it, and stop if the fix is not local;
   - `release()`; open again → `Ok`; the rows are readable.

   If the second open fails with `Locked`, the failed open leaked the `LOCK`: that is a finding, fixed in its own commit.

   The behaviour is decided (user, 2026-11-15): open fails on a store error. Add a row to spec 03 §10: "Object store errors at open | The anchor rebuild reads recent `traces` files; a store error, after the client's retries, fails the open. A later open retries. Acknowledged data stays in the WAL."
2. **Verification never quarantines on store errors.** `verification_keeps_a_file_the_store_fails_to_read`, one case per fault:
   - `hold(&[Call::Head])`;
   - `hold(&[Call::Get])`;
   - `push(Call::Get, Fault::Truncated)` on every `Get` of the pass (push 64).

   Each case: suspect one flushed file, `verify_pass()` → `unavailable == 1`, `quarantined == 0`; `release()`, which also drops the `Truncated` faults the pass did not use (R4.1); the next pass → `healthy == 1`.
3. **A bad block in the read cache.** First the confirmation test `a_corrupt_read_does_not_stay_in_the_cache`:
   - flush one file, then evict its cache (`evict_file`);
   - `push(Call::Get, Fault::Corrupt)` once;
   - run a span read (as `a_flushed_file_is_cached_on_write` does). It may fail, or return rows; a corrupt byte may hit data the query doesn't decode. Either is acceptable for this one read;
   - mark the file suspect, `verify_pass()` → `healthy == 1` (verification reads the raw store);
   - **then** the same read again must return the right rows, and the cached bytes of the file must equal the object's bytes: compare `objects.get_range(path, Some(0..file.size))` with the bytes on `inner`. The range must be bounded, with `file.size` from the catalog, or the read bypasses the cache (R4.4).

   Expected today: the last step fails, because the corrupt block stays cached (§ What we already know, item 2).

   **Fix:** in `compaction/verify_pass.rs`, on `Verdict::Healthy`, drop the file's cached blocks: `if let Some(cache) = self.objects.read_cache() { cache.evict_file(&name) }`, with `name = cached_name(&ObjectPath::from(file.path.as_str()))`. `evict_file` is blocking file work: run it in `spawn_blocking`, as `CacheStore::delete_stream` does. The cost is one re-fetch of a file that was suspect anyway.

   **Spec 03 §6, S3 read cache**, last bullet becomes: "verification of a suspect file (§10) reads the S3 store directly, never the cache, and a file found healthy has its cached blocks dropped, so a bad block cannot outlive the check."

   If the confirmation test passes before the fix (because the cache rejected the block for another reason), don't fix: record why in the report.

   With the fix, add to `CHANGELOG.md` § `[Unreleased]` § `Fixed`: "With S3, a file that passes verification has its read-cache blocks dropped, so a bad cached block no longer keeps failing queries (track R)."
4. **Media under faults.** `media_put_under_faults_leaves_nothing_behind`:
   - `put_media` with `push(Call::Put, Fault::After(SlowDown))` → `Err`; the object exists on `inner`, its path is staged;
   - a second `put_media` of the same blob → `Ok` (overwrite, spec 03 §9);
   - `collect_staged(Duration::ZERO)`, then the orphan audit (B5 or the helper) is clean and the blob is readable with `get_media`.
5. **Flush backoff is bounded and grows.** `flush_retries_back_off_while_the_store_fails`:
   - `flush.max_age = 100 ms`, `flush.upload_concurrency = 1`, `hold(&[Call::Put, Call::MultipartStart])`, append one span. With one upload at a time, each attempt makes exactly one put (the `spans` file fails, and `put_all` returns before the `traces` file);
   - after 7.5 s of real time, `count(Call::Put)` is between 3 and 5. The `run` loop in `flush.rs` makes an attempt, then sleeps 1, 2, 4 and 8 s between attempts (spec 03 §2), so attempts start at about 0.1, 1.1, 3.1, 7.1 and 15.1 s;
   - release at 7.5 s. The next attempt is at about 15.1 s, so poll `catalog_files` for up to **12 s** after the release: the file is committed. `staged_paths().len() >= 2` then: the failed attempts' names stay staged (`spans` and `traces`, both staged before the upload), at least one attempt's worth;
   - `shutdown` returns within 35 s whatever happens.

   This is a timing test: keep the bounds loose as given, and if it is flaky, report rather than tighten.
6. **GC under faults.** `gc_retries_a_failed_delete`:
   - obsolete files: compact two L0 files. That obsoletes the two `spans` inputs and their two derived `traces` files, and possibly more; don't assume a number;
   - read `n = SELECT COUNT(*) FROM data_files WHERE state = 'obsolete'` from `catalog.db` (read-only sqlx connection), and assert `n >= 2`;
   - `push(Call::Delete, Fault::After(SlowDown))`: the first delete takes effect and then fails;
   - `collect_obsolete(ZERO)` → `Err`. It stops at the first error (`gc.rs::collect_obsolete`, the `?` after `collect_obsolete_file`). Still `n` obsolete rows; exactly one of their objects is gone from `inner`;
   - the next `collect_obsolete(ZERO)` → `Ok` with `deleted == n`: `delete_len` sees `NotFound` for the one already gone and reports 0 bytes for it. No obsolete row is left, and the audit is clean.

   Also `staged_collection_continues_after_one_failed_delete`: two staged orphans, `push(Call::Delete, Fault::Before(..))` once, `collect_staged(ZERO)`. Today `collect_once` returns at the first error (`?` on `collect_staged_path`). Check what it does with the second path: if the second path is left for the next pass only, that is acceptable (the next pass collects it); assert the next pass leaves `staged_paths()` empty. Don't change the early return unless a test shows a path is never collected.
7. **Blocking spill reads** (§ What we already know, item 4). No test can show a blocked worker reliably, so no test is added. If R4.10 has time, move the remote branch's file reads into `spawn_blocking` as the local branch does; otherwise list it under § Findings as "not fixed, no observed effect".

### Commands

```bash
cargo test -p llmobs-storage --all-features s3_fault_tests
```

---

## R4.7 — Toxiproxy in front of Versity Gateway

Commit: `storage: add the S3 chaos suite through toxiproxy (R4.7)`

### Setup (documented in the test file's module doc, as `tests/s3.rs` does)

```text
docker run --rm --name llmobs-s3 -p 7070:7070 --tmpfs /data:rw,mode=1777,size=4g -e ROOT_ACCESS_KEY=llmobs -e ROOT_SECRET_KEY=llmobs-secret -e VGW_BACKEND=posix -e VGW_BACKEND_ARGS=/data -e VGW_ARGS="--port :7070" versity/versitygw:v1.11.0
curl --aws-sigv4 "aws:amz:us-east-1:s3" --user llmobs:llmobs-secret -X PUT http://127.0.0.1:7070/llmobs-test
docker run --rm --name llmobs-toxiproxy --net=host ghcr.io/shopify/toxiproxy:2.13.0 -host 127.0.0.1
```

Pass `-host 127.0.0.1` to the server (append it after the image name; the image's entrypoint is the server), so the API (`:8474`) and the proxies listen on loopback only. The image otherwise binds `0.0.0.0` under `--net=host`. Both settings were checked against v2.13.0:
- the server's flags (`cmd/server/server.go`: `-host`, default `localhost`, and `-port`, default `8474`);
- its README, which documents an optional `TOXIPROXY_AUTH_TOKEN` (every request then needs `Authorization: Bearer <token>`).

With `-host 127.0.0.1` no token is needed. If one is used anyway, set the same value in `LLMOBS_TEST_TOXIPROXY_TOKEN`, and the test sends the header. Re-check both when M4 runs, together with the image tags. Without Docker, run the release binary `toxiproxy-server-linux-amd64` (v2.13.0) with `-host 127.0.0.1`.

### `crates/storage/tests/s3_chaos.rs`

Every test is `#[ignore = "needs toxiproxy and an S3 endpoint: just s3-chaos"]`, and also skips (prints and returns) when `LLMOBS_TEST_S3_ENDPOINT` or `LLMOBS_TEST_TOXIPROXY` is unset. Environment:
- `LLMOBS_TEST_S3_ENDPOINT`: Versity's address (`http://127.0.0.1:7070`);
- `LLMOBS_TEST_S3_BUCKET`;
- `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`;
- `LLMOBS_TEST_TOXIPROXY`: the API (`http://127.0.0.1:8474`);
- `LLMOBS_TEST_TOXIPROXY_TOKEN` (optional).

A small client over `reqwest` (new dev-dependency, § Conventions):

```rust
struct Toxiproxy { api: String, token: Option<String>, http: reqwest::Client }
impl Toxiproxy {
    /// POST /proxies {name, listen: "127.0.0.1:0", upstream, enabled: true}; returns the
    /// actual `listen` address from the response (port 0 picks a free one).
    async fn proxy(&self, name: &str, upstream: &str) -> String;
    /// POST /proxies/{proxy}/toxics {name, type, stream, toxicity, attributes}.
    async fn toxic(&self, proxy: &str, name: &str, kind: &str, stream: &str, toxicity: f64, attributes: serde_json::Value);
    /// DELETE /proxies/{proxy}/toxics/{name}.
    async fn remove(&self, proxy: &str, name: &str);
    /// POST /proxies/{proxy} {"enabled": bool}: the `down` toxic.
    async fn enabled(&self, proxy: &str, on: bool);
    /// DELETE /proxies/{proxy}.
    async fn delete(&self, proxy: &str);
}
```

`upstream` is the endpoint's `host:port`. Each test uses its own proxy, `llmobs-<test name>`:
- it first calls `delete(name)`, ignoring a 404, so a proxy left behind by an earlier failed run can't make `proxy()` fail with 409;
- it creates the proxy, points the storage config's `s3.endpoint` at `http://<listen>`, and uses a fresh prefix `s3-chaos-<test>-<ms>`;
- at the end it deletes the proxy, then the prefix, with `delete_prefix` copied from `tests/s3.rs`, calling Versity directly, not through the proxy.

The storage config is `tests/s3.rs::config` with `flush.max_age = 1 s`, `memtable.hard_limit = 64 MiB`, `flush.max_bytes = 16 MiB`. Integration tests can't reach `#[cfg(test)]` items, so they use the public API only: `Storage::open`, `append_spans`, `query`, `health()`, `collect_staged`, `collect_obsolete`, `compact`, `shutdown`, and `collect_orphans` once B5 has merged (before that, skip the audit here and say so in the report).

**Incomplete uploads.** `fn incomplete_uploads(endpoint, bucket, prefix) -> usize` runs `curl -s -K - --aws-sigv4 "aws:amz:<region>:s3" "<url>"` and counts `<Upload>` in the XML body. `ListMultipartUploads` is not in `object_store`'s API.
- **The keys:** it writes `user = "<key>:<secret>"` to curl's stdin (`-K -`), so they never appear on a command line.
- **The URL** follows `LLMOBS_TEST_S3_PATH_STYLE` (R4.8; default `true`):
  - path style: `<endpoint>/<bucket>?prefix=<prefix>&uploads`;
  - virtual-hosted: `<scheme>://<bucket>.<endpoint host>/?prefix=<prefix>&uploads`.

  The query parameters are in sorted order, so the canonical query string curl signs matches the URL. The prefix is percent-encoded, though test prefixes only use `[a-z0-9-]`.

Tests. Each one ends with `shutdown` (asserted to return within 75 s), then `incomplete_uploads == 0`, retried every second for up to 10 s (an abort sent just before shutdown may still be in flight at the gateway). If a count stays above 0 after `reset_peer` (test 2) or `limit_data` (test 4), the client lost an abort or a create answer. That is expected there, and recorded as a finding with the count rather than failing the test, since the lifecycle rule covers it. In the other tests it fails. and the span count read through a fresh `Storage::open` **directly against Versity** equals what was acknowledged.
1. `latency_and_jitter`:
   - `latency` 200 ms ± 100 ms, on both `downstream` and `upstream`;
   - 4 appends, wait for the flush;
   - a query returns 4 rows within 30 s.
2. `reset_peer_on_some_connections`:
   - `reset_peer` (`timeout: 0`), `upstream`, toxicity 0.3;
   - 20 appends of 1 MiB each (`spans(16, 64 * 1024)` from `tests/s3.rs`), then a compaction pass (`compact(now)` with a `now` past `close_delay`, as `tests/s3.rs` builds its times) and a multipart upload (the 12,000-row batch of `tests/s3.rs`).
   - Everything commits: the client retries resets on puts, parts, `complete`, `GET` and `HEAD`, which are all idempotent or safe. `object_store` logs its retries at `info` (`"Encountered transport error …"`): run with `RUST_LOG=object_store=info` and count them for the report.
   - Then run GC under the same toxic: `collect_obsolete(Duration::ZERO)` in a loop, at most 10 times, until it returns `Ok` with nothing left. Its deletes are bulk `POST ?delete`, which the client does not retry when a connection is reset mid-request (§ What we already know, item 5), so a pass may fail. Expected: some passes return `Err` and a later one succeeds, and the obsolete rows (catalog count, as in R4.6 test 6) reach 0. Record how many passes failed: that is finding `M4-n` (delete not retried on interrupted connections; harmless because GC retries).
3. `slicer_small_packets`: `slicer` `{average_size: 64, size_variation: 32, delay: 10}`, downstream. A round trip whose reads go cold to S3 through the cache.
4. `limit_data_cuts_large_bodies`:
   - `limit_data` `{bytes: 1048576}`, upstream, toxicity 0.5;
   - the multipart round trip of `tests/s3.rs`;
   - every part over 1 MiB is cut half the time, the client retries, and the upload completes.
5. `blackhole_then_recovery` (the `timeout` toxic, `timeout: 0`, both streams):
   - append, add the toxic, keep appending 1 MiB batches until `append_spans` returns `Backpressure`;
   - assert `health().memtable_bytes <= memtable_hard_limit + 2 MiB` (bounded memory);
   - after 45 s, `catalog_rows` (count rows through a fresh read-only sqlx connection to `catalog.db`, as `largest_object` does) is unchanged since the toxic;
   - remove the toxic. Within **6 minutes** the memtable drains (an append succeeds) and the data is committed.

   The bound: a request in flight can retry for up to `retry_timeout` (3 min), plus one 30 s timeout, plus 15 s of backoff, then the flush's own backoff (up to 60 s). Record the measured recovery time.
6. `down_during_shutdown`:
   - append, `enabled(false)` (connections refused), `shutdown()`;
   - it returns within 75 s (30 s for compaction plus 30 s for flushes, spec 03 §2);
   - re-enable, reopen through the proxy: the rows replay from the WAL and are readable.
7. `bandwidth_and_large_single_put`:
   - this test overrides the config: `flush.max_bytes = 256 MiB` (the default) and `memtable.hard_limit = 1 GiB`, so the memtable is not cut into 16 MiB flushes;
   - `bandwidth` `{rate: 1024}` (KB/s), upstream;
   - append `spans(6_000, 8 * 1024)` in one batch, aiming at one file between 40 and 64 MiB (a single put).

   **Check the size the test actually produced.** After the toxic is removed and the flush commits, read `largest_object(&config.data_dir)` (copied from `tests/s3.rs`) and assert `40 MiB <= size <= 64 MiB`. If it falls outside, adjust the row count (compression decides the size) and report the value.

   At 1 MiB/s the PUT needs over 40 s, more than the client's 30 s request timeout, so every attempt times out and is retried. **Expected outcome: the flush never commits while the toxic stays.** Keep the toxic for 3 minutes: `catalog_rows` (read-only sqlx, as test 5) stays unchanged, and `shutdown` is not called meanwhile.

   Then remove the toxic. The flush must commit within **6 minutes**: poll `catalog_rows` every 5 s and fail at the deadline. The bound is the same as test 5's: one request's remaining retries (up to 3 min plus a 30 s timeout plus 15 s), then the flush backoff (up to 60 s). Record the measured time.

**The likely finding of test 7.** A slow link can make every single-put attempt time out, because a file up to 64 MiB goes out in one request with a 30 s timeout, while 16 MiB parts would each fit. Record it in `hardening.md` as `M4-n`. The decision (user, 2026-11-15) is to choose after this measurement: if it is confirmed, R4.10 lowers `PutSizes::S3.single_max` to 16 MiB. Don't change it in R4.7.

### `justfile` and `AGENTS.md`

```just
# S3 chaos through toxiproxy in front of an S3 endpoint (R M4). Needs
# LLMOBS_TEST_S3_ENDPOINT, LLMOBS_TEST_S3_BUCKET, AWS_ACCESS_KEY_ID,
# AWS_SECRET_ACCESS_KEY and LLMOBS_TEST_TOXIPROXY; see crates/storage/tests/s3_chaos.rs.
s3-chaos:
    cargo test --release --workspace --all-features --test s3_chaos -- --ignored --test-threads 1 --nocapture
```

In `AGENTS.md`:
- § Commands: `just s3-chaos` | S3 fault tests through toxiproxy in front of an S3 endpoint (R M4);
- § Tests: "`LLMOBS_TEST_TOXIPROXY` (with `LLMOBS_TEST_S3_*` and optionally `LLMOBS_TEST_TOXIPROXY_TOKEN`) runs `just s3-chaos`; `crates/storage/tests/s3_chaos.rs` shows how to start toxiproxy and Versity Gateway."

### Run

Run `just s3-chaos` once. Record in `hardening.md` § M4 § Runs:
- the machine;
- the toxiproxy and Versity versions;
- per test, pass or fail, duration and the retry counts.

Each failure is a finding, fixed in R4.10.

---

## R4.8 — Real S3: Hetzner Object Storage

Commit: `storage: run the S3 suites against a remote provider and probe it (R4.8)`

The user provides a bucket when M4 starts: location (`fsn1`, `nbg1` or `hel1`), bucket name, and keys. They go **only** into environment variables of the shell that runs the tests.

### Settings the tests read

`tests/s3.rs` hard-codes `region: "us-east-1"` and `force_path_style: true` (right for Versity). Add, in `tests/s3.rs::config` and `delete_prefix`:
- `LLMOBS_TEST_S3_REGION` (default `us-east-1`);
- `LLMOBS_TEST_S3_PATH_STYLE` (default `true`; `false` for Hetzner, as `docs/self-hosting/s3.md` says).

`s3_chaos.rs` and `s3_probe.rs` use the same two variables. `AGENTS.md` § Tests: extend the `LLMOBS_TEST_S3_ENDPOINT` bullet with "`LLMOBS_TEST_S3_REGION` and `LLMOBS_TEST_S3_PATH_STYLE` adapt it to a remote provider (R M4)".

Hetzner settings: endpoint `https://<location>.your-objectstorage.com`, region `<location>`, path style `false`.

### The model against the real bucket

`LLMOBS_MODEL_S3_REMOTE=1` makes `storage_matches_the_model_on_s3` build its `inner` from the `LLMOBS_TEST_S3_*` variables, instead of `InMemory`:
- `AmazonS3Builder::from_env()` with bucket, endpoint, region and path style;
- wrapped in `PrefixStore::new(store, format!("model-{ms}-{case}"))`;
- with **remote part sizes**: `Harness::new_s3(inner, REMOTE_PUT_SIZES)`, where `pub(super) const REMOTE_PUT_SIZES: PutSizes = PutSizes { single_max: 16 * 1024, part: 5 * 1024 * 1024 };` is added in `harness.rs`, used only on this path.
  - Real S3 and Hetzner refuse a non-last part under 5 MiB (`EntityTooSmall`; `object_store/src/upload.rs` documents the rule).
  - With these sizes, a file above 16 KiB still takes the multipart path, as one last part, which every provider accepts.
  - `InMemory` runs keep `MODEL_PUT_SIZES`, with its 8 KiB parts.

At the end of each case it lists the prefix and deletes every object. Uploads that the case left incomplete (abandoned ones: a failed abort, a lost create answer, a cancellation) can't be listed through `object_store`. They stay on the bucket until the lifecycle rule removes them, a day later. That is expected, and the reason the rule is applied before R4.8 runs. The schedule still runs on top. Add `AGENTS.md` § Tests: "`LLMOBS_MODEL_S3_REMOTE=1` runs the S3-mode model test against the `LLMOBS_TEST_S3_*` bucket."

Run `LLMOBS_MODEL_S3_REMOTE=1 just model-s3 20`. Network latency makes a case take tens of seconds; 20 cases is the target. Report the time.

### Probes (`crates/storage/tests/s3_probe.rs`)

Every test is skipped without `LLMOBS_TEST_S3_ENDPOINT`, like `tests/s3.rs`, and uses a prefix `probe-<ms>`, deleted at the end. Each test prints one line `probe <name>: <result>`, for `hardening.md`.

What our code **relies on** is asserted. What is only information is printed.

| Test | What | Assert or print |
|---|---|---|
| `read_after_write` | `put`, then immediately `get` and `head` of 100 fresh keys | **assert** every read succeeds with the right bytes and size (flush then query, and `verify_file`'s `head`) |
| `delete_missing_is_ok` | `delete` of a key never written | **assert** `Ok` (GC deletes idempotently) |
| `head_after_delete_is_not_found` | put, delete, `head` | **assert** `NotFound` (GC's `delete_len`, verification's missing-object rule) |
| `list_after_write` | put 50 keys, `list` the prefix | **assert** all 50 listed, each once (N's `collect_orphans` and the audit rely on it) |
| `abort_after_complete_keeps_the_object` | multipart (2 parts of 5 MiB + a short last), `complete`, then `abort` on the same upload | **assert** the object stays readable; print abort's result (`NotFound`, i.e. `NoSuchUpload`, or `Ok`). R4.3's fix relies on this |
| `multipart_part_sizes` | parts of 16 MiB (ours), then an upload with a 4 MiB non-last part | **assert** ours completes; print whether the 4 MiB one is refused (`EntityTooSmall`) |
| `multipart_part_count` | an upload of 40 parts of 5 MiB (above our maximum: `compaction.target_file_size` 512 MiB / 16 MiB = 32 parts) | **assert** it completes |
| `conditional_put` | `put_opts(PutMode::Create)` twice on one key (the builder's default is `S3ConditionalPut::ETagMatch`) | print: `AlreadyExists` on the second, or `NotImplemented`/other. Storage doesn't use it today; it's information for a future multi-node catalog (spec 03 §11) |
| `incomplete_upload_is_listed` | start an upload, put one part, don't complete; `incomplete_uploads()` (curl, as R4.7) | **assert** it is listed; then abort it and assert it is gone |
| `lifecycle_rule_present` | `GET ?lifecycle` through curl | print the rule; **assert** an `AbortIncompleteMultipartUpload` rule exists when `LLMOBS_TEST_S3_EXPECT_LIFECYCLE=1` |

`lifecycle_rule_present` builds its `GET ?lifecycle` URL with the same path-style rule as `incomplete_uploads` (R4.7).

Decided (user, 2026-11-15): the user applies the lifecycle rule once, before R4.8 runs, with the AWS CLI as Hetzner documents (`aws s3api put-bucket-lifecycle-configuration --bucket <bucket> --lifecycle-configuration file://lifecycle.json --endpoint-url https://<location>.your-objectstorage.com`). `lifecycle.json`:

```json
{ "Rules": [ { "ID": "abort-incomplete-uploads", "Status": "Enabled", "Prefix": "",
  "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 1 } } ] }
```

R4.8 does not apply it from the test (putting a lifecycle configuration needs a `Content-MD5` or checksum header, which `object_store` doesn't offer).

**The lifecycle check, over days.** `leave_an_incomplete_upload` (ignored even with the env set; run it by name) starts one upload under `lifecycle-probe/` and prints its upload id. R4.10 checks, two days later, that `incomplete_uploads(.., "lifecycle-probe/")` is 0, and records it.

### Runs

With the Hetzner variables set:

```bash
cargo test --release -p llmobs-storage --all-features --test s3 -- --nocapture
cargo test --release -p llmobs-storage --all-features --test s3_probe -- --nocapture
LLMOBS_MODEL_S3_REMOTE=1 just model-s3 20
```

Don't run `just s3-chaos` against Hetzner. Pointing toxiproxy at `<location>.your-objectstorage.com:443` and the endpoint at the proxy would break the TLS host check, so toxiproxy runs against Versity only.

Record each run in `hardening.md` § M4 § Runs: provider, location, date, durations, and every probe line. Do not record the bucket name. A failed assertion is a finding for R4.10.

---

## R4.9 — Self-hosting: the lifecycle rule

Commit: `docs: recommend the abort rule for incomplete multipart uploads (R4.9)`

**Ownership.** `docs/self-hosting/` was written by track N (N11, implemented). Decided (user, 2026-11-15): R edits `docs/self-hosting/s3.md` directly. R's scope includes `docs/` (`00-design.md` § Cross-cutting rules).

Add to `docs/self-hosting/s3.md`, before "Moving an existing installation…":

```markdown
## Incomplete uploads

Large files are uploaded in parts. When an upload fails, llmobs aborts it, but an upload cut
short by a crash, a killed process or a lost network answer can stay on the bucket. Its parts are
invisible and billed until something removes them. Add a lifecycle rule that aborts incomplete
uploads after a day:

    {"Rules": [{"ID": "abort-incomplete-uploads", "Status": "Enabled", "Prefix": "",
      "AbortIncompleteMultipartUpload": {"DaysAfterInitiation": 1}}]}

Apply it with `aws s3api put-bucket-lifecycle-configuration --bucket <bucket>
--lifecycle-configuration file://lifecycle.json` (add `--endpoint-url` for a provider other than
AWS). Hetzner Object Storage, AWS S3, Cloudflare R2 and Garage support this rule. Use a prefix
filter if the bucket is shared. Do not add an `Expiration` rule to the llmobs prefix: llmobs
deletes its own files, and an expiring rule would delete data the catalog still needs.
```

Then:
- **Garage** (the compose example): check that it supports `AbortIncompleteMultipartUpload` (Garage's docs, `garage bucket` CLI or its S3 compatibility table). If it doesn't, change the support sentence to name only the providers that do, and report it.
- **The "list" permission:** `s3.md` says the key needs to list objects. Storage itself never lists, but N's `collect_orphans` and backups do, so leave that sentence as it is.

Spec 03 §10: append to the *Ambiguous upload* row: "Uploads no abort reached are left to the bucket's lifecycle rule (`docs/self-hosting/s3.md`)."

`CHANGELOG.md` § `[Unreleased]` § `Added`: "The S3 self-hosting guide recommends a lifecycle rule that aborts incomplete multipart uploads (track R)."

---

## R4.10 — Long runs and fixes

Commit per bug: `storage: <fix in imperative> (R4.10)`. A final docs commit: `docs: record the M4 runs (R4.10)`.

1. Run `just model-s3 2000`, three times, each with a fresh `PROPTEST_RNG_SEED`. Print it in the report. Start each in the background with its output in a log file (`just model-s3 2000 > <scratchpad>/model-s3-1.log 2>&1`); one run takes hours.
2. Run `just s3-chaos` three times in a row.
3. Two days after R4.8, check the `lifecycle-probe/` upload is gone (R4.8). Record the result.
4. **Single-put size** (decided by the user, 2026-11-15, to follow R4.7's measurement). If R4.7 test 7 confirmed that a large single put never completes on a slow link, set `PutSizes::S3.single_max` to `16 * 1024 * 1024`, so anything above one part goes multipart.
   - Commit: `storage: upload files above 16 MiB in parts (R4.10)`.
   - Rerun R4.7 test 7: it now commits under the toxic. Update its expectation in the same commit.
   - Update the `PutSizes::S3` doc comment and spec 03 §2 step 4 ("S3: the PUT, or the multipart upload above 16 MiB, has completed").
   - Add a `CHANGELOG.md` § `Changed` line: "With S3, files above 16 MiB are uploaded in 16 MiB parts, so a slow link no longer makes a large upload time out (track R)."

   If R4.7 did not confirm the finding, change nothing and record that.
5. **The audit in local mode.** If M2 and N's B5 have both merged, run R4.4's orphan audit in local mode too (remove the S3-only gate), and run `just model 500` to check it. Otherwise list it under § M4 § Open.
6. **For each failure,** the same procedure as M1's R1.8:
   1. Keep the shrunk ops and seed: `crates/storage/proptest-regressions/model_tests/mod.txt`.
   2. Add a named regression test `regression_s3_<what>`. **Its default form is scripted:**
      - take the failing case's journal (printed on failure, R4.5);
      - turn each faulted entry `"{call}#{ordinal} … {fault}"` into `chaos.push_at(call, ordinal, fault)`;
      - run the shrunk ops with the schedule off: `run_case_s3_scripted(&[...], &[(Call, u64, Fault)])`, a small variant of `run_case_s3` that applies the pushes after `Harness::new_s3`.

      Use `run_case_s3(&[...], Some((seed, Rates::MODEL)))` only if the scripted form does not reproduce and the seeded one does, and say so.
   3. Classify it:
      - **generator or model** (fix the test and say which rule);
      - **`ChaosStore` injected something unrealistic** (fix `ChaosStore`, and justify with the S3 behaviour it now matches);
      - **storage** (fix in `crates/storage/src` when local, otherwise stop and report the case, the cause and the change it needs).
   4. Add a row to `hardening.md` § M4 § Findings, with these columns:
      - id `M4-<n>`;
      - test or shrunk case;
      - cause;
      - classification;
      - fix commit, or "question" for the ones that need the user.
7. When the three model runs and three chaos runs pass:
   - record them in § Runs;
   - update `docs/roadmap.md` row R: "M4 done";
   - list what is still open in § M4 § Open (for example the local-mode audit, if M2 or B5 has not merged).

---

## What not to build in M4

- **Listing faults.** Storage never lists; the never-listed check replaces them (§ Where faults appear).
- **`NotFound` for existing objects, and wrong bytes in the model:** see § Where faults appear. Wrong bytes only appear in R4.6's focused test.
- **A retry layer of our own over `object_store`.** The client retries; ours is the flush backoff. M4 measures both and does not add a third.
- **Abort-on-drop for multipart uploads.** A guard that spawns an abort when an upload future is dropped would not run after a crash, which is when uploads are cut. The lifecycle rule covers every case at once.
- **Startup cleanup of incomplete uploads.** `object_store` has no `ListMultipartUploads`, and the lifecycle rule does it.
- **New S3 config keys** (timeouts, retries). They are public config: raise them as questions with the evidence of R4.7 and R4.8.
- **Toxiproxy against Hetzner.** TLS through a TCP proxy with a different host breaks the certificate check; the network faults run against Versity only.
- **Concurrent background tasks in the S3 mode:** M6.
- **Media in the model:** M4's media coverage is R4.6 test 4. Modelling media needs a model of reference counting and the 1-hour age rule, which no read of M1 checks.

---

## Decisions and design text for the coordinator

This plan departs from D-R-8 and from `00-design.md` § M4 in two places (no listing faults; no wrong bytes in the model), and it adds three decisions. The coordinating session records them; the implementer of M4 does not edit `docs/mvp-decisions.md` or `00-design.md`.

**D-R-8, new text** (replaces the current row):

> | D-R-8 | S3 faults | **A test-only `ChaosStore` implementing `object_store`'s `ObjectStore` (it replaces `FaultStore`) injects the errors `object_store`'s S3 client returns once its own retries are exhausted. That means `Error::Generic { store: "S3" }` for latency, 503/500 before or after the effect, multipart failures (part, `complete` before or after the effect, `abort`), `head` and `get` errors, and bodies cut mid-stream. It never injects `NotFound` for an existing object (S3 is strongly consistent and verification would quarantine), nor listing faults (storage never lists the bucket; a test asserts that instead). It injects wrong bytes only in focused tests. Toxiproxy in front of Versity Gateway adds TCP faults, so the client's own retries run.** Then one run against a Hetzner Object Storage bucket the user provides when M4 starts (user); credentials in environment variables only. |

**New decisions** (numbers to be given by the coordinator):
- **Multipart abort.** Every failure after `CreateMultipartUpload` aborts the upload, best effort, including after an ambiguous `complete`: an abort cannot remove a completed object. Uploads no abort reaches are left to a bucket lifecycle rule (1 day), which `docs/self-hosting/s3.md` recommends. That covers a failed abort, a lost create answer, and an upload cut by a crash or a cancelled task. There is no abort-on-drop guard and no startup listing of uploads.
- **Read cache after verification.** A suspect file that verifies healthy has its read-cache blocks dropped (if R4.6 confirms the bug).
- **M4 choices by the user (2026-11-15):**
  - the single-put size is decided after R4.7 measures the slow-link case (16 MiB if confirmed);
  - R edits `docs/self-hosting/s3.md`;
  - `open` fails on a store error during the anchor rebuild, documented in spec 03 §10;
  - the user applies the lifecycle rule to the Hetzner bucket with the AWS CLI.

**`00-design.md` § M4, new text for the first bullet's list:**

> - latency (as `ThrottledStore`);
> - 503 SlowDown and 500 errors before or after the request takes effect (the "ambiguous upload" of spec 03 §2), as the S3 client returns them after its retries;
> - multipart failures (one part fails, `complete` fails, `complete` succeeds but reports an error, `abort` fails);
> - `head` and `get` errors, and bodies cut mid-stream. Wrong bytes appear only in focused tests: over TLS they are not a realistic S3 answer, and the model could not tell them from right ones;
> - no listing faults: storage never lists the bucket, and the model asserts it.

And in its **Checks** list, replace "every multipart upload we start is completed or aborted" with: "every multipart upload we start is completed or aborted, except those only a lifecycle rule can remove (a failed abort, a lost create answer, a cancelled task)".
