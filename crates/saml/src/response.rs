// SPDX-License-Identifier: Elastic-2.0
//! SAML Response decoding and validation (track SA, D-SA-9). A Response is trusted only after
//! `validate_response` has checked it against the pinned IdP keys and this SP's expectations.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bergshamra::dsig::verify::{VerifyResult, verify_all_document};
use bergshamra::xml::uppsala::{Document, NodeId};
use bergshamra::{DsigContext, KeysManager};

use crate::xml::{
    C14N_EXCLUSIVE, C14N_EXCLUSIVE_COMMENTS, CM_BEARER, DIGEST_SHA256, DIGEST_SHA384,
    DIGEST_SHA512, NAMEID_EMAIL, NAMEID_ENTITY, NAMEID_TRANSIENT, NS_ASSERTION, NS_DSIG,
    NS_PROTOCOL, ParseFailure, SIG_ECDSA_SHA256, SIG_ECDSA_SHA384, SIG_RSA_SHA256, SIG_RSA_SHA384,
    SIG_RSA_SHA512, STATUS_SUCCESS, TRANSFORM_ENVELOPED, attr, children_named, descendants_named,
    is_element, only_child, parse_strict, strict_text,
};

/// The largest base64 form value read (before decoding), in bytes.
pub const MAX_ENCODED_BYTES: usize = 360_000;
/// The largest decoded Response, in bytes (D-SA-9 rule 1).
pub const MAX_DECODED_BYTES: usize = 256 * 1024;
/// The clock skew allowed on every time check (D-SA-9 rule 16), in nanoseconds.
const SKEW_NS: i64 = 60_000_000_000;
/// How old an `IssueInstant` may be, in nanoseconds (D-SA-9 rule 15).
const ISSUE_MAX_AGE_NS: i64 = 300_000_000_000;
/// The longest NameID accepted, in bytes.
const MAX_NAME_ID_BYTES: usize = 1024;
/// The longest email accepted, in bytes.
const MAX_EMAIL_BYTES: usize = 320;
/// The longest display name kept, in characters (longer names are truncated).
const MAX_DISPLAY_NAME_CHARS: usize = 256;

/// The signature methods allowed on a Response or Assertion (D-SA-9 rule 8). RSA-SHA1 is not here.
const SIGNATURE_METHODS: [&str; 5] = [
    SIG_RSA_SHA256,
    SIG_RSA_SHA384,
    SIG_RSA_SHA512,
    SIG_ECDSA_SHA256,
    SIG_ECDSA_SHA384,
];
/// The digest methods allowed on a reference: 256 bits and above (D-SA-9 rule 9).
const DIGEST_METHODS: [&str; 3] = [DIGEST_SHA256, DIGEST_SHA384, DIGEST_SHA512];

/// Why a Response was refused (D-SA-9). Messages carry no payload. M2 maps
/// `EncryptedAssertion` to the login code `saml_unsupported` and every other variant to
/// `saml_invalid`, and logs the variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ResponseError {
    #[error("the SAML response is too large")]
    TooLarge,
    #[error("the SAML response is not valid base64")]
    Base64,
    #[error("the SAML response is not UTF-8")]
    Utf8,
    #[error("the SAML response contains a DTD or an entity declaration")]
    Doctype,
    #[error("the SAML response is not well-formed XML or exceeds the parse limits")]
    Xml,
    #[error("the document is not a SAML 2.0 Response")]
    NotAResponse,
    #[error("the SAML version is not 2.0")]
    Version,
    #[error("the identity provider did not report success")]
    Status,
    #[error("encrypted assertions are not supported")]
    EncryptedAssertion,
    #[error("the response must contain exactly one assertion, directly in the response")]
    AssertionCount,
    #[error("the response is not signed")]
    SignatureMissing,
    #[error("a signature is in an unexpected place or covers an unexpected element")]
    SignatureLayout,
    #[error("a signature uses an algorithm that is not allowed")]
    Algorithm,
    #[error("a signature does not verify against the trusted certificates")]
    SignatureInvalid,
    #[error("the issuer is not the configured identity provider")]
    Issuer,
    #[error("the destination is not this service provider's ACS URL")]
    Destination,
    #[error("the response does not answer the pending request")]
    InResponseTo,
    #[error("the issue instant is outside the allowed window")]
    IssueInstant,
    #[error("the assertion's conditions are missing, not yet valid or expired")]
    Conditions,
    #[error("the assertion is not addressed to this service provider")]
    Audience,
    #[error("no bearer subject confirmation is valid for this request")]
    SubjectConfirmation,
    #[error("the assertion has no usable name identifier")]
    NameIdMissing,
    #[error("a transient name identifier cannot identify a person")]
    NameIdTransient,
    #[error("a value contains markup or a comment, or is too long")]
    ValueContent,
}

/// A decoded, well-formed Response whose root is a SAML 2.0 `Response`, before validation.
pub struct ReceivedResponse {
    xml: String,
    in_response_to: Option<String>,
}

impl ReceivedResponse {
    /// The root's `InResponseTo`; `None` means unsolicited (D-SA-5).
    pub fn in_response_to(&self) -> Option<&str> {
        self.in_response_to.as_deref()
    }

    /// Reads the root of `xml` (rules 1, 2, 18): it must be a SAML 2.0 `Response`.
    pub(crate) fn from_xml(xml: String) -> Result<Self, ResponseError> {
        let in_response_to = {
            let doc = parse_response_xml(&xml)?;
            let root = doc.document_element().ok_or(ResponseError::NotAResponse)?;
            if !is_element(&doc, root, NS_PROTOCOL, "Response") {
                return Err(ResponseError::NotAResponse);
            }
            attr(&doc, root, "InResponseTo").map(str::to_owned)
        };
        Ok(Self {
            xml,
            in_response_to,
        })
    }
}

/// `Debug` prints the length and whether `InResponseTo` is set; never the XML.
impl std::fmt::Debug for ReceivedResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReceivedResponse")
            .field("xml_len", &self.xml.len())
            .field("in_response_to_set", &self.in_response_to.is_some())
            .finish()
    }
}

/// Attribute names to read, each list tried in order; the first attribute present wins (D-SA-6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeNames {
    pub email: Vec<String>,
    pub display_name: Vec<String>,
    pub first_name: Vec<String>,
    pub last_name: Vec<String>,
}

impl Default for AttributeNames {
    fn default() -> Self {
        Self {
            email: names(&[
                "email",
                "emailAddress",
                "mail",
                "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress",
                "urn:oid:0.9.2342.19200300.100.1.3",
            ]),
            display_name: names(&[
                "displayName",
                "http://schemas.microsoft.com/identity/claims/displayname",
                "urn:oid:2.16.840.1.113730.3.1.241",
                "name",
            ]),
            first_name: names(&[
                "firstName",
                "givenName",
                "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/givenname",
                "urn:oid:2.5.4.42",
            ]),
            last_name: names(&[
                "lastName",
                "surname",
                "sn",
                "http://schemas.xmlsoap.org/ws/2005/05/identity/claims/surname",
                "urn:oid:2.5.4.4",
            ]),
        }
    }
}

/// The owned attribute names of one list.
fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

/// What the server expects of this Response.
#[derive(Debug, Clone, Copy)]
pub struct ExpectedResponse<'a> {
    pub sp_entity_id: &'a str,
    pub acs_url: &'a str,
    pub idp: &'a crate::IdpMetadata,
    pub request_id: &'a str,
    pub now_ns: i64,
    pub attributes: &'a AttributeNames,
}

/// The facts of the one Assertion. `Debug` prints lengths only.
#[derive(Clone, PartialEq, Eq)]
pub struct Assertion {
    /// The NameID's exact text (D-SA-13).
    pub name_id: String,
    /// The NameID `Format`, when given.
    pub name_id_format: Option<String>,
    /// The email, trimmed, not lowercased (the server normalizes it).
    pub email: Option<String>,
    /// The display name, trimmed, at most 256 characters.
    pub display_name: Option<String>,
}

/// `Debug` prints lengths only.
impl std::fmt::Debug for Assertion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assertion")
            .field("name_id_len", &self.name_id.len())
            .field(
                "name_id_format_len",
                &self.name_id_format.as_ref().map(String::len),
            )
            .field("email_len", &self.email.as_ref().map(String::len))
            .field(
                "display_name_len",
                &self.display_name.as_ref().map(String::len),
            )
            .finish()
    }
}

/// Decodes the `SAMLResponse` form value (D-SA-9 rule 1) and reads `InResponseTo`.
pub fn decode_response(saml_response: &str) -> Result<ReceivedResponse, ResponseError> {
    if saml_response.len() > MAX_ENCODED_BYTES {
        return Err(ResponseError::TooLarge);
    }
    let compact: String = saml_response
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let bytes = STANDARD
        .decode(compact)
        .map_err(|_| ResponseError::Base64)?;
    if bytes.len() > MAX_DECODED_BYTES {
        return Err(ResponseError::TooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| ResponseError::Utf8)?;
    // Strip one leading byte order mark.
    let text = if text.starts_with('\u{feff}') {
        text['\u{feff}'.len_utf8()..].to_owned()
    } else {
        text
    };
    ReceivedResponse::from_xml(text)
}

/// Validates `received` against `expected` (D-SA-9 rules 2–19).
pub fn validate_response(
    received: &ReceivedResponse,
    expected: &ExpectedResponse<'_>,
) -> Result<Assertion, ResponseError> {
    // Rule 2: the Response is read again with the same parser; `from_xml` already accepted it.
    let doc = parse_response_xml(&received.xml)?;
    let root = doc.document_element().ok_or(ResponseError::NotAResponse)?;

    // Rule 3: the version.
    if attr(&doc, root, "Version") != Some("2.0") {
        return Err(ResponseError::Version);
    }
    // Rule 4: encrypted assertions are refused.
    if !descendants_named(&doc, root, NS_ASSERTION, "EncryptedAssertion").is_empty() {
        return Err(ResponseError::EncryptedAssertion);
    }
    // Rule 4: exactly one Assertion, directly in the Response.
    let assertion = one_assertion(&doc, root)?;
    // Rule 3: the status.
    check_status(&doc, root)?;
    // Rules 5 to 8: the signatures, their layout and their profile, then their verification.
    check_signatures(&doc, root, expected)?;
    // Rules 11, 13, 15: the Response-level values.
    check_response_level(&doc, root, expected)?;
    // Rules 3, 14, 15: the Assertion's version, issuer and issue instant.
    check_assertion_header(&doc, assertion, expected)?;

    // Rules 18, 19: the subject and its NameID.
    let subject =
        only_child(&doc, assertion, NS_ASSERTION, "Subject").ok_or(ResponseError::NameIdMissing)?;
    let name_id =
        only_child(&doc, subject, NS_ASSERTION, "NameID").ok_or(ResponseError::NameIdMissing)?;
    let name_id_format = attr(&doc, name_id, "Format");
    if name_id_format == Some(NAMEID_TRANSIENT) {
        return Err(ResponseError::NameIdTransient);
    }
    let name_id_text = strict_text(&doc, name_id).ok_or(ResponseError::ValueContent)?;
    if name_id_text.is_empty() {
        return Err(ResponseError::NameIdMissing);
    }
    if name_id_text.len() > MAX_NAME_ID_BYTES || has_control(&name_id_text) {
        return Err(ResponseError::ValueContent);
    }

    // Rules 11, 13, 14, 17: a bearer confirmation that answers this request.
    if !subject_confirmed(&doc, subject, expected) {
        return Err(ResponseError::SubjectConfirmation);
    }
    // Rules 12, 16: the conditions and the audience.
    check_conditions(&doc, assertion, expected)?;

    // Step 14: the attributes (D-SA-6).
    let email = email_of(
        &doc,
        assertion,
        expected.attributes,
        name_id_format,
        &name_id_text,
    )?;
    let display_name = display_name_of(&doc, assertion, expected.attributes)?;

    Ok(Assertion {
        name_id: name_id_text,
        name_id_format: name_id_format.map(str::to_owned),
        email,
        display_name,
    })
}

/// Parses a Response (or its own text) with the one parser configuration (rule 2).
fn parse_response_xml(text: &str) -> Result<Document<'_>, ResponseError> {
    parse_strict(text).map_err(|failure| match failure {
        ParseFailure::Doctype => ResponseError::Doctype,
        ParseFailure::Xml => ResponseError::Xml,
    })
}

/// The one Assertion, which must be a direct child of the Response (rule 4).
fn one_assertion(doc: &Document<'_>, root: NodeId) -> Result<NodeId, ResponseError> {
    match descendants_named(doc, root, NS_ASSERTION, "Assertion").as_slice() {
        [only] if doc.parent(*only) == Some(root) => Ok(*only),
        _ => Err(ResponseError::AssertionCount),
    }
}

/// The status must be `Success` (rule 3).
fn check_status(doc: &Document<'_>, root: NodeId) -> Result<(), ResponseError> {
    let status = only_child(doc, root, NS_PROTOCOL, "Status").ok_or(ResponseError::Status)?;
    let code = only_child(doc, status, NS_PROTOCOL, "StatusCode").ok_or(ResponseError::Status)?;
    if attr(doc, code, "Value") == Some(STATUS_SUCCESS) {
        Ok(())
    } else {
        Err(ResponseError::Status)
    }
}

/// Rules 5 to 8: the signatures are present, placed on the Response or the Assertion, use the
/// allowed profile, and verify against the pinned IdP keys only.
fn check_signatures(
    doc: &Document<'_>,
    root: NodeId,
    expected: &ExpectedResponse<'_>,
) -> Result<(), ResponseError> {
    let assertion = one_assertion(doc, root)?;

    // Rule 5 (layout): one or two signatures, each a child of the Response or the Assertion,
    // no two under the same parent. The signed element is the signature's parent.
    let signatures = descendants_named(doc, root, NS_DSIG, "Signature");
    if signatures.is_empty() {
        return Err(ResponseError::SignatureMissing);
    }
    if signatures.len() > 2 {
        return Err(ResponseError::SignatureLayout);
    }
    let mut signed: Vec<NodeId> = Vec::with_capacity(signatures.len());
    for &signature in &signatures {
        let parent = doc
            .parent(signature)
            .ok_or(ResponseError::SignatureLayout)?;
        if parent != root && parent != assertion {
            return Err(ResponseError::SignatureLayout);
        }
        if signed.contains(&parent) {
            return Err(ResponseError::SignatureLayout);
        }
        signed.push(parent);
    }

    // Rules 7, 8, 9: the SignedInfo profile of every signature.
    for (&signature, &signed_element) in signatures.iter().zip(&signed) {
        check_signed_info(doc, signature, signed_element)?;
    }

    // Rules 6, 7: verify every signature against the pinned IdP certificates only.
    let mut keys = KeysManager::new();
    for certificate in expected.idp.certificates() {
        keys.add_key(
            certificate
                .key()
                .map_err(|_| ResponseError::SignatureInvalid)?,
        );
    }
    let ctx = DsigContext::new(keys);
    let results = verify_all_document(&ctx, doc).map_err(|_| ResponseError::SignatureInvalid)?;
    if results.len() != signatures.len() {
        return Err(ResponseError::SignatureInvalid);
    }
    for result in &results {
        let VerifyResult::Valid {
            signature_node,
            references,
            ..
        } = result
        else {
            return Err(ResponseError::SignatureInvalid);
        };
        if !signatures.contains(signature_node) {
            return Err(ResponseError::SignatureInvalid);
        }
        let parent = doc
            .parent(*signature_node)
            .ok_or(ResponseError::SignatureInvalid)?;
        match references.as_slice() {
            [reference] if reference.digest_verified && reference.resolved_node == Some(parent) => {
            }
            _ => return Err(ResponseError::SignatureInvalid),
        }
    }
    Ok(())
}

/// Rule 7 for one signature: the canonicalization, signature and digest methods, one reference
/// to the signed element's `ID`, and the enveloped and exclusive transforms.
fn check_signed_info(
    doc: &Document<'_>,
    signature: NodeId,
    signed: NodeId,
) -> Result<(), ResponseError> {
    let signed_info =
        only_child(doc, signature, NS_DSIG, "SignedInfo").ok_or(ResponseError::SignatureLayout)?;

    let canonicalization = algorithm_of(doc, signed_info, "CanonicalizationMethod")?;
    if canonicalization != C14N_EXCLUSIVE && canonicalization != C14N_EXCLUSIVE_COMMENTS {
        return Err(ResponseError::Algorithm);
    }
    let signature_method = algorithm_of(doc, signed_info, "SignatureMethod")?;
    if !SIGNATURE_METHODS.contains(&signature_method) {
        return Err(ResponseError::Algorithm);
    }

    let reference = match children_named(doc, signed_info, NS_DSIG, "Reference").as_slice() {
        [only] => *only,
        _ => return Err(ResponseError::SignatureLayout),
    };
    let id = attr(doc, signed, "ID")
        .filter(|id| !id.is_empty())
        .ok_or(ResponseError::SignatureLayout)?;
    if attr(doc, reference, "URI") != Some(format!("#{id}").as_str()) {
        return Err(ResponseError::SignatureLayout);
    }

    // The enveloped transform, then exclusive c14n. A lone enveloped transform would fall back to
    // inclusive c14n, so exactly two transforms are required.
    let transforms =
        only_child(doc, reference, NS_DSIG, "Transforms").ok_or(ResponseError::Algorithm)?;
    match children_named(doc, transforms, NS_DSIG, "Transform").as_slice() {
        [first, second] => {
            if attr(doc, *first, "Algorithm") != Some(TRANSFORM_ENVELOPED) {
                return Err(ResponseError::Algorithm);
            }
            match attr(doc, *second, "Algorithm") {
                Some(C14N_EXCLUSIVE) | Some(C14N_EXCLUSIVE_COMMENTS) => {}
                _ => return Err(ResponseError::Algorithm),
            }
        }
        _ => return Err(ResponseError::Algorithm),
    }

    let digest = algorithm_of(doc, reference, "DigestMethod")?;
    if !DIGEST_METHODS.contains(&digest) {
        return Err(ResponseError::Algorithm);
    }
    Ok(())
}

/// The `Algorithm` of the only `local` child of `parent`. A missing or repeated child, or one
/// without the attribute, is `Algorithm`.
fn algorithm_of<'d>(
    doc: &'d Document<'_>,
    parent: NodeId,
    local: &str,
) -> Result<&'d str, ResponseError> {
    let node = only_child(doc, parent, NS_DSIG, local).ok_or(ResponseError::Algorithm)?;
    attr(doc, node, "Algorithm").ok_or(ResponseError::Algorithm)
}

/// Step 9: the Response's `Destination`, `InResponseTo`, `Issuer` and `IssueInstant`.
///
/// When only the Assertion is signed, the Response-level values (Status, Destination,
/// InResponseTo, Issuer, IssueInstant) are unsigned, and their checks can only add failures. No
/// success path depends on Response-level values alone: the signed Assertion carries Issuer,
/// IssueInstant and the bearer confirmation's Recipient and InResponseTo (step 12), which is what
/// binds the request.
fn check_response_level(
    doc: &Document<'_>,
    root: NodeId,
    expected: &ExpectedResponse<'_>,
) -> Result<(), ResponseError> {
    if attr(doc, root, "Destination").is_some_and(|destination| destination != expected.acs_url) {
        return Err(ResponseError::Destination);
    }
    if attr(doc, root, "InResponseTo") != Some(expected.request_id) {
        return Err(ResponseError::InResponseTo);
    }
    match children_named(doc, root, NS_ASSERTION, "Issuer").as_slice() {
        [] => {}
        [issuer] => check_issuer(doc, *issuer, expected)?,
        _ => return Err(ResponseError::Issuer),
    }
    let issued = attr(doc, root, "IssueInstant").ok_or(ResponseError::IssueInstant)?;
    check_issue_instant(issued, expected.now_ns)
}

/// Rule 10: the Assertion's version, issuer and issue instant.
fn check_assertion_header(
    doc: &Document<'_>,
    assertion: NodeId,
    expected: &ExpectedResponse<'_>,
) -> Result<(), ResponseError> {
    if attr(doc, assertion, "Version") != Some("2.0") {
        return Err(ResponseError::Version);
    }
    let issuer = only_child(doc, assertion, NS_ASSERTION, "Issuer").ok_or(ResponseError::Issuer)?;
    check_issuer(doc, issuer, expected)?;
    let issued = attr(doc, assertion, "IssueInstant").ok_or(ResponseError::IssueInstant)?;
    check_issue_instant(issued, expected.now_ns)
}

/// An `Issuer` has no `Format` or the entity format, and its text is the IdP's entity ID.
fn check_issuer(
    doc: &Document<'_>,
    issuer: NodeId,
    expected: &ExpectedResponse<'_>,
) -> Result<(), ResponseError> {
    if !matches!(attr(doc, issuer, "Format"), None | Some(NAMEID_ENTITY)) {
        return Err(ResponseError::Issuer);
    }
    if strict_text(doc, issuer).as_deref() != Some(expected.idp.entity_id()) {
        return Err(ResponseError::Issuer);
    }
    Ok(())
}

/// Rule 15: the issue instant is at most 5 minutes old and not more than 60 seconds ahead.
fn check_issue_instant(value: &str, now_ns: i64) -> Result<(), ResponseError> {
    let issued = parse_instant(value).ok_or(ResponseError::IssueInstant)?;
    let oldest = now_ns
        .checked_sub(ISSUE_MAX_AGE_NS)
        .ok_or(ResponseError::IssueInstant)?;
    let newest = now_ns
        .checked_add(SKEW_NS)
        .ok_or(ResponseError::IssueInstant)?;
    let issued_with_skew = issued
        .checked_add(SKEW_NS)
        .ok_or(ResponseError::IssueInstant)?;
    if issued_with_skew >= oldest && issued <= newest {
        Ok(())
    } else {
        Err(ResponseError::IssueInstant)
    }
}

/// An instant in the `Z` form, with optional fractional seconds, as nanoseconds since the epoch.
fn parse_instant(text: &str) -> Option<i64> {
    if !text.ends_with('Z') {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()?
        .timestamp_nanos_opt()
}

/// Rule 16: `now` is before `not_on_or_after` plus the skew. An overflow fails.
fn not_yet_expired(now_ns: i64, not_on_or_after_ns: i64) -> bool {
    not_on_or_after_ns
        .checked_add(SKEW_NS)
        .is_some_and(|limit| now_ns < limit)
}

/// Rule 16: `now` plus the skew is at or after `not_before`. An overflow fails.
fn already_valid(now_ns: i64, not_before_ns: i64) -> bool {
    now_ns
        .checked_add(SKEW_NS)
        .is_some_and(|limit| limit >= not_before_ns)
}

/// The attribute `name` of `node` is absent, or an instant that satisfies `holds`.
fn optional_instant(
    doc: &Document<'_>,
    node: NodeId,
    name: &str,
    holds: impl Fn(i64) -> bool,
) -> bool {
    match attr(doc, node, name) {
        None => true,
        Some(text) => parse_instant(text).is_some_and(holds),
    }
}

/// Rules 11, 13, 14, 17: at least one bearer `SubjectConfirmation` whose data answers this
/// request, for this ACS URL, before its `NotOnOrAfter` and after its `NotBefore`.
fn subject_confirmed(doc: &Document<'_>, subject: NodeId, expected: &ExpectedResponse<'_>) -> bool {
    children_named(doc, subject, NS_ASSERTION, "SubjectConfirmation")
        .into_iter()
        .any(|confirmation| {
            if attr(doc, confirmation, "Method") != Some(CM_BEARER) {
                return false;
            }
            let Some(data) = only_child(doc, confirmation, NS_ASSERTION, "SubjectConfirmationData")
            else {
                return false;
            };
            let now = expected.now_ns;
            attr(doc, data, "Recipient") == Some(expected.acs_url)
                && attr(doc, data, "InResponseTo") == Some(expected.request_id)
                && attr(doc, data, "NotOnOrAfter")
                    .and_then(parse_instant)
                    .is_some_and(|not_on_or_after| not_yet_expired(now, not_on_or_after))
                && optional_instant(doc, data, "NotBefore", |not_before| {
                    already_valid(now, not_before)
                })
        })
}

/// Rules 12, 16: the conditions hold at `now`, and every audience restriction names this SP.
fn check_conditions(
    doc: &Document<'_>,
    assertion: NodeId,
    expected: &ExpectedResponse<'_>,
) -> Result<(), ResponseError> {
    let now = expected.now_ns;
    let conditions =
        only_child(doc, assertion, NS_ASSERTION, "Conditions").ok_or(ResponseError::Conditions)?;
    let window_holds = optional_instant(doc, conditions, "NotBefore", |not_before| {
        already_valid(now, not_before)
    }) && optional_instant(doc, conditions, "NotOnOrAfter", |not_on_or_after| {
        not_yet_expired(now, not_on_or_after)
    });
    if !window_holds {
        return Err(ResponseError::Conditions);
    }

    // Restrictions are ANDed: at least one, and each must name this SP.
    let restrictions = children_named(doc, conditions, NS_ASSERTION, "AudienceRestriction");
    if restrictions.is_empty()
        || !restrictions
            .iter()
            .all(|&restriction| audience_matches(doc, restriction, expected.sp_entity_id))
    {
        return Err(ResponseError::Audience);
    }
    Ok(())
}

/// Whether an `AudienceRestriction` names `sp_entity_id`. An `Audience` with markup or a comment
/// is skipped, not read.
fn audience_matches(doc: &Document<'_>, restriction: NodeId, sp_entity_id: &str) -> bool {
    children_named(doc, restriction, NS_ASSERTION, "Audience")
        .into_iter()
        .any(|audience| strict_text(doc, audience).is_some_and(|text| text == sp_entity_id))
}

/// Rule 14: the email. The first `email` attribute with a value wins; otherwise, when the NameID
/// format is `emailAddress`, the NameID text, trimmed.
fn email_of(
    doc: &Document<'_>,
    assertion: NodeId,
    names: &AttributeNames,
    name_id_format: Option<&str>,
    name_id: &str,
) -> Result<Option<String>, ResponseError> {
    let email = match first_attribute(doc, assertion, &names.email)? {
        Some(value) => Some(value),
        None if name_id_format == Some(NAMEID_EMAIL) => {
            let trimmed = name_id.trim_ascii();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        }
        None => None,
    };
    if email
        .as_deref()
        .is_some_and(|value| value.len() > MAX_EMAIL_BYTES || has_control(value))
    {
        return Err(ResponseError::ValueContent);
    }
    Ok(email)
}

/// Rule 14: the display name. The display-name attribute wins; otherwise the first and last names
/// joined by one space (either alone when the other is absent). Truncated to 256 characters.
fn display_name_of(
    doc: &Document<'_>,
    assertion: NodeId,
    names: &AttributeNames,
) -> Result<Option<String>, ResponseError> {
    let name = match first_attribute(doc, assertion, &names.display_name)? {
        Some(name) => Some(name),
        None => {
            let first = first_attribute(doc, assertion, &names.first_name)?;
            let last = first_attribute(doc, assertion, &names.last_name)?;
            match (first, last) {
                (Some(first), Some(last)) => Some(format!("{first} {last}")),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    };
    match name {
        Some(name) if has_control(&name) => Err(ResponseError::ValueContent),
        Some(name) => Ok(Some(name.chars().take(MAX_DISPLAY_NAME_CHARS).collect())),
        None => Ok(None),
    }
}

/// The trimmed, non-empty value of the first name in `names` that an `Attribute` of the
/// assertion's `AttributeStatement`s carries (D-SA-6). For each name, the first `Attribute` with
/// that name in document order decides it; an empty value counts as absent, and the next name in
/// the list is tried. Only attributes whose names are in `names` have their values read.
fn first_attribute(
    doc: &Document<'_>,
    assertion: NodeId,
    names: &[String],
) -> Result<Option<String>, ResponseError> {
    let attributes: Vec<NodeId> =
        children_named(doc, assertion, NS_ASSERTION, "AttributeStatement")
            .into_iter()
            .flat_map(|statement| children_named(doc, statement, NS_ASSERTION, "Attribute"))
            .collect();
    for name in names {
        let Some(&attribute) = attributes
            .iter()
            .find(|&&attribute| attr(doc, attribute, "Name") == Some(name.as_str()))
        else {
            continue;
        };
        let Some(value) = children_named(doc, attribute, NS_ASSERTION, "AttributeValue")
            .first()
            .copied()
        else {
            continue;
        };
        let text = strict_text(doc, value).ok_or(ResponseError::ValueContent)?;
        let trimmed = text.trim_ascii();
        if !trimmed.is_empty() {
            return Ok(Some(trimmed.to_owned()));
        }
    }
    Ok(None)
}

/// Whether `text` has a control character (`char::is_control`, which includes tab and newline).
fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::test_idp::{
        ACS_URL, NOW_NS, REQUEST_ID, ResponseOptions, SP_ENTITY_ID, Signing, TestKey, metadata_xml,
        response_b64,
    };

    /// One second, in nanoseconds.
    const SECOND_NS: i64 = 1_000_000_000;

    /// Validates the base64 `encoded` Response against the metadata of `keys`.
    fn run_encoded(
        encoded: &str,
        keys: &[TestKey],
        request_id: &str,
        now_ns: i64,
    ) -> Result<Assertion, ResponseError> {
        let idp = crate::parse_metadata(metadata_xml(keys).as_bytes())
            .expect("the test metadata is valid");
        let attributes = AttributeNames::default();
        let expected = ExpectedResponse {
            sp_entity_id: SP_ENTITY_ID,
            acs_url: ACS_URL,
            idp: &idp,
            request_id,
            now_ns,
            attributes: &attributes,
        };
        let received = decode_response(encoded)?;
        validate_response(&received, &expected)
    }

    /// Validates a Response built from `options`, signed with its own key.
    fn run_with(
        options: &ResponseOptions,
        keys: &[TestKey],
        request_id: &str,
        now_ns: i64,
    ) -> Result<Assertion, ResponseError> {
        run_encoded(&response_b64(options), keys, request_id, now_ns)
    }

    /// Validates a Response built from `options` against the default IdP key and clock.
    fn run(options: &ResponseOptions) -> Result<Assertion, ResponseError> {
        run_with(options, &[TestKey::Idp], REQUEST_ID, NOW_NS)
    }

    #[test]
    fn a_signed_assertion_is_accepted() {
        let assertion = run(&ResponseOptions::default()).expect("a valid Response is accepted");
        assert_eq!(assertion.name_id, "ada@example.com");
        assert_eq!(assertion.email.as_deref(), Some("ada@example.com"));
        assert_eq!(assertion.display_name.as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn a_signed_response_is_accepted() {
        let options = ResponseOptions {
            signing: Signing::Response,
            ..ResponseOptions::default()
        };
        run(&options).expect("a signed Response is accepted");
    }

    #[test]
    fn both_signatures_are_accepted() {
        let options = ResponseOptions {
            signing: Signing::Both,
            ..ResponseOptions::default()
        };
        run(&options).expect("a doubly signed Response is accepted");
    }

    #[test]
    fn an_ec_signature_is_accepted() {
        let options = ResponseOptions {
            key: TestKey::IdpEc,
            ..ResponseOptions::default()
        };
        run_with(&options, &[TestKey::IdpEc], REQUEST_ID, NOW_NS)
            .expect("an EC P-256 signature is accepted");
    }

    #[test]
    fn the_next_certificate_is_accepted_during_rotation() {
        let options = ResponseOptions {
            key: TestKey::IdpNext,
            ..ResponseOptions::default()
        };
        run_with(
            &options,
            &[TestKey::Idp, TestKey::IdpNext],
            REQUEST_ID,
            NOW_NS,
        )
        .expect("the next certificate is trusted during rotation");
    }

    #[test]
    fn base64_with_line_breaks_is_accepted() {
        let encoded = response_b64(&ResponseOptions::default());
        let wrapped = encoded
            .as_bytes()
            .chunks(76)
            .map(|line| std::str::from_utf8(line).expect("base64 is ASCII"))
            .collect::<Vec<_>>()
            .join("\r\n");
        run_encoded(&wrapped, &[TestKey::Idp], REQUEST_ID, NOW_NS)
            .expect("line breaks in the base64 are ignored");
    }

    #[test]
    fn skew_within_60_seconds_is_accepted() {
        // The NotOnOrAfter grace: the windows end at +300 s and the clock is at +359 s.
        let options = ResponseOptions {
            issue_instant_ns: NOW_NS + 300 * SECOND_NS,
            not_on_or_after_ns: NOW_NS + 300 * SECOND_NS,
            confirmation_not_on_or_after_ns: NOW_NS + 300 * SECOND_NS,
            ..ResponseOptions::default()
        };
        run_with(
            &options,
            &[TestKey::Idp],
            REQUEST_ID,
            NOW_NS + 359 * SECOND_NS,
        )
        .expect("NotOnOrAfter keeps a 59 s grace");

        // The NotBefore grace: the window opens at +59 s and the clock is at 0.
        let options = ResponseOptions {
            not_before_ns: NOW_NS + 59 * SECOND_NS,
            ..ResponseOptions::default()
        };
        run(&options).expect("NotBefore keeps a 59 s grace");
    }

    #[test]
    fn the_email_falls_back_to_an_email_name_id() {
        let options = ResponseOptions {
            attributes: vec![("displayName".to_owned(), vec!["Ada Lovelace".to_owned()])],
            ..ResponseOptions::default()
        };
        let assertion = run(&options).expect("a valid Response is accepted");
        assert_eq!(assertion.email.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn first_and_last_name_make_the_display_name() {
        let options = ResponseOptions {
            attributes: vec![
                ("firstName".to_owned(), vec!["Ada".to_owned()]),
                ("lastName".to_owned(), vec!["Lovelace".to_owned()]),
            ],
            ..ResponseOptions::default()
        };
        let assertion = run(&options).expect("a valid Response is accepted");
        assert_eq!(assertion.display_name.as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn a_long_display_name_is_truncated() {
        let options = ResponseOptions {
            attributes: vec![("displayName".to_owned(), vec!["é".repeat(300)])],
            ..ResponseOptions::default()
        };
        let assertion = run(&options).expect("a valid Response is accepted");
        assert_eq!(assertion.display_name, Some("é".repeat(256)));
    }

    #[test]
    fn unrelated_attributes_with_markup_are_ignored() {
        // A signed Response cannot carry markup in an attribute value without breaking its
        // signature, so the reader is tested on hand-written, unsigned XML.
        let xml = format!(
            "<saml:Assertion xmlns:saml=\"{NS_ASSERTION}\"><saml:AttributeStatement>\
             <saml:Attribute Name=\"other\"><saml:AttributeValue>x<!---->y</saml:AttributeValue></saml:Attribute>\
             <saml:Attribute Name=\"email\"><saml:AttributeValue>ada@example.com</saml:AttributeValue></saml:Attribute>\
             </saml:AttributeStatement></saml:Assertion>"
        );
        let doc = parse_strict(&xml).expect("well formed");
        let root = doc.document_element().expect("has a root element");
        assert_eq!(
            first_attribute(&doc, root, &["email".to_owned()]),
            Ok(Some("ada@example.com".to_owned()))
        );
    }

    #[test]
    fn a_persistent_name_id_is_accepted() {
        let options = ResponseOptions {
            name_id: "opaque-7f3a".to_owned(),
            name_id_format: Some("urn:oasis:names:tc:SAML:2.0:nameid-format:persistent".to_owned()),
            ..ResponseOptions::default()
        };
        let assertion = run(&options).expect("a persistent NameID is accepted");
        assert_eq!(assertion.name_id, "opaque-7f3a");
        assert_eq!(assertion.email.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn no_name_id_format_is_accepted() {
        let options = ResponseOptions {
            name_id_format: None,
            ..ResponseOptions::default()
        };
        let assertion = run(&options).expect("a NameID without a format is accepted");
        assert_eq!(assertion.name_id_format, None);
        assert_eq!(assertion.email.as_deref(), Some("ada@example.com"));
    }

    #[test]
    fn oversized_input_is_too_large() {
        assert_eq!(
            decode_response(&"A".repeat(MAX_ENCODED_BYTES + 1)).err(),
            Some(ResponseError::TooLarge)
        );
    }

    #[test]
    fn bad_base64_is_refused() {
        assert_eq!(decode_response("***").err(), Some(ResponseError::Base64));
    }

    #[test]
    fn non_utf8_is_refused() {
        let encoded = STANDARD.encode([0xff_u8, 0xfe_u8]);
        assert_eq!(decode_response(&encoded).err(), Some(ResponseError::Utf8));
    }

    #[test]
    fn a_non_response_root_is_refused() {
        let authn_request = format!(
            "<samlp:AuthnRequest xmlns:samlp=\"{NS_PROTOCOL}\" ID=\"{REQUEST_ID}\" Version=\"2.0\"/>"
        );
        assert_eq!(
            decode_response(&STANDARD.encode(authn_request)).err(),
            Some(ResponseError::NotAResponse)
        );
    }

    #[test]
    fn version_1_1_is_refused() {
        let options = ResponseOptions {
            version: "1.1".to_owned(),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Version));
    }

    #[test]
    fn a_failure_status_is_refused() {
        let options = ResponseOptions {
            status: "urn:oasis:names:tc:SAML:2.0:status:Requester".to_owned(),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Status));
    }

    #[test]
    fn an_unsigned_response_is_refused() {
        let options = ResponseOptions {
            signing: Signing::None,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::SignatureMissing));
    }

    #[test]
    fn the_attacker_key_is_refused() {
        let options = ResponseOptions {
            key: TestKey::Attacker,
            key_info: true,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::SignatureInvalid));
    }

    #[test]
    fn a_rotated_out_certificate_is_refused() {
        let options = ResponseOptions {
            key: TestKey::IdpNext,
            ..ResponseOptions::default()
        };
        assert_eq!(
            run_with(&options, &[TestKey::Idp], REQUEST_ID, NOW_NS).err(),
            Some(ResponseError::SignatureInvalid)
        );
    }

    #[test]
    fn a_wrong_destination_is_refused() {
        let options = ResponseOptions {
            destination: Some("https://cetra.test/other/acs".to_owned()),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Destination));
    }

    #[test]
    fn a_missing_response_in_response_to_is_refused() {
        let options = ResponseOptions {
            response_in_response_to: None,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::InResponseTo));
    }

    #[test]
    fn a_wrong_response_in_response_to_is_refused() {
        let options = ResponseOptions {
            response_in_response_to: Some("_ffffffffffffffffffffffffffffffff".to_owned()),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::InResponseTo));
    }

    #[test]
    fn a_wrong_confirmation_in_response_to_is_refused() {
        let options = ResponseOptions {
            confirmation_in_response_to: Some("_ffffffffffffffffffffffffffffffff".to_owned()),
            ..ResponseOptions::default()
        };
        assert_eq!(
            run(&options).err(),
            Some(ResponseError::SubjectConfirmation)
        );
    }

    #[test]
    fn a_wrong_recipient_is_refused() {
        let options = ResponseOptions {
            recipient: "https://cetra.test/other/acs".to_owned(),
            ..ResponseOptions::default()
        };
        assert_eq!(
            run(&options).err(),
            Some(ResponseError::SubjectConfirmation)
        );
    }

    #[test]
    fn an_expired_confirmation_is_refused() {
        let options = ResponseOptions {
            confirmation_not_on_or_after_ns: NOW_NS - 61 * SECOND_NS,
            ..ResponseOptions::default()
        };
        assert_eq!(
            run(&options).err(),
            Some(ResponseError::SubjectConfirmation)
        );
    }

    #[test]
    fn a_wrong_response_issuer_is_refused() {
        let options = ResponseOptions {
            response_issuer: Some("https://evil.test".to_owned()),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Issuer));
    }

    #[test]
    fn a_wrong_assertion_issuer_is_refused() {
        let options = ResponseOptions {
            assertion_issuer: "https://evil.test".to_owned(),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Issuer));
    }

    #[test]
    fn an_old_issue_instant_is_refused() {
        let options = ResponseOptions {
            issue_instant_ns: NOW_NS - 6 * 60 * SECOND_NS - SECOND_NS,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::IssueInstant));
    }

    #[test]
    fn a_future_issue_instant_is_refused() {
        let options = ResponseOptions {
            issue_instant_ns: NOW_NS + 61 * SECOND_NS,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::IssueInstant));
    }

    #[test]
    fn missing_conditions_are_refused() {
        let options = ResponseOptions {
            conditions: false,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Conditions));
    }

    #[test]
    fn not_yet_valid_conditions_are_refused() {
        let options = ResponseOptions {
            not_before_ns: NOW_NS + 61 * SECOND_NS,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Conditions));
    }

    #[test]
    fn expired_conditions_are_refused() {
        let options = ResponseOptions {
            not_on_or_after_ns: NOW_NS - 61 * SECOND_NS,
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Conditions));
    }

    #[test]
    fn a_wrong_audience_is_refused() {
        let options = ResponseOptions {
            audiences: vec!["https://other.test/sp".to_owned()],
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Audience));
    }

    #[test]
    fn no_audience_restriction_is_refused() {
        let options = ResponseOptions {
            audiences: vec![],
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::Audience));
    }

    #[test]
    fn a_commented_other_audience_is_ignored() {
        // A signed Response cannot be edited, so the helper is tested on hand-written XML.
        let xml = format!(
            "<saml:AudienceRestriction xmlns:saml=\"{NS_ASSERTION}\"><saml:Audience>x<!---->y</saml:Audience><saml:Audience>{SP_ENTITY_ID}</saml:Audience></saml:AudienceRestriction>"
        );
        let doc = parse_strict(&xml).expect("well formed");
        let root = doc.document_element().expect("has a root element");
        assert!(audience_matches(&doc, root, SP_ENTITY_ID));
    }

    #[test]
    fn a_transient_name_id_is_refused() {
        let options = ResponseOptions {
            name_id_format: Some("urn:oasis:names:tc:SAML:2.0:nameid-format:transient".to_owned()),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::NameIdTransient));
    }

    #[test]
    fn an_empty_name_id_is_refused() {
        let options = ResponseOptions {
            name_id: String::new(),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::NameIdMissing));
    }

    #[test]
    fn an_overlong_name_id_is_refused() {
        let options = ResponseOptions {
            name_id: "a".repeat(1025),
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::ValueContent));
    }

    #[test]
    fn an_overlong_email_is_refused() {
        let options = ResponseOptions {
            attributes: vec![("email".to_owned(), vec!["a".repeat(321)])],
            ..ResponseOptions::default()
        };
        assert_eq!(run(&options).err(), Some(ResponseError::ValueContent));
    }

    #[test]
    fn an_invalid_request_id_never_matches() {
        assert_eq!(
            run_with(&ResponseOptions::default(), &[TestKey::Idp], "x", NOW_NS).err(),
            Some(ResponseError::InResponseTo)
        );
    }

    #[test]
    fn debug_hides_the_payload() {
        let encoded = response_b64(&ResponseOptions::default());
        let received = decode_response(&encoded).expect("the Response decodes");
        assert!(!format!("{received:?}").contains("Ada"));

        let assertion = run(&ResponseOptions::default()).expect("a valid Response is accepted");
        let debug = format!("{assertion:?}");
        assert!(!debug.contains("ada@example.com"));
        assert!(!debug.contains("Ada"));
    }
}
