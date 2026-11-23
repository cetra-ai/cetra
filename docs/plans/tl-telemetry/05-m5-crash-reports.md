# TL M5 — Opt-in crash reports

**Summary.** This milestone lets an operator opt in to crash reports: with `telemetry.crash_reports = true`, every panic and every ERROR-level `tracing` event of `cetra serve` becomes a small JSON report of code-defined facts (code location with the path rule, callsite target and module, field **names**, demangled function names of the backtrace, bucketed uptime, build facts, a count and two Unix hours). Reports are written synchronously to `<data_dir>/telemetry/spool/` by a `tracing` layer and by the panic hook's M1 seam, deduplicated per location per hour and capped at 20 an hour, and a sender task posts them to `<telemetry.endpoint>/v1/crash` every minute with the instance id. Events bridged from the `log` crate (no file, no line) are not reported. The switchboard gains a `crash_reports` decision (rules 1–3 and the config; rule 4 does not apply) and a second startup notice line, and the heartbeat gains `features.crash_reports_on`. Leak tests, one of them in a child process with the real panic hook, prove that a planted `sk-cetra-` key, an email and a prompt never reach a spool file or a request body. The Telemetry docs page, the configuration reference, spec 12, the CHANGELOG and the requests to track Q are updated.

**Prerequisites.**
- Work in `/home/sebastien/travaux/llm-observer/.claude/worktrees/cross-track-review-6a41ad`, branch `claude/telemetry-9e2a24`.
- M1 and M4 are committed on this branch (up to `bcd67db Format track TL's M1 and M4 code`): `crates/server/src/observability/{mod,panic,facts,buckets,switchboard,instance,heartbeat,otlp,subscriber}.rs`, `config.rs` (`TelemetryConfig`), `serve.rs` (the heartbeat start in `bind`), `crates/cli/src/main.rs` (`panic::install` then `subscriber::init` in `serve`), `telemetry.md`, spec 12 and the Q notes file. Every anchor and name in this plan was checked against that code; each phase lists the names it relies on. **If a name, signature or quoted text differs, stop and report**; do not adapt silently.
- M2 (spans), M3 (debug bundle) and M6 (product analytics) are implemented in parallel in the same checkout and may have edited a shared file (`switchboard.rs`, `heartbeat.rs`, `subscriber.rs`, `observability/mod.rs`, `config.rs`, `serve.rs`, `backup.rs`, `telemetry.md`, the configuration reference, spec 12, `CHANGELOG.md`) before your phase runs. Every M5 edit to a shared file is **additive**: add a field, a line, a match arm, a row or a paragraph. **Re-read the file just before editing it, keep every addition another milestone made** (for example an extra field in `Decision`, `Inputs`, `Features`, `Observability` or the heartbeat golden JSON, or another layer in `build`), and put M5's addition next to them. If an M5 step cannot be done without removing or rewriting another milestone's code, stop and report. The coordinator serializes phases that share a file (§ Phase graph).

**Read first.**
- `AGENTS.md`: § Rules (no `unwrap`/`expect` outside tests without an invariant comment; never panic on disk input; `thiserror`), § Privacy, § Tests, § Commits and reports.
- `docs/plans/tl-telemetry/README.md`: § 3 Crash reports to us (all of it: the content table, the path rule, backtraces, the never-list, delivery, panics), § The switchboard, § Instance id, § Details settled while planning (uptime from process start), § Decisions D-TL-5, D-TL-10, D-TL-11, D-TL-12, § Requests to track Q (TL-Q1, TL-Q2), § Milestones M5.
- The committed code (the source of truth; the M1 and M4 plans only explain it):
  - `crates/server/src/observability/panic.rs` (`install`, `report`, `HookGuard`, `PANIC_EVENT_TARGET`), `subscriber.rs` (`BoxedLayer`, `ComposedSubscriber`, `Observability { logs }`, `init`, `build`), `switchboard.rs` (`Switch`, `OffReason`, `Decision`, `Inputs`, `decide`, `shared_off`, `for_config`, `NOTICE_TARGET`), `facts.rs` (`VERSION`, `TARGET`, `profile`, `Profile`, `Deployment`, `deployment`, `probe_host`, `process_uptime`, `mark_process_start`), `buckets.rs` (`UptimeBucket`, `uptime_bucket`), `instance.rs` (`instance_id`), `heartbeat.rs` (`Features`, `features`, `client`, `REQUEST_TIMEOUT`, `start`, and its tests: `golden`, `KEYS`, `FEATURE_KEYS`, `the_payload_matches_the_golden_json`, `features_follow_the_config`, the private `Receiver`, `start_when_off_logs_the_reason_and_spawns_nothing`, `start_when_on_logs_the_instance_id_and_spawns`);
  - `crates/server/src/config.rs` (`TelemetryConfig` and its `Default`, `load_from`, the test `the_configuration_reference_matches_the_loader`), `crates/server/src/serve.rs` (`bind`: `mark_process_start` near line 289, the `for_config` / `heartbeat::start` block near line 621; the restore-mode tests), `crates/server/src/backup.rs` (`write`: it copies only named files; the test `a_full_backup_has_every_file_and_a_manifest`), `crates/server/src/clock.rs` (`system_now_ns` uses `expect`, so the panic path must not call it), `crates/server/src/test_support/mod.rs` (`TestApp::{new, with_config}`, `capture_logs`, `config`, `MANUAL_NOW_NS` = 2026-01-01T00:00Z), `crates/cli/src/main.rs` `serve` (`panic::install()` before the config loads, then `subscriber::init`).
- `docs/plans/tl-telemetry/01-m1-self-monitoring-logs.md` § Interface for later milestones and `04-m4-heartbeat.md` TL4.6 and TL4.9, for the reasons behind that code.
- The crate sources under `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/`:
  - `backtrace-0.3.77/src/backtrace/mod.rs` (`trace`, `Frame`), `src/symbolize/mod.rs` (`resolve_frame`, `Symbol::name`, `SymbolName`'s `Display`, which forwards the formatter, so `{:#}` drops the `::h…` hash), `src/lib.rs` (`mod lock`: the global lock is re-entrant on one thread);
  - `tracing-subscriber-0.3.23/src/layer/mod.rs` (`Layer::on_event`, `Layer::with_filter`, `Layer::boxed`), `src/filter/filter_fn.rs`;
  - `tracing-core` `Metadata` (`file`, `line`, `target`, `module_path`, `fields`, `level`, `is_event`) and `field::Field::name` (`&'static str`).
- `crates/storage/src/crash_tests.rs:366-400` (`spawn_child`): the pattern for re-running the test binary as a child process.

## What it is, and what it is not

**What works when it's done:**
- `telemetry.crash_reports` (boolean, default `false`) in the config and the configuration reference.
- The switchboard decides `crash_reports`: `CETRA_TELEMETRY` off, `DO_NOT_TRACK` and restore mode turn it off (rules 1–3); debug builds and CI do not; then the config. A second startup line under `cetra_server::observability::notice` says whether crash reports are on, or why not.
- When on, a `tracing` layer turns each ERROR event (except M1's `panic` event, and except events bridged from the `log` crate, which have no file or line) into a report, and M1's panic hook turns each panic into one, both written synchronously to `<data_dir>/telemetry/spool/` (at most 50 files of at most 63 KiB, oldest dropped, written to a temporary file and renamed). Neither path logs, panics or deadlocks; errors are swallowed.
- A report holds exactly the fields of § `CrashReport` and nothing else: no message, no field value, no error text, no path outside the path rule, no exact time.
- A sender task posts spooled reports to `<telemetry.endpoint>/v1/crash` at startup and every minute, adding the instance id; it deletes a report on 2xx or on a 4xx other than 408 and 429, keeps it otherwise (408, 429, 1xx, 3xx, 5xx, network failure), and drops reports older than 7 days. Its own client: no redirects, 10 s, failures at `debug`.
- When crash reports are off, nothing is spooled and an existing spool is deleted at startup.
- Backups never copy the spool (`backup::write` copies named files only; a test proves it).
- The heartbeat carries `features.crash_reports_on`.
- Leak tests: in-process (the layer, the spool, the sender) and in a child process with the real hook and a real panic.
- Docs: the Telemetry page's crash-report section listing every field, the configuration reference row, CHANGELOG `Added`, spec 12, the backup page, and the crash body in the Q notes file.

**Out of scope:**
- The relay at `telemetry.cetra.sh` (track Q, TL-Q1). Until it exists, posts fail silently at `debug` and reports wait in the spool for 7 days.
- Crash reports from any command other than `cetra serve`.
- Server-side symbolication, file/line per frame (release archives keep only the symbol table, D-N-16).
- Showing crash state in `cetra debug-bundle` (M3 may read `Decision.crash_reports`).
- The roadmap row (the coordinator).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-TL-1 | `[telemetry]` is a config table; `crash_reports` is one of its keys |
| D-TL-5 | Crash reports carry code-defined facts only: no message, no field values, function names without lines (user) |
| D-TL-9 | Instance id: random UUIDv4 in `instance_settings`, created on first use |
| D-TL-10 | Crash reports go to `telemetry.cetra.sh`, a relay track Q runs |
| D-TL-11 | One switchboard: rules 1–3 turn crash reports off; rule 4 (debug, CI) is heartbeat only; then the config; licences never change it |
| D-TL-12 | Crash reports are spooled to disk before sending |
| D-N-16 | Release archives are stripped of DWARF but keep the symbol table (`tools/packaging/dist.sh`) |
| D-N-25 | Restore mode runs no outbound work |
| D166 | `telemetry.endpoint` is operator config, not passed through the egress guard |
| D172 | Licence verification stays offline; no telemetry code reads licence state |
| Design § Details settled while planning, bullet **Crash reports (M5)** (coordinator, 2026-11-23) | The build triple is `target`, the callsite target `event_target` (absent for panics, as is `module`); the leading run of reporter and runtime frames is dropped with no marker, then up to 48 frames; dedup per kind and location per clock hour, the first occurrence spooled at once, repeats and capped occurrences spooled as one report after the hour (lost at exit); panics share the dedup and cap; `message` is left out of `fields`; 1xx, 3xx, **408 and 429** are kept like a 5xx and stop the pass; caps of 512 / 256 bytes, 32 field names, 63 KiB per body; `crash_reports_on` is the switchboard's decision; dependencies' ERROR events are reported; the instance id is added at send time |
| Design § Details settled while planning (earlier bullets) | `uptime` is the time since the process started, bucketed as the heartbeat's; `http` endpoints only to a loopback IP |

The user's answers this plan uses: design § Answers 5 (milestones in parallel) and the "(user)" marks of § 3 (every panic and ERROR event; code-defined facts only).

Plan-level choices within those decisions:
- **`log`-crate events are skipped.** `subscriber::init` installs the `log` bridge, so a `log::error!` of a dependency arrives as an event with target `log`, no file, no line and the fixed field names `log.target`, `log.module_path`, …: every such event would share one report (`location: unknown`, `event_target: log`) that says nothing, and the origin is only in field values, which are never read. The crash layer skips events whose metadata has no file. The docs and spec 12 say so.
- **Synchronous writes.** The hook and the layer write on their own thread; a stuck data directory (a hung network mount) delays a panicking thread until the write returns. Accepted and documented.
- **The first symbolization is heavy.** `backtrace::resolve_frame` parses the executable's symbol table on first use (and DWARF line tables in dev and test builds), then caches it: a few hundred milliseconds on the first new error location of a process, tens of MB kept. Under nextest every test that captures a backtrace pays it again. Only the tests of TL5.3 (`capture_frames_names_its_caller`), TL5.6 (the layer and reporter tests), TL5.7 and TL5.9 capture; the limiter, spool and sender tests build reports with `event_report(…, Vec::new(), …)` and never capture. The docs say the first report of a process may take a moment.

## Versions

Checked on 2026-11-23 with `cargo info` and the registry sources.

| Crate | Version | Features | Where | Why |
|---|---|---|---|---|
| `backtrace` | **0.3.77** (`cargo info backtrace`: latest; licence `MIT OR Apache-2.0`; rust-version 1.88, the toolchain is 1.99.1) | default (`std`) only | root `[workspace.dependencies]`; `cetra-server` `[dependencies]` | Structured frames for the report's `backtrace` (`trace`, `resolve_frame`); `std::backtrace` has no stable frame API (design § Backtraces) |

New transitive crates, all on the `deny.toml` allow-list:
- `addr2line` 0.27.1 (`Apache-2.0 OR MIT`);
- `gimli` 0.34.0 (`MIT OR Apache-2.0`);
- `object` 0.40.0 (`Apache-2.0 OR MIT`). It is a **second version**: `object` 0.39.1 is already in the lock through `ar_archive_writer`. `deny.toml` has `multiple-versions = "warn"` and `object` is not in the `deny` family list, so `just deny` warns and passes;
- `rustc-demangle` 0.1.28 (`MIT/Apache-2.0`, which cargo-deny reads as `MIT OR Apache-2.0`).

`miniz_oxide` 0.9.1, `adler2`, `cfg-if`, `libc` and `memchr` are already in the lock. `deny.toml` is not edited. The implementer of TL5.1 re-runs `cargo info backtrace` and reports the version and licence lines, and the exact list of new `[[package]]` entries in `Cargo.lock`. The coordinator's final `just deny` confirms the licences.

## Phase graph

```
TL5.1 dependency ──► TL5.3 report ─► TL5.4 limiter ─► TL5.5 spool ─► TL5.6 reporter+layer ─┐
TL5.2 switch, config, heartbeat feature ─────────────────────────────────────────────────────┼─► TL5.7 wiring ─► TL5.8 sender+startup ─► TL5.9 leak tests
TL5.10 docs (any time; its links resolve once TL5.2 is in)                                  ┘
```

TL5.2 may run in parallel with TL5.1 and TL5.3–TL5.6 (no shared file). TL5.3–TL5.6 and TL5.8–TL5.9 share `crash/mod.rs` and run in order. TL5.10 touches no Rust.

| Phase | Files | Shared with other milestones (coordinator serializes) |
|---|---|---|
| TL5.1 | `Cargo.toml`, `crates/server/Cargo.toml`, `Cargo.lock` | M2 (new crates), M6 maybe |
| TL5.2 | `config.rs`, `observability/switchboard.rs`, `observability/heartbeat.rs`, `website/…/self-hosting/configuration-reference.md` | all four: M3 (switchboard), M6 (`heartbeat.rs`, `config.rs`, the reference) |
| TL5.3 | `observability/mod.rs`, `observability/crash/mod.rs` (new), `observability/crash/report.rs` (new), `observability/facts.rs`, `observability/buckets.rs` | `mod.rs` (M2, M3, M6 add `mod` lines), `facts.rs` (M3), `buckets.rs` (M6) |
| TL5.4 | `observability/crash/limiter.rs` (new), `observability/crash/mod.rs` | none |
| TL5.5 | `observability/crash/spool.rs` (new), `observability/crash/mod.rs` | none |
| TL5.6 | `observability/crash/mod.rs`, `observability/crash/layer.rs` (new) | none |
| TL5.7 | `observability/panic.rs`, `observability/subscriber.rs` | `subscriber.rs` (M2, M3 add layers) |
| TL5.8 | `observability/crash/sender.rs` (new), `observability/crash/mod.rs`, `observability/test_receiver.rs` (new, test-only), `observability/mod.rs` (one line), `serve.rs`, `backup.rs` (one test) | `observability/mod.rs`, `serve.rs` (M2, M3, M6), `backup.rs` (M2's `backup` span) |
| TL5.9 | `observability/crash/leak_tests.rs` (new), `observability/crash/mod.rs` (one line) | none |
| TL5.10 | `website/…/self-hosting/telemetry.md`, `website/…/self-hosting/backup.md`, `CHANGELOG.md`, `docs/spec/12-operations.md`, the Q notes file (outside the repo) | `telemetry.md`, `CHANGELOG.md`, spec 12 (M2, M3, M6) |

(`observability/…` is `crates/server/src/observability/…`; `config.rs`, `serve.rs`, `backup.rs` are in `crates/server/src/`; `website/…` is `website/src/content/docs/docs`.)

## Conventions for every phase

- The implementer is Haiku. Follow the plan exactly. If a step is unclear, contradicts the code, or cannot be done as written, stop and report the question; do not improvise.
- Edit only the files in the phase's **Edit** list. Report any change needed elsewhere.
- **Do not run** `cargo build`, `cargo test`, `cargo check`, `cargo clippy`, `cargo nextest` or any `just` recipe: the machine is loaded, and the coordinator runs the checks once at the end of the milestone. Allowed: `git diff`, `grep`, reading sources under `~/.cargo/registry/src`, `cargo info`, `rustfmt --edition 2024 <your .rs files>` (run it on every Rust file you changed before committing), and, where a phase says so, `cargo metadata --format-version 1 --offline > /dev/null`.
- Write every listed test, even though you do not run it. Make it compile by reading the APIs; never guess a signature: look it up in the code or the registry sources of **Read first**.
- **Done when** for every phase: the listed files changed as specified, nothing else changed (`git status`), and one commit with the phase's subject exists.
- Commit only your phase's files: `git add <files> && git commit -m "<subject>" -- <files>` (the `git add` is needed for new files). Other agents commit in the same checkout: if `.git/index.lock` exists, wait a few seconds and retry. Never `git stash`, never `git add -A`.
- Rust rules: no `unwrap()`/`expect()` outside tests unless a comment states the invariant; no `unsafe` (tests never call `std::env::set_var`); no panic on disk input; `thiserror` errors; new items `pub(crate)` with a doc comment; the workspace lints `missing_debug_implementations`, `unreachable_pub` and `unused_qualifications` are on.
- **The spool path and the report path never call `tracing`** (no `tracing::…!` in `report.rs`, `limiter.rs`, `spool.rs`, `layer.rs`, nor in `Reporter`'s methods): an event there would re-enter the crash layer, and a panic hook must not log. Only `start` and the sender loop log, at `debug` (plus the two notice lines at `info`).
- Doc comments spell the noun "licence" (British), never the identifier spelling; in every new file all non-test items come before the `#[cfg(test)]` items at the end (TL5.8's licence test cuts each file at its first `#[cfg(test)]`).
- `mod` lines stay in alphabetical order (rustfmt `reorder_modules`).
- **Shared files are edited additively** (§ Prerequisites): re-read the file right before the edit, keep what M2, M3 or M6 added, and add M5's field, line or row next to theirs. No step of this plan replaces a whole function body or struct of M1 or M4 code; where a step shows a body, it shows the lines to add.
- **Test names and module paths never contain a string a leak test forbids** (`sk-cetra-`, `example.com`, `prompt text`, and in TL5.6's layer test also `planted`): the first kept backtrace frame of an event or panic report is often the test function itself.
- A test that needs an HTTP receiver uses `crate::observability::test_receiver::Receiver` (TL5.8); never write another one.
- The milestone's final check, run by the coordinator after all phases: `just fmt`, then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`, then `just deny`.

---

### TL5.1 — The `backtrace` dependency

**Goal.** Declare `backtrace` 0.3.77 in the workspace and in `cetra-server`.

**Edit:**
- `Cargo.toml` (root)
- `crates/server/Cargo.toml`
- `Cargo.lock` (rewritten by `cargo metadata`)

**Steps.**
1. Run `cargo info backtrace` (read-only) and note the version and licence lines. If the latest version is not 0.3.77 or the licence is not `MIT OR Apache-2.0`, stop and report.
2. In the root `Cargo.toml` `[workspace.dependencies]`, right after the line `anyhow = "1.0"`, add:
   ```toml
   # Structured stack frames for crash reports (track TL M5, D-TL-5): `trace` and `resolve_frame`.
   # `std::backtrace` has no stable frame API. Default features (`std`) only.
   backtrace = "0.3.77"
   ```
3. In `crates/server/Cargo.toml` `[dependencies]`, add `backtrace.workspace = true` right after `axum-extra.workspace = true`.
4. Run `cargo metadata --format-version 1 --offline > /dev/null` from the worktree root (it rewrites `Cargo.lock` without compiling). `git diff Cargo.lock` must show only: new `[[package]]` entries for `backtrace` 0.3.77, `addr2line` 0.27.x, `gimli` 0.34.x, `object` 0.40.x and `rustc-demangle` 0.1.x; `cetra-server`'s dependency list gaining `backtrace`; and dependency lists that now name `object 0.39.1` with its version (the lock disambiguates two versions). If any other package changed version, stop and report. If `--offline` fails because a crate is not cached, run it without `--offline` and report that.

**Tests.** None: a declaration has no behaviour. The final `just deny` checks licences and duplicates.

**Done when.** The three files changed as above; the report lists the `cargo info` lines and every new lock entry with its version.

**Commit:** `Add the backtrace dependency for crash reports`

---

### TL5.2 — The crash-reports switch, its config key and the heartbeat feature

**Goal.** `telemetry.crash_reports` exists, the switchboard decides it, and the heartbeat's `features` carries `crash_reports_on`.

**Edit:**
- `crates/server/src/config.rs`
- `crates/server/src/observability/switchboard.rs`
- `crates/server/src/observability/heartbeat.rs`
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`

**M4 names used here** (checked against the committed code; stop if one differs): in `config.rs`, `pub struct TelemetryConfig { heartbeat, endpoint, env }` (line ~412) and its `impl Default`; in `switchboard.rs`, `Switch`, `OffReason` (with `Config`), `pub(crate) struct Decision { pub(crate) heartbeat: Switch }`, `pub(crate) struct Inputs { env, restore_mode, debug_build, heartbeat }`, `decide` (a `let heartbeat = match shared_off(inputs) { … };` then `Decision { heartbeat }`), `shared_off`, `for_config` (an `Inputs { … }` literal); in `heartbeat.rs`, `Features` (doc "Which features the instance uses. M5 adds `crash_reports_on`."; last field `self_monitoring`), `features` (last line `self_monitoring: config.self_monitoring.exports(),`), the import `use crate::observability::switchboard::{Decision, NOTICE_TARGET, Switch};`, and in its tests `golden()`, `FEATURE_KEYS: [&str; 10]`, `the_payload_matches_the_golden_json`, `features_follow_the_config` (its `none` literal), `start_when_off_logs_the_reason_and_spawns_nothing` and `start_when_on_logs_the_instance_id_and_spawns` (each with a `Decision { heartbeat: … }` literal); the configuration reference's `` ## `[telemetry]` `` section with its `telemetry.heartbeat` row.

Every edit here is additive (§ Prerequisites). M6 adds `usage` to the heartbeat and may have added fields to `Features`, the golden JSON or `FEATURE_KEYS`; M3 reads `Decision`. Keep their additions.

**Steps.**
1. Before editing, run `grep -rn "Decision {\|Inputs {\|TelemetryConfig {\|Features {" crates/server/src` (ignore `routes/saved_views.rs`, whose `Decision` is another type). Every literal of the telemetry types must be in `switchboard.rs`, `heartbeat.rs` or `config.rs`, or use `..TelemetryConfig::default()` (`test_support/mod.rs`, `ingest/fixtures_tests.rs`). If another file builds a `Decision`, `Inputs`, `Features` or full `TelemetryConfig` literal (M3 or M6 may have added one), add the M5 field to it only if that file is in this phase's Edit list; otherwise stop and report.
2. `config.rs`, `TelemetryConfig`: add after the `heartbeat` field:
   ```rust
   /// Opt-in crash reports to `<endpoint>/v1/crash` (D-TL-5). Default false.
   #[serde(default)]
   pub crash_reports: bool,
   ```
   and add `crash_reports: false,` to its `Default` impl after `heartbeat: default_heartbeat(),`. Change the struct's doc comment to say `[telemetry]` controls the heartbeat and crash reports.
3. `switchboard.rs`, additions only:
   1. In `Decision`, add after the `heartbeat` field:
      ```rust
      /// Crash reports (D-TL-5).
      pub(crate) crash_reports: Switch,
      ```
      and change its doc `What may go to us. M5 adds \`crash_reports\`.` to `What may go to us.`
   2. In `Inputs`, add after the `heartbeat` field:
      ```rust
      /// `telemetry.crash_reports`.
      pub(crate) crash_reports: bool,
      ```
   3. In `decide`: change `let heartbeat = match shared_off(inputs) {` to `let shared = shared_off(inputs);` followed by `let heartbeat = match shared {` (the arms do not change). Before the final `Decision { … }` literal, add:
      ```rust
      // Rule 4 (debug builds, CI) is heartbeat only: crash reports run only when asked for.
      let crash_reports = match shared {
          Some(reason) => Switch::Off(reason),
          None if !inputs.crash_reports => Switch::Off(OffReason::Config),
          None => Switch::On,
      };
      ```
      and add `crash_reports` to that literal (keep any other field in it).
   4. In `for_config`'s `Inputs { … }` literal, add `crash_reports: config.telemetry.crash_reports,`.
   5. In the tests: add `crash_reports: false` to every `Inputs { … }` literal (or to the helper that builds the all-clear inputs). Where an existing test compares a whole `Decision`, change it to compare `.heartbeat` only. Do not change what the existing tests assert about the heartbeat.
4. `heartbeat.rs`, additions only:
   1. In `Features`, add after `self_monitoring` (after any field M6 added, if it is last):
      ```rust
      /// Crash reports are on: the switchboard's decision, not only the config (D-TL-5).
      pub(crate) crash_reports_on: bool,
      ```
      and change the struct's doc to `Which features the instance uses.`
   2. In `features`, add after `self_monitoring: config.self_monitoring.exports(),`:
      ```rust
      crash_reports_on: for_config(config).crash_reports == Switch::On,
      ```
      and add `for_config` to the existing `use crate::observability::switchboard::{Decision, NOTICE_TARGET, Switch};`.
   3. Tests:
      - `golden()`: add `crash_reports_on: false,` after `self_monitoring: false,` in its `Features` literal.
      - `the_payload_matches_the_golden_json`: in the expected text, the line `    "self_monitoring": false` gets a trailing comma and is followed by the new line `    "crash_reports_on": false` (same indentation).
      - `FEATURE_KEYS`: add `"crash_reports_on",` after `"self_monitoring",` and raise the array length by one (11 if nobody else added a key).
      - `features_follow_the_config`: add `crash_reports_on: false,` to the `none` literal; before the final `assert_eq!`, also set `config.telemetry.crash_reports = true;` and add `crash_reports_on: true,` to its expected `Features { …, ..none }` literal.
      - `start_when_off_logs_the_reason_and_spawns_nothing`: add `crash_reports: Switch::Off(OffReason::DoNotTrack),` to its `Decision` literal. `start_when_on_logs_the_instance_id_and_spawns`: add `crash_reports: Switch::Off(OffReason::Config),` (the test module already imports `OffReason`).
5. `configuration-reference.md`, section `` ## `[telemetry]` ``:
   1. Replace the section's first sentence, `What cetra sends to us: an anonymous heartbeat once a day.`, with `What cetra sends to us: an anonymous heartbeat once a day, and crash reports if you turn them on.`
   2. Add this row right after the `telemetry.heartbeat` row:
      ```markdown
      | `telemetry.crash_reports` | | boolean | `false` | Send crash reports: each panic and error event as a code location and function names, never a message or a value. `CETRA_TELEMETRY=off`, `DO_NOT_TRACK=1` and restore mode turn it off; CI environments and debug builds do not. See [Crash reports](/docs/self-hosting/telemetry/#crash-reports). |
      ```
      Keep the Environment column empty, like the other `[telemetry]` rows.

**Tests.**
- `config.rs`: in `telemetry_defaults`, also assert `crash_reports == false`. New: `telemetry_crash_reports_from_toml` (`"[telemetry]\ncrash_reports = true\n"` → true); `telemetry_crash_reports_from_env` (`[("CETRA_TELEMETRY__CRASH_REPORTS", "true")]` → true); `the_configuration_reference_matches_the_loader` (existing) passes with the new row.
- `switchboard.rs` (all-clear inputs, then change fields):
  - `crash_reports_are_off_by_default` (`crash_reports: false` → `Off(Config)`);
  - `crash_reports_on_when_asked` (`crash_reports: true` → `On`);
  - `crash_reports_follow_rules_one_to_three` (`crash_reports: true` with `forced_off` → `Off(CetraTelemetry)`; with `do_not_track` → `Off(DoNotTrack)`; with `restore_mode` → `Off(RestoreMode)`; rule order as for the heartbeat: `forced_off` + `restore_mode` → `CetraTelemetry`);
  - `crash_reports_ignore_debug_builds_and_ci` (`crash_reports: true`, `debug_build: true`, `env.ci: true` → `crash_reports == On` while `heartbeat == Off(DebugBuild)`);
  - `the_two_switches_decide_independently` (`heartbeat: false, crash_reports: true` → `Off(Config)` and `On`; `heartbeat: true, crash_reports: false` → `On` and `Off(Config)`);
  - `for_config_reads_crash_reports` (`crate::test_support::config(dir)` with `telemetry.crash_reports = true` → `On` in a test build; with `restore_mode = true` too → `Off(RestoreMode)`).
- `heartbeat.rs`: the updated golden, key-set and features tests above.

**Commit:** `Add the crash-reports switch and its heartbeat feature`

---

### TL5.3 — The crash report type, the path rule and the frame cleanup

**Goal.** `CrashReport` as a fixed serde type, built from a panic location or an event callsite, with the path rule, the backtrace capture and cleanup, and the size caps, all pure and tested.

**Edit:**
- `crates/server/src/observability/mod.rs` (one `mod` line)
- `crates/server/src/observability/crash/mod.rs` (new)
- `crates/server/src/observability/crash/report.rs` (new)
- `crates/server/src/observability/facts.rs` (two derives)
- `crates/server/src/observability/buckets.rs` (one derive)

**M4 names used here** (committed): `facts::{VERSION, TARGET, profile, Profile, Deployment}` (`Profile` and `Deployment` derive `Debug, Clone, Copy, PartialEq, Eq, Serialize`, facts.rs lines ~31 and ~50), `buckets::{UptimeBucket, uptime_bucket}` (`UptimeBucket` derives the same, buckets.rs line ~79).

**Steps.**
1. `observability/mod.rs`: add `pub(crate) mod crash;` in alphabetical order.
2. `facts.rs`: add `Deserialize` to the derive lists of `Profile` and `Deployment`, and to the `use serde::…` import. `buckets.rs`: add `Deserialize` to `UptimeBucket`'s derive list and to the import. Additive only: keep any derive or item M3 or M6 added. The spool is read back into `CrashReport` (TL5.8).
3. Create `crash/mod.rs` with exactly:
   ```rust
   //! Opt-in crash reports (track TL § 3, D-TL-5, D-TL-12). With `telemetry.crash_reports` on and
   //! the switchboard agreeing, every panic and every ERROR `tracing` event becomes a report of
   //! code-defined facts, spooled to `<data_dir>/telemetry/spool/` at once and sent to
   //! `<telemetry.endpoint>/v1/crash` by a background task. Nothing in a report comes from a
   //! message or a field value.

   pub(crate) mod report;
   ```
4. Create `crash/report.rs`. Module doc: `//! The crash report (D-TL-5): a fixed type whose values are constants, buckets, code locations, callsite metadata and function names. Its constructors take no message and no field value, so neither can reach a report.` Contents (in full; give every item a doc comment as shown):
   ```rust
   use std::panic::Location;
   use std::time::Duration;

   use serde::{Deserialize, Serialize};
   use uuid::Uuid;

   use crate::observability::buckets::{UptimeBucket, uptime_bucket};
   use crate::observability::facts::{self, Deployment, Profile};

   /// The report's schema version (TL-Q1: the relay refuses other versions).
   pub(crate) const CRASH_SCHEMA: u32 = 1;
   /// At most this many function names in `backtrace`.
   pub(crate) const MAX_FRAMES: usize = 48;
   /// The stack walk stops after this many frames, before cleanup.
   const MAX_RAW_FRAMES: usize = 256;
   /// A function name is cut to this many bytes.
   pub(crate) const MAX_NAME_BYTES: usize = 512;
   /// `location` is cut to this many bytes.
   pub(crate) const MAX_LOCATION_BYTES: usize = 512;
   /// `event_target`, `module` and each field name are cut to this many bytes.
   pub(crate) const MAX_TARGET_BYTES: usize = 256;
   /// At most this many field names (tracing's own limit is 32).
   pub(crate) const MAX_FIELDS: usize = 32;
   /// A spooled body is at most this large; the sender adds `instance_id` and stays under the
   /// relay's 64 KiB (TL-Q1).
   pub(crate) const MAX_SPOOLED_BYTES: usize = 63 * 1024;
   const NANOS_PER_HOUR: i64 = 3_600_000_000_000;

   /// Frames of the reporter itself, dropped from the top of a backtrace.
   const REPORTER_PREFIXES: [&str; 6] = [
       "cetra_server::observability::crash::report::capture_frames",
       "cetra_server::observability::crash::Reporter::",
       "cetra_server::observability::crash::report_panic",
       "cetra_server::observability::crash::layer::CrashLayer",
       "cetra_server::observability::panic::report",
       "cetra_server::observability::panic::install",
   ];

   /// Crates whose frames above the first kept frame are dropped (design § 3: runtime frames
   /// collapsed). `backtrace` and the `tracing` crates are the reporter's own path.
   const RUNTIME_ROOTS: [&str; 8] = [
       "std",
       "core",
       "alloc",
       "tokio",
       "backtrace",
       "tracing",
       "tracing_core",
       "tracing_subscriber",
   ];

   /// What a report is about.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub(crate) enum ReportKind {
       /// A panic, from the panic hook.
       Panic,
       /// An ERROR `tracing` event, from the crash layer.
       ErrorEvent,
   }

   /// The crash report body (D-TL-5). Field order is the JSON key order. The spool holds it
   /// without `instance_id`; the sender adds it.
   #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(deny_unknown_fields)]
   pub(crate) struct CrashReport {
       /// [`CRASH_SCHEMA`].
       pub(crate) schema: u32,
       /// `instance_settings['telemetry.instance_id']`, added by the sender.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub(crate) instance_id: Option<Uuid>,
       /// [`facts::VERSION`].
       pub(crate) version: String,
       /// [`facts::TARGET`]: the build's target triple.
       pub(crate) target: String,
       /// [`facts::profile`].
       pub(crate) profile: Profile,
       /// [`facts::deployment`].
       pub(crate) deployment: Deployment,
       /// Time since the process started, at the first occurrence.
       pub(crate) uptime: UptimeBucket,
       /// Panic or error event.
       pub(crate) kind: ReportKind,
       /// `path:line:column` of a panic or `path:line` of an event, path cut by [`report_path`].
       pub(crate) location: String,
       /// The event's `tracing` target; absent for a panic.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub(crate) event_target: Option<String>,
       /// The event's module path; absent for a panic or when unknown.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub(crate) module: Option<String>,
       /// The callsite's field names, never values; `message` left out. Empty for a panic.
       pub(crate) fields: Vec<String>,
       /// Function names, innermost first, cleaned by [`clean_frames`].
       pub(crate) backtrace: Vec<String>,
       /// Occurrences this report stands for.
       pub(crate) count: u64,
       /// Unix hour of the first of them.
       pub(crate) first_hour: u64,
       /// Unix hour of the last of them.
       pub(crate) last_hour: u64,
   }

   /// An event callsite, as the crash layer reads it from `tracing` metadata.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct EventSite<'a> {
       /// `Metadata::file`.
       pub(crate) file: Option<&'a str>,
       /// `Metadata::line`.
       pub(crate) line: Option<u32>,
       /// `Metadata::target`.
       pub(crate) target: &'a str,
       /// `Metadata::module_path`.
       pub(crate) module_path: Option<&'a str>,
       /// The names of `Metadata::fields`.
       pub(crate) fields: Vec<&'static str>,
   }

   impl<'a> EventSite<'a> {
       /// The callsite of `metadata`. Reads names only: an event's values are never visited.
       pub(crate) fn from_metadata(metadata: &'a tracing::Metadata<'a>) -> Self {
           Self {
               file: metadata.file(),
               line: metadata.line(),
               target: metadata.target(),
               module_path: metadata.module_path(),
               fields: metadata.fields().iter().map(|field| field.name()).collect(),
           }
       }
   }
   ```
5. The functions, in full:
   ```rust
   /// The Unix hour of `now_ns`; 0 before the epoch.
   pub(crate) fn unix_hour(now_ns: i64) -> u64 {
       u64::try_from(now_ns.max(0) / NANOS_PER_HOUR).unwrap_or(0)
   }

   /// The design's path rule: a workspace path (`crates/…`) is kept; a path under
   /// `/.cargo/registry/src/<index>/` or `/.cargo/git/checkouts/` keeps what follows; anything
   /// else, or any path with a `..` component, keeps its file name only. So no home directory
   /// reaches a report. Fails closed: an unknown shape gives the file name, an empty one `unknown`.
   pub(crate) fn report_path(file: &str) -> String {
       const REGISTRY: &str = "/.cargo/registry/src/";
       const GIT: &str = "/.cargo/git/checkouts/";
       let file = file.replace('\\', "/");
       let has_parent = file.split('/').any(|part| part == "..");
       if !has_parent {
           if file.starts_with("crates/") {
               return truncate_utf8(&file, MAX_LOCATION_BYTES);
           }
           if let Some(index) = file.find(REGISTRY) {
               // What follows starts with the index directory (`index.crates.io-…`): dropped.
               let rest = &file[index + REGISTRY.len()..];
               if let Some((_, kept)) = rest.split_once('/')
                   && !kept.is_empty()
               {
                   return truncate_utf8(kept, MAX_LOCATION_BYTES);
               }
           }
           if let Some(index) = file.find(GIT) {
               let kept = &file[index + GIT.len()..];
               if !kept.is_empty() {
                   return truncate_utf8(kept, MAX_LOCATION_BYTES);
               }
           }
       }
       match file.rsplit('/').next() {
           Some(name) if !name.is_empty() => truncate_utf8(name, MAX_LOCATION_BYTES),
           _ => "unknown".to_owned(),
       }
   }

   /// `path:line:column` of a panic, or `unknown`.
   pub(crate) fn location_of_panic(location: Option<&Location<'_>>) -> String {
       location.map_or_else(
           || "unknown".to_owned(),
           |location| {
               let text = format!(
                   "{}:{}:{}",
                   report_path(location.file()),
                   location.line(),
                   location.column()
               );
               truncate_utf8(&text, MAX_LOCATION_BYTES)
           },
       )
   }

   /// `path:line` of an event, `path` without a line, or `unknown` without a file.
   pub(crate) fn location_of_event(file: Option<&str>, line: Option<u32>) -> String {
       let text = match (file, line) {
           (Some(file), Some(line)) => format!("{}:{line}", report_path(file)),
           (Some(file), None) => report_path(file),
           (None, _) => return "unknown".to_owned(),
       };
       truncate_utf8(&text, MAX_LOCATION_BYTES)
   }

   /// `text` cut to at most `max` bytes, on a character boundary.
   pub(crate) fn truncate_utf8(text: &str, max: usize) -> String {
       if text.len() <= max {
           return text.to_owned();
       }
       let mut end = max;
       // Index 0 is always a boundary, so the loop ends.
       while !text.is_char_boundary(end) {
           end -= 1;
       }
       text[..end].to_owned()
   }

   /// `name` without a legacy-mangling hash suffix (`::h` and 16 hex digits).
   pub(crate) fn strip_hash(name: &str) -> &str {
       const SUFFIX_LEN: usize = 19;
       let Some(start) = name.len().checked_sub(SUFFIX_LEN) else {
           return name;
       };
       match name.get(start..) {
           Some(tail)
               if tail.starts_with("::h")
                   && tail.get(3..).is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit())) =>
           {
               name.get(..start).unwrap_or(name)
           }
           _ => name,
       }
   }

   /// The first path segment of a function name, ignoring leading `<` (`<alloc::boxed::Box<F>
   /// as …>::call` is `alloc`); empty for a name without `::` (a C symbol).
   fn root(name: &str) -> &str {
       let trimmed = name.trim_start_matches('<');
       trimmed.find("::").map_or("", |end| &trimmed[..end])
   }

   /// A frame of a runtime crate or a C symbol.
   fn is_runtime(name: &str) -> bool {
       let root = root(name);
       root.is_empty() || RUNTIME_ROOTS.contains(&root)
   }

   /// A frame of the reporter itself.
   fn is_reporter(name: &str) -> bool {
       let trimmed = name.trim_start_matches('<');
       REPORTER_PREFIXES.iter().any(|prefix| trimmed.starts_with(prefix))
   }

   /// Raw demangled names, innermost first, to the report's `backtrace`: hash suffix removed,
   /// each cut to [`MAX_NAME_BYTES`], empty names skipped, the leading run of reporter and
   /// runtime frames dropped, then at most [`MAX_FRAMES`].
   pub(crate) fn clean_frames(raw: Vec<String>) -> Vec<String> {
       raw.into_iter()
           .map(|name| truncate_utf8(strip_hash(&name), MAX_NAME_BYTES))
           .filter(|name| !name.is_empty())
           .skip_while(|name| is_runtime(name) || is_reporter(name))
           .take(MAX_FRAMES)
           .collect()
   }

   /// The current thread's stack as cleaned function names. Symbols come from the binary's
   /// symbol table (release archives keep it, D-N-16); a frame without a name is skipped.
   pub(crate) fn capture_frames() -> Vec<String> {
       let mut names = Vec::new();
       backtrace::trace(|frame| {
           // The global lock `trace` holds is re-entrant on this thread (backtrace's `mod lock`).
           backtrace::resolve_frame(frame, |symbol| {
               if let Some(name) = symbol.name() {
                   // `{:#}` drops the hash suffix of legacy mangling.
                   names.push(format!("{name:#}"));
               }
           });
           names.len() < MAX_RAW_FRAMES
       });
       clean_frames(names)
   }

   /// A panic report: the location, the frames and the build facts. There is no message
   /// parameter: Rust's panic messages embed data, so the message is dropped whole (D-TL-5).
   pub(crate) fn panic_report(
       location: Option<&Location<'_>>,
       frames: Vec<String>,
       deployment: Deployment,
       uptime: Duration,
   ) -> CrashReport {
       CrashReport {
           schema: CRASH_SCHEMA,
           instance_id: None,
           version: facts::VERSION.to_owned(),
           target: facts::TARGET.to_owned(),
           profile: facts::profile(),
           deployment,
           uptime: uptime_bucket(uptime),
           kind: ReportKind::Panic,
           location: location_of_panic(location),
           event_target: None,
           module: None,
           fields: Vec::new(),
           backtrace: frames,
           count: 1,
           first_hour: 0,
           last_hour: 0,
       }
   }

   /// An error-event report: the callsite's location, target, module and field names.
   pub(crate) fn event_report(
       site: &EventSite<'_>,
       frames: Vec<String>,
       deployment: Deployment,
       uptime: Duration,
   ) -> CrashReport {
       CrashReport {
           schema: CRASH_SCHEMA,
           instance_id: None,
           version: facts::VERSION.to_owned(),
           target: facts::TARGET.to_owned(),
           profile: facts::profile(),
           deployment,
           uptime: uptime_bucket(uptime),
           kind: ReportKind::ErrorEvent,
           location: location_of_event(site.file, site.line),
           event_target: Some(truncate_utf8(site.target, MAX_TARGET_BYTES)),
           module: site
               .module_path
               .map(|module| truncate_utf8(module, MAX_TARGET_BYTES)),
           fields: site
               .fields
               .iter()
               .filter(|name| **name != "message")
               .take(MAX_FIELDS)
               .map(|name| truncate_utf8(name, MAX_TARGET_BYTES))
               .collect(),
           backtrace: frames,
           count: 1,
           first_hour: 0,
           last_hour: 0,
       }
   }

   /// The spooled body of `report`: its JSON if at most [`MAX_SPOOLED_BYTES`], else the JSON
   /// without its backtrace if that fits, else `None`.
   pub(crate) fn encode(report: &CrashReport) -> Option<Vec<u8>> {
       let body = serde_json::to_vec(report).ok()?;
       if body.len() <= MAX_SPOOLED_BYTES {
           return Some(body);
       }
       let mut smaller = report.clone();
       smaller.backtrace.clear();
       let body = serde_json::to_vec(&smaller).ok()?;
       (body.len() <= MAX_SPOOLED_BYTES).then_some(body)
   }
   ```
   (`count`, `first_hour` and `last_hour` of a new report are placeholders the limiter overwrites, TL5.4.) If rustfmt or the edition refuses the `if let … && …` let-chain in `report_path`, rewrite it as a nested `if let` with the same behaviour.

**Tests** (in `report.rs`'s `#[cfg(test)] mod tests`; `const HOUR0: u64 = 490_896;` is 2026-01-01T00:00Z):
- `the_event_report_matches_the_golden_json`: a `CrashReport { schema: 1, instance_id: Some(Uuid::parse_str("8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d").unwrap()), version: "0.1.0".into(), target: "x86_64-unknown-linux-musl".into(), profile: Profile::Release, deployment: Deployment::Docker, uptime: UptimeBucket::HourToDay, kind: ReportKind::ErrorEvent, location: "crates/storage/src/wal.rs:412".into(), event_target: Some("cetra_storage::wal".into()), module: Some("cetra_storage::wal".into()), fields: vec!["error".into(), "project_id".into()], backtrace: vec!["cetra_storage::wal::Wal::sync".into(), "cetra_storage::flush::run".into()], count: 3, first_hour: HOUR0, last_hour: HOUR0 + 1 }`; `serde_json::to_string_pretty` equals exactly:
  ```json
  {
    "schema": 1,
    "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
    "version": "0.1.0",
    "target": "x86_64-unknown-linux-musl",
    "profile": "release",
    "deployment": "docker",
    "uptime": "1h-1d",
    "kind": "error_event",
    "location": "crates/storage/src/wal.rs:412",
    "event_target": "cetra_storage::wal",
    "module": "cetra_storage::wal",
    "fields": [
      "error",
      "project_id"
    ],
    "backtrace": [
      "cetra_storage::wal::Wal::sync",
      "cetra_storage::flush::run"
    ],
    "count": 3,
    "first_hour": 490896,
    "last_hour": 490897
  }
  ```
- `the_spooled_panic_report_matches_the_golden_json`: kind `Panic`, `instance_id: None`, `event_target: None`, `module: None`, `fields: vec![]`, location `crates/storage/src/wal.rs:412:9`, backtrace `["cetra_storage::wal::Wal::sync"]`, uptime `UnderHour`, deployment `Binary`, count 1, both hours `HOUR0`; expected text in full (no `instance_id`, `event_target` or `module` key; `"fields": []`).
- `the_key_set_is_fixed`: the top-level keys of the event golden are exactly the 16 above in that order; no key contains `message`, `value`, `licen` or `host`.
- `a_report_round_trips_through_json`; `an_unknown_key_is_refused` (the panic golden text with `"message": "x"` inserted fails to deserialize); `a_missing_instance_id_reads_as_none`.
- `report_path_rules`, a table (input → output); also assert no output contains `planted-user`:
  - `crates/storage/src/wal.rs` → itself;
  - `/home/planted-user/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/arrow-55.0.0/src/array.rs` → `arrow-55.0.0/src/array.rs`;
  - `/home/planted-user/.cargo/git/checkouts/foo-abc123/def456/src/lib.rs` → `foo-abc123/def456/src/lib.rs`;
  - `/home/planted-user/src/cetra/crates/storage/src/wal.rs` → `wal.rs`;
  - `/usr/local/cargo/registry/src/index.crates.io-x/tokio-1.54.0/src/lib.rs` → `lib.rs` (only `/.cargo/` is recognised);
  - `/rustc/90b35a6239c3d8bdabc530a6a0816f7ff89a0aaf/library/core/src/option.rs` → `option.rs`;
  - `crates/../../home/planted-user/secret.rs` → `secret.rs`;
  - `/home/planted-user/.cargo/registry/src/index/../../../etc/passwd` → `passwd`;
  - `C:\Users\planted-user\.cargo\registry\src\index.crates.io-x\tokio-1.54.0\src\lib.rs` → `tokio-1.54.0/src/lib.rs`;
  - `/home/planted-user/.cargo/registry/src/index.crates.io-x` → `index.crates.io-x`;
  - `src/main.rs` → `main.rs`; `` (empty) → `unknown`; `/home/planted-user/` → `unknown`;
  - `crates/` followed by 600 × `a` → 512 bytes.
- `location_of_panic_rules` (`Location::caller()` in the test → starts with `crates/server/src/observability/crash/report.rs:` and has three `:`-separated parts; `None` → `unknown`). `location_of_event_rules` (`(Some("crates/x.rs"), Some(7))` → `crates/x.rs:7`; `(Some("crates/x.rs"), None)` → `crates/x.rs`; `(None, Some(7))` → `unknown`).
- `truncate_utf8_cuts_on_a_boundary` (`"aé"` × 300 at 11 bytes → valid, ≤ 11 bytes; a short text unchanged; max 0 → empty).
- `strip_hash_rules` (`a::b::h0123456789abcdef` → `a::b`; `a::b::h0123` unchanged; `a::b::hXYZ0123456789abcd` unchanged (not hex); `a::{{closure}}` unchanged; exactly `::h0123456789abcdef` → empty; `ab` unchanged).
- `a_panic_stack_starts_at_the_first_cetra_frame`: `clean_frames` of, in order: `backtrace::backtrace::libunwind::trace::h0123456789abcdef`, `_Unwind_Backtrace`, `cetra_server::observability::crash::report::capture_frames`, `cetra_server::observability::crash::Reporter::report_panic`, `cetra_server::observability::crash::report_panic`, `cetra_server::observability::panic::report`, `cetra_server::observability::panic::install::{{closure}}`, `<alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call`, `std::panicking::rust_panic_with_hook`, `rust_begin_unwind`, `core::panicking::panic_fmt`, `core::result::unwrap_failed`, `cetra_storage::wal::Wal::sync::h0123456789abcdef`, `cetra_storage::flush::run::{{closure}}`, `<core::pin::Pin<P> as core::future::future::Future>::poll`, `tokio::runtime::task::harness::poll_future`, `cetra_server::serve::bind`, `main` → exactly `[cetra_storage::wal::Wal::sync, cetra_storage::flush::run::{{closure}}, <core::pin::Pin<P> as core::future::future::Future>::poll, tokio::runtime::task::harness::poll_future, cetra_server::serve::bind, main]`.
- `an_event_stack_drops_the_tracing_frames`: `backtrace::trace`, `cetra_server::observability::crash::report::capture_frames`, `cetra_server::observability::crash::Reporter::report_event`, `<cetra_server::observability::crash::layer::CrashLayer as tracing_subscriber::layer::Layer<S>>::on_event`, `<alloc::vec::Vec<L> as tracing_subscriber::layer::Layer<S>>::on_event`, `<tracing_subscriber::layer::layered::Layered<L,S> as tracing_core::subscriber::Subscriber>::event`, `tracing_core::event::Event::dispatch`, `cetra_storage::wal::Wal::sync` → `[cetra_storage::wal::Wal::sync]`.
- `a_dependency_frame_above_cetra_is_kept` (`core::panicking::panic_bounds_check`, `arrow_array::array::PrimitiveArray<T>::value`, `cetra_storage::query::scan` → the last two).
- `at_most_48_frames` (100 names `cetra_x::f{i}` → 48, first `cetra_x::f0`); `long_names_are_cut` (`cetra_x::` + `é` × 400 → ≤ 512 bytes, valid UTF-8); `empty_names_are_skipped`; `an_all_runtime_stack_is_empty`.
- `capture_frames_names_its_caller`: a `#[inline(never)] fn marker_for_capture_test() -> Vec<String> { capture_frames() }` in the test module; the result has a frame containing `marker_for_capture_test`; no frame starts with `backtrace::` before it; no frame ends with `::h` and 16 hex digits.
- `panic_report_has_only_the_location_and_frames` (`panic_report(Some(Location::caller()), vec!["cetra_x::f".into()], Deployment::Binary, Duration::from_secs(10))`: kind `Panic`, location from this file, `fields` empty, `event_target` and `module` `None`, `uptime == UnderHour`, `version == facts::VERSION`, `target == facts::TARGET`, `count == 1`).
- `event_report_reads_names_only` (an `EventSite { file: Some("crates/x.rs"), line: Some(3), target: "cetra_x", module_path: None, fields: vec!["message", "error", "project_id"] }` → `fields == ["error", "project_id"]`, `module == None`, `event_target == Some("cetra_x")`); `event_report_caps_fields` (40 distinct `&'static str` names from a `const` array → 32).
- `encode_keeps_a_normal_report` (decodes back equal); `encode_drops_the_backtrace_when_too_large` (48 names of 512 × `\u{1}`, which JSON escapes to 6 bytes each → `Some`, decoded backtrace empty, length ≤ `MAX_SPOOLED_BYTES`); `encode_refuses_what_never_fits` (300 field names of 256 bytes → `None`).
- `unix_hour_rules` (0 → 0; −1 → 0; `1_767_225_600_000_000_000` → `490_896`; `i64::MAX` does not panic).

**Commit:** `Add the crash report type`

---

### TL5.4 — Dedup and the hourly cap

**Goal.** A pure limiter that decides, per occurrence and per minute tick, which reports to spool: one per location per clock hour, at most 20 an hour, the rest counted.

**Edit:**
- `crates/server/src/observability/crash/limiter.rs` (new)
- `crates/server/src/observability/crash/mod.rs` (one `mod` line)

**Steps.**
1. `crash/mod.rs`: add `pub(crate) mod limiter;`.
2. Create `limiter.rs` with exactly (design bullet "Crash reports (M5)", dedup):
   ```rust
   //! Dedup and caps for crash reports (design § 3): the first occurrence of a location in a
   //! clock hour is spooled at once; later ones are counted and spooled as one report when the
   //! hour is over; at most [`MAX_REPORTS_PER_HOUR`] reports an hour. Hours are passed in, so
   //! tests drive the clock. Never logs.

   use std::collections::HashMap;

   use super::report::{CrashReport, ReportKind};

   /// At most this many reports are spooled in one clock hour, all locations together.
   pub(crate) const MAX_REPORTS_PER_HOUR: u32 = 20;
   /// At most this many locations are tracked. Real ones are a few hundred; each holds a
   /// template of up to 48 × 512 bytes of names, so this bounds the memory to a few MB.
   pub(crate) const MAX_TRACKED: usize = 512;

   /// What occurrences are grouped by.
   #[derive(Debug, Clone, PartialEq, Eq, Hash)]
   pub(crate) struct ReportKey {
       /// Panic or error event.
       pub(crate) kind: ReportKind,
       /// The report's `location`.
       pub(crate) location: String,
   }

   /// One location's state.
   #[derive(Debug)]
   struct Entry {
       /// The report built at the first occurrence; counts and hours are filled per spool.
       template: CrashReport,
       /// The hour a report of this location was last spooled.
       last_spooled_hour: Option<u64>,
       /// Occurrences not yet in a spooled report.
       pending: u64,
       /// Hour of the first pending occurrence.
       pending_first_hour: u64,
       /// Hour of the last pending occurrence.
       pending_last_hour: u64,
   }

   impl Entry {
       /// Pending occurrences may be spooled now.
       fn is_due(&self, hour: u64) -> bool {
           self.pending > 0 && self.last_spooled_hour != Some(hour)
       }

       /// The report of the pending occurrences; resets them.
       fn take(&mut self, hour: u64) -> CrashReport {
           let mut report = self.template.clone();
           report.count = self.pending;
           report.first_hour = self.pending_first_hour;
           report.last_hour = self.pending_last_hour;
           self.pending = 0;
           self.last_spooled_hour = Some(hour);
           report
       }
   }

   /// The limiter. One per [`super::Reporter`], behind its mutex.
   #[derive(Debug, Default)]
   pub(crate) struct Limiter {
       /// The clock hour `spooled_this_hour` counts.
       hour: u64,
       /// Reports spooled in `hour`.
       spooled_this_hour: u32,
       /// Tracked locations.
       entries: HashMap<ReportKey, Entry>,
   }

   impl Limiter {
       /// Whether `key` is tracked, so the caller can skip building a template (and a backtrace).
       pub(crate) fn knows(&self, key: &ReportKey) -> bool {
           self.entries.contains_key(key)
       }

       /// Counts one occurrence of `key` at `hour`, and returns the report to spool now, if any.
       /// `template` is the report built for an unknown key. An unknown key without one is
       /// dropped: that happens only when [`Self::flush`] forgot the key between the caller's
       /// [`Self::knows`] and this call, which is rare and loses one occurrence. A new key
       /// beyond [`MAX_TRACKED`] is dropped too.
       pub(crate) fn record(
           &mut self,
           key: &ReportKey,
           template: Option<CrashReport>,
           hour: u64,
       ) -> Option<CrashReport> {
           self.roll(hour);
           if !self.entries.contains_key(key) {
               let template = template?;
               if self.entries.len() >= MAX_TRACKED {
                   self.forget_quiet(hour);
                   if self.entries.len() >= MAX_TRACKED {
                       return None;
                   }
               }
               self.entries.insert(
                   key.clone(),
                   Entry {
                       template,
                       last_spooled_hour: None,
                       pending: 0,
                       pending_first_hour: hour,
                       pending_last_hour: hour,
                   },
               );
           }
           let entry = self.entries.get_mut(key)?;
           if entry.pending == 0 {
               entry.pending_first_hour = hour;
           }
           entry.pending = entry.pending.saturating_add(1);
           entry.pending_last_hour = hour;
           if entry.is_due(hour) && self.spooled_this_hour < MAX_REPORTS_PER_HOUR {
               self.spooled_this_hour += 1;
               Some(entry.take(hour))
           } else {
               None
           }
       }

       /// The minute tick: the reports of pending occurrences that may go now, oldest first
       /// (by first pending hour, then location, then kind), within the cap. Then forgets
       /// locations with nothing pending and no report this hour.
       pub(crate) fn flush(&mut self, hour: u64) -> Vec<CrashReport> {
           self.roll(hour);
           let mut due: Vec<(u64, ReportKey)> = self
               .entries
               .iter()
               .filter(|(_, entry)| entry.is_due(hour))
               .map(|(key, entry)| (entry.pending_first_hour, key.clone()))
               .collect();
           due.sort_by(|a, b| (a.0, &a.1.location, a.1.kind).cmp(&(b.0, &b.1.location, b.1.kind)));
           let mut reports = Vec::new();
           for (_, key) in due {
               if self.spooled_this_hour >= MAX_REPORTS_PER_HOUR {
                   break;
               }
               if let Some(entry) = self.entries.get_mut(&key) {
                   self.spooled_this_hour += 1;
                   reports.push(entry.take(hour));
               }
           }
           self.forget_quiet(hour);
           reports
       }

       /// Starts a new cap window when the hour changed (a clock going back counts as a change).
       fn roll(&mut self, hour: u64) {
           if hour != self.hour {
               self.hour = hour;
               self.spooled_this_hour = 0;
           }
       }

       /// Drops locations with nothing pending and no report in `hour`.
       fn forget_quiet(&mut self, hour: u64) {
           self.entries
               .retain(|_, entry| entry.pending > 0 || entry.last_spooled_hour == Some(hour));
       }
   }
   ```

**Tests** (in `limiter.rs`; a helper `fn key(n: u32) -> ReportKey` with kind `ErrorEvent` and location `crates/x.rs:{n}`, and `fn template() -> CrashReport` built with `report::event_report(&site, Vec::new(), Deployment::Binary, Duration::ZERO)` on a fixed `EventSite` (no backtrace capture); `H = 490_896`; calls pass the key by reference, `record(&key(1), Some(template()), H)`):
- `the_first_occurrence_is_spooled_at_once` (`record(&key(1), Some(template()), H)` → `Some`, count 1, first = last = `H`).
- `repeats_in_the_hour_are_counted_not_spooled` (three more `record(&key(1), None, H)` → `None` each).
- `the_next_hour_flush_spools_the_repeats` (then `flush(H + 1)` → one report, count 3, first = last = `H`; a second `flush(H + 1)` → empty).
- `a_repeat_in_a_later_hour_is_spooled_at_once_with_the_pending_count` (record at `H` → `Some`; two at `H` → `None`; one at `H + 1` → `Some` with count 3, first `H`, last `H + 1`).
- `at_most_twenty_reports_an_hour` (25 distinct keys at `H` → 20 `Some`, 5 `None`; `flush(H)` → empty; `flush(H + 1)` → 5 reports, count 1, first = last = `H`, in location order).
- `the_cap_resets_each_hour` (20 keys at `H`, then a new key at `H + 1` → `Some`).
- `a_known_key_without_a_template_still_counts`; `an_unknown_key_without_a_template_is_dropped` (`record(&key(9), None, H)` → `None`, `knows(&key(9))` false).
- `flush_forgets_quiet_locations` (record at `H`, `flush(H + 1)` with nothing pending → `knows` false).
- `panics_and_events_at_one_location_are_separate` (same location, two kinds → two `Some`).
- `a_clock_going_back_is_a_new_window` (record at `H` → `Some`; record at `H - 1` → `Some`, count 1).
- `counts_saturate` (set the entry's `pending` to `u64::MAX` through `limiter.entries.get_mut`; record → no panic; next flush → count `u64::MAX`).
- `tracked_locations_are_bounded` (`MAX_TRACKED + 10` distinct keys at `H`: the first 20 are spooled and the rest stay pending, so nothing can be forgotten; `entries.len() == MAX_TRACKED` and the last 10 `record` calls return `None` with `knows` false).

**Commit:** `Add crash report dedup and hourly cap`

---

### TL5.5 — The spool

**Goal.** Reports are written atomically to `<data_dir>/telemetry/spool/`, at most 50 files, oldest dropped, with no logging and no panic, and can be listed, cleared and cleaned.

**Edit:**
- `crates/server/src/observability/crash/spool.rs` (new)
- `crates/server/src/observability/crash/mod.rs` (one `mod` line)

**Steps.**
1. `crash/mod.rs`: add `pub(crate) mod spool;`.
2. Create `spool.rs`. Module doc: `//! The crash-report spool (D-TL-12): `<data_dir>/telemetry/spool/`, one JSON file per report, written by the panic hook and the crash layer on their own thread. Every function returns or swallows its error: none logs (a log would re-enter the crash layer) and none panics (a panic in a panic hook aborts). Writes are synchronous: a stuck disk delays the writing thread, a panicking one included. Only `telemetry/` and `spool/` are ever created, never the data directory. Backups never copy this directory (`backup::write` copies named files only).` Contents in full:
   ```rust
   use std::ffi::OsStr;
   use std::io::{self, Write as _};
   use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
   use std::path::{Path, PathBuf};
   use std::sync::atomic::{AtomicU64, Ordering};
   use std::sync::{Mutex, PoisonError};

   use super::report::{CrashReport, encode};

   /// At most this many reports wait in the spool; the oldest go first.
   pub(crate) const MAX_SPOOL_FILES: usize = 50;
   const DIR_MODE: u32 = 0o700;
   const FILE_MODE: u32 = 0o600;

   /// Makes file names unique within the process.
   static SEQUENCE: AtomicU64 = AtomicU64::new(0);
   /// Serializes prune-and-write, so concurrent writers keep the cap. Never held across a call
   /// that could report (the spool calls nothing that logs or panics).
   static WRITE_LOCK: Mutex<()> = Mutex::new(());

   /// Why the spool could not do something. Carries an I/O kind, never a path or content.
   #[derive(Debug, thiserror::Error)]
   pub(crate) enum SpoolError {
       /// A file system call failed.
       #[error("crash spool i/o failed: {0:?}")]
       Io(io::ErrorKind),
       /// The report does not fit [`super::report::MAX_SPOOLED_BYTES`] even without its backtrace.
       #[error("crash report too large to spool")]
       TooLarge,
   }

   /// One spooled report.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct SpoolFile {
       /// Its path.
       pub(crate) path: PathBuf,
       /// When it was written, from its name (local only: never sent).
       pub(crate) written_at_ns: i64,
   }

   /// The spool directory.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct Spool {
       dir: PathBuf,
   }

   impl Spool {
       /// A spool in `dir`. Does no I/O: the directory is created by the first write.
       pub(crate) fn new(dir: PathBuf) -> Self {
           Self { dir }
       }

       /// `<data_dir>/telemetry/spool`.
       pub(crate) fn for_data_dir(data_dir: &Path) -> Self {
           Self::new(data_dir.join("telemetry").join("spool"))
       }
   }

   /// `crash-<20-digit ns>-<10-digit sequence>.json`: names sort by time. A negative time is 0.
   pub(crate) fn file_name(written_at_ns: i64, seq: u64) -> String {
       format!(
           "crash-{:020}-{:010}.json",
           written_at_ns.max(0),
           seq % 10_000_000_000
       )
   }

   /// The time in a [`file_name`], or `None` for any other name.
   pub(crate) fn parse_file_name(name: &str) -> Option<i64> {
       let stem = name.strip_prefix("crash-")?.strip_suffix(".json")?;
       let (time, seq) = stem.split_once('-')?;
       let digits = |text: &str, len: usize| {
           text.len() == len && text.bytes().all(|byte| byte.is_ascii_digit())
       };
       if !digits(time, 20) || !digits(seq, 10) {
           return None;
       }
       time.parse::<i64>().ok()
   }

   /// Creates the spool directory `dir` and its parent (`telemetry/`), each with mode 0700,
   /// when missing. The grandparent (the data directory) must exist: an error event before
   /// `serve::bind` created it (a startup failure) must not create it with our mode.
   fn create_dirs(dir: &Path) -> io::Result<()> {
       let parent = dir.parent().ok_or(io::ErrorKind::NotFound)?;
       let grandparent = parent.parent().ok_or(io::ErrorKind::NotFound)?;
       if !grandparent.is_dir() {
           return Err(io::ErrorKind::NotFound.into());
       }
       for path in [parent, dir] {
           match std::fs::DirBuilder::new().mode(DIR_MODE).create(path) {
               Ok(()) => {}
               Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
               Err(error) => return Err(error),
           }
       }
       Ok(())
   }

   /// Creates `path` (it must not exist) with `body`.
   fn write_new(path: &Path, body: &[u8]) -> io::Result<()> {
       let mut file = std::fs::OpenOptions::new()
           .write(true)
           .create_new(true)
           .mode(FILE_MODE)
           .open(path)?;
       file.write_all(body)
   }
   ```
3. These methods go **inside the `impl Spool` block of step 2**, after `for_data_dir`; `file_name`, `parse_file_name`, `create_dirs` and `write_new` stay free functions. In full:
   ```rust
   /// Writes `report` as a new file: encode, create `telemetry/spool/` if missing (never the
   /// data directory), drop the oldest files down to 49, write a temporary file, rename it. No fsync: a process exit keeps the page cache,
   /// and only a power loss could lose a report. Returns the new file's path.
   pub(crate) fn write(&self, report: &CrashReport, written_at_ns: i64) -> Result<PathBuf, SpoolError> {
       let body = encode(report).ok_or(SpoolError::TooLarge)?;
       let _guard = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
       create_dirs(&self.dir).map_err(|error| SpoolError::Io(error.kind()))?;
       self.prune(MAX_SPOOL_FILES - 1);
       let name = file_name(written_at_ns, SEQUENCE.fetch_add(1, Ordering::Relaxed));
       let temporary = self.dir.join(format!(".{name}.tmp"));
       let target = self.dir.join(&name);
       let result = write_new(&temporary, &body).and_then(|()| std::fs::rename(&temporary, &target));
       if let Err(error) = result {
           let _ = std::fs::remove_file(&temporary);
           return Err(SpoolError::Io(error.kind()));
       }
       Ok(target)
   }

   /// The spooled reports, oldest first. A missing directory is empty; names that are not
   /// [`file_name`]s are ignored.
   pub(crate) fn list(&self) -> Result<Vec<SpoolFile>, SpoolError> {
       let entries = match std::fs::read_dir(&self.dir) {
           Ok(entries) => entries,
           Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
           Err(error) => return Err(SpoolError::Io(error.kind())),
       };
       let mut files = Vec::new();
       for entry in entries.flatten() {
           let name = entry.file_name();
           if let Some(written_at_ns) = name.to_str().and_then(parse_file_name) {
               files.push(SpoolFile { path: entry.path(), written_at_ns });
           }
       }
       files.sort_by(|a, b| a.path.cmp(&b.path));
       Ok(files)
   }

   /// Deletes the oldest reports until at most `keep` remain. Errors are ignored: the sender
   /// may have deleted a file first.
   fn prune(&self, keep: usize) {
       let Ok(files) = self.list() else {
           return;
       };
       let excess = files.len().saturating_sub(keep);
       for file in files.iter().take(excess) {
           let _ = std::fs::remove_file(&file.path);
       }
   }

   /// Deletes temporary files a crash left mid-write (`.crash-….tmp`). Errors are ignored.
   pub(crate) fn remove_temporaries(&self) {
       let Ok(entries) = std::fs::read_dir(&self.dir) else {
           return;
       };
       for entry in entries.flatten() {
           let name = entry.file_name();
           if name
               .to_str()
               .is_some_and(|name| name.starts_with(".crash-") && name.ends_with(".tmp"))
           {
               let _ = std::fs::remove_file(entry.path());
           }
       }
   }

   /// Deletes the spool directory (crash reports are off), then its parent when that is an
   /// empty `telemetry` directory. A missing directory is fine.
   pub(crate) fn clear(&self) -> Result<(), SpoolError> {
       match std::fs::remove_dir_all(&self.dir) {
           Ok(()) => {}
           Err(error) if error.kind() == io::ErrorKind::NotFound => {}
           Err(error) => return Err(SpoolError::Io(error.kind())),
       }
       if let Some(parent) = self.dir.parent()
           && parent.file_name() == Some(OsStr::new("telemetry"))
       {
           // Removes only an empty directory.
           let _ = std::fs::remove_dir(parent);
       }
       Ok(())
   }
   ```
   (Rewrite a let-chain as nested `if let` if the edition refuses it.)

**Tests** (in `spool.rs`; `tempfile::tempdir()` each; a helper `fn report() -> CrashReport` from `report::event_report` with `Vec::new()` frames, no capture):
- `the_spool_is_under_the_data_dir` (`Spool::for_data_dir(p).dir == p.join("telemetry/spool")`; the test module reads the private field).
- `a_missing_data_dir_is_not_created` (`Spool::for_data_dir(&root.join("absent"))`, one write → `Err(SpoolError::Io(NotFound))`; `root.join("absent")` does not exist afterwards).
- `only_telemetry_and_spool_are_created` (`for_data_dir(root)`, one write → `root/telemetry` and `root/telemetry/spool` exist with mode `0o700`; the mode of `root` is unchanged).
- `write_creates_the_directory_and_one_file` (one file in `list()`; its bytes parse back to the report; `std::os::unix::fs::PermissionsExt` modes: file `0o600`, directory `0o700` (mask `0o777`)).
- `file_names_sort_by_time_and_parse_back` (`file_name(5, 1) < file_name(10, 0)`; `parse_file_name(&file_name(123, 7)) == Some(123)`; `None` for `crash-1-2.json`, `.crash-…​.tmp`, `other.json`, `crash-00000000000000000001-0000000001.txt`).
- `a_negative_time_is_written_as_zero` (`file_name(-5, 0)` starts with `crash-00000000000000000000-`).
- `the_spool_keeps_at_most_fifty_files` (55 writes at times 1..=55 → 50 files; the smallest `written_at_ns` left is 6).
- `no_temporary_file_is_left` (after writes, no name starts with `.`).
- `an_unwritable_spool_is_an_error_not_a_panic` (`Spool::new(root.join("spool"))` where `root/spool` is an existing regular file → `Err(SpoolError::Io(_))`, nothing else written).
- `a_report_that_never_fits_is_refused` (300 field names of 256 bytes → `Err(SpoolError::TooLarge)`, no directory created).
- `remove_temporaries_deletes_only_temporaries` (a `.crash-x.tmp`, a real report and `notes.txt` → only the first is gone).
- `clear_removes_the_spool_and_an_empty_telemetry_dir` (`for_data_dir(root)`, one write, `clear` → neither `telemetry/spool` nor `telemetry` exists; `clear` again → `Ok`).
- `clear_keeps_a_telemetry_dir_with_other_files` (`telemetry/keep.txt` survives).
- `list_of_a_missing_dir_is_empty`.
- `concurrent_writes_keep_the_cap` (8 `std::thread`s × 10 writes on clones of one `Spool` → `list().len() <= 50`, each file parses).

**Commit:** `Add the crash report spool`

---

### TL5.6 — The reporter and the crash layer

**Goal.** One `Reporter` ties the limiter, the spool, the build facts and an injectable clock together, re-entrancy safe; the crash layer feeds it ERROR events.

**Edit:**
- `crates/server/src/observability/crash/mod.rs`
- `crates/server/src/observability/crash/layer.rs` (new)

**M1 names used here:** `crate::observability::panic::PANIC_EVENT_TARGET`, `crate::observability::subscriber::BoxedLayer`. **M4 names:** `facts::{deployment, probe_host, process_uptime, Deployment}`.

**Steps.**
1. `crash/mod.rs`: add `pub(crate) mod layer;` (alphabetical), then below the `mod` lines, in full:
   ```rust
   use std::cell::Cell;
   use std::fmt;
   use std::panic::{Location, PanicHookInfo};
   use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
   use std::time::{SystemTime, UNIX_EPOCH};

   use crate::observability::facts::{self, Deployment};

   use self::limiter::{Limiter, ReportKey};
   use self::report::{
       CrashReport, EventSite, ReportKind, capture_frames, event_report, location_of_event,
       location_of_panic, panic_report, unix_hour,
   };
   use self::spool::Spool;

   /// The clock a [`Reporter`] reads: nanoseconds since the Unix epoch.
   pub(crate) type NowNs = Arc<dyn Fn() -> i64 + Send + Sync>;

   /// The reporter the panic hook uses, set once by `subscriber::init` when the switchboard
   /// turned crash reports on. Unset: the hook spools nothing (design § Panics).
   static REPORTER: OnceLock<Arc<Reporter>> = OnceLock::new();

   thread_local! {
       /// Set while this thread builds or spools a report: an event or a panic raised meanwhile
       /// is skipped instead of re-entering (and deadlocking on) the limiter.
       static REPORTING: Cell<bool> = const { Cell::new(false) };
   }

   /// Marks this thread as reporting until dropped.
   struct ReportGuard;

   impl ReportGuard {
       /// `None` when this thread is already reporting, or its thread-local storage is gone.
       fn enter() -> Option<Self> {
           let entered = REPORTING
               .try_with(|flag| !flag.replace(true))
               .unwrap_or(false);
           entered.then_some(Self)
       }
   }

   impl Drop for ReportGuard {
       fn drop(&mut self) {
           let _ = REPORTING.try_with(|flag| flag.set(false));
       }
   }

   /// Builds, deduplicates and spools crash reports. Never logs.
   pub(crate) struct Reporter {
       spool: Spool,
       limiter: Mutex<Limiter>,
       deployment: Deployment,
       now_ns: NowNs,
   }

   impl fmt::Debug for Reporter {
       fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
           f.debug_struct("Reporter")
               .field("spool", &self.spool)
               .field("deployment", &self.deployment)
               .finish_non_exhaustive()
       }
   }
   ```
2. `impl Reporter`, with these items:
   - `pub(crate) fn new(spool: Spool, deployment: Deployment, now_ns: NowNs) -> Self` (limiter `Mutex::new(Limiter::default())`).
   - `pub(crate) fn system(spool: Spool) -> Self`: `Self::new(spool, facts::deployment(facts::probe_host()), Arc::new(system_now_ns))`. Not `crate::clock::Clock::System`: its `system_now_ns` uses `expect`, and a panic inside the panic hook aborts the process.
   - `fn limiter(&self) -> MutexGuard<'_, Limiter>`: `self.limiter.lock().unwrap_or_else(PoisonError::into_inner)`.
   - `fn occurrence(&self, key: &ReportKey, build: impl FnOnce() -> CrashReport)` (the key by reference: `clippy::needless_pass_by_value`), in full:
     ```rust
     fn occurrence(&self, key: &ReportKey, build: impl FnOnce() -> CrashReport) {
         let Some(_guard) = ReportGuard::enter() else {
             return;
         };
         let hour = unix_hour((self.now_ns)());
         let known = self.limiter().knows(key);
         // The backtrace is captured outside the lock, and only for a new location.
         let template = if known { None } else { Some(build()) };
         let due = self.limiter().record(key, template, hour);
         if let Some(report) = due {
             let _ = self.spool.write(&report, (self.now_ns)());
         }
     }
     ```
     (Each `self.limiter()` guard is a temporary dropped at the end of its statement: the lock is never held while building or writing.)
   - `pub(crate) fn report_event(&self, site: &EventSite<'_>)`: `let key = ReportKey { kind: ReportKind::ErrorEvent, location: location_of_event(site.file, site.line) };` then `self.occurrence(&key, || event_report(site, capture_frames(), self.deployment, facts::process_uptime()));`.
   - `pub(crate) fn report_panic(&self, location: Option<&Location<'_>>)`: the same with `ReportKind::Panic`, `location_of_panic(location)` and `panic_report(location, capture_frames(), self.deployment, facts::process_uptime())`.
   - `pub(crate) fn flush_pending(&self)`: enter a `ReportGuard` (return if `None`); `let hour = unix_hour((self.now_ns)());`; `let reports = self.limiter().flush(hour);`; write each with `let _ = self.spool.write(&report, (self.now_ns)());`.
3. Free functions in `crash/mod.rs`, after `impl Reporter`:
   ```rust
   /// Nanoseconds since the Unix epoch, or 0 when the clock is before it or out of range.
   /// Never panics: the panic hook reads it.
   fn system_now_ns() -> i64 {
       SystemTime::now()
           .duration_since(UNIX_EPOCH)
           .ok()
           .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
           .unwrap_or(0)
   }

   /// Lets the panic hook spool reports through `reporter`. The first call wins; `cetra serve`
   /// makes one, from `subscriber::init`, after the switchboard turned crash reports on.
   pub(crate) fn enable_panic_reports(reporter: Arc<Reporter>) {
       let _ = REPORTER.set(reporter);
   }

   /// The panic hook's crash report (M1's `panic::report` seam). Does nothing until
   /// [`enable_panic_reports`] ran. Reads the panic's location, never its message.
   pub(crate) fn report_panic(info: &PanicHookInfo<'_>) {
       if let Some(reporter) = REPORTER.get() {
           reporter.report_panic(info.location());
       }
   }
   ```
4. Create `crash/layer.rs`. Module doc: `//! The crash layer: every ERROR `tracing` event except the panic hook's own becomes a crash report (design § 3). It reads the event's metadata only; the event's values are never visited.` Contents in full:
   ```rust
   use std::sync::Arc;

   use tracing::{Event, Level, Metadata, Subscriber};
   use tracing_subscriber::Layer;
   use tracing_subscriber::filter::filter_fn;
   use tracing_subscriber::layer::Context;

   use super::Reporter;
   use super::report::EventSite;
   use crate::observability::panic::PANIC_EVENT_TARGET;
   use crate::observability::subscriber::BoxedLayer;

   /// Turns ERROR events into crash reports.
   #[derive(Debug, Clone)]
   pub(crate) struct CrashLayer {
       reporter: Arc<Reporter>,
   }

   impl CrashLayer {
       /// A layer that reports to `reporter`.
       pub(crate) fn new(reporter: Arc<Reporter>) -> Self {
           Self { reporter }
       }
   }

   impl<S: Subscriber> Layer<S> for CrashLayer {
       fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
           let metadata = event.metadata();
           if is_reported(metadata) {
               self.reporter.report_event(&EventSite::from_metadata(metadata));
           }
       }
   }

   /// Whether the crash layer reports this callsite: [`reportable`] over its metadata.
   pub(crate) fn is_reported(metadata: &Metadata<'_>) -> bool {
       reportable(
           metadata.is_event(),
           *metadata.level(),
           metadata.target(),
           metadata.file().is_some(),
       )
   }

   /// An ERROR event that is not the panic hook's `panic` event (the hook reports panics itself,
   /// so a panic is reported once) and has a source file. Events bridged from the `log` crate
   /// have none: they would all share one report saying nothing (target `log`, location
   /// `unknown`), so they are skipped.
   fn reportable(is_event: bool, level: Level, target: &str, has_file: bool) -> bool {
       is_event && level == Level::ERROR && target != PANIC_EVENT_TARGET && has_file
   }

   /// The crash layer as one of the subscriber's layers, with its own filter: it sees ERROR
   /// events whatever `log.level` says, and no span.
   pub(crate) fn boxed(reporter: Arc<Reporter>) -> BoxedLayer {
       CrashLayer::new(reporter)
           .with_filter(filter_fn(is_reported))
           .boxed()
   }
   ```
   If `filter_fn(is_reported)` does not type-check (it wants `Fn(&Metadata<'_>) -> bool`), use `filter_fn(|metadata| is_reported(metadata))`. If `BoxedLayer` is not reachable as `crate::observability::subscriber::BoxedLayer`, stop and report.

**Tests.** In `layer.rs` (`tempfile::tempdir()` each; never the global `REPORTER`, never `set_global_default`):
- Imports of the test module: `std::path::Path`, `std::panic::Location`, `std::sync::Arc`, `std::sync::atomic::{AtomicI64, Ordering}`, `tracing::Dispatch`, `tracing_subscriber::layer::SubscriberExt`, `crate::observability::facts::Deployment`, `crate::observability::crash::report::{CrashReport, EventSite, ReportKind}`, `crate::observability::crash::spool::Spool`, `crate::observability::crash::ReportGuard` (a private item of the parent module, visible here), and `super::*`.
- Helpers: `fn reporter(dir: &Path, now: &Arc<AtomicI64>) -> Arc<Reporter>` = `Arc::new(Reporter::new(Spool::new(dir.join("spool")), Deployment::Binary, { let now = Arc::clone(now); Arc::new(move || now.load(Ordering::Relaxed)) }))`, the clock starting at `1_767_225_600_000_000_000`; `fn dispatch(reporter: &Arc<Reporter>) -> tracing::Dispatch` = `Dispatch::new(tracing_subscriber::registry().with(vec![boxed(Arc::clone(reporter))]))`; `fn read_all(dir: &Path) -> Vec<(String, CrashReport)>` (each spool file's text and parsed report, oldest first; it reads the spool directory the test created, `dir.join("spool")`); `#[inline(never)] fn emit_one()` containing one `tracing::error!(project_id = 7, "repeated")` (one callsite).
- `an_error_event_is_spooled_with_its_field_names_only`: under `tracing::dispatcher::with_default(&dispatch, || …)`, `tracing::error!(key = "sk-cetra-planted0000000000000000000000000", user = "planted-user@example.com", input = "planted prompt text", "failed for planted-user@example.com")`. One file; kind `ErrorEvent`; location starts with `crates/server/src/observability/crash/layer.rs:`; `event_target == Some(module_path!().to_owned())`; `fields == ["key", "user", "input"]`; `count == 1`; `first_hour == 490_896`. The raw text contains none of `sk-cetra-`, `planted`, `example.com`, `prompt text`, `failed for`.
- `warn_and_info_events_are_not_reported` (no spool directory).
- `the_panic_event_is_not_reported` (`tracing::error!(target: PANIC_EVENT_TARGET, location = "x", "panic")` → no file).
- `reportable_rules` (pure: `(true, ERROR, "cetra_x", true)` → true; a span (`is_event` false), `WARN`, the target `PANIC_EVENT_TARGET`, and `has_file` false (a `log`-bridged event) → false each).
- `errors_below_a_quiet_stdout_level_are_still_seen` (the layer has its own filter: a `registry().with(vec![boxed(r), fmt layer filtered at "off"])` still spools one file).
- `one_callsite_is_reported_once_an_hour` (`emit_one()` × 5 → one file; advance the clock by one hour; `reporter.flush_pending()` → two files, the second with `count == 4`, `first_hour == last_hour == 490_896`).
- `distinct_callsites_get_distinct_reports` (two different `error!` lines → two files).
- `the_reporter_caps_reports_per_hour` (25 calls of `reporter.report_event(&EventSite { file: Some("crates/x.rs"), line: Some(n), target: "cetra_x", module_path: None, fields: vec![] })` with `n` in 1..=25 → 20 files).
- `concurrent_errors_from_one_callsite_spool_one_report` (8 `std::thread`s, each `tracing::dispatcher::with_default(&dispatch.clone(), || for _ in 0..50 { emit_one() })` → one file; after one hour and `flush_pending` → a second file with `count == 399`).
- `a_spool_failure_is_swallowed` (spool dir path is a regular file; `error!` → no panic, the file is unchanged).
- `report_panic_writes_a_panic_report` (`reporter.report_panic(Some(Location::caller()))` → kind `Panic`, location from this file, `fields` empty, `event_target` and `module` absent from the raw text, the backtrace's first frame does not start with `backtrace::`, `std::` or `cetra_server::observability::crash::Reporter`).
In `crash/mod.rs` (`#[cfg(test)] mod tests` at the end of the file):
- `report_guard_refuses_a_second_entry` (as M1's `hook_guard_refuses_a_second_entry`).
- `an_occurrence_while_reporting_is_skipped` (hold `ReportGuard::enter()`, call `reporter.report_event(…)` → no file; drop the guard, call again → one file).
- `report_panic_without_a_reporter_does_nothing`: not testable without a `PanicHookInfo`; instead `the_global_reporter_is_unset_in_tests` asserts `REPORTER.get().is_none()` (no test sets it; TL5.9's child process does, in its own process).

**Commit:** `Add the crash reporter and the crash layer`

---

### TL5.7 — Wire the layer and the panic hook

**Goal.** `subscriber::build` adds the crash layer when the switchboard allows crash reports, `init` enables panic reports, and the panic hook spools through the M1 seam.

**Edit:**
- `crates/server/src/observability/panic.rs` (M1's)
- `crates/server/src/observability/subscriber.rs` (M1's)

**M1 names used here** (checked against the committed code; stop if one differs): in `panic.rs`, `pub fn install()` with its `static INSTALLED: Once`, and `fn report(info: &PanicHookInfo<'_>)` whose body is `let Some(_guard) = HookGuard::enter() else { return; };`, `let location = panic_location(info);` and `tracing::error!(target: PANIC_EVENT_TARGET, location = %location, "panic");`, with the doc "What the hook adds to the default hook. Track TL M5 adds the crash-report spool here, after the event.". In `subscriber.rs`: `#[derive(Debug)] pub struct Observability { logs: Option<LogPipeline> }` with `exporting_logs`; `pub fn init` (its `subscriber.try_init().map_err(|_| ObservabilityError::AlreadyInstalled)?;` step); `pub(crate) fn build` (its `let mut layers: Vec<BoxedLayer> = vec![stdout];`, the `let logs = match config.self_monitoring.logs_endpoint() { … };` block, and the final `Ok((tracing_subscriber::registry().with(layers), Observability { logs }))`); the test helpers `Buffer`, `stdout_writer`, `build_for`. M2 or M3 may have added layers, `Observability` fields or `init` lines first: keep them all, and add M5's items next to them (§ Prerequisites).

**Steps.**
1. `panic.rs`:
   1. In `install`, add as the first statement (before `static INSTALLED`): `crate::observability::facts::mark_process_start();` with the comment `// The crash reports' uptime counts from here (track TL M5); \`serve::bind\` marks it too.`
   2. In `report`, add between the `HookGuard` line and `let location = panic_location(info);`:
      ```rust
      // Opt-in crash report first (track TL M5): the event below goes through every layer, which
      // may block on a lock the panicking thread holds; the spool is what must survive.
      // Nothing happens until `subscriber::init` enabled it.
      crate::observability::crash::report_panic(info);
      ```
      The `tracing::error!` line stays as it is, after it. The crash layer ignores that event either way.
   3. Change `report`'s doc comment to: `/// What the hook adds to the default hook: the crash report when crash reports are on (track TL M5), then the \`panic\` event.`
2. `subscriber.rs`, additions only:
   1. Imports: `std::sync::Arc`; `crate::observability::crash::{self, Reporter}`; `crate::observability::crash::spool::Spool`; `crate::observability::switchboard::{Switch, for_config}` (merge with the existing `use crate::observability::switchboard::NOTICE_DIRECTIVE;`).
   2. In `Observability`, add the field (after `logs` and any field another milestone added):
      ```rust
      /// The crash reporter when crash reports are on (track TL M5).
      crash: Option<Arc<Reporter>>,
      ```
      and, in `impl Observability`, the method:
      ```rust
      /// Whether the crash layer is installed.
      pub fn reporting_crashes(&self) -> bool {
          self.crash.is_some()
      }
      ```
      `Observability::shutdown` does not change. Run `grep -rn "Observability {" crates/server/src crates/cli/src` and add `crash: None` to every literal other than the one in `build` (none exists in the committed code; M3 or M2 may have added one).
   3. In `build`, add right before the final `Ok(…)` (after any layer M2 or M3 pushed):
      ```rust
      // Crash reports to us, when the switchboard allows them (track TL M5, D-TL-11). The
      // decision is pure, so `serve::bind` reaches the same one for the sender.
      let crash = match for_config(config).crash_reports {
          Switch::On => {
              let reporter = Arc::new(Reporter::system(Spool::for_data_dir(&config.data_dir)));
              layers.push(crash::layer::boxed(Arc::clone(&reporter)));
              Some(reporter)
          }
          Switch::Off(_) => None,
      };
      ```
      and add `crash` to the `Observability { … }` literal of the final `Ok(…)`.
   4. In `init`, add right after the `try_init` step:
      ```rust
      if let Some(reporter) = &observability.crash {
          crash::enable_panic_reports(Arc::clone(reporter));
      }
      ```
      `build` never sets the global, so tests that call `build` do not change the process's panic hook behaviour.

**Tests** (in `subscriber.rs`'s `mod tests`; each is `#[tokio::test]`, since `build` takes `Handle::current()`; configs from `load_from(Some(&toml), env)`, with `data_dir` in a `tempfile::tempdir()` written as a TOML literal string `data_dir = '<path>'` and `[telemetry]` with `endpoint = "http://127.0.0.1:9"`; the subscriber built with `build(&config, LogFormat::Pretty, stdout_writer(&buffer), Handle::current())` and set with `tracing::dispatcher::set_default(&Dispatch::new(subscriber))`; extra imports: none beyond the module's existing `Dispatch`, `Buffer`, `stdout_writer`, `load_from`):
- `crash_reports_off_means_no_crash_layer` (`load_from(None, &[])` → `reporting_crashes() == false`).
- `crash_reports_on_adds_the_crash_layer` (`crash_reports = true` → `true`; `tracing::error!("crash-canary")` → one file under `<data_dir>/telemetry/spool/`, whose text does not contain `crash-canary`).
- `cetra_telemetry_off_means_no_crash_layer` (`crash_reports = true` with `&[("CETRA_TELEMETRY", "off")]` → `false`; no `telemetry` directory after an `error!`).
- `restore_mode_means_no_crash_layer` (`restore_mode = true` → `false`).
- `the_crash_layer_ignores_the_log_level` (`[log]\nlevel = "off"` and `crash_reports = true`: an `error!` is spooled, the stdout buffer is empty).
- M1's tests keep passing unchanged.

**Commit:** `Wire crash reports into the subscriber and the panic hook`

---

### TL5.8 — The sender, the startup notice and the spool cleanup

**Goal.** At startup `cetra serve` logs the crash-reports notice; when crash reports are on it starts a task that posts spooled reports every minute, and when they are off it deletes the spool. Backups never contain the spool.

**Edit:**
- `crates/server/src/observability/crash/sender.rs` (new)
- `crates/server/src/observability/crash/mod.rs`
- `crates/server/src/observability/test_receiver.rs` (new, test-only)
- `crates/server/src/observability/mod.rs` (one line)
- `crates/server/src/serve.rs`
- `crates/server/src/backup.rs` (one test only)

**M4 names used here** (committed): `heartbeat::{client, REQUEST_TIMEOUT}` (`client(timeout) -> Result<reqwest::Client, HeartbeatError>`: no redirects, the timeout, `User-Agent: cetra/<version>`), `instance::instance_id(&MetaDb) -> Result<Uuid, MetaError>`, `switchboard::{Decision, Switch, NOTICE_TARGET, OffReason}`, `AppState.clock` (`Clock`, `now_ns()`), and in `serve.rs` `bind` the lines `let decision = crate::observability::switchboard::for_config(&state.config);` and `if let Some(task) = crate::observability::heartbeat::start(&state, decision).await { tasks.push(task); }` (near line 621). M2, M3 or M6 may have added lines around them: keep them.

**Steps.**
1. Create `sender.rs`. Module doc: `//! Posts spooled crash reports to `<telemetry.endpoint>/v1/crash` (D-TL-12): oldest first, with the instance id added; deleted on 2xx or on a 4xx other than 408 and 429, kept on anything else, dropped unsent after 7 days. Logs nothing itself; the loop in `start` logs a summary at `debug`.` Contents:
   ```rust
   use std::time::Duration;

   use uuid::Uuid;

   use super::report::{CRASH_SCHEMA, CrashReport, MAX_SPOOLED_BYTES};
   use super::spool::Spool;

   /// One pass a minute.
   pub(crate) const SEND_PERIOD: Duration = Duration::from_secs(60);
   /// A report not sent within this long is dropped.
   pub(crate) const MAX_AGE_NS: i64 = 7 * 24 * 3_600 * 1_000_000_000;

   /// What one pass did.
   #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
   pub(crate) struct SendSummary {
       /// Accepted (2xx) and deleted.
       pub(crate) sent: u32,
       /// Deleted unsent: refused (a 4xx other than 408 and 429), too old, too large, unreadable as a report.
       pub(crate) dropped: u32,
       /// Kept for the next pass: 408, 429, 1xx, 3xx, 5xx, network failure (the pass stops there).
       pub(crate) kept: u32,
   }

   /// `<endpoint>/v1/crash`.
   pub(crate) fn crash_url(endpoint: &str) -> String {
       format!("{}/v1/crash", endpoint.trim_end_matches('/'))
   }
   ```
   and `pub(crate) async fn send_spool(client: &reqwest::Client, url: &str, spool: &Spool, instance_id: Option<Uuid>, now_ns: i64) -> SendSummary`, whose steps are, in order:
   1. `let Ok(files) = spool.list() else { return SendSummary::default(); };`
   2. For each file, oldest first:
      1. If `now_ns.saturating_sub(file.written_at_ns) > MAX_AGE_NS`: `let _ = tokio::fs::remove_file(&file.path).await;`, `dropped += 1`, next file.
      2. `tokio::fs::read(&file.path).await`: `NotFound` → next file (pruned meanwhile); another error → `kept += 1`, next file.
      3. If the bytes are longer than `MAX_SPOOLED_BYTES`: remove, `dropped += 1`, next.
      4. `serde_json::from_slice::<CrashReport>(&bytes)`: an error (unknown key, bad value) → remove, `dropped += 1`, next. A `schema` other than `CRASH_SCHEMA` → remove, `dropped += 1`, next.
      5. `report.instance_id = instance_id;`
      6. `client.post(url).json(&report).send().await`:
         - `Err(_)` → `kept += 1`, **stop the pass**;
         - a 2xx status → remove, `sent += 1`;
         - 408 or 429 (the relay is busy or rate-limits) → `kept += 1`, **stop the pass**;
         - any other 4xx status → remove, `dropped += 1`;
         - any other status (1xx, 3xx, 5xx) → `kept += 1`, **stop the pass**.
         The response body is never read.
   3. Return the summary.
2. `crash/mod.rs`: add `pub(crate) mod sender;`. Add imports `tokio::task::JoinHandle`, `crate::http::AppState`, `crate::observability::heartbeat`, `crate::observability::instance`, `crate::observability::switchboard::{Decision, NOTICE_TARGET, Switch}`, `self::sender::{SEND_PERIOD, SendSummary, crash_url, send_spool}`. Add:
   ```rust
   /// The docs section listing every field, logged in the notice.
   pub(crate) const CRASH_DOCS_URL: &str = "https://cetra.sh/docs/self-hosting/telemetry/#crash-reports";

   /// The startup line when crash reports are on. Shows at `log.level = "warn"` too.
   fn notice_on() {
       tracing::info!(
           target: NOTICE_TARGET,
           docs = CRASH_DOCS_URL,
           "telemetry: crash reports are on: panics and error events are sent to us as code locations and function names, never messages or values; turn them off with telemetry.crash_reports = false or CETRA_TELEMETRY=off"
       );
   }

   /// The startup line when crash reports are off, with why.
   fn notice_off(reason: &'static str) {
       tracing::info!(target: NOTICE_TARGET, reason, "telemetry: crash reports are off");
   }
   ```
   and `pub(crate) async fn start(state: &AppState, decision: Decision) -> Option<JoinHandle<()>>`. Doc: "Logs the crash-reports notice; when off, deletes the spool; when on, spawns the sender. Fails closed." Steps:
   1. `let spool = Spool::for_data_dir(&state.config.data_dir);`
   2. If `decision.crash_reports` is `Switch::Off(reason)`: `if let Err(error) = spool.clear() { tracing::debug!(error = %error, "telemetry: crash spool not cleared"); }`, `notice_off(reason.as_str())`, return `None`.
   3. `spool.remove_temporaries();`
   4. `let Ok(client) = heartbeat::client(heartbeat::REQUEST_TIMEOUT) else { notice_off("client_unavailable"); return None; };` (its own client instance: no redirects, 10 s, `User-Agent: cetra/<version>`).
   5. `let instance_id = match instance::instance_id(&state.meta).await { Ok(id) => Some(id), Err(error) => { tracing::debug!(error = %error, "telemetry: instance id"); None } };`
   6. `notice_on();`
   7. `let url = crash_url(&state.config.telemetry.endpoint);` and `let clock = state.clock.clone();`
   8. Spawn and return:
      ```rust
      Some(tokio::spawn(async move {
          loop {
              // Counted repeats of past hours go to the spool first (design § Details settled, dedup).
              if let Some(reporter) = REPORTER.get() {
                  reporter.flush_pending();
              }
              let summary = send_spool(&client, &url, &spool, instance_id, clock.now_ns()).await;
              if summary != SendSummary::default() {
                  tracing::debug!(
                      sent = summary.sent,
                      dropped = summary.dropped,
                      kept = summary.kept,
                      "telemetry: crash reports"
                  );
              }
              tokio::time::sleep(SEND_PERIOD).await;
          }
      }))
      ```
3. The shared test receiver. In `observability/mod.rs`, add `#[cfg(test)]` then `pub(crate) mod test_receiver;` in alphabetical position among the `mod` lines. Create `observability/test_receiver.rs` with exactly:
   ```rust
   //! A local HTTP receiver for the telemetry tests (crash reports; the heartbeat's tests keep
   //! their own copy in `heartbeat.rs`). Serves `POST <path>` on `127.0.0.1:0`: waits `delay`,
   //! records the request, answers `status`, with `Location: /elsewhere` when `status` is a 3xx.
   //! `GET` and `POST /elsewhere` count hits and answer 200. Aborts its task on drop.

   use std::sync::atomic::{AtomicUsize, Ordering};
   use std::sync::{Arc, Mutex, PoisonError};
   use std::time::Duration;

   use axum::Router;
   use axum::body::Bytes;
   use axum::extract::State;
   use axum::http::{HeaderMap, StatusCode};
   use axum::response::{IntoResponse, Response};
   use axum::routing::{get, post};

   /// A running receiver.
   pub(crate) struct Receiver {
       /// `http://127.0.0.1:<port>`, without a trailing slash.
       pub(crate) base: String,
       requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
       elsewhere: Arc<AtomicUsize>,
       task: tokio::task::JoinHandle<()>,
   }

   #[derive(Clone)]
   struct ReceiverState {
       status: u16,
       delay: Duration,
       requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
       elsewhere: Arc<AtomicUsize>,
   }

   impl Receiver {
       /// Serves `POST path` (for example `/v1/crash`).
       pub(crate) async fn start(path: &str, status: u16, delay: Duration) -> Self {
           let requests = Arc::new(Mutex::new(Vec::new()));
           let elsewhere = Arc::new(AtomicUsize::new(0));
           let state = ReceiverState {
               status,
               delay,
               requests: Arc::clone(&requests),
               elsewhere: Arc::clone(&elsewhere),
           };
           let router = Router::new()
               .route(path, post(record))
               .route("/elsewhere", get(other).post(other))
               .with_state(state);
           let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
           let addr = listener.local_addr().unwrap();
           let task = tokio::spawn(async move {
               let _ = axum::serve(listener, router).await;
           });
           Self {
               base: format!("http://{addr}"),
               requests,
               elsewhere,
               task,
           }
       }

       /// The requests received so far, in order.
       pub(crate) fn requests(&self) -> Vec<(HeaderMap, Bytes)> {
           self.requests
               .lock()
               .unwrap_or_else(PoisonError::into_inner)
               .clone()
       }

       /// The request bodies received so far, in order.
       pub(crate) fn bodies(&self) -> Vec<Bytes> {
           self.requests().into_iter().map(|(_, body)| body).collect()
       }

       /// Hits on `/elsewhere` (a followed redirect).
       pub(crate) fn elsewhere_hits(&self) -> usize {
           self.elsewhere.load(Ordering::SeqCst)
       }
   }

   impl Drop for Receiver {
       fn drop(&mut self) {
           self.task.abort();
       }
   }

   async fn record(State(state): State<ReceiverState>, headers: HeaderMap, body: Bytes) -> Response {
       tokio::time::sleep(state.delay).await;
       state
           .requests
           .lock()
           .unwrap_or_else(PoisonError::into_inner)
           .push((headers, body));
       let status = StatusCode::from_u16(state.status).unwrap();
       if status.is_redirection() {
           (status, [(axum::http::header::LOCATION, "/elsewhere")]).into_response()
       } else {
           status.into_response()
       }
   }

   async fn other(State(state): State<ReceiverState>) -> StatusCode {
       state.elsewhere.fetch_add(1, Ordering::SeqCst);
       StatusCode::OK
   }
   ```
   The file is compiled in tests only (the `#[cfg(test)]` on its `mod` line), so its `unwrap` calls are allowed (`clippy.toml` `allow-unwrap-in-tests`). If clippy's `missing_debug_implementations` still warns for `Receiver` in the test build, add `#[derive(Debug)]` to `ReceiverState` and `Receiver` (`JoinHandle`, `HeaderMap`, `Bytes`, `Mutex` and `AtomicUsize` are all `Debug`). M4's heartbeat tests keep their private copy: moving them means editing nine call sites and the imports of `heartbeat.rs`, a file M6 edits in parallel, for no behaviour change. A later cleanup may switch them to `Receiver::start("/v1/heartbeat", …)`.
4. `serve.rs` `bind`: right after M4's `if let Some(task) = crate::observability::heartbeat::start(&state, decision).await { … }` block, add:
   ```rust
   if let Some(task) = crate::observability::crash::start(&state, decision).await {
       tasks.push(task);
   }
   ```
   Shutdown aborts it with the other `tasks`. `restore_mode_skips_the_sending_and_dns_loops` keeps its count of 4: test configs leave `crash_reports` false, so neither run starts the sender.
5. `backup.rs`, test module only: add the test below. Do not change `write` or any other code: it copies `meta.db` (`VACUUM INTO`), `catalog.db`, the objects the catalog lists, `secret.key` and `manifest.json` by name, so `telemetry/` is never copied.

**Tests.**
In `sender.rs` (`#[tokio::test]`; the receiver is `crate::observability::test_receiver::Receiver::start("/v1/crash", status, Duration::ZERO)`; the client is `heartbeat::client(heartbeat::REQUEST_TIMEOUT).unwrap()`; reports come from `report::event_report(…, Vec::new(), …)` with `count`, `first_hour` and `last_hour` set, written with `Spool::write`; `NOW = 1_767_225_600_000_000_000`, passed as `send_spool`'s `now_ns`):
- `crash_url_appends_the_path` (`https://telemetry.cetra.sh` and `https://r.example.com/cetra/` → `…/v1/crash`).
- `a_report_is_posted_with_the_instance_id_and_deleted` (one spooled file at `NOW`; `Some(id)` → one request; the body parses to the report with `instance_id == Some(id)`; `content-type` starts with `application/json`; `user-agent == format!("cetra/{}", facts::VERSION)`; the file is gone; summary `sent: 1`).
- `without_an_instance_id_the_key_is_absent` (the body JSON has no `instance_id` key).
- `a_4xx_deletes_the_report` (400 and 413 → gone, `dropped: 1` each).
- `a_408_or_429_keeps_the_report_and_stops_the_pass` (two files; 408 → exactly one request, both kept, `kept: 1`; the same with 429).
- `a_5xx_keeps_the_report_and_stops_the_pass` (two files; 503 → exactly one request; both files remain; `kept: 1`).
- `a_redirect_is_not_followed_and_keeps_the_report` (307 → `elsewhere_hits() == 0`; file kept).
- `a_network_failure_keeps_the_report` (bind a `TcpListener`, note its address, drop it; post there → file kept, `kept: 1`).
- `reports_older_than_seven_days_are_dropped_unsent` (one file at `NOW - MAX_AGE_NS - 1` → removed, no request; one at `NOW - MAX_AGE_NS` → sent).
- `a_foreign_or_corrupt_file_is_dropped_unsent` (files named by `spool::file_name` holding `{"schema":1,"message":"x"}`, the panic golden with `"schema": 2`, and `not json` → all removed, no request, `dropped: 3`).
- `an_oversized_file_is_dropped` (a `file_name` file of `MAX_SPOOLED_BYTES + 1` bytes → removed, no request).
- `the_oldest_report_goes_first` (files at `NOW - 2` and `NOW - 1` with different locations → requests in that order).
- `an_empty_or_missing_spool_sends_nothing`.
In `crash/mod.rs` `mod tests` (`TestApp`, `crate::test_support::capture_logs`):
- `start_when_off_clears_the_spool_and_logs_the_reason` (write a report with `Spool::for_data_dir(&app.state.config.data_dir)` at `app.state.clock.now_ns()`; `start(&app.state, Decision { heartbeat: Switch::Off(OffReason::Config), crash_reports: Switch::Off(OffReason::DoNotTrack) })` → `None`; the spool directory is gone; logs contain `telemetry: crash reports are off` and `DO_NOT_TRACK`; no `telemetry.instance_id` row).
- `start_when_on_logs_and_spawns` (`crash_reports: Switch::On` → `Some(handle)`, aborted; logs contain `telemetry: crash reports are on` and `CETRA_TELEMETRY=off`; the instance id row exists).
- `the_sender_posts_what_was_spooled` (`let receiver = Receiver::start("/v1/crash", 204, Duration::ZERO).await;` then `TestApp::with_config(|c| { c.telemetry.endpoint = receiver.base.clone(); c.telemetry.crash_reports = true; })`; write one report with `written_at_ns = app.state.clock.now_ns()` (the sender compares against that clock); `start(…, Switch::On)`; poll the receiver every 50 ms for up to 5 s until one request arrived; its `instance_id` equals the stored id; the spool is empty; abort the handle).
- `the_crash_modules_do_not_touch_licences`, in the shape of M4's `the_telemetry_modules_do_not_touch_licences`, over `include_str!` of `mod.rs`, `layer.rs`, `limiter.rs`, `report.rs`, `sender.rs`, `spool.rs` (each cut at its first `#[cfg(test)]`), with the same forbidden words built by `concat!`.
- `the_report_path_never_logs`: `report.rs`, `limiter.rs`, `spool.rs` and `layer.rs`, cut at their first `#[cfg(test)]`, contain none of `tracing::error!`, `tracing::warn!`, `tracing::info!`, `tracing::debug!`, `tracing::trace!`, `tracing::event!` (the reporter's paths must not log; `layer.rs` imports types from `tracing`, which is fine).
In `serve.rs` tests:
- `the_startup_notice_says_crash_reports_are_off` (`capture_logs(Box::pin(bind(config(dir.path()), None)))`: logs contain `telemetry: crash reports are off` and `config`; shut down).
- `crash_reports_on_logs_the_notice` (config with `telemetry.crash_reports = true` → logs contain `telemetry: crash reports are on`; shut down).
- `restore_mode_clears_the_crash_spool` (create `<data>/telemetry/spool/crash-00000000000000000001-0000000001.json` first; bind with `restore_mode = true` → the `spool` directory is gone; the line with `telemetry: crash reports are off` contains `restore_mode`).
In `backup.rs` tests:
- `the_crash_spool_is_not_backed_up` (as `a_full_backup_has_every_file_and_a_manifest`: bind, write `data/telemetry/spool/crash-00000000000000000001-0000000001.json` with `{}`, append 3 spans, `run(…, false)`, shut down; `target.join("telemetry")` does not exist, and the target's top-level names are exactly `catalog.db`, `manifest.json`, `meta.db`, `objects`, `secret.key`).

**Commit:** `Send spooled crash reports and log the crash-reports notice`

---

### TL5.9 — Leak tests, in process and in a child process

**Goal.** Prove end to end that a planted secret key, email and prompt in a panic message or in event fields never reach a spool file or a request body, with the real panic hook in a child process.

**Edit:**
- `crates/server/src/observability/crash/leak_tests.rs` (new)
- `crates/server/src/observability/crash/mod.rs` (one line)

**Steps.**
1. `crash/mod.rs`: add `#[cfg(test)] mod leak_tests;` at the end of the file, right before `#[cfg(test)] mod tests`.
2. Create `leak_tests.rs`. Module doc: `//! Leak tests for crash reports (design § 3 Never). The child-process test runs the real panic hook, which is process-global, in its own process, so no other test sees it.` Constants:
   ```rust
   const PLANTED_KEY: &str = "sk-cetra-planted0000000000000000000000000";
   const PLANTED_EMAIL: &str = "planted-user@example.com";
   const PLANTED_PROMPT: &str = "planted prompt text";
   /// Set by the parent: the child's spool directory.
   const CHILD_SPOOL_VAR: &str = "CETRA_TEST_CRASH_CHILD_SPOOL";
   /// What no spool file or request body may contain.
   const FORBIDDEN: [&str; 5] = [PLANTED_KEY, "sk-cetra-", PLANTED_EMAIL, "@example.com", PLANTED_PROMPT];
   /// The reporter's fixed clock in the in-process tests: 2026-01-01T00:00Z. `send_spool` gets
   /// the same value, so no report is dropped as older than 7 days.
   const FIXED_NOW_NS: i64 = 1_767_225_600_000_000_000;
   ```
   There is no bare `planted` needle: the first kept frame of a report is often the test function, whose name must not trip the check. No test name contains a `FORBIDDEN` string (§ Conventions).
   and `fn assert_clean(text: &str)` asserting each `FORBIDDEN` needle is absent (the message names the needle, not the text).
3. The child, in full:
   ```rust
   /// Run by `a_real_panic_and_error_leak_nothing` in a child process; returns at once
   /// otherwise (for example under `--run-ignored`).
   #[test]
   #[ignore = "a child process of a_real_panic_and_error_leak_nothing"]
   fn crash_child_process() {
       let Some(dir) = std::env::var_os(CHILD_SPOOL_VAR) else {
           return;
       };
       let reporter = Arc::new(Reporter::system(Spool::new(PathBuf::from(dir))));
       let subscriber = tracing_subscriber::registry().with(vec![layer::boxed(Arc::clone(&reporter))]);
       tracing::subscriber::set_global_default(subscriber).unwrap();
       enable_panic_reports(reporter);
       crate::observability::panic::install();
       tracing::error!(
           key = PLANTED_KEY,
           user = PLANTED_EMAIL,
           input = PLANTED_PROMPT,
           "failed for {PLANTED_EMAIL}: {PLANTED_PROMPT}"
       );
       // `unwrap` puts the error's Debug, planted values included, in the panic message.
       let failed: Result<(), String> = Err(format!("{PLANTED_KEY} {PLANTED_EMAIL} {PLANTED_PROMPT}"));
       failed.unwrap();
   }
   ```
4. The parent, `#[tokio::test(flavor = "multi_thread", worker_threads = 2)] async fn a_real_panic_and_error_leak_nothing()`, steps:
   1. `let dir = tempfile::tempdir().unwrap(); let spool_dir = dir.path().join("spool");`
   2. Run the child with `std::process::Command::new(std::env::current_exe().unwrap())`, args `--exact observability::crash::leak_tests::crash_child_process --ignored --nocapture --test-threads=1`, `.env(CHILD_SPOOL_VAR, &spool_dir)`, `.env("RUST_BACKTRACE", "0")`, `.stdout(Stdio::null())`, then `.output()` (the shape of `crates/storage/src/crash_tests.rs:spawn_child`, without its abort loop: the child ends by itself).
   3. Assert the child failed (`!output.status.success()`), and that its stderr contains `PLANTED_KEY`: Rust's default hook printed the message, so the planted value really was in the panic.
   4. `Spool::new(spool_dir.clone()).list()` has exactly two files. For each, read the text, `assert_clean(&text)`, and parse it as `CrashReport`. One is `ErrorEvent` with `fields == ["key", "user", "input"]`; one is `Panic` with a location starting with `crates/server/src/observability/crash/leak_tests.rs:`, empty `fields`, and a backtrace whose **first** frame contains `crash_child_process` (`core::result::unwrap_failed` and the hook chain, `crash::report_panic` included, are dropped).
   5. `let receiver = crate::observability::test_receiver::Receiver::start("/v1/crash", 204, Duration::ZERO).await;`, then `send_spool(&heartbeat::client(heartbeat::REQUEST_TIMEOUT).unwrap(), &crash_url(&receiver.base), &Spool::new(spool_dir), Some(Uuid::parse_str("8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d").unwrap()), now_ns).await`, where `now_ns` is the current system time computed in the test (`SystemTime::now().duration_since(UNIX_EPOCH)` as `i64` nanoseconds; the child wrote its files with the real clock). Assert `summary.sent == 2` and `receiver.bodies().len() == 2` before checking that both bodies pass `assert_clean`.
5. In-process tests (no global state: a `Dispatch` per test with `tracing::dispatcher::with_default`, a `Reporter::new` whose clock returns `FIXED_NOW_NS`, `Receiver::start("/v1/crash", 204, Duration::ZERO)`, and `send_spool(…, FIXED_NOW_NS)`; every test that sends asserts `summary.sent` and `receiver.bodies().len()` equal the number of spooled files, and that number is above zero, before checking any body):
   - `event_values_reach_neither_spool_nor_request`: an `error!` with the planted values in fields and in the message, then `send_spool` to the receiver → one file, `sent == 1`, one body; the spool file text (read before sending) and the body both pass `assert_clean`.
   - `debug_and_display_field_values_stay_out`: `error!(error = %PLANTED_EMAIL, detail = ?PLANTED_PROMPT, key = PLANTED_KEY)` → the spool file passes `assert_clean`; `fields == ["error", "detail", "key"]`.
   - `a_home_directory_never_reaches_a_report` (no send): `reporter.report_event` with `EventSite`s whose `file` is `/home/planted-user/work/main.rs` and `/home/planted-user/.cargo/registry/src/index.crates.io-x/arrow-55.0.0/src/array.rs` → every spool file passes `assert_clean`; the locations are `main.rs:1` and `arrow-55.0.0/src/array.rs:2`.
   - `a_panic_report_has_no_message_input` (no send): `reporter.report_panic(Some(Location::caller()))` inside a function whose local variable holds `PLANTED_KEY` (unused by the call) → the file passes `assert_clean`. (The compile-time guarantee is `panic_report`'s signature; this test pins it.)

**Tests.** The ones above.

**Done when** (besides the common rule): a comment above the parent states that it re-runs the test binary, so under `cargo test` and nextest alike the child gets its own process and its own global subscriber and hook.

**Commit:** `Add crash report leak tests`

---

### TL5.10 — Docs, spec 12, CHANGELOG and the requests to Q

**Goal.** The Telemetry page documents crash reports field by field, the backup page and spec 12 say the spool is not backed up, the CHANGELOG lists the feature, and track Q gets the crash body.

**Edit:**
- `website/src/content/docs/docs/self-hosting/telemetry.md`
- `website/src/content/docs/docs/self-hosting/backup.md`
- `CHANGELOG.md`
- `docs/spec/12-operations.md`
- `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/tl-requests-for-q.md` (outside the repo, not committed)

Every edit below replaces or follows a text M4 (TL4.9, TL4.10) wrote. If a quoted text is missing because M6 or M3 changed it first, keep their change and add M5's text next to it; if that is not obvious, stop and report.

**Steps.**
1. `telemetry.md`:
   1. After the paragraph that starts `cetra sends one anonymous heartbeat a day`, add the paragraph: `cetra can also send crash reports. They are off unless you turn them on: see [Crash reports](#crash-reports).`
   2. After the `## Turn it off` table's following paragraph (the one starting `Any other value of \`CETRA_TELEMETRY\``), add: `` `CETRA_TELEMETRY=off` and `DO_NOT_TRACK` turn crash reports off too. ``
   3. In `## The startup line`, replace the paragraph with:
      ```markdown
      At every start, cetra logs two lines, at `info` with the target `cetra_server::observability::notice`: one says whether the heartbeat is on, the other whether crash reports are on. They show even when `log.level` is `warn`. When the heartbeat is on, its line gives the instance id and a link to this page. When something is off, the line's `reason` field says why: `CETRA_TELEMETRY`, `DO_NOT_TRACK`, `restore_mode`, `ci`, `debug_build` or `config` (crash reports are never off for `ci` or `debug_build`).
      ```
   4. In the `## What it sends` table, add after the `features.self_monitoring` row:
      ```markdown
      | `features.crash_reports_on` | `true`, `false` | Crash reports are on. |
      ```
      and in the example JSON add a trailing comma to the line `"self_monitoring": false` (inside `features`; keep its indentation, four spaces in the committed page) and add after it the line `"crash_reports_on": false` with the same indentation. If M6 added keys after `self_monitoring`, add `crash_reports_on` after the last `features` key instead.
   5. In `## The instance id`, replace `the first time it sends a heartbeat` with `the first time it sends a heartbeat or a crash report`, and `To have us delete the heartbeats of an instance` with `To have us delete the heartbeats and crash reports of an instance`.
   6. Append at the end of the page:
      ````markdown
      ## Crash reports

      Crash reports are off unless you turn them on with `telemetry.crash_reports = true` (or `CETRA_TELEMETRY__CRASH_REPORTS=true`). Then cetra sends us a report for every panic and every error-level log event, so we can fix bugs you have not reported. A report names the place in the code where the problem happened. It never contains the error message, a value, or anything from your data.

      `CETRA_TELEMETRY=off`, `DO_NOT_TRACK` and restore mode turn crash reports off whatever the setting says. CI environments and debug builds do not, since crash reports only run when you ask for them.

      ### What a report contains

      | Field | Example | Meaning |
      |---|---|---|
      | `schema` | `1` | The version of this format. |
      | `instance_id` | a random UUID | The [instance id](#the-instance-id), added when the report is sent. |
      | `version`, `target`, `profile`, `deployment` | `0.1.0`, `x86_64-unknown-linux-musl`, `release`, `docker` | As in the heartbeat. |
      | `uptime` | `<1h`, `1h-1d`, `1d-7d`, `7d-30d`, `30d+` | How long the server process had run. |
      | `kind` | `panic`, `error_event` | A panic, or an error-level log event. |
      | `location` | `crates/storage/src/wal.rs:412:9` | The file and line in cetra's code (a panic also has a column). A file of a dependency keeps only the part after Cargo's download directory (`arrow-55.0.0/src/array.rs`); any other file keeps only its name. |
      | `event_target`, `module` | `cetra_storage::wal` | The log event's target and module. Absent for a panic. |
      | `fields` | `["error", "project_id"]` | The names of the log event's fields, never their values. Empty for a panic. |
      | `backtrace` | `["cetra_storage::wal::Wal::sync", …]` | The names of the functions on the stack, at most 48, without file or line. The frames of Rust's own panic machinery above cetra's code are left out. |
      | `count` | `3` | How many times this happened. |
      | `first_hour`, `last_hour` | `490896` | The hours (since 1970, UTC) of the first and last time, never the exact time. |

      An example:

      ```json
      {
        "schema": 1,
        "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
        "version": "0.1.0",
        "target": "x86_64-unknown-linux-musl",
        "profile": "release",
        "deployment": "docker",
        "uptime": "1h-1d",
        "kind": "error_event",
        "location": "crates/storage/src/wal.rs:412",
        "event_target": "cetra_storage::wal",
        "module": "cetra_storage::wal",
        "fields": ["error", "project_id"],
        "backtrace": ["cetra_storage::wal::Wal::sync", "cetra_storage::flush::run"],
        "count": 3,
        "first_hour": 490896,
        "last_hour": 490897
      }
      ```

      A report never contains the panic or error message, field values, error texts, thread names, host names, file paths outside cetra's code and its dependencies, environment variables, configuration, or exact times. Rust panic messages can quote data (an `unwrap` prints the error it found), so cetra drops the message whole rather than trying to clean it.

      ### How reports are sent

      - The same place in the code is reported at most once an hour; later occurrences are counted and sent as one report after the hour. At most 20 reports go out an hour.
      - cetra writes each report to `<data_dir>/telemetry/spool/` the moment it happens, so a panic that stops the server is sent at the next start. The spool holds at most 50 reports; the oldest go first.
      - Every minute, cetra sends the spooled reports to `https://telemetry.cetra.sh/v1/crash` (or [`telemetry.endpoint`](/docs/self-hosting/configuration-reference/#telemetry)`/v1/crash`) and deletes each one the relay accepts or refuses. A report that cannot be sent (the relay is down, busy or rate-limiting) waits for the next try and is deleted unsent after 7 days. Requests follow no redirect and give up after 10 seconds; failures are logged at `debug` only.
      - Writing a report takes a moment the first time in a process, while cetra reads its own function names. Error-level events of libraries that log through the `log` crate are not reported: they carry no code location.
- Turning crash reports off deletes the spool at the next start. Backups do not include it.
      ````
2. `backup.md`: replace `` `tmp/` (query spill files) and `LOCK` can be left out. `` with `` `tmp/` (query spill files), `LOCK` and `telemetry/` (unsent [crash reports](/docs/self-hosting/telemetry/#crash-reports)) can be left out. `` If the sentence differs, stop and report.
3. `CHANGELOG.md`, append to `## [Unreleased]` → `### Added`:
   ```markdown
   - Opt-in crash reports (`telemetry.crash_reports = true`): each panic and error event is sent to
     `telemetry.cetra.sh` as a code location, callsite field names and function names, never a
     message, a value or an exact time. Reports are spooled in `<data_dir>/telemetry/spool/`, sent
     every minute and turned off by `CETRA_TELEMETRY=off`, `DO_NOT_TRACK` or restore mode. The
     heartbeat gains `features.crash_reports_on`.
   ```
4. `docs/spec/12-operations.md`:
   1. Add a row right after the `telemetry.heartbeat` row:
      ```markdown
      | `telemetry.crash_reports` | | `false`. Opt-in crash reports to `<endpoint>/v1/crash` (D-TL-5); rules 1–3 of the switchboard can turn them off |
      ```
   2. § Process lifecycle, Startup bullet: replace `decide what may go to us, log the telemetry notice, and start the heartbeat task when the switchboard allows it (D-TL-11)` with `decide what may go to us, log the two telemetry notices, start the heartbeat task and the crash-report sender when the switchboard allows them, and delete the crash spool when crash reports are off (D-TL-11, D-TL-12)`.
   3. § Backups, the cold snapshot sentence: replace `` `tmp/` and `LOCK` may be skipped `` with `` `tmp/`, `LOCK` and `telemetry/` (the crash-report spool) may be skipped ``. In the online-backup paragraph, after `copies \`secret.key\`, and writes \`manifest.json\` last.`, add ` It never copies \`telemetry/\`.`
   4. In `### Telemetry to us (track TL M4)`, after the switchboard paragraph's last sentence (`Licences never change it (D172).`), add: ` Crash reports follow rules 1–3, then \`telemetry.crash_reports\`; rule 4 does not apply to them.` In the startup-notice paragraph, replace `one \`info\` line` with `one \`info\` line for the heartbeat and one for crash reports`.
   5. Append after that section:
      ```markdown
      ### Crash reports (track TL M5)

      Opt-in (`telemetry.crash_reports`, D-TL-5). Every panic (from the panic hook, before its `panic` event) and every ERROR `tracing` event with a source file (from the crash layer, which skips the hook's own `panic` event and events bridged from the `log` crate, and has its own filter, so `log.level` does not hide errors from it) becomes a report: `schema` 1, `version`, `target`, `profile`, `deployment`, `uptime` bucket, `kind` (`panic`, `error_event`), `location` (`path:line[:column]`; a `crates/…` path is kept, a path under `/.cargo/registry/src/<index>/` or `/.cargo/git/checkouts/` keeps what follows, anything else, or a path with `..`, keeps its file name), `event_target` and `module` (events only), `fields` (callsite field names, `message` excluded), `backtrace` (demangled function names from the `backtrace` crate, hash suffix removed, the leading reporter and runtime frames dropped, at most 48), `count`, `first_hour`, `last_hour` (Unix hours). Never a message, a field value, error text, thread or host names, other paths, environment, config or exact times. Dedup per kind and location per clock hour (the first occurrence spooled at once, later ones counted and spooled after the hour), at most 20 reports an hour. Reports are written synchronously to `<data_dir>/telemetry/spool/` (temporary file then rename, at most 50 files of at most 63 KiB, oldest dropped; only `telemetry/` and `spool/` are created, never the data directory; no logging and no panic on that path, D-TL-12; a stuck disk delays the writing thread). A sender task posts them to `<endpoint>/v1/crash` at startup and every minute with `instance_id` added, deletes a report on 2xx or on a 4xx other than 408 and 429, keeps it otherwise (the pass stops), and drops reports older than 7 days; no redirects, 10 s timeout, failures at `debug`. When crash reports are off, nothing is spooled and the spool is deleted at startup. Backups do not copy the spool.
      ```
5. The Q notes file: if it does not exist, stop and report (M4's TL4.10 writes it). In § TL-Q1, after the table of exact strings (`| Field | Values |`, which ends with the `features.*` row), add:
   ````markdown
   - The crash body of schema 1 (from M5); `instance_id`, `event_target` and `module` may be absent:
     ```json
     {"schema":1,"instance_id":"8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d","version":"0.1.0","target":"x86_64-unknown-linux-musl","profile":"release","deployment":"docker","uptime":"1h-1d","kind":"error_event","location":"crates/storage/src/wal.rs:412","event_target":"cetra_storage::wal","module":"cetra_storage::wal","fields":["error","project_id"],"backtrace":["cetra_storage::wal::Wal::sync","cetra_storage::flush::run"],"count":3,"first_hour":490896,"last_hour":490897}
     ```
     `kind` is `panic` or `error_event`; `fields` has at most 32 strings and `backtrace` at most 48; `count` ≥ 1; `first_hour` ≤ `last_hour`, Unix hours; `profile`, `deployment` and `uptime` take the heartbeat's strings. Bodies stay under 64 KiB. For Sentry, group by `kind` and `location`; there is no message to show. A report may arrive up to 7 days late (it waits in the server's spool), and a relay answer of 4xx deletes it on the server, except 408 and 429, which keep it like a 5xx: answer 429 when rate-limiting a report you still want.
   ````

**Tests.** By hand: `telemetry.md` has no `Cetra`/`CETRA` word outside code, no `simply`, `powerful`, `seamless`; every `CETRA_*` name used is `CETRA_TELEMETRY` or a documented key's form. The coordinator's `just check-docs` runs the term, env-var and link checks (`#crash-reports` and `#the-instance-id` exist on the page; `#telemetry` exists in the configuration reference).

**Commit:** `Document opt-in crash reports` (the Q notes file is not committed).

---

## After the last phase (coordinator)

- Review each phase's diff against this plan, then run `just fmt` and `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` once, then `just deny` (it should warn about two `object` versions and pass).
- Check by hand once (crash reports run in debug builds too): `CETRA_TELEMETRY__CRASH_REPORTS=true CETRA_TELEMETRY__ENDPOINT=http://127.0.0.1:9 just run serve` prints the crash-reports notice; a spool file appears after an ERROR event, and its backtrace has function names.
- A Sonnet code review of the milestone; fixes by a Haiku agent with an exact brief.
- If M2, M3 or M6 lands after M5 and builds an `Observability`, `Decision`, `Inputs` or `Features` literal, or edits the heartbeat golden JSON, it must add M5's `crash`, `crash_reports` or `crash_reports_on` (M3's plan has an `Observability { logs: None }` test literal).
- Update the TL row of `docs/roadmap.md` when the track's status changes. Hand the Q notes file to track Q.
- Tell M3 that `Decision.crash_reports` exists, if its debug-bundle telemetry state should show it.
