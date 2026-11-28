# Statam — Cross-Domain Format Analysis & Prior Art

**Date:** 2026-11-26  
**Status:** Stage 1 deliverable (`crates/statam` / `cetra-statam`)  
**Builds on:** `docs/research/file-format/{README,parquet,vortex,other-formats-a,other-formats-b,prototype-plan}.md`

This document extends the repository's prior file-format research without repeating basics already covered there. It focuses on **gaps** relative to Statam's charter—specifically game-engine streaming containers, trained dictionaries (and their GDPR hazard), LLM-domain structural chunking, Meta Nimble, Lance v2.x, FastLanes, and asymmetric write/read tiering—and concludes with a concrete **Steal vs Reject** synthesis matrix mapped to Cetra's three tables (`spans`, `traces`, `scores`), write profiles (`L0Fast` vs `L1Compact`), and four adversarial workloads (`W1`–`W4`).

---

## 1. Video Game Asset & Streaming Containers

Game engines solve a surprisingly close sibling of Cetra's S3 + `ReadCache` problem: **latency-sensitive random access into tens of gigabytes of mixed tiny metadata and huge repetitive assets over block-granularity storage (NVMe / console SSDs / CDN range reads)**, with zero parse stalls on open.

### 1.1 Unreal Engine 5 IoStore (`.utoc` / `.ucas`)

UE5 replaced monolithic `.pak` archives (string-path lookup, per-file headers) with **IoStore**, splitting containers into two immutable files:
- **`.utoc` (Table of Contents):** Fixed-width header, chunk-ID table (`FIoChunkId`: 8 B GUID + 1 B type + index), flat chunk offset/length array, **Compression Block Table** (`offset`, `compressed_size`, `raw_size`, `codec_index`), and encryption GUID/flags. Mounted once in memory with zero tree parsing via a perfect hash or sorted array.
- **`.ucas` (Content-Addressable Store):** Contiguous stream of uniformly sized **compression blocks** (default **64 KiB** uncompressed, compressed with Oodle Kraken or LZ4/Zlib). Multiple small assets pack into a single compression block; large assets span consecutive blocks.

**What translates directly to Statam:**
1. **Strict TOC / Bulk Separation for L1 Chunk Packs (`.stpk`):** Separating content-addressed immutable payload packs from columnar data files mirrors `.ucas`/`.utoc`. Packing deduplicated LLM chunks into fixed-target **64 KiB compressed frames** gives bounded random-access read amplification (13–21 KiB per lookup in our SWE-agent measurements) without sacrificing cross-chunk compression.
2. **Zero-Parse Fixed-Width Tables:** `.utoc` indexes chunks by dense integer IDs into flat `#[repr(C)]`-like arrays rather than nested Thrift/Protobuf/FlatBuffer trees. In Statam, fixed-width `ColumnDirEntry`, `BlockEntry`, `FrameEntry`, and `ChunkEntry` arrays give O(1) slice access directly over the tail buffer.
3. **Container-Level Encryption Metadata Slot:** `.utoc` carries encryption flags and key GUID in the header so encrypted and unencrypted containers share the exact same layout. Statam adopts this pattern for its optional AEAD envelope slot.

### 1.2 EA Frostbite CAS, DirectStorage & PlayStation Kraken Streaming

EA's Frostbite **Content Access System (CAS)** stores immutable deduplicated chunks addressed by cryptographic hash (SHA-1) across catalog/bundle files (`cas.cat`/`.casx`), each block carrying its own compact compressed/raw header. Microsoft **DirectStorage (1.1–1.4)** and PS5 **Kraken** add two critical I/O principles:
- **Transfer-Unit Alignment:** Compressed blocks are sized and aligned to match the underlying storage/cache transfer quantum (64 KiB NVMe queues / 256 KiB–1 MiB CDN/S3 blocks), avoiding pathological reads where a single 32 KiB compressed block straddles a cache-block boundary and triggers **2× transfer amplification**.
- **Pre-Conditioning Before Entropy Coding:** DirectStorage + GACL applies cheap domain-specific transforms (byte shuffling / channel separation) *before* Zstd/GDeflate so the entropy coder sees uniform streams.

**What translates directly to Statam:**
1. **1 MiB `ReadCache` Boundary Alignment:** Cetra's `crates/storage/src/read_cache.rs` fetches and caches S3 files in strict **`BLOCK = 1 MiB`** (`1 << 20`) aligned chunks with per-block CRC-32C.
   - **Hot tail ≤ 1 MiB:** Packing Postscript + Schema + Column Directory + Zone Maps + Sort-Key Index + `trace_id` Bloom + small file-level dictionaries into the final ≤ 1 MiB means cold file open + prune is **1 S3 GET and 1 `ReadCache` block**.
   - **Pack frame boundary guard:** In `.stpk`, if appending the next compressed frame (~10–32 KiB) would straddle a 1 MiB `ReadCache` boundary while `< 16 KiB` remains in the current 1 MiB slot, pad to the 1 MiB boundary (< 1.5% worst-case overhead) so reading one frame never fetches 2 MiB from S3.
   - **Query-aware I/O coalescing:** Like DirectStorage's request batching (and avoiding Vortex's blind 1 MiB coalescing across pruned segments), Statam distinguishes **Point** reads (never bridge pruned gaps > 32 KiB unless both ends lie in the same 1 MiB `ReadCache` block) from **Scan** reads (coalesce up to 1 MiB gap / 8 MiB max range).
2. **Type-Specific Conditioning:** Instead of feeding raw structs or IEEE-754 floats directly into Zstd (like `pq_cetra`), Statam applies FastLanes Delta/FOR, ALP, FSST, and flat-map shredding first—often eliminating Zstd altogether on narrow columns.

### 1.3 Valve Source 2 VPK (`PreloadBytes` Inline Tail-Packing)

Valve's split VPK format (`*_dir.vpk` + `*_000.vpk`) embeds a `PreloadBytes: u16` payload directly inside each directory entry for tiny assets (material scripts, headers). Reading the directory immediately satisfies tiny asset fetches with **zero secondary seeks** to the archive packs.

**What translates directly to Statam:**
- **Inline Small-Payload Threshold (`INLINE_PAYLOAD_THRESHOLD = 256 B`):** Short `input`/`output` values (e.g., `"OK"`, short tool args, `input_preview`/`output_preview`, short classifier outputs) are stored **inline** inside the data file's narrow/wide-row block rather than indirected through chunk hashing and `.stpk` packs. That eliminates pack lookups, hash-table bloat, and extra S3 GETs for short values, while routing values `> 256 B` through chunk deduplication.

### 1.4 Trained Compression Dictionaries (Zstd Dict / Oodle) — and the GDPR Hazard

Game archives train small shared dictionaries (32–112 KiB) over thousands of small structured blobs (JSON/XML configs, shader headers), recovering 1.5–2× compression ratio when random access forces small frames. In Cetra's prior `payload.rs` benchmark, a 110 KiB Zstd dictionary improved 64 KiB raw blocks from 4.9× → 7.3× (synthetic) and 8.4× → 12.6× (SWE-agent), at ~2× encode cost.

**Critical Statam findings on trained dicts:**
1. **Deduplication beats trained dicts on repeated text, and costs less CPU:** Once repeated prompts/messages are deduplicated by chunk hash, unique chunks packed into 64 KiB Zstd-3 frames already achieve 10.0× (synthetic) and 122× (SWE-agent) *without* dictionary training overhead.
2. **Where trained dicts still help:** High-cardinality short structured JSON blobs (`events`, `attributes` residual, short unique `input`/`output` turns) in `L1Compact` when a pack contains many small unique chunks (`< 2 KiB` avg) and Zstd-3 ratio is weak.
3. **CRITICAL GDPR Erasure Hazard:** A Zstd dictionary trained on raw user payloads embeds literal byte substrings from the training samples inside the dictionary blob itself. If user `U` is later GDPR-erased from `(project, day)`, keeping the old trained dictionary would leave erased user substrings on disk/S3!
   - **Statam rule:** Trained dictionaries are **opt-in / `L1Compact`-only** (`dict_id` field reserved in `.stpk` `FrameEntry` and `DictSection`), **disabled in `L0Fast`**, and whenever GDPR rewrite touches a `.stpk` pack containing a `DictSection`, the old dictionary is **unconditionally discarded** (and any re-compressed frames use plain Zstd-3 or a fresh dictionary trained exclusively on surviving chunks).

---

## 2. Video & Media Containers (Deepening Beyond `other-formats-a.md`)

`other-formats-a.md` already cataloged ISOBMFF (`stbl`/`moof`/`mfra`), Matroska (`Cues`), FLAC (`SEEKTABLE`), and GOP I/P-frames. Two concrete mechanics translate cleanly to Statam:

1. **ISOBMFF `sidx` / Fast-Start Single-Fetch Seek Index:** Fragmented MP4 places a compact `sidx` (Segment Index) box mapping time/byte offsets to independently decodable media fragments so DASH/HLS players seek in **1 HTTP Range request** without scanning. Statam's **Hot Tail** acts as `sidx` + `moov`: every Row Group and 4,096-row mini-block has a fixed-width entry with exact byte offset, stored length, row range, and inline sort-key bounds, enabling exact O(log N) binary search with zero extra RTT.
2. **Video GOP Keyframes & Long-Term References → Tail-Sharing Chunk-Ref Lists:** In multi-turn coding agents and chat sessions (`W1`, `W2`), span `k+1`'s `input` is span `k`'s entire message array plus 1–2 appended messages. Storing raw chunk-ID lists per row is $O(T^2)$ for a $T$-turn trajectory (e.g., 50 turns = 1,275 chunk IDs). Modeling ref pages like a video GOP:
   - Row `r` encodes `(back: u8, keep_prefix: u16, new_chunks: [u32])`, referencing a prior row `r - back` (`back ∈ 1..=32`) in the same ref page and reusing its first `keep_prefix` chunk IDs.
   - To guarantee bounded O(1) random-access reconstruction (just like GOP keyframe spacing!), **reference chain depth is capped at `MAX_REF_CHAIN_DEPTH = 15`**; every 16th chained row emits a full keyframe ref list. This collapses $O(T^2)$ ref metadata to $O(T)$ while keeping single-row decode strictly bounded.

---

## 3. Compression, Dedup & Read-Only Filesystems

### 3.1 Zstd Seekable Format, `--long`, SquashFS & EROFS

- **Zstd Seekable Format:** Independent frames + trailing seek table (`0x184D2A5E` skippable frame) prove that frame-level random access with Zstd incurs negligible overhead (< 0.1%) when frames are ≥ 16–64 KiB. Statam's `.stpk` adopts the exact same principle natively (with CRC-32C per frame and zero-parse fixed-width `FrameEntry` table).
- **Zstd `--long` (LDM):** Rejected for storage frames because a multi-megabyte back-reference window destroys random access (requires decompressing megabytes of prefix for one point read). Content-addressed deduplication achieves *infinite* window distance within `(project, day)` while keeping point-read frames small (64 KiB raw / ~16–32 KiB compressed).
- **SquashFS Fragments & EROFS `ztailpacking`:** Both show that small payloads must be coalesced/inlined rather than given dedicated compressed blocks, and that an uncompressed pass-through bit (`Codec::None`) is mandatory so incompressible data (`W4`) suffers 0% bloat and memcpy-speed reads.

### 3.2 Structural LLM Message-Array Chunking vs Byte-Level FastCDC

`other-formats-a.md` (§3.2) noted Gear-hash FastCDC and briefly mentioned message-boundary cuts. For Cetra's actual `W1`/`W2` data, **structural chunking is a major breakthrough**:

In OTel GenAI spans (`spans.input`, `spans.system_instructions`, `spans.tool_definitions`), `input` is overwhelmingly a JSON array of messages:
```json
[{"role":"user","parts":[...]},{"role":"assistant","parts":[...]},{"role":"tool","parts":[...]}]
```
When turn $k+1$ appends two messages to turn $k$'s array:
- **Byte-level FastCDC (Gear hash):** Hashes every single byte sequentially (~400 MB/s single-core), and may cut *inside* a message depending on rolling-window phase near the array opening/closing brackets (`[` / `,` / `]`), creating spurious boundary misses when a trailing `]` becomes `,`.
- **Structural Top-Level JSON Array Scanner (Statam's primary chunker for `[` payloads):**
  - A lightweight, zero-allocation single-pass state machine tracks only top-level bracket depth (`depth == 1`), JSON string quotes (`"`), and backslash escapes (`\`), cutting cleanly **between top-level array elements** (inclusive of leading `[` or `,` separator, with the final `]` kept with the last element or split as a tiny tail only when needed — wait: keeping the trailing `]` separate when the last element exceeds `MIN_CHUNK = 256 B` ensures that when turn `k+1` replaces `]` with `,{"role":...}]`, **100% of turn `k`'s final message chunk hashes identically to turn `k`!**).
  - **Tiny-message coalescing (`MIN_CHUNK = 256 B`):** Consecutive tiny elements (e.g., short 40 B `"ok"` messages) accumulate until `>= 256 B` before cutting, guaranteeing the **exact boundary search invariant**: *every non-last chunk has length `>= 256 B`, so any search needle of length `<= 64 B` can span at most 2 adjacent chunks (never 3+).*
  - **Oversized-message fallback (`MAX_CHUNK = 8 KiB`):** Any single top-level JSON element larger than `8 KiB` (e.g., a 200 KiB pasted source file or huge tool output) is sub-chunked internally with **FastCDC (Gear hash, min 512 B, avg 2 KiB, max 8 KiB)** so shared file slices inside large tool outputs *also* deduplicate!
  - **Non-JSON / plain-text payloads:** Fall straight through to FastCDC (Gear hash).
  - **Adversarial / Incompressible Bypass (`W4`):** If a payload column's values are high-entropy/unique (no chunk repeats in the first sample window and values don't compress), the writer bypasses CDC hashing and stores frames directly with `Codec::None` (raw pass-through) — zero hashing CPU wasted on random `W4` blobs!

### 3.3 Per-`(project, day)` Scoped Content-Addressed Packs vs Global Dedup

`other-formats-a.md` warned that a global cross-day dedup index at 2 KiB chunks needs 12–25 GB RAM per TB and complex garbage collection. Statam's settled charter solves this cleanly by scoping `.stpk` packs strictly to **`(project_id, day)`**:
- **Bounded RAM:** One day of one project has far fewer unique chunks (typically 10k–500k chunks = 0.5–20 MiB hash index during compaction).
- **Cross-file dedup within a day:** Captures repeated system prompts, tool schemas, RAG docs, and multi-turn chat histories across separate L0 flushes and L1 files in the same day (`W2`).
- **O(1) retention:** Expiring a day deletes the `(project, day)` directory/prefix (data files + packs) with zero ref-counting.
- **Cryptographic chunk IDs (`BLAKE3-128`):** Already in `Cargo.lock` (`blake3 1.8.7`, AVX2 multi-GB/s). Unlike non-cryptographic `xxh3-128`, `BLAKE3-128` resists adversarial collision attacks (`W4` / multi-tenant safety) so two distinct chunks will never collide and silently corrupt payloads or leak another user's text.
- **Airtight GDPR Erasure:** Rewriting affected `.statam` files in `(project, day)` collects the exact live `(pack_id, chunk_id)` bitmap; 100%-live frames copy byte-for-byte (zero recompression), mixed frames decompress and drop erased chunks before recompressing, dead frames drop completely, any old trained dict is discarded, and the old `.stpk` pack is deleted atomically. **0 bytes of erased payloads survive.**

---

## 4. Modern Columnar & Analytical Formats

### 4.1 Parquet vs Vortex: Lessons Carried Forward

From `parquet.md`, `vortex.md`, and `README.md`:
- **Parquet's structural bottlenecks:** O(R×C) Thrift footer parse with no checksum; scattered per-RG blooms (`AfterRowGroup`) causing extra RTTs; PageIndex fetched for all columns; 1 MiB Zstd pages defeat point lookups while 64 KiB pages (`pq_tuned`) bloat size by 1.9× on repetitive text; maps store rep/def levels with one combined min/max across all keys; zero substring search index.
- **Vortex's strengths & failure mode:** Cascading BtrBlocks encodings + FastLanes/ALP/FSST + compute-on-compressed gave 2–7× faster narrow scans, **but** fixed 8,192-row variable-width segments + no sub-segment reads + blind coalescing of pruned segments caused **66–80 MB point lookups**, and it lacks `FixedSizeBinary`, `Map` key pushdown, key indexes, and stable blooms.

### 4.2 Lance v2 (Mini-Block vs Full-Zip & Decoupled Granularity)

Lance v2 (2.0–2.2) decouples physical chunking from rigid monolithic row groups:
- **Mini-Block (4–8 KiB compressed):** For narrow fixed-width/low-cardinality columns where decoding a tiny block is cheap (~µs) and random access jumps straight to the target mini-block.
- **Full-Zip:** Zips variable-width/wide row components side-by-side so fetching row `i` is an O(1) offset read in 1–2 RTTs without decoding unrelated columns or huge pages.

**How Statam adapts Lance v2 for Cetra's 1 MiB `ReadCache`:**
Statam uses **16,384-row PAX Row Groups** composed of **4,096-row Mini-Blocks**, ordered into two contiguous sub-regions per Row Group:
1. **Core Narrow Sub-Region** (scalar/filter/sort/dict/shredded-hot-key columns, ~150–480 KiB per 16k RG): column-major within the RG so narrow analytical scans read only projected columns, while point lookups fetch the entire Core Sub-Region in **1 contiguous read (≤ 1 `ReadCache` block)**.
2. **Wide-Row Sub-Region** (`input_preview`, `output_preview`, `request_params`, `events`, `links`, residual maps, payload chunk-ref lists): contiguous per 4,096-row stripe at the tail of the RG so narrow scans never touch it, while full `SELECT *` point lookups fetch Core + target Wide-Row stripe in **1 coalesced GET (≤ 1 MiB)**!
3. **External Payload Layer** (`.stpk` packs in `L1Compact`, or dedicated payload section in `L0Fast`): completely decoupled from narrow row-group sizing.

### 4.3 Meta Nimble (Flat-Map Shredding & Stream Encodings)

Meta's Nimble (formerly Alpha, integrated with Velox) solves wide dynamic `MAP<K, V>` tables where traditional columnar map encoding forces full-map deserialization on `map['k']` lookups:
- **Flat-Map Encoding:** Breaks hot map keys into individualized per-key value streams, paired with a per-key **`in_map` presence bitmap** and **`null` bitmap**. A query referencing `metadata['customer_id']` reads *only* that key's tiny stream + presence bitmap (and gets full zone-map pruning and dictionary/FSST encoding on that key!).
- **Encoding Tree & Stream Decoupling:** Decouples physical streams from lightweight encodings (`MainlyConstant`, `SparseBool`, `FixedBitWidth`, `RLE`, `Dict`), allowing `StreamSlicer` to slice row ranges directly.

**How Statam adapts Nimble (`ShreddedMap` in `L1Compact`):**
- Keys appearing in **≥ 5% of rows** (capped at `MAX_SHREDDED_KEYS = 64` per map column so `W4` high-cardinality 5,000-key maps shred **0** keys and never explode metadata!) are promoted to first-class narrow sub-columns with their own encoding (`Const`/`Rle`/`Dict`/`Fsst`), zone maps, and `in_map` presence bitmap.
- Rare keys, null-valued entries for promoted keys (or represented cleanly via 2-state `in_map` + `value_null` bitmaps!), and duplicate keys live in a compact **Residual Map** column (with file-level key dictionary).
- Each shredded key records a `residual_count: u32` in metadata: if `residual_count == 0` (the 99.9% case), `ScanSpec.map_keys` reads **ONLY** the shredded sub-column and **never touches the residual map**!

### 4.4 FastLanes (1024-Value Unified Transposed SIMD Layout)

FastLanes (Afroozeh & Boncz, VLDB 2023) achieves >100 billion ints/sec decompression across AVX2, AVX-512, and NEON using a **1024-value virtual SIMD register (`MM1024`)**:
- **Interleaved Bit-Packing:** Packs 1024 `u32` or `u64` values at bit-width `W ∈ 0..=64` across interleaved SIMD lanes (8 lanes × 4×32-bit for AVX2 / 4 lanes × 64-bit for AVX2), eliminating cross-lane bit-shift dependencies.
- **Parallel Delta (`04261537` Transposed Order):** Classic delta decoding (`x[i] = x[i-1] + d[i]`) has a serial loop-carried dependency that stalls SIMD pipelines. FastLanes seeds independent lane bases and permutes tiles (`04261537` order) so all SIMD lanes execute prefix-sum in parallel with zero cross-lane stalls, fitting comfortably inside L1 cache (8 KiB for 1024 × `i64`).
- **Statam implementation:** `crates/statam/src/simd/bitpack.rs` implements FastLanes-1024 bit-pack/unpack + parallel delta/FOR with explicit `unsafe` AVX2 kernels (runtime-detected via `is_x86_feature_detected!("avx2")`), NEON kernels on `aarch64`, and a 100% safe portable scalar fallback verified bit-for-bit against SIMD and under `cargo miri test`.

### 4.5 DuckDB & ClickHouse Techniques (ALP, FSST, Trigram Skip Indexes)

- **ALP (SIGMOD 2024, CWI / DuckDB):** Compresses IEEE-754 `f64` values that originated as decimals (exactly Cetra's `cost_input`, `cost_output`, `cost_total`, `value_num`!) by finding `(exponent e, factor f)` via lightweight sampling such that `round(x * 10^e / 10^f)` converts losslessly back to exact `f64` bits (`to_bits()` equality!). The scaled integers are FastLanes FOR-bitpacked, while any non-conforming values (`NaN` with exact payload bits, `±inf`, `-0.0`, high-precision scientific floats) go into a compact `(index: u16, raw_bits: u64)` exception patch list. Bit-exact round-trip guaranteed on every IEEE-754 bit pattern, 2–5× smaller and 5–10× faster to decode than Zstd on `F64`!
- **FSST (VLDB 2020, CWI / DuckDB):** Builds a ≤ 255-entry static symbol table of frequent 1–8 byte substrings (plus escape byte `0xFF`), compressing short/medium high-cardinality strings (`name`, `user_id`, `session_id`, `input_preview`, `output_preview`, `request_params`, `score_id`, shredded map values) by 2–3× while preserving **O(1) independent per-row random access** and fast equality/prefix evaluation without decompressing a block.
- **ClickHouse Trigram Skip Index (`ngrambf_v1`):** Per-frame lowercase ASCII/byte trigram bloom filter, **sized adaptively against compressed frame size** (never > 10% of compressed frame bytes, omitted if saturation > 50%) so it never bloats dense `W1` data (fixing the 5 MiB fixed-size overhead observed in `README.md` line 236) and never wastes space on incompressible `W4` data. Paired with 63 B chunk-tail edge tables for exact cross-boundary matching.

---

## 5. Steal vs Reject Synthesis Matrix

| Technique | Origin | Verdict | Tables | Write Profile | Workloads | Engineering Rationale |
|---|---|---|---|---|---|---|
| **Zero-parse fixed-width tail + 64 B Postscript** | UE5 `.utoc`, ISOBMFF `sidx`, Strata v0 | **Steal** | `spans`, `traces`, `scores` | `L0Fast` & `L1Compact` | W1–W4 | Eliminates Parquet's O(R×C) Thrift parse; 1 S3 GET (≤ 1 MiB tail) opens & prunes cold files. |
| **PAX 16k Row Groups (Core + Wide-Row) + 4k Mini-Blocks** | Lance v2, ClickHouse, PAX | **Adapt** | `spans`, `traces`, `scores` | `L0Fast` & `L1Compact` | W1–W4 | Guarantees ≤ 2 GET point lookups (≤ 1 MiB `ReadCache` block) AND zero wide-column I/O on narrow scans. |
| **1 MiB `ReadCache` boundary alignment (tail + pack frames)** | Frostbite CAS, DirectStorage, EROFS | **Steal** | `spans`, `traces`, `scores` | `L0Fast` & `L1Compact` | W1–W4 | Eliminates 2× S3 amplification from small frames straddling 1 MiB `ReadCache` blocks (< 1.5% pad). |
| **Inline small-payload threshold (≤ 256 B)** | Source 2 VPK `PreloadBytes`, EROFS `ztailpacking` | **Steal** | `spans`, `traces`, `scores` | `L0Fast` & `L1Compact` | W1–W4 | Avoids chunk-pack indirection & extra GETs for short previews, replies, and error strings. |
| **Per-`(project, day)` immutable `.stpk` chunk packs (64 KiB frames, BLAKE3-128)** | UE5 `.ucas`, Frostbite CAS, Git packfiles | **Adapt** | `spans` (payloads) | `L1Compact` only | W1, W2 | Cross-file day dedup + O(1) day retention drop + cryptographic collision safety + airtight GDPR rewrite. |
| **Structural JSON message-array chunking + FastCDC fallback + `>=256 B` rule** | LLM-domain + FastCDC | **Steal** | `spans` (`input`, `system_instructions`, `tool_definitions`) | `L1Compact` only | W1, W2 | Near-100% dedup on growing multi-turn conversations at ~5× lower CPU than byte CDC; exact ≤ 64 B boundary proof. |
| **Tail-sharing chunk-ref lists (`back 1..=32`, depth ≤ 15)** | Video GOP I/P-frames, Strata v0 | **Steal** | `spans` (payload refs) | `L1Compact` only | W1, W2 | Collapses $O(T^2)$ agent history ref lists to $O(T)$ with bounded O(1) single-row decode. |
| **FastLanes-1024 SIMD Bit-Packing, FOR & Parallel Delta** | FastLanes (CWI), Vortex | **Steal** | `spans`, `traces`, `scores` | `L0Fast` & `L1Compact` | W1–W4 | Multi-GB/s AVX2/NEON decode & encode for timestamps, durations, tokens, dict codes, offsets. |
| **ALP Lossless Float Compression (+ exact bit exceptions)** | CWI / DuckDB (SIGMOD 2024) | **Steal** | `spans`, `traces`, `scores` (`cost_*`, `value_num`) | `L1Compact` (fast check in `L0Fast`) | W1–W4 | 2–5× smaller and 5–10× faster than Zstd on decimal-origin USD costs; bit-exact for `NaN`/`±inf`/`-0.0`. |
| **Sorted File-Level Dictionaries + Zero-Decompress `Column::Dict`/`Rle`/`Const`** | Parquet, Vortex, `cetra-column` | **Steal** | `spans`, `traces`, `scores` | `L0Fast` (bounded) & `L1Compact` (full sorted) | W1–W4 | Zero-copy pass-through into `cetra-query`; sorted dicts turn range/prefix filters into integer code checks. |
| **FSST String Compression** | CWI / DuckDB (VLDB 2020) | **Steal** | `spans`, `traces`, `scores` (short/med strings) | `L1Compact` only | W1–W3 | 2–3× compression with O(1) per-row random access; skipped in `L0Fast` (LZ4) and bypassed on `W4`. |
| **Meta Nimble Flat-Map Shredding (≥ 5% presence, ≤ 64 keys)** | Meta Nimble / Velox | **Adapt** | `spans` (`attributes`, `resource_attributes`, `metadata`), `traces`/`scores` (`metadata`) | `L1Compact` only | W1–W4 | `map['key']` reads 1 tiny sub-column (3–10× faster); `W4` 5,000-key maps shred 0 keys (zero bloat). |
| **Native Typed `Events` & `Links` Sub-Streams** | `cetra-column` (D-QE-11) | **Steal** | `spans` (`events`, `links`) | `L0Fast` & `L1Compact` | W1–W4 | Eliminates JSON/Arrow-IPC serialization overhead; timestamps use Delta, names use Dict. |
| **Contiguous SIMD Split-Block Bloom Filter (hot `trace_id` + lazy IDs)** | Parquet SBBF + AVX2 SIMD | **Adapt** | `spans`, `traces`, `scores` | `L0Fast` & `L1Compact` | W1–W4 | `trace_id` bloom packed in hot tail (1 GET prune); AVX2 probes 8 lanes in 1 SIMD op; faster build than Binary Fuse in `L0Fast`. |
| **Adaptive Trigram Frame Filter + 63 B Chunk-Tail Edge Table** | ClickHouse `ngrambf_v1`, Strata v0 | **Adapt** | `spans` (payloads & `name`) | `L1Compact` only | W1–W3 | 8–15× faster `LIKE`/`ILIKE` (0 frames read on absent needle); sized ≤ 10% compressed frame & omitted if saturated (`W4`). |
| **Incompressibility Guard (`Codec::None` if compressed ≥ raw - 16 B)** | EROFS, SquashFS, ORC | **Steal** | `spans`, `traces`, `scores` | `L0Fast` & `L1Compact` | W4 | Guarantees ≤ 5% size overhead and memcpy-speed reads on random/incompressible data. |
| **Optional AEAD Encryption Envelope Slot** | UE5 `.utoc`, Parquet Modular Encryption | **Adapt** | `.statam` & `.stpk` | `L0Fast` & `L1Compact` | All | Reserved header/footer slot + per-block/frame auth-tag support; 0 B / 0 CPU overhead when disabled. |
| **Zstd Trained Dictionaries (default-on)** | Zstd / Oodle | **Reject (default); Opt-in slot only** | `.stpk` (optional `DictSection`) | `L1Compact` (opt-in) | W1, W2 | Dedup already beats trained dicts at lower CPU; GDPR erasure requires discarding/retraining old dicts. |
| **Zstd `--long` (LDM) across large streams** | Zstd CLI | **Reject** | — | — | — | Destroys random access on point lookups; `.stpk` dedup achieves day-wide distance with 64 KiB frames. |
| **Global Cross-Day Chunk Store** | restic / borg | **Reject** | — | — | — | Unbounded RAM index (12–25 GB/TB) and ref-counted GC breaks O(1) day retention. |
| **Monolithic Fixed-Row Variable-Width Segments (Vortex 8192-row)** | Vortex | **Reject** | — | — | — | Causes 66–80 MB point lookups; replaced by 4k mini-blocks + `.stpk` 64 KiB frames. |
| **Blind I/O Coalescing Across Pruned Blocks** | Vortex `FileSegmentSource` | **Reject** | — | — | — | Pulls pruned segments on point lookups; replaced by explicit `Point` vs `Scan` coalescing policies. |
| **Arrow IPC Schema & Fallback Encoding** | Parquet / Arrow / Strata v0 | **Reject** | — | — | — | Statam is 100% Cetra-native (`cetra-column`); compact native schema descriptor removes Arrow dependency. |
| **Suffix Arrays / FM-Index / Full Inverted Text Index** | Tantivy / Zoekt | **Reject** | — | — | — | High write CPU/RAM and multi-× storage overhead; adaptive trigram bloom + unique-chunk scan wins for Cetra's workload. |
