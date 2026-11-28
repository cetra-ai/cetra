# Strata v0 · M1: container and narrow layer

Implementation plan for phases 1, 2a, 2b and 2c of [the prototype design](../prototype-plan.md). Four sub-phases (M1.1–M1.4), each done by one Haiku agent in one go, in order, each ending with the checks of § Conventions green and one commit.

**Prerequisites:** phase 0 is committed: the `fmt-testdata` crate exists at `docs/research/file-format/testdata/` and exposes `fmt_testdata::datagen`.

**Read first:**
- `AGENTS.md` § Rules (errors, safety, tests). The research crates follow them, except that they live outside the workspace.
- [`../prototype-plan.md`](../prototype-plan.md): § Conventions (all structures), S1, S2, S3, and § Scope.
- `crates/core/src/schema.rs` (`spans_schema()`): the column types M1 must store.
- `docs/research/file-format/testdata/src/datagen.rs`: `World`, `Rng`, `World::trace`.
- `docs/research/file-format/bench/src/counting.rs`: the counting `ObjectStore` that M1.1 copies.

## What M1 is, and what it is not

When M1 is done:
- `StrataWriter` writes a file holding every column of the `spans` schema through the narrow layer (S3). No payload layer yet: in M1 every Utf8 column is narrow, however long its values.
- `StrataFile::open` opens such a file from any `ObjectStore` with one suffix read.
- `read_column` decodes one whole column back to an Arrow array equal to the input.

**Out of scope:**
- the payload layer (M2);
- key filters, map shredding and `lookup` (M3);
- `scan`, predicates, pruning and coalescing (M4);
- DataFusion (M5).

Hot-tail sections 4, 5 and 6 are written **empty** (length 0) in M1.

## Versions

Take each version from this table. These are the versions `bench/Cargo.toml` or the cetra workspace already use, so no lookup is needed, except for `proptest`.

| Crate | Spec | Where | Why |
|---|---|---|---|
| `arrow` | `{ version = "59.2", default-features = false, features = ["ipc"] }` | dep | arrays, row format, IPC: `arrow::ipc::{reader::StreamReader, writer::StreamWriter}` |
| `object_store` | `"0.13"` | dep | storage abstraction |
| `bytes` | `"1.12"` | dep | `Bytes` buffers |
| `futures` | `"0.3.34"` | dep | `BoxFuture` in the counting store |
| `async-trait` | `"0.1.92"` | dev-dep | `ObjectStore` impl in the counting store (tests) |
| `zstd` | `"0.13"` | dep | segment codec 1 |
| `xxhash-rust` | `{ version = "0.8", features = ["xxh3"] }` | dep | checksums |
| `thiserror` | `"2.0"` | dep | errors |
| `tokio` | `{ version = "1.54", features = ["rt-multi-thread", "macros"] }` | dev-dep | async tests |
| `proptest` | `"1.12.0"` (the workspace pin; check with `cargo info proptest` and report the version actually resolved) | dev-dep | property tests |
| `fmt-testdata` | `{ path = "../testdata" }` | dev-dep, added in M1.4 | realistic data |
| `cetra-core` | `{ path = "/home/sebastien/travaux/llm-observer/crates/core" }` | dev-dep (M1.1) | `schema::spans_schema`, `batch::spans_to_record_batch` |
| `parquet` | `{ version = "59.2", default-features = false, features = ["arrow", "zstd"] }` | dev-dep, added in M1.4 | size comparison in `roundtrip_light` |

## Conventions for every sub-phase

- **Build environment.** Run every cargo command with:

  ```bash
  mkdir -p /tmp/claude-1000/-home-sebastien-travaux-llm-observer/16446d53-f065-45fb-a416-4a592a2406df/scratchpad
  export S=/tmp/claude-1000/-home-sebastien-travaux-llm-observer/16446d53-f065-45fb-a416-4a592a2406df/scratchpad
  export CARGO_TARGET_DIR=$S/target-strata RUST_MIN_STACK=134217728
  cd /home/sebastien/travaux/llm-observer/docs/research/file-format/strata
  ```

  `strata` itself does not depend on Vortex, so it needs no libclang.
- **Checks.** A sub-phase is done when, in `strata/`, both of these pass, and `cargo +1.99.1 fmt --check` is clean:

  ```bash
  cargo +1.99.1 test
  cargo +1.99.1 clippy --all-targets -- -D warnings
  ```
- **Lint header.** `lib.rs` starts with:

  ```rust
  #![forbid(unsafe_code)]
  #![warn(missing_docs)]
  #![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
  ```

  Tests may opt out of these with `#[allow(...)]` on the test module.
- **Modules and docs.** Declare modules as private `mod x;`, never `pub mod`. Only the items re-exported in `lib.rs` are public API; they need a doc comment on the item and on every `pub` field and enum variant. Other items are `pub(crate)` and need no docs. Do not add `#[allow(missing_docs)]`.
- **No `[]` with a non-constant index**, even on fixed arrays.
  - Use `.get(..)` / `.get_mut(..)` with `.ok_or(StrataError::Corrupt { .. })` (or `Schema` for writer-side checks), `copy_from_slice` on a sub-slice obtained from `get_mut`, iterators, or `u64::from_le_bytes(arr)` on a `[u8; 8]` from `try_into()`.
  - A constant in-bounds range on a fixed array, such as `header[0..12]` on a `[u8; 16]`, is fine.
  - `#[allow(clippy::indexing_slicing)]` is allowed only on `#[cfg(test)]` modules.
- **No panics on file input.** Every byte read from a file goes through `format::Cursor` (M1.1), whose methods return `Result`. Never index a slice read from a file directly; use `.get(..)`. Arithmetic on values read from the file uses `checked_*`, and a failure returns `StrataError::Corrupt`.
- **Errors.** One enum, `StrataError` in `error.rs`. Messages carry offsets, lengths, column ids and kinds, never row content.
- **Public items** get a doc comment.
- **Tests.**
  - Unit tests sit next to the code in `#[cfg(test)] mod tests`, and may use crate-private items.
  - The **only** integration test target in M1 is `strata/tests/roundtrip_light.rs` (M1.4).
  - Shared test helpers live in `strata/tests/common/` (a subdirectory, so cargo does not build them as targets). Each helper file starts with `#![allow(dead_code)]`. Unit tests load them with `#[cfg(test)] #[path = "../tests/common/<file>.rs"] mod <name>;`.
  - Proptests that promise "N cases" set `ProptestConfig::with_cases(N)`; the default is 256.
- **Commits.** One per sub-phase, with the message given in each sub-phase. Commit `Cargo.lock`. Do not push.
- **Scope.** Only edit files under `docs/research/file-format/strata/`. If something outside it seems necessary, stop and report instead.

## Final layout after M1

```
strata/
  Cargo.toml
  Cargo.lock
  src/
    lib.rs          crate docs, lints, module declarations, re-exports
    config.rs       Config
    error.rs        StrataError
    format.rs       constants, checksum32, Cursor (bounds-checked little-endian reader), Out (writer helpers)
    segment.rs      Kind, Codec, SegmentHeader, encode_segment, decode_segment
    hot.rs          Postscript, HotTail (7 sections), DirectoryRecord, PageRef, file header
    file.rs         StrataFile::open, hot-tail parsing, the block-table cache
    narrow/
      mod.rs        Encoding, BlockEntry, Zone, ChunkFlags
      bitpack.rs    pack / unpack of u64 values at width 0..=64
      chunker.rs    chunk boundaries (4,096 rows / 32 KiB)
      encode.rs     one Arrow slice → (BlockEntry fields, segment body)
      decode.rs     one chunk → ArrayRef
      dict.rs       sorted dictionaries (M1.3)
    writer.rs       StrataWriter, FileSummary
    reader.rs       read_column
    testgen.rs      #[cfg(test)] deterministic test batches (M1.2, extended in M1.3)
  tests/
    common/counting.rs   counting InMemory store, used by unit tests via #[path]
    roundtrip_light.rs   (M1.4; the only integration test target)
```

## Shared definitions

These types are written once, in M1.1 or M1.2 as noted, and used unchanged afterwards.

```rust
// error.rs (M1.1)
#[derive(Debug, thiserror::Error)]
pub enum StrataError {
    #[error("corrupt file: {what} at offset {offset}")]
    Corrupt { what: &'static str, offset: u64 },
    #[error("unsupported: {what}")]
    Unsupported { what: String },          // a type or layout M1 cannot store; the message names the type, never values
    #[error("rows are not sorted by the sort key at row {row}")]
    Unsorted { row: u64 },
    #[error("schema mismatch: {reason}")]
    Schema { reason: String },
    #[error("too many rows: {rows} (limit 4294967295)")]
    TooManyRows { rows: u64 },
    #[error(transparent)]
    Arrow(#[from] arrow::error::ArrowError),
    #[error(transparent)]
    ObjectStore(#[from] object_store::Error),
    #[error("io: {action}")]
    Io { action: &'static str, #[source] source: std::io::Error },
}
pub type Result<T> = std::result::Result<T, StrataError>;

// config.rs (M1.1)
#[derive(Debug, Clone)]
pub struct Config {
    pub zstd_level: i32,              // 1
    pub chunk_max_rows: u32,          // 4096; StrataWriter::new rejects values above 65_535 (Schema), since FSB_RLE run lengths are u16
    pub chunk_max_bytes: usize,       // 32 * 1024
    pub page_max_entries: usize,      // 64
    pub dict_max_values: usize,       // 65_535
    pub dict_max_bytes: usize,        // 64 * 1024
    pub open_default_suffix: u64,     // 256 * 1024
}
impl Default for Config { /* the values in the comments */ }
```

The constants, in `format.rs` (M1.1):

```rust
pub const FILE_MAGIC: &[u8; 4] = b"STRA";
pub const VERSION: u16 = 0;
pub const HEADER_LEN: u64 = 8;              // "STRA" · u16 version · u16 flags
pub const POSTSCRIPT_LEN: u64 = 64;
pub const SEGMENT_MAGIC: u16 = 0x5347;
pub const SEGMENT_HEADER_LEN: usize = 16;
pub const DIRECTORY_RECORD_LEN: usize = 64;
pub const BLOCK_ENTRY_LEN: usize = 56;
pub const PAGE_REF_LEN: usize = 16;
pub const HOT_SECTIONS: usize = 7;
```

---

## M1.1: crate, container and open

**Goal:** a crate that writes and reads the S1 container (header, segments, hot tail with 7 sections, postscript) and opens a file from an `ObjectStore` with one suffix read. There are no columns yet: the directory has zero records.

**Edit:** `strata/{Cargo.toml,src/lib.rs,src/config.rs,src/error.rs,src/format.rs,src/segment.rs,src/hot.rs,src/file.rs}`, `strata/tests/common/counting.rs`.

### Steps

1. **`Cargo.toml`.**
   - `[package] name = "strata"`, `version = "0.0.0"`, `edition = "2024"`, `publish = false`.
   - An empty `[workspace]` table.
   - The dependencies of § Versions; `fmt-testdata` is added in M1.4.

2. **`format.rs`.**
   - The constants above.
   - `pub fn checksum32(parts: &[&[u8]]) -> u32`: feed every part to one `xxhash_rust::xxh3::Xxh3` streaming hasher, take `digest()` (u64), and keep the low 32 bits (`as u32`).
   - `pub struct Cursor<'a> { buf: &'a [u8], pos: usize, base: u64 }`. `base` is the absolute file offset of `buf[0]`, used in error offsets. Methods:
     - `new(buf, base)`;
     - `u8()`, `u16()`, `u32()`, `u64()`, little-endian;
     - `bytes(n, what) -> Result<&'a [u8]>`;
     - `remaining() -> usize`, `position() -> u64` (= `base + pos`).

     Each returns `Err(StrataError::Corrupt { what, offset: self.position() })` when not enough bytes remain. `what` is a `&'static str` passed by the caller: `fn u32(&mut self, what: &'static str) -> Result<u32>`.
   - `pub struct Out(pub Vec<u8>)` with `u8`, `u16`, `u32`, `u64` (little-endian `extend_from_slice`), `bytes(&[u8])`, and `len() -> usize`.

3. **`segment.rs`.**
   - `#[repr(u8)] pub enum Kind`, with the S1 table values:

     | Variant | Value |
     |---|---|
     | `NarrowChunk` | 1 |
     | `PayloadFrame` | 2 |
     | `PayloadRefs` | 3 |
     | `PayloadHeader` | 4 |
     | `PayloadFilters` | 5 |
     | `PayloadEdges` | 6 |
     | `BlockTablePage` | 7 |
     | `Dictionary` | 8 |
     | `KeyFilter` | 9 |

     Plus `fn from_u8(u8) -> Option<Kind>`.
   - `#[repr(u8)] pub enum Codec { Raw = 0, Zstd = 1 }`, plus `from_u8`.
   - `pub fn encode_segment(kind: Kind, body: &[u8], prefer: Codec, level: i32, out: &mut Out) -> SegmentRef`:
     - If `prefer == Zstd`, compress with `zstd::bulk::compress(body, level)`. If that fails, or the result is not strictly smaller than `body`, store raw.
     - Write the 16-byte header: `SEGMENT_MAGIC`, kind, codec, `raw_len = body.len() as u32`, `stored_len`, then `checksum32(&[&header[0..12], stored])`. Then write the stored bytes.
     - Return `SegmentRef { offset: u64, len: u32 }`, where `offset = out.len() as u64` before the header is written, and `len = 16 + stored_len`.
     - The writer builds the **whole file in one `Out` starting at byte 0**, file header first, so `Out`'s length is always the absolute file offset. There is no base parameter. Never build a segment in a separate buffer.
     - A body longer than `(u32::MAX - 16) as usize` bytes is `Unsupported`, so this function returns `Result<SegmentRef>`.
   - `pub struct SegmentRef { pub offset: u64, pub len: u32 }`.
   - `pub fn decode_segment(bytes: &[u8], offset: u64, expect: Kind) -> Result<Vec<u8>>`. `bytes` is exactly the `len` bytes at `offset`; `offset` is used only in error messages.
     - Check the magic, that the kind equals `expect`, that the codec is known, and that `stored_len + 16 == bytes.len()`. Then check the checksum.
     - Raw: return the stored bytes. Zstd: `zstd::bulk::decompress(stored, raw_len as usize)` and check the output length equals `raw_len`.
     - Any failure is `Corrupt { what: "segment …", offset }`, with a distinct `what` per check ("segment magic", "segment kind", "segment codec", "segment length", "segment checksum", "segment decompress").

4. **`hot.rs`.**
   - **File header:** `pub fn write_file_header(out: &mut Out)` writes `FILE_MAGIC`, `VERSION`, flags 0.
   - **`Postscript`:**
     - `pub struct Postscript { pub hot_offset: u64, pub hot_len: u32, pub hot_checksum: u32, pub row_count: u64, pub n_columns: u32, pub version: u16, pub flags: u16 }`.
     - `encode(&self, out)` writes exactly 64 bytes: the S1 fields in order, 28 zero bytes, `FILE_MAGIC`.
     - `decode` also returns `Corrupt` when `version != VERSION` or `flags != 0`.
     - `decode(bytes: &[u8; 64], offset) -> Result<Postscript>` checks the trailing magic and that the 28 reserved bytes are zero.
   - **`DirectoryRecord`:**
     - The 64-byte S2 record, all fields `pub`:

       ```
       column_id u32 · parent u32 · layer u8 · flags u8 · reserved u16 ·
       pages_offset u32 · n_pages u32 · n_chunks u32 ·
       dict_offset u64 · dict_len u32 · payload_offset u64 · payload_len u32 ·
       null_count u64 · reserved2 u64
       ```
     - Field sizes: 4 + 4 + 1 + 1 + 2 + 4 + 4 + 4 + 8 + 4 + 8 + 4 + 8 + 8 = 64.
     - `encode(&self, out)` and `decode(cursor) -> Result<Self>`. Decode requires `reserved == 0` and `reserved2 == 0`.
     - `#[repr(u8)] pub enum Layer { Narrow = 0, Payload = 1, MapParent = 2, MapChild = 3, Residual = 4 }`, with `fn from_u8(u8) -> Option<Layer>`. `DirectoryRecord::decode` returns `Corrupt` for an unknown layer.
   - **`PageRef`:** `{ first_row: u32, offset: u64, len: u32 }`, 16 bytes, with `encode` and `decode`.
   - **`HotTail`:**

     ```rust
     pub struct HotTail {
         pub schema: SchemaRef,
         pub directory: Vec<DirectoryRecord>,
         pub sort_key_entries: Vec<u8>,   // section 3 raw bytes (56-byte entries); M1.2 fills it
         pub sort_key_filter: Vec<u8>,    // section 4, empty in M1
         pub map_keys: Vec<u8>,           // section 5, empty in M1
         pub key_filter_index: Vec<u8>,   // section 6, empty in M1
         pub pages: Vec<u8>,              // section 7 raw bytes; directory pages_offset indexes into it
     }
     ```

     - `encode(&self, out) -> Result<()>` writes the 7 sections, each as a `u32` length followed by the bytes. The length is `u32::try_from(len)`, and an overflow is `Unsupported`.
       - Section 1 is an Arrow IPC stream with the schema and no batch: `let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &schema)?; w.finish()?;`. (`IpcSchemaEncoder` produces a bare `Schema` flatbuffer that `try_schema_from_flatbuffer_bytes` cannot read, so it is not used.)
       - Section 2 is the directory records back to back.
     - `decode(bytes, offset) -> Result<HotTail>`:
       - Section 1 is decoded with `arrow::ipc::reader::StreamReader::try_new(section1, None)` and its `.schema()`; an error becomes `Corrupt { what: "hot schema" }`. The stream must hold no batch: `reader.next()` must be `None`, otherwise `Corrupt`.
       - `HotTail::decode` is only ever called after `file.rs` has verified the hot-tail checksum (step 5.6). arrow-ipc can panic on a forged flatbuffer (`convert.rs:198`), and the checksum makes that unreachable for random corruption.
       - The never-panic proptests of `hot.rs` therefore exercise `HotTail::decode` with valid schema bytes and random bytes in the other sections only. The open-level proptests go through the checksum as usual.
       - Section 2's length must be a multiple of 64.
       - Section 3's length must be a multiple of 56.
       - Section 7's length must be a multiple of 16.
       - Any bytes left after section 7 are `Corrupt`.
     - `pages_offset` is a **byte** offset into the section-7 buffer, always a multiple of 16. `pub fn pages_of(&self, rec: &DirectoryRecord) -> Result<Vec<PageRef>>` decodes `n_pages` refs starting there, through `Cursor` (bounds-checked).

5. **`file.rs`.**
   - `pub struct StrataFile { store: Arc<dyn ObjectStore>, path: object_store::path::Path, size: u64, postscript: Postscript, hot: HotTail }`.
   - `pub async fn open(store: Arc<dyn ObjectStore>, path: Path, hot_len_hint: Option<u32>, config: &Config) -> Result<StrataFile>`:
     1. `n = hot_len_hint.map(|h| h as u64 + POSTSCRIPT_LEN).unwrap_or(config.open_default_suffix)`.
     2. `let r = store.get_opts(&path, GetOptions { range: Some(GetRange::Suffix(n)), ..Default::default() }).await?`. Then `size = r.meta.size`, `start = r.range.start`, `tail = r.bytes().await?`.
     3. If `size < HEADER_LEN + POSTSCRIPT_LEN`: `Corrupt { what: "file too small", offset: 0 }`.
     4. Decode the postscript from the last 64 bytes of `tail`. With checked arithmetic, check that `hot_offset ≥ HEADER_LEN` and `hot_offset + hot_len + 64 == size`.
     5. If `hot_offset < start`, fetch the missing `hot_offset..start` with `store.get_range(&path, hot_offset..start)` (from `object_store::ObjectStoreExt`) and prepend it.
     6. Check `checksum32(&[hot])` against the postscript, then `HotTail::decode`. Check `directory.len() == n_columns`.

     The header at offset 0 is **not** read on open; the postscript's version and flags are checked against `VERSION` and 0.
   - Accessors: `schema()`, `row_count()`, `size()`, `hot_len()`, `directory()`.
   - `pub async fn read_segment(&self, r: &SegmentRef, kind: Kind) -> Result<Vec<u8>>`: one `get_range(offset..offset + len)`, then `decode_segment`.

6. **`lib.rs`.** Dead code: in M1.1 the encoders (`format::Out`, `segment::encode_segment`, the `encode` methods of `hot.rs`, `write_file_header`, `HotTail::pages_of`) are used only by tests; the writer that calls them arrives in M1.2. Put exactly this attribute on each of the module declarations `mod format;`, `mod segment;` and `mod hot;`:

   ```rust
   #[allow(dead_code)] // the writer arrives in M1.2, which removes this attribute
   ```

   This is the only `allow` permitted outside test modules. M1.2 deletes the three attributes, and its clippy run proves nothing is left unused.

   The crate docs, which say:
   - what Strata is;
   - that it is a research prototype outside the cetra workspace;
   - a pointer to `../prototype-plan.md`;
   - the M1 limits from § What M1 is.

   Then the lint header, the modules, and re-exports of `Config`, `StrataError`, `Result` and `StrataFile`.

7. **`tests/common/counting.rs`.**
   - Start the file with `#![allow(dead_code)]`, then copy `bench/src/counting.rs`, wrapping `object_store::memory::InMemory` instead of `LocalFileSystem`.
   - Add a third counter, `gets: AtomicU64`, incremented once per `get_opts` call and read by `pub fn gets(&self) -> u64`. `ObjectStoreExt::get_range` goes through `get_opts`, so each of its calls counts once. `get_ranges` is delegated and not counted in `gets`.
   - Unit tests load it with `#[cfg(test)] #[path = "../tests/common/counting.rs"] mod counting;`.

8. **A test-only writer.** `pub(crate) fn write_empty_file(schema: SchemaRef) -> Vec<u8>` is a helper behind `#[cfg(test)]`, in `hot.rs`. It writes the header, no segments, a hot tail with the schema, zero directory records and empty sections 3–7, and a postscript with `row_count = 0`. M1.2 deletes it and switches its callers to `StrataWriter`.

### Tests

**`segment.rs`:**
- A round trip of 1,000 random bodies (`ProptestConfig::with_cases(1000)`, lengths 0..70,000) with `prefer = Zstd` and `prefer = Raw`. Each case first pushes a random-length (0..100) prefix of zero bytes into the `Out`, so offsets vary.
- An incompressible body is stored with `Codec::Raw`.
- Flipping each byte of a small segment (header and body), one at a time, gives `Err`. A flip in the stored body gives "segment checksum"; flips in the header give a structural error or "segment checksum". Never `Ok` with different bytes, and never a panic.
- **Never panic:** a proptest of random `Vec<u8>` (0..512 bytes) passed to `decode_segment`.

**`hot.rs`:**
- A round trip of `Postscript`, `DirectoryRecord`, `PageRef` and a `HotTail` with 3 directory records and non-empty sections 3 and 7.
- The encoded sizes are exactly 64, 64 and 16 bytes.

**`file.rs` unit tests** (`#[cfg(test)] mod tests` in `src/file.rs`, with `InMemory`, the counting store via `#[path]`, and `write_empty_file`):
- An empty file opens: `row_count() == 0`, and the schema equals `cetra_core::schema::spans_schema()` (dev-dependency, see § Versions).
- With `hot_len_hint = Some(actual hot_len)`, open makes exactly 1 `get`.
- With `hot_len_hint = Some(1)`, open makes exactly 2.
- With no hint, it makes 1 (the 256 KiB default covers the small tail).
- Corruptions each give `Corrupt`: the file truncated by 1 byte; the last 4 bytes changed; a byte flipped inside the hot tail (checksum); `hot_offset` patched to a wrong value.
- **Never panic:**
  - a proptest writes random bytes (0..2,048) to `InMemory` and opens them, and the result is `Ok` or `Err`;
  - a second proptest takes the valid empty file, flips one random byte or truncates at a random length, and opens it, again `Ok` or `Err`, never a panic.

### Done when

§ Conventions checks pass. `cargo +1.99.1 tree -d` (duplicates) is reported in the commit message body.

**Commit:** `strata: add the container, hot tail and open (M1.1)`

---

## M1.2: narrow core

**Goal:** `StrataWriter` writes the integer, float, timestamp, boolean and FixedSizeBinary columns of any schema with the S3 encodings `CONST`, `CONST_NULL`, `FOR_BITPACK`, `PLAIN`, `FSB_RAW`, `FSB_RLE` and `BOOL`, with block-table pages, hot-tail sections 3 and 7, and zones. `read_column` decodes them back. Columns of any other type make `finish` return `Unsupported` until M1.3/M1.4.

**Edit:** `strata/src/{lib.rs,writer.rs,reader.rs,testgen.rs,hot.rs}`, `strata/src/narrow/{mod.rs,bitpack.rs,chunker.rs,encode.rs,decode.rs}`, `strata/src/file.rs` (the block-table page cache).

### Types (`narrow/mod.rs`)

```rust
#[repr(u8)]
pub enum Encoding { Const = 0, ConstNull = 1, ForBitpack = 2, Plain = 3, FsbRaw = 4, FsbRle = 5,
                    Dict = 6, Varbin = 7, ListDict = 8, Bool = 9, Ipc = 10 }

pub mod flags { pub const HAS_NULLS: u8 = 1; pub const HAS_ZONE: u8 = 2; pub const HAS_NAN: u8 = 4; pub const ALL_NULL: u8 = 8; }

pub struct BlockEntry {
    pub first_row: u32, pub n_rows: u32, pub segment_offset: u64, pub segment_len: u32,
    pub encoding: Encoding, pub flags: u8, pub const_len: u16, pub min: [u8; 16], pub max: [u8; 16],
}
```

`Encoding::from_u8(u8) -> Option<Encoding>`. `BlockEntry::encode(&self, out)` writes exactly 56 bytes in the S3 order. `BlockEntry::decode(cursor)` returns `Corrupt` for:
- an unknown encoding value;
- `const_len > 16`;
- `segment_offset == 0` with a non-zero `segment_len`, or the reverse;
- `Const` or `ConstNull` with `segment_len != 0`;
- any other encoding with `segment_len == 0`.

### Supported types in M1.2 and their mapping

| Arrow type | Physical value | Encoding candidates | Zone |
|---|---|---|---|
| Int8, Int16, Int32, Int64, UInt8, UInt16, UInt32 | `i64` (sign- or zero-extended) | CONST / CONST_NULL / FOR_BITPACK | i64 min and max (LE in bytes 0..8) |
| Timestamp(any unit, any tz), Date32, Date64 | `i64` of the underlying primitive | same | same |
| UInt64 | `u64` | CONST / CONST_NULL / PLAIN | none |
| Float32, Float64 | the bits | CONST / CONST_NULL / PLAIN | f64 min and max of non-NaN values, with −0.0 normalised to 0.0; HAS_NAN if any NaN. With no non-NaN value, no zone. |
| Boolean | bit | CONST / CONST_NULL / BOOL | none |
| FixedSizeBinary(w) | `w` bytes | CONST (w ≤ 16) / CONST_NULL / FSB_RAW / FSB_RLE | for w ≤ 16, the min and max values zero-padded, compared bytewise over `w` bytes; none for w > 16 |

### Steps

1. **`bitpack.rs`.**
   - `pub fn pack(values: &[u64], width: u8, out: &mut Vec<u8>)`:
     - width 0 writes nothing;
     - otherwise value *i* occupies bits `[i·width, (i+1)·width)` of a little-endian bit stream, stored as `ceil(n·width/64)` u64 words, each written little-endian, with the last word zero-padded;
     - width 64 copies the values.
   - `pub fn unpack(bytes: &[u8], n: usize, width: u8) -> Result<Vec<u64>>`:
     - checks `width ≤ 64` and that `bytes.len() == ceil(n·width/64)·8`, otherwise `Corrupt`;
     - width 0 returns `n` zeros.
   - `pub fn width_for(max_delta: u64) -> u8 { (64 - max_delta.leading_zeros()) as u8 }`.

   To avoid indexing: work on `chunks_exact(8)` / `u64::from_le_bytes`, and index bits with shifts. If an index is unavoidable, use `.get()`.

2. **`chunker.rs`.** `pub fn chunk_ranges(array: &dyn Array, max_rows: u32, max_bytes: usize) -> Vec<Range<usize>>`:
   - Walk the rows, closing a chunk at `max_rows` rows, or when the accumulated value bytes reach `max_bytes`.
   - Value bytes per row: the fixed width (`data_type().primitive_width()`, or `w` for FixedSizeBinary; Boolean counts as 1). Later sub-phases add Utf8 (`len + 4`) and IPC (see M1.4).
   - An empty array gives no ranges. Every range is non-empty, even when one row exceeds `max_bytes`, and the ranges cover `0..len` in order.

3. **`encode.rs`.** `pub fn encode_chunk(array: &ArrayRef, config: &Config) -> Result<EncodedChunk>`, where:

   ```rust
   pub struct EncodedChunk { pub encoding: Encoding, pub flags: u8, pub const_len: u16,
                             pub min: [u8; 16], pub max: [u8; 16], pub body: Option<Vec<u8>> }
   ```

   Rules, in order:
   1. Every row null: `ConstNull`, flags `ALL_NULL | HAS_NULLS`, no body.
   2. No nulls, and every value bitwise-equal: `Const`, with the value bytes in `min[0..len]` and `const_len = len`. Values per type:
      - integers as the 8 LE bytes of the i64, so `const_len = 8`;
      - floats as their native bytes (4 or 8);
      - Boolean as 1 byte (0 or 1);
      - FixedSizeBinary as the `w` bytes, when `w ≤ 16`.

      No body. Bytes of `min` and `max` beyond the used length are zero. The zone of a `Const` chunk depends on the type:
      - Integers, timestamps, dates, FSB(w ≤ 16), and dictionary codes (M1.3): `HAS_ZONE` set, `max` = the same bytes as `min`.
      - UInt64: its 8 LE bytes, `const_len = 8`, no zone.
      - Floats and Boolean: no zone (`HAS_ZONE` clear, `max` all zero). `min[0..const_len]` holds the native value bytes (4 or 8, or 1). A constant NaN chunk sets `HAS_NAN`.
   3. Otherwise a body is built.
      - The body starts with the validity bitmap when there are nulls: `ceil(n/8)` bytes in Arrow layout (bit *i* set when row *i* is valid), copied from the array's null buffer with any offset applied. Set `HAS_NULLS`.
      - Then the payload for the encoding:
        - **`ForBitpack`:**
          - `min`/`max` are taken over the non-null values;
          - `u = (v as u64).wrapping_sub(min as u64)`, and null slots use `u = 0`;
          - `width = width_for(max u)`;
          - body: `u8 width`, then `pack`.
        - **`Plain`:** values as LE bytes; null slots zero.
        - **`Bool`:** the Arrow values bitmap, `ceil(n/8)` bytes; null slots 0.
        - **`FsbRaw`:** the `n·w` value bytes; null slots zero.
        - **`FsbRle`:** compute runs of equal values over all slots, with nulls taking value zero. Use RLE when `n_runs * 2 < n`; the body is `u16 n_runs`, `u16 run_len[n_runs]`, then `n_runs · w` value bytes. Otherwise `FsbRaw`. Decode requires `n_runs > 0` and `sum(run_len) == n_rows`, else `Corrupt`.
   4. Zone: as in the table. Set `HAS_ZONE` when a zone is stored.

   Any other data type: `Unsupported { what: format!("{dt}") }`.

4. **`decode.rs`.** `pub fn decode_chunk(entry: &BlockEntry, body: Option<&[u8]>, field: &Field) -> Result<ArrayRef>`. It is the inverse of `encode_chunk`:
   - `Const` repeats the value `n_rows` times.
   - `ConstNull` builds `new_null_array(field.data_type(), n_rows)`.
   - Every other encoding requires `body` and splits off the validity bitmap first when `HAS_NULLS` is set.
   - The output array has exactly `field.data_type()`, timezone included.
   - Integer narrowing back to Int8/16/32 and UInt8/16/32 uses checked conversion, and a value out of range is `Corrupt { what: "value out of range" }`.

5. **`writer.rs`.**

   ```rust
   pub struct StrataWriter { schema: SchemaRef, sort_key: Vec<usize>, config: Config, batches: Vec<RecordBatch>, rows: u64 }
   pub struct ColumnSummary { pub name: String, pub encoding_counts: [u32; 11], pub bytes: u64, pub chunks: u32 }
   pub struct FileSummary { pub rows: u64, pub bytes: u64, pub hot_len: u32, pub columns: Vec<ColumnSummary> }
   impl StrataWriter {
       pub fn new(schema: SchemaRef, sort_key: &[&str], config: Config) -> Result<Self>;
       pub fn write(&mut self, batch: &RecordBatch) -> Result<()>;
       pub fn finish(self) -> Result<(Vec<u8>, FileSummary)>;
   }
   ```

   **`new`** resolves the sort-key names to column indexes. An unknown name is `Schema`. M1 requires the first sort-key column to be FixedSizeBinary, or an integer or timestamp type.

   **`write`** checks that `batch.schema()` has fields equal to `self.schema`'s (names, types, nullability), then checks the sort order:
   - `new` builds `arrow::row::RowConverter::new(fields)`, with one `SortField::new(data_type)` per sort-key column, in order (default options: ascending, nulls first). It is stored in a field `converter`.
   - Per batch: `let rows = converter.convert_columns(&[sort-key columns])?`. For each `i ≥ 1`, require `rows.row(i - 1) <= rows.row(i)`. For row 0, compare with `self.last_key: Option<OwnedRow>` (previous batch's last row, kept as `rows.row(n - 1).owned()`).
   - Equal keys are allowed (non-decreasing). The first violating row is `Unsorted { row }`, with `row` the global index from the start of the file.
   - A null in a sort-key column is a `Schema` error: M1 requires non-null sort-key values.
   - Then the batch is stored. Total rows ≥ 2³² is `TooManyRows`.

   **`finish`** (all in memory):
   1. Concatenate each column across the batches (`arrow::compute::concat`).
   2. Create **one** `Out` for the whole file and write the file header into it. Every segment is appended to this `Out` (see `encode_segment`).
   3. For each column in schema order:
      - chunk it, then encode each chunk;
      - write each body as a `NarrowChunk` segment, codec `Zstd` at `config.zstd_level`;
      - build `BlockEntry`s with absolute offsets;
      - group the entries into pages of `config.page_max_entries`, and write each page as a `BlockTablePage` segment with codec `Raw`;
      - append one `PageRef { first_row of the page's first entry, offset, len }` per page to the section-7 buffer, where `len` is the page segment's `16 + stored_len`;
      - fill a `DirectoryRecord`: `layer = Narrow`, `parent = u32::MAX`, `pages_offset` = the section-7 buffer's **byte** length before this column's refs, `n_pages`, `n_chunks`, `null_count`, everything else 0.
   4. Section 3: the encoded `BlockEntry`s of the first sort-key column, all of them, back to back.
   5. Write the hot tail and the postscript.
   6. Return the bytes and the summary. `ColumnSummary.bytes` is the sum of that column's segment lengths, pages included.

   An empty file (no rows) writes zero chunks per column, `n_pages = 0`, and `row_count = 0`.

6. **`file.rs`.**
   - `pub async fn block_entries(&self, column: usize) -> Result<Vec<BlockEntry>>` fetches every page of the column (concurrently, `futures::future::try_join_all`), decodes the entries, and checks them:
     - `first_row` values are strictly increasing, start at 0 and are contiguous (each = previous `first_row + n_rows`);
     - the total equals `row_count`.

     It caches the result per column in a `std::sync::Mutex<HashMap<usize, Arc<Vec<BlockEntry>>>>` and returns `Arc<Vec<BlockEntry>>`.

7. **`reader.rs`.** `pub async fn read_column(file: &StrataFile, column: usize) -> Result<ArrayRef>` fetches every chunk segment of the column concurrently, decodes them, and concatenates them. A file with zero rows returns an empty array of the field's type.

8. **`lib.rs`.** Remove the three `#[allow(dead_code)]` attributes added in M1.1. Add the modules and re-export `StrataWriter`, `FileSummary`, `ColumnSummary` and `read_column`. Delete the M1.1 test-only `write_empty_file`, and switch the M1.1 tests to `StrataWriter` with zero rows. Update `ColumnSummary.encoding_counts` with `if let Some(c) = counts.get_mut(enc as usize) { *c += 1 }`.

9. **`testgen.rs`** (`#[cfg(test)]` module). `pub(crate) fn batch(n: usize) -> RecordBatch` with schema:

   `trace_id FixedSizeBinary(16)` not null · `start_time Timestamp(Nanosecond, Some("UTC"))` not null · `input_tokens Int64` nullable · `cost Float64` nullable · `flag Boolean` not null.

   Row *i*:
   - `trace_id = ((i / 5) as u128).to_be_bytes()`;
   - `start_time = 1_700_000_000_000_000_000 + i as i64 * 1_000_003`;
   - `input_tokens` is null when `i % 7 == 0`, else `(i * 37 % 5000) as i64`;
   - `cost` is null when `i % 11 == 0`, NaN when `i % 997 == 0` (and not null), else `i as f64 * 0.001`;
   - `flag = i % 3 == 0`.

   The rows are sorted by `(trace_id, start_time)` by construction.

   Proptest array strategies throughout M1 are built as `prop::collection::vec(prop::option::of(value_strategy), 0..5_000)` and turned into arrays with `from_iter`.

### Tests

**`bitpack.rs`:**
- A proptest round trip for every width 0..=64, with random `n` in 0..5,000 and values masked to the width.
- `unpack` with a wrong byte length, or width 65, gives `Err`.

**`chunker.rs`:**
- 10,000 Int64 rows give chunks of 4,096, 4,096 and 1,808 rows.
- FixedSizeBinary(16) rows: 32 KiB ÷ 16 = 2,048 rows per chunk.
- An empty array gives `[]`.

**`encode.rs`/`decode.rs`.** A proptest round trip per supported type (`encode_chunk` then `decode_chunk`, compared with `==` on the arrays), with generators covering:
- all-null; all-equal; all-equal with nulls (must be `ForBitpack` width 0, not `Const`); a single row;
- `i64::MIN` and `i64::MAX` in the same chunk (width 64);
- NaN, −0.0 and +0.0 (bitwise round trip);
- Timestamp(Nanosecond, Some("UTC")), whose type must survive;
- FixedSizeBinary(16) with long runs (must pick `FsbRle`) and with all-distinct values (`FsbRaw`);
- sliced arrays with a non-zero offset (the validity bitmap must be re-aligned).

**Plus:**
- A `Const` chunk writes no segment: `body.is_none()`, and the file has no segment for it.
- `"a"` versus `"a\0"` does not apply yet (no Utf8 in M1.2). Instead, two FixedSizeBinary(2) chunks of `[0x61,0x00]` and `[0x61,0x61]` stay distinct.
- **Never panic:** a proptest of random `BlockEntry` fields plus random body bytes passed to `decode_chunk` for each supported type gives `Ok` or `Err`, never a panic.

**End to end, as `#[cfg(test)]` unit tests in `src/reader.rs`** (through `InMemory`; they may read `file.hot` and `file.directory()`):
- `testgen::batch(20_000)`, written in batches of 3,000 rows: write it, open it, `read_column` every column, and compare.
- A row out of order inside a batch, and another exactly at a batch boundary, each give `Unsorted { row }` with the right global row number.
- A Utf8 column gives `Unsupported` from `finish`. M1.3 replaces this test; M1.4 deletes it.
- An empty file round trips.
- Hot-tail section 3 holds exactly `n_chunks(trace_id) × 56` bytes.
- Section 7 has `ceil(n_chunks / 64)` refs per column.

### Done when

The § Conventions checks pass.

**Commit:** `strata: add narrow encodings for numbers, booleans and fixed-size binary (M1.2)`

---

## M1.3: dictionaries

**Goal:** Utf8 columns and List(Utf8) columns whose values fit a file-level sorted dictionary are stored with `DICT` and `LIST_DICT` (and `CONST` for constant chunks).

**Edit:** `strata/src/narrow/{mod.rs,dict.rs,encode.rs,decode.rs,chunker.rs}`, `strata/src/{writer.rs,reader.rs,file.rs,testgen.rs}`.

**Signature change.** This sub-phase changes the M1.2 signatures to `encode_chunk(array: &ArrayRef, dict: Option<&Dictionary>, config: &Config)` and `decode_chunk(entry: &BlockEntry, body: Option<&[u8]>, field: &Field, dict: Option<&Dictionary>)`. Update every M1.2 caller and test in the same commit.

### Steps

1. **`dict.rs`.**
   - `pub struct Dictionary { values: Vec<Vec<u8>> }`, sorted bytewise and unique.
   - `pub fn build(arrays: &[&StringArray], max_values: usize, max_bytes: usize) -> Option<Dictionary>`:
     - collects the distinct non-null values into a `BTreeSet<&[u8]>`;
     - returns `None` when there are more than `max_values` values, or when their total bytes plus `4 × (n + 1)` exceed `max_bytes`.
   - `code_of(&self, v: &[u8]) -> Option<u32>` (binary search) and `value(&self, code: u32) -> Option<&[u8]>`.
   - `encode(&self, out)` writes the S3 layout: `u32 n`, `u32 offsets[n+1]` (offsets[0] = 0, relative to the bytes area), then the bytes.
   - `decode(bytes, offset) -> Result<Dictionary>`:
     - checks that the offsets are monotone and in bounds;
     - checks that the values are strictly increasing bytewise (otherwise `Corrupt { what: "dictionary order" }`);
     - checks that every value is valid UTF-8 (`Corrupt` otherwise).

2. **Writer.** For each Utf8 column, and for each List(Utf8) column, `finish` calls `dict::build` over the whole concatenated column.
   - For a list, the values are `list.values().slice(offsets[0], offsets[n] - offsets[0])`, which respects the array offset; child values under null lists are ignored.
   - `ColumnSummary.bytes` includes the dictionary segment.
   - With `Some(dict)`: write it as a `Dictionary` segment, codec `Zstd`, and set the directory record's `dict_offset`, `dict_len` and flag bit 0 (`flags |= 1`).
   - With `None`: a Utf8 column falls through to `VARBIN` (M1.4). Until M1.4 lands, that is `Unsupported`, and the M1.3 tests only use columns that fit.

3. **Encodings.** `encode_chunk` takes a `dict: Option<&Dictionary>` parameter.
   - **Utf8 with a dictionary:**
     1. All null: `ConstNull`.
     2. No nulls, and all values equal: `Const`, with `min[0..4]` holding the code as u32 LE and `const_len = 4`. The constant is a code, never the string, so the `"a"`/`"a\0"` ambiguity cannot arise.
     3. Otherwise `Dict`: the validity bitmap if there are nulls, then `u8 width` and `pack(codes)`, with null slots coded 0 and the base fixed at 0. The zone is the min and max code (u32 LE in bytes 0..4), with `HAS_ZONE`.
   - **List(Utf8) with a dictionary:**
     1. All null: `ConstNull`.
     2. Any **element** inside a non-null list is null: return `Unsupported { what: "null list element" }`. M1.4 turns this into `IPC`.
     3. Otherwise `ListDict`: the validity bitmap if needed; then `u32 list_offsets[n+1]`, relative so the first is 0, with null lists having zero length; then `u8 width` and the packed codes of all elements in order. No `Const` for lists, and no zone.
   - The chunker counts Utf8 value bytes as `len + 4` per row. For lists it counts the sum of the element lengths, plus 4 per element, plus 4 per row.

4. **Reader.** `read_column` fetches the dictionary segment (if `dict_offset != 0`, using `dict_len`), decodes it once per call, and passes it to `decode_chunk`.
   - A code ≥ the dictionary length is `Corrupt { what: "dictionary code" }`.
   - The output types are exactly `Utf8` and `List(Utf8)`, with the field's child field name and nullability from the schema.

### Tests

- **`dict.rs`:** a round trip; `build` returns `None` at exactly `max_values + 1` values and at `max_bytes + 1` bytes; `decode` rejects unsorted values, duplicates, bad offsets and invalid UTF-8.
- **Encode/decode proptests** for Utf8 (with nulls, empty strings, multi-byte UTF-8) and List(Utf8) (empty lists, null lists, long lists).
- A constant Utf8 chunk is `Const` with `const_len = 4`.
- `"a"` and `"a\0"` in the same column round trip as different values.
- **`testgen`.** Extend `testgen::batch` with five columns. Row *i*:
  - `span_kind` cycles over `["llm", "tool", "agent", "chain", "span"]`;
  - `status` alternates `"ok"` and `"error"`;
  - `model` is null when `i % 4 == 0`, else one of `["gpt-4o", "gpt-4o-mini", "claude-3-5-sonnet", "claude-3-haiku", "llama-3-70b", "gemini-1.5-pro"]` by `i % 6`;
  - `environment` is always `"prod"`;
  - `tags` is a list of `i % 4` elements, element *k* being `["a", "b", "c", "d", "e", "f", "g", "h"][(i + k) % 8]`.
- **End to end** (unit test in `src/reader.rs`): `testgen::batch(20_000)` round trips. `ColumnSummary.encoding_counts` for these five columns shows only `Dict`, `Const` and `ListDict`.
- Replace M1.2's "Utf8 gives Unsupported" test with: a Utf8 column of 65,536 distinct values gives `Unsupported` (no dictionary, and no `VARBIN` until M1.4). M1.4 deletes this test.
- **Never panic:** random bytes passed to `Dictionary::decode`, and random bodies passed to `decode_chunk` for `Dict` and `ListDict`.

### Done when

The § Conventions checks pass.

**Commit:** `strata: add sorted dictionaries, DICT and LIST_DICT (M1.3)`

---

## M1.4: varbin, IPC and the light round trip

**Goal:** every column type of `spans_schema()` can be written. With this sub-phase, a whole light-mode datagen file round trips.

**Edit:** `strata/src/narrow/{encode.rs,decode.rs,chunker.rs}`, `strata/src/{writer.rs,reader.rs}`, `strata/Cargo.toml` (add the `fmt-testdata` and `parquet` dev-dependencies), `strata/tests/roundtrip_light.rs`.

### Steps

1. **`Varbin`** for Utf8 and Binary columns with no dictionary:
   - validity bitmap if needed;
   - `u32 offsets[n+1]`, relative, starting at 0, with null slots having zero length;
   - then the bytes.

   Rules: all null is `ConstNull`; no nulls and every value bitwise-equal with length ≤ 16 is `Const`, with the bytes in `min[0..len]` and `const_len = len`. There is no zone. On decode, Utf8 validates UTF-8 (`Corrupt` otherwise).
2. **`Ipc`** for every other type (Struct, List of non-Utf8, List(Utf8) whose chunk has a null element, Map), and for List(Utf8) columns with no dictionary:
   - the body is the bytes of an Arrow IPC stream: `arrow::ipc::writer::StreamWriter` over a one-column `RecordBatch` with schema `[field]`, one batch, then `finish`;
   - no validity bitmap prefix (IPC carries nulls), and no `HAS_NULLS` flag;
   - all null is still `ConstNull`; no `Const`.
   - **Decode** with `arrow::ipc::reader::StreamReader::try_new(body, None)`, where `body: &[u8]` (a byte slice implements `Read`). Do not use `std::io::Cursor`, which would clash with `format::Cursor`. Validation stays on (the default). Require exactly one batch, one column, and the field's data type, otherwise `Corrupt { what: "ipc chunk" }`.
3. **Chunker.**
   - For `Varbin`, a row counts as `len + 4` bytes.
   - For `Ipc`, measure each 256-row slice with `fn slice_bytes(array: &dyn Array) -> Result<usize>`, defined recursively:
     - **List / LargeList / Map:** `(rows + 1) × offset width` plus `slice_bytes` of the child sliced to `offsets[first] .. offsets[last]`. The child range comes from `value_offsets()`; an empty slice adds 0.
     - **Struct:** the sum of `slice_bytes` of its (already sliced) columns, plus `ceil(rows / 8)` when it has nulls.
     - **Anything else:** `array.to_data().get_slice_memory_size()?`.

     arrow 59.3's `get_slice_memory_size` on a list slice counts the whole child array, which is why the recursion is needed.

     Close the chunk when the sum reaches `chunk_max_bytes`, or at `chunk_max_rows`. Do not use `get_array_memory_size()`.
4. **`writer.rs` type dispatch.** A column is encoded by the first rule that matches:

   | Column type | Encoding |
   |---|---|
   | integer, timestamp, date, float, boolean, FixedSizeBinary | M1.2 |
   | Utf8 with a dictionary | `Dict` |
   | Utf8 without one | `Varbin` |
   | Binary | `Varbin` |
   | List(Utf8) with a dictionary | `ListDict` per chunk, or `Ipc` for a chunk with a null element |
   | List(Utf8) without a dictionary | `Ipc` |
   | anything else | `Ipc` |

   No type is `Unsupported` any more.
5. **`tests/roundtrip_light.rs`.**
   - Generate the spans:

     ```rust
     let world = fmt_testdata::datagen::World::new(7);
     let mut rng = fmt_testdata::datagen::Rng::new(42);
     let mut spans = Vec::new();
     for _ in 0..10_000 { world.trace(&mut rng, &mut spans); }
     ```
   - Clear the payload fields exactly like the `FMT_LIGHT` block in `bench/src/main.rs` (copy that block).
   - Sort with `spans.sort_by_key(|s| (s.trace_id, s.start_time))`.
   - Build batches with `cetra_core::batch::spans_to_record_batch`, one call per 8,192-span chunk (`spans.chunks(8192)`).
   - Write the file with sort key `["trace_id", "start_time"]`, open it from `InMemory`, and `read_column` every column. Compare each with the input column using `==`.
   - Print the `FileSummary`: per column, the bytes and the encoding counts. Also print the file size next to the size of the same batches written with the bench's `pq_default` writer properties (zstd 3, defaults otherwise, via `parquet::arrow::ArrowWriter`; add `parquet = { version = "59.2", default-features = false, features = ["arrow", "zstd"] }` as a dev-dependency).

     These numbers are reported in the commit body. They are not asserted.

### Tests

- Proptests for `Varbin` (Utf8 and Binary: empty strings, nulls, multi-byte UTF-8, a 64 KiB value) and for `Ipc` (`events`-shaped List(Struct), Map(Utf8,Utf8) with empty and null maps, List(Utf8) with null elements).
- A `Const` Varbin chunk distinguishes `"a"` from `"a\0"` through `const_len`.
- A single 100 KiB Utf8 value forms a one-row chunk; the chunker never produces an empty chunk.
- **Never panic:** random bodies passed to `decode_chunk` for `Varbin` and `Ipc`.
- An `events`-shaped List(Struct) column of 100,000 rows chunks into several chunks, each with more than 256 rows (checks the slice measurement).
- Delete M1.3's "65,536 distinct Utf8 values give Unsupported" test, and add one checking that such a column round trips as `Varbin`.
- `roundtrip_light` is always marked `#[ignore]`.

### Done when

The § Conventions checks pass, `cargo +1.99.1 test --release -- --ignored roundtrip_light` passes, and the commit body holds the size table: Strata versus `pq_default`, per column and in total.

**Commit:** `strata: add VARBIN and IPC encodings and the light round trip (M1.4)`
