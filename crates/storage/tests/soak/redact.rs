//! Secrets out of every committed file (track R M7, R7.5): the bucket, the S3
//! endpoint host, `$HOME` and the hostname are replaced by placeholders.

use crate::common::s3::S3Env;

/// Shortest secret accepted: a shorter one would rewrite ordinary words.
const MIN_SECRET_BYTES: usize = 4;

#[derive(Debug, Clone)]
pub(crate) struct Redactor {
    pairs: Vec<(String, &'static str)>,
}

impl Redactor {
    /// Secrets of this run, each with its placeholder: the S3 bucket → "<bucket>",
    /// the S3 endpoint host unless it is 127.0.0.1 or localhost → "<endpoint>",
    /// $HOME → "~", the hostname (/proc/sys/kernel/hostname) → "<host>".
    /// Empty values are skipped. A secret shorter than 4 bytes panics.
    pub(crate) fn for_run(s3: Option<&S3Env>) -> Self {
        let mut pairs = Vec::new();
        if let Some(s3) = s3 {
            pairs.push((s3.bucket.clone(), "<bucket>"));
            let rest = s3
                .endpoint
                .split_once("://")
                .map_or(s3.endpoint.as_str(), |(_, rest)| rest);
            let host = rest.split(['/', ':']).next().unwrap_or("");
            if host != "127.0.0.1" && host != "localhost" {
                pairs.push((host.to_owned(), "<endpoint>"));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            pairs.push((home, "~"));
        }
        if let Ok(host) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            pairs.push((host.trim().to_owned(), "<host>"));
        }
        pairs.retain(|(secret, _)| !secret.is_empty());
        for (secret, _) in &pairs {
            assert!(
                secret.len() >= MIN_SECRET_BYTES,
                "a secret of this run is shorter than {MIN_SECRET_BYTES} bytes: rename the bucket or the host"
            );
        }
        // Longest first, so a secret that contains another is replaced whole.
        pairs.sort_by_key(|(secret, _)| std::cmp::Reverse(secret.len()));
        Self { pairs }
    }

    /// Every occurrence replaced, longest secret first. A match must sit on a
    /// boundary: the bytes before and after it are not `[A-Za-z0-9_-]`, so a
    /// short name can't rewrite JSON keys or other words.
    pub(crate) fn apply(&self, text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        'scan: while i < text.len() {
            for (secret, placeholder) in &self.pairs {
                let end = i + secret.len();
                if text.get(i..end) == Some(secret.as_str())
                    && (i == 0 || !is_word(bytes[i - 1]))
                    && (end >= bytes.len() || !is_word(bytes[end]))
                {
                    out.push_str(placeholder);
                    i = end;
                    continue 'scan;
                }
            }
            let Some(ch) = text[i..].chars().next() else {
                break;
            };
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}
