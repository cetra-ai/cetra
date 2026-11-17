//! The gram index of a payload run (S4 search).
//!
//! The index records, per frame of a run, the hashed 5-grams of its chunks, and of a window after
//! each chunk (see [`window`]), so a needle that starts in a chunk has all its grams in that
//! chunk's frame. It is stored bit-sliced: row `r` holds one bit per frame, set when a gram of
//! the frame hashes to `r`. A query reads only the rows of the needle's grams, ANDs them, and
//! gets the frames that may hold the needle.
//!
//! Rows are stored in pages of `rows_per_page` rows, each `ceil(frames / 8)` bytes, as
//! `PayloadIndex` segments.

use std::collections::BTreeSet;

use crate::error::{Result, StrataError};

/// Gram length in bytes.
pub(crate) const GRAM: usize = 5;
/// Distinct grams per row of a frame's bitmap, on average: a load of 0.8 sets about 55% of
/// the bits.
const LOAD: f64 = 0.8;
/// Target size of one index page: a query reads one row per gram of its needle, so small
/// pages keep the rows it does not need out of its reads.
pub(crate) const PAGE_BYTES: usize = 1024;
/// Bytes of the next chunk indexed with a chunk: a needle of up to `MAX_NEEDLE` bytes that
/// starts in a chunk ends within this many bytes of the next one.
pub(crate) const WINDOW_HEAD: usize = 63;
/// Fewest rows of an index.
const MIN_ROWS: u32 = 64;
/// Most rows of an index.
pub(crate) const MAX_ROWS: u32 = 1 << 24;

/// The hash of the 5-gram held in the low 40 bits of `v` (its first byte lowest).
fn gram_hash(v: u64) -> u32 {
    ((v & 0xFF_FFFF_FFFF).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32
}

/// The row of hash `h` in an index of `rows` rows.
fn row_of(h: u32, rows: u32) -> u32 {
    ((u64::from(h) * u64::from(rows)) >> 32) as u32
}

/// Calls `f` with the hash of every 5-gram of `bytes`, in order. Eight grams at a time come from
/// one 16-byte load.
fn for_each_gram(bytes: &[u8], mut f: impl FnMut(u32)) {
    let n = bytes.len();
    if n < GRAM {
        return;
    }
    let mut start = 0usize;
    while let Some(block) = bytes.get(start..start + 16) {
        if let Ok(block) = <[u8; 16]>::try_from(block) {
            let word = u128::from_le_bytes(block);
            for k in 0..8 {
                f(gram_hash((word >> (8 * k)) as u64));
            }
        }
        start += 8;
    }
    // The last grams, with fewer than 16 bytes left.
    for s in start..=n - GRAM {
        let mut word = [0u8; 8];
        let src = bytes.get(s..n.min(s + 8)).unwrap_or_default();
        if let Some(dst) = word.get_mut(..src.len()) {
            dst.copy_from_slice(src);
        }
        f(gram_hash(u64::from_le_bytes(word)));
    }
}

/// The window indexed with chunk `left` when `right` follows it in some value: the last
/// `GRAM - 1` bytes of `left` and the first `head` bytes of `right`. With `head` =
/// `WINDOW_HEAD`, its grams are the ones that cross the boundary and the ones of `right`'s head;
/// with `GRAM - 1`, only the ones that cross it.
pub(crate) fn window(left: &[u8], right: &[u8], head: usize, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(
        left.get(left.len().saturating_sub(GRAM - 1)..)
            .unwrap_or_default(),
    );
    out.extend_from_slice(right.get(..right.len().min(head)).unwrap_or_default());
}

/// Estimates the distinct grams of `samples` by linear counting over a 2^20-bit bitmap.
pub(crate) fn estimate_distinct<'a>(samples: impl Iterator<Item = &'a [u8]>) -> f64 {
    const BITS: u32 = 1 << 20;
    let mut bitmap = vec![0u64; (BITS / 64) as usize];
    for bytes in samples {
        for_each_gram(bytes, |h| {
            let r = row_of(h, BITS);
            if let Some(word) = bitmap.get_mut((r / 64) as usize) {
                *word |= 1 << (r % 64);
            }
        });
    }
    let set: u32 = bitmap.iter().map(|w| w.count_ones()).sum();
    let zero = f64::from(BITS - set).max(1.0);
    f64::from(BITS) * (f64::from(BITS) / zero).ln()
}

/// The row count for frames of about `distinct` grams each.
pub(crate) fn rows_for(distinct: f64) -> u32 {
    let rows = (distinct / LOAD).ceil();
    if rows.is_finite() && rows > 0.0 {
        (rows as u64).clamp(u64::from(MIN_ROWS), u64::from(MAX_ROWS)) as u32
    } else {
        MIN_ROWS
    }
}

/// Builds the bitmaps of the frames of one run, then their bit-sliced pages.
pub(crate) struct IndexBuilder {
    rows: u32,
    words: usize,
    /// One bitmap of `rows` bits per frame.
    bitmaps: Vec<Vec<u64>>,
    /// One byte per row for the frame `open`, packed into its bitmap when another frame starts.
    /// A gram sets its byte with a plain store; OR-ing into a bitmap word would make every gram
    /// of a small index wait on the last one that hit the same word.
    seen: Vec<u8>,
    open: Option<usize>,
}

impl IndexBuilder {
    /// An index of `rows` rows over `frames` frames.
    pub(crate) fn new(rows: u32, frames: usize) -> Self {
        let words = (rows as usize).div_ceil(64);
        Self {
            rows,
            words,
            bitmaps: vec![vec![0u64; words]; frames],
            seen: vec![0u8; rows as usize],
            open: None,
        }
    }

    /// Adds the grams of `bytes` to frame `frame`. A frame out of range adds nothing. Calls for
    /// one frame should come together: each change of frame packs the previous one.
    pub(crate) fn add(&mut self, frame: usize, bytes: &[u8]) {
        if frame >= self.bitmaps.len() {
            return;
        }
        if self.open != Some(frame) {
            self.flush();
            self.open = Some(frame);
        }
        let rows = self.rows;
        let seen = &mut self.seen;
        for_each_gram(bytes, |h| {
            if let Some(byte) = seen.get_mut(row_of(h, rows) as usize) {
                *byte = 1;
            }
        });
    }

    /// Adds the grams of `bytes` to frame `frame` directly in its bitmap, for calls that jump
    /// between frames. A frame out of range adds nothing.
    pub(crate) fn add_scattered(&mut self, frame: usize, bytes: &[u8]) {
        self.flush();
        let rows = self.rows;
        if let Some(bitmap) = self.bitmaps.get_mut(frame) {
            for_each_gram(bytes, |h| {
                let r = row_of(h, rows);
                if let Some(word) = bitmap.get_mut((r >> 6) as usize) {
                    *word |= 1 << (r & 63);
                }
            });
        }
    }

    /// Packs the open frame's bytes into its bitmap.
    fn flush(&mut self) {
        let Some(frame) = self.open.take() else {
            return;
        };
        if let Some(bitmap) = self.bitmaps.get_mut(frame) {
            for (word, bytes) in bitmap.iter_mut().zip(self.seen.chunks(64)) {
                let mut bits = 0u64;
                for (i, &b) in bytes.iter().enumerate() {
                    bits |= u64::from(b & 1) << i;
                }
                *word |= bits;
            }
        }
        self.seen.fill(0);
    }

    /// Rows of the index.
    pub(crate) fn rows(&self) -> u32 {
        self.rows
    }

    /// Rows per page for this index.
    pub(crate) fn rows_per_page(&self) -> u32 {
        rows_per_page(self.bitmaps.len())
    }

    /// The pages: rows `[p × rows_per_page, …)`, each row `ceil(frames / 8)` bytes, bit `f`
    /// of a row set when frame `f` has a gram in that row.
    pub(crate) fn pages(&mut self) -> Vec<Vec<u8>> {
        self.flush();
        let row_bytes = self.bitmaps.len().div_ceil(8);
        let rows = self.rows as usize;
        let mut matrix = vec![0u8; rows * row_bytes];
        // Eight frames make one byte column; each 64-row word of theirs is eight 8×8 bit blocks.
        for (col, group) in self.bitmaps.chunks(8).enumerate() {
            for w in 0..self.words {
                let mut words = [0u64; 8];
                for (dst, bitmap) in words.iter_mut().zip(group) {
                    *dst = bitmap.get(w).copied().unwrap_or(0);
                }
                if words == [0; 8] {
                    continue;
                }
                for j in 0..8 {
                    // Byte k of `x` holds rows `w × 64 + j × 8 + 0..8` of frame `col × 8 + k`.
                    let mut x = 0u64;
                    for (k, word) in words.iter().enumerate() {
                        x |= ((word >> (8 * j)) & 0xFF) << (8 * k);
                    }
                    let t = transpose8(x);
                    for i in 0..8 {
                        let r = w * 64 + j * 8 + i;
                        if let Some(byte) = matrix.get_mut(r * row_bytes + col) {
                            *byte = (t >> (8 * i)) as u8;
                        }
                    }
                }
            }
        }
        let page = self.rows_per_page() as usize * row_bytes;
        matrix.chunks(page.max(1)).map(<[u8]>::to_vec).collect()
    }
}

/// Transposes the 8×8 bit matrix `x` (byte `k` is row `k`, bit `i` of it column `i`), so bit
/// `k` of byte `i` of the result is bit `i` of byte `k` of `x` (Hacker's Delight 7-3).
fn transpose8(mut x: u64) -> u64 {
    let mut t = (x ^ (x >> 7)) & 0x00AA_00AA_00AA_00AA;
    x ^= t ^ (t << 7);
    t = (x ^ (x >> 14)) & 0x0000_CCCC_0000_CCCC;
    x ^= t ^ (t << 14);
    t = (x ^ (x >> 28)) & 0x0000_0000_F0F0_F0F0;
    x ^= t ^ (t << 28);
    x
}

/// Rows per page for an index over `frames` frames: about `PAGE_BYTES` per page.
pub(crate) fn rows_per_page(frames: usize) -> u32 {
    let row_bytes = frames.div_ceil(8).max(1);
    u32::try_from((PAGE_BYTES / row_bytes).max(1)).unwrap_or(u32::MAX)
}

/// The rows a query for `needle` reads: those of all its grams, sorted and unique. A needle's
/// grams are mostly common (in nearly every frame); the few rare ones do the pruning, so none is
/// left out. Empty for a needle shorter than a gram, which every frame may hold.
pub(crate) fn needle_rows(needle: &[u8], rows: u32) -> Vec<u32> {
    let mut picked = BTreeSet::new();
    for_each_gram(needle, |h| {
        picked.insert(row_of(h, rows));
    });
    picked.into_iter().collect()
}

/// The frames set in every row of `rows` (each the bytes of one row), ascending. `frames` is
/// the index's frame count; a row of the wrong length is corrupt.
pub(crate) fn frames_in_all(rows: &[&[u8]], frames: usize, offset: u64) -> Result<Vec<usize>> {
    let row_bytes = frames.div_ceil(8);
    let mut acc = vec![0xFFu8; row_bytes];
    for row in rows {
        if row.len() != row_bytes {
            return Err(StrataError::Corrupt {
                what: "payload index row",
                offset,
            });
        }
        for (a, b) in acc.iter_mut().zip(row.iter()) {
            *a &= b;
        }
    }
    Ok((0..frames)
        .filter(|g| {
            acc.get(g / 8)
                .is_some_and(|byte| byte & (1 << (g % 8)) != 0)
        })
        .collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// The pages of `b`, one set bit at a time.
    fn naive_pages(b: &IndexBuilder) -> Vec<Vec<u8>> {
        let row_bytes = b.bitmaps.len().div_ceil(8);
        let mut matrix = vec![0u8; b.rows as usize * row_bytes];
        for (g, bitmap) in b.bitmaps.iter().enumerate() {
            for r in 0..b.rows as usize {
                if bitmap[r / 64] >> (r % 64) & 1 == 1 {
                    matrix[r * row_bytes + g / 8] |= 1 << (g % 8);
                }
            }
        }
        let page = b.rows_per_page() as usize * row_bytes;
        matrix.chunks(page.max(1)).map(<[u8]>::to_vec).collect()
    }

    #[test]
    fn pages_match_the_naive_transpose() {
        for (rows, frames) in [(64, 1), (100, 7), (1_000, 9), (4_099, 130)] {
            let mut b = IndexBuilder::new(rows, frames);
            let mut state = 0x1234_5678_9abc_def0u64 ^ u64::from(rows);
            for bitmap in &mut b.bitmaps {
                for word in bitmap.iter_mut() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *word = state & (state >> 3);
                }
                // Bits past the last row stay clear, as `add` never sets them.
                let tail = rows as usize % 64;
                if tail != 0
                    && let Some(last) = bitmap.last_mut()
                {
                    *last &= (1u64 << tail) - 1;
                }
            }
            let naive = naive_pages(&b);
            assert_eq!(b.pages(), naive, "rows {rows} frames {frames}");
        }
    }

    #[test]
    fn grouped_and_scattered_adds_set_the_same_bits() {
        let texts: Vec<Vec<u8>> = (0..12u32)
            .map(|i| format!("frame {i} text {} and more", i * 7919).into_bytes())
            .collect();
        let mut grouped = IndexBuilder::new(500, texts.len());
        let mut scattered = IndexBuilder::new(500, texts.len());
        for (f, t) in texts.iter().enumerate() {
            grouped.add(f, t);
            grouped.add(f, b"shared tail");
        }
        // Scattered adds after grouped ones, into earlier frames, as the pair windows do.
        grouped.add_scattered(1, b"late pair window");
        grouped.add(11, b"again");
        for (f, t) in texts.iter().enumerate().rev() {
            scattered.add_scattered(f, t);
            scattered.add_scattered(f, b"shared tail");
        }
        scattered.add_scattered(1, b"late pair window");
        scattered.add_scattered(11, b"again");
        assert_eq!(grouped.pages(), scattered.pages());
    }

    /// The frames an index built from `frames` (each a list of chunks) gives for `needle`.
    fn query(frames: &[Vec<&[u8]>], rows: u32, needle: &[u8]) -> Vec<usize> {
        let mut b = IndexBuilder::new(rows, frames.len());
        for (f, chunks) in frames.iter().enumerate() {
            for c in chunks {
                b.add(f, c);
            }
        }
        let pages = b.pages();
        let rpp = b.rows_per_page() as usize;
        let row_bytes = frames.len().div_ceil(8);
        let wanted = needle_rows(needle, rows);
        let row_slices: Vec<&[u8]> = wanted
            .iter()
            .map(|&r| {
                let page = &pages[r as usize / rpp];
                let at = (r as usize % rpp) * row_bytes;
                &page[at..at + row_bytes]
            })
            .collect();
        frames_in_all(&row_slices, frames.len(), 0).unwrap()
    }

    #[test]
    fn a_frame_holding_the_needle_is_always_returned() {
        let texts: Vec<Vec<u8>> = (0..20)
            .map(|i| {
                format!(
                    "frame {i} holds the token tok{i:04}x and filler text {}",
                    "ab".repeat(i)
                )
                .into_bytes()
            })
            .collect();
        let frames: Vec<Vec<&[u8]>> = texts.iter().map(|t| vec![t.as_slice()]).collect();
        for rows in [64u32, 1_000, 100_000] {
            for i in 0..20 {
                let needle = format!("tok{i:04}x");
                let got = query(&frames, rows, needle.as_bytes());
                assert!(
                    got.contains(&i),
                    "rows {rows}: frame {i} missing from {got:?}"
                );
            }
            // A needle shorter than a gram reads every frame.
            assert_eq!(query(&frames, rows, b"tok").len(), 20);
        }
        // With enough rows, a needle unique to one frame selects only that frame.
        assert_eq!(query(&frames, 100_000, b"tok0007x"), vec![7]);
    }

    #[test]
    fn pages_hold_every_row_once() {
        let mut b = IndexBuilder::new(3_000, 70);
        b.add(69, b"hello world");
        let pages = b.pages();
        let row_bytes = 70usize.div_ceil(8);
        let total: usize = pages.iter().map(Vec::len).sum();
        assert_eq!(total, 3_000 * row_bytes);
        assert!(pages.iter().all(|p| p.len() <= PAGE_BYTES));
        assert_eq!(rows_per_page(70) as usize, PAGE_BYTES / row_bytes);
    }

    #[test]
    fn needle_rows_are_bounded_and_short_needles_have_none() {
        assert!(needle_rows(b"abcd", 1_000).is_empty());
        assert_eq!(needle_rows(b"abcde", 1_000).len(), 1);
        let long = b"the quick brown fox jumps over the lazy dog";
        assert!(needle_rows(long, 1 << 20).len() <= long.len() - GRAM + 1);
    }

    #[test]
    fn rows_grow_with_the_estimate() {
        let text: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let est = estimate_distinct(std::iter::once(text.as_slice()));
        // About 800k grams, nearly all distinct.
        assert!(est > 500_000.0 && est < 1_000_000.0, "{est}");
        assert_eq!(rows_for(0.0), MIN_ROWS);
        assert!(rows_for(1e12) <= MAX_ROWS);
        assert!(rows_for(12_000.0) >= 9_000);
    }

    #[test]
    fn every_gram_is_visited_once_in_order() {
        for n in 0..20usize {
            let bytes: Vec<u8> = (0..n as u8).map(|b| b.wrapping_mul(37)).collect();
            let mut got = Vec::new();
            for_each_gram(&bytes, |h| got.push(h));
            let want: Vec<u32> = bytes
                .windows(GRAM)
                .map(|g| {
                    let mut word = [0u8; 8];
                    word[..GRAM].copy_from_slice(g);
                    gram_hash(u64::from_le_bytes(word))
                })
                .collect();
            assert_eq!(got, want, "n {n}");
        }
    }

    #[test]
    fn windows_join_a_tail_and_a_head() {
        let mut w = Vec::new();
        window(b"abcdefgh", &[b'x'; 100], WINDOW_HEAD, &mut w);
        assert_eq!(&w[..4], b"efgh");
        assert_eq!(w.len(), 4 + WINDOW_HEAD);
        window(b"abcdefgh", &[b'x'; 100], GRAM - 1, &mut w);
        assert_eq!(w, b"efghxxxx");
        window(b"ab", b"xy", WINDOW_HEAD, &mut w);
        assert_eq!(w, b"abxy");
    }

    #[test]
    fn a_wrong_row_length_is_corrupt() {
        assert!(frames_in_all(&[&[0u8; 3][..]], 9, 0).is_err());
        assert_eq!(frames_in_all(&[], 3, 0).unwrap(), vec![0, 1, 2]);
    }
}
