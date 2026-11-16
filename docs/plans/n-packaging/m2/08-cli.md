# B8 — `cetra admin backup` and `cetra admin gc-orphans`

Read [00-design.md](00-design.md) first, especially § Design › Trigger and § Design › Restore
(D-N-17, D-N-21).

**Also (cross-track review X15):** `cetra admin migrate [--config <file>]` requires `catalog.db`
(`AdminError::NoCatalog`) and `meta.db` (`AdminError::NoMetaDb`), then opens the storage with
`Maintenance::open` **first** (the server must be stopped: a held `LOCK` maps to
`AdminError::Locked` before `meta.db` is touched), then `meta.db` with `MetaDb::open`. That runs both
databases' migrations (and B9's pre-migration copies); it then closes both and exits, printing `meta.db and catalog.db are at this version's
schema`. A restore of an older backup runs it before `reapply-deletions`, so the server never starts
on restored data before the deletions are re-applied. Test `migrate_on_a_stopped_server_succeeds` and
`migrate_while_the_server_runs_is_refused`; spec 09 § CLI gets the row `| \`cetra admin migrate\` |
with the server stopped: run the migrations of \`meta.db\` and \`catalog.db\`, then exit (restores of
older backups, D-N-22) |`.

**Goal:**
- `cetra admin backup --to <dir> [--meta-only] [--config <file>]` asks the running server, through
  `<data_dir>/admin.sock`, for a backup, and prints the report.
- `cetra admin gc-orphans [--min-age 1h] [--dry-run] [--config <file>]` opens the storage (the server
  must be stopped), runs `collect_orphans`, prints the counts, and exits with status 1 when objects
  are missing.

**Edit only:** `crates/cli/src/main.rs`, `crates/cli/src/test_server.rs` (two accessors),
`crates/server/src/admin.rs` (a `gc_orphans` function, a report struct, the `Locked` text and the
module doc), `docs/spec/09-api.md` (§ CLI rows).

**Commit:** `Add the backup and gc-orphans admin commands`

**Needs:** B5, B7.

## Read first

- `crates/cli/src/main.rs`: `AdminCommand`, `admin_command`, `load_config`, and the existing parse
  tests (`rotate_encryption_key_parses`).
- `crates/server/src/admin.rs`: how `list_users` opens the data directory and returns an `AdminError`
  (what it does when the server holds `LOCK`).
- B7's `ADMIN_SOCKET`, `AdminRequest`, `AdminResponse`; B5's `Storage::collect_orphans`, `OrphanReport`.

## Changes

### `main.rs`, `AdminCommand`

```rust
/// Back up the running server into DIR (online; the server writes DIR).
Backup {
    /// Target directory on the server's filesystem: absent or empty, outside the data directory.
    #[arg(long)]
    to: PathBuf,
    /// Metadata only (S3-compatible object storage; objects stay in the bucket).
    #[arg(long)]
    meta_only: bool,
    /// Config file. Overrides `CETRA_CONFIG`.
    #[arg(long)]
    config: Option<PathBuf>,
},
/// Delete objects the catalog does not know and report the ones it misses. Stop the server first.
GcOrphans {
    /// Leave unknown objects younger than this (e.g. `1h`, `30m`).
    #[arg(long, default_value = "1h", value_parser = humantime_duration)]
    min_age: Duration,
    /// Report only; delete nothing.
    #[arg(long)]
    dry_run: bool,
    /// Required in S3 mode without `--dry-run`: deleting orphans there rolls the live bucket back.
    #[arg(long)]
    rollback_bucket: bool,
    /// Config file. Overrides `CETRA_CONFIG`.
    #[arg(long)]
    config: Option<PathBuf>,
},
```

`humantime_duration` is a small hand-written parser. The workspace has only `humantime-serde`, not
`humantime`, and a flag does not justify a new dependency. It accepts `<n>s`, `<n>m`, `<n>h` and
`<n>d` and has its own unit test. That the config value (`humantime-serde`) accepts richer forms is
intended.

### `admin_command`

**`Backup`:**
1. `load_config`. Resolve `to` to an absolute path against the current directory: the server
   process has another working directory.
2. Connect a `tokio::net::UnixStream` to `config.data_dir.join(ADMIN_SOCKET)`. On `NotFound` or
   `ConnectionRefused`, bail with `no running cetra serve answers on <socket>; start it, or for a
   stopped server copy the data directory (docs/self-hosting/backup.md)`.
3. Write `serde_json::to_string(&AdminRequest::Backup { to, meta_only })` and `\n`. Read one line and
   parse an `AdminResponse`.
4. On `Done`, print:
   ```text
   backup written to <dir>
   mode: full
   objects: 812 (3.0 GiB; 812 linked, 0 copied)
   took: 4.2 s
   ```
   For `meta-only`, the objects line reads `objects: 812 (3.0 GiB; in the bucket)`.
5. On `Failed`, bail with `backup failed (<code>): <message>`.

**`GcOrphans`:** call `cetra_server::admin::gc_orphans(&config, min_age, dry_run)`. That function
opens `Maintenance::open(config.storage_config())`.
- In S3 mode, without `--dry-run`, `--rollback-bucket` must be given, else a new
  `AdminError::RollbackNeedsConfirmation` (`in S3 mode this deletes every object written after the
  backup from the live bucket; run with --dry-run first, then add --rollback-bucket`).
- First, `<data_dir>/catalog.db` must exist, else a new
  `AdminError::NoCatalog` (`no catalog.db in the data directory: restore it before running
  gc-orphans`). `Maintenance::open` would otherwise create an empty catalog, and every object would
  then look like an orphan.
- `OpenError::Locked { .. }` maps to the existing `AdminError::Locked`. Its message becomes `the
  server is running: stop it first`; update the rotate-key tests that assert the old text.
- Any other `OpenError` maps to a new `AdminError::Storage(cetra_storage::error::OpenError)`.
- It returns a server-side `pub struct GcOrphansReport { orphans, orphan_bytes, recent_unknown,
  missing, missing_media, dry_run }`, copied from `OrphanReport`. The CLI does not depend on
  `cetra-storage`.

It then runs `collect_orphans`, calls `storage.shutdown().await`, and returns the `OrphanReport`.
Update `admin.rs`'s module doc: it says these commands work while the server runs, which is no
longer true of `gc_orphans`. Print:

```text
orphans deleted: 3 (12.5 MiB)        # "orphans found" with --dry-run
recent unknown objects kept: 0
missing objects: 0
```

Also print `missing media: N (attachments only; traces are intact)` when `missing_media > 0`; that
alone does not fail the command. When `missing > 0`, also print `the catalog needs objects the store does not have: the restore is
incomplete (backup older than gc.grace, or objects/ not copied)`, and exit with status 1 (return an
`Err`).

### `docs/spec/09-api.md` § CLI

Replace the row that lists `cetra admin compact … · wal-inspect · gc-orphans` with:

```markdown
| `cetra admin backup --to <dir> [--meta-only]` | online backup of the running server through `<data_dir>/admin.sock`; the server writes `<dir>` (N M2, D-N-17–D-N-19) |
| `cetra admin gc-orphans [--min-age 1h] [--dry-run]` | with the server stopped: deletes objects the catalog does not know (older than `--min-age`) and reports objects it needs but the store lacks; exit 1 when some are missing (D-N-21) |
| `cetra admin compact --project … --day …` · `cetra admin wal-inspect` | maintenance |
```

## Tests (`main.rs` test module)

- `backup_parses`: `["cetra","admin","backup","--to","/b"]` gives `meta_only == false`, and
  `--meta-only` sets it.
- `gc_orphans_parses_defaults`: `min_age` is 1 h by default, `--min-age 30m` gives 30 min, and
  `--dry-run` is parsed.
- `backup_without_a_server_names_the_socket`: with a temp data dir and no server, `run_cli` returns an
  error whose text contains `admin.sock` and `docs/self-hosting/backup.md`.
- `backup_through_a_running_server`: `TestServer` keeps its config private, so start a server the
  way `TestServer::start` does internally: write a`cetra.toml` in a short `tempfile::tempdir()`
  (the socket path must stay under 108 bytes), then `serve::bind(config, None)`. Run
  `run_cli(["cetra","admin","backup","--to",<tmp>/backup,"--config",<toml>])`, and get `Ok`;
  `<tmp>/backup/manifest.json` exists. Set `otlp_http_addr` and `grpc_addr` to `""` in the toml, as
  `TestServer` does.
- `gc_orphans_refuses_a_data_dir_without_a_catalog`: on an empty temp data dir, it returns `Err`
  containing `no catalog.db`.
- `gc_orphans_on_a_stopped_server_reports_zero_missing`: create a data dir by starting and stopping a
  server, then `run_cli([... "gc-orphans", "--config", ...])` is `Ok`.
- `gc_orphans_fails_when_objects_are_missing`:
  1. Start a server with `TestServer::start` (it ingests a trace). Add `pub(crate) fn data_dir()` and
     `pub(crate) fn config_path()` accessors to `TestServer`. Wait for a flush: with B2 merged,
     call `running.state().storage.flush_now`, or poll until `objects/tables` holds a file.
  2. Stop the server.
  3. Delete one Parquet file under `<data_dir>/objects/tables/`.
  4. `run_cli(gc-orphans)` returns `Err`, and the text mentions the incomplete restore.
- `backup_meta_only_in_local_mode_is_refused`: through a running local-mode server,
  `--meta-only` gives `Err` containing `meta_only_needs_s3`.

## Verify

```bash
cargo test -p cetra-cli --all-features backup
cargo test -p cetra-cli --all-features gc_orphans
just check
```

Then by hand with a release build, as § How to verify in `00-design.md` describes (start, ingest,
`admin backup`, stop, restore, `gc-orphans`, start, read back). Report the outputs.

## Done when

- [ ] Both commands exist with the seven tests, and spec 09 lists them.
- [ ] The manual run passes.
- [ ] `just check` passes. One commit: `Add the backup and gc-orphans admin commands`.
