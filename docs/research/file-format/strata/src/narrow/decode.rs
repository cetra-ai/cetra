//! Decoding one narrow chunk back to an Arrow array: the inverse of `encode.rs`.

use std::sync::Arc;

use arrow::array::{
    ArrayData, ArrayRef, BinaryArray, ListArray, StringArray, StringBuilder, make_array,
    new_null_array,
};
use arrow::buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field};
use arrow::ipc::reader::StreamReader;

use super::bitpack::unpack;
use super::{
    BlockEntry, Class, Dictionary, Encoding, Numeric, fits, flags, numeric_of, unsupported,
};
use crate::error::{Result, StrataError};
use crate::format::{Cursor, MAX_CHUNK_ELEMENTS, MAX_CHUNK_OUTPUT, MAX_CHUNK_ROWS, le_u64};

/// Decodes `entry` with its body (`None` for `Const` and `ConstNull`) into an array of
/// `field`'s type. `dict` is the column's dictionary when it has one: `Dict` and `ListDict`
/// need it, and so does a `Const` of Utf8, whose value is then a code. The entry's
/// `segment_offset` is used only in error offsets.
pub fn decode_chunk(
    entry: &BlockEntry,
    body: Option<&[u8]>,
    field: &Field,
    dict: Option<&Dictionary>,
) -> Result<ArrayRef> {
    let at = entry.segment_offset;
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    if entry.n_rows > MAX_CHUNK_ROWS {
        return Err(corrupt("chunk rows"));
    }
    let n = entry.n_rows as usize;
    let dt = field.data_type();
    match entry.encoding {
        Encoding::ConstNull => return Ok(new_null_array(dt, n)),
        Encoding::Const => return decode_const(entry, dt, n, dict),
        _ => {}
    }

    let body = body.ok_or_else(|| corrupt("chunk body"))?;
    if entry.encoding == Encoding::Ipc {
        return decode_ipc(entry, body, field, n, at);
    }
    let mut c = Cursor::new(body, at);
    let nulls = if entry.flags & flags::HAS_NULLS != 0 {
        Some(read_validity(&mut c, n)?)
    } else {
        None
    };
    match dt {
        DataType::Utf8 if entry.encoding == Encoding::Varbin => {
            decode_varbin(entry, &mut c, n, nulls, dt, at)
        }
        DataType::Utf8 => decode_utf8(entry, &mut c, n, nulls, dict, at),
        DataType::Binary => decode_varbin(entry, &mut c, n, nulls, dt, at),
        DataType::List(child) if child.data_type() == &DataType::Utf8 => {
            decode_list_utf8(entry, &mut c, n, nulls, child, dict, at)
        }
        DataType::Boolean => {
            if entry.encoding != Encoding::Bool {
                return Err(corrupt("encoding for type"));
            }
            let bytes = c.bytes(n.div_ceil(8), "bool values")?.to_vec();
            expect_end(&c, at)?;
            build_array(dt, n, nulls, bytes, at)
        }
        DataType::FixedSizeBinary(w) => {
            let width = usize::try_from(*w).map_err(|_| unsupported(dt))?;
            decode_fsb(entry, &mut c, n, width, nulls, dt, at)
        }
        _ => {
            let num = numeric_of(dt).ok_or_else(|| corrupt("encoding for type"))?;
            decode_numeric(entry, num, &mut c, n, nulls, dt, at)
        }
    }
}

/// An `IPC` chunk: an Arrow IPC stream of one batch holding one column of `field`'s type
/// and `n` rows. It must not carry the validity flag, since the stream holds the nulls.
fn decode_ipc(
    entry: &BlockEntry,
    body: &[u8],
    field: &Field,
    n: usize,
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    if entry.flags & flags::HAS_NULLS != 0 {
        return Err(corrupt("ipc flags"));
    }
    let mut reader = StreamReader::try_new(body, None).map_err(|_| corrupt("ipc chunk"))?;
    let batch = match reader.next() {
        Some(Ok(batch)) => batch,
        _ => return Err(corrupt("ipc chunk")),
    };
    if reader.next().is_some() {
        return Err(corrupt("ipc chunk"));
    }
    match batch.columns() {
        [column] if batch.num_rows() == n && column.data_type() == field.data_type() => {
            Ok(Arc::clone(column))
        }
        _ => Err(corrupt("ipc chunk")),
    }
}

/// A `VARBIN` chunk of Utf8 or Binary: the validity bitmap if flagged, `u32 offsets[n+1]`
/// (zero length for null slots), then the bytes. Utf8 values are checked to be UTF-8.
fn decode_varbin(
    entry: &BlockEntry,
    c: &mut Cursor<'_>,
    n: usize,
    nulls: Option<NullBuffer>,
    dt: &DataType,
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    if entry.encoding != Encoding::Varbin {
        return Err(corrupt("encoding for type"));
    }
    let mut offsets: Vec<usize> = Vec::with_capacity(n + 1);
    for _ in 0..=n {
        offsets.push(c.u32("varbin offsets")? as usize);
    }
    if offsets.first() != Some(&0) {
        return Err(corrupt("varbin offsets"));
    }
    for (i, pair) in offsets.windows(2).enumerate() {
        let [start, end] = pair else {
            return Err(corrupt("varbin offsets"));
        };
        if start > end {
            return Err(corrupt("varbin offsets"));
        }
        let valid = nulls.as_ref().is_none_or(|v| v.is_valid(i));
        if !valid && start != end {
            return Err(corrupt("varbin null length"));
        }
    }
    let total = offsets.last().copied().unwrap_or_default();
    let bytes = c.bytes(total, "varbin bytes")?;
    expect_end(c, at)?;
    if matches!(dt, DataType::Utf8) {
        for pair in offsets.windows(2) {
            let [start, end] = pair else {
                return Err(corrupt("varbin offsets"));
            };
            let value = bytes
                .get(*start..*end)
                .ok_or_else(|| corrupt("varbin offsets"))?;
            if std::str::from_utf8(value).is_err() {
                return Err(corrupt("varbin utf8"));
            }
        }
    }
    let offsets = offsets
        .iter()
        .map(|o| i32::try_from(*o).map_err(|_| corrupt("varbin offsets")))
        .collect::<Result<Vec<i32>>>()?;
    // Invariant: `offsets` has n + 1 entries, starts at 0, never decreases and fits in i32
    // (all checked above), so `OffsetBuffer::new` cannot panic.
    let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets));
    let values = Buffer::from_vec(bytes.to_vec());
    match dt {
        DataType::Utf8 => {
            let array = StringArray::try_new(offsets, values, nulls)
                .map_err(|_| corrupt("varbin values"))?;
            Ok(Arc::new(array))
        }
        DataType::Binary => {
            let array = BinaryArray::try_new(offsets, values, nulls)
                .map_err(|_| corrupt("varbin values"))?;
            Ok(Arc::new(array))
        }
        _ => Err(corrupt("encoding for type")),
    }
}

/// Reads the validity bitmap of `n` rows.
fn read_validity(c: &mut Cursor<'_>, n: usize) -> Result<NullBuffer> {
    let bytes = c.bytes(n.div_ceil(8), "validity bitmap")?.to_vec();
    Ok(NullBuffer::new(BooleanBuffer::new(
        Buffer::from_vec(bytes),
        0,
        n,
    )))
}

/// Checks that the body has no bytes left after the encoding's payload.
fn expect_end(c: &Cursor<'_>, at: u64) -> Result<()> {
    if c.remaining() == 0 {
        Ok(())
    } else {
        Err(StrataError::Corrupt {
            what: "chunk trailing bytes",
            offset: at,
        })
    }
}

/// Builds the array from its value buffer, checking the buffer's length against the type.
fn build_array(
    dt: &DataType,
    n: usize,
    nulls: Option<NullBuffer>,
    values: Vec<u8>,
    at: u64,
) -> Result<ArrayRef> {
    let data = ArrayData::builder(dt.clone())
        .len(n)
        .add_buffer(Buffer::from_vec(values))
        .nulls(nulls)
        .build()
        .map_err(|_| StrataError::Corrupt {
            what: "chunk values",
            offset: at,
        })?;
    Ok(make_array(data))
}

/// Builds a numeric array from one physical value per row, checking that each value fits.
fn build_numeric(
    dt: &DataType,
    num: Numeric,
    bits: &[u64],
    nulls: Option<NullBuffer>,
    at: u64,
) -> Result<ArrayRef> {
    let mut values = Vec::with_capacity(bits.len() * num.width);
    for v in bits {
        if !fits(*v, num.width, num.signed) {
            return Err(StrataError::Corrupt {
                what: "value out of range",
                offset: at,
            });
        }
        let le = v.to_le_bytes();
        values.extend_from_slice(le.get(..num.width).unwrap_or_default());
    }
    build_array(dt, bits.len(), nulls, values, at)
}

fn decode_numeric(
    entry: &BlockEntry,
    num: Numeric,
    c: &mut Cursor<'_>,
    n: usize,
    nulls: Option<NullBuffer>,
    dt: &DataType,
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let bits: Vec<u64> = match (entry.encoding, num.class) {
        (Encoding::ForBitpack, Class::Int) => {
            if entry.flags & flags::HAS_ZONE == 0 {
                return Err(corrupt("chunk zone"));
            }
            let base = entry
                .min
                .get(..8)
                .and_then(le_u64)
                .ok_or_else(|| corrupt("chunk zone"))?;
            let width = c.u8("bitpack width")?;
            let rest = c.remaining();
            let packed = c.bytes(rest, "bitpack values")?;
            let deltas = unpack(packed, n, width).map_err(|_| corrupt("bitpack values"))?;
            deltas.iter().map(|d| base.wrapping_add(*d)).collect()
        }
        (Encoding::Plain, Class::Wide | Class::Float) => {
            let len = n
                .checked_mul(num.width)
                .ok_or_else(|| corrupt("plain values"))?;
            let raw = c.bytes(len, "plain values")?;
            raw.chunks_exact(num.width)
                .map(le_u64)
                .collect::<Option<Vec<u64>>>()
                .ok_or_else(|| corrupt("plain values"))?
        }
        _ => return Err(corrupt("encoding for type")),
    };
    expect_end(c, at)?;
    build_numeric(dt, num, &bits, nulls, at)
}

fn decode_fsb(
    entry: &BlockEntry,
    c: &mut Cursor<'_>,
    n: usize,
    width: usize,
    nulls: Option<NullBuffer>,
    dt: &DataType,
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let values = match entry.encoding {
        Encoding::FsbRaw => {
            let len = n.checked_mul(width).ok_or_else(|| corrupt("fsb values"))?;
            c.bytes(len, "fsb values")?.to_vec()
        }
        Encoding::FsbRle => {
            let n_runs = usize::from(c.u16("rle runs")?);
            if n_runs == 0 {
                return Err(corrupt("rle runs"));
            }
            let mut lens = Vec::with_capacity(n_runs);
            for _ in 0..n_runs {
                lens.push(usize::from(c.u16("rle run length")?));
            }
            let value_len = n_runs
                .checked_mul(width)
                .ok_or_else(|| corrupt("rle values"))?;
            let run_values = c.bytes(value_len, "rle values")?;
            let total = lens
                .iter()
                .fold(0usize, |acc, len| acc.saturating_add(*len));
            if total != n {
                return Err(corrupt("rle length"));
            }
            // Checked before the output is allocated.
            let out_len = n
                .checked_mul(width)
                .filter(|len| *len <= MAX_CHUNK_OUTPUT)
                .ok_or_else(|| corrupt("chunk output size"))?;
            let mut out = Vec::with_capacity(out_len);
            for (k, len) in lens.iter().enumerate() {
                let start = k * width;
                let value = run_values
                    .get(start..start + width)
                    .ok_or_else(|| corrupt("rle values"))?;
                for _ in 0..*len {
                    out.extend_from_slice(value);
                }
            }
            out
        }
        _ => return Err(corrupt("encoding for type")),
    };
    expect_end(c, at)?;
    build_array(dt, n, nulls, values, at)
}

/// The dictionary a `Dict` or `ListDict` chunk needs.
fn require_dict(dict: Option<&Dictionary>, at: u64) -> Result<&Dictionary> {
    dict.ok_or(StrataError::Corrupt {
        what: "chunk dictionary",
        offset: at,
    })
}

/// Reads the packed codes: a width byte, then the codes of `n` values.
fn read_codes(c: &mut Cursor<'_>, n: usize, at: u64) -> Result<Vec<u64>> {
    let width = c.u8("dictionary width")?;
    let rest = c.remaining();
    let packed = c.bytes(rest, "dictionary codes")?;
    unpack(packed, n, width).map_err(|_| StrataError::Corrupt {
        what: "dictionary codes",
        offset: at,
    })
}

/// The value of a code, as a string. A code outside the dictionary is corrupt.
fn code_str(dict: &Dictionary, code: u64, at: u64) -> Result<&str> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let value = u32::try_from(code)
        .ok()
        .and_then(|code| dict.value(code))
        .ok_or_else(|| corrupt("dictionary code"))?;
    std::str::from_utf8(value).map_err(|_| corrupt("dictionary utf8"))
}

/// A `Dict` chunk of Utf8: one code per row. Null rows also carry a code, which must be
/// in the dictionary too.
fn decode_utf8(
    entry: &BlockEntry,
    c: &mut Cursor<'_>,
    n: usize,
    nulls: Option<NullBuffer>,
    dict: Option<&Dictionary>,
    at: u64,
) -> Result<ArrayRef> {
    if entry.encoding != Encoding::Dict {
        return Err(StrataError::Corrupt {
            what: "encoding for type",
            offset: at,
        });
    }
    let dict = require_dict(dict, at)?;
    let codes = read_codes(c, n, at)?;

    // First pass: every code is in the dictionary, and the value bytes fit the output limit.
    // Nothing is allocated for the output before this passes.
    let mut total = 0usize;
    for (i, code) in codes.iter().enumerate() {
        let value = code_str(dict, *code, at)?;
        if nulls.as_ref().is_none_or(|valid| valid.is_valid(i)) {
            total = total
                .checked_add(value.len())
                .ok_or_else(|| corrupt_output(at))?;
        }
    }
    if total > MAX_CHUNK_OUTPUT {
        return Err(corrupt_output(at));
    }

    let mut builder = StringBuilder::with_capacity(n, total);
    for (i, code) in codes.into_iter().enumerate() {
        let value = code_str(dict, code, at)?;
        if nulls.as_ref().is_none_or(|valid| valid.is_valid(i)) {
            builder.append_value(value);
        } else {
            builder.append_null();
        }
    }
    Ok(Arc::new(builder.finish()))
}

/// The error for a chunk whose decoded values would exceed `MAX_CHUNK_OUTPUT`.
fn corrupt_output(at: u64) -> StrataError {
    StrataError::Corrupt {
        what: "chunk output size",
        offset: at,
    }
}

/// A `ListDict` chunk of List(Utf8): `n + 1` list offsets, then the codes of all elements.
/// A null list has zero length.
fn decode_list_utf8(
    entry: &BlockEntry,
    c: &mut Cursor<'_>,
    n: usize,
    nulls: Option<NullBuffer>,
    child: &Arc<Field>,
    dict: Option<&Dictionary>,
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    if entry.encoding != Encoding::ListDict {
        return Err(corrupt("encoding for type"));
    }
    let dict = require_dict(dict, at)?;

    let mut offsets: Vec<usize> = Vec::new();
    for _ in 0..=n {
        offsets.push(c.u32("list offsets")? as usize);
    }
    if offsets.first() != Some(&0) {
        return Err(corrupt("list offsets"));
    }
    for (i, pair) in offsets.windows(2).enumerate() {
        let [start, end] = pair else {
            return Err(corrupt("list offsets"));
        };
        if start > end {
            return Err(corrupt("list offsets"));
        }
        let valid = nulls.as_ref().is_none_or(|v| v.is_valid(i));
        if !valid && start != end {
            return Err(corrupt("list null length"));
        }
    }
    let total = offsets.last().copied().unwrap_or_default();
    if total > MAX_CHUNK_ELEMENTS {
        return Err(corrupt("list elements"));
    }

    let codes = read_codes(c, total, at)?;
    // First pass: every code is in the dictionary, and the value bytes fit the output limit.
    let mut bytes = 0usize;
    for code in &codes {
        let value = code_str(dict, *code, at)?;
        bytes = bytes
            .checked_add(value.len())
            .ok_or_else(|| corrupt_output(at))?;
    }
    if bytes > MAX_CHUNK_OUTPUT {
        return Err(corrupt_output(at));
    }
    let mut builder = StringBuilder::with_capacity(total, bytes);
    for code in codes {
        builder.append_value(code_str(dict, code, at)?);
    }
    let values: StringArray = builder.finish();

    let offsets = offsets
        .iter()
        .map(|o| i32::try_from(*o).map_err(|_| corrupt("list offsets")))
        .collect::<Result<Vec<i32>>>()?;
    // Invariant: `offsets` is non-empty, starts at 0 and never decreases, checked above, so
    // `OffsetBuffer::new` cannot panic.
    let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets));
    let list = ListArray::try_new(Arc::clone(child), offsets, Arc::new(values), nulls)
        .map_err(|_| corrupt("list values"))?;
    Ok(Arc::new(list))
}

/// A `Const` chunk: its one value repeated `n` times. For Utf8 with a dictionary the value is
/// a code, in the first four bytes of `min`. Without one, a Utf8 or Binary value is inline,
/// `const_len` bytes of `min`.
fn decode_const(
    entry: &BlockEntry,
    dt: &DataType,
    n: usize,
    dict: Option<&Dictionary>,
) -> Result<ArrayRef> {
    let at = entry.segment_offset;
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let len = usize::from(entry.const_len);
    match dt {
        DataType::Utf8 if dict.is_some() => {
            if len != 4 {
                return Err(corrupt("const length"));
            }
            let dict = require_dict(dict, at)?;
            let code = entry
                .min
                .get(..4)
                .and_then(le_u64)
                .ok_or_else(|| corrupt("const value"))?;
            let value = code_str(dict, code, at)?;
            let total = value
                .len()
                .checked_mul(n)
                .filter(|total| *total <= MAX_CHUNK_OUTPUT)
                .ok_or_else(|| corrupt_output(at))?;
            let mut builder = StringBuilder::with_capacity(n, total);
            for _ in 0..n {
                builder.append_value(value);
            }
            Ok(Arc::new(builder.finish()))
        }
        DataType::Utf8 | DataType::Binary => {
            let value = entry
                .min
                .get(..len)
                .ok_or_else(|| corrupt("const length"))?;
            if matches!(dt, DataType::Utf8) {
                let text = std::str::from_utf8(value).map_err(|_| corrupt("const utf8"))?;
                Ok(Arc::new(StringArray::from(vec![text; n])))
            } else {
                Ok(Arc::new(BinaryArray::from_iter_values(
                    std::iter::repeat_n(value, n),
                )))
            }
        }
        DataType::List(_) => Err(corrupt("encoding for type")),
        DataType::Boolean => {
            if len != 1 {
                return Err(corrupt("const length"));
            }
            let byte = match entry.min.first().copied() {
                Some(0) => 0u8,
                Some(1) => 0xFF,
                _ => return Err(corrupt("const value")),
            };
            build_array(dt, n, None, vec![byte; n.div_ceil(8)], at)
        }
        DataType::FixedSizeBinary(w) => {
            let width = usize::try_from(*w).map_err(|_| unsupported(dt))?;
            if len != width {
                return Err(corrupt("const length"));
            }
            let value = entry
                .min
                .get(..width)
                .ok_or_else(|| corrupt("const value"))?;
            build_array(dt, n, None, value.repeat(n), at)
        }
        _ => {
            let num = numeric_of(dt).ok_or_else(|| unsupported(dt))?;
            let expected = match num.class {
                Class::Float => num.width,
                Class::Int | Class::Wide => 8,
            };
            if len != expected {
                return Err(corrupt("const length"));
            }
            let bits = entry
                .min
                .get(..len)
                .and_then(le_u64)
                .ok_or_else(|| corrupt("const value"))?;
            build_numeric(dt, num, &vec![bits; n], None, at)
        }
    }
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
    use crate::config::Config;
    use crate::narrow::{EncodedChunk, encode_chunk, for_column};
    use crate::testgen::{assert_same, batch};
    use arrow::array::{
        Array, BooleanArray, FixedSizeBinaryArray, Float32Array, Float64Array, Int8Array,
        Int16Array, Int32Array, Int64Array, ListBuilder, MapBuilder, StringArray, StringBuilder,
        StructArray, TimestampNanosecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
    };
    use arrow::datatypes::{Fields, TimeUnit};
    use proptest::prelude::*;
    use std::sync::Arc;

    /// The dictionary of all rows of `array`. The codec tests lift the writer's size limits,
    /// so that random arrays of any size have one.
    fn dict_of(array: &ArrayRef) -> Option<Dictionary> {
        let config = Config {
            dict_max_values: usize::MAX,
            dict_max_bytes: usize::MAX,
            ..Config::default()
        };
        for_column(array.as_ref(), &config)
    }

    /// Encodes `array` with its own dictionary, decodes it with the field of the same type and
    /// compares the result.
    fn round_trip(array: ArrayRef) -> EncodedChunk {
        let dict = dict_of(&array);
        round_trip_with(array, dict.as_ref())
    }

    /// Encodes `array` with `dict`, decodes it and compares the result.
    fn round_trip_with(array: ArrayRef, dict: Option<&Dictionary>) -> EncodedChunk {
        let chunk = encode_chunk(&array, dict, &Config::default()).unwrap();
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
        let decoded = decode_chunk(&entry, chunk.body.as_deref(), &field, dict).unwrap();
        assert_eq!(decoded.data_type(), array.data_type());
        assert_same(decoded.as_ref(), array.as_ref());
        chunk
    }

    /// A List(Utf8) array with the field the writer's schema gives it, from rows that are
    /// `None` (a null list) or a list of values.
    fn list_of_utf8(rows: &[Option<Vec<String>>]) -> ArrayRef {
        let mut builder = ListBuilder::new(StringBuilder::new()).with_field(Arc::new(Field::new(
            "item",
            DataType::Utf8,
            true,
        )));
        for row in rows {
            match row {
                Some(elements) => {
                    for element in elements {
                        builder.values().append_value(element);
                    }
                    builder.append(true);
                }
                None => builder.append(false),
            }
        }
        Arc::new(builder.finish())
    }

    macro_rules! int_round_trip {
        ($name:ident, $ty:ty, $array:ty) => {
            proptest! {
                #[test]
                fn $name(v in proptest::collection::vec(proptest::option::of(any::<$ty>()), 0..5_000)) {
                    let array: ArrayRef = Arc::new(<$array>::from_iter(v));
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
                    let decoded = decode_chunk(&entry, chunk.body.as_deref(), &field, None).unwrap();
                    assert_same(decoded.as_ref(), array.as_ref());
                }
            }
        };
    }

    int_round_trip!(int8_round_trips, i8, Int8Array);
    int_round_trip!(int16_round_trips, i16, Int16Array);
    int_round_trip!(int32_round_trips, i32, Int32Array);
    int_round_trip!(int64_round_trips, i64, Int64Array);
    int_round_trip!(uint8_round_trips, u8, UInt8Array);
    int_round_trip!(uint16_round_trips, u16, UInt16Array);
    int_round_trip!(uint32_round_trips, u32, UInt32Array);
    int_round_trip!(uint64_round_trips, u64, UInt64Array);

    proptest! {
        #[test]
        fn float64_round_trips_bitwise(
            v in proptest::collection::vec(proptest::option::of(any::<u64>().prop_map(f64::from_bits)), 0..5_000),
        ) {
            round_trip(Arc::new(Float64Array::from_iter(v)));
        }

        #[test]
        fn float32_round_trips_bitwise(
            v in proptest::collection::vec(proptest::option::of(any::<u32>().prop_map(f32::from_bits)), 0..5_000),
        ) {
            round_trip(Arc::new(Float32Array::from_iter(v)));
        }

        #[test]
        fn booleans_round_trip(v in proptest::collection::vec(proptest::option::of(any::<bool>()), 0..5_000)) {
            round_trip(Arc::new(BooleanArray::from_iter(v)));
        }

        #[test]
        fn fixed_size_binary_16_round_trips(
            v in proptest::collection::vec(proptest::option::of(any::<[u8; 16]>()), 0..5_000),
        ) {
            let array = FixedSizeBinaryArray::try_from_sparse_iter_with_size(v.into_iter(), 16).unwrap();
            round_trip(Arc::new(array));
        }

        #[test]
        fn fixed_size_binary_runs_round_trip(
            v in proptest::collection::vec(proptest::option::of(proptest::sample::select(vec![[0u8; 4], [1u8; 4], [2u8; 4]])), 0..5_000),
        ) {
            let array = FixedSizeBinaryArray::try_from_sparse_iter_with_size(v.into_iter(), 4).unwrap();
            round_trip(Arc::new(array));
        }

    }

    /// The number of array kinds that [`typed_array`] draws.
    const SLICED_KINDS: usize = 13;

    /// A random array of `rows` rows with nulls, of the kind with index `kind`:
    /// 0 Int64, 1 Int16, 2 UInt32, 3 Float64, 4 Float32, 5 Timestamp(ns, UTC), 6 Boolean,
    /// 7 FixedSizeBinary(16), 8 Utf8 (with a dictionary), 9 Utf8 (without one), 10 Binary,
    /// 11 List(Utf8) (with a dictionary), 12 Map(Utf8, Utf8 nullable).
    fn typed_array(kind: usize, rows: usize) -> BoxedStrategy<ArrayRef> {
        match kind {
            0 => proptest::collection::vec(proptest::option::of(any::<i64>()), rows)
                .prop_map(|v| Arc::new(Int64Array::from_iter(v)) as ArrayRef)
                .boxed(),
            1 => proptest::collection::vec(proptest::option::of(any::<i16>()), rows)
                .prop_map(|v| Arc::new(Int16Array::from_iter(v)) as ArrayRef)
                .boxed(),
            2 => proptest::collection::vec(proptest::option::of(any::<u32>()), rows)
                .prop_map(|v| Arc::new(UInt32Array::from_iter(v)) as ArrayRef)
                .boxed(),
            3 => proptest::collection::vec(
                proptest::option::of(any::<u64>().prop_map(f64::from_bits)),
                rows,
            )
            .prop_map(|v| Arc::new(Float64Array::from_iter(v)) as ArrayRef)
            .boxed(),
            4 => proptest::collection::vec(
                proptest::option::of(any::<u32>().prop_map(f32::from_bits)),
                rows,
            )
            .prop_map(|v| Arc::new(Float32Array::from_iter(v)) as ArrayRef)
            .boxed(),
            5 => proptest::collection::vec(proptest::option::of(any::<i64>()), rows)
                .prop_map(|v| {
                    Arc::new(TimestampNanosecondArray::from_iter(v).with_timezone("UTC"))
                        as ArrayRef
                })
                .boxed(),
            6 => proptest::collection::vec(proptest::option::of(any::<bool>()), rows)
                .prop_map(|v| Arc::new(BooleanArray::from_iter(v)) as ArrayRef)
                .boxed(),
            7 => proptest::collection::vec(proptest::option::of(any::<[u8; 16]>()), rows)
                .prop_map(|v| {
                    let array =
                        FixedSizeBinaryArray::try_from_sparse_iter_with_size(v.into_iter(), 16)
                            .unwrap();
                    Arc::new(array) as ArrayRef
                })
                .boxed(),
            8 | 9 => proptest::collection::vec(proptest::option::of(".{0,12}"), rows)
                .prop_map(|v| Arc::new(StringArray::from_iter(v)) as ArrayRef)
                .boxed(),
            10 => proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40)),
                rows,
            )
            .prop_map(|v| Arc::new(BinaryArray::from_iter(v)) as ArrayRef)
            .boxed(),
            11 => proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(".{0,6}", 0..6)),
                rows,
            )
            .prop_map(|v| list_of_utf8(&v))
            .boxed(),
            _ => proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(
                    (".{0,6}", proptest::option::of(".{0,6}")),
                    0..4,
                )),
                rows,
            )
            .prop_map(|v| map_of(&v))
            .boxed(),
        }
    }

    /// One sliced-array case: the kind, the whole array, and the slice's start and length.
    /// The start is at most 69, and the length fits in the rest of the array.
    fn sliced_case() -> impl Strategy<Value = (usize, ArrayRef, usize, usize)> {
        (0usize..SLICED_KINDS, 0usize..300)
            .prop_flat_map(|(kind, rows)| (Just(kind), typed_array(kind, rows), 0usize..70))
            .prop_flat_map(|(kind, full, skip_draw)| {
                let rows = full.len();
                let skip = skip_draw.min(rows);
                (Just(kind), Just(full), Just(skip), 0usize..=(rows - skip))
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn sliced_arrays_round_trip((kind, full, skip, len) in sliced_case()) {
            // The dictionary is built from the whole array, so a slice uses a part of it.
            let dict = if matches!(kind, 8 | 11) { dict_of(&full) } else { None };
            round_trip_with(full.slice(skip, len), dict.as_ref());
        }
    }

    #[test]
    fn list_dict_offsets_past_the_element_limit_are_corrupt() {
        let sample: ArrayRef = Arc::new(StringArray::from(vec!["a"]));
        let dict = dict_of(&sample).unwrap();
        // One list whose offsets end at MAX_CHUNK_ELEMENTS + 1, with width 0 and no codes.
        let mut body = Vec::new();
        for offset in [0u32, MAX_CHUNK_ELEMENTS as u32 + 1] {
            body.extend_from_slice(&offset.to_le_bytes());
        }
        body.push(0);
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 1,
            segment_offset: 8,
            segment_len: body.len() as u32 + 16,
            encoding: Encoding::ListDict,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new(
            "c",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        );
        assert!(matches!(
            decode_chunk(&entry, Some(&body), &field, Some(&dict)),
            Err(StrataError::Corrupt { .. })
        ));
    }

    #[test]
    fn dict_chunk_whose_values_exceed_the_output_limit_is_corrupt() {
        let sample: ArrayRef = Arc::new(StringArray::from(vec!["x".repeat(64 * 1024)]));
        let dict = dict_of(&sample).unwrap();
        // 65,535 rows, all code 0: width 0 and no packed codes.
        let body = vec![0u8];
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 65_535,
            segment_offset: 8,
            segment_len: body.len() as u32 + 16,
            encoding: Encoding::Dict,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new("c", DataType::Utf8, true);
        assert!(matches!(
            decode_chunk(&entry, Some(&body), &field, Some(&dict)),
            Err(StrataError::Corrupt {
                what: "chunk output size",
                ..
            })
        ));
    }

    #[test]
    fn const_chunk_whose_value_exceeds_the_output_limit_is_corrupt() {
        let sample: ArrayRef = Arc::new(StringArray::from(vec!["x".repeat(64 * 1024)]));
        let dict = dict_of(&sample).unwrap();
        // The value is code 0, repeated 65,535 times.
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 65_535,
            segment_offset: 0,
            segment_len: 0,
            encoding: Encoding::Const,
            flags: 0,
            const_len: 4,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new("c", DataType::Utf8, true);
        assert!(matches!(
            decode_chunk(&entry, None, &field, Some(&dict)),
            Err(StrataError::Corrupt {
                what: "chunk output size",
                ..
            })
        ));
    }

    #[test]
    fn all_null_chunks_round_trip() {
        for dt in [
            DataType::Int64,
            DataType::Float64,
            DataType::Boolean,
            DataType::FixedSizeBinary(3),
        ] {
            let array = arrow::array::new_null_array(&dt, 17);
            assert_eq!(round_trip(array).encoding, Encoding::ConstNull);
        }
    }

    #[test]
    fn all_equal_chunks_are_const() {
        let chunk = round_trip(Arc::new(Int64Array::from(vec![42; 9])));
        assert_eq!(chunk.encoding, Encoding::Const);
        assert!(chunk.body.is_none());
    }

    #[test]
    fn single_row_round_trips() {
        round_trip(Arc::new(Int32Array::from(vec![-3])));
        round_trip(Arc::new(Int32Array::from(vec![None])));
    }

    #[test]
    fn min_and_max_in_one_chunk_use_width_64() {
        let array = Int64Array::from(vec![i64::MIN, 0, i64::MAX, 5]);
        let chunk = round_trip(Arc::new(array));
        assert_eq!(chunk.encoding, Encoding::ForBitpack);
        assert_eq!(chunk.body.unwrap()[0], 64);
    }

    #[test]
    fn nan_and_signed_zero_round_trip_bitwise() {
        let values = vec![f64::NAN, -0.0, 0.0, 1.5, f64::NAN];
        let chunk = round_trip(Arc::new(Float64Array::from(values)));
        assert_eq!(chunk.encoding, Encoding::Plain);
        assert_ne!(chunk.flags & flags::HAS_NAN, 0);
    }

    #[test]
    fn timestamp_keeps_its_timezone() {
        let array = TimestampNanosecondArray::from(vec![1_700_000_000_000_000_000i64, 3, 9])
            .with_timezone("UTC");
        let chunk = round_trip(Arc::new(array.clone()));
        assert_eq!(chunk.encoding, Encoding::ForBitpack);
        let field = Field::new("c", array.data_type().clone(), true);
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 3,
            segment_offset: 8,
            segment_len: 0,
            encoding: chunk.encoding,
            flags: chunk.flags,
            const_len: chunk.const_len,
            min: chunk.min,
            max: chunk.max,
        };
        let decoded = decode_chunk(&entry, chunk.body.as_deref(), &field, None).unwrap();
        assert_eq!(decoded.data_type(), array.data_type());
    }

    #[test]
    fn long_fixed_size_binary_runs_pick_rle() {
        let rows = (0..1000).map(|i| if i < 900 { [1u8; 16] } else { [2u8; 16] });
        let array =
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(rows.map(Some), 16).unwrap();
        assert_eq!(round_trip(Arc::new(array)).encoding, Encoding::FsbRle);
    }

    #[test]
    fn distinct_fixed_size_binary_values_pick_raw() {
        let rows = (0..100u8).map(|i| Some([i; 16]));
        let array = FixedSizeBinaryArray::try_from_sparse_iter_with_size(rows, 16).unwrap();
        assert_eq!(round_trip(Arc::new(array)).encoding, Encoding::FsbRaw);
    }

    #[test]
    fn constants_of_different_bytes_stay_distinct() {
        let a = FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            vec![Some([0x61u8, 0x00]); 3].into_iter(),
            2,
        )
        .unwrap();
        let b = FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            vec![Some([0x61u8, 0x61]); 3].into_iter(),
            2,
        )
        .unwrap();
        let a = round_trip(Arc::new(a));
        let b = round_trip(Arc::new(b));
        assert_eq!(a.encoding, Encoding::Const);
        assert_ne!(a.min, b.min);
    }

    #[test]
    fn test_batch_columns_round_trip() {
        let full = batch(2_000);
        for column in full.columns() {
            round_trip(Arc::clone(column));
        }
    }

    proptest! {
        #[test]
        fn utf8_round_trips(
            v in proptest::collection::vec(proptest::option::of(".{0,12}"), 0..5_000),
        ) {
            round_trip(Arc::new(StringArray::from_iter(v)));
        }

        #[test]
        fn list_of_utf8_round_trips(
            rows in proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(".{0,8}", 0..40)),
                0..1_000,
            ),
        ) {
            round_trip(list_of_utf8(&rows));
        }
    }

    #[test]
    fn utf8_constant_chunk_is_const_with_a_code() {
        let chunk = round_trip(Arc::new(StringArray::from(vec!["x"; 9])));
        assert_eq!(chunk.encoding, Encoding::Const);
        assert_eq!(chunk.const_len, 4);
        assert!(chunk.body.is_none());
    }

    #[test]
    fn empty_and_nul_suffixed_strings_stay_distinct() {
        let array = StringArray::from(vec![Some("a"), Some("a\0"), Some(""), None, Some("a")]);
        assert_eq!(round_trip(Arc::new(array)).encoding, Encoding::Dict);
        let array = StringArray::from(vec!["a", "a\0", "a", "a\0"]);
        round_trip(Arc::new(array));
    }

    #[test]
    fn long_list_round_trips_as_list_dict() {
        let long: Vec<String> = (0..3_000).map(|i| format!("v{}", i % 17)).collect();
        let rows = vec![Some(vec![]), None, Some(long), Some(vec!["v1".to_string()])];
        let chunk = round_trip(list_of_utf8(&rows));
        assert_eq!(chunk.encoding, Encoding::ListDict);
    }

    proptest! {
        #[test]
        fn random_dictionary_bodies_never_panic(
            list in any::<bool>(),
            n_rows in 0u32..5_000,
            entry_flags in any::<u8>(),
            min in any::<[u8; 16]>(),
            max in any::<[u8; 16]>(),
            body in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let sample: ArrayRef = Arc::new(StringArray::from(vec!["alpha", "beta", "gamma"]));
            let dict = dict_of(&sample).unwrap();
            let (encoding, field) = if list {
                (
                    Encoding::ListDict,
                    Field::new("c", DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))), true),
                )
            } else {
                (Encoding::Dict, Field::new("c", DataType::Utf8, true))
            };
            let entry = BlockEntry {
                first_row: 0,
                n_rows,
                segment_offset: 8,
                segment_len: body.len() as u32 + 16,
                encoding,
                flags: entry_flags,
                const_len: 4,
                min,
                max,
            };
            let _ = decode_chunk(&entry, Some(&body), &field, Some(&dict));
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn random_entries_and_bodies_never_panic(
            encoding in 0u8..=10,
            n_rows in 0u32..70_000,
            const_len in 0u16..40,
            entry_flags in any::<u8>(),
            min in any::<[u8; 16]>(),
            max in any::<[u8; 16]>(),
            kind in 0usize..13,
            with_dict in any::<bool>(),
            structured in any::<bool>(),
            width in 0u8..=64,
            raw in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            if let Some(encoding) = Encoding::from_u8(encoding) {
            let dt = match kind {
                0 => DataType::Int8,
                1 => DataType::Int64,
                2 => DataType::UInt64,
                3 => DataType::Float32,
                4 => DataType::Float64,
                5 => DataType::Boolean,
                6 => DataType::Timestamp(arrow::datatypes::TimeUnit::Nanosecond, None),
                7 => DataType::FixedSizeBinary(4),
                8 => DataType::FixedSizeBinary(16),
                9 => DataType::UInt32,
                10 => DataType::Utf8,
                11 => DataType::Binary,
                _ => DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            };
            // Half the bodies are random bytes. The other half have a validity bitmap, a
            // width byte and the packed byte count for `n_rows` codes, with random contents.
            let n = n_rows as usize;
            let fill = |len: usize| -> Vec<u8> {
                (0..len)
                    .map(|i| raw.get(i % raw.len().max(1)).copied().unwrap_or(0))
                    .collect()
            };
            let (body, entry_flags) = if structured {
                let mut body = fill(n.div_ceil(8));
                body.push(width);
                body.extend(fill((n * usize::from(width)).div_ceil(8)));
                (body, entry_flags | flags::HAS_NULLS)
            } else {
                (raw.clone(), entry_flags)
            };
            let sample: ArrayRef = Arc::new(StringArray::from(vec!["alpha", "beta", "gamma"]));
            let dict = if with_dict { dict_of(&sample) } else { None };
            let entry = BlockEntry {
                first_row: 0,
                n_rows,
                segment_offset: 8,
                segment_len: body.len() as u32 + 16,
                encoding,
                flags: entry_flags,
                const_len,
                min,
                max,
            };
            let field = Field::new("c", dt, true);
            let _ = decode_chunk(&entry, Some(&body), &field, dict.as_ref());
            }
        }
    }

    /// The fields of one `events` item, as the spans schema has them.
    fn event_fields() -> Fields {
        Fields::from(vec![
            Field::new(
                "time",
                DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                false,
            ),
            Field::new("name", DataType::Utf8, false),
            Field::new("attributes", DataType::Utf8, true),
        ])
    }

    /// An events-shaped List(Struct) array with one list per row of `rows`.
    fn events_of(rows: &[Vec<(i64, String, Option<String>)>]) -> ArrayRef {
        let mut lengths = Vec::with_capacity(rows.len());
        let mut times = Vec::new();
        let mut names = Vec::new();
        let mut attributes: Vec<Option<String>> = Vec::new();
        for row in rows {
            lengths.push(row.len());
            for (time, name, attribute) in row {
                times.push(*time);
                names.push(name.clone());
                attributes.push(attribute.clone());
            }
        }
        let fields = event_fields();
        let structs = StructArray::try_new(
            fields.clone(),
            vec![
                Arc::new(TimestampNanosecondArray::from(times).with_timezone("UTC")),
                Arc::new(StringArray::from(names)),
                Arc::new(StringArray::from(attributes)),
            ],
            None,
        )
        .unwrap();
        Arc::new(
            ListArray::try_new(
                Arc::new(Field::new("item", DataType::Struct(fields), false)),
                OffsetBuffer::from_lengths(lengths),
                Arc::new(structs),
                None,
            )
            .unwrap(),
        )
    }

    /// A Map(Utf8, Utf8) array: `None` rows are null maps, and values may be null.
    /// One Map row: `None` is a null map, otherwise its entries.
    type MapRow = Option<Vec<(String, Option<String>)>>;

    fn map_of(rows: &[MapRow]) -> ArrayRef {
        let mut builder = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        for row in rows {
            match row {
                Some(entries) => {
                    for (key, value) in entries {
                        builder.keys().append_value(key);
                        builder.values().append_option(value.as_deref());
                    }
                    builder.append(true).unwrap();
                }
                None => builder.append(false).unwrap(),
            }
        }
        Arc::new(builder.finish())
    }

    /// A List(Utf8) array whose elements may be null.
    fn list_with_null_elements(rows: &[Option<Vec<Option<String>>>]) -> ArrayRef {
        let mut builder = ListBuilder::new(StringBuilder::new()).with_field(Arc::new(Field::new(
            "item",
            DataType::Utf8,
            true,
        )));
        for row in rows {
            match row {
                Some(elements) => {
                    for element in elements {
                        builder.values().append_option(element.as_deref());
                    }
                    builder.append(true);
                }
                None => builder.append(false),
            }
        }
        Arc::new(builder.finish())
    }

    proptest! {
        #[test]
        fn utf8_without_a_dictionary_round_trips(
            v in proptest::collection::vec(proptest::option::of(".{0,12}"), 0..5_000),
        ) {
            round_trip_with(Arc::new(StringArray::from_iter(v)), None);
        }

        #[test]
        fn binary_round_trips(
            v in proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40)),
                0..5_000,
            ),
        ) {
            round_trip_with(Arc::new(BinaryArray::from_iter(v)), None);
        }

        #[test]
        fn events_round_trip_as_ipc(
            rows in proptest::collection::vec(
                proptest::collection::vec((any::<i64>(), ".{0,6}", proptest::option::of(".{0,6}")), 0..4),
                0..300,
            ),
        ) {
            let chunk = round_trip_with(events_of(&rows), None);
            if !rows.is_empty() {
                assert_eq!(chunk.encoding, Encoding::Ipc);
            }
        }

        #[test]
        fn maps_round_trip_as_ipc(
            rows in proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(
                    (".{0,6}", proptest::option::of(".{0,6}")),
                    0..4,
                )),
                0..300,
            ),
        ) {
            round_trip_with(map_of(&rows), None);
        }

        #[test]
        fn lists_with_null_elements_round_trip(
            rows in proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(proptest::option::of(".{0,6}"), 0..6)),
                0..300,
            ),
        ) {
            let array = list_with_null_elements(&rows);
            let dict = dict_of(&array);
            round_trip_with(array, dict.as_ref());
        }

        #[test]
        fn random_varbin_and_ipc_bodies_never_panic(
            ipc in any::<bool>(),
            kind in 0usize..4,
            n_rows in 0u32..2_000,
            entry_flags in any::<u8>(),
            const_len in 0u16..40,
            min in any::<[u8; 16]>(),
            max in any::<[u8; 16]>(),
            body in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let dt = match kind {
                0 => DataType::Utf8,
                1 => DataType::Binary,
                2 => events_of(&[]).data_type().clone(),
                _ => map_of(&[]).data_type().clone(),
            };
            let entry = BlockEntry {
                first_row: 0,
                n_rows,
                segment_offset: 8,
                segment_len: body.len() as u32 + 16,
                encoding: if ipc { Encoding::Ipc } else { Encoding::Varbin },
                flags: entry_flags,
                const_len,
                min,
                max,
            };
            let field = Field::new("c", dt, true);
            let _ = decode_chunk(&entry, Some(&body), &field, None);
        }
    }

    #[test]
    fn inline_constants_keep_their_length() {
        // "a" and "a\0" share their zero-padded bytes, so only `const_len` tells them apart.
        let a = round_trip_with(Arc::new(StringArray::from(vec!["a"; 3])), None);
        let b = round_trip_with(Arc::new(StringArray::from(vec!["a\0"; 3])), None);
        assert_eq!(a.encoding, Encoding::Const);
        assert_eq!(a.const_len, 1);
        assert_eq!(b.const_len, 2);
    }

    #[test]
    fn a_100_kib_value_round_trips_as_varbin() {
        let big = "y".repeat(100 * 1024);
        let array = StringArray::from(vec![big.as_str(), "z"]);
        assert_eq!(
            round_trip_with(Arc::new(array), None).encoding,
            Encoding::Varbin
        );
    }

    #[test]
    fn a_64_kib_value_with_nulls_round_trips() {
        let big = "x".repeat(64 * 1024);
        let array = StringArray::from(vec![Some(big), None, Some(String::new())]);
        round_trip_with(Arc::new(array), None);
    }

    #[test]
    fn varbin_with_decreasing_offsets_is_corrupt() {
        let mut body = Vec::new();
        for offset in [0u32, 3, 2] {
            body.extend_from_slice(&offset.to_le_bytes());
        }
        body.extend_from_slice(b"abc");
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 2,
            segment_offset: 8,
            segment_len: body.len() as u32 + 16,
            encoding: Encoding::Varbin,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new("c", DataType::Utf8, true);
        assert!(matches!(
            decode_chunk(&entry, Some(&body), &field, None),
            Err(StrataError::Corrupt {
                what: "varbin offsets",
                ..
            })
        ));
    }
}
