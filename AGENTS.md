# AGENTS.md

Instructions for any agent (Claude, Cursor, Codex, …) working in this repository. `CLAUDE.md` is a symlink to this file.

## Project

**llmobs** (working name) is an open-source LLM observability and evaluation platform, written in Rust. It competes with Langfuse, Phoenix, Opik, LangWatch, Helicone, Laminar and MLflow GenAI.

What sets it apart is that it runs as **one binary on one machine**, while staying fast at volume:
- OTLP ingestion;
- a custom storage engine (WAL → memtable → Parquet files, queried with Apache DataFusion);
- a SQLite metadata DB;
- a React UI embedded in the binary;
- Python and TypeScript SDKs built on OpenTelemetry.

The project is unreleased. There are no users, no stored data and no API clients to keep compatible.

## Source of truth

Read these before writing code, and follow them:

| Document | What it is |
|---|---|
| `docs/mvp-decisions.md` | Every product/technical decision (D1, D2, …) and the MVP scope summary |
| `docs/spec/README.md` → `docs/spec/01…12-*.md` | The technical specification: data model, ingestion, storage, metadata DB, query, evaluation, alerts, auth, API, SDK, UI, operations |
| `docs/roadmap.md` | Phases, waves and tracks: each track's plan, status, PRs and decision IDs |
| `docs/plans/` | Step-by-step implementation plans; do the sub-phase you were given, nothing more |
| `docs/research/competitors.md` | Why the decisions were made (competitor features and code-level findings) |

If the code has to deviate from the spec, update the spec **in the same change**, and add a decision to `docs/mvp-decisions.md` if the change is not a detail. A track's decisions are named `D-<track>-<n>` (e.g. `D-G-1`) and go in that track's section; only the coordinating session adds plain-numbered decisions (`docs/roadmap.md` § Decision IDs). If the spec is ambiguous or contradicts itself, stop and ask instead of guessing.

## Rules

**No compatibility layers.** When a type, function, format, config key or protocol field changes, update every caller and delete the old path in the same change. No shims, aliases, `#[deprecated]` wrappers, versioned codecs, feature flags or dual read/write paths. Entitlement checks (what a plan or licence allows, D179) are product behaviour, not feature flags.

**No unnecessary abstractions.**
- Prefer concrete types and plain functions.
- A trait needs at least two implementations (test doubles don't count unless the test truly can't use the real type).
- No wrapper whose only job is to forward to the type it holds.
- Add a generic, newtype or module split only when it removes real duplication or enforces a real invariant.

**No placeholders.** Don't add stub modules, empty types, `todo!()` functions or CLI subcommands for future work. Code is added together with its implementation and tests.

**Dependencies.**
- Every version is declared once in the root `Cargo.toml` `[workspace.dependencies]`; crates use `dep = { workspace = true }`.
- `datafusion`, `arrow`, `parquet` and `object_store` are a pinned family: never bump one without the others. DataFusion picks the majors. Dependabot ignores arrow-side majors; a DataFusion major PR is completed by hand with the arrow, parquet and object_store versions that release requires. `cargo deny` fails on two versions of a family crate.
- Look up real versions (`cargo search`, `cargo info`); never guess one.
- JS packages use exact versions and respect `minimumReleaseAge`; Python dev tools are pinned by `uv.lock`.
- `typescript` stays on 5.x while `openapi-typescript` requires it (`.github/dependabot.yml` ignores TypeScript majors).

**Errors.** Each library crate defines its own error enum(s) with `thiserror`. `anyhow` is only allowed in `llmobs-cli`.

**Safety and robustness.**
- `unsafe` is forbidden workspace-wide.
- No `unwrap()` / `expect()` outside tests, unless a comment next to it states the invariant that makes it infallible.
- Never panic on input coming from the network or from disk; return an error.

**Data conventions** (spec README § Cross-cutting conventions):
- Time is `i64` nanoseconds since the Unix epoch, UTC.
- Entity IDs are UUIDv7.
- Trace IDs are 16 bytes and span IDs 8 bytes, written as lowercase hex in JSON.
- Money is USD as `f64`.

**Privacy.** Logs and error messages never contain payload content (prompts, completions, attribute values), secrets, API keys or email addresses. Log IDs and sizes instead.

**Tests.**
- Every behaviour change comes with tests.
- Normalization uses golden fixtures in `fixtures/otlp/`.
- Storage needs crash-recovery and concurrency tests.
- `LLMOBS_CRASH_ITERATIONS=N just test` runs the storage crash harness longer (default 25).
- `LLMOBS_LOSSY_SEEDS=N just test` runs each lossy crash point with N seeds (default 1).
- `just lazyfs` runs the crash children and a short ingest on a LazyFS mount, and drops unsynced data at each crash (D-R-5). It needs FUSE 3 and `user_allow_other` (`tools/lazyfs/README.md`).
- `LLMOBS_HOOK_CRASH_CASES=N just test` runs N model cases that crash inside their last operation (default 25; the last of the 26 pairs is not in the default run).
- `LLMOBS_MODEL_CASES=N` sets the case count of the storage model test (`model_tests::storage_matches_the_model`, default 8); `just model N` runs it in release mode. `model_tests::storage_matches_the_model_in_interval_mode` (the `wal.sync` interval variant) runs `max(N/2, 2)` cases, and `just model` matches it too, since the filter is a substring. It is separate from `LLMOBS_PROPTEST_CASES`, which sets the count of the cheap property tests of otel and eval: one model case costs about a second, so one variable for both would make a high count unusable.
- `LLMOBS_MODEL_S3_CASES=N` sets the case count of the S3-mode model test (`model_tests::storage_matches_the_model_on_s3`, default 8: six debug runs took 11 s to 32 s); `just model-s3 N` runs it in release mode with the statistics on.
- `LLMOBS_TEST_S3_ENDPOINT` (with `LLMOBS_TEST_S3_BUCKET` and the `AWS_*` credentials) runs the storage S3 test against a real S3-compatible server; `crates/storage/tests/s3.rs` shows how to start one (D119).
- `LLMOBS_TEST_S3_REGION` and `LLMOBS_TEST_S3_PATH_STYLE` adapt it to a remote provider (R M4).
- `LLMOBS_MODEL_S3_REMOTE=1` runs the S3-mode model test against the `LLMOBS_TEST_S3_*` bucket, in a prefix of its own, and deletes the prefix's objects after each case (`just model-s3 N` with it; R M4).
- `crates/storage/tests/s3_probe.rs` probes what our S3 code relies on against a real provider. Its tests are `#[ignore]`d and skip without `LLMOBS_TEST_S3_ENDPOINT`: `cargo test --release -p llmobs-storage --all-features --test s3_probe -- --ignored --nocapture` (R M4).
- `LLMOBS_TEST_TOXIPROXY` (with `LLMOBS_TEST_S3_*` and optionally `LLMOBS_TEST_TOXIPROXY_TOKEN`) runs `just s3-chaos`; `crates/storage/tests/s3_chaos.rs` shows how to start toxiproxy and Versity Gateway.
- Tests cover the current behaviour only.
- UI tests use the MSW mocks from `fixtures/api/`; a page test overrides an endpoint with `server.use` instead of mocking modules.
- Captured OTLP fixtures are recorded with `just capture` (tools/capture) and never edited by hand; `just check-capture` before committing one.

**Reference implementations.** The competitors' source code is checked out in `/home/sebastien/github/{lmnr,phoenix,langfuse,langwatch,helicone,opik,mlflow}`. Read it to compare designs and learn from their mistakes. Never copy code from it.

## Layout

```
crates/
  core/      llmobs-core     shared contracts: IDs, enums, names, messages, Span/Score,
                             Arrow schemas + batch conversion, OpenAPI schemas,
                             no config (D59)                                          (spec 01)
  otel/      llmobs-otel     OTLP decode, normalization, media, truncation, cost      (spec 02)
  eval/      llmobs-eval     evaluator execution: checks, mapping, templates, judges,
                             webhooks, judge traces                                   (spec 06)
  storage/   llmobs-storage  WAL, memtable, Parquet, catalog (catalog.db), compaction,
                             deletion, DataFusion providers, filter DSL, SQL sandbox  (specs 03, 05)
  token/     llmobs-token    signed licence, org and control-plane tokens (Elastic-2.0, D-P-26)
  server/    llmobs-server   API, auth, metadata DB, evaluation, alerts, email, MCP   (specs 04, 06–09)
  cli/       llmobs-cli      the `llmobs` binary                                       (spec 09 § CLI)
  ui/        llmobs-ui       the web UI embedded at build time (D-G-18, D-G-35)
web/                         React UI (Vite, TanStack, shadcn/ui, pnpm)               (spec 11)
sdks/python/                 Python SDK (uv)                                           (spec 10)
sdks/typescript/             TypeScript SDK (pnpm)                                     (spec 10)
fixtures/otlp/               OTLP fixtures shared by Rust golden tests and SDK tests  (spec 02 § Testing)
fixtures/api/                example API responses (UI mocks), checked against the server types (spec 09)
tools/                       dev tools (fixture capture, benchmarks); not shipped
tools/capture/               fixture capture: mock provider, OTLP receiver, scenarios (B2)
tools/e2e/                   run.sh for just e2e: a real llmobs serve and both SDK suites (D-G-20)
docs/                        decisions, specs, plans, research
```

**Crate dependency graph (enforced):**
- `core` depends on no workspace crate.
- `otel` and `storage` depend only on `core`. **They never depend on each other**, so they can be built in parallel; `server` connects them.
- `eval` depends on `core` and `otel`.
- `token` depends on no workspace crate. `server` and `tools/license` use it, and so does the control plane (by git commit).
- `server` depends on `core`, `otel`, `storage`, `eval` and `token`; `cli` depends on `server`, `core` and `ui`.
- `ui` depends on no workspace crate.
- A `tools/` crate is never a dependency of a shipped crate. It may be a dev-dependency, for tests only: `llmobs-cli`'s end-to-end test uses `llmobs-capture`'s mock provider (D-I-43).

## Parallel work and ownership

Several sessions may work at the same time, each in its own git worktree and branch.

- **Only edit the directories your brief names.** If you need a change elsewhere, say so in your report instead of making it.
- **`llmobs-core` is a shared contract.** Don't change its public types unless your brief allows it. Request the change, and the coordinating session makes it.
- The same goes for the root `Cargo.toml`: add a dependency only when your work needs it. Never change existing pins unless your brief says so.
- Keep changes on your own branch; don't merge other branches into it.
- A PR that changes a track's status updates its row in `docs/roadmap.md`.

## Commands

Run everything through `just`:

| Command | What it does |
|---|---|
| `just check` | Rust, web, Python SDK, and TypeScript SDK (D79); **must pass before every commit** |
| `just check-rust` | format check, clippy (`-D warnings`), tests, cargo-deny |
| `just check-web` | web lint, types, tests, production build |
| `just check-release` | build-release, then check the UI is embedded |
| `just check-py` | Python SDK format, lint, types, tests |
| `just check-ts` | TypeScript SDK lint, types, tests, package exports |
| `just fmt` | format Rust, the web and TypeScript SDK (Biome), and the Python SDK (ruff) |
| `just lint` | clippy, warnings as errors |
| `just test` | all Rust tests |
| `just model [cases]` | the storage model test with more cases, in release mode (D-R-14) |
| `just model-s3 [cases]` | the storage model test on a remote store with injected S3 faults, in release mode (R M4) |
| `just s3-chaos` | the S3 fault tests through toxiproxy in front of an S3 endpoint (R M4) |
| `just lazyfs` | the storage crash tests and a short ingest on a LazyFS mount, which drops unsynced data at each crash (R2.12, D-R-5); needs FUSE 3 (`tools/lazyfs/README.md`) |
| `just eval-live` | opt-in live provider tests; ignored by `just check` (D167) |
| `just deny` | supply-chain checks (advisories, bans, licences, sources; D-P-3) |
| `just check-ee` | the `ee` licence files and SPDX headers (D-P-2) |
| `just deny-advisories` | RustSec advisories only |
| `just assert-clean` | fail if the working tree changed; CI runs it after each job |
| `just ts-smoke <tarball>` | load a packed TS SDK from ESM and CommonJS |
| `just check-workflows` | lint and audit GitHub Actions workflows (actionlint, zizmor) |
| `just openapi` | regenerate `crates/server/openapi.json` and `web/src/api/schema.gen.ts` |
| `just web-dev` | web dev server with the API mocked from `fixtures/api` |
| `just web-dev-server` | web dev server proxied at the real API |
| `just run <args>` | run the `llmobs` binary |
| `just capture <group>/<case>` | record an OTLP fixture from a scenario, e.g. `just capture openinference/openai-chat-tool` |
| `just capture-all` | recapture every scenario |
| `just check-capture` | lint and type-check the capture scenarios, then recapture every scenario and compare (timing excluded) |
| `just e2e` | build the web, run the Rust e2e tests with the UI embedded, then the Python and TypeScript SDK suites against a real `llmobs serve` |
| `just dist <target>` | release archive of one target into `target/dist` (and the image context on Linux) |
| `just image [tag]` · `just image-smoke [tag]` | build and smoke-test the Docker image from `target/dist` |
| `just compose-smoke [tag] [s3]` · `just helm-check` · `just helm-smoke [tag]` · `just install-smoke` | packaging smoke tests (D-N-13) |
| `just versions-check` | the chart and compose files carry the workspace version |
| `just release-prep <version>` | set a release version everywhere and date the changelog (D-N-5) |
| `just check-packaging-lint` | cheap packaging checks (versions, chart, compose files, Biome); CI's `packaging` job (D-N-13) |
| `just check-packaging` | full packaging checks on x86_64 Linux (archive, image, compose, chart, install script); `packaging.yml` on main and weekly (D-N-13) |

Required tools:
- `rustup` (the toolchain is pinned in `rust-toolchain.toml`)
- `just`
- `cargo-deny` (`cargo install cargo-deny --locked`)
- Node (version in `.node-version`)
- `pnpm` (version in the root `package.json` `packageManager`)
- `uv`
- `actionlint` and `zizmor` (only when editing `.github/`)
- for packaging (`just dist`, `just check-packaging`): `musl-tools` (Linux), `cargo-auditable` 0.7.7, `cargo-about` 0.9.3 (`--features cli`), Docker Engine 25+ with Compose 2.24+, helm 4.3.1, kubeconform 0.8.0; kind 0.33.0 and kubectl for `just helm-smoke`

## CI

`.github/workflows/ci.yml` runs on every pull request and every push to `main`. `ci-ok` is the required status.

CI runs the `just` recipes from the table. A branch that passes `just check` locally and leaves a clean tree passes CI, except for the runtime matrix (Python 3.15, Node 22.12). The `packaging` job runs `just check-packaging-lint`; the full `just check-packaging` runs in `packaging.yml` on pushes to `main` and weekly, outside `ci-ok` (D-N-13).

Actions are pinned by commit SHA with the tag in a comment; Dependabot updates them. Never use a tag or branch reference.

Workflow `run:` steps call `just` recipes. New checks go into the `justfile` first.

Updated by hand: `rust-toolchain.toml`, `.node-version`, `packageManager`.

## Naming

Crates are prefixed `llmobs-`. The binary is `llmobs`, env vars use the `LLMOBS_` prefix, and span attributes use the `llmobs.*` namespace. API keys are `sk-llmobs-…`, `pk-llmobs-…` and `pat-llmobs-…`.

All of these will be renamed mechanically once the product has its real name (D46). Keep the name in as few places as practical, e.g. one constant for the attribute prefix.

## Commits and reports

- Small commits with a short imperative subject (e.g. `Add WAL group commit`). One sub-phase gives one commit unless the plan says otherwise.
- Don't commit generated artifacts (`target/`, `node_modules/`, `dist/`, `.venv/`).
- A user-visible change adds a line to `CHANGELOG.md` § `[Unreleased]` (`Added`, `Changed`, `Fixed`, `Removed`); one that needs action on upgrade goes under `### Upgrade notes` (D-N-5).
- When you finish, report:
  - what was done;
  - any deviation from the plan or spec, and why;
  - the exact versions you chose for any new dependency;
  - anything left undone or unverified.
