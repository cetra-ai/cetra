# P · M4 — The data window, the retention cap and the gates

Milestone 4 of track P ([`00-design.md`](00-design.md)). It builds on M3 ([`03-m3-units-and-limits.md`](03-m3-units-and-limits.md), merged), on track K (merged), on track M's M6 (merged) and, for its last two sub-phases only, on track I's M3 (not merged when this plan was written).

When M4 is done:
- a cell trusts org tokens and control-plane tokens only when `entitlements.control_plane = true`; otherwise every `/api/internal/v1/*` route answers 404 (D-P-50);
- every storage read the server makes goes through `entitlements::query_options(visibility, around)`, and data older than the org's `data_access_days` is hidden from the UI, the API, the SQL console and exports, MCP, streams, alerts and evaluators (D175, D-P-21, D-P-44, D-P-47); deletion reads are exempt;
- a media blob is hidden when every span that referenced it is hidden (D-P-21);
- track M's hourly retention caps each org at the largest window of the last 30 days plus 30 days, and ingest's D85 age check at the current window plus 30 days (D-P-22, D-P-45);
- `max_alert_rules` limits enabled alert rules (D-P-23), and, once I's M3 is in, `max_active_online_rules` limits active online rules;
- an org drop keeps the org's `usage_daily` rows; a daily task deletes them 40 days after the drop is applied (D-P-46);
- the control plane deletes orgs (`DELETE /api/internal/v1/orgs/{org_id}`, R17) and reads applied drops (`GET /api/internal/v1/deletions`, R5); internal org creation refuses a dropped or dropping id (D-P-49);
- `llmobs admin entitlements show` prints the month's units (D-P-32);
- the time-range picker shows the window with a lock, and the retention, alert and online-rule pages show the upgrade states of D-P-15 (D-P-48).

**Read first, in this order:**
1. `AGENTS.md`.
2. [`00-design.md`](00-design.md) § Decisions D-P-13–D-P-15, D-P-21–D-P-23, D-P-28, D-P-30, D-P-44–D-P-50; § Interfaces for M, K and I; § Interfaces for track Q.
3. `docs/mvp-decisions.md` D85, D101, D175, D-M-7, D-M-10, D-M-15–D-M-17, D-K-14, D-K-16, D-I-32, D-I-34.
4. `docs/spec/03-storage.md` § 6 and § 9; `docs/spec/05-query.md` § SQL point 7; `docs/spec/09-api.md` (the internal endpoints row); `docs/spec/12-operations.md` § Licence keys, plans and entitlements.
5. M2's and M3's code: `crates/server/src/entitlements/` (`EntitlementsState`, `of`, `retention_window_at`, `Limits`, `plan_limit`), `routes/internal_orgs.rs`, `routes/internal_usage.rs` (an internal route and its OpenAPI operation), `routes/time_cursor.rs`, `auth/principal.rs` (`control_plane_principal`), `http/access.rs` (`Access::ControlPlane`), `test_support/mod.rs` (`set_limits`, `org_token`, `control_plane_token`, `default_config`).
6. The code named in each sub-phase before you change it.

## Conventions

The same as M3 (`03-m3-units-and-limits.md` § Conventions), which are M2's: one commit per sub-phase with the subject given; server code returns `ApiError`; never log or return payload, tokens, emails or names (log ids and counts); time from `state.clock.now_ns()`; tests on `TestApp` (manual clock `2026-01-01T00:00:00Z` = `1_767_225_600_000_000_000` ns); web tests with Vitest + Testing Library on MSW with `server.use`, never `vi.mock`.

**Dead code between phases.** An item with no non-test caller until a later phase gets an item-level `#[cfg_attr(not(test), allow(dead_code))]` with the comment `// Track P M4: P<n> uses it.`; the phase that adds the caller removes it, and P16 checks none is left. **None is expected:** every item this plan adds has its production caller in the same sub-phase (P11's `flushed_month_units` included). If the compiler disagrees, use the attribute and report it.

**Generated files.** `crates/server/openapi.json`, `web/src/api/schema.gen.ts` and `fixtures/api/manifest.json` are regenerated (`just openapi`, `just api-examples`), never hand-merged. `crates/server/tests/snapshots/meta_schema.snap` is regenerated with `INSTA_UPDATE=always cargo test -p llmobs-server schema_matches_the_snapshot` and reviewed.

**Migrations.** A new `meta.db` migration takes the next free number in `crates/server/migrations/meta/` **at implementation time** (0009 is the last today; track I's M3 may add more). A new `catalog.db` migration takes the next free number in `crates/storage/migrations/catalog/` (0006 is the last today).

**Module wiring.** As M3: a new route module gets `pub(crate) mod …;` in `routes/mod.rs` and `routes.extend(crate::routes::<module>::routes());` in `http/access.rs`'s route table; a new `entitlements` module its line in `entitlements/mod.rs`.

**Every new or changed API operation:** the eight steps of M3 § Conventions ("Every new or changed API operation"), including `QUERY_OR_BODY`, the expected-errors branch and `collect_schemas`. Operations tagged `internal` have no API example (M3 P7 added that `continue`).

**Names used below.** `DAY_NS = 86_400_000_000_000`. "The window" of an org is `entitlements::of(state, org).limits.data_access_days` (`Option<u64>` days, `None` = unlimited). `visible_from` is the instant (ns) before which rows are hidden: `now − days × DAY_NS`, saturating.

## Commands

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
just fmt
```

```bash
just check
```

`just check` before every commit (over 20 minutes). Quick loops: `cargo test -p llmobs-storage <filter>`, `cargo test -p llmobs-server <filter>`, `cargo test -p llmobs-cli <filter>`, `just lint`, `just check-web`, `cd web && pnpm vitest run <file>`.

## Sub-phases

| # | Subject | What | After |
|---|---|---|---|
| P1 | `Make the control plane opt-in` (**done**, bb36d4a) | `entitlements.control_plane`; `/api/internal/v1/*` 404 and org/control-plane tokens refused when off (D-P-50) | M3 |
| P2 | `Add visible_from to storage queries` | `QueryOptions::new`, private fields, no `Default`; `ProjectQuery::visible_from`; the base views filter (D-P-44, D-P-47) | P1 |
| P3 | `Read through the org's data window` | `entitlements::window` (`Visibility`, `query_options`); `open_query*` take the org/visibility; every caller; the invariant tests; the deletion exemption | P2 |
| P4 | `Apply the window to streams, metrics, alerts and evaluators` | stream visibility, span-stream filter, metrics cache key, alert evaluator, `load_trace`; one test per surface | P3 |
| P5 | `Hide media outside the window` | catalog `media.newest_span_start`; `put_media`/`get_media`; ingest computes it | P4 |
| P6 | `Cap retention and ingest age by the plan` | `retention_cap_days`; `retention_once` per org; `Ingest::context`/`accept` take the cap | P5 |
| P7 | `Limit alert rules by the plan` | `alerts::quota::rule_limit` body; alert pages' Upgrade button | P6 |
| P8 | `Keep a dropped org's usage for 40 days` | the org drop keeps `usage_daily`; the daily pruner (D-P-46) | P7 |
| P9 | `Let the control plane delete orgs` | `control_plane` actor kind; `deletion::request_org_drop`; `DELETE /api/internal/v1/orgs/{org_id}`; create refuses dropped ids (D-P-49) | P8 |
| P10 | `List applied drops for the control plane` | `GET /api/internal/v1/deletions` (scope `deletions`), index migration | P9 |
| P11 | `Show the month's units in admin entitlements` | `EntitlementsReport::month_units`, CLI line (D-P-32) | P10 |
| P12 | `Lock time ranges beyond the data window` | `beyondWindow`, the picker lock (D-P-48) | P1 |
| P13 | `Show the data window on the retention settings` | retention lock line, window note, Upgrade on 403 | P1 |
| P14 | `Limit active online rules by the plan` | **after I's M3**: `eval::rule_limit::limit` body | P11 and I's M3 |
| P15 | `Show the online-rule limit upgrade state` | **after I's M3**: Upgrade button on the rule form's `403 plan_limit` | P14 |
| P16 | `Close out M4` | dead-code attributes, specs check, roadmap row | all |

P1–P11 run in order, one at a time (they edit the same server files and the generated contract). P12 and P13 touch only `web/` files that no server phase regenerates, plus spec 11 in two different bullets (P12 the Traces Header bullet, P13 the Settings Project bullet), so they do not conflict (`time-range-picker.tsx`, `lib/time-range.ts`, `settings/general.tsx`); each can run in parallel with P2–P11 in its own worktree, after P1. P14 and P15 start only once I's M3 is merged into `main`. P16 runs last.

---

## P1 — The control plane is opt-in (D-P-50)

**Done: commit bb36d4a.** The text below records what was built.

Every release build trusts the `cloud-1` key for `org` and `control_plane` tokens. Without this phase, a leaked `cloud-1` key would let anyone mint tokens for any self-hosted instance (any `audience`): create orgs, delete users, unlock features. A cell now trusts those kinds only when configured to.

### Config (`crates/server/src/config.rs`)

`EntitlementsConfig` gains, after `control_plane_provider`:

```rust
/// This instance is a cloud cell managed by the control plane (D-P-50). When false (the
/// default), every `/api/internal/v1/*` route answers 404, stored and pushed org tokens are
/// ignored, and `llmobs1.` bearer tokens are refused: only licence keys are trusted.
pub control_plane: bool,
```

The manual `impl Default for EntitlementsConfig` sets it to `false`. Config tests (next to the `control_plane_provider` tests): the default is `false`; `[entitlements] control_plane = true` parses; the env var follows the existing mapping (find how `entitlements.upgrade_url` maps to an env var in `config.rs` and test the same way, if the module has such a test).

Spec 12's config table already has the `entitlements.control_plane` row (added with this plan); check the code matches it.

### Where it applies

1. **Routes.** `http/access.rs` has `pub(crate) fn hidden_without_control_plane(state: &AppState, path: &str) -> bool` (`path.starts_with("/api/internal/") && !state.config.entitlements.control_plane`). `build_router` passes `route.path` to `enforce`, which answers `ApiError::NotFound` (with its `denial_marker`, `x-test-access: denied-404` in tests) instead of calling `prepare` when the helper is true. This also covers the public org-token `PUT`, for which `prepare` would return early. `auth/principal.rs` `authenticate` calls the same helper on `request.uri().path()` and, when true, skips reading the bearer and the cookie, so no credential is checked (and no limiter touched) for a hidden route. The route table and the OpenAPI document do not change; `http/access_suite.rs` runs with the opt-in on.
2. **Bearer tokens.** `auth/principal.rs` `control_plane_principal`: first line `if !state.config.entitlements.control_plane { return Err(ApiError::Unauthorized); }`.
3. **Stored org tokens.** `EntitlementsState::load` gains a parameter `control_plane: bool` (after `public_url`). When false, it reads no stored token: it returns an empty `orgs` map, and when `all_tokens` would have returned rows it logs once `tracing::warn!(count, "stored org tokens ignored: entitlements.control_plane is off")` (read the rows to count them; never log a token). Callers: `serve.rs` (passes `config.entitlements.control_plane`) and `admin.rs` `entitlements_in` (passes `config.entitlements.control_plane`), plus any test that calls `load`.

### Tests

`test_support::default_config` sets `config.entitlements.control_plane = true`, with the comment `// Most tests act as a cloud cell (D-P-50); a test of the default turns it off.` Every existing M2/M3 test of an internal route keeps passing.

New tests:
- `http::access::tests::internal_routes_are_hidden_without_the_control_plane` (`TestApp::with_config(|c| c.entitlements.control_plane = false)`): `POST /api/internal/v1/orgs` with a valid `control_plane_token("orgs")` → 404; `PUT /api/internal/v1/orgs/{seeded org}/entitlements` with a valid org token → 404, and the org's `entitlements::of` is unchanged; `GET /api/internal/v1/usage?from=2026-01-01&to=2026-01-01` → 404.
- `auth::principal::tests::control_plane_tokens_are_refused_when_off`: `control_plane_principal` → `Unauthorized`.
- `entitlements::tests::stored_tokens_are_ignored_when_off`: store a token with `put_token`, `load(…, false)` → `resolve` gives the defaults; the log (`capture_logs`) has `stored org tokens ignored` and `count=1`, never the token text.
- With the flag on (default test config), the existing `PUT` and internal-route tests are unchanged.

### Docs

Spec 09's `/api/internal/*` auth row and spec 12 (config table, § Org tokens, the enforcement bullets) were updated with this plan; 00-design § Interfaces for track Q says cells set the key. Check them against the code.

### Done when

`just check` passes; the four new tests pass.

---

## P2 — `visible_from` in storage (D-P-44, D-P-47)

### `QueryOptions` (`crates/storage/src/lib.rs`, ~line 219)

Replace the struct with:

```rust
/// Options of one [`Storage::query`] call. Built only with [`QueryOptions::new`], so a caller
/// always states the data window it reads under (D-P-44).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryOptions {
    around: Option<i64>,
    visible_from: Option<i64>,
}

impl QueryOptions {
    /// `around`: time hint for a single-trace lookup (`?around=`), ns (D154).
    /// `visible_from`: rows before this instant (ns) are hidden from the views: spans and
    /// traces by `start_time`, scores by `created_at`; `None` hides nothing (D175, D-P-21).
    pub fn new(around: Option<i64>, visible_from: Option<i64>) -> Self {
        Self { around, visible_from }
    }

    /// The `around` hint.
    pub fn around(&self) -> Option<i64> {
        self.around
    }

    /// The data-window bound.
    pub fn visible_from(&self) -> Option<i64> {
        self.visible_from
    }
}
```

No `Default`. Update every use in `crates/storage` (`options.around` → `options.around()`; `QueryOptions::default()` → `QueryOptions::new(None, None)`; `QueryOptions { around: Some(x) }` → `QueryOptions::new(Some(x), None)`): `lib.rs` (`project_query`), `query/traces.rs` (`ScanContext::new`'s `provider.options.around`, and tests ~2061), `query/spans.rs` tests, `query/scores.rs` test ~622, `media.rs` ~216 (the media live set: `None, None`, it must see every row), `retention.rs` ~43 (`None, None`), `compaction/rewrite.rs` tests, `deletion_tests.rs`, `drop_tests.rs`, `quarantine_tests.rs`, `tests/trace_bench.rs`. Find them all with `grep -rn "QueryOptions" crates/storage`.

The server's uses (`routes/read.rs`, `eval/trace_load.rs`, `deletion/resolve.rs` and tests) do not compile after this change until P3. **So P2 also makes the minimal server edit**: replace each `QueryOptions { around }` / `QueryOptions { around: None }` / `QueryOptions::default()` in `crates/server/src` with `QueryOptions::new(around, None)` / `QueryOptions::new(None, None)`. P3 then replaces them all.

### `ProjectQuery` (`crates/storage/src/views/mod.rs`)

Add a field, last:

```rust
/// Rows before this instant are hidden from the views (D-P-21, D-P-47). The providers
/// above stay unfiltered: storage's own reads (compaction, media GC, retention) build
/// their options with `None`.
pub visible_from: Option<i64>,
```

`project_query` (`lib.rs`) sets it from `options.visible_from()` in both returns (the dropped-project branch too). Every literal `ProjectQuery { … }` (storage `test_support::example_query` and the test literals in `filter/mod.rs`, `query/traces.rs`, `reads/*.rs`, `views/hex.rs`, `views/mod.rs`, `views/sessions.rs` ~356 and `crates/storage/tests/trace_bench.rs` ~1070; `grep -rn "ProjectQuery\s*{" crates/` catches multi-line ones) gains `visible_from: None`.

### The views (`crates/storage/src/views/`)

`view_plan` gains a parameter `visible_from: Option<i64>` after `window`. After the window filter:

```rust
if let Some(from) = visible_from {
    let bound = datafusion::common::ScalarValue::TimestampNanosecond(Some(from), Some("UTC".into()));
    builder = builder.filter(col(projection.window_column).gt_eq(Expr::Literal(bound, None)))?;
}
```

`spans_view`, `traces_view`, `scores_view` gain the same parameter and pass it. `QueryContext::new` reads `let visible_from = source.visible_from;` at the top and passes it to all three. `sessions` follows from the filtered `traces` plan, as with the window. The filter is on the view's scan, so it is pushed into the providers like D101's window: the traces provider evaluates it on merged rows (D152), so a trace is hidden by its merged `start_time`; spans and scores prune files and memtable batches by it.

Update the `QueryContext::new` doc: "`source.visible_from`, when set, hides older rows from every base view the same way (D-P-21)."

### Tests (`crates/storage/src/views/mod.rs` tests)

Use `example_query()` (the API dataset). Read its span start times and score `created_at`s from `fixtures/api/dataset` and pick a bound `B` that splits them (`B` = the 3rd-earliest trace's `start_time`).
1. `visible_from_hides_older_spans_traces_and_scores`: `ProjectQuery { visible_from: Some(B), ..example_query() }` (build the struct then set the field). `SELECT count(*) FROM spans` equals the count of dataset spans with `start_time >= B`; `traces` the count of traces whose merged `start_time >= B`; `scores` the count with `created_at >= B`; `sessions` only sessions with a visible trace.
2. `visible_from_and_the_window_intersect`: window `[B − 1 day, B + 1 h)` with `visible_from = B` → only rows in `[B, B + 1 h)`.
3. `no_visible_from_changes_nothing`: `None` → the same counts as today.
4. `a_trace_is_hidden_by_its_merged_start`: with `Storage` (follow a test in `query/traces.rs` that appends spans and opens `storage.query`): one trace with spans at `T` and `T + 10 s`; `QueryOptions::new(None, Some(T + 5 s))`: `traces` has no row for it, `spans` has the second span only.
5. In `lib.rs` tests (or wherever `Storage::query` is tested): `project_query_carries_visible_from` — `storage.query(p, QueryOptions::new(None, Some(42))).await?.visible_from == Some(42)`.

### Spec

`docs/spec/03-storage.md` § 6, the sentence "`Storage::query` takes `QueryOptions` (`around`, D154)." becomes "`Storage::query` takes `QueryOptions::new(around, visible_from)`: `around` (D154), and `visible_from`, the org's data-window bound, which the base views of 05 apply to `start_time` (`spans`, `traces`) and `created_at` (`scores`) (D-P-21, D-P-47); the providers themselves are unfiltered, for storage's own reads." Spec 05 § SQL point 7, last sentence, becomes: "Independently of it, an org's plan data-access window (D175) hides older rows from every base view through `QueryOptions::visible_from`, which the server sets for every read (D-P-21, D-P-47)."

### Done when

`cargo test -p llmobs-storage` and `cargo test -p llmobs-server` pass; `just check`.

---

## P3 — Reads through the org's window (D-P-21, D-P-44)

### `crates/server/src/entitlements/window.rs` (new; `pub(crate) mod window;` and `pub(crate) use window::{Visibility, query_options};` in `entitlements/mod.rs`)

```rust
//! The org's data window on every read (D175, D-P-21, D-P-44).

use llmobs_core::ids::OrgId;
use llmobs_storage::QueryOptions;

use crate::http::AppState;

const DAY_NS: i64 = 86_400_000_000_000;

/// Which rows a read may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Visibility {
    /// The org's window: rows before the instant (ns) are hidden. `Window(None)`: no window.
    Window(Option<i64>),
    /// Every row. Deletion reads only (D-P-21): `deletion/resolve.rs`, `deletion/mod.rs` and
    /// `routes/deletions.rs`.
    Exempt,
}

impl Visibility {
    /// The org's window now: `now − data_access_days`, or `Window(None)` without a limit.
    pub(crate) fn window(state: &AppState, org: OrgId) -> Self {
        let now = state.clock.now_ns();
        Self::Window(
            super::of(state, org)
                .limits
                .data_access_days
                .map(|days| visible_from(now, days)),
        )
    }

    /// The bound a query hides rows before; `None` for no window and for `Exempt`.
    pub(crate) fn visible_from(self) -> Option<i64> {
        match self {
            Self::Window(from) => from,
            Self::Exempt => None,
        }
    }
}

/// `now − days`, saturating (a token's huge window never overflows).
fn visible_from(now: i64, days: u64) -> i64 {
    let span = i64::try_from(days).unwrap_or(i64::MAX).saturating_mul(DAY_NS);
    now.saturating_sub(span)
}

/// The only code in the server that builds [`QueryOptions`] (D-P-44).
pub(crate) fn query_options(visibility: Visibility, around: Option<i64>) -> QueryOptions {
    QueryOptions::new(around, visibility.visible_from())
}
```

### `routes/read.rs`

```rust
/// Admits a query on `project` of `org` under the org's window, without D101's window.
pub(crate) async fn open_query(state: &AppState, org: OrgId, project: ProjectId, around: Option<i64>) -> Result<QueryContext, ApiError> {
    open_query_on(&state.storage, project, Visibility::window(state, org), around, None).await
}

/// [`open_query`] for a caller that holds the storage but not the whole state (a stream task), or
/// that needs another visibility. `window` is D101's.
pub(crate) async fn open_query_on(storage: &Storage, project: ProjectId, visibility: Visibility, around: Option<i64>, window: Option<TimeWindow>) -> Result<QueryContext, ApiError> {
    let source = storage.query(project, query_options(visibility, around)).await?;
    Ok(QueryContext::new(source, window)?)
}

/// [`open_query`] with D101's window. The SQL console and MCP's `query_sql` use it.
pub(crate) async fn open_query_windowed(state: &AppState, org: OrgId, project: ProjectId, window: Option<TimeWindow>) -> Result<QueryContext, ApiError> {
    open_query_on(&state.storage, project, Visibility::window(state, org), None, window).await
}
```

### Every caller

Change each call; the org is the one the handler already holds (`access.project.org_id` for `ProjectAccess`; `project.org_id` for a `ProjectRow`; `seeded.org_id` in tests):

| File | Call | Becomes |
|---|---|---|
| `routes/sessions.rs` (2), `routes/traces.rs` (4), `routes/users.rs` (2), `routes/scores.rs`, `routes/metrics.rs`, `mcp/traces.rs` (3) | `open_query(&state, access.project.id, x)` | `open_query(&state, access.project.org_id, access.project.id, x)` |
| `routes/sql.rs` (2), `mcp/sql.rs` | `open_query_windowed(state, access.project.id, w)` | `open_query_windowed(state, access.project.org_id, access.project.id, w)` |
| `routes/alert_rules.rs` `checked_condition(state, project, condition)` | `open_query(state, project, None)` | give `checked_condition` an `org: OrgId` parameter (its callers hold the org: read them) and pass it |
| `scores.rs` `live_scores(state, project: ProjectId, ids)` | `open_query(state, project, None)` | `live_scores(state, project: &ProjectRow, ids)`; `open_query(state, project.org_id, project.id, None)`; callers `write` and `delete` pass `project` |
| `routes/streams.rs` `check_filter(state, project, params)` | `open_query(state, project, None)` | `check_filter(state, org, project, params)`; the caller passes `access.project.org_id` |
| `routes/streams.rs` `refresh` | `open_query_on(storage, project, None, None)` | the code change of P4 § Streams, made here (`TraceStream` gains `visibility`; `refresh` takes it) |
| `alerts/evaluate.rs` `measure_rule` | `open_query_on(&state.storage, project, None, None)` | the code change of P4 § Alerts, made here (`due_rules` returns the org) |
| `routes/deletions.rs` `matching` (create and preview) | `read::open_query(state, project, None)` | `read::open_query_on(&state.storage, project, Visibility::exempt(), None, None)` |
| `deletion/resolve.rs` `resolve` | `state.storage.query(project, QueryOptions::new(None, None))` | `state.storage.query(project, query_options(Visibility::exempt(), None))` |
| `eval/trace_load.rs` `load_trace_capped` | `QueryOptions::new(around, None)` | the code change of P4 § Evaluators, made here (`load_trace` takes a `Visibility`) |
| tests: `api/convert.rs`, `routes/read.rs`, `eval/trace_load.rs`, `scores.rs`, `mcp/sql.rs`, … | `open_query(&app.state, seeded.project.id, x)` | add `seeded.org_id` (or the project's `org_id`) |

`grep -rn "open_query\(_on\|_windowed\)\?(" crates/server/src` must show no old-shape call. If track I's M3 has merged by now, its calls (`routes/online_rules.rs` filter checks, the executor's `load_trace`) are callers too: update them the same way.

**Test-only storage reads.** The tests that open storage directly (`routes/otlp.rs`, `ingest/grpc.rs`, `ingest/mod.rs`, `ingest/fixtures_tests.rs`: `grep -rn "QueryOptions" crates/server/src`) call a new helper instead:

```rust
// test_support/mod.rs
/// Every row of `project`, no window: for tests that read storage directly.
pub(crate) async fn raw_query(storage: &Storage, project: ProjectId) -> ProjectQuery {
    storage.query(project, crate::entitlements::query_options(crate::entitlements::Visibility::Window(None), None)).await.expect("query opens")
}
```

Also in `test_support/mod.rs`:

```rust
/// Spans of `project` in storage, no window: `SELECT count(*) FROM spans` over
/// `QueryContext::new(raw_query(storage, project).await, None)`.
pub(crate) async fn raw_span_count(storage: &Storage, project: ProjectId) -> u64
```

(Run the SQL through the context's session the way `routes/read.rs` tests read batches, or with `llmobs_storage::sql_json` as the SQL route does; read the `count` column as `Int64` and convert with `u64::try_from(..).unwrap_or(0)`.)

After this phase `QueryOptions` appears in `crates/server/src` only in `entitlements/window.rs`, and `Visibility::Window(` only there and in `test_support/mod.rs`.

**P3 also makes the signature changes of P4 § Streams (the `TraceStream`/`refresh` part), § Alerts and § Evaluators**, so no interim `Window(None)` ever exists in server code; P4 adds the rest (the span-stream filter, the metrics key) and every per-surface test.

### Invariant tests (`entitlements::window::tests`)

Scan the sources with `std::fs` from `concat!(env!("CARGO_MANIFEST_DIR"), "/src")`, recursively, every `.rs` file; paths compared relative to `src/` with `/` separators.
1. `only_query_options_builds_query_options`: no file other than `entitlements/window.rs` contains `QueryOptions::new(` or `QueryOptions {` (also checked with whitespace removed: `QueryOptions{`). Name the offending files in the failure message.
2. `exempt_reads_are_deletion_reads`: the files containing `Visibility::exempt()` are exactly a subset of `{"entitlements/window.rs", "deletion/resolve.rs", "deletion/mod.rs", "routes/deletions.rs"}`, and both `deletion/resolve.rs` and `routes/deletions.rs` are in the set (so a refactor that drops the exemption fails too).
3. `no_unwindowed_visibility`: the files containing `Visibility::Window(` are a subset of `{"entitlements/window.rs", "test_support/mod.rs"}`. Server code gets its window only from `Visibility::window(state, org)`; tests that need no window use `test_support::raw_query`, or `Visibility::window` on an org without a limit. The scan cannot skip `#[cfg(test)]` modules, so no test module elsewhere may write `Visibility::Window(` either.
4. `direct_storage_reads_are_listed`: remove all whitespace from each file's text, then look for the needle built as `concat!("storage", ".query(")` (so the test's own source in `entitlements/window.rs` does not match it; test 1's and 3's needles are built the same way, `concat!("QueryOptions", "::new(")`, `concat!("Visibility", "::Window(")`, or the scan skips `entitlements/window.rs` for them, since that file is allowed anyway) (this also matches the multi-line `state\n    .storage\n    .query(` form). The files containing it are a subset of `{"routes/read.rs", "deletion/resolve.rs", "eval/trace_load.rs", "test_support/mod.rs"}`. A new direct read (track I's sweeper and backfill, M's M8) adds its file to this list in the same change, and builds its options with `query_options`.
5. `visible_from_saturates`: `visible_from(NOW, 30) == NOW − 30 × DAY_NS`; `visible_from(NOW, u64::MAX) == NOW.saturating_sub(i64::MAX)` (the span saturates at `i64::MAX`, then the subtraction); `visible_from(i64::MIN, 1) == i64::MIN`.
6. `window_follows_the_org`: `TestApp`, seeded org: no limit → `Window(None)`; `set_limits(app, org, "2026-01-01T00:00:00Z", &json!({"data_access_days": 30}))` → `Window(Some(NOW − 30 × DAY_NS))`; advance the clock one hour → the bound moves one hour.

### Behaviour tests

Helper for the next tests, in `test_support/mod.rs` (`impl TestApp`): `pub(crate) async fn commit_trace_at(&self, project: &ProjectRow, trace: TraceId, span_index: u8, start_time: i64)` builds `dataset_span(0)` with `trace_id = trace`, `span_id` = `SpanId` from the hex `format!("c4{span_index:014x}")`, `parent_span_id = None`, `start_time`, `end_time = start_time + 1_000_000_000`, `ingest_time = self.state.clock.now_ns()`, and calls `self.commit_spans(project, vec![span])` (which goes through `Ingest::commit`, so the span is appended, counted in usage **and published on the live bus**). The private `commit_at` copies in `retention.rs` and `routes/projects.rs` and `commit_span` in `routes/streams.rs` stay as they are (their span ids and fields differ); new tests in this plan use `commit_trace_at`.

With `NOW` the clock and the org at `data_access_days: 30`: an "old" trace at `NOW − 40 × DAY_NS` and a "new" trace at `NOW − 1 × DAY_NS`.
- `routes::traces::tests::the_window_hides_old_traces`: `GET …/traces?from=<NOW − 60 days>&to=<NOW>` first, before any `set_limits`, lists both traces; then `set_limits(…, json!({"data_access_days": 30}))` and the same request lists the new trace only; `GET …/traces/<old>` → 404; `GET …/traces/<new>` → 200.
- `routes::sql::tests::the_window_applies_to_sql`: `POST …/sql` `SELECT count(*) AS n FROM traces` → 1; same for the CSV export (one data row).
- `mcp` test (follow an existing `search_traces` tool test in `mcp/traces.rs`): the old trace is not returned.
- `routes::deletions::tests::deletion_counts_include_hidden_traces`: with the window in force, `POST …/deletions/preview` with a filter matching both traces and a window covering both → `count` 2; `POST …/deletions` with the same filter → 202, and after `deletion::run_once` the scope (`scope_traces`) holds both trace ids; before the deletion `raw_span_count` is 2; after it is applied, `raw_span_count` is 0.

### Done when

`just check` passes; `grep -rn "QueryOptions" crates/server/src` lists only `entitlements/window.rs` (and `test_support/mod.rs` does not name the type).

---

## P4 — Streams, metrics, alerts and evaluators (D-P-21, D-K-16, D-I-34, D-P-47)

### Streams (`routes/streams.rs`)

- (Code made in P3.) `TraceStream` gains `visibility: Visibility`, set in `stream_traces` from `Visibility::window(&state, access.project.org_id)` when the stream opens (stale by at most a stream's life, D-G-34; accepted, D-P-47). `run_trace_stream` passes it to `refresh`, which gains a parameter `visibility: Visibility` and calls `open_query_on(storage, project, visibility, None, None)`.
- `SpanStream` gains `visible_from: Option<i64>` = `Visibility::window(&state, access.project.org_id).visible_from()`. In `run_span_stream`, the span filter becomes `.filter(|span| span.trace_id == trace_id && visible_from.is_none_or(|from| span.start_time >= from))`.
- Tests (`routes::streams::tests`): `the_span_stream_drops_spans_before_the_window` (org at 30 days; publish, through a commit, one span of the trace at `NOW − 40 days` and one at `NOW`: only the second arrives); `the_trace_stream_reads_through_the_window` (a committed span of an old trace does not produce a summary event; follow `the_trace_stream_sends_merged_summaries`).

### Metrics cache (`routes/metrics.rs`)

`MetricsKey` gains `data_access_days: Option<u64>` = `entitlements::of(&state, access.project.org_id).limits.data_access_days`, so a plan change never serves an answer cached under another window. Test `metrics_cache_is_keyed_by_the_window`: request once without a limit, then `set_limits` 30 days and request again within the TTL: the second answer excludes the old trace.

### Alerts (`alerts/evaluate.rs`, `meta/alerts.rs`)

- (Code made in P3.) `meta::alerts::due_rules` returns `Result<Vec<(OrgId, RuleRow)>, MetaError>`: add `p.org_id AS org_id` to its `SELECT` and decode into a new private record `#[derive(sqlx::FromRow)] struct DueRecord { org_id: String, #[sqlx(flatten)] rule: RuleRecord }` (an `org_id` that does not parse is `MetaError::Corrupt { table: "projects", column: "org_id" }`). The decode loop becomes: `let records: Vec<DueRecord> = sqlx::query_as(…)…; let mut due: Vec<(OrgId, RuleRow)> = Vec::new(); for record in records { let org: OrgId = record.org_id.parse().map_err(|_| MetaError::Corrupt { table: "projects", column: "org_id" })?; let rule = rule_from(record.rule, Vec::new())?; let is_due = /* unchanged test on rule */; if is_due { due.push((org, rule)); } }`. The sort becomes `due.sort_by_key(|(_, rule)| (rule.last_evaluated_at, rule.id));` and the channel loop `for (_, rule) in &mut due { rule.channel_ids = rule_channel_ids(pool, rule.id).await?; }` (`meta/alerts.rs` ~822–825).
- `evaluate_once` maps `(org, rule)` into `evaluate_rule(state, org, rule, now)`; `evaluate_rule` passes `org` to `measure_rule(state, org, project, …)`, which calls `open_query_on(&state.storage, project, Visibility::window(state, org), None, None)`.
- Update `due_rules`' tests for the tuple. Test `alerts::evaluate::tests::alerts_do_not_count_hidden_spans`: a count rule over a window that starts before `visible_from` counts only the visible spans. Alert windows are at most 24 h and the smallest data window is 1 day, so the test calls `measure_rule` directly with a wider range: `set_limits({"data_access_days": 1})`, commit one span at `NOW − 2 h` and one at `NOW − 30 h`, then `measure_rule(state, org, project, &condition, NOW − 2 × DAY_NS, NOW)` with a span-count condition (build it as the module's tests build conditions) → count 1.

### Evaluators (`eval/trace_load.rs`)

(Made in P3.) `load_trace` and `load_trace_capped` gain a parameter `visibility: Visibility` after `project`, used as `query_options(visibility, around)`. Callers: `eval/test_run.rs` passes `Visibility::window(state, project.org_id)` (it holds the `ProjectRow`); tests pass `Visibility::window(&app.state, seeded.org_id)` (the seeded org has no limit); I's executor, if merged, passes `Visibility::window(state, project.org_id)` (its step 1 holds the project row). Test `trace_load::tests::a_trace_before_the_window_is_not_found`: `load_trace` of an old trace under the org's window → `LoadError::NotFound`.

### Done when

`just check`.

---

## P5 — Media outside the window (D-P-21)

A blob is read by hash, not through a span. Storage records, per project and blob, the newest `start_time` of a span that referenced it; a read under a window hides a blob whose newest span is hidden.

### Catalog (`crates/storage`)

- Migration `crates/storage/migrations/catalog/NNNN_media_span_start.sql` (next number, 0007 today):

  ```sql
  -- The newest start_time (ns) of a span that referenced the blob (D-P-21).
  ALTER TABLE media ADD COLUMN newest_span_start INTEGER NOT NULL DEFAULT 0;
  ```

  (SQLite needs a default to add a `NOT NULL` column; no stored data exists, and every write sets the column.) `catalog/committer.rs` ~line 1839 `assert_eq!(migrations, 6);` becomes `7`. Spec 03's `media` DDL (§ 2, ~line 196) gains `newest_span_start INTEGER NOT NULL DEFAULT 0,   -- ns, newest span that referenced it (D-P-21)`.
- `MediaBlob` (`media.rs`) gains `pub newest_span_start: i64` ("The newest `start_time` (ns) of a span that references the blob; [`crate::Storage::get_media`] hides the blob before it.").
- `committer.rs`: `touch_media(project, sha, now, newest_span_start)` sets `last_seen_at = MAX(last_seen_at, ?), newest_span_start = MAX(newest_span_start, ?)`. `insert_media(…, newest_span_start, now)`: the `INSERT OR IGNORE INTO media` (~line 482) becomes
  ```sql
  INSERT INTO media (project_id, sha256, mime_type, size_bytes, first_seen_at, last_seen_at, newest_span_start)
  VALUES (?, ?, ?, ?, ?, ?, ?)
  ON CONFLICT (project_id, sha256) DO UPDATE SET
    last_seen_at = MAX(media.last_seen_at, excluded.last_seen_at),
    newest_span_start = MAX(media.newest_span_start, excluded.newest_span_start)
  ```
  so two puts of one new blob racing past `touch_media` both leave their span start. `media_row` returns `(mime_type, size, newest_span_start)`.
- `Storage::put_media` passes `blob.newest_span_start` to both.
- `Storage::get_media(project, sha256, range, visible_from: Option<i64>)`: after reading the row, `if visible_from.is_some_and(|from| newest_span_start < from) { return Err(MediaError::NotFound); }` before reading any byte. Doc: "`visible_from` hides a blob whose newest span is older (D-P-21)."
- Every storage caller and test (`media.rs`, `crash_tests.rs`, `drop_tests.rs`, `gc.rs` if it builds blobs) passes `None` / sets `newest_span_start`. New storage tests (`media.rs` tests): `a_blob_is_hidden_before_its_newest_span` (put with 100, `get_media(.., Some(101))` → `NotFound`, `Some(100)` → found, `None` → found); `a_later_put_raises_the_newest_span` (put 100, put again with 200 → visible at `Some(150)`); a lower second put does not lower it; `concurrent_puts_keep_the_newest_span` (`#[tokio::test(flavor = "multi_thread")]`: two `put_media` of the same new blob, with 100 and 200, started together with `tokio::join!` → the row has 200, whichever won the insert).

### Server

- `ingest/mod.rs`: a pure helper

  ```rust
  /// The newest `start_time` of the spans that reference each hash (spans' `media_refs`).
  fn newest_span_starts(spans: &[Span]) -> HashMap<String, i64>
  ```

  `commit` computes it before moving the spans, and `store_blob(blob, newest: i64)` fills `newest_span_start` with `starts.get(&blob.sha256).copied().unwrap_or(i64::MIN)` (a blob no kept span references stays hidden under any window). Unit test: two spans referencing `a`, at 5 and 9, one referencing `b` at 3 → `{a: 9, b: 3}`.
- `test_support::load_example_dataset` builds the media blobs with `newest_span_start` from the dataset spans' `media_refs` (call the same helper; make it `pub(crate)`).
- `routes/media.rs` `get_media`: `.get_media(access.project.id, &sha256, None, Visibility::window(&state, access.project.org_id).visible_from())`. Other callers (`ingest/mod.rs`, `ingest/fixtures_tests.rs` tests) pass `None`.
- Test `routes::media::tests::media_of_hidden_spans_is_not_found`: ingest (OTLP, with `media.min_bytes` = 1 as `ingest/mod.rs` tests do) a span at `NOW − 40 days` carrying a base64 image; with no limit `GET …/media/<sha>` → 200; after `set_limits` 30 days → 404; ingest a new span at `NOW` with the same image → 200.

### Spec

Spec 03 § 9, after "`get_media` reads a blob, whole or a byte range.": "Each row keeps `newest_span_start`, the newest `start_time` of the spans that referenced the blob (raised by every `put_media`); `get_media` with an org's `visible_from` answers not found when it is older (D-P-21)." Spec 09's `GET /media/{sha256}` row: append "A blob whose spans are all older than the org's data window is `404` (D-P-21)."

### Done when

`just check`.

---

## P6 — Retention cap and the ingest age bound (D-P-22, D-P-45)

### `Entitlements::retention_cap_days` (`entitlements/mod.rs`)

```rust
/// The current window plus 30 days, in days; `None` without a window (D-P-22). Ingest's D85
/// age check uses it. M's retention uses the window history instead (D-P-45).
pub(crate) fn retention_cap_days(self) -> Option<u32> {
    self.limits.data_access_days.map(|days| u32::try_from(days.saturating_add(30)).unwrap_or(u32::MAX))
}
```

Test: `None` → `None`; 30 → 60; `u64::MAX` → `u32::MAX`; `u64::from(u32::MAX)` → `u32::MAX`.

### Retention (`retention.rs`)

`retention_once` computes the cap **once per org** and isolates errors per org:

- Before the loop: `let mut caps: HashMap<OrgId, Result<Option<u32>, ()>> = HashMap::new();`.
- In the loop, for `(org, project, own)`: when `caps` has no entry for `org`, compute it once: `state.entitlements.retention_window_at(&state.meta, org, now).await` → `Ok(window)` gives `Ok(window.map(|days| u32::try_from(days.saturating_add(30)).unwrap_or(u32::MAX)))`; `Err(error)` logs `tracing::warn!(org_id = %org, error = %error, "retention window unavailable")`, adds 1 to `run.failures`, and stores `Err(())`. Then `let Ok(cap) = caps[&org] else { continue };` (copy the value out of the map) and go on with `effective_days(own, cap)` as today.
- A failed org counts **one** failure and all its projects are skipped: deleting nothing is the safe side. Other orgs still run. Replace the D-M-15 comment with: `// D-P-45: the largest window of the last 30 days, plus 30 days (D175).` and update the module doc's decision list to `(spec 03 § 8, D-M-10, D-M-15, D-P-45)`.

Tests (`retention::tests`, using `commit_at`/`commit_trace_at`):
- `the_plan_caps_retention`: org at `data_access_days: 30`, project without `retention_days`: spans at `NOW − 70 days` and `NOW − 50 days` → after `retention_once`, `raw_span_count` goes from 2 to 1: the 70-day span is dropped and the 50-day one kept (cap 60 days, day-granular via `keep_from`).
- `a_downgrade_waits_30_days`: org's token history: record a window end of 1 825 days 1 day ago (`meta::entitlements::record_window_end`, as M2's tests do) and a current limit of 30 days → spans at `NOW − 70 days` are kept (`raw_span_count` unchanged; cap = 1 825 + 30).
- `a_window_read_error_skips_the_org`: two orgs. Create org A and its two projects in one write, as `http/access.rs` tests do (~line 795): `app.state.meta.write(move |conn| Box::pin(async move { let org = insert_org(conn, OrgId::new_v7(), "A", "a", now).await?; let p1 = insert_project(conn, org.id, "A1", "a1", None, now).await?; let p2 = insert_project(conn, org.id, "A2", "a2", None, now).await?; Ok((org, p1, p2)) })).await.unwrap()` (read `insert_org`/`insert_project`'s exact signatures and return types; you need the `ProjectRow`s). Org A gets a 30-day limit (`set_limits`) and each project a span at `NOW − 70 days` (`app.commit_trace_at(&p1, …)`, `app.commit_trace_at(&p2, …)`); insert one corrupt window row for A only: `INSERT INTO org_window_ends (org_id, ended_at, data_access_days) VALUES (?, ?, -1)` with A's id and `NOW − DAY_NS` (a negative `data_access_days` makes `window_ends_since` return `MetaError::Corrupt`; read `meta/entitlements.rs` ~146 to confirm and use the table's exact columns). Org B (the seeded org) has a project with `retention_days = 1` and a span at `NOW − 3 days`. `retention_once` → `failures == 1` (one per org, not per project: this also proves the window is read once per org), `raw_span_count` is 1 for each of A's projects, and 0 for B's project.
- `the_cap_is_read_once_per_org`: covered by the `failures == 1` assertion above with two projects in A; no separate counter is added to production code.

### Ingest D85 bound (`ingest/mod.rs`, `routes/otlp.rs`, `ingest/grpc.rs`, `eval/test_run.rs`)

`Ingest` has no `AppState`, so the cap is a parameter:

- `Ingest::context(&self, project: &ProjectRow, retention_cap: Option<u32>, now_ns: i64) -> IngestContext` sets `retention_days: crate::retention::effective_days(project.retention_days, retention_cap)`.
- `Ingest::accept(raw, reservation, format, encoding, project, retention_cap, now_ns)` and `Ingest::accept_request(request, reservation, request_bytes, decompressed_bytes, project, retention_cap, now_ns)` gain `retention_cap: Option<u32>` after `project` and pass it to `context`.
- Call sites:
  - `routes/otlp.rs` `export_body`: `let retention_cap = entitlements::of(state, project.org_id).retention_cap_days();` before `accept`, passed in.
  - `ingest/grpc.rs` (the export handler, ~line 130): the same before `accept_request`.
  - `eval/test_run.rs` (~line 210): `state.ingest.context(project, None, now)` — a judge trace of a test run is written now and never hits the age bound; `None` keeps its current behaviour.
  - tests calling `accept`/`accept_request`/`context` (`ingest/mod.rs`, `routes/otlp.rs`, `ingest/grpc.rs`): pass `None`.
  - `routes/connections.rs` builds its own `IngestContext` with `retention_days: None` (no project): unchanged.
- Test `routes::otlp::tests::the_plan_bounds_span_age`: org at `data_access_days: 30`, project without retention: an OTLP request with a span starting at `NOW − 61 days` and one at `NOW − 59 days` → 200 with `partial_success.rejected_spans = 1` (follow an existing D85 rejection test there for the response shape).

### Done when

`just check`.

---

## P7 — Alert rule limit (D-P-23, D-K-14)

- `alerts/quota.rs` `rule_limit`: body `crate::entitlements::of(state, org_id).limits.max_alert_rules`; the parameters lose their `_` prefixes; doc: "The most enabled alert rules `org_id` may have (D178, D-P-23); `None` means no limit." 
- Server test `routes::alert_rules::tests::the_plan_limits_enabled_rules`: `set_limits({"max_alert_rules": 1})`; create an enabled rule → 201; a second enabled → 403 `plan_limit`, detail "Your plan allows 1 enabled alert rule."; a second created disabled → 201; enabling it → 403.
- Web (D-P-15, as M3 P4 does for seats): under the existing `planLimited` lines, render `<UpgradeButton url={useOrgEntitlements(org).upgradeUrl} />`:
  - `web/src/components/alerts/rule-form.tsx` (after the "Upgrade your plan, disable another rule, or save this one disabled." line; the form knows `org`: read its props, or take it from `useParams({ from: "/$org" })`);
  - `web/src/routes/$org/$project/alerts/$ruleId/index.tsx` (after "Upgrade your plan or disable another rule.").
- Web tests: `alerts/new.test.tsx` "shows the plan limit with the upgrade line" also expects a link named "Upgrade" with `href` = the `upgrade_url` of `fixtures/api/static/org-entitlements.json`; the same in `alerts/$ruleId/index.test.tsx` for the enable toggle (follow its existing `plan_limit` test, or add one with `server.use` answering the PATCH with `problemResponse(403, "plan_limit", { detail: "Your plan allows 1 enabled alert rule." })`).

### Done when

`just check`.

---

## P8 — A dropped org's usage stays 40 days (D-P-46, amends D-M-7)

- `deletion/mod.rs` `drop_org_step`: remove `crate::usage::delete_org_usage(conn, org).await?;` from the write; keep the usage flush before it (counts in flight still land, now as kept rows). Update the function doc: "Drops every project of the org (including any created after the request) and the org row. The org's `usage_daily` rows stay for the control plane's billing (D-P-46)."
- `usage.rs`: delete `delete_org_usage`; add

  ```rust
  /// How long a dropped org's usage rows stay (D-P-46).
  pub(crate) const DROPPED_USAGE_KEEP_NS: i64 = 40 * 86_400_000_000_000;

  /// Deletes the `usage_daily` rows of orgs whose org drop was applied 40 or more days before
  /// `now` and that have no `orgs` row. Returns the rows deleted.
  pub(crate) async fn prune_dropped_orgs(meta: &MetaDb, now: i64) -> Result<u64, MetaError>
  ```

  SQL, in one `meta.write`:

  ```sql
  DELETE FROM usage_daily
  WHERE org_id IN (
    SELECT org_id FROM deletions
    WHERE kind = 'org' AND status = 'applied' AND applied_at <= ?)
  AND org_id NOT IN (SELECT id FROM orgs)
  ```

  bound to `now - DROPPED_USAGE_KEEP_NS`; return `rows_affected()`.
- `serve.rs`: a task that runs `prune_dropped_orgs` 60 s after start and then every 24 h (follow the retention task's spawn, interval and shutdown handling); `info!(rows, "dropped orgs' usage pruned")` when non-zero, `warn!(error = %error, "usage prune failed")` on error.
- Tests: in `deletion::tests`, `org_drop_removes_the_org_and_its_usage` becomes `org_drop_keeps_the_usage` (after the drop the org's `usage_daily` rows are still there; the org row is gone). Other M tests that counted usage rows after a drop (`grep -n "usage_daily" crates/server/src/deletion/mod.rs`) change the same way. In `usage::tests`: `dropped_orgs_usage_is_pruned_after_40_days` — an org with usage, dropped (run the drop through `deletion::run_once` as M's tests do), clock + 39 days → `prune_dropped_orgs` deletes 0; + 40 days → deletes the rows; another org's rows untouched; `an_org_row_protects_its_usage` (an applied org deletion row for an id that still has an `orgs` row → nothing deleted).
- The internal usage export (M3 P7) needs no change: its rows of dropped orgs now stay.

### Spec and docs

Spec 04 § Metering (the `usage_daily` paragraph): "A dropped org's rows stay 40 days after its org deletion is applied, then a daily task deletes them (D-P-46)." In `docs/plans/m-deletion/README.md`, the `usage_daily` row of the lifecycle table (~line 128): delete the parenthesis "; until P's M4 lands, the drop still deletes them after one `Usage::flush`".

### Done when

`just check`.

---

## P9 — The control plane deletes orgs (R17, D-P-49)

### Actor kind

- `meta/deletions.rs` `ActorKind` gains `ControlPlane` (`"control_plane"`; `parse` accepts it). Doc: "The control plane (cloud), by a `control_plane` token; the id is the token's `id`."
- `api/deletions.rs` `DeletionActorKind` gains `ControlPlane` ("The cloud control plane. `id` is its request token's id."); `DeletionActor.kind`'s doc becomes "`user`, `api_key` or `control_plane`."; `routes/deletions.rs` `actor_kind_json` maps it.
- `just openapi`; `fixtures/api/static/org-deletions.json`: no change needed (its rows stay `user`), but run `just api-examples` twice.
- Web: `web/src/components/deletions/deletion-history.tsx`: line ~108 uses a new helper `actorLabel(kind)` → `"API key"` / `"Control plane"` / `"Member"`; line ~112 (the `IdText` `kind` prop) becomes `"API key"` / `"control plane"` / `"user"` (a second small mapping, `actorIdKind(kind)`). Test in the deletion history's test file (find it with `grep -rln "deletion-history\|DeletionHistory" web/src --include=*.test.tsx`): a row with `requested_by.kind = "control_plane"` shows "Control plane".

### One org-drop request (`crate::deletion`)

Move the body of `routes/deletions.rs` `drop_org` after its permission and `confirm` checks into:

```rust
/// Starts the drop of `org` (D-M-7): marks it `deleting`, records the deletion and its
/// projects in one write, wakes the worker and logs the audit event. `Ok(None)` when the
/// org is no longer active (another drop started first).
pub(crate) async fn request_org_drop(state: &AppState, org: &OrgRow, actor: (ActorKind, Uuid)) -> Result<Option<DeletionRow>, MetaError>
```

(`mark_org_deleting` returning false → `Ok(None)` instead of `MetaError::Conflict`.) `drop_org` calls it with `(ActorKind::User, user_id)` and turns `None` into the existing 409 `"the organization is already being deleted"`. The audit line logs `actor_kind = actor.0.as_str()`.

### `DELETE /api/internal/v1/orgs/{org_id}` (`routes/internal_orgs.rs`)

`Access::ControlPlane { scope: "orgs" }`, path parameter `OrgIdPath` (M2). No `?confirm=`: the control plane is the authority. Steps:
1. `org_by_id(reader, org)`:
   - `Some(row)` with `state == "active"` → `request_org_drop(&state, &row, (ActorKind::ControlPlane, token_id))`; `Some(deletion)` → **202** with `deletion_json(&deletion)` (already `pub(crate)` in `routes/deletions.rs`); `None` → step 2.
   - `Some(row)` in another state (`deleting`) → step 2.
   - `None` → step 2.
2. `meta::deletions::org_drop_of(reader, org) -> Result<Option<DeletionRow>, MetaError>` (new: the org's newest row with `kind = 'org'`, `ORDER BY created_at DESC, id DESC LIMIT 1`): a row with status `applied` → **200** with it; another status → **202** with it; none → **404**.

So a retry is idempotent whatever happened in between. Audit as `drop_org`. OpenAPI: `delete_internal_org`, tag `internal`, bearer-only, responses 200 and 202 `Deletion`, errors `401, 403, 404, 429`.

### Internal create refuses dropped ids (P-5)

`create_org`'s write closure branches on `org_by_id_tx(conn, id)`:
- `Some(org)` with `org.state == "deleting"` → new `Outcome::Dropping` → **409** `conflict`, detail "the organization is being deleted";
- `Some(org)` otherwise → `Outcome::Existing(org)` (200), as today;
- `None` and `org_drop_exists_tx(conn, id)` (new in `meta/deletions.rs`: `SELECT 1 FROM deletions WHERE org_id = ? AND kind = 'org' LIMIT 1`, `Result<bool, MetaError>`) → new `Outcome::Dropped` → **409** `conflict`, detail "the organization was deleted; use a new id";
- `None` otherwise → create, as today.

`post_internal_org` gains 409 in its OpenAPI errors and expected-errors branch.

### Tests (`routes::internal_orgs::tests`)

- `the_control_plane_drops_an_org`: `DELETE` with `control_plane_token("orgs")` → 202, body `requested_by.kind == "control_plane"`, `requested_by.id` = the token's `id`; the org answers 404 to its owner; a second `DELETE` → 202 with the same deletion id; after `deletion::run_once` → `DELETE` → 200, `status == "applied"`.
- unknown org → 404; scope `usage` → 403; a session → 403 (follow the M2 internal-route tests).
- `create_refuses_a_dropping_or_dropped_id`: create org X, `DELETE` it → `POST /api/internal/v1/orgs` with X → 409; after the drop applies → 409 again; a new id → 201.
- `routes::deletions::tests`: the existing `drop_org` tests still pass (user actor).
- **Authorization suite (`http/access_suite.rs`).** `concrete_path` replaces `{org_id}` with the world's real org id (~line 381), and the suite's `orgs` control-plane caller is `Allowed` on `Access::ControlPlane { scope: "orgs" }` routes (~500–506): run as is, the new `DELETE` would drop the world's org and break later rows. In `concrete_path`, before the `{org_id}` replacement, add: when `route.method == Method::DELETE && route.path == "/api/internal/v1/orgs/{org_id}"`, substitute `01900000-0000-7000-8000-000000009999` instead. The handler answers 404 for it (no org, no drop row), and `enforce` records `allowed`, so the matrix still sees the route as allowed.

### Design and spec

Spec 09's internal row gains `DELETE /api/internal/v1/orgs/{org_id}` (scope `orgs`; starts the org drop of D-M-7 without `confirm`; 202 with the deletion, 200 once applied; idempotent) and `POST /api/internal/v1/orgs` "409 for an id that is being or was deleted".

### Done when

`just openapi` twice, `just api-examples` twice, `just check`.

---

## P10 — The drops feed (R5, D-P-49)

### Migration (`crates/server/migrations/meta/NNNN_deletion_feed.sql`)

```sql
-- Applied project and org drops, oldest first, for the control plane (D-P-49).
CREATE INDEX deletions_applied_drops ON deletions(applied_at, id)
  WHERE kind IN ('project', 'org') AND status = 'applied';
```

Regenerate the schema snapshot (one new index).

### Types (`api/internal.rs`)

```rust
/// A drop kind in the feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum InternalDropKind { Project, Org }

/// One applied drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InternalDrop {
    /// The deletion id.
    pub id: Uuid,
    pub kind: InternalDropKind,
    pub org_id: Uuid,
    /// Project drops only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub applied_at: i64,
}

/// Query of `GET /api/internal/v1/deletions`.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct InternalDropsQuery {
    /// `project` or `org`; both when omitted.
    pub kind: Option<InternalDropKind>,
    /// 1–10 000, default 1 000.
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}
```

(Follow `InternalUsageQuery` for the utoipa attributes.) The page is `Page<InternalDrop>`, registered as `set.insert_page::<InternalDrop>("InternalDropPage")`.

### Route (`routes/internal_deletions.rs`, new; wired)

`GET /api/internal/v1/deletions`, `Access::ControlPlane { scope: "deletions" }`. `limit` outside 1–10 000 → `field_error("limit", "between 1 and 10000")`. The cursor reuses `routes::time_cursor` (`encode_time_cursor(applied_at, &id.to_string())` / `decode_time_cursor`): its JSON field is named `created_at`, and here it holds the last row's `applied_at`; the cursor is opaque, so the name is not part of the contract. Its module doc says "newest first"; change it to "Opaque `(time, id)` list cursors for `meta.db` lists", since this feed pages **oldest first**: rows strictly after `(applied_at, id)` of the cursor, in `ORDER BY applied_at ASC, id ASC`. SQL (a new `meta::deletions::applied_drops(pool, kind: Option<DeletionKind>, after: Option<(i64, Uuid)>, limit: u32)`):

```sql
SELECT id, kind, org_id, project_id, applied_at FROM deletions
WHERE kind IN ('project', 'org') AND status = 'applied'
  AND (? IS NULL OR kind = ?)
  AND (applied_at, id) > (?, ?)
ORDER BY applied_at, id
LIMIT ?
```

(no cursor: bind `(i64::MIN, "")`; fetch `limit + 1` to set `next_cursor`.) Rows are kept forever (D-M-17), so the control plane can page from any point.

OpenAPI: `list_internal_deletions`, tag `internal`, bearer-only, errors `400, 401, 403, 429`, `QUERY_OR_BODY`.

### Tests

Insert applied rows with `meta::deletions` helpers (as M's store tests do): a project drop at t=10, an org drop at t=20, a `trace_ids` deletion applied at t=15 (never listed), a pending org drop (never listed). Default → `[project@10, org@20]`; `kind=org` → `[org@20]`; `limit=1` pages through both with `next_cursor`; a bad cursor → 400; `kind=user` → 400; scope `orgs` → 403; control plane off (P1) → 404.

### Docs

Spec 09's internal row gains `GET /api/internal/v1/deletions?kind&limit&cursor` (scope `deletions`; a page of `{id, kind, org_id, project_id?, applied_at}`, applied project and org drops, oldest first, D-M-17, D-P-49).

### Done when

`just openapi` twice, `just check`.

---

## P11 — Month units in `llmobs admin entitlements show` (D-P-32)

- `usage.rs`: extract the `usage_daily` sum of `Usage::month_units`' slow path into `pub(crate) async fn flushed_month_units(pool: &SqlitePool, org: OrgId, now_ns: i64) -> Result<u64, MetaError>` (the same SQL and month bounds); `month_units` calls it. Offline, counts not yet flushed by a running server (at most 10 s) are missing; the doc says so.
- `admin.rs` `EntitlementsReport` gains `pub month_units: u64` ("Units (spans + scores) this UTC month, flushed only (D-P-16).") after `limits`; `entitlements_in` fills it with `flushed_month_units(meta.reader(), row.id, now)`.
- `crates/cli/src/main.rs` `write_entitlements` (~lines 557–560): add `writeln!(out, "units this month: {}", report.month_units)?;` immediately **before** the `let window = report.retention_window_days…` / `retention window:` lines, so the output ends with `units this month: …` then `retention window: …`. The CLI's test that builds an `EntitlementsReport` (~line 835) sets `month_units: 1240` and expects the line.
- Server test (`admin::tests`, next to the existing entitlements report test): insert `usage_daily` rows this month (spans 10, scores 5) and last month (spans 100) → `month_units == 15`.

### Done when

`just check`.

---

## P12 — The time-range picker shows the window (D-P-48)

Web only. Self-hosted orgs (no window) see no change.

### `web/src/lib/time-range.ts`

```ts
/** Whether `state` reaches before the org's data window (`windowDays`, undefined = none). */
export function beyondWindow(state: RangeState, windowDays: number | undefined, nowMs: number): boolean
```

`windowDays === undefined` → `false`. A preset: `PRESET_MS[preset] > windowDays * DAY_MS`. An absolute range: `Date.parse(state.from) < nowMs - windowDays * DAY_MS`. Tests in `time-range.test.ts`: no window → false; `30d` with 30 → false, with 7 → true; `24h` with 1 → false; absolute from 8 days ago with 7 → true, from 6 days ago → false.

### `web/src/components/time-range/time-range-picker.tsx`

- `const { org } = useParams({ from: "/$org" });` `const { query, upgradeUrl } = useOrgEntitlements(org);` `const windowDays = query.data?.limits.data_access_days;`
- `PresetRow` gains `locked: boolean`: a locked row is `disabled`, shows `<Lock className="size-3" aria-hidden="true" />` before the label, and its accessible name is `"{label}, available on a paid plan"` (`aria-label`).
- When any preset is locked, under the preset list: `<p role="note" className="flex flex-wrap items-center gap-1 text-xs text-muted-foreground"><Lock …/>Data older than {windowDays} days: available on a paid plan. <UpgradeButton url={upgradeUrl} /></p>` (`{windowDays}` as a number; "1 day" when it is 1).
- `applyTyped` and `applyCustom`: when the resolved range is `beyondWindow`, do not apply; show under that form the same note text in `text-destructive` with the Upgrade button (state `windowError: boolean`, cleared on input like the other errors).

### Tests (`web/src/components/time-range/time-range-picker.test.tsx`, new; render the traces page through the app as `routes/$org/$project/sql/index.test.tsx` does, with `server.use` on `GET /api/v1/orgs/{org}/entitlements`)

- the fixture (1 825 days): no preset is disabled, no note;
- `limits.data_access_days: 7`: "Last 30 days, available on a paid plan" is disabled; the note "Data older than 7 days: available on a paid plan." and an "Upgrade" link to the fixture's `upgrade_url`; "Last 7 days" is enabled;
- typing `10d` and pressing Enter keeps the popover open and shows the note; typing `3d` applies;
- `limits` without `data_access_days`: nothing locked.

### Spec

Spec 11 § Traces (list), the **Header** bullet (time-range picker presets): append "Presets and custom ranges older than the org's data window (`data_access_days`) show a lock and \"available on a paid plan\" with the Upgrade button, and are not applied; a link to a hidden trace is a plain 404 (D-P-48)."

### Done when

`just check-web`.

---

## P13 — The retention settings show the window (D-P-15, D-P-48)

`web/src/routes/$org/$project/settings/general.tsx`:
- `const { locked, upgradeUrl, query: entitlements } = useOrgEntitlements(org);`
- When `locked("retention_management")`: render `<LockedFeature feature="retention_management" upgradeUrl={upgradeUrl} />` above the retention field and disable the days input (the "Keep data forever" checkbox stays enabled: clearing retention is never gated, D-M-16).
- When `entitlements.data?.limits.data_access_days` is set (call it `n`): the retention field's description gains a second sentence: "Your plan shows the last {n} days; older data is hidden, then deleted 30 days later."
- On `403 plan_required`, render `<UpgradeButton url={upgradeUrl} />` after the error (the error text stays).
- Tests (`general.test.tsx`): with the fixture (feature present, 1 825 days) → no lock, the description names 1825 days; `server.use` entitlements without `retention_management` → the "Locked feature" note with "Available on the Pro plan." and an Upgrade link, the days input disabled, the checkbox enabled; a PATCH answered `problemResponse(403, "plan_required", …)` → the message and an Upgrade link.

Spec 11 § Settings, the **Project** bullet (the sentence ending "on `403 plan_required` (D-M-16)."): append, before its final period, "; with `retention_management` locked the days field is disabled with the lock line, and the plan's window is stated under it (D-P-15)."

### Done when

`just check-web`.

---

## After track I's M3 is merged

**P14 and P15 start only after track I's M3 is in `main`.** First check that these exist; if any is missing or named differently, stop and report instead of guessing:
- `crates/server/src/eval/rule_limit.rs` with `pub(crate) fn limit(_state: &AppState, _org: OrgId) -> Option<u64>` returning `None`, `RuleLimitError` and `impl From<RuleLimitError> for ApiError` (`PlanLimit`);
- `crates/server/src/routes/online_rules.rs` (create and PATCH call `rule_limit::check`);
- the rule form's `403 plan_limit` branch in `web/src/` (find it: `grep -rn "plan_limit" web/src --include=*.tsx`; I's M3 plan § I3.10 shows "A paused rule still counts as active: disable or delete a rule to free the slot.").

Also check that P3/P4 reached I's code: `grep -rn "load_trace(\|open_query(" crates/server/src/eval crates/server/src/routes/online_rules.rs` shows the new signatures (if I merged after P3/P4, I's merge had to adapt them; if not, fix them here as P3 and P4 say).

## P14 — Active online-rule limit (D-P-23, D-I-32)

- `eval/rule_limit.rs` `limit`: body `crate::entitlements::of(state, org).limits.max_active_online_rules`; parameters lose the `_`; doc: "The org's active-rule limit (D-I-32, D-P-23); `None` means no limit." Remove any dead-code attribute I left on it.
- Test (`routes::online_rules::tests`): replace the M3 test that had to call the write helper with `Some(1)` (`the_active_rule_limit_refuses_create_and_enable`) by a route test: `set_limits({"max_active_online_rules": 1})`; create an enabled rule → 201; a second enabled → 403 `plan_limit`, detail "Your plan allows 1 active online rule."; created disabled → 201; enabling it → 403. Keep the unit tests of `check`.

`just check`.

## P15 — Online-rule upgrade state (D-P-15)

In the rule form's `403 plan_limit` branch, render `<UpgradeButton url={useOrgEntitlements(org).upgradeUrl} />` after the existing alert text. Its existing `plan_limit` test also expects the "Upgrade" link to the fixture's `upgrade_url`.

`just check-web`.

---

## P16 — Close out

- No `// Track P M4:` dead-code attribute is left (`grep -rn "Track P M4" crates`); `just lint` clean.
- Specs: 03 (§ 6 options, `media` DDL, § 9), 04 (usage of dropped orgs), 05 (point 7), 09 (`/api/internal/*` auth row, internal endpoints row, media row), 11 (picker, retention), 12 (config row, enforcement bullets) were updated in their phases; check each against the code.
- `docs/plans/p-entitlements/00-design.md`: the M4 row says "written" in the plan list at the top.
- `CHANGELOG.md` § `[Unreleased]` (D-N-5):
  - `### Added`: "Plans with a data-access window hide older traces, spans, scores and media from every read (UI, API, SQL, MCP, alerts, evaluators); the time-range picker shows the limit." · "Control-plane org deletion (`DELETE /api/internal/v1/orgs/{org_id}`) and an applied-drops feed (`GET /api/internal/v1/deletions`) for cloud cells."
  - `### Changed`: "A dropped organization's daily usage is kept 40 days after the drop, for billing, then deleted."
  - `### Upgrade notes`: "Cloud cells must set `entitlements.control_plane = true`; without it, `/api/internal/v1/*` answers 404 and org and control-plane tokens are refused (D-P-50)."
  Add each line in the sub-phase that ships it if the coordinator prefers; P16 checks they are all there.
- The coordinator updates `docs/roadmap.md`; report what to write: "M1–M3 merged; M4 implemented: control plane opt-in, data window on every read (storage `visible_from`), media window, retention cap from the window history, D85 bound, alert and online-rule limits, dropped org usage kept 40 days, control-plane org delete and drops feed, admin month units, UI window lock and upgrade states."
- `just check`, `just e2e`.

## Coordination

- **Track I.** If I's M3 merges after P3/P4, its executor's `load_trace` call takes the `Visibility` (`Visibility::window(state, project.org_id)`) and its routes' `open_query` calls take the org. I's later milestones (the sweeper and live enqueue in I's M4, backfill in I's M6) read with `crate::entitlements::query_options(Visibility::window(state, org), around)` (D-I-34, with D-P-44's signature) or through `open_query`. A new file with a direct `storage.query(` must be added to the allow-list of `direct_storage_reads_are_listed` (P3) in the same change, and must never write `Visibility::Window(` (`no_unwindowed_visibility`).
- **Track M.** M's M8 (eval cascade, not merged) adds deletion reads: they live in `deletion/` and may use `Visibility::exempt()`; a new file there must be added to the allow-lists of `exempt_reads_are_deletion_reads` and, for a direct read, `direct_storage_reads_are_listed`.
- **Track Q.** Cells set `entitlements.control_plane = true` (D-P-50). New endpoints: `DELETE /api/internal/v1/orgs/{org_id}` (scope `orgs`) and `GET /api/internal/v1/deletions` (scope `deletions`); `POST /api/internal/v1/orgs` answers 409 for a dropping or dropped id (D-P-49).

## Report

Per sub-phase: what was done, deviations, the migration numbers used, whether I's M3 was merged before P3, anything unverified.
