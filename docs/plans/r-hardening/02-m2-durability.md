# R · M2 — Durability: losing unsynced writes

The second milestone of track R (`00-design.md` § M2, decisions D-R-5, D-R-6, D-R-13). Today no test loses a write that was never fsynced: `crash_tests.rs` aborts the process, which keeps the OS page cache. M2 adds a seam that every local file call of `crates/storage` goes through (`src/fs.rs`), and a **lossy mode** behind it that can drop what a power loss may drop. It uses that mode in the M1 model test and in the hook-abort children, and it adds model cases that crash inside an operation, so every read is checked after such a crash. It then confirms and fixes the audit findings, makes every fsync failure fatal for its component (D-R-6), and cross-checks the shim against Jepsen's LazyFS on the real binary.

**What it proves.**
- In `wal.sync = "always"`, an acknowledged write survives a power loss at every point the tests reach. In interval mode, what survives is a prefix of each WAL (stated exactly in R2.9).
- No crash state leaves behind a file the catalog doesn't know, a segment that replay refuses, or a cache entry that serves wrong bytes.
- A failed fsync stops its component instead of being retried.

Sub-phases R2.1–R2.14, one commit each. R2.14 can produce several commits, one per bug found.

## Conventions for every sub-phase

- **`just check` passes before each commit.** Node comes from nvm: run `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` first (if that version is gone, `ls ~/.local/share/nvm/` for the current one). One commit per sub-phase, with the subject given.
- **Scope.** Edit only:
  - `crates/storage/` (all of it: the seam touches most modules);
  - `tools/lazyfs/` (new, R2.12);
  - `justfile` (R2.10, R2.11, R2.12);
  - `AGENTS.md` § Commands and § Tests, for the recipes and env vars this milestone adds (R2.10, R2.12);
  - `docs/`: spec 03, `docs/research/storage/hardening.md`, and `docs/roadmap.md` row R in R2.14;
  - `CHANGELOG.md` § `[Unreleased]`: the sub-phases that change user-visible behaviour add a line there (R2.3, R2.4, R2.5 and R2.6 under `Fixed`; R2.7 and R2.8 under `Added`). Each sub-phase gives the exact line.

  **Not** `crates/server/`: the `/readyz` change of R2.7–R2.8 is requested from the coordinating session (R2.7 § Requested outside R). **Not** `.github/`: the LazyFS probe of R2.12 runs on a throwaway branch, and the nightly workflow belongs to M3.
- **No new dependency.** Everything needed is already a dependency of `cetra-storage`: `rustix` (statvfs), `crc32c`, `sqlx`, `tempfile`, `proptest`. The lossy crash's RNG is a 10-line SplitMix64 in `fs/lossy.rs`, not `rand`.
- **No production cost (`00-design.md` § Cross-cutting rules).** Every `crate::fs` function is `#[inline]`. Outside `cfg(test)` it is the `std::fs`/`rustix` call and nothing else. Lossy mode and its registry are `#[cfg(test)]`, like `hooks.rs`.
- **Errors.** New variants go in `error.rs` with `thiserror` and a doc line each. Their messages add no payloads, user data or ids beyond what the enum already prints (`AGENTS.md` § Privacy).
- **Logging.** A failure that stops a component logs once at `error`, with the component and the action (`"object store stopped after a failed fsync"`) and never a payload. Object keys contain project ids and media keys contain blob hashes, so keys are logged at `debug` only, as `gc.rs` does.
- **Panics.** None in production code (`AGENTS.md`). Test code may `unwrap`/`expect`. Lossy mode panics on an operation it does not model, with a message that names the operation and says "extend fs::lossy". An unmodelled path then fails loudly instead of being silently ignored.
- **Decided by the user (2026-11-15).**
  - The R2.12 FUSE probe may push the throwaway branch `claude/r-lazyfs-probe`, with a `workflow_dispatch`-only workflow, and then delete it.
  - If the probe shows that FUSE works on GitHub runners, M3's nightly workflow runs `just lazyfs`.
  - When the directory fsync of a WAL truncation fails, the flush task retries it, and the spec documents why that is safe (R2.13).
  - Startup fsyncs every object directory, in one walk (R2.3).
  - `just test` runs one lossy seed per hook point; `CETRA_LOSSY_SEEDS` raises it (R2.10).
- **Report at the end of each sub-phase:**
  - what was done;
  - any deviation from this plan, and why;
  - for a fix: the test name and how it failed before the fix (paste the assertion message);
  - anything left undone or unverified.

### Rebase point (N's M2)

Track N's M2 (online backups, `docs/plans/n-packaging/m2/`) adds file operations to this crate (`00-design.md` § Other tracks in `crates/storage`). R2.1 starts with `git fetch origin && git rebase origin/main`, then checks which of these commits the branch now has, with `git log --oneline | grep -F '<subject>'`:

| N sub-phase | Commit subject | File operations it adds |
|---|---|---|
| B3 | `Add catalog snapshots for online backups` | `VACUUM INTO` (SQLite, outside the shim) |
| B4 | `Add Storage::copy_objects` | `create_dir_all`, `hard_link`, `copy` + `sync_all`, a final fsync walk of `dest` (`src/snapshot.rs`) |
| B5 | `Add Storage::collect_orphans` | lists the store (a `LocalFileSystem` read), deletes through `Objects::delete` |
| B9 | `Copy the databases before migrating them` | `VACUUM INTO <name>.tmp`, `rename` to `<name>.pre-<version>`, `remove_file` of older copies (`catalog/db.rs`) |

For each one:
- **Merged:** R2.1 migrates its calls (the rows marked "N B…" in § Inventory), and R2.13 fills in its row of the § Durability table.
- **Not merged:** there is nothing to migrate. The R2.1 report lists the missing ones, and the coordinator tells N's implementer to call `crate::fs` instead of `std::fs`. The functions R2.1 adds have the same names and signatures as `std::fs`. R2.13 still writes their § Durability rows, marked "(N M2, not merged yet)".

If B4 is merged, `copy_objects` writes outside the data directory. Its calls go through the seam, but lossy mode records them only when a test registers `dest` as a lossy root.

If B9 is merged, R2.3 adds `fs::sync_dir(dir)` after its rename: the pre-migration copy must be durable before the migration commits. Its test is `a_pre_migration_copy_survives_a_lossy_crash`: open an older catalog with lossy mode on, `crash_with(DropAll, 0)`, and check that the copy exists.

---

## R2.1 — The `fs` seam

Commit: `storage: route every local file call through crate::fs (R2.1)`

A pure refactor: no behaviour changes, and every existing test passes unchanged.

### `crates/storage/src/fs.rs`

`lib.rs` gets `pub(crate) mod fs;` next to `pub(crate) mod disk;`. The module is the seam (D-R-5): free functions with the names and signatures of `std::fs`, plus two concrete types. There is no trait.

```rust
//! Every local file call of the storage engine (track R, D-R-5).
//!
//! Outside tests each function is the `std::fs` call it is named after. Under
//! `cfg(test)`, a test can register a data directory in lossy mode
//! (`lossy::Lossy`): calls on paths under it are recorded, and a later lossy
//! crash drops what a power loss may drop. SQLite and DataFusion do their own
//! I/O and are not seen here.

/// `std::fs::File`, recorded in lossy mode.
#[derive(Debug)]
pub(crate) struct File {
    inner: std::fs::File,
    #[cfg(test)]
    tracked: Option<lossy::Tracked>,
}

/// `std::fs::OpenOptions`, with only the flags this crate sets.
#[derive(Debug, Clone, Default)]
pub(crate) struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    pub(crate) fn new() -> Self;
    /// `write`, `append`, `create` and `create_new` have the same shape.
    pub(crate) fn read(&mut self, on: bool) -> &mut Self;
    pub(crate) fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File>;
}

impl File {
    /// Read-only.
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> io::Result<File>;
    pub(crate) fn sync_all(&self) -> io::Result<()>;
    pub(crate) fn sync_data(&self) -> io::Result<()>;
    pub(crate) fn set_len(&self, len: u64) -> io::Result<()>;
    pub(crate) fn metadata(&self) -> io::Result<std::fs::Metadata>;
    pub(crate) fn try_lock(&self) -> Result<(), std::fs::TryLockError>;
    /// For reading only: the remote multipart upload streams the spill file
    /// through `tokio::fs::File`. Writes through the result are not recorded.
    pub(crate) fn into_std(self) -> std::fs::File;
}
impl io::Write for File { /* write, flush */ }
impl io::Read for File { /* … */ }
impl io::Seek for File { /* … */ }

pub(crate) fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()>;
pub(crate) fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()>;
pub(crate) fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()>;
pub(crate) fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()>;
/// Walks the tree: `remove_file` on files, then `remove_dir` bottom-up, so
/// lossy mode records each entry.
pub(crate) fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()>;
pub(crate) fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()>;
/// Creates or truncates, then `write_all`, like `std::fs::write`.
pub(crate) fn write<P: AsRef<Path>, B: AsRef<[u8]>>(path: P, bytes: B) -> io::Result<()>;
pub(crate) fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>>;
pub(crate) fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String>;
pub(crate) fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<std::fs::ReadDir>;
pub(crate) fn metadata<P: AsRef<Path>>(path: P) -> io::Result<std::fs::Metadata>;
pub(crate) fn try_exists<P: AsRef<Path>>(path: P) -> io::Result<bool>;
pub(crate) fn canonicalize<P: AsRef<Path>>(path: P) -> io::Result<PathBuf>;
/// Opens the directory and `sync_all`s it, which makes its entries durable.
pub(crate) fn sync_dir<P: AsRef<Path>>(path: P) -> io::Result<()>;
/// `statvfs`: free (`f_bavail * f_frsize`) and total (`f_blocks * f_frsize`) bytes.
pub(crate) fn space<P: AsRef<Path>>(path: P) -> io::Result<Space>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Space {
    pub(crate) free: u64,
    pub(crate) total: u64,
}
```

The generic bounds match `std::fs`, so call sites that pass a `&str`, a `PathBuf` or a `&Path` compile unchanged.

**No dead code in any commit.** Add a function, method or `OpenOptions` flag only when a call site in the inventory uses it. `lock_dir` today calls `.truncate(false)`, which is the default and is only there for clippy's `suspicious_open_options`. Drop that call rather than add a `truncate` method: inside `OpenOptions::open`, pass `.truncate(false)` to the `std::fs::OpenOptions` explicitly whenever `create` is set. The flags in use are `read`, `write`, `append`, `create` and `create_new`. If clippy reports a seam item as unused, delete the item; don't silence the lint.

Add only what the inventory below needs. If B4 is merged, also add `hard_link(src, dst)` and `copy(src, dst) -> io::Result<u64>`. In R2.1 the `cfg(test)` field `tracked` is always `None`, and `lossy` does not exist yet. The `#[cfg(test)]` branches go into the functions in R2.2, not now (no placeholders): until then, leave the `tracked` field out too and add it in R2.2.

`remove_dir_all` keeps the semantics of `std::fs::remove_dir_all` for its callers: a missing path is `NotFound`, and symlinks are removed, not followed (use `symlink_metadata`).

`wal::sync_dir` is deleted. Its callers (`wal/segment.rs`, `wal/mod.rs`, `lib.rs`, `objects.rs`) call `crate::fs::sync_dir`. `disk::free_bytes` and `disk::total_bytes_io` call `fs::space`.

### Inventory: every call migrated

Production code only. **Test code keeps `std::fs`**: a test that writes into a lossy directory directly bypasses the shim on purpose, to plant a state.

A module that switches to `use crate::fs;` would change what `fs::…` means inside its `mod tests` (which does `use super::*;`). Test writes would then go through the shim without anyone noticing. So in every migrated module:
- the production code calls `crate::fs::…`, imported as `use crate::fs;`;
- its `mod tests` adds `use std::fs;` explicitly, which shadows the glob import, or calls `std::fs::…` by full path. Check every `fs::` in the module's tests after the change.

A test that should go through the shim (R2.2–R2.12) calls `crate::fs::…` by full path, so the reader can see it.

Find the call sites with:

```bash
grep -rnE 'std::fs|tokio::fs|use std::fs|fs::(File|OpenOptions)|OpenOptions::new|rustix::fs::statvfs|LocalFileSystem|\.delete\(&location\)|\.head\(&location\)' crates/storage/src --include=*.rs
```

| Module | Call sites | Becomes |
|---|---|---|
| `wal/mod.rs` | `sync_dir`; `truncate_segments` (`remove_file`); `list_segment_files` (`read_dir`) | `fs::sync_dir`, `fs::remove_file`, `fs::read_dir` |
| `wal/segment.rs` | `open_segment` (`OpenOptions` append + `create_new`, `metadata`, reopen); `SegmentWriter { file: std::fs::File }`; `write_all`, `sync_data`, `set_len`, `sync_all` | `fs::OpenOptions`, `fs::File`, `fs::metadata` |
| `wal/writer.rs` | `discard_unacked` (`remove_file`); `truncate_path` (`OpenOptions`, `set_len`, `sync_all`) | the same names in `fs` |
| `wal/replay.rs` | `replay` (`std::fs::read`); `truncate_tail` (`OpenOptions`, `set_len`, `sync_all`); `list_segments` (`read_dir`) | the same names in `fs` |
| `lib.rs` | `ensure_dir` (`create_dir`, `sync_dir`); `lock_dir` (`OpenOptions` create, `try_lock`, `sync_dir`); `remove_spill_files` (`read_dir`, `remove_dir_all`, `remove_file`); `Storage { lock: Mutex<Option<std::fs::File>> }` | `fs::…`. The lock field holds an `fs::File` |
| `objects.rs` | `prepare_local_root`, `create_dir_chain`, `create_dir_chain_from` (`try_exists`, `is_dir`, `create_dir`, `sync_dir`, `canonicalize`); `create_output` (both kinds: `OpenOptions` `create_new`; remote `tokio::fs::create_dir_all` → `fs::create_dir_all` inside the existing `spawn_blocking`); `publish_output` (`metadata`, `seek`, `read_exact`; `tokio::fs::File::from_std(file)` → `from_std(file.into_std())`; `fs::File::open` of the spill file); `discard_output` and `remove_temp_path`; `remove_temp_files` and `remove_temps` (`read_dir`, `remove_file`); `put_local` (`OpenOptions`, `write_all`); `publish_local` (`sync_all`, `try_exists`, `remove_file`, `rename`, `sync_dir`); `try_exists` | `fs::…`. The file of `create_output`'s result and `publish_output`'s parameter become `fs::File` |
| `objects.rs`, **local deletes** | `Objects::delete` and `delete_len` call `self.store.delete`/`head`. In local mode that is `LocalFileSystem::delete` (`std::fs::remove_file`, with `automatic_cleanup` off) and `head` (a stat) | For `Kind::Local`, on `spawn_blocking`: `fs::metadata(root.join(path))` for the size (`NotFound` → 0), then `fs::remove_file`. `NotFound` is `Ok`, including when the parent directory is missing too. `Kind::Remote` is unchanged. Without this the shim cannot see GC's unlinks (audit item 2) |
| `compaction/job.rs`, `compaction/merge.rs`, `parquet.rs` | the output file passed to `FileWriter<W: Write + Send>`; `seek`/`read_exact` of its tail | The type follows `create_output`; no call changes |
| `read_cache.rs` | `ReadCache::open` (`create_dir_all`, `read_dir`, `read_to_string`, `metadata`); `set_size`; `write_blocks`; `read_block` (`fs::read`); `write_renamed` (`fs::write`, `rename`); `remove_file`; `remove_dir` (`remove_dir_all`); `insert_file(&mut fs::File)` | `fs::…` |
| `disk.rs` | `rustix::fs::statvfs`, twice | `fs::space` |
| `catalog/db.rs` (N B9, if merged) | `try_exists`, `remove_file`, `rename`, `read_dir` around `VACUUM INTO` | `fs::…` |
| `snapshot.rs` (N B4, if merged) | `create_dir_all`, `hard_link`, `copy`, `File::open(dst)?.sync_all()`, the final directory fsync walk | `fs::…` |

**Not in the seam** (each one is listed in R2.13's gaps):
- SQLite: `catalog.db`, `meta.db`, `VACUUM INTO`;
- DataFusion's own reads of Parquet files through `LocalFileSystem`, and its spill files under `tmp/query`;
- `object_store`'s HTTP client.

Reads by DataFusion don't matter to lossy mode. A lossy crash runs with the storage closed (R2.2), so the next life reads the post-crash state from the real directory.

After the migration, the grep above may only return:
- test code and doc comments;
- the `LocalFileSystem::new_with_prefix` registration in `prepare_local_root` (used for reads only);
- `fs.rs` itself.

Paste the remaining hits in the report.

### Tests

None new: nothing changes behaviour. `just check` runs the whole suite. That includes `objects.rs`'s local delete tests over the new local delete path (`objects.delete(&key)` twice is `Ok` both times).

---

## R2.2 — Lossy mode

Commit: `storage: add the lossy mode of the fs seam (R2.2)`

### Model (after RocksDB's `FaultInjectionTestFS`)

`crates/storage/src/fs/lossy.rs` is declared as `#[cfg(test)] pub(crate) mod lossy;` in `fs.rs`. Everything in it is test code.

A test **registers a root**: the data directory it owns, plus a **shadow directory** on the same filesystem and outside the root. From then on, every `crate::fs` call on a path under the root is recorded, in this process's memory, until the registration is dropped.

**The lossy shim serves in-crate tests only.** It exists under `cfg(test)`. `crates/storage/tests/crash.rs` is an integration test, linked against the library built without `cfg(test)`, so it can't use the shim. LazyFS covers it, and the real binary (R2.12).

**Ino.** An `Ino` is a per-root counter (`u64`), assigned on create and on adopt, never a real inode number. Iteration order and RNG draws then depend only on the history, so a seed always gives the same state. A file's identity in the shim comes from the `names` map.

What is recorded, in one `State` per root:
- **Names.** `names: BTreeMap<PathBuf, Ino>`, from root-relative path to inode id: the shim's view of which file each name refers to. Every operation that adds, moves or removes a name updates it: create, rename, unlink, link, `rmdir`, and each step of `remove_dir_all`. Every call that takes a path resolves the inode through this map, so a file reopened by path is the same inode as before: `open`, `metadata`, `rename`, `remove_file`, and a later `sync_all` through the new handle. R2.4's reopen test, R2.9's model and B4's `File::open(dst)?.sync_all()` all depend on that. A name the map doesn't know is adopted (below).
- **Per file (inode):**
  - `synced_len` and `len`;
  - the unsynced writes, as `(start, end)` ranges with a poisoned flag each;
  - `path: Option<PathBuf>`, its current name, or `None` once it has no name in the shim's view.
- **Per directory incarnation.** `dirs: BTreeMap<PathBuf, DirId>` maps a directory path to its current incarnation. `create_dir` gives the new directory a fresh `DirId`, and `rmdir` ends it, so a directory removed and created again at the same path is two incarnations. Each incarnation has its pending entry operations since its last `sync_dir`, in order, each with a global sequence number and a poisoned flag:
  - `Create { name, ino }`;
  - `Mkdir { name, child: DirId }`;
  - `Rename { from, to, ino, replaced: Option<(Ino, ShadowName)> }`;
  - `Unlink { name, ino, shadow }`;
  - `Rmdir { name, child: DirId }`;
  - `Link { name, ino }`, with N B4 merged.

  `sync_dir(path)` applies to the current incarnation of `path`. A directory that existed before registration, or that survived a crash, gets an incarnation on first touch.

Rules:
- **Writes are appends.** A write must start at the file's current length: `stream_position()` before the write, or `metadata().len()` for an append-mode file. Anything else panics with "lossy fs: non-append write at <path>; extend fs::lossy". Every write path of the crate appends: WAL segments, temp objects through `FileWriter`, cache blocks, and `fs::write` of a new file.
- **`set_len(n)`:**
  - below `synced_len`, it is durable at once (`synced_len = len = n`, unsynced writes dropped). This is an approximation, and a safe one here: the crate always fsyncs right after such a `set_len` (`rewind`, `truncate_path`, `truncate_tail`);
  - between `synced_len` and `len`, it drops every unsynced write that starts at or after `n`, and cuts the one that straddles `n` to `(start, n)`;
  - above `len`, it panics.

  `fs::write` on an existing file is `set_len(0)` followed by the write.
- **`sync_all` and `sync_data`** make the writes of the file's inode durable (`synced_len = len`). That holds whichever handle syncs, since the inode is resolved by name. They don't make the file's directory entry durable: strict POSIX, as ALICE and RocksDB assume.
- **`sync_dir(d)`** makes every pending entry operation of `d`'s current incarnation durable.
- **Renames stay within one directory.** Every rename of the crate is temp → final in the same directory. A cross-directory rename panics.
- **Unlinks keep the inode.** Before the real `remove_file`, the shim hard-links the file into the shadow directory (`<shadow>/<ino>-<n>`), so a crash that undoes the unlink can link it back. The same goes for the file a rename replaces.
- **`fs::copy`** (B4) is recorded as a `Create` of the destination plus one write of `0..n`, where `n` is the byte count `std::fs::copy` returns.
- **A failed sync poisons.** `Lossy::fail_next_sync(prefix)` makes the next file or directory sync under `prefix` return `io::Error::from_raw_os_error(5)` (EIO). The writes or entry operations pending at that moment are marked poisoned: no later successful sync makes them durable, and every crash drops them. That is Linux after a writeback error (fsyncgate).
- **Files the shim has never seen** are adopted on first touch as fully synced: they existed before the registration, or they survived a crash reset. SQLite's files are never touched, so a crash never changes them.
- **The root itself** is created by `enable`, which also syncs its parent. Operations on the root path itself are not recorded.

The process-wide registry is `static ROOTS: Mutex<Vec<Arc<Root>>>`, holding each root's canonical path. To find a call's root, canonicalize the nearest ancestor of the call's path that exists, then append the remaining components. A new file's path can't be canonicalized itself, and `Objects` canonicalizes its root while the WAL code doesn't, so both spellings must match the same root. The call belongs to the root that is a prefix of the result.

Each `Root` has one `Mutex<State>`, held across the real operation, fsyncs included. Tests therefore serialize their file calls per root: at most one operation per root is ever in flight, and the recorded order is the real order.

`File` carries `tracked: Option<Tracked>`, with `Tracked { root: Arc<Root>, ino: Ino }`. `Root::open_files` counts the live tracked files: `Tracked::new` increments it, and **`impl Drop for Tracked`** decrements it. The decrement is not in `File`'s `Drop`: `File` has no `Drop` impl, so `into_std` can move `inner` out (a `Drop` impl on `File` would make that error E0509).

### A crash in a process that aborts

The lossy state lives in the memory of the process that wrote. A hook-abort child (R2.10, R2.11) dies at `std::process::abort()`, and its parent has no record of what was unsynced. So **the child applies the crash itself, just before it aborts**:
- `Lossy::crash_on_abort(&self, seed: u64)` stores the seed in the `Root`.
- In `hooks.rs`, the abort branch of `Hooks::pause` (the `if aborting` block) calls `crate::fs::lossy::crash_before_abort()` right before `std::process::abort()`.
- `crash_before_abort` takes, for every registered root with a stored seed, that root's `State` mutex and keeps it. A thread inside a shim operation finishes the operation first, since it holds the mutex. Every later shim call blocks on the mutex until the process dies, so nothing changes the directory while the crash is applied.
- It then applies `crash_with(CrashMode::from_seed(seed), seed)` in the mode that allows open files (`apply(…, allow_open: true)`): those files are never used again. It prints the `CrashReport` to stderr (`eprintln!("lossy crash before abort: {report:?}")`), so the parent's output shows it.
- Its signature is `pub(crate) fn crash_before_abort() -> Vec<MutexGuard<'static, State>>`. The guards it returns stay held until the process dies: `Hooks::pause` keeps them in a local variable and calls `std::process::abort()`. Roots live in the `static` registry as `Arc`s, so the guards can be `'static`: hold an `Arc<Root>` clone in a `static` vector, or use `std::mem::forget` on the `Arc`. Choose the simpler one and say which in the report. A unit test can drop the guards instead of aborting.
- `Hooks::pause` is never called from inside a shim operation, so the aborting thread never already holds a root's mutex.

SQLite keeps writing `catalog.db` on other threads during this: the crash doesn't touch SQLite's files, and SQLite's own durability is what LazyFS checks. Because the crash runs under the mutex before the abort, there is no journal, no recovery step, and no operation half-recorded at the abort.

### API

```rust
/// A registered lossy root. Dropping it unregisters the root.
pub(crate) struct Lossy {
    root: Arc<Root>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashMode {
    /// Every unsynced write and every pending entry operation is lost.
    DropAll,
    /// Per directory, a random prefix of its pending operations is kept. Per
    /// file, each unsynced write is kept or dropped at random; a dropped write
    /// followed by a kept one leaves zeros.
    KeepRandom,
    /// Every pending entry operation is kept, and every unsynced write too,
    /// except each file's last one, which is cut to a random prefix (0 bytes
    /// included). This is the ext4 delayed-allocation state: names durable,
    /// data torn.
    TearLast,
}

#[derive(Debug)]
pub(crate) struct CrashReport {
    pub(crate) mode: CrashMode,
    pub(crate) dropped_writes: usize,
    pub(crate) torn_writes: usize,
    pub(crate) dropped_entries: usize,
}

impl Lossy {
    /// Creates `dir` (syncing its parent) and `shadow`, and registers `dir`.
    /// Panics if `shadow` is inside `dir`, or if `dir` is already registered.
    pub(crate) fn enable(dir: &Path, shadow: &Path) -> Lossy;
    /// Makes the next hook abort of this process (`Hooks::abort_at`) apply
    /// `crash(seed)` to this root first.
    pub(crate) fn crash_on_abort(&self, seed: u64);
    /// `crash_with(CrashMode::from_seed(seed), seed)`.
    pub(crate) fn crash(&self, seed: u64) -> CrashReport;
    /// Applies a crash state to the real directory, then forgets everything:
    /// what is left on disk counts as synced. Panics if a tracked file is
    /// still open (the storage must be closed first).
    pub(crate) fn crash_with(&self, mode: CrashMode, seed: u64) -> CrashReport;
    /// The next sync of a file or directory under `prefix` fails with EIO.
    pub(crate) fn fail_next_sync(&self, prefix: &Path);
}

impl CrashMode {
    /// `[DropAll, KeepRandom, TearLast][seed % 3]`.
    pub(crate) fn from_seed(seed: u64) -> Self;
}
```

### Applying a crash to the real directory

`crash_with` runs with the storage closed, so nothing else touches the directory. It uses `std::fs` directly:
1. **Choose,** with `SplitMix64::new(seed)`. Iterate incarnations and files in sorted order (`BTreeMap`), so a seed always gives the same state.
   - **Per incarnation:** how many pending operations to keep, as a prefix. `DropAll` keeps none, `KeepRandom` keeps a count uniform in `0..=n`, and `TearLast` keeps all `n`. The prefix always stops before the first poisoned operation.
   - **Causal rules.** Apply these until nothing changes; each pass only shortens prefixes, so it terminates. They hold for `TearLast` too.
     - If a `Mkdir { child }` is dropped, everything in `child`'s incarnation is dropped, and so on down the subtree: a lost directory entry loses the subtree.
     - An `Rmdir { child }` is kept only when every operation of `child`'s incarnation is kept: a directory can only have been removed once it was empty. Otherwise, cut the parent's prefix to just before that `Rmdir`.
   - **Per file:** which unsynced writes to keep. `DropAll` keeps none, `KeepRandom` flips a coin for each, and `TearLast` keeps all of them with the last one cut to `rng % (len + 1)` bytes. Poisoned writes are always dropped.
2. **Undo** the dropped entry operations in reverse global sequence order, updating `names`, `dirs` and each inode's `path` as it goes:
   - `Create` → `remove_file(dir/name)`;
   - `Mkdir` → `remove_dir_all(dir/name)`. Then every inode whose path is under it gets `path = None`, and its `names` entries go;
   - `Rename { from, to, replaced }` → `rename(to, from)`, then `hard_link(shadow, to)` when it replaced a file;
   - `Unlink { name, shadow }` → `hard_link(shadow, dir/name)`;
   - `Rmdir` → `create_dir`;
   - `Link { name }` → `remove_file`.

   An undo step that gets `NotFound`, because an earlier undo already removed its parent directory, is not an error: skip it. Any other error panics with the step and the path.
3. **Cut data.** For every inode whose `path` is `Some` and that exists on disk:
   - `set_len(new_len)`, where `new_len` is the end of its last kept write (possibly cut), or `synced_len` when it keeps none;
   - overwrite each dropped write below `new_len` with zeros (`std::os::unix::fs::FileExt::write_all_at`).
4. **Reset.** Delete the shadow files and clear the state; the `Ino` counter keeps counting. The root stays registered. (On the abort path, skip the reset: the process is about to die.)

### Tests (in `fs/lossy.rs`)

Each test uses a `tempfile::TempDir` `t`, with root `t/data` and shadow `t/shadow`, and calls `crate::fs` functions directly.
- `drop_all_truncates_to_the_synced_length`: write `"abc"`, `sync_all`, write `"def"`. After `DropAll`, the file is `"abc"`, and the report is `mode == DropAll`, `dropped_writes == 1`, `torn_writes == 0`, `dropped_entries == 0`. Clippy runs with `-D warnings`, so every `CrashReport` field must be read by some test: this one reads them all.
- `an_unsynced_create_is_lost_and_a_synced_one_stays`: in `data/d`, created and synced before, create `a` and `sync_all` it, then `sync_dir(d)`. Then create `b` with no `sync_dir`. After `DropAll`, `a` exists, `b` does not, and `dropped_entries == 1`.
- `an_unsynced_unlink_is_undone_with_its_content`: create a file, write `"x"`, `sync_all`, `sync_dir`, then `remove_file`. After `DropAll` the file is back with `"x"`.
- `an_unsynced_rename_over_a_file_restores_both`: file `f` synced with `"old"`, temp synced with `"new"`, both entries synced, then `rename(temp, f)` with no `sync_dir`. After `DropAll`, `f` is `"old"` and temp is `"new"`.
- `a_lost_directory_loses_its_synced_files`: `create_dir(data/d)` without syncing `data`; in `d`, create a file, `sync_all`, `sync_dir(d)`. After `DropAll`, `d` is gone.
- `tear_last_keeps_names_and_cuts_the_last_write`: for seeds `0..32`, a new file gets the writes `"aaaa"` and `"bbbb"` with no sync. After `TearLast`:
  - the file exists and starts with `"aaaa"`;
  - its length is in `4..=8`;
  - its tail is a prefix of `"bbbb"`.
- `keep_random_leaves_zeros_for_a_dropped_write`: with two 4-byte writes, look for a seed in `0..256` whose crash drops the first write and keeps the second. Assert that at least one seed does, and that the file then starts with 4 zero bytes.
- `a_failed_sync_poisons_the_writes`: write, `fail_next_sync(path)`. The first `sync_all` returns `Err` with `raw_os_error() == Some(5)`, and a second `sync_all` returns `Ok`. After `DropAll` the file is empty, and also after `TearLast`.
- `the_same_seed_gives_the_same_state`: build the same history (3 files, 2 directories) under two roots, `crash(7)` each, and compare the two trees byte for byte.
- `crash_before_abort_works_with_open_files`: open a file, write `"abc"`, `sync_all`, write `"def"`, and keep the handle open. Then `crash_on_abort(0)` (`DropAll`), and `let guards = crash_before_abort();`. On disk the file is `"abc"`. Drop the guards, and a shim call on that root works again.
- `crash_with_an_open_file_panics`: `#[should_panic(expected = "open")]`.
- `non_append_writes_panic`: after a write, seek to 0 and write again. `#[should_panic(expected = "non-append")]`.
- `reopened_file_keeps_its_unsynced_state`: create `f`, write `"abc"`, and drop the handle. Reopen `f` by path with `fs::OpenOptions::new().write(true)`, `sync_all`, then `sync_dir`. After `DropAll`, `f` is `"abc"`. Then rename a synced file and write to it through a handle reopened at its new name: `DropAll` keeps only the synced bytes, under the old name.
- `set_len_cuts_a_straddling_write`: write `"aaaa"` and `sync_all`, then write `"bbbbbb"`, then `set_len(7)`. After `TearLast` with any seed, the file is `"aaaa"` followed by a prefix of `"bbb"`. After `DropAll`, it is `"aaaa"`.
- `a_kept_rmdir_implies_its_children`: in `d`, a synced, empty directory, create file `x` and `sync_all` it, then `remove_file(d/x)` and `remove_dir(d)`, with no `sync_dir` anywhere. For seeds `0..64` with `KeepRandom`, no crash panics, and whenever `d` is gone, `x` is gone too. `CrashReport` has counts only, so the test doesn't try to say which ops were dropped.
- `a_recreated_directory_is_a_new_incarnation`: `create_dir(d)`, `sync_dir(root)`, create `d/a` and `sync_all` and `sync_dir(d)`, `remove_file(d/a)`, `remove_dir(d)`, `create_dir(d)`, create `d/b`. For seeds `0..64` with `KeepRandom`, nothing panics, and the tree is one that some prefix of the history produces: `d/a` and `d/b` never exist at the same time.

```bash
cargo test -p cetra-storage --all-features fs::lossy
```

---

## R2.3 — Durable unlinks and directory entries (audit item 2, new item 6)

Commit: `storage: make object deletes and directory creation durable (R2.3)`

### Re-check first

- **Item 2: GC unlinks are not durable.** `gc::collect_obsolete` calls `Committer::collect_obsolete_file`. That runs `delete()` (`Objects::delete_len`, which is `fs::remove_file` since R2.1), then `DELETE FROM data_files`, with no directory fsync in between. After a power loss the file can come back with no row pointing to it. The other deletes work the same way:
  - `collect_once`, for staged rows;
  - `MediaGc` (`media.rs`: `delete_len` and `delete`);
  - N B5's `collect_orphans`.

  Only `collect_orphans` lists the object tree, and only on an operator's request, so the file leaks.
- **Item 6, found while planning M2: directories created but never synced.** `ensure_dir` (`lib.rs`) and `create_dir_chain`/`create_dir_chain_from` (`objects.rs`) fsync the parent only when they create the directory. A process killed between `create_dir` and that `sync_dir` leaves an entry that exists but is not durable. Every later call takes the `AlreadyExists`/`try_exists` branch and never syncs it. Then:
  1. a file is published in that directory, and the file and the directory are synced;
  2. the catalog commit names the file;
  3. a power loss loses the directory entry, and the file with it, while the catalog row stays.

  An acknowledged and flushed row is gone.

If a test below passes before the fix, its finding is not reproduced: stop and report.

### Tests (write them first, see them fail, then fix)

In the `objects.rs` tests, with lossy root `t/data`, objects root `t/data/objects` and `local_config(t/data)`:
1. `a_deleted_object_stays_deleted_after_a_lossy_crash`: `put_durable(key)`, `delete(key)`, drop `objects`, `crash_with(DropAll, 0)`. Then `root.join(key)` does not exist. Before the fix the unlink is undone.
2. `an_object_in_a_directory_left_unsynced_survives_a_lossy_crash`:
   - create the directory chain of `key` with `crate::fs::create_dir`, one level at a time, **without** syncing (the state a killed process leaves);
   - `Objects::new`, `objects.remove_temp_files()` (what `Storage::open` runs), `put_durable(key)`, drop `objects`;
   - `crash_with(DropAll, 0)`.

   The object exists with its bytes. Before the fix it is lost.
3. `deleting_a_key_whose_directory_is_missing_is_ok`: `delete` and `delete_len` of a key under `tables/spans/project=…/day=…/` whose day directory was never created return `Ok`, and `delete_len` returns 0. This passes before the fix, and must still pass after it.

In the `lib.rs` tests:

4. `the_data_dir_layout_is_durable_after_a_crash_before_its_sync`:
   - lossy root `t/data`;
   - `fs::create_dir(t/data/wal)` without a sync, as a killed first open leaves it;
   - a life (`test_support::crashed_life`, below) opens the storage and appends one span;
   - `crash_with(DropAll, 0)`, then reopen.

   The span is there. Before the fix, the whole `wal/` is lost.
5. `gc_leaves_no_orphan_after_a_lossy_crash`:
   - lossy root;
   - three lives that each append one span of trace 1 and shut down, which makes three L0 files (as `crash_tests::compaction_child` does);
   - a fourth life that runs `compact(now)` and `collect_obsolete(Duration::ZERO)`, then crashes;
   - `crash_with(DropAll, 0)`, then reopen.

   `test_support::orphan_objects(dir).await` is empty. Before the fix it lists the compacted inputs.

### Fix

- **Local deletes** (`objects.rs`, `Kind::Local`, the path R2.1 added):
  - `fs::remove_file` succeeds: `fs::sync_dir(parent)`;
  - `NotFound` while the parent directory exists: also `fs::sync_dir(parent)`, since a previous attempt may have unlinked the file without the sync;
  - `NotFound` while the parent directory is missing too: return `Ok` without a sync. There is nothing to make durable, and GC and media GC must not turn a missing object into an error.

  Until R2.7, a sync failure is an `ObjectError::Io { action: "syncing object directory", .. }`.
- **`ensure_dir`** (`lib.rs`): sync the parent in both branches, `Ok` and `AlreadyExists`. It runs at open only: six fsyncs per open.
- **`create_dir_chain`** (`objects.rs`): rewrite it. Today's loop walks up to the first existing ancestor and syncs only the parents of the directories it creates. Make it walk **down** instead:
  1. when the object root is inside the data directory, start from the first component of `path` inside it (`config.data_dir`, passed in). When the root is outside it, start below the nearest ancestor of `path` that already exists. Never sync from `/` down, which can fail with `EACCES` on directories the process doesn't own;
  2. for each component, `fs::create_dir` (treating `AlreadyExists` as success when it is a directory), then `fs::sync_dir` of its parent, whether the directory was created or already existed.

  It runs at open only: one fsync per component.
- **`remove_temps`** (`objects.rs`): also `fs::sync_dir` every directory it walks, after its entries (children first), and finally the object root itself, which holds the `tables` and `media` entries. It already walks `tables/` and `media/` at every open (`Objects::remove_temp_files`), so every directory a crashed process left unsynced is synced before the first write of the new life.
- **`create_dir_chain_from`** stays as is. The startup walk covers what a crash left, and in a running process a directory it creates is synced before it returns.

The walk costs one fsync per partition directory, at open. Measure it, on `trace_bench`'s 1M-trace directory if one is at hand, or on 1,000 partition directories made by a loop, and put the number in the report. The user chose this walk (§ Conventions, decided). If it takes more than 2 s for 1,000 directories, keep it anyway and give the number in the report.

### Test helpers (`test_support.rs`)

```rust
/// One storage life on a runtime of its own, ended like a crash (D-R-4): the
/// storage is dropped without `shutdown`, then the runtime is shut down, so
/// no task of this life can touch the directory afterwards.
pub(crate) fn crashed_life<T>(config: &StorageConfig, body: impl AsyncFnOnce(&Storage) -> T) -> T;
/// The same, ended with `shutdown`.
pub(crate) fn clean_life<T>(config: &StorageConfig, body: impl AsyncFnOnce(&Storage) -> T) -> T;
/// Object keys under `objects/tables` and `objects/media` (temp names
/// excluded) that no `data_files`, `staged_objects` or `media` row names.
/// Opens `catalog.db` read-only, on a connection of its own.
pub(crate) async fn orphan_objects(data_dir: &Path) -> Vec<String>;
```

The runtimes are built as M1's `open_life` builds them: `new_multi_thread().worker_threads(2).enable_all()`, ended with `shutdown_timeout(10 s)`. Media keys are `media/<project_id>/<sha256>`, from `SELECT project_id, sha256 FROM media`. The `sha256` column is `TEXT` and already holds the lowercase hex digest (`Storage::put_media` binds `hex::encode(blob.sha256)`, `lib.rs` around line 715), so it is used as is. If the code has changed and stores bytes, hex-encode them first.

### Changelog

`CHANGELOG.md` § `[Unreleased]` › `Fixed`: "Storage: a local object deleted by garbage collection, or a directory created just before a crash, could reappear or disappear after a power loss."

### Spec

In spec 03 § Layout, add to the paragraph on local object writes: "A local delete unlinks the file and fsyncs its directory before the catalog row is removed. Startup fsyncs every directory of the data directory layout and of the local object tree, so an entry that a crashed process created is durable before it is used."

---

## R2.4 — WAL: sync a segment before it stops being the last (audit item 1)

Commit: `storage: sync a WAL segment before a newer one exists (R2.4)`

### Re-check first

- **Rotation (item 1).** In interval mode (`sync_after == false`), `IoJob::commit` (`wal/writer.rs`) writes a group, then `SegmentWriter::create`s the next segment and drops the old `SegmentWriter` without a `sync`. The old segment's unsynced tail can tear on a power loss. `replay::scan_segment` treats `Short` or `Invalid` in a segment that is not the last as corruption, and `Storage::open` fails with `OpenError::WalCorrupt`.
- **Open (item 1b, found while planning M2).** `recover_table` (`lib.rs`) replays, then `SegmentWriter::create`s a new segment at `next_seq`. The segment replay ended in is never synced. In interval mode its unsynced tail survives an abort (the page cache keeps it), and it can be torn by the next power loss while it is no longer the last segment. The open then fails the same way.

`"always"` mode syncs every group before rotating, so only interval mode is affected. The fix applies to both.

### Tests (in the `lib.rs` tests, failing first)

The config is `test_support`'s quiet config with `wal.sync = WalSync::Interval(Duration::from_secs(3_600))` and `wal.segment_bytes = 1 MiB`, so the writer syncs only when it must. Both tests use a `TempDir` `t` with `data_dir = t/data`, and register `Lossy::enable(t/data, t/shadow)` **before** the first life, so that every write of every life is recorded.

**A batch that really rotates.** WAL records are Arrow IPC with LZ4-frame body compression (`wal/record.rs`, `encode`), so a repeated string compresses to a few KiB and never reaches `segment_bytes`. Add to `test_support.rs`:

```rust
/// One span of `trace` whose `input` is `len` bytes of incompressible
/// printable ASCII: SplitMix64 output, each byte mapped to `b'!' + b % 94`.
/// Its WAL record is about `len` bytes, whatever the compression.
pub(crate) fn big_span_batch(trace: u16, len: usize) -> RecordBatch;
```

It is built from `span_row(trace, 0, start, ingest)` with `input = Some(text)`, through `span_rows`. Every test that needs a rotation calls `big_span_batch(1, 1_572_864)` (1.5 MiB) and asserts the rotation happened: right after that append, `wal/spans` holds 2 segment files (`parse_segment_name` is `Some`).

1. `interval_rotation_syncs_the_old_segment`. For each seed in `0..8`, in a fresh directory:
   - a `crashed_life` appends `big_span_batch(1, 1_572_864)` and asserts there are 2 segments, then appends one small batch;
   - `crash_with(CrashMode::TearLast, seed)`;
   - the next open returns `Ok`, and the big span is readable (query `span_id` as `crash_tests::stored_ids` does).

   Before the fix, either the torn old segment fails the open (`WalCorrupt`), or a cut to 0 bytes loses the big span.
2. `reopen_syncs_the_replayed_segment`. For each seed in `0..8`: a `crashed_life` appends one small batch; a second `crashed_life` opens (and replays it) and appends nothing; `crash_with(TearLast, seed)`. The next open returns `Ok`, and the first span is readable.

### Fix

- **`IoJob::commit`:** when `self.segment.needs_rotation()` and the group was not synced (`!self.sync_after`), call `self.segment.sync()` before `SegmentWriter::create`. On an error, return `KeptSegment` as the other sync failures do, with no rewind (R2.5's rule); the writer stops. When this sync ran, set `did_sync` and `group_synced` to true in the returned `Written`: the group is durable.
- **`recover_table`** (`lib.rs`): after `replay` and before `SegmentWriter::create`, `sync_all` the last segment file, when there is one (`fs::OpenOptions::new().write(true).open(path)?.sync_all()`). An error is `OpenError::Io { action: "syncing WAL segment", .. }`. `list_segments` is private to `replay.rs`, so instead of listing again, `replay` returns the last segment's path in `ReplayOutcome` (`pub last_segment: Option<PathBuf>`).

### Changelog

`CHANGELOG.md` § `[Unreleased]` › `Fixed`: "Storage: in interval mode (`wal.sync = "<duration>"`), a power loss could leave a torn WAL segment that was no longer the last one, and the next start refused it as corrupt. A segment is now fsynced before a newer one is created."

### Spec

In spec 03 §1, after "The `wal/<table>/` directory is fsynced when a segment is created or deleted.", add: "A segment is fsynced before a newer one is created, at rotation and at open, so only the last segment can have a torn tail. In interval mode, a power loss therefore loses a suffix of each WAL: the records acknowledged since that WAL's last fsync."

---

## R2.5 — WAL: a failed group (audit item 3)

Commit: `storage: leave a failed WAL group as written and sync the discarded segment's removal (R2.5)`

### Re-check first

In `wal/writer.rs`:
- **(a)** `IoJob::commit` calls `rewind_to` (`set_len` + `sync_all`) after `sync_after` failed: a second fsync on a file whose fsync just failed.
- **(b)** `discard_unacked` runs when the memtable insert fails after a durable group. It removes the new, empty segment with `remove_file` and no directory fsync. After a power loss:
  1. the empty segment named after the seq that follows the discarded group comes back;
  2. the next open creates the active segment at the lower `first_seq` of the discarded group;
  3. the stale empty file sorts **after** it, so it is "the last segment";
  4. a torn tail in the real active segment is then corruption, and the open fails.
- **(c)** After a restart, an unacknowledged group can still be on disk and be replayed. That is acceptable: the client got `WalFailed` and may retry, and span and score duplicates merge (spec 03 §2 and §5). But it is not documented.

### Seam for (b)

`hooks.rs`: `HookPoint::AfterWalSync` gets a `fail`. In `run` (`wal/writer.rs`), right after `hooks.pause(HookPoint::AfterWalSync)`, `if hooks.fail(HookPoint::AfterWalSync).is_some()`, the group takes the `insert_all` error path (`discard_unacked`, `fail_group`, `fail_queued`, `break`), as a memtable insert error would. Add to the variant's doc comment: "`fail` takes the memtable-insert failure path: the durable group is discarded and the writer stops."

### Tests (failing first)

In the `wal/writer.rs` tests, or the `lib.rs` tests if the writer tests can't take a lossy root:
1. `a_failed_sync_leaves_the_group_and_does_not_sync_again`:
   - `"always"`, with `hooks.arm_fail(HookPoint::WalSync)`;
   - the append returns `WalFailed`, and `hooks.hits(HookPoint::WalSync) == 1`;
   - the life crashes, and the next open runs without a lossy crash.

   The failed append's rows **are** readable: the page cache has them, and nothing rewound them. Before the fix they are not readable, because the group was rewound.
2. `a_failed_sync_group_is_lost_on_power_loss`: as test 1, with a lossy root and `crash_with(DropAll, 0)` before the reopen. The open succeeds, and the rows are not readable. This test passes before and after the fix: it pins the other allowed outcome.
3. `a_discarded_segment_stays_removed_after_a_lossy_crash`:
   - `"always"`, `segment_bytes = 1 MiB`, a lossy root, and `AfterWalSync` armed to fail;
   - before the append, list the segment names in `wal/spans`. A fresh directory has one, the segment open created at `first_seq` (1);
   - append `big_span_batch(1, 1_572_864)` (R2.4): the writer syncs the group, rotates to a new segment named `segment_name(first_seq + 1)` (the group is one record), then the insert "fails", and the append returns `WalFailed`. `discard_unacked` has already removed the new segment by the time the append returns, so it can't be listed afterwards;
   - after the failed append, list `wal/spans` again: it equals the listing from before the append, and `segment_name(first_seq + 1)` is absent. This passes before and after the fix, and shows the rotation's segment was created and then removed: assert that `hooks.hits(HookPoint::AfterWalSync) == 1`, and that the old segment's length is back to its length from before the append;
   - the life crashes, then `crash_with(DropAll, 0)`.

   `wal/spans` still has the same listing: `segment_name(first_seq + 1)` does not exist. The next open succeeds. Before the fix, the lossy crash undoes the unsynced unlink, and the empty segment comes back.

### Changelog

`CHANGELOG.md` § `[Unreleased]` › `Fixed`: "Storage: a WAL group whose fsync failed is no longer truncated with a second fsync, and the segment a failed group discards stays removed after a power loss."

### An existing test changes

`wal/writer.rs` tests, `a_sync_error_acks_wal_failed_and_replay_keeps_acked_records`: today it ends with `assert_eq!(replay_seqs(running.dir.path()), vec![1]);`, which holds only because the failed group is rewound. Its `group_max_bytes` is one batch, so the failed group is the one record with seq 2; the other queued appends are failed without being written. After the fix that group stays in the file, so the assertion becomes `assert_eq!(replay_seqs(running.dir.path()), vec![1, 2]);`, with a comment: "the group whose fsync failed is left as written and replays (spec 03 §1)". Run the test before the fix to confirm it gives `[1]`, and after the fix `[1, 2]`. If the seq is not 2, read why before changing the assertion. The neighbouring test `a_write_error_acks_wal_failed_and_replay_keeps_acked_records` (`WalWrite`) keeps `vec![first.seq]`: a failed write still rewinds.

### Fix

- **`IoJob::commit`:** after a failed `sync` (`sync_first` or `sync_after`), return `KeptSegment` **without** `rewind_to`. A failed `write` still rewinds: no fsync has failed yet, and the rewind removes a partial record.
- **`discard_unacked`:** after `remove_file(&new_path)`, call `fs::sync_dir(&dir)`, and log an error if it fails (the writer is stopping either way). Pass the directory in, either as a field of `Written` or as a `dir: &Path` parameter of `discard_unacked`.
- Update `IoJob::commit`'s doc comment, which today says: "On failure the segment is back at `mark` unless the failure was a sync of already-acknowledged bytes, which adds nothing." Make it: "A failed write leaves the segment back at `mark`. A failed sync leaves the file as written, with no second sync; the writer stops, and the group may be replayed after a restart."
- Update the module doc of `wal/writer.rs`: a write that fails ends the task and truncates the group; a sync that fails ends the task without touching the file again, so the group may be replayed after a restart.

### Spec

In spec 03 §1, append to **Write errors are final**: "A group whose `fdatasync` failed is left as written: a restart may replay it, although its appends answered `WalFailed`. Clients retry on 503, and replayed duplicates merge like any re-send (§2, §5)."

---

## R2.6 — Read cache: a checksummed size file (audit item 4)

Commit: `storage: checksum the read cache size file (R2.6)`

### Re-check first

`ReadCache::set_size` writes `size.to_string()` through `write_renamed`: a temp file, then a rename, with no fsync and no checksum. A torn write that keeps a prefix of the digits still parses (`"1048579"` → `"104"`). `CacheStore::read` clamps every range to that size (`range.end.min(size)`), so a range that crosses it comes back short. Blocks carry a CRC; the size does not. Confirm with the test below.

### Test (failing first, in the `read_cache.rs` tests)

`a_torn_size_file_never_serves_short_ranges`:
- lossy root `t/data`, cache directory `t/data/cache`;
- an `InMemory` inner store holding one data file, `tables/spans/project=…/day=…/0-<uuid>.parquet`, of 1,048,579 bytes (byte `i` is `(i % 251) as u8`).

For each seed in `0..16`, with a fresh root each time:
1. open the cache and a `CacheStore`, and `get_ranges(&location, &[0..200_000])`: this records the size and caches block 0;
2. drop both, then `crash_with(TearLast, seed)`;
3. reopen and `get_ranges(&location, &[0..200_000])` again.

It returns exactly the 200,000 original bytes. Before the fix, a torn size below 200,000 returns fewer bytes.

### Fix

- The `size` file holds 12 bytes: the size as a little-endian `u64`, then the CRC-32C of those 8 bytes, little-endian (the blocks' convention). `set_size` writes it with `write_renamed`.
- `ReadCache::open` parses it with a new `fn parse_size(bytes: &[u8]) -> Option<u64>`. Anything but exactly 12 bytes with a matching CRC is `None`, and the directory is dropped, as an unparsable size is today.
- No fsync is added: the cache is fsync-free by design. Update the module doc: "…Each block file and the `size` file end with a CRC-32C…".
- Unit tests of `parse_size`: a round trip; 11 bytes; a flipped bit; the old text format `b"12"` → `None`.
- The existing test that writes `"12x"` into a size file keeps its meaning: an unparsable size drops the directory.

This changes a format surface, the read-cache layout (D-R-12). Before the first public release there is no constraint; the commit body says so.

### Changelog

`CHANGELOG.md` § `[Unreleased]` › `Fixed`: "Storage: a read-cache size file torn by a power loss could make S3-mode queries read short ranges. The size file now carries a CRC-32C; cache directories written in the old format are dropped at open and fetched again."

### Spec

In spec 03 § S3 read cache: "`cache/<file name>/size` holds the object size as 8 bytes and their CRC-32C; a size that fails its check drops the file's blocks."

---

## R2.7 — A failed object fsync stops the local object store (D-R-6)

Commit: `storage: stop local object writes after a failed fsync (R2.7)`

### Behaviour

D-R-6: a local object publish whose fsync fails fails the job and marks the store unhealthy, with no retry on the same file. The component is **the local object store as a whole**. A failed fsync is any of:
- a failed `sync_all` of a temp file;
- a failed `sync_dir` of an object directory, in a publish, a delete, or the startup walk of R2.3.

After it, until the process restarts, every local `put_durable`, `create_output`, `publish_output`, `put_media`, `delete` and `delete_len` returns `ObjectError::SyncFailed`. The reason: a failed directory fsync says nothing about which entries of that directory are durable, including other jobs' renames. A retry under a new name would sync the directory again, and could report success after an entry was lost (fsyncgate, on a directory).

Reads keep working. Remote mode has no local fsync and is not affected.

### Code

- `error.rs`, in `ObjectError`:

  ```rust
  /// A local fsync failed earlier in this process. Local object writes and
  /// deletes are refused until a restart (D-R-6).
  #[error("local object store stopped after a failed fsync; restart required")]
  SyncFailed,
  ```
- `objects.rs`:
  - `Objects` gets `sync_failed: Arc<AtomicBool>`, cloned into the `spawn_blocking` closures, and `pub(crate) fn sync_failed(&self) -> bool`.
  - Every local write and delete entry point checks it first.
  - `publish_local`, the delete path and `remove_temps` set it when `sync_all` or `sync_dir` returns an error, and log once at `error` (`"object store stopped after a failed fsync"`, with the action). The injected `HookPoint::ObjectSync` and `HookPoint::ObjectDirSync` faults set it too: they stand for exactly that failure. `ObjectWrite` and `ObjectRename` faults do not.
- `lib.rs`: `StorageHealth` gets `pub objects_sync_failed: bool`, documented "A local object fsync failed. Until a restart, every flush attempt and every compaction job fails, and GC deletes nothing (D-R-6)", and filled from `self.objects.sync_failed()`.
- `flush.rs`, `run`: in the `Err(_)` arm, before sleeping, `if stopping(&params.stop) && (params.objects.sync_failed() || params.committer.stopped())`, then `break`. R2.8 adds `Committer::stopped`, so this sub-phase checks `sync_failed()` only, and R2.8 adds the other half. Without this, shutdown waits the full 30 s for a flush that can't succeed. The rows stay in the WAL.

### Tests

- `objects.rs`, `local_faults_leave_no_final_file`: add, for each point, `assert_eq!(objects.sync_failed(), matches!(point, HookPoint::ObjectSync | HookPoint::ObjectDirSync))`.
- `objects.rs`, `a_failed_object_fsync_refuses_later_writes_and_deletes`: arm `ObjectSync`.
  - `put_durable` returns `Io`;
  - a second `put_durable`, of another key, returns `SyncFailed`;
  - `delete` of a third key returns `SyncFailed`;
  - `get_range` of an existing object still works.
- `objects.rs`, `a_failed_delete_sync_stops_the_store`: lossy root; `put_durable(key)`; `lossy.fail_next_sync(root.join(key).parent())`. `delete(key)` returns `Io`, and `sync_failed()` is true.
- `lib.rs`, `a_failed_flush_fsync_makes_storage_unhealthy`: flush `max_age` 500 ms; arm `ObjectSync`; append.
  - `health().objects_sync_failed` becomes true within 10 s (poll);
  - appends still return `Ok`;
  - `shutdown` returns within 5 s;
  - after a reopen, the rows are readable from the WAL and `health()` is clean.

### Spec

Spec 03 §10, a new row of the table: "Local object fsync error (publish, delete, startup walk) | the job fails; every later local object write and delete fails with `SyncFailed`, so every flush attempt and every compaction job fails, on each retry or pass, until a restart; health reports it (`/readyz` 503); restart required. Reads continue (D-R-6)."

### Changelog

`CHANGELOG.md` § `[Unreleased]` › `Added`: "Storage: a failed fsync of a local object stops local object writes until a restart, and `StorageHealth::objects_sync_failed` reports it (D-R-6). New error `ObjectError::SyncFailed`."

### Requested outside R

In `crates/server/src/http/health.rs`, `readyz` needs `if health.objects_sync_failed { failed.push("object store"); }` and, from R2.8, `if health.catalog_failed { failed.push("catalog"); }`, with a test each. Send these exact lines to the coordinator in the R2.7 report. The new `StorageHealth` fields are additive, and the server only reads fields, so the server builds unchanged until then.

---

## R2.8 — A catalog I/O error stops the committer (audit item 5, D-R-6)

Commit: `storage: stop the catalog committer after a SQLite I/O error (R2.8)`

### Re-check first

`catalog/committer.rs`: a failed `transaction.commit()` maps to `CommitError::Db` or `CatalogError::Db`, and every caller retries: `flush.rs` `run` with its backoff, compaction at its next pass, GC at its next interval.

`sqlx` 0.9 reports a SQLite error as `sqlx::Error::Database(SqliteError)`. Its `code()` is the **extended** result code as a decimal string (`sqlx-sqlite-0.9.0/src/error.rs`: `sqlite3_extended_errcode`, and `code()` is `format!("{}", self.code)`). A failed fsync is `SQLITE_IOERR_FSYNC` (1034) or `SQLITE_IOERR_DIR_FSYNC` (1290). Not every extended code with primary code 10 means the data on disk is uncertain: `SQLITE_IOERR_NOMEM` (3082), `SQLITE_IOERR_LOCK` (3850) and `SQLITE_IOERR_BLOCKED` (2826) don't. So the fatal codes are an explicit set, not a mask.

Why stop rather than retry: in WAL mode, a commit's frames are already in `catalog.db-wal` when its fsync fails. Whether they are durable is unknown, and at the next open SQLite's WAL recovery may find them valid and apply the transaction the caller saw fail. Every caller's next step is correct in both outcomes, but only after a restart: objects are durable before a commit names them (R2.3, R2.7), and the WAL is truncated only after a commit returned `Ok`. No test checks this reading of SQLite's `wal.c`, and LazyFS can't inject an fsync error: record it as unverified in `hardening.md`.

### Code

- `error.rs`: `CatalogError::Stopped` and `CommitError::Stopped`, both `#[error("catalog stopped after an I/O error; restart required")]`, documented with D-R-6.
- `committer.rs`:
  - the fatal set, as named constants:

    ```rust
    /// D-R-6: extended SQLite codes after which the bytes on disk are
    /// uncertain. `SQLITE_IOERR` (10), `_READ` (266), `_WRITE` (778),
    /// `_FSYNC` (1034), `_DIR_FSYNC` (1290), `_TRUNCATE` (1546).
    const FATAL_IO_CODES: [i32; 6] = [10, 266, 778, 1034, 1290, 1546];

    /// An I/O error at a commit point that stops the committer (D-R-6).
    /// Busy (5), full (13), constraint errors and the other `SQLITE_IOERR_*`
    /// codes (NOMEM 3082, LOCK 3850, BLOCKED 2826) stay retryable.
    fn is_fatal_io(error: &sqlx::Error) -> bool
    ```
    It is true for `sqlx::Error::Database(db)` when `db.code()` parses as an `i32` that is in `FATAL_IO_CODES`, and false for everything else.
  - `Committer` and `CommitterInner` share a `stopped: Arc<AtomicBool>`. `pub(crate) fn stopped(&self) -> bool` reads it without the lock.
  - `async fn lock_running(&self) -> Result<MutexGuard<'_, CommitterInner>, CatalogError>` takes the lock, then returns `Err(CatalogError::Stopped)` when stopped. Every `pub(crate)` method that returns a `Result` takes the lock through it:
    - with `?` in methods returning `CatalogError`, `DeletionError`, `GcError` or `MediaError`, which all implement `From<CatalogError>`;
    - with `.map_err(|_| CommitError::Stopped)?` in methods returning `CommitError`.

    Methods without a `Result` (`catalog`, `flushed_seq`, `tombstones`) read in-memory state and stay as they are.
  - **Commit points** pass their result through `fn note_commit<T>(stopped: &AtomicBool, result: Result<T, sqlx::Error>) -> Result<T, sqlx::Error>`. When `is_fatal_io`, it sets `stopped` and logs once at `error` (`"catalog stopped after a SQLite I/O error"`, with the extended code). The commit points are:
    - every `transaction.commit()`;
    - every write statement run outside a transaction on `inner.connection`: in `touch_media`, `stage_media`, `collect_staged_path`, `collect_obsolete_file`, `delete_media_row`, `finish_drop` and `collect_media`. Find them with `grep -n 'execute(&mut inner.connection)' committer.rs` and keep the `INSERT`, `UPDATE` and `DELETE` statements.

    A statement inside a transaction is not a commit point: its failure rolls the transaction back and stays retryable.
- `hooks.rs`: `HookPoint::CatalogFsync` (index 24, `COUNT = 25`), documented "Replaces the SQLite commit of `commit_inner`: `fail` rolls the transaction back and returns `SQLITE_IOERR_FSYNC` as the commit's error, as a failed fsync would." In `commit_inner`, after the `CatalogCommit` hook:

  ```rust
  #[cfg(test)]
  if hooks.fail(HookPoint::CatalogFsync).is_some() {
      let _ = transaction.rollback().await;
      let error = note_commit::<()>(&inner.stopped, Err(injected_fsync_error()));
      return Err(CommitError::db(error.unwrap_err()));
  }
  ```
  `injected_fsync_error()` and its type `InjectedSqliteError { code: i32 }` live in a `#[cfg(test)]` block of `committer.rs`. The type implements `std::error::Error`, `Display`, and `sqlx::error::DatabaseError` (`message`, `code`, `as_error`, `as_error_mut`, `into_error`, and `kind` returning `ErrorKind::Other`). It is a test double of a `sqlx` trait, not a trait of ours.
- `lib.rs`: `StorageHealth` gets `pub catalog_failed: bool`, documented "A `catalog.db` commit failed with an I/O error. Until a restart, every catalog call fails: flush attempts, compaction and deletion passes, retention, drops and GC (D-R-6)", and filled from `self.committer.stopped()`. In `flush.rs` `run`, add `params.committer.stopped()` to R2.7's condition.
- `Storage::register_deletion`, `apply_retention` and `drop_project` return the `Stopped` error through their existing `Catalog`/`Commit` variants. The server already maps those to an error response.

### Tests

- `committer.rs`, `fatal_io_codes`: `is_fatal_io` is true for `InjectedSqliteError` with each of the codes 10, 266, 778, 1034, 1290 and 1546. It is false for codes 5 (`BUSY`), 13 (`FULL`), 19 (`CONSTRAINT`), 2067 (`CONSTRAINT_UNIQUE`), 3082 (`IOERR_NOMEM`), 3850 (`IOERR_LOCK`) and 2826 (`IOERR_BLOCKED`), for `sqlx::Error::RowNotFound`, and for `sqlx::Error::Io(..)`.
- `committer.rs`, `an_fsync_error_stops_the_committer_until_reopen`: arm `CatalogFsync`.
  - `commit` returns `CommitError::Db`, and `stopped()` is true;
  - the next `commit` returns `CommitError::Stopped`;
  - `stage` returns `CatalogError::Stopped`;
  - `register_deletion` returns `DeletionError::Catalog(CatalogError::Stopped)`;
  - the database still has the old state (load it with `open_catalog_db` and `load`);
  - a new `Committer::open` on the directory works and commits.
- `committer.rs`, `an_injected_commit_fault_leaves_the_database_and_memory_unchanged` (existing, `CatalogCommit`): add `assert!(!committer.stopped())`. That fault is a rollback before the commit, not an I/O error, and stays retryable.
- `lib.rs`, `a_catalog_fsync_error_makes_storage_unhealthy`: as R2.7's storage test, with `CatalogFsync` armed:
  - `health().catalog_failed` becomes true within 10 s;
  - appends still return `Ok`;
  - `shutdown` returns within 5 s;
  - after a reopen, the rows are readable and `health()` is clean.
- `flush.rs`, `a_failed_commit_retries_with_the_same_anchor_days` (existing, `CatalogCommit`) passes unchanged.

### Spec

- Spec 03 §4 (catalog), after the committer's description: "A commit that fails with a SQLite I/O error (`SQLITE_IOERR`, `_READ`, `_WRITE`, `_FSYNC`, `_DIR_FSYNC` or `_TRUNCATE`) stops the committer: every later catalog call fails until a restart, and health reports it (D-R-6). The commit's frames may already be in `catalog.db-wal`, so the restart, not a retry, decides whether it happened."
- Spec 03 §10, a new row of the table: "`catalog.db` I/O error at a commit | the committer stops; every flush attempt, compaction pass, deletion pass, retention, drop and GC run fails, on each retry or pass, until a restart; appends continue until backpressure; health reports it (`/readyz` 503); restart required (D-R-6)".

### Changelog

`CHANGELOG.md` § `[Unreleased]` › `Added`: "Storage: a `catalog.db` commit that fails with an I/O error stops the catalog until a restart, and `StorageHealth::catalog_failed` reports it (D-R-6). New errors `CatalogError::Stopped` and `CommitError::Stopped`."

---

## R2.9 — Lossy crashes in the model test

Commit: `storage: add lossy crashes to the model test (R2.9)`

### Harness (`model_tests/harness.rs`)

- `Harness::new` uses a `TempDir` `t`, with data directory `t/data` and shadow `t/shadow`, and calls `Lossy::enable(data, shadow)` **before** the first open. It keeps the result in a new field, `lossy: crate::fs::lossy::Lossy`. `dir()` returns `t/data`, and `model_config(dir)` is unchanged.
- `pub(super) fn lossy_crash(&mut self, seed: u64) -> CrashReport` ends the life exactly as `crash` does (drop the storage, then `runtime.shutdown_timeout(10 s)`), then runs `self.lossy.crash(seed)`, then `open_life`.
- `Harness::with_config(change: impl FnOnce(&mut StorageConfig)) -> Self`, for the interval variant. `new()` calls it with a no-op.
- `pub(super) fn lossy(&self) -> &Lossy`, for R2.11.

### Which records survive

A **record** is one successful `append_spans` or `append_scores` call: one WAL record. `Model` keeps, per WAL (`spans` and `scores`):
- `unsynced: Vec<Record>`: the records appended in this life. Each holds what the model pushed for it, possibly nothing when the keep rule kept nothing: `Vec<(P, u8, u8, i64)>` span copies, or `Vec<(P, ScoreKey, u32)>` score versions;
- `durable_unflushed: u64`: the records of earlier lives that are still unflushed. They are durable, because R2.4 syncs the replayed segment at open;
- `synced_floor: usize`: how many of the first `unsynced` records are known to be durable;
- `segments: usize`: the number of segment files in `wal/<table>` after the last append, read from the directory (`parse_segment_name` is `Some`).

Updates:
- every successful append pushes a `Record` onto its WAL's `unsynced`. Then:
  - in `"always"` mode, `synced_floor = unsynced.len()`: the group was fsynced before its ack;
  - in interval mode, if `wal/<table>` now has more segments than `segments`, the writer rotated, and R2.4 synced every record up to this one first: `synced_floor = unsynced.len()`. In both modes, `segments` is then updated;
- `Restart` and `Settle` shut down, which flushes: `unsynced` is cleared, `synced_floor = 0`, `durable_unflushed = 0`, and `segments` is read again;
- `Crash`: `durable_unflushed += unsynced.len()`, `unsynced` is cleared, `synced_floor = 0`, and `segments` is read again after the reopen;
- `LossyCrash`, after the reopen, per WAL:
  1. `k = storage.replayed(table).checked_sub(durable_unflushed).expect("replayed fewer records than the earlier lives left durable")`. `Storage::replayed` is a private `#[cfg(test)]` method of the crate root, callable from `model_tests`;
  2. assert `synced_floor <= k <= unsynced.len()`: every record known durable survived. The failure message prints the op index, the WAL, `k`, `synced_floor` and `unsynced.len()`;
  3. remove from the model what the records after the first `k` pushed: pop the span copy with that `ingest`, or the score version with that `ingest`;
  4. `durable_unflushed += k`, `unsynced` is cleared, `synced_floor = 0`, and `segments` is read again.

In `wal.sync = "always"`, `synced_floor` always equals `unsynced.len()`, so the check is `k == unsynced.len()`: nothing acknowledged may be lost. In interval mode any `k` in `synced_floor..=unsynced.len()` is allowed. The model's records are small, so a rotation is rare there. R2.4's tests cover rotation directly, and the floor catches a rotation-synced record that a crash loses anyway. Replay stops at the first missing or torn record of each WAL, so what survives is a prefix (spec 03 §1, after R2.4). The lost records can include keys that a later `Resend` picks, which is fine: the model already allows re-sending any key below `next_span`.

### Op

```rust
/// Drops the storage without shutdown, applies a lossy crash with `seed`
/// (`CrashMode::from_seed`), and reopens.
LossyCrash { seed: u64 },
```

It has weight 3 in `prop_oneof!`, and `Op::project()` returns `None` for it. `run_case` runs `check_full` after it.

### Physical check: no orphan

`check_full` adds `orphan_objects(h.dir())` (R2.3's helper, through `h.block_on`) == empty, with the op index in the failure message. After R2.3 every op sequence must pass it.

### The interval variant

```rust
proptest! {
    #![proptest_config(ProptestConfig::with_cases(interval_cases()))]

    /// `wal.sync = "1h"`: a lossy crash may drop a suffix of each WAL.
    #[test]
    fn storage_matches_the_model_in_interval_mode(ops in ops::ops()) {
        run_case_with(&ops, |config| {
            config.wal.sync = WalSync::Interval(Duration::from_secs(3_600));
        });
    }
}
```

`run_case(ops)` becomes `run_case_with(ops, |_| {})`. With one hour, the writer syncs only when it must (rotation, shutdown, open), which maximizes what a crash can drop. `interval_cases()` is `cases() / 2`, at least 2. Re-measure the M1 budget (D-R-14: under 60 s in debug, both tests together), lower the default case count if needed as R1.3 says, and report the times.

### Regression tests

- `regression_lossy_crash_after_gc`: `Append` of trace 1 in project A three times with a `Restart` after each, `Compact { hours: 30 }`, `Gc`, `LossyCrash { seed: 0 }` (`DropAll`). This is audit item 2 in model form; it passes after R2.3.
- `regression_lossy_crash_tears_the_wal_tail`, in interval mode: `Append`, `Append`, `LossyCrash { seed: 2 }` (`TearLast`).

### Commands

```bash
cargo test -p cetra-storage --all-features model_tests
CETRA_MODEL_CASES=300 cargo test --release -p cetra-storage --all-features model_tests
```

A failure in the long run is a finding for R2.14: move its seed file out of the tree as R1.3 says.

---

## R2.10 — Lossy variants of the crash children

Commit: `storage: crash the hook-abort children with lost unsynced writes (R2.10)`

### Refactor (no behaviour change)

In `crash_tests.rs`, move each parent's checks after the reopen into a function, so that the abort parents, the lossy parents and R2.12's LazyFS parent share them:

```rust
async fn check_flush(dir: &Path, name: &str);
async fn check_compaction(dir: &Path, name: &str);
async fn check_deletion(dir: &Path, name: &str);
async fn check_media(dir: &Path, name: &str);
async fn check_drop(dir: &Path, name: &str);
```

Each opens the storage with its group's config and runs exactly what its parent does today after `run_child`. Each one also asserts that `test_support::orphan_objects(dir).await` is empty: after its GC step, or for a group without a GC step, after the reopen (the helper counts staged files as known). `run_child` gets an `extra: &[(&str, &OsStr)]` parameter of env vars; the existing callers pass `&[]`.

### Children

The lossy state lives in the child's memory, so **the child applies the seeded lossy crash to its directory before it calls `process::abort`** (R2.2 § A crash in a process that aborts). The parent has no record of what was unsynced, and it doesn't need one. Each `*_child` starts with:

```rust
let _lossy = lossy_from_env(Path::new(&dir));
```

```rust
/// With `CETRA_LOSSY_SHADOW` and `CETRA_LOSSY_SEED` set: registers `dir`
/// in lossy mode and arms the crash before the hook abort.
fn lossy_from_env(dir: &Path) -> Option<crate::fs::lossy::Lossy> {
    let shadow = std::env::var_os("CETRA_LOSSY_SHADOW")?;
    let seed: u64 = std::env::var("CETRA_LOSSY_SEED").ok()?.parse().ok()?;
    let lossy = crate::fs::lossy::Lossy::enable(dir, Path::new(&shadow));
    lossy.crash_on_abort(seed);
    Some(lossy)
}
```

The registration lives until the abort. The child prints the `CrashReport` to stderr before aborting, and `run_child` passes stderr through.

### Parents

Five new ignored tests: `every_flush_hook_survives_a_lossy_crash`, and the same for compaction, deletion, media and drop. For each point at index `i` of its list, and each `s` in `0..seeds()`:
1. `t = tempdir()`, `dir = t/data`, `shadow = t/shadow`, and `seed = (i as u64) * 1_000 + s`, so consecutive points cycle through the three modes;
2. `run_child(…, &[("CETRA_LOSSY_SHADOW", shadow), ("CETRA_LOSSY_SEED", seed)])`. The child crashes its directory itself, then aborts;
3. `check_*(dir, name)`, with the point and the seed in the failure message.

`fn seeds() -> u64` reads `CETRA_LOSSY_SEEDS`, default 1.

### `justfile` and `AGENTS.md`

In the `justfile`, under `test:`, after the five existing crash lines, add the same five lines for the `_survives_a_lossy_crash` parents. In `AGENTS.md` § Tests, add: "`CETRA_LOSSY_SEEDS=N just test` runs each lossy crash point with N seeds (default 1)."

### Expected results

After R2.3–R2.5, all of them pass in `"always"` mode. A failure is an R2.14 finding: the point and the seed reproduce it, and the child's stderr shows the `CrashReport`.

---

## R2.11 — Crash at a hook in the model test

Commit: `storage: crash the model test inside an operation (R2.11)`

### Why

No test yet checks every read after a crash **inside** an operation:
- M1 crashes only between completed ops, with background tasks off;
- `crash_tests.rs` checks row ids, file counts and deletion coverage;
- `tests/crash.rs` (kill -9) checks spans only.

This sub-phase crashes a model case in the middle of its last op, at a hook point, in a child process, optionally with lost unsynced writes. Then it runs `check_full` against the oracle (`00-design.md` § M2, review finding R-1).

### Reproducible cases

Parent and child must build the same case, and the same op results, independently.
- **Ops from a case number.** `fn case(n: u64) -> HookCase` builds the case with proptest's deterministic RNG:

  ```rust
  let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes(n)); // n as LE in the first 8 of 32 bytes
  let mut runner = TestRunner::new_with_rng(Config::default(), rng);
  let (ops, last) = (ops::ops(), last_op()).new_tree(&mut runner).expect("case").current();
  ```
  The imports are `proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner}` and `proptest::strategy::{Strategy, ValueTree}`. No shrinking happens: the cases are a seed list, not a proptest.
- **The last op and its hook point.** `last_op()` is `prop::sample::select(LAST_OPS)` over `(Op, HookPoint)` pairs:
  - `Op::Restart` (the flush on shutdown) with each of `FlushAfterFreeze`, `FlushAfterStage`, `ObjectSync`, `ObjectRename`, `ObjectDirSync`, `FlushAfterWrite`, `CatalogCommit`, `FlushAfterCommit`, `FlushAfterTruncate`;
  - `Op::Compact { hours: 30 }` with `CompactionAfterStage`, `ObjectSync`, `ObjectRename`, `ObjectDirSync`, `CompactionAfterWrite`, `CatalogCommit`, `CompactionAfterCommit`;
  - `Op::ApplyDeletions { hours: 1 }` with the points of `crash_tests::DELETION_POINTS`;
  - `Op::Gc` with `GcObsoleteBeforeDelete` and `GcObsoleteAfterDelete`;
  - `Op::Retention { project: P::A, keep: 1 }` with `CatalogCommit`;
  - `Op::DropB` with `CatalogCommit`.

  `HookPoint` is `Copy`, which `select` needs.
- **Deletion ids.** M1's `DeleteTraces` and `DeleteUser` use `Uuid::now_v7()`. Replace it, in both the in-process runner and here, with `shape::deletion_id(op_index: usize) -> Uuid`, a fixed UUIDv7-shaped value: `uuid::Builder::from_unix_timestamp_millis(1_700_000_000_000 + op_index as u64, &[0; 10]).into_uuid()`. Ids only have to be unique within one data directory, and every case has its own.
- **Deletion scopes** (`DeleteUser`'s traces and sessions) are already resolved from the model (R1.5), never from storage. Keep it so: the parent resolves them through its own oracle.
- **The clock** is a function of the ops (R1.3), so both processes see the same ingest times.

The case runs in `wal.sync = "always"` mode. A lossy crash then loses nothing acknowledged, so the oracle's rules below stay exact.

### Child

`model_tests/hook_crash.rs` (a `mod hook_crash;` line in `model_tests/mod.rs`):

```rust
#[test]
#[ignore = "run by every_model_hook_crash_reads_back"]
fn hook_crash_child() {
    let Ok(dir) = std::env::var("CETRA_HOOK_CRASH_DIR") else { return };
    // CETRA_HOOK_CRASH_CASE: the case number. CETRA_LOSSY_SHADOW/CETRA_LOSSY_SEED: optional.
    ...
}
```

1. `Harness::attach(dir, shadow)`: a new constructor that uses an existing `data` directory and a shadow path instead of making a `TempDir`. It registers lossy mode (always: `LossyCrash` ops in the prefix need it) and opens.
2. Run the prefix ops with M1's `apply`, against a `Model` and a clock, exactly as `run_case` does but **without** the checks: the parent runs those.
3. If `CETRA_LOSSY_SEED` is set, call `harness.lossy().crash_on_abort(seed)`.
4. `harness.storage().hooks().abort_at(point)`, then `apply(&last, …)`.
5. If the op returns without reaching the point, `std::process::exit(3)`, without shutdown. A pass with no job never reaches its hook, so this happens.

### Parent

`every_model_hook_crash_reads_back`, `#[ignore]`d, runs cases `0..cases()`. `cases()` reads `CETRA_HOOK_CRASH_CASES`, default 8. Case `n` gets a lossy crash with seed `n` when `n` is odd, and none when it is even. For each case:
1. `t = tempdir()`, then `run_child("model_tests::hook_crash::hook_crash_child", …)` with `CETRA_HOOK_CRASH_DIR = t/data` and `CETRA_HOOK_CRASH_CASE = n`, plus `CETRA_LOSSY_SHADOW = t/shadow` and `CETRA_LOSSY_SEED = n` for odd `n`. Accept two outcomes:
   - an abort by signal: the point was reached;
   - exit code 3: the op completed;

   anything else fails the case. `run_child` lives in `crash_tests.rs`: make it `pub(crate)`, and give it a parameter that accepts exit code 3, or copy its spawn-and-wait loop. Report which.
2. **The oracle.** Build the model by running the prefix ops with `apply` on a scratch `Harness::new()`, in its own temp directory. This is the same deterministic code the child ran, so the model and clock come out identical. It costs a second run of the prefix; that is accepted in exchange for not splitting `apply` into a storage half and a model half. Then apply the last op's model rule:
   - `Restart`, `Compact`, `ApplyDeletions` and `Gc` don't change any read in the model (R1.3, R1.5): the model is unchanged, whether the op finished or not;
   - `Retention` and `DropB` at `CatalogCommit` are aborted before their commit: they **did not happen** when the child aborted, and **did** happen when it exited with code 3. Apply the model rule only in the second case.

   Advance the parent's clock as the op does in both cases.
3. `Harness::attach(t/data, t/shadow)`, which reopens the child's directory, then `check_full(&harness, &mut model, ops.len())`. That checks every read, and from R2.9 the orphan audit too.
4. `settle`, then `check_full` again: the interrupted work is redone and leaves nothing behind.

Failure messages print the case number, the last op, the point, and whether a lossy crash ran.

### `justfile` and `AGENTS.md`

- `justfile`, under `test:`, after the crash lines: `cargo test --workspace --all-features -- --ignored --exact model_tests::hook_crash::every_model_hook_crash_reads_back`.
- `AGENTS.md` § Tests: "`CETRA_HOOK_CRASH_CASES=N just test` runs N model cases that crash inside their last operation (default 8)."

Measure the default run in debug and report the time. Keep the default at 8 unless it takes more than 60 s; then lower it, and report the time.

### Expected results

After R2.3–R2.8, every case passes. A failure is an R2.14 finding, and its case number reproduces it.

---

## R2.12 — LazyFS cross-check

Commit: `tools: cross-check the crash tests on LazyFS (R2.12)`

LazyFS (Jepsen, MIT, no releases) is a FUSE filesystem with its own page cache. `lazyfs::clear-cache` drops every unsynced write under the mount: a power loss, on the real binary. That includes SQLite's `catalog.db`, which the shim can't see (D-R-5). The pinned commit is **`3edad01aa936b81fed9bd64e47e725fc5ef6c977`**, `main` as of 2026-11-15 (`git ls-remote https://github.com/dsrhaslab/lazyfs`).

What LazyFS models, to check in this sub-phase: its README says unsynced **file data** is cached and dropped. It doesn't say that namespace operations (create, rename, unlink) are cached, so they probably pass straight through to the backing ext4. If so, LazyFS can't reproduce audit items 2 and 6 (lost directory entries), and the comparison in `hardening.md` says so. Step 2 below checks it.

### Files

- `tools/lazyfs/lazyfs.sh`, the script below;
- `tools/lazyfs/lazyfs.toml.in`, the LazyFS config template, with `@WORK@` where the work directory goes;
- `tools/lazyfs/README.md`.

**The config template is a copy of LazyFS's own `lazyfs/config/default.toml` at the pinned SHA**, not written from memory. Take it with `git -C "$CETRA_LAZYFS_HOME" show 3edad01aa936b81fed9bd64e47e725fc5ef6c977:lazyfs/config/default.toml`, and change only the values listed below. When this plan was written, that file had the sections `[faults]`, `[cache]`, `[cache.simple]` and `[filesystem]` (not `[file system]`, as the README shows), and `custom_size="1gb"`, lower case. If the file at the SHA differs, follow the file.

`tools/lazyfs/lazyfs.sh`, in bash, with `set -euo pipefail`, clean under shellcheck. Its subcommands:
- `build`:
  - clones LazyFS into `${CETRA_LAZYFS_HOME:-$HOME/.cache/cetra/lazyfs}` and checks out the pinned SHA;
  - runs `cd libs/libpcache && ./build.sh`, then `cd lazyfs && ./build.sh`;
  - first checks that `g++`, `cmake`, `libfuse3-dev` and `fuse3` are present (`command -v`, `pkg-config --exists fuse3`), and prints the `apt install` line for any that is missing.
- `run`:
  1. checks `grep -q '^user_allow_other' /etc/fuse.conf`. If the line is absent, it prints the line to add and exits 1; it never edits `/etc`;
  2. makes a fresh work directory `W` with `mktemp -d`;
  3. writes `W/config.toml` from `tools/lazyfs/lazyfs.toml.in`, with `@WORK@` replaced by `W` (`sed`). The values that differ from LazyFS's default:
     - `[faults]`: `fifo_path="@WORK@/faults.fifo"`, and `fifo_path_completed="@WORK@/done.fifo"` uncommented;
     - `[cache]`: `apply_eviction=false` (the default, kept);
     - `[cache.simple]`: `custom_size="1gb"`, `blocks_per_page=1`;
     - `[filesystem]`: `log_all_operations=false`, `logfile="@WORK@/lazyfs.log"`;
  4. mounts in the background with `scripts/mount-lazyfs.sh -c W/config.toml -m W/mnt -r W/root`, and waits up to 10 s for `mountpoint -q W/mnt`;
  5. sets a `trap` that runs `scripts/umount-lazyfs.sh -m W/mnt`, and `fusermount3 -uz W/mnt` if that fails;
  6. runs the two checks below, and prints a summary.
- `probe`: mounts, then:
  - writes one file with an fsync and one without;
  - creates and renames a file without a directory fsync;
  - sends `lazyfs::clear-cache`, and prints what survived.

The README covers:
- what LazyFS is, the pinned SHA, and how to bump it (`git ls-remote`, then rerun `probe`);
- the prerequisites and the commands;
- how to read a failure;
- that the run is manual, and nightly from M3.

### The checks

1. **Crash children.** A new ignored test, `crash_tests::every_hook_survives_lazyfs`. It reads three env vars: `CETRA_LAZYFS_DIR` (a directory on the mount), `CETRA_LAZYFS_FIFO` and `CETRA_LAZYFS_DONE`. Without them it returns, as the children do. For every group and point:
   1. `dir = tempdir_in(CETRA_LAZYFS_DIR)`;
   2. `run_child`, without the lossy env var;
   3. `clear_cache()`: write `"lazyfs::clear-cache\n"` to the FIFO, then read one line from the done FIFO, on a thread with a 30 s timeout;
   4. the group's `check_*` (R2.10).

   The script runs it with `cargo test --workspace --all-features -- --ignored --exact crash_tests::every_hook_survives_lazyfs`, with the workspace feature set, as `just test` does.
2. **A short ingest.** `crash_recovery` in `crates/storage/tests/crash.rs` gets two optional env vars:
   - `CETRA_CRASH_DIR_ROOT`: create the test directory with `tempfile::tempdir_in` it instead of `tempdir`;
   - `CETRA_CRASH_CLEAR_CACHE=<fifo>:<done fifo>`: after each kill and before `check_dir`, the same `clear_cache`.

   The script runs `CETRA_CRASH_ITERATIONS=10 cargo test --workspace --all-features --test crash -- --exact crash_recovery`. The child `sync_data`s `acked.log` after each line, so the log survives the clear.

A new recipe, `just lazyfs`, runs `tools/lazyfs/lazyfs.sh build && tools/lazyfs/lazyfs.sh run`. `AGENTS.md` gets:
- a § Commands row for `just lazyfs`;
- a § Tests line: "`just lazyfs` runs the crash children and a short ingest on a LazyFS mount, and drops unsynced data at each crash (D-R-5). It needs FUSE 3 and `user_allow_other` (`tools/lazyfs/README.md`)."

### Steps

1. Run `tools/lazyfs/lazyfs.sh build` on the workstation. If the build fails at the pinned SHA, stop and report the error; don't patch LazyFS.
2. Run `tools/lazyfs/lazyfs.sh probe`, and record its output in `hardening.md` § LazyFS and the shim: is unsynced data lost? Are unsynced creates and renames lost?
3. Run `just lazyfs` on this commit. Then show that the cross-check has teeth: in a scratch checkout of the commit before R2.4, set `wal.sync` to an interval in the crash children's config (a local edit, never committed) and run it there. The flush group should fail on a torn segment that is not the last, as R2.4's test did. Record both runs.
4. **`/dev/fuse` on GitHub runners (unverified).** Make a throwaway branch, `claude/r-lazyfs-probe`, off `main`, with one workflow, `.github/workflows/lazyfs-probe.yml` (`workflow_dispatch`, `ubuntu-24.04`):
   - `sudo apt-get install -y g++ cmake libfuse3-dev fuse3`;
   - `echo user_allow_other | sudo tee -a /etc/fuse.conf`;
   - `ls -l /dev/fuse`;
   - `tools/lazyfs/lazyfs.sh build && tools/lazyfs/lazyfs.sh probe`.

   **The user has approved this push** (§ Conventions, decided). The workflow has only the `workflow_dispatch` trigger. Push the branch, run `gh workflow run lazyfs-probe.yml --ref claude/r-lazyfs-probe`, wait for the run, record the result in `hardening.md`, and delete the remote branch. Outcomes:
   - the probe passes: as the user decided, M3's nightly workflow runs `just lazyfs` (note it for M3);
   - `/dev/fuse` is missing or the mount fails: the LazyFS check runs on the soak machine (M7's Hetzner server), by hand before each release and nightly from a cron job there. Note it for M3 and M7.
5. A check that fails on LazyFS while the shim variant of the same point passes is a **shim bug** (`00-design.md` § M2). Fix `fs::lossy` so that the shim reproduces it, add a `fs::lossy` unit test for that state, and log it in `hardening.md`. If the failure is in SQLite's durability, stop and report.

### Risks to check in step 3

- `flock`, from `lock_dir`'s `try_lock`, on a FUSE mount: if `Storage::open` fails with an I/O error on `LOCK`, stop and report. Don't add a bypass.
- LazyFS's cache size: `1gb` holds every test. A bigger ingest needs `apply_eviction=false` to keep everything unsynced until the clear.

---

## R2.13 — Spec § Durability, the findings and the gaps

Commit: `docs: add the storage durability table and the M2 findings (R2.13)`

### Spec 03: a new section, `## Durability`, after § 10

It opens with one paragraph: "Every write path, what it syncs and in what order. 'fsync dir' is an fsync of the directory that holds the entry. A failed fsync is fatal for its component (D-R-6), except where the table says otherwise and why. The shim tests run every path under a lossy crash (`crates/storage/src/fs/lossy.rs`, track R M2); LazyFS runs the crash tests on the real binary (`tools/lazyfs/`)."

Then the table. The rows below are its content. Fill the **Test** column with the real test names after R2.3–R2.12, one or more per row:

| Write path | Code | Steps, in order | On fsync failure | Test |
|---|---|---|---|---|
| Data directory layout | `lib.rs` `prepare_layout`, `ensure_dir` | `mkdir` (or it exists) → fsync parent, at every open | open fails | `the_data_dir_layout_is_durable_after_a_crash_before_its_sync` |
| Lock | `lock_dir` | create `LOCK` → `flock` → fsync data dir | open fails | the open tests |
| WAL segment creation | `SegmentWriter::create` | create, or reuse an empty file → fsync `wal/<table>/` | at open: open fails; at rotation: the group fails, the writer stops | crash children, model |
| WAL group, `"always"` | `IoJob::commit` | `write_all` → `fdatasync` → memtable → publish → ack | the writer stops, the group stays as written, `WalFailed` | `a_failed_sync_leaves_the_group_and_does_not_sync_again`, model `LossyCrash` |
| WAL group, interval | `IoJob::commit`, `run` | `write_all` → ack. `fdatasync` at the first group after the interval, at the idle timer, at shutdown, and before rotation | as above | `interval_rotation_syncs_the_old_segment`, the model's interval variant |
| WAL rotation | `IoJob::commit` | fdatasync the old segment (interval) → create the new one → fsync dir | the writer stops | `interval_rotation_syncs_the_old_segment` |
| WAL replay | `recover_table`, `truncate_tail` | torn tail: `set_len` → fsync. Then fsync the last segment → create a new one | open fails | `reopen_syncs_the_replayed_segment` |
| Discarded group | `discard_unacked` | `set_len(mark)` → fsync file; unlink the new segment → fsync dir | logged; the writer is stopping anyway | `a_discarded_segment_stays_removed_after_a_lossy_crash` |
| WAL truncation | `truncate_segments` | unlink covered segments → fsync dir | **retried** by the flush task (the user's decision): a segment that comes back holds only records ≤ `flushed_seq`, which replay skips, and R2.4 made it fully synced | flush tests |
| Local object publish (flush, compaction, rewrite, media) | `put_local`, `publish_local` | each new directory: fsync parent → temp file → write → fsync file → rename → fsync dir → the caller's catalog commit | the store stops (`SyncFailed`) | `a_failed_object_fsync_refuses_later_writes_and_deletes`, crash children |
| Local object delete (GC of obsolete, staged and media objects; orphans) | `Objects::delete`, `delete_len` | unlink → fsync dir → the caller's row delete | the store stops | `a_deleted_object_stays_deleted_after_a_lossy_crash`, `gc_leaves_no_orphan_after_a_lossy_crash` |
| Object tree at open | `remove_temp_files` | unlink temp files → fsync every directory, children first | open fails | `an_object_in_a_directory_left_unsynced_survives_a_lossy_crash` |
| `catalog.db` | `Committer` | SQLite, WAL journal, `synchronous=FULL` | the committer stops | `an_fsync_error_stops_the_committer_until_reopen`, LazyFS |
| S3 objects | `put_remote`, `put_multi` | the PUT or `complete` returned → catalog commit | n/a: no local fsync | `tests/s3.rs` |
| Read cache | `read_cache.rs` | no fsync: blocks and `size` carry a CRC-32C, and a bad one is dropped and fetched again | n/a | `a_torn_size_file_never_serves_short_ranges` |
| Scratch: `tmp/query`, `tmp/compaction` | DataFusion; `create_output` (remote) | no fsync; emptied at open | n/a | the spill cleanup test of `lib.rs` (check its name) |
| Catalog snapshot (N B3) | `snapshot_catalog` | `VACUUM INTO` (SQLite) | the backup fails | N's tests |
| Backup object copies (N B4) | `copy_objects` | hard link, or copy → fsync file; at the end, fsync every directory of `dest` | the backup fails | N's tests |
| Pre-migration copy (N B9) | `catalog/db.rs` | `VACUUM INTO <tmp>` → rename → fsync dir (added by R2.3 when merged) → migrations | open fails | `a_pre_migration_copy_survives_a_lossy_crash` |

The rows of N sub-phases that have not merged yet say "(N M2, not merged yet)" in the Code column. If the code doesn't match a step above (this plan was written before the code), correct the row and say so in the report.

### `docs/research/storage/hardening.md`

Add `## M2 — Durability`, with:
- one paragraph: the seam, lossy mode, the three crash modes, where lossy crashes run (model test, crash children, unit tests), and LazyFS;
- `### Runs`: the R2.9 long run, R2.10 with `CETRA_LOSSY_SEEDS=10`, R2.11 with `CETRA_HOOK_CRASH_CASES=500`, `just lazyfs`, and the GitHub probe. For each: date, commit, command, machine, duration, result;
- `### Findings`: one row per audit item, `M2-1` to `M2-8`. Each row has the item, its test, how the test failed before the fix (the assertion message), the cause and the fix commit. The items are 1 (rotation), 1b (open), 2 (GC unlink), 3a (second fsync), 3b (discarded segment), 4 (size file), 5 (catalog errors) and 6 (unsynced directory entries), plus any R2.14 finding;
- `### LazyFS and the shim`: what each one loses (from the probe), which findings each reproduces (run R2.12 step 3's pre-fix check per finding where it applies), and the shim bugs found, if any;
- `### Unverified`: SQLite's WAL recovery after a failed commit fsync (R2.8), and anything else left open.

Then `## Gaps toward deterministic simulation` (D-R-13). For each gap, give the count and where, from these commands (paste the counts):

```bash
grep -rnE 'SystemTime::now|Instant::now|now_ns\(\)' crates/storage/src --include=*.rs | grep -v test
grep -rnE 'tokio::time::(sleep|interval|timeout|Instant)' crates/storage/src --include=*.rs | grep -v test
grep -rn 'spawn_blocking' crates/storage/src --include=*.rs | grep -v test
grep -rnE 'now_v7|new_v7' crates/storage/src --include=*.rs | grep -v test
grep -rnE 'HashMap|HashSet' crates/storage/src --include=*.rs | grep -v test
```

The gaps to list:
- wall-clock reads: `now_ns`, `SystemTime`, `Instant`, and the tokio timers of the writer's group wait and sync interval, and of the flush, GC, compaction and disk-monitor intervals;
- threads outside the async runtime: `spawn_blocking` for WAL commits, object writes, Parquet encoding, replay and cleanup;
- UUIDv7 randomness in file names, temp names and deletion ids;
- hash iteration order (`RandomState`);
- SQLite's own I/O, which would need a VFS;
- DataFusion's reads and spill files, outside the seam;
- `object_store`'s HTTP client and its retries;
- tokio's multi-thread scheduling.

And what M2 closed: every local file call of the crate, statvfs included, goes through `crate::fs`.

---

## R2.14 — Long runs and fixes

Commit per bug: `storage: <fix in imperative> (R2.14)`. A final docs commit: `docs: record the M2 runs (R2.14)`.

1. Run these in the background, each with its output in a log file under the scratchpad:
   - `CETRA_MODEL_CASES=5000 cargo test --release --workspace --all-features -- model_tests`, twice, each time with a fresh `PROPTEST_RNG_SEED` (print it);
   - for each of the five groups, `CETRA_LOSSY_SEEDS=20 cargo test --workspace --all-features -- --ignored --exact crash_tests::every_<group>_hook_survives_a_lossy_crash`;
   - `CETRA_HOOK_CRASH_CASES=500 cargo test --workspace --all-features -- --ignored --exact model_tests::hook_crash::every_model_hook_crash_reads_back` (R2.11);
   - `just lazyfs`, three times.
2. **For each failure**, follow R1.8's procedure:
   1. Keep the reproducer: the proptest seed file (`crates/storage/proptest-regressions/model_tests/mod.txt`), the group, point and seed, or R2.11's case number.
   2. Add a named regression test: a `regression_<what>` that calls `run_case_with`, or a unit test that replays the child's work in-process with that seed.
   3. Decide whose bug it is:
      - **the shim** models a state no filesystem allows, or misses one that LazyFS shows: fix `fs::lossy`, and cite the rule (POSIX, the ext4 behaviour, or the LazyFS run);
      - **the model** (the interval-mode prefix bookkeeping of R2.9): fix it, and cite spec 03 §1;
      - **storage**: find the cause. A local fix (one module, no format, public API or spec semantic change) goes in the same commit as its test. Otherwise, stop and report the case, the cause and the change it needs.
   4. Add a row to `hardening.md` § M2 Findings.
3. When the runs pass, record them in § Runs, and update the roadmap row R: "M2 done".

If a storage cause is not found within the code paths the failing check touches, stop and report. Don't guess.

---

## What not to build in M2

- **A deterministic simulator** (D-R-13): only the list of gaps.
- **Fault injection beyond fsync failures and lost writes:** read errors, `EIO` on write, `ENOSPC`, slow disks. `ENOSPC` and full disks belong to M5, read corruption to M3 (fuzzing).
- **Lossy mode for S3:** that is M4's `ChaosStore`.
- **A custom SQLite VFS** to bring `catalog.db` into the shim. LazyFS covers it (D-R-5).
- **A trait for the seam, or an `Fs` value threaded through constructors.** The seam is one module of functions and two concrete types. Lossy state is found by path, in a test-only registry, because it must outlive a `Storage` (it spans lives) and cover code that has no handle (replay, open cleanup).
- **Fsync in the read cache or the spill directories:** they are rebuilt or fetched again by design, and the CRC makes them safe.
- **The nightly workflow:** M3. R2.12 only probes whether it can run LazyFS.
- **Changes to `crates/server/`:** requested from the coordinator (R2.7).
