# N1 — `cetra health`

Read [README.md](README.md) first.

**Goal:** `cetra health [--url <url>]` sends `GET <url>/readyz` and exits 0 on a `200` answer.
Otherwise, or if no answer comes within 5 s, it prints the reason on stderr and exits 1. The image's
`HEALTHCHECK` (N4) and the compose healthcheck (N5) call it: the distroless image has no curl
(D-N-4). The default URL is `http://127.0.0.1:8080`, which is `http_addr`'s default port on
loopback.

**Edit only:**
- `crates/cli/src/health.rs` (new);
- `crates/cli/src/main.rs`.

Spec 09 § CLI already lists the command (the plan commit added it).

**Commit:** `Add cetra health`

**Tools:** the usual `just check` toolchain.

## Read first

- `crates/cli/src/main.rs`: the `Command` enum, `run_cli`, and the `mod` list at the top.
- `crates/cli/src/client.rs` lines 140–160: how the CLI builds its `reqwest::Client`.
- `crates/cli/src/test_server.rs`: `TestServer::start`, `TestServer::stop`, `Canned::json` and
  `Canned::recorded`.
- `crates/server/src/http/health.rs`: `/readyz` answers `200 {"status":"ready"}`, or a problem
  with status `503`.

## `crates/cli/src/health.rs` (new)

```rust
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
```

## `crates/cli/src/main.rs`

1. In the module list at the top, add `mod health;` after `mod credentials;`. The list is
   alphabetical: `client`, `credentials`, `health`, `login`, …
2. In `enum Command`, add this variant after `Version`:

   ```rust
       /// Exit 0 if the server answers `GET <url>/readyz` with 200 (the container healthcheck).
       Health {
           /// Server URL.
           #[arg(long, default_value = health::DEFAULT_URL)]
           url: String,
       },
   ```

3. In `run_cli`'s `match cli.command`, add this arm after `Command::Version => print_version()?,`:

   ```rust
           Command::Health { url } => health::check(&url).await?,
   ```

4. In `mod tests` of `main.rs`, add after `rotate_encryption_key_parses`:

   ```rust
       #[test]
       fn health_parses_with_and_without_url() {
           let url_of = |argv: &[&str]| match Cli::try_parse_from(argv).unwrap().command {
               Command::Health { url } => url,
               other => panic!("unexpected command {other:?}"),
           };
           assert_eq!(url_of(&["cetra", "health"]), "http://127.0.0.1:8080");
           assert_eq!(
               url_of(&["cetra", "health", "--url", "http://10.0.0.1:9000"]),
               "http://10.0.0.1:9000"
           );
       }
   ```

`main` already turns an `Err` from `run_cli` into exit status 1 and prints the error chain on
stderr. Do not add a separate exit path.

## Verify

```bash
just check
cargo run -q -p cetra-cli -- health; echo "exit=$?"
```

The second command, with no server running, prints `GET http://127.0.0.1:8080/readyz: …` and
`exit=1`. With `just run serve` running in another terminal, it prints nothing and `exit=0`
(once `/readyz` is ready).

## Done when

- [ ] `crates/cli/src/health.rs` exists with the 4 tests, and `main.rs` has the variant, the arm
      and `health_parses_with_and_without_url`.
- [ ] `just check` passes. One commit: `Add cetra health`.
