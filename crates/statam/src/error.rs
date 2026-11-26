//! Privacy-safe error types for `cetra-statam`.

use std::fmt;

use thiserror::Error;

/// Errors produced by `cetra-statam`.
///
/// `Display` and `Debug` intentionally never include payload text, user/session
/// IDs, or map values.
#[derive(Debug, Error)]
pub enum StatamError {
    /// Corrupt container bytes, invalid magic, or malformed structure.
    #[error("corrupt statam data: {reason}")]
    Corrupt {
        /// Static diagnostic reason (never payload bytes).
        reason: &'static str,
    },

    /// File or pack format version is unsupported by this reader.
    #[error("unsupported statam format version {found} (expected {expected})")]
    UnsupportedVersion {
        /// Version found in the file.
        found: u16,
        /// Version supported by this reader.
        expected: u16,
    },

    /// File or pack requires unknown feature bits.
    #[error("unsupported statam feature bits: 0x{mask:08x}")]
    UnsupportedFeature {
        /// Unsupported feature bitmask.
        mask: u32,
    },

    /// Untrusted-input limit exceeded before or during allocation.
    #[error("statam bounds exceeded: {reason}")]
    BoundsExceeded {
        /// Static diagnostic reason (never payload bytes).
        reason: &'static str,
    },

    /// CRC-32C checksum verification failed.
    #[error("checksum mismatch in {context}")]
    ChecksumMismatch {
        /// Static diagnostic context.
        context: &'static str,
    },

    /// File or pack is encrypted and no decryption key was provided.
    #[error("encryption key required to read encrypted statam data")]
    EncryptionRequired,

    /// Schema or column type mismatch between file metadata and request.
    #[error("schema mismatch: {reason}")]
    SchemaMismatch {
        /// Static diagnostic reason.
        reason: &'static str,
    },

    /// Referenced external `.stpk` chunk pack could not be resolved.
    #[error("missing chunk pack {}", Hex16(*.pack_id))]
    MissingPack {
        /// 128-bit pack identifier.
        pack_id: [u8; 16],
    },

    /// Underlying I/O error.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// Error propagated from `cetra-column`.
    #[error("column error: {0}")]
    Column(#[from] cetra_column::ColumnError),
}

struct Hex16([u8; 16]);

impl fmt::Display for Hex16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}
