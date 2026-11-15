// SPDX-License-Identifier: Elastic-2.0
//! TXT lookups for domain verification (D-P-54, D-P-64).

use std::fmt;
use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use hickory_resolver::proto::rr::rdata::TXT;

use crate::auth::AuthError;
use crate::auth::secret::random_bytes;

/// One lookup, including the wait for the answer (D-P-64).
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a lookup failed. No variant holds the name.
#[derive(Debug, Clone, Copy, thiserror::Error)]
pub(crate) enum DnsError {
    /// No usable system resolver configuration.
    #[error("no DNS resolver is configured")]
    Unconfigured,
    /// The lookup failed or timed out.
    #[error("DNS lookup failed")]
    Lookup,
}

/// Where TXT records come from. Tests use `Fixed`, since they have no network.
pub(crate) enum TxtResolver {
    /// The system resolver (`/etc/resolv.conf`), with its cache off.
    System(Box<TokioResolver>),
    /// The system configuration could not be read at startup.
    Unconfigured,
    /// Records set by a test.
    #[cfg(test)]
    Fixed(FixedTxt),
}

impl fmt::Debug for TxtResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System(_) => formatter.write_str("TxtResolver::System"),
            Self::Unconfigured => formatter.write_str("TxtResolver::Unconfigured"),
            #[cfg(test)]
            Self::Fixed(_) => formatter.write_str("TxtResolver::Fixed"),
        }
    }
}

/// Test records, and a switch that makes every lookup fail (P8's 503 test).
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct FixedTxt {
    records: std::sync::Mutex<std::collections::HashMap<String, Vec<String>>>,
    fail: std::sync::atomic::AtomicBool,
}

impl TxtResolver {
    /// The system resolver; `Unconfigured` (with one `warn`, no detail) when it cannot be built.
    ///
    /// The cache is off, so a record added a moment ago is seen at once (D-P-64).
    pub(crate) fn system() -> Self {
        let built = TokioResolver::builder_tokio().and_then(|mut builder| {
            builder.options_mut().cache_size = 0;
            builder.build()
        });
        match built {
            Ok(resolver) => Self::System(Box::new(resolver)),
            Err(_) => {
                tracing::warn!("no usable DNS configuration; org domain verification is off");
                Self::Unconfigured
            }
        }
    }

    /// An empty fixed resolver.
    #[cfg(test)]
    pub(crate) fn fixed() -> Self {
        Self::Fixed(FixedTxt::default())
    }

    /// Sets the records of `name` (tests; no-op on another variant).
    #[cfg(test)]
    pub(crate) fn set(&self, name: &str, values: &[&str]) {
        if let Self::Fixed(fixed) = self {
            let values = values.iter().map(|value| (*value).to_owned()).collect();
            fixed
                .records
                .lock()
                .expect("test records lock")
                .insert(name.trim_end_matches('.').to_owned(), values);
        }
    }

    /// Makes every lookup of the fixed resolver fail with `Lookup` (tests).
    #[cfg(test)]
    pub(crate) fn set_fail(&self, fail: bool) {
        if let Self::Fixed(fixed) = self {
            fixed.fail.store(fail, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The TXT strings at `name`, each record's character-strings joined (RFC 7208 § 3.3).
    /// No record and NXDOMAIN are `Ok(vec![])`. 10 s timeout.
    ///
    /// # Errors
    ///
    /// Returns [`DnsError::Unconfigured`] without a system configuration, and
    /// [`DnsError::Lookup`] when the lookup fails or times out.
    pub(crate) async fn txt(&self, name: &str) -> Result<Vec<String>, DnsError> {
        match self {
            Self::Unconfigured => Err(DnsError::Unconfigured),
            Self::System(resolver) => {
                // An absolute name, so no search domain from resolv.conf is tried.
                let absolute = if name.ends_with('.') {
                    name.to_owned()
                } else {
                    format!("{name}.")
                };
                let answer =
                    tokio::time::timeout(LOOKUP_TIMEOUT, resolver.txt_lookup(absolute.as_str()))
                        .await
                        .map_err(|_| DnsError::Lookup)?;
                match answer {
                    Ok(lookup) => Ok(lookup
                        .answers()
                        .iter()
                        .filter_map(|record| match &record.data {
                            RData::TXT(txt) => Some(txt_string(txt)),
                            _ => None,
                        })
                        .collect()),
                    Err(error) if error.is_no_records_found() => Ok(Vec::new()),
                    Err(_) => Err(DnsError::Lookup),
                }
            }
            #[cfg(test)]
            Self::Fixed(fixed) => fixed.lookup(name),
        }
    }
}

#[cfg(test)]
impl FixedTxt {
    fn lookup(&self, name: &str) -> Result<Vec<String>, DnsError> {
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(DnsError::Lookup);
        }
        let records = self.records.lock().expect("test records lock");
        Ok(records
            .get(name.trim_end_matches('.'))
            .cloned()
            .unwrap_or_default())
    }
}

/// The character-strings of one TXT record, joined and decoded.
fn txt_string(txt: &TXT) -> String {
    let bytes: Vec<u8> = txt
        .txt_data
        .iter()
        .flat_map(|segment| segment.iter().copied())
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `_llmobs-verify.<domain>`.
pub(crate) fn record_name(domain: &str) -> String {
    format!(
        "{}.{domain}",
        concat!("_", llmobs_core::llmobs_name!("-verify"))
    )
}

/// `llmobs-verify=<token>`.
pub(crate) fn record_value(token: &str) -> String {
    format!("{}={token}", llmobs_core::llmobs_name!("-verify"))
}

/// A new token: 16 random bytes as 32 lowercase hex characters.
///
/// # Errors
///
/// Returns [`AuthError`] when the random source fails.
pub(crate) fn new_token() -> Result<String, AuthError> {
    Ok(hex::encode(random_bytes(16)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_name_and_value_use_the_product_prefix() {
        assert_eq!(record_name("acme.test"), "_llmobs-verify.acme.test");
        assert_eq!(record_value("ab"), "llmobs-verify=ab");
    }

    #[test]
    fn tokens_are_32_lowercase_hex_characters_and_differ() {
        let first = new_token().expect("token");
        let second = new_token().expect("token");
        assert_eq!(first.len(), 32);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn fixed_resolver_returns_what_was_set() {
        let resolver = TxtResolver::fixed();
        resolver.set("acme.test", &["llmobs-verify=ab"]);
        assert_eq!(
            resolver.txt("acme.test").await.unwrap(),
            vec!["llmobs-verify=ab".to_owned()]
        );
        assert_eq!(
            resolver.txt("acme.test.").await.unwrap(),
            vec!["llmobs-verify=ab".to_owned()]
        );
        assert!(resolver.txt("other.test").await.unwrap().is_empty());

        resolver.set_fail(true);
        assert!(matches!(
            resolver.txt("acme.test").await,
            Err(DnsError::Lookup)
        ));
        resolver.set_fail(false);
        assert_eq!(resolver.txt("acme.test").await.unwrap().len(), 1);
    }

    #[test]
    fn system_resolver_builds_without_a_query() {
        // Building reads the system configuration only; no lookup is sent.
        let resolver = TxtResolver::system();
        let text = format!("{resolver:?}");
        assert!(
            text == "TxtResolver::System" || text == "TxtResolver::Unconfigured",
            "{text}"
        );
    }

    #[tokio::test]
    async fn unconfigured_resolver_refuses_every_lookup() {
        let error = TxtResolver::Unconfigured
            .txt("acme.test")
            .await
            .expect_err("unconfigured");
        assert!(matches!(error, DnsError::Unconfigured));
        assert_eq!(error.to_string(), "no DNS resolver is configured");
    }
}
