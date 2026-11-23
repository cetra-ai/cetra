# Plan: org home with activity cards and a usage strip

The org home `/$org` is a plain table of projects (Name, Slug, Your role, Retention). This plan turns it into an activity view: one card per project with the last 24 hours (traces, error rate, LLM cost, an hourly traces sparkline with errors stacked on top, and the last ingest time), a Cards/Table switch remembered in the browser, one search box for both views, and an org usage strip above the projects (units this month against the plan, month totals, daily units, a link to Plan). The data comes from a new endpoint, `GET /api/v1/orgs/{org}/projects/activity?ids=`, that answers up to 50 projects in one request by running the existing widget query (spec 05 § Dashboard queries) once per project, four at a time. Every current behaviour of the page stays: New project (admins and owners, `DisabledReason` for others), Settings, the empty state, Load more, the pending skeletons, not-found, errors and the project link to `/$org/$project/traces`.

**Prerequisites:** branch `claude/org-home` (at `4551730`, the merge of #231, when this plan was written). Phases branch from the tip of `claude/org-home` when the phases start. Worktree `/home/sebastien/travaux/llm-observer/.claude/worktrees/org-home`.

**Read first (every phase):**
- `AGENTS.md` (§ Rules, § Tests, § Commits and reports);
- `docs/mvp-decisions.md` § Track C, rows D-C-20 (the ingestion dot and `last_ingest_at`), D-C-25 (page shell) and D-C-26;
- `docs/spec/05-query.md` § Dashboard queries, § Default exclusions;
- `docs/spec/09-api.md` § Orgs, projects, members, keys (the table at lines 62–76);
- `docs/spec/11-ui.md` § Navigation and § Pages;
- your phase's own section, and the files it names.

## Conventions (every phase)

- **Never guess.** If a step is unclear, contradicts the code, or fails in a way the phase does not explain, STOP. Make no further edits. Your final message starts with `QUESTION:` followed by the question and what you found (file, line, error text). You will be resumed with an answer.
- Work only in the worktree your brief names. Every path in this plan is relative to it. Do not create branches or worktrees. Do not stash, reset, checkout or switch branches. Commit once, at the end, with the phase's subject.
- Edit only the files your phase lists under **Edit**. If you need a change elsewhere, STOP and ask.
- Cargo: `export CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target`.
- Node: before any `node`, `pnpm` or `just` recipe that runs JS: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`. If `node_modules/` is missing in the worktree, run `just js-install` once.
- **Checks (the user's rule):** while several agents run, nobody runs full suites. A phase runs only the formatters (`cargo fmt -p cetra-server`; `node_modules/.bin/biome check --write <its files>` from the worktree root; a "schema version does not match" line is informational) and its own tests: web `cd web && ./node_modules/.bin/vitest run <paths> --maxWorkers=8` (delete `web/.vitest/` if a run creates it); Rust: compile and test once, right before the commit, with the commands in **Done when**. No `tsc`, no clippy, no `just check`: the coordinator runs `just check` once at the end.
- AGENTS.md rules: no `unwrap`/`expect` outside tests unless a comment states the invariant; no placeholders; no compatibility shims (when a signature changes, update every caller); errors through `ApiError`/`thiserror`; no payload content, prompts, filters or attribute values in logs or error messages (log ids and fixed reasons); tests for every behaviour.
- Text sizes use the named scale only (`text-caption` 11 px, `text-body` 12 px, `text-ui` 13 px, `text-title` 15 px, `text-heading` 18 px; D-C-11). A test bans `text-xs`…`text-2xl` and `text-[Npx]`.
- Web tests use the MSW mocks from `fixtures/api/`; a test that needs another answer overrides the endpoint with `server.use(...)`. Never mock modules.
- Final report: files changed, tests added, the exact commands you ran and their results, any deviation and why, anything not done.

## What it is, and what it is not

When this plan is done:
- **Endpoint.** `GET /api/v1/orgs/{org}/projects/activity?ids=<id>,<id>,…` (any org member; sessions and personal access tokens; API keys get 404 like every org route) answers, for each requested project the caller can see, in request order: its trace count, error-trace count and LLM cost (`cost_total` sum, USD) over the activity window, 24 hourly buckets `{traces, errors}`, and `last_ingest_at` when the server has received spans for it since it started. A project whose query fails is listed in `unavailable` instead of failing the request.
- **Usage strip.** Above the projects: the month's units against the plan (`UsageMeter`, compact), the month's spans, scores and judge cost, a small daily-units bar chart, and a "Plan" link to `/$org/settings/plan`. It shows a skeleton while loading and disappears (no error, no throw) when either `GET /orgs/{org}/entitlements` or `GET /orgs/{org}/usage` fails.
- **Cards (default).** One card per project, clickable as a whole (it opens `/$org/$project/traces`; the name is its one real link): name, slug, the caller's role, a 24-hour sparkline (OK traces with errors stacked on top), Traces, Error rate, Cost and Last trace. "Last trace" is `last_ingest_at` as relative time, or "—" when the server does not know it, even when the 24-hour count is above 0.
- **Table.** The current four columns plus Traces, Error rate, Cost and Last trace.
- **Switch and search.** A Cards/Table switch stored in localStorage (`cetra.org_home_view`); one search box that filters the loaded projects on name or slug, case-insensitive, in both views.
- Spec 09, spec 11, D-C-27, the CHANGELOG, the roadmap row and the UI tour page say so.

Out of scope:
- Server-side search, sorting by activity, latency and tokens on the card (user's choice).
- Remembering `last_ingest_at` across restarts (D-C-20 keeps it in memory).
- The generated demo mock (`web/src/mocks/generated/`, `?mock=large`): it does not serve org routes, so the demo shows the static fixture.
- Docs screenshots: they wait for the product freeze (user's rule).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-C-20 | `last_ingest_at` is the in-memory time of the project's last committed OTLP batch; absent until the first batch after a start. |
| D-C-25 / D-C-26 | List and item page look. The org home is not a project page (no project sidebar) and keeps its centred `max-w-5xl` column; its search box uses the page-shell input size (`h-7 w-56`). |
| D-C-27 (new, phase H5) | The org home: activity cards, Cards/Table switch, shared search, usage strip, and the activity endpoint (full text in H5). |
| OH-1 | **Window.** The activity window is the 24 epoch-aligned UTC hours whose last one contains the request time: `from = floor_hour(now) − 23 h`, `to = now + 1 ns` (exclusive, so a span starting at `now` counts and the current hour is always bucket 23). It covers between 23 and 24 hours. Totals cover the same window as the buckets. |
| OH-2 | **Query path.** Each project runs `cetra_storage::widget_query` (the function behind `POST /dashboards/query`, the Overview tiles and the traces histogram) with source `traces`, `bucket = 1h`, shape `series`, default exclusions, and three measures: `count`, `count where status = 'error'`, `sum(cost_total)`. Cost is the Overview "Cost" tile's measure: LLM cost of the traces, not judge cost. |
| OH-3 | **Fan-out, not one plan.** Storage opens a query per project (`Storage::query(project, …)` admits memory per query and builds providers for one project); there is no cross-project plan. The endpoint takes 1–50 ids and runs the per-project queries at most 4 at a time (`CONCURRENCY`, as alert evaluation), in request order, under one 30 s `Deadline`. The UI sends one request per loaded page of the project list (pages are 50). |
| OH-4 | **Failures.** A project whose query cannot open, fails, or runs out of time goes to `unavailable` (request order); the request still answers 200. The log line has the project id and a fixed reason (`timeout`, `open`, `query`). |
| OH-5 | **Visibility.** The endpoint sees exactly the projects `GET /orgs/{org}/projects` lists for the caller (active, and the effective project role is not `none`): one shared helper. Unknown ids, ids of another org and hidden projects are left out silently. |
| OH-6 | **No server cache.** `to` moves with every request, so a cache would never hit. The UI keeps answers 30 s (`staleTime`) and refetches every 60 s while the page is open. |
| OH-7 | **Order, layout and the card link.** Cards keep the list order (name, then id, as the server pages them; user, 2026-11-23). Grid: 1 column, 2 from `sm` (640 px), 3 from `lg` (1024 px). **The whole card is clickable** and opens `/$org/$project/traces` (user, 2026-11-23): the project name stays the card's only real `<Link>`, for keyboard and screen readers, and a stretched-link `::after` covers the card (`relative` on the card), so a click, a middle click or a Cmd/Ctrl-click anywhere on the card acts on that link. The card also has an `onClick` that navigates when the click did not come from a link or button inside it (jsdom does no hit testing, so this is what a test exercises; in a browser the `::after` takes the click first and the event's target is the link, so the handler does nothing). The overlay sits over the sparkline, so the card's sparkline shows no hover tooltip. |
| OH-8 | **Table columns (the user, 2026-11-23: "Today's + activity").** Name, Slug, Your role, Retention, Traces, Error rate, Cost, Last trace. No sparkline in the table. |
| OH-9 | **Strip failure.** Either request failing hides the whole strip. |

The user's answers (2026-11-23):
1. Window: last 24 hours, 24 hourly buckets for the sparkline.
2. Card contents besides name, slug and role: trace count, error rate, LLM cost, a traces sparkline with errors stacked on top, and the last trace time. Not latency, not tokens.
3. Cards by default, plus a Cards/Table switch remembered in the browser (localStorage); both views share one search box (client-side filter on name or slug over the loaded projects).
4. Keep an org usage strip above the projects: units this month against the plan (`UsageMeter`), month totals (spans, scores, judge cost), a small daily bar chart, and a link to the Plan settings page. The strip must not break the page if its requests fail.
5. Last trace: "—" when unknown, even when the 24-hour count is above 0, with no extra query for the newest trace's start (confirmed 2026-11-23).
6. Sort: by name, the list's order (OH-7, 2026-11-23).
7. The whole card is clickable and opens the project's Traces; the name stays the real link (OH-7, 2026-11-23).
8. Cost: LLM cost only, the Overview Cost tile's number (OH-2, 2026-11-23).

## Versions

No new dependency. `futures` (server) and `recharts`, `lucide-react` (web) are already declared.

## Phases and dependencies

```
H1 server endpoint, OpenAPI, fixtures, spec 09 ──┐
H2 web building blocks ───────────────────────────┼─> H3 card, table, activity hook ──> H4 page and page tests
H5 decision, spec 11, docs, changelog, roadmap ───┘
```

| Phase | Edit (summary) | Runs |
|---|---|---|
| H1 | `crates/server/src/{api,routes}/project_activity.rs` (new), `api/mod.rs`, `routes/mod.rs`, `routes/projects.rs`, `http/access.rs`, `api/openapi.rs`, `api_examples.rs`, `tests/openapi.rs`, `tests/api_examples.rs`, `fixtures/api/static/projects-activity.json` (new), `docs/spec/09-api.md`; generated: `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json` | parallel with H2, H5 |
| H2 | `web/src/lib/org-home.ts` (+test), `web/src/components/home/{view-switch,activity-sparkline,usage-strip}.tsx` (+tests) | parallel with H1, H5 |
| H5 | `docs/mvp-decisions.md`, `docs/spec/11-ui.md`, `website/src/content/docs/docs/get-started/ui-tour.mdx`, `CHANGELOG.md`, `docs/roadmap.md` | parallel with H1, H2 |
| H3 | `web/src/api/queries.ts`, `web/src/components/home/{use-project-activity.ts,project-card.tsx,project-table.tsx}` (+tests) | after H1 and H2 are merged |
| H4 | `web/src/routes/$org/index.tsx`, `web/src/routes/home.test.tsx` | after H3 |

H1, H2 and H5 each run in their own worktree and branch, all cut from the tip of `claude/org-home` when the phases start; the coordinator merges them into `claude/org-home` one at a time. The generated files (`openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json`) belong to H1 only. H3 needs H1's generated types (`Schema<"ProjectActivity">`, the new path in `$api`) and its fixture; it does not use a hand-written type. H3 and H4 run in `claude/org-home` after the merges. A Sonnet code review runs after H3 and after H4; then the coordinator runs `just check` and checks the page in a browser on the mocks (`just web-dev`).

---

## H1: server endpoint, OpenAPI, fixture, spec 09

**Goal:** add `GET /api/v1/orgs/{org}/projects/activity` with its types, docs, fixture and tests.

**Edit:**
- `crates/server/src/api/project_activity.rs` (new)
- `crates/server/src/api/mod.rs`
- `crates/server/src/routes/project_activity.rs` (new)
- `crates/server/src/routes/mod.rs`
- `crates/server/src/routes/projects.rs`
- `crates/server/src/http/access.rs` (one line)
- `crates/server/src/api/account.rs` (one doc comment)
- `crates/server/src/api/openapi.rs`
- `crates/server/src/api_examples.rs`
- `crates/server/tests/openapi.rs`
- `crates/server/tests/api_examples.rs`
- `fixtures/api/static/projects-activity.json` (new)
- `docs/spec/09-api.md`
- generated, by the commands in step 9 only: `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json`

**Steps:**

1. Create `crates/server/src/api/project_activity.rs` exactly as follows:

```rust
//! Project activity API types (spec 09 § Orgs, projects, members, keys; D-C-27).

use cetra_core::ids::ProjectId;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters of `GET /api/v1/orgs/{org}/projects/activity`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ProjectActivityParams {
    /// One to 50 distinct project ids, comma-separated, in the order of the answer.
    pub ids: String,
}

/// One hour of a project's traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ActivityBucket {
    /// Traces that started in the hour.
    pub traces: u64,
    /// Of those, the traces whose status is `error`.
    pub errors: u64,
}

/// One project's traces over the activity window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProjectActivity {
    /// The project.
    pub project_id: ProjectId,
    /// Traces that started in the window, with the default exclusions (spec 05).
    pub traces: u64,
    /// Of those, the traces whose status is `error`.
    pub errors: u64,
    /// Sum of the traces' `cost_total`, USD.
    pub cost_usd: f64,
    /// 24 hourly buckets, oldest first. Bucket `i` starts `i` hours after `from`.
    pub buckets: Vec<ActivityBucket>,
    /// When the server last committed spans sent over OTLP to this project (D-C-20). Kept in
    /// memory: absent until the first batch after the server starts.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_ingest_at: Option<i64>,
}

/// `GET /api/v1/orgs/{org}/projects/activity`: the requested projects' last 24 hours (D-C-27).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProjectsActivity {
    /// Window start: the start of the hour 23 hours before the current one. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub from: i64,
    /// Window end, exclusive: one nanosecond after the request time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub to: i64,
    /// One entry per requested project the caller can see, in request order.
    pub data: Vec<ProjectActivity>,
    /// Requested projects whose query failed or ran out of time, in request order.
    pub unavailable: Vec<ProjectId>,
}
```

2. `crates/server/src/api/mod.rs`: add `pub mod project_activity;` between `pub mod prompts;` and `pub mod saved_views;`.

3. `crates/server/src/routes/projects.rs`: extract the visibility loop of `list_projects` into a shared helper, and make `list_projects` use it. Add `use uuid::Uuid;` if it is not imported.

```rust
/// The org's active projects the caller can see, in `projects_of_org` order, each with the
/// caller's effective project role (OH-5). `GET /orgs/{org}/projects` and the activity endpoint
/// share it, so they see the same projects.
pub(crate) async fn visible_projects(
    state: &AppState,
    access: &OrgAccess,
    user_id: Uuid,
) -> Result<Vec<(ProjectRow, Role)>, ApiError>
```

   Body: the current loop of `list_projects`, unchanged in behaviour: `projects::projects_of_org(state.meta.reader(), access.org.id)`, skip `state != "active"`, `project_override(state.meta.reader(), project.id, user_id)`, `roles::effective_project_role(access.role, ov)`, skip `None`, push `(project, role)`. Then `list_projects` becomes: `let user = principal::session_user(&principal)?;`, `let visible = visible_projects(&state, &access, user.id).await?;`, map each `(project, role)` to `(project.name.clone(), project.id.to_string(), project_json(&project, Some(role)))`, and `page_named(...)` as today. Every existing test of the file must still pass unchanged.

4. Create `crates/server/src/routes/project_activity.rs`:

   - Module doc: `//! The org home's per-project activity (spec 09, D-C-27).`
   - Imports you need: `std::collections::HashMap`, `std::collections::HashSet`, `axum::extract::{Extension, State}`, `axum::routing::get`, `futures::StreamExt`, `cetra_core::ids::ProjectId`, `cetra_storage as storage`, `crate::api::project_activity::{ActivityBucket, ProjectActivity, ProjectActivityParams, ProjectsActivity}`, `crate::auth::principal::{self, Principal}`, `crate::auth::roles::OrgPerm`, `crate::http::AppState`, `crate::http::access::{Access, OrgAccess, RouteDef}`, `crate::http::error::{self, ApiError, field_error}`, `crate::meta::projects::ProjectRow`, `crate::routes::projects::visible_projects`, `crate::routes::read::{Deadline, open_query}`.
   - Constants, exactly:

```rust
/// Most project ids in one request: one page of the project list.
const MAX_IDS: usize = 50;
/// Hourly buckets in the window (OH-1).
const BUCKETS: usize = 24;
/// One hour in nanoseconds.
const HOUR_NS: i64 = 3_600_000_000_000;
/// Projects queried at once (OH-3).
const CONCURRENCY: usize = 4;
/// The error traces, a traces-view filter (the UI's `ERROR` preset).
const ERROR_CONDITION: &str = "status = 'error'";
/// The 400 message on `ids`. It names no value.
const IDS_MESSAGE: &str = "ids must be 1 to 50 distinct project ids";
```

   - `routes()`:

```rust
/// `GET /api/v1/orgs/{org}/projects/activity`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: axum::http::Method::GET,
        path: "/api/v1/orgs/{org}/projects/activity",
        access: Access::Org {
            perm: OrgPerm::View,
            session_only: false,
        },
        documented: true,
        handler: get(get_activity),
    }]
}
```

   - `fn parse_ids(raw: &str) -> Result<Vec<ProjectId>, ApiError>`: split on `,`, trim each part, parse each with `ProjectId::from_str` (import `std::str::FromStr`); any parse error, a count outside `1..=MAX_IDS`, or a duplicate gives `field_error("ids", IDS_MESSAGE)` (`http/error.rs:407`: it returns the whole `ApiError::BadRequest` with that one field error and no detail). An empty string splits into one empty part, which fails to parse: 400. Same shape as `parse_ids` in `routes/experiments.rs` (do not touch that one).
   - `fn activity_window(now: i64) -> Result<storage::TimeWindow, ApiError>`, exactly:

```rust
/// The activity window at `now` (OH-1): the 24 epoch-aligned hours whose last one holds `now`.
/// `to` is one nanosecond after `now`, so the current hour is always the last bucket.
fn activity_window(now: i64) -> Result<storage::TimeWindow, ApiError> {
    let hour = now.div_euclid(HOUR_NS) * HOUR_NS;
    let from = hour.saturating_sub(23 * HOUR_NS);
    let to = now.saturating_add(1);
    Ok(storage::TimeWindow::new(from, to)?)
}
```

   - `fn activity_query() -> storage::WidgetQuery`, exactly:

```rust
/// Traces, error traces and cost, in this measure order (OH-2).
fn activity_query() -> storage::WidgetQuery {
    use storage::{Agg, Measure, WidgetQuery, WidgetSource};
    WidgetQuery {
        source: WidgetSource::Traces,
        filter: None,
        measures: vec![
            Measure { agg: Agg::Count, field: None, condition: None },
            Measure {
                agg: Agg::Count,
                field: None,
                condition: Some(ERROR_CONDITION.to_owned()),
            },
            Measure {
                agg: Agg::Sum,
                field: Some(cetra_core::schema::traces::COST_TOTAL.to_owned()),
                condition: None,
            },
        ],
        breakdown: None,
    }
}
```

   (If `cetra_core::schema::traces::COST_TOTAL` is not a `&str` constant, STOP and ask.)

   - `fn activity_of(project_id: ProjectId, from: i64, data: &storage::WidgetData, last_ingest_at: Option<i64>) -> ProjectActivity`, exactly (a point outside the 24-bucket grid is ignored; a missing value counts as 0):

```rust
/// One project's answer from its series (OH-2). Buckets without a point stay at zero.
fn activity_of(
    project_id: ProjectId,
    from: i64,
    data: &storage::WidgetData,
    last_ingest_at: Option<i64>,
) -> ProjectActivity {
    let mut buckets = vec![ActivityBucket { traces: 0, errors: 0 }; BUCKETS];
    let mut cost_usd = 0.0;
    if let storage::WidgetData::Series { series, .. } = data {
        for line in series {
            for point in &line.points {
                let offset = point.time.saturating_sub(from);
                if offset < 0 || offset % HOUR_NS != 0 {
                    continue;
                }
                let Ok(index) = usize::try_from(offset / HOUR_NS) else {
                    continue;
                };
                let Some(bucket) = buckets.get_mut(index) else {
                    continue;
                };
                let value = point.value.unwrap_or(0.0);
                match line.measure {
                    0 => bucket.traces = whole(value),
                    1 => bucket.errors = whole(value),
                    2 => cost_usd += value,
                    _ => {}
                }
            }
        }
    }
    ProjectActivity {
        project_id,
        traces: buckets.iter().map(|bucket| bucket.traces).sum(),
        errors: buckets.iter().map(|bucket| bucket.errors).sum(),
        cost_usd,
        buckets,
        last_ingest_at,
    }
}

/// A count from an aggregate value: rounded, and 0 for a negative or non-finite value.
fn whole(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        // Positive and finite, so the cast cannot wrap; Rust's float-to-int cast saturates above
        // `u64::MAX`, and a count never gets near it.
        value.round() as u64
    } else {
        0
    }
}
```

   - `async fn project_activity(state: &AppState, project: &ProjectRow, window: storage::TimeWindow, deadline: Deadline) -> Result<ProjectActivity, &'static str>`:
     1. `let timeout = deadline.remaining().map_err(|_| "timeout")?;`
     2. `let ctx = open_query(state, project.org_id, project.id, None).await.map_err(|_| "open")?;`
     3. `let query = activity_query();` then `storage::widget_query(&ctx, storage::WidgetRequest { window, bucket: storage::Bucket::Fixed(storage::BucketWidth::H1), dashboard_filter: None, group_by: None, include_eval: false, include_experiments: false, query: &query, shape: storage::Shape::Series, compare: false, timeout })`; map the error: `storage::WidgetError::Run(storage::RunError::Timeout)` → `"timeout"`, any other → `"query"`.
     4. `Ok(activity_of(project.id, window.from(), &data, state.last_ingest.get(project.id)))`.
   - `fn split(results: Vec<(ProjectId, Result<ProjectActivity, &'static str>)>) -> (Vec<ProjectActivity>, Vec<ProjectId>)`: keeps the order; an `Ok` goes to the first list; an `Err(reason)` logs `tracing::warn!(project_id = %id, reason, "project activity failed");` and goes to the second list.
   - The handler:

```rust
/// `GET /orgs/{org}/projects/activity` (D-C-27).
async fn get_activity(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Query(params): error::Query<ProjectActivityParams>,
) -> Result<error::Json<ProjectsActivity>, ApiError>
```

     Steps, in order:
     1. `let user = principal::session_user(&principal)?;`
     2. `let ids = parse_ids(&params.ids)?;` (before any database read, so a bad request costs nothing).
     3. `let mut visible: HashMap<ProjectId, ProjectRow> = visible_projects(&state, &access, user.id).await?.into_iter().map(|(project, _)| (project.id, project)).collect();`
     4. `let wanted: Vec<ProjectRow> = ids.iter().filter_map(|id| visible.remove(id)).collect();` (request order; unknown, foreign and hidden ids drop out).
     5. `let window = activity_window(state.clock.now_ns())?;` and `let deadline = Deadline::start();`
     6. Run the queries in order, at most `CONCURRENCY` at a time:

```rust
let state_ref = &state;
let results = futures::stream::iter(wanted)
    .map(|project| {
        Box::pin(async move {
            let outcome = project_activity(state_ref, &project, window, deadline).await;
            (project.id, outcome)
        })
    })
    .buffered(CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
```

        (`buffered`, not `buffer_unordered`: the answer keeps request order. `TimeWindow` and `Deadline` are both `Copy`.)
     7. `let (data, unavailable) = split(results);` and answer `ProjectsActivity { from: window.from(), to: window.to(), data, unavailable }`.

4b. `crates/server/src/api/account.rs`, the doc comment of `Project::last_ingest_at` (lines 153–155): replace "Present only when one project is read." with "Present when one project is read; lists omit it. `GET /orgs/{org}/projects/activity` also returns it per project (D-C-27)." Nothing else in that file changes. The regenerated `openapi.json` and `schema.gen.ts` (step 9) carry the new description.

5. Register the module and the route: `crates/server/src/routes/mod.rs` gets `pub(crate) mod project_activity;` between `projects` and `prompts`; `crates/server/src/http/access.rs` gets `routes.extend(crate::routes::project_activity::routes());` on the line after `routes.extend(crate::routes::projects::routes());` (line 143).

6. OpenAPI, `crates/server/src/api/openapi.rs`:
   - Import `use super::project_activity::{ActivityBucket, ProjectActivity, ProjectActivityParams, ProjectsActivity};` next to the other `use super::…` lines (alphabetical position, after `prompts`).
   - Next to `set.insert::<UsageDay>();` (~line 252) add `set.insert::<ProjectsActivity>();`, `set.insert::<ProjectActivity>();`, `set.insert::<ActivityBucket>();`.
   - In `fn project_paths`, after the `let list = operation("list_projects", …)…build();` block (~line 1824), add:

```rust
    let activity = operation(
        "get_projects_activity",
        "projects",
        "Read the projects' last 24 hours",
    )
    .description(Some(
        "The org home's activity of the requested projects (D-C-27): traces, error traces and the sum of `cost_total` (USD) with the default exclusions, 24 hourly buckets from `from` (the epoch-aligned hours whose last one holds the request time; `to` is one nanosecond after it), and `last_ingest_at` when the server has received spans for the project since it started. `ids` is 1 to 50 distinct project ids, comma-separated; the answer follows their order. Unknown ids, ids of another org and projects hidden from the caller are left out. A project whose query fails or runs out of time is listed in `unavailable`. Any org member, through a session or a personal access token.",
    ))
    .parameters(Some(with_query::<ProjectActivityParams>(vec![path_string(
        "org",
        "Org UUID or slug.",
    )])))
    .responses(responses(
        "200",
        json_of::<ProjectsActivity>("The activity of the requested projects."),
        QUERY_ERRORS,
    ))
    .build();
```

   - In the `paths` chain at the end of `project_paths` (~line 2194), after `.path("/api/v1/orgs/{org}/projects", projects)`, add `.path("/api/v1/orgs/{org}/projects/activity", PathItem::new(HttpMethod::Get, activity))`.

7. `crates/server/tests/openapi.rs`: in `OPERATIONS`, after `("post", "/api/v1/orgs/{org}/projects", "post_project"),` (line 225) add `("get", "/api/v1/orgs/{org}/projects/activity", "get_projects_activity"),`; in `QUERY_OR_BODY`, after `"list_projects",` add `"get_projects_activity",`. If another list in that file must name the operation (a test failure says which), STOP and ask.

8. Fixture and examples:
   - Create `fixtures/api/static/projects-activity.json` with a script, never by hand: `roundtrip` (`crates/server/tests/api_examples.rs`, used by `static_files_match_their_types`) requires the file to equal `canonical_json` of the parsed value, which is `serde_json::to_string_pretty` plus a trailing newline: 2-space indent, `": "` after keys, one object field per line, and fields in struct order (`from`, `to`, `data`, `unavailable`; `project_id`, `traces`, `errors`, `cost_usd`, `buckets`, `last_ingest_at`; `traces`, `errors`). Python's `json.dumps(obj, indent=2)` prints the same layout, and the same floats for these values (`1.2345`, `0.0`) and `[]` for an empty list. Run from the worktree root:

```bash
python3 -I -c '
import json
T = [0,0,0,0,0,0,0,0,1,2,3,2,1,0,0,0,2,4,6,5,4,3,5,4]
E = {17: 1, 19: 1, 22: 1}
support = {
    "project_id": "01900000-0000-7000-8000-000000000001",
    "traces": 42,
    "errors": 3,
    "cost_usd": 1.2345,
    "buckets": [{"traces": t, "errors": E.get(i, 0)} for i, t in enumerate(T)],
    "last_ingest_at": "2026-01-01T11:29:12.000000000Z",
}
playground = {
    "project_id": "01900000-0000-7000-8000-000000000002",
    "traces": 0,
    "errors": 0,
    "cost_usd": 0.0,
    "buckets": [{"traces": 0, "errors": 0} for _ in range(24)],
}
doc = {
    "from": "2025-12-31T12:00:00.000000000Z",
    "to": "2026-01-01T11:30:00.000000001Z",
    "data": [support, playground],
    "unavailable": [],
}
assert sum(T) == 42 and sum(E.values()) == 3
open("fixtures/api/static/projects-activity.json", "w").write(json.dumps(doc, indent=2) + "\n")
'
```

     Support-bot then has 42 traces and 3 errors (buckets 17, 19 and 22); playground has none and no `last_ingest_at`. If `static_files_match_their_types` still reports the file as not canonical, compare its text with the test's expected output and STOP and ask; do not edit the JSON by hand.
   - `crates/server/src/api_examples.rs`: after the `get_org_usage` example (~line 813–819) add:

```rust
        example(
            "get_projects_activity",
            params(&[("org", "acme")]),
            BTreeMap::new(),
            "static/projects-activity.json",
            "application/json",
        ),
```

     (An empty query: the mocks then serve it for any `ids`.)
   - `crates/server/tests/api_examples.rs`: import `cetra_server::api::project_activity::ProjectsActivity`; in `static_files_match_their_types` add `roundtrip::<ProjectsActivity>("static/projects-activity.json");` after the `org-usage` line; in `generated_files_roundtrip` add the arm `"get_projects_activity" => roundtrip::<ProjectsActivity>(&example.file),` after the `"get_org_usage"` arm.

9. Regenerate (in this order; the Node PATH export first): `just openapi`, then `just api-examples`. Then `git diff --stat`: the generated changes must be only the new operation and its three schemas, plus the new `Project.last_ingest_at` description, in `crates/server/openapi.json` and `web/src/api/schema.gen.ts`, and one new entry in `fixtures/api/manifest.json`. Anything else: STOP and ask.

10. Spec 09, `docs/spec/09-api.md`: add a row to the "Method & path" table of § Orgs, projects, members, keys, directly after the `GET/POST /api/v1/orgs/{org}/projects` row:

```
| `GET /api/v1/orgs/{org}/projects/activity?ids=` (any org member, sessions and personal access tokens; the org home, D-C-27): `ids` is 1–50 distinct project ids, comma-separated (otherwise `400` on `ids`). The answer is `{from, to, data, unavailable}`: for each requested project the caller can see, in request order, `traces`, `errors` (traces with status `error`) and `cost_usd` (sum of `cost_total`) with the default exclusions, 24 hourly `buckets` of `{traces, errors}` from `from`, and `last_ingest_at` when known (D-C-20). `from` is the start of the hour 23 hours before the current one and `to` is one nanosecond after the request time. Unknown ids, ids of another org and hidden projects are left out; a project whose query fails or runs past the 30 s deadline is listed in `unavailable`. One widget query per project (spec 05 § Dashboard queries), at most 4 at a time; no cache |
```

11. Tests, in `crates/server/src/routes/project_activity.rs` under `#[cfg(test)] mod tests` (`use super::*;`, `crate::test_support::{TestApp, dataset_span, second_org}`, `serde_json::Value`, `axum::http::{Method, header}`). Query tests use `#[tokio::test(flavor = "multi_thread")]` (as the dashboard query tests do). Fixed times, as module constants: `const T: i64 = 1_767_267_000_000_000_000;` (2026-01-01T11:30:00Z), `const FROM: i64 = 1_767_182_400_000_000_000;` (2025-12-31T12:00:00Z), `const MIN: i64 = 60_000_000_000;`. Helpers in the module:
    - `async fn commit(app: &TestApp, project: &ProjectRow, trace: &str, start: i64, error: bool, cost: Option<f64>)`: `let mut span = dataset_span(0);` then set `trace_id = TraceId::from_str(trace)`, `span_id = "c400000000000001".parse()`, `parent_span_id = None`, `start_time = start`, `end_time = start + 1_000_000_000`, `ingest_time = app.state.clock.now_ns()`, `environment = Some("production".to_owned())` (not a reserved environment, so the default exclusions keep it; spec 05 § Default exclusions), `metadata.clear()` (no `cetra.experiment_id` key, `cetra_core::names::metadata_key::EXPERIMENT_ID`, so the experiment exclusion keeps it; `dataset_span(0)` has an empty metadata today, the clear makes it explicit), `session_id = None`, `status = if error { SpanStatus::Error } else { SpanStatus::Ok }` (`cetra_core::enums::SpanStatus`), `cost_input = None`, `cost_output = None`, `cost_total = cost`; then `app.commit_spans(project, vec![span]).await`. Trace ids are 32 hex characters (`"a7000000000000000000000000000001"` …).
    - `async fn get(app: &TestApp, cookie: &str, query: &str) -> TestResponse` → `GET /api/v1/orgs/acme/projects/activity{query}` with the cookie.

    Named cases:
    - `activity_counts_traces_errors_and_cost_by_hour`: `seed()`, `clock.set(T)`; commit A at `FROM + 10*MIN` ok cost 0.5; B at `FROM + 21*60*MIN + 5*MIN` error cost 0.25; C at `FROM + 21*60*MIN + 40*MIN` ok no cost; D at `T - 1_000_000_000` error cost 1.0; E at `FROM - 1_000_000_000` ok cost 9.0 (before the window). Request `?ids=<project id>`. Assert 200, `from == "2025-12-31T12:00:00.000000000Z"`, `to == "2026-01-01T11:30:00.000000001Z"`, `data[0].traces == 4`, `errors == 2`, `cost_usd == 1.75`, `buckets` has 24 entries, `buckets[0] == {traces: 1, errors: 0}`, `buckets[21] == {2, 1}`, `buckets[23] == {1, 1}`, every other bucket `{0, 0}`, `unavailable == []`. (If trace status does not follow the root span's `Error`, STOP and ask.)
    - `the_current_hour_is_the_last_bucket_even_on_the_hour`: `clock.set(T + 30*MIN)` (12:00:00 exactly); commit one trace at `T + 30*MIN` (that instant). Assert `from == "2025-12-31T13:00:00.000000000Z"` and `buckets[23].traces == 1`.
    - `answers_follow_request_order_and_skip_unknown_ids`: `seed()`, `add_project(&seeded, "playground", None)`; a project of another org: `let (other_org, _) = second_org(&app, "other", "owner@other.test").await;` then, as `TestApp::add_project` does (`test_support/mod.rs:544-580`), `let now = app.state.clock.now_ns(); let foreign = app.state.meta.write(move |conn| Box::pin(async move { crate::meta::projects::insert_project(conn, other_org, "Other", "other", None, now).await })).await.unwrap();` (signature `insert_project(conn, org_id, name, slug, retention_days, now)`, `meta/projects.rs:76`); request `?ids=<playground>,<random ProjectId::new_v7()>,<other org's project>,<support-bot>`. Assert `data` has two entries, playground first then support-bot, and `unavailable == []`.
    - `hidden_projects_are_left_out`: `seed()`; `let bob: Member = app.add_member(&seeded, "bob@example.com", OrgRole::Member).await;` (`Member { id, cookie }`, `test_support/mod.rs:588`). First, bob asks for the seeded project with `bob.cookie`: 200 and `data` has that project. Then set the override, as `routes/projects.rs` tests write meta rows:

```rust
let (project_id, user_id) = (seeded.project.id, bob.id);
app.state
    .meta
    .write(move |conn| {
        Box::pin(async move {
            crate::meta::members::set_project_override(
                conn,
                project_id,
                user_id,
                crate::auth::roles::ProjectOverride::None,
            )
            .await
        })
    })
    .await
    .unwrap();
```

      Bob asks again: 200 and `data == []`. `GET /api/v1/orgs/acme/projects` with `bob.cookie` also lists no project now (same visibility, OH-5). (If `set_project_override`'s error type does not fit `meta.write`'s closure, copy the error mapping of the nearest test in `routes/projects.rs` that writes through `app.state.meta.write`; do not change `set_project_override`.)
    - `last_ingest_at_is_present_only_when_known`: two projects; `app.state.last_ingest.record(support_bot.id, T - 5*MIN)`; assert support-bot's `last_ingest_at == "2026-01-01T11:25:00.000000000Z"` and playground's entry has no `last_ingest_at` key. The explicit `record` is intended: `TestApp::commit_spans` does not call `last_ingest.record` (only the OTLP handlers do, `ingest/grpc.rs:366` and `routes/otlp.rs:247`), as `routes/projects.rs`'s `get_project_shows_last_ingest_at_after_a_batch` also does.
    - `a_project_without_traces_has_zero_buckets`: a fresh project; 24 buckets of zeros, `traces == 0`, `cost_usd == 0.0`.
    - `bad_ids_are_refused`: each of `?ids=`, `?ids=nope`, `?ids=<id>,<id>` (duplicate), 51 distinct random ids → 400 with one field error on `ids` (`response.problem()`); `?ids=<id>&foo=1` → 400; no `ids` → 400.
    - `fifty_ids_are_allowed`: 49 random ids plus the seeded project → 200, `data` has one entry.
    - `outsiders_get_404`: a signed-in user of no org (copy the `outsider` function with its doc comment, `crates/server/src/routes/usage.rs` lines 105–148, into this test module, with the imports it needs: `crate::meta::users::{NewUser, insert_user}`, `crate::auth::session::start_session`, `crate::auth::session::SessionOrigin`, `crate::meta::users::EmailVerifiedVia`) → 404.
    - `an_api_key_gets_404`: `Authorization: Bearer <seeded.read_key>` → 404 (the org access layer answers 404 for keys).
    - `failed_projects_are_listed_as_unavailable_in_request_order`: unit test of `split` with three results (ok, err "timeout", ok): the first list holds the two activities in order, the second the failed id.
    - `points_off_the_grid_are_ignored`: unit test of `activity_of` with a hand-built `storage::WidgetData::Series { bucket_ns: HOUR_NS, series: vec![…] }`: a point at `from - HOUR_NS`, one at `from + 24*HOUR_NS`, one at `from + HOUR_NS/2`, and one at `from + 2*HOUR_NS` with `value: None` for measure 0 and `Some(3.0)` for measure 1; assert all buckets zero except `buckets[2].errors == 3`, and `traces == 0`. Add points at `from + 5*HOUR_NS`: measure 0 `Some(4.0)` and no measure-1 point; and at `from + 6*HOUR_NS`: measure 0 `Some(2.0)`, measure 1 `Some(0.0)`; assert `buckets[5] == {traces: 4, errors: 0}` and `buckets[6] == {traces: 2, errors: 0}`, so then `traces == 6` and `errors == 3`.
    - In `routes/projects.rs` nothing new: its existing tests (including `lists_never_show_last_ingest_at`) cover the refactor.

**Done when** (Rust compiled and tested once, right before the commit):
- `cargo fmt -p cetra-server`
- `cargo test -p cetra-server --lib project_activity` and `cargo test -p cetra-server --lib routes::projects` pass;
- `cargo test -p cetra-server --test openapi` and `cargo test -p cetra-server --test api_examples` pass, including `static_files_match_their_types` (the new fixture is canonical) and `generated_files_roundtrip`;
- `just api-examples` leaves no further diff;
- `cd web && ./node_modules/.bin/vitest run src/mocks --maxWorkers=8` passes (`fixtures/api/manifest.json` feeds `web/src/mocks/manifest.ts`, tested by `manifest.test.ts` and `fallbacks.test.ts`);
- `web/src/api/schema.gen.ts` has the path `/api/v1/orgs/{org}/projects/activity` and the schemas `ProjectsActivity`, `ProjectActivity`, `ActivityBucket`.

**Commit:** `Add the projects activity endpoint for the org home`

---

## H2: web building blocks

**Goal:** add the view preference, the search filter, the sparkline, the view switch and the usage strip, each with tests, without touching the page.

**Edit:**
- `web/src/lib/org-home.ts` (new), `web/src/lib/org-home.test.ts` (new)
- `web/src/components/home/view-switch.tsx` (new), `web/src/components/home/view-switch.test.tsx` (new)
- `web/src/components/home/activity-sparkline.tsx` (new), `web/src/components/home/activity-sparkline.test.tsx` (new)
- `web/src/components/home/usage-strip.tsx` (new), `web/src/components/home/usage-strip.test.tsx` (new)

**Steps:**

1. `web/src/lib/org-home.ts`. Copy the structure of `web/src/lib/list-layout.ts` (its own `listeners` set and `fallback`, `readStored`, `emit`, `subscribe`, `useSyncExternalStore`), with these contracts:

```ts
import { useSyncExternalStore } from "react";
import { STORAGE_PREFIX } from "@/product";

/** How the org home shows its projects (D-C-27). */
export type ProjectView = "cards" | "table";

export const PROJECT_VIEWS: readonly ProjectView[] = ["cards", "table"];

export const DEFAULT_PROJECT_VIEW: ProjectView = "cards";

/** `cetra.org_home_view`: one preference per browser, for every org. */
export const PROJECT_VIEW_STORAGE_KEY = `${STORAGE_PREFIX}org_home_view`;

/** One hour in milliseconds: the width of an activity bucket. */
export const HOUR_MS = 3_600_000;

export function getProjectView(): ProjectView;
export function setProjectView(next: ProjectView): void;
export function useProjectView(): ProjectView; // server snapshot: DEFAULT_PROJECT_VIEW

/**
 * The projects whose name or slug contains `query`, ignoring case and the query's surrounding
 * spaces, in their order. A blank query keeps them all.
 */
export function filterProjects<T extends { name: string; slug: string }>(
  projects: readonly T[],
  query: string,
): T[];

/** `errors / traces`, or undefined when there are no traces. */
export function errorRate(traces: number, errors: number): number | undefined;
```

   `readStored` returns the stored value when it is one of `PROJECT_VIEWS`, else `fallback ?? DEFAULT_PROJECT_VIEW`, and catches a throwing `localStorage` the same way. `setProjectView` sets `fallback`, writes `localStorage` inside `try`, then emits. `filterProjects`: `const needle = query.trim().toLowerCase();` empty → `[...projects]`; else keep `name.toLowerCase().includes(needle) || slug.toLowerCase().includes(needle)`. `errorRate`: `traces > 0 ? errors / traces : undefined`.

   Tests `org-home.test.ts` (reset with `localStorage.clear()` and `setProjectView("cards")` in `afterEach`, as `list-layout.test.ts`): `defaults to cards`; `stores the choice` (key `cetra.org_home_view` holds `table`); `reads a garbage stored value as cards`; `re-renders a hook consumer when the view changes` (`renderHook` + `act`); `keeps the choice when storage throws` (spy `getItem`/`setItem` to throw); `filterProjects keeps all for a blank query` (`"  "`); `matches name or slug ignoring case` (`"PLAY"` keeps Playground; `"support-b"` keeps Support bot by slug); `trims the query`; `keeps the input order`; `errorRate is undefined without traces` and `3 of 42 is 3/42`.

2. `web/src/components/home/view-switch.tsx`: copy the markup of `LayoutSwitcher` (`web/src/components/layout-switcher.tsx`): a `role="radiogroup"` with `aria-label="View"`, one ghost `icon-xs` `Button` per option with `role="radio"`, `aria-checked`, `aria-label`, a `Tooltip` with the label, `bg-muted` on the checked one, `onClick` → `setProjectView(id)`. Options, in this order: `{ id: "cards", label: "Cards", icon: LayoutGrid }`, `{ id: "table", label: "Table", icon: List }` (both from `lucide-react`). Export `export function ProjectViewSwitch()`; it reads `useProjectView()`.

   Tests `view-switch.test.tsx` (render inside `TooltipProvider` from `@/components/ui/tooltip`): `checks Cards by default`; `clicking Table checks it and stores it` (`aria-checked="true"`, localStorage value `table`).

3. `web/src/components/home/activity-sparkline.tsx`, exactly as follows:

```tsx
import type { BarShapeProps } from "recharts";
import { Bar, BarChart, XAxis, YAxis } from "recharts";
import { BucketBar, roundsTop } from "@/components/charts/plot-helpers";
import { type ChartConfig, ChartContainer } from "@/components/ui/chart";
import { epochMs } from "@/lib/metrics";
import { HOUR_MS } from "@/lib/org-home";

/** One hour of a project's traces: all of them, and the error traces among them. */
export type ActivityCounts = { traces: number; errors: number };

/** One bar: its start in ms, the OK and error traces; null draws no segment. */
export type SparkRow = { time: number; ok: number | null; errors: number | null };

const CONFIG = {
  ok: { label: "OK", color: "var(--chart-ok)" },
  errors: { label: "Errors", color: "var(--chart-error)" },
} satisfies ChartConfig;

/** The stack of a bar, bottom first: errors sit on top of the OK traces (the traces histogram's colours). */
const STACK = ["ok", "errors"] as const;

/** Bucket `i` starts `i` hours after `from`. Zero segments are null, so no empty bar is drawn. */
export function sparklineRows(from: string, buckets: readonly ActivityCounts[]): SparkRow[] {
  const start = epochMs(from);
  if (start === undefined) {
    return [];
  }
  return buckets.map((bucket, index) => {
    const errors = Math.max(0, Math.min(bucket.errors, bucket.traces));
    const ok = Math.max(0, bucket.traces - errors);
    return { time: start + index * HOUR_MS, ok: ok > 0 ? ok : null, errors: errors > 0 ? errors : null };
  });
}

/**
 * A 32 px stacked bar chart of a project's last 24 hours, without axes or tooltip: on a card the
 * stretched name link covers it (OH-7), so a tooltip could never show.
 */
export function ActivitySparkline({
  from,
  buckets,
}: {
  from: string;
  buckets: readonly ActivityCounts[];
}) {
  const rows = sparklineRows(from, buckets);
  return (
    <div role="img" aria-label="Traces in the last 24 hours" className="h-8 w-full select-none">
      <ChartContainer
        config={CONFIG}
        initialDimension={{ width: 280, height: 32 }}
        className="aspect-auto h-8 w-full"
      >
        <BarChart
          data={rows}
          barCategoryGap="15%"
          margin={{ top: 1, right: 0, bottom: 0, left: 0 }}
          accessibilityLayer={false}
        >
          <XAxis dataKey="time" hide />
          <YAxis hide domain={[0, "auto"]} />
          <Bar
            dataKey="ok"
            stackId="traces"
            fill="var(--chart-ok)"
            isAnimationActive={false}
            shape={(props: BarShapeProps) => (
              <BucketBar {...props} roundTop={roundsTop(props.payload, "ok", STACK)} />
            )}
          />
          <Bar
            dataKey="errors"
            stackId="traces"
            fill="var(--chart-error)"
            isAnimationActive={false}
            shape={(props: BarShapeProps) => (
              <BucketBar {...props} roundTop={roundsTop(props.payload, "errors", STACK)} />
            )}
          />
        </BarChart>
      </ChartContainer>
    </div>
  );
}
```

   Tests `activity-sparkline.test.tsx`: `sparklineRows starts each bucket an hour apart` (from `"2025-12-31T12:00:00.000000000Z"`: row 1's time is row 0's + 3 600 000); `splits OK and errors and nulls zeros` (`{traces: 4, errors: 1}` → `ok 3, errors 1`; `{0, 0}` → both null); `never draws more errors than traces` (`{traces: 1, errors: 5}` → `ok null, errors 1`); `is empty for an unparseable from`; `draws one segment per non-zero part` (buckets `[{2,0},{0,0},{3,1}]` → `waitFor` 3 `.recharts-bar-rectangle` inside the `img` named "Traces in the last 24 hours", the approach of `count-bar-chart.test.tsx`). No tooltip test: the sparkline has none.

4. `web/src/components/home/usage-strip.tsx`:

```ts
/** One UTC day of the strip's chart: `units` is null on a day without units (no bar). */
export type DayRow = { day: string; time: number; units: number | null };

/** Every UTC day from `usage.from` to `usage.to`, inclusive, with its units. */
export function usageDayRows(usage: {
  from: string;
  to: string;
  days: readonly { day: string; units: number }[];
}): DayRow[];

/** The month's spans, scores and judge cost, summed over `days`. */
export function usageTotals(
  days: readonly { spans: number; scores: number; judge_cost_usd: number }[],
): { spans: number; scores: number; judgeCost: number };

/** The org's usage this month (D-C-27). Hidden when either of its requests fails. */
export function UsageStrip({ org }: { org: string });
```

   `usageDayRows` body, exactly:

```ts
const DAY_MS = 86_400_000;

export function usageDayRows(usage: {
  from: string;
  to: string;
  days: readonly { day: string; units: number }[];
}): DayRow[] {
  const start = Date.parse(`${usage.from}T00:00:00Z`);
  const end = Date.parse(`${usage.to}T00:00:00Z`);
  if (Number.isNaN(start) || Number.isNaN(end) || end < start) {
    return [];
  }
  const units = new Map(usage.days.map((row) => [row.day, row.units]));
  const rows: DayRow[] = [];
  for (let time = start; time <= end; time += DAY_MS) {
    const day = new Date(time).toISOString().slice(0, 10);
    const value = units.get(day) ?? 0;
    rows.push({ day, time, units: value > 0 ? value : null });
  }
  return rows;
}
```

   `UsageStrip` steps:
   1. `const { query: entitlements } = useOrgEntitlements(org);` (`@/components/plan/use-org-entitlements`) and `const usage = useQuery(orgUsageQuery(org));` (`@/api/queries-settings`).
   2. If `entitlements.isError || usage.isError` → `return null` (never throw: OH-9).
   3. If either `data` is undefined → `return <Skeleton className="h-20 w-full" />`.
   4. Otherwise render:

```tsx
<section
  aria-label="Usage this month"
  className="grid gap-4 rounded-md border p-3 md:grid-cols-[minmax(0,16rem)_auto_minmax(0,1fr)] md:items-center"
>
  <div className="flex flex-col gap-1">
    <div className="flex items-center justify-between gap-2">
      <span className="text-caption font-medium text-muted-foreground">Usage this month</span>
      <Link
        to="/$org/settings/plan"
        params={{ org }}
        className="focus-ring rounded-sm text-caption text-muted-foreground hover:text-foreground hover:underline"
      >
        Plan
      </Link>
    </div>
    <UsageMeter
      variant="compact"
      units={month.units}
      resetsAt={month.resets_at}
      included={limits.units_included}
      cap={limits.units_cap}
    />
  </div>
  <dl className="flex gap-4">
    {/* one <div> per total: <dt className="text-caption text-muted-foreground">, <dd className="text-body tabular-nums"> */}
    Spans: numbers.format(totals.spans) · Scores: numbers.format(totals.scores) · Judge cost: formatCost(totals.judgeCost)
  </dl>
  {rows.some((row) => row.units !== null) ? (
    <DailyUnitsChart rows={rows} />
  ) : (
    <p className="text-body text-muted-foreground">No usage this month.</p>
  )}
</section>
```

      where `month = entitlements.data.usage`, `limits = entitlements.data.limits`, `rows = usageDayRows(usage.data)`, `totals = usageTotals(usage.data.days)`; `numbers` and `UsageMeter` come from `@/components/plan/usage-meter`, `formatCost` from `@/lib/format`, `Link` from `@tanstack/react-router`, `Skeleton` from `@/components/ui/skeleton`. Write the `<dl>` as three `<div>`s, each a `<dt>` (label) then a `<dd>` (value), with the classes in the comment.
   5. `DailyUnitsChart` (private, same file): a `role="img"` `aria-label="Daily units"` `div` of class `h-12 w-full select-none`, holding a `ChartContainer` (`config = { units: { label: "Units", color: "var(--chart-1)" } }`, `initialDimension={{ width: 320, height: 48 }}`, `className="aspect-auto h-12 w-full"`) with a `BarChart` (`data={rows}`, `barCategoryGap="20%"`, `margin={{ top: 2, right: 0, bottom: 0, left: 0 }}`, `accessibilityLayer={false}`), `<XAxis dataKey="time" hide />`, `<YAxis hide domain={[0, "auto"]} />`, a `Tooltip` whose content shows the row's `day` (mono, muted) and "Units" with `numbers.format(units)` (the markup of `HistogramTooltip` in `web/src/components/trace/traces-histogram.tsx`: a `grid min-w-32 gap-1 rounded-md border bg-popover px-2.5 py-1.5 text-caption shadow-md` box, the day in a `font-mono text-caption text-muted-foreground` line, then one `flex items-center justify-between gap-4` row with "Units" muted and the value in `font-mono tabular-nums`; `null` when inactive or without a row), and one `Bar dataKey="units" fill="var(--chart-1)" isAnimationActive={false}` whose `shape` is `<BucketBar {...props} roundTop={roundsTop(props.payload, "units")} />`.

   Tests `usage-strip.test.tsx`. Render with a root route, as `web/src/components/datasets/datasets-empty.test.tsx` does (`createRootRoute({ component: () => <UsageStrip org="acme" /> })`, a `QueryClient` with `retry: false`, `RouterProvider`, `await router.load()`); the MSW fixtures answer (`static/org-entitlements.json`, `static/org-usage.json`). Cases:
   - `usageDayRows fills every day of the range` (from `2026-01-01` to `2026-01-03` with days 01 and 03 → 3 rows, the middle `units: null`, times one day apart);
   - `usageDayRows is empty for an inverted range`;
   - `usageTotals sums the days`;
   - `shows the meter, the month totals, the daily chart and the Plan link` (`findByRole("region", { name: "Usage this month" })`; a `meter` named "Units this month"; texts `2,000`, `50`, `$0.42`; `img` "Daily units"; link "Plan" with `href="/acme/settings/plan"`);
   - `says when the month has no usage` (`server.use` usage → `{from: "2026-01-01", to: "2026-01-03", days: []}`; "No usage this month.");
   - `hides itself when usage fails` (`server.use(http.get("/api/v1/orgs/:org/usage", () => problemResponse(500, "internal")))`; `waitFor` no `[data-slot='skeleton']`, then no region);
   - `hides itself when entitlements fail` (same with `/api/v1/orgs/:org/entitlements`).

**Done when:** in a fresh worktree first `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just js-install`; then `node_modules/.bin/biome check --write web/src/lib/org-home.ts web/src/lib/org-home.test.ts web/src/components/home/` passes; `cd web && ./node_modules/.bin/vitest run src/lib/org-home.test.ts src/components/home --maxWorkers=8` passes.

**Commit:** `Add the org home's view switch, sparkline and usage strip`

---

## H3: activity hook, project card and project table

Starts after H1 and H2 are merged into `claude/org-home` (it needs `Schema<"ProjectActivity">` and the new path in `schema.gen.ts`, and H2's components).

**Goal:** fetch the activity per loaded page, and render one project as a card or a table row.

**Edit:**
- `web/src/api/queries.ts`
- `web/src/components/home/use-project-activity.ts` (new), `web/src/components/home/use-project-activity.test.tsx` (new)
- `web/src/components/home/project-card.tsx` (new), `web/src/components/home/project-card.test.tsx` (new)
- `web/src/components/home/project-table.tsx` (new), `web/src/components/home/project-table.test.tsx` (new)

**Steps:**

1. `web/src/api/queries.ts`: after `projectListOptions`, add

```ts
/** The last 24 hours of up to 50 projects of `org` (D-C-27). */
export function projectActivityQuery(org: string, ids: readonly string[]) {
  return $api.queryOptions("get", "/api/v1/orgs/{org}/projects/activity", {
    params: { path: { org }, query: { ids: ids.join(",") } },
  });
}
```

2. `web/src/components/home/use-project-activity.ts`, modelled on `web/src/components/experiments/use-experiment-summaries.ts`:

```ts
/** How long an activity answer is fresh, and how often the open page refetches it (OH-6). */
export const ACTIVITY_STALE_MS = 30_000;
export const ACTIVITY_REFETCH_MS = 60_000;

/** Where one project's activity stands. `none`: unavailable, failed, or left out of the answer. */
export type ActivityState =
  | { state: "loading" }
  | { state: "ready"; from: string; activity: Schema<"ProjectActivity"> }
  | { state: "none" };

/** A project's activity as text, for a card or a table row. */
export type ActivityCells = {
  traces: string;
  errorRate: string;
  cost: string;
  /** RFC 3339; absent when unknown (shown as "—"). */
  lastIngestAt?: string;
};

/**
 * The activity of the loaded projects, one request per page of the project list (OH-3).
 * Returns the lookup by project id.
 */
export function useProjectActivity(
  org: string,
  pages: readonly (readonly Schema<"Project">[])[],
): (id: string) => ActivityState;

/**
 * The text of a project's activity: `undefined` while loading (the caller shows skeletons),
 * dashes when the activity is `none`, else the formatted numbers.
 */
export function activityCells(state: ActivityState): ActivityCells | undefined;
```

   `useProjectActivity`: `const chunks = pages.filter((page) => page.length > 0);` `useQueries({ queries: chunks.map((chunk) => ({ ...projectActivityQuery(org, chunk.map((project) => project.id)), staleTime: ACTIVITY_STALE_MS, refetchInterval: ACTIVITY_REFETCH_MS })) })`. Build `ready: Map<string, {from, activity}>` from each answered query (`for (const activity of query.data.data) ready.set(activity.project_id, { from: query.data.from, activity })`) and `loading: Set<string>` from the ids of chunks whose query `isPending`. Lookup: ready → `{ state: "ready", ... }`; in loading → `{ state: "loading" }`; else `{ state: "none" }` (so an id in `unavailable`, a failed request, or an id the answer left out all read `none`).

   `activityCells`: `loading` → `undefined`; `none` → `{ traces: "—", errorRate: "—", cost: "—" }`; `ready` → `{ traces: formatCount(a.traces), errorRate: rate === undefined ? "—" : formatPercent(rate), cost: formatCost(a.cost_usd), lastIngestAt: a.last_ingest_at }` with `rate = errorRate(a.traces, a.errors)` (`formatCount`, `formatPercent` from `@/lib/metrics`; `formatCost` from `@/lib/format`; `errorRate` from `@/lib/org-home`). Omit `lastIngestAt` when `a.last_ingest_at` is undefined.

   Tests `use-project-activity.test.tsx` (`renderHook` inside a `QueryClientProvider` with `retry: false`; the fixture answers `projects-activity.json`): `reads ready activity from the fixture` (support-bot id → `ready`, `activity.traces === 42`, `from` is the fixture's); `is loading before the answer` (first render: `loading`); `reads none for an unavailable project` (`server.use` answering `const body: Schema<"ProjectsActivity"> = { ...activityFixture, data: [], unavailable: [supportId] };`, where `activityFixture` is the JSON import of `fixtures/api/static/projects-activity.json` cast once as `activityFixture as Schema<"ProjectsActivity">`); `reads none when the request fails` (500); `asks once per non-empty page with that page's ids` (two pages; capture `ids` of each request: the two joined id lists); `activityCells` cases: loading → undefined; none → three "—"; support-bot → `"42"`, `"7.1%"`, `"$1.23"`, `lastIngestAt` the fixture's; playground → `"0"`, `"—"`, `"$0"`, no `lastIngestAt`.

3. `web/src/components/home/project-card.tsx`:

```ts
export function ProjectCard({
  org,
  project,
  activity,
}: {
  org: string;
  project: Schema<"Project">;
  activity: ActivityState;
}): JSX.Element;
```

   Markup, in this order:
   - The component returns the `article` directly, preceded by this suppression line (checked against Biome 2.6.6 with the repo's `recommended` preset: an `<article onClick>` raises only `lint/a11y/useKeyWithClickEvents`; `noStaticElementInteractions` does not fire on `article`, which has an implicit role, so do not add an ignore for it, since an unused suppression is itself an error):

```tsx
  return (
    // biome-ignore lint/a11y/useKeyWithClickEvents: the name link is the keyboard path; the click only extends it to the card body (OH-7)
    <article
```

     If Biome reports another rule on the `article`, STOP and ask.
   - `<article aria-label={project.name} onClick={openOnBodyClick} className="relative flex cursor-pointer flex-col gap-3 rounded-md border bg-card p-3 transition-colors hover:border-foreground/20 focus-within:border-foreground/20">`
   - a header row `flex items-start justify-between gap-2`: left a `flex min-w-0 flex-col` with the name `<Link to="/$org/$project/traces" params={{ org, project: project.slug }} className="focus-ring truncate rounded-sm text-ui font-medium hover:underline after:absolute after:inset-0 after:rounded-md after:content-['']">{project.name}</Link>` (the stretched link of OH-7: its `::after` covers the whole `relative` card) and the slug `<span className="truncate font-mono text-caption text-muted-foreground">{project.slug}</span>`; right, when `project.role` is set, `<Badge variant="secondary">{roleLabel(project.role)}</Badge>` (`@/lib/roles`).
   - the chart slot, always `h-8`: `loading` → `<Skeleton className="h-8 w-full" />`; `ready` with `activity.traces > 0` → `<ActivitySparkline from={state.from} buckets={state.activity.buckets} />`; `ready` with 0 traces → `<p className="flex h-8 items-center text-caption text-muted-foreground">No traces in the last 24 hours</p>`; `none` → the same `p` with "Activity unavailable".
   - The card holds no other link or button: the name is its only link (OH-7). The role `Badge` is a `span`.
   - `openOnBodyClick` (in the component, with `const navigate = useNavigate();` from `@tanstack/react-router`), exactly:

```tsx
/** A click on the card body opens the project (OH-7). A click on a link or button inside is theirs. */
function openOnBodyClick(event: MouseEvent<HTMLElement>) {
  if (event.defaultPrevented || (event.target as Element).closest("a, button")) {
    return;
  }
  void navigate({ to: "/$org/$project/traces", params: { org, project: project.slug } });
}
```

     (`import type { MouseEvent } from "react";`)
   - `<dl className="grid grid-cols-4 gap-2">` with four `<div className="flex min-w-0 flex-col">`, each `<dt className="text-caption text-muted-foreground">` then `<dd className="truncate text-body tabular-nums">`: "Traces" → `cells.traces`; "Error rate" → `cells.errorRate`; "Cost" → `cells.cost`; "Last trace" → `cells.lastIngestAt ? <RelativeTime value={cells.lastIngestAt} /> : "—"` (`@/components/relative-time`). While loading (`cells` undefined) each `dd` holds `<Skeleton className="h-4 w-10" />`.

   Tests `project-card.test.tsx` (root-route render as in H2's strip test, inside `TooltipProvider`): `links the name to the project's traces` (`href="/acme/support-bot/traces"`); `exposes exactly one link, named after the project` (`within(article).getAllByRole("link")` has length 1 and its name is "Support bot"); `stretches the name's link over the card` (the article has class `relative`, the link has `after:absolute` and `after:inset-0`); `navigates when the card body is clicked` (its own render helper, copying `renderBell` in `web/src/components/shell/alert-bell.test.tsx`: `const root = createRootRoute({ component: () => <TooltipProvider><Outlet /></TooltipProvider> });`, `const index = createRoute({ getParentRoute: () => root, path: "/", component: () => <ProjectCard org="acme" project={supportBot} activity={{ state: "loading" }} /> });`, `const traces = createRoute({ getParentRoute: () => root, path: "$org/$project/traces", component: () => <p>Traces page</p> });`, `const router = createRouter({ routeTree: root.addChildren([index, traces]), history: createMemoryHistory({ initialEntries: ["/"] }) });`, render it in a `QueryClientProvider` with `retry: false`, `await router.load()`. `org` is the prop `"acme"` and `project` is the fixture project `supportBot = projectsFixture.data[0]` (slug `support-bot`), so the card navigates with `params: { org: "acme", project: "support-bot" }`. Click the "Traces" `dt` text (`within(screen.getByRole("article", { name: "Support bot" })).getByText("Traces")`); then `findByText("Traces page")` and `router.state.location.pathname` is `/acme/support-bot/traces`); `shows slug and role`; `shows the activity numbers and the sparkline` (ready support-bot from the fixture: "42", "7.1%", "$1.23", a `time` element for Last trace, `img` "Traces in the last 24 hours"); `says when there are no traces` (playground: "No traces in the last 24 hours", "0", two "—", "$0"); `shows dashes when activity is unavailable` ("Activity unavailable", four "—"); `shows skeletons while loading`; `shows a dash for an unknown last trace even with traces` (build it by destructuring: `const { last_ingest_at: _unused, ...withoutTime } = supportActivity;` then `activity={{ state: "ready", from: activityFixture.from, activity: withoutTime }}`, where `supportActivity: Schema<"ProjectActivity">` is `activityFixture.data[0]`; `last_ingest_at` is optional in the schema, so `withoutTime` type-checks; if Biome flags the unused binding, use `const withoutTime = { ...supportActivity }; delete withoutTime.last_ingest_at;` instead. Last trace is "—" while Traces is "42").

4. `web/src/components/home/project-table.tsx`:

```ts
export function ProjectTable({
  org,
  rows,
  activity,
}: {
  org: string;
  rows: readonly Schema<"Project">[];
  activity: (id: string) => ActivityState;
}): JSX.Element;
```

   Move the current `ProjectTable` of `web/src/routes/$org/index.tsx` (lines 119–151) here unchanged (same `aria-label="Projects"`, the name `Link`, slug in mono, role `Badge`, `formatRetention`), and add four `TableHead`s after Retention: "Traces", "Error rate", "Cost", "Last trace", each with `className="text-right"`; their cells `className="text-right tabular-nums"` show `activityCells(activity(project.id))`: the three strings, and for Last trace `<RelativeTime>` or "—"; while loading each holds `<Skeleton className="ml-auto h-4 w-10" />`. (H4 deletes the old copy from the route file.)

   Tests `project-table.test.tsx`: `shows role, retention and slug` (the current page test's assertions: Support bot row has "Owner", "30 days", "support-bot"; Playground row has "Forever"); `shows the activity columns` (Support bot row: "42", "7.1%", "$1.23"; Playground row: "0", "—", "$0"); `shows dashes for unavailable activity`.

**Done when:** in a fresh worktree first `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just js-install`; then `node_modules/.bin/biome check --write web/src/api/queries.ts web/src/components/home/` passes; `cd web && ./node_modules/.bin/vitest run src/components/home --maxWorkers=8` passes.

**Commit:** `Add the org home's project card, table and activity hook`

---

## H4: the org home page

**Goal:** assemble the page from H2 and H3 and update its tests.

**Edit:**
- `web/src/routes/$org/index.tsx`
- `web/src/routes/home.test.tsx`

**Steps:**

1. `web/src/routes/$org/index.tsx`. Keep the route definition (loader, `pendingMs: 0`, `pendingComponent`), the `me` query, the infinite projects query, the error throw, the New project button with its `DisabledReason`, the Settings button, `NewProjectDialog` and Load more exactly as they are. Delete the local `ProjectTable` (now in `@/components/home/project-table`) and the imports only it used.
2. In `OrgHome`, before the `if (projects.error)` line (hooks run on every render):
   - `const [search, setSearch] = useState("");`
   - `const view = useProjectView();` (`@/lib/org-home`)
   - `const activity = useProjectActivity(org, projects.data?.pages.map((page) => page.data) ?? []);`
3. After the pending check: `const rows = projects.data.pages.flatMap((page) => page.data);` and `const shown = filterProjects(rows, search);`.
4. The `<main>` keeps its classes. Its children, in order:
   1. the header row as today, but without the `<h2>Projects</h2>` (only the `h1` with the org name stays on the left);
   2. `<UsageStrip org={org} />`;
   3. `<section className="flex flex-col gap-3">` containing (the section has no accessible name, so it is not a landmark; only the list and the table are named "Projects"):
      - a row `flex flex-wrap items-center gap-2`: `<h2 className="text-title font-medium">Projects</h2>`, `<span className="text-body text-muted-foreground">Last 24 hours</span>`, then, only when `rows.length > 0`, a `div` with `ml-auto flex items-center gap-2` holding `<Input aria-label="Search projects" placeholder="Search by name or slug" value={search} onChange={(event) => setSearch(event.target.value)} className="h-7 w-56 text-body" />` (`@/components/ui/input`) and `<ProjectViewSwitch />`;
      - `rows.length === 0` → the current `EmptyState` (unchanged text);
      - else `shown.length === 0` → `<p className="text-body text-muted-foreground">No projects match the search.</p>`;
      - else `view === "table"` → `<ProjectTable org={org} rows={shown} activity={activity} />`;
      - else `<ul aria-label="Projects" className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">` with `<li key={project.id}><ProjectCard org={org} project={project} activity={activity(project.id)} /></li>` per shown project;
      - the Load more button, unchanged, inside the section.
   4. `NewProjectDialog` as today.
5. `ProjectsPending`: keep `<main>` and its classes; children: `<Skeleton className="h-8 w-40" />`, `<Skeleton className="h-20 w-full" />`, then a `div` of class `grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3` with three `<Skeleton className="h-36 w-full" />`.
6. `web/src/routes/home.test.tsx`. Keep every test. Query the views by role, never with `getByLabelText("Projects")`: `getByRole("list", { name: "Projects" })` for the cards and `getByRole("table", { name: "Projects" })` for the table (the heading is `getByRole("heading", { name: "Projects" })`). Change only:
   - `shows role, retention and slug`: before the row assertions, `await user.click(screen.getByRole("radio", { name: "Table" }));` (take `user` from `renderApp`). Assertions unchanged.
   - Add, using the fixtures (`projects-activity.json` via an import like the existing fixture imports):
     - `shows a card per project with its last 24 hours`: `within(screen.getByRole("article", { name: "Support bot" }))` has "Owner", "support-bot", and after `findByText`, "42", "7.1%", "$1.23", the `img` "Traces in the last 24 hours"; the Playground article has "No traces in the last 24 hours", "0", "$0", and its Last trace is "—".
     - `opens the project when its card body is clicked`: click the "Traces" `dt` inside the Support bot article (`within(...).getByText("Traces")`); `waitFor` the pathname `/acme/support-bot/traces`. Override `/api/v1/projects/:project/traces` with an empty page as the "creates a project" test does, if the traces page needs it.
     - `shows a dash for an unknown last trace`: `server.use` the activity endpoint with the fixture whose first entry has no `last_ingest_at` (build it as in H3's card test: destructure `last_ingest_at` out of `activityFixture.data[0]`, then `{ ...activityFixture, data: [withoutTime, activityFixture.data[1]] }`); its card's Last trace `dd` is "—" while Traces is "42".
     - `switches to the table and remembers it`: click radio "Table"; a `table` named "Projects" shows the activity columns; `localStorage.getItem("cetra.org_home_view")` is `"table"`; a second `renderApp("/acme")` (after `cleanup()` from `@testing-library/react`) opens on the table.
     - `filters projects by name or slug in both views`: type `play` in "Search projects" → only the Playground article; clear, type `SUPPORT-B` → only Support bot; switch to Table → only the Support bot row; type `zzz` → "No projects match the search."
     - `hides search and switch when there are no projects`: the empty list test's setup; no "Search projects" textbox, no "View" radiogroup.
     - `shows dashes when the activity request fails`: activity → `problemResponse(500, "internal")`; the page renders; both cards say "Activity unavailable".
     - Note: the file's `project()` helper builds ids like `01900000-0000-7000-8000-first0000000`, which are not valid UUIDs. That is fine: the activity mock answers any `ids`, and the web code never parses them. New tests must not depend on the ids being valid, and must not expect activity for these projects (the fixture's entries are for the two fixture projects, so their cards read "Activity unavailable").
     - `asks the activity of each loaded page`: extend the Load more setup: capture the `ids` of each activity request; after Load more, the requests asked for `first`'s id and then `second`'s id.
     - `shows the org usage strip`: region "Usage this month" with the meter, "2,000", "Daily units" and the Plan link `href="/acme/settings/plan"`.
     - `keeps the page when usage fails`: usage → 500; projects still listed; no region "Usage this month".
     - `keeps the page when entitlements fail`: entitlements → 500; same.
   - The skeleton test keeps passing unchanged (`[data-slot='skeleton']` exists while the project list loads).

**Done when:** in a fresh worktree first `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just js-install`; then `node_modules/.bin/biome check --write web/src/routes/\$org/index.tsx web/src/routes/home.test.tsx` passes; `cd web && ./node_modules/.bin/vitest run src/routes/home.test.tsx src/components/home --maxWorkers=8` passes. Verify by hand with `just web-dev` (coordinator, after the commit): cards, switch, search, strip, light and dark themes.

**Commit:** `Show activity cards and the usage strip on the org home`

---

## H5: decision, spec 11, docs, changelog, roadmap

**Goal:** record D-C-27 and describe the page where users and implementers read about it.

**Edit:**
- `docs/mvp-decisions.md`
- `docs/spec/11-ui.md`
- `website/src/content/docs/docs/get-started/ui-tour.mdx`
- `CHANGELOG.md`
- `docs/roadmap.md`

**Steps:**

1. `docs/mvp-decisions.md` § Track C: in the D-C-20 row, after the sentence that says the dot reads `last_ingest_at` on `GET /projects/{project}`, add " The org home's activity endpoint also returns it (D-C-27)." (if that sentence is not there, add it at the end of the row's decision cell and say so in your report). Then add a row after D-C-26, in the same three-cell form as D-C-25 and D-C-26 (`| # | Topic | Decision |`, no source cell):

```
| D-C-27 | Org home | **The org home shows one activity card per project, a Cards/Table switch, one search box and an org usage strip.** A card opens the project's Traces when clicked anywhere (a stretched link: the name is the card's only real link, for keyboard and screen readers) and shows the name, slug, the caller's role, and for the last 24 hours the traces, error rate, LLM cost (the sum of the traces' `cost_total` with the default exclusions, the Overview's Cost tile) and a sparkline of 24 hourly buckets with error traces stacked on the OK ones, plus the last ingest time of D-C-20 ("—" when the server does not know it, even when there are traces). This extends D-C-20: `last_ingest_at` is still kept in memory only, and is now returned by `GET /projects/{project}` and by the activity endpoint, never by the project list. The 24 hours are the epoch-aligned UTC hours whose last one holds now. Cards are the default; the table keeps Name, Slug, Your role and Retention and adds Traces, Error rate, Cost and Last trace. The choice is stored per browser as `cetra.org_home_view`. The search filters the loaded projects on name or slug in both views. Above the projects, a strip shows the month's units against the plan, the month's spans, scores and judge cost, units per day and a link to Plan; it hides itself when its requests fail. Data: `GET /api/v1/orgs/{org}/projects/activity?ids=` answers 1–50 projects the caller can see (the list's visibility) in one request; the server runs the widget query of spec 05 once per project, at most 4 at a time under one 30 s deadline, because storage opens queries per project and has no cross-project plan; a project whose query fails is listed in `unavailable`. No server cache (the window moves with every request); the UI keeps answers 30 s and refetches every 60 s, one request per loaded page of 50. Rejected: activity fields on `GET /orgs/{org}/projects` (every list call, including the CLI's, would pay N queries), latency and tokens on the card (user's choice), server-side search. |
```

2. `docs/spec/11-ui.md`:
   - § Navigation, the line `/:org    org home (projects list)`: replace the description with `org home: usage strip and project activity cards (D-C-27)`, keeping the column alignment.
   - § Pages: add, right after the `## Pages` heading and before `### Filter bar`:

```
### Org home

The org home (D-C-27) has the org name and the New project (admins and owners; others see why it is disabled) and Settings buttons, then a usage strip: the month's units against the plan with the 80 % and 100 % marks, the month's spans, scores and judge cost, a bar per day of units, and a Plan link; the strip is a skeleton while loading and disappears when `GET /orgs/{org}/entitlements` or `GET /orgs/{org}/usage` fails. Below it, Projects with "Last 24 hours", a search box (name or slug, case-insensitive, over the loaded projects) and a Cards/Table switch stored per browser. A card: the whole card opens the project's Traces (the name is its one real link), then name, slug, role badge, a 24-bar sparkline of hourly traces with errors stacked on top, then Traces, Error rate, Cost and Last trace (relative; "—" when unknown). The table has Name, Slug, Your role, Retention, Traces, Error rate, Cost and Last trace. Cards keep the list order (name). The whole card is clickable (stretched link on the name, the card's only link). Activity comes from `GET /orgs/{org}/projects/activity`, one request per loaded page; a project without an answer shows "Activity unavailable" and dashes. No projects: the "No projects yet" empty state, without search or switch; Load more pages the list as before.
```

3. `website/src/content/docs/docs/get-started/ui-tour.mdx`: add a section after the intro paragraph and before `## The layout`:

```
## Organization home

Opening an organization shows its home page:

- A usage strip: this month's units against your plan, the month's spans, scores and judge cost, units per day, and a link to Plan.
- One card per project you can see. Click a card to open the project's traces. Each card shows its name, slug and your role, and for the last 24 hours: the number of traces, the error rate, the LLM cost, a chart of traces per hour with errors on top, and when the server last received spans for it. That time shows "—" until the project receives spans after the server starts.
- A switch between cards and a table. The table also shows each project's retention. cetra remembers the choice in your browser.
- A search box that filters the projects by name or slug.
- New project (admins and owners) and Settings.
```

   Keep the site's style (`website/STYLE.md`): no banned words (`website/scripts/check-terms.mjs`), "cetra" in lower case.
4. `CHANGELOG.md` § `[Unreleased]`: add a `### Added` section above `### Changed` with one line: `- The organization home shows a card per project with its last 24 hours (traces, error rate, LLM cost, an hourly chart with errors, the last ingest time), a cards/table switch, a search box, and this month's usage against the plan. \`GET /api/v1/orgs/{org}/projects/activity\` serves the cards.`
5. `docs/roadmap.md`, row **C** (line 37), in the status cell after the Facets sentence (`… [\`plans/c-web-ui/facets-batch.md\`](plans/c-web-ui/facets-batch.md).`): append ` Org home activity cards and usage strip (D-C-27): [\`plans/c-web-ui/org-home.md\`](plans/c-web-ui/org-home.md).`

**Done when:** `cd website && node scripts/check-terms.mjs` prints nothing and exits 0. No other check (the coordinator's `just check` builds the site).

**Commit:** `Document the org home's activity cards`

---

## Final checks (coordinator)

After H1–H5 are merged into `claude/org-home`:
1. Sonnet code review of the whole branch against `origin/main` (fetch first); fixes by a Haiku agent with an exact brief.
2. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`.
3. Browser check on the mocks (`just web-dev`): `/acme` in light and dark, at 375 px, 768 px and desktop widths; the switch survives a reload; search in both views; the strip's Plan link.
4. Report: what was done, deviations, and that docs screenshots wait for the product freeze.
