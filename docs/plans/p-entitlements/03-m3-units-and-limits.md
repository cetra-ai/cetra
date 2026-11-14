# P · M3 — Units, limits and usage

Milestone 3 of track P ([`00-design.md`](00-design.md)). It builds on M2 ([`02-m2-org-entitlements.md`](02-m2-org-entitlements.md), merged first): M2 gives every org a resolved `Entitlements` with nine limits; M3 enforces the volume and count limits and shows usage.

When M3 is done:
- each org's units this UTC month (spans + scores, D173, D-P-16) are counted in memory, loaded from `usage_daily` on first use (D-P-36);
- at `units_cap`, OTLP HTTP ingest, score writes and public feedback answer `402 quota_exceeded` with `Retry-After` = seconds to the reset; OTLP gRPC answers `RESOURCE_EXHAUSTED` (D-P-17, D-P-37);
- spans over the org's ingest rate answer `429 rate_limited` (gRPC `UNAVAILABLE`) (D-P-19, D-P-35);
- invites, role changes, invite acceptance, signup with an invite, OIDC auto-provisioning and project creation respect `max_seats` and `max_projects` (`403 plan_limit`, D-P-20);
- org owners get one email, and owners and admins see a banner, when the month reaches 80 % and 100 % of `units_included` (D-P-18, D-P-38);
- `GET /api/v1/orgs/{org}/usage` returns daily usage; the entitlements response carries the month's units; the Plan page shows a month bar and a daily chart (D-P-42);
- the control plane reads usage (scope `usage`), revokes, disables or deletes a user by its OIDC subject (scope `users`), and reads memberships (scope `members`) (D-P-24, D-P-39, D-P-40);
- the Python and TypeScript SDKs pause exporting after a 402, for at most one hour at a time (D-P-34).

**Read first, in this order:**
1. `AGENTS.md`.
2. [`00-design.md`](00-design.md) § Decisions D-P-13–D-P-20, D-P-24, D-P-28, D-P-34–D-P-43; § Interfaces for track Q.
3. `docs/spec/02-ingestion.md` (responses, limits table, pipeline); `docs/spec/04-metadata-db.md` § Metering and § Entitlements; `docs/spec/09-api.md`; `docs/spec/12-operations.md` § Licence keys, plans and entitlements.
4. M2's code: `crates/server/src/entitlements/` (`EntitlementsState::resolve`, `plan_at`, `Limits`, `tokens::grant_from_token`, `put_grant`), `routes/entitlements.rs`, `routes/internal_orgs.rs` (the pattern for an internal route and its OpenAPI operation), `auth/principal.rs` (`Principal::ControlPlane`), `http/access.rs` (`Access::ControlPlane`), `test_support` (`org_token`, `control_plane_token`).
5. The code named in each sub-phase before you change it.

## Conventions

The same as M2 (`02-m2-org-entitlements.md` § Conventions), including "Dead code between phases" (here P13 removes every attribute M3 added) and "Generated files". Server code returns `ApiError`; never log or return payload, tokens, emails or names; time from `state.clock.now_ns()`; months and days are UTC; tests on `TestApp` (manual clock `2026-01-01T00:00:00Z`); web tests on MSW with `server.use`.

### Module wiring

Every new route module (`routes/usage.rs`, `routes/internal_usage.rs`, `routes/internal_users.rs`, `routes/internal_members.rs`) gets `pub(crate) mod …;` in `routes/mod.rs` and `routes.extend(crate::routes::<module>::routes());` in `http/access.rs`'s route table; every new `meta` module (`meta/usage.rs`) gets `pub(crate) mod usage;` in `meta/mod.rs`; every new `entitlements` module (`units`, `rate`, `notices`) its line in `entitlements/mod.rs`; every new `api` module (`api/usage.rs`) `pub mod usage;` in `api/mod.rs`.

### Every new or changed API operation

For each operation a sub-phase adds or changes, do all of these (the OpenAPI lint and the API-examples tests check them):
1. the operation in `crates/server/src/api/openapi.rs` with its id, tag, parameters, request body, responses and error list;
2. security: public (`.securities(Some(Vec::new()))`), the document default (no call, for session-or-PAT org/project routes), or bearer-only (`Access::ControlPlane` routes: copy `post_internal_org` from M2), and the matching arm in `tests/openapi.rs` (`document_lint`'s security checks);
3. `OPERATIONS` in `crates/server/tests/openapi.rs`;
4. the expected-errors branch in `tests/openapi.rs` with exactly the operation's error codes (the default branch is `["401", "403", "404", "429"]`);
5. `QUERY_OR_BODY` in `tests/openapi.rs` when the operation has query parameters or a body;
6. `collect_schemas` registration of every new type;
7. for a `GET` with a `200` body: an API example (`fixtures/api/static/<name>.json`, its `api_examples.rs` static entry, and `roundtrip::<Type>` plus the `generated_files_roundtrip` arm in `tests/api_examples.rs`). **Exception:** operations tagged `internal` have no examples: in `tests/api_examples.rs` `manifest_covers_the_read_operations`, skip operations whose first tag is `internal` (add that `continue` in this milestone, P7).
8. `just openapi`, then `just api-examples`, each twice with no change the second time.

### Test helper for limits

P2 adds to `crate::test_support`:

```rust
/// Gives `org_id` a valid org token (issued at `issued_at`, valid until 2099) with these
/// limits, through `grant_from_token` and `state.entitlements.put_grant`. `limits` is the
/// JSON object of the `limits` claim; missing keys take the defaults.
pub(crate) fn set_limits(app: &TestApp, org_id: OrgId, issued_at: &str, limits: serde_json::Value)
```

(It signs with `org_token`, calls `crate::entitlements::tokens::grant_from_token(&token, app.state.config.public_url.origin(), &app.state.entitlements.defaults().limits)`, and `put_grant`; it does not write `meta.db`.) Use a later `issued_at` to lower a limit.

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
| P2 | `Cap monthly units with 402` | `quota_exceeded`, OTLP HTTP and gRPC, `scores::write`, `set_limits` | P1 |
| P3 | `Rate-limit ingest per org` | `entitlements::rate`, `IngestError::RateLimited` | P2 |
| P4 | `Enforce seats and projects` | `plan_limit`, every seat-adding path, projects; web states | P3 |
| P5 | `Add the usage API` | `GET /api/v1/orgs/{org}/usage`; month usage in the entitlements response | P4 |
| P6 | `Send usage notices` | `usage_notices`, the task, the email, the banner | P5 |
| P7 | `Export usage to the control plane` | `GET /api/internal/v1/usage` | P6 |
| P8 | `Let the control plane manage users` | `POST /api/internal/v1/users/{revoke,disable,delete}` | P7 |
| P9 | `Let the control plane read memberships` | lookup and org members | P8 |
| P10 | `Show usage on the Plan page` | month bar, daily chart, `formatBytes` | P9 |
| P11 | `Pause the Python exporter on 402` | `sdks/python` | — |
| P12 | `Pause the TypeScript exporter on 402` | `sdks/typescript` | — |
| P13 | `Close out M3` | dead-code attributes, roadmap | all |

P1–P10 run in order (they edit the same server files and the generated contract). P11 and P12 touch only their SDK and depend only on the HTTP contract (402 with `Retry-After`), so each can run in parallel with P1–P10 in its own worktree. P13 runs last.

**SDK warning text** (both SDKs, exactly): `llmobs: the organization reached its monthly units cap; telemetry is dropped until <reset> (retrying at most every hour)`, where `<reset>` is the UTC instant `now + Retry-After` formatted `YYYY-MM-DDTHH:MM:SSZ`.

---

## P1 — Month units

### `crates/server/src/usage.rs`

```rust
/// Units (spans + scores) of one org in one UTC month (D-P-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonthUnits {
    /// `year * 12 + (month − 1)` of the month counted.
    month: i32,
    /// Units since 00:00 UTC on the 1st, flushed or not.
    units: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Usage {
    counts: Mutex<HashMap<UsageKey, UsageCounts>>,
    /// Loaded month counters. Always locked while `counts` is held (lock order: counts, then months).
    months: Mutex<HashMap<OrgId, MonthUnits>>,
    /// Held by `flush` from taking the counts to the end of its write, and by
    /// `month_units` while it loads, so a load never misses counts in flight.
    flush_lock: tokio::sync::Mutex<()>,
}
```

- `fn month_key(now_ns: i64) -> i32` from `DateTime::from_timestamp_nanos(now_ns)`: `year * 12 + (month0 as i32)`.
- `add(...)`: while holding the `counts` lock, also lock `months` and, when `spans + scores > 0` and the org has a `MonthUnits` with `month == month_key(now_ns)`, add `spans + scores` (saturating). An older month is left alone (the next `month_units` reloads it).
- `flush`: take `self.flush_lock.lock().await` for its whole body.
- New:

```rust
/// The org's units this UTC month at `now_ns` (D-P-16, D-P-36): `usage_daily` from the 1st
/// plus counts not flushed yet. Loads once per org and month, then follows `add`.
///
/// # Errors
/// [`MetaError`] when the load query fails.
pub(crate) async fn month_units(&self, meta: &MetaDb, org_id: OrgId, now_ns: i64) -> Result<u64, MetaError>

```

`month_units`:
1. Fast path: lock `months`; a counter with this month → return its `units`.
2. Slow path: `let _guard = self.flush_lock.lock().await;` then check the fast path again; run `SELECT COALESCE(SUM(spans + scores), 0) FROM usage_daily WHERE org_id = ? AND day >= ? AND day < ?` with the month's first day and the next month's first day (`YYYY-MM-DD`); then **while holding the `counts` lock**, add the unflushed counts of this org whose `day` is in the month, and insert the `MonthUnits` into `months` (locked inside `counts`, the same order as `add`). Return the units. `SUM` is `i64`: convert with `u64::try_from(..).unwrap_or(0)`.

`month_units` has no non-test caller until P2: give it `#[cfg_attr(not(test), allow(dead_code))]` (item level), removed in P13.

### Tests (`usage::tests`)

1. `month_units_loads_and_follows_adds`: `usage_daily` rows this month for the org (spans 10, scores 5; spans 1), last month (spans 100), another org (spans 50) → `month_units` = 16; `add` 3 spans and 2 scores → 21; another org's `add` does not change it.
2. `unflushed_counts_are_included_at_load`: `add` 4 spans (not flushed), first `month_units` → 4; `flush`, then `month_units` → 4.
3. `a_new_month_starts_at_zero`: load at 2026-01-15 (units 7); call `month_units` at 2026-02-01T00:00:00Z → reloads, 0 plus February rows.
4. `concurrent_flush_load_and_add_lose_nothing`: `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]`; spawn 4 tasks that each `add` 1 span 500 times (`tokio::task::spawn`), 1 task that `flush`es 50 times, and 1 task that calls `month_units` 50 times; after all finish, `month_units` equals 2 000.

### Done

`just check`.

---

## P2 — The monthly cap

### Error

`api/common.rs` `ProblemCode::QuotaExceeded` (`quota_exceeded`, status 402, reason `"Payment Required"`, doc `/// The org reached its monthly units cap (D-P-17).`); add to the status test. `http/error.rs` `ApiError::QuotaExceeded { detail: String, retry_after_s: u64 }`: the problem with the detail and the header `Retry-After: <retry_after_s>`, built the way `ApiError::RateLimited` adds its header; add to `error_statuses`. `api/openapi.rs` `problem(402)`: `"The organization reached its monthly units cap (code quota_exceeded). Retry-After gives the seconds until it resets."`.

### The check (`crates/server/src/entitlements/units.rs`, new; `pub(crate) mod units;` in `entitlements/mod.rs`)

```rust
//! The monthly units cap (D-P-17, D-P-37).

/// The org is at its cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapExceeded {
    pub(crate) detail: String,
    pub(crate) retry_after_s: u64,
}

impl From<CapExceeded> for ApiError { /* ApiError::QuotaExceeded { detail, retry_after_s } */ }

/// `Err(CapExceeded)` when the org's units this month are at or above its `units_cap`.
/// No cap: `Ok` without touching the database. A load error fails open: `warn` with the
/// org id, `Ok`.
pub(crate) async fn check_cap(state: &AppState, org: OrgId) -> Result<(), CapExceeded>

/// The decision, pure: `Some` when `units >= cap`.
fn decide(units: u64, cap: u64, now_ns: i64) -> Option<CapExceeded>
```

`decide`: the reset is 00:00 UTC on the 1st of the next month (`chrono`: from `now`'s year and month; month 12 rolls to January of the next year); `retry_after_s = max(1, ceil((reset_ns − now_ns) / 1e9))`; detail `format!("This organization reached its monthly limit of {cap} units. Ingest resumes on {} (UTC).", reset.format("%Y-%m-%d"))`.

### Where it runs

- **OTLP HTTP** (`routes/otlp.rs`): in `export`, before `export_body`: `if let Err(cap) = check_cap(&state, access.project.org_id).await { tracing::debug!(target: "ingest", project_id = %access.project.id, reason = "quota", "request refused"); return Err(cap.into()); }` — before the body is read and before any budget is reserved.
- **OTLP gRPC** (`ingest/grpc.rs`): after the project resolves, before `reserve_exact`: `if let Err(cap) = check_cap(state, project.org_id).await { return Err(tonic::Status::resource_exhausted(cap.detail)); }` (no `RetryInfo`).
- **Scores**: `scores::write`, at the top: `check_cap(state, project.org_id).await.map_err(ApiError::from)?;`. This covers `POST …/scores` (`create_scores`, `create_key_scores`), `POST …/feedback` (`post_feedback`) and evaluator scores (track I's jobs then fail; see § Coordination). `scores::delete` is separate and is never capped.
- OpenAPI: `create_scores`, `create_key_scores` and `post_feedback` gain `402` in their error lists and in their expected-error branches in `tests/openapi.rs`.

### Tests

- `test_support::set_limits` (§ Conventions).
- `entitlements::units::tests`: `decide` at 2026-01-15T12:00:00.5Z with cap 10 and units 10 → retry `1_425_600` seconds (ceil of 1 425 599.5), detail names `10 units` and `2026-02-01`; at 2026-12-31 → `2027-01-01`; units 9 → `None`. `check_cap` with no cap → `Ok`; a load error (rename the table, as a test in `usage.rs` does with another name: `ALTER TABLE usage_daily RENAME TO usage_daily_gone`) with a cap → `Ok`, and a `warn` in `capture_logs`.
- `routes::otlp::tests::capped_org_gets_402`: `set_limits(units_cap 10)`; `state.usage.add` 10 spans for the org → POST `/v1/traces` → 402, code `quota_exceeded`, `Retry-After` header present, nothing appended; with 9 counted, a request of 5 spans is accepted whole (D-P-17).
- `ingest::grpc::tests::capped_org_gets_resource_exhausted`: same → `Code::ResourceExhausted`.
- `scores::tests::capped_org_cannot_write_scores`: score write → 402; feedback → 402; score deletion → allowed.

### Done

`just openapi` twice, `just check`.

---

## P3 — Per-org ingest rate

### `crates/server/src/entitlements/rate.rs` (new; `pub(crate) mod rate;`)

```rust
//! Per-org span rate (D-P-19, D-P-35): a token bucket per org.

#[derive(Debug, Default)]
pub(crate) struct IngestRate {
    /// tokens, last refill (ns)
    buckets: Mutex<HashMap<OrgId, (f64, i64)>>,
}

impl IngestRate {
    /// Takes `spans` tokens for `org`. See the rules below.
    pub(crate) fn take(&self, org: OrgId, spans: u64, rate: u64, burst: u64, now_ns: i64) -> Result<(), crate::auth::limits::RetryAfter>;
}
```

Rules: `spans == 0` → `Ok` without touching the bucket. A new org starts full (`burst` tokens). Refill: `tokens = min(burst, tokens + rate × elapsed_s)`. `cost = min(spans, burst)` (an oversize request needs a full bucket). `tokens >= cost` → subtract, `Ok`. Otherwise `RetryAfter(Duration::from_secs(max(1, ceil((cost − tokens) / rate))))`. `rate == 0` → `RetryAfter(60 s)`. `burst == 0` → use `rate` as the burst. At most 100 000 orgs are kept: when inserting the 100 001st, remove any one org whose bucket is full after refill (scan; if none, remove an arbitrary one). `AppInner` gains `ingest_rate: crate::entitlements::rate::IngestRate` (in `serve.rs` and `test_support::open`, `IngestRate::default()`).

### Error

`ingest/mod.rs` `IngestError::RateLimited { retry_after_s: u64 }` (`#[error("ingest rate limit")]`). Every exhaustive match gains its arm: `From<IngestError> for ApiError` → `ApiError::RateLimited { retry_after_s }`; `From<IngestError> for tonic::Status` in `ingest/grpc.rs` → `Status::unavailable("rate limited")`; `log_refusal` in `routes/otlp.rs` logs every refusal at `warn` today: make it return early with a `tracing::debug!(target: "ingest", project_id, reason = "rate", "request refused")` for `RateLimited`; the gRPC wrapper in `ingest/grpc.rs` (which logs every refusal at `warn`) gets the same early `debug` for rate and cap refusals; the table test `error_mapping_matches_the_design_table` gains the row.

### Where it runs

In both handlers, after `accept` / `accept_request` returns and before `commit_in_task`: `let limits = state.entitlements.resolve(org, now).limits;` when `limits.ingest_spans_per_sec` is `Some(rate)`: `state.ingest_rate.take(org, u64::try_from(normalized.spans.len()).unwrap_or(u64::MAX), rate, limits.ingest_burst.unwrap_or(rate), now).map_err(|retry| IngestError::RateLimited { retry_after_s: retry.0.as_secs().max(1) })?`. The accepted batch and its reservation drop; nothing is stored.

### Tests

`entitlements::rate::tests`: full bucket allows `burst`; next refused with the right retry; refill after time; oversize request needs a full bucket; zero spans pass; `rate 0`. `routes::otlp::tests::rate_limited_org_gets_429` (`set_limits({"ingest_spans_per_sec": 1, "ingest_burst": 2})`, three one-span requests at the same instant → the third is 429 with `Retry-After: 1`). `ingest::grpc::tests::rate_limited_org_gets_unavailable`.

### Done

`just check`.

---

## P4 — Seats and projects

### Error and helpers

If `ProblemCode::PlanLimit` / `ApiError::PlanLimit { detail: String }` do not exist yet (track K adds them, D-K-14), add them as K's plan `docs/plans/k-alerts/06-rules-api.md` § 3b specifies (403, reason "Forbidden"). Otherwise reuse them.

`crates/server/src/entitlements/mod.rs`:

```rust
/// Whether `count + adding` would exceed `limit` (D-P-20). `None` never does.
pub(crate) fn over_limit(limit: Option<u64>, count: u64, adding: u64) -> bool

/// The `plan_limit` error for `limit` (D-P-14). `noun` is (singular, plural).
pub(crate) fn plan_limit(limit: u64, noun: (&str, &str)) -> ApiError
```

Detail: `format!("Your plan allows {limit} {}.", if limit == 1 { noun.0 } else { noun.1 })`.

### Counts (`meta/members.rs`, `meta/projects.rs`)

```rust
/// Members whose role is owner, admin or member.
pub(crate) async fn seat_members_tx(conn: &mut SqliteConnection, org_id: OrgId) -> Result<u64, MetaError>;
/// Pending (not accepted, not expired at `now`) invites whose role is not viewer,
/// excluding the pending invite of `except_email` when given.
pub(crate) async fn seat_invites_tx(conn: &mut SqliteConnection, org_id: OrgId, now: i64, except_email: Option<&str>) -> Result<u64, MetaError>;
/// Active projects of the org.
pub(crate) async fn active_projects_tx(conn: &mut SqliteConnection, org_id: OrgId) -> Result<u64, MetaError>;
```

### Where the checks run

Each site reads `let limits = state.entitlements.resolve(org, now).limits;` before its `meta.write` and checks inside the transaction **before any write in the closure** (an outcome returned with `Ok` commits whatever was written before it; the single writer makes counts exact). The closure returns an outcome variant `OverLimit(u64)` (the limit) that the handler turns into `plan_limit(limit, noun)`. Where to add it: `create_invite`'s outcome enum (check before `replace_invite`); `accept_invite`'s outcome enum (check before `add_org_member`); `signup`'s closure (check before `insert_user`; add an outcome enum if it returns `Option<User>` today); `update_org_member`: give it its own outcome enum `RoleChange { Done, Missing, Forbidden, OverLimit(u64) }` (do not extend `MemberChange`, which `remove_org_member` and `ee/project_roles.rs` match exhaustively), checked before `set_org_role`; `create_project`: an outcome enum (check before the insert); `provision_org`: check before `add_org_member` and return `Ok(())` without adding:

| Site | Condition checked (only when the new role is not viewer) | Noun |
|---|---|---|
| `routes/invites.rs` `create_invite` (outcome `Created`; `replace_invite` deletes then inserts a pending invite for the email) | `over_limit(max_seats, seat_members + seat_invites(except_email = the invite's email), 1)` | seat |
| `routes/invites.rs` `accept_invite`, the branch that calls `add_org_member` (the caller is not yet a member) | `over_limit(max_seats, seat_members, 1)` (invites excluded: this one becomes a member) | seat |
| `routes/auth.rs` `signup`, inside its write closure where the invite is accepted and `add_org_member` runs | same as accept | seat |
| `routes/members.rs` `update_org_member` (outcome `MemberChange`), a change from viewer to owner, admin or member | `over_limit(max_seats, seat_members + seat_invites(None), 1)` | seat |
| `auth/oidc/flow.rs` `provision_org` (auto-provisioning on OIDC login) | `over_limit(max_seats, seat_members + seat_invites(None), 1)` → do **not** add the membership; `tracing::info!(org_id, "auto-provision skipped: seat limit")`; the login itself succeeds | — |
| `routes/projects.rs` `create_project` | `over_limit(max_projects, active_projects, 1)` | project |

Viewers are never limited (D176). Existing members and projects are never removed.

### Web

Each place shows a `403 plan_limit` problem's detail and, under it, `<UpgradeButton url={useOrgEntitlements(org).upgradeUrl} />`; check `problemOf(error)?.code === "plan_limit"` **before** any existing 403 branch:
- the invite dialog (`web/src/components/settings/invite-dialog.tsx`) and the role select (`org-members.tsx`): tests in `web/src/routes/$org/settings/members.test.tsx`;
- the new-project dialog (`web/src/components/settings/new-project-dialog.tsx`, which already branches on 403): tests in `web/src/routes/home.test.tsx`.

Tests: `server.use` the write answering `problemResponse(403, "plan_limit", { detail: "Your plan allows 1 seat." })` (or "1 project.") → the detail and an Upgrade link to the fixture's `upgrade_url`.

### Tests (server)

`set_limits({"max_seats": 1, "max_projects": 1})` on the seeded org (whose owner fills the seat and whose project fills the project):
- `routes::invites::tests::seats_are_limited`: inviting a member → 403 `plan_limit` "Your plan allows 1 seat."; a viewer → 201; with `max_seats 2`: invite member A → 201, re-invite A as member (replace) → 201, invite B → 403; re-invite A as viewer → 201, then invite B → 201.
- `accept_after_the_limit_was_lowered`: `max_seats 2`, invite a member, then `set_limits` later with `max_seats 1`; accepting → 403; a viewer invite accepts.
- `routes::auth::tests::signup_with_invite_respects_seats`: the same through signup.
- `routes::members::tests::promoting_a_viewer_needs_a_seat`.
- `auth::oidc` test (follow the module's auto-provision tests): at the limit, login succeeds and no membership is added.
- `routes::projects::tests::projects_are_limited`: → 403 "Your plan allows 1 project."
- No limits (defaults) → all allowed.

`post_org_invite`, `post_project`, `patch_org_member`, `post_invite_accept` and `post_signup` already list 403; check, and add it (and to their expected-error branches) where missing. `just openapi`.

### Done

`just check`.

---

## P5 — The usage API and the month's usage

### Types (`crates/server/src/api/usage.rs`, new; `pub mod usage;` in `api/mod.rs`)

```rust
/// One UTC day of an org's usage, summed over its projects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UsageDay {
    /// `YYYY-MM-DD`.
    pub day: String,
    pub spans: u64,
    pub scores: u64,
    /// `spans + scores`.
    pub units: u64,
    pub bytes: u64,
    pub judge_input_tokens: u64,
    pub judge_output_tokens: u64,
    pub judge_cost_usd: f64,
}

/// `GET /api/v1/orgs/{org}/usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OrgUsage {
    pub from: String,
    pub to: String,
    /// Days with usage, oldest first. Days without usage are absent.
    pub days: Vec<UsageDay>,
}
```

(`f64` prevents `Eq`; derive `PartialEq` only.)

### Route (`crates/server/src/routes/usage.rs`, new; wired in `http/access.rs`)

`GET /api/v1/orgs/{org}/usage?from=YYYY-MM-DD&to=YYYY-MM-DD`, `Access::Org { perm: OrgPerm::View, session_only: false }`. `from` and `to` are optional inclusive UTC days (`chrono::NaiveDate::parse_from_str(.., "%Y-%m-%d")`; invalid → `field_error("from" | "to", "expected YYYY-MM-DD")`); defaults: the 1st of the current month and today. `from > to` → 400; `to − from` (in days, `(to - from).num_days()`) greater than 365 → 400. SQL: `SELECT day, SUM(spans), SUM(scores), SUM(bytes), SUM(judge_input_tokens), SUM(judge_output_tokens), SUM(judge_cost_usd) FROM usage_daily WHERE org_id = ? AND day >= ? AND day <= ? GROUP BY day ORDER BY day` (in a `meta/usage.rs` function `org_days(pool, org, from, to)`). Today's row has only flushed counts (up to 10 s behind).

OpenAPI per § Conventions: operation `get_org_usage`, tag `orgs`, default security, `AUTH_ERRORS` plus 400 (`QUERY_OR_BODY`), example `fixtures/api/static/org-usage.json`:

```json
{
  "from": "2026-01-01",
  "to": "2026-01-03",
  "days": [
    {
      "day": "2026-01-01",
      "spans": 1200,
      "scores": 40,
      "units": 1240,
      "bytes": 3500000,
      "judge_input_tokens": 52000,
      "judge_output_tokens": 3100,
      "judge_cost_usd": 0.42
    },
    {
      "day": "2026-01-03",
      "spans": 800,
      "scores": 10,
      "units": 810,
      "bytes": 2100000,
      "judge_input_tokens": 0,
      "judge_output_tokens": 0,
      "judge_cost_usd": 0.0
    }
  ]
}
```

### Month usage in the entitlements response

`api/entitlements.rs`:

```rust
/// Usage level against `units_included`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageNotice { None, Warning, Reached }

/// This UTC month's units (D-P-16, D-P-38).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgMonthUsage {
    /// `YYYY-MM`.
    pub month: String,
    /// Units so far this month, flushed or not.
    pub units: u64,
    /// 00:00 UTC on the 1st of the next month.
    #[serde(with = "llmobs_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub resets_at: i64,
    pub notice: UsageNotice,
}
```

`OrgEntitlements` gains `pub usage: OrgMonthUsage` (after `limits`). The notice: `entitlements::units::notice(units, included: Option<u64>) -> UsageNotice` (pure): `None` included or `included == 0` → `None`; otherwise compare in `u128`: `units * 100 >= included * 100` → `Reached`, `>= included * 80` → `Warning`, else `None`. `get_entitlements` calls `state.usage.month_units(&state.meta, org, now)` (a `MetaError` is a 500 as elsewhere). The fixture `org-entitlements.json` gains `"usage": {"month": "2026-01", "units": 1240, "resets_at": "2026-02-01T00:00:00.000000000Z", "notice": "none"}` (`none`, so the banner of P6 does not appear in other page tests).

### Tests

`routes::usage::tests`: two projects on the same day are summed; inclusive bounds; default range; `from > to` 400; `to − from = 366` → 400, `365` → 200; bad date 400; outsider 404. `entitlements::units::tests::notice_levels`: 79 %, 80 %, 100 %, included 0, `u64::MAX` values (no overflow). `routes::entitlements::tests::usage_block`.

### Done

`just check`.

---

## P6 — Usage notices

### Migration

The next free number `NNNN_usage_notices.sql`:

```sql
-- One row per org, month and threshold, so each notice is sent once (D-P-18, D-P-38).
CREATE TABLE usage_notices (
  org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
  month TEXT NOT NULL,
  threshold INTEGER NOT NULL,
  sent_at INTEGER NOT NULL,
  PRIMARY KEY (org_id, month, threshold)
);
```

Regenerate `crates/server/tests/snapshots/meta_schema.snap` (`INSTA_UPDATE=always cargo test -p llmobs-server schema_matches_the_snapshot`, review: one new table), and commit it. Spec 04 § Entitlements gains this table (copy the SQL).

### Task (`crates/server/src/entitlements/notices.rs`, new; `pub(crate) mod notices;`)

```rust
/// Records and sends the month's 80 % / 100 % notices (D-P-38). Returns how many were recorded.
pub(crate) async fn notices_once(state: &AppState) -> Result<u32, MetaError>
```

1. `SELECT id FROM orgs WHERE state = 'active'` (all active orgs, so orgs without a cap are counted too).
2. For each: `included = state.entitlements.resolve(org, now).limits.units_included`; skip when `None` or 0. `units = state.usage.month_units(&state.meta, org, now).await?` (loads and rolls the month over as needed). `level = units::notice(units, included)`.
3. When `level` is `Warning` or `Reached`: the threshold is 80 or 100. Insert the row for the **reached threshold** and, for `Reached`, also the 80 row (`INSERT … ON CONFLICT DO NOTHING`, `rows_affected`); send **one** email for the highest threshold whose row was newly inserted (crossing 80 and 100 in one tick sends only the 100 email).
4. Email: to every owner (`meta::members::org_members`, role owner) through `state.mailer` (`send_in_background`); no mailer → the rows are still written (never retried).

`serve.rs`: a task every 60 s (spawned next to the usage flush task; follow its pattern and its shutdown handling), logging `info!` with the count when non-zero and `warn!` (error kind only) on error.

### Email

`email/mod.rs` `Email::UsageNotice { org_name: String, threshold: u8, units: u64, included: u64, paid: bool, url: String }` — `paid` = `units_cap` is `None` or greater than `included`; `url` = `<public_url>/<org slug>/settings/plan`. Templates `email/templates/usage_notice.txt` and `.html` (register both in the template list; follow `invite.*`), subject `"{PRODUCT}: {org_name} used {threshold}% of its monthly units"`. Text:
- `paid = false`, 80: "Your organization {org_name} has used {units} of its {included} monthly units. At {included} units, ingest stops until the 1st of next month (UTC)."
- `paid = false`, 100: "Your organization {org_name} reached its {included} monthly units. Ingest is paused until the 1st of next month (UTC)."
- `paid = true`, 80: "Your organization {org_name} has used {units} of the {included} units included in its plan this month. Additional units are billed as overage."
- `paid = true`, 100: "Your organization {org_name} has used all {included} units included in its plan this month. Additional units are billed as overage."
Then "See your plan: {url}". Render tests like the existing template tests (each of the four texts).

### Banner (web)

`web/src/components/plan/usage-banner.tsx` (MIT). Rendered in `web/src/routes/$org/route.tsx` `OrgLayout`, inside `TopBarLayout` above `<Outlet />`. Shown only to owners and admins: the role is `meQuery().orgs.find((o) => o.slug === org)?.role` (use the same `me` query the layout already reads, or `$api.useQuery("get", "/api/auth/me")`). It reads `useOrgEntitlements(org)`; `usage.notice === "warning"` → "This organization has used 80% of its monthly units."; `"reached"` with a `limits.units_cap` → "This organization reached its monthly units; ingest is paused until {resets_at day} (UTC)."; `"reached"` without a cap → "This organization used all units included in its plan; additional units are billed as overage."; each with `<UpgradeButton>`. `role="status"`. Tests (`usage-banner.test.tsx`, rendering the app at `/acme/support-bot` with `server.use` overrides): warning; reached with and without cap; hidden for a member role; hidden for `none`.

### Tests (server)

`entitlements::notices::tests` (with `TestApp::with_mailer`): `set_limits({"units_included": 100, "units_cap": 100})`, add 85 units → `notices_once` records 1, one email to the owner (wait: `app.state.mailer` sends in a spawned task — call `tokio::task::yield_now()` a few times or poll `app.sent()` until non-empty with a 1 s timeout); run again → 0, no new email; add 20 → records 1 (100), one email; from 0 straight to 120 in a new month (advance the clock to February) → records 2, **one** email (100). No `units_included` → nothing. A member (not owner) gets no email.

### Done

`just check`.

---

## P7 — Usage export for the control plane

### API types (`api/internal.rs`)

```rust
/// One org's usage on one UTC day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InternalUsageRow {
    pub org_id: Uuid,
    pub day: String,
    pub spans: u64,
    pub scores: u64,
    pub units: u64,
    pub bytes: u64,
    pub judge_input_tokens: u64,
    pub judge_output_tokens: u64,
    pub judge_cost_usd: f64,
}

/// Query of `GET /api/internal/v1/usage`.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct InternalUsageQuery {
    /// First day, inclusive, `YYYY-MM-DD`. Required.
    pub from: String,
    /// Last day, inclusive. Required. `to − from` at most 92 days.
    pub to: String,
    /// 1–10 000, default 1 000.
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}
```

(Check how other query structs declare utoipa params; follow them.)

### Route (`crates/server/src/routes/internal_usage.rs`, new)

`GET /api/internal/v1/usage`, `Access::ControlPlane { scope: "usage" }`. Rows summed per org and day over projects, ordered by `(day, org_id)`, keyset-paged: the cursor is base64url (no padding) of `{"day": "…", "org_id": "…"}` (`deny_unknown_fields`; new private helpers `encode_cursor` / `decode_cursor`; bad cursor → `field_error("cursor", "invalid cursor")`). SQL: `… WHERE day >= ? AND day <= ? AND (day, org_id) > (?, ?) GROUP BY day, org_id ORDER BY day, org_id LIMIT ?` (fetch `limit + 1` to know whether a next page exists). Response `Page<InternalUsageRow>`, registered in `collect_schemas` as a named page: `set.insert_page::<InternalUsageRow>("InternalUsagePage")` (as `InvitePage` is). Rows of dropped orgs stay 40 days after the drop is applied (D-P-46); until P's M4 changes track M's org drop, the drop still deletes them.

OpenAPI per § Conventions: `list_internal_usage`, tag `internal`, bearer-only, errors `400, 401, 403, 429`, `QUERY_OR_BODY`; in `tests/api_examples.rs` `manifest_covers_the_read_operations`, skip operations whose first tag is `internal`.

Tests: two orgs, three days → order; `limit 2` pages through all rows; `to − from = 93` → 400; missing `from` → 400; scope `orgs` → 403; a session → 403; bad cursor → 400.

---

## P8 — Users for the control plane (Q's R14, D-P-39)

### Config

`EntitlementsConfig.control_plane_provider: String`: the `identities.provider` of the control plane's OIDC provider, default `"oidc:cloud"`. `EntitlementsConfig` derives `Default`, which would give `""`: replace the derive with a manual `impl Default` (and keep `#[serde(default)]` on the struct). Validation: non-empty after trimming → else `Invalid { key: "entitlements.control_plane_provider", reason: "must not be empty" }`. Config test: default, a custom value, empty refused. Spec 12's config table gains the row `entitlements.control_plane_provider` | | `"oidc:cloud"`: the identity provider whose subjects the control plane uses (D-P-39).

### Routes (`crates/server/src/routes/internal_users.rs`, new)

Body for all three: `InternalUserRef { subject: String }` (`api/internal.rs`, `deny_unknown_fields`). The user is `meta::identities::identity_user(pool, &config.entitlements.control_plane_provider, &subject)`. All three refuse an instance admin with `403 forbidden` (the control plane never manages operators). Scope `users`, tag `internal`, bearer-only.

- `POST /api/internal/v1/users/revoke` → in one transaction `web_sessions::delete_user_sessions(conn, user, None)` and `personal_tokens::revoke_user_tokens(conn, user, now)`; after the commit evict each returned token hash from `state.pat_cache` (`evict(hash)`, as `routes/me.rs` does). Unknown subject → 404. 204.
- `POST /api/internal/v1/users/disable` → revoke as above plus `users::set_disabled(conn, user, Some(now), now)` (signature `set_disabled(conn, id, disabled_at, now)`) in the same transaction. Unlike the instance admin's disable, it does **not** refuse the last active owner of an org (the control plane closes accounts; the org stays). Unknown subject → 404. 204.
- `POST /api/internal/v1/users/delete` → in one transaction: collect `sole_owner_of` (orgs where the user is an owner and `count_owners == 1`); collect the user's personal-token hashes (`SELECT token_hash FROM personal_tokens WHERE user_id = ?`); `UPDATE invites SET invited_by = NULL WHERE invited_by = ?`; `UPDATE api_keys SET created_by = NULL WHERE created_by = ?`; `DELETE FROM users WHERE id = ?` (the other references cascade or set NULL). After the commit evict the hashes. Audit `tracing::info!(target: "audit", user_id = %id, actor = "control_plane", "user deleted")`. Response 200 `InternalUserDeleted { sole_owner_of: Vec<Uuid> }` (those orgs are kept, without an owner, for the control plane to resolve). An unknown subject answers 200 `{"sole_owner_of": []}` (idempotent: a retry after a lost response must not fail; the control plane keeps its own record of the orgs).

OpenAPI per § Conventions: `post_internal_user_revoke`, `post_internal_user_disable` (errors `400, 401, 403, 404, 429`), `post_internal_user_delete` (errors `400, 401, 403, 429`), `QUERY_OR_BODY`, register `InternalUserRef`, `InternalUserDeleted`.

### Tests

Insert a user with an `identities` row (`meta::identities::insert_identity` with provider `oidc:cloud`), a session, a PAT (`TestApp::personal_token`), and memberships.
- revoke: the session cookie → 401 afterwards; the PAT → 401 (cache evicted); the user can still be found.
- disable: as revoke, plus `disabled_at` set; the user is the last owner of an org → still 204.
- delete: the user is sole owner of org A and co-owner of org B (with another owner) → `{"sole_owner_of": [A]}`; the user row is gone; `PRAGMA foreign_key_check` returns no rows; an invite they sent and an API key they created still exist with NULL references; a second call → 200 `[]`.
- instance admin → 403 on each; unknown subject → 404 (revoke, disable); scope `usage` → 403.

---

## P9 — Memberships for the control plane (Q's R6, D-P-40)

Scope `members`, tag `internal`, bearer-only. Types in `api/internal.rs`:

```rust
pub struct MembershipLookupRequest { pub subject: String, pub email: String }            // deny_unknown_fields
pub struct MembershipOrg { pub org_id: Uuid, pub slug: String, pub role: OrgRole }
pub struct MembershipLookup { pub user_id: Option<Uuid>, pub orgs: Vec<MembershipOrg> }  // user_id omitted when None
pub struct InternalMember { pub user_id: Uuid, pub email: String, #[serde(skip_serializing_if = "Option::is_none")] pub name: Option<String>, pub role: OrgRole }
pub struct InternalMembers { pub data: Vec<InternalMember> }
```

- `POST /api/internal/v1/memberships/lookup` (`routes/internal_members.rs`): the user of `(control_plane_provider, subject)`, else the user with `normalize_email(email)` (an invalid email → 400 `field_error("email", "invalid email")`), else `{"orgs": []}`. Otherwise the user's memberships: `meta::orgs::orgs_of_user` (it returns orgs in every state, ordered by name), keep `state == "active"`, sort by slug in Rust. 200.
- `GET /api/internal/v1/orgs/{org_id}/members?role=owner,admin`: `{org_id}` is a UUID path parameter (`OrgIdPath` from M2); unknown or inactive org → 404; `role` optional, comma-separated `owner`, `admin`, `member`, `viewer`; any other value → 400 `field_error("role", "unknown role")`. Members ordered by email. The email goes only to the control plane (the account system); never log it.

OpenAPI per § Conventions: `post_internal_membership_lookup` (errors `400, 401, 403, 429`), `list_internal_org_members` (errors `400, 401, 403, 404, 429`, `QUERY_OR_BODY`).

Tests: lookup by subject; by email when the subject is unknown; unknown both → empty; invalid email 400; members filter by role; bad role 400; unknown org 404; scope `users` → 403.

---

## P10 — Usage on the Plan page

- `web/src/lib/format.ts`: `export function formatBytes(bytes: number): string` — `B`, `KB`, `MB`, `GB`, `TB` with base 1000, one decimal above `B` (`999 B`, `3.5 MB`), with tests in `format.test.ts`.
- `web/src/api/queries-settings.ts`: `orgUsageQuery(org)` (`GET /api/v1/orgs/{org}/usage` with no query, the default range).
- `web/src/routes/$org/settings/plan.tsx` gains a "Usage this month" section above the features:
  - a bar (`role="meter"`, `aria-label="Units this month"`, `aria-valuenow` = units, `aria-valuemax` = the base) of `usage.units` against `limits.units_included`, or `limits.units_cap` when there is no included value; with neither, only the text. Marks at 80 % and 100 %. Text: `"{units} of {base} units"` with `new Intl.NumberFormat("en-US").format`, and "Resets on {resets_at day} (UTC)." For a capped org: "Ingest pauses at {cap} units."
  - a daily chart: stacked bars of spans and scores per day from `orgUsageQuery`, with `ChartContainer` from `web/src/components/ui/chart.tsx` and Recharts' `BarChart` as the metrics dashboard does, `aria-label="Daily units"`; below it a table `aria-label="Usage totals"` with the month's spans, scores, units, bytes (`formatBytes`) and judge cost (`formatCost` from `format.ts`). No days → "No usage this month." instead of chart and table.
- Tests (`plan.test.tsx`): meter with the fixture (`aria-valuenow` 1240, text "1,240 of 200,000 units"); no `units_included` and no cap → text only; the totals table sums the fixture days (units 2,050; bytes "5.6 MB"); `server.use` with `days: []` → "No usage this month."

---

## P11 — Python SDK pauses on 402 (D-P-34)

- `sdks/python/src/llmobs/_quota.py` (new):

```python
class QuotaState:
    """Shared by the trace exporter and the score queue (D-P-34)."""
    def __init__(self, clock: Callable[[], float] = time.monotonic, wall: Callable[[], float] = time.time) -> None: ...
    def paused(self) -> bool: ...                        # clock() < until
    def on_402(self, retry_after: str | None) -> None:   # pause min(seconds, 3600); warn once per pause start
```

  `on_402`: `seconds = int(retry_after)` if it parses and is > 0, else 3600; `self.until = clock() + min(seconds, 3600)`; when not already paused, log the SDK warning (§ Sub-phases, `<reset>` = `wall() + seconds`, UTC) through the SDK's logger (`llmobs._log`).
  `class QuotaPauseExporter(SpanExporter)`: wraps an exporter; `export` returns `SpanExportResult.SUCCESS` without calling it while `state.paused()`; else delegates. `shutdown` and `force_flush` delegate.
- `_runtime.py` `create`: build one `QuotaState`; a `requests.Session()` whose `hooks["response"]` gets a function calling `state.on_402(response.headers.get("Retry-After"))` when `response.status_code == 402`; pass `session=` to `OTLPSpanExporter` (supported by `opentelemetry-exporter-otlp-proto-http`); wrap the masking exporter in `QuotaPauseExporter(state, …)`. Pass the same `state` to the score queue.
- `_scores.py` `ScoreQueue.__init__` gains a keyword `quota: QuotaState | None = None` (None → its own `QuotaState()`); `_runtime.py` passes the shared one; existing constructors in `tests/test_scores.py` keep working. In `_send`: when `quota.paused()`, drop the batch without sending; a 402 response is handled **before** the existing non-retryable-status warning: `quota.on_402(...)`, drop the batch, no other warning. The pause warning is logged with `logger.warning` (not `warn_once`, which is once per process).
- Tests (`sdks/python/tests/test_quota.py`, with the existing `http_recorder` fixture and its `queue_response` from `tests/conftest.py`): a 402 with `Retry-After: 60` → the next span export sends nothing (`clock` injected: a list-backed fake); after advancing the fake clock 60 s it sends; `Retry-After: 86400` pauses one hour, not a day; one warning per pause; a score 402 pauses the trace exporter too.

Spec 10 (`docs/spec/10-sdk.md`, the exporter section): add "After a `402`, the exporter and score writes drop data for `Retry-After` seconds, at most one hour at a time, then send again; one warning per pause (D-P-34)."

`just check-py`.

---

## P12 — TypeScript SDK pauses on 402 (D-P-34)

- `sdks/typescript/src/quota.ts` (new): `class QuotaState { constructor(now: () => number = Date.now) ; paused(): boolean; on402(retryAfter: string | null): void }` with the same rules as Python (milliseconds; at most one hour; one warning per pause start; `log.ts` gains `export function warn(message: string): void` (through `runtimeGlobals().console?.warn`, like `warnOnce` but without the once-per-process key) with a test in `log.test.ts`; the exact text of § Sub-phases).
- `exporter.ts` `FetchTraceExporter`: takes a `QuotaState` (constructor option, default a new one using the exporter's injected `this.now`); before fetching, `paused()` → resolve `SUCCESS` without fetching; on a 402 response → `on402(response.headers.get("Retry-After"))`, `SUCCESS` (spans dropped). 402 stays out of `RETRYABLE_STATUSES`.
- `scores.ts`: shares the same `QuotaState` (wire it where the runtime builds both); paused → drop; a 402 → `on402`, drop, resolve without throwing.
- Tests (`quota.test.ts`, `exporter.test.ts`, `scores.test.ts` with the existing fake fetch and an injected `now`): pause and resume, one-hour ceiling, one warning per pause, a score 402 pauses exports.

`just check-ts`.

---

## P13 — Close out

- Remove every `allow(dead_code)` attribute M3 added; `just lint` clean.
- Spec 09: the response bodies of the internal endpoints (`InternalUsageRow` page, `InternalUserDeleted`, `MembershipLookup`, `InternalMembers`) in one line each on the internal-endpoints row; spec 11: the Plan page's usage section and the banner (one sentence each). Specs 02, 04 (`usage_notices`, P6), 10 (P11) and 12 (`control_plane_provider`, P8) were updated in their phases; check.
- `docs/roadmap.md` row **P** Status: `**In progress.** M1 and M2 merged. M3 implemented: monthly units and the 402 cap, per-org ingest rate, seats and projects, usage notices, the usage API and export, users and memberships for the control plane, SDK pause on 402. M4 and M5 plans to follow.`
- `just check`, `just e2e`.

## Coordination

- **I (online evaluation):** evaluator scores go through `scores::write`, so at the cap they fail with `quota_exceeded`; I's executor should treat it as a permanent failure of the job (no retry, no rule pause).
- **Not in M3 (for the coordinator):** Q's R7, R13, R16 go to a small auth track; R5 and R17 go to P's M4 once track M is implemented (D-P-43).

## Report

Per sub-phase: what was done, deviations, the migration number used, whether `plan_limit` came from track K or was added here, anything unverified.
