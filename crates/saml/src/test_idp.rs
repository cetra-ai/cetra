// SPDX-License-Identifier: Elastic-2.0
//! A test identity provider that signs SAML Responses with the `fixtures/saml` keys
//! (track SA, D-SA-15). Test support only: never compiled into release builds.
#![allow(clippy::expect_used, clippy::unwrap_used)] // test support, never built in release

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bergshamra::{DsigContext, KeysManager};

use crate::xml::{
    BINDING_POST, BINDING_REDIRECT, C14N_EXCLUSIVE, CM_BEARER, DIGEST_SHA256, NAMEID_EMAIL,
    NS_ASSERTION, NS_DSIG, NS_METADATA, NS_PROTOCOL, SIG_ECDSA_SHA256, SIG_RSA_SHA256,
    STATUS_SUCCESS, TRANSFORM_ENVELOPED, escape_attr, escape_text,
};

pub const SP_ENTITY_ID: &str =
    "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001";
pub const ACS_URL: &str =
    "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs";
pub const IDP_ENTITY_ID: &str = "https://idp.test/metadata";
pub const IDP_SSO_URL: &str = "https://idp.test/sso";
pub const REQUEST_ID: &str = "_0123456789abcdef0123456789abcdef";
pub const RESPONSE_ID: &str = "_r0000000000000000000000000000001";
pub const ASSERTION_ID: &str = "_a0000000000000000000000000000001";
/// 2026-01-01T00:00:00Z.
pub const NOW_NS: i64 = 1_767_225_600_000_000_000;

/// The seconds-to-nanoseconds factor for the validity windows below.
const SECOND_NS: i64 = 1_000_000_000;
/// The authentication context of every test Response.
const AUTHN_CONTEXT: &str = "urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport";
/// Where the placeholder for the Response's signature goes, after its `Issuer`.
const RESPONSE_SLOT: &str = "<!--RS-->";
/// Where the placeholder for the Assertion's signature goes, after its `Issuer`.
const ASSERTION_SLOT: &str = "<!--AS-->";
/// The start of the Response's `Status`, which follows the Response's signature slot.
const STATUS_START: &str = "<samlp:Status";

/// The keys the test IdP signs with. Each is a PEM pair in `fixtures/saml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKey {
    Idp,
    IdpNext,
    IdpEc,
    Attacker,
}

impl TestKey {
    /// The PKCS#8 or PKCS#1 private key, PEM.
    pub fn key_pem(self) -> &'static str {
        match self {
            Self::Idp => include_str!("../../../fixtures/saml/idp-rsa.key.pem"),
            Self::IdpNext => include_str!("../../../fixtures/saml/idp-rsa-next.key.pem"),
            Self::IdpEc => include_str!("../../../fixtures/saml/idp-ec.key.pem"),
            Self::Attacker => include_str!("../../../fixtures/saml/attacker-rsa.key.pem"),
        }
    }

    /// The certificate of the key, PEM.
    pub fn cert_pem(self) -> &'static str {
        match self {
            Self::Idp => include_str!("../../../fixtures/saml/idp-rsa.cert.pem"),
            Self::IdpNext => include_str!("../../../fixtures/saml/idp-rsa-next.cert.pem"),
            Self::IdpEc => include_str!("../../../fixtures/saml/idp-ec.cert.pem"),
            Self::Attacker => include_str!("../../../fixtures/saml/attacker-rsa.cert.pem"),
        }
    }

    /// The certificate as the crate reads it.
    pub fn certificate(self) -> crate::IdpCertificate {
        crate::IdpCertificate::from_pem(self.cert_pem()).expect("the fixture certificate is valid")
    }

    /// The XML-DSig signature method for this key.
    pub fn signature_method(self) -> &'static str {
        match self {
            Self::IdpEc => SIG_ECDSA_SHA256,
            _ => SIG_RSA_SHA256,
        }
    }
}

/// Which elements of a test Response are signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signing {
    Assertion,
    Response,
    Both,
    None,
}

/// Every knob of a test Response. `Default` is a valid Response for the constants above.
#[derive(Debug, Clone)]
pub struct ResponseOptions {
    pub response_in_response_to: Option<String>,
    pub confirmation_in_response_to: Option<String>,
    pub destination: Option<String>,
    pub recipient: String,
    pub audiences: Vec<String>,
    pub conditions: bool,
    pub response_issuer: Option<String>,
    pub assertion_issuer: String,
    pub name_id: String,
    pub name_id_format: Option<String>,
    pub attributes: Vec<(String, Vec<String>)>,
    pub issue_instant_ns: i64,
    pub not_before_ns: i64,
    pub not_on_or_after_ns: i64,
    pub confirmation_not_on_or_after_ns: i64,
    pub status: String,
    pub version: String,
    pub signing: Signing,
    pub key: TestKey,
    pub key_info: bool,
}

impl Default for ResponseOptions {
    fn default() -> Self {
        Self {
            response_in_response_to: Some(REQUEST_ID.to_owned()),
            confirmation_in_response_to: Some(REQUEST_ID.to_owned()),
            destination: Some(ACS_URL.to_owned()),
            recipient: ACS_URL.to_owned(),
            audiences: vec![SP_ENTITY_ID.to_owned()],
            conditions: true,
            response_issuer: Some(IDP_ENTITY_ID.to_owned()),
            assertion_issuer: IDP_ENTITY_ID.to_owned(),
            name_id: "ada@example.com".to_owned(),
            name_id_format: Some(NAMEID_EMAIL.to_owned()),
            attributes: vec![
                ("email".to_owned(), vec!["ada@example.com".to_owned()]),
                ("displayName".to_owned(), vec!["Ada Lovelace".to_owned()]),
            ],
            issue_instant_ns: NOW_NS,
            not_before_ns: NOW_NS - 60 * SECOND_NS,
            not_on_or_after_ns: NOW_NS + 300 * SECOND_NS,
            confirmation_not_on_or_after_ns: NOW_NS + 300 * SECOND_NS,
            status: STATUS_SUCCESS.to_owned(),
            version: "2.0".to_owned(),
            signing: Signing::Assertion,
            key: TestKey::Idp,
            key_info: true,
        }
    }
}

/// Builds a Response for `options` and signs it as `options.signing` says.
pub fn response_xml(options: &ResponseOptions) -> String {
    let unsigned = unsigned_response(options);
    let assertion_template = signature_template(ASSERTION_ID, options);
    let response_template = signature_template(RESPONSE_ID, options);
    match options.signing {
        Signing::None => unsigned
            .replace(RESPONSE_SLOT, "")
            .replace(ASSERTION_SLOT, ""),
        Signing::Response => sign_once(
            options.key,
            &unsigned
                .replace(RESPONSE_SLOT, &response_template)
                .replace(ASSERTION_SLOT, ""),
        ),
        Signing::Assertion => sign_once(
            options.key,
            &unsigned
                .replace(RESPONSE_SLOT, "")
                .replace(ASSERTION_SLOT, &assertion_template),
        ),
        Signing::Both => {
            // The first pass signs only the Assertion: the Response's slot is still empty, so
            // the Assertion's Signature is the document's first.
            let signed_assertion = sign_once(
                options.key,
                &unsigned
                    .replace(RESPONSE_SLOT, "")
                    .replace(ASSERTION_SLOT, &assertion_template),
            );
            // The Response's slot sits right before `<samlp:Status`. Insert the Response
            // template there and sign again: bergshamra signs the first Signature in document
            // order, which is now the Response's, and leaves the Assertion's filled values alone.
            let at = signed_assertion
                .find(STATUS_START)
                .expect("the unsigned Response has a Status element");
            let mut with_response = signed_assertion;
            with_response.insert_str(at, &response_template);
            sign_once(options.key, &with_response)
        }
    }
}

/// `response_xml` as base64, the form an HTTP-POST binding carries.
pub fn response_b64(options: &ResponseOptions) -> String {
    STANDARD.encode(response_xml(options))
}

/// IdP metadata for `IDP_ENTITY_ID` and `IDP_SSO_URL`, with one signing `KeyDescriptor` per key.
pub fn metadata_xml(certs: &[TestKey]) -> String {
    let keys: String = certs
        .iter()
        .map(|key| {
            format!(
                "<md:KeyDescriptor use=\"signing\"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>",
                STANDARD.encode(key.certificate().der())
            )
        })
        .collect();
    format!(
        "<md:EntityDescriptor xmlns:md=\"{NS_METADATA}\" xmlns:ds=\"{NS_DSIG}\" entityID=\"{IDP_ENTITY_ID}\"><md:IDPSSODescriptor protocolSupportEnumeration=\"{NS_PROTOCOL}\">{keys}<md:SingleSignOnService Binding=\"{BINDING_POST}\" Location=\"https://idp.test/post\"/><md:SingleSignOnService Binding=\"{BINDING_REDIRECT}\" Location=\"{IDP_SSO_URL}\"/></md:IDPSSODescriptor></md:EntityDescriptor>"
    )
}

/// The parsed metadata of the test IdP with its `Idp` certificate.
pub fn idp_metadata() -> crate::IdpMetadata {
    crate::parse_metadata(metadata_xml(&[TestKey::Idp]).as_bytes()).unwrap()
}

/// The Response with both slots marked and nothing signed.
fn unsigned_response(options: &ResponseOptions) -> String {
    let issued = instant_text(options.issue_instant_ns);
    let response_issuer = options
        .response_issuer
        .as_deref()
        .map(|issuer| format!("<saml:Issuer>{}</saml:Issuer>", escape_text(issuer)))
        .unwrap_or_default();
    let destination = optional_attr("Destination", options.destination.as_deref());
    let in_response_to = optional_attr("InResponseTo", options.response_in_response_to.as_deref());
    let name_id_format = optional_attr("Format", options.name_id_format.as_deref());
    let confirmation_in_response_to = optional_attr(
        "InResponseTo",
        options.confirmation_in_response_to.as_deref(),
    );
    let confirmation_data = format!(
        "<saml:SubjectConfirmationData NotOnOrAfter=\"{}\" Recipient=\"{}\"{confirmation_in_response_to}/>",
        instant_text(options.confirmation_not_on_or_after_ns),
        escape_attr(&options.recipient),
    );
    let conditions = if options.conditions {
        let audiences: String = if options.audiences.is_empty() {
            String::new()
        } else {
            let audiences: String = options
                .audiences
                .iter()
                .map(|audience| format!("<saml:Audience>{}</saml:Audience>", escape_text(audience)))
                .collect();
            format!("<saml:AudienceRestriction>{audiences}</saml:AudienceRestriction>")
        };
        format!(
            "<saml:Conditions NotBefore=\"{}\" NotOnOrAfter=\"{}\">{audiences}</saml:Conditions>",
            instant_text(options.not_before_ns),
            instant_text(options.not_on_or_after_ns),
        )
    } else {
        String::new()
    };
    let attributes: String = options
        .attributes
        .iter()
        .map(|(name, values)| {
            let values: String = values
                .iter()
                .map(|value| {
                    format!(
                        "<saml:AttributeValue>{}</saml:AttributeValue>",
                        escape_text(value)
                    )
                })
                .collect();
            format!(
                "<saml:Attribute Name=\"{}\">{values}</saml:Attribute>",
                escape_attr(name)
            )
        })
        .collect();

    format!(
        "<samlp:Response xmlns:samlp=\"{NS_PROTOCOL}\" xmlns:saml=\"{NS_ASSERTION}\" ID=\"{RESPONSE_ID}\" Version=\"{version}\" IssueInstant=\"{issued}\"{destination}{in_response_to}>{response_issuer}{RESPONSE_SLOT}<samlp:Status><samlp:StatusCode Value=\"{status}\"/></samlp:Status><saml:Assertion ID=\"{ASSERTION_ID}\" Version=\"{version}\" IssueInstant=\"{issued}\"><saml:Issuer>{assertion_issuer}</saml:Issuer>{ASSERTION_SLOT}<saml:Subject><saml:NameID{name_id_format}>{name_id}</saml:NameID><saml:SubjectConfirmation Method=\"{CM_BEARER}\">{confirmation_data}</saml:SubjectConfirmation></saml:Subject>{conditions}<saml:AuthnStatement AuthnInstant=\"{issued}\" SessionIndex=\"_s1\"><saml:AuthnContext><saml:AuthnContextClassRef>{AUTHN_CONTEXT}</saml:AuthnContextClassRef></saml:AuthnContext></saml:AuthnStatement><saml:AttributeStatement>{attributes}</saml:AttributeStatement></saml:Assertion></samlp:Response>",
        version = escape_attr(&options.version),
        status = escape_attr(&options.status),
        assertion_issuer = escape_text(&options.assertion_issuer),
        name_id = escape_text(&options.name_id),
    )
}

/// An XML-DSig `Signature` template for the element `id`, with empty values to fill (D-SA-15).
fn signature_template(id: &str, options: &ResponseOptions) -> String {
    let key_info = if options.key_info {
        format!(
            "<ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo>",
            STANDARD.encode(options.key.certificate().der())
        )
    } else {
        String::new()
    };
    format!(
        "<ds:Signature xmlns:ds=\"{NS_DSIG}\"><ds:SignedInfo><ds:CanonicalizationMethod Algorithm=\"{C14N_EXCLUSIVE}\"/><ds:SignatureMethod Algorithm=\"{method}\"/><ds:Reference URI=\"#{id}\"><ds:Transforms><ds:Transform Algorithm=\"{TRANSFORM_ENVELOPED}\"/><ds:Transform Algorithm=\"{C14N_EXCLUSIVE}\"/></ds:Transforms><ds:DigestMethod Algorithm=\"{DIGEST_SHA256}\"/><ds:DigestValue></ds:DigestValue></ds:Reference></ds:SignedInfo><ds:SignatureValue></ds:SignatureValue>{key_info}</ds:Signature>",
        method = options.key.signature_method(),
    )
}

/// Signs the first empty `Signature` of `xml` with `key`.
fn sign_once(key: TestKey, xml: &str) -> String {
    let mut keys = KeysManager::new();
    keys.add_key(
        bergshamra::keys::loader::load_pem_auto(key.key_pem().as_bytes(), None)
            .expect("the fixture private key loads"),
    );
    let ctx = DsigContext::new_permissive(keys);
    bergshamra::dsig::sign::sign(&ctx, xml).expect("the test template signs")
}

/// ` name="value"` when `value` is present, otherwise nothing.
fn optional_attr(name: &str, value: Option<&str>) -> String {
    value
        .map(|value| format!(" {name}=\"{}\"", escape_attr(value)))
        .unwrap_or_default()
}

/// An instant as `%Y-%m-%dT%H:%M:%SZ` (as in SA1.3).
fn instant_text(ns: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(ns)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bergshamra::dsig::verify::{VerifyResult, verify_all};

    /// A verification context that trusts only `key`'s certificate.
    fn trusting(key: TestKey) -> DsigContext {
        let mut keys = KeysManager::new();
        keys.add_key(key.certificate().key().expect("the certificate loads"));
        DsigContext::new(keys)
    }

    /// The URIs of the references of every signature, after checking each is `Valid`.
    fn valid_uris(options: &ResponseOptions) -> Vec<String> {
        let xml = response_xml(options);
        let results =
            verify_all(&trusting(options.key), &xml).expect("the Response is well formed");
        results
            .iter()
            .flat_map(|result| match result {
                VerifyResult::Valid { references, .. } => references
                    .iter()
                    .map(|reference| reference.uri.clone())
                    .collect::<Vec<_>>(),
                VerifyResult::Invalid { reason } => {
                    panic!("a signature is invalid: {reason}")
                }
            })
            .collect()
    }

    #[test]
    fn a_default_response_verifies_with_bergshamra() {
        assert_eq!(
            valid_uris(&ResponseOptions::default()),
            vec![format!("#{ASSERTION_ID}")]
        );
    }

    #[test]
    fn both_signatures_verify() {
        let options = ResponseOptions {
            signing: Signing::Both,
            ..ResponseOptions::default()
        };
        let uris = valid_uris(&options);
        assert_eq!(uris.len(), 2);
        assert!(uris.contains(&format!("#{RESPONSE_ID}")));
        assert!(uris.contains(&format!("#{ASSERTION_ID}")));
    }

    #[test]
    fn the_ec_key_signs() {
        let options = ResponseOptions {
            key: TestKey::IdpEc,
            signing: Signing::Both,
            ..ResponseOptions::default()
        };
        assert_eq!(valid_uris(&options).len(), 2);
    }

    #[test]
    fn no_marker_is_left() {
        for signing in [
            Signing::Assertion,
            Signing::Response,
            Signing::Both,
            Signing::None,
        ] {
            let options = ResponseOptions {
                signing,
                ..ResponseOptions::default()
            };
            let xml = response_xml(&options);
            assert!(!xml.contains(RESPONSE_SLOT), "{signing:?} left a marker");
            assert!(!xml.contains(ASSERTION_SLOT), "{signing:?} left a marker");
        }
    }

    #[test]
    fn the_metadata_parses() {
        let metadata = idp_metadata();
        assert_eq!(metadata.entity_id(), IDP_ENTITY_ID);
        assert_eq!(metadata.sso_url(), IDP_SSO_URL);
        assert_eq!(metadata.certificates().len(), 1);
    }
}
