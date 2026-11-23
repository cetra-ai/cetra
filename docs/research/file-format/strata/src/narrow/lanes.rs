//! The lanes layout of bit-packed blocks (`flags::LANES`): values in blocks of 1,024, transposed
//! across 32 lanes of 32-bit words, after FastLanes (Afroozeh and Boncz, VLDB 2023).
//!
//! Value `i` of a block is in lane `i % 32`, row `i / 32`. Each lane packs its 32 rows one after
//! another, `width` bits each, into `width` words; word `k` of lane `l` is word `k * 32 + l` of
//! the block, little-endian. Unpacking row `r` shifts every lane by the same amount, so the 32
//! lanes are one loop the compiler vectorizes, and the 32 values of row `r` are values
//! `32 r .. 32 r + 32` of the block: the output is in row order. The layout takes exactly the
//! bytes of [`super::bitpack`]'s for a whole number of blocks: a chunk with `flags::LANES` has
//! every packed block of a multiple of 1,024 values and 1 to 32 bits in lanes (integer deltas,
//! dictionary codes, lengths), and its other blocks in order.
//!
//! The kernel is compiled twice on x86-64: for the baseline target and with AVX2, picked at run
//! time ([`unpack_lanes`]). Both are the same code; only the instructions differ.

use crate::error::{Result, StrataError};

/// Values per block.
pub const BLOCK: usize = 1024;
/// Lanes per block, and rows per lane.
const LANES: usize = 32;

/// Whether `n` values of `width` bits are stored in lanes: a whole number of blocks, 1 to 32
/// bits each.
pub fn fits_lanes(n: usize, width: u8) -> bool {
    (1..=32).contains(&width) && n > 0 && n.is_multiple_of(BLOCK)
}

/// The bytes of `n` values (a whole number of blocks) of `width` bits in lanes.
fn lanes_len(n: usize, width: u8) -> usize {
    n / BLOCK * usize::from(width) * LANES * 4
}

/// The mask of `width` bits (1 to 32).
fn mask32(width: usize) -> u32 {
    if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    }
}

/// Appends `values` in the lanes layout. They must fit [`fits_lanes`], and each must be below
/// `2^width`; higher bits are dropped.
pub fn pack_lanes(values: &[u64], width: u8, out: &mut Vec<u8>) {
    let w = usize::from(width);
    let m = mask32(w);
    out.reserve(lanes_len(values.len(), width));
    let mut words = vec![0u32; w * LANES];
    for block in values.as_chunks::<BLOCK>().0 {
        words.fill(0);
        for (i, &v) in block.iter().enumerate() {
            let (row, lane) = (i / LANES, i % LANES);
            let bit = row * w;
            let (k, s) = (bit / 32, bit % 32);
            // The mask keeps `width` bits, which is at most 32.
            let v = (v as u32) & m;
            if let Some(word) = words.get_mut(k * LANES + lane) {
                *word |= v << s;
            }
            if s + w > 32
                && let Some(word) = words.get_mut((k + 1) * LANES + lane)
            {
                *word |= v >> (32 - s);
            }
        }
        for word in &words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
}

/// Packs the blocks of one chunk: in lanes where they fit ([`fits_lanes`]) when `lanes` is set,
/// else in order, and remembers whether any block went in lanes (the chunk then carries
/// `flags::LANES`).
pub(crate) struct Packer {
    lanes: bool,
    used: std::cell::Cell<bool>,
}

impl Packer {
    pub(crate) fn new(lanes: bool) -> Self {
        Self {
            lanes,
            used: std::cell::Cell::new(false),
        }
    }

    /// Appends `values` (each below `2^width`) in lanes or in order.
    pub(crate) fn pack(&self, values: &[u64], width: u8, out: &mut Vec<u8>) {
        if self.lanes && fits_lanes(values.len(), width) {
            pack_lanes(values, width, out);
            self.used.set(true);
        } else {
            super::bitpack::pack(values, width, out);
        }
    }

    /// Whether a block went in lanes.
    pub(crate) fn used(&self) -> bool {
        self.used.get()
    }
}

/// Reads `n` values of `width` bits: in lanes when the chunk carries `flags::LANES` (`lanes`)
/// and they fit, else in order. Both take the same bytes.
pub(crate) fn unpack_block(bytes: &[u8], n: usize, width: u8, lanes: bool) -> Result<Vec<u64>> {
    if lanes && fits_lanes(n, width) {
        let mut out = Vec::with_capacity(n);
        extend_lanes(&mut out, bytes, n, width, |v| v)?;
        Ok(out)
    } else {
        super::bitpack::unpack(bytes, n, width)
    }
}

/// [`unpack_block`] for values of at most 32 bits, as `u32`. A width above 32 is `Corrupt`.
pub(crate) fn unpack_block_u32(bytes: &[u8], n: usize, width: u8, lanes: bool) -> Result<Vec<u32>> {
    if lanes && fits_lanes(n, width) {
        check_lanes(bytes, n, width)?;
        let mut out = vec![0u32; n];
        unpack_lanes(bytes, width, &mut out);
        Ok(out)
    } else {
        super::bitpack::unpack_u32(bytes, n, width)
    }
}

/// Checks that `bytes` holds exactly `n` values of `width` bits in lanes.
fn check_lanes(bytes: &[u8], n: usize, width: u8) -> Result<()> {
    if !fits_lanes(n, width) || bytes.len() != lanes_len(n, width) {
        return Err(StrataError::Corrupt {
            what: "lanes values",
            offset: 0,
        });
    }
    Ok(())
}

/// Writes `f` of each of the `n` values of `width` bits in lanes in `bytes` into `out`, in
/// order. A wrong length, width or count is `Corrupt`, and `out` is then left as it was.
pub fn extend_lanes<T>(
    out: &mut Vec<T>,
    bytes: &[u8],
    n: usize,
    width: u8,
    f: impl FnMut(u64) -> T,
) -> Result<()> {
    check_lanes(bytes, n, width)?;
    let mut deltas = vec![0u32; n];
    unpack_lanes(bytes, width, &mut deltas);
    out.extend(deltas.into_iter().map(u64::from).map(f));
    Ok(())
}

/// Writes `f` of the values at positions `at` (each below `n`) of the `n` values of `width` bits
/// in lanes in `bytes` into `out`, in the order of `at`. A wrong length or a position past `n`
/// is `Corrupt`, and `out` is then left as it was.
pub fn pick_lanes<T>(
    out: &mut Vec<T>,
    bytes: &[u8],
    n: usize,
    width: u8,
    at: &[u32],
    mut f: impl FnMut(u64) -> T,
) -> Result<()> {
    check_lanes(bytes, n, width)?;
    if at.iter().any(|&i| i as usize >= n) {
        return Err(StrataError::Corrupt {
            what: "lanes position",
            offset: 0,
        });
    }
    let w = usize::from(width);
    let m = mask32(w);
    let word = |index: usize| {
        bytes
            .get(index * 4..)
            .and_then(<[u8]>::first_chunk::<4>)
            .map_or(0, |b| u32::from_le_bytes(*b))
    };
    out.reserve(at.len());
    for &i in at {
        let i = i as usize;
        let (block, j) = (i / BLOCK, i % BLOCK);
        let (row, lane) = (j / LANES, j % LANES);
        let bit = row * w;
        let (k, s) = (bit / 32, bit % 32);
        let first = block * w * LANES;
        let mut v = word(first + k * LANES + lane) >> s;
        if s + w > 32 {
            v |= word(first + (k + 1) * LANES + lane) << (32 - s);
        }
        out.push(f(u64::from(v & m)));
    }
    Ok(())
}

/// The words of one row of a block, one per lane, as stored.
type Row = [[u8; 4]; LANES];

/// An output value of the unpack kernel: `base + delta`, wrapping, at the column's width. The
/// kernel adds the chunk's base as it unpacks, so a decode makes one pass.
pub(crate) trait LaneOut: Copy + Default {
    fn add(base: Self, delta: u32) -> Self;
}

impl LaneOut for u32 {
    #[inline(always)]
    fn add(base: Self, delta: u32) -> Self {
        base.wrapping_add(delta)
    }
}

impl LaneOut for u64 {
    #[inline(always)]
    fn add(base: Self, delta: u32) -> Self {
        base.wrapping_add(u64::from(delta))
    }
}

/// The 32 deltas of row `r` of a block of `W`-bit values, given to `f` lane by lane.
#[inline(always)]
fn row_deltas<const W: usize>(block: &[Row; W], r: usize, mut f: impl FnMut(usize, u32)) {
    let m = mask32(W);
    let bit = r * W;
    let (k, s) = (bit / 32, bit % 32);
    let Some(lo) = block.get(k) else {
        return;
    };
    match block.get(k + 1) {
        Some(hi) if s + W > 32 => {
            for (l, (lo, hi)) in lo.iter().zip(hi).enumerate() {
                let (lo, hi) = (u32::from_le_bytes(*lo), u32::from_le_bytes(*hi));
                f(l, ((lo >> s) | (hi << (32 - s))) & m);
            }
        }
        _ => {
            for (l, lo) in lo.iter().enumerate() {
                f(l, (u32::from_le_bytes(*lo) >> s) & m);
            }
        }
    }
}

/// Unpacks one block of `W`-bit values plus `base`: `block` is its `W` rows of words, `out` its
/// 32 rows of values.
#[inline(always)]
fn unpack_one_block<const W: usize, O: LaneOut>(
    block: &[Row; W],
    out: &mut [[O; LANES]; LANES],
    base: O,
) {
    for (r, values) in out.iter_mut().enumerate() {
        row_deltas::<W>(block, r, |l, d| {
            if let Some(v) = values.get_mut(l) {
                *v = O::add(base, d);
            }
        });
    }
}

/// Unpacks every block of `rows` (`W` rows a block) plus `base` into `out` (32 rows a block).
#[inline(always)]
fn unpack_all<const W: usize, O: LaneOut>(rows: &[Row], out: &mut [[O; LANES]], base: O) {
    for (block, out) in rows
        .as_chunks::<W>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<LANES>().0)
    {
        unpack_one_block::<W, O>(block, out, base);
    }
}

/// [`unpack_all`] compiled with AVX2.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn unpack_all_avx2<const W: usize, O: LaneOut>(rows: &[Row], out: &mut [[O; LANES]], base: O) {
    unpack_all::<W, O>(rows, out, base);
}

/// The bitmap words of `lo <= delta <= lo + len` for every block of `rows`: one word a row, bit
/// `l` for lane `l`, which is the row's 32 values in order.
#[inline(always)]
fn mask_all<const W: usize>(rows: &[Row], out: &mut [u32], lo: u32, len: u32) {
    for (block, words) in rows
        .as_chunks::<W>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<LANES>().0)
    {
        for (r, word) in words.iter_mut().enumerate() {
            let mut bits = [0u32; LANES];
            row_deltas::<W>(block, r, |l, d| {
                if let Some(b) = bits.get_mut(l) {
                    *b = u32::from(d.wrapping_sub(lo) <= len);
                }
            });
            *word = bits
                .iter()
                .enumerate()
                .fold(0, |word, (l, &b)| word | (b << l));
        }
    }
}

/// [`mask_all`] compiled with AVX2.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn mask_all_avx2<const W: usize>(rows: &[Row], out: &mut [u32], lo: u32, len: u32) {
    mask_all::<W>(rows, out, lo, len);
}

/// Which compiled kernel [`unpack_lanes`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kernel {
    /// The baseline target's instructions.
    Base,
    /// AVX2, when the CPU has it.
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

/// The fastest kernel this CPU runs. `is_x86_feature_detected!` caches its answer.
fn best_kernel() -> Kernel {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        return Kernel::Avx2;
    }
    Kernel::Base
}

/// Unpacks the values of `bytes` (checked by [`check_lanes`]) of `width` bits into `out`, one
/// per value, on the fastest kernel of this CPU.
fn unpack_lanes(bytes: &[u8], width: u8, out: &mut [u32]) {
    unpack_with(best_kernel(), bytes, width, out, 0);
}

/// [`unpack_lanes`] on a given kernel, adding `base` to each value. Its one `unsafe` call runs
/// the AVX2 kernel, which is only passed when the CPU has AVX2.
#[allow(unsafe_code)]
fn unpack_with<O: LaneOut>(kernel: Kernel, bytes: &[u8], width: u8, out: &mut [O], base: O) {
    let rows = bytes.as_chunks::<4>().0.as_chunks::<LANES>().0;
    let out = out.as_chunks_mut::<LANES>().0;
    macro_rules! widths {
        ($($w:literal)*) => {
            match width {
                $($w => match kernel {
                    Kernel::Base => unpack_all::<$w, O>(rows, out, base),
                    // SAFETY: `Kernel::Avx2` comes from `best_kernel`, which checked that the CPU
                    // has AVX2 (or from a test that checks it), so its instructions can run.
                    #[cfg(target_arch = "x86_64")]
                    Kernel::Avx2 => unsafe { unpack_all_avx2::<$w, O>(rows, out, base) },
                },)*
                _ => {}
            }
        };
    }
    widths!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
}

/// The bitmap words of `lo <= delta <= lo + len` over values of `bytes` (checked by
/// [`check_lanes`]) of `width` bits, one word per 32 values, on a given kernel. Its one `unsafe`
/// call runs the AVX2 kernel, which is only passed when the CPU has AVX2.
#[allow(unsafe_code)]
fn mask_with(kernel: Kernel, bytes: &[u8], width: u8, out: &mut [u32], lo: u32, len: u32) {
    let rows = bytes.as_chunks::<4>().0.as_chunks::<LANES>().0;
    macro_rules! widths {
        ($($w:literal)*) => {
            match width {
                $($w => match kernel {
                    Kernel::Base => mask_all::<$w>(rows, out, lo, len),
                    // SAFETY: as in `unpack_with`.
                    #[cfg(target_arch = "x86_64")]
                    Kernel::Avx2 => unsafe { mask_all_avx2::<$w>(rows, out, lo, len) },
                },)*
                _ => {}
            }
        };
    }
    widths!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
}

/// The `n` values of `width` bits in lanes in `bytes`, each plus `base`, appended to `out` in
/// one pass. A wrong length, width or count is `Corrupt`, and `out` is then left as it was.
pub(crate) fn extend_lanes_plus<O: LaneOut>(
    out: &mut Vec<O>,
    bytes: &[u8],
    n: usize,
    width: u8,
    base: O,
) -> Result<()> {
    check_lanes(bytes, n, width)?;
    let start = out.len();
    out.resize(start + n, O::default());
    if let Some(tail) = out.get_mut(start..) {
        unpack_with(best_kernel(), bytes, width, tail, base);
    }
    Ok(())
}

/// The bitmap of `lo <= delta <= lo + len` over the `n` values of `width` bits in lanes in
/// `bytes`: one `u32` word per 32 values, value `i` at bit `i % 32` of word `i / 32`. A wrong
/// length, width or count is `Corrupt`.
pub(crate) fn range_words(
    bytes: &[u8],
    n: usize,
    width: u8,
    lo: u32,
    len: u32,
) -> Result<Vec<u32>> {
    check_lanes(bytes, n, width)?;
    let mut words = vec![0u32; n / LANES];
    mask_with(best_kernel(), bytes, width, &mut words, lo, len);
    Ok(words)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// `n` values below `2^width`, from a fixed pseudo-random sequence.
    fn values(n: usize, width: u8) -> Vec<u64> {
        let m = u64::from(mask32(usize::from(width)));
        (0..n as u64)
            .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(23) & m)
            .collect()
    }

    /// The kernels this CPU can run.
    fn kernels() -> Vec<Kernel> {
        let mut out = vec![Kernel::Base];
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            out.push(Kernel::Avx2);
        }
        out
    }

    #[test]
    fn every_width_and_kernel_round_trips() {
        for width in 1u8..=32 {
            for blocks in [1usize, 2, 3] {
                let n = blocks * BLOCK;
                let v = values(n, width);
                let mut bytes = Vec::new();
                pack_lanes(&v, width, &mut bytes);
                assert_eq!(bytes.len(), lanes_len(n, width));
                for kernel in kernels() {
                    let mut out = vec![0u32; n];
                    unpack_with(kernel, &bytes, width, &mut out, 0);
                    let got: Vec<u64> = out.into_iter().map(u64::from).collect();
                    assert_eq!(got, v, "width {width}, {blocks} blocks, {kernel:?}");
                    // With a base, in 32 and 64 bits, wrapping.
                    let base32 = u32::MAX - 7;
                    let mut out = vec![0u32; n];
                    unpack_with(kernel, &bytes, width, &mut out, base32);
                    for (got, &want) in out.iter().zip(&v) {
                        assert_eq!(*got, base32.wrapping_add(want as u32));
                    }
                    let base64 = (1u64 << 40) + 3;
                    let mut out = vec![0u64; n];
                    unpack_with(kernel, &bytes, width, &mut out, base64);
                    for (got, &want) in out.iter().zip(&v) {
                        assert_eq!(*got, base64 + want);
                    }
                    // A range test, on bounds inside, at and around the values.
                    let top = mask32(usize::from(width));
                    for (lo, len) in [(0, 0), (0, top), (1, top / 2), (top / 3, 5), (top, 0)] {
                        let mut words = vec![0u32; n / LANES];
                        mask_with(kernel, &bytes, width, &mut words, lo, len);
                        for (i, &x) in v.iter().enumerate() {
                            let bit = words[i / LANES] >> (i % LANES) & 1 == 1;
                            let want = (x as u32).wrapping_sub(lo) <= len;
                            assert_eq!(bit, want, "width {width} {lo}+{len} row {i} {kernel:?}");
                        }
                    }
                }
                let mut out = Vec::new();
                extend_lanes(&mut out, &bytes, n, width, |x| x).unwrap();
                assert_eq!(out, v);
                let at: Vec<u32> = [0, 1, 31, 32, 33, 1023, n - 1, 517, 517]
                    .into_iter()
                    .map(|i| i as u32)
                    .collect();
                let mut picked = Vec::new();
                pick_lanes(&mut picked, &bytes, n, width, &at, |x| x).unwrap();
                let want: Vec<u64> = at.iter().map(|&i| v[i as usize]).collect();
                assert_eq!(picked, want, "width {width}");
            }
        }
    }

    #[test]
    fn the_layout_takes_the_bytes_of_plain_bit_packing() {
        for width in 1u8..=32 {
            let v = values(2 * BLOCK, width);
            let (mut lanes, mut packed) = (Vec::new(), Vec::new());
            pack_lanes(&v, width, &mut lanes);
            super::super::bitpack::pack(&v, width, &mut packed);
            assert_eq!(lanes.len(), packed.len(), "width {width}");
        }
    }

    #[test]
    fn only_whole_blocks_of_1_to_32_bits_fit() {
        assert!(fits_lanes(1024, 1));
        assert!(fits_lanes(2048, 32));
        assert!(!fits_lanes(1024, 0));
        assert!(!fits_lanes(1024, 33));
        assert!(!fits_lanes(1000, 8));
        assert!(!fits_lanes(0, 8));
    }

    #[test]
    fn wrong_lengths_and_positions_are_errors_that_write_nothing() {
        let v = values(BLOCK, 9);
        let mut bytes = Vec::new();
        pack_lanes(&v, 9, &mut bytes);
        let mut out: Vec<u64> = Vec::new();
        assert!(extend_lanes(&mut out, &bytes[..bytes.len() - 1], BLOCK, 9, |x| x).is_err());
        assert!(extend_lanes(&mut out, &bytes, BLOCK, 10, |x| x).is_err());
        assert!(extend_lanes(&mut out, &bytes, 2 * BLOCK, 9, |x| x).is_err());
        assert!(pick_lanes(&mut out, &bytes, BLOCK, 9, &[3, 1024], |x| x).is_err());
        assert!(out.is_empty());
    }
}
