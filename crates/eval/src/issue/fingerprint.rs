//! Error fingerprints (D-IC-2, D-IC-17): message normalization and the stored fingerprint.

use std::fmt;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use sha2::{Digest, Sha256};

/// Longest normalized message, in characters (D-IC-2).
pub const MAX_MESSAGE_CHARS: usize = 512;

/// Only the first 8 192 characters are read; volatile parts beyond them do not matter, since the result is cut at 512.
pub const MAX_INPUT_CHARS: usize = 8_192;

/// Label hashed first in every fingerprint (D-IC-17).
const LABEL: &[u8] = b"cetra.issue-fp.v1";

const URL: &str = r##"(?i)\b([a-z][a-z0-9+.\-]*)://([^\s/?#"'<>]+)[^\s"'<>]*"##;
const EMAIL: &str = r##"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"##;
const UUID: &str = r##"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"##;
const TIME: &str =
    r##"\b\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:[.,]\d+)?)?(?:Z|[+\-]\d{2}:?\d{2})?)?\b"##;
const IPV4: &str = r##"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b"##;
const IPV6: &str = r##"(?i)\b(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}\b"##;
const PATH: &str = r##"(?:[A-Za-z]:)?(?:[\\/][\w.@\-]+){2,}[\\/]?"##;
const HEX: &str = r##"\b[0-9A-Fa-f]{8,}\b"##;
const QUOTED: &str = r##""[^"\n]{3,}"|'[^'\n]{3,}'|`[^`\n]{3,}`"##;
const NUMBER: &str = r##"\d+(?:\.\d+)?"##;

#[derive(Clone, Copy)]
enum RuleKind {
    Url,
    Email,
    Uuid,
    Time,
    Ip,
    Path,
    Hex,
    Quoted,
    Number,
}

struct Rule {
    regex: Regex,
    kind: RuleKind,
}

/// The ten rules, in the order of D-IC-2.
static RULES: LazyLock<[Rule; 10]> = LazyLock::new(|| {
    [
        rule(URL, RuleKind::Url),
        rule(EMAIL, RuleKind::Email),
        rule(UUID, RuleKind::Uuid),
        rule(TIME, RuleKind::Time),
        rule(IPV4, RuleKind::Ip),
        rule(IPV6, RuleKind::Ip),
        rule(PATH, RuleKind::Path),
        rule(HEX, RuleKind::Hex),
        rule(QUOTED, RuleKind::Quoted),
        rule(NUMBER, RuleKind::Number),
    ]
});

#[allow(clippy::expect_used)]
fn rule(pattern: &str, kind: RuleKind) -> Rule {
    // The patterns are constants, compiled by every test.
    let regex = Regex::new(pattern).expect("the fingerprint patterns are constants");
    Rule { regex, kind }
}

impl Rule {
    /// The placeholder for one match, or `None` to leave the match as plain text.
    fn replacement(&self, caps: &Captures<'_>) -> Option<String> {
        match self.kind {
            RuleKind::Url => Some(url_placeholder(caps)),
            RuleKind::Email => Some("<email>".to_owned()),
            RuleKind::Uuid => Some("<uuid>".to_owned()),
            RuleKind::Time => Some("<time>".to_owned()),
            RuleKind::Ip => Some("<ip>".to_owned()),
            RuleKind::Path => Some("<path>".to_owned()),
            RuleKind::Hex => {
                let whole = caps.get(0).map_or("", |m| m.as_str());
                let has_letter = whole.bytes().any(|b| b.is_ascii_alphabetic());
                let has_digit = whole.bytes().any(|b| b.is_ascii_digit());
                (has_letter && has_digit).then(|| "<hex>".to_owned())
            }
            RuleKind::Quoted => Some("<str>".to_owned()),
            RuleKind::Number => Some("<n>".to_owned()),
        }
    }
}

/// `<url:SCHEME://HOST>`: the lowercased scheme, and the host without userinfo, port or path.
fn url_placeholder(caps: &Captures<'_>) -> String {
    let scheme = caps.get(1).map_or("", |m| m.as_str()).to_ascii_lowercase();
    let authority = caps.get(2).map_or("", |m| m.as_str());
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_ascii_lowercase();
    let host = if host_port.starts_with('[') {
        host_port.find(']').map_or(host_port.as_str(), |end| {
            host_port.get(..=end).unwrap_or("")
        })
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            _ => host_port.as_str(),
        }
    };
    format!("<url:{scheme}://{host}>")
}

/// A piece of the message. Frozen pieces are placeholders that no later rule may rewrite.
struct Segment {
    text: String,
    frozen: bool,
}

fn push_text(out: &mut Vec<Segment>, text: &str) {
    if !text.is_empty() {
        out.push(Segment {
            text: text.to_owned(),
            frozen: false,
        });
    }
}

/// Runs one rule on the unfrozen segments only. Each accepted match becomes a frozen segment.
fn apply_rule(segments: Vec<Segment>, rule: &Rule) -> Vec<Segment> {
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        if segment.frozen {
            out.push(segment);
            continue;
        }
        let mut last = 0;
        for caps in rule.regex.captures_iter(&segment.text) {
            let Some(whole) = caps.get(0) else { continue };
            let Some(placeholder) = rule.replacement(&caps) else {
                continue;
            };
            push_text(
                &mut out,
                segment.text.get(last..whole.start()).unwrap_or(""),
            );
            out.push(Segment {
                text: placeholder,
                frozen: true,
            });
            last = whole.end();
        }
        push_text(&mut out, segment.text.get(last..).unwrap_or(""));
    }
    out
}

/// Replaces volatile parts of an error message with placeholders (D-IC-2, D-IC-17).
///
/// Only the first [`MAX_INPUT_CHARS`] characters are read, so the cost is bounded. The ten rules
/// run in order. A later rule never rewrites inside an earlier placeholder.
/// Whitespace runs collapse to one space, the result is trimmed and cut to
/// [`MAX_MESSAGE_CHARS`] characters.
///
/// Known limits, accepted: compressed IPv6 (`fe80::1`) is not recognized; a date such as
/// `11/20/2026` becomes path-like text; apostrophes can pair up as quotes (`don't ... can't`
/// becomes `<str>`).
pub fn normalize_message(message: &str) -> String {
    // Cut at a char boundary: `char_indices` yields only boundaries, so `get` never fails here.
    let head = match message.char_indices().nth(MAX_INPUT_CHARS) {
        Some((end, _)) => message.get(..end).unwrap_or(message),
        None => message,
    };
    let mut segments = vec![Segment {
        text: head.to_owned(),
        frozen: false,
    }];
    for rule in RULES.iter() {
        segments = apply_rule(segments, rule);
    }
    let joined: String = segments.iter().map(|s| s.text.as_str()).collect();

    let mut collapsed = String::with_capacity(joined.len());
    let mut in_space = false;
    for c in joined.chars() {
        if c.is_whitespace() {
            if !in_space {
                collapsed.push(' ');
                in_space = true;
            }
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    let cut: String = collapsed.trim().chars().take(MAX_MESSAGE_CHARS).collect();
    cut.trim_end().to_owned()
}

/// The four fields of an error fingerprint. An absent field is `None`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ErrorFields<'a> {
    pub service_name: Option<&'a str>,
    pub span_name: &'a str,
    pub exception_type: Option<&'a str>,
    /// Already normalized with [`normalize_message`].
    pub message: Option<&'a str>,
}

/// Prints the field lengths only: the fields can carry payload text.
impl fmt::Debug for ErrorFields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErrorFields")
            .field("service_name_len", &self.service_name.map(str::len))
            .field("span_name_len", &self.span_name.len())
            .field("exception_type_len", &self.exception_type.map(str::len))
            .field("message_len", &self.message.map(str::len))
            .finish()
    }
}

/// First 16 bytes of the SHA-256 fingerprint (D-IC-17).
///
/// Hashes the label `cetra.issue-fp.v1`, then for each field in the order service name, span
/// name, exception type, message: its UTF-8 byte length as a `u32` big-endian (saturated), then
/// its bytes. An absent field has length 0 and no bytes.
pub fn fingerprint(fields: &ErrorFields<'_>) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(LABEL);
    for field in [
        fields.service_name,
        Some(fields.span_name),
        fields.exception_type,
        fields.message,
    ] {
        let bytes = field.map_or(&[][..], str::as_bytes);
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        hasher.update(len.to_be_bytes());
        hasher.update(bytes);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// [`fingerprint`] as 32 lowercase hex characters, the stored form.
pub fn fingerprint_hex(fields: &ErrorFields<'_>) -> String {
    hex::encode(fingerprint(fields))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use sha2::{Digest, Sha256};

    use super::*;

    fn normalized(message: &str) -> String {
        normalize_message(message)
    }

    #[test]
    fn normalizes_urls_to_scheme_and_host() {
        assert_eq!(
            normalized("Connection to https://user@API.Example.com:443/v1/x?id=7 failed after 30s"),
            "Connection to <url:https://api.example.com> failed after <n>s"
        );
        assert_eq!(
            normalized("GET http://[::1]:8080/x"),
            "GET <url:http://[::1]>"
        );
        assert_eq!(normalized("https://h.io/2026-01-01"), "<url:https://h.io>");
    }

    #[test]
    fn normalizes_emails_uuids_times_and_ips() {
        assert_eq!(
            normalized("user bob@example.org not found"),
            "user <email> not found"
        );
        assert_eq!(
            normalized("order 3f2a1b9c-1d2e-4f5a-8b6c-7d8e9f0a1b2c missing"),
            "order <uuid> missing"
        );
        assert_eq!(
            normalized("expired at 2026-11-20T10:00:00Z"),
            "expired at <time>"
        );
        assert_eq!(
            normalized("from 10.0.0.12:5432 and 2001:db8:0:0:0:0:0:1"),
            "from <ip> and <ip>"
        );
    }

    #[test]
    fn normalizes_paths_hex_quotes_and_numbers() {
        assert_eq!(
            normalized("open /var/lib/cetra/data.db: denied"),
            "open <path>: denied"
        );
        assert_eq!(normalized("C:\\Users\\x\\a.txt"), "<path>");
        assert_eq!(
            normalized("hash deadbeef42 vs 12345678"),
            "hash <hex> vs <n>"
        );
        assert_eq!(
            normalized("key \"customer-name\" invalid, 'ab' ok"),
            "key <str> invalid, 'ab' ok"
        );
        assert_eq!(
            normalized("took 1.5 ms (retry 3)"),
            "took <n> ms (retry <n>)"
        );
    }

    #[test]
    fn collapses_and_trims_whitespace() {
        assert_eq!(normalized("  a \n\t b  "), "a b");
    }

    #[test]
    fn normalize_cuts_at_512_chars_on_a_char_boundary() {
        let message = "é".repeat(600);
        let out = normalized(&message);
        assert_eq!(out.chars().count(), 512);
    }

    #[test]
    fn normalize_reads_only_the_first_8192_chars() {
        let head = "a".repeat(MAX_INPUT_CHARS);
        let with_tail = format!("{head}123");
        assert_eq!(normalized(&with_tail), normalized(&head));
        // Whitespace collapses, so this differs from the uncapped result ("abc") and proves the
        // cut: the tail is beyond the first 8 192 characters and is never read.
        let spaces_then_word = format!("{}abc", " ".repeat(MAX_INPUT_CHARS));
        assert_eq!(normalized(&spaces_then_word), "");
    }

    #[test]
    fn fingerprint_golden() {
        let fields = ErrorFields {
            service_name: Some("api"),
            span_name: "GET /v1/items",
            exception_type: Some("TimeoutError"),
            message: Some("timed out after <n>s"),
        };
        // The layout of D-IC-17, written out by hand.
        let mut bytes = LABEL.to_vec();
        for part in [
            "api",
            "GET /v1/items",
            "TimeoutError",
            "timed out after <n>s",
        ] {
            let len = u32::try_from(part.len()).unwrap_or(u32::MAX);
            bytes.extend(len.to_be_bytes());
            bytes.extend(part.as_bytes());
        }
        let digest = Sha256::digest(&bytes);
        let expected = hex::encode(&digest[..16]);
        assert_eq!(fingerprint_hex(&fields), expected);
        assert_eq!(fingerprint_hex(&fields), "6e1fbc8e90baeb14a97f6394daf2f8a8");
    }

    #[test]
    fn fingerprint_separates_fields() {
        let base = ErrorFields {
            service_name: None,
            span_name: "",
            exception_type: None,
            message: None,
        };
        let split_ab_c = ErrorFields {
            service_name: Some("ab"),
            span_name: "c",
            ..base
        };
        let split_a_bc = ErrorFields {
            service_name: Some("a"),
            span_name: "bc",
            ..base
        };
        assert_ne!(fingerprint(&split_ab_c), fingerprint(&split_a_bc));
    }

    #[test]
    fn fingerprint_is_stable_across_volatile_parts() {
        let first = normalized("order 3f2a1b9c-1d2e-4f5a-8b6c-7d8e9f0a1b2c missing 12");
        let second = normalized("order 00000000-0000-4000-8000-000000000001 missing 99");
        let fields = |message| ErrorFields {
            service_name: Some("api"),
            span_name: "GET /v1/items",
            exception_type: None,
            message: Some(message),
        };
        assert_eq!(fingerprint(&fields(&first)), fingerprint(&fields(&second)));
    }

    #[test]
    fn debug_prints_lengths_not_text() {
        let fields = ErrorFields {
            service_name: None,
            span_name: "span",
            exception_type: None,
            message: Some("SECRET-PAYLOAD"),
        };
        let text = format!("{fields:?}");
        assert!(!text.contains("SECRET-PAYLOAD"));
        assert!(text.contains("message_len"));
    }

    proptest! {
        #![proptest_config(crate::hostile_tests::config())]

        #[test]
        fn normalize_never_panics_and_is_bounded(message in any::<String>()) {
            let out = normalize_message(&message);
            prop_assert!(out.chars().count() <= MAX_MESSAGE_CHARS);
            prop_assert_eq!(out.trim(), out.as_str());
        }
    }
}
