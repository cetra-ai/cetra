//! `cetra agents hook <agent>`: run by a coding agent's hook. Reads the hook JSON on stdin,
//! posts the session's repository, branch and person to `POST /v1/agent-context`, and never
//! fails. Nothing goes to stdout; notes go to stderr (D-CA-8).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Agent, AgentsFile, Paths};

/// The whole hook, from start to the response (D-CA-8).
const BUDGET: Duration = Duration::from_secs(2);
const STDIN_LIMIT: u64 = 65_536;
const STDIN_WAIT: Duration = Duration::from_millis(500);
const GIT_LIMIT: Duration = Duration::from_millis(800);
/// Time kept for the post: git gets what is left beyond this.
const POST_RESERVE: Duration = Duration::from_millis(400);
const MIN_POST: Duration = Duration::from_millis(100);

#[derive(Deserialize)]
struct HookInput {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    cwd: String,
}

/// What the post needs, prepared off the async runtime.
struct Prepared {
    file: AgentsFile,
    body: Value,
}

/// Runs the hook. Never fails and never writes to stdout; notes go to `err`.
pub(crate) async fn hook(
    agent: Agent,
    paths: &Paths,
    stdin: impl Read + Send + 'static,
    err: &mut dyn Write,
) {
    let deadline = Instant::now() + BUDGET;
    if cfg!(windows) {
        return;
    }
    let paths = paths.clone();
    let prepared =
        tokio::task::spawn_blocking(move || prepare(agent, &paths, stdin, deadline)).await;
    let prepared = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(note)) => {
            note_line(err, note);
            return;
        }
        Err(_) => {
            note_line(err, "hook failed");
            return;
        }
    };
    post(prepared, deadline, err).await;
}

/// Steps 4 to 8 of the hook: stdin, parse, agents.toml, git, body.
fn prepare(
    agent: Agent,
    paths: &Paths,
    stdin: impl Read + Send + 'static,
    deadline: Instant,
) -> Result<Prepared, &'static str> {
    let input = read_input(stdin, deadline)?;
    let hook_input: HookInput =
        serde_json::from_slice(&input).map_err(|_| "hook input is not valid JSON")?;
    if hook_input.session_id.is_empty() || hook_input.session_id.chars().count() > 256 {
        return Err("hook input has no session id");
    }
    let file = AgentsFile::load(&paths.agents_file())
        .map_err(|_| "agents.toml is invalid")?
        .ok_or("not set up; run cetra agents setup")?;

    // Both git calls run at once, each with the same timeout.
    let timeout = git_timeout(deadline);
    let cwd = hook_input.cwd.as_str();
    let (repository, branch) = thread::scope(|scope| {
        let remote = scope.spawn(|| git_value(cwd, &["remote", "get-url", "origin"], timeout));
        let head = scope.spawn(|| git_value(cwd, &["rev-parse", "--abbrev-ref", "HEAD"], timeout));
        (remote.join().ok().flatten(), head.join().ok().flatten())
    });
    let repository = repository.map(|url| strip_user_info(&url));
    let branch = branch.filter(|branch| branch != "HEAD");

    let mut body = json!({
        "agent": agent.wire_name(),
        "session_id": hook_input.session_id,
        "repository": repository,
        "branch": branch,
    });
    if let Some(user) = &file.user {
        body["user"] = json!(user);
    }
    Ok(Prepared { file, body })
}

/// Reads stdin to EOF on a thread, at most `STDIN_LIMIT` bytes. Waits at most `STDIN_WAIT`
/// and never past the deadline; a reader still blocked then is left behind.
fn read_input(
    stdin: impl Read + Send + 'static,
    deadline: Instant,
) -> Result<Vec<u8>, &'static str> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdin.take(STDIN_LIMIT).read_to_end(&mut bytes);
        let _ = sender.send(result.map(|_| bytes));
    });
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .min(STDIN_WAIT);
    match receiver.recv_timeout(wait) {
        Ok(Ok(bytes)) => Ok(bytes),
        _ => Err("no hook input"),
    }
}

/// The timeout of each git call: `min(GIT_LIMIT, time left - POST_RESERVE)`. `None` when that
/// is not positive, and then git is not run at all.
fn git_timeout(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    let budget = left.checked_sub(POST_RESERVE)?;
    (!budget.is_zero()).then(|| budget.min(GIT_LIMIT))
}

/// The trimmed stdout of `git -C <cwd> <args>`, or `None` when it is empty, fails or `timeout`
/// is `None`.
fn git_value(cwd: &str, args: &[&str], timeout: Option<Duration>) -> Option<String> {
    let timeout = timeout?;
    let mut full = vec!["-C", cwd];
    full.extend_from_slice(args);
    let output = run_with_timeout("git", &full, &[], timeout)?;
    let value = output.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Posts the body to `{url}/v1/agent-context`. Notes only; the error text never carries the
/// URL, because `reqwest::Error` does.
async fn post(prepared: Prepared, deadline: Instant, err: &mut dyn Write) {
    let left = deadline.saturating_duration_since(Instant::now());
    if left < MIN_POST {
        return note_line(err, "out of time");
    }
    let client = match reqwest::Client::builder()
        .connect_timeout(left)
        .timeout(left)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(_) => return note_line(err, "could not reach the server"),
    };
    let url = format!("{}/v1/agent-context", prepared.file.url);
    let response = client
        .post(url)
        .bearer_auth(&prepared.file.key)
        .json(&prepared.body)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => note_line(
            err,
            &format!("server answered {}", response.status().as_u16()),
        ),
        Err(_) => note_line(err, "could not reach the server"),
    }
}

/// Writes one note line to `err`. A failed write is ignored: the hook never fails.
fn note_line(err: &mut dyn Write, note: &str) {
    let _ = writeln!(err, "cetra agents hook: {note}");
}

/// Runs `program args` with no stdin and the extra `envs`, captures stdout, and kills it at
/// `timeout`. `None` on spawn failure, timeout, non-zero exit or non-UTF-8 output.
pub(super) fn run_with_timeout(
    program: &str,
    args: &[&str],
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut stdout = Vec::new();
    child.stdout.take()?.read_to_end(&mut stdout).ok()?;
    String::from_utf8(stdout).ok()
}

/// Removes `user[:password]@` from the authority of a `scheme://` URL. Other text is returned
/// unchanged.
pub(super) fn strip_user_info(remote: &str) -> String {
    let Some(scheme_end) = remote.find("://") else {
        return remote.to_owned();
    };
    let start = scheme_end + 3;
    let rest = &remote[start..];
    let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_len];
    let Some(at) = authority.rfind('@') else {
        return remote.to_owned();
    };
    format!("{}{}", &remote[..start], &rest[at + 1..])
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use serde_json::Value;

    use super::{hook, run_with_timeout, strip_user_info};
    use crate::agents::{Agent, AgentsFile, Paths};
    use crate::test_server::{Canned, Recorded};

    #[test]
    fn strip_user_info_removes_credentials_from_the_authority() {
        let cases = [
            (
                "https://tok@github.com/o/r.git",
                "https://github.com/o/r.git",
            ),
            ("https://u:p@ss@host/o/r", "https://host/o/r"),
            ("https://host/o?x=a@b", "https://host/o?x=a@b"),
            ("git@github.com:o/r.git", "git@github.com:o/r.git"),
            ("https://github.com/o/r", "https://github.com/o/r"),
            ("/local/path", "/local/path"),
        ];
        for (input, expected) in cases {
            assert_eq!(strip_user_info(input), expected, "{input}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_a_slow_program() {
        let started = Instant::now();
        let output = run_with_timeout("sleep", &["5"], &[], Duration::from_millis(100));
        assert_eq!(output, None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_captures_stdout_and_env() {
        let hi = run_with_timeout("echo", &["hi"], &[], Duration::from_secs(1));
        assert_eq!(hi.as_deref(), Some("hi\n"));
        let env = run_with_timeout(
            "sh",
            &["-c", "echo $X"],
            &[("X", "y")],
            Duration::from_secs(1),
        );
        assert_eq!(env.as_deref(), Some("y\n"));
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_returns_none_for_a_missing_program() {
        let output = run_with_timeout("cetra-no-such-program", &[], &[], Duration::from_secs(1));
        assert_eq!(output, None);
    }

    /// Paths under `root`: `root/cetra` holds agents.toml.
    fn paths_under(root: &Path) -> Paths {
        Paths {
            home: root.to_path_buf(),
            cetra_dir: root.join("cetra"),
            exe: root.join("cetra-bin"),
        }
    }

    fn save_agents_file(paths: &Paths, url: &str, user: Option<&str>) {
        AgentsFile {
            url: url.to_owned(),
            key: "sk-cetra-test".to_owned(),
            user: user.map(str::to_owned),
        }
        .save(&paths.agents_file())
        .unwrap();
    }

    /// Runs `git` in `dir` with no system config and `home` as HOME.
    #[cfg(unix)]
    fn git(home: &Path, dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", home)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// A repository on `feature/x` with origin `https://tok@github.com/Acme/Widgets.git`.
    #[cfg(unix)]
    fn repository(home: &Path) -> std::path::PathBuf {
        let repo = home.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(home, &repo, &["init", "-b", "feature/x"]);
        git(
            home,
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "i",
            ],
        );
        git(
            home,
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://tok@github.com/Acme/Widgets.git",
            ],
        );
        repo
    }

    fn hook_input(session: &str, cwd: &Path) -> Cursor<Vec<u8>> {
        let text = format!(
            r#"{{"session_id":"{session}","cwd":{},"hook_event_name":"SessionStart"}}"#,
            serde_json::to_string(&cwd.display().to_string()).unwrap()
        );
        Cursor::new(text.into_bytes())
    }

    fn body_of(recorded: &Recorded) -> Value {
        serde_json::from_str(&recorded.body).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_posts_repository_and_branch() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let repo = repository(home);
        let paths = paths_under(home);
        let server = Canned::json(vec![(204, String::new())]).await;
        save_agents_file(&paths, &server.url, Some("dev@example.com"));

        let mut err = Vec::new();
        hook(Agent::ClaudeCode, &paths, hook_input("s1", &repo), &mut err).await;

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].head.starts_with("POST /v1/agent-context"));
        assert!(
            recorded[0]
                .head
                .to_ascii_lowercase()
                .contains("authorization: bearer sk-cetra-test")
        );
        assert_eq!(
            body_of(&recorded[0]),
            serde_json::json!({
                "agent": "claude-code",
                "session_id": "s1",
                "repository": "https://github.com/Acme/Widgets.git",
                "branch": "feature/x",
                "user": "dev@example.com",
            })
        );
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_sends_null_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let server = Canned::json(vec![(204, String::new())]).await;
        save_agents_file(&paths, &server.url, None);

        let mut err = Vec::new();
        hook(Agent::Codex, &paths, hook_input("s2", &plain), &mut err).await;

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 1);
        let body = body_of(&recorded[0]);
        assert_eq!(body["repository"], Value::Null);
        assert_eq!(body["branch"], Value::Null);
        assert!(body.get("user").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_sends_null_branch_when_detached() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let repo = repository(home);
        git(home, &repo, &["checkout", "--detach"]);
        let paths = paths_under(home);
        let server = Canned::json(vec![(204, String::new())]).await;
        save_agents_file(&paths, &server.url, None);

        let mut err = Vec::new();
        hook(Agent::ClaudeCode, &paths, hook_input("s3", &repo), &mut err).await;

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(body_of(&recorded[0])["branch"], Value::Null);
    }

    #[tokio::test]
    async fn hook_without_agents_toml_posts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let server = Canned::json(vec![(204, String::new())]).await;

        let mut err = Vec::new();
        hook(Agent::Codex, &paths, hook_input("s4", dir.path()), &mut err).await;

        assert!(server.recorded().is_empty());
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("not set up"), "{text}");
    }

    #[tokio::test]
    async fn hook_with_bad_input_posts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        let server = Canned::json(vec![(204, String::new())]).await;
        save_agents_file(&paths, &server.url, None);

        let invalid = Cursor::new(b"not json".to_vec());
        let mut err = Vec::new();
        hook(Agent::Codex, &paths, invalid, &mut err).await;
        let no_session = Cursor::new(br#"{"cwd":"/tmp"}"#.to_vec());
        hook(Agent::Codex, &paths, no_session, &mut err).await;

        assert!(server.recorded().is_empty());
        let text = String::from_utf8(err).unwrap();
        assert!(text.contains("not valid JSON"), "{text}");
        assert!(text.contains("no session id"), "{text}");
    }

    #[tokio::test]
    async fn hook_returns_within_its_budget_when_the_server_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_under(dir.path());
        // Bind and drop a listener to get a port that refuses connections.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        save_agents_file(&paths, &format!("http://127.0.0.1:{port}"), None);

        let started = Instant::now();
        let mut err = Vec::new();
        hook(Agent::Codex, &paths, hook_input("s5", dir.path()), &mut err).await;

        assert!(started.elapsed() < Duration::from_millis(2500));
        assert!(!err.is_empty());
    }

    #[tokio::test]
    async fn agents_file_round_trips_with_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cetra").join("agents.toml");
        let file = AgentsFile {
            url: "http://h:1".to_owned(),
            key: "sk-cetra-k".to_owned(),
            user: Some("dev@example.com".to_owned()),
        };
        file.save(&path).unwrap();
        assert_eq!(AgentsFile::load(&path).unwrap(), Some(file));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(
            AgentsFile::load(&dir.path().join("missing.toml"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn hook_through_run_cli_exits_zero() {
        // Nothing is posted without agents.toml; skip when this machine has one.
        if Paths::detect().unwrap().agents_file().exists() {
            return;
        }
        crate::run_cli(["cetra", "agents", "hook", "codex"])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hook_with_bad_arguments_through_run_cli_exits_zero() {
        // A missing agent is a clap error; the hook still reports success.
        crate::run_cli(["cetra", "agents", "hook"]).await.unwrap();
    }

    #[test]
    fn agents_file_debug_hides_the_key() {
        let file = AgentsFile {
            url: "http://h:1".to_owned(),
            key: "sk-cetra-secret".to_owned(),
            user: Some("dev@example.com".to_owned()),
        };
        let text = format!("{file:?}");
        assert!(!text.contains("sk-cetra-secret"), "{text}");
        assert!(!text.contains("dev@example.com"), "{text}");
    }
}
