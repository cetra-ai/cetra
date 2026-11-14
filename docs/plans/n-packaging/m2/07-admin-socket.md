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
- B6's `backup::run`, `BackupRequest`, `BackupReport`, `BackupError::code`.

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
    Backup { to: PathBuf, #[serde(default)] meta_only: bool },
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
- After binding, `std::fs::set_permissions(path, Permissions::from_mode(0o600))`.
- The accept loop spawns one task per connection. Each task:
  1. Reads one line with a `tokio::io::BufReader` limited to 64 KiB (`take(65 * 1024)`, then check
     the length).
  2. Parses `AdminRequest`.
  3. Runs `backup::run(&state, BackupRequest { to, meta_only })`.
  4. Writes the response line and shuts the stream down.

  A backup can take minutes, so put no timeout on the request.

### `serve.rs`

- In `bind`, right where the `tasks` vector is built (after every listener has bound, so a later bind
  failure cannot leak the accept task), call `admin_socket::spawn` and push the handle when it is
  `Some`.
- In `Running::shutdown`, after the HTTP drain and before the storage shutdown, remove
  `<data_dir>/admin.sock`, ignoring `NotFound`. A backup in progress is cancelled with its task;
  its target is left without `manifest.json`, so it is visibly incomplete.

## Tests (`admin_socket.rs`)

Use `serve::bind` with a temp data dir under `std::env::temp_dir()`; keep paths short.

- `the_socket_exists_with_mode_0600_and_is_removed_at_shutdown`.
- `a_backup_request_returns_the_report`: connect with `tokio::net::UnixStream`, send a `backup`
  request to a new temp dir, read the line, and parse `AdminResponse::Done` with `ok == true` and
  `report.mode == Full`. The backup's `manifest.json` exists.
- `bad_input_gets_bad_request`: send `not json\n`, and get `ok: false, code: "bad_request"`.
- `an_unknown_command_is_refused`: send `{"command":"restore"}\n`, and get `unknown_command`.
- `a_stale_socket_file_is_replaced`: create a regular file at `admin.sock` before `bind`. The server
  starts and the socket answers.

## Verify

```bash
cargo test -p llmobs-server --all-features admin_socket
just check
```

## Done when

- [ ] `llmobs serve` serves `<data_dir>/admin.sock` with mode 0600 and removes it at shutdown.
- [ ] The five tests pass, and `just check` passes. One commit: `Serve the admin control socket`.
