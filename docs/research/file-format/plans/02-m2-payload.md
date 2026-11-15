# Strata v0 · M2: payload layer

Implementation plan for phases 3a, 3b and 3c of [the prototype design](../prototype-plan.md).

| Sub-phase | Design phase | Content | Status of this plan |
|---|---|---|---|
| M2.1 | 3a | value chunking: JSON scanner, gear chunker, accumulation | **detailed below**; independent of M1.2–M1.4, so it can run in parallel with them |
| M2.2 | 3b | dedup, frames, payload header, refs pages, writer and reader integration | **detailed below**; starts after M1.4 is committed and M2.1 is merged |
| M2.3 | 3c | edge groups, trigram filters, rollup, `Contains` | written once M2.2 is committed |

**Prerequisites for M2.1:** M1.1 is committed, so the `strata` crate exists with `StrataError` and the conventions of `01-m1-container-narrow.md`.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions, § Conventions for every sub-phase. They all apply here unchanged: build environment, checks, lints, no indexing, no panics, tests.
- [`../prototype-plan.md`](../prototype-plan.md): § S4, Chunking, steps 1–5.
- `docs/research/file-format/bench/src/payload.rs`, `struct Gear`: the reference gear chunker. M2.1 rewrites it; that file stays untouched.

## Parallel work with M1

M2.1 runs in its **own git worktree and branch** while the M1 agent keeps working in the main checkout.

**Setup** (the orchestrating session runs this before starting the M2.1 agent, and gives it the hash):

```bash
cd /home/sebastien/travaux/llm-observer
git worktree add /home/sebastien/travaux/llm-observer-m2 -b claude/strata-m2-1 <M1.1-COMMIT-HASH>
```

The M2.1 agent's environment replaces the `cd` line and the target directory of M1 § Conventions › Build environment:

```bash
mkdir -p /tmp/claude-1000/-home-sebastien-travaux-llm-observer/16446d53-f065-45fb-a416-4a592a2406df/scratchpad
export S=/tmp/claude-1000/-home-sebastien-travaux-llm-observer/16446d53-f065-45fb-a416-4a592a2406df/scratchpad
export CARGO_TARGET_DIR=$S/target-strata-m2 RUST_MIN_STACK=134217728
cd /home/sebastien/travaux/llm-observer-m2/docs/research/file-format/strata
```

`target-strata-m2` is not `target-strata`: the M1 agent builds there at the same time.

**Rules for the M2.1 agent:**
- Work only in `/home/sebastien/travaux/llm-observer-m2/docs/research/file-format/strata`.
- Never touch the main checkout `/home/sebastien/travaux/llm-observer`, and never run `git checkout`, `switch`, `merge`, `rebase` or `stash` there.
- If the M1.1 commit hash was not given, stop and report.

**Files.**
- M2.1 adds two new files, `src/payload/mod.rs` and `src/payload/chunk.rs`.
- In `src/lib.rs` it adds exactly two lines: the `#[allow(dead_code)]` attribute and `mod payload;`, placed in alphabetical order among the `mod` lines (rustfmt's order), with the attribute directly above it. It changes no other line of `lib.rs`, so M1.2's removal of its own `allow` lines merges cleanly.
- In `Cargo.toml` it adds one line, `serde_json = "1"`, under `[dev-dependencies]`.
- It does not touch `config.rs`, `error.rs`, or any `narrow/`, `hot.rs`, `file.rs` or `segment.rs` file.

**After the commit.** Commit `Cargo.lock` too. Leave the worktree and the branch in place: the orchestrating session merges them into the research branch after M1, and removes them. Expected conflicts on merge:
- `Cargo.lock`: take M1's side, then rerun `cargo test`, which regenerates it;
- `Cargo.toml` `[dev-dependencies]`: keep both sides;
- `lib.rs`: keep both sides.

---

## M2.1: value chunking

**Goal:** a pure function that cuts one payload value into chunks according to S4 steps 1–5. Concatenating the chunks gives back the value exactly, and every chunk but the last is at least 256 bytes. Nothing is stored yet; M2.2 uses the function.

**Edit:** `strata/src/payload/mod.rs`, `strata/src/payload/chunk.rs`, `strata/src/lib.rs` (two lines), `strata/Cargo.toml` (one dev-dependency line).

**Dependency** (in addition to M1 § Versions):

| Crate | Spec | Where | Why |
|---|---|---|---|
| `serde_json` | `"1"` | dev-dep | random JSON arrays in the chunk tests |

### Constants (`chunk.rs`)

These are part of the file format (version 0). They are constants, not `Config` fields.

```rust
/// Minimum size of every chunk of a value except its last (S4 step 5).
pub(crate) const MIN_CHUNK: usize = 256;
/// A JSON unit at least this long is cut by the gear chunker (S4 step 2).
pub(crate) const GEAR_UNIT_THRESHOLD: usize = 8192;
/// Gear chunker parameters (S4 step 2).
pub(crate) const GEAR_MIN: usize = 512;
pub(crate) const GEAR_MAX: usize = 8192;
pub(crate) const GEAR_MASK: u64 = (1 << 11) - 1;
const GEAR_SEED: u64 = 0x1234_5678_9abc_def1;
```

### Steps

1. **`payload/mod.rs`** contains exactly `mod chunk;`. It has no `pub(crate) use` re-export: nothing uses the function until M2.2, so a re-export would be an unused import and fail `-D warnings`. M2.2 adds `pub(crate) use chunk::chunk_value;` when the writer first calls it.

2. **`lib.rs`.** Two lines, as described in § Parallel work with M1:

   ```rust
   #[allow(dead_code)] // used by the writer from M2.2 on
   mod payload;
   ```

   This `allow` covers `chunk_value`, `json_units`, `gear_pieces` and the constants. The tests inside `chunk.rs` use these private items directly.

3. **The gear table.** It must reproduce `bench/src/payload.rs::Gear::new` exactly:

   ```rust
   static GEAR_TABLE: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();

   fn gear_table() -> [u64; 256] {
       let mut t = [0u64; 256];
       let mut x: u64 = GEAR_SEED;
       for v in t.iter_mut() {
           x ^= x << 13;
           x ^= x >> 7;
           x ^= x << 17;
           *v = x;
       }
       t
   }

   fn gear_entry(b: u8) -> u64 {
       GEAR_TABLE.get_or_init(gear_table).get(usize::from(b)).copied().unwrap_or(0)
   }
   ```

   The index is a `u8`, so `get` always succeeds; it is used only to satisfy the no-indexing lint.

4. **`fn gear_pieces(data: &[u8], out: &mut Vec<std::ops::Range<usize>>, base: usize)`** appends piece ranges covering `data` exactly, with `base` added to both ends of every range. The algorithm is identical to the reference:

   ```rust
   let mut start = 0usize;
   while start < data.len() {
       let Some(rest) = data.get(start..) else { break };     // never breaks: start < len
       if rest.len() <= GEAR_MIN {
           out.push(base + start..base + data.len());
           break;
       }
       let mut h: u64 = 0;
       let mut cut = rest.len().min(GEAR_MAX);
       for (i, &b) in rest.iter().enumerate().take(GEAR_MAX).skip(GEAR_MIN) {
           h = (h << 1).wrapping_add(gear_entry(b));
           if (h >> 52) & GEAR_MASK == 0 {
               cut = i + 1;
               break;
           }
       }
       out.push(base + start..base + start + cut);
       start += cut;
   }
   ```

   Bytes before `GEAR_MIN` are not hashed. An empty `data` pushes nothing.

5. **The JSON scanner.** `fn json_units(value: &[u8]) -> Option<Vec<usize>>` returns the **cut positions**: the absolute byte indexes where a new unit starts. Unit *k* runs from `cuts[k−1]` (or 0) to `cuts[k]` (or `value.len()`). It returns `None` when `value` is not a JSON array the scanner accepts.

   Whitespace means exactly the four bytes `b' '`, `b'\t'`, `b'\n'`, `b'\r'`. Write it as `fn is_ws(b: u8) -> bool { matches!(b, b' ' | b'\t' | b'\n' | b'\r') }`. Do not use `is_ascii_whitespace`, which also accepts form feed.

   ```rust
   let first = value.iter().position(|&b| !is_ws(b))?;          // all-whitespace or empty → None
   if value.get(first) != Some(&b'[') { return None; }
   let (mut depth, mut in_string, mut escape) = (1u32, false, false);
   let mut cuts = Vec::new();
   for (i, &c) in value.iter().enumerate().skip(first + 1) {   // i stays an absolute index
       if in_string {
           if escape { escape = false; }
           else if c == b'\\' { escape = true; }
           else if c == b'"' { in_string = false; }
           continue;
       }
       match c {
           b'"' => in_string = true,
           b'[' | b'{' => depth += 1,
           b']' | b'}' => {
               depth -= 1;                       // depth ≥ 1 here, so no underflow
               if depth == 0 {
                   let tail_ok = value.get(i + 1..).is_some_and(|t| t.iter().all(|&b| is_ws(b)));
                   return (c == b']' && tail_ok).then_some(cuts);
               }
           }
           b',' if depth == 1 => cuts.push(i + 1),
           _ => {}
       }
   }
   None                                          // loop ended with depth > 0 or inside a string
   ```

   With this rule:
   - the opening `[` and any leading whitespace belong to unit 0;
   - the closing `]` and trailing whitespace belong to the last unit;
   - the whitespace after a comma belongs to the next unit;
   - `[]` and `[x]` have no cuts and form one unit.

6. **`pub(crate) fn chunk_value(value: &[u8]) -> Vec<std::ops::Range<usize>>`** returns chunk ranges into `value`:
   1. An empty `value` returns an empty `Vec` (zero chunks).
   2. **Pieces.** Collect `pieces: Vec<Range<usize>>`.
      - When `json_units(value)` is `Some(cuts)`: the unit ranges are `0..cuts[0]`, `cuts[0]..cuts[1]`, …, `cuts[last]..value.len()`, or `0..value.len()` with no cuts. Build them with an iterator over `cuts` and no indexing.
        - A unit shorter than `GEAR_UNIT_THRESHOLD` is pushed as one piece.
        - A longer unit `u` is cut with `gear_pieces(value.get(u.clone()).unwrap_or_default(), &mut pieces, u.start)`. `value.get(u)` is always `Some`, because the scanner only produces in-bounds cuts; `unwrap_or_default()` returns an empty slice and is neither `unwrap` nor `expect`.
      - Otherwise: `gear_pieces(value, &mut pieces, 0)`.
   3. **Accumulation.** Keep `chunk_start = 0`. For each piece `p`, in order, if `p.end - chunk_start >= MIN_CHUNK`, push `chunk_start..p.end` and set `chunk_start = p.end`. After the loop, if `chunk_start < value.len()`, push `chunk_start..value.len()`.
   4. Return the ranges. They are contiguous, non-empty, and cover `0..value.len()`.

### Tests (`#[cfg(test)] mod tests` in `chunk.rs`)

The test module carries `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]`.

**Gear chunker:**
- **Reference equality.**
  - Copy `bench/src/payload.rs::Gear` (`new` and `chunks`) into the test module as `RefGear`, verbatim except for writing `(h >> 52) & MASK` with explicit parentheses.
  - A proptest (`ProptestConfig::with_cases(500)`) over byte vectors of length 0..40,000 checks that the lengths of the ranges from `gear_pieces(&data, &mut out, 0)` equal the lengths of the slices from `RefGear::chunks`. Half the cases draw bytes from `b"ab {}[],\"\n"` to get repeated content; the other half draw any byte.
- Fixed cases, each also equal to the reference:

  | Input | Pieces |
  |---|---|
  | empty | none |
  | 512 zero bytes | `[512]` |
  | 513 zero bytes | `[513]` |
  | 8,192 zero bytes | `[8192]` |
  | 8,193 zero bytes | `[8192, 1]` |
  | 9,000 zero bytes | `[8192, 808]` |
- `gear_pieces` with `base = 1000` shifts every range by 1,000.

**JSON scanner (`json_units`):**

| Input | Expected |
|---|---|
| `[]` | `Some([])` |
| `[1]` | `Some([])` |
| `[1,2]` | `Some([3])` |
| `  [1, 2]  ` | `Some([5])` |
| `[[1,2],[3]]` | `Some([7])`: the inner comma is at depth 2 and is not a cut |
| `["a,b","c]d","e\"f",{"k":[1,2]}]` | `Some([7, 13, 20])` |
| `["\\",1]` | `Some([6])`: an escaped backslash, then the closing quote |
| `[1,2` | `None` |
| `[1]]` | `None` |
| `[1] x` | `None` |
| `[1}` | `None` |
| `{"a":1}` | `None` |
| `"[1,2]"` | `None` |
| `[` | `None` |
| `["unterminated]` | `None` |
| `["a\` (escape at end of input) | `None` |
| empty input, and `"   "` | `None` |
| `[1]` followed by a form feed byte `0x0C` | `None` (form feed is not whitespace here) |

**`chunk_value`:**
- An empty value gives `[]`.
- `b"x".repeat(1)`, `b"x".repeat(255)` and `b"x".repeat(256)` (not JSON) each give one chunk, the whole value.
- **Four short elements.**
  - The value is `serde_json::to_string` of four strings of 100 `x` each: 413 bytes, with units of 104, 103, 103 and 103 bytes.
  - Expected chunks: `0..310` (after the 3rd comma, since 104 + 103 + 103 = 310 ≥ 256, while two units are only 207) and `310..413`.
- **One large element.**
  - The value is `serde_json::to_string` of `["a", <102,400 deterministic pseudo-random alphanumeric chars from a seeded xorshift>, "b"]`.
  - Assert:
    - at least 13 chunks;
    - every chunk except the last is ≥ 256 B and ≤ 8,192 + 255 B;
    - the concatenation equals the value.
  - Do **not** assert a boundary at the start of the big unit: the 5-byte first unit is merged into the first gear piece.
- Invalid JSON `[1,2` followed by 20,000 `x` bytes: the ranges equal those from `gear_pieces` followed by the accumulation (compute the expected value with the same accumulation over `RefGear` pieces).
- **Property** (`ProptestConfig::with_cases(500)`). Values are either:
  - random JSON arrays: a `serde_json::Value::Array` of 0..40 elements, each a random string (0..3,000 chars from an alphabet including `"`, `\`, `,`, `[`, `]`, `{`, `}`, `\n`, `é` and `日`), a nested array of up to 3 numbers, an object `{"k": <string>}`, or a number, serialised with `serde_json::to_string`; or
  - random bytes of length 0..50,000.

  For each value, check:
  1. the ranges are contiguous from 0, non-empty, and end at `value.len()` (an empty value gives no ranges);
  2. the concatenation of the chunks equals the value;
  3. every chunk except the last has length ≥ `MIN_CHUNK`;
  4. calling `chunk_value` twice gives the same ranges;
  5. when `json_units(value)` is `Some(cuts)` **and** every unit length (computed from the cuts, plus the last unit up to `value.len()`) is below `GEAR_UNIT_THRESHOLD`, every chunk end except the last is one of the cuts. The check is skipped otherwise; do not assume it always applies.

### Done when

In the M2.1 worktree, with the environment of § Parallel work with M1, these all pass:

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
```

**Commit:** `strata: add payload value chunking (M2.1)`, on branch `claude/strata-m2-1`. Include `Cargo.lock`, and do not push.

---

## M2.2: payload storage

**Prerequisites:**
- M1.4 is committed on the research branch.
- The orchestrating session has merged `claude/strata-m2-1` into the research branch (M2.1's `payload::chunk` is present), and `cargo test` is green after the merge.

**Goal:** the writer stores every payload column (S4 § Classification) with the payload layer, and `read_column` reads it back:
- values are cut with `chunk_value`;
- chunks are deduplicated;
- unique chunks are packed into zstd frames;
- each row's chunk-id list is stored in refs pages with tail sharing;
- one payload header per column ties them together.

No search structures yet (M2.3): the header's `n_edge_groups`, `filters_offset` and `filters_len` are 0.

**Edit:** `strata/src/config.rs`, `strata/src/lib.rs`, `strata/src/writer.rs`, `strata/src/reader.rs`, `strata/src/payload/{mod.rs,chunk.rs,varint.rs,frames.rs,refs.rs,header.rs,write.rs,read.rs}`, and `strata/tests/roundtrip_payload.rs` (new; the second integration test target).

### Config

Add one field to `Config`, with its doc comment and its default:

```rust
/// A Utf8 or Binary column whose average non-null value is at least this many bytes is
/// stored with the payload layer. Default 256.
pub payload_min_avg: usize,
```

### Constants (`payload/mod.rs`)

Format constants, version 0:

```rust
pub(crate) const FRAME_MAX_RAW: usize = 64 * 1024;        // close a frame at this raw size …
pub(crate) const FRAME_MAX_EST: f64 = 16.0 * 1024.0;      // … or at this estimated compressed size
pub(crate) const FRAME_INITIAL_RATIO: f64 = 4.0;          // compression ratio assumed for the first frame
pub(crate) const FRAME_ZSTD_LEVEL: i32 = 3;
pub(crate) const REFS_PAGE_ROWS: usize = 4096;
pub(crate) const REFS_MAX_BACK: usize = 32;               // a based record looks at most 32 rows back
pub(crate) const REFS_MAX_DEPTH: u8 = 15;                 // keyframe depth 0; based depth = base + 1 ≤ 15
pub(crate) const HEADER_FIXED_LEN: usize = 32;
```

### Steps

1. **`payload/mod.rs`.**
   - `mod chunk; mod varint; mod frames; mod refs; mod header; mod write; mod read;`
   - `pub(crate) use write::write_payload_column; pub(crate) use read::read_payload_column;`
   - The constants above.
   - Remove the `#[allow(dead_code)]` M2.1 put on `mod payload;` in `lib.rs`; clippy with `-D warnings` then proves everything is used.

2. **`payload/varint.rs`.** Unsigned LEB128 and zigzag, per the design's § Conventions:
   - `pub(crate) fn put_uvarint(out: &mut Vec<u8>, v: u64)`;
   - `pub(crate) fn get_uvarint(c: &mut Cursor<'_>, what: &'static str) -> Result<u64>`. It rejects encodings longer than 10 bytes, and a 10th byte with value above 1, as `Corrupt { what }`.
   - `pub(crate) fn zigzag(d: i64) -> u64 { ((d << 1) ^ (d >> 63)) as u64 }` and `pub(crate) fn unzigzag(z: u64) -> i64 { ((z >> 1) as i64) ^ -((z & 1) as i64) }`.

3. **`payload/frames.rs`: frame building and dedup.**

   ```rust
   pub(crate) struct FrameEntry { pub offset: u64, pub len: u32, pub raw_len: u32, pub first_chunk: u32, pub n_chunks: u32 }

   pub(crate) struct FrameBuilder {
       ids: HashMap<u128, u32>,     // xxh3_128(chunk bytes) → chunk id
       next_id: u32,
       cur: Vec<u8>,                // chunk bytes of the open frame
       cur_ends: Vec<u32>,          // end offsets within `cur`
       cur_first: u32,              // id of the open frame's first chunk
       ratio: f64,                  // running raw/compressed ratio; FRAME_INITIAL_RATIO at start
       raw_total: u64, stored_total: u64,
       pub frames: Vec<FrameEntry>,
   }
   impl FrameBuilder {
       pub fn new() -> Self;
       /// Returns the chunk's id; appends it to the open frame if new, closing the frame when full.
       pub fn add(&mut self, chunk: &[u8], out: &mut Out) -> Result<u32>;
       /// Closes the open frame if it has any chunk.
       pub fn finish(&mut self, out: &mut Out) -> Result<()>;
   }
   ```

   **`add`:**
   1. Compute `h = xxhash_rust::xxh3::xxh3_128(chunk)`. If `h` is in `ids`, return its id.
   2. Otherwise `id = next_id`, `next_id += 1` (overflow is `Unsupported`), insert into `ids`, append the chunk to `cur`, and push `cur.len()` as `u32` to `cur_ends`.
   3. Then, if `cur.len() >= FRAME_MAX_RAW` or `cur.len() as f64 / ratio >= FRAME_MAX_EST`, close the frame.
   4. Return `id`.

   **Closing a frame:**
   1. Build the raw frame: `u32 n_chunks`, then `u32 end_offset[n]` (the `cur_ends`), then `cur`.
   2. `encode_segment(Kind::PayloadFrame, &raw, Codec::Zstd, FRAME_ZSTD_LEVEL, out)`.
   3. Push `FrameEntry { offset, len, raw_len: raw.len(), first_chunk: cur_first, n_chunks }`.
   4. Update the ratio: `raw_total += raw.len()`, `stored_total += seg.len - 16`, `ratio = raw_total / stored_total` (as `f64`; keep the old ratio if `stored_total == 0`).
   5. Clear `cur` and `cur_ends`, and set `cur_first = next_id`.

   A chunk never spans two frames, so the ids of frame *f* are `first_chunk .. first_chunk + n_chunks`, consecutive, in frame order (the S4 invariant).

   **Decoding:** `pub(crate) fn decode_frame(raw: &[u8], offset: u64, n_chunks: u32) -> Result<Vec<Range<usize>>>`.
   - It returns the chunk ranges within the data area, as offsets into `raw` itself.
   - It checks that the stored `n_chunks` equals the argument, that the end offsets are non-decreasing, and that the last one equals the data length.

4. **`payload/refs.rs`: refs pages.**

   ```rust
   pub(crate) enum RowRefs<'a> { Null, Ids(&'a [u32]) }
   pub(crate) fn encode_page(rows: &[RowRefs<'_>], out: &mut Vec<u8>) -> Result<()>;
   pub(crate) fn decode_page(bytes: &[u8], offset: u64) -> Result<Vec<Option<Vec<u32>>>>;   // None = null row
   ```

   **Page layout:** `u32 n_rows` · `u32 row_offset[n_rows + 1]` (relative to the record area; `row_offset[0] = 0`) · records.

   **Records:**
   - `0` (null): no further bytes.
   - `1` (keyframe): `uvarint n`, then the ids. The first id is a `uvarint`; each later id is `zigzag(id − previous)` as a uvarint.
   - `2` (based): `uvarint back` (1..=32) · `uvarint n_new` · the new ids. The first new id is `zigzag(id − last id of the base's full list)`; each later one is `zigzag(id − previous)`.

   **Encoder.** Keep, for each row of the page, its full list and its depth. For row *r* with a non-empty list `L`:
   1. Candidates are rows `r − k` for k = 1..=`REFS_MAX_BACK` (within the page), that are non-null, have depth ≤ `REFS_MAX_DEPTH − 1`, and whose full list `B` is non-empty with `B.len() ≤ L.len()` and `L[..B.len()] == B`.
   2. Choose the candidate with the longest `B`; on ties, the smallest k.
   3. If a candidate exists, write a based record with `back = k` and the new ids `L[B.len()..]`, and depth = depth(base) + 1. Otherwise write a keyframe with depth 0.

   An empty list (an empty value) is a keyframe with n = 0. A null value is tag 0, never a base.

   **Decoder:**
   - Checks every offset: `row_offset` non-decreasing, the last equals the record area length, and each record consumes exactly its bytes.
   - Rejects:
     - a `back` of 0, greater than 32, or reaching before row 0;
     - a base that is a null row;
     - a chain depth above 15;
     - an id that leaves the `u32` range;
     - an unknown tag.

     Each rejection is `Corrupt`.
   - Builds full lists in row order, so a base is always already built (no recursion).
   - Caps `n` and `n_new` at `bytes.len()` before allocating, since each id takes at least one byte.

5. **`payload/header.rs`: the payload header.**

   ```rust
   pub(crate) struct RefPage { pub offset: u64, pub len: u32, pub first_row: u32 }
   pub(crate) struct EdgeGroup { pub offset: u64, pub len: u32, pub first_chunk: u32 }       // empty in M2.2
   pub(crate) struct PayloadHeader {
       pub n_rows: u32, pub n_chunks: u32,
       pub filters_offset: u64, pub filters_len: u32,                                           // 0 in M2.2
       pub ref_pages: Vec<RefPage>, pub edge_groups: Vec<EdgeGroup>, pub frames: Vec<FrameEntry>,
   }
   ```

   **`encode(&self, out: &mut Out)`** writes the design's S4 layout exactly:
   - `u32 n_rows` · `u32 n_frames` · `u32 n_chunks` · `u32 n_ref_pages` · `u32 n_edge_groups` · `u64 filters_offset` · `u32 filters_len` (the 32-byte fixed part);
   - the ref page table (16 B each: `u64 offset · u32 len · u32 first_row`);
   - the edge group table (16 B each);
   - the frame table (24 B each: `u64 offset · u32 len · u32 raw_len · u32 first_chunk · u32 n_chunks`).

   **`decode(bytes, offset) -> Result<PayloadHeader>`** checks that:
   - the length is exactly `32 + 16·n_ref_pages + 16·n_edge_groups + 24·n_frames`, with checked arithmetic, counts bounded by the byte length before allocating;
   - frames are contiguous: `first_chunk` of frame 0 is 0, each next one is the previous `first_chunk + n_chunks`, and the total equals `n_chunks`;
   - ref pages start at row 0 and are increasing.

   The header is stored as a `Kind::PayloadHeader` segment with `Codec::Zstd` at `config.zstd_level`.

6. **`payload/write.rs`.**
   - `pub(crate) fn is_payload(array: &dyn Array, config: &Config) -> bool` is true when the type is Utf8 or Binary, at least one value is non-null, and (sum of non-null value lengths) ÷ (number of non-null values) ≥ `config.payload_min_avg`, compared in integers as `sum >= min_avg × count`.
   - `pub(crate) fn write_payload_column(array: &dyn Array, config: &Config, out: &mut Out) -> Result<(SegmentRef, PayloadStats)>`, where `PayloadStats { pub unique_chunks: u32, pub frames: u32, pub chunk_refs: u64, pub bytes: u64 }` (`bytes` = the sum of the segment lengths it wrote).

   `write_payload_column` steps:
   1. For each row: null becomes `RowRefs::Null`. Otherwise get the value bytes (`StringArray::value(i).as_bytes()` or `BinaryArray::value(i)`), compute `chunk_value`, and map each range through `FrameBuilder::add` (slice with `.get(range)`), giving the row's id list.
   2. After the last row, call `FrameBuilder::finish`.
   3. Group the rows by `REFS_PAGE_ROWS`. Encode each group with `encode_page`, write it as a `Kind::PayloadRefs` segment (`Codec::Zstd`, `config.zstd_level`), and record its `RefPage`.
   4. Build the `PayloadHeader`, write it, and return its `SegmentRef` and the stats.

   Frames, refs pages and the header are written in that order, so all of a column's frames are contiguous in the file.

7. **`payload/read.rs`.** `pub(crate) async fn read_payload_column(file: &StrataFile, record: &DirectoryRecord, field: &Field) -> Result<ArrayRef>`:
   1. Read the header segment (`record.payload_offset`, `record.payload_len`) and decode it.
   2. Concurrently (`try_join_all`), read and decompress every refs page and every frame.
   3. Decode the frames into a `Vec<(Vec<u8>, Vec<Range<usize>>)>` indexed by frame number.
   4. For each row's list, look up each id: the frame is found by `partition_point` on `first_chunk`, and the chunk within it is `id − first_chunk`. An id ≥ `n_chunks` is `Corrupt`. Append the chunk bytes to a value buffer.
   5. Build a `StringArray` (validating UTF-8: `String::from_utf8`, error → `Corrupt { what: "payload utf8" }`) or a `BinaryArray`, with nulls, using `GenericByteBuilder`.
   6. The row count must equal `header.n_rows` and the file's `row_count`.

   This whole-column read is what `read_column` needs. Point lookups come with `scan` in M4.

8. **`writer.rs`.** In `finish`, before the narrow path of each column:

   ```rust
   if payload::write::is_payload(array.as_ref(), &self.config) { … payload path …; continue; }
   ```

   The payload path calls `write_payload_column` and pushes a `DirectoryRecord` with:
   - `layer: Layer::Payload`, `parent: u32::MAX`, `flags: 0`;
   - `pages_offset` = the current section-7 byte length, `n_pages: 0`, `n_chunks: 0`;
   - `dict_offset: 0`, `dict_len: 0`;
   - `payload_offset` and `payload_len` from the header `SegmentRef`;
   - `null_count`.

   Its `ColumnSummary` has all-zero `encoding_counts`, `chunks: 0`, and `bytes = stats.bytes`. Add `pub payload: Option<PayloadStats>` to `ColumnSummary`: `None` for narrow columns.

   A payload column can never be the first sort-key column; `new` already restricts the sort key's type.

9. **`reader.rs`.** `read_column` checks `record.layer`:
   - `Layer::Payload`: call `read_payload_column`;
   - `Layer::Narrow`: the existing path;
   - anything else: `Unsupported`.

10. **`file.rs`.**
    - `block_entries` returns an empty list for a payload column without reading anything (its `n_pages` is 0).
    - `pub(crate) fn directory_record(&self, column: usize) -> Result<&DirectoryRecord>` is added if not already present.

### Tests

**`varint.rs`:**
- Round trip of `0`, `1`, `127`, `128`, `u32::MAX`, `u64::MAX`, and 1,000 random values.
- Zigzag of `i64::MIN`, −1, 0, 1 and `i64::MAX`.
- 11-byte encodings, and 10-byte encodings with a 10th byte > 1, are rejected.

**`frames.rs`:**
- Adding the same chunk twice returns the same id, and it is stored once.
- 200 distinct 1 KiB chunks give frames each ≤ 64 KiB + 1 KiB of raw data; ids are consecutive per frame; `first_chunk` values are contiguous.
- **Frame closing:**
  - Add 400 distinct incompressible 1 KiB chunks: random bytes from a seeded xorshift, distinct by construction.
  - The first frame closes at its first chunk boundary where raw ≥ 64 KiB (the 4.0 initial ratio gives the same 16 KiB estimate there).
  - After that the measured ratio is about 1.0, so later frames close on the estimate. Assert that every frame after the first has a raw data size between 15 KiB and 18 KiB, excluding the last frame, which `finish` may close early.
  - Then add 400 distinct **compressible** 1 KiB chunks, each `format!("{k:08}")` followed by 1,016 `b'a'`, to a fresh builder. Every frame except the last closes on the 64 KiB raw limit (raw ≥ 64 KiB, and < 65 KiB).
- `decode_frame` round trips. Wrong `n_chunks`, decreasing offsets and short data are `Corrupt`.

**`refs.rs`:**
- A proptest (`with_cases(300)`) over pages of 1..600 rows, where each row is null (10%), empty (5%), or an id list built as a random prefix of the previous non-null row's list extended with 0..5 random ids (60%), or a fresh random list (25%). It checks that `decode_page(encode_page(rows)) == rows`, that every based record has `back ≤ 32`, and that the depth never exceeds 15 (expose a `#[cfg(test)]` helper returning the chosen depth per row).
- A conversation-like page (row *k*'s list = row *k − 1*'s list + 2 new ids, for 100 rows) is ≤ 25% of the size of the same page with only keyframes. Compute the keyframe-only size by encoding with `REFS_MAX_BACK = 0` through a `#[cfg(test)]` variant `encode_page_with(rows, max_back)`.
- **Never panic:** random bytes passed to `decode_page`.

**`header.rs`:**
- Round trip; exact encoded length.
- Non-contiguous `first_chunk` → `Corrupt`; truncated → `Corrupt`; never-panic proptest.

**`tests/roundtrip_payload.rs`** (integration target, `#[ignore]`d, run in release as part of Done when):
- Generate 3,000 traces exactly as `roundtrip_light.rs` does, **without** clearing the payload fields.
- Write with sort key `["trace_id", "start_time"]`, open from `InMemory`, and `read_column` every column; each equals the input.
- Assert that `input`, `output`, `system_instructions` and `tool_definitions` are payload columns (directory `layer` = Payload). For every other column, print its layer; do not assert it. Whether a column is payload depends on the data's average value length, which is what S4 specifies.
- Print, per payload column: raw value bytes, stored bytes, unique chunks, frames, chunk refs. Also print the total file size against `pq_default` (as in `roundtrip_light`).

**Edge cases** (unit test in `payload/write.rs`, through `write_payload_column` then `read_payload_column` on an in-memory file built by `StrataWriter`). A Utf8 column whose values include:
- null, `""`, 1 B, 255 B, 256 B and 1 MiB;
- invalid JSON (`[1,2`);
- a JSON array with a 100 KiB element;
- a string containing `]` and `\"`;
- 500 identical 10 KiB values (dedup: unique chunks ≈ one value's worth).

It round trips, and `is_payload` is true for it.

**Never panic:** random bytes passed to `decode_frame` and `PayloadHeader::decode`.

### Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light
```

The commit body holds the `roundtrip_payload` table.

**Commit:** `strata: add payload storage: dedup, frames and refs (M2.2)`
