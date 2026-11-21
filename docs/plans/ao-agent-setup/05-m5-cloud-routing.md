Status: plan, written 2026-11-21. Not implemented.

# AO · M5 — Cloud routing, cell side

`npx @cetra-ai/cli init` with no URL talks to `https://app.cetra.sh`, the control plane (track Q, private repository `cetra-control`). The control plane:
- issues the device code;
- signs the user in and finds the org's cell;
- registers the code on that cell through a new internal route;
- tells the polling CLI to move to the cell.

The cell mints the secrets, so they never pass through the control plane. This milestone builds the cell's side and the CLI's `moved` handling, and writes Q's side as a request.

The design is `docs/plans/ao-agent-setup/00-design.md` § Cloud routing (M5 and Q's request) and § M5; D-AO-14 applies. **Haiku implements it one phase at a time.** If a step is unclear or contradicts the code, **stop and report**.

**Check rule** (user, 2026-11-20): phases are committed without the full checks. E1 is Rust and is committed without compiling; the coordinating session compiles after E2. E2 may run its own vitest files, the type check, and Biome from `node-cli/`. `just check` and `just e2e` run once after E3.

## Read first

- `AGENTS.md`.
- Design § Device flow (M1) and § Cloud routing.
- `crates/server/src/auth/device.rs`: the store (D1, with the approval tickets).
- `crates/server/src/routes/device.rs` and `crates/server/src/api/device.rs`.
- `crates/server/src/routes/internal_deletions.rs`: a control-plane route with `Access::ControlPlane { scope }`.
- `crates/server/tests/openapi.rs`: the `OPERATIONS` list, the security lists (find where `post_internal_org` sits) and the error-set arms.
- `node-cli/src/device.ts`, `init.ts`, `login.ts`.

## What it is, and what it is not

**When M5 is done:**
- The cell answers `POST /api/internal/v1/device/codes` (a control-plane token with scope `device`). It stores a code that the control plane issued, keyed by the SHA-256 of its device code, as if the cell had issued it.
- The CLI follows one `moved` answer from the server it polls to the cell that answer names, with the same device code. From then on the cell is the CLI's server: the credentials key, the link file and `CETRA_BASE_URL`.
- `ao-requests-for-q.md` in the session notes folder says what the control plane must build.

**Not in M5:**
- the control plane's routes and pages (track Q);
- the Rust `cetra login` (it always has a `--url`, the cell's).

## Decisions

| ID | Summary |
|---|---|
| D-AO-14 | `app.cetra.sh` issues the code and registers it on the cell; the CLI moves on `moved`; secrets never pass through the control plane |
| D-P-28 | Control-plane tokens: short-lived, one scope, for one cell |

Settled here, written into the design's § Cloud routing in E3:
- **The `moved` answer** is a `400 application/problem+json` with `code: "moved"` and one extra member `server`, the cell's public URL. The control plane answers it to `POST /api/auth/device/token` once the code is registered on a cell.
- **The CLI follows `moved` at most once**, to the URL's **origin** only, over `https`, or over loopback `http` only when the typed server is itself loopback (tests). It sends nothing there but the device code. Afterwards, every request that carries the new token (`/api/auth/me`) goes to the new server, never to the old one.
- **Trust boundary.** The control plane is the cells' identity provider (D183). A compromised one can already sign in as anyone on any cell, so it is trusted. "The secrets never pass through it" means they are minted on the cell and sent only to the CLI; the control plane, which knows the device code, could still poll for a grant itself. The design and D-AO-14 say so (E3). A PKCE-style poll secret is rejected: it protects nothing against an identity provider.
- **Registration is idempotent.** The same registration twice (same hash, user code, purpose, host and expiry, still pending) answers 204. Any other clash answers 409.

---

## E1 — The cell's internal route

**Goal:** the control plane can register a device code on a cell.

**Edit:**
- `crates/server/src/auth/device.rs`
- `crates/server/src/api/device.rs`
- `crates/server/src/api/openapi.rs`
- `crates/server/src/routes/device.rs`
- `crates/server/tests/openapi.rs`
- `docs/spec/08-auth.md`
- `docs/spec/09-api.md`

**Steps.**
1. **`DeviceStore::register`** (`auth/device.rs`):

   ```rust
   /// A code the control plane issued (D-AO-14): stored as if this cell had issued it.
   /// `device_code_sha256` is the lowercase hex SHA-256 of the device code, 64 characters.
   pub(crate) fn register(
       &self,
       device_code_sha256: &str,
       user_code: &str,
       purpose: Purpose,
       host: Option<String>,
       expires_at: i64,
       now: i64,
   ) -> Result<(), DeviceError>;
   ```

   - Add `DeviceError::Invalid(&'static str)`, with `#[error("{0}")]`, and `DeviceError::Taken`, with `#[error("device code already registered")]`.
   - **Checks, in order:**
     1. `device_code_sha256` is 64 characters of `[0-9a-f]`; otherwise `Invalid("device_code_sha256")`.
     2. `normalize_user_code(user_code)` is `Some`; otherwise `Invalid("user_code")`.
     3. `now < expires_at <= now + CODE_TTL_NS`; otherwise `Invalid("expires_at")`.
     4. Sweep always (`inner.sweep(now)`), so that an expired entry never blocks a registration. Then refuse with `Full` at `MAX_CODES`.
     5. An entry with this hash exists: when it is `Pending` and its user code, purpose, host and `expires_at` are all equal, return `Ok(())` (a retry). Otherwise `Taken`.
     6. Another entry has this user code: `Taken`.

     Registered codes count against the same `MAX_CODES`; spec 08 says so.
   - **Insert** the entry exactly as `issue` does, with `created_at = now`, `interval_s = POLL_INTERVAL_S`, `last_poll = None`, `State::Pending` and the given `expires_at`.
2. **API type** (`api/device.rs`):

   ```rust
   /// The control plane registers a code it issued (D-AO-14).
   #[serde(deny_unknown_fields)]
   pub struct InternalDeviceCode {
       pub device_code_sha256: String,
       pub user_code: String,
       pub purpose: DevicePurpose,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       pub host: Option<String>,
       #[serde(with = "time_serde")]
       #[schema(value_type = String, format = DateTime)]
       pub expires_at: i64,
   }
   ```

   Derives: `#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]` (deserialized only, as `DeviceCodeRequest`). `Debug` is derived normally: the hash is not a secret, since the code cannot be recovered from it. Give each field a doc comment like its neighbours'; `time_serde` is already imported in `api/device.rs`.
3. **Route** (`routes/device.rs`):
   - **Declaration:** add `RouteDef { method: POST, path: "/api/internal/v1/device/codes", access: Access::ControlPlane { scope: "device" }, documented: true, handler: post(register_code) }`.
   - **`register_code`:**
     - **Host.** `host` goes through the existing `sanitise_host`.
     - **Errors.**
       - `Invalid(field)` becomes `field_error(field, "invalid")`, which is 400.
       - `Taken` becomes `ApiError::Conflict { detail: "device code already registered" }`, which is 409.
       - `Full` becomes `ApiError::Unavailable { detail: "too many pending device codes" }`, which is 503.
     - **Success:** 204.
     - **Audit:** `tracing::info!(target: "audit", purpose = ?…, "device code registered by the control plane")`. Never log the user code or the hash.
4. **OpenAPI** (`api/openapi.rs`):
   1. Import and register `InternalDeviceCode` next to the other device types.
   2. Build the operation as `post_internal_org`'s is built: the same tag, the same `.securities(...)`, `.request_body(Some(json_body("InternalDeviceCode")))`, and the id `post_internal_device_code`.
   3. Responses: a `204`, the errors `[400, 401, 403, 409]`, and a 503 declared the way `post_device_code` declares its 503. The handler never answers 429, so 429 is not declared.
   4. **Attach it to its path:** `.path("/api/internal/v1/device/codes", PathItem::new(HttpMethod::Post, op))`, in the chain of the other `/api/internal/v1/…` paths. Without it, `document_lint` and the route-table check in `http/access.rs` fail.
5. **`tests/openapi.rs`.**
   - Add `("post", "/api/internal/v1/device/codes", "post_internal_device_code")` to `OPERATIONS`, anywhere: the list is compared as a set.
   - Add the id to the `matches!` that holds `post_internal_org`, the control-plane security list.
   - Add the arm `else if *id == "post_internal_device_code" { expected_errors.extend(["400", "401", "403", "409"]); }` **before** the final `else if *id != "get_auth_config"` fallbacks. The lint compares 4xx only, so the 503 is not listed.
6. **Specs.**
   - **Spec 08 § Device authorization:** add a paragraph covering:
     - the internal route and the scope `device`;
     - the checks;
     - idempotence;
     - that registered codes count against the 10 000 cap;
     - that a registered code then behaves exactly like an issued one.

     Add a row for the route to the table of routes at the end of spec 08, if it has one: control-plane token, scope `device`, no `Origin`; 204, 400, 401, 403, 409, 503.
   - **Spec 09:** in the long internal-routes cell of § Orgs, projects, members, keys, insert right after the `GET /api/internal/v1/deletions` item: `` · `POST /api/internal/v1/device/codes` (scope `device`; body `{device_code_sha256, user_code, purpose, host?, expires_at}`; 204; an identical retry is 204, any other clash 409) ``.

**Tests:**
- **Inline, `auth/device.rs`:**
  - a register then `pending` sees the code; a lower-case `bcdf-ghjk` is accepted and normalized;
  - the same registration twice is `Ok`; the same hash with another user code is `Taken`;
  - an expired, unswept entry with the same user code does not block a new registration 11 minutes later;
  - boundaries: `expires_at == now` is invalid, `now + CODE_TTL_NS` is valid, and `+ 1` is invalid; a 63- or 65-character hash, upper-case hex, and a user code with a vowel are invalid;
  - `claim`, `complete` and a `poll` with the original device code (hashed by the store) gives `Granted`;
  - each `Invalid` case;
  - `Taken` for the same hash, and for the same user code;
  - an `expires_at` 11 minutes ahead is invalid.
- **Inline, `routes/device.rs`** (`TestApp`, `.control_plane("device")`). Times come from `app.state.clock.now_ns()` (the manual clock starts on 2026-01-01), for example `expires_at = now + 600 s`, never from the wall clock:
  - 204 on a valid body, then `GET /api/auth/device/{code}` with a session shows it;
  - a token with another scope gets 403;
  - a session gets 403;
  - a duplicate gets 409;
  - a bad hash gets 400 on `device_code_sha256`; an unknown JSON field gets 400; a 100-character host is sanitised, not refused;
  - an identical retry gets 204;
  - **end to end:** register, approve with a session, and `POST /api/auth/device/token` with the device code whose hash was registered gives the grant.

  The access suite covers the route automatically, since its access is a `ControlPlane` variant.

**Done when:** written and committed (check rule). `openapi.json` and `schema.gen.ts` are regenerated and committed by the coordinating session after E3.

**Commit:** `Let the control plane register a device code on a cell`

---

## E2 — The CLI follows `moved`

**Goal:** `deviceSignIn` follows one `moved` answer to the cell and reports which server granted.

**Edit:** `node-cli/src/device.ts`, `device.test.ts`, `init.ts`, `init.test.ts`, `login.ts`, `login.test.ts`.

**Steps.**
1. **`step`.** A 400 problem whose `code` is `moved` gives the new step `{ moved: string }`, with the problem's `server` member. A `moved` without a string `server` gives `{ failed: 400 }`.
2. **`deviceSignIn`** returns `{ grant: Grant; server: string }`, where `server` is the server that answered the grant. In the poll loop, on `{ moved: url }`:
   - **Second `moved`:** throw `CliError("sign-in failed: the server moved the sign-in twice", Exit.SignIn)`.
   - **The new server:** `checkServerUrl(url)` must pass; then take `new URL(url).origin` only. Its protocol must be `https:`, or `http:` with a loopback host **and** a loopback typed server. Write the loopback test inline in `device.ts` (`localhost`, `127.0.0.0/8`, `[::1]`), without importing `local.ts`, which imports `device.ts`. Otherwise throw `CliError("the server sent an invalid sign-in server", Exit.SignIn)`.
   - Print `Continuing the sign-in at <origin>` on stderr. The origin has no control characters, since `URL` serialises it.
   - From then on, poll the new origin with the same device code. The deadline, the interval and the "still waiting" warning state carry over unchanged. The `moved` count is per sign-in.
3. **`init`.**
   - Add `server: string` to `SignedIn`. `signInWithPassword` returns the local server; `signInWithDevice` returns `deviceSignIn`'s `server`.
   - `saveToken` and `writeLink` use `signedIn.server`.
   - `tracesUrl`, `CETRA_BASE_URL` and the JSON `server` stay `grantServer` (the grant's `server`).
4. **`login`.** The `/api/auth/me` check, `saveToken`, the success line and the `--json` output all use the returned `server`. The new token is never sent to the server that answered `moved`.

**Tests:**
- **`device.test.ts`.** Change its `signIn` helper to return `{ grant, server }` and adjust the existing assertions. Add `step` cases for `moved`: with a string `server`, without one, and with a non-string one. Then, with two canned servers A and B (both on `127.0.0.1`, so loopback is allowed because A is loopback):
  - A answers the code, `authorization_pending`, then `moved` to B's URL; B answers the grant. The grant is returned with `server` B, and B received the same `device_code`.
  - A second `moved` from B throws.
  - `moved` to `http://example.com` (not loopback) throws the invalid-server message;
  - `moved` to `http://127.0.0.1:<B port>/some/path?x=1` polls B's origin, `/api/auth/device/token`;
  - the stderr line names B's origin.
  - `moved` without `server` fails with HTTP 400.
- **`init.test.ts`:** A is the typed `--url`. A's canned answers are, in order: `GET /readyz` 200, the code, `authorization_pending`, `moved` to B. B answers the grant. The credentials file and `.cetra/project.json` name B.
- **`login.test.ts`:** A moves to B. `/api/auth/me` is called on B and never on A, and the credentials file names B only.

**Done when:** `pnpm --dir node-cli exec vitest run src/device.test.ts src/init.test.ts src/login.test.ts` passes; the type check passes; Biome is clean.

**Commit:** `Follow a moved sign-in to the org's cell`

---

## E3 — The request to Q, the design, the docs

**Goal:** track Q knows what to build, and the design and docs say how the cloud default works.

**Edit:**
- `docs/plans/ao-agent-setup/00-design.md`
- `docs/mvp-decisions.md` (the D-AO-14 row only)
- `website/src/content/docs/docs/get-started/coding-agent.mdx`
- `website/src/content/docs/docs/reference/cetra-cli.md`
- `CHANGELOG.md`
- `docs/roadmap.md`
- `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/ao-requests-for-q.md` (new, outside the repository)

**Steps.**
1. **`ao-requests-for-q.md`.** Title "AO requests for track Q: device sign-in through app.cetra.sh". It lists what the control plane builds:
   1. **The code route.** `POST /api/auth/device/code` on `app.cetra.sh`, the same request and answer as the cell's (spec 08 § Device authorization):
      - the control plane issues both codes;
      - `verification_uri` is `https://app.cetra.sh/device`;
      - it keeps the code's SHA-256, user code, purpose, host and expiry;
      - the same rate limit: 10 a minute per IP or IPv6 /64.
   2. **`/device` on the control plane.**
      - It signs the user in, and the user picks the org when they belong to orgs on more than one cell.
      - It mints a control-plane token with scope `device` for that cell, one per call: the token is bound to the method and path (D-P-71). It calls `POST <cell>/api/internal/v1/device/codes` with `{device_code_sha256 (lowercase hex SHA-256 of the device code), user_code, purpose ("login" or "init"), host?, expires_at (RFC 3339, the expiry it issued)}`.
      - **Answers:**
        - 204, including on an identical retry: go on;
        - 409: the user code clashes, so issue a new code to the CLI;
        - 503: the cell is full, so retry later;
        - 400: a bug.
      - It then redirects the browser to `<cell>/device?code=<user_code>`. It forwards `project` when the CLI's URL had one.
   3. **The token route.** `POST /api/auth/device/token` on `app.cetra.sh`, by the device code's hash:
      - `authorization_pending` until the code is registered on a cell;
      - then `400 application/problem+json {type: "about:blank", title: "Bad Request", status: 400, code: "moved", server: "<cell public_url origin>"}`;
      - `expired_token` after the expiry;
      - the same `slow_down` rule as the cell.
   4. **What the control plane never receives:** the token or the key. They are minted on the cell and handed to the CLI by the cell. The control plane, which knows the device code, must not poll the cell's token route itself.
   5. **`GET /readyz`** on `app.cetra.sh` answers 200. The CLI probes it before signing in.
2. **Design § Cloud routing.**
   - Replace `answers 400 {error: "moved", server: …}` with the problem+json shape of step 1.3.
   - Add the decisions settled in this plan's header: the `moved` shape; following it once, to an origin, over https; the trust boundary; idempotent registration.
3. **D-AO-14** (`docs/mvp-decisions.md` and the design's table, kept identical): append "The `moved` answer is a 400 problem with `code: \"moved\"` and `server`; the CLI follows it once, to an https origin. The control plane, as the cells' identity provider, is trusted: the secrets are not sent through it, but it knows the device code."
4. **Docs.**
   - **`coding-agent.mdx`:** one sentence on the cloud: with no `--url`, init signs you in at app.cetra.sh, and the sign-in continues on your organization's region.
   - **`cetra-cli.md`:** the same note under `init`'s `--url`.
5. **`CHANGELOG.md` `### Added`:** "`POST /api/internal/v1/device/codes` lets a control plane register a device code on a cell (cloud sign-in)."
6. **`docs/roadmap.md`:** append to the AO row's Status: `; M5 implemented (the cell's internal route, \`moved\` in the CLI); the control plane's side is a request to track Q (\`ao-requests-for-q.md\`)`. Keep the M1–M4 text.

**Done when:**
- `cd website && node scripts/check-terms.mjs` exits 0;
- `pnpm --dir website run build` succeeds.

**Commit:** `Document the cloud sign-in and the request to Q`

---

## After E3 (the coordinating session)

1. `just fmt`, `just openapi`; commit the regenerated `openapi.json` and `schema.gen.ts` as `Regenerate the API contract`; then `just check` and `just e2e`.
2. A Sonnet code review of E1–E3, then the fixes.
3. Report to the user.
