//! Columnar storage buffers and encodings (`Flat`, `Const`, `Dict`, `Rle`).

use std::cmp::Ordering;
use std::fmt;

use crate::{
    Bitmap, ColumnError, DataType, EventItem, Interval, LinkItem, MAX_NESTING_DEPTH, Reservation,
    ScalarValue,
};

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
        let s = std::str::from_utf8(&bytes).map_err(|_| ColumnError::InvalidUtf8)?;
        for &o in &offsets {
            if !s.is_char_boundary(o as usize) {
                return Err(ColumnError::InvalidUtf8);
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

    /// Slices `offset..offset + len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "flat column slice out of bounds",
            });
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
            FlatData::List {
                item_type,
                offsets,
                values,
            } => {
                let base = offsets[offset];
                let child_start = base as usize;
                let child_end = offsets[offset + len] as usize;
                let new_offsets = offsets[offset..=offset + len]
                    .iter()
                    .map(|&o| o - base)
                    .collect();
                let new_values = values.slice(child_start, child_end - child_start)?;
                FlatData::List {
                    item_type: item_type.clone(),
                    offsets: new_offsets,
                    values: Box::new(new_values),
                }
            }
            FlatData::MapUtf8(m) => FlatData::MapUtf8(Box::new(m.slice(offset, len)?)),
            FlatData::Events(e) => FlatData::Events(Box::new(e.slice(offset, len)?)),
            FlatData::Links(l) => FlatData::Links(Box::new(l.slice(offset, len)?)),
        };
        let nulls = slice_optional_bitmap(self.nulls.as_ref(), offset, len)?;
        Ok(Self { data, nulls })
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

        let mut out_valid = Bitmap::new_ones(indices.len());
        for (out_pos, &idx) in indices.iter().enumerate() {
            if valid_mask.is_some_and(|m| !m.get(out_pos)) {
                let _ = out_valid.set(out_pos, false);
            } else {
                let row = idx as usize;
                if row >= self.len() {
                    return Err(ColumnError::InvalidBatch {
                        reason: "take index out of bounds",
                    });
                }
                if self.is_null(row) {
                    let _ = out_valid.set(out_pos, false);
                }
            }
        }

        let out_nulls = if matches!(self.data, FlatData::Null(_)) || out_valid.all_ones() {
            None
        } else {
            Some(out_valid)
        };

        let nulls_bytes = out_nulls.as_ref().map_or(0, Bitmap::memory_bytes);

        let data = match &self.data {
            FlatData::Null(_) => FlatData::Null(indices.len()),
            FlatData::Bool(src) => {
                let out_bytes = indices.len().div_ceil(64) * 8 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                let mut bm = Bitmap::new_zeroed(indices.len());
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if out_nulls.as_ref().is_none_or(|m| m.get(out_pos)) && src.get(idx as usize) {
                        let _ = bm.set(out_pos, true);
                    }
                }
                FlatData::Bool(bm)
            }
            FlatData::I64(src) => {
                let out_bytes = indices.len() * 8 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::I64(gather_fixed(src, indices, out_nulls.as_ref(), 0))
            }
            FlatData::F64(src) => {
                let out_bytes = indices.len() * 8 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::F64(gather_fixed(src, indices, out_nulls.as_ref(), 0.0))
            }
            FlatData::Decimal128 {
                values,
                precision,
                scale,
            } => {
                let out_bytes = indices.len() * 16 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::Decimal128 {
                    values: gather_fixed(values, indices, out_nulls.as_ref(), 0),
                    precision: *precision,
                    scale: *scale,
                }
            }
            FlatData::TimestampNs { values, utc } => {
                let out_bytes = indices.len() * 8 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::TimestampNs {
                    values: gather_fixed(values, indices, out_nulls.as_ref(), 0),
                    utc: *utc,
                }
            }
            FlatData::Date(src) => {
                let out_bytes = indices.len() * 4 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::Date(gather_fixed(src, indices, out_nulls.as_ref(), 0))
            }
            FlatData::Interval(src) => {
                let out_bytes = indices.len() * size_of::<Interval>() + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::Interval(gather_fixed(
                    src,
                    indices,
                    out_nulls.as_ref(),
                    Interval::default(),
                ))
            }
            FlatData::FixedBytes8(src) => {
                let out_bytes = indices.len() * 8 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::FixedBytes8(gather_fixed(src, indices, out_nulls.as_ref(), [0u8; 8]))
            }
            FlatData::FixedBytes16(src) => {
                let out_bytes = indices.len() * 16 + nulls_bytes;
                reconcile_reservation(pool.as_deref_mut(), before, out_bytes)?;
                FlatData::FixedBytes16(gather_fixed(src, indices, out_nulls.as_ref(), [0u8; 16]))
            }
            FlatData::Utf8(u) => {
                let gathered = u.take(indices, out_nulls.as_ref())?;
                FlatData::Utf8(gathered)
            }
            FlatData::Binary(b) => {
                let gathered = b.take(indices, out_nulls.as_ref())?;
                FlatData::Binary(gathered)
            }
            FlatData::List {
                item_type,
                offsets,
                values,
            } => {
                let mut total_child = 0usize;
                for (out_pos, &idx) in indices.iter().enumerate() {
                    if out_nulls.as_ref().is_none_or(|m| m.get(out_pos)) {
                        let row = idx as usize;
                        let count = (offsets[row + 1] - offsets[row]) as usize;
                        total_child =
                            total_child
                                .checked_add(count)
                                .ok_or(ColumnError::Overflow {
                                    reason: "list take child count overflow",
                                })?;
                    }
                }
                let _ = checked_u32(total_child, "list take child count exceeds u32::MAX")?;
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
                FlatData::List {
                    item_type: item_type.clone(),
                    offsets: out_offsets,
                    values: Box::new(Column::Flat(out_values)),
                }
            }
            FlatData::MapUtf8(m) => {
                FlatData::MapUtf8(Box::new(m.take(indices, out_nulls.as_ref())?))
            }
            FlatData::Events(e) => FlatData::Events(Box::new(e.take(indices, out_nulls.as_ref())?)),
            FlatData::Links(l) => FlatData::Links(Box::new(l.take(indices, out_nulls.as_ref())?)),
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

    pub(crate) fn concat(
        data_type: &DataType,
        parts: &[&Self],
        total_rows: usize,
    ) -> Result<Self, ColumnError> {
        if matches!(data_type, DataType::Null) {
            return Ok(Self {
                data: FlatData::Null(total_rows),
                nulls: None,
            });
        }
        let null_parts: Vec<(usize, Option<&Bitmap>)> =
            parts.iter().map(|p| (p.len(), p.nulls())).collect();
        let nulls = concat_bitmaps(&null_parts, total_rows);

        let data = match data_type {
            DataType::Null => FlatData::Null(total_rows),
            DataType::Bool => {
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
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::I64(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::I64(out)
            }
            DataType::F64 => {
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::F64(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::F64(out)
            }
            DataType::Decimal128 { precision, scale } => {
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
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::Date(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::Date(out)
            }
            DataType::Interval => {
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::Interval(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::Interval(out)
            }
            DataType::Utf8 => {
                let u_parts: Vec<&Utf8Data> = parts
                    .iter()
                    .filter_map(|p| match p.data() {
                        FlatData::Utf8(u) => Some(u),
                        _ => None,
                    })
                    .collect();
                FlatData::Utf8(Utf8Data::concat(&u_parts)?)
            }
            DataType::Binary => {
                let b_parts: Vec<&BinaryData> = parts
                    .iter()
                    .filter_map(|p| match p.data() {
                        FlatData::Binary(b) => Some(b),
                        _ => None,
                    })
                    .collect();
                FlatData::Binary(BinaryData::concat(&b_parts)?)
            }
            DataType::FixedBytes8 => {
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::FixedBytes8(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::FixedBytes8(out)
            }
            DataType::FixedBytes16 => {
                let mut out = Vec::with_capacity(total_rows);
                for p in parts {
                    if let FlatData::FixedBytes16(v) = p.data() {
                        out.extend_from_slice(v);
                    }
                }
                FlatData::FixedBytes16(out)
            }
            DataType::List { .. } => {
                let DataType::List(inner) = data_type else {
                    return Err(ColumnError::InvalidBatch {
                        reason: "list data type mismatch",
                    });
                };
                let mut total_child = 0usize;
                let mut child_cols = Vec::with_capacity(parts.len());
                for p in parts {
                    if let FlatData::List { values, .. } = p.data() {
                        total_child =
                            total_child
                                .checked_add(values.len())
                                .ok_or(ColumnError::Overflow {
                                    reason: "list concat child length overflow",
                                })?;
                        child_cols.push(values.as_ref());
                    }
                }
                let _ = checked_u32(total_child, "list concat child length exceeds u32::MAX")?;
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
                let mut flat_children = Vec::with_capacity(child_cols.len());
                for col in &child_cols {
                    flat_children.push(col.materialize(None)?);
                }
                let child_refs: Vec<&Self> = flat_children.iter().collect();
                let merged_child = Self::concat(inner, &child_refs, total_child)?;
                FlatData::List {
                    item_type: (**inner).clone(),
                    offsets,
                    values: Box::new(Column::Flat(merged_child)),
                }
            }
            DataType::MapUtf8 => {
                let m_parts: Vec<&MapData> = parts
                    .iter()
                    .filter_map(|p| match p.data() {
                        FlatData::MapUtf8(m) => Some(m.as_ref()),
                        _ => None,
                    })
                    .collect();
                FlatData::MapUtf8(Box::new(MapData::concat(&m_parts)?))
            }
            DataType::Events => {
                let e_parts: Vec<&EventsData> = parts
                    .iter()
                    .filter_map(|p| match p.data() {
                        FlatData::Events(e) => Some(e.as_ref()),
                        _ => None,
                    })
                    .collect();
                FlatData::Events(Box::new(EventsData::concat(&e_parts)?))
            }
            DataType::Links => {
                let l_parts: Vec<&LinksData> = parts
                    .iter()
                    .filter_map(|p| match p.data() {
                        FlatData::Links(l) => Some(l.as_ref()),
                        _ => None,
                    })
                    .collect();
                FlatData::Links(Box::new(LinksData::concat(&l_parts)?))
            }
        };

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
    values: Box<FlatColumn>,
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
            values: Box::new(values),
            codes,
            nulls,
        })
    }

    /// Dictionary values table.
    pub fn values(&self) -> &FlatColumn {
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

    /// Slices `offset..offset + len` (keeps `values`, slices `codes` and `nulls`).
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "dict column slice out of bounds",
            });
        }
        let codes = self.codes[offset..offset + len].to_vec();
        let nulls = slice_optional_bitmap(self.nulls.as_ref(), offset, len)?;
        Ok(Self {
            values: self.values.clone(),
            codes,
            nulls,
        })
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
    values: Box<FlatColumn>,
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

impl RleColumn {
    /// Validates `run_ends.len() == values.len()`, `run_ends[0] > 0` (if non-empty), and strict monotonicity.
    pub fn try_new(values: FlatColumn, run_ends: Vec<u32>) -> Result<Self, ColumnError> {
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
        Ok(Self {
            values: Box::new(values),
            run_ends,
        })
    }

    /// One value per run (null runs encoded via `values.nulls()`).
    pub fn values(&self) -> &FlatColumn {
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

    /// Slices `offset..offset + len`, trimming and rebasing `run_ends` to `0..len`.
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "rle column slice out of bounds",
            });
        }
        if len == 0 {
            return Ok(Self {
                values: Box::new(FlatColumn::empty(&self.data_type())),
                run_ends: Vec::new(),
            });
        }
        let end_row = offset + len;
        let first_run = self
            .run_ends
            .partition_point(|&end| (end as usize) <= offset);
        let last_run = self
            .run_ends
            .partition_point(|&end| (end as usize) < end_row);
        let run_count = last_run - first_run + 1;
        let new_values = self.values.slice(first_run, run_count)?;
        let new_run_ends = self.run_ends[first_run..=last_run]
            .iter()
            .map(|&re| ((re as usize).min(end_row) - offset) as u32)
            .collect();
        Ok(Self {
            values: Box::new(new_values),
            run_ends: new_run_ends,
        })
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
    /// Checked constructor for `Column::Const` (validates `value.validate(MAX_NESTING_DEPTH)`).
    pub fn try_new_const(value: ScalarValue, len: usize) -> Result<Self, ColumnError> {
        value.validate(MAX_NESTING_DEPTH)?;
        Ok(Self::Const { value, len })
    }

    pub(crate) fn validate_with_depth(&self, max_depth: usize) -> Result<(), ColumnError> {
        match self {
            Self::Flat(f) => {
                f.data_type().validate(max_depth)?;
                if let FlatData::List { values, .. } = f.data() {
                    if max_depth == 0 {
                        return Err(ColumnError::InvalidBatch {
                            reason: "exceeded maximum nesting depth",
                        });
                    }
                    values.validate_with_depth(max_depth - 1)?;
                }
                Ok(())
            }
            Self::Const { value, .. } => value.validate(max_depth),
            Self::Dict(d) => Self::Flat((*d.values).clone()).validate_with_depth(max_depth),
            Self::Rle(r) => Self::Flat((*r.values).clone()).validate_with_depth(max_depth),
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
                let out = f.clone();
                reconcile_reservation(pool.as_deref_mut(), before, out.memory_bytes())?;
                Ok(out)
            }
            Self::Const { value, len } => {
                if let Err(err) = value.validate(MAX_NESTING_DEPTH) {
                    rollback_reservation(pool, before);
                    return Err(err);
                }
                let one = match flat_from_scalar_one(value) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                let indices = vec![0u32; *len];
                one.take(&indices, pool)
            }
            Self::Dict(d) => {
                if let Some(code_nulls) = d.nulls() {
                    let mut indices = Vec::with_capacity(d.len());
                    for (i, &code) in d.codes().iter().enumerate() {
                        if code_nulls.get(i) {
                            indices.push(code);
                        } else {
                            indices.push(0);
                        }
                    }
                    d.values().take_with_mask(&indices, Some(code_nulls), pool)
                } else {
                    d.values().take(d.codes(), pool)
                }
            }
            Self::Rle(r) => {
                let mut indices = Vec::with_capacity(r.len());
                let mut prev = 0u32;
                for (run_idx, &end) in r.run_ends().iter().enumerate() {
                    let count = (end - prev) as usize;
                    indices.resize(indices.len() + count, run_idx as u32);
                    prev = end;
                }
                r.values().take(&indices, pool)
            }
        }
    }

    /// Gathers `indices` into a [`FlatColumn`], validating every `idx < self.len()` (including for `Column::Const`),
    /// writing canonical zero/empty backing values at null slots, and reconciling `pool` to exact `result.memory_bytes()`.
    pub fn take(
        &self,
        indices: &[u32],
        pool: Option<&mut Reservation>,
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
                let one = match flat_from_scalar_one(value) {
                    Ok(c) => c,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                let zero_indices = vec![0u32; indices.len()];
                one.take(&zero_indices, pool)
            }
            Self::Dict(d) => {
                let mut dict_indices = Vec::with_capacity(indices.len());
                if let Some(code_nulls) = d.nulls() {
                    let mut mask = Bitmap::new_ones(indices.len());
                    for (out_pos, &idx) in indices.iter().enumerate() {
                        let row = idx as usize;
                        if row >= d.len() {
                            rollback_reservation(pool, before);
                            return Err(ColumnError::InvalidBatch {
                                reason: "take index out of bounds",
                            });
                        }
                        if code_nulls.get(row) {
                            dict_indices.push(d.codes()[row]);
                        } else {
                            let _ = mask.set(out_pos, false);
                            dict_indices.push(0);
                        }
                    }
                    d.values().take_with_mask(&dict_indices, Some(&mask), pool)
                } else {
                    for &idx in indices {
                        let row = idx as usize;
                        if row >= d.len() {
                            rollback_reservation(pool, before);
                            return Err(ColumnError::InvalidBatch {
                                reason: "take index out of bounds",
                            });
                        }
                        dict_indices.push(d.codes()[row]);
                    }
                    d.values().take(&dict_indices, pool)
                }
            }
            Self::Rle(r) => {
                let mut run_indices = Vec::with_capacity(indices.len());
                for &idx in indices {
                    let row = idx as usize;
                    if row >= r.len() {
                        rollback_reservation(pool, before);
                        return Err(ColumnError::InvalidBatch {
                            reason: "take index out of bounds",
                        });
                    }
                    let run_idx = r.run_ends().partition_point(|&end| (end as usize) <= row);
                    run_indices.push(run_idx as u32);
                }
                r.values().take(&run_indices, pool)
            }
        }
    }

    /// Slices `offset..offset + len` (`Const` is O(1)).
    pub fn slice(&self, offset: usize, len: usize) -> Result<Self, ColumnError> {
        match self {
            Self::Flat(f) => Ok(Self::Flat(f.slice(offset, len)?)),
            Self::Const {
                value,
                len: cur_len,
            } => {
                if offset.checked_add(len).is_none_or(|end| end > *cur_len) {
                    return Err(ColumnError::InvalidBatch {
                        reason: "const column slice out of bounds",
                    });
                }
                Ok(Self::Const {
                    value: value.clone(),
                    len,
                })
            }
            Self::Dict(d) => Ok(Self::Dict(d.slice(offset, len)?)),
            Self::Rle(r) => Ok(Self::Rle(r.slice(offset, len)?)),
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

            let sliced = flat_col.slice(1, 2).unwrap();
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
            let const_sliced = const_col.slice(1, 2).unwrap();
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
            let dict_sliced = dict_col.slice(2, 2).unwrap();
            assert!(dict_sliced.semantic_eq(&Column::Flat(flat.slice(0, 2).unwrap())));
            assert!(dict_sliced.get_scalar(0).unwrap().semantic_eq(&scalars[0]));
            if !matches!(dt, DataType::Null) {
                assert!(!dict_col.slice(0, 1).unwrap().has_nulls());
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
            let rle_sliced = rle_col.slice(1, 3).unwrap();
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
        let batch = Batch::try_new(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3]), None).unwrap()),
                u_col
                    .slice(0, 2)
                    .unwrap()
                    .materialize(None)
                    .map(Column::Flat)
                    .unwrap(),
            ],
            2,
            Selection::Mask(Bitmap::from_bools(&[true, false])),
        );
        // Use a valid 3-row batch for compact
        drop(batch);
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
}
