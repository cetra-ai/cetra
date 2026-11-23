// SPDX-License-Identifier: Elastic-2.0
//! SAML 2.0 service provider for per-org single sign-on (track SA, `docs/plans/sa-saml/`).
//!
//! Pure functions over bytes and explicit inputs: no I/O, no clock, no randomness, no logging.
//! The server (M2) and the control plane (by git) share it.

mod cert;
mod metadata;
pub(crate) mod xml;

pub use cert::IdpCertificate;
pub use metadata::{
    IdpMetadata, MAX_CERTIFICATES, MAX_METADATA_BYTES, MetadataError, parse_metadata,
};
