//! Outbound HTTP for judges and webhooks, with the SSRF guard (D166).
//!
//! `no_proxy` is required. An `HTTP_PROXY` would connect to the proxy and skip
//! this resolver, so the guard would never see the target address.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{StatusCode, Url};

use crate::error::EgressError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request timeout of an identity provider's request (D-P-53).
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether resolved and literal addresses in a private range may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressPolicy {
    /// Keep addresses that [`is_denied`] would drop.
    pub allow_private_network: bool,
    /// Restore mode (N M3, D-N-25): every request is refused before it is sent.
    pub restore_mode: bool,
}

impl EgressPolicy {
    /// Whether a request to `url` may be attempted: `http` or `https`, and a literal IP host
    /// that is not denied (D166) unless private networks are allowed. A host name is allowed
    /// here; the resolver checks its addresses. Restore mode allows nothing.
    #[must_use]
    pub fn allows_url(&self, url: &Url) -> bool {
        !self.restore_mode
            && matches!(url.scheme(), "http" | "https")
            && (self.allow_private_network || !literal_ip(url).is_some_and(is_denied))
    }
}

/// One shared client with the SSRF guard (D166). Redirects are not followed; proxies are ignored.
#[derive(Debug, Clone)]
pub struct EgressClient {
    http: reqwest::Client,
    policy: EgressPolicy,
}

/// One POST. The caller owns the URL, the timeout, and the body cap.
///
/// [`Debug`](std::fmt::Debug) is hand-written: it prints the URL's host, the header names,
/// the body's length, and the limits, never the URL's path or query, the header values
/// (the API key), or the body.
pub struct EgressRequest<'a> {
    /// Destination. Only http and https are allowed.
    pub url: &'a Url,
    /// Extra request headers, in order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Request body, sent as is.
    pub body: Vec<u8>,
    /// Whole-request timeout.
    pub timeout: Duration,
    /// Larger bodies fail with [`EgressError::TooLarge`].
    pub max_response_bytes: usize,
}

impl fmt::Debug for EgressRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("EgressRequest")
            .field("url_host", &self.url.host_str())
            .field("headers", &headers)
            .field("body_bytes", &self.body.len())
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

/// The status and body of one response. The body is at most the request's cap.
#[derive(Debug)]
pub struct EgressResponse {
    /// HTTP status.
    pub status: StatusCode,
    /// Body, at most the request's cap.
    pub body: Bytes,
}

/// One streamed POST. The caller owns the URL, both timeouts, and the body cap.
///
/// [`Debug`](std::fmt::Debug) is hand-written, as for [`EgressRequest`].
pub struct EgressStreamRequest<'a> {
    /// Destination. Only http and https are allowed.
    pub url: &'a Url,
    /// Extra request headers, in order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Request body, sent as is.
    pub body: Vec<u8>,
    /// Whole-request timeout, from connecting to the last body byte.
    pub timeout: Duration,
    /// Longest wait for the response head, and between two body chunks.
    pub idle_timeout: Duration,
    /// More body bytes than this fail with [`EgressError::TooLarge`].
    pub max_response_bytes: usize,
}

impl fmt::Debug for EgressStreamRequest<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headers: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        formatter
            .debug_struct("EgressStreamRequest")
            .field("url_host", &self.url.host_str())
            .field("headers", &headers)
            .field("body_bytes", &self.body.len())
            .field("timeout", &self.timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

/// A response whose body is read chunk by chunk. Dropping it closes the connection.
///
/// [`Debug`](std::fmt::Debug) is hand-written: `reqwest::Response`'s prints the URL.
pub struct EgressStream {
    status: StatusCode,
    response: reqwest::Response,
    host: String,
    idle_timeout: Duration,
    max_response_bytes: usize,
    read: usize,
}

impl fmt::Debug for EgressStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressStream")
            .field("status", &self.status)
            .field("host", &self.host)
            .field("read", &self.read)
            .finish_non_exhaustive()
    }
}

/// A client for an identity provider an org registered (D-P-53): the same guarded
/// resolver as [`EgressClient`], no redirects, no proxy, rustls, a 10 s whole-request timeout.
///
/// This client does **not** guard IP-literal hosts: reqwest does not call the resolver for
/// them. Every caller checks [`EgressPolicy::allows_url`] before sending, as
/// [`EgressClient::post`] does.
///
/// # Errors
///
/// Returns [`EgressError::Request`] when the TLS backend cannot be initialized.
pub fn identity_http_client(policy: EgressPolicy) -> Result<reqwest::Client, EgressError> {
    client_builder(policy)
        .timeout(IDENTITY_TIMEOUT)
        .build()
        .map_err(|_| EgressError::Request)
}

fn client_builder(policy: EgressPolicy) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(user_agent())
        .use_rustls_tls()
        .dns_resolver(Arc::new(GuardedResolver {
            allow_private_network: policy.allow_private_network,
            restore_mode: policy.restore_mode,
        }))
}

impl EgressClient {
    /// Builds a rustls client that resolves through [`GuardedResolver`].
    ///
    /// # Errors
    ///
    /// Returns [`EgressError::Request`] when the TLS backend cannot be initialized.
    pub fn new(policy: EgressPolicy) -> Result<Self, EgressError> {
        let http = client_builder(policy)
            .build()
            .map_err(|_| EgressError::Request)?;
        Ok(Self { http, policy })
    }

    /// POSTs `req` and reads the body until the cap.
    ///
    /// # Errors
    ///
    /// Returns [`EgressError::Scheme`] for a non-HTTP URL, [`EgressError::Denied`]
    /// when the literal address or every resolved address is denied, and
    /// [`EgressError::TooLarge`] when the body grows past the cap. Other client
    /// failures become [`EgressError::Timeout`], [`EgressError::Connect`], or
    /// [`EgressError::Request`]. Their text is not the client error's `Display`,
    /// which contains the URL.
    pub async fn post(&self, req: EgressRequest<'_>) -> Result<EgressResponse, EgressError> {
        let host = self.guard(req.url)?;
        let response = self
            .builder(req.url, req.headers, req.body, req.timeout)
            .send()
            .await
            .map_err(|err| classify(&err, &host))?;

        let status = response.status();
        let mut response = response;
        let mut collected = Vec::new();
        loop {
            let Some(chunk) = response
                .chunk()
                .await
                .map_err(|err| classify(&err, &host))?
            else {
                break;
            };
            let next = collected.len().saturating_add(chunk.len());
            if next > req.max_response_bytes {
                return Err(EgressError::TooLarge {
                    limit: req.max_response_bytes,
                });
            }
            collected.extend_from_slice(&chunk);
        }
        Ok(EgressResponse {
            status,
            body: Bytes::from(collected),
        })
    }

    /// POSTs `req` and returns once the response head arrives. The body is read with
    /// [`EgressStream::next_chunk`].
    ///
    /// # Errors
    ///
    /// The same refusals as [`Self::post`]. [`EgressError::Timeout`] when the head takes
    /// longer than `idle_timeout` or `timeout`.
    pub async fn post_stream(
        &self,
        req: EgressStreamRequest<'_>,
    ) -> Result<EgressStream, EgressError> {
        let host = self.guard(req.url)?;
        let send = self
            .builder(req.url, req.headers, req.body, req.timeout)
            .send();
        match tokio::time::timeout(req.idle_timeout, send).await {
            Err(_) => Err(EgressError::Timeout),
            Ok(Err(err)) => Err(classify(&err, &host)),
            Ok(Ok(response)) => Ok(EgressStream {
                status: response.status(),
                response,
                host,
                idle_timeout: req.idle_timeout,
                max_response_bytes: req.max_response_bytes,
                read: 0,
            }),
        }
    }

    /// Restore mode, the scheme, and literal addresses (D166). Returns the host label.
    fn guard(&self, url: &Url) -> Result<String, EgressError> {
        if self.policy.restore_mode {
            return Err(EgressError::RestoreMode);
        }
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(EgressError::Scheme);
        }
        let host = host_label(url);
        if !self.policy.allows_url(url) {
            return Err(EgressError::Denied { host });
        }
        Ok(host)
    }

    fn builder(
        &self,
        url: &Url,
        headers: Vec<(HeaderName, HeaderValue)>,
        body: Vec<u8>,
        timeout: Duration,
    ) -> reqwest::RequestBuilder {
        let mut builder = self.http.post(url.clone()).timeout(timeout).body(body);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        builder
    }
}

impl EgressStream {
    /// HTTP status of the response.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The next body chunk, or `None` at the end of the body.
    ///
    /// # Errors
    ///
    /// [`EgressError::Timeout`] when no chunk arrives within the idle timeout or the
    /// whole-request timeout ends, [`EgressError::TooLarge`] when the body passes the cap,
    /// and the other client failures as in [`EgressClient::post`].
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, EgressError> {
        match tokio::time::timeout(self.idle_timeout, self.response.chunk()).await {
            Err(_) => Err(EgressError::Timeout),
            Ok(Err(err)) => Err(classify(&err, &self.host)),
            Ok(Ok(None)) => Ok(None),
            Ok(Ok(Some(chunk))) => {
                self.read = self.read.saturating_add(chunk.len());
                if self.read > self.max_response_bytes {
                    return Err(EgressError::TooLarge {
                        limit: self.max_response_bytes,
                    });
                }
                Ok(Some(chunk))
            }
        }
    }

    /// Reads the rest of the body. At most `max_bytes` (and the stream's own cap).
    ///
    /// # Errors
    ///
    /// [`EgressError::TooLarge`] with `limit: max_bytes` when the body passes `max_bytes`,
    /// and the errors of [`Self::next_chunk`].
    pub async fn read_to_end(mut self, max_bytes: usize) -> Result<Bytes, EgressError> {
        let mut collected = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            if collected.len().saturating_add(chunk.len()) > max_bytes {
                return Err(EgressError::TooLarge { limit: max_bytes });
            }
            collected.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(collected))
    }
}

fn user_agent() -> String {
    format!(
        "{}/{}",
        cetra_core::names::PRODUCT,
        env!("CARGO_PKG_VERSION")
    )
}

fn host_label(url: &Url) -> String {
    url.host_str().unwrap_or("").to_owned()
}

/// A literal address in the URL host. Bracketed IPv6 is unwrapped. A name is `None`.
fn literal_ip(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.parse().ok()
}

fn classify(err: &reqwest::Error, host: &str) -> EgressError {
    if caused_by_denied(err) {
        return EgressError::Denied {
            host: host.to_owned(),
        };
    }
    if err.is_timeout() {
        return EgressError::Timeout;
    }
    if err.is_connect() {
        return EgressError::Connect;
    }
    EgressError::Request
}

fn caused_by_denied(err: &reqwest::Error) -> bool {
    let mut current = std::error::Error::source(err);
    while let Some(cause) = current {
        if cause.is::<DeniedAddress>() {
            return true;
        }
        current = cause.source();
    }
    false
}

/// Returned from the resolver when every address was dropped.
#[derive(Debug)]
struct DeniedAddress;

impl fmt::Display for DeniedAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("address denied")
    }
}

impl std::error::Error for DeniedAddress {}

struct GuardedResolver {
    allow_private_network: bool,
    restore_mode: bool,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private_network = self.allow_private_network;
        let restore_mode = self.restore_mode;
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Restore mode resolves no name: the same refusal as a fully denied lookup.
            if restore_mode {
                return Err(Box::new(DeniedAddress) as Box<dyn std::error::Error + Send + Sync>);
            }
            let looked_up = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|_| Box::new(LookupFailed) as Box<dyn std::error::Error + Send + Sync>)?;
            let kept: Vec<SocketAddr> = looked_up
                .filter(|addr| allow_private_network || !is_denied(addr.ip()))
                .collect();
            if kept.is_empty() {
                return Err(Box::new(DeniedAddress) as Box<dyn std::error::Error + Send + Sync>);
            }
            let addrs: Addrs = Box::new(kept.into_iter());
            Ok(addrs)
        })
    }
}

#[derive(Debug)]
struct LookupFailed;

impl fmt::Display for LookupFailed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("lookup failed")
    }
}

impl std::error::Error for LookupFailed {}

/// Whether `ip` is in a D166 range.
///
/// Documentation addresses are the IANA ranges: `192.0.2.0/24`, `198.51.100.0/24`,
/// `203.0.113.0/24`, and `2001:db8::/32`. IPv4-mapped (`::ffff:0:0/96`) and NAT64
/// (`64:ff9b::/96`) addresses are judged by the embedded IPv4 address.
#[must_use]
pub(crate) fn is_denied(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => denied_v4(ip),
        IpAddr::V6(ip) => denied_v6(ip),
    }
}

fn denied_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
}

fn denied_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return denied_v4(v4);
    }
    if let Some(v4) = nat64_v4(ip) {
        return denied_v4(v4);
    }
    let segments = ip.segments();
    let [first, second, third, ..] = segments;
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || first & 0xffc0 == 0xfe80
        || first & 0xfe00 == 0xfc00
        || (first == 0x2001 && second == 0x0db8)
        || first & 0xffc0 == 0xfec0
        || first == 0x2002
        || (first == 0x0064 && second == 0xff9b && third == 1)
        || segments[..6].iter().all(|&segment| segment == 0)
}

/// The IPv4 address embedded in `64:ff9b::/96`, the well-known NAT64 prefix.
fn nat64_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let octets = ip.octets();
    if octets[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0] {
        Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::Router;
    use axum::extract::State;
    use axum::http::{StatusCode, Uri};
    use axum::response::{IntoResponse, Response};
    use reqwest::Url;
    use reqwest::header::{HeaderName, HeaderValue};

    use super::{
        EgressClient, EgressPolicy, EgressRequest, EgressStream, EgressStreamRequest,
        identity_http_client, is_denied,
    };
    use crate::error::{EgressError, assert_no_payload};
    use crate::stream_server::{Step, StreamReply, StreamServer};

    const PLANTED: &str = "SECRET-PAYLOAD";

    fn addr(text: &str) -> IpAddr {
        text.parse().expect(text)
    }

    #[test]
    fn denies_each_range_at_its_ends() {
        let denied = [
            "127.0.0.0",
            "127.255.255.255",
            "10.0.0.0",
            "10.255.255.255",
            "172.16.0.0",
            "172.31.255.255",
            "192.168.0.0",
            "192.168.255.255",
            "100.64.0.0",
            "100.127.255.255",
            "169.254.0.0",
            "169.254.255.255",
            "0.0.0.0",
            "224.0.0.0",
            "239.255.255.255",
            "255.255.255.255",
            "192.0.0.0",
            "192.0.0.255",
            "198.18.0.0",
            "198.19.255.255",
            "192.0.2.0",
            "192.0.2.255",
            "198.51.100.0",
            "198.51.100.255",
            "203.0.113.0",
            "203.0.113.255",
            "::1",
            "::",
            "fe80::",
            "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fc00::",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "ff00::",
            "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "2001:db8::",
            "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff",
            "::ffff:127.0.0.1",
            "::ffff:10.255.255.255",
            "::ffff:0.0.0.0",
            "::ffff:192.0.2.1",
            "::ffff:169.254.169.254",
            "0.0.0.1",
            "0.255.255.255",
            "240.0.0.0",
            "255.255.255.254",
            "::1.2.3.4",
            "::8.8.8.8",
            "64:ff9b:1::7f00:1",
            "64:ff9b:1:ffff:ffff:ffff:ffff:ffff",
            "2002::",
            "2002:808:808::1",
            "fec0::",
            "feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:0",
            "64:ff9b::ffff:ffff",
            "64:ff9b::c000:201",
        ];
        let allowed = [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "9.255.255.255",
            "11.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "1.0.0.0",
            "223.255.255.255",
            "192.0.1.0",
            "198.17.255.255",
            "198.20.0.0",
            "192.0.3.0",
            "198.51.101.0",
            "203.0.114.0",
            "2001:4860:4860::8888",
            "fe7f:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fbff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
            "fe00::",
            "2001:db7:ffff:ffff:ffff:ffff:ffff:ffff",
            "2001:db9::",
            "::ffff:1.1.1.1",
            "64:ff9b::101:101",
            "64:ff9b:2::1",
            "2003::",
            "fe7f::1",
            "::1:0:0:1",
        ];
        for text in denied {
            assert!(is_denied(addr(text)), "{text} is allowed");
        }
        for text in allowed {
            assert!(!is_denied(addr(text)), "{text} is denied");
        }
    }

    #[test]
    fn allows_url_checks_scheme_and_literal_addresses() {
        let strict = EgressPolicy {
            allow_private_network: false,
            restore_mode: false,
        };
        let open = EgressPolicy {
            allow_private_network: true,
            restore_mode: false,
        };
        let url = |text: &str| Url::parse(text).unwrap();

        assert!(strict.allows_url(&url("https://idp.example/.well-known")));
        assert!(strict.allows_url(&url("http://idp.example/")));
        assert!(!strict.allows_url(&url("http://10.0.0.1/")));
        assert!(open.allows_url(&url("http://10.0.0.1/")));
        assert!(!strict.allows_url(&url("https://[::1]/")));
        assert!(!strict.allows_url(&url("http://169.254.169.254/latest")));
        assert!(!strict.allows_url(&url("ftp://x")));
        assert!(!open.allows_url(&url("ftp://x")));
    }

    #[test]
    fn request_debug_hides_payload() {
        let url = Url::parse(&format!("https://h.example/p?token={PLANTED}")).expect("url");
        let headers = vec![(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&format!("Bearer {PLANTED}")).expect("header"),
        )];
        let plain = EgressRequest {
            url: &url,
            headers: headers.clone(),
            body: PLANTED.as_bytes().to_vec(),
            timeout: Duration::from_secs(2),
            max_response_bytes: 64,
        };
        let stream = EgressStreamRequest {
            url: &url,
            headers,
            body: PLANTED.as_bytes().to_vec(),
            timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(1),
            max_response_bytes: 64,
        };
        for debug in [format!("{plain:?}"), format!("{stream:?}")] {
            assert!(debug.contains("h.example"), "{debug}");
            assert!(debug.contains("authorization"), "{debug}");
            assert_no_payload(&debug, PLANTED);
        }
    }

    #[tokio::test]
    async fn identity_client_guards_host_names_and_follows_no_redirects() {
        let mock = Mock::start().await;
        let name = mock.url(true);

        let strict = identity_http_client(EgressPolicy {
            allow_private_network: false,
            restore_mode: false,
        })
        .expect("client");
        strict
            .get(name.clone())
            .send()
            .await
            .expect_err("localhost is refused by the resolver");
        assert_eq!(mock.hits(), 0);

        let open = identity_http_client(EgressPolicy {
            allow_private_network: true,
            restore_mode: false,
        })
        .expect("client");
        let response = open.get(mock.path("/redirect")).send().await.expect("send");
        assert_eq!(response.status(), StatusCode::FOUND);
        let response = open.get(name).send().await.expect("localhost allowed");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(mock.hits(), 2);
    }

    #[tokio::test]
    async fn blocks_loopback_unless_the_policy_allows_it() {
        let mock = Mock::start().await;
        let blocked = client(false);
        let literal = mock.url(false);
        let err = post(&blocked, &literal, Duration::from_secs(2), 64)
            .await
            .expect_err("literal loopback");
        assert_eq!(err, EgressError::Denied { host: mock.ip() });
        assert_no_secret(&err);

        let name = mock.url(true);
        let err = post(&blocked, &name, Duration::from_secs(2), 64)
            .await
            .expect_err("localhost");
        assert_eq!(
            err,
            EgressError::Denied {
                host: "localhost".to_owned(),
            }
        );
        assert_no_secret(&err);
        assert_eq!(mock.hits(), 0);

        let allowed = client(true);
        let response = post(&allowed, &literal, Duration::from_secs(2), 64)
            .await
            .expect("allowed literal");
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.body.as_ref(), b"ok");
        let response = post(&allowed, &name, Duration::from_secs(2), 64)
            .await
            .expect("allowed localhost");
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(mock.hits(), 2);
    }

    #[tokio::test]
    async fn returns_a_redirect_and_caps_the_body() {
        let mock = Mock::start().await;
        let client = client(true);
        let redirect = mock.path("/redirect");
        let response = post(&client, &redirect, Duration::from_secs(2), 64)
            .await
            .expect("redirect");
        assert_eq!(response.status, StatusCode::FOUND);
        assert_eq!(mock.hits(), 1);

        let big = mock.path("/big");
        let err = post(&client, &big, Duration::from_secs(2), 4)
            .await
            .expect_err("body cap");
        assert_eq!(err, EgressError::TooLarge { limit: 4 });
        assert_no_secret(&err);

        let exact = post(&client, &mock.path("/"), Duration::from_secs(2), 2)
            .await
            .expect("exact cap");
        assert_eq!(exact.body.as_ref(), b"ok");
    }

    #[tokio::test]
    async fn times_out_and_refuses_other_schemes() {
        let mock = Mock::start().await;
        let client = client(true);
        let err = post(&client, &mock.path("/slow"), Duration::from_millis(200), 64)
            .await
            .expect_err("timeout");
        assert_eq!(err, EgressError::Timeout);
        assert_no_secret(&err);

        let ftp = Url::parse(&format!("ftp://example.com/hook?token={PLANTED}")).unwrap();
        let err = post(&client, &ftp, Duration::from_secs(2), 64)
            .await
            .expect_err("scheme");
        assert_eq!(err, EgressError::Scheme);
        assert_no_secret(&err);
        assert!(!err.to_string().contains("example.com"));
    }

    fn client(allow_private_network: bool) -> EgressClient {
        EgressClient::new(EgressPolicy {
            allow_private_network,
            restore_mode: false,
        })
        .expect("client")
    }

    #[tokio::test]
    async fn restore_mode_refuses_before_sending() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let client = EgressClient::new(EgressPolicy {
            allow_private_network: true,
            restore_mode: true,
        })
        .expect("client");
        let url = Url::parse(&format!("http://127.0.0.1:{port}/hook")).unwrap();
        let err = post(&client, &url, Duration::from_secs(2), 64)
            .await
            .expect_err("restore mode");
        assert_eq!(err, EgressError::RestoreMode);
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "a connection was accepted");
    }

    #[test]
    fn restore_mode_denies_every_url() {
        let policy = EgressPolicy {
            allow_private_network: true,
            restore_mode: true,
        };
        let url = |text: &str| Url::parse(text).unwrap();
        assert!(!policy.allows_url(&url("https://example.com/")));
        assert!(!policy.allows_url(&url("http://127.0.0.1/")));
    }

    #[tokio::test]
    async fn identity_client_in_restore_mode_cannot_resolve() {
        let mock = Mock::start().await;
        let client = identity_http_client(EgressPolicy {
            allow_private_network: true,
            restore_mode: true,
        })
        .expect("client");
        client
            .get(mock.url(true))
            .send()
            .await
            .expect_err("restore mode resolves no name");
        assert_eq!(mock.hits(), 0);
    }

    #[tokio::test]
    async fn post_stream_yields_chunks_in_order() {
        let server = StreamServer::start().await;
        server.enqueue(StreamReply {
            status: 200,
            head_delay: Duration::ZERO,
            steps: vec![
                Step::Send(b"a".to_vec()),
                Step::Sleep(Duration::from_millis(50)),
                Step::Send(b"b".to_vec()),
            ],
        });
        let client = client(true);
        let mut stream = open(
            &client,
            &server.url("/stream"),
            Duration::from_secs(5),
            Duration::from_secs(2),
            64,
        )
        .await
        .expect("stream");
        assert_eq!(stream.status(), StatusCode::OK);
        assert_eq!(read_all(&mut stream).await.expect("body"), b"ab");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/stream");
    }

    #[tokio::test]
    async fn post_stream_fails_after_an_idle_gap() {
        let server = StreamServer::start().await;
        server.enqueue(StreamReply {
            status: 200,
            head_delay: Duration::ZERO,
            steps: vec![
                Step::Send(b"a".to_vec()),
                Step::Sleep(Duration::from_millis(600)),
                Step::Send(b"b".to_vec()),
            ],
        });
        let client = client(true);
        let mut stream = open(
            &client,
            &server.url("/stream"),
            Duration::from_secs(5),
            Duration::from_millis(150),
            64,
        )
        .await
        .expect("stream");
        assert_eq!(
            stream.next_chunk().await.expect("first chunk").as_deref(),
            Some(&b"a"[..])
        );
        let err = stream.next_chunk().await.expect_err("idle gap");
        assert_eq!(err, EgressError::Timeout);
    }

    #[tokio::test]
    async fn post_stream_idle_timeout_covers_the_head() {
        let server = StreamServer::start().await;
        server.enqueue(StreamReply {
            status: 200,
            head_delay: Duration::from_millis(600),
            steps: Vec::new(),
        });
        let client = client(true);
        let err = open(
            &client,
            &server.url("/stream"),
            Duration::from_secs(5),
            Duration::from_millis(150),
            64,
        )
        .await
        .expect_err("head timeout");
        assert_eq!(err, EgressError::Timeout);
    }

    #[tokio::test]
    async fn post_stream_whole_timeout_ends_a_slow_drip() {
        let server = StreamServer::start().await;
        let mut steps = Vec::new();
        for _ in 0..20 {
            steps.push(Step::Send(b"x".to_vec()));
            steps.push(Step::Sleep(Duration::from_millis(50)));
        }
        server.enqueue(StreamReply {
            status: 200,
            head_delay: Duration::ZERO,
            steps,
        });
        let client = client(true);
        let mut stream = open(
            &client,
            &server.url("/stream"),
            Duration::from_millis(300),
            Duration::from_secs(1),
            64,
        )
        .await
        .expect("stream");
        let result = loop {
            match stream.next_chunk().await {
                Ok(Some(_)) => {}
                other => break other,
            }
        };
        assert_eq!(result, Err(EgressError::Timeout));
    }

    #[tokio::test]
    async fn post_stream_caps_the_body() {
        let server = StreamServer::start().await;
        let body = b"0123456789".to_vec();
        server.enqueue(StreamReply {
            status: 200,
            head_delay: Duration::ZERO,
            steps: vec![Step::Send(body.clone())],
        });
        let client = client(true);
        let mut capped = open(
            &client,
            &server.url("/stream"),
            Duration::from_secs(5),
            Duration::from_secs(2),
            4,
        )
        .await
        .expect("stream");
        let result = loop {
            match capped.next_chunk().await {
                Ok(Some(_)) => {}
                other => break other,
            }
        };
        assert_eq!(result, Err(EgressError::TooLarge { limit: 4 }));

        server.enqueue(StreamReply {
            status: 200,
            head_delay: Duration::ZERO,
            steps: vec![Step::Send(body.clone())],
        });
        let mut whole = open(
            &client,
            &server.url("/stream"),
            Duration::from_secs(5),
            Duration::from_secs(2),
            10,
        )
        .await
        .expect("stream");
        assert_eq!(read_all(&mut whole).await.expect("body"), body);
    }

    #[tokio::test]
    async fn read_to_end_reads_an_error_body_and_caps_it() {
        let server = StreamServer::start().await;
        server.enqueue(StreamReply {
            status: 500,
            head_delay: Duration::ZERO,
            steps: vec![Step::Send(b"oops".to_vec())],
        });
        let client = client(true);
        let stream = open(
            &client,
            &server.url("/stream"),
            Duration::from_secs(5),
            Duration::from_secs(2),
            64,
        )
        .await
        .expect("stream");
        assert_eq!(stream.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = stream.read_to_end(64).await.expect("error body");
        assert_eq!(body.as_ref(), b"oops");

        server.enqueue(StreamReply {
            status: 500,
            head_delay: Duration::ZERO,
            steps: vec![Step::Send(b"oops".to_vec())],
        });
        let stream = open(
            &client,
            &server.url("/stream"),
            Duration::from_secs(5),
            Duration::from_secs(2),
            64,
        )
        .await
        .expect("stream");
        let err = stream.read_to_end(2).await.expect_err("capped");
        assert_eq!(err, EgressError::TooLarge { limit: 2 });
    }

    #[tokio::test]
    async fn post_stream_keeps_the_guard() {
        let server = StreamServer::start().await;
        let mut literal = server.url("/stream");
        literal.set_query(Some(&format!("token={PLANTED}")));
        let mut name = literal.clone();
        name.set_host(Some("localhost")).expect("host name");
        let ftp = Url::parse(&format!("ftp://127.0.0.1/stream?token={PLANTED}")).expect("url");

        let strict = client(false);
        for url in [&literal, &name] {
            let err = open(
                &strict,
                url,
                Duration::from_secs(2),
                Duration::from_secs(2),
                64,
            )
            .await
            .expect_err("denied");
            assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
            assert_no_secret(&err);
        }

        let restore = EgressClient::new(EgressPolicy {
            allow_private_network: true,
            restore_mode: true,
        })
        .expect("client");
        let err = open(
            &restore,
            &literal,
            Duration::from_secs(2),
            Duration::from_secs(2),
            64,
        )
        .await
        .expect_err("restore mode");
        assert_eq!(err, EgressError::RestoreMode);
        assert_no_secret(&err);

        let err = open(
            &client(true),
            &ftp,
            Duration::from_secs(2),
            Duration::from_secs(2),
            64,
        )
        .await
        .expect_err("scheme");
        assert_eq!(err, EgressError::Scheme);
        assert_no_secret(&err);

        assert!(server.requests().is_empty(), "a request reached the server");
    }

    async fn open(
        client: &EgressClient,
        url: &Url,
        timeout: Duration,
        idle_timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<EgressStream, EgressError> {
        client
            .post_stream(EgressStreamRequest {
                url,
                headers: Vec::new(),
                body: Vec::new(),
                timeout,
                idle_timeout,
                max_response_bytes,
            })
            .await
    }

    async fn read_all(stream: &mut EgressStream) -> Result<Vec<u8>, EgressError> {
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next_chunk().await? {
            collected.extend_from_slice(&chunk);
        }
        Ok(collected)
    }

    async fn post(
        client: &EgressClient,
        url: &Url,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<super::EgressResponse, EgressError> {
        client
            .post(EgressRequest {
                url,
                headers: Vec::new(),
                body: Vec::new(),
                timeout,
                max_response_bytes,
            })
            .await
    }

    fn assert_no_secret(err: &EgressError) {
        let message = err.to_string();
        assert_no_payload(&message, PLANTED);
        assert_no_payload(&message, "/hook");
        assert_no_payload(&message, "token");
    }

    struct Mock {
        port: u16,
        hits: Arc<AtomicUsize>,
    }

    impl Mock {
        async fn start() -> Self {
            let hits = Arc::new(AtomicUsize::new(0));
            let app = Router::new().fallback(hit).with_state(Arc::clone(&hits));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self { port, hits }
        }

        fn hits(&self) -> usize {
            self.hits.load(Ordering::Relaxed)
        }

        fn ip(&self) -> String {
            "127.0.0.1".to_owned()
        }

        fn url(&self, localhost: bool) -> Url {
            let host = if localhost { "localhost" } else { "127.0.0.1" };
            Url::parse(&format!("http://{host}:{}/hook?token={PLANTED}", self.port)).unwrap()
        }

        fn path(&self, path: &str) -> Url {
            Url::parse(&format!(
                "http://127.0.0.1:{}{path}?token={PLANTED}",
                self.port
            ))
            .unwrap()
        }
    }

    async fn hit(State(hits): State<Arc<AtomicUsize>>, uri: Uri) -> Response {
        hits.fetch_add(1, Ordering::Relaxed);
        match uri.path() {
            "/slow" => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                StatusCode::OK.into_response()
            }
            "/big" => (StatusCode::OK, PLANTED).into_response(),
            "/redirect" => {
                let mut response = StatusCode::FOUND.into_response();
                response.headers_mut().insert(
                    axum::http::header::LOCATION,
                    HeaderValue::from_static("/landed"),
                );
                response
            }
            _ => (StatusCode::OK, "ok").into_response(),
        }
    }
}
