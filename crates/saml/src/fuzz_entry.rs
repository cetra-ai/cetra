// SPDX-License-Identifier: Elastic-2.0
//! Fuzz entry points (track SA M1, D-SA-15). Each one takes arbitrary bytes and must not panic.
#![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

use std::sync::OnceLock;

use crate::response::validate_unsigned;
use crate::{
    AttributeNames, ExpectedResponse, IdpCertificate, IdpMetadata, ReceivedResponse,
    decode_response, parse_metadata, validate_response,
};

// Copied from `test_idp`, which is not compiled under `fuzzing` alone.
const SP_ENTITY_ID: &str = "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001";
const ACS_URL: &str = "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs";
const REQUEST_ID: &str = "_0123456789abcdef0123456789abcdef";
const NOW_NS: i64 = 1_767_225_600_000_000_000;

/// The test IdP's metadata, parsed once: its entity ID, SSO URL and RSA certificate.
fn idp() -> &'static IdpMetadata {
    static IDP: OnceLock<IdpMetadata> = OnceLock::new();
    IDP.get_or_init(|| {
        // Harness setup, not input-dependent.
        let certificate =
            IdpCertificate::from_pem(include_str!("../../../fixtures/saml/idp-rsa.cert.pem"))
                .expect("the fixture certificate reads");
        IdpMetadata::from_fields(
            "https://idp.test/metadata",
            "https://idp.test/sso",
            vec![certificate],
        )
        .expect("the fixture metadata is valid")
    })
}

/// The settings the fuzz targets validate against: the test IdP's, with the attribute defaults.
fn expected(attributes: &AttributeNames) -> ExpectedResponse<'_> {
    ExpectedResponse {
        sp_entity_id: SP_ENTITY_ID,
        acs_url: ACS_URL,
        idp: idp(),
        request_id: REQUEST_ID,
        now_ns: NOW_NS,
        attributes,
    }
}

/// Validates arbitrary bytes as a decoded Response XML against the test IdP's settings (track SA
/// M1, D-SA-15). The bytes are also read as a `SAMLResponse` form value, so the base64 path is
/// reached from the same corpus (review M2).
pub fn saml_response(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = decode_response(text);
    let Ok(received) = ReceivedResponse::from_xml(text.to_owned()) else {
        return;
    };
    let attributes = AttributeNames::default();
    let _ = validate_response(&received, &expected(&attributes));
}

/// Validates arbitrary bytes as a Response XML with the signature checks left out, against the
/// same settings as `saml_response`, so mutated inputs reach the NameID, confirmation, conditions
/// and attribute checks (review M2).
pub fn saml_assertion(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(received) = ReceivedResponse::from_xml(text.to_owned()) else {
        return;
    };
    let attributes = AttributeNames::default();
    let _ = validate_unsigned(&received, &expected(&attributes));
}

/// Parses arbitrary bytes as IdP metadata (track SA M1, D-SA-15).
pub fn saml_metadata(data: &[u8]) {
    let _ = parse_metadata(data);
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::test_idp::{ResponseOptions, Signing, TestKey, metadata_xml, response_xml};
    use crate::xml::{CM_BEARER, CM_HOLDER_OF_KEY, NAMEID_PERSISTENT};

    /// Runs `entry` on every file of `fuzz/corpus/<target>/` and
    /// `fuzz/regressions/<target>/`. The corpus must not be empty.
    fn replay(target: &str, entry: fn(&[u8])) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
        let corpus = sorted_files(&root.join("corpus").join(target));
        assert!(!corpus.is_empty(), "no corpus for {target}");
        let regressions = sorted_files(&root.join("regressions").join(target));
        for path in corpus.into_iter().chain(regressions) {
            let data = std::fs::read(&path).expect("read a corpus file");
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(&data)));
            if outcome.is_err() {
                panic!("{target}: {} panicked", path.display());
            }
        }
    }

    /// The files in `dir`, sorted by name. A missing directory has none.
    fn sorted_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .map(|entry| entry.expect("read a corpus entry").path())
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        files
    }

    #[test]
    fn saml_response_corpus_replays() {
        replay("saml_response", saml_response);
    }

    #[test]
    fn saml_metadata_corpus_replays() {
        replay("saml_metadata", saml_metadata);
    }

    #[test]
    fn saml_assertion_corpus_replays() {
        replay("saml_assertion", saml_assertion);
    }

    /// Writes the seed files of the SAML targets (track SA M1, `just fuzz-seed`): decoded
    /// Response XML into `fuzz/corpus/saml_response/`, IdP metadata into `saml_metadata/`, and
    /// unsigned Responses into `saml_assertion/`.
    #[test]
    #[ignore]
    fn write_seed_corpus() {
        let write = |target: &str, name: &str, bytes: &[u8]| {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fuzz/corpus")
                .join(target);
            std::fs::create_dir_all(&dir).expect("create the corpus directory");
            std::fs::write(dir.join(format!("seed-{name}")), bytes).expect("write a seed");
        };

        let assertion_only = ResponseOptions::default();
        let responses: Vec<(&str, ResponseOptions)> = vec![
            ("valid-assertion", assertion_only.clone()),
            (
                "valid-response",
                ResponseOptions {
                    signing: Signing::Response,
                    ..ResponseOptions::default()
                },
            ),
            (
                "valid-both",
                ResponseOptions {
                    signing: Signing::Both,
                    ..ResponseOptions::default()
                },
            ),
            (
                "ec",
                ResponseOptions {
                    key: TestKey::IdpEc,
                    ..ResponseOptions::default()
                },
            ),
            (
                "unsigned",
                ResponseOptions {
                    signing: Signing::None,
                    ..ResponseOptions::default()
                },
            ),
            (
                "wrong-audience",
                ResponseOptions {
                    audiences: vec!["https://other.test/sp".to_owned()],
                    ..ResponseOptions::default()
                },
            ),
        ];
        for (name, options) in &responses {
            write("saml_response", name, response_xml(options).as_bytes());
        }

        // The default Response with its Assertion duplicated before the closing tag.
        let xml = response_xml(&assertion_only);
        let start = xml
            .find("<saml:Assertion")
            .expect("the Response has an Assertion");
        let end = xml
            .rfind("</saml:Assertion>")
            .expect("the Response has an Assertion")
            + "</saml:Assertion>".len();
        let duplicated = xml.replace(
            "</samlp:Response>",
            &format!("{}</samlp:Response>", &xml[start..end]),
        );
        write("saml_response", "two-assertions", duplicated.as_bytes());

        let one_cert = metadata_xml(&[TestKey::Idp]);
        write("saml_metadata", "one-cert", one_cert.as_bytes());
        let two_certs = metadata_xml(&[TestKey::Idp, TestKey::IdpNext]);
        write("saml_metadata", "two-certs", two_certs.as_bytes());
        let no_redirect = one_cert.replace("HTTP-Redirect", "HTTP-Artifact");
        write("saml_metadata", "no-redirect", no_redirect.as_bytes());

        // Unsigned Responses for `saml_assertion`, the content checks without the signatures.
        let unsigned = |options: ResponseOptions| {
            response_xml(&ResponseOptions {
                signing: Signing::None,
                ..options
            })
        };
        write(
            "saml_assertion",
            "unsigned",
            unsigned(ResponseOptions::default()).as_bytes(),
        );
        let persistent = unsigned(ResponseOptions {
            name_id_format: Some(NAMEID_PERSISTENT.to_owned()),
            ..ResponseOptions::default()
        });
        write(
            "saml_assertion",
            "persistent-name-id",
            persistent.as_bytes(),
        );
        let four_attributes = unsigned(ResponseOptions {
            attributes: vec![
                ("email".to_owned(), vec!["ada@example.com".to_owned()]),
                ("displayName".to_owned(), vec!["Ada Lovelace".to_owned()]),
                ("firstName".to_owned(), vec!["Ada".to_owned()]),
                ("lastName".to_owned(), vec!["Lovelace".to_owned()]),
            ],
            ..ResponseOptions::default()
        });
        write("saml_assertion", "attributes", four_attributes.as_bytes());

        // The bearer confirmation with a copy of it first, whose method is holder-of-key.
        let base = unsigned(ResponseOptions::default());
        let start = base
            .find("<saml:SubjectConfirmation ")
            .expect("the Response has a SubjectConfirmation");
        let close = "</saml:SubjectConfirmation>";
        let end = start + base[start..].find(close).expect("the confirmation closes") + close.len();
        let confirmation = &base[start..end];
        let holder = confirmation.replacen(
            &format!("Method=\"{CM_BEARER}\""),
            &format!("Method=\"{CM_HOLDER_OF_KEY}\""),
            1,
        );
        let two = base.replacen(confirmation, &format!("{holder}{confirmation}"), 1);
        write("saml_assertion", "two-confirmations", two.as_bytes());
    }
}
