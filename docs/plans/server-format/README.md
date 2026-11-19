Status: done (SF1–SF8), 2026-11-16

# Server format stability (D185, track R extension)

Track R's M8 (`docs/plans/r-hardening/08-m8-format.md`) makes the storage formats of the data directory stable before the first public release tag (D185, D-R-12, D-R-26 to D-R-32). It left the server's own durable files to a separate plan (its § Requests, "A server-track plan before the first tag"). This is that plan. It covers everything outside `crates/storage` that a release writes and a later release must read:

| Surface | Writer | Reader today |
|---|---|---|
| `<data_dir>/meta.db` (SQLite, migrations `crates/server/migrations/meta/0001`–`0015` on `main` 2f5181a9, more from track J) | `meta::MetaDb::open` (`cetra serve`, `cetra admin migrate`) | the server and every `cetra admin` command |
| `<data_dir>/secret.key`, `secret.key.new` (D-I-1, D-I-3), and the sealed column values (D-I-2) | `crypto::write_key_file`, `admin::rotate_encryption_key`, `SecretBox::seal` | `crypto::load_key`, `finish_rotation`, `SecretBox::open` |
| `<data_dir>/admin.sock` protocol (D-N-17) | `admin_socket` (server) | `cetra admin backup` (`crates/cli/src/main.rs`) |
| a backup directory: `manifest.json`, `meta.db`, `catalog.db`, `secret.key`, `objects/` (D-N-19) | `backup::run` | nothing reads `manifest.json`; a restore copies the files back by hand (`docs/self-hosting/backup.md` § Restore) |

The plan has 8 sub-phases, **SF1–SF8**, one commit each, done in order by one agent working alone. The implementer is Haiku: nothing here may need a guess. If this plan and the code disagree (a name moved, a function changed), follow the code when the change is a rename, and **STOP and report** otherwise. Each sub-phase ends with `just check` green. The decisions are **D-R-33 to D-R-38** (`docs/mvp-decisions.md` § Track R). The spec text is already in `docs/spec/04-metadata-db.md` § Format versions and `docs/spec/12-operations.md`, with "(SF<n>)" markers on what a sub-phase still has to make true; each sub-phase deletes its markers.

**Before the first public tag there is no format constraint** (D185, `AGENTS.md`). This plan adds markers, refusals, lints and golden fixtures; it changes no format that has a reader to keep.

## What exists today (checked on `main` 2f5181a9)

**`meta.db`.**
- `MetaDb::open` (`crates/server/src/meta/mod.rs` around lines 62-85): `connect(path, create = true)`, `applied_version` (the highest `_sqlx_migrations.version`), `pre_migration_copy` (D-N-22: `meta.db.pre-m<applied>` when `0 < applied < expected`), then `migrator().run`. A `meta.db` that has applied a migration this binary lacks fails inside `sqlx` with `MigrateError::VersionMissing`, surfaced as `MetaError::Migrate` ("meta.db migration: migration N was previously applied but is missing…"). Nothing is written before that error (no copy: `applied >= expected`), but the message does not say "newer release".
- **`cetra serve` opens storage first** (`serve.rs::bind` around line 258: `Storage::open`, then `MetaDb::open`). A `meta.db` from a newer release whose storage format did not change is therefore detected only **after** `Storage::open` has taken `LOCK`, replayed the WAL, started a new WAL segment and collected obsolete files. `cetra admin migrate` has the same order (`Maintenance::open`, then `MetaDb::open`, `admin.rs` around line 531), and `gc-orphans` never opens `meta.db` at all.
- `MetaDb::open_existing` (admin commands) refuses any other applied version with `SchemaMismatch { found, expected }`; `open_existing_or_newer` (`reapply-deletions --from`, D-M-9) accepts a newer one and reads it by column name, relying on migrations only adding.
- No `PRAGMA user_version`. Nothing marks a change that adds no migration: a new value in a TEXT enum column (`members.role`, `deletions.kind`, `eval_jobs.status`, …), a changed JSON shape in a JSON column (`saved_views`, `alert_rules.config`, `evaluators.config`, …) or a new sealed-value envelope. An older binary would open such a database and fail later, row by row, with `MetaError::Corrupt`, or read a JSON value wrongly.
- Migrations: all `CREATE TABLE` / `CREATE INDEX` / `ALTER TABLE … ADD COLUMN`, except **`0008_entitlements.sql`: `ALTER TABLE orgs DROP COLUMN limits`**. `0013` and `0015` add columns to existing tables. `meta::tests::schema_matches_the_snapshot` already pins the schema (`crates/server/tests/snapshots/meta_schema.snap`, insta).

**`secret.key` and sealed values** (spec 04 § Encryption of secrets, D-I-1 to D-I-3).
- The file is the standard base64 of 32 bytes plus `\n` (a trailing `\r\n` or none is accepted), mode 0600, written through `secret.key.tmp` and a rename. `CETRA_ENCRYPTION_KEY` holds the same text. Anything else is `CryptoError::BadKey`, and `load_key` writes nothing when a file exists.
- A sealed value is `0x01` ‖ key id (4) ‖ nonce (12) ‖ ciphertext and tag; key id and check value are HMAC-SHA256 with the labels `cetra.key-id.v1` and `cetra.encryption-check.v1`; the AAD is `table:column:row_id`. `open` answers `Corrupt` for any first byte but `0x01`.
- **Key rotation exists**: `cetra admin rotate-encryption-key` (offline, server stopped, D-I-3), re-encrypting every column of `crypto::ENCRYPTED_COLUMNS` in one transaction, with `secret.key.new` finished or removed at the next start (`finish_rotation`). No test pins any of these bytes against a fixed key.

**`admin.sock`** (`admin_socket.rs`). One JSON line each way. Request `{"command": "backup", "to": …, "meta_only": …}` (`AdminRequest`, internally tagged); response untagged `{"ok": true, "report": BackupReport}` or `{"ok": false, "code", "message"}`. `parse` answers `unknown_command` for another command and `bad_request` for a bad shape. **No version.** The client and the server are the same binary in a container (`exec`) but not after an in-place binary upgrade while the old server runs (`install.sh` replaces `/usr/local/bin/cetra`): a new client then talks to an old server.

**Backup manifest** (`backup.rs::manifest`, around lines 430-485): `format: 1`, `cetra_version`, `created_at`, `mode`, `object_store`, `encryption_key`, `flushed_seq`, `gc_grace_secs`, `restorable_until`, `files` (path, bytes, sha256), `objects`. Write-only: no code reads it. A restore of a backup from a newer release onto an older binary is caught only when `meta.db` or `catalog.db` is opened, i.e. after `gc-orphans` has already run (step 5 of `backup.md` § Restore, which in S3 mode **deletes objects from the live bucket**).

**Golden fixtures.** None for the server. M8's golden directories (`fixtures/storage/golden/`, D-R-29) cover the storage data directory only, and explicitly not `meta.db`, `manifest.json` or `admin.sock`.

**Competitors** (reference only: their checkouts are not on the machine this plan was written on). From `docs/research/competitors.md` and general knowledge: Langfuse (Prisma), Phoenix and MLflow (Alembic) and Opik (Liquibase) keep their metadata stores upgradable with forward-only migrations tracked in a migrations table, and refuse or warn on an unknown head revision (MLflow: "Detected out-of-date database schema"); none of them stamps a separate content-format number, and none has a golden database per release. They run on a server database the operator upgrades; we ship one binary that must also refuse a newer file cleanly, which is what D-R-33 adds.

## Design summary

- **D-R-33 `meta.db` format version.** `META_FORMAT_VERSION: u16 = 1` in `PRAGMA user_version`: the version of everything `meta.db` and its satellites mean (content meaning, `secret.key`, the sealed envelope). One check function, `meta::check_marker`, refuses a newer format (`FormatTooNew`), an unknown applied migration (`SchemaTooNew`) and a pre-release database with rows (`PreFormat`), before any write; a new or empty database is stamped. **`cetra serve`, `admin migrate` and `admin gc-orphans` run it as a preflight before storage opens**, so a newer `meta.db` stops the binary before `LOCK`, the WAL or the bucket is touched.
- **D-R-34 what changes the `meta.db` format; the migration lint.** As D-R-31 for `catalog.db`: a migration must only add, unless listed; every altering migration has a data case; the insta schema snapshot stays. Plus the content rules (enum values, JSON shapes, sealed envelope, key file) that bump `META_FORMAT_VERSION` without a migration.
- **D-R-35 `secret.key` and sealed values.** Frozen as they are: no version byte in the key file (its strict decoding is the refusal; a format change bumps `META_FORMAT_VERSION`, which refuses first). Key rotation stays D-I-3's offline command; no keyring. Known-answer tests pin the bytes.
- **D-R-36 `admin.sock` protocol.** Every request and response carries `"v": 1`. A server answers a request of another or no `v` with `unsupported_protocol`; the client refuses a response of another `v`. Within `v` 1 changes are additive; error codes are never renamed.
- **D-R-37 backup manifest.** `format` stays 1, additive keys only; it gains `meta_format`, `meta_schema`, `storage_format`, `catalog_schema`, read from the copies. No manifest reader in the MVP (user). **A backup from a newer release is refused** (user): by the preflight at the first restore command, before `gc-orphans` deletes anything.
- **D-R-38 golden server directories.** `fixtures/server/golden/<version>/`: an online backup of a scenario server (meta.db with a row in every table, `catalog.db`, `secret.key`, `manifest.json`), the socket lines that produced it, and `expected.json`. A test restores a copy with the current build by the documented procedure and compares rows, decoded reads, secrets and sign-ins. Same release rule as D-R-30 (one `RELEASES` list); nothing committed before the first stable tag.

## Conventions for every sub-phase

- **Read first:** `AGENTS.md`; this file, all of it; `docs/spec/04-metadata-db.md` § Format versions and § Encryption of secrets; `docs/spec/12-operations.md` § Backups and § Process lifecycle; D-R-26 to D-R-38 in `docs/mvp-decisions.md`; every file the sub-phase edits, before editing it.
- **`just check` passes before each commit.** Node is not on the non-interactive PATH: see `~/ENV.md` (or `ls ~/.nvm/versions/node/`). Build with `CARGO_BUILD_JOBS=6` on the shared VM.
- **Commit subjects** are given per sub-phase. One commit each. No attribution lines.
- **Errors** are `thiserror` variants. Messages name versions and file names, never row values, emails, keys or secrets (privacy rule). Use the wording given.
- **No new dependency.** `sqlx`, `serde_json`, `sha2`, `hex`, `tempfile`, `insta` are already dependencies or dev-dependencies of `cetra-server`; check `crates/server/Cargo.toml`.
- **No compatibility layers.** Checks are equalities until a second version exists, as in M8: the first bump turns them into a range and adds the reader.
- **CHANGELOG:** no line. Everything here is pre-release behaviour (D185).
- **Report at the end of each sub-phase:** what was done; deviations and why; the run time of each new test; sizes where asked; anything left undone or unverified.

### Who else edits these files (tracks J, N and T work in `crates/server` at the same time)

| File | Sub-phases | Shared with | How to keep the conflict small |
|---|---|---|---|
| `crates/server/src/meta/mod.rs` | SF1, SF2 (one `mod` line and one doc sentence) | J and T add `pub(crate) mod <store>;` lines | Keep edits inside `MetaDb::open`, `open_checked`, `MetaError`, new functions placed after `applied_version`; add the `mod` line in alphabetical order |
| `crates/server/src/serve.rs` | SF1 (3 lines in `bind`) | **N** (packaging, startup) | Touch only the lines just before `Storage::open` in `bind` |
| `crates/server/src/admin.rs` | SF1 (`gc_orphans`, `migrate`, tests) | M is done; N may touch `migrate` | Add one preflight call at the top of each function |
| `crates/server/src/routes/connections.rs` | SF1 (three arms in `meta_error_kind`) | J and T add routes elsewhere | Only the match |
| `crates/server/src/crypto.rs` | SF3 (tests only) | none | |
| `crates/server/src/admin_socket.rs`, `crates/cli/src/main.rs` (`backup` command only) | SF4 | N (CLI) | In `main.rs` touch only the body of the backup command's function |
| `crates/server/src/backup.rs` | SF5 | none | |
| `crates/server/src/golden_tests/` (new), `crates/server/src/lib.rs` (one `mod` line) | SF6, SF7 | J, T add `mod` lines to `lib.rs` | Alphabetical `mod` line |
| `crates/server/src/meta/migration_tests.rs` (new) | SF2 | **J adds migrations `0016`+** | J's migrations must pass the lint (§ Requests) |
| `fixtures/server/golden/` (new), `.gitattributes`, `justfile` | SF6, SF7 | R's M8 adds `golden-dir` and `check-golden` | SF6 starts after R8.8 has merged |
| `docs/spec/04-metadata-db.md`, `docs/spec/12-operations.md`, `docs/self-hosting/backup.md`, `docs/self-hosting/upgrade.md`, `AGENTS.md` | SF4, SF5, SF8 | R8.8 edits spec 12 § Upgrades and `AGENTS.md` | Keep both sides on a rebase |

### Order and prerequisites

- SF1–SF5 depend only on `main`. SF1 first; SF2–SF5 after it, in order.
- SF5's `storage_format` reads the backup's `catalog.db` `user_version`. It is `0` until R8.2 (M8) has merged and `FORMAT_VERSION` after it; SF5's tests compare with the live catalog's value, so they hold either way.
- **SF6 and SF7 start after R8.8 has merged** (`fixtures/storage/golden/RELEASES`, `just golden-dir`, `just check-golden`). SF6 checks: `test -f fixtures/storage/golden/RELEASES && grep -n "^golden-dir" justfile && grep -n "^check-golden" justfile` prints three things; otherwise STOP and report.
- SF8 last.

### Names used throughout

- **`META_FORMAT_VERSION`**: `crate::meta::META_FORMAT_VERSION: u16 = 1` (SF1).
- **The golden key** `GOLDEN_KEY: [u8; 32]` = the bytes `0, 1, 2, …, 31`. Its key file text is `AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=\n`, its key id `c2f47f2a`, its check value `212e59571b77a8a332ea48a6e4c994fe54e3113cd5665185f707710faf865eb0` (computed independently with Python's `hmac`; SF3's test is the check). It is a public test key: it seals only fixture values.
- **`dir_digest(dir)`**: a test helper (SF1, in `meta/mod.rs` `#[cfg(test)] pub(crate) mod test_digest`) returning a sorted `Vec<(relative path, sha256 hex)>` of every regular file under `dir`, ignoring `meta.db-wal`, `meta.db-shm`, `catalog.db-wal`, `catalog.db-shm` and `LOCK`. M8 has the same helper inside `crates/storage`; the crates cannot share it.

---

## SF1 — The `meta.db` format marker, refusals and the preflight

Commit: `server: refuse a newer meta.db before anything opens, stamp new ones (SF1)`

**Edit:** `crates/server/src/meta/mod.rs`, `crates/server/src/serve.rs` (`bind` only), `crates/server/src/admin.rs` (`gc_orphans`, `migrate`, `map_meta`, tests), `crates/server/src/routes/connections.rs` (`meta_error_kind` only), `docs/spec/04-metadata-db.md` (markers), `docs/spec/12-operations.md` (markers).

### Steps

1. **Constant**, in `meta/mod.rs` after `BUSY_TIMEOUT`:

   ```rust
   /// The format version of `meta.db` and of what it seals (spec 04 § Format versions, D-R-33).
   ///
   /// Stored in `PRAGMA user_version`. It changes when a release writes something an older
   /// release would misread without a migration telling it so: a new value in a TEXT enum
   /// column, a changed JSON shape, a non-additive migration, a new sealed envelope or key
   /// file format (D-R-34). Today a reader accepts exactly this version.
   pub(crate) const META_FORMAT_VERSION: u16 = 1;
   ```

2. **Three `MetaError` variants**, after `SchemaMismatch`, with doc comments on each field like their neighbours:

   ```rust
   /// `meta.db` was written by a newer format (spec 04 § Format versions).
   #[error("meta.db has format version {found}, which this build (format {supported}) does not read; run the newer release, or restore the backup taken before the upgrade")]
   FormatTooNew { found: i64, supported: u16 },
   /// `meta.db` has applied a migration this build does not have.
   #[error("meta.db has applied migration {version}, which this build does not have; run the newer release, or restore the backup taken before the upgrade")]
   SchemaTooNew { version: i64 },
   /// A `meta.db` with data from before format versions existed (pre-release).
   #[error("meta.db has no format version: it was written before format versions were introduced; create a new data directory")]
   PreFormat,
   ```

   `routes/connections.rs::meta_error_kind`: three arms, `"format too new"`, `"schema too new"`, `"pre-format"`.

3. **`check_marker`**, a new private function after `applied_version`:

   ```rust
   /// What `check_marker` found.
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   enum Marker {
       /// `user_version` is `META_FORMAT_VERSION`.
       Current,
       /// `user_version` is 0 and the database holds no user and no org: new, or a crash
       /// between the migrations and the stamp. It gets stamped.
       Unstamped,
   }

   /// The format and migration checks of spec 04 § Format versions, in this order, writing
   /// nothing: a newer `user_version` is `FormatTooNew`; an applied migration this build does
   /// not embed is `SchemaTooNew` (unless `newer_schema_ok`); `user_version` 0 with a row in
   /// `users` or `orgs` is `PreFormat`.
   async fn check_marker(connection: &mut SqliteConnection, newer_schema_ok: bool) -> Result<Marker, MetaError>
   ```

   - `user_version`: `sqlx::query_scalar::<_, i64>("PRAGMA user_version")`. If it is neither 0 nor `META_FORMAT_VERSION`: `FormatTooNew { found, supported: META_FORMAT_VERSION }` (no older version exists, so a negative value is refused the same way).
   - Unknown migrations: when `_sqlx_migrations` exists (the `sqlite_master` query of `applied_version`), `SELECT version FROM _sqlx_migrations`; the versions not in `migrator().migrations` are unknown; if any and not `newer_schema_ok`: `SchemaTooNew { version: <the largest unknown> }`.
   - Pre-release: if `user_version` is 0, list `users` and `orgs` in `sqlite_master`; for those present, `SELECT EXISTS(SELECT 1 FROM <table>)`; any true: `PreFormat`. Else `Unstamped`.
   - `user_version` equal: `Current`.

4. **`stamp`**, private: `sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA user_version = {META_FORMAT_VERSION}")))` (a pragma takes no bound parameter; sqlx 0.9 needs `AssertSqlSafe` for a non-literal query, as `admin.rs` around lines 703 and 721 already does), on the writer, outside a transaction (`synchronous=FULL` makes it durable).

5. **`MetaDb::open`** new order, after `connect(&path, true)`:
   1. `let marker = …` (step 7: `check_file` on the path before `connect`, `Unstamped` for a new file) — before `connect`, `applied_version`, `pre_migration_copy` and the migrations, so a refused file is not opened for writing, gets no copy and no migration.
   2. `applied_version`, `pre_migration_copy`, `migrator().run` as today; map `MigrateError::VersionMissing(version)` to `SchemaTooNew { version }` (it cannot happen after the check, but a race with another writer is not excluded), every other error stays `Migrate`.
   3. `if marker == Marker::Unstamped { stamp(&mut writer).await?; }`.

6. **`open_checked`** (`open_existing`, `open_existing_or_newer`): after the `NotInitialized` check and before `connect(&path, false)`, `check_file(&path, newer_ok)` (step 7); an `Unstamped` result is accepted (an empty database). Then the existing equality, changed so that a **newer** applied version without `newer_ok` cannot reach it (the check already refused it) and an older one stays `SchemaMismatch`. `open_existing_or_newer` thus accepts a newer schema of the **same** format only; a newer format is `FormatTooNew` (D-M-9's rollback reads by column name, which a format change does not promise).

7. **`preflight`**, `pub(crate)`, after `open_checked`:

   ```rust
   /// The checks of [`check_marker`] on `<data_dir>/meta.db`, before anything else of the
   /// data directory is opened (spec 04 § Format versions, D-R-33). A missing file passes:
   /// `open` creates it. Writes nothing. `cetra serve`, `admin migrate` and `admin gc-orphans`
   /// call it before storage opens.
   pub(crate) async fn preflight(data_dir: &Path) -> Result<(), MetaError>
   ```

   It connects with **its own options**, `SqliteConnectOptions::new().filename(&path).create_if_missing(false).busy_timeout(BUSY_TIMEOUT).disable_statement_logging()`: **no `journal_mode`**. `writer_options` sets `journal_mode(Wal)`, and sqlx then runs `PRAGMA journal_mode = WAL`, which rewrites the header of a rollback-mode file, and a restored backup's `meta.db` is a `VACUUM INTO` output in rollback mode. It runs `check_marker(&mut connection, newer_schema_ok)`, closes the connection with `.close().await` and returns the `Marker`. A connection error is `MetaError::Open`. Shape: a private `async fn check_file(path: &Path, newer_schema_ok: bool) -> Result<Marker, MetaError>` doing that, and `pub(crate) async fn preflight(data_dir)` = missing file → `Ok(())`, else `check_file(.., false).map(|_| ())`.

   **Why not a read-only connection:** a read-only connection to a WAL database needs an existing `-shm` file, which a cleanly stopped server removes.

   **`MetaDb::open` and `open_checked` use `check_file` too**, as their first step (when the file exists), instead of running `check_marker` on the writer: so a refused file is never switched to WAL. Step 5.1 and step 6 therefore read: "`let marker = if path.is_file() { check_file(&path, …).await? } else { Marker::Unstamped };`, then `connect(...)`". The writer's own later checks are unchanged.

8. **`serve.rs::bind`**: right after `create_dir_all` and **before** `Storage::open`:

   ```rust
   meta::preflight(&data_dir).await?;
   ```

   (`ServeError` already has a `From<MetaError>`, used for `MetaDb::open`; if it has not, use the same mapping that line uses.) The module doc's first sentence becomes: "Startup creates `data_dir`, checks `meta.db` is not newer (`meta::preflight`), then `Storage::open` (which takes `LOCK`), then `meta.db`."

9. **`admin.rs`**: `gc_orphans` and `migrate` call `crate::meta::preflight(&config.data_dir).await.map_err(map_meta)?` after their file-existence checks and before `Maintenance::open`. The preflight only reads, so it may run before `LOCK` is taken; with a running server **and** a newer `meta.db`, the format error wins over `Locked`. Update the doc comment of `migrate` (around line 518, "The maintenance open comes next: its `LOCK` refuses a running server before `meta.db` is touched") to: "The `meta.db` preflight comes first (it reads only), then the maintenance open, whose `LOCK` refuses a running server before `meta.db` is written." Same for the comment near `crates/cli/src/main.rs` around line 1478 in the test `migrate_while_the_server_runs_is_refused` (comment only; the test compares `meta.db` bytes and keeps passing, since the preflight writes nothing). **Edit** for SF1 therefore includes that comment in `crates/cli/src/main.rs`. `rotate_encryption_key` and the user commands already go through `open_existing`, which runs `check_marker`. `map_meta`: `SchemaTooNew`, `FormatTooNew` and `PreFormat` fall into the existing `other => AdminError::Database(other.to_string())` arm, whose text is the message above: leave it so, do not add `AdminError` variants.

10. **Spec**: delete the "(SF1)" markers in `docs/spec/04-metadata-db.md` § Format versions and `docs/spec/12-operations.md` § Process lifecycle.

### Tests (`meta/mod.rs` `tests` unless said)

Add `test_digest::dir_digest` (see Names); it is `pub(crate)` under `cfg(test)`, so `admin.rs`, `serve.rs` and `backup.rs` tests use it as `crate::meta::test_digest::dir_digest`. Every test closes the `MetaDb` (`.close().await`) or the connection before it takes a digest.

1. `a_new_meta_db_is_stamped`: `MetaDb::open`, close, read `PRAGMA user_version` through `connect(&path, false)`: `META_FORMAT_VERSION`. Open again: still that value, no error.
2. `a_newer_format_is_refused_before_anything_changes`: open, close; set `PRAGMA user_version = 2` with a plain connection; `dir_digest`; `MetaDb::open`, `MetaDb::open_existing`, `MetaDb::open_existing_or_newer` and `preflight` each return `FormatTooNew { found: 2, supported: 1 }`; the digest is equal. Variant with migrations pending (build the file with the first migration only, the `first_only` pattern of `opening_an_older_meta_db_keeps_a_pre_migration_copy`, then set the marker to 2): refused, and no `meta.db.pre-m*` file exists.
3. `an_unknown_migration_is_refused_before_anything_changes`: open, insert a `_sqlx_migrations` row with version `expected_version() + 1` (the existing pattern of `open_existing_checks_the_file_and_the_version`), close, digest; `MetaDb::open`, `open_existing` and `preflight` return `SchemaTooNew { version: expected + 1 }`; `open_existing_or_newer` succeeds; digest equal, no `.pre-m` file. Update `open_existing_checks_the_file_and_the_version`: its future-version case now expects `SchemaTooNew`; its older-version case (if any) keeps `SchemaMismatch`. Update `admin.rs` `schema_mismatch_names_the_versions` (around lines 970-995): it inserts a future migration and asserts the text `this binary expects schema …`; it now gets `AdminError::Database` with `meta.db has applied migration {future}, which this build does not have`: assert that text, and add one case with an **older** schema (the `set_schema` helper around line 2235) that still gives `AdminError::Schema` and its text. `AdminError::Schema` is then reachable for older schemas only: say so in its doc comment. Search the crate for other tests that insert into `_sqlx_migrations` or match `SchemaMismatch` (`grep -rn "_sqlx_migrations\|SchemaMismatch\|expects schema" crates/server/src crates/cli/src`) and fix each the same way; list them in the report.
4. `a_pre_release_meta_db_with_data_is_refused`: open, insert one org with the file's `insert_org` helper, close, set `PRAGMA user_version = 0`; `MetaDb::open` returns `PreFormat` and the org is still there. Variant with the first migration only plus an org row: `PreFormat`, no `.pre-m` file, `_sqlx_migrations` still has one row.
5. `an_empty_meta_db_without_a_marker_is_stamped`: as 4 without rows: opens and is stamped; also with the first migration only (it migrates, keeps the `.pre-m` copy as today, then stamps). The existing `opening_an_older_meta_db_keeps_a_pre_migration_copy` and `an_existing_pre_migration_copy_is_kept` keep passing unchanged (their databases have no rows).
6. `the_pre_migration_copy_keeps_the_marker`: `VACUUM INTO` a stamped database (as `pre_migration_copy` does) and read the copy's `user_version`: `META_FORMAT_VERSION`. If SQLite does not copy it, STOP and report: D-R-33 relies on it (backups, SF5).
7. `serve.rs` tests, `a_newer_meta_db_stops_serve_before_storage_opens`: a data directory holding only a `meta.db` stamped `2` (create it with `MetaDb::open` in a temp dir, close, set the marker): `bind(config(dir), None)` returns an error whose text contains `format version 2`; afterwards the directory holds no `catalog.db`, no `wal/` and no `LOCK` (storage never opened), and `dir_digest` is unchanged.
8. `admin.rs` tests, `migrate_and_gc_orphans_refuse_a_newer_meta_db_first`: a data directory made by `serve::bind(test_support::config(dir), None)` then `running.shutdown()` (both `catalog.db` and `meta.db` must exist, or the commands stop at `NoCatalog`/`NoMetaDb` before the preflight; the `ready_config` helper makes only `meta.db`), then `meta.db` stamped `2` with a plain connection; digest; `admin::migrate` and `admin::gc_orphans(…, dry_run = true, …)` fail with `AdminError::Database(text)` where `text` contains `format version 2`; digest equal (the catalog was not opened: no `catalog.db.pre-m*`, no new WAL segment).

### Done when

`just check` green; the 8 tests above pass; `grep -n "preflight" crates/server/src/serve.rs crates/server/src/admin.rs` shows the three calls; spec 04 § Format versions has no "(SF1)".

---

## SF2 — The `meta.db` migration lint

Commit: `server: lint meta.db migrations as additive and test the altering ones (SF2)`

**Edit:** `crates/server/src/meta/migration_tests.rs` (new), `crates/server/src/meta/mod.rs` (one `#[cfg(test)] mod migration_tests;` line, one module-doc sentence).

No behaviour change. Mirrors R8.5 (D-R-31) for `meta.db`.

### Steps

1. `meta/mod.rs`: `#[cfg(test)] mod migration_tests;` after the `pub(crate) mod` lines. Module doc, new last paragraph: "A migration that has been in a release is never edited; a change is a new migration, and a migration that does more than add is a format change (spec 04 § Format versions, D-R-34)."
2. `migration_tests.rs` with these constants, each with a comment:

   ```rust
   /// Migrations from before the first public release that do more than add. Frozen at
   /// that release: never append to it afterwards (a later one is format-changing).
   const PRE_RELEASE_NON_ADDITIVE: &[i64] = &[8]; // 0008 drops orgs.limits (D-P-11)
   /// Released migrations that changed the format; each came with a META_FORMAT_VERSION bump.
   const FORMAT_CHANGING_MIGRATIONS: &[i64] = &[];
   ```

3. **`migrations_only_add`.** For every migration in `super::migrator().migrations` (sqlx 0.9.0: `migration.sql` is a `SqlStr`; read it with `migration.sql.as_str()`) whose version is in neither list: remove `--` comments to the end of the line, split on `;`, trim, skip empty statements, collapse whitespace, and require each statement to start (case-insensitively) with `CREATE TABLE `, `CREATE INDEX ` or `CREATE UNIQUE INDEX `, or to match `ALTER TABLE <name> ADD COLUMN <column> <definition>` where `<definition>` contains `DEFAULT` or does not contain `NOT NULL`. Anything else fails with: `migration {version:04} is not additive: a meta.db format change. Bump META_FORMAT_VERSION (spec 04 § Format versions), then list the migration in FORMAT_CHANGING_MIGRATIONS.` (No migration on `main` 2f5181a9 has a `--` inside a string literal; if one does, STOP and report.) A partial index (`CREATE INDEX … WHERE …`) passes. A `CHECK` on an added column passes (SQLite checks it against the existing rows when the column is added, so a migration that would violate it fails at open instead of corrupting).
4. **`migrations_that_alter_a_table_keep_its_rows`**, table-driven, one case per altering migration:
   - **8** (`orgs.limits` dropped): build a database with migrations up to 7 (the `first_only` pattern of `meta/mod.rs` tests, cutting the list at the index of version 8), stamp it (`PRAGMA user_version = 1`: a released database has the marker; without it the open returns `PreFormat`), insert an org with every column of 0001-0007's `orgs` (read the migrations for the list), `MetaDb::open`: the org's other columns are unchanged.
   - **13** (`web_sessions.sso_org_id`): database at 12, stamped, one user, one org and one `web_sessions` row; after the open, the row is intact and `sso_org_id` is NULL.
   - **15** (`users.email_verified_via`, `web_sessions.via_provider`): database at 14, stamped, a user and a session; after the open `email_verified_via` is NULL and `via_provider` is 0.
   Raw SQL inserts are fine here: the point is rows written by an older schema.
5. **`every_altering_migration_has_a_case`**: the versions of every migration whose SQL contains `ALTER TABLE` (after comment removal) equal the case versions (`[8, 13, 15]` on `main` 2f5181a9; add the cases for track J's altering migrations that are on `main` when SF2 runs, and say so in the report). Failure message: `migration {v:04} alters a table: add a case to migrations_that_alter_a_table_keep_its_rows`.
6. **`pre_release_exceptions_predate_the_first_release`**: read `fixtures/storage/golden/RELEASES` if it exists; if it lists at least one version and `fixtures/server/golden/<first version>/expected.json` exists (SF6), every entry of `PRE_RELEASE_NON_ADDITIVE` is at most that file's `meta_schema`. Otherwise the test passes and prints `no released server golden directory yet`. This is what freezes the list at the first release.
7. The schema snapshot `schema_matches_the_snapshot` stays as it is; it is the readable diff of a schema change (D-R-34).
8. `docs/spec/04-metadata-db.md` § Format versions: delete "(SF2)". **Edit** for SF2 therefore also includes that file.

### Tests

The four tests above. Report each one's run time.

### Done when

`just check` green; temporarily writing `UPDATE orgs SET name = name;` at the end of a copy of the last migration, in a scratch test run only, makes `migrations_only_add` fail with the message (do not commit; say in the report that you saw it fail).

---

## SF3 — Pin `secret.key` and the sealed values

Commit: `server: pin the key file and sealed value formats with known answers (SF3)`

**Edit:** `crates/server/src/crypto.rs` (tests module only), `crates/server/src/admin.rs` (tests module only, test 6), `docs/spec/04-metadata-db.md` (markers).

No production code change: the formats stay as D-I-1 and D-I-2 define them (D-R-35).

### Tests (`crypto.rs` `tests`)

`const GOLDEN_KEY: [u8; 32]` = `0..=31` (build it with `core::array::from_fn(|i| i as u8)` or a literal).

1. `the_key_file_format_is_pinned`: `write_key_file(dir/secret.key, &GOLDEN_KEY)`; the file's bytes are exactly `b"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=\n"`; mode `0o600`. `read_key(None, dir).unwrap().unwrap().key` (it returns `Result<Option<LoadedKey>, _>`) is `GOLDEN_KEY` for that file, for the same text with `\r\n`, and with no newline; `decode_key` of the text without newline equals `GOLDEN_KEY` (the `CETRA_ENCRYPTION_KEY` form).
2. `a_key_file_this_build_does_not_read_is_never_rewritten`: for each of `"cetra-key:2:AAEC…\n"` (any non-base64 prefix), the base64 of 31 bytes, `"<golden text>\n\n"` and `" <golden text>"`: write the file, take its bytes; `load_key(None, dir, false)` returns `BadKey`; the file bytes are unchanged; no `secret.key.tmp` exists. This is the refusal that makes a version byte unnecessary (D-R-35).
3. `the_key_id_and_check_value_are_pinned`: `hex::encode(key_id_of(&GOLDEN_KEY)) == "c2f47f2a"`; `SecretBox::new(&GOLDEN_KEY).check_value() == "212e59571b77a8a332ea48a6e4c994fe54e3113cd5665185f707710faf865eb0"`. If either differs, STOP and report: either the code or this plan's independent computation is wrong, and the plan must not be "fixed" by pasting the code's value.
4. `a_sealed_value_of_format_1_opens`: `SecretBox::new(&GOLDEN_KEY).open(&hex::decode(SEALED_V1), Aad { table: "llm_connections", column: "secret_enc", row_id: &Uuid::parse_str("018f6b4e-7c1a-7c1a-8c1a-0000000000c1") })` returns `b"golden-secret"`, where

   ```rust
   /// Sealed with GOLDEN_KEY, nonce 00 01 … 0b, outside this code (Python `cryptography`
   /// AESGCM), so a change of the envelope, the AAD text or the key id fails here.
   const SEALED_V1: &str = "01c2f47f2a000102030405060708090a0b206dba7fa08bef68e822e5eec592c7bcf69ecc62ea156d686a259dec42";
   ```

   The same bytes with another AAD row id are `Corrupt`; with byte 0 set to `0x02`, `Corrupt`.
5. `the_sealed_layout_is_pinned`: `seal(b"x", aad)` is 1 + 4 + 12 + 1 + 16 bytes long, starts with `0x01`, then the key id `c2 f4 7f 2a`.
6. `rotation_keeps_the_formats`, in `admin.rs` tests next to the rotation tests (around line 1266, whose setup it copies): after `rotate_encryption_key` on a directory sealed with `GOLDEN_KEY`, `secret.key` is one base64 line plus `\n` of a different 32-byte key, every sealed value starts with `0x01` and the new key id, and the stored check value is the new key's.

### Spec

`docs/spec/04-metadata-db.md` § Format versions: delete "(SF3)".

### Done when

`just check` green; the six tests pass.

---

## SF4 — A version on the `admin.sock` protocol

Commit: `server: version the admin socket protocol (SF4)`

**Edit:** `crates/server/src/admin_socket.rs`, `crates/cli/src/main.rs` (`backup_command` only, around lines 598-669), `docs/self-hosting/backup.md` (the code list), `docs/spec/12-operations.md` (marker).

### Steps

1. `admin_socket.rs`:

   ```rust
   /// The admin protocol version (D-R-36). Every request and response carries it as `"v"`.
   /// Within version 1 a change only adds: a new command, a new request field with a
   /// default, a new response field, a new error code. Renaming or removing any of them,
   /// or changing a field's meaning, is version 2.
   pub const ADMIN_PROTOCOL: u32 = 1;
   ```

2. **Request.** A new struct, the one thing a client sends:

   ```rust
   /// One request line: the protocol version and the command.
   #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
   pub struct AdminEnvelope {
       pub v: u32,
       #[serde(flatten)]
       pub request: AdminRequest,
   }
   ```

   `parse` becomes four steps: JSON (`bad_request`, as today); then `v`: `value.get("v").and_then(Value::as_u64)` must be `Some(ADMIN_PROTOCOL)`, else `AdminResponse::failed("unsupported_protocol", format!("this server (cetra {}) speaks admin protocol {ADMIN_PROTOCOL}; the request has {}; run the cetra binary of the server's release", env!("CARGO_PKG_VERSION"), found))` where `found` is `version N` or `no version`; then the command (`unknown_command`, as today); then `serde_json::from_value::<AdminEnvelope>` (`bad_request`). It returns the inner `AdminRequest`.
3. **Response.** Both `AdminResponse` variants get a first field `v: u32`. The two constructors set it to `ADMIN_PROTOCOL`: `AdminResponse::failed` (used by `forbidden` too) and `run_backup`'s `AdminResponse::Done { ok: true, report }`. The CLI's patterns use `..` and need no change.
4. **Client side**, in `admin_socket.rs` so it is testable without the CLI:

   ```rust
   /// Why a response line could not be read (the client side of D-R-36).
   #[derive(Debug, thiserror::Error)]
   pub enum AdminProtocolError {
       #[error("the server's answer is not JSON")]
       NotJson,
       #[error("the server speaks admin protocol {found}, this command speaks {ADMIN_PROTOCOL}; run the cetra binary of the server's release")]
       OtherProtocol { found: String },
       #[error("the server's answer has an unknown shape")]
       Shape,
   }

   /// Parses one response line: JSON, then `v`, then the shape.
   pub fn decode_response(line: &str) -> Result<AdminResponse, AdminProtocolError>
   ```

   `found` is the number, or `none` when `v` is missing.
5. `crates/cli/src/main.rs`, `backup_command` (around lines 598-669): send `serde_json::to_string(&AdminEnvelope { v: ADMIN_PROTOCOL, request: AdminRequest::Backup { to, meta_only } })`; read the answer with `decode_response(&line).context("reading the server's answer")?`. Nothing else in the function changes.
6. `docs/self-hosting/backup.md`, the paragraph listing the codes: add `unknown_command` and `unsupported_protocol` ("the command and the server are different releases: run the command of the server's release, e.g. through `docker compose exec`"). `docs/spec/12-operations.md` § Backups: delete "(SF4)".

### Tests (`admin_socket.rs` `tests`)

Existing tests that send raw JSON add `"v": 1`; those that parse responses use `decode_response`.

1. `a_request_without_the_protocol_version_is_refused`: `{"command":"backup","to":"/x"}` and the same with `"v": 2`: code `unsupported_protocol`, message contains `protocol 1` and the server's `CARGO_PKG_VERSION`; no backup ran (the target does not exist).
2. `every_response_carries_the_protocol_version`: a successful backup, a `bad_request`, an `unknown_command` (`{"v":1,"command":"restore"}`) and a `forbidden` (if the existing forbidden test can produce one; otherwise call `AdminResponse::failed` directly): each line's `v` is 1. `forbidden` cannot be produced from a test (the peer uid check), so check it through `AdminResponse::failed("forbidden", "m")` serialised.
3. `the_client_refuses_another_protocol`: `decode_response` of `{"v":2,"ok":true}` is `OtherProtocol { found: "2" }`, of `{"ok":false,"code":"busy","message":"m"}` is `OtherProtocol { found: "none" }`, of `nope` is `NotJson`; of a recorded success line with an extra unknown field (`"later": 1`) succeeds (additive).
4. `the_error_codes_are_stable`: build one value of every `BackupError` variant (`Flush(FlushNowError::Timeout)`, `Snapshot(SnapshotError::Exists)`, `Objects(CopyObjectsError::RemoteStore)`, `Io(io::Error::other("x"))`, `Meta(..)` with the error type it holds; read the enum for the exact paths) and collect `code()`, plus the socket's own codes; the sorted set equals `["bad_request", "busy", "flush_failed", "forbidden", "io", "meta_copy_failed", "meta_only_needs_s3", "objects_failed", "snapshot_failed", "target_inside_data_dir", "target_not_empty", "unknown_command", "unsupported_protocol"]`. Message: `admin protocol error codes are stable (D-R-36): add new codes, never rename one`. If `BackupError` has a variant not covered, the match in `code()` names it: add its code to the list and say so in the report.
5. CLI (`crates/cli` tests, if the backup command has one; read `crates/cli/tests/`): a backup through the real binary still prints `backup written to`. If there is no such test, `admin_socket::tests::a_backup_request_returns_the_report` sending an `AdminEnvelope` built like the CLI does is the check.

### Done when

`just check` green; `grep -rn '"command"' crates/server/src/admin_socket.rs` shows no test request without `"v"`.

---

## SF5 — Version fields in the backup manifest

Commit: `server: record the database formats in the backup manifest (SF5)`

**Edit:** `crates/server/src/backup.rs`, `docs/self-hosting/backup.md` (§ The manifest and § Restore), `docs/spec/12-operations.md` (marker).

### Steps

1. `ManifestParts` gets `formats: Formats`, a new private struct:

   ```rust
   /// The database versions of the backup, read from its own copies (D-R-37).
   struct Formats {
       /// `PRAGMA user_version` of the backup's `meta.db` (META_FORMAT_VERSION).
       meta_format: i64,
       /// Highest `_sqlx_migrations.version` of the backup's `meta.db`.
       meta_schema: i64,
       /// `PRAGMA user_version` of the backup's `catalog.db` (storage FORMAT_VERSION, D-R-26).
       storage_format: i64,
       /// Highest `_sqlx_migrations.version` of the backup's `catalog.db`.
       catalog_schema: i64,
   }
   ```

2. `read_formats(dir: &Path) -> Result<Formats, BackupError>`: for `dir/meta.db` and `dir/catalog.db`, a `sqlx` connection with `SqliteConnectOptions::new().filename(..).read_only(true)` (the copies are `VACUUM INTO` outputs, not WAL databases; if a read-only open fails, use `create_if_missing(false)` without `read_only`, and say so in the report), the two queries, `close().await`. Errors map to `BackupError::Io` (`io::Error::other(error.to_string())`; the text names no values). Call it after both copies are written and synced and before `manifest`, in `run` (read `run` for the step order; it is § "What the server does" steps 2 and 5 of `backup.md`).
3. `manifest` writes the four values as top-level keys `meta_format`, `meta_schema`, `storage_format`, `catalog_schema`, after `cetra_version`. `FORMAT` stays 1, with a doc comment: "`manifest.json` format (D-R-37). Within 1, keys are only added; a removed, renamed or retyped key, or a changed meaning, is 2. Nothing reads the manifest in the MVP: a newer backup is refused by the database checks at restore."
4. `docs/self-hosting/backup.md` § The manifest: four rows: `meta_format` "the format of `meta.db`"; `meta_schema` "the last `meta.db` migration"; `storage_format` "the format of the data directory (`catalog.db`)"; `catalog_schema` "the last `catalog.db` migration". § Restore, a paragraph before step 1: "**A backup from a newer release cannot be restored with an older one.** The first command of step 5 (`gc-orphans`) or step 6 (`migrate`) refuses it before it changes anything: `meta.db has format version …` or `meta.db has applied migration …, which this build does not have`. Restore it with the release in the manifest's `cetra_version`, or a newer one." And in step 5, one sentence: "It checks `meta.db` first and stops on a backup from a newer release."
5. `docs/spec/12-operations.md` § Backups: delete "(SF5)".

### Tests (`backup.rs` `tests`)

1. `the_manifest_records_the_database_versions`: after a backup (the existing test setup), `meta_format == META_FORMAT_VERSION`, `meta_schema == meta::expected_version()`, and `storage_format` and `catalog_schema` equal `PRAGMA user_version` and `MAX(version)` of the live `<data_dir>/catalog.db` (read with a plain connection after the backup).
2. `the_manifest_keys_are_pinned`: the sorted top-level keys equal `["catalog_schema", "created_at", "encryption_key", "files", "flushed_seq", "format", "gc_grace_secs", "cetra_version", "meta_format", "meta_schema", "mode", "object_store", "objects", "restorable_until", "storage_format"]`, and each value has the JSON type of § The manifest (string, number, object, array, or null for `restorable_until` in full mode). Message: `manifest.json format 1 only gains keys (D-R-37): a removed, renamed or retyped key bumps FORMAT`.
3. `a_newer_backup_is_refused_by_the_first_restore_command`: take a backup, stamp the backup's `meta.db` with `user_version = 2` through a connection that sets no journal mode (SF1's `check_file` options: the copy is in rollback mode and must stay so); restore it as `backup.md` § Restore step 3 says into a fresh data directory (copy `meta.db`, `catalog.db`, `secret.key`, `objects/`); `admin::gc_orphans(dry_run = false)` (local mode) fails with text containing `format version 2`, and `dir_digest` of the restored directory is unchanged (nothing deleted, catalog not opened).

### Done when

`just check` green; the three tests pass.

---

## SF6 — The golden server directory generator

Commit: `server: generate a golden backup directory from the current build (SF6)`

**Edit:** `crates/server/src/golden_tests/mod.rs`, `build.rs`, `dump.rs` (new), `crates/server/src/lib.rs` (`#[cfg(test)] mod golden_tests;`), `justfile` (`golden-dir`), `AGENTS.md` (Commands, Tests, Layout).

Prerequisite: R8.8 merged (§ Order). Nothing is committed under `fixtures/server/golden/` before the first stable tag (D-R-38, as D-R-29).

### What a golden server directory is

```
fixtures/server/golden/<version>/
  manifest.json        written by the server's online backup
  meta.db              "
  catalog.db           "
  secret.key           "   (GOLDEN_KEY: a public test key)
  objects/             "   (only if the backup has objects; the scenario writes none)
  admin_socket.jsonl   the request and response lines of the socket calls that produced it
  expected.json        what the check compares (below)
```

It is an online backup because that is the one server-written artifact that holds every durable file: proving that a backup of release X restores with every later release proves the four surfaces of this plan at once.

### `golden_tests/build.rs`

```rust
/// Writes the golden scenario into `out` (must not exist): a data directory filled
/// through the server's own store functions, served once, and backed up through the
/// admin socket into `out`. Returns the socket lines. Its `expected.json` (dump.rs) is
/// deterministic except for the `rows` ids and times, which are recorded as found.
pub(super) async fn build(out: &Path) -> Vec<SocketLine>;
```

Steps:

1. A temp data directory. `crypto::write_key_file(<data>/secret.key, &GOLDEN_KEY)`.
2. `MetaDb::open(<data>)`, `SecretBox::new(&GOLDEN_KEY)`, `crypto::verify_or_record`.
3. **The scenario**, through the store functions in `crates/server/src/meta/*.rs` and the services the routes call (read the route of each table to find them; the tests of each store show the call shapes). Fixed names, emails, plaintexts; ids are whatever the functions generate. At least:
   - users `golden-owner@example.com` (password `golden-password-1`, through the same hashing as signup: `auth::password`), `golden-member@example.com`; one org `golden-org`; memberships owner and member; one pending invite;
   - projects `golden-p1` and `golden-p2`; one project role override (D139) if the table exists;
   - an `sk-cetra-` and a `pk-cetra-` key on `golden-p1` (record the plaintexts), a personal access token for the owner, a web session for the owner (record the token plaintexts: the check signs in with them);
   - an LLM connection with a sealed key `golden-secret` and one extra header; an evaluator of each kind (judge, check, webhook with a sealed secret); an online rule with a filter; a score definition of each type; a saved view; an alert rule and a notification channel of each kind (sealed config); an org SSO provider with a sealed client secret, if `ee` is compiled in tests (it is: the crate has no feature for it; check);
   - a deletion of each kind (`DeletionKind`), one applied, one pending, one failed;
   - an org entitlement token (`test_support::org_token`, the dev key) and a window end; `usage_daily` rows; one row in each queue table (`eval_jobs`, `pending_traces`, `backfills`, `rule_spend`, `usage_notices`, `email_verifications`, `password_resets`) through its store function;
   - any table track J or T has added by the time SF6 runs (datasets, items, experiments, model prices): one row each, through its store.
   Where a table's only writer is a background task, call that task's function, or insert the row with the SQL that task uses (copy its column list and values, with a comment naming the task).

   **Writers, by table** (`main` 2f5181a9; found with `grep -rn "INSERT INTO <table>" crates/server/src`; use the function, or the function of that file that contains the `INSERT`, and copy the call shape from that file's tests):

   | Table | Writer |
   |---|---|
   | `users` | `meta/users.rs::insert_user` (password hash from `auth::password`) |
   | `identities` | `meta/identities.rs::insert_identity` |
   | `web_sessions` | `meta/web_sessions.rs::insert_session` |
   | `orgs` | `meta/orgs.rs::insert_org` |
   | `org_members` | `meta/members.rs::add_org_member` |
   | `projects` | `meta/projects.rs::insert_project` |
   | `project_members` | `meta/members.rs::set_project_override` |
   | `invites` | `meta/invites.rs::replace_invite` |
   | `password_resets` | `meta/password_resets.rs::insert_reset` |
   | `email_verifications` | `meta/email_verifications.rs::replace_verification` |
   | `api_keys` | `meta/api_keys.rs::insert_api_key` |
   | `personal_tokens` | `meta/personal_tokens.rs::insert_token` |
   | `instance_settings` | `meta/instance_settings.rs::put_setting` (and `crypto::verify_or_record`) |
   | `llm_connections` | `meta/connections.rs::insert_connection` |
   | `saved_views` | `meta/saved_views.rs::insert_view` |
   | `usage_notices` | `entitlements/notices.rs` (the function that contains the `INSERT`) |
   | `usage_daily` | `usage.rs` (the flush's upsert) |
   | `backfills` | `meta/backfills.rs::insert_backfill` |
   | `org_sso_providers`, `org_sso_domains` | `ee/org_sso/store.rs` |
   | `pending_traces` | `meta/pending_traces.rs::write_pending` |
   | `org_entitlements` | `meta/entitlements.rs::put_token` (token from `test_support::org_token`) |
   | `org_window_ends` | `retention.rs` (the function that contains the `INSERT`) |
   | `notification_channels`, `alert_rules`, `alert_rule_channels`, `alert_events`, `notification_deliveries` | `meta/alerts.rs` (`insert_channel`, `insert_rule`, and the functions with the other `INSERT`s) |
   | `evaluators` | `meta/evaluators.rs::insert_evaluator` |
   | `score_definitions` | `meta/score_definitions.rs::insert_definition` |
   | `deletions`, `deletion_traces`, `deletion_sessions`, `deletion_projects` | `meta/deletions.rs::insert_deletion` with `insert_traces`, `insert_sessions`, `insert_projects` (as the `admin.rs` tests call them) |
   | `online_rules` | `meta/online_rules.rs::insert_rule` |
   | `eval_jobs` | `meta/eval_jobs.rs` (the enqueue function the dispatcher's tests use) |
   | `rule_spend` | `meta/rule_spend.rs::add_spend` |

   A table missing from this list (added by J or T after 2f5181a9) is found the same way; name its writer in the report.

   **Rows the startup tasks would change.** `bind` starts the eval dispatcher, backfill runner, deletion worker, pending-trace sweep, pruners and retention task (spec 12 § Process lifecycle), and they run one pass at once. Give queue rows states those tasks leave alone: `eval_jobs` in `done` and `failed` with `created_at` = now (the pruner deletes them only after 7 days); `backfills` in `failed`; deletions `applied` and `failed`, plus one `pending` whose project is `golden-p2` (the worker applies it; the backup records whatever state it reached, and `expected.json` is computed from the backup, so that is fine). After the first `build`, list the tables that the startup tasks emptied (compare row counts before `bind` and in the backup); for each, either find a state the task keeps, or add it to `EMPTY_TABLES` with the reason `consumed at startup by <task>`. Report the list.

4. Close `MetaDb`. `serve::bind(config(<data>), None)` (`test_support::config`); it opens storage (empty), migrates nothing, verifies the key.
5. Over `<data>/admin.sock`, as the CLI does (an `AdminEnvelope` line, `decode_response`), send in order and record each `(request line, response line)`:
   - `{"v":1,"command":"backup","to":"<out>","meta_only":false}` → `ok`;
   - the same again → `target_not_empty`;
   - `{"v":1,"command":"backup","to":"relative"}` → `bad_request`;
   - `{"v":1,"command":"restore"}` → `unknown_command`;
   - `{"command":"backup","to":"/nowhere"}` → `unsupported_protocol`.
   The `to` of the recorded first line is replaced by the literal `"/golden/out"` before it is written (paths differ per machine; the check sends recorded requests to a temp target of its own).
6. `running.shutdown()`. Write `<out>/admin_socket.jsonl`: one JSON object per line, `{"request": <request>, "response": <response>}` with `"created_at"` and `"duration_ms"` of the report replaced by `"<redacted>"` and `0` (they vary).

### `golden_tests/dump.rs`

```rust
/// What a restored golden directory must answer, as JSON (expected.json).
pub(super) async fn dump(meta: &MetaDb, secrets: &SecretBox, data_dir: &Path) -> serde_json::Value;
```

Keys:

```
generator          1 (frozen from the first release; a scenario change is a new number beside the old)
workspace_version  CARGO_PKG_VERSION at generation
meta_format        META_FORMAT_VERSION at generation
meta_schema        expected_version() at generation
rows               {"<table>": [[<column values in column order>], …]} for every table of
                   meta.db except _sqlx_migrations, plus "columns": {"<table>": [names]};
                   rows ordered by every column (ORDER BY 1, 2, …, n); BLOBs as lowercase
                   hex, NULL as null
reads              per table, the decoded values the server's own read functions return,
                   keyed by a natural key (email, slug, name), never by generated ids:
                   roles, states, kinds, statuses, parsed JSON configs as the server type
                   serialises them, booleans and numbers
secrets            {"<table>.<column>.<natural key>": "<plaintext>"} for every sealed value,
                   opened with `secrets`
auth               {"password": true, "sk": "golden-p1", "pk": "golden-p1", "pat": "golden-owner@example.com",
                    "session": "golden-owner@example.com"}: what each recorded credential resolves to
                   through the server's verification functions (argon2 verify, hash_token lookups)
credentials        the plaintexts the auth checks use (test values)
encryption_check   the stored check value (equals GOLDEN_KEY's)
```

`reads` lists, for each table, which function produced it in a comment in `dump.rs`. A table with no read function in the server has only `rows`.

### Tests (`golden_tests/mod.rs`)

- **Speed.** One fresh build per process: `static FRESH: OnceLock<tempfile::TempDir>`, built on first use with its `expected.json` (the M8 pattern). Argon2 makes a password hash cost tens of milliseconds; the scenario has two.
- `every_table_has_a_golden_row`: in the fresh directory's `meta.db`, every table except `_sqlx_migrations` has at least one row, unless listed in `const EMPTY_TABLES: &[(&str, &str)]` (name, reason), which starts empty. Message: `table {t} has no row in the golden scenario: add one in golden_tests/build.rs`. This keeps the scenario complete while tracks add tables before the release.
- `write_server_golden_dir`, `#[ignore = "writes a fixtures directory; run by just golden-dir <version>"]`: reads `CETRA_SERVER_GOLDEN_OUT` (absolute, must not exist), runs `build`, then restores a **copy** (SF7's `restore_copy`) to compute the dump, writes `expected.json` (`to_string_pretty`, trailing newline), prints the total size. Refuses when `CARGO_PKG_VERSION` differs from the directory name unless the name starts with `scratch` (as R8.8).
- **Size**: measure the fresh directory; report total bytes and the largest files. The limit is 1 MiB (`GOLDEN_MAX_BYTES`, SF7). A `meta.db` of about 40 tables and their indexes is a few hundred KB.

### `justfile`

The `golden-dir` recipe of R8.8 runs, after the storage line:

```bash
CETRA_SERVER_GOLDEN_OUT="$root/fixtures/server/golden/{{name}}" cargo test -p cetra-server --all-features -- --ignored --exact golden_tests::write_server_golden_dir
```

and refuses to start when `fixtures/server/golden/{{name}}` exists, like the storage one. The `RELEASES` append stays once.

### `AGENTS.md`

Commands: `just golden-dir` "also writes `fixtures/server/golden/<name>/` (D-R-38)". Tests: `CETRA_SERVER_GOLDEN_OUT`; "a `meta.db` format change (spec 04 § Format versions) bumps `META_FORMAT_VERSION`; from the first public release the golden backups under `fixtures/server/golden/` must keep restoring". Layout: `fixtures/server/golden/`.

### Done when

`just check` green; `just golden-dir scratch` writes both directories; report the server one's size and file list; delete `fixtures/storage/golden/scratch` and `fixtures/server/golden/scratch` before the commit.

---

## SF7 — The golden restore check and the release rule

Commit: `server: restore every golden backup with the current build (SF7)`

**Edit:** `crates/server/src/golden_tests/mod.rs`, `justfile` (`check-golden`), `.gitattributes`, `docs/spec/04-metadata-db.md` and `docs/spec/12-operations.md` (markers).

### `restore_copy` and `check_dir`

```rust
/// Restores the golden directory `dir` into a fresh temp data directory by the procedure
/// of docs/self-hosting/backup.md § Restore, with the current build. `dir` is never written.
fn copy_files(dir: &Path) -> tempfile::TempDir;

/// Steps 5 and 6 of § Restore on a directory `copy_files` made.
async fn restore_steps(data_dir: &Path) -> Result<(), AdminError>;

/// `copy_files`, then `restore_steps`, unwrapped.
async fn restore_copy(dir: &Path) -> tempfile::TempDir;

/// Restores `dir`, then checks it against its expected.json.
async fn check_dir(dir: &Path);
```

`copy_files`: copy `meta.db`, `catalog.db`, `secret.key` and `objects/` (when present) into a new temp data directory, as § Restore step 3 says, and return it. `restore_steps`: with `config = test_support::config(data_dir)`, `admin::gc_orphans(&config, Duration::from_secs(3600), false, false)?` must report `missing == 0` (assert, with the directory name), then `admin::migrate(&config)?`. `restore_copy(dir)` used below is `copy_files` then `restore_steps(..).unwrap()`.

`check_dir` steps (each assertion message names the directory's basename and the step):

1. Parse `expected.json`; `generator` must be 1.
2. **Manifest.** `manifest.json` parses; `format` is 1; every key of SF5's list is present with its type; each `files` entry's `sha256` equals the file's in `dir`; `meta_format` equals `expected.meta_format`.
3. **Markers.** `PRAGMA user_version` of `dir/meta.db` (through a connection on a temp copy) equals `expected.meta_format`; of `catalog.db` equals the manifest's `storage_format`.
4. `restore_copy(dir)`. If `expected.meta_schema < meta::expected_version()`, the restored directory holds `meta.db.pre-m{meta_schema:04}`; otherwise no `meta.db.pre-m*` file. The same for `catalog.db.pre-m{catalog_schema:04}` against the manifest's `catalog_schema` and the storage's highest catalog migration (the highest 4-digit number prefix among the file names of `concat!(env!("CARGO_MANIFEST_DIR"), "/../storage/migrations/catalog")`).
5. **Rows.** Open the restored `meta.db` (`MetaDb::open_existing`). For every table of `expected.rows`, `SELECT <the expected columns, by name> FROM <table> ORDER BY 1, …, n` equals the expected rows. A missing table or column fails: `table {t} column {c} of the golden directory is gone: a format change (D-R-34)`. New columns and new tables are ignored (additive).
6. **Reads.** `crypto::load_key(None, restored, true)` gives `GOLDEN_KEY`; `verify_or_record` answers `Matched`; `dump(…)` of the restored directory equals `expected.json` on `reads`, `secrets`, `auth` and `encryption_check`.
7. **It serves.** `serve::bind(test_support::config(restored), None)` succeeds; `reqwest::get(format!("http://{}/readyz", running.local_addr))` answers 200 (`reqwest` is a dependency of the crate); `shutdown`.
8. **New writes on old data.** With the server stopped: create a project `golden-probe` with `meta/projects.rs::insert_project` in the golden org, and an LLM connection `golden-probe` with `meta/connections.rs::insert_connection` and a key sealed with `GOLDEN_KEY` (`SecretBox::seal`, with the AAD that function's callers use); run `admin::rotate_encryption_key(&config, None)` (the current key is a file, so the command generates the new key and writes `secret.key`); load the new key with `crypto::load_key(None, restored, true)`: every `secrets` entry still opens to its plaintext, and the probe's too.
9. **Socket lines.** For each line of `admin_socket.jsonl`: the request, with every `backup` request's `to` of `"/golden/out"` replaced by **one** temp path chosen once for the whole file (so the repeated backup finds it non-empty and answers `target_not_empty`, as recorded), is sent to a running server on the restored directory (one `bind` for all lines, sent in file order) and the response's `ok` and, when failed, `code` equal the recorded ones; and `decode_response` of the recorded response line succeeds (a client of this build reads a server of that release).

### Tests

- `a_fresh_golden_backup_restores`: the fresh directory, `check_dir`. **The test that exercises the machinery before anything is committed.**
- `the_committed_golden_backups_restore`: for every directory under `fixtures/server/golden/` (none before the first release: print that and pass), `check_dir`.
- `golden_backups_stay_small`: every committed directory and the fresh one are at most `GOLDEN_MAX_BYTES = 1 << 20`, with M8's message.
- `a_newer_golden_backup_is_refused`: for every committed directory, or the fresh one: `copy_files`; set the copy's `meta.db` `user_version` to `META_FORMAT_VERSION + 1` with a connection that sets no journal mode (the `check_file` options of SF1); take `dir_digest` of the copy; `restore_steps` returns `Err` whose text contains `format version`; `dir_digest` is unchanged (nothing ran). Then, on a fresh `copy_files`, insert an unknown `_sqlx_migrations` row instead: the text contains `applied migration`.
- `every_released_version_has_a_server_golden_directory`: each version line of `fixtures/storage/golden/RELEASES` (D-R-30: one list) has `fixtures/server/golden/<version>/expected.json` with that `workspace_version`.
- `every_server_golden_directory_is_released`: every directory under `fixtures/server/golden/` is listed in `RELEASES`.

### `justfile`, `.gitattributes`

- `check-golden` (R8.8): the `git diff` lines take `fixtures/storage/golden fixtures/server/golden` instead of the storage path alone.
- `.gitattributes`: `fixtures/server/golden/** -text -diff`.

### Spec

Delete "(SF7)" in spec 04 § Format versions and spec 12.

### Done when

`just check` green; report the run time of `check_dir` on the fresh directory (target: a few seconds) and the directory's size.

---

## SF8 — Docs closeout

Commit: `docs: server format stability closeout (SF8)`

**Edit:** `docs/self-hosting/upgrade.md`, `docs/spec/04-metadata-db.md`, `docs/spec/12-operations.md`, `docs/plans/server-format/README.md` (status line).

1. `docs/self-hosting/upgrade.md`, the Downgrade bullet: "Downgrading is not supported. An older version refuses data written by a newer one before it changes anything, with one of these messages: `meta.db has format version N…`, `meta.db has applied migration N, which this build does not have…`, `this data directory has format version N…`, `WAL segment … format N…`, `catalog.db has applied migration N…`. To go back, restore the pre-upgrade backup (or the `*.pre-m*` copies) and run the old binary." Keep the sentence about deletions after the upgrade, and replace "`reapply-deletions` cannot read a newer database" with "`reapply-deletions` reads a newer `meta.db` only when its format did not change". (M8 asks track N for the same bullet; if R8.8 or N has already rewritten it, merge the meta lines into theirs.)
2. Spec 04 and 12: no "(SF" marker left. Add to spec 04 § Format versions one sentence: "Before the first public release `META_FORMAT_VERSION` stays 1: a format change then needs no bump (D185)." (`grep -n "(SF" docs/spec` prints nothing).
3. This file's status line: `Status: done (SF1–SF8)` with the date.

### Done when

`just check` green; `grep -rn "(SF[0-9])" docs/spec` prints nothing.

---

## Requests (for the coordinator; not edited by this plan)

- **`docs/roadmap.md`:** the track R row gains "server format plan `server-format/`, SF1–SF8, D-R-33 to D-R-38" (the coordinator owns the row). The sub-phase names may become R's M9 if the coordinator prefers.
- **Track J** (migrations `0016`+): every new migration must pass SF2's lint, and one that alters a table needs a case in `migrations_that_alter_a_table_keep_its_rows`. A new TEXT enum value or JSON shape in an existing column after the first release bumps `META_FORMAT_VERSION` (D-R-34). Every new table gets a golden row (SF6's `every_table_has_a_golden_row`).
- **Track N** (`release-prep.sh`, `release-verify.sh`): M8's request covers `just golden-dir <version>`; `release-verify` also checks `fixtures/server/golden/<version>/expected.json`. `serve.rs::bind` gains one preflight line (SF1): keep it before `Storage::open` when rebasing.
- **`.github/workflows/ci.yml`:** M8's request stands and now covers both paths: a `just check-golden` step in one pull-request job, preceded by `git fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main` (the checkout is shallow), or `check-golden` never runs in CI.
- **CLI `credentials.toml`** (`crates/cli/src/credentials.rs`, on the user's machine) and **`cetra.toml`** are durable too and outside this plan: see Open questions.

## What not to build

- A manifest reader, a`cetra admin restore` or `verify-backup` command (user: no reader in the MVP).
- Online key rotation, a keyring, or a version byte in `secret.key` (D-R-35).
- A range of readable `meta.db` formats, or a reader for pre-release databases (D185; no compatibility layers).
- Golden directories for storage (M8's) or for the OTLP fixtures.
- A second protocol version for `admin.sock`: only the field and the refusal.

## Open questions

1. **Sub-phase naming.** `SF1`–`SF8` here; the coordinator may rename them R M9.
2. **`credentials.toml` and `cetra.toml`.** The CLI's credentials file and the server's config file outlive releases too. Proposal (not in this plan): config keys follow D-R-12's "a new key is not a format change"; renaming or removing a key after the first release needs an upgrade note; `credentials.toml` gains no version until its first change. Say if either needs a plan.
3. **Pre-release `meta.db` with data is refused (`PreFormat`)**, following the user's choice for storage directories (D-R-27). Existing dev and demo data directories (`~/cetra-demo` on the VM) stop opening after SF1 if they have users, as they already will after R8.2.
