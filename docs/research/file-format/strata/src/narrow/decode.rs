//! Decoding one narrow chunk back to an Arrow array: the inverse of `encode.rs`.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayData, ArrayRef, BinaryArray, DictionaryArray, ListArray, MapArray, StringArray,
    StringBuilder, StructArray, UInt32Array, make_array, new_null_array,
};
use arrow::buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{ArrowNativeType, DataType, Field, UInt32Type};

use super::bitpack::{for_each_packed, unpack, unpack_u32};
use super::lengths::{decode_lengths, offsets_of, total_of};
use super::{
    BlockEntry, Class, Dictionary, Encoding, Numeric, fits, flags, is_utf8_map, numeric_of,
    unsupported,
};
use crate::error::{Result, StrataError};
use crate::format::{
    Cursor, MAX_CHUNK_ELEMENTS, MAX_CHUNK_OUTPUT, MAX_CHUNK_ROWS, le_u64, read_ipc_stream,
};

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
        DataType::Map(..) => {
            if entry.encoding != Encoding::Map || !is_utf8_map(dt) {
                return Err(corrupt("encoding for type"));
            }
            decode_map(&mut c, n, nulls, dt, at)
        }
        DataType::Boolean => {
            if entry.encoding != Encoding::Bool {
                return Err(corrupt("encoding for type"));
            }
            let bytes = Buffer::from(c.bytes(n.div_ceil(8), "bool values")?);
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
    let batches = read_ipc_stream(body).map(|(_, batches)| batches);
    let [batch] = batches.as_deref().unwrap_or_default() else {
        return Err(corrupt("ipc chunk"));
    };
    match batch.columns() {
        [column] if batch.num_rows() == n && column.data_type() == field.data_type() => {
            Ok(Arc::clone(column))
        }
        _ => Err(corrupt("ipc chunk")),
    }
}

/// A `VARBIN` chunk of Utf8 or Binary: the validity bitmap if flagged, a lengths block of the
/// `n` value lengths (null slots have length 0), then the bytes. Utf8 bytes are checked once
/// with `simdutf8`.
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
    let lens = decode_lengths(c, n, at)?;
    check_null_lengths(&lens, nulls.as_ref(), at)?;
    // The bytes are taken before the offsets are built, so the offsets are bounded by the body.
    let total = total_of(&lens, "varbin bytes", at)?;
    let bytes = c.bytes(total, "varbin bytes")?;
    expect_end(c, at)?;
    // Invariant: the offsets start at 0 and never decrease, and the total fits the body, so
    // `OffsetBuffer::new` cannot panic.
    let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets_of(&lens, "varbin offsets", at)?));
    match dt {
        DataType::Utf8 => {
            // `StringArray::try_new` would check the bytes again with `std::str::from_utf8`,
            // several times slower than `simdutf8` on non-ASCII text. The bytes are checked
            // once here, with the character boundary at each offset.
            let text = simdutf8::basic::from_utf8(bytes).map_err(|_| corrupt("varbin utf8"))?;
            let on_boundaries = offsets
                .iter()
                .all(|&o| usize::try_from(o).is_ok_and(|o| text.is_char_boundary(o)));
            if !on_boundaries {
                return Err(corrupt("varbin utf8"));
            }
            // The binary constructor checks the offsets and the nulls, but not UTF-8.
            let values = Buffer::from_vec(bytes.to_vec());
            let (offsets, values, nulls) = BinaryArray::try_new(offsets, values, nulls)
                .map_err(|_| corrupt("varbin values"))?
                .into_parts();
            // SAFETY: the offsets and nulls are those of a valid `BinaryArray`, the values are
            // valid UTF-8 (checked above), and every offset is a character boundary of them,
            // so each value is valid UTF-8.
            #[allow(unsafe_code)]
            let array = unsafe { StringArray::new_unchecked(offsets, values, nulls) };
            Ok(Arc::new(array))
        }
        DataType::Binary => {
            let values = Buffer::from_vec(bytes.to_vec());
            let array = BinaryArray::try_new(offsets, values, nulls)
                .map_err(|_| corrupt("varbin values"))?;
            Ok(Arc::new(array))
        }
        _ => Err(corrupt("encoding for type")),
    }
}

/// The null rule of the lengths-based encodings: a null slot has length 0.
fn check_null_lengths(lens: &[u32], nulls: Option<&NullBuffer>, at: u64) -> Result<()> {
    for (i, len) in lens.iter().enumerate() {
        let valid = nulls.is_none_or(|v| v.is_valid(i));
        if !valid && *len != 0 {
            return Err(StrataError::Corrupt {
                what: "null length",
                offset: at,
            });
        }
    }
    Ok(())
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
    values: Buffer,
    at: u64,
) -> Result<ArrayRef> {
    let data = ArrayData::builder(dt.clone())
        .len(n)
        .add_buffer(values)
        .nulls(nulls)
        .build()
        .map_err(|_| StrataError::Corrupt {
            what: "chunk values",
            offset: at,
        })?;
    Ok(make_array(data))
}

/// Unpacks `n` values of `width` bits from `packed` into a buffer of `T`, one `cast` each.
fn unpack_typed<T: ArrowNativeType>(
    packed: &[u8],
    n: usize,
    width: u8,
    mut cast: impl FnMut(u64) -> T,
) -> Result<Buffer> {
    let mut values: Vec<T> = Vec::with_capacity(n);
    for_each_packed(packed, n, width, |delta| values.push(cast(delta)))?;
    Ok(Buffer::from_vec(values))
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
    build_array(dt, bits.len(), nulls, Buffer::from_vec(values), at)
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
    match (entry.encoding, num.class) {
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
            expect_end(c, at)?;
            // One pass: unpack, add the base, check the range, and write the value at its width.
            let mut fits_all = true;
            let mut check = |v: u64| {
                fits_all &= fits(v, num.width, num.signed);
                v
            };
            // Each cast keeps the low bytes of the value, which is its little-endian encoding at
            // that width when the value fits.
            let values = match num.width {
                1 => unpack_typed(packed, n, width, |d| check(base.wrapping_add(d)) as u8),
                2 => unpack_typed(packed, n, width, |d| check(base.wrapping_add(d)) as u16),
                4 => unpack_typed(packed, n, width, |d| check(base.wrapping_add(d)) as u32),
                8 => unpack_typed(packed, n, width, |d| check(base.wrapping_add(d))),
                _ => return Err(corrupt("numeric width")),
            }
            .map_err(|_| corrupt("bitpack values"))?;
            if !fits_all {
                return Err(corrupt("value out of range"));
            }
            build_array(dt, n, nulls, values, at)
        }
        (Encoding::Plain, Class::Wide | Class::Float | Class::Int) => {
            // Plain values are stored at the type's width, little-endian: the bytes are the
            // Arrow buffer. Every value at the type's width is in range.
            let len = n
                .checked_mul(num.width)
                .ok_or_else(|| corrupt("plain values"))?;
            let raw = c.bytes(len, "plain values")?;
            expect_end(c, at)?;
            build_array(dt, n, nulls, Buffer::from(raw), at)
        }
        _ => Err(corrupt("encoding for type")),
    }
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
    build_array(dt, n, nulls, Buffer::from_vec(values), at)
}

/// Decodes a chunk for a scan: like [`decode_chunk`], except that a top-level Utf8 column with
/// a dictionary comes out as `Dictionary(UInt32, Utf8)` over the dictionary's shared values, so
/// no row's string is copied. Every chunk of such a column has that type, whatever its
/// encoding, so the chunks concatenate. A Utf8 column without a dictionary stays Utf8.
pub fn decode_chunk_view(
    entry: &BlockEntry,
    body: Option<&[u8]>,
    field: &Field,
    dict: Option<&Dictionary>,
) -> Result<ArrayRef> {
    let Some(dict) = dict.filter(|_| field.data_type() == &DataType::Utf8) else {
        return decode_chunk(entry, body, field, dict);
    };
    let at = entry.segment_offset;
    if entry.encoding != Encoding::Dict {
        // Every chunk shares the file dictionary's values, so a scan's dictionary arrays all
        // hold the same sorted values (predicate evaluation relies on it).
        let array = decode_chunk(entry, body, field, Some(dict))?;
        let strings = array
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| StrataError::Corrupt {
                what: "encoding for type",
                offset: at,
            })?;
        let keys: Vec<u32> = (0..strings.len())
            .map(|i| {
                if strings.is_null(i) {
                    return Ok(0);
                }
                dict.code_of(strings.value(i).as_bytes())
                    .ok_or_else(|| StrataError::Corrupt {
                        what: "value outside the dictionary",
                        offset: at,
                    })
            })
            .collect::<Result<_>>()?;
        let keys = UInt32Array::new(keys.into(), strings.nulls().cloned());
        return Ok(Arc::new(DictionaryArray::<UInt32Type>::try_new(
            keys,
            dict.values_array(),
        )?));
    }
    if entry.n_rows > MAX_CHUNK_ROWS {
        return Err(StrataError::Corrupt {
            what: "chunk rows",
            offset: at,
        });
    }
    let n = entry.n_rows as usize;
    let body = body.ok_or_else(|| StrataError::Corrupt {
        what: "chunk body",
        offset: at,
    })?;
    let mut c = Cursor::new(body, at);
    let nulls = if entry.flags & flags::HAS_NULLS != 0 {
        Some(read_validity(&mut c, n)?)
    } else {
        None
    };
    let width = c.u8("dictionary width")?;
    let rest = c.remaining();
    let packed = c.bytes(rest, "dictionary codes")?;
    expect_end(&c, at)?;
    let corrupt_codes = || StrataError::Corrupt {
        what: "dictionary codes",
        offset: at,
    };
    let keys = unpack_u32(packed, n, width).map_err(|_| corrupt_codes())?;
    // Null rows carry a code too, which must be in the dictionary.
    if keys
        .iter()
        .copied()
        .max()
        .is_some_and(|k| k as usize >= dict.len())
    {
        return Err(StrataError::Corrupt {
            what: "dictionary code",
            offset: at,
        });
    }
    let keys = UInt32Array::new(keys.into(), nulls);
    Ok(Arc::new(DictionaryArray::<UInt32Type>::try_new(
        keys,
        dict.values_array(),
    )?))
}

/// The dictionary a `Dict` or `ListDict` chunk needs.
fn require_dict(dict: Option<&Dictionary>, at: u64) -> Result<&Dictionary> {
    dict.ok_or_else(|| StrataError::Corrupt {
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

/// The value of a code, as a string. A code outside the dictionary is corrupt. The dictionary
/// holds only valid UTF-8, so the value needs no check here.
fn code_str(dict: &Dictionary, code: u64, at: u64) -> Result<&str> {
    u32::try_from(code)
        .ok()
        .and_then(|code| dict.str_value(code))
        .ok_or_else(|| StrataError::Corrupt {
            what: "dictionary code",
            offset: at,
        })
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

/// A `ListDict` chunk of List(Utf8): a lengths block of the `n` element counts, then the codes
/// of all elements. A null list has zero elements.
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

    let counts = decode_lengths(c, n, at)?;
    check_null_lengths(&counts, nulls.as_ref(), at)?;
    let total = total_of(&counts, "list elements", at)?;
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

    // Invariant: the offsets start at 0 and never decrease, and the total is the number of
    // elements, so `OffsetBuffer::new` cannot panic.
    let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets_of(&counts, "list offsets", at)?));
    let list = ListArray::try_new(Arc::clone(child), offsets, Arc::new(values), nulls)
        .map_err(|_| corrupt("list values"))?;
    Ok(Arc::new(list))
}

/// A `Map` chunk of Map<Utf8, Utf8>, in the layout of `encode_map`. Every count is checked
/// before the bytes it covers are taken from the body, and the output limit is checked before
/// anything is allocated.
fn decode_map(
    c: &mut Cursor<'_>,
    n: usize,
    nulls: Option<NullBuffer>,
    dt: &DataType,
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let DataType::Map(entries_field, sorted) = dt else {
        return Err(corrupt("encoding for type"));
    };
    let DataType::Struct(fields) = entries_field.data_type() else {
        return Err(corrupt("encoding for type"));
    };

    let counts = decode_lengths(c, n, at)?;
    check_null_lengths(&counts, nulls.as_ref(), at)?;
    let entries = total_of(&counts, "map entries", at)?;
    if entries > MAX_CHUNK_ELEMENTS {
        return Err(corrupt("map entries"));
    }

    let key_lens = decode_lengths(c, entries, at)?;
    let key_total = total_of(&key_lens, "map keys", at)?;
    let key_bytes = c.bytes(key_total, "map keys")?;

    let value_nulls = c.u8("map value nulls")?;
    let value_valid = match value_nulls {
        0 => None,
        1 => {
            let bits = c.bytes(entries.div_ceil(8), "map value validity")?.to_vec();
            Some(NullBuffer::new(BooleanBuffer::new(
                Buffer::from_vec(bits),
                0,
                entries,
            )))
        }
        _ => return Err(corrupt("map value nulls")),
    };
    let value_lens = decode_lengths(c, entries, at)?;
    check_null_lengths(&value_lens, value_valid.as_ref(), at)?;
    let value_total = total_of(&value_lens, "map values", at)?;
    if key_total
        .checked_add(value_total)
        .is_none_or(|sum| sum > MAX_CHUNK_OUTPUT)
    {
        return Err(corrupt_output(at));
    }
    let value_bytes = c.bytes(value_total, "map values")?;
    expect_end(c, at)?;

    // Invariant: every offsets vector starts at 0, never decreases and ends at its child's
    // length, so the constructors below cannot panic.
    let keys = StringArray::try_new(
        OffsetBuffer::new(ScalarBuffer::from(offsets_of(&key_lens, "map keys", at)?)),
        Buffer::from_vec(key_bytes.to_vec()),
        None,
    )
    .map_err(|_| corrupt("map utf8"))?;
    let values = StringArray::try_new(
        OffsetBuffer::new(ScalarBuffer::from(offsets_of(
            &value_lens,
            "map values",
            at,
        )?)),
        Buffer::from_vec(value_bytes.to_vec()),
        value_valid,
    )
    .map_err(|_| corrupt("map utf8"))?;
    let structs =
        StructArray::try_new(fields.clone(), vec![Arc::new(keys), Arc::new(values)], None)
            .map_err(|_| corrupt("map entries"))?;
    let map = MapArray::try_new(
        Arc::clone(entries_field),
        OffsetBuffer::new(ScalarBuffer::from(offsets_of(&counts, "map offsets", at)?)),
        structs,
        nulls,
        *sorted,
    )
    .map_err(|_| corrupt("map values"))?;
    Ok(Arc::new(map))
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
            build_array(dt, n, None, Buffer::from_vec(vec![byte; n.div_ceil(8)]), at)
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
            build_array(dt, n, None, Buffer::from_vec(value.repeat(n)), at)
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
    use crate::narrow::lengths::encode_lengths;
    use crate::narrow::{EncodedChunk, encode_chunk, for_column};
    use crate::testgen::{assert_same, batch, wide_batch};
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

    /// Encodes `values` as a Binary `Varbin` chunk and decodes the body as Utf8.
    fn varbin_as_utf8(values: Vec<Option<&[u8]>>) -> Result<ArrayRef> {
        let array: ArrayRef = Arc::new(BinaryArray::from(values));
        let chunk = encode_chunk(&array, None, &Config::default()).unwrap();
        assert_eq!(chunk.encoding, Encoding::Varbin);
        let entry = BlockEntry {
            first_row: 0,
            n_rows: array.len() as u32,
            segment_offset: 8,
            segment_len: chunk.body.as_ref().map_or(0, |b| b.len() as u32 + 16),
            encoding: chunk.encoding,
            flags: chunk.flags,
            const_len: chunk.const_len,
            min: chunk.min,
            max: chunk.max,
        };
        let field = Field::new("c", DataType::Utf8, true);
        decode_chunk(&entry, chunk.body.as_deref(), &field, None)
    }

    #[test]
    fn varbin_utf8_keeps_values_and_nulls() {
        let decoded = varbin_as_utf8(vec![
            Some("é".as_bytes()),
            None,
            Some(&b"x"[..]),
            Some(&b""[..]),
        ])
        .unwrap();
        let expected: ArrayRef = Arc::new(StringArray::from(vec![
            Some("é"),
            None,
            Some("x"),
            Some(""),
        ]));
        assert_same(decoded.as_ref(), expected.as_ref());
    }

    #[test]
    fn varbin_utf8_rejects_invalid_bytes() {
        let err = varbin_as_utf8(vec![Some(&b"ok"[..]), Some(&b"\xff"[..])]).unwrap_err();
        assert!(matches!(
            err,
            StrataError::Corrupt {
                what: "varbin utf8",
                ..
            }
        ));
    }

    #[test]
    fn varbin_utf8_rejects_a_value_that_splits_a_character() {
        // The bytes of "é" are valid together, but each value holds half of the character.
        let err = varbin_as_utf8(vec![Some(&b"\xc3"[..]), Some(&b"\xa9"[..])]).unwrap_err();
        assert!(matches!(
            err,
            StrataError::Corrupt {
                what: "varbin utf8",
                ..
            }
        ));
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
    fn list_dict_lengths_past_the_element_limit_are_corrupt() {
        let sample: ArrayRef = Arc::new(StringArray::from(vec!["a"]));
        let dict = dict_of(&sample).unwrap();
        // One list of MAX_CHUNK_ELEMENTS + 1 elements, with width 0 and no codes.
        let mut body = Vec::new();
        encode_lengths(&[MAX_CHUNK_ELEMENTS as u32 + 1], &mut body);
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
            encoding in 0u8..=11,
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
        fn maps_round_trip_as_map(
            rows in proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(
                    (".{0,6}", proptest::option::of(".{0,6}")),
                    0..4,
                )),
                0..300,
            ),
        ) {
            let chunk = round_trip_with(map_of(&rows), None);
            if rows.iter().any(|row| row.is_some()) {
                assert_eq!(chunk.encoding, Encoding::Map);
            }
        }

        #[test]
        fn varbin_round_trips_with_empty_and_long_values(
            v in proptest::collection::vec(proptest::option::of(".{0,12}"), 0..300),
            b in proptest::collection::vec(proptest::option::of(proptest::collection::vec(any::<u8>(), 0..40)), 0..300),
        ) {
            let mut text = v;
            text.push(Some("x".repeat(64 * 1024)));
            round_trip_with(Arc::new(StringArray::from_iter(text)), None);
            let mut bytes = b;
            bytes.push(Some(vec![7u8; 64 * 1024]));
            round_trip_with(Arc::new(BinaryArray::from_iter(bytes)), None);
        }

        #[test]
        fn list_dict_round_trips_null_empty_and_single_element_lists(
            rows in proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(".{0,6}", 0..2)),
                0..300,
            ),
        ) {
            let array = list_of_utf8(&rows);
            let dict = dict_of(&array);
            round_trip_with(array, dict.as_ref());
        }

        #[test]
        fn map_round_trips_with_null_and_empty_keys_and_values(
            rows in proptest::collection::vec(
                proptest::option::of(proptest::collection::vec(
                    (".{0,6}", proptest::option::of(".{0,6}")),
                    0..50,
                )),
                0..200,
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
        fn random_varbin_ipc_and_map_bodies_never_panic(
            pick in 0usize..3,
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
            let encoding = match pick {
                0 => Encoding::Varbin,
                1 => Encoding::Ipc,
                _ => Encoding::Map,
            };
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
    fn varbin_lengths_whose_sum_exceeds_the_bytes_are_corrupt() {
        // Two values of 3 bytes each, but only 3 bytes follow the lengths block.
        let mut body = Vec::new();
        encode_lengths(&[3, 3], &mut body);
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
            Err(StrataError::Corrupt { .. })
        ));
    }

    #[test]
    fn varbin_null_slot_with_a_length_is_corrupt() {
        // Row 1 is null (bit 1 clear) but has length 1.
        let mut body = vec![0b01u8];
        encode_lengths(&[3, 1], &mut body);
        body.extend_from_slice(b"abcd");
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 2,
            segment_offset: 8,
            segment_len: body.len() as u32 + 16,
            encoding: Encoding::Varbin,
            flags: flags::HAS_NULLS,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new("c", DataType::Utf8, true);
        assert!(matches!(
            decode_chunk(&entry, Some(&body), &field, None),
            Err(StrataError::Corrupt {
                what: "null length",
                ..
            })
        ));
    }

    #[test]
    fn map_with_a_bad_value_nulls_byte_is_corrupt() {
        // One map with one entry ("k", null), then a value-nulls byte of 2.
        let mut body = Vec::new();
        encode_lengths(&[1], &mut body);
        encode_lengths(&[1], &mut body);
        body.push(b'k');
        body.push(2);
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 1,
            segment_offset: 8,
            segment_len: body.len() as u32 + 16,
            encoding: Encoding::Map,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new("c", map_of(&[]).data_type().clone(), true);
        assert!(matches!(
            decode_chunk(&entry, Some(&body), &field, None),
            Err(StrataError::Corrupt {
                what: "map value nulls",
                ..
            })
        ));
    }

    #[test]
    fn map_on_a_non_map_encoding_is_corrupt() {
        let entry = BlockEntry {
            first_row: 0,
            n_rows: 1,
            segment_offset: 8,
            segment_len: 16,
            encoding: Encoding::Varbin,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let field = Field::new("c", map_of(&[]).data_type().clone(), true);
        assert!(matches!(
            decode_chunk(&entry, Some(&[0u8; 8][..]), &field, None),
            Err(StrataError::Corrupt {
                what: "encoding for type",
                ..
            })
        ));
    }

    #[test]
    fn wide_batch_columns_round_trip() {
        let full = wide_batch(2_000);
        for column in full.columns() {
            round_trip(Arc::clone(column));
        }
    }

    proptest! {
        #[test]
        fn random_map_bodies_never_panic(
            n_rows in 0u32..2_000,
            entry_flags in any::<u8>(),
            body in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let entry = BlockEntry {
                first_row: 0,
                n_rows,
                segment_offset: 8,
                segment_len: body.len() as u32 + 16,
                encoding: Encoding::Map,
                flags: entry_flags,
                const_len: 0,
                min: [0; 16],
                max: [0; 16],
            };
            let field = Field::new("c", map_of(&[]).data_type().clone(), true);
            let _ = decode_chunk(&entry, Some(&body), &field, None);
        }
    }
}
