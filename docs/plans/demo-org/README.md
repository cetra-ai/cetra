# Demo org

Cross-cutting (server, web, tools). Plan for 7 phases, DO1–DO7, each one commit, done in order. Do one phase, nothing more. Decision **D187**; spec 08 § Demo org, spec 09 (`GET /api/auth/me`, `GET /api/v1/orgs`, 429), spec 11 § Navigation → Demo org, spec 12 § Configuration (`demo.*`) and § Demo org.

## Goal

Like Langfuse's demo project: every signed-in user of a cloud cell finds a **"cetra Demo" org** in their org switcher, next to their own org. They browse it read-only (traces, sessions, users, dashboards, datasets, experiments, evaluators, the SQL console). The website's "Try demo" link opens `https://<cell>/demo`. The data is live: a sidecar generator ingests realistic traces all the time, and real online rules score them with a judge the generator serves.

## Decisions (already made by the user, do not revisit)

1. The demo is **an org on the cloud cell where users have their orgs** (today `eu1`), not a separate instance and not an anonymous page. Visitors have full cloud accounts (they sign up on the control plane, own their org, can upgrade).
2. **Implicit viewer, no membership row** (D187). `demo.org` names the org by id; every signed-in user who is not an instance admin reads it as `viewer` unless they have a real row. No seat, no invite, nothing written.
3. Visitors see only the real members (the team) in member lists, never each other.
4. The SQL console stays open, with caps (10 s, 1,000 rows). Viewers cannot export (existing rule).
5. Limits on implicit visitors: 120 requests a minute and 4 in flight per user, 16 in flight across all visitors.
6. Data from a **sidecar generator** in `tools/demo` (`cetra-demo`, not shipped in `cetra`): 30-day backfill, then continuous ingest built from the captured fixtures in `fixtures/otlp/`.
7. Scores from **real evaluators and online rules**, whose LLM judge is an OpenAI-compatible endpoint served by the generator on a public hostname (the cell keeps `security.allow_private_network_egress = false`).
8. The demo org is created and configured by the team, by hand, following `tools/demo/README.md` (DO6). Nothing seeds evaluators, rules or dashboards automatically.

## Rules from AGENTS.md that matter here

- No compatibility layers, no placeholders, no unneeded abstractions. Do not add a trait, wrapper or generic unless this plan says so.
- `unwrap`/`expect` only in tests. Never panic on network or disk input.
- Logs never contain payload content, emails or keys; log ids and counts.
- Every behaviour change has tests. UI tests use MSW with `server.use`, never module mocks.
- Node: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before any web check.
- Commit message: short imperative subject, no attribution line. One commit per phase.
- **Never guess.** If the code differs from what a phase quotes, or a step is ambiguous, stop and end your turn with `QUESTION:` and the exact questions.

## Facts about the current code (verified on 2026-11-18; re-check before use)

- `crates/server/src/meta/members.rs`: `pub(crate) async fn org_role(pool, org_id: OrgId, user_id: Uuid) -> Result<Option<OrgRole>, MetaError>` reads `org_members`. Also `org_role_tx` (writer connection).
- `crates/server/src/meta/orgs.rs`: `pub(crate) async fn orgs_of_user(pool, user_id) -> Result<Vec<(OrgRow, OrgRole)>, MetaError>` (active orgs, by name then id). `org_by_id`, `org_by_slug`. `const RESERVED: &[&str]` lists the org slugs the UI needs (`"account"`, `"admin"`, … `"verify"`).
- Callers of `orgs_of_user`: `routes/auth.rs` `me_from` (builds `Me { user, orgs: Vec<OrgMembership> }`), `routes/orgs.rs` `list_orgs` (`GET /api/v1/orgs`, paged by name), `mcp/tools.rs` `list_projects`, `routes/internal_members.rs` (control plane; **must keep real rows only**).
- `crates/server/src/http/access.rs`: `build_router` wraps every route in `enforce(state, access, path, request, next)`, which calls `prepare(state, access, request) -> Result<Request, ApiError>` and then `next.run(request).await`. `grant_org` and `user_project` call `org_role(state.meta.reader(), org.id, user.id)`; a `None` is 404. `user_project` then applies `project_override` and `roles::effective_project_role`.
- `routes/deletions.rs` `drop_project` calls `org_role` to require owner or admin; an implicit viewer must stay refused there, so it keeps calling the raw `org_role`.
- `crates/server/src/api/account.rs`: `pub struct OrgMembership { id: OrgId, slug: String, name: String, role: OrgRole }` (also used by `GET /api/v1/orgs`).
- `crates/server/src/auth/principal.rs`: `AuthUser { id: Uuid, is_instance_admin: bool, email: String }`. `Principal::Session { user, .. }`, `Principal::Pat { user, .. }`.
- `crates/server/src/auth/limits.rs`: `Bucket { capacity, per }`, `KeyedLimiter<K>` (`check(&key, now) -> Result<(), RetryAfter>`, `prune`), `McpLimits` (a `KeyedLimiter<Uuid>` plus an `Arc<Mutex<HashMap<Uuid, u32>>>` of requests in flight, guard `McpGuard` decrements on drop), `Limits::new()` built in `serve.rs` and `test_support/mod.rs`.
- `crates/server/src/routes/sql.rs`: `check_request(body) -> Result<CheckedSql, ApiError>` with `limit` (1–`SQL_MAX_ROWS` = 10,000, default 10,000) and `timeout` (1,000–60,000 ms, default 60,000); `run_sql` is the handler.
- `crates/server/src/config.rs`: `ServerConfig` (`#[serde(deny_unknown_fields)]`), sections like `pub orgs: OrgsConfig` with `#[serde(default)]`, `fn validate(config: &mut ServerConfig)`.
- `web/src/routes/index.tsx`: the loader redirects to `me.orgs[0]`. `web/src/components/org-switcher.tsx` maps `me.orgs`.
- `fixtures/otlp/<group>/<case>/input.json` is an OTLP `ExportTraceServiceRequest` in OTLP JSON (`resourceSpans[].scopeSpans[].spans[]`, hex `traceId`/`spanId`, `startTimeUnixNano`/`endTimeUnixNano` as strings). `/v1/traces` accepts `application/json` (spec 02).
- Spec 06 § `llm_judge`: the OpenAI-compatible adapter posts `{base_url}/chat/completions` with `response_format: {type: "json_schema", strict: true, json_schema: {…}}`; the schema is an object with `explanation` (only with `include_reason`) then `value` (number or boolean) or `label` (enum).

---

## DO1: config and the reserved slug (server)

**Edit** `crates/server/src/config.rs`, `crates/server/src/serve.rs`, `crates/server/src/meta/orgs.rs`, `docs/self-hosting/configuration.md`, `CHANGELOG.md`.

**Steps**
1. `pub struct DemoConfig` with `#[serde(deny_unknown_fields, default)]`: `org: Option<OrgId>`, `requests_per_minute: u32` (120), `user_in_flight: u32` (4), `org_in_flight: u32` (16), `sql_timeout: Duration` (10 s), `sql_max_rows: usize` (1000). Parse `sql_timeout` with the same duration format and helper the existing config sections use (spec 12: strings like `"10s"`); if `config.rs` has no such helper for server sections, stop and ask. Add `pub demo: DemoConfig` to `ServerConfig` with `#[serde(default)]` and a doc comment citing D187.
2. In `validate`: each count ≥ 1; `user_in_flight ≤ org_in_flight`; `sql_timeout` within 1–60 s; `sql_max_rows` within 1–10,000. Errors name the key, never a value that could be secret (these are not secret, but follow the file's style).
3. In `serve.rs`, after `meta.db` is open and migrated and before the listeners start: when `demo.org` is set, load it with `org_by_id`; missing or not `active` fails startup with an error naming the key and the id. Find the place where `serve.rs` first reads `meta.db` after migrations; if there is no clear one, stop and ask.
4. Add `"demo"` to `RESERVED` in `meta/orgs.rs` (the UI route `/demo`).
5. Document the keys in `docs/self-hosting/configuration.md` next to `[orgs]`, as spec 12 lists them. `CHANGELOG.md` `Added`: "A demo org that every signed-in user can read (`demo.org`)."

**Tests**
- config: defaults; a full `[demo]` table from TOML; `CETRA_DEMO__ORG` from the environment; each validation error; an unknown key under `[demo]` is an error.
- serve: startup fails when `demo.org` names no org, and when the org is not active; starts when it is active (use the existing startup test helpers in `serve.rs`).
- orgs: creating an org with slug `demo` is refused like the other reserved slugs.

**Done when** `just check-rust` passes.

---

## DO2: the implicit viewer (server)

**Edit** new `crates/server/src/demo.rs` (and `lib.rs` to declare it), `http/access.rs`, `routes/auth.rs`, `routes/orgs.rs`, `mcp/tools.rs`, `api/account.rs`, `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (via `just openapi`), `fixtures/api/` responses that contain orgs, `CHANGELOG.md` only if DO1's line needs no change (it does not).

**Steps**
1. `demo.rs`:
   - `pub(crate) struct OrgRoleOf { pub(crate) role: OrgRole, pub(crate) implicit: bool }`.
   - `pub(crate) async fn org_role_for(state: &AppState, org_id: OrgId, user: &AuthUser) -> Result<Option<OrgRoleOf>, MetaError>`: the stored role (`implicit: false`) when `org_members` has one; else `Some(Viewer, implicit: true)` when `state.config.demo.org == Some(org_id)` and `!user.is_instance_admin`; else `None`.
   - `pub(crate) async fn orgs_for(state: &AppState, user: &AuthUser) -> Result<Vec<(OrgRow, OrgRole, bool)>, MetaError>`: `orgs_of_user` with `demo = (Some(org.id) == demo.org)` on each, then, when `demo.org` is set, the user is not an instance admin and not already a member, the demo org (loaded with `org_by_id`, only if `active`) as `(row, Viewer, true)`; the demo org (member or not) is moved to the **end**.
2. `access.rs`: `grant_org` and `user_project` use `org_role_for` instead of `org_role` (take `.role`). When the role is implicit, insert a `DemoVisitor` marker (`#[derive(Clone, Copy)] pub(crate) struct DemoVisitor;` in `demo.rs`) into `parts.extensions`. `user_project` is also called outside the middleware: check its callers and keep their behaviour (an implicit viewer is a viewer there too); if a caller needs the marker and cannot get it, stop and ask.
3. `api/account.rs`: `OrgMembership` gains `pub demo: bool` with a doc comment (D187). `me_from` and `list_orgs` build it from `orgs_for`. `list_orgs` pages by name today (`page_named`); keep the demo org **last** across pages: page the non-demo orgs as today and append the demo org to the last page only. If `page_named` makes that awkward, stop and ask before changing it.
4. `mcp/tools.rs` `list_projects`: iterate `orgs_for` (the demo org last); update its doc comment.
5. Leave `routes/internal_members.rs` and `routes/deletions.rs` on the raw `orgs_of_user` / `org_role`.
6. `just openapi`; update every `fixtures/api` response that contains an `OrgMembership` with `"demo": false`; run `cargo test -p cetra-server --test api_examples`.

**Tests** (in `demo.rs` and the access suite, with `TestApp`)
- with `demo.org` set: a user with no row gets `200` on `GET /api/v1/orgs/{demo}` and on a project read (`GET …/traces`), `200` on `POST …/sql`, `403` on a member route (e.g. creating a score definition) and on a SQL export (`Accept: text/csv`), `403` on a project-admin route;
- a real member of the demo org keeps their role (`implicit: false`, no `DemoVisitor`);
- an instance admin without a row gets `404`;
- without `demo.org`, the same user gets `404` (today's behaviour);
- `GET /api/auth/me` and `GET /api/v1/orgs`: the demo org is last with `demo: true` and role `viewer`; other orgs have `demo: false`; with a `limit=1` page walk, the demo org comes last;
- `GET /api/v1/orgs/{demo}/members` lists the real members only;
- `POST /api/internal/v1/memberships/lookup` does not return the demo org for a user without a row;
- MCP `list_projects` includes the demo projects last;
- `DELETE …/projects/{p}` by an implicit viewer is `403`.

**Done when** `just check-rust` passes and `cargo test -p cetra-server --test api_examples` passes.

---

## DO3: limits for implicit visitors (server)

**Edit** `auth/limits.rs`, `http/access.rs`, `routes/sql.rs`, `serve.rs` and `test_support/mod.rs` (the `Limits::new` calls).

**Steps**
1. `limits.rs`: move the in-flight map of `McpLimits` into `pub(crate) struct InFlight<K>` (`admit(&self, key) -> Result<InFlightGuard<K>, RetryAfter>` with a per-key maximum, guard decrements on drop and removes zero entries). `McpLimits` uses it with `MCP_IN_FLIGHT`; its behaviour and tests stay the same. This generic removes the duplication the demo limits would add (AGENTS.md).
2. `pub(crate) struct DemoLimits { rate: KeyedLimiter<Uuid>, users: InFlight<Uuid>, org: Arc<AtomicU32> or a Mutex<u32>, org_max: u32 }` built from `DemoConfig` (`Bucket { capacity: requests_per_minute, per: 60 s }`). `admit(user, now) -> Result<DemoGuard, RetryAfter>`: org count full → `RetryAfter(1 s)` without spending a token; user in flight full → `RetryAfter(1 s)`; then the rate check; the guard releases both slots on drop. `Limits::new` takes `&DemoConfig`; `Limits` gains `pub(crate) demo: DemoLimits`; `prune` prunes it.
3. `access.rs` `enforce`: when `prepare` inserted `DemoVisitor`, call `state.limits.demo.admit(user.id, now)` before `next.run`; on `Err(RetryAfter)` answer `429 rate_limited` with `Retry-After` exactly as the MCP route does (reuse its helper; find it in `mcp/` or `http/error.rs`). Hold the guard across `next.run(request).await`. A streaming answer (SSE) releases its slot when the handler returns, so open streams are not counted; that is intended (the streams have their own caps).
4. `routes/sql.rs`: `run_sql` takes `Option<Extension<DemoVisitor>>`; for a visitor, after `check_request`, lower `limit` to `demo.sql_max_rows` and `timeout` to `demo.sql_timeout` when they are larger. The field rules and their 400s are unchanged.

**Tests**
- `InFlight`: admit up to the max, refuse the next, a dropped guard frees a slot, the map is empty after all guards drop.
- `McpLimits` tests unchanged and passing.
- `DemoLimits`: org-wide cap across two users; per-user cap; rate refusal with the right `Retry-After`; a refused admission spends no rate token.
- access: an implicit visitor over the rate gets `429` with `Retry-After`; a real member of the demo org with the same request count is not limited; the slot is released after the response (a second request after the first finishes is admitted with `user_in_flight = 1`).
- sql: a visitor's `limit: 5000` returns at most `sql_max_rows` rows with `truncated: true` when more matched; a visitor's `timeout_ms: 60000` runs with `sql_timeout` (use a test config with a tiny `sql_timeout` and a query that the existing tests already use to provoke a timeout; if none exists, stop and ask); a member's request is unchanged.

**Done when** `just check-rust` passes.

---

## DO4: the web (switcher, landing, /demo, banner)

**Edit** `web/src/routes/index.tsx`, new `web/src/routes/demo.tsx`, `web/src/components/org-switcher.tsx`, the org layout route that renders the top bar (find it from `web/src/components/shell/top-bar.tsx`'s users), a new `web/src/components/demo-banner.tsx`, tests next to each, the MSW handlers in `fixtures/api` only if a new example is needed.

**Steps**
1. `index.tsx`: redirect to the first org with `demo === false`, else the first org (the demo).
2. `demo.tsx` (`/demo`, needs a session like every non-public route): find the org with `demo === true` in `me.orgs`; load its projects with the existing projects query and redirect to the first by name (`/$org/$project`); no project → `/$org`; no demo org → `/`.
3. `org-switcher.tsx`: render the demo org last, after a separator, with a small "Demo" badge (use the existing `Badge` component from `web/src/components/ui`).
4. `demo-banner.tsx`: in an org whose membership has `demo: true` and `role: "viewer"`, a thin bar under the top bar: "You're exploring the cetra demo, read-only." and a link "Send your own traces" to `/`. Nothing for other orgs or for real members.

**Tests**
- index: with orgs `[demo(first by name), own]` it opens `own`; with only the demo it opens the demo.
- `/demo`: opens the demo org's first project; no demo org → `/`; signed out → `/login?redirect=/demo` (the existing auth gate).
- switcher: the demo org is last and badged.
- banner: shown for a demo viewer, hidden for an `admin` of the demo org and in a normal org.

**Done when** `just check-web` passes.

---

## DO5: the generator, ingest (tools/demo)

**Edit** new crate `tools/demo/` (`Cargo.toml`, `src/main.rs`, `src/*.rs`), root `Cargo.toml` `[workspace] members` (and `[workspace.dependencies]` only if a needed crate is missing: look up the real version, never guess), `justfile` (a `demo` recipe running it), `deny.toml` only if a new crate needs it.

**Steps**
1. Binary `cetra-demo`, `publish = false`, `[lints] workspace = true`, errors with `thiserror` (it is not `cetra-cli`, so no `anyhow`). Config by flags with env fallbacks (`clap` `env`): `--url` (`CETRA_DEMO_URL`, the cell's base URL), `--key` (`CETRA_DEMO_KEY`, an `sk` key; never logged), `--fixtures` (default `fixtures/otlp`), `--rate` (traces per minute at the daily peak, default 60), `--backfill-days` (default 30), `--seed` (optional, for tests).
2. Load every `input.json` under `--fixtures` (skip groups `edge` and any case whose `case.json` `origin` is not `captured`; if that leaves fewer than 10 templates, stop and ask). A template is one fixture's `resourceSpans`.
3. To emit a trace from a template: a new random trace id for every trace id of the request (keep the mapping so parent links stay valid), new span ids likewise; shift every `startTimeUnixNano`/`endTimeUnixNano` so the root starts at the target time, scaling all durations by a factor in 0.5–2.0 (one factor per trace); multiply integer token-count attributes (`gen_ai.usage.*`, `llm.token_count.*`) by 0.7–1.3; set resource or root attributes `user.id` (from a pool of 200 fake ids `user-0001`…), `session.id` (a session groups 1–8 consecutive traces of one user), `deployment.environment` (`production` 90%, `staging` 10%) and `service.version` (a release that changes every 7 days); mark 3% of traces as errors (root span `status.code = 2` with a fixed message). Only touch attributes that exist in spec 02's mapping tables; if a convention stores the user or session elsewhere, follow spec 02 and say which in a comment.
4. Daily curve: the rate follows a smooth day/night curve (peak at 14:00 UTC, trough at 25% of the peak at 03:00 UTC), with ±20% noise.
5. Backfill: when the project has no trace in the last `--backfill-days` (ask `GET /api/v1/projects/current/traces?limit=1` with the key), emit that many days at the curve's rate, oldest first, in batches of at most 500 traces per request; then the live loop emits at the current rate.
6. `POST {url}/v1/traces` with `Content-Type: application/json`, `Authorization: Bearer <key>`; on 429/503 wait the `Retry-After` (or 5 s) and retry; on 402 (units cap) log it and wait 10 minutes. Log counts only.

**Tests** (unit, with `--seed`): ids are all new and links stay consistent; times land at the target and durations scale; token counts scale and stay integers; the curve's peak and trough; a batch never exceeds 500 traces; a wiremock-free HTTP test with a local `axum` server (as `tools/capture` tests do) receiving one batch with the bearer header and JSON content type, and the 429 retry.

**Done when** `just check-rust` passes and `just demo -- --help` prints the flags.

---

## DO6: the judge endpoint, image and runbook (tools/demo)

**Edit** `tools/demo/src/*`, `tools/demo/Dockerfile`, `tools/demo/README.md`, `justfile` (`demo-image`).

**Steps**
1. `--judge-addr` (`CETRA_DEMO_JUDGE_ADDR`, unset = off) and `--judge-key` (`CETRA_DEMO_JUDGE_KEY`, required when the address is set): an `axum` server with `POST /v1/chat/completions` that requires `Authorization: Bearer <judge-key>` (else 401), reads `response_format.json_schema.schema` and answers a Chat Completions body whose message content is a JSON object matching that schema: `explanation` (one of 20 canned sentences) when present, `value` a number in [0, 1] drawn from a beta-like spread centred near 0.8 or a boolean (80% `true`), or `label` the first enum value 70% of the time and the others otherwise. `usage` with plausible token counts. A request without that schema is `400`. Never log the request body.
2. `Dockerfile`: multi-stage, builds `cetra-demo` from the workspace and copies `fixtures/otlp` into the image; runs as a non-root user. `just demo-image [tag]` builds it.
3. `README.md`, the runbook of spec 12 § Demo org: create the org through the control plane with a team account; ask the control plane operator for its override (no data-access window, no units cap, an ingest rate above the generator's); set `demo.org`; in the org create the project `demo`, an `sk` key (`ingest, write`), an LLM connection (OpenAI-compatible, base URL `https://<judge-host>/v1`, the judge key, `structured_output = native`); the evaluators (two built-in judge templates and one `check`), two online rules (sampling 20%), score definitions, one alert rule, and two dashboards (names and settings listed in the README); the compose service and the reverse-proxy route for the judge host. Use team email addresses only.

**Tests**: the judge answers each schema kind (numeric, boolean, categorical, with and without `explanation`) and the answer parses with cetra's own judge parser (add `cetra-eval` as a dev-dependency only if it exposes that parser publicly; otherwise assert the JSON shape and say so in the report); 401 without the key; 400 without a schema.

**Done when** `just check-rust` passes and `just demo-image` builds (if Docker is unavailable, say so in the report).

---

## DO7: closeout

**Edit** `docs/roadmap.md` (a line under "Open" or the relevant section only if the coordinating session asks), `CHANGELOG.md`, this README (status line).

**Steps**
1. Write the control-plane request in this README's § Requests to the control plane (below) as done or pending, as the user says.
2. Run `just check`.

**Done when** `just check` passes.

---

## Requests to the control plane (cetra-control)

Not done here; the user forwards them.

- **R-demo-1**: an org override for the demo org: no data-access window, no units cap, ingest rate at least the generator's peak; exempt from "one Free org per account" (it belongs to a team account).
- **R-demo-2**: the eu1 cell's generated config sets `demo.org`; the compose stack runs `cetra-demo` (image of DO6) with the `sk` key and judge key from the cell's env file, and the reverse proxy serves the judge on its own hostname.
- **R-demo-3** (later, with a second region): each region's cell gets its own demo org and generator, and "Try demo" picks the visitor's region.

## Not in this plan

- Custom dashboards of demo visitors in the browser: `docs/plans/c-web-ui/custom-dashboards.md` CD8, amended to key on the org's `demo` flag (D-C-17).
- Realistic prompt and answer text: the generator replays the captured fixtures' text. A text catalogue per fake app is a later improvement.
