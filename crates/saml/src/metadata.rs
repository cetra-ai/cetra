// SPDX-License-Identifier: Elastic-2.0
//! IdP metadata: the entity ID, the HTTP-Redirect single sign-on URL and 1 to 4 signing
//! certificates (D-SA-11). `validUntil`, `cacheDuration` and the other elements are ignored.

use base64::Engine as _;

use crate::cert::IdpCertificate;
use crate::xml::{
    BINDING_REDIRECT, NS_DSIG, NS_METADATA, NS_PROTOCOL, ParseFailure, attr, children_named,
    descendants_named, is_element, parse_strict, strict_text,
};

/// The largest metadata document accepted, in bytes.
pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
/// The most signing certificates kept (D-SA-11).
pub const MAX_CERTIFICATES: usize = 4;

/// The most signing `KeyDescriptor` elements read before the metadata is refused.
const MAX_SIGNING_KEY_DESCRIPTORS: usize = 16;
/// The longest entity ID accepted, in characters.
const MAX_ENTITY_ID_CHARS: usize = 1024;
/// The longest single sign-on URL accepted, in bytes.
const MAX_SSO_URL_BYTES: usize = 2048;

/// Why IdP metadata or a certificate was refused. Messages carry no payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MetadataError {
    #[error("the metadata is larger than 1 MiB")]
    TooLarge,
    #[error("the metadata is not UTF-8")]
    Utf8,
    #[error("the metadata contains a DTD or an entity declaration")]
    Doctype,
    #[error("the metadata is not well-formed XML or exceeds the parse limits")]
    Xml,
    #[error("the metadata's root is not one SAML 2.0 EntityDescriptor")]
    NotEntityDescriptor,
    #[error(
        "the entity ID is missing, empty, longer than 1024 characters or contains control characters"
    )]
    EntityId,
    #[error("the metadata has no SAML 2.0 IDPSSODescriptor, or more than one")]
    NoIdpDescriptor,
    #[error("the identity provider has no single sign-on service for the HTTP-Redirect binding")]
    NoRedirectSso,
    #[error("the single sign-on URL must be an https URL without user information or fragment")]
    SsoUrl,
    #[error("the identity provider has no signing certificate")]
    NoSigningCertificate,
    #[error("the identity provider lists more than 4 signing certificates")]
    TooManyCertificates,
    #[error("a certificate could not be read")]
    Certificate,
    #[error("a certificate's key is not RSA of at least 2048 bits or ECDSA P-256 or P-384")]
    WeakKey,
}

/// What cetra keeps from IdP metadata (D-SA-11).
#[derive(Clone, PartialEq, Eq)]
pub struct IdpMetadata {
    entity_id: String,
    sso_url: String,
    certificates: Vec<IdpCertificate>,
}

impl IdpMetadata {
    /// Builds metadata from manual fields, with the same checks as parsing.
    pub fn from_fields(
        entity_id: &str,
        sso_url: &str,
        certificates: Vec<IdpCertificate>,
    ) -> Result<Self, MetadataError> {
        check_entity_id(entity_id)?;
        check_sso_url(sso_url)?;
        let certificates = dedup_certificates(certificates)?;
        Ok(Self {
            entity_id: entity_id.to_owned(),
            sso_url: sso_url.to_owned(),
            certificates,
        })
    }

    /// The IdP entity ID (the `Issuer` of its responses).
    pub fn entity_id(&self) -> &str {
        &self.entity_id
    }

    /// The single sign-on URL for the HTTP-Redirect binding.
    pub fn sso_url(&self) -> &str {
        &self.sso_url
    }

    /// The trusted signing certificates, 1 to 4, distinct.
    pub fn certificates(&self) -> &[IdpCertificate] {
        &self.certificates
    }
}

/// `Debug` prints lengths and certificate hashes only.
impl std::fmt::Debug for IdpMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdpMetadata")
            .field("entity_id_len", &self.entity_id.len())
            .field("sso_url_len", &self.sso_url.len())
            .field("certificates", &self.certificates)
            .finish()
    }
}

/// Parses IdP metadata XML (D-SA-11).
pub fn parse_metadata(xml: &[u8]) -> Result<IdpMetadata, MetadataError> {
    if xml.len() > MAX_METADATA_BYTES {
        return Err(MetadataError::TooLarge);
    }
    let text = std::str::from_utf8(xml).map_err(|_| MetadataError::Utf8)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let doc = parse_strict(text).map_err(|failure| match failure {
        ParseFailure::Doctype => MetadataError::Doctype,
        ParseFailure::Xml => MetadataError::Xml,
    })?;

    let root = doc
        .document_element()
        .ok_or(MetadataError::NotEntityDescriptor)?;
    if !is_element(&doc, root, NS_METADATA, "EntityDescriptor") {
        return Err(MetadataError::NotEntityDescriptor);
    }
    let entity_id = attr(&doc, root, "entityID").ok_or(MetadataError::EntityId)?;
    check_entity_id(entity_id)?;

    // Exactly one IDPSSODescriptor that supports SAML 2.0 protocol.
    let mut idp_descriptors = children_named(&doc, root, NS_METADATA, "IDPSSODescriptor")
        .into_iter()
        .filter(|&descriptor| {
            attr(&doc, descriptor, "protocolSupportEnumeration").is_some_and(|protocols| {
                protocols
                    .split_ascii_whitespace()
                    .any(|protocol| protocol == NS_PROTOCOL)
            })
        });
    let descriptor = match (idp_descriptors.next(), idp_descriptors.next()) {
        (Some(descriptor), None) => descriptor,
        _ => return Err(MetadataError::NoIdpDescriptor),
    };

    let sso = children_named(&doc, descriptor, NS_METADATA, "SingleSignOnService")
        .into_iter()
        .find(|&service| attr(&doc, service, "Binding") == Some(BINDING_REDIRECT))
        .ok_or(MetadataError::NoRedirectSso)?;
    let sso_url = attr(&doc, sso, "Location").ok_or(MetadataError::SsoUrl)?;
    check_sso_url(sso_url)?;

    let mut certificates = Vec::new();
    let mut signing_descriptors = 0usize;
    for key_descriptor in children_named(&doc, descriptor, NS_METADATA, "KeyDescriptor") {
        if !matches!(attr(&doc, key_descriptor, "use"), None | Some("signing")) {
            continue;
        }
        signing_descriptors += 1;
        if signing_descriptors > MAX_SIGNING_KEY_DESCRIPTORS {
            return Err(MetadataError::TooManyCertificates);
        }
        for certificate in descendants_named(&doc, key_descriptor, NS_DSIG, "X509Certificate") {
            let encoded = strict_text(&doc, certificate).ok_or(MetadataError::Certificate)?;
            let compact: String = encoded
                .chars()
                .filter(|c| !c.is_ascii_whitespace())
                .collect();
            let der = base64::engine::general_purpose::STANDARD
                .decode(compact)
                .map_err(|_| MetadataError::Certificate)?;
            certificates.push(IdpCertificate::from_der(&der)?);
        }
    }

    Ok(IdpMetadata {
        entity_id: entity_id.to_owned(),
        sso_url: sso_url.to_owned(),
        certificates: dedup_certificates(certificates)?,
    })
}

/// The entity ID is non-empty, at most 1024 characters, with no control characters.
fn check_entity_id(value: &str) -> Result<(), MetadataError> {
    if value.is_empty()
        || value.chars().count() > MAX_ENTITY_ID_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(MetadataError::EntityId);
    }
    Ok(())
}

/// The single sign-on URL is an `https` URL with no user information and no fragment.
fn check_sso_url(value: &str) -> Result<(), MetadataError> {
    if value.len() > MAX_SSO_URL_BYTES {
        return Err(MetadataError::SsoUrl);
    }
    let url = url::Url::parse(value).map_err(|_| MetadataError::SsoUrl)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(MetadataError::SsoUrl);
    }
    Ok(())
}

/// Keeps the first of each distinct certificate, in order. Refuses an empty list and more than
/// `MAX_CERTIFICATES` distinct certificates (D-SA-11). Stops at the first certificate over the
/// limit, which gives the same answer as collecting them all first.
fn dedup_certificates(
    certificates: Vec<IdpCertificate>,
) -> Result<Vec<IdpCertificate>, MetadataError> {
    let mut kept: Vec<IdpCertificate> = Vec::new();
    for certificate in certificates {
        if kept.contains(&certificate) {
            continue;
        }
        kept.push(certificate);
        if kept.len() > MAX_CERTIFICATES {
            return Err(MetadataError::TooManyCertificates);
        }
    }
    if kept.is_empty() {
        return Err(MetadataError::NoSigningCertificate);
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDP_RSA: &str = include_str!("../../../fixtures/saml/idp-rsa.cert.pem");
    const IDP_RSA_NEXT: &str = include_str!("../../../fixtures/saml/idp-rsa-next.cert.pem");
    const IDP_EC: &str = include_str!("../../../fixtures/saml/idp-ec.cert.pem");
    const IDP_EC384: &str = include_str!("../../../fixtures/saml/idp-ec384.cert.pem");
    const ATTACKER_RSA: &str = include_str!("../../../fixtures/saml/attacker-rsa.cert.pem");

    const ENTITY: &str = "https://idp.test/entity";
    const SSO: &str = "https://idp.test/sso";

    /// The base64 body of a PEM block, without its markers and line breaks.
    fn pem_body(pem: &str) -> String {
        pem.lines()
            .filter(|line| !line.starts_with("-----"))
            .collect()
    }

    /// A signing `KeyDescriptor` for one base64 certificate body.
    fn signing_key(body: &str) -> String {
        format!(
            "<md:KeyDescriptor use=\"signing\"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{body}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>"
        )
    }

    /// Metadata with one signing `KeyDescriptor` per certificate body, a POST and a Redirect
    /// single sign-on service, and `extra` inside the IDPSSODescriptor.
    fn metadata_xml(entity_id: &str, sso: &str, bodies: &[&str], extra: &str) -> String {
        let keys: String = bodies.iter().map(|body| signing_key(body)).collect();
        format!(
            "<md:EntityDescriptor xmlns:md=\"urn:oasis:names:tc:SAML:2.0:metadata\" xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" entityID=\"{entity_id}\"><md:IDPSSODescriptor protocolSupportEnumeration=\"urn:oasis:names:tc:SAML:2.0:protocol\">{keys}<md:SingleSignOnService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\" Location=\"https://idp.test/post\"/><md:SingleSignOnService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect\" Location=\"{sso}\"/>{extra}</md:IDPSSODescriptor></md:EntityDescriptor>"
        )
    }

    fn parse_ok(xml: &str) -> IdpMetadata {
        parse_metadata(xml.as_bytes()).expect("metadata is valid")
    }

    #[test]
    fn okta_shaped_metadata_parses() {
        let metadata = parse_ok(&metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], ""));
        assert_eq!(metadata.entity_id(), ENTITY);
        assert_eq!(metadata.sso_url(), SSO);
        assert_eq!(metadata.certificates().len(), 1);
        let expected = IdpCertificate::from_pem(IDP_RSA).expect("fixture is valid");
        assert_eq!(metadata.certificates()[0], expected);
    }

    #[test]
    fn two_certificates_are_kept_for_rotation() {
        let metadata = parse_ok(&metadata_xml(
            ENTITY,
            SSO,
            &[&pem_body(IDP_RSA), &pem_body(IDP_RSA_NEXT)],
            "",
        ));
        let certificates = metadata.certificates();
        assert_eq!(certificates.len(), 2);
        let first = IdpCertificate::from_pem(IDP_RSA).expect("fixture is valid");
        assert_eq!(certificates[0], first);
    }

    #[test]
    fn a_duplicate_certificate_is_kept_once() {
        let body = pem_body(IDP_RSA);
        let metadata = parse_ok(&metadata_xml(ENTITY, SSO, &[&body, &body], ""));
        assert_eq!(metadata.certificates().len(), 1);
    }

    #[test]
    fn an_encryption_only_key_is_ignored() {
        let encryption = format!(
            "<md:KeyDescriptor use=\"encryption\"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>",
            pem_body(ATTACKER_RSA)
        );
        let metadata = parse_ok(&metadata_xml(
            ENTITY,
            SSO,
            &[&pem_body(IDP_RSA)],
            &encryption,
        ));
        assert_eq!(metadata.certificates().len(), 1);
        let expected = IdpCertificate::from_pem(IDP_RSA).expect("fixture is valid");
        assert_eq!(metadata.certificates()[0], expected);
    }

    #[test]
    fn five_distinct_certificates_are_too_many() {
        let bodies = [
            pem_body(IDP_RSA),
            pem_body(IDP_RSA_NEXT),
            pem_body(IDP_EC),
            pem_body(IDP_EC384),
            pem_body(ATTACKER_RSA),
        ];
        let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
        assert_eq!(
            parse_metadata(metadata_xml(ENTITY, SSO, &refs, "").as_bytes()).err(),
            Some(MetadataError::TooManyCertificates)
        );
    }

    #[test]
    fn seventeen_key_descriptors_are_too_many() {
        let body = pem_body(IDP_RSA);
        let bodies = vec![body.as_str(); 17];
        assert_eq!(
            parse_metadata(metadata_xml(ENTITY, SSO, &bodies, "").as_bytes()).err(),
            Some(MetadataError::TooManyCertificates)
        );
    }

    #[test]
    fn a_p384_certificate_is_kept() {
        let metadata = parse_ok(&metadata_xml(ENTITY, SSO, &[&pem_body(IDP_EC384)], ""));
        assert_eq!(metadata.certificates().len(), 1);
    }

    #[test]
    fn no_redirect_binding_is_refused() {
        let xml = metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], "")
            .replace("HTTP-Redirect", "HTTP-Artifact");
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::NoRedirectSso)
        );
    }

    #[test]
    fn an_http_sso_url_is_refused() {
        let xml = metadata_xml(ENTITY, "http://idp.test/sso", &[&pem_body(IDP_RSA)], "");
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::SsoUrl)
        );
    }

    #[test]
    fn a_sso_url_with_user_information_is_refused() {
        let xml = metadata_xml(
            ENTITY,
            "https://u:p@idp.test/sso",
            &[&pem_body(IDP_RSA)],
            "",
        );
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::SsoUrl)
        );
    }

    #[test]
    fn a_sso_url_with_a_fragment_is_refused() {
        let xml = metadata_xml(ENTITY, "https://idp.test/sso#x", &[&pem_body(IDP_RSA)], "");
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::SsoUrl)
        );
    }

    #[test]
    fn an_entities_descriptor_is_refused() {
        let xml = metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], "")
            .replace("EntityDescriptor", "EntitiesDescriptor");
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::NotEntityDescriptor)
        );
    }

    #[test]
    fn two_idp_descriptors_are_refused() {
        let second = "<md:IDPSSODescriptor protocolSupportEnumeration=\"urn:oasis:names:tc:SAML:2.0:protocol\"/>";
        let xml = metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], "").replace(
            "</md:EntityDescriptor>",
            &format!("{second}</md:EntityDescriptor>"),
        );
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::NoIdpDescriptor)
        );
    }

    #[test]
    fn a_saml1_only_descriptor_is_ignored() {
        let xml = metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], "").replace(
            "urn:oasis:names:tc:SAML:2.0:protocol",
            "urn:oasis:names:tc:SAML:1.1:protocol",
        );
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::NoIdpDescriptor)
        );
    }

    #[test]
    fn no_signing_certificate_is_refused() {
        let xml = metadata_xml(ENTITY, SSO, &[], "");
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::NoSigningCertificate)
        );
    }

    #[test]
    fn a_doctype_is_refused() {
        let xml = format!(
            "<!DOCTYPE md:EntityDescriptor>{}",
            metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], "")
        );
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::Doctype)
        );
    }

    #[test]
    fn oversized_metadata_is_refused() {
        let xml = " ".repeat(MAX_METADATA_BYTES + 1);
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::TooLarge)
        );
    }

    #[test]
    fn a_bom_is_accepted() {
        let xml = format!(
            "\u{feff}{}",
            metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], "")
        );
        assert!(parse_metadata(xml.as_bytes()).is_ok());
    }

    #[test]
    fn an_empty_entity_id_is_refused() {
        let xml = metadata_xml("", SSO, &[&pem_body(IDP_RSA)], "");
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::EntityId)
        );
    }

    #[test]
    fn an_entity_id_with_a_control_character_is_refused() {
        // A literal newline in an attribute is normalized to a space by XML, so the control
        // character here is DEL, which XML allows.
        let xml = metadata_xml(
            "https://idp.test/\u{7f}entity",
            SSO,
            &[&pem_body(IDP_RSA)],
            "",
        );
        assert_eq!(
            parse_metadata(xml.as_bytes()).err(),
            Some(MetadataError::EntityId)
        );
    }

    #[test]
    fn from_fields_applies_the_same_checks() {
        let cert = IdpCertificate::from_pem(IDP_RSA).expect("fixture is valid");
        assert_eq!(
            IdpMetadata::from_fields(ENTITY, "http://idp.test/sso", vec![cert.clone()]).err(),
            Some(MetadataError::SsoUrl)
        );
        assert_eq!(
            IdpMetadata::from_fields(ENTITY, SSO, vec![]).err(),
            Some(MetadataError::NoSigningCertificate)
        );
        assert_eq!(
            IdpMetadata::from_fields(ENTITY, SSO, vec![cert.clone(), cert.clone(), cert.clone()])
                .map(|metadata| metadata.certificates().len()),
            Ok(1)
        );
        assert_eq!(
            IdpMetadata::from_fields("a\nb", SSO, vec![cert]).err(),
            Some(MetadataError::EntityId)
        );
        let five: Vec<IdpCertificate> = [IDP_RSA, IDP_RSA_NEXT, IDP_EC, IDP_EC384, ATTACKER_RSA]
            .iter()
            .map(|pem| IdpCertificate::from_pem(pem).expect("fixture is valid"))
            .collect();
        assert_eq!(
            IdpMetadata::from_fields(ENTITY, SSO, five).err(),
            Some(MetadataError::TooManyCertificates)
        );
    }

    #[test]
    fn debug_hides_urls_and_the_entity_id() {
        let metadata = parse_ok(&metadata_xml(ENTITY, SSO, &[&pem_body(IDP_RSA)], ""));
        let debug = format!("{metadata:?}");
        assert!(!debug.contains(ENTITY));
        assert!(!debug.contains(SSO));
        assert!(debug.contains("entity_id_len"));
    }
}
