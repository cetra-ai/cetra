//! The debug bundle's second guard (track TL § 4): log lines are payload-free by the AGENTS.md
//! rules, and shown config values are not secrets, but before either goes into a file meant
//! for a public issue, anything shaped like a password in a URL, a cetra key or an email
//! address is replaced by [`REDACTED`]. Plain byte scans, no regex: every delimiter is ASCII,
//! so every cut is on a character boundary.

/// What a hidden value reads.
pub(crate) const REDACTED: &str = "<redacted>";

/// The prefixes of cetra's API keys and personal access tokens.
const KEY_PREFIXES: [&str; 3] = ["sk-cetra-", "pk-cetra-", "pat-cetra-"];

/// `text` with URL user info, cetra keys and email addresses replaced by [`REDACTED`].
/// Idempotent: scrubbing the result changes nothing.
pub(crate) fn scrub_text(text: &str) -> String {
    let text = scrub_userinfo(text);
    let text = scrub_keys(&text);
    scrub_emails(&text)
}

/// A byte of a key's body.
fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

/// A byte of an email's local part. Bytes of non-ASCII characters count (`josé@…`): every
/// byte of a multi-byte character is >= 0x80, so a scan never stops inside one.
fn is_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte >= 0x80 || matches!(byte, b'.' | b'_' | b'%' | b'+' | b'-')
}

/// A byte of an email's domain (non-ASCII bytes count, as for the local part).
fn is_domain_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte >= 0x80 || matches!(byte, b'.' | b'-')
}

/// `scheme://user:password@host` becomes `scheme://<redacted>@host`: everything between
/// `://` and the last `@` before the next `/`, quote, backslash, whitespace or list or map
/// delimiter (`,`, `;`, `<`, `>`, `)`, `]`), so unquoted `key=value,key=value` text such as
/// `url=https://relay.example,user=bob@corp.example` does not swallow across the comma (the
/// email keeps its own form for `scrub_emails`). `?` and `#` do not end the scan, so a
/// password containing them is covered; the cost is over-redacting `https://h?x=a@b`, which
/// is harmless.
fn scrub_userinfo(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut search = 0;
    while let Some(found) = text[search..].find("://") {
        let start = search + found + 3;
        let mut end = start;
        let mut at = None;
        while end < bytes.len()
            && !matches!(
                bytes[end],
                b'/' | b'"' | b'\'' | b'\\' | b',' | b';' | b'<' | b'>' | b')' | b']'
            )
            && !bytes[end].is_ascii_whitespace()
        {
            if bytes[end] == b'@' {
                at = Some(end);
            }
            end += 1;
        }
        if let Some(at) = at {
            out.push_str(&text[copied..start]);
            out.push_str(REDACTED);
            copied = at;
        }
        // Resume right after this `://`, not where its scan stopped: a `://` that abuts the
        // previous URL (`://u:p@://k@h`) would otherwise be skipped on one pass and examined
        // on the next, and scrubbing must be idempotent.
        search = start;
    }
    out.push_str(&text[copied..]);
    out
}

/// A key prefix and the key bytes after it become [`REDACTED`].
fn scrub_keys(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let hit = KEY_PREFIXES
            .iter()
            .find(|prefix| bytes[i..].starts_with(prefix.as_bytes()));
        match hit {
            Some(prefix) => {
                let mut end = i + prefix.len();
                while end < bytes.len() && is_key_byte(bytes[end]) {
                    end += 1;
                }
                out.push_str(&text[copied..i]);
                out.push_str(REDACTED);
                copied = end;
                i = end;
            }
            None => i += 1,
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// `local@domain.tld` becomes [`REDACTED`] when the local part is not empty, the domain has
/// a dot, and its last label has at least two bytes and starts with a letter or a non-ASCII
/// byte (so a version such as `crate@1.40` is kept). A trailing `.` or `-` is punctuation,
/// not the domain.
fn scrub_emails(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        let mut start = i;
        while start > copied && is_local_byte(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = i + 1;
        while end < bytes.len() && is_domain_byte(bytes[end]) {
            end += 1;
        }
        while end > i + 1 && matches!(bytes[end - 1], b'.' | b'-') {
            end -= 1;
        }
        let domain = &text[i + 1..end];
        let tld = domain.rsplit('.').next().unwrap_or("");
        let email = start < i
            && domain.contains('.')
            && !domain.starts_with('.')
            && tld.len() >= 2
            && (tld.as_bytes()[0].is_ascii_alphabetic() || tld.as_bytes()[0] >= 0x80);
        if email {
            out.push_str(&text[copied..start]);
            out.push_str(REDACTED);
            copied = end;
            i = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inputs that hold an email, with what they must scrub to.
    const EMAILS: [(&str, &str); 6] = [
        ("ada@example.com", "<redacted>"),
        (
            r#"{"user":"bob.smith+x@corp.example.org"}"#,
            r#"{"user":"<redacted>"}"#,
        ),
        ("mail ada@example.com.", "mail <redacted>."),
        ("a@b.co and c@d.io", "<redacted> and <redacted>"),
        ("josé@example.com", "<redacted>"),
        ("ada@exämple.com", "<redacted>"),
    ];

    /// Inputs that must stay unchanged.
    const NOT_EMAILS: [&str; 7] = [
        "crate@1.40",
        "@user",
        "a @ b.com",
        "foo@localhost",
        "x@.com",
        "a@b.c",
        "",
    ];

    /// Inputs with a cetra key, with what they must scrub to.
    const KEYS: [(&str, &str); 6] = [
        ("key sk-cetra-AbC_12-x9 end", "key <redacted> end"),
        ("key pk-cetra-AbC_12-x9 end", "key <redacted> end"),
        ("key pat-cetra-AbC_12-x9 end", "key <redacted> end"),
        ("\"sk-cetra-\"", "\"<redacted>\""),
        ("sk-cetra-sk-cetra-abc", "<redacted>"),
        ("pat-cetra-", "<redacted>"),
    ];

    /// URL inputs, with what they must scrub to.
    const URLS: [(&str, &str); 13] = [
        ("https://u:p@h.example/x", "https://<redacted>@h.example/x"),
        (
            "smtp://user:pa%40ss@mail.example:587",
            "smtp://<redacted>@mail.example:587",
        ),
        ("\"s3://bucket/prefix\"", "\"s3://bucket/prefix\""),
        ("https://h.example/a@b", "https://h.example/a@b"),
        ("https://u:pa#ss@h.example", "https://<redacted>@h.example"),
        ("https://u:pa?ss@h.example", "https://<redacted>@h.example"),
        // No path: the scan runs to the end of the text.
        (
            "https://u:p@relay.example",
            "https://<redacted>@relay.example",
        ),
        // The userinfo scan stops at list delimiters, so an email in the next field is
        // scrubbed whole (no domain left behind) and the host before it is kept.
        (
            "url=https://relay.example,user=bob@corp.example",
            "url=https://relay.example,user=<redacted>",
        ),
        (
            "https://u:p@relay.example,x@y.example",
            "https://<redacted>@relay.example,<redacted>",
        ),
        (
            "https://u:p@relay.example;user=bob@corp.example",
            "https://<redacted>@relay.example;user=<redacted>",
        ),
        (
            "(https://relay.example) bob@corp.example",
            "(https://relay.example) <redacted>",
        ),
        (
            "see <https://u:p@relay.example>",
            "see <https://<redacted>@relay.example>",
        ),
        (
            "[https://relay.example] a@b.example",
            "[https://relay.example] <redacted>",
        ),
    ];

    #[test]
    fn emails_are_redacted() {
        for (input, expected) in EMAILS {
            assert_eq!(scrub_text(input), expected, "input: {input}");
        }
    }

    #[test]
    fn non_emails_are_kept() {
        for input in NOT_EMAILS {
            assert_eq!(scrub_text(input), input, "input: {input:?}");
        }
    }

    #[test]
    fn keys_are_redacted() {
        for (input, expected) in KEYS {
            assert_eq!(scrub_text(input), expected, "input: {input}");
        }
    }

    #[test]
    fn url_user_info_is_redacted() {
        for (input, expected) in URLS {
            assert_eq!(scrub_text(input), expected, "input: {input}");
        }
    }

    #[test]
    fn non_ascii_text_around_matches_is_kept() {
        assert_eq!(
            scrub_text("é ada@example.com ü sk-cetra-x ✓"),
            "é <redacted> ü <redacted> ✓"
        );
    }

    #[test]
    fn scrub_is_idempotent() {
        let inputs = EMAILS
            .iter()
            .map(|(input, _)| *input)
            .chain(NOT_EMAILS)
            .chain(KEYS.iter().map(|(input, _)| *input))
            .chain(URLS.iter().map(|(input, _)| *input));
        for input in inputs {
            let once = scrub_text(input);
            assert_eq!(scrub_text(&once), once, "input: {input}");
        }
    }

    #[test]
    fn scrub_is_total_and_idempotent_on_a_small_alphabet() {
        let pieces = [
            "a",
            "@",
            ".",
            "-",
            "é",
            "sk-cetra-",
            "://",
            ":",
            "/",
            "\"",
            ",",
            ";",
            "<",
            ">",
        ];
        for len in 1..=5u32 {
            let total = pieces.len().pow(len);
            for n in 0..total {
                let mut input = String::new();
                let mut k = n;
                for _ in 0..len {
                    input.push_str(pieces[k % pieces.len()]);
                    k /= pieces.len();
                }
                let once = scrub_text(&input);
                assert_eq!(scrub_text(&once), once, "input: {input:?}");
            }
        }
    }
}
