//! Encoding one Arrow slice into one narrow chunk, following the S3 rules.

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, BooleanArray, FixedSizeBinaryArray, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;

use super::bitpack::{pack, width_for};
use super::{Class, Dictionary, Encoding, Numeric, flags, numeric_of, unsupported};
use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{MAX_CHUNK_ELEMENTS, MAX_CHUNK_OUTPUT, le_u64};

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

/// Encodes one chunk. `dict` is the column's dictionary, when it has one: Utf8 and
/// List(Utf8) chunks use it. `_config` is unused: the writer applies zstd.
pub fn encode_chunk(
    array: &ArrayRef,
    dict: Option<&Dictionary>,
    _config: &Config,
) -> Result<EncodedChunk> {
    if array.null_count() == array.len() {
        return Ok(EncodedChunk::bare(
            Encoding::ConstNull,
            flags::ALL_NULL | flags::HAS_NULLS,
        ));
    }
    let dt = array.data_type();
    let values: &dyn Array = array.as_ref();
    match dt {
        DataType::Boolean => encode_bool(values),
        DataType::FixedSizeBinary(w) => {
            let width = usize::try_from(*w).map_err(|_| unsupported(dt))?;
            encode_fsb(values, width)
        }
        DataType::Utf8 => match dict {
            Some(dict) => encode_utf8(values, dict),
            None => encode_varbin(values),
        },
        DataType::Binary => encode_varbin(values),
        DataType::List(field) if field.data_type() == &DataType::Utf8 => {
            // A chunk with a null element inside a non-null list is `IPC`.
            let listed = match dict {
                Some(dict) => encode_list_utf8(values, dict)?,
                None => None,
            };
            match listed {
                Some(chunk) => Ok(chunk),
                None => encode_ipc(array),
            }
        }
        _ => match numeric_of(dt) {
            Some(num) => encode_numeric(values, num),
            None => encode_ipc(array),
        },
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

/// The dictionary code of `v`. The dictionary holds every value of its column, so a miss is
/// a writer bug, reported without the value.
fn code_of(dict: &Dictionary, v: &[u8]) -> Result<u32> {
    dict.code_of(v).ok_or_else(|| StrataError::Schema {
        reason: "a value is missing from its dictionary".to_string(),
    })
}

/// The byte length of the dictionary value of `code`. The code came from [`code_of`].
fn value_len(dict: &Dictionary, code: u32) -> Result<usize> {
    dict.value(code)
        .map(<[u8]>::len)
        .ok_or_else(|| StrataError::Schema {
            reason: "a value is missing from its dictionary".to_string(),
        })
}

/// Adds `len` decoded bytes to `total`. A total above `MAX_CHUNK_OUTPUT` is refused, since
/// the reader refuses such a chunk.
fn add_output(total: usize, len: usize) -> Result<usize> {
    total
        .checked_add(len)
        .filter(|sum| *sum <= MAX_CHUNK_OUTPUT)
        .ok_or_else(|| StrataError::Unsupported {
            what: "chunk output above MAX_CHUNK_OUTPUT".to_string(),
        })
}

/// A Utf8 chunk with a dictionary: `Const` when every value is the same, else `Dict`.
fn encode_utf8(array: &dyn Array, dict: &Dictionary) -> Result<EncodedChunk> {
    let strings = array
        .as_string_opt::<i32>()
        .ok_or_else(|| unsupported(array.data_type()))?;
    let n = strings.len();
    let has_nulls = strings.null_count() > 0;
    // Null rows take code 0 in the packed codes; the zone uses only the valid codes.
    let mut codes: Vec<u64> = Vec::with_capacity(n);
    let mut valid_codes: Vec<u32> = Vec::with_capacity(n);
    // The decoded bytes of the valid rows, checked against `MAX_CHUNK_OUTPUT`.
    let mut decoded = 0usize;
    for i in 0..n {
        if strings.is_valid(i) {
            let code = code_of(dict, strings.value(i).as_bytes())?;
            decoded = add_output(decoded, value_len(dict, code)?)?;
            codes.push(u64::from(code));
            valid_codes.push(code);
        } else {
            codes.push(0);
        }
    }

    let first = valid_codes.first().copied().unwrap_or_default();
    if !has_nulls && valid_codes.iter().all(|c| *c == first) {
        let mut chunk = EncodedChunk::bare(Encoding::Const, flags::HAS_ZONE);
        chunk.const_len = 4;
        set_prefix(&mut chunk.min, &first.to_le_bytes());
        chunk.max = chunk.min;
        return Ok(chunk);
    }

    let lo = valid_codes.iter().min().copied().unwrap_or_default();
    let hi = valid_codes.iter().max().copied().unwrap_or_default();
    let mut body = Vec::new();
    let mut chunk_flags = flags::HAS_ZONE;
    if has_nulls {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    // Null rows hold code 0, so the largest code is the largest valid one.
    let width = width_for(u64::from(hi));
    body.push(width);
    pack(&codes, width, &mut body);
    let mut chunk = EncodedChunk::bare(Encoding::Dict, chunk_flags);
    set_prefix(&mut chunk.min, &lo.to_le_bytes());
    set_prefix(&mut chunk.max, &hi.to_le_bytes());
    chunk.body = Some(body);
    Ok(chunk)
}

/// The values of a Utf8 or Binary chunk, one slice per row. A null row gives an empty slice,
/// which is what its varbin slot holds.
fn varbin_rows(array: &dyn Array) -> Result<Vec<&[u8]>> {
    let empty: &[u8] = &[];
    match array.data_type() {
        DataType::Utf8 => {
            let strings = array
                .as_string_opt::<i32>()
                .ok_or_else(|| unsupported(array.data_type()))?;
            Ok((0..strings.len())
                .map(|i| {
                    if strings.is_valid(i) {
                        strings.value(i).as_bytes()
                    } else {
                        empty
                    }
                })
                .collect())
        }
        DataType::Binary => {
            let binaries = array
                .as_binary_opt::<i32>()
                .ok_or_else(|| unsupported(array.data_type()))?;
            Ok((0..binaries.len())
                .map(|i| {
                    if binaries.is_valid(i) {
                        binaries.value(i)
                    } else {
                        empty
                    }
                })
                .collect())
        }
        dt => Err(unsupported(dt)),
    }
}

/// A Utf8 or Binary chunk without a dictionary. When every row is the same and at most 16
/// bytes long, and none is null, it is `Const` with the bytes inline. Otherwise it is
/// `Varbin`: the validity bitmap if there are nulls, `u32 offsets[n+1]` (null slots have
/// zero length), then the bytes.
fn encode_varbin(array: &dyn Array) -> Result<EncodedChunk> {
    let rows = varbin_rows(array)?;
    let has_nulls = array.null_count() > 0;
    let first = rows.first().copied().unwrap_or_default();
    if !has_nulls && first.len() <= 16 && rows.iter().all(|row| *row == first) {
        let mut chunk = EncodedChunk::bare(Encoding::Const, 0);
        chunk.const_len = first.len() as u16;
        set_prefix(&mut chunk.min, first);
        return Ok(chunk);
    }

    let too_long = || unsupported(array.data_type());
    let mut body = Vec::new();
    let mut chunk_flags = 0u8;
    if has_nulls {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    let mut total = 0u32;
    body.extend_from_slice(&total.to_le_bytes());
    for row in &rows {
        let len = u32::try_from(row.len()).map_err(|_| too_long())?;
        total = total.checked_add(len).ok_or_else(too_long)?;
        body.extend_from_slice(&total.to_le_bytes());
    }
    for row in &rows {
        body.extend_from_slice(row);
    }
    let mut chunk = EncodedChunk::bare(Encoding::Varbin, chunk_flags);
    chunk.body = Some(body);
    Ok(chunk)
}

/// Any chunk of a type without a narrow encoding: an Arrow IPC stream holding one batch
/// with one column. The stream carries the nulls, so there is no validity prefix.
fn encode_ipc(array: &ArrayRef) -> Result<EncodedChunk> {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "c",
        array.data_type().clone(),
        true,
    )]));
    let batch = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::clone(array)])?;
    let mut body = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut body, &schema)?;
        writer.write(&batch)?;
        writer.finish()?;
    }
    let mut chunk = EncodedChunk::bare(Encoding::Ipc, 0);
    chunk.body = Some(body);
    Ok(chunk)
}

/// A List(Utf8) chunk with a dictionary, as `ListDict`: the list offsets, then the packed
/// codes of every element. Returns `None` when a non-null list holds a null element, which
/// the caller stores as `IPC`.
fn encode_list_utf8(array: &dyn Array, dict: &Dictionary) -> Result<Option<EncodedChunk>> {
    let list = array
        .as_list_opt::<i32>()
        .ok_or_else(|| unsupported(array.data_type()))?;
    let child = list
        .values()
        .as_string_opt::<i32>()
        .ok_or_else(|| unsupported(array.data_type()))?;
    let n = list.len();
    let has_nulls = list.null_count() > 0;
    let mut codes: Vec<u64> = Vec::new();
    // The decoded bytes of the elements of the valid lists, checked against `MAX_CHUNK_OUTPUT`.
    let mut decoded = 0usize;
    let mut offsets: Vec<u32> = Vec::with_capacity(n + 1);
    offsets.push(0);
    for (i, pair) in list.value_offsets().windows(2).enumerate() {
        if list.is_valid(i) {
            let [start, end] = pair else {
                return Err(StrataError::Schema {
                    reason: "list offsets are not paired".to_string(),
                });
            };
            let start = usize::try_from(*start).map_err(|_| StrataError::Schema {
                reason: "list offset is negative".to_string(),
            })?;
            let end = usize::try_from(*end).map_err(|_| StrataError::Schema {
                reason: "list offset is negative".to_string(),
            })?;
            for j in start..end {
                if child.is_null(j) {
                    return Ok(None);
                }
                let code = code_of(dict, child.value(j).as_bytes())?;
                decoded = add_output(decoded, value_len(dict, code)?)?;
                codes.push(u64::from(code));
            }
        }
        // A null list has no elements in the chunk, whatever its child range holds.
        if codes.len() > MAX_CHUNK_ELEMENTS {
            return Err(StrataError::Unsupported {
                what: format!("a list chunk of more than {MAX_CHUNK_ELEMENTS} elements"),
            });
        }
        let end = u32::try_from(codes.len()).map_err(|_| StrataError::Unsupported {
            what: "a list chunk of more than 4 billion elements".to_string(),
        })?;
        offsets.push(end);
    }

    let mut body = Vec::new();
    let mut chunk_flags = 0u8;
    if has_nulls {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    for offset in &offsets {
        body.extend_from_slice(&offset.to_le_bytes());
    }
    let width = width_for(codes.iter().copied().max().unwrap_or_default());
    body.push(width);
    pack(&codes, width, &mut body);
    let mut chunk = EncodedChunk::bare(Encoding::ListDict, chunk_flags);
    chunk.body = Some(body);
    Ok(Some(chunk))
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
    use crate::narrow::BlockEntry;
    use arrow::array::{Float32Array, Float64Array, Int64Array, new_null_array};
    use std::sync::Arc;

    #[test]
    fn a_constant_chunk_has_no_body() {
        let array: ArrayRef = Arc::new(Int64Array::from(vec![7; 100]));
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Const);
        assert!(chunk.body.is_none());
    }

    #[test]
    fn all_null_is_const_null_without_body() {
        let array = new_null_array(&DataType::Int64, 10);
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::ConstNull);
        assert!(chunk.body.is_none());
    }

    #[test]
    fn all_equal_with_nulls_is_for_width_zero() {
        let array: ArrayRef = Arc::new(Int64Array::from(vec![Some(5), None, Some(5), Some(5)]));
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::ForBitpack);
        let body = chunk.body.unwrap();
        // One validity byte, then the width byte.
        assert_eq!(body[1], 0);
    }

    #[test]
    fn utf8_without_a_dictionary_is_varbin() {
        let array: ArrayRef = Arc::new(arrow::array::StringArray::from(vec!["a", "bb"]));
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Varbin);
        assert!(chunk.body.is_some());
    }

    #[test]
    fn short_equal_utf8_without_a_dictionary_is_inline_const() {
        let array: ArrayRef = Arc::new(arrow::array::StringArray::from(vec!["abc"; 4]));
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Const);
        assert_eq!(chunk.const_len, 3);
        assert!(chunk.body.is_none());
    }

    #[test]
    fn null_list_element_falls_back_to_ipc() {
        let mut builder = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        builder.values().append_value("a");
        builder.values().append_null();
        builder.append(true);
        let array: ArrayRef = Arc::new(builder.finish());
        let dict = crate::narrow::for_column(array.as_ref(), &Config::default());
        let chunk = encode_chunk(&array, dict.as_ref(), &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Ipc);
        assert_eq!(chunk.flags & flags::HAS_NULLS, 0);
    }

    #[test]
    fn map_column_is_ipc() {
        let mut builder = arrow::array::MapBuilder::new(
            None,
            arrow::array::StringBuilder::new(),
            arrow::array::StringBuilder::new(),
        );
        builder.keys().append_value("k");
        builder.values().append_value("v");
        builder.append(true).unwrap();
        let array: ArrayRef = Arc::new(builder.finish());
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Ipc);
    }

    /// Encodes `array` without a dictionary, decodes it and checks the values bitwise.
    fn round_trip(array: ArrayRef) -> EncodedChunk {
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        let entry = BlockEntry {
            first_row: 0,
            n_rows: array.len() as u32,
            segment_offset: if chunk.body.is_some() { 8 } else { 0 },
            segment_len: chunk.body.as_ref().map_or(0, |b| b.len() as u32 + 16),
            encoding: chunk.encoding,
            flags: chunk.flags,
            const_len: chunk.const_len,
            min: chunk.min,
            max: chunk.max,
        };
        let field = Field::new("c", array.data_type().clone(), true);
        let decoded =
            crate::narrow::decode_chunk(&entry, chunk.body.as_deref(), &field, None).unwrap();
        crate::testgen::assert_same(decoded.as_ref(), array.as_ref());
        chunk
    }

    #[test]
    fn float_zone_excludes_nan_and_takes_signed_zero_as_zero() {
        let array = Float64Array::from(vec![Some(0.0), Some(1.5), Some(f64::NAN), None]);
        let chunk = round_trip(Arc::new(array));
        assert_eq!(chunk.encoding, Encoding::Plain);
        assert_ne!(chunk.flags & flags::HAS_NAN, 0);
        assert_ne!(chunk.flags & flags::HAS_ZONE, 0);
        assert_eq!(&chunk.min[..8], &0.0f64.to_le_bytes()[..]);
        assert_eq!(&chunk.max[..8], &1.5f64.to_le_bytes()[..]);
    }

    #[test]
    fn negative_zero_in_a_zone_is_stored_as_positive_zero() {
        let chunk = round_trip(Arc::new(Float64Array::from(vec![-0.0, 2.0])));
        assert_eq!(&chunk.min[..8], &0.0f64.to_le_bytes()[..]);
        assert_eq!(&chunk.max[..8], &2.0f64.to_le_bytes()[..]);
    }

    #[test]
    fn constant_negative_zero_keeps_its_bits_and_has_no_zone() {
        let chunk = round_trip(Arc::new(Float64Array::from(vec![-0.0; 5])));
        assert_eq!(chunk.encoding, Encoding::Const);
        assert_eq!(chunk.const_len, 8);
        assert_eq!(&chunk.min[..8], &(-0.0f64).to_le_bytes()[..]);
        assert_eq!(chunk.flags & flags::HAS_ZONE, 0);
    }

    #[test]
    fn constant_nan_sets_has_nan_and_no_zone() {
        let chunk = round_trip(Arc::new(Float64Array::from(vec![f64::NAN; 5])));
        assert_eq!(chunk.encoding, Encoding::Const);
        assert_ne!(chunk.flags & flags::HAS_NAN, 0);
        assert_eq!(chunk.flags & flags::HAS_ZONE, 0);
    }

    #[test]
    fn float32_constants_take_four_bytes() {
        let zero = round_trip(Arc::new(Float32Array::from(vec![-0.0f32; 5])));
        assert_eq!(zero.encoding, Encoding::Const);
        assert_eq!(zero.const_len, 4);
        assert_eq!(&zero.min[..4], &(-0.0f32).to_le_bytes()[..]);
        assert_eq!(zero.flags & flags::HAS_ZONE, 0);
        let nan = round_trip(Arc::new(Float32Array::from(vec![f32::NAN; 5])));
        assert_eq!(nan.encoding, Encoding::Const);
        assert_eq!(nan.const_len, 4);
        assert_ne!(nan.flags & flags::HAS_NAN, 0);
        assert_eq!(nan.flags & flags::HAS_ZONE, 0);
    }

    #[test]
    fn utf8_constant_chunks_with_and_without_a_dictionary() {
        let array: ArrayRef = Arc::new(arrow::array::StringArray::from(vec!["abc"; 4]));
        let with_dict = crate::narrow::for_column(array.as_ref(), &Config::default()).unwrap();
        let chunk = encode_chunk(&array, Some(&with_dict), &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Const);
        assert_eq!(chunk.const_len, 4);
        assert!(chunk.body.is_none());

        let no_dict = Config {
            dict_max_values: 0,
            ..Config::default()
        };
        assert!(crate::narrow::for_column(array.as_ref(), &no_dict).is_none());
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Const);
        assert_eq!(chunk.const_len, 3);
        assert_eq!(&chunk.min[..3], &b"abc"[..]);
        assert!(chunk.body.is_none());
    }
}
