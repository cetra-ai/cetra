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

M2.1 runs in its own git worktree, on its own branch cut from the commit that holds M1.1, while M1.2–M1.4 continue on the main research branch.

- It adds new files only (`src/payload/mod.rs`, `src/payload/chunk.rs`), plus **one line** in `src/lib.rs` and one dev-dependency line in `Cargo.toml`.
- It does **not** touch `config.rs`, `error.rs` or any `narrow/` file.

The orchestrating session merges it after M1 and resolves the `lib.rs` and `Cargo.toml` one-line conflicts.

---

## M2.1: value chunking

**Goal:** a pure function that cuts one payload value into chunks according to S4 steps 1–5. Concatenating the chunks gives back the value exactly, and every chunk but the last is at least 256 bytes. Nothing is stored yet; M2.2 uses the function.

**Edit:** `strata/src/payload/mod.rs`, `strata/src/payload/chunk.rs`, `strata/src/lib.rs` (one line), `strata/Cargo.toml` (one dev-dependency).

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

1. **`payload/mod.rs`:** `mod chunk;`, then `pub(crate) use chunk::chunk_value;`.

2. **`lib.rs`:** add `#[allow(dead_code)] // used by the writer from M2.2 on` on the line before `mod payload;`. That is the only change to `lib.rs`.

3. **The gear table.** `fn gear_table() -> [u64; 256]` must reproduce `bench/src/payload.rs::Gear::new` exactly:

   ```rust
   let mut t = [0u64; 256];
   let mut x: u64 = GEAR_SEED;
   for v in t.iter_mut() {
       x ^= x << 13;
       x ^= x >> 7;
       x ^= x << 17;
       *v = x;
   }
   t
   ```

   Build it once in a `std::sync::OnceLock<[u64; 256]>` named `GEAR`. Read entries with `GEAR_TABLE.get(b as usize).copied().unwrap_or(0)`; the index is a `u8`, so it is always in bounds, and `get` satisfies the no-indexing lint.

4. **`fn gear_pieces(data: &[u8], out: &mut Vec<Range<usize>>, base: usize)`** appends piece ranges, offset by `base`, covering `data` exactly. The algorithm, identical to the reference:

   ```
   start = 0
   while start < data.len():
       rest = data[start..]
       if rest.len() <= GEAR_MIN: push start..data.len(); break
       h = 0u64
       cut = min(rest.len(), GEAR_MAX)
       for i in GEAR_MIN .. min(rest.len(), GEAR_MAX):     // bytes before GEAR_MIN are not hashed
           h = (h << 1).wrapping_add(GEAR[rest[i]])
           if (h >> 52) & GEAR_MASK == 0: cut = i + 1; break
       push start..start + cut
       start += cut
   ```

   Use `rest.iter().enumerate().take(GEAR_MAX).skip(GEAR_MIN)` as the reference does, which needs no indexing. An empty `data` pushes nothing.

5. **The JSON scanner.** `fn json_units(value: &[u8]) -> Option<Vec<usize>>` returns the **cut positions**: the byte indexes where a new unit starts. Unit *k* runs from `cuts[k−1]` (or 0) to `cuts[k]` (or `value.len()`). It returns `None` if `value` is not a JSON array the scanner accepts.

   Algorithm, over the bytes:
   1. Skip ASCII whitespace (`b' '`, `b'\t'`, `b'\n'`, `b'\r'`). If the next byte is not `b'['`, return `None`. Set `depth = 1`, `in_string = false`, `escape = false`.
   2. For each following byte `c` at index `i`:
      - **In a string:** if `escape`, set `escape = false`; else if `c == b'\\'`, set `escape = true`; else if `c == b'"'`, set `in_string = false`. Nothing else matters inside strings.
      - **Outside a string:**
        - `b'"'` sets `in_string = true`;
        - `b'['` or `b'{'` does `depth += 1`;
        - `b']'` or `b'}'` does `depth -= 1`. If `depth` reaches 0, this byte must be `b']'`, and everything after it must be ASCII whitespace; otherwise return `None`. Stop the loop either way.
        - `b','` with `depth == 1` pushes the cut `i + 1` (the comma stays in the unit before the cut).
        - Any other byte is ignored. The scanner does not validate numbers, literals or the pairing of `[`/`{` with `]`/`}` beyond the depth count.
   3. If the loop ends with `depth != 0` or `in_string == true`, return `None`.

   With this rule, the opening `[` and any leading whitespace belong to unit 0, the closing `]` and trailing whitespace belong to the last unit, and the whitespace after a comma belongs to the next unit. `[]` and `[x]` have no cuts, so they form one unit.

6. **`pub(crate) fn chunk_value(value: &[u8]) -> Vec<Range<usize>>`** returns chunk ranges into `value`:
   1. An empty `value` returns an empty `Vec` (zero chunks).
   2. **Pieces:**
      - When `json_units(value)` is `Some(cuts)`: for each unit range, a unit shorter than `GEAR_UNIT_THRESHOLD` is one piece; a longer unit is cut by `gear_pieces(&value[unit], out, unit.start)`.
      - Otherwise: `gear_pieces(value, out, 0)`.
   3. **Accumulation:** walk the pieces in order, keeping `pending_start` (the start of the current chunk) and `pending_end`. After adding each piece, if `pending_end - pending_start >= MIN_CHUNK`, push `pending_start..pending_end` and start a new chunk at `pending_end`. After the last piece, if `pending_end > pending_start`, push the remainder.
   4. Return the ranges. They are contiguous, non-empty, and cover `0..value.len()`.

   Slicing `value[unit]` with ranges the scanner produced is not allowed under the lints; use `value.get(range)`, and treat a `None` as unreachable by returning the whole value as one chunk. Ranges from the scanner are always in bounds, so this never happens, but no `unwrap` or index is used.

7. **`Cargo.toml`:** add `serde_json = "1"` under `[dev-dependencies]`, the same spec as `bench/Cargo.toml`.

### Tests (`#[cfg(test)] mod tests` in `chunk.rs`)

**Gear chunker:**
- **Reference equality.** Copy `bench/src/payload.rs::Gear` (table and `chunks`) verbatim into the test module as `RefGear`. A proptest (`ProptestConfig::with_cases(500)`) over random byte vectors of length 0..40,000, half of them drawn from a small alphabet (`b"ab {}[],\"\n"`) to get repeated content, checks that `gear_pieces(data, out, 0)` produces exactly the piece lengths of `RefGear::chunks`.
- An empty input gives no pieces; 512 bytes give one piece; 8,193 zero bytes give pieces of lengths `[8192, 1]` (and match the reference).

**JSON scanner (`json_units`):**

| Input | Expected |
|---|---|
| `[]` | `Some([])` |
| `[1]` | `Some([])` |
| `[1,2]` | `Some([3])` |
| `  [1, 2]  ` | `Some([5])` |
| `[[1,2],[3]]` | `Some([7])`: the inner comma is at depth 2 and is not a cut |
| `["a,b","c]d","e\"f",{"k":[1,2]}]` | cuts only after the three top-level commas |
| `["\\",1]` | `Some([6])`: an escaped backslash followed by the closing quote |
| `[1,2` | `None` |
| `[1]]` | `None` |
| `[1] x` | `None` |
| `{"a":1}` | `None` |
| `"[1,2]"` | `None` |
| `[` | `None` |
| `["unterminated]` | `None` |
| empty input | `None` |

**`chunk_value`:**
- Empty value: `[]`.
- A 1 B, a 255 B and a 256 B non-JSON value each give one chunk, the whole value.
- A JSON array of four 100-byte string elements (each unit about 103 B) gives two chunks. The first ends right after the 3rd top-level comma (3 units ≥ 256 B); the second holds the rest.
- A JSON array with one 100 KiB element in the middle: its gear pieces appear as chunk boundaries inside it, every non-last chunk is ≥ 256 B, and the concatenation equals the value.
- Invalid JSON (`[1,2`) of 20 KiB is chunked by gear only.
- **Property** (`ProptestConfig::with_cases(500)`). Values are either:
  - random JSON arrays: a `serde_json::Value::Array` of 0..40 elements, each a random string (0..3,000 chars, alphabet including `"`, `\`, `,`, `[`, `]`, `{`, `}`, `\n` and multi-byte UTF-8), a nested array, an object, or a number, serialised with `serde_json::to_string`; or
  - random bytes of length 0..50,000.

  For each value, check that:
  1. the ranges are contiguous from 0, non-empty, and end at `value.len()`;
  2. the concatenation of the chunks equals the value;
  3. every chunk except the last has length ≥ `MIN_CHUNK`;
  4. calling `chunk_value` twice gives the same ranges (deterministic);
  5. for JSON values whose units are all shorter than `GEAR_UNIT_THRESHOLD`, every chunk end except the last is one of the cut positions returned by `json_units`.

### Done when

The checks of M1 § Conventions pass on the M2.1 branch: `cargo +1.99.1 test`, `cargo +1.99.1 clippy --all-targets -- -D warnings`, `cargo +1.99.1 fmt --check`.

**Commit:** `strata: add payload value chunking (M2.1)`
