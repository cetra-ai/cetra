//! `cetra login` and `cetra logout` (spec 08 § Device authorization, D-AO-4). Login asks the
//! server for a device code, prints the URL and the code, and polls `POST /api/auth/device/token`
//! until the user approves or denies, or the code expires. The token is checked with
//! `GET /api/auth/me` and stored in the credentials file. It is never logged.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::credentials::{SavedToken, chosen_server, load, save, server_url};

/// How long past the code's `expires_in` the poll loop waits before it gives up.
const DEADLINE_SLACK: Duration = Duration::from_secs(5);
/// What `slow_down` adds to the poll interval (RFC 8628 § 3.5).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// The longest code lifetime the CLI accepts from the server, in seconds.
const MAX_EXPIRES_IN_S: u64 = 900;
/// The shortest poll interval the CLI uses, whatever the server says.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const EXPIRED: &str = "the sign-in code expired; run `cetra login` again";
const BAD_URL: &str = "the server sent an invalid sign-in URL";
const BAD_CODE: &str = "the server sent an invalid sign-in code";

/// `cetra login`. `output` receives the sign-in URL, then the code, one line each.
///
/// The token expires after 90 days. When it does, run `cetra login` again.
///
/// # Errors
///
/// Returns an error when the server URL is refused by `credentials::server_url`, the server
/// refuses to issue a code, the user denies the code or it expires, the server's token is not
/// a personal access token or is rejected, or the credentials file cannot be written. The
/// message does not include the token, and a refused URL is never printed.
pub(crate) async fn login(
    raw: &str,
    no_browser: bool,
    credentials: &Path,
    mut output: impl Write + Send,
) -> anyhow::Result<()> {
    let server = server_url(raw)?;
    let host = local_host().as_deref().and_then(sanitise_host);
    let client = http_client()?;
    let code = request_code(&client, &server, host.as_deref()).await?;
    // The server's answer is checked before anything is printed or opened.
    let sign_in_url = parse_sign_in_url(&code.verification_uri_complete)?;
    if !is_user_code(&code.user_code) {
        anyhow::bail!(BAD_CODE);
    }
    writeln!(output, "{}", sign_in_url.as_str()).context("printing the sign-in url")?;
    writeln!(output, "Code: {}", code.user_code).context("printing the sign-in code")?;
    output.flush().context("printing the sign-in url")?;
    drop(output);
    if !no_browser {
        open_browser(sign_in_url.as_str());
    }
    let token = poll_for_token(&client, &server, &code).await?;
    if !is_personal_token(&token) {
        anyhow::bail!("the server returned an invalid token");
    }
    verify(&server, &token).await?;
    let mut file = load(credentials)?;
    file.servers.insert(server, SavedToken { token });
    save(credentials, &file)?;
    Ok(())
}

/// `cetra logout`. Removes the saved server even when the revoke call fails.
///
/// # Errors
///
/// Returns an error when no server can be chosen or the credentials file cannot
/// be updated. A failed revoke is a warning on stderr and is not an error.
pub(crate) async fn logout(raw: Option<&str>, credentials: &Path) -> anyhow::Result<()> {
    let mut file = load(credentials)?;
    let server = chosen_server(raw, &file)?;
    let Some(saved) = file.servers.remove(&server) else {
        anyhow::bail!("no saved token for that server");
    };
    if let Err(message) = revoke(&server, &saved.token).await {
        warn(&message);
    }
    if file.servers.is_empty() {
        if credentials.exists() {
            fs::remove_file(credentials).context("removing credentials")?;
        }
        return Ok(());
    }
    save(credentials, &file)
}

/// Asks the server for a device code for a `login`. `host` is the sanitised machine name.
async fn request_code(
    client: &reqwest::Client,
    server: &str,
    host: Option<&str>,
) -> anyhow::Result<CodeAnswer> {
    let response = client
        .post(format!("{server}/api/auth/device/code"))
        .json(&CodeRequest {
            purpose: "login",
            host,
        })
        .send()
        .await
        .context("requesting a sign-in code")?;
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        anyhow::bail!(
            "the server refused a sign-in code (HTTP {})",
            status.as_u16()
        );
    }
    response
        .json::<CodeAnswer>()
        .await
        .context("reading the sign-in code")
}

/// Polls the token endpoint until the grant arrives, the user denies, or the code expires.
/// A network error on one poll is not fatal: the poll is retried until the deadline. The
/// server's interval is at least `MIN_INTERVAL` and its lifetime at most `MAX_EXPIRES_IN_S`.
async fn poll_for_token(
    client: &reqwest::Client,
    server: &str,
    code: &CodeAnswer,
) -> anyhow::Result<String> {
    let lifetime = Duration::from_secs(code.expires_in.min(MAX_EXPIRES_IN_S));
    let deadline = tokio::time::Instant::now() + lifetime.saturating_add(DEADLINE_SLACK);
    let mut interval = Duration::from_secs(code.interval).max(MIN_INTERVAL);
    let mut warned = false;
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(EXPIRED);
        }
        let response = match client
            .post(format!("{server}/api/auth/device/token"))
            .json(&TokenRequest {
                device_code: &code.device_code,
            })
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                if !warned {
                    warn("cannot reach the server; still waiting");
                    warned = true;
                }
                continue;
            }
        };
        let status = response.status().as_u16();
        if status == 200 {
            let grant = response
                .json::<Grant>()
                .await
                .context("reading the sign-in grant")?;
            return Ok(grant.token);
        }
        let body = response.bytes().await.unwrap_or_default();
        match step(status, &body) {
            Step::Wait => {}
            Step::SlowDown => interval = next_interval(interval),
            Step::Denied => anyhow::bail!("sign-in denied in the browser"),
            Step::Expired => anyhow::bail!(EXPIRED),
            Step::Failed(failed) => anyhow::bail!("sign-in failed (HTTP {failed})"),
        }
    }
}

/// What one poll answer means for the loop. A 400 with an RFC 8628 code continues it, and so
/// does a 429 or a 502, 503 or 504 from a proxy: a blip does not end a live code.
fn step(status: u16, body: &[u8]) -> Step {
    if matches!(status, 429 | 502 | 503 | 504) {
        return Step::Wait;
    }
    if status != 400 {
        return Step::Failed(status);
    }
    let code = serde_json::from_slice::<ProblemCodeOnly>(body)
        .ok()
        .and_then(|problem| problem.code);
    match code.as_deref() {
        Some("authorization_pending") => Step::Wait,
        Some("slow_down") => Step::SlowDown,
        Some("access_denied") => Step::Denied,
        Some("expired_token") => Step::Expired,
        _ => Step::Failed(status),
    }
}

/// The sign-in URL the server sent, parsed. Only `http` and `https` are accepted; the returned
/// URL is re-serialised, so it is the one that gets printed and opened.
fn parse_sign_in_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).map_err(|_| anyhow::anyhow!(BAD_URL))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        _ => anyhow::bail!(BAD_URL),
    }
}

/// `XXXX-XXXX` in ASCII capitals, the shape the server issues (spec 08 § Device authorization).
fn is_user_code(code: &str) -> bool {
    let bytes = code.as_bytes();
    bytes.len() == 9
        && bytes[4] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || byte.is_ascii_uppercase())
}

fn next_interval(current: Duration) -> Duration {
    current + SLOW_DOWN_STEP
}

/// Keeps only `[A-Za-z0-9.-]`, at most 64 characters. `None` when nothing is left.
fn sanitise_host(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
        .take(64)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// The machine's host name as the operating system reports it, not yet sanitised.
fn local_host() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
}

fn is_personal_token(token: &str) -> bool {
    let mut prefix = String::from("pat-");
    prefix.push_str(cetra_core::names::PRODUCT);
    prefix.push('-');
    let Some(tail) = token.strip_prefix(&prefix) else {
        return false;
    };
    tail.len() == 32 && tail.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// The body of `POST /api/auth/device/code`, which carries no cookie and no `Origin`.
#[derive(Serialize)]
struct CodeRequest<'a> {
    purpose: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<&'a str>,
}

/// The answer to `POST /api/auth/device/code`. `Debug` hides the device code.
#[derive(Deserialize)]
struct CodeAnswer {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    /// Seconds.
    expires_in: u64,
    /// Seconds between polls.
    interval: u64,
}

impl fmt::Debug for CodeAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeAnswer")
            .field("device_code", &"<redacted>")
            .field("user_code", &self.user_code)
            .field("verification_uri_complete", &self.verification_uri_complete)
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// The body of `POST /api/auth/device/token`.
#[derive(Serialize)]
struct TokenRequest<'a> {
    device_code: &'a str,
}

/// The grant answer. Only the token is read here. `Debug` hides it.
#[derive(Deserialize)]
struct Grant {
    token: String,
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// The `code` of a problem answer; any other field is ignored.
#[derive(Deserialize)]
struct ProblemCodeOnly {
    code: Option<String>,
}

/// What one poll means for the loop.
#[derive(Debug, PartialEq)]
enum Step {
    Wait,
    SlowDown,
    Denied,
    Expired,
    Failed(u16),
}

async fn verify(server: &str, token: &str) -> anyhow::Result<()> {
    let client = http_client()?;
    let response = client
        .get(format!("{server}/api/auth/me"))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .context("could not reach the server")?;
    if response.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("the server rejected the token")
    }
}

async fn revoke(server: &str, token: &str) -> Result<(), String> {
    let client = http_client().map_err(|_| "could not reach the server".to_owned())?;
    let response = client
        .delete(format!("{server}/api/v1/me/tokens/current"))
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .await
        .map_err(|_| "could not reach the server".to_owned())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("the server answered {}", response.status()))
    }
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building the http client")
}

fn warn(message: &str) {
    let _ = writeln!(io::stderr(), "warning: {message}");
}

fn open_browser(url: &str) {
    let _ = browser_command(url).status();
}

fn browser_command(url: &str) -> Command {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(url);
        command
    }
    #[cfg(target_os = "windows")]
    {
        // No shell: `cmd /c start` would read `&` and `|` in the URL as commands.
        let mut command = Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url]);
        command
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::normalize_server_url;
    use crate::test_server::Canned;

    /// A pending poll answer, as the server sends it.
    const PENDING: &str = r#"{"code":"authorization_pending"}"#;

    fn example_token() -> String {
        format!(
            "pat-{}-EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0",
            cetra_core::names::PRODUCT
        )
    }

    /// A code answer with a zero interval, so that a canned flow polls once a second.
    fn code_answer() -> String {
        code_answer_with("http://127.0.0.1:8080/device?code=BCDF-GHJK", "BCDF-GHJK")
    }

    /// A code answer with the given sign-in URL and user code.
    fn code_answer_with(url: &str, user_code: &str) -> String {
        serde_json::json!({
            "device_code": "d".repeat(43),
            "user_code": user_code,
            "verification_uri_complete": url,
            "expires_in": 600,
            "interval": 0,
        })
        .to_string()
    }

    fn grant(token: &str) -> String {
        format!(r#"{{"server":"http://127.0.0.1:8080","token":"{token}"}}"#)
    }

    struct LineWriter {
        tx: tokio::sync::mpsc::UnboundedSender<String>,
        buf: Vec<u8>,
    }

    impl Write for LineWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(data);
            while let Some(pos) = self.buf.iter().position(|byte| *byte == b'\n') {
                let line = String::from_utf8_lossy(&self.buf[..=pos]).trim().to_owned();
                let _ = self.tx.send(line);
                self.buf.drain(..=pos);
            }
            Ok(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn query_value(url: &str, name: &str) -> Option<String> {
        let query = url.split_once('?')?.1;
        for pair in query.split('&') {
            let (key, value) = pair.split_once('=')?;
            if key == name {
                return Some(value.to_owned());
            }
        }
        None
    }

    #[test]
    fn a_personal_token_has_the_pat_shape() {
        let tail = "EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0";
        let token = format!("pat-{}-{tail}", cetra_core::names::PRODUCT);
        assert!(is_personal_token(&token));
        assert!(!is_personal_token(&format!(
            "sk-{}-{tail}",
            cetra_core::names::PRODUCT
        )));
    }

    #[test]
    fn the_host_is_sanitised_and_truncated() {
        assert_eq!(
            sanitise_host("my host!.local").as_deref(),
            Some("myhost.local")
        );
        assert_eq!(sanitise_host("  !! "), None);
        let long = "a".repeat(100);
        assert_eq!(sanitise_host(&long).map(|host| host.len()), Some(64));
    }

    #[test]
    fn step_maps_each_rfc_8628_code() {
        assert_eq!(step(400, PENDING.as_bytes()), Step::Wait);
        assert_eq!(step(400, br#"{"code":"slow_down"}"#), Step::SlowDown);
        assert_eq!(step(400, br#"{"code":"access_denied"}"#), Step::Denied);
        assert_eq!(step(400, br#"{"code":"expired_token"}"#), Step::Expired);
    }

    #[test]
    fn a_busy_or_unavailable_server_keeps_the_code_alive() {
        assert_eq!(step(429, b""), Step::Wait);
        assert_eq!(step(502, b""), Step::Wait);
        assert_eq!(step(503, b""), Step::Wait);
        assert_eq!(step(504, b""), Step::Wait);
        assert_eq!(step(500, b""), Step::Failed(500));
    }

    #[test]
    fn the_sign_in_url_must_be_http_or_https() {
        assert!(parse_sign_in_url("javascript:alert(1)").is_err());
        assert!(parse_sign_in_url("file:///etc/passwd").is_err());
        assert!(parse_sign_in_url("not a url").is_err());
        let url = parse_sign_in_url("https://cetra.example/device?code=BCDF-GHJK").unwrap();
        assert_eq!(url.as_str(), "https://cetra.example/device?code=BCDF-GHJK");
    }

    #[test]
    fn the_user_code_is_four_capitals_a_dash_and_four_capitals() {
        assert!(is_user_code("BCDF-GHJK"));
        assert!(!is_user_code("bcdf-ghjk"));
        assert!(!is_user_code("BCDF-GHJK\n"));
        assert!(!is_user_code("BCDFGHJK"));
        assert!(!is_user_code("BCDF-GHJ"));
    }

    #[tokio::test]
    async fn a_javascript_sign_in_url_is_refused_before_anything_is_printed() {
        let canned = Canned::json(vec![(
            200,
            code_answer_with("javascript:alert(1)", "BCDF-GHJK"),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("cetra").join("credentials.toml");
        let mut printed = Vec::new();
        let error = login(&canned.url, true, &credentials, &mut printed)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), BAD_URL);
        assert!(printed.is_empty());
        assert!(!credentials.exists());
    }

    #[tokio::test]
    async fn a_sign_in_url_with_a_space_and_an_ampersand_is_printed_without_a_space() {
        let canned = Canned::json(vec![
            (
                200,
                code_answer_with(
                    "http://127.0.0.1:8080/device?code=BCDF-GHJK&note=a b",
                    "BCDF-GHJK",
                ),
            ),
            (400, r#"{"code":"access_denied"}"#.to_owned()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("cetra").join("credentials.toml");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let error = login(
            &canned.url,
            true,
            &credentials,
            LineWriter {
                tx,
                buf: Vec::new(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "sign-in denied in the browser");

        let printed = rx.recv().await.expect("the sign-in url is printed");
        assert!(!printed.contains(' '), "{printed}");
        assert!(printed.starts_with("http://127.0.0.1:8080/device?code=BCDF-GHJK&"));
        assert!(printed.contains("note=a%20b"), "{printed}");
    }

    #[tokio::test]
    async fn a_user_code_that_is_not_in_the_shape_is_refused_before_anything_is_printed() {
        for user_code in ["bcdf-ghjk", "BCDF-GHJK\n"] {
            let canned = Canned::json(vec![(
                200,
                code_answer_with("http://127.0.0.1:8080/device?code=BCDF-GHJK", user_code),
            )])
            .await;
            let dir = tempfile::tempdir().unwrap();
            let credentials = dir.path().join("cetra").join("credentials.toml");
            let mut printed = Vec::new();
            let error = login(&canned.url, true, &credentials, &mut printed)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), BAD_CODE, "{user_code:?}");
            assert!(printed.is_empty(), "{user_code:?}");
        }
    }

    #[test]
    fn other_answers_fail_the_sign_in() {
        assert_eq!(
            step(400, br#"{"code":"invalid_request"}"#),
            Step::Failed(400)
        );
        assert_eq!(step(500, PENDING.as_bytes()), Step::Failed(500));
        assert_eq!(step(400, b"not json"), Step::Failed(400));
    }

    #[test]
    fn a_slow_down_adds_five_seconds() {
        assert_eq!(
            next_interval(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
    }

    #[tokio::test]
    async fn login_refuses_a_url_with_credentials_before_printing() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("cetra").join("credentials.toml");
        let mut printed = Vec::new();
        let error = login("http://user:pw@h", true, &credentials, &mut printed)
            .await
            .unwrap_err();
        let text = error.to_string();
        assert_eq!(text, "the server URL must not contain credentials");
        assert!(!text.contains("user") && !text.contains("pw"));
        assert!(printed.is_empty());
        assert!(!credentials.exists());
    }

    #[tokio::test]
    async fn a_pending_code_is_polled_until_the_grant_and_the_token_is_saved() {
        let token = example_token();
        let canned = Canned::json(vec![
            (200, code_answer()),
            (400, PENDING.to_owned()),
            (200, grant(&token)),
            (200, "{}".to_owned()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("cetra").join("credentials.toml");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        login(
            &canned.url,
            true,
            &credentials,
            LineWriter {
                tx,
                buf: Vec::new(),
            },
        )
        .await
        .unwrap();

        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        assert_eq!(
            lines,
            vec![
                "http://127.0.0.1:8080/device?code=BCDF-GHJK".to_owned(),
                "Code: BCDF-GHJK".to_owned(),
            ]
        );

        let requests = canned.recorded();
        let targets: Vec<&str> = requests.iter().map(|r| r.target.as_str()).collect();
        assert_eq!(
            targets,
            [
                "/api/auth/device/code",
                "/api/auth/device/token",
                "/api/auth/device/token",
                "/api/auth/me",
            ]
        );
        assert!(requests[0].body.contains(r#""purpose":"login""#));
        for request in &requests[..3] {
            let head = request.head.to_ascii_lowercase();
            assert!(
                !head.contains("\norigin:") && !head.contains("\nauthorization:"),
                "{head}"
            );
        }

        let saved = load(&credentials).unwrap();
        assert_eq!(
            saved.servers[&normalize_server_url(&canned.url)].token,
            token
        );
    }

    #[tokio::test]
    async fn a_denied_code_fails_and_saves_nothing() {
        let canned = Canned::json(vec![
            (200, code_answer()),
            (400, r#"{"code":"access_denied"}"#.to_owned()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("cetra").join("credentials.toml");
        let error = login(&canned.url, true, &credentials, Vec::<u8>::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "sign-in denied in the browser");
        assert!(!credentials.exists());
    }

    #[tokio::test]
    async fn an_expired_code_fails_and_saves_nothing() {
        let canned = Canned::json(vec![
            (200, code_answer()),
            (400, r#"{"code":"expired_token"}"#.to_owned()),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("cetra").join("credentials.toml");
        let error = login(&canned.url, true, &credentials, Vec::<u8>::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), EXPIRED);
        assert!(!credentials.exists());
    }

    #[tokio::test]
    async fn a_grant_with_a_secret_key_is_refused_and_saves_nothing() {
        let token = format!(
            "sk-{}-EXAMPLE0EXAMPLE0EXAMPLE0EXAMPLE0",
            cetra_core::names::PRODUCT
        );
        let canned = Canned::json(vec![(200, code_answer()), (200, grant(&token))]).await;
        let dir = tempfile::tempdir().unwrap();
        let credentials = dir.path().join("cetra").join("credentials.toml");
        let error = login(&canned.url, true, &credentials, Vec::<u8>::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "the server returned an invalid token");
        assert!(!credentials.exists());
    }

    #[tokio::test]
    async fn login_saves_a_token_and_logout_revokes_it() {
        let dir = std::env::temp_dir().join(format!(
            "cetra-login-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let data = dir.join("data");
        let config_path = dir.join("cetra.toml");
        fs::write(
            &config_path,
            format!(
                "data_dir = \"{}\"\nhttp_addr = \"127.0.0.1:0\"\notlp_http_addr = \"\"\ngrpc_addr = \"\"\npublic_url = \"http://127.0.0.1:8080\"\n\n[init]\nadmin_email = \"ada@example.com\"\nadmin_password = \"p17-cli-login-secret\"\norg = \"Acme\"\nproject = \"App\"\n",
                data.display()
            ),
        )
        .unwrap();
        let config = cetra_server::config::ServerConfig::load(Some(&config_path)).unwrap();
        let running = cetra_server::serve::bind(config, None).await.unwrap();
        let server = format!("http://{}", running.local_addr);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        let logged_in = client
            .post(format!("{server}/api/auth/login"))
            .header("origin", "http://127.0.0.1:8080")
            .json(&serde_json::json!({
                "email": "ada@example.com",
                "password": "p17-cli-login-secret"
            }))
            .send()
            .await
            .unwrap();
        assert!(logged_in.status().is_success(), "{}", logged_in.status());
        let cookie = logged_in
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split_once(';')
            .unwrap()
            .0
            .to_owned();

        let credentials = dir.join("cetra").join("credentials.toml");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let server_for_login = server.clone();
        let credentials_for_login = credentials.clone();
        let login_task = tokio::spawn(async move {
            login(
                &server_for_login,
                true,
                &credentials_for_login,
                LineWriter {
                    tx,
                    buf: Vec::new(),
                },
            )
            .await
        });
        let printed = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("login did not print a url")
            .expect("login closed the url channel");
        let code = query_value(&printed, "code").expect("device code in the url");

        // The session approves the code. The server's poll then hands the token to the CLI.
        let approved = client
            .post(format!("{server}/api/auth/device/{code}/approve"))
            .header("origin", "http://127.0.0.1:8080")
            .header(reqwest::header::COOKIE, &cookie)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(approved.status(), 204, "{}", approved.status());

        // The first poll comes one interval (5 s) after the code was issued.
        tokio::time::timeout(Duration::from_secs(15), login_task)
            .await
            .expect("login did not finish after the approval")
            .unwrap()
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&credentials).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = fs::metadata(credentials.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        let saved = load(&credentials).unwrap();
        let secret = saved.servers[&normalize_server_url(&server)].token.clone();
        assert!(secret.starts_with("pat-"), "the saved token is a PAT");

        logout(Some(&server), &credentials).await.unwrap();
        assert!(
            !credentials.exists()
                || !load(&credentials)
                    .unwrap()
                    .servers
                    .contains_key(&normalize_server_url(&server))
        );
        let me = client
            .get(format!("{server}/api/auth/me"))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {secret}"))
            .send()
            .await
            .unwrap();
        assert_eq!(me.status(), 401);

        running.shutdown().await.unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
