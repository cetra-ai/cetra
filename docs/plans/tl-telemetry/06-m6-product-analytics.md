# TL M6 — Product analytics

> Superseded in part (user, 2026-11-28): the heartbeat's feature-usage counts described below were removed; see D-TL-14.

**Summary.** This milestone adds the product-analytics half of track TL (D-TL-14, D-TL-15): one typed catalog of 40 product events (6 recorded by the web UI, 34 by API handlers and MCP tools), one `analytics::record` function that every source calls, a session-only `POST /api/v1/analytics/events` endpoint through which the UI sends its events (always `204`, at most 60 requests a minute per user, unknown names and properties dropped, the user taken from the session and the org and project from the route parameters when a PostHog sink needs them, `DNT`/`Sec-GPC` honoured for the PostHog sink), the `analytics` flag in `GET /api/auth/config` (true when either sink runs: the PostHog sink or the heartbeat's feature counters), a small web client that queues catalog events and posts them in batches (page views by TanStack route id, never the URL), the `[analytics]` config and the PostHog sink (batches of 100 or every 10 s, bounded queue, no email, name, org name, IP or geolocation), the per-event feature counters saved hourly to `instance_settings['telemetry.usage_counts']`, and the heartbeat's new `usage` field (one bucket per catalog event). Golden tests fix the catalog's names and property keys on both sides (`fixtures/analytics/catalog.json`), privacy tests plant emails, slugs and URLs in requests and check that the PostHog body never carries them, and the Telemetry docs page, the configuration reference, specs 04/09/11/12, the CHANGELOG and the requests to track Q describe it all.

The coordinator's answers to the planning questions are recorded in the design (`README.md` § Details settled while planning, bullet "Product analytics (M6)"); this plan follows them.

## Prerequisites

- Work in `/home/sebastien/travaux/llm-observer/.claude/worktrees/cross-track-review-6a41ad`, branch `claude/telemetry-9e2a24`.
- **M1 and M4 are committed on this branch** (`01-m1-self-monitoring-logs.md`, `04-m4-heartbeat.md`). M6 builds on these names, checked against the code at planning time; each phase that uses one checks it again, and **if one is missing or differs, stops and reports**:
  - `crate::observability::switchboard::{Decision, Switch, OffReason, for_config}`; `Decision { heartbeat: Switch }` derives `Copy` (M5 adds `crash_reports`); `Switch::{On, Off(OffReason)}`.
  - `crate::observability::heartbeat`: `pub(crate) struct Heartbeat` (15 fields, `features` last, line ≈63), `collect` (its literal at ≈206), `client` (with the `#[cfg(test)] no_proxy()` line, ≈255), `Sender::tick` (≈342: `let payload = match collect(…) { Ok(payload) => payload, … };`, then `self.send(&payload)`, then the `put_setting(conn, TELEMETRY_LAST_HEARTBEAT_AT, …)` write and `tracing::debug!("telemetry: heartbeat sent")`), `start` (≈408); in its tests: `const KEYS: [&str; 15]`, `fn golden() -> Heartbeat`, `the_payload_matches_the_golden_json` (≈554), `the_key_set_is_fixed` (≈590), `a_fresh_instance_collects_zeros` (≈602), `the_payload_is_small` (≈962), `tick_sends_and_records_the_time`, `tick_waits_when_sent_recently`, `tick_does_not_record_a_failure`, the test `Receiver` and `sender_for`.
  - `crate::meta::instance_settings::{get_setting, put_setting, TELEMETRY_INSTANCE_ID, TELEMETRY_LAST_HEARTBEAT_AT}`.
  - `crate::config::{TelemetryConfig (≈412), load_from (≈1013), secret_key (≈1151), validate (≈1179), validate_telemetry (≈1795), is_secret_path (≈2238)}`; `ServerConfig.telemetry` (≈885); the test `the_configuration_reference_matches_the_loader` (≈4447); the `telemetry: TelemetryConfig { endpoint: "http://127.0.0.1:9".to_owned(), ..TelemetryConfig::default() }` blocks in the three `ServerConfig { … }` literals.
  - In `serve::bind` (≈622): `let decision = crate::observability::switchboard::for_config(&state.config);` then `if let Some(task) = crate::observability::heartbeat::start(&state, decision).await {` / `tasks.push(task);` / `}`.
- **Order with M2:** the coordinator runs M2's TL2.9 (query spans in `routes/sql.rs` and `mcp/mod.rs`, `02-m2-spans.md`) **before** TL6.10 and TL6.11. Those two phases quote `run_sql` and `McpServer` as TL2.9 leaves them, and their edits there are pure insertions that never replace or move a line of M2's.
  - `website/src/content/docs/docs/self-hosting/telemetry.md` with `## Turn it off`, `## The startup line`, `## When it is sent`, `## What it sends`, `## The instance id`.
  - The Q notes file `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/tl-requests-for-q.md` with a `## TL-Q4 — PostHog (for M6)` section.
- **M2 (spans), M3 (debug bundle) and M5 (crash reports) are implemented in parallel in the same checkout and may land first in shared files.** Every M6 edit to a shared file is additive and local: add the new item next to the anchor named, and **keep every line another milestone added** (an extra field in `Decision`, `Features`, `AppInner` or `ServerConfig`, a key in the heartbeat's golden JSON, a layer in `finish`, a span or field in a route handler, a CHANGELOG bullet, a docs section). When an anchor moved because another milestone inserted lines, use the anchor's text, not its line number. Shared files, and the M6 phase that touches each (the coordinator serializes a phase with any other milestone's phase on the same file):

  | Shared file | M6 phase | Also edited by |
  |---|---|---|
  | `crates/server/src/config.rs` | TL6.2 | M2 (traces keys), M5 (`crash_reports`) |
  | `crates/server/src/test_support/mod.rs` | TL6.2 (literals), TL6.6 (`AppInner` literal) | M2, M5 possibly |
  | `crates/server/src/ingest/fixtures_tests.rs` | TL6.2 | M2, M5 config literals |
  | `website/…/self-hosting/configuration-reference.md` | TL6.2 | M2, M5 |
  | `crates/server/src/observability/heartbeat.rs` | TL6.7 | M5 (`features.crash_reports_on`), M3 (reads `collect`) |
  | `crates/server/src/serve.rs` | TL6.6 | M5 (spool sender), M2 |
  | `crates/server/src/http/mod.rs` | TL6.6 | M2 (`http.request` span layer) |
  | `crates/server/src/meta/instance_settings.rs` | TL6.4 | none planned |
  | `crates/server/src/lib.rs` | TL6.1 | none planned |
  | `crates/server/src/routes/sql.rs`, `mcp/mod.rs`, other route files | TL6.10–TL6.12 | M2 (`query` spans on the SQL console and MCP tool calls) |
  | `CHANGELOG.md`, `docs/spec/12-operations.md`, `website/…/self-hosting/telemetry.md` | TL6.15 | M2, M3, M5 docs phases |

- **Generated files.** Among M2, M3, M5 and M6 only M6 changes the OpenAPI document. No phase runs `just openapi`; the coordinator runs it once after TL6.8 and TL6.9 are committed and commits `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (§ After the last phase).
- **M3's config classification.** M3's debug bundle classifies every key of the configuration reference. Whichever of TL6.2 and M3's classification phase lands second adds `analytics.posthog_host` (allow-listed) and `analytics.posthog_key` (secret) to M3's list; TL6.2 never edits M3's files (coordinator).
- **A test that is red until a later phase.** TL6.1's `the_telemetry_page_lists_every_event` fails until TL6.15 writes the event list; the checks run once at the end of the milestone, so this is expected. The coordinator does not run it before TL6.15 is in.

## Read first

- `AGENTS.md`: § Rules (no compatibility layers, no placeholders, errors, safety, privacy), § Tests (fixtures/api, MSW and `server.use`, configuration reference test, docs checks), § Commits and reports.
- `docs/plans/tl-telemetry/README.md`: § Configuration (the `[analytics]` table), § 5 Heartbeat, § 6 Product analytics, § Details settled while planning (bullet "Product analytics (M6)"), § The switchboard, § Licence separation, § Decisions D-TL-14 and D-TL-15, § Requests to track Q (TL-Q4), § Milestones (M6), § Answers (6: no account opt-out; `DNT`/`Sec-GPC` honoured).
- `docs/plans/tl-telemetry/04-m4-heartbeat.md`: § Conventions for every phase, TL4.2, TL4.3 steps 1–7, TL4.4, TL4.5, TL4.6, TL4.9, TL4.10 step 5. The committed code is the reference where it differs.
- `crates/server/src/config.rs`: `Secret` (≈72–130), `TelemetryConfig` (≈412), `ServerConfig` (≈792–890), `load_from` (≈1013), `secret_key` (≈1151), `validate` (≈1179), `validate_telemetry` (≈1795), `is_secret_path` (≈2238), `the_configuration_reference_matches_the_loader` (≈4447).
- `crates/server/src/observability/heartbeat.rs` (whole file), `crates/server/src/observability/switchboard.rs` (`Decision`, `for_config`).
- `crates/server/src/http/mod.rs` (`AppInner`, `finish`), `crates/server/src/http/access.rs` (`Access`, `RouteDef`, `routes`, `OrgAccess`, `ProjectAccess`, `user_project`), `crates/server/src/auth/principal.rs` (`Principal`, `AuthUser`), `crates/server/src/auth/limits.rs` (`KeyedLimiter`, `Bucket`), `crates/server/src/auth/session.rs` (`SessionOrigin`, `start_session`), `crates/server/src/demo.rs` (`org_role_for`, `OrgRoleOf`), `crates/server/src/meta/orgs.rs` (`org_by_id`, `org_by_slug`), `crates/server/src/entitlements/mod.rs` (`EntitlementsState::plan_at`, `PlanView::name`).
- `crates/server/src/routes/auth.rs` (`routes`, `config`), `crates/server/src/api/account.rs` (`AuthConfig`, `account_types_snapshot`), `crates/server/tests/snapshots/auth_config.snap`.
- `crates/server/src/api/openapi.rs`: `collect_schemas`, `openapi` tags, `paths`, `admin_paths`'s `session` security, `operation`, `json_body`, `responses`, `ResponseBuilder` uses (e.g. `post_agent_context`).
- `crates/server/tests/api_examples.rs`: `roundtrip`, `request_examples_match_their_types_and_the_openapi_document`. `fixtures/api/README.md`, `fixtures/api/static/auth-config.json`.
- `crates/server/src/serve.rs` `bind` (the `AppInner` literal, M4's heartbeat block), `crates/server/src/test_support/mod.rs` (`TestApp`, the `AppInner` literal, `seed`, `request`, `TestRequest::header`, `capture_logs`, `second_org`), `crates/server/src/mcp/mod.rs` (`mcp`, `McpServer`, `call_tool`, `run_tool`).
- `web/src/router.tsx`, `web/src/main.tsx`, `web/src/api/auth.ts` (`authConfigQuery`), `web/src/api/session.ts` (`isSessionApiPath`: why the analytics client does not use `api`), `web/src/lib/redirect.ts` (`isPublicPath`), `web/src/test/setup.ts` (unhandled requests fail a test), `web/src/test/render.tsx` (`renderApp`), `web/src/mocks/handlers.ts`, `web/src/components/command/palette.tsx`, `web/src/components/live-toggle.tsx`, `web/src/components/theme-toggle.tsx`, `web/src/lib/list-layout.ts`, `web/src/lib/assistant/panel.ts`, and the tests `palette.test.tsx`, `theme-toggle.test.tsx`, `list-layout.test.ts`, `assistant/panel.test.ts`.
- PostHog's capture API (`https://posthog.com/docs/api/capture`, read 2026-11-23): `POST <host>/batch/` with `{"api_key", "batch": [{"event", "distinct_id", "timestamp", "properties"}]}`; the docs say to put `distinct_id` at the event's top level, while their batch example has it in `properties` (cetra sends both, same value); `$groups` in `properties` maps a group type to a key; `$groupidentify` with `$group_type`, `$group_key`, `$group_set`; `$process_person_profile`; `$set` sets person properties; bodies under 20 MB; invalid events still answer 200. `$ip` is not documented there; cetra sends `"$ip": null` and `"$geoip_disable": true` (the property PostHog's server SDKs use to turn GeoIP off), and TL-Q4 asks Q to verify the payload with one real batch and to turn on "Discard client IP data".

## What it is, and what it is not

**What works when it's done:**
- `crate::analytics::catalog::Event`: 40 variants, each with a dotted append-only name, a source (`web` or `server`) and typed properties; `CATALOG` lists them sorted by name; `PAGE_ROUTES` lists the 65 page route templates of the signed-in UI. `fixtures/analytics/catalog.json` is the golden file both the Rust and the web tests compare with.
- `crate::analytics::record(&state, &event, &actor)`: every catalog event, from the UI, the CLI, an SDK, MCP or the API, goes through it once; it increments the event's counter and, when the PostHog sink runs, the actor has a user and did not opt out, queues one PostHog event (and a `$groupidentify` when due).
- 34 handlers record their server event after success (accounts, keys, evaluation, datasets, prompts, playground, queries, dashboards, annotation queues, scores, alerts, issues, deletions, the assistant, MCP tool calls, sign-in); the MCP tools `query_sql` and `get_trace` also record `sql_console.run` and `trace.viewed`, so those two count whichever client ran them.
- `POST /api/v1/analytics/events` (session cookie only): at most 50 events, body at most 64 KiB, at most 60 requests a minute per user (more are dropped), always `204` once past authentication; only the 6 web events are accepted; unknown names, unknown properties, wrong types and URLs instead of route templates are dropped; when the PostHog sink runs, the event's `org` and `project` are resolved (at most 5 distinct pairs per request, each once) and an event the user cannot read is dropped; without the sink, events only feed the counters and nothing is looked up. In the OpenAPI document, with a request example in `fixtures/api/requests/`.
- `GET /api/auth/config` has `analytics: bool`: true when the PostHog sink runs or the heartbeat's feature counters run (the switchboard's heartbeat decision). So a self-hosted release build with the heartbeat on counts page views and the UI-only events too; nothing about them leaves the server except the buckets.
- The web client (`web/src/lib/analytics.ts`, `analytics-install.ts`): sends nothing unless `analytics` is true; then a page view per path change by route id, plus 5 UI-only events, in batches of at most 50 every 5 s and when the page is hidden.
- `[analytics]` with `posthog_host` and `posthog_key` (secret), both or neither, validated without quoting values; in the configuration reference.
- The PostHog sink: batches of 100 or every 10 s, queue of 10 000 (new events dropped when full), no retry, failures at `debug` at most once a minute, `distinct_id` = user UUID, `$groups.org` = org id, `$ip` null, `$geoip_disable` true, `$process_person_profile` true, `$set.org_role`, group property `plan` sent with `distinct_id` = `org_<org id>`. No email, name, org name, slug, URL or free text can reach it: the `Actor` type carries none.
- Feature counters per event name; saved hourly to `instance_settings['telemetry.usage_counts']` while the heartbeat runs; the heartbeat's `usage` field buckets them (`0`, `1-9`, `10-99`, `100-999`, `1000+`), and an accepted heartbeat subtracts what it sent.
- Docs: Telemetry page sections "Feature usage" and "Product analytics", the configuration reference rows, CHANGELOG, specs 04, 09, 11, 12, TL-Q4 in the Q notes.

**Out of scope:**
- PostHog dashboards, the PostHog project and the cloud privacy policy (track Q, TL-Q4).
- An account switch for analytics (design § Answers 6: none for now).
- A UI for analytics settings; session replay, autocapture, any third-party script (non-goals).
- Events of the control plane's own pages (Q may reuse the catalog names).
- The audit log catalog (D-EA-4, track EA): a separate catalog with the same idea; nothing is shared.
- Theme commands of the command palette (only the theme menu records `theme.changed`; the Telemetry page says "in the theme menu").
- Checking that a `page.viewed` route matches the event's `org`/`project`: they are independent values (the route is a template, the params resolve the group).
- Showing `usage` in M3's debug-bundle preview: M3 calls `collect`, whose `usage` is all zeros; calling `Heartbeat::with_usage` there is M3's choice (hand-off note).
- The roadmap row (coordinator).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-TL-14 | One catalog, two sinks: PostHog per-user events when `[analytics]` is set; bucketed counts in the heartbeat; web events go through the server; no posthog-js |
| D-TL-15 | Typed catalog, dotted append-only names, code-defined properties (enums, booleans, counts, durations, entity ids); page views by route template; user and org from the session; `DNT`/`Sec-GPC` skip the PostHog sink |
| Design § Details settled, "Product analytics (M6)" | The flag is true when either sink runs; web events carry the leaf route's org and project slugs, resolved with the normal access checks and never forwarded; the sink depends on `[analytics]` and restore mode only; counters count in memory always, saved and sent only while the heartbeat runs; no-user events are counted only; outside a request the signal is "opted out"; `org_role` only when known without a query; group properties at most hourly per org; counters saved hourly and after an accepted heartbeat; `trace.viewed` includes router preloads |
| D-TL-8 | Heartbeat: buckets, never exact counts; M6 adds `usage` |
| D-TL-11 | The switchboard; the counters and the `analytics` flag follow its heartbeat decision |
| D-TL-1 | `[analytics]` is named in the design's config block |
| D172 | Licence separation: the counters and the catalog read nothing of licences (test in TL6.4) |
| D-N-25 | Restore mode: no outbound work, so no PostHog sink |
| D166 | `posthog_host` is operator config: no SSRF guard (like `telemetry.endpoint`) |
| D138 | The access table: the endpoint is `Access::User { session_only: true }` |
| D141 | CSRF: the UI's same-origin `fetch` sends `Origin` |
| D-N-5 | CHANGELOG `[Unreleased]` |

User answers used: design § Answers 1 (`[analytics]` name), 5 (parallel milestones), 6 (no account opt-out; `DNT`/`Sec-GPC`).

Implementation details within those decisions:
- **Module.** `crates/server/src/analytics/` (new top-level module), not `observability/analytics/`: analytics is product behaviour called by ~35 handlers, owns an HTTP route and API types, and keeping it beside `observability/` keeps `observability/mod.rs` (edited by M2, M3 and M5 in parallel) out of M6's edits. Only `heartbeat.rs` is shared, for the `usage` field.
- **Counting** = `switchboard::for_config(config).heartbeat == Switch::On`, computed once in `Analytics::new` (the same pure function `serve::bind` calls). A debug build always has the heartbeat off, so tests turn counting on with the test-only `Analytics::set_counting`.
- **Route template** = the TanStack Router route id of the leaf match (`/$org/$project/traces/` for `src/routes/$org/$project/traces/index.tsx`). The server accepts only the ids in `PAGE_ROUTES` and stores the constant from that list, never the request's string. The public pre-sign-in pages (`/login`, `/signup`, `/reset`, `/verify`, `/setup/`, `/invite/$token`) are not in it: the client never sends them.
- **`via`** (`session`, `pat`, `api_key`, `control_plane`) is a context property of server events; `project_id` is a context property of any event with a project. Both are listed in the golden file's `context`. Web events are never `via`.
- **Per-user limit** on the events endpoint: `KeyedLimiter<Uuid>` from `auth::limits`, 60 requests a minute per user, held in `Analytics` (no edit to `auth::limits`); a refused request is dropped with `204`.
- **Timestamps** sent to PostHog: RFC 3339 with milliseconds, UTC, from the server clock.
- **Lints.** `record` takes `&Event`; items read only by tests (`EventSpec::source`, `EventSpec::properties`, `CONTEXT_PROPERTIES`, `Counters::get`) carry `#[cfg_attr(not(test), allow(dead_code))]` with a comment saying the golden test or the route tests read them.
- **No new decision IDs**: everything above is a detail of D-TL-14/15 recorded in the design.

## Versions

No new Rust crate, no new feature, no new JS package. Used, already present: `reqwest` 0.12.28 (`json`, `rustls-tls`), `tokio` (`task_local!`, `mpsc`, `time`), `chrono` (as in M4's heartbeat), `serde_json` with `preserve_order` (property maps keep insertion order), `axum` 0.8 (`body::to_bytes`, `middleware::from_fn`), `utoipa` 6.0.0; web: `@tanstack/react-router` 1.170.46, `msw`, `vitest`. Implementers report "no dependency change".

## Phase graph

```
TL6.1 catalog ─► TL6.3 actor ─► TL6.4 counters ─► TL6.5 posthog ─► TL6.6 state+record ─┬► TL6.7 heartbeat usage
TL6.2 config ──────────────────────────────────────────────────────┘                 ├► TL6.8 auth config flag
                                                                                       ├► TL6.9 events endpoint + OpenAPI
                                                                                       ├► TL6.10 server events A (+ MCP)
                                                                                       ├► TL6.11 server events B
                                                                                       └► TL6.12 server events C
TL6.1 ─► TL6.13 web client ─► TL6.14 web UI events
TL6.1 ─► TL6.15 docs, specs, CHANGELOG, Q notes
```

- TL6.1, TL6.3, TL6.4, TL6.5, TL6.6 run in order (each adds a `mod` line to `analytics/mod.rs` and uses the previous one). TL6.2 runs in parallel with TL6.1–TL6.5 and must be in before TL6.6.
- TL6.7–TL6.12 run in parallel after TL6.6: their files are disjoint.
- TL6.13 needs only TL6.1's golden file; TL6.14 follows TL6.13. TL6.15 needs TL6.1's names.

| Phase | Files |
|---|---|
| TL6.1 | `crates/server/src/lib.rs`, `crates/server/src/analytics/mod.rs` (new), `crates/server/src/analytics/catalog.rs` (new), `fixtures/analytics/catalog.json` (new) |
| TL6.2 | `crates/server/src/config.rs`, `crates/server/src/test_support/mod.rs`, `crates/server/src/ingest/fixtures_tests.rs`, `website/src/content/docs/docs/self-hosting/configuration-reference.md` |
| TL6.3 | `crates/server/src/analytics/actor.rs` (new), `crates/server/src/analytics/mod.rs` |
| TL6.4 | `crates/server/src/analytics/counters.rs` (new), `crates/server/src/analytics/mod.rs`, `crates/server/src/meta/instance_settings.rs` |
| TL6.5 | `crates/server/src/analytics/posthog.rs` (new), `crates/server/src/analytics/test_receiver.rs` (new), `crates/server/src/analytics/mod.rs` |
| TL6.6 | `crates/server/src/analytics/mod.rs`, `crates/server/src/http/mod.rs`, `crates/server/src/test_support/mod.rs`, `crates/server/src/serve.rs` |
| TL6.7 | `crates/server/src/observability/heartbeat.rs` |
| TL6.8 | `crates/server/src/api/account.rs`, `crates/server/src/routes/auth.rs`, `fixtures/api/static/auth-config.json`, `crates/server/tests/snapshots/auth_config.snap` |
| TL6.9 | `crates/server/src/api/analytics.rs` (new), `crates/server/src/api/mod.rs`, `crates/server/src/routes/analytics.rs` (new), `crates/server/src/routes/mod.rs`, `crates/server/src/http/access.rs`, `crates/server/src/api/openapi.rs`, `fixtures/api/requests/analytics-events.json` (new), `crates/server/tests/api_examples.rs` |
| TL6.10 | `crates/server/src/auth/session.rs`, `crates/server/src/routes/{orgs,projects,invites,api_keys,tokens,device,connections,evaluators,eval_jobs,online_rules,backfills}.rs`, `crates/server/src/mcp/mod.rs` |
| TL6.11 | `crates/server/src/routes/{datasets,issue_dataset_items,experiments,prompts,playground,traces,sql,saved_views}.rs` |
| TL6.12 | `crates/server/src/routes/{dashboards,annotation_queues,scores,alert_rules,channels,issues,deletions,assistant}.rs` |
| TL6.13 | `web/src/lib/analytics.ts` (new), `web/src/lib/analytics.test.ts` (new), `web/src/lib/analytics-install.ts` (new), `web/src/lib/analytics-install.test.tsx` (new), `web/src/lib/analytics-catalog.test.ts` (new), `web/src/main.tsx`, `web/src/mocks/handlers.ts` |
| TL6.14 | `web/src/components/command/palette.tsx`, `web/src/components/command/palette.test.tsx`, `web/src/components/live-toggle.tsx`, `web/src/components/live-toggle.test.tsx` (new), `web/src/components/theme-toggle.tsx`, `web/src/components/theme-toggle.test.tsx`, `web/src/lib/list-layout.ts`, `web/src/lib/list-layout.test.ts`, `web/src/lib/assistant/panel.ts`, `web/src/lib/assistant/panel.test.ts` |
| TL6.15 | `website/src/content/docs/docs/self-hosting/telemetry.md`, `CHANGELOG.md`, `docs/spec/04-metadata-db.md`, `docs/spec/09-api.md`, `docs/spec/11-ui.md`, `docs/spec/12-operations.md`, the Q notes file (outside the repo, not committed) |

## Conventions for every phase

- The implementer is Haiku. Follow the plan exactly. If a step is unclear, contradicts the code, or cannot be done as written (a name differs, a function has another shape), **stop and report**; do not improvise.
- Edit only the files in the phase's **Edit** list. Report any change needed elsewhere.
- **Shared files:** add next to the named anchor and keep every line another milestone (M2, M3, M5) added there; never revert or reorder their lines.
- **Do not run** `cargo build`, `cargo test`, `cargo clippy`, `cargo nextest`, any `just` recipe, `pnpm`, `vitest`, `tsc` or a web build (the machine is loaded; the coordinator runs the checks once at the end). Allowed: `git diff`, `grep`, `cargo info`, `rustfmt --edition 2024 <your .rs files>`, and for web files `cd web && ../node_modules/.bin/biome format --write <your files>` (paths relative to `web/`).
- **Done when** for every phase: the listed files changed as specified, nothing else changed, one commit with the phase's subject exists.
- Commit only your files: `git add <files> && git commit -m "<subject>" -- <files>` (`git add` is needed for new files). Other agents commit in the same checkout: if `.git/index.lock` exists, wait a few seconds and retry. Never `git stash`, never `git add -A`.
- Rust rules (AGENTS.md): no `unwrap()`/`expect()` outside tests unless a comment states the invariant; no `unsafe`; never panic on input; `thiserror` errors; logs and errors never carry payload, emails, names, slugs, URLs, keys or property values (event names, ids and counts only).
- New Rust items are `pub(crate)` with a doc comment; module docs use "licence" (British spelling) as a noun, never the identifier spelling (TL6.4's test scans for it). In every new module, all non-test items come before one `#[cfg(test)] mod tests` at the end of the file (a single `#[cfg(test)]` method named in a step is the only exception). The workspace lint `unused_qualifications` is on: import a name once rather than writing its full path where an import exists.
- Tests compare `ConfigError` and `MetaError` values with `matches!` (no `PartialEq`).
- Web rules: imports through `@/…`; tests use MSW (`server.use`) for network, never module mocks; the test setup fails a test on any unhandled request.
- The milestone check, run by the coordinator: `just fmt`, `just openapi`, then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`.

---

### TL6.1 — The event catalog

**Goal.** The catalog as one Rust enum with names, sources, properties, the page route list, the web-event parser, and the golden file.

**Edit:**
- `crates/server/src/lib.rs` (one line)
- `crates/server/src/analytics/mod.rs` (new)
- `crates/server/src/analytics/catalog.rs` (new)
- `fixtures/analytics/catalog.json` (new)

**Steps.**
1. `lib.rs`: add `pub(crate) mod analytics;` in alphabetical order (right after `pub(crate) mod alerts;`, before `pub mod api;`).
2. Create `analytics/mod.rs` with exactly:
   ```rust
   //! Product analytics (track TL M6, D-TL-14, D-TL-15): one typed catalog of product events,
   //! recorded once wherever an action happens (web UI, API, CLI, SDK, MCP), feeding two sinks:
   //! per-event feature counters for the heartbeat's `usage` buckets, and per-user events to the
   //! operator's PostHog when `[analytics]` is set.

   pub(crate) mod catalog;
   ```
3. Create `analytics/catalog.rs`. Module doc: `//! The event catalog (D-TL-15). Names are dotted and append-only: a name is never renamed or reused, so a dashboard keeps its meaning. Properties are code-defined values (enums, booleans, our entity ids): never names, free text, URLs, filter values, attribute keys or payload. `fixtures/analytics/catalog.json` is the golden file; any change shows in review.` Contents, in full:
   ```rust
   use serde::{Deserialize, Serialize};
   use serde_json::{Map, Value};

   use crate::api::account::OrgRole;
   use crate::api::alerts::ChannelKind;
   use crate::api::annotation_queues::QueueTarget;
   use crate::api::connections::LlmAdapter;
   use crate::api::datasets::{ExportFormat, ImportFormat};
   use crate::api::evaluators::EvaluatorKind;
   use crate::api::issues::IssueSourceKind;
   use crate::api::keys::ApiKeyKind;
   use crate::api::online_rules::RuleTarget;
   use crate::api::prompts::PromptKind;
   use crate::api::saved_views::SavedViewKind;

   /// Where an event is recorded.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   #[serde(rename_all = "snake_case")]
   pub(crate) enum Source {
       /// The web UI, through `POST /api/v1/analytics/events`.
       Web,
       /// An API handler, whatever client called it.
       Server,
   }

   /// One catalog entry: what the golden file fixes.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) struct EventSpec {
       /// The dotted name.
       pub(crate) name: &'static str,
       /// Where it is recorded. Read by the golden test.
       #[cfg_attr(not(test), allow(dead_code))]
       pub(crate) source: Source,
       /// Its property keys, sorted. Read by the golden test.
       #[cfg_attr(not(test), allow(dead_code))]
       pub(crate) properties: &'static [&'static str],
   }

   /// Context properties a sink may add to an event besides its own: `project_id` when the
   /// event has a project, `via` for server events. Read by the golden test.
   #[cfg_attr(not(test), allow(dead_code))]
   pub(crate) const CONTEXT_PROPERTIES: [&str; 2] = ["project_id", "via"];

   /// Every event, sorted by name. Append-only.
   pub(crate) const CATALOG: &[EventSpec] = &[
       spec("alert_rule.created", Source::Server, &[]),
       spec("annotation_queue.created", Source::Server, &["target"]),
       spec("annotation_queue.item_completed", Source::Server, &[]),
       spec("api_key.created", Source::Server, &["kind"]),
       spec("assistant.panel_opened", Source::Web, &[]),
       spec("assistant.turn_sent", Source::Server, &[]),
       spec("backfill.started", Source::Server, &[]),
       spec("channel.created", Source::Server, &["kind"]),
       spec("command_palette.opened", Source::Web, &[]),
       spec("dashboard.created", Source::Server, &[]),
       spec("dashboard.edited", Source::Server, &[]),
       spec("dataset.created", Source::Server, &[]),
       spec("dataset.exported", Source::Server, &["format"]),
       spec("dataset.imported", Source::Server, &["format"]),
       spec("dataset.items_added", Source::Server, &["source"]),
       spec("deletion.requested", Source::Server, &[]),
       spec("device.approved", Source::Server, &[]),
       spec("evaluator.created", Source::Server, &["kind"]),
       spec("evaluator.run", Source::Server, &[]),
       spec("experiment.created", Source::Server, &[]),
       spec("invite.created", Source::Server, &["role"]),
       spec("issue_source.created", Source::Server, &["kind"]),
       spec("list_layout.changed", Source::Web, &["layout"]),
       spec("live_tail.toggled", Source::Web, &["on"]),
       spec("llm_connection.created", Source::Server, &["adapter"]),
       spec("mcp.tool_called", Source::Server, &["ok", "tool"]),
       spec("online_rule.created", Source::Server, &["target"]),
       spec("org.created", Source::Server, &[]),
       spec("page.viewed", Source::Web, &["route"]),
       spec("personal_token.created", Source::Server, &["write"]),
       spec("playground.run", Source::Server, &[]),
       spec("project.created", Source::Server, &[]),
       spec("prompt.created", Source::Server, &["kind"]),
       spec("prompt.version_created", Source::Server, &[]),
       spec("saved_view.created", Source::Server, &["kind"]),
       spec("scores.created", Source::Server, &[]),
       spec("sql_console.run", Source::Server, &[]),
       spec("theme.changed", Source::Web, &["theme"]),
       spec("trace.viewed", Source::Server, &[]),
       spec("user.signed_in", Source::Server, &["method"]),
   ];

   const fn spec(
       name: &'static str,
       source: Source,
       properties: &'static [&'static str],
   ) -> EventSpec {
       EventSpec { name, source, properties }
   }

   /// The UI's page route templates: the TanStack Router ids of its leaf routes, sorted, except
   /// the public pre-sign-in pages (`/login`, `/signup`, `/reset`, `/verify`, `/setup/`,
   /// `/invite/$token`), which the client never reports. `page.viewed` accepts only these. A new
   /// signed-in page adds its id here and to the golden file.
   pub(crate) const PAGE_ROUTES: &[&str] = &[
       "/",
       "/$org/",
       "/$org/$project/",
       "/$org/$project/alerts/",
       "/$org/$project/alerts/$ruleId/",
       "/$org/$project/alerts/$ruleId/edit",
       "/$org/$project/alerts/new",
       "/$org/$project/annotation-queues/",
       "/$org/$project/annotation-queues/$queueId/",
       "/$org/$project/annotation-queues/$queueId/review/$itemId",
       "/$org/$project/dashboard/",
       "/$org/$project/dashboard/$dashboardId",
       "/$org/$project/datasets/",
       "/$org/$project/datasets/$datasetId",
       "/$org/$project/evaluators/",
       "/$org/$project/evaluators/$evaluatorId",
       "/$org/$project/evaluators/new",
       "/$org/$project/experiments/",
       "/$org/$project/experiments/$experimentId",
       "/$org/$project/experiments/compare",
       "/$org/$project/issue-sources/",
       "/$org/$project/issue-sources/$sourceId/",
       "/$org/$project/issue-sources/$sourceId/edit",
       "/$org/$project/issue-sources/new",
       "/$org/$project/issues/",
       "/$org/$project/issues/$issueId",
       "/$org/$project/online-rules/",
       "/$org/$project/online-rules/$ruleId/",
       "/$org/$project/online-rules/$ruleId/edit",
       "/$org/$project/online-rules/new",
       "/$org/$project/playground/",
       "/$org/$project/prompts/",
       "/$org/$project/prompts/$prompt",
       "/$org/$project/sessions/",
       "/$org/$project/sessions/$sessionId",
       "/$org/$project/settings/",
       "/$org/$project/settings/api-keys",
       "/$org/$project/settings/channels",
       "/$org/$project/settings/data",
       "/$org/$project/settings/general",
       "/$org/$project/settings/members",
       "/$org/$project/settings/models",
       "/$org/$project/settings/score-definitions",
       "/$org/$project/spans/",
       "/$org/$project/sql/",
       "/$org/$project/traces/",
       "/$org/$project/traces/$traceId",
       "/$org/$project/users/",
       "/$org/$project/users/$userId",
       "/$org/settings/",
       "/$org/settings/assistant",
       "/$org/settings/general",
       "/$org/settings/llm-connections",
       "/$org/settings/members",
       "/$org/settings/models",
       "/$org/settings/plan",
       "/$org/settings/sso",
       "/account",
       "/admin/",
       "/admin/settings",
       "/admin/users",
       "/demo",
       "/device",
       "/setup/workspace",
       "/sso/no-seat",
   ];

   /// The traces list layout (`web/src/lib/list-layout.ts`).
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "lowercase")]
   pub(crate) enum ListLayout {
       Drawer,
       Bottom,
       Page,
   }

   /// The UI theme (`web/src/components/theme-provider.tsx`).
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "lowercase")]
   pub(crate) enum Theme {
       Light,
       Dark,
       System,
   }

   /// How dataset items were added.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   #[serde(rename_all = "snake_case")]
   pub(crate) enum ItemSource {
       /// `POST …/items`.
       Manual,
       /// `POST …/items/from-trace`.
       Trace,
       /// `POST …/items/from-traces`.
       Traces,
       /// `POST …/issues/{id}/dataset-items`.
       Issue,
   }

   /// How a session started (`auth::session::SessionOrigin`).
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   #[serde(rename_all = "snake_case")]
   pub(crate) enum SignInMethod {
       /// A password: login, signup, setup, or the first session after a reset.
       Password,
       /// GitHub, Google or an `[[auth.oidc]]` provider.
       Provider,
       /// An org's own identity provider (D-P-59).
       OrgProvider,
   }

   /// One product event. Field types are the property types; `&'static str` values come from
   /// code-defined lists (`PAGE_ROUTES`, the MCP tool names).
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) enum Event {
       AlertRuleCreated,
       AnnotationQueueCreated { target: QueueTarget },
       AnnotationQueueItemCompleted,
       ApiKeyCreated { kind: ApiKeyKind },
       AssistantPanelOpened,
       AssistantTurnSent,
       BackfillStarted,
       ChannelCreated { kind: ChannelKind },
       CommandPaletteOpened,
       DashboardCreated,
       DashboardEdited,
       DatasetCreated,
       DatasetExported { format: ExportFormat },
       DatasetImported { format: ImportFormat },
       DatasetItemsAdded { source: ItemSource },
       DeletionRequested,
       DeviceApproved,
       EvaluatorCreated { kind: EvaluatorKind },
       EvaluatorRun,
       ExperimentCreated,
       InviteCreated { role: OrgRole },
       IssueSourceCreated { kind: IssueSourceKind },
       ListLayoutChanged { layout: ListLayout },
       LiveTailToggled { on: bool },
       LlmConnectionCreated { adapter: LlmAdapter },
       McpToolCalled { tool: &'static str, ok: bool },
       OnlineRuleCreated { target: RuleTarget },
       OrgCreated,
       PageViewed { route: &'static str },
       PersonalTokenCreated { write: bool },
       PlaygroundRun,
       ProjectCreated,
       PromptCreated { kind: PromptKind },
       PromptVersionCreated,
       SavedViewCreated { kind: SavedViewKind },
       ScoresCreated,
       SqlConsoleRun,
       ThemeChanged { theme: Theme },
       TraceViewed,
       UserSignedIn { method: SignInMethod },
   }
   ```
   Give every variant a one-line doc comment saying when it is recorded (copy the "Recorded" column of the table in step 4).
4. `impl Event` with three methods, written in full by the implementer from this table (no wildcard arm in any `match`: every variant is listed, so a new variant forces a decision):

   | Variant | `name()` | `source()` | `properties()` (key → value) | Recorded (doc comment) |
   |---|---|---|---|---|
   | `AlertRuleCreated` | `alert_rule.created` | Server | — | an alert rule is created |
   | `AnnotationQueueCreated { target }` | `annotation_queue.created` | Server | `target` → enum | an annotation queue is created |
   | `AnnotationQueueItemCompleted` | `annotation_queue.item_completed` | Server | — | a reviewer completes a queue item |
   | `ApiKeyCreated { kind }` | `api_key.created` | Server | `kind` → enum | a project API key is created |
   | `AssistantPanelOpened` | `assistant.panel_opened` | Web | — | the assistant panel opens |
   | `AssistantTurnSent` | `assistant.turn_sent` | Server | — | a message is sent to the assistant |
   | `BackfillStarted` | `backfill.started` | Server | — | an online-rule backfill starts |
   | `ChannelCreated { kind }` | `channel.created` | Server | `kind` → enum | a notification channel is created |
   | `CommandPaletteOpened` | `command_palette.opened` | Web | — | the command palette opens |
   | `DashboardCreated` | `dashboard.created` | Server | — | a dashboard is created |
   | `DashboardEdited` | `dashboard.edited` | Server | — | a dashboard or the overview is saved |
   | `DatasetCreated` | `dataset.created` | Server | — | a dataset is created |
   | `DatasetExported { format }` | `dataset.exported` | Server | `format` → enum | a dataset export is downloaded |
   | `DatasetImported { format }` | `dataset.imported` | Server | `format` → enum | a file is imported into a dataset |
   | `DatasetItemsAdded { source }` | `dataset.items_added` | Server | `source` → enum | items are added to a dataset |
   | `DeletionRequested` | `deletion.requested` | Server | — | a GDPR deletion is requested |
   | `DeviceApproved` | `device.approved` | Server | — | a device sign-in (`cetra login`, `cetra-cli init`) is approved |
   | `EvaluatorCreated { kind }` | `evaluator.created` | Server | `kind` → enum | an evaluator is created |
   | `EvaluatorRun` | `evaluator.run` | Server | — | an evaluator run (eval job) starts |
   | `ExperimentCreated` | `experiment.created` | Server | — | an experiment is created |
   | `InviteCreated { role }` | `invite.created` | Server | `role` → enum | an org invite is created |
   | `IssueSourceCreated { kind }` | `issue_source.created` | Server | `kind` → enum | an issue source is created |
   | `ListLayoutChanged { layout }` | `list_layout.changed` | Web | `layout` → enum | the list layout changes |
   | `LiveTailToggled { on }` | `live_tail.toggled` | Web | `on` → bool | the Live switch of a list is flipped |
   | `LlmConnectionCreated { adapter }` | `llm_connection.created` | Server | `adapter` → enum | an LLM connection is created |
   | `McpToolCalled { tool, ok }` | `mcp.tool_called` | Server | `ok` → bool, then `tool` → string | an MCP tool call ends (not when cancelled) |
   | `OnlineRuleCreated { target }` | `online_rule.created` | Server | `target` → enum | an online rule is created |
   | `OrgCreated` | `org.created` | Server | — | an org is created |
   | `PageViewed { route }` | `page.viewed` | Web | `route` → string | the UI shows a page (path change) |
   | `PersonalTokenCreated { write }` | `personal_token.created` | Server | `write` → bool | a personal access token is created |
   | `PlaygroundRun` | `playground.run` | Server | — | a playground run starts |
   | `ProjectCreated` | `project.created` | Server | — | a project is created |
   | `PromptCreated { kind }` | `prompt.created` | Server | `kind` → enum | a prompt is created |
   | `PromptVersionCreated` | `prompt.version_created` | Server | — | a prompt version is created |
   | `SavedViewCreated { kind }` | `saved_view.created` | Server | `kind` → enum | a saved view or SQL query is saved |
   | `ScoresCreated` | `scores.created` | Server | — | scores are written through a project route |
   | `SqlConsoleRun` | `sql_console.run` | Server | — | a SQL console query succeeds |
   | `ThemeChanged { theme }` | `theme.changed` | Web | `theme` → enum | the theme is picked in the menu |
   | `TraceViewed` | `trace.viewed` | Server | — | a trace's detail is read |
   | `UserSignedIn { method }` | `user.signed_in` | Server | `method` → enum | a web session starts |

   Signatures and helper, in full:
   ```rust
   impl Event {
       /// The dotted catalog name.
       pub(crate) fn name(&self) -> &'static str { /* match, from the table */ }

       /// Where the event is recorded.
       pub(crate) fn source(&self) -> Source { /* match: the 6 Web variants, the rest Server */ }

       /// The event's own properties, in the table's key order. Values are strings, booleans or
       /// numbers, never null.
       pub(crate) fn properties(&self) -> Map<String, Value> {
           let mut map = Map::new();
           match self {
               Self::PageViewed { route } => {
                   map.insert("route".to_owned(), Value::from(*route));
               }
               Self::LiveTailToggled { on } => {
                   map.insert("on".to_owned(), Value::Bool(*on));
               }
               Self::ListLayoutChanged { layout } => {
                   map.insert("layout".to_owned(), enum_value(layout));
               }
               // … one arm per variant with properties, as in the table …
               Self::McpToolCalled { tool, ok } => {
                   map.insert("ok".to_owned(), Value::Bool(*ok));
                   map.insert("tool".to_owned(), Value::from(*tool));
               }
               Self::AlertRuleCreated
               | Self::AnnotationQueueItemCompleted
               // … and the other unit variants (those without fields in step 3), listed …
               | Self::TraceViewed => {}
           }
           map
       }
   }

   /// A unit enum's serde string. Every property enum serializes to a string, so the
   /// `Null` fallback is never taken; `property_values_are_scalars` checks it.
   fn enum_value(value: impl Serialize) -> Value {
       serde_json::to_value(value).unwrap_or(Value::Null)
   }
   ```
   The last arm lists exactly the 20 variants that have no fields in the enum of step 3 (`AlertRuleCreated`, `AnnotationQueueItemCompleted`, `AssistantPanelOpened`, `AssistantTurnSent`, `BackfillStarted`, `CommandPaletteOpened`, `DashboardCreated`, `DashboardEdited`, `DatasetCreated`, `DeletionRequested`, `DeviceApproved`, `EvaluatorRun`, `ExperimentCreated`, `OrgCreated`, `PlaygroundRun`, `ProjectCreated`, `PromptVersionCreated`, `ScoresCreated`, `SqlConsoleRun`, `TraceViewed`, and no other); the 20 variants with fields each have their own arm.
5. Add, in full:
   ```rust
   /// The catalog's `&'static` spelling of `name`, if it is a catalog name.
   pub(crate) fn static_name(name: &str) -> Option<&'static str> {
       CATALOG.iter().find(|spec| spec.name == name).map(|spec| spec.name)
   }

   /// A web event from `POST /api/v1/analytics/events`: `None` for a server event's name, an
   /// unknown name, a missing or mistyped property, or a route that is not in [`PAGE_ROUTES`].
   /// Unknown properties are ignored. A stored route is the catalog's constant, never the input.
   /// The route is not checked against the event's `org`/`project`: they are independent values.
   pub(crate) fn parse_web(name: &str, properties: &Map<String, Value>) -> Option<Event> {
       match name {
           "page.viewed" => {
               let route = properties.get("route")?.as_str()?;
               let route = PAGE_ROUTES.iter().copied().find(|known| *known == route)?;
               Some(Event::PageViewed { route })
           }
           "command_palette.opened" => Some(Event::CommandPaletteOpened),
           "live_tail.toggled" => Some(Event::LiveTailToggled {
               on: properties.get("on")?.as_bool()?,
           }),
           "list_layout.changed" => Some(Event::ListLayoutChanged {
               layout: ListLayout::deserialize(properties.get("layout")?).ok()?,
           }),
           "theme.changed" => Some(Event::ThemeChanged {
               theme: Theme::deserialize(properties.get("theme")?).ok()?,
           }),
           "assistant.panel_opened" => Some(Event::AssistantPanelOpened),
           _ => None,
       }
   }
   ```
6. Create `fixtures/analytics/catalog.json` with exactly this content (two-space indentation, a final newline):
   ```json
   {
     "events": [
       {"name": "alert_rule.created", "source": "server", "properties": []},
       {"name": "annotation_queue.created", "source": "server", "properties": ["target"]},
       {"name": "annotation_queue.item_completed", "source": "server", "properties": []},
       {"name": "api_key.created", "source": "server", "properties": ["kind"]},
       {"name": "assistant.panel_opened", "source": "web", "properties": []},
       {"name": "assistant.turn_sent", "source": "server", "properties": []},
       {"name": "backfill.started", "source": "server", "properties": []},
       {"name": "channel.created", "source": "server", "properties": ["kind"]},
       {"name": "command_palette.opened", "source": "web", "properties": []},
       {"name": "dashboard.created", "source": "server", "properties": []},
       {"name": "dashboard.edited", "source": "server", "properties": []},
       {"name": "dataset.created", "source": "server", "properties": []},
       {"name": "dataset.exported", "source": "server", "properties": ["format"]},
       {"name": "dataset.imported", "source": "server", "properties": ["format"]},
       {"name": "dataset.items_added", "source": "server", "properties": ["source"]},
       {"name": "deletion.requested", "source": "server", "properties": []},
       {"name": "device.approved", "source": "server", "properties": []},
       {"name": "evaluator.created", "source": "server", "properties": ["kind"]},
       {"name": "evaluator.run", "source": "server", "properties": []},
       {"name": "experiment.created", "source": "server", "properties": []},
       {"name": "invite.created", "source": "server", "properties": ["role"]},
       {"name": "issue_source.created", "source": "server", "properties": ["kind"]},
       {"name": "list_layout.changed", "source": "web", "properties": ["layout"]},
       {"name": "live_tail.toggled", "source": "web", "properties": ["on"]},
       {"name": "llm_connection.created", "source": "server", "properties": ["adapter"]},
       {"name": "mcp.tool_called", "source": "server", "properties": ["ok", "tool"]},
       {"name": "online_rule.created", "source": "server", "properties": ["target"]},
       {"name": "org.created", "source": "server", "properties": []},
       {"name": "page.viewed", "source": "web", "properties": ["route"]},
       {"name": "personal_token.created", "source": "server", "properties": ["write"]},
       {"name": "playground.run", "source": "server", "properties": []},
       {"name": "project.created", "source": "server", "properties": []},
       {"name": "prompt.created", "source": "server", "properties": ["kind"]},
       {"name": "prompt.version_created", "source": "server", "properties": []},
       {"name": "saved_view.created", "source": "server", "properties": ["kind"]},
       {"name": "scores.created", "source": "server", "properties": []},
       {"name": "sql_console.run", "source": "server", "properties": []},
       {"name": "theme.changed", "source": "web", "properties": ["theme"]},
       {"name": "trace.viewed", "source": "server", "properties": []},
       {"name": "user.signed_in", "source": "server", "properties": ["method"]}
     ],
     "context": ["project_id", "via"],
     "page_routes": [ … the 65 strings of PAGE_ROUTES, in the same order, one per line … ]
   }
   ```
   The golden test compares parsed JSON values, not text, so this compact layout is fine; write the 65 `page_routes` strings one per line, indented four spaces, in `PAGE_ROUTES` order.

**Tests** (in `catalog.rs`'s `mod tests`; a helper `fn every_event() -> Vec<Event>` returns one sample of each of the 40 variants, e.g. `PageViewed { route: PAGE_ROUTES[0] }`, `McpToolCalled { tool: "query_sql", ok: true }`, `InviteCreated { role: OrgRole::Member }`):
- `the_catalog_matches_the_golden_file`: parse `include_str!("../../../../fixtures/analytics/catalog.json")` as `serde_json::Value`; build `json!({"events": CATALOG.iter().map(|s| json!({"name": s.name, "source": s.source, "properties": s.properties})).collect::<Vec<_>>(), "context": CONTEXT_PROPERTIES, "page_routes": PAGE_ROUTES})`; assert equal (on failure the message says "update fixtures/analytics/catalog.json and the web catalog together").
- `catalog_names_are_sorted_unique_and_dotted`: names strictly increasing; each matches `^[a-z][a-z_]*\.[a-z][a-z_]*$` (check by hand with `split_once('.')` and byte checks; no regex crate); exactly 40 entries; each `properties` slice is strictly increasing.
- `every_variant_is_in_the_catalog`: the set of `every_event()` names equals the set of `CATALOG` names, and `every_event().len() == 40`.
- `properties_match_the_spec`: for each sample, `properties()` keys sorted equal the spec's `properties`, and `source()` equals the spec's `source`.
- `property_values_are_scalars`: every value of every sample is a string, a bool or a number (never null, object or array).
- `page_routes_are_sorted_unique_templates`: strictly increasing; 65 entries; none of `/login`, `/signup`, `/reset`, `/verify`, `/setup/`, `/invite/$token`; each starts with `/`, contains no `//`, no `?`, no `#`; every non-empty segment is either `$` followed by ASCII letters, or made of `a-z`, `0-9` and `-` only.
- `parse_web_accepts_each_web_event`: the 6 web names with valid properties give the expected variants (`page.viewed` with `/$org/$project/traces/`, `live_tail.toggled` with `true`, `list_layout.changed` with `"bottom"`, `theme.changed` with `"system"`, the two without properties).
- `parse_web_refuses_server_and_unknown_names`: `dataset.created`, `org.created`, `user.signed_in`, `custom.event`, `""`, `PAGE.VIEWED` → `None`.
- `parse_web_refuses_a_url_as_route`: routes `/acme/support-bot/traces`, `/$org/$project/traces` (no trailing slash), `https://cetra.example/acme`, `/$org/$project/traces/?q=ada@example.com` → `None`.
- `parse_web_drops_unknown_properties`: `page.viewed` with `{"route": "/$org/$project/traces/", "email": "ada@example.com", "note": "free text"}` → `Some(PageViewed)`; its `properties()` has exactly the key `route`; the JSON text of `properties()` contains neither `ada@` nor `free text`.
- `parse_web_refuses_wrong_types`: `on: "true"`, `on` missing, `layout: "grid"`, `theme: 1`, `route: 5` → `None`.
- `the_route_value_is_the_catalog_constant`: for a route built at run time (`String::from("/$org/")`), the parsed `route` satisfies `std::ptr::eq(route, PAGE_ROUTES[1])`.
- `static_name_finds_catalog_names` (`"trace.viewed"` → `Some`; `"trace.view"` → `None`).
- `the_telemetry_page_lists_every_event`: `std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../website/src/content/docs/docs/self-hosting/telemetry.md"))`; for each catalog name, the text contains `` `<name>` `` (TL6.15 writes the list; the test passes once both are in).

**Commit:** `Add the product analytics event catalog`

---

### TL6.2 — `[analytics]` in the config

**Goal.** `[analytics]` with `posthog_host` and `posthog_key` (secret), both or neither, validated without quoting values; the literals and the configuration reference.

**Edit:**
- `crates/server/src/config.rs`
- `crates/server/src/test_support/mod.rs` (the two `ServerConfig { … }` literals and the `use crate::config::{…}` list only)
- `crates/server/src/ingest/fixtures_tests.rs` (the `ServerConfig { … }` literal and the `use crate::config::{…}` list only)
- `website/src/content/docs/docs/self-hosting/configuration-reference.md`

**M4 names used:** `TelemetryConfig`, `validate_telemetry` (TL4.3 step 5, its exact body and reasons), the `telemetry:` field of `ServerConfig` and of the three literals. Check them first; stop if they differ.

**Steps.**
1. In `config.rs`, next to `TelemetryConfig`, add:
   ```rust
   /// The rule `[analytics]` breaks when only one of its keys is set. Static.
   const ANALYTICS_PAIR: &str = "set posthog_host and posthog_key together, or leave both unset";

   /// `[analytics]`: per-user product events to the operator's PostHog (track TL M6, D-TL-14).
   /// Off while unset; restore mode turns it off too.
   #[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
   #[serde(deny_unknown_fields)]
   pub struct AnalyticsConfig {
       /// PostHog ingestion host, e.g. `https://eu.i.posthog.com`; batches go to `<host>/batch/`.
       /// `https`, or `http` to a loopback IP. Stored without a trailing slash.
       #[serde(default)]
       pub posthog_host: Option<String>,
       /// The PostHog project API key (`phc_…`).
       #[serde(default)]
       pub posthog_key: Option<Secret<String>>,
   }

   impl AnalyticsConfig {
       /// The host and key, when both are set.
       pub fn posthog(&self) -> Option<(&str, &Secret<String>)> {
           match (&self.posthog_host, &self.posthog_key) {
               (Some(host), Some(key)) => Some((host.as_str(), key)),
               _ => None,
           }
       }
   }
   ```
2. In `ServerConfig`, right after the `telemetry` field (and after any field M5 added right after it), add:
   ```rust
   /// `[analytics]`: per-user product events to PostHog (track TL M6).
   #[serde(default)]
   pub analytics: AnalyticsConfig,
   ```
3. Replace M4's `validate_telemetry` by a shared URL check and a one-line `validate_telemetry`; the reasons stay byte-identical, so M4's tests still pass:
   ```rust
   /// An outbound base URL of operator config (`telemetry.endpoint`, `analytics.posthog_host`):
   /// `https`, or `http` to a loopback IP address; no user info, query or fragment. Returned
   /// without a trailing slash. Reasons are static and never quote the input.
   fn outbound_base_url(raw: &str, key: &'static str) -> Result<String, ConfigError> {
       let invalid = |reason| ConfigError::Invalid { key, reason };
       let url = url::Url::parse(raw.trim())
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
       Ok(url.as_str().trim_end_matches('/').to_owned())
   }

   /// `telemetry.endpoint` (D-TL-10), by [`outbound_base_url`].
   fn validate_telemetry(telemetry: &mut TelemetryConfig) -> Result<(), ConfigError> {
       telemetry.endpoint = outbound_base_url(&telemetry.endpoint, "telemetry.endpoint")?;
       Ok(())
   }

   /// `[analytics]`: blank values are unset; both keys or neither; the host by
   /// [`outbound_base_url`]; the key printable ASCII without spaces, at most 256 bytes.
   fn validate_analytics(analytics: &mut AnalyticsConfig) -> Result<(), ConfigError> {
       if analytics.posthog_host.as_deref().is_some_and(|host| host.trim().is_empty()) {
           analytics.posthog_host = None;
       }
       if analytics
           .posthog_key
           .as_ref()
           .is_some_and(|key| key.expose().trim().is_empty())
       {
           analytics.posthog_key = None;
       }
       match (&analytics.posthog_host, &analytics.posthog_key) {
           (None, None) => return Ok(()),
           (Some(_), Some(_)) => {}
           _ => {
               return Err(ConfigError::Invalid {
                   key: "analytics",
                   reason: ANALYTICS_PAIR,
               });
           }
       }
       if let Some(host) = analytics.posthog_host.take() {
           analytics.posthog_host = Some(outbound_base_url(&host, "analytics.posthog_host")?);
       }
       if let Some(key) = &analytics.posthog_key {
           let value = key.expose();
           if value.len() > 256 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
               return Err(ConfigError::Invalid {
                   key: "analytics.posthog_key",
                   reason: "must be printable ASCII without spaces, at most 256 bytes",
               });
           }
       }
       Ok(())
   }
   ```
   In `validate`, right after `validate_telemetry(&mut config.telemetry)?;`, add `validate_analytics(&mut config.analytics)?;`.
4. In `secret_key`, before the `[..] if path.last()…client_secret` arm, add:
   ```rust
           [parent, key] if parent == "analytics" && key == "posthog_key" => {
               Some("analytics.posthog_key")
           }
   ```
5. In `is_secret_path`, add `| "analytics.posthog_key"` to the `matches!` list (an all-digit env value stays a string and is never printed).
6. Do not touch `ALIASES` or `is_filtered`: the env forms are `CETRA_ANALYTICS__POSTHOG_HOST` and `CETRA_ANALYTICS__POSTHOG_KEY`, which `split_key` maps.
7. In `test_support/mod.rs`, in both `ServerConfig { … }` literals, add `analytics: AnalyticsConfig::default(),` right after the `telemetry: TelemetryConfig { … },` block, and add `AnalyticsConfig` to the `use crate::config::{…}` list (alphabetical). Same in `ingest/fixtures_tests.rs` (`serve_config`).
8. In `configuration-reference.md`, right after the `## \`[telemetry]\`` section (its table and any rows M5 added), add:
   ```markdown
   ## `[analytics]`

   Per-user product events sent through the server to a PostHog project of yours. Off while unset; restore mode turns it off. cetra's own cloud sets it. A self-hosted install sends us only bucketed feature counts in the heartbeat: see [Telemetry](/docs/self-hosting/telemetry/#product-analytics).

   | Key | Environment | Type | Default | Description |
   |---|---|---|---|---|
   | `analytics.posthog_host` | | URL | unset (off) | Your PostHog ingestion host, such as `https://eu.i.posthog.com`. cetra posts batches to `<host>/batch/`. `https`, or `http` to a loopback IP address. No user info, query or fragment. Set together with `posthog_key`. |
   | `analytics.posthog_key` | | secret | unset | The PostHog project API key (`phc_…`). Set together with `posthog_host`. |
   ```

**Tests** (in `config.rs`'s `mod tests`, with `load_from`; add `AnalyticsConfig` to its `use super::{…}`):
- `analytics_defaults` (`load_from(None, &[])`: both `None`, `posthog()` is `None`); add `assert_eq!(config.analytics, AnalyticsConfig::default());` to `defaults_match_spec`.
- `analytics_loads_from_toml_and_env` (TOML `[analytics]\nposthog_host = "https://eu.i.posthog.com/"\n`, env `CETRA_ANALYTICS__POSTHOG_KEY=phc_abc123`: host stored `https://eu.i.posthog.com`; `posthog()` is `Some`; `format!("{config:?}")` does not contain `phc_abc123`).
- `an_all_digit_posthog_key_stays_a_string` (env key `12345` with a host: loads; `expose() == "12345"`).
- `analytics_needs_both_keys` (host only; key only → `matches!(e, ConfigError::Invalid { key: "analytics", reason } if reason == ANALYTICS_PAIR)`).
- `blank_analytics_values_are_unset` (`CETRA_ANALYTICS__POSTHOG_HOST=` and `CETRA_ANALYTICS__POSTHOG_KEY=  ` → loads, both `None`).
- `posthog_host_refuses` (each with a valid key, `key: "analytics.posthog_host"` and the matching reason of step 3): `not a url`; `http://eu.i.posthog.com`, `http://localhost:8000`, `ftp://h.example`; `https://u:p@h.example`, `https://h.example/?a=1`, `https://h.example/#f`.
- `posthog_host_accepts_loopback_http` (`http://127.0.0.1:9999/` → `http://127.0.0.1:9999`).
- `posthog_key_refuses_without_quoting_it` (`phc abc`, 257 × `a`, `phc\u{7f}x` → `Invalid { key: "analytics.posthog_key", .. }`; `error.to_string()` contains none of the values).
- `a_bad_posthog_key_type_is_reported_by_key` (TOML `posthog_key = 5` → `Invalid { key: "analytics.posthog_key", reason: "expected a string" }`).
- `unknown_analytics_key_is_refused` (`[analytics]\nposthog = 1\n` → `Err(ConfigError::Figment(_))`).
- M4's `telemetry_endpoint_accepts` and `telemetry_endpoint_refuses` pass unchanged; `the_configuration_reference_matches_the_loader` passes with the two new rows.

**Commit:** `Add the [analytics] configuration`

---

### TL6.3 — The actor and the privacy signal

**Goal.** Who did an event (`Actor`, `Via`), built from principals and access extensions, and the request's `DNT`/`Sec-GPC` opt-out kept in a task-local by a middleware.

**Edit:**
- `crates/server/src/analytics/actor.rs` (new)
- `crates/server/src/analytics/mod.rs` (one line: `pub(crate) mod actor;`)

**Steps.**
1. Create `actor.rs`. Module doc: `//! Who an event is about (D-TL-15): the user and org come from the session and the access checks, never from the browser's claim. The request's `DNT: 1` or `Sec-GPC: 1` is kept in a task-local for the length of the request; anywhere else the answer is "opted out", so a forgotten scope never sends.` Contents, in full:
   ```rust
   use axum::extract::Request;
   use axum::http::HeaderMap;
   use axum::middleware::Next;
   use axum::response::Response;
   use cetra_core::ids::{OrgId, ProjectId};
   use serde::Serialize;
   use uuid::Uuid;

   use crate::auth::principal::Principal;
   use crate::auth::roles::OrgRole;
   use crate::http::access::{OrgAccess, ProjectAccess};

   tokio::task_local! {
       /// The current request opted out of per-user analytics (`DNT: 1` or `Sec-GPC: 1`).
       static OPTED_OUT: bool;
   }

   /// How a server event's caller authenticated (the `via` context property).
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   #[serde(rename_all = "snake_case")]
   pub(crate) enum Via {
       /// A web session (the UI).
       Session,
       /// A personal access token (CLI, MCP, scripts).
       Pat,
       /// A project API key (SDKs).
       ApiKey,
       /// The cloud control plane.
       ControlPlane,
   }

   /// Who an event is about. Carries ids and enums only: no email, name, slug or address can
   /// reach a sink through it.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct Actor {
       /// The user; `None` for API keys and the control plane (counters only).
       pub(crate) user_id: Option<Uuid>,
       /// How a server event's caller authenticated; `None` for web events.
       pub(crate) via: Option<Via>,
       /// The org, when the action has one.
       pub(crate) org_id: Option<OrgId>,
       /// The user's role in that org, when known without another query.
       pub(crate) org_role: Option<OrgRole>,
       /// The project, when the action has one.
       pub(crate) project_id: Option<ProjectId>,
       /// The request opted out of per-user analytics: no PostHog event, counters only.
       pub(crate) opted_out: bool,
   }

   impl Actor {
       /// The caller of this request, without an org.
       pub(crate) fn principal(principal: &Principal) -> Self {
           let (user_id, via) = match principal {
               Principal::Session { user, .. } => (Some(user.id), Via::Session),
               Principal::Pat { user, .. } => (Some(user.id), Via::Pat),
               Principal::ApiKey { .. } => (None, Via::ApiKey),
               Principal::ControlPlane { .. } => (None, Via::ControlPlane),
           };
           Self {
               user_id,
               via: Some(via),
               org_id: None,
               org_role: None,
               project_id: None,
               opted_out: request_opted_out(),
           }
       }

       /// The caller on a project route: the project and its org.
       pub(crate) fn project(principal: &Principal, access: &ProjectAccess) -> Self {
           Self {
               org_id: Some(access.project.org_id),
               project_id: Some(access.project.id),
               ..Self::principal(principal)
           }
       }

       /// The caller on an org route: the org and the caller's role there.
       pub(crate) fn org(principal: &Principal, access: &OrgAccess) -> Self {
           Self {
               org_id: Some(access.org.id),
               org_role: Some(access.role),
               ..Self::principal(principal)
           }
       }

       /// A user known by id only (a session that just started, an MCP tool call).
       pub(crate) fn user(user_id: Uuid, via: Via) -> Self {
           Self {
               user_id: Some(user_id),
               via: Some(via),
               org_id: None,
               org_role: None,
               project_id: None,
               opted_out: request_opted_out(),
           }
       }
   }

   /// `DNT: 1` or `Sec-GPC: 1` (values trimmed).
   pub(crate) fn opts_out(headers: &HeaderMap) -> bool {
       ["dnt", "sec-gpc"].iter().any(|name| {
           headers
               .get(*name)
               .and_then(|value| value.to_str().ok())
               .is_some_and(|value| value.trim() == "1")
       })
   }

   /// Whether the current request opted out. Outside a request scope: `true` (fail closed).
   pub(crate) fn request_opted_out() -> bool {
       OPTED_OUT.try_with(|opted_out| *opted_out).unwrap_or(true)
   }

   /// Middleware: runs the rest of the request with its opt-out in [`OPTED_OUT`].
   pub(crate) async fn privacy_signal(request: Request, next: Next) -> Response {
       let opted_out = opts_out(request.headers());
       OPTED_OUT.scope(opted_out, next.run(request)).await
   }
   ```
   If `OrgAccess`, `ProjectAccess`, `Principal` or `OrgRole` live elsewhere than the imports above, stop and report.

**Tests** (in `actor.rs`):
- `opts_out_reads_dnt_and_gpc`: `DNT: 1` → true; `Sec-GPC: 1` → true; `DNT:  1 ` → true; `DNT: 0`, `DNT: yes`, `Sec-GPC: 0`, no header → false.
- `outside_a_request_is_opted_out` (`request_opted_out()` in a plain `#[tokio::test]` → true).
- `the_middleware_scopes_the_signal`: a `Router::new().route("/", get(|| async { request_opted_out().to_string() })).layer(axum::middleware::from_fn(privacy_signal))`, called with `tower::ServiceExt::oneshot`: with `DNT: 1` → body `true`; with `Sec-GPC: 1` → `true`; with no header → `false`.
- `principal_actors` (build each `Principal` variant by hand; for `Session`, `AuthUser { id, is_instance_admin: false, email: "ada@example.com".to_owned() }`, `session_hash: "h".to_owned()`, `sso_org: None`): `Session` → user `Some(id)`, via `Session`; `Pat` → `Pat`; `ApiKey` → user `None`, via `ApiKey`; `ControlPlane` → `ControlPlane`; outside a scope each has `opted_out == true`.
- `project_and_org_actors` (a `ProjectAccess` from a hand-built `ProjectRow`, or `TestApp::new().await.seed().await.project`: org and project ids set, role `None`; an `OrgAccess` with role `Admin`: org id and `Some(OrgRole::Admin)`, project `None`).
- `the_actor_has_no_text_fields`: `format!("{actor:?}")` of a session actor built from a user with email `ada@example.com` does not contain `ada@`.

**Commit:** `Add the analytics actor and the DNT and GPC signal`

---

### TL6.4 — Feature counters

**Goal.** A per-event-name counter that counts every recorded event, its usage buckets for the heartbeat, and its persistence in `instance_settings` while the heartbeat runs.

**Edit:**
- `crates/server/src/analytics/counters.rs` (new)
- `crates/server/src/analytics/mod.rs` (one line: `pub(crate) mod counters;`)
- `crates/server/src/meta/instance_settings.rs` (one constant)

**M4 names used:** `Decision`, `Switch`, `get_setting`, `put_setting`, `MetaDb::write`. Check first.

**Steps.**
1. In `meta/instance_settings.rs`, after `TELEMETRY_LAST_HEARTBEAT_AT`, add:
   ```rust
   /// Key of the feature counts not yet sent in a heartbeat (track TL M6): a JSON object of
   /// catalog event name to count.
   pub(crate) const TELEMETRY_USAGE_COUNTS: &str = "telemetry.usage_counts";
   ```
2. Create `counters.rs`. Module doc: `//! Feature counters (design § 6): every catalog event, from any source, adds one to its name's counter. The heartbeat sends them as buckets in `usage` and subtracts what was accepted. While the heartbeat runs they are saved hourly to `instance_settings`; otherwise they stay in memory and nothing reads them. No users, orgs, sequences or timings. This module reads nothing of the licence code (D172).` Contents, in full:
   ```rust
   use std::collections::BTreeMap;
   use std::sync::{Mutex, MutexGuard, PoisonError};
   use std::time::Duration;

   use serde::Serialize;
   use tokio::task::JoinHandle;

   use crate::analytics::catalog::{CATALOG, static_name};
   use crate::http::AppState;
   use crate::meta::instance_settings::{TELEMETRY_USAGE_COUNTS, get_setting, put_setting};
   use crate::observability::switchboard::{Decision, Switch};

   /// How often the counts are saved while the heartbeat runs.
   pub(crate) const SAVE_PERIOD: Duration = Duration::from_secs(60 * 60);

   /// Counts by catalog event name.
   pub(crate) type Counts = BTreeMap<&'static str, u64>;

   /// A count since the last heartbeat, as sent (D-TL-8: never an exact count).
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
   pub(crate) enum UsageBucket {
       #[serde(rename = "0")]
       Zero,
       #[serde(rename = "1-9")]
       UnderTen,
       #[serde(rename = "10-99")]
       UnderHundred,
       #[serde(rename = "100-999")]
       UnderThousand,
       #[serde(rename = "1000+")]
       ThousandOrMore,
   }

   /// `n` as a [`UsageBucket`].
   pub(crate) fn usage_bucket(n: u64) -> UsageBucket {
       match n {
           0 => UsageBucket::Zero,
           1..=9 => UsageBucket::UnderTen,
           10..=99 => UsageBucket::UnderHundred,
           100..=999 => UsageBucket::UnderThousand,
           _ => UsageBucket::ThousandOrMore,
       }
   }

   /// The heartbeat's `usage`: every catalog name, with its bucket (`0` when not counted).
   pub(crate) fn usage_buckets(counts: &Counts) -> BTreeMap<&'static str, UsageBucket> {
       CATALOG
           .iter()
           .map(|spec| {
               let count = counts.get(spec.name).copied().unwrap_or(0);
               (spec.name, usage_bucket(count))
           })
           .collect()
   }

   /// The in-memory counters.
   #[derive(Debug, Default)]
   pub(crate) struct Counters {
       counts: Mutex<Counts>,
   }

   impl Counters {
       fn lock(&self) -> MutexGuard<'_, Counts> {
           self.counts.lock().unwrap_or_else(PoisonError::into_inner)
       }

       /// Adds one to `name` (saturating).
       pub(crate) fn increment(&self, name: &'static str) {
           let mut counts = self.lock();
           let count = counts.entry(name).or_insert(0);
           *count = count.saturating_add(1);
       }

       /// The count of `name`. Read by tests.
       #[cfg_attr(not(test), allow(dead_code))]
       pub(crate) fn get(&self, name: &str) -> u64 {
           self.lock().get(name).copied().unwrap_or(0)
       }

       /// A copy of every count.
       pub(crate) fn snapshot(&self) -> Counts {
           self.lock().clone()
       }

       /// Removes what a heartbeat sent; events counted since the snapshot stay.
       pub(crate) fn subtract(&self, sent: &Counts) {
           let mut counts = self.lock();
           for (name, sent) in sent {
               if let Some(count) = counts.get_mut(name) {
                   *count = count.saturating_sub(*sent);
               }
           }
           counts.retain(|_, count| *count > 0);
       }

       /// Adds saved counts (at startup).
       pub(crate) fn add(&self, saved: &Counts) {
           let mut counts = self.lock();
           for (name, saved) in saved {
               let count = counts.entry(*name).or_insert(0);
               *count = count.saturating_add(*saved);
           }
       }
   }

   /// The saved form: a JSON object of name to count.
   pub(crate) fn encode(counts: &Counts) -> String {
       // A map of strings to integers always serializes; "{}" is never reached.
       serde_json::to_string(counts).unwrap_or_else(|_| "{}".to_owned())
   }

   /// Reads the saved form. Names no longer in the catalog and unreadable text are dropped:
   /// a lost count is the safe side.
   pub(crate) fn decode(text: &str) -> Counts {
       let Ok(parsed) = serde_json::from_str::<BTreeMap<String, u64>>(text) else {
           return Counts::new();
       };
       parsed
           .into_iter()
           .filter_map(|(name, count)| static_name(&name).map(|name| (name, count)))
           .collect()
   }
   ```
3. Add the persistence functions; bodies as numbered:
   - `pub(crate) async fn load(state: &AppState)`: `get_setting(state.meta.reader(), TELEMETRY_USAGE_COUNTS).await`; `Ok(Some(text))` → `state.analytics.counters.add(&decode(&text))`; `Ok(None)` → nothing; `Err(error)` → `tracing::debug!(error = %error, "analytics: saved feature counts not loaded")`.
   - `pub(crate) async fn save(state: &AppState)`: `let text = encode(&state.analytics.counters.snapshot());` then `state.meta.write(move |conn| Box::pin(async move { put_setting(conn, TELEMETRY_USAGE_COUNTS, &text).await })).await`; on `Err(error)`: `tracing::debug!(error = %error, "analytics: feature counts not saved")`.
   - `pub(crate) async fn start(state: &AppState, decision: Decision) -> Option<JoinHandle<()>>`. Doc: "Loads the saved counts and saves them every hour, only while the heartbeat runs (the switchboard)." Steps: (1) `if let Switch::Off(_) = decision.heartbeat { return None; }` (2) `load(state).await;` (3) `let state = state.clone();` and return `Some(tokio::spawn(async move { loop { tokio::time::sleep(SAVE_PERIOD).await; save(&state).await; } }))`.
   `state.analytics` is the field TL6.6 adds; this phase compiles once TL6.6 is in (the milestone check sees both).

**Tests** (in `counters.rs`):
- `usage_bucket_edges` (0, 1, 9, 10, 99, 100, 999, 1000, `u64::MAX`).
- `bucket_strings_are_the_wire_format` (each variant serializes to `"0"`, `"1-9"`, `"10-99"`, `"100-999"`, `"1000+"`).
- `usage_buckets_list_every_catalog_name` (empty counts → 40 entries, all `Zero`; keys equal the `CATALOG` names).
- `usage_buckets_bucket_the_counts` (`{"trace.viewed": 1500, "page.viewed": 250}` → `ThousandOrMore`, `UnderThousand`, the rest `Zero`).
- `increment_get_and_snapshot` (three increments of `dataset.created` → `get == 3`; snapshot has one entry).
- `subtract_keeps_what_came_after` (count 5, snapshot, two more increments, subtract snapshot → 2; subtract a bigger count → entry removed, never negative).
- `add_merges_saved_counts` (and saturates at `u64::MAX`).
- `encode_and_decode_round_trip`; `decode_drops_unknown_names_and_bad_text` (`{"dataset.created": 2, "old.event": 9}` → only `dataset.created`; `not json`, `[]`, `{"dataset.created": -1}` → empty).
- `concurrent_increments_are_not_lost` (8 threads × 1000 increments → 8000).
- A test helper builds decisions without naming `Decision`'s other fields (M5 adds `crash_reports`, and may land first), in full:
  ```rust
  /// A switchboard decision with `heartbeat` set, whatever other fields `Decision` has.
  fn decision(heartbeat: Switch) -> Decision {
      let mut decision = for_config(&crate::test_support::config(std::path::Path::new(".")));
      decision.heartbeat = heartbeat;
      decision
  }
  ```
  (import `for_config` and `OffReason` from `crate::observability::switchboard` in the test module). Never write a `Decision { … }` literal.
- `start_is_off_without_the_heartbeat` (`TestApp::new()`, `decision(Switch::Off(OffReason::DebugBuild))` → `None`; no `telemetry.usage_counts` row written).
- `start_loads_saved_counts` (`put_setting(TELEMETRY_USAGE_COUNTS, r#"{"org.created":4}"#)`, `start(.., decision(Switch::On))` → `Some(handle)`; abort it; `counters.get("org.created") == 4`).
- `save_writes_the_snapshot` (increment twice, `save`, `get_setting` → `{"dataset.created":2}`).
- `the_counter_modules_do_not_touch_licences`: the same scan as M4's `the_telemetry_modules_do_not_touch_licences` over `include_str!("counters.rs")` and `include_str!("catalog.rs")`, each cut at its first `#[cfg(test)]`, forbidding `concat!("crate::", "ee")`, `concat!("licen", "se")`, `concat!("entitle", "ments")`.

**Commit:** `Add the analytics feature counters`

---

### TL6.5 — The PostHog sink

**Goal.** A bounded queue and a background worker that post catalog events to PostHog's batch endpoint, with the exact payload shape, group identification and quiet failures.

**Edit:**
- `crates/server/src/analytics/posthog.rs` (new)
- `crates/server/src/analytics/test_receiver.rs` (new)
- `crates/server/src/analytics/mod.rs` (two lines: `pub(crate) mod posthog;` and `#[cfg(test)] pub(crate) mod test_receiver;`)

**Steps.**
1. Create `posthog.rs`. Module doc: `//! The PostHog sink (D-TL-14): per-user catalog events, batched to `<host>/batch/` every 10 s or 100 events, from a queue of 10 000 that drops new events when full. No retry; failures are logged at `debug` at most once a minute. A capture carries the user's UUID (as `distinct_id` at the top level and in `properties`), the org id as the `org` group, the catalog properties, `project_id`, `via`, `$ip: null`, `$geoip_disable: true`, and the org role as the only person property; the org's plan is the only group property, sent by `$groupidentify` with the group-scoped `distinct_id` `org_<org id>`. Q verifies the shape with one real batch (TL-Q4). Field names follow PostHog's capture API (https://posthog.com/docs/api/capture).` Contents:
   ```rust
   use std::collections::HashMap;
   use std::sync::{Mutex, PoisonError};
   use std::time::{Duration, Instant};

   use cetra_core::ids::OrgId;
   use chrono::{DateTime, SecondsFormat};
   use serde::Serialize;
   use serde_json::{Map, Value};
   use tokio::sync::mpsc;
   use tokio::time::MissedTickBehavior;
   use uuid::Uuid;

   use crate::analytics::actor::Actor;
   use crate::analytics::catalog::{Event, Source};
   use crate::config::Secret;

   /// A batch goes out at least this often while events wait.
   pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_secs(10);
   /// A batch goes out as soon as it holds this many events.
   pub(crate) const MAX_BATCH: usize = 100;
   /// Events waiting at most; later ones are dropped until the worker catches up.
   pub(crate) const QUEUE_CAPACITY: usize = 10_000;
   /// Each request gives up after this long.
   pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
   /// At most one failure line per this interval.
   const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);
   /// The PostHog group type of an org.
   pub(crate) const GROUP_TYPE: &str = "org";
   /// A group's `plan` is sent again after this long (ns), or when it changes.
   const GROUP_REFRESH_NS: i64 = 3_600 * 1_000_000_000;

   /// Why a batch was not sent. Never carries a URL, the key or a body.
   #[derive(Debug, thiserror::Error)]
   pub(crate) enum PosthogError {
       /// The HTTP client could not be built.
       #[error("analytics client could not be built")]
       Client,
       /// The request failed before a response.
       #[error("analytics request failed (timeout: {timeout})")]
       Request {
           /// The request hit the client's timeout.
           timeout: bool,
       },
       /// PostHog answered something other than 2xx (redirects are not followed).
       #[error("analytics endpoint answered {0}")]
       Status(u16),
   }

   /// One PostHog event of a batch. Field order is the JSON key order.
   #[derive(Debug, Clone, PartialEq, Serialize)]
   pub(crate) struct Capture {
       pub(crate) event: String,
       pub(crate) distinct_id: String,
       pub(crate) timestamp: String,
       pub(crate) properties: Map<String, Value>,
   }

   /// The body of `POST <host>/batch/`.
   #[derive(Serialize)]
   struct BatchBody<'a> {
       api_key: &'a str,
       batch: &'a [Capture],
   }

   /// The handle `record` uses: the queue and the group bookkeeping.
   #[derive(Debug)]
   pub(crate) struct PosthogSink {
       queue: mpsc::Sender<Capture>,
       /// Per org: the plan last sent and when (ns).
       groups: Mutex<HashMap<OrgId, (String, i64)>>,
   }

   /// The background half: drains the queue and posts batches. Spawned by `serve`.
   #[derive(Debug)]
   pub(crate) struct SinkWorker {
       queue: mpsc::Receiver<Capture>,
       client: reqwest::Client,
       url: String,
       key: Secret<String>,
       flush_interval: Duration,
   }
   ```
   Give the `Capture` fields one-line docs (`event`: the catalog name or `$groupidentify`; `distinct_id`: the user's UUID; `timestamp`: RFC 3339 UTC with milliseconds; `properties`: as built by [`capture`] or [`group_identify`]).
2. Free functions, in full:
   ```rust
   /// `<host>/batch/`.
   pub(crate) fn batch_url(host: &str) -> String {
       format!("{}/batch/", host.trim_end_matches('/'))
   }

   /// RFC 3339, UTC, milliseconds: `2026-01-01T00:00:00.000Z`.
   fn timestamp(now_ns: i64) -> String {
       DateTime::from_timestamp_nanos(now_ns).to_rfc3339_opts(SecondsFormat::Millis, true)
   }

   /// The PostHog event of a catalog event.
   pub(crate) fn capture(event: &Event, actor: &Actor, user_id: Uuid, now_ns: i64) -> Capture {
       let mut properties = event.properties();
       if let Some(project) = actor.project_id {
           properties.insert("project_id".to_owned(), Value::String(project.to_string()));
       }
       if let (Some(via), Source::Server) = (actor.via, event.source()) {
           properties.insert("via".to_owned(), serde_json::to_value(via).unwrap_or(Value::Null));
       }
       if let Some(org) = actor.org_id {
           let mut groups = Map::new();
           groups.insert(GROUP_TYPE.to_owned(), Value::String(org.to_string()));
           properties.insert("$groups".to_owned(), Value::Object(groups));
       }
       if let Some(role) = actor.org_role {
           let mut set = Map::new();
           set.insert("org_role".to_owned(), serde_json::to_value(role).unwrap_or(Value::Null));
           properties.insert("$set".to_owned(), Value::Object(set));
       }
       properties.insert("distinct_id".to_owned(), Value::String(user_id.to_string()));
       properties.insert("$ip".to_owned(), Value::Null);
       properties.insert("$geoip_disable".to_owned(), Value::Bool(true));
       properties.insert("$process_person_profile".to_owned(), Value::Bool(true));
       Capture {
           event: event.name().to_owned(),
           distinct_id: user_id.to_string(),
           timestamp: timestamp(now_ns),
           properties,
       }
   }

   /// `$groupidentify` for `org` with its plan, the only group property. Its `distinct_id` is
   /// group-scoped (`org_<id>`) and builds no person profile, as PostHog's own SDKs do.
   pub(crate) fn group_identify(org: OrgId, plan: &str, now_ns: i64) -> Capture {
       let distinct_id = format!("{GROUP_TYPE}_{org}");
       let mut set = Map::new();
       set.insert("plan".to_owned(), Value::String(plan.to_owned()));
       let mut properties = Map::new();
       properties.insert("distinct_id".to_owned(), Value::String(distinct_id.clone()));
       properties.insert("$group_type".to_owned(), Value::String(GROUP_TYPE.to_owned()));
       properties.insert("$group_key".to_owned(), Value::String(org.to_string()));
       properties.insert("$group_set".to_owned(), Value::Object(set));
       properties.insert("$ip".to_owned(), Value::Null);
       properties.insert("$geoip_disable".to_owned(), Value::Bool(true));
       properties.insert("$process_person_profile".to_owned(), Value::Bool(false));
       Capture {
           event: "$groupidentify".to_owned(),
           distinct_id,
           timestamp: timestamp(now_ns),
           properties,
       }
   }

   /// The sink's own HTTP client: no redirects, 10 s timeout, `User-Agent: cetra/<version>`.
   fn client() -> Result<reqwest::Client, PosthogError> {
       let builder = reqwest::Client::builder()
           .redirect(reqwest::redirect::Policy::none())
           .timeout(REQUEST_TIMEOUT)
           .user_agent(format!("cetra/{}", env!("CARGO_PKG_VERSION")));
       // A developer's HTTP(S)_PROXY must not catch the tests' 127.0.0.1 receivers (as the heartbeat's client).
       #[cfg(test)]
       let builder = builder.no_proxy();
       builder.build().map_err(|_| PosthogError::Client)
   }

   /// Posts one batch. The response body is never read.
   async fn post(
       client: &reqwest::Client,
       url: &str,
       api_key: &str,
       batch: &[Capture],
   ) -> Result<(), PosthogError> {
       let response = client
           .post(url)
           .json(&BatchBody { api_key, batch })
           .send()
           .await
           .map_err(|error| PosthogError::Request { timeout: error.is_timeout() })?;
       let status = response.status();
       if status.is_success() {
           Ok(())
       } else {
           Err(PosthogError::Status(status.as_u16()))
       }
   }
   ```
   `serde_json::to_value` of a unit enum (`Via`, `OrgRole`) always gives a string; add that invariant as a comment above each `unwrap_or(Value::Null)`.
3. `impl PosthogSink`:
   - `pub(crate) fn new(host: &str, key: Secret<String>, flush_interval: Duration) -> Result<(Self, SinkWorker), PosthogError>`: (1) `let client = client()?;` (2) `let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);` (3) return the sink (`groups` empty) and `SinkWorker { queue: receiver, client, url: batch_url(host), key, flush_interval }`.
   - `pub(crate) fn enqueue(&self, capture: Capture)`: `let _ = self.queue.try_send(capture);` with the comment "Full or closed: the event is dropped (design: bounded queue, dropped when full)."
   - `pub(crate) fn group_due(&self, org: OrgId, plan: &str, now_ns: i64) -> bool`: lock `groups` (`unwrap_or_else(PoisonError::into_inner)`); due when the org is absent, its plan differs from `plan`, or `now_ns.saturating_sub(sent_at) >= GROUP_REFRESH_NS`; when due, store `(plan.to_owned(), now_ns)` and return true; else false.
4. `impl SinkWorker`:
   - `pub(crate) async fn run(mut self)`, in full:
     ```rust
     let mut batch: Vec<Capture> = Vec::with_capacity(MAX_BATCH);
     let mut ticker = tokio::time::interval(self.flush_interval);
     ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
     ticker.tick().await; // The first tick completes at once.
     let mut last_failure_log: Option<Instant> = None;
     loop {
         tokio::select! {
             received = self.queue.recv() => match received {
                 Some(capture) => {
                     batch.push(capture);
                     if batch.len() >= MAX_BATCH {
                         self.flush(&mut batch, &mut last_failure_log).await;
                     }
                 }
                 None => {
                     self.flush(&mut batch, &mut last_failure_log).await;
                     return;
                 }
             },
             _ = ticker.tick() => {
                 if !batch.is_empty() {
                     self.flush(&mut batch, &mut last_failure_log).await;
                 }
             }
         }
     }
     ```
   - `async fn flush(&self, batch: &mut Vec<Capture>, last_failure_log: &mut Option<Instant>)`: (1) if `batch` is empty, return; (2) `let result = post(&self.client, &self.url, self.key.expose(), batch).await;` (3) `batch.clear();` (no retry) (4) on `Err(error)`, if `last_failure_log.is_none_or(|at| at.elapsed() >= FAILURE_LOG_INTERVAL)`: set it to `Some(Instant::now())` and `tracing::debug!(error = %error, "analytics: batch not sent")`. Nothing in this module logs above `debug`.
5. Create `test_receiver.rs` (compiled in tests only, through the `#[cfg(test)]` line in `mod.rs`). Module doc: `//! An in-process PostHog stand-in for tests: records `POST /batch/` bodies.` Contents, in full:
   ```rust
   use std::sync::atomic::{AtomicUsize, Ordering};
   use std::sync::{Arc, Mutex, PoisonError};
   use std::time::Duration;

   use axum::body::Bytes;
   use axum::extract::State;
   use axum::http::{HeaderMap, StatusCode, header};
   use axum::response::{IntoResponse, Response};
   use axum::routing::{get, post};
   use axum::Router;

   /// Serves `POST /batch/` on `127.0.0.1:0` and answers `status` (with
   /// `Location: /elsewhere` for a 3xx). `/elsewhere` counts hits. Aborts its task on drop.
   pub(crate) struct Receiver {
       pub(crate) base: String,
       requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
       pub(crate) elsewhere: Arc<AtomicUsize>,
       task: tokio::task::JoinHandle<()>,
   }

   #[derive(Clone)]
   struct ReceiverState {
       status: u16,
       requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
       elsewhere: Arc<AtomicUsize>,
   }

   impl Receiver {
       pub(crate) async fn start(status: u16) -> Self {
           let requests = Arc::new(Mutex::new(Vec::new()));
           let elsewhere = Arc::new(AtomicUsize::new(0));
           let state = ReceiverState {
               status,
               requests: Arc::clone(&requests),
               elsewhere: Arc::clone(&elsewhere),
           };
           let router = Router::new()
               .route("/batch/", post(batch))
               .route("/elsewhere", get(other).post(other))
               .with_state(state);
           let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
           let addr = listener.local_addr().unwrap();
           let task = tokio::spawn(async move {
               let _ = axum::serve(listener, router).await;
           });
           Self { base: format!("http://{addr}"), requests, elsewhere, task }
       }

       /// Every request so far.
       pub(crate) fn requests(&self) -> Vec<(HeaderMap, Bytes)> {
           self.requests.lock().unwrap_or_else(PoisonError::into_inner).clone()
       }

       /// The JSON bodies so far.
       pub(crate) fn bodies(&self) -> Vec<serde_json::Value> {
           self.requests()
               .iter()
               .map(|(_, body)| serde_json::from_slice(body).unwrap())
               .collect()
       }

       /// Waits up to `within` for at least `count` requests, then returns the bodies.
       pub(crate) async fn wait_for(&self, count: usize, within: Duration) -> Vec<serde_json::Value> {
           let deadline = tokio::time::Instant::now() + within;
           while self.requests().len() < count && tokio::time::Instant::now() < deadline {
               tokio::time::sleep(Duration::from_millis(20)).await;
           }
           self.bodies()
       }
   }

   impl Drop for Receiver {
       fn drop(&mut self) {
           self.task.abort();
       }
   }

   async fn batch(State(state): State<ReceiverState>, headers: HeaderMap, body: Bytes) -> Response {
       state.requests.lock().unwrap_or_else(PoisonError::into_inner).push((headers, body));
       let status = StatusCode::from_u16(state.status).unwrap();
       if status.is_redirection() {
           (status, [(header::LOCATION, "/elsewhere")]).into_response()
       } else {
           status.into_response()
       }
   }

   async fn other(State(state): State<ReceiverState>) -> StatusCode {
       state.elsewhere.fetch_add(1, Ordering::SeqCst);
       StatusCode::OK
   }
   ```
   (`unwrap()` is allowed: test-only module.)

**Tests** (in `posthog.rs`; `const NOW: i64 = 1_767_225_600_000_000_000;` is 2026-01-01T00:00:00Z; fixed ids `user = Uuid::parse_str("01900000-0000-7000-8000-000000000101")`, `org = "01900000-0000-7000-8000-000000000010".parse::<OrgId>()`, `project = "01900000-0000-7000-8000-000000000001".parse::<ProjectId>()`):
- `batch_url_appends_batch` (`https://eu.i.posthog.com` and `https://eu.i.posthog.com/` → `https://eu.i.posthog.com/batch/`; `http://127.0.0.1:9` → `http://127.0.0.1:9/batch/`).
- `capture_has_the_documented_shape`: `capture(&Event::EvaluatorCreated { kind: EvaluatorKind::LlmJudge }, &Actor { user_id: Some(user), via: Some(Via::Session), org_id: Some(org), org_role: Some(OrgRole::Admin), project_id: Some(project), opted_out: false }, user, NOW)`; `serde_json::to_value` equals exactly:
  ```json
  {
    "event": "evaluator.created",
    "distinct_id": "01900000-0000-7000-8000-000000000101",
    "timestamp": "2026-01-01T00:00:00.000Z",
    "properties": {
      "kind": "llm_judge",
      "project_id": "01900000-0000-7000-8000-000000000001",
      "via": "session",
      "$groups": {"org": "01900000-0000-7000-8000-000000000010"},
      "$set": {"org_role": "admin"},
      "distinct_id": "01900000-0000-7000-8000-000000000101",
      "$ip": null,
      "$geoip_disable": true,
      "$process_person_profile": true
    }
  }
  ```
  and the serialized text keeps that key order (`preserve_order`).
- `a_web_event_has_no_via` (`PageViewed` with `via: Some(Via::Session)` → no `via` key; `route` present).
- `capture_without_org_or_role_has_no_groups_or_set`.
- `group_identify_has_the_documented_shape` (`group_identify(org, "Self-hosted", NOW)` is exactly `{"event": "$groupidentify", "distinct_id": "org_01900000-0000-7000-8000-000000000010", "timestamp": "2026-01-01T00:00:00.000Z", "properties": {"distinct_id": "org_01900000-0000-7000-8000-000000000010", "$group_type": "org", "$group_key": "01900000-0000-7000-8000-000000000010", "$group_set": {"plan": "Self-hosted"}, "$ip": null, "$geoip_disable": true, "$process_person_profile": false}}`, in that key order).
- `group_due_rules` (first call true; again false; another plan true; same plan 1 h later true; 59 min later false).
- `the_worker_posts_a_batch_after_the_interval`: `Receiver::start(200)`, `PosthogSink::new(&receiver.base, Secret::new("phc_test".to_owned()), Duration::from_millis(50))`, spawn `worker.run()`, enqueue 3 captures; `wait_for(1, 2 s)`: one body with `api_key == "phc_test"`, `batch` of 3 in enqueue order; `content-type` starts with `application/json`; `user-agent == format!("cetra/{}", env!("CARGO_PKG_VERSION"))`.
- `the_worker_flushes_at_100_events` (interval 1 h; enqueue 100 → one request with 100 events within 2 s; enqueue 1 more → still one request after 300 ms).
- `dropping_the_sink_flushes_and_stops` (interval 1 h; enqueue 2, drop the sink; the worker's join handle finishes within 2 s; one request with 2 events).
- `a_full_queue_drops_new_events` (do not spawn the worker; enqueue `QUEUE_CAPACITY + 5` captures without blocking; then spawn the worker with interval 50 ms: the total number of events received within 5 s is exactly `QUEUE_CAPACITY`).
- `redirects_are_not_followed` (receiver 307: one request, `elsewhere` stays 0).
- `failures_are_logged_at_debug_once_a_minute` (`crate::test_support::capture_logs` around: receiver 503, interval 50 ms, enqueue one capture, wait 200 ms, enqueue another, wait 200 ms; the output contains neither `WARN` nor `ERROR`, contains `analytics: batch not sent` at most once, and does not contain the receiver's port, `phc_test` or `/batch/`).
- `the_key_never_reaches_debug_output` (`format!("{worker:?}")` does not contain `phc_test`).

**Commit:** `Add the PostHog sink for product analytics`

---

### TL6.6 — Analytics state, `record`, and the wiring

**Goal.** The `Analytics` state on `AppInner` (sink, counters, counting flag, per-user limit), the single `record` function, the privacy middleware on the router, and the sink worker and counter saver started by `serve`.

**Edit:**
- `crates/server/src/analytics/mod.rs`
- `crates/server/src/http/mod.rs`
- `crates/server/src/test_support/mod.rs` (the `AppInner { … }` literal and the lines just before it only)
- `crates/server/src/serve.rs`

**Prerequisite:** TL6.2 committed (`AnalyticsConfig`).

**Steps.**
1. In `analytics/mod.rs`, after the `mod` lines, add:
   ```rust
   use std::sync::atomic::{AtomicBool, Ordering};
   use std::time::Duration;

   use uuid::Uuid;

   use crate::analytics::actor::Actor;
   use crate::analytics::catalog::Event;
   use crate::analytics::counters::Counters;
   use crate::analytics::posthog::{FLUSH_INTERVAL, PosthogSink, SinkWorker, capture, group_identify};
   use crate::auth::limits::{Bucket, KeyedLimiter};
   use crate::config::ServerConfig;
   use crate::http::AppState;
   use crate::observability::switchboard::{Switch, for_config};

   /// Requests to `POST /api/v1/analytics/events` per user and minute; more are dropped.
   pub(crate) const EVENTS_PER_MINUTE: u32 = 60;
   /// Users the limiter tracks at most.
   const LIMITER_KEYS: usize = 100_000;

   /// The analytics state on `AppInner`.
   pub(crate) struct Analytics {
       /// Feature counters for the heartbeat's `usage`.
       pub(crate) counters: Counters,
       /// The heartbeat (and so the feature counts) runs: the switchboard's heartbeat decision.
       counting: AtomicBool,
       /// `None` unless `[analytics]` is set and restore mode is off.
       sink: Option<PosthogSink>,
       /// The per-user limit of the events endpoint.
       limiter: KeyedLimiter<Uuid>,
   }

   impl Analytics {
       /// The state for `config`, and the sink's worker for `serve` to spawn. Counting follows
       /// the switchboard's heartbeat decision for `config` (the same pure function `serve`
       /// calls).
       pub(crate) fn new(config: &ServerConfig) -> (Self, Option<SinkWorker>) {
           let counting = matches!(for_config(config).heartbeat, Switch::On);
           Self::with_flush_interval(config, FLUSH_INTERVAL, counting)
       }

       /// [`Analytics::new`] with another flush interval and an explicit counting flag (tests).
       pub(crate) fn with_flush_interval(
           config: &ServerConfig,
           flush_interval: Duration,
           counting: bool,
       ) -> (Self, Option<SinkWorker>) {
           let (sink, worker) = match config.analytics.posthog() {
               Some((host, key)) if !config.restore_mode => {
                   match PosthogSink::new(host, key.clone(), flush_interval) {
                       Ok((sink, worker)) => (Some(sink), Some(worker)),
                       Err(_) => {
                           tracing::warn!(
                               "analytics: the PostHog client could not be built; per-user product analytics are off"
                           );
                           (None, None)
                       }
                   }
               }
               _ => (None, None),
           };
           let limiter = KeyedLimiter::new(
               Bucket {
                   capacity: EVENTS_PER_MINUTE,
                   per: Duration::from_secs(60),
               },
               LIMITER_KEYS,
           );
           let analytics = Self {
               counters: Counters::default(),
               counting: AtomicBool::new(counting),
               sink,
               limiter,
           };
           (analytics, worker)
       }

       /// Whether per-user events go to PostHog.
       pub(crate) fn posthog_enabled(&self) -> bool {
           self.sink.is_some()
       }

       /// `analytics` in `GET /api/auth/config`: the UI sends its events when either sink runs,
       /// the PostHog sink or the heartbeat's feature counters.
       pub(crate) fn ui_events_wanted(&self) -> bool {
           self.counting.load(Ordering::Relaxed) || self.sink.is_some()
       }

       /// Takes one request of `user` from the events endpoint's limit.
       pub(crate) fn admit(&self, user: Uuid, now_ns: i64) -> bool {
           self.limiter.check(&user, now_ns).is_ok()
       }

       /// Turns counting on or off: a test build has the heartbeat off (debug build), so tests of
       /// the self-hosted path set it by hand.
       #[cfg(test)]
       pub(crate) fn set_counting(&self, on: bool) {
           self.counting.store(on, Ordering::Relaxed);
       }
   }

   /// Records one catalog event (design § 6): counts it, and when the PostHog sink runs, the
   /// actor has a user and the request did not opt out, queues it (after the org's
   /// `$groupidentify` when due). Never fails and never blocks.
   pub(crate) fn record(state: &AppState, event: &Event, actor: &Actor) {
       state.analytics.counters.increment(event.name());
       let Some(sink) = state.analytics.sink.as_ref() else {
           return;
       };
       let Some(user_id) = actor.user_id else {
           return;
       };
       if actor.opted_out {
           return;
       }
       let now = state.clock.now_ns();
       if let Some(org) = actor.org_id {
           let plan = state.entitlements.plan_at(org, now).name;
           if sink.group_due(org, &plan, now) {
               sink.enqueue(group_identify(org, &plan, now));
           }
       }
       sink.enqueue(capture(event, actor, user_id, now));
   }
   ```
   `Analytics` derives nothing (`KeyedLimiter` has no `Debug`; `AppInner` derives none either). If `crate::auth::limits` or `Bucket`'s fields are not visible from here, stop and report.
2. In `http/mod.rs`:
   - In `AppInner`, after `issue_wake` (and after any field M2 or M5 added after it), add:
     ```rust
     /// Product analytics: feature counters and the PostHog sink (track TL M6).
     pub(crate) analytics: crate::analytics::Analytics,
     ```
   - In `finish`, right after the line `.layer(middleware::from_fn_with_state(state.clone(), authenticate))`, add `.layer(middleware::from_fn(crate::analytics::actor::privacy_signal))`, with the comment `// DNT and Sec-GPC for the length of the request (D-TL-15).` on the line above. Keep any layer M2 added.
3. In `test_support/mod.rs`, right before `let state = AppState::new(AppInner {`, add:
   ```rust
   // Counting off, as in any debug build; tests turn it on with `Analytics::set_counting`.
   let (analytics, analytics_worker) =
       crate::analytics::Analytics::with_flush_interval(&config, Duration::from_millis(50), false);
   if let Some(worker) = analytics_worker {
       drop(tokio::spawn(worker.run()));
   }
   ```
   and `analytics,` as the last field of the literal. `Duration` is already imported there.
4. In `serve.rs` `bind`:
   1. Right before `let state = AppState::new(AppInner {`: `let (analytics, analytics_worker) = crate::analytics::Analytics::new(&config);`, and `analytics,` as the last field of the literal.
   2. Right after M4's block
      ```rust
      if let Some(task) = crate::observability::heartbeat::start(&state, decision).await {
          tasks.push(task);
      }
      ```
      (and after any block M5 put right after it), add the two blocks below, with the same shape (no `Box::pin`; the coordinator adds one if clippy's `large_futures` asks):
      ```rust
      // Feature counts for the heartbeat's `usage` (track TL M6), loaded and saved while it runs.
      if let Some(task) = crate::analytics::counters::start(&state, decision).await {
          tasks.push(task);
      }
      // Per-user product events to the operator's PostHog, when `[analytics]` is set (D-TL-14).
      if let Some(worker) = analytics_worker {
          tasks.push(tokio::spawn(worker.run()));
      }
      ```
   Shutdown aborts both with the other `tasks`; events still queued are dropped (at most 10 s of them).

**Tests** (in `analytics/mod.rs`'s `mod tests`; `use crate::analytics::test_receiver::Receiver;`; a helper `async fn posthog_app(receiver: &Receiver) -> TestApp` = `TestApp::with_config(|c| { c.analytics.posthog_host = Some(receiver.base.clone()); c.analytics.posthog_key = Some(Secret::new("phc_test".to_owned())); })`):
- `record_counts_without_a_sink` (`TestApp::new()`: `posthog_enabled()` false; `record` of `&Event::DatasetCreated` twice → `counters.get("dataset.created") == 2`).
- `ui_events_follow_either_sink` (`TestApp::new()`: `ui_events_wanted()` false; after `set_counting(true)`: true; `posthog_app`: true with counting off).
- `new_counts_only_when_the_heartbeat_runs` (`Analytics::new(&crate::test_support::config(dir))`: counting off in a test build, so `ui_events_wanted()` false; the switchboard rules themselves are M4's tests).
- `admit_limits_each_user` (`EVENTS_PER_MINUTE` calls of `admit(user, now)` are true, the next false; another user is still admitted; 61 s later the first is admitted again).
- `record_sends_a_session_event_with_its_group`: `Receiver::start(200)`, `posthog_app`, `seed()`; `Actor::project(&session_principal, &access)` with the seeded user (email `ada@example.com`), `ProjectAccess { project: seeded.project.clone(), role: Some(Role::Admin), implicit: false }`, then `opted_out = false`; `record(&app.state, &Event::DatasetCreated, &actor)`; `wait_for(1, 2 s)`: the batch holds a `$groupidentify` (`distinct_id` `org_<seeded org id>`, `plan` `Self-hosted`, the default `entitlements.defaults.plan_name`) then `dataset.created` with `distinct_id` = seeded user id (top level and in `properties`), `$groups.org` = org id, `project_id`, `via` `session`, `$ip` null, `$geoip_disable` true; the body text contains none of `ada@`, `example.com`, `Acme`, `acme`, `Support`, `support-bot`.
- `the_group_is_identified_once` (two `record` calls → one `$groupidentify`, two events).
- `an_opted_out_actor_is_counted_but_not_sent` (`opted_out = true`; counter 1; no request within 300 ms).
- `an_actor_without_a_user_is_counted_but_not_sent` (`Actor::principal(&Principal::ApiKey { … })`).
- `restore_mode_has_no_sink` (`[analytics]` set and `restore_mode = true` → `posthog_enabled()` false).
- `the_router_sets_the_privacy_signal`: `TestApp::with_routes(Router::new().route("/probe", get(|| async { actor::request_opted_out().to_string() })))`; a request with `DNT: 1` → body `true`; without → `false`.
In `serve.rs` tests: `serve_starts_without_analytics` (`bind(config(dir.path()), None)` starts and shuts down; the existing `restore_mode_skips_the_sending_and_dns_loops` keeps its count, since in a test build the heartbeat is off and `[analytics]` is unset, so neither new task starts).

**Commit:** `Record product analytics events and start their sinks`

---

### TL6.7 — The heartbeat's `usage` field

**Goal.** The heartbeat carries one bucket per catalog event, and an accepted heartbeat subtracts what it sent.

**Edit:**
- `crates/server/src/observability/heartbeat.rs`

**M4 names used** (see § Prerequisites for their lines): `Heartbeat` (15 fields, `features` last), `collect`, `Sender::tick`, the test items `KEYS`, `golden()`, `Receiver`, `sender_for`, `the_payload_matches_the_golden_json`, `the_key_set_is_fixed`, `a_fresh_instance_collects_zeros`, `the_payload_is_small`. M5 adds `features.crash_reports_on` in the same file and may land first: keep its field, its `FEATURE_KEYS` entry and its golden-JSON key everywhere below; this phase only adds. Check first; stop if a name differs.

**Steps.**
1. Imports: `use std::collections::BTreeMap;` and `use crate::analytics::counters::{self, Counts, UsageBucket, usage_buckets};`.
2. In `Heartbeat`, after `features`, add:
   ```rust
   /// Catalog events counted since the last accepted heartbeat, bucketed (track TL M6). Every
   /// catalog name is present; `0` when none.
   pub(crate) usage: BTreeMap<&'static str, UsageBucket>,
   ```
3. Right after the struct, add:
   ```rust
   impl Heartbeat {
       /// The heartbeat with `usage` from `counts`.
       pub(crate) fn with_usage(mut self, counts: &Counts) -> Self {
           self.usage = usage_buckets(counts);
           self
       }
   }
   ```
4. In `collect`'s `Heartbeat { … }` literal, add `usage: usage_buckets(&Counts::new()),` after `features`.
5. In `Sender::tick`: right after the `let payload = match collect(…) { … };` statement, add `let snapshot = state.analytics.counters.snapshot();` and `let payload = payload.with_usage(&snapshot);`. After the `if let Err(error) = recorded { … }` block that follows the `TELEMETRY_LAST_HEARTBEAT_AT` write, add `state.analytics.counters.subtract(&snapshot);` and `counters::save(state).await;`, before `tracing::debug!("telemetry: heartbeat sent")`. On any failure path nothing is subtracted.
6. In the test module: append `"usage"` to `const KEYS` (its length becomes 16), and give `golden()`'s literal (and any other `Heartbeat { … }` literal) the `usage` field as in Tests.

**Tests** (in `heartbeat.rs`'s `mod tests`):
- `the_payload_matches_the_golden_json`: `golden()`'s literal gains `usage: usage_buckets(&Counts::from([("dataset.created", 3), ("page.viewed", 250), ("sql_console.run", 12), ("trace.viewed", 1500)]))`; the expected JSON gains, after `"features": {…}`:
  ```json
    "usage": {
      "alert_rule.created": "0",
      "annotation_queue.created": "0",
      "annotation_queue.item_completed": "0",
      "api_key.created": "0",
      "assistant.panel_opened": "0",
      "assistant.turn_sent": "0",
      "backfill.started": "0",
      "channel.created": "0",
      "command_palette.opened": "0",
      "dashboard.created": "0",
      "dashboard.edited": "0",
      "dataset.created": "1-9",
      "dataset.exported": "0",
      "dataset.imported": "0",
      "dataset.items_added": "0",
      "deletion.requested": "0",
      "device.approved": "0",
      "evaluator.created": "0",
      "evaluator.run": "0",
      "experiment.created": "0",
      "invite.created": "0",
      "issue_source.created": "0",
      "list_layout.changed": "0",
      "live_tail.toggled": "0",
      "llm_connection.created": "0",
      "mcp.tool_called": "0",
      "online_rule.created": "0",
      "org.created": "0",
      "page.viewed": "100-999",
      "personal_token.created": "0",
      "playground.run": "0",
      "project.created": "0",
      "prompt.created": "0",
      "prompt.version_created": "0",
      "saved_view.created": "0",
      "scores.created": "0",
      "sql_console.run": "10-99",
      "theme.changed": "0",
      "trace.viewed": "1000+",
      "user.signed_in": "0"
    }
  ```
  (the comma after `features`' closing brace is added; indentation as the rest of the pretty JSON).
- `the_key_set_is_fixed`: 16 top-level keys, `usage` last; `usage`'s keys equal the `CATALOG` names; still no key contains `licen`.
- `a_fresh_instance_collects_zeros`: also every `usage` value is `UsageBucket::Zero`.
- `the_payload_is_small` still holds (under 4 096 bytes).
- `with_usage_buckets_the_counts` (a collected payload `with_usage` of `{"org.created": 1}` → `usage["org.created"] == UnderTen`, the other 39 `Zero`).
- `tick_sends_usage_and_subtracts_it` (`TestApp::new()`; three `record(&app.state, &Event::DatasetCreated, &Actor::user(Uuid::now_v7(), Via::Session))`; a `Receiver` answering 204; `tick` → body's `usage["dataset.created"] == "1-9"`; `counters.get("dataset.created") == 0`; `instance_settings['telemetry.usage_counts']` is `{}`).
- `tick_keeps_counts_on_failure` (receiver 503 → `counters.get` unchanged).
- `tick_waits_when_sent_recently` (M4) still sends nothing and subtracts nothing.

**Commit:** `Add feature usage buckets to the heartbeat`

---

### TL6.8 — `analytics` in `GET /api/auth/config`

**Goal.** The UI learns whether to send events.

**Edit:**
- `crates/server/src/api/account.rs`
- `crates/server/src/routes/auth.rs`
- `fixtures/api/static/auth-config.json`
- `crates/server/tests/snapshots/auth_config.snap`

**Steps.**
1. In `AuthConfig`, after `sso_discovery`, add:
   ```rust
   /// Product analytics are on: the UI sends its catalog events to
   /// `POST /api/v1/analytics/events` (track TL M6, D-TL-14). True when either sink runs: the
   /// PostHog sink (`[analytics]` set, not restore mode) or the heartbeat's feature counters.
   pub analytics: bool,
   ```
   In the test `account_types_snapshot`, add `analytics: false,` to the `AuthConfig` literal.
2. In `routes/auth.rs` `config`, add `analytics: state.analytics.ui_events_wanted(),` after `sso_discovery`.
3. In `fixtures/api/static/auth-config.json`, add `"analytics": false` as the last key (after `"sso_discovery": false`, which gains a comma). In `crates/server/tests/snapshots/auth_config.snap`, the same change in the JSON body.

**Tests** (in `routes/auth.rs`'s `mod tests`):
- `auth_config_says_analytics_is_off_in_a_test_build` (`TestApp::new()`: no `[analytics]`, counting off as in any debug build → `analytics == false`).
- `auth_config_says_analytics_is_on_with_posthog` (`TestApp::with_config` setting `analytics.posthog_host = Some("http://127.0.0.1:9".to_owned())` and a key → `true`).
- `auth_config_says_analytics_is_on_while_counting` (`TestApp::new()`, then `app.state.analytics.set_counting(true)` → `true`).
- `auth_config_says_analytics_is_off_in_restore_mode` (`[analytics]` set and `restore_mode = true`, counting off as the switchboard's restore rule gives → `false`).
- `account_types_snapshot` and `crates/server/tests/api_examples.rs`'s `roundtrip::<AuthConfig>("static/auth-config.json")` pass with the new field.

**Commit:** `Tell the UI whether product analytics are on`

---

### TL6.9 — `POST /api/v1/analytics/events`

**Goal.** The session-only endpoint that takes the UI's catalog events, with its API types, OpenAPI entry and request example.

**Edit:**
- `crates/server/src/api/analytics.rs` (new)
- `crates/server/src/api/mod.rs` (one line)
- `crates/server/src/routes/analytics.rs` (new)
- `crates/server/src/routes/mod.rs` (one line)
- `crates/server/src/http/access.rs` (one line in `routes`)
- `crates/server/src/api/openapi.rs`
- `fixtures/api/requests/analytics-events.json` (new)
- `crates/server/tests/api_examples.rs`

**Steps.**
1. Create `api/analytics.rs`. Module doc: `//! Product analytics events from the web UI (track TL M6, spec 09 § Product analytics).` Contents, in full:
   ```rust
   use std::fmt;

   use serde::{Deserialize, Serialize};
   use serde_json::{Map, Value};
   use utoipa::ToSchema;

   /// `POST /api/v1/analytics/events`. At most 50 events are read; the rest are dropped.
   #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
   pub struct AnalyticsEventsRequest {
       /// Catalog events of the web UI.
       pub events: Vec<AnalyticsEventInput>,
   }

   /// One web event. The server keeps it only when `name` is a web event of the catalog with
   /// valid properties, and the user can read `org` and `project`.
   #[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
   pub struct AnalyticsEventInput {
       /// A web event name of the catalog, e.g. `page.viewed`.
       pub name: String,
       /// The page's org, as in the URL (slug or id). Resolved by the server, never stored.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub org: Option<String>,
       /// The page's project, as in the URL (slug or id). Resolved by the server, never stored.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub project: Option<String>,
       /// The event's properties; unknown keys are dropped.
       #[serde(default, skip_serializing_if = "Map::is_empty")]
       #[schema(value_type = Object)]
       pub properties: Map<String, Value>,
   }

   /// Name and property count only: property values are not logged.
   impl fmt::Debug for AnalyticsEventInput {
       fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
           formatter
               .debug_struct("AnalyticsEventInput")
               .field("name", &self.name)
               .field("properties", &self.properties.len())
               .finish_non_exhaustive()
       }
   }
   ```
   In `api/mod.rs`, add `pub mod analytics;` right after `pub mod alerts;` (alphabetical order).
2. Create `routes/analytics.rs`. Module doc: `//! `POST /api/v1/analytics/events` (track TL M6, D-TL-14): the UI's catalog events. Session only; always 204 past authentication, whatever the body holds, so the UI never retries and a probe learns nothing.` Contents:
   ```rust
   use std::collections::HashMap;

   use axum::body::Body;
   use axum::extract::{Extension, State};
   use axum::http::StatusCode;
   use axum::routing::post;
   use cetra_core::ids::{OrgId, ProjectId};
   use serde::Deserialize;
   use serde_json::Value;

   use crate::analytics::actor::{Actor, request_opted_out};
   use crate::analytics::catalog::parse_web;
   use crate::analytics::record;
   use crate::api::analytics::AnalyticsEventInput;
   use crate::auth::principal::{AuthUser, Principal};
   use crate::auth::roles::{OrgRole, ProjectPerm};
   use crate::demo::org_role_for;
   use crate::http::AppState;
   use crate::http::access::{Access, RouteDef, user_project};
   use crate::meta::orgs::{org_by_id, org_by_slug};

   /// Events read per request; later ones are dropped.
   pub(crate) const MAX_EVENTS: usize = 50;
   /// Largest body read; a larger one is dropped whole.
   pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;
   /// Longest `org` or `project` value looked up.
   const MAX_KEY_BYTES: usize = 256;
   /// Distinct `(org, project)` pairs resolved per request; events of further pairs are dropped.
   const MAX_CONTEXTS: usize = 5;

   /// The analytics route.
   pub(crate) fn routes() -> Vec<RouteDef> {
       vec![RouteDef {
           method: axum::http::Method::POST,
           path: "/api/v1/analytics/events",
           access: Access::User { session_only: true },
           documented: true,
           handler: post(post_events),
       }]
   }

   /// The body, read loosely so one bad event never spoils the others.
   #[derive(Deserialize)]
   struct RawBatch {
       #[serde(default)]
       events: Vec<Value>,
   }

   /// What an event's `org` and `project` resolved to.
   #[derive(Debug, Clone, Copy, Default)]
   struct Context {
       org_id: Option<OrgId>,
       org_role: Option<OrgRole>,
       project_id: Option<ProjectId>,
   }
   ```
3. `async fn post_events(State(state): State<AppState>, Extension(principal): Extension<Principal>, body: Body) -> StatusCode`. Doc: "Records each valid web event for the session's user; answers 204 whatever happens." Steps, in order:
   1. `let Principal::Session { user, .. } = &principal else { return StatusCode::NO_CONTENT; };` (the access layer already refused other principals).
   2. `if !state.analytics.admit(user.id, state.clock.now_ns()) { return StatusCode::NO_CONTENT; }` (over the per-user limit: the whole request is dropped, the body is not read).
   3. `let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else { return StatusCode::NO_CONTENT; };` (a larger body is dropped whole; the router's 1 MiB limit applies to extractors only).
   4. `let Ok(raw) = serde_json::from_slice::<RawBatch>(&bytes) else { return StatusCode::NO_CONTENT; };`
   5. `let opted_out = request_opted_out();`, `let resolving = state.analytics.posthog_enabled();` and `let mut contexts: HashMap<(Option<String>, Option<String>), Option<Context>> = HashMap::new();` (the per-request cache of resolutions).
   6. For each `value` in `raw.events.into_iter().take(MAX_EVENTS)`:
      1. `let Ok(input) = serde_json::from_value::<AnalyticsEventInput>(value) else { continue };`
      2. `let Some(event) = parse_web(&input.name, &input.properties) else { continue };`
      3. The context: when `resolving` is false, `Context::default()` (the counters need no ids, so nothing is looked up). Otherwise look up `(input.org.clone(), input.project.clone())` in `contexts`; when absent and `contexts.len() >= MAX_CONTEXTS`, skip the event (`continue`); when absent otherwise, compute `resolve(&state, user, input.org.as_deref(), input.project.as_deref()).await` and insert it. `let Some(context) = <that> else { continue };`
      4. `record(&state, &event, &Actor { user_id: Some(user.id), via: None, org_id: context.org_id, org_role: context.org_role, project_id: context.project_id, opted_out });`
   7. `StatusCode::NO_CONTENT`.
   Nothing is logged per event (no names, no values).
4. `async fn resolve(state: &AppState, user: &AuthUser, org: Option<&str>, project: Option<&str>) -> Option<Context>`. Doc: "`None`: drop the event (unknown, inactive, or not readable by the user, or a lookup failed)." Steps:
   1. If `org` or `project` is longer than `MAX_KEY_BYTES` bytes: `None`.
   2. `(None, None)` → `Some(Context::default())`.
   3. `(_, Some(project))`: `let key = match org { Some(org) => format!("{org}/{project}"), None => project.to_owned() };` `let access = user_project(state, user, &key, ProjectPerm::View).await.ok()?;` `let found = org_role_for(state, access.project.org_id, user).await.ok()??;` → `Some(Context { org_id: Some(access.project.org_id), org_role: Some(found.role), project_id: Some(access.project.id) })`.
   4. `(Some(org), None)`: `let pool = state.meta.reader();` `let row = match org.parse::<OrgId>() { Ok(id) => org_by_id(pool, id).await.ok()?, Err(_) => org_by_slug(pool, org).await.ok()? }?;` if `row.state != "active"`: `None`; `let found = org_role_for(state, row.id, user).await.ok()??;` → `Some(Context { org_id: Some(row.id), org_role: Some(found.role), project_id: None })`.
5. In `routes/mod.rs`, add `pub(crate) mod analytics;` after `pub(crate) mod alert_rules;`. In `http/access.rs` `routes`, add `routes.extend(crate::routes::analytics::routes());` right after `routes.extend(crate::routes::me::routes());`.
6. In `api/openapi.rs`:
   1. `use super::analytics::{AnalyticsEventInput, AnalyticsEventsRequest};` with the other `use super::…` lines (alphabetical).
   2. In `collect_schemas`, after `set.insert::<AuthConfig>();`, add `set.insert::<AnalyticsEventsRequest>();` and `set.insert::<AnalyticsEventInput>();`.
   3. In `openapi()`'s tags, after the `"auth"` tag, add `tag("analytics", "Product analytics events from the web UI (session only).")`.
   4. Add, after `fn admin_paths`:
      ```rust
      fn analytics_paths(paths: PathsBuilder) -> PathsBuilder {
          let events = operation("post_analytics_events", "analytics", "Send product analytics events")
              .description(Some(
                  "Session only. Web events of the catalog (`page.viewed` with a route template, `command_palette.opened`, `live_tail.toggled`, `list_layout.changed`, `theme.changed`, `assistant.panel_opened`). At most 50 events and 64 KiB are read, and 60 requests a minute per user. Unknown names and properties and invalid values are dropped; when a PostHog sink runs, events of orgs or projects the user cannot read are dropped too. The user comes from the session. `DNT: 1` or `Sec-GPC: 1` keeps the events out of PostHog; they still count toward the anonymous feature counts. Always 204 once signed in.",
              ))
              .securities(Some(vec![SecurityRequirement::new("session", [] as [&str; 0])]))
              .request_body(Some(json_body("AnalyticsEventsRequest")))
              .responses(responses(
                  "204",
                  ResponseBuilder::new().description("Accepted, whatever the body held.").build(),
                  &[401, 403],
              ))
              .build();
          paths.path(
              "/api/v1/analytics/events",
              PathItem::new(HttpMethod::Post, events),
          )
      }
      ```
   5. In `paths()`, add `let paths = analytics_paths(paths);` right after `let paths = admin_paths(paths);`.
7. Create `fixtures/api/requests/analytics-events.json` with exactly (two-space indentation, final newline):
   ```json
   {
     "events": [
       {
         "name": "page.viewed",
         "org": "acme",
         "project": "support-bot",
         "properties": {
           "route": "/$org/$project/traces/"
         }
       },
       {
         "name": "live_tail.toggled",
         "org": "acme",
         "project": "support-bot",
         "properties": {
           "on": true
         }
       },
       {
         "name": "command_palette.opened"
       }
     ]
   }
   ```
8. In `crates/server/tests/api_examples.rs`: import `cetra_server::api::analytics::AnalyticsEventsRequest;` (keep the import list sorted); in `request_examples_match_their_types_and_the_openapi_document`, add `roundtrip::<AnalyticsEventsRequest>("requests/analytics-events.json");` with the other `roundtrip` lines, and `("post_analytics_events", "requests/analytics-events.json"),` to the `requests` array.

**Tests** (in `routes/analytics.rs`'s `mod tests`; `Receiver` from `crate::analytics::test_receiver`; helper `posthog_app(&receiver)` as in TL6.6; `seed()` gives `ada@example.com`, org `acme` (`Acme`), project `support-bot` (`Support`); requests carry `.header("cookie", &seeded.cookie)`; `page(route, org, project)` builds one event value; every test that checks a PostHog body or a dropped context uses `posthog_app`, the others `TestApp::new()`; `EVENTS_PER_MINUTE` is `crate::analytics::EVENTS_PER_MINUTE`):
- `without_posthog_events_are_counted_without_lookups` (`TestApp::new()`, `seed()`: an event with `org: "nope"`, `project: "missing"` → counted (`counters.get("page.viewed") == 1`), since nothing is resolved without a sink).
- `a_page_view_reaches_posthog_by_template`: one `page.viewed` with `org: "acme"`, `project: "support-bot"`, route `/$org/$project/traces/` → 204; `counters.get("page.viewed") == 1`; the PostHog body has `page.viewed` with `properties.route == "/$org/$project/traces/"`, `project_id` = seeded project id, `$groups.org` = seeded org id, `$set.org_role == "owner"`, no `via`.
- `planted_text_never_reaches_posthog` (privacy): one request with (a) `page.viewed` whose route is `/acme/support-bot/traces` (dropped), (b) `page.viewed` with the template plus `"email": "ada@example.com"`, `"note": "Planted free text"`, (c) name `custom.ada@example.com`, (d) `theme.changed` with `theme: "Planted"` (dropped); → 204; one PostHog event (b); the whole body text contains none of `ada@`, `example.com`, `Planted`, `acme`, `Acme`, `support-bot`, `Support`, `/acme/`.
- `server_event_names_are_refused` (`dataset.created`, `org.created` → nothing counted).
- `dnt_and_gpc_skip_posthog_but_count` (`DNT: 1` → 204, counter +1, no PostHog request within 300 ms; same with `Sec-GPC: 1`).
- `at_most_50_events_are_read` (60 valid `command_palette.opened` → counter 50).
- `a_foreign_org_is_dropped` (with `posthog_app`; `second_org(&app, "other", "bob@example.com")`; an event with `org: "other"` from ada (not a member) → counter 0; ada's own org → 1).
- `an_unknown_project_is_dropped` (with `posthog_app`; `project: "nope"` → counter 0).
- `at_most_five_contexts_per_request` (with `posthog_app`; 7 events whose `org` values are 7 distinct strings, the first being `acme` → the first event counted, the 6th and 7th dropped without lookup: counter 1).
- `the_per_user_limit_drops_requests` (`EVENTS_PER_MINUTE` requests of one valid event each → counter `EVENTS_PER_MINUTE`; the next request → 204, counter unchanged).
- `an_org_only_event_resolves_the_org` (`org: "acme"`, no project → `$groups.org` set, no `project_id`).
- `a_user_level_event_has_no_group` (no org, no project → no `$groups`).
- `bad_bodies_answer_204` (`not json`, `{}`, `{"events": 5}`, `{"events": [1, "x", null]}`, a 70 KiB body of valid events → each 204, counter 0).
- `sessions_only` (no cookie → 401; a PAT from `app.personal_token(seeded.user_id, true)` as bearer → 403).
- The access suite (`http::access_suite`) covers the new route without change; `crates/server/tests/api_examples.rs` checks the example against the type and the OpenAPI schema once the coordinator has run `just openapi`.

**Commit:** `Add the product analytics events endpoint`

---

### TL6.10 — Server events: accounts, keys, evaluation, MCP

**Goal.** The handlers of this group, and MCP tool calls, record their catalog events after success.

**Edit:**
- `crates/server/src/auth/session.rs`
- `crates/server/src/routes/orgs.rs`, `projects.rs`, `invites.rs`, `api_keys.rs`, `tokens.rs`, `device.rs`, `connections.rs`, `evaluators.rs`, `eval_jobs.rs`, `online_rules.rs`, `backfills.rs` (all under `crates/server/src/routes/`)
- `crates/server/src/mcp/mod.rs`

**Rules for every handler of TL6.10–TL6.12.**
- Import what you use: `use crate::analytics::actor::Actor;` (and `Via` where used), `use crate::analytics::catalog::Event;` (and `ItemSource`, `SignInMethod` where used), `use crate::analytics::record;`, and `Principal` from `crate::auth::principal` when you add it.
- `record` takes references: `record(&state, &Event::X, &Actor::…(…));`.
- A property taken from the request body or query is copied into a local at the top of the function, before the body is moved (`let kind = body.kind;`); every such type is `Copy`.
- **Where the line goes** (the table's "Where" column says which case applies):
  - **final**: right before the function's last expression, its success `Ok(…)`.
  - **arm `X`**: the function ends with a `match`; put the line inside the arm `X`, right before that arm's `Ok(…)`.
  - **bind**: the function ends with a call that returns the whole `Result` (e.g. `write_batch(…).await`); replace that tail expression `f(…).await` by `let value = f(…).await?;`, then the `record` line, then `Ok(value)`. The return type does not change.
  - **sql**: see TL6.11 row 13.
- When a handler lacks `Extension(principal): Extension<Principal>`, add it as the parameter right after `State(state): State<AppState>`.
- M2 adds spans to some of these handlers (the SQL console, MCP tool calls) and may have landed first: keep its lines; the `record` line goes where this plan says, inside or after its span code as the anchors give.
- **Tests:** the table names, per handler, the existing test in that file's `mod tests` that drives the endpoint to success. At its end (after its last assertion), add `assert!(app.state.analytics.counters.get("<event name>") >= 1, "<event name> not counted");`, using that test's `TestApp` variable (usually `app`). If the named test does not exist or does not reach the handler's success path, stop and report. No other test changes.

**Steps:**

| # | File | Function | Before (top of the function) | Where | `record` line | Test to extend |
|---|---|---|---|---|---|---|
| 1 | `auth/session.rs` | `start_session` | `let method = match origin { SessionOrigin::Password => SignInMethod::Password, SessionOrigin::Provider => SignInMethod::Provider, SessionOrigin::OrgProvider(_) => SignInMethod::OrgProvider };` and `let sso_org = match origin { SessionOrigin::OrgProvider(org) => Some(org), _ => None };` (`SessionOrigin` is `Copy`) | final (`Ok(set_cookie(…))`) | `record(state, &Event::UserSignedIn { method }, &Actor { org_id: sso_org, ..Actor::user(user_id, Via::Session) });` | `start_sets_the_cookie_and_stores_only_the_hash` (event `user.signed_in`) |
| 2 | `routes/orgs.rs` | `create_org` | — | final | `record(&state, &Event::OrgCreated, &Actor::principal(&principal));` | `ada_creates_orgs_and_lists_only_hers` |
| 3 | `routes/projects.rs` | `create_project` | add the `Principal` extension | final | `record(&state, &Event::ProjectCreated, &Actor::org(&principal, &access));` | `create_reserves_settings_and_takes_null_retention` |
| 4 | `routes/invites.rs` | `create_invite` | `let role = body.role;` | final | `record(&state, &Event::InviteCreated { role }, &Actor::org(&principal, &access));` | `invite_stores_the_hash_and_replaces_the_previous` |
| 5 | `routes/api_keys.rs` | `create_api_key` | `let kind = body.kind;` | final | `record(&state, &Event::ApiKeyCreated { kind }, &Actor::project(&principal, &access));` | `last_used_at_is_written_on_flush_and_the_secret_is_not_stored` |
| 6 | `routes/tokens.rs` | `create_token` | `let write = body.scopes.contains(&PersonalTokenScope::Write);` (import `crate::api::keys::PersonalTokenScope` if missing) | final | `record(&state, &Event::PersonalTokenCreated { write }, &Actor::principal(&principal));` | `create_lists_the_hash_and_not_the_secret` |
| 7 | `routes/device.rs` | `approve` | — | final (`Ok(StatusCode::NO_CONTENT)`) | `record(&state, &Event::DeviceApproved, &Actor::principal(&principal));` | `the_login_flow_mints_a_read_write_token_and_the_grant_is_collected_once` |
| 8 | `routes/connections.rs` | `create_llm_connection` | `let adapter = body.adapter;` | final | `record(&state, &Event::LlmConnectionCreated { adapter }, &Actor::org(&principal, &access));` | `owner_creates_and_reads_back_without_the_key` |
| 9 | `routes/evaluators.rs` | `create_evaluator` | `let kind = body.kind;` | arm `Outcome::Updated(row)` | `record(&state, &Event::EvaluatorCreated { kind }, &Actor::project(&principal, &access));` | `judge_round_trip` |
| 10 | `routes/eval_jobs.rs` | `run_evaluator` | — | final | `record(&state, &Event::EvaluatorRun, &Actor::project(&principal, &access));` | `run_queues_a_manual_job` |
| 11 | `routes/online_rules.rs` | `create_online_rule` | `let target = body.target;` | arm `RuleWrite::Done { row, .. }` | `record(&state, &Event::OnlineRuleCreated { target }, &Actor::project(&principal, &access));` | `create_get_list_round_trip` |
| 12 | `routes/backfills.rs` | `create_backfill` | — | arm `Create::Created(row)` | `record(&state, &Event::BackfillStarted, &Actor::project(&principal, &access));` | `create_is_audited` |

13. `mcp/mod.rs`:
    1. Add a field to `McpServer`: `/// The request opted out of per-user analytics (captured here: rmcp runs tools in spawned tasks, outside the request's scope).` `opted_out: bool,`. In `mcp`, the literal as M2's TL2.9 leaves it, `let server = McpServer { state, user, request_span: tracing::Span::current() };`, gains one field at its end: `let server = McpServer { state, user, request_span: tracing::Span::current(), opted_out: crate::analytics::actor::request_opted_out() };`. Keep `request_span` and its doc comment. (It is the only literal; if another exists, or `request_span` is missing because TL2.9 is not in, stop and report.)
    2. Add, after `impl McpServer { … }` (outside it, before the test module), in full:
       ```rust
       /// The catalog events of one finished tool call: `mcp.tool_called`, plus the event of the
       /// same action in the UI or API for the tools that have one (`query_sql` is a SQL console
       /// run, `get_trace` a trace read), so those count whichever client ran them.
       fn tool_events(tool: &'static str, ok: bool) -> Vec<Event> {
           let mut events = vec![Event::McpToolCalled { tool, ok }];
           if ok && tool == sql::QUERY_SQL {
               events.push(Event::SqlConsoleRun);
           }
           if ok && tool == traces::GET_TRACE {
               events.push(Event::TraceViewed);
           }
           events
       }
       ```
    3. In `call_tool` as TL2.9 leaves it: right after M2's line `span.record(spans::TOOL, tool);` (which follows the `tokio::select!`), insert `let ok = outcome.is_ok();`. Leave M2's `span` lines in the select and in the `match outcome` arms as they are; after the `let result = match outcome { … };` block, add:
       ```rust
       let actor = Actor {
           opted_out: self.opted_out,
           ..Actor::user(self.user.id, Via::Pat)
       };
       for event in tool_events(tool, ok) {
           record(&self.state, &event, &actor);
       }
       ```
       The cancelled branch records nothing. Keep any span code M2 added around the tool call.

**Tests.**
- The 12 table rows, as the rules say.
- `mcp/mod.rs`: `list_projects_lists_visible_projects` gains `assert!(app.state.analytics.counters.get("mcp.tool_called") >= 1);`; the existing test at line ≈1004 that calls `call(&app, &token, "query_sql", &select_one)` successfully (the one in the test module whose body has that call; report its name) gains `assert!(app.state.analytics.counters.get("sql_console.run") >= 1);`; new pure test `tool_events_add_sql_and_trace_reads` (`tool_events(sql::QUERY_SQL, true)` → `[McpToolCalled { tool: "query_sql", ok: true }, SqlConsoleRun]`; `tool_events(traces::GET_TRACE, true)` → ends with `TraceViewed`; `tool_events(sql::QUERY_SQL, false)` → only `McpToolCalled`; `tool_events(tools::LIST_PROJECTS, true)` → only `McpToolCalled`); `unknown_tool_is_a_protocol_error` gains `assert_eq!(app.state.analytics.counters.get("mcp.tool_called"), 0);` (if its `TestApp` variable is not reachable there, report and skip that one line).

**Commit:** `Record account, key and evaluation events`

---

### TL6.11 — Server events: datasets, prompts, playground, queries

**Goal.** The handlers of this group record their catalog event after success.

**Edit:** `crates/server/src/routes/datasets.rs`, `issue_dataset_items.rs`, `experiments.rs`, `prompts.rs`, `playground.rs`, `traces.rs`, `sql.rs`, `saved_views.rs`.

**Rules:** as in TL6.10.

**Steps:**

| # | File | Function | Before | Where | `record` line | Test to extend |
|---|---|---|---|---|---|---|
| 1 | `datasets.rs` | `create_dataset` | — | final | `record(&state, &Event::DatasetCreated, &Actor::project(&principal, &access));` | `create_get_list_delete` |
| 2 | `datasets.rs` | `add_items` | — | final | `record(&state, &Event::DatasetItemsAdded { source: ItemSource::Manual }, &Actor::project(&principal, &access));` | `personal_tokens_write_items` |
| 3 | `datasets.rs` | `add_from_trace` | — | final | the same with `ItemSource::Trace` | `from_trace_copies_the_root` |
| 4 | `datasets.rs` | `add_from_traces` | — | final | the same with `ItemSource::Traces` | `bulk_add_copies_what_it_can_and_lists_the_rest` |
| 5 | `datasets.rs` | `import_items` | `let format = params.format;` | final | `record(&state, &Event::DatasetImported { format }, &Actor::project(&principal, &access));` | `import_csv_makes_one_version` |
| 6 | `datasets.rs` | `export_items` | add the `Principal` extension; use the existing `let format = params.format.unwrap_or(ExportFormat::Jsonl);` | final | `record(&state, &Event::DatasetExported { format }, &Actor::project(&principal, &access));` | `export_csv` |
| 7 | `issue_dataset_items.rs` | `add_issue_examples` | — | final | `record(&state, &Event::DatasetItemsAdded { source: ItemSource::Issue }, &Actor::project(&principal, &access));` | `adds_the_newest_distinct_traces` |
| 8 | `experiments.rs` | `create_experiment` | — | final | `record(&state, &Event::ExperimentCreated, &Actor::project(&principal, &access));` | `create_pins_the_version_and_returns_items` |
| 9 | `prompts.rs` | `create_prompt` | `let kind = body.kind;` | final | `record(&state, &Event::PromptCreated { kind }, &Actor::project(&principal, &access));` | `create_get_and_list` |
| 10 | `prompts.rs` | `create_version` | — | final | `record(&state, &Event::PromptVersionCreated, &Actor::project(&principal, &access));` | `versions_and_resolve` |
| 11 | `playground.rs` | `run_playground` | — | final (`Ok(sse_response(rx))`) | `record(&state, &Event::PlaygroundRun, &Actor::project(&principal, &access));` | `streams_a_run` |
| 12 | `traces.rs` | `get_trace` | add the `Principal` extension | final (`Ok(error::Json(TraceDetail { … }))`) | `record(&state, &Event::TraceViewed, &Actor::project(&principal, &access));` | `trace_detail_builds_the_tree` |
| 13 | `sql.rs` | `run_sql` | add the `Principal` extension | sql (below) | `record(&state, &Event::SqlConsoleRun, &Actor::project(&principal, &access));` | `example_request_returns_the_example_result` |
| 14 | `saved_views.rs` | `create_view` | `let kind = body.kind;` | final | `record(&state, &Event::SavedViewCreated { kind }, &Actor::project(&principal, &access));` | `create_list_update_delete_round_trip` |

Row 13 (`run_sql` as M2's TL2.9 leaves it; it returns from two places; both edits are single-line insertions): (a) in the `let Some(export) = format.export() else { … };` block, right after `log_end(access.project.id, "json", query_len, started, &result);` and before `return result.map(…)`, insert `if result.is_ok() { <record line> }`; (b) in the export `match start_export(…).instrument(span.clone()).await`, inside the arm
   ```rust
   Ok(response) => {
       span.record(spans::OUTCOME, "ok");
       Ok(response)
   }
   ```
   insert the `<record line>` between `span.record(spans::OUTCOME, "ok");` and `Ok(response)`. Change no line of M2's. If `run_sql` does not have this shape (TL2.9 not in), stop and report.

**Tests:** the 14 table rows, as the rules say.

**Commit:** `Record dataset, prompt and query events`

---

### TL6.12 — Server events: dashboards, annotation, scores, alerts, issues, deletions, assistant

**Goal.** The handlers of this group record their catalog event after success.

**Edit:** `crates/server/src/routes/dashboards.rs`, `annotation_queues.rs`, `scores.rs`, `alert_rules.rs`, `channels.rs`, `issues.rs`, `deletions.rs`, `assistant.rs`.

**Rules:** as in TL6.10.

**Steps:**

| # | File | Function | Before | Where | `record` line | Test to extend |
|---|---|---|---|---|---|---|
| 1 | `dashboards.rs` | `create_dashboard` | — | final | `record(&state, &Event::DashboardCreated, &Actor::project(&principal, &access));` | `create_get_patch_delete_round_trip` (event `dashboard.created`) |
| 2 | `dashboards.rs` | `update_dashboard` | — | arm `UpdateOutcome::Updated(row)` | `record(&state, &Event::DashboardEdited, &Actor::project(&principal, &access));` | `create_get_patch_delete_round_trip` (also `dashboard.edited`) |
| 3 | `dashboards.rs` | `save_overview` | — | arm `OverviewSave::Saved { row, created }` | the same as row 2 | `save_with_the_read_version_updates_the_overview` |
| 4 | `annotation_queues.rs` | `create_queue` | `let target = body.target;` | arm `QueueCreate::Created(row)` | `record(&state, &Event::AnnotationQueueCreated { target }, &Actor::project(&principal, &access));` | `create_list_get_update_delete` |
| 5 | `annotation_queues.rs` | `complete_item` | — | bind (`finish(&state, &principal, &access, path, store::Finish::Complete).await`) | `record(&state, &Event::AnnotationQueueItemCompleted, &Actor::project(&principal, &access));` | `complete_skip_reopen` |
| 6 | `scores.rs` | `create_scores` | — | bind (`write_batch(&state, &access, writer, kind, batch).await`) | `record(&state, &Event::ScoresCreated, &Actor::project(&principal, &access));` | `session_write_is_stored_as_human_and_read_key_is_refused` |
| 7 | `alert_rules.rs` | `create_rule` | — | final | `record(&state, &Event::AlertRuleCreated, &Actor::project(&principal, &access));` | `create_fills_defaults` |
| 8 | `channels.rs` | `create_channel` | `let kind = body.kind;` | final | `record(&state, &Event::ChannelCreated { kind }, &Actor::project(&principal, &access));` | `create_and_list_each_kind` |
| 9 | `issues.rs` | `create_issue_source` | `let kind = body.kind;` | arm `SourceWrite::Done(row)` | `record(&state, &Event::IssueSourceCreated { kind }, &Actor::project(&principal, &access));` | `create_list_get_update_delete_round_trip` |
| 10 | `deletions.rs` | `create_deletion` | — | final | `record(&state, &Event::DeletionRequested, &Actor::project(&principal, &access));` | `trace_ids_deletion_is_accepted_and_runs` |
| 11 | `assistant.rs` | `post_turn` | — | final (`Ok(sse_response(sse_rx))`) | `record(&state, &Event::AssistantTurnSent, &Actor::project(&principal, &access));` | `streams_a_turn` |

**Tests:** the 11 table rows, as the rules say (rows 1 and 2 assert two counters in the same test).

**Commit:** `Record dashboard, annotation, alert and issue events`

---

### TL6.13 — The web analytics client

**Goal.** A small client that, when the server says analytics is on, queues catalog events with the page's org and project, records page views by route id, and posts batches to the server.

**Edit:**
- `web/src/lib/analytics.ts` (new)
- `web/src/lib/analytics.test.ts` (new)
- `web/src/lib/analytics-install.ts` (new)
- `web/src/lib/analytics-install.test.tsx` (new)
- `web/src/lib/analytics-catalog.test.ts` (new)
- `web/src/main.tsx`
- `web/src/mocks/handlers.ts`

**Steps.**
1. Create `web/src/lib/analytics.ts` (the core: no import of the API client or the router, so any module may import `track` without a cycle), in full:
   ```ts
   /**
    * Product analytics (track TL M6, D-TL-14). Catalog events go to this server only
    * (`POST /api/v1/analytics/events`), never to a third party; nothing is sent until
    * `startAnalytics` runs, which `installAnalytics` does only when `GET /api/auth/config` says
    * `analytics: true`. The catalog is `fixtures/analytics/catalog.json`.
    */

   /** The web events of the catalog, with their properties. */
   export type WebEvent =
     | { name: "page.viewed"; properties: { route: string } }
     | { name: "command_palette.opened" }
     | { name: "live_tail.toggled"; properties: { on: boolean } }
     | { name: "list_layout.changed"; properties: { layout: "drawer" | "bottom" | "page" } }
     | { name: "theme.changed"; properties: { theme: "light" | "dark" | "system" } }
     | { name: "assistant.panel_opened" };

   /** The property keys of each web event, sorted (checked against the golden catalog). */
   export const WEB_EVENT_PROPERTIES: Record<WebEvent["name"], readonly string[]> = {
     "assistant.panel_opened": [],
     "command_palette.opened": [],
     "list_layout.changed": ["layout"],
     "live_tail.toggled": ["on"],
     "page.viewed": ["route"],
     "theme.changed": ["theme"],
   };

   /** One event as sent: `org` and `project` are the page's route parameters. */
   export interface WireEvent {
     name: WebEvent["name"];
     org?: string;
     project?: string;
     properties?: Record<string, string | boolean>;
   }

   /** The page an event happened on. */
   export interface AnalyticsContext {
     org?: string;
     project?: string;
   }

   export type Sender = (events: WireEvent[]) => Promise<void>;

   /** Events per request; the server reads at most 50. A full batch is sent at once, so no more
    * than 50 events ever wait. */
   export const MAX_BATCH = 50;
   /** A batch goes out at least this often while events wait. */
   export const FLUSH_MS = 5_000;

   export const ANALYTICS_PATH = "/api/v1/analytics/events";

   interface Active {
     queue: WireEvent[];
     send: Sender;
     context: () => AnalyticsContext;
     timer: ReturnType<typeof setInterval>;
     onVisibility: () => void;
   }

   let active: Active | undefined;

   /**
    * Posts a batch to this server with the session cookie. Plain `fetch`, not the API client:
    * a 401 here must not send the user to the login page (`isSessionApiPath`). `keepalive`
    * lets the last batch leave while the page unloads.
    */
   export const postEvents: Sender = async (events) => {
     await globalThis.fetch(new URL(ANALYTICS_PATH, window.location.origin).toString(), {
       method: "POST",
       credentials: "same-origin",
       headers: { "Content-Type": "application/json" },
       body: JSON.stringify({ events }),
       keepalive: true,
     });
   };

   /** Queues `event` with the current page's org and project. Does nothing while stopped. */
   export function track(event: WebEvent): void {
     if (!active) {
       return;
     }
     const { org, project } = active.context();
     active.queue.push({
       name: event.name,
       ...(org === undefined ? {} : { org }),
       ...(project === undefined ? {} : { project }),
       ...("properties" in event ? { properties: event.properties } : {}),
     });
     if (active.queue.length >= MAX_BATCH) {
       flushAnalytics();
     }
   }

   /** Sends up to `MAX_BATCH` queued events. A failed send is dropped (no retry). */
   export function flushAnalytics(): void {
     if (!active || active.queue.length === 0) {
       return;
     }
     const batch = active.queue.splice(0, MAX_BATCH);
     void active.send(batch).catch(() => undefined);
   }

   /**
    * Starts sending: a batch every `flushMs` and when the page is hidden. Returns `stop`, which
    * sends everything still queued and stops tracking; a `stop` of a start that a later start
    * replaced does nothing. A second start replaces the first.
    */
   export function startAnalytics(options: {
     send?: Sender;
     context?: () => AnalyticsContext;
     flushMs?: number;
   }): () => void {
     stopActive();
     const onVisibility = () => {
       if (document.visibilityState === "hidden") {
         flushAnalytics();
       }
     };
     const instance: Active = {
       queue: [],
       send: options.send ?? postEvents,
       context: options.context ?? (() => ({})),
       timer: setInterval(flushAnalytics, options.flushMs ?? FLUSH_MS),
       onVisibility,
     };
     active = instance;
     document.addEventListener("visibilitychange", onVisibility);
     return () => {
       if (active === instance) {
         stopActive();
       }
     };
   }

   function stopActive(): void {
     if (!active) {
       return;
     }
     while (active.queue.length > 0) {
       flushAnalytics();
     }
     clearInterval(active.timer);
     document.removeEventListener("visibilitychange", active.onVisibility);
     active = undefined;
   }
   ```
   (`stopActive` is a function declaration, so calling it above its definition is fine.)
2. Create `web/src/lib/analytics-install.ts`, in full:
   ```ts
   import type { QueryClient } from "@tanstack/react-query";
   import { authConfigQuery } from "@/api/auth";
   import { type AnalyticsContext, type Sender, startAnalytics, track } from "@/lib/analytics";
   import { isPublicPath } from "@/lib/redirect";
   import type { AppRouter } from "@/router";

   /** The leaf route's id (`/$org/$project/traces/`): the page template, never the URL. */
   export function pageRoute(router: AppRouter): string | undefined {
     const leaf = router.state.matches.at(-1);
     if (!leaf || leaf.routeId === "__root__" || isPublicPath(router.state.location.pathname)) {
       return undefined;
     }
     return leaf.routeId;
   }

   /** The leaf route's `org` and `project` parameters, as in the URL. */
   export function pageContext(router: AppRouter): AnalyticsContext {
     const params = (router.state.matches.at(-1)?.params ?? {}) as Record<string, unknown>;
     return {
       ...(typeof params.org === "string" ? { org: params.org } : {}),
       ...(typeof params.project === "string" ? { project: params.project } : {}),
     };
   }

   /**
    * Turns analytics on when the server says so: a page view now (if the router has resolved a
    * page) and on every path change. Returns `uninstall`. Does nothing when `analytics` is off
    * or the config cannot be read.
    */
   export async function installAnalytics(
     router: AppRouter,
     queryClient: QueryClient,
     options: { send?: Sender; flushMs?: number } = {},
   ): Promise<() => void> {
     let enabled = false;
     try {
       enabled = (await queryClient.fetchQuery(authConfigQuery)).analytics;
     } catch {
       return () => {};
     }
     if (!enabled) {
       return () => {};
     }
     const stop = startAnalytics({ ...options, context: () => pageContext(router) });
     const viewed = () => {
       const route = pageRoute(router);
       if (route !== undefined) {
         track({ name: "page.viewed", properties: { route } });
       }
     };
     const unsubscribe = router.subscribe("onResolved", (event) => {
       if (event.fromLocation?.pathname === event.toLocation.pathname) {
         return;
       }
       viewed();
     });
     if (router.state.resolvedLocation !== undefined) {
       viewed();
     }
     return () => {
       unsubscribe();
       stop();
     };
   }
   ```
   `analytics` exists on the generated `AuthConfig` type once the coordinator has run `just openapi` (after TL6.8); until then `tsc` fails on this line, which is expected.
3. `web/src/main.tsx`: import `installAnalytics` from `@/lib/analytics-install`; right after `const router = createAppRouter({ queryClient });`, add `void installAnalytics(router, queryClient);`.
4. `web/src/mocks/handlers.ts`: add to the `handlers` array, right after `facetsFixture,`, `analyticsEvents,` and define above the array:
   ```ts
   /** The UI's product analytics batches (track TL M6): accepted and dropped, as a self-hosted server without `[analytics]` would. */
   const analyticsEvents = http.post("/api/v1/analytics/events", () => new HttpResponse(null, { status: 204 }));
   ```

**Tests.**
- `web/src/lib/analytics.test.ts` (each test calls the returned `stop` in a `finally` or `afterEach`):
  - `track does nothing before start` (spy `send`; `track` a palette event; after 50 ms the spy was not called).
  - `sends queued events after the interval` (`startAnalytics({ send, flushMs: 10 })`; two events → `vi.waitFor`: one call with both, in order).
  - `adds the context's org and project` (`context: () => ({ org: "acme", project: "support-bot" })` → the wire events carry them; an empty context sends neither key).
  - `sends at most 50 events per batch` (`flushMs: 60_000`; track 120 → a call of 50 at the 50th event and another at the 100th; `stop()` → a third call of 20; 120 in total, in order).
  - `a replaced start's stop does nothing` (`const stopA = startAnalytics({ send: a })`; `const stopB = startAnalytics({ send: b })`; `stopA()`; `track` one event; `stopB()` → `b` got it, `a` got nothing).
  - `a hidden page flushes` (`flushMs: 60_000`; track one; set `document.visibilityState` to `hidden` with `Object.defineProperty(document, "visibilityState", { configurable: true, value: "hidden" })`, dispatch `visibilitychange` → one call; restore the property).
  - `stop stops tracking` (after `stop()`, `track` sends nothing).
  - `a failed send is ignored` (`send` rejects; no unhandled rejection; later batches still go).
  - `postEvents posts JSON to the events endpoint` (`server.use(http.post("/api/v1/analytics/events", async ({ request }) => { captured = { type: request.headers.get("content-type"), body: await request.json() }; return new HttpResponse(null, { status: 204 }); }))`; `await postEvents([{ name: "command_palette.opened" }])` → `type` starts with `application/json`, `body` equals `{ events: [{ name: "command_palette.opened" }] }`).
- `web/src/lib/analytics-install.test.tsx` (uses `renderApp` and `server.use`; `authConfig` from `fixtures/api/static/auth-config.json`):
  - `installs nothing when analytics is off` (`renderApp("/acme/support-bot/traces")`, `installAnalytics(router, queryClient, { send, flushMs: 10 })`, navigate to `/acme/support-bot/sessions`; after 100 ms `send` was not called).
  - `sends page views by route template` (`server.use(http.get("/api/auth/config", () => HttpResponse.json({ ...authConfig, analytics: true })))` before `renderApp("/acme/support-bot/traces")`; install with a spy; `vi.waitFor`: an event `{ name: "page.viewed", org: "acme", project: "support-bot", properties: { route: "/$org/$project/traces/" } }`; `router.navigate({ to: "/$org/$project/sessions", params: { org: "acme", project: "support-bot" } })` → a second page view with route `/$org/$project/sessions/`; no sent `route` contains `acme` or `support-bot`).
  - `a search change is not a page view` (navigate to the same path with another search → no third `page.viewed`).
  - `public pages send no page view` (analytics on; `renderApp("/login")`; install; after 100 ms no `page.viewed`).
  - `the config failing leaves analytics off` (`server.use(http.get("/api/auth/config", () => HttpResponse.json({}, { status: 500 })))` → `installAnalytics` resolves; nothing sent).
- `web/src/lib/analytics-catalog.test.ts` (imports `golden from "../../../fixtures/analytics/catalog.json"`):
  - `web events match the golden catalog` (`golden.events.filter((e) => e.source === "web")` mapped to `[name, properties]` equals `Object.entries(WEB_EVENT_PROPERTIES)` sorted by name).
  - `page routes match the router` (`createAppRouter({ queryClient: new QueryClient(), history: createMemoryHistory() })`; the leaf ids are `Object.values(router.routesById)` filtered to `route.id !== "__root__"` and no children (`const children = (route as { children?: unknown }).children; children === undefined || (Array.isArray(children) ? children.length === 0 : Object.keys(children as object).length === 0)`), minus the six public ids `/login`, `/signup`, `/reset`, `/verify`, `/setup/`, `/invite/$token` (listed in the test), sorted; equal `golden.page_routes`). If this fails on the coordinator's run because TanStack spells ids differently than `PAGE_ROUTES`, the coordinator corrects `PAGE_ROUTES` and the golden file from the test's diff.

**Commit:** `Add the web product analytics client`

---

### TL6.14 — The UI-only events

**Goal.** The five UI actions that only the browser sees call `track`.

**Edit:**
- `web/src/components/command/palette.tsx`, `web/src/components/command/palette.test.tsx`
- `web/src/components/live-toggle.tsx`, `web/src/components/live-toggle.test.tsx` (new)
- `web/src/components/theme-toggle.tsx`, `web/src/components/theme-toggle.test.tsx`
- `web/src/lib/list-layout.ts`, `web/src/lib/list-layout.test.ts`
- `web/src/lib/assistant/panel.ts`, `web/src/lib/assistant/panel.test.ts`

**Steps.** In each source file, `import { track } from "@/lib/analytics";`.
1. `palette.tsx`, in `CommandPalette`, right after the line `const [palette, setPalette] = useState(false);` (line ≈51; `palette` is also a name in the theme code of this file, so use this exact line), add:
   ```tsx
   useEffect(() => {
     if (palette) {
       track({ name: "command_palette.opened" });
     }
   }, [palette]);
   ```
2. `live-toggle.tsx`: the `Switch`'s `onCheckedChange={onPressedChange}` becomes
   ```tsx
   onCheckedChange={(on) => {
     track({ name: "live_tail.toggled", properties: { on } });
     onPressedChange(on);
   }}
   ```
3. `theme-toggle.tsx`, in `ThemeMenuItems`, the theme group's `onValueChange` body becomes:
   ```tsx
   track({ name: "theme.changed", properties: { theme: value as Theme } });
   setTheme(value as Theme);
   ```
4. `list-layout.ts`, `setListLayout`: first line `const changed = next !== getListLayout();`; after `emit();` add `if (changed) { track({ name: "list_layout.changed", properties: { layout: next } }); }`.
5. `assistant/panel.ts`, `setPanelOpen`: first line `const opening = open && !isOpen();`; after `emit();` add `if (opening) { track({ name: "assistant.panel_opened" }); }`.

**Tests** (each starts analytics with `const stop = startAnalytics({ send, flushMs: 10 })` where `send` pushes into an array, and calls `stop()` at the end; nothing reaches the network):
- `palette.test.tsx`: `opening the palette is tracked once` (as the first test: `renderApp("/acme/support-bot/traces")`, open with `openPalette`; `vi.waitFor` → exactly one sent event, named `command_palette.opened`; `startAnalytics` has no context here, so assert the name only).
- `live-toggle.test.tsx` (new): `flipping the switch is tracked` (render `<LiveToggle pressed={false} disabled={false} status="closed" onPressedChange={spy} />`; click the `Live` switch → `spy` called with `true` and one `live_tail.toggled` with `{ on: true }`).
- `theme-toggle.test.tsx`: `picking a theme is tracked` (as `selects Dark from the menu` → one `theme.changed` with `{ theme: "dark" }`).
- `list-layout.test.ts`: `a layout change is tracked once` (`setListLayout("bottom")` twice → one `list_layout.changed` with `{ layout: "bottom" }`).
- `panel.test.ts`: `opening the panel is tracked` (`setPanelOpen(true)` twice, then `setPanelOpen(false)` → one `assistant.panel_opened`); `togglePanel` from closed → one event.
- Every existing test of these files still passes: without `startAnalytics`, `track` does nothing.

**Commit:** `Track the UI-only product events`

---

### TL6.15 — Docs, specs, CHANGELOG and the request to Q

**Goal.** The Telemetry page says what product analytics send, specs 04/09/11/12 and the CHANGELOG describe M6, and TL-Q4 holds the details Q needs.

**Edit:**
- `website/src/content/docs/docs/self-hosting/telemetry.md`
- `CHANGELOG.md`
- `docs/spec/04-metadata-db.md`
- `docs/spec/09-api.md`
- `docs/spec/11-ui.md`
- `docs/spec/12-operations.md`
- `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/tl-requests-for-q.md` (outside the repo, not committed)

**Steps.**
1. `telemetry.md` (M4's page; if a heading quoted here is missing, stop and report):
   1. In `## What it sends`, add a last table row: `| \`usage\` | an object, one key per event below, each \`0\`, \`1-9\`, \`10-99\`, \`100-999\` or \`1000+\` | How often each feature was used since the last heartbeat, see [Feature usage](#feature-usage). |`. In the example JSON, add after the `features` object the `usage` object of TL6.7's golden test (same 40 keys and values).
   2. After `## What it sends` (and after any section M5 added there), add:
      ````markdown
      ## Feature usage

      cetra counts how often each product event happens, whoever does it: the web UI, the CLI, an SDK, an agent over MCP, or the API. The heartbeat's `usage` field sends each count since the last heartbeat as a bucket: `0`, `1-9`, `10-99`, `100-999` or `1000+`. It never says who did what, in which organization or project, in what order, or when. cetra keeps the counts in `meta.db` while the heartbeat is on, saved once an hour, and clears what the relay accepted. When the heartbeat is off, nothing is saved or sent.

      While the heartbeat is on, the web UI reports its own events (marked "UI" below) to your server, through `POST /api/v1/analytics/events` on its own origin, only so that they are counted: your server keeps no per-user record of them and sends nothing about them except the buckets. The UI loads no analytics script and sets no cookie. When the heartbeat is off and [product analytics](#product-analytics) are not set up, the UI reports nothing.

      | Event | Counted when |
      |---|---|
      | `alert_rule.created` | an alert rule is created |
      | `annotation_queue.created` | an annotation queue is created |
      | `annotation_queue.item_completed` | a reviewer completes a queue item |
      | `api_key.created` | a project API key is created |
      | `assistant.panel_opened` | UI: the assistant panel opens |
      | `assistant.turn_sent` | a message is sent to the assistant |
      | `backfill.started` | an online-rule backfill starts |
      | `channel.created` | a notification channel is created |
      | `command_palette.opened` | UI: the command palette opens |
      | `dashboard.created` | a dashboard is created |
      | `dashboard.edited` | a dashboard or the overview is saved |
      | `dataset.created` | a dataset is created |
      | `dataset.exported` | a dataset export is downloaded |
      | `dataset.imported` | a file is imported into a dataset |
      | `dataset.items_added` | items are added to a dataset |
      | `deletion.requested` | a GDPR deletion is requested |
      | `device.approved` | a device sign-in (`cetra login`, `npx @cetra-ai/cli init`) is approved |
      | `evaluator.created` | an evaluator is created |
      | `evaluator.run` | an evaluator run starts |
      | `experiment.created` | an experiment is created |
      | `invite.created` | an organization invite is created |
      | `issue_source.created` | an issue source is created |
      | `list_layout.changed` | UI: the list layout changes |
      | `live_tail.toggled` | UI: a list's Live switch is flipped |
      | `llm_connection.created` | an LLM connection is created |
      | `mcp.tool_called` | an MCP tool call ends (`query_sql` also counts as `sql_console.run`, `get_trace` as `trace.viewed`) |
      | `online_rule.created` | an online rule is created |
      | `org.created` | an organization is created |
      | `page.viewed` | UI: a page is shown |
      | `personal_token.created` | a personal access token is created |
      | `playground.run` | a playground run starts |
      | `project.created` | a project is created |
      | `prompt.created` | a prompt is created |
      | `prompt.version_created` | a prompt version is created |
      | `saved_view.created` | a view or SQL query is saved |
      | `scores.created` | scores are written to a project |
      | `sql_console.run` | a SQL query succeeds, in the SQL console, the API or the MCP tool `query_sql` |
      | `theme.changed` | UI: a theme is picked in the theme menu |
      | `trace.viewed` | a trace's detail is loaded, by the UI (including when hovering a link preloads it), the API or the MCP tool `get_trace` |
      | `user.signed_in` | someone signs in to the web UI |

      New events are only ever added to this list; an event keeps its name.

      ## Product analytics

      A self-hosted cetra sends us no per-user events: only the bucketed counts above, inside the heartbeat. Events are counted whether a person or a program did them, so an action made in the UI, the CLI, an SDK or over MCP counts once.

      cetra can send per-user product events to a PostHog project, set with [`[analytics]`](/docs/self-hosting/configuration-reference/#analytics). It is off unless you set it, and restore mode turns it off. cetra's own cloud sets it to a PostHog project of ours in the EU. When it is on:

      - The web UI tells the server which page it shows and the UI events above, through `POST /api/v1/analytics/events` on its own origin. The UI loads no analytics script, sets no cookie, and records no clicks or screens: only the events of the list above.
      - The server sends each event to PostHog with the user's id (a random UUID), the organization's id, the project's id for project pages, and the event's own values (such as the page's route template, `/$org/$project/traces/`, never its address). It adds the user's role in the organization and the organization's plan.
      - It never sends emails, names, organization or project names, addresses of pages, IP addresses, filters, queries, prompts or any trace data, and it asks PostHog not to geolocate events.
      - A browser that sends `DNT: 1` (Do Not Track) or `Sec-GPC: 1` (Global Privacy Control) keeps its events out of PostHog; they still count toward the feature counts.
      - Events made with an API key, which has no user, are only counted.
      ````
2. `CHANGELOG.md`, append to `## [Unreleased]` › `### Added`:
   ```markdown
   - The heartbeat also sends how often each product feature was used since the last one, as buckets (`usage`); the Telemetry page lists every event. Product analytics to a PostHog project of your own can be turned on with `[analytics]` (`posthog_host`, `posthog_key`): the server forwards product events with user and organization ids only, and honours `DNT` and `Sec-GPC`. While the heartbeat or `[analytics]` is on, the web UI reports pages and a few UI actions to its own server. `GET /api/auth/config` gains `analytics`; `POST /api/v1/analytics/events` is new.
   ```
3. `docs/spec/04-metadata-db.md`, in the `instance_settings` block, after M4's `'telemetry.last_heartbeat_at'` line:
   ```sql
   -- 'telemetry.usage_counts': JSON object of catalog event name to count, not yet sent in a heartbeat; saved hourly while the heartbeat runs (track TL M6)
   ```
4. `docs/spec/09-api.md`:
   1. In § Auth, the `GET /config` row: append `; \`analytics\` says the UI sends its product events: true when the PostHog sink runs (\`[analytics]\` set, not restore mode) or the heartbeat's feature counters run (track TL M6)`.
   2. After § Pricing & GDPR (before `## Response shapes`), add:
      ```markdown
      ### Product analytics

      | Method & path | Notes |
      |---|---|
      | `POST /api/v1/analytics/events` | session only; `{events: [{name, org?, project?, properties?}]}`, the web events of the catalog (`fixtures/analytics/catalog.json`); at most 50 events and 64 KiB read; at most 60 requests a minute per user; unknown names and properties and invalid values (a `page.viewed` route must be a template of the catalog) are dropped; `org` and `project` are the page's route parameters: when the PostHog sink runs they are resolved with the page's access checks (at most 5 distinct pairs per request), an event the user cannot read is dropped, and they are never stored; without the sink nothing is looked up; the user comes from the session; `DNT: 1` or `Sec-GPC: 1` keeps the events out of PostHog; always `204` once signed in (D-TL-14, D-TL-15). API handlers and MCP tools record the server events of the catalog themselves, so an action counts once whichever client made it (MCP's `query_sql` and `get_trace` count as `sql_console.run` and `trace.viewed` too) |
      ```
5. `docs/spec/11-ui.md`, before `## Quality bar`, add:
   ```markdown
   ## Product analytics

   `lib/analytics.ts` queues the web events of the catalog (`fixtures/analytics/catalog.json`) and `lib/analytics-install.ts` turns it on at startup only when `GET /api/auth/config` says `analytics: true`, which is when the PostHog sink or the heartbeat's feature counters run (track TL M6, D-TL-14). It then sends `page.viewed` on every path change with the leaf route's id as `route` (`/$org/$project/traces/`, never the URL; public pages are skipped), plus `command_palette.opened`, `live_tail.toggled`, `list_layout.changed`, `theme.changed` and `assistant.panel_opened`. Each event carries the leaf route's `org` and `project` parameters. Batches of at most 50 go to `POST /api/v1/analytics/events` every 5 s and when the page is hidden, with a plain same-origin `fetch` (`keepalive`), so a 401 never redirects to the login page; a full batch is sent at once. No third-party script, no cookie, no autocapture.
   ```
6. `docs/spec/12-operations.md`:
   1. In the configuration table, right after M4's two `telemetry.*` rows (and any M5 row), add:
      ```markdown
      | `analytics.posthog_host` | | unset. PostHog ingestion host (`https`, or `http` to a loopback IP; no user info, query or fragment); batches go to `<host>/batch/`; set with `posthog_key` (track TL M6, D-TL-14) |
      | `analytics.posthog_key` | | unset; secret. PostHog project API key; set with `posthog_host` |
      ```
   2. At the end of § Self-monitoring (after M4's "Telemetry to us" subsection and any M5 text), add:
      ```markdown
      ### Product analytics (track TL M6)

      One typed catalog (`crates/server/src/analytics/catalog.rs`, golden `fixtures/analytics/catalog.json`): 6 web events sent by the UI to `POST /api/v1/analytics/events` (60 requests a minute per user), 34 server events recorded by API handlers and MCP tools with `analytics::record`. The UI sends when `GET /api/auth/config` says `analytics`, true when either sink runs. Every event increments a per-name counter; while the heartbeat runs the counters are loaded at startup, saved hourly to `instance_settings['telemetry.usage_counts']`, sent in the heartbeat's `usage` field as buckets (`0`, `1-9`, `10-99`, `100-999`, `1000+`, every catalog name present) and reduced by what an accepted heartbeat sent. When `[analytics]` is set and restore mode is off, events with a user and without `DNT: 1`/`Sec-GPC: 1` (read by a middleware into a task-local; outside a request: opted out) also go to PostHog: a bounded queue of 10 000 (new events dropped when full), batches of 100 or every 10 s to `<posthog_host>/batch/`, no retry, failures at `debug` at most once a minute, its own HTTP client (no redirects, 10 s timeout). A capture carries `distinct_id` = the user's UUID, `$groups.org` = the org id, `project_id`, `via` (server events), the catalog properties, `$set.org_role` when the role is known, `$ip: null`, `$geoip_disable: true`, `$process_person_profile: true`; `$groupidentify` (with `distinct_id` `org_<org id>`, no person profile) sets the group's `plan` at most once an hour per org. Org and project ids of web events are resolved only when the sink runs. Shutdown drops queued events.
      ```
7. Q notes file: in `## TL-Q1 — the relay at \`telemetry.cetra.sh\``, append to its bullet list:
   ```markdown
   - `usage` (from M6): an object whose keys are the catalog event names (`fixtures/analytics/catalog.json` in the repo, append-only; 40 today, every one present) and whose values are exactly `0`, `1-9`, `10-99`, `100-999` or `1000+`. Accept unknown keys (new events) with those values; a 400 would keep the counts from ever being cleared.
   ```
   and add a row to its table of accepted strings: `| \`usage.*\` | \`0\`, \`1-9\`, \`10-99\`, \`100-999\`, \`1000+\` |`. Then, in `## TL-Q4 — PostHog (for M6)`, append:
   ```markdown

   Details from M6's plan (`docs/plans/tl-telemetry/06-m6-product-analytics.md`):
   - cetra posts `{"api_key", "batch": [{"event", "distinct_id", "timestamp", "properties"}]}` to `<analytics.posthog_host>/batch/` (e.g. `https://eu.i.posthog.com/batch/`) with `User-Agent: cetra/<version>`, every 10 s or 100 events.
   - `distinct_id` is the user's UUID, at the event's top level and again in `properties`; `properties` also hold the catalog properties, `project_id`, `via`, `$groups: {"org": <org id>}`, `$set: {"org_role": …}` when known, `$ip: null`, `$geoip_disable: true`, `$process_person_profile: true`. `$groupidentify` (`distinct_id: "org_<org id>"`, `$group_type: "org"`, `$group_key: <org id>`, `$group_set: {"plan": <plan name>}`, `$process_person_profile: false`) at most hourly per org.
   - Before M6 ships to the cloud, verify with one real batch against the PostHog EU project that: events land on the person of the user's UUID; the `org` group gets its `plan` and no stray person `org_<id>` appears; no location is recorded (`$geoip_disable`); `$set.org_role` updates the person. Report any field to change to track TL.
   - In the PostHog project: create the group type `org` (first group type), turn on "Discard client IP data" (the requests come from the cells, but this is the second guard), and leave autocapture and session replay off (the UI loads no PostHog script).
   - The event names and their properties: `fixtures/analytics/catalog.json` (append-only). Events of a browser sending `DNT: 1` or `Sec-GPC: 1` never arrive.
   ```

**Tests.** By hand: `git diff` shows only the six repo files; the Telemetry page's description is unchanged; no new `Cetra`/`CETRA` word outside code, no `simply`/`powerful`/`seamless`. The coordinator's `just check` runs the docs checks (terms, env vars, links: `#feature-usage`, `#product-analytics`, `configuration-reference/#analytics`) and TL6.1's `the_telemetry_page_lists_every_event`.

**Commit:** `Document product analytics` (the Q notes file is not committed).

---

## After the last phase (coordinator)

1. After TL6.8 and TL6.9 are committed: `just openapi`; review the diff of `crates/server/openapi.json` (the `analytics` property of `AuthConfig`, the two new schemas, the new path and tag) and `web/src/api/schema.gen.ts`; commit both with `Regenerate the OpenAPI document for product analytics`.
2. M3: add `analytics.posthog_host` (allow-listed) and `analytics.posthog_key` (secret) to the debug bundle's key classification if M3's list exists by then; tell M3's owner otherwise. Hand M3 `Heartbeat::with_usage` (its preview may show `usage` from `state.analytics.counters.snapshot()`).
3. Review each phase's diff against this plan (TL6.10–TL6.12: every `record` line sits in the success path the table names, and the named tests carry the assertions; shared files keep M2/M3/M5's lines), then `just fmt` and `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check` once (TL6.1's `the_telemetry_page_lists_every_event` needs TL6.15).
4. A Sonnet code review of the milestone; fixes by a Haiku agent with an exact brief.
5. Update the TL row of `docs/roadmap.md`; hand the Q notes file to track Q (TL-Q4 includes the real-batch check).

## Interface for later work

- `crate::analytics::record(state: &AppState, event: &catalog::Event, actor: &actor::Actor)`; `Actor::{principal, project, org, user}`; `Via`; `Analytics::{posthog_enabled, ui_events_wanted, admit}`.
- `crate::analytics::catalog::{Event, CATALOG, PAGE_ROUTES, parse_web, static_name}`: a new event is a new variant, a `CATALOG` row (sorted), a golden-file row, a Telemetry page row, and, for a web event, a `WebEvent` member and a `WEB_EVENT_PROPERTIES` row. A new signed-in UI page adds its route id to `PAGE_ROUTES` and the golden file.
- `crate::analytics::counters::{Counters, Counts, UsageBucket, usage_buckets, save}`; `Heartbeat::with_usage`.
- Web: `track(event)` from `@/lib/analytics`.
