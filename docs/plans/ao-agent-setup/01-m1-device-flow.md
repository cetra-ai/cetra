Status: plan, written 2026-11-21. Not implemented.

# AO · M1 — Device flow, the approval page, `cetra login` on it

This milestone replaces the localhost-callback CLI login with a device authorization flow (RFC 8628):
- the server hands out a code and holds the grant in memory;
- the web page `/device` approves it, and picks the project for `purpose: init`;
- `cetra login` polls for the token.

The callback, `/cli-auth`, the loopback CSP entry and the `cli-auth` reserved slug are deleted.

The design is `docs/plans/ao-agent-setup/00-design.md` § Device flow (M1) and § M1. D-AO-3 to D-AO-6 apply. **Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**; do not improvise.

**Check rule** (user, 2026-11-20): phases are committed without compiling or running tests. After D4 the coordinating session regenerates the generated files and runs `just check` once:
- `just openapi` (`crates/server/openapi.json`, `web/src/api/schema.gen.ts`);
- `just api-examples` (`fixtures/api/manifest.json`);
- `just docs-gen` (the CLI reference).

No phase edits those generated files by hand.

## Prerequisites

- The design doc is approved (2026-11-21).
- M2 is independent; the two touch different files.

## Read first

- `AGENTS.md` (§ Rules, § Errors, § Safety, § Privacy, § Tests).
- Spec 08 § CLI login (line 158), the CSRF rule (line 41), rate limits (line 27) and § Access. Spec 09 § Conventions (errors), § Auth, § CLI. Spec 11 line 12. Spec 12 line 165.
- The session note `ao-m1-code-facts.md` in `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/`. It holds the exact files, lines and helper names this plan refers to: § 0 (traps), § 1 (routes), § 2 (PAT and key creation), § 4 (limits), § 5 (`AppInner`), § 6 (OpenAPI and fixtures), § 9 (test helpers), § 11–12 (web) and § 14 (CLI).
- `crates/server/src/routes/tokens.rs` and `routes/api_keys.rs`: how a PAT and a key are minted. The new handler mints them the same way.
- `crates/server/src/ee/org_sso/login.rs` `discover`: a public route with a per-IP limit.
- `web/src/routes/cli-auth.tsx` and `web/src/routes/setup/workspace.tsx`: the page shape, and how a page creates a project.
- `crates/cli/src/login.rs`, `credentials.rs` and `test_server.rs` (`Canned`).

## What it is, and what it is not

**When M1 is done:**
- The server has five routes:
  - `POST /api/auth/device/code`;
  - `POST /api/auth/device/token`;
  - `GET /api/auth/device/{user_code}`;
  - `POST /api/auth/device/{user_code}/approve`;
  - `POST /api/auth/device/{user_code}/deny`.

  They sit on an in-memory store with a 10-minute expiry, `slow_down`, a cap of 10 000 pending codes, and a sweep every minute.
- An `init` approval mints a `read` PAT and an ingest-only key for a project where the user may manage settings. A `login` approval mints a `read, write` PAT. Both last 90 days for the PAT, and the key does not expire.
- `/device` in the web UI enters or reads a code, shows the host and the purpose, picks or creates a project for `init`, and approves or denies.
- `cetra login` prints the URL and the code, opens the browser, polls, verifies and saves the token.
- `/cli-auth`, the CLI callback server, the CSP's `http://127.0.0.1:*` and the `cli-auth` slug are gone. Specs 08, 09, 11 and 12, the decisions, the CHANGELOG and the API keys docs page say so.

**Not in M1:**
- the Node CLI (`@cetra-ai/cli`, M3), the first caller of `purpose: init`;
- the control plane's `POST /api/internal/v1/device/codes` and the CLI's `moved` handling (M5);
- the docs page "Set up with your coding agent" (M3).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-AO-3 | Device flow on the cell; the browser picks the project; the session mints the secrets (creating keys stays session-only, D-P-70) |
| D-AO-4 | `cetra login` moves to the device flow; the callback and `/cli-auth` are deleted, with no compatibility path |
| D-AO-5 | The grant's plaintext secrets live in memory only, until the first successful poll or expiry |
| D-AO-6 | `init`: `read` PAT plus `ingest` key; `login`: `read, write` PAT |
| D-L-9 | PAT name `CLI on <host>` (`CLI` without a host); kept for `login`, and `CLI init on <host>` / `CLI init` for `init` (design § Device flow) |
| D141 | CSRF `Origin` check on `/api/auth/` writes; the two public device routes are exempt (design § Device flow, CSRF) |
| D138 | 404 for a resource outside the caller's reach, 403 for a visible one with too low a role |

The decisions this plan settles from the code (recorded in the design on 2026-11-21):
- **Error bodies.** The RFC 8628 errors are problem+json with four new problem codes (`authorization_pending`, `slow_down`, `access_denied` and `expired_token`), all with status 400. Every 4xx in the API is problem+json, and `document_lint` enforces it.
- **Permission.** Approving for a project needs `ProjectPerm::ManageSettings`, the permission behind `POST …/api-keys`.
- **Credentials key.** The CLI saves the token under the URL the user passed (`server_url(raw)`), as today. `DeviceGrant.server` is the server's `public_url` origin; the Node CLI (M3) uses it for `CETRA_BASE_URL`.

## Versions

No new dependency. `rand` and `base64` leave `crates/cli/Cargo.toml` if nothing else in `crates/cli/src` uses them after D4 (code facts § 0.12); they stay in the workspace for other crates.

## Phases

```
D1 (store, codes, limits) ──► D2 (routes, API types, OpenAPI, specs) ──┬──► D3 (web /device)
                                                                       └──► D4 (cetra login)
```

D3 and D4 do not depend on each other and touch disjoint files; they may run in parallel after D2. Files per phase:
- D1: `crates/server/src/auth/{device.rs (new), mod.rs, limits.rs}`, `crates/server/src/api/common.rs`, `crates/server/src/http/{error.rs, mod.rs}`, `crates/server/src/serve.rs`, `crates/server/src/test_support/mod.rs`.
- D2: `website/src/content/docs/docs/reference/api/overview.mdx`, `crates/server/src/routes/{device.rs (new), mod.rs}`, `crates/server/src/api/{device.rs (new), mod.rs, openapi.rs}`, `crates/server/src/http/{access.rs, csrf.rs, access_suite.rs}`, `crates/server/src/meta/orgs.rs`, `crates/server/src/api_examples.rs`, `crates/server/tests/{openapi.rs, api_examples.rs}`, `fixtures/api/static/device-request.json` (new), `docs/spec/08-auth.md`, `docs/spec/09-api.md`.
- D3: `web/src/routes/device.tsx` and `device.test.tsx` (new); `web/src/routes/cli-auth.tsx`, `cli-auth.test.tsx`, `web/src/lib/cli-auth.ts` and `cli-auth.test.ts` (deleted); `web/src/lib/{external-navigation.ts, external-navigation.test.ts, redirect.test.ts}`; `web/src/routes/invite/$token.test.tsx`; `web/src/routes/login.test.tsx` (only if it sets `submit`); `web/src/mocks/writes.ts`; `crates/ui/src/lib.rs`; `docs/spec/11-ui.md`; `docs/spec/12-operations.md`.
- D4: `crates/cli/src/{login.rs, main.rs}`, `crates/cli/Cargo.toml`, `Cargo.lock`, `docs/spec/08-auth.md` (the CLI login bullet only), `docs/spec/09-api.md` (the CLI row only), `docs/mvp-decisions.md`, `CHANGELOG.md`, `website/src/content/docs/docs/platform/api-keys.mdx`, `docs/roadmap.md`.

D2 and D4 both edit specs 08 and 09, in different paragraphs. D4 starts from D2's commit, so there is no conflict.

---

## D1 — The device store, its problem codes and its limiter

**Goal:** an in-memory store of device codes with every state transition, unit-tested with the manual clock, wired into `AppInner` and swept every minute.

**Edit:**
- `crates/server/src/auth/device.rs` (new)
- `crates/server/src/auth/mod.rs`
- `crates/server/src/auth/limits.rs`
- `crates/server/src/api/common.rs`
- `crates/server/src/http/error.rs`
- `crates/server/src/http/mod.rs`
- `crates/server/src/serve.rs`
- `crates/server/src/test_support/mod.rs`

**Steps.**
1. **Problem codes** (`api/common.rs`). Add four variants to `ProblemCode`, after `Conflict`. Serde already renames them to snake_case:

   ```rust
   /// Device flow: the user has not approved or denied the code yet (RFC 8628 § 3.5).
   AuthorizationPending,
   /// Device flow: polled faster than the interval; the interval grew by 5 s.
   SlowDown,
   /// Device flow: the user denied the code.
   AccessDenied,
   /// Device flow: the code expired, was already used, or is unknown.
   ExpiredToken,
   ```

   In `status`, they return `400`. In `reason_phrase`, they return `"Bad Request"`.

   Find every other exhaustive `match` on `ProblemCode` with `grep -rn "ProblemCode::RestoreMode" crates/` and add the four variants there with the same meaning. If one of those places is not obviously a status, a title or a phrase, stop and report.
2. **`ApiError`** (`http/error.rs`). Add a variant:

   ```rust
   /// A device-flow poll that has no grant yet, or never will (400 with an RFC 8628 code).
   #[error("device grant")]
   DeviceGrant {
       /// One of `AuthorizationPending`, `SlowDown`, `AccessDenied`, `ExpiredToken`.
       code: ProblemCode,
   },
   ```

   In `problem()` it maps to `Problem::new(*code, None)`, as `Forbidden { code }` does.
3. **Limiters** (`auth/limits.rs`). Add two fields to `Limits`, set them in `Limits::new`, and prune them in `Limits::prune` like `sso_discover`:
   - `pub(crate) device_code: KeyedLimiter<IpAddr>`: `KeyedLimiter::new(Bucket { capacity: 10, per: Duration::from_secs(60) }, MAX_KEYS)`. Doc comment: "`POST /api/auth/device/code`: 10 a minute per client IP, an IPv6 address counted by its /64 (D-AO-3)."
   - `pub(crate) device_lookup: KeyedLimiter<Uuid>`: `Bucket { capacity: 30, per: Duration::from_secs(60) }`. Doc comment: "Device-code lookups, approvals and denials: 30 a minute per user, so a signed-in user cannot enumerate user codes (D-AO-3)."

   Add, in `limits.rs`, the function the `device_code` call site uses for its key:

   ```rust
   /// The limiter key of a client address: an IPv6 address counts by its /64, since one
   /// host usually holds the whole /64; an IPv4 address counts by itself.
   pub(crate) fn network_key(ip: IpAddr) -> IpAddr {
       match ip {
           IpAddr::V4(_) => ip,
           IpAddr::V6(v6) => {
               let s = v6.segments();
               IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
           }
       }
   }
   ```
4. **The store** (`auth/device.rs`, new; add `pub(crate) mod device;` to `auth/mod.rs` in alphabetical order). Write these contracts exactly:

   ```rust
   //! The device authorization store (RFC 8628, D-AO-3, D-AO-5): pending codes and the
   //! grants waiting for their CLI. In memory only: a restart loses pending codes, and the
   //! plaintext secrets of a grant never reach the disk or a log.

   use std::collections::HashMap;
   use std::fmt;
   use std::sync::Mutex;

   use cetra_core::ids::ProjectId; // use the path the rest of the crate uses for ProjectId

   use crate::auth::secret::{hash_token, new_opaque_token, random_bytes};
   use crate::auth::AuthError;

   /// A code lives 10 minutes.
   pub(crate) const CODE_TTL_NS: i64 = 600 * 1_000_000_000;
   /// The first polling interval, in seconds.
   pub(crate) const POLL_INTERVAL_S: u64 = 5;
   /// What `slow_down` adds to a code's interval, in seconds.
   pub(crate) const SLOW_DOWN_STEP_S: u64 = 5;
   /// How long an approval may take between `claim` and `complete`.
   pub(crate) const APPROVING_TTL_NS: i64 = 60 * 1_000_000_000;
   /// At most this many codes at once (pending, being approved or approved and not collected).
   pub(crate) const MAX_CODES: usize = 10_000;
   /// The user-code letters: no vowels, so a code never spells a word (RFC 8628 § 6.1).
   pub(crate) const USER_CODE_ALPHABET: &[u8; 20] = b"BCDFGHJKLMNPQRSTVWXZ";

   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub(crate) enum Purpose { Login, Init }

   /// What `issue` hands to the CLI. `Debug` hides the device code.
   pub(crate) struct Issued {
       pub(crate) device_code: String, // plaintext, 43 base64url characters
       pub(crate) user_code: String,   // "XXXX-XXXX"
   }

   /// The project of an `init` grant.
   #[derive(Clone, Debug, PartialEq, Eq)]
   pub(crate) struct GrantProject {
       pub(crate) id: ProjectId,
       pub(crate) org_slug: String,
       pub(crate) slug: String,
       pub(crate) name: String,
   }

   /// The secrets minted at approval. `Debug` prints `Grant { token: "<redacted>", project, ingest_key: Some("<redacted>") }`.
   pub(crate) struct Grant {
       pub(crate) token: String,
       pub(crate) project: Option<GrantProject>,
       pub(crate) ingest_key: Option<String>,
   }

   /// A pending code as the approval page sees it.
   #[derive(Clone, Debug, PartialEq, Eq)]
   pub(crate) struct Pending {
       pub(crate) purpose: Purpose,
       pub(crate) host: Option<String>,
       pub(crate) created_at: i64,
   }

   /// The answer to one poll. `Debug` is derived: `Grant`'s own `Debug` hides the secrets.
   #[derive(Debug)]
   pub(crate) enum Poll { Pending, SlowDown, Denied, Expired, Granted(Grant) }

   #[derive(Debug, thiserror::Error)]
   pub(crate) enum DeviceError {
       /// `MAX_CODES` codes exist.
       #[error("too many device codes")]
       Full,
       /// The code is unknown, expired, or not in the state the call needs.
       #[error("device code not found")]
       NotFound,
       #[error(transparent)]
       Auth(#[from] AuthError),
   }

   pub(crate) struct DeviceStore { inner: Mutex<Inner> }

   impl DeviceStore {
       pub(crate) fn new() -> Self;
       /// A new pending code.
       pub(crate) fn issue(&self, purpose: Purpose, host: Option<String>, now: i64) -> Result<Issued, DeviceError>;
       /// A pending, unexpired code; `None` otherwise.
       pub(crate) fn pending(&self, user_code: &str, now: i64) -> Option<Pending>;
       /// Pending → approving. While approving (at most `APPROVING_TTL_NS`), `deny` and `claim`
       /// refuse it and a poll answers `Pending`. An approval older than that falls back to
       /// pending: its handler was dropped (a closed tab) and never called `release` or `complete`.
       pub(crate) fn claim(&self, user_code: &str, now: i64) -> Result<Pending, DeviceError>;
       /// Approving → pending, after the minting failed.
       pub(crate) fn release(&self, user_code: &str);
       /// Approving → approved, with the grant.
       pub(crate) fn complete(&self, user_code: &str, grant: Grant) -> Result<(), DeviceError>;
       /// Pending → denied.
       pub(crate) fn deny(&self, user_code: &str, now: i64) -> Result<(), DeviceError>;
       /// One poll by the CLI.
       pub(crate) fn poll(&self, device_code: &str, now: i64) -> Poll;
       /// Drops expired entries that are not approving.
       pub(crate) fn sweep(&self, now: i64);
   }

   /// `XXXX-XXXX` or `xxxxxxxx` or `xxxx-xxxx` → `XXXXXXXX` when it is 8 alphabet letters.
   pub(crate) fn normalize_user_code(raw: &str) -> Option<String>;
   ```

   Make the `use` paths compile against the crate as it is: find where `ProjectId` and `AuthError` are imported elsewhere in `crates/server/src/auth/`, and use those paths. `#[derive(Debug)]` is not allowed on `Issued` or `Grant`: write `Debug` by hand so that no secret is printed.
5. **The internals.**
   - **`Inner`** holds:
     - `by_device: HashMap<String, Entry>`, keyed by `hash_token(device_code)`;
     - `by_user: HashMap<String, String>`, from the normalized user code to that hash.
   - **`Entry`** has `user_code` (normalized), `purpose`, `host`, `created_at`, `expires_at`, `interval_s: u64`, `last_poll: Option<i64>` and `state: State`.
   - **`State`** is `Pending`, `Approving { since: i64 }`, `Denied` or `Approved(Grant)`.
   - **Stale approvals.** Every method that takes `now` (`pending`, `claim`, `deny`, `poll`, `sweep`) first turns an `Approving { since }` with `now >= since + APPROVING_TTL_NS` back into `Pending`. `complete` takes no `now`, and accepts any approving entry, because its handler is still running.
   - **The lock.** Lock the mutex with a helper that recovers a poisoned lock (`into_inner` on the poison error), as `auth/limits.rs` does with its private `lock`. Copy that helper into `device.rs`; do not change `limits.rs` for it.
6. **`issue`:**
   1. If `by_device.len() >= MAX_CODES`, run the sweep logic first. If it is still full, return `DeviceError::Full`.
   2. `let token = new_opaque_token()?;` gives the device code plaintext and its hash.
   3. **The user code.** Draw letters from `random_bytes(16)?`, keep a byte `b` only when `b < 240`, and map it to `USER_CODE_ALPHABET[(b % 20) as usize]`. Draw more bytes while fewer than 8 letters are kept.
   4. If the user code is already in `by_user`, draw again, at most 5 times. Then return `DeviceError::Full`, because that can only mean a broken RNG or a full table.
   5. Insert the entry with `expires_at = now + CODE_TTL_NS`, `interval_s = POLL_INTERVAL_S`, `last_poll = None` and `State::Pending`.
   6. Return `Issued { device_code, user_code: "<first four>-<last four>" }`.
7. **`pending` / `claim` / `deny`:**
   1. Normalize the code; an invalid one is `None` or `NotFound`.
   2. Find the entry. A missing entry, or one with `now >= expires_at` and not `Approving`, is `None` or `NotFound`.
   3. `pending` returns `Some` only in `State::Pending`.
   4. `claim` moves `Pending` to `Approving { since: now }` and returns the `Pending` view; in any other state it returns `NotFound`.
   5. `deny` moves `Pending` to `Denied`; in any other state it returns `NotFound`.
8. **`release`** moves `Approving` back to `Pending` and does nothing in any other state. **`complete`** moves `Approving` to `Approved(grant)`; in any other state, or for an unknown code, it returns `NotFound`.
9. **`poll(device_code, now)`:**
   1. Look up `hash_token(device_code)`. Missing → `Expired`.
   2. If `now >= expires_at` and the state is `Pending` (after the stale-approval rule), remove the entry from both maps and return `Expired`. A `Denied` entry answers `Denied` even after its expiry; an approving or approved one is never expired here.
   3. **`Approved`:** remove the entry from both maps and return `Granted(grant)`. The grant leaves the store at that moment, so a second poll gets `Expired`.
   4. **`Denied`:** remove the entry and return `Denied`.
   5. **`Pending` or `Approving`:** if `last_poll` is `Some(t)` and `now - t < i64::try_from(interval_s).unwrap_or(i64::MAX).saturating_mul(1_000_000_000)` (`unwrap_or` is allowed; it is not `unwrap`), set `interval_s += SLOW_DOWN_STEP_S` and `last_poll = Some(now)`, and return `SlowDown`. Otherwise set `last_poll = Some(now)` and return `Pending`.
10. **`sweep(now)`:**
    - Apply the stale-approval rule, then remove every entry whose `expires_at <= now` and whose state is `Pending` or `Denied`.
    - Also remove an `Approved` entry whose `expires_at + CODE_TTL_NS <= now`: a grant nobody collected within 10 minutes after the expiry is dropped.

    `by_user` is kept in step.
11. **Wiring.**
    - Add `pub(crate) device: crate::auth::device::DeviceStore` to `AppInner` (`http/mod.rs`), after `limits`.
    - Set it to `DeviceStore::new()` in both places that build `AppInner`: `serve.rs` (about line 468) and `test_support/mod.rs` (about line 381).
    - In `spawn_prune` (`serve.rs`), after `state.limits.prune(now)`, call `state.device.sweep(now)` with the same `now`. Bind `let now = state.clock.now_ns();` once.
12. Never log a device code, a user code, a token or a key. Log nothing in `device.rs`.

**Tests** (`#[cfg(test)] mod tests` in `auth/device.rs`, plain functions and the `now` argument, no `TestApp`):
- `issue` returns a 43-character device code and a user code matching `^[BCDFGHJKLMNPQRSTVWXZ]{4}-[BCDFGHJKLMNPQRSTVWXZ]{4}$`; 1 000 issues give 1 000 distinct user codes.
- `normalize_user_code` accepts `bcdf-ghjk`, `BCDFGHJK` and `BCDF-GHJK`, and refuses `BCDF-GHJ`, `BCDF-GHJA` (a vowel), `BCDF GHJK` and the empty string.
- **Polling.** A fresh code polls `Pending`. A second poll 1 s later is `SlowDown`. A poll 10 s after that is `Pending`, because the interval is now 10 s. A poll 5 s after that is `SlowDown`.
- **Approve and collect.** `claim`, `complete` then `poll` gives `Granted` with the same token. A second `poll` gives `Expired`.
- **While approving.** A poll answers `Pending`. `deny` returns `NotFound`. With a code claimed 30 s before its expiry, `sweep` 10 s after the expiry keeps the entry, and `complete` still succeeds.
- **Stale approval.** `claim`, then 61 s later a second `claim` succeeds: the first approval was dropped. `sweep` past the expiry and 61 s after a `claim` removes the entry.
- **Denied late.** `deny`, then a poll after the expiry gives `Denied`.
- **Release.** `claim` then `release` gives `Pending` again, and a second `claim` works.
- **Deny.** `deny` then `poll` gives `Denied`, and the next poll gives `Expired`.
- **Expiry.** After `CODE_TTL_NS`, `pending` is `None`, `claim` is `NotFound`, and `poll` is `Expired`.
- **Sweep.** It removes expired pending entries. It removes an approved entry only after `expires_at + CODE_TTL_NS`.
- **Full.** With `MAX_CODES` entries, `issue` is `Full`. After they expire, `issue` succeeds, because it sweeps first. Build the full table with a test-only loop of `issue` calls.
- **`Debug`.** `format!("{:?}", grant)` and `format!("{:?}", issued)` contain neither the token, the key nor the device code.
- **Limiters** (`auth/limits.rs`'s tests):
  - the 11th `device_code.check` from one IP within a minute is refused, and another IP passes;
  - `network_key` maps `2001:db8:1:2:3:4:5:6` and `2001:db8:1:2:ff::1` to the same key, and leaves `192.0.2.1` alone;
  - the 31st `device_lookup.check` for one user id within a minute is refused.
- **Problem codes.**
  - `api/common.rs`'s tests: the four new codes have status 400 and phrase "Bad Request".
  - `http/error.rs`'s tests: `ApiError::DeviceGrant { code: ProblemCode::SlowDown }` gives a 400 problem with code `slow_down`.

**Done when:** the code is written as above and committed. It is compiled and tested in the final check (check rule). D1 alone would not pass `just lint`: the store and the limiters are unused until D2.

**Commit:** `Add the device authorization store`

---

## D2 — The device routes, their API types, OpenAPI and specs

**Goal:** the five routes, with their access, CSRF exemption, OpenAPI entries, example fixture, tests and spec text.

**Edit:**
- `crates/server/src/api/device.rs` (new)
- `crates/server/src/api/mod.rs`
- `crates/server/src/api/openapi.rs`
- `crates/server/src/routes/device.rs` (new)
- `crates/server/src/routes/mod.rs`
- `crates/server/src/http/access.rs`
- `crates/server/src/http/csrf.rs`
- `crates/server/src/http/access_suite.rs`
- `crates/server/src/meta/orgs.rs`
- `crates/server/src/api_examples.rs`
- `crates/server/tests/openapi.rs`
- `crates/server/tests/api_examples.rs`
- `fixtures/api/static/device-request.json` (new)
- `docs/spec/08-auth.md`
- `docs/spec/09-api.md`
- `website/src/content/docs/docs/reference/api/overview.mdx`

**Steps.**
1. **API types** (`api/device.rs`; `pub mod device;` in `api/mod.rs`, alphabetical). Every type derives `Debug` (except where hand-written below), `Clone`, `Serialize`, `Deserialize` and `ToSchema`. Requests use `#[serde(deny_unknown_fields)]`.

   ```rust
   /// Who asks for a code: `cetra login` (or `cetra-cli login`), or `cetra-cli init`.
   #[serde(rename_all = "snake_case")]
   pub enum DevicePurpose { Login, Init }

   pub struct DeviceCodeRequest {
       pub purpose: DevicePurpose,
       /// The machine's name as the CLI reports it; shown on the approval page.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub host: Option<String>,
   }

   pub struct DeviceCodeResponse {
       /// Hand-written Debug: prints `<redacted>` for this field.
       pub device_code: String,
       pub user_code: String,
       pub verification_uri: String,
       pub verification_uri_complete: String,
       /// Seconds.
       pub expires_in: u64,
       /// Seconds between polls.
       pub interval: u64,
   }

   pub struct DeviceTokenRequest {
       /// Hand-written Debug: `<redacted>`.
       pub device_code: String,
   }

   pub struct DeviceGrantProject {
       pub id: ProjectId,       // the type other API structs use for project ids
       pub org_slug: String,
       pub slug: String,
       pub name: String,
   }

   /// Hand-written Debug: `token` and `ingest_key` print `<redacted>`.
   pub struct DeviceGrant {
       /// The server's public URL origin: what the CLI and the SDK talk to.
       pub server: String,
       /// A personal access token, `pat-cetra-…`, valid 90 days.
       pub token: String,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub project: Option<DeviceGrantProject>,
       /// A secret key with the `ingest` scope only, `sk-cetra-…`.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub ingest_key: Option<String>,
   }

   pub struct DeviceRequest {
       pub purpose: DevicePurpose,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub host: Option<String>,
       #[serde(with = "time_serde")]
       #[schema(value_type = String, format = DateTime)]
       pub created_at: i64,
   }

   pub struct DeviceApproveRequest {
       /// Required for `init`, refused for `login`.
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub project_id: Option<ProjectId>,
   }
   ```

   Copy the `time_serde` import and the `ProjectId` type from `api/keys.rs` or `api/account.rs`, whichever uses them.

   In `routes/device.rs`, write three private conversion functions:
   - `purpose_in(DevicePurpose) -> Purpose`;
   - `purpose_out(Purpose) -> DevicePurpose`;
   - `project_out(GrantProject) -> DeviceGrantProject`, field by field: `id`, `org_slug`, `slug`, `name`.
2. **Routes** (`routes/device.rs`; `pub(crate) mod device;` in `routes/mod.rs`, alphabetical; `routes.extend(crate::routes::device::routes())` in `http/access.rs` `routes()`, after the `auth` line). Five `RouteDef`s, all `documented: true`:

   | Method, path | Access | Handler |
   |---|---|---|
   | `POST /api/auth/device/code` | `Access::Public` | `post(issue_code)` |
   | `POST /api/auth/device/token` | `Access::Public` | `post(poll_token)` |
   | `GET /api/auth/device/{user_code}` | `Access::User { session_only: true }` | `get(get_request)` |
   | `POST /api/auth/device/{user_code}/approve` | `Access::User { session_only: true }` | `post(approve)` |
   | `POST /api/auth/device/{user_code}/deny` | `Access::User { session_only: true }` | `post(deny)` |

   **Extractors.** Handlers use `State(state): State<AppState>`, `ClientIp(ip): ClientIp`, `Extension(principal): Extension<Principal>`, `error::Path(user_code): error::Path<String>` and `error::Json(body): error::Json<T>`, as `routes/tokens.rs` does. Never axum's own `Path` or `Json`.

   **The three session handlers** start with `let user = session_user(&principal)?;`, then `state.limits.device_lookup.check(&user.id, now)`. A refusal maps to `ApiError::RateLimited { retry_after_s: retry.0.as_secs().max(1) }`.
3. **`issue_code(State, ClientIp(ip), error::Json<DeviceCodeRequest>)`:**
   1. `state.limits.device_code.check(&network_key(ip), now)`. A refusal returns `ApiError::RateLimited { retry_after_s: retry.0.as_secs().max(1) }`.
   2. **Host.** Keep only `[A-Za-z0-9.-]` characters, truncate to 64 characters, and use `None` when nothing is left. This is the CLI's `sanitise_host` rule; write it here as a private `fn sanitise_host(raw: &str) -> Option<String>`.
   3. `state.device.issue(purpose, host, now)`:
      - `Full` → `ApiError::Unavailable { detail: "too many pending device codes" }`;
      - `Auth(e)` → `ApiError::Internal(Box::new(e))`.
   4. Answer `200` with `DeviceCodeResponse`:
      - `verification_uri = state.config.public_url.join("/device")`;
      - `verification_uri_complete = state.config.public_url.join(&format!("/device?code={user_code}"))`;
      - `expires_in = 600`, `interval = POLL_INTERVAL_S`.
4. **`poll_token(State, error::Json<DeviceTokenRequest>)`:** map `state.device.poll(&device_code, now)`:
   - `Pending` → `ApiError::DeviceGrant { code: ProblemCode::AuthorizationPending }`;
   - `SlowDown` → `SlowDown`;
   - `Denied` → `AccessDenied`;
   - `Expired` → `ExpiredToken`;
   - `Granted(g)` → `200 DeviceGrant { server: state.config.public_url.origin().to_owned(), token: g.token, project: g.project.map(project_out), ingest_key: g.ingest_key }`, with the headers `Cache-Control: no-store` and `Pragma: no-cache` (RFC 6749 § 5.1). The handler returns `Result<Response, ApiError>` and builds that answer as `([(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")], error::Json(grant)).into_response()`.
5. **`get_request`:** after the user and limiter lines, call `state.device.pending(&user_code, now)`. `None` is `ApiError::NotFound`; otherwise answer `200 DeviceRequest { purpose: purpose_out(p.purpose), host: p.host, created_at: p.created_at }`.
6. **`approve`** (state, principal, path, and `error::Json<DeviceApproveRequest>`). It is split in two, so that no `?` can skip the release:

   ```rust
   async fn approve(/* extractors */) -> Result<StatusCode, ApiError> {
       let user = session_user(&principal)?;
       let now = state.clock.now_ns();
       state.limits.device_lookup.check(&user.id, now).map_err(/* RateLimited */)?;
       let pending = state.device.claim(&user_code, now).map_err(|_| ApiError::NotFound)?;
       match mint(&state, user, &pending, body, now).await {
           Ok(minted) => { /* steps 6.4 to 6.6 */ }
           Err(error) => {
               state.device.release(&user_code);
               Err(error)
           }
       }
   }

   /// The grant, the PAT's id, and for `init` the key's id and its project.
   async fn mint(state: &AppState, user: &AuthUser, pending: &Pending, body: DeviceApproveRequest, now: i64)
       -> Result<(Grant, Uuid, Option<(Uuid, ProjectId)>), ApiError>;
   ```

   If the handler future is dropped mid-way, because the tab closed, D1's stale-approval rule puts the code back to pending after 60 s.

   **`mint`** may use `?` freely:
   1. **`Login`.** If `body.project_id` is `Some`, return `field_error("project_id", "only for init")` (`http/error.rs`).
      - Mint a PAT as `routes/tokens.rs` `create_token` does: `secret::new_key(KeyKind::Personal)`, then `personal_tokens::insert_token` with `NewToken { user_id, name, prefix, hash, scopes: vec![KeyScope::Read, KeyScope::Write], expires_at: Some(now + NINETY_DAYS_NS) }`.
      - The name is `CLI on <host>`, or `CLI` without a host.
      - Grant `{ token, project: None, ingest_key: None }`.
   2. **`Init`.** If `body.project_id` is `None`, return `field_error("project_id", "required for init")`.
      - `let access = user_project(state, user, &project_id.to_string(), ProjectPerm::ManageSettings).await?;` answers 404 or 403 per D138.
      - `let org = org_by_id(state.meta.reader(), access.project.org_id).await?`; `None` is `ApiError::NotFound`.
      - In **one** `state.meta.write` transaction, insert:
        - the PAT: scopes `[KeyScope::Read]`, expiry now + 90 days, name `CLI init on <host>` / `CLI init`;
        - the API key: `NewApiKey { project_id: access.project.id, kind: "secret", scopes: vec![KeyScope::Ingest], name: Some(<same name>), prefix, hash, created_by: user.id, expires_at: None }` (`secret::new_key(KeyKind::Secret)`).
      - Grant `{ token, project: Some(GrantProject { id, org_slug: org.slug, slug, name }), ingest_key: Some(key) }`.
   3. A `meta` error returns `ApiError::from(error)`, and a `secret::new_key` error returns `ApiError::Internal(Box::new(e))`.

   **`approve`, on `Ok`:**

   4. `state.device.complete(&user_code, grant)`. A `NotFound` here cannot happen, because `complete` accepts any approving entry. Map it to `ApiError::Internal` anyway.
   5. Audit: `tracing::info!(target: "audit", %token_id, user_id = %user.id, purpose = ?pending.purpose, "device code approved")`. For `init`, add `%key_id` and `project_id = %…`. Never the codes or secrets.
   6. Answer `204`.

   `const NINETY_DAYS_NS: i64 = 90 * 24 * 60 * 60 * 1_000_000_000;` is a private const in `routes/device.rs`.
7. **`deny`:** the user and limiter lines, then `state.device.deny(&user_code, now)`. `NotFound` → `ApiError::NotFound`. Audit `"device code denied"` with `user_id`. Answer `204`.
8. **CSRF** (`http/csrf.rs` `must_check`). Exempt exactly the two public paths, with the comment "The device flow's CLI calls carry no cookie and no Origin (D-AO-3)":

   ```rust
   let path = request.uri().path();
   let device_cli = path == "/api/auth/device/code" || path == "/api/auth/device/token";
   session || (path.starts_with("/api/auth/") && !device_cli)
   ```

   A session request to those two paths is still checked, because `session` is true.
9. **Reserved slugs** (`meta/orgs.rs` `RESERVED`). Replace `"cli-auth"` with `"device"`, keeping the list's order rule. Update the test `slugs_match_the_shape_and_the_reserved_list` the same way.
10. **Access suite** (`http/access_suite.rs` `concrete_path`). Add `.replace("{user_code}", "BCDF-GHJK")` next to the other replacements.
11. **OpenAPI** (`api/openapi.rs`):
    - Insert the eight types (`DevicePurpose`, `DeviceCodeRequest`, `DeviceCodeResponse`, `DeviceTokenRequest`, `DeviceGrantProject`, `DeviceGrant`, `DeviceRequest`, `DeviceApproveRequest`) in `collect_schemas`, with a `use crate::api::device::{…};` at the top of `openapi.rs`.
    - Add five operations in `auth_paths` with tag `auth`, ids `post_device_code`, `post_device_token`, `get_device_request`, `post_device_approve` and `post_device_deny`.
    - Securities: the first two empty, the last three the `session` requirement.
    - Request bodies: `DeviceCodeRequest`, `DeviceTokenRequest` and `DeviceApproveRequest`.
    - Path parameter: `path_string("user_code", "The code shown by the CLI, `XXXX-XXXX`.")`.
    - Responses, with their error codes:
      - `post_device_code`: `200 DeviceCodeResponse`; `400`, `429`, `503`;
      - `post_device_token`: `200 DeviceGrant`; `400`;
      - `get_device_request`: `200 DeviceRequest`; `401`, `403`, `404`, `429`;
      - `post_device_approve`: `204`; `400`, `401`, `403`, `404`, `429`;
      - `post_device_deny`: `204`; `401`, `403`, `404`, `429`.

    Each description is one or two sentences from design § Device flow. `post_device_token`'s description names the four problem codes.
12. **`tests/openapi.rs`:**
    - Add the five `(method, path, id)` rows to `OPERATIONS`, in its sort order.
    - Put `post_device_code` and `post_device_token` in the public security list, and the other three in the session-only list.
    - Add an `else if` arm per id with the error sets of step 11. If an id's set equals the fallthrough default, no arm is needed.
13. **Example fixture:**
    - `fixtures/api/static/device-request.json` is `{"purpose":"init","host":"dev-laptop","created_at":"2026-01-01T00:00:00.000000000Z"}`, canonical JSON as the other static files.
    - In `api_examples.rs`, add `example("get_device_request", params(&[("user_code", "BCDF-GHJK")]), BTreeMap::new(), "static/device-request.json", "application/json")` next to `get_invite_preview`.
    - In `tests/api_examples.rs`, add the arm `"get_device_request" => roundtrip::<DeviceRequest>(&example.file)`.
14. **Spec 08:**
    - **Line 41 (CSRF).** After "…whatever its credentials (…)", add: "except the device flow's `POST /api/auth/device/code` and `POST /api/auth/device/token`, which carry no cookie (D-AO-3)".
    - **Line 27 (rate limits).** Add "Device codes: 10 per minute per client IP."
    - **Line 133 (session-only).** Add: "Approving a device code (`POST /api/auth/device/{user_code}/approve`) mints a PAT, and for `init` an ingest-only key, with the session (D-AO-3)."
    - **New subsection after the CLI login bullet**, `### Device authorization (D-AO-3)`. It holds design § Device flow (M1) as prose and the routes table, with:
      - the codes, polling, approval, scopes and names;
      - the hand-over, including that a grant whose answer was lost leaves its PAT and key on the account and project pages under those names, where the user can revoke them;
      - the bounds and limits: 10 codes a minute per IP or IPv6 /64, 30 lookups a minute per user, 60 s for an approval in progress;
      - the CSRF and phishing rules, and the problem codes.

      Leave the CLI login bullet itself to D4.
15. **Spec 09:**
    - **§ Auth table.** Add the five routes in the existing row style.
    - **§ Conventions, errors.** Add `authorization_pending`, `slow_down`, `access_denied`, `expired_token` (400; device flow) to the list of codes.
    - **Line 10** (the `/api/auth/*` row): add "device flow" to its list.
16. **`website/src/content/docs/docs/reference/api/overview.mdx`.** Add one row per new problem code to its problem-code table (about lines 56–67): status 400, with spec 09's wording.

**Tests:**
- **Inline in `routes/device.rs`** (`TestApp`, `app.seed()`, the manual clock, `TestRequest` with `.origin(None)` on the two public calls so that the CSRF exemption is really exercised):
  - `post_device_code` with no `Origin` answers 200: the URIs start with the public URL, `interval` is 5, `expires_in` is 600.
  - The 11th code from the same `.peer(ip)` within a minute is 429; another IP gets 200; two IPv6 peers of one /64 share the bucket.
  - The 31st `GET /api/auth/device/{code}` by one user within a minute is 429.
  - The grant answer carries `Cache-Control: no-store`.
  - Polling a new code is a 400 problem `authorization_pending`. Polling again at once is `slow_down`. An unknown device code is `expired_token`.
  - **Login flow.** `GET /api/auth/device/{code}` with the seeded cookie is `{purpose: "login", host}`. Approve with `{}` is 204. The poll returns `server` = public origin, a `pat-cetra-` token, and no project or key. `GET /api/auth/me` with that bearer answers 200. The token's row has scopes `read, write`, name `CLI on <host>`, and expires 90 days after `now`. A second poll is `expired_token`.
  - **Init flow.** Approve with the seeded project's id. The grant has the project (`acme`, `support-bot`) and an `sk-cetra-` key. The key's row has scopes `ingest` only and the name `CLI init on <host>`. The PAT has scope `read` only.
  - Init approve without `project_id` is 400 on `project_id`; login approve with one is 400. The code stays pending after both: `GET` still answers 200.
  - Init approve by an org member (role `member`, `app.add_member`) is 403. For a project of another org it is 404: create it with `test_support::second_org(app, slug, owner_email)`, which returns `(OrgId, Member)`, then insert a project in that org the way `second_org`'s callers do. The seeded user is an instance admin, and instance admins have no implicit access to other orgs, so the 404 holds. The code stays pending after both.
  - Deny gives 204, then the poll gives `access_denied`. Deny of an unknown code is 404.
  - A bearer PAT on `GET`, approve and deny is 403, because they are session-only.
  - A session approve with no `Origin` is 403, because CSRF still applies to the session.
  - The logs contain neither the token nor the key. Wrap approve and the poll in `let (grant, logs) = capture_logs(async { … }).await;`, return the `DeviceGrant` from the block, then assert `!logs.contains(&grant.token)` and `!logs.contains(<the key>)`, and that the approve body is empty. `assert_logs_clean` does not fit here: it needs the needles before the block runs.
- **The access suite** covers the five routes automatically once `concrete_path` knows `{user_code}`.
- **`tests/openapi.rs`** and **`tests/api_examples.rs`** cover the contract and the fixture.

**Done when:** written and committed (check rule). The generated `openapi.json`, `schema.gen.ts` and `manifest.json` are regenerated by the coordinating session after D4.

**Commit:** `Add the device authorization routes`

---

## D3 — The `/device` page; `/cli-auth` deleted

**Goal:** the approval page in the web UI, with its tests and mocks. The old page, its helpers and the loopback CSP entry are deleted.

**Edit:**
- `web/src/routes/device.tsx` (new)
- `web/src/routes/device.test.tsx` (new)
- `web/src/routes/cli-auth.tsx`, `cli-auth.test.tsx` (delete)
- `web/src/lib/cli-auth.ts`, `cli-auth.test.ts` (delete)
- `web/src/lib/external-navigation.ts`
- `web/src/lib/external-navigation.test.ts`
- `web/src/lib/redirect.test.ts`
- `web/src/routes/invite/$token.test.tsx`
- `web/src/routes/login.test.tsx` (only if it sets `submit`)
- `web/src/mocks/writes.ts`
- `crates/ui/src/lib.rs`
- `docs/spec/11-ui.md`
- `docs/spec/12-operations.md`

The page uses the types of D2's OpenAPI. `schema.gen.ts` is regenerated after D4 (check rule), so write the calls against the paths and schema names of D2 step 11 exactly.

**Steps.**
1. **`web/src/routes/device.tsx`.** `createFileRoute("/device")`.
   - `validateSearch` turns unknown input into `{ code?: string; project?: string }`. Keep a value only when it is a string; TanStack may parse `123` as a number, so a number becomes `String(value)`. Follow `parseLoginSearch` in `web/src/lib/search/auth.ts`.
   - The page is not public: `isPublicPath` stays as it is, so the root gate sends a signed-out user to `/login?redirect=/device?code=…` and back.
   - The layout is `AuthLayout`, with the title `` `Connect the ${PRODUCT_NAME} CLI` ``.
2. **No `code` in the search.** A form with one text field, "Code" (placeholder `XXXX-XXXX`), and a "Continue" button that navigates to `/device?code=<value>`. It keeps `project` when present.
3. **With a `code`.** Query `$api.useQuery("get", "/api/auth/device/{user_code}", { params: { path: { user_code: code } } }, { retry: false })`.
   - **404:** the text "This code is unknown or has expired. Run the command again to get a new code." and a link "Enter another code" to `/device`.
   - **Loaded:** show:
     - the code in a `<code>` element;
     - "Computer: `<host>`" (or "Computer: not reported");
     - the warning paragraph: "Only continue if you just ran `cetra-cli init`, `cetra-cli login` or `cetra login` on this computer. Approving gives that command access to your account.";
     - for `login`: "This creates a personal access token that can read and write, valid for 90 days.";
     - for `init`: "This creates a read-only personal access token, valid for 90 days, and a key that can only send traces to the project you choose."
4. **Project picker** (`purpose === "init"` only):
   - **Orgs.** `me.orgs` without `demo` orgs, from `meQuery`.
   - **Projects.** One query per org, through TanStack's `useQueries`: `const options = (slug: string) => $api.queryOptions("get", "/api/v1/orgs/{org}/projects", { params: { path: { org: slug }, query: { limit: 1000 } } })`. After a project is created, invalidate with `queryClient.invalidateQueries({ queryKey: options(slug).queryKey })`: `projectListOptions(org)` keys on `limit: 50` and would not refresh this query. The page's tests stub the call with `server.use`, so the dev-mock manifest does not need a `limit=1000` entry.
   - **Eligible.** A project is eligible when its `role` is `admin` or `owner`.
   - **The list.** A `RadioGroup`, grouped by org name, of eligible projects labelled `<org name> / <project name>`.
   - **Preselection.** The project whose `<org slug>/<project slug>` equals `search.project`; else the only eligible project when there is exactly one.
   - **"New project"** button, shown when at least one org has role `owner` or `admin`. It opens an inline form with:
     - a `Select` of those orgs, defaulting to the first;
     - a name field;
     - "Create", which calls `$api.useMutation("post", "/api/v1/orgs/{org}/projects")` with `{ name }`.

     On success, invalidate that org's projects query (above) and select the new project. Errors:
     - `plan_limit`: `problemMessage(error, "Your plan does not allow another project.")`, as `new-project-dialog.tsx` does;
     - 409: "A project with this name already exists. Choose another name." This text is new on purpose: this form sends no slug, so a 409 comes from the name;
     - 403: "Only admins and owners can create projects.";
     - anything else: `problemMessage(error, "Could not create the project.")`.
   - **No eligible project and no org to create in:** "You need the admin role on a project to connect the CLI. Ask an admin of your organization." Approve stays disabled.
5. **Actions:**
   - **Approve** (primary): `$api.useMutation("post", "/api/auth/device/{user_code}/approve", { gcTime: 0 })` with body `{}` for login and `{ project_id }` for init. Disabled for init until a project is selected.
   - **Deny** (secondary): `"/api/auth/device/{user_code}/deny"`.
   - **After success** the page shows only one sentence: "Done. Return to the terminal." after approve, "Cancelled. You can close this window." after deny.
   - **On error** a `FormError` with `problemMessage(error, "Could not approve the code.")`. A 404 shows the unknown-or-expired text of step 3.
6. **Delete** `web/src/routes/cli-auth.tsx`, `cli-auth.test.tsx`, `web/src/lib/cli-auth.ts` and `cli-auth.test.ts`.
7. **`external-navigation.ts`.**
   - Remove `submitExternalForm` and the `submit` member of `ExternalNavigator` and `browserNavigator`. Their only caller was `cli-auth.tsx`.
   - Remove the form test and the `submit:` members from `external-navigation.test.ts`, `routes/invite/$token.test.tsx`, and `routes/login.test.tsx` if it has one.
   - Run `grep -rn "submitExternalForm\|submit:" web/src` to find every other use. If a non-test file still uses them, stop and report.
8. **`redirect.test.ts`.** Change `isPublicPath("/cli-auth")` to `isPublicPath("/device")`; it is still expected to be `false`.
9. **`web/src/mocks/writes.ts`.** It has two structures, and `writes.test.ts` checks both: the `WRITE_OPERATIONS` array (about line 62) and the `RESPONSES` record (about line 540). Add the four ids to `WRITE_OPERATIONS`, after `post_invite_accept`, and four keys to `RESPONSES`:
   - `post_device_code: () => HttpResponse.json({ device_code: "mock-device-code-0000000000000000000000000000", user_code: "BCDF-GHJK", verification_uri: "http://localhost:5173/device", verification_uri_complete: "http://localhost:5173/device?code=BCDF-GHJK", expires_in: 600, interval: 5 })`;
   - `post_device_token: () => problem(400, "authorization_pending")`, with the file's `problem` helper (about line 1065);
   - `post_device_approve: () => noContent()` and `post_device_deny: () => noContent()`.

   `writes.test.ts` needs no edit: it reads `openapi.json` and passes once that is regenerated.
10. **CSP** (`crates/ui/src/lib.rs`).
    - Remove ` http://127.0.0.1:*` from `form-action` in `CONTENT_SECURITY_POLICY_VALUE`, which becomes `form-action 'self'`, and from its test.
    - Update the comment that cites the CLI callback.
11. **Spec 11 line 12.** Replace the `/cli-auth?port=&state=&host=` row with `/device?code=&project=     CLI sign-in: approve a device code; for init, pick or create the project (D-AO-3)`, in that block's column alignment.
12. **Spec 12 line 165.** The CSP becomes `form-action 'self'`; remove the sentence about the loopback callback.

**Tests** (`device.test.tsx`, `renderApp`, `server.use` for every call; the vitest server fails on unhandled requests):
- `/device?code=1234&project=123/456` keeps both values as strings (`"1234"`, `"123/456"`), and `?project=1e3` gives the string `"1e3"`. Read the raw query string with `router.history.location.search` and `URLSearchParams`, as `cli-auth.tsx` did, if `validateSearch` cannot give that.
- A signed-out visit to `/device?code=BCDF-GHJK` redirects to `/login` with `redirect=/device?code=BCDF-GHJK`.
- Without a code, typing `bcdf-ghjk` and pressing Continue navigates to `/device?code=bcdf-ghjk`.
- An unknown code (404) shows the unknown-or-expired text.
- **Login purpose.** The page shows the host, the warning and the read-and-write sentence. Approve posts `{}` and shows "Done. Return to the terminal.".
- **Deny** posts to `…/deny` and shows the cancelled text.
- **Init purpose with two orgs.** Only `admin`/`owner` projects are listed. Approve stays disabled until one is picked, then posts `{ project_id }` with the picked project's id.
- **Init with `?project=acme/support-bot`.** That project is preselected.
- **Init with exactly one eligible project.** It is preselected.
- **Init with no eligible project, as a `member` everywhere.** The admin-role message shows and Approve is disabled.
- **New project.** It posts `{ name }` to the chosen org; after success the new project is selected. A `plan_limit` problem shows the plan message.
- **Approve failing with 403** shows the problem message.
- **The CSP test** in `crates/ui` expects `form-action 'self'`.

**Done when:** written and committed (check rule).

**Commit:** `Add the device approval page and remove /cli-auth`

---

## D4 — `cetra login` on the device flow

**Goal:** `cetra login` requests a code, prints the URL and the code, polls until the grant, verifies and saves the token. The callback server is deleted. Docs and decisions follow.

**Edit:**
- `crates/cli/src/login.rs`
- `crates/cli/src/main.rs` (the `Login` doc comments only)
- `crates/cli/Cargo.toml`
- `Cargo.lock` (only the `cetra-cli` entry, as cargo rewrites it)
- `docs/spec/08-auth.md` (the CLI login bullet)
- `docs/spec/09-api.md` (the CLI row)
- `docs/mvp-decisions.md`
- `CHANGELOG.md`
- `website/src/content/docs/docs/platform/api-keys.mdx`
- `docs/roadmap.md`

**Steps.**
1. **`login.rs`** header doc: "`cetra login` and `cetra logout` (spec 08 § Device authorization, D-AO-4). Login asks the server for a device code, prints the URL and the code, and polls `POST /api/auth/device/token` until the user approves, denies or the code expires. The token is checked with `GET /api/auth/me` and stored in the credentials file. It is never logged."
2. **Delete** everything that code facts § 14 lists as dead after the device flow:
   - `Callback`, `Head`, `WAIT`, `READ_TIMEOUT`, `HEAD_LIMIT`, `BODY_LIMIT`, `PAGE`, `CANCELLED_PAGE`;
   - `cli_auth_url`, `new_state`, `wait_for_callback`, `handle_callback`, `read_head`, `parse_length`, `parse_head`, `callback_for`, `read_body`, `is_form`, `token_callback`, `respond`, `first_field`, `form_fields`, `form_decode`;
   - their tests;
   - the imports nothing uses afterwards: `base64`, `URL_SAFE_NO_PAD`, `rand::TryRng`, `tokio::io::{AsyncReadExt, AsyncWriteExt}`, `tokio::net::{TcpListener, TcpStream}`. `Duration` stays.

   Keep the test helpers `LineWriter` and `query_value` (the real-server test uses them).

   Keep:
   - `sanitise_host`, `local_host`, `open_browser`, `browser_command`, `verify`, `revoke`, `http_client`, `warn`, `logout`;
   - `is_personal_token`, used in step 4;
   - the tests `a_personal_token_has_the_pat_shape` and `login_refuses_a_url_with_credentials_before_printing`;
   - the sanitising part of `the_host_is_sanitised_and_truncated_before_it_is_sent`, renamed `the_host_is_sanitised_and_truncated`.
3. **Private serde types**, local to `login.rs`:

   ```rust
   #[derive(Serialize)]
   struct CodeRequest<'a> { purpose: &'static str /* "login" */, #[serde(skip_serializing_if = "Option::is_none")] host: Option<&'a str> }

   #[derive(Deserialize)]
   struct CodeAnswer { device_code: String, user_code: String, verification_uri_complete: String, expires_in: u64, interval: u64 }

   #[derive(Serialize)]
   struct TokenRequest<'a> { device_code: &'a str }

   #[derive(Deserialize)]
   struct Grant { token: String }

   #[derive(Deserialize)]
   struct ProblemCodeOnly { code: Option<String> }

   /// What one poll means for the loop.
   #[derive(Debug, PartialEq)]
   enum Step { Wait, SlowDown, Denied, Expired, Failed(u16) }
   ```

   Write `Debug` by hand for `CodeAnswer` and `Grant`, hiding `device_code` and `token`; `TokenRequest` needs no `Debug`.
4. **`pub(crate) async fn login(raw, no_browser, credentials, mut output)`**, with the same signature:
   1. `let server = server_url(raw)?;` (unchanged; the refused-URL test still holds).
   2. Request the code exactly so (no `Origin`, no `Authorization`; the tests check both):

      ```rust
      let host = local_host().as_deref().and_then(sanitise_host);
      let client = http_client()?;
      let response = client
          .post(format!("{server}/api/auth/device/code"))
          .json(&CodeRequest { purpose: "login", host: host.as_deref() })
          .send()
          .await
          .context("requesting a sign-in code")?;
      let status = response.status();
      if status != reqwest::StatusCode::OK {
          anyhow::bail!("the server refused a sign-in code (HTTP {})", status.as_u16());
      }
      let answer: CodeAnswer = response.json().await.context("reading the sign-in code")?;
      ```

      If `http_client` does not return a `Result`, drop its `?`. Each poll is built the same way, against `/api/auth/device/token` with `TokenRequest`, and passes `status.as_u16()` and `response.bytes().await` to `step`.
   3. Write two lines to `output`:
      - line 1: `verification_uri_complete`;
      - line 2: `Code: <user_code>`.

      Then flush and drop `output`, as today. Unless `no_browser`, `open_browser(&verification_uri_complete)`.
   4. **Poll.**
      - The deadline is `now + expires_in + 5 s`, and `interval = Duration::from_secs(answer.interval)`.
      - Loop: `tokio::time::sleep(interval)`, then `POST {server}/api/auth/device/token` with `TokenRequest`. A 200 parses `Grant`. Any other status goes through `fn step(status: u16, body: &[u8]) -> Step`:
        - a 400 whose problem `code` is `authorization_pending` → `Wait`; `slow_down` → `SlowDown`; `access_denied` → `Denied`; `expired_token` → `Expired`;
        - anything else → `Failed(status)`.
      - What each step does:
        - `Wait` continues;
        - `SlowDown` sets `interval = next_interval(interval)`;
        - `Denied` bails `"sign-in denied in the browser"`;
        - `Expired` bails `"the sign-in code expired; run \`cetra login\` again"`;
        - `Failed(s)` bails `"sign-in failed (HTTP <s>)"`.
      - Past the deadline, bail with the `Expired` message.
      - A network error on one poll is not fatal: warn once (`warn("cannot reach the server; still waiting")`) and continue until the deadline.
   5. **Check the grant.** If `!is_personal_token(&grant.token)`, bail `"the server returned an invalid token"`.
   6. `verify(&server, &grant.token)`, then save under `server`, as today. The grant's other fields are ignored: `login` gets no project or key.
   7. `fn next_interval(current: Duration) -> Duration { current + Duration::from_secs(5) }`.
5. **`main.rs`.** Change only the `Login` doc comments:
   - command: `/// Sign in with a code you approve in the browser, and save a personal access token for a server.` and `/// The token expires after 90 days. Run \`cetra login\` again when it does.`;
   - `--no-browser`: `/// Print the sign-in URL and code, and do not open a browser.`
6. **`Cargo.toml`.** Remove `rand` and `base64` from `crates/cli/Cargo.toml` `[dependencies]`: only the deleted `new_state` used them. A `base64` hit in a `main.rs` doc comment is prose, not a use. `Cargo.lock` changes with it; cargo rewrites it at the final check, and that commit includes it.
7. **Spec 08, the CLI login bullet (line 158).** Replace it whole with:

   "CLI login: `cetra login --url https://…` gets a device code (§ Device authorization), prints the URL and the code, and opens the browser unless `--no-browser` is given. It polls until the user approves or denies on `/device`, or the code expires after 10 minutes. On approval it gets a `read, write` PAT named `CLI on <host>` (`CLI` without a host) that expires after 90 days, checks it with `GET /api/auth/me`, and saves it in `~/.config/cetra/credentials.toml` (0600) under the URL given to `--url`. Because the token expires after 90 days, `cetra login` must be run again then. `cetra logout` revokes that PAT with `DELETE /api/v1/me/tokens/current`. The device flow works when the browser runs on another machine (SSH, containers) (D-AO-4)."
8. **Spec 09, the CLI row.** It becomes `| \`cetra login --url …\` · \`cetra logout\` | PAT login with a device code approved in the browser; the token expires after 90 days, so run \`cetra login\` again then (08 § CLI login, D-AO-4) |`.
9. **`docs/mvp-decisions.md`.**
   - D-L-9: prefix its decision text with "**Superseded by D-AO-4** (the device flow; the token name and expiry are kept)."
   - D140: append "The CLI sign-in is the device flow since D-AO-4."
   - D-L-8 (the route list with `/cli-auth?port=&state=&host=`) and every reserved-slug list that names `cli-auth`: replace the route with `/device?code=&project=` and the slug with `device`, followed by "(D-AO-4)". Find them with `grep -n "cli-auth" docs/mvp-decisions.md`.
10. **`CHANGELOG.md` `[Unreleased]`.**
    - `### Changed`: "`cetra login` signs in with a code you approve in the browser, so it works over SSH and in containers."
    - `### Removed`: "The `/cli-auth` page and the CLI's localhost callback."
11. **`api-keys.mdx` line 31.** It becomes: "For the CLI, `cetra login` shows a code that you approve in the browser, and saves a token that expires after 90 days."
12. **`docs/roadmap.md`.** In the AO row's Status, write `**Design** approved; M1 implemented (the device flow, \`/device\`, \`cetra login\` on it).` If M2 is already marked there, keep its words.

**Tests** (in `login.rs`):
- **`step`:**
  - a 400 with `{"code":"authorization_pending"}` is `Wait`;
  - `slow_down`, `access_denied` and `expired_token` map to their steps;
  - a 400 with another code, a 500, and a 400 with an unparseable body are `Failed`.
- `next_interval(5 s)` is 10 s.
- **Canned flows** (`test_server::Canned::json`, with `interval: 0` in the canned code answer so the test does not wait):
  - Code, then `authorization_pending`, then a grant, then `GET /api/auth/me` 200: the token is saved, the output has the URL line and `Code: BCDF-GHJK`, and the recorded requests are those four in order. The two POSTs carry no `Origin` and no `Authorization`.
  - Code, then `access_denied`: the error is "sign-in denied in the browser", and no credentials file exists.
  - Code, then `expired_token`: the expired message, and no file.
  - Code, then a grant whose token is `sk-cetra-…`: "the server returned an invalid token", and no file.
- **Real server** (rewrite `login_saves_a_token_and_logout_revokes_it`; keep its server start, admin login and assertions):
  1. Run `login(...)` in a task with `LineWriter`, and read the first line, the URL.
  2. Parse `code` from its query with a small helper.
  3. `POST {server}/api/auth/device/{code}/approve` with the session cookie, `origin: http://127.0.0.1:8080` and body `{}`.
  4. Await `login`: it returns `Ok` within one poll interval (5 s).
  5. The credentials file holds the token (0600, directory 0700). `logout` revokes it, and `/api/auth/me` with it is then 401.

  The test takes about 5 s because the server's interval is 5 s. That is accepted.

**Done when:** written and committed (check rule).

**Commit:** `Move cetra login to the device flow`

---

## After D4 (the coordinating session)

1. `just openapi`, `just api-examples`, `just docs-gen` (commit the regenerated `openapi.json`, `schema.gen.ts`, `fixtures/api/manifest.json` and `reference/cli.md` as `Regenerate the API contract and the CLI reference`).
2. `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`, once; fixes go back to a Haiku agent with an exact brief.
3. A Sonnet code review of D1–D4.
