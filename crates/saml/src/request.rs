// SPDX-License-Identifier: Elastic-2.0
//! SP-initiated AuthnRequests and SP metadata (D-SA-8, D-SA-12).

use std::io::Write;

use base64::Engine;
use url::Url;

use crate::xml::{self, BINDING_POST, NAMEID_UNSPECIFIED, NS_ASSERTION, NS_METADATA, NS_PROTOCOL};

/// Why an AuthnRequest could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("the request ID must be `_` followed by 32 lowercase hex digits")]
    RequestId,
    #[error("a URL or entity ID is not usable")]
    Url,
    #[error("the request could not be encoded")]
    Encode,
}

/// The inputs of one SP-initiated login (D-SA-12). The caller makes `request_id` from 16 random
/// bytes as `_` + lowercase hex, and remembers it with its pending entry (D-SA-10).
#[derive(Clone, Copy)]
pub struct AuthnRequest<'a> {
    pub sp_entity_id: &'a str,
    pub acs_url: &'a str,
    pub sso_url: &'a str,
    pub request_id: &'a str,
    pub relay_state: &'a str,
    pub issue_instant_ns: i64,
}

impl std::fmt::Debug for AuthnRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthnRequest")
            .field("request_id_len", &self.request_id.len())
            .field("sso_url_len", &self.sso_url.len())
            .finish()
    }
}

/// Whether `id` is `_` followed by exactly 32 lowercase hex digits.
pub fn is_valid_request_id(id: &str) -> bool {
    id.len() == 33
        && id.starts_with('_')
        && id[1..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// The IdP URL that starts the login: `sso_url` with `SAMLRequest` and `RelayState` appended
/// (HTTP-Redirect binding: raw DEFLATE, base64, URL encoding). Unsigned (D-SA-8).
pub fn authn_request_redirect_url(request: &AuthnRequest<'_>) -> Result<String, RequestError> {
    if !is_valid_request_id(request.request_id) {
        return Err(RequestError::RequestId);
    }
    // Parsing strips tabs and newlines silently, so control characters are refused before it.
    if [request.sp_entity_id, request.acs_url, request.sso_url]
        .iter()
        .any(|value| value.chars().any(char::is_control))
    {
        return Err(RequestError::Url);
    }
    for value in [request.sp_entity_id, request.acs_url] {
        Url::parse(value).map_err(|_| RequestError::Url)?;
    }
    let sso = Url::parse(request.sso_url).map_err(|_| RequestError::Url)?;
    if sso.scheme() != "https"
        || !sso.username().is_empty()
        || sso.password().is_some()
        || sso.fragment().is_some()
    {
        return Err(RequestError::Url);
    }

    let issue_instant =
        chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(request.issue_instant_ns)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
    let id = xml::escape_attr(request.request_id);
    // The normalized single sign-on URL, which is also the redirect target without its parameters.
    let destination = xml::escape_attr(sso.as_str());
    let acs = xml::escape_attr(request.acs_url);
    let issuer = xml::escape_text(request.sp_entity_id);
    let xml = format!(
        "<samlp:AuthnRequest xmlns:samlp=\"{NS_PROTOCOL}\" xmlns:saml=\"{NS_ASSERTION}\" ID=\"{id}\" Version=\"2.0\" IssueInstant=\"{issue_instant}\" Destination=\"{destination}\" AssertionConsumerServiceURL=\"{acs}\" ProtocolBinding=\"{BINDING_POST}\"><saml:Issuer>{issuer}</saml:Issuer><samlp:NameIDPolicy Format=\"{NAMEID_UNSPECIFIED}\" AllowCreate=\"true\"/></samlp:AuthnRequest>"
    );

    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(xml.as_bytes())
        .map_err(|_| RequestError::Encode)?;
    let deflated = encoder.finish().map_err(|_| RequestError::Encode)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(deflated);

    let mut url = sso;
    url.query_pairs_mut()
        .append_pair("SAMLRequest", &encoded)
        .append_pair("RelayState", request.relay_state);
    Ok(url.to_string())
}

/// The SP metadata XML served at the SP entity ID (D-SA-12).
pub fn sp_metadata_xml(sp_entity_id: &str, acs_url: &str) -> String {
    let entity = xml::escape_attr(sp_entity_id);
    let acs = xml::escape_attr(acs_url);
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<md:EntityDescriptor xmlns:md=\"{NS_METADATA}\" entityID=\"{entity}\"><md:SPSSODescriptor AuthnRequestsSigned=\"false\" WantAssertionsSigned=\"true\" protocolSupportEnumeration=\"{NS_PROTOCOL}\"><md:NameIDFormat>{NAMEID_UNSPECIFIED}</md:NameIDFormat><md:AssertionConsumerService Binding=\"{BINDING_POST}\" Location=\"{acs}\" index=\"0\" isDefault=\"true\"/></md:SPSSODescriptor></md:EntityDescriptor>"
    )
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use base64::Engine;
    use url::Url;

    use super::*;

    const SP: &str = "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001";
    const ACS: &str = "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs";
    const SSO: &str = "https://idp.test/sso";
    const ID: &str = "_0123456789abcdef0123456789abcdef";
    const INSTANT: i64 = 1_767_225_600_000_000_000;

    const EXPECTED_XML: &str = r#"<samlp:AuthnRequest xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="_0123456789abcdef0123456789abcdef" Version="2.0" IssueInstant="2026-01-01T00:00:00Z" Destination="https://idp.test/sso" AssertionConsumerServiceURL="https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs" ProtocolBinding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST"><saml:Issuer>https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001</saml:Issuer><samlp:NameIDPolicy Format="urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified" AllowCreate="true"/></samlp:AuthnRequest>"#;

    const EXPECTED_METADATA: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001"><md:SPSSODescriptor AuthnRequestsSigned="false" WantAssertionsSigned="true" protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol"><md:NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified</md:NameIDFormat><md:AssertionConsumerService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" Location="https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs" index="0" isDefault="true"/></md:SPSSODescriptor></md:EntityDescriptor>"#;

    fn request() -> AuthnRequest<'static> {
        AuthnRequest {
            sp_entity_id: SP,
            acs_url: ACS,
            sso_url: SSO,
            request_id: ID,
            relay_state: "/projects",
            issue_instant_ns: INSTANT,
        }
    }

    /// The value of query parameter `name` in `url`.
    fn query_value(url: &str, name: &str) -> String {
        Url::parse(url)
            .expect("valid URL")
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .expect("parameter present")
    }

    /// The XML of a base64 raw-DEFLATE `SAMLRequest` value.
    fn inflate(encoded: &str) -> String {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64");
        let mut xml = String::new();
        flate2::read::DeflateDecoder::new(&bytes[..])
            .read_to_string(&mut xml)
            .expect("raw DEFLATE");
        xml
    }

    #[test]
    fn request_ids_are_checked() {
        assert!(is_valid_request_id(ID));
        assert!(!is_valid_request_id("_0123456789ABCDEF0123456789ABCDEF"));
        assert!(!is_valid_request_id("_0123456789abcdef0123456789abcde"));
        assert!(!is_valid_request_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_request_id("0_123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn the_redirect_url_round_trips() {
        let url = authn_request_redirect_url(&request()).expect("valid request");
        assert_eq!(inflate(&query_value(&url, "SAMLRequest")), EXPECTED_XML);
        assert_eq!(query_value(&url, "RelayState"), "/projects");
    }

    #[test]
    fn an_existing_query_is_kept() {
        let url = authn_request_redirect_url(&AuthnRequest {
            sso_url: "https://idp.test/sso?tenant=a",
            ..request()
        })
        .expect("valid request");
        assert_eq!(query_value(&url, "tenant"), "a");
        assert!(!query_value(&url, "SAMLRequest").is_empty());
        assert_eq!(query_value(&url, "RelayState"), "/projects");
    }

    #[test]
    fn special_characters_are_escaped() {
        let acs =
            "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs?a=1&b=2";
        let url = authn_request_redirect_url(&AuthnRequest {
            acs_url: acs,
            ..request()
        })
        .expect("valid request");
        let xml = inflate(&query_value(&url, "SAMLRequest"));
        assert!(xml.contains(
            "AssertionConsumerServiceURL=\"https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs?a=1&amp;b=2\""
        ));
        assert!(xml::parse_strict(&xml).is_ok());
    }

    #[test]
    fn a_bad_request_id_is_refused() {
        assert_eq!(
            authn_request_redirect_url(&AuthnRequest {
                request_id: "_0123456789ABCDEF0123456789ABCDEF",
                ..request()
            })
            .err(),
            Some(RequestError::RequestId)
        );
    }

    #[test]
    fn an_http_sso_url_is_refused() {
        assert_eq!(
            authn_request_redirect_url(&AuthnRequest {
                sso_url: "http://idp.test/sso",
                ..request()
            })
            .err(),
            Some(RequestError::Url)
        );
    }

    #[test]
    fn a_sso_url_with_a_tab_is_refused() {
        assert_eq!(
            authn_request_redirect_url(&AuthnRequest {
                sso_url: "https://idp.test/s\tso",
                ..request()
            })
            .err(),
            Some(RequestError::Url)
        );
    }

    #[test]
    fn control_characters_in_any_input_are_refused() {
        let inputs = [
            AuthnRequest {
                sp_entity_id: "https://cetra.test/\nsp",
                ..request()
            },
            AuthnRequest {
                acs_url: "https://cetra.test/a\rcs",
                ..request()
            },
            AuthnRequest {
                sso_url: "https://idp.test/sso\u{7f}",
                ..request()
            },
        ];
        for input in &inputs {
            assert_eq!(
                authn_request_redirect_url(input).err(),
                Some(RequestError::Url)
            );
        }
    }

    #[test]
    fn an_sso_url_with_user_information_or_a_fragment_is_refused() {
        for sso_url in ["https://u:p@idp.test/sso", "https://idp.test/sso#x"] {
            assert_eq!(
                authn_request_redirect_url(&AuthnRequest {
                    sso_url,
                    ..request()
                })
                .err(),
                Some(RequestError::Url)
            );
        }
    }

    #[test]
    fn the_destination_matches_the_redirect_target() {
        let url = authn_request_redirect_url(&AuthnRequest {
            sso_url: "HTTPS://Idp.test/sso?tenant=a",
            ..request()
        })
        .expect("valid request");
        let xml = inflate(&query_value(&url, "SAMLRequest"));
        assert!(xml.contains("Destination=\"https://idp.test/sso?tenant=a\""));
        assert!(url.starts_with("https://idp.test/sso?tenant=a&"));
    }

    #[test]
    fn debug_hides_the_request_id_and_the_sso_url() {
        let debug = format!("{:?}", request());
        assert!(!debug.contains(ID));
        assert!(!debug.contains(SSO));
        assert!(debug.contains("request_id_len"));
    }

    #[test]
    fn a_relative_acs_url_is_refused() {
        assert_eq!(
            authn_request_redirect_url(&AuthnRequest {
                acs_url: "/api/auth/saml/018f6b4e/acs",
                ..request()
            })
            .err(),
            Some(RequestError::Url)
        );
    }

    #[test]
    fn sp_metadata_is_exact() {
        let xml = sp_metadata_xml(SP, ACS);
        assert_eq!(xml, EXPECTED_METADATA);
        let doc = xml::parse_strict(&xml).expect("well formed");
        let root = doc.document_element().expect("root element");
        assert!(xml::is_element(&doc, root, NS_METADATA, "EntityDescriptor"));
    }
}
