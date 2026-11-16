# 03: restore mode

Read [00-design.md](00-design.md) first, especially § Restore mode (D-N-25, D-N-27) and § What exists › Outbound calls.

**Goal:** `restore_mode = true` (or `CETRA_RESTORE_MODE=true`) starts a server that makes no outbound connection.
- The five outbound choke points are closed.
- Only the outbound background loops are left unstarted.
- The storage's object deletion (D-N-34) is wired by phase 02, which runs after phase 01's storage switch; this phase does not touch storage.
- The mode is visible in a startup log line and in `/readyz`.

Phase 04 adds the clear 503 answers of the user-triggered handlers. This phase makes sure nothing can leave the process even without them.

**Edit only:**
- `crates/eval/src/egress.rs`, `crates/eval/src/error.rs`, and every `EgressPolicy { … }` literal in the workspace;
- `crates/server/src/alerts/deliver.rs` (its exhaustive `match` on `EgressError`, near line 159), `crates/server/src/alerts/mod.rs` (`spawn_loops`), `crates/server/src/eval/mod.rs` (`spawn`);
- every `ServerConfig { … }` literal: `crates/server/src/test_support/mod.rs` (`default_config` and `config`), `crates/server/src/ingest/fixtures_tests.rs`; `grep -rn "ServerConfig {" crates/` finds all;
- `crates/server/src/config.rs`;
- `crates/server/src/serve.rs`;
- `crates/server/src/email/mod.rs`;
- `crates/server/src/auth/oidc/mod.rs`;
- `crates/server/src/ee/org_sso/` (only if `txt_resolver` goes there);
- `crates/server/src/test_support/mod.rs`;
- `crates/server/src/http/health.rs` (the `restore_mode` field);
- `docs/spec/12-operations.md` (§ Configuration, a row for the key).

**Commit:** `Add restore mode`

**Needs:** nothing. If 02 is merged, keep its `readyz` body and add the field to it.

## Read first

- `crates/eval/src/egress.rs` lines 1–140, and `GuardedResolver` (search `impl Resolve for GuardedResolver`).
- `crates/eval/src/error.rs`: `EgressError` and its `label`/`Display` if it has them.
- `crates/server/src/serve.rs` `bind`, from opening the secrets to `Ok(Running { … })`: the `EvalRunner`, `Mailer`, `Providers` and `OrgSso` constructors, and the spawned tasks.
- `crates/server/src/email/mod.rs`: `Transport`, `Mailer::from_config`, `send_now`, `send_in_background`, `MailError` and its `label`.
- `crates/server/src/auth/oidc/mod.rs`: `Providers::from_config`, `identity_client`.
- `crates/server/src/test_support/mod.rs` lines 190–330: how `TestApp` builds the same pieces.
- `crates/server/src/config.rs`: how a top-level key like `grpc_addr` is declared, defaulted and read from `CETRA_*`, and the config tests near the end of the file.

## Changes

### 1. `EgressPolicy` (cetra-eval)

```rust
pub struct EgressPolicy {
    /// Keep addresses that [`is_denied`] would drop.
    pub allow_private_network: bool,
    /// Restore mode (N M3, D-N-25): every request is refused before it is sent.
    pub restore_mode: bool,
}
```

- **`allows_url`** returns `false` when `restore_mode` is set.
- **`EgressError`** gets a new variant, `RestoreMode`:
  - `Display`: `"outbound calls are off: restore mode"`;
  - label: `restore_mode`, if the enum has labels;
  - `EgressError::class()` (`eval/src/error.rs`, near line 338) puts it in the `Permanent` class: nothing retries it;
  - every `match` on `EgressError` in the workspace gets an arm. `grep -rn "EgressError::" crates` finds them, including `crates/server/src/alerts/deliver.rs` near line 159. In outcomes it is a failure like `Denied`; the server handlers come in phase 04.
- **`EgressClient::post`** returns `Err(EgressError::RestoreMode)` as its **first** statement when `self.policy.restore_mode`.
- **`GuardedResolver`** gets a `restore_mode: bool` set from the policy in `client_builder`. When it is true, `resolve` returns an error for every name, so a client built with that policy cannot connect to any host name. Use the same error path the resolver uses when every address is denied.
- **Update every `EgressPolicy { … }` literal** with `restore_mode: false`, except the two server sites below:
  - `crates/eval/tests/*.rs` and `crates/eval/src/egress.rs` tests;
  - `crates/server/src/auth/oidc/provider.rs` test;
  - `crates/server/src/test_support/mod.rs`.

  `grep -rn "EgressPolicy {" crates` must show a `restore_mode` in each.

### 2. Config (cetra-server)

`ServerConfig` gets:

```rust
/// Restore mode (N M3, D-N-25): serve reads and run no outbound work. For restore drills.
#[serde(default)]
pub restore_mode: bool,
```

- Place it after `grpc_addr`.
- Make sure `CETRA_RESTORE_MODE=true` sets it: follow how the env source maps a top-level key, and add a case to the env test that covers top-level keys.
- `docs/spec/12-operations.md` § Configuration gets the key in its table or list, in that section's format: `restore_mode` (`CETRA_RESTORE_MODE`), default `false`, "no outbound work: alert deliveries, webhooks, judges, email and identity providers are off; for restore drills (D-N-25)".

### 3. The constructors (`serve.rs` and `TestApp`)

Put each restore decision **inside** the function that builds the piece, so `serve.rs` and `TestApp` behave the same:

- **Both `EgressPolicy` literals in `serve.rs`** (`EvalRunner::new` and `OrgSso::new`) get `restore_mode: config.restore_mode`. `TestApp` gets the same from its config.
- **`OrgSso`'s DNS resolver.** In `serve.rs`, pass `crate::ee::org_sso::dns::TxtResolver::Unconfigured` instead of `TxtResolver::system()` when `config.restore_mode`. The variant already exists and looks nothing up; check its behaviour in `ee/org_sso/dns.rs`. `TestApp` builds its own resolver for the DNS tests; leave it.
- **`Providers`.** Add `Providers::for_config(config: &ServerConfig, http: &reqwest::Client) -> Result<Self, OidcError>`:
  - It returns `Self { github: None, oidc: Vec::new() }` when `config.restore_mode`, and `Self::from_config(&config.auth, http)` otherwise.
  - `serve.rs` and `TestApp` call it instead of `from_config`.
  - If `from_config` then has no other caller, make it private.
- **`Mailer::from_config`.** When `config.restore_mode` and `smtp_url` is set, build the mailer with `Transport::Off` instead of `Transport::Smtp(…)`.
  - Validate `from` and the templates as before, so a broken config still fails at start.
  - Add `Transport::Off` (not `cfg(test)`). Every `match` on `Transport` gets an arm: `transmit` (the send path), `sent()` and the stub inbox accessors. In particular:
    - `send_now` → `Err(MailError::RestoreMode)`;
    - the stub inbox accessors → empty.
  - `send_in_background` for `Off` logs `tracing::warn!(template = template_name(&email), "email not sent: restore mode")` (the free function `template_name` it already logs with) and returns without spawning. Never log the address.
  - `MailError::RestoreMode`: `Display` `"email is off: restore mode"`; its arm in `MailError::kind()` returns `"restore_mode"` (`kind()` also feeds its `Debug`).
  - Keep `smtp_enabled` in `/api/auth/config` as `mailer.is_some()`; it stays `true`. Note it in the report.

### 4. The loops (`serve.rs` `bind`)

When `config.restore_mode`:

| Loop | Restore mode |
|---|---|
| `alerts::spawn_loops` | spawn only the history-prune loop. Change `spawn_loops(state, timing)` to read `state.config.restore_mode` and return only `spawn_history_prune_loop` when set. |
| `eval::spawn` | do not spawn `run_dispatcher` and `run_backfills`. Keep the other five. `EvalTasks::stop` must handle the shorter list: check how it waits on the dispatcher's workers and keep that correct. |
| `spawn_notices` | not spawned |
| `spawn_org_sso` | unchanged: it only reloads from `meta.db` |
| everything else | unchanged |

**The startup log.** After the loops are spawned, when `restore_mode`, log once:

```rust
tracing::warn!("restore mode: outbound work is off (alerts, webhooks, judges, email, identity providers)");
```

### 5. `/readyz` body

`Probe` gets `#[serde(skip_serializing_if = "std::ops::Not::not")] restore_mode: bool`, set from `state.config.restore_mode` on the ready answer. `healthz` sets it to `false`, so it is never shown there.

### Note for the report

Restore mode leaves **API keys** as the only way in for:
- everyone, when `auth.disable_password` is set (Q's cloud cells, Q2.27): there are no providers and no password;
- members of orgs that enforce SSO (`enforces()` in `auth/session.rs` and `auth/principal.rs`): their password login is blocked, and org-SSO login is refused.

Say so in the report, so Q's drill reads with an API key. D-N-27 is the planner's choice.

## Tests

**cetra-eval (`egress.rs` tests):**
- `restore_mode_refuses_before_sending`:
  - Start a local listener with the existing helper (`tokio::net::TcpListener` on `127.0.0.1:0`, as the open-policy tests do), with `allow_private_network: true, restore_mode: true`.
  - `post` gives `Err(EgressError::RestoreMode)`.
  - The listener's `accept` does not complete within 200 ms (`tokio::time::timeout`).
- `restore_mode_denies_every_url`: `allows_url` is `false` for `https://example.com/` and `http://127.0.0.1/`.
- `identity_client_in_restore_mode_cannot_resolve`: `identity_http_client(policy with restore_mode)` sends a GET to `http://localhost:<port>/` of a local listener and gets an error. Nothing is accepted.

**cetra-server:**
- `config.rs`: `restore_mode` defaults to `false`; `restore_mode = true` in TOML sets it; `CETRA_RESTORE_MODE=true` sets it.
- `email/mod.rs`:
  - `restore_mode_mailer_refuses`: build the mailer with `Mailer::from_config` on a config with `smtp_url`, `email_from` and `restore_mode = true`. Not through `TestApp::with_mailer_config`: that installs the stub transport, which still "sends". `send_now` is `Err(MailError::RestoreMode)`, and `send_in_background` logs `email not sent: restore mode` (use `capture_logs`) and no address.
  - Without `smtp_url` the mailer is `None`.
- `auth/oidc/mod.rs`: `restore_mode_builds_no_providers`, with GitHub and Google configured and `restore_mode`, `for_config(…).list()` is empty; without `restore_mode`, it has both.
- `routes/auth.rs` (or wherever `/api/auth/config` is tested): with `TestApp::with_github(…, |c| c.restore_mode = true)`, `providers` is `[]`.
- `http/health.rs`: `readyz_reports_restore_mode`. `TestApp::with_config(|c| c.restore_mode = true)` gives `/readyz` `{"status":"ready","restore_mode":true}`; without it, the body has no `restore_mode` key.
- `ee/org_sso` or `serve.rs`: `restore_mode_uses_no_dns`. Put the choice in a small function, `fn txt_resolver(config: &ServerConfig) -> TxtResolver` (`Unconfigured` in restore mode, `system()` otherwise), called by `bind`, and test that it returns `Unconfigured` when `restore_mode` is set (match on the variant).
- `serve.rs`: `restore_mode_logs_at_startup`. Start through `bind` with `restore_mode = true`, the way the other `serve.rs` tests do, under `capture_logs`. The log contains `restore mode: outbound work is off`, and `/healthz` answers. Shut down cleanly.
- `alerts/mod.rs`: `restore_mode_spawns_only_the_prune_loop`. `spawn_loops` on a restore-mode state returns one handle; abort it.
- `eval/mod.rs`: `restore_mode_spawns_no_dispatcher`. `eval::spawn` on a restore-mode state returns five handles, and `stop` returns within `drain` + 5 s.

## Verify

```bash
cargo test -p cetra-eval --all-features restore_mode
cargo test -p cetra-server --all-features restore_mode
just check
```

## Done when

- [ ] All five choke points honour `restore_mode`, and the four loops (alert evaluation, alert delivery, eval dispatcher, backfill runner) and usage notices are not spawned.
- [ ] The startup line and the `/readyz` field exist.
- [ ] The tests pass, and spec 12 lists the key.
- [ ] `just check` passes. One commit.
