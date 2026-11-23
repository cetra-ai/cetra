# TL M3 — `cetra debug-bundle`

**Summary.** This milestone gives a user filing a bug one local, reviewable JSON file instead of a round of questions. It adds:
- a log ring buffer layer: the last 2000 stdout-level lines as JSON, at most 2 MiB, pushed onto M1's subscriber;
- an allow-list view of the configuration: the merged sources (the file, then `CETRA_*`) as a key tree where only allow-listed, non-secret keys show their value and every other leaf reads `"<redacted>"`;
- a second-guard scrubber (emails, `sk-/pk-/pat-cetra-` keys, passwords in URLs), regex-free;
- a generalized admin socket with a `debug_info` command (additive within protocol 1, D-R-36) and a response size cap. The reply carries uptime, the log lines, storage statistics, catalog files per table and level, the `/readyz` checks, `meta.db` counts, the self-monitoring state, the telemetry switchboard decision and the heartbeat payload as it would be sent;
- `cetra debug-bundle [--config <file>] [--output <file>|-]`. It writes `cetra-debug-<UTC timestamp>.json` (mode 0600, at most 8 MiB) with build and host facts, the config view, and either the server's `debug_info` or, when no server answers, the data directory's sizes and format versions read from disk. It never contacts the network.
- the CLI reference, a "Reporting a bug" docs page, a GitHub issue form that asks for the bundle, specs 09 and 12, and the CHANGELOG.

**Prerequisites.**
- Work in `/home/sebastien/travaux/llm-observer/.claude/worktrees/cross-track-review-6a41ad`, branch `claude/telemetry-9e2a24`.
- **M1** (`01-m1-self-monitoring-logs.md`) and **M4** (`04-m4-heartbeat.md`) are committed on this branch (through `bcd67db Format track TL's M1 and M4 code`). Every M1/M4 name, signature and anchor this plan uses was checked against that code. Each phase lists the names it uses.
- **M2** (spans), **M5** (crash reports) and **M6** (product analytics) are implemented in parallel with this milestone, in the same checkout, and any of them may land first in a shared file: `observability/subscriber.rs` (`build`, `Observability`), `observability/mod.rs`, `config.rs`, `switchboard::Decision`, `crates/cli/src/main.rs`, `telemetry.md`, `CHANGELOG.md`, spec 12. **Every edit this plan makes to a shared file is additive: add your lines beside what is there, and keep every addition another milestone made, even when it is not described here.** If an anchor this plan names has moved because another milestone added lines around it, use the same anchor text where it now is. Stop only when an item this plan uses was removed or renamed. The coordinator never runs two phases on one file at once (§ Phase graph).

## Read first

- `AGENTS.md`, all of it. These sections matter most:
  - § Rules (no `unwrap`/`expect` without an invariant comment; no panic on input; `thiserror`; dependencies declared once in the root);
  - § Privacy;
  - § Tests: `CETRA_DOCS_WRITE=1` and `cli_reference_is_up_to_date`; the configuration reference test; the server golden tests and their socket lines;
  - § Commits and reports.
- `docs/mvp-decisions.md`: D-R-36 (admin protocol: additive within version 1, error codes listed by a test), D-TL-6, D-TL-9, D-TL-11, D-N-17.
- `docs/plans/tl-telemetry/README.md`:
  - § Architecture and § Where the code goes;
  - § 4 `cetra debug-bundle`, all of it;
  - § Details settled while planning;
  - § The switchboard and § Instance id;
  - § Milestones M3;
  - § Answers 3.
- The committed M1/M4 code (`crates/server/src/observability/`):
  - `mod.rs`: the `mod` lines (all in one alphabetical list, whatever their visibility: `buckets, facts, heartbeat, instance, otlp, panic, subscriber, switchboard`) and `ObservabilityError`;
  - `subscriber.rs`: `BoxedLayer`, `Observability { logs }`, `init`, `build` (the `layers` vector and the returned `(tracing_subscriber::registry().with(layers), Observability { logs })` tuple), `log_filter` (with the notice directive), and the tests module;
  - `facts.rs`: `VERSION`, `TARGET`, `Profile`, `profile`, `Deployment`, `probe_host`, `deployment`, `process_uptime`;
  - `switchboard.rs`: `NOTICE_TARGET`, `OffReason::as_str`, `Switch`, `Decision` (M4 has the field `heartbeat`; M5 may have added `crash_reports`), `for_config`;
  - `instance.rs`: its imports and `instance_id`;
  - `heartbeat.rs`: `StorageKind`, `storage_kind`, and `collect(meta: &MetaDb, config: &ServerConfig, instance_id: Uuid, deployment: Deployment, uptime: Duration, now_ns: i64) -> Result<Heartbeat, MetaError>`.
- `crates/server/src/config.rs`: `OtlpEndpoint` (line ~447; its `Debug` prints the host only, because some vendors put a token in the path) and `host`, `SelfMonitoringConfig` (line ~480: `otlp_endpoint`, `level`, `logs_endpoint`, `exports`), `parse_otlp_endpoint` (line ~1956).
- `website/src/content/docs/docs/self-hosting/telemetry.md` § "The startup line".
- `crates/server/src/admin_socket.rs`, the whole file, tests included.
- `crates/server/src/golden_tests/build.rs`, `build` (lines ~93–180) and `write_socket_lines`; `crates/server/src/golden_tests/mod.rs` lines ~735–790 (step 9 replays the socket lines with `decode_response`).
- `crates/cli/src/main.rs`:
  - the `Command` enum;
  - `run_cli`;
  - `backup_command` (line ~621: connecting to `admin.sock`, `decode_response` and its exhaustive `match` with `AdminResponse::Done` at line ~657 and `AdminResponse::Failed` at line ~690);
  - the test helper `cli_reference` (its intro sentence at line ~968) and the test `cli_reference_is_up_to_date`.
- `crates/server/src/config.rs`:
  - the module doc and `ALIASES`;
  - `ServerConfig::load`, `ConfigError`, `default_data_dir` (line ~963), `load_from` (line ~1013: M4's `telemetry_env` statement first, then the four `figment` lines at ~1024–1028), `read_toml` (line ~1084), `configured_path` (line ~1099), `map_figment`;
  - `EnvSource` (its `data` skips `auth.oidc` names), `split_key`, `is_filtered`, `is_secret_path`;
  - the test helpers `REFERENCE` and `documented`.
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`: every table row is a key.
- `crates/server/src/http/health.rs` (`readyz`, `sticky_failures`) and `crates/server/src/http/mod.rs` lines 36–120 (`AppState`, `AppInner`).
- `crates/server/src/backup.rs` lines ~457–495: `read_versions` is the model for a read-only SQLite connection.
- `crates/server/src/meta/mod.rs`: `META_FORMAT_VERSION` (line ~71), `expected_version`, `check_file` (line ~608), `MetaDb::reader`; `crates/server/src/meta/instance_settings.rs` (`get_setting`).
- `crates/server/migrations/meta/0001_identity.sql`: `users.disabled_at`, `orgs.state`, `projects.state`, `api_keys.revoked_at`, `personal_tokens.revoked_at`.
- `crates/storage/src/lib.rs`:
  - `StorageHealth` and `StorageStats` (lines ~176–226);
  - `Storage::health` and `Storage::stats`;
  - `wal_dirs`: `<data_dir>/wal/spans` and `<data_dir>/wal/scores`.
- `crates/storage/src/wal/segment.rs` lines 1–80: the segment header is `b"LWAL"`, the format version as a little-endian `u16`, then flags. `crates/storage/src/wal/mod.rs` `segment_name`: `<20 digits>.wal`.
- `crates/storage/src/catalog/schema.txt`: the `data_files` row.
- `crates/storage/src/memory.rs` `detected_memory` (lines ~22–50): the `sysinfo` 0.40 calls `host.rs` repeats. `crates/storage/src/lib.rs` `lock_dir` (line ~1975): the server's exclusive lock on `<data_dir>/LOCK`.
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/sysinfo-0.40.3/src/common/system.rs`:
  - `System::kernel_version` (line ~831) and `System::long_os_version` (line ~872), both associated functions returning `Result<String, Error>`;
  - `System::new_with_specifics`, `total_memory`, `refresh_processes_specifics`, `process`, `Process::cgroup_limits` (line ~1874, Linux only, `None` elsewhere) and `CGroupLimits::total_memory`.
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/figment-0.10.19/src/value/value.rs`: `Value` (line 27) and `Num` (line 544, `Copy`; `to_f64`, `to_u128_lossy`, `to_i128`). `src/error.rs`: `Error` (public `path`, `kind`; `IntoIterator`) and `Kind` (line 105, not `non_exhaustive`).
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/sqlx-sqlite-0.9.0/src/options/mod.rs`: `read_only`, `immutable` (line ~405), `create_if_missing`. The default options set no journal mode.
- `website/STYLE.md`, `website/scripts/check-terms.mjs`, `website/scripts/check-samples.mjs`, `website/astro.config.mjs` (the Help sidebar group), `website/src/content/docs/docs/help/support.mdx`, `website/src/content/docs/docs/self-hosting/backup.md` lines 100–135 (Compose and Kubernetes command style), `deploy/docker/Dockerfile` (distroless, uid 65532, `CETRA_DATA_DIR=/data`).
- `docs/spec/09-api.md` § CLI (the table row of `cetra admin backup`) and `docs/spec/12-operations.md` § Backups (the paragraph that starts `**Online backups`).

## What it is, and what it is not

**What works when it's done:**
- `cetra serve` keeps its last 2000 stdout-level log lines in memory as JSON lines (at most 2 MiB, 16 KiB per line). The stdout output does not change.
- `{"v":1,"command":"debug_info"}` on `<data_dir>/admin.sock` is answered with one line of at most 4 MiB, carrying the `DebugInfo` of § Contracts. Every existing request, response and error code is unchanged.
- `cetra debug-bundle` writes the bundle of § Contracts:
  - into `cetra-debug-<YYYYMMDDTHHMMSSZ>.json` in the current directory, or into `--output`, or to stdout with `--output -`;
  - as a new file with mode 0600: it never overwrites;
  - at most 8 MiB, the oldest log lines dropped first.
  It then prints the path and the review notice. It succeeds:
  - when the server runs;
  - when no server answers (the `not_running` section), or when a server holds `<data_dir>/LOCK` without a socket (`unreachable`, and nothing in the directory is read);
  - when the socket refuses (another uid) or speaks another protocol;
  - when the configuration file is missing, has a syntax error, or fails validation (`config_error`).
- A planted value in any secret or email key, or in `self_monitoring.otlp_endpoint`, never appears in the bundle. Neither does a planted value in a failing key's error text, nor an email, cetra key or URL password in any string of the written file (values and keys).
- Log lines are embedded as JSON objects when they parse as one, and as strings otherwise.
- The CLI reference documents the command. A "Reporting a bug" page and `.github/ISSUE_TEMPLATE/bug_report.yml` ask for the bundle. Specs 09 and 12 and the CHANGELOG say what was added.

**Out of scope:**
- Spans in the ring (M2 adds spans; the ring keeps events only, without span fields).
- Filling `crash_reports` in the telemetry state and classifying M2/M5/M6 config keys when those milestones have not landed yet: TL3.3 and TL3.6 do it for whatever is in the code when they run, and the coordinator for the rest (§ After the last phase).
- A tarball, a log file collector, or reading container logs (the docs say to attach `docker logs`/`journalctl` output).
- Uploading the bundle anywhere.
- A Windows build (the admin socket is Unix-only today).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-TL-6 | One local JSON file; config values shown by allow-list; data from the running server over `admin.sock`; never opening the storage engine next to a running server |
| D-TL-9 | The instance id is read, never created, by a read-only command (review m2) |
| D-TL-11 | The switchboard decision is shown as computed by `switchboard::for_config` |
| D-R-36 | Admin protocol 1: commands, response fields and error codes are only added; a test lists the error codes |
| D-N-17 | The admin socket: one JSON line each way, peer uid checked |
| D-N-30 | `/readyz` check names and order |
| D166 | Nothing here makes an outbound connection |
| Design § Answers 3 | Host names, bucket names and paths are kept; the user reviews the file |
| Design § Details settled while planning, bullet "Debug bundle (M3)" | `config` shows the keys the user set (file and environment) plus the names of the `CETRA_*` variables, without serde defaults; domains, the OIDC issuer and client id, and the `init` org and project names are shown; `config_error` carries the key path, the kind and its static reason, never figment or serde text; catalog file counts come from a read-only query of `catalog.db`; `--output -` writes to stdout; the golden server scenario records one `debug_info` exchange; host facts use `sysinfo` |

Plan-level details (within those decisions):
- **Ring buffer.** It uses the stdout filter (`log_filter(&config.log.level)`, so the `rmcp` cap and M4's notice directive apply) and always formats JSON, even when stdout is pretty. It has no span fields. Bounds: 2000 lines, 2 MiB of line bytes, 16 KiB per line (a longer line is cut and ends with `…<truncated>`).
- **Where the ring is reached.** `subscriber::init` installs it in a process-wide `OnceLock` (`ring::install`). The socket handler reads it with `ring::installed()`. Servers built by tests have no ring, and their `debug_info.logs` is `null`.
- **Response cap.** 4 MiB per response line (`MAX_RESPONSE`). The server drops the oldest log lines until the line fits and sets `logs.trimmed`. A client refuses a longer line.
- **Bundle cap.** 8 MiB (`MAX_BUNDLE_BYTES`), pretty-printed JSON, measured on the scrubbed document. The oldest log lines go first; if the bundle is still too large without them, `BundleError::TooLarge`. Each shown config string is cut at 4096 bytes.
- **Scrubbing.** After the bundle is assembled, it is serialized to a `serde_json::Value`, and every string in it (values and object keys: log lines, config values and key names, `config_error.key`, server strings) goes through the scrubber, so the second guard covers fields that are only validated or static today. Shown config strings are also scrubbed where they are rendered (`config_view`), which keeps that module's tests self-contained; scrubbing is idempotent. The socket answer is not scrubbed: only the server's uid or root reads it.
- **Log lines in the bundle.** Each ring line that parses as a JSON object is embedded as that object; any other line stays a string. On the socket, lines stay strings (`LogsView.lines: Vec<String>`).
- **Host facts.**
  - `sysinfo` (already in `Cargo.lock`, MIT) gives the kernel release, the long OS version and total memory on Linux and macOS. macOS `sysctl` would need FFI, and `unsafe` is forbidden.
  - The cgroup memory limit is `sysinfo::Process::cgroup_limits` of this process, which follows the process's own cgroup and its ancestors for cgroup v2 and v1, and reports only limits below the machine's RAM: the same source as `cetra_storage`'s `memory::detected_memory`. That function is `pub(crate)` in a `pub(crate) mod memory` of `cetra-storage`, so the server crate cannot call it, and the design keeps storage's public API unchanged; `host.rs` repeats its few `sysinfo` calls.
  - The CPU count is `std::thread::available_parallelism`.
- **Format markers without the engine.**
  - `meta.db` and `catalog.db`: `PRAGMA user_version` and `MAX(version)` of `_sqlx_migrations`, on a `read_only` + `immutable` connection. It writes nothing, not even `-shm`, and does not read an un-checkpointed `-wal`.
  - WAL segments: the first 6 bytes of every `*.wal` file in `wal/spans` and `wal/scores` (`LWAL` + `u16` LE version).
- **Server detection.** `connect` to `<data_dir>/admin.sock`:
  - `NotFound` or `ConnectionRefused`: the bundle then probes `<data_dir>/LOCK`, which a running server holds with an exclusive `flock` (`cetra_storage`'s `lock_dir`), with a non-blocking shared lock on a read-only handle (never creating the file). Held → `unreachable` ("the data directory is in use but admin.sock does not answer": a server whose data directory path is too long for a Unix socket runs without one, and a starting server has none yet), and nothing in the directory is read. Not held, absent or unreadable → `not_running`;
  - any other connect error means `unreachable`;
  - a `Failed` answer means `refused`, with the code and message.
- **Environment.** The bundle reads the process environment with `std::env::vars_os`, skipping a pair whose name or value is not UTF-8, so a stray non-UTF-8 variable never makes the support command panic.
- **The instance id** is read with a new `instance::existing_instance_id` (never `instance_id`, which creates it). The heartbeat preview uses the nil UUID when none exists, and `telemetry.instance_id` is then `null`.

## Versions

No new crate. One existing workspace dependency is added to one more crate:

| Crate | Version | Change | Where | Why |
|---|---|---|---|---|
| `sysinfo` | workspace pin `0.40.1`, locked at `0.40.3` (`cargo info sysinfo`; licence MIT, on the `deny.toml` allow-list) | `sysinfo.workspace = true` in `crates/server/Cargo.toml` `[dependencies]`; no feature change (the workspace already enables `system`) | `cetra-server` | kernel release, OS version and total memory on Linux and macOS, and the process's cgroup memory limit on Linux (TL3.4) |

`Cargo.lock` changes only in `cetra-server`'s dependency list. The implementer of TL3.4 runs `cargo info sysinfo@0.40.3` (read-only) and reports its version and licence lines.

## Contracts

These are the types and wire formats the phases implement. Field order is JSON key order. Every phase copies its part from here.

### Admin protocol (additive within version 1)

Request, new command:
```json
{"v":1,"command":"debug_info"}
```
Unknown extra fields are ignored, as for `backup`. The `parse` allow-list becomes `["backup", "debug_info"]`.

Response, new variant (between `Done` and `Failed` in the untagged enum, so a backup line still decodes as `Done`):
```json
{"v":1,"ok":true,"debug_info":{"schema":1,"version":"0.1.0","uptime_secs":3605,"logs":{"lines":["{\"timestamp\":\"2026-11-23T10:00:00.000Z\",\"level\":\"INFO\",\"fields\":{\"message\":\"admin socket listening\"},\"target\":\"cetra_server::admin_socket\"}"],"dropped":0,"trimmed":false},"storage":{"object_store":"file","stats":{"wal_bytes":{"spans":8,"scores":8},"flushes_ok":{"spans":0,"scores":0},"flushes_failed":{"spans":0,"scores":0},"flush_nanos":{"spans":0,"scores":0},"compactions_ok":0,"compactions_failed":0,"compactions_deferred":0,"compactions_conflict":0,"compaction_bytes_written":0,"gc_objects_deleted":0},"health":{"spans_wal_failed":false,"scores_wal_failed":false,"objects_sync_failed":false,"catalog_failed":false,"memtable_bytes":0,"memtable_hard_limit":268435456,"disk_low":false,"disk_free_bytes":100000000000,"disk_min_free_bytes":2147483648},"catalog_files":[{"table":"spans","level":0,"files":3,"bytes":120000}]},"ready":{"ready":true,"failed":[]},"meta":{"orgs":1,"active_orgs":1,"active_projects":2,"users":3,"disabled_users":0,"api_keys":2,"personal_tokens":1,"alert_rules":0,"online_rules":1},"self_monitoring":{"exporting":false,"logs":false,"endpoint_host":null,"level":"warn","reason":"no_endpoint"},"telemetry":{"heartbeat":{"on":false,"reason":"debug_build"},"instance_id":null,"endpoint":"https://telemetry.cetra.sh","heartbeat_preview":{"schema":1,"instance_id":"00000000-0000-0000-0000-000000000000","version":"0.1.0","…":"…"},"crash_reports":null},"errors":[]}}
```
No new error code: `debug_info` cannot fail as a whole, and a failing part goes into `errors`. The error-code list of `the_error_codes_are_stable` stays the same 13 codes.

Constants in `admin_socket.rs`:
```rust
/// The commands `parse` accepts (D-R-36: only added to).
const COMMANDS: [&str; 2] = ["backup", "debug_info"];
/// The largest response line the server writes and a client reads, newline included. The
/// `debug_info` log lines are trimmed to fit.
pub const MAX_RESPONSE: usize = 4 * 1024 * 1024;
```

### `DebugInfo` (`crates/server/src/observability/debug_info.rs`, `pub`)

```rust
/// The `debug_info` schema version. Within admin protocol 1 fields are only added. A new field
/// of `DebugInfo` or of any type nested in it is an `Option` or carries `#[serde(default)]`, so
/// a recorded answer (the golden socket lines) keeps decoding (D-R-36).
pub const DEBUG_INFO_SCHEMA: u32 = 1;

/// What `debug_info` answers: the running server's state for a bug report (track TL § 4).
/// Counts are exact (the file stays local); no value is a payload, a secret or an email.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugInfo {
    /// [`DEBUG_INFO_SCHEMA`].
    pub schema: u32,
    /// The server's version (it may differ from the CLI's after an upgrade).
    pub version: String,
    /// Seconds since the process started (`facts::process_uptime`).
    pub uptime_secs: u64,
    /// The ring buffer, `None` when the process installed none (tests).
    pub logs: Option<LogsView>,
    /// Storage statistics.
    pub storage: StorageView,
    /// The `/readyz` checks.
    pub ready: ReadyView,
    /// `meta.db` counts, `None` when the query failed (see `errors`).
    pub meta: Option<MetaCounts>,
    /// The export to the operator's OTLP endpoint (M1).
    pub self_monitoring: SelfMonitoringState,
    /// What may go to us and what would be sent (M4).
    pub telemetry: TelemetryState,
    /// The parts that could not be read, by name and static kind. Never error text.
    pub errors: Vec<SectionError>,
}

/// The ring buffer's lines, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsView {
    /// JSON lines at the stdout filter.
    pub lines: Vec<String>,
    /// Lines the ring dropped since the process started (its bounds).
    pub dropped: u64,
    /// Lines were removed to fit the response or the bundle cap.
    pub trimmed: bool,
}

/// One value per table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerTable {
    pub spans: u64,
    pub scores: u64,
}

/// `StorageStats`, with named tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsView {
    pub wal_bytes: PerTable,
    pub flushes_ok: PerTable,
    pub flushes_failed: PerTable,
    pub flush_nanos: PerTable,
    pub compactions_ok: u64,
    pub compactions_failed: u64,
    pub compactions_deferred: u64,
    pub compactions_conflict: u64,
    pub compaction_bytes_written: u64,
    pub gc_objects_deleted: u64,
}

/// `StorageHealth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthView {
    pub spans_wal_failed: bool,
    pub scores_wal_failed: bool,
    pub objects_sync_failed: bool,
    pub catalog_failed: bool,
    pub memtable_bytes: u64,
    pub memtable_hard_limit: u64,
    pub disk_low: bool,
    pub disk_free_bytes: u64,
    pub disk_min_free_bytes: u64,
}

/// The storage part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageView {
    /// `file` or `s3`.
    pub object_store: String,
    pub stats: StatsView,
    pub health: HealthView,
    /// Live files per table and level, from `catalog.db` (empty when unreadable; see `errors`).
    pub catalog_files: Vec<LevelFiles>,
}

/// The `/readyz` checks (D-N-30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadyView {
    /// No check failed.
    pub ready: bool,
    /// The failed checks' names, in `/readyz` order.
    pub failed: Vec<String>,
}

/// `meta.db` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaCounts {
    pub orgs: u64,
    pub active_orgs: u64,
    pub active_projects: u64,
    pub users: u64,
    pub disabled_users: u64,
    /// API keys not revoked.
    pub api_keys: u64,
    /// Personal access tokens not revoked.
    pub personal_tokens: u64,
    pub alert_rules: u64,
    pub online_rules: u64,
}

/// The self-monitoring export (M1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfMonitoringState {
    /// `self_monitoring.exports()`.
    pub exporting: bool,
    /// Log records are exported (`logs_endpoint().is_some()`).
    pub logs: bool,
    /// The endpoint's host, never its path or headers.
    pub endpoint_host: Option<String>,
    /// `self_monitoring.level`.
    pub level: String,
    /// Why nothing is exported: `no_endpoint` or `logs_off`; `None` when exporting.
    pub reason: Option<String>,
}

/// On, or off with the switchboard's reason (`OffReason::as_str`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchView {
    pub on: bool,
    pub reason: Option<String>,
}

/// What may go to us (D-TL-11) and the heartbeat as it would be sent now (D-TL-8).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryState {
    /// The switchboard's heartbeat decision.
    pub heartbeat: SwitchView,
    /// `instance_settings['telemetry.instance_id']`, `None` when none was created yet.
    pub instance_id: Option<Uuid>,
    /// `telemetry.endpoint`.
    pub endpoint: String,
    /// `heartbeat::collect` serialized, shown even when the heartbeat is off; the nil UUID
    /// stands for an id not created yet. `None` when it could not be built (see `errors`).
    pub heartbeat_preview: Option<serde_json::Value>,
    /// The switchboard's crash-report decision when M5's `Decision.crash_reports` exists;
    /// `None` before M5 (and in answers of a server without it).
    #[serde(default)]
    pub crash_reports: Option<SwitchView>,
}

/// A part of `debug_info` that could not be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionError {
    /// The part, for example `meta` or `storage.catalog_files`.
    pub section: String,
    /// A static kind, for example `query_failed`.
    pub kind: String,
}
```
`LevelFiles` lives in `observability/disk.rs` (TL3.5), `pub`:
```rust
/// Live data files of one table at one level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelFiles {
    pub table: String,
    pub level: i64,
    pub files: u64,
    pub bytes: u64,
}
```

### The bundle (`crates/server/src/observability/bundle.rs`; Serialize only, `pub(crate)` unless marked)

```rust
/// The bundle's schema version.
pub(crate) const BUNDLE_SCHEMA: u32 = 1;
/// The largest bundle, in bytes.
pub(crate) const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
/// What the CLI prints after writing the bundle.
pub const REVIEW_NOTICE: &str = "review it before you share it: it holds your configuration (secret values replaced by <redacted>), host facts and recent log lines";

/// The typed bundle. It is written through `document` (TL3.8): serialized to a
/// `serde_json::Value`, each log line that is a JSON object embedded as that object, every
/// string and object key scrubbed, then size-checked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Bundle {
    pub(crate) schema: u32,
    /// RFC 3339, UTC, seconds.
    pub(crate) created_at: String,
    pub(crate) build: BuildFacts,
    pub(crate) host: HostFacts,                  // observability::host (TL3.4)
    pub(crate) config: ConfigView,
    /// `None` when the configuration loads.
    pub(crate) config_error: Option<ConfigErrorView>,  // observability::config_view (TL3.3)
    pub(crate) server: ServerSection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct BuildFacts {
    pub(crate) version: &'static str,   // facts::VERSION
    pub(crate) target: &'static str,    // facts::TARGET
    pub(crate) profile: Profile,        // facts::profile()
    pub(crate) ui_embedded: bool,       // from the CLI (cetra_ui::embedded)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ConfigView {
    /// The config file read (`--config`, else `CETRA_CONFIG`), as given.
    pub(crate) file: Option<String>,
    /// Names of the `CETRA_*` variables set, sorted. Never their values.
    pub(crate) env_names: Vec<String>,
    /// The merged file and environment values, redacted by the allow-list.
    pub(crate) values: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ServerSection {
    /// A server answered `debug_info`.
    Running { debug_info: Box<DebugInfo> },
    /// No server answered on `admin.sock` (absent, or refused the connection) and
    /// `<data_dir>/LOCK` is not held.
    NotRunning { data_dir: String, disk: DiskFacts, hint: &'static str },
    /// `admin.sock` could not be used, or `LOCK` is held while the socket does not answer:
    /// a static reason. Nothing in the data directory is read.
    Unreachable { data_dir: String, reason: String },
    /// The server answered with an error (for example `forbidden`, `unknown_command`).
    Refused { code: String, message: String },
}
```
`HostFacts` (`observability/host.rs`, TL3.4):
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HostFacts {
    pub(crate) os: &'static str,                       // std::env::consts::OS
    pub(crate) os_version: Option<String>,             // sysinfo long_os_version
    pub(crate) kernel_release: Option<String>,         // sysinfo kernel_version
    pub(crate) arch: &'static str,                     // std::env::consts::ARCH
    pub(crate) cpus: Option<usize>,                    // available_parallelism
    pub(crate) total_memory_bytes: Option<u64>,        // sysinfo, None when 0
    pub(crate) container_memory_limit_bytes: Option<u64>, // sysinfo Process::cgroup_limits; None off Linux or without a limit
    pub(crate) deployment: Deployment,                 // facts::deployment(facts::probe_host())
}
```
`ConfigErrorView` (`observability/config_view.rs`, TL3.3):
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConfigErrorKind {
    MissingFile, UnreadableFile, TomlSyntax, InvalidType, InvalidValue, InvalidLength,
    UnknownVariant, UnknownField, MissingField, DuplicateField, OutOfRange, Unsupported,
    Message, Invalid, Provider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ConfigErrorView {
    pub(crate) kind: ConfigErrorKind,
    /// The dotted key path, or the variable name for `provider`.
    pub(crate) key: Option<String>,
    /// The static rule of `invalid` and `provider` (never input).
    pub(crate) reason: Option<&'static str>,
}
```
`DiskFacts` (`observability/disk.rs`, TL3.5):
```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DbFileFacts {
    pub(crate) present: bool,
    pub(crate) bytes: u64,
    /// Size of the `-wal` file beside it (0 when absent).
    pub(crate) wal_bytes: u64,
    /// The versions below were read.
    pub(crate) readable: bool,
    /// `PRAGMA user_version` (the format version).
    pub(crate) format: Option<i64>,
    /// The highest `_sqlx_migrations.version`.
    pub(crate) schema: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WalDirFacts {
    pub(crate) present: bool,
    /// `*.wal` files.
    pub(crate) segments: u64,
    pub(crate) bytes: u64,
    /// Distinct header format versions, sorted.
    pub(crate) header_versions: Vec<u16>,
    /// Segments shorter than 6 bytes or without the `LWAL` magic.
    pub(crate) bad_headers: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct WalFacts {
    pub(crate) spans: WalDirFacts,
    pub(crate) scores: WalDirFacts,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DiskFacts {
    pub(crate) data_dir_present: bool,
    pub(crate) meta_db: DbFileFacts,
    pub(crate) catalog_db: DbFileFacts,
    /// `None` when `catalog.db` is absent or unreadable.
    pub(crate) catalog_files: Option<Vec<LevelFiles>>,
    pub(crate) wal: WalFacts,
}
```

Example of a `not_running` bundle (abbreviated values):
```json
{
  "schema": 1,
  "created_at": "2026-11-23T10:20:30Z",
  "build": {"version": "0.1.0", "target": "x86_64-unknown-linux-musl", "profile": "release", "ui_embedded": true},
  "host": {"os": "linux", "os_version": "Linux (Debian GNU/Linux 13)", "kernel_release": "6.8.0-48-generic", "arch": "x86_64", "cpus": 4, "total_memory_bytes": 8221470720, "container_memory_limit_bytes": 2147483648, "deployment": "docker"},
  "config": {"file": "/etc/cetra/cetra.toml", "env_names": ["CETRA_DATA_DIR", "CETRA_ENCRYPTION_KEY"], "values": {"data_dir": "/data", "encryption_key": "<redacted>", "log": {"level": "info"}}},
  "config_error": null,
  "server": {"state": "not_running", "data_dir": "/data", "disk": {"data_dir_present": true, "meta_db": {"present": true, "bytes": 1228800, "wal_bytes": 0, "readable": true, "format": 1, "schema": 31}, "catalog_db": {"present": true, "bytes": 98304, "wal_bytes": 0, "readable": true, "format": 1, "schema": 9}, "catalog_files": [{"table": "spans", "level": 0, "files": 2, "bytes": 40960}], "wal": {"spans": {"present": true, "segments": 1, "bytes": 8, "header_versions": [1], "bad_headers": 0}, "scores": {"present": true, "segments": 1, "bytes": 8, "header_versions": [1], "bad_headers": 0}}}, "hint": "…"}
}
```

In a `running` bundle the server section carries the `debug_info` above, with each log line that parses as a JSON object embedded as that object, and any other line kept as a string:
```json
"server": {"state": "running", "debug_info": {"schema": 1, "logs": {"lines": [{"timestamp": "2026-11-23T10:00:00.000Z", "level": "INFO", "fields": {"message": "admin socket listening"}, "target": "cetra_server::admin_socket"}, "a line that was cut…<truncated>"], "dropped": 0, "trimmed": false}, "…": "…"}}
```

### CLI

```
cetra debug-bundle [--config <file>] [--output <file>|-]
```
```rust
/// Write a JSON file for a bug report: build and host facts, the configuration with secret
/// values redacted, the running server's state and recent log lines. Contacts no network.
DebugBundle {
    /// Config file. Overrides `CETRA_CONFIG`.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Where to write the file. Default: `cetra-debug-<UTC time>.json` in the current
    /// directory. `-` writes it to stdout. An existing file is never overwritten.
    #[arg(long)]
    output: Option<PathBuf>,
},
```
Output on success:
- to a file: two stdout lines, `wrote <absolute path>` then [`REVIEW_NOTICE`];
- with `-`: the JSON on stdout, then [`REVIEW_NOTICE`] on stderr.

## Phase graph

```
M1 + M4 committed
  │
  ├─► TL3.1 scrub + ring ─┬─► TL3.2 ring layer in the subscriber      (subscriber.rs: shared with M2)
  │                       └─► TL3.3 config view ─► TL3.4 host facts ─► TL3.5 disk facts ─► TL3.6 debug_info
  │                                                                     ─► TL3.7 admin socket ─► TL3.8 bundle ─► TL3.9 CLI
  └─► TL3.10 docs, issue form, specs, changelog   (any time; after M4's TL4.9 for telemetry.md and astro.config.mjs)
```

TL3.3–TL3.8 run one after the other: each adds a line to `observability/mod.rs`, and each builds on the one before. TL3.2 may run in parallel with TL3.3–TL3.6 (it touches no file of theirs), and TL3.10 with anything.

| Phase | Files | Shared with a parallel milestone (the coordinator serializes) |
|---|---|---|
| TL3.1 | `observability/scrub.rs` (new), `observability/ring.rs` (new), `observability/mod.rs` | `mod.rs` (M2, M5, M6 may add lines) |
| TL3.2 | `observability/subscriber.rs` | M2 (stdout span fields, trace layer) |
| TL3.3 | `crates/server/src/config.rs`, `observability/config_view.rs` (new), `observability/mod.rs` | `config.rs` (M2, M5, M6 add keys), `mod.rs` |
| TL3.4 | `observability/host.rs` (new), `observability/mod.rs`, `crates/server/Cargo.toml`, `Cargo.lock` | `Cargo.toml`, `Cargo.lock` (M5 adds `backtrace`), `mod.rs` |
| TL3.5 | `observability/disk.rs` (new), `observability/mod.rs` | `mod.rs` |
| TL3.6 | `observability/debug_info.rs` (new), `observability/instance.rs` (M4's), `crates/server/src/http/health.rs`, `observability/mod.rs` | `instance.rs` (M5 reads it; if M5 edits it, serialize), `mod.rs` |
| TL3.7 | `crates/server/src/admin_socket.rs`, `crates/server/src/golden_tests/build.rs`, `crates/cli/src/main.rs` (one `match` arm) | `main.rs` (M2/M5/M6 if they edit it) |
| TL3.8 | `observability/bundle.rs` (new), `observability/mod.rs` | `mod.rs` |
| TL3.9 | `crates/cli/src/debug_bundle.rs` (new), `crates/cli/src/main.rs` | `main.rs` |
| TL3.10 | `website/src/content/docs/docs/help/reporting-a-bug.mdx` (new), `website/astro.config.mjs`, `website/src/content/docs/docs/help/support.mdx`, `website/src/content/docs/docs/self-hosting/telemetry.md`, `.github/ISSUE_TEMPLATE/bug_report.yml` (new), `.github/ISSUE_TEMPLATE/config.yml` (new), `CHANGELOG.md`, `docs/spec/09-api.md`, `docs/spec/12-operations.md` | `telemetry.md` (M5, M6), `CHANGELOG.md`, spec 12 (M2, M5, M6), `astro.config.mjs` |

(`observability/…` is `crates/server/src/observability/…`.) M3 does not edit `serve.rs`, `heartbeat.rs` or `observability/panic.rs`.

## Rules for every phase (the implementer is Haiku)

- Follow the plan exactly. Do not guess. If a step is unclear, contradicts the code, or names an M1/M4 item that was removed or renamed, stop and report the question instead of improvising.
- **Shared files are edited additively** (§ Prerequisites): add your lines beside what is there and keep every line M2, M5 or M6 added, even when this plan does not describe it. Lines another milestone added around an anchor are not a reason to stop.
- Edit only the files of the phase's **Edit** list. Report any change needed elsewhere.
- **Do not run** `cargo build`, `cargo test`, `cargo check`, `cargo clippy`, `cargo nextest` or any `just` recipe: the machine is loaded, and the coordinator runs the checks once at the end of the milestone. These are fine:
  - read-only commands: `grep`, `git diff`, `cargo info`, reading sources under `~/.cargo/registry/src`;
  - `rustfmt --edition 2024 <your .rs files>`;
  - where a phase says so, `cargo metadata --format-version 1 --offline > /dev/null`, which only rewrites `Cargo.lock`.
- Write every test the phase lists, even though you do not run them. Make them compile by reading the APIs; never guess a signature.
- **Done when** for every phase: the listed files changed as specified, nothing else changed (`git status`), and one commit with the given subject exists.
- Commit only your phase's files: `git add <new files> && git commit -m "<subject>" -- <files>`. Other agents commit in the same checkout: if `.git/index.lock` exists, wait a few seconds and retry. Never `git stash`, never `git add -A`.
- Rust rules (AGENTS.md):
  - no `unwrap()`/`expect()` outside tests unless a comment states the invariant;
  - no `unsafe`, so tests never call `std::env::set_var`: inject the environment;
  - no panic on input from disk or the socket;
  - `thiserror` errors;
  - logs and errors never carry payload, emails, secrets or config values.
- New items are `pub(crate)` unless § Contracts marks them `pub`. The CLI and `AdminResponse` reach those through `pub mod` lines. Every item has a doc comment. The workspace lint `unused_qualifications` is on: import a name once rather than writing its path twice.
- In every new module, all non-test items come before the single `#[cfg(test)] mod tests` at the end of the file.
- `observability/mod.rs` has one alphabetical list of `mod` lines, whatever their visibility (committed: `buckets, facts, heartbeat, instance, otlp, panic, subscriber, switchboard`). Insert each new line at its alphabetical place in that list, keeping every line another milestone added (M2, M5 and M6 add lines too). This plan adds `bundle` (`pub`, first), `config_view`, `debug_info` (`pub`), `disk` (`pub`), `host`, `ring` and `scrub`.
- No test asserts that `debug_info.logs` is `None`. Under `cargo test` one process runs every server test, and `ring::install` may have run.
- The milestone check is the coordinator's, after TL3.10 (§ After the last phase).

---

### TL3.1 — The scrubber and the log ring

**Goal.** Two pure modules: a regex-free scrubber for emails, cetra keys and URL passwords, and a bounded in-memory ring of log lines with its `tracing` layer.

**Edit:**
- `crates/server/src/observability/scrub.rs` (new)
- `crates/server/src/observability/ring.rs` (new)
- `crates/server/src/observability/mod.rs` (two `mod` lines)

**M1 names used:** `crate::observability::subscriber::BoxedLayer` (`pub(crate) type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>`). Check it in `subscriber.rs` before you start.

**Steps.**
1. Create `scrub.rs` with exactly this content:
   ```rust
   //! The debug bundle's second guard (track TL § 4): log lines are payload-free by the AGENTS.md
   //! rules, and shown config values are not secrets, but before either goes into a file meant
   //! for a public issue, anything shaped like a password in a URL, a cetra key or an email
   //! address is replaced by [`REDACTED`]. Plain byte scans, no regex: every delimiter is ASCII,
   //! so every cut is on a character boundary.

   /// What a hidden value reads.
   pub(crate) const REDACTED: &str = "<redacted>";

   /// The prefixes of cetra's API keys and personal access tokens.
   const KEY_PREFIXES: [&str; 3] = ["sk-cetra-", "pk-cetra-", "pat-cetra-"];

   /// `text` with URL user info, cetra keys and email addresses replaced by [`REDACTED`].
   /// Idempotent: scrubbing the result changes nothing.
   pub(crate) fn scrub_text(text: &str) -> String {
       let text = scrub_userinfo(text);
       let text = scrub_keys(&text);
       scrub_emails(&text)
   }

   /// A byte of a key's body.
   fn is_key_byte(byte: u8) -> bool {
       byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
   }

   /// A byte of an email's local part. Bytes of non-ASCII characters count (`josé@…`): every
   /// byte of a multi-byte character is >= 0x80, so a scan never stops inside one.
   fn is_local_byte(byte: u8) -> bool {
       byte.is_ascii_alphanumeric() || byte >= 0x80 || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
   }

   /// A byte of an email's domain (non-ASCII bytes count, as for the local part).
   fn is_domain_byte(byte: u8) -> bool {
       byte.is_ascii_alphanumeric() || byte >= 0x80 || matches!(byte, b'.' | b'-')
   }

   /// `scheme://user:password@host` becomes `scheme://<redacted>@host`: everything between
   /// `://` and the last `@` before the next `/`, quote, backslash, whitespace or the end. `?`
   /// and `#` do not end the scan, so a password containing them is covered; the cost is
   /// over-redacting `https://h?x=a@b`, which is harmless.
   fn scrub_userinfo(text: &str) -> String {
       let bytes = text.as_bytes();
       let mut out = String::with_capacity(text.len());
       let mut copied = 0;
       let mut search = 0;
       while let Some(found) = text[search..].find("://") {
           let start = search + found + 3;
           let mut end = start;
           let mut at = None;
           while end < bytes.len()
               && !matches!(bytes[end], b'/' | b'"' | b'\'' | b'\\')
               && !bytes[end].is_ascii_whitespace()
           {
               if bytes[end] == b'@' {
                   at = Some(end);
               }
               end += 1;
           }
           if let Some(at) = at {
               out.push_str(&text[copied..start]);
               out.push_str(REDACTED);
               copied = at;
           }
           search = end;
       }
       out.push_str(&text[copied..]);
       out
   }

   /// A key prefix and the key bytes after it become [`REDACTED`].
   fn scrub_keys(text: &str) -> String {
       let bytes = text.as_bytes();
       let mut out = String::with_capacity(text.len());
       let mut copied = 0;
       let mut i = 0;
       while i < bytes.len() {
           let hit = KEY_PREFIXES
               .iter()
               .find(|prefix| bytes[i..].starts_with(prefix.as_bytes()));
           match hit {
               Some(prefix) => {
                   let mut end = i + prefix.len();
                   while end < bytes.len() && is_key_byte(bytes[end]) {
                       end += 1;
                   }
                   out.push_str(&text[copied..i]);
                   out.push_str(REDACTED);
                   copied = end;
                   i = end;
               }
               None => i += 1,
           }
       }
       out.push_str(&text[copied..]);
       out
   }

   /// `local@domain.tld` becomes [`REDACTED`] when the local part is not empty, the domain has
   /// a dot, and its last label has at least two bytes and starts with a letter or a non-ASCII
   /// byte (so a version such as `crate@1.40` is kept). A trailing `.` or `-` is punctuation,
   /// not the domain.
   fn scrub_emails(text: &str) -> String {
       let bytes = text.as_bytes();
       let mut out = String::with_capacity(text.len());
       let mut copied = 0;
       let mut i = 0;
       while i < bytes.len() {
           if bytes[i] != b'@' {
               i += 1;
               continue;
           }
           let mut start = i;
           while start > copied && is_local_byte(bytes[start - 1]) {
               start -= 1;
           }
           let mut end = i + 1;
           while end < bytes.len() && is_domain_byte(bytes[end]) {
               end += 1;
           }
           while end > i + 1 && matches!(bytes[end - 1], b'.' | b'-') {
               end -= 1;
           }
           let domain = &text[i + 1..end];
           let tld = domain.rsplit('.').next().unwrap_or("");
           let email = start < i
               && domain.contains('.')
               && !domain.starts_with('.')
               && tld.len() >= 2
               && (tld.as_bytes()[0].is_ascii_alphabetic() || tld.as_bytes()[0] >= 0x80);
           if email {
               out.push_str(&text[copied..start]);
               out.push_str(REDACTED);
               copied = end;
               i = end;
           } else {
               i += 1;
           }
       }
       out.push_str(&text[copied..]);
       out
   }
   ```
2. Create `ring.rs`:
   - Module doc: `//! The log ring buffer of `cetra serve` (track TL § 4): the last log lines at the stdout filter, as JSON, kept in memory for the admin socket's `debug_info`. Bounded by line count, total bytes and line length, so a burst or a huge field cannot grow it.`
   - Imports: `std::collections::VecDeque`, `std::io::{self, Write}`, `std::sync::{Arc, Mutex, OnceLock, PoisonError}`, `tracing_subscriber::Layer`, `tracing_subscriber::filter::EnvFilter`, `tracing_subscriber::fmt::MakeWriter`, `super::subscriber::BoxedLayer`.
   - Contents, verbatim:
   ```rust
   /// Lines kept.
   pub(crate) const MAX_LINES: usize = 2000;
   /// Bytes of all kept lines together.
   pub(crate) const MAX_BYTES: usize = 2 * 1024 * 1024;
   /// Bytes of one line; a longer line is cut and ends with [`TRUNCATED`].
   pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024;
   /// The end of a cut line.
   pub(crate) const TRUNCATED: &str = "…<truncated>";
   /// What one writer buffers for one event before the cut (the rest is discarded).
   const WRITE_CAP: usize = 2 * MAX_LINE_BYTES;

   /// The ring. Cloning shares it.
   #[derive(Debug, Clone, Default)]
   pub(crate) struct LogRing(Arc<Mutex<RingState>>);

   #[derive(Debug, Default)]
   struct RingState {
       lines: VecDeque<String>,
       /// Sum of `lines`' lengths; never above [`MAX_BYTES`].
       bytes: usize,
       /// Lines removed to make room since the ring was made.
       dropped: u64,
   }

   /// A copy of the ring's content, oldest line first.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct RingSnapshot {
       pub(crate) lines: Vec<String>,
       pub(crate) dropped: u64,
   }

   /// The ring of this process, set once by `subscriber::init`.
   static INSTALLED: OnceLock<LogRing> = OnceLock::new();

   /// Makes `ring` the process's ring. The first call wins; later calls change nothing.
   pub(crate) fn install(ring: LogRing) {
       let _ = INSTALLED.set(ring);
   }

   /// The process's ring, if `subscriber::init` installed one.
   pub(crate) fn installed() -> Option<&'static LogRing> {
       INSTALLED.get()
   }
   ```
   - `impl LogRing`:
     - `pub(crate) fn new() -> Self` → `Self::default()`.
     - `pub(crate) fn push(&self, line: &str)`, body verbatim:
       ```rust
       let line = line.trim_end_matches(['\n', '\r']);
       if line.is_empty() {
           return;
       }
       let line = truncate_line(line);
       let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
       while !state.lines.is_empty()
           && (state.lines.len() >= MAX_LINES || state.bytes + line.len() > MAX_BYTES)
       {
           if let Some(old) = state.lines.pop_front() {
               state.bytes -= old.len();
               state.dropped += 1;
           }
       }
       state.bytes += line.len();
       state.lines.push_back(line);
       ```
       (Invariant, as a comment: `MAX_LINE_BYTES < MAX_BYTES`, so once the loop ends `bytes + line.len() <= MAX_BYTES`.)
     - `pub(crate) fn snapshot(&self) -> RingSnapshot`: lock (same poison handling), `RingSnapshot { lines: state.lines.iter().cloned().collect(), dropped: state.dropped }`.
     - `pub(crate) fn layer(&self, filter: EnvFilter) -> BoxedLayer`, verbatim:
       ```rust
       tracing_subscriber::fmt::layer()
           .json()
           .with_current_span(false)
           .with_span_list(false)
           .with_ansi(false)
           .with_writer(self.clone())
           .with_filter(filter)
           .boxed()
       ```
   - `fn truncate_line(line: &str) -> String`, verbatim:
     ```rust
     if line.len() <= MAX_LINE_BYTES {
         return line.to_owned();
     }
     let mut cut = MAX_LINE_BYTES - TRUNCATED.len();
     while !line.is_char_boundary(cut) {
         cut -= 1;
     }
     format!("{}{TRUNCATED}", &line[..cut])
     ```
   - The writer, verbatim:
     ```rust
     /// Buffers one event's formatted text and pushes its lines into the ring when dropped
     /// (the fmt layer writes one event per writer).
     pub(crate) struct RingWriter {
         ring: LogRing,
         buf: Vec<u8>,
     }

     impl Write for RingWriter {
         fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
             let room = WRITE_CAP.saturating_sub(self.buf.len());
             self.buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
             Ok(bytes.len())
         }

         fn flush(&mut self) -> io::Result<()> {
             Ok(())
         }
     }

     impl Drop for RingWriter {
         fn drop(&mut self) {
             let text = String::from_utf8_lossy(&self.buf);
             for line in text.split('\n') {
                 self.ring.push(line);
             }
         }
     }

     impl<'a> MakeWriter<'a> for LogRing {
         type Writer = RingWriter;

         fn make_writer(&'a self) -> Self::Writer {
             RingWriter { ring: self.clone(), buf: Vec::new() }
         }
     }
     ```
3. In `observability/mod.rs`, add `pub(crate) mod ring;` and `pub(crate) mod scrub;` in alphabetical position.

**Tests.**

In `scrub.rs`:
- `emails_are_redacted`, a table:
  - `ada@example.com` → `<redacted>`;
  - `{"user":"bob.smith+x@corp.example.org"}` → `{"user":"<redacted>"}`;
  - `mail ada@example.com.` → `mail <redacted>.`;
  - `a@b.co and c@d.io` → `<redacted> and <redacted>`;
  - `josé@example.com` → `<redacted>` (a non-ASCII local part);
  - `ada@exämple.com` → `<redacted>` (a non-ASCII domain).
- `non_emails_are_kept`: each of `crate@1.40`, `@user`, `a @ b.com`, `foo@localhost`, `x@.com`, `a@b.c`, `` (empty) is unchanged.
- `keys_are_redacted`:
  - `key sk-cetra-AbC_12-x9 end` → `key <redacted> end`;
  - the same for `pk-cetra-` and `pat-cetra-`;
  - `"sk-cetra-"` alone → `"<redacted>"`;
  - `sk-cetra-sk-cetra-abc` → `<redacted>`.
- `url_user_info_is_redacted`:
  - `https://u:p@h.example/x` → `https://<redacted>@h.example/x`;
  - `smtp://user:pa%40ss@mail.example:587` → `smtp://<redacted>@mail.example:587`;
  - `"s3://bucket/prefix"` is unchanged;
  - `https://h.example/a@b` is unchanged (the `@` is in the path);
  - `https://u:pa#ss@h.example` → `https://<redacted>@h.example`, and `https://u:pa?ss@h.example` → `https://<redacted>@h.example` (a password with `#` or `?`).
- `non_ascii_text_around_matches_is_kept`: `é ada@example.com ü sk-cetra-x ✓` → `é <redacted> ü <redacted> ✓`.
- `scrub_is_idempotent`: for every input of the tables above, `scrub_text(&scrub_text(x)) == scrub_text(x)`.
- `scrub_is_total_and_idempotent_on_a_small_alphabet`: every string of length 1 to 5 over the pieces `["a", "@", ".", "-", "é", "sk-cetra-", "://", ":", "/", "\""]` (10⁵ + … combinations, built with nested index loops). Scrubbing never panics and is idempotent.

In `ring.rs`:
- `push_keeps_lines_in_order`: push `a`, `b`, `c`; `snapshot().lines == ["a", "b", "c"]`, `dropped == 0`.
- `empty_lines_and_newlines_are_not_kept`: push `""`, `"\n"`, `"x\r\n"`; `lines == ["x"]`.
- `the_line_count_is_bounded`: push 2500 lines `line-{i}`; `lines.len() == 2000`, the first is `line-500`, `dropped == 500`.
- `the_byte_total_is_bounded`: push 300 lines of 10 000 bytes; the sum of lengths is `<= MAX_BYTES`, the last line is the last pushed, and `dropped == 300 - lines.len()`.
- `a_long_line_is_cut_on_a_char_boundary`:
  - a line of 20 000 `é` (2 bytes each) → its length is `<= MAX_LINE_BYTES`, it ends with `TRUNCATED`, and it is valid UTF-8 (a `String`);
  - a line of `MAX_LINE_BYTES` bytes exactly is kept whole.
- `concurrent_pushes_keep_the_bounds`: 8 `std::thread`s push 1000 lines of 600 bytes each into one cloned ring. Afterwards `lines.len() <= MAX_LINES`, the byte sum is `<= MAX_BYTES`, and `lines.len() as u64 + dropped == 8000`.
- `the_layer_writes_json_lines_without_span_fields`:
  1. `let ring = LogRing::new();` and `let subscriber = tracing_subscriber::registry().with(ring.layer(EnvFilter::new("info")));` (import `tracing_subscriber::layer::SubscriberExt`).
  2. Under `tracing::subscriber::with_default(subscriber, || { let span = tracing::info_span!("outer", id = 7); let _entered = span.enter(); tracing::info!(project_id = 7, "hello"); tracing::debug!("hidden"); })`.
  3. Exactly one line. It parses as `serde_json::Value` with `fields.message == "hello"` and `fields.project_id == 7`, and has neither a `span` nor a `spans` key.
- `the_writer_caps_one_event`: a `RingWriter` from `ring.make_writer()` gets `write_all` of 100 000 `a` bytes, then is dropped. One line of length `<= MAX_LINE_BYTES`, ending with `TRUNCATED`.
- `install_keeps_the_first_ring`: `install(a.clone())`, then `install(b)`. `installed()` is `Some`, and pushing into `a` shows in `installed().unwrap().snapshot()`. Comment: the only test that installs; other tests never assume `installed()` is `None`.

**Commit:** `Add the debug bundle's scrubber and log ring`

---

### TL3.2 — The ring layer in the subscriber

**Goal.** `subscriber::build` pushes the ring layer, with the stdout filter, onto M1's layer list, and `init` installs the ring for the admin socket.

**Edit:**
- `crates/server/src/observability/subscriber.rs` (shared with M2)

**M1/M4 names used** (checked in the committed `subscriber.rs`):
- `pub(crate) fn build(config: &ServerConfig, format: LogFormat, writer: BoxMakeWriter, runtime: Handle) -> Result<(ComposedSubscriber, Observability), ObservabilityError>`, its statement `let mut layers: Vec<BoxedLayer> = vec![stdout];`, and its last expression `Ok((tracing_subscriber::registry().with(layers), Observability { logs }))`;
- `pub(crate) fn log_filter(level: &str) -> Result<EnvFilter, ParseError>` (with M4's notice directive);
- `ObservabilityError::LogLevel`;
- `pub struct Observability { logs: Option<LogPipeline> }`;
- `pub fn init(config: &ServerConfig, format: LogFormat) -> Result<Observability, ObservabilityError>`.

**Shared with M2, additive.** M2 may have landed first and added a trace layer to `layers`, a field to `Observability`, span settings on the stdout layer, or a step to `init` and `shutdown`. Keep all of it; add only the lines below, beside M2's. Stop only if an item above was removed or renamed.

**Steps.**
1. Import `super::ring::{self, LogRing}`.
2. Add a field to `Observability`, after `logs` (and after any field M2 added):
   ```rust
   /// The ring buffer of the last log lines (track TL M3), installed by [`init`].
   ring: LogRing,
   ```
   and the method:
   ```rust
   /// The ring buffer this subscriber writes.
   pub(crate) fn log_ring(&self) -> &LogRing {
       &self.ring
   }
   ```
3. In `build`, right after `let mut layers: Vec<BoxedLayer> = vec![stdout];`, add:
   ```rust
   // The ring keeps what stdout shows, as JSON whatever the stdout format (track TL M3).
   let ring = LogRing::new();
   let ring_filter = log_filter(&config.log.level).map_err(ObservabilityError::LogLevel)?;
   layers.push(ring.layer(ring_filter));
   ```
   and add `ring` to the `Observability { … }` literal in `build`'s last expression (today `Observability { logs }`, which becomes `Observability { logs, ring }`, plus any field M2 added). If M2 pushed its own layer right after `vec![stdout]`, put the three lines after M2's push: layer order does not matter here.
4. In `init`, right after the statement that ends with `.try_init().map_err(|_| ObservabilityError::AlreadyInstalled)?;` (it spans three lines), add `ring::install(observability.ring.clone());`.
5. Extend the module doc's layer list by one sentence: `The ring buffer layer (track TL M3) keeps the stdout-level lines as JSON for the admin socket's `debug_info`.`
6. If a test in this file builds `Observability { logs: None }` (or another literal) directly, add `ring: LogRing::new()` to it.

**Tests** (in `subscriber.rs` `mod tests`; `#[tokio::test]`, since `build` takes `Handle::current()`; configs from `crate::config::load_from`):
- `the_ring_keeps_stdout_level_lines_as_json`:
  1. `build` with `load_from(Some("[log]\nlevel = \"info\"\n"), &[])`, `LogFormat::Json` and a buffer writer, as M1's tests do.
  2. With `tracing::dispatcher::set_default`, emit:
     - `info!("ring-canary")` and `debug!("ring-debug")`;
     - `warn!(target: "rmcp::service", "rmcp-warn")` and `info!(target: "rmcp::service", "rmcp-info")`.
  3. `observability.log_ring().snapshot().lines`:
     - has one line containing `ring-canary` and one containing `rmcp-warn`;
     - has none containing `ring-debug` or `rmcp-info`;
     - every line parses as JSON with the keys `timestamp`, `level`, `fields` and `target`.
- `the_ring_is_json_when_stdout_is_pretty`: the same build with `LogFormat::Pretty`; `info!("pretty-canary")` → the ring's line parses as JSON.
- `the_notice_reaches_the_ring_at_warn`: `log.level = "warn"`; `info!(target: crate::observability::switchboard::NOTICE_TARGET, "notice-canary")` → in the ring (M4's directive in `log_filter`).
- `an_invalid_log_level_is_an_error` (M1's) still passes unchanged: `log_filter` is parsed for stdout first.

**Commit:** `Keep the last log lines in a ring buffer`

---

### TL3.3 — The allow-list config view

**Goal.** The merged configuration sources as a JSON key tree where only allow-listed keys show their value, and the load error as a key path and kind.

**Edit:**
- `crates/server/src/config.rs` (shared: keep the edit to the items below)
- `crates/server/src/observability/config_view.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)

**M1/M4 names used:** `pub(crate) fn load_from(toml: Option<&str>, env: &[(&str, &str)]) -> Result<ServerConfig, ConfigError>` (line ~1013), whose first statement is M4's `let telemetry_env = …TelemetryEnv::from_pairs(env)…;`, followed by the four lines that build the figment (~1024–1028); the `[self_monitoring]` and `[telemetry]` rows of the configuration reference.

**Shared with M2, M5 and M6, additive.** They add config tables, keys, validation and reference rows to `config.rs`. Keep all of it. Your edit is: three `pub(crate)` visibility changes, two new functions placed right before `load_from`, and the four figment lines of `load_from` replaced by one call. If another milestone changed those four lines, keep its change inside `sources` and report it.

**Steps.**
1. In `config.rs`:
   1. Make these existing functions `pub(crate)`, changing nothing else in them:
      - `default_data_dir`;
      - `read_toml`;
      - `configured_path`.
      Add one doc line to each: `/// Also read by the debug bundle (track TL M3).`
   2. Add, right before `load_from`:
      ```rust
      /// The file, then the `CETRA_*` variables, as one figment: the sources [`load_from`]
      /// extracts and [`source_tree`] shows.
      fn sources(toml: Option<&str>, env: &[(&str, &str)]) -> Figment {
          let mut figment = Figment::new();
          if let Some(toml) = toml {
              figment = figment.merge(Toml::string(toml));
          }
          figment.merge(EnvSource { pairs: env })
      }

      /// The merged sources as one dictionary, before serde defaults, validation and the
      /// `storage.data_dir` injection: what the debug bundle shows (D-TL-6). A TOML syntax
      /// error is mapped as [`load_from`] maps it, so its text names a position only.
      pub(crate) fn source_tree(toml: Option<&str>, env: &[(&str, &str)]) -> Result<Dict, ConfigError> {
          let mut data = sources(toml, env).data().map_err(map_figment)?;
          Ok(data.remove(&Profile::Default).unwrap_or_default())
      }
      ```
   3. In `load_from`, replace these lines:
      ```rust
      let mut figment = Figment::new();
      if let Some(toml) = toml {
          figment = figment.merge(Toml::string(toml));
      }
      figment = figment.merge(EnvSource { pairs: env });
      ```
      with `let mut figment = sources(toml, env);`. Keep M4's `telemetry_env` statement before it, and everything after it, unchanged.
2. Create `config_view.rs`. Module doc:

   > The configuration as the debug bundle shows it (D-TL-6). The merged sources (the file, then `CETRA_*`) are walked as a key tree. A leaf shows its value only when its dotted path is in [`SHOWN_KEYS`]; every other leaf reads `"<redacted>"`, so a key added later is hidden until someone allow-lists it. Shown strings go through the scrubber and are cut at 4096 bytes. A load error becomes its key path and kind, never its text (figment and serde texts can quote values).

   Imports: `figment::error::Kind`, `figment::value::{Dict, Num, Value as FigmentValue}`, `serde::Serialize`, `serde_json::{Map, Value}`, `crate::config::ConfigError`, `super::scrub::{REDACTED, scrub_text}`.
3. The allow-list, verbatim. Array elements are written `[]`, as on the reference page:
   ```rust
   /// Keys whose value the bundle shows: every key of the configuration reference except those
   /// that can hold a secret or an email address. Host names, bucket names, paths and domains are
   /// shown (design answer 3: the user reviews the file).
   pub(crate) const SHOWN_KEYS: &[&str] = &[
       "data_dir",
       "public_url",
       "http_addr",
       "otlp_http_addr",
       "grpc_addr",
       "metrics_addr",
       "trusted_proxies",
       "restore_mode",
       "storage.min_free_bytes",
       "storage.object_store",
       "storage.s3.region",
       "storage.s3.endpoint",
       "storage.s3.force_path_style",
       "storage.s3.allow_http",
       "storage.wal.sync",
       "storage.wal.segment_bytes",
       "storage.wal.group_max_bytes",
       "storage.wal.group_max_wait",
       "storage.wal.skip_corrupt",
       "storage.flush.max_bytes",
       "storage.flush.max_age",
       "storage.flush.upload_concurrency",
       "storage.memtable.hard_limit",
       "storage.query.target_partitions",
       "storage.query.metadata_cache_bytes",
       "storage.query.trace_lookback",
       "storage.query.max_memory_total",
       "storage.query.max_memory",
       "storage.query.initial_memory",
       "storage.query.admission_wait",
       "storage.query.read_cache_bytes",
       "storage.anchor.window",
       "storage.gc.grace",
       "storage.compaction.interval",
       "storage.compaction.threads",
       "storage.compaction.target_file_size",
       "storage.compaction.size_ratio",
       "storage.compaction.l0_files",
       "storage.compaction.l0_bytes",
       "storage.compaction.l0_age",
       "storage.compaction.close_delay",
       "storage.compaction.max_memory",
       "storage.deletion.max_delay",
       "ingest.max_request_bytes",
       "ingest.max_spans_per_request",
       "ingest.max_attribute_bytes",
       "ingest.max_inflight_bytes",
       "ingest.max_grpc_message_bytes",
       "media.min_bytes",
       "media.max_bytes",
       "eval.workers",
       "issues.workers",
       "security.allow_private_network_egress",
       "entitlements.upgrade_url",
       "entitlements.control_plane_provider",
       "entitlements.control_plane",
       "entitlements.defaults.plan_name",
       "entitlements.defaults.features",
       "entitlements.defaults.units_included",
       "entitlements.defaults.units_cap",
       "entitlements.defaults.ingest_spans_per_sec",
       "entitlements.defaults.ingest_burst",
       "entitlements.defaults.data_access_days",
       "entitlements.defaults.max_seats",
       "entitlements.defaults.max_projects",
       "entitlements.defaults.max_alert_rules",
       "entitlements.defaults.max_active_online_rules",
       "entitlements.defaults.max_active_issue_sources",
       "entitlements.defaults.max_annotation_queues",
       "alerts.email_recipients",
       "orgs.user_create",
       "orgs.create_url",
       "demo.org",
       "demo.requests_per_minute",
       "demo.user_in_flight",
       "demo.org_in_flight",
       "demo.sql_timeout",
       "demo.sql_max_rows",
       "auth.signup",
       "auth.signup_domains",
       "auth.disable_password",
       "auth.require_email_verification",
       "auth.enforce_sso_domains",
       "auth.github.client_id",
       "auth.github.link_existing_by_email",
       "auth.google.client_id",
       "auth.google.allowed_domains",
       "auth.google.link_existing_by_email",
       "auth.oidc[].name",
       "auth.oidc[].display_name",
       "auth.oidc[].issuer",
       "auth.oidc[].client_id",
       "auth.oidc[].scopes",
       "auth.oidc[].allowed_domains",
       "auth.oidc[].auto_provision.org",
       "auth.oidc[].auto_provision.role",
       "auth.oidc[].link_existing_by_email",
       "init.org",
       "init.project",
       "log.format",
       "log.level",
       "self_monitoring.logs",
       "self_monitoring.level",
       "self_monitoring.resource",
       "telemetry.heartbeat",
       "telemetry.endpoint",
   ];

   /// The longest shown string, in bytes; a longer one is cut and ends with `…<truncated>`.
   const MAX_VALUE_BYTES: usize = 4096;
   ```
   Before you write it, list the page's keys (`grep '^| \`' website/src/content/docs/docs/self-hosting/configuration-reference.md`) and compare them with this list and the test's `REDACTED_KEYS`. M2, M5 or M6 may have landed first and added keys. Classify each such key that is on the page:
   - shown (add to `SHOWN_KEYS`, at the end): `self_monitoring.traces`, `self_monitoring.trace_sample_ratio` (M2), `telemetry.crash_reports` (M5), `analytics.posthog_host` (M6);
   - redacted (add to the test's `REDACTED_KEYS`): `analytics.posthog_key` (M6).
   Do not add a key that is not on the page: the test also refuses list entries the page lacks. Leave any other unclassified key alone: `every_reference_key_is_classified` fails loudly, naming the key and this file, and the coordinator classifies it. Report it.
4. The tree conversion and the walk, verbatim:
   ```rust
   /// The merged sources as redacted JSON.
   pub(crate) fn redacted_values(tree: &Dict) -> Value {
       let json = Value::Object(
           tree.iter()
               .map(|(key, value)| (key.clone(), figment_to_json(value)))
               .collect(),
       );
       redact(&json)
   }

   /// A figment value as JSON. A number JSON cannot hold exactly becomes its decimal text.
   fn figment_to_json(value: &FigmentValue) -> Value {
       match value {
           FigmentValue::String(_, text) => Value::String(text.clone()),
           FigmentValue::Char(_, c) => Value::String(c.to_string()),
           FigmentValue::Bool(_, b) => Value::Bool(*b),
           FigmentValue::Num(_, num) => num_to_json(*num),
           FigmentValue::Empty(_, _) => Value::Null,
           FigmentValue::Dict(_, dict) => Value::Object(
               dict.iter()
                   .map(|(key, child)| (key.clone(), figment_to_json(child)))
                   .collect(),
           ),
           FigmentValue::Array(_, items) => Value::Array(items.iter().map(figment_to_json).collect()),
       }
   }

   fn num_to_json(num: Num) -> Value {
       if let Some(float) = num.to_f64() {
           return serde_json::Number::from_f64(float).map_or(Value::Null, Value::Number);
       }
       if let Some(unsigned) = num.to_u128_lossy() {
           return u64::try_from(unsigned)
               .map_or_else(|_| Value::String(unsigned.to_string()), |n| Value::Number(n.into()));
       }
       if let Some(signed) = num.to_i128() {
           return i64::try_from(signed)
               .map_or_else(|_| Value::String(signed.to_string()), |n| Value::Number(n.into()));
       }
       Value::Null
   }

   /// `tree` with every leaf outside [`SHOWN_KEYS`] replaced by [`REDACTED`].
   pub(crate) fn redact(tree: &Value) -> Value {
       redact_at("", tree)
   }

   /// Objects are walked key by key. An array that holds an object or an array is walked
   /// element by element under `path[]`; an array of scalars is one leaf. Anything else is a
   /// leaf: shown when `path` is allow-listed, else [`REDACTED`] (fail closed: a dict where a
   /// scalar key is expected is walked, and its children are not on the list).
   fn redact_at(path: &str, value: &Value) -> Value {
       match value {
           Value::Object(map) => Value::Object(
               map.iter()
                   .map(|(key, child)| (key.clone(), redact_at(&join(path, key), child)))
                   .collect::<Map<String, Value>>(),
           ),
           Value::Array(items) if items.iter().any(|item| item.is_object() || item.is_array()) => {
               let element = format!("{path}[]");
               Value::Array(items.iter().map(|item| redact_at(&element, item)).collect())
           }
           leaf => {
               if SHOWN_KEYS.contains(&path) {
                   show(leaf)
               } else {
                   Value::String(REDACTED.to_owned())
               }
           }
       }
   }

   fn join(path: &str, key: &str) -> String {
       if path.is_empty() { key.to_owned() } else { format!("{path}.{key}") }
   }

   /// A shown leaf: strings scrubbed and capped.
   fn show(value: &Value) -> Value {
       match value {
           Value::String(text) => Value::String(cap(scrub_text(text))),
           Value::Array(items) => Value::Array(items.iter().map(show).collect()),
           other => other.clone(),
       }
   }

   fn cap(text: String) -> String {
       if text.len() <= MAX_VALUE_BYTES {
           return text;
       }
       let mut cut = MAX_VALUE_BYTES;
       while !text.is_char_boundary(cut) {
           cut -= 1;
       }
       format!("{}…<truncated>", &text[..cut])
   }
   ```
5. The error view: the two types of § Contracts (`ConfigErrorKind`, `ConfigErrorView`), then, verbatim:
   ```rust
   /// `error` as a kind and a key path. Figment and serde texts are never read, except the
   /// prefix that marks a TOML syntax error, whose text `map_figment` already cut to a position.
   pub(crate) fn error_view(error: &ConfigError) -> ConfigErrorView {
       match error {
           ConfigError::MissingFile(_) => view(ConfigErrorKind::MissingFile, None, None),
           ConfigError::Invalid { key, reason } => {
               view(ConfigErrorKind::Invalid, Some((*key).to_owned()), Some(*reason))
           }
           ConfigError::Provider { key, reason } => {
               view(ConfigErrorKind::Provider, Some(key.clone()), Some(*reason))
           }
           ConfigError::Figment(error) => figment_view(error),
       }
   }

   /// A failure of [`crate::config::read_toml`]: the file is missing, or could not be read
   /// (whose figment text is an I/O message, never shown).
   pub(crate) fn read_error_view(error: &ConfigError) -> ConfigErrorView {
       match error {
           ConfigError::MissingFile(_) => view(ConfigErrorKind::MissingFile, None, None),
           _ => view(ConfigErrorKind::UnreadableFile, None, None),
       }
   }

   fn figment_view(error: &figment::Error) -> ConfigErrorView {
       let Some(part) = error.clone().into_iter().next() else {
           return view(ConfigErrorKind::Message, None, None);
       };
       let key = (!part.path.is_empty()).then(|| part.path.join("."));
       let kind = match &part.kind {
           Kind::Message(text) if part.path.is_empty() && text.starts_with("TOML parse error") => {
               ConfigErrorKind::TomlSyntax
           }
           Kind::Message(_) => ConfigErrorKind::Message,
           Kind::InvalidType(..) => ConfigErrorKind::InvalidType,
           Kind::InvalidValue(..) => ConfigErrorKind::InvalidValue,
           Kind::InvalidLength(..) => ConfigErrorKind::InvalidLength,
           Kind::UnknownVariant(..) => ConfigErrorKind::UnknownVariant,
           Kind::UnknownField(..) => ConfigErrorKind::UnknownField,
           Kind::MissingField(_) => ConfigErrorKind::MissingField,
           Kind::DuplicateField(_) => ConfigErrorKind::DuplicateField,
           Kind::ISizeOutOfRange(_) | Kind::USizeOutOfRange(_) => ConfigErrorKind::OutOfRange,
           Kind::Unsupported(_) | Kind::UnsupportedKey(..) => ConfigErrorKind::Unsupported,
       };
       view(kind, key, None)
   }

   fn view(kind: ConfigErrorKind, key: Option<String>, reason: Option<&'static str>) -> ConfigErrorView {
       ConfigErrorView { kind, key, reason }
   }
   ```
6. In `observability/mod.rs` add `pub(crate) mod config_view;`.

**Tests** (in `config_view.rs`):
- The test module holds the deny side of the classification:
  ```rust
  /// Keys whose value the bundle never shows: secrets and email addresses.
  const REDACTED_KEYS: &[&str] = &[
      "encryption_key",
      "license_key",
      "smtp_url",
      "email_from",
      "auth.github.client_secret",
      "auth.google.client_secret",
      "auth.oidc[].client_secret",
      "init.admin_email",
      "init.admin_password",
      "init.secret_key",
      "init.read_key",
      "self_monitoring.otlp_headers",
      // Some vendors put a token in the endpoint's path (M1's `OtlpEndpoint` logs its host
      // only); a running server's `debug_info.self_monitoring.endpoint_host` gives the host.
      "self_monitoring.otlp_endpoint",
  ];
  const REFERENCE: &str = include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../../website/src/content/docs/docs/self-hosting/configuration-reference.md"
  ));
  ```
  and a helper `fn reference_keys() -> BTreeSet<String>`. For each line starting with ``| ` ``, it takes the text between the first two backticks, as `config.rs`'s `documented` does.
- `every_reference_key_is_classified`:
  - every reference key is in exactly one of `SHOWN_KEYS` and `REDACTED_KEYS`;
  - every entry of both lists is a reference key;
  - neither list has a duplicate.
  The failure messages name the key and say `classify it in observability/config_view.rs (SHOWN_KEYS or the test's REDACTED_KEYS)`.
- A test helper `fn tree_with(paths: &[(&str, Value)]) -> Value`. It builds a JSON object from dotted paths: a segment ending in `[]` becomes an array holding one object, and later paths under the same `x[]` go into that same object.
- `a_planted_value_in_every_secret_key_never_appears`:
  1. Build a tree with every `REDACTED_KEYS` path set to `"PLANTED-{i}-sekret"`, except `init.admin_email` and `email_from`, which get `"planted{i}@leak.example"`.
  2. Render `redact(&tree)`. The JSON text contains neither `PLANTED` nor `leak.example`.
  3. Each path reads `"<redacted>"`; for `auth.oidc[].client_secret`, that is `values["auth"]["oidc"][0]["client_secret"]`.
- `every_shown_key_shows_its_value`: a tree with every `SHOWN_KEYS` path set to `"v-{i}"` → each reads `"v-{i}"`.
- `an_unknown_key_is_redacted_and_named`: `{"zz_unknown": "x", "log": {"level": "info", "extra": 1}}` → `zz_unknown` and `log.extra` read `"<redacted>"`, and `log.level` reads `"info"`.
- `a_table_where_a_shown_scalar_belongs_is_redacted`: `{"log": {"level": {"a": "PLANTED"}}}` → `log.level.a` reads `"<redacted>"`; no `PLANTED`.
- `arrays_of_scalars_are_one_leaf`: `trusted_proxies = ["10.0.0.0/8"]` is shown whole; `init.secret_key = ["PLANTED"]` is `"<redacted>"`.
- `shown_strings_are_scrubbed_and_capped`:
  - `init.org = "ada@leak.example"` → `"<redacted>"`;
  - `storage.s3.endpoint = "https://u:PLANTED@minio.example"` → `"https://<redacted>@minio.example"`;
  - `log.level` of 5000 `a` → 4096 bytes plus `…<truncated>`.
- `numbers_and_booleans_convert`: `figment_to_json` of `FigmentValue::Num(Tag::Default, Num::U8(3))`, `Num(Tag::Default, Num::I64(-4))`, `Num(Tag::Default, Num::U128(u128::MAX))`, `Num(Tag::Default, Num::F64(0.5))`, `FigmentValue::Bool(Tag::Default, true)`, `FigmentValue::Empty(Tag::Default, figment::value::Empty::None)` → `3`, `-4`, `"340282366920938463463374607431768211455"`, `0.5`, `true`, `null` (import `figment::value::Tag`).
- `source_tree_merges_the_file_and_the_environment` (uses `crate::config::source_tree`):
  - TOML `"[log]\nlevel = \"info\"\nencryption_key = …"`. The TOML is top-level-first: put `encryption_key = "PLANTED-file"` before `[log]`.
  - env `[("CETRA_LOG", "debug"), ("CETRA_SMTP_URL", "smtp://u:PLANTED@h")]`.
  - `redacted_values(&tree)`: `log.level == "debug"` (env wins), `encryption_key` and `smtp_url` read `"<redacted>"`, and no `PLANTED` anywhere.
- `oidc_client_secrets_from_the_environment_are_not_in_the_tree`: env `[("CETRA_AUTH__OIDC__OKTA__CLIENT_SECRET", "PLANTED")]` → the text has no `PLANTED`.
- `cetra_telemetry_is_not_in_the_tree`: env `[("CETRA_TELEMETRY", "off")]` → the tree has no `telemetry` key.
- `error_view_never_quotes_a_value`, each through `crate::config::load_from`. For each case, `serde_json::to_string(&error_view(&e))` has no `PLANTED`:
  - `http_addr = "PLANTED-nope"` → key `http_addr`;
  - `[storage.wal]\nsync = "PLANTED"` → key `storage.wal.sync`, kind `unknown_variant` or `message` (assert the key, and that the kind is one of the two);
  - `smtp_url = "PLANTED://x"` → kind `invalid`, key `smtp_url`, reason `Some("invalid URL")` or `Some("expected a string")`.
- `a_toml_syntax_error_is_toml_syntax`: `source_tree(Some("encryption_key = \"PLANTED"), &[])` is `Err(e)`; `error_view(&e)` is kind `TomlSyntax` with key `None`; no `PLANTED`.
- `an_unknown_key_is_unknown_field`: `load_from(Some("zz_probe = 1\n"), &[])` → kind `UnknownField`; the key contains `zz_probe`.
- `a_bad_cetra_telemetry_is_a_provider_error`: env `[("CETRA_TELEMETRY", "PLANTED")]` → kind `Provider`, key `CETRA_TELEMETRY`; no `PLANTED` (M4's `TELEMETRY_VALUES` reason).
- `read_errors_have_their_own_kinds`: `read_error_view(&ConfigError::MissingFile(PathBuf::from("/x")))` → `MissingFile`; `read_error_view(&ConfigError::Figment(Box::new(figment::Error::from("x".to_owned()))))` → `UnreadableFile`.
- `error_kinds_serialize_snake_case`: `serde_json::to_string(&ConfigErrorKind::TomlSyntax) == "\"toml_syntax\""` and `UnknownField` → `"unknown_field"`.

**Commit:** `Show the configuration by allow-list for the debug bundle`

---

### TL3.4 — Host facts

**Goal.** The host part of the bundle: OS, kernel, CPU count, memory, cgroup memory limit and deployment kind, read without the network.

**Edit:**
- `crates/server/src/observability/host.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)
- `crates/server/Cargo.toml` (one line; shared with M5, which adds `backtrace`: additive, keep its line)
- `Cargo.lock` (rewritten by `cargo metadata`; if another milestone's lock change is uncommitted in the checkout, wait for the coordinator)

**M4 names used:** `facts::{Deployment, deployment, probe_host}`.

**Steps.**
1. In `crates/server/Cargo.toml` `[dependencies]`, add `sysinfo.workspace = true` in alphabetical position. Run `cargo info sysinfo@0.40.3` (read-only) and report its version and licence lines. Run `cargo metadata --format-version 1 --offline > /dev/null`. `git diff Cargo.lock` must show only `"sysinfo"` added to `cetra-server`'s dependency list (plus nothing else of yours); if it shows more, stop and report.
2. Create `host.rs`. Module doc: `//! Facts about the host for the debug bundle (track TL § 4): operating system, kernel, CPU count, memory and the cgroup memory limit, read from the OS through `sysinfo`; nothing leaves the process. The memory calls repeat those of `cetra_storage`'s `memory::detected_memory`, which is `pub(crate)` in that crate and so not reachable from here (storage's public API does not change in this track).` Contents: the `HostFacts` type of § Contracts, then, verbatim:
   ```rust
   /// The host facts now.
   pub(crate) fn host_facts() -> HostFacts {
       let (total_memory_bytes, container_memory_limit_bytes) = memory();
       HostFacts {
           os: std::env::consts::OS,
           os_version: System::long_os_version().ok(),
           kernel_release: System::kernel_version().ok(),
           arch: std::env::consts::ARCH,
           cpus: std::thread::available_parallelism().ok().map(NonZeroUsize::get),
           total_memory_bytes,
           container_memory_limit_bytes,
           deployment: deployment(probe_host()),
       }
   }

   /// The machine's RAM and this process's cgroup memory limit (its own cgroup and its
   /// ancestors, cgroup v2 or v1; `sysinfo` reports only limits below the RAM). Either is
   /// `None` when the OS reports nothing; the limit is always `None` off Linux.
   fn memory() -> (Option<u64>, Option<u64>) {
       let Ok(mut system) = System::new_with_specifics(
           RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
       ) else {
           return (None, None);
       };
       let total = Some(system.total_memory()).filter(|bytes| *bytes > 0);
       let Ok(pid) = sysinfo::get_current_pid() else {
           return (total, None);
       };
       system.refresh_processes_specifics(
           ProcessesToUpdate::Some(&[pid]),
           false,
           ProcessRefreshKind::nothing(),
       );
       let limit = system
           .process(pid)
           .and_then(Process::cgroup_limits)
           .map(|limits| limits.total_memory)
           .filter(|bytes| *bytes > 0);
       (total, limit)
   }
   ```
   Imports: `std::num::NonZeroUsize`, `serde::Serialize`, `sysinfo::{MemoryRefreshKind, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System}`, `super::facts::{Deployment, deployment, probe_host}`. Each `sysinfo` call above is the one `crates/storage/src/memory.rs` `detected_memory` makes (compare them). `System::long_os_version` and `System::kernel_version` are associated functions returning `Result<String, sysinfo::Error>` (`sysinfo-0.40.3/src/common/system.rs` lines ~831 and ~872).
3. In `observability/mod.rs` add `pub(crate) mod host;` at its alphabetical place.

**Tests** (in `host.rs`):
- `host_facts_name_this_platform`:
  - `os == std::env::consts::OS` and `arch == std::env::consts::ARCH`;
  - `cpus` is `Some(n)` with `n > 0`;
  - on Linux (`if cfg!(target_os = "linux")`), `total_memory_bytes` is `Some`;
  - when both `total_memory_bytes` and `container_memory_limit_bytes` are `Some`, the limit is `<=` the total.
- `there_is_no_cgroup_limit_off_linux`: `if !cfg!(target_os = "linux") { assert_eq!(memory().1, None); }`.
- `host_facts_serialize_with_the_contract_keys`: the keys of `serde_json::to_value(host_facts())` are exactly `os`, `os_version`, `kernel_release`, `arch`, `cpus`, `total_memory_bytes`, `container_memory_limit_bytes` and `deployment`, in that order.

**Commit:** `Read host facts for the debug bundle`

---

### TL3.5 — Data directory facts read from disk

**Goal.** Sizes and format markers of `meta.db`, `catalog.db` and the WAL segments, and the catalog's live files per table and level, read without opening the storage engine and without writing anything.

**Edit:**
- `crates/server/src/observability/disk.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)

**Steps.**
1. Create `disk.rs`. Module doc:

   > What the data directory holds, read from disk without the storage engine (track TL § 4, D-TL-6): file sizes, the format markers of `meta.db`, `catalog.db` and the WAL segments, and the catalog's live files per table and level. The debug bundle reads it when no server answers on `admin.sock`. The server's `debug_info` reads the catalog counts. The SQLite connections are read-only, and immutable when no server runs, so reading writes no file, not even `-shm`.

   Imports: `std::collections::BTreeSet`, `std::fs::{File, TryLockError}`, `std::io::Read`, `std::path::{Path, PathBuf}`, `serde::{Deserialize, Serialize}`, `sqlx::{ConnectOptions as _, Connection as _}` (as `backup.rs` imports them: `connect` and `close` are trait methods), `sqlx::sqlite::SqliteConnectOptions`.
2. Contents: `LevelFiles` (`pub`) and the four `DiskFacts` types of § Contracts, then:
   ```rust
   /// The first bytes of a WAL segment (spec 03 § 1, `cetra_storage::wal::segment`): the magic,
   /// then the format version as a little-endian `u16`.
   const WAL_SEGMENT_MAGIC: [u8; 4] = *b"LWAL";
   /// Live files per table and level (`crates/storage/src/catalog/schema.txt`).
   const CATALOG_LEVELS_SQL: &str = "SELECT table_name, level, COUNT(*), COALESCE(SUM(size_bytes), 0) FROM data_files WHERE state = 'active' GROUP BY table_name, level ORDER BY table_name, level";
   ```
3. `pub(crate) async fn catalog_levels(path: &Path, immutable: bool) -> Result<Vec<LevelFiles>, sqlx::Error>`, verbatim:
   ```rust
   let mut connection = SqliteConnectOptions::new()
       .filename(path)
       .read_only(true)
       .immutable(immutable)
       .create_if_missing(false)
       .disable_statement_logging()
       .connect()
       .await?;
   let rows = sqlx::query_as::<_, (String, i64, i64, i64)>(CATALOG_LEVELS_SQL)
       .fetch_all(&mut connection)
       .await;
   // Closing a read-only connection cannot lose data; its error is not interesting.
   let _ = connection.close().await;
   Ok(rows?
       .into_iter()
       .map(|(table, level, files, bytes)| LevelFiles {
           table,
           level,
           files: u64::try_from(files).unwrap_or(0),
           bytes: u64::try_from(bytes).unwrap_or(0),
       })
       .collect())
   ```
   Doc: "`immutable` is for a directory no server has open: SQLite then takes no lock and reads no `-wal`. With a running server, pass `false`: a WAL reader sees the committed state and blocks no writer."
4. `async fn database_versions(path: &Path) -> Result<(i64, i64), sqlx::Error>`: the same connection options with `immutable(true)`. Then `PRAGMA user_version` and `SELECT COALESCE(MAX(version), 0) FROM _sqlx_migrations`, both with `query_scalar::<_, i64>`, inside one `async` block as `backup::read_versions` does. Then `let _ = connection.close().await;` and return the block's result.
5. `async fn db_file_facts(path: &Path) -> DbFileFacts`, in order:
   1. `present = path.is_file()`, `bytes = file_len(path)`, `wal_bytes = file_len(&sidecar(path, "-wal"))`.
   2. Not present → `DbFileFacts { present: false, bytes: 0, wal_bytes, readable: false, format: None, schema: None }`.
   3. Else `match database_versions(path).await`: `Ok((format, schema))` → `readable: true, format: Some(format), schema: Some(schema)`; `Err(_)` → `readable: false`, both `None`.
6. `fn wal_dir_facts(dir: &Path) -> WalDirFacts`, in order:
   1. `std::fs::read_dir(dir)`; on `Err` return all zero, with `present: false`.
   2. For each entry: skip any entry whose name does not end in `.wal` or that is not a file (`entry.metadata()`, `is_file()`; skip on error). `segments += 1`, and `bytes += metadata.len()` (saturating).
   3. `read_header(&path)`:
      - `Some(header)` whose first four bytes equal `WAL_SEGMENT_MAGIC` → insert `u16::from_le_bytes([header[4], header[5]])` into a `BTreeSet<u16>`;
      - otherwise `bad_headers += 1`.
   4. Return `WalDirFacts { present: true, segments, bytes, header_versions: set.into_iter().collect(), bad_headers }`.
7. Helpers, verbatim:
   ```rust
   /// The first 6 bytes of a file, `None` when shorter or unreadable.
   fn read_header(path: &Path) -> Option<[u8; 6]> {
       let mut header = [0_u8; 6];
       let mut file = std::fs::File::open(path).ok()?;
       file.read_exact(&mut header).ok()?;
       Some(header)
   }

   /// The size of a regular file, 0 when absent or not a file.
   fn file_len(path: &Path) -> u64 {
       std::fs::metadata(path)
           .map(|metadata| if metadata.is_file() { metadata.len() } else { 0 })
           .unwrap_or(0)
   }

   /// `path` with `suffix` appended to its file name (`meta.db` → `meta.db-wal`).
   fn sidecar(path: &Path, suffix: &str) -> PathBuf {
       let mut name = path.as_os_str().to_owned();
       name.push(suffix);
       PathBuf::from(name)
   }
   ```
8. The lock probe, verbatim:
   ```rust
   /// Whether a process holds `<data_dir>/LOCK`: `cetra serve` holds an exclusive `flock` on it
   /// for as long as its storage is open (`cetra_storage`'s `lock_dir`). Opens the file
   /// read-only, never creating it, and tries a shared lock without waiting; a lock it gets is
   /// released when the handle drops at the end of this function. Absent or unreadable: not
   /// held.
   pub(crate) fn lock_held(data_dir: &Path) -> bool {
       let Ok(file) = File::open(data_dir.join("LOCK")) else {
           return false;
       };
       matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
   }
   ```
   (`File::try_lock_shared` is stable since Rust 1.89; the toolchain is 1.99.1.)
9. `pub(crate) async fn disk_facts(data_dir: &Path) -> DiskFacts`, in order:
   1. `let catalog = data_dir.join("catalog.db");`
   2. `let catalog_files = if catalog.is_file() { catalog_levels(&catalog, true).await.ok() } else { None };`
   3. Return:
      ```rust
      DiskFacts {
          data_dir_present: data_dir.is_dir(),
          meta_db: db_file_facts(&data_dir.join("meta.db")).await,
          catalog_db: db_file_facts(&catalog).await,
          catalog_files,
          wal: WalFacts {
              spans: wal_dir_facts(&data_dir.join("wal").join("spans")),
              scores: wal_dir_facts(&data_dir.join("wal").join("scores")),
          },
      }
      ```
10. In `observability/mod.rs` add `pub mod disk;` at its alphabetical place (`pub`, because `LevelFiles` appears in the `pub` `DebugInfo`).

**Tests** (in `disk.rs`; `#[tokio::test]`):
- A helper `fn listing(dir: &Path) -> Vec<(String, u64)>`: every file under `dir`, recursive, as a relative path and a size, sorted.
- `a_stopped_server_s_directory_is_read`:
  1. `crate::serve::bind(crate::test_support::config(&data), None)`, then `running.shutdown()`.
  2. `disk_facts(&data)`:
     - `data_dir_present`;
     - `meta_db.present && meta_db.readable`, `meta_db.format == Some(i64::from(crate::meta::META_FORMAT_VERSION))`, `meta_db.schema == Some(crate::meta::expected_version())`;
     - `catalog_db.readable` and `catalog_db.format == Some(1)`;
     - `catalog_files == Some(vec![])`;
     - both WAL dirs `present`, `bad_headers == 0`, and `header_versions` either empty or `[1]`.
- `reading_writes_nothing`: after shutdown, `listing(&data)` before and after `disk_facts(&data)` are equal.
- `a_missing_directory_has_nothing`: `disk_facts` of a path that does not exist → `data_dir_present == false`, both DBs `present == false`, `catalog_files == None`, both WAL dirs `present == false`.
- `wal_headers_are_classified`. In a temp `wal/spans`, write:
  - `00000000000000000001.wal` = `b"LWAL\x01\x00\x00\x00"`;
  - `00000000000000000002.wal` = `b"LWAL\x02\x00\x00\x00rest"`;
  - `00000000000000000003.wal` = `b"LW"`;
  - `00000000000000000004.wal` = `b"XXXXXXXX"`;
  - `notes.txt` = `b"LWAL\x09\x00"`.
  `wal_dir_facts` → `segments == 4`, `header_versions == [1, 2]`, `bad_headers == 2`, and `bytes` equals the sum of the four `.wal` sizes.
- `catalog_levels_groups_live_files`:
  1. Create a SQLite file with `SqliteConnectOptions::new().filename(p).create_if_missing(true)`, holding `CREATE TABLE data_files (table_name TEXT NOT NULL, level INTEGER NOT NULL, size_bytes INTEGER NOT NULL, state TEXT NOT NULL)` and the rows `('spans',0,10,'active')`, `('spans',0,5,'active')`, `('spans',1,100,'active')`, `('scores',0,7,'active')`, `('spans',0,999,'obsolete')`. Close it.
  2. `catalog_levels(p, true)` → `[scores/0: 1 file 7 bytes, spans/0: 2 files 15 bytes, spans/1: 1 file 100 bytes]`.
- `a_corrupt_database_is_unreadable`: a `catalog.db` file of 100 `x` bytes → `db_file_facts` gives `present: true, readable: false, format: None`.
- `the_lock_is_seen_while_a_server_runs`: with a server bound by `crate::serve::bind(crate::test_support::config(&data), None)`, `lock_held(&data)` is `true`; after `running.shutdown()`, it is `false`. Comment: the storage releases its lock at shutdown; the file may stay.
- `a_missing_lock_file_is_not_held_and_not_created`: in an empty temp dir, `lock_held` is `false` and no `LOCK` file exists afterwards.
- `level_files_round_trip`: `serde_json` of a `LevelFiles` deserializes to the same value.

**Commit:** `Read the data directory's sizes and format markers from disk`

---

### TL3.6 — `debug_info`

**Goal.** The `DebugInfo` type and the function that fills it from the running server's state, with a read-only instance id getter and the `/readyz` checks as a reusable function.

**Edit:**
- `crates/server/src/observability/debug_info.rs` (new)
- `crates/server/src/observability/instance.rs` (M4's file: one function and one import)
- `crates/server/src/http/health.rs`
- `crates/server/src/observability/mod.rs` (one `mod` line)

**M4 names used:**
- `facts::{VERSION, process_uptime, deployment, probe_host}`;
- `switchboard::{for_config, Switch}` and `OffReason::as_str`;
- `instance.rs`: its import `use crate::meta::instance_settings::{TELEMETRY_INSTANCE_ID, get_setting_tx, insert_setting_if_absent};` and `use crate::meta::{MetaDb, MetaError};`;
- `switchboard::Decision` (M4: `heartbeat: Switch`; M5 may have added `crash_reports: Switch`);
- `heartbeat::{collect, storage_kind, StorageKind}` with `collect(meta: &MetaDb, config: &ServerConfig, instance_id: Uuid, deployment: Deployment, uptime: Duration, now_ns: i64) -> Result<Heartbeat, MetaError>`.

**M1 names used:** `SelfMonitoringConfig::{exports, logs_endpoint}`, its fields `otlp_endpoint: Option<OtlpEndpoint>` and `level: String`, and `OtlpEndpoint::host`.

**Shared files, additive.** `instance.rs` may have been edited by M5: add your import and function, keep its lines. `mod.rs`: one line at its alphabetical place.

**Steps.**
1. In `http/health.rs`, move the checks out of `readyz`:
   ```rust
   /// The readiness checks that fail now, in `/readyz` order (D-N-30): empty when ready. The
   /// object-store probe goes through the cache. Also read by the admin socket's `debug_info`.
   pub(crate) async fn failed_checks(state: &AppState) -> Vec<&'static str> {
       // the body of `readyz` from `let mut failed = Vec::new();` through the object-store
       // push, unchanged
   }
   ```
   `readyz` becomes `let failed = failed_checks(&state).await;`, followed by its existing `if failed.is_empty() { … } else { … }` unchanged.
2. In `instance.rs`, add `get_setting` to the `use crate::meta::instance_settings::{…}` list. After `instance_id`, add:
   ```rust
   /// This instance's id when one was created, without creating it: a read-only command (the
   /// debug bundle) must not create the id (D-TL-9).
   ///
   /// # Errors
   ///
   /// [`MetaError`] when the read fails, or [`MetaError::Corrupt`] when the stored value is not
   /// a UUID.
   pub(crate) async fn existing_instance_id(meta: &MetaDb) -> Result<Option<Uuid>, MetaError> {
       match get_setting(meta.reader(), TELEMETRY_INSTANCE_ID).await? {
           None => Ok(None),
           Some(value) => Uuid::parse_str(&value).map(Some).map_err(|_| MetaError::Corrupt {
               table: "instance_settings",
               column: "value",
           }),
       }
   }
   ```
3. Create `debug_info.rs`:
   - Module doc: `//! The admin socket's `debug_info` (track TL § 4): the running server's state for a bug report. Every part that fails is listed in `errors` by name and static kind, so the answer always comes back.`
   - The `DebugInfo` types of § Contracts, verbatim, with these imports:
     - `serde::{Deserialize, Serialize}`, `uuid::Uuid`;
     - `cetra_storage::{StorageHealth, StorageStats}`;
     - `crate::http::AppState`;
     - `super::disk::{LevelFiles, catalog_levels}`;
     - `super::ring::LogRing`;
     - `super::{facts, heartbeat, instance, switchboard}`;
     - `super::switchboard::Switch`;
     - `super::heartbeat::StorageKind`.
4. Conversions, verbatim. A full destructuring, so a new storage field stops the build here:
   ```rust
   impl From<[u64; 2]> for PerTable {
       fn from([spans, scores]: [u64; 2]) -> Self {
           Self { spans, scores }
       }
   }

   impl From<StorageStats> for StatsView {
       fn from(stats: StorageStats) -> Self {
           let StorageStats {
               wal_bytes,
               flushes_ok,
               flushes_failed,
               flush_nanos,
               compactions_ok,
               compactions_failed,
               compactions_deferred,
               compactions_conflict,
               compaction_bytes_written,
               gc_objects_deleted,
           } = stats;
           Self {
               wal_bytes: wal_bytes.into(),
               flushes_ok: flushes_ok.into(),
               flushes_failed: flushes_failed.into(),
               flush_nanos: flush_nanos.into(),
               compactions_ok,
               compactions_failed,
               compactions_deferred,
               compactions_conflict,
               compaction_bytes_written,
               gc_objects_deleted,
           }
       }
   }

   impl From<StorageHealth> for HealthView {
       fn from(health: StorageHealth) -> Self {
           let StorageHealth {
               spans_wal_failed,
               scores_wal_failed,
               objects_sync_failed,
               catalog_failed,
               memtable_bytes,
               memtable_hard_limit,
               disk_low,
               disk_free_bytes,
               disk_min_free_bytes,
           } = health;
           Self {
               spans_wal_failed,
               scores_wal_failed,
               objects_sync_failed,
               catalog_failed,
               memtable_bytes,
               memtable_hard_limit,
               disk_low,
               disk_free_bytes,
               disk_min_free_bytes,
           }
       }
   }

   impl SwitchView {
       fn of(switch: Switch) -> Self {
           match switch {
               Switch::On => Self { on: true, reason: None },
               Switch::Off(reason) => Self { on: false, reason: Some(reason.as_str().to_owned()) },
           }
       }
   }
   ```
5. The `meta.db` counts, verbatim:
   ```rust
   const META_COUNTS_SQL: &str = "SELECT \
       (SELECT COUNT(*) FROM orgs), \
       (SELECT COUNT(*) FROM orgs WHERE state = 'active'), \
       (SELECT COUNT(*) FROM projects WHERE state = 'active'), \
       (SELECT COUNT(*) FROM users), \
       (SELECT COUNT(*) FROM users WHERE disabled_at IS NOT NULL), \
       (SELECT COUNT(*) FROM api_keys WHERE revoked_at IS NULL), \
       (SELECT COUNT(*) FROM personal_tokens WHERE revoked_at IS NULL), \
       (SELECT COUNT(*) FROM alert_rules), \
       (SELECT COUNT(*) FROM online_rules)";

   async fn meta_counts(state: &AppState) -> Result<MetaCounts, sqlx::Error> {
       let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64, i64, i64, i64, i64)>(META_COUNTS_SQL)
           .fetch_one(state.meta.reader())
           .await?;
       let count = |value: i64| u64::try_from(value).unwrap_or(0);
       Ok(MetaCounts {
           orgs: count(row.0),
           active_orgs: count(row.1),
           active_projects: count(row.2),
           users: count(row.3),
           disabled_users: count(row.4),
           api_keys: count(row.5),
           personal_tokens: count(row.6),
           alert_rules: count(row.7),
           online_rules: count(row.8),
       })
   }
   ```
6. `fn self_monitoring_state(config: &crate::config::SelfMonitoringConfig) -> SelfMonitoringState`:
   - `exporting = config.exports()`, `logs = config.logs_endpoint().is_some()`;
   - `endpoint_host = config.otlp_endpoint.as_ref().map(|e| e.host().to_owned())`;
   - `level = config.level.clone()`;
   - `reason`: `None` when `exporting`; else `Some("no_endpoint")` when `otlp_endpoint` is `None`; else `Some("logs_off")`. Owned `String`s.
7. `pub(crate) async fn collect(state: &AppState, ring: Option<&LogRing>) -> DebugInfo`. Doc: "The running server's state now. Never fails: a part that cannot be read is empty and listed in `errors`." Steps, in order:
   1. `let mut errors = Vec::new();` and a local `let mut fail = |section: &str, kind: &str| errors.push(SectionError { section: section.to_owned(), kind: kind.to_owned() });`. If the borrow checker refuses the closure, write a small `fn section_error(section, kind) -> SectionError` and push it instead.
   2. `logs`: `ring.map(|ring| { let snapshot = ring.snapshot(); LogsView { lines: snapshot.lines, dropped: snapshot.dropped, trimmed: false } })`.
   3. `stats = StatsView::from(state.storage.stats().await)`, `health = HealthView::from(state.storage.health())`.
   4. `catalog_files`: `catalog_levels(&state.config.data_dir.join("catalog.db"), false).await`. On `Err(_)`: `fail("storage.catalog_files", "unreadable")` and an empty `Vec`.
   5. `object_store`: `match heartbeat::storage_kind(&state.config) { StorageKind::File => "file", StorageKind::S3 => "s3" }.to_owned()`.
   6. `ready`: `let failed = crate::http::health::failed_checks(state).await;` then `ReadyView { ready: failed.is_empty(), failed: failed.iter().map(|name| (*name).to_owned()).collect() }`.
   7. `meta`: `meta_counts(state).await`. On `Err(_)`: `fail("meta", "query_failed")` and `None`.
   8. `telemetry`:
      1. `let decision = switchboard::for_config(&state.config);`
      2. `let instance_id = match instance::existing_instance_id(&state.meta).await { Ok(id) => id, Err(_) => { fail("telemetry.instance_id", "unreadable"); None } };`
      3. The preview:
         ```rust
         let preview = heartbeat::collect(
             &state.meta,
             &state.config,
             instance_id.unwrap_or(Uuid::nil()),
             facts::deployment(facts::probe_host()),
             facts::process_uptime(),
             state.clock.now_ns(),
         )
         .await;
         ```
         `Ok(heartbeat)` → `serde_json::to_value(&heartbeat).ok()`. `Err(_)` → `fail("telemetry.heartbeat_preview", "query_failed")` and `None`.
      4. `crash_reports`: when `switchboard::Decision` has a `crash_reports: Switch` field in the code you see (M5 landed first), `Some(SwitchView::of(decision.crash_reports))`. Otherwise `None`, with the comment `// M5 fills this from Decision.crash_reports (track TL M3 plan, § After the last phase).`
      5. `TelemetryState { heartbeat: SwitchView::of(decision.heartbeat), instance_id, endpoint: state.config.telemetry.endpoint.clone(), heartbeat_preview, crash_reports }`.
   9. Return `DebugInfo { schema: DEBUG_INFO_SCHEMA, version: facts::VERSION.to_owned(), uptime_secs: facts::process_uptime().as_secs(), logs, storage: StorageView { object_store, stats, health, catalog_files }, ready, meta, self_monitoring: self_monitoring_state(&state.config.self_monitoring), telemetry, errors }`.
8. A test-only sample, after `collect` and before `mod tests`, marked `#[cfg(test)]`: `pub(crate) fn sample() -> DebugInfo`. It returns a fixed value whose fields are all zero, `false`, empty or `None`, except:
   - `schema: 1`, `version: "0.0.0-test"`;
   - `logs: Some(LogsView { lines: vec!["{\"fields\":{\"message\":\"sample\"}}".into()], dropped: 0, trimmed: false })`;
   - `object_store: "file"`;
   - `ready.ready: true`;
   - `self_monitoring.level: "warn"` with `reason: Some("no_endpoint")`;
   - `telemetry.heartbeat: SwitchView { on: false, reason: Some("debug_build") }` and `endpoint: "http://127.0.0.1:9"`.
   TL3.7 and TL3.8 use it.
9. In `observability/mod.rs` add `pub mod debug_info;` (`pub`: `AdminResponse` carries it).

**Tests.**
- In `health.rs`: `failed_checks_is_empty_when_ready` (`TestApp::new()`: `failed_checks(&app.state).await.is_empty()`); `failed_checks_names_low_disk` (as `low_disk_fails_readiness_but_not_liveness`, waiting until the monitor reports low: the list contains `"free disk"`). The existing `/readyz` tests stay unchanged and pass.
- In `instance.rs`:
  - `existing_instance_id_is_none_and_creates_nothing` (`meta::test_db()`): `None`, and `get_setting(…, TELEMETRY_INSTANCE_ID)` is still `None`;
  - `existing_instance_id_reads_a_created_id` (`instance_id` first: equal);
  - `existing_instance_id_refuses_a_corrupt_value` (`put_setting(…, "not-a-uuid")` → `matches!(…, Err(MetaError::Corrupt { .. }))`).
- In `debug_info.rs`:
  - `a_fresh_server_reports_its_state`: `TestApp::new()`, then `collect(&app.state, None)`:
    - `schema == 1`, `version == facts::VERSION`, `errors.is_empty()`;
    - `ready.ready` and `storage.catalog_files.is_empty()`;
    - `meta == Some(MetaCounts { all zero })`;
    - `storage.object_store == "file"`.
  - `counts_follow_the_rows`: `TestApp::new()`, `seed()` → `meta.orgs == 1`, `active_orgs == 1`, `active_projects == 1`, `users == 1`.
  - `the_telemetry_state_says_why_and_creates_no_id`: `TestApp::new()`. Then:
    - `telemetry.heartbeat == SwitchView { on: false, reason: Some("debug_build".into()) }` (a test build) and `instance_id == None`;
    - `heartbeat_preview` is `Some(v)` with `v["schema"] == 1` and `v["instance_id"] == "00000000-0000-0000-0000-000000000000"`;
    - afterwards `get_setting(app.state.meta.reader(), TELEMETRY_INSTANCE_ID)` is `None`.
  - `an_existing_instance_id_is_shown`: `instance::instance_id(&app.state.meta)` first → `telemetry.instance_id == Some(id)`, and the preview's `instance_id` is `id.to_string()`.
  - `the_ring_lines_are_returned`: a `LogRing` with two pushed lines → `logs == Some(LogsView { lines: [both], dropped: 0, trimmed: false })`.
  - `self_monitoring_reasons`: a default config → `exporting: false`, `reason: Some("no_endpoint")`. With `otlp_endpoint = Some(parse_otlp_endpoint("https://otlp.example.com").unwrap())` and `logs = false` → `reason: Some("logs_off")`, `endpoint_host: Some("otlp.example.com")`. With `logs = true` → `exporting: true`, `reason: None`.
  - `debug_info_round_trips`: `serde_json::from_value::<DebugInfo>(serde_json::to_value(sample())?)` equals `sample()`.
  - `the_key_set_is_fixed`: the top-level keys of `serde_json::to_value(sample())` are exactly `schema`, `version`, `uptime_secs`, `logs`, `storage`, `ready`, `meta`, `self_monitoring`, `telemetry` and `errors`, in order.
  - `no_planted_value_reaches_debug_info`:
    1. `TestApp::with_config(|c| { c.email_from = Some("Planted <noreply@planted-domain.example>".into()); c.auth.signup_domains = vec!["planted-domain.example".into()]; })`, then `seed()`.
    2. `serde_json::to_string(&collect(&app.state, None).await)` contains none of `planted`, `noreply@` and `ada@example.com` (the email `seed()` gives its user; `Seeded` has no email field). A regression guard: `DebugInfo` holds none of these by construction.
  - `debug_info_modules_use_no_network`: the non-test code (cut at the first `#[cfg(test)]`, as M4's licence test does) of `include_str!("debug_info.rs")` and `include_str!("disk.rs")` contains none of `reqwest`, `TcpStream`, `UdpSocket` and `lookup_host`.

**Commit:** `Collect the server state for debug_info`

---

### TL3.7 — The generalized admin socket

**Goal.** The socket accepts `debug_info` beside `backup`, answers it with a new response variant under a size cap, and every existing line, code and test keeps its meaning (D-R-36).

**Edit:**
- `crates/server/src/admin_socket.rs`
- `crates/server/src/golden_tests/build.rs`
- `crates/cli/src/main.rs` (one `match` arm in `backup_command`; shared, additive: keep other milestones' edits)

**Steps.**
1. Module doc of `admin_socket.rs`: replace the sentence ``"`cetra admin backup` is the only client in M2."`` with ``"Its clients are `cetra admin backup` (command `backup`) and `cetra debug-bundle` (command `debug_info`, track TL M3)."``
2. Imports: add `crate::observability::debug_info::{self, DebugInfo}` and `crate::observability::ring`.
3. Add the two constants of § Contracts (`COMMANDS`, `MAX_RESPONSE`) after `MAX_REQUEST`.
4. `AdminRequest`, add after `Backup`:
   ```rust
   /// The running server's state for a bug report (`observability::debug_info::collect`).
   DebugInfo,
   ```
5. `AdminResponse`, add between `Done` and `Failed`. The order matters: the enum is untagged, and a backup answer must still decode as `Done`:
   ```rust
   /// The `debug_info` answer (`ok` is `true`).
   Info {
       v: u32,
       ok: bool,
       debug_info: Box<DebugInfo>,
   },
   ```
   Extend the enum's doc: `Variants are tried in order: a line with `report` is `Done`, one with `debug_info` is `Info`, else `Failed`.`
6. In `parse`, replace:
   ```rust
   if let Some(command) = value.get("command").and_then(Value::as_str)
       && command != "backup"
   ```
   with:
   ```rust
   if let Some(command) = value.get("command").and_then(Value::as_str)
       && !COMMANDS.contains(&command)
   ```
   Change the doc's "then the command" to "then the command (one of [`COMMANDS`])".
7. In `serve_connection`, add the arm `Ok(AdminRequest::DebugInfo) => debug_info_response(&state).await,` after the `Backup` arm.
8. Add, after `run_backup`:
   ```rust
   /// Collects the server's state and trims its log lines so the line fits [`MAX_RESPONSE`].
   async fn debug_info_response(state: &AppState) -> AdminResponse {
       let mut info = debug_info::collect(state, ring::installed()).await;
       fit_response(&mut info);
       AdminResponse::Info {
           v: ADMIN_PROTOCOL,
           ok: true,
           debug_info: Box::new(info),
       }
   }

   /// Room for the envelope around the debug info (`{"v":1,"ok":true,"debug_info":…}`) and the
   /// newline.
   const ENVELOPE_BYTES: usize = 64;

   /// Drops the oldest tenth of the log lines (at least one) until the serialized info fits in
   /// [`MAX_RESPONSE`] with its envelope, and marks the lines trimmed. Without log lines left,
   /// it stops: the rest is a few kilobytes.
   fn fit_response(info: &mut DebugInfo) {
       loop {
           let size = serde_json::to_vec(info).map_or(usize::MAX, |bytes| bytes.len());
           if size.saturating_add(ENVELOPE_BYTES) <= MAX_RESPONSE {
               return;
           }
           let Some(logs) = info.logs.as_mut() else {
               return;
           };
           if logs.lines.is_empty() {
               return;
           }
           let drop = (logs.lines.len() / 10).max(1);
           logs.lines.drain(..drop);
           logs.trimmed = true;
       }
   }
   ```
9. In `golden_tests/build.rs` `build`:
   - after `lines.push(send_raw(&data, json!({"command": "backup", "to": "/nowhere"})).await);` add `lines.push(send_raw(&data, json!({"v": ADMIN_PROTOCOL, "command": "debug_info"})).await);`;
   - append `None` to `expected_codes`;
   - extend the comment above `expected_codes`: `…, the three refusals name their codes, and debug_info answers (track TL M3).`
   - in `write_socket_lines`, after the `if let Some(report) = …` block, normalize the `debug_info` answer so `just golden-dir` writes the same line on every run:
     ```rust
     if let Some(info) = response.get_mut("debug_info").and_then(Value::as_object_mut) {
         // Values that differ per run or machine (track TL M3); the replay checks `ok` and
         // `code` and decodes the line, which a null `Option` and a zero still do.
         info.insert("uptime_secs".to_owned(), json!(0));
         info.insert("logs".to_owned(), Value::Null);
         if let Some(telemetry) = info.get_mut("telemetry").and_then(Value::as_object_mut) {
             telemetry.insert("heartbeat_preview".to_owned(), Value::Null);
         }
         if let Some(health) = info
             .get_mut("storage")
             .and_then(|storage| storage.get_mut("health"))
             .and_then(Value::as_object_mut)
         {
             health.insert("disk_free_bytes".to_owned(), json!(0));
         }
         if let Some(stats) = info
             .get_mut("storage")
             .and_then(|storage| storage.get_mut("stats"))
             .and_then(Value::as_object_mut)
         {
             for key in ["wal_bytes", "flush_nanos"] {
                 stats.insert(key.to_owned(), json!({"spans": 0, "scores": 0}));
             }
         }
     }
     ```
     Extend the function's doc: `…, the times of the report redacted, and the per-run values of the debug_info answer zeroed.`
10. In `crates/cli/src/main.rs` `backup_command`, add to the `match decode_response(&line)…` an arm before `AdminResponse::Failed`:
    ```rust
    AdminResponse::Info { .. } => {
        anyhow::bail!("the server answered a backup request with debug info")
    }
    ```

**Tests** (in `admin_socket.rs` `mod tests`):
- `a_debug_info_request_returns_the_server_state`:
  1. `bind(config(dir))`, then `ask` with `{"v":1,"command":"debug_info"}\n`.
  2. `decode_response` gives `Info { ok: true, debug_info, .. }`, with `debug_info.schema == 1`, `debug_info.version == env!("CARGO_PKG_VERSION")` and `debug_info.ready.ready`.
  3. Shut down.
- `a_debug_info_request_round_trips`:
  - `serde_json::to_string(&AdminEnvelope { v: ADMIN_PROTOCOL, request: AdminRequest::DebugInfo })` equals `{"v":1,"command":"debug_info"}`;
  - `parse` of that line is `Ok(AdminRequest::DebugInfo)`.
- `extra_request_fields_are_ignored`: `parse(br#"{"v":1,"command":"debug_info","later":true}"#)` is `Ok(AdminRequest::DebugInfo)`.
- `a_backup_answer_still_decodes_as_done`: the `Done` line of `the_client_refuses_another_protocol` decodes as `Done`, not `Info`.
- `the_client_reads_an_info_answer_with_an_unknown_field`: `serde_json::to_value(AdminResponse::Info { v: 1, ok: true, debug_info: Box::new(debug_info::sample()) })`, add `"later": 1` at the top and `"later": 1` inside `debug_info` → `decode_response` gives `Ok(Info { .. })`.
- `fit_response_trims_the_oldest_lines`:
  1. `sample()` with `logs.lines` = 3000 lines of 2048 bytes each (`format!("{i:06}{}", "a".repeat(2042))`).
  2. After `fit_response`:
     - `serde_json::to_vec(&info).len() + ENVELOPE_BYTES <= MAX_RESPONSE`;
     - `trimmed` is set;
     - the last line still starts with `002999`.
- `fit_response_leaves_a_small_info_alone`: `sample()` is unchanged, and `trimmed` stays `false`.
- `every_response_carries_the_protocol_version`: add `ask(&socket, "{\"v\":1,\"command\":\"debug_info\"}\n").await` to `lines`.
- `the_error_codes_are_stable`:
  - add `"{\"v\":1,\"command\":\"debug_info\"}"` to the requests loop. It parses, so it adds no code;
  - add `assert!(matches!(parse(b"{\"v\":1,\"command\":\"debug_info\"}"), Ok(AdminRequest::DebugInfo)));`.
  The expected list stays exactly the same 13 codes.
- In `golden_tests/build.rs` no new test: `the golden check` (step 9 of `golden_tests/mod.rs`) replays the new line and decodes the normalized answer, which proves the normalization keeps it decodable.
- `a_backup_request_returns_the_report`: add the arm `AdminResponse::Info { .. } => panic!("{response}")`.

**Commit:** `Answer debug_info on the admin socket`

---

### TL3.8 — The bundle

**Goal.** Assemble the bundle: build and host facts, the config view and its error, the server's `debug_info` over the socket or the data directory read from disk. Embed the log lines as JSON, scrub every string, cap the bundle at 8 MiB, and write it as a new 0600 file.

**Edit:**
- `crates/server/src/observability/bundle.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)

**M4 names used:** `facts::{VERSION, TARGET, profile, Profile}` (checked in the committed `facts.rs`).

**Steps.**
1. Create `bundle.rs`:
   - Module doc:

     > `cetra debug-bundle` (track TL § 4, D-TL-6): one local JSON file for a bug report. Build and host facts, the configuration by allow-list, and the running server's `debug_info` over `admin.sock`, or the data directory read from disk when no server answers. The command contacts no network. Log lines and shown config strings go through the scrubber. The file is at most 8 MiB, mode 0600, and never overwrites.

   - Imports:
     - `std::io::{self, Write as _}`, `std::os::unix::fs::OpenOptionsExt as _`, `std::path::{Path, PathBuf}`, `std::time::Duration`;
     - `chrono::{DateTime, SecondsFormat, Utc}`, `serde::Serialize`, `serde_json::Value`;
     - `tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}`, `tokio::net::UnixStream`;
     - `crate::admin_socket::{ADMIN_PROTOCOL, ADMIN_SOCKET, AdminEnvelope, AdminRequest, AdminResponse, MAX_RESPONSE, decode_response}`;
     - `crate::config`;
     - `super::config_view::{ConfigErrorView, error_view, read_error_view, redacted_values}`;
     - `super::debug_info::DebugInfo`;
     - `super::disk::{DiskFacts, disk_facts, lock_held}`;
     - `super::facts::{self, Profile}`;
     - `super::host::{HostFacts, host_facts}`;
     - `super::scrub::scrub_text`.
   - The bundle types and the constants of § Contracts (`BUNDLE_SCHEMA`, `MAX_BUNDLE_BYTES`, `REVIEW_NOTICE` (`pub`), `Bundle`, `BuildFacts`, `ConfigView`, `ServerSection`), then:
   ```rust
   /// How long the bundle waits to connect to `admin.sock`.
   const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
   /// How long it waits for the `debug_info` answer.
   const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
   /// The `not_running` section's advice.
   const NOT_RUNNING_HINT: &str = "no cetra serve answered on <data_dir>/admin.sock and none holds <data_dir>/LOCK. If the server runs, this data directory is not its own: pass --config, or set CETRA_CONFIG or CETRA_DATA_DIR, as the server's environment does (a relative data_dir is read from the current directory). If the server does not start, attach its last log lines too: docker compose logs --tail 500 cetra, kubectl logs <pod> --tail 500, or journalctl -u <unit> -n 500";
   /// The `unreachable` reason when `LOCK` is held but the socket does not answer.
   const LOCK_HELD_REASON: &str = "the data directory is in use but admin.sock does not answer: the server is starting, or its data directory path is too long for a Unix socket (about 100 bytes)";

   /// Why the bundle was not written.
   #[derive(Debug, thiserror::Error)]
   pub enum BundleError {
       /// `serde_json` refused the bundle.
       #[error("the bundle could not be encoded as JSON")]
       Encode,
       /// Over [`MAX_BUNDLE_BYTES`] even without log lines.
       #[error("the bundle is larger than 8 MiB without its log lines")]
       TooLarge,
       /// The output path exists: the bundle never overwrites.
       #[error("{} already exists; choose another --output", .0.display())]
       Exists(PathBuf),
       /// The file could not be written.
       #[error("writing {}", path.display())]
       Write {
           /// The output path.
           path: PathBuf,
           /// The I/O error.
           #[source]
           source: io::Error,
       },
   }
   ```
2. `pub fn file_name(now: DateTime<Utc>) -> String`: `format!("cetra-debug-{}.json", now.format("%Y%m%dT%H%M%SZ"))`.
3. `pub fn write_new(path: &Path, bytes: &[u8]) -> Result<(), BundleError>`, verbatim:
   ```rust
   let write_error = |source| BundleError::Write { path: path.to_path_buf(), source };
   let mut file = match std::fs::OpenOptions::new()
       .write(true)
       .create_new(true)
       .mode(0o600)
       .open(path)
   {
       Ok(file) => file,
       Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
           return Err(BundleError::Exists(path.to_path_buf()));
       }
       Err(source) => return Err(write_error(source)),
   };
   file.write_all(bytes).map_err(write_error)?;
   file.sync_all().map_err(write_error)
   ```
4. `pub async fn render(config_path: Option<&Path>, ui_embedded: bool, now: DateTime<Utc>) -> Result<Vec<u8>, BundleError>`. Doc: "The bundle as JSON bytes. Reads the configuration as `cetra serve` does: `config_path`, else `CETRA_CONFIG`, then the process's `CETRA_*` variables. A variable whose name or value is not UTF-8 is skipped, so it never makes this command panic." Body:
   ```rust
   let env: Vec<(String, String)> = std::env::vars_os()
       .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
       .collect();
   let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
   let bundle = build(config_path, &pairs, ui_embedded, now).await;
   let mut document = document(&bundle)?;
   encode(&mut document)
   ```
5. `pub(crate) async fn build(config_path: Option<&Path>, env: &[(&str, &str)], ui_embedded: bool, now: DateTime<Utc>) -> Bundle`. Steps, in order:
   1. `let (config, config_error, data_dir) = config_section(config_path, env);`
   2. `let server = server_section(&data_dir).await;`
   3. Return:
      ```rust
      Bundle {
          schema: BUNDLE_SCHEMA,
          created_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
          build: BuildFacts {
              version: facts::VERSION,
              target: facts::TARGET,
              profile: facts::profile(),
              ui_embedded,
          },
          host: host_facts(),
          config,
          config_error,
          server,
      }
      ```
6. `fn config_section(config_path: Option<&Path>, env: &[(&str, &str)]) -> (ConfigView, Option<ConfigErrorView>, PathBuf)`. Steps, in order:
   1. `let file = config::configured_path(config_path).map(|p| p.display().to_string());`
   2. `let env_names`: every name in `env` whose first 6 bytes equal `CETRA_` ignoring ASCII case (`name.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("CETRA_"))`), owned, sorted, deduplicated.
   3. `let mut error = None;`
   4. `let toml = match config::read_toml(config_path) { Ok(toml) => toml, Err(e) => { error = Some(read_error_view(&e)); None } };`
   5. `let read_failed = error.is_some();`
   6. `let tree = match config::source_tree(toml.as_deref(), env) { Ok(tree) => Some(tree), Err(e) => { error.get_or_insert(error_view(&e)); None } };`
   7. `let values = tree.as_ref().map_or_else(|| serde_json::Value::Object(serde_json::Map::new()), redacted_values);`
   8. `let loaded = if read_failed { None } else { match config::load_from(toml.as_deref(), env) { Ok(config) => Some(config), Err(e) => { error.get_or_insert(error_view(&e)); None } } };`. When the file could not be read, the load is skipped: a missing file must not also report the errors of a config without it.
   9. The data directory:
      - `Some(config)` → `config.data_dir.clone()`;
      - else the tree's `data_dir` when it is a string (`tree.as_ref().and_then(|t| t.get("data_dir")).and_then(|v| v.as_str()).map(PathBuf::from)`). `figment::value::Value::as_str` exists;
      - else `config::default_data_dir()`.
   10. Return `(ConfigView { file, env_names, values }, error, data_dir)`.
7. `async fn server_section(data_dir: &Path) -> ServerSection`. Every arm below `return`s its section; the closure `unreachable` borrows `shown`, so the `NotRunning` arm passes `shown.clone()`. Steps, in order:
   1. `let socket = data_dir.join(ADMIN_SOCKET);` `let shown = data_dir.display().to_string();`
   2. `let unreachable = |reason: &str| ServerSection::Unreachable { data_dir: shown.clone(), reason: reason.to_owned() };`
   3. Connect: `let stream = match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&socket)).await { … }` with these arms:
      - `Err(_)` → `return unreachable("connecting timed out")`;
      - `Ok(Err(e))` with kind `NotFound` or `ConnectionRefused`:
        - if `lock_held(data_dir)` → `return unreachable(LOCK_HELD_REASON)` (a server holds the directory: its files are not read);
        - else `return ServerSection::NotRunning { data_dir: shown.clone(), disk: disk_facts(data_dir).await, hint: NOT_RUNNING_HINT }`;
      - `Ok(Err(e))` otherwise → `return unreachable(&e.kind().to_string())` (a static description, never a path);
      - `Ok(Ok(stream))` → `stream`.
   4. `let response = match tokio::time::timeout(ANSWER_TIMEOUT, ask_debug_info(stream)).await { Err(_) => return unreachable("the server did not answer within 30 s"), Ok(Err(reason)) => return unreachable(reason), Ok(Ok(response)) => response };`
   5. `match response`:
      - `AdminResponse::Info { debug_info, .. }` → `ServerSection::Running { debug_info }`;
      - `AdminResponse::Failed { code, message, .. }` → `ServerSection::Refused { code, message }`;
      - `AdminResponse::Done { .. }` → `unreachable("the server answered with a backup report")`.
8. `async fn ask_debug_info(mut stream: UnixStream) -> Result<AdminResponse, &'static str>`, in order:
   1. The request line: `serde_json::to_string(&AdminEnvelope { v: ADMIN_PROTOCOL, request: AdminRequest::DebugInfo })`. Map an error to `"the request could not be encoded"`. Append `\n`.
   2. `stream.write_all(…)`; an error → `"the request could not be sent"`.
   3. Read: `let mut line = Vec::new(); BufReader::new(stream.take(MAX_RESPONSE as u64 + 1)).read_until(b'\n', &mut line).await`. An error → `"the answer could not be read"`.
   4. `line.is_empty()` → `"the server closed the connection without an answer"`. `line.len() > MAX_RESPONSE` → `"the answer is larger than 4 MiB"`.
   5. `decode_response(&String::from_utf8_lossy(&line))`. Map an error to `"the server's answer is not admin protocol 1"`.
9. The document, the log lines and the scrub, verbatim:
   ```rust
   /// The bundle as the JSON document that is written: each log line of a `running` server that
   /// parses as a JSON object embedded as that object (other lines stay strings), then every
   /// string and object key scrubbed (the second guard, track TL § 4).
   pub(crate) fn document(bundle: &Bundle) -> Result<Value, BundleError> {
       let mut document = serde_json::to_value(bundle).map_err(|_| BundleError::Encode)?;
       if let Some(lines) = log_lines(&mut document) {
           for line in lines.iter_mut() {
               if let Some(text) = line.as_str()
                   && let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(text)
               {
                   *line = parsed;
               }
           }
       }
       scrub_value(&mut document);
       Ok(document)
   }

   /// `server.debug_info.logs.lines` of a `running` bundle.
   fn log_lines(document: &mut Value) -> Option<&mut Vec<Value>> {
       document
           .pointer_mut("/server/debug_info/logs/lines")
           .and_then(Value::as_array_mut)
   }

   /// Every string of `value`, and every object key, through [`scrub_text`]. Two keys that
   /// scrub to the same text keep the later value.
   fn scrub_value(value: &mut Value) {
       match value {
           Value::String(text) => *text = scrub_text(text),
           Value::Array(items) => items.iter_mut().for_each(scrub_value),
           Value::Object(map) => {
               let entries = std::mem::take(map);
               for (key, mut child) in entries {
                   scrub_value(&mut child);
                   map.insert(scrub_text(&key), child);
               }
           }
           Value::Null | Value::Bool(_) | Value::Number(_) => {}
       }
   }
   ```
10. `fn encode(document: &mut Value) -> Result<Vec<u8>, BundleError>`, verbatim:
    ```rust
    loop {
        let mut bytes = serde_json::to_vec_pretty(document).map_err(|_| BundleError::Encode)?;
        bytes.push(b'\n');
        if bytes.len() <= MAX_BUNDLE_BYTES {
            return Ok(bytes);
        }
        let Some(lines) = log_lines(document).filter(|lines| !lines.is_empty()) else {
            return Err(BundleError::TooLarge);
        };
        let drop = (lines.len() / 10).max(1);
        lines.drain(..drop);
        if let Some(trimmed) = document.pointer_mut("/server/debug_info/logs/trimmed") {
            *trimmed = Value::Bool(true);
        }
    }
    ```
11. In `observability/mod.rs` add `pub mod bundle;` at its alphabetical place (first in the list).

**Tests** (in `bundle.rs`; `#[tokio::test]` where async). A helper `fn now() -> DateTime<Utc>` returns `Utc.with_ymd_and_hms(2026, 11, 23, 10, 20, 30).unwrap()` (import `chrono::TimeZone`). A helper `fn toml_for(data: &Path) -> String` returns `format!("data_dir = {:?}\n", data.display().to_string())`, written to `<temp>/cetra.toml`.
- `a_running_server_is_asked_over_the_socket`:
  1. `bind(test_support::config(&data), None)`, and a config file whose `data_dir` is `data`.
  2. `build(Some(&file), &[], true, now())`:
     - `server` is `Running`, with `debug_info.version == facts::VERSION`;
     - `config_error.is_none()`;
     - `config.values["data_dir"] == data.display().to_string()`;
     - `build.ui_embedded`;
     - `created_at == "2026-11-23T10:20:30Z"`.
  3. Shut down.
- `a_stopped_server_gets_the_not_running_section`:
  1. Bind, then shut down.
  2. `build` → `NotRunning { disk, hint, .. }`:
     - `disk.meta_db.format == Some(i64::from(crate::meta::META_FORMAT_VERSION))` and `disk.catalog_db.readable`;
     - `hint` contains `docker compose logs` and `journalctl`.
- `the_not_running_bundle_writes_nothing_to_the_data_dir`: after shutdown, the sorted `(relative path, size)` list of `data` is the same before and after `build`.
- `a_held_lock_without_a_socket_is_unreachable`: bind a server, then remove `<data>/admin.sock` (`std::fs::remove_file`) while it runs → `server` is `Unreachable { reason, .. }` with `reason == LOCK_HELD_REASON`; shut down.
- `a_missing_data_dir_is_not_running`: a config pointing at `<temp>/absent` → `NotRunning` with `disk.data_dir_present == false`.
- `an_unusable_socket_is_unreachable`: create a regular file named `admin.sock` in `data` → `server` is `Unreachable` (the error kind is not `NotFound`/`ConnectionRefused`). If the platform reports `ConnectionRefused` for a regular file, assert `NotRunning` instead: accept either and assert the bundle was built. Comment why.
- `a_missing_config_file_is_reported`: `build(Some(Path::new("/nonexistent/cetra.toml")), &[], false, now())` → `config_error == Some(ConfigErrorView { kind: MissingFile, key: None, reason: None })`; `host.os` is set; `config.values == {}`.
- `a_toml_syntax_error_still_gives_a_bundle`: the file `encryption_key = "PLANTED` → `config_error.kind == TomlSyntax`, and the written bytes (`encode(&mut document(&bundle)?)`) have no `PLANTED`. "The whole JSON" below means these bytes.
- `an_invalid_secret_never_appears`: `smtp_url = "smtp://u:PLANTED@h:notaport"` → `config_error.key == Some("smtp_url")`, `config.values["smtp_url"] == "<redacted>"`, and the whole JSON has no `PLANTED`.
- `an_invalid_shown_key_names_the_key_without_its_error_text`: `http_addr = "PLANTED-nope"` → `config_error.key == Some("http_addr")`, and `serde_json::to_string(&bundle.config_error)` has no `PLANTED`. The shown value itself is visible by design (D-TL-6).
- `env_names_are_listed_without_values`: env `[("CETRA_LOG", "debug"), ("cetra_encryption_key", "PLANTED-key"), ("HOME", "/x")]` → `env_names == ["CETRA_LOG", "cetra_encryption_key"]` (sorted as bytes), and no `PLANTED` in the JSON.
- `document_embeds_json_log_lines_and_keeps_others`: a `Running` bundle (built by hand, `server: ServerSection::Running { debug_info: Box::new(info) }`, the other fields from a `NotRunning` test bundle) whose `info` is `debug_info::sample()` with `logs.lines = ["{\"fields\":{\"message\":\"hi\"}}", "not json", "[1,2]"]` → `document["server"]["debug_info"]["logs"]["lines"]` is `[{"fields": {"message": "hi"}}, "not json", "[1,2]"]` (an array line stays a string).
- `document_scrubs_every_string_and_key`: in such a bundle, set:
  - a log line `{"user":"ada@example.com","k":"sk-cetra-abc"}`;
  - `config.values = {"zz_ada@leak.example": "<redacted>"}`;
  - `config_error = Some(ConfigErrorView { kind: UnknownField, key: Some("x.bob@leak.example".into()), reason: None })`;
  - `debug_info.telemetry.endpoint = "https://u:p@relay.example"`;
  - `debug_info.ready.failed = vec!["carol@leak.example".into()]`.
  The serialized `document` contains none of `ada@`, `leak.example`, `sk-cetra-abc` and `u:p@`, and the log line's `user` field reads `<redacted>`.
- `encode_trims_logs_to_fit`: a hand-built `Running` bundle whose debug info is `sample()` with 6000 lines of 2048 bytes (12 MiB, lines `format!("{i:06}{}", "a".repeat(2042))`, not JSON); `let mut doc = document(&bundle)?`:
  - `encode(&mut doc)?.len() <= MAX_BUNDLE_BYTES`;
  - `doc["server"]["debug_info"]["logs"]["trimmed"] == true`;
  - the last remaining line starts with `005999`.
- `encode_fails_when_too_large_without_logs`: a `NotRunning` bundle whose `config.values` is an object of 3000 keys of 4096-byte strings → `encode(&mut document(&bundle)?)` is `Err(BundleError::TooLarge)`.
- `the_bundle_keys_are_fixed`: for a `NotRunning` bundle, the top-level keys are exactly `schema`, `created_at`, `build`, `host`, `config`, `config_error` and `server`, in order; `server["state"] == "not_running"`; `build`'s keys are `version`, `target`, `profile` and `ui_embedded`.
- `server_states_serialize_snake_case`: `serde_json::to_value` of `Unreachable { data_dir: "/d".into(), reason: "r".into() }` → `{"state": "unreachable", "data_dir": "/d", "reason": "r"}`; `Refused` → `"state": "refused"`.
- `file_name_is_a_utc_timestamp`: `file_name(now()) == "cetra-debug-20261123T102030Z.json"`.
- `write_new_makes_a_0600_file_and_never_overwrites`:
  - writing to a fresh path → the content matches and `mode & 0o777 == 0o600`;
  - a second `write_new` to the same path → `Err(BundleError::Exists(_))`, and the content is unchanged.
- `write_new_names_the_path_on_failure`: writing into a missing directory → `Err(BundleError::Write { path, .. })` with that path.
- `the_bundle_modules_contact_no_network`: the non-test code of `include_str!("bundle.rs")`, `include_str!("host.rs")`, `include_str!("config_view.rs")` and `include_str!("scrub.rs")` contains none of `reqwest`, `TcpStream`, `UdpSocket`, `lookup_host` and `ToSocketAddrs`.

**Commit:** `Assemble the debug bundle`

---

### TL3.9 — `cetra debug-bundle`

**Goal.** The CLI subcommand writes the bundle to a new file or to stdout and prints the review notice.

**Edit:**
- `crates/cli/src/debug_bundle.rs` (new)
- `crates/cli/src/main.rs` (shared with M2, M5 and M6: additive; keep every line another milestone added, including commands, arms and tests)

**Steps.**
1. Create `debug_bundle.rs`. Module doc: `//! `cetra debug-bundle` (track TL § 4): writes the bundle `cetra_server::observability::bundle` renders, to a new file or to stdout, then prints the review notice. Contacts no network.` Contents:
   ```rust
   use std::io::Write;
   use std::path::{Path, PathBuf};

   use anyhow::Context;
   use cetra_server::observability::bundle::{self, REVIEW_NOTICE};

   /// Writes the bundle to `output` (default: `cetra-debug-<UTC time>.json` in the current
   /// directory; `-` is stdout) and prints where, then [`REVIEW_NOTICE`]. With `-`, the notice
   /// goes to `err` so `out` holds the JSON only.
   pub(crate) async fn run(
       config: Option<&Path>,
       output: Option<&Path>,
       ui_embedded: bool,
       out: &mut impl Write,
       err: &mut impl Write,
   ) -> anyhow::Result<()> {
       let now = chrono::Utc::now();
       let bytes = bundle::render(config, ui_embedded, now)
           .await
           .context("building the debug bundle")?;
       if output == Some(Path::new("-")) {
           out.write_all(&bytes)?;
           out.flush()?;
           writeln!(err, "{REVIEW_NOTICE}")?;
           return Ok(());
       }
       let path = output.map_or_else(|| PathBuf::from(bundle::file_name(now)), Path::to_path_buf);
       let path = std::path::absolute(&path).context("resolving the output path")?;
       bundle::write_new(&path, &bytes)?;
       writeln!(out, "wrote {}", path.display())?;
       writeln!(out, "{REVIEW_NOTICE}")?;
       Ok(())
   }
   ```
2. In `main.rs`:
   1. Add `mod debug_bundle;` in alphabetical position among the `mod` lines at the top (after `mod credentials;`, before `mod health;`).
   2. Add the `DebugBundle` variant of § Contracts to `Command`, right after the `Health { … }` variant (line ~57).
   3. In `run_cli`'s `match cli.command`, add this arm right after `Command::Health { url } => health::check(&url).await?,` (line ~297):
      ```rust
      Command::DebugBundle { config, output } => {
          let (mut out, mut err) = (io::stdout().lock(), io::stderr().lock());
          debug_bundle::run(
              config.as_deref(),
              output.as_deref(),
              cetra_ui::embedded(),
              &mut out,
              &mut err,
          )
          .await?;
      }
      ```
   4. In the test helper `cli_reference` (line ~968), change the intro sentence to: ``"The `cetra` binary runs the server (`cetra serve`), administers a data directory (`cetra admin`), writes a file for bug reports (`cetra debug-bundle`) and talks to a running server (`cetra login`, `cetra query`, `cetra traces`, `cetra projects`, `cetra agents`). This page is generated from the binary's `--help`."`` Keep the string's line breaks and `\` continuations as they are.
   The regenerated `website/src/content/docs/docs/reference/cli.md` is the coordinator's (§ After the last phase). Do not edit it by hand.

**Tests.** In `debug_bundle.rs` `mod tests` (`#[tokio::test]`; each test writes a `cetra.toml` with `data_dir` in a `tempfile::tempdir()`, and no server runs):
- `it_writes_a_reviewable_file`:
  1. `run(Some(&toml), Some(&out_path), false, &mut out, &mut err)`.
  2. The file exists and parses as JSON with `server.state == "not_running"` and `schema == 1`.
  3. `out` is `wrote <absolute out_path>\n` followed by `REVIEW_NOTICE` and a newline; `err` is empty.
- `dash_writes_the_json_to_stdout`: `output = Some(Path::new("-"))` → `out` parses as JSON, `err` contains `review it before you share it`, and no file was created in the temp dir.
- `an_existing_output_is_not_overwritten`: write `x` to `out_path` first → `run` errs, the chain contains `already exists`, and the file still reads `x`.
- `an_invalid_config_still_writes_a_bundle`: the toml `http_addr = "nope"` → `Ok`, and the file's `config_error.key == "http_addr"`.
In `main.rs` tests:
- `debug_bundle_parses_its_flags`: `Cli::try_parse_from(["cetra", "debug-bundle", "--config", "c.toml", "--output", "-"])` matches `Command::DebugBundle { config: Some(c), output: Some(o) }` with `c == PathBuf::from("c.toml")` and `o == PathBuf::from("-")`; `["cetra", "debug-bundle"]` gives both `None`.
- `cli_reference_is_up_to_date` fails until the coordinator regenerates the page. That is expected; do not edit `cli.md`.

**Commit:** `Add cetra debug-bundle`

---

### TL3.10 — Docs, issue form, specs and changelog

**Goal.** Users find out how to write, check and attach a bundle. GitHub's issue form asks for it. Specs 09 and 12 and the CHANGELOG describe the command and the socket command.

**Edit:**
- `website/src/content/docs/docs/help/reporting-a-bug.mdx` (new)
- `website/astro.config.mjs`
- `website/src/content/docs/docs/help/support.mdx`
- `website/src/content/docs/docs/self-hosting/telemetry.md` (M4's page: one sentence)
- `.github/ISSUE_TEMPLATE/bug_report.yml` (new)
- `.github/ISSUE_TEMPLATE/config.yml` (new)
- `CHANGELOG.md`
- `docs/spec/09-api.md`
- `docs/spec/12-operations.md`

**Steps.**
1. Create `reporting-a-bug.mdx` with exactly this content:
   ````markdown
   ---
   title: Reporting a bug
   description: Write a debug bundle with cetra debug-bundle, check it, and attach it to a GitHub issue.
   ---

   This page shows how to put what we need to fix a bug into one file, and how to report it.

   ## Write a debug bundle

   `cetra debug-bundle` writes one JSON file with:

   - the cetra version and build, and the host's operating system, kernel, CPU count, memory and container memory limit;
   - your configuration as cetra reads it from the file and the `CETRA_*` variables, with the values of settings that can hold a secret or an email address replaced by `<redacted>`;
   - when the server is running: its uptime, its last 2,000 log lines, storage and `meta.db` statistics, the readiness checks, and the telemetry state with the heartbeat it would send;
   - when the server is not running: the sizes and format versions of the files in the data directory.

   The command reads the same configuration as `cetra serve` and asks the running server through `<data_dir>/admin.sock`. It never contacts the network. Run it as the user that runs the server (the socket answers only that user and root), with the server's configuration: the same `--config` file, and the same `CETRA_*` variables and working directory. A service manager's environment, such as a systemd unit's `Environment=` lines, is not in your shell, so pass `--config` and set `CETRA_DATA_DIR` as the unit does. Inside a container, `exec` already has the container's environment.

   On a host:

   ```bash
   cetra debug-bundle
   ```

   It prints the path of the file it wrote, `cetra-debug-<UTC time>.json` in the current directory. Add `--config <file>` if `cetra serve` uses one, and `--output <file>` to choose the path. The command never overwrites a file.

   With Docker Compose, write the file to your machine with `--output -`, which prints the JSON on standard output (the image has no shell to copy files with). `-T` keeps Compose from allocating a terminal, which would change the line endings:

   ```bash
   docker compose exec -T cetra cetra debug-bundle --output - > cetra-debug.json
   ```

   With Kubernetes:

   ```bash
   kubectl -n cetra exec cetra-0 -- cetra debug-bundle --output - > cetra-debug.json
   ```

   ## Check it before you share it

   Open the file and read it before you attach it to a public issue. The bundle replaces the values of secret and email settings (`encryption_key`, `license_key`, `smtp_url`, `email_from`, the `client_secret` of each sign-in provider, the `init` passwords and keys, `self_monitoring.otlp_headers`, and `self_monitoring.otlp_endpoint`, whose path can hold a token; a running server's section gives the endpoint's host) and of any setting it does not know with `<redacted>`. It keeps host names, bucket names, paths, domains and the names of your organization and project. Remove anything you do not want to share.

   Log lines never contain prompts, completions, keys or email addresses. As a second check, the bundle replaces anything that looks like an email address, a cetra key (`sk-cetra-…`, `pk-cetra-…`, `pat-cetra-…`) or a password in a URL with `<redacted>`.

   ## When the server does not start

   The bundle is written even when the configuration does not load: `config_error` names the setting and the kind of error, without the value. When the server is not running, attach its last log lines too:

   ```bash
   docker compose logs --tail 500 cetra
   kubectl -n cetra logs cetra-0 --tail 500
   journalctl -u cetra -n 500
   ```

   ## Open an issue

   1. Open a [bug report](https://github.com/cetra-ai/cetra/issues/new?template=bug_report.yml) on GitHub.
   2. Say what happened, what you expected, and how to reproduce it.
   3. Drag the bundle into the **Debug bundle** field. If GitHub refuses the `.json` file, rename it to `.txt`.

   Do not report a security problem in a public issue: see [Support and security](/docs/help/support/#report-a-vulnerability).

   ## What the bundle contains

   | Key | What it holds |
   |---|---|
   | `build` | `version`, `target` (the platform the binary was built for), `profile`, and whether the web UI is embedded. |
   | `host` | `os`, `os_version`, `kernel_release`, `arch`, `cpus`, `total_memory_bytes`, `container_memory_limit_bytes` and `deployment` (`binary`, `docker` or `kubernetes`). |
   | `config` | The config file's path, the names of the `CETRA_*` variables that are set (never their values), and the settings you set, redacted as above. Defaults are not listed: they are those of the [configuration reference](/docs/self-hosting/configuration-reference/) for the `build.version`. |
   | `config_error` | `null`, or the `kind` of error and the `key` it concerns. |
   | `server` | `state`: `running` with the server's `debug_info` (its log lines as JSON objects), `not_running` with the data directory's files, `unreachable` with a reason (for example a server that runs without its socket), or `refused` with the socket's error code. |

   `server.debug_info.telemetry` shows whether the heartbeat is on and why, the instance id, and the heartbeat the server would send now ([Telemetry](/docs/self-hosting/telemetry/)). The full command reference is [`cetra debug-bundle`](/docs/reference/cli/#cetra-debug-bundle).
   ````
2. In `astro.config.mjs` (additive: keep entries others added), in the Help group's `items`, add `{ label: "Reporting a bug", link: "/docs/help/reporting-a-bug/" },` right after the Troubleshooting entry.
3. In `support.mdx`, replace the first bullet of "Bugs and questions" with:
   ```markdown
   - **A bug or a missing feature:** open an issue at [github.com/cetra-ai/cetra/issues](https://github.com/cetra-ai/cetra/issues). For a bug, attach a debug bundle: [Reporting a bug](/docs/help/reporting-a-bug/) shows how to write one with `cetra debug-bundle` and what to check before you share it. Do not paste prompts, completions or keys.
   ```
4. In `telemetry.md` (shared with M5 and M6, which add sections: additive, keep them), at the end of the paragraph of "## The startup line" that starts "At every start, cetra logs one line", add the sentence: `` `cetra debug-bundle` shows the instance id too, with the heartbeat the server would send now ([Reporting a bug](/docs/help/reporting-a-bug/)). `` If the paragraph is not there (M4's TL4.9 wording changed), stop and report.
5. Create `.github/ISSUE_TEMPLATE/bug_report.yml` with exactly:
   ```yaml
   name: Bug report
   description: Something in cetra does not work as documented.
   labels: ["bug"]
   body:
     - type: markdown
       attributes:
         value: |
           Thanks for the report. Do not report a security problem here: see https://cetra.sh/docs/help/support/#report-a-vulnerability.
     - type: textarea
       id: what-happened
       attributes:
         label: What happened
         description: What you did, what you saw, and what you expected instead.
       validations:
         required: true
     - type: textarea
       id: reproduce
       attributes:
         label: How to reproduce
         description: The smallest steps that show the problem.
     - type: input
       id: version
       attributes:
         label: cetra version
         description: The output of `cetra version`.
         placeholder: cetra 0.1.0
       validations:
         required: true
     - type: dropdown
       id: deployment
       attributes:
         label: How you run cetra
         options:
           - The binary on a host
           - Docker or Docker Compose
           - Kubernetes (Helm chart)
           - npx @cetra-ai/cli init
           - Other
       validations:
         required: true
     - type: textarea
       id: bundle
       attributes:
         label: Debug bundle
         description: >-
           Run `cetra debug-bundle` (https://cetra.sh/docs/help/reporting-a-bug/), read the file,
           then drag it here. If GitHub refuses the .json file, rename it to .txt. If you cannot
           attach it, say why.
       validations:
         required: true
     - type: textarea
       id: logs
       attributes:
         label: Server log lines
         description: Only when the server does not start or stops: its last log lines (`docker compose logs --tail 500 cetra`, `kubectl logs`, `journalctl`).
         render: text
   ```
6. Create `.github/ISSUE_TEMPLATE/config.yml` with exactly:
   ```yaml
   blank_issues_enabled: true
   contact_links:
     - name: Report a vulnerability
       url: https://github.com/cetra-ai/cetra/security/advisories/new
       about: Report a security problem privately, not in a public issue.
     - name: Documentation
       url: https://cetra.sh/docs/
       about: Guides, troubleshooting and the FAQ.
   ```
7. In `CHANGELOG.md` `## [Unreleased]` `### Added`, append this line after the last existing line of that list (other milestones add lines too: keep them):
   ```markdown
   - `cetra debug-bundle` writes one JSON file for a bug report: build and host facts, the configuration with secret values redacted, and the running server's state and last log lines (or, when it is stopped, the data directory's file sizes and format versions). It contacts no network. The admin socket answers a new `debug_info` command for it. See https://cetra.sh/docs/help/reporting-a-bug/.
   ```
8. In `docs/spec/09-api.md` § CLI, add this row right after the `cetra admin backup` row:
   ```markdown
   | `cetra debug-bundle [--config <file>] [--output <file>\|-]` | one local JSON file for a bug report, at most 8 MiB, mode 0600, never overwriting: build and host facts, the configuration by allow-list (D-TL-6), and the running server's `debug_info` over `<data_dir>/admin.sock` or, without a server, the data directory's sizes and format versions read from disk; contacts no network (track TL M3) |
   ```
9. In `docs/spec/12-operations.md` § Backups, add this paragraph right after the paragraph that starts `**Online backups (track N's M2` (shared with M2, M5 and M6, which edit § Self-monitoring: additive):
   ```markdown
   **Debug bundle (track TL M3, D-TL-6):** `cetra debug-bundle` asks the server through the same socket with the command `debug_info` (additive within protocol 1, no new error code). The answer is one line of at most 4 MiB. It carries uptime, the last log lines (a ring of 2000 JSON lines at the stdout filter, at most 2 MiB, oldest dropped first when the line would be longer), storage statistics, live catalog files per table and level (read from `catalog.db` with a read-only connection), the `/readyz` checks, `meta.db` counts, the self-monitoring state, the telemetry switchboard decision and the heartbeat as it would be sent (the instance id is read, never created). When the socket does not answer and no process holds `<data_dir>/LOCK`, the bundle reads the sizes and format markers of `meta.db`, `catalog.db` (immutable read-only connections, so nothing is written) and the WAL segment headers; when `LOCK` is held, it reads nothing and says the socket does not answer. The configuration is shown as the merged file and `CETRA_*` values, where only allow-listed keys show their value; a load error is its key path and kind, never its text. In the written file, log lines that are JSON are embedded as objects, and every string has emails, cetra keys and URL passwords replaced by `<redacted>`.
   ```

**Tests.** None in Rust. The docs checks run at the milestone check: `check-terms`, `check-samples` (only `bash`/`yaml` blocks), `check-env-vars`, and the link validator. The link `/docs/reference/cli/#cetra-debug-bundle` resolves once the coordinator regenerates `cli.md`, and `/docs/self-hosting/telemetry/` exists since M4.

**Done when.** The files are created or changed as above and committed. Re-read the page against `website/STYLE.md` before committing:
- "cetra" lower case at every sentence start;
- no marketing words;
- headings in sentence case;
- internal links absolute with a trailing slash.

**Commit:** `Document cetra debug-bundle and ask for it in bug reports`

---

## After the last phase (coordinator)

1. **CLI reference.** Run `CETRA_DOCS_WRITE=1 just docs-gen`. It rewrites `website/src/content/docs/docs/reference/cli.md` and the skill references; only `cli.md` should change. Review that the diff adds `cetra debug-bundle` and the new intro sentence, then commit with `Regenerate the CLI reference for cetra debug-bundle`.
2. Once M2, M5 and M6 have all landed: classify in `config_view.rs` any of their config keys TL3.3 could not see (`self_monitoring.traces`, `self_monitoring.trace_sample_ratio`, `telemetry.crash_reports`, `analytics.posthog_host` shown; `analytics.posthog_key` redacted), and fill `TelemetryState.crash_reports` from `Decision.crash_reports` if TL3.6 left it `None`. The heartbeat preview needs nothing for M6's `usage` field: it is a `serde_json::Value`.
3. `just fmt`, then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`. Fix findings with a Haiku agent and an exact brief.
4. `just deny`: `sysinfo` is not a new crate, but `cetra-server`'s graph changed.
5. Check by hand:
   1. `just run serve` in one terminal. In another, run `just run debug-bundle --output -`:
      - the JSON shows `server.state == "running"` and recent log lines;
      - `config.values` hides any secret you set (`CETRA_ENCRYPTION_KEY=…`).
   2. Stop the server and run it again: `not_running` with format versions.
   3. With a broken `cetra.toml`, check that `config_error` is set.
6. The roadmap row of track TL M3 in `docs/roadmap.md`, and the Sonnet code review the user's rules require at the end of the plan.
