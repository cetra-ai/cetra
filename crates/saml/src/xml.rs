// SPDX-License-Identifier: Elastic-2.0
//! Strict XML reading shared by metadata and response parsing (D-SA-9 rules 1, 2, 18).

use bergshamra::xml::uppsala::{Document, NodeId, NodeKind, Parser};

#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NS_PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NS_ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NS_METADATA: &str = "urn:oasis:names:tc:SAML:2.0:metadata";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NS_DSIG: &str = "http://www.w3.org/2000/09/xmldsig#";

#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const BINDING_REDIRECT: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const BINDING_POST: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const STATUS_SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const CM_BEARER: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NAMEID_UNSPECIFIED: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NAMEID_EMAIL: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NAMEID_TRANSIENT: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:transient";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const NAMEID_ENTITY: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:entity";

#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const C14N_EXCLUSIVE: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const C14N_EXCLUSIVE_COMMENTS: &str =
    "http://www.w3.org/2001/10/xml-exc-c14n#WithComments";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const TRANSFORM_ENVELOPED: &str =
    "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const SIG_RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const SIG_RSA_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha384";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const SIG_RSA_SHA512: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const SIG_ECDSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const SIG_ECDSA_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha384";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const SIG_RSA_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#rsa-sha1";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const DIGEST_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const DIGEST_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#sha384";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const DIGEST_SHA512: &str = "http://www.w3.org/2001/04/xmlenc#sha512";
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const DIGEST_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#sha1";

#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const MAX_DEPTH: u32 = 32;
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const MAX_ELEMENTS: usize = 10_000;
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) const MAX_ATTRIBUTES: usize = 64;

/// Why strict parsing refused a document.
#[allow(dead_code)] // used by later phases or by tests only
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseFailure {
    /// A `<!DOCTYPE` or `<!ENTITY` declaration (checked on the bytes, before parsing).
    Doctype,
    /// Not well-formed, too deep, too many elements, or too many attributes on one element.
    Xml,
}

/// Parses `text` with the one parser configuration every read uses (rule 2).
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn parse_strict(text: &str) -> Result<Document<'_>, ParseFailure> {
    if text.contains("<!DOCTYPE") || text.contains("<!ENTITY") {
        return Err(ParseFailure::Doctype);
    }
    let doc = Parser::new()
        .with_forbid_dtd(true)
        .with_forbid_entities(true)
        .with_max_depth(MAX_DEPTH)
        .parse(text)
        .map_err(|_| ParseFailure::Xml)?;
    // Iterative walk (rule 3): an explicit stack, never recursion.
    let mut stack: Vec<NodeId> = vec![doc.root()];
    let mut elements = 0usize;
    while let Some(id) = stack.pop() {
        if let Some(NodeKind::Element(element)) = doc.node_kind(id) {
            elements += 1;
            if elements > MAX_ELEMENTS || element.attributes.len() > MAX_ATTRIBUTES {
                return Err(ParseFailure::Xml);
            }
        }
        stack.extend(doc.children(id));
    }
    Ok(doc)
}

/// The concatenated text of `node`'s children. Refuses (`None`) when a child is an element,
/// a comment or a processing instruction (rule 18). Text and CDATA children are concatenated
/// in document order; no trimming.
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn strict_text(doc: &Document<'_>, node: NodeId) -> Option<String> {
    let mut out = String::new();
    for child in doc.children(node) {
        match doc.node_kind(child)? {
            NodeKind::Text(text) | NodeKind::CData(text) => out.push_str(text),
            _ => return None,
        }
    }
    Some(out)
}

/// The child elements of `parent` named `local` in namespace `ns`, in document order.
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn children_named(
    doc: &Document<'_>,
    parent: NodeId,
    ns: &str,
    local: &str,
) -> Vec<NodeId> {
    doc.children(parent)
        .into_iter()
        .filter(|&child| is_element(doc, child, ns, local))
        .collect()
}

/// The only child element named `local` in `ns`; `None` when there are zero or several.
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn only_child(
    doc: &Document<'_>,
    parent: NodeId,
    ns: &str,
    local: &str,
) -> Option<NodeId> {
    match children_named(doc, parent, ns, local).as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

/// Every descendant element of `root` (excluding `root`) named `local` in `ns`, in document order.
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn descendants_named(
    doc: &Document<'_>,
    root: NodeId,
    ns: &str,
    local: &str,
) -> Vec<NodeId> {
    doc.descendants(root)
        .into_iter()
        .filter(|&node| is_element(doc, node, ns, local))
        .collect()
}

/// Whether `node` is an element named `local` in `ns`.
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn is_element(doc: &Document<'_>, node: NodeId, ns: &str, local: &str) -> bool {
    doc.element(node)
        .is_some_and(|element| element.name.matches(Some(ns), local))
}

/// An attribute of `node` without a namespace (SAML attributes are unqualified).
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn attr<'d>(doc: &'d Document<'_>, node: NodeId, local: &str) -> Option<&'d str> {
    doc.element(node)?
        .attributes
        .iter()
        .find(|attribute| {
            attribute.name.namespace_uri.is_none() && *attribute.name.local_name == *local
        })
        .map(|attribute| &*attribute.value)
}

/// Escapes an attribute value: `&` → `&amp;`, `<` → `&lt;`, `>` → `&gt;`, `"` → `&quot;`, `'` → `&apos;` (`&` first).
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Escapes element text: `&`, `<`, `>` as in `escape_attr` (`&` first).
#[allow(dead_code)] // used by later phases or by tests only
pub(crate) fn escape_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document element of a parsed test document.
    fn root_element(doc: &Document<'_>) -> NodeId {
        doc.document_element()
            .expect("test document has a root element")
    }

    #[test]
    fn a_doctype_is_refused_before_parsing() {
        assert_eq!(
            parse_strict("<!DOCTYPE r><r/>").err(),
            Some(ParseFailure::Doctype)
        );
    }

    #[test]
    fn an_entity_declaration_is_refused() {
        assert_eq!(
            parse_strict("<r><!ENTITY x \"y\"></r>").err(),
            Some(ParseFailure::Doctype)
        );
    }

    #[test]
    fn ill_formed_xml_is_refused() {
        assert_eq!(parse_strict("<a><b></a>").err(), Some(ParseFailure::Xml));
    }

    #[test]
    fn depth_above_the_limit_is_refused() {
        let nested = |n: usize| format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
        assert!(parse_strict(&nested(32)).is_ok());
        assert_eq!(parse_strict(&nested(33)).err(), Some(ParseFailure::Xml));
    }

    #[test]
    fn too_many_elements_are_refused() {
        let with_children = |n: usize| format!("<r>{}</r>", "<c/>".repeat(n));
        // The root is one element, so 9 999 children make 10 000 elements, the limit.
        assert!(parse_strict(&with_children(9_999)).is_ok());
        // 10 000 children make 10 001 elements.
        assert_eq!(
            parse_strict(&with_children(10_000)).err(),
            Some(ParseFailure::Xml)
        );
    }

    #[test]
    fn too_many_attributes_are_refused() {
        let with_attributes = |n: usize| {
            let attributes: String = (0..n).map(|i| format!(" a{i}=\"\"")).collect();
            format!("<a{attributes}/>")
        };
        assert!(parse_strict(&with_attributes(64)).is_ok());
        assert_eq!(
            parse_strict(&with_attributes(65)).err(),
            Some(ParseFailure::Xml)
        );
    }

    #[test]
    fn strict_text_concatenates_text_and_cdata() {
        let doc = parse_strict("<a>x<![CDATA[y]]>z</a>").expect("well formed");
        assert_eq!(
            strict_text(&doc, root_element(&doc)),
            Some("xyz".to_owned())
        );
    }

    #[test]
    fn strict_text_refuses_a_comment() {
        let doc = parse_strict("<a>user@evil.com<!---->.attacker.com</a>").expect("well formed");
        assert_eq!(strict_text(&doc, root_element(&doc)), None);
    }

    #[test]
    fn strict_text_refuses_a_child_element_and_a_pi() {
        let doc = parse_strict("<a>x<b/></a>").expect("well formed");
        assert_eq!(strict_text(&doc, root_element(&doc)), None);
        let doc = parse_strict("<a>x<?p q?></a>").expect("well formed");
        assert_eq!(strict_text(&doc, root_element(&doc)), None);
    }

    #[test]
    fn strict_text_keeps_whitespace() {
        let doc = parse_strict("<a> x </a>").expect("well formed");
        assert_eq!(
            strict_text(&doc, root_element(&doc)),
            Some(" x ".to_owned())
        );
    }

    #[test]
    fn only_child_is_none_for_two() {
        let one = parse_strict("<r xmlns=\"urn:t\"><c/></r>").expect("well formed");
        assert!(only_child(&one, root_element(&one), "urn:t", "c").is_some());
        let two = parse_strict("<r xmlns=\"urn:t\"><c/><c/></r>").expect("well formed");
        assert_eq!(only_child(&two, root_element(&two), "urn:t", "c"), None);
    }

    #[test]
    fn attr_ignores_namespaced_attributes() {
        let doc = parse_strict("<a xmlns:p=\"u\" p:ID=\"1\"/>").expect("well formed");
        assert_eq!(attr(&doc, root_element(&doc), "ID"), None);
        let doc = parse_strict("<a xmlns:p=\"u\" p:ID=\"1\" ID=\"2\"/>").expect("well formed");
        assert_eq!(attr(&doc, root_element(&doc), "ID"), Some("2"));
    }

    #[test]
    fn escaping_covers_the_five_characters() {
        assert_eq!(escape_attr("&<>\"'"), "&amp;&lt;&gt;&quot;&apos;");
        assert_eq!(escape_text("a&b<c>"), "a&amp;b&lt;c&gt;");
    }
}
