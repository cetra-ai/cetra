# W10 — Platform: alerts, MCP, access, SSO, deletion, plans; auth and email setup

**Commit subject:** `Document alerts, MCP, access control, deletion and plans`

**Goal:** the Docs topic's Platform group (seven pages) and two Self-hosting pages: authentication setup (password, GitHub, Google, OIDC with Okta, Entra ID and Keycloak) and email (SMTP).

## Read first

- README § Page list › Platform and the `self-hosting/authentication`, `self-hosting/email` rows; § Style guide; D-W-14.
- `website/STYLE.md`.
- `docs/spec/07-alerts.md`; `docs/spec/09-api.md` § MCP and the key kinds; `docs/spec/08-auth.md` (all: sign-up policy, passwords, OAuth/OIDC, roles, keys and PATs, per-org SSO, instance admins); `docs/spec/12-operations.md` § Licence keys, plans and entitlements; `docs/spec/03-storage.md` § Deletion and the retention sections; `docs/spec/04-metadata-db.md` § deletions (what a user sees: statuses).
- `website/src/content/docs/docs/self-hosting/configuration.md` § Secrets and `configuration-reference.md` § `[auth]` (W4).
- `crates/server/src/config.rs` `AuthSettings`, `GithubAuth`, `GoogleAuth`, `OidcAuth` (field names for the TOML examples).

**Check first:** `configuration-reference.md` has a `## [auth]` section with `auth.oidc[].issuer`. If not (W4 not merged), stop.

## Files

| File | Change |
|---|---|
| `website/src/content/docs/docs/platform/{alerts,mcp,api-keys,organizations,sso,deletion-and-retention,plans-and-licence}.mdx` (new) | Platform |
| `website/src/content/docs/docs/self-hosting/{authentication,email}.mdx` (new) | setup pages |
| `website/astro.config.mjs`, `index.mdx`, `tracing/users.mdx`, `tracing/privacy-controls.mdx`, `evaluation/online-rules.mdx`, `self-hosting/configuration.md` | sidebar and links |

## Steps

### 1. Platform pages

- **`platform/alerts.mdx`** "Alerts": what a rule watches (the metrics of spec 07), conditions and windows, evaluation interval, channels: Slack (incoming webhook), webhook (payload shape and signature verification, with a `json` payload example and a Python verification snippet marked `{/* untested: verifies a webhook request */}` unless it can run offline, in which case add it as a `{/* test */}` block that verifies a fixed example), email (needs SMTP, link `/docs/self-hosting/email/`); delivery, retries and the alert history; restore mode turns deliveries off; plan limits where they apply.
- **`platform/mcp.mdx`** "MCP server": what it gives a coding agent; create a personal access token (read-only is enough); the endpoint `POST <public_url>/mcp`; client configuration for Claude Code (`claude mcp add --transport http cetra <url>/mcp --header "Authorization: Bearer pat-cetra-…"`), Cursor (`.cursor/mcp.json`) and a generic client (`json`); the seven tools, one row each with arguments (from spec 09 § MCP); limits (rate, size caps, truncation notes); what it cannot do (read-only). Check the Claude Code command against `claude mcp add --help` if available; otherwise mark the block's paragraph with "check your client's documentation for the current syntax".
- **`platform/api-keys.mdx`** "API keys and tokens": a table of the three kinds (prefix, scope, where created, what it can call, can it be public); creating, listing, revoking; rotation without downtime (create the new key, deploy, revoke the old); never put an `sk-cetra-` key in a browser.
- **`platform/organizations.mdx`** "Organizations, projects and roles": orgs and projects; members and invites; the roles table (each role × each permission, from spec 08 § Roles); instance admins (self-hosted).
- **`platform/sso.mdx`** "Single sign-on": what a member sees on the login page; GitHub, Google and OIDC sign-in; per-org SSO with verified domains and enforcement (state that it is part of the Teams add-on where spec 08 and spec 12 say so); link the setup page.
- **`platform/deletion-and-retention.mdx`** "Deletion and retention": delete by user, by trace IDs, by filter, a project, an org; preview before deleting; statuses and how long until data is gone (`storage.deletion.max_delay`, link the reference); what is deleted with it (scores, evaluation traces, dataset items copied from traces, media); retention per project and the plan's cap; backups and deletions (restored backups re-apply deletions: link `/docs/self-hosting/backup/`); "Erasing a person's data (GDPR)": a step-by-step with the user-ID deletion.
- **`platform/plans-and-licence.mdx`** "Plans and licence": open core (MIT core; `ee` features under the Elastic License, locked without a licence key); the list of `ee` features from spec 12 § Licence keys; adding a licence key (`CETRA_LICENSE_KEY`); what a self-hosted instance without a key has (every core feature, no limits unless configured: `entitlements.defaults.*`); the usage page. No prices (they belong on the marketing site).

### 2. Self-hosting setup pages

- **`self-hosting/authentication.mdx`** "Authentication setup": sign-up policy (`auth.signup`, `auth.signup_domains`), password login and `auth.disable_password` (instance admins keep `/login?password=1`), email verification; then one section per provider with the provider console's steps (redirect URI `<public_url>/api/auth/oauth/<provider>/callback`: take the exact path from spec 08, not from this plan), the TOML block and the env form of the secret:
  - GitHub, Google (`[auth.github]`, `[auth.google]`, split env form);
  - OIDC generic (`[[auth.oidc]]` fields), then Okta, Microsoft Entra ID and Keycloak, each with its issuer URL pattern and console steps;
  - per-org SSO for the Teams add-on (link the platform page);
  - every TOML key on the page must be in the configuration reference (the env-var check covers `CETRA_*`; check the TOML keys by hand against `configuration-reference.md` and say so in the report).
  Provider console steps change over time: write them at the level of "create an OAuth app, set the callback URL to …, copy the client ID and secret", not menu paths.
- **`self-hosting/email.mdx`** "Email (SMTP)": `smtp_url` forms (`smtp://`, `smtps://`, `smtp+starttls://`, take the accepted schemes from `SmtpUrl` in `config.rs`), credentials in the URL and why the env file, `email_from`; examples for a generic provider; which emails cetra sends (invites, password reset, verification, alert emails); what happens without SMTP (invite links shown in the UI, `cetra admin reset-password`).

### 3. Sidebar and links

Docs topic items, after Evaluation:

```js
{
  label: "Platform",
  items: [
    { label: "Alerts", link: "/docs/platform/alerts/" },
    { label: "MCP server", link: "/docs/platform/mcp/" },
    { label: "API keys and tokens", link: "/docs/platform/api-keys/" },
    { label: "Organizations and roles", link: "/docs/platform/organizations/" },
    { label: "Single sign-on", link: "/docs/platform/sso/" },
    { label: "Deletion and retention", link: "/docs/platform/deletion-and-retention/" },
    { label: "Plans and licence", link: "/docs/platform/plans-and-licence/" },
  ],
},
```

Self-hosting topic › Configure: add "Authentication" and "Email (SMTP)" after "Configuration reference". Link from `tracing/users.mdx` (deleting a user's data), `tracing/privacy-controls.mdx` (deletion), `evaluation/online-rules.mdx` (plan limits), `self-hosting/configuration.md` (the two new pages).

## Tests

- `just check-docs` (env vars on the auth and email pages must all exist; links); `just check-py` if a webhook verification block is tested.

## Done when

- `just check` passes; nine pages exist and are in the sidebars.
