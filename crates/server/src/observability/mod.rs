//! cetra's own observability (track TL, `docs/plans/tl-telemetry/README.md`): the panic hook,
//! the OTLP log and span export to the operator's endpoint, the server's spans, and the
//! `tracing` subscriber the CLI installs. Prometheus `/metrics` is [`crate::telemetry`] (D-N-28).

pub(crate) mod buckets;
pub(crate) mod config_view;
pub(crate) mod crash;
pub mod disk;
pub(crate) mod facts;
pub(crate) mod heartbeat;
pub(crate) mod host;
pub(crate) mod instance;
pub(crate) mod otlp;
pub mod panic;
pub(crate) mod ring;
pub(crate) mod scrub;
pub(crate) mod spans;
pub mod subscriber;
pub(crate) mod switchboard;

/// Why the subscriber or the export could not be built. Messages name keys and static
/// reasons, never a header or resource value.
#[derive(Debug, thiserror::Error)]
pub enum ObservabilityError {
    /// The export's HTTP client could not be built.
    #[error("self_monitoring: cannot build the HTTP client")]
    Client,
    /// `self_monitoring.otlp_headers` did not convert to HTTP headers.
    #[error("self_monitoring.otlp_headers: invalid header")]
    Header,
    /// `self_monitoring.resource` did not parse.
    #[error("self_monitoring.resource: invalid attribute list")]
    Resource,
    /// `log.level` is not a valid filter.
    #[error("log.level: {0}")]
    LogLevel(tracing_subscriber::filter::ParseError),
    /// `self_monitoring.level` is not a valid filter.
    #[error("self_monitoring.level: {0}")]
    ExportLevel(tracing_subscriber::filter::ParseError),
    /// [`subscriber::init`] was called outside a tokio runtime.
    #[error("no tokio runtime to run the export on")]
    NoRuntime,
    /// A global subscriber is already installed.
    #[error("a global tracing subscriber is already installed")]
    AlreadyInstalled,
}
