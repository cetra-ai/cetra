//! File-level dictionary builder, `DT01` wire codec, and `Dict` mini-block codec.
//!
//! Supports `L0Fast` insertion-order dictionaries (bounded `<= 4096` entries,
//! bailing out cleanly if exceeded) and `L1Compact` lexicographically sorted
//! dictionaries, plus zero-expansion `Column::Dict` pass-through up to 256 KiB.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::BuildHasherDefault;

#[derive(Default)]
pub(crate) struct FastSliceHasher(u64);

impl std::hash::Hasher for FastSliceHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0 ^ (bytes.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
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
    BinaryData, Bitmap, Column, DataType, DictColumn, FlatColumn, FlatData, ScalarValue, Utf8Data,
};

use crate::format::{Codec, DICT_MAGIC, compress_guarded, decompress_checked};
use crate::simd::bitpack::{min_bits_u32, pack_u32, packed_bytes_len, unpack_u32, unpack_u32_at};
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
    ///
    /// Enforces strict ascending order (`prev < cur`) when `sorted == true`, and
    /// entry uniqueness (via HashDoS-safe `HashSet`) when `sorted == false`.
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
        validate_dict_entries(&values, sorted)?;
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
        Self::from_flat(flat, sorted)
    }
}

fn validate_dict_entries(values: &FlatColumn, sorted: bool) -> Result<(), StatamError> {
    let n = values.len();
    if n <= 1 {
        return Ok(());
    }
    if sorted {
        let mut prev = flat_row_bytes(values, 0).ok_or(StatamError::Corrupt {
            reason: "dictionary entry missing",
        })?;
        for i in 1..n {
            let cur = flat_row_bytes(values, i).ok_or(StatamError::Corrupt {
                reason: "dictionary entry missing",
            })?;
            if prev >= cur {
                return Err(StatamError::Corrupt {
                    reason: "sorted dictionary entries are not strictly ascending",
                });
            }
            prev = cur;
        }
    } else {
        // Use std HashSet (SipHash-1-3) so untrusted unsorted dicts cannot trigger HashDoS,
        // and cap initial reservation so a duplicate-heavy dict fails before large allocations.
        let mut seen: HashSet<&[u8]> = HashSet::with_capacity(n.min(4_096));
        for i in 0..n {
            let cur = flat_row_bytes(values, i).ok_or(StatamError::Corrupt {
                reason: "dictionary entry missing",
            })?;
            if !seen.insert(cur) {
                return Err(StatamError::Corrupt {
                    reason: "dictionary contains duplicate entries",
                });
            }
        }
    }
    Ok(())
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

struct ParsedDictRle {
    run_ends: Vec<u32>,
    rv_base: u32,
    rv_deltas: Vec<u32>,
}

fn parse_dict_rle_body(src: &[u8], row_count: usize) -> Result<ParsedDictRle, StatamError> {
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
    if re_bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid u32 bit_width",
        });
    }
    let re_bytes = packed_bytes_len(num_runs, re_bw)?;
    let pos_after_re = 6 + re_bytes;
    if src.len() < pos_after_re + 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated dict Rle values header",
        });
    }
    let rv_base = u32::from_le_bytes([
        src[pos_after_re],
        src[pos_after_re + 1],
        src[pos_after_re + 2],
        src[pos_after_re + 3],
    ]);
    let rv_bw = src[pos_after_re + 4];
    if rv_bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid u32 bit_width",
        });
    }
    let rv_bytes = packed_bytes_len(num_runs, rv_bw)?;
    if src.len() != pos_after_re + 5 + rv_bytes {
        return Err(StatamError::Corrupt {
            reason: "dict Rle length mismatch",
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

    let mut rv_deltas = Vec::with_capacity(num_runs);
    unpack_u32(&src[pos_after_re + 5..], num_runs, rv_bw, &mut rv_deltas)?;

    Ok(ParsedDictRle {
        run_ends,
        rv_base,
        rv_deltas,
    })
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
            if bw > 32 {
                return Err(StatamError::Corrupt {
                    reason: "invalid u32 bit_width",
                });
            }
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
            let parsed = parse_dict_rle_body(src, row_count)?;
            let mut out = Vec::with_capacity(row_count);
            let mut cur_start = 0usize;
            for (&end, &d) in parsed.run_ends.iter().zip(&parsed.rv_deltas) {
                let code = parsed.rv_base.checked_add(d).ok_or(StatamError::Corrupt {
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

/// Computes an accurate logical materialized byte estimate for a dictionary column,
/// returning `Err(BoundsExceeded)` if it exceeds `limit`.
pub(crate) fn dict_logical_materialized_bytes(
    dict_values: &FlatColumn,
    codes: &[u32],
    nulls: Option<&Bitmap>,
    limit: usize,
) -> Result<usize, StatamError> {
    let nulls_bytes = if nulls.is_some() {
        codes.len().div_ceil(8)
    } else {
        0
    };
    let data_bytes = match dict_values.data() {
        FlatData::Utf8(u) => {
            let offsets_bytes = codes
                .len()
                .checked_add(1)
                .and_then(|n| n.checked_mul(4))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "materialized dict offsets overflow",
                })?;
            let payload = sum_varlen_take_bytes(u.offsets(), codes, nulls, limit)?;
            offsets_bytes
                .checked_add(payload)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "materialized dict byte length overflow",
                })?
        }
        FlatData::Binary(b) => {
            let offsets_bytes = codes
                .len()
                .checked_add(1)
                .and_then(|n| n.checked_mul(4))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "materialized dict offsets overflow",
                })?;
            let payload = sum_varlen_take_bytes(b.offsets(), codes, nulls, limit)?;
            offsets_bytes
                .checked_add(payload)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "materialized dict byte length overflow",
                })?
        }
        FlatData::FixedBytes8(_) => {
            codes
                .len()
                .checked_mul(8)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "materialized Fixed8 dict overflow",
                })?
        }
        FlatData::FixedBytes16(_) => {
            codes
                .len()
                .checked_mul(16)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "materialized Fixed16 dict overflow",
                })?
        }
        _ => {
            return Err(StatamError::SchemaMismatch {
                reason: "unsupported dictionary value data type",
            });
        }
    };
    let total = data_bytes
        .checked_add(nulls_bytes)
        .ok_or(StatamError::BoundsExceeded {
            reason: "materialized dict byte length overflow",
        })?;
    if total > limit {
        return Err(StatamError::BoundsExceeded {
            reason: "materialized dict bytes exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    Ok(total)
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
        let _ = dict_logical_materialized_bytes(
            dict.values(),
            &codes,
            nulls.as_ref(),
            MAX_BLOCK_RAW_BYTES,
        )?;
        let dc = DictColumn::try_new(dict.values().clone(), codes, nulls)?;
        return Ok(Column::Dict(dc));
    }

    // Large dictionary (> 256 KiB): materialize directly without cloning the whole dict.
    let flat = materialize_from_dict(dict.values(), &codes, nulls)?;
    Ok(Column::Flat(flat))
}

/// Sparsely decodes only `mask`-selected rows of a `Dict` mini-block body without
/// full-block `take` or cloning large dictionaries.
pub fn decode_dict_selected(
    src: &[u8],
    row_count: usize,
    nulls: Option<Bitmap>,
    dict: &FileDict,
    mask: &Bitmap,
) -> Result<Column, StatamError> {
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if nulls.as_ref().is_some_and(|bm| bm.len() != row_count) {
        return Err(StatamError::Corrupt {
            reason: "null bitmap length mismatch",
        });
    }
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
        return Ok(Column::Flat(FlatColumn::empty(&dict.values().data_type())));
    }
    if mask.all_ones() {
        return decode_dict_column(src, row_count, nulls, dict);
    }

    let Some(&sub_enc) = src.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated dict sub_enc byte",
        });
    };

    let selected_count = mask.count_ones();
    let dict_len = dict.len();
    let mut selected_pairs: Vec<(usize, u32)> = Vec::with_capacity(selected_count);

    match sub_enc {
        0 => {
            if src.len() < 6 {
                return Err(StatamError::Corrupt {
                    reason: "truncated dict ForBitpack header",
                });
            }
            let base = u32::from_le_bytes([src[1], src[2], src[3], src[4]]);
            let bw = src[5];
            if bw > 32 {
                return Err(StatamError::Corrupt {
                    reason: "invalid u32 bit_width",
                });
            }
            let needed = packed_bytes_len(row_count, bw)?;
            if src.len() != 6 + needed {
                return Err(StatamError::Corrupt {
                    reason: "dict ForBitpack length mismatch",
                });
            }
            if selected_count > 0 && selected_count.saturating_mul(16) < row_count {
                let packed = &src[6..];
                for row in mask.iter_ones() {
                    if row >= row_count {
                        break;
                    }
                    if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                        let d = unpack_u32_at(packed, row_count, bw, row)?;
                        let code = base.checked_add(d).ok_or(StatamError::Corrupt {
                            reason: "dict code overflow",
                        })?;
                        if (code as usize) >= dict_len {
                            return Err(StatamError::Corrupt {
                                reason: "dict code out of bounds",
                            });
                        }
                        selected_pairs.push((row, code));
                    }
                }
            } else {
                let mut deltas = Vec::with_capacity(row_count);
                unpack_u32(&src[6..], row_count, bw, &mut deltas)?;
                if selected_count > 0 {
                    for row in mask.iter_ones() {
                        if row >= row_count {
                            break;
                        }
                        if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                            let code =
                                base.checked_add(deltas[row]).ok_or(StatamError::Corrupt {
                                    reason: "dict code overflow",
                                })?;
                            if (code as usize) >= dict_len {
                                return Err(StatamError::Corrupt {
                                    reason: "dict code out of bounds",
                                });
                            }
                            selected_pairs.push((row, code));
                        }
                    }
                } else {
                    for (row, &d) in deltas.iter().enumerate() {
                        if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                            let code = base.checked_add(d).ok_or(StatamError::Corrupt {
                                reason: "dict code overflow",
                            })?;
                            if (code as usize) >= dict_len {
                                return Err(StatamError::Corrupt {
                                    reason: "dict code out of bounds",
                                });
                            }
                        }
                    }
                }
            }
        }
        1 => {
            let parsed = parse_dict_rle_body(src, row_count)?;
            if selected_count > 0 {
                let mut run_idx = 0usize;
                for row in mask.iter_ones() {
                    if row >= row_count {
                        break;
                    }
                    while (parsed.run_ends[run_idx] as usize) <= row {
                        run_idx += 1;
                    }
                    if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                        let code = parsed
                            .rv_base
                            .checked_add(parsed.rv_deltas[run_idx])
                            .ok_or(StatamError::Corrupt {
                                reason: "dict Rle code overflow",
                            })?;
                        if (code as usize) >= dict_len {
                            return Err(StatamError::Corrupt {
                                reason: "dict code out of bounds",
                            });
                        }
                        selected_pairs.push((row, code));
                    }
                }
            } else {
                let mut run_start = 0usize;
                for (&end, &d) in parsed.run_ends.iter().zip(&parsed.rv_deltas) {
                    let code = parsed.rv_base.checked_add(d).ok_or(StatamError::Corrupt {
                        reason: "dict Rle code overflow",
                    })?;
                    let run_end = end as usize;
                    if (code as usize) >= dict_len
                        && nulls
                            .as_ref()
                            .is_none_or(|bm| (run_start..run_end).any(|r| bm.get(r)))
                    {
                        return Err(StatamError::Corrupt {
                            reason: "dict code out of bounds",
                        });
                    }
                    run_start = run_end;
                }
            }
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "unknown dict sub_enc",
            });
        }
    }

    if selected_pairs.is_empty() {
        return Ok(Column::Const {
            value: ScalarValue::Null(dict.values().data_type()),
            len: row_count,
        });
    }

    if dict.memory_bytes() <= DICT_PASSTHROUGH_MAX_BYTES
        && selected_count.saturating_mul(16) >= row_count
    {
        let mut codes = vec![0u32; row_count];
        let mut out_nulls = Bitmap::new_zeroed(row_count);
        for &(row, code) in &selected_pairs {
            codes[row] = code;
            let _ = out_nulls.set(row, true);
        }
        let nulls_opt = if out_nulls.all_ones() {
            None
        } else {
            Some(out_nulls)
        };
        let _ = dict_logical_materialized_bytes(
            dict.values(),
            &codes,
            nulls_opt.as_ref(),
            MAX_BLOCK_RAW_BYTES,
        )?;
        let dc = DictColumn::try_new(dict.values().clone(), codes, nulls_opt)?;
        return Ok(Column::Dict(dc));
    }

    let flat = materialize_selected_from_dict(dict.values(), row_count, &selected_pairs)?;
    Ok(Column::Flat(flat))
}

/// Decodes only `indices` of a `Dict` mini-block body into a compact `indices.len()`-row
/// [`Column::Flat`] without allocating `row_count`-sized outputs or cloning the dictionary.
pub(crate) fn decode_dict_take(
    after_nulls: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
    dict: &FileDict,
    indices: &[usize],
) -> Result<Column, StatamError> {
    if nulls.is_some_and(|bm| bm.len() != row_count) {
        return Err(StatamError::Corrupt {
            reason: "null bitmap length mismatch",
        });
    }
    if row_count
        .checked_mul(size_of::<u32>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "dict decode row_count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    if indices
        .len()
        .checked_mul(size_of::<u32>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "dict take indices exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    for &row in indices {
        if row >= row_count {
            return Err(StatamError::Corrupt {
                reason: "dict take index out of bounds",
            });
        }
    }
    if row_count == 0 {
        if !after_nulls.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty dict block",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(&dict.values().data_type())));
    }

    let Some(&sub_enc) = after_nulls.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated dict sub_enc byte",
        });
    };

    let dict_len = dict.len();
    let mut codes = Vec::with_capacity(indices.len());
    let mut out_nulls = if nulls.is_some() {
        Some(Bitmap::new_ones(indices.len()))
    } else {
        None
    };
    let mut any_null = false;

    match sub_enc {
        0 => {
            if after_nulls.len() < 6 {
                return Err(StatamError::Corrupt {
                    reason: "truncated dict ForBitpack header",
                });
            }
            let base = u32::from_le_bytes([
                after_nulls[1],
                after_nulls[2],
                after_nulls[3],
                after_nulls[4],
            ]);
            let bw = after_nulls[5];
            if bw > 32 {
                return Err(StatamError::Corrupt {
                    reason: "invalid u32 bit_width",
                });
            }
            let needed = packed_bytes_len(row_count, bw)?;
            if after_nulls.len() != 6 + needed {
                return Err(StatamError::Corrupt {
                    reason: "dict ForBitpack length mismatch",
                });
            }
            let packed = &after_nulls[6..];
            if indices.len().saturating_mul(16) < row_count {
                let mut deltas = Vec::with_capacity(indices.len());
                crate::simd::bitpack::unpack_u32_indices(
                    packed,
                    row_count,
                    bw,
                    indices,
                    &mut deltas,
                )?;
                for (out_pos, (&row, d)) in indices.iter().zip(deltas).enumerate() {
                    if nulls.is_none_or(|bm| bm.get(row)) {
                        let code = base.checked_add(d).ok_or(StatamError::Corrupt {
                            reason: "dict code overflow",
                        })?;
                        if (code as usize) >= dict_len {
                            return Err(StatamError::Corrupt {
                                reason: "dict code out of bounds",
                            });
                        }
                        codes.push(code);
                    } else {
                        any_null = true;
                        if let Some(bm) = out_nulls.as_mut() {
                            let _ = bm.set(out_pos, false);
                        }
                        codes.push(0);
                    }
                }
            } else {
                let mut deltas = Vec::with_capacity(row_count);
                unpack_u32(packed, row_count, bw, &mut deltas)?;
                for (out_pos, &row) in indices.iter().enumerate() {
                    if nulls.is_none_or(|bm| bm.get(row)) {
                        let code = base.checked_add(deltas[row]).ok_or(StatamError::Corrupt {
                            reason: "dict code overflow",
                        })?;
                        if (code as usize) >= dict_len {
                            return Err(StatamError::Corrupt {
                                reason: "dict code out of bounds",
                            });
                        }
                        codes.push(code);
                    } else {
                        any_null = true;
                        if let Some(bm) = out_nulls.as_mut() {
                            let _ = bm.set(out_pos, false);
                        }
                        codes.push(0);
                    }
                }
            }
        }
        1 => {
            let parsed = parse_dict_rle_body(after_nulls, row_count)?;
            let mut run_idx = 0usize;
            let mut run_start = 0usize;
            for (out_pos, &row) in indices.iter().enumerate() {
                if row < run_start {
                    run_idx = parsed.run_ends.partition_point(|&e| (e as usize) <= row);
                } else {
                    while (parsed.run_ends[run_idx] as usize) <= row {
                        run_idx += 1;
                    }
                }
                run_start = if run_idx == 0 {
                    0
                } else {
                    parsed.run_ends[run_idx - 1] as usize
                };
                if nulls.is_none_or(|bm| bm.get(row)) {
                    let code = parsed
                        .rv_base
                        .checked_add(parsed.rv_deltas[run_idx])
                        .ok_or(StatamError::Corrupt {
                            reason: "dict Rle code overflow",
                        })?;
                    if (code as usize) >= dict_len {
                        return Err(StatamError::Corrupt {
                            reason: "dict code out of bounds",
                        });
                    }
                    codes.push(code);
                } else {
                    any_null = true;
                    if let Some(bm) = out_nulls.as_mut() {
                        let _ = bm.set(out_pos, false);
                    }
                    codes.push(0);
                }
            }
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "unknown dict sub_enc",
            });
        }
    }

    if indices.is_empty() {
        return Ok(Column::Flat(FlatColumn::empty(&dict.values().data_type())));
    }

    let final_nulls = if any_null { out_nulls } else { None };
    let flat = materialize_from_dict(dict.values(), &codes, final_nulls)?;
    Ok(Column::Flat(flat))
}

fn materialize_selected_from_dict(
    dict_values: &FlatColumn,
    row_count: usize,
    selected_pairs: &[(usize, u32)],
) -> Result<FlatColumn, StatamError> {
    let mut out_nulls = Bitmap::new_zeroed(row_count);
    let data = match dict_values.data() {
        FlatData::Utf8(u) => {
            let (offsets, bytes) = materialize_varlen_selected(
                u.offsets(),
                u.bytes(),
                row_count,
                selected_pairs,
                &mut out_nulls,
            )?;
            FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?)
        }
        FlatData::Binary(b) => {
            let (offsets, bytes) = materialize_varlen_selected(
                b.offsets(),
                b.bytes(),
                row_count,
                selected_pairs,
                &mut out_nulls,
            )?;
            FlatData::Binary(BinaryData::try_new(offsets, bytes)?)
        }
        FlatData::FixedBytes8(v) => {
            if row_count
                .checked_mul(8)
                .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
            {
                return Err(StatamError::BoundsExceeded {
                    reason: "materialized Fixed8 dict exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let mut out = vec![[0u8; 8]; row_count];
            for &(row, code) in selected_pairs {
                let entry = v.get(code as usize).ok_or(StatamError::Corrupt {
                    reason: "dict code out of bounds",
                })?;
                out[row] = *entry;
                let _ = out_nulls.set(row, true);
            }
            FlatData::FixedBytes8(out)
        }
        FlatData::FixedBytes16(v) => {
            if row_count
                .checked_mul(16)
                .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
            {
                return Err(StatamError::BoundsExceeded {
                    reason: "materialized Fixed16 dict exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let mut out = vec![[0u8; 16]; row_count];
            for &(row, code) in selected_pairs {
                let entry = v.get(code as usize).ok_or(StatamError::Corrupt {
                    reason: "dict code out of bounds",
                })?;
                out[row] = *entry;
                let _ = out_nulls.set(row, true);
            }
            FlatData::FixedBytes16(out)
        }
        _ => {
            return Err(StatamError::SchemaMismatch {
                reason: "unsupported dictionary value data type",
            });
        }
    };
    let nulls_opt = if out_nulls.all_ones() {
        None
    } else {
        Some(out_nulls)
    };
    Ok(FlatColumn::try_new(data, nulls_opt)?)
}

fn materialize_varlen_selected(
    dict_offsets: &[u32],
    dict_bytes: &[u8],
    row_count: usize,
    selected_pairs: &[(usize, u32)],
    out_nulls: &mut Bitmap,
) -> Result<(Vec<u32>, Vec<u8>), StatamError> {
    let mut total_bytes = 0usize;
    for &(_, code) in selected_pairs {
        let idx = code as usize;
        if idx + 1 >= dict_offsets.len() {
            return Err(StatamError::Corrupt {
                reason: "dict code out of bounds",
            });
        }
        let item_len = (dict_offsets[idx + 1] - dict_offsets[idx]) as usize;
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

    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut out_bytes = Vec::with_capacity(total_bytes);
    for &(row, code) in selected_pairs {
        let cur_off = out_bytes.len() as u32;
        if out_offsets.len() <= row {
            out_offsets.resize(row + 1, cur_off);
        }
        let idx = code as usize;
        let start = dict_offsets[idx] as usize;
        let end = dict_offsets[idx + 1] as usize;
        out_bytes.extend_from_slice(&dict_bytes[start..end]);
        let _ = out_nulls.set(row, true);
        out_offsets.push(out_bytes.len() as u32);
    }
    let tail_off = out_bytes.len() as u32;
    if out_offsets.len() < row_count + 1 {
        out_offsets.resize(row_count + 1, tail_off);
    }
    Ok((out_offsets, out_bytes))
}

pub(crate) fn sum_varlen_take_bytes(
    offsets: &[u32],
    codes: &[u32],
    nulls: Option<&Bitmap>,
    limit: usize,
) -> Result<usize, StatamError> {
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
            if total_bytes > limit {
                return Err(StatamError::BoundsExceeded {
                    reason: "materialized dict bytes exceed MAX_BLOCK_RAW_BYTES",
                });
            }
        }
    }
    Ok(total_bytes)
}

pub(crate) fn check_varlen_take_bytes(
    offsets: &[u32],
    codes: &[u32],
    nulls: Option<&Bitmap>,
) -> Result<(), StatamError> {
    let _ = sum_varlen_take_bytes(offsets, codes, nulls, MAX_BLOCK_RAW_BYTES)?;
    Ok(())
}

pub(crate) fn materialize_from_dict(
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
                    let entry = v.get(c as usize).ok_or(StatamError::Corrupt {
                        reason: "dict code out of bounds",
                    })?;
                    out.push(*entry);
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
                    let entry = v.get(c as usize).ok_or(StatamError::Corrupt {
                        reason: "dict code out of bounds",
                    })?;
                    out.push(*entry);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_dict_selected_parity(
        body: &[u8],
        row_count: usize,
        nulls: Option<&Bitmap>,
        dict: &FileDict,
    ) {
        let full = decode_dict_column(body, row_count, nulls.cloned(), dict)
            .unwrap()
            .materialize(None)
            .unwrap();

        let empty_mask = Bitmap::new_zeroed(row_count);
        let mut one_mask = Bitmap::new_zeroed(row_count);
        let _ = one_mask.set(row_count / 3, true);
        let mut scattered_mask = Bitmap::new_zeroed(row_count);
        for idx in [0usize, 1, 7, 19, 63, row_count - 1] {
            if idx < row_count {
                let _ = scattered_mask.set(idx, true);
            }
        }
        let all_mask = Bitmap::new_ones(row_count);

        for (name, mask) in [
            ("empty", &empty_mask),
            ("one_row", &one_mask),
            ("scattered", &scattered_mask),
            ("all_selected", &all_mask),
        ] {
            let sel = decode_dict_selected(body, row_count, nulls.cloned(), dict, mask)
                .unwrap()
                .materialize(None)
                .unwrap();
            assert_eq!(sel.len(), row_count, "{name}");
            for r in 0..row_count {
                if mask.get(r) {
                    assert_eq!(sel.is_null(r), full.is_null(r), "{name} row {r}");
                    if !full.is_null(r) {
                        assert_eq!(
                            sel.get_scalar(r).unwrap(),
                            full.get_scalar(r).unwrap(),
                            "{name} row {r}"
                        );
                    }
                } else {
                    assert!(sel.is_null(r), "{name} unselected row {r} must be null");
                }
            }
        }
    }

    #[test]
    fn decode_dict_selected_parity_small_large_forbitpack_rle_and_all_null() {
        let row_count = 128usize;
        let mut valid_bits = Vec::with_capacity(row_count);
        for i in 0..row_count {
            valid_bits.push(i % 6 != 0);
        }
        let nulls = Bitmap::from_bools(&valid_bits);
        let all_nulls = Bitmap::new_zeroed(row_count);

        // 1. Small Utf8 FileDict with ForBitpack (non-repeating codes) and Rle (repeating runs).
        let small_dict_vals: Vec<String> = (0..16).map(|k| format!("small-entry-{k:02}")).collect();
        let small_dict = FileDict::from_flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(small_dict_vals.iter().map(String::as_str)).unwrap(),
                ),
                None,
            )
            .unwrap(),
            true,
        )
        .unwrap();
        let non_rle_codes: Vec<u32> = (0..row_count).map(|i| (i % 16) as u32).collect();
        let body_fb = encode_dict_codes_body(&non_rle_codes, Some(&nulls)).unwrap();
        assert_eq!(body_fb[0], 0);
        assert_dict_selected_parity(&body_fb, row_count, Some(&nulls), &small_dict);
        assert_dict_selected_parity(&body_fb, row_count, None, &small_dict);
        assert_dict_selected_parity(&body_fb, row_count, Some(&all_nulls), &small_dict);

        let rle_codes: Vec<u32> = (0..row_count).map(|i| ((i / 32) % 16) as u32).collect();
        let body_rle = encode_dict_codes_body(&rle_codes, None).unwrap();
        assert_eq!(body_rle[0], 1);
        assert_dict_selected_parity(&body_rle, row_count, Some(&nulls), &small_dict);
        assert_dict_selected_parity(&body_rle, row_count, None, &small_dict);

        // 2. Large Utf8 FileDict (> 256 KiB passthrough threshold) exercising zero-take materialization.
        let big_pad = "x".repeat(4096);
        let big_dict_vals: Vec<String> = (0..80).map(|k| format!("big-{k:03}-{big_pad}")).collect();
        let big_dict = FileDict::from_flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(big_dict_vals.iter().map(String::as_str)).unwrap(),
                ),
                None,
            )
            .unwrap(),
            true,
        )
        .unwrap();
        assert!(big_dict.memory_bytes() > DICT_PASSTHROUGH_MAX_BYTES);
        let big_codes: Vec<u32> = (0..row_count).map(|i| (i % 80) as u32).collect();
        let big_body = encode_dict_codes_body(&big_codes, Some(&nulls)).unwrap();
        assert_dict_selected_parity(&big_body, row_count, Some(&nulls), &big_dict);

        // 3. Binary & FixedBytes16 FileDicts.
        let bin_vals: Vec<Vec<u8>> = (0..8)
            .map(|k| vec![k as u8, 0xFF, 0x00, (k * 3) as u8])
            .collect();
        let bin_dict = FileDict::from_flat(
            FlatColumn::try_new(
                FlatData::Binary(
                    BinaryData::try_from_slices(bin_vals.iter().map(Vec::as_slice)).unwrap(),
                ),
                None,
            )
            .unwrap(),
            false,
        )
        .unwrap();
        let bin_codes: Vec<u32> = (0..row_count).map(|i| (i % 8) as u32).collect();
        let bin_body = encode_dict_codes_body(&bin_codes, None).unwrap();
        assert_dict_selected_parity(&bin_body, row_count, None, &bin_dict);

        let f16_vals: Vec<[u8; 16]> = (0..8).map(|k| [k as u8; 16]).collect();
        let f16_dict = FileDict::from_flat(
            FlatColumn::try_new(FlatData::FixedBytes16(f16_vals), None).unwrap(),
            false,
        )
        .unwrap();
        assert_dict_selected_parity(&bin_body, row_count, Some(&nulls), &f16_dict);
    }

    #[test]
    fn decode_dict_selected_empty_mask_rejects_out_of_bounds_codes() {
        let dict = FileDict::from_flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["a", "b"]).unwrap()),
                None,
            )
            .unwrap(),
            true,
        )
        .unwrap();
        let row_count = 16usize;
        let empty_mask = Bitmap::new_zeroed(row_count);

        // 1. ForBitpack sub_enc with out-of-bounds code (99 >= 2).
        let fb_bad = encode_dict_codes_body(&vec![99u32; row_count], None).unwrap();
        assert!(matches!(
            decode_dict_selected(&fb_bad, row_count, None, &dict, &empty_mask),
            Err(StatamError::Corrupt {
                reason: "dict code out of bounds",
            })
        ));

        // 2. Rle sub_enc with out-of-bounds code in a run (50 >= 2).
        let mut rle_codes = vec![0u32; 8];
        rle_codes.extend_from_slice(&[50u32; 8]);
        let rle_bad = encode_dict_codes_body(&rle_codes, None).unwrap();
        assert_eq!(rle_bad[0], 1);
        assert!(matches!(
            decode_dict_selected(&rle_bad, row_count, None, &dict, &empty_mask),
            Err(StatamError::Corrupt {
                reason: "dict code out of bounds",
            })
        ));
    }

    #[test]
    fn file_dict_rejects_unsorted_and_duplicate_entries_in_from_flat_and_decode() {
        // 1. Unsorted when sorted == true -> Corrupt.
        let unsorted_flat = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["zebra", "apple"]).unwrap()),
            None,
        )
        .unwrap();
        assert!(matches!(
            FileDict::from_flat(unsorted_flat.clone(), true),
            Err(StatamError::Corrupt {
                reason: "sorted dictionary entries are not strictly ascending",
            })
        ));
        // Valid unsorted dict encoded to DT01 bytes, then decoded with sorted == true -> Corrupt.
        let unsorted_dict = FileDict::from_flat(unsorted_flat, false).unwrap();
        let encoded_unsorted = unsorted_dict.encode(Codec::None, 0).unwrap();
        assert!(matches!(
            FileDict::decode(&encoded_unsorted, true),
            Err(StatamError::Corrupt {
                reason: "sorted dictionary entries are not strictly ascending",
            })
        ));

        // 2. Duplicate entries rejected for both sorted == true and sorted == false.
        let dup_flat = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["a", "b", "b"]).unwrap()),
            None,
        )
        .unwrap();
        assert!(matches!(
            FileDict::from_flat(dup_flat.clone(), true),
            Err(StatamError::Corrupt {
                reason: "sorted dictionary entries are not strictly ascending",
            })
        ));
        assert!(matches!(
            FileDict::from_flat(dup_flat, false),
            Err(StatamError::Corrupt {
                reason: "dictionary contains duplicate entries",
            })
        ));

        // 3. Hand-crafted DT01 with duplicate entries rejected by FileDict::decode(..., false).
        let mut raw_body = Vec::new();
        for off in [0u32, 1, 2] {
            raw_body.extend_from_slice(&off.to_le_bytes());
        }
        raw_body.extend_from_slice(b"aa");
        let crc = crc32c::crc32c(&raw_body);
        let mut dt01 = Vec::new();
        dt01.extend_from_slice(&DICT_MAGIC);
        dt01.extend_from_slice(&2u32.to_le_bytes());
        dt01.push(Codec::None as u8);
        dt01.push(DictValueKind::Utf8 as u8);
        dt01.extend_from_slice(&0u16.to_le_bytes());
        dt01.extend_from_slice(&(raw_body.len() as u32).to_le_bytes());
        dt01.extend_from_slice(&crc.to_le_bytes());
        dt01.extend_from_slice(&raw_body);
        assert!(matches!(
            FileDict::decode(&dt01, false),
            Err(StatamError::Corrupt {
                reason: "dictionary contains duplicate entries",
            })
        ));

        // 4. Invalid bit_width (> 32) rejected early in ForBitpack and Rle dict code bodies.
        let bad_fb_bw = [0u8, 0, 0, 0, 0, 33];
        assert!(matches!(
            decode_dict_codes_body(&bad_fb_bw, 4, 2, None),
            Err(StatamError::Corrupt {
                reason: "invalid u32 bit_width",
            })
        ));
        let bad_rle_bw = [1u8, 1, 0, 0, 0, 33];
        assert!(matches!(
            decode_dict_codes_body(&bad_rle_bw, 4, 2, None),
            Err(StatamError::Corrupt {
                reason: "invalid u32 bit_width",
            })
        ));
    }
}
