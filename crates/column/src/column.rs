//! Columnar storage buffers and encodings (`Flat`, `Const`, `Dict`, `Rle`).

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use crate::{
    Bitmap, ColumnError, DataType, EventItem, Interval, LinkItem, MAX_NESTING_DEPTH, Reservation,
    ScalarValue,
};

pub(crate) enum FlatPiece<'a> {
    Borrowed(&'a FlatColumn),
    Owned(FlatColumn),
}

impl FlatPiece<'_> {
    pub(crate) fn as_flat(&self) -> &FlatColumn {
        match self {
            Self::Borrowed(f) => f,
            Self::Owned(f) => f,
        }
    }
}

fn checked_mul_bytes(a: usize, b: usize, reason: &'static str) -> Result<usize, ColumnError> {
    a.checked_mul(b).ok_or(ColumnError::Overflow { reason })
}

fn checked_add_bytes(a: usize, b: usize, reason: &'static str) -> Result<usize, ColumnError> {
    a.checked_add(b).ok_or(ColumnError::Overflow { reason })
}

fn fixed_take_bytes(
    row_count: usize,
    elem_bytes: usize,
    nulls_bytes: usize,
) -> Result<usize, ColumnError> {
    let data_bytes = checked_mul_bytes(row_count, elem_bytes, "take byte size overflow")?;
    checked_add_bytes(data_bytes, nulls_bytes, "take byte size overflow")
}

fn varlen_take_bytes(
    offsets: &[u32],
    nulls: Option<&Bitmap>,
    indices: &[u32],
    valid_mask: Option<&Bitmap>,
    nulls_bytes: usize,
    overflow_reason: &'static str,
    u32_reason: &'static str,
) -> Result<usize, ColumnError> {
    let mut total_bytes: usize = 0;
    for (out_pos, &idx) in indices.iter().enumerate() {
        let row = idx as usize;
        if valid_mask.is_none_or(|m| m.get(out_pos)) && nulls.is_none_or(|bm| bm.get(row)) {
            let item_len = (offsets[row + 1] - offsets[row]) as usize;
            total_bytes = checked_add_bytes(total_bytes, item_len, overflow_reason)?;
        }
    }
    let _ = checked_u32(total_bytes, u32_reason)?;
    let offsets_bytes = checked_mul_bytes(
        checked_add_bytes(indices.len(), 1, overflow_reason)?,
        4,
        overflow_reason,
    )?;
    let data_bytes = checked_add_bytes(offsets_bytes, total_bytes, overflow_reason)?;
    checked_add_bytes(data_bytes, nulls_bytes, overflow_reason)
}

/// Reconciles `pool` so its size becomes `before + target_bytes`, rolling back to `before` on error.
pub(crate) fn reconcile_reservation(
    pool: Option<&mut Reservation>,
    before: usize,
    target_bytes: usize,
) -> Result<(), ColumnError> {
    let Some(res) = pool else {
        return Ok(());
    };
    let Some(target) = before.checked_add(target_bytes) else {
        rollback_reservation(Some(res), before);
        return Err(ColumnError::Overflow {
            reason: "reservation byte total overflow",
        });
    };
    match target.cmp(&res.size()) {
        Ordering::Greater => {
            if let Err(err) = res.try_grow(target - res.size()) {
                rollback_reservation(Some(res), before);
                return Err(err);
            }
        }
        Ordering::Less => {
            res.shrink(res.size() - target);
        }
        Ordering::Equal => {}
    }
    Ok(())
}

/// Restores `pool` back to `before` on error.
pub(crate) fn rollback_reservation(pool: Option<&mut Reservation>, before: usize) {
    let Some(res) = pool else {
        return;
    };
    let cur = res.size();
    if cur > before {
        res.shrink(cur - before);
    }
}

fn validate_offsets(offsets: &[u32], expected_end: usize) -> Result<(), ColumnError> {
    if offsets.is_empty() {
        return Err(ColumnError::InvalidBatch {
            reason: "offsets cannot be empty",
        });
    }
    if offsets[0] != 0 {
        return Err(ColumnError::InvalidBatch {
            reason: "first offset must be 0",
        });
    }
    for w in offsets.windows(2) {
        if w[0] > w[1] {
            return Err(ColumnError::InvalidBatch {
                reason: "offsets must be monotonic",
            });
        }
    }
    let Some(&last) = offsets.last() else {
        return Err(ColumnError::InvalidBatch {
            reason: "offsets cannot be empty",
        });
    };
    if last as usize != expected_end {
        return Err(ColumnError::InvalidBatch {
            reason: "last offset does not match buffer length",
        });
    }
    Ok(())
}

fn normalize_nulls(
    nulls: Option<Bitmap>,
    expected_len: usize,
) -> Result<Option<Bitmap>, ColumnError> {
    match nulls {
        Some(bm) => {
            if bm.len() != expected_len {
                return Err(ColumnError::InvalidBatch {
                    reason: "nulls bitmap length mismatch",
                });
            }
            if bm.all_ones() {
                Ok(None)
            } else {
                Ok(Some(bm))
            }
        }
        None => Ok(None),
    }
}

fn slice_optional_bitmap(
    bm: Option<&Bitmap>,
    offset: usize,
    len: usize,
) -> Result<Option<Bitmap>, ColumnError> {
    match bm {
        Some(b) => {
            let s = b.slice(offset, len)?;
            if s.all_ones() { Ok(None) } else { Ok(Some(s)) }
        }
        None => Ok(None),
    }
}

fn checked_u32(val: usize, reason: &'static str) -> Result<u32, ColumnError> {
    u32::try_from(val).map_err(|_| ColumnError::Overflow { reason })
}

pub(crate) fn concat_bitmaps(
    parts: &[(usize, Option<&Bitmap>)],
    total_len: usize,
) -> Option<Bitmap> {
    let any_some = parts.iter().any(|(_, bm)| bm.is_some());
    if !any_some {
        return None;
    }
    let mut out = Bitmap::new_ones(total_len);
    let mut pos = 0usize;
    for &(len, bm) in parts {
        if let Some(b) = bm {
            for i in 0..len {
                if !b.get(i) {
                    let _ = out.set(pos + i, false);
                }
            }
        }
        pos += len;
    }
    if out.all_ones() { None } else { Some(out) }
}

/// Contiguous UTF-8 string buffer (`offsets.len() == len + 1`, `offsets[0] == 0`, monotonic,
/// `*offsets.last() as usize == bytes.len()`, valid UTF-8 and every offset is a char boundary).
/// `Debug` prints only row count and total byte length.
#[derive(Clone, PartialEq, Eq)]
pub struct Utf8Data {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl fmt::Debug for Utf8Data {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Utf8Data")
            .field("len", &self.len())
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

impl Utf8Data {
    /// Validates UTF-8, offset monotonicity, exact end byte length, and UTF-8 char boundaries on every offset.
    pub fn try_new(offsets: Vec<u32>, bytes: Vec<u8>) -> Result<Self, ColumnError> {
        validate_offsets(&offsets, bytes.len())?;
        if !bytes.is_ascii() {
            let s = std::str::from_utf8(&bytes).map_err(|_| ColumnError::InvalidUtf8)?;
            for &o in &offsets {
                if !s.is_char_boundary(o as usize) {
                    return Err(ColumnError::InvalidUtf8);
                }
            }
        }
        Ok(Self { offsets, bytes })
    }

    /// Empty string buffer (0 rows).
    pub fn empty() -> Self {
        Self {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }

    /// Builds from an iterator of string slices (fails with `Overflow` if total bytes `> u32::MAX`).
    pub fn try_from_slices<'a>(
        iter: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, ColumnError> {
        let mut offsets = vec![0u32];
        let mut bytes = Vec::new();
        for s in iter {
            let next_len = bytes
                .len()
                .checked_add(s.len())
                .ok_or(ColumnError::Overflow {
                    reason: "utf8 byte length overflow",
                })?;
            let next_u32 = checked_u32(next_len, "utf8 byte length exceeds u32::MAX")?;
            bytes.extend_from_slice(s.as_bytes());
            offsets.push(next_u32);
        }
        Ok(Self { offsets, bytes })
    }

    /// Number of rows (`offsets.len() - 1`).
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row offsets (`len + 1` elements).
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Contiguous UTF-8 bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// String slice at `row`, or `None` if out of bounds.
    pub fn get(&self, row: usize) -> Option<&str> {
        if row >= self.len() {
            return None;
        }
        let start = self.offsets[row] as usize;
        let end = self.offsets[row + 1] as usize;
        std::str::from_utf8(&self.bytes[start..end]).ok()
    }

    /// Slices `offset..offset + len`, rebasing offsets to start at 0.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "utf8 slice out of bounds",
            });
        }
        let base = self.offsets[offset];
        let byte_start = base as usize;
        let byte_end = self.offsets[offset + len] as usize;
        let offsets = self.offsets[offset..=offset + len]
            .iter()
            .map(|&o| o - base)
            .collect();
        let bytes = self.bytes[byte_start..byte_end].to_vec();
        Ok(Self { offsets, bytes })
    }

    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`,
    /// returning `InvalidBatch` on mismatch): when bit `out_pos == 0`, emits `""` without indexing `self`;
    /// otherwise requires `(indices[out_pos] as usize) < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError> {
        if let Some(mask) = null_mask
            && mask.len() != indices.len()
        {
            return Err(ColumnError::InvalidBatch {
                reason: "take null_mask length mismatch",
            });
        }
        let mut total_bytes: usize = 0;
        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_none_or(|m| m.get(out_pos)) {
                let row = idx as usize;
                if row >= self.len() {
                    return Err(ColumnError::InvalidBatch {
                        reason: "take index out of bounds",
                    });
                }
                let item_len = (self.offsets[row + 1] - self.offsets[row]) as usize;
                total_bytes = total_bytes
                    .checked_add(item_len)
                    .ok_or(ColumnError::Overflow {
                        reason: "utf8 take byte length overflow",
                    })?;
            }
        }
        let _ = checked_u32(total_bytes, "utf8 take byte length exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(indices.len() + 1);
        offsets.push(0);
        let mut bytes = Vec::with_capacity(total_bytes);
        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_some_and(|m| !m.get(out_pos)) {
                offsets.push(bytes.len() as u32);
            } else {
                let row = idx as usize;
                let start = self.offsets[row] as usize;
                let end = self.offsets[row + 1] as usize;
                bytes.extend_from_slice(&self.bytes[start..end]);
                offsets.push(bytes.len() as u32);
            }
        }
        Ok(Self { offsets, bytes })
    }

    /// Deterministic logical bytes (`offsets.len() * 4 + bytes.len()`).
    pub fn memory_bytes(&self) -> usize {
        self.offsets.len() * 4 + self.bytes.len()
    }

    pub(crate) fn concat(parts: &[&Self]) -> Result<Self, ColumnError> {
        let total_rows: usize = parts.iter().map(|p| p.len()).sum();
        let total_bytes = parts.iter().try_fold(0usize, |acc, p| {
            acc.checked_add(p.bytes.len()).ok_or(ColumnError::Overflow {
                reason: "utf8 concat byte length overflow",
            })
        })?;
        let _ = checked_u32(total_bytes, "utf8 concat byte length exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(total_rows + 1);
        offsets.push(0);
        let mut bytes = Vec::with_capacity(total_bytes);
        for part in parts {
            let base = bytes.len() as u32;
            bytes.extend_from_slice(&part.bytes);
            for &o in &part.offsets[1..] {
                offsets.push(base + o);
            }
        }
        Ok(Self { offsets, bytes })
    }
}

/// Contiguous binary buffer (`offsets.len() == len + 1`, `offsets[0] == 0`, monotonic,
/// `*offsets.last() as usize == bytes.len()`). `Debug` prints only row count and byte length.
#[derive(Clone, PartialEq, Eq)]
pub struct BinaryData {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl fmt::Debug for BinaryData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BinaryData")
            .field("len", &self.len())
            .field("bytes_len", &self.bytes.len())
            .finish()
    }
}

impl BinaryData {
    /// Validates offset monotonicity and exact end byte length.
    pub fn try_new(offsets: Vec<u32>, bytes: Vec<u8>) -> Result<Self, ColumnError> {
        validate_offsets(&offsets, bytes.len())?;
        Ok(Self { offsets, bytes })
    }

    /// Empty binary buffer (0 rows).
    pub fn empty() -> Self {
        Self {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }

    /// Builds from an iterator of byte slices.
    pub fn try_from_slices<'a>(
        iter: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<Self, ColumnError> {
        let mut offsets = vec![0u32];
        let mut bytes = Vec::new();
        for b in iter {
            let next_len = bytes
                .len()
                .checked_add(b.len())
                .ok_or(ColumnError::Overflow {
                    reason: "binary byte length overflow",
                })?;
            let next_u32 = checked_u32(next_len, "binary byte length exceeds u32::MAX")?;
            bytes.extend_from_slice(b);
            offsets.push(next_u32);
        }
        Ok(Self { offsets, bytes })
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row offsets (`len + 1` elements).
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Contiguous raw bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Byte slice at `row`, or `None` if out of bounds.
    pub fn get(&self, row: usize) -> Option<&[u8]> {
        if row >= self.len() {
            return None;
        }
        let start = self.offsets[row] as usize;
        let end = self.offsets[row + 1] as usize;
        Some(&self.bytes[start..end])
    }

    /// Slices `offset..offset + len`, rebasing offsets to 0.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "binary slice out of bounds",
            });
        }
        let base = self.offsets[offset];
        let byte_start = base as usize;
        let byte_end = self.offsets[offset + len] as usize;
        let offsets = self.offsets[offset..=offset + len]
            .iter()
            .map(|&o| o - base)
            .collect();
        let bytes = self.bytes[byte_start..byte_end].to_vec();
        Ok(Self { offsets, bytes })
    }

    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits empty bytes without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError> {
        if let Some(mask) = null_mask
            && mask.len() != indices.len()
        {
            return Err(ColumnError::InvalidBatch {
                reason: "take null_mask length mismatch",
            });
        }
        let mut total_bytes: usize = 0;
        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_none_or(|m| m.get(out_pos)) {
                let row = idx as usize;
                if row >= self.len() {
                    return Err(ColumnError::InvalidBatch {
                        reason: "take index out of bounds",
                    });
                }
                let item_len = (self.offsets[row + 1] - self.offsets[row]) as usize;
                total_bytes = total_bytes
                    .checked_add(item_len)
                    .ok_or(ColumnError::Overflow {
                        reason: "binary take byte length overflow",
                    })?;
            }
        }
        let _ = checked_u32(total_bytes, "binary take byte length exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(indices.len() + 1);
        offsets.push(0);
        let mut bytes = Vec::with_capacity(total_bytes);
        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_some_and(|m| !m.get(out_pos)) {
                offsets.push(bytes.len() as u32);
            } else {
                let row = idx as usize;
                let start = self.offsets[row] as usize;
                let end = self.offsets[row + 1] as usize;
                bytes.extend_from_slice(&self.bytes[start..end]);
                offsets.push(bytes.len() as u32);
            }
        }
        Ok(Self { offsets, bytes })
    }

    /// Deterministic logical bytes (`offsets.len() * 4 + bytes.len()`).
    pub fn memory_bytes(&self) -> usize {
        self.offsets.len() * 4 + self.bytes.len()
    }

    pub(crate) fn concat(parts: &[&Self]) -> Result<Self, ColumnError> {
        let total_rows: usize = parts.iter().map(|p| p.len()).sum();
        let total_bytes = parts.iter().try_fold(0usize, |acc, p| {
            acc.checked_add(p.bytes.len()).ok_or(ColumnError::Overflow {
                reason: "binary concat byte length overflow",
            })
        })?;
        let _ = checked_u32(total_bytes, "binary concat byte length exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(total_rows + 1);
        offsets.push(0);
        let mut bytes = Vec::with_capacity(total_bytes);
        for part in parts {
            let base = bytes.len() as u32;
            bytes.extend_from_slice(&part.bytes);
            for &o in &part.offsets[1..] {
                offsets.push(base + o);
            }
        }
        Ok(Self { offsets, bytes })
    }
}

/// Contiguous string-to-nullable-string map column (`offsets.len() == len + 1`,
/// `*offsets.last() as usize == keys.len() == values.len()`). `Debug` prints only counts.
#[derive(Clone, PartialEq, Eq)]
pub struct MapData {
    offsets: Vec<u32>,
    keys: Utf8Data,
    values: Utf8Data,
    value_nulls: Option<Bitmap>,
}

impl fmt::Debug for MapData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapData")
            .field("len", &self.len())
            .field("entry_count", &self.entry_count())
            .field("keys_bytes", &self.keys.bytes().len())
            .field("values_bytes", &self.values.bytes().len())
            .field("has_value_nulls", &self.value_nulls.is_some())
            .finish()
    }
}

impl MapData {
    /// Validates offsets, equal key/value entry counts, and optional `value_nulls` length (normalizing all-ones to `None`).
    pub fn try_new(
        offsets: Vec<u32>,
        keys: Utf8Data,
        values: Utf8Data,
        value_nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError> {
        if keys.len() != values.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "map keys and values length mismatch",
            });
        }
        validate_offsets(&offsets, keys.len())?;
        let value_nulls = normalize_nulls(value_nulls, keys.len())?;
        Ok(Self {
            offsets,
            keys,
            values,
            value_nulls,
        })
    }

    /// Empty map column (0 rows).
    pub fn empty() -> Self {
        Self {
            offsets: vec![0],
            keys: Utf8Data::empty(),
            values: Utf8Data::empty(),
            value_nulls: None,
        }
    }

    /// Number of map rows.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total entries across all rows.
    pub fn entry_count(&self) -> usize {
        self.keys.len()
    }

    /// Row offsets into `keys` and `values`.
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Non-null map keys.
    pub fn keys(&self) -> &Utf8Data {
        &self.keys
    }

    /// Map values (empty string at null positions).
    pub fn values(&self) -> &Utf8Data {
        &self.values
    }

    /// Validity bitmap for `values` (`1` = valid, `0` = null).
    pub fn value_nulls(&self) -> Option<&Bitmap> {
        self.value_nulls.as_ref()
    }

    /// Entries `(key, Option<value>)` for `row`.
    pub fn get(&self, row: usize) -> Option<Vec<(String, Option<String>)>> {
        if row >= self.len() {
            return None;
        }
        let start = self.offsets[row] as usize;
        let end = self.offsets[row + 1] as usize;
        let mut out = Vec::with_capacity(end - start);
        for idx in start..end {
            let k = self.keys.get(idx)?.to_owned();
            let v = if self.value_nulls.as_ref().is_some_and(|bm| !bm.get(idx)) {
                None
            } else {
                Some(self.values.get(idx)?.to_owned())
            };
            out.push((k, v));
        }
        Some(out)
    }

    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "map slice out of bounds",
            });
        }
        let base = self.offsets[offset];
        let entry_start = base as usize;
        let entry_end = self.offsets[offset + len] as usize;
        let entry_len = entry_end - entry_start;
        let offsets = self.offsets[offset..=offset + len]
            .iter()
            .map(|&o| o - base)
            .collect();
        let keys = self.keys.slice(entry_start, entry_len)?;
        let values = self.values.slice(entry_start, entry_len)?;
        let value_nulls = slice_optional_bitmap(self.value_nulls.as_ref(), entry_start, entry_len)?;
        Ok(Self {
            offsets,
            keys,
            values,
            value_nulls,
        })
    }

    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits an empty map without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError> {
        if let Some(mask) = null_mask
            && mask.len() != indices.len()
        {
            return Err(ColumnError::InvalidBatch {
                reason: "take null_mask length mismatch",
            });
        }
        let mut total_entries: usize = 0;
        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_none_or(|m| m.get(out_pos)) {
                let row = idx as usize;
                if row >= self.len() {
                    return Err(ColumnError::InvalidBatch {
                        reason: "take index out of bounds",
                    });
                }
                let count = (self.offsets[row + 1] - self.offsets[row]) as usize;
                total_entries = total_entries
                    .checked_add(count)
                    .ok_or(ColumnError::Overflow {
                        reason: "map take entry count overflow",
                    })?;
            }
        }
        let _ = checked_u32(total_entries, "map take entry count exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(indices.len() + 1);
        offsets.push(0);
        let mut entry_indices = Vec::with_capacity(total_entries);
        let mut val_nulls = self
            .value_nulls
            .as_ref()
            .map(|_| Bitmap::new_ones(total_entries));
        let mut out_entry = 0usize;

        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_some_and(|m| !m.get(out_pos)) {
                offsets.push(entry_indices.len() as u32);
            } else {
                let row = idx as usize;
                let start = self.offsets[row] as usize;
                let end = self.offsets[row + 1] as usize;
                for src in start..end {
                    entry_indices.push(src as u32);
                    if let (Some(src_bm), Some(dst_bm)) = (&self.value_nulls, &mut val_nulls)
                        && !src_bm.get(src)
                    {
                        let _ = dst_bm.set(out_entry, false);
                    }
                    out_entry += 1;
                }
                offsets.push(entry_indices.len() as u32);
            }
        }

        let value_nulls = val_nulls.filter(|bm| !bm.all_ones());
        let keys = self.keys.take(&entry_indices, None)?;
        let values = self.values.take(&entry_indices, value_nulls.as_ref())?;
        Ok(Self {
            offsets,
            keys,
            values,
            value_nulls,
        })
    }

    /// Deterministic logical bytes.
    pub fn memory_bytes(&self) -> usize {
        self.offsets.len() * 4
            + self.keys.memory_bytes()
            + self.values.memory_bytes()
            + self.value_nulls.as_ref().map_or(0, Bitmap::memory_bytes)
    }

    pub(crate) fn concat(parts: &[&Self]) -> Result<Self, ColumnError> {
        let total_rows: usize = parts.iter().map(|p| p.len()).sum();
        let total_entries = parts.iter().try_fold(0usize, |acc, p| {
            acc.checked_add(p.entry_count())
                .ok_or(ColumnError::Overflow {
                    reason: "map concat entry count overflow",
                })
        })?;
        let _ = checked_u32(total_entries, "map concat entry count exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(total_rows + 1);
        offsets.push(0);
        let mut running = 0u32;
        for part in parts {
            for &o in &part.offsets[1..] {
                offsets.push(running + o);
            }
            running += part.entry_count() as u32;
        }
        let keys_parts: Vec<&Utf8Data> = parts.iter().map(|p| &p.keys).collect();
        let val_parts: Vec<&Utf8Data> = parts.iter().map(|p| &p.values).collect();
        let null_parts: Vec<(usize, Option<&Bitmap>)> = parts
            .iter()
            .map(|p| (p.entry_count(), p.value_nulls.as_ref()))
            .collect();
        let keys = Utf8Data::concat(&keys_parts)?;
        let values = Utf8Data::concat(&val_parts)?;
        let value_nulls = concat_bitmaps(&null_parts, total_entries);
        Ok(Self {
            offsets,
            keys,
            values,
            value_nulls,
        })
    }
}

/// Typed `spans.events` column (`offsets.len() == len + 1`,
/// `*offsets.last() as usize == times.len() == names.len() == attributes.len()`).
#[derive(Clone, PartialEq, Eq)]
pub struct EventsData {
    offsets: Vec<u32>,
    times: Vec<i64>,
    names: Utf8Data,
    attributes: Utf8Data,
    attribute_nulls: Option<Bitmap>,
}

impl fmt::Debug for EventsData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventsData")
            .field("len", &self.len())
            .field("event_count", &self.event_count())
            .field("names_bytes", &self.names.bytes().len())
            .field("attributes_bytes", &self.attributes.bytes().len())
            .field("has_attribute_nulls", &self.attribute_nulls.is_some())
            .finish()
    }
}

impl EventsData {
    /// Validates offsets, equal child lengths, and optional `attribute_nulls` (normalizing all-ones to `None`).
    pub fn try_new(
        offsets: Vec<u32>,
        times: Vec<i64>,
        names: Utf8Data,
        attributes: Utf8Data,
        attribute_nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError> {
        if times.len() != names.len() || names.len() != attributes.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "events child buffer length mismatch",
            });
        }
        validate_offsets(&offsets, times.len())?;
        let attribute_nulls = normalize_nulls(attribute_nulls, times.len())?;
        Ok(Self {
            offsets,
            times,
            names,
            attributes,
            attribute_nulls,
        })
    }

    /// Empty events column (0 rows).
    pub fn empty() -> Self {
        Self {
            offsets: vec![0],
            times: Vec::new(),
            names: Utf8Data::empty(),
            attributes: Utf8Data::empty(),
            attribute_nulls: None,
        }
    }

    /// Number of span rows.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total event items across all rows.
    pub fn event_count(&self) -> usize {
        self.times.len()
    }

    /// Row offsets into child event arrays.
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Event timestamps (UTC ns).
    pub fn times(&self) -> &[i64] {
        &self.times
    }

    /// Event names.
    pub fn names(&self) -> &Utf8Data {
        &self.names
    }

    /// Event attributes JSON strings.
    pub fn attributes(&self) -> &Utf8Data {
        &self.attributes
    }

    /// Validity bitmap for `attributes`.
    pub fn attribute_nulls(&self) -> Option<&Bitmap> {
        self.attribute_nulls.as_ref()
    }

    /// Event items for `row`.
    pub fn get(&self, row: usize) -> Option<Vec<EventItem>> {
        if row >= self.len() {
            return None;
        }
        let start = self.offsets[row] as usize;
        let end = self.offsets[row + 1] as usize;
        let mut out = Vec::with_capacity(end - start);
        for idx in start..end {
            let name = self.names.get(idx)?.to_owned();
            let attributes = if self.attribute_nulls.as_ref().is_some_and(|bm| !bm.get(idx)) {
                None
            } else {
                Some(self.attributes.get(idx)?.to_owned())
            };
            out.push(EventItem {
                time: self.times[idx],
                name,
                attributes,
            });
        }
        Some(out)
    }

    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "events slice out of bounds",
            });
        }
        let base = self.offsets[offset];
        let start = base as usize;
        let end = self.offsets[offset + len] as usize;
        let count = end - start;
        let offsets = self.offsets[offset..=offset + len]
            .iter()
            .map(|&o| o - base)
            .collect();
        let times = self.times[start..end].to_vec();
        let names = self.names.slice(start, count)?;
        let attributes = self.attributes.slice(start, count)?;
        let attribute_nulls = slice_optional_bitmap(self.attribute_nulls.as_ref(), start, count)?;
        Ok(Self {
            offsets,
            times,
            names,
            attributes,
            attribute_nulls,
        })
    }

    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits an empty event list without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError> {
        if let Some(mask) = null_mask
            && mask.len() != indices.len()
        {
            return Err(ColumnError::InvalidBatch {
                reason: "take null_mask length mismatch",
            });
        }
        let mut total_events: usize = 0;
        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_none_or(|m| m.get(out_pos)) {
                let row = idx as usize;
                if row >= self.len() {
                    return Err(ColumnError::InvalidBatch {
                        reason: "take index out of bounds",
                    });
                }
                let count = (self.offsets[row + 1] - self.offsets[row]) as usize;
                total_events = total_events
                    .checked_add(count)
                    .ok_or(ColumnError::Overflow {
                        reason: "events take count overflow",
                    })?;
            }
        }
        let _ = checked_u32(total_events, "events take count exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(indices.len() + 1);
        offsets.push(0);
        let mut item_indices = Vec::with_capacity(total_events);
        let mut times = Vec::with_capacity(total_events);
        let mut attr_nulls = self
            .attribute_nulls
            .as_ref()
            .map(|_| Bitmap::new_ones(total_events));
        let mut out_ev = 0usize;

        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_some_and(|m| !m.get(out_pos)) {
                offsets.push(item_indices.len() as u32);
            } else {
                let row = idx as usize;
                let start = self.offsets[row] as usize;
                let end = self.offsets[row + 1] as usize;
                for src in start..end {
                    item_indices.push(src as u32);
                    times.push(self.times[src]);
                    if let (Some(src_bm), Some(dst_bm)) = (&self.attribute_nulls, &mut attr_nulls)
                        && !src_bm.get(src)
                    {
                        let _ = dst_bm.set(out_ev, false);
                    }
                    out_ev += 1;
                }
                offsets.push(item_indices.len() as u32);
            }
        }

        let attribute_nulls = attr_nulls.filter(|bm| !bm.all_ones());
        let names = self.names.take(&item_indices, None)?;
        let attributes = self
            .attributes
            .take(&item_indices, attribute_nulls.as_ref())?;
        Ok(Self {
            offsets,
            times,
            names,
            attributes,
            attribute_nulls,
        })
    }

    /// Deterministic logical bytes.
    pub fn memory_bytes(&self) -> usize {
        self.offsets.len() * 4
            + self.times.len() * 8
            + self.names.memory_bytes()
            + self.attributes.memory_bytes()
            + self
                .attribute_nulls
                .as_ref()
                .map_or(0, Bitmap::memory_bytes)
    }

    pub(crate) fn concat(parts: &[&Self]) -> Result<Self, ColumnError> {
        let total_rows: usize = parts.iter().map(|p| p.len()).sum();
        let total_events = parts.iter().try_fold(0usize, |acc, p| {
            acc.checked_add(p.event_count())
                .ok_or(ColumnError::Overflow {
                    reason: "events concat count overflow",
                })
        })?;
        let _ = checked_u32(total_events, "events concat count exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(total_rows + 1);
        offsets.push(0);
        let mut times = Vec::with_capacity(total_events);
        let mut running = 0u32;
        for part in parts {
            for &o in &part.offsets[1..] {
                offsets.push(running + o);
            }
            running += part.event_count() as u32;
            times.extend_from_slice(&part.times);
        }
        let name_parts: Vec<&Utf8Data> = parts.iter().map(|p| &p.names).collect();
        let attr_parts: Vec<&Utf8Data> = parts.iter().map(|p| &p.attributes).collect();
        let null_parts: Vec<(usize, Option<&Bitmap>)> = parts
            .iter()
            .map(|p| (p.event_count(), p.attribute_nulls.as_ref()))
            .collect();
        let names = Utf8Data::concat(&name_parts)?;
        let attributes = Utf8Data::concat(&attr_parts)?;
        let attribute_nulls = concat_bitmaps(&null_parts, total_events);
        Ok(Self {
            offsets,
            times,
            names,
            attributes,
            attribute_nulls,
        })
    }
}

/// Typed `spans.links` column (`offsets.len() == len + 1`,
/// `*offsets.last() as usize == trace_ids.len() == span_ids.len() == attributes.len()`).
#[derive(Clone, PartialEq, Eq)]
pub struct LinksData {
    offsets: Vec<u32>,
    trace_ids: Vec<[u8; 16]>,
    span_ids: Vec<[u8; 8]>,
    attributes: Utf8Data,
    attribute_nulls: Option<Bitmap>,
}

impl fmt::Debug for LinksData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinksData")
            .field("len", &self.len())
            .field("link_count", &self.link_count())
            .field("attributes_bytes", &self.attributes.bytes().len())
            .field("has_attribute_nulls", &self.attribute_nulls.is_some())
            .finish()
    }
}

impl LinksData {
    /// Validates offsets, equal child lengths, and optional `attribute_nulls` (normalizing all-ones to `None`).
    pub fn try_new(
        offsets: Vec<u32>,
        trace_ids: Vec<[u8; 16]>,
        span_ids: Vec<[u8; 8]>,
        attributes: Utf8Data,
        attribute_nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError> {
        if trace_ids.len() != span_ids.len() || span_ids.len() != attributes.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "links child buffer length mismatch",
            });
        }
        validate_offsets(&offsets, trace_ids.len())?;
        let attribute_nulls = normalize_nulls(attribute_nulls, trace_ids.len())?;
        Ok(Self {
            offsets,
            trace_ids,
            span_ids,
            attributes,
            attribute_nulls,
        })
    }

    /// Empty links column (0 rows).
    pub fn empty() -> Self {
        Self {
            offsets: vec![0],
            trace_ids: Vec::new(),
            span_ids: Vec::new(),
            attributes: Utf8Data::empty(),
            attribute_nulls: None,
        }
    }

    /// Number of span rows.
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total link items across all rows.
    pub fn link_count(&self) -> usize {
        self.trace_ids.len()
    }

    /// Row offsets into child link arrays.
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Linked 16-byte trace IDs.
    pub fn trace_ids(&self) -> &[[u8; 16]] {
        &self.trace_ids
    }

    /// Linked 8-byte span IDs.
    pub fn span_ids(&self) -> &[[u8; 8]] {
        &self.span_ids
    }

    /// Link attributes JSON strings.
    pub fn attributes(&self) -> &Utf8Data {
        &self.attributes
    }

    /// Validity bitmap for `attributes`.
    pub fn attribute_nulls(&self) -> Option<&Bitmap> {
        self.attribute_nulls.as_ref()
    }

    /// Link items for `row`.
    pub fn get(&self, row: usize) -> Option<Vec<LinkItem>> {
        if row >= self.len() {
            return None;
        }
        let start = self.offsets[row] as usize;
        let end = self.offsets[row + 1] as usize;
        let mut out = Vec::with_capacity(end - start);
        for idx in start..end {
            let attributes = if self.attribute_nulls.as_ref().is_some_and(|bm| !bm.get(idx)) {
                None
            } else {
                Some(self.attributes.get(idx)?.to_owned())
            };
            out.push(LinkItem {
                trace_id: self.trace_ids[idx],
                span_id: self.span_ids[idx],
                attributes,
            });
        }
        Some(out)
    }

    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "links slice out of bounds",
            });
        }
        let base = self.offsets[offset];
        let start = base as usize;
        let end = self.offsets[offset + len] as usize;
        let count = end - start;
        let offsets = self.offsets[offset..=offset + len]
            .iter()
            .map(|&o| o - base)
            .collect();
        let trace_ids = self.trace_ids[start..end].to_vec();
        let span_ids = self.span_ids[start..end].to_vec();
        let attributes = self.attributes.slice(start, count)?;
        let attribute_nulls = slice_optional_bitmap(self.attribute_nulls.as_ref(), start, count)?;
        Ok(Self {
            offsets,
            trace_ids,
            span_ids,
            attributes,
            attribute_nulls,
        })
    }

    /// Gathers rows by `indices`. `null_mask`, if `Some`, is **output-indexed** (`null_mask.len() == indices.len()`):
    /// when bit `out_pos == 0`, emits an empty link list without indexing `self`; otherwise requires `indices[out_pos] < self.len()`.
    pub fn take(&self, indices: &[u32], null_mask: Option<&Bitmap>) -> Result<Self, ColumnError> {
        if let Some(mask) = null_mask
            && mask.len() != indices.len()
        {
            return Err(ColumnError::InvalidBatch {
                reason: "take null_mask length mismatch",
            });
        }
        let mut total_links: usize = 0;
        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_none_or(|m| m.get(out_pos)) {
                let row = idx as usize;
                if row >= self.len() {
                    return Err(ColumnError::InvalidBatch {
                        reason: "take index out of bounds",
                    });
                }
                let count = (self.offsets[row + 1] - self.offsets[row]) as usize;
                total_links = total_links
                    .checked_add(count)
                    .ok_or(ColumnError::Overflow {
                        reason: "links take count overflow",
                    })?;
            }
        }
        let _ = checked_u32(total_links, "links take count exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(indices.len() + 1);
        offsets.push(0);
        let mut item_indices = Vec::with_capacity(total_links);
        let mut trace_ids = Vec::with_capacity(total_links);
        let mut span_ids = Vec::with_capacity(total_links);
        let mut attr_nulls = self
            .attribute_nulls
            .as_ref()
            .map(|_| Bitmap::new_ones(total_links));
        let mut out_lk = 0usize;

        for (out_pos, &idx) in indices.iter().enumerate() {
            if null_mask.is_some_and(|m| !m.get(out_pos)) {
                offsets.push(item_indices.len() as u32);
            } else {
                let row = idx as usize;
                let start = self.offsets[row] as usize;
                let end = self.offsets[row + 1] as usize;
                for src in start..end {
                    item_indices.push(src as u32);
                    trace_ids.push(self.trace_ids[src]);
                    span_ids.push(self.span_ids[src]);
                    if let (Some(src_bm), Some(dst_bm)) = (&self.attribute_nulls, &mut attr_nulls)
                        && !src_bm.get(src)
                    {
                        let _ = dst_bm.set(out_lk, false);
                    }
                    out_lk += 1;
                }
                offsets.push(item_indices.len() as u32);
            }
        }

        let attribute_nulls = attr_nulls.filter(|bm| !bm.all_ones());
        let attributes = self
            .attributes
            .take(&item_indices, attribute_nulls.as_ref())?;
        Ok(Self {
            offsets,
            trace_ids,
            span_ids,
            attributes,
            attribute_nulls,
        })
    }

    /// Deterministic logical bytes.
    pub fn memory_bytes(&self) -> usize {
        self.offsets.len() * 4
            + self.trace_ids.len() * 16
            + self.span_ids.len() * 8
            + self.attributes.memory_bytes()
            + self
                .attribute_nulls
                .as_ref()
                .map_or(0, Bitmap::memory_bytes)
    }

    pub(crate) fn concat(parts: &[&Self]) -> Result<Self, ColumnError> {
        let total_rows: usize = parts.iter().map(|p| p.len()).sum();
        let total_links = parts.iter().try_fold(0usize, |acc, p| {
            acc.checked_add(p.link_count())
                .ok_or(ColumnError::Overflow {
                    reason: "links concat count overflow",
                })
        })?;
        let _ = checked_u32(total_links, "links concat count exceeds u32::MAX")?;

        let mut offsets = Vec::with_capacity(total_rows + 1);
        offsets.push(0);
        let mut trace_ids = Vec::with_capacity(total_links);
        let mut span_ids = Vec::with_capacity(total_links);
        let mut running = 0u32;
        for part in parts {
            for &o in &part.offsets[1..] {
                offsets.push(running + o);
            }
            running += part.link_count() as u32;
            trace_ids.extend_from_slice(&part.trace_ids);
            span_ids.extend_from_slice(&part.span_ids);
        }
        let attr_parts: Vec<&Utf8Data> = parts.iter().map(|p| &p.attributes).collect();
        let null_parts: Vec<(usize, Option<&Bitmap>)> = parts
            .iter()
            .map(|p| (p.link_count(), p.attribute_nulls.as_ref()))
            .collect();
        let attributes = Utf8Data::concat(&attr_parts)?;
        let attribute_nulls = concat_bitmaps(&null_parts, total_links);
        Ok(Self {
            offsets,
            trace_ids,
            span_ids,
            attributes,
            attribute_nulls,
        })
    }
}

/// Typed flat values buffer (one variant per [`DataType`]).
/// Large nested buffers are boxed so `size_of::<FlatData>()` stays well below `clippy::large_enum_variant`.
#[derive(Clone, PartialEq)]
pub enum FlatData {
    /// All-null column of `len` rows.
    Null(usize),
    /// Packed boolean values.
    Bool(Bitmap),
    /// 64-bit integers.
    I64(Vec<i64>),
    /// 64-bit floats.
    F64(Vec<f64>),
    /// Fixed-precision 128-bit decimals.
    Decimal128 {
        /// Unscaled 128-bit values.
        values: Vec<i128>,
        /// Precision (`1..=38`).
        precision: u8,
        /// Scale.
        scale: i8,
    },
    /// Nanosecond timestamps.
    TimestampNs {
        /// Nanoseconds since epoch.
        values: Vec<i64>,
        /// True when UTC-zoned.
        utc: bool,
    },
    /// Days since Unix epoch.
    Date(Vec<i32>),
    /// Calendar intervals.
    Interval(Vec<Interval>),
    /// UTF-8 strings.
    Utf8(Utf8Data),
    /// Variable-width binary.
    Binary(BinaryData),
    /// 8-byte fixed binary (`SpanId`).
    FixedBytes8(Vec<[u8; 8]>),
    /// 16-byte fixed binary (`TraceId`).
    FixedBytes16(Vec<[u8; 16]>),
    /// Homogeneous list (`offsets.len() == len + 1`, `values.len() == *offsets.last() as usize`).
    List {
        /// Element type.
        item_type: DataType,
        /// Row boundaries into `values`.
        offsets: Vec<u32>,
        /// Child values column (may itself be `Flat`, `Const`, `Dict` or `Rle`).
        values: Box<Column>,
    },
    /// String-to-nullable-string maps.
    MapUtf8(Box<MapData>),
    /// Span events.
    Events(Box<EventsData>),
    /// Span links.
    Links(Box<LinksData>),
}

impl fmt::Debug for FlatData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null(len) => f.debug_struct("Null").field("len", len).finish(),
            Self::Bool(bm) => f.debug_struct("Bool").field("len", &bm.len()).finish(),
            Self::I64(v) => f.debug_struct("I64").field("len", &v.len()).finish(),
            Self::F64(v) => f.debug_struct("F64").field("len", &v.len()).finish(),
            Self::Decimal128 {
                values,
                precision,
                scale,
            } => f
                .debug_struct("Decimal128")
                .field("len", &values.len())
                .field("precision", precision)
                .field("scale", scale)
                .finish(),
            Self::TimestampNs { values, utc } => f
                .debug_struct("TimestampNs")
                .field("len", &values.len())
                .field("utc", utc)
                .finish(),
            Self::Date(v) => f.debug_struct("Date").field("len", &v.len()).finish(),
            Self::Interval(v) => f.debug_struct("Interval").field("len", &v.len()).finish(),
            Self::Utf8(u) => f.debug_tuple("Utf8").field(u).finish(),
            Self::Binary(b) => f.debug_tuple("Binary").field(b).finish(),
            Self::FixedBytes8(v) => f
                .debug_struct("FixedBytes8")
                .field("len", &v.len())
                .finish(),
            Self::FixedBytes16(v) => f
                .debug_struct("FixedBytes16")
                .field("len", &v.len())
                .finish(),
            Self::List {
                item_type,
                offsets,
                values,
            } => f
                .debug_struct("List")
                .field("item_type", item_type)
                .field("len", &offsets.len().saturating_sub(1))
                .field("values", values)
                .finish(),
            Self::MapUtf8(m) => f.debug_tuple("MapUtf8").field(m).finish(),
            Self::Events(e) => f.debug_tuple("Events").field(e).finish(),
            Self::Links(l) => f.debug_tuple("Links").field(l).finish(),
        }
    }
}

impl FlatData {
    /// Logical row count.
    pub fn len(&self) -> usize {
        match self {
            Self::Null(n) => *n,
            Self::Bool(bm) => bm.len(),
            Self::I64(v) => v.len(),
            Self::F64(v) => v.len(),
            Self::Decimal128 { values, .. } => values.len(),
            Self::TimestampNs { values, .. } => values.len(),
            Self::Date(v) => v.len(),
            Self::Interval(v) => v.len(),
            Self::Utf8(u) => u.len(),
            Self::Binary(b) => b.len(),
            Self::FixedBytes8(v) => v.len(),
            Self::FixedBytes16(v) => v.len(),
            Self::List { offsets, .. } => offsets.len().saturating_sub(1),
            Self::MapUtf8(m) => m.len(),
            Self::Events(e) => e.len(),
            Self::Links(l) => l.len(),
        }
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Null(_) => DataType::Null,
            Self::Bool(_) => DataType::Bool,
            Self::I64(_) => DataType::I64,
            Self::F64(_) => DataType::F64,
            Self::Decimal128 {
                precision, scale, ..
            } => DataType::Decimal128 {
                precision: *precision,
                scale: *scale,
            },
            Self::TimestampNs { utc, .. } => DataType::TimestampNs { utc: *utc },
            Self::Date(_) => DataType::Date,
            Self::Interval(_) => DataType::Interval,
            Self::Utf8(_) => DataType::Utf8,
            Self::Binary(_) => DataType::Binary,
            Self::FixedBytes8(_) => DataType::FixedBytes8,
            Self::FixedBytes16(_) => DataType::FixedBytes16,
            Self::List { item_type, .. } => DataType::List(Box::new(item_type.clone())),
            Self::MapUtf8(_) => DataType::MapUtf8,
            Self::Events(_) => DataType::Events,
            Self::Links(_) => DataType::Links,
        }
    }

    /// Empty flat data (0 rows) for `data_type`.
    pub fn empty(data_type: &DataType) -> Self {
        match data_type {
            DataType::Null => Self::Null(0),
            DataType::Bool => Self::Bool(Bitmap::new_zeroed(0)),
            DataType::I64 => Self::I64(Vec::new()),
            DataType::F64 => Self::F64(Vec::new()),
            DataType::Decimal128 { precision, scale } => Self::Decimal128 {
                values: Vec::new(),
                precision: *precision,
                scale: *scale,
            },
            DataType::TimestampNs { utc } => Self::TimestampNs {
                values: Vec::new(),
                utc: *utc,
            },
            DataType::Date => Self::Date(Vec::new()),
            DataType::Interval => Self::Interval(Vec::new()),
            DataType::Utf8 => Self::Utf8(Utf8Data::empty()),
            DataType::Binary => Self::Binary(BinaryData::empty()),
            DataType::FixedBytes8 => Self::FixedBytes8(Vec::new()),
            DataType::FixedBytes16 => Self::FixedBytes16(Vec::new()),
            DataType::List(inner) => Self::List {
                item_type: (**inner).clone(),
                offsets: vec![0],
                values: Box::new(Column::Flat(FlatColumn::empty(inner))),
            },
            DataType::MapUtf8 => Self::MapUtf8(Box::new(MapData::empty())),
            DataType::Events => Self::Events(Box::new(EventsData::empty())),
            DataType::Links => Self::Links(Box::new(LinksData::empty())),
        }
    }

    /// Deterministic logical heap bytes across all backing buffers.
    pub fn memory_bytes(&self) -> usize {
        match self {
            Self::Null(_) => 0,
            Self::Bool(bm) => bm.memory_bytes(),
            Self::I64(v) => v.len() * 8,
            Self::F64(v) => v.len() * 8,
            Self::Decimal128 { values, .. } => values.len() * 16,
            Self::TimestampNs { values, .. } => values.len() * 8,
            Self::Date(v) => v.len() * 4,
            Self::Interval(v) => v.len() * size_of::<Interval>(),
            Self::Utf8(u) => u.memory_bytes(),
            Self::Binary(b) => b.memory_bytes(),
            Self::FixedBytes8(v) => v.len() * 8,
            Self::FixedBytes16(v) => v.len() * 16,
            Self::List {
                offsets, values, ..
            } => offsets.len() * 4 + values.memory_bytes(),
            Self::MapUtf8(m) => m.memory_bytes(),
            Self::Events(e) => e.memory_bytes(),
            Self::Links(l) => l.memory_bytes(),
        }
    }
}

/// Uncompressed column with an optional validity bitmap (`1` = valid, `0` = null).
/// Canonical invariant: `nulls` is `None` when all rows are valid or for `FlatData::Null`.
#[derive(Clone, PartialEq)]
pub struct FlatColumn {
    data: FlatData,
    nulls: Option<Bitmap>,
}

impl fmt::Debug for FlatColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlatColumn")
            .field("data", &self.data)
            .field("has_nulls", &self.has_nulls())
            .finish()
    }
}

impl FlatColumn {
    /// Validates `data` invariants (`data.data_type().validate(MAX_NESTING_DEPTH)`, decimal bounds,
    /// list offsets/child type/length/child `Const` scalar validity) and `nulls` length.
    /// Normalizes `Some(all_ones)` to `None` and normalizes `FlatData::Null(_)` to `nulls: None`.
    pub fn try_new(data: FlatData, nulls: Option<Bitmap>) -> Result<Self, ColumnError> {
        data.data_type().validate(MAX_NESTING_DEPTH)?;
        if let FlatData::List {
            item_type,
            offsets,
            values,
        } = &data
        {
            validate_offsets(offsets, values.len())?;
            if values.data_type() != *item_type {
                return Err(ColumnError::InvalidBatch {
                    reason: "list child column type does not match item_type",
                });
            }
            values.validate_with_depth(MAX_NESTING_DEPTH - 1)?;
        }
        let len = data.len();
        let normalized_nulls = if matches!(data, FlatData::Null(_)) {
            if let Some(bm) = nulls
                && bm.len() != len
            {
                return Err(ColumnError::InvalidBatch {
                    reason: "nulls bitmap length mismatch",
                });
            }
            None
        } else {
            normalize_nulls(nulls, len)?
        };
        Ok(Self {
            data,
            nulls: normalized_nulls,
        })
    }

    /// Empty flat column (0 rows) for `data_type`.
    pub fn empty(data_type: &DataType) -> Self {
        Self {
            data: FlatData::empty(data_type),
            nulls: None,
        }
    }

    pub(crate) fn validate_with_depth(&self, max_depth: usize) -> Result<(), ColumnError> {
        self.data_type().validate(max_depth)?;
        if let FlatData::List { values, .. } = self.data() {
            if max_depth == 0 {
                return Err(ColumnError::InvalidBatch {
                    reason: "exceeded maximum nesting depth",
                });
            }
            values.validate_with_depth(max_depth - 1)?;
        }
        Ok(())
    }

    /// Underlying flat values buffer.
    pub fn data(&self) -> &FlatData {
        &self.data
    }

    /// Optional validity bitmap (`1` = valid, `0` = null).
    pub fn nulls(&self) -> Option<&Bitmap> {
        self.nulls.as_ref()
    }

    /// Row count.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType {
        self.data.data_type()
    }

    /// True iff `row < self.len()` and that row is null (`FlatData::Null` is null for all `row < len`).
    pub fn is_null(&self, row: usize) -> bool {
        if row >= self.len() {
            return false;
        }
        if matches!(self.data, FlatData::Null(_)) {
            return true;
        }
        self.nulls.as_ref().is_some_and(|bm| !bm.get(row))
    }

    /// True if at least one row in `0..len` is null.
    pub fn has_nulls(&self) -> bool {
        if matches!(self.data, FlatData::Null(n) if n > 0) {
            return true;
        }
        self.nulls.as_ref().is_some_and(|bm| !bm.all_ones())
    }

    /// Extracts row `row` as a [`ScalarValue`], or `InvalidBatch` if out of bounds.
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError> {
        if row >= self.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "row index out of bounds",
            });
        }
        if self.is_null(row) {
            return Ok(ScalarValue::Null(self.data_type()));
        }
        match &self.data {
            FlatData::Null(_) => Ok(ScalarValue::Null(DataType::Null)),
            FlatData::Bool(bm) => Ok(ScalarValue::Bool(bm.get(row))),
            FlatData::I64(v) => Ok(ScalarValue::I64(v[row])),
            FlatData::F64(v) => Ok(ScalarValue::F64(v[row])),
            FlatData::Decimal128 {
                values,
                precision,
                scale,
            } => Ok(ScalarValue::Decimal128 {
                value: values[row],
                precision: *precision,
                scale: *scale,
            }),
            FlatData::TimestampNs { values, utc } => Ok(ScalarValue::TimestampNs {
                value: values[row],
                utc: *utc,
            }),
            FlatData::Date(v) => Ok(ScalarValue::Date(v[row])),
            FlatData::Interval(v) => Ok(ScalarValue::Interval(v[row])),
            FlatData::Utf8(u) => {
                let s = u.get(row).ok_or(ColumnError::InvalidBatch {
                    reason: "utf8 row out of bounds",
                })?;
                Ok(ScalarValue::Utf8(s.to_owned()))
            }
            FlatData::Binary(b) => {
                let bytes = b.get(row).ok_or(ColumnError::InvalidBatch {
                    reason: "binary row out of bounds",
                })?;
                Ok(ScalarValue::Binary(bytes.to_vec()))
            }
            FlatData::FixedBytes8(v) => Ok(ScalarValue::FixedBytes8(v[row])),
            FlatData::FixedBytes16(v) => Ok(ScalarValue::FixedBytes16(v[row])),
            FlatData::List {
                item_type,
                offsets,
                values,
            } => {
                let start = offsets[row] as usize;
                let end = offsets[row + 1] as usize;
                let mut items = Vec::with_capacity(end - start);
                for idx in start..end {
                    items.push(values.get_scalar(idx)?);
                }
                Ok(ScalarValue::List {
                    item_type: item_type.clone(),
                    values: items,
                })
            }
            FlatData::MapUtf8(m) => {
                let entries = m.get(row).ok_or(ColumnError::InvalidBatch {
                    reason: "map row out of bounds",
                })?;
                Ok(ScalarValue::MapUtf8(entries))
            }
            FlatData::Events(e) => {
                let evs = e.get(row).ok_or(ColumnError::InvalidBatch {
                    reason: "events row out of bounds",
                })?;
                Ok(ScalarValue::Events(evs))
            }
            FlatData::Links(l) => {
                let lks = l.get(row).ok_or(ColumnError::InvalidBatch {
                    reason: "links row out of bounds",
                })?;
                Ok(ScalarValue::Links(lks))
            }
        }
    }

    /// Slices `offset..offset + len`, accounting for newly allocated slice buffers in `pool`
    /// when `Some` (rolling back on error).
    pub fn slice(
        &self,
        offset: usize,
        len: usize,
        mut pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "flat column slice out of bounds",
            });
        }
        let before = pool.as_ref().map_or(0, |p| p.size());
        match self.slice_inner(offset, len, pool.as_deref_mut(), before) {
            Ok(out) => Ok(out),
            Err(err) => {
                rollback_reservation(pool, before);
                Err(err)
            }
        }
    }

    fn slice_inner(
        &self,
        offset: usize,
        len: usize,
        mut pool: Option<&mut Reservation>,
        before: usize,
    ) -> Result<Self, ColumnError> {
        let nulls_max_bytes = if self.nulls.is_some() {
            checked_mul_bytes(len.div_ceil(64), 8, "flat slice nulls byte overflow")?
        } else {
            0
        };

        if let FlatData::List {
            item_type,
            offsets,
            values,
        } = &self.data
        {
            let offsets_bytes = checked_mul_bytes(
                checked_add_bytes(len, 1, "list slice offsets overflow")?,
                4,
                "list slice offsets overflow",
            )?;
            let pre_bytes =
                checked_add_bytes(offsets_bytes, nulls_max_bytes, "list slice byte overflow")?;
            if let Some(res) = pool.as_deref_mut() {
                res.try_grow(pre_bytes)?;
            }
            let base = offsets[offset];
            let child_start = base as usize;
            let child_end = offsets[offset + len] as usize;
            let new_values =
                values.slice(child_start, child_end - child_start, pool.as_deref_mut())?;
            let new_offsets = offsets[offset..=offset + len]
                .iter()
                .map(|&o| o - base)
                .collect();
            let nulls = slice_optional_bitmap(self.nulls.as_ref(), offset, len)?;
            if nulls.is_none()
                && nulls_max_bytes > 0
                && let Some(res) = pool
            {
                res.shrink(nulls_max_bytes);
            }
            return Ok(Self {
                data: FlatData::List {
                    item_type: item_type.clone(),
                    offsets: new_offsets,
                    values: Box::new(new_values),
                },
                nulls,
            });
        }

        let max_data_bytes = match &self.data {
            FlatData::Null(_) => 0,
            FlatData::Bool(_) => checked_mul_bytes(len.div_ceil(64), 8, "bool slice overflow")?,
            FlatData::I64(_)
            | FlatData::F64(_)
            | FlatData::TimestampNs { .. }
            | FlatData::FixedBytes8(_) => checked_mul_bytes(len, 8, "flat slice byte overflow")?,
            FlatData::Decimal128 { .. } | FlatData::Interval(_) | FlatData::FixedBytes16(_) => {
                checked_mul_bytes(len, 16, "flat slice byte overflow")?
            }
            FlatData::Date(_) => checked_mul_bytes(len, 4, "flat slice byte overflow")?,
            FlatData::Utf8(u) => {
                let offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(len, 1, "utf8 slice offsets overflow")?,
                    4,
                    "utf8 slice offsets overflow",
                )?;
                let str_bytes = (u.offsets()[offset + len] - u.offsets()[offset]) as usize;
                checked_add_bytes(offsets_bytes, str_bytes, "utf8 slice byte overflow")?
            }
            FlatData::Binary(b) => {
                let offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(len, 1, "binary slice offsets overflow")?,
                    4,
                    "binary slice offsets overflow",
                )?;
                let bin_bytes = (b.offsets()[offset + len] - b.offsets()[offset]) as usize;
                checked_add_bytes(offsets_bytes, bin_bytes, "binary slice byte overflow")?
            }
            FlatData::MapUtf8(m) => {
                let offsets_bytes = checked_mul_bytes(len + 1, 4, "map slice offsets overflow")?;
                let start = m.offsets()[offset] as usize;
                let end = m.offsets()[offset + len] as usize;
                let count = end - start;
                let entry_offsets = checked_mul_bytes(count + 1, 4, "map slice byte overflow")?;
                let key_bytes = (m.keys().offsets()[end] - m.keys().offsets()[start]) as usize;
                let val_bytes = (m.values().offsets()[end] - m.values().offsets()[start]) as usize;
                let vnulls = if m.value_nulls().is_some() {
                    checked_mul_bytes(count.div_ceil(64), 8, "map slice byte overflow")?
                } else {
                    0
                };
                offsets_bytes
                    .checked_add(entry_offsets)
                    .and_then(|s| s.checked_add(key_bytes))
                    .and_then(|s| s.checked_add(entry_offsets))
                    .and_then(|s| s.checked_add(val_bytes))
                    .and_then(|s| s.checked_add(vnulls))
                    .ok_or(ColumnError::Overflow {
                        reason: "map slice byte overflow",
                    })?
            }
            FlatData::Events(e) => {
                let offsets_bytes = checked_mul_bytes(len + 1, 4, "events slice offsets overflow")?;
                let start = e.offsets()[offset] as usize;
                let end = e.offsets()[offset + len] as usize;
                let count = end - start;
                let times_bytes = checked_mul_bytes(count, 8, "events slice byte overflow")?;
                let entry_offsets = checked_mul_bytes(count + 1, 4, "events slice byte overflow")?;
                let name_bytes = (e.names().offsets()[end] - e.names().offsets()[start]) as usize;
                let attr_bytes =
                    (e.attributes().offsets()[end] - e.attributes().offsets()[start]) as usize;
                let anulls = if e.attribute_nulls().is_some() {
                    checked_mul_bytes(count.div_ceil(64), 8, "events slice byte overflow")?
                } else {
                    0
                };
                offsets_bytes
                    .checked_add(times_bytes)
                    .and_then(|s| s.checked_add(entry_offsets))
                    .and_then(|s| s.checked_add(name_bytes))
                    .and_then(|s| s.checked_add(entry_offsets))
                    .and_then(|s| s.checked_add(attr_bytes))
                    .and_then(|s| s.checked_add(anulls))
                    .ok_or(ColumnError::Overflow {
                        reason: "events slice byte overflow",
                    })?
            }
            FlatData::Links(l) => {
                let offsets_bytes = checked_mul_bytes(len + 1, 4, "links slice offsets overflow")?;
                let start = l.offsets()[offset] as usize;
                let end = l.offsets()[offset + len] as usize;
                let count = end - start;
                let ids_bytes = checked_mul_bytes(count, 24, "links slice byte overflow")?;
                let entry_offsets = checked_mul_bytes(count + 1, 4, "links slice byte overflow")?;
                let attr_bytes =
                    (l.attributes().offsets()[end] - l.attributes().offsets()[start]) as usize;
                let anulls = if l.attribute_nulls().is_some() {
                    checked_mul_bytes(count.div_ceil(64), 8, "links slice byte overflow")?
                } else {
                    0
                };
                offsets_bytes
                    .checked_add(ids_bytes)
                    .and_then(|s| s.checked_add(entry_offsets))
                    .and_then(|s| s.checked_add(attr_bytes))
                    .and_then(|s| s.checked_add(anulls))
                    .ok_or(ColumnError::Overflow {
                        reason: "links slice byte overflow",
                    })?
            }
            FlatData::List { .. } => unreachable!(),
        };
        let pre_bytes = checked_add_bytes(max_data_bytes, nulls_max_bytes, "flat slice overflow")?;
        if let Some(res) = pool.as_deref_mut() {
            res.try_grow(pre_bytes)?;
        }

        let data = match &self.data {
            FlatData::Null(_) => FlatData::Null(len),
            FlatData::Bool(bm) => FlatData::Bool(bm.slice(offset, len)?),
            FlatData::I64(v) => FlatData::I64(v[offset..offset + len].to_vec()),
            FlatData::F64(v) => FlatData::F64(v[offset..offset + len].to_vec()),
            FlatData::Decimal128 {
                values,
                precision,
                scale,
            } => FlatData::Decimal128 {
                values: values[offset..offset + len].to_vec(),
                precision: *precision,
                scale: *scale,
            },
            FlatData::TimestampNs { values, utc } => FlatData::TimestampNs {
                values: values[offset..offset + len].to_vec(),
                utc: *utc,
            },
            FlatData::Date(v) => FlatData::Date(v[offset..offset + len].to_vec()),
            FlatData::Interval(v) => FlatData::Interval(v[offset..offset + len].to_vec()),
            FlatData::Utf8(u) => FlatData::Utf8(u.slice(offset, len)?),
            FlatData::Binary(b) => FlatData::Binary(b.slice(offset, len)?),
            FlatData::FixedBytes8(v) => FlatData::FixedBytes8(v[offset..offset + len].to_vec()),
            FlatData::FixedBytes16(v) => FlatData::FixedBytes16(v[offset..offset + len].to_vec()),
            FlatData::MapUtf8(m) => FlatData::MapUtf8(Box::new(m.slice(offset, len)?)),
            FlatData::Events(e) => FlatData::Events(Box::new(e.slice(offset, len)?)),
            FlatData::Links(l) => FlatData::Links(Box::new(l.slice(offset, len)?)),
            FlatData::List { .. } => unreachable!(),
        };
        let nulls = slice_optional_bitmap(self.nulls.as_ref(), offset, len)?;
        let out = Self { data, nulls };
        reconcile_reservation(pool, before, out.memory_bytes())?;
        Ok(out)
    }

    /// Gathers rows by `indices`, writing canonical zero/empty backing values into null slots
    /// and reserving output `memory_bytes()` from `pool` (rolling back on error).
    pub fn take(
        &self,
        indices: &[u32],
        pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        self.take_with_mask(indices, None, pool)
    }

    pub(crate) fn take_with_mask(
        &self,
        indices: &[u32],
        valid_mask: Option<&Bitmap>,
        mut pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        let before = pool.as_ref().map_or(0, |p| p.size());
        match self.take_with_mask_inner(indices, valid_mask, pool.as_deref_mut(), before) {
            Ok(out) => {
                reconcile_reservation(pool, before, out.memory_bytes())?;
                Ok(out)
            }
            Err(err) => {
                rollback_reservation(pool, before);
                Err(err)
            }
        }
    }

    fn take_with_mask_inner(
        &self,
        indices: &[u32],
        valid_mask: Option<&Bitmap>,
        mut pool: Option<&mut Reservation>,
        before: usize,
    ) -> Result<Self, ColumnError> {
        if let Some(mask) = valid_mask
            && mask.len() != indices.len()
        {
            return Err(ColumnError::InvalidBatch {
                reason: "take null_mask length mismatch",
            });
        }

        let mut any_null = false;
        for (out_pos, &idx) in indices.iter().enumerate() {
            if valid_mask.is_some_and(|m| !m.get(out_pos)) {
                any_null = true;
            } else {
                let row = idx as usize;
                if row >= self.len() {
                    return Err(ColumnError::InvalidBatch {
                        reason: "take index out of bounds",
                    });
                }
                if self.is_null(row) {
                    any_null = true;
                }
            }
        }

        let has_out_nulls = any_null && !matches!(self.data, FlatData::Null(_));
        let nulls_bytes = if has_out_nulls {
            indices.len().div_ceil(64) * 8
        } else {
            0
        };

        let build_out_nulls = || -> Option<Bitmap> {
            if !has_out_nulls {
                return None;
            }
            let mut out_valid = Bitmap::new_ones(indices.len());
            for (out_pos, &idx) in indices.iter().enumerate() {
                if valid_mask.is_some_and(|m| !m.get(out_pos)) || self.is_null(idx as usize) {
                    let _ = out_valid.set(out_pos, false);
                }
            }
            Some(out_valid)
        };

        let (data, out_nulls) = match &self.data {
            FlatData::Null(_) => (FlatData::Null(indices.len()), None),
            FlatData::Bool(src) => {
                let out_bytes = checked_add_bytes(
                    indices.len().div_ceil(64) * 8,
                    nulls_bytes,
                    "bool take byte size overflow",
                )?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                let mut bm = Bitmap::new_zeroed(indices.len());
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if out_nulls.as_ref().is_none_or(|m| m.get(out_pos)) && src.get(idx as usize) {
                        let _ = bm.set(out_pos, true);
                    }
                }
                (FlatData::Bool(bm), out_nulls)
            }
            FlatData::I64(src) => {
                let out_bytes = fixed_take_bytes(indices.len(), 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::I64(gather_fixed(src, indices, out_nulls.as_ref(), 0)),
                    out_nulls,
                )
            }
            FlatData::F64(src) => {
                let out_bytes = fixed_take_bytes(indices.len(), 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::F64(gather_fixed(src, indices, out_nulls.as_ref(), 0.0)),
                    out_nulls,
                )
            }
            FlatData::Decimal128 {
                values,
                precision,
                scale,
            } => {
                let out_bytes = fixed_take_bytes(indices.len(), 16, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::Decimal128 {
                        values: gather_fixed(values, indices, out_nulls.as_ref(), 0),
                        precision: *precision,
                        scale: *scale,
                    },
                    out_nulls,
                )
            }
            FlatData::TimestampNs { values, utc } => {
                let out_bytes = fixed_take_bytes(indices.len(), 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::TimestampNs {
                        values: gather_fixed(values, indices, out_nulls.as_ref(), 0),
                        utc: *utc,
                    },
                    out_nulls,
                )
            }
            FlatData::Date(src) => {
                let out_bytes = fixed_take_bytes(indices.len(), 4, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::Date(gather_fixed(src, indices, out_nulls.as_ref(), 0)),
                    out_nulls,
                )
            }
            FlatData::Interval(src) => {
                let out_bytes =
                    fixed_take_bytes(indices.len(), size_of::<Interval>(), nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::Interval(gather_fixed(
                        src,
                        indices,
                        out_nulls.as_ref(),
                        Interval::default(),
                    )),
                    out_nulls,
                )
            }
            FlatData::FixedBytes8(src) => {
                let out_bytes = fixed_take_bytes(indices.len(), 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::FixedBytes8(gather_fixed(src, indices, out_nulls.as_ref(), [0u8; 8])),
                    out_nulls,
                )
            }
            FlatData::FixedBytes16(src) => {
                let out_bytes = fixed_take_bytes(indices.len(), 16, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                (
                    FlatData::FixedBytes16(gather_fixed(
                        src,
                        indices,
                        out_nulls.as_ref(),
                        [0u8; 16],
                    )),
                    out_nulls,
                )
            }
            FlatData::Utf8(u) => {
                let out_bytes = varlen_take_bytes(
                    &u.offsets,
                    self.nulls.as_ref(),
                    indices,
                    valid_mask,
                    nulls_bytes,
                    "utf8 take byte length overflow",
                    "utf8 take byte length exceeds u32::MAX",
                )?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                let gathered = u.take(indices, out_nulls.as_ref())?;
                (FlatData::Utf8(gathered), out_nulls)
            }
            FlatData::Binary(b) => {
                let out_bytes = varlen_take_bytes(
                    &b.offsets,
                    self.nulls.as_ref(),
                    indices,
                    valid_mask,
                    nulls_bytes,
                    "binary take byte length overflow",
                    "binary take byte length exceeds u32::MAX",
                )?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let out_nulls = build_out_nulls();
                let gathered = b.take(indices, out_nulls.as_ref())?;
                (FlatData::Binary(gathered), out_nulls)
            }
            FlatData::List {
                item_type,
                offsets,
                values,
            } => {
                let mut total_child = 0usize;
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if valid_mask.is_none_or(|m| m.get(out_pos)) && !self.is_null(idx as usize) {
                        let row = idx as usize;
                        let count = (offsets[row + 1] - offsets[row]) as usize;
                        total_child = checked_add_bytes(
                            total_child,
                            count,
                            "list take child count overflow",
                        )?;
                    }
                }
                let _ = checked_u32(total_child, "list take child count exceeds u32::MAX")?;
                let offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(indices.len(), 1, "list take offsets overflow")?,
                    4,
                    "list take offsets overflow",
                )?;
                let child_indices_bytes =
                    checked_mul_bytes(total_child, 4, "list take child indices overflow")?;
                let pre_bytes = checked_add_bytes(
                    checked_add_bytes(offsets_bytes, nulls_bytes, "list take bytes overflow")?,
                    child_indices_bytes,
                    "list take bytes overflow",
                )?;
                reconcile_reservation(pool.as_deref_mut(), before, pre_bytes)?;

                let out_nulls = build_out_nulls();
                let mut out_offsets = Vec::with_capacity(indices.len() + 1);
                out_offsets.push(0);
                let mut child_indices = Vec::with_capacity(total_child);
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if out_nulls.as_ref().is_some_and(|m| !m.get(out_pos)) {
                        out_offsets.push(child_indices.len() as u32);
                    } else {
                        let row = idx as usize;
                        let start = offsets[row];
                        let end = offsets[row + 1];
                        for c in start..end {
                            child_indices.push(c);
                        }
                        out_offsets.push(child_indices.len() as u32);
                    }
                }
                let out_values = values.take(&child_indices, pool)?;
                (
                    FlatData::List {
                        item_type: item_type.clone(),
                        offsets: out_offsets,
                        values: Box::new(Column::Flat(out_values)),
                    },
                    out_nulls,
                )
            }
            FlatData::MapUtf8(m) => {
                let mut total_entries = 0usize;
                let mut keys_bytes = 0usize;
                let mut values_bytes = 0usize;
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if valid_mask.is_none_or(|bm| bm.get(out_pos)) && !self.is_null(idx as usize) {
                        let row = idx as usize;
                        let start = m.offsets[row] as usize;
                        let end = m.offsets[row + 1] as usize;
                        total_entries = checked_add_bytes(
                            total_entries,
                            end - start,
                            "map take entry count overflow",
                        )?;
                        keys_bytes = checked_add_bytes(
                            keys_bytes,
                            (m.keys.offsets[end] - m.keys.offsets[start]) as usize,
                            "utf8 take byte length overflow",
                        )?;
                        if let Some(vn) = &m.value_nulls {
                            for src in start..end {
                                if vn.get(src) {
                                    values_bytes = checked_add_bytes(
                                        values_bytes,
                                        (m.values.offsets[src + 1] - m.values.offsets[src])
                                            as usize,
                                        "utf8 take byte length overflow",
                                    )?;
                                }
                            }
                        } else {
                            values_bytes = checked_add_bytes(
                                values_bytes,
                                (m.values.offsets[end] - m.values.offsets[start]) as usize,
                                "utf8 take byte length overflow",
                            )?;
                        }
                    }
                }
                let _ = checked_u32(total_entries, "map take entry count exceeds u32::MAX")?;
                let _ = checked_u32(keys_bytes, "utf8 take byte length exceeds u32::MAX")?;
                let _ = checked_u32(values_bytes, "utf8 take byte length exceeds u32::MAX")?;

                let row_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(indices.len(), 1, "map take offsets overflow")?,
                    4,
                    "map take offsets overflow",
                )?;
                let entry_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_entries, 1, "map take entry offsets overflow")?,
                    4,
                    "map take entry offsets overflow",
                )?;
                let val_nulls_bytes = if m.value_nulls.is_some() {
                    total_entries.div_ceil(64) * 8
                } else {
                    0
                };
                let scratch_bytes =
                    checked_mul_bytes(total_entries, 4, "map take entry indices overflow")?;
                let mut reserve_bytes =
                    checked_add_bytes(row_offsets_bytes, nulls_bytes, "map take bytes overflow")?;
                reserve_bytes = checked_add_bytes(
                    reserve_bytes,
                    entry_offsets_bytes,
                    "map take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, keys_bytes, "map take bytes overflow")?;
                reserve_bytes = checked_add_bytes(
                    reserve_bytes,
                    entry_offsets_bytes,
                    "map take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, values_bytes, "map take bytes overflow")?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, val_nulls_bytes, "map take bytes overflow")?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, scratch_bytes, "map take bytes overflow")?;
                reconcile_reservation(pool.as_deref_mut(), before, reserve_bytes)?;

                let out_nulls = build_out_nulls();
                let gathered = m.take(indices, out_nulls.as_ref())?;
                (FlatData::MapUtf8(Box::new(gathered)), out_nulls)
            }
            FlatData::Events(e) => {
                let mut total_events = 0usize;
                let mut names_bytes = 0usize;
                let mut attrs_bytes = 0usize;
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if valid_mask.is_none_or(|bm| bm.get(out_pos)) && !self.is_null(idx as usize) {
                        let row = idx as usize;
                        let start = e.offsets[row] as usize;
                        let end = e.offsets[row + 1] as usize;
                        total_events = checked_add_bytes(
                            total_events,
                            end - start,
                            "events take count overflow",
                        )?;
                        names_bytes = checked_add_bytes(
                            names_bytes,
                            (e.names.offsets[end] - e.names.offsets[start]) as usize,
                            "utf8 take byte length overflow",
                        )?;
                        if let Some(an) = &e.attribute_nulls {
                            for src in start..end {
                                if an.get(src) {
                                    attrs_bytes = checked_add_bytes(
                                        attrs_bytes,
                                        (e.attributes.offsets[src + 1] - e.attributes.offsets[src])
                                            as usize,
                                        "utf8 take byte length overflow",
                                    )?;
                                }
                            }
                        } else {
                            attrs_bytes = checked_add_bytes(
                                attrs_bytes,
                                (e.attributes.offsets[end] - e.attributes.offsets[start]) as usize,
                                "utf8 take byte length overflow",
                            )?;
                        }
                    }
                }
                let _ = checked_u32(total_events, "events take count exceeds u32::MAX")?;
                let _ = checked_u32(names_bytes, "utf8 take byte length exceeds u32::MAX")?;
                let _ = checked_u32(attrs_bytes, "utf8 take byte length exceeds u32::MAX")?;

                let row_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(indices.len(), 1, "events take offsets overflow")?,
                    4,
                    "events take offsets overflow",
                )?;
                let times_bytes = checked_mul_bytes(total_events, 8, "events take times overflow")?;
                let item_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_events, 1, "events take item offsets overflow")?,
                    4,
                    "events take item offsets overflow",
                )?;
                let attr_nulls_bytes = if e.attribute_nulls.is_some() {
                    total_events.div_ceil(64) * 8
                } else {
                    0
                };
                let scratch_bytes =
                    checked_mul_bytes(total_events, 4, "events take item indices overflow")?;
                let mut reserve_bytes = checked_add_bytes(
                    row_offsets_bytes,
                    nulls_bytes,
                    "events take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, times_bytes, "events take bytes overflow")?;
                reserve_bytes = checked_add_bytes(
                    reserve_bytes,
                    item_offsets_bytes,
                    "events take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, names_bytes, "events take bytes overflow")?;
                reserve_bytes = checked_add_bytes(
                    reserve_bytes,
                    item_offsets_bytes,
                    "events take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, attrs_bytes, "events take bytes overflow")?;
                reserve_bytes = checked_add_bytes(
                    reserve_bytes,
                    attr_nulls_bytes,
                    "events take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, scratch_bytes, "events take bytes overflow")?;
                reconcile_reservation(pool.as_deref_mut(), before, reserve_bytes)?;

                let out_nulls = build_out_nulls();
                let gathered = e.take(indices, out_nulls.as_ref())?;
                (FlatData::Events(Box::new(gathered)), out_nulls)
            }
            FlatData::Links(l) => {
                let mut total_links = 0usize;
                let mut attrs_bytes = 0usize;
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if valid_mask.is_none_or(|bm| bm.get(out_pos)) && !self.is_null(idx as usize) {
                        let row = idx as usize;
                        let start = l.offsets[row] as usize;
                        let end = l.offsets[row + 1] as usize;
                        total_links = checked_add_bytes(
                            total_links,
                            end - start,
                            "links take count overflow",
                        )?;
                        if let Some(an) = &l.attribute_nulls {
                            for src in start..end {
                                if an.get(src) {
                                    attrs_bytes = checked_add_bytes(
                                        attrs_bytes,
                                        (l.attributes.offsets[src + 1] - l.attributes.offsets[src])
                                            as usize,
                                        "utf8 take byte length overflow",
                                    )?;
                                }
                            }
                        } else {
                            attrs_bytes = checked_add_bytes(
                                attrs_bytes,
                                (l.attributes.offsets[end] - l.attributes.offsets[start]) as usize,
                                "utf8 take byte length overflow",
                            )?;
                        }
                    }
                }
                let _ = checked_u32(total_links, "links take count exceeds u32::MAX")?;
                let _ = checked_u32(attrs_bytes, "utf8 take byte length exceeds u32::MAX")?;

                let row_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(indices.len(), 1, "links take offsets overflow")?,
                    4,
                    "links take offsets overflow",
                )?;
                let trace_ids_bytes =
                    checked_mul_bytes(total_links, 16, "links take trace_ids overflow")?;
                let span_ids_bytes =
                    checked_mul_bytes(total_links, 8, "links take span_ids overflow")?;
                let item_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_links, 1, "links take item offsets overflow")?,
                    4,
                    "links take item offsets overflow",
                )?;
                let attr_nulls_bytes = if l.attribute_nulls.is_some() {
                    total_links.div_ceil(64) * 8
                } else {
                    0
                };
                let scratch_bytes =
                    checked_mul_bytes(total_links, 4, "links take item indices overflow")?;
                let mut reserve_bytes =
                    checked_add_bytes(row_offsets_bytes, nulls_bytes, "links take bytes overflow")?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, trace_ids_bytes, "links take bytes overflow")?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, span_ids_bytes, "links take bytes overflow")?;
                reserve_bytes = checked_add_bytes(
                    reserve_bytes,
                    item_offsets_bytes,
                    "links take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, attrs_bytes, "links take bytes overflow")?;
                reserve_bytes = checked_add_bytes(
                    reserve_bytes,
                    attr_nulls_bytes,
                    "links take bytes overflow",
                )?;
                reserve_bytes =
                    checked_add_bytes(reserve_bytes, scratch_bytes, "links take bytes overflow")?;
                reconcile_reservation(pool, before, reserve_bytes)?;

                let out_nulls = build_out_nulls();
                let gathered = l.take(indices, out_nulls.as_ref())?;
                (FlatData::Links(Box::new(gathered)), out_nulls)
            }
        };

        Ok(Self {
            data,
            nulls: out_nulls,
        })
    }

    /// NaN-aware, null-slot-ignoring semantic equality.
    pub fn semantic_eq(&self, other: &Self) -> bool {
        if self.data_type() != other.data_type() || self.len() != other.len() {
            return false;
        }
        for row in 0..self.len() {
            let null_a = self.is_null(row);
            let null_b = other.is_null(row);
            if null_a != null_b {
                return false;
            }
            if null_a {
                continue;
            }
            let eq = match (&self.data, &other.data) {
                (FlatData::Null(_), FlatData::Null(_)) => true,
                (FlatData::Bool(a), FlatData::Bool(b)) => a.get(row) == b.get(row),
                (FlatData::I64(a), FlatData::I64(b)) => a[row] == b[row],
                (FlatData::F64(a), FlatData::F64(b)) => {
                    let x = a[row];
                    let y = b[row];
                    (x.is_nan() && y.is_nan()) || x == y
                }
                (
                    FlatData::Decimal128 { values: a, .. },
                    FlatData::Decimal128 { values: b, .. },
                ) => a[row] == b[row],
                (
                    FlatData::TimestampNs { values: a, .. },
                    FlatData::TimestampNs { values: b, .. },
                ) => a[row] == b[row],
                (FlatData::Date(a), FlatData::Date(b)) => a[row] == b[row],
                (FlatData::Interval(a), FlatData::Interval(b)) => a[row] == b[row],
                (FlatData::Utf8(a), FlatData::Utf8(b)) => a.get(row) == b.get(row),
                (FlatData::Binary(a), FlatData::Binary(b)) => a.get(row) == b.get(row),
                (FlatData::FixedBytes8(a), FlatData::FixedBytes8(b)) => a[row] == b[row],
                (FlatData::FixedBytes16(a), FlatData::FixedBytes16(b)) => a[row] == b[row],
                (
                    FlatData::List {
                        offsets: oa,
                        values: va,
                        ..
                    },
                    FlatData::List {
                        offsets: ob,
                        values: vb,
                        ..
                    },
                ) => {
                    let sa = oa[row] as usize;
                    let ea = oa[row + 1] as usize;
                    let sb = ob[row] as usize;
                    let eb = ob[row + 1] as usize;
                    if ea - sa != eb - sb {
                        false
                    } else {
                        (0..(ea - sa)).all(|k| {
                            matches!(
                                (va.get_scalar(sa + k), vb.get_scalar(sb + k)),
                                (Ok(xa), Ok(xb)) if xa.semantic_eq(&xb)
                            )
                        })
                    }
                }
                (FlatData::MapUtf8(a), FlatData::MapUtf8(b)) => a.get(row) == b.get(row),
                (FlatData::Events(a), FlatData::Events(b)) => a.get(row) == b.get(row),
                (FlatData::Links(a), FlatData::Links(b)) => a.get(row) == b.get(row),
                _ => false,
            };
            if !eq {
                return false;
            }
        }
        true
    }

    /// Deterministic logical heap bytes (`data.memory_bytes() + nulls.map_or(0, Bitmap::memory_bytes)`).
    pub fn memory_bytes(&self) -> usize {
        self.data.memory_bytes() + self.nulls.as_ref().map_or(0, Bitmap::memory_bytes)
    }

    #[cfg(test)]
    pub(crate) fn concat(
        data_type: &DataType,
        parts: &[&Self],
        total_rows: usize,
    ) -> Result<Self, ColumnError> {
        Self::concat_with_pool(data_type, parts, total_rows, None)
    }

    pub(crate) fn concat_with_pool(
        data_type: &DataType,
        parts: &[&Self],
        total_rows: usize,
        mut pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        let before = pool.as_ref().map_or(0, |p| p.size());
        match Self::concat_inner(data_type, parts, total_rows, pool.as_deref_mut(), before) {
            Ok(out) => {
                reconcile_reservation(pool, before, out.memory_bytes())?;
                Ok(out)
            }
            Err(err) => {
                rollback_reservation(pool, before);
                Err(err)
            }
        }
    }

    fn concat_inner(
        data_type: &DataType,
        parts: &[&Self],
        total_rows: usize,
        mut pool: Option<&mut Reservation>,
        before: usize,
    ) -> Result<Self, ColumnError> {
        let sum_rows: usize = parts.iter().try_fold(0usize, |acc, p| {
            checked_add_bytes(acc, p.len(), "concat row count overflow")
        })?;
        if sum_rows != total_rows {
            return Err(ColumnError::InvalidBatch {
                reason: "concat total_rows mismatch",
            });
        }

        if matches!(data_type, DataType::Null) {
            for p in parts {
                if !matches!(p.data(), FlatData::Null(_)) {
                    return Err(ColumnError::InvalidBatch {
                        reason: "concat column data type mismatch",
                    });
                }
            }
            return Ok(Self {
                data: FlatData::Null(total_rows),
                nulls: None,
            });
        }

        let has_nulls = parts
            .iter()
            .any(|p| p.nulls().is_some_and(|bm| !bm.all_ones()));
        let nulls_bytes = if has_nulls {
            total_rows.div_ceil(64) * 8
        } else {
            0
        };
        let build_nulls = || -> Option<Bitmap> {
            if !has_nulls {
                return None;
            }
            let null_parts: Vec<(usize, Option<&Bitmap>)> =
                parts.iter().map(|p| (p.len(), p.nulls())).collect();
            concat_bitmaps(&null_parts, total_rows)
        };

        let data = match data_type {
            DataType::Null => FlatData::Null(total_rows),
            DataType::Bool => {
                for p in parts {
                    if !matches!(p.data(), FlatData::Bool(_)) {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                }
                let out_bytes = checked_add_bytes(
                    total_rows.div_ceil(64) * 8,
                    nulls_bytes,
                    "bool concat byte size overflow",
                )?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut bm = Bitmap::new_zeroed(total_rows);
                let mut pos = 0usize;
                for p in parts {
                    if let FlatData::Bool(src) = p.data() {
                        for i in 0..p.len() {
                            if src.get(i) {
                                let _ = bm.set(pos + i, true);
                            }
                        }
                    }
                    pos += p.len();
                }
                FlatData::Bool(bm)
            }
            DataType::I64 => {
                for p in parts {
                    if !matches!(p.data(), FlatData::I64(_)) {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::I64(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::I64(out)
            }
            DataType::F64 => {
                for p in parts {
                    if !matches!(p.data(), FlatData::F64(_)) {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::F64(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::F64(out)
            }
            DataType::Decimal128 { precision, scale } => {
                for p in parts {
                    match p.data() {
                        FlatData::Decimal128 {
                            precision: pp,
                            scale: ps,
                            ..
                        } if pp == precision && ps == scale => {}
                        _ => {
                            return Err(ColumnError::InvalidBatch {
                                reason: "concat column data type mismatch",
                            });
                        }
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, 16, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::Decimal128 { values, .. } = p.data() {
                        out.extend_from_slice(values);
                    }
                }
                FlatData::Decimal128 {
                    values: out,
                    precision: *precision,
                    scale: *scale,
                }
            }
            DataType::TimestampNs { utc } => {
                for p in parts {
                    match p.data() {
                        FlatData::TimestampNs { utc: pu, .. } if pu == utc => {}
                        _ => {
                            return Err(ColumnError::InvalidBatch {
                                reason: "concat column data type mismatch",
                            });
                        }
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::TimestampNs { values, .. } = p.data() {
                        out.extend_from_slice(values);
                    }
                }
                FlatData::TimestampNs {
                    values: out,
                    utc: *utc,
                }
            }
            DataType::Date => {
                for p in parts {
                    if !matches!(p.data(), FlatData::Date(_)) {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, 4, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::Date(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::Date(out)
            }
            DataType::Interval => {
                for p in parts {
                    if !matches!(p.data(), FlatData::Interval(_)) {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, size_of::<Interval>(), nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::Interval(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::Interval(out)
            }
            DataType::Utf8 => {
                let mut u_parts = Vec::with_capacity(parts.len());
                let mut total_bytes = 0usize;
                for p in parts {
                    let FlatData::Utf8(u) = p.data() else {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    };
                    total_bytes = checked_add_bytes(
                        total_bytes,
                        u.bytes.len(),
                        "utf8 concat byte length overflow",
                    )?;
                    u_parts.push(u);
                }
                let _ = checked_u32(total_bytes, "utf8 concat byte length exceeds u32::MAX")?;
                let offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_rows, 1, "utf8 concat offsets overflow")?,
                    4,
                    "utf8 concat offsets overflow",
                )?;
                let out_bytes = checked_add_bytes(
                    checked_add_bytes(offsets_bytes, total_bytes, "utf8 concat bytes overflow")?,
                    nulls_bytes,
                    "utf8 concat bytes overflow",
                )?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::Utf8(Utf8Data::concat(&u_parts)?)
            }
            DataType::Binary => {
                let mut b_parts = Vec::with_capacity(parts.len());
                let mut total_bytes = 0usize;
                for p in parts {
                    let FlatData::Binary(b) = p.data() else {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    };
                    total_bytes = checked_add_bytes(
                        total_bytes,
                        b.bytes.len(),
                        "binary concat byte length overflow",
                    )?;
                    b_parts.push(b);
                }
                let _ = checked_u32(total_bytes, "binary concat byte length exceeds u32::MAX")?;
                let offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_rows, 1, "binary concat offsets overflow")?,
                    4,
                    "binary concat offsets overflow",
                )?;
                let out_bytes = checked_add_bytes(
                    checked_add_bytes(offsets_bytes, total_bytes, "binary concat bytes overflow")?,
                    nulls_bytes,
                    "binary concat bytes overflow",
                )?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::Binary(BinaryData::concat(&b_parts)?)
            }
            DataType::FixedBytes8 => {
                for p in parts {
                    if !matches!(p.data(), FlatData::FixedBytes8(_)) {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, 8, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::FixedBytes8(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::FixedBytes8(out)
            }
            DataType::FixedBytes16 => {
                for p in parts {
                    if !matches!(p.data(), FlatData::FixedBytes16(_)) {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                }
                let out_bytes = fixed_take_bytes(total_rows, 16, nulls_bytes)?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::FixedBytes16(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::FixedBytes16(out)
            }
            DataType::List(inner) => {
                let mut total_child = 0usize;
                let mut child_cols = Vec::with_capacity(parts.len());
                for p in parts {
                    let FlatData::List {
                        item_type, values, ..
                    } = p.data()
                    else {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    };
                    if item_type != inner.as_ref() {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    }
                    total_child = checked_add_bytes(
                        total_child,
                        values.len(),
                        "list concat child length overflow",
                    )?;
                    child_cols.push(values.as_ref());
                }
                let _ = checked_u32(total_child, "list concat child length exceeds u32::MAX")?;
                let offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_rows, 1, "list concat offsets overflow")?,
                    4,
                    "list concat offsets overflow",
                )?;
                let pre_bytes =
                    checked_add_bytes(offsets_bytes, nulls_bytes, "list concat bytes overflow")?;
                reconcile_reservation(pool.as_deref_mut(), before, pre_bytes)?;

                let mut offsets = Vec::with_capacity(total_rows + 1);
                offsets.push(0);
                let mut running = 0u32;
                for p in parts {
                    if let FlatData::List {
                        offsets: p_off,
                        values,
                        ..
                    } = p.data()
                    {
                        for &o in &p_off[1..] {
                            offsets.push(running + o);
                        }
                        running += values.len() as u32;
                    }
                }

                let mut flat_children: Vec<FlatPiece<'_>> = Vec::with_capacity(child_cols.len());
                for col in &child_cols {
                    let piece = match col {
                        Column::Flat(fc) => FlatPiece::Borrowed(fc),
                        other => FlatPiece::Owned(other.materialize(pool.as_deref_mut())?),
                    };
                    flat_children.push(piece);
                }
                let child_refs: Vec<&Self> = flat_children.iter().map(FlatPiece::as_flat).collect();
                let merged_child = Self::concat_with_pool(inner, &child_refs, total_child, pool)?;
                FlatData::List {
                    item_type: (**inner).clone(),
                    offsets,
                    values: Box::new(Column::Flat(merged_child)),
                }
            }
            DataType::MapUtf8 => {
                let mut m_parts = Vec::with_capacity(parts.len());
                let mut total_entries = 0usize;
                let mut keys_bytes = 0usize;
                let mut values_bytes = 0usize;
                let mut has_val_nulls = false;
                for p in parts {
                    let FlatData::MapUtf8(m) = p.data() else {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    };
                    total_entries = checked_add_bytes(
                        total_entries,
                        m.entry_count(),
                        "map concat entry count overflow",
                    )?;
                    keys_bytes = checked_add_bytes(
                        keys_bytes,
                        m.keys.bytes.len(),
                        "utf8 concat byte length overflow",
                    )?;
                    values_bytes = checked_add_bytes(
                        values_bytes,
                        m.values.bytes.len(),
                        "utf8 concat byte length overflow",
                    )?;
                    if m.value_nulls.as_ref().is_some_and(|bm| !bm.all_ones()) {
                        has_val_nulls = true;
                    }
                    m_parts.push(m.as_ref());
                }
                let _ = checked_u32(total_entries, "map concat entry count exceeds u32::MAX")?;
                let _ = checked_u32(keys_bytes, "utf8 concat byte length exceeds u32::MAX")?;
                let _ = checked_u32(values_bytes, "utf8 concat byte length exceeds u32::MAX")?;
                let row_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_rows, 1, "map concat offsets overflow")?,
                    4,
                    "map concat offsets overflow",
                )?;
                let entry_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_entries, 1, "map concat entry offsets overflow")?,
                    4,
                    "map concat entry offsets overflow",
                )?;
                let val_nulls_bytes = if has_val_nulls {
                    total_entries.div_ceil(64) * 8
                } else {
                    0
                };
                let mut out_bytes =
                    checked_add_bytes(row_offsets_bytes, nulls_bytes, "map concat bytes overflow")?;
                out_bytes =
                    checked_add_bytes(out_bytes, entry_offsets_bytes, "map concat bytes overflow")?;
                out_bytes = checked_add_bytes(out_bytes, keys_bytes, "map concat bytes overflow")?;
                out_bytes =
                    checked_add_bytes(out_bytes, entry_offsets_bytes, "map concat bytes overflow")?;
                out_bytes =
                    checked_add_bytes(out_bytes, values_bytes, "map concat bytes overflow")?;
                out_bytes =
                    checked_add_bytes(out_bytes, val_nulls_bytes, "map concat bytes overflow")?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::MapUtf8(Box::new(MapData::concat(&m_parts)?))
            }
            DataType::Events => {
                let mut e_parts = Vec::with_capacity(parts.len());
                let mut total_events = 0usize;
                let mut names_bytes = 0usize;
                let mut attrs_bytes = 0usize;
                let mut has_attr_nulls = false;
                for p in parts {
                    let FlatData::Events(e) = p.data() else {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    };
                    total_events = checked_add_bytes(
                        total_events,
                        e.event_count(),
                        "events concat count overflow",
                    )?;
                    names_bytes = checked_add_bytes(
                        names_bytes,
                        e.names.bytes.len(),
                        "utf8 concat byte length overflow",
                    )?;
                    attrs_bytes = checked_add_bytes(
                        attrs_bytes,
                        e.attributes.bytes.len(),
                        "utf8 concat byte length overflow",
                    )?;
                    if e.attribute_nulls.as_ref().is_some_and(|bm| !bm.all_ones()) {
                        has_attr_nulls = true;
                    }
                    e_parts.push(e.as_ref());
                }
                let _ = checked_u32(total_events, "events concat count exceeds u32::MAX")?;
                let _ = checked_u32(names_bytes, "utf8 concat byte length exceeds u32::MAX")?;
                let _ = checked_u32(attrs_bytes, "utf8 concat byte length exceeds u32::MAX")?;
                let row_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_rows, 1, "events concat offsets overflow")?,
                    4,
                    "events concat offsets overflow",
                )?;
                let times_bytes =
                    checked_mul_bytes(total_events, 8, "events concat times overflow")?;
                let item_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_events, 1, "events concat item offsets overflow")?,
                    4,
                    "events concat item offsets overflow",
                )?;
                let attr_nulls_bytes = if has_attr_nulls {
                    total_events.div_ceil(64) * 8
                } else {
                    0
                };
                let mut out_bytes = checked_add_bytes(
                    row_offsets_bytes,
                    nulls_bytes,
                    "events concat bytes overflow",
                )?;
                out_bytes =
                    checked_add_bytes(out_bytes, times_bytes, "events concat bytes overflow")?;
                out_bytes = checked_add_bytes(
                    out_bytes,
                    item_offsets_bytes,
                    "events concat bytes overflow",
                )?;
                out_bytes =
                    checked_add_bytes(out_bytes, names_bytes, "events concat bytes overflow")?;
                out_bytes = checked_add_bytes(
                    out_bytes,
                    item_offsets_bytes,
                    "events concat bytes overflow",
                )?;
                out_bytes =
                    checked_add_bytes(out_bytes, attrs_bytes, "events concat bytes overflow")?;
                out_bytes =
                    checked_add_bytes(out_bytes, attr_nulls_bytes, "events concat bytes overflow")?;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::Events(Box::new(EventsData::concat(&e_parts)?))
            }
            DataType::Links => {
                let mut l_parts = Vec::with_capacity(parts.len());
                let mut total_links = 0usize;
                let mut attrs_bytes = 0usize;
                let mut has_attr_nulls = false;
                for p in parts {
                    let FlatData::Links(l) = p.data() else {
                        return Err(ColumnError::InvalidBatch {
                            reason: "concat column data type mismatch",
                        });
                    };
                    total_links = checked_add_bytes(
                        total_links,
                        l.link_count(),
                        "links concat count overflow",
                    )?;
                    attrs_bytes = checked_add_bytes(
                        attrs_bytes,
                        l.attributes.bytes.len(),
                        "utf8 concat byte length overflow",
                    )?;
                    if l.attribute_nulls.as_ref().is_some_and(|bm| !bm.all_ones()) {
                        has_attr_nulls = true;
                    }
                    l_parts.push(l.as_ref());
                }
                let _ = checked_u32(total_links, "links concat count exceeds u32::MAX")?;
                let _ = checked_u32(attrs_bytes, "utf8 concat byte length exceeds u32::MAX")?;
                let row_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_rows, 1, "links concat offsets overflow")?,
                    4,
                    "links concat offsets overflow",
                )?;
                let trace_ids_bytes =
                    checked_mul_bytes(total_links, 16, "links concat trace_ids overflow")?;
                let span_ids_bytes =
                    checked_mul_bytes(total_links, 8, "links concat span_ids overflow")?;
                let item_offsets_bytes = checked_mul_bytes(
                    checked_add_bytes(total_links, 1, "links concat item offsets overflow")?,
                    4,
                    "links concat item offsets overflow",
                )?;
                let attr_nulls_bytes = if has_attr_nulls {
                    total_links.div_ceil(64) * 8
                } else {
                    0
                };
                let mut out_bytes = checked_add_bytes(
                    row_offsets_bytes,
                    nulls_bytes,
                    "links concat bytes overflow",
                )?;
                out_bytes =
                    checked_add_bytes(out_bytes, trace_ids_bytes, "links concat bytes overflow")?;
                out_bytes =
                    checked_add_bytes(out_bytes, span_ids_bytes, "links concat bytes overflow")?;
                out_bytes = checked_add_bytes(
                    out_bytes,
                    item_offsets_bytes,
                    "links concat bytes overflow",
                )?;
                out_bytes =
                    checked_add_bytes(out_bytes, attrs_bytes, "links concat bytes overflow")?;
                out_bytes =
                    checked_add_bytes(out_bytes, attr_nulls_bytes, "links concat bytes overflow")?;
                reconcile_reservation(pool, before, out_bytes)?;
                FlatData::Links(Box::new(LinksData::concat(&l_parts)?))
            }
        };

        let nulls = build_nulls();
        Ok(Self { data, nulls })
    }
}

fn gather_fixed<T: Copy>(
    src: &[T],
    indices: &[u32],
    out_nulls: Option<&Bitmap>,
    zero: T,
) -> Vec<T> {
    let mut out = Vec::with_capacity(indices.len());
    for (out_pos, &idx) in indices.iter().enumerate() {
        if out_nulls.is_some_and(|m| !m.get(out_pos)) {
            out.push(zero);
        } else {
            out.push(src[idx as usize]);
        }
    }
    out
}

/// Dictionary-encoded column (`values` holds distinct entries; every non-null `codes[i] < values.len()`).
#[derive(Clone, PartialEq)]
pub struct DictColumn {
    values: Arc<FlatColumn>,
    codes: Vec<u32>,
    nulls: Option<Bitmap>,
}

impl fmt::Debug for DictColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DictColumn")
            .field("len", &self.len())
            .field("dict_len", &self.values.len())
            .field("has_code_nulls", &self.nulls.is_some())
            .field("values", &self.values)
            .finish()
    }
}

impl DictColumn {
    /// Validates `nulls` length (normalizing all-ones to `None`) and checks every non-null code `< values.len()`.
    pub fn try_new(
        values: FlatColumn,
        codes: Vec<u32>,
        nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError> {
        Self::try_new_shared(Arc::new(values), codes, nulls)
    }

    /// Validates `nulls` length (normalizing all-ones to `None`) and checks every non-null code `< values.len()`
    /// over a shared `Arc<FlatColumn>` dictionary.
    pub fn try_new_shared(
        values: Arc<FlatColumn>,
        codes: Vec<u32>,
        nulls: Option<Bitmap>,
    ) -> Result<Self, ColumnError> {
        let nulls = normalize_nulls(nulls, codes.len())?;
        let dict_len = values.len();
        for (row, &code) in codes.iter().enumerate() {
            if nulls.as_ref().is_none_or(|bm| bm.get(row)) && (code as usize) >= dict_len {
                return Err(ColumnError::InvalidBatch {
                    reason: "dictionary code out of bounds",
                });
            }
        }
        Ok(Self {
            values,
            codes,
            nulls,
        })
    }

    /// Dictionary values table.
    pub fn values(&self) -> &FlatColumn {
        &self.values
    }

    /// Shared `Arc` handle to the dictionary values table.
    pub fn values_arc(&self) -> &Arc<FlatColumn> {
        &self.values
    }

    /// Dictionary codes (one per row).
    pub fn codes(&self) -> &[u32] {
        &self.codes
    }

    /// Optional code-level validity bitmap (`1` = valid, `0` = null).
    pub fn nulls(&self) -> Option<&Bitmap> {
        self.nulls.as_ref()
    }

    /// Row count (`codes.len()`).
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType {
        self.values.data_type()
    }

    /// True iff `row < self.len()` and either code is null or `values.is_null(codes[row])`.
    pub fn is_null(&self, row: usize) -> bool {
        if row >= self.codes.len() {
            return false;
        }
        if self.nulls.as_ref().is_some_and(|bm| !bm.get(row)) {
            return true;
        }
        self.values.is_null(self.codes[row] as usize)
    }

    /// True iff `self.nulls.is_some() || (self.values.has_nulls() && self.codes.iter().any(|&c| self.values.is_null(c as usize)))`.
    /// Does not falsely report nulls when `values` holds an unreferenced null entry after slicing.
    pub fn has_nulls(&self) -> bool {
        self.nulls.is_some()
            || (self.values.has_nulls()
                && self.codes.iter().any(|&c| self.values.is_null(c as usize)))
    }

    /// Extracts row `row` as a [`ScalarValue`].
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError> {
        if row >= self.codes.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "row index out of bounds",
            });
        }
        if self.nulls.as_ref().is_some_and(|bm| !bm.get(row)) {
            return Ok(ScalarValue::Null(self.data_type()));
        }
        self.values.get_scalar(self.codes[row] as usize)
    }

    /// Slices `offset..offset + len` (shares `values` `Arc`, slices `codes` and `nulls`,
    /// and reserves the sliced column's full `memory_bytes()` in `pool`, rolling back on error).
    pub fn slice(
        &self,
        offset: usize,
        len: usize,
        pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        self.slice_with_accounting(offset, len, pool, true)
    }

    pub(crate) fn slice_with_accounting(
        &self,
        offset: usize,
        len: usize,
        mut pool: Option<&mut Reservation>,
        account_values: bool,
    ) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "dict column slice out of bounds",
            });
        }
        let before = pool.as_ref().map_or(0, |p| p.size());
        let values_bytes = if account_values {
            self.values.memory_bytes()
        } else {
            0
        };
        let codes_bytes = match checked_mul_bytes(len, 4, "dict slice codes overflow") {
            Ok(b) => b,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        let nulls_max_bytes = if self.nulls.is_some() {
            match checked_mul_bytes(len.div_ceil(64), 8, "dict slice nulls overflow") {
                Ok(b) => b,
                Err(err) => {
                    rollback_reservation(pool, before);
                    return Err(err);
                }
            }
        } else {
            0
        };
        let pre_bytes = match checked_add_bytes(values_bytes, codes_bytes, "dict slice overflow")
            .and_then(|s| checked_add_bytes(s, nulls_max_bytes, "dict slice overflow"))
        {
            Ok(b) => b,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        if let Some(res) = pool.as_deref_mut()
            && let Err(err) = res.try_grow(pre_bytes)
        {
            rollback_reservation(pool, before);
            return Err(err);
        }
        let codes = self.codes[offset..offset + len].to_vec();
        let nulls = match slice_optional_bitmap(self.nulls.as_ref(), offset, len) {
            Ok(n) => n,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        let out = Self {
            values: Arc::clone(&self.values),
            codes,
            nulls,
        };
        let target_bytes = if account_values {
            out.memory_bytes()
        } else {
            out.codes.len() * 4 + out.nulls.as_ref().map_or(0, Bitmap::memory_bytes)
        };
        reconcile_reservation(pool, before, target_bytes)?;
        Ok(out)
    }

    /// Deterministic logical heap bytes (`values.memory_bytes() + codes.len() * 4 + nulls`).
    pub fn memory_bytes(&self) -> usize {
        self.values.memory_bytes()
            + self.codes.len() * 4
            + self.nulls.as_ref().map_or(0, Bitmap::memory_bytes)
    }
}

/// Run-length encoded column (`run_ends.len() == values.len()`; when non-empty,
/// `run_ends[0] > 0` and `run_ends` is strictly increasing; logical length is `*run_ends.last().unwrap_or(&0)`).
#[derive(Clone, PartialEq)]
pub struct RleColumn {
    values: Arc<FlatColumn>,
    run_ends: Vec<u32>,
}

impl fmt::Debug for RleColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RleColumn")
            .field("len", &self.len())
            .field("run_count", &self.run_ends.len())
            .field("values", &self.values)
            .finish()
    }
}

fn empty_flat_memory_bytes(dt: &DataType) -> usize {
    match dt {
        DataType::Utf8 | DataType::Binary => 4,
        DataType::Links => 8,
        DataType::MapUtf8 | DataType::Events => 12,
        DataType::List(inner) => 4 + empty_flat_memory_bytes(inner),
        _ => 0,
    }
}

impl RleColumn {
    /// Validates `run_ends.len() == values.len()`, `run_ends[0] > 0` (if non-empty), and strict monotonicity.
    pub fn try_new(values: FlatColumn, run_ends: Vec<u32>) -> Result<Self, ColumnError> {
        Self::try_new_shared(Arc::new(values), run_ends)
    }

    /// Validates `run_ends.len() == values.len()`, `run_ends[0] > 0` (if non-empty), and strict monotonicity
    /// over a shared `Arc<FlatColumn>` run values table.
    pub fn try_new_shared(
        values: Arc<FlatColumn>,
        run_ends: Vec<u32>,
    ) -> Result<Self, ColumnError> {
        if run_ends.len() != values.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "rle run_ends length must match values length",
            });
        }
        if let Some(&first) = run_ends.first() {
            if first == 0 {
                return Err(ColumnError::InvalidBatch {
                    reason: "rle first run_end must be greater than 0",
                });
            }
            for w in run_ends.windows(2) {
                if w[0] >= w[1] {
                    return Err(ColumnError::InvalidBatch {
                        reason: "rle run_ends must be strictly increasing",
                    });
                }
            }
        }
        Ok(Self { values, run_ends })
    }

    /// One value per run (null runs encoded via `values.nulls()`).
    pub fn values(&self) -> &FlatColumn {
        &self.values
    }

    /// Shared `Arc` handle to the run values table.
    pub fn values_arc(&self) -> &Arc<FlatColumn> {
        &self.values
    }

    /// Exclusive end row index of each run.
    pub fn run_ends(&self) -> &[u32] {
        &self.run_ends
    }

    /// Logical row count.
    pub fn len(&self) -> usize {
        self.run_ends.last().copied().unwrap_or(0) as usize
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType {
        self.values.data_type()
    }

    /// True iff `row < self.len()` and its run value is null.
    pub fn is_null(&self, row: usize) -> bool {
        if row >= self.len() {
            return false;
        }
        let run_idx = self.run_ends.partition_point(|&end| (end as usize) <= row);
        self.values.is_null(run_idx)
    }

    /// True if any run is null (`values.has_nulls()`).
    pub fn has_nulls(&self) -> bool {
        self.values.has_nulls()
    }

    /// Extracts row `row` as a [`ScalarValue`] via binary search (`partition_point(|&end| (end as usize) <= row)`).
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError> {
        if row >= self.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "row index out of bounds",
            });
        }
        let run_idx = self.run_ends.partition_point(|&end| (end as usize) <= row);
        self.values.get_scalar(run_idx)
    }

    /// Slices `offset..offset + len`, trimming and rebasing `run_ends` to `0..len`
    /// and reserving the sliced column's full `memory_bytes()` in `pool` (rolling back on error).
    pub fn slice(
        &self,
        offset: usize,
        len: usize,
        mut pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "rle column slice out of bounds",
            });
        }
        let before = pool.as_ref().map_or(0, |p| p.size());
        if len == 0 {
            let empty_bytes = empty_flat_memory_bytes(&self.data_type());
            if let Some(res) = pool.as_deref_mut()
                && let Err(err) = res.try_grow(empty_bytes)
            {
                rollback_reservation(pool, before);
                return Err(err);
            }
            let empty = Self {
                values: Arc::new(FlatColumn::empty(&self.data_type())),
                run_ends: Vec::new(),
            };
            debug_assert_eq!(empty.memory_bytes(), empty_bytes);
            return Ok(empty);
        }
        let end_row = offset + len;
        let first_run = self
            .run_ends
            .partition_point(|&end| (end as usize) <= offset);
        let last_run = self
            .run_ends
            .partition_point(|&end| (end as usize) < end_row);
        let run_count = last_run - first_run + 1;
        let run_ends_bytes = match checked_mul_bytes(run_count, 4, "rle slice run_ends overflow") {
            Ok(b) => b,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        if let Some(res) = pool.as_deref_mut()
            && let Err(err) = res.try_grow(run_ends_bytes)
        {
            rollback_reservation(pool, before);
            return Err(err);
        }
        let new_values = match self.values.slice(first_run, run_count, pool.as_deref_mut()) {
            Ok(v) => v,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        let new_run_ends = self.run_ends[first_run..=last_run]
            .iter()
            .map(|&re| ((re as usize).min(end_row) - offset) as u32)
            .collect();
        let out = Self {
            values: Arc::new(new_values),
            run_ends: new_run_ends,
        };
        reconcile_reservation(pool, before, out.memory_bytes())?;
        Ok(out)
    }

    /// Deterministic logical heap bytes (`values.memory_bytes() + run_ends.len() * 4`).
    pub fn memory_bytes(&self) -> usize {
        self.values.memory_bytes() + self.run_ends.len() * 4
    }
}

/// One column in a [`crate::Batch`].
#[derive(Clone, PartialEq)]
pub enum Column {
    /// Flat uncompressed buffers.
    Flat(FlatColumn),
    /// Single value repeated `len` times.
    Const {
        /// Repeated value (or `ScalarValue::Null(dt)`).
        value: ScalarValue,
        /// Logical row count.
        len: usize,
    },
    /// Dictionary-encoded values.
    Dict(DictColumn),
    /// Run-length encoded values.
    Rle(RleColumn),
}

impl fmt::Debug for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flat(c) => f.debug_tuple("Flat").field(c).finish(),
            Self::Const { value, len } => f
                .debug_struct("Const")
                .field("value", value)
                .field("len", len)
                .finish(),
            Self::Dict(d) => f.debug_tuple("Dict").field(d).finish(),
            Self::Rle(r) => f.debug_tuple("Rle").field(r).finish(),
        }
    }
}

impl Column {
    /// Checked constructor for `Column::Const` (validates `len <= u32::MAX` and `value.validate(MAX_NESTING_DEPTH)`).
    pub fn try_new_const(value: ScalarValue, len: usize) -> Result<Self, ColumnError> {
        if len > u32::MAX as usize {
            return Err(ColumnError::Overflow {
                reason: "const column length exceeds u32::MAX",
            });
        }
        value.validate(MAX_NESTING_DEPTH)?;
        Ok(Self::Const { value, len })
    }

    pub(crate) fn validate_with_depth(&self, max_depth: usize) -> Result<(), ColumnError> {
        match self {
            Self::Flat(f) => f.validate_with_depth(max_depth),
            Self::Const { value, .. } => value.validate(max_depth),
            Self::Dict(d) => d.values.validate_with_depth(max_depth),
            Self::Rle(r) => r.values.validate_with_depth(max_depth),
        }
    }

    /// Logical row count.
    pub fn len(&self) -> usize {
        match self {
            Self::Flat(f) => f.len(),
            Self::Const { len, .. } => *len,
            Self::Dict(d) => d.len(),
            Self::Rle(r) => r.len(),
        }
    }

    /// True when `len() == 0`.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Concrete [`DataType`].
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Flat(f) => f.data_type(),
            Self::Const { value, .. } => value.data_type(),
            Self::Dict(d) => d.data_type(),
            Self::Rle(r) => r.data_type(),
        }
    }

    /// True iff `row < self.len()` and that row is null.
    pub fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Flat(f) => f.is_null(row),
            Self::Const { value, len } => row < *len && value.is_null(),
            Self::Dict(d) => d.is_null(row),
            Self::Rle(r) => r.is_null(row),
        }
    }

    /// True if at least one row in `0..len` is null (`Const { value: Null(_), len: 0 }` returns `false`).
    pub fn has_nulls(&self) -> bool {
        match self {
            Self::Flat(f) => f.has_nulls(),
            Self::Const { value, len } => *len > 0 && value.is_null(),
            Self::Dict(d) => d.has_nulls(),
            Self::Rle(r) => r.has_nulls(),
        }
    }

    /// Extracts row `row` as a [`ScalarValue`].
    pub fn get_scalar(&self, row: usize) -> Result<ScalarValue, ColumnError> {
        match self {
            Self::Flat(f) => f.get_scalar(row),
            Self::Const { value, len } => {
                if row >= *len {
                    return Err(ColumnError::InvalidBatch {
                        reason: "row index out of bounds",
                    });
                }
                Ok(value.clone())
            }
            Self::Dict(d) => d.get_scalar(row),
            Self::Rle(r) => r.get_scalar(row),
        }
    }

    /// Materializes `self` into a [`FlatColumn`], reserving exact output `memory_bytes()` from `pool`
    /// (and rolling back on error).
    pub fn materialize(
        &self,
        mut pool: Option<&mut Reservation>,
    ) -> Result<FlatColumn, ColumnError> {
        let before = pool.as_ref().map_or(0, |p| p.size());
        match self {
            Self::Flat(f) => {
                reconcile_reservation(pool.as_deref_mut(), before, f.memory_bytes())?;
                Ok(f.clone())
            }
            Self::Const { value, len } => {
                if let Err(err) = value.validate(MAX_NESTING_DEPTH) {
                    rollback_reservation(pool, before);
                    return Err(err);
                }
                let scratch_bytes =
                    match checked_mul_bytes(*len, 4, "const materialize scratch overflow") {
                        Ok(b) => b,
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    };
                reconcile_reservation(pool.as_deref_mut(), before, scratch_bytes)?;
                let one = match flat_from_scalar_one(value) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                let indices = vec![0u32; *len];
                let out = match one.take(&indices, pool.as_deref_mut()) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                drop(indices);
                reconcile_reservation(pool, before, out.memory_bytes())?;
                Ok(out)
            }
            Self::Dict(d) => {
                if let Some(code_nulls) = d.nulls() {
                    let scratch_bytes =
                        match checked_mul_bytes(d.len(), 4, "dict materialize scratch overflow") {
                            Ok(b) => b,
                            Err(err) => {
                                rollback_reservation(pool, before);
                                return Err(err);
                            }
                        };
                    reconcile_reservation(pool.as_deref_mut(), before, scratch_bytes)?;
                    let mut indices = Vec::with_capacity(d.len());
                    for (i, &code) in d.codes().iter().enumerate() {
                        if code_nulls.get(i) {
                            indices.push(code);
                        } else {
                            indices.push(0);
                        }
                    }
                    let out = match d.values().take_with_mask(
                        &indices,
                        Some(code_nulls),
                        pool.as_deref_mut(),
                    ) {
                        Ok(c) => c,
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    };
                    drop(indices);
                    reconcile_reservation(pool, before, out.memory_bytes())?;
                    Ok(out)
                } else {
                    d.values().take(d.codes(), pool)
                }
            }
            Self::Rle(r) => {
                let scratch_bytes =
                    match checked_mul_bytes(r.len(), 4, "rle materialize scratch overflow") {
                        Ok(b) => b,
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    };
                reconcile_reservation(pool.as_deref_mut(), before, scratch_bytes)?;
                let mut indices = Vec::with_capacity(r.len());
                let mut prev = 0u32;
                for (run_idx, &end) in r.run_ends().iter().enumerate() {
                    let count = (end - prev) as usize;
                    indices.resize(indices.len() + count, run_idx as u32);
                    prev = end;
                }
                let out = match r.values().take(&indices, pool.as_deref_mut()) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                drop(indices);
                reconcile_reservation(pool, before, out.memory_bytes())?;
                Ok(out)
            }
        }
    }

    /// Gathers `indices` into a [`FlatColumn`], validating every `idx < self.len()` (including for `Column::Const`),
    /// writing canonical zero/empty backing values at null slots, and reconciling `pool` to exact `result.memory_bytes()`.
    pub fn take(
        &self,
        indices: &[u32],
        mut pool: Option<&mut Reservation>,
    ) -> Result<FlatColumn, ColumnError> {
        let before = pool.as_ref().map_or(0, |p| p.size());
        match self {
            Self::Flat(f) => f.take(indices, pool),
            Self::Const { value, len } => {
                if let Err(err) = value.validate(MAX_NESTING_DEPTH) {
                    rollback_reservation(pool, before);
                    return Err(err);
                }
                for &idx in indices {
                    if (idx as usize) >= *len {
                        rollback_reservation(pool, before);
                        return Err(ColumnError::InvalidBatch {
                            reason: "take index out of bounds",
                        });
                    }
                }
                let scratch_bytes =
                    match checked_mul_bytes(indices.len(), 4, "const take scratch overflow") {
                        Ok(b) => b,
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    };
                reconcile_reservation(pool.as_deref_mut(), before, scratch_bytes)?;
                let one = match flat_from_scalar_one(value) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                let zero_indices = vec![0u32; indices.len()];
                let out = match one.take(&zero_indices, pool.as_deref_mut()) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                drop(zero_indices);
                reconcile_reservation(pool, before, out.memory_bytes())?;
                Ok(out)
            }
            Self::Dict(d) => {
                for &idx in indices {
                    if (idx as usize) >= d.len() {
                        rollback_reservation(pool, before);
                        return Err(ColumnError::InvalidBatch {
                            reason: "take index out of bounds",
                        });
                    }
                }
                let idx_bytes =
                    match checked_mul_bytes(indices.len(), 4, "dict take scratch overflow") {
                        Ok(b) => b,
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    };
                let mask_bytes = if d.nulls().is_some() {
                    indices.len().div_ceil(64) * 8
                } else {
                    0
                };
                let scratch_bytes =
                    match checked_add_bytes(idx_bytes, mask_bytes, "dict take scratch overflow") {
                        Ok(b) => b,
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    };
                reconcile_reservation(pool.as_deref_mut(), before, scratch_bytes)?;

                let mut dict_indices = Vec::with_capacity(indices.len());
                let out = if let Some(code_nulls) = d.nulls() {
                    let mut mask = Bitmap::new_ones(indices.len());
                    for (out_pos, &idx) in indices.iter().enumerate() {
                        let row = idx as usize;
                        if code_nulls.get(row) {
                            dict_indices.push(d.codes()[row]);
                        } else {
                            let _ = mask.set(out_pos, false);
                            dict_indices.push(0);
                        }
                    }
                    let res =
                        d.values()
                            .take_with_mask(&dict_indices, Some(&mask), pool.as_deref_mut());
                    drop(mask);
                    res
                } else {
                    for &idx in indices {
                        dict_indices.push(d.codes()[idx as usize]);
                    }
                    d.values().take(&dict_indices, pool.as_deref_mut())
                };
                drop(dict_indices);
                let out = match out {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                reconcile_reservation(pool, before, out.memory_bytes())?;
                Ok(out)
            }
            Self::Rle(r) => {
                for &idx in indices {
                    if (idx as usize) >= r.len() {
                        rollback_reservation(pool, before);
                        return Err(ColumnError::InvalidBatch {
                            reason: "take index out of bounds",
                        });
                    }
                }
                let scratch_bytes =
                    match checked_mul_bytes(indices.len(), 4, "rle take scratch overflow") {
                        Ok(b) => b,
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    };
                reconcile_reservation(pool.as_deref_mut(), before, scratch_bytes)?;

                let mut run_indices = Vec::with_capacity(indices.len());
                for &idx in indices {
                    let row = idx as usize;
                    let run_idx = r.run_ends().partition_point(|&end| (end as usize) <= row);
                    run_indices.push(run_idx as u32);
                }
                let out = match r.values().take(&run_indices, pool.as_deref_mut()) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                drop(run_indices);
                reconcile_reservation(pool, before, out.memory_bytes())?;
                Ok(out)
            }
        }
    }

    /// Slices `offset..offset + len` (`Const` is O(1)), reserving the sliced column's full
    /// `memory_bytes()` in `pool` when `Some` (rolling back on error).
    pub fn slice(
        &self,
        offset: usize,
        len: usize,
        mut pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        match self {
            Self::Flat(f) => Ok(Self::Flat(f.slice(offset, len, pool)?)),
            Self::Const {
                value,
                len: cur_len,
            } => {
                if offset.checked_add(len).is_none_or(|end| end > *cur_len) {
                    return Err(ColumnError::InvalidBatch {
                        reason: "const column slice out of bounds",
                    });
                }
                if let Some(res) = pool.as_deref_mut() {
                    res.try_grow(value.heap_bytes())?;
                }
                Ok(Self::Const {
                    value: value.clone(),
                    len,
                })
            }
            Self::Dict(d) => Ok(Self::Dict(d.slice(offset, len, pool)?)),
            Self::Rle(r) => Ok(Self::Rle(r.slice(offset, len, pool)?)),
        }
    }

    /// NaN-aware, encoding-independent semantic equality across `0..len`.
    pub fn semantic_eq(&self, other: &Self) -> bool {
        if self.data_type() != other.data_type() || self.len() != other.len() {
            return false;
        }
        match (self, other) {
            (Self::Flat(a), Self::Flat(b)) => a.semantic_eq(b),
            (Self::Const { value: a, len: la }, Self::Const { value: b, .. }) => {
                *la == 0 || a.semantic_eq(b)
            }
            _ => (0..self.len()).all(|row| {
                matches!(
                    (self.get_scalar(row), other.get_scalar(row)),
                    (Ok(a), Ok(b)) if a.semantic_eq(&b)
                )
            }),
        }
    }

    /// Deterministic logical heap bytes.
    pub fn memory_bytes(&self) -> usize {
        match self {
            Self::Flat(f) => f.memory_bytes(),
            Self::Const { value, .. } => value.heap_bytes(),
            Self::Dict(d) => d.memory_bytes(),
            Self::Rle(r) => r.memory_bytes(),
        }
    }
}

fn flat_from_scalar_one(value: &ScalarValue) -> Result<FlatColumn, ColumnError> {
    flat_from_scalar_slice(&value.data_type(), std::slice::from_ref(value))
}

fn flat_from_scalar_slice(
    data_type: &DataType,
    values: &[ScalarValue],
) -> Result<FlatColumn, ColumnError> {
    data_type.validate(MAX_NESTING_DEPTH)?;
    let len = values.len();
    let mut nulls = Bitmap::new_ones(len);
    for (i, v) in values.iter().enumerate() {
        if v.is_null() {
            if v.data_type() != *data_type {
                return Err(ColumnError::InvalidBatch {
                    reason: "null scalar data type mismatch",
                });
            }
            let _ = nulls.set(i, false);
        } else if v.data_type() != *data_type {
            return Err(ColumnError::InvalidBatch {
                reason: "scalar data type mismatch",
            });
        }
    }
    let nulls_opt = if matches!(data_type, DataType::Null) || nulls.all_ones() {
        None
    } else {
        Some(nulls)
    };

    let data = match data_type {
        DataType::Null => FlatData::Null(len),
        DataType::Bool => {
            let mut bm = Bitmap::new_zeroed(len);
            for (i, v) in values.iter().enumerate() {
                if let ScalarValue::Bool(b) = v
                    && *b
                {
                    let _ = bm.set(i, true);
                }
            }
            FlatData::Bool(bm)
        }
        DataType::I64 => FlatData::I64(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::I64(x) => *x,
                    _ => 0,
                })
                .collect(),
        ),
        DataType::F64 => FlatData::F64(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::F64(x) => *x,
                    _ => 0.0,
                })
                .collect(),
        ),
        DataType::Decimal128 { precision, scale } => FlatData::Decimal128 {
            values: values
                .iter()
                .map(|v| match v {
                    ScalarValue::Decimal128 { value, .. } => *value,
                    _ => 0,
                })
                .collect(),
            precision: *precision,
            scale: *scale,
        },
        DataType::TimestampNs { utc } => FlatData::TimestampNs {
            values: values
                .iter()
                .map(|v| match v {
                    ScalarValue::TimestampNs { value, .. } => *value,
                    _ => 0,
                })
                .collect(),
            utc: *utc,
        },
        DataType::Date => FlatData::Date(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::Date(x) => *x,
                    _ => 0,
                })
                .collect(),
        ),
        DataType::Interval => FlatData::Interval(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::Interval(x) => *x,
                    _ => Interval::default(),
                })
                .collect(),
        ),
        DataType::Utf8 => {
            FlatData::Utf8(Utf8Data::try_from_slices(values.iter().map(|v| match v {
                ScalarValue::Utf8(s) => s.as_str(),
                _ => "",
            }))?)
        }
        DataType::Binary => FlatData::Binary(BinaryData::try_from_slices(values.iter().map(
            |v| match v {
                ScalarValue::Binary(b) => b.as_slice(),
                _ => &[],
            },
        ))?),
        DataType::FixedBytes8 => FlatData::FixedBytes8(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::FixedBytes8(b) => *b,
                    _ => [0u8; 8],
                })
                .collect(),
        ),
        DataType::FixedBytes16 => FlatData::FixedBytes16(
            values
                .iter()
                .map(|v| match v {
                    ScalarValue::FixedBytes16(b) => *b,
                    _ => [0u8; 16],
                })
                .collect(),
        ),
        DataType::List(inner) => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0);
            let mut child_scalars = Vec::new();
            for v in values {
                if let ScalarValue::List { values: items, .. } = v {
                    child_scalars.extend(items.iter().cloned());
                }
                let end = checked_u32(child_scalars.len(), "list scalar length exceeds u32::MAX")?;
                offsets.push(end);
            }
            let child_col = flat_from_scalar_slice(inner, &child_scalars)?;
            FlatData::List {
                item_type: (**inner).clone(),
                offsets,
                values: Box::new(Column::Flat(child_col)),
            }
        }
        DataType::MapUtf8 => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0);
            let mut keys = Vec::new();
            let mut vals = Vec::new();
            let mut val_valid = Vec::new();
            for v in values {
                if let ScalarValue::MapUtf8(entries) = v {
                    for (k, mv) in entries {
                        keys.push(k.as_str());
                        vals.push(mv.as_deref().unwrap_or(""));
                        val_valid.push(mv.is_some());
                    }
                }
                let end = checked_u32(keys.len(), "map scalar entries exceed u32::MAX")?;
                offsets.push(end);
            }
            let keys_data = Utf8Data::try_from_slices(keys)?;
            let vals_data = Utf8Data::try_from_slices(vals)?;
            let val_nulls = Bitmap::from_bools(&val_valid);
            let map = MapData::try_new(offsets, keys_data, vals_data, Some(val_nulls))?;
            FlatData::MapUtf8(Box::new(map))
        }
        DataType::Events => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0);
            let mut times = Vec::new();
            let mut names = Vec::new();
            let mut attrs = Vec::new();
            let mut attr_valid = Vec::new();
            for v in values {
                if let ScalarValue::Events(evs) = v {
                    for ev in evs {
                        times.push(ev.time);
                        names.push(ev.name.as_str());
                        attrs.push(ev.attributes.as_deref().unwrap_or(""));
                        attr_valid.push(ev.attributes.is_some());
                    }
                }
                let end = checked_u32(times.len(), "events scalar count exceeds u32::MAX")?;
                offsets.push(end);
            }
            let names_data = Utf8Data::try_from_slices(names)?;
            let attrs_data = Utf8Data::try_from_slices(attrs)?;
            let attr_nulls = Bitmap::from_bools(&attr_valid);
            let events =
                EventsData::try_new(offsets, times, names_data, attrs_data, Some(attr_nulls))?;
            FlatData::Events(Box::new(events))
        }
        DataType::Links => {
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0);
            let mut trace_ids = Vec::new();
            let mut span_ids = Vec::new();
            let mut attrs = Vec::new();
            let mut attr_valid = Vec::new();
            for v in values {
                if let ScalarValue::Links(lks) = v {
                    for lk in lks {
                        trace_ids.push(lk.trace_id);
                        span_ids.push(lk.span_id);
                        attrs.push(lk.attributes.as_deref().unwrap_or(""));
                        attr_valid.push(lk.attributes.is_some());
                    }
                }
                let end = checked_u32(trace_ids.len(), "links scalar count exceeds u32::MAX")?;
                offsets.push(end);
            }
            let attrs_data = Utf8Data::try_from_slices(attrs)?;
            let attr_nulls = Bitmap::from_bools(&attr_valid);
            let links =
                LinksData::try_new(offsets, trace_ids, span_ids, attrs_data, Some(attr_nulls))?;
            FlatData::Links(Box::new(links))
        }
    };

    FlatColumn::try_new(data, nulls_opt)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{Batch, Field, GlobalPool, MemoryLimitKind, Schema, Selection};

    use super::*;

    fn sample_scalars_for_type(dt: &DataType) -> Vec<ScalarValue> {
        match dt {
            DataType::Null => vec![
                ScalarValue::Null(DataType::Null),
                ScalarValue::Null(DataType::Null),
                ScalarValue::Null(DataType::Null),
            ],
            DataType::Bool => vec![
                ScalarValue::Bool(true),
                ScalarValue::Null(DataType::Bool),
                ScalarValue::Bool(false),
            ],
            DataType::I64 => vec![
                ScalarValue::I64(-42),
                ScalarValue::Null(DataType::I64),
                ScalarValue::I64(99),
            ],
            DataType::F64 => vec![
                ScalarValue::F64(f64::NAN),
                ScalarValue::Null(DataType::F64),
                ScalarValue::F64(3.25),
            ],
            DataType::Decimal128 { precision, scale } => vec![
                ScalarValue::Decimal128 {
                    value: 123_456,
                    precision: *precision,
                    scale: *scale,
                },
                ScalarValue::Null(dt.clone()),
                ScalarValue::Decimal128 {
                    value: -789,
                    precision: *precision,
                    scale: *scale,
                },
            ],
            DataType::TimestampNs { utc } => vec![
                ScalarValue::TimestampNs {
                    value: 1_700_000_000,
                    utc: *utc,
                },
                ScalarValue::Null(dt.clone()),
                ScalarValue::TimestampNs {
                    value: 0,
                    utc: *utc,
                },
            ],
            DataType::Date => vec![
                ScalarValue::Date(19_000),
                ScalarValue::Null(DataType::Date),
                ScalarValue::Date(-5),
            ],
            DataType::Interval => vec![
                ScalarValue::Interval(Interval {
                    months: 1,
                    days: -2,
                    nanos: 300,
                }),
                ScalarValue::Null(DataType::Interval),
                ScalarValue::Interval(Interval::default()),
            ],
            DataType::Utf8 => vec![
                ScalarValue::Utf8("hello".into()),
                ScalarValue::Null(DataType::Utf8),
                ScalarValue::Utf8(String::new()),
            ],
            DataType::Binary => vec![
                ScalarValue::Binary(vec![1, 2, 3]),
                ScalarValue::Null(DataType::Binary),
                ScalarValue::Binary(Vec::new()),
            ],
            DataType::FixedBytes8 => vec![
                ScalarValue::FixedBytes8([1; 8]),
                ScalarValue::Null(DataType::FixedBytes8),
                ScalarValue::FixedBytes8([2; 8]),
            ],
            DataType::FixedBytes16 => vec![
                ScalarValue::FixedBytes16([3; 16]),
                ScalarValue::Null(DataType::FixedBytes16),
                ScalarValue::FixedBytes16([4; 16]),
            ],
            DataType::List(inner) => {
                if matches!(inner.as_ref(), DataType::List(_)) {
                    vec![
                        ScalarValue::List {
                            item_type: (**inner).clone(),
                            values: vec![
                                ScalarValue::List {
                                    item_type: DataType::Utf8,
                                    values: vec![
                                        ScalarValue::Utf8("a".into()),
                                        ScalarValue::Null(DataType::Utf8),
                                    ],
                                },
                                ScalarValue::Null((**inner).clone()),
                            ],
                        },
                        ScalarValue::Null(dt.clone()),
                        ScalarValue::List {
                            item_type: (**inner).clone(),
                            values: vec![],
                        },
                    ]
                } else {
                    vec![
                        ScalarValue::List {
                            item_type: (**inner).clone(),
                            values: vec![ScalarValue::Utf8("x".into())],
                        },
                        ScalarValue::Null(dt.clone()),
                        ScalarValue::List {
                            item_type: (**inner).clone(),
                            values: vec![],
                        },
                    ]
                }
            }
            DataType::MapUtf8 => vec![
                ScalarValue::MapUtf8(vec![("k1".into(), Some("v1".into())), ("k2".into(), None)]),
                ScalarValue::Null(DataType::MapUtf8),
                ScalarValue::MapUtf8(vec![]),
            ],
            DataType::Events => vec![
                ScalarValue::Events(vec![
                    EventItem {
                        time: 100,
                        name: "ev1".into(),
                        attributes: Some("{\"a\":1}".into()),
                    },
                    EventItem {
                        time: 200,
                        name: "ev2".into(),
                        attributes: None,
                    },
                ]),
                ScalarValue::Null(DataType::Events),
                ScalarValue::Events(vec![]),
            ],
            DataType::Links => vec![
                ScalarValue::Links(vec![
                    LinkItem {
                        trace_id: [9; 16],
                        span_id: [8; 8],
                        attributes: Some("{\"l\":1}".into()),
                    },
                    LinkItem {
                        trace_id: [7; 16],
                        span_id: [6; 8],
                        attributes: None,
                    },
                ]),
                ScalarValue::Null(DataType::Links),
                ScalarValue::Links(vec![]),
            ],
        }
    }

    #[test]
    fn all_types_materialize_and_take_match_scalars() {
        let types = vec![
            DataType::Null,
            DataType::Bool,
            DataType::I64,
            DataType::F64,
            DataType::cost_decimal(),
            DataType::timestamp_utc(),
            DataType::Date,
            DataType::Interval,
            DataType::Utf8,
            DataType::Binary,
            DataType::FixedBytes8,
            DataType::FixedBytes16,
            DataType::List(Box::new(DataType::Utf8)),
            DataType::List(Box::new(DataType::List(Box::new(DataType::Utf8)))),
            DataType::MapUtf8,
            DataType::Events,
            DataType::Links,
        ];

        for dt in &types {
            let scalars = sample_scalars_for_type(dt);
            let flat = flat_from_scalar_slice(dt, &scalars).expect("flat_from_scalar_slice");
            assert_eq!(flat.len(), scalars.len());
            for (i, expected) in scalars.iter().enumerate() {
                assert!(flat.get_scalar(i).unwrap().semantic_eq(expected));
            }

            let flat_col = Column::Flat(flat.clone());
            assert!(flat_col.materialize(None).unwrap().semantic_eq(&flat));
            let taken = flat_col.take(&[2, 0, 1, 2], None).unwrap();
            let expected_taken = flat_from_scalar_slice(
                dt,
                &[
                    scalars[2].clone(),
                    scalars[0].clone(),
                    scalars[1].clone(),
                    scalars[2].clone(),
                ],
            )
            .unwrap();
            assert!(taken.semantic_eq(&expected_taken));

            let sliced = flat_col.slice(1, 2, None).unwrap();
            assert!(sliced.get_scalar(0).unwrap().semantic_eq(&scalars[1]));
            assert!(sliced.get_scalar(1).unwrap().semantic_eq(&scalars[2]));

            // Const
            let const_col = Column::try_new_const(scalars[0].clone(), 3).unwrap();
            let const_mat = const_col.materialize(None).unwrap();
            for i in 0..3 {
                assert!(const_mat.get_scalar(i).unwrap().semantic_eq(&scalars[0]));
            }
            let const_taken = const_col.take(&[2, 0], None).unwrap();
            assert_eq!(const_taken.len(), 2);
            assert!(const_taken.get_scalar(0).unwrap().semantic_eq(&scalars[0]));
            let const_sliced = const_col.slice(1, 2, None).unwrap();
            assert_eq!(const_sliced.len(), 2);
            assert!(const_sliced.get_scalar(1).unwrap().semantic_eq(&scalars[0]));

            // Dict (with code-level null and dict-level null)
            let dict = DictColumn::try_new(
                flat.clone(),
                vec![2, 999, 0, 1],
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap();
            let dict_col = Column::Dict(dict);
            let dict_mat = dict_col.materialize(None).unwrap();
            assert!(dict_mat.get_scalar(0).unwrap().semantic_eq(&scalars[2]));
            assert!(dict_mat.get_scalar(1).unwrap().is_null());
            assert!(dict_mat.get_scalar(2).unwrap().semantic_eq(&scalars[0]));
            assert!(dict_mat.get_scalar(3).unwrap().semantic_eq(&scalars[1]));
            let dict_taken = dict_col.take(&[3, 1, 0], None).unwrap();
            assert!(dict_taken.get_scalar(0).unwrap().semantic_eq(&scalars[1]));
            assert!(dict_taken.get_scalar(1).unwrap().is_null());
            assert!(dict_taken.get_scalar(2).unwrap().semantic_eq(&scalars[2]));
            let dict_sliced = dict_col.slice(2, 2, None).unwrap();
            assert!(dict_sliced.semantic_eq(&Column::Flat(flat.slice(0, 2, None).unwrap())));
            assert!(dict_sliced.get_scalar(0).unwrap().semantic_eq(&scalars[0]));
            if !matches!(dt, DataType::Null) {
                assert!(!dict_col.slice(0, 1, None).unwrap().has_nulls());
            }

            // Rle
            let rle = RleColumn::try_new(flat.clone(), vec![2, 3, 6]).unwrap();
            let rle_col = Column::Rle(rle);
            let rle_mat = rle_col.materialize(None).unwrap();
            assert_eq!(rle_mat.len(), 6);
            assert!(rle_mat.get_scalar(0).unwrap().semantic_eq(&scalars[0]));
            assert!(rle_mat.get_scalar(1).unwrap().semantic_eq(&scalars[0]));
            assert!(rle_mat.get_scalar(2).unwrap().semantic_eq(&scalars[1]));
            assert!(rle_mat.get_scalar(5).unwrap().semantic_eq(&scalars[2]));
            let rle_taken = rle_col.take(&[5, 2, 0], None).unwrap();
            assert!(rle_taken.get_scalar(0).unwrap().semantic_eq(&scalars[2]));
            assert!(rle_taken.get_scalar(1).unwrap().semantic_eq(&scalars[1]));
            assert!(rle_taken.get_scalar(2).unwrap().semantic_eq(&scalars[0]));
            let rle_sliced = rle_col.slice(1, 3, None).unwrap();
            assert_eq!(rle_sliced.len(), 3);
            assert!(rle_sliced.get_scalar(0).unwrap().semantic_eq(&scalars[0]));
            assert!(rle_sliced.get_scalar(1).unwrap().semantic_eq(&scalars[1]));
            assert!(rle_sliced.get_scalar(2).unwrap().semantic_eq(&scalars[2]));
        }
    }

    #[test]
    fn utf8_non_char_boundary_offset_rejected() {
        let bytes = "é".as_bytes().to_vec();
        assert_eq!(bytes, vec![0xc3, 0xa9]);
        let err = Utf8Data::try_new(vec![0, 1, 2], bytes).unwrap_err();
        assert_eq!(err, ColumnError::InvalidUtf8);
    }

    #[test]
    fn invalid_invariants_rejected() {
        assert!(Utf8Data::try_new(vec![], vec![]).is_err());
        assert!(Utf8Data::try_new(vec![1], vec![b'a']).is_err());
        assert!(Utf8Data::try_new(vec![0, 2, 1], vec![b'a', b'b']).is_err());
        assert!(Utf8Data::try_new(vec![0, 1], vec![b'a', b'b']).is_err());
        assert_eq!(
            Utf8Data::try_new(vec![0, 1], vec![0xff]).unwrap_err(),
            ColumnError::InvalidUtf8
        );

        assert!(BinaryData::try_new(vec![], vec![]).is_err());
        assert!(BinaryData::try_new(vec![1], vec![1]).is_err());
        assert!(BinaryData::try_new(vec![0, 2, 1], vec![1, 2]).is_err());
        assert!(BinaryData::try_new(vec![0, 1], vec![1, 2]).is_err());

        // take null_mask mismatch and out-of-bounds vs null bit skipping
        let u = Utf8Data::try_from_slices(["a", "b"]).unwrap();
        assert!(u.take(&[0], Some(&Bitmap::new_ones(2))).is_err());
        let masked = u
            .take(&[999, 1], Some(&Bitmap::from_bools(&[false, true])))
            .unwrap();
        assert_eq!(masked.get(0), Some(""));
        assert_eq!(masked.get(1), Some("b"));
        assert!(u.take(&[999], None).is_err());

        let flat_i64 = FlatColumn::try_new(FlatData::I64(vec![1, 2]), None).unwrap();
        assert!(DictColumn::try_new(flat_i64.clone(), vec![2], None).is_err());
        assert!(DictColumn::try_new(flat_i64.clone(), vec![0], Some(Bitmap::new_ones(2))).is_err());

        assert!(RleColumn::try_new(flat_i64.clone(), vec![0, 5]).is_err());
        assert!(RleColumn::try_new(flat_i64.clone(), vec![3, 3]).is_err());
        assert!(RleColumn::try_new(flat_i64, vec![1]).is_err());

        assert!(FlatColumn::try_new(FlatData::I64(vec![1]), Some(Bitmap::new_ones(2))).is_err());
        assert!(
            Column::try_new_const(
                ScalarValue::Decimal128 {
                    value: 1,
                    precision: 0,
                    scale: 0
                },
                3
            )
            .is_err()
        );
        assert!(
            Column::try_new_const(
                ScalarValue::List {
                    item_type: DataType::I64,
                    values: vec![ScalarValue::Utf8("bad".into())]
                },
                2
            )
            .is_err()
        );
    }

    #[test]
    fn const_null_zero_len_allowed_in_non_nullable_batch() {
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let col = Column::try_new_const(ScalarValue::Null(DataType::I64), 0).unwrap();
        assert!(!col.has_nulls());
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 0).unwrap();
        assert_eq!(batch.num_rows(), 0);

        let col_nonempty = Column::try_new_const(ScalarValue::Null(DataType::I64), 1).unwrap();
        assert!(col_nonempty.has_nulls());
        assert!(Batch::try_new_all(schema, vec![col_nonempty], 1).is_err());
    }

    #[test]
    fn debug_never_leaks_payloads() {
        let secret = "SECRET_SENTINEL_VALUE";
        let utf8 = Utf8Data::try_from_slices([secret]).unwrap();
        let bin = BinaryData::try_from_slices([secret.as_bytes()]).unwrap();
        let map = MapData::try_new(
            vec![0, 1],
            Utf8Data::try_from_slices([secret]).unwrap(),
            Utf8Data::try_from_slices([secret]).unwrap(),
            None,
        )
        .unwrap();
        let events = EventsData::try_new(
            vec![0, 1],
            vec![123],
            Utf8Data::try_from_slices([secret]).unwrap(),
            Utf8Data::try_from_slices([secret]).unwrap(),
            None,
        )
        .unwrap();
        let links = LinksData::try_new(
            vec![0, 1],
            vec![[1; 16]],
            vec![[2; 8]],
            Utf8Data::try_from_slices([secret]).unwrap(),
            None,
        )
        .unwrap();
        let col = Column::Flat(FlatColumn::try_new(FlatData::Utf8(utf8.clone()), None).unwrap());
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let batch = Batch::try_new_all(schema, vec![col.clone()], 1).unwrap();

        for dbg in [
            format!("{utf8:?}"),
            format!("{bin:?}"),
            format!("{map:?}"),
            format!("{events:?}"),
            format!("{links:?}"),
            format!("{col:?}"),
            format!("{batch:?}"),
        ] {
            assert!(!dbg.contains(secret), "leaked secret in {dbg}");
        }
    }

    #[test]
    fn reservation_exact_accounting_and_rollback_on_error() {
        let scope = GlobalPool::standalone(4096);
        let mut res = scope.register("test", true);
        res.try_grow(100).unwrap();
        let before = res.size();

        // 1. materialize: Const(List) where ScalarValue heap > 1-row FlatData and Dict/Rle
        let list_scalar = ScalarValue::List {
            item_type: DataType::Utf8,
            values: vec![
                ScalarValue::Utf8("hello".into()),
                ScalarValue::Utf8("world".into()),
            ],
        };
        let const_col = Column::try_new_const(list_scalar, 3).unwrap();
        let mat = const_col.materialize(Some(&mut res)).unwrap();
        assert_eq!(res.size(), before + mat.memory_bytes());
        res.shrink(mat.memory_bytes());
        assert_eq!(res.size(), before);

        // 2. take including empty output (Utf8Data empty still has 4 bytes for offsets=[0])
        let u_flat = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
            None,
        )
        .unwrap();
        let u_col = Column::Flat(u_flat);
        let empty_taken = u_col.take(&[], Some(&mut res)).unwrap();
        assert_eq!(empty_taken.memory_bytes(), 4);
        assert_eq!(res.size(), before + 4);
        res.shrink(4);
        assert_eq!(res.size(), before);

        let taken = u_col.take(&[1, 0, 1], Some(&mut res)).unwrap();
        assert_eq!(res.size(), before + taken.memory_bytes());
        res.shrink(taken.memory_bytes());
        assert_eq!(res.size(), before);

        // 3. Rollback on error for materialize and take under tight limit
        let tight_scope = GlobalPool::standalone(50);
        let mut tight_res = tight_scope.register("tight", true);
        tight_res.try_grow(40).unwrap();
        let err = u_col.materialize(Some(&mut tight_res)).unwrap_err();
        assert!(matches!(
            err,
            ColumnError::MemoryLimit {
                kind: MemoryLimitKind::QueryCap | MemoryLimitKind::SpillShare,
                ..
            }
        ));
        assert_eq!(tight_res.size(), 40);

        let err2 = u_col.take(&[0, 1, 0, 1], Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err2, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 40);

        // 4. compact & concat exact accounting + rollback
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::I64, false),
            Field::new("b", DataType::Utf8, true),
        ])
        .unwrap();
        let batch3 = Batch::try_new(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["aa", "bb", "cc"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            3,
            Selection::Mask(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();

        let compacted = batch3.compact(Some(&mut res)).unwrap();
        assert_eq!(res.size(), before + compacted.memory_bytes());
        res.shrink(compacted.memory_bytes());
        assert_eq!(res.size(), before);

        // compact rollback when col 0 fits but col 1 overflows
        let tight_compact_scope = GlobalPool::standalone(60);
        let mut tc_res = tight_compact_scope.register("tc", true);
        tc_res.try_grow(40).unwrap(); // 20 bytes left: col0 (16 bytes) fits, col1 (4+4+8+4=20 bytes) overflows!
        let err3 = batch3.compact(Some(&mut tc_res)).unwrap_err();
        assert!(matches!(err3, ColumnError::MemoryLimit { .. }));
        assert_eq!(tc_res.size(), 40);

        // concat empty batches still reserves empty batch offset bytes (4 for Utf8)
        let empty_concat = Batch::concat(Arc::clone(&schema), &[], Some(&mut res)).unwrap();
        assert_eq!(empty_concat.memory_bytes(), 4);
        assert_eq!(res.size(), before + 4);
        res.shrink(4);
        assert_eq!(res.size(), before);

        // concat non-empty
        let merged =
            Batch::concat(Arc::clone(&schema), &[&batch3, &compacted], Some(&mut res)).unwrap();
        assert_eq!(res.size(), before + merged.memory_bytes());
        res.shrink(merged.memory_bytes());
        assert_eq!(res.size(), before);

        // concat rollback on error
        let err4 = Batch::concat(
            Arc::clone(&schema),
            &[&batch3, &compacted],
            Some(&mut tc_res),
        )
        .unwrap_err();
        assert!(matches!(err4, ColumnError::MemoryLimit { .. }));
        assert_eq!(tc_res.size(), 40);
    }

    #[test]
    fn const_and_rle_huge_len_reject_before_scratch_allocation() {
        let tiny_scope = GlobalPool::standalone(1024);
        let mut tiny_res = tiny_scope.register("tiny", true);

        let huge_const = Column::Const {
            value: ScalarValue::I64(1),
            len: 10_000_000,
        };
        let err = huge_const.materialize(Some(&mut tiny_res)).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(tiny_res.size(), 0);

        let huge_rle = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap(),
                vec![10_000_000],
            )
            .unwrap(),
        );
        let err_rle = huge_rle.materialize(Some(&mut tiny_res)).unwrap_err();
        assert!(matches!(err_rle, ColumnError::MemoryLimit { .. }));
        assert_eq!(tiny_res.size(), 0);

        // Const::take also rejects before allocating scratch indices when pool cannot hold scratch + output
        let tiny_take_scope = GlobalPool::standalone(16);
        let mut tiny_take_res = tiny_take_scope.register("tiny_take", true);
        let idx = [0u32, 1, 2, 3, 4];
        let err_take = huge_const.take(&idx, Some(&mut tiny_take_res)).unwrap_err();
        assert!(matches!(err_take, ColumnError::MemoryLimit { .. }));
        assert_eq!(tiny_take_res.size(), 0);
    }

    #[test]
    fn variable_width_take_and_concat_respect_tight_reservation_and_reject_mismatched_parts() {
        let map_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::MapUtf8(Box::new(
                    MapData::try_new(
                        vec![0, 2],
                        Utf8Data::try_from_slices(["key1", "key2"]).unwrap(),
                        Utf8Data::try_from_slices(["val1", ""]).unwrap(),
                        Some(Bitmap::from_bools(&[true, false])),
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        );
        let ev_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(
                        vec![0, 1],
                        vec![100],
                        Utf8Data::try_from_slices(["evt"]).unwrap(),
                        Utf8Data::try_from_slices(["{\"a\":1}"]).unwrap(),
                        None,
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        );
        let lk_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(
                        vec![0, 1],
                        vec![[1u8; 16]],
                        vec![[2u8; 8]],
                        Utf8Data::try_from_slices(["attr"]).unwrap(),
                        None,
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        );
        let list_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 2],
                    values: Box::new(Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(["hello", "world"]).unwrap()),
                            None,
                        )
                        .unwrap(),
                    )),
                },
                None,
            )
            .unwrap(),
        );

        let ample_scope = GlobalPool::standalone(4096);
        let mut ample_res = ample_scope.register("ample", true);
        for col in [&map_col, &ev_col, &lk_col, &list_col] {
            let taken = col.take(&[0, 0], Some(&mut ample_res)).unwrap();
            assert_eq!(ample_res.size(), taken.memory_bytes());
            ample_res.free();

            let tiny_scope = GlobalPool::standalone(taken.memory_bytes() - 1);
            let mut tiny_res = tiny_scope.register("tiny", true);
            let err = col.take(&[0, 0], Some(&mut tiny_res)).unwrap_err();
            assert!(matches!(err, ColumnError::MemoryLimit { .. }));
            assert_eq!(tiny_res.size(), 0);
        }

        // Mismatched FlatData variants in FlatColumn::concat return InvalidBatch
        let i64_flat = FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap();
        let f64_flat = FlatColumn::try_new(FlatData::F64(vec![1.0]), None).unwrap();
        let err_mismatch =
            FlatColumn::concat(&DataType::I64, &[&i64_flat, &f64_flat], 2).unwrap_err();
        assert!(matches!(err_mismatch, ColumnError::InvalidBatch { .. }));
    }

    #[test]
    fn dict_slice_shares_values_arc_and_reserves_memory_bytes() {
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices(["alpha_long_entry", "beta_long_entry"]).unwrap(),
            ),
            None,
        )
        .unwrap();
        let dict = DictColumn::try_new(
            dict_vals,
            vec![0, 1, 0, 1],
            Some(Bitmap::from_bools(&[true, false, true, true])),
        )
        .unwrap();

        let scope = GlobalPool::standalone(4096);
        let mut res = scope.register("dict_slice", true);
        let sliced = dict.slice(1, 2, Some(&mut res)).unwrap();
        assert!(Arc::ptr_eq(dict.values_arc(), sliced.values_arc()));
        assert_eq!(res.size(), sliced.memory_bytes());
        res.shrink(sliced.memory_bytes());
        assert_eq!(res.size(), 0);

        // Tight pool smaller than sliced.memory_bytes() rejects and rolls back cleanly.
        let tight = GlobalPool::standalone(sliced.memory_bytes() - 1);
        let mut tight_res = tight.register("tight", true);
        let err = dict.slice(1, 2, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 0);
    }

    #[test]
    fn rle_slice_zero_len_reserves_empty_memory_bytes() {
        let map_col = FlatColumn::try_new(
            FlatData::MapUtf8(Box::new(
                MapData::try_new(
                    vec![0, 1],
                    Utf8Data::try_from_slices(["k"]).unwrap(),
                    Utf8Data::try_from_slices(["v"]).unwrap(),
                    None,
                )
                .unwrap(),
            )),
            None,
        )
        .unwrap();
        let rle = RleColumn::try_new(map_col, vec![3]).unwrap();

        let scope = GlobalPool::standalone(4096);
        let mut res = scope.register("rle_empty", true);
        let empty = rle.slice(1, 0, Some(&mut res)).unwrap();
        assert_eq!(empty.len(), 0);
        assert!(empty.memory_bytes() > 0);
        assert_eq!(res.size(), empty.memory_bytes());

        let tight = GlobalPool::standalone(empty.memory_bytes() - 1);
        let mut tight_res = tight.register("tight_rle_empty", true);
        let err = rle.slice(1, 0, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 0);
    }

    #[test]
    fn const_rejects_len_exceeding_u32_max() {
        assert!(Column::try_new_const(ScalarValue::I64(1), u32::MAX as usize).is_ok());
        let err = Column::try_new_const(ScalarValue::I64(1), u32::MAX as usize + 1).unwrap_err();
        assert!(matches!(err, ColumnError::Overflow { .. }));
    }

    #[test]
    fn utf8_data_try_new_ascii_fast_path_and_non_ascii_boundary_checks() {
        // Pure ASCII valid
        let ascii = Utf8Data::try_new(vec![0, 5, 5, 11], b"hello world".to_vec()).unwrap();
        assert_eq!(ascii.len(), 3);
        assert_eq!(ascii.get(0), Some("hello"));
        assert_eq!(ascii.get(1), Some(""));
        assert_eq!(ascii.get(2), Some(" world"));

        // Pure ASCII non-monotonic offsets rejected
        assert!(matches!(
            Utf8Data::try_new(vec![0, 5, 4, 11], b"hello world".to_vec()),
            Err(ColumnError::InvalidBatch {
                reason: "offsets must be monotonic",
            })
        ));

        // Pure ASCII mismatched end offset rejected
        assert!(matches!(
            Utf8Data::try_new(vec![0, 5, 10], b"hello world".to_vec()),
            Err(ColumnError::InvalidBatch {
                reason: "last offset does not match buffer length",
            })
        ));

        // Valid multi-byte UTF-8 accepted ("é" is 2 bytes, "🦀" is 4 bytes)
        let mb = Utf8Data::try_new(vec![0, 2, 6], "é🦀".as_bytes().to_vec()).unwrap();
        assert_eq!(mb.get(0), Some("é"));
        assert_eq!(mb.get(1), Some("🦀"));

        // Invalid UTF-8 rejected
        assert!(matches!(
            Utf8Data::try_new(vec![0, 2], vec![0xFF, 0xFE]),
            Err(ColumnError::InvalidUtf8)
        ));

        // Offset splitting a multi-byte char rejected
        assert!(matches!(
            Utf8Data::try_new(vec![0, 1, 6], "é🦀".as_bytes().to_vec()),
            Err(ColumnError::InvalidUtf8)
        ));
    }
}
