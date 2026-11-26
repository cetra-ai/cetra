//! File-level dictionary builder, `DT01` wire codec, and `Dict` mini-block codec.
//!
//! Supports `L0Fast` insertion-order dictionaries (bounded `<= 4096` entries,
//! bailing out cleanly if exceeded) and `L1Compact` lexicographically sorted
//! dictionaries, plus zero-expansion `Column::Dict` pass-through up to 256 KiB.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasherDefault;

#[derive(Default)]
struct FastSliceHasher(u64);

impl std::hash::Hasher for FastSliceHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        let (chunks, rem) = bytes.as_chunks::<8>();
        for &c in chunks {
            let v = u64::from_le_bytes(c);
            h = (h ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
        if !rem.is_empty() {
            let mut tail = [0u8; 8];
            tail[..rem.len()].copy_from_slice(rem);
            let v = u64::from_le_bytes(tail);
            h = (h ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
        self.0 = h;
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.0 = (self.0 ^ (i as u64)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }

    #[inline]
    fn finish(&self) -> u64 {
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

use cetra_column::{
    BinaryData, Bitmap, Column, DataType, DictColumn, FlatColumn, FlatData, Utf8Data,
};

use crate::format::{Codec, DICT_MAGIC, compress_guarded, decompress_checked};
use crate::simd::bitpack::{min_bits_u32, pack_u32, packed_bytes_len, unpack_u32};
use crate::{MAX_BLOCK_RAW_BYTES, MAX_DICT_BYTES, StatamError};

/// Header length of a `DT01` dictionary section (20 bytes).
pub const DICT_HEADER_LEN: usize = 20;

/// Maximum dictionary `memory_bytes()` for zero-expansion `Column::Dict` pass-through (256 KiB).
pub const DICT_PASSTHROUGH_MAX_BYTES: usize = 256 * 1024;

/// Maximum distinct entries permitted in an `L0Fast` dictionary.
pub const L0_DICT_MAX_ENTRIES: usize = 4_096;

/// Value representation inside a [`FileDict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DictValueKind {
    /// UTF-8 variable-length strings.
    Utf8 = 0,
    /// Variable-length raw binary bytes.
    Binary = 1,
    /// Fixed 8-byte values.
    Fixed8 = 2,
    /// Fixed 16-byte values.
    Fixed16 = 3,
}

impl DictValueKind {
    /// Parses a wire `u8` tag into [`DictValueKind`].
    pub fn from_u8(v: u8) -> Result<Self, StatamError> {
        match v {
            0 => Ok(Self::Utf8),
            1 => Ok(Self::Binary),
            2 => Ok(Self::Fixed8),
            3 => Ok(Self::Fixed16),
            _ => Err(StatamError::Corrupt {
                reason: "unknown dictionary value kind",
            }),
        }
    }

    /// Maps a [`DataType`] to its [`DictValueKind`], if dictionary-encodable.
    pub fn from_data_type(dt: &DataType) -> Option<Self> {
        match dt {
            DataType::Utf8 => Some(Self::Utf8),
            DataType::Binary => Some(Self::Binary),
            DataType::FixedBytes8 => Some(Self::Fixed8),
            DataType::FixedBytes16 => Some(Self::Fixed16),
            _ => None,
        }
    }
}

/// File-level (or inline-list) dictionary storing distinct non-null entries.
#[derive(Clone, PartialEq)]
pub struct FileDict {
    values: FlatColumn,
    sorted: bool,
}

impl fmt::Debug for FileDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileDict")
            .field("entry_count", &self.values.len())
            .field("data_type", &self.values.data_type())
            .field("sorted", &self.sorted)
            .field("memory_bytes", &self.values.memory_bytes())
            .finish()
    }
}

impl FileDict {
    /// Constructs a [`FileDict`] from a validated non-null `FlatColumn`.
    pub fn from_flat(values: FlatColumn, sorted: bool) -> Result<Self, StatamError> {
        if values.has_nulls() {
            return Err(StatamError::SchemaMismatch {
                reason: "dictionary values cannot contain nulls",
            });
        }
        if DictValueKind::from_data_type(&values.data_type()).is_none() {
            return Err(StatamError::SchemaMismatch {
                reason: "unsupported dictionary value data type",
            });
        }
        Ok(Self { values, sorted })
    }

    /// Underlying `FlatColumn` holding the dictionary entries (`nulls == None`).
    pub fn values(&self) -> &FlatColumn {
        &self.values
    }

    /// Whether entries are sorted lexicographically by raw byte representation.
    pub fn is_sorted(&self) -> bool {
        self.sorted
    }

    /// Number of dictionary entries.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// True when the dictionary has zero entries.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Deterministic logical heap bytes of the dictionary values.
    pub fn memory_bytes(&self) -> usize {
        self.values.memory_bytes()
    }

    /// [`DictValueKind`] of this dictionary.
    pub fn value_kind(&self) -> DictValueKind {
        match self.values.data() {
            FlatData::Utf8(_) => DictValueKind::Utf8,
            FlatData::Binary(_) => DictValueKind::Binary,
            FlatData::FixedBytes8(_) => DictValueKind::Fixed8,
            FlatData::FixedBytes16(_) => DictValueKind::Fixed16,
            _ => unreachable!("validated on construction"),
        }
    }

    /// Returns the raw bytes for dictionary code `code`.
    pub fn entry_bytes(&self, code: u32) -> Option<&[u8]> {
        let idx = code as usize;
        match self.values.data() {
            FlatData::Utf8(u) => u.get(idx).map(str::as_bytes),
            FlatData::Binary(b) => b.get(idx),
            FlatData::FixedBytes8(v) => v.get(idx).map(|a| a.as_slice()),
            FlatData::FixedBytes16(v) => v.get(idx).map(|a| a.as_slice()),
            _ => None,
        }
    }

    /// Looks up `target` in the dictionary, using binary search when `sorted == true`.
    pub fn lookup_code(&self, target: &[u8]) -> Option<u32> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        if self.sorted {
            let mut lo = 0usize;
            let mut hi = len;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let entry = self.entry_bytes(mid as u32)?;
                match entry.cmp(target) {
                    Ordering::Less => lo = mid + 1,
                    Ordering::Greater => hi = mid,
                    Ordering::Equal => return Some(mid as u32),
                }
            }
            None
        } else {
            for i in 0..len {
                if self.entry_bytes(i as u32) == Some(target) {
                    return Some(i as u32);
                }
            }
            None
        }
    }

    /// Maps every non-null row of `col` to its code in `self`.
    ///
    /// Returns `Ok(None)` if any non-null value is absent from `self` or if `col`
    /// does not match `self.values().data_type()`.
    pub fn try_map_column(&self, col: &FlatColumn) -> Result<Option<Vec<u32>>, StatamError> {
        if col.data_type() != self.values.data_type() {
            return Ok(None);
        }
        let n = col.len();
        let nulls = col.nulls();
        let mut codes = Vec::with_capacity(n);

        if !self.sorted || (n >= 128 && self.len() >= 16) {
            let mut map: HashMap<&[u8], u32, BuildHasherDefault<FastSliceHasher>> =
                HashMap::with_capacity_and_hasher(self.len(), BuildHasherDefault::default());
            for i in 0..self.len() {
                if let Some(b) = self.entry_bytes(i as u32) {
                    map.insert(b, i as u32);
                }
            }
            for row in 0..n {
                if nulls.is_some_and(|bm| !bm.get(row)) {
                    codes.push(0);
                    continue;
                }
                let Some(val) = flat_row_bytes(col, row) else {
                    return Ok(None);
                };
                let Some(&code) = map.get(val) else {
                    return Ok(None);
                };
                codes.push(code);
            }
        } else {
            for row in 0..n {
                if nulls.is_some_and(|bm| !bm.get(row)) {
                    codes.push(0);
                    continue;
                }
                let Some(val) = flat_row_bytes(col, row) else {
                    return Ok(None);
                };
                let Some(code) = self.lookup_code(val) else {
                    return Ok(None);
                };
                codes.push(code);
            }
        }

        Ok(Some(codes))
    }

    /// Encodes the dictionary using the `DT01` 20-byte header + guarded compressed body.
    pub fn encode(&self, codec: Codec, zstd_level: i32) -> Result<Vec<u8>, StatamError> {
        let entry_count = u32::try_from(self.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "dictionary entry_count exceeds u32::MAX",
        })?;
        let kind = self.value_kind();

        let mut raw_body = Vec::new();
        match self.values.data() {
            FlatData::Utf8(u) => {
                raw_body.reserve((u.len() + 1) * 4 + u.bytes().len());
                for &off in u.offsets() {
                    raw_body.extend_from_slice(&off.to_le_bytes());
                }
                raw_body.extend_from_slice(u.bytes());
            }
            FlatData::Binary(b) => {
                raw_body.reserve((b.len() + 1) * 4 + b.bytes().len());
                for &off in b.offsets() {
                    raw_body.extend_from_slice(&off.to_le_bytes());
                }
                raw_body.extend_from_slice(b.bytes());
            }
            FlatData::FixedBytes8(v) => {
                raw_body.reserve(v.len() * 8);
                for item in v {
                    raw_body.extend_from_slice(item);
                }
            }
            FlatData::FixedBytes16(v) => {
                raw_body.reserve(v.len() * 16);
                for item in v {
                    raw_body.extend_from_slice(item);
                }
            }
            _ => unreachable!("validated on construction"),
        }

        if raw_body.len() > MAX_DICT_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "dictionary raw body exceeds MAX_DICT_BYTES",
            });
        }
        let raw_len = u32::try_from(raw_body.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "dictionary raw_len exceeds u32::MAX",
        })?;

        let (actual_codec, stored_body) = compress_guarded(&raw_body, codec, zstd_level)?;
        let crc = crc32c::crc32c(&stored_body);

        let mut out = Vec::with_capacity(DICT_HEADER_LEN + stored_body.len());
        out.extend_from_slice(&DICT_MAGIC);
        out.extend_from_slice(&entry_count.to_le_bytes());
        out.push(actual_codec as u8);
        out.push(kind as u8);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&raw_len.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&stored_body);
        Ok(out)
    }

    /// Decodes a `DT01` dictionary section, checking bounds & CRC-32C before allocating.
    pub fn decode(bytes: &[u8], sorted: bool) -> Result<Self, StatamError> {
        if bytes.len() < DICT_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "truncated DT01 dictionary header",
            });
        }
        if bytes[0..4] != DICT_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid DT01 dictionary magic",
            });
        }
        let entry_count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let codec = Codec::from_u8(bytes[8])?;
        let kind = DictValueKind::from_u8(bytes[9])?;
        let reserved = u16::from_le_bytes([bytes[10], bytes[11]]);
        if reserved != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in DT01 header",
            });
        }
        let raw_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
        let expected_crc = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);

        let stored_body = &bytes[DICT_HEADER_LEN..];
        let actual_crc = crc32c::crc32c(stored_body);
        if actual_crc != expected_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "dictionary section",
            });
        }

        let raw_body = decompress_checked(stored_body, codec, raw_len, MAX_DICT_BYTES)?;
        let data = match kind {
            DictValueKind::Utf8 | DictValueKind::Binary => {
                let offsets_count =
                    entry_count
                        .checked_add(1)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "dictionary entry_count overflow",
                        })?;
                let offsets_bytes =
                    offsets_count
                        .checked_mul(4)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "dictionary offsets size overflow",
                        })?;
                if raw_body.len() < offsets_bytes {
                    return Err(StatamError::Corrupt {
                        reason: "dictionary raw_len smaller than offset table",
                    });
                }
                let mut offsets = Vec::with_capacity(offsets_count);
                for &chunk in raw_body[..offsets_bytes].as_chunks::<4>().0 {
                    offsets.push(u32::from_le_bytes(chunk));
                }
                let data_bytes = raw_body[offsets_bytes..].to_vec();
                if kind == DictValueKind::Utf8 {
                    FlatData::Utf8(Utf8Data::try_new(offsets, data_bytes)?)
                } else {
                    FlatData::Binary(BinaryData::try_new(offsets, data_bytes)?)
                }
            }
            DictValueKind::Fixed8 => {
                let expected_bytes =
                    entry_count
                        .checked_mul(8)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "dictionary Fixed8 size overflow",
                        })?;
                if raw_body.len() != expected_bytes {
                    return Err(StatamError::Corrupt {
                        reason: "dictionary Fixed8 length mismatch",
                    });
                }
                let items = raw_body.as_chunks::<8>().0.to_vec();
                FlatData::FixedBytes8(items)
            }
            DictValueKind::Fixed16 => {
                let expected_bytes =
                    entry_count
                        .checked_mul(16)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "dictionary Fixed16 size overflow",
                        })?;
                if raw_body.len() != expected_bytes {
                    return Err(StatamError::Corrupt {
                        reason: "dictionary Fixed16 length mismatch",
                    });
                }
                let items = raw_body.as_chunks::<16>().0.to_vec();
                FlatData::FixedBytes16(items)
            }
        };

        let flat = FlatColumn::try_new(data, None)?;
        Ok(Self {
            values: flat,
            sorted,
        })
    }
}

/// Profile mode for [`DictBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DictBuildMode {
    /// Bounded insertion-order dictionary (`<= max_entries`, default 4096).
    L0Fast {
        /// Maximum distinct entries before bailing out.
        max_entries: usize,
    },
    /// Lexicographically sorted dictionary (`<= max_entries`, default unbounded).
    L1Compact {
        /// Maximum distinct entries before bailing out.
        max_entries: usize,
    },
}

/// Builds a [`FileDict`] and row codes from a [`FlatColumn`].
#[derive(Debug, Clone)]
pub struct DictBuilder {
    mode: DictBuildMode,
}

impl DictBuilder {
    /// Creates an `L0Fast` dictionary builder capped at `4096` entries.
    pub fn l0_fast() -> Self {
        Self {
            mode: DictBuildMode::L0Fast {
                max_entries: L0_DICT_MAX_ENTRIES,
            },
        }
    }

    /// Creates an `L0Fast` dictionary builder with a custom entry cap.
    pub fn l0_with_max(max_entries: usize) -> Self {
        Self {
            mode: DictBuildMode::L0Fast { max_entries },
        }
    }

    /// Creates an `L1Compact` sorted dictionary builder.
    pub fn l1_compact() -> Self {
        Self {
            mode: DictBuildMode::L1Compact {
                max_entries: usize::MAX,
            },
        }
    }

    /// Creates an `L1Compact` sorted dictionary builder with a custom entry cap.
    pub fn l1_with_max(max_entries: usize) -> Self {
        Self {
            mode: DictBuildMode::L1Compact { max_entries },
        }
    }

    /// Builds a `(FileDict, codes)` pair from `col`.
    ///
    /// Returns `Ok(None)` if `col` is not dictionary-encodable, has zero non-null rows,
    /// or exceeds the `L0Fast` entry cap.
    pub fn build(&self, col: &FlatColumn) -> Result<Option<(FileDict, Vec<u32>)>, StatamError> {
        let Some(kind) = DictValueKind::from_data_type(&col.data_type()) else {
            return Ok(None);
        };
        let n = col.len();
        if n == 0 {
            return Ok(None);
        }
        let nulls = col.nulls();

        let max_entries = match self.mode {
            DictBuildMode::L0Fast { max_entries } | DictBuildMode::L1Compact { max_entries } => {
                max_entries
            }
        };

        let mut seen: HashMap<&[u8], u32, BuildHasherDefault<FastSliceHasher>> = HashMap::default();
        let mut unique: Vec<&[u8]> = Vec::new();
        let mut codes = Vec::with_capacity(n);
        let mut any_valid = false;

        for row in 0..n {
            if nulls.is_some_and(|bm| !bm.get(row)) {
                codes.push(0);
                continue;
            }
            any_valid = true;
            let Some(val) = flat_row_bytes(col, row) else {
                return Ok(None);
            };
            if let Some(&code) = seen.get(val) {
                codes.push(code);
            } else {
                if unique.len() >= max_entries {
                    return Ok(None);
                }
                let code =
                    u32::try_from(unique.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "dictionary entry_count exceeds u32::MAX",
                    })?;
                seen.insert(val, code);
                unique.push(val);
                codes.push(code);
            }
        }

        if !any_valid {
            return Ok(None);
        }

        let sorted = matches!(self.mode, DictBuildMode::L1Compact { .. });
        if sorted {
            let mut order: Vec<u32> = (0..unique.len() as u32).collect();
            order.sort_unstable_by(|&a, &b| unique[a as usize].cmp(unique[b as usize]));
            let mut remap = vec![0u32; unique.len()];
            let mut sorted_unique = Vec::with_capacity(unique.len());
            for (new_code, &old_idx) in order.iter().enumerate() {
                remap[old_idx as usize] = new_code as u32;
                sorted_unique.push(unique[old_idx as usize]);
            }
            for (row, code) in codes.iter_mut().enumerate() {
                if nulls.is_none_or(|bm| bm.get(row)) {
                    *code = remap[*code as usize];
                }
            }
            unique = sorted_unique;
        }

        let dict_flat = build_flat_from_slices(kind, &unique)?;
        let dict = FileDict::from_flat(dict_flat, sorted)?;
        Ok(Some((dict, codes)))
    }
}

fn flat_row_bytes(col: &FlatColumn, row: usize) -> Option<&[u8]> {
    match col.data() {
        FlatData::Utf8(u) => u.get(row).map(str::as_bytes),
        FlatData::Binary(b) => b.get(row),
        FlatData::FixedBytes8(v) => v.get(row).map(|a| a.as_slice()),
        FlatData::FixedBytes16(v) => v.get(row).map(|a| a.as_slice()),
        _ => None,
    }
}

fn build_flat_from_slices(
    kind: DictValueKind,
    slices: &[&[u8]],
) -> Result<FlatColumn, StatamError> {
    let data = match kind {
        DictValueKind::Utf8 => {
            let mut offsets = Vec::with_capacity(slices.len() + 1);
            offsets.push(0);
            let mut total = 0usize;
            for &s in slices {
                total = total
                    .checked_add(s.len())
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "dictionary utf8 byte size overflow",
                    })?;
            }
            let _ = u32::try_from(total).map_err(|_| StatamError::BoundsExceeded {
                reason: "dictionary utf8 byte size exceeds u32::MAX",
            })?;
            let mut bytes = Vec::with_capacity(total);
            for &s in slices {
                bytes.extend_from_slice(s);
                offsets.push(bytes.len() as u32);
            }
            FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?)
        }
        DictValueKind::Binary => {
            let mut offsets = Vec::with_capacity(slices.len() + 1);
            offsets.push(0);
            let mut total = 0usize;
            for &s in slices {
                total = total
                    .checked_add(s.len())
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "dictionary binary byte size overflow",
                    })?;
            }
            let _ = u32::try_from(total).map_err(|_| StatamError::BoundsExceeded {
                reason: "dictionary binary byte size exceeds u32::MAX",
            })?;
            let mut bytes = Vec::with_capacity(total);
            for &s in slices {
                bytes.extend_from_slice(s);
                offsets.push(bytes.len() as u32);
            }
            FlatData::Binary(BinaryData::try_new(offsets, bytes)?)
        }
        DictValueKind::Fixed8 => {
            let mut items = Vec::with_capacity(slices.len());
            for &s in slices {
                if s.len() != 8 {
                    return Err(StatamError::Corrupt {
                        reason: "dictionary Fixed8 entry length mismatch",
                    });
                }
                let mut arr = [0u8; 8];
                arr.copy_from_slice(s);
                items.push(arr);
            }
            FlatData::FixedBytes8(items)
        }
        DictValueKind::Fixed16 => {
            let mut items = Vec::with_capacity(slices.len());
            for &s in slices {
                if s.len() != 16 {
                    return Err(StatamError::Corrupt {
                        reason: "dictionary Fixed16 entry length mismatch",
                    });
                }
                let mut arr = [0u8; 16];
                arr.copy_from_slice(s);
                items.push(arr);
            }
            FlatData::FixedBytes16(items)
        }
    };
    Ok(FlatColumn::try_new(data, None)?)
}

/// Encodes `u32` dictionary codes (without leading row null bitmap) using `ForBitpack` or `Rle`.
pub fn encode_dict_codes_body(
    codes: &[u32],
    nulls: Option<&Bitmap>,
) -> Result<Vec<u8>, StatamError> {
    if codes.is_empty() {
        return Ok(Vec::new());
    }

    // Normalize null slots to first valid code (or 0) so RLE/ForBitpack stays compact.
    let first_valid = (0..codes.len())
        .find(|&i| nulls.is_none_or(|bm| bm.get(i)))
        .map_or(0, |i| codes[i]);
    let mut norm = Vec::with_capacity(codes.len());
    for (i, &c) in codes.iter().enumerate() {
        if nulls.is_some_and(|bm| !bm.get(i)) {
            norm.push(first_valid);
        } else {
            norm.push(c);
        }
    }

    // Option 0: ForBitpack.
    let min_code = norm.iter().copied().min().unwrap_or(0);
    let deltas: Vec<u32> = norm.iter().map(|&c| c - min_code).collect();
    let bw = min_bits_u32(&deltas);
    let for_packed_len = packed_bytes_len(norm.len(), bw)?;
    let for_total = 1 + 4 + 1 + for_packed_len;

    // Option 1: Rle over codes when runs are long enough.
    let mut run_ends: Vec<u32> = Vec::new();
    let mut run_vals: Vec<u32> = Vec::new();
    let mut cur = norm[0];
    for (i, &c) in norm.iter().enumerate().skip(1) {
        if c != cur {
            run_ends.push(i as u32);
            run_vals.push(cur);
            cur = c;
        }
    }
    run_ends.push(norm.len() as u32);
    run_vals.push(cur);

    if run_vals.len() >= 2 && run_vals.len() * 4 <= norm.len() {
        let re_bw = min_bits_u32(&run_ends);
        let rv_base = run_vals.iter().copied().min().unwrap_or(0);
        let rv_deltas: Vec<u32> = run_vals.iter().map(|&v| v - rv_base).collect();
        let rv_bw = min_bits_u32(&rv_deltas);
        let rle_total = 1
            + 4
            + 1
            + packed_bytes_len(run_ends.len(), re_bw)?
            + 4
            + 1
            + packed_bytes_len(rv_deltas.len(), rv_bw)?;

        if rle_total < for_total {
            let mut out = Vec::with_capacity(rle_total);
            out.push(1); // sub_enc = Rle
            out.extend_from_slice(&(run_ends.len() as u32).to_le_bytes());
            out.push(re_bw);
            pack_u32(&run_ends, re_bw, &mut out)?;
            out.extend_from_slice(&rv_base.to_le_bytes());
            out.push(rv_bw);
            pack_u32(&rv_deltas, rv_bw, &mut out)?;
            return Ok(out);
        }
    }

    let mut out = Vec::with_capacity(for_total);
    out.push(0); // sub_enc = ForBitpack
    out.extend_from_slice(&min_code.to_le_bytes());
    out.push(bw);
    pack_u32(&deltas, bw, &mut out)?;
    Ok(out)
}

/// Decodes `u32` dictionary codes (without leading row null bitmap) for `row_count` rows.
pub fn decode_dict_codes_body(
    src: &[u8],
    row_count: usize,
    dict_len: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<u32>, StatamError> {
    if row_count
        .checked_mul(size_of::<u32>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "dict decode row_count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    if row_count == 0 {
        if !src.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty dict block",
            });
        }
        return Ok(Vec::new());
    }
    let Some(&sub_enc) = src.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated dict sub_enc byte",
        });
    };

    let codes = match sub_enc {
        0 => {
            if src.len() < 6 {
                return Err(StatamError::Corrupt {
                    reason: "truncated dict ForBitpack header",
                });
            }
            let base = u32::from_le_bytes([src[1], src[2], src[3], src[4]]);
            let bw = src[5];
            let needed = packed_bytes_len(row_count, bw)?;
            if src.len() != 6 + needed {
                return Err(StatamError::Corrupt {
                    reason: "dict ForBitpack length mismatch",
                });
            }
            let mut deltas = Vec::with_capacity(row_count);
            unpack_u32(&src[6..], row_count, bw, &mut deltas)?;
            let mut out = Vec::with_capacity(row_count);
            for d in deltas {
                out.push(base.checked_add(d).ok_or(StatamError::Corrupt {
                    reason: "dict code overflow",
                })?);
            }
            out
        }
        1 => {
            if src.len() < 6 {
                return Err(StatamError::Corrupt {
                    reason: "truncated dict Rle header",
                });
            }
            let num_runs = u32::from_le_bytes([src[1], src[2], src[3], src[4]]) as usize;
            if num_runs == 0 || num_runs > row_count {
                return Err(StatamError::Corrupt {
                    reason: "invalid dict Rle num_runs",
                });
            }
            let re_bw = src[5];
            let re_bytes = packed_bytes_len(num_runs, re_bw)?;
            let pos_after_re = 6 + re_bytes;
            if src.len() < pos_after_re + 5 {
                return Err(StatamError::Corrupt {
                    reason: "truncated dict Rle values header",
                });
            }
            let mut run_ends = Vec::with_capacity(num_runs);
            unpack_u32(&src[6..pos_after_re], num_runs, re_bw, &mut run_ends)?;
            let mut prev = 0u32;
            for &end in &run_ends {
                if end <= prev || (end as usize) > row_count {
                    return Err(StatamError::Corrupt {
                        reason: "invalid dict Rle run_ends",
                    });
                }
                prev = end;
            }
            if prev as usize != row_count {
                return Err(StatamError::Corrupt {
                    reason: "dict Rle run_ends do not cover row_count",
                });
            }

            let rv_base = u32::from_le_bytes([
                src[pos_after_re],
                src[pos_after_re + 1],
                src[pos_after_re + 2],
                src[pos_after_re + 3],
            ]);
            let rv_bw = src[pos_after_re + 4];
            let rv_bytes = packed_bytes_len(num_runs, rv_bw)?;
            if src.len() != pos_after_re + 5 + rv_bytes {
                return Err(StatamError::Corrupt {
                    reason: "dict Rle length mismatch",
                });
            }
            let mut rv_deltas = Vec::with_capacity(num_runs);
            unpack_u32(&src[pos_after_re + 5..], num_runs, rv_bw, &mut rv_deltas)?;

            let mut out = Vec::with_capacity(row_count);
            let mut cur_start = 0usize;
            for (&end, &d) in run_ends.iter().zip(&rv_deltas) {
                let code = rv_base.checked_add(d).ok_or(StatamError::Corrupt {
                    reason: "dict Rle code overflow",
                })?;
                let end_usize = end as usize;
                out.resize(out.len() + (end_usize - cur_start), code);
                cur_start = end_usize;
            }
            out
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "unknown dict sub_enc",
            });
        }
    };

    for (row, &code) in codes.iter().enumerate() {
        if nulls.is_none_or(|bm| bm.get(row)) && (code as usize) >= dict_len {
            return Err(StatamError::Corrupt {
                reason: "dict code out of bounds",
            });
        }
    }

    Ok(codes)
}

/// Decodes a `Dict` mini-block body into either `Column::Dict` (zero-expansion pass-through
/// when `dict.memory_bytes() <= DICT_PASSTHROUGH_MAX_BYTES`) or a materialized `Column::Flat`.
pub fn decode_dict_column(
    src: &[u8],
    row_count: usize,
    nulls: Option<Bitmap>,
    dict: &FileDict,
) -> Result<Column, StatamError> {
    let mut codes = decode_dict_codes_body(src, row_count, dict.len(), nulls.as_ref())?;
    if let Some(bm) = &nulls {
        for (i, c) in codes.iter_mut().enumerate() {
            if !bm.get(i) {
                *c = 0;
            }
        }
    }

    if dict.memory_bytes() <= DICT_PASSTHROUGH_MAX_BYTES {
        let dc = DictColumn::try_new(dict.values().clone(), codes, nulls)?;
        return Ok(Column::Dict(dc));
    }

    // Large dictionary (> 256 KiB): materialize directly without cloning the whole dict.
    let flat = materialize_from_dict(dict.values(), &codes, nulls)?;
    Ok(Column::Flat(flat))
}

pub(crate) fn check_varlen_take_bytes(
    offsets: &[u32],
    codes: &[u32],
    nulls: Option<&Bitmap>,
) -> Result<(), StatamError> {
    let mut total_bytes: usize = 0;
    for (i, &c) in codes.iter().enumerate() {
        if nulls.is_none_or(|bm| bm.get(i)) {
            let idx = c as usize;
            if idx + 1 >= offsets.len() {
                return Err(StatamError::Corrupt {
                    reason: "dict code out of bounds",
                });
            }
            let item_len = (offsets[idx + 1] - offsets[idx]) as usize;
            total_bytes = total_bytes
                .checked_add(item_len)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "materialized dict byte length overflow",
                })?;
            if total_bytes > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "materialized dict bytes exceed MAX_BLOCK_RAW_BYTES",
                });
            }
        }
    }
    Ok(())
}

fn materialize_from_dict(
    dict_values: &FlatColumn,
    codes: &[u32],
    nulls: Option<Bitmap>,
) -> Result<FlatColumn, StatamError> {
    let data = match dict_values.data() {
        FlatData::Utf8(u) => {
            check_varlen_take_bytes(u.offsets(), codes, nulls.as_ref())?;
            FlatData::Utf8(u.take(codes, nulls.as_ref())?)
        }
        FlatData::Binary(b) => {
            check_varlen_take_bytes(b.offsets(), codes, nulls.as_ref())?;
            FlatData::Binary(b.take(codes, nulls.as_ref())?)
        }
        FlatData::FixedBytes8(v) => {
            if codes
                .len()
                .checked_mul(8)
                .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
            {
                return Err(StatamError::BoundsExceeded {
                    reason: "materialized Fixed8 dict exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let mut out = Vec::with_capacity(codes.len());
            for (i, &c) in codes.iter().enumerate() {
                if nulls.as_ref().is_some_and(|bm| !bm.get(i)) {
                    out.push([0u8; 8]);
                } else {
                    out.push(v[c as usize]);
                }
            }
            FlatData::FixedBytes8(out)
        }
        FlatData::FixedBytes16(v) => {
            if codes
                .len()
                .checked_mul(16)
                .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
            {
                return Err(StatamError::BoundsExceeded {
                    reason: "materialized Fixed16 dict exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let mut out = Vec::with_capacity(codes.len());
            for (i, &c) in codes.iter().enumerate() {
                if nulls.as_ref().is_some_and(|bm| !bm.get(i)) {
                    out.push([0u8; 16]);
                } else {
                    out.push(v[c as usize]);
                }
            }
            FlatData::FixedBytes16(out)
        }
        _ => {
            return Err(StatamError::SchemaMismatch {
                reason: "unsupported dictionary value data type",
            });
        }
    };
    Ok(FlatColumn::try_new(data, nulls)?)
}
