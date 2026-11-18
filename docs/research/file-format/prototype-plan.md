# Prototype plan: Strata v0

Status: **draft for review**. Nothing is implemented yet.

This plan builds a prototype of the two-layer format from [README.md](README.md) § Proposed design and measures it end to end. DataFusion stays the query engine. Two hand-written fast paths (trace detail and trace list) run next to it, so engine overhead is measured as well. The prototype exists to make one decision: build the real format, change the design, or stop.

**Read first:**
- [README.md](README.md): the measurements and the design;
- [parquet.md](parquet.md) and [vortex.md](vortex.md): what to keep and what to avoid;
- [other-formats-a.md](other-formats-a.md) and [other-formats-b.md](other-formats-b.md): ideas from other formats. This plan adopts some of them, cited by number (a#3 is idea 3 in `other-formats-a.md`).

**Design versus implementation plans.** This file is the design. § Phases below is the milestone outline only. Each milestone gets a detailed implementation plan in [`plans/`](plans/), written just before the milestone starts, from the code that exists then, in the format of `docs/plans/a1-storage/01-m1-walking-skeleton.md`. Each sub-phase there has an **Edit** list, numbered steps with exact types, signatures and formats, a **Tests** list, **Done when** and a **Commit** message.

| Milestone | Phases here | Plan |
|---|---|---|
| M0 | 0 | the phase 0 brief (moving the test-data crate) |
| M1 | 1, 2a, 2b, 2c | [plans/01-m1-container-narrow.md](plans/01-m1-container-narrow.md) |
| M2 | 3a, 3b, 3c | written after M1 |
| M3 | 4 | written after M2 |
| M4 | 5a, 5b | written after M3 |
| M5 | 6, 7 | written after M4 |
| M6 | 8 | written after M5 |
| M7 | 9a, 9b, 9c | written after M6 |

**Who works on it.**
- Phases 0–9b are implemented by Haiku, one commit each, on the research branch.
- A Sonnet review runs after phases 3c, 6 and 9b.
- Phase 9c (benchmark runs on `ubuntu-vm` and the report) is run by the orchestrating session, not by Haiku.

**Revision 3.** Revision 2 applied the first Sonnet cold review (10 blockers, 13 majors, 13 minors). Revision 3 applies the second (4 blockers, 8 majors, 14 minors). Per the review convention, there is no third round.

## Where the code lives

| Crate | Path | Content |
|---|---|---|
| `fmt-testdata` | `docs/research/file-format/testdata/` | Library. `datagen.rs` moved from `bench` unchanged, plus the SWE-agent conversion (`swe.rs`, moved from `bench/src/main.rs::swe_cmd`, returning `Vec<Span>` instead of printing). Depends on `cetra-core` by path. |
| `strata` | `docs/research/file-format/strata/` | Library: format, writer, reader, DataFusion provider, fast paths, merge. `fmt-testdata` is a **dev-dependency** only. |
| `fmtbench` | `docs/research/file-format/bench/` | Existing binary. Depends on `fmt-testdata` and `strata`. |

Tests that need realistic data live in `strata/tests/` and use `fmt-testdata`. Unit tests inside `strata/src` build their own Arrow batches.

All three crates have their own `[workspace]` table, so they stay outside the cetra workspace and outside `just check` and CI. Versions are pinned to the workspace family: datafusion 55.2, arrow and parquet 59.2, object_store 0.13. Any new dependency's version is looked up (`cargo search` / `cargo info`), never guessed, and listed in the phase's commit message.

The repository rules still apply here:
- `#![forbid(unsafe_code)]`;
- `thiserror` error enums in `strata`;
- no `unwrap` or `expect` on data read from disk (corrupt input is an error);
- tests for every behaviour.

The prototype does not need the production extras: no versioning beyond `version = 0` and no crash-safe publishing.

## Profile: what the format can and cannot change

The profile was taken before planning (`fmtbench profile`). It reports the minimum of 30 runs, with process CPU time beside it. The machine was loaded (load average 40–55 on 32 threads) while it ran. Times below are milliseconds per query, warm.

**`pq_cetra`, `target_partitions = 1`:**

| Query | Total | SQL + logical plan | Physical plan | Execute | Bytes scanned |
|---|---|---|---|---|---|
| Trace lookup, all columns | 14.7 | 0.74 | 1.51 | 12.4 | 2.6 MB |
| Trace lookup, 9 light columns | 3.3 | 0.14 | 1.04 | 2.1 | 0.56 MB |
| Trace list (top 50 in 1 h) | 22.7 | 0.32 | 1.72 | 20.5 | 4.7 MB |
| `SELECT 1` | 0.11 | 0.03 | 0.06 | 0.01 | — |

**`pq_tuned`:**

| Query | Total | SQL + logical plan | Physical plan | Execute |
|---|---|---|---|---|
| Trace lookup, all columns | 10.7 | 0.75 | 1.52 | 8.3 |
| Trace lookup, 9 light columns | 4.9 | 0.22 | 1.23 | 3.4 |
| Trace list | 30.2 | 0.36 | 1.84 | 27.9 |

With `target_partitions = 32`, wall time is about the same, but CPU time is 3–5× higher: file opening costs 18–25 ms of CPU, summed across threads, per lookup.

What the profile shows:
1. **DataFusion's fixed cost is about 1.3–2.3 ms per query**, physical planning being most of it. That is 10–60% of a lookup and under 10% of a trace list. A fast path saves at most this much, plus the operator overhead inside "execute".
2. **Most of the time is in execution**, where the format decides: decoding whole pages, decompressing 1 MiB pages for a few rows, page-index and bloom handling, building Arrow arrays. Execution is the main target.
3. **Planning with 32 partitions wastes CPU on point lookups.** The provider must plan a single-file, single-partition scan when the key index has narrowed the lookup to one file.
4. The "direct arrow-rs" floor from that run is **not valid**, and is not used. The synchronous reader fetched whole column chunks (638 ms for an all-columns lookup).

**`pq_default` and `vx_compact`**, measured in a second run (load average 61 at the start, 28 at the end, on 32 threads). Each cell is total ms / execute ms.

| Query | `pq_default`, 1 partition | `pq_default`, 32 | `vx_compact`, 1 partition | `vx_compact`, 32 |
|---|---|---|---|---|
| Trace lookup, all columns | 24.6 / 22.1 | 25.5 / 22.0 | 52.6 / 50.3 | 34.2 / 31.6 (CPU 125 ms) |
| Trace lookup, 9 light columns | 10.2 / 6.9 | 6.7 / 3.9 | 7.3 / 6.1 | 4.9 / 3.2 |
| Trace list | 40.7 / 36.5 | 27.0 / 23.8 | **6.1 / 4.4** | 8.0 / 5.8 |

What these add:
- **Vortex makes the trace list 4–5× faster than Parquet.** Its zone maps on `start_time` and its kernels on compressed data make top-k cheap.
- **Vortex is the slowest on a full lookup.** Execution takes 50 ms, because whole segments of the large strings are decoded.
- **The two strengths combine in one format.** Vortex-style narrow encodings serve the trace list; small payload frames serve the lookup.
- **DataFusion's planning cost is the same for every format** (1.1–2.3 ms), so a fast path saves the same couple of milliseconds whatever the format.

## Scope

**In scope:**
- a writer and a reader for one file;
- the narrow layer, the payload layer, indexes and shredded maps;
- opening a file by suffix read, on local disk or any `object_store`;
- a DataFusion `TableProvider`;
- two fast paths;
- merging N files;
- benchmarks and a report.

**Out of scope:**
- WAL, memtable and catalog;
- schema evolution beyond "missing column → nulls";
- encryption, Parquet export, production versioning;
- the cross-file chunk store: other-formats-a § Ranked ideas, "Flag for the README", shows its index does not scale at 2 KiB chunks;
- bit-sliced trigram signatures (a#1);
- the full-zip layout for 256 B–8 KiB values (b#7);
- ALP and SIMD bit-packing (v1 candidates, see § Risks).

## Conventions (all structures)

- Every integer is little-endian.
- A **varint** is unsigned LEB128.
- A **zigzag varint** of `i64 d` is the varint of `(d << 1) ^ (d >> 63)`.
- `checksum32(bytes)` is the low 32 bits of `xxhash_rust::xxh3::xxh3_64(bytes)`.
- Every offset stored in the file is an **absolute byte offset from the start of the file**, unless stated otherwise.
- Any length or offset read from a file is bounds-checked. A violation returns `StrataError::Corrupt { what, offset }` and never panics.
- Limits: a file holds fewer than 2³² rows, and a merge has at most 255 inputs. Exceeding either is an error.
- **Limits (decoder bounds).** These are format rules, and a file that breaks one is corrupt; the reader checks each one before it allocates. `MAX_SEGMENT_RAW` (256 MiB) bounds the raw body of a segment. `MAX_CHUNK_ROWS` (65,535) bounds the rows of a narrow chunk. `MAX_CHUNK_ELEMENTS` (2²⁰) bounds the list or map elements of one chunk. `MAX_CHUNK_OUTPUT` (256 MiB) bounds the value bytes a chunk may decode to. `MAX_PAGE_ENTRIES` (4,096) bounds the block-table entries of one page. `MAX_DICT_VALUES` (2²⁰) bounds the values of one dictionary. `MAX_PROMOTED_KEYS` (64) bounds the promoted keys of one shredded map. `MAX_ROWS` (2³² − 1) bounds the rows of a file. A refs page holds exactly 1,024 rows (4,096 in v0; smaller pages cut the refs a lookup reads by four), the last one fewer, so a column has one refs page per started block of 1,024 rows. `MAX_PAGE_IDS` (2²⁴) bounds the chunk ids of one refs page after tail sharing is expanded. The writer keeps every chunk and page within them.

**Limits of the reader (scan, M4F).** These bound the work and memory one request may cause; they are not format rules.
- `MAX_PREDICATE_DEPTH` (64) and `MAX_PREDICATE_NODES` (4,096) bound a predicate: each `In` literal counts as one node. A larger predicate fails with `Unsupported` before any recursion.
- `MAX_READ_IDS` (2²⁸) bounds the chunk ids one read of a payload column returns, summed over its rows. Above it the read fails with `Corrupt`; the writer refuses a column with more chunk references.
- `MAX_CONCURRENT_GETS` (64) bounds the ranged GETs of one wave that are in flight at once. Counters and waves do not change.
- With a predicate, `limit` does not bound the bytes fetched: the filter wave still fetches every surviving predicate chunk. `limit` bounds the evaluation (per window of 65,536 rows) and the output.

## Design decisions for v0

### S1. File layout

```
0          "STRA" · u16 version = 0 · u16 flags = 0                 (8 B)
8          segment*
hot_offset hot tail
end − 64   postscript (64 B)
```

**Segment** (a#9). A 16-byte header, then the stored bytes:

```
u16 magic = 0x5347 · u8 kind · u8 codec · u32 raw_len · u32 stored_len · u32 checksum32(header bytes 0..12 ‖ stored bytes)
bytes[stored_len]
```

- Every reference to a segment stores its offset (the header's offset) and its length (`16 + stored_len`), so the reader fetches the header and the body in one range.
- `codec`: 0 = raw, 1 = zstd. Level `config.zstd_level` (default 3) for narrow, frame and table segments; level 9 for payload frames in v1 (3 in v0 and v2; see § S4 in v2).
- The writer stores a segment raw when zstd does not make it smaller (a#12). A narrow chunk, which every scan decompresses, is stored raw unless zstd saves at least 30% (15% in v1), to spare scans the decompression of chunks that zstd barely shrinks. With 2,048-row chunks, it grew `light`'s files from 168 to 177 MiB.

| `kind` | Content |
|---|---|
| 1 | narrow chunk |
| 2 | payload frame |
| 3 | payload refs page |
| 4 | payload header (frame table, refs page table) |
| 5 | payload filters (rollup + per-frame trigram filters) |
| 6 | payload edge group (chunk tails, 1,024 chunks per segment) |
| 7 | column block-table page (≤ 64 entries, raw) |
| 8 | dictionary |
| 9 | file key filter (binary fuse) |

**Hot tail.** Seven sections in this order, each prefixed by a `u32` length. A section may be empty (length 0); every section exists from phase 1 on and is empty until its phase fills it.

1. Arrow schema: an Arrow IPC **stream** holding only the schema message and the end-of-stream marker (`arrow::ipc::writer::StreamWriter::try_new(&mut buf, &schema)?` then `finish()`, with no batch). It is read back with `arrow::ipc::reader::StreamReader::try_new(bytes, None)?.schema()`.
2. Column directory (S2).
3. Sort-key block table: a copy of the block table of the first sort-key column (S3, 56 B entries).
4. Sort-key filter: the binary fuse filter over the first sort-key column (S5).
5. Shredded map keys (S6).
6. Key filter index: `u32 n` · `n × (u32 column_id · u64 offset · u32 len)`, pointing to the kind 9 segments of the other filtered columns (S5).
7. Block-table page index. For each narrow column, the list of its block-table pages, each entry `u32 first_row · u64 offset · u32 len` (16 B). The directory record points into this section.

Only these sections go into the hot tail. Block-table pages, dictionaries, filters and payload headers are separate segments, fetched on demand and cached by `StrataFile`. The hot tail size is about:

`n_columns × 64 B` (directory) `+ n_sort_key_chunks × 56 B` (section 3) `+ 1.13 B × distinct sort keys` (section 4) `+ n_columns × ceil(n_chunks / 64) × 16 B` (section 7).

The writer reports it in `FileSummary::hot_len`.

**Arrow IPC decoding and panics.** arrow-ipc 59.3.2 can panic on a structurally invalid flatbuffer (for example `convert.rs:198`, `fields().unwrap()`, and the `type_as_*().unwrap()` calls). Strata therefore decodes Arrow IPC bytes (hot-tail section 1, `IPC` chunks) **only after** their checksum has been verified.
- Random corruption cannot reach those panics; only a deliberately forged file with a correct checksum can.
- That residual risk is accepted for the prototype and noted as a v1 item. A production format would carry its own schema encoding.

**Postscript** (64 B):

```
u64 hot_offset · u32 hot_len · u32 checksum32(hot tail) · u64 row_count · u32 n_columns (directory records)
u16 version (= header version) · u16 flags (= header flags) · u32 sort_key_column · [u8; 24] zero · "STRA"
```

Field sizes: 8 + 4 + 4 + 8 + 4 + 2 + 2 + 4 + 24 + 4 = 64. `sort_key_column` is the index of the first sort-key column, or `u32::MAX` for none. The reader checks:
- `hot_offset + hot_len + 64 == file length`;
- the version and flags equal the header's.

**Open.**
- `StrataFile::open(store, path, hot_len_hint)` issues one `store.get_opts(path, GetOptions { range: Some(GetRange::Suffix(n)), .. })`, where `n: u64 = hint + 64`, or 64 KiB with no hint (256 KiB in v0; the hot tails measured 12–110 KiB). All object-store offsets are `u64` (`get_range` takes `Range<u64>` in object_store 0.13.2).
- `GetRange::Suffix` saturates at the object length, and the `GetResult` reports the object size, so no HEAD request is needed.
- If the hot tail is longer than what was read, one more ranged read fetches the rest.

### S2. Column directory

One 64-byte record per column. Top-level schema columns come first, in schema order (`column_id` = schema index). Hidden shredded-map children follow, with ids from `n_top_level` up.

Every reference carries both offset and length, so W1 (S7) can fetch it without first reading a segment header.

```
u32 column_id · u32 parent (u32::MAX = none; for a layer-3 child, the map's column_id)
u8 layer · u8 flags · u16 reserved
u32 pages_offset (byte offset of this column's page list within hot-tail section 7) · u32 n_pages · u32 n_chunks
u64 dict_offset (kind 8 segment; 0 = none) · u32 dict_len
u64 payload_offset (kind 4 segment; 0 unless layer = 1) · u32 payload_len
u64 null_count · u64 reserved2 (zero)
```

Field sizes: 4 + 4 + 1 + 1 + 2 + 4 + 4 + 4 + 8 + 4 + 8 + 4 + 8 + 8 = **64**.

| `layer` | Meaning |
|---|---|
| 0 | narrow column |
| 1 | payload column |
| 2 | shredded map parent, no data of its own |
| 3 | shredded map child (narrow rules) |
| 4 | residual map (narrow rules, `MAP` encoding) |

`flags`: bit 0 = sorted dictionary present.

### S3. Narrow layer

**Precedence.** A column is a payload column if S4 says so. Otherwise it is narrow.

**Chunks.**
- Each column is cut into chunks independently.
- A chunk is closed when it holds 2,048 rows (4,096 before v2), or when the bytes of its values reach 32 KiB, whichever comes first. Value bytes are the fixed width × rows, or for variable-width values the encoded width of the row: a Utf8 row with a dictionary is 4 B (its code), a Utf8 or Binary row without one is its length + 4 B, a List(Utf8) row with a dictionary is 4 B per element + 4 B, and a `MAP` row is, per entry, key length + value length + 8 B, plus 4 B. For `IPC` columns, the slice's `to_data().get_slice_memory_size()` is used, measured per 256-row step.
- The chunk is the unit of I/O and decoding: a mini-block in the Lance sense (b#1).

**Block table.** One 56-byte entry per chunk, stored raw in kind 7 pages of at most 64 entries (3.6 KiB). A lookup fetches only the page holding its row range, found through hot-tail section 7. The sort-key column's entries are also copied whole into hot-tail section 3.

```
u32 first_row · u32 n_rows · u64 segment_offset (0 = no segment) · u32 segment_len
u8 encoding · u8 flags · u16 const_len · [u8; 16] min · [u8; 16] max
```

Field sizes: 4 + 4 + 8 + 4 + 1 + 1 + 2 + 16 + 16 = 56. `const_len` is the byte length of an inline `CONST` value, and 0 otherwise.

`flags`:

| Bit | Meaning |
|---|---|
| 0 | has nulls |
| 1 | zone values valid |
| 2 | has NaN |
| 3 | all null |

**Zone values:**
- Integers (Int8–Int64, UInt8–UInt32) and timestamps: i64 in `min[0..8]` and `max[0..8]`. UInt64 has no zone.
- Floats: f64 over non-NaN values, with −0.0 stored as 0.0. NaN sets flag bit 2.
- Dictionary columns: the minimum and maximum code, as u32.
- FixedSizeBinary(w), w ≤ 16: the minimum and maximum values, zero-padded. They are exact, so comparisons are inclusive.
- Other types: none (flag bit 1 clear).

**Encodings** (the `encoding` byte):

| Code | Name | Used for | Body (after the validity bitmap, if `has nulls`) |
|---|---|---|---|
| 0 | `CONST` | any chunk whose rows are all non-null and **bitwise** equal (floats compared by bits, so NaN and −0.0 are exact), with a value of ≤ 16 B or a dictionary code (b#3) | No segment. The value sits inline in `min`, and its byte length in `const_len`, so `"a"` and `"a\0"` differ. |
| 1 | `CONST_NULL` | all rows null | No segment. Flag bit 3 is set. |
| 2 | `FOR_BITPACK` | Int8–Int64, UInt8–UInt32, Timestamp, Date | `u8 width`, then values packed LSB-first into u64 words. See below. |
| 3 | `PLAIN` | Float64, Float32, UInt64 | Raw values. |
| 4 | `FSB_RAW` | FixedSizeBinary | Values back to back. |
| 5 | `FSB_RLE` | FixedSizeBinary when runs < n_rows / 2 | `u16 n_runs` · `u16 run_len[n_runs]` · `n_runs` values. |
| 6 | `DICT` | Utf8 with a sorted file dictionary | `u8 width`, then the codes packed like `FOR_BITPACK` with an explicit base of 0 (the zone's min code is not used as the base). |
| 7 | `VARBIN` | other Utf8 and Binary | lengths block of the `n` value lengths, then the bytes (see below). |
| 8 | `LIST_DICT` | List(Utf8) whose values use a dictionary and with no null element inside any list (a chunk with one uses `IPC`) | lengths block of the `n` list lengths, then `u8 width` and the packed codes of all elements. |
| 9 | `BOOL` | Boolean | Arrow bitmap, `ceil(n/8)` bytes. |
| 10 | `IPC` | every other type (events, links, maps other than Map<Utf8, Utf8>) | Arrow IPC stream (`StreamWriter`) of a one-column batch holding the chunk. |
| 11 | `MAP` | `Map<Utf8, Utf8>` with the shape below | lengths block of the entry count of each map (null maps 0) · lengths block of the key lengths · key bytes · `u8 value_nulls` (0, or 1 and a value-validity bitmap of E bits) · lengths block of the value lengths · value bytes. `E` is the number of entries. |

**Lengths block.** `u32 base · u8 width · packed (len − base)`, where `base` is the minimum length (0 when there is a null, since null slots have length 0) and `width` is the bit width of the largest `len − base`. `width` above 32 is corrupt. The packed values use the `FOR_BITPACK` bit layout. Storing lengths instead of cumulative offsets keeps the bodies small and repetitive, so zstd compresses them well. **Null rule:** in `VARBIN`, `LIST_DICT` and `MAP`, a null slot has length 0, and a decoder rejects any other length there as corrupt.

**`MAP` shape.** A column uses `MAP` when `is_utf8_map` holds: the type is `Map(entries, _)` with a non-nullable `entries` field, whose struct has exactly two fields, a non-nullable Utf8 `keys` field and a nullable Utf8 `values` field. Other maps stay `IPC`.

**`FOR_BITPACK` rules:**
- Each value becomes `u = (v as i64 as u64).wrapping_sub(min as u64)`.
- `width = 64 − (max u).leading_zeros()`; width 0 is allowed and stores no words. Otherwise the body holds `ceil(n × width / 64)` u64 words, with the last word zero-padded.
- The decoder adds `min` back with `wrapping_add`.
- Null slots hold `min`. A chunk whose non-null values are all equal but which has nulls uses `FOR_BITPACK` with width 0.

**Validity bitmap.** When flag bit 0 is set, the segment body starts with the Arrow-layout validity bitmap, `ceil(n/8)` bytes. Null slots are dense: they hold a filler value (`min`, zero, or an empty string).

**Dictionaries** (kind 8 segment):
- Layout: `u32 n` · `u32 offsets[n+1]` · bytes, values sorted bytewise (b#4, a#11). A code is the value's rank.
- Built when a Utf8 column (or a List(Utf8)'s values) has ≤ 65,535 distinct values **and** the dictionary is ≤ 1 MiB. Otherwise `VARBIN`. (v0 used 64 KiB; the M8 speed work raised it so that `user_id` gets a dictionary.)
- A predicate on a dictionary column is evaluated once against the dictionary, which turns it into a set or range of codes.
- A scan decodes a dictionary column's chunks as `Dictionary(UInt32, Utf8)` arrays that share the file dictionary's values, so no row's string is copied. Comparisons run on the codes: the dictionary is sorted bytewise, so the literal's position in it turns `=`, `<>`, `<`, `<=`, `>` and `>=` into comparisons of codes. Through DataFusion the columns come out as `Utf8View` built over the same values.

**Row order.** The writer is given the sort key (`["trace_id", "start_time"]`). It checks that rows arrive in that order and returns `StrataError::Unsorted` otherwise.

### S4. Payload layer

v2 replaces parts of this section; see § S4 in v2.

**Classification.** A Utf8 or Binary column is a payload column when its average non-null value length in the file is ≥ `config.payload_min_avg` (256 B). It is decided per file by the writer, which buffers the file (S7).

**Chunking.** The concatenation of a value's chunks must equal the value exactly.

1. **JSON array values.** The writer uses a hand-written scanner, not serde. The scanner:
   - tracks string state (`"` and backslash escapes) and nesting depth;
   - identifies the top-level elements of a value that, after leading whitespace, starts with `[` and ends (after trailing whitespace) with the matching `]`.

   Element *i*'s span runs from the byte after the separator that ends element *i − 1* (or from byte 0 for the first element) to the end of element *i*, including its trailing `,` if one follows. The last element's span also includes the closing `]` and any trailing whitespace. These spans are **units**.
2. **Pieces.** A unit < 8 KiB is one piece. A unit ≥ 8 KiB is cut into pieces by the gear content-defined chunker (min 512 B, average 2 KiB, max 8 KiB, the table and cut rule of `bench/src/payload.rs::Gear`).
3. **Other values** become pieces through the gear chunker directly: values that are not JSON arrays, and JSON the scanner rejects (unbalanced brackets or strings).
4. **Accumulation.** Pieces are appended, in order, to a pending buffer:
   - when the buffer reaches ≥ 256 B (`config.payload_min_chunk`) it is emitted as a chunk;
   - a gear piece is never split, so a chunk may exceed 8 KiB by less than 256 B;
   - at the end of the value, the remainder is emitted, even if it is short.
5. **Size rule.** Every chunk of a value except its last is ≥ 256 B; short pieces and gear tails are carried into the next chunk. A value shorter than 256 B is one chunk. This rule is what makes boundary search correct (below). Phase 3a tests it with a property test.

**Dedup.** A `HashMap<u128, u32>` per column maps `xxh3_128(chunk)` to a chunk id.
- **Invariant:** the chunks of one frame have consecutive ids, and frames are numbered in id order.
- The writer assigns ids in order of first appearance. A merge assigns them frame by frame (S10).

**One chunk store per file** (M8, after v0): all payload columns of a file share one frame builder. Rows are walked in order and, within a row, the payload columns in schema order, so a value and its near copy in a later column of the same trace (an answer, then the next call's input that repeats it) are written close together and zstd matches them; an identical chunk is stored once whatever its column. Each column keeps its own refs pages and header, and every header lists the same frames. Columns named in `config.own_store_columns` (default `input_preview`, `output_preview`) get a store of their own: previews are near copies of the payloads' first bytes and would fill the shared store, and as narrow columns their long values make chunks of a few dozen rows, which a lookup reads by the hundred. Search filters and edge groups are written only with `config.payload_filters` (default off, since they cost about 20% of the payload); without them `Contains` reads and searches every value, and a header without filters has no edge groups.

**Frames** (kind 2, zstd level 9):
- New chunks are appended to the current frame.
- A frame is closed when its raw size reaches 64 KiB, or when its estimated compressed size reaches 16 KiB (a#10). The estimate is raw size ÷ the column's running compression ratio, which starts at 4.0 and is updated after each frame.
- **Raw frame layout:** `u32 n_chunks` · `u32 end_offset[n_chunks]` (relative to the data start) · chunk bytes. Chunk *k* of the frame is `[end_offset[k−1] or 0, end_offset[k])`.

**Payload header** (kind 4, zstd), one per payload column, pointed to by the directory's `payload_offset` and `payload_len`:

```
u32 n_rows · u32 n_frames · u32 n_chunks · u32 n_ref_pages · u32 n_edge_groups
u64 filters_offset · u32 filters_len
ref page table:   n_ref_pages × (u64 offset · u32 len · u32 first_row)                              (16 B each)
edge group table: n_edge_groups × (u64 offset · u32 len · u32 first_chunk)                          (16 B each)
frame table:      n_frames × (u64 offset · u32 len · u32 raw_len · u32 first_chunk · u32 n_chunks)  (24 B each)
```

The fixed part is 4 × 5 + 8 + 4 = 32 B. The header grows with the number of frames (24 B each, before zstd). The writer reports its size, and G1 counts it.

Chunk id → frame is a binary search on `first_chunk`. A point lookup reads the header, the refs page, and only the frames it needs; there is no per-chunk table to fetch.

**Refs pages** (kind 3, zstd) use tail sharing (a#3). A page covers up to 4,096 consecutive rows:

```
u32 n_rows · u32 row_offset[n_rows + 1] (relative to the record area) · records
record := u8 tag, then:
  tag 0  null                       (no further bytes)
  tag 1  keyframe: varint n · n ids
  tag 2  based:    varint back (1..=32) · varint n_new · n_new ids
```

- **Id coding.** In a keyframe, the first id is a varint and each later id is a zigzag-varint delta from the previous id. In a based record, the first new id is a zigzag delta from the last id of the base's full list, and later ids are deltas from the previous one. An empty list is a keyframe with n = 0.
- **Base selection (writer).** Look at the previous ≤ 32 rows of the same page whose depth is ≤ 14. Pick the one whose full id list is the longest **complete prefix** of this row's list, verified by comparing ids. If none has a prefix of length ≥ 1, write a keyframe. Depth is 0 for a keyframe and the base's depth + 1 otherwise, so it never exceeds 15.
- **Decode.** `full(row)` is the keyframe's list, or `full(row − back)` followed by the new ids. It is memoised per page. A page decodes independently.

**Edge groups** (kind 6, zstd).
- One segment per 1,024 consecutive chunk ids, holding one fixed 64-byte entry per chunk: `u8 tail_len` · `tail[63]`, the last ≤ 63 bytes of the chunk, zero-padded.
- A chunk's head is not stored: the boundary check only runs on a pair whose right chunk sits in a candidate frame (below), and that frame is decompressed anyway.

**Filters** (kind 5, zstd):

```
u32 rollup_bits (0 = no rollup) · rollup bitmap (rollup_bits / 8 bytes)
u32 n_frames · per frame: u32 bitmap_offset (relative to the bitmap area; u32::MAX = no filter) · u32 bits
bitmap area
```

The trigram hash: for bytes `a, b, c`, `x = a | b<<8 | c<<16` as u64, then `h = ((x × 0x9E3779B97F4A7C15) >> 32) as u32`.

**Trigram sets.**
- The **in-chunk set** of a chunk is the trigrams inside the chunk.
- The **boundary set** of an adjacent chunk pair (L, R), as it occurs in some row's list, is every trigram of `tail63(L) ‖ head63(R)`.
- Every needle of 3–64 B that crosses the L/R boundary lies inside that 126-byte window, so all its trigrams are in the boundary set.

**Frame filter.**
- Frame *f*'s filter covers the in-chunk sets of its chunks, plus the boundary sets of every distinct pair whose right chunk R is in *f*.
- The writer buffers the file, so filters are finished after every row's pairs are known, then written.
- Size: with `d` the number of distinct `h` it receives, `bits = max(1024, round_up_64(2 × d))`, and a bit is `(h as u64 × bits) >> 32` (fastrange). This is the density measured in the prototype as `tri2k`: 16 of 16,629 frames read for a rare needle.

**Filter budget.**
- A frame gets no filter (`u32::MAX`, always scanned) if its filter would exceed 50% of the frame's compressed size.
- Phase 3c asserts that ≥ 80% of frames have a filter on the datagen and SWE-like test data. G7 is not judged if that fails on `swe-spans`.

**Rollup.**
- 2¹⁸ bits (32 KiB), set from every in-chunk and boundary trigram of the column: bit `h & (2¹⁸ − 1)`.
- Omitted (`rollup_bits = 0`) when more than 50% of its bits are set.

The writer reports filter bytes, rollup saturation and the percentage of frames without a filter in `FileSummary`. G6 counts filter, edge and header bytes as payload bytes.

**`Contains(needle)`** on a payload column, exact:
1. If `needle.len() > 64`, it is not supported. S8 marks it `Unsupported`, and the fast paths do not offer it.
2. If `needle.len() ≥ 3` and the rollup exists and any needle trigram's bit is clear, no row matches. This is safe because the rollup includes boundary trigrams.
3. **Candidate frames.** These are the frames with no filter, or whose filter has every needle trigram bit set (needles under 3 B: all frames). Decompress them and search each of their chunks with `memchr::memmem`, marking matching chunk ids.
4. **Boundary matches.** For each row, for each adjacent pair (L, R) of its chunk ids where R is in a candidate frame:
   - take `tail63(L)` from L's edge group (fetched only for the groups needed) and the head of R from R's decompressed frame;
   - search the last `needle.len() − 1` bytes of the tail, followed by the first `needle.len() − 1` bytes of the head;
   - cache the result per distinct pair.

   This is complete because every chunk except a value's last is ≥ 256 B, so a needle of ≤ 64 B spans at most two chunks. Any such crossing puts R's frame among the candidates.
5. A row matches if any of its chunks matches, or any of its pairs matches.

### S4 in v2 (M9)

Strata v2 replaces the payload layer's frames table, filters, edge groups and rollup above. The chunking rules, the refs pages and `Contains`'s exactness stay. The code is the reference: `strata/src/payload/{store,gram,search,copy,write}.rs`.

**Chunking.** Units of 2 KiB or more (was 8 KiB) are cut by the gear chunker with pieces of 256 B to 4 KiB (average about 1 KiB, mask 10 bits; was 512 B to 8 KiB). Smaller pieces let near copies of mid-size tool outputs dedup: −13% of payload bytes on `swe`. Pieces are still merged to ≥ 256 B (`MIN_CHUNK`). A JSON scan skips strings with `memchr`.

**Reuse of the previous value.** A value that equals the previous non-null value of its column reuses its chunk ids. A value that starts with the previous one, up to a chunk ending at a top-level cut (a conversation that grew by some messages), reuses those chunk ids and only cuts the rest. The prefix is checked byte for byte, so the result equals chunking from scratch. A value of 256 B or more is also remembered by its `xxh3_128`, so a value seen earlier anywhere in the store (a system prompt or tool list shared by many traces) reuses its chunk ids without being cut again. On `payload` it cuts the bytes that go through the chunker from 477 to 325 MiB per file (185 MiB end up unique). Comparing a value with the last 8 values of its column instead of the last one was tried and dropped: 10% fewer bytes to cut, no time saved, and slower writes on `swe`.

**Stores and runs.** A payload store (kind 6, `PayloadStore`) holds the unique chunks of one or more payload columns: one shared store per file, plus one per `own_store_columns` column. The two roles are written differently (`StoreRole`): the shared store gets zstd level 3 frames and a dictionary; an own store (the previews) gets level 1 frames and none, since its texts are short and a dictionary costs one training per store. Every store with frames gets a gram index. The directory record of a payload column points to its store with `dict_offset`/`dict_len`, and to its header with `payload_offset`/`payload_len`. The store body: `u32 n_chunks · u32 n_frames · u32 n_runs`, then per run `u32 first_frame · u32 dict_len · dict · u32 index_rows · u32 index_frames · u32 rows_per_page · u32 n_pages · n_pages × (u64 offset · u32 len)`, then per frame `u64 offset · u32 len · u32 raw_len · u32 n_chunks`. A write makes one run; a merge copies the runs of its inputs. The column header keeps only `u32 n_rows · u32 n_pages · n_pages × (u64 offset · u32 len)`.

**Frames.** A frame closes once it holds 64 KiB of raw chunk bytes (the compressed-size estimate is gone). It is compressed at its store's level and, for a shared store of at least 4 MiB of unique chunks, with a 64 KiB dictionary trained on 4 KiB samples of evenly spread frames (256 KiB in all) and attached (`ForceAttachDict`), not copied, per frame. Such frames use codec 2 (`ZstdDict`): `decode_segment` checks them, and the reader decompresses them with the run's dictionary, which is loaded with the store, in the metadata wave. A dictionary trained on one store is not reused by another: it fits another store's text badly and slows zstd down.

**Gram index** (kind 5, `PayloadIndex`, raw). Per run, one bit per frame and per row: row `r` of frame `f` is set when a 5-gram of `f`'s chunks hashes to `r` (`h = ((x & 0xFF_FFFF_FFFF) × 0x9E3779B97F4A7C15) >> 32` over the gram's 5 bytes, little-endian; `r = (h × rows) >> 32`). Grams that cross a chunk boundary are covered by windows, each the last 4 bytes of a chunk and the first 63 bytes of the chunk that follows it in some value, added to the left chunk's frame: for every chunk and the chunk with the next id (only the 4 crossing grams when both are in one frame), and for every other pair seen in a value. So every gram of a needle of ≤ 64 B lies in the frame of the chunk where an occurrence starts. `rows` is about the distinct grams of a frame ÷ 0.8 (estimated by linear counting on the first, middle and last frames), so about 55% of the bits are set. The matrix is stored bit-sliced, row-major, in pages of about 1 KiB. The writer sets one byte per row for the frame being indexed and packs it into the frame's bitmap when the frame ends, then transposes the bitmaps eight frames at a time with an 8×8 bit-matrix transpose: OR-ing into a bitmap word made each gram wait on the last store to that word.

**`Contains(needle)`**, exact, after the header and store (metadata):
1. One wave: the index rows of every gram of the needle, in every run. Their AND gives the candidate frames (every frame for a needle under 5 B).
2. One wave: every refs page of the column (scan coalescing) and the candidate frames (point coalescing, 4 KiB gaps).
3. In the candidate frames, a chunk is a hit when it holds the needle, and a partial hit for each `k` where it ends with the needle's first `k` bytes.
4. A row matches when it refers to a hit, or to a partial hit followed by chunks that hold the rest of the needle. A rest that needs a frame not read yet costs one more wave.

**Point reads.** A read of some rows of several payload columns fetches and decompresses each frame once, even when the columns share its store: frames are planned across the columns, by file offset. A refs page is decoded only up to the last row wanted from it, since a row's list builds on earlier rows only. On `payload`, the two cut a warm full-trace lookup's execute time from 4.57 to 3.81 ms (one partition).

**Building values.** The frames of a read are fetched as stored and decoded on first use, by the thread that needs them, so their bytes are still in cache when copied; a frame used by several columns or rows is decoded once. Values are built in pieces of 8,192 rows (one per output batch) on one thread per core. With `views` (the DataFusion path), a Utf8 column is built as `Utf8View`: a value whose chunks are contiguous in one decoded frame is a view into that frame's buffer, shared without a copy; other values are copied into blocks of up to 256 KiB, each value sized first so a block never grows. Each value is checked as UTF-8 when its view is added. Each thread keeps a zstd context loaded with the last dictionary it used. A wave of more than 4 MiB of other segments is decoded on several threads. A scan does its I/O up front, then builds batches a few at a time (one per thread) as DataFusion pulls them, and drops each frame after the last batch that uses it; fetched segments are slices of the read buffers, not copies. On `payload`, a full scan of `input` and `output` went from 674 to 370 ms (1.14× the best Parquet's minimum; 1.09× by median), CPU from 4.5 to 3.3 s (Parquet 2.1 s) and peak RSS from 4.6 to 1.84 GB (Parquet 0.77 GB); on `swe` from 499 to 290 ms (1.09×). The rest of the memory gap is frames shared across a file: later rows reuse early frames, so most of a file's decoded frames stay until near its end.

**Merge** (S10 in v2). When every input groups the same payload columns in the same stores, each store is copied: frames and index pages byte for byte, dictionaries as they are, chunk ids shifted by the chunks of earlier inputs. Only the refs pages are written again, in the merged row order. Identical chunks of two inputs are then stored twice, in two runs. Otherwise the payload columns are decoded and written again.

### S5. Indexes

**Sort-key lookup** (the first sort-key column, FixedSizeBinary of 1 to 16 bytes), using the sort-key block table (hot-tail section 3):
1. Candidates are the entries whose zone holds the key, compared bytewise over the first `w` bytes (`min ≤ key ≤ max`), plus the entries that have no zone. The table is scanned linearly; it is short.
2. The candidate chunks are fetched concurrently and decoded. The rows equal to `key` are collected, and adjacent rows are merged into ranges.
3. A sort-key filter that answers "absent" ends the lookup before any read.

The result is a list of row ranges (usually 1–2 chunks).

**Key filters.** A binary fuse 8-bit filter (a#2) from the `xorf` crate 0.13.0 (crates.io, `default-features = false`, feature `binary-fuse`).
- Built over `xxh3_64` of the distinct non-null values, sorted and deduplicated before construction. Integer and timestamp values are their 8-byte little-endian `i64`.
- A filter body starts with a tag:
  - `0` empty: no non-null value; always absent;
  - `1` one value, stored inline as `u32 len · bytes`, with `len ≤ 1024`;
  - `2` binary fuse: `u64 seed · u32 segment_length · u32 segment_length_mask · u32 segment_count_length · u32 n_fingerprints · fingerprints[n_fingerprints]`;
  - `3` unknown: the filter could not be built; always "may contain".
- Validity for tag 2: `segment_length` is a power of two; `segment_length_mask = segment_length − 1`; `segment_count_length ≥ segment_length` and is a multiple of it; `n_fingerprints = segment_count_length + 2 × segment_length`. These keep `contains` in bounds, so a forged geometry is `Corrupt`, never a panic. Any other byte after the body is `Corrupt`.
- Kept for the first sort-key column (hot-tail section 4, the body raw), and for `user_id` and `session_id` (kind 9 segments, indexed by hot-tail section 6). Only narrow columns with key bytes get one; maps and payload columns do not.
- A kind 9 segment's body is `u32 filter_len` · the filter body · the column's **chunk postings**, when the column has a dictionary (nothing otherwise).
- **Chunk postings** list, for each bucket of `2^shift` consecutive codes, the chunks holding one of them: `u32 n_codes` · `u8 shift` · `u32 n_buckets` · `u32 offsets[n_buckets + 1]` · the lists, each ascending chunk indexes as LEB128 deltas. `shift` is the smallest giving at most 4,096 buckets. A scan whose predicate matches at most 256 codes of the column reads only the listed chunks; `StrataFile::may_match` uses them, with the zones, to drop whole files before DataFusion partitions are planned.
- The only accessor is `StrataFile::may_contain(column, value) -> Result<Option<bool>>`. `Some(false)` is a proof of absence; `None` means the column has no filter.

### S6. Shredded maps

Applies to `Map<Utf8, Utf8>` columns.

**Promotion.** A key becomes a child column (layer 3, narrow rules, usually `DICT`) when it occurs **exactly once with a non-null value** in at least 5% of the map column's rows. The count is over rows, rounded up, with a minimum of one row. A null map row counts in the denominator.
- A key longer than 1,024 bytes is never promoted.
- At most 64 keys are promoted (`MAX_PROMOTED_KEYS`). Candidates are ranked by count, descending, then by key bytes, ascending, and the top 64 are kept. The promoted keys are stored sorted bytewise.
- A child's null means "key absent", or "the key stays in the residual of this row".
- A row in which a promoted key has a null value, or appears more than once, keeps every occurrence of that key in the residual (the child is null for that row).
- For each promoted key the writer records `residual_hits`: the number of rows in which the key was present but stayed in the residual (a null value, or more than one occurrence). Each row counts once. S8 uses it.

**Null versus empty map.** The residual column carries the map's validity:
- a null residual means the map is null;
- an empty residual with every child null means an empty map.

**Residual.** Keys that are not promoted form the residual map column (layer 4, `MAP` encoding).

**Reconstruction.** The map is rebuilt with entries sorted bytewise by key, using the field names and nullability of the map's type in the Arrow schema. cetra-core already writes maps from `BTreeMap`s, and tests compare maps with sorted keys. `unshred` returns each row's entries sorted bytewise by key (stable for equal keys); the original entry order is not kept.

**Hot-tail section 5 layout:** `u32 n_maps`, then per map: `u32 map_column_id` · `u32 residual_column_id` · `u32 n_keys` · `n_keys × (u32 child_column_id · u32 residual_hits · u16 key_len · key bytes)`.

Children are not in the Arrow schema; they exist only in the directory.

### S7. Writer and reader APIs

```rust
pub struct Config { zstd_level: i32, payload_min_avg: usize, payload_min_chunk: usize, /* … */ }
pub struct StrataWriter;
impl StrataWriter {
    pub fn new(schema: SchemaRef, sort_key: Vec<String>, config: Config) -> Result<Self>;
    pub fn write(&mut self, batch: &RecordBatch) -> Result<()>;            // buffers; checks order
    pub fn finish<W: std::io::Write>(self, out: W) -> Result<FileSummary>; // all encoding happens here
}
pub struct FileSummary { pub rows: u64, pub bytes: u64, pub hot_len: u32, pub per_column: Vec<ColumnSummary>, /* filter bytes, rollup saturation … */ }

pub struct StrataFile;
impl StrataFile {
    pub async fn open(store: Arc<dyn ObjectStore>, path: Path, hint: Option<u32>, config: &Config) -> Result<Self>;
    pub fn schema(&self) -> SchemaRef;                     // file schema (Utf8, Binary)
    pub async fn lookup(&self, key: &[u8]) -> Result<Vec<Range<u32>>>;
    pub async fn lookup_scan(&self, key: &[u8], projection: &[usize]) -> Result<Vec<RecordBatch>>; // M4.3; the W0–W3 path below
    pub fn may_contain(&self, column: &str, value: &[u8]) -> Option<bool>;
    pub async fn scan(&self, req: &ScanRequest) -> Result<Vec<RecordBatch>>; // M4.2
    pub fn io_stats(&self) -> IoStats;                     // requests, bytes, waves
}
pub struct ScanRequest { projection: Vec<usize>, predicate: Option<Predicate>, row_ranges: Option<Vec<Range<u32>>>, limit: Option<usize>, kind: ScanKind /* Point | Scan */ }
pub enum Lit { Null, Bool(bool), Int(i64), UInt(u64), Float(f64), Utf8(String), Bytes(Vec<u8>) }
pub enum CmpOp { Eq, NotEq, Lt, LtEq, Gt, GtEq }
pub enum Predicate { And(Vec<Predicate>), Or(Vec<Predicate>), Not(Box<Predicate>), Cmp(usize, CmpOp, Lit),
                     In(usize, Vec<Lit>), IsNull(usize), IsNotNull(usize), MapKeyEq(usize, String, Lit), Contains(usize, Vec<u8>) }
```

Returning a `Vec` instead of a stream is a prototype simplification; M5 wraps it.

**The writer buffers the whole file.** Every decision is per file: payload classification, dictionary eligibility, map promotion. G8 compares against the Parquet writer on the same data, and the report states that Parquet streams while Strata buffers.

**Predicate semantics.** SQL three-valued logic. A row is selected only when the predicate is TRUE; NULL compares as unknown.

**Waves.** A **wave** is one `try_join_all` batch of requests. `IoStats` counts waves, requests and bytes. Metadata already cached costs no request and no wave.

**`scan`, wave by wave:**
1. **Metadata.** Fetch, not yet cached: the block-table pages covering the requested row ranges (all pages of a column for a full scan) for every predicate and projection column; their dictionaries; the payload headers of the payload columns involved; the residuals of `MapKeyEq` masks; and, for `Contains`, the filters segment.
2. **Filter.** Zones prune chunks, and dictionary predicates become code sets. Fetch the surviving chunks of the predicate columns, and evaluate them to a row mask. `MapKeyEq` on a promoted key is a plain comparison on its child column; other `MapKeyEq` terms are masks over the residual.
3. **Project.** Fetch the projection's narrow chunks that hold a selected row, plus the refs pages covering the selected rows.
4. **Frames.** Fetch the frames the selected rows reference that are not already loaded.

Waves with nothing to fetch are skipped. Without `Contains`, a cold scan therefore takes at most 4 waves after open. Each `Contains` term adds up to 4 waves of its own, before the filter wave: its filters, refs pages, candidate frames and edge groups, as `payload_contains` reads them.

**Output.** One batch per chunk-aligned range of up to 8,192 rows. Columns use the file schema's types (Utf8, not Utf8View); the DataFusion layer converts (S8).

**I/O.**
- Strata does its own coalescing, then issues one `store.get_range(path, range)` per merged range, all concurrently (`futures::future::try_join_all`). It does not use `get_ranges`, whose built-in 1 MiB coalescing would undo the point-lookup policy.
- `ScanKind::Point` merges ranges whose gap is < 4 KiB (64 KiB in v0, which doubled the bytes of a lookup); `ScanKind::Scan` merges gaps < 1 MiB, with merged ranges capped at 16 MiB.
- `StrataFile` caches the hot tail, block tables, dictionaries, payload headers and key filters after first use.

**Dependent waves.** A wave is one batch of concurrent requests that needs the results of the previous batch. Cold (nothing cached), a single-trace all-columns lookup is:

| Wave | Reads |
|---|---|
| W0 | open: the suffix read |
| W1 | The candidate sort-key chunks found in hot-tail section 3. Their entries give a candidate row span (from the first chunk's `first_row` to the end of the last), with no decoding needed. Also, for that span: the block-table pages of the projected columns, their dictionaries, and the payload headers. |
| W2 | Decode the sort-key chunks to get the exact row ranges. Fetch the projected narrow chunks overlapping those ranges, plus the refs pages covering them. |
| W3 | the frames |

`lookup_scan` drives this sequence, as M4.3 sets it out. Cold, a single-trace lookup takes 3 waves (W1, W2, W3) when a payload column is projected, and 2 (W1, W2) when none is: W3 is skipped. When the metadata is already cached, W1 holds only the candidate sort-key chunks, the narrow chunks overlapping the candidate span and the refs pages of its rows, so the lookup takes 2 waves with a payload column (W1, then the frames) and 1 without. Absent keys that the sort-key filter or the zones rule out cost no wave at all.

The Parquet baseline is measured with the same counting store (`bench/src/counting.rs`), so its waves and bytes come from the same instrument.

### S8. DataFusion integration

**`StrataTable`.**
- `StrataTable::open(store, paths, &Config)` opens the files concurrently; `StrataTable::new(files)` takes files already open. Every file must have the same schema and the same sort key.
- `schema()` is the file schema with top-level Utf8 → Utf8View and Binary → BinaryView. This matches DataFusion's Parquet reader with `schema_force_view_types = true`; `StrataExec` casts its output.
- `supports_filters_pushdown` returns, per filter expression:

| Expression shape | Result |
|---|---|
| `trace_id = <FixedSizeBinary literal of 16 B>` | `Exact` (key index + filter); lookup mode when it is the only filter |
| `col {=,<,<=,>,>=,!=} literal` (either side), `col IN (literals)`, `col IS [NOT] NULL`, `col BETWEEN`, with `col` a narrow column of a comparable type, and timestamp literals of any timezone normalised to ns UTC | `Exact` |
| `AND`, `OR` of `Exact` shapes; `NOT` of an `Exact` shape with no `Contains` term | `Exact` |
| `AND` or `OR` mixing `Exact` and `Inexact` terms | `Inexact` |
| any `AND` or `OR` with a `No` term | `Unsupported` |
| `get_field(map_col, 'k') = literal`, with `k` promoted and `residual_hits = 0` in **every** file | `Exact` (`MapKeyEq`): `get_field` returns the first entry with the key, so the same key in the residual would differ |
| `get_field` in any other case, including `k` in some file's residual or not promoted | `Unsupported` |
| `col LIKE '%lit%'` on a payload column, `lit` 1–64 B, with no `%`, `_` or escape character inside | `Exact` (`Contains`, which is exact since v2). Under `NOT` it is `Unsupported`: `Contains` is FALSE on a null row, where `LIKE` is NULL |
| any `Cast` or `TRY_CAST` on the column, a non-literal operand, `ILIKE`, anything else | `Unsupported` (no unwrapping of casts) |

- Literal types must match the column's file type exactly; a mismatched literal is `Unsupported`.
- Float columns (Float32, Float64) are never pushed: a comparison, `IN` or `BETWEEN` on them is `Unsupported`. DataFusion compares floats with `-0.0 == 0.0`, while Arrow's kernels, which Strata uses, order them by the IEEE total order, so the two would disagree.
- A boolean literal `TRUE`, `FALSE` or `NULL` is `Exact` and becomes the `Const` predicate, a constant TRUE, FALSE or NULL for every row. DataFusion's simplifier produces these when it rewrites a filter, for example `i IN (1, NULL)` becomes `i = 1 OR NULL`. A top-level FALSE or NULL conjunct makes `scan` prune every chunk.
- `supports_filters_pushdown` accepts a filter only while the predicates it has accepted before still fit the predicate size limit together; a filter that would not fit is `Unsupported`.
- `scan()` translates the pushed filters to `Predicate` and leaves `Unsupported` filters to DataFusion. DataFusion passes it only the filters that were accepted, so a filter that does not translate there, or a set of filters that no longer fits the size limit together (the same running check as above), is a `DataFusionError::Plan` and never a dropped filter; a dropped filter would return wrong rows. For `trace_id` equality it calls `may_contain` on each file and drops the files whose filter says no. It uses `lookup_scan` only when that filter is the only kept filter and there is no `limit`.
- `scan()` needs the hot tails, so it opens the files (cached in `StrataTable`). Planning a lookup can therefore cost the open wave.

**`StrataExec`.**
- One partition per remaining file; with no file left, one partition that reads nothing. A `trace_id` lookup that leaves one file therefore reports 1 partition, as does one that leaves none.
- `output_ordering`: the first sort-key column only, declared when that column is in the projection. Each partition reads at most one file, so the order holds per partition.
- `limit` reaches `scan` only when DataFusion passes it, which it does only when every pushed filter is `Exact`.
- Metrics, registered with `MetricBuilder`:

| Metric | Kind |
|---|---|
| `output_rows` | counter |
| `elapsed_compute` | time |
| `strata_bytes_fetched` | counter |
| `strata_requests` | counter |
| `strata_waves` | counter |

  The counters come from `IoStats` before and after each partition's read. Strata's `scan` does not report chunks pruned or frames read, so those two counters are dropped.

### S9. Fast paths (no DataFusion)

```rust
pub async fn trace_detail(files: &[Arc<StrataFile>], trace_id: [u8; 16], projection: &[&str]) -> Result<RecordBatch>;
pub async fn trace_list(files: &[Arc<StrataFile>], time: Range<i64>, predicate: Option<Predicate>, k: usize, projection: &[&str]) -> Result<RecordBatch>;
```

Both return batches in the `StrataTable` schema (`table_schema` of the file schema), so they match DataFusion's rows. An empty result is an empty batch of that schema.

**`trace_list` ordering.** Rows are ordered by `(start_time DESC, trace_id DESC, span_id DESC)`. The bench query `list_llm_1h` is changed to use the same tie-break, so DataFusion and the fast path agree.

**`trace_list` algorithm:**
1. Read the `start_time` block table of every file. Keep the chunks whose zone overlaps `time`, and order them by `max` descending.
2. Scan the kept chunks one at a time, in that order, with the window and the predicate, and the projection of each chunk. Keep a top-k heap of the rows seen.
3. Stop when the heap holds `k` rows and the next chunk's `max` is strictly below the heap's minimum `start_time`. Equal times still continue, because of the tie-break.
4. Build the output columns from the kept chunks with `interleave`, in heap order.
5. **Compaction.** After each chunk, when the kept batches hold more than `2k + 8,192` rows, they are replaced by one batch of the heap's rows, and each heap entry points at its row of that batch. The rows keep their (batch, row) order, so ties resolve as before. Memory then stays proportional to `k`, not to the window. An empty projection gives a batch with the row count and no columns.

Each visited chunk costs at least one wave. The early stop only helps when `start_time` zones are narrow, that is, when the file is sorted by time. Zone pruning on `start_time` works only if chunks cover narrow time ranges. Files sorted by `trace_id` within a time slice give wide `start_time` zones, so the report states which dataset layout the gain depends on.

### S10. Merge

```rust
pub async fn merge(inputs: &[Arc<StrataFile>], sort_key: &[&str], config: Config) -> Result<(Vec<u8>, FileSummary)>;
```

Inputs must share one schema and be sorted by the first sort-key column; both are checked. Empty inputs are skipped. Output is returned as bytes, like the writer. No input count is limited.

1. **Sort key.** K-way merge of the inputs' sort-key columns; ties are broken by input index, then row. This produces a row-source map of one `(input, row)` pair per output row, 16 bytes per row, with no input-count limit. The map is grown by pushing, not pre-sized from the files' claimed row counts.
2. **Re-decide per output file.** The output is written by the writer's own code (`write_file`) from the merged column arrays, so every per-column decision is taken again on the output data: payload classification, dictionary eligibility, and map promotion and key filters. A column whose layer differs between inputs is decoded fully and re-encoded under the output's decision. Frame copying (step 4) applies only to columns that are payload in the input and the output.
3. **Narrow columns,** one at a time: each column is decoded from every input, interleaved by the row-source map and encoded again. M6.1 does this for every column.
4. **Payload columns** (M6.2; until then M6.1 re-encodes them like narrow columns, from the merged arrays).
   - Decompress every input frame and hash its chunks with `xxh3_128` (no hash is stored in the file).
   - Inputs are processed in input order, frame by frame. An input frame is **copied byte for byte** when every chunk in it is new to the output and referenced by at least one output row. Its chunks then get the next consecutive output ids, keeping the S4 invariant, and the frame's raw layout is unchanged. The trigram filter is rebuilt, since boundary pairs change.
   - Every other chunk that is still referenced is re-packed into new frames.
   - Refs, payload headers, filters and edge tables are rebuilt.
5. Key filters, the hot tail and the rollups are rebuilt.

Memory:
- Narrow phase: peak ≤ 2 × the largest single column's decoded Arrow size across the inputs, plus the row-source map (16 bytes per row) and the sort-key row format. Tested on 4 inputs of 1,000,000 rows in release (`tests/merge_memory.rs`, M6.1); the test allows 48 bytes per row for the map and keys, and half the largest column for the output and codecs.
- Payload phase: peak memory is reported, not bounded. The frame copy keeps the merged array plus every input's stored and raw frames of that column in memory.

## Phases

Each phase ends with `cargo test` and `cargo clippy --all-targets -- -D warnings` clean in every crate it touches, and is one commit. The commit message lists any new dependency with its exact version.

**Phase 0: test-data crate.**
- Create `testdata/` (`fmt-testdata`). Move `bench/src/datagen.rs` into it, and move `bench/src/main.rs::swe_cmd`'s trajectory → span-input logic into `swe.rs`. The SWE function takes a Parquet path and returns `Vec<(input, output, system)>` values; the spans with narrow columns come in phase 9a.
- Make `bench` depend on it. `fmt-testdata` depends on `cetra-core` by path; that crate inherits its dependency versions from the cetra workspace, so it builds from outside that workspace root.
- Test: `fmtbench gen` on 3,000 traces gives byte-identical Parquet files before and after (compare `sha256sum`).

**Phase 1: container.**
- Crate skeleton, `Config`, `StrataError`, the segment writer and reader, header, postscript, hot-tail framing (all seven sections, empty), and `open` (S1).
- The test data is a file with only raw test segments (kind 1, arbitrary bytes) and an empty directory.
- Tests:
  - round trip of 1,000 random segments, raw and zstd;
  - a flipped byte in a segment body or in the hot tail → `Corrupt`;
  - a truncated file, a wrong magic or a wrong `hot_offset` → `Corrupt`;
  - open costs 1 read when the tail fits the hint, and 2 when it doesn't;
  - an empty file (0 rows) opens;
  - **no panic on garbage**: a proptest feeds random bytes and random truncations to `open` and to the segment reader, and expects `Ok` or `Err`, never a panic. Every later phase adds the same proptest for each decoder it introduces (S3 encodings, refs pages, frames, filters, edge groups, payload header, map keys). Arrow IPC decoding keeps validation on.

  Reads are counted with a wrapping `ObjectStore` (copy `bench/src/counting.rs`).

**Phase 2a: narrow core.**
- Directory, block-table pages and hot-tail section 7, the sort-key copy in hot-tail section 3, `CONST`, `CONST_NULL`, `FOR_BITPACK`, `PLAIN`, `FSB_RAW`, `FSB_RLE` and `BOOL`; zones; validity bitmaps; the chunking rule. The writer for these types, and a reader that decodes whole columns.
- Tests: for each encoding, a proptest round trip (proptest version looked up), with arrays that include nulls, all-null chunks, one-row chunks, width-0 and width-64 bit-packing, `i64::MIN..=i64::MAX` ranges, NaN and −0.0; plus a test that a constant chunk writes no segment.

**Phase 2b: dictionaries.**
- Sorted dictionaries with their caps, `DICT` and `LIST_DICT`.
- Tests: proptests per encoding, including a list chunk with a null element, which must fall back to `IPC` once phase 2c lands (until then it is an `Unsupported` error).

**Phase 2c: varbin, IPC and the light round trip.**
- `VARBIN` and `IPC`, plus the writer's per-column chunking over a full schema.
- Tests:
  - proptests per encoding;
  - `strata/tests/roundtrip_light.rs`: a 50,000-row batch from `fmt_testdata::datagen` (light mode) is written and read back, and every column equals the input.

**Phase 3a: chunking.**
- `strata::payload::chunk`: the JSON scanner, gear pieces and accumulation (S4 steps 1–5). This is a rewrite: the bench copy in `bench/src/payload.rs` stays untouched.
- Tests:
  - a proptest over random JSON arrays and random bytes checks that chunks concatenate to the value and that every non-last chunk is ≥ 256 B;
  - edge cases: null, empty, 1 B, 255 B, 256 B, 1 MiB, invalid JSON, nested JSON, a JSON array with a 100 KiB element, a string containing `]` and `\"`.

**Phase 3b: payload storage.**
- Dedup, frames, payload header, refs pages with tail sharing (S4). The writer's `finish` now emits payload columns (narrow columns as in phase 2).
- Tests:
  - round trip of every value of a 3,000-trace datagen file;
  - tail-sharing depth never exceeds 15;
  - a based record's base is within 32 rows;
  - the frame-id invariant holds.

**Phase 3c: payload search.**
- Edge groups, frame filters with boundary sets, rollup, `Contains`.
- Tests:
  - `Contains` equals brute-force `memmem` over the values, for 100 needles of length 1–64 drawn from the data (present) and 100 random ones (absent);
  - 100 needles placed to cross a chunk boundary, with ≥ 1 byte on each side (so at least one boundary trigram), including at least 30 whose 4th or later trigram crosses it;
  - ≥ 80% of frames have a filter, on the 3,000-trace datagen file and on a 200-trajectory SWE sample (`#[ignore]`, run with `FMT_SWE_SHARDS`);
  - the summary's rollup saturation and filter bytes are printed.

→ **Sonnet review 1** (phases 0–3c).

**Phase 4: indexes and maps.**
- Key lookup, binary fuse filters, map shredding and reconstruction (S5, S6).
- Tests:
  - every trace ID of a datagen file returns exactly its rows;
  - 10,000 absent IDs return none, and fewer than 1% pass the filter;
  - maps round trip (keys sorted);
  - a null-valued promoted key and a duplicated key go to the residual;
  - `MapKeyEq` on promoted and residual keys.

**Phase 5a: narrow scan.**
- `scan` for narrow columns: pruning, dictionary predicates, three-valued logic, late materialization, coalescing, waves, `IoStats`.
- Tests:
  - 30 predicates, with `scan` results equal to Arrow compute kernels over the fully decoded file;
  - point versus scan coalescing produces the expected merged ranges on a synthetic layout.

**Phase 5b: payload scan.**
- Payload projection (refs → frames) and `Contains` inside `scan`.
- Tests:
  - an all-columns single-trace lookup through `lookup_scan` equals the decoded rows;
  - `IoStats.waves` is 4 cold (open included) and 2 with metadata cached.

**Phase 6: DataFusion provider.**
- `StrataTable` and `StrataExec` (S8).
- Tests (`strata/tests/datafusion.rs`): every query of `fmtbench query` returns the same rows on Strata files as on Parquet files written from the same batches. Both sides are cast to the canonical schema and compared with map keys sorted. Rows are compared in order where the query has `ORDER BY`, as sorted multisets otherwise.

→ **Sonnet review 2** (phases 4–6).

**Phase 7: fast paths** (S9).
- Tests: `trace_detail` and `trace_list` equal the DataFusion results for 100 trace IDs and 10 time windows, with the S9 tie-break.

**Phase 8: merge** (S10).
- Tests:
  - merging 4 files equals writing their concatenated, sorted rows in one file (compare decoded batches);
  - a frame whose chunks are all new is byte-identical in the output;
  - the narrow-phase memory bound holds (a counting `GlobalAlloc` in the test binary);
  - merging an empty file is a no-op.

**Phase 9a: `swe-spans` dataset.**
- `fmt_testdata::swe::spans(parquet_paths, max_trajectories) -> Vec<Span>`.
- Input: the Hugging Face dataset `nebius/SWE-agent-trajectories` (CC-BY-4.0), shards `data/train-0000{0,1}-of-00012.parquet`. Their Parquet column `trajectory` is a `List<Struct{cutoff_date: Utf8, mask: Bool, role: Utf8, system_prompt: Utf8, text: Utf8}>`. The orchestrating session downloads them; paths are passed as arguments.
- Mapping:
  - each trajectory becomes one trace, with `trace_id` = `xxh3_128` of the trajectory index;
  - one root `agent` span;
  - one `llm` span per assistant (`role = "ai"`) message: `input` = GenAI message JSON of the earlier non-system messages, `output` = the reply, `system_instructions` = the system prompt;
  - one `tool` span per `user` message after the first, with `output` = its text;
  - timestamps start at a datagen-style random start and advance 2 s per step;
  - other narrow columns are filled with `datagen`'s helpers.
- Tests: `#[ignore]`, run only when `FMT_SWE_SHARDS` (a comma-separated list of paths) is set. With `max_trajectories = 1674`, the LLM span count and payload byte total match the `fmtbench swe` figures in README.md (47,198 LLM spans, 2.08 GB of payload).
- Memory: the full conversion holds about 2 GB of strings plus the buffering writer, so it runs on `ubuntu-vm` (62 GB).

**Phase 9b: bench variants.**
- `fmtbench`:
  - `gen` writes an `st` variant;
  - `query` runs `st` (DataFusion) and `st_fast` (fast paths, for the trace lookup and trace list queries);
  - a new `trace_lookup_cold` query opens fresh `StrataFile`s and a fresh `SessionContext` per lookup, with metadata caches cleared;
  - `IoStats` waves are printed;
  - a `--dataset swe` option for `gen`;
  - **`pq_fast`**, a hand-written async arrow-rs Parquet lookup, as the "no engine" floor for Parquet: `ParquetRecordBatchStreamBuilder` with cached metadata, row groups pruned by statistics, a `RowSelection` built from the `trace_id` page index, and the projection. It is the fair counterpart of `st_fast`.

→ **Sonnet review 3** (phases 7–9b).

**Phase 9c: runs and report** (orchestrating session, not Haiku). § Benchmark protocol, then README § Results and a go/no-go section.

## Benchmark protocol

**Datasets:**

| Name | Content |
|---|---|
| `payload` | synthetic, 60,000 traces |
| `light` | synthetic, 300,000 traces, `FMT_LIGHT=1` |
| `swe-spans` | phase 9a, both shards |

Every variant is written from the same sorted batches with the same file slicing (4 files per dataset), so row order is the same.

**Variants:** `pq_default`, `pq_tuned`, `pq_cetra`, `pq_fast` (lookups only), `vx_compact`, `st`, `st_fast`. "Best Parquet" means the best of the three Parquet variants on that metric.

**Storage:**
- **Local tmpfs:** CPU-bound.
- **S3 with latency:** Versity Gateway v1.11.0 behind toxiproxy 2.13.0, adding 20 ms ± 5 ms per request, on `ubuntu-vm`. Both images are already pulled there (`~/ENV.md`). The orchestrating session sets up the proxy, using the S3 environment variables of `docs/research/storage/benchmarks.md`.

**When and how timings are taken:**
- Final timings run on `ubuntu-vm` while it is otherwise idle; the development machine runs builds.
- Every figure is the minimum of 30 runs, with median process CPU time beside it, and the load average logged.
- A run with a 1-minute load average above the core count is marked "loaded".
- Sizes, bytes fetched, requests and waves are deterministic. They are the primary metrics.

**Warm and cold.**
- *Warm*: files open, metadata cached, OS page cache warm.
- *Cold*: fresh process state, nothing cached in the process. On S3 the gateway's own cache may still be warm, which the report notes.

## Go / no-go criteria

"Best Parquet" is measured per metric and per query.

| # | Criterion | Dataset | Measured as | Go if |
|---|---|---|---|---|
| G1a | Trace lookup, all columns, **metadata cached**, S3 | `payload`, `swe-spans` | data bytes fetched and waves, mean over 20 trace IDs | ≤ 0.5 MB and ≤ 2 waves; best Parquet (footer cached) needs ≥ 2× the bytes |
| G1b | Trace lookup, all columns, **fully cold**, S3 | `payload`, `swe-spans` | all bytes fetched, metadata included, and waves | ≤ 0.5 × best Parquet's bytes, and ≤ 4 waves |
| G2 | Warm trace lookup, no engine: `st_fast` vs `pq_fast` | `payload` | min wall time | light columns ≤ 0.5 × `pq_fast`; all columns ≤ 0.5 × `pq_fast` |
| G3 | Warm trace lookup through DataFusion: `st` vs best Parquet | `payload` | min wall time, and execute time from the plan metrics | total ≤ 0.75 ×, and execute ≤ 0.5 ×, for both light and all columns |
| G4 | Narrow queries: count, rollup, list, user filter | `light` | min wall time, `st` | **for each query:** ≤ 1.3 × `vx_compact` **and** ≤ 0.6 × best Parquet |
| G5 | Map-key filter | `light` | min wall time, `st` | ≤ 0.5 × best Parquet (`vx_compact` cannot run it) |
| G6 | Payload column bytes (filters, edges and headers included) | `swe-spans` | bytes on disk | ≤ 0.6 × best Parquet's bytes for those columns |
| G7 | Rare substring search (`LIKE '%needle%'`, 20 needles of 8–32 B with 1–1,000 hits) | `swe-spans` | warm min wall time with one partition, and cold bytes fetched | both ≤ 0.2 × best Parquet. Not judged if under 80% of frames have a filter. |
| G8 | Write throughput | `payload` | Arrow MB/s, one thread for both formats; Parquet streams, Strata buffers | ≥ 0.8 × `pq_default` |
| G9 | Merge 4 files into 1 | `payload` | wall time from the same 4 input files on local disk: Strata `merge`, against reading the 4 Parquet files and writing one sorted Parquet file with `ArrowWriter` (one thread each) | ≤ 0.5 × |

**Outcome.** "The payload layer wins on its own" means G6 and G7 both pass.

| Result | Decision |
|---|---|
| G1a, G1b, G4 and G6 pass, and at most two of the others fail | **Go**: build the real format. |
| G1 and G6 pass, G4 fails | **Go with Vortex-style narrow encodings**: v1 swaps in `fastlanes` or the Vortex encoding crates before deciding. |
| G1 fails, and the payload layer wins on its own | **Redesign**: store payloads as a sidecar next to Parquet (README § cheaper alternative). |
| G6 fails, G7 passes | **Redesign**: keep the trigram filters, drop dedup. |
| None of G1a, G6 and G7 pass | **Stop.** |
| Any other combination | Written up case by case in the report, with these rules as the default. |

## Risks

- **Scalar bit-packing and no ALP may lose G4 to Vortex's SIMD kernels.** If G4 fails on CPU only, v1 adopts the `fastlanes` crate (already in the local cargo registry) or the Vortex encodings.
- **Tail sharing relies on trace-sorted rows.** Other orders fall back to keyframes: correct, but larger.
- **Trigram filters may saturate on code-heavy payloads.** Phase 3c measures saturation and filter coverage before G7 relies on them.
- **The buffering writer needs the whole file in memory,** which suits the 512 MiB compaction target. Streaming writes are a v1 concern.
- **Payload values are rebuilt into new buffers,** one copy per value. If G2 fails on that, v1 decodes into `Utf8View` buffers directly.
- **The payload header grows with the frame count** (24 B per frame before zstd). On the synthetic data that is about 19k frames per column, or about 0.1–0.4 MB per file cold. It counts in G1b, and paging it like the block tables is the v1 fix if G1b fails on it.
