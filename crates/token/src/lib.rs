// SPDX-License-Identifier: Elastic-2.0
//! Signed tokens: licence keys, org entitlement tokens and control-plane requests (D-P-5).
//!
//! A token is `cetra1.<kid>.<payload>.<signature>`. `<payload>` and `<signature>` are
//! base64url without padding. The signature is ed25519 over the ASCII bytes of
//! `cetra1.<kid>.<payload>`, checked before the payload is decoded as JSON, so the JSON
//! is never re-serialised. Release builds trust only the release keys; debug builds also
//! trust the development key (D-P-6). Each key may sign only the kinds it is bound to
//! (D-P-27). No config can add a key.
//!
//! Shared by `cetra-server`, `tools/license` and the control plane. Depends on no other
//! workspace crate.

#![warn(missing_docs)]

use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod keys;
mod rfc3339;

pub use keys::{BuiltInKey, built_in_keys};

/// Format tag. Renamed with the product.
pub const TOKEN_TAG: &str = "cetra1";

/// Largest accepted token, in bytes.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;

/// What a token is for. The JSON `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A self-hosted licence key (D-P-7).
    License,
    /// An org's entitlements, pushed to its cell (D-P-11).
    Org,
    /// A control-plane request to a cell (D-P-24).
    ControlPlane,
}

/// Why a token was refused. The text never quotes the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// More than [`MAX_TOKEN_BYTES`].
    #[error("the token is longer than 16 KiB")]
    TooLarge,
    /// Not four dot-separated parts, a wrong tag, a bad key id, or bad base64.
    #[error("the token is malformed")]
    Malformed,
    /// The key id is not trusted by this build.
    #[error("the token is signed with a key this build does not trust")]
    UnknownKey,
    /// The signature does not match.
    #[error("the token signature does not verify")]
    BadSignature,
    /// The key may not sign this kind of token (D-P-27).
    #[error("the token's key may not sign this kind of token")]
    KeyNotAllowed,
    /// The payload is not a JSON object of a known kind with every required field.
    #[error("the token payload is invalid")]
    Payload,
    /// A valid token of another kind than the caller expects.
    #[error("the token is not of the expected kind")]
    WrongKind,
}

/// `kind: "license"` (D-P-7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseClaims {
    /// Licence id (UUIDv7).
    pub id: Uuid,
    /// ns. When the token was issued.
    #[serde(with = "rfc3339")]
    pub issued_at: i64,
    /// Display name of the customer.
    pub customer: String,
    /// Feature names; unknown names are ignored by the reader.
    pub features: Vec<String>,
    /// ns. Active before this instant.
    #[serde(with = "rfc3339")]
    pub expires_at: i64,
    /// ns. In grace before this instant, expired from it on.
    #[serde(with = "rfc3339")]
    pub grace_ends_at: i64,
}

/// `kind: "org"` (D-P-11). `audience` is the cell's `public_url`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgClaims {
    /// Token id (UUIDv7).
    pub id: Uuid,
    /// ns. When the token was issued; the newest `issued_at` wins on a cell.
    #[serde(with = "rfc3339")]
    pub issued_at: i64,
    /// ns. The token is valid before this instant.
    #[serde(with = "rfc3339")]
    pub expires_at: i64,
    /// ns. The grace period ends at this instant.
    #[serde(with = "rfc3339")]
    pub grace_ends_at: i64,
    /// The cell's `public_url` the token is meant for.
    pub audience: String,
    /// The org the entitlements belong to.
    pub org_id: Uuid,
    /// The plan the org is on.
    pub plan: PlanClaim,
    /// Features granted to the org. Missing: no feature.
    #[serde(default)]
    pub features: Vec<String>,
    /// Limits of the org. Missing: every limit takes the cell's default (D-P-13).
    #[serde(default)]
    pub limits: LimitsClaim,
    /// The org's verified SSO domains on the control plane, used by cells for SCIM only (D-EA-17).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sso_domains: Vec<String>,
}

/// The plan an org token names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanClaim {
    /// Stable plan id, for example `pro`.
    pub id: String,
    /// Display name of the plan.
    pub name: String,
    /// Add-on names, for example `teams`. Missing: none.
    #[serde(default)]
    pub add_ons: Vec<String>,
}

/// The limits of D-P-13 and D-IC-13. Outer `None`: the key is missing (the cell's default applies);
/// `Some(None)`: JSON `null` (unlimited); `Some(Some(n))`: the limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitsClaim {
    /// Units included each month.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub units_included: Option<Option<u64>>,
    /// Hard cap on units each month.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub units_cap: Option<Option<u64>>,
    /// Sustained ingest rate, spans per second.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub ingest_spans_per_sec: Option<Option<u64>>,
    /// Ingest burst size, spans.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub ingest_burst: Option<Option<u64>>,
    /// How many days of data the org can see.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub data_access_days: Option<Option<u64>>,
    /// Members and pending invites.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_seats: Option<Option<u64>>,
    /// Active projects.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_projects: Option<Option<u64>>,
    /// Alert rules.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_alert_rules: Option<Option<u64>>,
    /// Enabled online evaluation rules.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_active_online_rules: Option<Option<u64>>,
    /// Enabled issue sources of active projects (D-IC-13).
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_active_issue_sources: Option<Option<u64>>,
    /// Annotation queues per org (D-AQ-10).
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_annotation_queues: Option<Option<u64>>,
    /// Audit log retention in days (D-EA-7): `0` keeps forever. Missing or `null`: the
    /// instance's `audit.retention_days`.
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub audit_retention_days: Option<Option<u64>>,
}

/// `kind: "control_plane"` (D-P-24, D-P-28).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPlaneClaims {
    /// Request id (UUIDv7).
    pub id: Uuid,
    /// ns. When the request was issued.
    #[serde(with = "rfc3339")]
    pub issued_at: i64,
    /// ns. The request is valid before this instant.
    #[serde(with = "rfc3339")]
    pub expires_at: i64,
    /// The cell's `public_url` the request is meant for.
    pub audience: String,
    /// One of `orgs`, `usage`, `deletions`, `members`, `users`; the cell matches it per route.
    pub scope: String,
    /// The HTTP method of the one request the token is for, in capitals (D-P-71).
    pub method: String,
    /// The path of that request, without the query: it names the org or user acted on, so a
    /// captured token cannot be replayed on another resource (D-P-71).
    pub path: String,
}

/// A payload, tagged by `kind`.
// One value is parsed per token and used at once, so the size gap between variants costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Claims {
    /// `kind: "license"`.
    License(LicenseClaims),
    /// `kind: "org"`.
    Org(OrgClaims),
    /// `kind: "control_plane"`.
    ControlPlane(ControlPlaneClaims),
}

impl Claims {
    /// The payload's kind.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::License(_) => Kind::License,
            Self::Org(_) => Kind::Org,
            Self::ControlPlane(_) => Kind::ControlPlane,
        }
    }
}

/// A token whose signature, key binding and payload are valid. Its dates are not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The key that signed it.
    pub kid: String,
    /// The payload.
    pub claims: Claims,
}

/// Verifies `token` (surrounding whitespace ignored) against the built-in keys, in this
/// order: size and shape (`TooLarge`, `Malformed`), a trusted `kid` (`UnknownKey`), the
/// signature (`BadSignature`), the payload's `kind` (`Payload` when absent or unknown), the
/// key's binding to that kind (`KeyNotAllowed`), the full payload (`Payload`).
///
/// # Errors
///
/// Returns the first [`TokenError`] that applies. The text never contains the token.
pub fn verify(token: &str) -> Result<Verified, TokenError> {
    verify_with(token, trusted_keys())
}

/// The built-in keys decoded once, on first use.
fn trusted_keys() -> &'static [(&'static BuiltInKey, VerifyingKey)] {
    static KEYS: OnceLock<Vec<(&'static BuiltInKey, VerifyingKey)>> = OnceLock::new();
    KEYS.get_or_init(|| {
        built_in_keys()
            .filter_map(|key| keys::decode(key.public).map(|verifying| (key, verifying)))
            .collect()
    })
}

/// [`verify`] against an explicit key table.
fn verify_with(token: &str, keys: &[(&BuiltInKey, VerifyingKey)]) -> Result<Verified, TokenError> {
    let token = token.trim();
    if token.len() > MAX_TOKEN_BYTES {
        return Err(TokenError::TooLarge);
    }
    let mut parts = token.split('.');
    let (Some(tag), Some(kid), Some(payload), Some(signature), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(TokenError::Malformed);
    };
    if tag != TOKEN_TAG || !valid_kid(kid) {
        return Err(TokenError::Malformed);
    }
    let (key, verifying) = keys
        .iter()
        .find(|(key, _)| key.kid == kid)
        .ok_or(TokenError::UnknownKey)?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| TokenError::Malformed)?;
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| TokenError::Malformed)?;
    let signature = Signature::from_slice(&signature_bytes).map_err(|_| TokenError::Malformed)?;
    let signed_len = tag.len() + 1 + kid.len() + 1 + payload.len();
    let signed = token
        .as_bytes()
        .get(..signed_len)
        .ok_or(TokenError::Malformed)?;
    verifying
        .verify_strict(signed, &signature)
        .map_err(|_| TokenError::BadSignature)?;

    #[derive(Deserialize)]
    struct Probe {
        kind: Kind,
    }
    let probe: Probe = serde_json::from_slice(&payload_bytes).map_err(|_| TokenError::Payload)?;
    if !key.kinds.contains(&probe.kind) {
        return Err(TokenError::KeyNotAllowed);
    }
    let claims: Claims = serde_json::from_slice(&payload_bytes).map_err(|_| TokenError::Payload)?;
    Ok(Verified {
        kid: kid.to_owned(),
        claims,
    })
}

/// Signs `claims` with `key` under `kid`, as compact JSON.
///
/// # Errors
///
/// Returns [`TokenError::Malformed`] when `kid` is not `[a-z0-9-]{1,32}`, and
/// [`TokenError::Payload`] when the claims cannot be serialised.
pub fn sign(key: &SigningKey, kid: &str, claims: &Claims) -> Result<String, TokenError> {
    let json = serde_json::to_vec(claims).map_err(|_| TokenError::Payload)?;
    sign_raw(key, kid, &json)
}

/// Signs the bytes `payload` as they are, whether or not they are JSON. For tests and the
/// development tool.
///
/// # Errors
///
/// Returns [`TokenError::Malformed`] when `kid` is not `[a-z0-9-]{1,32}`.
pub fn sign_raw(key: &SigningKey, kid: &str, payload: &[u8]) -> Result<String, TokenError> {
    if !valid_kid(kid) {
        return Err(TokenError::Malformed);
    }
    let signed = format!("{TOKEN_TAG}.{kid}.{}", URL_SAFE_NO_PAD.encode(payload));
    let signature = key.sign(signed.as_bytes());
    Ok(format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

/// `[a-z0-9-]{1,32}`.
fn valid_kid(kid: &str) -> bool {
    (1..=32).contains(&kid.len())
        && kid
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// `Option<Option<u64>>` where a missing key is `None`, JSON `null` is `Some(None)` and a
/// number is `Some(Some(n))`. Pair it with `#[serde(default)]`.
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<Option<u64>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            None => serializer.serialize_none(),
            Some(inner) => inner.serialize(serializer),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Option<u64>>, D::Error> {
        Option::<u64>::deserialize(deserializer).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;

    /// The development seed, as shipped in `fixtures/license/dev-1.key`.
    const DEV_KEY_FILE: &str = include_str!("../../../fixtures/license/dev-1.key");

    fn fixture(path: &str) -> String {
        std::fs::read_to_string(format!(
            "{}/../../fixtures/license/{path}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn dev_key() -> SigningKey {
        let seed: [u8; 32] = STANDARD
            .decode(DEV_KEY_FILE.trim())
            .unwrap()
            .try_into()
            .unwrap();
        SigningKey::from_bytes(&seed)
    }

    /// Signs `payload` as a `dev-1` token.
    fn dev_sign(payload: &[u8]) -> String {
        sign_raw(&dev_key(), "dev-1", payload).unwrap()
    }

    fn parse_claims(text: &str) -> Claims {
        serde_json::from_str(text).unwrap()
    }

    fn ns(text: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap()
    }

    #[test]
    fn keys_decode() {
        for key in built_in_keys() {
            assert!(
                keys::decode(key.public).is_some(),
                "{} does not decode",
                key.kid
            );
            assert!(valid_kid(key.kid), "{} is not a valid kid", key.kid);
        }
    }

    #[test]
    fn cloud_keys_cannot_sign_licences() {
        for kid in ["cloud-1", "cloud-2"] {
            let key = built_in_keys().find(|key| key.kid == kid).unwrap();
            assert!(!key.kinds.contains(&Kind::License), "{kid} signs licences");
            assert_eq!(key.kinds, &[Kind::Org, Kind::ControlPlane]);
        }
    }

    #[test]
    fn licence_keys_sign_licences_only() {
        let control_plane = r#"{"kind":"control_plane","id":"0193a3b4-0000-7000-8000-000000000001",
            "issued_at":"2026-11-14T00:00:00Z","expires_at":"2099-01-01T00:00:00Z",
            "audience":"https://eu1.example.com","scope":"orgs","method":"PUT",
            "path":"/api/internal/v1/orgs/0193a3b4-0000-7000-8000-000000000002"}"#;
        assert!(verify(&dev_sign(control_plane.as_bytes())).is_ok());

        for kid in ["license-offline-1", "license-1", "license-2"] {
            let key = built_in_keys().find(|key| key.kid == kid).unwrap();
            assert_eq!(key.kinds, &[Kind::License], "{kid}");

            // The same binding, with the dev key's halves standing in for this key's.
            let bound = BuiltInKey {
                kid: "dev-1",
                public: "",
                kinds: key.kinds,
            };
            let keys = [(&bound, dev_key().verifying_key())];
            assert!(
                verify_with(&dev_sign(fixture("payloads/active.json").as_bytes()), &keys).is_ok(),
                "{kid}"
            );
            for payload in [fixture("payloads/org-pro.json"), control_plane.to_owned()] {
                assert_eq!(
                    verify_with(&dev_sign(payload.as_bytes()), &keys),
                    Err(TokenError::KeyNotAllowed),
                    "{kid}"
                );
            }
        }
    }

    #[test]
    fn dev_seed_matches_dev_key() {
        let public = dev_key().verifying_key().to_bytes();
        let dev = built_in_keys().find(|key| key.kid == "dev-1").unwrap();
        assert_eq!(STANDARD.encode(public), dev.public);
    }

    #[test]
    fn round_trip() {
        let payload = fixture("payloads/active.json");
        let token = dev_sign(payload.as_bytes());
        let verified = verify(&token).unwrap();
        assert_eq!(verified.kid, "dev-1");
        assert_eq!(verified.claims, parse_claims(&payload));

        let padded = format!("  {token}\n");
        assert_eq!(verify(&padded).unwrap(), verified);
    }

    #[test]
    fn tampering_is_refused() {
        let token = dev_sign(fixture("payloads/active.json").as_bytes());
        let parts: Vec<&str> = token.split('.').collect();

        // Old signature, different payload.
        let body = URL_SAFE_NO_PAD.encode(br#"{"kind":"other"}"#);
        let forged = format!("{}.{}.{body}.{}", parts[0], parts[1], parts[3]);
        assert_eq!(verify(&forged), Err(TokenError::BadSignature));

        // Last character of the signature changed to another base64url symbol.
        let signature = parts[3];
        let last = signature.chars().last().unwrap();
        let replacement = if last == 'A' { 'B' } else { 'A' };
        let altered = format!(
            "{}.{}.{}.{}{replacement}",
            parts[0],
            parts[1],
            parts[2],
            &signature[..signature.len() - 1]
        );
        assert!(matches!(
            verify(&altered),
            Err(TokenError::BadSignature | TokenError::Malformed)
        ));
    }

    #[test]
    fn malformed_tokens() {
        let body = URL_SAFE_NO_PAD.encode(b"{}");
        let sig = URL_SAFE_NO_PAD.encode([0_u8; 64]);
        let cases = [
            String::new(),
            "cetra1.dev-1.abc".to_owned(),
            format!("cetra1.dev-1.{body}.{sig}.extra"),
            format!("cetra2.dev-1.{body}.{sig}"),
            format!("cetra1.Dev-1.{body}.{sig}"),
            format!("cetra1.{}.{body}.{sig}", "a".repeat(33)),
            format!("cetra1.dev-1.!!.{sig}"),
            format!("cetra1.dev-1.{body}.{}", URL_SAFE_NO_PAD.encode([0_u8; 10])),
        ];
        for token in cases {
            assert_eq!(verify(&token), Err(TokenError::Malformed), "{token}");
        }
        let oversized = "a".repeat(MAX_TOKEN_BYTES + 1);
        assert_eq!(verify(&oversized), Err(TokenError::TooLarge));
    }

    #[test]
    fn unknown_key() {
        let token = dev_sign(b"{}").replacen(".dev-1.", ".nope-1.", 1);
        assert_eq!(verify(&token), Err(TokenError::UnknownKey));
    }

    #[test]
    fn release_key_needs_prod_signature() {
        let token = dev_sign(fixture("payloads/active.json").as_bytes()).replacen(
            ".dev-1.",
            ".license-offline-1.",
            1,
        );
        assert_eq!(verify(&token), Err(TokenError::BadSignature));
    }

    #[test]
    fn keys_are_bound_to_kinds() {
        // A payload of kind `org` under `license-offline-1` fails on the signature, before the kind check.
        let org = fixture("payloads/org-pro.json");
        let token = dev_sign(org.as_bytes()).replacen(".dev-1.", ".license-offline-1.", 1);
        assert_eq!(verify(&token), Err(TokenError::BadSignature));

        // Every kind verifies under the development key, which is bound to all three.
        for path in [
            "payloads/active.json",
            "payloads/org-pro.json",
            "payloads/wrong-kind.json",
        ] {
            assert!(
                verify(&dev_sign(fixture(path).as_bytes())).is_ok(),
                "{path}"
            );
        }

        // A key bound only to licences refuses an org payload it signed.
        let licence_only = BuiltInKey {
            kid: "dev-1",
            public: "",
            kinds: &[Kind::License],
        };
        let keys = [(&licence_only, dev_key().verifying_key())];
        assert_eq!(
            verify_with(&dev_sign(org.as_bytes()), &keys),
            Err(TokenError::KeyNotAllowed)
        );
    }

    #[test]
    fn payload_errors() {
        let payloads: [&[u8]; 4] = [
            b"not json",
            br#"{"kind":"nope"}"#,
            br#"{"kind":"org"}"#,
            br#"{}"#,
        ];
        for payload in payloads {
            assert_eq!(
                verify(&dev_sign(payload)),
                Err(TokenError::Payload),
                "{}",
                String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn error_text_never_quotes_the_token() {
        for error in [
            TokenError::TooLarge,
            TokenError::Malformed,
            TokenError::UnknownKey,
            TokenError::BadSignature,
            TokenError::KeyNotAllowed,
            TokenError::Payload,
            TokenError::WrongKind,
        ] {
            assert!(!error.to_string().contains("cetra1."), "{error}");
        }
    }

    #[test]
    fn fixture_tokens_verify() {
        for name in ["active", "future-feature", "wrong-kind", "org-pro"] {
            let token = fixture(&format!("tokens/{name}.token"));
            let payload = fixture(&format!("payloads/{name}.json"));
            let verified = verify(&token).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(verified.kid, "dev-1", "{name}");
            assert_eq!(verified.claims, parse_claims(&payload), "{name}");
        }
    }

    #[test]
    fn org_claims_round_trip() {
        let claims = Claims::Org(OrgClaims {
            id: Uuid::parse_str("01900000-0000-7000-8000-000000000520").unwrap(),
            issued_at: ns("2026-01-01T00:00:00Z"),
            expires_at: ns("2099-01-01T00:00:00Z"),
            grace_ends_at: ns("2099-02-01T00:00:00Z"),
            audience: "http://localhost:8080".to_owned(),
            org_id: Uuid::parse_str("01900000-0000-7000-8000-000000000010").unwrap(),
            plan: PlanClaim {
                id: "pro".to_owned(),
                name: "Pro".to_owned(),
                add_ons: vec!["teams".to_owned()],
            },
            features: vec!["retention_management".to_owned()],
            limits: LimitsClaim {
                units_included: Some(None),
                units_cap: Some(None),
                ingest_spans_per_sec: Some(None),
                ingest_burst: Some(None),
                data_access_days: Some(Some(30)),
                max_seats: Some(None),
                max_projects: Some(None),
                max_alert_rules: Some(None),
                max_active_online_rules: Some(None),
                max_active_issue_sources: Some(None),
                max_annotation_queues: Some(None),
                audit_retention_days: Some(Some(365)),
            },
            sso_domains: vec!["acme.test".to_owned()],
        });
        let token = sign(&dev_key(), "dev-1", &claims).unwrap();
        assert_eq!(verify(&token).unwrap().claims, claims);

        let payload = token.split('.').nth(2).unwrap();
        let json = String::from_utf8(URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert!(json.contains(r#""units_cap":null"#), "{json}");
        assert!(json.contains(r#""data_access_days":30"#), "{json}");

        // A missing limit is `None`, and `None` is not written.
        assert_eq!(
            serde_json::to_value(LimitsClaim::default()).unwrap(),
            json!({})
        );
        assert_eq!(
            serde_json::from_str::<LimitsClaim>("{}").unwrap(),
            LimitsClaim::default()
        );

        // An annotation-queue limit from a token that names it round-trips (D-AQ-10).
        let queues = LimitsClaim {
            max_annotation_queues: Some(Some(1)),
            ..LimitsClaim::default()
        };
        let value = serde_json::to_value(&queues).unwrap();
        assert_eq!(value, json!({"max_annotation_queues": 1}));
        assert_eq!(
            serde_json::from_value::<LimitsClaim>(value).unwrap(),
            queues
        );

        // An audit retention limit round-trips too: `null` is distinct from a missing key (D-EA-7).
        let retention = LimitsClaim {
            audit_retention_days: Some(None),
            ..LimitsClaim::default()
        };
        let value = serde_json::to_value(&retention).unwrap();
        assert_eq!(value, json!({"audit_retention_days": null}));
        assert_eq!(
            serde_json::from_value::<LimitsClaim>(value).unwrap(),
            retention
        );
        let kept = LimitsClaim {
            audit_retention_days: Some(Some(0)),
            ..LimitsClaim::default()
        };
        let value = serde_json::to_value(&kept).unwrap();
        assert_eq!(value, json!({"audit_retention_days": 0}));
        assert_eq!(
            serde_json::from_value::<LimitsClaim>(value).unwrap(),
            kept
        );
    }

    #[test]
    fn sso_domains_round_trip_and_default_to_empty() {
        let base = |sso_domains: Vec<String>| OrgClaims {
            id: Uuid::parse_str("01900000-0000-7000-8000-000000000521").unwrap(),
            issued_at: ns("2026-01-01T00:00:00Z"),
            expires_at: ns("2099-01-01T00:00:00Z"),
            grace_ends_at: ns("2099-02-01T00:00:00Z"),
            audience: "http://localhost:8080".to_owned(),
            org_id: Uuid::parse_str("01900000-0000-7000-8000-000000000010").unwrap(),
            plan: PlanClaim {
                id: "pro".to_owned(),
                name: "Pro".to_owned(),
                add_ons: Vec::new(),
            },
            features: Vec::new(),
            limits: LimitsClaim::default(),
            sso_domains,
        };

        // Empty domains are not written, so an org token without SSO domains keeps its old bytes.
        let empty = serde_json::to_value(base(Vec::new())).unwrap();
        assert!(empty.get("sso_domains").is_none(), "{empty}");

        // A token from before the claim existed parses with no domains.
        let parsed: OrgClaims = serde_json::from_value(empty).unwrap();
        assert!(parsed.sso_domains.is_empty());

        // Domains round-trip through a signed token.
        let claims = Claims::Org(base(vec![
            "acme.test".to_owned(),
            "acme.example".to_owned(),
        ]));
        let token = sign(&dev_key(), "dev-1", &claims).unwrap();
        assert_eq!(verify(&token).unwrap().claims, claims);
        let payload = token.split('.').nth(2).unwrap();
        let json = String::from_utf8(URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert!(
            json.contains(r#""sso_domains":["acme.test","acme.example"]"#),
            "{json}"
        );
    }

    #[test]
    fn rfc3339_accepts_offsets_and_writes_nanos() {
        #[derive(Debug, Serialize, Deserialize)]
        struct Wrap {
            #[serde(with = "rfc3339")]
            at: i64,
        }

        let wrap: Wrap = serde_json::from_str(r#"{"at":"2026-01-01T01:00:00+01:00"}"#).unwrap();
        assert_eq!(wrap.at, ns("2026-01-01T00:00:00Z"));

        let text = serde_json::to_string(&Wrap { at: wrap.at }).unwrap();
        assert_eq!(text, r#"{"at":"2026-01-01T00:00:00.000000000Z"}"#);

        let error = serde_json::from_str::<Wrap>(r#"{"at":"2026-01-01"}"#).unwrap_err();
        assert!(!error.to_string().contains("2026"), "{error}");
    }
}
