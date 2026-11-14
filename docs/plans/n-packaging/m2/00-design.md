# N M2 — Online backups: design and plan

Track N's second milestone. M1 (`../README.md`) shipped the packaging and a documented **cold** snapshot
(stop, copy, start; D-N-11). M2 adds what spec 12 § Backups promised and what track Q needs for its
cells (Q's requests R9 and R10, `docs/plans/n-packaging/README.md` § Requests for Q):

- `llmobs admin backup --to <dir> [--meta-only]`: a consistent backup of a **running** server, taken
  without stopping it;
- object retention long enough that a metadata-only backup of an S3-mode server stays restorable
  against the live bucket;
- `llmobs admin gc-orphans`: after a restore, deletes the objects the restored catalog does not know
  and reports the ones it needs but cannot find;
- copies of `meta.db` and `catalog.db` taken automatically before migrations run.

The user chose every option below on 2026-11-14 (decisions D-N-17–D-N-24).

This file is the design and the phase list. One agent implements each phase file (`01-…` to `10-…`)
in one go, in order unless § Phases says two can run in parallel, using only this file and its own
phase file. Each phase ends with `just check` green and one commit.

## Read first (every phase)

- `AGENTS.md`, all of it.
- This file, all of it. Then your own phase file.
- `docs/spec/03-storage.md` § 1 (WAL), § 2 Flush, § 4 Catalog, § 5 (GC of obsolete files), § 9 Media.
- `docs/spec/12-operations.md` § Backups and § Upgrades.
- `docs/mvp-decisions.md` § Track N: D-N-11, D-N-12 and D-N-17–D-N-24.

## What exists (checked 2026-11-14 on `main` at `aa5527b`)

**The lock.** `llmobs serve` holds `<data_dir>/LOCK` (`crates/storage/src/lib.rs`, `lock_dir`). Another
process cannot open the storage engine while the server runs, so an online backup has to be done
*by* the server: the CLI only asks for it.

**Flush.** Each table (spans, scores) has one flush task (`crates/storage/src/flush.rs`, `spawn`, `run`).
- It freezes the active memtable when `flush_due` is true: a frozen memtable is waiting, the task is
  stopping, the active memtable is at least `flush.max_bytes`, or its oldest row is at least
  `flush.max_age` old.
- It writes Parquet files, commits them with `flushed_seq = view.max_seq` (`Committer::commit`), then
  truncates the WAL.
- Nothing can force a flush today. `FlushHandle::request_stop` makes the task flush once more, but the
  task then exits; shutdown uses it.
- The writer inserts a batch into the memtable before it acknowledges it (`wal/writer.rs`). So "the
  committed `flushed_seq` ≥ the memtable's largest sequence at time T" means every row acknowledged
  before T is in Parquet and the catalog, or was dropped at flush under a committed tombstone
  (`uncovered_rows`).
- `Committer::flushed_seq(table)` returns the committed value; `Memtable::active_max_seq()` exists,
  and the frozen view's `max_seq` has no getter.

**Catalog.** `catalog.db` has one SQLite connection, owned by `Committer` (`crates/storage/src/catalog/committer.rs`)
behind `tokio::sync::Mutex<CommitterInner>`, in WAL journal mode. `CommitterInner` holds the connection,
`catalog: Arc<Catalog>`, the `flushed` sequences, and `retired: HashMap<FileId, Arc<FileMeta>>`.

**Object deletion.** Every object the storage deletes goes through one of these:

| Path | Rule today | Pinned by a catalog `Arc`? |
|---|---|---|
| Obsolete data files: `gc::collect_obsolete` → `Committer::collect_obsolete_file` | `obsoleted_at ≤ now − gc.grace` **and** `Arc::strong_count(retired[id]) == 1` | **Yes**: a cloned `Arc<Catalog>` that still lists the file keeps it |
| Staged objects: `gc::collect_once` → `Committer::collect_staged_path` | older than 1 h and not in `data_files` or `media` | n/a (never in a catalog snapshot) |
| Stale media: `MediaGc::collect` → `Committer::collect_media` | `last_seen_at` older than 1 h and no `spans` row references it | **No** |
| Dropped-project media: `MediaGc::finish_drop` | immediately, outside the committer lock | **No** |

`Objects::delete` and `Objects::delete_len` treat "not found" as success, so GC runs cleanly over a
restored catalog whose obsolete files are already gone.

**`gc.grace`.** `GcConfig { grace }` (`crates/storage/src/config.rs`): default 5 min, validated to
1 s–24 h. It is the only rule for obsolete data files after a restart, because `retired` is in
memory only.

**WAL after a restore.** Replay starts the writer at `max(last segment seq, flushed_seq) + 1`
(`wal/replay.rs`). A data directory restored with `catalog.db` and an empty `wal/` therefore opens
correctly and never reuses a sequence number.

**Object keys.** Parquet: `tables/{table}/project={id}/day={YYYY-MM-DD}/{level}-{uuidv7}.parquet`.
Media: `media/{project}/{sha256}`. Both are relative to the store root: `<data_dir>/objects`, a
configured `file://` path, or the `s3://bucket/prefix` behind a `PrefixStore`. `Objects::raw_store()`
returns the `ObjectStore`, and `Objects::local_root()` returns the local root in local mode. Nothing
lists the store today.

## Design

### Trigger: a local control socket (D-N-17)

`llmobs serve` listens on a Unix socket at `<data_dir>/admin.sock`, created with mode `0600`.
- At bind time it removes a stale file at that path. That is safe because `LOCK` is already held.
- It removes the socket at shutdown.
- The protocol is one request per connection: the client writes one JSON line and reads one JSON
  line back.

```json
{"command":"backup","to":"/backups/2026-11-14","meta_only":false}
```
```json
{"ok":true,"report":{ ... BackupReport ... }}
{"ok":false,"code":"busy","message":"another backup is running"}
```

`llmobs admin backup` reads the config to find `data_dir`, connects to `<data_dir>/admin.sock`, sends
the request and prints the report. It is the only command on the socket in M2.

Why a socket:
- it works with `docker compose exec llmobs llmobs admin backup …` (Q's cells) and over SSH;
- authentication: the server checks the connecting process's uid (`SO_PEERCRED`, through
  `UnixStream::peer_cred`) and accepts only its own uid or root. Its own uid is the owner of the
  socket file it just bound, not the data directory's owner: a Kubernetes PVC root stays root's under
  `fsGroup`, and `kubectl exec` runs as the server's uid (review N-1). File permissions
  alone are not enough: macOS ignores them on `connect`;
- it adds no network surface and no token.

The backup is written by the server, so `<dir>` is a path on the **server's** filesystem, writable by
the server's user (uid 65532 in the image).

If the socket cannot be bound (a Unix socket path is limited to 108 bytes), the server logs a
warning and runs without it. `llmobs admin backup` then reports that no server answers.

### Consistency: flush, then snapshot under a pin (D-N-18)

`backup::run` (server) does, in order:

1. **Claim the single backup slot.** A `tokio::sync::Mutex<()>` in `AppState`, taken with `try_lock`;
   a second request answers `busy`.
2. **Prepare `<dir>`.**
   - It must be an absolute path (`bad_request`).
   - It must not be inside `data_dir` or a local object-store root, after canonicalising, symlinks
     included (`target_inside_data_dir`).
   - It must not exist, or must be an empty directory (`target_not_empty`).
   - It is created with mode `0700`; an existing empty directory is set to `0700`.
3. **`Storage::flush_now(timeout)`** (60 s). Both tables flush everything acknowledged before the call
   and wait for the commit. Ingestion continues: rows appended after the call stay in the WAL and are
   not in the backup. The backup's point in time is the flush.
4. **`meta.db`:** `VACUUM INTO '<dir>/meta.db'` on a `meta.db` reader connection. That gives a
   consistent copy while writes continue (WAL mode). `meta.db` is copied **before** `catalog.db`, so a
   deletion registered with storage between the two copies is in the catalog but not in `meta.db`.
   That is harmless: the storage applies its tombstone anyway. The reverse order could leave a `meta.db`
   row that believes a deletion is registered when the restored catalog does not have it.

   The cost is a small skew: `meta.db` is up to a few seconds newer than the data. A row written in
   that window can point at data the backup lacks, for example an evaluation whose judge score was
   acknowledged after the flush. The restored server treats it as it treats any reference to data
   not flushed yet.
5. **`Storage::snapshot_catalog(path)`**:
   - first takes the write side of a new `object_gc_gate: tokio::sync::RwLock<()>`, before the
     committer lock and never after it (lock order: gate, then committer);
   - then, under the committer lock, runs `VACUUM INTO '<dir>/catalog.db'` and clones
     `Arc<Catalog>`;
   - returns a `CatalogSnapshot` that holds the clone and the gate guard, and lists the snapshot's live
     objects (the active `data_files` paths and the `media` keys, read from the copied file).

   While the snapshot is alive, no object it lists can be deleted:
   - data files through the `retired` reference count, which exists today;
   - obsolete-file GC also takes the gate for read before deleting. That covers files the verify
     pass quarantines mid-backup: they leave the catalog without entering `retired`;
   - media because `collect_media` and `finish_drop` take the gate for read before deleting.

   Commits wait for the catalog `VACUUM INTO`, which runs under the committer lock. That is a known
   cost: it grows with the `data_files` and `media` rows, and on a large catalog flushes stall for its
   duration. Deletions wait for the whole backup, except `drop_project`, which leaves its media to the
   GC retry. The `Arc<Catalog>` pin is defence in depth: the gate alone already blocks every
   deletion.

   Compaction, the deletion rewriter, retention and flushes keep running. They add files and mark old
   ones obsolete, which the snapshot does not care about.
6. **Objects:**
   - **Local mode, full backup:** `Storage::copy_objects(&snapshot, <dir>/objects)` hard-links each
     listed object into the same relative path under `<dir>/objects`. When the backup directory is on
     another filesystem (`EXDEV`), it copies the object instead. Objects are immutable once published,
     so a hard link is a safe copy.
   - **S3 mode:** objects are not copied. The backup is metadata-only and relies on `gc.grace`
     (D-N-20) and, optionally, bucket versioning. `meta_only: true` is accepted in S3 mode; it is an
     error in local mode (`meta_only_needs_s3`), where it would make an unrestorable backup.
7. **`secret.key`:** if `<data_dir>/secret.key` exists (no `LLMOBS_ENCRYPTION_KEY`), it is copied to
   `<dir>/secret.key` with mode `0600`. With `LLMOBS_ENCRYPTION_KEY` set, the manifest says
   `"encryption_key": "environment"` and the operator keeps that key with the backup.
8. **`manifest.json`** (D-N-19), written last through a temporary file, then `fsync` and rename. A
   backup without `manifest.json` is incomplete.
9. **Drop the snapshot,** which releases the pin and the gate, then the slot. Answer with the
   `BackupReport`.

An error at any step removes nothing from `data_dir`. If the server created `<dir>`, it removes it
again (unlinking hard links is safe); a pre-existing empty `<dir>` is emptied. It answers
`ok: false` with the step's code.

### Backup layout and manifest (D-N-19)

```
<dir>/
  manifest.json
  meta.db
  catalog.db
  secret.key            (when the key is a file)
  objects/              (local mode only; the same relative keys as the store)
    tables/...
    media/...
```

`manifest.json`:

```json
{
  "format": 1,
  "llmobs_version": "0.1.0",
  "created_at": "2026-11-14T21:04:05.123456789Z",
  "mode": "full",
  "object_store": "local",
  "encryption_key": "file",
  "flushed_seq": {"spans": 1234, "scores": 56},
  "gc_grace_secs": 300,
  "restorable_until": null,
  "files": [
    {"path": "meta.db", "bytes": 1048576, "sha256": "…"},
    {"path": "catalog.db", "bytes": 65536, "sha256": "…"},
    {"path": "secret.key", "bytes": 45, "sha256": "…"}
  ],
  "objects": {"count": 812, "bytes": 3221225472}
}
```

- `mode`: `"full"` (local mode) or `"meta-only"` (S3 mode).
- `object_store`: `"local"` or `"s3"`. The bucket and prefix are not written: the restore uses the
  server's own config, and the manifest stays free of anything sensitive.
- `encryption_key`: `"file"` or `"environment"`.
- `restorable_until`: for `meta-only`, `created_at + gc.grace` in RFC 3339; `null` for `full`.
  It uses the `gc.grace` in force when the backup was taken; lowering `gc.grace` later shortens the
  window. It holds for data files. Media blobs follow their own rule: a blob is deleted an hour after no
  span references it. A deletion applied after the backup can therefore remove a blob that the
  backup still references. `gc-orphans` reports such blobs separately, as missing media: the
  attachments are lost, the traces are not.
- `objects`: the count and total bytes of the objects the snapshot lists. They are hard-linked or
  copied in `full` mode, and only listed in `meta-only` mode.
- Object files are not hashed: they are immutable, named by UUIDv7 or by SHA-256, and hashing
  hundreds of GB would make a backup slow.

`BackupReport` (the socket answer, printed by the CLI) has: `dir`, `mode`, `created_at`, `objects`,
`object_bytes`, `linked`, `copied` and `duration_ms`.

### S3 object retention: raise `gc.grace`'s cap (D-N-20)

- Spec 12's `gc.object_retention` is not added.
- `storage.gc.grace`'s maximum goes from 24 h to **30 days**; its default stays 5 min.
- A meta-only backup can be restored against the live bucket for `gc.grace` after it was taken: every
  file its catalog lists as active is either still active, or obsolete for less than `gc.grace`.
- Q sets `gc.grace = "8d"` for nightly backups kept a week.
- Privacy: data erased by a deletion stays in the obsolete files the rewriter replaced until GC
  removes them, that is for up to `gc.grace`. The self-hosting docs say so next to the advice to keep
  backups 30 days or less.

### Restore (D-N-21)

The restore is documented, not a command. Restoring is rare, needs the server stopped, and differs
between local and S3 mode; a command would hide steps the operator must see. With the server
stopped:

1. Keep the current data directory aside, for `reapply-deletions`.
2. Create an empty data directory. Copy `meta.db`, `catalog.db`, `secret.key` (if present) and, for a
   full backup, `objects/` into it (`cp -a` keeps the hard links' content). There is no `wal/` to
   copy: the server creates it, and the WAL resumes after the catalog's `flushed_seq`.
3. Run **`llmobs admin gc-orphans`**, with the server still stopped. It opens the storage (which takes
   `LOCK`) and lists the store:
   - it deletes every object under `tables/` and `media/` that the catalog does not know and that is
     older than `--min-age` (default `1h`);
   - it reports, as an error, every object the catalog lists as active that the store does not have.

   `--dry-run` only reports. A missing data file means the backup is older than `gc.grace`
   (meta-only), or `objects/` was not copied: the restore is incomplete, and the command exits 1.
   Missing media is reported as a warning.

   **In S3 mode this is a rollback of the live bucket.** Every object written after the backup is
   unknown to the restored catalog and is deleted permanently. Run it with `--dry-run` first.
4. If the backup is from an older version, run **`llmobs admin migrate`**: with the server stopped,
   it migrates `meta.db` and `catalog.db` and exits (added by B8).
5. Run `llmobs admin reapply-deletions --from <kept data directory>` (D-M-9). It re-opens every
   deletion the restored `meta.db` lacks, with its scope.
6. Only now start `llmobs serve`. The deletion worker registers the re-opened deletions with storage
   within seconds of the start. Starting before reapply would serve, evaluate and alert on erased
   data in that window (cross-track review X15). How evaluation results produced from restored data
   are cascaded is track M's (M7, M8).

`gc-orphans` with `--min-age` also works on a normal stopped installation, to clean what a crash left.
"Known" means a row in `data_files` (any state: active, obsolete or quarantined), `media` or
`staged_objects`. "Needed" means an active `data_files` row, or a `media` row of a project that is
not dropping or dropped. Temporary names (the private `is_temp_name` in `objects.rs`) are skipped.

### Pre-migration copies (D-N-22)

- **`meta.db`:** when `MetaDb::open` finds an existing file whose applied migration is older than the
  binary's, it runs `VACUUM INTO '<data_dir>/meta.db.pre-m<applied>'` before migrating, then
  deletes every other `meta.db.pre-*`.
- **`catalog.db`:** `open_catalog_db` does the same with `catalog.db.pre-m<applied>`.
- **Name:** `<name>.pre-m<applied>`, where `<applied>` is the highest migration applied before
  this start, zero-padded to 4 digits: `meta.db.pre-m0012`. It is not the package version, which
  stays `0.0.0` between releases while commit-pinned builds migrate (cross-track review X14).
- **Failed upgrades:** an existing `<name>.pre-m<applied>` is kept, never replaced. A half-failed
  migration leaves `<applied>` unchanged, so the retry keeps the good copy. A copy goes through a
  `.tmp` file and a rename.
- **When there is no copy:** a fresh data directory (no file yet) or a file already at the binary's
  version.

Spec 12 § Upgrades already describes this; M2 builds it.

### What M2 does not do

- No built-in schedule: cron, systemd timers or Q's own scheduler call `llmobs admin backup`
  (D-N-23).
- No restore command (D-N-21).
- No backup over HTTP and no remote target such as `s3://` for `--to`. Operators copy `<dir>` with
  restic, rclone or `aws s3 sync`.
- No incremental local backups beyond what hard links give: a second full backup on the same
  filesystem links the unchanged objects again and costs only the new ones.

## Decisions

| # | Decision |
|---|---|
| D-N-17 | **Online backups go through a Unix control socket** `<data_dir>/admin.sock` (mode 0600; one JSON line each way); `llmobs admin backup --to <dir> [--meta-only]` asks the running server, which writes the backup (user). |
| D-N-18 | **Flush, then a pinned snapshot.** Force-flush both tables and wait; `VACUUM INTO` `meta.db`, then `catalog.db` under the committer lock while cloning the catalog and taking an `object_gc_gate` (before the committer lock) that obsolete-file GC, media GC and dropped-project media deletion respect; hard-link (or copy) the snapshot's objects in local mode; copy `secret.key`; write `manifest.json` last. Compaction, deletion and retention keep running (user). |
| D-N-19 | **Backup layout:** `manifest.json` (format 1, version, time, mode, store kind, key source, flushed sequences, `gc_grace_secs`, `restorable_until`, SHA-256 of the database and key files, object count and bytes), `meta.db`, `catalog.db`, `secret.key`, `objects/`. One backup at a time (`busy`); the target must be absent or empty and outside `data_dir`. |
| D-N-20 | **`gc.grace` may be up to 30 days** (was 24 h), and is the retention that keeps a meta-only S3 backup restorable; no separate `gc.object_retention` key. Erased data stays in obsolete files for up to `gc.grace` (user). |
| D-N-21 | **Restore is documented, plus `llmobs admin gc-orphans [--min-age 1h] [--dry-run]`** (server stopped): it deletes unknown objects older than the minimum age and reports catalog data files missing from the store (exit 1) and missing media (a warning); in S3 mode it rolls the live bucket back to the backup (user). |
| D-N-22 | **Pre-migration copies:** `meta.db.pre-m<applied>` and `catalog.db.pre-m<applied>` (the highest migration applied before the upgrade; not the package version, which stays `0.0.0` between releases) by `VACUUM INTO` before migrations run; an existing copy of that name is kept; only the last pair remains. Restores re-apply deletions before the first start (`llmobs admin migrate` for older backups). |
| D-N-23 | **No built-in backup schedule;** external schedulers call the command (user). |
| D-N-24 | **`--meta-only` is S3-mode only, and S3-mode backups are always meta-only;** a local-mode `--meta-only` is refused because it would not be restorable. |

## Phases

| # | File | Adds | Crates | Needs |
|---|---|---|---|---|
| B1 | [`01-gc-grace.md`](01-gc-grace.md) | `gc.grace` up to 30 days | storage | — |
| B2 | [`02-flush-now.md`](02-flush-now.md) | `Storage::flush_now` | storage | — |
| B3 | [`03-catalog-snapshot.md`](03-catalog-snapshot.md) | `object_gc_gate`, `Storage::snapshot_catalog`, `CatalogSnapshot` | storage | — |
| B4 | [`04-copy-objects.md`](04-copy-objects.md) | `Storage::copy_objects` | storage | B3 |
| B5 | [`05-orphans.md`](05-orphans.md) | `Storage::collect_orphans` | storage | B3 |
| B6 | [`06-backup.md`](06-backup.md) | `server::backup::run`, `BackupReport`, the manifest | server | B2, B3, B4 |
| B7 | [`07-admin-socket.md`](07-admin-socket.md) | the control socket in `llmobs serve` | server | B6 |
| B8 | [`08-cli.md`](08-cli.md) | `llmobs admin backup`, `llmobs admin gc-orphans` | cli, server | B5, B7 |
| B9 | [`09-pre-migration.md`](09-pre-migration.md) | pre-migration copies | server, storage | — |
| B10 | [`10-docs.md`](10-docs.md) | self-hosting docs, spec 12, roadmap, changelog | docs | B1–B9 |

Phases that may run in parallel, in separate worktrees, are those that touch different files:
- B1, B2, B3 and B9 first;
- then B4 and B5 (both after B3);
- then B6, B7 and B8 in order;
- B10 last.

When two parallel phases add methods to `crates/storage/src/lib.rs`, merge the second onto the
first.

## Conventions

- **Code rules:** the `AGENTS.md` rules hold. No `unsafe`, no `unwrap`/`expect` outside tests,
  `thiserror` enums in library crates, logs carry ids and sizes and never paths' contents or
  secrets.
- **Tests:** every phase adds the tests its file lists. Storage tests use the crate's
  `test_support` helpers. Server tests use the existing `serve::bind` test pattern.
- **Errors:** each new storage operation returns its own error enum in `crates/storage/src/error.rs`
  (`FlushNowError`, `SnapshotError`, `CopyObjectsError`, `OrphanError`). The server's `BackupError`
  carries a stable `code()` string, the codes listed in § Design.
- **Decisions:** the plan commit adds D-N-17–D-N-24 to `docs/mvp-decisions.md`. Phases cite them
  and add none.

## How to verify (whole milestone)

1. `just check` passes after every phase.
2. After B8, by hand, with a release build (`just build-release`):
   1. Start `./target/release/llmobs serve` on a temporary data dir with `LLMOBS_INIT_*` set.
   2. Send spans (`tools/packaging/smoke.sh`).
   3. Run `./target/release/llmobs admin backup --to <tmp>/b1` while it runs.
   4. Stop the server and restore `<tmp>/b1` into a new data dir as § Restore says (`gc-orphans`
      reports 0 missing).
   5. Start it and read the trace back.
3. The B10 docs describe exactly that sequence.
