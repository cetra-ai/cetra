# Strata v0 · M2: payload layer

Implementation plan for phases 3a, 3b and 3c of [the prototype design](../prototype-plan.md).

| Sub-phase | Design phase | Content | Status of this plan |
|---|---|---|---|
| M2.1 | 3a | value chunking: JSON scanner, gear chunker, accumulation | **detailed below**; independent of M1.2–M1.4, so it can run in parallel with them |
| M2.2 | 3b | dedup, frames, payload header, refs pages, writer integration | written once M1.4 is committed, because it extends M1's `StrataWriter::finish` and `read_column` |
| M2.3 | 3c | edge groups, trigram filters, rollup, `Contains` | written together with M2.2 |

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
