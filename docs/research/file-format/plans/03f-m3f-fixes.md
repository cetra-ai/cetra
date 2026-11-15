# Strata v0 · M3F: robustness fixes from the M2.2–M3 code review

One sub-phase, done by one Haiku agent in one go, ending with the checks green and one commit. It fixes the review findings that a crafted file can turn into a panic or a huge allocation, and adds the missing tests.

**Prerequisites:** M3.2b is committed.

**Read first:**
- [`01-m1-container-narrow.md`](01-m1-container-narrow.md): § Versions and § Conventions.
- [`03-m3-indexes-maps.md`](03-m3-indexes-maps.md): § Global rules.
- The code named below.

**Edit:** `strata/src/{format.rs,writer.rs,file.rs,payload/mod.rs,payload/header.rs,payload/refs.rs,payload/write.rs,payload/read.rs,payload/search.rs,shred.rs}`, plus `docs/research/file-format/prototype-plan.md` (§ Limits only).

No GOLDEN change is expected. If `golden_output_hash` changes, stop and report.

---

## Steps

1. **Row-count limit** (`format.rs`, `writer.rs`, `file.rs`).
   - Add to `format.rs`, after `MAX_PROMOTED_KEY_LEN`:

     ```rust
     /// Largest number of rows in one file: row ids are `u32`, and `row_count` itself must fit.
     pub const MAX_ROWS: u64 = u32::MAX as u64;
     ```

   - `writer.rs` `write`: replace `if total >= 1 << 32` with `if total > MAX_ROWS`. Behaviour is unchanged.
   - `file.rs` `open`: right after the postscript is decoded and before any hot-tail section is decoded, return `Corrupt { what: "row count", offset: <the postscript's file offset, as the other postscript errors in open use> }` when `postscript.row_count > MAX_ROWS`.

2. **Refs pages are exactly 4,096 rows** (`payload/header.rs`). In `PayloadHeader::decode`, after the ref pages are read, replace the current checks on `ref_pages` (first page at 0, increasing, `first_row < n_rows`) with one stronger rule: `ref_pages.len() == n_rows.div_ceil(REFS_PAGE_ROWS)` and `ref_pages[i].first_row == i × REFS_PAGE_ROWS` for every `i`, computed in `u64`. Otherwise `Corrupt { what: "payload ref page" }` (keep the error string the current checks use, if it differs, and say which in the report).
   - First check, in `payload/write.rs`, that the writer makes pages of exactly `REFS_PAGE_ROWS` rows (the last one shorter) and makes **no** page when `n_rows == 0`. If it does anything else, stop and report.
   - Update or replace the header unit tests the old checks had (`ref_pages_must_start_at_zero_and_increase` and the `with_chunks` helper) so they build valid pages under the new rule. Add one test: `n_rows = 4_097` with one page is `Corrupt`; with pages at `0` and `4_096` it decodes; with pages at `0` and `4_000` it is `Corrupt`.

3. **No allocation sized by an unchecked row count** (`payload/read.rs`). In `read_rows`, replace `Vec::with_capacity(header.n_rows as usize)` with `Vec::new()`. Rows are pushed page by page after each page's row count is checked, so the memory grows only with decoded pages.
   - `payload/search.rs` keeps `vec![false; n_rows]` on the rollup-miss path. Step 2 ties `n_rows` to the header size (one 16-byte page reference per 4,096 rows), which bounds it. Add a comment there stating this.

4. **Id cap per refs page** (`payload/mod.rs`, `payload/refs.rs`, `payload/write.rs`).
   - Add to `payload/mod.rs`:

     ```rust
     /// Largest total number of chunk ids over the rows of one refs page, after tail sharing is
     /// expanded. Bounds the memory a crafted page can make the reader allocate.
     pub(crate) const MAX_PAGE_IDS: usize = 1 << 24;
     ```

   - `refs.rs` `decode_page`: keep a running total of the lengths of the decoded lists (keyframe lists and expanded based lists). Before building a list (before `base.clone()` for a based record, before `read_ids` for a keyframe), check that `total + its length ≤ MAX_PAGE_IDS` with checked arithmetic; otherwise `Corrupt { what: "refs ids" }`.
   - `payload/write.rs`: when building a refs page, if its rows' total ids exceed `MAX_PAGE_IDS`, return `Unsupported { what: "more than 2^24 chunk ids in one refs page".to_string() }`. Find the place where the writer has each page's row id lists; if there is no single place, stop and report.
   - Test (`refs.rs`): encode, with the existing default-look-back page encoder, a page of 4,096 rows where row `r` holds the ids `g × 16_384 .. (g + 1) × 16_384` with `g = r / 16`. Rows in a group of 16 are identical, so the encoder may store them as based records; either way the page expands to 2^26 ids. `decode_page` on the encoded bytes gives `Corrupt { what: "refs ids" }`. A second test: the same construction with 1,024 rows (2^24 ids, exactly the cap) decodes, and equals the input.

5. **i32 offsets in `shred` and `unshred`** (`shred.rs`). Arrow `StringBuilder::append_value` panics when the total bytes pass `i32::MAX`.
   - `unshred` (the builders around line 321): before building, sum the key and value bytes the output will hold (residual keys and values plus, per row, the promoted keys and child values that are present), in `u64`. Above `i32::MAX as u64`, return `Corrupt { what: "shredded map", offset: 0 }`.
   - `shred` (the builders around line 195): sum the bytes each child and the residual's key and value builders will receive, in `u64`. Above `i32::MAX as u64`, return `Unsupported { what: "map column larger than 2 GiB".to_string() }`.
   - No test builds 2 GiB. Factor the check as `fn check_offsets(total: u64, err: impl FnOnce() -> StrataError) -> Result<()>` and unit-test it at `i32::MAX` (ok) and `i32::MAX + 1` (error).

6. **Docs** (`prototype-plan.md` § Limits): add `MAX_ROWS` (2^32 − 1 rows per file), `MAX_PAGE_IDS` (2^24 ids per refs page), and the 4,096-row refs page rule. Also add, in § S6, one sentence: "`unshred` returns each row's entries sorted bytewise by key (stable for equal keys); the original entry order is not kept."

## Tests (new, besides those in the steps)

- **`file.rs`:** patch the postscript `row_count` to `1 << 32` (with the same patch-and-refresh approach the existing postscript tests use) and expect `Corrupt { what: "row count" }` at open.
- **`payload/search.rs`:** a column of 1,500 distinct values of 1,000 random lowercase letters each, written with the default config so that it has more than 1,024 chunks (assert `header.n_chunks > 1_024` first; if it is not, increase the row count until it is and say so). Find, with `read_rows`, the rows whose id lists contain chunk 1,023 or 1,024. Take every 16-byte window starting at offsets `0, 8, 16, …` of those rows' values (at most 2,000 needles), plus 30 random 16-byte substrings of other rows. For each needle, `payload_contains` equals brute force over `read_column`.
- **`payload/search.rs`:** a payload column forced with `payload_min_avg = 0` where every value is the empty string `""` and 10% are null. `payload_contains` with needles `b"a"`, `b"ab"`, `b"abc"` is all-false; `read_column` round-trips.
- **`file.rs`:** `lookup` on a file whose sort-key column is a single `Const` chunk (every `trace_id` equal), with a different key of the right width, returns no range; with that key, one range `0..n`.

## Done when

```bash
cargo +1.99.1 test
cargo +1.99.1 clippy --all-targets -- -D warnings
cargo +1.99.1 fmt --check
cargo +1.99.1 test --release -- --ignored roundtrip_payload roundtrip_light
```

**Commit:** `strata: bound row counts, refs pages and map offsets on read (M3F)`
