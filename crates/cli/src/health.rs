//! `cetra health`: the container healthcheck (D-N-4). The distroless image has no curl, so
//! Docker's `HEALTHCHECK` and compose's `healthcheck` run this command.

use std::time::Duration;

use anyhow::Context;

/// `http_addr`'s default port, on loopback.
pub(crate) const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// The whole exchange must finish in this time. Docker's healthcheck timeout is 10 s.
const TIMEOUT: Duration = Duration::from_secs(5);

/// `GET <url>/readyz`. `Ok` on `200`; an error naming the URL and the status otherwise.
pub(crate) async fn check(url: &str) -> anyhow::Result<()> {
    let http = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("cetra-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building the http client")?;
    let target = format!("{}/readyz", url.trim_end_matches('/'));
    let response = http
        .get(&target)
        .send()
        .await
        .with_context(|| format!("GET {target}"))?;
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        anyhow::bail!("GET {target}: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{Canned, TestServer};

    #[tokio::test]
    async fn a_ready_server_passes() {
        let server = TestServer::start().await;
        check(&server.url).await.unwrap();
        server.stop().await;
    }

    #[tokio::test]
    async fn asks_readyz_once_and_accepts_200() {
        let canned = Canned::json(vec![(200, r#"{"status":"ready"}"#.to_owned())]).await;
        check(&format!("{}/", canned.url)).await.unwrap();
        let recorded = canned.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].target, "/readyz");
    }

    #[tokio::test]
    async fn a_not_ready_server_fails_with_the_status() {
        let canned = Canned::json(vec![(503, r#"{"status":503}"#.to_owned())]).await;
        let error = check(&canned.url).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("/readyz"), "{text}");
        assert!(text.contains("503"), "{text}");
    }

    #[tokio::test]
    async fn an_unreachable_server_fails() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let error = check(&url).await.unwrap_err();
        assert!(format!("{error:#}").contains("/readyz"), "{error:#}");
    }
}
