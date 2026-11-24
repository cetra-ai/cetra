// SPDX-License-Identifier: Elastic-2.0
//! IdP signing certificates: the pinned key and its validity dates for display (D-SA-11).

use base64::Engine as _;
use bergshamra::keys::loader::load_x509_cert_der;
use rsa::pkcs8::DecodePublicKey;
use rsa::traits::PublicKeyParts;
use sha2::{Digest, Sha256};
use x509_cert::der::Decode;
use x509_cert::time::Time;

use crate::metadata::MetadataError;

/// The largest DER certificate accepted, in bytes.
const MAX_CERTIFICATE_DER_BYTES: usize = 16_384;
/// The smallest RSA modulus accepted, in bytes (2048 bits).
const MIN_RSA_MODULUS_BYTES: usize = 256;
/// The largest PEM text accepted, in bytes (review L7).
const MAX_PEM_BYTES: usize = 65_536;

/// A trusted IdP signing certificate: a pinned key plus its validity dates for display (D-SA-11).
#[derive(Clone, PartialEq, Eq)]
pub struct IdpCertificate {
    der: Vec<u8>,
    sha256: [u8; 32],
    not_before_ns: i64,
    not_after_ns: i64,
}

impl IdpCertificate {
    /// Reads a DER certificate and checks its key.
    pub fn from_der(der: &[u8]) -> Result<Self, MetadataError> {
        if der.len() > MAX_CERTIFICATE_DER_BYTES {
            return Err(MetadataError::Certificate);
        }
        let cert = x509_cert::Certificate::from_der(der).map_err(|_| MetadataError::Certificate)?;
        let not_before_ns = unix_nanos(cert.tbs_certificate.validity.not_before);
        let not_after_ns = unix_nanos(cert.tbs_certificate.validity.not_after);

        let key = load_x509_cert_der(der).map_err(|_| MetadataError::Certificate)?;
        match key.data.algorithm_name() {
            "RSA" => {
                let spki = key.data.to_spki_der().ok_or(MetadataError::Certificate)?;
                let public = rsa::PublicKey::from_public_key_der(&spki)
                    .map_err(|_| MetadataError::Certificate)?;
                if public.size() < MIN_RSA_MODULUS_BYTES {
                    return Err(MetadataError::WeakKey);
                }
            }
            "EC-P256" | "EC-P384" => {}
            _ => return Err(MetadataError::WeakKey),
        }

        let sha256: [u8; 32] = Sha256::digest(der).into();
        Ok(Self {
            der: der.to_vec(),
            sha256,
            not_before_ns,
            not_after_ns,
        })
    }

    /// Reads a PEM `CERTIFICATE` block, or bare base64 of the DER (as IdP consoles show it).
    pub fn from_pem(text: &str) -> Result<Self, MetadataError> {
        const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
        const END: &str = "-----END CERTIFICATE-----";
        if text.len() > MAX_PEM_BYTES {
            return Err(MetadataError::Certificate);
        }
        let text = text.trim_ascii();
        let body = match text.strip_prefix(BEGIN) {
            Some(rest) => rest
                .split_once(END)
                .map(|(body, _)| body)
                .ok_or(MetadataError::Certificate)?,
            None => text,
        };
        let compact: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let der = base64::engine::general_purpose::STANDARD
            .decode(compact)
            .map_err(|_| MetadataError::Certificate)?;
        Self::from_der(&der)
    }

    /// The DER bytes.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The SHA-256 of the DER, lowercase hex: the certificate's display and dedup identity.
    pub fn sha256_hex(&self) -> String {
        hex::encode(self.sha256)
    }

    /// `notBefore`, nanoseconds since the epoch.
    pub fn not_before_ns(&self) -> i64 {
        self.not_before_ns
    }

    /// `notAfter`, nanoseconds since the epoch. Shown and warned about, never enforced (D-SA-11).
    /// `i64::MAX` when the certificate expires after April 2262.
    pub fn not_after_ns(&self) -> i64 {
        self.not_after_ns
    }

    /// The bergshamra key for verification.
    pub(crate) fn key(&self) -> Result<bergshamra::Key, MetadataError> {
        load_x509_cert_der(&self.der).map_err(|_| MetadataError::Certificate)
    }
}

impl std::fmt::Debug for IdpCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdpCertificate")
            .field("sha256", &self.sha256_hex())
            .finish_non_exhaustive()
    }
}

/// A certificate time as nanoseconds since the epoch, saturated at `i64::MAX` (April 2262): the
/// dates are shown, never enforced, and some IdPs issue certificates valid for centuries.
fn unix_nanos(time: Time) -> i64 {
    i64::try_from(time.to_unix_duration().as_nanos()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDP_RSA: &str = include_str!("../../../fixtures/saml/idp-rsa.cert.pem");
    const IDP_EC: &str = include_str!("../../../fixtures/saml/idp-ec.cert.pem");
    const IDP_EC384: &str = include_str!("../../../fixtures/saml/idp-ec384.cert.pem");
    const WEAK_RSA: &str = include_str!("../../../fixtures/saml/weak-rsa-1024.cert.pem");
    const FAR_FUTURE: &str = include_str!("../../../fixtures/saml/idp-far-future.cert.pem");

    /// The base64 body of a PEM block, without its markers and line breaks.
    fn pem_body(pem: &str) -> String {
        pem.lines()
            .filter(|line| !line.starts_with("-----"))
            .collect()
    }

    #[test]
    fn an_rsa_2048_certificate_is_read() {
        let cert = IdpCertificate::from_pem(IDP_RSA).expect("fixture is a valid RSA certificate");
        assert!(cert.not_after_ns() > 4_102_444_800_000_000_000); // after 2100-01-01
        assert!(cert.not_after_ns() > cert.not_before_ns());
        assert_eq!(cert.sha256_hex().len(), 64);
    }

    #[test]
    fn a_certificate_expiring_after_2262_is_read() {
        let cert = IdpCertificate::from_pem(FAR_FUTURE)
            .expect("a far-future X.509 v1 certificate is accepted");
        assert_eq!(cert.not_after_ns(), i64::MAX);
        assert!(cert.not_before_ns() < cert.not_after_ns());
    }

    #[test]
    fn an_ec_p256_certificate_is_read() {
        assert!(IdpCertificate::from_pem(IDP_EC).is_ok());
    }

    #[test]
    fn an_ec_p384_certificate_is_read() {
        assert!(IdpCertificate::from_pem(IDP_EC384).is_ok());
    }

    #[test]
    fn bare_base64_is_read() {
        let pem = IdpCertificate::from_pem(IDP_RSA).expect("fixture is a valid certificate");
        let bare = IdpCertificate::from_pem(&pem_body(IDP_RSA)).expect("bare base64 is accepted");
        assert_eq!(pem, bare);
    }

    #[test]
    fn a_1024_bit_rsa_key_is_weak() {
        assert_eq!(
            IdpCertificate::from_pem(WEAK_RSA).err(),
            Some(MetadataError::WeakKey)
        );
    }

    #[test]
    fn garbage_is_not_a_certificate() {
        assert_eq!(
            IdpCertificate::from_der(b"not a cert").err(),
            Some(MetadataError::Certificate)
        );
        assert_eq!(
            IdpCertificate::from_pem("-----BEGIN CERTIFICATE-----\nAAAA").err(),
            Some(MetadataError::Certificate)
        );
    }

    #[test]
    fn an_oversized_pem_is_refused() {
        // Valid PEM followed by whitespace past the limit: refused by size, not by content.
        let text = format!("{IDP_RSA}{}", " ".repeat(65_536));
        assert_eq!(
            IdpCertificate::from_pem(&text).err(),
            Some(MetadataError::Certificate)
        );
    }

    #[test]
    fn debug_shows_only_the_hash() {
        let cert = IdpCertificate::from_pem(IDP_RSA).expect("fixture is a valid certificate");
        let debug = format!("{cert:?}");
        assert!(debug.contains(&cert.sha256_hex()));
        assert!(!debug.contains(&pem_body(IDP_RSA)));
    }
}
