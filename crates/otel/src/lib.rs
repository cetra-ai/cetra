//! OTLP decoding, convention detection and normalization, media extraction,
//! truncation, and cost computation.
//!
//! Decoding and the span skeleton are specified in `docs/spec/02-ingestion.md`.

mod agents;
mod bag;
mod context;
mod conventions;
mod convert;
mod cost;
mod decode;
mod detect;
mod error;
mod fields;
mod kind;
mod logs;
mod media;
mod normalize;
mod payload;
mod reject;
mod truncate;
mod usage;
mod value;
mod warn;

/// Fuzz entry points (track R M3). Not part of the API.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzz_entry;

pub use agents::{AgentContext, AgentContexts, canonical_repository};
pub use cost::{PriceRow, PriceScope, PriceTable, PriceTier, builtin_price_rows, exact_pattern};
pub use decode::{decode_json, decode_protobuf};
pub use error::{DecodeError, JsonErrorCategory, PriceError};
pub use logs::{count_genai_log_records, decode_logs_json, decode_logs_protobuf};
pub use normalize::{IngestContext, IngestLimits, Normalized, normalize};
pub use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
pub use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
pub use reject::{RejectReason, Rejections};
