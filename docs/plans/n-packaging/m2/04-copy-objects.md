# B4 — `Storage::copy_objects`

Read [00-design.md](00-design.md) first, especially § Design › Consistency step 6.

**Goal:** in local mode, `Storage::copy_objects(&snapshot, dest)` puts every object a snapshot lists
under `dest`, at the same relative key. It hard-links each one, and copies it when `dest` is on
another filesystem. In S3 mode it refuses: objects stay in the bucket (D-N-24).

**Edit only:** `crates/storage/src/snapshot.rs`, `crates/storage/src/lib.rs`, `crates/storage/src/error.rs`.

**Commit:** `Add Storage::copy_objects`

**Needs:** B3.

## Read first

- `objects.rs`: `Objects::local_root()`, `table_file_path`, `media_path`.
- B3's `CatalogSnapshot` and `LiveObject`.

## Changes

### `lib.rs`

```rust
/// Puts every object of `snapshot` under `dest` at its store-relative key: a
/// hard link, or a copy when `dest` is on another filesystem (N M2, D-N-18).
/// Local mode only.
///
/// # Errors
///
/// [`CopyObjectsError::RemoteStore`] in S3 mode; [`CopyObjectsError::Io`]
/// when an object is missing or cannot be linked or copied.
pub async fn copy_objects(
    &self,
    snapshot: &CatalogSnapshot,
    dest: &Path,
) -> Result<CopyReport, CopyObjectsError>
```

```rust
/// What [`Storage::copy_objects`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CopyReport {
    pub count: u64,
    pub bytes: u64,
    pub linked: u64,
    pub copied: u64,
}
```

The algorithm runs in `tokio::task::spawn_blocking`, because it is filesystem work:

1. `let Some(root) = self.objects.local_root() else { return Err(RemoteStore) };`
2. For each `LiveObject`:
   1. `src = root.join(&key)` and `dst = dest.join(&key)`.
   2. `std::fs::create_dir_all(dst.parent())`.
   3. Call a private `fn link_or_copy(src: &Path, dst: &Path, force_copy: bool) -> io::Result<Linked>`,
      where `Linked` is `Linked::Hard` or `Linked::Copied`:
      - unless `force_copy`, it tries `std::fs::hard_link(src, dst)`;
      - on an error of kind `CrossesDevices` (or `raw_os_error() == Some(18)`), or when
        `force_copy` is true, it runs `std::fs::copy(src, dst)` and `File::open(dst)?.sync_all()`;
      - it returns any other error.

      `copy_objects` passes `force_copy = false`.
   4. Any other error, a missing source included, returns
      `CopyObjectsError::Io { key_index, source }`. `key_index` is the object's index in the list, so
      the error text carries no path; paths name blob hashes.
   5. Count `linked` or `copied`, and add `size_bytes` to `bytes`.
3. Finally, `fsync` every directory created, walking `dest` once at the end.

Re-export `CopyReport` and `CopyObjectsError` from `lib.rs` (`pub use`), so the server can name
them at the crate root.

### `error.rs`

```rust
/// Why [`crate::Storage::copy_objects`] failed.
#[derive(Debug, thiserror::Error)]
pub enum CopyObjectsError {
    #[error("objects are in a remote store; back up the bucket instead")]
    RemoteStore,
    #[error("object {key_index} of the snapshot: {source}")]
    Io { key_index: usize, source: std::io::Error },
}
```

## Tests

Local-mode storage from the test helpers; `dest` is a temp directory on the same filesystem.

- `copy_objects_links_every_object`:
  1. Append spans and a media blob, flush, snapshot.
  2. `copy_objects` returns `count == snapshot.objects.len()`, `linked == count` and `copied == 0`.
  3. Every `dest/key` exists with the right size, and on Unix `metadata().nlink() >= 2`.
- `a_copied_backup_opens_and_answers_queries`. This is the core restore test:
  1. Append spans and scores, flush, snapshot to `backup/catalog.db`, copy the objects to
     `backup/objects`.
  2. Shut the storage down.
  3. Open a new `Storage` on a fresh data dir: copy `backup/catalog.db` to `<new>/catalog.db`, and
     set `object_store` to `Local(<backup>/objects)`.
  4. A query returns every row appended before the snapshot. Check this before appending anything:
     later flushes write into `<backup>/objects`.
  5. Appending works, and its receipt's seq is greater than the snapshot's `flushed_seq` (no reuse).
- `link_or_copy_copies_when_forced`: `link_or_copy(src, dst, true)` returns `Copied`. The content is
  equal, and on Unix `nlink == 1`. `EXDEV` itself cannot be forced portably, so the copy branch is
  tested this way.
- `copy_objects_reports_an_unlinkable_object`: create a directory at one object's destination path
  first; `copy_objects` returns `Io`.
- `copy_objects_refuses_a_remote_store`: open the storage with `Storage::open_with_store` and an
  `object_store::memory::InMemory` store, as the remote-store test in `lib.rs` does. Snapshot, then
  `copy_objects` returns `RemoteStore`.

## Verify

```bash
cargo test -p cetra-storage --all-features copy_objects
cargo test -p cetra-storage --all-features a_copied_backup_opens_and_answers_queries
just check
```

## Done when

- [ ] `Storage::copy_objects`, `CopyReport` and `CopyObjectsError` exist.
- [ ] The restore test passes.
- [ ] `just check` passes. One commit: `Add Storage::copy_objects`.
