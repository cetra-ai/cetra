//! Encoding one Arrow slice into one narrow chunk, following the S3 rules.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayData, ArrayRef, AsArray, BooleanArray, FixedSizeBinaryArray, RecordBatch,
};
use arrow::datatypes::{DataType, Field, Float64Type, Schema};
use arrow::ipc::writer::StreamWriter;

use super::bitpack::width_for;
use super::lanes::{BLOCK, Packer};
use super::lengths::encode_lengths;
use super::{
    Class, Dictionary, Encoding, Numeric, SHARED_PREFIX_MIN, SHARED_SUFFIX, flags, is_utf8_map,
    numeric_of, prefix_hash, unsupported,
};
use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{MAX_CHUNK_ELEMENTS, MAX_CHUNK_OUTPUT, MAX_CHUNK_ROWS, le_u64};

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
/// List(Utf8) chunks use it. `_config` is unused: the writer applies zstd. Its bit-packed
/// blocks are in lanes where they fit ([`flags::LANES`]).
pub fn encode_chunk(
    array: &ArrayRef,
    dict: Option<&Dictionary>,
    _config: &Config,
) -> Result<EncodedChunk> {
    encode_chunk_with(array, dict, true)
}

/// [`encode_chunk`], with the bit-packed blocks in lanes where they fit when `lanes` is set,
/// else all in order. The chunk carries `flags::LANES` when a block went in lanes.
fn encode_chunk_with(
    array: &ArrayRef,
    dict: Option<&Dictionary>,
    lanes: bool,
) -> Result<EncodedChunk> {
    let packer = Packer::new(lanes);
    let mut chunk = encode_chunk_packed(array, dict, &packer)?;
    if packer.used() {
        chunk.flags |= flags::LANES;
    }
    Ok(chunk)
}

fn encode_chunk_packed(
    array: &ArrayRef,
    dict: Option<&Dictionary>,
    packer: &Packer,
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
            Some(dict) => encode_utf8(values, dict, packer),
            None => encode_varbin(values, packer),
        },
        DataType::Binary => encode_varbin(values, packer),
        DataType::List(field) if field.data_type() == &DataType::Utf8 => {
            // A chunk with a null element inside a non-null list is `IPC`.
            let listed = match dict {
                Some(dict) => encode_list_utf8(values, dict, packer)?,
                None => None,
            };
            match listed {
                Some(chunk) => Ok(chunk),
                None => encode_ipc(array),
            }
        }
        DataType::Map(..) if is_utf8_map(dt) => encode_map(values, packer),
        DataType::Float64 if let Some(dict) = dict => encode_float_dict(values, dict, packer),
        _ => match numeric_of(dt) {
            Some(num) => encode_numeric(values, num, packer),
            None => encode_ipc(array),
        },
    }
}

/// The candidate encodings of one chunk, fastest to decode first; the writer stores a later one
/// only when its stored segment is at least 30% smaller. An integer chunk has two: `ForBitpack`,
/// and `Plain`, whose values zstd can still compress (bit-packing hides repeated values from
/// it). A `FloatDict` chunk has `Plain` as its second, for a chunk whose values zstd compresses
/// better than their codes. Every other chunk has the one [`encode_chunk`] gives. A chunk with a
/// block in lanes ([`flags::LANES`]) has the same chunk packed in order right after it: lanes
/// hide repeated values from zstd more than packing in order does.
pub fn encode_chunk_candidates(
    array: &ArrayRef,
    dict: Option<&Dictionary>,
    config: &Config,
) -> Result<Vec<EncodedChunk>> {
    let chunk = encode_chunk(array, dict, config)?;
    let num = || numeric_of(array.data_type()).ok_or_else(|| unsupported(array.data_type()));
    let last = match chunk.encoding {
        Encoding::ForBitpack => Some(plain_int(array.as_ref(), num()?, &chunk)?),
        Encoding::FloatDict => Some(encode_numeric(array.as_ref(), num()?, &Packer::new(false))?),
        _ => None,
    };
    // A chunk of steps between nearby values: slower to decode than lanes, faster than zstd.
    let delta = match chunk.encoding {
        Encoding::ForBitpack => encode_delta(array.as_ref(), num()?)?,
        _ => None,
    };
    let in_order = match chunk.flags & flags::LANES {
        0 => None,
        _ => Some(encode_chunk_with(array, dict, false)?),
    };
    Ok(std::iter::once(chunk)
        .chain(delta)
        .chain(in_order)
        .chain(last)
        .collect())
}

/// The `Plain` encoding of the integer chunk that `bitpacked` encodes: the same validity,
/// zone and flags, and each value at the type's width, little-endian (null slots are 0).
fn plain_int(array: &dyn Array, num: Numeric, bitpacked: &EncodedChunk) -> Result<EncodedChunk> {
    let data = array.to_data();
    let values = value_bytes_of(&data, array.data_type(), array.len(), num.width)?;
    let nulls = array.nulls();
    let mut body = Vec::with_capacity(values.len() + array.len().div_ceil(8));
    if bitpacked.flags & flags::HAS_NULLS != 0 {
        body.extend(validity_bitmap(array));
    }
    for (i, row) in values.chunks_exact(num.width.max(1)).enumerate() {
        if nulls.is_none_or(|nb| nb.is_valid(i)) {
            body.extend_from_slice(row);
        } else {
            body.extend(std::iter::repeat_n(0, row.len()));
        }
    }
    // A plain chunk has no packed block, so no lanes.
    Ok(EncodedChunk {
        encoding: Encoding::Plain,
        flags: bitpacked.flags & !flags::LANES,
        body: Some(body),
        ..*bitpacked
    })
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

/// The Arrow validity bitmap of `array`: bit `i` is set when row `i` is valid. Called only when
/// the array has nulls.
fn validity_bitmap(array: &dyn Array) -> Vec<u8> {
    let len = array.len();
    let nbytes = len.div_ceil(8);
    // `sliced` copies the slice's bits to bit 0, so the bytes start at row 0. Its last byte can
    // hold bits of rows past the end, which are cleared below.
    let mut bitmap = match array.nulls() {
        Some(nulls) => nulls
            .inner()
            .sliced()
            .as_slice()
            .get(..nbytes)
            .unwrap_or_default()
            .to_vec(),
        None => vec![0xFF; nbytes],
    };
    bitmap.resize(nbytes, 0);
    let rem = len % 8;
    if rem != 0
        && let Some(last) = bitmap.last_mut()
    {
        *last &= (1u8 << rem) - 1;
    }
    bitmap
}

/// The value bytes of the `n` rows of a numeric array of `width` bytes per row, read from its
/// value buffer with the array's offset applied.
fn value_bytes_of<'a>(
    data: &'a ArrayData,
    dt: &DataType,
    n: usize,
    width: usize,
) -> Result<&'a [u8]> {
    let raw = data
        .buffers()
        .first()
        .ok_or_else(|| unsupported(dt))?
        .as_slice();
    let start = data
        .offset()
        .checked_mul(width)
        .ok_or_else(|| unsupported(dt))?;
    let end = n
        .checked_mul(width)
        .and_then(|len| start.checked_add(len))
        .ok_or_else(|| unsupported(dt))?;
    raw.get(start..end).ok_or_else(|| unsupported(dt))
}

/// The physical bits of each row of `values`, which holds `num.width` bytes per row. Null rows
/// hold whatever the buffer has there, so callers check validity. Signed values are
/// sign-extended.
fn row_bits(values: &[u8], num: Numeric) -> impl Iterator<Item = u64> + '_ {
    values.chunks_exact(num.width.max(1)).map(move |row| {
        let raw = le_u64(row).unwrap_or_default();
        if num.signed {
            super::sign_extend(raw, num.width)
        } else {
            raw
        }
    })
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

/// Bytes of one patch of a `ForDelta` chunk at most: a `u16` row and a 64-bit value. Patch
/// values are bit-packed from their minimum, so most take far less.
const PATCH_BYTES: usize = 10;

/// The `ForDelta` encoding of an integer chunk without nulls of a whole number of 1,024-value
/// blocks, or `None` when it does not fit or is no smaller than `ForBitpack` in lanes. Each value
/// from row 32 on is stored as its step from the value 32 rows earlier (the same lane), less a
/// reference step, bit-packed in lanes; the steps that do not fit the width are patches.
///
/// Body: `u8 width · u64 reference · u8 first width · the first 32 values less the chunk's min,
/// packed in order · n steps in lanes (none at width 0; the first 32 are 0) · u32 patch count ·
/// u16 rows · u64 patch base · u8 patch width · the patch values less the base, packed in order`.
/// A value is `value[i - 32] + reference + step[i]`, wrapping, and a patched step is its value.
fn encode_delta(array: &dyn Array, num: Numeric) -> Result<Option<EncodedChunk>> {
    let n = array.len();
    if num.class != Class::Int
        || array.null_count() > 0
        || n < BLOCK
        || !n.is_multiple_of(BLOCK)
        || n > MAX_CHUNK_ROWS as usize
    {
        return Ok(None);
    }
    let data = array.to_data();
    let values: Vec<u64> =
        row_bits(value_bytes_of(&data, array.data_type(), n, num.width)?, num).collect();
    let (min, max) = values
        .iter()
        .map(|&b| b as i64)
        .fold((i64::MAX, i64::MIN), |(lo, hi), x| (lo.min(x), hi.max(x)));
    if min == max {
        return Ok(None);
    }
    let steps: Vec<u64> = (0..n)
        .map(|i| match i.checked_sub(32) {
            Some(back) => values
                .get(i)
                .zip(values.get(back))
                .map_or(0, |(v, b)| v.wrapping_sub(*b)),
            None => 0,
        })
        .collect();
    let (width, reference, patched) = best_delta_width(steps.get(32..).unwrap_or_default());
    let lanes_bytes = n / 8 * usize::from(width);
    let size = 1 + 8 + 1 + 32 + lanes_bytes + 4 + patched * PATCH_BYTES;
    let bitpack_bytes = n / 8 * usize::from(width_for((max as u64).wrapping_sub(min as u64)));
    if size >= bitpack_bytes {
        return Ok(None);
    }
    let base = min as u64;
    let firsts: Vec<u64> = values
        .iter()
        .take(32)
        .map(|v| v.wrapping_sub(base))
        .collect();
    let first_width = width_for(firsts.iter().copied().max().unwrap_or(0));
    let mask = if width == 0 {
        0
    } else {
        u64::MAX >> (64 - u32::from(width))
    };
    let mut body = Vec::with_capacity(size);
    body.push(width);
    body.extend_from_slice(&reference.to_le_bytes());
    body.push(first_width);
    super::bitpack::pack(&firsts, first_width, &mut body);
    let mut packed = Vec::with_capacity(n);
    let mut patches: Vec<(u16, u64)> = Vec::with_capacity(patched);
    for (i, step) in steps.iter().enumerate() {
        let rest = step.wrapping_sub(reference);
        if i < 32 {
            packed.push(0);
        } else if rest <= mask {
            packed.push(rest);
        } else {
            packed.push(0);
            // `i` is below the chunk's row count, at most 65,535.
            patches.push((i as u16, rest));
        }
    }
    if width > 0 {
        Packer::new(true).pack(&packed, width, &mut body);
    }
    body.extend_from_slice(&(patches.len() as u32).to_le_bytes());
    for (row, _) in &patches {
        body.extend_from_slice(&row.to_le_bytes());
    }
    // The patch values from their least, as signed numbers: steps that all go far down (a
    // counter that wraps) or all far up pack into a few bits each, often none.
    let patch_base = patches.iter().map(|(_, v)| *v as i64).min().unwrap_or(0) as u64;
    let offsets: Vec<u64> = patches
        .iter()
        .map(|(_, v)| v.wrapping_sub(patch_base))
        .collect();
    let patch_width = width_for(offsets.iter().copied().max().unwrap_or(0));
    body.extend_from_slice(&patch_base.to_le_bytes());
    body.push(patch_width);
    super::bitpack::pack(&offsets, patch_width, &mut body);
    let mut chunk = EncodedChunk::bare(Encoding::ForDelta, flags::HAS_ZONE);
    set_prefix(&mut chunk.min, &min.to_le_bytes());
    set_prefix(&mut chunk.max, &max.to_le_bytes());
    chunk.body = Some(body);
    Ok(Some(chunk))
}

/// The width (0 to 32), reference step and patch count that store `steps` in the fewest bytes:
/// for each width, the reference is the start of the window of `2^width` values that holds the
/// most steps, and every step outside it is a patch. Steps are compared as signed numbers.
fn best_delta_width(steps: &[u64]) -> (u8, u64, usize) {
    let mut sorted: Vec<i64> = steps.iter().map(|&s| s as i64).collect();
    sorted.sort_unstable();
    let n = sorted.len();
    let mut best = (
        0u8,
        sorted.first().copied().unwrap_or(0) as u64,
        n,
        usize::MAX,
    );
    for width in 0u8..=32 {
        let span = (1i128 << width) - 1;
        // The window starting at each step: the most steps within `span` of it.
        let (mut end, mut covered, mut start) = (0usize, 0usize, 0i64);
        for (i, &lo) in sorted.iter().enumerate() {
            while sorted
                .get(end)
                .is_some_and(|&hi| i128::from(hi) - i128::from(lo) <= span)
            {
                end += 1;
            }
            if end - i > covered {
                covered = end - i;
                start = lo;
            }
        }
        let patches = n - covered;
        let bytes = (n + 32) / 8 * usize::from(width) + patches * PATCH_BYTES;
        if bytes < best.3 {
            best = (width, start as u64, patches, bytes);
        }
    }
    (best.0, best.1, best.2)
}

fn encode_numeric(array: &dyn Array, num: Numeric, packer: &Packer) -> Result<EncodedChunk> {
    let data = array.to_data();
    let values = value_bytes_of(&data, array.data_type(), array.len(), num.width)?;
    let nulls = array.nulls();
    let is_valid = |i: usize| nulls.is_none_or(|nb| nb.is_valid(i));
    let has_nulls = array.null_count() > 0;
    let first = row_bits(values, num).next().unwrap_or_default();
    if !has_nulls && row_bits(values, num).all(|b| b == first) {
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
            for (i, b) in row_bits(values, num).enumerate() {
                if is_valid(i) {
                    let x = b as i64;
                    min = min.min(x);
                    max = max.max(x);
                }
            }
            // Every valid delta lies in 0..=max - min, and max - min is one of them, so the
            // width comes from max - min without a pass over the deltas. Null slots are 0.
            let base = min as u64;
            let width = width_for((max as u64).wrapping_sub(base));
            let deltas: Vec<u64> = row_bits(values, num)
                .enumerate()
                .map(|(i, b)| if is_valid(i) { b.wrapping_sub(base) } else { 0 })
                .collect();
            body.push(width);
            packer.pack(&deltas, width, &mut body);
            chunk.encoding = Encoding::ForBitpack;
            set_prefix(&mut chunk.min, &min.to_le_bytes());
            set_prefix(&mut chunk.max, &max.to_le_bytes());
            chunk_flags |= flags::HAS_ZONE;
        }
        Class::Wide => {
            for (i, b) in row_bits(values, num).enumerate() {
                push_le(&mut body, if is_valid(i) { b } else { 0 }, num.width);
            }
        }
        Class::Float => {
            let mut has_nan = false;
            let mut zone: Option<(f64, f64)> = None;
            for (i, b) in row_bits(values, num).enumerate() {
                if !is_valid(i) {
                    push_le(&mut body, 0, num.width);
                    continue;
                }
                push_le(&mut body, b, num.width);
                let x = float_of(b, num.width);
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

/// A Float64 chunk with a dictionary: the `Const` chunk [`encode_numeric`] gives when every
/// value is equal and non-null, else `FloatDict` with that function's validity, zone and flags,
/// and one packed code per row. Null rows hold code 0.
fn encode_float_dict(
    array: &dyn Array,
    dict: &Dictionary,
    packer: &Packer,
) -> Result<EncodedChunk> {
    let floats = array
        .as_primitive_opt::<Float64Type>()
        .ok_or_else(|| unsupported(array.data_type()))?;
    let num = numeric_of(array.data_type()).ok_or_else(|| unsupported(array.data_type()))?;
    let plain = encode_numeric(array, num, packer)?;
    if plain.encoding != Encoding::Plain {
        return Ok(plain);
    }
    let mut codes: Vec<u64> = Vec::with_capacity(floats.len());
    let mut hi = 0u32;
    for i in 0..floats.len() {
        if floats.is_valid(i) {
            let code = code_of(dict, &floats.value(i).to_bits().to_le_bytes())?;
            hi = hi.max(code);
            codes.push(u64::from(code));
        } else {
            codes.push(0);
        }
    }
    let mut body = Vec::new();
    if plain.flags & flags::HAS_NULLS != 0 {
        body.extend(validity_bitmap(array));
    }
    let width = width_for(u64::from(hi));
    body.push(width);
    packer.pack(&codes, width, &mut body);
    Ok(EncodedChunk {
        encoding: Encoding::FloatDict,
        flags: plain.flags,
        const_len: plain.const_len,
        min: plain.min,
        max: plain.max,
        body: Some(body),
    })
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
fn encode_utf8(array: &dyn Array, dict: &Dictionary, packer: &Packer) -> Result<EncodedChunk> {
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
    packer.pack(&codes, width, &mut body);
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
/// `Varbin`: the validity bitmap if there are nulls, a lengths block of the value lengths
/// (null slots have length 0), then the bytes.
///
/// A `Varbin` chunk has a zone from its lowest and highest valid value ([`set_varbin_zone`]).
fn encode_varbin(array: &dyn Array, packer: &Packer) -> Result<EncodedChunk> {
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
    let lens = rows
        .iter()
        .map(|row| u32::try_from(row.len()).map_err(|_| too_long()))
        .collect::<Result<Vec<u32>>>()?;
    encode_lengths(&lens, &mut body, packer);
    for row in &rows {
        body.extend_from_slice(row);
    }
    let mut chunk = EncodedChunk::bare(Encoding::Varbin, chunk_flags);
    // Not all null (that chunk is `ConstNull`), so there is a valid value.
    let mut valid = rows
        .iter()
        .enumerate()
        .filter(|(i, _)| array.is_valid(*i))
        .map(|(_, row)| *row);
    if let Some(first) = valid.next() {
        let (lo, hi) = valid.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v)));
        set_varbin_zone(&mut chunk, lo, hi);
    }
    chunk.body = Some(body);
    Ok(chunk)
}

/// Sets the zone of a `Varbin` chunk from its lowest and highest valid value. Every value lies
/// between them, so every value starts with their common prefix. A common prefix of at least
/// [`SHARED_PREFIX_MIN`] bytes (sorted ids often share long ones) gives the shared-prefix form
/// ([`flags::SHARED_PREFIX`]); otherwise the zone holds the first 16 bytes of each, zero-padded.
fn set_varbin_zone(chunk: &mut EncodedChunk, lo: &[u8], hi: &[u8]) {
    let shared = lo
        .iter()
        .zip(hi)
        .take_while(|(a, b)| a == b)
        .count()
        .min(255);
    chunk.flags |= flags::HAS_ZONE;
    if shared < SHARED_PREFIX_MIN {
        set_prefix(&mut chunk.min, lo.get(..16).unwrap_or(lo));
        set_prefix(&mut chunk.max, hi.get(..16).unwrap_or(hi));
        return;
    }
    chunk.flags |= flags::SHARED_PREFIX;
    let hash = prefix_hash(lo.get(..shared).unwrap_or(lo));
    for (bound, value) in [(&mut chunk.min, lo), (&mut chunk.max, hi)] {
        let rest = value.get(shared..).unwrap_or_default();
        let mut zone = vec![shared as u8];
        zone.extend_from_slice(&hash);
        zone.extend_from_slice(rest.get(..SHARED_SUFFIX).unwrap_or(rest));
        set_prefix(bound, &zone);
    }
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

/// A List(Utf8) chunk with a dictionary, as `ListDict`: a lengths block of the element counts
/// of the lists, then the packed codes of every element. Returns `None` when a non-null list
/// holds a null element, which the caller stores as `IPC`.
fn encode_list_utf8(
    array: &dyn Array,
    dict: &Dictionary,
    packer: &Packer,
) -> Result<Option<EncodedChunk>> {
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
    // The element count of each list. A null list has none.
    let mut counts: Vec<u32> = Vec::with_capacity(n);
    for (i, pair) in list.value_offsets().windows(2).enumerate() {
        let before = codes.len();
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
        let count = codes.len().saturating_sub(before);
        counts.push(u32::try_from(count).map_err(|_| StrataError::Unsupported {
            what: "a list chunk of more than 4 billion elements".to_string(),
        })?);
    }

    let mut body = Vec::new();
    let mut chunk_flags = 0u8;
    if has_nulls {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    encode_lengths(&counts, &mut body, packer);
    let width = width_for(codes.iter().copied().max().unwrap_or_default());
    body.push(width);
    packer.pack(&codes, width, &mut body);
    let mut chunk = EncodedChunk::bare(Encoding::ListDict, chunk_flags);
    chunk.body = Some(body);
    Ok(Some(chunk))
}

/// A `Map<Utf8, Utf8>` chunk, as `Map`: the validity bitmap if there are nulls, a lengths
/// block of the entry count of each map, a lengths block of the key lengths and the key bytes,
/// then the value-validity byte with its bitmap when a value is null, a lengths block of the
/// value lengths and the value bytes. Null maps have no entries, and null values have length 0.
fn encode_map(array: &dyn Array, packer: &Packer) -> Result<EncodedChunk> {
    let dt = array.data_type();
    let map = array.as_map_opt().ok_or_else(|| unsupported(dt))?;
    let entries = map.entries();
    let keys = entries
        .column(0)
        .as_string_opt::<i32>()
        .ok_or_else(|| unsupported(dt))?;
    let values = entries
        .column(1)
        .as_string_opt::<i32>()
        .ok_or_else(|| unsupported(dt))?;

    // The offsets index the whole entries array, so a sliced map reads its own range.
    let mut counts: Vec<u32> = Vec::with_capacity(map.len());
    let mut key_lens: Vec<u32> = Vec::new();
    let mut key_bytes: Vec<u8> = Vec::new();
    let mut value_lens: Vec<u32> = Vec::new();
    let mut value_bytes: Vec<u8> = Vec::new();
    let mut value_valid: Vec<bool> = Vec::new();
    for (i, pair) in map.value_offsets().windows(2).enumerate() {
        let [start, end] = pair else {
            return Err(StrataError::Schema {
                reason: "map offsets are not paired".to_string(),
            });
        };
        if !map.is_valid(i) {
            counts.push(0);
            continue;
        }
        let start = usize::try_from(*start).map_err(|_| StrataError::Schema {
            reason: "map offset is negative".to_string(),
        })?;
        let end = usize::try_from(*end).map_err(|_| StrataError::Schema {
            reason: "map offset is negative".to_string(),
        })?;
        counts.push(u32::try_from(end.saturating_sub(start)).map_err(|_| unsupported(dt))?);
        for j in start..end {
            let key = keys.value(j).as_bytes();
            key_lens.push(u32::try_from(key.len()).map_err(|_| unsupported(dt))?);
            key_bytes.extend_from_slice(key);
            let valid = values.is_valid(j);
            let value: &[u8] = if valid {
                values.value(j).as_bytes()
            } else {
                &[]
            };
            value_lens.push(u32::try_from(value.len()).map_err(|_| unsupported(dt))?);
            value_bytes.extend_from_slice(value);
            value_valid.push(valid);
        }
        if key_lens.len() > MAX_CHUNK_ELEMENTS {
            return Err(StrataError::Unsupported {
                what: format!("a map chunk of more than {MAX_CHUNK_ELEMENTS} entries"),
            });
        }
    }

    let mut body = Vec::new();
    let mut chunk_flags = 0u8;
    if map.null_count() > 0 {
        chunk_flags |= flags::HAS_NULLS;
        body.extend(validity_bitmap(array));
    }
    encode_lengths(&counts, &mut body, packer);
    encode_lengths(&key_lens, &mut body, packer);
    body.extend_from_slice(&key_bytes);
    let value_nulls = value_valid.iter().any(|valid| !valid);
    body.push(u8::from(value_nulls));
    if value_nulls {
        let mut bitmap = vec![0u8; value_valid.len().div_ceil(8)];
        for (j, valid) in value_valid.iter().enumerate() {
            if *valid {
                set_bit(&mut bitmap, j);
            }
        }
        body.extend(bitmap);
    }
    encode_lengths(&value_lens, &mut body, packer);
    body.extend_from_slice(&value_bytes);
    let mut chunk = EncodedChunk::bare(Encoding::Map, chunk_flags);
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
    use crate::narrow::BlockEntry;
    use arrow::array::{Float32Array, Float64Array, Int64Array, new_null_array};
    use std::sync::Arc;

    /// Decodes `chunk` as a column of `array`'s type and checks it against `array`.
    fn decodes_to(chunk: &EncodedChunk, array: &ArrayRef) {
        let entry = BlockEntry {
            first_row: 0,
            n_rows: array.len() as u32,
            segment_offset: 0,
            segment_len: 1,
            encoding: chunk.encoding,
            flags: chunk.flags,
            const_len: chunk.const_len,
            min: chunk.min,
            max: chunk.max,
        };
        let field = arrow::datatypes::Field::new("v", array.data_type().clone(), true);
        let decoded =
            crate::narrow::decode_chunk(&entry, chunk.body.as_deref(), &field, None).unwrap();
        assert_eq!(decoded.as_ref(), array.as_ref(), "{:?}", chunk.encoding);
    }

    #[test]
    fn delta_chunks_round_trip_with_their_patches() {
        use arrow::array::{Int8Array, Int16Array, Int32Array, TimestampNanosecondArray};
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut noise = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let arrays: Vec<ArrayRef> = vec![
            // Counting with a wrap: steps of +32, and patches where the count wraps.
            Arc::new(Int32Array::from_iter_values((0..2_048).map(|i| i % 1_000))),
            // Timestamps a few milliseconds apart, with jitter and one long pause.
            Arc::new(TimestampNanosecondArray::from_iter_values(
                (0..4_096i64).map(|i| {
                    1_700_000_000_000_000_000
                        + i * 3_000_000
                        + (noise() % 1_000_000) as i64
                        + if i > 3_000 { 60_000_000_000 } else { 0 }
                }),
            )),
            // Signed values going down, with rare outliers far away.
            Arc::new(Int64Array::from_iter_values((0..2_048i64).map(|i| {
                if i % 333 == 7 {
                    i64::MIN + i
                } else {
                    5_000 - 3 * i
                }
            }))),
            Arc::new(Int16Array::from_iter_values(
                (0..1_024).map(|i| (i * 7 - 3_000) as i16),
            )),
            Arc::new(Int8Array::from_iter_values(
                (0..1_024).map(|i| (i / 16 - 30) as i8),
            )),
        ];
        for array in arrays {
            let candidates = encode_chunk_candidates(&array, None, &Config::default()).unwrap();
            let delta = candidates
                .iter()
                .find(|c| c.encoding == Encoding::ForDelta)
                .unwrap_or_else(|| panic!("no delta candidate for {:?}", array.data_type()));
            decodes_to(delta, &array);
            for chunk in &candidates {
                decodes_to(chunk, &array);
            }
        }
    }

    #[test]
    fn a_count_with_wraps_is_mostly_patches_free_and_much_smaller() {
        use arrow::array::Int32Array;
        let array: ArrayRef = Arc::new(Int32Array::from_iter_values((0..2_048).map(|i| i % 1_000)));
        let num = numeric_of(array.data_type()).unwrap();
        let chunk = encode_delta(array.as_ref(), num).unwrap().unwrap();
        let body = chunk.body.as_ref().unwrap();
        // Width 0: every step is the reference, +32, except across the two wraps, where 32 rows
        // each step down: 64 patches.
        assert_eq!(body[0], 0);
        assert_eq!(u64::from_le_bytes(body[1..9].try_into().unwrap()), 32);
        // The tail: the count, 64 rows, the patch base and a width of 0: every wrap steps down by
        // the same amount, so the patch values take no bits.
        let patches_at = body.len() - 4 - 64 * 2 - 8 - 1;
        let count = u32::from_le_bytes(body[patches_at..patches_at + 4].try_into().unwrap());
        assert_eq!(count, 64);
        assert_eq!(body[body.len() - 1], 0);
        assert!(body.len() < 200, "{} bytes", body.len());
    }

    #[test]
    fn no_delta_for_nulls_partial_blocks_or_random_values() {
        use arrow::array::Int32Array;
        let num = numeric_of(&DataType::Int32).unwrap();
        let with_null: ArrayRef = Arc::new(Int32Array::from_iter(
            (0..1_024).map(|i| (i != 5).then_some(i)),
        ));
        assert!(encode_delta(with_null.as_ref(), num).unwrap().is_none());
        let short: ArrayRef = Arc::new(Int32Array::from_iter_values(0..1_000));
        assert!(encode_delta(short.as_ref(), num).unwrap().is_none());
        let mut x = 7u64;
        let random: ArrayRef = Arc::new(Int32Array::from_iter_values((0..2_048).map(|_| {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (x >> 40) as i32
        })));
        assert!(encode_delta(random.as_ref(), num).unwrap().is_none());
    }

    #[test]
    fn corrupt_delta_bodies_are_errors() {
        use arrow::array::Int32Array;
        let array: ArrayRef = Arc::new(Int32Array::from_iter_values((0..2_048).map(|i| i % 1_000)));
        let num = numeric_of(array.data_type()).unwrap();
        let chunk = encode_delta(array.as_ref(), num).unwrap().unwrap();
        let body = chunk.body.clone().unwrap();
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 2_048,
            segment_offset: 0,
            segment_len: 1,
            encoding: Encoding::ForDelta,
            flags: chunk.flags,
            const_len: 0,
            min: chunk.min,
            max: chunk.max,
        };
        let field = arrow::datatypes::Field::new("v", DataType::Int32, false);
        let decode = |body: &[u8]| crate::narrow::decode_chunk(&entry, Some(body), &field, None);
        assert!(decode(&body).is_ok());
        // Truncated anywhere, one byte too long, a width above 32, a patch row below 32.
        for cut in [1, 9, 10, 20, body.len() - 1] {
            assert!(decode(&body[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = body.clone();
        longer.push(0);
        assert!(decode(&longer).is_err());
        let mut wide = body.clone();
        wide[0] = 33;
        assert!(decode(&wide).is_err());
        let rows_at = body.len() - 64 * 2 - 8 - 1;
        let mut early = body.clone();
        early[rows_at..rows_at + 2].copy_from_slice(&5u16.to_le_bytes());
        assert!(decode(&early).is_err());
        // Patch rows out of order, or repeated, are an error.
        let mut repeated = body.clone();
        let second = rows_at + 2;
        let first_row: [u8; 2] = body[rows_at..rows_at + 2].try_into().unwrap();
        repeated[second..second + 2].copy_from_slice(&first_row);
        assert!(decode(&repeated).is_err());
        // Sums wrap at the column's width: a patch outside an Int32 still decodes to Int32s
        // (the segment checksum, not the decoder, catches a corrupt body).
        let mut big = body.clone();
        let base_at = body.len() - 8 - 1;
        big[base_at..base_at + 8].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(decode(&big).is_ok());
        // A patch width above 64 is an error.
        let mut wide_patch = body.clone();
        let last = wide_patch.len() - 1;
        wide_patch[last] = 65;
        assert!(decode(&wide_patch).is_err());
    }

    #[test]
    fn a_float_dict_chunk_has_a_plain_candidate() {
        let array: ArrayRef = Arc::new(Float64Array::from_iter_values(
            (0..64).map(|i| f64::from(i % 4) * 0.5),
        ));
        let dict = crate::narrow::for_column(array.as_ref(), &Config::default()).unwrap();
        let candidates = encode_chunk_candidates(&array, Some(&dict), &Config::default()).unwrap();
        let encodings: Vec<Encoding> = candidates.iter().map(|c| c.encoding).collect();
        assert_eq!(encodings, vec![Encoding::FloatDict, Encoding::Plain]);
        assert_eq!(candidates[0].min, candidates[1].min);
        assert_eq!(candidates[0].max, candidates[1].max);
        assert_eq!(candidates[0].flags, candidates[1].flags);
    }

    #[test]
    fn an_integer_chunk_has_bitpacked_and_plain_candidates_that_decode_alike() {
        use arrow::array::Int32Array;
        let arrays: [ArrayRef; 2] = [
            Arc::new(Int64Array::from(
                (0..2_000i64)
                    .map(|i| (i % 7 != 3).then_some(i * 1_000_003 - 5))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from((0..2_000).map(|i| -i).collect::<Vec<_>>())),
        ];
        for array in arrays {
            let candidates = encode_chunk_candidates(&array, None, &Config::default()).unwrap();
            let encodings: Vec<Encoding> = candidates.iter().map(|c| c.encoding).collect();
            assert_eq!(encodings, vec![Encoding::ForBitpack, Encoding::Plain]);
            let (bitpacked, plain) = (&candidates[0], &candidates[1]);
            assert_eq!(
                (plain.flags, plain.min, plain.max),
                (bitpacked.flags, bitpacked.min, bitpacked.max)
            );
            let field = arrow::datatypes::Field::new("v", array.data_type().clone(), true);
            for chunk in &candidates {
                let entry = BlockEntry {
                    first_row: 0,
                    n_rows: array.len() as u32,
                    segment_offset: 0,
                    segment_len: 1,
                    encoding: chunk.encoding,
                    flags: chunk.flags,
                    const_len: chunk.const_len,
                    min: chunk.min,
                    max: chunk.max,
                };
                let decoded =
                    crate::narrow::decode_chunk(&entry, chunk.body.as_deref(), &field, None)
                        .unwrap();
                assert_eq!(decoded.as_ref(), array.as_ref(), "{:?}", chunk.encoding);
            }
        }
    }

    #[test]
    fn a_chunk_in_lanes_has_in_order_and_plain_candidates_that_decode_alike() {
        use arrow::array::Int32Array;
        let arrays: [ArrayRef; 2] = [
            Arc::new(Int64Array::from(
                (0..2_048i64)
                    .map(|i| (i % 7 != 3).then_some(i * 1_000_003 - 5))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                (0..2_048).map(|i| i % 1_000).collect::<Vec<_>>(),
            )),
        ];
        for array in arrays {
            let candidates = encode_chunk_candidates(&array, None, &Config::default()).unwrap();
            // A delta candidate, when the values step evenly, comes right after the lanes chunk.
            let candidates: Vec<EncodedChunk> = candidates
                .into_iter()
                .filter(|c| c.encoding != Encoding::ForDelta)
                .collect();
            let encodings: Vec<Encoding> = candidates.iter().map(|c| c.encoding).collect();
            assert_eq!(
                encodings,
                vec![Encoding::ForBitpack, Encoding::ForBitpack, Encoding::Plain]
            );
            let (lanes, in_order) = (&candidates[0], &candidates[1]);
            assert_eq!(lanes.flags & flags::LANES, flags::LANES);
            assert_eq!(
                (lanes.flags & !flags::LANES, lanes.min, lanes.max),
                (in_order.flags, in_order.min, in_order.max)
            );
            assert_eq!(
                lanes.body.as_ref().map(Vec::len),
                in_order.body.as_ref().map(Vec::len)
            );
            let field = arrow::datatypes::Field::new("v", array.data_type().clone(), true);
            for chunk in &candidates {
                let entry = BlockEntry {
                    first_row: 0,
                    n_rows: array.len() as u32,
                    segment_offset: 0,
                    segment_len: 1,
                    encoding: chunk.encoding,
                    flags: chunk.flags,
                    const_len: chunk.const_len,
                    min: chunk.min,
                    max: chunk.max,
                };
                let decoded =
                    crate::narrow::decode_chunk(&entry, chunk.body.as_deref(), &field, None)
                        .unwrap();
                assert_eq!(decoded.as_ref(), array.as_ref(), "{:?}", chunk.encoding);
            }
        }
    }

    #[test]
    fn other_chunks_have_one_candidate() {
        let floats: ArrayRef = Arc::new(Float64Array::from(vec![0.5, 1.5, 2.25]));
        let constant: ArrayRef = Arc::new(Int64Array::from(vec![7; 10]));
        for array in [floats, constant] {
            let candidates = encode_chunk_candidates(&array, None, &Config::default()).unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(
                candidates[0],
                encode_chunk(&array, None, &Config::default()).unwrap()
            );
        }
    }

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
    fn map_column_is_map() {
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
        assert_eq!(chunk.encoding, Encoding::Map);
    }

    #[test]
    fn map_with_non_nullable_values_is_ipc() {
        let mut builder = arrow::array::MapBuilder::new(
            None,
            arrow::array::StringBuilder::new(),
            arrow::array::StringBuilder::new(),
        )
        .with_values_field(Arc::new(Field::new("values", DataType::Utf8, false)));
        builder.keys().append_value("k");
        builder.values().append_value("v");
        builder.append(true).unwrap();
        let array: ArrayRef = Arc::new(builder.finish());
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Ipc);
    }

    #[test]
    fn map_with_int64_values_is_ipc() {
        let mut builder = arrow::array::MapBuilder::new(
            None,
            arrow::array::StringBuilder::new(),
            arrow::array::Int64Builder::new(),
        );
        builder.keys().append_value("k");
        builder.values().append_value(1);
        builder.append(true).unwrap();
        let array: ArrayRef = Arc::new(builder.finish());
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Ipc);
    }

    #[test]
    fn list_dict_of_one_repeated_element_is_tiny() {
        // Every list has one element, so the lengths block is 5 bytes and the codes take no
        // bits: the body is the lengths block and the width byte.
        let mut builder = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        for _ in 0..4096 {
            builder.values().append_value("openinference");
            builder.append(true);
        }
        let array: ArrayRef = Arc::new(builder.finish());
        let dict = crate::narrow::for_column(array.as_ref(), &Config::default());
        let chunk = encode_chunk(&array, dict.as_ref(), &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::ListDict);
        let body = chunk.body.unwrap();
        assert!(body.len() <= 64, "{} bytes", body.len());
    }

    #[test]
    fn map_rows_repeating_six_entries_compress_to_two_kib() {
        let pairs = [
            ("telemetry.sdk.name", "opentelemetry"),
            ("telemetry.sdk.language", "python"),
            ("telemetry.sdk.version", "1.27.0"),
            ("host.name", "chat-api-3"),
            ("process.pid", "1003"),
            ("deployment.environment", "production"),
        ];
        let mut builder = arrow::array::MapBuilder::new(
            None,
            arrow::array::StringBuilder::new(),
            arrow::array::StringBuilder::new(),
        );
        for _ in 0..4096 {
            for (key, value) in pairs {
                builder.keys().append_value(key);
                builder.values().append_value(value);
            }
            builder.append(true).unwrap();
        }
        let array: ArrayRef = Arc::new(builder.finish());
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Map);
        let body = chunk.body.unwrap();
        let compressed = zstd::bulk::compress(&body, 3).unwrap();
        assert!(compressed.len() <= 2048, "{} bytes", compressed.len());
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
