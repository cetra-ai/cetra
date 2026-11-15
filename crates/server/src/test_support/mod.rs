//! HTTP tests: a real `meta.db` and storage, a manual clock, `oneshot` (D151).
#![cfg(test)]

pub(crate) mod hook_receiver;
pub(crate) mod mock_idp;
pub(crate) mod mock_llm;
pub(crate) mod mock_webhook;
pub(crate) mod oauth;

use std::future::Future;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use bytesize::ByteSize;
use http_body_util::BodyExt;
use llmobs_core::batch::{scores_to_record_batch, spans_to_record_batch};
use llmobs_core::ids::{OrgId, ProjectId, SpanId, TraceId};
use llmobs_core::names::PRODUCT;
use llmobs_core::score::Score;
use llmobs_core::span::Span;
use llmobs_storage::config::{
    AnchorConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use llmobs_storage::{MediaBlob, ProjectQuery, QueryContext, Storage};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqliteConnection;
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::common::Problem;
use crate::auth::Limits;
use crate::auth::roles::{KeyScope, OrgRole};
use crate::auth::secret::{self, KeyKind, NewKey};
use crate::auth::session::{SessionOrigin, start_session};
use crate::clock::Clock;
use crate::config::{
    AuthSettings, EntitlementsConfig, EvalConfig, IngestConfig, InitConfig, LogConfig, MediaConfig,
    OrgsConfig, PublicUrl, SecurityConfig, ServerConfig,
};
use crate::http::access::RouteDef;
use crate::http::{AppInner, AppState, router, router_with, serve_router};
use crate::meta::api_keys::{self, NewApiKey};
use crate::meta::members::add_org_member;
use crate::meta::orgs::insert_org;
use crate::meta::personal_tokens::{NewToken, insert_token};
use crate::meta::projects::{ProjectRow, insert_project};
use crate::meta::users::EmailVerifiedVia;
use crate::meta::users::set_email_verified;
use crate::meta::users::{self, NewUser, insert_user};
use crate::meta::{MetaDb, MetaError};

/// `2026-01-01T00:00:00Z`.
const MANUAL_NOW_NS: i64 = 1_767_225_600_000_000_000;

/// The key every test `SecretBox` is built from (D-I-2).
pub(crate) const TEST_KEY: [u8; 32] = [7; 32];

/// The development signing seed (`fixtures/license/dev-1.key`, D-P-6).
const DEV_SEED: &str = include_str!("../../../../fixtures/license/dev-1.key");

/// The `dev-1` signing key. Debug builds trust its public half.
pub(crate) fn dev_signing_key() -> ed25519_dalek::SigningKey {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    let seed: [u8; 32] = STANDARD
        .decode(DEV_SEED.trim())
        .expect("dev seed is base64")
        .try_into()
        .expect("dev seed is 32 bytes");
    ed25519_dalek::SigningKey::from_bytes(&seed)
}

/// Signs `payload` as a `dev-1` token (D-P-5). Debug builds trust it.
pub(crate) fn sign_token(payload: &serde_json::Value) -> String {
    sign_raw(&serde_json::to_vec(payload).expect("payload serializes"))
}

/// Signs the bytes `payload` as a `dev-1` token, whatever they are (not necessarily JSON).
pub(crate) fn sign_raw(payload: &[u8]) -> String {
    llmobs_token::sign_raw(&dev_signing_key(), "dev-1", payload).expect("dev key signs")
}

/// A `dev-1` licence listing `features`, issued 2025-01-01, active until 2099-01-01.
pub(crate) fn license_key(features: &[&str]) -> String {
    license_key_until(features, "2099-01-01T00:00:00Z", "2099-01-01T00:00:00Z")
}

/// A `dev-1` licence listing `features` with the given RFC 3339 `expires_at` and
/// `grace_ends_at`. The manual clock is `2026-01-01T00:00:00Z`.
pub(crate) fn license_key_until(
    features: &[&str],
    expires_at: &str,
    grace_ends_at: &str,
) -> String {
    sign_token(&serde_json::json!({
        "kind": "license",
        "id": "01900000-0000-7000-8000-000000000510",
        "issued_at": "2025-01-01T00:00:00Z",
        "customer": "Test Customer",
        "features": features,
        "expires_at": expires_at,
        "grace_ends_at": grace_ends_at,
    }))
}

/// A `dev-1` org token for `org_id`, audience the default test `public_url`'s origin, plan
/// `pro` with add-on `teams`. Times are RFC 3339; `limits` is the JSON object of the
/// `limits` claim.
pub(crate) fn org_token(
    org_id: OrgId,
    issued_at: &str,
    expires_at: &str,
    grace_ends_at: &str,
    features: &[&str],
    limits: &serde_json::Value,
) -> String {
    sign_token(&serde_json::json!({
        "kind": "org",
        "id": Uuid::now_v7().to_string(),
        "issued_at": issued_at,
        "expires_at": expires_at,
        "grace_ends_at": grace_ends_at,
        "audience": PublicUrl::default().origin(),
        "org_id": org_id.to_string(),
        "plan": {"id": "pro", "name": "Pro", "add_ons": ["teams"]},
        "features": features,
        "limits": limits,
    }))
}

/// Gives `org_id` a valid org token (issued at `issued_at`, valid until 2099) with these
/// limits, through `grant_from_token` and `state.entitlements.put_grant`. `limits` is the
/// JSON object of the `limits` claim; missing keys take the defaults. Use a later
/// `issued_at` to lower a limit.
pub(crate) fn set_limits(
    app: &TestApp,
    org_id: OrgId,
    issued_at: &str,
    limits: &serde_json::Value,
) {
    let token = org_token(
        org_id,
        issued_at,
        "2099-01-01T00:00:00Z",
        "2099-02-01T00:00:00Z",
        &[],
        limits,
    );
    let defaults = app.state.entitlements.defaults().limits;
    let (org, grant) = crate::entitlements::tokens::grant_from_token(
        &token,
        app.state.config.public_url.origin(),
        &defaults,
    )
    .expect("the test token verifies");
    app.state.entitlements.put_grant(org, grant);
}

/// A `dev-1` control-plane token for the default test `public_url`, issued at the manual
/// clock (2026-01-01T00:00:00Z), valid 5 minutes, with this scope, for one request (D-P-71).
pub(crate) fn control_plane_token(scope: &str, method: &str, path: &str) -> String {
    sign_token(&serde_json::json!({
        "kind": "control_plane",
        "id": Uuid::now_v7().to_string(),
        "issued_at": "2026-01-01T00:00:00Z",
        "expires_at": "2026-01-01T00:05:00Z",
        "audience": PublicUrl::default().origin(),
        "scope": scope,
        "method": method,
        "path": path,
    }))
}

/// One process's data directory and router.
pub(crate) struct TestApp {
    /// Kept so the directory outlives the open files.
    _dir: TempDir,
    pub(crate) state: AppState,
    router: Router,
}

impl TestApp {
    /// Defaults, `data_dir` in a temp directory, `public_url` `http://localhost:8080`.
    pub(crate) async fn new() -> Self {
        Self::open(|_| {}, None, None, false, None, None, None).await
    }

    /// `tweak` runs before storage and `meta.db` open.
    ///
    /// `storage.data_dir` is then copied from the top-level `data_dir`.
    pub(crate) async fn with_config(tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::open(tweak, None, None, false, None, None, None).await
    }

    /// Same as [`Self::new`] with extra routes inside the shared layers.
    pub(crate) async fn with_routes(routes: Router<AppState>) -> Self {
        Self::open(|_| {}, Some(routes), None, false, None, None, None).await
    }

    /// [`Self::with_config`] and [`Self::with_routes`] together.
    pub(crate) async fn with(
        tweak: impl FnOnce(&mut ServerConfig),
        routes: Router<AppState>,
    ) -> Self {
        Self::open(tweak, Some(routes), None, false, None, None, None).await
    }

    /// Serves this app's router on `127.0.0.1` at an ephemeral port, in a background task that
    /// ends with the test's runtime. Returns the address. For tests that need a real socket
    /// (how hyper ends a failed body).
    pub(crate) async fn serve_loopback(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("loopback address");
        let router = self.router.clone();
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        addr
    }

    /// The given route table instead of the server's, with the same layers.
    pub(crate) async fn with_route_table(routes: Vec<RouteDef>) -> Self {
        Self::open(|_| {}, None, Some(routes), false, None, None, None).await
    }

    /// [`Self::new`] with a trace load waiting `permit_wait` for a permit (a job's `Saturated`
    /// requeue, § I3.6), so a test does not wait 30 s.
    pub(crate) async fn with_permit_wait(permit_wait: Duration) -> Self {
        Self::open(|_| {}, None, None, false, None, None, Some(permit_wait)).await
    }

    /// The stub mailer. [`Self::sent`] reads what it accepted.
    pub(crate) async fn with_mailer() -> Self {
        Self::open(|_| {}, None, None, true, None, None, None).await
    }

    /// [`Self::with_config`] with the stub mailer.
    pub(crate) async fn with_mailer_config(tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::open(tweak, None, None, true, None, None, None).await
    }

    /// An app whose licence lists `features`.
    pub(crate) async fn with_license(features: &[&str]) -> Self {
        let key = license_key(features);
        Self::with_config(move |config| {
            config.license_key = Some(crate::config::Secret::new(key));
        })
        .await
    }

    /// [`Self::with_config`] with GitHub's endpoints pointed at `base`.
    pub(crate) async fn with_github(base: &str, tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::open(tweak, None, None, false, Some(base), None, None).await
    }

    /// [`Self::with_config`] with Google's issuer pointed at `issuer`.
    pub(crate) async fn with_google(issuer: &str, tweak: impl FnOnce(&mut ServerConfig)) -> Self {
        Self::open(tweak, None, None, false, None, Some(issuer), None).await
    }

    /// Messages the stub mailer accepted. Empty when the stub is not installed.
    pub(crate) fn sent(&self) -> Vec<crate::email::SentMail> {
        self.state
            .mailer
            .as_ref()
            .map(|mailer| mailer.sent())
            .unwrap_or_default()
    }

    async fn open(
        tweak: impl FnOnce(&mut ServerConfig),
        routes: Option<Router<AppState>>,
        table: Option<Vec<RouteDef>>,
        stub_mailer: bool,
        github_base: Option<&str>,
        google_issuer: Option<&str>,
        permit_wait: Option<Duration>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = default_config(dir.path());
        tweak(&mut config);
        config.storage.data_dir.clone_from(&config.data_dir);
        let storage = Storage::open(config.storage_config())
            .await
            .expect("storage opens");
        let meta = MetaDb::open(&config.data_dir).await.expect("meta.db opens");
        let secrets = crate::crypto::SecretBox::new(&TEST_KEY);
        crate::crypto::verify_or_record(
            &meta,
            &secrets,
            &crate::crypto::key_path(&config.data_dir),
        )
        .await
        .expect("check value");
        let eval_runner = Arc::new(
            llmobs_eval::EvalRunner::new(llmobs_eval::EgressPolicy {
                allow_private_network: config.security.allow_private_network_egress,
                restore_mode: config.restore_mode,
            })
            .expect("eval runner"),
        );
        let telemetry = crate::telemetry::Telemetry::new(config.restore_mode);
        let mailer = if stub_mailer {
            let from = format!("{PRODUCT} <noreply@example.com>");
            Some(Arc::new(crate::email::Mailer::stub(
                &from,
                telemetry.emails.clone(),
            )))
        } else {
            crate::email::Mailer::from_config(&config, telemetry.emails.clone())
                .expect("smtp settings build a mailer")
                .map(Arc::new)
        };
        let mut providers = {
            let http = crate::auth::oidc::identity_client().expect("identity client");
            crate::auth::oidc::Providers::for_config(&config, &http).expect("identity providers")
        };
        if let Some(base) = github_base {
            providers.retarget_github(base);
        }
        if let Some(issuer) = google_issuer {
            providers.retarget_google(issuer);
        }
        let prices = llmobs_otel::PriceTable::new(llmobs_otel::builtin_price_rows())
            .expect("built-in price table");
        let ingest = crate::ingest::Ingest::new(
            &config.ingest,
            &config.media,
            Arc::new(crate::prices::Prices::new(prices)),
        );
        let license = crate::ee::license::LicenseState::from_key(
            config.license_key.as_ref().map(|key| key.expose().as_str()),
        );
        let defaults = crate::entitlements::Defaults::from_config(&config.entitlements.defaults);
        let entitlements = crate::entitlements::EntitlementsState::load(
            &meta,
            license,
            defaults,
            config.public_url.origin(),
            config.entitlements.control_plane,
        )
        .await
        .expect("stored org tokens load");
        let eval = crate::eval::EvalQueue::new(config.eval.workers);
        let org_sso = crate::ee::org_sso::OrgSso::new(
            llmobs_eval::EgressPolicy {
                allow_private_network: config.security.allow_private_network_egress,
                restore_mode: config.restore_mode,
            },
            crate::ee::org_sso::dns::TxtResolver::fixed(),
        )
        .expect("org SSO state");
        let state = AppState::new(AppInner {
            config,
            meta,
            storage: Arc::new(storage),
            ready: crate::http::health::ReadyCache::default(),
            clock: Clock::Manual(Arc::new(AtomicI64::new(MANUAL_NOW_NS))),
            limits: Limits::new(),
            key_cache: crate::auth::principal::ApiKeyCache::new(),
            pat_cache: crate::auth::principal::PatCache::new(),
            last_used: crate::auth::last_used::LastUsed::new(),
            token_last_used: crate::auth::last_used::LastUsed::tokens(),
            setup: crate::bootstrap::SetupState::new(),
            password_hasher: tokio::sync::OnceCell::new(),
            mailer,
            providers,
            oauth_key: crate::auth::oidc::random_key().expect("oauth key"),
            ingest,
            live: crate::live::LiveBus::new(crate::live::StreamTiming::TEST),
            metrics_cache: crate::routes::metrics::MetricsCache::default(),
            usage: crate::usage::Usage::default(),
            alert_email_count: crate::alerts::quota::DailyEmailCount::default(),
            deletions: crate::deletion::Deletions::new(),
            secrets,
            eval_runner,
            connections: crate::eval::connection::ConnectionCache::default(),
            entitlements,
            ingest_rate: crate::entitlements::rate::IngestRate::default(),
            trace_loads: crate::eval::trace_load::TraceLoads::new(
                permit_wait.unwrap_or(crate::routes::read::READ_TIMEOUT),
            ),
            definitions: crate::eval::definitions::DefinitionCache::default(),
            org_sso,
            backup_slot: tokio::sync::Mutex::new(()),
            eval,
            pending: crate::eval::pending::PendingTraces::default(),
            telemetry,
        });
        state
            .org_sso
            .reload(&state.meta, &state.secrets, &state.config.auth)
            .await
            .expect("org SSO reload");
        let router = if let Some(table) = table {
            serve_router(state.clone(), table)
        } else if let Some(routes) = routes {
            router_with(state.clone(), routes)
        } else {
            router(state.clone(), None)
        };
        Self {
            _dir: dir,
            state,
            router,
        }
    }

    /// Sets the TXT records the fixed resolver answers for `name`.
    pub(crate) fn dns_txt(&self, name: &str, values: &[&str]) {
        self.state.org_sso.dns.set(name, values);
    }

    /// Makes every TXT lookup fail (P8's 503 test).
    pub(crate) fn dns_fail(&self, fail: bool) {
        self.state.org_sso.dns.set_fail(fail);
    }

    pub(crate) fn request(&self, method: Method, path: &str) -> TestRequest {
        TestRequest {
            router: self.router.clone(),
            origin_default: self.state.config.public_url.origin().to_owned(),
            method,
            path: path.to_owned(),
            headers: HeaderMap::new(),
            body: Bytes::new(),
            peer: SocketAddr::from(([127, 0, 0, 1], 50_000)),
            origin: OriginChoice::Default,
        }
    }
}

/// A user (`ada@example.com`, instance admin), an org `acme` they own, a
/// project `support-bot`, a session cookie, and one `sk` key per scope set.
pub(crate) struct Seeded {
    /// `ada@example.com`'s `users.id`.
    pub(crate) user_id: Uuid,
    pub(crate) org_id: OrgId,
    pub(crate) project: ProjectRow,
    pub(crate) cookie: String,
    /// `sk-llmobs-…` with `["ingest"]`.
    pub(crate) ingest_key: String,
    /// `sk-llmobs-…` with `["read"]`.
    pub(crate) read_key: String,
    /// `pk-llmobs-…` with `["feedback"]`.
    pub(crate) public_key: String,
}

impl TestApp {
    /// Writes the [`Seeded`] rows directly, without HTTP.
    pub(crate) async fn seed(&self) -> Seeded {
        let now = self.state.clock.now_ns();
        let ingest = secret::new_key(KeyKind::Secret).unwrap();
        let read = secret::new_key(KeyKind::Secret).unwrap();
        let public = secret::new_key(KeyKind::Public).unwrap();
        let ingest_key = ingest.plaintext.clone();
        let read_key = read.plaintext.clone();
        let public_key = public.plaintext.clone();
        let (user_id, org_id, project) = self
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: "ada@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: true,
                        },
                        now,
                    )
                    .await?;
                    let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", now).await?;
                    add_org_member(conn, org.id, user.id, OrgRole::Owner, now).await?;
                    let project =
                        insert_project(conn, org.id, "Support", "support-bot", None, now).await?;
                    let scoped = [
                        (ingest, "secret", vec![KeyScope::Ingest]),
                        (read, "secret", vec![KeyScope::Read]),
                        (public, "public", vec![KeyScope::Feedback]),
                    ];
                    for (key, kind, scopes) in scoped {
                        insert_key(conn, project.id, key, kind, scopes, user.id, now).await?;
                    }
                    Ok((user.id, org.id, project))
                })
            })
            .await
            .unwrap();
        let cookie = session_cookie_via(self, user_id, None).await;
        Seeded {
            user_id,
            org_id,
            project,
            cookie,
            ingest_key,
            read_key,
            public_key,
        }
    }

    /// Another project in `seeded`'s org, with optional retention, and one `sk`
    /// key with `["ingest"]` for it. The key is returned as its plaintext.
    pub(crate) async fn add_project(
        &self,
        seeded: &Seeded,
        slug: &str,
        retention_days: Option<u32>,
    ) -> (ProjectRow, String) {
        let now = self.state.clock.now_ns();
        let ingest = secret::new_key(KeyKind::Secret).unwrap();
        let plaintext = ingest.plaintext.clone();
        let org_id = seeded.org_id;
        let slug = slug.to_owned();
        let user = users::user_by_email(self.state.meta.reader(), "ada@example.com")
            .await
            .unwrap()
            .expect("seeded user exists");
        let user_id = user.id;
        let project = self
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let project =
                        insert_project(conn, org_id, &slug, &slug, retention_days, now).await?;
                    insert_key(
                        conn,
                        project.id,
                        ingest,
                        "secret",
                        vec![KeyScope::Ingest],
                        user_id,
                        now,
                    )
                    .await?;
                    Ok(project)
                })
            })
            .await
            .unwrap();
        (project, plaintext)
    }
}

/// A user added to `seeded`'s org by [`TestApp::add_member`].
#[derive(Debug, Clone)]
pub(crate) struct Member {
    /// `users.id`.
    pub(crate) id: Uuid,
    /// `llmobs_session=…`, ready for a `cookie` header.
    pub(crate) cookie: String,
}

impl TestApp {
    /// Marks `user_id`'s email address verified.
    pub(crate) async fn verify_email(&self, user_id: Uuid) {
        let now = self.state.clock.now_ns();
        self.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_email_verified(conn, user_id, now, EmailVerifiedVia::MailToken).await
                })
            })
            .await
            .unwrap();
    }

    /// A new user `email` with org role `role` in `seeded`'s org (no project override), and a
    /// session cookie for them.
    pub(crate) async fn add_member(&self, seeded: &Seeded, email: &str, role: OrgRole) -> Member {
        member_in_org(self, seeded.org_id, email, role).await
    }

    /// A personal access token for `user_id`, returned as its plaintext: scope `read`, plus
    /// `write` when `write` is true.
    pub(crate) async fn personal_token(&self, user_id: Uuid, write: bool) -> String {
        let minted = secret::new_key(KeyKind::Personal).unwrap();
        let plaintext = minted.plaintext.clone();
        let now = self.state.clock.now_ns();
        let scopes = if write {
            vec![KeyScope::Read, KeyScope::Write]
        } else {
            vec![KeyScope::Read]
        };
        self.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_token(
                        conn,
                        NewToken {
                            user_id,
                            name: "test".to_owned(),
                            prefix: minted.prefix,
                            hash: minted.hash,
                            scopes,
                            expires_at: None,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        plaintext
    }
}

/// The example project's fixed id (`fixtures/api/README.md` § Fixed identifiers).
pub(crate) const EXAMPLE_PROJECT: &str = "01900000-0000-7000-8000-000000000001";

impl TestApp {
    /// The example user, org and project of `fixtures/api/README.md` § Fixed identifiers, with
    /// the project's fixed id `01900000-0000-7000-8000-000000000001`. Returns a session cookie
    /// for `ada@example.com`. Used by `just api-examples` (D-G-25).
    pub(crate) async fn seed_examples(&self) -> String {
        let now = self.state.clock.now_ns();
        let user_id = self
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let user = insert_user(
                        conn,
                        NewUser {
                            email: "ada@example.com".to_owned(),
                            name: Some("Ada Example".to_owned()),
                            password_hash: None,
                            email_verified_at: None,
                            email_verified_via: None,
                            is_instance_admin: true,
                        },
                        now,
                    )
                    .await?;
                    let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", now).await?;
                    add_org_member(conn, org.id, user.id, OrgRole::Owner, now).await?;
                    let project =
                        insert_project(conn, org.id, "Support bot", "support-bot", Some(30), now)
                            .await?;
                    // Nothing references the project yet, so renaming its id is safe.
                    sqlx::query("UPDATE projects SET id = ? WHERE id = ?")
                        .bind(EXAMPLE_PROJECT)
                        .bind(project.id.to_string())
                        .execute(&mut *conn)
                        .await
                        .map_err(MetaError::Db)?;
                    Ok(user.id)
                })
            })
            .await
            .unwrap();
        session_cookie_via(self, user_id, None).await
    }
}

/// The spans of `fixtures/api/dataset/spans.json`, as written.
fn dataset_spans() -> Vec<Span> {
    let dataset = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/api/dataset");
    let spans_text = std::fs::read_to_string(format!("{dataset}/spans.json")).unwrap();
    serde_json::from_str(&spans_text).unwrap()
}

/// The `index`-th span of the example dataset, as written (its `project_id` is the fixture's).
pub(crate) fn dataset_span(index: usize) -> Span {
    dataset_spans()
        .into_iter()
        .nth(index)
        .expect("the dataset has that many spans")
}

/// Every row of `project`, no window: for tests that read storage directly.
pub(crate) async fn raw_query(storage: &Storage, project: ProjectId) -> ProjectQuery {
    storage
        .query(
            project,
            crate::entitlements::query_options(crate::entitlements::Visibility::unlimited(), None),
        )
        .await
        .expect("query opens")
}

/// Spans of `project` in storage, no window: `SELECT count(*) FROM spans` over
/// `QueryContext::new(raw_query(storage, project).await, None)`.
pub(crate) async fn raw_span_count(storage: &Storage, project: ProjectId) -> u64 {
    let ctx = QueryContext::new(raw_query(storage, project).await, None).expect("context");
    let batches = ctx
        .session()
        .sql("SELECT count(*) AS n FROM spans")
        .await
        .expect("count plans")
        .collect()
        .await
        .expect("count runs");
    let count = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .expect("count is Int64")
        .value(0);
    u64::try_from(count).unwrap_or(0)
}

impl TestApp {
    /// Commits one span of `trace` that starts at `start_time`, through [`Self::commit_spans`].
    /// `span_index` makes the span id distinct within the trace.
    pub(crate) async fn commit_trace_at(
        &self,
        project: &ProjectRow,
        trace: TraceId,
        span_index: u8,
        start_time: i64,
    ) {
        let mut span: Span = dataset_span(0);
        span.trace_id = trace;
        span.span_id = format!("c4{span_index:014x}")
            .parse::<SpanId>()
            .expect("span id hex");
        span.parent_span_id = None;
        span.start_time = start_time;
        span.end_time = start_time + 1_000_000_000;
        span.ingest_time = self.state.clock.now_ns();
        self.commit_spans(project, vec![span]).await;
    }

    /// Commits `spans` to `project` through the ingest path: the live bus sees them and the
    /// usage counters count them. The spans' `project_id` is set to `project`.
    pub(crate) async fn commit_spans(&self, project: &ProjectRow, mut spans: Vec<Span>) {
        for span in &mut spans {
            span.project_id = project.id;
        }
        self.state
            .ingest
            .commit(
                &self.state.storage,
                &self.state.usage,
                &self.state.live,
                &self.state.pending,
                crate::ingest::CommitTarget {
                    org_id: project.org_id,
                    project_id: project.id,
                    request_bytes: 0,
                    now_ns: self.state.clock.now_ns(),
                },
                llmobs_otel::Normalized {
                    spans,
                    media: Vec::new(),
                    rejections: llmobs_otel::Rejections::default(),
                },
            )
            .await
            .unwrap();
    }
}

impl TestApp {
    /// Appends `fixtures/api/dataset` (spans and scores) to `project` and stores the PNGs of
    /// `fixtures/api/media`. Every row's `project_id` is set to `project`.
    pub(crate) async fn load_example_dataset(&self, project: ProjectId) {
        let dataset = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/api/dataset");
        let mut spans = dataset_spans();
        for span in &mut spans {
            span.project_id = project;
        }
        let batch = spans_to_record_batch(&spans).unwrap();
        self.state
            .storage
            .append_spans(project, batch)
            .await
            .unwrap();

        let scores_text = std::fs::read_to_string(format!("{dataset}/scores.json")).unwrap();
        let mut scores: Vec<Score> = serde_json::from_str(&scores_text).unwrap();
        for score in &mut scores {
            score.project_id = project;
        }
        let batch = scores_to_record_batch(&scores).unwrap();
        self.state
            .storage
            .append_scores(project, batch)
            .await
            .unwrap();

        let starts = crate::ingest::newest_span_starts(&spans);
        let media_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/api/media");
        for entry in std::fs::read_dir(media_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("png") {
                continue;
            }
            let stem = path.file_stem().and_then(|stem| stem.to_str()).unwrap();
            let sha256: [u8; 32] = hex::decode(stem).unwrap().try_into().unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let newest_span_start = starts.get(stem).copied().unwrap_or(i64::MIN);
            let blob = MediaBlob {
                sha256,
                mime_type: "image/png".to_owned(),
                bytes: Bytes::from(bytes),
                newest_span_start,
            };
            self.state.storage.put_media(project, blob).await.unwrap();
        }
    }
}

async fn insert_key(
    conn: &mut SqliteConnection,
    project_id: ProjectId,
    key: NewKey,
    kind: &'static str,
    scopes: Vec<KeyScope>,
    created_by: Uuid,
    now: i64,
) -> Result<(), MetaError> {
    api_keys::insert_api_key(
        conn,
        NewApiKey {
            project_id,
            kind,
            scopes,
            name: None,
            prefix: key.prefix,
            hash: key.hash,
            created_by,
            expires_at: None,
        },
        now,
    )
    .await?;
    Ok(())
}

/// A session for `user_id`, started through `sso_org`'s provider when given, as a `cookie`
/// header value (`llmobs_session=<token>`): the `Set-Cookie` value up to its first `;`.
pub(crate) async fn session_cookie_via(
    app: &TestApp,
    user_id: Uuid,
    sso_org: Option<OrgId>,
) -> String {
    let header = start_session(
        &app.state,
        user_id,
        None,
        None,
        sso_org.map_or(SessionOrigin::Password, SessionOrigin::OrgProvider),
    )
    .await
    .unwrap();
    header
        .to_str()
        .unwrap()
        .split_once(';')
        .unwrap()
        .0
        .to_owned()
}

fn default_config(data_dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        data_dir: data_dir.to_path_buf(),
        public_url: PublicUrl::default(),
        http_addr: SocketAddr::from(([0, 0, 0, 0], 8080)),
        trusted_proxies: Vec::new(),
        // Tests must not bind the OTLP port (4318).
        otlp_http_addr: None,
        // Tests must not bind the gRPC port (4317).
        grpc_addr: None,
        metrics_addr: None,
        restore_mode: false,
        storage: StorageConfig {
            delete_objects: true,
            data_dir: data_dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig::default(),
            anchor: AnchorConfig::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        },
        ingest: IngestConfig::default(),
        media: MediaConfig::default(),
        eval: EvalConfig::default(),
        security: SecurityConfig::default(),
        // Most tests act as a cloud cell (D-P-50); a test of the default turns it off.
        entitlements: EntitlementsConfig {
            control_plane: true,
            ..EntitlementsConfig::default()
        },
        orgs: OrgsConfig::default(),
        encryption_key: None,
        license_key: None,
        auth: AuthSettings::default(),
        smtp_url: None,
        email_from: None,
        init: InitConfig::default(),
        log: LogConfig::default(),
    }
}

enum OriginChoice {
    Default,
    Override(Option<String>),
}

/// A request against [`TestApp`]. `Origin` defaults to the public origin on
/// methods other than `GET` and `HEAD`.
pub(crate) struct TestRequest {
    router: Router,
    origin_default: String,
    method: Method,
    path: String,
    headers: HeaderMap,
    body: Bytes,
    peer: SocketAddr,
    origin: OriginChoice,
}

impl TestRequest {
    pub(crate) fn json(mut self, value: &impl Serialize) -> Self {
        self.body = Bytes::from(serde_json::to_vec(value).unwrap());
        self.headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        self
    }

    pub(crate) fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
        self
    }

    /// Authenticates as the control plane with a token of `scope` for this request only: its
    /// method and path (D-P-71).
    pub(crate) fn control_plane(self, scope: &str) -> Self {
        let token = control_plane_token(
            scope,
            self.method.as_str(),
            self.path.split('?').next().unwrap_or_default(),
        );
        self.header("authorization", &format!("Bearer {token}"))
    }

    pub(crate) fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    pub(crate) fn peer(mut self, ip: IpAddr) -> Self {
        self.peer.set_ip(ip);
        self
    }

    pub(crate) fn origin(mut self, origin: Option<&str>) -> Self {
        self.origin = OriginChoice::Override(origin.map(str::to_owned));
        self
    }

    pub(crate) async fn send(self) -> TestResponse {
        let (router, request) = self.build();
        let response = router.oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = axum::body::to_bytes(body, 8 * 1024 * 1024)
            .await
            .expect("response body");
        TestResponse {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    /// Sends the request and keeps the body open, for an SSE response. Dropping the returned
    /// [`TestStream`] ends the stream.
    pub(crate) async fn send_stream(self) -> TestStream {
        let (router, request) = self.build();
        let response = router.oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        TestStream {
            status: parts.status,
            headers: parts.headers,
            body,
            buffer: String::new(),
        }
    }

    fn build(self) -> (Router, Request<Body>) {
        let mut builder = Request::builder()
            .method(self.method.clone())
            .uri(&self.path);
        if let Some(origin) = self.origin_value() {
            builder = builder.header(axum::http::header::ORIGIN, origin);
        }
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        let mut request = builder.body(Body::from(self.body.clone())).unwrap();
        request.extensions_mut().insert(ConnectInfo(self.peer));
        (self.router, request)
    }

    fn origin_value(&self) -> Option<String> {
        match &self.origin {
            OriginChoice::Override(value) => value.clone(),
            OriginChoice::Default if self.method == Method::GET || self.method == Method::HEAD => {
                None
            }
            OriginChoice::Default => Some(self.origin_default.clone()),
        }
    }
}

/// Status, headers, and the raw body of one response.
pub(crate) struct TestResponse {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

impl TestResponse {
    pub(crate) fn json<T: DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "json body: {error}: {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub(crate) fn problem(&self) -> Problem {
        self.json()
    }

    /// The header value, when it is a single valid string.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

/// A streaming response: read event by event (D-G-12) or frame by frame. Dropping it ends the
/// stream.
pub(crate) struct TestStream {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    body: Body,
    /// Bytes read from the body that do not yet form a whole event.
    buffer: String,
}

/// One server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) event: String,
    pub(crate) id: Option<String>,
    pub(crate) data: String,
}

impl TestStream {
    /// Reads data frames until the body ends or fails. Returns the bytes and whether the body
    /// ended with an error. Panics when it does not end within `wait`.
    pub(crate) async fn read_to_end(&mut self, wait: Duration) -> (Vec<u8>, bool) {
        let read = async {
            let mut bytes = Vec::new();
            loop {
                match self.body.frame().await {
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            bytes.extend_from_slice(&data);
                        }
                    }
                    Some(Err(_)) => return (bytes, true),
                    None => return (bytes, false),
                }
            }
        };
        tokio::time::timeout(wait, read)
            .await
            .unwrap_or_else(|_| panic!("body did not end"))
    }

    /// The next data frame, or `None` when the body ends, fails, or `wait` passes first.
    pub(crate) async fn next_frame(&mut self, wait: Duration) -> Option<Bytes> {
        let next = async {
            loop {
                match self.body.frame().await {
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            return Some(data);
                        }
                    }
                    Some(Err(_)) | None => return None,
                }
            }
        };
        tokio::time::timeout(wait, next).await.ok().flatten()
    }

    /// The next event, or `None` when `wait` passes first or the body ends.
    pub(crate) async fn next_event(&mut self, wait: Duration) -> Option<SseEvent> {
        tokio::time::timeout(wait, self.read_event())
            .await
            .ok()
            .flatten()
    }

    /// The next event that is not `ping`, within `wait` in total.
    pub(crate) async fn next_non_ping(&mut self, wait: Duration) -> Option<SseEvent> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = self.next_event(remaining).await?;
            if event.event != "ping" {
                return Some(event);
            }
        }
    }

    /// Whether the body ends within `wait`. Frames read on the way are discarded.
    pub(crate) async fn body_closed(&mut self, wait: Duration) -> bool {
        let drain = async {
            loop {
                match self.body.frame().await {
                    None | Some(Err(_)) => return true,
                    Some(Ok(_)) => {}
                }
            }
        };
        tokio::time::timeout(wait, drain).await.unwrap_or(false)
    }

    async fn read_event(&mut self) -> Option<SseEvent> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if let Some(event) = parse_event(&block) {
                    return Some(event);
                }
                continue;
            }
            let frame = self.body.frame().await?.ok()?;
            if let Ok(data) = frame.into_data() {
                self.buffer.push_str(&String::from_utf8_lossy(&data));
            }
        }
    }
}

/// One block of an event stream. A block without `event:` (a comment) is `None`.
fn parse_event(block: &str) -> Option<SseEvent> {
    let mut event = None;
    let mut id = None;
    let mut data = Vec::new();
    for line in block.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("id:") {
            id = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    Some(SseEvent {
        event: event?,
        id,
        data: data.join("\n"),
    })
}

/// Runs `future` with a capturing subscriber and fails if any needle appears.
///
/// The subscriber stays the thread default until `future` finishes, so a task
/// spawned on this thread is captured too. HTTP tests use the current-thread
/// runtime.
pub(crate) async fn assert_logs_clean<T>(needles: &[&str], future: impl Future<Output = T>) -> T {
    let (value, logs) = capture_logs(future).await;
    for needle in needles {
        assert!(!logs.contains(needle), "log contains {needle}: {logs}");
    }
    value
}

/// Installs, once per test binary, a global subscriber that is interested in every callsite
/// and discards everything. Without one, a callsite first registered while no capturing
/// subscriber is the default on any thread caches the interest `never`, and a later capture
/// on another test's thread misses its events (empty logs, flaky asserts).
pub(crate) fn ensure_global_subscriber() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        // An error means a global subscriber already exists, which serves as well.
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry());
    });
}

/// The formatted tracing output produced while `future` is polled.
pub(crate) async fn capture_logs<T>(future: impl Future<Output = T>) -> (T, String) {
    ensure_global_subscriber();
    let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = Arc::clone(&buffer);
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(move || LogBuffer(Arc::clone(&writer)))
        .with_ansi(false)
        .with_target(true);
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(layer));
    let value = {
        let _guard = tracing::dispatcher::set_default(&dispatch);
        future.await
    };
    drop(dispatch);
    let bytes = buffer.lock().expect("log buffer").clone();
    let logs = String::from_utf8_lossy(&bytes).into_owned();
    (value, logs)
}

/// `io::Write` over the shared log buffer. The layer clones the `Arc` per event.
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log buffer").write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A new user `email` with org role `role` in `org`, and a session cookie for them.
pub(crate) async fn member_in_org(app: &TestApp, org: OrgId, email: &str, role: OrgRole) -> Member {
    let now = app.state.clock.now_ns();
    let email = email.to_owned();
    let id = app
        .state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let user = insert_user(
                    conn,
                    NewUser {
                        email,
                        name: None,
                        password_hash: None,
                        email_verified_at: None,
                        email_verified_via: None,
                        is_instance_admin: false,
                    },
                    now,
                )
                .await?;
                add_org_member(conn, org, user.id, role, now).await?;
                Ok(user.id)
            })
        })
        .await
        .unwrap();
    let cookie = session_cookie_via(app, id, None).await;
    Member { id, cookie }
}

/// Saves the org's provider pointing at `idp` (the mock's client id and secret), with the
/// flags given, verifies `domain` in `meta.db`, sets `last_login_at` to now (as if Test
/// sign-in had run), and reloads the routing.
pub(crate) async fn configure_org_sso(
    app: &TestApp,
    org: OrgId,
    idp: &mock_idp::MockIdp,
    domain: &str,
    enabled: bool,
    enforced: bool,
) {
    use crate::api::org_sso::JitRole;
    use crate::ee::org_sso::store::{self, DomainRow, ProviderWrite};
    let id = Uuid::now_v7();
    let sealed = app
        .state
        .secrets
        .seal(mock_idp::CLIENT_SECRET.as_bytes(), store::secret_aad(&id))
        .expect("seal");
    let token = crate::ee::org_sso::dns::new_token().expect("token");
    let now = app.state.clock.now_ns();
    let issuer = idp.issuer().to_owned();
    let domain = domain.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let write = ProviderWrite {
                    display_name: "Acme Okta".to_owned(),
                    issuer,
                    client_id: mock_idp::CLIENT_ID.to_owned(),
                    client_secret_enc: Some(sealed),
                    jit_role: JitRole::Viewer,
                };
                store::put_provider(conn, org, id, write, now).await?;
                store::set_flags(conn, org, enabled, enforced, now).await?;
                store::set_last_login(conn, org, now).await?;
                let row = DomainRow {
                    id: Uuid::now_v7(),
                    org_id: org,
                    domain,
                    token,
                    verified_at: None,
                    last_checked_at: None,
                    created_at: now,
                };
                let domain_id = row.id;
                store::insert_domain(conn, &row).await?;
                store::mark_verified(conn, domain_id, now).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    reload_org_sso(app).await;
}

/// Rebuilds the routing from `meta.db`, the job the server runs after every org SSO write.
pub(crate) async fn reload_org_sso(app: &TestApp) {
    app.state
        .org_sso
        .reload(&app.state.meta, &app.state.secrets, &app.state.config.auth)
        .await
        .expect("org SSO reload");
}

/// Makes `user_id` an instance admin.
pub(crate) async fn make_instance_admin(app: &TestApp, user_id: Uuid) {
    let now = app.state.clock.now_ns();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                users::set_instance_admin(conn, user_id, true, now).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
}

/// A second org `slug` (name = slug) with a new non-admin owner `owner_email` and that owner's
/// session.
pub(crate) async fn second_org(app: &TestApp, slug: &str, owner_email: &str) -> (OrgId, Member) {
    let org = OrgId::new_v7();
    let now = app.state.clock.now_ns();
    let slug = slug.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                insert_org(conn, org, &slug, &slug, now).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    let owner = member_in_org(app, org, owner_email, OrgRole::Owner).await;
    (org, owner)
}

/// An org SSO test app: private egress allowed, a licence listing `org_sso`, `seed()`, and a
/// non-admin owner `owner@acme.test` of the seeded org `acme`.
pub(crate) async fn org_sso_app() -> (TestApp, Seeded, Member) {
    org_sso_app_with(|_| {}).await
}

/// [`org_sso_app`] with `tweak` applied after its settings.
pub(crate) async fn org_sso_app_with(
    tweak: impl FnOnce(&mut ServerConfig),
) -> (TestApp, Seeded, Member) {
    let app = TestApp::with_config(move |config| {
        config.security.allow_private_network_egress = true;
        config.license_key = Some(crate::config::Secret::new(license_key(&["org_sso"])));
        tweak(config);
    })
    .await;
    let seeded = app.seed().await;
    let owner = app
        .add_member(&seeded, "owner@acme.test", OrgRole::Owner)
        .await;
    (app, seeded, owner)
}

/// The OTLP key the tests send with, also the project key of [`with_init`].
pub(crate) const OTLP_KEY: &str = "sk-llmobs-0123456789ABCDEFGHIJKLMNOPQRSTUV";

/// A server config with `init` set, so `bind` seeds an admin, an org, a project and [`OTLP_KEY`].
pub(crate) fn with_init(mut config: ServerConfig) -> ServerConfig {
    config.init = serde_json::from_value(serde_json::json!({
        "admin_email": "ada@example.com",
        "admin_password": "llmobs-ok1",
        "org": "Acme",
        "project": "Support bot",
        "secret_key": OTLP_KEY,
    }))
    .unwrap();
    config
}

/// A config for a server on `data_dir`, on ephemeral loopback ports, with the defaults.
pub(crate) fn config(data_dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        data_dir: data_dir.to_path_buf(),
        public_url: PublicUrl::default(),
        http_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        trusted_proxies: Vec::new(),
        otlp_http_addr: None,
        grpc_addr: None,
        metrics_addr: None,
        restore_mode: false,
        storage: StorageConfig {
            delete_objects: true,
            data_dir: data_dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig::default(),
            anchor: AnchorConfig::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        },
        ingest: IngestConfig::default(),
        media: MediaConfig::default(),
        eval: EvalConfig::default(),
        security: SecurityConfig::default(),
        entitlements: EntitlementsConfig::default(),
        orgs: OrgsConfig::default(),
        encryption_key: None,
        license_key: None,
        auth: AuthSettings::default(),
        smtp_url: None,
        email_from: None,
        init: InitConfig::default(),
        log: LogConfig::default(),
    }
}
