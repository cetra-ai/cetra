# P · M5 — Per-org SSO

Milestone 5 of track P ([`00-design.md`](00-design.md)). It builds on M2 ([`02-m2-org-entitlements.md`](02-m2-org-entitlements.md)) and M3 ([`03-m3-units-and-limits.md`](03-m3-units-and-limits.md), for the seat check), both merged. It does not depend on M4.

Per-org SSO serves **self-hosted instances** (D-P-41): on the cloud the control plane federates Teams SSO, so cells never grant `org_sso` in practice. The code does not test where it runs: it works for any org whose entitlements include `org_sso` (on a self-hosted instance, a licence listing it, since org tokens are ignored there, D-P-50; on a cell, an org token).

When M5 is done:
- an org owner registers one OIDC identity provider for the org; its client secret is sealed in `meta.db` like an LLM connection key (D-I-2) (D-P-51, D-P-52);
- the owner proves the org's email domains with a DNS TXT record; a domain is verified by at most one org (D-P-54);
- the org's provider signs in only emails of the org's verified domains, links existing accounts, and adds new people to the org with the role the org chose (viewer or member), within `max_seats` (D-P-55, D-P-56);
- the login page asks for the email first and sends emails of an SSO domain to their org's provider (D-P-60);
- when the org enforces SSO, users of its domains lose their password sessions and personal access tokens at once, and password login, password reset and the instance-wide providers are refused for them; project API keys keep working (D-P-57, D-P-58);
- a lapse of `org_sso` (licence expired, feature gone) never widens access: everything set up keeps applying; only setting up or changing it is locked (D-P-8, D-P-61);
- `llmobs admin org-sso disable <org>` turns an org's SSO off when its provider is broken (D-P-63);
- all of it is `ee` (D-P-4): `crates/server/src/ee/org_sso/` and `web/src/ee/org-sso/`.

**Read first, in this order:**
1. `AGENTS.md` (rules, commands, privacy).
2. [`00-design.md`](00-design.md) § Decisions D-P-1, D-P-2, D-P-4, D-P-8, D-P-10, D-P-12, D-P-14, D-P-15, D-P-20, D-P-25, D-P-41, and § Per-org SSO (M5) (D-P-51 … D-P-65, also in the table below).
3. `docs/spec/08-auth.md` (all of it); `docs/spec/04-metadata-db.md` § Identity & tenancy and § Encryption of secrets; `docs/spec/09-api.md` § Auth; `docs/spec/12-operations.md` § Security (the SSRF guard, D166).
4. The code: `crates/server/src/routes/auth.rs`, `routes/oauth.rs`, `auth/oidc/{mod,flow,provider,state_cookie}.rs`, `auth/session.rs`, `ee/sso_enforcement.rs`, `routes/connections.rs` (`sealed_key`, how a secret is sealed and opened), `crypto.rs` (`Aad`, `ENCRYPTED_COLUMNS`), `crates/eval/src/egress.rs`, `entitlements/mod.rs` (`require`, `over_limit`, `plan_limit`), `meta/{invites,members,identities,personal_tokens,web_sessions}.rs`, `test_support/{mod,mock_idp}.rs`.
5. The code named in each sub-phase before you change it.

## Decisions of this milestone

These are the decisions the plan implements, also in [`00-design.md`](00-design.md) § Per-org SSO (M5) and `docs/mvp-decisions.md` § Track P. The user chose the (user) items on 2026-11-15.

| # | Decision |
|---|---|
| D-P-51 | **Owners manage org SSO.** New `OrgPerm::ManageSso`, minimum role owner, session-only. Enforcement can lock every member out and JIT decides who joins, so it sits with deleting the org and changing owners (spec 08's owner-only row), not with members and invites (admins). |
| D-P-52 | **One generic OIDC provider per org.** Authorization code with PKCE (S256), scopes `openid email profile`, the same `openidconnect` code as `[[auth.oidc]]`. Identities are stored with provider `org:<org_id>`. Changing the issuer deletes the org's identities (a new issuer's subjects are a different namespace), and a new client secret is then required. Callback URL for every org: `<public_url>/api/auth/callback/sso` (D-P-65). |
| D-P-53 | **Org issuers pass the SSRF guard.** Unlike `[[auth.oidc]]` (operator configuration), an org's issuer is typed by an org owner, so discovery, the token request and the JWKS fetch go through a client with D166's resolver guard (`security.allow_private_network_egress`, which an operator sets for an IdP on a private network), no redirects, no proxy, 10 s timeout. The issuer follows `[[auth.oidc]]`'s rule (`https`, or `http` on a loopback host; no multi-tenant Entra). Discovered `token_endpoint` and `jwks_uri` must be `http(s)` and not a denied literal IP. Saving the provider loads its discovery document, so a wrong issuer fails at once (400). |
| D-P-54 | **Domains.** Exact match on the email's domain (no subdomains, as `allowed_domains`). At most 20 per org. A domain is verified by at most one org at a time (unique partial index). A domain listed in `auth.enforce_sso_domains`, `auth.google.allowed_domains` or any `[[auth.oidc]].allowed_domains` cannot be added, and is ignored by org SSO if the config lists it later (startup warning with a count). Proof: a TXT record `_llmobs-verify.<domain>` whose value is `llmobs-verify=<32 lowercase hex>` (a random token per org and domain, shown on the settings page; both names built from `llmobs_name!`, D46). Verification is checked when the owner clicks Verify, at most 10 times a minute per org, and is not re-checked later; an owner removes a domain the org no longer controls. |
| D-P-55 | **What the org's provider may sign in.** Only emails whose domain is one of the org's verified domains (`domain_not_allowed` otherwise), for new and known identities alike. Its email claim is trusted without `email_verified` (Entra omits it): the org proved the domain and runs the IdP. An unknown identity whose email belongs to an existing account is linked to it (an org provider always links); when that account's email was never verified, linking also deletes its web sessions, revokes its personal access tokens and clears its password (D148's defence against a pre-registered account); a verified account is linked without revocation. Instance admins are never linked (`link_refused`, as D148). A disabled account is `account_disabled`. A new account is created verified and without a password; under `auth.signup = disabled` it is `signup_closed`, under `invite_only` the org's verified domain counts as the invitation, `signup_domains` do not apply. |
| D-P-56 | **JIT provisioning (user).** A user who signs in through the org's provider and is not a member joins the org. The role is the org's pending invitation's role when one exists for that email (the invitation is then accepted), else the org's `jit_role`, `viewer` (default) or `member`. Seats (D-P-20): a non-viewer role needs a free seat (`seat_members + seat_invites`, or `seat_members` alone when an invitation is accepted, as D-P-20's accept rule); over `max_seats`, a seat limit never locks anyone out of the orgs they already belong to (user): a user who already has another org membership, or a pending invitation to another org, signs in without joining and lands on `/sso/no-seat?org=<slug>` ("No seat available in <org>; ask an owner."); only a user who would end up with no membership at all is refused, before anything is written, with `/login?error=seat_limit`. While the org lacks `org_sso` (a lapse), sign-in keeps working but nobody joins (a new membership would widen access, D-P-8). |
| D-P-57 | **Enabled, then enforced.** A new provider is disabled. Discovery (D-P-60) sends users only to an enabled provider; the owner tests a disabled one through "Test sign-in" (`GET /api/auth/sso/authorize?domain=` works for any verified domain of an org with a provider). Enforcing needs the provider enabled, at least one verified domain, and, when the caller's own email is in one of those domains, a session started through this org's provider (so an owner cannot enforce a provider they never got through). While enforced, the provider cannot be disabled or deleted (409). |
| D-P-58 | **Enforcement revokes (user).** When an org turns enforcement on, and when a domain is verified while it is on, every user of those domains who is not an instance admin loses at once: web sessions not started through this org's provider, all personal access tokens (the CLI's credential), pending password resets. Project API keys (`sk`, `pk`) are untouched. While enforced, for those users: password login answers `403 sso_required` (checked after the password verifies, so it reveals nothing to someone without the password); password signup and invitation signup answer `403 sso_required`; `password/forgot` sends nothing (still 204); `password/reset` answers `403 sso_required`; GitHub, Google and `[[auth.oidc]]` logins redirect to `/login?error=sso_required`. **Instance admins are exempt** (the operator's break-glass; they sign in with a password at `/login?password=1`). `auth.enforce_sso_domains` (instance-wide, D-P-8) is unchanged. |
| D-P-59 | **Sessions record their org provider.** `web_sessions.sso_org_id` (nullable, `ON DELETE SET NULL`) is the org whose provider started the session. Enforcement keeps those sessions; D-P-57's caller check reads it. |
| D-P-60 | **Email-first login (user).** `GET /api/auth/config` gains `sso_discovery` (true when at least one verified domain routes to an enabled org provider). When true, the login page asks for the email, posts it to `POST /api/auth/sso/discover`, and either navigates to `/api/auth/sso/authorize?domain=<domain>&redirect=…` or shows the password form and the instance providers as today. The answer is `{"sso": bool}`, a function of the domain only (never of an account), and an invalid email answers `{"sso": false}`. Discovery and authorize share a per-IP limiter (30 per minute, D143). URLs carry the domain, never the org; nothing logs an email or a domain (ids and counts only). |
| D-P-61 | **Lapses (D-P-8).** Without `org_sso`: reading the settings, removing a domain, turning enforcement off, and disabling or deleting a provider that is not enforced still work; saving the provider, adding or verifying a domain, enabling and enforcing answer `403 plan_required`. Sign-in through the org's provider and enforcement keep applying; JIT stops (D-P-56). |
| D-P-62 | **Routing in memory.** `AppInner.org_sso` holds the verified domains of active orgs that have a provider (domain → org, enabled, enforced) and each org's built provider (metadata cached one hour, kept across reloads while the row is unchanged). It is reloaded after every org SSO write and every 60 s (which also picks up org drops and the CLI), so every check is synchronous. |
| D-P-63 | **Break-glass.** `llmobs admin org-sso disable <org>` sets the org's provider disabled and not enforced in `meta.db`; a running server applies it within 60 s (D-P-62). |
| D-P-64 | **DNS.** `hickory-resolver` with the system configuration (`/etc/resolv.conf`); no fallback to a public resolver. A lookup has a 10 s timeout; a failure answers `503`. Without a usable system configuration the server starts, logs a warning, and verification answers `503`. |
| D-P-65 | **One callback.** The org flow reuses the OAuth state cookie (`Path=/api/auth/callback`) with provider `sso` and the org id inside it; `GET /api/auth/callback/{provider}` hands `sso` to the org flow. New login error codes: `seat_limit`, `sso_required`, `sso_unavailable` (no org provider for that domain). |

## Conventions

The same as M3 (`03-m3-units-and-limits.md` § Conventions) and M2 (`02-m2-org-entitlements.md` § Conventions): one commit per sub-phase with the subject given; server code returns `ApiError`; no `unwrap()`/`expect()` outside tests without an invariant comment; time from `state.clock.now_ns()`; tests on `TestApp` (manual clock `2026-01-01T00:00:00Z`); web tests on MSW with `server.use`, never `vi.mock`.

**`ee` files.** Every `.rs`, `.ts` and `.tsx` file under `crates/server/src/ee/` or `web/src/ee/` starts with exactly `// SPDX-License-Identifier: Elastic-2.0`, and no other file contains that string (`just check-ee`). New `ee` files in M5: everything under `crates/server/src/ee/org_sso/` and `web/src/ee/org-sso/`. The API types (`crates/server/src/api/org_sso.rs`), the migration, the route file `web/src/routes/$org/settings/sso.tsx` and the core changes (login page, `auth.rs`, `oauth.rs`, `flow.rs`, sessions) stay MIT, as M1 kept the override types in `api/members.rs` and only the handlers in `ee`.

**Privacy.** Never log or return a client secret, an email, a domain, an issuer URL, a subject, a TXT token in logs, or an id_token. Logs carry org ids, user ids and counts. Problem details are static text.

### Module wiring

- `crates/server/src/ee/mod.rs` gains `pub(crate) mod org_sso;` (P3).
- `crates/server/src/ee/org_sso/mod.rs` declares `pub(crate) mod store;` (P3), `pub(crate) mod dns;` (P5), `pub(crate) mod settings;` (P7), `pub(crate) mod login;` (P10).
- `crates/server/src/api/mod.rs` gains `pub mod org_sso;` (P7).
- Route tables: `routes.extend(crate::ee::org_sso::settings::routes());` (P7) and `routes.extend(crate::ee::org_sso::login::routes());` (P10) in `http/access.rs`, after `crate::ee::project_roles::routes()`.

### Every new or changed API operation

Exactly M3's checklist (§ Conventions, items 1–8). Here:
- org settings routes are session-only: their security is the session-only scheme; copy `patch_org_member`'s security call, and add each id to the session-only `matches!` arm of `document_lint` in `crates/server/tests/openapi.rs`;
- `post_sso_discover` and `get_sso_authorize` are public (`.securities(Some(Vec::new()))`, the public arm with `post_login`);
- tag `orgs` for `/api/v1/orgs/{org}/sso…`, tag `auth` for `/api/auth/sso/…`;
- the API example for `GET /api/v1/orgs/{org}/sso` is `fixtures/api/static/org-sso.json` (P7).

### Dead code between phases

M2's rule (`02-m2-org-entitlements.md` § Conventions): a module whose items have no non-test caller until a later phase gets `#![cfg_attr(not(test), allow(dead_code))]` at its top with the comment `// Track P M5: <phase> uses these outside tests.`; a single item gets `#[cfg_attr(not(test), allow(dead_code))]` with the same comment. P16 removes every one M5 added. The phases below say where.

### Generated files

`crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json` and `crates/server/tests/snapshots/meta_schema.snap` are regenerated (`just openapi`, `just api-examples`, the snapshot test with `INSTA_UPDATE=always`), never hand-merged.

### Migrations

Take the **next free number** in `crates/server/migrations/meta/` when you implement P3 (M4 and track I may have added some since this plan was written; `0009_usage_notices.sql` was the last one).

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

`just check` before every commit (over 20 minutes). Quick loops: `cargo test -p llmobs-server <filter>`, `cargo test -p llmobs-eval egress`, `just lint`, `just check-ee`, `just deny`, `just check-web`, `cd web && pnpm vitest run <file>`.

## Sub-phases

| # | Subject | What | After |
|---|---|---|---|
| P1 | `Add the org_sso feature and owner permission` | `Feature::OrgSso`, `OrgPerm::ManageSso`, web feature labels | M2, M3 |
| P2 | `Guard org identity-provider requests` | `llmobs_eval::identity_http_client`, `EgressPolicy::allows_url`, `OidcProvider::for_org`, `config::issuer_problem` | P1 |
| P3 | `Store org SSO settings` | migration, `ee/org_sso/store.rs`, sealed secret column | P2 |
| P4 | `Record the org provider of a session` | `web_sessions.sso_org_id` in `start_session`, `session_sso_org` | P3 |
| P5 | `Look up DNS TXT records` | `hickory-resolver`, `ee/org_sso/dns.rs` | P4 |
| P6 | `Keep org SSO routing in memory` | `OrgSso` state, reload at startup, after writes and every 60 s | P5 |
| P7 | `Manage an org's identity provider` | `GET /orgs/{org}/sso`, `PUT/DELETE …/sso/provider`, `org-sso.json` | P6 |
| P8 | `Verify org email domains` | add, verify, remove domains | P7 |
| P9 | `Enable and enforce org SSO` | `PATCH /orgs/{org}/sso`, revocation | P8 |
| P10 | `Sign in through an org's identity provider` | `/api/auth/sso/authorize`, the `sso` callback, linking, JIT, seats | P9 |
| P11 | `Discover SSO from the email domain` | `POST /api/auth/sso/discover`, `AuthConfig.sso_discovery` | P10 |
| P12 | `Refuse other sign-ins under org enforcement` | login, signup, password forgot and reset, instance providers | P11 |
| P13 | `Add the org SSO break-glass command` | `llmobs admin org-sso disable` | P12 |
| P14 | `Ask for the email first on the login page` | `web/src/routes/login.tsx`, `/sso/no-seat` | P13 |
| P15 | `Add the org SSO settings page` | `web/src/ee/org-sso/`, `/$org/settings/sso` | P13 |
| P16 | `Close out M5` | dead-code attributes, placeholders, specs check, roadmap | all |

P1–P13 run in order, one at a time: they edit the same server files (`http/access.rs`, `api/openapi.rs`, `tests/openapi.rs`, `test_support/mod.rs`) and the generated contract. P14 and P15 touch disjoint web files and need only the contract of P1–P13, so they may run in parallel, each in its own worktree, after P13. P16 runs last.

---

## P1 — The feature and the permission

### `crates/server/src/api/entitlements.rs`

`Feature` gains `OrgSso` **after** `SsoEnforcement` (the bit is the declaration index; append only, D-P-12):
- doc `/// Per-org OIDC providers, domains and enforcement (M5): Teams add-on or an enterprise licence.`;
- `ALL` gains it (length 4);
- `name()`: `"org_sso"`;
- `is_ee()`: `ProjectRoles | SsoEnforcement | OrgSso`;
- `locked_detail()`: `"Single sign-on for an organization needs the Teams add-on or an enterprise licence."`.

### `crates/server/src/auth/roles.rs`

`OrgPerm::ManageSso` (doc `/// The org's identity provider, domains and enforcement (owner only, D-P-51).`), `min_role` → `OrgRole::Owner`; the existing `min_role` test gains `assert_eq!(OrgPerm::ManageSso.min_role(), OrgRole::Owner);`. Add the arm wherever `OrgPerm` is matched exhaustively (`cargo build` lists them).

### Web

- `web/src/components/plan/feature-label.ts`: `case "org_sso": return "Available with the Teams add-on or an enterprise licence";`.
- `web/src/routes/$org/settings/plan.tsx`: `FEATURES` gains `"org_sso"` last; its label map gains `org_sso: "Single sign-on"`.
- `web/src/ee/license-card.tsx`: its label map gains `org_sso: "Single sign-on"`.
- Tests: `web/src/components/plan/locked-feature.test.tsx` gains a case for `org_sso` (the offer text); `plan.test.tsx`'s feature list assertion, if it lists every feature, gains "Single sign-on".

### Tests (server)

- `api::entitlements` tests (or `entitlements::tests`): `Feature::from_name("org_sso") == Some(Feature::OrgSso)`, `OrgSso.is_ee()`.
- `config` test: `[entitlements.defaults] features = ["org_sso"]` is refused like `project_roles` (follow the existing test of an `ee` name in defaults).
- `entitlements::tests`: `TestApp::with_license(&["org_sso"])` → `of(state, org).allows(Feature::OrgSso)`; without a licence → false.

### Done

`just openapi` (twice, no change the second time), `just check`.

---

## P2 — Guarded identity-provider requests (D-P-53)

### `crates/eval/src/egress.rs` (track I's crate; this is the only change M5 makes there)

```rust
impl EgressPolicy {
    /// Whether a request to `url` may be attempted: `http` or `https`, and a literal IP host
    /// that is not denied (D166) unless private networks are allowed. A host name is allowed
    /// here; the resolver checks its addresses.
    #[must_use]
    pub fn allows_url(&self, url: &Url) -> bool
}

/// A client for an identity provider an org registered (D-P-53): the same guarded
/// resolver as [`EgressClient`], no redirects, no proxy, rustls, a 10 s whole-request timeout.
///
/// # Errors
///
/// Returns [`EgressError::Request`] when the TLS backend cannot be initialized.
pub fn identity_http_client(policy: EgressPolicy) -> Result<reqwest::Client, EgressError>
```

`allows_url`: `matches!(url.scheme(), "http" | "https") && (self.allow_private_network || !literal_ip(url).is_some_and(is_denied))`. `identity_http_client`: the builder of `EgressClient::new` with `.timeout(Duration::from_secs(10))` added. `EgressClient::post` uses `allows_url` for its scheme and literal checks (keep its distinct `Scheme`/`Denied` errors by checking the scheme first, then `allows_url`). Re-export `identity_http_client` from `crates/eval/src/lib.rs` next to `EgressClient`.

Tests (`egress.rs` tests): `allows_url` on `https://idp.example`, `http://10.0.0.1/` (denied, allowed with the policy), `ftp://x` (false), `https://[::1]/` (denied); `identity_http_client(policy false)` refuses `http://127.0.0.1:<port>` of a local listener (the request errors, nothing is accepted), and with `allow_private_network: true` reaches it.

### `crates/server/src/config.rs`

Extract from `validate_issuer`:

```rust
/// Why `issuer` cannot be an OIDC issuer: not a URL, user info, not `https` (or `http` on a
/// loopback host), or a multi-tenant Microsoft issuer. `None` when it can.
pub(crate) fn issuer_problem(issuer: &str) -> Option<&'static str>
```

returning the same three reason strings `validate_issuer` uses today (`"invalid issuer"`, `"issuer must be https, or http on a loopback host"`, `"multi-tenant Microsoft issuers are not allowed"`); `validate_issuer` becomes `issuer_problem(issuer).map_or(Ok(()), |reason| Err(ConfigError::Provider { key, reason }))`. The existing config tests keep passing unchanged; add `issuer_problem` unit cases for each reason and `None`.

### `crates/server/src/auth/oidc/provider.rs`

- `OidcProvider` gains `endpoint_policy: Option<llmobs_eval::EgressPolicy>` (`None` from `from_google`, `from_generic`, `for_test`).
- New constructor:

```rust
/// An org's provider (D-P-52, D-P-53): id `org:<org_id>`, default scopes, no domain list,
/// no auto-provision, no linking flag (the org flow decides), and discovered endpoints checked
/// against `policy`. `http` must come from `llmobs_eval::identity_http_client`.
pub(crate) fn for_org(
    org_id: OrgId,
    display_name: &str,
    issuer: &str,
    client_id: &str,
    client_secret: Secret<String>,
    http: reqwest::Client,
    policy: llmobs_eval::EgressPolicy,
) -> Result<Self, OidcError>
```

  It calls `Self::new(OidcKind::Generic, format!("org:{org_id}"), display_name.trim().to_owned(), issuer, client_id, client_secret, default_scopes(), Vec::new(), None, false, http)` then sets `endpoint_policy = Some(policy)`.
- `metadata` and `refresh_metadata`: after a successful `discover_async`, when `endpoint_policy` is `Some(policy)` and `!endpoints_allowed(&fresh, policy)`, log `tracing::warn!("identity provider endpoints refused")` (no URL) and treat it as a failed discovery (do not cache it; `metadata` falls back to the cached document as for any failure, `refresh_metadata` returns `Unavailable`).

```rust
/// `token_endpoint` (when present) and `jwks_uri` pass `policy.allows_url`.
fn endpoints_allowed(metadata: &CoreProviderMetadata, policy: EgressPolicy) -> bool
```

  (`metadata.token_endpoint().map(|url| url.url())`, `metadata.jwks_uri().url()`; check the accessor names in `openidconnect` 4.1 docs.)
- Tests: `for_org` builds id `org:<uuid>`, keeps the secret out of `Debug`; with the mock IdP (`MockIdp::start()`), `identity_http_client(EgressPolicy { allow_private_network: true })` loads metadata; with `allow_private_network: false` metadata is `Unavailable` (the resolver drops 127.0.0.1); `endpoints_allowed` refuses a document whose `jwks_uri` is `http://169.254.169.254/` (build the metadata from JSON with `serde_json::from_value::<CoreProviderMetadata>`, copying the mock's discovery document and changing `jwks_uri`).

`for_org` has no caller until P6: the module already carries `#![cfg_attr(not(test), allow(dead_code))]`; nothing to add.

### Spec

`docs/spec/12-operations.md` § Security, the SSRF paragraph: replace "Identity-provider URLs are operator configuration and are not passed through that guard." with "Identity providers from `[auth]` are operator configuration and are not passed through that guard; an org's own identity provider (track P's M5) is: its discovery, token and JWKS requests use the guarded resolver, and its discovered endpoints must not be denied literal addresses (D-P-53)."

### Done

`just check`.

---

## P3 — Storage

### Migration `NNNN_org_sso.sql` (next free number)

```sql
-- Track P M5: per-org OIDC providers and email domains (D-P-52, D-P-54).
CREATE TABLE org_sso_providers (
    id TEXT PRIMARY KEY,                -- UUIDv7; the row id in client_secret_enc's associated data
    org_id TEXT NOT NULL UNIQUE REFERENCES orgs(id) ON DELETE CASCADE,
    display_name TEXT NOT NULL,
    issuer TEXT NOT NULL,
    client_id TEXT NOT NULL,
    client_secret_enc BLOB NOT NULL,    -- sealed (D-I-2)
    enabled INTEGER NOT NULL DEFAULT 0,
    enforced INTEGER NOT NULL DEFAULT 0,
    jit_role TEXT NOT NULL DEFAULT 'viewer',   -- 'viewer' | 'member'
    last_login_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE org_sso_domains (
    id TEXT PRIMARY KEY,
    org_id TEXT NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
    domain TEXT NOT NULL,               -- lowercase, no trailing dot
    token TEXT NOT NULL,                -- 32 lowercase hex; the TXT value's random part (public)
    verified_at INTEGER,
    last_checked_at INTEGER,
    created_at INTEGER NOT NULL,
    UNIQUE (org_id, domain)
);

CREATE UNIQUE INDEX org_sso_domains_verified ON org_sso_domains(domain) WHERE verified_at IS NOT NULL;

-- The org whose provider started the session (D-P-59).
ALTER TABLE web_sessions ADD COLUMN sso_org_id TEXT REFERENCES orgs(id) ON DELETE SET NULL;
```

Regenerate `crates/server/tests/snapshots/meta_schema.snap` (`INSTA_UPDATE=always cargo test -p llmobs-server schema_matches_the_snapshot`; review: two tables, one index, one column) and commit it.

### `crates/server/src/crypto.rs`

`ENCRYPTED_COLUMNS` gains `("org_sso_providers", "client_secret_enc")`. The test that compares it with every `%_enc` column keeps passing; `admin::rotate_encryption_key` re-seals it with no other change (it selects `id`, which this table has).

### `crates/server/src/ee/org_sso/mod.rs` (new, `ee`)

For now only:

```rust
// SPDX-License-Identifier: Elastic-2.0
//! Per-org SSO (track P M5, D-P-25, D-P-41): org OIDC providers, verified email domains,
//! enforcement and login discovery. Self-hosted instances; works for any org with `org_sso`.

pub(crate) mod store;
```

and `pub(crate) mod org_sso;` in `ee/mod.rs`.

### `crates/server/src/ee/org_sso/store.rs` (new, `ee`)

`#![cfg_attr(not(test), allow(dead_code))]` with `// Track P M5: P6–P10 use these outside tests.`

```rust
/// `jit_role` (D-P-56). Never admin or owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JitRole { Viewer, Member }
// as_str: "viewer" | "member"; from_str_opt(&str) -> Option<Self>; fn org_role(self) -> OrgRole

/// One `org_sso_providers` row. `Debug` omits the sealed secret.
pub(crate) struct ProviderRow {
    pub(crate) id: Uuid,
    pub(crate) org_id: OrgId,
    pub(crate) display_name: String,
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) client_secret_enc: Vec<u8>,
    pub(crate) enabled: bool,
    pub(crate) enforced: bool,
    pub(crate) jit_role: JitRole,
    pub(crate) last_login_at: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// One `org_sso_domains` row.
#[derive(Debug, Clone)]
pub(crate) struct DomainRow {
    pub(crate) id: Uuid,
    pub(crate) org_id: OrgId,
    pub(crate) domain: String,
    pub(crate) token: String,
    pub(crate) verified_at: Option<i64>,
    pub(crate) last_checked_at: Option<i64>,
    pub(crate) created_at: i64,
}

/// What `put_provider` writes. `client_secret_enc: None` keeps the stored one.
pub(crate) struct ProviderWrite {
    pub(crate) display_name: String,
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) client_secret_enc: Option<Vec<u8>>,
    pub(crate) jit_role: JitRole,
}

/// `AAD` of `client_secret_enc` for provider row `id` (table `org_sso_providers`).
pub(crate) fn secret_aad(id: &Uuid) -> Aad<'_>

pub(crate) async fn provider(pool: &SqlitePool, org: OrgId) -> Result<Option<ProviderRow>, MetaError>;
pub(crate) async fn provider_tx(conn: &mut SqliteConnection, org: OrgId) -> Result<Option<ProviderRow>, MetaError>;
/// Providers of active orgs, ordered by org id.
pub(crate) async fn active_providers(pool: &SqlitePool) -> Result<Vec<ProviderRow>, MetaError>;
/// Inserts (with `id`, enabled and enforced off) or updates the org's provider. On insert
/// `client_secret_enc` must be `Some` (else `MetaError::Corrupt`, a caller bug).
pub(crate) async fn put_provider(conn: &mut SqliteConnection, org: OrgId, id: Uuid, write: ProviderWrite, now: i64) -> Result<(), MetaError>;
/// Sets the two flags and `updated_at`.
pub(crate) async fn set_flags(conn: &mut SqliteConnection, org: OrgId, enabled: bool, enforced: bool, now: i64) -> Result<bool, MetaError>;
pub(crate) async fn set_last_login(conn: &mut SqliteConnection, org: OrgId, now: i64) -> Result<(), MetaError>;
/// Deletes the provider row and the org's identities (`identities.provider = 'org:<org>'`).
pub(crate) async fn delete_provider(conn: &mut SqliteConnection, org: OrgId) -> Result<bool, MetaError>;
/// Deletes the org's identities only (issuer change, D-P-52).
pub(crate) async fn delete_identities(conn: &mut SqliteConnection, org: OrgId) -> Result<u64, MetaError>;
/// `org:<org>`, the `identities.provider` of the org's provider.
pub(crate) fn identity_provider(org: OrgId) -> String;

/// The org's domains, ordered by domain.
pub(crate) async fn domains(pool: &SqlitePool, org: OrgId) -> Result<Vec<DomainRow>, MetaError>;
pub(crate) async fn domains_tx(conn: &mut SqliteConnection, org: OrgId) -> Result<Vec<DomainRow>, MetaError>;
pub(crate) async fn domain_tx(conn: &mut SqliteConnection, org: OrgId, id: Uuid) -> Result<Option<DomainRow>, MetaError>;
/// Inserts a pending domain. `MetaError::Conflict` when the org already lists it.
pub(crate) async fn insert_domain(conn: &mut SqliteConnection, row: &DomainRow) -> Result<(), MetaError>;
/// `verified_at = now, last_checked_at = now`. `MetaError::Conflict` when another org verified
/// it first (the partial unique index).
pub(crate) async fn mark_verified(conn: &mut SqliteConnection, id: Uuid, now: i64) -> Result<(), MetaError>;
pub(crate) async fn mark_checked(conn: &mut SqliteConnection, id: Uuid, now: i64) -> Result<(), MetaError>;
/// Whether another org has `domain` verified.
pub(crate) async fn verified_elsewhere_tx(conn: &mut SqliteConnection, org: OrgId, domain: &str) -> Result<bool, MetaError>;
pub(crate) async fn delete_domain(conn: &mut SqliteConnection, org: OrgId, id: Uuid) -> Result<bool, MetaError>;

/// Verified domains of active orgs that have a provider, with the provider's flags (P6's index).
pub(crate) async fn routes(pool: &SqlitePool) -> Result<Vec<(String, OrgId, bool, bool)>, MetaError>;
```

`routes` SQL:

```sql
SELECT d.domain, d.org_id, p.enabled, p.enforced
FROM org_sso_domains d
JOIN org_sso_providers p ON p.org_id = d.org_id
JOIN orgs o ON o.id = d.org_id
WHERE d.verified_at IS NOT NULL AND o.state = 'active'
ORDER BY d.domain
```

`active_providers` joins `orgs` the same way. Map stored text the way `meta/invites.rs` does (a bad uuid, role or flag is `MetaError::Corrupt { table, column }`). `MetaError::Conflict` for unique violations comes from the existing `From<sqlx::Error>` (check how `insert_org` gets it). The `ProviderRow` `Debug` impl is manual and omits `client_secret_enc`.

### Tests (`ee::org_sso::store::tests`, on `crate::meta::test_db()` with an org from `meta::orgs::insert_org`)

1. `provider_round_trip`: insert, read back every field, update without a secret keeps the sealed bytes, update with one replaces them; `Debug` does not contain the sealed bytes' hex.
2. `domain_verified_by_one_org_only`: two orgs list `acme.test`; verifying it for A succeeds; `mark_verified` for B is `Conflict`; `verified_elsewhere_tx(B, "acme.test")` is true; after `delete_domain(A)` B verifies.
3. `routes_skip_unverified_inactive_and_providerless`: verified domain with provider → listed with its flags; pending domain → absent; verified domain of an org without provider → absent; an org with `state = 'deleting'` (`meta::orgs::mark_org_deleting`) → absent.
4. `delete_provider_removes_identities`: an identity `org:<org>` and one `oidc:x` → only the first goes.
5. `org_drop_cascades`: `meta::orgs::delete_org` removes both tables' rows and sets `web_sessions.sso_org_id` to NULL (insert a session row with it set through SQL).

### Spec

`docs/spec/04-metadata-db.md` § Identity & tenancy: after `web_sessions`, the `sso_org_id` column (one line, D-P-59); a new section **Per-org SSO (track P, `ee`)** before § Encryption of secrets with the two tables and the index (copy the SQL above). § Encryption of secrets: the sealed columns list (if it lists them) gains `org_sso_providers.client_secret_enc`.

### Done

`just check`.

---

## P4 — The org provider of a session (D-P-59)

### `crates/server/src/meta/web_sessions.rs`

- `NewSession` gains `pub(crate) sso_org_id: Option<OrgId>`; `insert_session` writes it (`sso_org_id` column, bound as `Option<String>`).
- New:

```rust
/// The org whose provider started session `hash`. `None` for another sign-in or no session.
pub(crate) async fn session_sso_org(pool: &SqlitePool, hash: &str) -> Result<Option<OrgId>, MetaError>
```

  It has no caller until P9: `#[cfg_attr(not(test), allow(dead_code))]` (the module already has a module-level allow; if so, nothing to add).

### `crates/server/src/auth/session.rs`

`start_session` gains a last parameter `sso_org: Option<OrgId>` (doc: "The org whose identity provider signed the user in (D-P-59); `None` otherwise") and passes it to `NewSession`. Update **every** caller: pass `None` everywhere (there are about 25, most in tests: `grep -rn "start_session(" crates/server/src`). The OAuth callback passes `None` until P10.

### Tests (`auth::session::tests`)

`a_session_records_its_org_provider`: `start_session(.., Some(org))` → the cookie's hash (`hash_token` of the cookie value) has `session_sso_org == Some(org)`; `start_session(.., None)` → `None`; an unknown hash → `None`.

### Done

`just check`.

---

## P5 — DNS TXT lookups (D-P-64)

### Dependency

Root `Cargo.toml` `[workspace.dependencies]`, with a comment:

```toml
# DNS TXT lookups for org domain verification (track P M5, D-P-64). System resolver config.
hickory-resolver = "0.26.3"
```

`crates/server/Cargo.toml`: `hickory-resolver = { workspace = true }`. Keep the default features (`system-config`, `tokio`). Run `just deny`: if it reports a licence outside the allow-list or a banned duplicate, stop and report it instead of changing `deny.toml`.

### `crates/server/src/ee/org_sso/dns.rs` (new, `ee`)

`#![cfg_attr(not(test), allow(dead_code))]` with `// Track P M5: P6 and P8 use these outside tests.`

```rust
//! TXT lookups for domain verification (D-P-54, D-P-64).

/// Why a lookup failed. No variant holds the name.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum DnsError {
    /// No usable system resolver configuration.
    #[error("no DNS resolver is configured")]
    Unconfigured,
    /// The lookup failed or timed out.
    #[error("DNS lookup failed")]
    Lookup,
}

/// Where TXT records come from. Tests use `Fixed`, since they have no network.
pub(crate) enum TxtResolver {
    System(hickory_resolver::TokioResolver),
    /// The system configuration could not be read at startup.
    Unconfigured,
    #[cfg(test)]
    Fixed(std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>),
}

impl TxtResolver {
    /// The system resolver; `Unconfigured` (with one `warn`, no detail) when it cannot be built.
    pub(crate) fn system() -> Self;
    #[cfg(test)]
    pub(crate) fn fixed() -> Self;
    /// Sets the records of `name` (tests).
    #[cfg(test)]
    pub(crate) fn set(&self, name: &str, values: &[&str]);
    /// The TXT strings at `name`, each record's character-strings joined (RFC 7208 § 3.3).
    /// No record and NXDOMAIN are `Ok(vec![])`. 10 s timeout.
    pub(crate) async fn txt(&self, name: &str) -> Result<Vec<String>, DnsError>;
}

/// `_llmobs-verify.<domain>`.
pub(crate) fn record_name(domain: &str) -> String;
/// `llmobs-verify=<token>`.
pub(crate) fn record_value(token: &str) -> String;
/// A new token: 16 random bytes as 32 lowercase hex characters.
pub(crate) fn new_token() -> Result<String, crate::auth::AuthError>;
```

- `record_name`: `format!("{}.{domain}", concat!("_", llmobs_core::llmobs_name!("-verify")))`; `record_value`: `format!("{}={token}", llmobs_core::llmobs_name!("-verify"))`.
- `new_token`: `crate::auth::secret::random_bytes(16)` then lowercase hex (the module has a hex helper; reuse it).
- `system`: `hickory_resolver::TokioResolver::builder_tokio()` then `.build()` (both return `Result` in 0.26; confirm with `cargo doc -p hickory-resolver`). `Debug` for `TxtResolver` is manual (`"TxtResolver::System"` etc.).
- `txt`: `tokio::time::timeout(Duration::from_secs(10), resolver.txt_lookup(name))`; a timeout or an error is `Lookup`, except the "no records" error (`NetError::Dns(DnsError::NoRecordsFound(_))` in 0.26, check the exact path) which is `Ok(vec![])`. Each TXT rdata's `txt_data()` segments are concatenated, decoded with `String::from_utf8_lossy`. `Unconfigured` → `Err(DnsError::Unconfigured)`. `Fixed` → the stored list or empty.

### Tests (`ee::org_sso::dns::tests`)

`record_name("acme.test") == "_llmobs-verify.acme.test"`; `record_value("ab") == "llmobs-verify=ab"`; `new_token` is 32 chars of `[0-9a-f]` and two calls differ; `Fixed` returns what `set` stored and `[]` for another name; `Unconfigured.txt(..)` is `Err(Unconfigured)`. No test touches the network.

### Done

`just deny`, `just check`.

---

## P6 — Routing in memory (D-P-62)

### `crates/server/src/ee/org_sso/mod.rs`

`#![cfg_attr(not(test), allow(dead_code))]` with `// Track P M5: P7–P12 use these outside tests.`

```rust
/// A verified domain's org and its provider's flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DomainRoute {
    pub(crate) org_id: OrgId,
    pub(crate) enabled: bool,
    pub(crate) enforced: bool,
}

/// An org's built provider. `Debug` omits the client secret (OidcProvider's own `Debug` does).
#[derive(Debug)]
pub(crate) struct OrgProvider {
    pub(crate) org_id: OrgId,
    pub(crate) oidc: OidcProvider,
    pub(crate) jit_role: store::JitRole,
    updated_at: i64,
}

#[derive(Debug, Default)]
struct Index {
    domains: HashMap<String, DomainRoute>,
    providers: HashMap<OrgId, Arc<OrgProvider>>,
}

/// Org SSO state in `AppInner` (D-P-62).
#[derive(Debug)]
pub(crate) struct OrgSso {
    index: std::sync::RwLock<Arc<Index>>,
    http: reqwest::Client,
    policy: llmobs_eval::EgressPolicy,
    pub(crate) dns: dns::TxtResolver,
}

/// Why `OrgSso::new` failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OrgSsoError {
    #[error("could not build the identity provider client")]
    Client,
}

/// What `reload` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reloaded {
    /// Verified domains ignored because instance config lists them (D-P-54).
    pub(crate) claimed: usize,
    /// Providers whose secret could not be opened (skipped).
    pub(crate) unreadable: usize,
}

impl OrgSso {
    pub(crate) fn new(policy: EgressPolicy, dns: dns::TxtResolver) -> Result<Self, OrgSsoError>;
    /// Rebuilds the index from `meta.db`. A provider whose `updated_at` did not change keeps
    /// its `Arc` (and so its cached metadata).
    pub(crate) async fn reload(&self, meta: &MetaDb, secrets: &SecretBox, auth: &AuthSettings) -> Result<Reloaded, MetaError>;
    /// The route of `email`'s domain (the part after the last `@`), if any.
    pub(crate) fn route(&self, email: &str) -> Option<DomainRoute>;
    pub(crate) fn route_domain(&self, domain: &str) -> Option<DomainRoute>;
    pub(crate) fn provider(&self, org: OrgId) -> Option<Arc<OrgProvider>>;
    /// The org's verified, routed domains, sorted.
    pub(crate) fn domains_of(&self, org: OrgId) -> Vec<String>;
    /// Whether any routed domain has an enabled provider (`AuthConfig.sso_discovery`).
    pub(crate) fn discovery_enabled(&self) -> bool;
    /// A provider built from settings (the save-time discovery check of P7 and `reload`).
    pub(crate) fn build_provider(&self, org: OrgId, display_name: &str, issuer: &str, client_id: &str, secret: Secret<String>) -> Result<OidcProvider, OidcError>;
}

/// Whether instance config owns `domain` (D-P-54): `enforce_sso_domains`, Google's
/// `allowed_domains` or any `[[auth.oidc]].allowed_domains`.
pub(crate) fn instance_claims(auth: &AuthSettings, domain: &str) -> bool;

/// Whether `email` must sign in through its org's provider (D-P-58). The caller exempts
/// instance admins.
pub(crate) fn enforces(state: &AppState, email: &str) -> bool {
    state.org_sso.route(email).is_some_and(|route| route.enforced)
}

/// Reloads and logs failures (`warn`, error class only). Called after every org SSO write.
pub(crate) async fn refresh(state: &AppState);
```

- `new`: `http = llmobs_eval::identity_http_client(policy).map_err(|_| OrgSsoError::Client)?`.
- `reload`: `store::routes` → skip `instance_claims` (count `claimed`) → domains map. `store::active_providers` → for each row: reuse the current `Arc` when its `updated_at` equals the row's; else open the secret (`secrets.open(&row.client_secret_enc, store::secret_aad(&row.id))`, UTF-8) and `build_provider`; an open or build failure is `tracing::warn!(org_id = %row.org_id, "org identity provider skipped")`, counted in `unreadable`. Swap the `Arc<Index>` in one write-lock.
- `route`: `email.rsplit_once('@')` → `route_domain(domain)`. Emails are already lowercase (`normalize_email`).
- Lock poisoning: use `.read().unwrap_or_else(std::sync::PoisonError::into_inner)` (no panic), same for write.
- `refresh(state)`: `if let Err(error) = state.org_sso.reload(&state.meta, &state.secrets, &state.config.auth).await { tracing::warn!(error = %error, "org SSO reload failed") }`. Make sure `MetaError`'s `Display` carries no payload (it does not: it is SQL error text of our queries; check and use the error kind only if unsure).

### Wiring

- `http/mod.rs` `AppInner` gains `pub(crate) org_sso: crate::ee::org_sso::OrgSso` (doc: "Per-org SSO routing (track P M5)").
- `serve.rs`: build it with `OrgSso::new(EgressPolicy { allow_private_network: config.security.allow_private_network_egress }, TxtResolver::system())` (an error fails startup like the eval runner's client does); after `AppState` exists, `let reloaded = state.org_sso.reload(...).await?` (a `MetaError` fails startup as other startup loads do); when `reloaded.claimed > 0`, `tracing::warn!(domains = reloaded.claimed, "verified org SSO domains are ignored: the server configuration lists them")`. Spawn a task that calls `crate::ee::org_sso::refresh(&state)` every 60 s, next to the usage flush task, with the same shutdown handling.
- `test_support::open`: `OrgSso::new(EgressPolicy { allow_private_network: config.security.allow_private_network_egress }, TxtResolver::fixed())`, then `reload` after the state is built (no task). Add:

```rust
/// Sets the TXT records the fixed resolver answers for `name`.
pub(crate) fn dns_txt(&self, name: &str, values: &[&str])
```

  on `TestApp` (`self.state.org_sso.dns.set(name, values)`).

### Tests (`ee::org_sso::tests`)

Use `TestApp::with_config(|c| c.security.allow_private_network_egress = true)` and `MockIdp::start()`; insert rows through `store` in a `meta.write`, seal the secret with `app.state.secrets.seal(b"test-secret", store::secret_aad(&id))`.
1. `reload_routes_verified_domains`: provider (disabled) + verified `acme.test` + pending `beta.test` → `route("ada@acme.test") == Some(DomainRoute { enabled: false, enforced: false, .. })`, `route("x@beta.test")` is `None`, `discovery_enabled()` false; after `set_flags(true, false)` and `reload`, true.
2. `claimed_domains_are_ignored`: config `auth.enforce_sso_domains = ["acme.test"]` → `route` is `None`, `Reloaded.claimed == 1`; the same with an `[[auth.oidc]]` whose `allowed_domains` lists it.
3. `unchanged_providers_keep_their_metadata`: `provider(org)` before and after a second `reload` are the same `Arc` (`Arc::ptr_eq`); after `put_provider` (new `updated_at`) and `reload`, a different one.
4. `an_unreadable_secret_is_skipped`: corrupt `client_secret_enc` (`UPDATE … SET client_secret_enc = X'00'`) → `reload` is `Ok` with `unreadable == 1`, `provider(org)` is `None`; the log has the org id and no issuer.
5. `enforces` follows the flag.

### Done

`just check`.

---

## P7 — The provider settings API

### API types (`crates/server/src/api/org_sso.rs`, new, MIT; `pub mod org_sso;` in `api/mod.rs`)

```rust
/// Role given to people who join through the org's provider (D-P-56).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JitRole { Viewer, Member }

/// The org's identity provider. The client secret is write-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSsoProvider {
    pub display_name: String,
    pub issuer: String,
    pub client_id: String,
    /// Discovery sends users to this provider (D-P-57).
    pub enabled: bool,
    /// Password and other sign-ins are refused for the org's domains (D-P-58).
    pub enforced: bool,
    pub jit_role: JitRole,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "crate::api::keys::optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_login_at: Option<i64>,
    #[serde(with = "llmobs_core::time_serde")] #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    #[serde(with = "llmobs_core::time_serde")] #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DomainStatus { Pending, Verified }

/// One email domain of the org and how to prove it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSsoDomain {
    pub id: Uuid,
    pub domain: String,
    pub status: DomainStatus,
    /// TXT record name, `_llmobs-verify.<domain>`.
    pub txt_name: String,
    /// TXT record value, `llmobs-verify=<token>`.
    pub txt_value: String,
    // verified_at, last_checked_at: Option<i64> (optional_time, omitted when None); created_at: i64
}

/// `GET /api/v1/orgs/{org}/sso` and every settings write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OrgSso {
    /// The redirect URI to register at the identity provider.
    pub callback_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<OrgSsoProvider>,
    /// Ordered by domain.
    pub domains: Vec<OrgSsoDomain>,
}

/// `PUT /api/v1/orgs/{org}/sso/provider`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutOrgSsoProvider {
    pub display_name: String,
    pub issuer: String,
    pub client_id: String,
    /// Required when creating, and when the issuer or client id changes; omit to keep it.
    #[serde(default)]
    pub client_secret: Option<String>,
    pub jit_role: JitRole,
}
```

(Check `crate::api::keys::optional_time`'s exact path, as M1 notes.) `PutOrgSsoProvider` gets a manual `Debug` that omits `client_secret`. Register every type in `collect_schemas`.

### Routes (`crates/server/src/ee/org_sso/settings.rs`, new, `ee`)

All three: `Access::Org { perm: OrgPerm::ManageSso, session_only: true }`, `documented: true`.

- `GET /api/v1/orgs/{org}/sso` → `get_org_sso`: `view(&state, org).await?` (shared helper below). No feature needed (D-P-61).
- `PUT /api/v1/orgs/{org}/sso/provider` → `put_org_sso_provider`:
  1. `entitlements::require(&state, org, Feature::OrgSso)?`.
  2. Validate (field errors through the same helper shape as `routes/auth.rs::field_error`; make a local copy or a `pub(crate)` one in `http/error.rs` if one exists): `display_name` trimmed 1–100 chars; `issuer` trimmed, at most 512 bytes, `config::issuer_problem` → `field_error("issuer", reason)`; `client_id` trimmed 1–255; `client_secret` trimmed 1–1024 when present.
  3. `let existing = store::provider(reader, org).await?`. The secret to use: the new one, else (when `existing` and issuer and client id unchanged) the stored one opened with `secret_aad`, else `field_error("client_secret", "required")` (create) or `field_error("client_secret", "required when the issuer or client id changes")`.
  4. Discovery check: `state.org_sso.build_provider(org, …)` (`OidcError` → `field_error("issuer", "invalid issuer")`), then `.metadata(now).await` → `Err` → `field_error("issuer", "the issuer's discovery document could not be loaded")`.
  5. Write (`meta.write`): `id = existing.map_or_else(Uuid::now_v7, |row| row.id)`; seal the new secret (when any) with `secret_aad(&id)`; `store::put_provider`; when `existing` had another issuer, `store::delete_identities(conn, org)`.
  6. `refresh(&state).await`; `tracing::info!(target: "audit", org_id = %org, user_id = %caller, "org sso provider saved")`.
  7. `200` with `view(&state, org)`.
- `DELETE /api/v1/orgs/{org}/sso/provider` → `delete_org_sso_provider`: no feature check. In one `meta.write`: no row → `NotFound`; `enforced` → `ApiError::Conflict { detail: "Turn off enforcement before removing the identity provider." }`; else `store::delete_provider`. `refresh`; audit `"org sso provider removed"`; `204`.

```rust
/// The settings view: provider (without secret), domains, callback URL.
async fn view(state: &AppState, org: OrgId) -> Result<OrgSso, ApiError>
```

`callback_url` is `state.config.public_url.join("/api/auth/callback/sso")`. Domains map to `OrgSsoDomain` with `dns::record_name` / `dns::record_value`.

### OpenAPI (per § Conventions)

`get_org_sso` (errors `401, 403, 404, 429`), `put_org_sso_provider` (`400, 401, 403, 404, 429`; `QUERY_OR_BODY`), `delete_org_sso_provider` (`401, 403, 404, 409, 429`). Path parameter `org` as the other org routes. Example `fixtures/api/static/org-sso.json`:

```json
{
  "callback_url": "http://localhost:8080/api/auth/callback/sso",
  "provider": {
    "display_name": "Acme Okta",
    "issuer": "https://acme.okta.com",
    "client_id": "0oa1b2c3d4",
    "enabled": true,
    "enforced": false,
    "jit_role": "viewer",
    "last_login_at": "2026-01-01T00:00:00.000000000Z",
    "created_at": "2026-01-01T00:00:00.000000000Z",
    "updated_at": "2026-01-01T00:00:00.000000000Z"
  },
  "domains": [
    {
      "id": "01900000-0000-7000-8000-000000000701",
      "domain": "acme.com",
      "status": "verified",
      "txt_name": "_llmobs-verify.acme.com",
      "txt_value": "llmobs-verify=0123456789abcdef0123456789abcdef",
      "verified_at": "2026-01-01T00:00:00.000000000Z",
      "last_checked_at": "2026-01-01T00:00:00.000000000Z",
      "created_at": "2026-01-01T00:00:00.000000000Z"
    },
    {
      "id": "01900000-0000-7000-8000-000000000702",
      "domain": "acme.io",
      "status": "pending",
      "txt_name": "_llmobs-verify.acme.io",
      "txt_value": "llmobs-verify=fedcba9876543210fedcba9876543210",
      "created_at": "2026-01-01T00:00:00.000000000Z"
    }
  ]
}
```

(How `api_examples.rs` builds a static example's instant fields decides the exact time text; follow `org-entitlements.json`.)

### Test helper (`crate::test_support`)

```rust
/// An org SSO test app: private egress allowed, the seeded org with a licence listing `org_sso`,
/// and the mock IdP's issuer. Returns the app, the seeded data and the owner's session cookie.
pub(crate) async fn org_sso_app(idp: &MockIdp) -> (TestApp, Seeded, String)
```

(`TestApp::with_config` setting `allow_private_network_egress = true` and `license_key = license_key(&["org_sso"])`, then `seed()`; the cookie is how other tests get an owner session — follow `routes/members.rs` tests.) And:

```rust
/// Saves the org's provider pointing at `idp` (client id/secret of the mock), enabled or not,
/// verifies `domain` directly in `meta.db`, and reloads. For tests of later phases.
pub(crate) async fn configure_org_sso(app: &TestApp, org: OrgId, idp: &MockIdp, domain: &str, enabled: bool, enforced: bool)
```

### Tests (`ee::org_sso::settings::tests`)

1. `owner_saves_and_reads_the_provider`: PUT with the mock issuer, `CLIENT_ID`, `CLIENT_SECRET`, `jit_role: member` → 200, `provider.enabled == false`, no field in the JSON contains `test-secret`; GET returns the same; the row's secret opens to `test-secret`.
2. `admins_and_members_cannot`: an admin's session → 403; a viewer → 403; a PAT of the owner → 403 (session-only); another org's owner → 404.
3. `create_needs_a_secret` (400 `client_secret`); `issuer_change_needs_a_secret` and deletes `org:<org>` identities (insert one first); an update without a secret and the same issuer keeps it.
4. `bad_issuers_are_400`: `ftp://x`, `https://login.microsoftonline.com/common/v2.0`, a URL whose discovery fails (`idp.fail_discovery()`) → 400 `issuer`; with `allow_private_network_egress = false`, the mock's loopback issuer → 400 (the guard).
5. `writes_need_the_feature_and_removal_does_not` (D-P-61): an app without the licence: PUT → 403 `plan_required` with `Feature::OrgSso.locked_detail()`; a provider inserted through `store` can be deleted (204) and read (200).
6. `an_enforced_provider_cannot_be_removed` → 409.
7. `logs_carry_no_secret_or_issuer`: `assert_logs_clean(&["test-secret", idp.issuer()], …)` around a PUT.

### Spec

`docs/spec/09-api.md` § Orgs, projects, members, keys: a row "`GET /api/v1/orgs/{org}/sso` · `PUT/DELETE /api/v1/orgs/{org}/sso/provider` (owners, session only; the client secret is write-only; track P's M5, `ee`, D-P-51)". P8 and P9 extend it.

### Done

`just openapi` and `just api-examples` (twice each), `just check`.

---

## P8 — Domains (D-P-54)

### Limiter (`crates/server/src/auth/limits.rs`)

`Limits` gains `pub(crate) sso_dns_checks: KeyedLimiter<OrgId>` — 10 per minute per org (doc: "Org domain verifications, D-P-54, D-P-64"), built like `connection_tests`. (`OrgId` is `Hash + Eq + Copy`; if `KeyedLimiter` needs `Clone`, it has it.)

### Domain syntax (`ee/org_sso/mod.rs`)

```rust
/// A domain as stored: trimmed, lowercase ASCII, one trailing dot removed; at most 253
/// characters; at least two labels; each label 1–63 of `[a-z0-9-]`, not starting or ending
/// with `-`; the last label not all digits (refuses IPv4). `None` otherwise.
pub(crate) fn normalize_domain(raw: &str) -> Option<String>
```

Unit tests: `"Acme.COM."` → `acme.com`; `xn--bcher-kva.example` accepted; refused: `""`, `acme`, `-acme.com`, `acme-.com`, `a..com`, `1.2.3.4`, `acme.com/x`, `ädme.com` (non-ASCII: the owner types the punycode form), a 64-character label, a 254-character domain.

### Routes (`settings.rs`), session-only, `OrgPerm::ManageSso`

- `POST /api/v1/orgs/{org}/sso/domains`, body `AddOrgSsoDomain { domain: String }` (`deny_unknown_fields`, in `api/org_sso.rs`) → `post_org_sso_domain`:
  1. `require(.., Feature::OrgSso)?`.
  2. `normalize_domain` → else `field_error("domain", "invalid domain")`; `instance_claims(&state.config.auth, &domain)` → `field_error("domain", "this domain is managed by the server configuration")`.
  3. In `meta.write`: `domains_tx` count ≥ 20 → outcome `TooMany` (`field_error("domain", "an organization can have at most 20 domains")`); `insert_domain` with `id = Uuid::now_v7()`, `token = dns::new_token()?` (generate before the closure), `Conflict` → `ApiError::Conflict { detail: "This domain is already listed." }`.
  4. `201` with `view`. Audit `"org sso domain added"` with the domain **id**.
- `POST /api/v1/orgs/{org}/sso/domains/{id}/verify` → `post_org_sso_domain_verify` (`{id}` is a UUID path parameter; parse failure 404 as the other `{id}` routes do):
  1. `require(.., Feature::OrgSso)?`.
  2. Load the row (`domain_tx` in a read; not this org's → `NotFound`). Already verified → `200` with `view` (idempotent, no lookup).
  3. `state.limits.sso_dns_checks.check(&org, now)` → `ApiError::RateLimited`.
  4. `state.org_sso.dns.txt(&dns::record_name(&row.domain)).await`: `Err(_)` → `mark_checked`, then `ApiError::Unavailable { detail: "The DNS lookup failed. Try again later." }`.
  5. Found = any value equal to `dns::record_value(&row.token)` after trimming whitespace and one pair of surrounding `"`.
  6. In `meta.write`: `verified_elsewhere_tx` → outcome `Elsewhere` (and `mark_checked`); not found → `mark_checked`, outcome `Missing`; found → `mark_verified` (its `Conflict` → `Elsewhere`). P9 adds the enforcement revocation here.
  7. Outcomes: `Elsewhere` → `Conflict { detail: "This domain is verified by another organization." }`; `Missing` → `Conflict { detail: "The TXT record was not found. DNS changes can take a while to appear; try again later." }`; verified → `refresh`, audit `"org sso domain verified"` (domain id), `200` with `view`.
- `DELETE /api/v1/orgs/{org}/sso/domains/{id}` → `delete_org_sso_domain`: no feature check; `delete_domain` false → `NotFound`; `refresh`; audit `"org sso domain removed"`; `204`.

OpenAPI: `post_org_sso_domain` (`400, 401, 403, 404, 409, 429`, `QUERY_OR_BODY`, response 201 `OrgSso`), `post_org_sso_domain_verify` (`401, 403, 404, 409, 429, 503`; add a `problem(503)` response description if none exists: "The DNS lookup failed."), `delete_org_sso_domain` (`401, 403, 404, 429`). Register `AddOrgSsoDomain`.

### Tests (`settings::tests`)

1. `add_verify_and_route`: add `Acme.test` → 201, `domains[0]` is `acme.test`, pending, `txt_name` `_llmobs-verify.acme.test`, `txt_value` `llmobs-verify=<32 hex>`; verify without a record → 409 "not found", `last_checked_at` set; `app.dns_txt(name, &["\"llmobs-verify=<token>\""])` (quoted) → 200 verified; with a provider configured, `app.state.org_sso.route("ada@acme.test")` routes to the org; verify again → 200 without a lookup (set the record to something else first).
2. `one_org_per_domain`: org B (second org owned by another user) adds `acme.test` and verifies with its own token → 409 "another organization".
3. `instance_config_owns_some_domains`: `auth.enforce_sso_domains = ["corp.test"]` → 400.
4. `limits`: 20 domains then the 21st → 400; the 11th verify within a minute → 429.
5. `dns_failure_is_503`: an app whose resolver is `Unconfigured` (replace through a `#[cfg(test)]` setter on `OrgSso`, or build `TxtResolver::Unconfigured` in a dedicated `TestApp` constructor; choose the smaller change) → 503.
6. `remove_works_without_the_feature`: drop the licence (an app without it, rows inserted through `store`) → DELETE 204; POST → 403 `plan_required`.
7. `logs_carry_no_domain`: `assert_logs_clean(&["acme.test"], …)` around add, verify and remove.

### Spec

`docs/spec/09-api.md`: the P7 row gains "`POST /api/v1/orgs/{org}/sso/domains` · `POST …/sso/domains/{id}/verify` (DNS TXT `_llmobs-verify.<domain>` = `llmobs-verify=<token>`) · `DELETE …/sso/domains/{id}`".

### Done

`just openapi` twice, `just check`.

---

## P9 — Enable and enforce (D-P-57, D-P-58)

### Revocation (`store.rs`)

```rust
/// Enforcement's revocation (D-P-58) for every user of `domains` who is not an instance
/// admin: deletes their web sessions not started through `org`'s provider and their password
/// resets, and revokes all their personal access tokens. Returns the number of users and the
/// revoked token hashes (to evict from the PAT cache after commit).
pub(crate) async fn revoke_for_domains(conn: &mut SqliteConnection, org: OrgId, domains: &[String], now: i64) -> Result<(u64, Vec<String>), MetaError>
```

For each domain: `SELECT id FROM users WHERE is_instance_admin = 0 AND substr(email, instr(email, '@') + 1) = ?`; for each user: `DELETE FROM web_sessions WHERE user_id = ? AND (sso_org_id IS NULL OR sso_org_id != ?)`, `personal_tokens::revoke_user_tokens(conn, user, now)` (extend the hashes), `password_resets::delete_user_resets(conn, user)`.

### Route `PATCH /api/v1/orgs/{org}/sso` (`settings.rs`), session-only, `OrgPerm::ManageSso`

Body `PatchOrgSso { enabled: Option<bool>, enforced: Option<bool> }` (`deny_unknown_fields`, `api/org_sso.rs`); response `200` `OrgSso`. Handler `patch_org_sso` takes `Extension(principal): Extension<Principal>` (a `Principal::Session` on this route; anything else is a bug → `ApiError::Forbidden { code: Forbidden }`).

1. Read the provider; none → `Conflict { detail: "Set up the identity provider first." }`.
2. `new_enabled = body.enabled.unwrap_or(row.enabled)`, `new_enforced = body.enforced.unwrap_or(row.enforced)`.
3. Turning anything on (`!row.enabled && new_enabled` or `!row.enforced && new_enforced`) → `require(.., Feature::OrgSso)?`.
4. `new_enforced && !new_enabled` → `Conflict { detail: "Enable the identity provider before enforcing it." }` (this also refuses disabling an enforced provider).
5. Turning enforcement on: `let domains = state.org_sso.domains_of(org)`; empty → `Conflict { detail: "Verify a domain before enforcing single sign-on." }`; when the caller is not an instance admin and their email's domain is in `domains`, `web_sessions::session_sso_org(reader, &session_hash) != Some(org)` → `Conflict { detail: "Sign in through your identity provider before enforcing it." }`.
6. `meta.write`: `store::set_flags`; when enforcement turns on, `revoke_for_domains(conn, org, &domains, now)`.
7. After commit: `state.pat_cache.evict(&hash)` for each revoked hash; `refresh`; audit `tracing::info!(target: "audit", org_id, user_id, enabled, enforced, users = revoked_users, "org sso settings changed")`.

Also in P8's verify: when the provider is `enforced` at the time of `mark_verified`, call `revoke_for_domains(conn, org, &[row.domain], now)` in the same transaction and evict the hashes after commit.

OpenAPI: `patch_org_sso` (`400, 401, 403, 404, 409, 429`, `QUERY_OR_BODY`); register `PatchOrgSso`.

### Tests (`settings::tests`)

Accounts: the owner `owner@acme.test` (not instance admin), a member `ada@acme.test` with a session and a PAT, `bob@other.test` with a session, an instance admin `root@acme.test` with a session. Use `configure_org_sso(app, org, idp, "acme.test", true, false)`.
1. `enforcing_revokes_domain_users`: owner's session started with `start_session(.., Some(org))` (as if through the IdP) → PATCH `{enforced: true}` → 200; Ada's session → 401 on `/api/auth/me`, her PAT → 401 (cache evicted: call it once before the PATCH so it is cached), her password reset row gone; Bob's session works; the admin's session works; an `sk` key of the project still ingests (`POST /v1/traces` 200 as in `routes/otlp.rs` tests); the owner's own session (sso_org_id = org) still works.
2. `enforcing_needs_the_callers_sso_session`: the owner's password session (`sso_org_id` NULL) → 409; the org has no verified domain → 409; provider disabled → 409.
3. `disable_is_refused_while_enforced` (`{enabled: false}` → 409); `{enforced: false}` then `{enabled: false}` → 200.
4. `lapse_allows_only_turning_off` (D-P-61): a provider enabled and enforced in `meta.db`, no licence → `{enforced: false}` → 200, `{enforced: true}` → 403 `plan_required`, `{enabled: true}` from disabled → 403.
5. `verifying_under_enforcement_revokes`: enforced on `acme.test`; add and verify `acme.io` → a user `eve@acme.io`'s session and PAT are revoked.

### Spec

`docs/spec/09-api.md` row gains "`PATCH /api/v1/orgs/{org}/sso` (`{enabled?, enforced?}`; enforcing revokes the domains' password sessions and personal access tokens, D-P-58)". `docs/spec/08-auth.md`: a new section **Per-org SSO (track P M5, `ee`)** after § OAuth / OIDC, holding D-P-52, D-P-54 to D-P-58, D-P-60 and D-P-61 in prose (write it now with what exists; P10–P12 complete it). In § Sign-up policy, the `LLMOBS_AUTH_ENFORCE_SSO_DOMAINS` row keeps its text and gains "An org can enforce SSO for its own verified domains (§ Per-org SSO)."

### Done

`just openapi` twice, `just check`.

---

## P10 — Sign in through an org's provider (D-P-55, D-P-56, D-P-65)

### Shared pieces (core)

- `auth/oidc/state_cookie.rs`: `OAuthState` gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub(crate) org_id: Option<Uuid>` ("The org of an org-provider login (D-P-65)"); `Debug` prints it. `flow::start` sets `None`.
- `auth/oidc/flow.rs`: `LoggedIn` gains `pub(crate) sso_org: Option<OrgId>` (`None` in `finish`). `LoginError` gains `SeatLimit` (code `seat_limit`); P12 adds `SsoRequired`. `login_exchange` and `internal` become `pub(crate)`.
- `routes/oauth.rs`: `redirect` becomes `pub(crate)`. `callback` dispatches: `if provider == crate::ee::org_sso::login::PROVIDER { login::finish(&state, &headers, query.state.as_deref(), query.code.as_deref(), query.error.is_some()).await } else { flow::finish(...).await }`; `start_session(.., logged_in.sso_org)`; the audit `login` event also gets `org_id` when `sso_org` is `Some` (two `tracing::info!` calls, as `setup` does for its optional key id).
- `meta/invites.rs`:

```rust
/// The org's pending, unexpired invitation for `email` (D-P-56).
pub(crate) async fn pending_invite_for_tx(conn: &mut SqliteConnection, org_id: OrgId, email: &str, now: i64) -> Result<Option<InviteRow>, MetaError>
```

  (`… WHERE org_id = ? AND email = ? AND accepted_at IS NULL AND expires_at > ?`, mapped as `pending_invites` maps rows.)

### Limiter

`Limits` gains `pub(crate) sso_discover: KeyedLimiter<IpAddr>`, 30 per minute per client IP (doc: "Org SSO discovery and authorize, D-P-60"). P11's discover uses it too.

### `crates/server/src/ee/org_sso/login.rs` (new, `ee`)

```rust
//! Sign-in through an org's provider (D-P-55, D-P-56, D-P-65) and discovery (P11).

/// The callback's provider segment and the state cookie's provider.
pub(crate) const PROVIDER: &str = "sso";

pub(crate) fn routes() -> Vec<RouteDef>   // P10: authorize; P11 adds discover

/// `GET /api/auth/sso/authorize?domain=&redirect=`.
async fn authorize(State(state): State<AppState>, ClientIp(ip): ClientIp, error::Query(query): error::Query<SsoAuthorizeQuery>) -> Result<Response, ApiError>

/// The callback's work; the route starts the session.
pub(crate) async fn finish(state: &AppState, headers: &HeaderMap, query_state: Option<&str>, code: Option<&str>, provider_error: bool) -> Result<LoggedIn, LoginError>
```

`SsoAuthorizeQuery { domain: Option<String>, redirect: Option<String> }` (private, `Deserialize`).

**authorize:**
1. `state.limits.sso_discover.check(&ip, now)` → `ApiError::RateLimited`.
2. `query.domain.as_deref().and_then(normalize_domain)` → `route_domain` (enabled **or not**: "Test sign-in", D-P-57) → `provider(route.org_id)`; any `None` → `redirect("/login?error=sso_unavailable", [])`.
3. As `flow::start`: `redirect_uri = public_url.join("/api/auth/callback/sso")`, new `csrf` and `nonce` (`new_opaque_token`), `provider.oidc.authorization_url(..)` (`ExchangeError::Unavailable` → `redirect("/login?error=provider_unavailable", [])`), seal `OAuthState { provider: PROVIDER, org_id: Some(org uuid), setup_token: None, redirect: safe_redirect(query.redirect), .. }`, `redirect(&location, [cookie])`.

**finish:**
1. `state_cookie::open(&state.oauth_key, headers, PROVIDER, query_state.unwrap_or(""), now)` → `InvalidState`; `saved.org_id` `None` → `InvalidState`.
2. `provider_error` → `ProviderError`; no `code` → `ProviderError`.
3. `state.org_sso.provider(org)` → `None` → `ProviderUnavailable`.
4. `provider.oidc.exchange_code(&redirect_uri, code, &saved.pkce_verifier, &saved.nonce, now).await.map_err(flow::login_exchange)?`; empty subject → `ProviderError`.
5. `email = claims.email.as_deref().and_then(|e| normalize_email(e).ok())` → `None` → `EmailNotVerified`. `email_verified` is ignored (D-P-55).
6. `let domains = state.org_sso.domains_of(org)`; the email's domain not in `domains` → `DomainNotAllowed`.
7. `jit = entitlements::of(state, org).allows(Feature::OrgSso).then_some(provider.jit_role.org_role())`; `max_seats = state.entitlements.resolve(org, now).limits.max_seats`; `signup = state.config.auth.signup`.
8. One `meta.write` running `admit(conn, &Admit { org, identity: store::identity_provider(org), subject, email, name: display_name(claims.name), domains, jit, max_seats, signup, now })` (move owned values in). `display_name` from `flow.rs`: make it `pub(crate)`.
9. After commit: evict revoked PAT hashes; audit as below; return `LoggedIn { user_id, redirect, sso_org: Some(org) }`, where `redirect` is `saved.redirect`, or, when `no_seat`, `/sso/no-seat?org=<org slug>` (the slug from `meta::orgs::org_by_id`, URL-encoded; the saved redirect is dropped). Audit `no_seat` as `tracing::info!(target: "audit", %user_id, %org_id, "org sso join skipped: seat limit")`.

```rust
enum Admitted {
    /// `no_seat`: JIT was refused for lack of a seat, and the person keeps their other orgs (D-P-56).
    User { user_id: Uuid, revoked: Vec<String>, linked: bool, joined: Option<OrgRole>, no_seat: bool },
    AccountDisabled,
    LinkRefused,
    SignupClosed,
    SeatLimit,
    DomainNotAllowed,
}
```

**`admit`** — all reads first, then writes, so a refusal writes nothing:
1. `identities::identity_user_tx(conn, &identity, &subject)` → `Some(id)`: load `users::user_by_id_tx`; its email's domain not in `domains` → `DomainNotAllowed`; disabled → `AccountDisabled`. Case **Known(user)**.
2. Else `users::user_by_email_tx(conn, &email)` → `Some(user)`: disabled → `AccountDisabled`; instance admin → `LinkRefused`. Case **Link(user)**.
3. Else signup `Disabled` → `SignupClosed`. Case **New**.
4. Membership (Known and Link: `members::org_role_tx(conn, org, user.id)` is `None`; New: always): when `jit` is `Some(jit_role)`: `invite = invites::pending_invite_for_tx(conn, org, &email, now)`; `role = invite.map_or(jit_role, |i| i.role)`; when `role != Viewer` and `max_seats` is `Some(limit)`: `seats = seat_members_tx(conn, org) + if invite.is_some() { 0 } else { seat_invites_tx(conn, org, now, None) }`; `over_limit(Some(limit), seats, 1)` → over the limit (D-P-56): when `invites::has_membership_or_invite_elsewhere_tx(conn, user_id, &email, org, now)` is true (`user_id` is `None` for a new user, who can only have invitations), continue **without** a membership and with `no_seat: true` (a new user is still created); otherwise → `SeatLimit`, before any write.

```rust
/// Whether the person has a membership in an org other than `org` (`user_id` given) or a
/// pending, unexpired invitation of `email` to another org (D-P-56).
pub(crate) async fn has_membership_or_invite_elsewhere_tx(conn: &mut SqliteConnection, user_id: Option<Uuid>, email: &str, org: OrgId, now: i64) -> Result<bool, MetaError>
```

(in `meta/invites.rs`: `SELECT EXISTS (SELECT 1 FROM org_members WHERE user_id = ? AND org_id != ?) OR EXISTS (SELECT 1 FROM invites WHERE email = ? AND org_id != ? AND accepted_at IS NULL AND expires_at > ?)`, binding `user_id` as `Option<String>`, which matches no row when `None`). When `jit` is `None`: no membership.
5. Writes: New → `insert_user` (`password_hash: None`, `email_verified_at: Some(now)`, `is_instance_admin: false`) and `insert_identity(conn, user.id, &identity, &subject, &email, now)`; Link → `insert_identity`, and when `user.email_verified_at.is_none()`: `web_sessions::delete_user_sessions(conn, id, None)`, `revoked = personal_tokens::revoke_user_tokens(conn, id, now)`, `users::set_password_hash(conn, id, None, now)`, `users::set_email_verified(conn, id, now)`. Membership → `members::add_org_member(conn, org, id, role, now)` (`MetaError::Conflict` ignored, as `provision_org`), and `invites::mark_accepted` for the invitation. Then `store::set_last_login(conn, org, now)`.
6. A `MetaError::Conflict` from `insert_user` or `insert_identity` (a concurrent first login) → `LinkRefused`, as `flow::decide` does.

Outcomes map to `LoginError` (`SeatLimit` → `LoginError::SeatLimit`). Audit after commit: `linked` → `tracing::info!(target: "audit", %user_id, %org_id, "identity linked")`; `joined: Some(role)` → `tracing::info!(target: "audit", %user_id, %org_id, role = role_str, "org sso joined")`.

Wire `routes()` into `http/access.rs`. `GET /api/auth/sso/authorize` is `Access::Public`, `documented: true`.

### OpenAPI

- `get_sso_authorize` (tag `auth`, public, `302` with `redirect_to(…)`, errors `429`; query `domain` (required: false), `redirect`). Expected-errors branch `["429"]`.
- `get_oauth_callback`'s descriptions: provider `github`, `google`, `oidc:<name>`, or `sso` (an org's provider); codes gain `seat_limit` and `sso_unavailable`. Its `authorize` sibling's description is unchanged.

### Web contract

`web/src/lib/search/auth.ts` `OAUTH_ERROR_CODES` gains `seat_limit` and `sso_unavailable`; `web/src/components/auth/messages.ts` `OAUTH_MESSAGES` gains:
- `seat_limit`: "Your organization has no free seat for you. Ask an owner to free one or upgrade the plan."
- `sso_unavailable`: "Single sign-on is not set up for this email domain."

(`Record<OAuthErrorCode, string>` needs both, or `just check-web` fails.) Spec 08's list of codes gains both (with `sso_required` in P12).

### Test helpers

Move `Pending`, `begin_with` (generalized to take the full authorize path and query), `finish`, `grant`, `one_cookie`, `set_cookies`, `cookie_header`, `location` and `me` from `routes/oauth.rs`'s test module to `crates/server/src/test_support/oauth.rs` (`pub(crate)`, `#[cfg(test)]` through `test_support`), and make `routes/oauth.rs` tests use them. No behaviour change.

### Tests (`ee::org_sso::login::tests`)

`org_sso_app` + `configure_org_sso(.., "acme.test", true, false)` + `auth.signup = invite_only` unless stated.
1. `a_new_domain_user_joins_with_the_jit_role`: authorize `?domain=acme.test&redirect=/acme` → 302 to the mock, cookie path `/api/auth/callback`; the mock grant with `ada@acme.test` and **no** `email_verified` → callback `/api/auth/callback/sso?code&state` → 302 to `/acme`; `/api/auth/me` with the session works; `org_members` role `viewer`; `identities.provider` `org:<org>`; `session_sso_org` is the org; `last_login_at` set.
2. `a_pending_invitation_sets_the_role`: invite `ada@acme.test` as member → login → role `member`, invitation accepted.
3. Seat limit (D-P-56), with `jit_role member` and `set_limits({"max_seats": 1})` (the owner fills it):
   - `seat_limit_refuses_someone_with_no_other_org`: a new `ada@acme.test` → `/login?error=seat_limit`, no user row, no identity; an existing user with no membership anywhere → `seat_limit`, no identity added;
   - `seat_limit_keeps_other_orgs`: `bob@acme.test`, a member of another org → 302 to `/sso/no-seat?org=acme`, a session cookie is set, no `org_members` row in `acme`, his identity is linked; a new `cara@acme.test` with a pending invitation to another org → the same redirect, her user row exists, no membership in `acme`, the other invitation still pending;
   - `jit_role viewer` → login works and joins (viewers use no seat).
4. `another_domain_is_refused`: grant `eve@other.test` → `domain_not_allowed`, nothing written.
5. `linking`: a password user `bob@acme.test` with unverified email, a session and a PAT → login links, his old session and PAT are revoked, `password_hash` NULL, email verified; a verified user `carl@acme.test` with a session → linked, session kept; an instance admin `root@acme.test` → `link_refused`; a disabled user → `account_disabled`.
6. `a_known_identity_signs_in_again` without a second membership; after the org removes `acme.test`, the same identity → `domain_not_allowed`.
7. `signup_disabled_refuses_new_users` (`signup_closed`); `open` with `signup_domains = ["x.test"]` still admits `acme.test` users.
8. `lapse_signs_in_without_joining`: no licence (rows inserted directly) → a new user signs in, no membership.
9. `authorize_without_a_route`: unknown or pending domain → `/login?error=sso_unavailable`; a disabled provider still starts (Test sign-in); the 31st authorize in a minute from one IP → 429.
10. `state_is_bound_to_the_org_flow`: a cookie from `/api/auth/authorize/oidc:mock` used on `/api/auth/callback/sso` → `invalid_state`, and an `sso` cookie on `/api/auth/callback/oidc:mock` → `invalid_state`.
11. `logs_carry_no_email_or_domain`: `assert_logs_clean(&["ada@acme.test", "acme.test", idp.issuer()], …)` around a full login.

### Spec

`docs/spec/08-auth.md` § Per-org SSO: the login, linking and JIT rules (D-P-55, D-P-56 and D-P-65) and the callback URL; § OAuth / OIDC's error-code list gains `seat_limit`, `sso_unavailable`. `docs/spec/09-api.md` § Auth: `GET /sso/authorize` row ("`?domain=&redirect=`; starts an org provider's login, D-P-60") and the callback row "`{provider}` is also `sso`".

### Done

`just openapi` twice, `just check`.

---

## P11 — Discovery (D-P-60)

### API types (`api/org_sso.rs`)

```rust
/// `POST /api/auth/sso/discover`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SsoDiscoverRequest { pub email: String }   // manual Debug: omits the email

/// Whether the email's domain signs in through an org's provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SsoDiscovery { pub sso: bool }
```

`api/account.rs` `AuthConfig` gains `pub sso_discovery: bool` (doc: "The login page asks for the email first: some domain signs in through an org's provider (D-P-60)"); `routes/auth.rs::config` sets `state.org_sso.discovery_enabled()`. `fixtures/api/static/auth-config.json` gains `"sso_discovery": false`.

### Route (`login.rs`)

`POST /api/auth/sso/discover`, `Access::Public`, documented. Limiter `sso_discover` (per IP) → 429. `normalize_email(&body.email)`: error → `{"sso": false}`; else `state.org_sso.route(&email).is_some_and(|route| route.enabled)`. `200`. Nothing is logged.

OpenAPI: `post_sso_discover` (tag `auth`, public, errors `400, 429`, `QUERY_OR_BODY`), register both types; `get_auth_config`'s schema changes through `AuthConfig`.

### Tests (`login::tests`)

`discovery_depends_only_on_the_domain`: enabled provider + verified `acme.test` → `ada@acme.test` and `nobody@acme.test` (no account) both `true`; `x@other.test` false; `not-an-email` false; provider disabled → false; enforced → true; `GET /api/auth/config` `sso_discovery` true only while enabled; the 31st call in a minute → 429; `assert_logs_clean(&["acme.test", "ada@"], …)`.

### Spec

`docs/spec/09-api.md` § Auth: `POST /sso/discover` row ("`{email}` → `{sso}`; depends only on the email's domain; 30 per minute per IP with `/sso/authorize`"); the `GET /config` row mentions `sso_discovery`. `docs/spec/08-auth.md` § Per-org SSO: discovery and the email-first login.

### Done

`just openapi` and `just api-examples` twice, `just check`.

---

## P12 — Refuse other sign-ins under org enforcement (D-P-58)

All checks call `crate::ee::org_sso::enforces(&state, email)` and exempt instance admins.

### `crates/server/src/routes/auth.rs`

- `login`: after `let Some(needs_rehash) = accepted else { … };` and **before** the email-verification check: `if crate::ee::org_sso::enforces(&state, &user.email) && !user.is_instance_admin { return Err(ApiError::Forbidden { code: ProblemCode::SsoRequired }); }`. (Wrong passwords keep answering 401 and counting toward lockout.)
- `signup`, open branch: next to the instance-wide check, `|| crate::ee::org_sso::enforces(&state, &email)`; `pending_signup_invite`: the same. (A new account is never an instance admin.)
- `forgot_password`: the `if let Some(user) … && …` chain gains `&& !(crate::ee::org_sso::enforces(&state, &email) && !user.is_instance_admin)` (still 204).
- `reset_password`: after loading `user`, before the policy check: enforced and not admin → `Err(ApiError::Forbidden { code: ProblemCode::SsoRequired })` (the token is left unused).

OpenAPI: check `post_password_reset` lists `403`; add it (and to its expected-error branch in `tests/openapi.rs`) if missing.

### `crates/server/src/auth/oidc/flow.rs`

`LoginError::SsoRequired` (code `sso_required`). In `resolve`, before any write:
- known identity: after `active(..)`, load the user (`users::user_by_id`) → `enforces(state, &user.email) && !user.is_instance_admin` → `Err(SsoRequired)`;
- otherwise, right after `verified_email(claims)`: when `enforces(state, &email)`, look up `users::user_by_email(reader, &email)`; continue only when it is an instance admin, else `Err(SsoRequired)`.

This covers GitHub, Google and `[[auth.oidc]]`. `get_oauth_callback`'s description gains `sso_required`.

### Web contract

`OAUTH_ERROR_CODES` gains `sso_required`; message: "Your organization requires signing in through its identity provider. Enter your email to continue." `loginErrorMessage`'s `sso_required` text becomes "Your organization requires single sign-on. Enter your email again to continue." (both the instance-wide and org cases).

### Tests

`routes::auth::tests` (with `configure_org_sso(.., "acme.test", true, true)`):
1. `org_enforcement_refuses_password_login`: Ada (`ada@acme.test`, password) → right password 403 `sso_required`, wrong password 401; `bob@other.test` → 200; an instance admin `root@acme.test` → 200.
2. `org_enforcement_refuses_password_signup` (open) and invitation signup → 403 `sso_required`.
3. `org_enforcement_silences_forgot_and_refuses_reset`: forgot → 204 and no mail (`with_mailer`); an existing reset token → 403; the admin's forgot sends.
4. `lapse_keeps_enforcement`: rows enforced, no licence → still 403 (D-P-8).

`routes::oauth::tests::instance_providers_are_refused_under_org_enforcement`: `[[auth.oidc]]` mock login of `ada@acme.test` (new, then existing user) → `/login?error=sso_required`, nothing written (no identity row); the instance admin passes.

### Spec

`docs/spec/08-auth.md` § Per-org SSO: the enforcement paragraph (D-P-58, D-P-59 and D-P-61) and the instance-admin exemption with `/login?password=1`; § OAuth / OIDC codes gain `sso_required`; § Passwords "Uniform answers" gains "Under an org's enforcement, a correct password of a non-admin user of its domains answers 403 `sso_required`."

### Done

`just openapi` twice, `just check`.

---

## P13 — Break-glass (D-P-63)

### `crates/server/src/ee/org_sso/store.rs`

```rust
/// `enabled = 0, enforced = 0` for the org. `false` when it has no provider.
pub(crate) async fn disable(conn: &mut SqliteConnection, org: OrgId, now: i64) -> Result<bool, MetaError>
```

(`set_flags(conn, org, false, false, now)` is the same; call it rather than adding a function if it already returns whether a row changed.)

### `crates/server/src/admin.rs`

```rust
/// Turns an org's SSO off (D-P-63). `org` is a slug or an id, as `entitlements` takes it.
/// A running server applies it within 60 seconds.
///
/// # Errors
/// `AdminError` when the org is unknown or `meta.db` fails.
pub async fn org_sso_disable(config: &ServerConfig, org: &str) -> Result<bool, AdminError>
```

Open `meta.db` and resolve the org the way `entitlements` does; `meta.write` → `store::set_flags(.., false, false, now)`; `tracing::info!(target: "audit", org_id, actor = "cli", "org sso disabled")`.

### `crates/cli/src/main.rs`

`AdminCommand::OrgSso { #[command(subcommand)] command: OrgSsoCommand }` with `OrgSsoCommand::Disable { org: String, #[arg(long)] config: Option<PathBuf> }` (follow `Entitlements`/`Show`). Output: `org SSO disabled for <org>; a running server applies it within a minute` or `<org> has no identity provider` (exit 0). Tests: clap parsing (`llmobs admin org-sso disable acme`; missing org is an error) and `admin::org_sso_disable` on a temp data dir (a provider enabled and enforced → both off; unknown org → error).

### Spec

`docs/spec/09-api.md` § CLI: `llmobs admin org-sso disable <org>` ("turns an org's provider and enforcement off; applied by a running server within a minute; for a broken identity provider"). `docs/spec/08-auth.md` § Per-org SSO: one sentence on it.

### Done

`just check`.

---

## P14 — Email first on the login page (D-P-60)

### `web/src/lib/search/auth.ts`

`LoginSearch` gains `password?: boolean`; `parseLoginSearch` sets it when `search.password` is `"1"` or `1` (the instance admins' path, D-P-58). Tests in the existing search test file.

### `web/src/lib/redirect.ts`

```ts
/** `/api/auth/sso/authorize?domain=…&redirect=…` (D-P-60). */
export function ssoAuthorizeHref(domain: string, redirect?: string): string
```

Test next to `authorizeHref`'s.

### `web/src/routes/login.tsx`

When `config.sso_discovery && !search.password`, the page has two steps (state `step: "email" | "password"`, starting at `"email"` unless `search.email` is set and discovery already answered — keep it simple: always start at `"email"`, prefilled from `search.email`):
- **email step**: the `Email` field and a `Continue` button (`SubmitButton`), nothing else (no provider buttons, no password). Submit: empty → "Enter your email address."; else `$api.useMutation("post", "/api/auth/sso/discover")` with `{ email: email.trim() }`; `sso: true` → `navigateExternal(ssoAuthorizeHref(domainOf(email), search.redirect))` (`@/lib/external-navigation`) and keep the button pending; `sso: false` → step `"password"`. A 429 shows `RATE_LIMITED`; another error shows "Sign-in failed. Try again." and stays on the email step.
- **password step**: today's page, with the email filled and a `Use another email` link-button that returns to the email step.
When `sso_discovery` is false, or `search.password` is set, the page is exactly today's. `domainOf(email)` is the text after the last `@`, lowercased.

### `web/src/routes/sso/no-seat.tsx` (new, MIT)

The page an org sign-in lands on when JIT found no seat but the person keeps other orgs (D-P-56). `createFileRoute("/sso/no-seat")`, search `org` parsed with `parseString(search.org, 64)` (slug). It requires a session like the other signed-in pages (follow `routes/account.tsx`'s guard). Inside `AuthLayout` titled "No seat available": the text `No seat available in {org}; ask an owner.` (without `org`: "No seat available in this organization; ask an owner."), `role="status"`, and a `Continue` link to `/`. Test `web/src/routes/sso/no-seat.test.tsx`: `/sso/no-seat?org=acme` shows "No seat available in acme; ask an owner." and Continue goes to `/`; without `org`, the generic text.

### Tests (`login.test.tsx`)

Existing tests keep passing (the fixture has `sso_discovery: false`). New, each with `withConfig({ sso_discovery: true })` and `setExternalNavigator` (restore in `afterEach`):
1. an SSO domain: type `ada@acme.com`, Continue → the discover body is `{ email: "ada@acme.com" }`; the navigator received `/api/auth/sso/authorize?domain=acme.com&redirect=%2Facme` (from `?redirect=/acme`); no password field was shown.
2. another domain: discover answers `{ sso: false }` → the password field and the provider buttons appear, the email is kept; signing in works as before; `Use another email` goes back.
3. `?password=1` → today's page at once (no discover request).
4. discover 429 → `RATE_LIMITED`.
5. the error messages for `?error=seat_limit`, `sso_required`, `sso_unavailable` (`oauthErrorMessage`).

### Spec

`docs/spec/11-ui.md` § Settings, **Plans** bullet's neighbour: a sentence in the auth pages' description (or § Navigation if the login page is described there): "When `sso_discovery` is on, the login page asks for the email first and sends emails of an SSO domain to their org's identity provider; `/login?password=1` skips it (instance admins, D-P-58, D-P-60)."

### Done

`just check-web`, then `just check`.

---

## P15 — The org SSO settings page

### Data

`web/src/api/queries-settings.ts`: `orgSsoInit(org)` (`{ params: { path: { org } } }`) and `orgSsoQuery(org)` (`$api.queryOptions("get", "/api/v1/orgs/{org}/sso", …)`). After every write, `queryClient.setQueryData(orgSsoQuery(org).queryKey, response)` when the response is an `OrgSso`, else `invalidateQueries`.

### Navigation

- `web/src/components/settings/settings-nav.ts`: each `orgSettingsNav` item gains `ownerOnly: false`, and a new last item `{ to: "/$org/settings/sso", label: "Single sign-on", ownerOnly: true }`.
- `settings-sidebar.tsx`: read the role (`$api.useQuery("get", "/api/auth/me")`, `me.orgs.find((o) => o.slug === org)?.role`) and filter `!item.ownerOnly || role === "owner"`. A test in `settings-nav.test.ts` or the sidebar's test: an owner sees "Single sign-on", an admin does not.

### Route (`web/src/routes/$org/settings/sso.tsx`, MIT)

`createFileRoute("/$org/settings/sso")`; `SettingsPage` title "Single sign-on", description "Let people sign in with your organization's identity provider." Owners get `<OrgSsoSettings org={org} />` from `@/ee/org-sso/org-sso-settings`; others get "Only owners can manage single sign-on." (`role="note"`).

### `web/src/ee/org-sso/` (each file starts with the SPDX line)

- `org-sso-settings.tsx`: loads `orgSsoQuery`; when `useOrgEntitlements(org).locked("org_sso")`, shows `<LockedFeature feature="org_sso" upgradeUrl={…} />` at the top and passes `locked` to the sections. Renders `ProviderSection`, `DomainsSection`, `StatusSection` in that order.
- `provider-section.tsx`: form with `Display name`, `Issuer URL`, `Client ID`, `Client secret` (password input; when a provider exists, placeholder "Stored; leave blank to keep it"), `Role for new members` select (`Viewer` with help "Viewers do not use a seat.", `Member`), `Save` → `PUT …/sso/provider` (omit `client_secret` when blank). Server field errors under their fields (`fieldErrors`). The callback URL from the response with a copy button (`aria-label="Copy callback URL"`) and the text "Register this redirect URI at your identity provider." `Remove provider` (only when one exists) → `ConfirmDialog` → `DELETE`; its 409 detail is shown. When `locked`, the form is disabled and `Remove provider` stays enabled.
- `domains-section.tsx`: an `Add domain` input and button (`POST …/domains`, disabled when locked); a table (`aria-label="Domains"`): domain, status badge (`Verified` / `Pending`), for pending rows the TXT name and value with copy buttons and a `Verify` button (`POST …/verify`; disabled when locked; 409 and 503 details shown under the table, `role="alert"`), and `Remove` on every row (`ConfirmDialog`: "People with this domain's emails will no longer be sent to your identity provider.").
- `status-section.tsx`: two switches, `Enabled` ("Send people with these domains to the identity provider when they sign in.") and `Enforced` ("Require it: password sign-in and personal access tokens stop working for these domains. API keys keep working."), each `PATCH …/sso`. Turning `Enforced` on opens a `ConfirmDialog` naming the verified domains and the consequence; 409 details are shown (`role="alert"`). When `locked`, a switch can only be turned off. A `Test sign-in` link (`ssoAuthorizeHref(first verified domain, "/<org>/settings/sso")`) when a provider and a verified domain exist, with "Sign in through the provider once before you enforce it." Hidden without a provider.

### Tests (`web/src/routes/$org/settings/sso.test.tsx`)

Rendering `/acme/settings/sso` with the fixture (`org-sso.json`) and `server.use` overrides:
1. owner: provider fields filled, the secret empty with the placeholder, the callback URL, two domain rows (verified and pending with the TXT name and value), switches reflect `enabled: true`, `enforced: false`.
2. saving sends the body without `client_secret` when blank, and with it when typed.
3. a 400 `issuer` field error shows under Issuer URL.
4. add domain posts `{ domain }`; verify shows the 409 detail; remove asks for confirmation then deletes.
5. enforcing opens the confirmation listing `acme.com`, then PATCHes `{ enforced: true }`; a 409 detail is shown.
6. locked (`org-entitlements.json` override without `org_sso`): the lock note and Upgrade button, Save and Add disabled, Remove still enabled, the enforced switch can be turned off but not on.
7. an admin sees "Only owners can manage single sign-on." and the nav has no "Single sign-on" entry.

### Spec

`docs/spec/11-ui.md` § Settings, **Org** bullet: "Single sign-on (owners, `web/src/ee`, track P's M5): the identity provider (issuer, client id, write-only secret, role for new members, callback URL to register), domains with their DNS TXT record and Verify, the Enabled and Enforced switches with Test sign-in; locked without `org_sso`, where only removals and turning off remain (D-P-61)."

### Done

`just check-web`, `just check-ee`, then `just check`.

---

## P16 — Close out

- Remove every `allow(dead_code)` attribute M5 added (P3 `store.rs`, P5 `dns.rs`, P6 `org_sso/mod.rs`, any item-level one); `just lint` clean.
- Check that specs 04 (P3), 08 (P9–P13), 09 (P7–P13), 11 (P14, P15) and 12 (P2) say what the code does.
- `CHANGELOG.md` § `[Unreleased]` `### Added`: "Per-org single sign-on (enterprise): an org's own OIDC provider, DNS-verified email domains, email-first login, just-in-time membership and SSO enforcement; `llmobs admin org-sso disable`."
- `docs/roadmap.md` row **P** Status: add "M5 implemented: per-org OIDC providers, DNS-verified domains, email-first login discovery, JIT membership within seats, per-org SSO enforcement, the org SSO settings page and the break-glass command." (keep the M4 text as the coordinator left it).
- `just check`, `just e2e`.

## Coordination

- **Track I (`crates/eval`):** P2 adds `EgressPolicy::allows_url` and `identity_http_client` to `egress.rs` and re-exports the latter; `EgressClient::post` uses `allows_url`. No behaviour change for judges and webhooks.
- **Track M (org drop):** nothing to call: a dropping org (`state != 'active'`) leaves the routing index at the next 60 s reload, and its rows cascade on delete.
- **Instance-wide enforcement (`auth.enforce_sso_domains`):** unchanged by M5; it still does not refuse password reset. Aligning it with D-P-58 is a later, separate change (not M5).

## Report

Per sub-phase: what was done, deviations, the migration number used, the `hickory-resolver` API differences from this plan (if any), whether `cargo deny` needed attention, anything unverified (the real DNS path is never exercised by tests; say so).
