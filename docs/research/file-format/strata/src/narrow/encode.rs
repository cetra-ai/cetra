//! Encoding one Arrow slice into one narrow chunk, following the S3 rules.

use arrow::array::{Array, ArrayRef, BooleanArray, FixedSizeBinaryArray};
use arrow::datatypes::DataType;

use super::bitpack::{pack, width_for};
use super::{Class, Encoding, Numeric, flags, numeric_of, unsupported};
use crate::config::Config;
use crate::error::Result;
use crate::format::le_u64;

/// One chunk, encoded. `body` is `None` for `Const` and `ConstNull`, which write no
/// segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedChunk {
    /// The encoding of the chunk.
    pub encoding: Encoding,
    /// [`flags`] bits.
    pub flags: u8,
    /// Length of the constant in `min`, for `Const`.
    pub const_len: u16,
    /// Zone minimum, or the constant value.
    pub min: [u8; 16],
    /// Zone maximum.
    pub max: [u8; 16],
    /// The segment body, `None` when the chunk has none.
    pub body: Option<Vec<u8>>,
}

impl EncodedChunk {
    fn bare(encoding: Encoding, flags: u8) -> Self {
        Self {
            encoding,
            flags,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
            body: None,
        }
    }
}

/// Encodes one chunk. `_config` is not used in M1.2: the writer applies zstd. M1.3 uses it
/// for dictionaries.
pub fn encode_chunk(array: &ArrayRef, _config: &Config) -> Result<EncodedChunk> {
    let array: &dyn Array = array.as_ref();
    if array.null_count() == array.len() {
        return Ok(EncodedChunk::bare(
            Encoding::ConstNull,
            flags::ALL_NULL | flags::HAS_NULLS,
        ));
    }
    let dt = array.data_type();
    match dt {
        DataType::Boolean => encode_bool(array),
        DataType::FixedSizeBinary(w) => {
            let width = usize::try_from(*w).map_err(|_| unsupported(dt))?;
            encode_fsb(array, width)
        }
        _ => {
            let num = numeric_of(dt).ok_or_else(|| unsupported(dt))?;
            encode_numeric(array, num)
        }
    }
}

/// Copies the low `src.len()` bytes of `src` into the start of `dst`.
fn set_prefix(dst: &mut [u8], src: &[u8]) {
    if let Some(part) = dst.get_mut(..src.len()) {
        part.copy_from_slice(src);
    }
}

/// Appends the low `width` bytes of `v`, little-endian.
fn push_le(body: &mut Vec<u8>, v: u64, width: usize) {
    let le = v.to_le_bytes();
    body.extend_from_slice(le.get(..width).unwrap_or_default());
}

fn set_bit(bitmap: &mut [u8], i: usize) {
    if let Some(byte) = bitmap.get_mut(i / 8) {
        *byte |= 1u8 << (i % 8);
    }
}

/// The Arrow validity bitmap of `array`: bit `i` is set when row `i` is valid.
fn validity_bitmap(array: &dyn Array) -> Vec<u8> {
    let mut bitmap = vec![0u8; array.len().div_ceil(8)];
    for i in 0..array.len() {
        if array.is_valid(i) {
            set_bit(&mut bitmap, i);
        }
    }
    bitmap
}

/// The physical bits of every row, read from the value buffer. Null rows read as 0.
fn read_bits(array: &dyn Array, num: Numeric) -> Result<Vec<u64>> {
    let data = array.to_data();
    let bytes = data
        .buffers()
        .first()
        .ok_or_else(|| unsupported(array.data_type()))?
        .as_slice();
    let offset = data.offset();
    let mut bits = Vec::with_capacity(array.len());
    for i in 0..array.len() {
        if !array.is_valid(i) {
            bits.push(0);
            continue;
        }
        let start = (offset + i) * num.width;
        let raw = bytes
            .get(start..start + num.width)
            .and_then(le_u64)
            .ok_or_else(|| unsupported(array.data_type()))?;
        bits.push(if num.signed {
            super::sign_extend(raw, num.width)
        } else {
            raw
        });
    }
    Ok(bits)
}

/// The float value of `bits`, read as `f32` when `width` is 4.
fn float_of(bits: u64, width: usize) -> f64 {
    if width == 4 {
        f64::from(f32::from_bits(bits as u32))
    } else {
        f64::from_bits(bits)
    }
}

fn is_nan_bits(bits: u64, width: usize) -> bool {
    float_of(bits, width).is_nan()
}

/// The chunk for a numeric chunk whose non-null values are all equal to `value`.
fn constant_numeric(num: Numeric, value: u64) -> EncodedChunk {
    let mut chunk = EncodedChunk::bare(Encoding::Const, 0);
    let le = value.to_le_bytes();
    match num.class {
        Class::Int => {
            chunk.const_len = 8;
            set_prefix(&mut chunk.min, &le);
            set_prefix(&mut chunk.max, &le);
            chunk.flags = flags::HAS_ZONE;
        }
        Class::Wide => {
            chunk.const_len = 8;
            set_prefix(&mut chunk.min, &le);
        }
        Class::Float => {
            chunk.const_len = num.width as u16;
            set_prefix(&mut chunk.min, le.get(..num.width).unwrap_or_default());
            if is_nan_bits(value, num.width) {
                chunk.flags = flags::HAS_NAN;
            }
        }
    }
    chunk
}

fn encode_numeric(array: &dyn Array, num: Numeric) -> Result<EncodedChunk> {
    let bits = read_bits(array, num)?;
    let valid: Vec<bool> = (0..array.len()).map(|i| array.is_valid(i)).collect();
    let has_nulls = array.null_count() > 0;
    let first = bits.first().copied().unwrap_or_default();
    if !has_nulls && bits.iter().all(|b| *b == first) {
        return Ok(constant_numeric(num, first));
    }

    let mut body = Vec::new();
    let mut chunk_flags = 0u8;
    if has_nulls {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    let mut chunk = EncodedChunk::bare(Encoding::Plain, 0);
    match num.class {
        Class::Int => {
            // The min and max are taken over the non-null values, which exist because the
            // chunk is not all null.
            let mut min = i64::MAX;
            let mut max = i64::MIN;
            for (b, v) in bits.iter().zip(&valid) {
                if *v {
                    let x = *b as i64;
                    min = min.min(x);
                    max = max.max(x);
                }
            }
            let deltas: Vec<u64> = bits
                .iter()
                .zip(&valid)
                .map(|(b, v)| if *v { b.wrapping_sub(min as u64) } else { 0 })
                .collect();
            let width = width_for(deltas.iter().copied().max().unwrap_or_default());
            body.push(width);
            pack(&deltas, width, &mut body);
            set_prefix(&mut chunk.min, &min.to_le_bytes());
            set_prefix(&mut chunk.max, &max.to_le_bytes());
            chunk_flags |= flags::HAS_ZONE;
            chunk.encoding = Encoding::ForBitpack;
        }
        Class::Wide => {
            for b in &bits {
                push_le(&mut body, *b, num.width);
            }
        }
        Class::Float => {
            let mut has_nan = false;
            let mut zone: Option<(f64, f64)> = None;
            for (b, v) in bits.iter().zip(&valid) {
                push_le(&mut body, *b, num.width);
                if !*v {
                    continue;
                }
                let x = float_of(*b, num.width);
                if x.is_nan() {
                    has_nan = true;
                    continue;
                }
                // -0.0 and 0.0 are one zone value.
                let x = if x == 0.0 { 0.0 } else { x };
                zone = Some(match zone {
                    None => (x, x),
                    Some((lo, hi)) => (lo.min(x), hi.max(x)),
                });
            }
            if has_nan {
                chunk_flags |= flags::HAS_NAN;
            }
            if let Some((lo, hi)) = zone {
                set_prefix(&mut chunk.min, &lo.to_le_bytes());
                set_prefix(&mut chunk.max, &hi.to_le_bytes());
                chunk_flags |= flags::HAS_ZONE;
            }
        }
    }
    chunk.flags = chunk_flags;
    chunk.body = Some(body);
    Ok(chunk)
}

fn encode_bool(array: &dyn Array) -> Result<EncodedChunk> {
    let booleans = array
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| unsupported(array.data_type()))?;
    let n = booleans.len();
    let values: Vec<bool> = (0..n)
        .map(|i| booleans.is_valid(i) && booleans.value(i))
        .collect();
    let has_nulls = booleans.null_count() > 0;
    let first = values.first().copied().unwrap_or_default();
    if !has_nulls && values.iter().all(|v| *v == first) {
        let mut chunk = EncodedChunk::bare(Encoding::Const, 0);
        chunk.const_len = 1;
        set_prefix(&mut chunk.min, &[u8::from(first)]);
        return Ok(chunk);
    }

    let mut body = Vec::new();
    let mut chunk_flags = 0u8;
    if has_nulls {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    let mut bitmap = vec![0u8; n.div_ceil(8)];
    for (i, v) in values.iter().enumerate() {
        if *v {
            set_bit(&mut bitmap, i);
        }
    }
    body.extend(bitmap);
    let mut chunk = EncodedChunk::bare(Encoding::Bool, chunk_flags);
    chunk.body = Some(body);
    Ok(chunk)
}

/// The 16-byte zone value of a fixed-size binary value of at most 16 bytes, zero-padded.
fn pad16(row: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    set_prefix(&mut out, row);
    out
}

fn encode_fsb(array: &dyn Array, width: usize) -> Result<EncodedChunk> {
    let fsb = array
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .ok_or_else(|| unsupported(array.data_type()))?;
    let n = fsb.len();
    let has_nulls = fsb.null_count() > 0;
    let zero = vec![0u8; width];
    // Null rows take the value zero, in every encoding.
    let rows: Vec<&[u8]> = (0..n)
        .map(|i| {
            if fsb.is_valid(i) {
                fsb.value(i)
            } else {
                zero.as_slice()
            }
        })
        .collect();
    let first = rows.first().copied().unwrap_or_default();
    if !has_nulls && width <= 16 && rows.iter().all(|r| *r == first) {
        let mut chunk = EncodedChunk::bare(Encoding::Const, flags::HAS_ZONE);
        chunk.const_len = width as u16;
        set_prefix(&mut chunk.min, first);
        set_prefix(&mut chunk.max, first);
        return Ok(chunk);
    }

    let mut body = Vec::new();
    let mut chunk_flags = 0u8;
    if has_nulls {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    let mut chunk = EncodedChunk::bare(Encoding::FsbRaw, 0);
    if width <= 16 {
        let mut zone: Option<([u8; 16], [u8; 16])> = None;
        for (i, row) in rows.iter().enumerate() {
            if !fsb.is_valid(i) {
                continue;
            }
            let padded = pad16(row);
            zone = Some(match zone {
                None => (padded, padded),
                Some((lo, hi)) => (lo.min(padded), hi.max(padded)),
            });
        }
        if let Some((lo, hi)) = zone {
            chunk.min = lo;
            chunk.max = hi;
            chunk_flags |= flags::HAS_ZONE;
        }
    }

    let mut runs: Vec<(usize, &[u8])> = Vec::new();
    for row in &rows {
        match runs.last_mut() {
            Some((len, value)) if *value == *row => *len += 1,
            _ => runs.push((1, *row)),
        }
    }
    let too_long = || unsupported(array.data_type());
    if runs.len() * 2 < n {
        let n_runs = u16::try_from(runs.len()).map_err(|_| too_long())?;
        body.extend_from_slice(&n_runs.to_le_bytes());
        for (len, _) in &runs {
            let len = u16::try_from(*len).map_err(|_| too_long())?;
            body.extend_from_slice(&len.to_le_bytes());
        }
        for (_, value) in &runs {
            body.extend_from_slice(value);
        }
        chunk.encoding = Encoding::FsbRle;
    } else {
        for row in &rows {
            body.extend_from_slice(row);
        }
    }
    chunk.flags = chunk_flags;
    chunk.body = Some(body);
    Ok(chunk)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use arrow::array::{Int64Array, new_null_array};
    use std::sync::Arc;

    #[test]
    fn a_constant_chunk_has_no_body() {
        let array: ArrayRef = Arc::new(Int64Array::from(vec![7; 100]));
        let chunk = encode_chunk(&array, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Const);
        assert!(chunk.body.is_none());
    }

    #[test]
    fn all_null_is_const_null_without_body() {
        let array = new_null_array(&DataType::Int64, 10);
        let chunk = encode_chunk(&array, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::ConstNull);
        assert!(chunk.body.is_none());
    }

    #[test]
    fn all_equal_with_nulls_is_for_width_zero() {
        let array: ArrayRef = Arc::new(Int64Array::from(vec![Some(5), None, Some(5), Some(5)]));
        let chunk = encode_chunk(&array, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::ForBitpack);
        let body = chunk.body.unwrap();
        // One validity byte, then the width byte.
        assert_eq!(body[1], 0);
    }

    #[test]
    fn utf8_is_unsupported_in_m1_2() {
        let array = arrow::array::StringArray::from(vec!["a"]);
        let array: ArrayRef = Arc::new(array);
        assert!(matches!(
            encode_chunk(&array, &Config::default()),
            Err(crate::error::StrataError::Unsupported { .. })
        ));
    }
}
