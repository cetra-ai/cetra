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
- **The CLI follows `moved` at most once**, only to an `https` URL (or a loopback `http` URL, for tests), and never sends anything but the device code there.

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
     4. Sweep, then refuse with `Full` at `MAX_CODES`, as `issue` does.
     5. The hash or the user code is already present: `Taken`.
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

   It derives `Debug` normally: the hash is not a secret, since the code cannot be recovered from it.
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
4. **OpenAPI** (`api/openapi.rs`).
   - Register `InternalDeviceCode`.
   - Add the operation `post_internal_device_code` with the tag and security the other internal routes use (copy `post_internal_org`'s), and the request body.
   - Responses: `204`, plus errors 400, 401, 403, 409 and 503, or exactly the set the internal routes' lint arm expects with 409 and 503 added.
5. **`tests/openapi.rs`.**
   - Add `("post", "/api/internal/v1/device/codes", "post_internal_device_code")` to `OPERATIONS`, in order.
   - Put the operation in the same security list as `post_internal_org`.
   - Add an error arm with the 4xx set from step 4. The lint compares 4xx only.
6. **Specs.**
   - **Spec 08 § Device authorization:** add a paragraph on the internal route, the scope `device`, the checks, and that a registered code then behaves exactly like an issued one.
   - **Spec 09:** add the route to the internal routes' paragraph of § Orgs, projects, members, keys, after `GET /api/internal/v1/deletions`.

**Tests:**
- **Inline, `auth/device.rs`:**
  - a register then `pending` sees the code;
  - `claim`, `complete` and a `poll` with the original device code (hashed by the store) gives `Granted`;
  - each `Invalid` case;
  - `Taken` for the same hash, and for the same user code;
  - an `expires_at` 11 minutes ahead is invalid.
- **Inline, `routes/device.rs`** (`TestApp`, `.control_plane("device")`):
  - 204 on a valid body, then `GET /api/auth/device/{code}` with a session shows it;
  - a token with another scope gets 403;
  - a session gets 403;
  - a duplicate gets 409;
  - a bad hash gets 400 on `device_code_sha256`;
  - **end to end:** register, approve with a session, and `POST /api/auth/device/token` with the device code whose hash was registered gives the grant.

  The access suite covers the route automatically, since its access is a `ControlPlane` variant.

**Done when:** written and committed (check rule).

**Commit:** `Let the control plane register a device code on a cell`

---

## E2 — The CLI follows `moved`

**Goal:** `deviceSignIn` follows one `moved` answer to the cell and reports which server granted.

**Edit:** `node-cli/src/device.ts`, `device.test.ts`, `init.ts`, `init.test.ts`, `login.ts`.

**Steps.**
1. **`step`.** A 400 problem whose `code` is `moved` gives the new step `{ moved: string }`, with the problem's `server` member. A `moved` without a string `server` gives `{ failed: 400 }`.
2. **`deviceSignIn`** returns `{ grant: Grant; server: string }`, where `server` is the server that answered the grant. In the poll loop, on `{ moved: url }`:
   - **Second `moved`:** throw `CliError("sign-in failed: the server moved the sign-in twice", Exit.SignIn)`.
   - **The new server:** `serverUrl(url)` must succeed, and the URL's protocol must be `https:`, or `http:` with a loopback host (`isLoopback` from `local.ts`). Otherwise throw `CliError("the server sent an invalid sign-in server", Exit.SignIn)`.
   - Print `Continuing the sign-in at <origin>` on stderr.
   - From then on, poll the new server with the same device code. Keep the deadline and the interval.
3. **`init`.** Everything that used the typed server after the sign-in uses the returned `server`: the credentials key, the link's `server`, and `tracesUrl`'s base when there is no `grant.server`. `CETRA_BASE_URL` stays `grant.server`.
4. **`login`.** Save the token under the returned `server`, and print it in the success line.

**Tests:**
- **`device.test.ts`**, with two canned servers A and B:
  - A answers the code, `authorization_pending`, then `moved` to B's URL; B answers the grant. The grant is returned with `server` B, and B received the same `device_code`.
  - A second `moved` from B throws.
  - `moved` to `http://example.com` (not loopback) throws the invalid-server message.
  - `moved` without `server` fails with HTTP 400.
- **`init.test.ts`:** A is the typed `--url` and moves to B. The credentials file and `.cetra/project.json` name B.

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
      - It mints a control-plane token with scope `device` for that cell and calls `POST <cell>/api/internal/v1/device/codes` with `{device_code_sha256, user_code, purpose, host, expires_at}`.
      - It then redirects the browser to `<cell>/device?code=<user_code>`. It forwards `project` when the CLI's URL had one.
   3. **The token route.** `POST /api/auth/device/token` on `app.cetra.sh`, by the device code's hash:
      - `authorization_pending` until the code is registered on a cell;
      - then `400 application/problem+json {type: "about:blank", title: "Bad Request", status: 400, code: "moved", server: "<cell public_url>"}`;
      - `expired_token` after the expiry;
      - the same `slow_down` rule as the cell.
   4. **What the control plane never sees:** the token or the key. They are minted on the cell and handed to the CLI by the cell.
   5. **`GET /readyz`** on `app.cetra.sh` answers 200. The CLI probes it before signing in.
2. **Design § Cloud routing.** Replace `answers 400 {error: "moved", server: …}` with the problem+json shape of step 1.3. Add the two decisions settled in this plan's header (the `moved` shape; the CLI follows it once, to https or loopback only).
3. **D-AO-14** (`docs/mvp-decisions.md` and the design's table, kept identical): append "The `moved` answer is a 400 problem with `code: \"moved\"` and `server`; the CLI follows it once, to an https URL."
4. **Docs.**
   - **`coding-agent.mdx`:** one sentence on the cloud: with no `--url`, init signs you in at app.cetra.sh, and the sign-in continues on your organization's region.
   - **`cetra-cli.md`:** the same note under `init`'s `--url`.
5. **`CHANGELOG.md`:** no new line. The M3 line covers `init`.
6. **`docs/roadmap.md`:** the AO row's Status becomes `**Implemented** (M1–M5); the control plane's side of the cloud sign-in is a request to track Q (\`ao-requests-for-q.md\`).`

**Done when:**
- `cd website && node scripts/check-terms.mjs` exits 0;
- `pnpm --dir website run build` succeeds.

**Commit:** `Document the cloud sign-in and the request to Q`

---

## After E3 (the coordinating session)

1. `just fmt`, `just openapi`, `just check`, `just e2e`.
2. A Sonnet code review of E1–E3, then the fixes.
3. Report to the user.
