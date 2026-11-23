//! `cetra debug-bundle` (track TL § 4, D-TL-6): one local JSON file for a bug report. Build and host facts, the configuration by allow-list, and the running server's `debug_info` over `admin.sock`, or the data directory read from disk when no server answers. The command contacts no network. Log lines and shown config strings go through the scrubber. The file is at most 8 MiB, mode 0600, and never overwrites.

use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use super::config_view::{ConfigErrorView, error_view, read_error_view, redacted_values};
use super::debug_info::DebugInfo;
use super::disk::{DiskFacts, disk_facts, lock_held};
use super::facts::{self, Profile};
use super::host::{HostFacts, host_facts};
use super::scrub::scrub_text;
use crate::admin_socket::{
    ADMIN_PROTOCOL, ADMIN_SOCKET, AdminEnvelope, AdminRequest, AdminResponse, MAX_RESPONSE,
    decode_response,
};
use crate::config;

/// The bundle's schema version.
pub(crate) const BUNDLE_SCHEMA: u32 = 1;
/// The largest bundle, in bytes.
pub(crate) const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
/// What the CLI prints after writing the bundle.
pub const REVIEW_NOTICE: &str = "review it before you share it: it holds your configuration (secret values replaced by <redacted>), host facts and recent log lines";

/// The typed bundle. It is written through `document` (TL3.8): serialized to a
/// `serde_json::Value`, each log line that is a JSON object embedded as that object, every
/// string and object key scrubbed, then size-checked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Bundle {
    /// [`BUNDLE_SCHEMA`].
    pub(crate) schema: u32,
    /// RFC 3339, UTC, seconds.
    pub(crate) created_at: String,
    /// The build this CLI was made from.
    pub(crate) build: BuildFacts,
    /// The host this command runs on.
    pub(crate) host: HostFacts,
    /// The configuration by allow-list.
    pub(crate) config: ConfigView,
    /// `None` when the configuration loads.
    pub(crate) config_error: Option<ConfigErrorView>,
    /// The running server, or the data directory read from disk.
    pub(crate) server: ServerSection,
}

/// What the build is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct BuildFacts {
    /// `facts::VERSION`.
    pub(crate) version: &'static str,
    /// `facts::TARGET`.
    pub(crate) target: &'static str,
    /// `facts::profile()`.
    pub(crate) profile: Profile,
    /// Whether the web UI is embedded in this build.
    pub(crate) ui_embedded: bool,
}

/// The configuration as the bundle shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ConfigView {
    /// The config file read (`--config`, else `CETRA_CONFIG`), as given.
    pub(crate) file: Option<String>,
    /// Names of the `CETRA_*` variables set, sorted. Never their values.
    pub(crate) env_names: Vec<String>,
    /// The merged file and environment values, redacted by the allow-list.
    pub(crate) values: Value,
}

/// The server section of the bundle.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ServerSection {
    /// A server answered `debug_info`.
    Running {
        /// The server's answer.
        debug_info: Box<DebugInfo>,
    },
    /// No server answered on `admin.sock` (absent, or refused the connection) and
    /// `<data_dir>/LOCK` is not held.
    NotRunning {
        /// The data directory, as the configuration names it.
        data_dir: String,
        /// What the data directory holds, read from disk.
        disk: DiskFacts,
        /// Advice on what to attach.
        hint: &'static str,
    },
    /// `admin.sock` could not be used, or `LOCK` is held while the socket does not answer:
    /// a static reason. Nothing in the data directory is read.
    Unreachable {
        /// The data directory, as the configuration names it.
        data_dir: String,
        /// A static reason.
        reason: String,
    },
    /// The server answered with an error (for example `forbidden`, `unknown_command`).
    Refused {
        /// The stable error code.
        code: String,
        /// The server's message.
        message: String,
    },
}

/// How long the bundle waits to connect to `admin.sock`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long it waits for the `debug_info` answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// The `not_running` section's advice.
const NOT_RUNNING_HINT: &str = "no cetra serve answered on <data_dir>/admin.sock and none holds <data_dir>/LOCK. If the server runs, this data directory is not its own: pass --config, or set CETRA_CONFIG or CETRA_DATA_DIR, as the server's environment does (a relative data_dir is read from the current directory). If the server does not start, attach its last log lines too: docker compose logs --tail 500 cetra, kubectl logs <pod> --tail 500, or journalctl -u <unit> -n 500";
/// The `unreachable` reason when `LOCK` is held but the socket does not answer.
const LOCK_HELD_REASON: &str = "the data directory is in use but admin.sock does not answer: the server is starting, or its data directory path is too long for a Unix socket (about 100 bytes)";

/// Why the bundle was not written.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// `serde_json` refused the bundle.
    #[error("the bundle could not be encoded as JSON")]
    Encode,
    /// Over [`MAX_BUNDLE_BYTES`] even without log lines.
    #[error("the bundle is larger than 8 MiB without its log lines")]
    TooLarge,
    /// The output path exists: the bundle never overwrites.
    #[error("{} already exists; choose another --output", .0.display())]
    Exists(PathBuf),
    /// The file could not be written.
    #[error("writing {}", path.display())]
    Write {
        /// The output path.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: io::Error,
    },
}

/// The file name of a bundle written at `now`: `cetra-debug-<UTC time>.json`.
pub fn file_name(now: DateTime<Utc>) -> String {
    format!("cetra-debug-{}.json", now.format("%Y%m%dT%H%M%SZ"))
}

/// Writes `bytes` to a new file at `path` with mode 0600. An existing file is never
/// overwritten.
pub fn write_new(path: &Path, bytes: &[u8]) -> Result<(), BundleError> {
    let write_error = |source| BundleError::Write {
        path: path.to_path_buf(),
        source,
    };
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(BundleError::Exists(path.to_path_buf()));
        }
        Err(source) => return Err(write_error(source)),
    };
    file.write_all(bytes).map_err(write_error)?;
    file.sync_all().map_err(write_error)
}

/// The process environment for [`render`]: every pair whose name and value are UTF-8 (a
/// non-UTF-8 variable is skipped, so it never makes this command panic).
pub fn process_env() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// The bundle as JSON bytes. Reads the configuration as `cetra serve` does: `config_path`,
/// else `CETRA_CONFIG`, then the `CETRA_*` variables of `env` (the CLI passes [`process_env`];
/// tests pass their own, so the developer's shell never changes a test).
pub async fn render(
    config_path: Option<&Path>,
    env: &[(String, String)],
    ui_embedded: bool,
    now: DateTime<Utc>,
) -> Result<Vec<u8>, BundleError> {
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let bundle = build(config_path, &pairs, ui_embedded, now).await;
    let mut document = document(&bundle)?;
    encode(&mut document)
}

/// The bundle as it is assembled, before the document and size steps.
pub(crate) async fn build(
    config_path: Option<&Path>,
    env: &[(&str, &str)],
    ui_embedded: bool,
    now: DateTime<Utc>,
) -> Bundle {
    let (config, config_error, data_dir) = config_section(config_path, env);
    let server = server_section(&data_dir).await;
    Bundle {
        schema: BUNDLE_SCHEMA,
        created_at: now.to_rfc3339_opts(SecondsFormat::Secs, true),
        build: BuildFacts {
            version: facts::VERSION,
            target: facts::TARGET,
            profile: facts::profile(),
            ui_embedded,
        },
        host: host_facts(),
        config,
        config_error,
        server,
    }
}

/// The configuration view, its load error, and the data directory the server would use.
fn config_section(
    config_path: Option<&Path>,
    env: &[(&str, &str)],
) -> (ConfigView, Option<ConfigErrorView>, PathBuf) {
    let file = config::configured_path(config_path).map(|p| p.display().to_string());
    let mut env_names: Vec<String> = env
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| {
            name.get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case("CETRA_"))
        })
        .map(str::to_owned)
        .collect();
    env_names.sort();
    env_names.dedup();

    let mut error = None;
    let toml = match config::read_toml(config_path) {
        Ok(toml) => toml,
        Err(e) => {
            error = Some(read_error_view(&e));
            None
        }
    };
    let read_failed = error.is_some();
    let tree = match config::source_tree(toml.as_deref(), env) {
        Ok(tree) => Some(tree),
        Err(e) => {
            error.get_or_insert(error_view(&e));
            None
        }
    };
    let values = tree
        .as_ref()
        .map_or_else(|| Value::Object(Map::new()), redacted_values);
    let loaded = if read_failed {
        None
    } else {
        match config::load_from(toml.as_deref(), env) {
            Ok(config) => Some(config),
            Err(e) => {
                error.get_or_insert(error_view(&e));
                None
            }
        }
    };
    let data_dir = match loaded {
        Some(config) => config.data_dir.clone(),
        None => tree
            .as_ref()
            .and_then(|t| t.get("data_dir"))
            .and_then(|v| v.clone().into_string())
            .map(PathBuf::from)
            .unwrap_or_else(config::default_data_dir),
    };
    (
        ConfigView {
            file,
            env_names,
            values,
        },
        error,
        data_dir,
    )
}

/// The server section: the answer of `admin.sock`, or what the data directory tells.
async fn server_section(data_dir: &Path) -> ServerSection {
    let socket = data_dir.join(ADMIN_SOCKET);
    let shown = data_dir.display().to_string();
    let unreachable = |reason: &str| ServerSection::Unreachable {
        data_dir: shown.clone(),
        reason: reason.to_owned(),
    };
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&socket)).await {
        Err(_) => return unreachable("connecting timed out"),
        Ok(Err(error))
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::InvalidInput
            ) =>
        {
            // `InvalidInput` is a socket path too long for a Unix address: such a server runs
            // without its socket, so the lock decides.
            if lock_held(data_dir) {
                return unreachable(LOCK_HELD_REASON);
            }
            return ServerSection::NotRunning {
                data_dir: shown.clone(),
                disk: disk_facts(data_dir).await,
                hint: NOT_RUNNING_HINT,
            };
        }
        Ok(Err(error)) => return unreachable(&error.kind().to_string()),
        Ok(Ok(stream)) => stream,
    };
    let response = match tokio::time::timeout(ANSWER_TIMEOUT, ask_debug_info(stream)).await {
        Err(_) => return unreachable("the server did not answer within 30 s"),
        Ok(Err(reason)) => return unreachable(reason),
        Ok(Ok(response)) => response,
    };
    match response {
        AdminResponse::Info { debug_info, .. } => ServerSection::Running { debug_info },
        AdminResponse::Failed { code, message, .. } => ServerSection::Refused { code, message },
        AdminResponse::Done { .. } => unreachable("the server answered with a backup report"),
    }
}

/// Sends `debug_info` on `stream` and reads one answer line. The reason of a failure is static.
async fn ask_debug_info(mut stream: UnixStream) -> Result<AdminResponse, &'static str> {
    let mut request = serde_json::to_string(&AdminEnvelope {
        v: ADMIN_PROTOCOL,
        request: AdminRequest::DebugInfo,
    })
    .map_err(|_| "the request could not be encoded")?;
    request.push('\n');
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|_| "the request could not be sent")?;
    let mut line = Vec::new();
    BufReader::new(stream.take(MAX_RESPONSE as u64 + 1))
        .read_until(b'\n', &mut line)
        .await
        .map_err(|_| "the answer could not be read")?;
    if line.is_empty() {
        return Err("the server closed the connection without an answer");
    }
    if line.len() > MAX_RESPONSE {
        return Err("the answer is larger than 4 MiB");
    }
    decode_response(&String::from_utf8_lossy(&line))
        .map_err(|_| "the server's answer is not admin protocol 1")
}

/// The bundle as the JSON document that is written: each log line of a `running` server that
/// parses as a JSON object embedded as that object (other lines stay strings), then every
/// string and object key scrubbed (the second guard, track TL § 4).
pub(crate) fn document(bundle: &Bundle) -> Result<Value, BundleError> {
    let mut document = serde_json::to_value(bundle).map_err(|_| BundleError::Encode)?;
    if let Some(lines) = log_lines(&mut document) {
        for line in lines.iter_mut() {
            if let Some(text) = line.as_str()
                && let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(text)
            {
                *line = parsed;
            }
        }
    }
    scrub_value(&mut document);
    Ok(document)
}

/// `server.debug_info.logs.lines` of a `running` bundle.
fn log_lines(document: &mut Value) -> Option<&mut Vec<Value>> {
    document
        .pointer_mut("/server/debug_info/logs/lines")
        .and_then(Value::as_array_mut)
}

/// Every string of `value`, and every object key, through [`scrub_text`]. Two keys that
/// scrub to the same text keep the later value.
fn scrub_value(value: &mut Value) {
    match value {
        Value::String(text) => *text = scrub_text(text),
        Value::Array(items) => items.iter_mut().for_each(scrub_value),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut child) in entries {
                scrub_value(&mut child);
                map.insert(scrub_text(&key), child);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The document as pretty JSON with a final newline, at most [`MAX_BUNDLE_BYTES`]. The oldest
/// log lines go first, a tenth at a time, and the `trimmed` flag is set when any did.
fn encode(document: &mut Value) -> Result<Vec<u8>, BundleError> {
    loop {
        let mut bytes = serde_json::to_vec_pretty(document).map_err(|_| BundleError::Encode)?;
        bytes.push(b'\n');
        if bytes.len() <= MAX_BUNDLE_BYTES {
            return Ok(bytes);
        }
        let Some(lines) = log_lines(document).filter(|lines| !lines.is_empty()) else {
            return Err(BundleError::TooLarge);
        };
        let drop = (lines.len() / 10).max(1);
        lines.drain(..drop);
        if let Some(trimmed) = document.pointer_mut("/server/debug_info/logs/trimmed") {
            *trimmed = Value::Bool(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::observability::config_view::ConfigErrorKind;
    use crate::observability::debug_info::{self, LogsView};
    use crate::serve::bind;
    use crate::test_support;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 23, 10, 20, 30).unwrap()
    }

    fn toml_for(data: &Path) -> String {
        format!("data_dir = {:?}\n", data.display().to_string())
    }

    fn write_config(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("cetra.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A bundle for a data directory with no server (the config file names it).
    async fn not_running_bundle(dir: &Path) -> Bundle {
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let file = write_config(dir, &toml_for(&data));
        build(Some(&file), &[], false, now()).await
    }

    /// Every file under `root` with its size, sorted by path.
    fn listing(root: &Path) -> Vec<(String, u64)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let name = path.strip_prefix(root).unwrap().display().to_string();
                    out.push((name, entry.metadata().unwrap().len()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    fn running(bundle: &mut Bundle, info: DebugInfo) {
        bundle.server = ServerSection::Running {
            debug_info: Box::new(info),
        };
    }

    #[tokio::test]
    async fn a_running_server_is_asked_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let running_server = bind(test_support::config(&data), None).await.unwrap();
        let file = write_config(dir.path(), &toml_for(&data));
        let bundle = build(Some(&file), &[], true, now()).await;
        match &bundle.server {
            ServerSection::Running { debug_info } => {
                assert_eq!(debug_info.version.as_str(), facts::VERSION);
            }
            other => panic!("{other:?}"),
        }
        assert!(bundle.config_error.is_none());
        assert_eq!(bundle.config.values["data_dir"], data.display().to_string());
        assert!(bundle.build.ui_embedded);
        assert_eq!(bundle.created_at, "2026-11-23T10:20:30Z");
        running_server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_stopped_server_gets_the_not_running_section() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let running_server = bind(test_support::config(&data), None).await.unwrap();
        running_server.shutdown().await.unwrap();
        let file = write_config(dir.path(), &toml_for(&data));
        let bundle = build(Some(&file), &[], false, now()).await;
        match bundle.server {
            ServerSection::NotRunning { disk, hint, .. } => {
                assert_eq!(
                    disk.meta_db.format,
                    Some(i64::from(crate::meta::META_FORMAT_VERSION))
                );
                assert!(disk.catalog_db.readable);
                assert!(hint.contains("docker compose logs"));
                assert!(hint.contains("journalctl"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn the_not_running_bundle_writes_nothing_to_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let running_server = bind(test_support::config(&data), None).await.unwrap();
        running_server.shutdown().await.unwrap();
        let before = listing(&data);
        let file = write_config(dir.path(), &toml_for(&data));
        let _ = build(Some(&file), &[], false, now()).await;
        assert_eq!(listing(&data), before);
    }

    #[tokio::test]
    async fn a_held_lock_without_a_socket_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let running_server = bind(test_support::config(&data), None).await.unwrap();
        std::fs::remove_file(data.join(ADMIN_SOCKET)).unwrap();
        let file = write_config(dir.path(), &toml_for(&data));
        let bundle = build(Some(&file), &[], false, now()).await;
        match bundle.server {
            ServerSection::Unreachable { reason, .. } => assert_eq!(reason, LOCK_HELD_REASON),
            other => panic!("{other:?}"),
        }
        running_server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_long_data_dir_path_reaches_the_lock_probe() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().display().to_string().len();
        // Over the 108-byte limit of a Unix socket address, as in admin_socket's tests.
        let data = dir.path().join("d".repeat(110 - base));
        std::fs::create_dir_all(&data).unwrap();
        let file = write_config(dir.path(), &toml_for(&data));

        let bundle = build(Some(&file), &[], false, now()).await;
        match bundle.server {
            ServerSection::NotRunning { disk, .. } => assert!(disk.data_dir_present),
            other => panic!("{other:?}"),
        }

        let running_server = bind(test_support::config(&data), None).await.unwrap();
        let bundle = build(Some(&file), &[], false, now()).await;
        match bundle.server {
            ServerSection::Unreachable { reason, .. } => assert_eq!(reason, LOCK_HELD_REASON),
            other => panic!("{other:?}"),
        }
        running_server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_missing_data_dir_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("absent");
        let file = write_config(dir.path(), &toml_for(&data));
        let bundle = build(Some(&file), &[], false, now()).await;
        match bundle.server {
            ServerSection::NotRunning { disk, .. } => assert!(!disk.data_dir_present),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unusable_socket_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        // A regular file where the socket should be: the kernel refuses the connect with an
        // error kind that depends on the platform, so either section is accepted here.
        std::fs::write(data.join(ADMIN_SOCKET), b"x").unwrap();
        let file = write_config(dir.path(), &toml_for(&data));
        let bundle = build(Some(&file), &[], false, now()).await;
        assert!(matches!(
            bundle.server,
            ServerSection::Unreachable { .. } | ServerSection::NotRunning { .. }
        ));
    }

    #[tokio::test]
    async fn a_missing_config_file_is_reported() {
        let bundle = build(
            Some(Path::new("/nonexistent/cetra.toml")),
            &[],
            false,
            now(),
        )
        .await;
        assert_eq!(
            bundle.config_error,
            Some(ConfigErrorView {
                kind: ConfigErrorKind::MissingFile,
                key: None,
                reason: None,
            })
        );
        assert!(!bundle.host.os.is_empty());
        assert_eq!(bundle.config.values, json!({}));
    }

    #[tokio::test]
    async fn a_toml_syntax_error_still_gives_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_config(dir.path(), "encryption_key = \"PLANTED\n");
        let bundle = build(Some(&file), &[], false, now()).await;
        assert_eq!(
            bundle.config_error.as_ref().map(|e| e.kind),
            Some(ConfigErrorKind::TomlSyntax)
        );
        let bytes = encode(&mut document(&bundle).unwrap()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("PLANTED"));
    }

    #[tokio::test]
    async fn an_invalid_secret_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_config(dir.path(), "smtp_url = \"smtp://u:PLANTED@h:notaport\"\n");
        let bundle = build(Some(&file), &[], false, now()).await;
        assert_eq!(
            bundle.config_error.as_ref().and_then(|e| e.key.clone()),
            Some("smtp_url".to_owned())
        );
        assert_eq!(bundle.config.values["smtp_url"], "<redacted>");
        let bytes = encode(&mut document(&bundle).unwrap()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("PLANTED"));
    }

    #[tokio::test]
    async fn an_invalid_shown_key_names_the_key_without_its_error_text() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_config(dir.path(), "http_addr = \"PLANTED-nope\"\n");
        let bundle = build(Some(&file), &[], false, now()).await;
        assert_eq!(
            bundle.config_error.as_ref().and_then(|e| e.key.clone()),
            Some("http_addr".to_owned())
        );
        let text = serde_json::to_string(&bundle.config_error).unwrap();
        assert!(!text.contains("PLANTED"));
    }

    #[tokio::test]
    async fn env_names_are_listed_without_values() {
        let env = [
            ("CETRA_LOG", "debug"),
            ("cetra_encryption_key", "PLANTED-key"),
            ("HOME", "/x"),
        ];
        let bundle = build(None, &env, false, now()).await;
        assert_eq!(
            bundle.config.env_names,
            ["CETRA_LOG", "cetra_encryption_key"]
        );
        let text = serde_json::to_string(&bundle.config).unwrap();
        assert!(!text.contains("PLANTED"));
    }

    #[tokio::test]
    async fn document_embeds_json_log_lines_and_keeps_others() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = not_running_bundle(dir.path()).await;
        let mut info = debug_info::sample();
        info.logs = Some(LogsView {
            lines: vec![
                "{\"fields\":{\"message\":\"hi\"}}".to_owned(),
                "not json".to_owned(),
                "[1,2]".to_owned(),
            ],
            dropped: 0,
            trimmed: false,
        });
        running(&mut bundle, info);
        let doc = document(&bundle).unwrap();
        assert_eq!(
            doc["server"]["debug_info"]["logs"]["lines"],
            json!([{"fields": {"message": "hi"}}, "not json", "[1,2]"])
        );
    }

    #[tokio::test]
    async fn document_scrubs_every_string_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = not_running_bundle(dir.path()).await;
        let mut info = debug_info::sample();
        info.logs = Some(LogsView {
            lines: vec!["{\"user\":\"ada@example.com\",\"k\":\"sk-cetra-abc\"}".to_owned()],
            dropped: 0,
            trimmed: false,
        });
        info.telemetry.endpoint = "https://u:p@relay.example".to_owned();
        info.ready.failed = vec!["carol@leak.example".to_owned()];
        running(&mut bundle, info);
        bundle.config.values = json!({"zz_ada@leak.example": "<redacted>"});
        bundle.config_error = Some(ConfigErrorView {
            kind: ConfigErrorKind::UnknownField,
            key: Some("x.bob@leak.example".to_owned()),
            reason: None,
        });
        let doc = document(&bundle).unwrap();
        let text = serde_json::to_string(&doc).unwrap();
        for needle in ["ada@", "leak.example", "sk-cetra-abc", "u:p@"] {
            assert!(!text.contains(needle), "{needle} leaked");
        }
        assert_eq!(
            doc["server"]["debug_info"]["logs"]["lines"][0]["user"],
            "<redacted>"
        );
    }

    #[tokio::test]
    async fn encode_trims_logs_to_fit() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = not_running_bundle(dir.path()).await;
        let mut info = debug_info::sample();
        info.logs = Some(LogsView {
            lines: (0..6000)
                .map(|i| format!("{i:06}{}", "a".repeat(2042)))
                .collect(),
            dropped: 0,
            trimmed: false,
        });
        running(&mut bundle, info);
        let mut doc = document(&bundle).unwrap();
        let bytes = encode(&mut doc).unwrap();
        assert!(bytes.len() <= MAX_BUNDLE_BYTES);
        assert_eq!(doc["server"]["debug_info"]["logs"]["trimmed"], true);
        let lines = doc["server"]["debug_info"]["logs"]["lines"]
            .as_array()
            .unwrap();
        assert!(
            lines
                .last()
                .and_then(Value::as_str)
                .is_some_and(|line| line.starts_with("005999"))
        );
    }

    #[tokio::test]
    async fn encode_fails_when_too_large_without_logs() {
        let dir = tempfile::tempdir().unwrap();
        let mut bundle = not_running_bundle(dir.path()).await;
        bundle.config.values = Value::Object(
            (0..3000)
                .map(|i| (format!("k{i:04}"), Value::String("x".repeat(4096))))
                .collect(),
        );
        let mut doc = document(&bundle).unwrap();
        assert!(matches!(encode(&mut doc), Err(BundleError::TooLarge)));
    }

    #[tokio::test]
    async fn the_bundle_keys_are_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = not_running_bundle(dir.path()).await;
        let text = serde_json::to_string(&bundle).unwrap();
        let mut at = 0;
        for key in [
            "schema",
            "created_at",
            "build",
            "host",
            "config",
            "config_error",
            "server",
        ] {
            let found = text[at..].find(&format!("\"{key}\":")).unwrap();
            at += found + key.len();
        }
        let doc = document(&bundle).unwrap();
        assert_eq!(doc["server"]["state"], "not_running");
        let mut build_keys: Vec<&str> = doc["build"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        build_keys.sort_unstable();
        assert_eq!(build_keys, ["profile", "target", "ui_embedded", "version"]);
    }

    #[test]
    fn server_states_serialize_snake_case() {
        let unreachable = ServerSection::Unreachable {
            data_dir: "/d".to_owned(),
            reason: "r".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&unreachable).unwrap(),
            json!({"state": "unreachable", "data_dir": "/d", "reason": "r"})
        );
        let refused = ServerSection::Refused {
            code: "forbidden".to_owned(),
            message: "m".to_owned(),
        };
        assert_eq!(serde_json::to_value(&refused).unwrap()["state"], "refused");
    }

    #[test]
    fn file_name_is_a_utc_timestamp() {
        assert_eq!(file_name(now()), "cetra-debug-20261123T102030Z.json");
    }

    #[test]
    fn write_new_makes_a_0600_file_and_never_overwrites() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.json");
        write_new(&path, b"one").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(matches!(
            write_new(&path, b"two"),
            Err(BundleError::Exists(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
    }

    #[test]
    fn write_new_names_the_path_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("bundle.json");
        match write_new(&path, b"x") {
            Err(BundleError::Write { path: written, .. }) => assert_eq!(written, path),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_bundle_modules_contact_no_network() {
        for source in [
            include_str!("bundle.rs"),
            include_str!("host.rs"),
            include_str!("config_view.rs"),
            include_str!("scrub.rs"),
        ] {
            let code = source.split("#[cfg(test)]").next().unwrap();
            for word in [
                "reqwest",
                "TcpStream",
                "UdpSocket",
                "lookup_host",
                "ToSocketAddrs",
            ] {
                assert!(!code.contains(word), "{word} in a bundle module");
            }
        }
    }
}
