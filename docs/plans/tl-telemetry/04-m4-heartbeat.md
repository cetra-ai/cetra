# TL M4 — The switchboard and the heartbeat

**Summary.** This milestone adds what a self-hosted cetra sends to us by default: an anonymous daily heartbeat. It brings the `[telemetry]` table (`heartbeat`, `endpoint`), the switchboard that decides at startup what may go to us (`CETRA_TELEMETRY`, `DO_NOT_TRACK`, restore mode, CI and debug builds, then the config; fail closed), the shared build and host facts (with the process uptime), the buckets, the instance id in `instance_settings`, the heartbeat payload and the queries that fill it, its schedule and HTTP client, and the startup notice that the stdout filter always lets through. It sets `CETRA_TELEMETRY=off` in every script that starts a server for a test, and documents everything: a Telemetry page, the configuration reference, the marketing and docs claims that become false, the README, the CHANGELOG with an upgrade note, specs 04 and 12, D-W-18, and the requests to track Q. Crash reports (M5), the debug-bundle preview (M3) and product-analytics counts (M6) build on these types later.

**Prerequisites.**
- Work in `/home/sebastien/travaux/llm-observer/.claude/worktrees/cross-track-review-6a41ad`, branch `claude/telemetry-9e2a24`. M1 (self-monitoring, `docs/plans/tl-telemetry/01-m1-self-monitoring-logs.md`) runs in parallel in the same checkout. Its phases named here, by their IDs in that plan:
  - **TL1.2** (the `[self_monitoring]` configuration) is committed before **TL4.3** starts. TL1.2 adds `self_monitoring: SelfMonitoringConfig::default()` to the three `ServerConfig { … }` literals (`test_support/mod.rs` twice, `ingest/fixtures_tests.rs` once), makes `load_from` `pub(crate)`, and adds `pub(crate) fn parse_otlp_endpoint`. TL4.3 adds only the `telemetry` field to those literals.
  - **TL1.3** (creates `crates/server/src/observability/mod.rs` and `pub mod observability;` in `lib.rs`) is committed before **TL4.1** starts. M4 never edits `lib.rs`.
  - **TL1.5** (`crates/server/src/observability/subscriber.rs`: `capped_filter`, `log_filter` (stdout), `export_filter`, `build`) is committed before **TL4.7** starts.
  - **TL1.7** (`monitoring.md`, `backup.md` § Restore mode, `configuration.md`'s `restore_mode` row, `CHANGELOG.md`, spec 12) is committed before **TL4.9** and **TL4.10** start: both append to restore-mode text TL1.7 writes, and never rewrite it.
  - **TL1.1** (root `Cargo.toml`) is committed before **TL4.4** starts: `git commit -- Cargo.toml` commits the whole file, so the two never run at the same time.
  - TL1.4 and TL1.5 also add lines to `observability/mod.rs`: never at the same time as TL4.1, TL4.2, TL4.4 or TL4.5.
- If a name of M1's that this plan uses (listed in each phase) differs in the committed code, stop and report: M1's plan may still change under review.

**Read first.**
- `AGENTS.md`, all of § Rules, § Tests, § Commits and reports.
- `docs/plans/tl-telemetry/README.md`: § Configuration, § 5 Heartbeat, § The switchboard (D-TL-11), § Instance id (D-TL-9), § Licence separation, § Details settled while planning, the crash-report table's `version`, `target`, `profile`, `deployment`, `uptime` rows, § Requests to track Q, § Milestones (M4, M3, M5, M6), § Answers.
- `docs/plans/tl-telemetry/01-m1-self-monitoring-logs.md`: § Interface for later milestones (the names M4 uses: `SelfMonitoringConfig::exports`, `parse_otlp_endpoint`, `load_from`, `log_filter`/`capped_filter`/`export_filter`, `build`), TL1.2 step 9 (the three `ServerConfig` literals), TL1.3 (`observability/mod.rs`), TL1.7 step 3.2 (the spec 12 sentence).
- `docs/mvp-decisions.md` § Track TL (D-TL-1, D-TL-8 to D-TL-11), D172, D-N-25, D166, D-W-18.
- `crates/server/src/config.rs`: `ServerConfig` (lines 664–799), `ConfigError` (801–833), `load_from` (875–897), `validate` (1027–1127), `loopback_host` (around 1368), `EnvSource`, `split_key`, `is_filtered`, `alias_key`, `env_value`, `is_secret_path` (1825–1939), and the test `the_configuration_reference_matches_the_loader` (3579–3757).
- `crates/server/src/serve.rs`: `bind` (281–701), the `spawn_*` loops (742–921), `Running::shutdown` (946–1041), the tests `restore_mode_skips_the_sending_and_dns_loops` and `restore_mode_logs_at_startup` (1191–1224).
- `crates/server/src/meta/instance_settings.rs` (whole file), `crates/server/src/meta/mod.rs` (`MetaDb::write` 175–200, `MetaError` 236–312: it derives `Debug` and `thiserror::Error` only, no `PartialEq`), `crates/server/src/usage.rs` (`flushed_month_units`, how `day` is formatted).
- `crates/server/migrations/meta/0001_identity.sql` (`users`, `orgs`, `projects`), `0002_usage.sql`, `0005_alerts.sql` (`alert_rules`), `0006_evaluators.sql`, `0010_eval_queue.sql` (`online_rules`), `0017_datasets.sql`, `0023_prompts.sql`, `0025_annotation_queues.sql`.
- `crates/server/src/auth/oidc/mod.rs:186-198` (`identity_client`: the outbound client pattern), `crates/server/src/test_support/mock_webhook.rs` (an in-process receiver), `crates/server/src/test_support/mod.rs` (`TestApp`, `capture_logs` at 1609, `config` at 1814, `default_config` at 1257), `crates/server/src/routes/admin.rs:988-1000` (building an `SmtpUrl` in a test).
- `docs/spec/04-metadata-db.md:990-1000`, `docs/spec/12-operations.md` § Configuration, § Process lifecycle, § Self-monitoring.
- `website/STYLE.md`, `website/scripts/check-env-vars.mjs`, `website/scripts/check-terms.mjs`, `website/astro.config.mjs:143-182`, `website/src/content/docs/docs/self-hosting/{index.md,security.md,backup.md,configuration-reference.md}`, `website/src/marketing/home/DataOwnership.astro`, `website/src/pages/security.astro`.
- `tools/packaging/{binary,image,compose,helm}-smoke.sh`, `tools/e2e/run.sh`, `justfile` recipe `check-packaging-lint`, `deploy/compose/cetra.env.example`, `node-cli/src/local.ts` (`childEnvironment`: `cetra init`'s local server inherits the shell's environment).

## What it is, and what it is not

**What works when it's done:**
- `[telemetry]` with `heartbeat` (default `true`) and `endpoint` (default `"https://telemetry.cetra.sh"`; `https`, or `http` to a loopback IP address), both in the configuration reference.
- `CETRA_TELEMETRY` (`off`/`false`/`0`, any case) turns everything sent to us off; empty is ignored; any other value is a startup error. It is not mapped to a config key.
- `DO_NOT_TRACK`, restore mode, CI variables and debug builds turn the heartbeat off, in the design's order.
- A release build with nothing set sends one heartbeat 10–20 minutes after start, then every 24 h, at most one a day across restarts, to `<endpoint>/v1/heartbeat`, with a random instance id and bucketed facts only. Failures are logged at `debug`.
- One startup line at `info` under the target `cetra_server::observability::notice` says whether the heartbeat is on (with the instance id) or why it is off; it shows on stdout at `log.level = "warn"` and is never exported over OTLP by that rule.
- No script that starts a server for a test can send a heartbeat (`CETRA_TELEMETRY=off`, checked by `just check-packaging-lint`).
- A Telemetry docs page lists every field and how to turn it off; the pages that said "cetra sends no usage data" say what is true.

**Out of scope:**
- Crash reports, `telemetry.crash_reports`, the crash line of the notice, `features.crash_reports_on` (M5).
- The heartbeat preview and telemetry state in `cetra debug-bundle` (M3, which reuses `heartbeat::collect` and adds a read-only instance-id getter).
- The `usage` field (M6).
- The relay at `telemetry.cetra.sh` (track Q, TL-Q1). Until it exists, every send fails silently at `debug`.
- The roadmap row (the coordinator updates it when the milestone lands). Handing the Q notes file to track Q (the coordinator does it; the file is outside the repo).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-TL-1 | `[telemetry]` is a config table (amends spec 12's "no telemetry key" and D-W-18's "`telemetry.*` refused") |
| D-TL-8 | Daily heartbeat on by default; bucketed counts and feature booleans; no names, emails, domains, hosts, exact counts or licence data |
| D-TL-9 | Instance id: random UUIDv4 in `instance_settings['telemetry.instance_id']`, insert keeping an existing row |
| D-TL-10 | Heartbeats go to `telemetry.cetra.sh`, a relay track Q runs |
| D-TL-11 | Switchboard: `CETRA_TELEMETRY`, `DO_NOT_TRACK`, restore mode, then CI and debug builds for the heartbeat, then the config; fail closed; licences never change it |
| Design § Details settled while planning | Bucket sets for `uptime` (time since the process started) and `install_age` (oldest `users.created_at`; none → `<1d`); counts of active orgs, active projects, users not disabled; three sign-in booleans from the server config only; `crash_reports_on` joins `features` in M5; `last_heartbeat_at` written only after a 2xx; `http` endpoints only to a loopback IP literal, `localhost` refused, test configs on `http://127.0.0.1:9`; the switchboard's environment read once by the config loader and kept beside the config; `cetra init`'s local server gets the heartbeat too |
| D172 | Licence verification stays offline; the heartbeat shares no code, data or endpoint with licences |
| D-N-25 | Restore mode runs no outbound work |
| D166 | The egress guard is for user-controlled URLs; `telemetry.endpoint` is operator config and does not go through it (like identity providers) |
| D-W-7, D-W-18 | The configuration reference is checked against the loader; D-W-18 gets an "amended by D-TL-1" note |
| D-N-5 | CHANGELOG `[Unreleased]`, `### Upgrade notes` first |

The user's answers this plan uses (design § Answers): 2 (the `features` list and `deployment` as written), 5 (milestones in parallel).

Implementation details within those decisions:
- **Target triple.** A build script in `cetra-server` exports Cargo's `TARGET` as the compile-time variable `BUILD_TARGET`; `facts::TARGET = env!("BUILD_TARGET")`. Composing it from `std::env::consts` and `cfg(target_env)` would guess the vendor part.
- **Profile.** `release` when `debug_assertions` is off, `debug` otherwise. "Debug build" in the switchboard is `cfg!(debug_assertions)`.
- **Deployment.** `kubernetes` when `KUBERNETES_SERVICE_HOST` is non-empty (checked first: a pod is also a container), else `docker` when `/.dockerenv` or `/run/.containerenv` exists, else `binary`.
- **Process start.** `facts::mark_process_start()` records an `Instant` in a `OnceLock` (first call wins); `serve::bind` calls it first, and M5 may call it earlier. `facts::process_uptime()` is the time since then (since the first call to either if none was made).
- **Env names.** `CETRA_TELEMETRY` matches without regard to case, like every `CETRA_` name of the loader (`is_filtered` drops `cetra_telemetry` too, so it must be read the same way). `DO_NOT_TRACK` and the CI names match exactly. Values are trimmed before they are compared.
- **Where the switchboard environment lives.** `TelemetryConfig.env`, a `#[serde(skip)]` field filled by `load_from` from the same environment slice it reads; never a key.
- **Endpoint path.** `telemetry.endpoint` may have a path (a relay or egress proxy); it is stored without a trailing slash and `/v1/heartbeat` is appended.
- **Not `loopback_host`.** `config.rs`'s `loopback_host` (OIDC issuers) accepts the name `localhost` and only `127.0.0.1`/`::1`. The design refuses `localhost` for `telemetry.endpoint` and accepts all of `127.0.0.0/8`, so `validate_telemetry` checks `url::Host` itself.

## Versions

No new crate. One feature on an existing dependency:

| Crate | Version | Change | Where | Why |
|---|---|---|---|---|
| `uuid` | 1.29.2 in `Cargo.lock` (`cargo info uuid@1.29.2`: latest 1.29.3) | add feature `v4` to the existing `["v7", "serde"]` | root `Cargo.toml` `[workspace.dependencies]` | `Uuid::new_v4()` for the instance id (D-TL-9). `v4 = ["rng"]`, the same as the `v7` already on, so `Cargo.lock` does not change. |

The implementer of TL4.4 re-runs `cargo info uuid@1.29.2` and reports the version and the `v4` feature line.

`rand` 0.10.3 (already a dependency of `cetra-server`, default features, so `thread_rng` is on) gives `rand::random_range` for the jitter. `reqwest` 0.12.28 (features `rustls-tls`, `json`) is already a dependency.

## Phase graph

```
TL1.3 ─► TL4.1 facts+buckets ─► TL4.2 switchboard ─► TL4.3 config ─► TL4.4 instance id ─► TL4.5 payload ─► TL4.6 sender+serve ─► TL4.7 notice filter
TL1.2 ──────────────────────────────────────────────┘                                                                            TL1.5 ─┘
TL4.8 packaging            (any time)
TL1.7 ─► TL4.9 docs and marketing
TL1.7 ─► TL4.10 README, CHANGELOG, specs, D-W-18, Q notes
```

TL4.1–TL4.7 run one after the other: they share `observability/mod.rs` and build on each other. TL4.8, TL4.9 and TL4.10 touch no Rust and may run in parallel with them and with each other; TL4.9 and TL4.10 once TL1.7 is committed. The docs links (`#telemetry`) and env-var checks only pass once TL4.2 and TL4.3 are in, which the final `just check` sees.

| Phase | Files | Shared with M1 (the coordinator serializes) |
|---|---|---|
| TL4.1 | `crates/server/build.rs` (new), `observability/facts.rs` (new), `observability/buckets.rs` (new), `observability/mod.rs` | `observability/mod.rs` (TL1.3–TL1.5) |
| TL4.2 | `observability/switchboard.rs` (new), `observability/mod.rs` | `observability/mod.rs` |
| TL4.3 | `config.rs`, `observability/switchboard.rs`, `test_support/mod.rs`, `ingest/fixtures_tests.rs`, `website/…/configuration-reference.md` | all but `switchboard.rs`: after TL1.2 |
| TL4.4 | `Cargo.toml`, `meta/instance_settings.rs`, `observability/instance.rs` (new), `observability/mod.rs` | `Cargo.toml` (TL1.1), `observability/mod.rs` |
| TL4.5 | `observability/heartbeat.rs` (new), `observability/mod.rs` | `observability/mod.rs` |
| TL4.6 | `observability/heartbeat.rs`, `serve.rs` | `serve.rs` (M1 does not plan to edit it) |
| TL4.7 | `observability/subscriber.rs` (M1's) | after TL1.5 |
| TL4.8 | `tools/packaging/{binary,image,compose,helm}-smoke.sh`, `tools/packaging/telemetry-off-check.sh` (new), `tools/e2e/run.sh`, `justfile`, `deploy/compose/cetra.env.example` | none |
| TL4.9 | `website/…/self-hosting/telemetry.md` (new), `website/astro.config.mjs`, `website/…/self-hosting/{security.md,index.md,backup.md,configuration.md}`, `website/src/marketing/home/DataOwnership.astro`, `website/src/pages/security.astro` | `backup.md`, `configuration.md`: after TL1.7 |
| TL4.10 | `README.md`, `CHANGELOG.md`, `docs/spec/04-metadata-db.md`, `docs/spec/12-operations.md`, `docs/mvp-decisions.md`, the Q notes file (outside the repo) | `CHANGELOG.md`, spec 12, `mvp-decisions.md`: after TL1.7 |

(`observability/…` is `crates/server/src/observability/…`; `website/…` is `website/src/content/docs/docs`.)

## Conventions for every phase

- The implementer is Haiku. Follow the plan exactly. If a step is unclear, contradicts the code, or cannot be done as written, stop and report the question; do not improvise.
- Edit only the files in the phase's **Edit** list. Report any change needed elsewhere.
- **Do not run** `cargo build`, `cargo test`, `cargo clippy`, `cargo nextest` or any `just` recipe (the machine is loaded; the coordinator runs the checks once at the end of the milestone). Read-only commands (`git diff`, `grep`, `cargo info`, `bash -n`) are fine.
- Formatting: run `rustfmt --edition 2024 <file>` on the **leaf** Rust files you created or changed (`facts.rs`, `buckets.rs`, `switchboard.rs`, `instance.rs`, `heartbeat.rs`, `config.rs`, `instance_settings.rs`, `serve.rs`, `subscriber.rs`, `test_support/mod.rs`, `fixtures_tests.rs`, `build.rs`), never on `lib.rs`, `observability/mod.rs` or `main.rs`: rustfmt also formats every module such a file declares, which may be another agent's half-written file. The coordinator's `just fmt` covers the rest.
- **Done when** for every phase: the listed files changed as specified, nothing else changed, and one commit with the phase's subject exists.
- Commit only your phase's files: `git add <files> && git commit -m "<subject>" -- <files>` (the `git add` is needed for new files). Other agents commit in the same checkout: if `.git/index.lock` exists, wait a few seconds and retry. Never `git stash`, never `git add -A`.
- The milestone's final check, run by the coordinator: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just fmt && just check`.
- Rust rules (AGENTS.md): no `unwrap()`/`expect()` outside tests unless a comment states the invariant; no `unsafe`; no panic on input; `thiserror` errors; logs and errors never carry payload, emails, names, URLs with secrets or config values (ids and sizes only).
- Doc comments in the new modules spell the noun "licence" (British), never the identifier spelling: TL4.5's test scans the non-test code for the identifier spelling.
- In every new module, all non-test items come before the single `#[cfg(test)] mod tests` at the end of the file (TL4.5's licence test cuts each file at its first `#[cfg(test)]`).
- New Rust items are `pub(crate)` (M1 makes `observability` a `pub` module; `pub(crate)` keeps them out of the crate's API). Every item has a doc comment. The workspace lint `unused_qualifications` is on: import a name once rather than writing its full path where an import already exists.
- Tests compare `MetaError` and `ConfigError` values with `matches!` (neither derives `PartialEq`).

---

### TL4.1 — Build facts and buckets

**Goal.** The shared facts (version, target triple, profile, deployment kind, process uptime) and the bucket functions, as pure code with tests.

**Edit:**
- `crates/server/build.rs` (new)
- `crates/server/src/observability/facts.rs` (new)
- `crates/server/src/observability/buckets.rs` (new)
- `crates/server/src/observability/mod.rs` (two `mod` lines only)

**Steps.**
1. Create `crates/server/build.rs` with exactly:
   ```rust
   //! Exports the target triple the crate is built for as `BUILD_TARGET`, for the heartbeat and
   //! crash reports (track TL, `observability::facts::TARGET`). A build script reports to Cargo on
   //! stdout, so the workspace's print lint does not apply here.
   #![allow(clippy::print_stdout)]

   fn main() {
       println!("cargo:rerun-if-changed=build.rs");
       // Cargo always sets TARGET for build scripts; "unknown" keeps a hand-run build compiling.
       let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
       println!("cargo:rustc-env=BUILD_TARGET={target}");
   }
   ```
   Cargo finds `build.rs` next to `Cargo.toml` by itself: do not edit `crates/server/Cargo.toml`.
2. Create `crates/server/src/observability/facts.rs`. Module doc: `//! Build and host facts shared by the heartbeat, crash reports and the debug bundle (track TL). Every value is fixed by the build or read from a fixed probe; none comes from user data.` Contents:
   ```rust
   use std::path::Path;
   use std::sync::OnceLock;
   use std::time::{Duration, Instant};

   use serde::Serialize;

   /// When the process started, as far as this crate knows (see [`mark_process_start`]).
   static PROCESS_START: OnceLock<Instant> = OnceLock::new();

   /// Records now as the process start. The first call wins; later calls change nothing.
   /// `serve::bind` calls it first thing; M5 may call it earlier.
   pub(crate) fn mark_process_start() {
       let _ = PROCESS_START.get_or_init(Instant::now);
   }

   /// Time since [`mark_process_start`] (or since this first call, if it never ran). The
   /// `uptime` of the heartbeat and of crash reports.
   pub(crate) fn process_uptime() -> Duration {
       PROCESS_START.get_or_init(Instant::now).elapsed()
   }

   /// The workspace version, e.g. `0.1.0`.
   pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

   /// The target triple of this build, e.g. `x86_64-unknown-linux-musl` (`build.rs`).
   pub(crate) const TARGET: &str = env!("BUILD_TARGET");

   /// The build profile, by `debug_assertions`.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   #[serde(rename_all = "lowercase")]
   pub(crate) enum Profile {
       /// `debug_assertions` off.
       Release,
       /// `debug_assertions` on.
       Debug,
   }

   /// This build's profile.
   pub(crate) const fn profile() -> Profile {
       if cfg!(debug_assertions) { Profile::Debug } else { Profile::Release }
   }

   /// How the process runs.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   #[serde(rename_all = "lowercase")]
   pub(crate) enum Deployment {
       /// A container (Docker, Podman, Compose).
       Docker,
       /// A Kubernetes pod.
       Kubernetes,
       /// Anything else: the binary on a host.
       Binary,
   }

   /// What [`deployment`] looks at, read once by [`probe_host`].
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
   pub(crate) struct HostProbe {
       /// `/.dockerenv` exists (Docker).
       pub(crate) dockerenv: bool,
       /// `/run/.containerenv` exists (Podman).
       pub(crate) containerenv: bool,
       /// `KUBERNETES_SERVICE_HOST` is set and non-empty.
       pub(crate) kubernetes: bool,
   }

   /// Reads the probe files and variable of [`HostProbe`].
   pub(crate) fn probe_host() -> HostProbe {
       HostProbe {
           dockerenv: Path::new("/.dockerenv").exists(),
           containerenv: Path::new("/run/.containerenv").exists(),
           kubernetes: std::env::var_os("KUBERNETES_SERVICE_HOST").is_some_and(|value| !value.is_empty()),
       }
   }

   /// Kubernetes first (a pod is also a container), then a container, else the binary.
   pub(crate) fn deployment(probe: HostProbe) -> Deployment {
       if probe.kubernetes {
           Deployment::Kubernetes
       } else if probe.dockerenv || probe.containerenv {
           Deployment::Docker
       } else {
           Deployment::Binary
       }
   }
   ```
3. Create `crates/server/src/observability/buckets.rs`. Module doc: `//! Buckets for the counts and durations that leave the process (D-TL-8): never an exact count. The serde strings are the wire format; changing one changes the heartbeat.` Contents in full:
   ```rust
   use std::time::Duration;

   use serde::Serialize;

   /// A count of orgs, projects or users.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   pub(crate) enum CountBucket {
       #[serde(rename = "0")]
       Zero,
       #[serde(rename = "1")]
       One,
       #[serde(rename = "2-5")]
       TwoToFive,
       #[serde(rename = "6-20")]
       SixToTwenty,
       #[serde(rename = "21-100")]
       TwentyOneToHundred,
       #[serde(rename = "101-1000")]
       HundredOneToThousand,
       /// More than 1000.
       #[serde(rename = "1000+")]
       OverThousand,
   }

   /// `n` as a [`CountBucket`].
   pub(crate) fn count_bucket(n: u64) -> CountBucket {
       match n {
           0 => CountBucket::Zero,
           1 => CountBucket::One,
           2..=5 => CountBucket::TwoToFive,
           6..=20 => CountBucket::SixToTwenty,
           21..=100 => CountBucket::TwentyOneToHundred,
           101..=1000 => CountBucket::HundredOneToThousand,
           _ => CountBucket::OverThousand,
       }
   }

   /// A daily volume of spans or scores.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   pub(crate) enum VolumeBucket {
       #[serde(rename = "0")]
       Zero,
       #[serde(rename = "<1k")]
       UnderThousand,
       #[serde(rename = "<10k")]
       UnderTenThousand,
       #[serde(rename = "<100k")]
       UnderHundredThousand,
       #[serde(rename = "<1M")]
       UnderMillion,
       #[serde(rename = "<10M")]
       UnderTenMillion,
       #[serde(rename = "<100M")]
       UnderHundredMillion,
       #[serde(rename = "100M+")]
       HundredMillionOrMore,
   }

   /// `n` as a [`VolumeBucket`].
   pub(crate) fn volume_bucket(n: u64) -> VolumeBucket {
       match n {
           0 => VolumeBucket::Zero,
           1..=999 => VolumeBucket::UnderThousand,
           1_000..=9_999 => VolumeBucket::UnderTenThousand,
           10_000..=99_999 => VolumeBucket::UnderHundredThousand,
           100_000..=999_999 => VolumeBucket::UnderMillion,
           1_000_000..=9_999_999 => VolumeBucket::UnderTenMillion,
           10_000_000..=99_999_999 => VolumeBucket::UnderHundredMillion,
           _ => VolumeBucket::HundredMillionOrMore,
       }
   }

   const HOUR: u64 = 3_600;
   const DAY: u64 = 24 * HOUR;

   /// How long the process has run.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   pub(crate) enum UptimeBucket {
       #[serde(rename = "<1h")]
       UnderHour,
       #[serde(rename = "1h-1d")]
       HourToDay,
       #[serde(rename = "1d-7d")]
       DayToWeek,
       #[serde(rename = "7d-30d")]
       WeekToMonth,
       #[serde(rename = "30d+")]
       MonthOrMore,
   }

   /// `uptime` as an [`UptimeBucket`]. Each lower bound is inclusive.
   pub(crate) fn uptime_bucket(uptime: Duration) -> UptimeBucket {
       match uptime.as_secs() {
           s if s < HOUR => UptimeBucket::UnderHour,
           s if s < DAY => UptimeBucket::HourToDay,
           s if s < 7 * DAY => UptimeBucket::DayToWeek,
           s if s < 30 * DAY => UptimeBucket::WeekToMonth,
           _ => UptimeBucket::MonthOrMore,
       }
   }

   /// How long ago the instance was set up.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   pub(crate) enum InstallAgeBucket {
       #[serde(rename = "<1d")]
       UnderDay,
       #[serde(rename = "1d-7d")]
       DayToWeek,
       #[serde(rename = "7d-30d")]
       WeekToMonth,
       #[serde(rename = "30d-90d")]
       MonthToQuarter,
       #[serde(rename = "90d-1y")]
       QuarterToYear,
       #[serde(rename = "1y+")]
       YearOrMore,
   }

   /// `age` as an [`InstallAgeBucket`]. Each lower bound is inclusive; a year is 365 days.
   pub(crate) fn install_age_bucket(age: Duration) -> InstallAgeBucket {
       match age.as_secs() {
           s if s < DAY => InstallAgeBucket::UnderDay,
           s if s < 7 * DAY => InstallAgeBucket::DayToWeek,
           s if s < 30 * DAY => InstallAgeBucket::WeekToMonth,
           s if s < 90 * DAY => InstallAgeBucket::MonthToQuarter,
           s if s < 365 * DAY => InstallAgeBucket::QuarterToYear,
           _ => InstallAgeBucket::YearOrMore,
       }
   }
   ```
   The workspace has no `missing_docs` lint; the variants' serde strings document them.
4. In `crates/server/src/observability/mod.rs`, add (keeping M1's lines):
   ```rust
   pub(crate) mod buckets;
   pub(crate) mod facts;
   ```

**Tests** (in `#[cfg(test)] mod tests` of each file):
- facts: `process_uptime_grows_from_the_first_mark` (call `mark_process_start()`, read `a = process_uptime()`, sleep 20 ms, read `b`: `b > a`; a second `mark_process_start()` does not reset it: `process_uptime() >= b`); `the_version_is_the_package_version` (`VERSION == env!("CARGO_PKG_VERSION")`); `the_target_is_a_triple` (`TARGET` has at least two `-` and is not `"unknown"`); `a_test_build_is_a_debug_profile` (`profile() == Profile::Debug`, tests run with `debug_assertions`); `kubernetes_wins_over_a_container_file` (all three true → `Kubernetes`); `a_docker_file_is_docker`; `a_podman_file_is_docker`; `no_probe_is_binary`; `profile_and_deployment_serialize_lowercase` (`serde_json::to_string` gives `"release"`, `"debug"`, `"docker"`, `"kubernetes"`, `"binary"`).
- buckets: `count_bucket_edges` (0, 1, 2, 5, 6, 20, 21, 100, 101, 1000, 1001, `u64::MAX` → the expected variant); `volume_bucket_edges` (0, 1, 999, 1 000, 9 999, 10 000, 99 999, 100 000, 999 999, 1 000 000, 9 999 999, 10 000 000, 99 999 999, 100 000 000, `u64::MAX`); `uptime_bucket_edges` (0 s, 3 599 s, 3 600 s, 86 399 s, 86 400 s, 7 d − 1 s, 7 d, 30 d − 1 s, 30 d, `Duration::MAX`); `install_age_bucket_edges` (0 s, 1 d − 1 s, 1 d, 7 d, 30 d, 90 d − 1 s, 90 d, 365 d − 1 s, 365 d); `bucket_strings_are_the_wire_format` (serialize one value of every variant of the four enums and compare with the exact strings of step 3).

**Commit:** `Add build facts and buckets for telemetry`

---

### TL4.2 — The switchboard

**Goal.** One pure function decides, from an injected environment and three flags, whether the heartbeat may run and why not.

**Edit:**
- `crates/server/src/observability/switchboard.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)

**Steps.**
1. Create `switchboard.rs`. Module doc: `//! Who may send what to us (D-TL-11). [`decide`] runs once at startup over values captured by the config loader, so tests inject the environment without touching the process. The first matching rule wins; nothing here reads licence state, and licences never change the result (D172).` Contents, in full:
   ```rust
   /// The `tracing` target of the startup notice. The stdout filter always lets it through at
   /// `info` ([`NOTICE_DIRECTIVE`]), so the notice shows at `log.level = "warn"` too.
   pub(crate) const NOTICE_TARGET: &str = "cetra_server::observability::notice";

   /// The filter directive the stdout filter adds last, after the `rmcp` cap.
   pub(crate) const NOTICE_DIRECTIVE: &str = "cetra_server::observability::notice=info";

   /// Variables whose non-empty value marks a CI environment (besides `CI`, which is read
   /// with the `false`/`0` exception).
   const CI_VARIABLES: [&str; 13] = [
       "GITHUB_ACTIONS",
       "GITLAB_CI",
       "BUILDKITE",
       "CIRCLECI",
       "JENKINS_URL",
       "TF_BUILD",
       "TRAVIS",
       "TEAMCITY_VERSION",
       "BITBUCKET_BUILD_NUMBER",
       "CODEBUILD_BUILD_ID",
       "DRONE",
       "APPVEYOR",
       "SEMAPHORE",
   ];

   /// What the switchboard reads from the process environment.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
   pub(crate) struct TelemetryEnv {
       /// `CETRA_TELEMETRY` is `off`, `false` or `0` (any case).
       pub(crate) forced_off: bool,
       /// `DO_NOT_TRACK` is non-empty and not `0` or `false` (any case).
       pub(crate) do_not_track: bool,
       /// `CI` is non-empty and not `false` or `0`, or a [`CI_VARIABLES`] name is non-empty.
       pub(crate) ci: bool,
   }

   /// A `CETRA_TELEMETRY` value that is not `off`, `false`, `0` or empty. Carries no value.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
   pub(crate) enum TelemetryEnvError {
       /// The message names the accepted values, never the value given.
       #[error("CETRA_TELEMETRY: accepted values are off, false and 0 (any case)")]
       InvalidValue,
   }

   impl TelemetryEnv {
       /// Reads the switchboard's variables from `env`, the pairs the config loader reads.
       ///
       /// `CETRA_TELEMETRY` matches without regard to case, like every `CETRA_` name;
       /// `DO_NOT_TRACK` and the CI names match exactly. Values are trimmed first.
       ///
       /// # Errors
       ///
       /// [`TelemetryEnvError::InvalidValue`] when a `CETRA_TELEMETRY` value is not `off`,
       /// `false`, `0` or empty, so a typo never leaves telemetry on (`on` is refused too).
       pub(crate) fn from_pairs(env: &[(&str, &str)]) -> Result<Self, TelemetryEnvError> {
           let mut found = Self::default();
           for (name, raw) in env {
               let value = raw.trim();
               if name.eq_ignore_ascii_case("CETRA_TELEMETRY") {
                   if value.is_empty() {
                       continue;
                   }
                   if ["off", "false", "0"].iter().any(|off| value.eq_ignore_ascii_case(off)) {
                       found.forced_off = true;
                   } else {
                       return Err(TelemetryEnvError::InvalidValue);
                   }
               } else if *name == "DO_NOT_TRACK" {
                   found.do_not_track |= is_set_and_not_false(value);
               } else if *name == "CI" {
                   found.ci |= is_set_and_not_false(value);
               } else if CI_VARIABLES.contains(name) {
                   found.ci |= !value.is_empty();
               }
           }
           Ok(found)
       }
   }

   /// Non-empty and not `0` or `false` (any case).
   fn is_set_and_not_false(value: &str) -> bool {
       !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
   }

   /// Why something that may go to us is off.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum OffReason {
       /// Rule 1: `CETRA_TELEMETRY=off`.
       CetraTelemetry,
       /// Rule 2: `DO_NOT_TRACK`.
       DoNotTrack,
       /// Rule 3: `restore_mode = true` (D-N-25).
       RestoreMode,
       /// Rule 4, heartbeat only: a build with `debug_assertions`.
       DebugBuild,
       /// Rule 4, heartbeat only: a CI environment.
       Ci,
       /// Rule 5: the config turns it off.
       Config,
   }

   impl OffReason {
       /// The `reason` field of the startup notice.
       pub(crate) fn as_str(self) -> &'static str {
           match self {
               Self::CetraTelemetry => "CETRA_TELEMETRY",
               Self::DoNotTrack => "DO_NOT_TRACK",
               Self::RestoreMode => "restore_mode",
               Self::DebugBuild => "debug_build",
               Self::Ci => "ci",
               Self::Config => "config",
           }
       }
   }

   /// On, or off with the first rule that turned it off.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum Switch {
       /// May be sent.
       On,
       /// Must not be sent.
       Off(OffReason),
   }

   /// What may go to us. M5 adds `crash_reports`.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) struct Decision {
       /// The daily heartbeat (D-TL-8).
       pub(crate) heartbeat: Switch,
   }

   /// Everything [`decide`] looks at.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) struct Inputs {
       /// The captured environment.
       pub(crate) env: TelemetryEnv,
       /// `restore_mode`.
       pub(crate) restore_mode: bool,
       /// `cfg!(debug_assertions)` in production; tests pass either value.
       pub(crate) debug_build: bool,
       /// `telemetry.heartbeat`.
       pub(crate) heartbeat: bool,
   }

   /// The switchboard (D-TL-11). Total: every input gives a decision, so there is no error
   /// path that could leave something on.
   pub(crate) fn decide(inputs: &Inputs) -> Decision {
       let heartbeat = match shared_off(inputs) {
           Some(reason) => Switch::Off(reason),
           None if inputs.debug_build => Switch::Off(OffReason::DebugBuild),
           None if inputs.env.ci => Switch::Off(OffReason::Ci),
           None if !inputs.heartbeat => Switch::Off(OffReason::Config),
           None => Switch::On,
       };
       Decision { heartbeat }
   }

   /// Rules 1–3, which turn off everything that goes to us.
   fn shared_off(inputs: &Inputs) -> Option<OffReason> {
       if inputs.env.forced_off {
           Some(OffReason::CetraTelemetry)
       } else if inputs.env.do_not_track {
           Some(OffReason::DoNotTrack)
       } else if inputs.restore_mode {
           Some(OffReason::RestoreMode)
       } else {
           None
       }
   }
   ```
2. In `observability/mod.rs` add `pub(crate) mod switchboard;`. Everything stays `pub(crate)`: the stdout filter that reads `NOTICE_DIRECTIVE` is in this crate (M1's `subscriber.rs`), and the workspace lint `unreachable_pub` warns on a `pub` item the crate does not export.

**Tests** (in `switchboard.rs`):
- `from_pairs`:
  - `nothing_set_is_the_default`;
  - `off_false_and_zero_force_off_in_any_case` (`off`, `OFF`, `Off`, `false`, `FALSE`, `0`, ` off `);
  - `an_empty_cetra_telemetry_is_ignored` (`""` and `"  "` → `forced_off == false`, `Ok`);
  - `any_other_value_is_an_error` (`on`, `true`, `1`, `no`, `disabled`, `offf`, `of` → `Err(InvalidValue)`);
  - `the_error_never_carries_the_value` (value `planted-value-7f3`; `err.to_string()` does not contain it);
  - `the_cetra_telemetry_name_matches_in_any_case` (`cetra_telemetry=off` → forced; `Cetra_Telemetry=on` → `Err`);
  - `do_not_track_values` (`1`, `true`, `yes`, `anything` → true; `0`, `false`, `FALSE`, `""`, unset → false);
  - `do_not_track_name_is_exact` (`do_not_track=1` → false);
  - `ci_values` (`CI=true`, `CI=1` → ci; `CI=false`, `CI=0`, `CI=""` → not);
  - `every_ci_variable_marks_ci` (loop over `CI_VARIABLES` with value `x` → ci; with `""` → not);
  - `unrelated_variables_are_ignored` (`HOME`, `CETRA_LOG`, `CETRA_TELEMETRY__HEARTBEAT=false` → default).
- `decide` (build `Inputs` with all rules clear, `heartbeat: true`, then change one or two fields):
  - `all_clear_is_on`;
  - `cetra_telemetry_is_rule_one` (forced_off with everything else set → `CetraTelemetry`);
  - `do_not_track_is_rule_two` (do_not_track + restore + debug → `DoNotTrack`);
  - `restore_mode_is_rule_three` (restore + debug + ci → `RestoreMode`);
  - `a_debug_build_is_off` (debug + ci → `DebugBuild`);
  - `ci_is_off` (`Ci`);
  - `the_config_is_rule_five` (`heartbeat: false` → `Config`);
  - `the_config_cannot_turn_on_what_a_rule_turned_off` (`heartbeat: true` with each of rules 1–4 → off).
- `reasons_have_fixed_names` (each `OffReason::as_str` equals the string of step 1).
- `the_notice_directive_names_the_notice_target` (`NOTICE_DIRECTIVE == format!("{NOTICE_TARGET}=info")`).

**Commit:** `Add the telemetry switchboard`

---

### TL4.3 — `[telemetry]` and `CETRA_TELEMETRY` in the config

**Goal.** The `[telemetry]` table with `heartbeat` and `endpoint`, `CETRA_TELEMETRY` read first and filtered out of the `CETRA_` mapping, the switchboard input on `ServerConfig`, and the configuration reference rows.

**Edit:**
- `crates/server/src/config.rs`
- `crates/server/src/observability/switchboard.rs` (add `for_config` and its test only)
- `crates/server/src/test_support/mod.rs` (the two `ServerConfig { … }` literals and the `use crate::config::{…}` list only)
- `crates/server/src/ingest/fixtures_tests.rs` (the `ServerConfig { … }` literal and the `use crate::config::{…}` list only)
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`

**Steps.**
1. In `config.rs`, add next to `LogConfig`:
   ```rust
   /// The default `telemetry.endpoint`: the relay track Q runs (D-TL-10).
   const DEFAULT_TELEMETRY_ENDPOINT: &str = "https://telemetry.cetra.sh";

   /// The rule a bad `CETRA_TELEMETRY` breaks (D-TL-11). Never the value.
   const TELEMETRY_VALUES: &str = "accepted values are off, false and 0 (any case); unset or empty leaves the config in charge";

   /// `[telemetry]`: what cetra sends to us (D-TL-1, D-TL-8). The switchboard can turn it off
   /// whatever these say (D-TL-11).
   #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
   #[serde(deny_unknown_fields)]
   pub struct TelemetryConfig {
       /// The daily anonymous heartbeat. Default true.
       #[serde(default = "default_heartbeat")]
       pub heartbeat: bool,
       /// Base URL of the relay; the heartbeat goes to `<endpoint>/v1/heartbeat`. `https`, or
       /// `http` to a loopback IP. Stored without a trailing slash.
       #[serde(default = "default_telemetry_endpoint")]
       pub endpoint: String,
       /// `CETRA_TELEMETRY`, `DO_NOT_TRACK` and the CI variables, captured by the loader from
       /// the environment it reads. Not a key.
       #[serde(skip)]
       pub(crate) env: crate::observability::switchboard::TelemetryEnv,
   }

   impl Default for TelemetryConfig {
       fn default() -> Self {
           Self {
               heartbeat: default_heartbeat(),
               endpoint: default_telemetry_endpoint(),
               env: crate::observability::switchboard::TelemetryEnv::default(),
           }
       }
   }

   fn default_heartbeat() -> bool {
       true
   }

   fn default_telemetry_endpoint() -> String {
       DEFAULT_TELEMETRY_ENDPOINT.to_owned()
   }
   ```
   If `pub(crate) env` in a `pub` struct is refused by a lint (`unreachable_pub` or similar), stop and report.
2. In `ServerConfig`, after the `self_monitoring` field (which M1's TL1.2 put right after `log`), add:
   ```rust
   /// `[telemetry]`: the heartbeat to us (D-TL-8).
   #[serde(default)]
   pub telemetry: TelemetryConfig,
   ```
   Change `ServerConfig`'s doc comment, which is these two lines today:
   ```rust
   /// Keys that spec 12 lists for later work (telemetry, compaction, gc, deletion,
   /// anchor) are not fields. Setting one is an unknown-key error.
   ```
   to:
   ```rust
   /// Keys that spec 12 lists for later work (compaction, gc, deletion, anchor) are not
   /// fields. Setting one is an unknown-key error.
   ```
3. In `load_from`, as the very first statement (before `Figment::new()`):
   ```rust
   let telemetry_env = crate::observability::switchboard::TelemetryEnv::from_pairs(env)
       .map_err(|_| ConfigError::Provider {
           key: "CETRA_TELEMETRY".to_owned(),
           reason: TELEMETRY_VALUES,
       })?;
   ```
   After `apply_oidc_env(&mut config.auth, env)?;` and before `validate(&mut config)?;`, add `config.telemetry.env = telemetry_env;`.
4. In `is_filtered`, add `"CETRA_TELEMETRY"` to the array, and change its doc to: `` /// `CETRA_CONFIG`, `CETRA_TELEMETRY` (read by [`load_from`] before the sources, D-TL-11) and the CLI's own variables (D-H-12) are not config keys. ``
5. Add the validation, in full, after `validate_demo`:
   ```rust
   /// `telemetry.endpoint` (D-TL-10): `https`, or `http` to a loopback IP address; no user info,
   /// query or fragment. Stored without a trailing slash. Reasons are static.
   fn validate_telemetry(telemetry: &mut TelemetryConfig) -> Result<(), ConfigError> {
       let invalid = |reason| ConfigError::Invalid {
           key: "telemetry.endpoint",
           reason,
       };
       let url = url::Url::parse(telemetry.endpoint.trim())
           .map_err(|_| invalid("must be an absolute http or https URL"))?;
       let loopback = match url.host() {
           Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
           Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
           _ => false,
       };
       match url.scheme() {
           "https" => {}
           "http" if loopback => {}
           _ => return Err(invalid("must be https, or http to a loopback address")),
       }
       if !url.username().is_empty()
           || url.password().is_some()
           || url.query().is_some()
           || url.fragment().is_some()
       {
           return Err(invalid("must not contain user info, a query or a fragment"));
       }
       telemetry.endpoint = url.as_str().trim_end_matches('/').to_owned();
       Ok(())
   }
   ```
   Call it from `validate`, right after `validate_demo(&config.demo)?;`: `validate_telemetry(&mut config.telemetry)?;`. `telemetry.endpoint` is not a secret path: do not add it to `is_secret_path` or `secret_key`.
6. In `switchboard.rs`, add after `decide`:
   ```rust
   /// The decision for this process: [`decide`] over `config` and this build.
   pub(crate) fn for_config(config: &crate::config::ServerConfig) -> Decision {
       decide(&Inputs {
           env: config.telemetry.env,
           restore_mode: config.restore_mode,
           debug_build: cfg!(debug_assertions),
           heartbeat: config.telemetry.heartbeat,
       })
   }
   ```
7. In `test_support/mod.rs`, in both `ServerConfig { … }` literals (`default_config` near line 1258 and `config` near line 1815), add after the `self_monitoring: …` line that TL1.2 added (if it is missing, stop and report: TL1.2 is not in yet):
   ```rust
   // Loopback discard port: a test build without debug_assertions can never reach the relay.
   telemetry: TelemetryConfig {
       endpoint: "http://127.0.0.1:9".to_owned(),
       ..TelemetryConfig::default()
   },
   ```
   and add `TelemetryConfig` to the existing `use crate::config::{…}` list. Do the same in `ingest/fixtures_tests.rs` (`serve_config`, near line 438).
8. In `configuration-reference.md`, add this section after the `## [log]` section (if M1's `## [self_monitoring]` section is already after `[log]`, put this one after it):
   ```markdown
   ## `[telemetry]`

   What cetra sends to us: an anonymous heartbeat once a day. [Telemetry](/docs/self-hosting/telemetry/) lists every field. `CETRA_TELEMETRY=off`, `DO_NOT_TRACK=1`, restore mode, CI environments and debug builds turn it off whatever these settings say. `CETRA_TELEMETRY` is not the environment form of a setting: its only accepted values are `off`, `false` and `0`, and any other value stops cetra at startup.

   | Key | Environment | Type | Default | Description |
   |---|---|---|---|---|
   | `telemetry.heartbeat` | | boolean | `true` | Send the anonymous daily heartbeat. |
   | `telemetry.endpoint` | | URL | `"https://telemetry.cetra.sh"` | Where the heartbeat goes, as `<endpoint>/v1/heartbeat`. `https`, or `http` to a loopback IP address such as `127.0.0.1` (not the name `localhost`). No user info, query or fragment. For a relay of your own. |
   ```
   The Environment column stays empty: the reference test requires every name there to be an `ALIASES` entry, and `CETRA_TELEMETRY` is not one.

**Tests** (in `config.rs`'s `mod tests`, using `load_from`):
- `telemetry_defaults` (`load_from(None, &[])`: `heartbeat == true`, `endpoint == "https://telemetry.cetra.sh"`, `env == TelemetryEnv::default()`).
- `cetra_telemetry_off_loads_and_keeps_the_table` (`[("CETRA_TELEMETRY", "off")]`: loads, `env.forced_off`, `heartbeat == true`).
- `cetra_telemetry_is_not_mapped_to_the_table` (`[("CETRA_TELEMETRY", "OFF"), ("CETRA_TELEMETRY__HEARTBEAT", "false")]`: loads, `forced_off`, `heartbeat == false`).
- `a_bad_cetra_telemetry_is_a_load_error` (`on`, `1`, `true`: `Err(ConfigError::Provider { key, reason })` with `key == "CETRA_TELEMETRY"` and `reason == TELEMETRY_VALUES`, checked with `matches!` and a guard).
- `the_cetra_telemetry_error_never_quotes_the_value` (value `planted-value-7f3`: `error.to_string()` does not contain it).
- `an_empty_cetra_telemetry_loads` (`""`: `forced_off == false`).
- `do_not_track_and_ci_reach_the_config` (`DO_NOT_TRACK=1`, `GITHUB_ACTIONS=true`: `env.do_not_track && env.ci`).
- `telemetry_heartbeat_from_toml` (`"[telemetry]\nheartbeat = false\n"` → false).
- `telemetry_endpoint_accepts` (TOML or `CETRA_TELEMETRY__ENDPOINT`): `https://relay.example.com/cetra/` → stored `https://relay.example.com/cetra`; `http://127.0.0.1:9` → kept; `http://[::1]:8080/` → `http://[::1]:8080`; default → unchanged.
- `telemetry_endpoint_refuses` (each `matches!(error, ConfigError::Invalid { key: "telemetry.endpoint", reason } if reason == <the reason of step 5>)`): `""` and `not a url` (absolute URL); `http://localhost:8080`, `http://relay.example.com`, `ftp://relay.example.com`, `http://10.0.0.1` (scheme rule); `https://user:pw@relay.example.com`, `https://relay.example.com/?a=1`, `https://relay.example.com/#f` (user info rule).
- `telemetry_env_is_not_a_key` (`"[telemetry]\nenv = 1\n"` → `Err(ConfigError::Figment(_))`).
- `the_configuration_reference_matches_the_loader` (existing) passes with the two new rows.
In `switchboard.rs`: `for_config_reads_the_config` (`crate::test_support::config(dir)`, then `restore_mode = true` → `Off(RestoreMode)`; with `restore_mode = false` → `Off(DebugBuild)` in a test build; with `telemetry.env.forced_off = true` → `Off(CetraTelemetry)`).

**Commit:** `Add the [telemetry] table and CETRA_TELEMETRY`

---

### TL4.4 — The instance id

**Goal.** A random UUIDv4 in `instance_settings['telemetry.instance_id']`, created on first use with an insert that keeps an existing row.

**Edit:**
- `Cargo.toml` (root, the `uuid` line only)
- `crates/server/src/meta/instance_settings.rs`
- `crates/server/src/observability/instance.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)

**Steps.**
1. Root `Cargo.toml`: `uuid = { version = "1.29", features = ["v7", "serde"] }` becomes `uuid = { version = "1.29", features = ["v4", "v7", "serde"] }`. Run `cargo info uuid@1.29.2` (read-only) and report its version line and its `v4` feature line. Do not touch `Cargo.lock`.
2. In `meta/instance_settings.rs`, add after `LICENSE_KEY`:
   ```rust
   /// Key of this instance's random id for the heartbeat and crash reports (D-TL-9). A UUIDv4.
   pub(crate) const TELEMETRY_INSTANCE_ID: &str = "telemetry.instance_id";

   /// Key of the time of the last heartbeat the relay accepted (D-TL-8): i64 nanoseconds since
   /// the Unix epoch, as decimal text.
   pub(crate) const TELEMETRY_LAST_HEARTBEAT_AT: &str = "telemetry.last_heartbeat_at";
   ```
   and after `put_setting`:
   ```rust
   /// Inserts `key` with `value` unless the key exists; an existing value is kept (D-TL-9).
   ///
   /// # Errors
   ///
   /// Returns [`MetaError::Db`] when the write fails.
   pub(crate) async fn insert_setting_if_absent(
       conn: &mut SqliteConnection,
       key: &str,
       value: &str,
   ) -> Result<(), MetaError> {
       sqlx::query(
           "INSERT INTO instance_settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO NOTHING",
       )
       .bind(key)
       .bind(value)
       .execute(&mut *conn)
       .await?;
       Ok(())
   }
   ```
3. Create `observability/instance.rs`. Module doc: `//! The instance id (D-TL-9): a random UUIDv4 in `instance_settings`, not a UUIDv7 (its timestamp would reveal the install time). A restored backup keeps it. It is used for nothing but what goes to us and is never joined with a licence.` Contents:
   ```rust
   use uuid::Uuid;

   use crate::meta::instance_settings::{TELEMETRY_INSTANCE_ID, get_setting_tx, insert_setting_if_absent};
   use crate::meta::{MetaDb, MetaError};

   /// This instance's id, created on first use. Two callers at once get the same id: the insert
   /// keeps an existing row, and the read is in the same transaction.
   ///
   /// # Errors
   ///
   /// [`MetaError`] when the write fails, or [`MetaError::Corrupt`] when the stored value is not
   /// a UUID.
   pub(crate) async fn instance_id(meta: &MetaDb) -> Result<Uuid, MetaError> {
       let candidate = Uuid::new_v4().to_string();
       let stored = meta
           .write(move |conn| {
               Box::pin(async move {
                   insert_setting_if_absent(conn, TELEMETRY_INSTANCE_ID, &candidate).await?;
                   get_setting_tx(conn, TELEMETRY_INSTANCE_ID).await
               })
           })
           .await?;
       stored
           .as_deref()
           .and_then(|value| Uuid::parse_str(value).ok())
           .ok_or(MetaError::Corrupt {
               table: "instance_settings",
               column: "value",
           })
   }
   ```
4. In `observability/mod.rs` add `pub(crate) mod instance;`.

**Tests.**
- In `instance_settings.rs`: `insert_if_absent_writes_a_new_key`; `insert_if_absent_keeps_an_existing_value` (`put_setting(k, "first")`, then `insert_setting_if_absent(k, "second")` → `"first"`).
- In `instance.rs` (use `crate::meta::test_db()`):
  - `the_first_call_creates_a_v4_id` (`get_version_num() == 4`; the row equals its hyphenated lowercase form);
  - `later_calls_return_the_same_id`;
  - `an_existing_id_is_kept` (`put_setting(TELEMETRY_INSTANCE_ID, "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d")` first → that id);
  - `concurrent_calls_agree` (`tokio::join!` of two calls on the same `MetaDb` → equal);
  - `a_corrupt_value_is_an_error` (`put_setting(…, "not-a-uuid")` → `assert!(matches!(result, Err(MetaError::Corrupt { table: "instance_settings", column: "value" })))`, and the row is unchanged).

**Commit:** `Add the telemetry instance id`

---

### TL4.5 — The heartbeat payload

**Goal.** The heartbeat as a fixed serde type and the queries that fill it, with golden, leak and licence-separation tests.

**Edit:**
- `crates/server/src/observability/heartbeat.rs` (new)
- `crates/server/src/observability/mod.rs` (one `mod` line)

**M1 names used here** (`01-m1-self-monitoring-logs.md` § Interface for later milestones): `ServerConfig.self_monitoring: SelfMonitoringConfig` (after `log`), its field `pub otlp_endpoint: Option<OtlpEndpoint>`, its method `pub fn exports(&self) -> bool`, `pub(crate) fn parse_otlp_endpoint(&str) -> Result<OtlpEndpoint, &'static str>` and `pub(crate) fn load_from(Option<&str>, &[(&str, &str)]) -> Result<ServerConfig, ConfigError>` in `crate::config`. Before you start, check each in `crates/server/src/config.rs`; if one is missing or differs, stop and report.

**Steps.**
1. Create `heartbeat.rs`. Module doc: `//! The daily anonymous heartbeat (D-TL-8): a fixed type whose every value is a constant, a bucket or a boolean. It never carries emails, domains, names, URLs, host names, ids of orgs or projects, exact counts or licence state, and this module reads nothing of the licence code (D172).` Contents for this phase:
   ```rust
   use std::time::Duration;

   use cetra_storage::config::ObjectStoreUrl;
   use chrono::DateTime;
   use serde::Serialize;
   use uuid::Uuid;

   use crate::config::ServerConfig;
   use crate::meta::{MetaDb, MetaError};
   use crate::observability::buckets::{
       CountBucket, InstallAgeBucket, UptimeBucket, VolumeBucket, count_bucket,
       install_age_bucket, uptime_bucket, volume_bucket,
   };
   use crate::observability::facts::{self, Deployment, Profile};

   /// The payload's schema version (TL-Q1: the relay refuses other versions).
   pub(crate) const HEARTBEAT_SCHEMA: u32 = 1;

   /// Where flushed Parquet files live.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   #[serde(rename_all = "lowercase")]
   pub(crate) enum StorageKind {
       /// `file://`.
       File,
       /// `s3://`.
       S3,
   }

   /// Which features the instance uses. M5 adds `crash_reports_on`.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   pub(crate) struct Features {
       /// `smtp_url` is set.
       pub(crate) email: bool,
       /// `[auth.github]` is set.
       pub(crate) github_sign_in: bool,
       /// `[auth.google]` is set.
       pub(crate) google_sign_in: bool,
       /// At least one `[[auth.oidc]]` provider.
       pub(crate) oidc_sign_in: bool,
       /// An alert rule exists.
       pub(crate) alert_rules: bool,
       /// An online evaluation rule exists.
       pub(crate) online_rules: bool,
       /// A dataset exists.
       pub(crate) datasets: bool,
       /// A prompt exists.
       pub(crate) prompts: bool,
       /// An annotation queue exists.
       pub(crate) annotation_queues: bool,
       /// cetra exports its own telemetry over OTLP (`self_monitoring.exports()`).
       pub(crate) self_monitoring: bool,
   }

   /// The heartbeat body (D-TL-8). Field order is the JSON key order.
   #[derive(Debug, Clone, PartialEq, Eq, Serialize)]
   pub(crate) struct Heartbeat {
       /// [`HEARTBEAT_SCHEMA`].
       pub(crate) schema: u32,
       /// `instance_settings['telemetry.instance_id']`.
       pub(crate) instance_id: Uuid,
       /// [`facts::VERSION`].
       pub(crate) version: &'static str,
       /// [`facts::TARGET`].
       pub(crate) target: &'static str,
       /// [`facts::profile`].
       pub(crate) profile: Profile,
       /// [`facts::deployment`].
       pub(crate) deployment: Deployment,
       /// From `storage.object_store`.
       pub(crate) storage: StorageKind,
       /// Time since the process started ([`facts::process_uptime`]).
       pub(crate) uptime: UptimeBucket,
       /// Time since the first user was created.
       pub(crate) install_age: InstallAgeBucket,
       /// Active orgs.
       pub(crate) orgs: CountBucket,
       /// Active projects.
       pub(crate) projects: CountBucket,
       /// Users not disabled.
       pub(crate) users: CountBucket,
       /// Spans of yesterday (UTC) in `usage_daily`, every org.
       pub(crate) spans_per_day: VolumeBucket,
       /// Scores of yesterday (UTC) in `usage_daily`, every org.
       pub(crate) scores_per_day: VolumeBucket,
       /// Feature booleans.
       pub(crate) features: Features,
   }

   /// Whether each feature table has a row.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
   pub(crate) struct TableFlags {
       pub(crate) alert_rules: bool,
       pub(crate) online_rules: bool,
       pub(crate) datasets: bool,
       pub(crate) prompts: bool,
       pub(crate) annotation_queues: bool,
   }
   ```
   (Give each `TableFlags` field a one-line doc.)
2. `pub(crate) fn storage_kind(config: &ServerConfig) -> StorageKind`: `match config.storage.object_store_url()` → `ObjectStoreUrl::Local(_)` is `File`, `ObjectStoreUrl::S3 { .. }` is `S3`.
3. `pub(crate) fn features(config: &ServerConfig, tables: TableFlags) -> Features`, in full:
   ```rust
   Features {
       email: config.smtp_url.is_some(),
       github_sign_in: config.auth.github.is_some(),
       google_sign_in: config.auth.google.is_some(),
       oidc_sign_in: !config.auth.oidc.is_empty(),
       alert_rules: tables.alert_rules,
       online_rules: tables.online_rules,
       datasets: tables.datasets,
       prompts: tables.prompts,
       annotation_queues: tables.annotation_queues,
       self_monitoring: config.self_monitoring.exports(),
   }
   ```
4. `async fn table_flags(meta: &MetaDb) -> Result<TableFlags, MetaError>`: five `sqlx::query_scalar::<_, bool>` on `meta.reader()`, one per table, exactly:
   ```sql
   SELECT EXISTS(SELECT 1 FROM alert_rules)
   SELECT EXISTS(SELECT 1 FROM online_rules)
   SELECT EXISTS(SELECT 1 FROM datasets)
   SELECT EXISTS(SELECT 1 FROM prompts)
   SELECT EXISTS(SELECT 1 FROM annotation_queues)
   ```
   Errors go up with `?` (`MetaError` converts from `sqlx::Error`, as in `meta/usage.rs`).
5. `pub(crate) async fn collect(meta: &MetaDb, config: &ServerConfig, instance_id: Uuid, deployment: Deployment, uptime: Duration, now_ns: i64) -> Result<Heartbeat, MetaError>`. Doc: "The heartbeat as it would be sent now. Reads `meta.db` only; M3's debug bundle reuses it." Steps, in this order, every query on `meta.reader()`:
   1. `orgs`: `SELECT COUNT(*) FROM orgs WHERE state = 'active'` as `i64`.
   2. `projects`: `SELECT COUNT(*) FROM projects WHERE state = 'active'`.
   3. `users`: `SELECT COUNT(*) FROM users WHERE disabled_at IS NULL`.
   4. Yesterday: `let today = DateTime::from_timestamp_nanos(now_ns).date_naive();` `let yesterday = today.pred_opt().unwrap_or(today);` (comment: `NaiveDate::MIN` has no predecessor; never reached for a real clock), formatted `"%Y-%m-%d"` as `usage.rs` does. `let (spans, scores) = sqlx::query_as::<_, (i64, i64)>("SELECT COALESCE(SUM(spans), 0), COALESCE(SUM(scores), 0) FROM usage_daily WHERE day = ?").bind(day).fetch_one(meta.reader()).await?;`
   5. Install age: `sqlx::query_scalar::<_, Option<i64>>("SELECT MIN(created_at) FROM users").fetch_one(meta.reader()).await?`; age = `now_ns - min` when `Some` and positive, else 0, as `Duration::from_nanos(u64::try_from(…).unwrap_or(0))`.
   6. `table_flags(meta)`.
   7. Convert every `i64` count with `u64::try_from(value).unwrap_or(0)` (a negative count cannot happen; 0 is the safe side), then build `Heartbeat { schema: HEARTBEAT_SCHEMA, instance_id, version: facts::VERSION, target: facts::TARGET, profile: facts::profile(), deployment, storage: storage_kind(config), uptime: uptime_bucket(uptime), install_age: install_age_bucket(age), orgs: count_bucket(orgs), projects: count_bucket(projects), users: count_bucket(users), spans_per_day: volume_bucket(spans), scores_per_day: volume_bucket(scores), features: features(config, flags) }`.
6. In `observability/mod.rs` add `pub(crate) mod heartbeat;`.

**Tests** (in `heartbeat.rs`):
- `the_payload_matches_the_golden_json`: build a `Heartbeat` with `instance_id = Uuid::parse_str("8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d")`, `version: "0.1.0"`, `target: "x86_64-unknown-linux-musl"`, `Profile::Release`, `Deployment::Docker`, `StorageKind::File`, `UptimeBucket::UnderHour`, `InstallAgeBucket::MonthToQuarter`, orgs `One`, projects `TwoToFive`, users `SixToTwenty`, spans `UnderHundredThousand`, scores `UnderThousand`, features `email, google_sign_in, alert_rules, online_rules, prompts` true and the rest false; `serde_json::to_string_pretty` equals exactly:
  ```json
  {
    "schema": 1,
    "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
    "version": "0.1.0",
    "target": "x86_64-unknown-linux-musl",
    "profile": "release",
    "deployment": "docker",
    "storage": "file",
    "uptime": "<1h",
    "install_age": "30d-90d",
    "orgs": "1",
    "projects": "2-5",
    "users": "6-20",
    "spans_per_day": "<100k",
    "scores_per_day": "<1k",
    "features": {
      "email": true,
      "github_sign_in": false,
      "google_sign_in": true,
      "oidc_sign_in": false,
      "alert_rules": true,
      "online_rules": true,
      "datasets": false,
      "prompts": true,
      "annotation_queues": false,
      "self_monitoring": false
    }
  }
  ```
- `the_key_set_is_fixed` (`serde_json::to_value`: top-level keys are exactly the 15 above in that order; `features` keys exactly the 10 above; no key contains `licen`).
- `a_fresh_instance_collects_zeros` (`TestApp::new()`, `collect(…, Duration::ZERO, app.state.clock.now_ns())`: orgs, projects, users `Zero`; volumes `Zero`; `install_age == UnderDay`; `uptime == UnderHour`; all table flags false; `storage == File`; `version == facts::VERSION`).
- `collect_counts_and_buckets`: `TestApp::new()` then `seed()` (1 user, 1 org, 1 project); insert with `meta.write` and raw SQL two `usage_daily` rows: yesterday (`2025-12-31` for the test clock `MANUAL_NOW_NS` = 2026-01-01T00:00:00Z) with `spans = 1500, scores = 3`, and today (`2026-01-01`) with `spans = 1_000_000_000`; columns `org_id, project_id, day, spans, scores` (the others have defaults). Expect orgs/projects/users `One`, `spans_per_day == UnderTenThousand`, `scores_per_day == UnderThousand`.
- `deleting_orgs_and_disabled_users_are_not_counted` (after `seed()`, `UPDATE orgs SET state = 'deleting'`, `UPDATE users SET disabled_at = 1` → orgs `Zero`, users `Zero`).
- `install_age_uses_the_first_user` (`seed()`, then `UPDATE users SET created_at = now - 40 days in ns` → `MonthToQuarter`).
- `table_flags_see_each_table`: after `seed()`, insert one row per table in one `app.state.meta.write` with `sqlx::query(…)`, binding the seeded project id (`seeded.project.id.to_string()`) to each `?`, timestamps `1`:
  ```sql
  INSERT INTO alert_rules (id, project_id, name, metric, config, created_at, updated_at) VALUES ('a1', ?, 'r', 'error_rate', '{}', 1, 1)
  INSERT INTO evaluators (id, project_id, name, kind, config, output_score, created_at, updated_at) VALUES ('e1', ?, 'e', 'code', '{}', 's', 1, 1)
  INSERT INTO online_rules (id, project_id, evaluator_id, name, target, variable_mapping, created_at, updated_at) VALUES ('o1', ?, 'e1', 'o', 'trace', '{}', 1, 1)
  INSERT INTO datasets (id, project_id, name, created_at, updated_at) VALUES ('d1', ?, 'd', 1, 1)
  INSERT INTO prompts (id, project_id, name, type, latest_version, created_at, updated_at) VALUES ('p1', ?, 'p', 'text', 1, 1, 1)
  INSERT INTO annotation_queues (id, project_id, name, target, created_at, updated_at) VALUES ('q1', ?, 'q', 'trace', 1, 1)
  ```
  then `table_flags` is all true. (If a `CHECK` or `NOT NULL` constraint refuses one of these rows, read its migration and add the missing column; report it.)
- `features_follow_the_config`: `crate::test_support::config(dir)` gives all config features false; then set `smtp_url` (build an `SmtpUrl` as `routes/admin.rs:988-1000` does), `auth.github` (a `GithubAuth` with a `Secret` from `serde_json::from_str("\"x\"")`), `auth.google`, one `OidcAuth` (copy the construction used in `auth/oidc/mod.rs`'s tests `settings()`), and `config.self_monitoring.otlp_endpoint = Some(crate::config::parse_otlp_endpoint("https://otlp.example.com").unwrap())` (M1's function; `OtlpEndpoint`'s fields are private; `logs` stays `true`, so `exports()` is true) → each boolean true.
- `storage_kind_follows_the_object_store` (default → `File`; `config.storage.object_store = Some(ObjectStoreUrl::S3 { bucket: "b".into(), prefix: String::new() })` → `S3`).
- `no_planted_value_reaches_the_payload`: `TestApp::with_config(|c| { c.email_from = Some("Planted <noreply@planted-domain.example>".into()); c.auth.signup_domains = vec!["planted-domain.example".into()]; })`, then `seed()`; collect with the fixed id `8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d` (never a random one: a random UUID may contain a short forbidden substring by chance); the JSON text contains none of: `planted`, `ada@`, `example.com`, `Acme`, `acme`, `Support`, `support-bot`, the seeded org id, project id and user id strings.
- `a_licensed_instance_sends_the_same_keys` (`TestApp::with_license(&["org_sso"])`; collect; key set equals the one of `the_key_set_is_fixed`; the JSON text does not contain `licen` or `org_sso`).
- `the_telemetry_modules_do_not_touch_licences`:
  ```rust
  let sources = [
      include_str!("heartbeat.rs"),
      include_str!("instance.rs"),
      include_str!("switchboard.rs"),
      include_str!("facts.rs"),
      include_str!("buckets.rs"),
  ];
  let forbidden = [concat!("crate::", "ee"), concat!("licen", "se"), concat!("entitle", "ments")];
  for source in sources {
      let code = source.split("#[cfg(test)]").next().unwrap_or(source);
      for word in forbidden {
          assert!(!code.contains(word), "a telemetry module mentions {word}");
      }
  }
  ```
- `the_payload_is_small` (the golden payload's JSON is under 4 096 bytes; TL-Q1 caps bodies at 64 KiB).

**Commit:** `Add the heartbeat payload`

---

### TL4.6 — Sending the heartbeat, its schedule and the startup notice

**Goal.** The heartbeat client, the 24 h schedule kept across restarts, the startup notice, and the task started from `serve::bind` when the switchboard allows it.

**Edit:**
- `crates/server/src/observability/heartbeat.rs`
- `crates/server/src/serve.rs`

**Steps.**
1. Imports to add to `heartbeat.rs` (above the code of TL4.5; merge with its `use` lines):
   ```rust
   use tokio::task::JoinHandle;

   use crate::http::AppState;
   use crate::meta::instance_settings::{TELEMETRY_LAST_HEARTBEAT_AT, get_setting, put_setting};
   use crate::observability::instance;
   use crate::observability::switchboard::{Decision, NOTICE_TARGET, Switch};
   ```
   Every item of this phase goes above the `#[cfg(test)] mod tests` at the end of the file.
2. Add:
   ```rust
   /// The first heartbeat waits this long after start, plus up to [`MAX_JITTER`].
   pub(crate) const FIRST_DELAY: Duration = Duration::from_secs(10 * 60);
   /// The most random delay added to [`FIRST_DELAY`].
   pub(crate) const MAX_JITTER: Duration = Duration::from_secs(10 * 60);
   /// One heartbeat a day.
   pub(crate) const PERIOD: Duration = Duration::from_secs(24 * 60 * 60);
   /// Each request gives up after this long.
   pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
   /// The page that lists every field, logged in the notice's `docs` field.
   pub(crate) const TELEMETRY_DOCS_URL: &str = "https://cetra.sh/docs/self-hosting/telemetry/";

   /// Why a heartbeat was not sent. Logged at `debug` only; no variant carries a URL or a body.
   #[derive(Debug, thiserror::Error)]
   pub(crate) enum HeartbeatError {
       /// The HTTP client could not be built.
       #[error("heartbeat client could not be built")]
       Client,
       /// The request failed before a response (connection, TLS, timeout).
       #[error("heartbeat request failed (timeout: {timeout})")]
       Request {
           /// The request hit the client's timeout.
           timeout: bool,
       },
       /// The relay answered something other than 2xx (redirects are not followed).
       #[error("heartbeat endpoint answered {0}")]
       Status(u16),
   }
   ```
   (`meta.db` errors stay `MetaError` and are logged where they happen, step 7.)
3. `pub(crate) fn client(timeout: Duration) -> Result<reqwest::Client, HeartbeatError>`, mirroring `identity_client` (`auth/oidc/mod.rs:192`): 
   ```rust
   let builder = reqwest::Client::builder()
       .redirect(reqwest::redirect::Policy::none())
       .timeout(timeout)
       .user_agent(format!("cetra/{}", facts::VERSION));
   // A developer's HTTP(S)_PROXY must not catch the tests' 127.0.0.1 receivers (as M1's export client).
   #[cfg(test)]
   let builder = builder.no_proxy();
   builder.build().map_err(|_| HeartbeatError::Client)
   ```
   Outside tests it honours `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY`, like `identity_client`. Its own client: never the evaluation runner's or the identity client.
4. `pub(crate) fn heartbeat_url(endpoint: &str) -> String`: `format!("{}/v1/heartbeat", endpoint.trim_end_matches('/'))`.
5. `fn wait_before_send(last_sent_ns: Option<i64>, now_ns: i64) -> Option<Duration>`: how long until the next heartbeat is due, or `None` when it is due now. Compute in `i128` so nothing overflows:
   ```rust
   let last = i128::from(last_sent_ns?);
   let now = i128::from(now_ns);
   let period = i128::from(u64::try_from(PERIOD.as_nanos()).unwrap_or(u64::MAX));
   if last > now {
       // The clock went back: wait a whole period rather than send twice in a day.
       return Some(PERIOD);
   }
   let elapsed = now - last;
   if elapsed >= period {
       None
   } else {
       Some(Duration::from_nanos(u64::try_from(period - elapsed).unwrap_or(0)))
   }
   ```
6. `fn parse_last_sent(value: Option<String>) -> Option<i64>`: `value.and_then(|v| v.trim().parse::<i64>().ok())`. An unparseable row counts as never sent (the next success rewrites it).
7. A sender:
   ```rust
   /// What the heartbeat loop keeps between ticks.
   #[derive(Debug)]
   pub(crate) struct Sender {
       client: reqwest::Client,
       url: String,
       instance_id: Uuid,
       deployment: Deployment,
   }
   ```
   with:
   - `pub(crate) fn new(client: reqwest::Client, url: String, instance_id: Uuid, deployment: Deployment) -> Self`.
   - `async fn send(&self, body: &Heartbeat) -> Result<(), HeartbeatError>`: `self.client.post(&self.url).json(body).send().await`; a `reqwest::Error` becomes `Request { timeout: error.is_timeout() }`; a response whose `status().is_success()` is `Ok(())`; any other status is `Status(status.as_u16())`. The response body is never read.
   - `pub(crate) async fn tick(&self, state: &AppState) -> Duration`: one pass of the loop; returns how long to sleep next. In order:
     1. `let now = state.clock.now_ns();`
     2. `get_setting(state.meta.reader(), TELEMETRY_LAST_HEARTBEAT_AT).await`. On `Err(error)`: `tracing::debug!(error = %error, "telemetry: heartbeat not sent")`, return `PERIOD`.
     3. `if let Some(wait) = wait_before_send(parse_last_sent(value), now) { return wait; }`
     4. `collect(&state.meta, &state.config, self.instance_id, self.deployment, facts::process_uptime(), now).await`. On `Err(error)`: the same debug line, return `PERIOD`.
     5. `self.send(&payload).await`. On `Err(error)`: the same debug line, return `PERIOD`.
     6. On `Ok`: `state.meta.write(move |conn| Box::pin(async move { put_setting(conn, TELEMETRY_LAST_HEARTBEAT_AT, &now.to_string()).await })).await`; on `Err(error)`: `tracing::debug!(error = %error, "telemetry: heartbeat time not recorded")`. Then `tracing::debug!("telemetry: heartbeat sent")`. Return `PERIOD`.
     Nothing in this module logs above `debug` except the notice (step 8): an install without internet access must not get a daily warning.
8. The notice, two functions:
   ```rust
   /// The startup line when the heartbeat runs. Shows on stdout at `log.level = "warn"` too
   /// (NOTICE_DIRECTIVE).
   fn notice_on(instance_id: Uuid) {
       tracing::info!(
           target: NOTICE_TARGET,
           instance_id = %instance_id,
           docs = TELEMETRY_DOCS_URL,
           "telemetry: cetra sends an anonymous heartbeat once a day (version, platform, bucketed counts, features in use); turn it off with CETRA_TELEMETRY=off"
       );
   }

   /// The startup line when the heartbeat does not run, with why.
   fn notice_off(reason: &'static str) {
       tracing::info!(target: NOTICE_TARGET, reason, "telemetry: the heartbeat is off");
   }
   ```
9. `pub(crate) async fn start(state: &AppState, decision: Decision) -> Option<JoinHandle<()>>`. Doc: "Logs the startup notice and, when the switchboard allows it, spawns the heartbeat loop. Fails closed: any error here means no heartbeat." Steps:
   1. `if let Switch::Off(reason) = decision.heartbeat { notice_off(reason.as_str()); return None; }`
   2. `client(REQUEST_TIMEOUT)`; on `Err`: `notice_off("client_unavailable")`, return `None`.
   3. `instance::instance_id(&state.meta).await`; on `Err(error)`: `tracing::debug!(error = %error, "telemetry: instance id")`, `notice_off("instance_id_unavailable")`, return `None`.
   4. `notice_on(id)`.
   5. `let sender = Sender::new(client, heartbeat_url(&state.config.telemetry.endpoint), id, facts::deployment(facts::probe_host()));`
   6. `let first = FIRST_DELAY + Duration::from_secs(rand::random_range(0..=MAX_JITTER.as_secs()));`
   7. Spawn and return:
      ```rust
      let state = state.clone();
      Some(tokio::spawn(async move {
          tokio::time::sleep(first).await;
          loop {
              let wait = sender.tick(&state).await;
              tokio::time::sleep(wait).await;
          }
      }))
      ```
10. In `serve.rs` `bind`:
    1. As the first statement of `bind` (before `let data_dir = …`): `crate::observability::facts::mark_process_start();`
    2. Right after the restore-mode `if state.config.restore_mode { tracing::warn!(…); }` block (which follows `tasks.push(spawn_org_sso(state.clone()));`), add:
       ```rust
       // What may go to us (D-TL-11); the notice is logged either way.
       let decision = crate::observability::switchboard::for_config(&state.config);
       if let Some(task) = crate::observability::heartbeat::start(&state, decision).await {
           tasks.push(task);
       }
       ```
    Shutdown aborts it with the other `tasks`; nothing else changes in `Running`. `restore_mode_skips_the_sending_and_dns_loops` keeps its count of 4: in a test build the heartbeat is off in both runs (debug build, and restore mode).

**Tests** (in `heartbeat.rs`'s `mod tests`).

The test receiver, written in the test module in full:
```rust
/// Serves `POST /v1/heartbeat` on `127.0.0.1:0`: waits `delay`, records the request, answers
/// `status` (with `Location: /elsewhere` when `status` is a 3xx). `POST` and `GET /elsewhere`
/// count hits and answer 200. Aborts its task on drop.
struct Receiver {
    base: String,
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
    async fn start(status: u16, delay: Duration) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let elsewhere = Arc::new(AtomicUsize::new(0));
        let state = ReceiverState { status, delay, requests: Arc::clone(&requests), elsewhere: Arc::clone(&elsewhere) };
        let router = Router::new()
            .route("/v1/heartbeat", post(heartbeat))
            .route("/elsewhere", get(other).post(other))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self { base: format!("http://{addr}"), requests, elsewhere, task }
    }

    fn requests(&self) -> Vec<(HeaderMap, Bytes)> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn heartbeat(State(state): State<ReceiverState>, headers: HeaderMap, body: Bytes) -> Response {
    tokio::time::sleep(state.delay).await;
    state.requests.lock().unwrap_or_else(PoisonError::into_inner).push((headers, body));
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
(Imports for the test module: `std::sync::{Arc, Mutex, PoisonError}`, `std::sync::atomic::{AtomicUsize, Ordering}`, `axum::{Router, body::Bytes, extract::State, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, routing::{get, post}}`.) A test builds a `Sender` with `Sender::new(client(REQUEST_TIMEOUT).unwrap(), heartbeat_url(&receiver.base), id, Deployment::Binary)` and a payload with `collect` on a `TestApp`, or the golden `Heartbeat` of TL4.5.

- `heartbeat_url_appends_the_path` (`https://telemetry.cetra.sh` → `…/v1/heartbeat`; a trailing slash and a path prefix `https://r.example.com/cetra/` → `https://r.example.com/cetra/v1/heartbeat`).
- `wait_before_send_rules`: never sent → `None`; sent 1 h ago → `Some(23 h)`; exactly 24 h ago → `None`; 25 h ago → `None`; sent in the future → `Some(PERIOD)`; `(Some(i64::MIN), i64::MAX)` → `None` and `(Some(i64::MAX), i64::MIN)` → `Some(PERIOD)`, without a panic.
- `parse_last_sent_rules` (`"123"` → `Some(123)`; `" 123 "` → `Some(123)`; `"abc"`, `""`, `None` → `None`).
- `send_posts_json_with_the_user_agent` (`Receiver::start(204, ZERO)`: one request; `content-type` starts with `application/json`; `user-agent == format!("cetra/{}", facts::VERSION)`; the body parses to `serde_json::to_value(&payload)`).
- `send_does_not_follow_redirects` (`Receiver::start(307, ZERO)` → `Err(HeartbeatError::Status(307))` via `matches!`; `elsewhere` is 0).
- `a_4xx_and_a_5xx_are_errors` (`400` → `Status(400)`; `503` → `Status(503)`).
- `a_slow_endpoint_times_out` (`client(Duration::from_millis(200))`, `Receiver::start(204, 2 s)` → `Err(Request { timeout: true })`).
- `an_unreachable_endpoint_is_an_error` (bind a `TcpListener` on `127.0.0.1:0`, take its address, drop it, send to `http://<addr>/v1/heartbeat` → `Err(Request { .. })`).
- `tick_sends_and_records_the_time` (`TestApp::new()`; a `Receiver` answering 204; `tick` returns `PERIOD`; one request; `TELEMETRY_LAST_HEARTBEAT_AT` equals `app.state.clock.now_ns().to_string()`).
- `tick_waits_when_sent_recently` (store `now - 1 h in ns` with `put_setting`; `tick` returns exactly 23 h; the receiver got nothing).
- `tick_does_not_record_a_failure` (receiver `503`: `tick` returns `PERIOD`, the setting stays absent).
- `failures_are_logged_at_debug_only` (`crate::test_support::capture_logs` around a `tick` against a `503` receiver: the output contains neither `WARN` nor `ERROR`; the lines that contain `cetra_server` do not contain the receiver's port, so cetra logs no URL. Dependency lines are not checked: hyper may log its own connection at `trace`).
- `start_when_off_logs_the_reason_and_spawns_nothing` (`capture_logs(start(&app.state, Decision { heartbeat: Switch::Off(OffReason::DoNotTrack) }))` → `None`; logs contain `telemetry: the heartbeat is off` and `DO_NOT_TRACK`; no `telemetry.instance_id` row).
- `start_when_on_logs_the_instance_id_and_spawns` (`TestApp::new()`, whose endpoint is `http://127.0.0.1:9`; `Decision { heartbeat: Switch::On }` → `Some(handle)`; abort the handle; logs contain the stored instance id, `CETRA_TELEMETRY=off` and `TELEMETRY_DOCS_URL`; the `telemetry.instance_id` row exists).
In `serve.rs` tests:
- `the_startup_notice_says_why_the_heartbeat_is_off` (`capture_logs(Box::pin(bind(config(dir.path()), None)))`: logs contain `telemetry: the heartbeat is off` and `debug_build`; shut down).
- `restore_mode_is_the_telemetry_reason_in_restore_mode` (as `restore_mode_logs_at_startup`: the line that contains `telemetry: the heartbeat is off` also contains `restore_mode`; no line contains `debug_build`).

**Commit:** `Send the daily heartbeat`

---

### TL4.7 — The notice passes the stdout filter, and only that one

**Goal.** The stdout log filter always lets the telemetry notice through at `info`, whatever `log.level` says; the OTLP export filter does not get that directive.

**Prerequisite:** M1's TL1.5 is committed.

**Edit:**
- `crates/server/src/observability/subscriber.rs` (M1's file)

**M1 names used here** (`01-m1-self-monitoring-logs.md` § Interface for later milestones), checked first; if one is missing or has another signature, stop and report:
- `fn capped_filter(level: &str) -> Result<EnvFilter, ParseError>` (private: `EnvFilter::try_new(level)` plus the `rmcp=warn` directive);
- `pub(crate) fn log_filter(level: &str) -> Result<EnvFilter, ParseError>`, the stdout filter, whose body is `capped_filter(level)`;
- `fn export_filter(level: &str) -> Result<EnvFilter, ParseError>`, the export filter, which calls `capped_filter` and not `log_filter`;
- `pub(crate) fn build(config: &ServerConfig, format: LogFormat, writer: BoxMakeWriter, runtime: Handle) -> Result<(ComposedSubscriber, Observability), ObservabilityError>`, which builds the stdout layer from `log_filter`;
- `ParseError` imported from `tracing_subscriber::filter`;
- M1's tests `log_filter_caps_rmcp`, `a_debug_filter_drops_rmcp_debug_events` (with its `Buffer` writer) and `export_filter_adds_only_the_rmcp_cap`.

**Steps.**
1. Change only `log_filter`. Its body becomes, exactly:
   ```rust
   let notice = NOTICE_DIRECTIVE.parse()?;
   Ok(capped_filter(level)?.add_directive(notice))
   ```
   and add `use crate::observability::switchboard::NOTICE_DIRECTIVE;`. `Directive::from_str` returns `ParseError` too, so `?` needs no mapping. The notice directive comes last, so it replaces any directive for the same target in `log.level`.
2. Update `log_filter`'s doc comment: the stdout filter, `capped_filter` plus the telemetry notice directive, which the export filter never gets (D-TL-11, design § Startup notice).
3. `capped_filter`, `export_filter` and `build` do not change.

**Tests** (in `subscriber.rs`'s `mod tests`, next to M1's filter tests):
- `the_stdout_filter_has_the_notice_directive` (for `"info"`, `"warn"`, `"error"`: `log_filter(level).unwrap().to_string()` contains `cetra_server::observability::notice=info` and `rmcp=warn`).
- `the_export_filter_has_no_notice_directive` (for `"warn"` and `"info"`: `export_filter(level).unwrap().to_string()` does not contain `cetra_server::observability::notice`). M1's `export_filter_adds_only_the_rmcp_cap` keeps holding as well.
- `the_notice_shows_on_stdout_at_warn`: `build` with the config `crate::config::load_from(Some("[log]\nlevel = \"warn\"\n"), &[])` (no `[self_monitoring]` endpoint), M1's `Buffer` writer as a `BoxMakeWriter`, and `Handle::current()`; under `tracing::dispatcher::set_default`, emit `info!(target: NOTICE_TARGET, "notice-canary")` (`NOTICE_TARGET` from `crate::observability::switchboard`) and `info!(target: "cetra_server::serve", "dropped-canary")`. The buffer contains `notice-canary` and not `dropped-canary`.
- `the_notice_directive_wins_over_the_level` (`log_filter("warn,cetra_server::observability::notice=off")`: in a subscriber built from it over a `Buffer`, as `a_debug_filter_drops_rmcp_debug_events` does, the notice canary is still written).
- M1's other tests are unchanged and still hold.

**Commit:** `Let the telemetry notice through the stdout filter`

---

### TL4.8 — No heartbeat from test scripts

**Goal.** Every script that starts a cetra server for a test sets `CETRA_TELEMETRY=off`, and `just check-packaging-lint` fails if one does not.

**Edit:**
- `tools/packaging/binary-smoke.sh`
- `tools/packaging/image-smoke.sh`
- `tools/packaging/compose-smoke.sh`
- `tools/packaging/helm-smoke.sh`
- `tools/packaging/telemetry-off-check.sh` (new, executable)
- `tools/e2e/run.sh`
- `justfile` (the `check-packaging-lint` recipe only)
- `deploy/compose/cetra.env.example`

**Steps.**
1. `binary-smoke.sh`: add the line `CETRA_TELEMETRY=off \` right before `CETRA_DATA_DIR="$dir/data" \` in the server's environment prefix.
2. `image-smoke.sh`: add `-e CETRA_TELEMETRY=off \` right after `-p 127.0.0.1::8080 -p 127.0.0.1::4318 \` in `docker run`.
3. `compose-smoke.sh`: add the line `CETRA_TELEMETRY=off` to the `cetra.env` heredoc, right after `CETRA_PUBLIC_URL=…`.
4. `helm-smoke.sh`: add `--set-string secret.values.CETRA_TELEMETRY=off \` right after `--set persistence.size=1Gi \`.
5. `tools/e2e/run.sh`: add `CETRA_TELEMETRY=off \` right before `CETRA_DATA_DIR="$dir/data" \` (a debug build has the heartbeat off already; the variable makes the rule visible and survives a switch to a release build).
6. Create `tools/packaging/telemetry-off-check.sh` (mode 755: `chmod +x` before `git add`) with exactly:
   ```bash
   #!/usr/bin/env bash
   # Fails when a script that starts a cetra server for a test does not set CETRA_TELEMETRY=off
   # (track TL M4): release binaries in CI containers and kind pods do not see the runner's CI
   # variable, so the switchboard's CI rule cannot turn the heartbeat off for them.
   set -euo pipefail

   root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
   status=0
   for script in \
     tools/packaging/binary-smoke.sh \
     tools/packaging/image-smoke.sh \
     tools/packaging/compose-smoke.sh \
     tools/packaging/helm-smoke.sh \
     tools/e2e/run.sh; do
     # An assignment, not a comment: `NAME=off \`, `-e NAME=off`, `secret.values.NAME=off` or a heredoc line.
     if ! grep -Eq '^[[:space:]]*(-e |--set-string secret\.values\.)?CETRA_TELEMETRY=off([[:space:]]|$)' "$root/$script"; then
       echo "telemetry-off-check: $script does not set CETRA_TELEMETRY=off" >&2
       status=1
     fi
   done
   exit "$status"
   ```
7. `justfile`, recipe `check-packaging-lint`: add the line `    tools/packaging/telemetry-off-check.sh` right after `    pnpm exec biome check tools/packaging`.
8. `deploy/compose/cetra.env.example`: append, after the licence key block, a blank line and:
   ```
   # The anonymous daily heartbeat to telemetry.cetra.sh (https://cetra.sh/docs/self-hosting/telemetry/).
   # Uncomment to turn it off.
   #CETRA_TELEMETRY=off
   ```

**Tests.** By hand (read-only, no server): `bash tools/packaging/telemetry-off-check.sh` exits 0; `bash -n` on each edited script exits 0. The coordinator's `just check-packaging-lint` runs the check.

**Commit:** `Turn the heartbeat off in test scripts`

---

### TL4.9 — The Telemetry docs page

**Goal.** A Telemetry page under self-hosting that lists every field and every way to turn the heartbeat off, linked from the sidebar, the overview and the security checklist.

**Edit:**
- `website/src/content/docs/docs/self-hosting/telemetry.md` (new)
- `website/astro.config.mjs`
- `website/src/content/docs/docs/self-hosting/security.md`
- `website/src/content/docs/docs/self-hosting/index.md`
- `website/src/content/docs/docs/self-hosting/backup.md` (§ Restore mode: its intro sentence and its list)
- `website/src/content/docs/docs/self-hosting/configuration.md` (the `restore_mode` row only)
- `website/src/marketing/home/DataOwnership.astro` (one card)
- `website/src/pages/security.astro` (one card)

**Steps.**
1. Create `telemetry.md` with exactly this content (it follows `website/STYLE.md`: "cetra" lower case everywhere, no marketing words, tables for reference):
   ````markdown
   ---
   title: "Telemetry"
   description: "What a self-hosted cetra sends to us, field by field, and how to turn it off."
   ---

   This page lists what a self-hosted cetra sends to us and how to turn it off.

   cetra sends one anonymous heartbeat a day to `telemetry.cetra.sh`. It tells us which versions run, on which platforms, at what scale, and which features are in use. It carries no names, emails, domains, URLs, host names, licence data or exact counts.

   ## Turn it off

   Any one of these turns the heartbeat off:

   | Setting | Example |
   |---|---|
   | `CETRA_TELEMETRY` set to `off`, `false` or `0` (any case) | `CETRA_TELEMETRY=off` |
   | `DO_NOT_TRACK` set to anything but `0` or `false` | `DO_NOT_TRACK=1` |
   | `telemetry.heartbeat = false` | `CETRA_TELEMETRY__HEARTBEAT=false` |

   Any other value of `CETRA_TELEMETRY` stops cetra at startup with an error, so a typo never leaves the heartbeat on. An empty value is ignored. `CETRA_TELEMETRY` is not the environment form of a setting: `CETRA_TELEMETRY=on` is an error too.

   With Docker Compose, add `CETRA_TELEMETRY=off` to `cetra.env`. With the Helm chart, add it to `env` in your values:

   ```yaml
   env:
     - name: CETRA_TELEMETRY
       value: "off"
   ```

   The heartbeat is also off, whatever the settings say:

   - in [restore mode](/docs/self-hosting/backup/#restore-mode-restore-drills);
   - in a CI environment: `CI` set to anything but `false` or `0`, or any of `GITHUB_ACTIONS`, `GITLAB_CI`, `BUILDKITE`, `CIRCLECI`, `JENKINS_URL`, `TF_BUILD`, `TRAVIS`, `TEAMCITY_VERSION`, `BITBUCKET_BUILD_NUMBER`, `CODEBUILD_BUILD_ID`, `DRONE`, `APPVEYOR` or `SEMAPHORE` set;
   - in a debug build (built without `--release`).

   `npx @cetra-ai/cli init` can start a local `cetra serve` for you. It is a server like any other, so it sends the heartbeat too; it inherits your shell's environment, so `export CETRA_TELEMETRY=off` before you run `init` turns it off there.

   Turning the heartbeat off changes nothing else, and licences never turn it on or off. Licence keys are checked offline.

   ## The startup line

   At every start, cetra logs one line, at `info` with the target `cetra_server::observability::notice`, that says whether the heartbeat is on. It shows even when `log.level` is `warn`. When the heartbeat is on, the line gives the instance id and a link to this page. When it is off, its `reason` field says why: `CETRA_TELEMETRY`, `DO_NOT_TRACK`, `restore_mode`, `ci`, `debug_build` or `config`.

   ## When it is sent

   The first heartbeat goes 10 to 20 minutes after the server starts, then one every 24 hours. cetra records in `meta.db` when the last heartbeat was accepted, so restarts send at most one a day.

   The request is `POST https://telemetry.cetra.sh/v1/heartbeat` with a JSON body and the header `User-Agent: cetra/<version>`. It follows no redirect and gives up after 10 seconds. It goes through the proxy in `HTTPS_PROXY` when one is set, except for hosts in `NO_PROXY`. A heartbeat that fails is not retried before the next one, and failures are logged at `debug` only, so a server without internet access logs no warnings. To send it through a relay of your own, set [`telemetry.endpoint`](/docs/self-hosting/configuration-reference/#telemetry).

   ## What it sends

   | Field | Values | Meaning |
   |---|---|---|
   | `schema` | `1` | The version of this format. |
   | `instance_id` | a random UUID | The instance id (below). |
   | `version` | `0.1.0` | The cetra version. |
   | `target` | `x86_64-unknown-linux-musl` | The platform the binary was built for. |
   | `profile` | `release`, `debug` | The build profile. |
   | `deployment` | `docker`, `kubernetes`, `binary` | How cetra runs: a container, a Kubernetes pod, or the binary on a host. |
   | `storage` | `file`, `s3` | Where Parquet files live. |
   | `uptime` | `<1h`, `1h-1d`, `1d-7d`, `7d-30d`, `30d+` | How long the server process has run, measured when the heartbeat is sent. |
   | `install_age` | `<1d`, `1d-7d`, `7d-30d`, `30d-90d`, `90d-1y`, `1y+` | How long ago the first account was created. |
   | `orgs`, `projects` | `0`, `1`, `2-5`, `6-20`, `21-100`, `101-1000`, `1000+` | Active organizations and projects. |
   | `users` | the same buckets | Accounts that can sign in. |
   | `spans_per_day`, `scores_per_day` | `0`, `<1k`, `<10k`, `<100k`, `<1M`, `<10M`, `<100M`, `100M+` | Spans and scores received yesterday (UTC), all organizations together. |
   | `features.email` | `true`, `false` | Email (SMTP) is configured. |
   | `features.github_sign_in`, `features.google_sign_in`, `features.oidc_sign_in` | `true`, `false` | Sign-in with GitHub, Google or an OIDC provider is configured. |
   | `features.alert_rules`, `features.online_rules`, `features.datasets`, `features.prompts`, `features.annotation_queues` | `true`, `false` | At least one alert rule, online rule, dataset, prompt or annotation queue exists. |
   | `features.self_monitoring` | `true`, `false` | cetra exports its own logs to an OTLP endpoint of yours. |

   An example:

   ```json
   {
     "schema": 1,
     "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
     "version": "0.1.0",
     "target": "x86_64-unknown-linux-musl",
     "profile": "release",
     "deployment": "docker",
     "storage": "file",
     "uptime": "<1h",
     "install_age": "30d-90d",
     "orgs": "1",
     "projects": "2-5",
     "users": "6-20",
     "spans_per_day": "<100k",
     "scores_per_day": "<1k",
     "features": {
       "email": true,
       "github_sign_in": false,
       "google_sign_in": true,
       "oidc_sign_in": false,
       "alert_rules": true,
       "online_rules": true,
       "datasets": false,
       "prompts": true,
       "annotation_queues": false,
       "self_monitoring": false
     }
   }
   ```

   It never contains emails, email domains, names of organizations, projects or anything else, URLs, host names, IP addresses, organization or project ids, licence keys or licence state, or exact counts. Our relay sees the address the request comes from, as any server does, and does not store it.

   ## The instance id

   The instance id is a random UUID that cetra creates in `meta.db` the first time it sends a heartbeat. Nothing about your machine goes into it. A backup and its restore keep it, since they are the same instance; a copy of the data directory made by hand shares it with the original. The startup line shows it. To have us delete the heartbeats of an instance, write to `hello@cetra.sh` with its id.
   ````
2. `astro.config.mjs`: in the Self-hosting "Configure" group, add `{ label: "Telemetry", link: "/docs/self-hosting/telemetry/" },` right after the `Security` entry.
3. `security.md`, section "Outbound requests": add a last item:
   ```markdown
   - [ ] **Decide about the heartbeat.** cetra sends one anonymous heartbeat a day to `telemetry.cetra.sh`. Allow that host in your egress rules, or set `CETRA_TELEMETRY=off`. See [Telemetry](/docs/self-hosting/telemetry/).
   ```
4. `index.md`, the "Then:" list: add after the Monitoring bullet:
   ```markdown
   - [Telemetry](/docs/self-hosting/telemetry/): the anonymous daily heartbeat, field by field, and how to turn it off.
   ```

5. `backup.md`, § Restore mode. Append only; M1's TL1.7 wrote the text quoted here, and if it differs, stop and report.
   1. The intro sentence after TL1.7 ends with `starts a server that makes no outbound call, except the self-monitoring export to your own endpoint ([Monitoring](/docs/self-hosting/monitoring/#export-logs-over-otlp)).` Right after that sentence (same paragraph), add: `It sends nothing to us either: the heartbeat and crash reports are off in restore mode ([Telemetry](/docs/self-hosting/telemetry/)).`
   2. The list under `With restore mode on, the server:` ends, after TL1.7, with `- keeps exporting its own logs to \`self_monitoring.otlp_endpoint\`, when it is set: that endpoint is yours.` Add after it, as the new last bullet:
      ```markdown
      - sends no heartbeat and no crash report to us (see [Telemetry](/docs/self-hosting/telemetry/)).
      ```
   (Crash reports arrive in M5; the sentence holds from now on, since the switchboard turns both off in restore mode.)
5b. `configuration.md`, the `restore_mode` row. After TL1.7 it reads:
   ```markdown
   | `restore_mode` | `CETRA_RESTORE_MODE` | `false` | no outbound calls except the self-monitoring export, for restore drills ([Backup and restore](/docs/self-hosting/backup/#restore-mode-restore-drills)) |
   ```
   Change it to (one clause appended after `export`; the link kept):
   ```markdown
   | `restore_mode` | `CETRA_RESTORE_MODE` | `false` | no outbound calls except the self-monitoring export, and nothing sent to us (heartbeat, crash reports), for restore drills ([Backup and restore](/docs/self-hosting/backup/#restore-mode-restore-drills)) |
   ```
   If the row differs from the "after TL1.7" text, stop and report.
6. `website/src/marketing/home/DataOwnership.astro`: replace the card
   ```astro
       <Card title="Nothing phones home">
         cetra sends no usage data. A licence key is checked offline.
       </Card>
   ```
   with (same title; only the false sentence changes)
   ```astro
       <Card title="Nothing phones home">
         Self-hosted cetra sends only an <a href="/docs/self-hosting/telemetry/">anonymous daily heartbeat</a>, which you can turn off. A licence key is checked offline.
       </Card>
   ```
7. `website/src/pages/security.astro`, the "Self-hosted" card: replace
   ```astro
           <p>Traces go from your app to your server. cetra sends no usage data anywhere, and a licence key is verified offline.</p>
   ```
   with
   ```astro
           <p>Traces go from your app to your server. Self-hosted cetra sends only an <a href="/docs/self-hosting/telemetry/">anonymous daily heartbeat</a>, which you can turn off, and a licence key is verified offline.</p>
   ```
   Keep the surrounding markup and indentation as they are; if the card's text differs from the quoted one, stop and report.

**Tests.** By hand: the page's description is under 160 characters; it contains no `Cetra`/`CETRA` word outside code, no `simply`, `powerful`, `seamless`. `just check-docs` (coordinator) runs `check-terms.mjs`, `check-env-vars.mjs` (`CETRA_TELEMETRY` is read by `switchboard.rs`; `CETRA_TELEMETRY__HEARTBEAT` is the documented key `telemetry.heartbeat` after TL4.3) and the link validator (`#telemetry` exists after TL4.3; `#restore-mode-restore-drills` exists in `backup.md`).

**Commit:** `Document the telemetry heartbeat`

---

### TL4.10 — README, CHANGELOG, specs and the requests to Q

**Goal.** The README line, the CHANGELOG entry with its upgrade note, spec 04's keys, spec 12's config rows and switchboard, D-W-18's amendment, and the requests to track Q.

**Prerequisite:** M1's TL1.7 is committed (it edits `CHANGELOG.md`, spec 12 and `docs/mvp-decisions.md` first).

**Edit:**
- `README.md`
- `CHANGELOG.md`
- `docs/spec/04-metadata-db.md`
- `docs/spec/12-operations.md`
- `docs/mvp-decisions.md` (the D-W-18 row only)
- `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/tl-requests-for-q.md` (new; outside the repo, not committed)

**Steps.**
1. `README.md`, § "Run it from source": after the paragraph that ends "Its data goes to `./cetra-data`.", add a paragraph:
   ```markdown
   A release build sends one anonymous heartbeat a day: the version, the platform, bucketed counts
   and which features are in use. Set `CETRA_TELEMETRY=off` to turn it off; every field is listed
   in [Telemetry](https://cetra.sh/docs/self-hosting/telemetry/).
   ```
2. `CHANGELOG.md`, under `## [Unreleased]`:
   - Add as the first subsection (before `### Added`; if an `### Upgrade notes` section exists already, append the bullet to it):
     ```markdown
     ### Upgrade notes

     - cetra now makes one outbound HTTPS request a day, an anonymous heartbeat to
       `https://telemetry.cetra.sh` (https://cetra.sh/docs/self-hosting/telemetry/). To turn it off,
       set `CETRA_TELEMETRY=off` before you upgrade; otherwise allow that host in your egress rules.
       A `CETRA_TELEMETRY` value other than `off`, `false` or `0` now stops cetra at startup.
     ```
   - Append to `### Added`:
     ```markdown
     - An anonymous heartbeat once a day to `telemetry.cetra.sh`: a random instance id, the version and
       platform, bucketed counts and which features are in use, never names, emails, hosts or exact
       counts. `CETRA_TELEMETRY=off`, `DO_NOT_TRACK=1` or `telemetry.heartbeat = false` turn it off; it
       is off in restore mode, in CI and in debug builds. A startup line says whether it is on.
     ```
3. `docs/spec/04-metadata-db.md`, the `instance_settings` block (lines 994–996): add after the `'license.key'` comment line:
   ```sql
   -- 'telemetry.instance_id': random UUIDv4 of the instance for the heartbeat and crash reports, created on first use with an insert that keeps an existing row (D-TL-9)
   -- 'telemetry.last_heartbeat_at': i64 ns of the last heartbeat the relay accepted, as decimal text (D-TL-8)
   ```
4. `docs/spec/12-operations.md`:
   1. § Configuration, first paragraph: after the sentence about `CETRA_URL`, `CETRA_TOKEN` and `CETRA_PROJECT`, add: "`CETRA_TELEMETRY` is not a config key either: the loader reads it before the sources (`off`, `false` or `0` in any case; empty is ignored; any other value is a load error) and does not map it (D-TL-11)."
   2. The `restore_mode` row. Append only. After M1's TL1.7 step 2.3 it reads:
      ```markdown
      | `restore_mode` | | `false`. No outbound work: alert deliveries, webhooks, judges, the assistant, email and identity providers are off; for restore drills (D-N-25) The self-monitoring export to the operator's own endpoint stays on (D-TL-1). |
      ```
      Append ` Nothing is sent to us: the heartbeat and crash reports are off (D-TL-11).` at the end of the third cell, before its closing `|`, so it reads:
      ```markdown
      | `restore_mode` | | `false`. No outbound work: alert deliveries, webhooks, judges, the assistant, email and identity providers are off; for restore drills (D-N-25) The self-monitoring export to the operator's own endpoint stays on (D-TL-1). Nothing is sent to us: the heartbeat and crash reports are off (D-TL-11). |
      ```
      If the row differs from the first text (for example TL1.7 punctuated it differently), keep TL1.7's text as it is and only append the clause; report the difference.
   3. Add two rows right after M1's `self_monitoring.*` rows (which follow the `log.format` / `log.level` row; if they are missing, after the `log.format` / `log.level` row):
      ```markdown
      | `telemetry.heartbeat` | | `true`. The anonymous daily heartbeat (D-TL-8); the switchboard below can turn it off |
      | `telemetry.endpoint` | | `"https://telemetry.cetra.sh"` (D-TL-10). `https`, or `http` to a loopback IP; no user info, query or fragment; the heartbeat goes to `<endpoint>/v1/heartbeat` |
      ```
   4. The TLS line under the configuration table. It holds one of these two texts:
      - the original: `cetra has no \`tls.*\` keys and no telemetry key: the loader refuses them as unknown keys.`
      - after M1's TL1.7 step 3.2: `cetra has no \`tls.*\` keys: the loader refuses them as unknown keys. Its own logs are exported with \`[self_monitoring]\` (§ Self-monitoring, D-TL-1).`

      If it holds neither, stop and report. Replace whichever is there (keep the leading `TLS: terminated by a reverse proxy in the reference setups.`) with exactly:
      ```markdown
      cetra has no `tls.*` keys: the loader refuses them as unknown keys. Its own logs are exported with `[self_monitoring]` (§ Self-monitoring, D-TL-1); `[telemetry]` controls what is sent to us (§ Telemetry to us, D-TL-1).
      ```
   5. § Process lifecycle, the Startup bullet contains `the hourly retention task, D-M-3, D-M-10) → bind ports`. Replace that text with `the hourly retention task, D-M-3, D-M-10) → decide what may go to us, log the telemetry notice, and start the heartbeat task when the switchboard allows it (D-TL-11) → bind ports`.
   6. At the end of § Self-monitoring (after M1's text if present), add:
      ```markdown
      ### Telemetry to us (track TL M4)

      One switchboard decides at startup what may go to `telemetry.endpoint`; the first match wins (D-TL-11): `CETRA_TELEMETRY` is `off`, `false` or `0` → off; `DO_NOT_TRACK` non-empty and not `0`/`false` → off; `restore_mode` → off; for the heartbeat, a debug build or a CI environment (`CI` non-empty and not `false`/`0`, or any of `GITHUB_ACTIONS`, `GITLAB_CI`, `BUILDKITE`, `CIRCLECI`, `JENKINS_URL`, `TF_BUILD`, `TRAVIS`, `TEAMCITY_VERSION`, `BITBUCKET_BUILD_NUMBER`, `CODEBUILD_BUILD_ID`, `DRONE`, `APPVEYOR`, `SEMAPHORE` non-empty) → off; otherwise `telemetry.heartbeat`. The decision is total, and an error after it (the instance id, the queries, the request) means nothing is sent. Licences never change it (D172).

      The heartbeat (D-TL-8) is a fixed JSON type posted to `<endpoint>/v1/heartbeat` with `User-Agent: cetra/<version>`: `schema`, `instance_id`, `version`, `target`, `profile`, `deployment`, `storage`, `uptime` and `install_age` buckets, `orgs`/`projects`/`users` count buckets, yesterday's `spans_per_day`/`scores_per_day` from `usage_daily` in buckets, and feature booleans. The fields and buckets are listed in `website/src/content/docs/docs/self-hosting/telemetry.md`. The first one goes 10 minutes plus up to 10 minutes of jitter after start, then every 24 h; `instance_settings['telemetry.last_heartbeat_at']` keeps restarts to one a day. Its own HTTP client: no redirects, 10 s timeout, no retry before the next tick, failures at `debug`. The instance id is `instance_settings['telemetry.instance_id']`, a random UUIDv4 created on first use (D-TL-9).

      Startup notice: one `info` line under the target `cetra_server::observability::notice`, which the stdout filter always lets through: on, with the instance id and the docs URL, or off, with the reason. Every script that starts a server for a test sets `CETRA_TELEMETRY=off` (`tools/packaging/telemetry-off-check.sh`).
      ```
5. Write the Q notes file with exactly:
   ```markdown
   # Requests from track TL to track Q (2026-11-23)

   From `docs/plans/tl-telemetry/README.md` § Requests to track Q (design approved 2026-11-23). M4 (the heartbeat) ships with failures silent at `debug` until TL-Q1 exists.

   ## TL-Q1 — the relay at `telemetry.cetra.sh`

   - `POST /v1/heartbeat` and `POST /v1/crash`, JSON bodies (heartbeat: D-TL-8, crash: D-TL-5), `schema: 1`; an unknown schema answers 400.
   - Bodies at most 64 KiB. Rate limit per source address, without storing the address. Answer 2xx with an empty body. The server sends `User-Agent: cetra/<version>`, follows no redirect and times out after 10 s.
   - Heartbeats stored without the source address: one row per `instance_id` per day, kept 13 months, in the relay's own small database (queried with SQL; no product-analytics SaaS).
   - Crash reports forwarded to our error destination (Sentry EU, D-TL-13) as Sentry events so they group as issues, kept 90 days.
   - A way to delete an instance's data on request (users write to hello@cetra.sh with the instance id).
   - The heartbeat body of schema 1:
     ```json
     {"schema":1,"instance_id":"8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d","version":"0.1.0","target":"x86_64-unknown-linux-musl","profile":"release","deployment":"docker","storage":"file","uptime":"<1h","install_age":"30d-90d","orgs":"1","projects":"2-5","users":"6-20","spans_per_day":"<100k","scores_per_day":"<1k","features":{"email":true,"github_sign_in":false,"google_sign_in":true,"oidc_sign_in":false,"alert_rules":true,"online_rules":true,"datasets":false,"prompts":true,"annotation_queues":false,"self_monitoring":false}}
     ```
     M5 adds `features.crash_reports_on`; M6 adds `usage`. Accept unknown keys.
   - The exact strings the relay accepts (design § 5 and § Details settled while planning):

     | Field | Values |
     |---|---|
     | `profile` | `release`, `debug` |
     | `deployment` | `docker`, `kubernetes`, `binary` |
     | `storage` | `file`, `s3` |
     | `uptime` (time since the process started) | `<1h`, `1h-1d`, `1d-7d`, `7d-30d`, `30d+` |
     | `install_age` (from the oldest account) | `<1d`, `1d-7d`, `7d-30d`, `30d-90d`, `90d-1y`, `1y+` |
     | `orgs`, `projects`, `users` | `0`, `1`, `2-5`, `6-20`, `21-100`, `101-1000`, `1000+` |
     | `spans_per_day`, `scores_per_day` | `0`, `<1k`, `<10k`, `<100k`, `<1M`, `<10M`, `<100M`, `100M+` |
     | `features.*` | booleans |

   ## TL-Q2 — cells

   Each cell sets `self_monitoring.otlp_endpoint` and `self_monitoring.otlp_headers` (from a secret) to our destination, `self_monitoring.resource = "cetra.cell=<name>"`, `telemetry.heartbeat = false` (a cell is ours) and `telemetry.crash_reports = false` (from M5; its errors already reach us through self-monitoring). Before relying on Sentry, check that ERROR log records sent over OTLP show up as Sentry issues; if not, cells send to an OpenTelemetry Collector that forwards to Sentry.

   ## TL-Q3 — the control plane

   The control plane exports its own logs and traces the same way, so cloud errors are in one place.

   ## TL-Q4 — PostHog (for M6)

   A PostHog EU project; each cell sets `analytics.posthog_host` and `analytics.posthog_key` (from a secret); the control plane's own pages, if they want events, use the same catalog names; the cloud privacy policy names PostHog and Sentry as processors.
   ```

6. `docs/mvp-decisions.md`, the D-W-18 row: in its decision cell, replace `` `AWS_REGION` and `CETRA_RESTORE_MODE` are not aliases, `telemetry.*` and `tls.*` are refused, `` with `` `AWS_REGION` and `CETRA_RESTORE_MODE` are not aliases, `tls.*` is refused (`[telemetry]` exists since TL M4: amended by D-TL-1), ``. Nothing else in the row changes.

**Tests.** By hand: `git diff` shows only the listed repo files. The coordinator's `just check` covers the docs checks. The Q notes file is written but not committed; the coordinator hands it to track Q.

**Commit:** `Document the heartbeat in the README, CHANGELOG and specs` (the notes file is not committed).

---

## After the last phase (coordinator)

- Review each phase's diff against this plan, then run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just fmt && just check` once.
- A Sonnet code review of the milestone; fixes by a Haiku agent with an exact brief.
- Update the TL row of `docs/roadmap.md` when the track's status changes.
- Hand M3 the reuse points: `heartbeat::collect`, `switchboard::for_config` and `Decision`, `facts::*`; M3 adds a read-only `instance::existing_instance_id` (a `get_setting` that never creates the row). Hand M5: `shared_off`, `Inputs`, `Decision` (M5 adds `crash_reports`), `Features` (M5 adds `crash_reports_on`), `NOTICE_TARGET`, `instance::instance_id`, `facts::mark_process_start` and `facts::process_uptime`, `TelemetryConfig` (M5 adds `crash_reports`), the golden JSON. Hand M6: `Heartbeat` (M6 adds `usage`).
