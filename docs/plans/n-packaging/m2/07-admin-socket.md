# B7 — The control socket

Read [00-design.md](00-design.md) first, especially § Design › Trigger (D-N-17).

**Goal:** `llmobs serve` listens on `<data_dir>/admin.sock` and answers `backup` requests (B6), one
JSON line in and one JSON line out per connection.

**Edit only:** `crates/server/src/admin_socket.rs` (new), `crates/server/src/lib.rs`,
`crates/server/src/serve.rs`, and the root `Cargo.toml`: add `"net"` to the workspace `tokio`
features. `UnixListener` and `UnixStream` need it; today it is only on through feature unification.

**Commit:** `Serve the admin control socket`

**Needs:** B6.

## Read first

- `crates/server/src/serve.rs`: `bind` (where `tasks` are pushed), `Running::shutdown`, and the
  tests that start and stop a server.
- B6's `backup::run`, `BackupReport`, `BackupError::code`.

## Protocol

- Path: `<data_dir>/admin.sock`. `pub const ADMIN_SOCKET: &str = "admin.sock";` in
  `admin_socket.rs`. Declare the module `pub mod admin_socket;` in `lib.rs`, so the CLI can use the
  constant and the wire types; `spawn` stays `pub(crate)`. Remove B6's
  `#![cfg_attr(not(test), allow(dead_code))]` from `backup.rs`: `run` is called now.
- **Request:** one line of UTF-8 JSON, at most 64 KiB, ending in `\n`:
  `{"command":"backup","to":"<path>","meta_only":<bool>}`.
- **Response:** one JSON line, then the server closes the connection:
  - `{"ok":true,"report":<BackupReport>}`;
  - `{"ok":false,"code":"<code>","message":"<message>"}`.
- Parsing goes in two steps:
  1. Parse the line as `serde_json::Value`. Input that is not JSON, or longer than 64 KiB, gives
     `bad_request`.
  2. If `value["command"]` is a string other than `"backup"`, answer `unknown_command`. Otherwise
     deserialize `AdminRequest` from the value; a failure (e.g. a missing `to`) gives
     `bad_request`.

Define the wire types in `admin_socket.rs`, `pub` with `Serialize` and `Deserialize`, so the CLI (B8)
uses the same types:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum AdminRequest {
    Backup { to: PathBuf, #[serde(default)] meta_only: bool },  // calls backup::run(state, &to, meta_only)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum AdminResponse {
    Done { ok: bool, report: BackupReport },        // ok = true
    Failed { ok: bool, code: String, message: String }, // ok = false
}
```

## Changes

### `admin_socket.rs`

```rust
/// Binds `<data_dir>/admin.sock` (removing a stale file first; `LOCK` is held)
/// with mode 0600 and serves requests until the task is aborted.
pub(crate) fn spawn(state: AppState, data_dir: &Path) -> Option<JoinHandle<()>>
```

- If binding fails, for example because the path is longer than the 108-byte Unix socket limit, log
  a `warn` with the error (no path) and return `None`. The server still starts; the backup command
  then reports that no socket answers.
- Bind `admin.tmp` (no longer than `admin.sock`, so a path that fits one fits the other) (removing a leftover one first), `set_permissions(0o600)` on it, then
  `std::fs::rename` it to `admin.sock`. The socket never exists under its real name with a wider
  mode.
- **Authentication:** for each accepted connection, read `stream.peer_cred()?.uid()` and compare
  it with the server's own uid, read once at bind time as the owner of the socket file it just
  created (`std::fs::metadata(socket)?.uid()`, `std::os::unix::fs::MetadataExt`; no `unsafe`
  `geteuid`). Accept that uid or uid 0. Not the data directory's owner: on a Kubernetes PVC it is
  root, while the server and `kubectl exec` run as 65532 (review N-1). Any other caller gets `{"ok":false,"code":"forbidden",…}` and the connection is
  closed. macOS ignores socket file permissions on `connect`, so this check, not the mode, is the
  authentication.
- **Connections in a `JoinSet`:** the accept loop owns a `tokio::task::JoinSet`. Each connection is
  `join_set.spawn(...)`, and finished ones are reaped with `try_join_next` in the loop. Aborting the
  accept task drops the `JoinSet`, which aborts every connection and the backup it runs; the backup
  then releases the snapshot and the gate.
- The accept loop spawns one task per connection. Each task:
  1. Reads one line with a `tokio::io::BufReader` limited to 64 KiB (`take(65 * 1024)`, then check
     the length), within a 10 s `tokio::time::timeout`. On timeout, close the connection and send
     nothing.
  2. Parses `AdminRequest`.
  3. Runs `backup::run(&state, &to, meta_only)`.
  4. Writes the response line and shuts the stream down.

  A backup can take minutes, so put no timeout on the request.

### `serve.rs`

- In `bind`, right where the `tasks` vector is built (after every listener has bound, so a later bind
  failure cannot leak the accept task), call `admin_socket::spawn` and push the handle when it is
  `Some`.
- At the **start** of `Running::shutdown`, before the HTTP drain, abort the socket task and remove
  `<data_dir>/admin.sock` (ignore `NotFound`). This cancels a backup in progress, before storage
  shuts down, and refuses new ones during the drain. A cancelled backup leaves its target without
  `manifest.json`. `Drop for Running` does not remove the file; the stale-file removal at the next
  bind covers it.
- The CLI decides on the response variant (`Done` or `Failed`), never on the `ok` field.

## Tests (`admin_socket.rs`)

Use `serve::bind` with a temp data dir under `std::env::temp_dir()`; keep paths short.

- `the_socket_exists_with_mode_0600_and_is_removed_at_shutdown`.
- `a_backup_request_returns_the_report`: connect with `tokio::net::UnixStream`, send a `backup`
  request to a new temp dir, read the line, and parse `AdminResponse::Done` with `ok == true` and
  `report.mode == Full`. The backup's `manifest.json` exists.
- `bad_input_gets_bad_request`: send `not json\n`, and get `ok: false, code: "bad_request"`.
- `an_unknown_command_is_refused`: send `{"command":"restore"}\n`, and get `unknown_command`.
- `a_stale_socket_file_is_replaced`: bind and drop a real `std::os::unix::net::UnixListener` at
  `admin.sock`, which leaves a stale socket file, then `bind`. The server starts and the socket
  answers.
- `a_too_long_socket_path_does_not_stop_the_server`: a data dir whose `admin.sock` path exceeds
  108 bytes. `bind` succeeds and no socket file exists.
- `shutdown_removes_the_socket_before_draining`: call `Running::shutdown`. The socket file is gone,
  and a connect attempt is refused, before `shutdown` returns. Check it from a task that polls the
  path while shutdown runs; at the latest it must be gone when `shutdown` returns.

## Verify

```bash
cargo test -p llmobs-server --all-features admin_socket
just check
```

## Done when

- [ ] `llmobs serve` serves `<data_dir>/admin.sock` with mode 0600 and removes it at shutdown.
- [ ] The seven tests pass, and `just check` passes. One commit: `Serve the admin control socket`.
