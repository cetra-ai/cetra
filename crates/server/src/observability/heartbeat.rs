//! The daily anonymous heartbeat (D-TL-8): a fixed type whose every value is a constant, a bucket or a boolean. It never carries emails, domains, names, URLs, host names, ids of orgs or projects, exact counts or licence state, and this module reads nothing of the licence code (D172).

use std::time::Duration;

use cetra_storage::config::ObjectStoreUrl;
use chrono::DateTime;
use serde::Serialize;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::http::AppState;
use crate::meta::instance_settings::{TELEMETRY_LAST_HEARTBEAT_AT, get_setting, put_setting};
use crate::meta::{MetaDb, MetaError};
use crate::observability::buckets::{
    CountBucket, InstallAgeBucket, UptimeBucket, VolumeBucket, count_bucket, install_age_bucket,
    uptime_bucket, volume_bucket,
};
use crate::observability::facts::{self, Deployment, Profile};
use crate::observability::instance;
use crate::observability::switchboard::{Decision, NOTICE_TARGET, Switch};

/// The payload's schema version (TL-Q1: the relay refuses other versions).
pub(crate) const HEARTBEAT_SCHEMA: u32 = 1;

/// Where flushed Parquet files live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StorageKind {
    /// `file://`.
    File,
    /// `s3://`.
    S3,
}

/// Which features the instance uses. M5 adds `crash_reports_on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Features {
    /// `smtp_url` is set.
    pub(crate) email: bool,
    /// `[auth.github]` is set.
    pub(crate) github_sign_in: bool,
    /// `[auth.google]` is set.
    pub(crate) google_sign_in: bool,
    /// At least one `[[auth.oidc]]` provider.
    pub(crate) oidc_sign_in: bool,
    /// An alert rule exists.
    pub(crate) alert_rules: bool,
    /// An online evaluation rule exists.
    pub(crate) online_rules: bool,
    /// A dataset exists.
    pub(crate) datasets: bool,
    /// A prompt exists.
    pub(crate) prompts: bool,
    /// An annotation queue exists.
    pub(crate) annotation_queues: bool,
    /// cetra exports its own telemetry over OTLP (`self_monitoring.exports()`).
    pub(crate) self_monitoring: bool,
}

/// The heartbeat body (D-TL-8). Field order is the JSON key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Heartbeat {
    /// [`HEARTBEAT_SCHEMA`].
    pub(crate) schema: u32,
    /// `instance_settings['telemetry.instance_id']`.
    pub(crate) instance_id: Uuid,
    /// [`facts::VERSION`].
    pub(crate) version: &'static str,
    /// [`facts::TARGET`].
    pub(crate) target: &'static str,
    /// [`facts::profile`].
    pub(crate) profile: Profile,
    /// [`facts::deployment`].
    pub(crate) deployment: Deployment,
    /// From `storage.object_store`.
    pub(crate) storage: StorageKind,
    /// Time since the process started ([`facts::process_uptime`]).
    pub(crate) uptime: UptimeBucket,
    /// Time since the first user was created.
    pub(crate) install_age: InstallAgeBucket,
    /// Active orgs.
    pub(crate) orgs: CountBucket,
    /// Active projects.
    pub(crate) projects: CountBucket,
    /// Users not disabled.
    pub(crate) users: CountBucket,
    /// Spans of yesterday (UTC) in `usage_daily`, every org.
    pub(crate) spans_per_day: VolumeBucket,
    /// Scores of yesterday (UTC) in `usage_daily`, every org.
    pub(crate) scores_per_day: VolumeBucket,
    /// Feature booleans.
    pub(crate) features: Features,
}

/// Whether each feature table has a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TableFlags {
    /// `alert_rules` has a row.
    pub(crate) alert_rules: bool,
    /// `online_rules` has a row.
    pub(crate) online_rules: bool,
    /// `datasets` has a row.
    pub(crate) datasets: bool,
    /// `prompts` has a row.
    pub(crate) prompts: bool,
    /// `annotation_queues` has a row.
    pub(crate) annotation_queues: bool,
}

/// `storage.object_store` as a [`StorageKind`]: S3 when set to an S3 bucket, else files.
pub(crate) fn storage_kind(config: &ServerConfig) -> StorageKind {
    match config.storage.object_store_url() {
        ObjectStoreUrl::Local(_) => StorageKind::File,
        ObjectStoreUrl::S3 { .. } => StorageKind::S3,
    }
}

/// The feature booleans of `config` and the table flags.
pub(crate) fn features(config: &ServerConfig, tables: TableFlags) -> Features {
    Features {
        email: config.smtp_url.is_some(),
        github_sign_in: config.auth.github.is_some(),
        google_sign_in: config.auth.google.is_some(),
        oidc_sign_in: !config.auth.oidc.is_empty(),
        alert_rules: tables.alert_rules,
        online_rules: tables.online_rules,
        datasets: tables.datasets,
        prompts: tables.prompts,
        annotation_queues: tables.annotation_queues,
        self_monitoring: config.self_monitoring.exports(),
    }
}

/// Whether each feature table has at least one row.
async fn table_flags(meta: &MetaDb) -> Result<TableFlags, MetaError> {
    let reader = meta.reader();
    Ok(TableFlags {
        alert_rules: sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM alert_rules)")
            .fetch_one(reader)
            .await?,
        online_rules: sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM online_rules)")
            .fetch_one(reader)
            .await?,
        datasets: sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM datasets)")
            .fetch_one(reader)
            .await?,
        prompts: sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM prompts)")
            .fetch_one(reader)
            .await?,
        annotation_queues: sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM annotation_queues)",
        )
        .fetch_one(reader)
        .await?,
    })
}

/// The heartbeat as it would be sent now. Reads `meta.db` only; M3's debug bundle reuses it.
///
/// # Errors
///
/// [`MetaError`] when a query fails.
pub(crate) async fn collect(
    meta: &MetaDb,
    config: &ServerConfig,
    instance_id: Uuid,
    deployment: Deployment,
    uptime: Duration,
    now_ns: i64,
) -> Result<Heartbeat, MetaError> {
    let reader = meta.reader();
    let orgs = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM orgs WHERE state = 'active'")
        .fetch_one(reader)
        .await?;
    let projects =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM projects WHERE state = 'active'")
            .fetch_one(reader)
            .await?;
    let users =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users WHERE disabled_at IS NULL")
            .fetch_one(reader)
            .await?;
    let today = DateTime::from_timestamp_nanos(now_ns).date_naive();
    // `NaiveDate::MIN` has no predecessor; that is never reached for a real clock.
    let yesterday = today.pred_opt().unwrap_or(today);
    let day = yesterday.format("%Y-%m-%d").to_string();
    let (spans, scores) = sqlx::query_as::<_, (i64, i64)>(
        "SELECT COALESCE(SUM(spans), 0), COALESCE(SUM(scores), 0) FROM usage_daily WHERE day = ?",
    )
    .bind(day)
    .fetch_one(reader)
    .await?;
    let first_user = sqlx::query_scalar::<_, Option<i64>>("SELECT MIN(created_at) FROM users")
        .fetch_one(reader)
        .await?;
    // Zero when there is no user, or when the clock is behind the first user.
    let age = Duration::from_nanos(
        first_user
            .and_then(|first| u64::try_from(now_ns.saturating_sub(first)).ok())
            .unwrap_or(0),
    );
    let flags = table_flags(meta).await?;
    // A negative count cannot happen; 0 is the safe side if it does.
    let count = |value: i64| u64::try_from(value).unwrap_or(0);
    Ok(Heartbeat {
        schema: HEARTBEAT_SCHEMA,
        instance_id,
        version: facts::VERSION,
        target: facts::TARGET,
        profile: facts::profile(),
        deployment,
        storage: storage_kind(config),
        uptime: uptime_bucket(uptime),
        install_age: install_age_bucket(age),
        orgs: count_bucket(count(orgs)),
        projects: count_bucket(count(projects)),
        users: count_bucket(count(users)),
        spans_per_day: volume_bucket(count(spans)),
        scores_per_day: volume_bucket(count(scores)),
        features: features(config, flags),
    })
}

/// The first heartbeat waits this long after start, plus up to [`MAX_JITTER`].
pub(crate) const FIRST_DELAY: Duration = Duration::from_secs(10 * 60);
/// The most random delay added to [`FIRST_DELAY`].
pub(crate) const MAX_JITTER: Duration = Duration::from_secs(10 * 60);
/// One heartbeat a day.
pub(crate) const PERIOD: Duration = Duration::from_secs(24 * 60 * 60);
/// Each request gives up after this long.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The page that lists every field, logged in the notice's `docs` field.
pub(crate) const TELEMETRY_DOCS_URL: &str = "https://cetra.sh/docs/self-hosting/telemetry/";

/// Why a heartbeat was not sent. Logged at `debug` only; no variant carries a URL or a body.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HeartbeatError {
    /// The HTTP client could not be built.
    #[error("heartbeat client could not be built")]
    Client,
    /// The request failed before a response (connection, TLS, timeout).
    #[error("heartbeat request failed (timeout: {timeout})")]
    Request {
        /// The request hit the client's timeout.
        timeout: bool,
    },
    /// The relay answered something other than 2xx (redirects are not followed).
    #[error("heartbeat endpoint answered {0}")]
    Status(u16),
}

/// The heartbeat's own HTTP client: no redirects, a timeout, and cetra's user agent. Not the
/// evaluation runner's client nor the identity client.
pub(crate) fn client(timeout: Duration) -> Result<reqwest::Client, HeartbeatError> {
    let builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .user_agent(format!("cetra/{}", facts::VERSION));
    // A developer's HTTP(S)_PROXY must not catch the tests' 127.0.0.1 receivers (as M1's export client).
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder.build().map_err(|_| HeartbeatError::Client)
}

/// The heartbeat URL for the relay at `endpoint`: `<endpoint>/v1/heartbeat`.
pub(crate) fn heartbeat_url(endpoint: &str) -> String {
    format!("{}/v1/heartbeat", endpoint.trim_end_matches('/'))
}

/// How long until the next heartbeat is due, or `None` when it is due now (never sent, or a
/// full day has passed since `last_sent_ns`).
fn wait_before_send(last_sent_ns: Option<i64>, now_ns: i64) -> Option<Duration> {
    let last = i128::from(last_sent_ns?);
    let now = i128::from(now_ns);
    let period = i128::from(u64::try_from(PERIOD.as_nanos()).unwrap_or(u64::MAX));
    if last > now {
        // The clock went back: wait a whole period rather than send twice in a day.
        return Some(PERIOD);
    }
    let elapsed = now - last;
    if elapsed >= period {
        None
    } else {
        Some(Duration::from_nanos(
            u64::try_from(period - elapsed).unwrap_or(0),
        ))
    }
}

/// The stored last-send time. An unparseable row counts as never sent (the next success rewrites it).
fn parse_last_sent(value: Option<String>) -> Option<i64> {
    value.and_then(|v| v.trim().parse::<i64>().ok())
}

/// What the heartbeat loop keeps between ticks.
#[derive(Debug)]
pub(crate) struct Sender {
    client: reqwest::Client,
    url: String,
    instance_id: Uuid,
    deployment: Deployment,
}

impl Sender {
    /// A sender that posts to `url` as `instance_id` on `deployment`.
    pub(crate) fn new(
        client: reqwest::Client,
        url: String,
        instance_id: Uuid,
        deployment: Deployment,
    ) -> Self {
        Self {
            client,
            url,
            instance_id,
            deployment,
        }
    }

    /// Posts `body` as JSON. The response body is never read.
    async fn send(&self, body: &Heartbeat) -> Result<(), HeartbeatError> {
        let response = self
            .client
            .post(&self.url)
            .json(body)
            .send()
            .await
            .map_err(|error| HeartbeatError::Request {
                timeout: error.is_timeout(),
            })?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(HeartbeatError::Status(status.as_u16()))
        }
    }

    /// One pass of the loop: sends when due and records the send time after a 2xx. Returns how
    /// long to sleep before the next pass. Logs at `debug` only.
    pub(crate) async fn tick(&self, state: &AppState) -> Duration {
        let now = state.clock.now_ns();
        let value = match get_setting(state.meta.reader(), TELEMETRY_LAST_HEARTBEAT_AT).await {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!(error = %error, "telemetry: heartbeat not sent");
                return PERIOD;
            }
        };
        if let Some(wait) = wait_before_send(parse_last_sent(value), now) {
            return wait;
        }
        let payload = match collect(
            &state.meta,
            &state.config,
            self.instance_id,
            self.deployment,
            facts::process_uptime(),
            now,
        )
        .await
        {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!(error = %error, "telemetry: heartbeat not sent");
                return PERIOD;
            }
        };
        if let Err(error) = self.send(&payload).await {
            tracing::debug!(error = %error, "telemetry: heartbeat not sent");
            return PERIOD;
        }
        let recorded = state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    put_setting(conn, TELEMETRY_LAST_HEARTBEAT_AT, &now.to_string()).await
                })
            })
            .await;
        if let Err(error) = recorded {
            tracing::debug!(error = %error, "telemetry: heartbeat time not recorded");
        }
        tracing::debug!("telemetry: heartbeat sent");
        PERIOD
    }
}

/// The startup line when the heartbeat runs. Shows on stdout at `log.level = "warn"` too
/// (`NOTICE_DIRECTIVE`).
fn notice_on(instance_id: Uuid) {
    tracing::info!(
        target: NOTICE_TARGET,
        instance_id = %instance_id,
        docs = TELEMETRY_DOCS_URL,
        "telemetry: cetra sends an anonymous heartbeat once a day (version, platform, bucketed counts, features in use); turn it off with CETRA_TELEMETRY=off"
    );
}

/// The startup line when the heartbeat does not run, with why.
fn notice_off(reason: &'static str) {
    tracing::info!(target: NOTICE_TARGET, reason, "telemetry: the heartbeat is off");
}

/// Logs the startup notice and, when the switchboard allows it, spawns the heartbeat loop.
/// Fails closed: any error here means no heartbeat.
pub(crate) async fn start(state: &AppState, decision: Decision) -> Option<JoinHandle<()>> {
    if let Switch::Off(reason) = decision.heartbeat {
        notice_off(reason.as_str());
        return None;
    }
    let client = match client(REQUEST_TIMEOUT) {
        Ok(client) => client,
        Err(_error) => {
            notice_off("client_unavailable");
            return None;
        }
    };
    let id = match instance::instance_id(&state.meta).await {
        Ok(id) => id,
        Err(error) => {
            tracing::debug!(error = %error, "telemetry: instance id");
            notice_off("instance_id_unavailable");
            return None;
        }
    };
    notice_on(id);
    let sender = Sender::new(
        client,
        heartbeat_url(&state.config.telemetry.endpoint),
        id,
        facts::deployment(facts::probe_host()),
    );
    let first = FIRST_DELAY + Duration::from_secs(rand::random_range(0..=MAX_JITTER.as_secs()));
    let state = state.clone();
    Some(tokio::spawn(async move {
        tokio::time::sleep(first).await;
        loop {
            let wait = sender.tick(&state).await;
            tokio::time::sleep(wait).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        GithubAuth, GoogleAuth, OidcAuth, Secret, SmtpUrl, Tls, parse_otlp_endpoint,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};

    use crate::meta::instance_settings::TELEMETRY_INSTANCE_ID;
    use crate::observability::switchboard::OffReason;
    use crate::test_support::{TestApp, capture_logs};

    /// The fixed instance id of the golden tests (never random: see `no_planted_value`).
    const GOLDEN_ID: &str = "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d";

    /// The top-level keys of the heartbeat, in order.
    const KEYS: [&str; 15] = [
        "schema",
        "instance_id",
        "version",
        "target",
        "profile",
        "deployment",
        "storage",
        "uptime",
        "install_age",
        "orgs",
        "projects",
        "users",
        "spans_per_day",
        "scores_per_day",
        "features",
    ];

    /// The keys of `features`, in order.
    const FEATURE_KEYS: [&str; 10] = [
        "email",
        "github_sign_in",
        "google_sign_in",
        "oidc_sign_in",
        "alert_rules",
        "online_rules",
        "datasets",
        "prompts",
        "annotation_queues",
        "self_monitoring",
    ];

    /// The fixed heartbeat of the golden test.
    fn golden() -> Heartbeat {
        Heartbeat {
            schema: HEARTBEAT_SCHEMA,
            instance_id: Uuid::parse_str(GOLDEN_ID).expect("a fixed UUID"),
            version: "0.1.0",
            target: "x86_64-unknown-linux-musl",
            profile: Profile::Release,
            deployment: Deployment::Docker,
            storage: StorageKind::File,
            uptime: UptimeBucket::UnderHour,
            install_age: InstallAgeBucket::MonthToQuarter,
            orgs: CountBucket::One,
            projects: CountBucket::TwoToFive,
            users: CountBucket::SixToTwenty,
            spans_per_day: VolumeBucket::UnderHundredThousand,
            scores_per_day: VolumeBucket::UnderThousand,
            features: Features {
                email: true,
                github_sign_in: false,
                google_sign_in: true,
                oidc_sign_in: false,
                alert_rules: true,
                online_rules: true,
                datasets: false,
                prompts: true,
                annotation_queues: false,
                self_monitoring: false,
            },
        }
    }

    /// A `Secret` from a string, as the config loader reads one.
    fn secret(value: &str) -> Secret<String> {
        serde_json::from_str(&format!("\"{value}\"")).expect("a secret")
    }

    /// Runs one SQL statement on the writer, with no parameters.
    async fn run(app: &TestApp, sql: &'static str) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(sql).execute(&mut *conn).await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .expect("the statement runs");
    }

    #[test]
    fn the_payload_matches_the_golden_json() {
        let expected = r#"{
  "schema": 1,
  "instance_id": "8f14e45f-ceea-4e7a-9d2b-1c3f5a6b7c8d",
  "version": "0.1.0",
  "target": "x86_64-unknown-linux-musl",
  "profile": "release",
  "deployment": "docker",
  "storage": "file",
  "uptime": "<1h",
  "install_age": "30d-90d",
  "orgs": "1",
  "projects": "2-5",
  "users": "6-20",
  "spans_per_day": "<100k",
  "scores_per_day": "<1k",
  "features": {
    "email": true,
    "github_sign_in": false,
    "google_sign_in": true,
    "oidc_sign_in": false,
    "alert_rules": true,
    "online_rules": true,
    "datasets": false,
    "prompts": true,
    "annotation_queues": false,
    "self_monitoring": false
  }
}"#;
        assert_eq!(
            serde_json::to_string_pretty(&golden()).expect("serializes"),
            expected
        );
    }

    #[test]
    fn the_key_set_is_fixed() {
        let value = serde_json::to_value(golden()).expect("serializes");
        let object = value.as_object().expect("an object");
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        assert_eq!(keys, KEYS);
        let features = object["features"].as_object().expect("an object");
        let feature_keys: Vec<&str> = features.keys().map(String::as_str).collect();
        assert_eq!(feature_keys, FEATURE_KEYS);
        assert!(!value.to_string().contains("licen"));
    }

    #[tokio::test]
    async fn a_fresh_instance_collects_zeros() {
        let app = TestApp::new().await;
        let heartbeat = collect(
            &app.state.meta,
            &app.state.config,
            Uuid::parse_str(GOLDEN_ID).expect("a fixed UUID"),
            Deployment::Binary,
            Duration::ZERO,
            app.state.clock.now_ns(),
        )
        .await
        .expect("collects");
        assert_eq!(heartbeat.orgs, CountBucket::Zero);
        assert_eq!(heartbeat.projects, CountBucket::Zero);
        assert_eq!(heartbeat.users, CountBucket::Zero);
        assert_eq!(heartbeat.spans_per_day, VolumeBucket::Zero);
        assert_eq!(heartbeat.scores_per_day, VolumeBucket::Zero);
        assert_eq!(heartbeat.install_age, InstallAgeBucket::UnderDay);
        assert_eq!(heartbeat.uptime, UptimeBucket::UnderHour);
        assert_eq!(heartbeat.storage, StorageKind::File);
        assert_eq!(heartbeat.version, facts::VERSION);
        assert!(!heartbeat.features.alert_rules);
        assert!(!heartbeat.features.online_rules);
        assert!(!heartbeat.features.datasets);
        assert!(!heartbeat.features.prompts);
        assert!(!heartbeat.features.annotation_queues);
    }

    #[tokio::test]
    async fn collect_counts_and_buckets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id.to_string();
        let project = seeded.project.id.to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    // The test clock is 2026-01-01T00:00:00Z, so "yesterday" is 2025-12-31.
                    sqlx::query(
                        "INSERT INTO usage_daily (org_id, project_id, day, spans, scores) \
                         VALUES (?, ?, '2025-12-31', 1500, 3)",
                    )
                    .bind(org.as_str())
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(
                        "INSERT INTO usage_daily (org_id, project_id, day, spans, scores) \
                         VALUES (?, ?, '2026-01-01', 1000000000, 0)",
                    )
                    .bind(org.as_str())
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .expect("the rows are inserted");
        let heartbeat = collect(
            &app.state.meta,
            &app.state.config,
            Uuid::parse_str(GOLDEN_ID).expect("a fixed UUID"),
            Deployment::Binary,
            Duration::ZERO,
            app.state.clock.now_ns(),
        )
        .await
        .expect("collects");
        assert_eq!(heartbeat.orgs, CountBucket::One);
        assert_eq!(heartbeat.projects, CountBucket::One);
        assert_eq!(heartbeat.users, CountBucket::One);
        assert_eq!(heartbeat.spans_per_day, VolumeBucket::UnderTenThousand);
        assert_eq!(heartbeat.scores_per_day, VolumeBucket::UnderThousand);
    }

    #[tokio::test]
    async fn deleting_orgs_and_disabled_users_are_not_counted() {
        let app = TestApp::new().await;
        app.seed().await;
        run(&app, "UPDATE orgs SET state = 'deleting'").await;
        run(&app, "UPDATE users SET disabled_at = 1").await;
        let heartbeat = collect(
            &app.state.meta,
            &app.state.config,
            Uuid::parse_str(GOLDEN_ID).expect("a fixed UUID"),
            Deployment::Binary,
            Duration::ZERO,
            app.state.clock.now_ns(),
        )
        .await
        .expect("collects");
        assert_eq!(heartbeat.orgs, CountBucket::Zero);
        assert_eq!(heartbeat.users, CountBucket::Zero);
    }

    #[tokio::test]
    async fn install_age_uses_the_first_user() {
        let app = TestApp::new().await;
        app.seed().await;
        let forty_days_ago = app.state.clock.now_ns() - 40 * 86_400_000_000_000_i64;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE users SET created_at = ?")
                        .bind(forty_days_ago)
                        .execute(&mut *conn)
                        .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .expect("the user is aged");
        let heartbeat = collect(
            &app.state.meta,
            &app.state.config,
            Uuid::parse_str(GOLDEN_ID).expect("a fixed UUID"),
            Deployment::Binary,
            Duration::ZERO,
            app.state.clock.now_ns(),
        )
        .await
        .expect("collects");
        assert_eq!(heartbeat.install_age, InstallAgeBucket::MonthToQuarter);
    }

    #[tokio::test]
    async fn table_flags_see_each_table() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id.to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO alert_rules (id, project_id, name, metric, config, created_at, updated_at) \
                         VALUES ('a1', ?, 'r', 'error_rate', '{}', 1, 1)",
                    )
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(
                        "INSERT INTO evaluators (id, project_id, name, kind, config, output_score, created_at, updated_at) \
                         VALUES ('e1', ?, 'e', 'code', '{}', 's', 1, 1)",
                    )
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(
                        "INSERT INTO online_rules (id, project_id, evaluator_id, name, target, variable_mapping, created_at, updated_at) \
                         VALUES ('o1', ?, 'e1', 'o', 'trace', '{}', 1, 1)",
                    )
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(
                        "INSERT INTO datasets (id, project_id, name, created_at, updated_at) \
                         VALUES ('d1', ?, 'd', 1, 1)",
                    )
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(
                        "INSERT INTO prompts (id, project_id, name, type, latest_version, created_at, updated_at) \
                         VALUES ('p1', ?, 'p', 'text', 1, 1, 1)",
                    )
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query(
                        "INSERT INTO annotation_queues (id, project_id, name, target, created_at, updated_at) \
                         VALUES ('q1', ?, 'q', 'trace', 1, 1)",
                    )
                    .bind(project.as_str())
                    .execute(&mut *conn)
                    .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .expect("the rows are inserted");
        let flags = table_flags(&app.state.meta).await.expect("reads the flags");
        assert_eq!(
            flags,
            TableFlags {
                alert_rules: true,
                online_rules: true,
                datasets: true,
                prompts: true,
                annotation_queues: true,
            }
        );
    }

    #[test]
    fn features_follow_the_config() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = crate::test_support::config(dir.path());
        let none = Features {
            email: false,
            github_sign_in: false,
            google_sign_in: false,
            oidc_sign_in: false,
            alert_rules: false,
            online_rules: false,
            datasets: false,
            prompts: false,
            annotation_queues: false,
            self_monitoring: false,
        };
        assert_eq!(features(&config, TableFlags::default()), none);

        config.smtp_url = Some(SmtpUrl {
            tls: Tls::Required,
            host: "smtp.example.com".to_owned(),
            port: 587,
            credentials: None,
        });
        config.auth.github = Some(GithubAuth {
            client_id: "gh".to_owned(),
            client_secret: secret("gh-secret"),
            link_existing_by_email: true,
        });
        config.auth.google = Some(GoogleAuth {
            client_id: "g".to_owned(),
            client_secret: secret("g-secret"),
            allowed_domains: Vec::new(),
            link_existing_by_email: true,
        });
        config.auth.oidc = vec![OidcAuth {
            name: "okta".to_owned(),
            display_name: "Okta".to_owned(),
            issuer: "https://acme.okta.com".to_owned(),
            client_id: "o".to_owned(),
            client_secret: secret("o-secret"),
            scopes: vec!["openid".to_owned(), "email".to_owned()],
            allowed_domains: Vec::new(),
            auto_provision: None,
            link_existing_by_email: false,
        }];
        config.self_monitoring.otlp_endpoint =
            Some(parse_otlp_endpoint("https://otlp.example.com").expect("a valid endpoint"));
        assert_eq!(
            features(&config, TableFlags::default()),
            Features {
                email: true,
                github_sign_in: true,
                google_sign_in: true,
                oidc_sign_in: true,
                self_monitoring: true,
                ..none
            }
        );
    }

    #[test]
    fn storage_kind_follows_the_object_store() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut config = crate::test_support::config(dir.path());
        assert_eq!(storage_kind(&config), StorageKind::File);
        config.storage.object_store = Some(ObjectStoreUrl::S3 {
            bucket: "b".to_owned(),
            prefix: String::new(),
        });
        assert_eq!(storage_kind(&config), StorageKind::S3);
    }

    #[tokio::test]
    async fn no_planted_value_reaches_the_payload() {
        let app = TestApp::with_config(|config| {
            config.email_from = Some("Planted <noreply@planted-domain.example>".to_owned());
            config.auth.signup_domains = vec!["planted-domain.example".to_owned()];
        })
        .await;
        let seeded = app.seed().await;
        let heartbeat = collect(
            &app.state.meta,
            &app.state.config,
            Uuid::parse_str(GOLDEN_ID).expect("a fixed UUID"),
            Deployment::Binary,
            Duration::ZERO,
            app.state.clock.now_ns(),
        )
        .await
        .expect("collects");
        let text = serde_json::to_string(&heartbeat).expect("serializes");
        let banned: Vec<String> = [
            "planted",
            "ada@",
            "example.com",
            "Acme",
            "acme",
            "Support",
            "support-bot",
        ]
        .iter()
        .map(|word| (*word).to_owned())
        .chain([
            seeded.org_id.to_string(),
            seeded.project.id.to_string(),
            seeded.user_id.to_string(),
        ])
        .collect();
        for word in &banned {
            assert!(!text.contains(word.as_str()), "the payload carries {word}");
        }
    }

    #[tokio::test]
    async fn a_licensed_instance_sends_the_same_keys() {
        let app = TestApp::with_license(&["org_sso"]).await;
        let heartbeat = collect(
            &app.state.meta,
            &app.state.config,
            Uuid::parse_str(GOLDEN_ID).expect("a fixed UUID"),
            Deployment::Binary,
            Duration::ZERO,
            app.state.clock.now_ns(),
        )
        .await
        .expect("collects");
        let value = serde_json::to_value(&heartbeat).expect("serializes");
        let keys: Vec<&str> = value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, KEYS);
        let text = value.to_string();
        assert!(!text.contains("licen"));
        assert!(!text.contains("org_sso"));
    }

    #[test]
    fn the_telemetry_modules_do_not_touch_licences() {
        let sources = [
            include_str!("heartbeat.rs"),
            include_str!("instance.rs"),
            include_str!("switchboard.rs"),
            include_str!("facts.rs"),
            include_str!("buckets.rs"),
        ];
        let forbidden = [
            concat!("crate::", "ee"),
            concat!("licen", "se"),
            concat!("entitle", "ments"),
        ];
        for source in sources {
            let code = source.split("#[cfg(test)]").next().unwrap_or(source);
            for word in forbidden {
                assert!(!code.contains(word), "a telemetry module mentions {word}");
            }
        }
    }

    #[test]
    fn the_payload_is_small() {
        let text = serde_json::to_string(&golden()).expect("serializes");
        assert!(text.len() < 4_096, "the payload is {} bytes", text.len());
    }

    /// Serves `POST /v1/heartbeat` on `127.0.0.1:0`: waits `delay`, records the request, answers
    /// `status` (with `Location: /elsewhere` when `status` is a 3xx). `POST` and `GET /elsewhere`
    /// count hits and answer 200. Aborts its task on drop.
    struct Receiver {
        base: String,
        requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
        elsewhere: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    #[derive(Clone)]
    struct ReceiverState {
        status: u16,
        delay: Duration,
        requests: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
        elsewhere: Arc<AtomicUsize>,
    }

    impl Receiver {
        async fn start(status: u16, delay: Duration) -> Self {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let elsewhere = Arc::new(AtomicUsize::new(0));
            let state = ReceiverState {
                status,
                delay,
                requests: Arc::clone(&requests),
                elsewhere: Arc::clone(&elsewhere),
            };
            let router = Router::new()
                .route("/v1/heartbeat", post(heartbeat))
                .route("/elsewhere", get(other).post(other))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, router).await;
            });
            Self {
                base: format!("http://{addr}"),
                requests,
                elsewhere,
                task,
            }
        }

        fn requests(&self) -> Vec<(HeaderMap, Bytes)> {
            self.requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    impl Drop for Receiver {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn heartbeat(
        State(state): State<ReceiverState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        tokio::time::sleep(state.delay).await;
        state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((headers, body));
        let status = StatusCode::from_u16(state.status).unwrap();
        if status.is_redirection() {
            (status, [(axum::http::header::LOCATION, "/elsewhere")]).into_response()
        } else {
            status.into_response()
        }
    }

    async fn other(State(state): State<ReceiverState>) -> StatusCode {
        state.elsewhere.fetch_add(1, Ordering::SeqCst);
        StatusCode::OK
    }

    /// A sender for `receiver` with the golden instance id.
    fn sender_for(receiver: &Receiver) -> Sender {
        Sender::new(
            client(REQUEST_TIMEOUT).unwrap(),
            heartbeat_url(&receiver.base),
            golden().instance_id,
            Deployment::Binary,
        )
    }

    /// One hour in nanoseconds.
    const HOUR_NS: i64 = 3_600_000_000_000;

    #[test]
    fn heartbeat_url_appends_the_path() {
        assert_eq!(
            heartbeat_url("https://telemetry.cetra.sh"),
            "https://telemetry.cetra.sh/v1/heartbeat"
        );
        assert_eq!(
            heartbeat_url("https://telemetry.cetra.sh/"),
            "https://telemetry.cetra.sh/v1/heartbeat"
        );
        assert_eq!(
            heartbeat_url("https://r.example.com/cetra/"),
            "https://r.example.com/cetra/v1/heartbeat"
        );
    }

    #[test]
    fn wait_before_send_rules() {
        let now = 100 * 24 * HOUR_NS;
        assert_eq!(wait_before_send(None, now), None);
        assert_eq!(
            wait_before_send(Some(now - HOUR_NS), now),
            Some(Duration::from_secs(23 * 3600))
        );
        assert_eq!(wait_before_send(Some(now - 24 * HOUR_NS), now), None);
        assert_eq!(wait_before_send(Some(now - 25 * HOUR_NS), now), None);
        assert_eq!(wait_before_send(Some(now + HOUR_NS), now), Some(PERIOD));
        assert_eq!(wait_before_send(Some(i64::MIN), i64::MAX), None);
        assert_eq!(wait_before_send(Some(i64::MAX), i64::MIN), Some(PERIOD));
    }

    #[test]
    fn parse_last_sent_rules() {
        assert_eq!(parse_last_sent(Some("123".to_owned())), Some(123));
        assert_eq!(parse_last_sent(Some(" 123 ".to_owned())), Some(123));
        assert_eq!(parse_last_sent(Some("abc".to_owned())), None);
        assert_eq!(parse_last_sent(Some(String::new())), None);
        assert_eq!(parse_last_sent(None), None);
    }

    #[tokio::test]
    async fn send_posts_json_with_the_user_agent() {
        let receiver = Receiver::start(204, Duration::ZERO).await;
        let sender = sender_for(&receiver);
        let payload = golden();
        sender.send(&payload).await.unwrap();
        let requests = receiver.requests();
        assert_eq!(requests.len(), 1);
        let (headers, body) = &requests[0];
        let content_type = headers.get("content-type").unwrap().to_str().unwrap();
        assert!(
            content_type.starts_with("application/json"),
            "{content_type}"
        );
        let user_agent = headers.get("user-agent").unwrap().to_str().unwrap();
        assert_eq!(user_agent, format!("cetra/{}", facts::VERSION));
        let parsed: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(parsed, serde_json::to_value(&payload).unwrap());
    }

    #[tokio::test]
    async fn send_does_not_follow_redirects() {
        let receiver = Receiver::start(307, Duration::ZERO).await;
        let result = sender_for(&receiver).send(&golden()).await;
        assert!(matches!(result, Err(HeartbeatError::Status(307))));
        assert_eq!(receiver.elsewhere.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_4xx_and_a_5xx_are_errors() {
        let bad = Receiver::start(400, Duration::ZERO).await;
        assert!(matches!(
            sender_for(&bad).send(&golden()).await,
            Err(HeartbeatError::Status(400))
        ));
        let unavailable = Receiver::start(503, Duration::ZERO).await;
        assert!(matches!(
            sender_for(&unavailable).send(&golden()).await,
            Err(HeartbeatError::Status(503))
        ));
    }

    #[tokio::test]
    async fn a_slow_endpoint_times_out() {
        let receiver = Receiver::start(204, Duration::from_secs(2)).await;
        let sender = Sender::new(
            client(Duration::from_millis(200)).unwrap(),
            heartbeat_url(&receiver.base),
            golden().instance_id,
            Deployment::Binary,
        );
        assert!(matches!(
            sender.send(&golden()).await,
            Err(HeartbeatError::Request { timeout: true })
        ));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let sender = Sender::new(
            client(REQUEST_TIMEOUT).unwrap(),
            heartbeat_url(&format!("http://{addr}")),
            golden().instance_id,
            Deployment::Binary,
        );
        assert!(matches!(
            sender.send(&golden()).await,
            Err(HeartbeatError::Request { .. })
        ));
    }

    #[tokio::test]
    async fn tick_sends_and_records_the_time() {
        let app = TestApp::new().await;
        let receiver = Receiver::start(204, Duration::ZERO).await;
        let sender = sender_for(&receiver);
        let now = app.state.clock.now_ns();
        assert_eq!(sender.tick(&app.state).await, PERIOD);
        assert_eq!(receiver.requests().len(), 1);
        let stored = get_setting(app.state.meta.reader(), TELEMETRY_LAST_HEARTBEAT_AT)
            .await
            .unwrap();
        assert_eq!(stored, Some(now.to_string()));
    }

    #[tokio::test]
    async fn tick_waits_when_sent_recently() {
        let app = TestApp::new().await;
        let receiver = Receiver::start(204, Duration::ZERO).await;
        let sender = sender_for(&receiver);
        let recent = (app.state.clock.now_ns() - HOUR_NS).to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(
                    async move { put_setting(conn, TELEMETRY_LAST_HEARTBEAT_AT, &recent).await },
                )
            })
            .await
            .unwrap();
        assert_eq!(
            sender.tick(&app.state).await,
            Duration::from_secs(23 * 3600)
        );
        assert!(receiver.requests().is_empty());
    }

    #[tokio::test]
    async fn tick_does_not_record_a_failure() {
        let app = TestApp::new().await;
        let receiver = Receiver::start(503, Duration::ZERO).await;
        let sender = sender_for(&receiver);
        assert_eq!(sender.tick(&app.state).await, PERIOD);
        let stored = get_setting(app.state.meta.reader(), TELEMETRY_LAST_HEARTBEAT_AT)
            .await
            .unwrap();
        assert_eq!(stored, None);
    }

    #[tokio::test]
    async fn failures_are_logged_at_debug_only() {
        let app = TestApp::new().await;
        let receiver = Receiver::start(503, Duration::ZERO).await;
        let sender = sender_for(&receiver);
        let (wait, logs) = capture_logs(sender.tick(&app.state)).await;
        assert_eq!(wait, PERIOD);
        assert!(!logs.contains("WARN"), "{logs}");
        assert!(!logs.contains("ERROR"), "{logs}");
        let port = receiver
            .base
            .rsplit(':')
            .next()
            .unwrap_or_default()
            .to_owned();
        for line in logs.lines().filter(|line| line.contains("cetra_server")) {
            assert!(
                !line.contains(&port),
                "a cetra line carries the URL: {line}"
            );
        }
    }

    #[tokio::test]
    async fn start_when_off_logs_the_reason_and_spawns_nothing() {
        let app = TestApp::new().await;
        let decision = Decision {
            heartbeat: Switch::Off(OffReason::DoNotTrack),
        };
        let (handle, logs) = capture_logs(start(&app.state, decision)).await;
        assert!(handle.is_none());
        assert!(logs.contains("telemetry: the heartbeat is off"), "{logs}");
        assert!(logs.contains("DO_NOT_TRACK"), "{logs}");
        let stored = get_setting(app.state.meta.reader(), TELEMETRY_INSTANCE_ID)
            .await
            .unwrap();
        assert_eq!(stored, None);
    }

    #[tokio::test]
    async fn start_when_on_logs_the_instance_id_and_spawns() {
        let app = TestApp::new().await;
        let decision = Decision {
            heartbeat: Switch::On,
        };
        let (handle, logs) = capture_logs(start(&app.state, decision)).await;
        let handle = handle.expect("the heartbeat task is spawned");
        handle.abort();
        let stored = get_setting(app.state.meta.reader(), TELEMETRY_INSTANCE_ID)
            .await
            .unwrap()
            .expect("the instance id row exists");
        assert!(logs.contains(&stored), "{logs}");
        assert!(logs.contains("CETRA_TELEMETRY=off"), "{logs}");
        assert!(logs.contains(TELEMETRY_DOCS_URL), "{logs}");
    }
}
