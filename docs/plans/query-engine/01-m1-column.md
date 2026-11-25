# QE · M1 — `cetra-column` data plane and checksummed binary framing

Milestone 1 of track QE ([`design.md`](design.md)). It builds `crates/column` (`cetra-column`), a pure-Rust, zero-`unsafe`, `tokio`-free columnar in-memory data plane with encoded columns, selection vectors, synchronous hierarchical memory accounting (`MemoryPool` + `FairSpill`), cooperative cancellation (`CancelToken`), and a checksummed, DoS-bounded binary batch frame format (+ LZ4) with a fuzz target. It also adds a temporary `cetra-column` ↔ Arrow `RecordBatch` conversion bridge inside `crates/storage` so M2–M6 can run `ParquetSource` and differential oracle tests without adding any Arrow dependency to `cetra-column`.

When M1 is done:
- `crates/column` exists with zero workspace dependencies and zero `tokio`, `arrow`, `parquet` or `datafusion` dependencies;
- `DataType`, `Field`, `Schema`, `Interval`, `ScalarValue`, `Bitmap`, `Selection`, `FlatColumn`, `DictColumn`, `RleColumn`, `Column` and `Batch` cover every stored column of spec 01 (`spans`, `traces`, `scores`) and every computed Shape B SQL type;
- `GlobalPool`, `ScopedPool`, `Reservation` and `CancelToken` implement process/query/compaction budgeting, `FairSpill` share math, and zero-`tokio` release callbacks;
- `frame::{encode_batch, decode_batch, decode_next}` round-trips every column type and encoding with CRC-32C checksums, LZ4 compression, bomb-ratio checks and zero panics on hostile bytes, backed by the `column_frame` fuzz target and committed seed corpus;
- `crates/storage/src/column_bridge.rs` converts `spans`, `traces`, `scores` and arbitrary SQL Arrow `RecordBatch`es to and from `cetra_column::Batch` losslessly;
- `docs/roadmap.md` and `docs/mvp-decisions.md` record track `QE` and `D-QE-1`–`D-QE-15`, and `just check` is green.

## Prerequisites

- Track QE design doc ([`design.md`](design.md)) is reviewed and cold-reviewed (2026-11-25) on branch `qe-query-engine` in worktree `.claude/worktrees/qe-query-engine`.
- Every phase runs in its own git worktree under `.claude/worktrees/<name>` cut from `qe-query-engine` (or from the merged branch of its predecessor phase).

## Read first

1. `AGENTS.md`: § Rules (no compatibility layers, no unnecessary abstractions, no `unsafe`, no `unwrap`/`expect` outside tests, privacy, dependencies), § Layout, § Crate dependency graph.
2. [`design.md`](design.md): § Goals, § Architecture, § Contracts between parts (Contracts 1 and 6), decisions `D-QE-2`, `D-QE-7`, `D-QE-9`, `D-QE-10`, `D-QE-11`, and § Milestones → M1.
3. `Cargo.toml` (root): `[workspace.lints]` and `[workspace.dependencies]` (`thiserror`, `crc32c`, `lz4_flex`, `proptest`).
4. `crates/storage/src/memory.rs`: `SharedPool`, `QueryPool`, `PoolState`, and `FairSpill` share formula (lines 81–330) — M1 mirrors this exact accounting semantics without DataFusion or `tokio`.
5. `crates/storage/src/wal/record.rs`: `MAX_PAYLOAD`, `MAX_DECODED_BYTES`, `LZ4_MAX_RATIO`, `LZ4_SLACK`, `Decoded`, and `lz4_frame_decodes_to` (lines 23–71, 349–395).
6. `crates/core/src/schema.rs`: `event_field`, `link_field`, `spans_schema()`, `traces_schema()`, `scores_schema()`, and the shapes of `map_utf8()`, `event_fields()`, `link_fields()` (lines 82–100, 215–397).
7. `crates/otel/src/lib.rs:28-30`, `crates/otel/src/fuzz_entry.rs:84-109, 146-174`, `fuzz/Cargo.toml`, `justfile` (`fuzz-seed`, `fuzz_targets`, `_fuzz-max-len`, `_fuzz-max-files`), and `.github/workflows/nightly.yml` — exact fuzz target, gating, seed naming, and replay conventions.

## What it is, and what it is not

**It is:**
- the pure in-memory columnar crate `cetra-column` (types, encodings, `Batch`, `Selection`, `MemoryPool`, `CancelToken`, binary framing, fuzz entry);
- the temporary `cetra-column` ↔ Arrow bridge inside `crates/storage` for M2–M6;
- roadmap and decision log entries for track `QE`.

**It is not:**
- expression evaluation, scalar functions or Filter DSL (M2);
- push-pipeline operators, `ScanSource`, `ParquetSource` or async `admission_wait` (M3);
- any change to `cetra-core`, `cetra-otel`, `cetra-server`, `cetra-cli`, `web/`, WAL production code, or memtable production code (M6–M7).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-QE-2 | Lean, `tokio`-free `cetra-column` crate shared by `cetra-core`, `cetra-query`, `cetra-storage`, `cetra-server` and future Strata; encodings `Flat`, `Const`, `Dict`, `Rle` and `Selection` vectors. |
| D-QE-6 | Buffering operators spill via `cetra-column` binary framing under `MemoryPool` + `FairSpill`. |
| D-QE-7 | Checksummed, DoS-bounded `cetra-column` binary framing (+ LZ4, CRC-32C) for WAL and spill files. |
| D-QE-9 | Temporary Arrow ↔ `cetra-column` bridge lives inside `cetra-storage` (never inside `cetra-column`) for M1–M6 differential testing. |
| D-QE-10 | Dedicated `crates/column` (`cetra-column`) crate with no workspace dependencies. |
| D-QE-11 | `spans.events` and `spans.links` are dedicated typed `Events` and `Links` columns in `cetra-column`. |

User answers relied on (2026-11-25): dedicated `cetra-column` crate; dedicated typed `Events` and `Links` columns; keep `.parquet` export in `cetra-storage`; follow `/home/sebastien/.claude/CLAUDE.md` with Hydrogen (`flash_lite`) implementing phases and Neon (`inherit`) reviewing.

## Versions

M1 adds **zero new external crates**. Every dependency is already pinned in root `Cargo.toml` (`[workspace.dependencies]`, verified 2026-11-25). The implementer re-checks `Cargo.toml` and reports the resolved versions.

| Crate | Version (root `Cargo.toml`) | Where | Why |
|---|---|---|---|
| `thiserror` | `2.0` (workspace) | `crates/column` | Typed `ColumnError` enum (`AGENTS.md` § Errors). |
| `crc32c` | `0.6.9` (workspace) | `crates/column` | CRC-32C checksum on binary batch frames (D-QE-7). |
| `lz4_flex` | `0.14.0`, `default-features = false`, `features = ["std", "frame"]` (workspace) | `crates/column` | Bounded LZ4 frame compression/decompression on binary batch frames (D-QE-7). |
| `proptest` | `1.12.0` (workspace) | `crates/column` `[dev-dependencies]` | Property tests for `Bitmap`, `Selection`, `Column`/`Batch` ops, and binary framing. |
| `cetra-column` | `{ path = "crates/column" }` | root `[workspace.dependencies]`, `crates/storage`, `fuzz/Cargo.toml` | The new crate built in this milestone. |

## Phases and dependency graph

```text
QE1.1 (crate scaffold, error, types, bitmap, selection)
  └── QE1.2 (memory pool, FairSpill, cancel token)
        └── QE1.3 (column encodings, batch ops)
              ├── QE1.4 (binary batch framing + proptests) ── QE1.6 (column_frame fuzz target)
              └── QE1.5 (cetra-storage Arrow ↔ Batch bridge)
                    └── QE1.7 (closeout, roadmap/decisions, full checks — coordinating session)
```

`QE1.4` and `QE1.5` touch disjoint crates (`crates/column` vs `crates/storage` + `Cargo.lock` + `AGENTS.md`) and run **in parallel** after `QE1.3` is merged. `QE1.6` runs after `QE1.4` (and can run in parallel with `QE1.5`, merging `Cargo.lock` only from `QE1.5`).

| Phase | Edits | Depends on |
|---|---|---|
| QE1.1 Crate scaffold, errors, types, bitmap, selection | root `Cargo.toml`, `Cargo.lock`, `AGENTS.md`, `crates/column/{Cargo.toml,src/{lib.rs,error.rs,types.rs,bitmap.rs,selection.rs}}` | — |
| QE1.2 Memory pool, FairSpill, cancel token | `crates/column/src/{lib.rs,memory.rs,cancel.rs}` | QE1.1 |
| QE1.3 Column encodings and `Batch` | `crates/column/src/{lib.rs,column.rs,batch.rs}` | QE1.2 |
| QE1.4 Checksummed binary batch framing | `crates/column/src/{lib.rs,frame.rs}` | QE1.3 |
| QE1.5 Temporary Arrow ↔ `cetra-column` bridge | root `Cargo.lock`, `AGENTS.md`, `crates/storage/{Cargo.toml,src/{lib.rs,column_bridge.rs}}` | QE1.3 |
| QE1.6 `column_frame` fuzz target and seed corpus | `crates/column/src/{lib.rs,fuzz_entry.rs}`, `fuzz/Cargo.toml`, `fuzz/fuzz_targets/column_frame.rs`, `fuzz/corpus/column_frame/*`, `justfile`, `.github/workflows/nightly.yml` | QE1.4 |
| QE1.7 Milestone closeout (coordinating session) | `docs/roadmap.md`, `docs/mvp-decisions.md`, `fuzz/corpus/column_frame/*`, any check fixes | QE1.5, QE1.6 |

## Conventions for every phase

- **Safe Rust only:** `#![forbid(unsafe_code)]` is enforced by `[workspace.lints.rust]`. Never write `unsafe`.
- **No `unwrap()` / `expect()` outside `#[cfg(test)]`**, unless an inline comment next to it states the exact invariant that makes it infallible.
- **Privacy (`AGENTS.md` § Privacy):** `Debug` implementations on `EventItem`, `LinkItem`, `ScalarValue`, `Utf8Data`, `BinaryData`, `MapData`, `EventsData`, `LinksData`, `FlatData`, `FlatColumn`, `DictColumn`, `RleColumn`, `Column` and `Batch` **must never** print string bytes, binary bytes, map keys/values, or event/link names/attributes. Print only types, counts and byte lengths. `ColumnError` messages never contain row values.
- **Lint cleanliness:** `missing_debug_implementations`, `unreachable_pub`, `unused_qualifications`, `unwrap_used`, `expect_used`, `needless_pass_by_value`, `redundant_clone`, `semicolon_if_nothing_returned`, `inefficient_to_string`, and `clippy::large_enum_variant` are workspace lints promoted to errors by `just lint`.
  - `crates/column/src/lib.rs` starts with a `//!` crate doc comment and `#![warn(missing_docs)]`. Every `pub` module, type, variant, field and function gets a one-line `///` doc comment.
  - Because `unused_qualifications` is enabled, **import** `ColumnError` via `use crate::ColumnError;` inside each module and refer to it unqualified as `ColumnError` (never mix `use crate::ColumnError;` with `crate::ColumnError`).
  - **Module visibility in `crates/column/src/lib.rs`:** expose `pub mod` for each module as it is added (`pub mod bitmap;`, `pub mod error;`, `pub mod selection;`, `pub mod types;` in QE1.1; `pub mod cancel;`, `pub mod memory;` in QE1.2; `pub mod batch;`, `pub mod column;` in QE1.3; `pub mod frame;` in QE1.4; `#[cfg(any(test, fuzzing))] #[doc(hidden)] pub mod fuzz_entry;` in QE1.6), **and** re-export top-level public types via `pub use` in `lib.rs` so callers can use either `cetra_column::Batch` or `cetra_column::frame::encode_batch`.
- **Equality semantics:** Derived `PartialEq` on `ScalarValue`, `Column`, `Batch`, etc. is **structural** (so `f64::NAN != f64::NAN` and backing buffer bytes at null slots are compared directly). All constructors/transforms (`take`, `materialize`, Arrow bridge) write canonical zero/empty backing values into null slots, and `ScalarValue::semantic_eq`, `Column::semantic_eq`, and `Batch::semantic_eq` compare active rows with `f64::to_bits()`-based NaN equality (`NaN == NaN`) while ignoring backing bytes at null slots (used by round-trip and property tests).
- **Machine load:** While implementing, run only the touched crate's tests (e.g. `cargo test -p cetra-column`), and run `cargo clippy -p <touched> --all-targets -- -D warnings` + `cargo fmt --all -- --check` once right before committing. Never run full workspace `just check` while sibling agents are active.

---

## QE1.1 — Crate scaffold, errors, types, bitmap and selection

**Goal:** Create `crates/column` (`cetra-column`) with zero `tokio`/Arrow/Parquet dependencies, defining its error enum, type system, `Schema`, `ScalarValue`, `Bitmap` and `Selection`.

**Edit:**
- `Cargo.toml` (root): add `cetra-column = { path = "crates/column" }` under `# workspace crates`.
- `Cargo.lock` (updated by `cargo metadata --format-version 1 > /dev/null`, never by hand).
- `AGENTS.md`: add `column/` to § Layout and `cetra-column` (depends on no workspace crate) to § Crate dependency graph.
- `crates/column/Cargo.toml` (new).
- `crates/column/src/lib.rs` (new).
- `crates/column/src/error.rs` (new).
- `crates/column/src/types.rs` (new).
- `crates/column/src/bitmap.rs` (new).
- `crates/column/src/selection.rs` (new).

### Contracts

`crates/column/Cargo.toml`:
```toml
[package]
name = "cetra-column"
description = "Pure-Rust columnar in-memory batches, encodings, memory pool, and binary framing"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true
license.workspace = true

[lints]
workspace = true

[dependencies]
crc32c.workspace = true
lz4_flex.workspace = true
thiserror.workspace = true

[dev-dependencies]
proptest.workspace = true
```

`crates/column/src/error.rs`:
```rust
use thiserror::Error;

/// Which budget refused a memory reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryLimitKind {
    /// A spilling operator exceeded its fair share of the query cap.
    SpillShare,
    /// A query exceeded its per-query cap.
    QueryCap,
    /// The process-wide shared pool has no free bytes left.
    GlobalPool,
}

/// Errors produced by `cetra-column`. Never carries payload values.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ColumnError {
    /// Memory reservation exceeded its limit.
    #[error("memory limit ({kind:?}) exceeded: requested {requested} bytes, limit {limit} bytes")]
    MemoryLimit {
        /// Which budget refused the allocation.
        kind: MemoryLimitKind,
        /// Bytes requested (for `SpillShare`: consumer's resulting total; for `QueryCap`: scope's resulting total; for `GlobalPool`: additional bytes needed from global pool).
        requested: usize,
        /// Budget limit in bytes.
        limit: usize,
    },
    /// Schema, column length, type, or invariant mismatch.
    #[error("invalid column or batch: {reason}")]
    InvalidBatch {
        /// Static diagnostic reason (never payload bytes).
        reason: &'static str,
    },
    /// Invalid or corrupt binary batch frame.
    #[error("invalid batch frame: {reason}")]
    InvalidFrame {
        /// Static diagnostic reason (never payload bytes).
        reason: &'static str,
    },
    /// Value out of range for a target type or offset width.
    #[error("value overflow: {reason}")]
    Overflow {
        /// Static diagnostic reason.
        reason: &'static str,
    },
    /// Invalid UTF-8 bytes or non-char-boundary offset in a string buffer.
    #[error("invalid utf-8 in column data")]
    InvalidUtf8,
}
```

`crates/column/src/types.rs`:
```rust
use std::fmt;
use std::sync::Arc;

use crate::ColumnError;

/// Precision of `Decimal128` used for deterministic cost sums (D-A2-4).
pub const COST_DECIMAL_PRECISION: u8 = 38;
/// Scale of `Decimal128` used for deterministic cost sums (D-A2-4).
pub const COST_DECIMAL_SCALE: i8 = 18;
/// Maximum allowed `Decimal128` precision.
pub const MAX_DECIMAL_PRECISION: u8 = 38;
/// Maximum allowed recursive `DataType::List` nesting depth (`Utf8` = 0, `List(Utf8)` = 1).
pub const MAX_NESTING_DEPTH: usize = 8;

/// Calendar-aware SQL interval (`months`, `days`, `nanos`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Interval {
    /// Whole calendar months (may be negative).
    pub months: i32,
    /// Whole calendar days (may be negative).
    pub days: i32,
    /// Nanoseconds within a day (may be negative).
    pub nanos: i64,
}

/// Concrete data types supported by `cetra-column`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DataType {
    /// Untyped SQL `NULL`.
    Null,
    /// Boolean.
    Bool,
    /// Signed 64-bit integer.
    I64,
    /// IEEE-754 64-bit float.
    F64,
    /// Fixed-precision 128-bit decimal (`1 <= precision <= 38`, `-precision <= scale <= precision`).
    Decimal128 {
        /// Total decimal digits (`1..=38`).
        precision: u8,
        /// Fractional digits.
        scale: i8,
    },
    /// Nanoseconds since Unix epoch (`utc = true` for stored/zoned timestamps, `false` for naive).
    TimestampNs {
        /// Whether the instant is UTC-zoned.
        utc: bool,
    },
    /// Days since Unix epoch (1970-01-01).
    Date,
    /// Calendar interval (`months, days, nanos`).
    Interval,
    /// UTF-8 string.
    Utf8,
    /// Variable-length binary bytes.
    Binary,
    /// Fixed 8-byte binary (`SpanId`).
    FixedBytes8,
    /// Fixed 16-byte binary (`TraceId`).
    FixedBytes16,
    /// Homogeneous list of elements.
    List(Box<DataType>),
    /// Map of non-null UTF-8 keys to nullable UTF-8 values.
    MapUtf8,
    /// Typed list of span events (`time: TimestampNs(utc), name: Utf8, attributes: Option<Utf8>`).
    Events,
    /// Typed list of span links (`trace_id: FixedBytes16, span_id: FixedBytes8, attributes: Option<Utf8>`).
    Links,
}

impl DataType {
    /// `Decimal128(38, 18)` used for deterministic cost sums.
    pub const fn cost_decimal() -> Self {
        Self::Decimal128 {
            precision: COST_DECIMAL_PRECISION,
            scale: COST_DECIMAL_SCALE,
        }
    }

    /// UTC nanosecond timestamp (`TimestampNs { utc: true }`).
    pub const fn timestamp_utc() -> Self {
        Self::TimestampNs { utc: true }
    }

    /// Validates `Decimal128` precision/scale bounds and recursive `List` nesting depth
    /// (`Utf8` has depth 0, `List(Utf8)` has depth 1; fails with `InvalidBatch` if `depth > max_depth`).
    pub fn validate(&self, max_depth: usize) -> Result<(), ColumnError>;

    /// Unified cetra `columns[].type` display name (D-QE-15).
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool => "boolean",
            Self::I64 => "integer",
            Self::F64 => "float",
            Self::Decimal128 { .. } => "decimal",
            Self::TimestampNs { .. } => "timestamp",
            Self::Date => "date",
            Self::Interval => "interval",
            Self::Utf8 => "string",
            Self::Binary | Self::FixedBytes8 | Self::FixedBytes16 => "binary",
            Self::List(inner) if matches!(inner.as_ref(), Self::Utf8) => "string_list",
            Self::List(_) => "list",
            Self::MapUtf8 => "string_map",
            Self::Events | Self::Links => "json",
        }
    }
}

/// One column in a [`Schema`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    /// Column name.
    pub name: String,
    /// Column data type.
    pub data_type: DataType,
    /// Whether rows in this column may be null.
    pub nullable: bool,
}

impl Field {
    /// Creates a new field.
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self;
}

/// Ordered list of fields describing a batch. Duplicate field names are permitted
/// (e.g. intermediate SQL projections); [`Self::index_of`] returns the first match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    /// Builds a schema; rejects empty field names or invalid data types (`MAX_NESTING_DEPTH`).
    pub fn try_new(fields: Vec<Field>) -> Result<Self, ColumnError>;
    /// Builds a schema wrapped in `Arc`.
    pub fn try_new_arc(fields: Vec<Field>) -> Result<Arc<Self>, ColumnError>;
    /// Fields in column order.
    pub fn fields(&self) -> &[Field];
    /// Number of columns.
    pub fn len(&self) -> usize;
    /// True when the schema has zero columns.
    pub fn is_empty(&self) -> bool;
    /// First column index with exact name `name`.
    pub fn index_of(&self, name: &str) -> Option<usize>;
    /// Projects columns by index (duplicate indices allowed), rejecting out-of-bounds indices.
    pub fn project(&self, indices: &[usize]) -> Result<Self, ColumnError>;
}

/// One typed span event item. `Debug` prints only `time` and string byte lengths.
#[derive(Clone, PartialEq, Eq)]
pub struct EventItem {
    /// Event timestamp in UTC nanoseconds.
    pub time: i64,
    /// Event name.
    pub name: String,
    /// Optional JSON attributes string.
    pub attributes: Option<String>,
}

/// One typed span link item. `Debug` prints only `attributes` byte length.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkItem {
    /// Linked 16-byte trace id.
    pub trace_id: [u8; 16],
    /// Linked 8-byte span id.
    pub span_id: [u8; 8],
    /// Optional JSON attributes string.
    pub attributes: Option<String>,
}

/// Single scalar value used by `Column::Const` and literals.
///
/// `Debug` prints only types, counts and byte lengths — never payload content.
#[derive(Clone, PartialEq)]
pub enum ScalarValue {
    /// Typed or untyped null.
    Null(DataType),
    /// Boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    I64(i64),
    /// IEEE-754 64-bit float.
    F64(f64),
    /// Fixed-precision 128-bit decimal (validation checks precision/scale bounds, not value magnitude).
    Decimal128 {
        /// Unscaled integer value.
        value: i128,
        /// Precision (`1..=38`).
        precision: u8,
        /// Scale.
        scale: i8,
    },
    /// Nanosecond timestamp.
    TimestampNs {
        /// Nanoseconds since epoch.
        value: i64,
        /// True when UTC-zoned.
        utc: bool,
    },
    /// Days since Unix epoch.
    Date(i32),
    /// Calendar interval.
    Interval(Interval),
    /// UTF-8 string.
    Utf8(String),
    /// Variable-width binary.
    Binary(Vec<u8>),
    /// 8-byte fixed binary (`SpanId`).
    FixedBytes8([u8; 8]),
    /// 16-byte fixed binary (`TraceId`).
    FixedBytes16([u8; 16]),
    /// Homogeneous list scalar.
    List {
        /// Element type.
        item_type: DataType,
        /// List elements (each matches `item_type` or is `Null(item_type)`).
        values: Vec<ScalarValue>,
    },
    /// String-to-nullable-string map scalar.
    MapUtf8(Vec<(String, Option<String>)>),
    /// Events list scalar.
    Events(Vec<EventItem>),
    /// Links list scalar.
    Links(Vec<LinkItem>),
}

impl ScalarValue {
    /// Concrete [`DataType`] of this scalar.
    pub fn data_type(&self) -> DataType;
    /// True if `self` is [`ScalarValue::Null`].
    pub fn is_null(&self) -> bool;
    /// Validates nested `DataType` bounds, `Decimal128` precision/scale, and `List` element types.
    pub fn validate(&self, max_depth: usize) -> Result<(), ColumnError>;
    /// NaN-aware semantic equality (`f64::NAN` equals `f64::NAN`, compares nested values recursively).
    pub fn semantic_eq(&self, other: &Self) -> bool;
    /// Deterministic owned logical heap bytes (excludes `Vec::capacity`):
    /// - `Utf8(s)` → `s.len()`
    /// - `Binary(b)` → `b.len()`
    /// - `List { values, .. }` → `values.len() * size_of::<ScalarValue>() + sum(v.heap_bytes())`
    /// - `MapUtf8(m)` → `m.len() * size_of::<(String, Option<String>)>() + sum(k.len() + v.map_or(0, len))`
    /// - `Events(e)` → `e.len() * size_of::<EventItem>() + sum(ev.name.len() + ev.attributes.map_or(0, len))`
    /// - `Links(l)` → `l.len() * size_of::<LinkItem>() + sum(lk.attributes.map_or(0, len))`
    /// - all fixed-size scalars (`Null`, `Bool`, `I64`, `F64`, `Decimal128`, `TimestampNs`, `Date`, `Interval`, `FixedBytes8`, `FixedBytes16`) → `0`.
    pub fn heap_bytes(&self) -> usize;
}
```

Manual `Debug` for `EventItem`, `LinkItem`, and `ScalarValue`:
- `EventItem`: `f.debug_struct("EventItem").field("time", &self.time).field("name_len", &self.name.len()).field("attributes_len", &self.attributes.as_ref().map(String::len)).finish()`
- `LinkItem`: `f.debug_struct("LinkItem").field("attributes_len", &self.attributes.as_ref().map(String::len)).finish()`
- `ScalarValue`: `Utf8(s)` prints `Utf8(len={})`, `Binary(b)` prints `Binary(len={})`, `FixedBytes8(_)` prints `FixedBytes8`, `FixedBytes16(_)` prints `FixedBytes16`, `MapUtf8(m)` prints `MapUtf8(entries={})`, `Events(e)` prints `Events(len={})`, `Links(l)` prints `Links(len={})`, `List { item_type, values }` prints `List { item_type, len }`; numeric/bool/temporal/null variants print their value/type.

`crates/column/src/bitmap.rs`:
```rust
use crate::ColumnError;

/// Packed bitset storing `len` bits in little-endian `u64` words (LSB-first: bit `i` is
/// `(words[i / 64] >> (i % 64)) & 1 != 0`; in bytes, `(bytes[i / 8] >> (i % 8)) & 1 != 0`).
///
/// Invariants: `words.len() == len.div_ceil(64)` and all trailing bits at indices `>= len`
/// in the final word are zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    /// Creates an all-zero bitmap of `len` bits.
    pub fn new_zeroed(len: usize) -> Self;
    /// Creates an all-one bitmap of `len` bits (trailing bits masked to zero; `len == 0` → empty).
    pub fn new_ones(len: usize) -> Self;
    /// Builds a bitmap from a slice of booleans (`true` = bit set).
    pub fn from_bools(bools: &[bool]) -> Self;
    /// Builds a bitmap from raw `u64` words; requires `words.len() == len.div_ceil(64)` and clears trailing bits.
    pub fn from_words(words: Vec<u64>, len: usize) -> Result<Self, ColumnError>;
    /// Builds a bitmap from little-endian bytes; requires `bytes.len() == len.div_ceil(8)` and clears trailing bits.
    pub fn from_bytes(bytes: &[u8], len: usize) -> Result<Self, ColumnError>;
    /// Serializes to exactly `len.div_ceil(8)` little-endian bytes.
    pub fn to_bytes(&self) -> Vec<u8>;
    /// Total number of bits.
    pub fn len(&self) -> usize;
    /// True when `len == 0`.
    pub fn is_empty(&self) -> bool;
    /// Returns bit at `index`, or `false` if `index >= self.len`.
    pub fn get(&self, index: usize) -> bool;
    /// Sets bit at `index` to `value`; returns `InvalidBatch` if `index >= self.len`.
    pub fn set(&mut self, index: usize, value: bool) -> Result<(), ColumnError>;
    /// Appends one bit.
    pub fn push(&mut self, value: bool);
    /// Number of set bits (`1`).
    pub fn count_ones(&self) -> usize;
    /// Number of clear bits (`0`).
    pub fn count_zeros(&self) -> usize;
    /// True if all `len` bits are `1` (`len == 0` returns `true`).
    pub fn all_ones(&self) -> bool;
    /// True if all `len` bits are `0` (`len == 0` returns `true`).
    pub fn all_zeros(&self) -> bool;
    /// Slices `offset..offset + len` into a new bitmap; rejects `offset + len > self.len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Bitwise AND of two equal-length bitmaps.
    pub fn and(&self, other: &Self) -> Result<Self, ColumnError>;
    /// Bitwise OR of two equal-length bitmaps.
    pub fn or(&self, other: &Self) -> Result<Self, ColumnError>;
    /// Bitwise NOT (trailing bits remain zero).
    pub fn not(&self) -> Self;
    /// Raw backing words.
    pub fn words(&self) -> &[u64];
    /// Deterministic logical byte footprint (`words.len() * 8`).
    pub fn memory_bytes(&self) -> usize;
    /// Iterates indices of all set bits in ascending order.
    pub fn iter_ones(&self) -> impl Iterator<Item = usize> + '_;
}
```

`crates/column/src/selection.rs`:
```rust
use crate::{Bitmap, ColumnError};

/// Active row filter attached to a batch so filters avoid copying payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Every row `0..raw_len` is active in natural order.
    All,
    /// Bit `i == 1` means row `i` is active; `mask.len() == raw_len`.
    Mask(Bitmap),
    /// Active row indices (each `< raw_len`; duplicates and non-sorted order allowed, preserved on compaction).
    Indices(Vec<u32>),
}

impl Selection {
    /// Validates `self` against a batch with `raw_len` physical rows (returns `Overflow` if
    /// `raw_len > u32::MAX as usize`, `InvalidBatch` on mask length mismatch or out-of-bounds index).
    pub fn validate(&self, raw_len: usize) -> Result<(), ColumnError>;
    /// Number of active rows for a batch with `raw_len` physical rows.
    pub fn active_len(&self, raw_len: usize) -> usize;
    /// True when no rows are active.
    pub fn is_empty(&self, raw_len: usize) -> bool;
    /// True when every row `0..raw_len` is active in natural order:
    /// - `All` → `true`
    /// - `Mask(m)` → `m.len() == raw_len && m.all_ones()`
    /// - `Indices(idx)` → `idx.len() == raw_len && idx.iter().enumerate().all(|(i, &v)| v as usize == i)`
    pub fn is_all(&self, raw_len: usize) -> bool;
    /// Materializes active row indices into a `Vec<u32>` (fails with `Overflow` if `raw_len > u32::MAX as usize`).
    pub fn to_indices(&self, raw_len: usize) -> Result<Vec<u32>, ColumnError>;
    /// Slices `self` to physical row range `offset..offset + len` (validating `offset.checked_add(len) <= Some(raw_len)` and `m.len() == raw_len`):
    /// - `All` → `All`
    /// - `Mask(m)` → `Mask(m.slice(offset, len)?)`
    /// - `Indices(idx)` → filters entries with `offset <= i < offset + len` and rebases each to `(i - offset) as u32`, preserving order.
    pub fn slice(&self, raw_len: usize, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Intersects `self` with an additional `mask` of length `raw_len` (validating `mask.len() == raw_len` for all 3 variants):
    /// - `All` → `Mask(mask.clone())`
    /// - `Mask(m)` → `Mask(m.and(mask)?)`
    /// - `Indices(idx)` → `Indices` retaining entries where `mask.get(i as usize)` is `true`, preserving order.
    pub fn intersect_mask(&self, raw_len: usize, mask: &Bitmap) -> Result<Self, ColumnError>;
    /// Deterministic logical heap bytes (`All` → 0, `Mask(m)` → `m.memory_bytes()`, `Indices(v)` → `v.len() * 4`).
    pub fn memory_bytes(&self) -> usize;
}
```

### Steps

1. Update root `Cargo.toml` under `# workspace crates` to add `cetra-column = { path = "crates/column" }`.
2. Update `AGENTS.md` § Layout and § Crate dependency graph to list `crates/column` (`cetra-column`, zero workspace deps).
3. Create `crates/column/Cargo.toml` with the exact manifest above.
4. Create `crates/column/src/error.rs` with `MemoryLimitKind` and `ColumnError`.
5. Create `crates/column/src/types.rs` implementing `Interval`, `DataType` (`validate` checking `1 <= precision <= 38`, `-precision <= scale <= precision`, and recursive `List` depth `<= max_depth`), `Field`, `Schema`, `EventItem`, `LinkItem`, and `ScalarValue` (`validate`, `semantic_eq`, `heap_bytes`, privacy-safe `Debug`).
6. Create `crates/column/src/bitmap.rs` implementing `Bitmap` with trailing-bit masking helper `clear_trailing_bits` (handling `len == 0` and `len % 64 == 0` without shifting by 64!), `iter_ones` via `u64::trailing_zeros`, and `count_ones` via `u64::count_ones`.
7. Create `crates/column/src/selection.rs` implementing `Selection` with exact `validate`, `is_all`, `to_indices`, `slice`, `intersect_mask`, and `memory_bytes`.
8. Create `crates/column/src/lib.rs` with `//!` crate doc comment, `#![warn(missing_docs)]`, `pub mod` for `bitmap`, `error`, `selection`, `types`, and `pub use` re-exports of their public types and constants.
9. Run `cargo metadata --format-version 1 > /dev/null` to update `Cargo.lock`.

### Tests

- `types::tests::display_names_match_d_qe_15` — checks all 16 `DataType` variants including `List(Utf8)` → `"string_list"` vs `List(I64)` → `"list"`.
- `types::tests::decimal_validation_rejects_bad_precision_and_scale` — precision 0, precision 39, scale > precision, scale < -precision.
- `types::tests::list_depth_validation_rejects_excessive_nesting` — depth 8 passes, depth 9 fails.
- `types::tests::scalar_debug_redacts_payloads_and_semantic_eq_handles_nan` — asserts `format!("{scalar:?}")` never contains secret marker text for `Utf8`, `Binary`, `FixedBytes8`, `FixedBytes16`, `MapUtf8`, `Events`, `Links`; asserts `ScalarValue::F64(f64::NAN).semantic_eq(&ScalarValue::F64(f64::NAN))`.
- `bitmap::tests::trailing_bits_are_always_zero` — tests lengths `0, 1, 63, 64, 65, 127, 128, 129` across `new_ones`, `not`, `from_bytes`, `from_words`, `slice`, and rejects wrong word/byte counts.
- `bitmap::tests::round_trip_bytes_and_iter_ones` — `proptest` over `Vec<bool>` (`0..=200`) checking `from_bools`, `to_bytes`/`from_bytes`, `count_ones`, `iter_ones`, `slice`, `and`, `or`, `not`.
- `selection::tests::validate_slice_and_intersect_mask` — `All`, `Mask`, `Indices` (including out-of-order, duplicates, empty, out-of-bounds rejection), verifying `slice` filters and rebases `Indices` and `is_all` works for all 3 variants.

### Done when

- `cargo test -p cetra-column` passes.
- `cargo clippy -p cetra-column --all-targets -- -D warnings` passes.
- `cargo doc -p cetra-column --no-deps` passes with zero warnings.
- `cargo fmt --all -- --check` passes.
- Verify `cargo tree -p cetra-column` contains no `tokio`, `arrow`, `parquet`, or `datafusion`.

**Commit:** `column: scaffold cetra-column with types, bitmap, and selection`

---

## QE1.2 — Synchronous `MemoryPool`, `FairSpill`, and `CancelToken`

**Goal:** Implement the `tokio`-free hierarchical memory pool (`GlobalPool`, `ScopedPool`, `Reservation` with `FairSpill` share math and deadlock-free release hook) and atomic `CancelToken`.

**Edit:**
- `crates/column/src/memory.rs` (new).
- `crates/column/src/cancel.rs` (new).
- `crates/column/src/lib.rs`: add `pub mod cancel;`, `pub mod memory;` and re-export `CancelToken`, `GlobalPool`, `Reservation`, `ScopedPool`.

### Contracts

`crates/column/src/memory.rs`:
```rust
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::{ColumnError, MemoryLimitKind};

/// Process-wide shared memory counter. `tokio`-free: callers that wait asynchronously
/// for admission pass a release hook (e.g. `tokio::sync::Notify::notify_waiters`).
pub struct GlobalPool {
    limit: usize,
    used: AtomicUsize,
    on_release: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl GlobalPool {
    /// Creates a shared pool with `limit` bytes and no release hook.
    pub fn new(limit: usize) -> Arc<Self>;
    /// Creates a shared pool that invokes `on_release` whenever bytes return to it.
    pub fn with_release_hook(
        limit: usize,
        on_release: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self>;
    /// Maximum bytes this pool allows.
    pub fn limit(&self) -> usize;
    /// Bytes currently held across all admitted scopes.
    pub fn used(&self) -> usize;
    /// Atomically takes `initial` bytes and returns a [`ScopedPool`] capped at `cap`.
    /// Returns `None` if `used + initial > limit`.
    pub fn try_admit(self: &Arc<Self>, initial: usize, cap: usize) -> Option<Arc<ScopedPool>>;
    /// Convenience for a standalone pool (e.g. `compaction.max_memory` or tests) with `initial = 0`.
    pub fn standalone(cap: usize) -> Arc<ScopedPool>;
}

/// One query's (or compaction job's) memory pool over a [`GlobalPool`], capped at `cap`.
///
/// Mirrors `crates/storage/src/memory.rs` (D-A1-8, D-A1-13):
/// - `initial` bytes stay held in `GlobalPool` until `ScopedPool` drops.
/// - A spilling consumer may hold at most `(cap - unspillable) / spillable_consumers.max(1)`.
#[derive(Debug)]
pub struct ScopedPool {
    shared: Arc<GlobalPool>,
    initial: usize,
    cap: usize,
    state: Mutex<PoolState>,
}

impl ScopedPool {
    /// Per-scope byte cap.
    pub fn cap(&self) -> usize;
    /// Bytes currently reserved across active [`Reservation`]s in this scope.
    pub fn reserved(&self) -> usize;
    /// Current fair share in bytes for a single spillable consumer.
    pub fn spill_share(&self) -> usize;
    /// Registers a new consumer and returns a zero-byte [`Reservation`].
    pub fn register(self: &Arc<Self>, name: &'static str, can_spill: bool) -> Reservation;
}

/// Guard for one operator's reserved bytes; releases bytes and unregisters on `Drop`.
#[derive(Debug)]
pub struct Reservation {
    pool: Arc<ScopedPool>,
    name: &'static str,
    can_spill: bool,
    size: usize,
}

impl Reservation {
    /// Static operator name (diagnostics only).
    pub fn name(&self) -> &'static str;
    /// True when this consumer participates in `FairSpill` sharing.
    pub fn can_spill(&self) -> bool;
    /// Bytes currently held by this reservation.
    pub fn size(&self) -> usize;
    /// Attempts to reserve `additional` bytes, enforcing `FairSpill`, `cap`, and `GlobalPool`.
    pub fn try_grow(&mut self, additional: usize) -> Result<(), ColumnError>;
    /// Releases `bytes.min(self.size)` bytes, returning surplus above `initial` to `GlobalPool`.
    pub fn shrink(&mut self, bytes: usize);
    /// Releases all bytes currently held by this reservation.
    pub fn free(&mut self);
    /// Grows or shrinks this reservation to exactly `new_size` bytes.
    pub fn resize(&mut self, new_size: usize) -> Result<(), ColumnError>;
    /// Splits off a new independent reservation (same `can_spill`) starting at 0 bytes.
    pub fn new_empty(&self, name: &'static str) -> Self;
}
```

`crates/column/src/cancel.rs`:
```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation flag shared across I/O prefetchers and CPU workers.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// Creates an active (non-cancelled) token.
    pub fn new() -> Self;
    /// Marks the token cancelled; idempotent and visible across threads immediately.
    pub fn cancel(&self);
    /// True once [`Self::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool;
}
```

### Steps

1. Implement `GlobalPool` in `crates/column/src/memory.rs` using `compare_exchange_weak` CAS loops for `try_take` and `give_back` (matching `crates/storage/src/memory.rs:100-135`), firing `on_release` whenever `give_back` decrements `used` by `> 0`. Implement `GlobalPool::standalone(cap)` directly as `Arc::new(ScopedPool::new(Self::new(cap), 0, cap))` (no `unwrap()`). Implement manual `Debug` for `GlobalPool` (printing `limit` and `used`).
2. Implement `#[derive(Debug, Default)] struct PoolState` (`used`, `unspillable`, `spillable`, `spillable_consumers`, `held`) and `ScopedPool` with `lock()` recovering from poison (`unwrap_or_else(PoisonError::into_inner)`).
3. Implement `ScopedPool::drop` giving back `held` bytes to `GlobalPool` (firing `on_release`).
4. Implement `Reservation::try_grow` with exact order of checks and exact `MemoryLimit` fields:
   - zero `additional` returns `Ok(())` immediately;
   - if `can_spill`, compute `share = cap.saturating_sub(state.unspillable) / state.spillable_consumers.max(1)`; if `self.size.checked_add(additional).is_none_or(|after| after > share)`, return `Err(ColumnError::MemoryLimit { kind: MemoryLimitKind::SpillShare, requested: self.size.saturating_add(additional), limit: share })`;
   - `let next = state.used.saturating_add(additional);` if `state.used.checked_add(additional).is_none() || next > cap`, return `Err(ColumnError::MemoryLimit { kind: MemoryLimitKind::QueryCap, requested: next, limit: cap })`;
   - if `next > state.held`, `let delta = next - state.held;`; if `!shared.try_take(delta)`, return `Err(ColumnError::MemoryLimit { kind: MemoryLimitKind::GlobalPool, requested: delta, limit: shared.limit })`;
   - commit `held = held.max(next)`, `used = next`, `spillable`/`unspillable += additional`, `self.size += additional`.
5. Implement `Reservation::shrink(&mut self, bytes: usize)`:
   - **Clamp first:** `let actual = bytes.min(self.size);` if `actual == 0`, return immediately;
   - Under the `ScopedPool` mutex: `self.size -= actual`, subtract `actual` from `state.used` and `state.spillable`/`state.unspillable`, compute `let keep = state.used.max(self.pool.initial);`, `let freed = state.held.saturating_sub(keep);`, `state.held -= freed;`;
   - **Drop the `MutexGuard` before calling `self.pool.shared.give_back(freed)`** (when `freed > 0`) so an `on_release` callback inspecting `ScopedPool` can never deadlock!
6. Implement `Drop for Reservation`: clamp/release `self.size`, decrement `spillable_consumers` if `can_spill`, drop the `MutexGuard`, then call `self.pool.shared.give_back(freed)` if `freed > 0`.
7. Implement `CancelToken` in `crates/column/src/cancel.rs` (`Ordering::Release` on `cancel`, `Ordering::Acquire` on `is_cancelled`).
8. Re-export from `crates/column/src/lib.rs`.

### Tests

- `memory::tests::admission_respects_global_limit_and_fires_release_hook` — admits up to limit, rejects next, drops one scope, verifies hook fired and re-admission succeeds.
- `memory::tests::initial_bytes_stay_held_until_scope_drops` — grow above `initial`, shrink to 0, verify `GlobalPool::used()` equals `initial`, drop scope, verify `used() == 0`.
- `memory::tests::shrink_past_self_size_does_not_corrupt_sibling_reservations` — two sibling reservations `a` (100 bytes) and `b` (200 bytes); call `a.shrink(500)`; assert `a.size() == 0`, `b.size() == 200`, `scope.reserved() == 200`.
- `memory::tests::release_hook_may_read_scoped_pool_without_deadlock` — hook inspects `scope.reserved()` on shrink/drop.
- `memory::tests::fair_spill_divides_remaining_after_unspillable` — two spillable + one unspillable consumer; verifies exact `(cap - unspillable) / 2` share and `MemoryLimitKind::SpillShare` error fields.
- `memory::tests::global_pool_exhaustion_leaves_scope_state_unchanged` — failed `try_grow` on `GlobalPool` leaves `reserved()` and `used()` unchanged.
- `memory::tests::concurrent_reservations_return_to_zero` — 8 threads rapidly admitting, growing, shrinking, and dropping scopes against one `GlobalPool`; asserts `used() == 0` at end and `used <= limit`.
- `cancel::tests::clone_observes_cancel_across_threads`.

### Done when

- `cargo test -p cetra-column` passes.
- `cargo clippy -p cetra-column --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check` passes.

**Commit:** `column: add synchronous MemoryPool, FairSpill, and CancelToken`

---

## QE1.3 — Column encodings (`Flat`, `Const`, `Dict`, `Rle`) and `Batch`

**Goal:** Implement all columnar storage buffers (`FlatColumn`, `DictColumn`, `RleColumn`, `Column`) and `Batch` with complete public constructors/accessors, UTF-8 char-boundary validation, deterministic `memory_bytes()`, exact `Reservation` accounting/rollback, `materialize`, `take`, `slice`, `project`, `compact`, `concat`, and `semantic_eq`.

**Edit:**
- `crates/column/src/column.rs` (new).
- `crates/column/src/batch.rs` (new).
- `crates/column/src/lib.rs`: add `pub mod batch;`, `pub mod column;` and re-export all public column/batch types.

### Contracts

`crates/column/src/column.rs`:
```rust
use crate::{Bitmap, ColumnError, DataType, EventItem, Interval, LinkItem, Reservation, ScalarValue};

/// Contiguous UTF-8 string buffer (`offsets.len() == len + 1`, `offsets[0] == 0`, monotonic,
/// `*offsets.last() as usize == bytes.len()`, valid UTF-8 and every offset is a char boundary).
/// `Debug` prints only row count and total byte length.
#[derive(Clone, PartialEq, Eq)]
pub struct Utf8Data {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl Utf8Data {
    /// Validates UTF-8, offset monotonicity, exact end byte length, and UTF-8 char boundaries on every offset.
    pub fn try_new(offsets: Vec<u32>, bytes: Vec<u8>) -> Result<Self, ColumnError>;
    /// Empty string buffer (0 rows).
    pub fn empty() -> Self;
    /// Builds from an iterator of string slices (fails with `Overflow` if total bytes `> u32::MAX`).
    pub fn try_from_slices<'a>(iter: impl IntoIterator<Item = &'a str>) -> Result<Self, ColumnError>;
    /// Number of rows (`offsets.len() - 1`).
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Row offsets (`len + 1` elements).
    pub fn offsets(&self) -> &[u32];
    /// Contiguous UTF-8 bytes.
    pub fn bytes(&self) -> &[u8];
    /// String slice at `row`, or `None` if out of bounds.
    pub fn get(&self, row: usize) -> Option<&str>;
    /// Slices `offset..offset + len`, rebasing offsets to start at 0.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`,
    /// returning `InvalidBatch` on mismatch): when bit `out_pos == 0`, emits `""` without indexing `self`;
    /// otherwise requires `(indices[out_pos] as usize) < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError>;
    /// Deterministic logical bytes (`offsets.len() * 4 + bytes.len()`).
    pub fn memory_bytes(&self) -> usize;
}

/// Contiguous binary buffer (`offsets.len() == len + 1`, `offsets[0] == 0`, monotonic,
/// `*offsets.last() as usize == bytes.len()`). `Debug` prints only row count and byte length.
#[derive(Clone, PartialEq, Eq)]
pub struct BinaryData {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl BinaryData {
    /// Validates offset monotonicity and exact end byte length.
    pub fn try_new(offsets: Vec<u32>, bytes: Vec<u8>) -> Result<Self, ColumnError>;
    /// Empty binary buffer (0 rows).
    pub fn empty() -> Self;
    /// Builds from an iterator of byte slices.
    pub fn try_from_slices<'a>(iter: impl IntoIterator<Item = &'a [u8]>) -> Result<Self, ColumnError>;
    /// Number of rows.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Row offsets (`len + 1` elements).
    pub fn offsets(&self) -> &[u32];
    /// Contiguous raw bytes.
    pub fn bytes(&self) -> &[u8];
    /// Byte slice at `row`, or `None` if out of bounds.
    pub fn get(&self, row: usize) -> Option<&[u8]>;
    /// Slices `offset..offset + len`, rebasing offsets to 0.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits empty bytes without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError>;
    /// Deterministic logical bytes (`offsets.len() * 4 + bytes.len()`).
    pub fn memory_bytes(&self) -> usize;
}

/// Contiguous string-to-nullable-string map column (`offsets.len() == len + 1`,
/// `*offsets.last() as usize == keys.len() == values.len()`). `Debug` prints only counts.
#[derive(Clone, PartialEq, Eq)]
pub struct MapData {
    offsets: Vec<u32>,
    keys: Utf8Data,
    values: Utf8Data,
    value_nulls: Option<Bitmap>,
}

impl MapData {
    /// Validates offsets, equal key/value entry counts, and optional `value_nulls` length (normalizing all-ones to `None`).
    pub fn try_new(
        offsets: Vec<u32>,
        keys: Utf8Data,
        values: Utf8Data,
        value_nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError>;
    /// Empty map column (0 rows).
    pub fn empty() -> Self;
    /// Number of map rows.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Total entries across all rows.
    pub fn entry_count(&self) -> usize;
    /// Row offsets into `keys` and `values`.
    pub fn offsets(&self) -> &[u32];
    /// Non-null map keys.
    pub fn keys(&self) -> &Utf8Data;
    /// Map values (empty string at null positions).
    pub fn values(&self) -> &Utf8Data;
    /// Validity bitmap for `values` (`1` = valid, `0` = null).
    pub fn value_nulls(&self) -> Option<&Bitmap>;
    /// Entries `(key, Option<value>)` for `row`.
    pub fn get(&self, row: usize) -> Option<Vec<(String, Option<String>)>>;
    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits an empty map without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError>;
    /// Deterministic logical bytes.
    pub fn memory_bytes(&self) -> usize;
}

/// Typed `spans.events` column (`offsets.len() == len + 1`,
/// `*offsets.last() as usize == times.len() == names.len() == attributes.len()`).
#[derive(Clone, PartialEq, Eq)]
pub struct EventsData {
    offsets: Vec<u32>,
    times: Vec<i64>,
    names: Utf8Data,
    attributes: Utf8Data,
    attribute_nulls: Option<Bitmap>,
}

impl EventsData {
    /// Validates offsets, equal child lengths, and optional `attribute_nulls` (normalizing all-ones to `None`).
    pub fn try_new(
        offsets: Vec<u32>,
        times: Vec<i64>,
        names: Utf8Data,
        attributes: Utf8Data,
        attribute_nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError>;
    /// Empty events column (0 rows).
    pub fn empty() -> Self;
    /// Number of span rows.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Total event items across all rows.
    pub fn event_count(&self) -> usize;
    /// Row offsets into child event arrays.
    pub fn offsets(&self) -> &[u32];
    /// Event timestamps (UTC ns).
    pub fn times(&self) -> &[i64];
    /// Event names.
    pub fn names(&self) -> &Utf8Data;
    /// Event attributes JSON strings.
    pub fn attributes(&self) -> &Utf8Data;
    /// Validity bitmap for `attributes`.
    pub fn attribute_nulls(&self) -> Option<&Bitmap>;
    /// Event items for `row`.
    pub fn get(&self, row: usize) -> Option<Vec<EventItem>>;
    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits an empty event/link list without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError>;
    /// Deterministic logical bytes.
    pub fn memory_bytes(&self) -> usize;
}

/// Typed `spans.links` column (`offsets.len() == len + 1`,
/// `*offsets.last() as usize == trace_ids.len() == span_ids.len() == attributes.len()`).
#[derive(Clone, PartialEq, Eq)]
pub struct LinksData {
    offsets: Vec<u32>,
    trace_ids: Vec<[u8; 16]>,
    span_ids: Vec<[u8; 8]>,
    attributes: Utf8Data,
    attribute_nulls: Option<Bitmap>,
}

impl LinksData {
    /// Validates offsets, equal child lengths, and optional `attribute_nulls` (normalizing all-ones to `None`).
    pub fn try_new(
        offsets: Vec<u32>,
        trace_ids: Vec<[u8; 16]>,
        span_ids: Vec<[u8; 8]>,
        attributes: Utf8Data,
        attribute_nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError>;
    /// Empty links column (0 rows).
    pub fn empty() -> Self;
    /// Number of span rows.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Total link items across all rows.
    pub fn link_count(&self) -> usize;
    /// Row offsets into child link arrays.
    pub fn offsets(&self) -> &[u32];
    /// Linked 16-byte trace IDs.
    pub fn trace_ids(&self) -> &[[u8; 16]];
    /// Linked 8-byte span IDs.
    pub fn span_ids(&self) -> &[[u8; 8]];
    /// Link attributes JSON strings.
    pub fn attributes(&self) -> &Utf8Data;
    /// Validity bitmap for `attributes`.
    pub fn attribute_nulls(&self) -> Option<&Bitmap>;
    /// Link items for `row`.
    pub fn get(&self, row: usize) -> Option<Vec<LinkItem>>;
    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits an empty event/link list without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError>;
    /// Deterministic logical bytes.
    pub fn memory_bytes(&self) -> usize;
}

/// Typed flat values buffer (one variant per [`DataType`]).
/// Large nested buffers are boxed so `size_of::<FlatData>()` stays well below `clippy::large_enum_variant`.
#[derive(Clone, PartialEq)]
pub enum FlatData {
    /// All-null column of `len` rows.
    Null(usize),
    /// Packed boolean values.
    Bool(Bitmap),
    /// 64-bit integers.
    I64(Vec<i64>),
    /// 64-bit floats.
    F64(Vec<f64>),
    /// Fixed-precision 128-bit decimals.
    Decimal128 {
        /// Unscaled 128-bit values.
        values: Vec<i128>,
        /// Precision (`1..=38`).
        precision: u8,
        /// Scale.
        scale: i8,
    },
    /// Nanosecond timestamps.
    TimestampNs {
        /// Nanoseconds since epoch.
        values: Vec<i64>,
        /// True when UTC-zoned.
        utc: bool,
    },
    /// Days since Unix epoch.
    Date(Vec<i32>),
    /// Calendar intervals.
    Interval(Vec<Interval>),
    /// UTF-8 strings.
    Utf8(Utf8Data),
    /// Variable-width binary.
    Binary(BinaryData),
    /// 8-byte fixed binary (`SpanId`).
    FixedBytes8(Vec<[u8; 8]>),
    /// 16-byte fixed binary (`TraceId`).
    FixedBytes16(Vec<[u8; 16]>),
    /// Homogeneous list (`offsets.len() == len + 1`, `values.len() == *offsets.last() as usize`).
    List {
        /// Element type.
        item_type: DataType,
        /// Row boundaries into `values`.
        offsets: Vec<u32>,
        /// Child values column (may itself be `Flat`, `Const`, `Dict` or `Rle`).
        values: Box<Column>,
    },
    /// String-to-nullable-string maps.
    MapUtf8(Box<MapData>),
    /// Span events.
    Events(Box<EventsData>),
    /// Span links.
    Links(Box<LinksData>),
}

impl FlatData {
    /// Logical row count.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType;
    /// Empty flat data (0 rows) for `data_type`.
    pub fn empty(data_type: &DataType) -> Self;
    /// Deterministic logical heap bytes across all backing buffers.
    pub fn memory_bytes(&self) -> usize;
}

/// Uncompressed column with an optional validity bitmap (`1` = valid, `0` = null).
/// Canonical invariant: `nulls` is `None` when all rows are valid or for `FlatData::Null`.
#[derive(Clone, PartialEq)]
pub struct FlatColumn {
    data: FlatData,
    nulls: Option<Bitmap>,
}

impl FlatColumn {
    /// Validates `data` invariants (`data.data_type().validate(MAX_NESTING_DEPTH)`, decimal bounds,
    /// list offsets/child type/length/child `Const` scalar validity) and `nulls` length.
    /// Normalizes `Some(all_ones)` to `None` and normalizes `FlatData::Null(_)` to `nulls: None`.
    pub fn try_new(data: FlatData, nulls: Option<Bitmap>) -> Result<Self, ColumnError>;
    /// Empty flat column (0 rows) for `data_type`.
    pub fn empty(data_type: &DataType) -> Self;
    /// Underlying flat values buffer.
    pub fn data(&self) -> &FlatData;
    /// Optional validity bitmap (`1` = valid, `0` = null).
    pub fn nulls(&self) -> Option<&Bitmap>;
    /// Row count.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType;
    /// True iff `row < self.len()` and that row is null (`FlatData::Null` is null for all `row < len`).
    pub fn is_null(&self, row: usize) -> bool;
    /// True if at least one row in `0..len` is null.
    pub fn has_nulls(&self) -> bool;
    /// Extracts row `row` as a [`ScalarValue`], or `InvalidBatch` if out of bounds.
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError>;
    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Gathers rows by `indices`, writing canonical zero/empty backing values into null slots
    /// and reserving output `memory_bytes()` from `pool` (rolling back on error).
    pub fn take(&self, indices: &[u32], pool: Option<&mut Reservation>) -> Result<Self, ColumnError>;
    /// NaN-aware, null-slot-ignoring semantic equality.
    pub fn semantic_eq(&self, other: &Self) -> bool;
    /// Deterministic logical heap bytes (`data.memory_bytes() + nulls.map_or(0, Bitmap::memory_bytes)`).
    pub fn memory_bytes(&self) -> usize;
}

/// Dictionary-encoded column (`values` holds distinct entries; every non-null `codes[i] < values.len()`).
#[derive(Clone, PartialEq)]
pub struct DictColumn {
    values: Box<FlatColumn>,
    codes: Vec<u32>,
    nulls: Option<Bitmap>,
}

impl DictColumn {
    /// Validates `nulls` length (normalizing all-ones to `None`) and checks every non-null code `< values.len()`.
    pub fn try_new(
        values: FlatColumn,
        codes: Vec<u32>,
        nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError>;
    /// Dictionary values table.
    pub fn values(&self) -> &FlatColumn;
    /// Dictionary codes (one per row).
    pub fn codes(&self) -> &[u32];
    /// Optional code-level validity bitmap (`1` = valid, `0` = null).
    pub fn nulls(&self) -> Option<&Bitmap>;
    /// Row count (`codes.len()`).
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType;
    /// True iff `row < self.len()` and either code is null or `values.is_null(codes[row])`.
    pub fn is_null(&self, row: usize) -> bool;
    /// True iff `self.nulls.is_some() || (self.values.has_nulls() && self.codes.iter().any(|&c| self.values.is_null(c as usize)))`.
    /// Does not falsely report nulls when `values` holds an unreferenced null entry after slicing.
    pub fn has_nulls(&self) -> bool;
    /// Extracts row `row` as a [`ScalarValue`].
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError>;
    /// Slices `offset..offset + len` (keeps `values`, slices `codes` and `nulls`).
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Deterministic logical heap bytes (`values.memory_bytes() + codes.len() * 4 + nulls`).
    pub fn memory_bytes(&self) -> usize;
}

/// Run-length encoded column (`run_ends.len() == values.len()`; when non-empty,
/// `run_ends[0] > 0` and `run_ends` is strictly increasing; logical length is `*run_ends.last().unwrap_or(&0)`).
#[derive(Clone, PartialEq)]
pub struct RleColumn {
    values: Box<FlatColumn>,
    run_ends: Vec<u32>,
}

impl RleColumn {
    /// Validates `run_ends.len() == values.len()`, `run_ends[0] > 0` (if non-empty), and strict monotonicity.
    pub fn try_new(values: FlatColumn, run_ends: Vec<u32>) -> Result<Self, ColumnError>;
    /// One value per run (null runs encoded via `values.nulls()`).
    pub fn values(&self) -> &FlatColumn;
    /// Exclusive end row index of each run.
    pub fn run_ends(&self) -> &[u32];
    /// Logical row count.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType;
    /// True iff `row < self.len()` and its run value is null.
    pub fn is_null(&self, row: usize) -> bool;
    /// True if any run is null (`values.has_nulls()`).
    pub fn has_nulls(&self) -> bool;
    /// Extracts row `row` as a [`ScalarValue`] via binary search (`partition_point(|&end| (end as usize) <= row)`).
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError>;
    /// Slices `offset..offset + len`, trimming and rebasing `run_ends` to `0..len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Deterministic logical heap bytes (`values.memory_bytes() + run_ends.len() * 4`).
    pub fn memory_bytes(&self) -> usize;
}

/// One column in a [`crate::Batch`].
#[derive(Clone, PartialEq)]
pub enum Column {
    /// Flat uncompressed buffers.
    Flat(FlatColumn),
    /// Single value repeated `len` times.
    Const {
        /// Repeated value (or `ScalarValue::Null(dt)`).
        value: ScalarValue,
        /// Logical row count.
        len: usize,
    },
    /// Dictionary-encoded values.
    Dict(DictColumn),
    /// Run-length encoded values.
    Rle(RleColumn),
}

impl Column {
    /// Checked constructor for `Column::Const` (validates `value.validate(MAX_NESTING_DEPTH)`).
    pub fn try_new_const(value: ScalarValue, len: usize) -> Result<Self, ColumnError>;
    /// Logical row count.
    pub fn len(&self) -> usize;
    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType;
    /// True iff `row < self.len()` and that row is null.
    pub fn is_null(&self, row: usize) -> bool;
    /// True if at least one row in `0..len` is null (`Const { value: Null(_), len: 0 }` returns `false`).
    pub fn has_nulls(&self) -> bool;
    /// Extracts row `row` as a [`ScalarValue`].
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError>;
    /// Materializes `self` into a [`FlatColumn`], reserving exact output `memory_bytes()` from `pool`
    /// (and rolling back on error).
    pub fn materialize(&self, pool: Option<&mut Reservation>) -> Result<FlatColumn, ColumnError>;
    /// Gathers `indices` into a [`FlatColumn`], validating every `idx < self.len()` (including for `Column::Const`),
    /// writing canonical zero/empty backing values at null slots, and reconciling `pool` to exact `result.memory_bytes()`.
    pub fn take(&self, indices: &[u32], pool: Option<&mut Reservation>) -> Result<FlatColumn, ColumnError>;
    /// Slices `offset..offset + len` (`Const` is O(1)).
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// NaN-aware, encoding-independent semantic equality across `0..len`.
    pub fn semantic_eq(&self, other: &Self) -> bool;
    /// Deterministic logical heap bytes.
    pub fn memory_bytes(&self) -> usize;
}
```

`crates/column/src/batch.rs`:
```rust
use std::sync::Arc;

use crate::{Bitmap, Column, ColumnError, Reservation, Schema, Selection};

/// Columnar batch of rows with an attached [`Selection`] filter.
#[derive(Clone, PartialEq)]
pub struct Batch {
    schema: Arc<Schema>,
    columns: Vec<Column>,
    raw_len: usize,
    selection: Selection,
}

impl Batch {
    /// Validates `raw_len <= u32::MAX as usize`, column count == `schema.len()`, every column's
    /// `data_type()` and `len() == raw_len`, `Const` scalar validity, `selection.validate(raw_len)`,
    /// and non-nullable field constraints across active rows without allocating `to_indices` or
    /// looping `0..raw_len`:
    /// - Fast path: if `!field.nullable && col.has_nulls()`:
    ///   - `Selection::All` → reject immediately in O(1);
    ///   - `Selection::Mask(m)` → check `m.iter_ones().any(|r| col.is_null(r))`;
    ///   - `Selection::Indices(idx)` → check `idx.iter().any(|&r| col.is_null(r as usize))`.
    pub fn try_new(
        schema: Arc<Schema>,
        columns: Vec<Column>,
        raw_len: usize,
        selection: Selection,
    ) -> Result<Self, ColumnError>;
    /// Convenience constructor with `Selection::All`.
    pub fn try_new_all(
        schema: Arc<Schema>,
        columns: Vec<Column>,
        raw_len: usize,
    ) -> Result<Self, ColumnError>;
    /// Creates an empty batch (0 rows, empty `FlatColumn` per field, `Selection::All`).
    pub fn empty(schema: Arc<Schema>) -> Self;
    /// Batch schema.
    pub fn schema(&self) -> &Arc<Schema>;
    /// All columns in schema order.
    pub fn columns(&self) -> &[Column];
    /// Column at index `i`.
    pub fn column(&self, index: usize) -> Option<&Column>;
    /// Column by exact field name.
    pub fn column_by_name(&self, name: &str) -> Option<&Column>;
    /// Physical row count before applying `selection`.
    pub fn raw_len(&self) -> usize;
    /// Active row count after applying `selection`.
    pub fn num_rows(&self) -> usize;
    /// True when `num_rows() == 0`.
    pub fn is_empty(&self) -> bool;
    /// Active selection vector.
    pub fn selection(&self) -> &Selection;
    /// Replaces `selection` after validating against `raw_len` and non-null field constraints.
    pub fn with_selection(&self, selection: Selection) -> Result<Self, ColumnError>;
    /// Intersects the current selection with `mask` (length `raw_len`) without copying column buffers.
    pub fn filter_mask(&self, mask: &Bitmap) -> Result<Self, ColumnError>;
    /// Projects a subset of columns by index, keeping `selection` intact.
    pub fn project(&self, indices: &[usize]) -> Result<Self, ColumnError>;
    /// Slices physical rows `offset..offset + len`, slicing `selection` accordingly.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError>;
    /// Materializes active rows (`Selection::All`, `raw_len == num_rows()`).
    /// When `self.selection.is_all(self.raw_len)`, preserves `Const`/`Dict`/`Rle` encodings and sets `Selection::All`;
    /// otherwise gathers active rows via `Column::take`. Reserves net output `memory_bytes()` from `pool`
    /// and rolls back all reservations on error.
    pub fn compact(&self, pool: Option<&mut Reservation>) -> Result<Self, ColumnError>;
    /// Concatenates batches with identical schemas into one compacted `Selection::All` batch.
    /// Reserves exact final output `memory_bytes()` in `pool` (releasing any temporary scratch
    /// reservations before returning `Ok`, and rolling back everything on `Err`).
    pub fn concat(
        schema: Arc<Schema>,
        batches: &[&Batch],
        pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError>;
    /// NaN-aware, encoding-independent semantic equality over active rows.
    pub fn semantic_eq(&self, other: &Self) -> bool;
    /// Deterministic logical heap bytes across all columns and selection.
    pub fn memory_bytes(&self) -> usize;
}
```

**Exact `Reservation` accounting contract (applies to QE1.3 and QE1.4):**
- Every function taking `pool: Option<&mut Reservation>` records `let before = pool.as_ref().map(|p| p.size());`.
- All paths — **including empty-output fast paths** (`Column::take(&[])`, `Batch::concat(&[])`, whose empty `Utf8Data`/`List` offsets still have non-zero `memory_bytes()`) — reconcile the final reservation to exact `before + result.memory_bytes()` via `try_grow` (for positive delta, e.g. `Const(List)` whose `ScalarValue` heap exceeds 1-row `FlatData`) or `shrink` (for normalized all-ones bitmaps, temporary scratch buffers, or `Const` scalar shrinkage).
- On `Err(_)`: any bytes reserved during the call are shrunk back so `pool.size() == before`.

### Steps

1. Implement `Utf8Data` (`try_new` checking `!offsets.is_empty()`, `offsets[0] == 0`, monotonic `offsets[i] <= offsets[i + 1]`, `*offsets.last() as usize == bytes.len()`, `std::str::from_utf8(&bytes)`, AND `s.is_char_boundary(o as usize)` for every `o` in `offsets`), `BinaryData`, `MapData`, `EventsData`, `LinksData` with all public accessors, `slice`, `take` (canonical empty backing entries at null positions), `memory_bytes`, and privacy-safe `Debug`.
2. Implement `FlatData` (boxing `MapData`, `EventsData`, `LinksData`) and `FlatColumn` covering all 16 `DataType` variants, normalizing `Some(all_ones)` → `None` and `FlatData::Null(_)` → `nulls: None`, with `get_scalar`, `slice`, `take`, `semantic_eq`, `memory_bytes`, and privacy-safe `Debug`.
3. Implement `DictColumn` and `RleColumn` (enforcing `run_ends[0] > 0` when non-empty and strict monotonicity) with all public accessors, `get_scalar`, `slice`, and `memory_bytes`.
4. Implement `Column` (`try_new_const`, `materialize`, `take`, `slice`, `semantic_eq`, `memory_bytes`, privacy-safe `Debug`) with exact `Reservation` reservation + rollback on error.
5. Implement `Batch` in `crates/column/src/batch.rs` (`try_new`, `empty`, `filter_mask`, `project`, `slice`, `compact`, `concat`, `semantic_eq`, `memory_bytes`, privacy-safe `Debug`) with exact `Reservation` postconditions.
6. Export `pub mod column;`, `pub mod batch;` and re-exports from `crates/column/src/lib.rs`.

### Tests

- `column::tests::all_types_materialize_and_take_match_scalars` — all 16 `DataType`s (including `f64::NAN`, nulls, empty strings/lists/maps/events/links, nested `List(List(Utf8))`), `Flat`/`Const`/`Dict`/`Rle` through `materialize`, `take`, `slice`, verified with `semantic_eq` and `get_scalar`.
- `column::tests::utf8_non_char_boundary_offset_rejected` — valid UTF-8 `"é"` (`[0xc3, 0xa9]`) with `offsets = [0, 1, 2]` returns `Err(ColumnError::InvalidUtf8)` and never panics.
- `column::tests::invalid_invariants_rejected` — empty offsets, non-zero `offsets[0]`, non-monotonic offsets, end offset `!= bytes.len()`, out-of-range dict code, `RleColumn` `run_ends = [0, 5]`, non-increasing `run_ends`, mismatched null bitmap length, `Const` with bad `Decimal128` or mismatched `List` item.
- `column::tests::const_null_zero_len_allowed_in_non_nullable_batch`.
- `column::tests::debug_never_leaks_payloads` — asserts secret sentinel strings do not appear in `Debug` of `Utf8Data`, `BinaryData`, `MapData`, `EventsData`, `LinksData`, `Column`, or `Batch`.
- `column::tests::reservation_exact_accounting_and_rollback_on_error` — verifies `pool.size() == before + result.memory_bytes()` on `Ok` and `pool.size() == before` on `Err` for `materialize`, `take`, `compact`, and `concat`.
- `batch::tests::filter_mask_does_not_copy_and_compact_preserves_encodings_on_is_all` — verifies `filter_mask` keeps `raw_len` unchanged, `compact` preserves `Dict`/`Rle`/`Const` when `selection.is_all(raw_len)`, and drops filtered rows otherwise; tests `project`, `slice`, `concat`.
- `batch::tests::proptest_take_and_slice_match_reference` — `proptest` with bounded sizes (`0..=32` rows, `0..=6` columns, list depth `<= 3`) across `Flat`/`Const`/`Dict`/`Rle` and random `Selection`s, asserting `semantic_eq`.

### Done when

- `cargo test -p cetra-column` passes.
- `cargo clippy -p cetra-column --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check` passes.

**Commit:** `column: implement Flat/Const/Dict/Rle columns and Batch`

---

## QE1.4 — Checksummed binary batch framing (`frame.rs`)

**Goal:** Implement the versioned, CRC-32C-checksummed, LZ4-compressed, DoS-bounded binary batch frame format for WAL and spill files, with zero panics on arbitrary bytes and exact `Reservation` accounting/rollback.

**Edit:**
- `crates/column/src/frame.rs` (new).
- `crates/column/src/lib.rs`: add `pub mod frame;`.

### Contracts

Constants and public API in `crates/column/src/frame.rs`:
```rust
use std::ops::Range;

use crate::{Batch, ColumnError, MAX_NESTING_DEPTH, Reservation, Schema};

/// 4-byte magic prefix of a v1 batch frame.
pub const FRAME_MAGIC: [u8; 4] = *b"CBM1";
/// Current binary batch frame format version.
pub const FRAME_VERSION: u8 = 1;
/// Fixed header length in bytes:
/// `magic(4) + version(1) + flags(1) + reserved(2) + uncompressed_len(4) + payload_len(4) + crc32c(4)`.
pub const FRAME_HEADER_LEN: usize = 20;
/// Flag bit indicating `payload` is an LZ4 frame.
pub const FLAG_LZ4: u8 = 0x01;
/// Maximum encoded payload length (256 MiB, matching `wal/record.rs`).
pub const MAX_PAYLOAD: u32 = 256 * 1024 * 1024;
/// Maximum decompressed body length (1 GiB, matching `wal/record.rs`).
pub const MAX_DECODED_BYTES: u32 = 1 << 30;
/// Maximum allowed LZ4 expansion ratio before rejecting as a bomb (matching `wal/record.rs`).
pub const LZ4_MAX_RATIO: u64 = 256;
/// Fixed slack bytes for LZ4 frame headers in the bomb check (matching `wal/record.rs`).
pub const LZ4_SLACK: u64 = 64;
/// Maximum number of columns in one framed batch (guards hostile column counts).
pub const MAX_COLUMNS: u32 = 1024;

/// One step of streaming frame scanning.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameSlice {
    /// Complete checksum-verified frame slice inside the buffer.
    Frame {
        /// Byte range of the full frame (header + payload) in `buf`.
        frame: Range<usize>,
        /// Offset of the next frame.
        next: usize,
    },
    /// `offset == buf.len()`.
    End,
    /// Incomplete header or payload at the tail of `buf` (torn write).
    Short {
        /// Offset where the incomplete frame begins.
        offset: usize,
    },
    /// Corrupt magic, version, flags, lengths, or CRC-32C checksum.
    Invalid {
        /// Offset where the bad frame begins.
        offset: usize,
        /// Static reason (never payload bytes).
        reason: &'static str,
    },
}

/// Encodes `batch` (compacting active rows first if `!selection.is_all(raw_len)`, preserving column encodings
/// when all rows are active) and appends one frame to `out`. On error, `out` is left unchanged.
pub fn encode_batch(batch: &Batch, out: &mut Vec<u8>) -> Result<(), ColumnError>;

/// [`encode_batch`] with a custom decoded-byte cap (for testing small caps).
pub fn encode_batch_with_limit(
    batch: &Batch,
    out: &mut Vec<u8>,
    max_decoded: u32,
) -> Result<(), ColumnError>;

/// Scans the next frame starting at `offset` in `buf`, checking header invariants and CRC-32C without decompressing.
pub fn decode_next(buf: &[u8], offset: usize) -> FrameSlice;

/// Decodes one complete frame (header + payload) into a [`Batch`], reserving from `pool`.
/// Postcondition: on `Ok(batch)`, `pool.size()` increased by `batch.memory_bytes()` (temporary decompressed
/// body buffer is released before returning); on `Err`, `pool.size()` is restored to its pre-call value.
/// When `expected_schema` is `Some`, rejects frames whose decoded schema differs.
pub fn decode_batch(
    frame: &[u8],
    expected_schema: Option<&Schema>,
    pool: Option<&mut Reservation>,
) -> Result<Batch, ColumnError>;

/// [`decode_batch`] with a custom decoded-byte cap (for testing small caps).
pub fn decode_batch_with_limit(
    frame: &[u8],
    expected_schema: Option<&Schema>,
    pool: Option<&mut Reservation>,
    max_decoded: u32,
) -> Result<Batch, ColumnError>;
```

**Exact wire format (all integers little-endian):**
- **Header (20 bytes):**
  - `[0..4]`: `FRAME_MAGIC` (`b"CBM1"`)
  - `[4]`: `version` (`1`)
  - `[5]`: `flags` (`0` = uncompressed, `FLAG_LZ4` = LZ4 frame; any other bit invalid)
  - `[6..8]`: `reserved` (`0u16`; any non-zero invalid)
  - `[8..12]`: `uncompressed_len: u32` (`<= max_decoded`)
  - `[12..16]`: `payload_len: u32` (`<= MAX_PAYLOAD`; if `flags == 0`, must equal `uncompressed_len`)
  - `[16..20]`: `crc32c: u32` = `crc32c::crc32c_append(crc32c::crc32c(&header[0..16]), payload)`
- **Exact `DataType` tags (`u8`):**
  `0 = Null`, `1 = Bool`, `2 = I64`, `3 = F64`, `4 = Decimal128` (+ `precision: u8, scale: i8`), `5 = TimestampNs` (+ `utc: u8`), `6 = Date`, `7 = Interval`, `8 = Utf8`, `9 = Binary`, `10 = FixedBytes8`, `11 = FixedBytes16`, `12 = List` (+ recursive `DataType` checked `<= MAX_NESTING_DEPTH`), `13 = MapUtf8`, `14 = Events`, `15 = Links`. Any tag `> 15` is `InvalidFrame`.
- **Boolean flag bytes (`nullable`, `utc`, `has_nulls`, `is_null`):** must be `0` or `1`; any value `> 1` is `InvalidFrame`.
- **Uncompressed body:**
  - `raw_len: u32`, `col_count: u32` (`<= MAX_COLUMNS`).
  - Schema: `col_count` fields, each `name_len: u32` + UTF-8 bytes + `nullable: u8` + `DataType`.
  - Columns: `col_count` encoded columns, each `encoding_tag: u8` (`0 = Flat`, `1 = Const`, `2 = Dict`, `3 = Rle`; any `> 3` invalid):
    - **`Flat` (`0`):** encoded `FlatColumn` body (no extra tag): `has_nulls: u8` (if `1`, `len.div_ceil(8)` bitmap bytes — rejected if `DataType::Null` has `has_nulls == 1`), followed by `FlatData` body for `(data_type, len)`:
      - `Null` → 0 bytes;
      - `Bool` → `len.div_ceil(8)` bitmap bytes;
      - `I64` / `TimestampNs` → `len * 8` LE bytes;
      - `F64` → `len * 8` LE `f64::to_bits()` bytes;
      - `Decimal128` → `len * 16` LE bytes;
      - `Date` → `len * 4` LE bytes;
      - `Interval` → `len * 16` bytes (`months: i32, days: i32, nanos: i64` per item);
      - `FixedBytes8` → `len * 8` bytes; `FixedBytes16` → `len * 16` bytes;
      - `Utf8` / `Binary` → `(len + 1) * 4` LE `u32` offsets + `bytes_len: u32` + `bytes_len` raw bytes;
      - `List(inner)` → `(len + 1) * 4` LE `u32` offsets + `child_len: u32` + recursive encoded `Column` of length `child_len` and type `inner` (incrementing depth check `<= MAX_NESTING_DEPTH`);
      - `MapUtf8` → `(len + 1) * 4` LE `u32` offsets + `entry_count: u32` + `keys Utf8Data(entry_count)` + `values Utf8Data(entry_count)` + `has_value_nulls: u8` + optional bitmap;
      - `Events` → `(len + 1) * 4` LE `u32` offsets + `ev_count: u32` + `times` (`ev_count * 8`) + `names Utf8Data(ev_count)` + `attributes Utf8Data(ev_count)` + `has_attr_nulls: u8` + optional bitmap;
      - `Links` → `(len + 1) * 4` LE `u32` offsets + `lk_count: u32` + `trace_ids` (`lk_count * 16`) + `span_ids` (`lk_count * 8`) + `attributes Utf8Data(lk_count)` + `has_attr_nulls: u8` + optional bitmap.
    - **`Const` (`1`):** `is_null: u8` (`1` → `ScalarValue::Null(dt)`; `0` → reject if `dt == DataType::Null`, else encode as a 1-row `FlatData` body with no null bitmap and decode via `read_flat_data(..., 1)` + `get_scalar(0)`).
    - **`Dict` (`2`):** `dict_len: u32` + encoded `FlatColumn` body (starts directly with `has_nulls: u8`, no `encoding_tag`) of length `dict_len` + `len * 4` LE `u32` codes + `has_nulls: u8` + optional bitmap (`len.div_ceil(8)` bytes).
    - **`Rle` (`3`):** `run_count: u32` + encoded `FlatColumn` body (starts directly with `has_nulls: u8`, no `encoding_tag`) of length `run_count` + `run_count * 4` LE `u32` `run_ends`.
  - Trailing bytes after the last column are rejected as `InvalidFrame`.

**Exact `decode_next(buf, offset)` check priority order:**
1. `offset == buf.len()` → `FrameSlice::End`.
2. `offset > buf.len()` or `buf.len() - offset < FRAME_HEADER_LEN` → `FrameSlice::Short { offset }`.
3. Validate header fields in order: `magic == FRAME_MAGIC`, `version == FRAME_VERSION`, `flags & !FLAG_LZ4 == 0`, `reserved == 0`, `payload_len <= MAX_PAYLOAD`, `uncompressed_len <= MAX_DECODED_BYTES`, if `flags == 0` then `payload_len == uncompressed_len`, `FRAME_HEADER_LEN.checked_add(payload_len as usize)` does not overflow → any failure returns `FrameSlice::Invalid { offset, reason }`.
4. `buf.len() - offset < total_len` → `FrameSlice::Short { offset }`.
5. Verify CRC-32C over `&header[0..16]` + `payload` → mismatch returns `FrameSlice::Invalid { offset, reason: "checksum mismatch" }`.
6. Return `FrameSlice::Frame { frame: offset..offset + total_len, next: offset + total_len }`.

### Steps

1. Implement `write_body` and `BodyReader<'a>` in `crates/column/src/frame.rs` using checked `usize`/`u64` arithmetic for every length calculation (`(len as usize).checked_add(1)?.checked_mul(4)`, `count.checked_mul(elem_size)`, `len.div_ceil(8)`) and checking `<= self.remaining()` **before** allocating any `Vec` or reserving from `pool`.
2. Implement `encode_batch_with_limit`: reject `batch.schema().len() > MAX_COLUMNS as usize` or `batch.num_rows() > u32::MAX as usize`; compact only when `!batch.selection().is_all(batch.raw_len())`; serialize into a temporary `Vec<u8>`; reject if `body.len() > max_decoded as usize`; compress with `lz4_flex::frame::FrameEncoder` when `body.len() >= 64` (falling back to uncompressed if compressed `>= body.len()`); reject if `payload.len() > MAX_PAYLOAD as usize`; compute CRC-32C; append header + payload to `out` atomically.
3. Implement `decode_next` following the exact 6-step priority order above.
4. Implement `decode_batch_with_limit` with a scratch-reservation rollback helper:
   - verify `frame.len() >= FRAME_HEADER_LEN`, header invariants, `frame.len() == FRAME_HEADER_LEN + payload_len`, and CRC-32C;
   - if `FLAG_LZ4`: verify bomb ratio `u64::from(uncompressed_len) <= u64::from(payload_len).saturating_mul(LZ4_MAX_RATIO).saturating_add(LZ4_SLACK)`; reserve `uncompressed_len as usize` as scratch from `pool` when `pool.is_some()`, then decode via `lz4_flex::frame::FrameDecoder::new(payload).take(u64::from(uncompressed_len) + 1)` into `Vec::new()` (do **not** eagerly `Vec::with_capacity(uncompressed_len)` so invalid fuzz inputs fail without a multi-MiB allocation), rejecting if actual decoded length `!= uncompressed_len`;
   - decode schema and columns, reserving decoded column bytes from `pool`, verify zero trailing bytes and `expected_schema` match, build `Batch::try_new_all`;
   - reconcile `pool` to exact `before + batch.memory_bytes()` (shrinking the temporary decompressed body scratch, normalized all-ones bitmaps, and `Const` shrink deltas, or calling `try_grow` if `Const(List)` exceeds 1-row `FlatData`) before returning `Ok(batch)`; on any `Err`, roll back all reservations to `before`.
5. Export `pub mod frame;` from `crates/column/src/lib.rs`.

### Tests

- `frame::tests::round_trip_all_types_and_encodings` — encodes and decodes a batch with all 16 `DataType`s (including `f64::NAN`), `Flat`/`Const`/`Dict`/`Rle` encodings, nested `List(List(Utf8))`, nulls, empty batch (0 rows), and active `Selection::Mask` (compacted on encode), verified via `Batch::semantic_eq`.
- `frame::tests::multi_frame_stream_and_torn_tail` — encodes 3 frames into one buffer, scans with `decode_next`, truncates at every byte position of the 3rd frame and verifies `FrameSlice::Short`, and verifies an oversized corrupt header returns `Invalid` (not `Short`).
- `frame::tests::corrupt_header_and_crc_rejected` — bad magic, bad version, unknown flags, non-zero reserved, `flags == 0` with mismatched lengths, `DataType::Null` with `Const { is_null: 0 }`, boolean flag `> 1`, flipped bit in header, flipped bit in payload, trailing byte after body.
- `frame::tests::lz4_bomb_and_size_caps_rejected` — fabricated header with huge `uncompressed_len` fails bomb ratio before allocation; small `max_decoded` rejected on both encode and decode; mismatched actual LZ4 output length rejected.
- `frame::tests::expected_schema_mismatch_rejected`.
- `frame::tests::decode_reservation_accounting_and_rollback` — on `Ok(batch)`, `pool.size() == before + batch.memory_bytes()` (scratch body freed); on tight `Reservation`, returns `ColumnError::MemoryLimit` with `pool.size() == before`.
- `frame::tests::proptest_round_trip_and_arbitrary_bytes_never_panic` — bounded `proptest` (`0..=32` rows, `0..=6` cols, depth `<= 3`) on random batches (asserting `semantic_eq`) and arbitrary byte slices.

### Done when

- `cargo test -p cetra-column` passes.
- `cargo clippy -p cetra-column --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check` passes.

**Commit:** `column: add checksummed LZ4 binary batch framing`

---

## QE1.5 — Temporary Arrow ↔ `cetra-column` conversion bridge in `cetra-storage`

**Goal:** Build `crates/storage/src/column_bridge.rs` (converting Arrow `RecordBatch` ↔ `cetra_column::Batch` for all spec 01 schemas, SQL types, sliced Arrow arrays, and Arrow `Dictionary`/`RunArray` encodings) while keeping `cetra-column` 100% Arrow-free.

**Edit:**
- `Cargo.lock` (updated by `cargo metadata --format-version 1 > /dev/null`).
- `AGENTS.md`: update § Crate dependency graph to note `storage` depends on `core` and `column`.
- `crates/storage/Cargo.toml`: add `cetra-column.workspace = true` under `[dependencies]`.
- `crates/storage/src/column_bridge.rs` (new).
- `crates/storage/src/lib.rs`: insert `#[allow(dead_code)] // used by M2–M6 before cut-over` + `pub(crate) mod column_bridge;` alphabetically between `catalog` and `compaction`.

### Contracts

`crates/storage/src/column_bridge.rs`:
```rust
use arrow::datatypes::Schema as ArrowSchema;
use arrow::record_batch::RecordBatch;
use cetra_column::{Batch, ColumnError, Schema};

/// Converts an Arrow `Schema` to a `cetra_column::Schema`.
///
/// Widens all 8 Arrow integer widths (`Int8..UInt64`) to `I64`, `Float32` to `F64`,
/// `LargeUtf8`/`Utf8View` to `Utf8`, `LargeBinary` to `Binary`, and recognizes
/// `spans.events` and `spans.links` `List<Struct>` shapes using
/// `cetra_core::schema::{event_field, link_field}` constants as `Events` / `Links`.
pub(crate) fn arrow_schema_to_column(schema: &ArrowSchema) -> Result<Schema, ColumnError>;

/// Converts a `cetra_column::Schema` to an Arrow `Schema` using default Arrow types:
/// - `TimestampNs { utc: true }` → `Timestamp(Nanosecond, Some("UTC".into()))`
/// - `TimestampNs { utc: false }` → `Timestamp(Nanosecond, None)`
/// - `List(inner)` → `List(Field::new("item", inner, true))`
/// - `MapUtf8` → `Map(Struct(keys: Utf8 non-null, values: Utf8 nullable), false)`
/// - `Events` / `Links` → matching `spans` `List<Struct>` layout using `event_field`/`link_field`.
pub(crate) fn column_schema_to_arrow(schema: &Schema) -> ArrowSchema;

/// Converts an Arrow `RecordBatch` into a `cetra_column::Batch` (`Selection::All`),
/// preserving Arrow `DictionaryArray` (all 8 integer key types) as `Column::Dict` and
/// Arrow `RunArray` (all 3 run-end types `Int16`/`Int32`/`Int64`) as `Column::Rle`.
pub(crate) fn record_batch_to_column(batch: &RecordBatch) -> Result<Batch, ColumnError>;

/// Converts a `cetra_column::Batch` (compacting active rows first) into an Arrow `RecordBatch`.
///
/// Encoding rules:
/// - When `target_schema` is `None`: uses `column_schema_to_arrow(batch.schema())` and
///   materializes every `Column` to flat Arrow arrays so the batch schema always matches.
/// - When `target_schema` is `Some(target)`: preserves exact target field names, timezones,
///   list/map nested field names and nullability; narrows `I64` to any of the 8 Arrow integer
///   types with checked `try_from` (returning `ColumnError::Overflow` on range failure);
///   emits Arrow `DictionaryArray` / `RunArray` when the target field's Arrow type is
///   `Dictionary` / `RunEndEncoded` (wrapping compacted `Flat` or `Const` into a trivial
///   1-to-1 `DictionaryArray` or 1-run/1-row-per-run `RunArray` as needed), and materializes
///   to flat Arrow arrays otherwise.
/// - Handles 0-column non-empty batches via `RecordBatch::try_new_with_options`.
pub(crate) fn column_to_record_batch(
    batch: &Batch,
    target_schema: Option<&ArrowSchema>,
) -> Result<RecordBatch, ColumnError>;
```

### Steps

1. Add `cetra-column.workspace = true` to `crates/storage/Cargo.toml` and update `Cargo.lock` and `AGENTS.md` § Crate dependency graph.
2. Implement `arrow_schema_to_column` handling every Arrow type in `cetra-core` and DataFusion SQL:
   - `Null` → `Null`; `Boolean` → `Bool`; `Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64` → `I64`; `Float32 | Float64` → `F64`;
   - `Decimal128(p, s)` → `Decimal128 { precision: *p, scale: *s }`;
   - `Timestamp(Nanosecond, tz)` → `TimestampNs { utc: tz.is_some() }`;
   - `Date32` → `Date`; `Interval(MonthDayNano)` → `Interval`;
   - `Utf8 | LargeUtf8 | Utf8View` → `Utf8`; `Binary | LargeBinary` → `Binary`;
   - `FixedSizeBinary(8)` → `FixedBytes8`; `FixedSizeBinary(16)` → `FixedBytes16`;
   - `Dictionary(_, value_type)` → convert `value_type`; `RunEndEncoded(_, value_field)` → convert `value_field.data_type()`;
   - `Map(entries, _)` (validating `entries` is `Struct` with 2 `Utf8` fields) → `MapUtf8`;
   - `List(item)` / `LargeList(item)`: match `Events` (`Struct` with `event_field::{TIME, NAME, ATTRIBUTES}`) and `Links` (`Struct` with `link_field::{TRACE_ID, SPAN_ID, ATTRIBUTES}`), else recursive `List(Box::new(inner))`;
   - Reject unsupported Arrow types with `ColumnError::InvalidBatch`.
3. Implement `record_batch_to_column` handling all slicing & encoding edge cases cleanly:
   - **Null bitmaps:** extract via `array.is_valid(i)` (or `NullBuffer` offset-aware slicing) so sliced Arrow arrays with non-zero bit offsets convert cleanly; write canonical zero/empty backing entries at null slots.
   - **Offset rebasing & null slots on sliced arrays:** for `StringArray`, `LargeStringArray`, `BinaryArray`, `LargeBinaryArray`, `ListArray`, `LargeListArray`: when `null_count() == 0`, rebase offsets by subtracting `offsets[0]` and slicing the child buffer/array (checking `u32::try_from` for `Large*`); when `null_count() > 0`, build row-by-row skipping null slots and emitting canonical empty ranges so non-empty backing bytes behind null Arrow slots are never copied. For `MapArray`, `Events` and `Links`, always build row-by-row skipping null parent rows (emitting empty ranges at null parent rows) so garbage entries behind null parent rows are never inspected or falsely rejected.
   - **Map & struct null validation:** on non-null parent rows, reject null map keys and null `Events`/`Links` struct items or non-null struct fields (`time`, `name`, `trace_id`, `span_id`) with `ColumnError::InvalidBatch`.
   - **`DictionaryArray`:** dispatch all 8 key types (`Int8..UInt64`), rejecting negative or `> u32::MAX` non-null keys with `ColumnError::Overflow` / `InvalidBatch`, and preserving `Column::Dict`.
   - **`RunArray`:** guard `if run.is_empty()` first and return an empty `RleColumn` (avoiding `arrow-array` slice underflow on empty/sliced-empty `RunArray`); otherwise dispatch all 3 run-end types (`Int16`/`Int32`/`Int64`), handling sliced `RunArray` via `get_start_physical_index()` / `get_end_physical_index()`, clamping each run end to `offset + len`, subtracting `offset()`, rejecting `<= 0` or `> u32::MAX`, and preserving `Column::Rle`.
   - **`UInt64Array`:** checked `i64::try_from` per valid row, returning `ColumnError::Overflow` if `> i64::MAX`.
4. Implement `column_schema_to_arrow` and `column_to_record_batch` following the exact `target_schema` encoding rules above (using `MapArray::try_new` with exact target `entries` field and `RecordBatch::try_new_with_options` for 0-column batches).
5. Insert `pub(crate) mod column_bridge;` alphabetically in `crates/storage/src/lib.rs` between `catalog` and `compaction`.

### Tests

- `column_bridge::tests::spans_traces_scores_round_trip_exact` — converts real fixture `Span`, trace partial, and `Score` `RecordBatch`es (`spans_schema()`, `traces_schema()`, `scores_schema()`), checks exact Arrow `RecordBatch` equality after `record_batch_to_column` → `column_to_record_batch(..., Some(schema))`, including non-empty `events`, `links`, `tags`, `schema_version`, and `partial_count`.
- `column_bridge::tests::map_with_null_values_and_rejected_null_key` — explicitly constructs an Arrow `MapArray` containing a null value (verifying lossless round-trip) and one with a null key (verifying `InvalidBatch`).
- `column_bridge::tests::sql_types_and_encodings_round_trip` — `Null`, `Decimal128(38, 18)`, `TimestampNs { utc: true/false }`, `Date32`, `Interval(MonthDayNano)`, `Binary`, `List(I64)`, `List(List(Utf8))`, `DictionaryArray` (multiple key widths) → `Column::Dict`, `RunArray` (`Int16`/`Int32`/`Int64`) → `Column::Rle`, and 0-column non-empty batch.
- `column_bridge::tests::sliced_arrow_arrays_preserve_null_and_value_offsets` — tests `batch.slice(3, 5)` on `StringArray`, `ListArray`, `MapArray`, `Events`, `Links`, `DictionaryArray`, and `RunArray` with nulls.
- `column_bridge::tests::overflow_on_u64_above_i64_max_and_bad_integer_narrowing`.

### Done when

- `cargo test -p cetra-storage column_bridge::` passes.
- `cargo clippy -p cetra-storage --lib --tests -- -D warnings` passes.
- `cargo fmt --all -- --check` passes.

**Commit:** `storage: add temporary Arrow to cetra-column conversion bridge`

---

## QE1.6 — `column_frame` fuzz target and seed corpus

**Goal:** Add the `column_frame` fuzz target, stable corpus replay test, seed corpus generator, and nightly CI entry matching `crates/otel/src/fuzz_entry.rs`, `fuzz/Cargo.toml`, and `justfile`.

**Edit:**
- `crates/column/src/fuzz_entry.rs` (new).
- `crates/column/src/lib.rs`: add `#[cfg(any(test, fuzzing))] #[doc(hidden)] pub mod fuzz_entry;`.
- `fuzz/Cargo.toml`: add `cetra-column = { path = "../crates/column" }` and `[[bin]] name = "column_frame"`.
- `fuzz/fuzz_targets/column_frame.rs` (new).
- `fuzz/corpus/column_frame/seed-*` (generated via `write_seed_corpus`).
- `justfile`: add `column_frame` to `fuzz_targets`.
- `.github/workflows/nightly.yml`: add `column_frame` to the `fuzz` job matrix.

### Contracts

`crates/column/src/fuzz_entry.rs`:
```rust
//! Fuzz entry point for `cetra-column` binary framing (D-QE-7). Must never panic.
#![allow(clippy::expect_used, clippy::unwrap_used)] // harness setup, not input-dependent

/// Decodes arbitrary bytes as a single frame and as a multi-frame stream.
pub fn column_frame(data: &[u8]) {
    let _ = crate::frame::decode_batch(data, None, None);
    let mut offset = 0;
    while let crate::frame::FrameSlice::Frame { frame, next } =
        crate::frame::decode_next(data, offset)
    {
        let _ = crate::frame::decode_batch(&data[frame], None, None);
        if next <= offset {
            break;
        }
        offset = next;
    }
}
```

`fuzz/fuzz_targets/column_frame.rs`:
```rust
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    cetra_column::fuzz_entry::column_frame(data);
});
```

### Steps

1. Create `crates/column/src/fuzz_entry.rs` with `pub fn column_frame(data: &[u8])` and `#[cfg(test)] pub(crate) mod tests` containing:
   - `sorted_files` helper copied in style from `crates/otel/src/fuzz_entry.rs:84-109` (treating a missing `fuzz/regressions/column_frame/` directory as empty);
   - `replay_column_frame_corpus` (replays every file in `fuzz/corpus/column_frame/` and `fuzz/regressions/column_frame/`, failing if the corpus directory is empty);
   - `valid_seeds_decode` (verifies every `seed-*` file in `fuzz/corpus/column_frame/` decodes cleanly with `frame::decode_batch` or multi-frame `decode_next`);
   - `#[ignore] fn write_seed_corpus()` (writes `seed-empty`, `seed-all-types`, `seed-encodings`, `seed-nested-list`, `seed-uncompressed`, `seed-multi-frame` into `fuzz/corpus/column_frame/` — hooks automatically into `just fuzz-seed`).
2. Add `/// Fuzz entry points (D-QE-7). Not part of the API.` above `#[cfg(any(test, fuzzing))] #[doc(hidden)] pub mod fuzz_entry;` in `crates/column/src/lib.rs`.
3. Update `fuzz/Cargo.toml` to depend on `cetra-column = { path = "../crates/column" }` and declare `[[bin]] name = "column_frame"`.
4. Create `fuzz/fuzz_targets/column_frame.rs`.
5. Add `column_frame` to `fuzz_targets` in `justfile` (line 166) and to `jobs.fuzz.strategy.matrix.target` in `.github/workflows/nightly.yml`.
6. Run `cargo test -p cetra-column -- --ignored --exact fuzz_entry::tests::write_seed_corpus` to populate `fuzz/corpus/column_frame/`.

### Tests

- `fuzz_entry::tests::replay_column_frame_corpus` — replays every committed seed and regression file on stable Rust.
- `fuzz_entry::tests::valid_seeds_decode` — confirms committed seeds are valid frames that decode without error.

### Done when

- `cargo test -p cetra-column` passes (including `replay_column_frame_corpus` and `valid_seeds_decode`).
- `cargo clippy -p cetra-column --all-targets -- -D warnings` passes.
- `cargo fmt --all -- --check && cargo fmt --manifest-path fuzz/Cargo.toml -- --check` passes.
- `just fuzz-lock` succeeds.
- `just deny-policy` passes.
- `just check-workflows` passes.

**Commit:** `column: add column_frame fuzz target and seed corpus`

---

## QE1.7 — Milestone 1 closeout (coordinating session)

**Goal:** Merge QE1.1–QE1.6 onto `qe-query-engine`, run the Neon code review, record track `QE` in `docs/roadmap.md` and `D-QE-1`–`D-QE-15` in `docs/mvp-decisions.md`, and verify the full workspace passes `just check`.

**Edit:**
- `docs/roadmap.md`: add track **QE** row to the `## Wave 2` table.
- `docs/mvp-decisions.md`: add `## Track QE — Custom query engine and columnar format` with `D-QE-1`–`D-QE-15`.
- `docs/plans/query-engine/design.md`: update `Status:` line to reflect M1 completion.
- Any fixes from the Neon code review.

### Steps

1. Merge `QE1.1` → `QE1.2` → `QE1.3` → `QE1.4`/`QE1.5` → `QE1.6` onto `qe-query-engine`, checking each diff, commit, and tests before starting dependent phases.
2. Run a Neon (`Model: "inherit"`) code review across all M1 commits (`crates/column/`, `crates/storage/src/column_bridge.rs`, `fuzz/`). Fix any findings via Hydrogen (`flash_lite`) and re-check.
3. Add track `QE` to `docs/roadmap.md` (`## Wave 2` table) and `D-QE-1`–`D-QE-15` to `docs/mvp-decisions.md`.
4. Run full verification: `just check`, `just check-workflows`, `just assert-clean`, and `cargo tree -p cetra-column`.

### Done when

- `just check` passes.
- `just check-workflows` passes.
- `just assert-clean` passes.
- `cargo tree -p cetra-column` shows only `crc32c`, `lz4_flex`, `thiserror` (and their transitive deps) — zero `tokio`, `arrow`, `parquet`, or `datafusion`.

**Commit:** `docs: record track QE roadmap row and decisions for M1`
