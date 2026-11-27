//! VLDB-2020 FSST (Fast Static Symbol Table) string & binary codec.
//!
//! Trains a static symbol table of at most 255 symbols (`1..=8` bytes each)
//! using 3-round gain-based training on a bounded sample (`<= 64 KiB`),
//! encodes rows using greedy longest-match with `0xFF` literal escapes and
//! FastLanes-packed per-row compressed lengths, and supports both full-block
//! decode and O(1) single-row `decode_row`.

use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasherDefault;

use cetra_column::{BinaryData, Bitmap, Utf8Data};

use crate::codec::WriteProfile;
use crate::format::{Codec, MAX_DECOMPRESS_RATIO, MAX_DECOMPRESS_SLACK, check_decompress_bounds};
use crate::simd::bitpack::{min_bits_u32, pack_u32, packed_bytes_len, unpack_u32};
use crate::{MAX_BLOCK_RAW_BYTES, StatamError};

/// Literal escape byte (`255`); valid symbol codes are `0..sym_count` with `sym_count <= 255`.
pub const FSST_ESCAPE: u8 = 0xFF;

/// Maximum symbols in an FSST symbol table.
pub const FSST_MAX_SYMBOLS: usize = 255;

/// Maximum length of a single FSST symbol in bytes.
pub const FSST_MAX_SYM_LEN: usize = 8;

/// Target row count per adaptive `LZ4`/`Zstd`/`None` sub-chunk inside `Encoding::Fsst`
/// when `BLOCK_FLAG_FSST_SUBCHUNKED` is set.
pub const FSST_SUBCHUNK_ROWS: u16 = 512;

/// Minimum row count (`> 512`) required to trigger sub-chunked FSST encoding.
const FSST_SUBCHUNK_MIN_ROWS: usize = 512;

/// Minimum FSST-coded payload byte length (`>= 16 KiB`) required to trigger sub-chunked encoding.
const FSST_SUBCHUNK_MIN_CODED_BYTES: usize = 16 * 1024;

/// Hard wire upper bound on `subchunk_rows` in a sub-chunked FSST header.
const FSST_SUBCHUNK_MAX_ROWS: u16 = 8_192;

/// Byte length of one sub-chunk directory entry (`codec: u8` + `raw_len: u32` + `stored_end: u32`).
const FSST_SUBCHUNK_ENTRY_LEN: usize = 9;

/// Maximum sample bytes consumed during FSST training.
const FSST_SAMPLE_BYTES: usize = 64 * 1024;

/// Flat L1-resident multi-byte symbol lookup entry inside [`FsstTable`].
#[derive(Clone, Copy, PartialEq, Eq)]
struct BucketEntry {
    word: u64,
    mask: u64,
    code: u8,
    len: u8,
}

/// Trained FSST static symbol table (`<= 255` symbols of `1..=8` bytes).
#[derive(Clone, PartialEq, Eq)]
pub struct FsstTable {
    sym_lens: Vec<u8>,
    sym_bytes: Vec<u8>,
    sym_offsets: Vec<u16>,
    len1_code: [u8; 256],
    bucket_offsets: [u16; 257],
    bucket_entries: Vec<BucketEntry>,
}

impl fmt::Debug for FsstTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsstTable")
            .field("sym_count", &self.sym_lens.len())
            .field("sym_bytes_len", &self.sym_bytes.len())
            .finish()
    }
}

#[derive(Clone, Copy)]
struct SymEntry {
    len: u8,
    bytes: [u8; FSST_MAX_SYM_LEN],
}

impl FsstTable {
    /// Builds an empty FSST table (every byte escaped).
    pub fn empty() -> Self {
        Self {
            sym_lens: Vec::new(),
            sym_bytes: Vec::new(),
            sym_offsets: vec![0],
            len1_code: [FSST_ESCAPE; 256],
            bucket_offsets: [0u16; 257],
            bucket_entries: Vec::new(),
        }
    }

    fn from_entries(entries: &[SymEntry]) -> Self {
        let count = entries.len().min(FSST_MAX_SYMBOLS);
        let mut sym_lens = Vec::with_capacity(count);
        let mut sym_offsets = Vec::with_capacity(count + 1);
        let mut sym_bytes = Vec::with_capacity(count * 4);
        let mut len1_code = [FSST_ESCAPE; 256];
        let mut counts = [0u16; 256];

        sym_offsets.push(0);
        for (code, entry) in entries[..count].iter().enumerate() {
            let len = entry.len;
            debug_assert!((1..=FSST_MAX_SYM_LEN as u8).contains(&len));
            let len_usize = usize::from(len);
            sym_lens.push(len);
            sym_bytes.extend_from_slice(&entry.bytes[..len_usize]);
            sym_offsets.push(sym_bytes.len() as u16);

            let code_u8 = code as u8;
            if len == 1 {
                len1_code[usize::from(entry.bytes[0])] = code_u8;
            } else {
                counts[usize::from(entry.bytes[0])] += 1;
            }
        }

        let mut bucket_offsets = [0u16; 257];
        let mut off = 0u16;
        for b in 0..256 {
            bucket_offsets[b] = off;
            off += counts[b];
        }
        bucket_offsets[256] = off;

        let mut bucket_entries = vec![
            BucketEntry {
                word: 0,
                mask: 0,
                code: 0,
                len: 0,
            };
            usize::from(off)
        ];
        let mut write_pos = bucket_offsets;
        for (code, entry) in entries[..count].iter().enumerate() {
            let len = entry.len;
            if len < 2 {
                continue;
            }
            let len_usize = usize::from(len);
            let mut word_bytes = [0u8; FSST_MAX_SYM_LEN];
            word_bytes[..len_usize].copy_from_slice(&entry.bytes[..len_usize]);
            let word = u64::from_le_bytes(word_bytes);
            let mask = if len_usize == FSST_MAX_SYM_LEN {
                u64::MAX
            } else {
                (1u64 << (len_usize * 8)) - 1
            };
            let b = usize::from(entry.bytes[0]);
            let idx = usize::from(write_pos[b]);
            write_pos[b] += 1;
            bucket_entries[idx] = BucketEntry {
                word,
                mask,
                code: code as u8,
                len,
            };
        }

        // Stable-sort each bucket by descending symbol length so first match is longest
        // while preserving exact ascending-code tie-breaking.
        for b in 0..256 {
            let start = usize::from(bucket_offsets[b]);
            let end = usize::from(bucket_offsets[b + 1]);
            if end - start > 1 {
                bucket_entries[start..end].sort_by_key(|a| std::cmp::Reverse(a.len));
            }
        }

        Self {
            sym_lens,
            sym_bytes,
            sym_offsets,
            len1_code,
            bucket_offsets,
            bucket_entries,
        }
    }

    /// Number of symbols in the table (`0..=255`).
    pub fn sym_count(&self) -> usize {
        self.sym_lens.len()
    }

    #[inline]
    fn symbol_slice(&self, code: usize) -> &[u8] {
        let start = usize::from(self.sym_offsets[code]);
        let end = usize::from(self.sym_offsets[code + 1]);
        &self.sym_bytes[start..end]
    }

    #[inline]
    fn find_longest_match(&self, input: &[u8]) -> Option<(u8, usize)> {
        let first = *input.first()?;
        let first_idx = usize::from(first);
        let start = usize::from(self.bucket_offsets[first_idx]);
        let end = usize::from(self.bucket_offsets[first_idx + 1]);
        if start < end && input.len() >= 2 {
            let bucket = &self.bucket_entries[start..end];
            if input.len() >= 8 {
                let in_word = u64::from_le_bytes([
                    input[0], input[1], input[2], input[3], input[4], input[5], input[6], input[7],
                ]);
                for entry in bucket {
                    if (in_word & entry.mask) == entry.word {
                        return Some((entry.code, usize::from(entry.len)));
                    }
                }
            } else {
                let mut buf = [0u8; FSST_MAX_SYM_LEN];
                buf[..input.len()].copy_from_slice(input);
                let in_word = u64::from_le_bytes(buf);
                for entry in bucket {
                    let slen = usize::from(entry.len);
                    if slen <= input.len() && (in_word & entry.mask) == entry.word {
                        return Some((entry.code, slen));
                    }
                }
            }
        }
        let c1 = self.len1_code[first_idx];
        if c1 != FSST_ESCAPE {
            Some((c1, 1))
        } else {
            None
        }
    }

    /// Trains a static symbol table using 3-round gain-based selection over a
    /// bounded (`<= 64 KiB`) sample of `inputs`.
    pub fn train(inputs: &[&[u8]]) -> Self {
        let mut samples: Vec<&[u8]> = Vec::new();
        let mut total_sampled = 0usize;
        for &s in inputs {
            if s.is_empty() {
                continue;
            }
            if total_sampled >= FSST_SAMPLE_BYTES {
                break;
            }
            let take_len = s.len().min(FSST_SAMPLE_BYTES - total_sampled);
            samples.push(&s[..take_len]);
            total_sampled += take_len;
        }
        if samples.is_empty() {
            return Self::empty();
        }

        let mut round0_end = 0usize;
        let mut round0_bytes = 0usize;
        while round0_end < samples.len() && round0_bytes < 16 * 1024 {
            round0_bytes += samples[round0_end].len();
            round0_end += 1;
        }
        let round0_samples = &samples[..round0_end.max(1)];

        let mut count2: HashMap<u32, u32, BuildHasherDefault<FastU32Hasher>> =
            HashMap::with_capacity_and_hasher(4096, BuildHasherDefault::default());
        let mut candidates: HashMap<(u8, u64), u64, BuildHasherDefault<FastSymKeyHasher>> =
            HashMap::with_capacity_and_hasher(4096, BuildHasherDefault::default());

        let mut table = Self::empty();
        for round in 0..3 {
            let cur_samples = if round == 0 { round0_samples } else { &samples };
            table = train_round(&table, cur_samples, &mut count2, &mut candidates);
            if round == 1 && total_sampled >= 8 * 1024 {
                let mut probe_raw = 0usize;
                let mut probe_coded = Vec::with_capacity(2048);
                for &s in samples.iter().take(64) {
                    if probe_raw >= 4 * 1024 {
                        break;
                    }
                    probe_raw += s.len();
                    table.encode_row(s, &mut probe_coded);
                }
                if probe_raw > 0
                    && probe_raw.saturating_mul(100) < probe_coded.len().saturating_mul(104)
                {
                    return Self::empty();
                }
            }
        }

        // Final pruning pass: drop unused symbols and fill spare slots with frequent literals.
        finalize_table(&table, &samples)
    }

    /// Encodes a single row into `out`.
    pub fn encode_row(&self, input: &[u8], out: &mut Vec<u8>) {
        let mut pos = 0usize;
        let len = input.len();
        while pos + 8 <= len {
            let first = input[pos];
            let first_idx = usize::from(first);
            let start = usize::from(self.bucket_offsets[first_idx]);
            let end = usize::from(self.bucket_offsets[first_idx + 1]);
            let mut matched = false;
            if start < end {
                let in_word = u64::from_le_bytes([
                    input[pos],
                    input[pos + 1],
                    input[pos + 2],
                    input[pos + 3],
                    input[pos + 4],
                    input[pos + 5],
                    input[pos + 6],
                    input[pos + 7],
                ]);
                for entry in &self.bucket_entries[start..end] {
                    if (in_word & entry.mask) == entry.word {
                        out.push(entry.code);
                        pos += usize::from(entry.len);
                        matched = true;
                        break;
                    }
                }
            }
            if !matched {
                let c1 = self.len1_code[first_idx];
                if c1 != FSST_ESCAPE {
                    out.push(c1);
                } else {
                    out.push(FSST_ESCAPE);
                    out.push(first);
                }
                pos += 1;
            }
        }
        while pos < len {
            if let Some((code, mlen)) = self.find_longest_match(&input[pos..]) {
                out.push(code);
                pos += mlen;
            } else {
                out.push(FSST_ESCAPE);
                out.push(input[pos]);
                pos += 1;
            }
        }
    }

    fn write_header(&self, out: &mut Vec<u8>) {
        out.push(self.sym_count() as u8);
        out.extend_from_slice(&self.sym_lens);
        out.extend_from_slice(&self.sym_bytes);
    }
}

#[derive(Default)]
struct FastU32Hasher(u64);

impl std::hash::Hasher for FastU32Hasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut v = 0u64;
        for &b in bytes {
            v = v.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ u64::from(b);
        }
        self.0 = v;
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.0 = u64::from(i).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

#[derive(Default)]
struct FastSymKeyHasher(u64);

impl std::hash::Hasher for FastSymKeyHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
        self.0 = h;
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.0 = (self.0 ^ u64::from(i)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0 ^ i).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

fn train_round(
    table: &FsstTable,
    samples: &[&[u8]],
    count2: &mut HashMap<u32, u32, BuildHasherDefault<FastU32Hasher>>,
    candidates: &mut HashMap<(u8, u64), u64, BuildHasherDefault<FastSymKeyHasher>>,
) -> FsstTable {
    let mut count1 = [0u32; 512];
    count2.clear();
    candidates.clear();

    if table.sym_count() == 0 {
        for &sample in samples {
            let mut prev_token: Option<u16> = None;
            for &b in sample {
                let token = 256 + u16::from(b);
                let slot = &mut count1[usize::from(token)];
                *slot = slot.saturating_add(1);
                if let Some(pt) = prev_token {
                    let key = (u32::from(pt) << 10) | u32::from(token);
                    let e = count2.entry(key).or_insert(0);
                    *e = e.saturating_add(1);
                }
                prev_token = Some(token);
            }
        }
    } else {
        for &sample in samples {
            let mut pos = 0usize;
            let mut prev_token: Option<(u16, u8)> = None;
            while pos < sample.len() {
                let (token, len) =
                    if let Some((code, mlen)) = table.find_longest_match(&sample[pos..]) {
                        (u16::from(code), mlen as u8)
                    } else {
                        (256 + u16::from(sample[pos]), 1u8)
                    };
                count1[usize::from(token)] = count1[usize::from(token)].saturating_add(1);
                if let Some((pt, plen)) = prev_token
                    && usize::from(plen) + usize::from(len) <= FSST_MAX_SYM_LEN
                {
                    let key = (u32::from(pt) << 10) | u32::from(token);
                    let e = count2.entry(key).or_insert(0);
                    *e = e.saturating_add(1);
                }
                prev_token = Some((token, len));
                pos += usize::from(len);
            }
        }
    }

    let mut tok_word = [0u64; 512];
    let mut tok_len = [0u8; 512];
    for code in 0..table.sym_count() {
        let s = table.symbol_slice(code);
        let mut buf = [0u8; FSST_MAX_SYM_LEN];
        buf[..s.len()].copy_from_slice(s);
        tok_word[code] = u64::from_le_bytes(buf);
        tok_len[code] = s.len() as u8;
    }
    for b in 0..256usize {
        tok_word[256 + b] = b as u64;
        tok_len[256 + b] = 1;
    }

    // Collect deduplicated candidate symbols with their estimated byte gain.
    for (tok, &cnt) in count1.iter().enumerate() {
        if cnt == 0 {
            continue;
        }
        let len = tok_len[tok];
        let word = tok_word[tok];
        // Each hit replaces `len` raw bytes (or `2` escaped bytes) with a 1-byte code.
        let gain = u64::from(cnt) * u64::from(len);
        let e = candidates.entry((len, word)).or_insert(0);
        if gain > *e {
            *e = gain;
        }
    }

    for (&pair_key, &cnt) in count2.iter() {
        if cnt < 2 {
            continue;
        }
        let pt = (pair_key >> 10) as usize;
        let ct = (pair_key & 0x3FF) as usize;
        let plen = tok_len[pt];
        let clen = tok_len[ct];
        let combined_len = plen + clen;
        if usize::from(combined_len) > FSST_MAX_SYM_LEN {
            continue;
        }
        let combined_word = tok_word[pt] | (tok_word[ct] << (u32::from(plen) * 8));
        let gain = u64::from(cnt) * u64::from(combined_len);
        let e = candidates.entry((combined_len, combined_word)).or_insert(0);
        if gain > *e {
            *e = gain;
        }
    }

    let cmp = |a: &((u8, u64), u64), b: &((u8, u64), u64)| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.0.cmp(&a.0.0))
            .then_with(|| a.0.1.swap_bytes().cmp(&b.0.1.swap_bytes()))
    };
    let mut ranked: Vec<((u8, u64), u64)> = candidates.drain().collect();
    if ranked.len() > FSST_MAX_SYMBOLS {
        ranked.select_nth_unstable_by(FSST_MAX_SYMBOLS, cmp);
        ranked.truncate(FSST_MAX_SYMBOLS);
    }
    ranked.sort_unstable_by(cmp);

    let entries: Vec<SymEntry> = ranked
        .into_iter()
        .map(|((len, word), _)| SymEntry {
            len,
            bytes: word.to_le_bytes(),
        })
        .collect();
    FsstTable::from_entries(&entries)
}

fn finalize_table(table: &FsstTable, samples: &[&[u8]]) -> FsstTable {
    let mut sym_hits = vec![0u32; table.sym_count()];
    let mut lit_hits = [0u32; 256];

    for &sample in samples {
        let mut pos = 0usize;
        while pos < sample.len() {
            if let Some((code, len)) = table.find_longest_match(&sample[pos..]) {
                sym_hits[usize::from(code)] = sym_hits[usize::from(code)].saturating_add(1);
                pos += len;
            } else {
                lit_hits[usize::from(sample[pos])] =
                    lit_hits[usize::from(sample[pos])].saturating_add(1);
                pos += 1;
            }
        }
    }

    let total_sample_bytes: usize = samples.iter().map(|s| s.len()).sum();
    let min_multi_gain = (total_sample_bytes / 1024).clamp(16, 64);

    let mut entries = Vec::with_capacity(FSST_MAX_SYMBOLS);
    let mut has_len1 = [false; 256];
    for (code, &hits) in sym_hits.iter().enumerate() {
        if hits == 0 {
            continue;
        }
        let sl = table.symbol_slice(code);
        if sl.len() >= 2 && (hits as usize).saturating_mul(sl.len() - 1) < min_multi_gain {
            for &b in sl {
                lit_hits[usize::from(b)] = lit_hits[usize::from(b)].saturating_add(hits);
            }
            continue;
        }
        let mut bytes = [0u8; FSST_MAX_SYM_LEN];
        bytes[..sl.len()].copy_from_slice(sl);
        if sl.len() == 1 {
            has_len1[usize::from(sl[0])] = true;
        }
        entries.push(SymEntry {
            len: sl.len() as u8,
            bytes,
        });
        if entries.len() == FSST_MAX_SYMBOLS {
            break;
        }
    }

    if entries.len() < FSST_MAX_SYMBOLS {
        let mut lits: Vec<(u8, u32)> = lit_hits
            .iter()
            .enumerate()
            .filter_map(|(b, &cnt)| (cnt >= 2 && !has_len1[b]).then_some((b as u8, cnt)))
            .collect();
        lits.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (b, _) in lits {
            if entries.len() >= FSST_MAX_SYMBOLS {
                break;
            }
            let mut bytes = [0u8; FSST_MAX_SYM_LEN];
            bytes[0] = b;
            entries.push(SymEntry { len: 1, bytes });
        }
    }

    FsstTable::from_entries(&entries)
}

/// Encodes `values` (without leading row null bitmap) using single-chunk FSST.
///
/// Returns `Ok(None)` if `values` is empty, has zero raw bytes, or achieves a
/// compression ratio `< 1.15x` against plain offsets+bytes representation.
pub fn encode_fsst_body(values: &[&[u8]]) -> Result<Option<Vec<u8>>, StatamError> {
    encode_fsst_body_impl(values, None).map(|opt| opt.map(|(buf, _)| buf))
}

/// Encodes `values` (without leading row null bitmap) using FSST, switching to
/// `512`-row adaptive `LZ4`/`Zstd`/`None` sub-chunks (`bool == true`) when
/// `values.len() > 512`, `coded_bytes.len() >= 16_384`, and (for `L1Compact`)
/// sub-chunked output is within `+10%` of whole-block Zstd.
///
/// The existing `>= 1.15x` FSST gain threshold is checked BEFORE sub-chunk
/// compression so strings that do not benefit from FSST still fall back to `Plain`.
pub(crate) fn encode_fsst_body_adaptive(
    values: &[&[u8]],
    profile: WriteProfile,
) -> Result<Option<(Vec<u8>, bool)>, StatamError> {
    encode_fsst_body_impl(values, Some(profile))
}

fn encode_fsst_body_impl(
    values: &[&[u8]],
    subchunk_profile: Option<WriteProfile>,
) -> Result<Option<(Vec<u8>, bool)>, StatamError> {
    if values.is_empty() {
        return Ok(None);
    }
    let raw_data_bytes: usize = values.iter().map(|s| s.len()).sum();
    if raw_data_bytes == 0 {
        return Ok(None);
    }

    let table = FsstTable::train(values);
    if table.sym_count() == 0 {
        return Ok(None);
    }

    // Fast-reject incompressible columns (e.g. W4 random IDs) on a 64-row sample before
    // encoding the entire mini-block.
    if values.len() > 128 {
        let mut sample_raw = 0usize;
        let mut sample_coded = Vec::with_capacity(4096);
        let step = (values.len() / 64).max(1);
        for &row in values.iter().step_by(step).take(64) {
            if !row.is_empty() {
                sample_raw += row.len();
                table.encode_row(row, &mut sample_coded);
            }
        }
        if sample_raw > 0 && sample_raw.saturating_mul(100) < sample_coded.len().saturating_mul(112)
        {
            return Ok(None);
        }
    }

    let mut row_lens = Vec::with_capacity(values.len());
    let mut coded_bytes = Vec::with_capacity(raw_data_bytes / 2 + 16);
    for &row in values {
        let start = coded_bytes.len();
        table.encode_row(row, &mut coded_bytes);
        let rlen =
            u32::try_from(coded_bytes.len() - start).map_err(|_| StatamError::BoundsExceeded {
                reason: "fsst compressed row length exceeds u32::MAX",
            })?;
        row_lens.push(rlen);
    }

    let header_len = 1 + table.sym_count() + table.sym_bytes.len();
    let len_bw = min_bits_u32(&row_lens);
    let packed_lens_bytes = packed_bytes_len(values.len(), len_bw)?;
    let single_chunk_len = header_len
        .saturating_add(1)
        .saturating_add(packed_lens_bytes)
        .saturating_add(coded_bytes.len());

    // Require >= 1.15x FSST compression over raw string payload bytes BEFORE sub-chunking.
    if raw_data_bytes.saturating_mul(100) < single_chunk_len.saturating_mul(115) {
        return Ok(None);
    }

    if let Some(profile) = subchunk_profile
        && values.len() > FSST_SUBCHUNK_MIN_ROWS
        && coded_bytes.len() >= FSST_SUBCHUNK_MIN_CODED_BYTES
    {
        let subchunk_rows = usize::from(FSST_SUBCHUNK_ROWS);
        let num_chunks = values.len().div_ceil(subchunk_rows);
        if let Ok(num_chunks_u16) = u16::try_from(num_chunks) {
            let (out, is_subchunked) = encode_fsst_subchunked(
                &table,
                &row_lens,
                len_bw,
                single_chunk_len,
                &coded_bytes,
                FSST_SUBCHUNK_ROWS,
                num_chunks_u16,
                profile,
            )?;
            return Ok(Some((out, is_subchunked)));
        }
    }

    let mut out = Vec::with_capacity(single_chunk_len);
    table.write_header(&mut out);
    out.push(len_bw);
    pack_u32(&row_lens, len_bw, &mut out)?;
    out.extend_from_slice(&coded_bytes);

    Ok(Some((out, false)))
}

#[allow(clippy::too_many_arguments)]
fn encode_fsst_subchunked(
    table: &FsstTable,
    row_lens: &[u32],
    len_bw: u8,
    single_chunk_len: usize,
    coded_bytes: &[u8],
    subchunk_rows: u16,
    num_chunks: u16,
    profile: WriteProfile,
) -> Result<(Vec<u8>, bool), StatamError> {
    let subchunk_rows_usize = usize::from(subchunk_rows);
    let num_chunks_usize = usize::from(num_chunks);
    let row_count = row_lens.len();

    #[cfg(not(miri))]
    let mut zstd_comp = if profile == WriteProfile::L1Compact {
        Some(
            zstd::bulk::Compressor::new(3).map_err(|_| StatamError::Corrupt {
                reason: "fsst subchunk zstd compressor init failed",
            })?,
        )
    } else {
        None
    };

    let mut entries: Vec<(Codec, u32, u32)> = Vec::with_capacity(num_chunks_usize);
    let mut payloads: Vec<u8> = Vec::with_capacity(coded_bytes.len());
    let mut sub_raw: Vec<u8> =
        Vec::with_capacity(coded_bytes.len() / num_chunks_usize.max(1) + 512);
    let mut coded_pos = 0usize;
    let mut total_sub_raw = 0usize;

    for chunk_idx in 0..num_chunks_usize {
        let row_start = chunk_idx * subchunk_rows_usize;
        let row_end = (row_start + subchunk_rows_usize).min(row_count);
        let sub_lens = &row_lens[row_start..row_end];
        let mut sub_coded_len = 0usize;
        for &l in sub_lens {
            sub_coded_len =
                sub_coded_len
                    .checked_add(l as usize)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "fsst subchunk coded length overflow",
                    })?;
        }
        let sub_coded = &coded_bytes[coded_pos..coded_pos + sub_coded_len];
        coded_pos += sub_coded_len;

        sub_raw.clear();
        let sub_bw = min_bits_u32(sub_lens);
        sub_raw.push(sub_bw);
        pack_u32(sub_lens, sub_bw, &mut sub_raw)?;
        sub_raw.extend_from_slice(sub_coded);

        let raw_len = sub_raw.len();
        total_sub_raw = total_sub_raw
            .checked_add(raw_len)
            .ok_or(StatamError::BoundsExceeded {
                reason: "fsst subchunk total raw length overflow",
            })?;
        if total_sub_raw > MAX_BLOCK_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "fsst subchunk total raw length exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        let raw_len_u32 = u32::try_from(raw_len).map_err(|_| StatamError::BoundsExceeded {
            reason: "fsst subchunk raw_len exceeds u32::MAX",
        })?;

        let ratio_ok = |stored_len: usize| -> bool {
            (raw_len as u64)
                <= (stored_len as u64)
                    .saturating_mul(MAX_DECOMPRESS_RATIO)
                    .saturating_add(MAX_DECOMPRESS_SLACK)
        };

        let lz4_buf = lz4_flex::block::compress(&sub_raw);
        let lz4_len = lz4_buf.len();
        let lz4_ok = ratio_ok(lz4_len)
            && (lz4_len as u64).saturating_mul(100) <= (raw_len as u64).saturating_mul(88);

        #[cfg(not(miri))]
        let zstd_buf: Option<Vec<u8>> = if let Some(comp) = zstd_comp.as_mut() {
            Some(comp.compress(&sub_raw).map_err(|_| StatamError::Corrupt {
                reason: "fsst subchunk zstd compression failed",
            })?)
        } else {
            None
        };
        #[cfg(miri)]
        let zstd_buf: Option<Vec<u8>> = None;

        let zstd_ok = zstd_buf.as_ref().is_some_and(|zb| {
            ratio_ok(zb.len())
                && (zb.len() as u64).saturating_mul(100) <= (raw_len as u64).saturating_mul(88)
        });

        let use_lz4 = lz4_ok
            && (profile == WriteProfile::L0Fast
                || cfg!(miri)
                || !zstd_ok
                || zstd_buf.as_ref().is_none_or(|zb| {
                    (lz4_len as u64).saturating_mul(100) <= (zb.len() as u64).saturating_mul(112)
                }));

        let chosen_codec = if use_lz4 {
            payloads.extend_from_slice(&lz4_buf);
            Codec::Lz4
        } else if zstd_ok && let Some(zb) = zstd_buf.as_ref() {
            payloads.extend_from_slice(zb);
            Codec::Zstd
        } else {
            payloads.extend_from_slice(&sub_raw);
            Codec::None
        };

        let stored_end =
            u32::try_from(payloads.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "fsst subchunk stored_end exceeds u32::MAX",
            })?;
        entries.push((chosen_codec, raw_len_u32, stored_end));
    }

    let header_len = 1 + table.sym_count() + table.sym_bytes.len();
    let total_out = header_len + 4 + num_chunks_usize * FSST_SUBCHUNK_ENTRY_LEN + payloads.len();
    if total_out > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "fsst subchunked block exceeds MAX_BLOCK_RAW_BYTES",
        });
    }

    #[cfg(not(miri))]
    if let Some(comp) = zstd_comp.as_mut() {
        let mut single_out = Vec::with_capacity(single_chunk_len);
        table.write_header(&mut single_out);
        single_out.push(len_bw);
        pack_u32(row_lens, len_bw, &mut single_out)?;
        single_out.extend_from_slice(coded_bytes);
        let whole_zstd_len = comp
            .compress(&single_out)
            .map_err(|_| StatamError::Corrupt {
                reason: "fsst whole-block zstd compression failed",
            })?
            .len();
        if (total_out as u64).saturating_mul(100) > (whole_zstd_len as u64).saturating_mul(110) {
            return Ok((single_out, false));
        }
    }
    #[cfg(miri)]
    let _ = (len_bw, single_chunk_len);

    let mut out = Vec::with_capacity(total_out);
    table.write_header(&mut out);
    out.extend_from_slice(&subchunk_rows.to_le_bytes());
    out.extend_from_slice(&num_chunks.to_le_bytes());
    for (codec, raw_len, stored_end) in entries {
        out.push(codec as u8);
        out.extend_from_slice(&raw_len.to_le_bytes());
        out.extend_from_slice(&stored_end.to_le_bytes());
    }
    out.extend_from_slice(&payloads);
    Ok((out, true))
}

/// Decoded `(offsets, raw_bytes, optional_null_bitmap)` returned by [`FsstBlockView::decode_selected`].
pub type FsstSelectedRows = (Vec<u32>, Vec<u8>, Option<Bitmap>);

/// Lightweight stack-allocated FSST decode table (`<= 255` symbols of `1..=8` bytes).
#[derive(Clone)]
struct FsstDecodeTable {
    words: [[u8; FSST_MAX_SYM_LEN]; 256],
    /// `1..=8` for valid symbol codes `0..sym_count`, `0xFF` for invalid codes `sym_count..256`.
    lens: [u8; 256],
    sym_count: u8,
}

impl fmt::Debug for FsstDecodeTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsstDecodeTable")
            .field("sym_count", &self.sym_count)
            .finish()
    }
}

impl FsstDecodeTable {
    fn empty() -> Self {
        Self {
            words: [[0u8; FSST_MAX_SYM_LEN]; 256],
            lens: [FSST_ESCAPE; 256],
            sym_count: 0,
        }
    }

    fn parse_header(src: &[u8]) -> Result<(Self, usize), StatamError> {
        let Some(&sym_count_u8) = src.first() else {
            return Err(StatamError::Corrupt {
                reason: "truncated fsst symbol count",
            });
        };
        let sym_count = usize::from(sym_count_u8);
        if sym_count > FSST_MAX_SYMBOLS {
            return Err(StatamError::Corrupt {
                reason: "fsst sym_count exceeds 255",
            });
        }
        let lens_end = 1 + sym_count;
        if src.len() < lens_end {
            return Err(StatamError::Corrupt {
                reason: "truncated fsst symbol lengths",
            });
        }
        let sym_lens_slice = &src[1..lens_end];
        let mut total_sym_bytes = 0usize;
        for &slen in sym_lens_slice {
            if !(1..=FSST_MAX_SYM_LEN as u8).contains(&slen) {
                return Err(StatamError::Corrupt {
                    reason: "invalid fsst symbol length (must be 1..=8)",
                });
            }
            total_sym_bytes += usize::from(slen);
        }
        let syms_end = lens_end + total_sym_bytes;
        if src.len() < syms_end {
            return Err(StatamError::Corrupt {
                reason: "truncated fsst symbol table bytes",
            });
        }

        let mut words = [[0u8; FSST_MAX_SYM_LEN]; 256];
        let mut lens = [FSST_ESCAPE; 256];
        let mut off = lens_end;
        for (code, &slen) in sym_lens_slice.iter().enumerate() {
            let len_usize = usize::from(slen);
            words[code][..len_usize].copy_from_slice(&src[off..off + len_usize]);
            lens[code] = slen;
            off += len_usize;
        }

        Ok((
            Self {
                words,
                lens,
                sym_count: sym_count_u8,
            },
            syms_end,
        ))
    }

    fn decode_row_checked(
        &self,
        coded: &[u8],
        out: &mut Vec<u8>,
        max_total_bytes: usize,
    ) -> Result<(), StatamError> {
        let mut i = 0usize;
        while i < coded.len() {
            let b = coded[i];
            if b == FSST_ESCAPE {
                if i + 1 >= coded.len() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated fsst literal escape",
                    });
                }
                if out.len() >= max_total_bytes {
                    return Err(StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                    });
                }
                out.push(coded[i + 1]);
                i += 2;
            } else {
                let slen = self.lens[usize::from(b)];
                if slen == FSST_ESCAPE {
                    return Err(StatamError::Corrupt {
                        reason: "fsst symbol code out of bounds",
                    });
                }
                let slen_usize = usize::from(slen);
                if out
                    .len()
                    .checked_add(slen_usize)
                    .is_none_or(|n| n > max_total_bytes)
                {
                    return Err(StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                    });
                }
                out.extend_from_slice(&self.words[usize::from(b)][..slen_usize]);
                i += 1;
            }
        }
        Ok(())
    }

    #[inline]
    fn decode_row_fast(
        &self,
        coded_row: &[u8],
        out_bytes: &mut Vec<u8>,
        pos: &mut usize,
        max_total_bytes: usize,
        grow_exponential: bool,
    ) -> Result<(), StatamError> {
        let max_row = coded_row.len().saturating_mul(FSST_MAX_SYM_LEN);
        if pos.saturating_add(max_row) <= max_total_bytes {
            let needed = pos.saturating_add(max_row).saturating_add(FSST_MAX_SYM_LEN);
            if needed > out_bytes.len() {
                let grown = if grow_exponential {
                    out_bytes
                        .len()
                        .saturating_mul(2)
                        .max(needed)
                        .min(max_total_bytes.saturating_add(FSST_MAX_SYM_LEN))
                } else {
                    needed
                };
                out_bytes.resize(grown, 0);
            }
            let out_slice = &mut out_bytes[..needed];
            let mut p = *pos;
            let mut i = 0usize;
            while i < coded_row.len() {
                let b = coded_row[i];
                if b == FSST_ESCAPE {
                    if i + 1 >= coded_row.len() {
                        return Err(StatamError::Corrupt {
                            reason: "truncated fsst literal escape",
                        });
                    }
                    out_slice[p] = coded_row[i + 1];
                    p += 1;
                    i += 2;
                } else {
                    let slen = self.lens[usize::from(b)];
                    if slen == FSST_ESCAPE {
                        return Err(StatamError::Corrupt {
                            reason: "fsst symbol code out of bounds",
                        });
                    }
                    out_slice[p..p + FSST_MAX_SYM_LEN].copy_from_slice(&self.words[usize::from(b)]);
                    p += usize::from(slen);
                    i += 1;
                }
            }
            *pos = p;
            Ok(())
        } else {
            out_bytes.truncate(*pos);
            self.decode_row_checked(coded_row, out_bytes, max_total_bytes)?;
            *pos = out_bytes.len();
            Ok(())
        }
    }
}

/// Zero-copy view over an FSST-encoded block supporting O(1) single-row decode.
#[derive(Clone)]
pub struct FsstBlockView<'a> {
    table: FsstDecodeTable,
    offsets: Vec<u32>,
    coded: &'a [u8],
}

impl fmt::Debug for FsstBlockView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsstBlockView")
            .field("row_count", &self.len())
            .field("table", &self.table)
            .field("coded_len", &self.coded.len())
            .finish()
    }
}

impl<'a> FsstBlockView<'a> {
    /// Parses an FSST block body (without leading row null bitmap) for `row_count` rows.
    pub fn parse(src: &'a [u8], row_count: usize) -> Result<Self, StatamError> {
        if row_count
            .checked_mul(size_of::<u32>())
            .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
        {
            return Err(StatamError::BoundsExceeded {
                reason: "fsst row_count exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        if row_count == 0 {
            if !src.is_empty() {
                return Err(StatamError::Corrupt {
                    reason: "trailing bytes in empty fsst block",
                });
            }
            return Ok(Self {
                table: FsstDecodeTable::empty(),
                offsets: vec![0],
                coded: &[],
            });
        }

        let (table, header_end) = FsstDecodeTable::parse_header(src)?;
        let rest = &src[header_end..];
        let Some(&len_bw) = rest.first() else {
            return Err(StatamError::Corrupt {
                reason: "truncated fsst row length bit_width",
            });
        };
        if len_bw > 32 {
            return Err(StatamError::Corrupt {
                reason: "invalid fsst row length bit_width",
            });
        }

        let packed_lens_slice = &rest[1..];
        let packed_lens_bytes = packed_bytes_len(row_count, len_bw)?;
        if packed_lens_slice.len() < packed_lens_bytes {
            return Err(StatamError::Corrupt {
                reason: "truncated fsst packed row lengths",
            });
        }
        let mut offsets = Vec::with_capacity(row_count + 1);
        offsets.push(0u32);
        unpack_u32(
            &packed_lens_slice[..packed_lens_bytes],
            row_count,
            len_bw,
            &mut offsets,
        )?;
        let coded = &packed_lens_slice[packed_lens_bytes..];

        let mut total_coded = 0usize;
        for off_slot in &mut offsets[1..] {
            total_coded =
                total_coded
                    .checked_add(*off_slot as usize)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "fsst coded byte total overflow",
                    })?;
            let off_u32 = u32::try_from(total_coded).map_err(|_| StatamError::BoundsExceeded {
                reason: "fsst coded byte offset exceeds u32::MAX",
            })?;
            *off_slot = off_u32;
        }
        if total_coded != coded.len() {
            return Err(StatamError::Corrupt {
                reason: "fsst coded byte length mismatch",
            });
        }

        Ok(Self {
            table,
            offsets,
            coded,
        })
    }

    /// Number of rows in this block.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Decodes a single row in O(1) time (proportional only to that row's length).
    pub fn decode_row(&self, row_idx: usize, out: &mut Vec<u8>) -> Result<(), StatamError> {
        if row_idx >= self.len() {
            return Err(StatamError::Corrupt {
                reason: "fsst decode_row index out of bounds",
            });
        }
        let start = self.offsets[row_idx] as usize;
        let end = self.offsets[row_idx + 1] as usize;
        let mut pos = out.len();
        self.table.decode_row_fast(
            &self.coded[start..end],
            out,
            &mut pos,
            MAX_BLOCK_RAW_BYTES,
            false,
        )?;
        out.truncate(pos);
        Ok(())
    }

    /// Decodes a single row at cursor `*pos` inside `out`, growing `out` geometrically
    /// without per-row truncation. Callers must call `out.truncate(pos)` once after their loop.
    pub fn decode_row_at(
        &self,
        row_idx: usize,
        out: &mut Vec<u8>,
        pos: &mut usize,
    ) -> Result<(), StatamError> {
        if row_idx >= self.len() {
            return Err(StatamError::Corrupt {
                reason: "fsst decode_row index out of bounds",
            });
        }
        let start = self.offsets[row_idx] as usize;
        let end = self.offsets[row_idx + 1] as usize;
        self.table
            .decode_row_fast(&self.coded[start..end], out, pos, MAX_BLOCK_RAW_BYTES, true)?;
        if *pos > MAX_BLOCK_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        Ok(())
    }

    /// Decodes a single row into `scratch` (reusing `scratch.len()` without zero-fill resizing
    /// across calls) and returns the decoded byte slice for that row.
    pub fn decode_row_into<'b>(
        &self,
        row_idx: usize,
        scratch: &'b mut Vec<u8>,
    ) -> Result<&'b [u8], StatamError> {
        if row_idx >= self.len() {
            return Err(StatamError::Corrupt {
                reason: "fsst decode_row index out of bounds",
            });
        }
        let start = self.offsets[row_idx] as usize;
        let end = self.offsets[row_idx + 1] as usize;
        let mut pos = 0usize;
        self.table.decode_row_fast(
            &self.coded[start..end],
            scratch,
            &mut pos,
            MAX_BLOCK_RAW_BYTES,
            true,
        )?;
        Ok(&scratch[..pos])
    }

    /// Decodes all rows into `(offsets, bytes)`, enforcing `MAX_BLOCK_RAW_BYTES`.
    pub fn decode_all(&self) -> Result<(Vec<u32>, Vec<u8>), StatamError> {
        let row_count = self.len();
        if row_count == 0 {
            return Ok((vec![0], Vec::new()));
        }

        let mut out_offsets = Vec::with_capacity(row_count + 1);
        out_offsets.push(0);
        let initial_cap =
            (self.coded.len().saturating_mul(2) + 64).min(MAX_BLOCK_RAW_BYTES) + FSST_MAX_SYM_LEN;
        let mut out_bytes = vec![0u8; initial_cap];
        let mut pos = 0usize;

        for row in 0..row_count {
            let start = self.offsets[row] as usize;
            let end = self.offsets[row + 1] as usize;
            self.table.decode_row_fast(
                &self.coded[start..end],
                &mut out_bytes,
                &mut pos,
                MAX_BLOCK_RAW_BYTES,
                true,
            )?;
            if pos > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let pos_u32 = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                reason: "fsst decoded size exceeds u32::MAX",
            })?;
            out_offsets.push(pos_u32);
        }
        out_bytes.truncate(pos);
        Ok((out_offsets, out_bytes))
    }

    /// Decodes only rows where `mask.get(row)` is true (and non-null per `nulls`).
    /// Unselected or null rows receive empty slices and `false` in the returned null bitmap.
    pub fn decode_selected(
        &self,
        mask: &Bitmap,
        nulls: Option<&Bitmap>,
    ) -> Result<FsstSelectedRows, StatamError> {
        let row_count = self.len();
        if mask.len() != row_count {
            return Err(StatamError::Corrupt {
                reason: "selection mask length mismatch",
            });
        }
        if nulls.is_some_and(|bm| bm.len() != row_count) {
            return Err(StatamError::Corrupt {
                reason: "null bitmap length mismatch",
            });
        }
        if row_count == 0 {
            return Ok((vec![0], Vec::new(), None));
        }

        let selected_count = mask.count_ones();
        if selected_count == 0 {
            return Ok((
                vec![0u32; row_count + 1],
                Vec::new(),
                Some(Bitmap::new_zeroed(row_count)),
            ));
        }

        let avg_coded = self.coded.len() / row_count.max(1);
        let est_decoded = avg_coded
            .saturating_mul(selected_count)
            .saturating_mul(2)
            .saturating_add(64);
        let initial_cap = est_decoded.min(MAX_BLOCK_RAW_BYTES) + FSST_MAX_SYM_LEN;

        let mut out_offsets = Vec::with_capacity(row_count + 1);
        out_offsets.push(0u32);
        let mut out_bytes = vec![0u8; initial_cap];
        let mut pos = 0usize;
        let mut out_nulls = Bitmap::new_zeroed(row_count);

        for row in mask.iter_ones() {
            if row >= row_count {
                break;
            }
            let cur_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                reason: "fsst decoded size exceeds u32::MAX",
            })?;
            if out_offsets.len() <= row {
                out_offsets.resize(row + 1, cur_off);
            }
            if nulls.is_none_or(|bm| bm.get(row)) {
                let start = self.offsets[row] as usize;
                let end = self.offsets[row + 1] as usize;
                self.table.decode_row_fast(
                    &self.coded[start..end],
                    &mut out_bytes,
                    &mut pos,
                    MAX_BLOCK_RAW_BYTES,
                    true,
                )?;
                if pos > MAX_BLOCK_RAW_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                    });
                }
                let _ = out_nulls.set(row, true);
            }
            let end_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                reason: "fsst decoded size exceeds u32::MAX",
            })?;
            out_offsets.push(end_off);
        }
        let tail_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
            reason: "fsst decoded size exceeds u32::MAX",
        })?;
        if out_offsets.len() < row_count + 1 {
            out_offsets.resize(row_count + 1, tail_off);
        }
        out_bytes.truncate(pos);
        Ok((out_offsets, out_bytes, Some(out_nulls)))
    }

    /// Decodes only `indices` into compact `indices.len()`-row `(offsets, bytes, nulls)`.
    pub(crate) fn decode_take(
        &self,
        indices: &[usize],
        nulls: Option<&Bitmap>,
    ) -> Result<FsstSelectedRows, StatamError> {
        let row_count = self.len();
        if nulls.is_some_and(|bm| bm.len() != row_count) {
            return Err(StatamError::Corrupt {
                reason: "null bitmap length mismatch",
            });
        }
        if indices
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(size_of::<u32>()))
            .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
        {
            return Err(StatamError::BoundsExceeded {
                reason: "fsst take offsets exceed MAX_BLOCK_RAW_BYTES",
            });
        }
        for &row in indices {
            if row >= row_count {
                return Err(StatamError::Corrupt {
                    reason: "fsst take index out of bounds",
                });
            }
        }
        if indices.is_empty() {
            return Ok((vec![0], Vec::new(), None));
        }

        let avg_coded = self.coded.len() / row_count.max(1);
        let est_decoded = avg_coded
            .saturating_mul(indices.len())
            .saturating_mul(2)
            .saturating_add(64);
        let initial_cap = est_decoded.min(MAX_BLOCK_RAW_BYTES) + FSST_MAX_SYM_LEN;

        let mut out_offsets = Vec::with_capacity(indices.len() + 1);
        out_offsets.push(0u32);
        let mut out_bytes = vec![0u8; initial_cap];
        let mut pos = 0usize;
        let mut out_nulls = if nulls.is_some() {
            Some(Bitmap::new_ones(indices.len()))
        } else {
            None
        };
        let mut any_null = false;

        for (out_pos, &row) in indices.iter().enumerate() {
            if nulls.is_none_or(|bm| bm.get(row)) {
                let start = self.offsets[row] as usize;
                let end = self.offsets[row + 1] as usize;
                self.table.decode_row_fast(
                    &self.coded[start..end],
                    &mut out_bytes,
                    &mut pos,
                    MAX_BLOCK_RAW_BYTES,
                    true,
                )?;
                if pos > MAX_BLOCK_RAW_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                    });
                }
            } else {
                any_null = true;
                if let Some(bm) = out_nulls.as_mut() {
                    let _ = bm.set(out_pos, false);
                }
            }
            let end_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                reason: "fsst decoded size exceeds u32::MAX",
            })?;
            out_offsets.push(end_off);
        }
        out_bytes.truncate(pos);
        let final_nulls = if any_null { out_nulls } else { None };
        Ok((out_offsets, out_bytes, final_nulls))
    }
}

#[derive(Clone, Copy)]
struct SubchunkMeta {
    codec: Codec,
    raw_len: u32,
    stored_start: u32,
    stored_end: u32,
}

struct SubchunkDecompressor {
    #[cfg(not(miri))]
    zstd: Option<zstd::bulk::Decompressor<'static>>,
    scratch: Vec<u8>,
}

impl SubchunkDecompressor {
    fn new() -> Self {
        Self {
            #[cfg(not(miri))]
            zstd: None,
            scratch: Vec::new(),
        }
    }

    fn decompress<'a>(
        &'a mut self,
        meta: SubchunkMeta,
        payload: &'a [u8],
    ) -> Result<&'a [u8], StatamError> {
        let stored = &payload[meta.stored_start as usize..meta.stored_end as usize];
        let raw_len = meta.raw_len as usize;
        check_decompress_bounds(raw_len, stored.len(), MAX_BLOCK_RAW_BYTES)?;
        match meta.codec {
            Codec::None => {
                if stored.len() != raw_len {
                    return Err(StatamError::Corrupt {
                        reason: "uncompressed fsst subchunk raw_len mismatch",
                    });
                }
                Ok(stored)
            }
            Codec::Lz4 => {
                self.scratch.resize(raw_len, 0);
                let written =
                    lz4_flex::block::decompress_into(stored, &mut self.scratch).map_err(|_| {
                        StatamError::Corrupt {
                            reason: "fsst subchunk lz4 decompression failed",
                        }
                    })?;
                if written != raw_len {
                    return Err(StatamError::Corrupt {
                        reason: "fsst subchunk lz4 decompressed length mismatch",
                    });
                }
                Ok(&self.scratch[..raw_len])
            }
            Codec::Zstd => {
                #[cfg(miri)]
                {
                    Err(StatamError::UnsupportedFeature { mask: 0x02 })
                }
                #[cfg(not(miri))]
                {
                    if self.zstd.is_none() {
                        let decomp =
                            zstd::bulk::Decompressor::new().map_err(|_| StatamError::Corrupt {
                                reason: "fsst subchunk zstd decompressor init failed",
                            })?;
                        self.zstd = Some(decomp);
                    }
                    let decomp = self.zstd.as_mut().ok_or(StatamError::Corrupt {
                        reason: "fsst subchunk zstd decompressor missing",
                    })?;
                    self.scratch.resize(raw_len, 0);
                    let written = decomp
                        .decompress_to_buffer(stored, &mut self.scratch)
                        .map_err(|_| StatamError::Corrupt {
                            reason: "fsst subchunk zstd decompression failed",
                        })?;
                    if written != raw_len {
                        return Err(StatamError::Corrupt {
                            reason: "fsst subchunk zstd decompressed length mismatch",
                        });
                    }
                    Ok(&self.scratch[..raw_len])
                }
            }
        }
    }
}

fn parse_subchunk_offsets<'a>(
    raw_chunk: &'a [u8],
    chunk_rows: usize,
    offsets: &mut Vec<u32>,
) -> Result<&'a [u8], StatamError> {
    let Some(&len_bw) = raw_chunk.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated fsst subchunk row length bit_width",
        });
    };
    if len_bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid fsst subchunk row length bit_width",
        });
    }
    let packed_slice = &raw_chunk[1..];
    let packed_bytes = packed_bytes_len(chunk_rows, len_bw)?;
    if packed_slice.len() < packed_bytes {
        return Err(StatamError::Corrupt {
            reason: "truncated fsst subchunk packed row lengths",
        });
    }
    offsets.clear();
    offsets.reserve(chunk_rows + 1);
    offsets.push(0u32);
    unpack_u32(&packed_slice[..packed_bytes], chunk_rows, len_bw, offsets)?;
    let coded = &packed_slice[packed_bytes..];

    let mut total_coded = 0usize;
    for off_slot in &mut offsets[1..] {
        total_coded =
            total_coded
                .checked_add(*off_slot as usize)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "fsst subchunk coded byte total overflow",
                })?;
        let off_u32 = u32::try_from(total_coded).map_err(|_| StatamError::BoundsExceeded {
            reason: "fsst subchunk coded byte offset exceeds u32::MAX",
        })?;
        *off_slot = off_u32;
    }
    if total_coded != coded.len() {
        return Err(StatamError::Corrupt {
            reason: "fsst subchunk coded byte length mismatch",
        });
    }
    Ok(coded)
}

/// Zero-copy parsed view over a sub-chunked (`BLOCK_FLAG_FSST_SUBCHUNKED`) FSST block.
///
/// Decompresses only the 256-row sub-chunks touched by a query, keeping sparse
/// `take_rows`, `selected`, and narrow `LIKE`/`ILIKE` scans fast.
#[derive(Clone)]
pub(crate) struct FsstSubchunkedView<'a> {
    table: FsstDecodeTable,
    row_count: usize,
    subchunk_rows: usize,
    chunks: Vec<SubchunkMeta>,
    payload: &'a [u8],
}

impl fmt::Debug for FsstSubchunkedView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsstSubchunkedView")
            .field("row_count", &self.row_count)
            .field("subchunk_rows", &self.subchunk_rows)
            .field("num_chunks", &self.chunks.len())
            .field("table", &self.table)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl<'a> FsstSubchunkedView<'a> {
    /// Parses and strictly validates a sub-chunked FSST block body (without leading row null bitmap).
    pub(crate) fn parse(src: &'a [u8], row_count: usize) -> Result<Self, StatamError> {
        if row_count
            .checked_mul(size_of::<u32>())
            .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
        {
            return Err(StatamError::BoundsExceeded {
                reason: "fsst row_count exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        if row_count == 0 {
            if !src.is_empty() {
                return Err(StatamError::Corrupt {
                    reason: "trailing bytes in empty subchunked fsst block",
                });
            }
            return Ok(Self {
                table: FsstDecodeTable::empty(),
                row_count: 0,
                subchunk_rows: 1,
                chunks: Vec::new(),
                payload: &[],
            });
        }

        let (table, header_end) = FsstDecodeTable::parse_header(src)?;
        let rest = &src[header_end..];
        if rest.len() < 4 {
            return Err(StatamError::Corrupt {
                reason: "truncated fsst subchunk header",
            });
        }
        let subchunk_rows = u16::from_le_bytes([rest[0], rest[1]]);
        let num_chunks = usize::from(u16::from_le_bytes([rest[2], rest[3]]));
        if subchunk_rows == 0 || subchunk_rows > FSST_SUBCHUNK_MAX_ROWS {
            return Err(StatamError::Corrupt {
                reason: "invalid fsst subchunk_rows",
            });
        }
        let subchunk_rows_usize = usize::from(subchunk_rows);
        let expected_chunks = row_count.div_ceil(subchunk_rows_usize);
        if num_chunks != expected_chunks {
            return Err(StatamError::Corrupt {
                reason: "fsst subchunk num_chunks mismatch",
            });
        }

        let dir_bytes =
            num_chunks
                .checked_mul(FSST_SUBCHUNK_ENTRY_LEN)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "fsst subchunk directory overflow",
                })?;
        if rest.len() - 4 < dir_bytes {
            return Err(StatamError::Corrupt {
                reason: "truncated fsst subchunk directory",
            });
        }
        let dir_slice = &rest[4..4 + dir_bytes];
        let payload = &rest[4 + dir_bytes..];

        let mut chunks = Vec::with_capacity(num_chunks);
        let mut prev_end = 0u32;
        let mut total_raw = 0usize;
        for i in 0..num_chunks {
            let off = i * FSST_SUBCHUNK_ENTRY_LEN;
            let codec = Codec::from_u8(dir_slice[off])?;
            let raw_len = u32::from_le_bytes([
                dir_slice[off + 1],
                dir_slice[off + 2],
                dir_slice[off + 3],
                dir_slice[off + 4],
            ]);
            let stored_end = u32::from_le_bytes([
                dir_slice[off + 5],
                dir_slice[off + 6],
                dir_slice[off + 7],
                dir_slice[off + 8],
            ]);
            if stored_end < prev_end || (stored_end as usize) > payload.len() {
                return Err(StatamError::Corrupt {
                    reason: "invalid fsst subchunk stored_end",
                });
            }
            let stored_len = (stored_end - prev_end) as usize;
            let raw_len_usize = raw_len as usize;
            if codec == Codec::None && raw_len_usize != stored_len {
                return Err(StatamError::Corrupt {
                    reason: "uncompressed fsst subchunk raw_len mismatch",
                });
            }
            check_decompress_bounds(raw_len_usize, stored_len, MAX_BLOCK_RAW_BYTES)?;
            total_raw =
                total_raw
                    .checked_add(raw_len_usize)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "fsst subchunk total raw_len overflow",
                    })?;
            if total_raw > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "fsst subchunk total raw_len exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            chunks.push(SubchunkMeta {
                codec,
                raw_len,
                stored_start: prev_end,
                stored_end,
            });
            prev_end = stored_end;
        }

        if (prev_end as usize) != payload.len() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in fsst subchunk payload",
            });
        }

        Ok(Self {
            table,
            row_count,
            subchunk_rows: subchunk_rows_usize,
            chunks,
            payload,
        })
    }

    /// Decodes all rows across all sub-chunks, reusing one Zstd decompressor and scratch buffer.
    pub(crate) fn decode_all_with_limit(
        &self,
        max_total_bytes: usize,
    ) -> Result<(Vec<u32>, Vec<u8>), StatamError> {
        let max_bytes = max_total_bytes.min(MAX_BLOCK_RAW_BYTES);
        if self.row_count == 0 {
            return Ok((vec![0], Vec::new()));
        }

        let mut out_offsets = Vec::with_capacity(self.row_count + 1);
        out_offsets.push(0u32);
        let sum_raw: usize = self.chunks.iter().map(|c| c.raw_len as usize).sum();
        let initial_cap = (sum_raw.saturating_mul(2) + 64).min(max_bytes) + FSST_MAX_SYM_LEN;
        let mut out_bytes = vec![0u8; initial_cap];
        let mut pos = 0usize;

        let mut decomp = SubchunkDecompressor::new();
        let mut chunk_offsets = Vec::with_capacity(self.subchunk_rows + 1);

        for (chunk_idx, &meta) in self.chunks.iter().enumerate() {
            let chunk_start = chunk_idx * self.subchunk_rows;
            let chunk_end = (chunk_start + self.subchunk_rows).min(self.row_count);
            let chunk_rows = chunk_end - chunk_start;

            let raw_chunk = decomp.decompress(meta, self.payload)?;
            let coded = parse_subchunk_offsets(raw_chunk, chunk_rows, &mut chunk_offsets)?;
            for local_row in 0..chunk_rows {
                let start = chunk_offsets[local_row] as usize;
                let end = chunk_offsets[local_row + 1] as usize;
                self.table.decode_row_fast(
                    &coded[start..end],
                    &mut out_bytes,
                    &mut pos,
                    max_bytes,
                    true,
                )?;
                if pos > max_bytes {
                    return Err(StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                    });
                }
                let pos_u32 = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                    reason: "fsst decoded size exceeds u32::MAX",
                })?;
                out_offsets.push(pos_u32);
            }
        }
        out_bytes.truncate(pos);
        Ok((out_offsets, out_bytes))
    }

    /// Decodes only `mask`-selected rows, decompressing ONLY touched sub-chunks.
    pub(crate) fn decode_selected_with_limit(
        &self,
        mask: &Bitmap,
        nulls: Option<&Bitmap>,
        max_total_bytes: usize,
    ) -> Result<FsstSelectedRows, StatamError> {
        let max_bytes = max_total_bytes.min(MAX_BLOCK_RAW_BYTES);
        let row_count = self.row_count;
        if mask.len() != row_count {
            return Err(StatamError::Corrupt {
                reason: "selection mask length mismatch",
            });
        }
        if nulls.is_some_and(|bm| bm.len() != row_count) {
            return Err(StatamError::Corrupt {
                reason: "null bitmap length mismatch",
            });
        }
        if row_count == 0 {
            return Ok((vec![0], Vec::new(), None));
        }

        let selected_count = mask.count_ones();
        if selected_count == 0 {
            return Ok((
                vec![0u32; row_count + 1],
                Vec::new(),
                Some(Bitmap::new_zeroed(row_count)),
            ));
        }

        let sum_raw: usize = self.chunks.iter().map(|c| c.raw_len as usize).sum();
        let avg_coded = sum_raw / row_count.max(1);
        let est_decoded = avg_coded
            .saturating_mul(selected_count)
            .saturating_mul(2)
            .saturating_add(64);
        let initial_cap = est_decoded.min(max_bytes) + FSST_MAX_SYM_LEN;

        let mut out_offsets = Vec::with_capacity(row_count + 1);
        out_offsets.push(0u32);
        let mut out_bytes = vec![0u8; initial_cap];
        let mut pos = 0usize;
        let mut out_nulls = Bitmap::new_zeroed(row_count);

        let mut decomp = SubchunkDecompressor::new();
        let mut chunk_offsets = Vec::with_capacity(self.subchunk_rows + 1);
        let mut chunk_rows_buf: Vec<usize> = Vec::with_capacity(self.subchunk_rows.min(64));

        let mut iter = mask.iter_ones().take_while(|&r| r < row_count).peekable();
        while let Some(&first_row) = iter.peek() {
            let chunk_idx = first_row / self.subchunk_rows;
            let chunk_start = chunk_idx * self.subchunk_rows;
            let chunk_end = (chunk_start + self.subchunk_rows).min(row_count);

            chunk_rows_buf.clear();
            let mut any_non_null = false;
            while let Some(&r) = iter.peek() {
                if r >= chunk_end {
                    break;
                }
                let _ = iter.next();
                if nulls.is_none_or(|bm| bm.get(r)) {
                    any_non_null = true;
                }
                chunk_rows_buf.push(r);
            }

            if any_non_null {
                let raw_chunk = decomp.decompress(self.chunks[chunk_idx], self.payload)?;
                let coded =
                    parse_subchunk_offsets(raw_chunk, chunk_end - chunk_start, &mut chunk_offsets)?;
                for &row in &chunk_rows_buf {
                    let cur_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds u32::MAX",
                    })?;
                    if out_offsets.len() <= row {
                        out_offsets.resize(row + 1, cur_off);
                    }
                    if nulls.is_none_or(|bm| bm.get(row)) {
                        let local = row - chunk_start;
                        let start = chunk_offsets[local] as usize;
                        let end = chunk_offsets[local + 1] as usize;
                        self.table.decode_row_fast(
                            &coded[start..end],
                            &mut out_bytes,
                            &mut pos,
                            max_bytes,
                            true,
                        )?;
                        if pos > max_bytes {
                            return Err(StatamError::BoundsExceeded {
                                reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                            });
                        }
                        let _ = out_nulls.set(row, true);
                    }
                    let end_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds u32::MAX",
                    })?;
                    out_offsets.push(end_off);
                }
            } else {
                let cur_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                    reason: "fsst decoded size exceeds u32::MAX",
                })?;
                for &row in &chunk_rows_buf {
                    if out_offsets.len() <= row {
                        out_offsets.resize(row + 1, cur_off);
                    }
                    out_offsets.push(cur_off);
                }
            }
        }

        let tail_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
            reason: "fsst decoded size exceeds u32::MAX",
        })?;
        if out_offsets.len() < row_count + 1 {
            out_offsets.resize(row_count + 1, tail_off);
        }
        out_bytes.truncate(pos);
        Ok((out_offsets, out_bytes, Some(out_nulls)))
    }

    /// Decodes only `indices` into compact `indices.len()`-row `(offsets, bytes, nulls)`,
    /// decompressing ONLY touched sub-chunks (at most once each).
    pub(crate) fn decode_take(
        &self,
        indices: &[usize],
        nulls: Option<&Bitmap>,
    ) -> Result<FsstSelectedRows, StatamError> {
        let row_count = self.row_count;
        if nulls.is_some_and(|bm| bm.len() != row_count) {
            return Err(StatamError::Corrupt {
                reason: "null bitmap length mismatch",
            });
        }
        if indices
            .len()
            .checked_add(1)
            .and_then(|n| n.checked_mul(size_of::<u32>()))
            .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
        {
            return Err(StatamError::BoundsExceeded {
                reason: "fsst take offsets exceed MAX_BLOCK_RAW_BYTES",
            });
        }
        for &row in indices {
            if row >= row_count {
                return Err(StatamError::Corrupt {
                    reason: "fsst take index out of bounds",
                });
            }
        }
        if indices.is_empty() {
            return Ok((vec![0], Vec::new(), None));
        }

        let sum_raw: usize = self.chunks.iter().map(|c| c.raw_len as usize).sum();
        let avg_coded = sum_raw / row_count.max(1);
        let est_decoded = avg_coded
            .saturating_mul(indices.len())
            .saturating_mul(2)
            .saturating_add(64);
        let initial_cap = est_decoded.min(MAX_BLOCK_RAW_BYTES) + FSST_MAX_SYM_LEN;

        let mut out_nulls = if nulls.is_some() {
            Some(Bitmap::new_ones(indices.len()))
        } else {
            None
        };
        let mut any_null = false;

        let mut decomp = SubchunkDecompressor::new();
        let mut chunk_offsets = Vec::with_capacity(self.subchunk_rows + 1);

        // Fast path: chunk indices are non-decreasing (always true for `StatamReader::take_rows`).
        let chunks_monotonic = indices
            .windows(2)
            .all(|w| w[0] / self.subchunk_rows <= w[1] / self.subchunk_rows);

        if chunks_monotonic {
            let mut out_offsets = Vec::with_capacity(indices.len() + 1);
            out_offsets.push(0u32);
            let mut out_bytes = vec![0u8; initial_cap];
            let mut pos = 0usize;

            let mut i = 0usize;
            while i < indices.len() {
                let chunk_idx = indices[i] / self.subchunk_rows;
                let chunk_start = chunk_idx * self.subchunk_rows;
                let chunk_end = (chunk_start + self.subchunk_rows).min(row_count);

                let group_start = i;
                let mut any_non_null = false;
                while i < indices.len() && indices[i] / self.subchunk_rows == chunk_idx {
                    if nulls.is_none_or(|bm| bm.get(indices[i])) {
                        any_non_null = true;
                    }
                    i += 1;
                }
                let group = &indices[group_start..i];

                if any_non_null {
                    let raw_chunk = decomp.decompress(self.chunks[chunk_idx], self.payload)?;
                    let coded = parse_subchunk_offsets(
                        raw_chunk,
                        chunk_end - chunk_start,
                        &mut chunk_offsets,
                    )?;
                    for (offset_in_group, &row) in group.iter().enumerate() {
                        let out_pos = group_start + offset_in_group;
                        if nulls.is_none_or(|bm| bm.get(row)) {
                            let local = row - chunk_start;
                            let start = chunk_offsets[local] as usize;
                            let end = chunk_offsets[local + 1] as usize;
                            self.table.decode_row_fast(
                                &coded[start..end],
                                &mut out_bytes,
                                &mut pos,
                                MAX_BLOCK_RAW_BYTES,
                                true,
                            )?;
                            if pos > MAX_BLOCK_RAW_BYTES {
                                return Err(StatamError::BoundsExceeded {
                                    reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                                });
                            }
                        } else {
                            any_null = true;
                            if let Some(bm) = out_nulls.as_mut() {
                                let _ = bm.set(out_pos, false);
                            }
                        }
                        let end_off =
                            u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                                reason: "fsst decoded size exceeds u32::MAX",
                            })?;
                        out_offsets.push(end_off);
                    }
                } else {
                    any_null = true;
                    let cur_off = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds u32::MAX",
                    })?;
                    for offset_in_group in 0..group.len() {
                        if let Some(bm) = out_nulls.as_mut() {
                            let _ = bm.set(group_start + offset_in_group, false);
                        }
                        out_offsets.push(cur_off);
                    }
                }
            }

            out_bytes.truncate(pos);
            let final_nulls = if any_null { out_nulls } else { None };
            return Ok((out_offsets, out_bytes, final_nulls));
        }

        // General path: unsorted or repeated indices across arbitrary chunks.
        // Group by chunk so every touched sub-chunk is still decompressed at most once.
        let mut order: Vec<(usize, usize, usize)> = Vec::with_capacity(indices.len());
        for (out_pos, &row) in indices.iter().enumerate() {
            if nulls.is_none_or(|bm| bm.get(row)) {
                order.push((row / self.subchunk_rows, out_pos, row));
            } else {
                any_null = true;
                if let Some(bm) = out_nulls.as_mut() {
                    let _ = bm.set(out_pos, false);
                }
            }
        }
        order.sort_unstable_by_key(|&(chunk_idx, out_pos, _)| (chunk_idx, out_pos));

        let mut staging_bytes = vec![0u8; initial_cap];
        let mut staging_pos = 0usize;
        let mut row_spans: Vec<(u32, u32)> = vec![(0, 0); indices.len()];

        let mut i = 0usize;
        while i < order.len() {
            let chunk_idx = order[i].0;
            let chunk_start = chunk_idx * self.subchunk_rows;
            let chunk_end = (chunk_start + self.subchunk_rows).min(row_count);
            let raw_chunk = decomp.decompress(self.chunks[chunk_idx], self.payload)?;
            let coded =
                parse_subchunk_offsets(raw_chunk, chunk_end - chunk_start, &mut chunk_offsets)?;
            while i < order.len() && order[i].0 == chunk_idx {
                let (_, out_pos, row) = order[i];
                let local = row - chunk_start;
                let s_off = staging_pos;
                let start = chunk_offsets[local] as usize;
                let end = chunk_offsets[local + 1] as usize;
                self.table.decode_row_fast(
                    &coded[start..end],
                    &mut staging_bytes,
                    &mut staging_pos,
                    MAX_BLOCK_RAW_BYTES,
                    true,
                )?;
                if staging_pos > MAX_BLOCK_RAW_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                    });
                }
                let s_u32 = u32::try_from(s_off).map_err(|_| StatamError::BoundsExceeded {
                    reason: "fsst decoded size exceeds u32::MAX",
                })?;
                let e_u32 =
                    u32::try_from(staging_pos).map_err(|_| StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds u32::MAX",
                    })?;
                row_spans[out_pos] = (s_u32, e_u32);
                i += 1;
            }
        }
        staging_bytes.truncate(staging_pos);

        let mut out_offsets = Vec::with_capacity(indices.len() + 1);
        out_offsets.push(0u32);
        let mut out_bytes = Vec::with_capacity(staging_pos);
        for &(s, e) in &row_spans {
            out_bytes.extend_from_slice(&staging_bytes[s as usize..e as usize]);
            let end_off =
                u32::try_from(out_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "fsst decoded size exceeds u32::MAX",
                })?;
            out_offsets.push(end_off);
        }
        let final_nulls = if any_null { out_nulls } else { None };
        Ok((out_offsets, out_bytes, final_nulls))
    }

    /// Decodes active element ranges (`offsets[row]..offsets[row + 1]` for each `row` in `mask`)
    /// into compact `(offsets, bytes, optional_nulls)`, decompressing ONLY touched sub-chunks.
    pub(crate) fn decode_ranges(
        &self,
        offsets: &[u32],
        row_count: usize,
        mask: &Bitmap,
        nulls: Option<&Bitmap>,
        max_total_bytes: usize,
    ) -> Result<FsstSelectedRows, StatamError> {
        let max_bytes = max_total_bytes.min(MAX_BLOCK_RAW_BYTES);
        let total_active: usize = mask
            .iter_ones()
            .take_while(|&r| r < row_count)
            .map(|r| (offsets[r + 1] - offsets[r]) as usize)
            .sum();

        let mut out_offsets = Vec::with_capacity(total_active + 1);
        out_offsets.push(0u32);
        if total_active == 0 {
            return Ok((out_offsets, Vec::new(), None));
        }

        let sum_raw: usize = self.chunks.iter().map(|c| c.raw_len as usize).sum();
        let avg_coded = sum_raw / self.row_count.max(1);
        let est_decoded = avg_coded
            .saturating_mul(total_active)
            .saturating_mul(2)
            .saturating_add(64);
        let initial_cap = est_decoded.min(max_bytes) + FSST_MAX_SYM_LEN;
        let mut out_bytes = vec![0u8; initial_cap];
        let mut pos = 0usize;
        let mut out_nulls: Option<Bitmap> = None;

        let mut decomp = SubchunkDecompressor::new();
        let mut chunk_offsets = Vec::with_capacity(self.subchunk_rows + 1);
        let mut chunk_items: Vec<(usize, usize)> = Vec::with_capacity(self.subchunk_rows.min(64));

        // Flush a contiguous batch of active child elements belonging to `chunk_idx`.
        let flush_chunk = |chunk_idx: usize,
                           items: &[(usize, usize)],
                           any_non_null: bool,
                           decomp: &mut SubchunkDecompressor,
                           chunk_offsets: &mut Vec<u32>,
                           out_bytes: &mut Vec<u8>,
                           pos: &mut usize,
                           out_offsets: &mut Vec<u32>,
                           out_nulls: &mut Option<Bitmap>|
         -> Result<(), StatamError> {
            let chunk_start = chunk_idx * self.subchunk_rows;
            let chunk_end = (chunk_start + self.subchunk_rows).min(self.row_count);
            if any_non_null {
                let raw_chunk = decomp.decompress(self.chunks[chunk_idx], self.payload)?;
                let coded =
                    parse_subchunk_offsets(raw_chunk, chunk_end - chunk_start, chunk_offsets)?;
                for &(active_idx, idx) in items {
                    if nulls.is_none_or(|bm| bm.get(idx)) {
                        let local = idx - chunk_start;
                        let start = chunk_offsets[local] as usize;
                        let end = chunk_offsets[local + 1] as usize;
                        self.table.decode_row_fast(
                            &coded[start..end],
                            out_bytes,
                            pos,
                            max_bytes,
                            true,
                        )?;
                        if *pos > max_bytes {
                            return Err(StatamError::BoundsExceeded {
                                reason: "active varlen bytes exceed MAX_BLOCK_RAW_BYTES",
                            });
                        }
                    } else {
                        let bm = out_nulls.get_or_insert_with(|| Bitmap::new_ones(total_active));
                        let _ = bm.set(active_idx, false);
                    }
                    let end_off = u32::try_from(*pos).map_err(|_| StatamError::BoundsExceeded {
                        reason: "active varlen offset exceeds u32::MAX",
                    })?;
                    out_offsets.push(end_off);
                }
            } else {
                let cur_off = u32::try_from(*pos).map_err(|_| StatamError::BoundsExceeded {
                    reason: "active varlen offset exceeds u32::MAX",
                })?;
                let bm = out_nulls.get_or_insert_with(|| Bitmap::new_ones(total_active));
                for &(active_idx, _) in items {
                    let _ = bm.set(active_idx, false);
                    out_offsets.push(cur_off);
                }
            }
            Ok(())
        };

        let mut active_idx = 0usize;
        let mut cur_chunk: Option<usize> = None;
        let mut cur_any_non_null = false;

        for row in mask.iter_ones() {
            if row >= row_count {
                break;
            }
            let rs = offsets[row] as usize;
            let re = offsets[row + 1] as usize;
            for idx in rs..re {
                if idx >= self.row_count {
                    return Err(StatamError::Corrupt {
                        reason: "fsst range child index out of bounds",
                    });
                }
                let cidx = idx / self.subchunk_rows;
                if let Some(prev) = cur_chunk
                    && prev != cidx
                {
                    flush_chunk(
                        prev,
                        &chunk_items,
                        cur_any_non_null,
                        &mut decomp,
                        &mut chunk_offsets,
                        &mut out_bytes,
                        &mut pos,
                        &mut out_offsets,
                        &mut out_nulls,
                    )?;
                    chunk_items.clear();
                    cur_any_non_null = false;
                }
                cur_chunk = Some(cidx);
                if nulls.is_none_or(|bm| bm.get(idx)) {
                    cur_any_non_null = true;
                }
                chunk_items.push((active_idx, idx));
                active_idx += 1;
            }
        }
        if let Some(prev) = cur_chunk
            && !chunk_items.is_empty()
        {
            flush_chunk(
                prev,
                &chunk_items,
                cur_any_non_null,
                &mut decomp,
                &mut chunk_offsets,
                &mut out_bytes,
                &mut pos,
                &mut out_offsets,
                &mut out_nulls,
            )?;
        }

        out_bytes.truncate(pos);
        Ok((out_offsets, out_bytes, out_nulls))
    }

    /// Evaluates a predicate over `active_mask` rows, decompressing ONLY touched sub-chunks.
    pub(crate) fn eval_substring(
        &self,
        active_mask: &Bitmap,
        nulls: Option<&Bitmap>,
        mut match_fn: impl FnMut(&[u8]) -> bool,
    ) -> Result<Bitmap, StatamError> {
        let row_count = self.row_count;
        if active_mask.len() != row_count {
            return Err(StatamError::Corrupt {
                reason: "active_mask length mismatch in subchunked fsst eval",
            });
        }
        let mut out = Bitmap::new_zeroed(row_count);
        if row_count == 0 || active_mask.all_zeros() {
            return Ok(out);
        }

        let mut decomp = SubchunkDecompressor::new();
        let mut chunk_offsets = Vec::with_capacity(self.subchunk_rows + 1);
        let mut row_scratch = Vec::new();
        let mut chunk_rows_buf: Vec<usize> = Vec::with_capacity(self.subchunk_rows.min(64));

        let mut iter = active_mask
            .iter_ones()
            .take_while(|&r| r < row_count)
            .peekable();
        while let Some(&first_row) = iter.peek() {
            let chunk_idx = first_row / self.subchunk_rows;
            let chunk_start = chunk_idx * self.subchunk_rows;
            let chunk_end = (chunk_start + self.subchunk_rows).min(row_count);

            chunk_rows_buf.clear();
            while let Some(&r) = iter.peek() {
                if r >= chunk_end {
                    break;
                }
                let _ = iter.next();
                if nulls.is_none_or(|bm| bm.get(r)) {
                    chunk_rows_buf.push(r);
                }
            }
            if chunk_rows_buf.is_empty() {
                continue;
            }

            let raw_chunk = decomp.decompress(self.chunks[chunk_idx], self.payload)?;
            let coded =
                parse_subchunk_offsets(raw_chunk, chunk_end - chunk_start, &mut chunk_offsets)?;
            for &row in &chunk_rows_buf {
                let local = row - chunk_start;
                let start = chunk_offsets[local] as usize;
                let end = chunk_offsets[local + 1] as usize;
                let mut pos = 0usize;
                self.table.decode_row_fast(
                    &coded[start..end],
                    &mut row_scratch,
                    &mut pos,
                    MAX_BLOCK_RAW_BYTES,
                    true,
                )?;
                if match_fn(&row_scratch[..pos]) {
                    let _ = out.set(row, true);
                }
            }
        }
        Ok(out)
    }
}

/// Decodes an FSST-encoded block body into [`Utf8Data`].
pub fn decode_fsst_utf8(src: &[u8], row_count: usize) -> Result<Utf8Data, StatamError> {
    let view = FsstBlockView::parse(src, row_count)?;
    let (offsets, bytes) = view.decode_all()?;
    Ok(Utf8Data::try_new(offsets, bytes)?)
}

/// Decodes an FSST-encoded block body into [`BinaryData`].
pub fn decode_fsst_binary(src: &[u8], row_count: usize) -> Result<BinaryData, StatamError> {
    let view = FsstBlockView::parse(src, row_count)?;
    let (offsets, bytes) = view.decode_all()?;
    Ok(BinaryData::try_new(offsets, bytes)?)
}

/// Decodes a sub-chunked (`BLOCK_FLAG_FSST_SUBCHUNKED`) FSST block body into [`Utf8Data`].
pub(crate) fn decode_fsst_utf8_subchunked(
    src: &[u8],
    row_count: usize,
    max_total_bytes: usize,
) -> Result<Utf8Data, StatamError> {
    let view = FsstSubchunkedView::parse(src, row_count)?;
    let (offsets, bytes) = view.decode_all_with_limit(max_total_bytes)?;
    Ok(Utf8Data::try_new(offsets, bytes)?)
}

/// Decodes a sub-chunked (`BLOCK_FLAG_FSST_SUBCHUNKED`) FSST block body into [`BinaryData`].
pub(crate) fn decode_fsst_binary_subchunked(
    src: &[u8],
    row_count: usize,
    max_total_bytes: usize,
) -> Result<BinaryData, StatamError> {
    let view = FsstSubchunkedView::parse(src, row_count)?;
    let (offsets, bytes) = view.decode_all_with_limit(max_total_bytes)?;
    Ok(BinaryData::try_new(offsets, bytes)?)
}

/// Decodes only `mask`-selected rows of an FSST-encoded block body into `(Utf8Data, nulls)`.
pub fn decode_fsst_utf8_selected(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
) -> Result<(Utf8Data, Option<Bitmap>), StatamError> {
    decode_fsst_utf8_selected_with_flag(src, row_count, mask, nulls, false, MAX_BLOCK_RAW_BYTES)
}

/// Decodes only `mask`-selected rows of an FSST-encoded block body into `(BinaryData, nulls)`.
pub fn decode_fsst_binary_selected(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
) -> Result<(BinaryData, Option<Bitmap>), StatamError> {
    decode_fsst_binary_selected_with_flag(src, row_count, mask, nulls, false, MAX_BLOCK_RAW_BYTES)
}

pub(crate) fn decode_fsst_utf8_selected_with_flag(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
    subchunked: bool,
    max_total_bytes: usize,
) -> Result<(Utf8Data, Option<Bitmap>), StatamError> {
    let (offsets, bytes, out_nulls) = if subchunked {
        let view = FsstSubchunkedView::parse(src, row_count)?;
        view.decode_selected_with_limit(mask, nulls, max_total_bytes)?
    } else {
        let view = FsstBlockView::parse(src, row_count)?;
        view.decode_selected(mask, nulls)?
    };
    Ok((Utf8Data::try_new(offsets, bytes)?, out_nulls))
}

pub(crate) fn decode_fsst_binary_selected_with_flag(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
    subchunked: bool,
    max_total_bytes: usize,
) -> Result<(BinaryData, Option<Bitmap>), StatamError> {
    let (offsets, bytes, out_nulls) = if subchunked {
        let view = FsstSubchunkedView::parse(src, row_count)?;
        view.decode_selected_with_limit(mask, nulls, max_total_bytes)?
    } else {
        let view = FsstBlockView::parse(src, row_count)?;
        view.decode_selected(mask, nulls)?
    };
    Ok((BinaryData::try_new(offsets, bytes)?, out_nulls))
}

/// Decodes only `indices` of an FSST-encoded block body into a compact `indices.len()`-row
/// `(Utf8Data, nulls)`.
pub(crate) fn decode_fsst_utf8_take(
    body: &[u8],
    row_count: usize,
    indices: &[usize],
    nulls: Option<&Bitmap>,
    subchunked: bool,
) -> Result<(Utf8Data, Option<Bitmap>), StatamError> {
    let (offsets, bytes, out_nulls) = if subchunked {
        let view = FsstSubchunkedView::parse(body, row_count)?;
        view.decode_take(indices, nulls)?
    } else {
        let view = FsstBlockView::parse(body, row_count)?;
        view.decode_take(indices, nulls)?
    };
    Ok((Utf8Data::try_new(offsets, bytes)?, out_nulls))
}

/// Decodes only `indices` of an FSST-encoded block body into a compact `indices.len()`-row
/// `(BinaryData, nulls)`.
pub(crate) fn decode_fsst_binary_take(
    body: &[u8],
    row_count: usize,
    indices: &[usize],
    nulls: Option<&Bitmap>,
    subchunked: bool,
) -> Result<(BinaryData, Option<Bitmap>), StatamError> {
    let (offsets, bytes, out_nulls) = if subchunked {
        let view = FsstSubchunkedView::parse(body, row_count)?;
        view.decode_take(indices, nulls)?
    } else {
        let view = FsstBlockView::parse(body, row_count)?;
        view.decode_take(indices, nulls)?
    };
    Ok((BinaryData::try_new(offsets, bytes)?, out_nulls))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_selected_rejects_mismatched_mask_and_nulls_lengths() {
        let rows = [
            b"{\"role\":\"assistant\",\"model\":\"claude\"}".as_slice(),
            b"{\"role\":\"assistant\",\"model\":\"claude-3\"}".as_slice(),
            b"{\"role\":\"assistant\",\"model\":\"claude-3.5\"}".as_slice(),
            b"{\"role\":\"assistant\",\"model\":\"claude-4\"}".as_slice(),
        ];
        let body = encode_fsst_body(&rows).unwrap().unwrap();
        let view = FsstBlockView::parse(&body, rows.len()).unwrap();

        let short_mask = Bitmap::new_ones(rows.len() - 1);
        assert!(matches!(
            view.decode_selected(&short_mask, None),
            Err(StatamError::Corrupt {
                reason: "selection mask length mismatch",
            })
        ));
        let long_mask = Bitmap::new_ones(rows.len() + 1);
        assert!(matches!(
            view.decode_selected(&long_mask, None),
            Err(StatamError::Corrupt {
                reason: "selection mask length mismatch",
            })
        ));

        let valid_mask = Bitmap::new_ones(rows.len());
        let bad_nulls = Bitmap::new_ones(rows.len() + 1);
        assert!(matches!(
            view.decode_selected(&valid_mask, Some(&bad_nulls)),
            Err(StatamError::Corrupt {
                reason: "null bitmap length mismatch",
            })
        ));
    }

    #[test]
    fn fsst_single_pass_8b_decode_matches_reference_and_rejects_corrupt_and_oversized() {
        // 1. Mixed empty strings, 1-byte..8-byte symbols, literal 0xFF escapes, non-ASCII, and large strings.
        let mut big_url = String::from("https://api.example.com/v1/traces/");
        for i in 0..1500 {
            big_url.push_str(&format!("seg{i}_"));
        }
        let mut raw_rows: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"https://".to_vec(),
            vec![0xFF, 0x00, 0xFF, 0xFE, 0x7F],
            b"{\"url\":\"https://cdn.example.net/assets/app.js?v=12345\"}".to_vec(),
            big_url.into_bytes(),
        ];
        // Add enough repeated patterns so FSST compresses >= 1.15x and trains 1..=8 byte symbols.
        for i in 0..120 {
            raw_rows.push(
                format!("https://observability.cetra.sh/api/v1/spans/trace_{i:04}/events")
                    .into_bytes(),
            );
        }
        let refs: Vec<&[u8]> = raw_rows.iter().map(Vec::as_slice).collect();
        let body = encode_fsst_body(&refs).unwrap().unwrap();
        let view = FsstBlockView::parse(&body, refs.len()).unwrap();

        // decode_all parity
        let (offs, decoded_bytes) = view.decode_all().unwrap();
        assert_eq!(offs.len(), refs.len() + 1);
        for (idx, expected) in refs.iter().enumerate() {
            let s = offs[idx] as usize;
            let e = offs[idx + 1] as usize;
            assert_eq!(&decoded_bytes[s..e], *expected);
        }

        // decode_row parity (including buffer reuse)
        let mut row_buf = Vec::new();
        for (idx, expected) in refs.iter().enumerate() {
            row_buf.clear();
            view.decode_row(idx, &mut row_buf).unwrap();
            assert_eq!(row_buf.as_slice(), *expected);
        }

        // decode_selected parity (sparse mask + nulls)
        let mut mask = Bitmap::new_zeroed(refs.len());
        let mut nulls = Bitmap::new_ones(refs.len());
        let _ = mask.set(0, true);
        let _ = mask.set(2, true);
        let _ = mask.set(3, true);
        let _ = mask.set(5, true);
        let _ = mask.set(25, true);
        let _ = nulls.set(2, false); // selected but null
        let (sel_offs, sel_bytes, sel_nulls) = view.decode_selected(&mask, Some(&nulls)).unwrap();
        let sel_nulls = sel_nulls.unwrap();
        for (idx, expected) in refs.iter().enumerate() {
            let s = sel_offs[idx] as usize;
            let e = sel_offs[idx + 1] as usize;
            if mask.get(idx) && nulls.get(idx) {
                assert!(sel_nulls.get(idx));
                assert_eq!(&sel_bytes[s..e], *expected);
            } else {
                assert!(!sel_nulls.get(idx));
                assert_eq!(s, e);
            }
        }

        // 2. sym_count == 255 with 8-byte symbols + 0xFF literal escape.
        let mut sym_entries = Vec::with_capacity(255);
        for code in 0..255u8 {
            let mut b = [b'A'; 8];
            b[0] = code;
            b[7] = code ^ 0x5A;
            sym_entries.push(SymEntry { len: 8, bytes: b });
        }
        let full_table = FsstTable::from_entries(&sym_entries);
        let mut full_body = Vec::new();
        full_table.write_header(&mut full_body);
        // 1 row, coded length = 3 (code 0, code 254, literal escape 0xFF 0x42)
        // Wait: 0xFF followed by 0x42 is 4 coded bytes total.
        let coded_row = [0u8, 254u8, FSST_ESCAPE, 0x42u8];
        full_body.push(3); // len_bw = 3 -> value 4 fits in 3 bits
        pack_u32(&[coded_row.len() as u32], 3, &mut full_body).unwrap();
        full_body.extend_from_slice(&coded_row);

        let full_view = FsstBlockView::parse(&full_body, 1).unwrap();
        let (f_offs, f_bytes) = full_view.decode_all().unwrap();
        let mut expected_full = Vec::new();
        expected_full.extend_from_slice(&sym_entries[0].bytes);
        expected_full.extend_from_slice(&sym_entries[254].bytes);
        expected_full.push(0x42);
        assert_eq!(
            &f_bytes[f_offs[0] as usize..f_offs[1] as usize],
            expected_full.as_slice()
        );

        // 3. Corrupt cases: truncated escape and out-of-bounds symbol code.
        let build_raw_block = |sym_count: u8, slens: &[u8], syms: &[u8], coded: &[u8]| -> Vec<u8> {
            let mut buf = Vec::new();
            buf.push(sym_count);
            buf.extend_from_slice(slens);
            buf.extend_from_slice(syms);
            let rlen = coded.len() as u32;
            let bw = min_bits_u32(&[rlen]);
            buf.push(bw);
            pack_u32(&[rlen], bw, &mut buf).unwrap();
            buf.extend_from_slice(coded);
            buf
        };

        let trunc_escape = build_raw_block(1, &[1], b"x", &[0, FSST_ESCAPE]);
        let trunc_view = FsstBlockView::parse(&trunc_escape, 1).unwrap();
        assert!(matches!(
            trunc_view.decode_all(),
            Err(StatamError::Corrupt {
                reason: "truncated fsst literal escape",
            })
        ));
        assert!(matches!(
            trunc_view.decode_row(0, &mut Vec::new()),
            Err(StatamError::Corrupt {
                reason: "truncated fsst literal escape",
            })
        ));

        let oob_code = build_raw_block(2, &[1, 2], b"xyy", &[0, 2]);
        let oob_view = FsstBlockView::parse(&oob_code, 1).unwrap();
        assert!(matches!(
            oob_view.decode_all(),
            Err(StatamError::Corrupt {
                reason: "fsst symbol code out of bounds",
            })
        ));
        assert!(matches!(
            oob_view.decode_selected(&Bitmap::new_ones(1), None),
            Err(StatamError::Corrupt {
                reason: "fsst symbol code out of bounds",
            })
        ));

        // 4. Fallback near max_total_bytes: valid near limit succeeds, +1 byte rejects with BoundsExceeded.
        let (dec_tbl, _) =
            FsstDecodeTable::parse_header(&[1, 8, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'])
                .unwrap();
        let coded_64b = [0u8; 8]; // 8 * 8 = 64 bytes decoded
        let mut small_buf = Vec::new();
        let mut pos = 0usize;
        dec_tbl
            .decode_row_fast(&coded_64b, &mut small_buf, &mut pos, 64, true)
            .unwrap();
        assert_eq!(pos, 64);
        assert_eq!(&small_buf[..pos], b"abcdefgh".repeat(8).as_slice());

        // Trigger fallback (`pos + max_row > max_total_bytes`) where actual decoded fits (escape consumes 2 -> 1 byte).
        let mut mixed_coded = vec![0u8; 7]; // 56 bytes
        mixed_coded.extend_from_slice(&[FSST_ESCAPE, b'Z']); // +1 = 57 bytes, max_row = 9*8 = 72 > 64!
        let mut fallback_buf = Vec::new();
        let mut fb_pos = 0usize;
        dec_tbl
            .decode_row_fast(&mixed_coded, &mut fallback_buf, &mut fb_pos, 64, true)
            .unwrap();
        assert_eq!(fb_pos, 57);
        assert_eq!(
            &fallback_buf[..fb_pos],
            [b"abcdefgh".repeat(7), vec![b'Z']].concat().as_slice()
        );

        // Exceeds limit (72 > 64) -> BoundsExceeded
        let coded_72b = [0u8; 9];
        let mut over_buf = Vec::new();
        let mut over_pos = 0usize;
        assert!(matches!(
            dec_tbl.decode_row_fast(&coded_72b, &mut over_buf, &mut over_pos, 64, true),
            Err(StatamError::BoundsExceeded {
                reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
            })
        ));

        // Full MAX_BLOCK_RAW_BYTES (64 MiB) boundary via decode_all (non-Miri).
        #[cfg(not(miri))]
        {
            let exact_codes = MAX_BLOCK_RAW_BYTES / 8;
            let exact_block = build_raw_block(1, &[8], b"01234567", &vec![0u8; exact_codes]);
            let exact_view = FsstBlockView::parse(&exact_block, 1).unwrap();
            let (eo, eb) = exact_view.decode_all().unwrap();
            assert_eq!(eo, vec![0, MAX_BLOCK_RAW_BYTES as u32]);
            assert_eq!(eb.len(), MAX_BLOCK_RAW_BYTES);

            let over_block = build_raw_block(1, &[8], b"01234567", &vec![0u8; exact_codes + 1]);
            let over_view = FsstBlockView::parse(&over_block, 1).unwrap();
            assert!(matches!(
                over_view.decode_all(),
                Err(StatamError::BoundsExceeded {
                    reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                })
            ));
        }
    }

    #[test]
    fn decode_fsst_selected_and_row_into_parity_across_masks() {
        let mut owned = Vec::with_capacity(64);
        let mut valid_bits = Vec::with_capacity(64);
        for i in 0..64usize {
            if i % 5 == 0 {
                owned.push(String::new());
                valid_bits.push(false);
            } else {
                owned.push(format!(
                    "{{\"role\":\"assistant\",\"turn\":{i},\"model\":\"claude-3-5-sonnet\",\"text\":\"hello world {i}\"}}"
                ));
                valid_bits.push(true);
            }
        }
        let refs: Vec<&[u8]> = owned.iter().map(String::as_bytes).collect();
        let body = encode_fsst_body(&refs).unwrap().unwrap();
        let nulls = Bitmap::from_bools(&valid_bits);
        let all_nulls = Bitmap::new_zeroed(refs.len());

        let full_u = decode_fsst_utf8(&body, refs.len()).unwrap();
        let full_b = decode_fsst_binary(&body, refs.len()).unwrap();
        let view = FsstBlockView::parse(&body, refs.len()).unwrap();

        let empty_mask = Bitmap::new_zeroed(refs.len());
        let mut one_mask = Bitmap::new_zeroed(refs.len());
        let _ = one_mask.set(17, true);
        let mut scattered_mask = Bitmap::new_zeroed(refs.len());
        for idx in [0usize, 1, 5, 17, 32, 63] {
            let _ = scattered_mask.set(idx, true);
        }
        let all_mask = Bitmap::new_ones(refs.len());

        let mut scratch = Vec::new();
        for (name, mask, row_nulls) in [
            ("empty", &empty_mask, Some(&nulls)),
            ("one_row", &one_mask, Some(&nulls)),
            ("scattered_with_nulls", &scattered_mask, Some(&nulls)),
            ("scattered_no_nulls", &scattered_mask, None),
            ("all_null", &scattered_mask, Some(&all_nulls)),
            ("all_selected", &all_mask, Some(&nulls)),
        ] {
            let (sel_u, sel_u_nulls) =
                decode_fsst_utf8_selected(&body, refs.len(), mask, row_nulls).unwrap();
            let (sel_b, sel_b_nulls) =
                decode_fsst_binary_selected(&body, refs.len(), mask, row_nulls).unwrap();
            let u_nulls = sel_u_nulls.unwrap();
            let b_nulls = sel_b_nulls.unwrap();
            assert_eq!(sel_u.len(), refs.len(), "{name}");
            assert_eq!(sel_b.len(), refs.len(), "{name}");
            for (r, &expected_slice) in refs.iter().enumerate() {
                let expected_valid = mask.get(r) && row_nulls.is_none_or(|bm| bm.get(r));
                assert_eq!(u_nulls.get(r), expected_valid, "{name} row {r}");
                assert_eq!(b_nulls.get(r), expected_valid, "{name} row {r}");
                if expected_valid {
                    assert_eq!(sel_u.get(r), full_u.get(r), "{name} row {r}");
                    assert_eq!(sel_b.get(r), full_b.get(r), "{name} row {r}");
                    let slice = view.decode_row_into(r, &mut scratch).unwrap();
                    assert_eq!(slice, expected_slice, "{name} row {r}");
                }
            }
        }
    }

    #[test]
    fn decode_row_at_20k_100b_is_linear_and_exact() {
        let row_count = 20_000usize;
        let mut owned = Vec::with_capacity(row_count);
        for i in 0..row_count {
            owned.push(format!(
                "https://observability.cetra.sh/api/v1/spans/trace_{i:06}/events/item_{:04}/payload_end",
                i % 997
            ));
        }
        let refs: Vec<&[u8]> = owned.iter().map(String::as_bytes).collect();
        let body = encode_fsst_body(&refs).unwrap().unwrap();
        let view = FsstBlockView::parse(&body, row_count).unwrap();

        let mut out_bytes = Vec::new();
        let mut out_pos = 0usize;
        let mut out_offsets = Vec::with_capacity(row_count + 1);
        out_offsets.push(0u32);
        for row in 0..row_count {
            view.decode_row_at(row, &mut out_bytes, &mut out_pos)
                .unwrap();
            out_offsets.push(out_pos as u32);
        }
        out_bytes.truncate(out_pos);

        assert_eq!(out_offsets.len(), row_count + 1);
        for (i, expected) in refs.iter().enumerate() {
            let s = out_offsets[i] as usize;
            let e = out_offsets[i + 1] as usize;
            assert_eq!(&out_bytes[s..e], *expected);
        }
    }

    #[test]
    fn subchunked_fsst_roundtrip_selected_take_and_eval_parity_across_profiles() {
        // Small block (<= 512 rows) must stay single-chunk.
        let small_owned: Vec<String> = (0..512usize)
            .map(|i| {
                format!(
                    "https://observability.cetra.sh/api/v1/spans/trace_{i:06}/events/item_{:04}",
                    i % 97
                )
            })
            .collect();
        let small_refs: Vec<&[u8]> = small_owned.iter().map(String::as_bytes).collect();
        let (_, small_sub) = encode_fsst_body_adaptive(&small_refs, WriteProfile::L1Compact)
            .unwrap()
            .unwrap();
        assert!(!small_sub, "row_count <= 512 must not sub-chunk");

        // > 512 rows AND >= 16 KiB coded bytes -> triggers 256-row sub-chunks in both profiles.
        let row_count = 1_030usize;
        let mut owned = Vec::with_capacity(row_count);
        let mut valid_bits = Vec::with_capacity(row_count);
        for i in 0..row_count {
            if i % 29 == 0 {
                owned.push(String::new());
                valid_bits.push(false);
            } else {
                owned.push(format!(
                    "https://observability.cetra.sh/api/v1/spans/trace_{i:06}/events/item_{:04}/marker_{}",
                    i % 197,
                    if i % 41 == 0 { "NEEDLE_HIT" } else { "normal" }
                ));
                valid_bits.push(true);
            }
        }
        let refs: Vec<&[u8]> = owned.iter().map(String::as_bytes).collect();
        let nulls = Bitmap::from_bools(&valid_bits);

        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let (body, is_subchunked) = encode_fsst_body_adaptive(&refs, profile)
                .unwrap()
                .expect("repetitive URLs must compress with FSST");
            assert!(is_subchunked, "{profile:?} must trigger sub-chunked FSST");

            let view = FsstSubchunkedView::parse(&body, row_count).unwrap();
            assert_eq!(view.subchunk_rows, usize::from(FSST_SUBCHUNK_ROWS));
            assert_eq!(
                view.chunks.len(),
                row_count.div_ceil(usize::from(FSST_SUBCHUNK_ROWS))
            );
            if profile == WriteProfile::L0Fast {
                assert!(
                    view.chunks.iter().all(|c| c.codec != Codec::Zstd),
                    "L0Fast sub-chunks must never use Zstd"
                );
            }

            // Full decode parity (Utf8 & Binary)
            let full_u =
                decode_fsst_utf8_subchunked(&body, row_count, MAX_BLOCK_RAW_BYTES).unwrap();
            let full_b =
                decode_fsst_binary_subchunked(&body, row_count, MAX_BLOCK_RAW_BYTES).unwrap();
            assert_eq!(full_u.len(), row_count);
            assert_eq!(full_b.len(), row_count);
            for (r, &expected) in refs.iter().enumerate() {
                assert_eq!(full_u.get(r).unwrap().as_bytes(), expected);
                assert_eq!(full_b.get(r).unwrap(), expected);
            }

            // Sparse selected parity across multiple sub-chunks (including null rows)
            let mut mask = Bitmap::new_zeroed(row_count);
            for idx in [0usize, 1, 29, 255, 256, 257, 511, 512, 767, 768, 1029] {
                let _ = mask.set(idx, true);
            }
            let (sel_u, sel_nulls) = decode_fsst_utf8_selected_with_flag(
                &body,
                row_count,
                &mask,
                Some(&nulls),
                true,
                MAX_BLOCK_RAW_BYTES,
            )
            .unwrap();
            let sel_nulls = sel_nulls.unwrap();
            for (r, &expected) in refs.iter().enumerate() {
                let want_valid = mask.get(r) && nulls.get(r);
                assert_eq!(sel_nulls.get(r), want_valid, "{profile:?} row {r}");
                if want_valid {
                    assert_eq!(sel_u.get(r).unwrap().as_bytes(), expected);
                }
            }

            // Sorted + unsorted/duplicate take parity
            for take_indices in [
                vec![0usize, 1, 29, 256, 512, 768, 1029],
                vec![1029usize, 29, 0, 512, 1029, 256, 1, 29],
            ] {
                let (taken_u, taken_nulls) =
                    decode_fsst_utf8_take(&body, row_count, &take_indices, Some(&nulls), true)
                        .unwrap();
                assert_eq!(taken_u.len(), take_indices.len());
                for (pos, &r) in take_indices.iter().enumerate() {
                    let is_valid = taken_nulls.as_ref().is_none_or(|bm| bm.get(pos));
                    assert_eq!(is_valid, nulls.get(r), "{profile:?} take pos {pos}");
                    if is_valid {
                        assert_eq!(taken_u.get(pos).unwrap().as_bytes(), refs[r]);
                    }
                }
            }

            // eval_substring parity
            let mut active = Bitmap::new_zeroed(row_count);
            for r in (0..row_count).step_by(3) {
                let _ = active.set(r, true);
            }
            let hits = view
                .eval_substring(&active, Some(&nulls), |s| {
                    s.windows(b"NEEDLE_HIT".len()).any(|w| w == b"NEEDLE_HIT")
                })
                .unwrap();
            for (r, s) in owned.iter().enumerate() {
                let expected_hit = active.get(r) && nulls.get(r) && s.contains("NEEDLE_HIT");
                assert_eq!(hits.get(r), expected_hit, "{profile:?} eval row {r}");
            }
        }
    }

    #[test]
    fn subchunked_fsst_rejects_zip_bomb_and_corrupt_headers() {
        // Minimal valid 1-chunk sub-chunked FSST body for 2 rows:
        // table: 1 symbol of len 1 ('a'), subchunk_rows=2, num_chunks=1.
        let mut raw_sub = Vec::new();
        raw_sub.push(1u8); // len_bw = 1 -> lengths [1, 1]
        pack_u32(&[1, 1], 1, &mut raw_sub).unwrap();
        raw_sub.extend_from_slice(&[0u8, 0u8]); // coded bytes: "a", "a"

        let build = |subchunk_rows: u16,
                     num_chunks: u16,
                     entries: &[(u8, u32, u32)],
                     payload: &[u8]|
         -> Vec<u8> {
            let mut buf = vec![1u8, 1u8, b'a']; // sym_count=1, sym_len=1, sym='a'
            buf.extend_from_slice(&subchunk_rows.to_le_bytes());
            buf.extend_from_slice(&num_chunks.to_le_bytes());
            for &(codec, raw_len, stored_end) in entries {
                buf.push(codec);
                buf.extend_from_slice(&raw_len.to_le_bytes());
                buf.extend_from_slice(&stored_end.to_le_bytes());
            }
            buf.extend_from_slice(payload);
            buf
        };

        // Sanity: valid None sub-chunk decodes cleanly.
        let valid = build(
            2,
            1,
            &[(
                Codec::None as u8,
                raw_sub.len() as u32,
                raw_sub.len() as u32,
            )],
            &raw_sub,
        );
        let u = decode_fsst_utf8_subchunked(&valid, 2, MAX_BLOCK_RAW_BYTES).unwrap();
        assert_eq!(u.get(0), Some("a"));
        assert_eq!(u.get(1), Some("a"));

        // 1. Zip-bomb sub_raw_len (> MAX_DECOMPRESS_RATIO * stored + slack).
        let lz4_sub = lz4_flex::block::compress(&raw_sub);
        let zip_bomb = build(
            2,
            1,
            &[(Codec::Lz4 as u8, 32 * 1024 * 1024, lz4_sub.len() as u32)],
            &lz4_sub,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&zip_bomb, 2),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 2. Trailing bytes (`stored_end < payload.len()`).
        let mut trailing_payload = raw_sub.clone();
        trailing_payload.push(0x99);
        let trailing = build(
            2,
            1,
            &[(
                Codec::None as u8,
                raw_sub.len() as u32,
                raw_sub.len() as u32,
            )],
            &trailing_payload,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&trailing, 2),
            Err(StatamError::Corrupt {
                reason: "trailing bytes in fsst subchunk payload",
            })
        ));

        // 3. Non-monotonic stored_end.
        let mut two_payload = raw_sub.clone();
        two_payload.extend_from_slice(&raw_sub);
        let non_mono = build(
            1,
            2,
            &[
                (
                    Codec::None as u8,
                    raw_sub.len() as u32,
                    raw_sub.len() as u32,
                ),
                (Codec::None as u8, 0, (raw_sub.len() - 1) as u32),
            ],
            &two_payload,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&non_mono, 2),
            Err(StatamError::Corrupt {
                reason: "invalid fsst subchunk stored_end",
            })
        ));

        // 4. Unknown sub-chunk codec.
        let bad_codec = build(
            2,
            1,
            &[(99u8, raw_sub.len() as u32, raw_sub.len() as u32)],
            &raw_sub,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&bad_codec, 2),
            Err(StatamError::Corrupt {
                reason: "unknown compression codec",
            })
        ));

        // 5. Codec::None with raw_len != stored_len.
        let bad_none_len = build(
            2,
            1,
            &[(
                Codec::None as u8,
                (raw_sub.len() + 1) as u32,
                raw_sub.len() as u32,
            )],
            &raw_sub,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&bad_none_len, 2),
            Err(StatamError::Corrupt {
                reason: "uncompressed fsst subchunk raw_len mismatch",
            })
        ));

        // 6. Invalid subchunk_rows (0 or > 8192) and num_chunks mismatch.
        let zero_rows = build(
            0,
            1,
            &[(
                Codec::None as u8,
                raw_sub.len() as u32,
                raw_sub.len() as u32,
            )],
            &raw_sub,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&zero_rows, 2),
            Err(StatamError::Corrupt { .. })
        ));
        let huge_rows = build(
            8193,
            1,
            &[(
                Codec::None as u8,
                raw_sub.len() as u32,
                raw_sub.len() as u32,
            )],
            &raw_sub,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&huge_rows, 2),
            Err(StatamError::Corrupt { .. })
        ));
        let wrong_chunks = build(
            1,
            1,
            &[(
                Codec::None as u8,
                raw_sub.len() as u32,
                raw_sub.len() as u32,
            )],
            &raw_sub,
        );
        assert!(matches!(
            FsstSubchunkedView::parse(&wrong_chunks, 2),
            Err(StatamError::Corrupt {
                reason: "fsst subchunk num_chunks mismatch",
            })
        ));
    }

    #[cfg(not(miri))]
    #[test]
    fn subchunked_fsst_l1_overhead_guard_falls_back_to_single_chunk() {
        // 4 identical cycles of 512 distinct URL-like strings: each 512-row sub-chunk has
        // distinct rows, while whole-block Zstd deduplicates across the 4 cycles by > 10%.
        let cycle = usize::from(FSST_SUBCHUNK_ROWS);
        let row_count = cycle * 4;
        let mut owned = Vec::with_capacity(row_count);
        for i in 0..row_count {
            let k = i % cycle;
            let h1 = (k as u32).wrapping_mul(2654435761);
            let h2 = (k as u32).wrapping_mul(2246822519);
            owned.push(format!(
                "https://clickbench.example.org/catalog/category_{k:04}/item_{h1:08x}/ref_{h2:08x}/utm_source=organic_search"
            ));
        }
        let refs: Vec<&[u8]> = owned.iter().map(String::as_bytes).collect();
        let (body, is_subchunked) = encode_fsst_body_adaptive(&refs, WriteProfile::L1Compact)
            .unwrap()
            .expect("URLs must compress with FSST");
        assert!(
            !is_subchunked,
            "L1 size-overhead guard must fall back to single-chunk FSST when whole-block Zstd beats sub-chunks by > 10%"
        );
        let decoded = decode_fsst_utf8(&body, row_count).unwrap();
        assert_eq!(decoded.len(), row_count);
        for (r, &expected) in refs.iter().enumerate() {
            assert_eq!(decoded.get(r).unwrap().as_bytes(), expected);
        }
    }

    #[test]
    fn pass16b_fsst_train_and_encode_row_determinism_short_and_incompressible() {
        // 1. Repetitive structured strings: training twice yields bit-identical table + coded bytes,
        //    and encoding every boundary length 0..=16 round-trips cleanly.
        let corpus: Vec<String> = (0..256usize)
            .map(|i| {
                format!(
                    "{{\"trace_id\":\"0196a3f0-{i:04x}\",\"service\":\"checkout-api\",\"status\":\"ok\",\"latency_ms\":{}}}",
                    i % 37
                )
            })
            .collect();
        let refs: Vec<&[u8]> = corpus.iter().map(String::as_bytes).collect();
        let t1 = FsstTable::train(&refs);
        let t2 = FsstTable::train(&refs);
        assert_eq!(t1, t2);
        assert!(t1.sym_count() > 0 && t1.sym_count() <= FSST_MAX_SYMBOLS);

        let body1 = encode_fsst_body(&refs).unwrap().unwrap();
        let body2 = encode_fsst_body(&refs).unwrap().unwrap();
        assert_eq!(body1, body2);

        // Verify encode_row on lengths 0..=16 and longer strings (hot 8-byte loop + tail).
        let sample_full = corpus[0].as_bytes();
        let mut dec_tbl_bytes = Vec::new();
        t1.write_header(&mut dec_tbl_bytes);
        let (dec_tbl, _) = FsstDecodeTable::parse_header(&dec_tbl_bytes).unwrap();
        for len in 0..=sample_full.len().min(32) {
            let slice = &sample_full[..len];
            let mut coded_fast = Vec::new();
            t1.encode_row(slice, &mut coded_fast);

            // Reference scalar loop via find_longest_match must be 100% bit-identical.
            let mut coded_ref = Vec::new();
            let mut pos = 0usize;
            while pos < slice.len() {
                if let Some((code, mlen)) = t1.find_longest_match(&slice[pos..]) {
                    coded_ref.push(code);
                    pos += mlen;
                } else {
                    coded_ref.push(FSST_ESCAPE);
                    coded_ref.push(slice[pos]);
                    pos += 1;
                }
            }
            assert_eq!(coded_fast, coded_ref, "encode_row mismatch at len={len}");

            let mut decoded = Vec::new();
            let mut dpos = 0usize;
            dec_tbl
                .decode_row_fast(
                    &coded_fast,
                    &mut decoded,
                    &mut dpos,
                    MAX_BLOCK_RAW_BYTES,
                    true,
                )
                .unwrap();
            assert_eq!(&decoded[..dpos], slice, "roundtrip mismatch at len={len}");
        }

        // 2. High-cardinality incompressible bytes must bail out cleanly without panics.
        let mut x = 0x1234_5678_9abc_def0u64;
        let random_rows: Vec<Vec<u8>> = (0..128)
            .map(|_| {
                let mut row = Vec::with_capacity(96);
                for _ in 0..12 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    row.extend_from_slice(&x.to_le_bytes());
                }
                row
            })
            .collect();
        let rand_refs: Vec<&[u8]> = random_rows.iter().map(Vec::as_slice).collect();
        assert!(encode_fsst_body(&rand_refs).unwrap().is_none());
    }
}
