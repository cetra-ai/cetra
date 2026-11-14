# P · M3 — Units, limits and usage

Milestone 3 of track P ([`00-design.md`](00-design.md)). It builds on M2 ([`02-m2-org-entitlements.md`](02-m2-org-entitlements.md), merged first): M2 gives every org a resolved `Entitlements` with nine limits; M3 enforces the volume and count limits and shows usage.

When M3 is done:
- each org's units this UTC month (spans + scores, D173, D-P-16) are counted in memory, loaded from `usage_daily` on first use;
- at `units_cap`, OTLP HTTP ingest, score writes and public feedback answer `402 quota_exceeded` with `Retry-After` = seconds to the reset; OTLP gRPC answers `RESOURCE_EXHAUSTED` (D-P-17, D-P-37);
- spans over the org's ingest rate answer `429 rate_limited` (gRPC `UNAVAILABLE`) (D-P-19, D-P-35);
- invites, role changes, invite acceptance and project creation respect `max_seats` and `max_projects` (`403 plan_limit`, D-P-20);
- org owners get one email and owners and admins see a banner when the month reaches 80 % and 100 % of `units_included` (D-P-18, D-P-38);
- `GET /api/v1/orgs/{org}/usage` returns daily usage; the entitlements response carries the month's units; the Plan page shows a month bar and a daily chart (D-P-42);
- the control plane reads usage (`GET /api/internal/v1/usage`, scope `usage`), revokes, disables or deletes a user by its OIDC subject (scope `users`), and reads memberships (scope `members`) (D-P-24, D-P-39, D-P-40);
- the Python and TypeScript SDKs pause exporting until the reset after a 402 (D-P-34).

**Read first, in this order:**
1. `AGENTS.md`.
2. [`00-design.md`](00-design.md) § Decisions D-P-13–D-P-20, D-P-24, D-P-28, D-P-34–D-P-43; § Interfaces for track Q.
3. `docs/spec/02-ingestion.md` (responses, limits table, pipeline); `docs/spec/04-metadata-db.md` § Metering; `docs/spec/09-api.md`; `docs/spec/12-operations.md` § Licence keys, plans and entitlements.
4. M2's code: `crates/server/src/entitlements/` (`EntitlementsState::resolve`, `plan_at`, `Limits`), `routes/entitlements.rs`, `routes/internal_orgs.rs`, `auth/principal.rs` (`Principal::ControlPlane`), `http/access.rs` (`Access::ControlPlane`), `test_support` (`org_token`, `control_plane_token`).
5. The code named in each sub-phase before you change it.

## Conventions

The same as M2 (`02-m2-org-entitlements.md` § Conventions), including "Dead code between phases" (here P13 removes every attribute M3 added) and "Generated files". Server code returns `ApiError`; never log or return payload, tokens, emails or names; time from `state.clock.now_ns()`; months and days are UTC; tests on `TestApp` (manual clock `2026-01-01T00:00:00Z`; advance it as M2's tests do); web tests on MSW with `server.use`; SDK tests follow each SDK's existing test style (`pytest` in `sdks/python/tests`, Vitest next to `sdks/typescript/src/*.ts`).

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

`just check` before every commit (over 20 minutes). Quick loops: `cargo test -p llmobs-server <filter>`, `just lint`, `just check-web`, `just check-py`, `just check-ts`.

## Sub-phases

| # | Subject | What | After |
|---|---|---|---|
| P1 | `Count units per org and month` | `Usage` month counters, flush lock, `month_units` | M2 |
| P2 | `Cap monthly units with 402` | `quota_exceeded`, OTLP HTTP/gRPC, `scores::write` | P1 |
| P3 | `Rate-limit ingest per org` | `entitlements::rate`, 429 / `UNAVAILABLE` | P2 |
| P4 | `Enforce seats and projects` | `plan_limit`, invites, roles, accept, signup, projects; web states | P3 |
| P5 | `Add the usage API` | `GET /api/v1/orgs/{org}/usage`; month usage in the entitlements response | P4 |
| P6 | `Send usage notices` | `usage_notices`, the task, the email, the banner | P5 |
| P7 | `Export usage to the control plane` | `GET /api/internal/v1/usage` | P6 |
| P8 | `Let the control plane manage users` | `POST /api/internal/v1/users/{revoke,disable,delete}` | P7 |
| P9 | `Let the control plane read memberships` | lookup and org members | P8 |
| P10 | `Show usage on the Plan page` | month bar, daily chart | P9 |
| P11 | `Pause the Python exporter on 402` | `sdks/python` | — |
| P12 | `Pause the TypeScript exporter on 402` | `sdks/typescript` | — |
| P13 | `Close out M3` | dead-code attributes, specs check, roadmap | all |

P1–P10 run in order (they edit the same server files and the generated contract). P11 and P12 touch only their SDK and depend only on the HTTP contract (402 with `Retry-After`), so each can run in parallel with P1–P10, in its own worktree. P13 runs last.

---

## P1 — Month units

### `crates/server/src/usage.rs`

`Usage` keeps today's per-day counts (D-G-11) and gains per-org month counters:

```rust
/// Units (spans + scores) of one org in one UTC month (D-P-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonthUnits {
    /// `year * 12 + month0` of the month counted.
    month: i32,
    /// Units since 00:00 UTC on the 1st, flushed or not.
    units: u64,
}

pub(crate) struct Usage {
    counts: Mutex<HashMap<UsageKey, UsageCounts>>,
    /// Loaded month counters. An org is absent until its first `month_units` call.
    months: Mutex<HashMap<OrgId, MonthUnits>>,
    /// Held by `flush` from taking the counts to the end of the write, and by
    /// `month_units` while it loads, so a load never misses counts in flight.
    flush_lock: tokio::sync::Mutex<()>,
}
```

(`Usage` derives `Default` today; keep `Default` working: `tokio::sync::Mutex<()>` implements it.)

- `fn month_key(now_ns: i64) -> i32`: from `DateTime::from_timestamp_nanos(now_ns)`: `year * 12 + (month - 1)`.
- `add(...)`: after updating `counts`, when `spans + scores > 0` and the org has a loaded `MonthUnits` for `month_key(now_ns)`, add `spans + scores` (saturating). A loaded counter of an older month is left alone (the next `month_units` reloads it).
- `flush`: take `self.flush_lock.lock().await` for the whole body.
- New:

```rust
/// The org's units this UTC month at `now_ns` (D-P-16): `usage_daily` from the 1st plus
/// counts not flushed yet. Loads once per org and month, then follows `add`.
///
/// # Errors
/// [`MetaError`] when the load query fails.
pub(crate) async fn month_units(&self, meta: &MetaDb, org_id: OrgId, now_ns: i64) -> Result<u64, MetaError>
```

  Fast path: a loaded counter for this month → its `units`. Slow path: take `flush_lock`; check again (another task may have loaded); `SELECT COALESCE(SUM(spans + scores), 0) FROM usage_daily WHERE org_id = ? AND day >= ?` with the month's first day `YYYY-MM-01` (and `day < next month's first day`); add the unflushed `counts` of this org whose `day` is in the month; store `MonthUnits`; release. `SUM` is `i64`; convert with `u64::try_from(..).unwrap_or(0)` for a negative (never written) value.

Judge-trace spans are already excluded from `usage_daily.spans` by track I (D-I-31) when its code lands; M3 counts what `add` receives.

### Tests (`usage::tests`)

1. `month_units_loads_and_follows_adds`: write two `usage_daily` rows for the org this month (spans 10, scores 5; spans 1) and one last month (spans 100) → `month_units` = 16; `add` 3 spans and 2 scores → 21; another org's adds do not count.
2. `unflushed_counts_are_included_at_load`: `add` 4 spans (not flushed), then the first `month_units` → 4; `flush`, then `month_units` → still 4.
3. `a_new_month_starts_at_zero`: load in January (units 7), advance the clock to 2026-02-01T00:00:00Z → `month_units` reloads → 0 plus February rows.
4. `concurrent_flush_and_load_lose_nothing`: spawn a `flush` and a `month_units` for the same org many times with adds in between (a loop of 100 with `tokio::join!`); the final `month_units` equals the total added.

### Done

`just check`.

---

## P2 — The monthly cap

### Error

`ProblemCode::QuotaExceeded` (`quota_exceeded`, status 402, reason `"Payment Required"`; doc `/// The org reached its monthly units cap (D-P-17).`). `ApiError::QuotaExceeded { detail: String, retry_after_s: u64 }`: the problem with the detail, plus the header `Retry-After: <retry_after_s>` (follow how `ApiError::RateLimited` adds its header in `http/error.rs`). Status tests as for other codes. `api/openapi.rs` `problem(402)`: `"The organization reached its monthly units cap (code quota_exceeded). Retry-After gives the seconds until the cap resets."`.

### The check (`crates/server/src/entitlements/units.rs`, new)

```rust
//! The monthly units cap (D-P-17).

/// `Err(ApiError::QuotaExceeded)` when the org's units this month are at or above its
/// `units_cap`. No cap: always `Ok`.
pub(crate) async fn check_cap(state: &AppState, org: OrgId) -> Result<(), ApiError>
```

- `let now = state.clock.now_ns(); let cap = state.entitlements.resolve(org, now).limits.units_cap;` `None` → `Ok(())`.
- `units = state.usage.month_units(&state.meta, org, now).await?`; `units < cap` → `Ok`.
- Otherwise: `reset` = 00:00 UTC on the 1st of the next month; `retry_after_s = max(1, (reset − now) / 1e9)`; detail `format!("This organization reached its monthly limit of {cap} units. Ingest resumes on {reset_date} (UTC).", reset_date = "YYYY-MM-DD")`.
- A `MetaError` while loading fails open: log a `warn` with the org id and return `Ok(())` (ingest is never refused because a count could not be read).

### Where it runs

- **OTLP HTTP** (`routes/otlp.rs`, `export_body`): first thing, before `body_format` and before reading the body: `check_cap(state, project.org_id).await` — map its `ApiError` into the function's error type the way other `ApiError`s reach the response there (if `export_body` returns `IngestError`, check in `export` before calling `export_body` instead, logging the refusal like `log_refusal` does with `reason = "quota"`).
- **OTLP gRPC** (`ingest/grpc.rs`): after the key and project resolve, before `reserve_exact`: on `Err(ApiError::QuotaExceeded { detail, .. })` return `tonic::Status::resource_exhausted(detail)` (no `RetryInfo`).
- **Scores** (`scores::write`): at the top, `entitlements::units::check_cap(state, project.org_id).await?`. This covers `POST …/scores`, the public feedback endpoint and evaluator scores (track I's jobs then fail with this error; reported to I). Score deletion does not call `write`'s check: if deletion goes through `write`, call `check_cap` only when at least one item is not a deletion.

### Tests

- `routes::otlp::tests::capped_org_gets_402`: an org token with `units_cap 10` (`test_support::org_token`, PUT through the internal route or `put_grant`), 10 spans already counted (`state.usage.add`) → POST `/v1/traces` → 402, code `quota_exceeded`, `Retry-After` = seconds from the clock to 2026-02-01, detail names `10 units` and `2026-02-01`; nothing appended; at 9 units the request is accepted whole (even with 5 spans, D-P-17).
- `ingest::grpc::tests::capped_org_gets_resource_exhausted`: same setup → `Code::ResourceExhausted`, no details.
- `scores::tests::capped_org_cannot_write_scores`: score write → 402; feedback → 402.
- `entitlements::units::tests`: no cap → Ok without touching the DB; a load error fails open (simulate by closing `state.meta`'s pool? If not practical, test the mapping function `cap_decision(units, cap, now)` that `check_cap` uses, and say so in the commit message).

### Done

`just check`.

---

## P3 — Per-org ingest rate

D-P-19 as amended by D-P-35: gRPC answers `UNAVAILABLE` (retried by OTel exporters) rather than `RESOURCE_EXHAUSTED` with `RetryInfo`, so no new dependency is needed.

### `crates/server/src/entitlements/rate.rs` (new)

```rust
//! Per-org span rate (D-P-19, D-P-35): a token bucket per org.

pub(crate) struct IngestRate {
    buckets: Mutex<HashMap<OrgId, (f64, i64)>>, // tokens, last refill ns
}

impl IngestRate {
    /// Takes `spans` tokens for `org` with `rate` per second and `burst` capacity.
    /// A request with more spans than `burst` costs `burst` (it needs a full bucket).
    /// `Err(RetryAfter)` (whole seconds, at least 1) when not enough tokens.
    pub(crate) fn take(&self, org: OrgId, spans: u64, rate: u64, burst: u64, now_ns: i64) -> Result<(), crate::auth::limits::RetryAfter>;
}
```

Refill `tokens = min(burst, tokens + rate × elapsed_s)`; a new org starts full. `rate == 0` refuses everything with a 60 s retry. `burst == 0` → treat as `rate` (one second's worth). At most 100 000 orgs are kept; past that, evict the fullest bucket (it would be full anyway). `AppInner` gains `ingest_rate: crate::entitlements::rate::IngestRate` (built in `serve.rs` and `test_support`).

### Where it runs

After `Ingest::accept` (or `accept_request`) returns, before `commit_in_task`, in both handlers: when `resolve(org, now).limits` has `ingest_spans_per_sec = Some(rate)`, `take(org, accepted spans, rate, ingest_burst.unwrap_or(rate), now)`. On `Err(retry)`: HTTP → `ApiError::RateLimited { retry_after_s }` (429 with `Retry-After`); gRPC → `tonic::Status::unavailable("rate limited")`. The reservation drops with the accepted batch (nothing is stored).

### Tests

`entitlements::rate::tests`: full bucket allows `burst`; then refuses with the right `Retry-After`; refills over time (advance `now`); oversize request needs a full bucket; `rate 0`. `routes::otlp::tests::rate_limited_org_gets_429` (token limits `ingest_spans_per_sec 1, ingest_burst 2`, three requests of one span at the same instant → third is 429). `ingest::grpc::tests::rate_limited_org_gets_unavailable`.

### Done

`just check`.

---

## P4 — Seats and projects

### Error

If `ProblemCode::PlanLimit` / `ApiError::PlanLimit { detail: String }` do not exist yet (track K adds them, D-K-14), add them exactly as K's plan `docs/plans/k-alerts/06-rules-api.md` § 3b specifies (403, reason "Forbidden"). Otherwise reuse them.

`crates/server/src/entitlements/mod.rs`:

```rust
/// `Err(ApiError::PlanLimit)` when `count + adding` would exceed `limit` (D-P-14).
/// `noun` is singular and plural, e.g. `("seat", "seats")`.
pub(crate) fn check_limit(limit: Option<u64>, count: u64, adding: u64, noun: (&str, &str)) -> Result<(), ApiError>
```

Detail: `format!("Your plan allows {limit} {}.", if limit == 1 { noun.0 } else { noun.1 })`.

### Counts (`crates/server/src/meta/members.rs`, `meta/projects.rs`)

```rust
/// Seats used (D-P-20): members whose role is owner, admin or member, plus pending,
/// unexpired invites with one of those roles. In the caller's transaction.
pub(crate) async fn seats_used_tx(conn: &mut SqliteConnection, org_id: OrgId, now: i64) -> Result<u64, MetaError>;

/// Active projects of the org, in the caller's transaction.
pub(crate) async fn active_projects_tx(conn: &mut SqliteConnection, org_id: OrgId) -> Result<u64, MetaError>;
```

(SQL: `SELECT COUNT(*) FROM org_members WHERE org_id = ? AND role != 'viewer'` plus `SELECT COUNT(*) FROM invites WHERE org_id = ? AND accepted_at IS NULL AND expires_at > ? AND role != 'viewer'`; `SELECT COUNT(*) FROM projects WHERE org_id = ? AND state = 'active'`.)

### Where the checks run (inside each write transaction, before the write)

- `routes/invites.rs` `create_invite`: when the invite's role is not viewer: `check_limit(max_seats, seats_used_tx, 1, ("seat", "seats"))`. Replacing a pending invite for the same email (`replace_invite`) does not add a seat: count it as `adding = 0` when a pending invite for that email exists.
- `routes/invites.rs` `accept_invite` and the signup-with-invite path in `routes/auth.rs` (`pending_signup_invite` callers): accepting an invite turns a pending seat into a member, `adding = 0`, **but** re-check that `seats_used − 1 (this invite) + 1 <= max_seats` only when the invite has expired between creation and acceptance is impossible (expired invites cannot be accepted). So acceptance needs no check, except when the limit was lowered after the invite: check `members_with_seat_roles + 1 <= max_seats` (members only, invites excluded) and refuse with `plan_limit` otherwise.
- `routes/members.rs` `update_org_member`: a change from `viewer` to owner, admin or member: `check_limit(max_seats, seats_used_tx, 1, …)`.
- `routes/projects.rs` `create_project`: `check_limit(max_projects, active_projects_tx, 1, ("project", "projects"))`.

The limit comes from `state.entitlements.resolve(org, now).limits` (read before the transaction). Two concurrent writes run in SQLite's single writer one after the other, so the count is exact.

### Web

- `web/src/components/settings/invite-dialog.tsx`, the role select in `org-members.tsx`, and `new-project-dialog.tsx`: a `403` with code `plan_limit` shows the problem's detail and, under it, `<UpgradeButton url={useOrgEntitlements(org).upgradeUrl} />`. Use `problemOf(error)?.code === "plan_limit"`.
- Tests in each component's existing test file (or the page tests that open them): `server.use` the write answering `problemResponse(403, "plan_limit", { detail: "Your plan allows 1 seat." })` → the detail and an Upgrade link (with the default entitlements fixture's `upgrade_url`).

### Tests (server)

`routes::invites::tests::seats_are_limited` (Free-like token `max_seats 1`: the owner alone fills it; inviting a member → 403 `plan_limit` "Your plan allows 1 seat."; inviting a viewer → 201); `routes::members::tests::promoting_a_viewer_needs_a_seat`; `routes::projects::tests::projects_are_limited` (`max_projects 1`: the seed's project fills it → 403); acceptance after the limit was lowered → 403; no limit (defaults) → everything allowed.

### Done

`just openapi` (new 403 responses where missing: `post_project` and `post_org_invite` already list 403; `patch_org_member` too — verify), `just check`.

---

## P5 — The usage API and the month's usage

### `GET /api/v1/orgs/{org}/usage?from=YYYY-MM-DD&to=YYYY-MM-DD`

`routes/usage.rs` (new), `Access::Org { perm: OrgPerm::View, session_only: false }`. `from` and `to` are inclusive UTC days, at most 366 days apart; defaults: the first of the current month to today. `from > to` → 400. Response:

```rust
/// One UTC day of an org's usage, summed over its projects.
pub struct UsageDay {
    pub day: String,              // YYYY-MM-DD
    pub spans: u64,
    pub scores: u64,
    pub units: u64,               // spans + scores
    pub bytes: u64,
    pub judge_input_tokens: u64,
    pub judge_output_tokens: u64,
    pub judge_cost_usd: f64,
}

/// `GET /api/v1/orgs/{org}/usage`.
pub struct OrgUsage {
    pub from: String,
    pub to: String,
    /// One row per day that has usage, oldest first. Days without usage are absent.
    pub days: Vec<UsageDay>,
}
```

(`crates/server/src/api/usage.rs`, new, `Serialize, Deserialize, ToSchema`.) SQL: `SELECT day, SUM(spans), SUM(scores), SUM(bytes), SUM(judge_input_tokens), SUM(judge_output_tokens), SUM(judge_cost_usd) FROM usage_daily WHERE org_id = ? AND day BETWEEN ? AND ? GROUP BY day ORDER BY day`. Today's row includes only flushed counts (up to 10 s behind).

### Month usage in the entitlements response

`OrgEntitlements` gains:

```rust
/// This UTC month's units (D-P-16, D-P-18).
pub usage: OrgMonthUsage,

pub struct OrgMonthUsage {
    /// `YYYY-MM`.
    pub month: String,
    /// Units so far this month (flushed and not).
    pub units: u64,
    /// When the month resets (00:00 UTC on the 1st of the next month).
    #[serde(with = "time_serde")] #[schema(value_type = String, format = DateTime)]
    pub resets_at: i64,
    /// `none`, `warning` (≥ 80 % of `units_included`) or `reached` (≥ 100 %).
    pub notice: UsageNotice,
}
```

`UsageNotice` is a snake_case enum. With no `units_included`, the notice is `none`. `get_entitlements` calls `state.usage.month_units` (a load error → 500 like any other `MetaError`).

OpenAPI: operation `get_org_usage` (tag `orgs`, `AUTH_ERRORS`, query params `from`, `to`), register types, `OPERATIONS`; fixtures `fixtures/api/static/org-usage.json` (three days) and the `usage` block in `org-entitlements.json`; `api_examples.rs` static entry and `tests/api_examples.rs` round trips. `just openapi`, `just api-examples`.

### Tests

`routes::usage::tests`: rows of two projects on the same day are summed; range bounds inclusive; default range; `from > to` 400; 367-day range 400; outsider 404. `routes::entitlements::tests::usage_block`: units and notice at 79 %, 80 %, 100 % of a token's `units_included`.

### Done

`just check`.

---

## P6 — Usage notices

### Migration

Next free number `NNNN_usage_notices.sql`:

```sql
-- One row per org, month and threshold, so each notice is sent once (D-P-18).
CREATE TABLE usage_notices (
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  month TEXT NOT NULL,          -- 'YYYY-MM'
  threshold INTEGER NOT NULL,   -- 80 or 100
  sent_at INTEGER NOT NULL,
  PRIMARY KEY (org_id, month, threshold)
);
```

Update the migration-count and schema-snapshot tests (as M2's P3 did).

### Task (`crates/server/src/entitlements/notices.rs`, new)

`pub(crate) async fn notices_once(state: &AppState) -> Result<u32, MetaError>`: for every org with a loaded month counter (`Usage::loaded_orgs()` — add it: the org ids in `months`), resolve `units_included`; for each threshold (80, 100) that `units * 100 >= included * threshold` reaches, `INSERT INTO usage_notices … ON CONFLICT DO NOTHING`; when a row was inserted, send `Email::UsageNotice` to every owner (`meta::members::org_members`, role owner) through `state.mailer` (`send_in_background`; no mailer → no email, the row is still written so it is not retried). Returns the number of notices recorded. `serve.rs` spawns it every 60 s (follow how `spawn_usage` spawns the flush loop), with `tracing::info!` of the count when non-zero.

### Email

`Email::UsageNotice { org_name: String, threshold: u8, units: u64, included: u64, paid: bool, url: String }` (`paid` = the org has no `units_cap` or a cap above `included`; `url` = `<public_url>/<org slug>/settings/plan`). Templates `usage_notice.txt` and `usage_notice.html` (register in the template list), subject `"{PRODUCT}: {org_name} used {threshold}% of its monthly units"`. Text for `paid = false`: at 80 "Your organization {org} has used {units} of its {included} monthly units. At {included}, ingest stops until the 1st of next month." At 100: "… reached its {included} monthly units. Ingest is paused until the 1st of next month (UTC)." For `paid = true`: "… Additional units are billed as overage." Then the link. Render tests like the existing template tests.

### Banner (web)

`web/src/components/plan/usage-banner.tsx` (MIT): for owners and admins only (the org role from the org context), reads `useOrgEntitlements(org)`; `notice === "warning"` → a warning bar "This organization has used 80% of its monthly units."; `reached` → "This organization reached its monthly units; ingest is paused until {resets_at day} (UTC)." (for orgs with a cap) or "…; additional units are billed as overage." (no cap); both with the Upgrade button. Mounted once in the org layout (find the component that wraps `$org` pages; render it above the outlet). Tests: warning, reached with and without cap, hidden for members, hidden for `none`.

### Tests (server)

`notices_once` sends one email at 80 % and one at 100 %, never twice (run it three times), to owners only; the next month sends again; no `units_included` → nothing.

### Done

`just check`.

---

## P7 — Usage export for the control plane

`GET /api/internal/v1/usage?from=YYYY-MM-DD&to=YYYY-MM-DD&limit=&cursor=`, `Access::ControlPlane { scope: "usage" }`, tag `internal`. Rows per org and day (summed over projects), ordered by `(day, org_id)`, `limit` default 1000, 1–10 000, cursor = base64url JSON `{"day": "…", "org_id": "…"}` (follow `api_keys.rs`'s cursor helpers), `from`/`to` inclusive and at most 92 days apart. Row type `InternalUsageRow { org_id: Uuid, day: String, spans, scores, units, bytes, judge_input_tokens, judge_output_tokens, judge_cost_usd }`, page `Page<InternalUsageRow>`. Rows of dropped orgs stay (`usage_daily` has no foreign key), so billing still sees them.

Tests: two orgs, three days → order and paging; scope `orgs` token → 403; session → 403; bad cursor → 400.

OpenAPI: operation `list_internal_usage`, bearer security like `post_internal_org`. `just openapi`.

---

## P8 — Users for the control plane (Q's R14)

Scope `users`, tag `internal`, bearer security. Body for all three: `InternalUserRef { subject: String }` (`deny_unknown_fields`), the subject of the user's `oidc:cloud` identity — the provider name is config: `entitlements.control_plane_provider` (new key in `EntitlementsConfig`, default `"oidc:cloud"`; match it against `identities.provider`). Unknown subject → 404.

- `POST /api/internal/v1/users/revoke`: deletes the user's `web_sessions` and `personal_tokens` rows (and evicts the PAT cache: `state.pat_cache` — find its evict/clear method, or clear the whole cache). 204.
- `POST /api/internal/v1/users/disable`: the same as the instance admin's disable (`meta::users::set_disabled`, plus revoke). Refused with 422 `owner_not_allowed`? No: disabling is allowed even for owners. Instance admins: 403 (the control plane never manages operators). 204.
- `POST /api/internal/v1/users/delete`: in one transaction: refuse an instance admin (403); collect the orgs where the user is the only owner (`count_owners` = 1 and the user is owner) → `sole_owner_of`; set `invites.invited_by`, `api_keys.created_by` to NULL where they reference the user; delete the `users` row (cascades: identities, sessions, members, overrides, tokens, resets, verifications; `saved_views` and `llm_connections` set NULL). 200 `{ "sole_owner_of": [org ids] }` — those orgs stay, without an owner, for the control plane to resolve. Audit log `user deleted` with the user id and `actor = control_plane`.

Tests per route, including a user owning an org alone, cascade checks, admin refusal, unknown subject 404, wrong scope 403.

---

## P9 — Memberships for the control plane (Q's R6)

Scope `members`, tag `internal`, bearer security.

- `POST /api/internal/v1/memberships/lookup`, body `{ "subject": "…", "email": "…" }` (both required strings; the email is normalized): the user of that `oidc:cloud` identity, else the user with that email, else 200 `{ "user_id": null, "orgs": [] }`. Otherwise `{ "user_id": "…", "orgs": [{ "org_id", "slug", "role" }] }` (active orgs only, ordered by slug).
- `GET /api/internal/v1/orgs/{org_id}/members?role=owner,admin` (`role` optional, comma-separated `OrgRole` names): `{ "data": [{ "user_id", "email", "name"?, "role" }] }` ordered by email; unknown org 404. (The email goes to the control plane, which is the account system; it is never logged.)

Tests: lookup by subject, by email fallback, unknown; members filter; wrong scope.

---

## P10 — Usage on the Plan page

`web/src/routes/$org/settings/plan.tsx` gains a "Usage this month" section above the features:
- a bar: `usage.units` of `limits.units_included` (or of `limits.units_cap` when there is no included value; no limit → just the number), with marks at 80 % and 100 %, the reset date (`usage.resets_at`, day), and for capped orgs "Ingest pauses at {cap} units.";
- a daily chart over the current month from `GET /api/v1/orgs/{org}/usage` (default range): stacked bars of spans and scores per day, using the app's chart components (`web/src/components/ui/chart.tsx`, as the metrics dashboard does); below it a small table of the month's totals (spans, scores, bytes with the existing byte formatter, judge cost with `formatCost` if it exists in `format.ts`, else `$x.xx`).
- `web/src/api/queries-settings.ts`: `orgUsageQuery(org)`.

Tests: bar text and marks with the fixture; no `units_included` → only the number; the chart renders three bars from `org-usage.json`; empty usage → "No usage this month.".

---

## P11 — Python SDK pauses on 402 (D-P-34)

`sdks/python/src/llmobs/_runtime.py` builds an `OTLPSpanExporter`. Give it a `requests.Session` whose response hook records a 402: `session.hooks["response"].append(self._on_response)`; on `status_code == 402`, read `Retry-After` (seconds; default 3600 when absent or invalid), set `paused_until = time.monotonic() + seconds`, and log one `warning` ("llmobs: the organization reached its monthly units cap; spans are dropped until <UTC reset time>"), at most once per pause. A new wrapping exporter (`_quota.py`, class `QuotaPause(SpanExporter)`) wraps the masking exporter: `export` returns `SpanExportResult.SUCCESS` without calling the inner exporter while paused (spans dropped, counted in a debug log), otherwise delegates. `shutdown`/`force_flush` delegate. Wire it in `create`. Score writes (`_scores.py`): a 402 logs the same warning once and drops the score (no retry).

Tests (`sdks/python/tests/test_quota.py`, with the existing local HTTP test server or `responses`-style mocking the SDK tests already use): a 402 with `Retry-After: 60` → the next export sends nothing; after 60 s (monkeypatch `time.monotonic`) it sends again; one warning only; a score write 402 → one warning, no exception.

`just check-py`.

---

## P12 — TypeScript SDK pauses on 402 (D-P-34)

`sdks/typescript/src/exporter.ts` `FetchTraceExporter`: on a `402` response, read `Retry-After` (seconds, default 3600), set `pausedUntil = Date.now() + seconds * 1000`, log one warning through the SDK's logger (same text as Python), and return `SUCCESS` (spans dropped). While `Date.now() < pausedUntil`, `export` returns `SUCCESS` immediately without fetching. 402 is not in `RETRYABLE_STATUSES`. `scores.ts`: a 402 logs the warning once and resolves without throwing.

Tests in `exporter.test.ts` and `scores.test.ts` with the existing fake fetch: pause, resume after the time (fake timers), one warning.

`just check-ts`.

---

## P13 — Close out

- Remove every `allow(dead_code)` attribute M3 added; `just lint` clean.
- Check that specs 02, 04, 09, 11 and 12 match what was built (the planning commit updated them; fix any drift in the same commit).
- `docs/roadmap.md` row **P** Status: `**In progress.** M1 and M2 merged. M3 implemented: monthly units and the 402 cap, per-org ingest rate, seats and projects, usage notices, the usage API and export, users and memberships for the control plane, SDK pause on 402. M4 and M5 plans to follow.`
- `just check`, `just e2e`.

## Coordination

- **I (online evaluation):** evaluator scores go through `scores::write`, so at the cap they fail with `quota_exceeded`; I's executor should treat it as a permanent failure of the job (no retry, no rule pause).
- **Not in M3 (for the coordinator):** Q's R7 (login straight to the only provider), R13 (instance admins keep password login), R16 (OIDC client secret from env or file) go to a small auth track (D-P-43); R5 (org deletions feed) and R17 (internal org delete) go to P's M4 once track M is implemented.

## Report

Per sub-phase: what was done, deviations, the migration number used, whether `plan_limit` came from track K or was added here, anything unverified.
