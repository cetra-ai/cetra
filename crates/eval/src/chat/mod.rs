//! Provider chat calls: one request shape for the OpenAI-compatible, Anthropic and
//! Gemini APIs (D21, D-V-19). Judges send it without streaming (D-V-29); the
//! playground streams it.

mod http;

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

pub(crate) use http::provider_error;

use crate::error::ChatError;
use crate::judge::JudgeConnection;
use crate::secret::Secret;

/// Largest provider response body, in bytes (D159).
pub(crate) const RESPONSE_MAX_BYTES: usize = 4 * 1024 * 1024;

const MODEL_MAX_BYTES: usize = 256;

/// Token counts in OTel semantics. Input includes cached prompt tokens. Output includes reasoning tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenUsage {
    /// Prompt tokens, including cached tokens.
    pub input: u64,
    /// Completion tokens, including reasoning tokens.
    pub output: u64,
    /// Cached prompt tokens.
    pub cache_read: u64,
    /// Prompt tokens written to the cache.
    pub cache_write: u64,
    /// Reasoning tokens. They are also included in `output`.
    pub reasoning: u64,
}

/// Rejects a model name that would be placed in a URL or a JSON body.
pub(crate) fn validate_model(model: &str) -> Result<(), ChatError> {
    if model.is_empty() || model.len() > MODEL_MAX_BYTES || model.chars().any(forbidden_in_model) {
        return Err(ChatError::InvalidModel);
    }
    Ok(())
}

/// The URL parser reads `\` as `/` in http(s) paths, and `%` starts an escape.
fn forbidden_in_model(ch: char) -> bool {
    ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '\\' | '%' | '?' | '#')
}

/// Joins `path` onto a provider base, preserving a query string.
///
/// A base without a trailing slash is treated as a directory. `set_path` drops
/// the query, so it is written back for bases such as `?api-version=`.
pub(crate) fn endpoint(
    base: Option<&Url>,
    default_base: &str,
    path: &str,
) -> Result<Url, ChatError> {
    let parsed;
    let base = match base {
        Some(base) => base,
        None => {
            parsed = Url::parse(default_base).map_err(|_| ChatError::InvalidRequest)?;
            &parsed
        }
    };
    let query = base.query().map(ToOwned::to_owned);
    let mut directory = base.clone();
    if !directory.path().ends_with('/') {
        let mut with_slash = directory.path().to_owned();
        with_slash.push('/');
        directory.set_path(&with_slash);
    }
    let mut url = directory
        .join(path)
        .map_err(|_| ChatError::InvalidRequest)?;
    if let Some(query) = query {
        url.set_query(Some(&query));
    }
    Ok(url)
}

/// Request headers: the connection's extra headers first, then `own`, which replace an
/// extra header of the same name (D-I-46).
pub(crate) fn request_headers(
    conn: &JudgeConnection,
    own: Vec<(HeaderName, HeaderValue)>,
) -> Result<Vec<(HeaderName, HeaderValue)>, ChatError> {
    let mut map = HeaderMap::new();
    for (name, value) in &conn.extra_headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| ChatError::InvalidHeader)?;
        map.insert(name, secret_header(value)?);
    }
    for (name, value) in own {
        map.insert(name, value);
    }
    Ok(map
        .into_iter()
        .filter_map(|(name, value)| name.map(|name| (name, value)))
        .collect())
}

/// A secret as a header value.
pub(crate) fn secret_header(value: &Secret) -> Result<HeaderValue, ChatError> {
    HeaderValue::from_bytes(value.expose().as_bytes()).map_err(|_| ChatError::InvalidHeader)
}

#[cfg(test)]
mod tests {
    use super::request_headers;
    use crate::error::ChatError;
    use crate::judge::{Adapter, JudgeConnection, StructuredOutput};
    use crate::secret::Secret;
    use reqwest::header::{HeaderName, HeaderValue};

    #[test]
    fn request_headers_put_own_headers_last() {
        let conn = JudgeConnection {
            adapter: Adapter::OpenAi,
            base_url: None,
            api_key: Secret::new("sk-test".to_owned()),
            extra_headers: vec![
                ("X-A".to_owned(), Secret::new("1".to_owned())),
                (
                    "Content-Type".to_owned(),
                    Secret::new("text/plain".to_owned()),
                ),
            ],
            structured_output: StructuredOutput::Native,
        };
        let own = vec![(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        )];
        let headers = request_headers(&conn, own).expect("headers");
        let values = |name: &str| -> Vec<String> {
            headers
                .iter()
                .filter(|(header, _)| header.as_str() == name)
                .map(|(_, value)| value.to_str().expect("ascii").to_owned())
                .collect()
        };
        assert_eq!(values("x-a"), vec!["1".to_owned()]);
        assert_eq!(values("content-type"), vec!["application/json".to_owned()]);

        let bad = JudgeConnection {
            extra_headers: vec![("bad name".to_owned(), Secret::new("v".to_owned()))],
            ..conn
        };
        assert_eq!(
            request_headers(&bad, Vec::new()),
            Err(ChatError::InvalidHeader)
        );
    }
}
