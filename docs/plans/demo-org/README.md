# Demo org

Status: DO1–DO6 implemented on 2026-11-18 on three branches: `claude/demo-org-impl` (DO1–DO3), `claude/demo-web` (DO4, cut from DO2), `claude/demo-gen` (DO5–DO6), not merged. Reviewed (Sonnet): no blocker; the minors are fixed. Not verified: the demo image (no Docker on the build machine), a run against a real cell, the UI in a browser. The control-plane requests below are pending.

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

**Edit** `crates/server/Cargo.toml`, `crates/server/src/config.rs`, `crates/server/src/serve.rs`, `crates/server/src/meta/orgs.rs`, `CHANGELOG.md`.

**Steps**
1. Add `humantime-serde = { workspace = true }` to `crates/server/Cargo.toml` (the root already declares it; `cetra-storage` uses it as `#[serde(with = "humantime_serde")]`, `crates/storage/src/config.rs`). `pub struct DemoConfig` with `#[serde(deny_unknown_fields, default)]`: `org: Option<OrgId>`, `requests_per_minute: u32` (120), `user_in_flight: u32` (4), `org_in_flight: u32` (16), `#[serde(with = "humantime_serde")] sql_timeout: Duration` (10 s; parses `"10s"`), `sql_max_rows: usize` (1000). Implement `Default` by hand, like `OrgsConfig`. Add `pub demo: DemoConfig` to `ServerConfig` with `#[serde(default)]` and a doc comment citing D187.
2. In `validate`: each count ≥ 1; `user_in_flight ≤ org_in_flight`; `sql_timeout` within 1–60 s; `sql_max_rows` within 1–10,000. Errors name the key, never a value that could be secret (these are not secret, but follow the file's style).
3. In `serve::bind`, directly after `MetaDb::open` succeeds and before `open_secrets`: when `demo.org` is set, load it with `org_by_id`. Missing → `ServeError::Config(ConfigError::Invalid { key: "demo.org", reason: "no org has this id" })`; not `active` → the same with `reason: "the org is not active"` (`reason` is `&'static str`; never print the id). On failure call `release_storage(storage).await; meta.close().await;` first, as the neighbouring error arms do.
4. Add `"demo"` to `RESERVED` in `meta/orgs.rs` (the UI route `/demo`).
5. Spec 12 already lists the keys; `docs/self-hosting/configuration.md` is not changed. `CHANGELOG.md` `Added`: "A demo org that every signed-in user can read (`demo.org`)."

**Tests**
- config: defaults; a full `[demo]` table from TOML; `CETRA_DEMO__ORG` from the environment; each validation error; an unknown key under `[demo]` is an error.
- serve: `bind(config, None).await.unwrap_err()` matches `ServeError::Config(ConfigError::Invalid { key: "demo.org", .. })` when `demo.org` names no org, and when the org is not active (model on `a_bad_init_secret_fails_startup`); `bind` succeeds when it is active.
- orgs: creating an org with slug `demo` is refused like the other reserved slugs.

**Done when** `just check-rust` passes.

---

## DO2: the implicit viewer (server)

**Edit** new `crates/server/src/demo.rs` (and `lib.rs` to declare it), `http/access.rs`, `http/access_suite.rs`, `routes/auth.rs`, `routes/orgs.rs`, `routes/invites.rs`, `mcp/tools.rs`, `api/account.rs`, `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (via `just openapi`), the `fixtures/api/` responses that contain an org membership, and the web test and mock literals that `pnpm tsc` reports.

**Steps**
1. `demo.rs`:
   - `pub(crate) struct OrgRoleOf { pub(crate) role: OrgRole, pub(crate) implicit: bool }`.
   - `pub(crate) async fn org_role_for(state: &AppState, org_id: OrgId, user: &AuthUser) -> Result<Option<OrgRoleOf>, MetaError>`: the stored role (`implicit: false`) when `org_members` has one; else `Some(Viewer, implicit: true)` when `state.config.demo.org == Some(org_id)` and `!user.is_instance_admin`; else `None`.
   - `pub(crate) async fn orgs_for(state: &AppState, user_id: Uuid, is_instance_admin: bool) -> Result<Vec<(OrgRow, OrgRole, bool)>, MetaError>` (`me_from` has a `UserRow`, which has `id` and `is_instance_admin`; `list_orgs` and MCP pass the `AuthUser` fields): `orgs_of_user` with `demo = (Some(org.id) == demo.org)` on each, then, when `demo.org` is set, the user is not an instance admin and not already a member, the demo org (loaded with `org_by_id`, only if `active`) as `(row, Viewer, true)`; the demo org (member or not) is moved to the **end**.
2. `access.rs`: `grant_org` and `user_project` use `org_role_for` instead of `org_role`. In `demo.rs`: `#[derive(Clone, Copy)] pub(crate) struct DemoVisitor(pub(crate) Uuid);` (the user id). `ProjectAccess` and `OrgAccess` gain `pub(crate) implicit: bool`: `true` when `org_role_for` said implicit, `false` for API-key access (`grant_key_project` and the key-only path). `grant_org` inserts `DemoVisitor(user.id)` into `parts.extensions` when its role is implicit; `grant_session_project` inserts it when `access.implicit`. `user_project`'s other callers (the MCP tools in `mcp/sql.rs`, `mcp/traces.rs`, `mcp/tools.rs`) only read `.role` and ignore the new field.
3. `api/account.rs`: `OrgMembership` gains `pub demo: bool` with a doc comment (D187). **Every** constructor sets `demo = state.config.demo.org == Some(org.id)`: `me_from` and `list_orgs` (from `orgs_for`), the setup response in `routes/auth.rs`, `membership()` in `routes/orgs.rs` (used by create, get and update org), the invite accept in `routes/invites.rs`, and the test in `api/account.rs`. `list_orgs`: do not change `page_named` (its cursor assumes rows sorted by `(name, id)`); give the demo row the sort key `format!("\u{10ffff}{}", org.name)` as the first tuple element, so it sorts after every real name and paging stays valid.
4. `mcp/tools.rs` `list_projects`: iterate `orgs_for` (the demo org last); update its doc comment.
5. Leave `routes/internal_members.rs` and `routes/deletions.rs` on the raw `orgs_of_user` / `org_role`.
6. `just openapi`. Add `"demo": false` to every org membership object (`id` + `slug` + `name` + `role`) in `fixtures/api/static/` (at least `me.json`, `orgs.json`, `org.json`, `setup-response.json`, `signup-response.json`; check `invite-created.json`, `invite-preview.json` and `project.json` and change them only if they hold such an object). Run `pnpm exec tsc --noEmit` in `web/` and add `demo: false` to every `OrgMembership` literal it reports (tests and `web/src/mocks`). Run `cargo test -p cetra-server --test api_examples`.

**Tests** (in `demo.rs`, and in `http/access_suite.rs` with an implicit viewer added as a `Who` next to `org_membership(who)`)
- with `demo.org` set: a user with no row gets `200` on `GET /api/v1/orgs/{demo}` and on a project read (`GET …/traces`), `200` on `POST …/sql`, `403` on a member route (e.g. creating a score definition) and on a SQL export (`Accept: text/csv`), `403` on a project-admin route;
- a real member of the demo org keeps their role (`implicit: false`, no `DemoVisitor`);
- an instance admin without a row gets `404`;
- without `demo.org`, the same user gets `404` (today's behaviour);
- `GET /api/auth/me` and `GET /api/v1/orgs`: the demo org is last with `demo: true` and role `viewer`; other orgs have `demo: false`; with a `limit=1` page walk, the demo org comes last;
- `GET /api/v1/orgs/{demo}/members` lists the real members only;
- `POST /api/internal/v1/memberships/lookup` does not return the demo org for a user without a row;
- MCP `list_projects` includes the demo projects last;
- `DELETE …/projects/{p}` by an implicit viewer is `403`;
- `GET /api/v1/orgs/{demo}` answers `demo: true` (also for a real member); other orgs `demo: false`;
- an implicit viewer's calls to the member routes (leave, change role) create and delete no `org_members` row (they are refused as for a viewer).

**Done when** `just check-rust` passes and `cargo test -p cetra-server --test api_examples` passes.

---

## DO3: limits for implicit visitors (server)

**Edit** `auth/limits.rs`, `http/access.rs`, `routes/sql.rs`, `serve.rs` and `test_support/mod.rs` (the `Limits::new` calls).

**Steps**
1. `limits.rs`: move the in-flight map of `McpLimits` into `pub(crate) struct InFlight<K>` (`admit(&self, key) -> Result<InFlightGuard<K>, RetryAfter>` with a per-key maximum, guard decrements on drop and removes zero entries). `McpLimits` uses it with `MCP_IN_FLIGHT`; its behaviour and tests stay the same. This generic removes the duplication the demo limits would add (AGENTS.md).
2. `pub(crate) struct DemoLimits { rate: KeyedLimiter<Uuid>, users: InFlight<Uuid> /* max user_in_flight */, org: InFlight<()> /* max org_in_flight */ }`, rate `Bucket { capacity: requests_per_minute, per: 60 s }`. `admit(user, now) -> Result<DemoGuard, RetryAfter>`: `org.admit(())?` first, then `users.admit(user)?`, then `rate.check(&user, now)?`; each in-flight refusal is `RetryAfter(1 s)`; a later refusal drops the earlier guards, and an in-flight refusal spends no rate token (as `McpLimits::admit`). `DemoGuard` holds the two guards. `Limits::new` takes `&DemoConfig`; `Limits` gains `pub(crate) demo: DemoLimits`; `prune` prunes its rate limiter. `McpLimits::in_flight(token)` (a `#[cfg(test)]` helper used by MCP tests) stays.
3. `access.rs` `enforce`: after `prepare` succeeds, read `request.extensions().get::<DemoVisitor>().copied()`; when present call `state.limits.demo.admit(visitor.0, now)` (the `now` the MCP route uses). On `Err(retry)` answer `ApiError::RateLimited { retry_after_s: retry.0.as_secs().max(1) }.into_response()` (its `IntoResponse` sets `Retry-After` and `RateLimit-*`, as `mcp/mod.rs` does) and `record_access(&mut response, "denied-other")`. On `Ok`, keep `let _guard = …;` alive across `next.run(request).await` (`enforce` is an async fn, so this compiles). A streaming answer (SSE) releases its slot when the handler returns, so open streams are not counted; that is intended (the streams have their own caps).
4. `routes/sql.rs`: `run_sql` takes `Option<Extension<DemoVisitor>>` and passes `Option<&DemoConfig>` (`Some(&state.config.demo)` for a visitor) to `run_json`, which lowers `checked.limit` to `sql_max_rows` and `checked.timeout` to `sql_timeout` when they are larger. Exports are refused to viewers before that, so `start_export` is unchanged. The field rules and their 400s are unchanged.
5. `POST /mcp` is not counted by the demo limits: it has no `{org}`, and it has its own per-token limits (D-H-10: 60 a minute, 4 in flight); MCP `query_sql` already caps rows at 1,000. Spec 08 § Demo org says so.

**Tests**
- `InFlight`: admit up to the max, refuse the next, a dropped guard frees a slot, the map is empty after all guards drop.
- `McpLimits` tests unchanged and passing.
- `DemoLimits`: org-wide cap across two users; per-user cap; rate refusal with the right `Retry-After`; a refused admission spends no rate token.
- access: an implicit visitor over the rate gets `429` with `Retry-After`; a real member of the demo org with the same request count is not limited; the slot is released after the response (a second request after the first finishes is admitted with `user_in_flight = 1`).
- sql: a visitor's `limit: 5000` returns at most `sql_max_rows` rows with `truncated: true` when more matched; a visitor's `timeout_ms: 60000` runs with `sql_timeout`: reuse the query of `a_query_past_its_timeout_is_422_query_timeout`, set `demo.sql_timeout = "1s"`, send `timeout_ms: 60000`, expect `422 query_timeout`; a member's request is unchanged.
- an implicit visitor's `POST /mcp` call does not take a demo org slot.

**Done when** `just check-rust` passes.

---

## DO4: the web (switcher, landing, /demo, banner)

**Edit** `web/src/routes/index.tsx`, new `web/src/routes/demo.tsx`, `web/src/components/org-switcher.tsx`, `web/src/routes/$org/route.tsx` (`OrgLayout`, where `UsageBanner` is rendered), a new `web/src/components/demo-banner.tsx`, tests (extend `web/src/components/org-switcher.test.tsx`, `web/src/routes/home.test.tsx`, `web/src/routes/$org/$project/layout.test.tsx`; new tests next to new files), the MSW handlers in `fixtures/api` only if a new example is needed.

**Steps**
1. `index.tsx`: `const org = me.orgs.find((o) => !o.demo) ?? me.orgs[0];`.
2. `demo.tsx` (`/demo`; not a public path, so the existing `requireSession` sends a signed-out visitor to `/login?redirect=/demo`): find the org with `demo === true` in `me.orgs`; load `projectListOptions(org.slug)` from `web/src/api/queries.ts` with `ensureInfiniteQueryData` and redirect to `pages[0].data[0]` (the server sorts projects by name, then id) as `/$org/$project/traces` (the project root's own target; a redirect to the layout route stayed on `/demo`); no project → `/$org`; no demo org → `/`.
3. `org-switcher.tsx`: render the demo org last, after a separator, with `<Badge variant="secondary">Demo</Badge>` (`web/src/components/ui/badge.tsx`).
4. `demo-banner.tsx`, rendered in `OrgLayout` next to `UsageBanner`: in an org whose membership has `demo: true` and `role: "viewer"`, a thin bar: "You're exploring the cetra demo, read-only." and a link "Send your own traces": to `/` when the user has another org; otherwise to `org_create_url` from `GET /api/auth/config` when set, else `/setup/workspace` when `org_create_allowed`, else no link. Nothing for other orgs or for real members.

**Tests**
- index: with orgs `[demo(first by name), own]` it opens `own`; with only the demo it opens the demo.
- `/demo`: opens the demo org's first project; no demo org → `/`; signed out → `/login?redirect=/demo` (the existing auth gate).
- switcher: the demo org is last and badged.
- banner: shown for a demo viewer, hidden for an `admin` of the demo org and in a normal org; the link target for a user with another org, and for a demo-only user with and without `org_create_url`.

**Done when** `just check-web` passes.

---

## DO5: the generator, ingest (tools/demo)

**Edit** new crate `tools/demo/` (`Cargo.toml` modelled on `tools/capture/Cargo.toml`: `publish = false`, `[lints] workspace = true`; `src/main.rs`, `src/*.rs`), root `Cargo.toml` `members` (add `"tools/demo"`), `justfile` (a `demo` recipe running it). Use only workspace deps: axum (features `http1`, `json`, `tokio`), reqwest (`rustls-tls`, `json`), clap (`derive`, `env`), rand 0.10, serde, serde_json, tokio, thiserror, hex, chrono, tower. rand 0.10 renamed APIs from 0.8/0.9: read its docs; seeded tests use `StdRng` with `SeedableRng`, and if `StdRng` is not available, stop and ask.

**Steps**
1. Binary `cetra-demo`, `publish = false`, `[lints] workspace = true`, errors with `thiserror` (it is not `cetra-cli`, so no `anyhow`). Config by flags with env fallbacks (`clap` `env`): `--url` (`CETRA_DEMO_URL`, the cell's base URL), `--key` (`CETRA_DEMO_KEY`, an `sk` key; never logged), `--fixtures` (default `fixtures/otlp`), `--rate` (traces per minute at the daily peak, default 60), `--backfill-days` (default 30), `--seed` (optional, for tests).
2. Load every `input.json` under `--fixtures` (skip groups `edge` and any case whose `case.json` `origin` is not `captured`; if that leaves fewer than 10 templates, stop and ask). A template is one fixture's `resourceSpans`.
3. To emit a trace from a template: a new random trace id for every trace id of the request (keep the mapping so parent links stay valid), new span ids likewise; shift every `startTimeUnixNano`/`endTimeUnixNano` so the root starts at the target time, scaling all durations by a factor in 0.5–2.0 (one factor per trace); multiply token counts by 0.7–1.3 (see below); set `user.id` (from a pool of 200 fake ids `user-0001`…) and `session.id` (a session groups 1–8 consecutive traces of one user) as attributes on **every span** (spec 02: span-level context fields, the SDK sets them on every span), and `deployment.environment` (`production` 90%, `staging` 10%) and `service.version` (a release that changes every 7 days) as **resource** attributes; mark 3% of traces as errors (root span `status.code = 2` with a fixed message). Token counts: one factor per trace (so `cache_read ≤ input` holds), applied to the `intValue` of the token keys spec 02 reads (`gen_ai.usage.*`, `llm.token_count.*`, `ai.usage.*`); in OTLP JSON `intValue` is a string.
4. Daily curve: the rate follows a smooth day/night curve (peak at 14:00 UTC, trough at 25% of the peak at 03:00 UTC), with ±20% noise.
5. Backfill: when the project has no trace in the last `--backfill-days` (ask `GET /api/v1/projects/current/traces?limit=1&from=<now − backfill-days, RFC 3339>&to=<now>` with the key, which needs the `read` scope), emit that many days at the curve's rate, oldest first, in batches of at most 500 traces per request; then the live loop emits at the current rate.
6. `POST {url}/v1/traces` with `Content-Type: application/json`, `Authorization: Bearer <key>`; on 429/503 wait the `Retry-After` (or 5 s) and retry; on 402 (units cap) log it and wait 10 minutes. Log counts only.

**Tests** (unit, with `--seed`): ids are all new and links stay consistent; times land at the target and durations scale; token counts scale and stay integers; the curve's peak and trough; a batch never exceeds 500 traces; a wiremock-free HTTP test with a local `axum` server (as `tools/capture` tests do) receiving one batch with the bearer header and JSON content type, and the 429 retry.

**Done when** `just check-rust` passes and `just demo -- --help` prints the flags.

---

## DO6: the judge endpoint, image and runbook (tools/demo)

**Edit** `tools/demo/src/*`, `tools/demo/Dockerfile`, `tools/demo/README.md`, `justfile` (`demo-image`).

**Steps**
1. `--judge-addr` (`CETRA_DEMO_JUDGE_ADDR`, unset = off) and `--judge-key` (`CETRA_DEMO_JUDGE_KEY`, required when the address is set): an `axum` server with `POST /v1/chat/completions` that requires `Authorization: Bearer <judge-key>` (else 401), reads `response_format.json_schema.schema` and answers a Chat Completions body whose message content is a JSON object matching that schema: `explanation` (one of 20 canned sentences) when present, `value` a number in [0, 1] drawn from a beta-like spread centred near 0.8 or a boolean (80% `true`), or `label` the first enum value 70% of the time and the others otherwise. `usage` with plausible token counts. A request without that schema is `400`. Never log the request body.
2. `just demo-image [tag]` builds `cetra-demo` for `x86_64-unknown-linux-musl` on the host (as `tools/packaging/dist.sh` does), stages it with `fixtures/otlp` in `target/demo-image/`, and builds `tools/demo/Dockerfile`, which only copies them onto the distroless static nonroot base pinned by the same digest as `deploy/docker/Dockerfile` (`USER 65532:65532`). No Rust builder image: the repo pins none.
3. `README.md`, the runbook of spec 12 § Demo org: create the org through the control plane with a team account; ask the control plane operator for its override (no data-access window, no units cap, an ingest rate above the generator's); set `demo.org`; in the org create the project `demo`, an `sk` key (`ingest, read`), an LLM connection (OpenAI-compatible, base URL `https://<judge-host>/v1`, the judge key, `structured_output = native`); the evaluators (two built-in judge templates and one `check`), two online rules (sampling 20%), score definitions, one alert rule, and two dashboards (names and settings listed in the README); the compose service and the reverse-proxy route for the judge host. Use team email addresses only.

**Tests**: the judge answers each schema kind (numeric, boolean, categorical, with and without `explanation`), asserted against the requested schema's `type`, `required` and `enum` (`cetra-eval`'s parser is `pub(crate)`; do not depend on it); 401 without the key; 400 without a schema.

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
