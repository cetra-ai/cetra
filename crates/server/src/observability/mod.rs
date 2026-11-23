//! cetra's own observability (track TL, `docs/plans/tl-telemetry/README.md`): the panic hook,
//! the OTLP log export to the operator's endpoint, and the `tracing` subscriber the CLI
//! installs. Prometheus `/metrics` is [`crate::telemetry`] (D-N-28).

pub mod panic;
pub(crate) mod buckets;
pub(crate) mod facts;
pub(crate) mod instance;
pub(crate) mod switchboard;
