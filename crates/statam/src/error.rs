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

    /// Transient object-store or transport error (privacy-safe static reason only).
    #[error("store error: {reason}")]
    Store {
        /// Static diagnostic reason (never URLs, endpoints, bucket names, or keys).
        reason: &'static str,
    },

    /// Operation was cancelled cooperatively via a `CancelToken`.
    #[error("statam operation cancelled")]
    Cancelled,

    /// Operation exceeded its memory `Reservation` budget.
    #[error("statam memory limit exceeded")]
    MemoryExceeded,
}

impl StatamError {
    /// Returns `true` for transient object-store/transport errors that must not trigger quarantine.
    #[inline]
    pub fn is_store_error(&self) -> bool {
        matches!(self, Self::Store { .. })
    }

    /// Returns `true` for data corruption, format/schema mismatch, missing pack, or missing/truncated file I/O.
    #[inline]
    pub fn is_corrupt(&self) -> bool {
        match self {
            Self::Corrupt { .. }
            | Self::UnsupportedVersion { .. }
            | Self::UnsupportedFeature { .. }
            | Self::BoundsExceeded { .. }
            | Self::ChecksumMismatch { .. }
            | Self::MissingPack { .. }
            | Self::SchemaMismatch { .. } => true,
            Self::Io(err) => matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::UnexpectedEof
            ),
            Self::Store { .. }
            | Self::Cancelled
            | Self::MemoryExceeded
            | Self::EncryptionRequired
            | Self::Column(_) => false,
        }
    }
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

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn error_classification_is_store_and_is_corrupt() {
        let store_err = StatamError::Store {
            reason: "object store read failed",
        };
        assert!(store_err.is_store_error());
        assert!(!store_err.is_corrupt());
        assert!(store_err.to_string().contains("object store read failed"));

        let cancelled = StatamError::Cancelled;
        assert!(!cancelled.is_store_error());
        assert!(!cancelled.is_corrupt());

        let mem = StatamError::MemoryExceeded;
        assert!(!mem.is_store_error());
        assert!(!mem.is_corrupt());

        let enc = StatamError::EncryptionRequired;
        assert!(!enc.is_store_error());
        assert!(!enc.is_corrupt());

        let col_err = StatamError::Column(cetra_column::ColumnError::InvalidBatch {
            reason: "bad batch",
        });
        assert!(!col_err.is_store_error());
        assert!(!col_err.is_corrupt());

        let corrupt_variants = [
            StatamError::Corrupt {
                reason: "bad magic",
            },
            StatamError::UnsupportedVersion {
                found: 99,
                expected: 1,
            },
            StatamError::UnsupportedFeature { mask: 0x1 },
            StatamError::BoundsExceeded { reason: "too big" },
            StatamError::ChecksumMismatch { context: "block" },
            StatamError::MissingPack { pack_id: [7u8; 16] },
            StatamError::SchemaMismatch { reason: "mismatch" },
            StatamError::Io(io::Error::new(io::ErrorKind::NotFound, "gone")),
            StatamError::Io(io::Error::new(io::ErrorKind::UnexpectedEof, "eof")),
        ];
        for err in &corrupt_variants {
            assert!(
                !err.is_store_error(),
                "unexpected is_store_error for {err:?}"
            );
            assert!(err.is_corrupt(), "expected is_corrupt for {err:?}");
        }

        let non_corrupt_io = [
            StatamError::Io(io::Error::new(io::ErrorKind::TimedOut, "timeout")),
            StatamError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
            StatamError::Io(io::Error::new(io::ErrorKind::Interrupted, "interrupted")),
        ];
        for err in &non_corrupt_io {
            assert!(!err.is_store_error());
            assert!(!err.is_corrupt(), "unexpected is_corrupt for {err:?}");
        }
    }
}
