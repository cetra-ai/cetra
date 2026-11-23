# EA M1 — Audit recording core

**Summary.**
- This milestone adds the `audit_events` table and `audit::record`, which writes one audit record in the `meta.db` transaction of the change it describes, and emits the `tracing` line operators already ship.
- It replaces every `target: "audit"` call in the access and membership areas with records: sign-in, orgs and projects, members, invites and overrides, credentials, SSO, instance and control plane, and deletions. It fills the gaps the design lists.
- It adds `audit.retention_days` and the hourly purge, deletes an org's records when its drop is applied, and clears IP and user agent when a user is deleted.
- Nothing reads the records yet: that is M3.

**Prerequisites.**
- Branch `claude/ea-enterprise-admin` in `/home/sebastien/travaux/llm-observer/.claude/worktrees/ea-design`, with the approved design (`docs/plans/ea-enterprise-admin/README.md`).
- Next migration number: `0031`.

**Read first.**
- `docs/plans/ea-enterprise-admin/README.md` § Audit log (all of it) and decisions D-EA-1 to D-EA-9 and D-EA-20 in `docs/mvp-decisions.md` § Track EA.
- `AGENTS.md` § Rules, § Tests, § Commits and reports.
- `crates/server/src/meta/mod.rs:170-205` (`MetaDb::write`: one writer, `BEGIN IMMEDIATE`, commit on `Ok`, rollback on `Err`).
- `crates/server/src/http/client_ip.rs` (the client IP rule, D142).
- `crates/server/src/auth/principal.rs:184-230` (`authenticate` inserts `Option<Principal>`) and `crates/server/src/http/access.rs:289-380` (`prepare` inserts the bare `Principal`, not for `Access::Public`).
- `crates/server/src/test_support/mod.rs:455-466, 1398-1415, 1609-1640` (`TestApp::request`, `ConnectInfo`, `capture_logs`).
- The facts file `/home/sebastien/.config/Claude-extra/side-session-notes/local_1c672712-2cc7-40e9-8d6b-d582417356b7/ea-m1-code-facts.md`: every call site with its closure and success expression. The phase tables below cite its line numbers; re-locate each by its function name if lines moved.

## What it is, and what it is not

**What works when it's done:**
- Every action in the M1 catalog writes a row to `audit_events`:
  - within the transaction of its change, after every check that can roll it back; or
  - in a transaction of its own, for actions that change nothing in `meta.db`.
- Each row carries the actor, the org and project, the target, typed details, the IP and the user agent.
- After the row is written, `audit::record` emits one `tracing` line with target `audit` whose message is the action name (e.g. `member.role_changed`).
- A failed sign-in for a known account writes at most one row per account per minute. An unknown account, and a SCIM or control-plane token that fails, write no row.
- `audit.retention_days` (default 365, 30 or more, `0` = forever) drives an hourly purge.
- An applied org drop deletes the org's rows and writes an instance record `org.dropped`. Deleting a user clears `ip` and `user_agent` on the rows they acted in.

**Out of scope:**
- project objects (evaluators, rules, alerts, connections, prices, prompts, datasets, queues, issues, dashboards, assistant), SQL runs, exports and MCP tool calls: M2;
- the `audit_log` feature, any read route or page, and the per-org `audit_retention_days` limit: M3;
- the stream: M4;
- SCIM: M5 and M6.

Their `target: "audit"` lines stay as they are until M2.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-EA-1 | Always record; recording is core (MIT), so `crates/server/src/audit/` has no SPDX header |
| D-EA-2 | Same transaction as the change; actions without a `meta.db` change record in their own transaction; a failed audit write fails the request; unauthenticated failures are tracing lines only; one record per bulk request |
| D-EA-3 | Ids only; IP and user agent (256 characters) kept |
| D-EA-4 | Typed catalog: one enum variant per action, dotted append-only names |
| D-EA-5 | Allow-listed details; no secrets, emails, names, payload, SQL text or URLs |
| D-EA-7 | `audit.retention_days`, default 365, min 30, 0 = forever; hourly purge; org drop deletes the org's rows |
| D-EA-8 | Rows with a null `org_id` form the instance log |
| D-EA-9 | Sign-ins are instance records; an org-provider sign-in also writes an org record |
| D-EA-20 | `seq INTEGER PRIMARY KEY AUTOINCREMENT` is the cursor; UUIDv7 `id` is the public id |
| D150 | Audit events carry ids, never emails, names or token values |
| D142 | Client IP rule |
| D-R-34 | Additive migrations only; a new value in a `CHECK`ed TEXT column is a format change, so the audit table has no `CHECK` |

**Plan-level choices** (detail the design leaves to the plan):
- **P-1. When the tracing line is emitted.** `audit::record` emits it right after the insert, inside the transaction. So every call site puts `audit::record` as the last statement before the success `Ok(...)`, after every check that can return `Err` and roll back (for example the last-owner checks in `routes/members.rs`). Only a failed `COMMIT` can then leave a line without a row. The design's "after commit" wording is amended in P6.
- **P-2. One catalog type per action group.** `audit::catalog` has one enum per name prefix (`auth`, `org`, `project`, `member`, `invite`, `api_key`, `pat`, `sso`, `user`, `instance`, `deletion`), each in its own file. M2 and M5 add files without touching these. `audit::Action` wraps them.
- **P-3. Deletions move into M1.** The design put deletions in M2. They are recorded here, because the org-drop deletion of audit rows lives in the same code. P6 updates the design's M1 and M2 lists.
- **P-4. Failed sign-in throttle.** Before inserting `auth.login_failed` for a user, the insert is skipped when a row with that action and target was written in the last 60 s. Rows carry no count. P6 amends the design's "with a count".
- **P-5. Lines that leave the audit target.** The tracing lines in the M1 files that are not catalog actions keep their message but drop `target: "audit"`: `deletion registered`, `deletion removed derived copies`, `init skipped: users exist` (already plain), and the org SSO `join skipped: seat limit` line. The rest of the codebase's audit lines are M2's.

## Versions

No new dependency. `serde_json` (`preserve_order`) and `uuid` (`v7`, `serde`) are already dependencies of `cetra-server`.

## Phase graph

```
A1 (core) ──┬── A2 (sign-in)
            ├── A3 (orgs, projects, members, invites, overrides)
            ├── A4 (credentials, SSO, instance, control plane)
            ├── A5 (deletions, retention, config)
            └── A6 (spec, design, changelog)   [after A2–A5]
```

A2, A3, A4 and A5 run in parallel after A1: their **Edit** lists do not overlap. A6 runs last.

**Shared rules for every phase** (also in each brief):
- Work in `/home/sebastien/travaux/llm-observer/.claude/worktrees/ea-design`, on the current branch. Never switch branches, stash or reset.
- Edit only the files the phase lists.
- **Do not compile, test, lint or format.** No `cargo`, `just` or `pnpm`. The coordinating session runs the checks once, when every phase is committed. Write the tests anyway.
- Commit only your own files: `git add <files> && git commit -m "<subject>" -- <files>`. If `index.lock` exists, wait 5 seconds and retry. Never `git add -A`.
- No `unwrap()` or `expect()` outside tests unless a comment states the invariant. No payload, email, name or secret in a record, a log or an error.
- **Id types.** `Event::org` and `Event::project` take `OrgId` / `ProjectId`. Catalog fields holding ids (`project_id`, `taken_from`, `to_org_id`, `follow_up_of`, `token_id`, `key_id`) are `uuid::Uuid`: convert typed ids with `.as_uuid()` and raw ids with `OrgId::from_uuid(..)` / `ProjectId::from_uuid(..)`. An optional project (`Option<ProjectId>`) is added with `if let Some(p) = .. { event = event.project(p) }`.
- When a deleted tracing line was the only use of a binding, parameter or import (for example `actor_id`, `caller`, `token_id`, `provider`), remove it too, so clippy's unused warnings do not fail. Never add `Extension(principal)` only to get the actor: `audit::Ctx` already reads the principal.

---

### A1 — The `audit_events` table and `audit::record`

**Goal:** the table, the core types, the M1 catalog, `record`, `record_alone`, the throttle and the `Ctx` extractor, with tests.

**Edit:**
- `crates/server/migrations/meta/0031_audit_events.sql` (new)
- `crates/server/src/audit/mod.rs` (new)
- `crates/server/src/audit/catalog/mod.rs` (new)
- `crates/server/src/audit/catalog/auth.rs`, `org.rs`, `project.rs`, `member.rs`, `invite.rs`, `api_key.rs`, `pat.rs`, `sso.rs`, `user.rs`, `instance.rs`, `deletion.rs` (new)
- `crates/server/src/audit/tests.rs` (new)
- `crates/server/src/lib.rs` (add `pub(crate) mod audit;`, keeping the module list's order)
- `crates/server/src/http/client_ip.rs` (make `client_ip` `pub(crate)`)
- `crates/server/src/auth/reset.rs` (add `reset_link_tx`, step 7)

**Steps:**

1. Write `0031_audit_events.sql` exactly:

   ```sql
   -- Audit records (track EA, D-EA-1 to D-EA-9, D-EA-20). No foreign keys: records
   -- outlive the users, orgs and projects they name. No CHECK: actions, kinds and
   -- outcomes are append-only names, and a CHECK would make each new name a format
   -- change (D-R-34).
   CREATE TABLE audit_events (
       seq INTEGER PRIMARY KEY AUTOINCREMENT,
       id TEXT NOT NULL UNIQUE,
       at INTEGER NOT NULL,
       org_id TEXT,
       project_id TEXT,
       action TEXT NOT NULL,
       outcome TEXT NOT NULL,
       actor_kind TEXT NOT NULL,
       actor_id TEXT,
       via TEXT,
       credential_id TEXT,
       target_kind TEXT,
       target_id TEXT,
       details TEXT NOT NULL DEFAULT '{}',
       ip TEXT,
       user_agent TEXT
   );
   CREATE INDEX audit_events_org ON audit_events (org_id, seq);
   CREATE INDEX audit_events_org_action ON audit_events (org_id, action, seq);
   CREATE INDEX audit_events_org_actor ON audit_events (org_id, actor_id, seq);
   CREATE INDEX audit_events_org_target ON audit_events (org_id, target_id, seq);
   CREATE INDEX audit_events_actor ON audit_events (actor_id);
   CREATE INDEX audit_events_at ON audit_events (at);
   ```

2. In `http/client_ip.rs`, change `fn client_ip(` to `pub(crate) fn client_ip(`. Nothing else changes.

3. `audit/mod.rs` starts with a module doc comment naming D-EA-1 to D-EA-9 and D-EA-20, then defines, exactly:

   ```rust
   pub(crate) mod catalog;
   #[cfg(test)]
   mod tests;

   pub(crate) use catalog::Action;

   /// User agents are cut to this many characters (as `web_sessions`).
   pub(crate) const USER_AGENT_MAX: usize = 256;
   /// A failed sign-in of one account is recorded at most once per this window (D-EA-2).
   pub(crate) const LOGIN_FAILED_WINDOW_NS: i64 = 60 * 1_000_000_000;

   /// How a user acted.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum Via {
       Session,
       Pat,
   }

   /// Who acted (D-EA-3).
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) enum Actor {
       /// A signed-in user. `credential_id` is the PAT id when `via` is `Pat`.
       User { id: uuid::Uuid, via: Via, credential_id: Option<uuid::Uuid> },
       /// A project API key.
       ApiKey { key_id: uuid::Uuid },
       /// The control plane, by its token id.
       ControlPlane { token_id: String },
       /// `cetra admin` on the server's host.
       Cli,
       /// The server itself: workers, startup, retention.
       System,
       /// No credential.
       Anonymous,
   }

   impl Actor {
       /// The `actor_kind` column.
       pub(crate) fn kind(&self) -> &'static str { /* user | api_key | control_plane | cli | system | anonymous */ }
       /// The `actor_id` column: the user id, key id or token id; `None` otherwise.
       pub(crate) fn id(&self) -> Option<String> { /* … */ }
       /// The `via` column: `session` or `pat` for a user; `None` otherwise.
       pub(crate) fn via(&self) -> Option<&'static str> { /* … */ }
       /// The `credential_id` column.
       pub(crate) fn credential_id(&self) -> Option<String> { /* … */ }
       /// The actor a principal stands for. `Session` → `User{via: Session}`,
       /// `Pat` → `User{via: Pat, credential_id: Some(token_id)}`, `ApiKey` → `ApiKey`,
       /// `ControlPlane` → `ControlPlane{token_id}`.
       pub(crate) fn of(principal: &crate::auth::principal::Principal) -> Self { /* … */ }
   }

   /// The request side of a record (D-EA-3).
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct Ctx {
       pub(crate) actor: Actor,
       pub(crate) ip: Option<std::net::IpAddr>,
       pub(crate) user_agent: Option<String>,
   }

   impl Ctx {
       /// No request: workers, startup and retention.
       pub(crate) fn system() -> Self { Self { actor: Actor::System, ip: None, user_agent: None } }
       /// `cetra admin` commands.
       pub(crate) fn cli() -> Self { Self { actor: Actor::Cli, ip: None, user_agent: None } }
       /// The same request with another actor: a sign-in names the user it signed in.
       pub(crate) fn with_actor(&self, actor: Actor) -> Self { /* clone, replace actor */ }
       /// A user acting through a session, from the given request values; `user_agent`
       /// is cut with [`truncate_user_agent`]. Used by `start_session`.
       pub(crate) fn for_session_user(id: uuid::Uuid, ip: Option<std::net::IpAddr>, user_agent: Option<&str>) -> Self { /* … */ }
   }

   /// Cuts a user agent to [`USER_AGENT_MAX`] characters, on a char boundary.
   pub(crate) fn truncate_user_agent(value: &str) -> String { /* value.chars().take(USER_AGENT_MAX).collect() */ }
   ```

   `Ctx` implements `axum::extract::FromRequestParts<S>` with the same bounds as `ClientIp` (`S: Send + Sync`, `AppState: FromRef<S>`), with `type Rejection = std::convert::Infallible`. It never fails:
   1. **Actor:** take the bare `Principal` extension when present, else the `Option<Principal>` extension that `authenticate` inserts when it is `Some`, and map it with `Actor::of`. Otherwise `Actor::Anonymous`.
   2. **IP:** when the `ConnectInfo<SocketAddr>` extension is present, `Some(client_ip(peer.ip(), &parts.headers, &state.config.trusted_proxies))`. Otherwise `None`.
   3. **User agent:** `parts.headers.get(USER_AGENT)`, `to_str().ok()`, `truncate_user_agent`.

4. Still in `audit/mod.rs`, define the event and target:

   ```rust
   /// `success` or `failure` (D-EA-4).
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum Outcome { Success, Failure }
   impl Outcome { pub(crate) fn as_str(self) -> &'static str { /* "success" | "failure" */ } }

   /// The `target_kind` column.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum TargetKind {
       User, Invite, ApiKey, Pat, Project, Org, SsoProvider, SsoDomain, Deletion, License,
   }
   impl TargetKind {
       /// user | invite | api_key | pat | project | org | sso_provider | sso_domain | deletion | license
       pub(crate) fn as_str(self) -> &'static str { /* … */ }
   }

   /// One record before it is written.
   #[derive(Debug, Clone, PartialEq)]
   pub(crate) struct Event {
       pub(crate) org_id: Option<cetra_core::ids::OrgId>,
       pub(crate) project_id: Option<cetra_core::ids::ProjectId>,
       pub(crate) target: Option<(TargetKind, String)>,
       pub(crate) action: Action,
       pub(crate) outcome: Outcome,
   }

   impl Event {
       /// An instance record (no org), success, no target.
       pub(crate) fn new(action: impl Into<Action>) -> Self { /* … */ }
       pub(crate) fn org(mut self, org: cetra_core::ids::OrgId) -> Self { /* sets org_id */ }
       pub(crate) fn project(mut self, project: cetra_core::ids::ProjectId) -> Self { /* sets project_id */ }
       pub(crate) fn target(mut self, kind: TargetKind, id: impl ToString) -> Self { /* … */ }
       pub(crate) fn failed(mut self) -> Self { /* outcome = Failure */ }
   }
   ```

   `org()` and `project()` take the typed ids. A call site holding a raw `uuid::Uuid` for an org or project wraps it with `OrgId::from_uuid(..)` / `ProjectId::from_uuid(..)`. The row stores `org.to_string()` (hyphenated lowercase).

5. Write `record` and `record_alone`:

   ```rust
   /// Writes `event` in the caller's transaction, then emits the `audit` tracing line.
   /// Call it last before the closure's success `Ok`, after every check that can roll
   /// back (plan P-1).
   ///
   /// # Errors
   /// The insert's [`MetaError`]; the caller's transaction then rolls back (D-EA-2).
   pub(crate) async fn record(
       conn: &mut sqlx::SqliteConnection,
       ctx: &Ctx,
       event: Event,
       now: i64,
   ) -> Result<(), crate::meta::MetaError>;

   /// [`record`] in a transaction of its own, for actions that change nothing in
   /// `meta.db` (sign-in failures, device denials, registrations).
   pub(crate) async fn record_alone(
       meta: &crate::meta::MetaDb,
       ctx: &Ctx,
       event: Event,
       now: i64,
   ) -> Result<(), crate::meta::MetaError>;

   /// `auth.login_failed` for a known account, skipped when one was recorded for
   /// `user_id` in the last [`LOGIN_FAILED_WINDOW_NS`] (plan P-4). Own transaction.
   pub(crate) async fn record_login_failed(
       meta: &crate::meta::MetaDb,
       ctx: &Ctx,
       user_id: uuid::Uuid,
       now: i64,
   ) -> Result<(), crate::meta::MetaError>;
   ```

   `record`:
   1. Serialize `event.action` with `serde_json::to_value`. The result is `{"action": "<name>", "details": {…}}`, or `{"action": "<name>"}` for a unit variant (`catalog` uses adjacent tagging). Take `name` from `action` and `details` from `details`, defaulting to `{}`. A serialization error maps to `MetaError::Corrupt { table: "audit_events", column: "details" }` (the variant at `meta/mod.rs:307`), never the value.
   2. `INSERT INTO audit_events (id, at, org_id, project_id, action, outcome, actor_kind, actor_id, via, credential_id, target_kind, target_id, details, ip, user_agent) VALUES (?, …)` with:
      - `uuid::Uuid::now_v7()` hyphenated;
      - `now`;
      - the ids hyphenated or NULL;
      - the actor columns from `Actor`;
      - `details` as a compact JSON string;
      - `ip` as `IpAddr::to_string()`.
   3. Emit:

      ```rust
      tracing::info!(target: "audit", action = name, org_id = …, project_id = …, actor_kind = …, actor_id = …, target_kind = …, target_id = …, outcome = …, details = %details_json, "{name}");
      ```

      Absent values print `-`. The line has no IP or user agent.

   `record_alone` is `meta.write(move |conn| Box::pin(async move { record(conn, &ctx, event, now).await }))`, cloning `ctx`.

   `record_login_failed` runs in one `meta.write`:
   1. `SELECT 1 FROM audit_events WHERE org_id IS NULL AND target_id = ? AND action = 'auth.login_failed' AND at >= ? LIMIT 1`, with `now - LOGIN_FAILED_WINDOW_NS`.
   2. When no row: `record` with `Event::new(AuthAction::LoginFailed).target(TargetKind::User, user_id).failed()` and `ctx`.

6. Write `audit/catalog/mod.rs`:

   ```rust
   mod api_key; mod auth; mod deletion; mod instance; mod invite; mod member; mod org; mod pat; mod project; mod sso; mod user;
   pub(crate) use api_key::ApiKeyAction; pub(crate) use auth::{AuthAction, LoginMethod, SignupMethod};
   pub(crate) use deletion::DeletionAction; pub(crate) use instance::InstanceAction;
   pub(crate) use invite::InviteAction; pub(crate) use member::{JoinVia, MemberAction};
   pub(crate) use org::OrgAction; pub(crate) use pat::PatAction; pub(crate) use project::ProjectAction;
   pub(crate) use sso::SsoAction; pub(crate) use user::UserAction;

   /// Every audit action (D-EA-4). Serializes as `{"action": "<group>.<name>", "details": {…}}`.
   #[derive(Debug, Clone, PartialEq, serde::Serialize)]
   #[serde(untagged)]
   pub(crate) enum Action {
       Auth(AuthAction), Org(OrgAction), Project(ProjectAction), Member(MemberAction),
       Invite(InviteAction), ApiKey(ApiKeyAction), Pat(PatAction), Sso(SsoAction),
       User(UserAction), Instance(InstanceAction), Deletion(DeletionAction),
   }
   ```

   Add one `impl From<XAction> for Action` per group, so `Event::new(MemberAction::Removed{…})` works.

   Each group file defines one enum with:

   ```rust
   #[derive(Debug, Clone, PartialEq, serde::Serialize)]
   #[serde(tag = "action", content = "details")]
   ```

   Each variant carries `#[serde(rename = "<group>.<snake_name>")]`. Struct variants have `#[serde(rename_all = "snake_case")]` fields.

   The full M1 catalog follows. Field types are exact. `uuid::Uuid` serializes hyphenated. Role and kind fields are `&'static str`, holding the stored spelling (`roles::org_role_str`, `roles::project_override_str`, and the `api_keys.kind` and scope spellings that `meta/api_keys.rs` writes).

   **`auth.rs` — `AuthAction`:**
   | Variant | Name | Fields | Scope |
   |---|---|---|---|
   | `Login { method: LoginMethod }` | `auth.login` | `LoginMethod` = `Password`, `Provider`, `OrgProvider` (serde `snake_case`) | instance; plus an org copy for `OrgProvider` |
   | `LoginFailed` | `auth.login_failed` | — | instance, outcome `failure` |
   | `Logout` | `auth.logout` | — | instance |
   | `Signup { method: SignupMethod }` | `auth.signup` | `SignupMethod` = `Password`, `Invitation`, `Provider`, `OrgProvider` | instance |
   | `EmailVerified` | `auth.email_verified` | — | instance |
   | `PasswordChanged` | `auth.password_changed` | — | instance |
   | `PasswordResetRequested` | `auth.password_reset_requested` | — | instance |
   | `PasswordReset` | `auth.password_reset` | — | instance |
   | `IdentityLinked { provider: String }` | `auth.identity_linked` | the identities `provider` string (`github`, `google`, `oidc:<name>`, `org:<uuid>`); never the subject | instance; org SSO links also get an org copy |
   | `SetupCompleted` | `auth.setup_completed` | — | instance |
   | `DeviceApproved { purpose: &'static str, token_id: uuid::Uuid, key_id: Option<uuid::Uuid> }` | `auth.device_approved` | `purpose`: `login` or `init` | instance |
   | `DeviceDenied` | `auth.device_denied` | — | instance |
   | `DeviceRegistered { purpose: &'static str }` | `auth.device_registered` | — | instance |

   **`org.rs` — `OrgAction`:**
   | Variant | Name | Fields |
   |---|---|---|
   | `Created` | `org.created` | — |
   | `Renamed` | `org.renamed` | — (names are never recorded) |
   | `EntitlementsUpdated { token_id: String }` | `org.entitlements_updated` | the org token's id |
   | `Dropped` | `org.dropped` | — (instance record, target `org`) |

   **`project.rs` — `ProjectAction`:**
   | Variant | Name | Fields |
   |---|---|---|
   | `Created` | `project.created` | — |
   | `Renamed` | `project.renamed` | — |
   | `RetentionChanged { old_days: Option<u32>, new_days: Option<u32> }` | `project.retention_changed` | |
   | `CompletionTimeoutChanged { old_seconds: Option<u32>, new_seconds: Option<u32> }` | `project.completion_timeout_changed` | use the existing field types if they differ, and say so in the report |
   | `Dropped` | `project.dropped` | — |

   **`member.rs` — `MemberAction`** (target `user`):
   | Variant | Name | Fields |
   |---|---|---|
   | `Added { role: &'static str, via: JoinVia }` | `member.added` | `JoinVia` = `SsoJit`, `AutoProvision` (serde `snake_case`) |
   | `RoleChanged { old_role: &'static str, new_role: &'static str }` | `member.role_changed` | |
   | `Removed { role: &'static str }` | `member.removed` | the removed member's role; self-removal shows as actor = target |
   | `OverrideSet { project_id: uuid::Uuid, old_role: Option<&'static str>, new_role: &'static str }` | `member.override_set` | |
   | `OverrideRemoved { project_id: uuid::Uuid, old_role: Option<&'static str> }` | `member.override_removed` | |

   **`invite.rs` — `InviteAction`** (target `invite`):
   | Variant | Name | Fields |
   |---|---|---|
   | `Created { role: &'static str }` | `invite.created` | never the email |
   | `Revoked` | `invite.revoked` | |
   | `Accepted { role: &'static str }` | `invite.accepted` | |

   **`api_key.rs` — `ApiKeyAction`** (target `api_key`, project-scoped):
   | Variant | Name | Fields |
   |---|---|---|
   | `Created { kind: &'static str, scopes: Vec<&'static str> }` | `api_key.created` | `secret`/`public`; scope names |
   | `Revoked` | `api_key.revoked` | |

   **`pat.rs` — `PatAction`** (target `pat`, instance record):
   | Variant | Name | Fields |
   |---|---|---|
   | `Created { scopes: Vec<&'static str>, expires_at: Option<i64> }` | `pat.created` | |
   | `Revoked` | `pat.revoked` | |

   **`sso.rs` — `SsoAction`** (org records):
   | Variant | Name | Fields |
   |---|---|---|
   | `ProviderSaved { issuer_changed: bool }` | `sso.provider_saved` | target `sso_provider` |
   | `ProviderRemoved` | `sso.provider_removed` | target `sso_provider` |
   | `SettingsChanged { enabled_before: bool, enabled: bool, enforced_before: bool, enforced: bool, revoked_users: u64 }` | `sso.settings_changed` | |
   | `DomainAdded` | `sso.domain_added` | target `sso_domain` (id only; never the domain text) |
   | `DomainVerified { taken_from: Option<uuid::Uuid> }` | `sso.domain_verified` | |
   | `DomainLost { to_org_id: uuid::Uuid }` | `sso.domain_lost` | written in the losing org's log |
   | `DomainRemoved` | `sso.domain_removed` | |
   | `DomainLapsed` | `sso.domain_lapsed` | actor `system` |
   | `Disabled` | `sso.disabled` | the break-glass command, actor `cli` |

   **`user.rs` — `UserAction`** (target `user`, instance records):
   | Variant | Name | Fields |
   |---|---|---|
   | `Created { instance_admin: bool }` | `user.created` | instance-admin API |
   | `Updated { disabled: Option<bool>, instance_admin: Option<bool> }` | `user.updated` | the requested changes |
   | `ResetLinkIssued` | `user.reset_link_issued` | |
   | `SessionsRevoked` | `user.sessions_revoked` | control plane |
   | `Disabled` | `user.disabled` | control plane |
   | `Deleted { sole_owner_orgs: u32 }` | `user.deleted` | control plane |

   **`instance.rs` — `InstanceAction`:**
   | Variant | Name | Fields |
   |---|---|---|
   | `InitApplied` | `instance.init_applied` | actor `system` |
   | `LicenseInstalled { license_id: String, kid: String }` | `instance.license_installed` | target `license`; never the key |

   **`deletion.rs` — `DeletionAction`** (target `deletion`, org records):
   | Variant | Name | Fields |
   |---|---|---|
   | `Requested { kind: &'static str, follow_up_of: Option<uuid::Uuid> }` | `deletion.requested` | `kind` from `crate::meta::deletions::DeletionKind::as_str()` |
   | `Applied { kind: &'static str }` | `deletion.applied` | trace, filter and user deletions; drops record `project.dropped` / `org.dropped` instead |
   | `Failed { kind: &'static str }` | `deletion.failed` | never the reason text |

7. `auth/reset.rs`: split `reset_link` so A2 and A4 can record in the same transaction:

   ```rust
   /// Stores `minted`'s reset token for `user_id` in the caller's transaction.
   pub(crate) async fn reset_link_tx(
       conn: &mut sqlx::SqliteConnection,
       minted: &MintedReset,
       user_id: Uuid,
       now: i64,
   ) -> Result<(), MetaError> {
       insert_reset(conn, &minted.hash, user_id, now, minted.expires_at).await
   }
   ```

   `reset_link` keeps its signature and calls `reset_link_tx` inside its `meta.write`. It needs a clone of `minted` (or of its hash and expiry) to move into the closure, and still returns `minted.url`. Check that `MintedReset`'s fields are `hash`, `expires_at` and `url`, and adjust the body to the real names.

8. `audit/tests.rs` (`#[tokio::test]` with `crate::meta::test_db()`, or `TestApp` where a request is needed):
   - `record_writes_one_row_with_every_column`: a user actor with a PAT, org, project, target, details. The row has the expected columns, `details` parses to the variant's fields, `seq` is 1 and `id` parses as a UUIDv7.
   - `a_unit_variant_stores_empty_details`: `AuthAction::Logout` → `details = '{}'`.
   - `record_in_a_rolled_back_transaction_leaves_no_row`: `record` then `Err(MetaError::Conflict("x"))` from the closure → zero rows.
   - `record_alone_commits`.
   - `seq_follows_commit_order`: three `record_alone` calls give `seq` 1, 2, 3 whatever their `id`s.
   - `every_action_name_is_group_dot_snake`: a `samples()` fn in `tests.rs` builds one value of **every** variant above. Each name matches `^[a-z_]+\.[a-z_]+$`, and its prefix equals the group (`Action::Auth` → `auth.`, `Action::ApiKey` → `api_key.`, …).
   - `action_names_are_unique`: over `samples()`.
   - `details_hold_no_forbidden_field`: for every sample, every key of `details` (recursively) is refused when it equals or ends with `email`, `password`, `secret`, `token`, `name`, `url`, `sql`, `prompt`, `subject` or `domain`. `token_id` and other `_id` suffixes are allowed.
   - `login_failed_is_recorded_once_a_minute`: two calls 10 s apart → one row; a third 61 s after the first → two rows.
   - `ctx_reads_the_principal_ip_and_user_agent`: a handler-less test through `Ctx::from_request_parts` on built `Parts`:
     - a session principal → `User{via: Session}`;
     - a 300-character UA → 256 characters;
     - a `ConnectInfo` with peer 10.0.0.1 and `trusted_proxies = []` → `Some(10.0.0.1)`;
     - no `ConnectInfo` → `None`;
     - no principal → `Anonymous`.
   - `actor_of_maps_every_principal`: Session, Pat (credential id), ApiKey, ControlPlane.
   - `the_tracing_line_names_the_action_and_no_ip`: `capture_logs` around `record` (current-thread runtime). The output contains `audit`, `member.role_changed` and the actor id, and not the IP or user agent.

**Done when:** committed. The coordinating session compiles and tests.

**Commit:** `Add audit_events and audit::record`

---

### A2 — Sign-in records

**Goal:** record every sign-in, sign-up, password and device action of the auth area.

**Edit:** `crates/server/src/auth/session.rs`, `crates/server/src/routes/auth.rs`, `crates/server/src/routes/me.rs`, `crates/server/src/routes/oauth.rs`, `crates/server/src/auth/oidc/flow.rs`, `crates/server/src/routes/device.rs`, `crates/server/src/bootstrap.rs`.

**Rules for every site:**
- Add a `ctx: audit::Ctx` extractor argument to each handler that records. It never fails. Put it before any body extractor.
- Inside the `state.meta.write` closure that makes the change, call `audit::record(conn, &ctx, event, now).await?` as the last statement before the success `Ok(...)`. Clone `ctx` into the closure (`let ctx = ctx.clone();` before `move`).
- Where a closure returns a helper's value directly (`insert_x(conn, …).await`), bind it with `let value = …?;`, record, then `Ok(value)`.
- Delete the replaced `tracing::info!(target: "audit", …)` call.
- Never record on an early `Ok(None)` or `Ok(Missing/Forbidden/…)` branch.

**Steps:**

1. **`auth/session.rs` `start_session`**: in its closure, after `insert_session(…)` succeeds, record `AuthAction::Login { method }`:
   - `method`: `Password` → `LoginMethod::Password`, `Provider` → `Provider`, `OrgProvider(org)` → `OrgProvider`;
   - ctx: `Ctx::for_session_user(user_id, ip, user_agent)` (the function's own parameters);
   - for `OrgProvider(org)`, record a second event: the same action with `.org(org)`.

   Return the closure's previous value. Every caller (login, signup, setup, OAuth callback, org SSO) then records a login with no change on their side.
2. **`routes/auth.rs` `login`:** delete the `login` tracing line; `start_session` records it.
3. **`routes/auth.rs` `audit_login_failed`:**
   - Change it to `async fn audit_login_failed(state: &AppState, ctx: &audit::Ctx, user: Option<Uuid>)`.
   - `None` keeps today's `tracing::info!(target: "audit", account = "unknown", "login failed")` exactly, so no row is written for an unknown email.
   - `Some(id)` calls `audit::record_login_failed(&state.meta, ctx, id, now)` (`ctx` is anonymous: the login is not authenticated). On `Err`, log `tracing::error!(error = %error, "audit write failed")` and keep answering the login's 401: a failed login must not turn into a 500. This is the one place where the audit error is logged instead of failing the request, because the request already fails.
   - Thread `ctx` from `login`'s new `ctx` argument.
4. **`routes/auth.rs` `reset_password`:**
   - Inside closure 384-395, before `Ok(Some((id, hashes)))`, record `AuthAction::PasswordReset` with target `(User, id)`.
   - The ctx is `ctx.with_actor(Actor::User { id, via: Via::Session, credential_id: None })`: the user proved the token.
5. **`routes/auth.rs` `setup`:**
   - `bootstrap::provision` must take the record. Add a parameter `ctx: Option<audit::Ctx>` to `bootstrap::create_setup` and `provision`.
   - Inside `provision`'s closure, after `insert_first`, record:
     - for setup (`Some(ctx)`): `AuthAction::SetupCompleted` with `.org(org).project(project)`, ctx `ctx.with_actor(User{id: user, via: Session})`;
     - for `apply_init` (`None`): `InstanceAction::InitApplied` with `.org(org).project(project)`, ctx `Ctx::system()`.
   - Delete `bootstrap::audit` and its call, and the setup handler's tracing lines and the `key_id` binding that only fed them (`routes/auth.rs:~177`).
6. **`routes/auth.rs` `logout`:** in the closure at 328, after `delete_session(…)`, record `AuthAction::Logout` with ctx `ctx.with_actor(User{id: row.user.id, via: Session, credential_id: None})`. Only inside the `if let Some((row, _))` branch.
7. **`routes/auth.rs` `signup`:**
   - plain branch (closure 701-717): bind `insert_user`'s row, record `AuthAction::Signup { method: Password }`, target `(User, user.id)`, actor the new user;
   - invited branch (closure 635-678): before `Ok(InvitedSignup::User(user))`, record `Signup { method: Invitation }` and `InviteAction::Accepted { role }` (`.org(pending.org_id)`, target `(Invite, pending.id)`), actor the new user.
8. **`routes/auth.rs` `verify_email`:** before `Ok(Some(()))` (501), record `AuthAction::EmailVerified`, target `(User, id)`, actor the user.
9. **`routes/auth.rs` `forgot_password`:**
   - Replace its `reset_link(&state.meta, …)` call: `let minted = auth::reset::mint_reset(&public_url, now).map_err(auth_error)?;` (`mint_reset` returns `AuthError`; `auth_error` is already in this file), and drop `reset_link` from the `use` of line 18 (keep `ResetError` and `verification_link`). then one `state.meta.write` that calls `reset_link_tx(conn, &minted, user.id, now)` (A1) and records `AuthAction::PasswordResetRequested`, target `(User, user.id)`, with `ctx` as is (the requester is anonymous). The mail uses `minted.url`.
   - Only the branch that runs today records. An unknown email still does nothing, and the response stays identical.
   - `resend_verification` is unchanged (not in the catalog).
10. **`routes/me.rs` `change_password`:** in closure 138-144, bind the `revoke_user_tokens` result, record `AuthAction::PasswordChanged` (target `(User, user_id)`), return it.
11. **`routes/oauth.rs` `callback`:** delete both `login` tracing lines; `start_session` records. Keep `provider` unused only if the compiler allows; otherwise drop the binding.
12. **`auth/oidc/flow.rs`:**
    - Add a `ctx: &audit::Ctx` parameter to `finish`, `resolve`, `admit`, `decide` and `provision_org`, and thread it down: `finish` → `resolve` (both `provision_org` calls at ~264 and ~293) and `finish` → `admit` → `decide` (clone into `admit`'s closure). Pass it from `routes/oauth.rs` `callback` (add the `ctx` extractor there). `callback` also calls `crate::ee::org_sso::login::finish` (line 96): pass `&ctx` as its new last argument too. A4 adds that parameter. Update any test in these two files that calls a changed function.
    - In `decide`, which runs on the closure's `conn`, record before returning each outcome:
      - `Outcome::Setup(id)`: `SetupCompleted`, actor the new user;
      - `Outcome::Linked{user_id, ..}`: `IdentityLinked{provider}`, actor the user;
      - `Outcome::User(id)` (new user): `Signup{method: Provider}`, actor the new user.
    - In `provision_org`'s closure, record `MemberAction::Added { role, via: AutoProvision }`, `.org(org)`, target `(User, user)`, with `ctx.with_actor(Actor::System)`, only on the arm where `add_org_member` returned `Ok(())`, never on `Err(Conflict)`.
    - Delete the two tracing lines at 431 and 438.
13. **`routes/device.rs`:**
    - `approve`: pass `ctx` into `mint`. In each closure, before its success value, record `AuthAction::DeviceApproved { purpose: "login" | "init", token_id, key_id }`. The init branch adds `.org(org).project(project)`.
    - The approval record commits with the minted token, before `state.device.complete`. A stale code then leaves a `device_approved` row for a token that was minted but never handed out. That matches the token row, and is intended.
    - `deny`: `audit::record_alone(&state.meta, &ctx, Event::new(AuthAction::DeviceDenied), now)`.
    - `register_code`: add `ctx`; record `DeviceRegistered { purpose }` with `record_alone`.
    - Delete the four tracing lines.
14. **Tests**, updated or added in the same files (every new test reads `audit_events` with `sqlx::query_as` on `app.state.meta.reader()`):
    - `routes/auth.rs`:
      - the login test at ~1640-1676: replace `logs.contains("login")` with an assert of one `auth.login` row whose actor is the user, `via = 'session'`, and `ip` is the request peer.
      - the failed login test: the unknown email gives zero `auth.login_failed` rows, and the line `login failed` with `unknown` stays; a bad password gives one row with outcome `failure`.
      - New: `logout_is_recorded`, `password_signup_is_recorded`, `invitation_signup_records_signup_and_invite`, `email_verification_is_recorded`, `forgot_records_only_known_accounts`, `password_reset_is_recorded`.
      - Keep every `assert_logs_clean` test as is.
    - `routes/me.rs`: `password_change_is_recorded`.
    - `routes/oauth.rs`:
      - at 1330, assert an `auth.setup_completed` row instead of the text;
      - at 1025, an `auth.identity_linked` row with `provider` in details, and still no email in the logs.
    - `bootstrap.rs`:
      - at 462/475/564, assert one `instance.init_applied` row, and none after the second prepare;
      - keep `init skipped: users exist` as is;
      - `read_key_id` is no longer in the line: assert the row's project instead;
      - keep the no-plaintext asserts.
    - `routes/device.rs`: `approval_is_recorded_with_token_and_key_ids`, `denial_is_recorded`.
    - `auth/session.rs`: `an_org_provider_session_writes_two_login_rows` (instance and org).

**Done when:** committed.

**Commit:** `Record sign-in, signup, password and device actions`

---

### A3 — Orgs, projects, members, invites and overrides

**Goal:** record org and project creation and renames, membership changes, invitations and project overrides.

**Edit:** `crates/server/src/routes/orgs.rs`, `crates/server/src/routes/projects.rs`, `crates/server/src/routes/members.rs`, `crates/server/src/routes/invites.rs`, `crates/server/src/ee/project_roles.rs`, `crates/server/src/meta/members.rs`.

The same rules as A2 apply to every site.

**Steps:**

1. `routes/orgs.rs`:
   - `create_org`: before `Ok(org)`, record `OrgAction::Created`, `.org(org.id)`, target `(Org, org.id)`.
   - `update_org`: add `ctx`. Turn the one-liner closure into a block: rename, then record `OrgAction::Renamed`, `.org(id)`, target `(Org, id)`, then return the rename's value.
2. `routes/projects.rs`:
   - `create_project` (add `ctx`): before `Ok(ProjectCreate::Created(project))`, record `ProjectAction::Created`, `.org(org).project(project.id)`, target `(Project, project.id)`.
   - `update_project`: before the closure, compute from `access.project` and the request:
     - `name_changed = name.as_ref().is_some_and(|n| *n != access.project.name)`, on the trimmed `name` the handler already builds from `body.name`;
     - the retention change. The request's retention is `Option<Option<u32>>`: outer `None` = unchanged, `Some(None)` = clear, `Some(Some(d))` = set. A change exists when the outer value is `Some(new)` and `new != access.project.retention_days`. Then `old_days = access.project.retention_days`, `new_days = new`.
     - the timeout change, the same way against `access.project.trace_completion_timeout_s` (the stored `Option<u32>`, not `completion_timeout_s()`).
     - `actor_id` becomes unused: keep `principal::session_user(&principal)?;` as the session check and drop the binding.

     Inside the closure, after `projects::update_project(…)`, record, each with `.org(…).project(…)` and target `(Project, id)`:
     - `ProjectAction::Renamed` when the name changed (compare with `access.project.name`);
     - `RetentionChanged{old_days, new_days}` when retention changed;
     - `CompletionTimeoutChanged{…}` when the timeout changed.

     Delete the two tracing lines after the write.
3. `routes/members.rs`:
   - `update_org_member`: in the closure, after the last-owner check and before `Ok(RoleChange::Done)`, record `MemberAction::RoleChanged { old_role: org_role_str(target_role), new_role: org_role_str(role) }`, `.org(org_id)`, target `(User, target)`.
   - `remove_org_member`: after the last-owner check, before `Ok(MemberChange::Done)`, record `MemberAction::Removed { role: org_role_str(target_role) }`.
   - Delete both tracing lines.
4. `routes/invites.rs`:
   - `create_invite`: before `Ok(Created::Invite(row))`, record `InviteAction::Created { role }`, target `(Invite, row.id)`.
   - `accept_invite`: before the `Ok(Accepted::Done{…})` of the branch that added the membership, record `InviteAction::Accepted { role }`. The branch where the user was already a member records nothing.
   - `delete_invite`: add `ctx` (it gives the actor), make the closure a block, and record `InviteAction::Revoked` only when `removed` is true.
   - Delete the two tracing lines.
5. `meta/members.rs`: add, next to `project_override`:

   ```rust
   /// The override of `user` on `project`, read on the writer connection.
   pub(crate) async fn project_override_tx(
       conn: &mut SqliteConnection,
       project: ProjectId,
       user: Uuid,
   ) -> Result<Option<ProjectOverride>, MetaError>
   ```

   Same query as `project_override`, on `&mut *conn`. Make `remove_project_override` return `Result<bool, MetaError>`: true when a row was deleted (`rows_affected() > 0`). Update its callers in this phase's files.
6. `ee/project_roles.rs` (keep the SPDX header):
   - `set_project_override`: add `ctx`. In the closure, read `old = project_override_tx(…)` before `set_project_override`, then record `MemberAction::OverrideSet { project_id, old_role: old.map(project_override_str), new_role: project_override_str(new) }`, `.org(org_id).project(project_id)`, target `(User, target)`.
   - `remove_project_override`: read `old` first, delete, and record `OverrideRemoved { project_id, old_role }` only when the delete removed a row.
7. **Tests:**
   - `routes/members.rs:562`: assert a `member.role_changed` row with `old_role`/`new_role`, keeping the no-email asserts. New: `removal_is_recorded_with_the_role`, `a_refused_last_owner_removal_records_nothing`.
   - `routes/orgs.rs:350`: assert an `org.created` row; keep the no-name/slug/email asserts. New: `rename_is_recorded_without_names` (details is `{}`).
   - `routes/projects.rs`:
     - 897-898 and 1053-1055: assert `project.completion_timeout_changed` and `project.retention_changed` rows with old and new values;
     - 910 and 1067: a name-only PATCH writes `project.renamed` and neither of those.
     - New: `project_create_is_recorded`, `an_over_limit_create_records_nothing`.
   - `routes/invites.rs`: `invite_create_accept_and_revoke_are_recorded`, `revoking_a_missing_invite_records_nothing`.
   - `ee/project_roles.rs`: `override_set_records_old_and_new`, `removing_no_override_records_nothing`.

**Done when:** committed.

**Commit:** `Record org, project, member, invite and override changes`

---

### A4 — Credentials, SSO, instance and control plane

**Goal:** record keys and personal access tokens, org SSO, the instance-admin user actions, the licence, entitlements tokens and control-plane calls, and clear IP and user agent when a user is deleted.

**Edit:** `crates/server/src/routes/api_keys.rs`, `crates/server/src/routes/tokens.rs`, `crates/server/src/ee/org_sso/settings.rs`, `crates/server/src/ee/org_sso/login.rs`, `crates/server/src/ee/org_sso/recheck.rs`, `crates/server/src/admin.rs`, `crates/server/src/routes/admin.rs`, `crates/server/src/routes/internal_orgs.rs`, `crates/server/src/routes/internal_users.rs`, `crates/server/src/routes/entitlements.rs`.

The same rules as A2 apply to every site. `ee/` files keep their SPDX header.

**Steps:**

1. `routes/api_keys.rs`:
   - `create_api_key`: bind the inserted row, then record `ApiKeyAction::Created { kind, scopes }`, `.org(org).project(project)`, target `(ApiKey, row.id)`.
   - `revoke_api_key`: record `Revoked` only when the helper returned `Some`.
2. `routes/tokens.rs`:
   - `create_token`: record `PatAction::Created { scopes, expires_at }`, target `(Pat, row.id)`.
   - `revoke_owned`: add a `ctx: &audit::Ctx` parameter and record `PatAction::Revoked` when `Some`. Add a `ctx: audit::Ctx` extractor to its two callers, `revoke_token` (~130) and `revoke_current` (~140), and pass `&ctx`.
3. `ee/org_sso/settings.rs`, on the success branch of each closure:
   - `put_org_sso_provider`: `ProviderSaved { issuer_changed: renames_issuer }`, target `(SsoProvider, id)`, on `Ok(Save::Saved)` only.
   - `delete_org_sso_provider`: `ProviderRemoved`.
   - `patch_org_sso`: `SettingsChanged { enabled_before, enabled, enforced_before, enforced, revoked_users }`, from the row read before `set_flags` and the request.
   - `post_org_sso_domain`: `DomainAdded`, target `(SsoDomain, id)`.
   - `post_org_sso_domain_verify`: `DomainVerified { taken_from }` in this org. When `taken_from` is `Some(holder)`, also `DomainLost { to_org_id: org }` with `.org(holder)`. The re-verify branch (528) records `DomainVerified { taken_from: None }` as today.
   - `delete_org_sso_domain`: make the closure a block; `DomainRemoved` when it returned true.
   - Delete the seven tracing lines.
4. `ee/org_sso/login.rs` `finish`:
   - Add a parameter `ctx: &audit::Ctx` to `finish` and pass it into `admit`'s closure (clone). Its only caller, `routes/oauth.rs:96` `callback`, is A2's file: A2 passes `&ctx` there. The two phases agree on this signature, and the code compiles once both are committed.
   - `admit` (`login.rs:~370`) is a free function called as `admit(conn, &input)` inside `finish`'s write closure. Add a `ctx: &audit::Ctx` parameter (clone `ctx` into the closure). Record only before its final `Ok(Admitted::User{…})`, never on the early return for the instance-admin test sign-in. Update the test calls of `finish` in this file (`~604`, `~1015`, `~1190`) with a ctx (`audit::Ctx::system()`). Record with `.org(org)` and the signed-in user as actor (`ctx.with_actor(Actor::User{id: user_id, via: Session, credential_id: None})`):
     - `AuthAction::IdentityLinked { provider }` when `linked` (the org's identity provider string);
     - `MemberAction::Added { role, via: SsoJit }` when `joined`;
     - `AuthAction::Signup { method: OrgProvider }` when the user was created here (the `Target::New` branch).
   - Make the `org sso join skipped: seat limit` line a plain `tracing::info!` without the audit target (plan P-5).
   - Remove the fields `linked` and `joined` from `Admitted::User`, which only fed the deleted lines, from every construction of it (the instance-admin early return included), and from the tests that read them. Keep `no_seat`.
5. `ee/org_sso/recheck.rs`: inside the closure, after `mark_unverified`, record `SsoAction::DomainLapsed`, `.org(row.org_id)`, target `(SsoDomain, id)`, `Ctx::system()`. Delete the `warn!` audit line. Keep a plain `tracing::warn!` with org and domain ids.
6. `admin.rs` `org_sso_disable_in`: in the closure, when `set_flags` returned true, record `SsoAction::Disabled`, `.org(id)`, `Ctx::cli()`. Delete the tracing line.
7. `routes/admin.rs`:
   - `create_user`: record `UserAction::Created { instance_admin }`, target `(User, user.id)`.
   - `update_user`: record `UserAction::Updated { disabled, instance_admin }` (the request's options) before returning the row, only when the user exists.
   - `reset_link`: replace the `reset::reset_link(&state.meta, …)` call with `let minted = reset::mint_reset(&public_url, now).map_err(auth_failure)?;`, and map the write's `MetaError` with `?`. Delete `reset_failure` (~522) and the `ResetError` import (~21), which become unused. `admin.rs:248` (the CLI) keeps calling `reset::reset_link`. then one `state.meta.write` that calls `reset::reset_link_tx(conn, &minted, id, now)` (A1) and records `UserAction::ResetLinkIssued`, target `(User, id)`. The response uses `minted.url`.
   - `put_license`: add `ctx`. Copy `license.id` and `license.kid` into `String`s before the closure (both `key` and `license` move later). In the closure, record `InstanceAction::LicenseInstalled { license_id, kid }`, target `(License, license_id)`. Delete the plain info line.
8. `routes/internal_orgs.rs`:
   - `create_org`: before `Ok(Outcome::Created(…))`, record `OrgAction::Created`, `.org(id)`, target `(Org, id)`. The ctx is the extractor's: the control-plane principal gives `Actor::ControlPlane`.
   - `drop_org`: add a `ctx: audit::Ctx` extractor and pass `&ctx` as the new last argument of `crate::deletion::request_org_drop` (A5 adds that parameter). Keep the `(ActorKind, Uuid)` tuple argument as it is.
9. `routes/internal_users.rs`:
   - `revoke_access`: add `ctx: &audit::Ctx`. Before `Ok(hashes)`, record `UserAction::Disabled` when `disable`, else `SessionsRevoked`, target `(User, user)`.
   - Delete `Matched` and `Matched::as_str` (~50-63), which only fed the deleted lines. `managed_user` returns the `Uuid`. Update `revoke`, `disable` and `delete`. Keep each handler's `let Principal::ControlPlane { .. } = principal else { .. }` guard, even when `token_id` is no longer used (`{ .. }`).
   - `delete`: before `DELETE FROM users`, run `UPDATE audit_events SET ip = NULL, user_agent = NULL WHERE actor_kind = 'user' AND actor_id = ?`. After it, record `UserAction::Deleted { sole_owner_orgs }`, target `(User, user)`.
   - Delete the three tracing lines.
10. `routes/entitlements.rs` `put_org_token`: record `OrgAction::EntitlementsUpdated { token_id }`, `.org(org)`, inside the closure when `store_token` returned `Stored`. The ctx is `ctx.with_actor(Actor::ControlPlane { token_id: grant.token_id.to_string() })` from a new `ctx` extractor (the route is public: the token is the credential). `store_token` is a free function; record in the closure after it returns `Stored`.
11. **Tests:**
    - `routes/internal_orgs.rs:317`: assert an `org.created` row with `actor_kind = 'control_plane'`.
    - `routes/internal_users.rs:620, 884`: assert a `user.deleted` row. New `deleting_a_user_clears_their_ip`: a user signs in (row with IP), is deleted; their `auth.login` row keeps its id, and `ip` is NULL.
    - `routes/entitlements.rs:776`: assert an `org.entitlements_updated` row; keep the no-`cetra1.` assert.
    - New: `api_key_create_and_revoke_are_recorded` (api_keys.rs), `pat_create_and_revoke_are_recorded` (tokens.rs), `domain_takeover_writes_both_orgs` (settings.rs), `lapse_is_recorded_by_system` (recheck.rs), `licence_install_is_recorded_without_the_key` (routes/admin.rs), `break_glass_is_recorded_by_cli` (admin.rs).

**Done when:** committed.

**Commit:** `Record credential, SSO, instance and control-plane actions`

---

### A5 — Deletions, retention and configuration

**Goal:** record deletion requests and outcomes, purge old records hourly, and delete an org's records when its drop is applied.

**Edit:** `crates/server/src/deletion/mod.rs`, `crates/server/src/deletion/resolve.rs`, `crates/server/src/routes/deletions.rs`, `crates/server/src/serve.rs`, `crates/server/src/config.rs`, `crates/server/src/test_support/mod.rs` (the two `ServerConfig` literals only), `crates/server/src/ingest/fixtures_tests.rs` (its `ServerConfig` literal only), `website/src/content/docs/docs/self-hosting/configuration-reference.md`, `crates/server/src/audit/retention.rs` (new), and one line in `crates/server/src/audit/mod.rs` (`pub(crate) mod retention;`).

**Steps:**

1. `config.rs`: add

   ```rust
   /// `[audit]` (track EA, D-EA-7).
   #[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
   #[serde(deny_unknown_fields, default)]
   pub struct AuditConfig {
       /// Days an audit record is kept. Default 365; 0 keeps records forever; otherwise 30 or more.
       pub retention_days: u32,
   }
   impl Default for AuditConfig { fn default() -> Self { Self { retention_days: 365 } } }
   ```

   - Add the field `#[serde(default)] pub audit: AuditConfig,` on `ServerConfig`, next to `eval`.
   - Add `audit: AuditConfig::default(),` after `eval: …,` in the three `ServerConfig` struct literals without `..`: `test_support/mod.rs:1286` (`default_config`), `test_support/mod.rs:1841` (`with_init`) and `ingest/fixtures_tests.rs:464`. Also search `crates/server/src` for any other `ServerConfig {` literal and fix it the same way.
   - In `validate`: `if config.audit.retention_days != 0 && config.audit.retention_days < 30 { return Err(ConfigError::Invalid { key: "audit.retention_days", reason: "must be 0 or at least 30" }) }`.
   - Tests:
     - `audit_retention_defaults_to_365`;
     - `audit_retention_refuses_29` (`[audit]\nretention_days = 29\n` → `Invalid { key: "audit.retention_days", .. }`);
     - `audit_retention_accepts_0_and_30`.
2. `configuration-reference.md`: add after the `[eval]` section:

   ```
   ## `[audit]`

   | Key | Environment | Type | Default | Description |
   |---|---|---|---|---|
   | `audit.retention_days` | | integer | `365` | Days an audit record is kept, then deleted by an hourly task. `0` keeps records forever; otherwise 30 or more. |
   ```
3. `audit/retention.rs` (core, no SPDX):

   ```rust
   /// Rows deleted per statement, so the writer is never held long.
   pub(crate) const PURGE_BATCH: i64 = 5_000;

   /// Deletes records older than `retention_days` (0 = keep all), in batches; returns the count.
   pub(crate) async fn purge(meta: &crate::meta::MetaDb, retention_days: u32, now: i64) -> Result<u64, crate::meta::MetaError>;

   /// Deletes every record of `org` (an applied org drop, D-EA-7).
   pub(crate) async fn delete_org_records(conn: &mut sqlx::SqliteConnection, org: uuid::Uuid) -> Result<u64, crate::meta::MetaError>;
   ```

   `purge`:
   1. With `retention_days = 0`, return 0.
   2. Otherwise `cutoff = now - retention_days as i64 * 86_400 * 1_000_000_000`, saturating.
   3. Loop: one `meta.write` per batch runs `DELETE FROM audit_events WHERE seq IN (SELECT seq FROM audit_events WHERE at < ? ORDER BY seq LIMIT ?)`. Sum `rows_affected`, and stop when a batch deletes fewer than `PURGE_BATCH`.

   Tests in the same file:
   - `purge_keeps_recent_and_removes_old` (3 old, 2 recent);
   - `purge_with_zero_keeps_everything`;
   - `purge_spans_batches`: 12,001 old rows inserted in one transaction → 12,001 deleted;
   - `delete_org_records_leaves_other_orgs_and_the_instance_log`.
4. `serve.rs`:
   - Add `/// How often old audit records are deleted (D-EA-7).` `const AUDIT_PURGE_EVERY: Duration = Duration::from_secs(60 * 60);`.
   - Add `fn spawn_audit_purge(state: AppState) -> JoinHandle<()>`, shaped exactly like `spawn_purge`, calling `crate::audit::retention::purge(&state.meta, state.config.audit.retention_days, now)`. It logs `tracing::info!(removed, "purged old audit records")` when `removed > 0`, and `tracing::error!(error = %error, "audit purge failed")` on error.
   - Spawn it inside the existing `if !state.config.restore_mode { … }` block: restore mode keeps a restored copy unchanged.
   - In `restore_mode_skips_the_sending_and_dns_loops` (`serve.rs:~1205`), the assertion `normal_tasks - restore_tasks == 4` becomes `== 5`, with its comment naming the audit purge.
5. `deletion/mod.rs`. Each record uses `Ctx::system()` unless an actor is given:
   - `request_org_drop`: keep the `actor: (ActorKind, Uuid)` parameter (it still fills the deletion row) and add `ctx: &audit::Ctx` as the last parameter. In the closure, before `Ok(Some(row))`, record `DeletionAction::Requested { kind: "org", follow_up_of: None }`, `.org(org)`, target `(Deletion, row.id)`, using `ctx` as is: its actor comes from the request's principal, so there is no mapping from `ActorKind`.
     - Callers: `routes/deletions.rs` `drop_org` (this phase) passes `&ctx`. `routes/internal_orgs.rs` `drop_org` is A4's file: A4 adds its `ctx` extractor and passes `&ctx`. The two phases agree on this signature.
   - `follow_up`: the follow-up row is a trace-ids deletion. Copy the org and the project into locals before the closure (it moves `row`), then record `Requested { kind: DeletionKind::TraceIds.as_str(), follow_up_of: Some(parent) }`, `.org(org)` (and the project when `Some`), target `(Deletion, id)` (the new row's id), `Ctx::system()`. Remove the `actor_kind`/`actor_id` locals that only fed the deleted line.
   - `applied_now` and `fail`: copy `row.kind.as_str()`, `row.org_id`, `row.project_id` and `row.id` before the closure. Record `Applied { kind }` (resp. `Failed { kind }`) when `mark_applied` (resp. `mark_failed`) returned true, with `.org(org)`, the project when `Some`, and target `(Deletion, id)`, using `Ctx::system()`.
   - `drop_project_step`: in the closure, write `let dropped = mark_dropped(conn, id, now).await?; if dropped { audit::record(conn, &Ctx::system(), Event::new(ProjectAction::Dropped).org(..).project(..).target(TargetKind::Project, project), now).await?; } Ok(dropped)`. No `deletion.applied` for a drop.
   - `drop_org_step`: in the closure, after `delete_org`, write `let dropped = mark_dropped(..).await?; if dropped { delete_org_records(conn, org.as_uuid()).await?; audit::record(conn, &Ctx::system(), Event::new(OrgAction::Dropped).target(TargetKind::Org, org), now).await?; } Ok(dropped)`. There is **no** `.org(…)`: it is an instance record (D-EA-7). No `deletion.applied` either: the org's records are gone.
   - The outer `let dropped = …` bindings after both writes (~605, ~709) become unused once `on_dropped` goes: drop the binding and keep `.await.map_err(..)?;`.
   - Delete `on_dropped` and the `Dropped` struct (`deletion/mod.rs:~196`), and their two calls (in `drop_project_step` and `drop_org_step`). If a test uses `Dropped`, rewrite it to assert the rows.
   - Delete these sites' tracing lines.
   - `deletion registered` (540) and `deletion/resolve.rs:161` drop `target: "audit"` (plan P-5).
6. `routes/deletions.rs`:
   - `drop_project` and `create_deletion`: add `ctx`. Before the success value, record `DeletionAction::Requested { kind, follow_up_of: None }`, `.org(org).project(project)`, target `(Deletion, row.id)`.
   - `drop_org`: pass `&ctx` to `request_org_drop`.
   - Delete the tracing lines.
7. **Tests:**
   - `routes/deletions.rs:1263, 1750`: assert a `deletion.requested` row; keep the no-scope and no-slug asserts on the logs and on `details`.
   - `deletion/mod.rs`:
     - 1507-1512: one `project.dropped` row after one run, still one after the second;
     - 1680-1684: the `deletion registered` line is a plain info line (no `audit` target); keep the no-scope assert;
     - 2494 unchanged.
     - New: `an_applied_org_drop_deletes_its_records_and_records_the_drop` (the org's rows are gone, and an instance row `org.dropped` targets the org).

**Done when:** committed.

**Commit:** `Record deletions, purge old audit records and drop an org's records`

---

### A6 — Specs, design notes and changelog

**Goal:** the docs match M1.

**Edit:** `docs/spec/04-metadata-db.md`, `docs/spec/08-auth.md`, `docs/spec/12-operations.md`, `docs/plans/ea-enterprise-admin/README.md`, `CHANGELOG.md`.

**Steps:**
1. **Spec 04:** add § Audit records (track EA) after § Per-org SSO. It holds the `audit_events` DDL from A1 step 1, one sentence per column from the design's record table, the "no foreign keys, no CHECK" rule, and the purge and org-drop rules.
2. **Spec 08:**
   - Replace § Auth events log with § Audit records: every access and membership action writes an `audit_events` row in the same transaction (D-EA-2), and emits the `audit` tracing line named after the action. The unknown-email and bad-token rule, and the one-per-minute failed sign-in, are stated. It links to the design for the catalog.
   - Keep D150's sentence.
3. **Spec 12:** in § Configuration, add `audit.retention_days` with its default and bounds, and the hourly purge, which is off in restore mode.
4. **Design `README.md`:**
   - § Writing: "After commit, the same function emits…" becomes "As its last step inside the transaction, the same function emits…, so call sites record after every check that can roll back".
   - The failed sign-in bullet: "at most one row per account and minute, with a count" becomes "at most one row per account and minute".
   - § Milestones: move "deletions" from M2's catalog list to M1's.
   - D-EA-2's row: "the `tracing` line follows the commit" becomes "the `tracing` line follows the insert, as the transaction's last step".
   - Mirror the D-EA-2 change in `docs/mvp-decisions.md`? No: that file is not in this phase's list. Report it, and the coordinating session edits it.
5. **`CHANGELOG.md` § `[Unreleased]` → `Added`:** "Audit records: sign-ins, membership, invitation, key, token, SSO, licence and deletion actions are recorded in `meta.db` with the actor, IP and user agent, and kept for `audit.retention_days` (default 365). Reading them comes with the enterprise audit log."

**Done when:** committed.

**Commit:** `Document audit recording`

---

## After the phases (coordinating session)

1. Review each commit's diff against its phase.
2. Run once: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH; just check-rust`, then `just check-docs`. Expected rework:
   - compile errors from parallel phases;
   - the `config::tests::the_configuration_reference_matches_the_loader` row;
   - `crates/server/tests/snapshots/meta_schema.snap` (`schema_matches_the_snapshot`, `meta/mod.rs:~694`): regenerate with `cargo insta accept`, or by the test's documented bless path, and review that only `audit_events` and its 6 indexes were added;
   - golden server dumps (`golden_tests/dump.rs:92`, `golden_tests/mod.rs:191`), which dump every table and now contain `audit_events` rows with UUIDv7 ids and times: make them deterministic, or exclude the table from the dump, and say which in the commit.
3. A Haiku fix phase for what fails, with an exact brief.
4. A Sonnet code review of M1. Its findings are fixed the same way.
5. Update the roadmap row: "M1 implemented".
