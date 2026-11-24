//! First user, org, and project (spec 08 § Bootstrapping, D144).
//!
//! Headless `[init]` runs in one transaction when `users` is empty. Otherwise
//! a setup token is kept in memory and logged once. That log line is the only
//! secret this process writes.

use std::sync::Mutex;

use crate::api::account::OrgRole;
use crate::audit::catalog::{AuthAction, InstanceAction};
use crate::audit::{self, Event};
use crate::auth::AuthError;
use crate::auth::password::{Hasher, PolicyError, check_policy};
use crate::auth::roles::KeyScope;
use crate::auth::secret::{
    KeyKind, display_prefix, hash_token, new_key, new_opaque_token, parse_key,
};
use crate::config::InitConfig;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::api_keys::{ApiKeyRow, NewApiKey, insert_api_key};
use crate::meta::members::add_org_member;
use crate::meta::orgs::{OrgRow, insert_org_with_derived_slug};
use crate::meta::projects::{ProjectRow, insert_project_with_derived_slug};
use crate::meta::users::EmailVerifiedVia;
use crate::meta::users::{
    NewUser, UserRow, count_users, count_users_tx, insert_user, normalize_email,
};
use cetra_core::ids::OrgId;

const FULL_SCOPES: [KeyScope; 3] = [KeyScope::Ingest, KeyScope::Read, KeyScope::Write];
const READ_SCOPES: [KeyScope; 1] = [KeyScope::Read];

/// The setup token, when first-run setup is still open.
pub(crate) struct SetupState {
    token: Mutex<Option<String>>,
}

impl SetupState {
    /// No token. [`prepare`] fills it when the database has no user and no `[init]`.
    pub(crate) fn new() -> Self {
        Self {
            token: Mutex::new(None),
        }
    }

    /// The current token. `None` means setup is closed.
    ///
    /// # Errors
    ///
    /// Returns [`BootstrapError::Lock`] when a holder panicked.
    pub(crate) fn token(&self) -> Result<Option<String>, BootstrapError> {
        Ok(self.token.lock().map_err(|_| BootstrapError::Lock)?.clone())
    }

    /// Forgets the token after a successful setup.
    ///
    /// # Errors
    ///
    /// Returns [`BootstrapError::Lock`] when a holder panicked.
    pub(crate) fn clear(&self) -> Result<(), BootstrapError> {
        *self.token.lock().map_err(|_| BootstrapError::Lock)? = None;
        Ok(())
    }

    fn set(&self, token: String) -> Result<(), BootstrapError> {
        *self.token.lock().map_err(|_| BootstrapError::Lock)? = Some(token);
        Ok(())
    }
}

/// Why first-run setup or headless init failed.
///
/// `Init` names the rule or the variable. It does not include the password,
/// the key, the email, or the org name.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BootstrapError {
    /// `[init]` failed a check. Startup stops.
    #[error("init failed: {reason}")]
    Init {
        /// The rule or the variable name.
        reason: String,
    },
    /// `meta.db` rejected the write.
    #[error(transparent)]
    Meta(#[from] MetaError),
    /// The random source or argon2 failed.
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// A setup-token lock holder panicked.
    #[error("setup token lock poisoned")]
    Lock,
}

/// What one transaction created.
pub(crate) struct Provisioned {
    /// The instance admin.
    pub(crate) user: UserRow,
    /// The org.
    pub(crate) org: OrgRow,
    /// The project.
    pub(crate) project: ProjectRow,
}

/// Values for [`provision`]. The password is already a PHC string.
struct Provision {
    email: String,
    name: Option<String>,
    password_hash: String,
    org_name: String,
    project_name: String,
    key: Option<KeyMaterial>,
    read_key: Option<KeyMaterial>,
}

struct KeyMaterial {
    name: String,
    prefix: String,
    hash: String,
}

/// Users exist, `[init]` is applied, or a setup token is logged once.
///
/// # Errors
///
/// Returns [`BootstrapError::Init`] when `[init]` fails a check. The database
/// is unchanged. A database or hashing failure is [`BootstrapError::Meta`] or
/// [`BootstrapError::Auth`].
pub(crate) async fn prepare(state: &AppState) -> Result<(), BootstrapError> {
    if count_users(state.meta.reader()).await? > 0 {
        return Ok(());
    }
    if init_complete(&state.config.init) {
        apply_init(state).await?;
        return Ok(());
    }
    let token = new_opaque_token()?;
    let url = format!(
        "{}?token={}",
        state.config.public_url.join("/setup"),
        token.plaintext
    );
    tracing::warn!(target: "setup", "setup required: open {url}");
    state.setup.set(token.plaintext)?;
    Ok(())
}

/// Creates the first admin from `[init]` when `users` is still empty.
///
/// Policy and the secret-key shape are checked before the transaction, so
/// argon2 does not hold the SQLite write lock. The inserts and the empty-table
/// re-check are one write. A crash before commit leaves an empty database.
///
/// # Errors
///
/// Returns [`BootstrapError::Init`] for a bad email, name, password, or secret
/// key, or a read key that is malformed or equal to the secret key. The
/// reason names the rule or the variable, not the value.
pub(crate) async fn apply_init(state: &AppState) -> Result<(), BootstrapError> {
    let init = state.config.init.clone();
    let email = init
        .admin_email
        .as_deref()
        .ok_or_else(|| init_error("init.admin_email is unset"))?;
    let email = normalize_email(email).map_err(|_| init_error("init.admin_email is invalid"))?;
    let password = init
        .admin_password
        .as_ref()
        .map(|secret| secret.expose().clone())
        .ok_or_else(|| init_error("init.admin_password is unset"))?;
    let org_name = required_name(init.org.as_deref(), "init.org")?;
    let project_name = required_name(init.project.as_deref(), "init.project")?;
    check_policy(&password, &email).map_err(|error| init_error(policy_reason(error)))?;
    let key = match init.secret_key.as_ref() {
        None => None,
        Some(secret) => Some(provided_key(
            secret.expose(),
            "init",
            "CETRA_INIT_SECRET_KEY is not a secret key",
        )?),
    };
    let read_key = match init.read_key.as_ref() {
        None => None,
        Some(secret) => Some(provided_key(
            secret.expose(),
            "init-read",
            "CETRA_INIT_READ_KEY is not a secret key",
        )?),
    };
    if let (Some(key), Some(read_key)) = (&key, &read_key)
        && key.hash == read_key.hash
    {
        return Err(init_error(
            "CETRA_INIT_READ_KEY must differ from CETRA_INIT_SECRET_KEY",
        ));
    }
    let hasher = Hasher::new().await?;
    let password_hash = hasher.hash(&password).await?;
    match provision(
        state,
        Provision {
            email,
            name: None,
            password_hash,
            org_name,
            project_name,
            key,
            read_key,
        },
        None,
    )
    .await?
    {
        None => {
            tracing::info!("init skipped: users exist");
            Ok(())
        }
        Some(_) => Ok(()),
    }
}

/// Inserts the first admin, org, project, and optional secret key.
///
/// `None` means a user appeared before this transaction. The caller answers
/// 409 or logs that init was skipped. `ctx` is the setup request (`auth.setup_completed`);
/// `None` is an init from the config (`instance.init_applied`, actor `system`).
///
/// # Errors
///
/// Returns [`BootstrapError`] when the write fails.
async fn provision(
    state: &AppState,
    input: Provision,
    ctx: Option<audit::Ctx>,
) -> Result<Option<Provisioned>, BootstrapError> {
    let now = state.clock.now_ns();
    let created = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if count_users_tx(conn).await? != 0 {
                    return Ok(None);
                }
                let created = insert_first(conn, &input, now).await?;
                let (ctx, event) = match ctx {
                    Some(ctx) => (
                        ctx.with_actor(audit::Actor::User {
                            id: created.user.id,
                            via: audit::Via::Session,
                            credential_id: None,
                        }),
                        Event::new(AuthAction::SetupCompleted),
                    ),
                    None => (
                        audit::Ctx::system(),
                        Event::new(InstanceAction::InitApplied),
                    ),
                };
                let event = event.org(created.org.id).project(created.project.id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(Some(created))
            })
        })
        .await?;
    Ok(created)
}

async fn insert_first(
    conn: &mut sqlx::SqliteConnection,
    input: &Provision,
    now: i64,
) -> Result<Provisioned, MetaError> {
    let user = insert_user(
        conn,
        NewUser {
            email: input.email.clone(),
            name: input.name.clone(),
            password_hash: Some(input.password_hash.clone()),
            email_verified_at: Some(now),
            email_verified_via: Some(EmailVerifiedVia::Operator),
            is_instance_admin: true,
        },
        now,
    )
    .await?;
    let org = insert_org_with_derived_slug(conn, OrgId::new_v7(), &input.org_name, now).await?;
    add_org_member(conn, org.id, user.id, OrgRole::Owner, now).await?;
    let project =
        insert_project_with_derived_slug(conn, org.id, &input.project_name, None, now).await?;
    if let Some(key) = &input.key {
        insert_key(conn, project.id, user.id, key, &FULL_SCOPES, now).await?;
    }
    if let Some(key) = &input.read_key {
        insert_key(conn, project.id, user.id, key, &READ_SCOPES, now).await?;
    }
    Ok(Provisioned { user, org, project })
}

async fn insert_key(
    conn: &mut sqlx::SqliteConnection,
    project_id: cetra_core::ids::ProjectId,
    created_by: uuid::Uuid,
    key: &KeyMaterial,
    scopes: &[KeyScope],
    now: i64,
) -> Result<ApiKeyRow, MetaError> {
    insert_api_key(
        conn,
        NewApiKey {
            project_id,
            kind: "secret",
            scopes: scopes.to_vec(),
            name: Some(key.name.clone()),
            prefix: key.prefix.clone(),
            hash: key.hash.clone(),
            created_by,
            expires_at: None,
        },
        now,
    )
    .await
}

fn init_complete(init: &InitConfig) -> bool {
    init.admin_email.is_some()
}

fn required_name(value: Option<&str>, key: &'static str) -> Result<String, BootstrapError> {
    let Some(value) = value else {
        return Err(init_error(match key {
            "init.org" => "init.org is unset",
            _ => "init.project is unset",
        }));
    };
    let name = value.trim();
    let length = name.chars().count();
    if !(1..=100).contains(&length) {
        return Err(init_error(match key {
            "init.org" => "init.org must be 1 to 100 characters",
            _ => "init.project must be 1 to 100 characters",
        }));
    }
    Ok(name.to_owned())
}

/// Checks the shape of an init key. `reason` names the variable, never the value.
fn provided_key(plaintext: &str, name: &str, reason: &str) -> Result<KeyMaterial, BootstrapError> {
    if parse_key(plaintext) != Some(KeyKind::Secret) {
        return Err(init_error(reason));
    }
    Ok(KeyMaterial {
        name: name.to_owned(),
        prefix: display_prefix(plaintext),
        hash: hash_token(plaintext),
    })
}

fn policy_reason(error: PolicyError) -> &'static str {
    match error {
        PolicyError::TooShort => "password is too short",
        PolicyError::TooLong => "password is too long",
        PolicyError::EqualsEmail => "password matches the email",
        PolicyError::TooCommon => "password is too common",
    }
}

fn init_error(reason: &str) -> BootstrapError {
    BootstrapError::Init {
        reason: reason.to_owned(),
    }
}

/// A completed `POST /api/auth/setup` write. `secret_key` is shown once.
pub(crate) struct SetupCreated {
    /// The rows that were inserted.
    pub(crate) created: Provisioned,
    /// Secret API key plaintext.
    pub(crate) secret_key: String,
    /// Type prefix plus the first 6 tail characters.
    pub(crate) secret_key_prefix: String,
}

/// Hashes the password, mints a secret key, and inserts the first admin.
///
/// `None` means a user appeared first. The plaintext key is not logged.
///
/// # Errors
///
/// Returns [`BootstrapError`] when hashing or the write fails.
pub(crate) async fn create_setup(
    state: &AppState,
    email: String,
    name: Option<String>,
    password: &str,
    org_name: String,
    project_name: String,
    ctx: Option<audit::Ctx>,
) -> Result<Option<SetupCreated>, BootstrapError> {
    let minted = new_key(KeyKind::Secret)?;
    let secret_key_prefix = minted.prefix.clone();
    let password_hash = Hasher::new().await?.hash(password).await?;
    let created = provision(
        state,
        Provision {
            email,
            name,
            password_hash,
            org_name,
            project_name,
            key: Some(KeyMaterial {
                name: "setup".to_owned(),
                prefix: minted.prefix,
                hash: minted.hash,
            }),
            read_key: None,
        },
        ctx,
    )
    .await?;
    Ok(created.map(|created| SetupCreated {
        created,
        secret_key: minted.plaintext,
        secret_key_prefix,
    }))
}

#[cfg(test)]
mod tests {
    use super::{apply_init, prepare};
    use crate::auth::secret::{KeyKind, hash_token, new_key};
    use crate::meta::users::count_users;
    use crate::test_support::{TestApp, capture_logs};
    use axum::http::{Method, StatusCode};

    fn init(value: serde_json::Value) -> crate::config::InitConfig {
        serde_json::from_value(value).unwrap()
    }

    /// The `instance.init_applied` rows in `audit_events`.
    async fn init_applied_rows(app: &TestApp) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'instance.init_applied'",
        )
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn init_creates_the_admin_once() {
        let key = new_key(KeyKind::Secret).unwrap();
        let app = TestApp::with_config(|config| {
            config.init = init(serde_json::json!({
                "admin_email": "Ada@Example.com",
                "admin_password": "cetra-ok-1",
                "org": " Acme ",
                "project": "Support bot",
                "secret_key": key.plaintext.clone(),
            }));
        })
        .await;
        let (prepared, logs) = capture_logs(prepare(&app.state)).await;
        prepared.unwrap();
        assert_eq!(init_applied_rows(&app).await, 1);
        assert!(!logs.contains("setup required"), "{logs}");
        assert!(!logs.contains(&key.plaintext), "{logs}");
        assert!(app.state.setup.token().unwrap().is_none());
        assert_eq!(count_users(app.state.meta.reader()).await.unwrap(), 1);
        let (hash, name): (String, String) = sqlx::query_as("SELECT key_hash, name FROM api_keys")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(hash, hash_token(&key.plaintext));
        assert_eq!(name, "init");
        let (again_result, again) = capture_logs(prepare(&app.state)).await;
        again_result.unwrap();
        assert!(!again.contains("setup required"), "{again}");
        assert_eq!(count_users(app.state.meta.reader()).await.unwrap(), 1);
        assert_eq!(init_applied_rows(&app).await, 1);
        let (skipped_result, skipped) = capture_logs(apply_init(&app.state)).await;
        skipped_result.unwrap();
        assert!(skipped.contains("init skipped: users exist"), "{skipped}");
        assert_eq!(init_applied_rows(&app).await, 1);
        let orgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orgs")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(orgs, 1);
    }

    #[tokio::test]
    async fn a_bad_init_secret_or_password_stops_startup() {
        let bad_key = TestApp::with_config(|config| {
            config.init = init(serde_json::json!({
                "admin_email": "ada@example.com",
                "admin_password": "cetra-ok-1",
                "org": "Acme",
                "project": "Support bot",
                "secret_key": "not-a-secret-key",
            }));
        })
        .await;
        let error = prepare(&bad_key.state).await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("CETRA_INIT_SECRET_KEY"), "{text}");
        assert!(!text.contains("not-a-secret-key"), "{text}");
        assert_eq!(count_users(bad_key.state.meta.reader()).await.unwrap(), 0);

        let weak = TestApp::with_config(|config| {
            config.init = init(serde_json::json!({
                "admin_email": "ada@example.com",
                "admin_password": "nope-nope",
                "org": "Acme",
                "project": "Support bot",
            }));
        })
        .await;
        let error = prepare(&weak.state).await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("password is too short"), "{text}");
        assert!(!text.contains("nope-nope"), "{text}");
        assert_eq!(count_users(weak.state.meta.reader()).await.unwrap(), 0);
    }

    /// The init values of [`init_creates_the_admin_once`], plus `extra`.
    fn init_with(extra: serde_json::Value) -> crate::config::InitConfig {
        let mut value = serde_json::json!({
            "admin_email": "ada@example.com",
            "admin_password": "cetra-ok-1",
            "org": "Acme",
            "project": "Support bot",
        });
        let serde_json::Value::Object(extra) = extra else {
            panic!("extra init values must be an object");
        };
        for (key, field) in extra {
            value[key] = field;
        }
        init(value)
    }

    /// The only project of the app.
    async fn the_project(app: &TestApp) -> String {
        sqlx::query_scalar("SELECT id FROM projects")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    fn bearer(key: &str) -> String {
        format!("Bearer {key}")
    }

    #[tokio::test]
    async fn the_init_read_key_reads_and_cannot_ingest_or_write() {
        let secret = new_key(KeyKind::Secret).unwrap();
        let read = new_key(KeyKind::Secret).unwrap();
        let app = TestApp::with_config(|config| {
            config.init = init_with(serde_json::json!({
                "secret_key": secret.plaintext.clone(),
                "read_key": read.plaintext.clone(),
            }));
        })
        .await;
        let (prepared, logs) = capture_logs(prepare(&app.state)).await;
        prepared.unwrap();
        assert_eq!(init_applied_rows(&app).await, 1);
        let project_matches: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'instance.init_applied' \
             AND project_id = (SELECT id FROM projects)",
        )
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(project_matches, 1);
        assert!(!logs.contains(&read.plaintext), "{logs}");
        assert!(!logs.contains(&secret.plaintext), "{logs}");
        let mut rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT name, key_hash, scopes FROM api_keys ORDER BY name")
                .fetch_all(app.state.meta.reader())
                .await
                .unwrap();
        rows.sort();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "init");
        assert_eq!(rows[0].1, hash_token(&secret.plaintext));
        assert_eq!(rows[1].0, "init-read");
        assert_eq!(rows[1].1, hash_token(&read.plaintext));
        assert_eq!(rows[1].2, r#"["read"]"#);

        let project = the_project(&app).await;
        let (reads, logs) = capture_logs(async {
            let traces = app
                .request(Method::GET, &format!("/api/v1/projects/{project}/traces"))
                .header("authorization", &bearer(&read.plaintext))
                .send()
                .await;
            let ingest = app
                .request(Method::POST, "/v1/traces")
                .header("authorization", &bearer(&read.plaintext))
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await;
            let write = app
                .request(
                    Method::POST,
                    &format!("/api/v1/projects/{project}/datasets"),
                )
                .header("authorization", &bearer(&read.plaintext))
                .json(&serde_json::json!({"name": "probe"}))
                .send()
                .await;
            let full = app
                .request(Method::POST, "/v1/traces")
                .header("authorization", &bearer(&secret.plaintext))
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await;
            (traces, ingest, write, full)
        })
        .await;
        let (traces, ingest, write, full) = reads;
        assert_eq!(traces.status, StatusCode::OK);
        assert_eq!(ingest.status, StatusCode::FORBIDDEN);
        assert_eq!(write.status, StatusCode::FORBIDDEN);
        assert_eq!(full.status, StatusCode::OK);
        assert!(!logs.contains(&read.plaintext), "{logs}");
    }

    #[tokio::test]
    async fn the_init_read_key_reads_in_restore_mode() {
        let read = new_key(KeyKind::Secret).unwrap();
        let app = TestApp::with_config(|config| {
            config.restore_mode = true;
            config.init = init_with(serde_json::json!({"read_key": read.plaintext.clone()}));
        })
        .await;
        prepare(&app.state).await.unwrap();
        let project = the_project(&app).await;
        let traces = app
            .request(Method::GET, &format!("/api/v1/projects/{project}/traces"))
            .header("authorization", &bearer(&read.plaintext))
            .send()
            .await;
        assert_eq!(traces.status, StatusCode::OK);
        let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(keys, 1);
    }

    #[tokio::test]
    async fn a_bad_or_reused_init_read_key_stops_startup() {
        let secret = new_key(KeyKind::Secret).unwrap();
        let public = new_key(KeyKind::Public).unwrap();
        let cases = [
            (
                serde_json::json!({"read_key": "sk-cetra-short"}),
                "sk-cetra-short".to_owned(),
                "CETRA_INIT_READ_KEY is not a secret key",
            ),
            (
                serde_json::json!({"read_key": public.plaintext.clone()}),
                public.plaintext.clone(),
                "CETRA_INIT_READ_KEY is not a secret key",
            ),
            (
                serde_json::json!({
                    "secret_key": secret.plaintext.clone(),
                    "read_key": secret.plaintext.clone(),
                }),
                secret.plaintext.clone(),
                "CETRA_INIT_READ_KEY must differ from CETRA_INIT_SECRET_KEY",
            ),
        ];
        for (extra, value, reason) in cases {
            let app = TestApp::with_config(|config| config.init = init_with(extra)).await;
            let error = prepare(&app.state).await.unwrap_err();
            let text = error.to_string();
            assert!(text.contains(reason), "{text}");
            assert!(!text.contains(&value), "{text}");
            assert_eq!(count_users(app.state.meta.reader()).await.unwrap(), 0);
        }
    }
}
