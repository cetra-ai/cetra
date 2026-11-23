//! Process configuration for `cetra serve` (spec 12).
//!
//! [`ServerConfig::load`] merges serde defaults, `cetra.toml`, and `CETRA_*`
//! environment variables. Later sources win. A secret's [`Debug`] output is
//! `Secret(***)`, and a rejected secret is reported by key only.
//!
//! The `[storage]` table is [`cetra_storage::config::StorageConfig`]. That
//! type requires `data_dir`, which is not a `[storage]` key here: a user-set
//! `storage.data_dir` is an error, and the top-level `data_dir` is injected
//! before the table is deserialized. [`StorageConfig::validate`] then runs
//! with the same field paths (`storage.wal.sync`, and so on).

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytesize::ByteSize;
use cetra_core::ids::OrgId;
use cetra_otel::IngestLimits;
use cetra_storage::config::StorageConfig;
use figment::providers::{Format, Serialized, Toml};
use figment::value::{Dict, Map, Value};
use figment::{Figment, Metadata, Profile, Provider};
use ipnet::IpNet;
use lettre::message::Mailbox;
use serde::Deserialize;
use serde::de::{self, Deserializer, SeqAccess, Visitor};

use crate::api::account::SignupPolicy;
use crate::api::entitlements::Feature;

/// `admin_email`, `admin_password`, `org`, and `project` are one group.
const INIT_GROUP: &str =
    "set admin_email, admin_password, org, and project together, or leave them all unset";

/// Environment names that map to a different key than `CETRA_` + `__` splitting.
///
/// These names are not also read by the split provider. `CETRA_AUTH_SIGNUP`
/// would otherwise become a top-level `auth_signup` and fail
/// [`deny_unknown_fields`](https://serde.rs/container-attrs.html#deny_unknown_fields).
const ALIASES: &[(&str, &str)] = &[
    ("CETRA_DATA_DIR", "data_dir"),
    ("CETRA_PUBLIC_URL", "public_url"),
    ("CETRA_HTTP_ADDR", "http_addr"),
    ("CETRA_OTLP_HTTP_ADDR", "otlp_http_addr"),
    ("CETRA_GRPC_ADDR", "grpc_addr"),
    ("CETRA_METRICS_ADDR", "metrics_addr"),
    ("CETRA_TRUSTED_PROXIES", "trusted_proxies"),
    ("CETRA_OBJECT_STORE", "storage.object_store"),
    ("CETRA_SMTP_URL", "smtp_url"),
    ("CETRA_EMAIL_FROM", "email_from"),
    ("CETRA_ENCRYPTION_KEY", "encryption_key"),
    ("CETRA_LICENSE_KEY", "license_key"),
    ("CETRA_AUTH_SIGNUP", "auth.signup"),
    ("CETRA_AUTH_SIGNUP_DOMAINS", "auth.signup_domains"),
    ("CETRA_AUTH_DISABLE_PASSWORD", "auth.disable_password"),
    (
        "CETRA_AUTH_REQUIRE_EMAIL_VERIFICATION",
        "auth.require_email_verification",
    ),
    ("CETRA_AUTH_ENFORCE_SSO_DOMAINS", "auth.enforce_sso_domains"),
    ("CETRA_INIT_ADMIN_EMAIL", "init.admin_email"),
    ("CETRA_INIT_ADMIN_PASSWORD", "init.admin_password"),
    ("CETRA_INIT_ORG", "init.org"),
    ("CETRA_INIT_PROJECT", "init.project"),
    ("CETRA_INIT_SECRET_KEY", "init.secret_key"),
    ("CETRA_INIT_READ_KEY", "init.read_key"),
    ("CETRA_LOG", "log.level"),
];

/// A value that must not appear in logs or error text.
///
/// [`Debug`] prints `Secret(***)`. There is no [`Display`] impl.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// The value this secret holds.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Wraps a value read from somewhere other than the config loader.
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

impl<'de> Deserialize<'de> for Secret<String> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(SecretVisitor)
    }
}

macro_rules! reject_secret {
    ($($method:ident($ty:ty)),+ $(,)?) => {
        $(
            fn $method<E: de::Error>(self, _value: $ty) -> Result<Secret<String>, E> {
                Err(E::custom("expected a string"))
            }
        )+
    };
}

struct SecretVisitor;

impl<'de> Visitor<'de> for SecretVisitor {
    type Value = Secret<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Secret<String>, E> {
        Ok(Secret(value.to_owned()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Secret<String>, E> {
        Err(E::custom("expected a string"))
    }

    fn visit_none<E: de::Error>(self) -> Result<Secret<String>, E> {
        Err(E::custom("expected a string"))
    }

    fn visit_some<D: Deserializer<'de>>(
        self,
        _deserializer: D,
    ) -> Result<Secret<String>, D::Error> {
        Err(de::Error::custom("expected a string"))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, _seq: A) -> Result<Secret<String>, A::Error> {
        Err(de::Error::custom("expected a string"))
    }

    fn visit_map<A: de::MapAccess<'de>>(self, _map: A) -> Result<Secret<String>, A::Error> {
        Err(de::Error::custom("expected a string"))
    }

    reject_secret!(
        visit_bool(bool),
        visit_i8(i8),
        visit_i16(i16),
        visit_i32(i32),
        visit_i64(i64),
        visit_i128(i128),
        visit_u8(u8),
        visit_u16(u16),
        visit_u32(u32),
        visit_u64(u64),
        visit_u128(u128),
        visit_f32(f32),
        visit_f64(f64),
        visit_char(char),
    );
}

/// `http` or `https` origin used in links and cookies.
///
/// The path is empty or `/`. A query, a fragment, or userinfo is rejected so a
/// password cannot sit in the URL.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicUrl {
    origin: String,
    https: bool,
}

impl PublicUrl {
    /// `scheme://host[:port]` with no trailing slash.
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Whether the scheme is `https`.
    pub fn is_https(&self) -> bool {
        self.https
    }

    /// `origin` joined with `path`. A path without a leading `/` gets one.
    pub fn join(&self, path: &str) -> String {
        if path.is_empty() {
            return self.origin.clone();
        }
        if path.starts_with('/') {
            format!("{}{path}", self.origin)
        } else {
            format!("{}/{path}", self.origin)
        }
    }
}

impl Default for PublicUrl {
    fn default() -> Self {
        Self {
            origin: "http://localhost:8080".to_owned(),
            https: false,
        }
    }
}

impl fmt::Debug for PublicUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublicUrl")
            .field("origin", &self.origin)
            .finish()
    }
}

impl<'de> Deserialize<'de> for PublicUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_public_url(&raw).map_err(de::Error::custom)
    }
}

/// Who may register, password and SSO flags, and the OAuth providers (spec 08).
///
/// GitHub and Google accept the split env form (`CETRA_AUTH__GITHUB__CLIENT_ID`,
/// `CETRA_AUTH__GOOGLE__CLIENT_SECRET`, and the other scalar fields).
/// `[[auth.oidc]]` providers are defined in TOML; only their client secret has an env
/// form, `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET` (D-S-2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSettings {
    /// Who may create an account after the first user exists.
    #[serde(default = "default_signup")]
    pub signup: SignupPolicy,
    /// Email domains allowed to sign up. Empty means no domain restriction.
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub signup_domains: Vec<String>,
    /// Refuse password login and password signup.
    #[serde(default)]
    pub disable_password: bool,
    /// New accounts must verify their email before signing in.
    #[serde(default)]
    pub require_email_verification: bool,
    /// Domains that must use SSO. Empty means SSO is not required.
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub enforce_sso_domains: Vec<String>,
    /// GitHub OAuth, when configured.
    #[serde(default)]
    pub github: Option<GithubAuth>,
    /// Google OIDC, when configured.
    #[serde(default)]
    pub google: Option<GoogleAuth>,
    /// Generic OIDC providers, in config order. TOML, plus each one's client secret from the env.
    #[serde(default)]
    pub oidc: Vec<OidcAuth>,
}

impl Default for AuthSettings {
    fn default() -> Self {
        Self {
            signup: default_signup(),
            signup_domains: Vec::new(),
            disable_password: false,
            require_email_verification: false,
            enforce_sso_domains: Vec::new(),
            github: None,
            google: None,
            oidc: Vec::new(),
        }
    }
}

/// `[auth.github]`. Linking an existing account by email defaults to on (D148).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubAuth {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: Secret<String>,
    /// Link a verified email to an existing account. Default true.
    #[serde(default = "default_link_known")]
    pub link_existing_by_email: bool,
}

/// `[auth.google]`. The issuer is fixed to `https://accounts.google.com`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleAuth {
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: Secret<String>,
    /// Checked against the verified email and the `hd` claim.
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub allowed_domains: Vec<String>,
    /// Link a verified email to an existing account. Default true.
    #[serde(default = "default_link_known")]
    pub link_existing_by_email: bool,
}

/// One `[[auth.oidc]]` provider. The id is `oidc:<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcAuth {
    /// `[a-z0-9-]{1,32}`, unique among the OIDC providers.
    pub name: String,
    /// Name shown on the login button.
    pub display_name: String,
    /// `https` issuer, or `http` on a loopback host.
    pub issuer: String,
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret. Optional in the file when
    /// `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET` sets it (D-S-2); required after loading.
    #[serde(default = "empty_secret")]
    pub client_secret: Secret<String>,
    /// Must include `openid`. Default `openid`, `email`, `profile`.
    #[serde(
        default = "default_oidc_scopes",
        deserialize_with = "deserialize_string_list"
    )]
    pub scopes: Vec<String>,
    /// Checked against the verified email.
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub allowed_domains: Vec<String>,
    /// Org and role for a new user. The org is not required to exist yet.
    #[serde(default)]
    pub auto_provision: Option<AutoProvision>,
    /// Link a verified email to an existing account. Default false.
    #[serde(default)]
    pub link_existing_by_email: bool,
}

/// `auto_provision` on a generic OIDC provider.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoProvision {
    /// Org slug.
    pub org: String,
    /// `admin`, `member`, or `viewer`. Never `owner`.
    pub role: String,
}

/// Values that create the first admin, org, and project when the database is empty.
///
/// The four `admin_email`, `admin_password`, `org`, and `project` fields are
/// all set or all unset. `secret_key` and `read_key` are independent and used
/// only with them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct InitConfig {
    /// Email of the first admin.
    pub admin_email: Option<String>,
    /// Password of the first admin.
    pub admin_password: Option<Secret<String>>,
    /// Name of the first organization.
    pub org: Option<String>,
    /// Name of the first project.
    pub project: Option<String>,
    /// The project's `sk-cetra-` key with scopes `ingest, read, write`. Optional.
    pub secret_key: Option<Secret<String>>,
    /// The project's `sk-cetra-` key with scope `read` only (D-N-38). Optional.
    pub read_key: Option<Secret<String>>,
}

/// Log format. Absent means the CLI chooses from `isatty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Multi-line, human-readable log lines.
    Pretty,
    /// One JSON object per line.
    Json,
}

/// Log sink settings. `level` is an `EnvFilter` directive.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// `pretty` or `json`. Absent leaves the choice to the CLI.
    #[serde(default)]
    pub format: Option<LogFormat>,
    /// `EnvFilter` directive. Defaults to `info`.
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            format: None,
            level: default_log_level(),
        }
    }
}

/// The default `telemetry.endpoint`: the relay track Q runs (D-TL-10).
const DEFAULT_TELEMETRY_ENDPOINT: &str = "https://telemetry.cetra.sh";

/// The rule a bad `CETRA_TELEMETRY` breaks (D-TL-11). Never the value.
const TELEMETRY_VALUES: &str =
    "accepted values are off, false and 0 (any case); unset or empty leaves the config in charge";

/// `[telemetry]` controls the heartbeat and crash reports: what cetra sends to us (D-TL-1,
/// D-TL-8). The switchboard can turn it off whatever these say (D-TL-11).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// The daily anonymous heartbeat. Default true.
    #[serde(default = "default_heartbeat")]
    pub heartbeat: bool,
    /// Opt-in crash reports to `<endpoint>/v1/crash` (D-TL-5). Default false.
    #[serde(default)]
    pub crash_reports: bool,
    /// Base URL of the relay; the heartbeat goes to `<endpoint>/v1/heartbeat`. `https`, or
    /// `http` to a loopback IP. Stored without a trailing slash.
    #[serde(default = "default_telemetry_endpoint")]
    pub endpoint: String,
    /// `CETRA_TELEMETRY`, `DO_NOT_TRACK` and the CI variables, captured by the loader from
    /// the environment it reads. Not a key.
    #[serde(skip)]
    pub(crate) env: crate::observability::switchboard::TelemetryEnv,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            heartbeat: default_heartbeat(),
            crash_reports: false,
            endpoint: default_telemetry_endpoint(),
            env: crate::observability::switchboard::TelemetryEnv::default(),
        }
    }
}

/// The rule `[analytics]` breaks when only one of its keys is set. Static.
const ANALYTICS_PAIR: &str = "set posthog_host and posthog_key together, or leave both unset";

/// `[analytics]`: per-user product events to the operator's PostHog (track TL M6, D-TL-14).
/// Off while unset; restore mode turns it off too.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyticsConfig {
    /// PostHog ingestion host, e.g. `https://eu.i.posthog.com`; batches go to `<host>/batch/`.
    /// `https`, or `http` to a loopback IP. Stored without a trailing slash.
    #[serde(default)]
    pub posthog_host: Option<String>,
    /// The PostHog project API key (`phc_…`).
    #[serde(default)]
    pub posthog_key: Option<Secret<String>>,
}

impl AnalyticsConfig {
    /// The host and key, when both are set.
    pub fn posthog(&self) -> Option<(&str, &Secret<String>)> {
        match (&self.posthog_host, &self.posthog_key) {
            (Some(host), Some(key)) => Some((host.as_str(), key)),
            _ => None,
        }
    }
}

fn default_heartbeat() -> bool {
    true
}

fn default_telemetry_endpoint() -> String {
    DEFAULT_TELEMETRY_ENDPOINT.to_owned()
}

/// `self_monitoring.otlp_endpoint` (track TL, D-TL-1): an `http` or `https` base URL without
/// user info, query or fragment. Its path is kept and `/v1/logs` or `/v1/traces` is appended to it.
#[derive(Clone, PartialEq, Eq)]
pub struct OtlpEndpoint {
    /// The URL without a trailing `/`, for example `https://otlp.example.com/otlp`.
    base: String,
    /// The host as written in the URL (IPv6 in brackets), for logs.
    host: String,
}

impl OtlpEndpoint {
    /// The URL log records are posted to: the base followed by `/v1/logs`.
    pub fn logs_url(&self) -> String {
        format!("{}/v1/logs", self.base)
    }

    /// The URL spans are posted to: the base followed by `/v1/traces` (track TL M2).
    pub fn traces_url(&self) -> String {
        format!("{}/v1/traces", self.base)
    }

    /// The endpoint's host, the only part of it cetra logs.
    pub fn host(&self) -> &str {
        &self.host
    }
}

/// Prints the host only: some vendors put a token in the path.
impl fmt::Debug for OtlpEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OtlpEndpoint")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// `self_monitoring.trace_sample_ratio` (track TL M2): the probability that a root span is
/// exported, from 0.0 to 1.0; children follow their root. Never NaN, so it is `Eq`.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(try_from = "f64")]
pub struct TraceSampleRatio(f64);

// `TryFrom` refuses NaN, the only `f64` that is not equal to itself, so equality is reflexive.
impl Eq for TraceSampleRatio {}

impl TraceSampleRatio {
    /// The default share of root spans, 10 % (D-TL-3).
    pub const DEFAULT: Self = Self(0.1);

    /// The ratio as a number in `[0.0, 1.0]`.
    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for TraceSampleRatio {
    type Error = &'static str;

    /// Accepts `[0.0, 1.0]`; refuses NaN, infinities and anything outside. The reason never
    /// quotes the input.
    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err("must be a number from 0.0 to 1.0")
        }
    }
}

/// `[self_monitoring]`: export of cetra's own logs and spans to the operator's OTLP/HTTP endpoint
/// (track TL, D-TL-1, D-TL-3). Off while `otlp_endpoint` is unset.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfMonitoringConfig {
    /// Base URL of the OTLP/HTTP endpoint. Unset or blank: no export.
    #[serde(default, deserialize_with = "deserialize_otlp_endpoint")]
    pub otlp_endpoint: Option<OtlpEndpoint>,
    /// Request headers, `name=value,name=value` (the `OTEL_EXPORTER_OTLP_HEADERS` syntax).
    #[serde(default)]
    pub otlp_headers: Option<Secret<String>>,
    /// Whether log records are exported. Defaults to `true`.
    #[serde(default = "default_self_monitoring_logs")]
    pub logs: bool,
    /// Whether spans are exported. Defaults to `true` (track TL M2).
    #[serde(default = "default_self_monitoring_traces")]
    pub traces: bool,
    /// `EnvFilter` directive of the exported events. Defaults to `warn`. The subscriber
    /// checks it and caps `rmcp` at `warn`, as for stdout. Spans record the events that pass it.
    #[serde(default = "default_self_monitoring_level")]
    pub level: String,
    /// Probability that a root span is exported; its children follow it. Defaults to 0.1.
    #[serde(default = "default_trace_sample_ratio")]
    pub trace_sample_ratio: TraceSampleRatio,
    /// Extra resource attributes, `key=value,key=value` (the `OTEL_RESOURCE_ATTRIBUTES` syntax).
    #[serde(default)]
    pub resource: String,
}

impl Default for SelfMonitoringConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            otlp_headers: None,
            logs: default_self_monitoring_logs(),
            traces: default_self_monitoring_traces(),
            level: default_self_monitoring_level(),
            trace_sample_ratio: default_trace_sample_ratio(),
            resource: String::new(),
        }
    }
}

impl SelfMonitoringConfig {
    /// The endpoint, when log records are exported: `otlp_endpoint` is set and `logs` is true.
    pub fn logs_endpoint(&self) -> Option<&OtlpEndpoint> {
        self.otlp_endpoint.as_ref().filter(|_| self.logs)
    }

    /// The endpoint, when spans are exported: `otlp_endpoint` is set, `traces` is true and
    /// `trace_sample_ratio` is above 0. At 0 no span would be kept, so no span layer is built.
    pub fn traces_endpoint(&self) -> Option<&OtlpEndpoint> {
        self.otlp_endpoint
            .as_ref()
            .filter(|_| self.traces && self.trace_sample_ratio.get() > 0.0)
    }

    /// Whether anything is exported: logs, spans or both.
    pub fn exports(&self) -> bool {
        self.logs_endpoint().is_some() || self.traces_endpoint().is_some()
    }
}

/// How an SMTP connection is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tls {
    /// `smtp://` — STARTTLS is required.
    Required,
    /// `smtps://` — TLS from the first byte.
    Wrapper,
    /// `smtp://…?tls=none` — a local relay.
    None,
}

/// A parsed `smtp_url`. The password stays inside [`Secret`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpUrl {
    /// Encryption for this host.
    pub tls: Tls,
    /// SMTP host, without brackets on an IPv6 address.
    pub host: String,
    /// Explicit port, or 587 / 465 / 25 when the URL omits one.
    pub port: u16,
    /// Username and password. Both are percent-decoded.
    pub credentials: Option<(String, Secret<String>)>,
}

/// `[ingest]` (spec 02 § Limits, spec 12).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IngestConfig {
    /// Raw and decompressed body cap. Default 10 MiB.
    pub max_request_bytes: ByteSize,
    /// Spans per request after decoding. Default 50 000.
    pub max_spans_per_request: u32,
    /// Single attribute value before truncation. Default 8 MiB.
    pub max_attribute_bytes: ByteSize,
    /// Decompressed bytes processed at once, all requests (D-G-5). Default 256 MiB.
    pub max_inflight_bytes: ByteSize,
    /// Decoded gRPC message cap. Default 32 MiB.
    pub max_grpc_message_bytes: ByteSize,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: ByteSize::mib(10),
            max_spans_per_request: 50_000,
            max_attribute_bytes: ByteSize::mib(8),
            max_inflight_bytes: ByteSize::mib(256),
            max_grpc_message_bytes: ByteSize::mib(32),
        }
    }
}

impl IngestConfig {
    /// The normalizer's limits, with the `[media]` bounds.
    ///
    /// Sizes were checked by [`validate_ingest`], so the conversions do not
    /// clip in practice; `unwrap_or(usize::MAX)` only avoids an `expect`.
    pub fn limits(&self, media: &MediaConfig) -> IngestLimits {
        IngestLimits {
            max_attribute_bytes: usize_or_max(self.max_attribute_bytes),
            media_min_bytes: usize_or_max(media.min_bytes),
            media_max_bytes: usize_or_max(media.max_bytes),
        }
    }
}

/// `[media]` (spec 02 § Media extraction).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MediaConfig {
    /// Blobs below this stay inline. Default 4 KiB.
    pub min_bytes: ByteSize,
    /// Blobs above this are dropped. Default 32 MiB.
    pub max_bytes: ByteSize,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            min_bytes: ByteSize::kib(4),
            max_bytes: ByteSize::mib(32),
        }
    }
}

/// `[eval]` (spec 06, D-I-6).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EvalConfig {
    /// Evaluation jobs run at once by this process. Default 16, 1 to 256.
    pub workers: u32,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self { workers: 16 }
    }
}

/// `[issues]` (track IC, D-IC-12).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IssuesConfig {
    /// Workers that summarize and embed issue candidates. They share each connection's limits
    /// with judges. Default 4, 1 to 64.
    pub workers: u32,
}

impl Default for IssuesConfig {
    fn default() -> Self {
        Self { workers: 4 }
    }
}

/// `[security]` (spec 12, D166).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SecurityConfig {
    /// Let judges and webhooks reach private, loopback and link-local addresses. Default false.
    pub allow_private_network_egress: bool,
}

/// `[entitlements]` (D-P-15).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EntitlementsConfig {
    /// Where Upgrade buttons go. `{org}` is replaced by the org id. Unset: no button.
    pub upgrade_url: Option<String>,
    /// What an org without a valid token gets (D-P-10, D-P-13).
    pub defaults: EntitlementDefaults,
    /// The `identities.provider` of the control plane's OIDC provider, whose subjects
    /// the control plane uses to name users (D-P-39). Default `"oidc:cloud"`.
    pub control_plane_provider: String,
    /// This instance is a cloud cell managed by the control plane (D-P-50). When false (the
    /// default), every `/api/internal/v1/*` route answers 404, stored and pushed org tokens are
    /// ignored, and `cetra1.` bearer tokens are refused: only licence keys are trusted.
    pub control_plane: bool,
}

impl Default for EntitlementsConfig {
    fn default() -> Self {
        Self {
            upgrade_url: None,
            defaults: EntitlementDefaults::default(),
            control_plane_provider: "oidc:cloud".to_owned(),
            control_plane: false,
        }
    }
}

/// `[entitlements.defaults]`: what an org without a valid token gets (D-P-10, D-P-13).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EntitlementDefaults {
    /// Shown as the org's plan. Default `"Self-hosted"`.
    pub plan_name: String,
    /// Core features granted. `None` (the key absent): every core feature.
    pub features: Option<Vec<String>>,
    /// Limits; unset means unlimited. Same names as D-P-13.
    pub units_included: Option<u64>,
    pub units_cap: Option<u64>,
    pub ingest_spans_per_sec: Option<u64>,
    pub ingest_burst: Option<u64>,
    pub data_access_days: Option<u64>,
    pub max_seats: Option<u64>,
    pub max_projects: Option<u64>,
    pub max_alert_rules: Option<u64>,
    pub max_active_online_rules: Option<u64>,
    pub max_active_issue_sources: Option<u64>,
    /// Annotation queues per org (D-AQ-10).
    pub max_annotation_queues: Option<u64>,
}

impl Default for EntitlementDefaults {
    fn default() -> Self {
        Self {
            plan_name: "Self-hosted".to_owned(),
            features: None,
            units_included: None,
            units_cap: None,
            ingest_spans_per_sec: None,
            ingest_burst: None,
            data_access_days: None,
            max_seats: None,
            max_projects: None,
            max_alert_rules: None,
            max_active_online_rules: None,
            max_active_issue_sources: None,
            max_annotation_queues: None,
        }
    }
}

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

/// `[orgs]` (D-P-31).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OrgsConfig {
    /// Users may create orgs. `false` on a cloud cell, where the control plane creates them.
    pub user_create: bool,
    /// Where "New organization" sends users when `user_create` is false. Unset: no link.
    pub create_url: Option<String>,
}

impl Default for OrgsConfig {
    fn default() -> Self {
        Self {
            user_create: true,
            create_url: None,
        }
    }
}

/// `[demo]`: the demo org every signed-in user can read, and the limits of its visitors (08 § Demo org, D187).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DemoConfig {
    /// The demo org's id. Unset: no demo org. The org must exist and be active at startup.
    pub org: Option<OrgId>,
    /// Requests a minute per implicit visitor.
    pub requests_per_minute: u32,
    /// Requests in flight at once per implicit visitor.
    pub user_in_flight: u32,
    /// Requests in flight at once across all implicit visitors.
    pub org_in_flight: u32,
    /// Longest SQL console query for an implicit visitor. Longer `timeout_ms` is lowered.
    #[serde(with = "humantime_serde")]
    pub sql_timeout: Duration,
    /// Most rows a SQL console answer returns to an implicit visitor.
    pub sql_max_rows: usize,
}

impl Default for DemoConfig {
    fn default() -> Self {
        Self {
            org: None,
            requests_per_minute: 120,
            user_in_flight: 4,
            org_in_flight: 16,
            sql_timeout: Duration::from_secs(10),
            sql_max_rows: 1_000,
        }
    }
}

/// Everything `cetra serve` reads from the environment and `cetra.toml`.
///
/// Keys that spec 12 lists for later work (compaction, gc, deletion, anchor) are not
/// fields. Setting one is an unknown-key error.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Directory that holds `LOCK`, `wal/`, `catalog.db`, and `meta.db`.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Origin used in links and cookies.
    #[serde(default)]
    pub public_url: PublicUrl,
    /// HTTP listen address.
    #[serde(
        default = "default_http_addr",
        deserialize_with = "deserialize_socket_addr"
    )]
    pub http_addr: SocketAddr,
    /// Proxies whose `X-Forwarded-For` is trusted (D142).
    #[serde(default, deserialize_with = "deserialize_proxies")]
    pub trusted_proxies: Vec<IpNet>,
    /// OTLP/HTTP-only listener (D-G-21). `None` when the value is an empty string.
    #[serde(
        default = "default_otlp_http_addr",
        deserialize_with = "deserialize_optional_socket_addr"
    )]
    pub otlp_http_addr: Option<SocketAddr>,
    /// OTLP/gRPC listener (D-G-21, D-G-29). `None` when the value is an empty string.
    #[serde(
        default = "default_grpc_addr",
        deserialize_with = "deserialize_optional_socket_addr"
    )]
    pub grpc_addr: Option<SocketAddr>,
    /// Prometheus metrics listener (`GET /metrics`, N M3, D-N-28). Unset or `""`: off.
    #[serde(default, deserialize_with = "deserialize_optional_socket_addr")]
    pub metrics_addr: Option<SocketAddr>,
    /// Restore mode (N M3, D-N-25): serve reads and run no outbound work. For restore drills.
    #[serde(default)]
    pub restore_mode: bool,
    /// `[storage]` table. `data_dir` is copied from the top-level field.
    ///
    /// `StorageConfig` requires `data_dir` and denies unknown fields, so the
    /// loader rejects a user-set `storage.data_dir`, injects the top-level
    /// directory, then deserializes this table as [`StorageConfig`].
    pub storage: StorageConfig,
    /// `[ingest]` limits.
    #[serde(default)]
    pub ingest: IngestConfig,
    /// `[media]` extraction bounds.
    #[serde(default)]
    pub media: MediaConfig,
    /// `[eval]` worker settings.
    #[serde(default)]
    pub eval: EvalConfig,
    /// `[issues]` worker settings (track IC).
    #[serde(default)]
    pub issues: IssuesConfig,
    /// `[security]` settings.
    #[serde(default)]
    pub security: SecurityConfig,
    /// `[entitlements]` settings (D-P-15).
    #[serde(default)]
    pub entitlements: EntitlementsConfig,
    /// `[orgs]` settings (D-P-31).
    #[serde(default)]
    pub orgs: OrgsConfig,
    /// `[alerts]` settings (D-K-21).
    #[serde(default)]
    pub alerts: AlertsConfig,
    /// `[demo]` settings: the demo org and the limits of its visitors (08 § Demo org, D187).
    #[serde(default)]
    pub demo: DemoConfig,
    /// 32 bytes in standard base64. Unset: `<data_dir>/secret.key` (D-I-1).
    #[serde(default)]
    pub encryption_key: Option<Secret<String>>,
    /// Self-hosted licence key, a signed token (D172, D-P-7). Unset: no licence.
    #[serde(default)]
    pub license_key: Option<Secret<String>>,
    /// Sign-up policy and the auth flags from spec 08.
    #[serde(default)]
    pub auth: AuthSettings,
    /// SMTP URL, including any credentials. Empty does not count as configured.
    #[serde(default, deserialize_with = "deserialize_smtp_url")]
    pub smtp_url: Option<SmtpUrl>,
    /// `From` mailbox. Required when `smtp_url` is set.
    #[serde(default)]
    pub email_from: Option<String>,
    /// First-run admin, org, and project.
    #[serde(default)]
    pub init: InitConfig,
    /// Log format and level.
    #[serde(default)]
    pub log: LogConfig,
    /// `[self_monitoring]`: export of cetra's own logs and spans over OTLP/HTTP (track TL, D-TL-1).
    #[serde(default)]
    pub self_monitoring: SelfMonitoringConfig,
    /// `[telemetry]`: the heartbeat to us (D-TL-8).
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// `[analytics]`: per-user product events to PostHog (track TL M6).
    #[serde(default)]
    pub analytics: AnalyticsConfig,
}

impl ServerConfig {
    /// Load defaults, then `path`, otherwise `$CETRA_CONFIG`, then `CETRA_*`.
    ///
    /// A path that is given but missing is [`ConfigError::MissingFile`]. An
    /// unset path and an unset `CETRA_CONFIG` mean there is no file.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when the file is missing, a key is unknown, or
    /// a value fails validation. Secret values are not part of the message.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let toml = read_toml(path)?;
        let env = std::env::vars().collect::<Vec<_>>();
        let pairs = env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        load_from(toml.as_deref(), &pairs)
    }

    /// The storage configuration, including the injected data directory. Object
    /// deletion is off in restore mode (N M3, D-N-34).
    pub fn storage_config(&self) -> StorageConfig {
        StorageConfig {
            delete_objects: !self.restore_mode,
            ..self.storage.clone()
        }
    }

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
}

/// Why [`ServerConfig::load`] rejected the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Figment could not parse or deserialize a value.
    ///
    /// The message names the key. Errors on `smtp_url`, `init.admin_password`,
    /// `init.secret_key`, `init.read_key`, and any `client_secret` are [`Self::Invalid`]
    /// instead, because figment's type error includes the rejected value. The
    /// error is boxed so [`Result`] stays small.
    #[error(transparent)]
    Figment(Box<figment::Error>),
    /// A value failed a check in this module or in [`StorageConfig::validate`].
    #[error("{key}: {reason}")]
    Invalid {
        /// Dotted config path, for example `auth.signup_domains`.
        key: &'static str,
        /// Static rule. Never the rejected input.
        reason: &'static str,
    },
    /// A provider value failed a check. `key` includes the provider name.
    ///
    /// `reason` is static. It is never a secret, an email, or the issuer URL.
    #[error("{key}: {reason}")]
    Provider {
        /// Dotted path, for example `auth.oidc.okta.issuer`, or the name of the environment variable at fault.
        key: String,
        /// Static rule.
        reason: &'static str,
    },
    /// `path` or `$CETRA_CONFIG` names a file that does not exist.
    #[error("config file not found: {}", .0.display())]
    MissingFile(PathBuf),
}

/// Also read by the debug bundle (track TL M3).
pub(crate) fn default_data_dir() -> PathBuf {
    PathBuf::from("./cetra-data")
}

fn default_http_addr() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

fn default_otlp_http_addr() -> Option<SocketAddr> {
    Some(SocketAddr::from(([0, 0, 0, 0], 4318)))
}

fn default_grpc_addr() -> Option<SocketAddr> {
    Some(SocketAddr::from(([0, 0, 0, 0], 4317)))
}

fn default_signup() -> SignupPolicy {
    SignupPolicy::InviteOnly
}

fn default_link_known() -> bool {
    true
}

fn default_oidc_scopes() -> Vec<String> {
    vec![
        "openid".to_owned(),
        "email".to_owned(),
        "profile".to_owned(),
    ]
}

fn empty_secret() -> Secret<String> {
    Secret(String::new())
}

fn default_log_level() -> String {
    "info".to_owned()
}

fn default_self_monitoring_logs() -> bool {
    true
}

fn default_self_monitoring_level() -> String {
    "warn".to_owned()
}

fn default_self_monitoring_traces() -> bool {
    true
}

fn default_trace_sample_ratio() -> TraceSampleRatio {
    TraceSampleRatio::DEFAULT
}

/// The file, then the `CETRA_*` variables, as one figment: the sources [`load_from`]
/// extracts and [`source_tree`] shows.
fn sources(toml: Option<&str>, env: &[(&str, &str)]) -> Figment {
    let mut figment = Figment::new();
    if let Some(toml) = toml {
        figment = figment.merge(Toml::string(toml));
    }
    figment.merge(EnvSource { pairs: env })
}

/// The merged sources as one dictionary, before serde defaults, validation and the
/// `storage.data_dir` injection: what the debug bundle shows (D-TL-6). A TOML syntax
/// error is mapped as [`load_from`] maps it, so its text names a position only.
pub(crate) fn source_tree(toml: Option<&str>, env: &[(&str, &str)]) -> Result<Dict, ConfigError> {
    let mut data = sources(toml, env).data().map_err(map_figment)?;
    Ok(data.remove(&Profile::Default).unwrap_or_default())
}

/// Defaults, then `toml`, then `env`, validated. Also used by tests outside this module to
/// build a config from TOML.
pub(crate) fn load_from(
    toml: Option<&str>,
    env: &[(&str, &str)],
) -> Result<ServerConfig, ConfigError> {
    let telemetry_env =
        crate::observability::switchboard::TelemetryEnv::from_pairs(env).map_err(|_| {
            ConfigError::Provider {
                key: "CETRA_TELEMETRY".to_owned(),
                reason: TELEMETRY_VALUES,
            }
        })?;
    let mut figment = sources(toml, env);
    if figment.find_value("storage.data_dir").is_ok() {
        return Err(ConfigError::Invalid {
            key: "storage.data_dir",
            reason: "set data_dir at the top level",
        });
    }
    let data_dir = match figment.find_value("data_dir") {
        Ok(value) => path_from_value(value)?,
        Err(_) => default_data_dir(),
    };
    let data_dir = path_string(&data_dir)?;
    figment = figment.merge(Serialized::default("storage.data_dir", data_dir));
    let mut config: ServerConfig = figment.extract().map_err(map_figment)?;
    apply_oidc_env(&mut config.auth, env)?;
    config.telemetry.env = telemetry_env;
    validate(&mut config)?;
    Ok(config)
}

/// The variable that sets provider `name`'s client secret (D-S-2): `cloud` →
/// `CETRA_AUTH__OIDC__CLOUD__CLIENT_SECRET`, `my-idp` → `…__MY_IDP__CLIENT_SECRET`.
fn oidc_env_name(name: &str) -> String {
    format!(
        "CETRA_AUTH__OIDC__{}__CLIENT_SECRET",
        name.to_ascii_uppercase().replace('-', "_")
    )
}

/// Applies `CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET` to the `[[auth.oidc]]` provider of
/// that name (D-S-2). Any other `CETRA_AUTH__OIDC__*` variable, and one naming no
/// provider, is an error that names the variable, never its value. Names compare without
/// regard to case, like every `CETRA_` variable.
fn apply_oidc_env(auth: &mut AuthSettings, env: &[(&str, &str)]) -> Result<(), ConfigError> {
    for (name, value) in env {
        let Some(dotted) = split_key(name) else {
            continue;
        };
        if dotted != "auth.oidc" && !dotted.starts_with("auth.oidc.") {
            continue;
        }
        let provider = auth
            .oidc
            .iter_mut()
            .find(|provider| name.eq_ignore_ascii_case(&oidc_env_name(&provider.name)));
        let Some(provider) = provider else {
            return Err(ConfigError::Provider {
                key: name.to_ascii_uppercase(),
                reason: "only CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET is read, for a provider of the config file",
            });
        };
        provider.client_secret = Secret((*value).to_owned());
    }
    Ok(())
}

/// Also read by the debug bundle (track TL M3).
pub(crate) fn read_toml(path: Option<&Path>) -> Result<Option<String>, ConfigError> {
    let Some(path) = configured_path(path) else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ConfigError::MissingFile(path))
        }
        Err(error) => Err(ConfigError::Figment(Box::new(figment::Error::from(
            error.to_string(),
        )))),
    }
}

/// Also read by the debug bundle (track TL M3).
pub(crate) fn configured_path(path: Option<&Path>) -> Option<PathBuf> {
    match path {
        Some(path) => Some(path.to_path_buf()),
        None => std::env::var_os("CETRA_CONFIG").map(PathBuf::from),
    }
}

fn path_from_value(value: Value) -> Result<PathBuf, ConfigError> {
    match value.into_string() {
        Some(path) => Ok(PathBuf::from(path)),
        None => Err(ConfigError::Invalid {
            key: "data_dir",
            reason: "expected a string",
        }),
    }
}

fn path_string(path: &Path) -> Result<String, ConfigError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or(ConfigError::Invalid {
            key: "data_dir",
            reason: "must be valid UTF-8",
        })
}

fn map_figment(error: figment::Error) -> ConfigError {
    for part in error.clone() {
        // The toml parser quotes the offending line, which may hold a secret. Its first line
        // names the position only.
        let text = part.to_string();
        if part.path.is_empty() && text.starts_with("TOML parse error") {
            let position = text.lines().next().unwrap_or("TOML parse error").to_owned();
            return ConfigError::Figment(Box::new(figment::Error::from(position)));
        }
        if let Some(key) = secret_key(&part.path) {
            // Figment's own text can quote the rejected URL, password included.
            // Only the static reason is kept.
            let invalid_url = part.to_string().contains("invalid URL");
            return ConfigError::Invalid {
                key,
                reason: if invalid_url {
                    "invalid URL"
                } else {
                    "expected a string"
                },
            };
        }
    }
    ConfigError::Figment(Box::new(error))
}

fn secret_key(path: &[String]) -> Option<&'static str> {
    match path {
        [key] if key == "encryption_key" => Some("encryption_key"),
        [key] if key == "license_key" => Some("license_key"),
        [key] if key == "smtp_url" => Some("smtp_url"),
        [parent, key] if parent == "init" && key == "admin_password" => Some("init.admin_password"),
        [parent, key] if parent == "init" && key == "secret_key" => Some("init.secret_key"),
        [parent, key] if parent == "init" && key == "read_key" => Some("init.read_key"),
        [section, provider, key]
            if section == "auth" && provider == "github" && key == "client_secret" =>
        {
            Some("auth.github.client_secret")
        }
        [section, provider, key]
            if section == "auth" && provider == "google" && key == "client_secret" =>
        {
            Some("auth.google.client_secret")
        }
        [parent, key] if parent == "self_monitoring" && key == "otlp_headers" => {
            Some("self_monitoring.otlp_headers")
        }
        [parent, key] if parent == "analytics" && key == "posthog_key" => {
            Some("analytics.posthog_key")
        }
        [..] if path.last().is_some_and(|key| key == "client_secret") => {
            Some("auth.oidc.client_secret")
        }
        _ => None,
    }
}

fn validate(config: &mut ServerConfig) -> Result<(), ConfigError> {
    normalize_domains("auth.signup_domains", &mut config.auth.signup_domains)?;
    normalize_domains(
        "auth.enforce_sso_domains",
        &mut config.auth.enforce_sso_domains,
    )?;
    validate_email(config)?;
    validate_entitlements(&mut config.entitlements)?;
    validate_orgs(&config.orgs)?;
    validate_demo(&config.demo)?;
    validate_telemetry(&mut config.telemetry)?;
    validate_analytics(&mut config.analytics)?;
    if config
        .license_key
        .as_ref()
        .is_some_and(|key| key.expose().trim().is_empty())
    {
        config.license_key = None;
    }
    if config.auth.require_email_verification && config.smtp_url.is_none() {
        return Err(ConfigError::Invalid {
            key: "auth.require_email_verification",
            reason: "email verification needs `smtp_url`",
        });
    }
    validate_auth(&mut config.auth)?;
    validate_init(&config.init)?;
    validate_self_monitoring(&config.self_monitoring)?;
    if config
        .otlp_http_addr
        .is_some_and(|addr| same_listener(addr, config.http_addr))
    {
        return Err(ConfigError::Invalid {
            key: "otlp_http_addr",
            reason: "must differ from http_addr",
        });
    }
    if config
        .grpc_addr
        .is_some_and(|addr| same_listener(addr, config.http_addr))
    {
        return Err(ConfigError::Invalid {
            key: "grpc_addr",
            reason: "must differ from http_addr",
        });
    }
    if let (Some(grpc), Some(otlp)) = (config.grpc_addr, config.otlp_http_addr)
        && same_listener(grpc, otlp)
    {
        return Err(ConfigError::Invalid {
            key: "grpc_addr",
            reason: "must differ from otlp_http_addr",
        });
    }
    if config
        .metrics_addr
        .is_some_and(|addr| same_listener(addr, config.http_addr))
    {
        return Err(ConfigError::Invalid {
            key: "metrics_addr",
            reason: "must differ from http_addr",
        });
    }
    if config.metrics_addr.is_some_and(|addr| {
        config
            .otlp_http_addr
            .is_some_and(|other| same_listener(addr, other))
    }) {
        return Err(ConfigError::Invalid {
            key: "metrics_addr",
            reason: "must differ from otlp_http_addr",
        });
    }
    if config.metrics_addr.is_some_and(|addr| {
        config
            .grpc_addr
            .is_some_and(|other| same_listener(addr, other))
    }) {
        return Err(ConfigError::Invalid {
            key: "metrics_addr",
            reason: "must differ from grpc_addr",
        });
    }
    validate_ingest(&config.ingest, &config.media)?;
    config.storage.validate().map_err(|error| match error {
        cetra_storage::error::ConfigError::Invalid { field, rule } => ConfigError::Invalid {
            key: field,
            reason: rule,
        },
    })?;
    if !(1..=256).contains(&config.eval.workers) {
        return Err(ConfigError::Invalid {
            key: "eval.workers",
            reason: "must be between 1 and 256",
        });
    }
    if !(1..=64).contains(&config.issues.workers) {
        return Err(ConfigError::Invalid {
            key: "issues.workers",
            reason: "must be between 1 and 64",
        });
    }
    Ok(())
}

/// The bounds of spec 12 and D-G-5. Messages name the key and the rule only.
fn validate_ingest(ingest: &IngestConfig, media: &MediaConfig) -> Result<(), ConfigError> {
    let sizes = [
        ("ingest.max_request_bytes", ingest.max_request_bytes),
        ("ingest.max_attribute_bytes", ingest.max_attribute_bytes),
        ("ingest.max_inflight_bytes", ingest.max_inflight_bytes),
        (
            "ingest.max_grpc_message_bytes",
            ingest.max_grpc_message_bytes,
        ),
        ("media.min_bytes", media.min_bytes),
        ("media.max_bytes", media.max_bytes),
    ];
    for (key, size) in sizes {
        if usize::try_from(size.as_u64()).is_err() {
            return Err(ConfigError::Invalid {
                key,
                reason: "too large for this platform",
            });
        }
    }
    let request = ingest.max_request_bytes;
    if request < ByteSize::kib(1) || request > ByteSize::gib(1) {
        return Err(invalid_size(
            "ingest.max_request_bytes",
            "must be between 1KiB and 1GiB",
        ));
    }
    if ingest.max_spans_per_request < 1 {
        return Err(ConfigError::Invalid {
            key: "ingest.max_spans_per_request",
            reason: "must be at least 1",
        });
    }
    let attribute = ingest.max_attribute_bytes;
    if attribute < ByteSize::kib(1) || attribute > ByteSize::mib(64) {
        return Err(invalid_size(
            "ingest.max_attribute_bytes",
            "must be between 1KiB and 64MiB",
        ));
    }
    // The budget is a semaphore counted in bytes, and tokio takes a `u32` count.
    let inflight = ingest.max_inflight_bytes;
    if inflight < request || inflight.as_u64() > u64::from(u32::MAX) {
        return Err(invalid_size(
            "ingest.max_inflight_bytes",
            "must be at least max_request_bytes and below 4GiB",
        ));
    }
    let grpc = ingest.max_grpc_message_bytes;
    if grpc < ByteSize::kib(1) || grpc > ByteSize::gib(1) {
        return Err(invalid_size(
            "ingest.max_grpc_message_bytes",
            "must be between 1KiB and 1GiB",
        ));
    }
    if grpc > inflight {
        return Err(invalid_size(
            "ingest.max_grpc_message_bytes",
            "must be at most max_inflight_bytes",
        ));
    }
    if media.min_bytes < ByteSize::b(1) || media.min_bytes > media.max_bytes {
        return Err(invalid_size(
            "media.min_bytes",
            "must be at least 1 byte and at most media.max_bytes",
        ));
    }
    if media.max_bytes > ByteSize::gib(1) {
        return Err(invalid_size("media.max_bytes", "must be at most 1GiB"));
    }
    Ok(())
}

fn invalid_size(key: &'static str, reason: &'static str) -> ConfigError {
    ConfigError::Invalid { key, reason }
}

fn usize_or_max(size: ByteSize) -> usize {
    usize::try_from(size.as_u64()).unwrap_or(usize::MAX)
}

fn normalize_domains(key: &'static str, domains: &mut [String]) -> Result<(), ConfigError> {
    for domain in domains {
        *domain = domain.to_lowercase();
        if !domain_ok(domain) {
            return Err(ConfigError::Invalid {
                key,
                reason: "invalid domain",
            });
        }
    }
    Ok(())
}

/// Two listen addresses collide when they are equal; port 0 asks the OS for a distinct free
/// port per listener, so it never collides.
fn same_listener(a: SocketAddr, b: SocketAddr) -> bool {
    a == b && a.port() != 0
}

fn validate_auth(auth: &mut AuthSettings) -> Result<(), ConfigError> {
    if let Some(github) = &auth.github {
        require_client(
            "auth.github.client_id",
            "auth.github.client_secret",
            &github.client_id,
            github.client_secret.expose(),
        )?;
    }
    if let Some(google) = &mut auth.google {
        require_client(
            "auth.google.client_id",
            "auth.google.client_secret",
            &google.client_id,
            google.client_secret.expose(),
        )?;
        normalize_domains("auth.google.allowed_domains", &mut google.allowed_domains)?;
    }
    let mut names = BTreeSet::new();
    for provider in &mut auth.oidc {
        validate_oidc(provider, &mut names)?;
    }
    let configured = auth.github.is_some() || auth.google.is_some() || !auth.oidc.is_empty();
    if auth.disable_password && !configured {
        return Err(ConfigError::Invalid {
            key: "auth.disable_password",
            reason: "a provider is required when passwords are disabled",
        });
    }
    if !auth.enforce_sso_domains.is_empty() && !configured {
        return Err(ConfigError::Invalid {
            key: "auth.enforce_sso_domains",
            reason: "a provider is required when SSO domains are set",
        });
    }
    Ok(())
}

fn require_client(
    id_key: &'static str,
    secret_key: &'static str,
    client_id: &str,
    client_secret: &str,
) -> Result<(), ConfigError> {
    if client_id.is_empty() {
        return Err(ConfigError::Invalid {
            key: id_key,
            reason: "client id is required",
        });
    }
    if client_secret.is_empty() {
        return Err(ConfigError::Invalid {
            key: secret_key,
            reason: "client secret is required",
        });
    }
    Ok(())
}

fn validate_oidc(provider: &mut OidcAuth, names: &mut BTreeSet<String>) -> Result<(), ConfigError> {
    if !oidc_name_ok(&provider.name) {
        return Err(ConfigError::Provider {
            key: "auth.oidc.name".to_owned(),
            reason: "name must be 1-32 characters of [a-z0-9-]",
        });
    }
    if !names.insert(provider.name.clone()) {
        return Err(ConfigError::Provider {
            key: format!("auth.oidc.{}", provider.name),
            reason: "duplicate name",
        });
    }
    let name = provider.name.clone();
    provider.display_name = provider.display_name.trim().to_owned();
    if provider.display_name.is_empty() {
        return Err(ConfigError::Provider {
            key: format!("auth.oidc.{name}.display_name"),
            reason: "display name is required",
        });
    }
    validate_issuer(format!("auth.oidc.{name}.issuer"), &provider.issuer)?;
    if provider.client_id.is_empty() {
        return Err(ConfigError::Provider {
            key: format!("auth.oidc.{name}.client_id"),
            reason: "client id is required",
        });
    }
    if provider.client_secret.expose().is_empty() {
        return Err(ConfigError::Provider {
            key: format!("auth.oidc.{name}.client_secret"),
            reason: "client secret is required",
        });
    }
    if !provider.scopes.iter().any(|scope| scope == "openid") {
        return Err(ConfigError::Provider {
            key: format!("auth.oidc.{name}.scopes"),
            reason: "scopes must include openid",
        });
    }
    normalize_named_domains(
        format!("auth.oidc.{name}.allowed_domains"),
        &mut provider.allowed_domains,
    )?;
    if let Some(provision) = &provider.auto_provision {
        validate_provision(&name, provision)?;
    }
    Ok(())
}

fn oidc_name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn validate_issuer(key: String, issuer: &str) -> Result<(), ConfigError> {
    match issuer_problem(issuer) {
        None => Ok(()),
        Some(reason) => Err(ConfigError::Provider { key, reason }),
    }
}

/// Why `issuer` cannot be an OIDC issuer: not a URL, user info, not `https` (or `http` on a
/// loopback host), or a multi-tenant Microsoft issuer. `None` when it can.
pub(crate) fn issuer_problem(issuer: &str) -> Option<&'static str> {
    let Ok(url) = url::Url::parse(issuer) else {
        return Some("invalid issuer");
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Some("invalid issuer");
    }
    let loopback = loopback_host(&url);
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Some("issuer must be https, or http on a loopback host");
    }
    if multi_tenant_entra(&url) {
        return Some("multi-tenant Microsoft issuers are not allowed");
    }
    None
}

pub(crate) fn loopback_host(url: &url::Url) -> bool {
    match url.host() {
        // `host_str` for IPv6 includes the brackets (`[::1]`). Match the parsed host.
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(addr)) => addr == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(addr)) => addr == std::net::Ipv6Addr::LOCALHOST,
        _ => false,
    }
}

fn multi_tenant_entra(url: &url::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if !host.eq_ignore_ascii_case("login.microsoftonline.com") {
        return false;
    }
    let segment = url.path().split('/').find(|segment| !segment.is_empty());
    matches!(
        segment.map(str::to_ascii_lowercase).as_deref(),
        Some("common" | "organizations" | "consumers")
    )
}

fn validate_provision(name: &str, provision: &AutoProvision) -> Result<(), ConfigError> {
    if crate::meta::orgs::validate_slug(&provision.org).is_err() {
        return Err(ConfigError::Provider {
            key: format!("auth.oidc.{name}.auto_provision.org"),
            reason: "invalid org slug",
        });
    }
    let reason = match provision.role.as_str() {
        "admin" | "member" | "viewer" => return Ok(()),
        "owner" => "role must not be owner",
        _ => "invalid role",
    };
    Err(ConfigError::Provider {
        key: format!("auth.oidc.{name}.auto_provision.role"),
        reason,
    })
}

fn normalize_named_domains(key: String, domains: &mut [String]) -> Result<(), ConfigError> {
    for domain in domains {
        *domain = domain.to_lowercase();
        if !domain_ok(domain) {
            return Err(ConfigError::Provider {
                key,
                reason: "invalid domain",
            });
        }
    }
    Ok(())
}

fn domain_ok(domain: &str) -> bool {
    !domain.is_empty()
        && !domain.contains('@')
        && !domain.chars().any(|c| c.is_whitespace() || c.is_control())
        && domain.split('.').all(|label| !label.is_empty())
}

fn validate_email(config: &mut ServerConfig) -> Result<(), ConfigError> {
    let trimmed = config
        .email_from
        .as_deref()
        .map(str::trim)
        .filter(|from| !from.is_empty())
        .map(str::to_owned);
    config.email_from = trimmed;
    if config
        .email_from
        .as_deref()
        .is_some_and(|from| from.parse::<Mailbox>().is_err())
    {
        return Err(ConfigError::Invalid {
            key: "email_from",
            reason: "invalid mailbox",
        });
    }
    if config.smtp_url.is_some() && config.email_from.is_none() {
        return Err(ConfigError::Invalid {
            key: "email_from",
            reason: "required when smtp_url is set",
        });
    }
    Ok(())
}

fn deserialize_smtp_url<'de, D>(deserializer: D) -> Result<Option<SmtpUrl>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(secret) = Option::<Secret<String>>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let raw = secret.expose().trim();
    if raw.is_empty() {
        return Ok(None);
    }
    parse_smtp_url(raw)
        .map(Some)
        .map_err(|()| de::Error::custom("invalid URL"))
}

fn parse_smtp_url(raw: &str) -> Result<SmtpUrl, ()> {
    let url = url::Url::parse(raw).map_err(|_| ())?;
    if url.fragment().is_some() {
        return Err(());
    }
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return Err(());
    }
    let host = url.host_str().filter(|host| !host.is_empty()).ok_or(())?;
    let tls = match url.scheme() {
        "smtp" => match url.query() {
            None => Tls::Required,
            Some("tls=none") => Tls::None,
            Some(_) => return Err(()),
        },
        "smtps" if url.query().is_none() => Tls::Wrapper,
        _ => return Err(()),
    };
    let port = url.port().unwrap_or(match tls {
        Tls::Required => 587,
        Tls::Wrapper => 465,
        Tls::None => 25,
    });
    let credentials = if url.username().is_empty() {
        if url.password().is_some() {
            return Err(());
        }
        None
    } else {
        Some((
            percent_decode(url.username())?,
            Secret(percent_decode(url.password().unwrap_or(""))?),
        ))
    };
    Ok(SmtpUrl {
        tls,
        host: host.to_owned(),
        port,
        credentials,
    })
}

/// `username()` and `password()` keep `%HH` sequences. Userinfo uses `%20`, not `+`.
fn percent_decode(raw: &str) -> Result<String, ()> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let high = hex_digit(bytes[index + 1])?;
            let low = hex_digit(bytes[index + 2])?;
            out.push((high << 4) | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn hex_digit(byte: u8) -> Result<u8, ()> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(()),
    }
}

fn validate_entitlements(entitlements: &mut EntitlementsConfig) -> Result<(), ConfigError> {
    if let Some(template) = entitlements.upgrade_url.as_deref() {
        validate_upgrade_url(template)?;
    }
    let provider = entitlements.control_plane_provider.trim().to_owned();
    if provider.is_empty() {
        return Err(ConfigError::Invalid {
            key: "entitlements.control_plane_provider",
            reason: "must not be empty",
        });
    }
    entitlements.control_plane_provider = provider;
    validate_entitlement_defaults(&mut entitlements.defaults)
}

fn validate_entitlement_defaults(defaults: &mut EntitlementDefaults) -> Result<(), ConfigError> {
    let plan_name = defaults.plan_name.trim().to_owned();
    if !(1..=64).contains(&plan_name.chars().count()) {
        return Err(ConfigError::Invalid {
            key: "entitlements.defaults.plan_name",
            reason: "must be 1 to 64 characters",
        });
    }
    defaults.plan_name = plan_name;
    let core_only =
        |name: &String| Feature::from_name(name).is_some_and(|feature| !feature.is_ee());
    if defaults
        .features
        .as_ref()
        .is_some_and(|names| !names.iter().all(core_only))
    {
        return Err(ConfigError::Invalid {
            key: "entitlements.defaults.features",
            reason: "must list core features only: retention_management",
        });
    }
    Ok(())
}

fn validate_upgrade_url(template: &str) -> Result<(), ConfigError> {
    let invalid = ConfigError::Invalid {
        key: "entitlements.upgrade_url",
        reason: "must be an http or https URL; {org} is the only placeholder",
    };
    let sample = template.replace("{org}", "00000000-0000-7000-8000-000000000000");
    if sample.contains('{') || sample.contains('}') || !is_http_url(&sample) {
        return Err(invalid);
    }
    Ok(())
}

/// `[demo]`: each count is at least 1, the user cap does not exceed the org cap, and the SQL caps
/// stay within the console's own bounds.
fn validate_demo(demo: &DemoConfig) -> Result<(), ConfigError> {
    let counts = [
        ("demo.requests_per_minute", demo.requests_per_minute),
        ("demo.user_in_flight", demo.user_in_flight),
        ("demo.org_in_flight", demo.org_in_flight),
    ];
    for (key, value) in counts {
        if value == 0 {
            return Err(ConfigError::Invalid {
                key,
                reason: "must be at least 1",
            });
        }
    }
    if demo.user_in_flight > demo.org_in_flight {
        return Err(ConfigError::Invalid {
            key: "demo.user_in_flight",
            reason: "must not exceed demo.org_in_flight",
        });
    }
    if !(Duration::from_secs(1)..=Duration::from_secs(60)).contains(&demo.sql_timeout) {
        return Err(ConfigError::Invalid {
            key: "demo.sql_timeout",
            reason: "must be 1 to 60 seconds",
        });
    }
    if !(1..=10_000).contains(&demo.sql_max_rows) {
        return Err(ConfigError::Invalid {
            key: "demo.sql_max_rows",
            reason: "must be 1 to 10000",
        });
    }
    Ok(())
}

/// An outbound base URL of operator config (`telemetry.endpoint`, `analytics.posthog_host`):
/// `https`, or `http` to a loopback IP address; no user info, query or fragment. Returned
/// without a trailing slash. Reasons are static and never quote the input.
fn outbound_base_url(raw: &str, key: &'static str) -> Result<String, ConfigError> {
    let invalid = |reason| ConfigError::Invalid { key, reason };
    let url = url::Url::parse(raw.trim())
        .map_err(|_| invalid("must be an absolute http or https URL"))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return Err(invalid("must be https, or http to a loopback address")),
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid("must not contain user info, a query or a fragment"));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// `telemetry.endpoint` (D-TL-10), by [`outbound_base_url`].
fn validate_telemetry(telemetry: &mut TelemetryConfig) -> Result<(), ConfigError> {
    telemetry.endpoint = outbound_base_url(&telemetry.endpoint, "telemetry.endpoint")?;
    Ok(())
}

/// `[analytics]`: blank values are unset; both keys or neither; the host by
/// [`outbound_base_url`]; the key printable ASCII without spaces, at most 256 bytes.
fn validate_analytics(analytics: &mut AnalyticsConfig) -> Result<(), ConfigError> {
    if analytics
        .posthog_host
        .as_deref()
        .is_some_and(|host| host.trim().is_empty())
    {
        analytics.posthog_host = None;
    }
    if analytics
        .posthog_key
        .as_ref()
        .is_some_and(|key| key.expose().trim().is_empty())
    {
        analytics.posthog_key = None;
    }
    match (&analytics.posthog_host, &analytics.posthog_key) {
        (None, None) => return Ok(()),
        (Some(_), Some(_)) => {}
        _ => {
            return Err(ConfigError::Invalid {
                key: "analytics",
                reason: ANALYTICS_PAIR,
            });
        }
    }
    if let Some(host) = analytics.posthog_host.take() {
        analytics.posthog_host = Some(outbound_base_url(&host, "analytics.posthog_host")?);
    }
    if let Some(key) = &analytics.posthog_key {
        let value = key.expose();
        if value.len() > 256 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ConfigError::Invalid {
                key: "analytics.posthog_key",
                reason: "must be printable ASCII without spaces, at most 256 bytes",
            });
        }
    }
    Ok(())
}

fn validate_orgs(orgs: &OrgsConfig) -> Result<(), ConfigError> {
    let Some(url) = orgs.create_url.as_deref() else {
        return Ok(());
    };
    if url.contains('{') || url.contains('}') || !is_http_url(url) {
        return Err(ConfigError::Invalid {
            key: "orgs.create_url",
            reason: "must be an http or https URL",
        });
    }
    Ok(())
}

/// An absolute http or https URL with a host.
fn is_http_url(value: &str) -> bool {
    // `url` reads `https:///host` as host `host`; the empty authority is refused here.
    let has_authority = value
        .split_once("://")
        .is_some_and(|(_, rest)| !rest.starts_with(['/', '\\']));
    match url::Url::parse(value) {
        Ok(url) => {
            has_authority && matches!(url.scheme(), "http" | "https") && url.host().is_some()
        }
        Err(_) => false,
    }
}

fn validate_init(init: &InitConfig) -> Result<(), ConfigError> {
    let parts = [
        text_set(&init.admin_email),
        secret_set(&init.admin_password),
        text_set(&init.org),
        text_set(&init.project),
    ];
    let set = parts.into_iter().filter(|present| *present).count();
    if set == 0 {
        return Ok(());
    }
    if set != parts.len() {
        return Err(ConfigError::Invalid {
            key: "init",
            reason: INIT_GROUP,
        });
    }
    match init.admin_email.as_deref() {
        Some(email) if email_ok(email) => Ok(()),
        Some(_) => Err(ConfigError::Invalid {
            key: "init.admin_email",
            reason: "invalid email",
        }),
        None => Err(ConfigError::Invalid {
            key: "init",
            reason: INIT_GROUP,
        }),
    }
}

fn text_set(value: &Option<String>) -> bool {
    value.as_ref().is_some_and(|text| !text.trim().is_empty())
}

fn secret_set(value: &Option<Secret<String>>) -> bool {
    value
        .as_ref()
        .is_some_and(|secret| !secret.expose().trim().is_empty())
}

/// Structural email check from the identity plan, without rewriting the value.
fn email_ok(email: &str) -> bool {
    if email.len() > 254 || email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !domain.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && domain.split('.').all(|label| !label.is_empty())
}

fn parse_public_url(raw: &str) -> Result<PublicUrl, &'static str> {
    let url = url::Url::parse(raw).map_err(|_| "must be an http or https URL")?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("must use http or https");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not contain user info");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not have a query or fragment");
    }
    if url.path() != "/" && !url.path().is_empty() {
        return Err("must not have a path");
    }
    let host = match url.host() {
        Some(url::Host::Ipv6(addr)) => format!("[{addr}]"),
        Some(url::Host::Ipv4(addr)) => addr.to_string(),
        Some(url::Host::Domain(domain)) => domain.to_owned(),
        None => return Err("must have a host"),
    };
    let origin = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    Ok(PublicUrl {
        https: url.scheme() == "https",
        origin,
    })
}

/// Most entries in `self_monitoring.otlp_headers` or `self_monitoring.resource`.
pub(crate) const MAX_PAIRS: usize = 32;

/// Longest `self_monitoring.otlp_headers` or `self_monitoring.resource` value, in bytes.
pub(crate) const MAX_PAIRS_BYTES: usize = 8192;

/// Headers cetra sets itself on every export request.
const RESERVED_HEADERS: &[&str] = &[
    "content-type",
    "content-encoding",
    "content-length",
    "host",
    "user-agent",
];

/// Why a reserved header is refused.
const RESERVED_HEADER_REASON: &str =
    "content-type, content-encoding, content-length, host and user-agent are set by cetra";

/// Checks `self_monitoring.otlp_endpoint`. The reasons never quote the input.
pub(crate) fn parse_otlp_endpoint(raw: &str) -> Result<OtlpEndpoint, &'static str> {
    let raw = raw.trim();
    if !is_http_url(raw) {
        return Err("must be an http or https URL");
    }
    let url = url::Url::parse(raw).map_err(|_| "must be an http or https URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not contain user info");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not have a query or fragment");
    }
    let path = url.path().trim_end_matches('/');
    if path.ends_with("/v1/logs") || path.ends_with("/v1/traces") {
        return Err("must be the base URL, without /v1/logs or /v1/traces");
    }
    let Some(host) = url.host_str() else {
        return Err("must have a host");
    };
    Ok(OtlpEndpoint {
        host: host.to_owned(),
        // No query or fragment, so the serialization ends with the path.
        base: url.as_str().trim_end_matches('/').to_owned(),
    })
}

/// `name=value` entries separated by `,` (the `OTEL_EXPORTER_OTLP_HEADERS` and
/// `OTEL_RESOURCE_ATTRIBUTES` syntax). Each entry is split at its first `=`. Names and values
/// are trimmed, and values are percent-decoded (`%20` is a space; `+` stays `+`, as in
/// base64). Empty entries are skipped. The reasons never quote the input.
fn parse_pairs(raw: &str) -> Result<Vec<(String, String)>, &'static str> {
    if raw.len() > MAX_PAIRS_BYTES {
        return Err("at most 8192 bytes");
    }
    let mut pairs = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((name, value)) = entry.split_once('=') else {
            return Err("entries must be name=value");
        };
        let name = name.trim();
        if name.is_empty() {
            return Err("a name is empty");
        }
        let value =
            percent_decode(value.trim()).map_err(|()| "a value is not valid percent-encoding")?;
        pairs.push((name.to_owned(), value));
    }
    if pairs.len() > MAX_PAIRS {
        return Err("at most 32 entries");
    }
    Ok(pairs)
}

/// `self_monitoring.otlp_headers` as `(lower-case name, value)` pairs, in order.
pub(crate) fn parse_otlp_headers(raw: &str) -> Result<Vec<(String, String)>, &'static str> {
    let mut headers = parse_pairs(raw)?;
    let mut seen = BTreeSet::new();
    for (name, value) in &mut headers {
        if !name.bytes().all(is_token_byte) {
            return Err("a header name is not a valid HTTP token");
        }
        name.make_ascii_lowercase();
        if RESERVED_HEADERS.contains(&name.as_str()) {
            return Err(RESERVED_HEADER_REASON);
        }
        if !seen.insert(name.clone()) {
            return Err("a name is listed twice");
        }
        if !value
            .bytes()
            .all(|byte| byte == b'\t' || (b' '..=b'~').contains(&byte))
        {
            return Err("a header value has a character HTTP does not allow");
        }
    }
    Ok(headers)
}

/// RFC 9110 `tchar`.
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// `self_monitoring.resource` as `(key, value)` pairs, in order.
pub(crate) fn parse_resource(raw: &str) -> Result<Vec<(String, String)>, &'static str> {
    let pairs = parse_pairs(raw)?;
    let mut seen = BTreeSet::new();
    for (key, _) in &pairs {
        if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("a key has whitespace or a control character");
        }
        if !seen.insert(key.as_str()) {
            return Err("a name is listed twice");
        }
    }
    Ok(pairs)
}

fn validate_self_monitoring(monitoring: &SelfMonitoringConfig) -> Result<(), ConfigError> {
    if let Some(headers) = &monitoring.otlp_headers {
        parse_otlp_headers(headers.expose()).map_err(|reason| ConfigError::Invalid {
            key: "self_monitoring.otlp_headers",
            reason,
        })?;
    }
    parse_resource(&monitoring.resource).map_err(|reason| ConfigError::Invalid {
        key: "self_monitoring.resource",
        reason,
    })?;
    Ok(())
}

fn deserialize_socket_addr<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SocketAddr, D::Error> {
    let raw = String::deserialize(deserializer)?;
    raw.parse()
        .map_err(|_error| de::Error::custom("invalid socket address"))
}

/// An empty (or blank) string is `None`: the listener is disabled.
fn deserialize_optional_socket_addr<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<SocketAddr>, D::Error> {
    let raw = String::deserialize(deserializer)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse()
        .map(Some)
        .map_err(|_error| de::Error::custom("invalid socket address"))
}

/// A blank string is `None` (no export), so `CETRA_SELF_MONITORING__OTLP_ENDPOINT=` unsets it.
fn deserialize_otlp_endpoint<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<OtlpEndpoint>, D::Error> {
    let Some(raw) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    parse_otlp_endpoint(&raw)
        .map(Some)
        .map_err(de::Error::custom)
}

fn deserialize_proxies<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<IpNet>, D::Error> {
    let items = deserialize_string_list(deserializer)?;
    items
        .iter()
        .map(|item| {
            item.parse::<IpNet>()
                .map_err(|_error| de::Error::custom("invalid CIDR"))
        })
        .collect()
}

fn deserialize_string_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    deserializer.deserialize_any(StringListVisitor)
}

struct StringListVisitor;

impl<'de> Visitor<'de> for StringListVisitor {
    type Value = Vec<String>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string or a list of strings")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<String>, E> {
        Ok(split_csv(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<String>, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<String>()? {
            let trimmed = item.trim();
            if !trimmed.is_empty() {
                items.push(trimmed.to_owned());
            }
        }
        Ok(items)
    }
}

fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

struct EnvSource<'a> {
    pairs: &'a [(&'a str, &'a str)],
}

impl Provider for EnvSource<'_> {
    fn metadata(&self) -> Metadata {
        Metadata::named("CETRA_ environment")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut dict = Dict::new();
        for (name, value) in self.pairs {
            if let Some(dotted) = split_key(name) {
                // `[[auth.oidc]]` is a TOML array, which the split form cannot index.
                // `apply_oidc_env` reads these variables after extraction (D-S-2).
                if dotted == "auth.oidc" || dotted.starts_with("auth.oidc.") {
                    continue;
                }
                insert_dotted(&mut dict, &dotted, env_value(&dotted, value));
            }
        }
        for (name, value) in self.pairs {
            if let Some(dotted) = alias_key(name) {
                insert_dotted(&mut dict, dotted, env_value(dotted, value));
            }
        }
        Ok(Profile::Default.collect(dict))
    }
}

fn split_key(name: &str) -> Option<String> {
    let (prefix, rest) = name.split_at_checked("CETRA_".len())?;
    if !prefix.eq_ignore_ascii_case("CETRA_") || is_filtered(name) {
        return None;
    }
    let parts: Vec<&str> = rest.split("__").collect();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    Some(
        parts
            .into_iter()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// `CETRA_CONFIG`, `CETRA_TELEMETRY` (read by [`load_from`] before the sources, D-TL-11) and the CLI's own variables (D-H-12) are not config keys.
fn is_filtered(name: &str) -> bool {
    [
        "CETRA_CONFIG",
        "CETRA_TELEMETRY",
        "CETRA_URL",
        "CETRA_TOKEN",
        "CETRA_PROJECT",
    ]
    .iter()
    .any(|filtered| name.eq_ignore_ascii_case(filtered))
        || alias_key(name).is_some()
}

fn alias_key(name: &str) -> Option<&'static str> {
    ALIASES
        .iter()
        .find_map(|(alias, key)| name.eq_ignore_ascii_case(alias).then_some(*key))
}

fn env_value(dotted: &str, raw: &str) -> Value {
    if is_secret_path(dotted) {
        // Keep the bytes as a string. Figment's env parser would turn an
        // all-digit secret into a number and then print that number in a
        // type error.
        Value::from(raw)
    } else {
        parse_env_value(raw)
    }
}

fn is_secret_path(dotted: &str) -> bool {
    matches!(
        dotted,
        "encryption_key"
            | "license_key"
            | "smtp_url"
            | "init.admin_password"
            | "init.secret_key"
            | "init.read_key"
            | "self_monitoring.otlp_headers"
            | "analytics.posthog_key"
    ) || dotted.ends_with(".client_secret")
}

fn parse_env_value(raw: &str) -> Value {
    let value: Result<Value, std::convert::Infallible> = raw.parse();
    match value {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

fn insert_dotted(dict: &mut Dict, dotted: &str, value: Value) {
    match figment::util::nest(dotted, value) {
        Value::Dict(_, nested) => merge_dict(dict, nested),
        other => {
            dict.insert(dotted.to_owned(), other);
        }
    }
}

fn merge_dict(into: &mut Dict, from: Dict) {
    for (key, incoming) in from {
        match (into.remove(&key), incoming) {
            (Some(Value::Dict(_, mut left)), Value::Dict(_, right)) => {
                merge_dict(&mut left, right);
                into.insert(key, Value::from(left));
            }
            (_, incoming) => {
                into.insert(key, incoming);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::ids::OrgId;
    use cetra_storage::config::{ObjectStoreUrl, WalSync};
    use figment::error::{Actual, Kind};

    use super::{
        ALIASES, ANALYTICS_PAIR, AlertsConfig, AnalyticsConfig, ConfigError, DemoConfig,
        EmailRecipients, IngestConfig, LogFormat, MediaConfig, OrgsConfig, Secret,
        SelfMonitoringConfig, ServerConfig, SmtpUrl, TELEMETRY_VALUES, TelemetryConfig, Tls,
        TraceSampleRatio, issuer_problem, load_from, oidc_env_name, parse_otlp_endpoint,
        parse_otlp_headers, parse_resource,
    };
    use crate::api::account::SignupPolicy;
    use crate::observability::switchboard::TelemetryEnv;

    fn socket(raw: &str) -> SocketAddr {
        raw.parse().unwrap()
    }

    #[test]
    fn issuer_problem_names_each_reason() {
        assert_eq!(issuer_problem("https://idp.example/realm"), None);
        assert_eq!(issuer_problem("http://localhost:8080"), None);
        assert_eq!(issuer_problem("not a url"), Some("invalid issuer"));
        assert_eq!(
            issuer_problem("https://user:pw@idp.example"),
            Some("invalid issuer")
        );
        assert_eq!(
            issuer_problem("http://idp.example"),
            Some("issuer must be https, or http on a loopback host")
        );
        assert_eq!(
            issuer_problem("https://login.microsoftonline.com/common/v2.0"),
            Some("multi-tenant Microsoft issuers are not allowed")
        );
    }

    #[test]
    fn defaults_match_spec() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.data_dir, PathBuf::from("./cetra-data"));
        assert_eq!(config.public_url.origin(), "http://localhost:8080");
        assert!(!config.public_url.is_https());
        assert_eq!(config.http_addr, socket("0.0.0.0:8080"));
        assert!(config.trusted_proxies.is_empty());
        assert_eq!(config.auth.signup, SignupPolicy::InviteOnly);
        assert!(config.auth.signup_domains.is_empty());
        assert!(!config.auth.disable_password);
        assert!(!config.auth.require_email_verification);
        assert!(config.auth.enforce_sso_domains.is_empty());
        assert!(config.smtp_url.is_none());
        assert!(config.email_from.is_none());
        assert!(config.init.admin_email.is_none());
        assert!(config.init.admin_password.is_none());
        assert!(config.init.org.is_none());
        assert!(config.init.project.is_none());
        assert!(config.init.secret_key.is_none());
        assert!(config.log.format.is_none());
        assert_eq!(config.log.level, "info");
        assert_eq!(config.self_monitoring, SelfMonitoringConfig::default());

        let storage = config.storage_config();
        assert_eq!(storage.data_dir, config.data_dir);
        assert!(storage.object_store.is_none());
        assert_eq!(
            storage.object_store_url(),
            ObjectStoreUrl::Local(config.data_dir.join("objects"))
        );
        assert!(storage.s3.region.is_none());
        assert!(storage.s3.endpoint.is_none());
        assert!(!storage.s3.force_path_style);
        assert!(!storage.s3.allow_http);
        assert_eq!(storage.wal.sync, WalSync::Always);
        assert_eq!(storage.wal.segment_bytes, ByteSize::mib(64));
        assert_eq!(storage.wal.group_max_bytes, ByteSize::mib(8));
        assert_eq!(storage.wal.group_max_wait, Duration::from_millis(2));
        assert_eq!(storage.flush.max_bytes, ByteSize::mib(256));
        assert_eq!(storage.flush.max_age, Duration::from_secs(120));
        assert_eq!(storage.flush.upload_concurrency, 8);
        assert_eq!(storage.memtable.hard_limit, ByteSize::gib(1));
        assert_eq!(storage.query.metadata_cache_bytes, ByteSize::mib(512));
        assert!(storage.query.target_partitions >= 1);
        assert_eq!(config.analytics, AnalyticsConfig::default());
    }

    #[test]
    fn toml_overrides_defaults_and_env_overrides_toml() {
        let toml = r#"
            data_dir = "/from/toml"
            http_addr = "127.0.0.1:9"
        "#;
        let config = load_from(
            Some(toml),
            &[
                ("CETRA_HTTP_ADDR", "127.0.0.1:10"),
                ("CETRA_DATA_DIR", "/from/env"),
            ],
        )
        .unwrap();
        assert_eq!(config.data_dir, PathBuf::from("/from/env"));
        assert_eq!(config.storage_config().data_dir, PathBuf::from("/from/env"));
        assert_eq!(config.http_addr, socket("127.0.0.1:10"));
    }

    #[test]
    fn alias_overrides_split_form() {
        let config = load_from(
            None,
            &[
                ("CETRA_AUTH__SIGNUP", "open"),
                ("CETRA_AUTH_SIGNUP", "disabled"),
                ("CETRA_LOG__LEVEL", "warn"),
                ("CETRA_LOG__FORMAT", "json"),
                ("CETRA_LOG", "debug"),
            ],
        )
        .unwrap();
        assert_eq!(config.auth.signup, SignupPolicy::Disabled);
        assert_eq!(config.log.level, "debug");
        assert_eq!(config.log.format, Some(LogFormat::Json));
    }

    #[test]
    fn telemetry_defaults() {
        let config = load_from(None, &[]).unwrap();
        assert!(config.telemetry.heartbeat);
        assert!(!config.telemetry.crash_reports);
        assert_eq!(config.telemetry.endpoint, "https://telemetry.cetra.sh");
        assert_eq!(config.telemetry.env, TelemetryEnv::default());
    }

    #[test]
    fn cetra_telemetry_off_loads_and_keeps_the_table() {
        let config = load_from(None, &[("CETRA_TELEMETRY", "off")]).unwrap();
        assert!(config.telemetry.env.forced_off);
        assert!(config.telemetry.heartbeat);
    }

    #[test]
    fn cetra_telemetry_is_not_mapped_to_the_table() {
        let env = [
            ("CETRA_TELEMETRY", "OFF"),
            ("CETRA_TELEMETRY__HEARTBEAT", "false"),
        ];
        let config = load_from(None, &env).unwrap();
        assert!(config.telemetry.env.forced_off);
        assert!(!config.telemetry.heartbeat);
    }

    #[test]
    fn a_bad_cetra_telemetry_is_a_load_error() {
        for value in ["on", "1", "true"] {
            let result = load_from(None, &[("CETRA_TELEMETRY", value)]);
            assert!(
                matches!(
                    &result,
                    Err(ConfigError::Provider { key, reason })
                        if key == "CETRA_TELEMETRY" && *reason == TELEMETRY_VALUES
                ),
                "value {value:?}"
            );
        }
    }

    #[test]
    fn the_cetra_telemetry_error_never_quotes_the_value() {
        let error = load_from(None, &[("CETRA_TELEMETRY", "planted-value-7f3")])
            .expect_err("an invalid value is an error");
        assert!(!error.to_string().contains("planted-value-7f3"));
    }

    #[test]
    fn an_empty_cetra_telemetry_loads() {
        let config = load_from(None, &[("CETRA_TELEMETRY", "")]).unwrap();
        assert!(!config.telemetry.env.forced_off);
    }

    #[test]
    fn do_not_track_and_ci_reach_the_config() {
        let env = [("DO_NOT_TRACK", "1"), ("GITHUB_ACTIONS", "true")];
        let config = load_from(None, &env).unwrap();
        assert!(config.telemetry.env.do_not_track);
        assert!(config.telemetry.env.ci);
    }

    #[test]
    fn telemetry_heartbeat_from_toml() {
        let config = load_from(Some("[telemetry]\nheartbeat = false\n"), &[]).unwrap();
        assert!(!config.telemetry.heartbeat);
    }

    #[test]
    fn telemetry_crash_reports_from_toml() {
        let config = load_from(Some("[telemetry]\ncrash_reports = true\n"), &[]).unwrap();
        assert!(config.telemetry.crash_reports);
    }

    #[test]
    fn telemetry_crash_reports_from_env() {
        let env = [("CETRA_TELEMETRY__CRASH_REPORTS", "true")];
        let config = load_from(None, &env).unwrap();
        assert!(config.telemetry.crash_reports);
    }

    #[test]
    fn telemetry_endpoint_accepts() {
        let cases = [
            (
                "https://relay.example.com/cetra/",
                "https://relay.example.com/cetra",
            ),
            ("http://127.0.0.1:9", "http://127.0.0.1:9"),
            ("http://[::1]:8080/", "http://[::1]:8080"),
        ];
        for (raw, stored) in cases {
            let toml = format!("[telemetry]\nendpoint = \"{raw}\"\n");
            let config = load_from(Some(&toml), &[]).unwrap();
            assert_eq!(config.telemetry.endpoint, stored, "{raw}");
            let env = [("CETRA_TELEMETRY__ENDPOINT", raw)];
            let config = load_from(None, &env).unwrap();
            assert_eq!(config.telemetry.endpoint, stored, "{raw}");
        }
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.telemetry, TelemetryConfig::default());
    }

    #[test]
    fn telemetry_endpoint_refuses() {
        let absolute = "must be an absolute http or https URL";
        let scheme = "must be https, or http to a loopback address";
        let user_info = "must not contain user info, a query or a fragment";
        let cases = [
            ("", absolute),
            ("not a url", absolute),
            ("http://localhost:8080", scheme),
            ("http://relay.example.com", scheme),
            ("ftp://relay.example.com", scheme),
            ("http://10.0.0.1", scheme),
            ("https://user:pw@relay.example.com", user_info),
            ("https://relay.example.com/?a=1", user_info),
            ("https://relay.example.com/#f", user_info),
        ];
        for (raw, reason) in cases {
            let toml = format!("[telemetry]\nendpoint = \"{raw}\"\n");
            let result = load_from(Some(&toml), &[]);
            assert!(
                matches!(
                    &result,
                    Err(ConfigError::Invalid { key: "telemetry.endpoint", reason: got })
                        if *got == reason
                ),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn analytics_defaults() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.analytics.posthog_host, None);
        assert!(config.analytics.posthog_key.is_none());
        assert!(config.analytics.posthog().is_none());
    }

    #[test]
    fn analytics_loads_from_toml_and_env() {
        let toml = "[analytics]\nposthog_host = \"https://eu.i.posthog.com/\"\n";
        let env = [("CETRA_ANALYTICS__POSTHOG_KEY", "phc_abc123")];
        let config = load_from(Some(toml), &env).unwrap();
        assert_eq!(
            config.analytics.posthog_host.as_deref(),
            Some("https://eu.i.posthog.com")
        );
        let (host, key) = config.analytics.posthog().unwrap();
        assert_eq!(host, "https://eu.i.posthog.com");
        assert_eq!(key.expose(), "phc_abc123");
        assert!(!format!("{config:?}").contains("phc_abc123"));
    }

    #[test]
    fn an_all_digit_posthog_key_stays_a_string() {
        let env = [
            ("CETRA_ANALYTICS__POSTHOG_HOST", "https://eu.i.posthog.com"),
            ("CETRA_ANALYTICS__POSTHOG_KEY", "12345"),
        ];
        let config = load_from(None, &env).unwrap();
        let key = config.analytics.posthog_key.as_ref().unwrap();
        assert_eq!(key.expose(), "12345");
    }

    #[test]
    fn analytics_needs_both_keys() {
        let host_only = [("CETRA_ANALYTICS__POSTHOG_HOST", "https://eu.i.posthog.com")];
        let result = load_from(None, &host_only);
        assert!(
            matches!(
                &result,
                Err(ConfigError::Invalid { key: "analytics", reason }) if *reason == ANALYTICS_PAIR
            ),
            "{result:?}"
        );
        let key_only = [("CETRA_ANALYTICS__POSTHOG_KEY", "phc_abc123")];
        let result = load_from(None, &key_only);
        assert!(
            matches!(
                &result,
                Err(ConfigError::Invalid { key: "analytics", reason }) if *reason == ANALYTICS_PAIR
            ),
            "{result:?}"
        );
    }

    #[test]
    fn blank_analytics_values_are_unset() {
        let env = [
            ("CETRA_ANALYTICS__POSTHOG_HOST", ""),
            ("CETRA_ANALYTICS__POSTHOG_KEY", "  "),
        ];
        let config = load_from(None, &env).unwrap();
        assert_eq!(config.analytics.posthog_host, None);
        assert!(config.analytics.posthog_key.is_none());
    }

    #[test]
    fn posthog_host_refuses() {
        let absolute = "must be an absolute http or https URL";
        let scheme = "must be https, or http to a loopback address";
        let user_info = "must not contain user info, a query or a fragment";
        let cases = [
            ("not a url", absolute),
            ("http://eu.i.posthog.com", scheme),
            ("http://localhost:8000", scheme),
            ("ftp://h.example", scheme),
            ("https://u:p@h.example", user_info),
            ("https://h.example/?a=1", user_info),
            ("https://h.example/#f", user_info),
        ];
        for (raw, reason) in cases {
            let env = [
                ("CETRA_ANALYTICS__POSTHOG_HOST", raw),
                ("CETRA_ANALYTICS__POSTHOG_KEY", "phc_abc123"),
            ];
            let result = load_from(None, &env);
            assert!(
                matches!(
                    &result,
                    Err(ConfigError::Invalid { key: "analytics.posthog_host", reason: got })
                        if *got == reason
                ),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn posthog_host_accepts_loopback_http() {
        let env = [
            ("CETRA_ANALYTICS__POSTHOG_HOST", "http://127.0.0.1:9999/"),
            ("CETRA_ANALYTICS__POSTHOG_KEY", "phc_abc123"),
        ];
        let config = load_from(None, &env).unwrap();
        assert_eq!(
            config.analytics.posthog_host.as_deref(),
            Some("http://127.0.0.1:9999")
        );
    }

    #[test]
    fn posthog_key_refuses_without_quoting_it() {
        let long = "a".repeat(257);
        for value in ["phc abc", long.as_str(), "phc\u{7f}x"] {
            let env = [
                ("CETRA_ANALYTICS__POSTHOG_HOST", "https://eu.i.posthog.com"),
                ("CETRA_ANALYTICS__POSTHOG_KEY", value),
            ];
            let error = load_from(None, &env).err().unwrap();
            assert!(
                matches!(
                    error,
                    ConfigError::Invalid {
                        key: "analytics.posthog_key",
                        ..
                    }
                ),
                "{error}"
            );
            let text = error.to_string();
            assert!(!text.contains(value), "{text}");
            assert!(!text.contains("phc"), "{text}");
        }
    }

    #[test]
    fn a_bad_posthog_key_type_is_reported_by_key() {
        let toml = "[analytics]\nposthog_host = \"https://eu.i.posthog.com\"\nposthog_key = 5\n";
        let error = load_from(Some(toml), &[]).err().unwrap();
        assert!(matches!(
            error,
            ConfigError::Invalid {
                key: "analytics.posthog_key",
                reason: "expected a string",
            }
        ));
    }

    #[test]
    fn unknown_analytics_key_is_refused() {
        let result = load_from(Some("[analytics]\nposthog = 1\n"), &[]);
        assert!(matches!(result, Err(ConfigError::Figment(_))));
    }

    #[test]
    fn telemetry_env_is_not_a_key() {
        let result = load_from(Some("[telemetry]\nenv = 1\n"), &[]);
        assert!(matches!(result, Err(ConfigError::Figment(_))));
    }

    #[test]
    fn self_monitoring_defaults() {
        let config = load_from(None, &[]).unwrap();
        let monitoring = &config.self_monitoring;
        assert!(monitoring.otlp_endpoint.is_none());
        assert!(monitoring.otlp_headers.is_none());
        assert!(monitoring.logs);
        assert_eq!(monitoring.level, "warn");
        assert_eq!(monitoring.resource, "");
        assert!(monitoring.logs_endpoint().is_none());
        assert!(!monitoring.exports());
        assert!(monitoring.traces);
        assert_eq!(monitoring.trace_sample_ratio.get(), 0.1);
        assert!(monitoring.traces_endpoint().is_none());
    }

    #[test]
    fn self_monitoring_loads_from_toml_and_env() {
        let toml = "[self_monitoring]\notlp_endpoint = \"https://otlp.example.com/otlp/\"\n";
        let env = [
            (
                "CETRA_SELF_MONITORING__OTLP_HEADERS",
                "authorization=Basic%20abc",
            ),
            ("CETRA_SELF_MONITORING__LOGS", "false"),
            ("CETRA_SELF_MONITORING__TRACES", "false"),
            ("CETRA_SELF_MONITORING__LEVEL", "info"),
            (
                "CETRA_SELF_MONITORING__RESOURCE",
                "deployment.environment=prod,cetra.cell=eu1",
            ),
        ];
        let config = load_from(Some(toml), &env).unwrap();
        let monitoring = &config.self_monitoring;
        let endpoint = monitoring.otlp_endpoint.as_ref().unwrap();
        assert_eq!(endpoint.logs_url(), "https://otlp.example.com/otlp/v1/logs");
        assert_eq!(endpoint.host(), "otlp.example.com");
        assert!(!monitoring.logs);
        assert!(monitoring.logs_endpoint().is_none());
        assert!(!monitoring.traces);
        assert!(monitoring.traces_endpoint().is_none());
        assert!(!monitoring.exports());
        assert_eq!(monitoring.level, "info");
        assert_eq!(
            monitoring.resource,
            "deployment.environment=prod,cetra.cell=eu1"
        );
        assert_eq!(
            format!("{:?}", monitoring.otlp_headers.as_ref().unwrap()),
            "Secret(***)"
        );

        // The same file without the env overrides exports logs.
        let config = load_from(Some(toml), &[]).unwrap();
        assert!(config.self_monitoring.logs);
        assert!(config.self_monitoring.exports());
    }

    #[test]
    fn otlp_endpoint_rules() {
        let accepted = [
            (
                "https://o1.ingest.de.sentry.io/api/2/integration/otlp",
                "https://o1.ingest.de.sentry.io/api/2/integration/otlp/v1/logs",
            ),
            ("http://collector:4318", "http://collector:4318/v1/logs"),
            ("http://collector:4318/", "http://collector:4318/v1/logs"),
            (
                "https://otlp-gateway-prod-eu-west-2.grafana.net/otlp//",
                "https://otlp-gateway-prod-eu-west-2.grafana.net/otlp/v1/logs",
            ),
            ("http://[::1]:4318", "http://[::1]:4318/v1/logs"),
        ];
        for (raw, logs_url) in accepted {
            let endpoint = parse_otlp_endpoint(raw).unwrap();
            assert_eq!(endpoint.logs_url(), logs_url, "{raw}");
        }
        assert_eq!(
            parse_otlp_endpoint("http://[::1]:4318").unwrap().host(),
            "[::1]"
        );
        let secret = parse_otlp_endpoint("https://x/api/s3cr3t/otlp").unwrap();
        assert_eq!(format!("{secret:?}"), "OtlpEndpoint { host: \"x\", .. }");

        let refused = [
            ("ftp://x", "must be an http or https URL"),
            ("not a url", "must be an http or https URL"),
            ("https:///x", "must be an http or https URL"),
            ("https://user:pw@x", "must not contain user info"),
            (
                "https://x/otlp?token=1",
                "must not have a query or fragment",
            ),
            ("https://x/otlp#f", "must not have a query or fragment"),
            (
                "https://x/otlp/v1/logs",
                "must be the base URL, without /v1/logs or /v1/traces",
            ),
            (
                "https://x/v1/traces/",
                "must be the base URL, without /v1/logs or /v1/traces",
            ),
        ];
        for (raw, reason) in refused {
            assert_eq!(parse_otlp_endpoint(raw).unwrap_err(), reason, "{raw}");
        }
    }

    #[test]
    fn a_blank_otlp_endpoint_is_unset() {
        for blank in ["", "   "] {
            let config =
                load_from(None, &[("CETRA_SELF_MONITORING__OTLP_ENDPOINT", blank)]).unwrap();
            assert!(config.self_monitoring.otlp_endpoint.is_none());
        }
    }

    #[test]
    fn an_invalid_otlp_endpoint_names_the_rule_not_the_value() {
        let toml = "[self_monitoring]\notlp_endpoint = \"https://user:hunter2@x\"\n";
        let text = load_from(Some(toml), &[]).err().unwrap().to_string();
        assert!(text.contains("must not contain user info"), "{text}");
        assert!(text.contains("otlp_endpoint"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn otlp_headers_rules() {
        fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
            items
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect()
        }
        let accepted = [
            (
                "authorization=Basic%20abc",
                vec![("authorization", "Basic abc")],
            ),
            (
                "x-sentry-auth=sentry sentry_key=abc",
                vec![("x-sentry-auth", "sentry sentry_key=abc")],
            ),
            (" A = b , ,c=d ", vec![("a", "b"), ("c", "d")]),
            ("a=", vec![("a", "")]),
            ("", vec![]),
            ("k=YWJj+/9=", vec![("k", "YWJj+/9=")]),
        ];
        for (raw, expected) in accepted {
            assert_eq!(parse_otlp_headers(raw).unwrap(), pairs(&expected), "{raw}");
        }

        let refused = [
            ("novalue", "entries must be name=value"),
            ("=v", "a name is empty"),
            ("bad name=v", "a header name is not a valid HTTP token"),
            ("a=%zz", "a value is not valid percent-encoding"),
            ("a=%4", "a value is not valid percent-encoding"),
            ("a=%FF", "a value is not valid percent-encoding"),
            (
                "a=%0A",
                "a header value has a character HTTP does not allow",
            ),
            ("a=1,A=2", "a name is listed twice"),
            (
                "Content-Type=x",
                "content-type, content-encoding, content-length, host and user-agent are set by cetra",
            ),
            (
                "user-agent=x",
                "content-type, content-encoding, content-length, host and user-agent are set by cetra",
            ),
        ];
        for (raw, reason) in refused {
            assert_eq!(parse_otlp_headers(raw).unwrap_err(), reason, "{raw}");
        }

        let many: Vec<String> = (0..33).map(|index| format!("h{index}=v")).collect();
        assert_eq!(
            parse_otlp_headers(&many.join(",")).unwrap_err(),
            "at most 32 entries"
        );
        let long = format!("a={}", "x".repeat(8191));
        assert_eq!(long.len(), 8193);
        assert_eq!(parse_otlp_headers(&long).unwrap_err(), "at most 8192 bytes");
    }

    #[test]
    fn resource_rules() {
        assert_eq!(
            parse_resource("deployment.environment=prod,cetra.cell=eu1").unwrap(),
            vec![
                ("deployment.environment".to_owned(), "prod".to_owned()),
                ("cetra.cell".to_owned(), "eu1".to_owned()),
            ]
        );
        assert_eq!(
            parse_resource("k=v%2Cw").unwrap(),
            vec![("k".to_owned(), "v,w".to_owned())]
        );
        assert_eq!(
            parse_resource("Service.Name=x").unwrap(),
            vec![("Service.Name".to_owned(), "x".to_owned())]
        );

        let refused = [
            ("k v=1", "a key has whitespace or a control character"),
            ("=1", "a name is empty"),
            ("k=1,k=2", "a name is listed twice"),
            ("k=%FF", "a value is not valid percent-encoding"),
        ];
        for (raw, reason) in refused {
            assert_eq!(parse_resource(raw).unwrap_err(), reason, "{raw}");
        }
    }

    #[test]
    fn invalid_otlp_headers_never_reach_the_error() {
        let error = load_from(None, &[("CETRA_SELF_MONITORING__OTLP_HEADERS", "12345")])
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ConfigError::Invalid {
                key: "self_monitoring.otlp_headers",
                reason: "entries must be name=value",
            }
        ));
        assert!(!error.to_string().contains("12345"));

        let error = load_from(Some("[self_monitoring]\notlp_headers = 5\n"), &[])
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ConfigError::Invalid {
                key: "self_monitoring.otlp_headers",
                reason: "expected a string",
            }
        ));
    }

    #[test]
    fn invalid_resource_is_refused() {
        let error = load_from(None, &[("CETRA_SELF_MONITORING__RESOURCE", "nokey")])
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ConfigError::Invalid {
                key: "self_monitoring.resource",
                reason: "entries must be name=value",
            }
        ));
    }

    #[test]
    fn unknown_self_monitoring_key_is_refused() {
        // cetra exports no metrics over OTLP (D-N-28), so `metrics` is refused.
        assert!(load_from(Some("[self_monitoring]\nmetrics = true\n"), &[]).is_err());
    }

    #[test]
    fn trace_sample_ratio_bounds() {
        for value in [0.0, 0.25, 1.0] {
            assert_eq!(TraceSampleRatio::try_from(value).unwrap().get(), value);
        }
        for value in [-0.1, 1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(
                TraceSampleRatio::try_from(value).unwrap_err(),
                "must be a number from 0.0 to 1.0",
                "{value}"
            );
        }
    }

    #[test]
    fn trace_settings_load_from_toml_and_env() {
        let toml = "[self_monitoring]\notlp_endpoint = \"https://otlp.example.com/otlp\"\n\
                    traces = false\ntrace_sample_ratio = 0.5\n";
        let config = load_from(Some(toml), &[]).unwrap();
        assert!(!config.self_monitoring.traces);
        assert_eq!(config.self_monitoring.trace_sample_ratio.get(), 0.5);
        assert!(config.self_monitoring.traces_endpoint().is_none());
        assert!(config.self_monitoring.exports());

        let env = [
            ("CETRA_SELF_MONITORING__TRACE_SAMPLE_RATIO", "1"),
            ("CETRA_SELF_MONITORING__LOGS", "false"),
        ];
        let config = load_from(
            Some("[self_monitoring]\notlp_endpoint = \"https://otlp.example.com/otlp\"\n"),
            &env,
        )
        .unwrap();
        assert_eq!(config.self_monitoring.trace_sample_ratio.get(), 1.0);
        let endpoint = config.self_monitoring.traces_endpoint().unwrap();
        assert_eq!(
            endpoint.traces_url(),
            "https://otlp.example.com/otlp/v1/traces"
        );
        assert!(config.self_monitoring.exports());
    }

    #[test]
    fn a_trace_sample_ratio_out_of_range_is_refused() {
        let endpoint = "[self_monitoring]\notlp_endpoint = \"https://otlp.example.com/otlp\"\n";
        for toml in ["trace_sample_ratio = 1.5\n", "trace_sample_ratio = nan\n"] {
            let error = load_from(Some(&format!("{endpoint}{toml}")), &[]).unwrap_err();
            let text = error.to_string();
            assert!(text.contains("must be a number from 0.0 to 1.0"), "{text}");
            assert!(text.contains("trace_sample_ratio"), "{text}");
        }
        let error = load_from(
            Some(endpoint),
            &[("CETRA_SELF_MONITORING__TRACE_SAMPLE_RATIO", "-1")],
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("must be a number from 0.0 to 1.0"), "{text}");
        assert!(text.contains("trace_sample_ratio"), "{text}");
    }

    #[test]
    fn a_zero_ratio_exports_no_span() {
        let base = "[self_monitoring]\notlp_endpoint = \"https://otlp.example.com/otlp\"\n\
                    logs = false\ntrace_sample_ratio = 0.0\n";
        let config = load_from(Some(base), &[]).unwrap();
        assert!(config.self_monitoring.traces_endpoint().is_none());
        assert!(!config.self_monitoring.exports());

        let with_logs = base.replace("logs = false", "logs = true");
        let config = load_from(Some(&with_logs), &[]).unwrap();
        assert!(config.self_monitoring.exports());
    }

    #[test]
    fn traces_url_appends_v1_traces() {
        let cases = [
            ("http://collector:4318/", "http://collector:4318/v1/traces"),
            (
                "https://o1.ingest.de.sentry.io/api/2/integration/otlp",
                "https://o1.ingest.de.sentry.io/api/2/integration/otlp/v1/traces",
            ),
        ];
        for (raw, traces_url) in cases {
            let endpoint = parse_otlp_endpoint(raw).unwrap();
            assert_eq!(endpoint.traces_url(), traces_url, "{raw}");
        }
    }

    #[test]
    fn storage_wal_sync_reads_the_split_env_key() {
        let config = load_from(None, &[("CETRA_STORAGE__WAL__SYNC", "5ms")]).unwrap();
        assert_eq!(
            config.storage.wal.sync,
            WalSync::Interval(Duration::from_millis(5))
        );
    }

    #[test]
    fn unknown_keys_name_themselves() {
        let toml_error = load_from(Some("nope = true\n"), &[]).unwrap_err();
        assert!(toml_error.to_string().contains("nope"), "{toml_error}");

        let env_error = load_from(None, &[("CETRA_FOO", "1")]).unwrap_err();
        assert!(env_error.to_string().contains("foo"), "{env_error}");
    }

    #[test]
    fn cli_variables_are_not_config_keys() {
        let config = load_from(
            None,
            &[
                ("CETRA_URL", "http://127.0.0.1:8080"),
                ("CETRA_TOKEN", "pat-cetra-EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0"),
                ("cetra_project", "acme/app"),
            ],
        );
        assert!(config.is_ok());
    }

    #[test]
    fn storage_data_dir_is_rejected_and_top_level_is_injected() {
        let error = load_from(Some("[storage]\ndata_dir = \"/secret-path\"\n"), &[]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("storage.data_dir"), "{text}");
        assert!(!text.contains("secret-path"), "{text}");

        let env_error =
            load_from(None, &[("CETRA_STORAGE__DATA_DIR", "/also-secret")]).unwrap_err();
        assert!(env_error.to_string().contains("data_dir"), "{env_error}");
        assert!(
            !env_error.to_string().contains("also-secret"),
            "{env_error}"
        );

        let config = load_from(Some("data_dir = \"/data\"\n"), &[]).unwrap();
        assert_eq!(config.storage_config().data_dir, PathBuf::from("/data"));
    }

    #[test]
    fn comma_lists_and_invalid_cidr() {
        let config = load_from(
            Some(
                r#"
                trusted_proxies = ["127.0.0.1/32"]
                auth.signup_domains = ["Example.COM", ""]

                [auth.github]
                client_id = "gh"
                client_secret = "secret"
                "#,
            ),
            &[(
                "CETRA_AUTH_ENFORCE_SSO_DOMAINS",
                "Acme.ORG,, partner.example",
            )],
        )
        .unwrap();
        assert_eq!(config.trusted_proxies.len(), 1);
        assert_eq!(config.auth.signup_domains, vec!["example.com".to_owned()]);
        assert_eq!(
            config.auth.enforce_sso_domains,
            vec!["acme.org".to_owned(), "partner.example".to_owned()]
        );

        let env_lists = load_from(
            None,
            &[("CETRA_TRUSTED_PROXIES", "127.0.0.1/32, 10.0.0.0/8")],
        )
        .unwrap();
        assert_eq!(env_lists.trusted_proxies.len(), 2);

        let error = load_from(None, &[("CETRA_TRUSTED_PROXIES", "not-a-cidr")]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("trusted_proxies"), "{text}");
        assert!(!text.contains("not-a-cidr"), "{text}");

        let domain_error =
            load_from(None, &[("CETRA_AUTH_SIGNUP_DOMAINS", "bad domain")]).unwrap_err();
        let domain_text = domain_error.to_string();
        assert!(domain_text.contains("auth.signup_domains"), "{domain_text}");
        assert!(!domain_text.contains("bad domain"), "{domain_text}");
    }

    #[test]
    fn public_url_rejects_path_query_and_non_http() {
        for bad in [
            "ftp://localhost",
            "http://localhost/api",
            "http://localhost/?q=1",
        ] {
            let error = load_from(None, &[("CETRA_PUBLIC_URL", bad)]).unwrap_err();
            assert!(error.to_string().contains("public_url"), "{bad}: {error}");
        }

        let userinfo =
            load_from(None, &[("CETRA_PUBLIC_URL", "http://ada:s3cret@localhost")]).unwrap_err();
        assert!(!userinfo.to_string().contains("s3cret"), "{userinfo}");

        let config = load_from(None, &[("CETRA_PUBLIC_URL", "http://localhost:8080/")]).unwrap();
        assert_eq!(config.public_url.origin(), "http://localhost:8080");
        assert_eq!(config.public_url.join("/api"), "http://localhost:8080/api");
        assert_eq!(config.public_url.join("api"), "http://localhost:8080/api");

        let ipv6 = load_from(None, &[("CETRA_PUBLIC_URL", "https://[::1]:8080/")]).unwrap();
        assert_eq!(ipv6.public_url.origin(), "https://[::1]:8080");
        assert!(ipv6.public_url.is_https());
    }

    #[test]
    fn http_addr_errors_name_the_key() {
        let error = load_from(None, &[("CETRA_HTTP_ADDR", "nope")]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("http_addr"), "{text}");
        assert!(!text.contains("nope"), "{text}");
    }

    /// Review 2026-11-15 #7: a TOML syntax error quoted the offending line, so a typo next to
    /// a secret printed the secret.
    #[test]
    fn toml_syntax_errors_omit_the_line() {
        for toml in [
            "smtp_url = \"smtp://u:hunter2@x.com\" junk\n",
            "smtp_url = \"smtp://u:hunter2@x.com\n",
            "[auth.github]\nclient_secret = hunter2\n",
        ] {
            let text = load_from(Some(toml), &[]).unwrap_err().to_string();
            assert!(text.contains("TOML parse error at line"), "{text}");
            assert!(!text.contains("hunter2"), "{text}");
        }
    }

    #[test]
    fn verification_requires_smtp() {
        let error = load_from(Some("auth.require_email_verification = true\n"), &[]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("auth.require_email_verification"), "{text}");
        assert!(text.contains("smtp_url"), "{text}");

        let blank = load_from(
            Some("smtp_url = \"  \"\nauth.require_email_verification = true\n"),
            &[],
        )
        .unwrap_err();
        assert!(blank.to_string().contains("smtp_url"), "{blank}");

        let config = load_from(
            Some(
                "smtp_url = \"smtp://localhost\"\nemail_from = \"noreply@example.com\"\nauth.require_email_verification = true\n",
            ),
            &[],
        )
        .unwrap();
        assert!(config.auth.require_email_verification);
        let smtp = config.smtp_url.expect("smtp");
        assert_eq!(smtp.tls, Tls::Required);
        assert_eq!(smtp.host, "localhost");
        assert_eq!(smtp.port, 587);
        assert!(smtp.credentials.is_none());
    }

    #[test]
    fn partial_init_and_invalid_email_omit_the_values() {
        let partial =
            load_from(Some("[init]\nadmin_password = \"hunter2hunter2\"\n"), &[]).unwrap_err();
        let partial_text = partial.to_string();
        assert!(partial_text.contains("init"), "{partial_text}");
        assert!(!partial_text.contains("hunter2hunter2"), "{partial_text}");

        let email = load_from(
            Some(
                r#"
                [init]
                admin_email = "not-an-email"
                admin_password = "hunter2hunter2"
                org = "Acme"
                project = "App"
                "#,
            ),
            &[],
        )
        .unwrap_err();
        let email_text = email.to_string();
        assert!(email_text.contains("init.admin_email"), "{email_text}");
        assert!(!email_text.contains("not-an-email"), "{email_text}");
        assert!(!email_text.contains("hunter2hunter2"), "{email_text}");
    }

    #[test]
    fn numeric_looking_password_from_env_stays_a_string() {
        let config = load_from(
            None,
            &[
                ("CETRA_INIT_ADMIN_EMAIL", "ada@example.com"),
                ("CETRA_INIT_ADMIN_PASSWORD", "9876543210"),
                ("CETRA_INIT_ORG", "Acme"),
                ("CETRA_INIT_PROJECT", "App"),
            ],
        )
        .unwrap();
        assert_eq!(config.init.admin_password.unwrap().expose(), "9876543210");
    }

    #[test]
    fn numeric_secret_error_omits_the_number() {
        let error = load_from(
            Some(
                r#"
                [init]
                admin_email = "ada@example.com"
                admin_password = 987654321
                org = "Acme"
                project = "App"
                "#,
            ),
            &[],
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("admin_password"), "{text}");
        assert!(!text.contains("987654321"), "{text}");
    }

    #[test]
    fn debug_hides_password_and_secret_key() {
        let config = load_from(
            Some(
                r#"
                smtp_url = "smtp://user:s3cret-token@localhost:587"
                email_from = "cetra <noreply@example.com>"
                [init]
                admin_email = "ada@example.com"
                admin_password = "hunter2hunter2"
                org = "Acme"
                project = "App"
                secret_key = "sk-cetra-EXAMPLEKEYEXAMPLEKEYEXAMPLE"
                "#,
            ),
            &[(
                "CETRA_INIT_READ_KEY",
                "sk-cetra-READKEYREADKEYREADKEYREADKEY",
            )],
        )
        .unwrap();
        assert_eq!(
            config.init.read_key.as_ref().unwrap().expose(),
            "sk-cetra-READKEYREADKEYREADKEYREADKEY"
        );
        let text = format!("{config:?}");
        assert!(!text.contains("READKEYREADKEY"), "{text}");
        assert!(text.contains("Secret(***)"), "{text}");
        assert!(!text.contains("hunter2hunter2"), "{text}");
        assert!(
            !text.contains("sk-cetra-EXAMPLEKEYEXAMPLEKEYEXAMPLE"),
            "{text}"
        );
        assert!(!text.contains("s3cret-token"), "{text}");
        assert_eq!(
            config.init.admin_password.unwrap().expose(),
            "hunter2hunter2"
        );
        let (user, password) = config.smtp_url.unwrap().credentials.unwrap();
        assert_eq!(user, "user");
        assert_eq!(password.expose(), "s3cret-token");
    }

    #[test]
    fn eval_and_security_default() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.eval.workers, 16);
        assert!(!config.security.allow_private_network_egress);
        assert!(config.encryption_key.is_none());
    }

    #[test]
    fn eval_workers_out_of_range_is_rejected() {
        for toml in ["[eval]\nworkers = 0\n", "[eval]\nworkers = 257\n"] {
            let error = load_from(Some(toml), &[]).unwrap_err();
            assert!(
                matches!(
                    error,
                    ConfigError::Invalid {
                        key: "eval.workers",
                        ..
                    }
                ),
                "{toml}: {error}"
            );
        }
    }

    #[test]
    fn issues_workers_default_is_four() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.issues.workers, 4);
    }

    #[test]
    fn issues_workers_bounds_are_one_to_sixty_four() {
        for workers in [1, 64] {
            let toml = format!("[issues]\nworkers = {workers}\n");
            let config = load_from(Some(toml.as_str()), &[]).unwrap();
            assert_eq!(config.issues.workers, workers);
        }
        for toml in ["[issues]\nworkers = 0\n", "[issues]\nworkers = 65\n"] {
            let error = load_from(Some(toml), &[]).unwrap_err();
            assert!(
                matches!(
                    error,
                    ConfigError::Invalid {
                        key: "issues.workers",
                        ..
                    }
                ),
                "{toml}: {error}"
            );
        }
    }

    #[test]
    fn issues_workers_comes_from_the_environment() {
        let config = load_from(None, &[("CETRA_ISSUES__WORKERS", "8")]).unwrap();
        assert_eq!(config.issues.workers, 8);
    }

    #[test]
    fn encryption_key_comes_from_the_environment() {
        let config = load_from(None, &[("CETRA_ENCRYPTION_KEY", "abc")]).unwrap();
        assert_eq!(
            config.encryption_key.map(|key| key.expose().clone()),
            Some("abc".to_owned())
        );
    }

    #[test]
    fn license_key_comes_from_the_environment() {
        let config = load_from(None, &[("CETRA_LICENSE_KEY", "abc")]).unwrap();
        assert_eq!(
            config.license_key.map(|key| key.expose().clone()),
            Some("abc".to_owned())
        );
    }

    #[test]
    fn empty_license_key_is_unset() {
        for value in ["", "   "] {
            let config = load_from(None, &[("CETRA_LICENSE_KEY", value)]).unwrap();
            assert!(config.license_key.is_none(), "{value:?}");
        }
    }

    #[test]
    fn license_key_type_error_does_not_quote_the_value() {
        let error = load_from(Some("license_key = 424242\n"), &[]).unwrap_err();
        assert!(
            matches!(
                error,
                ConfigError::Invalid {
                    key: "license_key",
                    ..
                }
            ),
            "{error}"
        );
        let text = error.to_string();
        assert!(!text.contains("424242"), "{text}");
    }

    #[test]
    fn numeric_encryption_key_stays_a_string() {
        let config = load_from(None, &[("CETRA_ENCRYPTION_KEY", "12345678")]).unwrap();
        assert_eq!(
            config.encryption_key.map(|key| key.expose().clone()),
            Some("12345678".to_owned())
        );
    }

    #[test]
    fn encryption_key_type_error_does_not_quote_the_value() {
        let error = load_from(Some("encryption_key = 424242\n"), &[]).unwrap_err();
        assert!(
            matches!(
                error,
                ConfigError::Invalid {
                    key: "encryption_key",
                    ..
                }
            ),
            "{error}"
        );
        let text = error.to_string();
        assert!(!text.contains("424242"), "{text}");
    }

    #[test]
    fn upgrade_url_accepts_the_org_placeholder() {
        let config = load_from(
            Some("[entitlements]\nupgrade_url = \"https://app.example.com/orgs/{org}/billing\"\n"),
            &[],
        )
        .unwrap();
        assert_eq!(
            config.entitlements.upgrade_url.as_deref(),
            Some("https://app.example.com/orgs/{org}/billing")
        );
    }

    #[test]
    fn upgrade_url_rejects_other_placeholders_and_schemes() {
        for value in [
            "https://x.example/{plan}",
            "ftp://x.example",
            "not a url",
            "https:///nohost",
        ] {
            let toml = format!("[entitlements]\nupgrade_url = \"{value}\"\n");
            let error = load_from(Some(&toml), &[]).unwrap_err();
            assert!(
                matches!(
                    error,
                    ConfigError::Invalid {
                        key: "entitlements.upgrade_url",
                        ..
                    }
                ),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn upgrade_url_from_env() {
        let config = load_from(
            None,
            &[("CETRA_ENTITLEMENTS__UPGRADE_URL", "https://example.com/up")],
        )
        .unwrap();
        assert_eq!(
            config.entitlements.upgrade_url.as_deref(),
            Some("https://example.com/up")
        );
    }

    #[test]
    fn control_plane_provider_defaults_to_oidc_cloud() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.entitlements.control_plane_provider, "oidc:cloud");
    }

    #[test]
    fn control_plane_provider_takes_a_custom_value() {
        let config = load_from(
            Some("[entitlements]\ncontrol_plane_provider = \"oidc:acme\"\n"),
            &[],
        )
        .unwrap();
        assert_eq!(config.entitlements.control_plane_provider, "oidc:acme");
    }

    #[test]
    fn control_plane_provider_refuses_an_empty_value() {
        let error = load_from(
            Some("[entitlements]\ncontrol_plane_provider = \"  \"\n"),
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                ConfigError::Invalid {
                    key: "entitlements.control_plane_provider",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn control_plane_is_off_by_default() {
        let config = load_from(None, &[]).unwrap();
        assert!(!config.entitlements.control_plane);
    }

    #[test]
    fn control_plane_parses_true() {
        let config = load_from(Some("[entitlements]\ncontrol_plane = true\n"), &[]).unwrap();
        assert!(config.entitlements.control_plane);
    }

    #[test]
    fn control_plane_from_env() {
        let config = load_from(None, &[("CETRA_ENTITLEMENTS__CONTROL_PLANE", "true")]).unwrap();
        assert!(config.entitlements.control_plane);
    }

    #[test]
    fn orgs_default_allows_user_created_orgs_without_a_link() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.orgs, OrgsConfig::default());
        assert!(config.orgs.user_create);
        assert_eq!(config.orgs.create_url, None);
    }

    #[test]
    fn orgs_user_create_off_with_a_create_url() {
        let config = load_from(
            Some("[orgs]\nuser_create = false\ncreate_url = \"https://cloud.example.com/new\"\n"),
            &[],
        )
        .unwrap();
        assert!(!config.orgs.user_create);
        assert_eq!(
            config.orgs.create_url.as_deref(),
            Some("https://cloud.example.com/new")
        );
    }

    #[test]
    fn orgs_from_env() {
        let config = load_from(None, &[("CETRA_ORGS__USER_CREATE", "false")]).unwrap();
        assert!(!config.orgs.user_create);
    }

    #[test]
    fn orgs_create_url_rejects_placeholders_and_non_http_urls() {
        for value in [
            "https://x.example/{org}",
            "ftp://x.example",
            "not a url",
            "https:///nohost",
        ] {
            let toml = format!("[orgs]\ncreate_url = \"{value}\"\n");
            let error = load_from(Some(&toml), &[]).unwrap_err();
            match error {
                ConfigError::Invalid { key, reason } => {
                    assert_eq!(key, "orgs.create_url", "{value}");
                    assert_eq!(reason, "must be an http or https URL", "{value}");
                }
                other => panic!("{value}: {other}"),
            }
        }
    }

    #[test]
    fn orgs_refuses_unknown_keys() {
        let error = load_from(Some("[orgs]\nallow_all = true\n"), &[]).unwrap_err();
        assert!(error.to_string().contains("allow_all"), "{error}");
    }

    #[test]
    fn alerts_default_follows_the_signup_policy() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.alerts, AlertsConfig::default());
        assert_eq!(config.email_recipients(), EmailRecipients::Any);

        let config = load_from(None, &[("CETRA_AUTH_SIGNUP", "open")]).unwrap();
        assert_eq!(config.email_recipients(), EmailRecipients::Members);

        let config = load_from(None, &[("CETRA_AUTH_SIGNUP", "disabled")]).unwrap();
        assert_eq!(config.email_recipients(), EmailRecipients::Any);
    }

    #[test]
    fn alerts_email_recipients_members_under_invite_only() {
        let config = load_from(Some("[alerts]\nemail_recipients = \"members\"\n"), &[]).unwrap();
        assert_eq!(
            config.alerts.email_recipients,
            Some(EmailRecipients::Members)
        );
        assert_eq!(config.email_recipients(), EmailRecipients::Members);
    }

    #[test]
    fn alerts_email_recipients_any_under_open_signup() {
        let config = load_from(
            Some("[alerts]\nemail_recipients = \"any\"\n"),
            &[("CETRA_AUTH_SIGNUP", "open")],
        )
        .unwrap();
        assert_eq!(config.email_recipients(), EmailRecipients::Any);
    }

    #[test]
    fn alerts_email_recipients_from_env() {
        let config = load_from(None, &[("CETRA_ALERTS__EMAIL_RECIPIENTS", "members")]).unwrap();
        assert_eq!(config.email_recipients(), EmailRecipients::Members);
    }

    #[test]
    fn alerts_email_recipients_rejects_an_unknown_value() {
        let error =
            load_from(Some("[alerts]\nemail_recipients = \"everyone\"\n"), &[]).unwrap_err();
        assert!(matches!(error, ConfigError::Figment(_)), "{error}");
    }

    #[test]
    fn alerts_refuses_unknown_keys() {
        let error = load_from(Some("[alerts]\nzz = 1\n"), &[]).unwrap_err();
        assert!(matches!(error, ConfigError::Figment(_)), "{error}");
    }

    const DEMO_ORG: &str = "0195f0a0-0000-7000-8000-000000000001";

    #[test]
    fn demo_defaults_are_off_with_the_spec_limits() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(config.demo, DemoConfig::default());
        assert_eq!(config.demo.org, None);
        assert_eq!(config.demo.requests_per_minute, 120);
        assert_eq!(config.demo.user_in_flight, 4);
        assert_eq!(config.demo.org_in_flight, 16);
        assert_eq!(config.demo.sql_timeout, Duration::from_secs(10));
        assert_eq!(config.demo.sql_max_rows, 1_000);
    }

    #[test]
    fn demo_table_parses_from_toml() {
        let toml = format!(
            "[demo]\norg = \"{DEMO_ORG}\"\nrequests_per_minute = 60\nuser_in_flight = 2\n\
             org_in_flight = 8\nsql_timeout = \"5s\"\nsql_max_rows = 500\n"
        );
        let config = load_from(Some(&toml), &[]).unwrap();
        assert_eq!(config.demo.org, Some(DEMO_ORG.parse::<OrgId>().unwrap()));
        assert_eq!(config.demo.requests_per_minute, 60);
        assert_eq!(config.demo.user_in_flight, 2);
        assert_eq!(config.demo.org_in_flight, 8);
        assert_eq!(config.demo.sql_timeout, Duration::from_secs(5));
        assert_eq!(config.demo.sql_max_rows, 500);
    }

    #[test]
    fn demo_org_from_env() {
        let config = load_from(None, &[("CETRA_DEMO__ORG", DEMO_ORG)]).unwrap();
        assert_eq!(config.demo.org, Some(DEMO_ORG.parse::<OrgId>().unwrap()));
    }

    #[test]
    fn demo_limits_refuse_out_of_range_values() {
        for (line, key) in [
            ("requests_per_minute = 0", "demo.requests_per_minute"),
            ("user_in_flight = 0", "demo.user_in_flight"),
            ("org_in_flight = 0", "demo.org_in_flight"),
            ("user_in_flight = 17", "demo.user_in_flight"),
            ("sql_timeout = \"0s\"", "demo.sql_timeout"),
            ("sql_timeout = \"61s\"", "demo.sql_timeout"),
            ("sql_max_rows = 0", "demo.sql_max_rows"),
            ("sql_max_rows = 10001", "demo.sql_max_rows"),
        ] {
            let toml = format!("[demo]\n{line}\n");
            match load_from(Some(&toml), &[]).unwrap_err() {
                ConfigError::Invalid { key: found, .. } => assert_eq!(found, key, "{line}"),
                other => panic!("{line}: {other}"),
            }
        }
    }

    #[test]
    fn demo_refuses_unknown_keys() {
        let error = load_from(Some("[demo]\nallow_all = true\n"), &[]).unwrap_err();
        assert!(error.to_string().contains("allow_all"), "{error}");
    }

    #[test]
    fn defaults_are_self_hosted_and_unlimited() {
        let config = load_from(None, &[]).unwrap();
        let defaults = &config.entitlements.defaults;
        assert_eq!(defaults.plan_name, "Self-hosted");
        assert_eq!(defaults.features, None);
        assert_eq!(defaults.units_included, None);
        assert_eq!(defaults.units_cap, None);
        assert_eq!(defaults.ingest_spans_per_sec, None);
        assert_eq!(defaults.ingest_burst, None);
        assert_eq!(defaults.data_access_days, None);
        assert_eq!(defaults.max_seats, None);
        assert_eq!(defaults.max_projects, None);
        assert_eq!(defaults.max_alert_rules, None);
        assert_eq!(defaults.max_active_online_rules, None);
        assert_eq!(defaults.max_active_issue_sources, None);
        assert_eq!(defaults.max_annotation_queues, None);
    }

    #[test]
    fn max_annotation_queues_from_env() {
        let config = load_from(
            None,
            &[("CETRA_ENTITLEMENTS__DEFAULTS__MAX_ANNOTATION_QUEUES", "2")],
        )
        .unwrap();
        assert_eq!(config.entitlements.defaults.max_annotation_queues, Some(2));
    }

    #[test]
    fn cloud_defaults_load() {
        let config = load_from(
            Some(
                "[entitlements.defaults]\n\
                 plan_name = \" Free \"\n\
                 features = []\n\
                 units_included = 100000\n\
                 units_cap = 100000\n\
                 max_seats = 1\n\
                 max_projects = 1\n",
            ),
            &[],
        )
        .unwrap();
        let defaults = &config.entitlements.defaults;
        assert_eq!(defaults.plan_name, "Free");
        assert_eq!(defaults.features, Some(Vec::new()));
        assert_eq!(defaults.units_included, Some(100_000));
        assert_eq!(defaults.units_cap, Some(100_000));
        assert_eq!(defaults.max_seats, Some(1));
        assert_eq!(defaults.max_projects, Some(1));
        assert_eq!(defaults.max_alert_rules, None);
    }

    #[test]
    fn defaults_refuse_ee_and_unknown_features() {
        for features in ["[\"project_roles\"]", "[\"org_sso\"]", "[\"nope\"]"] {
            let toml = format!("[entitlements.defaults]\nfeatures = {features}\n");
            let error = load_from(Some(&toml), &[]).unwrap_err();
            assert!(
                matches!(
                    error,
                    ConfigError::Invalid {
                        key: "entitlements.defaults.features",
                        reason: "must list core features only: retention_management",
                    }
                ),
                "{features}: {error}"
            );
        }
    }

    #[test]
    fn defaults_refuse_an_empty_plan_name() {
        let error =
            load_from(Some("[entitlements.defaults]\nplan_name = \"  \"\n"), &[]).unwrap_err();
        assert!(
            matches!(
                error,
                ConfigError::Invalid {
                    key: "entitlements.defaults.plan_name",
                    ..
                }
            ),
            "{error}"
        );
    }

    #[test]
    fn defaults_refuse_unknown_keys() {
        let error = load_from(Some("[entitlements.defaults]\nmax_users = 1\n"), &[]).unwrap_err();
        assert!(matches!(error, ConfigError::Figment(_)), "{error}");
    }

    #[test]
    fn security_section_parses() {
        let config = load_from(
            Some("[security]\nallow_private_network_egress = true\n"),
            &[],
        )
        .unwrap();
        assert!(config.security.allow_private_network_egress);
    }

    #[test]
    fn server_config_debug_hides_the_key() {
        let mut config = load_from(None, &[]).unwrap();
        config.encryption_key = Some(Secret::new("topsecretvalue".to_owned()));
        config.license_key = Some(Secret::new("licensevalue".to_owned()));
        let text = format!("{config:?}");
        assert!(!text.contains("topsecretvalue"), "{text}");
        assert!(!text.contains("licensevalue"), "{text}");
    }

    #[test]
    fn smtp_url_parses_the_three_forms() {
        let starttls = load_smtp("smtp://user:pass@localhost");
        assert_eq!(starttls.tls, Tls::Required);
        assert_eq!(starttls.host, "localhost");
        assert_eq!(starttls.port, 587);
        let (user, password) = starttls.credentials.unwrap();
        assert_eq!(user, "user");
        assert_eq!(password.expose(), "pass");

        let explicit = load_smtp("smtp://user:pass@mail.example:2525");
        assert_eq!(explicit.tls, Tls::Required);
        assert_eq!(explicit.port, 2525);

        let wrapper = load_smtp("smtps://user:pass@localhost");
        assert_eq!(wrapper.tls, Tls::Wrapper);
        assert_eq!(wrapper.port, 465);
        let submissions = load_smtp("smtps://user:pass@localhost:465");
        assert_eq!(submissions.port, 465);

        let plain = load_smtp("smtp://localhost?tls=none");
        assert_eq!(plain.tls, Tls::None);
        assert_eq!(plain.host, "localhost");
        assert_eq!(plain.port, 25);
        assert!(plain.credentials.is_none());
        let relay = load_smtp("smtp://localhost:25?tls=none");
        assert_eq!(relay.tls, Tls::None);
        assert_eq!(relay.port, 25);
    }

    #[test]
    fn smtp_url_percent_decodes_credentials() {
        let smtp = load_smtp("smtp://us%65r:s%3Acret-token@localhost");
        let (user, password) = smtp.credentials.unwrap();
        assert_eq!(user, "user");
        assert_eq!(password.expose(), "s:cret-token");
    }

    #[test]
    fn smtp_url_rejects_other_schemes_without_the_password() {
        let error = load_from(
            Some("smtp_url = \"http://user:s3cret-token@localhost\"\n"),
            &[],
        )
        .unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(display.contains("smtp_url"), "{display}");
        assert!(display.contains("invalid URL"), "{display}");
        assert!(!display.contains("s3cret-token"), "{display}");
        assert!(!debug.contains("s3cret-token"), "{debug}");

        for bad in [
            "smtp://localhost?tls=require",
            "smtp://localhost?tls=none&x=1",
            "smtps://localhost?tls=none",
            "ftp://localhost",
        ] {
            let error = load_smtp_error(bad);
            assert!(error.to_string().contains("smtp_url"), "{bad}: {error}");
            assert!(error.to_string().contains("invalid URL"), "{bad}: {error}");
        }
    }

    #[test]
    fn email_from_is_required_with_smtp_and_must_be_a_mailbox() {
        let missing = load_from(Some("smtp_url = \"smtp://localhost\"\n"), &[]).unwrap_err();
        let missing_text = missing.to_string();
        assert!(missing_text.contains("email_from"), "{missing_text}");
        assert!(
            missing_text.contains("required when smtp_url is set"),
            "{missing_text}"
        );
        assert!(!missing_text.contains("localhost"), "{missing_text}");

        let invalid = load_from(
            Some("smtp_url = \"smtp://localhost\"\nemail_from = \"not-a-mailbox\"\n"),
            &[],
        )
        .unwrap_err();
        let invalid_text = invalid.to_string();
        assert!(invalid_text.contains("email_from"), "{invalid_text}");
        assert!(invalid_text.contains("invalid mailbox"), "{invalid_text}");
        assert!(!invalid_text.contains("not-a-mailbox"), "{invalid_text}");
        assert!(!format!("{invalid:?}").contains("not-a-mailbox"));

        let named = load_from(
            Some("smtp_url = \"smtp://localhost\"\nemail_from = \"cetra <noreply@acme.com>\"\n"),
            &[],
        )
        .unwrap();
        assert_eq!(
            named.email_from.as_deref(),
            Some("cetra <noreply@acme.com>")
        );

        let bare = load_from(
            Some("smtp_url = \"smtp://localhost\"\nemail_from = \"  noreply@acme.com  \"\n"),
            &[],
        )
        .unwrap();
        assert_eq!(bare.email_from.as_deref(), Some("noreply@acme.com"));
    }

    #[test]
    fn mailer_follows_smtp_url() {
        let absent = load_from(None, &[]).unwrap();
        assert!(
            crate::email::Mailer::from_config(&absent, Default::default())
                .unwrap()
                .is_none()
        );

        let present = load_from(
            Some("smtp_url = \"smtp://localhost:2525\"\nemail_from = \"noreply@example.com\"\n"),
            &[],
        )
        .unwrap();
        assert!(
            crate::email::Mailer::from_config(&present, Default::default())
                .unwrap()
                .is_some()
        );
    }

    fn load_smtp(url: &str) -> SmtpUrl {
        let toml = format!("smtp_url = {url:?}\nemail_from = \"noreply@example.com\"\n");
        load_from(Some(&toml), &[])
            .unwrap_or_else(|error| panic!("parse {url}: {error}"))
            .smtp_url
            .unwrap()
    }

    fn load_smtp_error(url: &str) -> ConfigError {
        let toml = format!("smtp_url = {url:?}\nemail_from = \"noreply@example.com\"\n");
        load_from(Some(&toml), &[]).unwrap_err()
    }

    #[test]
    fn missing_config_file_names_the_path() {
        let path = Path::new("/no/such/cetra-config.toml");
        let error = ServerConfig::load(Some(path)).unwrap_err();
        assert!(
            error.to_string().contains("/no/such/cetra-config.toml"),
            "{error}"
        );
    }

    #[test]
    fn providers_parse_and_debug_hides_client_secrets() {
        let config = load_from(Some(PROVIDERS_TOML), &[]).expect("providers");
        let github = config.auth.github.as_ref().expect("github");
        assert_eq!(github.client_id, "gh-client");
        assert_eq!(github.client_secret.expose(), "gh-secret-value");
        assert!(github.link_existing_by_email);
        let google = config.auth.google.as_ref().expect("google");
        assert_eq!(google.allowed_domains, vec!["acme.test".to_owned()]);
        assert!(google.link_existing_by_email);
        assert_eq!(config.auth.oidc.len(), 5);
        let okta = &config.auth.oidc[0];
        assert_eq!(okta.name, "okta");
        assert_eq!(okta.display_name, "Okta");
        assert_eq!(
            okta.scopes,
            vec![
                "openid".to_owned(),
                "email".to_owned(),
                "profile".to_owned()
            ]
        );
        assert!(!okta.link_existing_by_email);
        assert_eq!(
            okta.auto_provision.as_ref().map(|item| item.role.as_str()),
            Some("member")
        );
        assert_eq!(config.auth.oidc[1].issuer, "http://127.0.0.1:9");
        assert_eq!(config.auth.oidc[2].issuer, "http://localhost:9");
        assert_eq!(config.auth.oidc[3].issuer, "http://[::1]:9");
        assert!(config.auth.oidc[4].link_existing_by_email);
        assert!(config.auth.disable_password);
        assert_eq!(
            config.auth.enforce_sso_domains,
            vec!["acme.test".to_owned()]
        );
        let rendered = format!("{config:?}");
        for secret in [
            "gh-secret-value",
            "google-secret-value",
            "okta-secret-value",
            "local-secret-value",
            "loop-secret-value",
            "v6-secret-value",
            "entra-secret-value",
        ] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        assert!(rendered.contains("Secret(***)"), "{rendered}");
    }

    #[test]
    fn provider_rules_name_the_key_and_hide_the_value() {
        assert_rejected(
            &oidc("name = \"Okta\""),
            "auth.oidc.name",
            "name must be 1-32 characters of [a-z0-9-]",
        );
        assert_rejected(
            &format!("{}\n{}", oidc("name = \"okta\""), oidc("name = \"okta\"")),
            "auth.oidc.okta",
            "duplicate name",
        );
        assert_rejected(
            &oidc("issuer = \"http://idp.example.com\""),
            "auth.oidc.okta.issuer",
            "issuer must be https, or http on a loopback host",
        );
        assert_rejected(
            &oidc("issuer = \"https://user:s3cret@idp.example.com\""),
            "auth.oidc.okta.issuer",
            "invalid issuer",
        );
        assert_rejected(
            &oidc("issuer = \"https://login.microsoftonline.com/common/v2.0\""),
            "auth.oidc.okta.issuer",
            "multi-tenant Microsoft issuers are not allowed",
        );
        assert_rejected(
            &oidc("issuer = \"https://login.microsoftonline.com/organizations\""),
            "auth.oidc.okta.issuer",
            "multi-tenant Microsoft issuers are not allowed",
        );
        assert_rejected(
            &oidc("issuer = \"https://LOGIN.MICROSOFTONLINE.COM/consumers/v2.0\""),
            "auth.oidc.okta.issuer",
            "multi-tenant Microsoft issuers are not allowed",
        );
        assert_rejected(
            &oidc("auto_provision = { org = \"acme\", role = \"owner\" }"),
            "auth.oidc.okta.auto_provision.role",
            "role must not be owner",
        );
        assert_rejected(
            &oidc("auto_provision = { org = \"x\", role = \"member\" }"),
            "auth.oidc.okta.auto_provision.org",
            "invalid org slug",
        );
        assert_rejected(
            &oidc("scopes = [\"email\"]"),
            "auth.oidc.okta.scopes",
            "scopes must include openid",
        );
        assert_rejected(
            "[auth]\ndisable_password = true\n",
            "auth.disable_password",
            "a provider is required when passwords are disabled",
        );
        assert_rejected(
            "[auth]\nenforce_sso_domains = [\"acme.test\"]\n",
            "auth.enforce_sso_domains",
            "a provider is required when SSO domains are set",
        );
    }

    #[test]
    fn github_client_secret_stays_a_string() {
        let config = load_from(
            None,
            &[
                ("CETRA_AUTH__GITHUB__CLIENT_ID", "gh-client"),
                ("CETRA_AUTH__GITHUB__CLIENT_SECRET", "987654321"),
            ],
        )
        .expect("env github");
        let github = config.auth.github.expect("github");
        assert_eq!(github.client_secret.expose(), "987654321");

        let error = load_from(
            Some("[auth.github]\nclient_id = \"gh\"\nclient_secret = 987654321\n"),
            &[],
        )
        .expect_err("number");
        let text = error.to_string();
        assert_eq!(text, "auth.github.client_secret: expected a string");
        assert!(!text.contains("987654321"), "{text}");
    }

    fn assert_rejected(toml: &str, key: &str, reason: &str) {
        let error = load_from(Some(toml), &[]).expect_err("rejected");
        assert_eq!(error.to_string(), format!("{key}: {reason}"));
    }

    fn oidc(extra: &str) -> String {
        let mut lines = vec![
            "name = \"okta\"".to_owned(),
            "display_name = \"Okta\"".to_owned(),
            "issuer = \"https://acme.okta.com\"".to_owned(),
            "client_id = \"client\"".to_owned(),
            "client_secret = \"secret\"".to_owned(),
        ];
        for line in extra.lines() {
            let Some(key) = line.split('=').next() else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            lines.retain(|existing| !existing.starts_with(&format!("{key} ")));
            lines.push(line.trim().to_owned());
        }
        format!("[[auth.oidc]]\n{}\n", lines.join("\n"))
    }

    const OIDC_WITHOUT_SECRET: &str = r#"
[[auth.oidc]]
name = "my-idp"
display_name = "My IdP"
issuer = "https://idp.example"
client_id = "client"
"#;

    #[test]
    fn oidc_client_secret_comes_from_the_env() {
        let config = load_from(
            Some(OIDC_WITHOUT_SECRET),
            &[("CETRA_AUTH__OIDC__MY_IDP__CLIENT_SECRET", "987654321")],
        )
        .expect("env secret");
        // An all-digit secret stays a string: it never goes through figment's env parser.
        assert_eq!(config.auth.oidc[0].client_secret.expose(), "987654321");
        assert!(!format!("{:?}", config.auth.oidc[0]).contains("987654321"));
    }

    #[test]
    fn oidc_env_name_ignores_case() {
        let config = load_from(
            Some(OIDC_WITHOUT_SECRET),
            &[("cetra_auth__oidc__my_idp__client_secret", "from-env")],
        )
        .expect("lower-case secret");
        assert_eq!(config.auth.oidc[0].client_secret.expose(), "from-env");
    }

    #[test]
    fn oidc_env_secret_wins_over_the_file() {
        let config = load_from(
            Some(&oidc("")),
            &[("CETRA_AUTH__OIDC__OKTA__CLIENT_SECRET", "from-env")],
        )
        .expect("env wins");
        assert_eq!(config.auth.oidc[0].client_secret.expose(), "from-env");
    }

    #[test]
    fn oidc_secret_is_still_required() {
        assert_rejected(
            OIDC_WITHOUT_SECRET,
            "auth.oidc.my-idp.client_secret",
            "client secret is required",
        );
        let error = load_from(
            Some(OIDC_WITHOUT_SECRET),
            &[("CETRA_AUTH__OIDC__MY_IDP__CLIENT_SECRET", "")],
        )
        .expect_err("empty env secret");
        assert_eq!(
            error.to_string(),
            "auth.oidc.my-idp.client_secret: client secret is required"
        );
    }

    #[test]
    fn stray_oidc_env_is_refused() {
        for var in [
            "CETRA_AUTH__OIDC__OTHER__CLIENT_SECRET",
            "CETRA_AUTH__OIDC__0__CLIENT_SECRET",
            "CETRA_AUTH__OIDC__OKTA__CLIENT_ID",
            "CETRA_AUTH__OIDC__0__NAME",
        ] {
            let error =
                load_from(Some(&oidc("")), &[(var, "leaked-value")]).expect_err("stray variable");
            let text = error.to_string();
            assert_eq!(
                text,
                format!(
                    "{var}: only CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET is read, for a provider of the config file"
                )
            );
            assert!(!text.contains("leaked-value"), "{text}");
        }
    }

    #[test]
    fn oidc_env_without_any_provider_is_refused() {
        let error = load_from(None, &[("CETRA_AUTH__OIDC__CLOUD__CLIENT_SECRET", "x")])
            .expect_err("no provider");
        assert_eq!(
            error.to_string(),
            "CETRA_AUTH__OIDC__CLOUD__CLIENT_SECRET: only CETRA_AUTH__OIDC__<NAME>__CLIENT_SECRET is read, for a provider of the config file"
        );
    }

    #[test]
    fn oidc_env_name_maps_dashes() {
        assert_eq!(
            oidc_env_name("cloud"),
            "CETRA_AUTH__OIDC__CLOUD__CLIENT_SECRET"
        );
        assert_eq!(
            oidc_env_name("my-idp-2"),
            "CETRA_AUTH__OIDC__MY_IDP_2__CLIENT_SECRET"
        );
    }

    const PROVIDERS_TOML: &str = r#"
[auth]
disable_password = true
enforce_sso_domains = ["Acme.test"]

[auth.github]
client_id = "gh-client"
client_secret = "gh-secret-value"

[auth.google]
client_id = "google-client"
client_secret = "google-secret-value"
allowed_domains = ["Acme.TEST"]

[[auth.oidc]]
name = "okta"
display_name = " Okta "
issuer = "https://acme.okta.com"
client_id = "okta-client"
client_secret = "okta-secret-value"
auto_provision = { org = "acme", role = "member" }

[[auth.oidc]]
name = "local"
display_name = "Local"
issuer = "http://127.0.0.1:9"
client_id = "local-client"
client_secret = "local-secret-value"

[[auth.oidc]]
name = "loop"
display_name = "Loop"
issuer = "http://localhost:9"
client_id = "loop-client"
client_secret = "loop-secret-value"

[[auth.oidc]]
name = "v6"
display_name = "V6"
issuer = "http://[::1]:9"
client_id = "v6-client"
client_secret = "v6-secret-value"

[[auth.oidc]]
name = "entra"
display_name = "Entra"
issuer = "https://login.microsoftonline.com/11111111-1111-1111-1111-111111111111/v2.0"
client_id = "entra-client"
client_secret = "entra-secret-value"
scopes = ["openid", "email"]
link_existing_by_email = true
"#;

    #[test]
    fn ingest_media_and_otlp_defaults_match_spec() {
        let config = load_from(None, &[]).unwrap();
        assert_eq!(
            config.ingest,
            IngestConfig {
                max_request_bytes: ByteSize::mib(10),
                max_spans_per_request: 50_000,
                max_attribute_bytes: ByteSize::mib(8),
                max_inflight_bytes: ByteSize::mib(256),
                max_grpc_message_bytes: ByteSize::mib(32),
            }
        );
        assert_eq!(
            config.media,
            MediaConfig {
                min_bytes: ByteSize::kib(4),
                max_bytes: ByteSize::mib(32),
            }
        );
        assert_eq!(config.otlp_http_addr, Some(socket("0.0.0.0:4318")));
        assert_eq!(config.grpc_addr, Some(socket("0.0.0.0:4317")));

        let limits = config.ingest.limits(&config.media);
        assert_eq!(limits.max_attribute_bytes, 8 * 1024 * 1024);
        assert_eq!(limits.media_min_bytes, 4 * 1024);
        assert_eq!(limits.media_max_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn ingest_toml_and_split_env_both_apply() {
        let config = load_from(
            Some("[ingest]\nmax_request_bytes = \"2MiB\"\n"),
            &[("CETRA_INGEST__MAX_SPANS_PER_REQUEST", "10")],
        )
        .unwrap();
        assert_eq!(config.ingest.max_request_bytes, ByteSize::mib(2));
        assert_eq!(config.ingest.max_spans_per_request, 10);
        assert_eq!(config.ingest.max_attribute_bytes, ByteSize::mib(8));
    }

    #[test]
    fn otlp_http_addr_empty_disables_the_listener() {
        let toml = load_from(Some("otlp_http_addr = \"\"\n"), &[]).unwrap();
        assert_eq!(toml.otlp_http_addr, None);

        let env = load_from(None, &[("CETRA_OTLP_HTTP_ADDR", "")]).unwrap();
        assert_eq!(env.otlp_http_addr, None);

        let blank = load_from(None, &[("CETRA_OTLP_HTTP_ADDR", "  ")]).unwrap();
        assert_eq!(blank.otlp_http_addr, None);

        let set = load_from(None, &[("CETRA_OTLP_HTTP_ADDR", "127.0.0.1:9999")]).unwrap();
        assert_eq!(set.otlp_http_addr, Some(socket("127.0.0.1:9999")));

        let error = load_from(None, &[("CETRA_OTLP_HTTP_ADDR", "nope")]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("invalid socket address"), "{text}");
        assert!(text.contains("otlp_http_addr"), "{text}");
        assert!(!text.contains("nope"), "{text}");
    }

    #[test]
    fn grpc_addr_empty_disables_the_listener() {
        let toml = load_from(Some("grpc_addr = \"\"\n"), &[]).unwrap();
        assert_eq!(toml.grpc_addr, None);

        let env = load_from(None, &[("CETRA_GRPC_ADDR", "")]).unwrap();
        assert_eq!(env.grpc_addr, None);

        let blank = load_from(None, &[("CETRA_GRPC_ADDR", "  ")]).unwrap();
        assert_eq!(blank.grpc_addr, None);

        let set = load_from(None, &[("CETRA_GRPC_ADDR", "127.0.0.1:9998")]).unwrap();
        assert_eq!(set.grpc_addr, Some(socket("127.0.0.1:9998")));

        let error = load_from(None, &[("CETRA_GRPC_ADDR", "nope")]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("invalid socket address"), "{text}");
        assert!(text.contains("grpc_addr"), "{text}");
        assert!(!text.contains("nope"), "{text}");
    }

    #[test]
    fn restore_mode_defaults_off_and_reads_toml_and_env() {
        let default = load_from(None, &[]).unwrap();
        assert!(!default.restore_mode);

        let toml = load_from(Some("restore_mode = true\n"), &[]).unwrap();
        assert!(toml.restore_mode);

        let env = load_from(None, &[("CETRA_RESTORE_MODE", "true")]).unwrap();
        assert!(env.restore_mode);
    }

    #[test]
    fn restore_mode_turns_off_object_deletion_in_storage_config() {
        let normal = load_from(None, &[]).unwrap();
        assert!(normal.storage_config().delete_objects);

        let restore = load_from(Some("restore_mode = true\n"), &[]).unwrap();
        assert!(!restore.storage_config().delete_objects);
    }

    #[test]
    fn ingest_and_media_reject_unknown_keys() {
        let ingest = load_from(Some("[ingest]\nmax_widgets = \"1MiB\"\n"), &[]).unwrap_err();
        assert!(ingest.to_string().contains("max_widgets"), "{ingest}");

        let media = load_from(Some("[media]\nnope = 1\n"), &[]).unwrap_err();
        assert!(media.to_string().contains("nope"), "{media}");
    }

    #[test]
    fn ingest_and_media_bounds_are_checked() {
        // (TOML, the key and reason the error must be, the rejected value that must not appear).
        let cases = [
            (
                "[ingest]\nmax_request_bytes = \"512B\"\n",
                "ingest.max_request_bytes: must be between 1KiB and 1GiB",
                "512B",
            ),
            (
                "[ingest]\nmax_request_bytes = \"2GiB\"\n",
                "ingest.max_request_bytes: must be between 1KiB and 1GiB",
                "2GiB",
            ),
            (
                "[ingest]\nmax_spans_per_request = 0\n",
                "ingest.max_spans_per_request: must be at least 1",
                "0",
            ),
            (
                "[ingest]\nmax_attribute_bytes = \"100MiB\"\n",
                "ingest.max_attribute_bytes: must be between 1KiB and 64MiB",
                "100MiB",
            ),
            (
                "[ingest]\nmax_inflight_bytes = \"1KiB\"\n",
                "ingest.max_inflight_bytes: must be at least max_request_bytes and below 4GiB",
                "1KiB",
            ),
            (
                "[ingest]\nmax_inflight_bytes = \"8GiB\"\n",
                "ingest.max_inflight_bytes: must be at least max_request_bytes and below 4GiB",
                "8GiB",
            ),
            (
                "[ingest]\nmax_grpc_message_bytes = \"512B\"\n",
                "ingest.max_grpc_message_bytes: must be between 1KiB and 1GiB",
                "512B",
            ),
            (
                "[ingest]\nmax_grpc_message_bytes = \"2GiB\"\n",
                "ingest.max_grpc_message_bytes: must be between 1KiB and 1GiB",
                "2GiB",
            ),
            (
                "[ingest]\nmax_grpc_message_bytes = \"512MiB\"\n",
                "ingest.max_grpc_message_bytes: must be at most max_inflight_bytes",
                "512MiB",
            ),
            (
                "[media]\nmin_bytes = \"0B\"\n",
                "media.min_bytes: must be at least 1 byte and at most media.max_bytes",
                "0B",
            ),
            (
                "[media]\nmin_bytes = \"64MiB\"\n",
                "media.min_bytes: must be at least 1 byte and at most media.max_bytes",
                "64MiB",
            ),
            (
                "[media]\nmax_bytes = \"2GiB\"\n",
                "media.max_bytes: must be at most 1GiB",
                "2GiB",
            ),
            (
                "http_addr = \"127.0.0.1:7070\"\notlp_http_addr = \"127.0.0.1:7070\"\n",
                "otlp_http_addr: must differ from http_addr",
                "7070",
            ),
            (
                "http_addr = \"127.0.0.1:7072\"\ngrpc_addr = \"127.0.0.1:7072\"\n",
                "grpc_addr: must differ from http_addr",
                "7072",
            ),
            (
                "otlp_http_addr = \"127.0.0.1:7073\"\ngrpc_addr = \"127.0.0.1:7073\"\n",
                "grpc_addr: must differ from otlp_http_addr",
                "7073",
            ),
            (
                "http_addr = \"127.0.0.1:7074\"\nmetrics_addr = \"127.0.0.1:7074\"\n",
                "metrics_addr: must differ from http_addr",
                "7074",
            ),
            (
                "otlp_http_addr = \"127.0.0.1:7075\"\nmetrics_addr = \"127.0.0.1:7075\"\n",
                "metrics_addr: must differ from otlp_http_addr",
                "7075",
            ),
            (
                "grpc_addr = \"127.0.0.1:7076\"\nmetrics_addr = \"127.0.0.1:7076\"\n",
                "metrics_addr: must differ from grpc_addr",
                "7076",
            ),
        ];
        for (toml, expected, rejected) in cases {
            let error = load_from(Some(toml), &[]).unwrap_err();
            let text = error.to_string();
            assert_eq!(text, expected, "{toml}");
            assert!(!text.contains(rejected), "{text}");
        }
    }

    #[test]
    fn metrics_addr_is_off_by_default_and_when_empty() {
        let default = load_from(None, &[]).unwrap();
        assert_eq!(default.metrics_addr, None);

        let toml = load_from(Some("metrics_addr = \"\"\n"), &[]).unwrap();
        assert_eq!(toml.metrics_addr, None);

        let env = load_from(None, &[("CETRA_METRICS_ADDR", "  ")]).unwrap();
        assert_eq!(env.metrics_addr, None);

        let set = load_from(Some("metrics_addr = \"127.0.0.1:9464\"\n"), &[]).unwrap();
        assert_eq!(set.metrics_addr, Some(socket("127.0.0.1:9464")));

        let from_env = load_from(None, &[("CETRA_METRICS_ADDR", "127.0.0.1:9465")]).unwrap();
        assert_eq!(from_env.metrics_addr, Some(socket("127.0.0.1:9465")));
    }

    #[test]
    fn port_zero_on_every_listener_is_accepted() {
        let config = load_from(
            Some("http_addr = \"127.0.0.1:0\"\notlp_http_addr = \"127.0.0.1:0\"\ngrpc_addr = \"127.0.0.1:0\"\n"),
            &[],
        )
        .unwrap();
        assert_eq!(config.http_addr.port(), 0);
    }

    #[test]
    fn ingest_and_media_accept_the_edges() {
        let config = load_from(
            Some(
                "[ingest]\nmax_request_bytes = \"1KiB\"\nmax_attribute_bytes = \"1KiB\"\nmax_inflight_bytes = \"1KiB\"\nmax_grpc_message_bytes = \"1KiB\"\n\n[media]\nmin_bytes = \"1B\"\nmax_bytes = \"1GiB\"\n",
            ),
            &[],
        )
        .unwrap();
        assert_eq!(config.ingest.max_request_bytes, ByteSize::kib(1));
        assert_eq!(config.media.min_bytes, ByteSize::b(1));
        assert_eq!(config.media.max_bytes, ByteSize::gib(1));
    }

    /// The configuration reference page (D-W-7): its keys are exactly the keys the loader
    /// accepts, and its Environment column is exactly `ALIASES`.
    const REFERENCE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../website/src/content/docs/docs/self-hosting/configuration-reference.md"
    ));

    /// What a probe found at a table path.
    enum Probe {
        /// A table that refuses unknown fields, with the fields it accepts.
        Table(Vec<String>),
        /// A field that takes an array of tables.
        ArrayOfTables,
        /// Any other field.
        Leaf,
    }

    /// The text between the first pair of backticks of a table cell.
    fn backticked(cell: &str) -> Option<String> {
        let (_, rest) = cell.split_once('`')?;
        let (text, _) = rest.split_once('`')?;
        Some(text.to_owned())
    }

    /// The keys of the page's tables, each with the name of its Environment cell if it has one.
    fn documented() -> BTreeMap<String, Option<String>> {
        let mut keys = BTreeMap::new();
        for line in REFERENCE.lines().filter(|line| line.starts_with("| `")) {
            let cells: Vec<&str> = line.split('|').collect();
            let key = backticked(cells[1]).expect("a row starts with a backticked key");
            let env = cells.get(2).and_then(|cell| backticked(cell));
            assert!(
                keys.insert(key.clone(), env).is_none(),
                "{key} is listed twice"
            );
        }
        keys
    }

    /// Loads a document that sets an unknown field in the table at `path` (`a.b[]` is an
    /// element of the array of tables `a.b`) and says what the loader answered.
    fn probe(path: &[String]) -> Probe {
        let mut toml = String::new();
        let mut dotted = Vec::new();
        for (index, segment) in path.iter().enumerate() {
            dotted.push(segment.trim_end_matches("[]"));
            if segment.ends_with("[]") {
                toml.push_str(&format!("[[{}]]\n", dotted.join(".")));
            } else if index + 1 == path.len() {
                toml.push_str(&format!("[{}]\n", dotted.join(".")));
            }
        }
        toml.push_str("zz_probe = 1\n");
        let name = path.join(".");
        let error = match load_from(Some(&toml), &[]) {
            Ok(_) => panic!("{name} accepts an unknown field: add `#[serde(deny_unknown_fields)]`"),
            Err(ConfigError::Figment(error)) => error,
            Err(_) => return Probe::Leaf,
        };
        let target: Vec<&str> = path
            .iter()
            .map(|segment| segment.trim_end_matches("[]"))
            .collect();
        for part in (*error).clone() {
            let part_path: Vec<&str> = part
                .path
                .iter()
                .map(String::as_str)
                .filter(|segment| segment.parse::<usize>().is_err())
                .collect();
            // Figment's path for an unknown field ends with the field itself.
            let owner = part_path.strip_suffix(&["zz_probe"]).unwrap_or(&part_path);
            match &part.kind {
                Kind::UnknownField(_, expected) if owner == target => {
                    return Probe::Table(
                        expected.iter().map(|field| (*field).to_owned()).collect(),
                    );
                }
                Kind::InvalidType(Actual::Map, expected)
                    if part_path == target && expected.contains("sequence") =>
                {
                    return Probe::ArrayOfTables;
                }
                _ => {}
            }
        }
        Probe::Leaf
    }

    /// Every leaf key the loader accepts, found by walking the tables from the top.
    fn accepted() -> BTreeSet<String> {
        fn walk(path: &[String], keys: &mut BTreeSet<String>) {
            let Probe::Table(fields) = probe(path) else {
                panic!("{} is not a table", path.join("."));
            };
            for field in fields {
                let mut child = path.to_vec();
                child.push(field);
                match probe(&child) {
                    Probe::Table(_) => walk(&child, keys),
                    Probe::ArrayOfTables => {
                        if let Some(last) = child.last_mut() {
                            last.push_str("[]");
                        }
                        if matches!(probe(&child), Probe::Table(_)) {
                            walk(&child, keys);
                        } else {
                            // An array of scalars (a `Vec<String>`), not of tables.
                            if let Some(last) = child.last_mut() {
                                last.truncate(last.len() - 2);
                            }
                            keys.insert(child.join("."));
                        }
                    }
                    Probe::Leaf => {
                        keys.insert(child.join("."));
                    }
                }
            }
        }
        let mut keys = BTreeSet::new();
        // The top level is the one table without a header.
        let top = match probe(&[]) {
            Probe::Table(fields) => fields,
            _ => panic!("the top level refuses no unknown field"),
        };
        for field in top {
            let child = vec![field];
            match probe(&child) {
                Probe::Table(_) => walk(&child, &mut keys),
                Probe::ArrayOfTables => walk(&[format!("{}[]", child[0])], &mut keys),
                Probe::Leaf => {
                    keys.insert(child[0].clone());
                }
            }
        }
        // The loader copies the top-level `data_dir` into `[storage]` and rejects this one.
        keys.remove("storage.data_dir");
        keys
    }

    #[test]
    fn the_configuration_reference_matches_the_loader() {
        let documented = documented();
        let accepted = accepted();
        let missing: Vec<_> = accepted
            .iter()
            .filter(|key| !documented.contains_key(*key))
            .collect();
        let unknown: Vec<_> = documented
            .keys()
            .filter(|key| !accepted.contains(*key))
            .collect();
        assert!(
            missing.is_empty(),
            "keys the loader accepts but the reference omits: {missing:?}"
        );
        assert!(
            unknown.is_empty(),
            "keys the reference lists but the loader refuses: {unknown:?}"
        );
        for (alias, key) in ALIASES {
            assert_eq!(
                documented.get(*key).and_then(Option::as_deref),
                Some(*alias),
                "{key} must list {alias} in the Environment column"
            );
        }
        for (key, env) in &documented {
            if let Some(env) = env {
                assert!(
                    ALIASES
                        .iter()
                        .any(|(alias, aliased)| alias == env && aliased == key),
                    "{key}: {env} is not its alias in ALIASES"
                );
            }
        }
    }
}
