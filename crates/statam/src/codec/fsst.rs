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

use crate::simd::bitpack::{min_bits_u32, pack_u32, packed_bytes_len, unpack_u32};
use crate::{MAX_BLOCK_RAW_BYTES, StatamError};

/// Literal escape byte (`255`); valid symbol codes are `0..sym_count` with `sym_count <= 255`.
pub const FSST_ESCAPE: u8 = 0xFF;

/// Maximum symbols in an FSST symbol table.
pub const FSST_MAX_SYMBOLS: usize = 255;

/// Maximum length of a single FSST symbol in bytes.
pub const FSST_MAX_SYM_LEN: usize = 8;

/// Maximum sample bytes consumed during FSST training.
const FSST_SAMPLE_BYTES: usize = 64 * 1024;

/// Trained FSST static symbol table (`<= 255` symbols of `1..=8` bytes).
#[derive(Clone, PartialEq, Eq)]
pub struct FsstTable {
    sym_lens: Vec<u8>,
    sym_bytes: Vec<u8>,
    sym_offsets: Vec<u16>,
    len1_code: [u8; 256],
    buckets: Vec<Vec<u8>>,
    sym_words: [u64; 256],
    sym_masks: [u64; 256],
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
            buckets: vec![Vec::new(); 256],
            sym_words: [0u64; 256],
            sym_masks: [0u64; 256],
        }
    }

    fn from_entries(entries: &[SymEntry]) -> Self {
        let count = entries.len().min(FSST_MAX_SYMBOLS);
        let mut sym_lens = Vec::with_capacity(count);
        let mut sym_offsets = Vec::with_capacity(count + 1);
        let mut sym_bytes = Vec::with_capacity(count * 4);
        let mut len1_code = [FSST_ESCAPE; 256];
        let mut buckets = vec![Vec::new(); 256];
        let mut sym_words = [0u64; 256];
        let mut sym_masks = [0u64; 256];

        sym_offsets.push(0);
        for (code, entry) in entries[..count].iter().enumerate() {
            let len = entry.len;
            debug_assert!((1..=FSST_MAX_SYM_LEN as u8).contains(&len));
            let len_usize = usize::from(len);
            sym_lens.push(len);
            sym_bytes.extend_from_slice(&entry.bytes[..len_usize]);
            sym_offsets.push(sym_bytes.len() as u16);

            let mut word_bytes = [0u8; FSST_MAX_SYM_LEN];
            word_bytes[..len_usize].copy_from_slice(&entry.bytes[..len_usize]);
            sym_words[code] = u64::from_le_bytes(word_bytes);
            sym_masks[code] = if len_usize == FSST_MAX_SYM_LEN {
                u64::MAX
            } else {
                (1u64 << (len_usize * 8)) - 1
            };

            let code_u8 = code as u8;
            if len == 1 {
                len1_code[usize::from(entry.bytes[0])] = code_u8;
            } else {
                buckets[usize::from(entry.bytes[0])].push(code_u8);
            }
        }

        // Sort each bucket by descending symbol length so first match is longest.
        for bucket in &mut buckets {
            bucket.sort_by(|&a, &b| sym_lens[usize::from(b)].cmp(&sym_lens[usize::from(a)]));
        }

        Self {
            sym_lens,
            sym_bytes,
            sym_offsets,
            len1_code,
            buckets,
            sym_words,
            sym_masks,
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
        let bucket = &self.buckets[usize::from(first)];
        if input.len() >= 8 {
            let in_word = u64::from_le_bytes([
                input[0], input[1], input[2], input[3], input[4], input[5], input[6], input[7],
            ]);
            for &code in bucket {
                let c = usize::from(code);
                if (in_word & self.sym_masks[c]) == self.sym_words[c] {
                    return Some((code, usize::from(self.sym_lens[c])));
                }
            }
        } else if input.len() >= 2 && !bucket.is_empty() {
            let mut buf = [0u8; FSST_MAX_SYM_LEN];
            buf[..input.len()].copy_from_slice(input);
            let in_word = u64::from_le_bytes(buf);
            for &code in bucket {
                let c = usize::from(code);
                let slen = usize::from(self.sym_lens[c]);
                if slen <= input.len() && (in_word & self.sym_masks[c]) == self.sym_words[c] {
                    return Some((code, slen));
                }
            }
        }
        let c1 = self.len1_code[usize::from(first)];
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

        let mut table = Self::empty();
        for round in 0..3 {
            let cur_samples = if round == 0 { round0_samples } else { &samples };
            table = train_round(&table, cur_samples);
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
        while pos < input.len() {
            if let Some((code, len)) = self.find_longest_match(&input[pos..]) {
                out.push(code);
                pos += len;
            } else {
                out.push(FSST_ESCAPE);
                out.push(input[pos]);
                pos += 1;
            }
        }
    }

    /// Decodes a single row's FSST-coded bytes into `out`, enforcing `max_total_bytes`.
    pub fn decode_row_checked(
        &self,
        coded: &[u8],
        out: &mut Vec<u8>,
        max_total_bytes: usize,
    ) -> Result<(), StatamError> {
        let mut i = 0usize;
        let sym_count = self.sym_count();
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
                let code = usize::from(b);
                if code >= sym_count {
                    return Err(StatamError::Corrupt {
                        reason: "fsst symbol code out of bounds",
                    });
                }
                let sym = self.symbol_slice(code);
                if out
                    .len()
                    .checked_add(sym.len())
                    .is_none_or(|n| n > max_total_bytes)
                {
                    return Err(StatamError::BoundsExceeded {
                        reason: "fsst decoded size exceeds MAX_BLOCK_RAW_BYTES",
                    });
                }
                out.extend_from_slice(sym);
                i += 1;
            }
        }
        Ok(())
    }

    /// Parses the leading symbol table from `src`, returning `(table, bytes_consumed)`.
    pub fn parse_header(src: &[u8]) -> Result<(Self, usize), StatamError> {
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

        let mut entries = Vec::with_capacity(sym_count);
        let mut off = lens_end;
        for &slen in sym_lens_slice {
            let len_usize = usize::from(slen);
            let mut bytes = [0u8; FSST_MAX_SYM_LEN];
            bytes[..len_usize].copy_from_slice(&src[off..off + len_usize]);
            entries.push(SymEntry { len: slen, bytes });
            off += len_usize;
        }
        Ok((Self::from_entries(&entries), syms_end))
    }

    fn write_header(&self, out: &mut Vec<u8>) {
        out.push(self.sym_count() as u8);
        out.extend_from_slice(&self.sym_lens);
        out.extend_from_slice(&self.sym_bytes);
    }
}

#[inline]
fn token_bytes(table: &FsstTable, token: u16) -> ([u8; FSST_MAX_SYM_LEN], u8) {
    let mut buf = [0u8; FSST_MAX_SYM_LEN];
    if token < 256 {
        let s = table.symbol_slice(usize::from(token));
        buf[..s.len()].copy_from_slice(s);
        (buf, s.len() as u8)
    } else {
        buf[0] = (token - 256) as u8;
        (buf, 1)
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
struct FastBytesKeyHasher(u64);

impl std::hash::Hasher for FastBytesKeyHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        self.0 = h;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

fn train_round(table: &FsstTable, samples: &[&[u8]]) -> FsstTable {
    let mut count1 = vec![0u32; 512];
    let mut count2: HashMap<u32, u32, BuildHasherDefault<FastU32Hasher>> = HashMap::default();

    for &sample in samples {
        let mut pos = 0usize;
        let mut prev_token: Option<(u16, u8)> = None;
        while pos < sample.len() {
            let (token, len) = if let Some((code, mlen)) = table.find_longest_match(&sample[pos..])
            {
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

    // Collect deduplicated candidate symbols with their estimated byte gain.
    let mut candidates: HashMap<
        (u8, [u8; FSST_MAX_SYM_LEN]),
        u64,
        BuildHasherDefault<FastBytesKeyHasher>,
    > = HashMap::default();

    for (tok, &cnt) in count1.iter().enumerate() {
        if cnt == 0 {
            continue;
        }
        let (bytes, len) = token_bytes(table, tok as u16);
        // Each hit replaces `len` raw bytes (or `2` escaped bytes) with a 1-byte code.
        let gain = u64::from(cnt) * u64::from(len);
        let e = candidates.entry((len, bytes)).or_insert(0);
        if gain > *e {
            *e = gain;
        }
    }

    for (&pair_key, &cnt) in &count2 {
        if cnt < 2 {
            continue;
        }
        let pt = (pair_key >> 10) as u16;
        let ct = (pair_key & 0x3FF) as u16;
        let (pbytes, plen) = token_bytes(table, pt);
        let (cbytes, clen) = token_bytes(table, ct);
        let combined_len = plen + clen;
        if usize::from(combined_len) > FSST_MAX_SYM_LEN {
            continue;
        }
        let mut combined = [0u8; FSST_MAX_SYM_LEN];
        combined[..usize::from(plen)].copy_from_slice(&pbytes[..usize::from(plen)]);
        combined[usize::from(plen)..usize::from(combined_len)]
            .copy_from_slice(&cbytes[..usize::from(clen)]);
        let gain = u64::from(cnt) * u64::from(combined_len);
        let e = candidates.entry((combined_len, combined)).or_insert(0);
        if gain > *e {
            *e = gain;
        }
    }

    let mut ranked: Vec<((u8, [u8; FSST_MAX_SYM_LEN]), u64)> = candidates.into_iter().collect();
    ranked.sort_unstable_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.0.0.cmp(&a.0.0))
            .then_with(|| a.0.1.cmp(&b.0.1))
    });
    ranked.truncate(FSST_MAX_SYMBOLS);

    let entries: Vec<SymEntry> = ranked
        .into_iter()
        .map(|((len, bytes), _)| SymEntry { len, bytes })
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

/// Encodes `values` (without leading row null bitmap) using FSST.
///
/// Returns `Ok(None)` if `values` is empty, has zero raw bytes, or achieves a
/// compression ratio `< 1.15x` against plain offsets+bytes representation.
pub fn encode_fsst_body(values: &[&[u8]]) -> Result<Option<Vec<u8>>, StatamError> {
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

    let len_bw = min_bits_u32(&row_lens);
    let mut out = Vec::with_capacity(1 + table.sym_count() + table.sym_bytes.len() + 1);
    table.write_header(&mut out);
    out.push(len_bw);
    pack_u32(&row_lens, len_bw, &mut out)?;
    out.extend_from_slice(&coded_bytes);

    // Require >= 1.15x compression over raw string payload bytes.
    if raw_data_bytes.saturating_mul(100) < out.len().saturating_mul(115) {
        return Ok(None);
    }

    Ok(Some(out))
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
    ) -> Result<(), StatamError> {
        let max_row = coded_row.len().saturating_mul(FSST_MAX_SYM_LEN);
        if pos.saturating_add(max_row) <= max_total_bytes {
            let needed = pos.saturating_add(max_row).saturating_add(FSST_MAX_SYM_LEN);
            if needed > out_bytes.len() {
                let grown = out_bytes
                    .len()
                    .saturating_mul(2)
                    .max(needed)
                    .min(max_total_bytes.saturating_add(FSST_MAX_SYM_LEN));
                out_bytes.resize(grown, 0);
            }
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
                    out_bytes[p] = coded_row[i + 1];
                    p += 1;
                    i += 2;
                } else {
                    let slen = self.lens[usize::from(b)];
                    if slen == FSST_ESCAPE {
                        return Err(StatamError::Corrupt {
                            reason: "fsst symbol code out of bounds",
                        });
                    }
                    out_bytes[p..p + FSST_MAX_SYM_LEN].copy_from_slice(&self.words[usize::from(b)]);
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
        self.table
            .decode_row_fast(&self.coded[start..end], out, &mut pos, MAX_BLOCK_RAW_BYTES)?;
        out.truncate(pos);
        Ok(())
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

/// Decodes only `mask`-selected rows of an FSST-encoded block body into `(Utf8Data, nulls)`.
pub fn decode_fsst_utf8_selected(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
) -> Result<(Utf8Data, Option<Bitmap>), StatamError> {
    let view = FsstBlockView::parse(src, row_count)?;
    let (offsets, bytes, out_nulls) = view.decode_selected(mask, nulls)?;
    Ok((Utf8Data::try_new(offsets, bytes)?, out_nulls))
}

/// Decodes only `mask`-selected rows of an FSST-encoded block body into `(BinaryData, nulls)`.
pub fn decode_fsst_binary_selected(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
) -> Result<(BinaryData, Option<Bitmap>), StatamError> {
    let view = FsstBlockView::parse(src, row_count)?;
    let (offsets, bytes, out_nulls) = view.decode_selected(mask, nulls)?;
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
            .decode_row_fast(&coded_64b, &mut small_buf, &mut pos, 64)
            .unwrap();
        assert_eq!(pos, 64);
        assert_eq!(&small_buf[..pos], b"abcdefgh".repeat(8).as_slice());

        // Trigger fallback (`pos + max_row > max_total_bytes`) where actual decoded fits (escape consumes 2 -> 1 byte).
        let mut mixed_coded = vec![0u8; 7]; // 56 bytes
        mixed_coded.extend_from_slice(&[FSST_ESCAPE, b'Z']); // +1 = 57 bytes, max_row = 9*8 = 72 > 64!
        let mut fallback_buf = Vec::new();
        let mut fb_pos = 0usize;
        dec_tbl
            .decode_row_fast(&mixed_coded, &mut fallback_buf, &mut fb_pos, 64)
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
            dec_tbl.decode_row_fast(&coded_72b, &mut over_buf, &mut over_pos, 64),
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
}
