# P · Licence renewal — the store link and installing a key without a restart

Track Q's requests R26 and R27 (cetra-control `docs/00-design.md`), both "nice to have", approved by the user on 2026-11-21. Self-hosted admins renew a licence in one click and without a restart: the licence card links to the licence store, and an instance admin pastes a new key on the card, which applies at once. This plan builds on track P's M1–M5 (merged).

**Prerequisites:** `main` at or after `ca104c7` (Release 0.1.0-rc.1). Nothing else.

**Read first, in this order:**
1. `AGENTS.md`.
2. `docs/mvp-decisions.md` § Track P: D-P-7, D-P-8, D-P-10, D-P-15, D-P-50, **D-P-74, D-P-75** (this plan's decisions).
3. `docs/spec/12-operations.md` § Licence key and § Entitlements; `docs/spec/09-api.md` (the admin row, line 74); `docs/spec/11-ui.md` (Instance admin bullet, line 245); `docs/spec/04-*.md` § `instance_settings` (around line 994).
4. The code: `crates/server/src/ee/license.rs` (whole file), `crates/server/src/entitlements/mod.rs` (`EntitlementsState`, `log_license`, the tests from `license_features_apply_to_every_org` on), `crates/server/src/ee/sso_enforcement.rs`, `crates/server/src/meta/instance_settings.rs`, `crates/server/src/routes/admin.rs` (`routes`, `get_license`, `update_user`, the license tests from line ~1010), `crates/server/src/api/admin.rs` (`LicenseStatus`, `AdminLicense`), `crates/server/src/serve.rs` (around line 414), `crates/server/src/admin.rs` (`entitlements_in`, test `entitlements_show_reads_the_token`), `crates/server/src/test_support/mod.rs` (around line 355, `license_key`, `license_key_until`, `assert_logs_clean`), `crates/server/src/api/openapi.rs` (`get_admin_license` and the admin paths), `crates/server/tests/openapi.rs`, `web/src/ee/license-card.tsx`, `web/src/ee/org-sso/provider-section.tsx` (a `SettingsFormCard` with a mutation), `web/src/routes/admin/settings.test.tsx`.
5. `docs/plans/p-entitlements/03-m3-units-and-limits.md` § "Every new or changed API operation" (the eight steps).

## What it is, and what it is not

When it is done:
- `GET /api/v1/admin/license` carries `renew_url` (`https://app.cetra.sh/licenses` unless `entitlements.upgrade_url` is set) and `source` (`config` or `stored`; omitted without a key) (D-P-74, D-P-75);
- `PUT /api/v1/admin/license` with `{"key": "cetra1.…"}` installs a licence key: stored in `meta.db` (`instance_settings['license.key']`), applied at once to every entitlement check, SSO enforcement included (D-P-75);
- startup and `cetra admin entitlements show` read the configured key, else the stored key;
- the admin licence card links to the store, says where the key comes from, and has an install form unless the key is set in the configuration;
- specs 04, 09, 11, 12, the site's "Plans and licence" page, the configuration reference, `CHANGELOG.md` and the roadmap's track P row say so.

Out of scope:
- re-reading `CETRA_LICENSE_KEY` or the config file on SIGHUP (rejected, D-P-75);
- removing an installed key (another key replaces it, D-P-75);
- a CLI command to install a key;
- org Upgrade buttons: unchanged (D-P-15);
- anything in cetra-control.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-P-7 | Licence key: active, grace, expired; a refused key is logged and the server runs without a licence |
| D-P-8 | Lapses never widen access |
| D-P-10 | Resolution in memory at every check: defaults or org token, plus the licence's features |
| D-P-15 | Org Upgrade button to `entitlements.upgrade_url`; no URL, no button |
| D-P-74 | `renew_url` = `LICENSE_STORE_URL` unless `upgrade_url` is set; the card links to it |
| D-P-75 | `PUT /api/v1/admin/license`; plain text in `instance_settings['license.key']`; config key wins (409); refused or expired key 400; live through `EntitlementsState`; no SIGHUP, no removal, no migration |

User's answers (2026-11-21): R27 is the paste-on-the-card approach only, no SIGHUP; the stored key is plain text, not sealed.

## Versions

No new dependency (Rust, web or docs). `tokio` (already a `cetra-server` dependency) provides `tokio::sync::Mutex`.

## Conventions

- One commit per phase, with the subject given. Server code returns `ApiError`. Never log or return the key or the customer name; log the licence id, the key id (`kid`) and the status.
- Server tests use `TestApp` (manual clock `2026-01-01T00:00:00Z`). Web tests use Vitest + Testing Library on MSW with `server.use`, never `vi.mock`.
- **Generated files** (`crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json`) are regenerated with `just openapi` and `just api-examples`, never edited by hand.
- **Dead code between phases.** P1 adds `EntitlementsState::set_license`, `EntitlementsState::lock_license_writes` and the field `InstanceLicense::source`, whose first non-test reader arrives in P2. Give each `#[cfg_attr(not(test), allow(dead_code))]` with the comment `// Licence renewal: P2 uses it.`; P2 removes all three. (`LICENSE_STORE_URL` is added by P2, not P1.)
- **Machine load.** Other agents run on this machine. While iterating, run only the touched tests (`cargo test -p cetra-server <filter>`, `cd web && pnpm vitest run <file> --maxWorkers=8`). Do not run `just check` or the full test suites in a phase: the coordinator runs `just check` once at the end.
- Before `pnpm` or `just openapi`: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`.

## Phases

| # | Subject | Edits | After |
|---|---|---|---|
| P1 | `Hold the licence live and read an installed key` | server: licence loading and `EntitlementsState` | — |
| P2 | `Install a licence key from the admin API` | server: the PUT route, `AdminLicense` fields, OpenAPI, examples | P1 |
| P3 | `Link to the licence store and install keys on the licence card` | `web/src/ee/license-card.tsx`, its tests | P2 (needs the regenerated `schema.gen.ts`) |
| P4 | `Document installing a licence key` | specs, site, changelog, roadmap | P2 |

P1 → P2 in order (same files). P3 and P4 touch disjoint files and may run in parallel after P2.

---

## P1 — Hold the licence live and read an installed key

**Goal:** the instance licence comes from the configured key, else the key stored in `instance_settings`, and `EntitlementsState` holds it behind a lock so it can be replaced at runtime.

**Edit:**
- `crates/server/src/meta/instance_settings.rs`
- `crates/server/src/ee/license.rs`
- `crates/server/src/entitlements/mod.rs`
- `crates/server/src/ee/sso_enforcement.rs`
- `crates/server/src/routes/admin.rs` (only `get_license`'s first line, see step 6)
- `crates/server/src/serve.rs`
- `crates/server/src/admin.rs`
- `crates/server/src/test_support/mod.rs`

**Steps:**

1. `meta/instance_settings.rs`: under `ENCRYPTION_CHECK`, add

   ```rust
   /// Key of the licence key an instance admin installed (D-P-75). A signed token, stored in plain text.
   pub(crate) const LICENSE_KEY: &str = "license.key";
   ```

2. `ee/license.rs`: change the doc of `LicenseState` from "The configured licence, read once at startup." to "A licence key, verified." Add, after `LicenseStatus`:

   ```rust
   /// Where the instance's licence key comes from (D-P-75).
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum LicenseSource {
       /// `license_key` (`CETRA_LICENSE_KEY`). Wins over an installed key.
       Config,
       /// Installed by an instance admin, in `instance_settings['license.key']`.
       Stored,
       /// No key anywhere.
       Missing,
   }

   /// The instance licence and where its key comes from.
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub(crate) struct InstanceLicense {
       /// The verified key.
       pub(crate) state: LicenseState,
       /// Where it comes from.
       #[cfg_attr(not(test), allow(dead_code))] // Licence renewal: P2 uses it.
       pub(crate) source: LicenseSource,
   }

   /// The licence in force: `configured` when set, else the installed key, else none (D-P-75).
   ///
   /// # Errors
   ///
   /// Returns [`MetaError::Db`] when reading the installed key fails.
   pub(crate) async fn load_license(
       pool: &sqlx::SqlitePool,
       configured: Option<&str>,
   ) -> Result<InstanceLicense, crate::meta::MetaError>
   ```

   Body, in order:
   1. `configured` is `Some(key)` → `InstanceLicense { state: LicenseState::from_key(Some(key)), source: LicenseSource::Config }`; the stored key is not read.
   2. Else read `crate::meta::instance_settings::get_setting(pool, LICENSE_KEY)`. `Some(key)` → `{ state: LicenseState::from_key(Some(&key)), source: Stored }` (a stored key that no longer verifies, e.g. a retired signing key, is `Invalid` with source `Stored`).
   3. Else `{ state: LicenseState::Absent, source: LicenseSource::Missing }`.

3. `entitlements/mod.rs`, `EntitlementsState`:
   - Replace the field `license: LicenseState` with

     ```rust
     /// The instance licence; replaced when an admin installs a key (D-P-75).
     license: RwLock<Arc<InstanceLicense>>,
     /// Serializes licence installs: the stored key and the held licence change together.
     license_writes: tokio::sync::Mutex<()>,
     ```

     (`RwLock` is the `std::sync::RwLock` already imported for `orgs`.)
   - `load` takes `license: InstanceLicense` instead of `LicenseState`; both `Ok(Self { … })` build `license: RwLock::new(Arc::new(license))` and `license_writes: tokio::sync::Mutex::new(())`.
   - `pub(crate) fn license(&self) -> Arc<InstanceLicense>`: a clone of the held `Arc`, read with `.read().unwrap_or_else(PoisonError::into_inner)` as `grant` does. Doc: "The instance licence now."
   - New, with the dead-code attribute of § Conventions:

     ```rust
     /// Replaces the instance licence. Every later check sees it (D-P-75).
     pub(crate) fn set_license(&self, license: InstanceLicense)

     /// Held while a licence key is stored and set, so two installs cannot interleave.
     pub(crate) async fn lock_license_writes(&self) -> tokio::sync::MutexGuard<'_, ()>
     ```

     `set_license` writes `Arc::new(license)` under `.write().unwrap_or_else(PoisonError::into_inner)`.
   - `resolve`: `let licensed = self.license().state.active_features(now);`.
   - Update the `use crate::ee::license::…` line to import `InstanceLicense` (and `LicenseSource` in tests where needed).
   - `log_license` keeps its signature (`&LicenseState`).

4. `ee/sso_enforcement.rs` `applies`: `.license().state.listed_features()`. Update the module doc's "the configured key" to "the instance's key (configured or installed)".

5. `serve.rs` (around line 414): replace `LicenseState::from_key(…)` with

   ```rust
   let license = match crate::ee::license::load_license(
       meta.reader(),
       config.license_key.as_ref().map(|key| key.expose().as_str()),
   )
   .await
   {
       Ok(license) => license,
       Err(error) => {
           release_storage(storage).await;
           meta.close().await;
           return Err(ServeError::Meta(error));
       }
   };
   crate::entitlements::log_license(&license.state, Clock::System.now_ns());
   ```

   and pass `license` to `EntitlementsState::load`.

6. `routes/admin.rs` `get_license`: `let license = state.entitlements.license();` stays; the `match license.status(…)` becomes `license.state.status(…)` and `match license {` becomes `match &license.state {`. No other change in this phase.

7. `admin.rs` `entitlements_in`: replace the `LicenseState::from_key(…)` line with `let license = load_license(meta.reader(), config.license_key.as_ref().map(|key| key.expose().as_str())).await.map_err(map_meta)?;` (import `load_license` from `crate::ee::license`; drop the `LicenseState` import if it becomes unused).

8. `test_support/mod.rs` (around line 355): same as step 7 with `.await.expect("licence loads")`.

9. Fix the two test calls of `EntitlementsState::load` in `entitlements/mod.rs` (around lines 830 and 874): pass `InstanceLicense { state: LicenseState::Absent, source: LicenseSource::Missing }`. Fix the tests that call `.license()` (around lines 650 and 663) to go through `.state`.

**Tests:**
- `ee::license::tests`:
  - `load_license_prefers_the_configured_key`: a `crate::meta::test_db()`, store `license_key(&["sso_enforcement"])` under `LICENSE_KEY` with `put_setting` in `db.write`; `load_license(db.reader(), Some(&license_key(&["project_roles"])))` → source `Config`, `listed_features()` contains `ProjectRoles` and not `SsoEnforcement`.
  - `load_license_reads_the_installed_key`: same store, `configured = None` → source `Stored`, state `Valid`, lists `SsoEnforcement`.
  - `load_license_without_a_key_is_absent`: empty db, `None` → `Absent`, source `Missing`.
  - `a_refused_installed_key_is_invalid`: store `"garbage"`, `None` → `Invalid(_)`, source `Stored`.
- `entitlements::tests::set_license_applies_at_once`: `TestApp::new()`; `of(&app.state, OrgId::new_v7())` does not allow `ProjectRoles`; `app.state.entitlements.set_license(InstanceLicense { state: LicenseState::from_key(Some(&license_key(&["project_roles"]))), source: LicenseSource::Stored })`; the same `of` call now allows it.
- `ee::sso_enforcement::tests::an_installed_licence_turns_enforcement_on`: `TestApp::with_config(sso_config)`; `applies(&app.state)` is false; `set_license` with a key listing `sso_enforcement`; `applies` is true.
- `admin::tests::entitlements_show_reads_the_installed_licence`: as `entitlements_show_reads_the_token` (`ready_config`, `MetaDb::open_existing`, `seed_org`), store `license_key(&["project_roles"])` under `LICENSE_KEY`, close; `entitlements(&config, "acme")` lists `Feature::ProjectRoles`.
- Every existing test still passes.

**Done when:**
- `cargo test -p cetra-server ee::license`, `cargo test -p cetra-server entitlements::`, `cargo test -p cetra-server sso_enforcement`, `cargo test -p cetra-server admin::tests::entitlements_show`, `cargo test -p cetra-server routes::admin::tests::license` pass;
- `cargo clippy -p cetra-server --all-targets -- -D warnings` passes;
- `cargo fmt --check -p cetra-server` passes.

**Commit:** `Hold the licence live and read an installed key`

---

## P2 — Install a licence key from the admin API

**Goal:** `PUT /api/v1/admin/license` installs a key, and `GET` reports its source and the store link.

**Edit:**
- `crates/server/src/api/admin.rs`
- `crates/server/src/routes/admin.rs`
- `crates/server/src/entitlements/mod.rs` (remove the two dead-code attributes only)
- `crates/server/src/api/openapi.rs`
- `crates/server/tests/openapi.rs`
- `fixtures/api/static/admin-license.json`
- generated: `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `fixtures/api/manifest.json` (if `just api-examples` changes it)

**Steps:**

0. `ee/license.rs`, after `LicenseStatus`:

   ```rust
   /// Where self-hosted admins buy and renew licences (D-P-74). The only copy of this URL.
   pub(crate) const LICENSE_STORE_URL: &str = "https://app.cetra.sh/licenses";
   ```

1. `api/admin.rs`, after `LicenseStatus`:

   ```rust
   /// Where the licence key comes from (D-P-75).
   #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
   #[serde(rename_all = "snake_case")]
   pub enum LicenseKeySource {
       /// `license_key` in the configuration or `CETRA_LICENSE_KEY`. It wins over an installed key, and the API cannot change it.
       Config,
       /// Installed with `PUT /api/v1/admin/license`.
       Stored,
   }
   ```

   Add to `AdminLicense`, after `grace_ends_at`:

   ```rust
   /// Where the key comes from. Omitted with `none`.
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub source: Option<LicenseKeySource>,
   /// Where to buy or renew a licence (D-P-74). Omitted when `entitlements.upgrade_url` is set.
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub renew_url: Option<String>,
   ```

   And the request, with a `Debug` that hides the key (write it exactly):

   ```rust
   /// `PUT /api/v1/admin/license`.
   #[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
   pub struct InstallLicenseRequest {
       /// The licence key (`cetra1.…`). Surrounding whitespace is ignored.
       pub key: String,
   }

   impl std::fmt::Debug for InstallLicenseRequest {
       fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
           f.debug_struct("InstallLicenseRequest")
               .field("key", &"<redacted>")
               .finish()
       }
   }
   ```

   Fix the doc of `LicenseStatus::None` to "No key is configured or installed." and of `Invalid` to "The key was refused."

2. `routes/admin.rs`:
   - Imports: add `put` to the `axum::routing::{get, patch, post}` import; `InstallLicenseRequest` and `LicenseKeySource` from `crate::api::admin`; `InstanceLicense`, `LicenseSource` and `LICENSE_STORE_URL` next to `LicenseState` from `crate::ee::license`; `LICENSE_KEY` and `put_setting` from `crate::meta::instance_settings`; `FieldError` if not already imported.
   - Move `get_license`'s body into `fn admin_license(state: &AppState) -> AdminLicense`. It also sets `source` with an explicit `match license.source { LicenseSource::Config => Some(LicenseKeySource::Config), LicenseSource::Stored => Some(LicenseKeySource::Stored), LicenseSource::Missing => None }` and `renew_url` (`state.config.entitlements.upgrade_url.is_none().then(|| LICENSE_STORE_URL.to_owned())`) on every variant, `none` included. `get_license` returns `error::Json(admin_license(&state))`.
   - Add a `RouteDef` after the GET one: `method: PUT`, `path: "/api/v1/admin/license"`, `access: Access::InstanceAdmin`, `documented: true`, `handler: put(put_license)` (import `axum::routing::put` next to `get`/`post`).
   - `async fn put_license(State(state), error::Json(body): error::Json<InstallLicenseRequest>) -> Result<error::Json<AdminLicense>, ApiError>`, in this order:
     1. `state.config.license_key.is_some()` → `Err(ApiError::Conflict { detail: "the licence key is set in the configuration" })`.
     2. `let key = body.key.trim();` empty → `ApiError::BadRequest { detail: Some("enter a licence key".to_owned()), errors: vec![FieldError { field: "key".to_owned(), message: "enter a licence key".to_owned(), position: None }] }`.
     3. `match LicenseState::from_key(Some(key))`: `Valid(license)` goes on; `Invalid(reason)` → `BadRequest` with detail `format!("the licence key was refused: {reason}")` and a `key` field error whose message is `reason.to_string()`; `Absent` (unreachable with `Some`) → the same 400 with reason text `"no key"`. The detail never contains the key.
     4. `state.clock.now_ns() >= license.grace_ends_at` → `BadRequest` with detail `"the licence has expired"` and a `key` field error with the same message.
     5. `let _guard = state.entitlements.lock_license_writes().await;`
     6. `state.meta.write(move |conn| Box::pin(async move { put_setting(conn, LICENSE_KEY, &key).await })).await?` with `key` an owned `String` (`From<MetaError> for ApiError` maps the error).
     7. `tracing::info!(license_id = %license.id, kid = %license.kid, "licence key installed");`
     8. `state.entitlements.set_license(InstanceLicense { state: LicenseState::Valid(license), source: LicenseSource::Stored });`
     9. Drop the guard; return `error::Json(admin_license(&state))` (status 200).
3. Remove the three `cfg_attr(…dead_code)` attributes and their comments from P1 (two in `entitlements/mod.rs`, one on `InstanceLicense::source` in `ee/license.rs`).
4. The eight steps of M3 § "Every new or changed API operation" for `put_admin_license`:
   - `openapi.rs`: `let install = operation("put_admin_license", "admin", "Install a licence key").securities(session.clone()).request_body(Some(json_body("InstallLicenseRequest"))).responses(responses("200", json_of::<AdminLicense>("The licence now in force. The key itself is never returned."), &[400, 401, 403, 409, 429])).build();` (before `test_email`, which takes `session` by value); the path item becomes `let mut license_item = PathItem::new(HttpMethod::Get, license); license_item.put = Some(install);`.
   - `collect_schemas`: `InstallLicenseRequest`, `LicenseKeySource`.
   - `tests/openapi.rs`, exactly these three edits: the `OPERATIONS` tuple `("put", "/api/v1/admin/license", "put_admin_license")` next to the `get_admin_license` one (near line 41); the security arm: add `| "put_admin_license"` after `"get_admin_license"` (near line 1259); the expected-errors branch: change the `post_admin_user` condition (near line 1367, which already expects `["400", "401", "403", "409", "429"]`) to `*id == "post_admin_user" || *id == "put_admin_license"`. Do not touch `QUERY_OR_BODY`.
   - No API example for the PUT. `fixtures/api/static/admin-license.json`: add `"source": "config"` and `"renew_url": "https://app.cetra.sh/licenses"` after `grace_ends_at`.
   - `just openapi`, then `just api-examples`, each twice with no change the second time.
5. Update the existing license tests in `routes/admin.rs` whose expected JSON is exact: `license_none_without_a_key` (add `renew_url` only), `license_active_lists_the_claims` and `license_invalid_never_echoes_the_key` (add `"source": "config"` and `"renew_url": "https://app.cetra.sh/licenses"`). `license_grace_and_expired_keep_the_listed_features` checks single fields and needs no change.
6. Add `PUT /api/v1/admin/license` (with body `{"key": "x"}`) and `GET /api/v1/admin/license` to the non-admin 403 table near line 585 of the `routes::admin` tests. The access suite (`http/access_suite.rs`) sends `{}` to the new route; that answers 4xx before any install, so it needs no change.

**Tests** (`routes::admin::tests`; use `admin_with`, `license`, `license_key`, `license_key_until`, `assert_logs_clean`; the manual clock is 2026-01-01):
- `install_license_needs_an_instance_admin`: a non-admin session gets 403, as `license_needs_an_instance_admin` does for GET; nothing is stored.
- `install_license_applies_without_a_restart`: `admin_with(|_| {})`; PUT `{"key": license_key(&["project_roles"])}` → 200 with `status: "active"`, `source: "stored"`, `features: ["project_roles"]`; then `GET` gives the same body; `crate::entitlements::of(&app.state, OrgId::new_v7()).allows(Feature::ProjectRoles)`.
- `install_license_stores_the_key`: after the PUT, `get_setting(app.state.meta.reader(), LICENSE_KEY)` is the key, and `load_license(app.state.meta.reader(), None)` has source `Stored`.
- `install_license_trims_the_key`: the key with a leading space and a trailing newline → 200; the stored value has no whitespace.
- `install_license_refuses_an_empty_key`: `"  "` → 400 with a `key` field error; nothing stored.
- `install_license_refuses_a_bad_key`: `"garbage"` → 400, detail starts with `the licence key was refused`, body does not contain `garbage`; status stays `none`.
- `install_license_refuses_an_expired_key`: `license_key_until(&["project_roles"], "2025-11-01T00:00:00Z", "2025-12-01T00:00:00Z")` → 400 `the licence has expired`; nothing stored.
- `install_license_accepts_a_key_in_grace`: `license_key_until(&["project_roles"], "2025-12-01T00:00:00Z", "2026-02-01T00:00:00Z")` → 200, `status: "grace"`.
- `install_license_replaces_the_installed_key`: install a key listing `project_roles`, then one listing `sso_enforcement` → `features: ["sso_enforcement"]`; `of(…)` no longer allows `ProjectRoles`.
- `install_license_conflicts_with_the_configured_key`: `admin_with` setting `config.license_key` → PUT answers 409; GET still shows `source: "config"`; nothing stored.
- `install_license_turns_sso_enforcement_on`: `admin_with` setting `config.auth.enforce_sso_domains = vec!["sso.example".to_owned()]`; `GET /api/v1/admin/settings` has `enforce_sso_applied: false`; PUT a key listing `sso_enforcement`; the same GET has `enforce_sso_applied: true`.
- `install_license_never_logs_the_key`: wrap the PUT in `assert_logs_clean(&[key.as_str(), "Test Customer"], …)`.
- `license_omits_the_store_link_with_an_upgrade_url`: `admin_with` setting `config.entitlements.upgrade_url = Some("https://up.example/{org}".to_owned())` → GET has no `renew_url`.
- `install_license_serializes_concurrent_installs`: `tokio::join!` two PUTs, one listing `project_roles`, one listing `sso_enforcement`; both 200; then `load_license(meta.reader(), None)`'s listed features equal `app.state.entitlements.license().state.listed_features()`.
- `install_license_request_debug_hides_the_key` (in `api::admin` tests or `routes::admin::tests`): `format!("{:?}", InstallLicenseRequest { key: "cetra1.secret".to_owned() })` does not contain `cetra1.secret`.

**Done when:**
- `cargo test -p cetra-server routes::admin`, `cargo test -p cetra-server --test openapi`, `cargo test -p cetra-server --test api_examples` and `cargo test -p cetra-server --lib api_examples::examples_are_up_to_date` pass;
- `just openapi` and `just api-examples` leave no change when run again;
- `cargo clippy -p cetra-server --all-targets -- -D warnings` and `cargo fmt --check -p cetra-server` pass;
- `grep -rn "app.cetra.sh/licenses" crates/` finds only `LICENSE_STORE_URL` and tests.

**Commit:** `Install a licence key from the admin API`

---

## P3 — Link to the licence store and install keys on the licence card

**Goal:** the admin licence card links to `renew_url`, says where the key comes from, and installs a pasted key.

**Edit:**
- `web/src/ee/license-card.tsx`
- `web/src/routes/admin/settings.test.tsx`

**Steps:**

1. In `LicenseCard`, inside the existing `SettingsCard`:
   - Replace the paragraph "Set the key with CETRA_LICENSE_KEY and restart the server." with the source line: `source === "config"` → "Set by CETRA_LICENSE_KEY or license_key in the configuration. Change it there and restart the server."; `source === "stored"` → "Installed from this page."; no `source` → nothing.
   - When `data.renew_url` is set, a paragraph after the `<dl>` with `<a href={data.renew_url} target="_blank" rel="noreferrer" className="underline">Buy or renew a licence</a>`. The URL is never written in `web/src`: it comes from the API.
2. After the `SettingsCard`, when `data.source !== "config"`, render a new `InstallLicenseForm` component (same file, not exported), modelled on `ProviderSection` in `web/src/ee/org-sso/provider-section.tsx`:
   - `SettingsFormCard` with `title="Install a licence key"`, `description="Paste the key. It applies at once, without a restart."`, `saveLabel="Install"`, `dirty={key !== ""}`, `invalid={key.trim() === ""}`, `pending`, `saved`, `error={install.error ? problemMessage(install.error, "Could not install the licence key.") : undefined}`.
   - One field: `<Label htmlFor="license-key">Licence key</Label>` and `<Textarea id="license-key" value={key} onChange=… rows={4} className="font-mono" spellCheck={false} autoComplete="off" />` (`@/components/ui/textarea`, `@/components/ui/label`).
   - `const install = $api.useMutation("put", "/api/v1/admin/license");`. `onSubmit`: `setSaved(false)`, `install.reset()`, return when `key.trim() === ""`, then `install.mutate({ body: { key: key.trim() } }, { onSuccess })`.
   - `onSuccess(answer)`: `setKey("")`, `setSaved(true)`, `queryClient.setQueryData(adminLicenseQuery.queryKey, answer)`, `void queryClient.invalidateQueries({ queryKey: adminSettingsQuery.queryKey })` (`enforce_sso_applied` depends on the licence), and `void queryClient.invalidateQueries({ predicate: (query) => query.queryKey[1] === "/api/v1/orgs/{org}/entitlements" })` (every org's plan).
3. `statusLabel`'s `none` text becomes "No licence key. Enterprise features are locked." (unchanged) — keep the existing status strings exactly; the existing tests check them.

**Tests** (`settings.test.tsx`; the default MSW mock is `fixtures/api/static/admin-license.json`, now with `source: "config"` and `renew_url`):
- `links to the licence store`: the default mock → a link named "Buy or renew a licence" with `href` `https://app.cetra.sh/licenses`.
- `has no store link without renew_url`: GET answers `{ status: "none", features: [] }` → no such link.
- `says the key comes from the configuration`: default mock → the configuration sentence is shown and there is no "Licence key" textbox.
- `installs a licence key`: GET answers `{ status: "none", features: [], renew_url: … }`; PUT answers an active body with `source: "stored"` and records the request body; type `"  cetra1.abc\n"` into "Licence key", click "Install" → the PUT body is `{ key: "cetra1.abc" }`; "Active until 2099-01-01", "Installed from this page." and the form's "Saved" status appear (`SettingsFormCard` shows "Saved" only when `saved` and not dirty; `setKey("")` makes it not dirty).
- `shows why installing a key was refused`: PUT answers `problemResponse(400, "bad_request", { detail: "the licence key was refused: the token is malformed" })` (the helper from `@/test/problem` the file already imports) → that detail is shown.
- `cannot install an empty key`: the Install button is disabled while the field is blank.
- Every existing test in the file still passes.

**Done when:** `cd web && pnpm vitest run src/routes/admin/settings.test.tsx --maxWorkers=8`, `cd web && pnpm typecheck`, and `cd web && pnpm exec biome check src/ee/license-card.tsx src/routes/admin/settings.test.tsx` pass. `grep -rn "app.cetra.sh" web/src` finds only the test file.

**Commit:** `Link to the licence store and install keys on the licence card`

---

## P4 — Document installing a licence key

**Goal:** the specs, the site, the changelog and the roadmap describe D-P-74 and D-P-75.

**Edit:**
- `docs/spec/04-*.md` (the `instance_settings` block, around line 994)
- `docs/spec/09-api.md` (the admin row, line 74)
- `docs/spec/11-ui.md` (Instance admin bullet, line 245)
- `docs/spec/12-operations.md` (the `license_key` row, line 25, and § Licence key, line 104)
- `website/src/content/docs/docs/platform/plans-and-licence.mdx`
- `website/src/content/docs/docs/self-hosting/configuration-reference.md` (the `license_key` row, line 25)
- `website/src/content/docs/docs/self-hosting/configuration.md` (the `license_key` row, line 28)
- `CHANGELOG.md`
- `docs/roadmap.md` (track P row, line 56)

**Steps:**
1. Spec 04: add the comment line `-- 'license.key': the licence key an instance admin installed, plain text (D-P-75)` under the `encryption.check` line.
2. Spec 09: after `GET /api/v1/admin/license (licence status, never the key, D-P-7)` add `· PUT /api/v1/admin/license (install a key: 400 refused or expired, 409 when license_key is configured; D-P-75)`; mention that the GET carries `source` and `renew_url` (D-P-74).
3. Spec 11: the licence card bullet adds "the store link (`renew_url`), where the key comes from, and an install form unless the key is configured (D-P-74, D-P-75)".
4. Spec 12: the `license_key` row adds "; wins over a key installed from the admin settings (D-P-75)". § Licence key: add two sentences: an instance admin may install a key from the admin settings, stored in `meta.db` and applied without a restart, the configured key winning (D-P-75); a failed read of the stored key aborts startup like any other `meta.db` read; the licence card links to `https://app.cetra.sh/licenses` unless `entitlements.upgrade_url` is set (D-P-74).
5. Site, "Plans and licence": rewrite § "Add a licence key" as two ways, keeping the style guide (`website/STYLE.md`): (1) **From the admin settings**: an instance admin opens Admin → Settings, pastes the key under "Install a licence key" and clicks Install; it applies at once, no restart; it is kept in `meta.db`, so backups carry it; to renew, follow "Buy or renew a licence" on the licence card and install the new key the same way. (2) **In the configuration**: the existing `CETRA_LICENSE_KEY` block; a configured key wins, and the card then shows no install form; changing it needs a restart. Keep the bash block marked as it is today (the samples check). Keep the paragraph about grace and lapses.
6. `configuration.md` line 28: the last cell becomes "enterprise features; wins over a key installed from the admin settings". Configuration reference `license_key` row: "The licence key that unlocks enterprise features. Unset: an installed key applies, else they stay locked. When set, it wins over a key installed from the admin settings."
7. `CHANGELOG.md` § `[Unreleased]`, `### Added`: "- Instance admins install a licence key from the admin settings, and it applies without a restart. A key in `CETRA_LICENSE_KEY` still wins." and "- The licence card links to the licence store to buy or renew a licence."
8. Roadmap track P row: append to the status cell "Licence renewal (Q's R26, R27; D-P-74, D-P-75): store link and key install without a restart, [`06-licence-renewal.md`](plans/p-entitlements/06-licence-renewal.md)."

**Tests:** none (docs only).

**Done when:** `cargo test -p cetra-server the_configuration_reference_matches_the_loader` passes, and `cd website && node scripts/check-terms.mjs && node scripts/check-samples.mjs && node scripts/check-env-vars.mjs` pass (run what `just check-docs` runs for content checks, not the full build).

**Commit:** `Document installing a licence key`
