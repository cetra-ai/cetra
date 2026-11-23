//! cetra's own observability (track TL, `docs/plans/tl-telemetry/README.md`): the panic hook,
//! the OTLP log export to the operator's endpoint, and the `tracing` subscriber the CLI
//! installs. Prometheus `/metrics` is [`crate::telemetry`] (D-N-28).

pub(crate) mod otlp;
pub mod panic;
pub(crate) mod buckets;
pub(crate) mod facts;
pub(crate) mod instance;
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
}
