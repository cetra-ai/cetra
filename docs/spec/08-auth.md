# 08 — Authentication, authorization, email

## Bootstrapping

- On first start with an empty `users` table, the UI shows a **setup page** at `/setup?token=…`. While no user exists, `cetra serve` generates a random setup token at startup and logs that URL once at `warn` (D144). This is the only secret that is ever logged. `POST /api/auth/setup` requires the token. The first user signs up with email/password (or via a configured OIDC provider), becomes `is_instance_admin`, and creates the first org + project (owner role). An API key is shown at the end with copy-paste OTel/SDK snippets.
- Headless alternative: `CETRA_INIT_ADMIN_EMAIL`, `CETRA_INIT_ADMIN_PASSWORD`, `CETRA_INIT_ORG`, `CETRA_INIT_PROJECT`, `CETRA_INIT_SECRET_KEY` (optional: a pre-chosen `sk` value, for IaC). It runs once, in one transaction, only when `users` is empty, and does not need the setup token. When `CETRA_INIT_SECRET_KEY` is set, it must be `sk-cetra-` plus 32 base62 characters, or startup fails.

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
- **Rate limiting and lockout** are in memory; a restart clears them (D143). Login: 10 per minute per (client IP, email). Signup, setup, password forgot and verification resend: 5 per minute per client IP; password forgot and resend also 3 per hour per email. After 20 consecutive failed password logins for one email, password login for that email is locked for 15 minutes. A success or a password reset clears the lockout. Lockout does not block OAuth/OIDC login or password reset.
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
- **CSRF (D141):** every state-changing request (not GET, HEAD, OPTIONS) authenticated by the session cookie, and every state-changing request under `/api/auth/` whatever its credentials (login, signup, setup, password reset, invite accept, logout), must carry an `Origin` equal to the origin of `public_url`. Requests authenticated by a bearer token skip the check. JSON bodies require `Content-Type: application/json`.
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
| Manage API keys, LLM connections, project settings (retention), GDPR deletions | | | ✓ | ✓ |
| Create/delete projects, manage members & invites, org limits | | | ✓ | ✓ |
| Delete org, transfer ownership, change owners | | | | ✓ |
| Instance settings (instance admins only) | — | — | — | — |

Evaluators, online rules and score definitions: project `member` and above; API keys cannot manage them (D-I-24). Dashboards: project `member` and above through a session or a read-write PAT; a `read` key and a read-only PAT read them and run their queries (D-C-16).

Prompts: every project member and a `read` key read them; members and above, read-write PATs and `write` keys create versions and move labels (`ProjectPerm::ManagePrompts`); protected labels need a project admin or a `write` key; project admins edit the protected list (D-V-10, D-V-13).

Playground runs: `ProjectPerm::RunPlayground`. Members and above run through a session or a read-write PAT, and never with an API key, because a run spends the org's provider money (D-V-26). Viewers open the playground and see its traces, but cannot run.

Annotations: a member changes or deletes only their own `human` scores; a project admin may change or delete anyone's (D-I-26, D-I-36). API keys keep their scope rules.

LLM connections belong to the org. Org admins and owners manage and test them, and every org member can list them without their keys (D-I-4, D-I-24).

Model prices: project admins manage a project's prices (`ProjectPerm::ManageSettings`; API keys cannot); org admins and owners manage the org's prices (`OrgPerm::ManagePrices`); every member can read both, and a `read` key can read a project's (D-T-6).

A resource outside the caller's reach answers **404**, never 403, so ids cannot be probed. A visible resource with too low a role answers **403** (D138).

Enforced in one place: every route is declared with its access (public, any signed-in user, session-only, an org permission, a project permission, or instance admin). One middleware enforces it before the handler, resolves `{org}` and `{project}`, and hands the handler the resolved access. The principal is a session user, a PAT user, or an API key.

**Session-only** (a session cookie; a PAT or an API key is refused): creating, listing or revoking API keys and personal access tokens, changing members, roles, overrides or invites, the password and the account name, and dropping a project or an org (D-P-70). Exception: a PAT may revoke itself with `DELETE /api/v1/me/tokens/current` (D140). A PAT with scope `read` may call safe methods (GET, HEAD) only, plus the two reads that use POST: `POST /api/v1/projects/{project}/sql` and `POST /mcp` (D-H-4); `read, write` may call everything except those session-only routes.

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
| Secret | `sk-cetra-` + 32 base62 chars | default `ingest, read, write`; can be `ingest` only | OTel exporters, SDK, CI |
| Public | `pk-cetra-` + 24 base62 chars | `feedback` | browsers (end-user feedback) |
| Personal access token | `pat-cetra-` + 32 base62 chars | `read` or `read, write`; acts as the user | CLI, MCP (the only credential `/mcp` accepts, D-H-3) |

- Stored as SHA-256 hex (high-entropy keys don't need a slow hash). The display `prefix` is the type prefix plus the first 6 random characters (for example `sk-cetra-a1B2c3`) (D140). Shown once at creation.
- Optional `expires_at`. `last_used_at` is coalesced in memory and written every 60 s and at shutdown.
- Header: `Authorization: Bearer <key>`. Credentials are read only from that header, never from a query string. The auth layer caches `hash → principal` for 60 s. Revoking or expiring a key evicts the entry at once in this process.
- CLI login: `cetra login --url https://…` starts a localhost callback server and opens the browser at `/cli-auth?port=…&state=…&host=…`. `host` is the machine's name, sanitised to `[A-Za-z0-9.-]`, at most 64 characters, and left out when nothing usable remains. The callback host is always `127.0.0.1`; only the port comes from the query. The page warns "Only continue if you just ran `cetra login` on this computer." and shows `host` as the name the CLI reported. After the user confirms, the page creates a PAT that expires after 90 days, and the browser sends the token to the callback as a top-level form POST to `http://127.0.0.1:<port>/callback` (`application/x-www-form-urlencoded`, fields `token` and `state`). The token therefore never appears in a URL or in browser history. The CLI reads the body (`Content-Length` required, at most 8 KiB, larger bodies refused unread), checks `state`, and answers "You're signed in. Return to the terminal." A GET that carries a `token` is refused with 400 and the CLI keeps waiting, as does a request with a wrong `state`. If the user cancels, the browser is sent to `/callback?error=access_denied&state=…`; the CLI answers "Sign-in cancelled" and `cetra login` fails at once with "sign-in cancelled in the browser". Because the token expires after 90 days, `cetra login` must be run again then. The token is saved in `~/.config/cetra/credentials.toml` (0600). `cetra logout` revokes that PAT with `DELETE /api/v1/me/tokens/current`.

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

## Auth events log

Not a full audit log (enterprise, later). Logins, failed logins, key creation/revocation, role changes and deletions are emitted as structured `tracing` events at `info` with `target = "audit"`, so operators can ship them. Events carry user, org, project and key ids, never emails, names or token values (D150). A failed login for an unknown email logs `account = "unknown"`.

## Client IP

`trusted_proxies` is a list of CIDRs, empty by default (`CETRA_TRUSTED_PROXIES`, D142). The client IP is the socket peer, unless the peer is in the list: then it is the rightmost address of `X-Forwarded-For` that is not itself in the list. Rate limits, lockout keys and `web_sessions.ip` use that address.
