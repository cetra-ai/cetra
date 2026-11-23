# 08 — Authentication, authorization, email

## Bootstrapping

- On first start with an empty `users` table, the UI shows a **setup page** at `/setup?token=…`. While no user exists, `cetra serve` generates a random setup token at startup and logs that URL once at `warn` (D144). This is the only secret that is ever logged. `POST /api/auth/setup` requires the token. The first user signs up with email/password (or via a configured OIDC provider), becomes `is_instance_admin`, and creates the first org + project (owner role). An API key is shown at the end with copy-paste OTel/SDK snippets.
- Headless alternative: `CETRA_INIT_ADMIN_EMAIL`, `CETRA_INIT_ADMIN_PASSWORD`, `CETRA_INIT_ORG`, `CETRA_INIT_PROJECT`, `CETRA_INIT_SECRET_KEY` (optional: a pre-chosen `sk` value, for IaC). It runs once, in one transaction, only when `users` is empty, and does not need the setup token. When `CETRA_INIT_SECRET_KEY` is set, it must be `sk-cetra-` plus 32 base62 characters, or startup fails. `CETRA_INIT_READ_KEY` (optional) is a second pre-chosen `sk` value of the same project, created in the same transaction with scope `read` only and the name `init-read`, so an operator can read the project at once, also in restore mode (D-N-38). It has the same format check, must differ from `CETRA_INIT_SECRET_KEY`, and either failure stops startup with an error that names the variable, never the value. On a database that already has a user, every `CETRA_INIT_*` value is ignored, these keys included, and not checked.

## Sign-up policy

| Setting | Default | Meaning |
|---|---|---|
| `CETRA_AUTH_SIGNUP` | `invite_only` | `open` \| `invite_only` \| `disabled`. After bootstrap, `invite_only` lets only invited emails register. |
| `CETRA_AUTH_SIGNUP_DOMAINS` | empty | With `open`, restrict to these email domains. |
| `CETRA_AUTH_DISABLE_PASSWORD` | `false` | Hide and refuse password login/sign-up (SSO only). Instance admins keep password login, password change and reset through a link, at `/login?password=1` (D-S-3, D-S-4); "forgot password", password sign-up, invite sign-up and setup stay refused for everyone. The CLI admin commands still work. |
| `CETRA_AUTH_REQUIRE_EMAIL_VERIFICATION` | `false` | Requires SMTP. |
| `CETRA_AUTH_ENFORCE_SSO_DOMAINS` | empty | Users with these email domains must log in through an OAuth/OIDC provider (password login refused). Any configured provider satisfies it. Enforced only with a genuine licence that lists `sso_enforcement`, expired or not (D-P-8); otherwise ignored with a startup warning. An org can enforce SSO for its own verified domains (§ Per-org SSO). Besides login and signup, enforcement ends the listed domains' password sessions (a session that no identity provider started stops resolving, with no admin exemption, as for login), refuses their personal access tokens, and answers password forgot and reset as for an enforced org (D-P-69). |

The default is `invite_only` (stricter than Langfuse's open default) because a self-hosted instance reachable on the internet should not accept strangers.

An invite token admits its own email only (D147). It does not prove the mailbox: the inviter can read the token, so an account made or verified with one is recorded as verified `via` `invite` (D-P-66). Signup under `open` with an email already registered answers 409 `conflict` (D145).

## Passwords

- **Hashing:** argon2id, m = 19 456 KiB, t = 2, p = 1, stored as a PHC string. Hashing runs in `spawn_blocking` behind a semaphore of `max(2, cores / 2)` permits (D143). Parameters can be raised; hashes are upgraded on the next successful login.
- **Policy:** NFC-normalized, then 10 to 128 characters counted in Unicode scalar values, not equal to the email, and not in the embedded common-password list (SecLists `10k-most-common.txt`, MIT; entries of 10 characters or more are kept).
- **Unknown email:** login verifies the password against a fixed dummy hash, so its timing matches a real user (D143).
- **Rate limiting and lockout** are in memory; a restart clears them (D143). Login: 10 per minute per (client IP, email). Signup, setup, password forgot and verification resend: 5 per minute per client IP; password forgot and resend also 3 per hour per email. After 20 consecutive failed password logins for one email, password login for that email is locked for 15 minutes. A success or a password reset clears the lockout. Lockout does not block OAuth/OIDC login or password reset. Device codes (`POST /api/auth/device/code`): 10 per minute per client IP, an IPv6 address counted by its /64 (D-AO-3).
- **Uniform answers (D145):** login answers the same 401 `unauthorized` for an unknown email, a wrong password, a disabled user, a user without a password and a locked email. Password forgot and verification resend always answer 204. Under an org's enforcement, a correct password of a non-admin user of its domains answers 403 `sso_required`; a wrong password still answers 401. With password login off, a correct password of a user who is not an instance admin answers 403 `forbidden`; every other failure answers the same 401 (D-S-3).
- **Change:** `POST /api/v1/me/password` takes the current password and the new one, and revokes every other web session and all personal access tokens.
- **Reset with SMTP:** an email with a single-use token valid for 1 h (`password_resets`, stored hashed). Resetting revokes all of the user's web sessions and personal access tokens.
- **Reset without SMTP:**
  - `cetra admin reset-password --email x` prints a reset URL.
  - Instance admins can also generate a reset link in the UI (admin → users) and send it to the user themselves.

## Web sessions

- **Cookie:** `cetra_session`, an opaque random token of 32 bytes. The DB stores its SHA-256 hex (`web_sessions`).
- **Cookie flags:** `HttpOnly`, `SameSite=Lax`, `Secure` when the public URL is https.
- **Lifetime:** 30 days, sliding, refreshed at most once per hour. Logout deletes the row.
- **Bearer wins (D140):** a bearer token wins over the cookie. When a request has both, the cookie is ignored.
- **CSRF (D141):** every state-changing request (not GET, HEAD, OPTIONS) authenticated by the session cookie, and every state-changing request under `/api/auth/` whatever its credentials (login, signup, setup, password reset, invite accept, logout), except the device flow's `POST /api/auth/device/code` and `POST /api/auth/device/token`, which carry no cookie (D-AO-3), must carry an `Origin` equal to the origin of `public_url`. Requests authenticated by a bearer token skip the check. JSON bodies require `Content-Type: application/json`.
- Disabling a user deletes their web sessions. Their personal access tokens fail at once, because every request checks `disabled_at` (D149).

## OAuth / OIDC

Configured in `cetra.toml` or via env.

```toml
[auth.github]            # OAuth2 (not OIDC); uses the primary *verified* email from /user/emails
client_id = "…"
client_secret = "…"
link_existing_by_email = true    # default true

[auth.google]            # OIDC, issuer https://accounts.google.com
client_id = "…"
client_secret = "…"
link_existing_by_email = true    # default true
allowed_domains = ["acme.com"]   # verified email domain; Google also checks the `hd` claim

[[auth.oidc]]            # any number of generic OIDC providers (Okta, Entra ID, Keycloak, Auth0, Authentik, …)
name = "okta"            # provider id → identities.provider = "oidc:okta"
display_name = "Okta"
issuer = "https://acme.okta.com"
client_id = "…"
client_secret = "…"      # or CETRA_AUTH__OIDC__OKTA__CLIENT_SECRET (spec 12, D-S-2)
scopes = ["openid", "email", "profile"]
link_existing_by_email = false   # default false
allowed_domains = []             # verified email domain
auto_provision = { org = "acme", role = "member" }   # optional: auto-join org on first login
```

- Flow: authorization code + PKCE (S256). `state`, the PKCE verifier and `nonce` travel in a cookie signed with a per-process random key, lasting 10 minutes, `SameSite=Lax`, `Path=/api/auth/callback`. Callback URL: `<public_url>/api/auth/callback/<provider>`. The callback ignores any existing session. It redirects to the relative path given to `GET /authorize/{provider}?redirect=` (the path must start with `/` and not `//`); errors redirect to `/login?error=<code>`. The code is one of `invalid_state`, `provider_error`, `provider_unavailable`, `email_not_verified`, `domain_not_allowed`, `signup_closed`, `account_disabled`, `link_refused`, `setup_required`, `seat_limit`, `sso_unavailable`, `sso_required`.
- Account linking (D148): a provider login with a known `(provider, subject)` signs that user in. Otherwise, when a user with the same email exists, the identity is linked only if the provider's `link_existing_by_email` is on, the provider asserts the email verified (a missing claim is false), and the user is not an instance admin. Linking revokes the user's web sessions and personal access tokens, and clears the password unless the email is proven: verified by a mailed token, an identity provider or the control plane. An email that was never verified, or was verified only by an invitation or by an operator (`users.email_verified_via` = `invite`, `operator`), loses its password, and the link records `idp` (D148, D-P-66). Otherwise a new user is created, subject to the sign-up policy — an IdP login counts as invited if `auto_provision` is set for that provider.
- `allowed_domains` checks the verified email's domain, and for Google also the `hd` claim.
- Multi-tenant Entra issuers (`common`, `organizations`, `consumers`) are refused at config load. Provider metadata is discovered lazily and cached for one hour.
- `id_token` is validated (`openidconnect` crate: signature via JWKS with caching, `iss`, `aud`, `exp`, `nonce`).
- SAML: not in MVP.

## Per-org SSO (track P M5, `ee`)

An org owner registers one OIDC provider for the org, and the org proves its email domains with DNS TXT records (D-P-51, D-P-52, D-P-54). The client secret is sealed in `meta.db` like an LLM connection key. The provider's discovery document is loaded at save time, and every provider request goes through the SSRF guard (D-P-53).

- **Domains (D-P-54).** Exact match on the email's domain. At most 20 per org. A domain is verified by at most one org at a time: the proof is a TXT record `_cetra-verify.<domain>` with the value `cetra-verify=<token>`, looked up with the cache off. A domain the server configuration owns (`auth.enforce_sso_domains`, Google's or an `[[auth.oidc]]`'s `allowed_domains`) cannot be added. A verified domain moves to another org when the holder's record is gone from the answer. Each verified domain is looked up again once a day: an answer without the record makes it unverified (it stops routing, enforcing and linking), and a failed lookup changes nothing (D-P-72).
- **Sign-in (D-P-55, D-P-56).** The org's provider signs in only emails of the org's verified domains. It links an existing account (a verified one without revocation; an unverified one loses its web sessions, tokens and password). A user who is not a member joins with the org's role for a pending invitation, or the org's `jit_role` (viewer or member), within `max_seats`.
- **Enabled, then enforced (D-P-57, D-P-58).** A new provider is disabled. Enforcing needs the provider enabled, a successful sign-in through it, a verified domain, and, when the caller's own domain is covered, a session started through this org's provider. Enforcing revokes, at once, the password sessions (those not started through the org's provider), the personal access tokens and the password resets of the covered users, except instance admins. Session and token resolution check it again on every request, so a session or token made while the revocation was committing does not work. Project API keys are not affected.
- **Refusals under enforcement (D-P-58).** While an org enforces its domains, every other sign-in path of a non-admin user of those domains is refused with 403 `sso_required`: password login (after the password verifies), password signup and invitation signup, password reset (the reset token is left unused), and the GitHub, Google and `[[auth.oidc]]` callbacks (`/login?error=sso_required`, nothing written). Password forgot still answers 204 and sends no mail. Instance admins are exempt: they sign in with a password at `/login?password=1` (break-glass). The refusal follows the domain's route, so after another org takes a domain (D-P-54) it applies to the new holder's enforcement only.
- **Lapses (D-P-61, D-P-8).** Enforcement keeps applying when the licence lapses; only setting it up or turning it on is refused.
- **Lapses (D-P-8, D-P-61).** Without `org_sso`, sign-in and enforcement keep applying, and only setting up or changing the provider is refused. Turning enforcement off and disabling a provider that is not enforced still work.
- **Email first (D-P-60).** `GET /api/auth/config` reports `sso_discovery` while some verified domain routes to an enabled org provider. The login page then asks for the email and posts it to `POST /api/auth/sso/discover`, which answers `{"sso": bool}` from the domain alone (an invalid email is `false`). A `true` answer sends the browser to `/api/auth/sso/authorize?domain=`; a `false` answer shows the password form and the instance providers. Discovery and authorize share one per-IP bucket of 300 requests a minute with a burst of 300; nothing logs an email or a domain.
- **Break-glass (D-P-63).** `cetra admin org-sso disable <org>` turns an org's provider and enforcement off when the provider is broken. It edits `meta.db` directly, so it works offline, and a running server applies it within 60 seconds.
- **Sign-in (D-P-55, D-P-56, D-P-65).** The login starts at `GET /api/auth/sso/authorize?domain=` (callback `/api/auth/callback/sso`). The discovered authorization endpoint must be `https` (or `http` on a loopback host when private networks are allowed) and pass the egress policy, else `sso_unavailable`. The state cookie carries the org, so an instance provider's cookie and an org's are not interchangeable. A known identity signs in again. An existing account with the email is linked only when it is not an instance admin, and a known identity of a user who was promoted to instance admin is refused too (`link_refused`); an unverified account loses its password, sessions and tokens at the link, and a verified one keeps them. A new user is created verified without a password. Under `signup = disabled` a new user is `signup_closed`; under `invite_only` or `open` the org's verified domain is the invitation, and `signup_domains` do not apply. A user who is not a member joins with the role of a pending invitation or the org's `jit_role`, within `max_seats`. At the seat limit, a person who belongs to another org (or has an invitation to one) signs in without joining and lands on `/sso/no-seat?org=<slug>`; anyone else gets `seat_limit`. A member an owner removed joins again at their next sign-in through the provider: block them at the IdP. An absent `email_verified` claim is trusted; an explicit `false` is refused.
- **Test sign-in (D-P-57).** While a provider is disabled, only an owner of the org, signed in with a session, can start a login through it (the state cookie is marked `test` and records that owner). That login signs in that same owner and no one else (any other resulting user is `sso_unavailable`, with nothing written); it records their identity; it creates no user and adds no member. An instance admin who owns the org signs in without an identity being written, so the operator can reach enforcement.

## SCIM (track EA M5, `ee`)

An org's identity provider provisions its members over SCIM 2.0 at `<public_url>/scim/v2` (D-EA-12 to D-EA-19, D-EA-24 to D-EA-28). The Users resource and the discovery documents are in `crates/server/src/ee/scim/`. The Groups resource is in `groups.rs`, the mapping routes in `mappings.rs`, and the role recompute in `recompute.rs`.

- **Tokens (D-EA-15).** `scim-cetra-…` bearer tokens, stored as SHA-256 (`auth/secret.rs`, `KeyKind::Scim`), at most two live per org (`MAX_LIVE_TOKENS`, a `409` when the org has two). Created, listed and revoked with `ManageProvisioning` (owner, session only). A token resolves to `Principal::Scim { org, token_id }` only if its org is active; every other access kind refuses that principal, and `Access::Scim` routes accept no other principal. The per-token limit is 600 requests a minute, burst 600 (`auth/limits.rs`), keyed by token id.
- **Domains (D-EA-13, P-1).** SCIM provisions an email only when its domain is a verified domain of the org: the org's `org_sso_domains` rows with `verified_at` set, minus domains the instance configuration claims (D-P-54). On a cell with `entitlements.control_plane`, the org token's `sso_domains` are added (D-EA-17). The in-memory `OrgSso` index is not used. Enabling SCIM needs at least one verified domain, and the `scim` feature.
- **Linking and creating.** An existing account with the email is linked as in org SSO (D-P-66): an unproven account loses its password, sessions and tokens; an instance admin is never touched beyond the membership (P-6). A new account is verified with `email_verified_via = 'idp'` and has no password. Either way the membership gets the role its groups give, or `org_scim.default_role` when no group maps it. A member who is already in the org is recomputed at once (D-EA-24, M6 P-2). The seat limit applies, as `403 plan_limit`.
- **Create as inactive.** A create with `active: false` creates the SCIM user inactive. When the person is already a member, the membership is removed as a deactivation removes it.
- **Deactivation (D-EA-16, P-7).** `active: false`, `PUT` or `PATCH` with it, and `DELETE` remove the membership and the project overrides in the org, revoke the user's web sessions and personal access tokens in every org (`meta::users::revoke_access_tx`), and disable the account when no membership is left. The SCIM row stays with `active = 0` for a deactivation and is deleted by `DELETE`. Deactivating or deleting the org's last active owner is refused with `409` ("an org needs an owner"); other owners may be removed. An instance admin only loses the membership.
- **Reactivation.** `active: true` re-adds the membership with the role its groups give, or the default role, and then recomputes its overrides (`recompute_tx`). It enables the account when `disabled_account` is set, which is when SCIM disabled it, even if the SCIM row was deleted since. An account disabled by an instance admin is refused with `409`, and nothing changes.
- **Locks (D-EA-18).** While SCIM is enabled and entitled (`scim::locks`), the org's JIT join is off, invitations to verified-domain emails are refused (creation and acceptance, `409 scim_managed`), and a managed member's role, project overrides and removal are refused with `409 scim_managed` on `update_org_member`, `remove_org_member` (self-leave included) and the project override routes. A member is managed while an active `scim_users` row links to them. Changes by the control plane's member API are not locked. Disabling SCIM or a lapse lifts the locks; roles stay.
- **Lapses (D-EA-19).** Without the `scim` feature, deactivations, deletes, removals and group member removals still run, and creates, reactivations, group creates, member additions and group renames are refused with `403 plan_required` as SCIM errors. Recomputes apply only lowerings (see § Recompute above).
- **Errors.** Every `/scim/v2` response is a SCIM error or resource with `application/scim+json` (P-2): a middleware rewrites problem responses under `/scim/v2` to the same status.
- **Groups (D-EA-12, M6).** `/scim/v2/Groups` with `displayName` and `externalId` (unique per org, `409 uniqueness`) and `members` (SCIM user ids of the org). `eq` filters only, on `displayName` and `externalId`. A `replace` or `PUT` whose members match no org user, and a `remove` without a member, are `400 invalidValue`; an empty list is the only way to empty a group. Creating a group, adding members and renaming need `scim` (`403 plan_required` otherwise); removals and deletes do not.
- **Roles from groups (D-EA-14).** An org's owner maps a group to an org role (`admin`, `member`, `viewer`) and to project roles (`admin`, `member`, `viewer`, `none`). A member's org role is the highest of their groups' org roles (`none` < `viewer` < `member` < `admin`), else `org_scim.default_role`. For each mapped project, the project role is the highest of the groups' roles, and a project no group maps has no override. Owners are never changed or mapped (`owner` is a 400). Admins get no overrides (D139). `auth::roles::org_rank` orders the org roles.
- **Recompute (D-EA-24, D-EA-25, M6 P-2 to P-8).** `ee::scim::recompute::recompute_tx` is the only code that sets a managed member's org role or overrides from SCIM. It runs in the transaction of every change that can affect a member: group membership, group deletion, mapping change, default-role change, linking and reactivation. Mapping and default-role changes recompute only while `org_scim.enabled` is true after the write. Users are processed by `scim_users.created_at, id`, with the mappings and the user-to-groups map loaded once per request.
- **Raises and lowerings (D-EA-26).** A change is judged by the effective project role before and after (`auth::roles::effective_project_role`, `none` meaning no access). Removing a `none` override is a raise; adding one is a lowering. With `scim` off, a recompute applies lowerings and skips raises without a record. With `project_roles` off, it writes no new override.
- **Seats.** A raise from `viewer` is checked against `max_seats` as a create is. A refused member keeps their role, and the request still succeeds. Each request writes one `scim.role_raise_refused` record with the count and at most 20 SCIM user ids. A `member` raised to `admin` needs no seat.
- **Audit (M6 P-4).** One request writes at most one group record (`scim.group_created`, `scim.group_updated`, `scim.group_deleted`, `scim.group_members_changed`, `scim.group_mapping_changed`), one `scim.roles_recomputed { users, changed }` when roles changed, and one `scim.role_raise_refused`. Group names are never recorded, only ids and counts.

## Authorization (RBAC)

Effective role on a project = `project_members.role` if present, else the org role (`owner` behaves as `admin` at project level). `none` in `project_members` hides the project from that user.

Setting an override needs the `project_roles` entitlement (Teams add-on or an enterprise licence; `403 plan_required` otherwise); removing one never does, and stored overrides keep applying whatever the plan (D178, D-P-8). Overrides apply only to org `member`s and `viewer`s (D139). They can raise the project role to `admin` or lower it to `viewer` or `none`. Org owners and admins are never overridden; setting an override for one is a 400. Only a caller with effective role `admin` on the project edits overrides. Nobody grants a role above their own, and only owners grant or change `owner`. An org always keeps at least one owner. Any signed-in user may create an org and becomes its owner. Instance admins get the `/api/v1/admin/*` endpoints only, with no implicit access to org data.

| Action | viewer | member | admin | owner |
|---|:-:|:-:|:-:|:-:|
| View traces, sessions, dashboards, datasets, prompts, experiments, SQL console | ✓ | ✓ | ✓ | ✓ |
| Annotate (human scores), add to dataset, edit datasets, run SQL exports (an `sk` key with `read` may export, D-H-5), share saved views (D-H-6) | | ✓ | ✓ | ✓ |
| Manage evaluators, online rules, alert rules, channels, score definitions, dashboards (D-C-16) | | ✓ | ✓ | ✓ |
| Run the playground: model runs through an LLM connection (D-V-26) | | ✓ | ✓ | ✓ |
| Create prompt versions, move unprotected prompt labels (D-V-13) | | ✓ | ✓ | ✓ |
| Move protected prompt labels, edit the protected list (D-V-10) | | | ✓ | ✓ |
| Create and edit annotation queues, add and remove items (D-AQ-11) | | ✓ | ✓ | ✓ |
| Use the in-app assistant: chat and manage one's own conversations, with a session only (D-AS-4) | | ✓ | ✓ | ✓ |
| Manage API keys, LLM connections, the assistant setting (D-AS-3), project settings (retention), GDPR deletions | | | ✓ | ✓ |
| Create/delete projects, manage members & invites, org limits | | | ✓ | ✓ |
| Delete org, transfer ownership, change owners | | | | ✓ |
| Manage SCIM provisioning: enable, default role, tokens (session only, `ManageProvisioning`) | | | | ✓ |
| Read and export the org's audit log (`ViewAudit`, session or PAT, never an API key; EA D-EA-10) | | | ✓ | ✓ |
| Manage the org's audit stream: the destination, its secret, the test (`ManageAuditStream`, session only; EA D-EA-11) | | | | ✓ |
| Instance settings (instance admins only) | — | — | — | — |

Evaluators, online rules and score definitions: project `member` and above; API keys cannot manage them (D-I-24). Dashboards: project `member` and above through a session or a read-write PAT; a `read` key and a read-only PAT read them and run their queries (D-C-16).

Prompts: every project member and a `read` key read them; members and above, read-write PATs and `write` keys create versions and move labels (`ProjectPerm::ManagePrompts`); protected labels need a project admin or a `write` key; project admins edit the protected list (D-V-10, D-V-13).

Playground runs: `ProjectPerm::RunPlayground`. Members and above run through a session or a read-write PAT, and never with an API key, because a run spends the org's provider money (D-V-26). Viewers open the playground and see its traces, but cannot run.

Annotations: a member changes or deletes only their own `human` scores; a project admin may change or delete anyone's (D-I-26, D-I-36). API keys keep their scope rules.

Annotation queues: `ManageQueues` (member, a `write` key, a read-write PAT) for queues and items; `Review` (member, sessions and read-write PATs only) for next, lease, complete, skip and reopen (D-AQ-11).

LLM connections belong to the org. Org admins and owners manage and test them, and every org member can list them without their keys (D-I-4, D-I-24).

Model prices: project admins manage a project's prices (`ProjectPerm::ManageSettings`; API keys cannot); org admins and owners manage the org's prices (`OrgPerm::ManagePrices`); every member can read both, and a `read` key can read a project's (D-T-6).

A resource outside the caller's reach answers **404**, never 403, so ids cannot be probed. A visible resource with too low a role answers **403** (D138).

Enforced in one place: every route is declared with its access (public, any signed-in user, session-only, an org permission, a project permission, or instance admin). One middleware enforces it before the handler, resolves `{org}` and `{project}`, and hands the handler the resolved access. The principal is a session user, a PAT user, or an API key. A SCIM token is a principal of its own (`Principal::Scim`): only `/scim/v2` routes accept it.

**Session-only** (a session cookie; a PAT or an API key is refused): creating, listing or revoking API keys and personal access tokens, changing members, roles, overrides or invites, the SCIM settings and tokens, the password and the account name, and dropping a project or an org (D-P-70). Exception: a PAT may revoke itself with `DELETE /api/v1/me/tokens/current` (D140). A PAT with scope `read` may call safe methods (GET, HEAD) only, plus the two reads that use POST: `POST /api/v1/projects/{project}/sql` and `POST /mcp` (D-H-4); `read, write` may call everything except those session-only routes. Approving a device code (`POST /api/auth/device/{user_code}/approve`) mints a PAT, and for `init` an ingest-only key, with the session (D-AO-3).

API keys are project-bound and their scopes map onto permissions: `ingest` (OTLP, scores write and the coding-agent context), `read` (safe methods of project-scoped routes, and `POST …/sql`, D-H-4), `write` (datasets, experiments, scores, deletions by trace), `feedback` (public feedback only). Track D enforces `read`; later tracks map `ingest`, `write` and `feedback` (D140). Deletion by user or by filter requires project `admin` with a session or a `read, write` PAT, never an API key; deleting a project requires the **org** role `admin` or `owner` (a project-admin override is not enough), and deleting an org requires `owner`; both need a session, so a stolen PAT cannot drop them (D-M-4, D-P-70). No API key reads the project member list (D-P-67). An `sk` key needs `write`, not `read`, even for `GET …/deletions`: the history names the people whose data was erased.

### Demo org (D187)

`demo.org` (spec 12) names one org of the instance by id. Every signed-in user who is not an instance admin, through a session or a PAT, reads that org as an **implicit `viewer`** when `org_members` has no row for them. A real row wins: the people who run the demo (owners, admins, members) keep their role. Nothing is written: no `org_members` row, no seat (D176), no invitation. API keys are project-bound and are not affected; the control plane's membership calls (`/api/internal/v1/memberships/lookup`, `…/orgs/{org}/members`, the users calls) see real rows only.

- **Where the role is read.** The one function that returns a user's org role for access (`http::access`, MCP's project list, `GET /api/auth/me`, `GET /api/v1/orgs`) returns `viewer` for an implicit visitor, so every route's existing check applies: reads and the read POSTs (`…/sql`, `/mcp`, dashboard queries) work; annotating, sharing, managing anything and SQL exports are `403` as for any viewer. A visitor's own private saved views and saved SQL queries are allowed, as for any viewer.
- **What a visitor sees of people.** The org's member and invite lists show the real members only. A visitor never appears in any list, so visitors never see each other.
- **Not a member.** An implicit visitor cannot leave the org (there is nothing to leave) and does not count anywhere a membership counts (seats, owners, alert recipients, the usage page's members).
- **Limits.** Requests of implicit visitors to the demo org's routes pass a per-user rate of `demo.requests_per_minute` (default 120) and at most `demo.user_in_flight` (default 4) at once per user, and at most `demo.org_in_flight` (default 16) at once across all visitors; above them the answer is `429 rate_limited` with `Retry-After`. Their SQL console requests run with at most `demo.sql_timeout` (default `"10s"`) and `demo.sql_max_rows` (default 1,000) rows; a larger `timeout_ms` or `limit` is lowered, not refused, and `truncated` says when rows were cut. Real members of the demo org are not limited this way. `POST /mcp` is not counted by these limits: it has its own per-token limits (D-H-10), and its `query_sql` returns at most 1,000 rows. Live streams (SSE) take a slot only while their handler runs, not while they stream; each visitor keeps the per-owner stream cap of the live updates, and there is no cap on all visitors' streams together. Accepted for the demo: many accounts holding streams open is what the cell's own connection limits and the operators' monitoring cover.
- **Instance admins** keep no implicit access to org data (§ above), the demo org included.

## API keys

| Kind | Format | Scopes | Use |
|---|---|---|---|
| Secret | `sk-cetra-` + 32 base62 chars | default `ingest, read, write`; any non-empty subset of them (`ingest` only for exporters, `read` only for readers such as `CETRA_INIT_READ_KEY`) | OTel exporters, SDK, CI |
| Public | `pk-cetra-` + 24 base62 chars | `feedback` | browsers (end-user feedback) |
| Personal access token | `pat-cetra-` + 32 base62 chars | `read` or `read, write`; acts as the user | CLI, MCP (the only credential `/mcp` accepts, D-H-3) |

- Stored as SHA-256 hex (high-entropy keys don't need a slow hash). The display `prefix` is the type prefix plus the first 6 random characters (for example `sk-cetra-a1B2c3`) (D140). Shown once at creation.
- Optional `expires_at`. `last_used_at` is coalesced in memory and written every 60 s and at shutdown.
- Header: `Authorization: Bearer <key>`. Credentials are read only from that header, never from a query string. The auth layer caches `hash → principal` for 60 s. Revoking or expiring a key evicts the entry at once in this process.
- CLI login: `cetra login --url https://…` gets a device code (§ Device authorization), prints the URL and the code, and opens the browser unless `--no-browser` is given. It polls until the user approves or denies on `/device`, or the code expires after 10 minutes. On approval it gets a `read, write` PAT named `CLI on <host>` (`CLI` without a host) that expires after 90 days, checks it with `GET /api/auth/me`, and saves it in `~/.config/cetra/credentials.toml` (0600) under the URL given to `--url`. Because the token expires after 90 days, `cetra login` must be run again then. `cetra logout` revokes that PAT with `DELETE /api/v1/me/tokens/current`. The device flow works when the browser runs on another machine (SSH, containers) (D-AO-4).

### Device authorization (D-AO-3)

The device flow (RFC 8628) signs a CLI in on the cell: the CLI shows a code and the URL, the user approves the code in the web UI at `/device` (spec 11), and the CLI polls for the grant. No callback server runs on the CLI's machine.

- **Codes.** `POST /api/auth/device/code` takes `{purpose, host?}`, where `purpose` is `login` or `init`. It answers a `device_code` (43 base64url characters; the store keeps only its SHA-256), a `user_code` (`XXXX-XXXX`, eight consonants from `BCDFGHJKLMNPQRSTVWXZ`, so no word is spelled), the approval page's URL with and without the code, `expires_in` 600 and `interval` 5. `host` is sanitised to `[A-Za-z0-9.-]`, at most 64 characters, and shown on the approval page. Codes live in memory only: a restart drops them. At most 10 000 are pending at once; a further code is 503 `busy`.
- **Polling.** `POST /api/auth/device/token` takes `{device_code}`. Until the user acts it answers 400 `authorization_pending`. A poll sooner than the interval answers 400 `slow_down`, and the interval grows by 5 seconds. After a denial it answers 400 `access_denied`. It answers 400 `expired_token` when the code expired, when the grant was already collected, or when the code is unknown. A grant answers 200 `{server, token, project?, ingest_key?}` once, with `Cache-Control: no-store`; `server` is the origin of `public_url`. The errors are problem+json with the RFC 8628 name in `code`.
- **Approval.** `GET /api/auth/device/{user_code}` gives the purpose and host to the approval page (404 for an unknown, expired or decided code). `POST …/approve` and `POST …/deny` decide it. They need a session: a PAT or an API key gets 403, because approving mints secrets. `login` approves with `{}`. `init` approves with `{project_id}`: the signed-in user needs the admin role on that project. A project the user cannot see is 404; one the user sees with a lower role is 403.
- **What is minted.** `login`: a PAT named `CLI on <host>` (`CLI` without a host), scopes `read, write`, expiring 90 days after the approval. `init`: a PAT named `CLI init on <host>` (`CLI init` without a host), scope `read`, expiring 90 days after the approval, and a secret key with the same name, scope `ingest` only, that does not expire, on the chosen project. Both are created in one transaction. The grant's secrets exist in memory until the first successful poll, or until one minute after the code expires, whichever comes first (D-AO-5). A grant whose answer was lost leaves its PAT and key on the account and project pages under those names, where the user can revoke them.
- **Bounds.** 10 code requests a minute per client network (an IPv6 address counts by its /64). 30 lookups, approvals and denials a minute per user. An approval that is not finished within 60 seconds falls back to pending, so a closed tab does not hold a code.
- **Phishing and CSRF.** The approval page shows the purpose and host and warns that approving gives the command access to the account; the CLI prints the code, and the user compares them. The two CLI calls carry no cookie and no `Origin`, so the origin check exempts exactly those two paths (D141). Approve and deny are session calls and keep the check.
- **Logs.** Device codes, user codes, tokens and keys are never logged; the audit log names the token's and key's ids.
- **Registration by the control plane (D-AO-14).** `POST /api/internal/v1/device/codes` lets the control plane register a code it issued on this cell. It takes a control-plane token with scope `device` (D-P-28) and no `Origin`, and the body `{device_code_sha256, user_code, purpose, host?, expires_at}`, where the hash is the lowercase hex SHA-256 of the device code. The cell checks that the hash is 64 hex characters, that the user code is in the alphabet (either spelling), and that `expires_at` is after now and at most 10 minutes plus 30 seconds of clock skew ahead; a failed check answers 400 naming the field. It sanitises `host` as above and stores the code as if it had issued it, so a registered code is polled, approved, denied and expired exactly like an issued one. Registered codes count against the 10 000 pending codes. A registration identical to a pending code (same hash, user code, purpose, host and expiry) answers 204 again; another code with the same hash or the same user code answers 409. A full cell answers 503.

| Method & path | Access | Answer |
|---|---|---|
| `POST /api/auth/device/code` | public, no `Origin` | 200 the codes; 429 over 10 a minute per network |
| `POST /api/auth/device/token` | public, no `Origin` | 200 the grant, or 400 `authorization_pending`, `slow_down`, `access_denied`, `expired_token` |
| `GET /api/auth/device/{user_code}` | session only | 200 `{purpose, host?, created_at}`; 404 |
| `POST /api/auth/device/{user_code}/approve` | session only | 204; 400 `project_id` (missing for `init`, refused for `login`); 403; 404 |
| `POST /api/auth/device/{user_code}/deny` | session only | 204; 404 |
| `POST /api/internal/v1/device/codes` | control-plane token, scope `device`, no `Origin` | 204; 400 (a malformed field); 401; 403; 409 (a clash); 503 `busy` when full |

## Email

- **Transport:** SMTP only, through `lettre` (async, tokio, rustls).
  - `CETRA_SMTP_URL`: `smtp://user:pass@host:587` (STARTTLS required), `smtps://…:465` (implicit TLS), or `smtp://host:25?tls=none` for local relays.
  - `CETRA_EMAIL_FROM`: e.g. `"cetra <noreply@acme.com>"`.
  - Resend, SES, SendGrid and Postmark all work through their SMTP relays.
- **Templates:** `minijinja` templates embedded in `cetra-server`, sent as multipart/alternative. The HTML is a simple inline-styled layout with no external assets. Links: `verify_email` uses `<public_url>/verify?token=…`, `password_reset` uses `<public_url>/reset?token=…`, and `invite` uses `<public_url>/invite/<token>`.
- **Emails in MVP:**

  | Template | Trigger |
  |---|---|
  | `invite` | member invited (link valid 7 days) |
  | `password_reset` | reset requested |
  | `verify_email` | sign-up when verification is required |
  | `alert` | alert fired / resolved (07) |
  | `test` | "send test email" in instance settings |

- **Without SMTP:**
  - invites show a copyable link in the UI;
  - password resets go through the CLI or an admin-generated link;
  - email alert channels are disabled, with an explanation;
  - email verification cannot be enabled (the setting is refused at startup).
- **Delivery:** transactional emails are sent by a background task after the response is decided (D145), with 3 retries. Alert emails go through `notification_deliveries` (durable). Recipient addresses and contents are never logged; only template name, message ID and status are.

## Audit records

Every access and membership action writes an `audit_events` row (spec 04 § Audit records) and emits the same record as a structured `tracing` event at `info` with `target = "audit"`, whose message is the action name, so operators can ship them. The row is written in the transaction of its change, or before the action runs when the action changes nothing in `meta.db` (D-EA-2). The tracing line is its last step, after the insert. A failed write fails the request.

Records carry user, org, project, key and token ids, the client IP and the user agent, never emails, names or token values (D150). The IP and user agent of a user's records are cleared when the user is deleted.

Unauthenticated failures write no row, only a tracing line: a failed login for an unknown email logs `account = "unknown"`, and a control-plane token that fails is refused without a record. A SCIM call with an unknown, revoked or expired token is refused the same way. SCIM operations that change a member record it with the SCIM token as the actor (`scim_token`), and the target is the cetra user id. A failed login for a known account writes at most one row per account per minute.

The catalog also covers project objects (evaluators, online rules, score definitions, alert rules and channels, LLM connections, model prices, prompts, datasets, issue sources, dashboards, annotation queues and the assistant setting) and data leaving the product: SQL runs, SQL and dataset exports, and MCP tool calls that name a project. SQL runs, exports and MCP tool calls record a SHA-256 of the query text, never the text, before they run. SQL runs by demo-org visitors are not recorded (D-EA-23).

Stream changes record `audit.stream_set`, `audit.stream_removed` and `audit.stream_secret_rotated`, with the host, whether a header is set and the enabled flag, never the header value or the secret (EA M4, spec 04 § Audit stream destinations).

The catalog of actions, and the reads and exports of the log, are in `docs/plans/ea-enterprise-admin/README.md` § Audit log (D-EA-1 to D-EA-10).

**Reads (track EA M3, `ee`).** Org owners and admins read their org's log with `OrgPerm::ViewAudit`, through a session or a PAT, never an API key. Instance admins read the instance log (records with no org) through `/api/v1/admin/audit-events`. Instance admins never read an org's log (D-EA-8). The instance log needs the `audit_log` feature on the licence, or a cloud cell (`entitlements.control_plane`), which needs no licence (D-EA-19). A lapse stops reads and exports with `403 plan_required`, and recording goes on. Each export records `audit.export_started` before it streams, with the format and the names of the filters, never their values (D-EA-6).

## Client IP

`trusted_proxies` is a list of CIDRs, empty by default (`CETRA_TRUSTED_PROXIES`, D142). The client IP is the socket peer, unless the peer is in the list: then it is the rightmost address of `X-Forwarded-For` that is not itself in the list. Rate limits, lockout keys and `web_sessions.ip` use that address.
