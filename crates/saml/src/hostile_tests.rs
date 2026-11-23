// SPDX-License-Identifier: Elastic-2.0
//! The hostile corpus of D-SA-15: signature wrapping, comment injection, DTDs, KeyInfo substitution, weak algorithms (track SA M1).

use std::ops::Range;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::test_idp::{
    self, ACS_URL, ASSERTION_ID, NOW_NS, REQUEST_ID, RESPONSE_ID, ResponseOptions, SP_ENTITY_ID,
    Signing, TestKey,
};
use crate::xml::{C14N_EXCLUSIVE, DIGEST_SHA1, DIGEST_SHA256, SIG_RSA_SHA1, SIG_RSA_SHA256};
use crate::{
    Assertion, AttributeNames, ExpectedResponse, ResponseError, decode_response, validate_response,
};

/// The canonicalization method that keeps comments, which the corpus swaps in.
const C14N_INCLUSIVE: &str = "http://www.w3.org/TR/2001/REC-xml-c14n-20010315";

/// The start tag of the Assertion that carries `ASSERTION_ID`.
fn assertion_start() -> String {
    format!("<saml:Assertion ID=\"{ASSERTION_ID}\"")
}

/// The Response that `test_idp` signs as `signing`, with every other option at its default.
fn response(signing: Signing) -> String {
    test_idp::response_xml(&ResponseOptions {
        signing,
        ..ResponseOptions::default()
    })
}

/// Validates the XML `xml` as the answer to `request_id`, against the test IdP's metadata.
fn check_with(xml: &str, request_id: &str) -> Result<Assertion, ResponseError> {
    let idp = test_idp::idp_metadata();
    let names = AttributeNames::default();
    let expected = ExpectedResponse {
        sp_entity_id: SP_ENTITY_ID,
        acs_url: ACS_URL,
        idp: &idp,
        request_id,
        now_ns: NOW_NS,
        attributes: &names,
    };
    let received = decode_response(&STANDARD.encode(xml))?;
    validate_response(&received, &expected)
}

/// `check_with` for the request the test IdP answers by default.
fn check(xml: &str) -> Result<Assertion, ResponseError> {
    check_with(xml, REQUEST_ID)
}

/// The unsigned Assertion that `test_idp` writes for `name_id`, with its ID replaced by `id`.
fn evil_assertion(id: &str, name_id: &str) -> String {
    let options = ResponseOptions {
        name_id: name_id.to_owned(),
        signing: Signing::None,
        ..ResponseOptions::default()
    };
    let xml = test_idp::response_xml(&options);
    let span = assertion_span(&xml);
    xml[span].replacen(
        &format!("ID=\"{ASSERTION_ID}\""),
        &format!("ID=\"{id}\""),
        1,
    )
}

/// `evil` with `inner` in an `<saml:Advice>` at its end.
fn with_advice(evil: &str, inner: &str) -> String {
    let close = "</saml:Assertion>";
    evil.replacen(
        close,
        &format!("<saml:Advice>{inner}</saml:Advice>{close}"),
        1,
    )
}

/// The byte range of the Assertion that carries `ASSERTION_ID`, from its start tag to its end tag.
fn assertion_span(xml: &str) -> Range<usize> {
    let start = xml
        .find(assertion_start().as_str())
        .expect("the signed Assertion is present");
    let close = "</saml:Assertion>";
    let end = start
        + xml[start..]
            .find(close)
            .expect("the signed Assertion closes")
        + close.len();
    start..end
}

/// Replaces the signed Assertion of `xml` by `rewrite` applied to its text.
fn rewrite_signed_assertion(xml: &str, rewrite: impl Fn(&str) -> String) -> String {
    let span = assertion_span(xml);
    let replacement = rewrite(&xml[span.clone()]);
    let mut out = xml.to_owned();
    out.replace_range(span, &replacement);
    out
}

/// The byte range of the last `ds:Signature` in `xml`. With one signature that is the Assertion's;
/// with two (`Signing::Both`) the Response's comes first, so the last one is the Assertion's.
fn last_signature_span(xml: &str) -> Range<usize> {
    let start = xml.rfind("<ds:Signature ").expect("a signature is present");
    let close = "</ds:Signature>";
    let end = start + xml[start..].find(close).expect("the signature closes") + close.len();
    start..end
}

/// The text of the Assertion's signature (see `last_signature_span`).
fn assertion_signature(xml: &str) -> String {
    xml[last_signature_span(xml)].to_owned()
}

/// `xml` without the Assertion's signature (see `last_signature_span`).
fn without_assertion_signature(xml: &str) -> String {
    let mut out = xml.to_owned();
    out.replace_range(last_signature_span(xml), "");
    out
}

/// The classic ten-level entity expansion: `lol9` expands to a billion `lol`s.
fn billion_laughs_dtd() -> String {
    let mut dtd = String::from("<!DOCTYPE lolz [<!ENTITY lol \"lol\">");
    let mut previous = String::from("lol");
    for level in 1..=9 {
        let name = format!("lol{level}");
        let reference = format!("&{previous};");
        dtd.push_str(&format!("<!ENTITY {name} \"{}\">", reference.repeat(10)));
        previous = name;
    }
    dtd.push_str("]>");
    dtd
}

#[test]
fn xsw_second_assertion_before_the_signed_one() {
    let xml = response(Signing::Assertion);
    let marker = assertion_start();
    assert!(xml.contains(&marker));
    let evil = evil_assertion("_evil", "admin@example.com");
    let altered = xml.replacen(&marker, &format!("{evil}{marker}"), 1);
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn xsw_second_assertion_after_the_signed_one() {
    let xml = response(Signing::Assertion);
    let marker = "</samlp:Response>";
    assert!(xml.contains(marker));
    let evil = evil_assertion("_evil", "admin@example.com");
    let altered = xml.replacen(marker, &format!("{evil}{marker}"), 1);
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn xsw_signed_assertion_moved_into_extensions() {
    let xml = response(Signing::Assertion);
    assert!(xml.contains(&assertion_start()));
    let evil = evil_assertion("_evil", "admin@example.com");
    let altered = rewrite_signed_assertion(&xml, |signed| {
        format!("<samlp:Extensions>{signed}</samlp:Extensions>{evil}")
    });
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn xsw_signed_assertion_inside_the_evil_one() {
    let xml = response(Signing::Assertion);
    assert!(xml.contains(&assertion_start()));
    let altered = rewrite_signed_assertion(&xml, |signed| {
        with_advice(&evil_assertion("_evil", "admin@example.com"), signed)
    });
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn xsw_signed_assertion_inside_the_signature_object() {
    let xml = response(Signing::Response);
    let name_id = ">ada@example.com</saml:NameID>";
    assert!(xml.contains(name_id));
    assert!(xml.contains("</ds:Signature>"));
    // The copy is taken before the direct Assertion's NameID changes, so it holds the original.
    let copy = xml[assertion_span(&xml)].to_owned();
    let altered = xml
        .replacen(name_id, ">admin@example.com</saml:NameID>", 1)
        .replacen(
            "</ds:Signature>",
            &format!("<ds:Object>{copy}</ds:Object></ds:Signature>"),
            1,
        );
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn xsw_duplicate_id() {
    let xml = response(Signing::Assertion);
    assert!(xml.contains(&assertion_start()));
    // The evil copy with the same ID takes the Response-level position; the signed one moves into
    // Extensions.
    let altered = rewrite_signed_assertion(&xml, |signed| {
        format!(
            "<samlp:Extensions>{signed}</samlp:Extensions>{}",
            evil_assertion(ASSERTION_ID, "admin@example.com")
        )
    });
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
    // The variant: the evil copy is the only Assertion, and the signed copy sits in its Advice.
    let altered = rewrite_signed_assertion(&xml, |signed| {
        with_advice(&evil_assertion(ASSERTION_ID, "admin@example.com"), signed)
    });
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn signature_moved_to_the_response() {
    let xml = response(Signing::Assertion);
    assert!(xml.contains("</saml:Issuer>"));
    let signature = assertion_signature(&xml);
    let altered = without_assertion_signature(&xml).replacen(
        "</saml:Issuer>",
        &format!("</saml:Issuer>{signature}"),
        1,
    );
    assert_eq!(check(&altered), Err(ResponseError::SignatureLayout));
}

#[test]
fn signature_in_the_subject() {
    let xml = response(Signing::Assertion);
    assert!(xml.contains("<saml:Subject>"));
    let signature = assertion_signature(&xml);
    let altered = without_assertion_signature(&xml).replacen(
        "<saml:Subject>",
        &format!("<saml:Subject>{signature}"),
        1,
    );
    assert_eq!(check(&altered), Err(ResponseError::SignatureLayout));
}

#[test]
fn empty_reference_uri() {
    let xml = response(Signing::Assertion);
    let marker = format!("URI=\"#{ASSERTION_ID}\"");
    assert!(xml.contains(&marker));
    let altered = xml.replacen(&marker, "URI=\"\"", 1);
    assert_eq!(check(&altered), Err(ResponseError::SignatureLayout));
}

#[test]
fn reference_to_another_id() {
    let xml = response(Signing::Response);
    let marker = format!("URI=\"#{RESPONSE_ID}\"");
    assert!(xml.contains(&marker));
    let altered = xml.replacen(&marker, &format!("URI=\"#{ASSERTION_ID}\""), 1);
    assert_eq!(check(&altered), Err(ResponseError::SignatureLayout));
}

#[test]
fn three_signatures() {
    let xml = response(Signing::Both);
    assert!(xml.contains("<samlp:Status>"));
    let signature = assertion_signature(&xml);
    let altered = xml.replacen("<samlp:Status>", &format!("<samlp:Status>{signature}"), 1);
    assert_eq!(check(&altered), Err(ResponseError::SignatureLayout));
}

#[test]
fn name_id_modified_after_signing() {
    let xml = response(Signing::Assertion);
    let marker = ">ada@example.com</saml:NameID>";
    assert!(xml.contains(marker));
    let altered = xml.replacen(marker, ">admin@example.com</saml:NameID>", 1);
    assert_eq!(check(&altered), Err(ResponseError::SignatureInvalid));
}

#[test]
fn comment_injected_after_signing() {
    let xml = response(Signing::Assertion);
    let marker = ">ada@example.com</saml:NameID>";
    assert!(xml.contains(marker));
    // The canonical text becomes `ada@example.com.evil.test`, which the IdP did not sign.
    let altered = xml.replacen(marker, ">ada@example.com<!---->.evil.test</saml:NameID>", 1);
    assert_eq!(check(&altered), Err(ResponseError::SignatureInvalid));
}

#[test]
fn comment_signed_by_the_idp() {
    let options = ResponseOptions {
        name_id: "ada@example.com.evil.test".to_owned(),
        ..ResponseOptions::default()
    };
    let xml = test_idp::response_xml(&options);
    let marker = ">ada@example.com.evil.test</saml:NameID>";
    assert!(xml.contains(marker));
    let altered = xml.replacen(marker, ">ada@example.com<!---->.evil.test</saml:NameID>", 1);
    assert_eq!(check(&altered), Err(ResponseError::ValueContent));
}

#[test]
fn comment_in_the_email_attribute() {
    let xml = response(Signing::Assertion);
    let marker = "<saml:AttributeValue>ada@example.com</saml:AttributeValue>";
    assert!(xml.contains(marker));
    let altered = xml.replacen(
        marker,
        "<saml:AttributeValue>ada@example.com<!----></saml:AttributeValue>",
        1,
    );
    assert_eq!(check(&altered), Err(ResponseError::ValueContent));
}

#[test]
fn cdata_name_id_is_plain_text() {
    let xml = response(Signing::Assertion);
    let marker = ">ada@example.com</saml:NameID>";
    assert!(xml.contains(marker));
    // CDATA is text in canonical XML, so the signature still verifies. If a provider's signature
    // ever fails here, that is a bergshamra gap to record, not a reason to weaken this test.
    let altered = xml.replacen(marker, "><![CDATA[ada@example.com]]></saml:NameID>", 1);
    let assertion = check(&altered).expect("a CDATA NameID is plain text");
    assert_eq!(assertion.name_id, "ada@example.com");
}

#[test]
fn doctype_entity_expansion() {
    let xml = response(Signing::Assertion);
    let marker = "<samlp:Response";
    assert!(xml.contains(marker));
    let altered = format!("<!DOCTYPE r [<!ENTITY x \"admin\">]>{xml}");
    assert_eq!(check(&altered), Err(ResponseError::Doctype));
}

#[test]
fn billion_laughs() {
    let xml = response(Signing::Assertion);
    let marker = "<samlp:Response";
    assert!(xml.contains(marker));
    let altered = format!("{}{xml}", billion_laughs_dtd());
    assert_eq!(check(&altered), Err(ResponseError::Doctype));
}

#[test]
fn external_entity() {
    let xml = response(Signing::Assertion);
    let marker = "<samlp:Response";
    assert!(xml.contains(marker));
    let altered = format!("<!DOCTYPE r [<!ENTITY x SYSTEM \"file:///etc/passwd\">]>{xml}");
    assert_eq!(check(&altered), Err(ResponseError::Doctype));
}

#[test]
fn keyinfo_certificate_is_ignored() {
    let options = ResponseOptions {
        key: TestKey::Attacker,
        key_info: true,
        ..ResponseOptions::default()
    };
    let xml = test_idp::response_xml(&options);
    assert!(xml.contains("<ds:X509Certificate>"));
    assert_eq!(check(&xml), Err(ResponseError::SignatureInvalid));
}

#[test]
fn rsa_sha1_is_refused() {
    let xml = response(Signing::Assertion);
    let marker = format!("<ds:SignatureMethod Algorithm=\"{SIG_RSA_SHA256}\"/>");
    assert!(xml.contains(&marker));
    let altered = xml.replacen(
        &marker,
        &format!("<ds:SignatureMethod Algorithm=\"{SIG_RSA_SHA1}\"/>"),
        1,
    );
    assert_eq!(check(&altered), Err(ResponseError::Algorithm));
}

#[test]
fn sha1_digest_is_refused() {
    let xml = response(Signing::Assertion);
    let marker = format!("<ds:DigestMethod Algorithm=\"{DIGEST_SHA256}\"/>");
    assert!(xml.contains(&marker));
    let altered = xml.replacen(
        &marker,
        &format!("<ds:DigestMethod Algorithm=\"{DIGEST_SHA1}\"/>"),
        1,
    );
    assert_eq!(check(&altered), Err(ResponseError::Algorithm));
}

#[test]
fn inclusive_c14n_is_refused() {
    let xml = response(Signing::Assertion);
    let marker = format!("<ds:CanonicalizationMethod Algorithm=\"{C14N_EXCLUSIVE}\"/>");
    assert!(xml.contains(&marker));
    let altered = xml.replacen(
        &marker,
        &format!("<ds:CanonicalizationMethod Algorithm=\"{C14N_INCLUSIVE}\"/>"),
        1,
    );
    assert_eq!(check(&altered), Err(ResponseError::Algorithm));
}

#[test]
fn xpath_transform_is_refused() {
    let xml = response(Signing::Assertion);
    let marker = "</ds:Transforms>";
    assert!(xml.contains(marker));
    let xpath = "<ds:Transform Algorithm=\"http://www.w3.org/TR/1999/REC-xpath-19991116\"><ds:XPath>1</ds:XPath></ds:Transform>";
    let altered = xml.replacen(marker, &format!("{xpath}{marker}"), 1);
    assert_eq!(check(&altered), Err(ResponseError::Algorithm));
}

#[test]
fn encrypted_assertion_is_unsupported() {
    let xml = response(Signing::Assertion);
    assert!(xml.contains(&assertion_start()));
    let encrypted = "<saml:EncryptedAssertion><xenc:EncryptedData xmlns:xenc=\"http://www.w3.org/2001/04/xmlenc#\"/></saml:EncryptedAssertion>";
    let altered = rewrite_signed_assertion(&xml, |_| encrypted.to_owned());
    assert_eq!(check(&altered), Err(ResponseError::EncryptedAssertion));
}

#[test]
fn encrypted_assertion_beside_a_signed_one() {
    let xml = response(Signing::Assertion);
    assert!(xml.contains(&assertion_start()));
    let encrypted = "<saml:EncryptedAssertion><xenc:EncryptedData xmlns:xenc=\"http://www.w3.org/2001/04/xmlenc#\"/></saml:EncryptedAssertion>";
    let altered = rewrite_signed_assertion(&xml, |signed| format!("{encrypted}{signed}"));
    assert_eq!(check(&altered), Err(ResponseError::EncryptedAssertion));
}

#[test]
fn unsigned_assertion_in_a_signed_response_twin() {
    let xml = response(Signing::Response);
    let marker = "</samlp:Response>";
    assert!(xml.contains(marker));
    let evil = evil_assertion("_evil", "admin@example.com");
    let altered = xml.replacen(marker, &format!("{evil}{marker}"), 1);
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn response_signature_with_a_tampered_assertion() {
    let xml = response(Signing::Response);
    let marker = format!("Recipient=\"{ACS_URL}\"");
    assert!(xml.contains(&marker));
    let altered = xml.replacen(&marker, "Recipient=\"https://cetra.test/other/acs\"", 1);
    assert_eq!(check(&altered), Err(ResponseError::SignatureInvalid));
}

#[test]
fn both_signed_one_broken() {
    let xml = response(Signing::Both);
    let open = "<ds:SignatureValue>";
    assert!(xml.contains(open));
    // The first SignatureValue is the Response's: its signature comes before the Status.
    let start = xml.find(open).expect("the marker is present") + open.len();
    let end = start
        + xml[start..]
            .find("</ds:SignatureValue>")
            .expect("the value closes");
    // The value is base64 (ASCII), and its middle is far from the padding.
    let middle = start + (end - start) / 2;
    let replacement = if xml.as_bytes()[middle] == b'A' {
        "B"
    } else {
        "A"
    };
    let mut altered = xml;
    altered.replace_range(middle..middle + 1, replacement);
    assert_eq!(check(&altered), Err(ResponseError::SignatureInvalid));
}

#[test]
fn id_copied_onto_another_element() {
    let xml = response(Signing::Assertion);
    let marker = "<saml:Subject>";
    assert!(xml.contains(marker));
    let altered = xml.replacen(marker, &format!("<saml:Subject ID=\"{ASSERTION_ID}\">"), 1);
    assert_eq!(check(&altered), Err(ResponseError::SignatureInvalid));
}

#[test]
fn namespace_confusion() {
    let xml = response(Signing::Assertion);
    let marker = "<saml:Assertion ID=";
    assert!(xml.contains(marker));
    let altered = xml.replacen(marker, "<saml:Assertion xmlns:saml=\"urn:evil\" ID=", 1);
    assert_eq!(check(&altered), Err(ResponseError::AssertionCount));
}

#[test]
fn replayed_response_for_another_request() {
    let xml = response(Signing::Assertion);
    let marker = format!("InResponseTo=\"{REQUEST_ID}\"");
    assert!(xml.contains(&marker));
    assert_eq!(
        check_with(&xml, "_ffffffffffffffffffffffffffffffff"),
        Err(ResponseError::InResponseTo)
    );
}
