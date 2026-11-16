# 02: `/readyz` checks the object store, free disk and the storage flags

Read [00-design.md](00-design.md) first, especially § `/readyz` (D-N-30, D182).

**Goal:** `/readyz` implements spec 12. Besides `meta.db` and both WALs, it fails on:
- a failed local object fsync;
- a failed catalog;
- free disk under the ingest-refusal threshold;
- an unreachable object store. That probe is cached 10 s, and concurrent requests share one probe.

`/healthz` also fails when a **sticky** storage failure is set (D-N-35), so Kubernetes' liveness probe restarts the pod.

It also wires restore mode's object-deletion switch (D-N-34): the server opens the storage with `delete_objects = !config.restore_mode`.

**Edit only:**
- `crates/server/src/http/health.rs`;
- `crates/server/src/http/mod.rs` (the `health_probes` test, and `AppInner` for the cache);
- the `TestApp` construction in `crates/server/src/test_support/mod.rs` and the `AppInner` literal in `crates/server/src/serve.rs` (one new field each);
- `docs/spec/12-operations.md` (§ Process lifecycle, the `/readyz` bullet);
- `docs/spec/09-api.md` (row 13).

**Commit:** `Check the object store, free disk and storage flags in /readyz`

(The commit also changes `/healthz`; the subject stays as the 00-design table has it.)

**Needs:** 01 (`StorageHealth::disk_low`, `Storage::probe_objects`, `StorageConfig::delete_objects`) and 03 (`config.restore_mode`).

## Read first

- `crates/server/src/http/health.rs`, all of it (40 lines).
- `crates/server/src/http/mod.rs`: `AppInner`, and the `health_probes` test.
- `crates/storage/src/lib.rs`: `StorageHealth`, `Storage::probe_objects`, `ProbeError` (phase 01).

## Changes

### `health.rs`

1. **Module doc.** Replace the "Those checks arrive with the tracks that own them" sentence with the list of checks and their order (the table in 00-design § `/readyz`).
2. Delete `#![cfg_attr(not(test), allow(dead_code))]` if nothing in the module is dead any more. If something still is, keep it and say why.
3. **The cache:**

```rust
/// How long an object-store probe result is reused, success or failure (spec 12).
const PROBE_TTL: Duration = Duration::from_secs(10);
/// One probe's whole time budget.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The last object-store probe. The mutex is held across the probe, so concurrent
/// `/readyz` requests wait for one probe instead of each sending their own.
#[derive(Default)]
pub(crate) struct ReadyCache {
    last: tokio::sync::Mutex<Option<(Instant, bool)>>,
}

impl ReadyCache {
    /// `true` when the store is reachable. Calls `probe` only when the cached result is
    /// missing or older than [`PROBE_TTL`] at `now`.
    async fn reachable<F, Fut>(&self, now: Instant, probe: F) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = bool>,
    { … }
}
```

`reachable` takes the probe as a closure so the tests can count the calls and fake failures without a failing store. It is a plain generic function, not a trait.

4. **`AppInner`** gets `ready: crate::http::health::ReadyCache`, set to `ReadyCache::default()` in both places that build `AppInner`: `serve.rs` `bind` and `TestApp`. Use `grep -rn "AppInner {" crates/server/src` to find them.

5. **`healthz` (D-N-35).** `healthz` takes `State(state)` and reads `state.storage.health()` (no I/O). When any of `spans_wal_failed`, `scores_wal_failed`, `objects_sync_failed` or `catalog_failed` is set, it answers the 503 `busy` problem with detail `restart needed: ` followed by the failure names of the `/readyz` table, joined by `, ` (e.g. `restart needed: catalog`). Otherwise it answers `{"status":"ok"}` as today. Update its doc comment: "Process alive, and no storage failure that only a restart clears (D-R-6)." Check the router entry in `http/access.rs`: it passes `health::healthz` as a handler, which still works once the handler takes `State`.

6. **`readyz`.** The order is the 00-design table's:

```rust
let health = state.storage.health();
if health.objects_sync_failed { failed.push("object fsync"); }
if health.catalog_failed { failed.push("catalog"); }
if health.disk_low { failed.push("free disk"); }
let storage = Arc::clone(&state.storage);
let reachable = state.ready.reachable(Instant::now(), || async move {
    matches!(tokio::time::timeout(PROBE_TIMEOUT, storage.probe_objects()).await, Ok(Ok(())))
}).await;
if !reachable { failed.push("object store"); }
```

   - The store is probed even when other checks failed, so the cache stays warm. Then the existing 503 `busy` problem with `failed.join(", ")` is built.
   - On success the body is the existing `Probe { status: "ready" }`. (Phase 03 adds `restore_mode`; if 03 is already merged, keep its field.)
   - Use `tokio::time::Instant` (not `std`), so the cache can be tested with `tokio::time::pause()`.

### Restore mode: no object deletion (D-N-34)

`ServerConfig::storage_config()` (`crates/server/src/config.rs`) is the single source of the `StorageConfig` that both `serve.rs` and `TestApp` open. Set `delete_objects: !self.restore_mode` there, so both follow. Add `crates/server/src/config.rs` to what you edit, and test it there too: `storage_config().delete_objects` is `false` with `restore_mode` and `true` without.

Test, in `serve.rs`: `restore_mode_opens_storage_without_object_deletion`. Start through `bind` with `restore_mode = true` under `capture_logs`; the log contains `object deletion is off (restore mode)` (phase 01's line). Without restore mode it does not.

## Spec

`docs/spec/12-operations.md` § Process lifecycle: replace the bullet with:

> `/healthz`: the process is alive, and no storage failure that only a restart clears is set (a WAL, a local object fsync or a catalog commit failed, D-R-6); otherwise 503 `busy` naming them, so a liveness probe restarts the server (D-N-35). `/readyz`: `meta.db` answers, neither WAL has failed, no local object fsync or catalog commit has failed (D-R-6), free disk is above `storage.min_free_bytes`, and the object store answers (a HEAD of the marker `_probe/readyz`, or a PUT of it when the HEAD fails; 2 s timeout; the result is reused for 10 s; D-N-36). A failure answers 503 `busy` naming the failed checks (D182, D-N-30). Liveness probes use `/healthz`: a full disk or an unreachable bucket takes the server out of rotation but must not restart it.

`docs/spec/09-api.md` row 13: change `probes` to `probes; \`/readyz\` checks are listed in spec 12 § Process lifecycle`.

## Tests

**In `http/mod.rs`, extend `health_probes`:**
- The ready body is still `{"status":"ready"}`.
- The meta.db failure detail is still exactly `"meta.db"`. The local store is reachable, the disk is not low (TestApp's `min_free_bytes` must be 0 or small: check, and set it in TestApp if needed), and the flags are clear.

**In `health.rs`, a test module:**
- `sticky_failures_fail_liveness`: needs a way to set a storage failure flag from a server test. If `cetra_storage` has no test seam the server can reach, test the pure part instead: move the "which flags are set" logic into `fn sticky_failures(health: &StorageHealth) -> Vec<&'static str>` and test it with hand-built `StorageHealth` values (all clear → empty; `catalog_failed` → `["catalog"]`; two set → both, in table order). Then `/healthz` on a healthy `TestApp` is still 200 `{"status":"ok"}`.
- `low_disk_fails_readiness`: `TestApp::with_config(|c| c.storage.min_free_bytes = ByteSize::pib(1))`. Wait until `state.storage.health().disk_low` is `true`: poll for up to 3 s, since the monitor measures every second. Then `/readyz` is 503, its detail contains `free disk`, and `/healthz` is 200.
- `probe_result_is_cached_for_ten_seconds` (`#[tokio::test(start_paused = true)]`):
  - A `ReadyCache` and an `AtomicUsize` count the probe calls.
  - Two calls at `t0` give one probe call.
  - Advancing 9 s, a third call still gives one probe call.
  - Advancing 2 s more (11 s), a fourth call gives two.
- `failed_probe_is_cached_too`: the probe returns `false`; the result is `false`, and a second call within 10 s does not call the probe.
- `concurrent_requests_share_one_probe`: start 10 `reachable` calls together (`futures::future::join_all`). The probe sleeps 100 ms and counts; the count is 1.
- `unreachable_store_fails_readiness`: make `reachable` return `false` by calling it once with a probe that returns `false`, at the same `Instant` the handler will see. With paused time, `Instant::now()` does not move. Then `/readyz` is 503 with `object store` in the detail. If that is awkward through `TestApp`, test it at the `readyz` function level with a state whose `ready` cache holds `Some((now, false))`.

## Verify

```bash
cargo test -p cetra-server --all-features health
just check
```

Then by hand, with a release build in S3 mode (`deploy/compose/compose.s3.yaml`):
1. `docker compose stop garage`. Within 10 s, `curl -i localhost:8080/readyz` is 503 naming `object store`.
2. `docker compose start garage`. Within 10 s, it is 200.

Report the outputs.

## Done when

- [ ] `/readyz` has the six checks in the 00-design order, and the probe is cached and single-flight.
- [ ] The tests pass, and specs 09 and 12 are updated.
- [ ] `just check` passes. One commit.
