# 05: serve Prometheus metrics on `metrics_addr`

Read [00-design.md](00-design.md) first, especially § `/metrics` (D-N-28, D-N-29).

**Goal:** a new listener on `metrics_addr` serves `GET /metrics` in the OpenMetrics text format, from a registry in `crates/server/src/telemetry.rs`. This phase adds:
- the endpoint;
- the registry, holding every metric of the 00-design table so phase 06 only increments;
- the gauges that need no new code elsewhere: build info, restore mode, memtable, storage failure flags, read cache, eval queue depth, process.

Phase 06 adds the event counters, and phase 07 adds the storage counters and the disk gauges.

**Edit only:**
- root `Cargo.toml` (`[workspace.dependencies]`) and `crates/server/Cargo.toml`;
- `Cargo.lock`;
- a new `crates/server/src/telemetry.rs` and its `mod` line in `lib.rs`;
- `crates/server/src/http/mod.rs` (`AppInner`);
- `crates/server/src/config.rs`;
- `crates/server/src/serve.rs`;
- `crates/server/src/test_support/mod.rs` (the `AppInner` literal, and the `ServerConfig` literals in `default_config` and `config`);
- `crates/server/src/ingest/fixtures_tests.rs` (its `ServerConfig` literal; `grep -rn "ServerConfig {" crates/` finds all);
- `crates/server/src/meta/eval_jobs.rs` (`count_open`);
- `docs/spec/12-operations.md` (§ Configuration, the key).

**Commit:** `Serve Prometheus metrics on metrics_addr`

**Needs:** 03, because `llmobs_restore_mode` reads `config.restore_mode`. 04 is not needed.

## Read first

- `crates/server/src/serve.rs`:
  - how `grpc_addr` is bound (`bind_listener`), served with a shutdown `oneshot`, stored in `Running`, and stopped in `Running::shutdown`;
  - the doc comment of `shutdown` (the order).
- `crates/server/src/config.rs`: `grpc_addr`, how `""` becomes `None`, and the "must differ from …" validation and its tests.
- 00-design § `/metrics` › **prometheus-client rules**: follow them exactly. They come from the crate's source, and getting one wrong panics in debug or renders an invalid document.
- `prometheus-client` 0.26.0 docs, with `cargo doc -p prometheus-client --open` or docs.rs:
  - `Registry::with_prefix`, `Registry::register`;
  - `metrics::{counter::Counter, gauge::Gauge, histogram::{Histogram, exponential_buckets}, family::Family, info::Info}`;
  - `encoding::text::encode`, `EncodeLabelSet`, `EncodeLabelValue`.

## Changes

### Dependency

Root `Cargo.toml` `[workspace.dependencies]`:

```toml
prometheus-client = "0.26.0"
```

`crates/server/Cargo.toml`: `prometheus-client = { workspace = true }`. Run `just deny` (licence Apache-2.0 OR MIT; no duplicate family crates).

### Config

```rust
/// Prometheus metrics listener (`GET /metrics`, N M3, D-N-28). Unset or `""`: off.
#[serde(default, deserialize_with = "<the same helper grpc_addr uses for \"\">")]
pub metrics_addr: Option<SocketAddr>,
```

- `LLMOBS_METRICS_ADDR` works, as for the other top-level addresses.
- Validation: when set, it must differ from `http_addr`, `otlp_http_addr` and `grpc_addr` ("must differ from …", like the existing checks).
- `docs/spec/12-operations.md` § Configuration gets the row: `metrics_addr` (`LLMOBS_METRICS_ADDR`), default unset (off), "Prometheus `/metrics`; no authentication, bind it to a private address (D-N-28)".

### `telemetry.rs`

**Structure.**
- `pub(crate) struct Telemetry` owns one root `prometheus_client::registry::Registry` (`Registry::default()`) and one field per metric of the 00-design table.
- `const PREFIX: &str = "llmobs";`. Register the `process_*` gauges on the root, and every other metric on `root.sub_registry_with_prefix(PREFIX)`. `render` encodes the root once, which gives a single `# EOF`.
- **Names:**
  - counters are registered without `_total`, e.g. `register("http_requests", …)`;
  - build info is `Family<VersionLabel, Gauge>` named `build_info`;
  - labelled histograms are `Family<L, Histogram, fn() -> Histogram>` via `Family::new_with_constructor`;
  - `flush_seconds` (phase 07) will be `Counter<f64, AtomicU64>`.

  See 00-design's rules. A debug `assert` in `register` catches a counter named `…_total`, so run the tests in debug.

**Label sets.** Small `#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]` structs whose fields are `#[derive(EncodeLabelValue)]` enums, a `String`, or, for `StatusClass`, an enum with a hand-written `impl EncodeLabelValue` (its values `1xx`…`5xx` are not identifiers). Derived enums write the variant identifier, so name variants exactly like the values (`otlp_http`, `GET`) with `#[allow(non_camel_case_types, clippy::upper_case_acronyms)]`:
- `HttpLabels { route: String, method: Method, status: StatusClass }`. `route` is a `String` because `MatchedPath` gives one; its values are bounded by the route table.
- `IngestLabels { protocol: Protocol, outcome: IngestOutcome }`;
- `ProtocolLabel { protocol: Protocol }`;
- `TableLabel { table: Table }`;
- `FlushLabels { table: Table, outcome: Outcome }`;
- `CheckLabel { check: StorageCheck }`;
- `EvalStatusLabel`, `EvalOutcomeLabel`, `EvalKindLabel`, `AlertOutcomeLabel`, `DeliveryLabels { channel, outcome }`, `EmailOutcomeLabel`.

Every enum's variants are exactly the values listed in the 00-design table.

**Fields.** One per metric of the 00-design table, all registered in `Telemetry::new()` with the help text from the table's "Source" column rewritten as a sentence. Phase 06 and 07 counters are registered now and stay at zero until those phases. **Exception:** phase 07's storage counters and disk gauges are added in 07, because they need phase 01's API.

**API.**
- `pub(crate) fn new(restore_mode: bool) -> Self`:
  - `build_info` is set to 1 with `version = env!("CARGO_PKG_VERSION")` (use `Info` or a `Family<VersionLabel, Gauge>`);
  - `restore_mode` is set once.
- `pub(crate) async fn render(&self, state: &AppState) -> String`. It refreshes the scrape-time gauges, then encodes:
  - `storage.health()`: `memtable_bytes`, `memtable_hard_limit`, and the four `llmobs_storage_failed{check}` values;
  - `storage.read_cache_stats()`: when `Some`, set the two counters with `inc_by(new - old)` and the bytes gauge. Keep the last seen totals in two `AtomicU64` fields, so the counters stay monotonic;
  - `llmobs_eval_jobs{status}`, from the cached count (below);
  - `process_*` (below).

**The eval queue count.**
- A `tokio::sync::Mutex<Option<(i64, [i64; 2])>>` reused for 10 s, with the time in `state.clock` nanoseconds (not `tokio::time::Instant`): a paused tokio clock auto-advances past the TTL during database I/O, so the test drives `state.clock` instead.
- The query is `SELECT status, COUNT(*) FROM eval_jobs WHERE status IN ('queued', 'running') GROUP BY status` on `state.meta.reader()`, written next to the other `eval_jobs` queries in `crates/server/src/meta/eval_jobs.rs` as `pub(crate) async fn count_open(…)`.
- A query error keeps the previous values and logs at `warn` with the error, which carries no payload.

**Process metrics (Linux).**
- `process_resident_memory_bytes`: the `VmRSS:` line of `/proc/self/status` (kB × 1024). Not `statm` × 4096: arm64 hosts can have 16K or 64K pages.
- `process_open_fds`: the entry count of `/proc/self/fd`.
- `process_start_time_seconds`: the process start time, taken once in `Telemetry::new()` from `SystemTime::now()`, which is close enough and avoids parsing `/proc/self/stat`.
- On non-Linux, or when a read fails, the gauge is left as it was. They are `#[cfg(target_os = "linux")]`-gated reads. macOS builds must compile.

### The listener (`serve.rs`)

- **Bind.** In `bind`, bind `metrics_addr` with the other sockets, before any server starts, with the same error handling: a failure releases the state.
- **Router.** A separate `axum::Router` with one route, `GET /metrics`, whose handler calls `state.telemetry.render(&state)`. It answers `200` with `Content-Type: application/openmetrics-text; version=1.0.0; charset=utf-8`. Any other path gets axum's default 404. No auth layer, no CORS, no access log beyond the default.
- **`Running`.** It gets `metrics_local_addr: Option<SocketAddr>`, `metrics_serve`, `metrics_shutdown_tx`, like gRPC. Add a `pub fn metrics_local_addr(&self)` accessor if the other addresses have one.
- **Shutdown** stops the metrics listener **first**, before the control socket: a scrape during the drain is useless, and the port is released early. Update `shutdown`'s doc comment and the comment that adds up its time budget (57 s), if the listener's stop adds to it (it should not: do not wait on it longer than the other listeners).
- **`Drop for Running`** aborts the metrics serve task too, as it does for the other serve tasks.
- **Log** `tracing::info!(metrics_local_addr = %addr, "listening")`.
- **`AppInner`** gets `telemetry: crate::telemetry::Telemetry`, built with `Telemetry::new(config.restore_mode)` in `bind` and in `TestApp`. In `bind`, the `AppInner { config, … }` literal moves `config`, so compute `let telemetry = Telemetry::new(config.restore_mode);` before the literal.

## Tests

**`config.rs`:**
- `metrics_addr` is `None` by default and for `""`.
- `"127.0.0.1:9464"` parses.
- Equal to `http_addr` → the "must differ" error.
- `LLMOBS_METRICS_ADDR` sets it.

**`telemetry.rs`:**
- `render_lists_the_base_metrics`: on a `TestApp` state, `render` contains:
  - `llmobs_build_info{version="<CARGO_PKG_VERSION>"} 1`;
  - `llmobs_restore_mode 0`;
  - `llmobs_storage_memtable_hard_limit_bytes`;
  - `llmobs_storage_failed{check="catalog"} 0`;
  - `llmobs_eval_jobs{status="queued"} 0`;
  - `# EOF`.
- `restore_mode_gauge_is_one`: `Telemetry::new(true)` renders `llmobs_restore_mode 1`.
- `eval_queue_count_is_cached`: insert one queued job the way the eval tests do. `render` shows `llmobs_eval_jobs{status="queued"} 1`. Insert another and render again within 10 s: still 1. With `tokio::time::pause()`, advance 11 s: 2.
- `labels_carry_no_tenant_data`: render after a seeded app has ingested a span (`app.seed()` plus one ingest, as other tests do). The output contains neither the org slug, the project id, nor the user email from the seed.
- `process_metrics_on_linux` (`#[cfg(target_os = "linux")]`): `process_open_fds` > 0 and `process_resident_memory_bytes` > 0.

**`serve.rs`:** `metrics_listener_serves_and_stops`.
- Start through `bind` with `metrics_addr = "127.0.0.1:0"`.
- `GET /metrics` on `metrics_local_addr` is 200 with the OpenMetrics content type and contains `llmobs_build_info`.
- `GET /` there is 404.
- `GET /metrics` on the main `http_addr` is **not** the metrics document: it gets the UI fallback or a 404, whichever the main router gives today; assert it lacks `llmobs_build_info`.
- After `shutdown`, connecting to the metrics port fails.

## Verify

```bash
cargo test -p llmobs-server --all-features metrics telemetry
just deny
just check
```

## Done when

- [ ] `metrics_addr` starts the listener, `GET /metrics` renders the registry, and the scrape-time gauges are fresh.
- [ ] The tests pass, `just deny` is clean, and spec 12 lists the key.
- [ ] `just check` passes. One commit.
