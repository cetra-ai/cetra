//! `cetra serve`: open storage, open `meta.db`, bind, and shut down (D137).
//!
//! Startup creates `data_dir`, checks `meta.db` is not newer (`meta::preflight`), then
//! `Storage::open` (which takes `LOCK`), then `meta.db`. A second process fails on the lock
//! before `meta.db`.
//!
//! Shutdown closes live streams, stops accepting, drains in-flight HTTP for at
//! most [`DRAIN`], waits at most [`USAGE_STOP_WAIT`] for the usage loop to finish
//! any flush in progress, aborts background tasks, waits at most [`COMMIT_WAIT`]
//! for detached commits (D-G-38), and gives storage [`STORAGE_SHUTDOWN`] (spec
//! 12's 30 s flush budget; `Storage::shutdown` also caps each flush at 30 s). These
//! bounded steps add up to 57 s, inside the 60 s grace period the packaging sets. The `last_used` and usage flushes and the `meta.db` close are
//! short meta writes outside that bound.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cetra_otel::{PriceError, PriceTable, builtin_price_rows};
use cetra_storage::Storage;
use cetra_storage::error::OpenError;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::admin_socket;
use crate::auth::Limits;
use crate::clock::Clock;
use crate::config::{ConfigError, ServerConfig};
use crate::crypto::{CryptoError, KeySource, SecretBox, finish_rotation, load_key};
use crate::http::{AppInner, AppState, otlp_router, router};
use crate::ingest::{Ingest, grpc};
use crate::meta::instance_settings::{ENCRYPTION_CHECK, get_setting};
use crate::meta::{MetaDb, MetaError};
use tonic::transport::server::TcpIncoming;

/// How long in-flight HTTP may finish after stop-accepting (spec 12).
const DRAIN: Duration = Duration::from_secs(20);

/// How long shutdown waits for the usage loop to finish an in-flight flush.
const USAGE_STOP_WAIT: Duration = Duration::from_secs(2);

/// How long shutdown waits for detached commits (D-G-38, spec 12).
const COMMIT_WAIT: Duration = Duration::from_secs(5);

/// Best-effort cap on `Storage::shutdown` (spec 12).
const STORAGE_SHUTDOWN: Duration = Duration::from_secs(30);

/// Concurrent HTTP/2 streams per gRPC connection (G-1). A stream holds a reservation before its
/// body is read, so this bounds the requests one client can hold open.
const GRPC_MAX_STREAMS: u32 = 100;

/// Requests in flight per gRPC connection (G-1), across all its streams.
const GRPC_CONCURRENCY_PER_CONNECTION: usize = 32;

/// The time one gRPC request may take, from its headers to its response, body included (G-1,
/// D-G-29). A stream that stalls is cut here, and its reservation goes with the dropped request.
const GRPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The OTLP/gRPC router: the admission layer behind the stream and concurrency limits, with
/// [`GRPC_REQUEST_TIMEOUT`] (or, in tests, a shorter one) on every request.
fn grpc_router(state: AppState, timeout: Duration) -> tonic::transport::server::Router {
    tonic::transport::Server::builder()
        .timeout(timeout)
        .max_concurrent_streams(Some(GRPC_MAX_STREAMS))
        .concurrency_limit_per_connection(GRPC_CONCURRENCY_PER_CONNECTION)
        .add_service(grpc::service(state.clone()))
        .add_service(grpc::logs_service(state.clone()))
        .add_service(grpc::metrics_service(state))
}

/// The Prometheus listener's router: `GET /metrics` and nothing else (N M3, D-N-28). Any other
/// path is axum's 404. There is no authentication, so the docs say to bind it privately.
fn metrics_router(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/metrics", axum::routing::get(metrics))
        .with_state(state)
}

/// The OpenMetrics document, refreshed on each scrape (`telemetry::Telemetry::render`). An encoding
/// failure answers 500 with an empty body, never a partial document.
async fn metrics(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match state.telemetry.render(&state).await {
        Some(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        None => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// How often idle rate-limit keys are dropped (D143).
const PRUNE_EVERY: Duration = Duration::from_secs(60);

/// How often coalesced `last_used_at` values are written (D140).
const LAST_USED_EVERY: Duration = Duration::from_secs(60);

/// How often usage counters are written (D-G-11).
const USAGE_EVERY: Duration = Duration::from_secs(10);

/// How often each audit stream destination's due records are sent (D-EA-11).
const AUDIT_STREAM_EVERY: Duration = Duration::from_secs(10);

/// How often org SSO routing is reloaded from `meta.db` (D-P-62).
const ORG_SSO_EVERY: Duration = Duration::from_secs(60);
/// How often verified domains due for their daily re-proof are checked (D-P-72).
const ORG_SSO_RECHECK_EVERY: Duration = Duration::from_secs(60 * 60);
/// When dropped orgs' usage is first pruned, then how often (D-P-46).
const USAGE_PRUNE_FIRST: Duration = Duration::from_secs(60);
const USAGE_PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// How often the 80 % and 100 % usage notices are checked (D-P-38).
const NOTICES_EVERY: Duration = Duration::from_secs(60);

/// How often expired web sessions are deleted.
const PURGE_EVERY: Duration = Duration::from_secs(60 * 60);

/// How often audit records older than `audit.retention_days` are deleted (D-EA-7).
const AUDIT_PURGE_EVERY: Duration = Duration::from_secs(60 * 60);

/// Why `cetra serve` stopped before it was listening, or the accept loop failed.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// The config file or a `CETRA_*` value was rejected.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// `Storage::open` failed. A held `LOCK` says the data directory is in use.
    #[error("{}", storage_message(.0))]
    Storage(#[source] OpenError),
    /// `meta.db` could not be opened or migrated.
    #[error(transparent)]
    Meta(#[from] MetaError),
    /// The encryption key could not be loaded, or does not match the data directory.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// The HTTP socket could not be bound.
    #[error("binding {addr}")]
    Bind {
        /// The address from `http_addr`.
        addr: SocketAddr,
        /// The bind or `local_addr` error.
        #[source]
        source: io::Error,
    },
    /// Creating `data_dir`, or the accept loop, failed.
    #[error(transparent)]
    Serve(io::Error),
    /// The built-in price table did not compile. A build defect, not a config value.
    #[error("built-in price table")]
    Prices(#[source] PriceError),
    /// Headless init failed a check. The reason names the rule or the variable,
    /// not the password, the key, or the email.
    #[error("init failed: {reason}")]
    Init {
        /// What was wrong.
        reason: String,
    },
}

fn serve_bootstrap(error: crate::bootstrap::BootstrapError) -> ServeError {
    match error {
        crate::bootstrap::BootstrapError::Meta(error) => ServeError::Meta(error),
        crate::bootstrap::BootstrapError::Init { reason } => ServeError::Init { reason },
        crate::bootstrap::BootstrapError::Auth(error) => ServeError::Init {
            reason: error.to_string(),
        },
        crate::bootstrap::BootstrapError::Lock => ServeError::Init {
            reason: "setup token lock poisoned".to_owned(),
        },
    }
}

impl From<OpenError> for ServeError {
    fn from(error: OpenError) -> Self {
        Self::Storage(error)
    }
}

/// `OpenError::Locked` says `data directory is locked: {path}`. The path field
/// is private to the storage crate, so the in-use wording is applied to that
/// sentence. Every other open error keeps its own message.
fn storage_message(error: &OpenError) -> String {
    let text = error.to_string();
    if let Some(rest) = text.strip_prefix("data directory is locked:") {
        format!("data directory is in use:{rest}")
    } else {
        text
    }
}

/// A bound server. [`Self::shutdown`] drains HTTP and releases `LOCK`.
pub struct Running {
    /// The address `bind` actually got, including an ephemeral port.
    pub local_addr: SocketAddr,
    /// The OTLP/HTTP listener's address, when `otlp_http_addr` is set (D-G-21).
    pub otlp_local_addr: Option<SocketAddr>,
    /// The OTLP/gRPC listener's address, when `grpc_addr` is set (D-G-21).
    pub grpc_local_addr: Option<SocketAddr>,
    /// The Prometheus listener's address, when `metrics_addr` is set (N M3, D-N-28).
    pub metrics_local_addr: Option<SocketAddr>,
    serve: Option<JoinHandle<io::Result<()>>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    otlp_serve: Option<JoinHandle<io::Result<()>>>,
    otlp_shutdown_tx: Option<oneshot::Sender<()>>,
    grpc_serve: Option<JoinHandle<io::Result<()>>>,
    grpc_shutdown_tx: Option<oneshot::Sender<()>>,
    metrics_serve: Option<JoinHandle<io::Result<()>>>,
    metrics_shutdown_tx: Option<oneshot::Sender<()>>,
    usage_stop_tx: Option<oneshot::Sender<()>>,
    usage_task: Option<JoinHandle<()>>,
    state: Option<AppState>,
    /// The control socket's accept task, when it bound (D-N-17). Shutdown stops it
    /// first, before the HTTP drain.
    admin_socket: Option<JoinHandle<()>>,
    /// Background work. Shutdown aborts whatever is here.
    tasks: Vec<JoinHandle<()>>,
    /// The eval loops. Shutdown stops them before the storage flush (D-I-12).
    eval_tasks: Option<crate::eval::EvalTasks>,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Running")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(serve) = self.serve.take() {
            serve.abort();
        }
        if let Some(serve) = self.otlp_serve.take() {
            serve.abort();
        }
        if let Some(serve) = self.grpc_serve.take() {
            serve.abort();
        }
        if let Some(serve) = self.metrics_serve.take() {
            serve.abort();
        }
        if let Some(task) = &self.usage_task {
            task.abort();
        }
        if let Some(socket) = self.admin_socket.take() {
            socket.abort();
        }
        for task in &self.tasks {
            task.abort();
        }
        if let Some(eval) = &self.eval_tasks {
            eval.abort();
        }
    }
}

/// Open the data directory, bind `config.http_addr`, and serve until `shutdown`.
/// `ui` answers the main listener's non-API paths; the OTLP listeners never get it.
///
/// # Errors
///
/// Returns a [`ServeError`] from opening storage or `meta.db`, from binding,
/// or from the accept loop.
pub async fn run(
    config: ServerConfig,
    ui: Option<axum::Router>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), ServeError> {
    // Boxed: `bind`'s future is large, and the CLI awaits this one (clippy's `large_futures`).
    let running = Box::pin(bind(config, ui)).await?;
    shutdown.await;
    running.shutdown().await
}

/// Create `data_dir`, open storage and `meta.db`, and start accepting. See [`run`] for `ui`.
///
/// # Errors
///
/// Returns a [`ServeError`] when the directory, the lock, `meta.db`, or the
/// socket fails. Storage is shut down before a later step's error is returned,
/// so `LOCK` is not held after `Err`.
pub async fn bind(config: ServerConfig, ui: Option<axum::Router>) -> Result<Running, ServeError> {
    let data_dir = config.data_dir.clone();
    let http_addr = config.http_addr;
    let otlp_http_addr = config.otlp_http_addr;
    let grpc_addr = config.grpc_addr;
    let metrics_addr = config.metrics_addr;
    if let Err(source) = tokio::fs::create_dir_all(&data_dir).await {
        return Err(directory_error(&data_dir, &source));
    }
    // Before storage: a newer meta.db stops here, before LOCK, the WAL or the bucket is touched.
    crate::meta::preflight(&data_dir).await?;
    let storage = Storage::open(config.storage_config()).await?;
    let meta = match MetaDb::open(&data_dir).await {
        Ok(meta) => meta,
        Err(error) => {
            release_storage(storage).await;
            return Err(error.into());
        }
    };
    if let Some(demo_org) = config.demo.org {
        let row = match crate::meta::orgs::org_by_id(meta.reader(), demo_org).await {
            Ok(row) => row,
            Err(error) => {
                release_storage(storage).await;
                meta.close().await;
                return Err(ServeError::Meta(error));
            }
        };
        let reason = match row {
            None => Some("no org has this id"),
            Some(row) if row.state != "active" => Some("the org is not active"),
            Some(_) => None,
        };
        if let Some(reason) = reason {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Config(ConfigError::Invalid {
                key: "demo.org",
                reason,
            }));
        }
    }
    let secrets = match open_secrets(&config, &meta).await {
        Ok(secrets) => secrets,
        Err(error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(error.into());
        }
    };
    let eval_runner = match cetra_eval::EvalRunner::new(cetra_eval::EgressPolicy {
        allow_private_network: config.security.allow_private_network_egress,
        restore_mode: config.restore_mode,
    }) {
        Ok(runner) => Arc::new(runner),
        Err(_error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Config(ConfigError::Invalid {
                key: "security",
                reason: "could not build the evaluation HTTP client",
            }));
        }
    };
    let prices = match PriceTable::new(builtin_price_rows()) {
        Ok(prices) => prices,
        Err(error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Prices(error));
        }
    };
    let prices = match crate::prices::Prices::load(meta.reader(), prices).await {
        Ok(prices) => prices,
        Err(error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Meta(error));
        }
    };
    let ingest = Ingest::new(&config.ingest, &config.media, Arc::new(prices));
    let telemetry = crate::telemetry::Telemetry::new(config.restore_mode);
    let mailer = match crate::email::Mailer::from_config(&config, telemetry.emails.clone()) {
        Ok(mailer) => mailer.map(Arc::new),
        Err(_error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Config(ConfigError::Invalid {
                key: "smtp_url",
                reason: "could not build the mailer",
            }));
        }
    };
    let providers = match crate::auth::oidc::identity_client()
        .and_then(|http| crate::auth::oidc::Providers::for_config(&config, &http))
    {
        Ok(providers) => providers,
        Err(error) => {
            release_storage(storage).await;
            meta.close().await;
            let reason = match error {
                crate::auth::oidc::OidcError::Client => {
                    "could not build the identity provider client"
                }
                crate::auth::oidc::OidcError::Unavailable
                | crate::auth::oidc::OidcError::Invalid => {
                    "identity provider configuration is invalid"
                }
            };
            return Err(ServeError::Config(ConfigError::Invalid {
                key: "auth",
                reason,
            }));
        }
    };
    let oauth_key = match crate::auth::oidc::random_key() {
        Ok(key) => key,
        Err(_error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Config(ConfigError::Invalid {
                key: "auth",
                reason: "could not generate the oauth state key",
            }));
        }
    };
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
    let defaults = crate::entitlements::Defaults::from_config(&config.entitlements.defaults);
    let entitlements = match crate::entitlements::EntitlementsState::load(
        &meta,
        license,
        defaults,
        config.public_url.origin(),
        config.entitlements.control_plane,
    )
    .await
    {
        Ok(entitlements) => entitlements,
        Err(error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Meta(error));
        }
    };
    let org_sso = match crate::ee::org_sso::OrgSso::new(
        cetra_eval::EgressPolicy {
            allow_private_network: config.security.allow_private_network_egress,
            restore_mode: config.restore_mode,
        },
        txt_resolver(&config),
    ) {
        Ok(org_sso) => org_sso,
        Err(error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Init {
                reason: error.to_string(),
            });
        }
    };
    let reloaded = match org_sso.reload(&meta, &secrets, &config.auth).await {
        Ok(reloaded) => reloaded,
        Err(error) => {
            release_storage(storage).await;
            meta.close().await;
            return Err(ServeError::Meta(error));
        }
    };
    if reloaded.claimed > 0 {
        tracing::error!(
            domains = reloaded.claimed,
            "verified org SSO domains are ignored: the server configuration lists them"
        );
    }
    let storage = Arc::new(storage);
    let eval = crate::eval::EvalQueue::new(config.eval.workers);
    let issue_workers = Arc::new(tokio::sync::Semaphore::new(
        usize::try_from(config.issues.workers).unwrap_or(usize::MAX),
    ));
    let limits = Limits::new(&config.demo);
    let state = AppState::new(AppInner {
        config,
        meta,
        storage,
        ready: crate::http::health::ReadyCache::default(),
        clock: Clock::System,
        limits,
        device: crate::auth::device::DeviceStore::new(),
        key_cache: crate::auth::principal::ApiKeyCache::new(),
        pat_cache: crate::auth::principal::PatCache::new(),
        last_used: crate::auth::last_used::LastUsed::new(),
        token_last_used: crate::auth::last_used::LastUsed::tokens(),
        scim_cache: crate::auth::principal::ScimCache::new(),
        scim_last_used: crate::auth::last_used::LastUsed::scim_tokens(),
        setup: crate::bootstrap::SetupState::new(),
        password_hasher: tokio::sync::OnceCell::new(),
        mailer,
        providers,
        oauth_key,
        ingest,
        last_ingest: crate::ingest::last_ingest::LastIngest::default(),
        live: crate::live::LiveBus::new(crate::live::StreamTiming::DEFAULT),
        widget_cache: crate::routes::dashboards::WidgetCache::default(),
        usage: crate::usage::Usage::default(),
        alert_email_count: crate::alerts::quota::DailyEmailCount::default(),
        deletions: crate::deletion::Deletions::new(),
        secrets,
        eval_runner,
        connections: crate::eval::connection::ConnectionCache::default(),
        entitlements,
        ingest_rate: crate::entitlements::rate::IngestRate::default(),
        trace_loads: crate::eval::trace_load::TraceLoads::default(),
        definitions: crate::eval::definitions::DefinitionCache::default(),
        org_sso,
        backup_slot: tokio::sync::Mutex::new(()),
        eval,
        pending: crate::eval::pending::PendingTraces::default(),
        telemetry,
        issue_score_rules: crate::issues::triggers::ScoreRules::default(),
        issue_workers,
        issue_wake: tokio::sync::Notify::new(),
    });
    crate::ee::sso_enforcement::warn_if_unlicensed(&state);
    if let Err(error) = crate::bootstrap::prepare(&state).await {
        release_state(state).await;
        return Err(serve_bootstrap(error));
    }
    if let Err(error) = crate::eval::recover(&state).await {
        release_state(state).await;
        return Err(ServeError::Meta(error));
    }
    // Losing the load only stops tracking of the traces that were pending, so startup goes on.
    if let Err(error) = crate::eval::trigger::load(&state).await {
        tracing::warn!(error = %error, "pending traces could not be loaded; tracking starts empty");
    }
    // Losing the load only loses the contexts already posted; hooks post again at the next prompt.
    match crate::meta::agent_contexts::load_contexts(state.meta.reader()).await {
        Ok(rows) => state.ingest.contexts().load(rows),
        Err(error) => {
            tracing::warn!(error = %error, "agent contexts could not be loaded; starting empty");
        }
    }
    // The guard is a safety net for judge runs; the server starts without it.
    if let Err(error) = state
        .deletions
        .guard
        .load(&state.meta, state.clock.now_ns())
        .await
    {
        tracing::error!(error = %error, "deletion guard load failed");
    }
    // Every socket is bound before any server starts. A failure drops the sockets
    // already bound and releases the state, so `LOCK` is free after `Err`.
    let (listener, local_addr) = match bind_listener(http_addr).await {
        Ok(bound) => bound,
        Err(error) => {
            release_state(state).await;
            return Err(error);
        }
    };
    let otlp = match otlp_http_addr {
        None => None,
        Some(addr) => match bind_listener(addr).await {
            Ok(bound) => Some(bound),
            Err(error) => {
                release_state(state).await;
                return Err(error);
            }
        },
    };
    let grpc = match grpc_addr {
        None => None,
        Some(addr) => match bind_listener(addr).await {
            Ok(bound) => Some(bound),
            Err(error) => {
                release_state(state).await;
                return Err(error);
            }
        },
    };
    let metrics = match metrics_addr {
        None => None,
        Some(addr) => match bind_listener(addr).await {
            Ok(bound) => Some(bound),
            Err(error) => {
                release_state(state).await;
                return Err(error);
            }
        },
    };
    let mut tasks = vec![
        spawn_prune(state.clone()),
        spawn_purge(state.clone()),
        spawn_last_used(state.clone()),
        crate::assistant::spawn_expiry(state.clone()),
    ];
    tasks.extend(crate::alerts::spawn_loops(
        &state,
        crate::alerts::AlertTiming::DEFAULT,
    ));
    let eval_tasks = crate::eval::spawn(&state, crate::eval::EvalTiming::DEFAULT);
    tasks.push(spawn_deletions(state.clone()));
    tasks.push(spawn_retention(state.clone()));
    tasks.push(crate::agent_context::spawn_agent_context_sweep(
        state.clone(),
    ));
    let (usage_stop_tx, usage_stop_rx) = oneshot::channel();
    let usage_task = spawn_usage(state.clone(), usage_stop_rx);
    tasks.push(spawn_usage_prune(state.clone()));
    // Usage notices send email, and the org-SSO recheck looks up DNS, which restore mode does
    // not do (N M3, D-N-25). `spawn_org_sso` only reloads from `meta.db`, so it keeps running.
    if !state.config.restore_mode {
        tasks.push(spawn_notices(state.clone()));
        tasks.push(spawn_org_sso_recheck(state.clone()));
        tasks.push(spawn_audit_purge(state.clone()));
        tasks.push(spawn_audit_stream(state.clone()));
    }
    tasks.push(spawn_org_sso(state.clone()));
    if state.config.restore_mode {
        tracing::warn!(
            "restore mode: outbound work is off (alerts, webhooks, judges, email, identity providers)"
        );
    }
    let admin_socket = admin_socket::spawn(state.clone(), &data_dir);
    tracing::info!(%local_addr, "listening");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let app = router(state.clone(), ui).into_make_service_with_connect_info::<SocketAddr>();
    let serve = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });
    let (otlp_local_addr, otlp_serve, otlp_shutdown_tx) = match otlp {
        None => (None, None, None),
        Some((listener, addr)) => {
            tracing::info!(otlp_http_local_addr = %addr, "listening");
            let (tx, rx) = oneshot::channel();
            let app =
                otlp_router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
            let serve = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = rx.await;
                    })
                    .await
            });
            (Some(addr), Some(serve), Some(tx))
        }
    };
    let (grpc_local_addr, grpc_serve, grpc_shutdown_tx) = match grpc {
        None => (None, None, None),
        Some((listener, addr)) => {
            tracing::info!(grpc_local_addr = %addr, "listening");
            let (tx, rx) = oneshot::channel();
            let router = grpc_router(state.clone(), GRPC_REQUEST_TIMEOUT);
            let serve = tokio::spawn(async move {
                router
                    .serve_with_incoming_shutdown(TcpIncoming::from(listener), async move {
                        let _ = rx.await;
                    })
                    .await
                    .map_err(io::Error::other)
            });
            (Some(addr), Some(serve), Some(tx))
        }
    };
    let (metrics_local_addr, metrics_serve, metrics_shutdown_tx) = match metrics {
        None => (None, None, None),
        Some((listener, addr)) => {
            tracing::info!(metrics_local_addr = %addr, "listening");
            let (tx, rx) = oneshot::channel();
            let app = metrics_router(state.clone());
            let serve = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = rx.await;
                    })
                    .await
            });
            (Some(addr), Some(serve), Some(tx))
        }
    };
    Ok(Running {
        local_addr,
        otlp_local_addr,
        grpc_local_addr,
        metrics_local_addr,
        serve: Some(serve),
        shutdown_tx: Some(shutdown_tx),
        otlp_serve,
        otlp_shutdown_tx,
        grpc_serve,
        grpc_shutdown_tx,
        metrics_serve,
        metrics_shutdown_tx,
        usage_stop_tx: Some(usage_stop_tx),
        usage_task: Some(usage_task),
        state: Some(state),
        admin_socket,
        tasks,
        eval_tasks: Some(eval_tasks),
    })
}

/// The DNS resolver of org-SSO domain verification. Restore mode looks nothing up (N M3, D-N-25).
fn txt_resolver(config: &ServerConfig) -> crate::ee::org_sso::dns::TxtResolver {
    if config.restore_mode {
        crate::ee::org_sso::dns::TxtResolver::Unconfigured
    } else {
        crate::ee::org_sso::dns::TxtResolver::system()
    }
}

/// Finishes an interrupted rotation, loads the key, then checks it (D-I-1, D-I-3).
async fn open_secrets(config: &ServerConfig, meta: &MetaDb) -> Result<SecretBox, CryptoError> {
    let check = get_setting(meta.reader(), ENCRYPTION_CHECK).await?;
    let env = config
        .encryption_key
        .as_ref()
        .map(|key| key.expose().as_str());
    if env.is_none() {
        finish_rotation(&config.data_dir, check.as_deref())?;
    }
    let loaded = load_key(env, &config.data_dir, check.is_some())?;
    let secrets = SecretBox::new(&loaded.key);
    crate::crypto::verify_or_record(meta, &secrets, &loaded.path).await?;
    if loaded.source == KeySource::Generated {
        tracing::warn!(
            path = %loaded.path.display(),
            "generated an encryption key; back this file up with the data directory"
        );
    }
    Ok(secrets)
}

/// Binds `addr` and returns the listener with the address it actually got.
async fn bind_listener(addr: SocketAddr) -> Result<(TcpListener, SocketAddr), ServeError> {
    let bind_error = |source| ServeError::Bind { addr, source };
    let listener = TcpListener::bind(addr).await.map_err(bind_error)?;
    let local_addr = listener.local_addr().map_err(bind_error)?;
    Ok((listener, local_addr))
}

fn spawn_purge(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(PURGE_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            let purged = state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        crate::meta::web_sessions::purge_expired_sessions(conn, now).await
                    })
                })
                .await;
            match purged {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "purged expired sessions");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(error = %error, "session purge failed");
                }
            }
        }
    })
}

fn spawn_audit_purge(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(AUDIT_PURGE_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            let purged = crate::audit::retention::purge(&state, now).await;
            match purged {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "purged old audit records");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(error = %error, "audit purge failed");
                }
            }
        }
    })
}

fn spawn_audit_stream(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(AUDIT_STREAM_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            if let Err(error) = crate::ee::audit::stream::deliver_once(&state, now).await {
                tracing::error!(error = %error, "audit stream delivery failed");
            }
        }
    })
}

fn spawn_last_used(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(LAST_USED_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = state.last_used.flush(&state.meta).await {
                tracing::error!(error = %error, "last_used flush failed");
            }
            if let Err(error) = state.token_last_used.flush(&state.meta).await {
                tracing::error!(error = %error, "last_used flush failed");
            }
            if let Err(error) = state.scim_last_used.flush(&state.meta).await {
                tracing::error!(error = %error, "last_used flush failed");
            }
        }
    })
}

fn spawn_deletions(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = crate::deletion::run_once(&state).await {
                tracing::error!(error = %error, "deletion run failed");
            }
            tokio::select! {
                () = state.deletions.wake.notified() => {}
                () = tokio::time::sleep(crate::deletion::TICK) => {}
            }
        }
    })
}

fn spawn_retention(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(crate::retention::FIRST_RUN).await;
        loop {
            match crate::retention::retention_once(&state).await {
                Ok(run) if run.files > 0 || run.failures > 0 => {
                    tracing::info!(
                        projects = run.projects,
                        failures = run.failures,
                        files = run.files,
                        "retention run"
                    );
                }
                Ok(_) => {}
                Err(error) => tracing::error!(error = %error, "retention run failed"),
            }
            tokio::time::sleep(crate::retention::EVERY).await;
        }
    })
}

/// The usage flush loop. A send on `stop` (or dropping it) ends the loop. The flush runs in the
/// branch body rather than inside the `select!`, so a stop that arrives during a flush waits for
/// that flush to commit; it is never cancelled mid-write (D-G-11).
fn spawn_usage(state: AppState, mut stop: oneshot::Receiver<()>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(USAGE_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(error) = state.usage.flush(&state.meta).await {
                        tracing::error!(error = %error, "usage flush failed");
                    }
                }
                _ = &mut stop => break,
            }
        }
    })
}

fn spawn_org_sso(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(ORG_SSO_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            crate::ee::org_sso::refresh(&state).await;
        }
    })
}

fn spawn_org_sso_recheck(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(ORG_SSO_RECHECK_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match crate::ee::org_sso::recheck::recheck_once(&state, state.clock.now_ns()).await {
                Ok(run) if run.lapsed > 0 || run.failed > 0 => tracing::info!(
                    kept = run.kept,
                    lapsed = run.lapsed,
                    failed = run.failed,
                    "org sso domain recheck"
                ),
                Ok(_) => {}
                Err(error) => tracing::warn!(error = %error, "org sso domain recheck failed"),
            }
        }
    })
}

fn spawn_usage_prune(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(USAGE_PRUNE_FIRST).await;
        loop {
            match crate::usage::prune_dropped_orgs(&state.meta, state.clock.now_ns()).await {
                Ok(0) => {}
                Ok(rows) => tracing::info!(rows, "dropped orgs' usage pruned"),
                Err(error) => tracing::warn!(error = %error, "usage prune failed"),
            }
            tokio::time::sleep(USAGE_PRUNE_EVERY).await;
        }
    })
}

fn spawn_notices(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(NOTICES_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match crate::entitlements::notices::notices_once(&state).await {
                Ok(0) => {}
                Ok(count) => {
                    tracing::info!(notices = count, "usage notices recorded");
                }
                Err(error) => tracing::warn!(error = %error, "usage notices failed"),
            }
        }
    })
}

fn spawn_prune(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(PRUNE_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = state.clock.now_ns();
            state.limits.prune(now);
            state.device.sweep(now);
        }
    })
}

impl Running {
    /// The server state, while the server runs.
    #[cfg(test)]
    pub(crate) fn state(&self) -> &AppState {
        self.state
            .as_ref()
            .expect("state is present until shutdown")
    }

    /// Stop the control socket, stop accepting, drain HTTP, stop the eval loops and background
    /// tasks, wait for detached commits, flush the pending traces, the last-used stamps and the
    /// usage counters, flush storage, close `meta.db`.
    ///
    /// The metrics listener is told to stop first, before the control socket. It drains with the
    /// HTTP listeners, under the same bound, so a scrape during the drain does not add time. The
    /// control socket goes next: a backup in progress is cancelled, and no
    /// new one starts during the drain. A storage flush error is logged. The lock
    /// is released inside `Storage::shutdown` before that error, and this method
    /// still closes `meta.db`.
    ///
    /// # Errors
    ///
    /// Returns the accept loop's error when it failed before the drain finished.
    pub async fn shutdown(mut self) -> Result<(), ServeError> {
        if let Some(sender) = self.metrics_shutdown_tx.take() {
            let _ = sender.send(());
        }
        if let Some(state) = &self.state {
            state.live.close();
        }
        if let Some(socket) = self.admin_socket.take() {
            socket.abort();
            let _ = socket.await;
        }
        if let Some(state) = &self.state
            && let Err(error) = admin_socket::remove_socket_file(&state.config.data_dir)
        {
            tracing::warn!(error = %error, "could not remove the admin socket");
        }
        if let Some(sender) = self.shutdown_tx.take() {
            let _ = sender.send(());
        }
        if let Some(sender) = self.otlp_shutdown_tx.take() {
            let _ = sender.send(());
        }
        if let Some(sender) = self.grpc_shutdown_tx.take() {
            let _ = sender.send(());
        }
        let (serve_result, otlp_result, grpc_result, metrics_result) = tokio::join!(
            drain_optional(self.serve.take()),
            drain_optional(self.otlp_serve.take()),
            drain_optional(self.grpc_serve.take()),
            drain_optional(self.metrics_serve.take()),
        );
        let serve_error = serve_result
            .or(otlp_result)
            .or(grpc_result)
            .or(metrics_result);
        // The usage loop stops cooperatively, so an in-flight flush commits before the final one.
        if let Some(sender) = self.usage_stop_tx.take() {
            let _ = sender.send(());
        }
        // Bounded so a stuck `meta.db` cannot hold shutdown past the grace period. On expiry the
        // task is aborted, and an aborted flush keeps its counts for the final flush.
        if let Some(mut task) = self.usage_task.take() {
            match tokio::time::timeout(USAGE_STOP_WAIT, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::error!(error = %error, "usage task failed"),
                Err(_elapsed) => {
                    tracing::error!("usage task did not stop before shutdown");
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        if let (Some(eval), Some(state)) = (self.eval_tasks.take(), self.state.as_ref()) {
            eval.shutdown(state).await;
        }
        for task in self.tasks.drain(..) {
            task.abort();
            let _ = task.await;
        }
        if let Some(state) = self.state.take() {
            // Commits whose request was dropped still run (D-G-38): they append and count
            // before the final flushes.
            if tokio::time::timeout(COMMIT_WAIT, state.ingest.wait_for_commits())
                .await
                .is_err()
            {
                tracing::error!("ingest commits did not finish before shutdown");
            }
            if let Err(error) = crate::eval::trigger::flush_all(&state).await {
                tracing::error!(error = %error, "pending traces flush failed");
            }
            if let Err(error) = state.last_used.flush(&state.meta).await {
                tracing::error!(error = %error, "last_used flush failed");
            }
            if let Err(error) = state.token_last_used.flush(&state.meta).await {
                tracing::error!(error = %error, "last_used flush failed");
            }
            if let Err(error) = state.scim_last_used.flush(&state.meta).await {
                tracing::error!(error = %error, "last_used flush failed");
            }
            if let Err(error) = state.usage.flush(&state.meta).await {
                tracing::error!(error = %error, "usage flush failed");
            }
            match tokio::time::timeout(STORAGE_SHUTDOWN, state.storage.shutdown()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!(error = %error, "storage shutdown");
                }
                Err(_elapsed) => {
                    tracing::error!("storage shutdown timed out");
                }
            }
            close_meta(state).await;
        }
        match serve_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

async fn drain_optional(serve: Option<JoinHandle<io::Result<()>>>) -> Option<ServeError> {
    match serve {
        Some(serve) => drain(serve).await,
        None => None,
    }
}

async fn drain(serve: JoinHandle<io::Result<()>>) -> Option<ServeError> {
    let abort = serve.abort_handle();
    match tokio::time::timeout(DRAIN, serve).await {
        Ok(Ok(Ok(()))) => None,
        Ok(Ok(Err(error))) => Some(ServeError::Serve(error)),
        Ok(Err(_join)) => Some(ServeError::Serve(io::Error::other(
            "http server task stopped",
        ))),
        Err(_elapsed) => {
            abort.abort();
            None
        }
    }
}

async fn close_meta(state: AppState) {
    match state.into_inner() {
        Ok(inner) => inner.meta.close().await,
        Err(state) => state.meta.reader().close().await,
    }
}

async fn release_storage(storage: Storage) {
    if let Err(error) = storage.shutdown().await {
        tracing::error!(error = %error, "storage shutdown");
    }
}

async fn release_state(state: AppState) {
    if let Err(error) = state.storage.shutdown().await {
        tracing::error!(error = %error, "storage shutdown");
    }
    close_meta(state).await;
}

fn directory_error(path: &Path, source: &io::Error) -> ServeError {
    ServeError::Serve(io::Error::new(
        source.kind(),
        format!("creating data directory {}: {source}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use bytesize::ByteSize;
    use cetra_otel::decode_json;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
    use tonic::codec::CompressionEncoding;

    use super::*;
    use crate::config::Secret;
    use crate::crypto::{decode_key, encode_key, random_key, write_key_file};
    use crate::meta::instance_settings::put_setting;
    use crate::meta::orgs::{insert_org, mark_org_deleting};
    use crate::test_support::{OTLP_KEY, config, with_init};
    use cetra_core::ids::OrgId;

    /// Writes one org into `meta.db` under `dir` before `bind` opens it. `deleting` takes it out of `active`.
    async fn org_in_meta(dir: &Path, deleting: bool) -> OrgId {
        let meta = MetaDb::open(dir).await.unwrap();
        let id = OrgId::new_v7();
        meta.write(move |conn| {
            Box::pin(async move {
                insert_org(conn, id, "Cetra demo", "cetra-demo", 1).await?;
                if deleting {
                    mark_org_deleting(conn, id, 2).await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();
        meta.close().await;
        id
    }

    #[tokio::test]
    async fn a_demo_org_that_names_no_org_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.demo.org = Some(OrgId::new_v7());
        let error = bind(config, None).await.unwrap_err();
        assert!(
            matches!(
                error,
                ServeError::Config(ConfigError::Invalid {
                    key: "demo.org",
                    reason: "no org has this id",
                })
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_demo_org_that_is_not_active_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let id = org_in_meta(dir.path(), true).await;
        let mut config = config(dir.path());
        config.demo.org = Some(id);
        let error = bind(config, None).await.unwrap_err();
        assert!(
            matches!(
                error,
                ServeError::Config(ConfigError::Invalid {
                    key: "demo.org",
                    reason: "the org is not active",
                })
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn an_active_demo_org_starts() {
        let dir = tempfile::tempdir().unwrap();
        let id = org_in_meta(dir.path(), false).await;
        let mut config = config(dir.path());
        config.demo.org = Some(id);
        bind(config, None).await.unwrap().shutdown().await.unwrap();
    }

    #[test]
    fn restore_mode_uses_no_dns() {
        let dir = tempfile::tempdir().unwrap();
        let mut restore = config(dir.path());
        restore.restore_mode = true;
        assert!(matches!(
            txt_resolver(&restore),
            crate::ee::org_sso::dns::TxtResolver::Unconfigured
        ));
    }

    /// Restore mode starts six fewer loops in `Running.tasks`: the two alert loops that send
    /// (evaluation and delivery), usage notices (email), the org-SSO recheck (DNS), the audit
    /// purge (a restored copy keeps its records unchanged) and the audit stream (outbound
    /// records). The eval dispatcher and backfill runner live in `eval_tasks`, which is not
    /// counted here.
    #[tokio::test]
    async fn restore_mode_skips_the_sending_and_dns_loops() {
        let dir = tempfile::tempdir().unwrap();
        let normal = bind(config(dir.path()), None).await.unwrap();
        let normal_tasks = normal.tasks.len();
        normal.shutdown().await.unwrap();
        let mut restore = config(dir.path());
        restore.restore_mode = true;
        let restored = bind(restore, None).await.unwrap();
        let restore_tasks = restored.tasks.len();
        restored.shutdown().await.unwrap();
        assert_eq!(normal_tasks - restore_tasks, 6);
    }

    #[tokio::test]
    async fn restore_mode_logs_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mut restore = config(dir.path());
        restore.restore_mode = true;
        let (running, logs) =
            crate::test_support::capture_logs(Box::pin(bind(restore, None))).await;
        let running = running.unwrap();
        assert!(
            logs.contains("restore mode: outbound work is off"),
            "{logs}"
        );
        let body = http_get(running.local_addr, "/healthz", "close").await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restore_mode_opens_storage_without_object_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let mut restore = config(dir.path());
        restore.restore_mode = true;
        let (running, logs) =
            crate::test_support::capture_logs(Box::pin(bind(restore, None))).await;
        running.unwrap().shutdown().await.unwrap();
        assert!(
            logs.contains("object deletion is off (restore mode)"),
            "{logs}"
        );

        let dir = tempfile::tempdir().unwrap();
        let (running, logs) =
            crate::test_support::capture_logs(Box::pin(bind(config(dir.path()), None))).await;
        running.unwrap().shutdown().await.unwrap();
        assert!(
            !logs.contains("object deletion is off (restore mode)"),
            "{logs}"
        );
    }

    #[tokio::test]
    async fn an_empty_database_logs_the_setup_url_once() {
        let dir = tempfile::tempdir().unwrap();
        let (running, logs) =
            crate::test_support::capture_logs(Box::pin(bind(config(dir.path()), None))).await;
        let running = running.unwrap();
        assert_eq!(
            logs.matches("setup required: open http://localhost:8080/setup?token=")
                .count(),
            1,
            "{logs}"
        );
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_newer_meta_db_stops_serve_before_storage_opens() {
        use sqlx::{ConnectOptions as _, Connection as _};
        let dir = tempfile::tempdir().unwrap();
        MetaDb::open(dir.path()).await.unwrap().close().await;
        let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("meta.db"))
            .connect()
            .await
            .unwrap();
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let before = crate::meta::test_digest::dir_digest(dir.path());

        let error = bind(config(dir.path()), None).await.unwrap_err();
        assert!(error.to_string().contains("format version 2"), "{error}");
        assert!(!dir.path().join("catalog.db").exists());
        assert!(!dir.path().join("wal").exists());
        assert!(!dir.path().join("LOCK").exists());
        assert_eq!(crate::meta::test_digest::dir_digest(dir.path()), before);
    }

    #[tokio::test]
    async fn a_bad_init_secret_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.init = serde_json::from_value(serde_json::json!({
            "admin_email": "ada@example.com",
            "admin_password": "cetra-ok-1",
            "org": "Acme",
            "project": "Support bot",
            "secret_key": "not-a-secret-key",
        }))
        .unwrap();
        let error = bind(config, None).await.unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, ServeError::Init { .. }), "{text}");
        assert!(text.contains("CETRA_INIT_SECRET_KEY"), "{text}");
        assert!(!text.contains("not-a-secret-key"), "{text}");
    }

    #[tokio::test]
    async fn an_init_read_key_on_a_used_data_dir_is_ignored() {
        use crate::auth::secret::{KeyKind, new_key};
        let dir = tempfile::tempdir().unwrap();
        let init = serde_json::json!({
            "admin_email": "ada@example.com",
            "admin_password": "cetra-ok-1",
            "org": "Acme",
            "project": "Support bot",
        });
        let mut first = config(dir.path());
        first.init = serde_json::from_value(init.clone()).unwrap();
        bind(first, None).await.unwrap().shutdown().await.unwrap();

        let read = new_key(KeyKind::Secret).unwrap();
        let mut later = config(dir.path());
        let mut with_key = init;
        with_key["read_key"] = serde_json::Value::String(read.plaintext.clone());
        later.init = serde_json::from_value(with_key).unwrap();
        let running = bind(later, None).await.unwrap();
        let state = running.state.as_ref().unwrap().clone();
        let keys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys")
            .fetch_one(state.meta.reader())
            .await
            .unwrap();
        assert_eq!(keys, 0);
        drop(state);
        running.shutdown().await.unwrap();

        // A malformed value is not checked either: init does not run.
        let mut bad = config(dir.path());
        bad.init = serde_json::from_value(serde_json::json!({
            "admin_email": "ada@example.com",
            "admin_password": "cetra-ok-1",
            "org": "Acme",
            "project": "Support bot",
            "read_key": "not-a-key",
        }))
        .unwrap();
        bind(bad, None).await.unwrap().shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_wrong_encryption_key_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = config(dir.path());
        first.encryption_key = Some(Secret::new(encode_key(&[1; 32])));
        let running = bind(first, None).await.unwrap();
        running.shutdown().await.unwrap();

        let mut second = config(dir.path());
        second.encryption_key = Some(Secret::new(encode_key(&[2; 32])));
        let error = bind(second, None).await.unwrap_err();
        assert!(matches!(error, ServeError::Crypto(_)), "{error}");

        // The failed start released the lock, and the first key still works.
        let mut third = config(dir.path());
        third.encryption_key = Some(Secret::new(encode_key(&[1; 32])));
        let running = bind(third, None).await.unwrap();
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn startup_finishes_an_interrupted_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        running.shutdown().await.unwrap();

        // An interrupted rotation: the database already uses the new key.
        let new = random_key().unwrap();
        write_key_file(&dir.path().join("secret.key.new"), &new).unwrap();
        let check = SecretBox::new(&new).check_value().to_owned();
        let meta = MetaDb::open(dir.path()).await.unwrap();
        meta.write(move |conn| {
            Box::pin(async move { put_setting(conn, ENCRYPTION_CHECK, &check).await })
        })
        .await
        .unwrap();
        meta.close().await;

        let running = bind(config(dir.path()), None).await.unwrap();
        running.shutdown().await.unwrap();
        let rotated = std::fs::read_to_string(dir.path().join("secret.key")).unwrap();
        assert_eq!(decode_key(rotated.trim_end(), "test").unwrap(), new);
        assert!(!dir.path().join("secret.key.new").exists());
    }

    #[tokio::test]
    async fn startup_removes_a_stale_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        running.shutdown().await.unwrap();
        let before = std::fs::read_to_string(dir.path().join("secret.key")).unwrap();
        write_key_file(&dir.path().join("secret.key.new"), &random_key().unwrap()).unwrap();

        let running = bind(config(dir.path()), None).await.unwrap();
        running.shutdown().await.unwrap();
        assert!(!dir.path().join("secret.key.new").exists());
        let after = std::fs::read_to_string(dir.path().join("secret.key")).unwrap();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn a_lost_key_file_fails_startup() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        running.shutdown().await.unwrap();
        std::fs::remove_file(dir.path().join("secret.key")).unwrap();

        let error = bind(config(dir.path()), None).await.unwrap_err();
        assert!(
            matches!(error, ServeError::Crypto(CryptoError::Missing { .. })),
            "{error}"
        );
    }

    #[tokio::test]
    async fn healthz_over_tcp_and_shutdown_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let body = http_get(running.local_addr, "/healthz", "close").await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.contains("\"status\":\"ok\""), "{text}");
        running.shutdown().await.unwrap();

        let again = bind(config(dir.path()), None).await.unwrap();
        again.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_second_bind_stops_on_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let error = bind(config(dir.path()), None).await.unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, ServeError::Storage(_)), "{text}");
        assert!(text.contains("in use"), "{text}");
        assert!(text.contains(&dir.path().display().to_string()), "{text}");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_dropped_running_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        drop(running);
        // The aborted tasks drop their state a few polls later, so retry for a short while.
        let started = Instant::now();
        loop {
            match bind(config(dir.path()), None).await {
                Ok(again) => {
                    again.shutdown().await.unwrap();
                    break;
                }
                Err(error) => {
                    assert!(started.elapsed() < Duration::from_secs(5), "{error}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    #[tokio::test]
    async fn idle_keepalive_shuts_down_within_the_drain() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let mut stream = TcpStream::connect(running.local_addr).await.unwrap();
        let request = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        stream.write_all(request).await.unwrap();
        let body = read_response(&mut stream).await;
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");

        let started = Instant::now();
        running.shutdown().await.unwrap();
        assert!(started.elapsed() <= DRAIN + Duration::from_secs(2));
    }

    async fn http_get(addr: SocketAddr, path: &str, connection: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: {connection}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        read_response(&mut stream).await
    }

    async fn read_response(stream: &mut TcpStream) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0; 2048];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let read = tokio::time::timeout(remaining, stream.read(&mut chunk))
                .await
                .expect("timed out waiting for the response");
            let count = read.expect("read");
            assert!(count > 0, "connection closed");
            buf.extend_from_slice(&chunk[..count]);
            if response_complete(&buf) {
                return buf;
            }
        }
    }

    fn response_complete(buf: &[u8]) -> bool {
        let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = &buf[..end];
        let body = &buf[end + 4..];
        let mut length = None;
        for line in headers.split(|&byte| byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Ok(line) = std::str::from_utf8(line) else {
                continue;
            };
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().ok();
            }
        }
        match length {
            Some(length) => body.len() >= length,
            None => !body.is_empty(),
        }
    }

    #[tokio::test]
    async fn shutdown_flushes_usage() {
        use crate::meta::MetaDb;
        use cetra_core::ids::{OrgId, ProjectId};

        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let state = running.state.as_ref().unwrap().clone();
        let project = ProjectId::new_v7();
        // Counted less than 10 s before shutdown, so only the shutdown flush can write it.
        state
            .usage
            .add(OrgId::new_v7(), project, state.clock.now_ns(), 3, 0, 0);
        running.shutdown().await.unwrap();
        let meta = MetaDb::open(dir.path()).await.unwrap();
        let spans: i64 = sqlx::query_scalar("SELECT spans FROM usage_daily WHERE project_id = ?")
            .bind(project.to_string())
            .fetch_one(meta.reader())
            .await
            .unwrap();
        assert_eq!(spans, 3);
    }

    #[tokio::test]
    async fn the_usage_loop_stops_when_signalled() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let state = running.state.as_ref().unwrap().clone();
        let (stop_tx, stop_rx) = oneshot::channel();
        let task = spawn_usage(state, stop_rx);
        stop_tx.send(()).unwrap();
        // Not aborted: the loop returns by itself, well before its 10 s interval.
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the loop stops on its signal")
            .unwrap();
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_flushes_the_map() {
        use crate::meta::MetaDb;
        use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
        use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
        use cetra_core::ids::{ProjectId, SpanId};

        let dir = tempfile::tempdir().unwrap();
        let running = bind(with_init(config(dir.path())), None).await.unwrap();
        let state = running.state.as_ref().unwrap().clone();
        let project_text: String = sqlx::query_scalar("SELECT id FROM projects")
            .fetch_one(state.meta.reader())
            .await
            .unwrap();
        let project = ProjectId::from_uuid(uuid::Uuid::parse_str(&project_text).unwrap());
        // A trace rule keeps the project tracked, as the sweeper's per-tick refresh would.
        let evaluator = uuid::Uuid::now_v7();
        let rule = uuid::Uuid::now_v7();
        let now = state.clock.now_ns();
        state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_evaluator(
                        conn,
                        evaluator,
                        project,
                        "check",
                        EvaluatorFields {
                            name: "Same".to_owned(),
                            config: "{}".to_owned(),
                            output_score: "{}".to_owned(),
                            connection_id: None,
                            secret_enc: None,
                            version: 1,
                        },
                        None,
                        now,
                    )
                    .await?;
                    insert_rule(
                        conn,
                        NewRule {
                            id: rule,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        RuleFields {
                            name: "Every trace".to_owned(),
                            filter: None,
                            variable_mapping: "{}".to_owned(),
                            sample_rate: 1.0,
                            budget_usd: None,
                            budget_period: None,
                            enabled: true,
                        },
                        now,
                    )
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        crate::eval::trigger::refresh_tracked(&state).await.unwrap();
        // A sweep that read the rules before the insert may still hold a stale tracked set. Every
        // tick that starts after this sleep tracks the project, so refreshing again is enough.
        tokio::time::sleep(crate::eval::EvalTiming::DEFAULT.sweep_every * 2).await;
        crate::eval::trigger::refresh_tracked(&state).await.unwrap();
        // An unrooted span: the trace stays pending for the 60 s timeout, so nothing completes it.
        let mut span = crate::test_support::dataset_span(0);
        span.parent_span_id = Some(SpanId::from_bytes([0, 0, 0, 0, 0, 0, 0, 1]).unwrap());
        span.environment = None;
        span.metadata.clear();
        state
            .pending
            .observe(project, &[span], state.clock.now_ns());
        running.shutdown().await.unwrap();
        let meta = MetaDb::open(dir.path()).await.unwrap();
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM pending_traces WHERE project_id = ?")
                .bind(project.to_string())
                .fetch_one(meta.reader())
                .await
                .unwrap();
        assert_eq!(rows, 1);
        meta.close().await;
    }

    #[tokio::test]
    async fn contexts_survive_a_restart() {
        use crate::meta::agent_contexts::{AgentContextRow, upsert_context};
        use cetra_core::ids::ProjectId;

        let dir = tempfile::tempdir().unwrap();
        let running = bind(with_init(config(dir.path())), None).await.unwrap();
        let state = running.state.as_ref().unwrap().clone();
        let project_text: String = sqlx::query_scalar("SELECT id FROM projects")
            .fetch_one(state.meta.reader())
            .await
            .unwrap();
        let project = ProjectId::from_uuid(uuid::Uuid::parse_str(&project_text).unwrap());
        let row = AgentContextRow {
            project_id: project,
            agent: "codex".to_owned(),
            session_id: "s1".to_owned(),
            repository: Some("github.com/acme/app".to_owned()),
            branch: Some("main".to_owned()),
            user_id: None,
            updated_at: state.clock.now_ns(),
        };
        state
            .meta
            .write(move |conn| {
                Box::pin(async move { upsert_context(conn, &row).await.map(|_| ()) })
            })
            .await
            .unwrap();
        running.shutdown().await.unwrap();

        let running = bind(config(dir.path()), None).await.unwrap();
        let state = running.state.as_ref().unwrap().clone();
        let contexts = state
            .ingest
            .contexts()
            .project(project)
            .expect("the project's contexts are loaded before the listeners open");
        let branch = contexts
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("codex", "s1")
            .map(|context| context.branch.clone());
        assert_eq!(branch, Some(Some("main".to_owned())));
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_ingest_request_over_tcp_is_counted_by_the_shutdown_flush() {
        use crate::meta::MetaDb;

        let dir = tempfile::tempdir().unwrap();
        let running = bind(with_init(config(dir.path())), None).await.unwrap();
        let accepted = post_traces(running.local_addr, OTLP_KEY).await;
        assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");
        running.shutdown().await.unwrap();
        let meta = MetaDb::open(dir.path()).await.unwrap();
        let spans: i64 = sqlx::query_scalar("SELECT spans FROM usage_daily")
            .fetch_one(meta.reader())
            .await
            .unwrap();
        assert!(spans > 0, "{spans}");
        meta.close().await;
    }

    #[tokio::test]
    async fn shutdown_flushes_last_used() {
        use crate::auth::roles::{KeyScope, OrgRole};
        use crate::meta::MetaDb;
        use crate::meta::api_keys::{NewApiKey, insert_api_key};
        use crate::meta::members::add_org_member;
        use crate::meta::orgs::insert_org;
        use crate::meta::projects::insert_project;
        use crate::meta::users::{NewUser, insert_user};

        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let state = running.state.as_ref().unwrap().clone();
        let now = state.clock.now_ns();
        let key_id = state
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
                    let key = insert_api_key(
                        conn,
                        NewApiKey {
                            project_id: project.id,
                            kind: "secret",
                            scopes: vec![KeyScope::Read],
                            name: None,
                            prefix: "sk-cetra-abcdef".to_owned(),
                            hash: "ab".repeat(32),
                            created_by: user.id,
                            expires_at: None,
                        },
                        now,
                    )
                    .await?;
                    Ok(key.id)
                })
            })
            .await
            .unwrap();
        state.last_used.record(key_id, now);
        running.shutdown().await.unwrap();
        let meta = MetaDb::open(dir.path()).await.unwrap();
        let stored: Option<i64> =
            sqlx::query_scalar("SELECT last_used_at FROM api_keys WHERE id = ?")
                .bind(key_id.to_string())
                .fetch_one(meta.reader())
                .await
                .unwrap();
        assert_eq!(stored, Some(now));
        meta.close().await;
    }

    const TRACES: &str =
        include_str!("../../../fixtures/otlp/genai/finish-reasons-fill/input.json");

    async fn send_raw(addr: SocketAddr, request: &[u8]) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request).await.unwrap();
        String::from_utf8_lossy(&read_response(&mut stream).await).into_owned()
    }

    async fn post_traces(addr: SocketAddr, key: &str) -> String {
        let request = format!(
            "POST /v1/traces HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer {key}\r\nConnection: close\r\n\r\n{TRACES}",
            TRACES.len()
        );
        send_raw(addr, request.as_bytes()).await
    }

    #[tokio::test]
    async fn otlp_port_takes_traces_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut both = with_init(config(dir.path()));
        both.otlp_http_addr = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        let running = bind(both, None).await.unwrap();
        let otlp = running.otlp_local_addr.expect("the OTLP listener is bound");
        assert_ne!(otlp, running.local_addr);

        let accepted = post_traces(otlp, OTLP_KEY).await;
        assert!(accepted.starts_with("HTTP/1.1 200"), "{accepted}");

        let health = send_raw(
            otlp,
            b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(health.starts_with("HTTP/1.1 404"), "{health}");

        let on_main = post_traces(running.local_addr, OTLP_KEY).await;
        assert!(on_main.starts_with("HTTP/1.1 200"), "{on_main}");
        running.shutdown().await.unwrap();
    }

    fn test_ui() -> axum::Router {
        axum::Router::new().fallback(|| async { "ui" })
    }

    #[tokio::test]
    async fn the_ui_answers_the_main_listener_but_not_api_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut both = config(dir.path());
        both.otlp_http_addr = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        let running = bind(both, Some(test_ui())).await.unwrap();

        let page = http_get(running.local_addr, "/some/page", "close").await;
        let text = String::from_utf8_lossy(&page);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(text.ends_with("ui"), "{text}");

        for path in ["/apiary", "/v1x"] {
            let page = http_get(running.local_addr, path, "close").await;
            let text = String::from_utf8_lossy(&page);
            assert!(text.starts_with("HTTP/1.1 200"), "{path}: {text}");
            assert!(text.ends_with("ui"), "{path}: {text}");
        }

        for path in ["/api", "/api/v1/nope", "/v1/nope", "/mcp/x"] {
            let missing = http_get(running.local_addr, path, "close").await;
            let text = String::from_utf8_lossy(&missing);
            assert!(text.starts_with("HTTP/1.1 404"), "{path}: {text}");
            assert!(text.contains("application/problem+json"), "{path}: {text}");
        }

        let health = http_get(running.local_addr, "/healthz", "close").await;
        assert!(String::from_utf8_lossy(&health).starts_with("HTTP/1.1 200"));

        let otlp = running.otlp_local_addr.expect("the OTLP listener is bound");
        let on_otlp = send_raw(
            otlp,
            b"GET /some/page HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(on_otlp.starts_with("HTTP/1.1 404"), "{on_otlp}");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn without_a_ui_unknown_paths_are_problems() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let missing = http_get(running.local_addr, "/some/page", "close").await;
        let text = String::from_utf8_lossy(&missing);
        assert!(text.starts_with("HTTP/1.1 404"), "{text}");
        assert!(text.contains("application/problem+json"), "{text}");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn no_otlp_address_binds_one_listener() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        assert!(running.otlp_local_addr.is_none());
        assert!(running.grpc_local_addr.is_none());
        running.shutdown().await.unwrap();
    }

    fn with_grpc(data_dir: &Path, tweak: impl FnOnce(&mut ServerConfig)) -> ServerConfig {
        let mut config = with_init(config(data_dir));
        config.grpc_addr = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        tweak(&mut config);
        config
    }

    /// `TRACES` as an export request, with the ingest key in `authorization`.
    fn authorized(message: ExportTraceServiceRequest) -> tonic::Request<ExportTraceServiceRequest> {
        let mut request = tonic::Request::new(message);
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {OTLP_KEY}").parse().expect("ASCII token"),
        );
        request
    }

    #[tokio::test]
    async fn grpc_export_over_tcp_plain_and_gzip() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(with_grpc(dir.path(), |_| {}), None).await.unwrap();
        let addr = running.grpc_local_addr.expect("the gRPC listener is bound");
        let message = decode_json(TRACES.as_bytes()).expect("fixture decodes");

        let mut plain = TraceServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        plain
            .export(authorized(message.clone()))
            .await
            .expect("plain export is accepted");

        let mut gzip = TraceServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap()
            .send_compressed(CompressionEncoding::Gzip);
        gzip.export(authorized(message))
            .await
            .expect("gzip export is accepted");
        drop(plain);
        drop(gzip);
        running.shutdown().await.unwrap();
    }

    /// `TRACES` with a 4 KiB attribute, which is over a 1 KiB `max_grpc_message_bytes`.
    fn oversized() -> ExportTraceServiceRequest {
        let mut message = decode_json(TRACES.as_bytes()).expect("fixture decodes");
        let padding = KeyValue {
            key: "pad".to_owned(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue("x".repeat(4096))),
            }),
            ..Default::default()
        };
        message.resource_spans[0].scope_spans[0].spans[0]
            .attributes
            .push(padding);
        message
    }

    #[tokio::test]
    async fn grpc_refuses_a_message_over_the_configured_size() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(
            with_grpc(dir.path(), |config| {
                config.ingest.max_grpc_message_bytes = ByteSize::kib(1);
            }),
            None,
        )
        .await
        .unwrap();
        let addr = running.grpc_local_addr.expect("the gRPC listener is bound");

        let mut client = TraceServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        let status = client
            .export(authorized(oversized()))
            .await
            .expect_err("the message is over the limit");
        assert_eq!(status.code(), tonic::Code::OutOfRange, "{status:?}");
        drop(client);
        running.shutdown().await.unwrap();
    }

    /// A gRPC export whose headers arrive and whose body never does (G-1, D-G-29): the admitted
    /// reservation is held while it stalls, and the request timeout cuts it and gives it back.
    #[tokio::test]
    async fn a_stalled_grpc_export_is_cut_and_releases_its_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let budget = ByteSize::mib(10);
        let running = bind(
            with_grpc(dir.path(), |config| {
                config.ingest.max_grpc_message_bytes = budget;
                config.ingest.max_inflight_bytes = budget;
            }),
            None,
        )
        .await
        .unwrap();
        let state = running.state.clone().expect("the server is running");
        // A second listener with a short timeout: the default one is 30 s.
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let serve =
            tokio::spawn(
                grpc_router(state.clone(), Duration::from_millis(300))
                    .serve_with_incoming_shutdown(TcpIncoming::from(listener), async move {
                        let _ = stop_rx.await;
                    }),
            );

        // The channel, with the body swapped for one that never yields: headers go out, then
        // nothing, so the server waits in the body read while holding its reservation.
        let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let stalled = tower::ServiceBuilder::new()
            .map_request(|request: axum::http::Request<tonic::body::Body>| {
                let (parts, _) = request.into_parts();
                let never =
                    futures::stream::pending::<Result<http_body::Frame<bytes::Bytes>, io::Error>>();
                let body = http_body_util::StreamBody::new(never);
                axum::http::Request::from_parts(parts, tonic::body::Body::new(body))
            })
            .service(channel);
        let mut client = TraceServiceClient::new(stalled);
        let export = tokio::spawn(async move {
            client
                .export(authorized(decode_json(TRACES.as_bytes()).unwrap()))
                .await
        });

        // While it stalls, the budget is held.
        let mut held = false;
        for _ in 0..500 {
            if state.ingest.reserve_exact(budget.as_u64()).is_err() {
                held = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(held, "the stalled export holds its reservation");

        // The timeout cuts it, and the reservation comes back.
        let outcome = tokio::time::timeout(Duration::from_secs(10), export)
            .await
            .expect("the stalled export is cut")
            .unwrap();
        assert!(outcome.is_err(), "a cut export is not accepted");
        let mut freed = false;
        for _ in 0..500 {
            if state.ingest.reserve_exact(budget.as_u64()).is_ok() {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(freed, "the reservation is released after the timeout");

        let _ = stop_tx.send(());
        serve.await.unwrap().unwrap();
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_unauthenticated_oversized_message_is_refused_before_it_is_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(
            with_grpc(dir.path(), |config| {
                config.ingest.max_grpc_message_bytes = ByteSize::kib(1);
            }),
            None,
        )
        .await
        .unwrap();
        let addr = running.grpc_local_addr.expect("the gRPC listener is bound");

        let mut client = TraceServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        let status = client
            .export(tonic::Request::new(oversized()))
            .await
            .expect_err("no credentials");
        assert_eq!(status.code(), tonic::Code::Unauthenticated, "{status:?}");
        assert_eq!(status.message(), "missing or invalid credentials");
        drop(client);
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_authenticated_export_is_busy_while_the_budget_is_held() {
        let dir = tempfile::tempdir().unwrap();
        // The smallest budget config accepts: at least `max_request_bytes` (10 MiB by default).
        let budget = ByteSize::mib(10);
        let running = bind(
            with_grpc(dir.path(), |config| {
                config.ingest.max_grpc_message_bytes = budget;
                config.ingest.max_inflight_bytes = budget;
            }),
            None,
        )
        .await
        .unwrap();
        let addr = running.grpc_local_addr.expect("the gRPC listener is bound");
        // One reservation of the whole budget: the next admission cannot reserve its message.
        let state = running.state.as_ref().expect("the server is running");
        let held = state
            .ingest
            .reserve_exact(budget.as_u64())
            .expect("the budget is free");

        let mut client = TraceServiceClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        let message = decode_json(TRACES.as_bytes()).expect("fixture decodes");
        let status = client
            .export(authorized(message))
            .await
            .expect_err("the budget is held");
        assert_eq!(status.code(), tonic::Code::Unavailable, "{status:?}");
        assert_eq!(status.message(), "server is busy, retry later");
        drop(held);
        drop(client);
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn empty_grpc_address_binds_no_grpc_listener() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        config.grpc_addr = None;
        let running = bind(config, None).await.unwrap();
        assert!(running.grpc_local_addr.is_none());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn metrics_listener_serves_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mut metrics = config(dir.path());
        metrics.metrics_addr = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        let running = bind(metrics, None).await.unwrap();
        let addr = running.metrics_local_addr.expect("metrics listener");

        let scrape = http_get(addr, "/metrics", "close").await;
        let text = String::from_utf8_lossy(&scrape);
        assert!(text.starts_with("HTTP/1.1 200"), "{text}");
        assert!(
            text.to_ascii_lowercase().contains(
                "content-type: application/openmetrics-text; version=1.0.0; charset=utf-8"
            ),
            "{text}"
        );
        assert!(text.contains("cetra_build_info"), "{text}");

        let other = http_get(addr, "/", "close").await;
        assert!(
            String::from_utf8_lossy(&other).starts_with("HTTP/1.1 404"),
            "the metrics listener serves only /metrics"
        );

        let main = http_get(running.local_addr, "/metrics", "close").await;
        let main_text = String::from_utf8_lossy(&main);
        assert!(!main_text.contains("cetra_build_info"), "{main_text}");

        running.shutdown().await.unwrap();
        assert!(TcpStream::connect(addr).await.is_err());
    }

    #[tokio::test]
    async fn empty_otlp_address_in_toml_binds_one_listener() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cetra.toml");
        std::fs::write(
            &path,
            format!(
                "data_dir = \"{}\"\nhttp_addr = \"127.0.0.1:0\"\notlp_http_addr = \"\"\ngrpc_addr = \"\"\n",
                dir.path().display()
            ),
        )
        .unwrap();
        let loaded = ServerConfig::load(Some(&path)).unwrap();
        assert_eq!(loaded.otlp_http_addr, None);
        let running = bind(loaded, None).await.unwrap();
        assert!(running.otlp_local_addr.is_none());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_occupied_otlp_port_fails_bind_and_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken_addr = taken.local_addr().unwrap();
        let mut clash = config(dir.path());
        clash.otlp_http_addr = Some(taken_addr);
        let error = bind(clash, None).await.unwrap_err();
        let text = error.to_string();
        assert!(
            matches!(error, ServeError::Bind { addr, .. } if addr == taken_addr),
            "{text}"
        );
        assert!(text.contains(&taken_addr.to_string()), "{text}");
        drop(taken);

        let again = bind(config(dir.path()), None).await.unwrap();
        again.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_with_both_listeners_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut both = config(dir.path());
        both.otlp_http_addr = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        both.grpc_addr = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        let running = bind(both, None).await.unwrap();
        assert!(running.otlp_local_addr.is_some());
        assert!(running.grpc_local_addr.is_some());
        running.shutdown().await.unwrap();

        let again = bind(config(dir.path()), None).await.unwrap();
        again.shutdown().await.unwrap();
    }
}
