# Techniques from non-analytics file formats (survey A)

Date: 2026-11-15. Status: research. Companion to [README.md](README.md) (the "Strata" two-layer design). Parquet and Vortex are not reviewed here.

**Sourcing.** squashfs/EROFS claims cite the local kernel tree (`/home/sebastien/github/linux/fs`). Everything else comes from the published specs and papers, recalled from the primary documents and **not re-fetched in this session**; check a number against the spec before relying on it. Numbers marked "est." are my arithmetic, not measurements.

Workloads: **A** narrow scans, **B** point lookup by sorted key, **C** large repetitive text and substring search, **D** write, size and compaction, **E** local disk vs S3.

---

## 1. Media containers

### 1.1 ISOBMFF / MP4 (ISO 14496-12)

**Sample tables.** Random access in `stbl` is a set of small tables, each in the most compact form its regularity allows:
- `stsz`: a single `sample_size` u32; if non-zero, all samples are that size and **no table follows**.
- `stco` (u32) vs `co64` (u64): the writer picks the width per file.
- `stsc`: runs `{first_chunk, samples_per_chunk, desc_index}`, one entry per *change*. `stts`: runs `{count, delta}`; a constant frame rate is one entry. `stss`: sorted keyframe numbers.
- Box header is `size u32 + fourcc`; `size==1` adds a u64 `largesize`; `size==0` means "to EOF".

**Fragmented MP4.** Each `moof`+`mdat` is self-describing with offsets relative to the `moof`. The optional `mfra` at the end holds `{time, moof_offset}`; its last box `mfro` is 16 fixed bytes ending in the `mfra` size, so a reader finds it from the end. A crash-truncated file is playable up to the last complete fragment by scanning.

**Steal / avoid:**
- *Per-table width and constants* (`stsz` constant, `co64` only when needed). Strata's block table is 24–32 B per block; declared widths and per-column constants (`rows = 8192` everywhere) cut it 3–4× (est.). A, B, E; low.
- **Self-describing segments plus a footer rebuildable by scan.** A 16 B header per block/frame `{magic, kind, column, rows, len, xxh3}` buys crash salvage and concatenation without offset rewrites. D; **NEW**.
- *Avoid faststart* (moov-first): it needs a two-pass write or reserved padding, and on S3 a suffix range read gets the tail in the same single round trip (4.5).

### 1.2 Matroska / WebM, FLAC (one-liners)

- **EBML varints**: the leading zero bits of byte 0 give the length (1–8 B), as in QUIC varints. For the payload layer's chunk-ID lists use prefix-varint or StreamVByte (est. 2–4 GB/s decode vs ~1 GB/s LEB128). C; trivial. Matroska `Cues` (`CueTime`, `CueClusterPosition`, `CueRelativePosition`) behind a front `SeekHead` costs an extra dependent read; Strata's tail-first design already avoids that.
- **FLAC** `SEEKTABLE`: 18 B points `{sample u64, offset u64, n_samples u16}`, sparse (one per ~10 s). Between points it bisects and **resyncs on a 14-bit sync code + CRC-8/CRC-16**. Bisection is log2(size/granularity) *dependent* reads: fine locally, **avoid on S3**; keep only as the salvage path. FLAC also picks predictors per frame by trial and a Rice parameter per partition of a few hundred samples: confirms Strata's "bit-width per 128–1024 values, try several, keep the smallest". Nothing new.

### 1.3 Video GOPs as an analogy for conversation deltas

An I-frame is intra-coded; a P-frame stores a residual against a reference. A seek decodes from the last I-frame, so GOP length (1–10 s) bounds worst-case seek cost against size. H.264 *long-term reference pictures* are exactly what a system prompt and tool schema are in a trace.

CDC dedup is "intra-only with a global reference store": no chain, no seek penalty, chunk granularity (~2 KiB). The analogy suggests a gap in the reference lists:

- **Tail-sharing ref lists (NEW).** Step *k* is step *k−1* plus a suffix. Store `row.refs = (parent_row, shared_prefix_len) + new_chunk_ids` instead of full lists. A 200-step trace with ~250 chunk refs of history at late steps costs O(n²) ≈ 25k–50k refs; tail sharing costs O(n) ≈ 250 refs plus 200 parent pointers. Bound the walk like a GOP: a full-list "keyframe" row every 16 steps, so at most 15 hops, all rows in the same block. C, D; est. 5–20× smaller ref-list column on agent data; low cost.
- Expose the new suffix as a logical `input_delta` column so the UI and judge paths do not re-materialise 500 KB of repeated history per row. C.

---

## 2. Compression containers

### 2.1 zstd seekable format

Independent zstd frames, then a **skippable frame** (magic `0x184D2A5E`, ignored by stock decoders) holding the seek table: per frame `{compressed u32, decompressed u32, [checksum u32]}` (8–12 B), and a 9-byte footer `{n_frames u32, descriptor u8, magic 0x8F92EAB1}`.

Strata's 64 KiB frames are this design. Overhead is 8–12 B per 64 KiB (0.02%). **Steal (NEW, trivial):** wrap the payload seek table in a skippable frame so every frame is a valid standalone zstd stream and `zstd -d` can decode any of them. Debuggability and interop at zero cost. Also steal squashfs's stored-raw flag (2.4).

### 2.2 zstd dictionaries, `--patch-from`, long-distance matching

- Dictionaries (COVER/fastCover, ~110 KB) give 2–5× on sub-KiB messages; the gain shrinks for 64 KiB frames of dedup'd text. The README's optional per-column dictionary is therefore low value.
- `--patch-from=ref` uses the reference as a prefix with LDM and windows up to 2^31; patches are near bsdiff, but the decoder needs the reference resident and each frame depends on it.
- LDM (64-byte rolling hash, window up to 2 GB) finds far matches only *within one frame*.

**Avoid** both as the dedup mechanism: they create inter-frame dependency chains, which kill random access (B, E). CDC captures most of the same repetition without a chain (README: 122–206× vs 75× for 1 MiB pages). What dedup misses is **near-duplicates**: a system prompt differing by a date changes one chunk, which is stored again in full. See idea 7.

### 2.3 xz, BGZF, tabix/CSI

- **xz**: blocks, then an Index of varint `{unpadded, uncompressed}` per block, then a 12-byte footer with `backward_size`. Same tail-first trick as the postscript; varint sizes suggest ~2–4 B per block entry.
- **BGZF**: gzip members ≤ 64 KiB with a `BC` extra field holding `BSIZE−1`; a 28-byte empty EOF block marks completeness. **Virtual offset** = `block_start << 16 | offset_in_block`: one u64 addresses any uncompressed byte. **Steal:** chunk refs as `frame_id << 16 | offset_in_frame` (6 B for a ≤ 64 KiB frame) so a ref is one sortable integer that delta-encodes well. C; low.
- **tabix/CSI**: hierarchical bins (min bin 2^14, 5–6 levels; CSI makes shift and depth parameters), a record goes in the smallest bin containing it, plus a **linear index** per 16 kb window of the minimum virtual offset of any overlapping record, to bound scans for long records. Needed only for interval-overlap queries; per-block `min(start)`/`max(end)` zone maps already do this for spans. **No change.**

### 2.4 squashfs and EROFS (local source)

**squashfs** (`fs/squashfs/squashfs_fs.h`):
- Data blocks ≤ 1 MiB (`SQUASHFS_FILE_MAX_SIZE`, line 29); metadata in **8 KiB blocks** (line 19) with a 2-byte header whose bit 15 means "stored uncompressed" (`SQUASHFS_COMPRESSED_BIT`, line 106); data-block size words use bit 24 (line 113). Incompressible blocks are stored raw, flagged in the size word.
- **Fragments**: file tails are packed together into shared fragment blocks; entries are 16 B. The fragment table is **two-level**: a small u64-pointer array, one pointer per 8 KiB metadata block (512 entries; arithmetic at lines 138–152). Lookup = one RAM pointer + one 8 KiB read. `mksquashfs` also dedups identical blocks and stores zero blocks as size 0.

**EROFS** (`fs/erofs/erofs_fs.h`, `zmap.c`):
- Incompatible vs compatible feature bits (`erofs_fs.h:26–37`): the same required/optional split as Strata's feature bits.
- **Fixed-size physical clusters**: take as much input as compresses into a fixed output, so reads are aligned and bounded (squashfs does the opposite: fixed input, variable output).
- **Compact index.** The full per-cluster index is 8 B (`struct z_erofs_lcluster_index`, line 403, asserted at 455). The compact form packs **16 entries in 32 B with one 4-byte base address**: `encodebits = ((vcnt << amortizedshift) - sizeof(__le32)) * 8 / vcnt` = 14 bits per entry (`zmap.c:153`, `vcnt = 16` at 141). That is 2 B vs 8 B per cluster, with O(1) access (`pack = pos/32`, no varint scan).
- `ZTAILPACKING` inlines the last compressed cluster into the metadata block (one I/O saved); `FRAGMENTS` shares tails; `DEDUPE` shares identical pclusters.

**Steal:**
1. **16-entry base + packed-delta packs** for block and chunk tables. 3–4× smaller footer, zero-parse O(1) access. A, B, E; **REFINES** (README: fixed-width records).
2. **Close frames on compressed size, not raw size.** Real agent data compresses ~100–200×, so a 64 KiB raw frame may be a few hundred bytes to 8 KiB, and tiny S3 ranges waste a first-byte latency each. Close at 64 KiB raw *or* when a running estimate reaches ~32 KiB compressed, with a larger raw cap (256 KiB) for highly compressible data. C, E; **NEW** (README has the raw-size curve, not adaptivity).
3. Stored-raw flag per block/frame (the `encoding u16` covers it). Trivial.
4. Tail packing: for tiny files, put the last small frame next to the directory inside the speculative tail read. E; low.

### 2.5 zip and 7z

- **zip**: End of Central Directory (22 B + comment up to 64 KiB, so a backward scan) with Zip64 locators; entries duplicated in local headers, so a damaged directory can be rebuilt and an append needs only a new directory. **Avoid** the variable EOCD (Strata's fixed 64 B postscript is right); **steal** the duplicated per-segment header (1.1).
- **7z**: a fixed 32-byte front header `{NextHeaderOffset, NextHeaderSize, CRCs}` patched after the write; a compressible end header (`EncodedHeader`); **solid folders** (one stream over many files, whole-folder decode to read one) and a **file ordering by extension/name so similar files are adjacent**. Solid size is the same knob as frame size. The ordering is idea 7. Compressing the directory (3–5× on records, est.) only matters on cold S3 open and breaks O(1); compress cold tables (stats, key dictionaries) only. Low.

---

## 3. Dedup, VCS, backup

### 3.1 Git (gitformat-pack, gitformat-chunk, technical/pack-heuristics)

**Packfile.** Objects `{type+size varint, zlib data}`; `OFS_DELTA` stores a varint negative offset to the base (a few bytes vs a 20 B SHA). Delta body = *copy{offset,size}* / *insert literal* instructions. Chain depth ≤ `pack.depth` (default 50), candidates from a sliding window (`pack.window`, default 10) over objects **sorted by type, name-hash, then size descending**, so likely-similar objects are adjacent.

**`.idx` v2.** `\377tOc` header; **fanout[256] u32** (cumulative count by first SHA byte); sorted SHA list; **CRC32 of each object's packed bytes**; 4-byte offsets with the high bit meaning "see the 8-byte table"; pack and idx checksums. About 28 B per object. A lookup narrows to ~n/256 entries, then binary-searches (12 probes at 1M objects, mmap'd).

**Sidecars.** multi-pack-index, commit-graph and bitmaps share a **chunk TOC** framing: `{4-byte id, 8-byte offset}` rows, then chunks (`OIDF`, `OIDL`, `OOFF`, ...); unknown chunk IDs are skipped. Reachability bitmaps are EWAH bit-vectors in pack order, XOR-ed against an earlier bitmap.

**Maintenance.** `git repack --geometric=2` keeps pack sizes in a geometric progression (each at least 2× the combined size of the smaller ones). Pack reuse copies compressed bytes verbatim; the idx CRC32 verifies a copy without inflating.

**Steal:**
1. **Geometric compaction policy.** Merge when the smaller files sum to ≥ half the next. Each byte is rewritten O(log N) times; file count stays O(log N). D; no format change, low cost; **NEW** (README says how compaction copies, not when).
2. **Chunk-TOC framing** `{id u32, offset u64, len u64}` for index sections so new optional sections (trigram, bitmaps) need no version bump. Low.
3. **Similarity sort before frame packing (idea 7).** Git's `type, name-hash, size` order is why a 10-object delta window works. Strata analogue: tag each unique chunk with a stream key (column, role, tool name, hash of the app's system prompt) and pack frames **by stream key, then first reference**, so near-duplicates share one zstd window. C, D; est. 5–15% on near-duplicate data, unmeasured; testable in `bench/src/payload.rs`.
4. **Keep per-segment checksums in the directory** so a frame copy during compaction verifies without reading the frame. D; trivial.

### 3.2 restic, borg, casync, FastCDC, dedup indexes

**Parameters:**
- restic: Rabin 64-bit fingerprint, min 512 KiB / avg 1 MiB / max 8 MiB; pack files of ~16 MiB ending in a header of `{type, length, SHA-256}` per blob plus a trailing 4-byte header length; an in-RAM index of ~40–50 B per blob.
- borg: buzhash, defaults `19,23,21,4095` (min 512 KiB, avg 2 MiB, max 8 MiB).
- casync: buzhash avg 64 KiB (16–256 KiB); the `.caibx` index is `{end_offset, 32-byte id}` per chunk.
- **FastCDC** (Xia et al., ATC'16): gear hash, **cut-point skipping** (skip the first `min` bytes), and **normalised chunking** with a harder mask below the average and an easier one above it. That tightens the size distribution, so fewer chunks are forced to min or max (the cuts that misalign after an edit). 3–10× faster than Rabin.

**Against the README (gear, min 512 B, avg ~2 KiB, max 8 KiB):**
- **Normalised chunking (level 2)** instead of a single mask. C; small; README says only "gear hash".
- **Structure-aware cuts (NEW).** History is JSON arrays of messages, and the repeated prefix is *byte-identical* up to a `},{"role"` boundary. Cut at message boundaries and use CDC only inside oversized messages: a whole message is one dedup unit, boundaries re-align after any edit, and the chunk table shrinks (fewer, more meaningful chunks). C; est. 5–15% on dedup ratio and chunk-table size; low cost (a cheap scanner for the GenAI message layout; CDC remains the fallback for non-JSON text).
- **Per-file hash width.** A 64-bit chunk hash is safe within a file (n²/2^65 ≈ 7×10⁻⁹ at 500k chunks, est.), cutting the dedup table from ~24 to ~16 B per chunk; keep 128 bits for any cross-file store.
- **Cross-file store (README item 8) does not scale at 2 KiB chunks.** 24–50 B per unique chunk in RAM is 1.2–2.5% of the unique data; fine per file (≤ 500k chunks), but 1 TB of unique data is 500M chunks, or 12–25 GB. Restic and borg use 1–2 MiB for this reason. Use a coarse global unit (frame or super-chunk named by the hash of its chunk-hash list), **sparse indexing** (Lillibridge et al., FAST'09: index 1/2^k sampled hashes, dedup against the most similar segments), or the **DDFS summary vector** (Zhu et al., FAST'08: a Bloom filter of all fingerprints plus locality-preserving containers, ~99% of index lookups avoided). D, E.

### 3.3 rsync rolling checksum

A weak 32-bit rolling sum (`a=Σx`, `b=Σ(n−i)x`) plus a strong hash finds old blocks inside a new file. It needs a reference file, which the append-only payload layer lacks; the gear hash is the same idea turned into a content-defined cut. **Avoid.**

---

## 4. HTTP / object-store-native single files

### 4.1 PMTiles v3 (protomaps spec v3)

- **127-byte fixed header**: magic, version, `{offset,len}` of root directory, metadata, leaf directories and tile data; entry counts; clustered flag; compression and tile type. The root directory is **guaranteed to fit in the first 16,384 bytes**, so one request gets header + root.
- **Directory entries** are sorted by Hilbert tile ID and serialised **columnar**: count, then all id-deltas, then all `run_length`s, then all `length`s, then all `offset`s, as varints, then gzip. `offset 0` means "contiguous with the previous entry". `run_length > 1` is dedup of identical tiles; `run_length == 0` marks a pointer to a leaf directory. Leaves start at ~4,096 entries, growing until the root fits in 16 KiB.
- Cost: header+root in 1 request, leaf in 1, tile in 1; at most 3 dependent reads for any tile.

**Steal:**
1. **A size contract for the tail.** The hot tables (postscript, column directory, schema, key table) go *last* and total ≤ 64–128 KiB; the bulky lazy sections (bloom, trigram) go before them. One speculative suffix read then opens the file and often answers the key lookup. B, E; **REFINES**: README orders the sections but sets no size contract.
2. **Columnar-delta directory with "contiguous" offsets**: matches idea 8. If a column's blocks are adjacent, offsets are free. Ordering segments by `(column, row range)` makes offsets prefix sums but scatters a point lookup across columns; keep README's `(row range, column)` order and delta-pack.
3. **Avoid two-level directories** with a sentinel entry until blocks per file exceed ~10⁴. At 128 KiB narrow blocks and 256 MB files that is ~2,000 blocks per column at most, so a flat table in the tail suffices.

### 4.2 Cloud-Optimized GeoTIFF, COPC, FlatGeobuf

- **COG**: IFDs and tile offset/byte-count arrays first, overviews coarsest first; GDAL ingests the first 16 KiB (`GDAL_INGESTED_BYTES_AT_OPEN`). A "ghost area" declares layout guarantees (`BLOCK_LEADER=SIZE_AS_UINT4`, `BLOCK_TRAILER=LAST_4_BYTES_REPEATED`): each tile is preceded by its u32 size and followed by a copy of its last 4 bytes, so scans walk blocks without the directory and torn writes are detectable. **Steal:** a u32 length leader per block. A, E; trivial, small gain.
- **COPC**: a fixed-offset info record, a hierarchy loaded in pages of 32 B entries `{level,x,y,z, offset u64, byte_size i32, point_count i32}`; `point_count == -1` means "pointer to a child page". Same sentinel idea as PMTiles. Only relevant for a cross-file index. **Avoid for now.**
- **FlatGeobuf**: a **packed Hilbert R-tree** up front: 40 B nodes `{4×f64 bbox, offset u64}`, fan-out 16, levels stored root-first with an **implicit layout** (level boundaries computed from `(count, node_size)`; no child pointers). One dependent read per level, coalesced. **Steal:** the implicit pointer-free layout only for a cross-file manifest of ≥ 10⁵ entries. In a file with ~2,000 blocks a flat sorted array in the tail beats any tree. **Avoid** 40 B per entry; for random 16 B trace IDs, LevelDB-style **shortest separator keys** are ~3 B (2,000 blocks: ~14 KB vs 32 KB). B; marginal.

### 4.3 Zarr v3 sharding (`codecs/sharding-indexed`)

A shard is N inner chunks plus an index of exactly `N × 16 B` `{offset u64, nbytes u64}` plus a crc32c, at the end (or the start via `index_location`). Missing chunks are `{2^64−1, 2^64−1}`. The index is dense and fixed-width, so chunk *i* is at `i*16`. The reader issues **one suffix range read** (`Range: bytes=-(16N+4)`), which needs no `HEAD` and no known file length.

**Steal:**
- **Suffix reads (`Range: bytes=-65536`) to open a file in one round trip** instead of `HEAD` + `GET`. Locally moot if the catalog stores sizes, but any `object_store` reader without a catalog entry pays it. E; **high value, trivial; NEW as a stated rule.**
- A dense fixed-width index with a missing-chunk sentinel suits sparse shredded-map sub-columns (absent in a block).

### 4.4 HDF5, linearized PDF, SQLite-over-HTTP

- **HDF5** chooses the chunk index by dataset shape (single chunk: none; implicit: arithmetic; fixed array; extensible array; B-tree v2), and for cloud use offers **paged aggregation** of all metadata into fixed pages (e.g. 4 MB). **Steal:** special-case tiny files (a 5-trace file needs no directory beyond the postscript); keep all metadata at one end.
- **Linearized PDF**: hint tables store per-page `{count, length, offset}` as *deltas from the minimum with declared bit widths*; xref streams declare field widths (`/W [1 2 1]`). This is FoR with explicit widths in the header, which supports idea 8. Incremental updates *append* a new xref with `/Prev`: the opposite of an immutable-file model; **avoid**.
- **sql.js-httpvfs**: a VFS turns page reads into range requests, with a **virtual read head** that grows request size on sequential access, and DB page sizes raised to 32–64 KiB. Each B-tree level is one dependent round trip (15–50 ms on S3). It confirms README principle 4 (≤ 3 dependent reads) and suggests **adaptive coalescing**: grow the merge window when a scan touches consecutive blocks (README: fixed 1 MiB gap, 16 MiB cap). E; low.

---

## 5. Zero-copy and succinct structures

### 5.1 FlatBuffers vs Cap'n Proto

FlatBuffers: root offset first, tables with per-table **vtables** (2 B per field slot), u32 offsets, vectors of fixed structs at zero overhead, forward/backward compatible by field ID. Cap'n Proto: 8-byte-aligned words, 64-bit pointers per struct, fixed field offsets with no vtable, traversal limits; larger on the wire for small messages; its packing pass destroys zero-copy. A footer dominated by arrays of fixed records needs neither: use `#[repr(C)]` little-endian record arrays with a small typed header (as README does), and FlatBuffers only for the schema and extension blobs. Confirms README.

### 5.2 Elias–Fano and FST

- **Elias–Fano**: *n* sorted integers in universe *u* cost `n(2 + ⌈log2(u/n)⌉)` bits with O(1) `select`; 1M offsets in 1 GB ≈ 12 bits each vs 64. At ≤ 10⁵ entries, EROFS-style packs (idea 8) are simpler and as fast. Use it only for a large cross-file manifest.
- **FST** (BurntSushi `fst`, Lucene): a minimal acyclic DFA over sorted keys with a u64 output; shares prefixes and suffixes; 2–4× smaller than a sorted array for names with shared structure (`gen_ai.request.model`, `llm.token_count.*`), nothing for random IDs. **Steal:** keep string dictionaries and the shredded-map key dictionary **sorted and order-preserving**, so `LIKE 'gpt-%'` and range predicates become integer compares on codes. A; low-medium; README does not say dictionaries are sorted.

### 5.3 Filters: bloom, xor, binary fuse, ribbon

Space per key; the lower bound is `log2(1/ε)` bits:

| Filter | Bits/key | ε ≈ 0.4% | ε ≈ 1.5e-5 | Notes |
|---|---|---|---|---|
| Bloom | `1.44·log2(1/ε)` | ~11.5 | ~23 | scattered probes |
| Split-block (Parquet SBBF) | ~1.2–1.3× Bloom | 12–14 | 26–29 | one cache line per probe |
| Xor | `1.23·log2(1/ε)` | 9.84 | ~19.7 | static, 3 probes |
| Binary fuse (3-/4-wise) | `~1.125 / 1.075·log2(1/ε)` | ~9.0 / 8.6 | ~18 / 17.2 | static, fast build |
| Ribbon (RocksDB) | `~1.005–1.08·log2(1/ε)` | ~8.1–8.7 | ~16.5 | static, slower build |

(Graf & Lemire, Xor Filters 2020 and Binary Fuse Filters 2022; Dillinger & Walzer, Ribbon 2021; figures rounded.)

- **Static filters instead of SBBF (NEW).** Strata builds its bloom section at file close from the complete key set, which is the static-filter case. For 1M trace IDs, SBBF at ~1% ≈ 1.3 MB vs binary fuse 8-bit (ε 0.39%) ≈ 1.1 MB: 20–35% smaller at a lower FPR (est.). Caveat: probes are scattered, so over S3 read the whole filter or issue 3 tiny parallel ranges. B, E; medium; low cost (`xorf` crate).
- **Filters in the catalog (NEW).** Store each file's filter in `catalog.db` as well as in the file, so a trace lookup prunes non-matching files *before any S3 request* (Iceberg Puffin files keep sketches outside data files for the same reason). A trace ID is random, so zone maps on it prune nothing across time-sliced files. B, E; high: removes a round trip per non-matching file.
- **Bit-sliced trigram signatures (NEW)** (BitFunnel, Goodwin et al., SIGIR'17). README stores a 2 KiB filter per 64 KiB frame; testing a needle then reads every frame's filter (~20 MB at F = 10,000 frames for a 650 MB raw file), or many dependent reads. Transpose the layout: **one row per hash bit position, one column per frame.** A needle with *t* trigrams and *k* hashes reads *t·k* rows of `F/8` B each and ANDs them. At F = 10,000, a row is 1.25 KB; a 10-trigram needle at k = 3 reads 30 rows ≈ **37 KB in one parallel wave** instead of ~20 MB. On local disk the saving is page-cache touches; on S3 it is 100–500× fewer bytes (est.). C, E; high; medium cost.
- **Presence bitmasks per block for dictionary columns ≤ 64 codes**: an 8 B `u64` per block per column. README's zone maps hold *code ranges*, which prune nothing for `status ∈ {ok, error, unset}`. Honest limit: with files sorted by trace ID, a scattered 3% error rate leaves ~245 errors in every 8192-row block, so no pruning; it helps only for rare or trace-clustered values (a rare model, a tenant). A; low.

### 5.4 PGM / learned index

A piecewise-linear key→position model with error bound ε, 16–24 B per segment. For uniform 128-bit trace IDs the model is one segment (`block ≈ prefix64/2^64 × n_blocks`, ε ≈ 1 block by order statistics). But the directory already holds each block's key min/max, so a binary search over ≤ 2,000 entries in the already-fetched tail costs the same, and saves no I/O. **Avoid in-file**; consider it only for a cross-file key→file map at ≥ 10⁸ keys.

---

## 6. Ranked ideas

Impact is relative to the README's *Expected gains*. **NEW** = not in the README design; **REFINES** = the README names the topic but not the mechanism.

| # | Idea | From | Load | Expected impact | Cost | Status |
|---|---|---|---|---|---|---|
| 1 | **Bit-sliced trigram signatures** (row = hash bit, column = frame) instead of per-frame filters | BitFunnel | C, E | ~37 KB vs ~20 MB read per needle at 10k frames (est.); same filter size | Medium | **NEW** |
| 2 | **Per-file static filters (binary fuse/xor, 8–16 bit) also stored in the catalog**, pruning files before any S3 request | Xor/fuse, Iceberg Puffin | B, E | one round trip saved per non-matching file; ~25% smaller than SBBF | Low-medium | **NEW** |
| 3 | **Tail-sharing row reference lists** `(parent, shared_prefix_len) + new_ids`, with a full-list keyframe every ~16 steps | video GOP, git `OFS_DELTA` | C, D | ref-list column O(n²) → O(n); est. 5–20× smaller on agent data | Low | **NEW** |
| 4 | **Message-boundary chunk cuts for JSON conversations** (CDC only inside big messages) plus FastCDC normalised chunking | FastCDC, message-aligned prefixes | C | est. 5–15% on dedup ratio; fewer, steadier chunks | Low | **NEW** |
| 5 | **Tail size contract and suffix-range open**: hot tables last and ≤ 64–128 KiB; open with `Range: bytes=-N`, no `HEAD` | PMTiles 16 KiB root, Zarr suffix index | B, E | 3 → 2 dependent reads on a cold lookup | Free | **REFINES** |
| 6 | **Geometric compaction policy**: merge when smaller files sum to ≥ half the next | `git repack --geometric` | D | bounded write amplification (~log2 N) and file count | Low | **NEW** |
| 7 | **Similarity-ordered frame packing** by stream key before first reference | git name-hash sort, 7z ordering | C, D | est. 5–15% smaller payloads on near-duplicates; unmeasured | Low | **NEW** |
| 8 | **16-entry base + packed-delta tables** for block and chunk tables | EROFS `zmap.c:153`, PDF hint tables | A, B, E | directory 3–4× smaller (est. 128 KB → ~35 KB); O(1) access | Low-medium | **REFINES** |
| 9 | **Self-describing segment headers** so the footer can be rebuilt by scan and segments concatenated | fragmented MP4, zip | D | crash salvage; +16 B per segment | Low | **NEW** |
| 10 | **Adaptive frame closure on compressed size** (with raw cap) for uniform S3 request sizes | EROFS fixed-output clusters | C, E | fewer sub-KiB range reads at ~100× compression | Low | **NEW** |
| 11 | **Sorted order-preserving dictionaries (FST for key dictionaries) and presence bitmasks for ≤ 64-code columns** | Lucene FST, git bitmaps | A | `LIKE 'x%'` as integer compare; pruning only for rare or clustered values | Low | **NEW** |
| 12 | **Skippable-frame wrapping of the payload seek table** (stock `zstd -d` decodes any frame) and a stored-raw flag | zstd seekable, squashfs bit 24 | C, D | debuggability and interop; no wasted CPU on incompressible blocks | Trivial | **NEW** |

**Rejected after evaluation:** zstd `--patch-from`/LDM chains (kill random access); faststart-style head indexes; FlatBuffers/Cap'n Proto for the hot directory; PGM or Elias–Fano inside a file (≤ 2,000 blocks); two-level directories before ~10⁴ blocks per file; tabix binning (time zone maps already bound overlap queries); rsync-style diffing (needs a reference file); FLAC-style bisection with resync on S3 (dependent reads).

**Flag for the README.** Payload item 8, the cross-file chunk store at ~2 KiB average chunks, does not scale its index: 24–50 B per chunk is 1.2–2.5% of unique data in RAM. If pursued, use a coarse global unit, sparse indexing, or a summary-vector filter (3.2).
