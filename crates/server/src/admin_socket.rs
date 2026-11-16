//! The local control socket of `llmobs serve` (N M2, D-N-17).
//!
//! `<data_dir>/admin.sock` answers one request per connection: the client writes
//! one JSON line and reads one JSON line back, then the server closes the
//! connection. `llmobs admin backup` is the only client in M2.
//!
//! The socket is bound as `admin.tmp`, made `0600`, then renamed, so it never
//! exists under its real name with a wider mode. A connecting process is accepted
//! only when its uid is the server's own uid or root (`SO_PEERCRED`): macOS
//! ignores socket file permissions on `connect`, so this check is the authentication.
//! The server's uid is the owner of the socket file it binds. It is not the owner of
//! the data directory, which on Kubernetes is usually root while the server runs as
//! a non-root uid.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};

use crate::backup::{self, BackupReport};
use crate::http::AppState;

/// The socket's file name in the data directory.
pub const ADMIN_SOCKET: &str = "admin.sock";
/// Where the socket is bound before it is renamed to [`ADMIN_SOCKET`]. It is as long
/// as `admin.sock`, so a data directory whose path fits the Unix socket path limit
/// for `admin.sock` also fits here. No other file of the data directory has this name.
const ADMIN_SOCKET_TMP: &str = "admin.tmp";
/// Mode of the socket file.
const SOCKET_MODE: u32 = 0o600;
/// The largest request, including its newline.
const MAX_REQUEST: usize = 64 * 1024;
/// How long a client may take to send its request. A backup itself has no limit.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the accept loop waits after a failed `accept` (for example `EMFILE`).
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// The admin protocol version (D-R-36). Every request and response carries it as `"v"`.
/// Within version 1 a change only adds: a new command, a new request field with a
/// default, a new response field, a new error code. Renaming or removing any of them,
/// or changing a field's meaning, is version 2.
pub const ADMIN_PROTOCOL: u32 = 1;

/// One request on the socket.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum AdminRequest {
    /// Takes an online backup into `to` (`backup::run`).
    Backup {
        to: PathBuf,
        #[serde(default)]
        meta_only: bool,
    },
}

/// One request line: the protocol version and the command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdminEnvelope {
    pub v: u32,
    #[serde(flatten)]
    pub request: AdminRequest,
}

/// One response on the socket. The client decides on the variant, not on `ok`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum AdminResponse {
    /// The request succeeded (`ok` is `true`).
    Done {
        v: u32,
        ok: bool,
        report: BackupReport,
    },
    /// The request failed (`ok` is `false`), with a stable `code`.
    Failed {
        v: u32,
        ok: bool,
        code: String,
        message: String,
    },
}

impl AdminResponse {
    fn failed(code: &str, message: impl Into<String>) -> Self {
        Self::Failed {
            v: ADMIN_PROTOCOL,
            ok: false,
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// Why a response line could not be read (the client side of D-R-36).
#[derive(Debug, thiserror::Error)]
pub enum AdminProtocolError {
    #[error("the server's answer is not JSON")]
    NotJson,
    #[error(
        "the server speaks admin protocol {found}, this command speaks {ADMIN_PROTOCOL}; run the llmobs binary of the server's release"
    )]
    OtherProtocol { found: String },
    #[error("the server's answer has an unknown shape")]
    Shape,
}

/// Parses one response line: JSON, then `v`, then the shape.
pub fn decode_response(line: &str) -> Result<AdminResponse, AdminProtocolError> {
    let value: Value = serde_json::from_str(line).map_err(|_| AdminProtocolError::NotJson)?;
    match value.get("v").and_then(Value::as_u64) {
        Some(version) if version == u64::from(ADMIN_PROTOCOL) => {}
        Some(version) => {
            return Err(AdminProtocolError::OtherProtocol {
                found: version.to_string(),
            });
        }
        None => {
            return Err(AdminProtocolError::OtherProtocol {
                found: "none".to_owned(),
            });
        }
    }
    serde_json::from_value(value).map_err(|_| AdminProtocolError::Shape)
}

/// Binds `<data_dir>/admin.sock` (replacing a stale file; `LOCK` is held) with mode
/// `0600` and serves requests until the returned task is aborted.
///
/// A bind failure, for example a path longer than the 108-byte Unix socket limit,
/// is logged and returns `None`: the server runs without the socket.
pub(crate) fn spawn(state: AppState, data_dir: &Path) -> Option<JoinHandle<()>> {
    let (listener, server_uid) = match bind_socket(data_dir) {
        Ok(bound) => bound,
        Err(error) => {
            tracing::warn!(error = %error, "admin socket unavailable; online backups cannot run");
            return None;
        }
    };
    tracing::info!("admin socket listening");
    Some(tokio::spawn(accept_loop(listener, state, server_uid)))
}

/// Removes `<data_dir>/admin.sock` and a leftover `<data_dir>/admin.tmp`. A missing
/// file is not an error.
pub(crate) fn remove_socket_file(data_dir: &Path) -> io::Result<()> {
    remove_file_if_exists(&data_dir.join(ADMIN_SOCKET))?;
    remove_file_if_exists(&data_dir.join(ADMIN_SOCKET_TMP))
}

/// Binds the socket and returns it with the server's uid: the owner of the file this
/// process just created, read before the rename so it is the bound file.
fn bind_socket(data_dir: &Path) -> io::Result<(UnixListener, u32)> {
    let socket = data_dir.join(ADMIN_SOCKET);
    let temporary = data_dir.join(ADMIN_SOCKET_TMP);
    remove_socket_file(data_dir)?;
    let listener = UnixListener::bind(&temporary)?;
    let secured = std::fs::metadata(&temporary).and_then(|metadata| {
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(SOCKET_MODE))?;
        std::fs::rename(&temporary, &socket)?;
        Ok(metadata.uid())
    });
    match secured {
        Ok(server_uid) => Ok((listener, server_uid)),
        Err(error) => {
            // Best effort: the temporary name is the only thing left to remove.
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Accepts connections until aborted. Aborting drops `connections`, which aborts
/// every connection still running, and the backup it runs.
async fn accept_loop(listener: UnixListener, state: AppState, server_uid: u32) {
    let mut connections = JoinSet::new();
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                while connections.try_join_next().is_some() {}
                connections.spawn(serve_connection(stream, state.clone(), server_uid));
            }
            Err(error) => {
                tracing::warn!(error = %error, "admin socket accept failed");
                tokio::time::sleep(ACCEPT_RETRY).await;
            }
        }
    }
}

async fn serve_connection(mut stream: UnixStream, state: AppState, server_uid: u32) {
    if !peer_is_allowed(&stream, server_uid) {
        tracing::warn!("admin socket refused a connection from another user");
        write_response(
            &mut stream,
            &AdminResponse::failed(
                "forbidden",
                "only the server's user or root may use the socket",
            ),
        )
        .await;
        return;
    }
    // Silence on timeout or on a read error: there is no client to answer.
    let Ok(Ok(line)) = tokio::time::timeout(READ_TIMEOUT, read_line(&mut stream)).await else {
        return;
    };
    let response = match parse(&line) {
        Err(response) => response,
        Ok(AdminRequest::Backup { to, meta_only }) => run_backup(&state, to, meta_only).await,
    };
    write_response(&mut stream, &response).await;
}

/// Runs the backup in its own task, so a client that disconnects does not cancel it
/// half-way. Aborting this connection (the JoinSet, at shutdown) aborts the backup
/// through [`AbortOnDrop`].
async fn run_backup(state: &AppState, to: PathBuf, meta_only: bool) -> AdminResponse {
    let task = tokio::spawn({
        let state = state.clone();
        async move { backup::run(&state, &to, meta_only).await }
    });
    let _abort = AbortOnDrop(task.abort_handle());
    match task.await {
        Ok(Ok(report)) => AdminResponse::Done {
            v: ADMIN_PROTOCOL,
            ok: true,
            report,
        },
        Ok(Err(error)) => AdminResponse::failed(error.code(), error.to_string()),
        Err(_join) => AdminResponse::failed("io", "the backup task stopped"),
    }
}

/// Aborts the task it holds when dropped. A connection task that is aborted or
/// dropped mid-backup takes its backup down with it.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Accepts the server's own uid and root. A peer whose uid cannot be read is refused.
fn peer_is_allowed(stream: &UnixStream, server_uid: u32) -> bool {
    let Ok(cred) = stream.peer_cred() else {
        return false;
    };
    allowed(cred.uid(), server_uid)
}

/// The uid rule: root, or the same uid as the server.
fn allowed(peer_uid: u32, server_uid: u32) -> bool {
    peer_uid == 0 || peer_uid == server_uid
}

/// Reads up to the first newline, or to end of input. It reads one byte past the
/// limit, so a longer request is detected and not truncated silently.
async fn read_line(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut reader = BufReader::new(stream.take(MAX_REQUEST as u64 + 1));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    Ok(line)
}

/// Four steps: the JSON, then the protocol version (D-R-36), then the command, then its
/// fields.
fn parse(line: &[u8]) -> Result<AdminRequest, AdminResponse> {
    if line.len() > MAX_REQUEST {
        return Err(AdminResponse::failed(
            "bad_request",
            "the request is longer than 64 KiB",
        ));
    }
    let value: Value = serde_json::from_slice(line)
        .map_err(|_| AdminResponse::failed("bad_request", "the request is not JSON"))?;
    match value.get("v").and_then(Value::as_u64) {
        Some(version) if version == u64::from(ADMIN_PROTOCOL) => {}
        Some(version) => {
            return Err(unsupported_protocol(&format!("version {version}")));
        }
        None => return Err(unsupported_protocol("no version")),
    }
    if let Some(command) = value.get("command").and_then(Value::as_str)
        && command != "backup"
    {
        return Err(AdminResponse::failed("unknown_command", "unknown command"));
    }
    serde_json::from_value::<AdminEnvelope>(value)
        .map(|envelope| envelope.request)
        .map_err(|_| AdminResponse::failed("bad_request", "the request does not match a command"))
}

/// The refusal of a request whose `v` is not [`ADMIN_PROTOCOL`]; `found` is `version N`
/// or `no version`.
fn unsupported_protocol(found: &str) -> AdminResponse {
    AdminResponse::failed(
        "unsupported_protocol",
        format!(
            "this server (llmobs {}) speaks admin protocol {ADMIN_PROTOCOL}; the request has {found}; run the llmobs binary of the server's release",
            env!("CARGO_PKG_VERSION")
        ),
    )
}

/// Writes the response line, then shuts the connection down. A failed write is not
/// reported anywhere: the client has gone.
async fn write_response(stream: &mut UnixStream, response: &AdminResponse) {
    let Ok(mut bytes) = serde_json::to_vec(response) else {
        return;
    };
    bytes.push(b'\n');
    let _ = stream.write_all(&bytes).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::UnixListener as StdUnixListener;

    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use super::*;
    use crate::backup::{BackupError, BackupMode};
    use crate::serve::bind;
    use crate::test_support::config;

    /// Sends `request` and returns the one response line.
    async fn ask(socket: &Path, request: &str) -> String {
        let mut stream = UnixStream::connect(socket).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        response
    }

    /// The response's `code`, after checking that `ok` is false.
    fn refusal_code(response: &str) -> String {
        let value: Value = serde_json::from_str(response).unwrap();
        assert_eq!(value["ok"], false, "{response}");
        value["code"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn the_socket_exists_with_mode_0600_and_is_removed_at_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let socket = dir.path().join(ADMIN_SOCKET);
        let metadata = std::fs::symlink_metadata(&socket).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert!(!dir.path().join(ADMIN_SOCKET_TMP).exists());
        running.shutdown().await.unwrap();
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn a_data_dir_that_fits_the_socket_path_for_admin_sock_gets_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        // The socket path is 106 bytes: under the 108-byte limit with `admin.sock`
        // (9 bytes), but over it with the 13 bytes of the old `admin.sock.tmp`.
        let base = dir.path().as_os_str().len();
        assert!(
            base < 90,
            "the temporary directory path is too long for this test"
        );
        let data = dir.path().join("d".repeat(96 - base - 1));
        assert_eq!(data.as_os_str().len(), 96);
        let running = bind(config(&data), None).await.unwrap();
        let response = ask(&data.join(ADMIN_SOCKET), "not json\n").await;
        assert_eq!(refusal_code(&response), "bad_request");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_oversized_request_is_refused_and_the_next_client_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let socket = dir.path().join(ADMIN_SOCKET);
        // One byte past the cap, with no newline. The server reads exactly the cap and
        // refuses, so it leaves no unread input that would reset the connection.
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        stream
            .write_all(&vec![b'a'; MAX_REQUEST + 1])
            .await
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        assert_eq!(refusal_code(&response), "bad_request");

        let next = ask(&socket, "{\"v\":1,\"command\":\"restore\"}\n").await;
        assert_eq!(refusal_code(&next), "unknown_command");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_backup_request_returns_the_report() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let running = bind(config(&data), None).await.unwrap();
        let target = root.path().join("backup");
        // Built as the CLI builds it (`backup_command`).
        let request = serde_json::to_string(&AdminEnvelope {
            v: ADMIN_PROTOCOL,
            request: AdminRequest::Backup {
                to: target.clone(),
                meta_only: false,
            },
        })
        .unwrap();
        let response = ask(&data.join(ADMIN_SOCKET), &format!("{request}\n")).await;

        match decode_response(&response).unwrap() {
            AdminResponse::Done { ok, report, .. } => {
                assert!(ok, "{response}");
                assert_eq!(report.mode, BackupMode::Full);
            }
            AdminResponse::Failed { .. } => panic!("{response}"),
        }
        assert!(target.join("manifest.json").is_file());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bad_input_gets_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let response = ask(&dir.path().join(ADMIN_SOCKET), "not json\n").await;
        assert_eq!(refusal_code(&response), "bad_request");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_unknown_command_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let response = ask(
            &dir.path().join(ADMIN_SOCKET),
            "{\"v\":1,\"command\":\"restore\"}\n",
        )
        .await;
        assert_eq!(refusal_code(&response), "unknown_command");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_request_without_the_protocol_version_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let running = bind(config(&data), None).await.unwrap();
        let target = root.path().join("backup");
        let socket = data.join(ADMIN_SOCKET);
        let cases = [
            json!({"command": "backup", "to": target}),
            json!({"v": 2, "command": "backup", "to": target}),
        ];
        for request in cases {
            let response = ask(&socket, &format!("{request}\n")).await;
            assert_eq!(
                refusal_code(&response),
                "unsupported_protocol",
                "{response}"
            );
            let message = serde_json::from_str::<Value>(&response).unwrap()["message"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(message.contains("protocol 1"), "{message}");
            assert!(message.contains(env!("CARGO_PKG_VERSION")), "{message}");
        }
        // Refused before the backup starts: nothing was written.
        assert!(!target.exists());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn every_response_carries_the_protocol_version() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let running = bind(config(&data), None).await.unwrap();
        let socket = data.join(ADMIN_SOCKET);
        let target = root.path().join("backup");
        let lines = [
            ask(
                &socket,
                &format!(
                    "{}\n",
                    json!({"v": 1, "command": "backup", "to": target, "meta_only": false})
                ),
            )
            .await,
            ask(&socket, "not json\n").await,
            ask(&socket, "{\"v\":1,\"command\":\"restore\"}\n").await,
            // `forbidden` cannot be produced from a test (it needs another uid).
            serde_json::to_string(&AdminResponse::failed("forbidden", "m")).unwrap() + "\n",
        ];
        for line in lines {
            let value: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["v"], 1, "{line}");
        }
        running.shutdown().await.unwrap();
    }

    #[test]
    fn the_client_refuses_another_protocol() {
        assert!(matches!(
            decode_response("{\"v\":2,\"ok\":true}"),
            Err(AdminProtocolError::OtherProtocol { found }) if found == "2"
        ));
        assert!(matches!(
            decode_response("{\"ok\":false,\"code\":\"busy\",\"message\":\"m\"}"),
            Err(AdminProtocolError::OtherProtocol { found }) if found == "none"
        ));
        assert!(matches!(
            decode_response("nope"),
            Err(AdminProtocolError::NotJson)
        ));
        // Additive changes within version 1 are read: an unknown field is ignored.
        let report = BackupReport {
            dir: PathBuf::from("/x"),
            mode: BackupMode::Full,
            created_at: "2026-11-16T00:00:00Z".to_owned(),
            objects: 0,
            object_bytes: 0,
            linked: 0,
            copied: 0,
            duration_ms: 1,
        };
        let mut line = serde_json::to_value(AdminResponse::Done {
            v: ADMIN_PROTOCOL,
            ok: true,
            report,
        })
        .unwrap();
        line["later"] = json!(1);
        assert!(matches!(
            decode_response(&line.to_string()),
            Ok(AdminResponse::Done { ok: true, .. })
        ));
    }

    #[test]
    fn the_error_codes_are_stable() {
        let backup_errors = [
            BackupError::Busy,
            BackupError::RelativeTarget,
            BackupError::TargetNotEmpty,
            BackupError::TargetInsideDataDir,
            BackupError::MetaOnlyNeedsS3,
            BackupError::Flush(llmobs_storage::FlushNowError::Timeout {
                table: "spans",
                timeout: Duration::from_secs(1),
            }),
            BackupError::Meta(sqlx::Error::PoolClosed),
            BackupError::Snapshot(llmobs_storage::SnapshotError::Exists),
            BackupError::Objects(llmobs_storage::CopyObjectsError::RemoteStore),
            BackupError::Io(io::Error::other("x")),
        ];
        // Exhaustive: a new `BackupError` variant stops this test from compiling until its
        // value is added to `backup_errors` above.
        for error in &backup_errors {
            match error {
                BackupError::Busy
                | BackupError::RelativeTarget
                | BackupError::TargetNotEmpty
                | BackupError::TargetInsideDataDir
                | BackupError::MetaOnlyNeedsS3
                | BackupError::Flush(_)
                | BackupError::Meta(_)
                | BackupError::Snapshot(_)
                | BackupError::Objects(_)
                | BackupError::Io(_) => {}
            }
        }
        // The codes the socket answers without a backup: the refusals of `parse` and the
        // one of the peer check.
        let mut codes: Vec<String> = backup_errors
            .iter()
            .map(|error| error.code().to_owned())
            .collect();
        for request in [
            "not json",
            "{\"v\":1,\"command\":\"restore\"}",
            "{\"command\":\"backup\",\"to\":\"/x\"}",
            "{\"v\":1,\"command\":\"backup\"}",
        ] {
            if let Err(AdminResponse::Failed { code, .. }) = parse(request.as_bytes()) {
                codes.push(code);
            }
        }
        codes.push("forbidden".to_owned());
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes,
            [
                "bad_request",
                "busy",
                "flush_failed",
                "forbidden",
                "io",
                "meta_copy_failed",
                "meta_only_needs_s3",
                "objects_failed",
                "snapshot_failed",
                "target_inside_data_dir",
                "target_not_empty",
                "unknown_command",
                "unsupported_protocol",
            ],
            "admin protocol error codes are stable (D-R-36): add new codes, never rename one"
        );
    }

    #[tokio::test]
    async fn a_stale_socket_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        // Dropping the listener leaves the socket file behind, as a crash would.
        drop(StdUnixListener::bind(data.join(ADMIN_SOCKET)).unwrap());
        let running = bind(config(&data), None).await.unwrap();
        let response = ask(&data.join(ADMIN_SOCKET), "not json\n").await;
        assert_eq!(refusal_code(&response), "bad_request");
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_too_long_socket_path_does_not_stop_the_server() {
        let dir = tempfile::tempdir().unwrap();
        // The socket path is over the 108-byte limit of a Unix socket address.
        let data = dir.path().join("d".repeat(90));
        let running = bind(config(&data), None).await.unwrap();
        assert!(!data.join(ADMIN_SOCKET).exists());
        assert!(!data.join(ADMIN_SOCKET_TMP).exists());
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_removes_the_socket_before_draining() {
        let dir = tempfile::tempdir().unwrap();
        let running = bind(config(dir.path()), None).await.unwrap();
        let socket = dir.path().join(ADMIN_SOCKET);
        assert!(socket.exists());
        running.shutdown().await.unwrap();
        // Checked after shutdown returns, which is the latest point it must hold.
        assert!(!socket.exists());
        assert!(UnixStream::connect(&socket).await.is_err());
    }

    #[test]
    fn the_server_uid_and_root_are_allowed_and_no_other_uid_is() {
        // The server's own uid, even when the data directory belongs to someone else
        // (root on a PVC): the data directory's owner is not consulted.
        assert!(allowed(65532, 65532));
        assert!(allowed(0, 65532));
        assert!(!allowed(1000, 65532));
        // The server does not run as root, so a root peer is only ever an extra allow.
        assert!(allowed(0, 1000));
        assert!(!allowed(65532, 1000));
    }
}
