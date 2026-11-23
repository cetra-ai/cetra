# SA · M1 — the `cetra-saml` crate

Milestone 1 of track SA ([`README.md`](README.md)). It builds `crates/saml`, a library crate that the server (M2) and the control plane (track Q, by git) use to act as a SAML 2.0 Service Provider. It has no server, storage or UI change.

When M1 is done:
- `cetra-saml` parses IdP metadata, builds the SP-initiated AuthnRequest redirect URL and the SP metadata XML, and validates a Response to the contract of D-SA-9;
- a dev-only test IdP (feature `test-idp`) signs valid and hostile Responses for this crate's tests and, from M2, for the server's;
- a hostile corpus covers signature wrapping, comment injection, DTDs, `KeyInfo` substitution, weak algorithms and every binding check;
- two fuzz targets (`saml_response`, `saml_metadata`) run in the nightly job, and their corpora replay in `just test`;
- the crate is `ee` (Elastic-2.0, D-SA-14), and `cargo deny` passes.

## Prerequisites

- The design doc ([`README.md`](README.md)) is approved (2026-11-23).
- The implementation runs in its own worktree and branch, cut from the track branch `saml-design` (or from `main` once the design PR is merged).

## Read first

1. `AGENTS.md`: Rules (errors, no `unwrap`, privacy), Dependencies, Layout.
2. [`README.md`](README.md): § Architecture → Parts 1 and § Contracts; decisions D-SA-8, D-SA-9, D-SA-11, D-SA-13, D-SA-14, D-SA-15.
3. The precedent for a shared Elastic-2.0 crate: `crates/token/Cargo.toml`, `crates/token/LICENSE`, `crates/token/src/lib.rs` (error enum style, `Debug` that hides payload).
4. The fuzz precedent: `crates/otel/src/fuzz_entry.rs` (entry points, `replay`, `write_seed_corpus`), `fuzz/Cargo.toml`, `fuzz/fuzz_targets/otlp_grpc.rs`, the `fuzz*` recipes and `_fuzz-max-len` in `justfile`, and the `fuzz` matrix in `.github/workflows/nightly.yml`.
5. The `bergshamra` 0.12.0 API used here: `bergshamra::dsig::verify::verify_all_document`, `bergshamra::dsig::sign::sign`, `bergshamra::DsigContext::{new, new_permissive}`, `bergshamra::KeysManager::{new, add_key}`, `bergshamra::keys::loader::{load_x509_cert_der, load_pem_auto}`, `bergshamra::VerifyResult`, `bergshamra::xml::uppsala::{Parser, Document, NodeId, NodeKind}`. Their source is in `~/.cargo/registry/src/*/bergshamra-*-0.12.0/` and `uppsala-0.12.1/` once fetched. The module docs of `bergshamra-dsig/src/lib.rs` explain the strict and trusted-keys-only modes this crate relies on.

## What it is, and what it is not

**It is:** a pure library. Every function takes bytes and explicit inputs (the clock, the expected values) and returns a value or a typed error. No I/O, no HTTP, no randomness, no logging.

**It is not:**
- the server's SAML providers, storage, routes, pending-request table or cookie (M2);
- the settings page and docs (M3);
- the security review and interop (M4);
- the control plane's federation (track Q; the request is written in SA1.8).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-SA-8 | No SP keys: unsigned AuthnRequests, `EncryptedAssertion` refused; our SP layer on `bergshamra` 0.12.0, default features off. |
| D-SA-9 | The response validation contract (rules 1–19), implemented in SA1.5 and tested in SA1.5–SA1.6. |
| D-SA-11 | Metadata: 1–4 signing certificates, `https` SSO URL without userinfo, `validUntil` ignored, 1 MiB. |
| D-SA-12 | SP entity ID `<public_url>/api/auth/saml/<org_id>`, ACS `…/acs`; SP metadata without keys, NameID format `unspecified`. |
| D-SA-13 | The subject is the NameID's exact text; every format except `transient`. |
| D-SA-14 | `crates/saml` is Elastic-2.0 with no workspace dependency. |
| D-SA-15 | The hostile corpus and a fuzz target. |
| D-SA-16 | No SLO, Artifact, ECP: the crate implements none of them. |

Answers the user gave that this plan relies on (2026-11-23): bergshamra rather than saml-rs; no encrypted assertions; SP-initiated only; internal review only.

### Two refinements of D-SA-9 made by this plan

Both are recorded in `README.md` and `docs/mvp-decisions.md` in the same commit as this plan:
- **Rule 2:** "one parser". The Response is parsed twice with the same parser and configuration: once to read `InResponseTo` (so the server can look up the pending entry, D-SA-10), once to validate. A deterministic parser gives the same tree both times. The rule's point is that no second XML parser ever reads the message.
- **Rule 13:** `InResponseTo` is **required** on the Response, not "when present". A Response without it is unsolicited and never reaches validation (D-SA-5).

## Versions

Looked up on 2026-11-23 with `cargo info` and a probe crate. The implementer runs `cargo info <crate>` again and reports the versions used.

| Crate | Version | Where | Why |
|---|---|---|---|
| `bergshamra` | `=0.12.0`, `default-features = false`, `features = ["rustcrypto"]` | root `[workspace.dependencies]`, used by `crates/saml` | XML parser (uppsala 0.12.1), exc-c14n, XML-DSig sign and verify (D-SA-8). Pinned exactly: a young crate whose minors may change behaviour. Licence BSD-2-Clause (allowed in `deny.toml`). |
| `x509-cert` | `0.2.5`, `default-features = false` | root `[workspace.dependencies]` | reads a certificate's validity dates for display (D-SA-11). Already in the graph through bergshamra; same version. Licence Apache-2.0 OR MIT. |
| `rsa` | workspace (`0.10.0`) | `crates/saml` | the RSA key-size check (2048 bits minimum). Already a workspace dependency. |
| `base64`, `chrono`, `flate2`, `thiserror`, `url`, `sha2`, `hex` | workspace | `crates/saml` | already pinned in the root. |

The probe (bergshamra with default features off and `rustcrypto`) has 256 packages, no C code, no OpenSSL. It brings pre-release pins through kryptering: `ed448-goldilocks 0.14.0-pre.15`, `x448 0.14.0-pre.12`, `slh-dsa 0.2.0-rc.5`. `deny.toml` does not ban pre-releases. It also brings second versions of `der`, `sha2`, `base64`, `rand` and `getrandom`, which `multiple-versions = "warn"` accepts. None of them is in the DataFusion family. SA1.8 records the real footprint.

## Phases and dependencies

The phases run **in order**: each one builds on the previous phase's modules, and every phase edits `crates/saml/src/lib.rs`. No two run in parallel.

| Phase | Edits | Depends on |
|---|---|---|
| SA1.1 Crate, fixtures, XML helpers | root `Cargo.toml`, `Cargo.lock`, `LICENSE`, `AGENTS.md`, `justfile` (`check-ee`), `crates/saml/{Cargo.toml,LICENSE,src/lib.rs,src/xml.rs}`, `fixtures/saml/*` | — |
| SA1.2 Certificates and IdP metadata | `crates/saml/src/{lib.rs,cert.rs,metadata.rs}` | SA1.1 |
| SA1.3 AuthnRequest and SP metadata | `crates/saml/src/{lib.rs,request.rs}` | SA1.1 |
| SA1.4 Test IdP | `crates/saml/{Cargo.toml,src/lib.rs,src/test_idp.rs}` | SA1.2 |
| SA1.5 Response validation | `crates/saml/src/{lib.rs,response.rs}` | SA1.2, SA1.4 |
| SA1.6 Hostile corpus | `crates/saml/src/{lib.rs,hostile_tests.rs}` | SA1.5 |
| SA1.7 Fuzz targets | `crates/saml/src/{lib.rs,fuzz_entry.rs}`, `fuzz/Cargo.toml`, `fuzz/fuzz_targets/{saml_response,saml_metadata}.rs`, `justfile`, `.github/workflows/nightly.yml` | SA1.5 |
| SA1.8 Checks, seeds, footprint, request to Q (coordinating session) | anything the checks require, `fuzz/corpus/saml_*/`, `README.md`, `docs/roadmap.md`, the shared notes folder | all |

## Conventions for every phase

- **No checks while implementing (the user, 2026-11-20).** Do not run `cargo build`, `cargo test`, `cargo clippy` or `just check`. Write the code and tests, commit with the phase's subject, and stop. The coordinating session runs the full checks once, in SA1.8, and fixes what fails. `cargo fetch` and `cargo info` are allowed. In SA1.1, `cargo metadata --format-version 1 > /dev/null` updates `Cargo.lock` without compiling.
- Every `.rs` file under `crates/saml/` starts with exactly `// SPDX-License-Identifier: Elastic-2.0` as its first line (`just check-ee`).
- No `unwrap()` / `expect()` outside `#[cfg(test)]` code and the `test_idp` module, unless a comment next to it states the invariant. `test_idp` is test support: it starts with `#![allow(clippy::expect_used, clippy::unwrap_used)] // test support, never built in release` as its first attribute after the header and doc comment.
- **Privacy.** No error message or `Debug` output contains a NameID, an email, an attribute value, an IdP entity ID or URL, or certificate bytes. The SP's own entity ID and ACS URL (cetra's URLs, not payload) may appear in `Debug` of `AuthnRequest` and `ExpectedResponse`. Errors are unit variants with static messages. Types that hold such values implement `Debug` by hand, printing only lengths, counts or the certificate's SHA-256 hex.
- Namespaces and URIs are spelled out once, as the constants of SA1.1. No other spelling appears in the code.
- Public items get a one-line doc comment. `unreachable_pub` is a workspace lint: items used only inside the crate are `pub(crate)`.
- Times are `i64` nanoseconds since the Unix epoch, UTC (`AGENTS.md` § Data conventions).

---

## SA1.1 — Crate, fixtures, XML helpers

**Goal:** create `cetra-saml` as an `ee` crate with its fixture certificates and the strict XML helpers every later phase uses.

**Edit:**
- `Cargo.toml` (root): two workspace dependencies, one workspace crate entry.
- `Cargo.lock` (updated by `cargo metadata`, not by hand).
- `LICENSE` (root): the `ee` directory list.
- `AGENTS.md`: § Layout and § Crate dependency graph.
- `justfile`: recipe `check-ee`.
- `crates/saml/Cargo.toml`, `crates/saml/LICENSE`, `crates/saml/src/lib.rs`, `crates/saml/src/xml.rs` (new).
- `fixtures/saml/README.md`, and the PEM files listed in step 5 (new).

**Steps:**

1. Root `Cargo.toml`, `[workspace.dependencies]`:
   - Under `# workspace crates`, after `cetra-token = …`, add `cetra-saml = { path = "crates/saml" }`.
   - After the `base64 = "0.23.1"` line, add:
     ```toml
     # SAML 2.0 SP (track SA, D-SA-8): XML parser, exc-c14n and XML-DSig. Exact pin: a young crate.
     bergshamra = { version = "=0.12.0", default-features = false, features = ["rustcrypto"] }
     # Certificate validity dates for SAML IdP certificates (D-SA-11). Already in the graph via bergshamra.
     x509-cert = { version = "0.2.5", default-features = false }
     ```
2. `crates/saml/Cargo.toml`:
   ```toml
   [package]
   name = "cetra-saml"
   description = "SAML 2.0 service provider: IdP metadata, AuthnRequest, response validation (track SA, D-SA-8)"
   version.workspace = true
   edition.workspace = true
   rust-version.workspace = true
   publish.workspace = true
   license = "Elastic-2.0"

   [dependencies]
   base64.workspace = true
   bergshamra.workspace = true
   chrono.workspace = true
   flate2.workspace = true
   hex.workspace = true
   rsa.workspace = true
   sha2.workspace = true
   thiserror.workspace = true
   url.workspace = true
   x509-cert.workspace = true

   [lints]
   workspace = true
   ```
   (SA1.4 adds the `[features]` table.)
3. `crates/saml/LICENSE`: a byte-for-byte copy of `crates/token/LICENSE` (`cp crates/token/LICENSE crates/saml/LICENSE`). `just check-ee` compares its SHA-256.
4. Root `LICENSE`: add the line `- crates/saml/` after `- crates/token/`, Insert `- crates/saml/` **before** `- crates/server/src/ee/`.
5. Fixture certificates, generated once with the OpenSSL command-line tool from `fixtures/saml/`. It is a dev tool, not a dependency. They are test keys, never trusted by anything else:
   ```sh
   mkdir -p fixtures/saml && cd fixtures/saml
   openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 36500 -subj "/CN=cetra test idp" -keyout idp-rsa.key.pem -out idp-rsa.cert.pem
   openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 36500 -subj "/CN=cetra test idp next" -keyout idp-rsa-next.key.pem -out idp-rsa-next.cert.pem
   openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -sha256 -nodes -days 36500 -subj "/CN=cetra test idp ec" -keyout idp-ec.key.pem -out idp-ec.cert.pem
   openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 36500 -subj "/CN=attacker" -keyout attacker-rsa.key.pem -out attacker-rsa.cert.pem
   openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-384 -sha384 -nodes -days 36500 -subj "/CN=cetra test idp p384" -keyout idp-ec384.key.pem -out idp-ec384.cert.pem
   openssl req -x509 -newkey rsa:1024 -sha256 -nodes -days 36500 -subj "/CN=weak" -keyout weak-rsa-1024.key.pem -out weak-rsa-1024.cert.pem
   ```
   If the `rsa:1024` line is refused by the local OpenSSL policy, run it with `-config /dev/null`. If it still fails, stop and report. `fixtures/saml/README.md` gets one short paragraph:
   - these are test keys for `cetra-saml`'s test IdP (track SA);
   - the commands above regenerate them;
   - `weak-rsa-1024` exists to be refused;
   - `attacker-rsa` is never in a trusted set;
   - none of them protects anything.
6. `justfile`, recipe `check-ee`:
   - in `for dir in crates/server/src/ee web/src/ee tools/license crates/token; do`, append ` crates/saml`;
   - in the `case` pattern `crates/server/src/ee/*|web/src/ee/*|tools/license/*|crates/token/*)`, append `|crates/saml/*`.
7. `AGENTS.md`:
   - In the Layout block, add after the `token/` line, aligned like it:
     `  saml/      cetra-saml     SAML 2.0 SP: IdP metadata, AuthnRequest, response validation (Elastic-2.0, D-SA-14)`
   - In the same block, after the `fixtures/api/` line, add:
     `fixtures/saml/               test IdP keys and certificates for cetra-saml (track SA)`
   - In "Crate dependency graph (enforced)", after the `token` bullet, add:
     ``- `saml` depends on no workspace crate. `server` uses it from track SA's M2, and so does the control plane (by git commit).``
8. `crates/saml/src/lib.rs`:
   ```rust
   // SPDX-License-Identifier: Elastic-2.0
   //! SAML 2.0 service provider for per-org single sign-on (track SA, `docs/plans/sa-saml/`).
   //!
   //! Pure functions over bytes and explicit inputs: no I/O, no clock, no randomness, no logging.
   //! The server (M2) and the control plane (by git) share it.

   pub(crate) mod xml;
   ```
   Later phases add their `mod` lines and `pub use` re-exports here.
9. `crates/saml/src/xml.rs`: the constants and helpers below. Signatures and constants are exact; the bodies follow the numbered rules.
   ```rust
   // SPDX-License-Identifier: Elastic-2.0
   //! Strict XML reading shared by metadata and response parsing (D-SA-9 rules 1, 2, 18).

   use bergshamra::xml::uppsala::{Document, NodeId, NodeKind, Parser};

   pub(crate) const NS_PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
   pub(crate) const NS_ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
   pub(crate) const NS_METADATA: &str = "urn:oasis:names:tc:SAML:2.0:metadata";
   pub(crate) const NS_DSIG: &str = "http://www.w3.org/2000/09/xmldsig#";

   pub(crate) const BINDING_REDIRECT: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect";
   pub(crate) const BINDING_POST: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST";
   pub(crate) const STATUS_SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
   pub(crate) const CM_BEARER: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";
   pub(crate) const NAMEID_UNSPECIFIED: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified";
   pub(crate) const NAMEID_EMAIL: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress";
   pub(crate) const NAMEID_TRANSIENT: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:transient";
   pub(crate) const NAMEID_ENTITY: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:entity";

   pub(crate) const C14N_EXCLUSIVE: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
   pub(crate) const C14N_EXCLUSIVE_COMMENTS: &str = "http://www.w3.org/2001/10/xml-exc-c14n#WithComments";
   pub(crate) const TRANSFORM_ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
   pub(crate) const SIG_RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
   pub(crate) const SIG_RSA_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha384";
   pub(crate) const SIG_RSA_SHA512: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512";
   pub(crate) const SIG_ECDSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256";
   pub(crate) const SIG_ECDSA_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha384";
   pub(crate) const SIG_RSA_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#rsa-sha1";
   pub(crate) const DIGEST_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
   pub(crate) const DIGEST_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#sha384";
   pub(crate) const DIGEST_SHA512: &str = "http://www.w3.org/2001/04/xmlenc#sha512";
   pub(crate) const DIGEST_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#sha1";

   pub(crate) const MAX_DEPTH: u32 = 32;
   pub(crate) const MAX_ELEMENTS: usize = 10_000;
   pub(crate) const MAX_ATTRIBUTES: usize = 64;

   /// Why strict parsing refused a document.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum ParseFailure {
       /// A `<!DOCTYPE` or `<!ENTITY` declaration (checked on the bytes, before parsing).
       Doctype,
       /// Not well-formed, too deep, too many elements, or too many attributes on one element.
       Xml,
   }

   /// Parses `text` with the one parser configuration every read uses (rule 2).
   pub(crate) fn parse_strict(text: &str) -> Result<Document<'_>, ParseFailure>;

   /// The concatenated text of `node`'s children. Refuses (`None`) when a child is an element,
   /// a comment or a processing instruction (rule 18). Text and CDATA children are concatenated
   /// in document order; no trimming.
   pub(crate) fn strict_text(doc: &Document<'_>, node: NodeId) -> Option<String>;

   /// The child elements of `parent` named `local` in namespace `ns`, in document order.
   pub(crate) fn children_named(doc: &Document<'_>, parent: NodeId, ns: &str, local: &str) -> Vec<NodeId>;

   /// The only child element named `local` in `ns`; `None` when there are zero or several.
   pub(crate) fn only_child(doc: &Document<'_>, parent: NodeId, ns: &str, local: &str) -> Option<NodeId>;

   /// Every descendant element of `root` (excluding `root`) named `local` in `ns`, in document order.
   pub(crate) fn descendants_named(doc: &Document<'_>, root: NodeId, ns: &str, local: &str) -> Vec<NodeId>;

   /// Whether `node` is an element named `local` in `ns`.
   pub(crate) fn is_element(doc: &Document<'_>, node: NodeId, ns: &str, local: &str) -> bool;

   /// An attribute of `node` without a namespace (SAML attributes are unqualified).
   pub(crate) fn attr<'d>(doc: &'d Document<'_>, node: NodeId, local: &str) -> Option<&'d str>;

   /// Escapes an attribute value: `&` → `&amp;`, `<` → `&lt;`, `>` → `&gt;`, `"` → `&quot;`, `'` → `&apos;` (`&` first).
   pub(crate) fn escape_attr(value: &str) -> String;

   /// Escapes element text: `&`, `<`, `>` as in `escape_attr` (`&` first).
   pub(crate) fn escape_text(value: &str) -> String;
   ```
   Some constants are first used in a later phase. Add `#[allow(dead_code)] // used by later phases or by tests only` above each constant and function that SA1.1 does not use itself. Leave these allows in place: some constants (`SIG_RSA_SHA1`, `DIGEST_SHA1`) are used only by tests, and later phases do not edit `xml.rs`. SA1.8 removes the ones clippy reports as unneeded (`clippy::allow_attributes` is not enabled, so leftovers do not fail).
   `parse_strict` rules:
   1. If `text` contains `<!DOCTYPE` or `<!ENTITY` (case-sensitive byte search; the XML grammar is case-sensitive), return `Doctype`.
   2. Parse with `Parser::new().with_forbid_dtd(true).with_forbid_entities(true).with_max_depth(MAX_DEPTH).parse(text)`. A parse error is `Xml`.
   3. Walk every node once, iteratively with a `Vec<NodeId>` stack starting at `doc.root()` using `doc.children(id)`, never recursively. Count `NodeKind::Element` nodes: more than `MAX_ELEMENTS` is `Xml`. For each element, `element.attributes.len() > MAX_ATTRIBUTES` is `Xml`.
   4. Return the document.

   `is_element` uses `doc.element(node)` and `element.name.matches(Some(ns), local)` (`QName::matches`). `attr` returns the value of the first attribute whose `name.local_name == local` and whose `name.namespace_uri` is `None`. Check the `QName` field names in `uppsala-0.12.1/src/dom.rs` and use them as they are. `descendants_named` uses `doc.descendants(root)` and filters with `is_element`.

**Tests** (`#[cfg(test)] mod tests` in `xml.rs`):
- `a_doctype_is_refused_before_parsing`: `<!DOCTYPE r><r/>` → `Doctype`.
- `an_entity_declaration_is_refused`: text containing `<!ENTITY x "y">` → `Doctype`.
- `ill_formed_xml_is_refused`: `<a><b></a>` → `Xml`.
- `depth_above_the_limit_is_refused`: 33 nested `<a>` → `Xml`; 32 nested → `Ok`.
- `too_many_elements_are_refused`: a root with 10 000 empty children (10 001 elements) → `Xml`; 9 999 children → `Ok`.
- `too_many_attributes_are_refused`: one element with 65 attributes → `Xml`; 64 → `Ok`.
- `strict_text_concatenates_text_and_cdata`: `<a>x<![CDATA[y]]>z</a>` → `Some("xyz")`.
- `strict_text_refuses_a_comment`: `<a>user@evil.com<!---->.attacker.com</a>` → `None`.
- `strict_text_refuses_a_child_element_and_a_pi`: `<a>x<b/></a>` → `None`; `<a>x<?p q?></a>` → `None`.
- `strict_text_keeps_whitespace`: `<a> x </a>` → `Some(" x ")`.
- `only_child_is_none_for_two`: two matching children → `None`.
- `attr_ignores_namespaced_attributes`: `<a xmlns:p="u" p:ID="1"/>` → `attr(.., "ID")` is `None`.
- `escaping_covers_the_five_characters`: `escape_attr("&<>\"'")` → `"&amp;&lt;&gt;&quot;&apos;"`; `escape_text("a&b<c>")` → `"a&amp;b&lt;c&gt;"`.

**Done when:** the files above are written and committed. No build or test is run (§ Conventions). `cargo metadata --format-version 1 > /dev/null` has updated `Cargo.lock`, and the diff of `Cargo.lock` adds bergshamra 0.12.0 and its dependencies.

**Commit:** `Add the cetra-saml crate with strict XML helpers`

---

## SA1.2 — Certificates and IdP metadata

**Goal:** parse and check IdP signing certificates and IdP metadata (D-SA-11).

**Edit:** `crates/saml/src/lib.rs`, `crates/saml/src/cert.rs` (new), `crates/saml/src/metadata.rs` (new).

**Steps:**

1. `lib.rs`: add `mod cert; mod metadata;` and `pub use cert::IdpCertificate; pub use metadata::{IdpMetadata, MAX_CERTIFICATES, MAX_METADATA_BYTES, MetadataError, parse_metadata};`.
2. `metadata.rs` holds the error enum, also used by `cert.rs`:
   ```rust
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
       #[error("the entity ID is missing, empty, longer than 1024 characters or contains control characters")]
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
   ```
3. `cert.rs`:
   ```rust
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
       pub fn from_der(der: &[u8]) -> Result<Self, MetadataError>;
       /// Reads a PEM `CERTIFICATE` block, or bare base64 of the DER (as IdP consoles show it).
       pub fn from_pem(text: &str) -> Result<Self, MetadataError>;
       /// The DER bytes.
       pub fn der(&self) -> &[u8];
       /// The SHA-256 of the DER, lowercase hex: the certificate's display and dedup identity.
       pub fn sha256_hex(&self) -> String;
       /// `notBefore`, nanoseconds since the epoch.
       pub fn not_before_ns(&self) -> i64;
       /// `notAfter`, nanoseconds since the epoch. Shown and warned about, never enforced (D-SA-11).
       pub fn not_after_ns(&self) -> i64;
       /// The bergshamra key for verification.
       pub(crate) fn key(&self) -> Result<bergshamra::Key, MetadataError>;
   }

   impl std::fmt::Debug for IdpCertificate {
       fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
           f.debug_struct("IdpCertificate").field("sha256", &self.sha256_hex()).finish_non_exhaustive()
       }
   }
   ```
   `from_der` steps:
   1. A DER longer than 16 384 bytes is `Certificate`.
   2. `x509_cert::Certificate::from_der(der)` (trait `x509_cert::der::Decode`); an error is `Certificate`.
   3. `not_before_ns` and `not_after_ns` come from `cert.tbs_certificate.validity.not_before` and `.not_after`, via `.to_unix_duration()` → `as_nanos()` → `i64::try_from`. An overflow is `Certificate`.
   4. `bergshamra::keys::loader::load_x509_cert_der(der)`; an error is `Certificate`. Then match `key.data.algorithm_name()`:
      - `"RSA"`: take `key.data.to_spki_der()` (`None` is `Certificate`), `rsa::PublicKey::from_public_key_der(&spki)` (trait `rsa::pkcs8::DecodePublicKey`; in rsa 0.10 the type is `PublicKey`, and `RsaPublicKey` is a deprecated alias that clippy refuses; an error, including a key above 8192 bits, is `Certificate`), and require `rsa::traits::PublicKeyParts::size(&public) >= 256` bytes, else `WeakKey`.
      - `"EC-P256"` | `"EC-P384"`: accepted.
      - Anything else is `WeakKey`.
   5. `sha256` = `sha2::Sha256::digest(der)`.

   `from_pem` steps:
   1. Trim ASCII whitespace.
   2. If the text starts with `-----BEGIN CERTIFICATE-----`, it must contain `-----END CERTIFICATE-----` after it, else `Certificate`. Keep only the text between the markers.
   3. Remove every ASCII whitespace character, decode with `base64::engine::general_purpose::STANDARD` (an error is `Certificate`), and call `from_der`.

   `key()` calls `load_x509_cert_der(&self.der)` again and maps an error to `Certificate`.
4. `metadata.rs`:
   ```rust
   /// What cetra keeps from IdP metadata (D-SA-11).
   #[derive(Clone, PartialEq, Eq)]
   pub struct IdpMetadata {
       entity_id: String,
       sso_url: String,
       certificates: Vec<IdpCertificate>,
   }

   impl IdpMetadata {
       /// Builds metadata from manual fields, with the same checks as parsing.
       pub fn from_fields(entity_id: &str, sso_url: &str, certificates: Vec<IdpCertificate>) -> Result<Self, MetadataError>;
       /// The IdP entity ID (the `Issuer` of its responses).
       pub fn entity_id(&self) -> &str;
       /// The single sign-on URL for the HTTP-Redirect binding.
       pub fn sso_url(&self) -> &str;
       /// The trusted signing certificates, 1 to 4, distinct.
       pub fn certificates(&self) -> &[IdpCertificate];
   }

   /// `Debug` prints lengths and certificate hashes only.
   impl std::fmt::Debug for IdpMetadata { /* entity_id_len, sso_url_len, certificates */ }

   /// The largest metadata document accepted, in bytes.
   pub const MAX_METADATA_BYTES: usize = 1024 * 1024;
   /// The most signing certificates kept (D-SA-11).
   pub const MAX_CERTIFICATES: usize = 4;

   /// Parses IdP metadata XML (D-SA-11).
   pub fn parse_metadata(xml: &[u8]) -> Result<IdpMetadata, MetadataError>;
   ```
   Shared checks, `check_entity_id(&str)`:
   - The value is used as is, not trimmed.
   - It must be non-empty, at most 1024 characters, with no `char::is_control` character. Otherwise `EntityId`.

   Shared checks, `check_sso_url(&str)`:
   - `url::Url::parse`; an error is `SsoUrl`.
   - Scheme must be `https`, `username()` empty, `password()` `None`, `fragment()` `None`, and the length at most 2048. Otherwise `SsoUrl`.

   Shared certificate list step, `dedup_certificates(Vec<IdpCertificate>)`:
   - Keep the first of each `sha256`, in order.
   - Empty → `NoSigningCertificate`; more than `MAX_CERTIFICATES` → `TooManyCertificates`.

   `from_fields` runs the three in that order.

   `parse_metadata` steps:
   1. `xml.len() > MAX_METADATA_BYTES` → `TooLarge`; `std::str::from_utf8` error → `Utf8`. A leading UTF-8 BOM (`\u{feff}`) is stripped.
   2. `xml::parse_strict`: `Doctype` → `Doctype`, `Xml` → `Xml`.
   3. The document element must be `EntityDescriptor` in `NS_METADATA`, else `NotEntityDescriptor` (an `EntitiesDescriptor` aggregate is refused this way).
   4. `entityID` attribute → `check_entity_id` (missing → `EntityId`).
   5. The `IDPSSODescriptor` children (`NS_METADATA`) whose `protocolSupportEnumeration`, split on ASCII whitespace, contains `NS_PROTOCOL`: exactly one, else `NoIdpDescriptor`.
   6. Its `SingleSignOnService` children whose `Binding` equals `BINDING_REDIRECT`: take the first. None → `NoRedirectSso`. Its `Location` attribute (missing → `SsoUrl`) → `check_sso_url`.
   7. Its `KeyDescriptor` children whose `use` attribute is absent or equals `signing`: at most 16 of them are read (a 17th → `TooManyCertificates`). For each one, take every `KeyInfo/X509Data/X509Certificate` (`NS_DSIG`) descendant of that KeyDescriptor. Read each with `strict_text` (`None` → `Certificate`), remove ASCII whitespace, base64-decode (error → `Certificate`), and call `IdpCertificate::from_der`.
   8. `dedup_certificates`.
   9. Return. `validUntil`, `cacheDuration`, `Signature`, `Extensions`, `Organization`, `ContactPerson`, `NameIDFormat` and other elements are ignored.

**Tests** (`#[cfg(test)]` in each file; read fixtures with `include_str!("../../../fixtures/saml/<name>")`):
- `cert.rs`:
  - `an_rsa_2048_certificate_is_read`: `idp-rsa.cert.pem` → `Ok`. `not_after_ns > 4_102_444_800_000_000_000` (after 2100-01-01) and `> not_before_ns`. `sha256_hex()` has 64 hex characters.
  - `an_ec_p256_certificate_is_read`.
  - `bare_base64_is_read`: the PEM body without the markers → equal to the PEM result.
  - `a_1024_bit_rsa_key_is_weak`: `weak-rsa-1024.cert.pem` → `WeakKey`.
  - `garbage_is_not_a_certificate`: `from_der(b"not a cert")` → `Certificate`; `from_pem("-----BEGIN CERTIFICATE-----\nAAAA")` (no end marker) → `Certificate`.
  - `debug_shows_only_the_hash`: the `Debug` output contains the hash and not the base64 body.
- `metadata.rs`. A test helper `metadata_xml(entity_id, sso, certs_b64: &[&str], extra: &str) -> String` builds:
  ```xml
  <md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" xmlns:ds="http://www.w3.org/2000/09/xmldsig#" entityID="{entity_id}"><md:IDPSSODescriptor protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol">{key descriptors}<md:SingleSignOnService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" Location="https://idp.test/post"/><md:SingleSignOnService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect" Location="{sso}"/>{extra}</md:IDPSSODescriptor></md:EntityDescriptor>
  ```
  with one `<md:KeyDescriptor use="signing"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{b64}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>` per certificate.
  - `okta_shaped_metadata_parses`: one RSA certificate → the entity ID, the Redirect URL (not the POST one), one certificate.
  - `two_certificates_are_kept_for_rotation`: `idp-rsa` and `idp-rsa-next` → 2, in order.
  - `a_duplicate_certificate_is_kept_once`.
  - `an_encryption_only_key_is_ignored`: one `use="encryption"` descriptor with `attacker-rsa` plus one signing `idp-rsa` → only `idp-rsa`.
  - `five_distinct_certificates_are_too_many`: `idp-rsa`, `idp-rsa-next`, `idp-ec`, `idp-ec384`, `attacker-rsa` → `TooManyCertificates`.
  - `seventeen_key_descriptors_are_too_many`: 17 KeyDescriptors (the same certificate repeated) → `TooManyCertificates`.
  - `a_p384_certificate_is_kept`: `idp-ec384` alone → `Ok`.
  - `no_redirect_binding_is_refused`: the helper's output with `HTTP-Redirect` replaced by `HTTP-Artifact` (`str::replace`) → `NoRedirectSso`.
  - `an_http_sso_url_is_refused`; `a_sso_url_with_userinfo_is_refused` (`https://u:p@idp.test/sso`); `a_sso_url_with_a_fragment_is_refused`.
  - `an_entities_descriptor_is_refused`: root `md:EntitiesDescriptor` → `NotEntityDescriptor`.
  - `two_idp_descriptors_are_refused`.
  - `a_saml1_only_descriptor_is_ignored`: `protocolSupportEnumeration="urn:oasis:names:tc:SAML:1.1:protocol"` only → `NoIdpDescriptor`.
  - `no_signing_certificate_is_refused`.
  - `a_doctype_is_refused`; `oversized_metadata_is_refused` (1 MiB + 1 byte of spaces after the document) → `TooLarge`; `a_bom_is_accepted`.
  - `an_empty_entity_id_is_refused`; `an_entity_id_with_a_newline_is_refused`.
  - `from_fields_applies_the_same_checks`: an http URL → `SsoUrl`; no certificates → `NoSigningCertificate`; five distinct → `TooManyCertificates`.
  - `debug_hides_urls`: `format!("{:?}", metadata)` contains neither the entity ID nor the URL.

**Done when:** written and committed (no build or test, § Conventions).

**Commit:** `Parse SAML IdP metadata and certificates`

---

## SA1.3 — AuthnRequest and SP metadata

**Goal:** build the SP-initiated redirect URL and the SP metadata XML (D-SA-8, D-SA-12).

**Edit:** `crates/saml/src/lib.rs`, `crates/saml/src/request.rs` (new).

**Steps:**

1. `lib.rs`: `mod request;` and `pub use request::{AuthnRequest, RequestError, authn_request_redirect_url, sp_metadata_xml, is_valid_request_id};`.
2. `request.rs`:
   ```rust
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
   #[derive(Debug, Clone, Copy)]
   pub struct AuthnRequest<'a> {
       pub sp_entity_id: &'a str,
       pub acs_url: &'a str,
       pub sso_url: &'a str,
       pub request_id: &'a str,
       pub relay_state: &'a str,
       pub issue_instant_ns: i64,
   }

   /// Whether `id` is `_` followed by exactly 32 lowercase hex digits.
   pub fn is_valid_request_id(id: &str) -> bool;

   /// The IdP URL that starts the login: `sso_url` with `SAMLRequest` and `RelayState` appended
   /// (HTTP-Redirect binding: raw DEFLATE, base64, URL encoding). Unsigned (D-SA-8).
   pub fn authn_request_redirect_url(request: &AuthnRequest<'_>) -> Result<String, RequestError>;

   /// The SP metadata XML served at the SP entity ID (D-SA-12).
   pub fn sp_metadata_xml(sp_entity_id: &str, acs_url: &str) -> String;
   ```
   (`#[derive(Debug)]` on `AuthnRequest` is acceptable: it holds the SP's own URLs and a request ID, no user data.)
3. Escaping uses `xml::escape_attr` and `xml::escape_text` (SA1.1).
4. `authn_request_redirect_url` steps:
   1. `is_valid_request_id(request.request_id)` else `RequestId`.
   2. `url::Url::parse` each of `sp_entity_id`, `acs_url` and `sso_url`; any error → `Url`. Additionally `sso_url` must be `https` (else `Url`).
   3. `issue_instant` = `chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(issue_instant_ns).format("%Y-%m-%dT%H:%M:%SZ")`.
   4. The XML, exactly (no XML declaration, no whitespace between elements), with `{x}` replaced by `escape_attr(x)`, except the Issuer text, which uses `escape_text`:
      ```xml
      <samlp:AuthnRequest xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="{request_id}" Version="2.0" IssueInstant="{issue_instant}" Destination="{sso_url}" AssertionConsumerServiceURL="{acs_url}" ProtocolBinding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST"><saml:Issuer>{sp_entity_id}</saml:Issuer><samlp:NameIDPolicy Format="urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified" AllowCreate="true"/></samlp:AuthnRequest>
      ```
   5. Raw DEFLATE (no zlib header): `flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default())`, `write_all`, `finish`. An I/O error → `Encode`.
   6. `base64::engine::general_purpose::STANDARD.encode`.
   7. `let mut url = Url::parse(sso_url)`, then `url.query_pairs_mut().append_pair("SAMLRequest", &encoded).append_pair("RelayState", relay_state)`. This keeps any query the IdP's URL already has. Return `url.to_string()`.
5. `sp_metadata_xml` returns exactly (with `escape_attr`):
   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <md:EntityDescriptor xmlns:md="urn:oasis:names:tc:SAML:2.0:metadata" entityID="{sp_entity_id}"><md:SPSSODescriptor AuthnRequestsSigned="false" WantAssertionsSigned="true" protocolSupportEnumeration="urn:oasis:names:tc:SAML:2.0:protocol"><md:NameIDFormat>urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified</md:NameIDFormat><md:AssertionConsumerService Binding="urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST" Location="{acs_url}" index="0" isDefault="true"/></md:SPSSODescriptor></md:EntityDescriptor>
   ```
   with a single `\n` after the XML declaration and none at the end.

**Tests** (in `request.rs`; constants `SP = "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001"`, `ACS = SP + "/acs"`, `SSO = "https://idp.test/sso"`, `ID = "_0123456789abcdef0123456789abcdef"`, instant `1_767_225_600_000_000_000` = 2026-01-01T00:00:00Z):
- `request_ids_are_checked`: valid `ID` → true; uppercase hex, 31 digits, no underscore, a digit first → false.
- `the_redirect_url_round_trips`: parse the result with `url::Url`, take `SAMLRequest`, base64-decode, raw-inflate (`flate2::read::DeflateDecoder`), and compare with the exact XML of step 4 (with `IssueInstant="2026-01-01T00:00:00Z"`). `RelayState` equals the input.
- `an_existing_query_is_kept`: `SSO = "https://idp.test/sso?tenant=a"` → the result has `tenant=a` and both new parameters.
- `special_characters_are_escaped`: an ACS URL containing `&` (as `?a=1&b=2`) appears as `&amp;` in the inflated XML, and the inflated XML parses with `xml::parse_strict`.
- `a_bad_request_id_is_refused` → `RequestId`; `an_http_sso_url_is_refused` → `Url`; `a_relative_acs_url_is_refused` → `Url`.
- `sp_metadata_is_exact`: compares the full string for `SP` and `ACS`. It also parses with `xml::parse_strict`, and its root is `EntityDescriptor` in `NS_METADATA`.

**Done when:** written and committed.

**Commit:** `Build the SAML AuthnRequest redirect and SP metadata`

---

## SA1.4 — Test IdP

**Goal:** a dev-only IdP that signs Responses, valid by default, with knobs for every case the response tests and the hostile corpus need (D-SA-15).

**Edit:** `crates/saml/Cargo.toml`, `crates/saml/src/lib.rs`, `crates/saml/src/test_idp.rs` (new).

**Steps:**

1. `Cargo.toml`: add
   ```toml
   [features]
   # The test IdP for this crate's tests and the server's (track SA M2). Never enabled in release builds.
   test-idp = []
   ```
2. `lib.rs`: `#[cfg(any(test, feature = "test-idp"))] pub mod test_idp;`.
3. `test_idp.rs` begins:
   ```rust
   // SPDX-License-Identifier: Elastic-2.0
   //! A test identity provider that signs SAML Responses with the `fixtures/saml` keys
   //! (track SA, D-SA-15). Test support only: never compiled into release builds.
   #![allow(clippy::expect_used, clippy::unwrap_used)] // test support, never built in release
   ```
4. Constants, all `pub`:
   ```rust
   pub const SP_ENTITY_ID: &str = "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001";
   pub const ACS_URL: &str = "https://cetra.test/api/auth/saml/018f6b4e-7c1a-7c1a-8c1a-000000000001/acs";
   pub const IDP_ENTITY_ID: &str = "https://idp.test/metadata";
   pub const IDP_SSO_URL: &str = "https://idp.test/sso";
   pub const REQUEST_ID: &str = "_0123456789abcdef0123456789abcdef";
   pub const RESPONSE_ID: &str = "_r0000000000000000000000000000001";
   pub const ASSERTION_ID: &str = "_a0000000000000000000000000000001";
   /// 2026-01-01T00:00:00Z.
   pub const NOW_NS: i64 = 1_767_225_600_000_000_000;
   ```
5. Keys:
   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub enum TestKey { Idp, IdpNext, IdpEc, Attacker }

   impl TestKey {
       pub fn key_pem(self) -> &'static str;   // include_str! of fixtures/saml/<name>.key.pem
       pub fn cert_pem(self) -> &'static str;  // include_str! of fixtures/saml/<name>.cert.pem
       pub fn certificate(self) -> crate::IdpCertificate; // IdpCertificate::from_pem(cert_pem).unwrap()
       pub fn signature_method(self) -> &'static str;     // ECDSA-SHA256 URI for IdpEc, RSA-SHA256 otherwise
   }
   ```
   File names: `Idp` → `idp-rsa`, `IdpNext` → `idp-rsa-next`, `IdpEc` → `idp-ec`, `Attacker` → `attacker-rsa`. Paths are `include_str!("../../../fixtures/saml/idp-rsa.key.pem")` and so on.
6. The signature, digest and transform URIs come from the `xml.rs` constants (SA1.1). `signature_method()` returns `xml::SIG_ECDSA_SHA256` for `IdpEc` and `xml::SIG_RSA_SHA256` otherwise.
7. Options:
   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub enum Signing { Assertion, Response, Both, None }

   /// Every knob of a test Response. `Default` is a valid Response for the constants above.
   #[derive(Debug, Clone)]
   pub struct ResponseOptions {
       pub response_in_response_to: Option<String>,     // Some(REQUEST_ID)
       pub confirmation_in_response_to: Option<String>, // Some(REQUEST_ID)
       pub destination: Option<String>,                 // Some(ACS_URL)
       pub recipient: String,                           // ACS_URL
       pub audiences: Vec<String>,                      // vec![SP_ENTITY_ID]; empty = no AudienceRestriction
       pub conditions: bool,                            // true; false = no Conditions element at all
       pub response_issuer: Option<String>,             // Some(IDP_ENTITY_ID)
       pub assertion_issuer: String,                    // IDP_ENTITY_ID
       pub name_id: String,                             // "ada@example.com"
       pub name_id_format: Option<String>,              // Some(NAMEID_EMAIL value, spelled out)
       pub attributes: Vec<(String, Vec<String>)>,      // [("email", ["ada@example.com"]), ("displayName", ["Ada Lovelace"])]
       pub issue_instant_ns: i64,                       // NOW_NS
       pub not_before_ns: i64,                          // NOW_NS - 60 s
       pub not_on_or_after_ns: i64,                     // NOW_NS + 5 min
       pub confirmation_not_on_or_after_ns: i64,        // NOW_NS + 5 min
       pub status: String,                              // STATUS_SUCCESS value, spelled out
       pub version: String,                             // "2.0"
       pub signing: Signing,                            // Signing::Assertion
       pub key: TestKey,                                // TestKey::Idp
       pub key_info: bool,                              // true: put the signing cert in KeyInfo
   }
   impl Default for ResponseOptions { /* the values in the comments */ }
   ```
   `name_id`, attribute values and URLs pass through `xml::escape_text` / `xml::escape_attr`.
8. `pub fn response_xml(options: &ResponseOptions) -> String` builds the document and signs it:
   1. The instant format is `%Y-%m-%dT%H:%M:%SZ` (as in SA1.3).
   2. The unsigned document, exactly, with the optional pieces as noted. Element order follows the SAML schema: Issuer, Signature, Status, Assertion. Inside the Assertion: Issuer, Signature, Subject, Conditions, AuthnStatement, AttributeStatement.
      ```xml
      <samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" ID="{RESPONSE_ID}" Version="{version}" IssueInstant="{instant}"[ Destination="{destination}"][ InResponseTo="{response_in_response_to}"]>[<saml:Issuer>{response_issuer}</saml:Issuer>]{RESPONSE_SIGNATURE_SLOT}<samlp:Status><samlp:StatusCode Value="{status}"/></samlp:Status><saml:Assertion ID="{ASSERTION_ID}" Version="{version}" IssueInstant="{instant}"><saml:Issuer>{assertion_issuer}</saml:Issuer>{ASSERTION_SIGNATURE_SLOT}<saml:Subject><saml:NameID[ Format="{name_id_format}"]>{name_id}</saml:NameID><saml:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer"><saml:SubjectConfirmationData NotOnOrAfter="{confirmation_noa}" Recipient="{recipient}"[ InResponseTo="{confirmation_in_response_to}"]/></saml:SubjectConfirmation></saml:Subject>[<saml:Conditions NotBefore="{not_before}" NotOnOrAfter="{not_on_or_after}">[<saml:AudienceRestriction>{<saml:Audience>{a}</saml:Audience> per audience}</saml:AudienceRestriction>]</saml:Conditions>]<saml:AuthnStatement AuthnInstant="{instant}" SessionIndex="_s1"><saml:AuthnContext><saml:AuthnContextClassRef>urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport</saml:AuthnContextClassRef></saml:AuthnContext></saml:AuthnStatement><saml:AttributeStatement>{per attribute: <saml:Attribute Name="{name}">{per value: <saml:AttributeValue>{v}</saml:AttributeValue>}</saml:Attribute>}</saml:AttributeStatement></saml:Assertion></samlp:Response>
      ```
      `[ … ]` is present only when the option is `Some` (or, for `AudienceRestriction`, when `audiences` is non-empty). An empty `attributes` list still writes an empty `<saml:AttributeStatement></saml:AttributeStatement>`.
   3. A signature template for an element with ID `{id}`, with `{method}` = `options.key.signature_method()`:
      ```xml
      <ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:SignedInfo><ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/><ds:SignatureMethod Algorithm="{method}"/><ds:Reference URI="#{id}"><ds:Transforms><ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"/><ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/></ds:Transforms><ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/><ds:DigestValue></ds:DigestValue></ds:Reference></ds:SignedInfo><ds:SignatureValue></ds:SignatureValue>[<ds:KeyInfo><ds:X509Data><ds:X509Certificate>{base64 of the cert DER}</ds:X509Certificate></ds:X509Data></ds:KeyInfo>]</ds:Signature>
      ```
      `KeyInfo` is present when `options.key_info`.
   4. Signing, in **two passes**. bergshamra's `sign` signs only the first `Signature` in document order, and replaces its empty values in place. The Response's signature comes first in the document, so the Assertion must be signed before the Response template is inserted:
      - The signing context is `DsigContext::new_permissive(keys)`. `keys` is a `KeysManager` holding `bergshamra::keys::loader::load_pem_auto(options.key.key_pem().as_bytes(), None)`.
      - `Signing::Assertion`: put the template (id `ASSERTION_ID`) in the assertion slot, the response slot empty; `bergshamra::sign(&ctx, &xml)`.
      - `Signing::Response`: the template (id `RESPONSE_ID`) in the response slot only; sign once.
      - `Signing::Both`: first the assertion template only and sign. Then insert the response template, a string insert at the position of the response slot, which is right after `</saml:Issuer>` of the Response, or right after the Response start tag when `response_issuer` is `None`. Then sign the result again. The second pass must leave the assertion's filled `DigestValue` and `SignatureValue` untouched. A test (below) proves it.
      - `Signing::None`: both slots empty.
      - Build the document with literal slot markers `<!--RS-->` and `<!--AS-->` and replace them with the templates or with `""` before signing; never leave a marker in the output.
9. `pub fn response_b64(options: &ResponseOptions) -> String`: `STANDARD.encode(response_xml(options))`.
10. `pub fn metadata_xml(certs: &[TestKey]) -> String`: IdP metadata for `IDP_ENTITY_ID` and `IDP_SSO_URL`, in the exact shape of SA1.2's test helper, one signing KeyDescriptor per key.
11. `pub fn idp_metadata() -> crate::IdpMetadata`: `crate::parse_metadata(metadata_xml(&[TestKey::Idp]).as_bytes()).unwrap()`.

**Tests** (in `test_idp.rs`, `#[cfg(test)]`):
- `a_default_response_verifies_with_bergshamra`: `response_xml(&Default::default())` → `bergshamra::dsig::verify::verify_all` with `DsigContext::new(keys)`, where `keys` holds `TestKey::Idp.certificate().key()`. There is one `Valid` result whose single reference resolves to the element with ID `ASSERTION_ID`.
- `both_signatures_verify`: `Signing::Both` → two `Valid` results.
- `the_ec_key_signs`: `key: IdpEc`, verified with a `KeysManager` holding `TestKey::IdpEc.certificate().key()` → `Valid`.
- `no_marker_is_left`: none of the outputs (all four `Signing` values) contains `<!--RS-->` or `<!--AS-->`.
- `the_metadata_parses`: `idp_metadata()` has `IDP_ENTITY_ID`, `IDP_SSO_URL` and one certificate.

**Done when:** written and committed.

**Commit:** `Add a test SAML identity provider`

---

## SA1.5 — Response validation

**Goal:** implement D-SA-9 in full: decode a Response, read its `InResponseTo`, and validate it into the facts of its one Assertion.

**Edit:** `crates/saml/src/lib.rs`, `crates/saml/src/response.rs` (new).

**Steps:**

1. `lib.rs`: `mod response;` and `pub use response::{Assertion, AttributeNames, ExpectedResponse, MAX_DECODED_BYTES, MAX_ENCODED_BYTES, ReceivedResponse, ResponseError, decode_response, validate_response};`.
2. Public contract:
   ```rust
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
   pub struct ReceivedResponse { xml: String, in_response_to: Option<String> }

   impl ReceivedResponse {
       /// The root's `InResponseTo`; `None` means unsolicited (D-SA-5).
       pub fn in_response_to(&self) -> Option<&str>;
   }
   /// `Debug` prints `xml_len` and whether `in_response_to` is set; never the XML.
   impl std::fmt::Debug for ReceivedResponse { /* … */ }

   /// Attribute names to read, each list tried in order; the first attribute present wins (D-SA-6).
   #[derive(Debug, Clone, PartialEq, Eq)]
   pub struct AttributeNames {
       pub email: Vec<String>,
       pub display_name: Vec<String>,
       pub first_name: Vec<String>,
       pub last_name: Vec<String>,
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

   /// Decodes the `SAMLResponse` form value (D-SA-9 rule 1) and reads `InResponseTo`.
   pub fn decode_response(saml_response: &str) -> Result<ReceivedResponse, ResponseError>;

   /// Validates `received` against `expected` (D-SA-9 rules 2–19).
   pub fn validate_response(received: &ReceivedResponse, expected: &ExpectedResponse<'_>) -> Result<Assertion, ResponseError>;
   ```
   `ExpectedResponse` derives `Debug`. Its `idp` field's `Debug` is already payload-free (SA1.2), and the SP's own URLs are not payload.
3. `impl Default for AttributeNames`, exact lists in this order:
   - `email`: `"email"`, `"emailAddress"`, `"mail"`, `"http://schemas.xmlsoap.org/ws/2005/05/identity/claims/emailaddress"`, `"urn:oid:0.9.2342.19200300.100.1.3"`.
   - `display_name`: `"displayName"`, `"http://schemas.microsoft.com/identity/claims/displayname"`, `"urn:oid:2.16.840.1.113730.3.1.241"`, `"name"`.
   - `first_name`: `"firstName"`, `"givenName"`, `"http://schemas.xmlsoap.org/ws/2005/05/identity/claims/givenname"`, `"urn:oid:2.5.4.42"`.
   - `last_name`: `"lastName"`, `"surname"`, `"sn"`, `"http://schemas.xmlsoap.org/ws/2005/05/identity/claims/surname"`, `"urn:oid:2.5.4.4"`.

   (Entra's `…/claims/name` is the UPN, not a display name, so it is not in the list.)
4. Constants:
   ```rust
   /// The largest base64 form value read (before decoding), in bytes.
   pub const MAX_ENCODED_BYTES: usize = 360_000;
   /// The largest decoded Response, in bytes (D-SA-9 rule 1).
   pub const MAX_DECODED_BYTES: usize = 256 * 1024;
   const SKEW_NS: i64 = 60_000_000_000;
   const ISSUE_MAX_AGE_NS: i64 = 300_000_000_000;
   const MAX_NAME_ID_BYTES: usize = 1024;
   const MAX_EMAIL_BYTES: usize = 320;
   const MAX_DISPLAY_NAME_CHARS: usize = 256;
   ```
5. `decode_response` steps:
   1. `saml_response.len() > MAX_ENCODED_BYTES` → `TooLarge`.
   2. Remove ASCII whitespace (IdPs wrap base64 lines), then `STANDARD.decode`; an error → `Base64`.
   3. Decoded length `> MAX_DECODED_BYTES` → `TooLarge`.
   4. `String::from_utf8` → `Utf8`. Strip one leading `\u{feff}`.
   5. `ReceivedResponse::from_xml(xml)` (below).

   `pub(crate) fn from_xml(xml: String) -> Result<ReceivedResponse, ResponseError>`:
   1. `xml::parse_strict(&xml)`: `Doctype` → `Doctype`, `Xml` → `Xml`.
   2. The document element must be `Response` in `NS_PROTOCOL`, else `NotAResponse`.
   3. `in_response_to` = `attr(root, "InResponseTo")` as an owned `String`.
   4. Drop the document and return the struct holding `xml`.
6. `validate_response` steps, **in this order**, each failure returning at once:
   1. Parse `received.xml` again with `parse_strict` (rule 2). An error here cannot happen for a value from `from_xml`; map it as in `from_xml` anyway, never `unwrap`. Let `root` be the document element.
   2. **Version (rule 3):** `attr(root, "Version") == Some("2.0")`, else `Version`.
   3. **Encrypted (rule 4):** any `descendants_named(root, NS_ASSERTION, "EncryptedAssertion")` → `EncryptedAssertion`.
   4. **One Assertion (rule 4):** `descendants_named(root, NS_ASSERTION, "Assertion")` must have exactly one element, whose parent is `root`. Otherwise `AssertionCount`. Call it `assertion`.
   5. **Status (rule 3):** `only_child(root, NS_PROTOCOL, "Status")`, then its `only_child(.., NS_PROTOCOL, "StatusCode")` with `Value == STATUS_SUCCESS`. Anything else → `Status`.
   6. **Signature layout (rule 5):** `signatures = descendants_named(root, NS_DSIG, "Signature")`.
      - Empty → `SignatureMissing`.
      - More than 2 → `SignatureLayout`.
      - Each signature's parent must be `root` or `assertion`, and no two may share a parent. Otherwise `SignatureLayout`.
      - For each signature, the *signed element* is its parent.
   7. **SignedInfo profile (rules 7, 8, 9)**, for each signature:
      - `only_child(sig, NS_DSIG, "SignedInfo")` is required (else `SignatureLayout`).
      - Inside it, `CanonicalizationMethod/@Algorithm` must be `C14N_EXCLUSIVE` or `C14N_EXCLUSIVE_COMMENTS` (else `Algorithm`).
      - `SignatureMethod/@Algorithm` must be one of the five allowed `SIG_*` constants, which excludes `SIG_RSA_SHA1` (else `Algorithm`).
      - Exactly one `Reference` child (else `SignatureLayout`). Its `URI` must equal `"#"` followed by the signed element's `ID` attribute, where `ID` is present and non-empty (else `SignatureLayout`; an empty `URI=""` is refused here).
      - Its `Transforms` child is required. It has exactly two `Transform` children: first `TRANSFORM_ENVELOPED`, then `C14N_EXCLUSIVE` or `C14N_EXCLUSIVE_COMMENTS`. A lone enveloped transform would fall back to inclusive c14n. Anything else is `Algorithm`. A `Transform` may have an `InclusiveNamespaces` child; it is allowed and ignored here.
      - `DigestMethod/@Algorithm` must be one of the three `DIGEST_SHA*` of 256 bits and above (else `Algorithm`).
      - Each of `CanonicalizationMethod`, `SignatureMethod`, `Transforms` and `DigestMethod` is read with `only_child`. Missing, duplicated or without an `Algorithm` attribute is `Algorithm`.
   8. **Verify (rules 6, 7):**
      - Build a `KeysManager` with `add_key(cert.key()?)` for every `expected.idp.certificates()`; a key error → `SignatureInvalid`.
      - `let ctx = bergshamra::DsigContext::new(keys)`. Its defaults are `trusted_keys_only` and `strict_verification`; do **not** change any field.
      - `bergshamra::dsig::verify::verify_all_document(&ctx, &doc)`. Any `Err` (duplicate IDs among them) → `SignatureInvalid`.
      - The result vector must have exactly `signatures.len()` entries, all `VerifyResult::Valid`. For each: `signature_node` is one of `signatures`, `references.len() == 1`, `references[0].digest_verified`, and `references[0].resolved_node == Some(parent of signature_node)`. Any failure → `SignatureInvalid`.
      - From here on, everything is read from `root` and `assertion` only. Both are covered: a Response signature covers its only Assertion.
   9. **Response attributes:**
      - `Destination`, if present, must equal `expected.acs_url` (rule 11, else `Destination`).
      - `InResponseTo` is required and must equal `expected.request_id` (rule 13, else `InResponseTo`).
      - The Response has at most one `Issuer` child (two → `Issuer`); if present, it must pass `check_issuer` (below).
      - `IssueInstant` is required and must pass `check_issue_instant` (rule 15).
   10. **Assertion:**
       - `attr(assertion, "Version") == Some("2.0")`, else `Version`.
       - `only_child(assertion, NS_ASSERTION, "Issuer")` is required (missing or several → `Issuer`) and must pass `check_issuer`.
       - `IssueInstant` is required → `check_issue_instant`.
   11. **Subject and NameID (rules 18, 19):**
       - `only_child(assertion, NS_ASSERTION, "Subject")` is required (else `NameIdMissing`), and inside it `only_child(subject, NS_ASSERTION, "NameID")` (else `NameIdMissing`; an `EncryptedID` or `BaseID` gives no `NameID`).
       - `Format == NAMEID_TRANSIENT` → `NameIdTransient`.
       - The text is `strict_text` (`None` → `ValueContent`). Empty → `NameIdMissing`; longer than `MAX_NAME_ID_BYTES`, or containing a `char::is_control` character → `ValueContent`. The same control-character rule applies to the email and display name values of step 14.
   12. **Subject confirmation (rules 11, 13, 14, 17):** at least one `SubjectConfirmation` child of `subject` with `Method == CM_BEARER` must have an `only_child` `SubjectConfirmationData` whose:
       - `Recipient` equals `expected.acs_url`;
       - `InResponseTo` equals `expected.request_id`;
       - `NotOnOrAfter` is present and parses, with `now_ns < not_on_or_after + SKEW_NS`;
       - `NotBefore` is absent, or parses with `now_ns + SKEW_NS >= not_before`.

       None → `SubjectConfirmation`.
   13. **Conditions (rules 12, 16):**
       - `only_child(assertion, NS_ASSERTION, "Conditions")` is required (else `Conditions`).
       - Its `NotBefore`, when present, must parse with `now_ns + SKEW_NS >= not_before`. Its `NotOnOrAfter`, when present, must parse with `now_ns < not_on_or_after + SKEW_NS`. Otherwise `Conditions`.
       - Its `AudienceRestriction` children: at least one, and **every** one must have an `Audience` child whose `strict_text` equals `expected.sp_entity_id` (restrictions are ANDed in SAML). Otherwise `Audience`, including when `strict_text` is `None`.
   14. **Attributes (D-SA-6):**
       - Collect `Attribute` elements of every `AttributeStatement` child of `assertion`, as (`Name` attribute, first `AttributeValue` child).
       - For a name list, the first name in list order that has an attribute wins. When several `Attribute` elements share that name, the first in document order wins. Its value is `strict_text` of that `AttributeValue` (`None` → `ValueContent`), trimmed of ASCII whitespace. Empty counts as absent.
       - `email` = the email list's value; else, when `name_id_format == Some(NAMEID_EMAIL)`, the NameID text trimmed. Longer than `MAX_EMAIL_BYTES` → `ValueContent`.
       - `display_name` = the display list's value; else `first` and `last` joined by one space (either alone when the other is absent); else `None`. Truncated to `MAX_DISPLAY_NAME_CHARS` characters on a `char` boundary.
       - A non-matching attribute is never read, so its content cannot fail validation.
   15. Return `Assertion`.

   Helpers:
   - `check_issuer(doc, node, expected)`: the `Format` attribute is absent or `NAMEID_ENTITY`; `strict_text` equals `expected.idp.entity_id()`. Otherwise `Issuer`.
   - `check_issue_instant(value, now_ns)`: `issue + SKEW_NS >= now_ns - ISSUE_MAX_AGE_NS` and `issue <= now_ns + SKEW_NS`. Otherwise `IssueInstant`. A parse failure is `IssueInstant` too.
   - `parse_instant(&str) -> Option<i64>`: the text must end with `Z`; `chrono::DateTime::parse_from_rfc3339`, then `timestamp_nanos_opt()`. Fractional seconds are accepted.
   - Use `checked_add` / `checked_sub` for every `SKEW_NS` and `ISSUE_MAX_AGE_NS` arithmetic. An overflow fails the check it belongs to; it never panics.

**Tests** (in `response.rs`, using `crate::test_idp`). `ExpectedResponse` borrows its metadata and attribute names, so a helper cannot return one built from temporaries. The helpers are:
- `fn run_with(options: &ResponseOptions, keys: &[TestKey], request_id: &str, now_ns: i64) -> Result<Assertion, ResponseError>`. It builds locals `let idp = parse_metadata(metadata_xml(keys).as_bytes()).unwrap(); let names = AttributeNames::default();`, then `ExpectedResponse { sp_entity_id: SP_ENTITY_ID, acs_url: ACS_URL, idp: &idp, request_id, now_ns, attributes: &names }`, then `decode_response(&response_b64(options))?` and `validate_response`.
- `fn run(options: &ResponseOptions) -> Result<Assertion, ResponseError>` = `run_with(options, &[TestKey::Idp], REQUEST_ID, NOW_NS)`.

Tests that need other keys or another clock call `run_with`: `IdpEc` metadata for the EC case, `[Idp, IdpNext]` for rotation, `[Idp]` with `key: IdpNext` for the rotated-out case.

Accepted:
- `a_signed_assertion_is_accepted`: the default → `name_id == "ada@example.com"`, `email == Some("ada@example.com")`, `display_name == Some("Ada Lovelace")`.
- `a_signed_response_is_accepted` (`Signing::Response`).
- `both_signatures_are_accepted` (`Signing::Both`).
- `an_ec_signature_is_accepted` (`key: IdpEc`, metadata `[IdpEc]`).
- `the_next_certificate_is_accepted_during_rotation`: `key: IdpNext`, with metadata of `[Idp, IdpNext]`.
- `base64_with_line_breaks_is_accepted`: the base64 wrapped every 76 characters with `\r\n`.
- `skew_within_60_seconds_is_accepted`, two cases:
  - Set `issue_instant_ns = NOW_NS + 300 s`, `not_on_or_after_ns = confirmation_not_on_or_after_ns = NOW_NS + 300 s`, and validate at `now = NOW_NS + 359 s`. The issue instant is 59 s old, within the window.
  - Keep `now = NOW_NS` and set `not_before_ns = NOW_NS + 59 s`.
- `the_email_falls_back_to_an_email_name_id`: no email attribute.
- `first_and_last_name_make_the_display_name`.
- `a_long_display_name_is_truncated`: 300 characters, multi-byte (`é`) → 256 characters.
- `unrelated_attributes_with_markup_are_ignored`: an attribute `other` whose value contains a comment.
- `a_persistent_name_id_is_accepted`: format `…:2.0:nameid-format:persistent`, opaque value → `email` from the attribute.
- `no_name_id_format_is_accepted`.

Refused, one test per error (each named after its cause):
- `oversized_input_is_too_large`; `bad_base64_is_refused`; `non_utf8_is_refused`; `a_non_response_root_is_refused` (an `AuthnRequest` document).
- `version_1_1_is_refused`; `a_failure_status_is_refused` (`…:status:Requester`); `an_unsigned_response_is_refused` (`Signing::None` → `SignatureMissing`).
- `the_attacker_key_is_refused` (`key: Attacker`, `key_info: true` → `SignatureInvalid`); `a_rotated_out_certificate_is_refused` (`key: IdpNext`, metadata `[Idp]` → `SignatureInvalid`).
- `a_wrong_destination_is_refused`; `a_missing_response_in_response_to_is_refused`; `a_wrong_response_in_response_to_is_refused`; `a_wrong_confirmation_in_response_to_is_refused` (→ `SubjectConfirmation`).
- `a_wrong_recipient_is_refused`; `an_expired_confirmation_is_refused`.
- `a_wrong_response_issuer_is_refused`; `a_wrong_assertion_issuer_is_refused`.
- `an_old_issue_instant_is_refused` (NOW − 6 min − 1 s); `a_future_issue_instant_is_refused` (NOW + 61 s).
- `missing_conditions_are_refused`; `not_yet_valid_conditions_are_refused`; `expired_conditions_are_refused`.
- `a_wrong_audience_is_refused`; `no_audience_restriction_is_refused`.
- `a_transient_name_id_is_refused`; `an_empty_name_id_is_refused`.
- `an_overlong_name_id_is_refused` (1025 bytes → `ValueContent`); `an_overlong_email_is_refused`.
- `an_invalid_request_id_never_matches`: `expected.request_id = "x"` → `InResponseTo`.
- `debug_hides_the_payload`: `format!("{:?}", assertion)` and `format!("{:?}", received)` contain neither `ada@example.com` nor `Ada`.

**Done when:** written and committed.

**Commit:** `Validate SAML responses`

---

## SA1.6 — Hostile corpus

**Goal:** prove D-SA-9 against the known attacks: each case is a signed Response altered the way the attack does it, and each must be refused with the named error.

**Edit:** `crates/saml/src/lib.rs`, `crates/saml/src/hostile_tests.rs` (new).

**Steps:**

1. `lib.rs`: `#[cfg(test)] mod hostile_tests;`.
2. `hostile_tests.rs`:
   - It starts with the SPDX header and `//! The hostile corpus of D-SA-15: signature wrapping, comment injection, DTDs, KeyInfo substitution, weak algorithms (track SA M1).`
   - It defines `fn check(xml: &str) -> Result<Assertion, ResponseError>`, which base64-encodes, decodes and validates against `expected()`. It also defines `expected()` here, copied from SA1.5's tests (tests do not share private helpers across modules).
   - Each case starts from `test_idp::response_xml(&options)` and alters the string with `str::replacen` / `insert_str` at a fixed marker. A case whose marker is not found fails with `assert!(xml.contains(marker))` first, so the test never passes vacuously.
3. The cases. Names and expected errors are exact. "Signed" means the default `Signing::Assertion` unless noted.

| Test | Alteration | Expected |
|---|---|---|
| `xsw_second_assertion_before_the_signed_one` | Insert an unsigned evil Assertion (ID `_evil`, NameID `admin@example.com`, otherwise a copy of the signed one's Subject and Conditions with the signature removed) right before `<saml:Assertion ID="{ASSERTION_ID}"` | `AssertionCount` |
| `xsw_second_assertion_after_the_signed_one` | The same evil Assertion inserted before `</samlp:Response>` | `AssertionCount` |
| `xsw_signed_assertion_moved_into_extensions` | Wrap the whole signed Assertion in `<samlp:Extensions>…</samlp:Extensions>` and put the evil Assertion where it was | `AssertionCount` |
| `xsw_signed_assertion_inside_the_evil_one` | Put the signed Assertion inside the evil Assertion's `<saml:Advice>`, as the Response's only direct Assertion | `AssertionCount` |
| `xsw_signed_assertion_inside_the_signature_object` | `Signing::Response`; copy the original Assertion into `<ds:Object>` inside the Response signature, then change the direct Assertion's NameID to `admin@example.com` | `AssertionCount` |
| `xsw_duplicate_id` | The evil Assertion carries the same `ID="{ASSERTION_ID}"` and replaces the Response-level position, the signed one moved into Extensions | `AssertionCount` (checked before verification); a variant with the evil copy as the only Assertion and the signed copy inside the evil one's `<saml:Advice>` gives `AssertionCount` too |
| `signature_moved_to_the_response` | `Signing::Assertion`; cut the `<ds:Signature …>…</ds:Signature>` from the Assertion and paste it after the Response's `</saml:Issuer>` | `SignatureLayout` (a Response-level signature whose reference names the Assertion's ID) |
| `signature_in_the_subject` | Move the Assertion's signature inside `<saml:Subject>` | `SignatureLayout` |
| `empty_reference_uri` | Replace `URI="#{ASSERTION_ID}"` by `URI=""` | `SignatureLayout` |
| `reference_to_another_id` | `Signing::Response`; replace `URI="#{RESPONSE_ID}"` by `URI="#{ASSERTION_ID}"` | `SignatureLayout` |
| `three_signatures` | `Signing::Both`, then paste a copy of the Assertion signature inside `<samlp:Status>` | `SignatureLayout` |
| `name_id_modified_after_signing` | Replace `>ada@example.com</saml:NameID>` by `>admin@example.com</saml:NameID>` | `SignatureInvalid` |
| `comment_injected_after_signing` | Replace `>ada@example.com</saml:NameID>` by `>ada@example.com<!---->.evil.test</saml:NameID>` | `SignatureInvalid` (the canonical text becomes `ada@example.com.evil.test`, which the IdP did not sign) |
| `comment_signed_by_the_idp` | `name_id: "ada@example.com.evil.test"` signed, then insert `<!---->` after `ada@example.com` | `ValueContent` |
| `comment_in_the_email_attribute` | Insert `<!---->` inside the `email` AttributeValue | `ValueContent` |
| `cdata_name_id_is_plain_text` | Replace `>ada@example.com</saml:NameID>` by `><![CDATA[ada@example.com]]></saml:NameID>` | accepted, `name_id == "ada@example.com"` (CDATA is text in c14n; no signature change) |
| `doctype_entity_expansion` | Prepend `<!DOCTYPE r [<!ENTITY x "admin">]>` | `Doctype` |
| `billion_laughs` | Prepend the classic 10-level `lol` entity DTD | `Doctype` |
| `external_entity` | Prepend `<!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]>` | `Doctype` |
| `keyinfo_certificate_is_ignored` | `key: Attacker`, `key_info: true` (the attacker's certificate in KeyInfo) | `SignatureInvalid` |
| `rsa_sha1_is_refused` | Replace the SignatureMethod URI `…#rsa-sha256` by `SIG_RSA_SHA1` | `Algorithm` |
| `sha1_digest_is_refused` | Replace `DIGEST_SHA256` by `DIGEST_SHA1` | `Algorithm` |
| `inclusive_c14n_is_refused` | Replace the whole element `<ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/>` by `<ds:CanonicalizationMethod Algorithm="http://www.w3.org/TR/2001/REC-xml-c14n-20010315"/>` (only that element; the Transform keeps exc-c14n) | `Algorithm` |
| `xpath_transform_is_refused` | Insert `<ds:Transform Algorithm="http://www.w3.org/TR/1999/REC-xpath-19991116"><ds:XPath>1</ds:XPath></ds:Transform>` as a third transform | `Algorithm` |
| `encrypted_assertion_is_unsupported` | Replace the whole Assertion with `<saml:EncryptedAssertion><xenc:EncryptedData xmlns:xenc="http://www.w3.org/2001/04/xmlenc#"/></saml:EncryptedAssertion>` | `EncryptedAssertion` |
| `encrypted_assertion_beside_a_signed_one` | Insert that EncryptedAssertion before the signed Assertion | `EncryptedAssertion` |
| `unsigned_assertion_in_a_signed_response_twin` | `Signing::Response`, then a second Assertion appended (unsigned) | `AssertionCount` |
| `response_signature_with_a_tampered_assertion` | `Signing::Response`; change the Assertion's `Recipient` | `SignatureInvalid` |
| `both_signed_one_broken` | `Signing::Both`; in the Response's `SignatureValue`, replace the character at index `len / 2` with a different base64 letter (`A` ↔ `B`), away from the `=` padding | `SignatureInvalid` |
| `id_copied_onto_another_element` | Add `ID="{ASSERTION_ID}"` to the `<saml:Subject>` start tag (a duplicate ID outside any second Assertion) | `SignatureInvalid` (bergshamra refuses duplicate IDs) |
| `namespace_confusion` | Replace `<saml:Assertion ID=` by `<saml:Assertion xmlns:saml="urn:evil" ID=` | `AssertionCount` (zero assertions in the SAML namespace) |
| `replayed_response_for_another_request` | Default response, validated with `request_id = "_ffffffffffffffffffffffffffffffff"` | `InResponseTo` |

4. The binding checks D-SA-15 also names (audience, recipient, destination, issuer, transient NameID, times) are covered by SA1.5's refused tests; this table does not repeat them.
5. Every evil Assertion string is built by a helper `evil_assertion(id: &str, name_id: &str) -> String`. It returns the exact unsigned Assertion that `test_idp` would write for `ResponseOptions { name_id, signing: Signing::None, .. }`, cut out of `response_xml` between `<saml:Assertion` and `</saml:Assertion>` inclusive, with `ID="{ASSERTION_ID}"` replaced by `ID="{id}"`.

**Done when:** written and committed.

**Commit:** `Add the SAML hostile corpus`

---

## SA1.7 — Fuzz targets

**Goal:** fuzz response validation and metadata parsing (D-SA-15), with the repository's fuzz setup.

**Edit:** `crates/saml/src/lib.rs`, `crates/saml/src/fuzz_entry.rs` (new), `fuzz/Cargo.toml`, `fuzz/fuzz_targets/saml_response.rs` (new), `fuzz/fuzz_targets/saml_metadata.rs` (new), `justfile`, `.github/workflows/nightly.yml`.

**Steps:**

1. `lib.rs`: `#[cfg(any(test, fuzzing))] pub mod fuzz_entry;`. The `fuzzing` cfg is set by cargo-fuzz and declared in the workspace's `check-cfg`. Gating keeps the fixture certificate out of release builds.
2. `fuzz_entry.rs`:
   ```rust
   // SPDX-License-Identifier: Elastic-2.0
   //! Fuzz entry points (track SA M1, D-SA-15). Each takes arbitrary bytes and must not panic.
   #![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

   /// Validates arbitrary bytes as a decoded Response XML against the test IdP's settings.
   pub fn saml_response(data: &[u8]);
   /// Parses arbitrary bytes as IdP metadata.
   pub fn saml_metadata(data: &[u8]);
   ```
   - `saml_response`: `if let Ok(text) = std::str::from_utf8(data)`, then `if let Ok(received) = ReceivedResponse::from_xml(text.to_owned())`, then `let _ = validate_response(&received, &expected)`.
   - `expected` is built from constants copied from `test_idp` (the module is not compiled under `fuzzing` alone): `SP_ENTITY_ID`, `ACS_URL`, `REQUEST_ID`, `NOW_NS`, `AttributeNames::default()`.
   - The IdP is metadata parsed once in a `std::sync::OnceLock<IdpMetadata>`, from `IdpMetadata::from_fields("https://idp.test/metadata", "https://idp.test/sso", vec![IdpCertificate::from_pem(include_str!("../../../fixtures/saml/idp-rsa.cert.pem")).expect("the fixture certificate reads")])`.
   - `saml_metadata`: `let _ = parse_metadata(data);`.
3. Tests in `fuzz_entry.rs` (`#[cfg(test)] pub(crate) mod tests`):
   - Copy `replay` and `sorted_files` from `crates/otel/src/fuzz_entry.rs` § tests, the same bodies.
   - `saml_response_corpus_replays`, `saml_metadata_corpus_replays`.
   - `#[test] #[ignore] fn write_seed_corpus()` writes into `fuzz/corpus/saml_response/` the files below, as decoded XML (not base64), using `crate::test_idp::response_xml`. The test is at the path `fuzz_entry::tests::write_seed_corpus`, which `just fuzz-seed` runs.
     - `seed-valid-assertion`, `seed-valid-response`, `seed-valid-both`, `seed-ec`;
     - `seed-unsigned`;
     - `seed-wrong-audience`;
     - `seed-two-assertions`, which uses the default response with its Assertion duplicated before `</samlp:Response>`.

     And into `fuzz/corpus/saml_metadata/`:
     - `seed-one-cert`, `seed-two-certs` (`test_idp::metadata_xml`);
     - `seed-no-redirect`, which is `seed-one-cert` with `HTTP-Redirect` replaced by `HTTP-Artifact`.
4. `fuzz/Cargo.toml`: add `cetra-saml = { path = "../crates/saml" }` under `[dependencies]`, and two `[[bin]]` entries, the same shape as the others: `saml_response` → `fuzz_targets/saml_response.rs`, `saml_metadata` → `fuzz_targets/saml_metadata.rs` (`test = false`, `doc = false`, `bench = false`).
5. `fuzz/fuzz_targets/saml_response.rs`:
   ```rust
   #![no_main]

   libfuzzer_sys::fuzz_target!(|data: &[u8]| cetra_saml::fuzz_entry::saml_response(data));
   ```
   and `saml_metadata.rs` likewise. These two files are under `fuzz/`, not an `ee` directory: no SPDX header, like the other targets.
6. `justfile`:
   - Append ` saml_response saml_metadata` to `fuzz_targets := "…"`.
   - In `_fuzz-max-len`, add `saml_response|saml_metadata` to the `262144` arm: `wal_replay|otlp_http_protobuf|otlp_http_json|otlp_grpc|saml_response|saml_metadata) echo 262144 ;;`.
   - `_fuzz-max-files` keeps its default of 2000 for them.
7. `.github/workflows/nightly.yml`: add `- saml_response` and `- saml_metadata` at the end of the `fuzz` job's `matrix.target` list.

**Tests:** the two replay tests above. They fail while the corpus is empty, so SA1.8 runs `just fuzz-seed` before the checks.

**Done when:** written and committed.

**Commit:** `Fuzz SAML response validation and metadata parsing`

---

## SA1.8 — Checks, seeds, footprint, request to Q

**Goal:** the milestone's checks pass, and the crate's footprint and the request to Q are recorded.

This phase is the **coordinating session's**, not an implementer's.

**Steps:**

1. `just fuzz-seed`, then commit the new `fuzz/corpus/saml_response/` and `fuzz/corpus/saml_metadata/` files.
2. `cargo test -p cetra-saml --all-features`, then `just check-rust` (format, clippy, tests, `check-ee`, `deny-policy`). Fix what fails in a fix phase brief, one Haiku agent, same rules, and re-run. A failure that contradicts this plan (an API that does not exist as written, a bergshamra behaviour different from what SA1.4 or SA1.5 assume) is answered here, and the plan and design doc are corrected in the same branch.
3. `just deny` (advisories included) once.
4. Record in [`README.md`](README.md) § Library:
   - the measured footprint: the new crate count in `Cargo.lock`, the pre-release crates, the duplicate versions `cargo deny` warns about;
   - the exact versions used.
5. A Sonnet code review of the milestone's diff against this plan and D-SA-9. Fix its findings with a Haiku agent; check the fix.
6. Write `sa-requests-for-q.md` in the shared notes folder. It holds:
   - the crate's git path and the commit to pin;
   - the public API (`parse_metadata`, `IdpMetadata::from_fields`, `IdpCertificate`, `authn_request_redirect_url`, `sp_metadata_xml`, `decode_response`, `validate_response`, the error enums);
   - the contract Q must keep on its side: D-SA-10's pending table and binder cookie, the unsolicited restart of D-SA-5, D-SA-11's refresh rules;
   - the gate: Enterprise orgs only, D-SA-3.
7. `docs/roadmap.md`: the SA row's status becomes "M1 implemented".

**Done when:** `just check-rust` and `just deny` pass, the review's findings are fixed, and the notes file exists.

**Commit:** `Seed the SAML fuzz corpora` (step 1), and one commit per fix phase with the fix brief's subject; `Record cetra-saml's footprint and M1 status` (steps 4, 7).
