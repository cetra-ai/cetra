# K · R20 — Email recipient policy (`alerts.email_recipients`)

Status: planned 2026-11-20

Track Q (cetra-control `docs/00-design.md` § Requests, R20; cross-track K-3) asks for a cell setting that limits email alert channels, and their "Send test", to the org's verified members whatever `auth.signup` is. Today D-K-19 applies that rule only under `auth.signup = "open"`; cloud cells run `invite_only`, so with SMTP on any member could make a cell mail any address. This plan adds `alerts.email_recipients = "members" | "any"` (D-K-21) and makes it the only thing the alert code reads. Two phases, in order: the config key, then its use in the channel routes and the delivery code.

**Prerequisites:** none beyond `main`. The decision (D-K-21), specs 07 and 12, the configuration reference, the alerts page, the roadmap row and the CHANGELOG line are written in the plan's own commit; the phases change only Rust code and the generated API files.

**Read first:**
- `AGENTS.md` (rules, tests, commands).
- `docs/mvp-decisions.md` § Track K: D-K-19 and D-K-21.
- `docs/spec/07-alerts.md` § Channels (the `email` row) and § Delivery.
- `docs/spec/12-operations.md` § Configuration, the `alerts.email_recipients` row.
- `website/src/content/docs/docs/self-hosting/configuration-reference.md` § `[alerts]` (already written; the loader must match it).
- `crates/server/src/config.rs`: `OrgsConfig` (the pattern to copy), `ServerConfig`, the tests `orgs_*` and `the_configuration_reference_matches_the_loader`.
- `crates/server/src/routes/channels.rs`: `member_scope`, `check_members`, `test_channel`, and the tests `open_signup_limits_recipients_to_verified_members`, `invite_only_allows_any_recipient`, `test_send`, `email_body`, `create_ok`.
- `crates/server/src/alerts/deliver.rs`: `send_email`; `crates/server/src/alerts/deliver/tests.rs`: `open_signup_skips_recipients_that_are_not_verified_members`.
- `crates/server/src/api/openapi.rs`: the `create_channel` and `update_channel` descriptions.

## What it is, and what it is not

When done:
- `[alerts] email_recipients = "members"` (or `CETRA_ALERTS__EMAIL_RECIPIENTS=members`) applies D-K-19's member rule under every signup policy: create and update refuse a non-member with `400` on `recipients`; a delivery and a "Send test" skip non-members and fail with a count.
- `"any"` allows any address under every signup policy.
- Unset, the behaviour is exactly today's: `members` under `open`, `any` otherwise.

Out of scope:
- No UI change: the channel form already shows the `400` field error. No API field exposes the policy.
- No migration and no change to stored channels: a channel saved under an earlier policy is checked again at each send (D-K-21).
- Invite and auth mail (spec 08) are not alert mail and are unchanged.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-K-19 | Test send rate limits, the daily cap per org, and the member rule (its error texts). |
| D-K-21 | `alerts.email_recipients`: `members` / `any`; unset follows `auth.signup` (`open` → `members`, else `any`); read only through `ServerConfig::email_recipients`; explicit `any` under `open` is allowed; stored channels are rechecked at each send. |

User's answer: track Q's session reported "Yes, schedule it" for R20 (2026-11-20). The default (unset keeps today's behaviour) is the one the request proposes.

Error texts (unchanged from D-K-19, listed so no phase changes them):
- channel create and update: `400`, field `recipients`, message `must be verified members of this organization`;
- delivery and test send, when some recipients were skipped and the rest were sent: `{n} recipient(s) skipped: not verified members of the organization` (permanent);
- when every recipient is skipped: `no recipient is a verified member of the organization` (permanent);
- a test send answers `200 {"ok": false, "error": <that text>}`.

## Versions

No new dependency.

## Phases

```
R20.1 config  ──►  R20.2 use sites
```

R20.2 needs R20.1's type and method. They run in order, one agent each.

| Phase | Edit |
|---|---|
| R20.1 | `crates/server/src/config.rs`, `crates/server/src/test_support/mod.rs`, `crates/server/src/ingest/fixtures_tests.rs` |
| R20.2 | `crates/server/src/routes/channels.rs`, `crates/server/src/alerts/deliver.rs`, `crates/server/src/alerts/deliver/tests.rs`, `crates/server/src/api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` |

Per the user's rule, no phase runs the full checks: each phase compiles and runs only its named tests. `just check` runs once at the end of the plan, by the coordinating session.

---

### R20.1 — The `[alerts]` config table

**Goal:** `ServerConfig` gains `alerts: AlertsConfig` with `email_recipients: Option<EmailRecipients>`, and `ServerConfig::email_recipients()` resolves it.

**Edit:** `crates/server/src/config.rs`, `crates/server/src/test_support/mod.rs`, `crates/server/src/ingest/fixtures_tests.rs`. Nothing else.

**Steps:**

1. In `crates/server/src/config.rs`, just above `/// \`[orgs]\` (D-P-31).`, add exactly:

```rust
/// Whom email alert channels may reach (D-K-21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailRecipients {
    /// Only verified, enabled members of the channel's org.
    Members,
    /// Any address.
    Any,
}

/// `[alerts]` (D-K-21).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AlertsConfig {
    /// Unset: [`EmailRecipients::Members`] under open signup, [`EmailRecipients::Any`] otherwise.
    /// Read it through [`ServerConfig::email_recipients`].
    pub email_recipients: Option<EmailRecipients>,
}
```

2. In `ServerConfig`, add a field right after `orgs`:

```rust
    /// `[alerts]` settings (D-K-21).
    #[serde(default)]
    pub alerts: AlertsConfig,
```

3. In `impl ServerConfig`, after `storage_config`, add exactly:

```rust
    /// Whom email alert channels and their test sends may reach (D-K-21): `alerts.email_recipients`,
    /// or when it is unset, members under open signup and any address otherwise.
    pub fn email_recipients(&self) -> EmailRecipients {
        self.alerts
            .email_recipients
            .unwrap_or(match self.auth.signup {
                SignupPolicy::Open => EmailRecipients::Members,
                SignupPolicy::InviteOnly | SignupPolicy::Disabled => EmailRecipients::Any,
            })
    }
```

   `SignupPolicy` is already imported in `config.rs` (`crate::api::account::SignupPolicy`); if the non-test module does not import it, add the import.

4. In `crates/server/src/test_support/mod.rs` (two `ServerConfig { … }` literals, at `default_config` and `config`) and `crates/server/src/ingest/fixtures_tests.rs` (`serve_config`), add `alerts: AlertsConfig::default(),` after `orgs: OrgsConfig::default(),`, and import `AlertsConfig` from `crate::config` next to `OrgsConfig` in each file.

5. No `ALIASES` entry: the env form is the general `CETRA_ALERTS__EMAIL_RECIPIENTS`, and the reference's Environment column stays empty.

**Tests** (in `config.rs` `mod tests`, next to the `orgs_*` tests; import `AlertsConfig` and `EmailRecipients` in its `use super::{…}`):
- `alerts_default_follows_the_signup_policy`: `load_from(None, &[])` gives `config.alerts == AlertsConfig::default()` and `email_recipients() == Any`; with `&[("CETRA_AUTH_SIGNUP", "open")]` it is `Members`; with `"disabled"` it is `Any`.
- `alerts_email_recipients_members_under_invite_only`: TOML `"[alerts]\nemail_recipients = \"members\"\n"` → `config.alerts.email_recipients == Some(Members)` and `email_recipients() == Members` (signup is the default `invite_only`).
- `alerts_email_recipients_any_under_open_signup`: TOML `"[alerts]\nemail_recipients = \"any\"\n"` with env `CETRA_AUTH_SIGNUP=open` → `email_recipients() == Any`.
- `alerts_email_recipients_from_env`: env `CETRA_ALERTS__EMAIL_RECIPIENTS=members` → `Members`.
- `alerts_email_recipients_rejects_an_unknown_value`: TOML `email_recipients = "everyone"` → `Err(ConfigError::Figment(_))`; `[alerts]\nzz = 1` → `Err(ConfigError::Figment(_))`.
- The existing `the_configuration_reference_matches_the_loader` must pass unchanged (the reference row is already written).

**Done when:**
- `cargo test -p cetra-server --lib config::tests` passes.
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes.
- `cargo fmt --check` passes.

**Commit:** `Add the alerts.email_recipients setting`

---

### R20.2 — Read the policy in the channel routes and the delivery

**Goal:** channel create, update, "Send test" and delivery apply the member rule when `state.config.email_recipients()` is `Members`, and nothing in the alert code reads `auth.signup`.

**Edit:** `crates/server/src/routes/channels.rs`, `crates/server/src/alerts/deliver.rs`, `crates/server/src/alerts/deliver/tests.rs`, `crates/server/src/api/openapi.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts` (the last two regenerated by `just openapi`, never edited by hand). Nothing else.

**Steps:**

1. `routes/channels.rs`, `member_scope`: replace the condition `state.config.auth.signup != SignupPolicy::Open` with `state.config.email_recipients() != EmailRecipients::Members` (import `crate::config::EmailRecipients`). Change its doc comment to: "The verified member emails that the recipients of a `kind` channel must stay within, when `alerts.email_recipients` is `members` (D-K-19, D-K-21). `None` when any address is allowed." Change `check_members`'s first doc line from "Under open signup" to "When the policy is `members`," and cite `(D-K-19, D-K-21)`. Remove the non-test `use crate::api::account::SignupPolicy;` if nothing else uses it (keep the test module's import only if a test still uses it).
2. `alerts/deliver.rs`, `send_email`: replace `if state.config.auth.signup == SignupPolicy::Open {` with `if state.config.email_recipients() == EmailRecipients::Members {`; import `crate::config::EmailRecipients`; remove the `SignupPolicy` import if unused. Change the doc paragraph "Under open signup only verified members of the org get mail (D-K-19)" to "When `alerts.email_recipients` is `members`, only verified members of the org get mail (D-K-19, D-K-21)". The skip logic, the messages and the cap stay as they are. `test_channel` already sends through `send_one` → `send_email`, so "Send test" follows the same policy with no change of its own.
3. `api/openapi.rs`: in both channel descriptions (the create one is a `\`-continued string literal; keep that layout and wrap at the same width) replace "Under open signup every email recipient must be a verified member of the org (400 on recipients)." with "When `alerts.email_recipients` is `members` (the default under open signup), every email recipient must be a verified member of the org (400 on recipients)."
4. Run `just openapi` to regenerate `crates/server/openapi.json` and `web/src/api/schema.gen.ts`; both change (the description text is in them). Commit both.
5. After the change, `SignupPolicy` appears in `routes/channels.rs` only inside `mod tests`, and in `alerts/` only in `deliver/tests.rs`: delete the non-test imports at the top of `channels.rs` and `deliver.rs` (their only uses were the two conditions replaced in steps 1–2).

**Tests:**

Helpers that exist (use them as they are): `TestApp::with_mailer_config(|config| …)`, `app.seed()` → `Seeded` (user `ada@example.com`, `.org_id`, `.user_id`, `.cookie`), `app.verify_email(user_id)`, `app.add_member(&seeded, email, OrgRole::Member)` → `Member { id, cookie }`, `app.sent()` → `Vec<SentMail>` (field `.to`). In `routes/channels.rs` tests: `channels(&seeded)` (the list path), `email_body(name, &[…])`, `create_ok(&app, &seeded, body)` → `NotificationChannel`, `send(&app, Method, &path, &cookie, Option<Value>)` → `TestResponse`, `problem_of(&response)`, `pairs(&[(field, message)])`, `test_send(&app, id, &seeded, &seeded.cookie)`; copy the PATCH call of `open_signup_limits_recipients_to_verified_members`. In `deliver/tests.rs`: `email(&[…])`, `send_one(&app.state, org_id, &config, &sample_notice(MessageKind::Fired, NOW), Uuid::now_v7(), NOW)`, `permanent(text)`. Add `use crate::config::EmailRecipients;` to both test modules (and `use crate::meta::members::remove_org_member;` to the channels tests).

In `routes/channels.rs` `mod tests`:
- Keep `open_signup_limits_recipients_to_verified_members` unchanged (it proves the unset default under `open`).
- Keep `invite_only_allows_any_recipient` unchanged (the unset default under `invite_only`).
- `members_policy_limits_recipients_under_invite_only`: `TestApp::with_mailer_config(|c| c.alerts.email_recipients = Some(EmailRecipients::Members))` (signup stays `invite_only`). Seed, `verify_email(seeded.user_id)`. POST `email_body("Mail", &["eve@example.com"])` → `400` with `pairs(&[("recipients", "must be verified members of this organization")])`. `create_ok` with `["ada@example.com"]` succeeds; PATCH it with `{"recipients": ["eve@example.com"]}` → the same `400`.
- `any_policy_allows_any_recipient_under_open_signup`: config `auth.signup = Open` and `alerts.email_recipients = Some(Any)`; `create_ok` with `["eve@example.com"]` succeeds and lists it.
- `members_policy_test_send_skips_a_recipient_who_left`: config `alerts.email_recipients = Some(Members)`. Seed, verify the seeded user, add member `ben@example.com` and verify him; `create_ok` with `["ada@example.com", "ben@example.com"]`. Remove ben from the org (the stored-channel case of D-K-21) with the existing `crate::meta::members::remove_org_member` (members.rs, `pub(crate)`, returns `Result<(), MetaError>`), in the form of `TestApp::verify_email`:
  ```rust
  let (org_id, ben_id) = (seeded.org_id, ben.id);
  app.state
      .meta
      .write(move |conn| Box::pin(async move { remove_org_member(conn, org_id, ben_id).await }))
      .await
      .unwrap();
  ``` Then `test_send` → `200` with `{"ok": false, "error": "1 recipient(s) skipped: not verified members of the organization"}`, and `app.sent()` has exactly one mail, to `ada@example.com`.

In `alerts/deliver/tests.rs`:
- Keep `open_signup_skips_recipients_that_are_not_verified_members` unchanged.
- `members_policy_skips_non_members_under_invite_only`: config `alerts.email_recipients = Some(Members)` only; seed, verify the seeded user; `send_one` with `email(&["ada@example.com", "eve@example.com"])` → `Err(permanent("1 recipient(s) skipped: not verified members of the organization"))`; sent to `["ada@example.com"]` only.
- `members_policy_with_no_member_fails_without_sending`: same config; `email(&["eve@example.com"])` → `Err(permanent("no recipient is a verified member of the organization"))`; `app.sent()` empty.
- `any_policy_sends_to_everyone_under_open_signup`: config `auth.signup = Open`, `alerts.email_recipients = Some(Any)`; `email(&["eve@example.com"])` → `Ok(())`; sent to `["eve@example.com"]`.

**Done when:**
- `cargo test -p cetra-server --lib routes::channels::tests alerts::deliver::tests` passes (run the two filters as two commands if one does not accept both).
- `cargo clippy -p cetra-server --all-targets -- -D warnings` and `cargo fmt --check` pass.
- `just openapi` leaves no further diff when run a second time.

**Commit:** `Apply alerts.email_recipients to alert email`

---

## After the phases

The coordinating session runs `just check` once, then a Sonnet review of the branch against `origin/main`, fixes its findings, and opens one PR. Track Q moves its cetra pin to the merge commit and sets `[alerts] email_recipients = "members"` on its cells.
