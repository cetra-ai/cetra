//! Narrow scalar column encoders and decoders (`ConstNull`, `Const`, `Rle`,
//! `ForBitpack`, `DeltaBitpack`, `Alp`, `BitpackBool`, `Plain`).

use cetra_column::{
    BinaryData, Bitmap, Column, DataType, FlatColumn, FlatData, Interval, MapData, RleColumn,
    ScalarValue, Utf8Data,
};

use crate::codec::alp::{
    decode_alp_body, decode_alp_selected, decode_alp_take, encode_alp_body, validate_alp_body,
};
use crate::codec::fsst::{
    FSST_SUBCHUNK_MIN_ROWS, FsstBlockView, FsstSubchunkedView, decode_fsst_binary,
    decode_fsst_binary_selected_with_flag, decode_fsst_binary_subchunked, decode_fsst_binary_take,
    decode_fsst_utf8, decode_fsst_utf8_selected_with_flag, decode_fsst_utf8_subchunked,
    decode_fsst_utf8_take, encode_fsst_body, encode_fsst_body_adaptive,
    encode_fsst_subchunked_only,
};
use crate::codec::zonemap::{ZoneMapSummary, compute_zone_map};
use crate::codec::{EncodedMiniBlock, WriteProfile, preferred_block_codec};
use crate::format::{
    BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_CONST_EMPTY_COLLECTION, BLOCK_FLAG_FSST_SUBCHUNKED,
    BLOCK_FLAG_HAS_NULLS, BlockEntry, Codec, Encoding,
};
use crate::simd::bitpack::{
    delta_bases_len, delta_decode_i64, delta_decode_i64_indices, delta_encode_i64, min_bits_u32,
    min_bits_u64, pack_u32, pack_u64, packed_bytes_len, unpack_u32, unpack_u32_at, unpack_u64,
    unpack_u64_at, zigzag_decode_i64, zigzag_encode_i64,
};
use crate::{MAX_BLOCK_RAW_BYTES, StatamError};

/// Appends `nulls` bitmap bytes (`len.div_ceil(8)`) when `nulls` is `Some`.
pub(crate) fn write_optional_nulls(nulls: Option<&Bitmap>, out: &mut Vec<u8>) {
    if let Some(bm) = nulls {
        out.extend_from_slice(&bm.to_bytes());
    }
}

/// Splits the optional leading row null bitmap bytes without allocating a [`Bitmap`].
pub(crate) fn split_optional_null_bytes(
    src: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<(Option<&[u8]>, &[u8]), StatamError> {
    if block_flags & BLOCK_FLAG_HAS_NULLS == 0 {
        return Ok((None, src));
    }
    let byte_len = row_count.div_ceil(8);
    if src.len() < byte_len {
        return Err(StatamError::Corrupt {
            reason: "truncated mini-block null bitmap",
        });
    }
    Ok((Some(&src[..byte_len]), &src[byte_len..]))
}

#[inline]
fn null_bytes_is_valid(null_bytes: Option<&[u8]>, row: usize) -> bool {
    null_bytes.is_none_or(|nb| ((nb[row / 8] >> (row % 8)) & 1) != 0)
}

/// Reads the optional leading row null bitmap if `BLOCK_FLAG_HAS_NULLS` is set.
pub(crate) fn read_optional_nulls(
    src: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<(Option<Bitmap>, &[u8]), StatamError> {
    let (null_bytes, rest) = split_optional_null_bytes(src, row_count, block_flags)?;
    let Some(bytes) = null_bytes else {
        return Ok((None, rest));
    };
    let bm = Bitmap::from_bytes(bytes, row_count)?;
    Ok((Some(bm), rest))
}

/// Attempts to encode `col` as `ConstNull` or `Const` (0 data bytes).
pub(crate) fn try_encode_const_or_null(
    col: &Column,
    dt: &DataType,
    zm: &ZoneMapSummary,
) -> Option<EncodedMiniBlock> {
    let row_count = col.len();
    if row_count == 0 {
        return None;
    }

    if zm.block_flags & BLOCK_FLAG_ALL_NULL != 0 || matches!(dt, DataType::Null) {
        return Some(EncodedMiniBlock {
            encoding: Encoding::ConstNull,
            block_flags: zm.block_flags | BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
            null_count: zm.null_count,
            min_inline_len: 0,
            max_inline_len: 0,
            min_inline: [0u8; 16],
            max_inline: [0u8; 16],
            raw_body: Vec::new(),
            preferred_codec: Codec::None,
        });
    }

    if zm.null_count != 0 {
        return None;
    }

    // Check if the entire column is a constant scalar (<= 16 B) or empty collection.
    let const_scalar: Option<ScalarValue> = match col {
        Column::Const { value, .. } => Some(value.clone()),
        Column::Rle(r) if r.run_ends().len() == 1 => r.values().get_scalar(0).ok(),
        Column::Rle(_) => None,
        Column::Flat(f) => flat_all_equal_scalar(f),
        Column::Dict(d) => {
            let first = d.codes()[0];
            if d.codes().iter().all(|&c| c == first) {
                d.values().get_scalar(first as usize).ok()
            } else {
                None
            }
        }
    };

    let scalar = const_scalar?;
    let mut min_inline = [0u8; 16];
    let min_inline_len: u8;
    let mut max_inline = zm.max_inline;
    let mut max_inline_len = zm.max_inline_len;
    let mut block_flags = zm.block_flags;

    match &scalar {
        ScalarValue::Bool(b) => {
            min_inline[0] = u8::from(*b);
            min_inline_len = 1;
        }
        ScalarValue::I64(v) | ScalarValue::TimestampNs { value: v, .. } => {
            min_inline[0..8].copy_from_slice(&v.to_le_bytes());
            min_inline_len = 8;
        }
        ScalarValue::F64(v) => {
            min_inline[0..8].copy_from_slice(&v.to_bits().to_le_bytes());
            min_inline_len = 8;
        }
        ScalarValue::Decimal128 { value: v, .. } => {
            min_inline.copy_from_slice(&v.to_le_bytes());
            min_inline_len = 16;
        }
        ScalarValue::Date(v) => {
            min_inline[0..4].copy_from_slice(&v.to_le_bytes());
            min_inline_len = 4;
        }
        ScalarValue::FixedBytes8(b) => {
            min_inline[0..8].copy_from_slice(b);
            min_inline_len = 8;
        }
        ScalarValue::FixedBytes16(b) => {
            min_inline.copy_from_slice(b);
            min_inline_len = 16;
        }
        ScalarValue::Interval(iv) => {
            min_inline[0..4].copy_from_slice(&iv.months.to_le_bytes());
            min_inline[4..8].copy_from_slice(&iv.days.to_le_bytes());
            min_inline[8..16].copy_from_slice(&iv.nanos.to_le_bytes());
            min_inline_len = 16;
            max_inline = [0u8; 16];
            max_inline_len = 0;
        }
        ScalarValue::Utf8(s) if s.len() <= 16 => {
            min_inline[..s.len()].copy_from_slice(s.as_bytes());
            min_inline_len = s.len() as u8;
        }
        ScalarValue::Binary(b) if b.len() <= 16 => {
            min_inline[..b.len()].copy_from_slice(b);
            min_inline_len = b.len() as u8;
        }
        ScalarValue::List { values, .. } if values.is_empty() => {
            block_flags |= BLOCK_FLAG_CONST_EMPTY_COLLECTION;
            min_inline_len = 0;
            max_inline_len = 0;
            max_inline = [0u8; 16];
        }
        ScalarValue::MapUtf8(m) if m.is_empty() => {
            block_flags |= BLOCK_FLAG_CONST_EMPTY_COLLECTION;
            min_inline_len = 0;
            max_inline_len = 0;
            max_inline = [0u8; 16];
        }
        ScalarValue::Events(e) if e.is_empty() => {
            block_flags |= BLOCK_FLAG_CONST_EMPTY_COLLECTION;
            min_inline_len = 0;
            max_inline_len = 0;
            max_inline = [0u8; 16];
        }
        ScalarValue::Links(l) if l.is_empty() => {
            block_flags |= BLOCK_FLAG_CONST_EMPTY_COLLECTION;
            min_inline_len = 0;
            max_inline_len = 0;
            max_inline = [0u8; 16];
        }
        _ => return None,
    }

    Some(EncodedMiniBlock {
        encoding: Encoding::Const,
        block_flags,
        null_count: 0,
        min_inline_len,
        max_inline_len,
        min_inline,
        max_inline,
        raw_body: Vec::new(),
        preferred_codec: Codec::None,
    })
}

fn flat_all_equal_scalar(flat: &FlatColumn) -> Option<ScalarValue> {
    let n = flat.len();
    if n == 0 || flat.has_nulls() {
        return None;
    }
    match flat.data() {
        FlatData::Bool(bm) => {
            let first = bm.get(0);
            if (1..n).all(|i| bm.get(i) == first) {
                Some(ScalarValue::Bool(first))
            } else {
                None
            }
        }
        FlatData::I64(v) => {
            let first = v[0];
            v[1..]
                .iter()
                .all(|&x| x == first)
                .then_some(ScalarValue::I64(first))
        }
        FlatData::F64(v) => {
            let first_bits = v[0].to_bits();
            v[1..]
                .iter()
                .all(|x| x.to_bits() == first_bits)
                .then_some(ScalarValue::F64(v[0]))
        }
        FlatData::Decimal128 {
            values,
            precision,
            scale,
        } => {
            let first = values[0];
            values[1..]
                .iter()
                .all(|&x| x == first)
                .then_some(ScalarValue::Decimal128 {
                    value: first,
                    precision: *precision,
                    scale: *scale,
                })
        }
        FlatData::TimestampNs { values, utc } => {
            let first = values[0];
            values[1..]
                .iter()
                .all(|&x| x == first)
                .then_some(ScalarValue::TimestampNs {
                    value: first,
                    utc: *utc,
                })
        }
        FlatData::Date(v) => {
            let first = v[0];
            v[1..]
                .iter()
                .all(|&x| x == first)
                .then_some(ScalarValue::Date(first))
        }
        FlatData::Interval(v) => {
            let first = v[0];
            v[1..]
                .iter()
                .all(|&x| x == first)
                .then_some(ScalarValue::Interval(first))
        }
        FlatData::FixedBytes8(v) => {
            let first = v[0];
            v[1..]
                .iter()
                .all(|&x| x == first)
                .then_some(ScalarValue::FixedBytes8(first))
        }
        FlatData::FixedBytes16(v) => {
            let first = v[0];
            v[1..]
                .iter()
                .all(|&x| x == first)
                .then_some(ScalarValue::FixedBytes16(first))
        }
        FlatData::Utf8(u) => {
            let first = crate::codec::utf8_bytes_at(u, 0)?;
            if first.len() <= 16 && (1..n).all(|i| crate::codec::utf8_bytes_at(u, i) == Some(first))
            {
                let s = std::str::from_utf8(first).ok()?;
                Some(ScalarValue::Utf8(s.to_owned()))
            } else {
                None
            }
        }
        FlatData::Binary(b) => {
            let first = b.get(0)?;
            if first.len() <= 16 && (1..n).all(|i| b.get(i) == Some(first)) {
                Some(ScalarValue::Binary(first.to_vec()))
            } else {
                None
            }
        }
        FlatData::List { offsets, .. } => (offsets.last() == Some(&0)).then(|| ScalarValue::List {
            item_type: match flat.data_type() {
                DataType::List(inner) => *inner,
                _ => unreachable!(),
            },
            values: Vec::new(),
        }),
        FlatData::MapUtf8(m) => (m.entry_count() == 0).then(|| ScalarValue::MapUtf8(Vec::new())),
        FlatData::Events(e) => (e.event_count() == 0).then(|| ScalarValue::Events(Vec::new())),
        FlatData::Links(l) => (l.link_count() == 0).then(|| ScalarValue::Links(Vec::new())),
        FlatData::Null(_) => None,
    }
}

/// Decodes a `ConstNull` or `Const` mini-block purely from its [`BlockEntry`].
pub(crate) fn decode_const_block(entry: &BlockEntry, dt: &DataType) -> Result<Column, StatamError> {
    let row_count = entry.row_count as usize;
    if entry.encoding == Encoding::ConstNull {
        return Ok(Column::Const {
            value: ScalarValue::Null(dt.clone()),
            len: row_count,
        });
    }
    if entry.null_count != 0 {
        return Err(StatamError::Corrupt {
            reason: "Const block cannot have non-zero null_count",
        });
    }

    if entry.block_flags & BLOCK_FLAG_CONST_EMPTY_COLLECTION != 0 {
        let val = match dt {
            DataType::List(inner) => ScalarValue::List {
                item_type: (**inner).clone(),
                values: Vec::new(),
            },
            DataType::MapUtf8 => ScalarValue::MapUtf8(Vec::new()),
            DataType::Events => ScalarValue::Events(Vec::new()),
            DataType::Links => ScalarValue::Links(Vec::new()),
            _ => {
                return Err(StatamError::Corrupt {
                    reason: "CONST_EMPTY_COLLECTION set on non-collection DataType",
                });
            }
        };
        return Ok(Column::Const {
            value: val,
            len: row_count,
        });
    }

    let inline_len = usize::from(entry.min_inline_len);
    if inline_len > 16 {
        return Err(StatamError::Corrupt {
            reason: "Const inline length exceeds 16",
        });
    }
    let bytes = &entry.min_inline[..inline_len];
    let val = match dt {
        DataType::Bool => {
            if inline_len != 1 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const Bool inline length",
                });
            }
            ScalarValue::Bool(bytes[0] != 0)
        }
        DataType::I64 => {
            if inline_len != 8 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const I64 inline length",
                });
            }
            ScalarValue::I64(i64::from_le_bytes(bytes.try_into().map_err(|_| {
                StatamError::Corrupt {
                    reason: "invalid Const I64 slice",
                }
            })?))
        }
        DataType::F64 => {
            if inline_len != 8 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const F64 inline length",
                });
            }
            let bits = u64::from_le_bytes(bytes.try_into().map_err(|_| StatamError::Corrupt {
                reason: "invalid Const F64 slice",
            })?);
            ScalarValue::F64(f64::from_bits(bits))
        }
        DataType::Decimal128 { precision, scale } => {
            if inline_len != 16 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const Decimal128 inline length",
                });
            }
            ScalarValue::Decimal128 {
                value: i128::from_le_bytes(entry.min_inline),
                precision: *precision,
                scale: *scale,
            }
        }
        DataType::TimestampNs { utc } => {
            if inline_len != 8 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const TimestampNs inline length",
                });
            }
            let value = i64::from_le_bytes(bytes.try_into().map_err(|_| StatamError::Corrupt {
                reason: "invalid Const TimestampNs slice",
            })?);
            ScalarValue::TimestampNs { value, utc: *utc }
        }
        DataType::Date => {
            if inline_len != 4 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const Date inline length",
                });
            }
            let value = i32::from_le_bytes(bytes.try_into().map_err(|_| StatamError::Corrupt {
                reason: "invalid Const Date slice",
            })?);
            ScalarValue::Date(value)
        }
        DataType::Interval => {
            if inline_len != 16 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const Interval inline length",
                });
            }
            let months = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let days = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            let nanos = i64::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ]);
            ScalarValue::Interval(Interval {
                months,
                days,
                nanos,
            })
        }
        DataType::FixedBytes8 => {
            if inline_len != 8 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const FixedBytes8 inline length",
                });
            }
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes);
            ScalarValue::FixedBytes8(arr)
        }
        DataType::FixedBytes16 => {
            if inline_len != 16 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const FixedBytes16 inline length",
                });
            }
            ScalarValue::FixedBytes16(entry.min_inline)
        }
        DataType::Utf8 => {
            let s = std::str::from_utf8(bytes).map_err(|_| StatamError::Corrupt {
                reason: "invalid UTF-8 in Const Utf8 inline",
            })?;
            ScalarValue::Utf8(s.to_owned())
        }
        DataType::Binary => ScalarValue::Binary(bytes.to_vec()),
        _ => {
            return Err(StatamError::Corrupt {
                reason: "unsupported DataType for Const encoding",
            });
        }
    };

    Ok(Column::Const {
        value: val,
        len: row_count,
    })
}

/// Encodes a narrow scalar [`Column`] (not `Dict`, not nested) into [`EncodedMiniBlock`].
pub(crate) fn encode_narrow_column(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    precomputed_zm: Option<&ZoneMapSummary>,
) -> Result<EncodedMiniBlock, StatamError> {
    encode_narrow_column_with_locality(col, dt, profile, precomputed_zm, false)
}

/// Encodes a narrow scalar [`Column`] with `is_wide_row` hint to skip FSST on `WideRow` strings.
pub(crate) fn encode_narrow_column_with_locality(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    precomputed_zm: Option<&ZoneMapSummary>,
    is_wide_row: bool,
) -> Result<EncodedMiniBlock, StatamError> {
    if col.is_empty() {
        return Ok(EncodedMiniBlock {
            encoding: Encoding::Plain,
            block_flags: 0,
            null_count: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            min_inline: [0u8; 16],
            max_inline: [0u8; 16],
            raw_body: Vec::new(),
            preferred_codec: Codec::None,
        });
    }

    let owned_zm;
    let zm = match precomputed_zm {
        Some(z) => z,
        None => {
            owned_zm = compute_zone_map(col);
            if let Some(inline) = try_encode_const_or_null(col, dt, &owned_zm) {
                return Ok(inline);
            }
            &owned_zm
        }
    };

    // Try RLE when input is already Rle, constant >16B, or has average run length >= 4.
    if let Some(rle_block) = try_encode_rle(col, dt, profile, zm)? {
        return Ok(rle_block);
    }

    let owned_flat;
    let flat = match col {
        Column::Flat(f) => f,
        other => {
            owned_flat = other.materialize(None)?;
            &owned_flat
        }
    };

    match dt {
        DataType::Bool => {
            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            if let FlatData::Bool(bm) = flat.data() {
                raw.extend_from_slice(&bm.to_bytes());
            }
            let preferred_codec = preferred_block_codec(profile, raw.len(), false);
            Ok(EncodedMiniBlock {
                encoding: Encoding::BitpackBool,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec,
            })
        }
        DataType::I64 | DataType::TimestampNs { .. } | DataType::Date => {
            let (enc, inner_body) = if is_monotonic_sample_32(flat) {
                if let Some(db) = encode_delta_bitpack_body(flat)? {
                    (Encoding::DeltaBitpack, db)
                } else {
                    (Encoding::Plain, encode_plain_body(flat)?)
                }
            } else if let Some(fb) = encode_for_bitpack_body(flat)? {
                (Encoding::ForBitpack, fb)
            } else {
                (Encoding::Plain, encode_plain_body(flat)?)
            };

            let raw = prepend_optional_nulls(flat.nulls(), inner_body);

            let block_codec = preferred_block_codec(profile, raw.len(), enc == Encoding::Plain);
            Ok(EncodedMiniBlock {
                encoding: enc,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec: block_codec,
            })
        }
        DataType::Decimal128 { .. } => {
            let (enc, inner_body) = if let Some(fb) = encode_for_bitpack_body(flat)? {
                (Encoding::ForBitpack, fb)
            } else {
                (Encoding::Plain, encode_plain_body(flat)?)
            };
            let raw = prepend_optional_nulls(flat.nulls(), inner_body);
            let block_codec = preferred_block_codec(profile, raw.len(), enc == Encoding::Plain);
            Ok(EncodedMiniBlock {
                encoding: enc,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec: block_codec,
            })
        }
        DataType::F64 => {
            let (enc, inner_body) = if profile == WriteProfile::L1Compact {
                let FlatData::F64(vals) = flat.data() else {
                    return Err(StatamError::SchemaMismatch {
                        reason: "expected F64 FlatData",
                    });
                };
                if let Some(alp) = encode_alp_body(vals, flat.nulls())? {
                    (Encoding::Alp, alp)
                } else {
                    (Encoding::Plain, encode_plain_body(flat)?)
                }
            } else {
                (Encoding::Plain, encode_plain_body(flat)?)
            };
            let raw = prepend_optional_nulls(flat.nulls(), inner_body);
            let block_codec = preferred_block_codec(profile, raw.len(), false);
            Ok(EncodedMiniBlock {
                encoding: enc,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec: block_codec,
            })
        }
        DataType::Utf8 | DataType::Binary => {
            let raw_varlen_bytes = match flat.data() {
                FlatData::Utf8(u) => u.bytes().len(),
                FlatData::Binary(b) => b.bytes().len(),
                _ => 0,
            };
            let (enc, raw, is_subchunked) = if profile == WriteProfile::L1Compact
                && !is_wide_row
                && (256..=8 * 1024 * 1024).contains(&raw_varlen_bytes)
            {
                let slices = extract_varlen_slices(flat)?;
                if let Some((fsst, subchunked)) = encode_fsst_body_adaptive(&slices, profile)? {
                    (
                        Encoding::Fsst,
                        prepend_optional_nulls(flat.nulls(), fsst),
                        subchunked,
                    )
                } else {
                    (
                        Encoding::Plain,
                        prepend_optional_nulls(flat.nulls(), encode_plain_body(flat)?),
                        false,
                    )
                }
            } else if profile == WriteProfile::L1Compact
                && is_wide_row
                && matches!(dt, DataType::Utf8)
                && flat.len() > FSST_SUBCHUNK_MIN_ROWS
                && (64 * 1024..=8 * 1024 * 1024).contains(&raw_varlen_bytes)
            {
                let nulls_prefix_len = flat.nulls().map_or(0, |bm| bm.len().div_ceil(8));
                let plain_raw = prepend_optional_nulls(flat.nulls(), encode_plain_body(flat)?);
                let slices = extract_varlen_slices(flat)?;
                if let Some(fsst_subchunked) =
                    encode_fsst_subchunked_only(&slices, profile, &plain_raw, nulls_prefix_len)?
                {
                    (
                        Encoding::Fsst,
                        prepend_optional_nulls(flat.nulls(), fsst_subchunked),
                        true,
                    )
                } else {
                    (Encoding::Plain, plain_raw, false)
                }
            } else {
                (
                    Encoding::Plain,
                    prepend_optional_nulls(flat.nulls(), encode_plain_body(flat)?),
                    false,
                )
            };
            let mut block_flags = zm.block_flags;
            let block_codec = if is_subchunked {
                block_flags |= BLOCK_FLAG_FSST_SUBCHUNKED;
                Codec::None
            } else {
                preferred_block_codec(profile, raw.len(), true)
            };
            Ok(EncodedMiniBlock {
                encoding: enc,
                block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec: block_codec,
            })
        }
        DataType::Interval | DataType::FixedBytes8 | DataType::FixedBytes16 | DataType::Null => {
            let raw = prepend_optional_nulls(flat.nulls(), encode_plain_body(flat)?);
            let preferred_codec = preferred_block_codec(profile, raw.len(), false);
            Ok(EncodedMiniBlock {
                encoding: Encoding::Plain,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec,
            })
        }
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links => {
            Err(StatamError::SchemaMismatch {
                reason: "nested DataType passed to narrow encoder",
            })
        }
    }
}

#[inline]
pub(crate) fn prepend_optional_nulls(nulls: Option<&Bitmap>, inner_body: Vec<u8>) -> Vec<u8> {
    let Some(bm) = nulls else {
        return inner_body;
    };
    let bm_bytes = bm.to_bytes();
    let mut raw = Vec::with_capacity(bm_bytes.len() + inner_body.len());
    raw.extend_from_slice(&bm_bytes);
    raw.extend_from_slice(&inner_body);
    raw
}

fn is_monotonic_sample_32(flat: &FlatColumn) -> bool {
    let nulls = flat.nulls();
    let mut prev: Option<i64> = None;
    let mut dir: i8 = 0;
    let mut sampled = 0usize;

    let mut step = |v: i64| -> Option<usize> {
        sampled += 1;
        if let Some(p) = prev {
            if v > p {
                if dir == -1 {
                    return None;
                }
                dir = 1;
            } else if v < p {
                if dir == 1 {
                    return None;
                }
                dir = -1;
            }
        }
        prev = Some(v);
        Some(sampled)
    };

    match flat.data() {
        FlatData::I64(vals) | FlatData::TimestampNs { values: vals, .. } => {
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    let Some(count) = step(v) else {
                        return false;
                    };
                    if count >= 32 {
                        break;
                    }
                }
            }
        }
        FlatData::Date(vals) => {
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    let Some(count) = step(i64::from(v)) else {
                        return false;
                    };
                    if count >= 32 {
                        break;
                    }
                }
            }
        }
        _ => return false,
    }
    sampled >= 2
}

fn extract_varlen_slices(flat: &FlatColumn) -> Result<Vec<&[u8]>, StatamError> {
    let n = flat.len();
    let nulls = flat.nulls();
    let mut out: Vec<&[u8]> = Vec::with_capacity(n);
    match flat.data() {
        FlatData::Utf8(u) => {
            for i in 0..n {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    out.push(&[]);
                } else {
                    out.push(crate::codec::utf8_bytes_at(u, i).unwrap_or(&[]));
                }
            }
        }
        FlatData::Binary(b) => {
            for i in 0..n {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    out.push(&[]);
                } else {
                    out.push(b.get(i).unwrap_or(&[]));
                }
            }
        }
        _ => {
            return Err(StatamError::SchemaMismatch {
                reason: "expected Utf8 or Binary FlatData",
            });
        }
    }
    Ok(out)
}

fn try_encode_rle(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    zm: &ZoneMapSummary,
) -> Result<Option<EncodedMiniBlock>, StatamError> {
    if !matches!(
        dt,
        DataType::Bool
            | DataType::I64
            | DataType::F64
            | DataType::Decimal128 { .. }
            | DataType::TimestampNs { .. }
            | DataType::Date
            | DataType::Interval
            | DataType::FixedBytes8
            | DataType::FixedBytes16
            | DataType::Utf8
            | DataType::Binary
    ) {
        return Ok(None);
    }

    let n = col.len();
    if n < 2 {
        return Ok(None);
    }

    let (run_values, run_ends) = match col {
        Column::Rle(r) => {
            if r.run_ends().is_empty() {
                return Ok(None);
            }
            (r.values().clone(), r.run_ends().to_vec())
        }
        Column::Const { value, len } if *len >= 4 && !value.is_null() => {
            let single = col.slice(0, 1, None)?.materialize(None)?;
            (single, vec![*len as u32])
        }
        Column::Flat(f) => {
            if !has_runs_sample_64(f) {
                return Ok(None);
            }
            let Some((vals, ends)) = compute_flat_runs(f)? else {
                return Ok(None);
            };
            if ends.is_empty() || ends.len() * 4 > n {
                return Ok(None);
            }
            (vals, ends)
        }
        _ => return Ok(None),
    };

    let num_runs = run_ends.len();
    let run_nulls = run_values.nulls();
    let has_run_nulls = run_nulls.is_some();

    let (inner_enc, inner_body) = encode_rle_inner(&run_values, dt, profile)?;

    let re_bw = min_bits_u32(&run_ends);
    let mut raw = Vec::new();
    raw.extend_from_slice(&(num_runs as u32).to_le_bytes());
    if has_run_nulls {
        write_optional_nulls(run_nulls, &mut raw);
    }
    raw.push(re_bw);
    pack_u32(&run_ends, re_bw, &mut raw)?;
    raw.push(inner_enc as u8);
    raw.extend_from_slice(&inner_body);

    let mut block_flags = zm.block_flags;
    if has_run_nulls {
        block_flags |= BLOCK_FLAG_HAS_NULLS;
    }

    let preferred_codec = preferred_block_codec(profile, raw.len(), false);
    Ok(Some(EncodedMiniBlock {
        encoding: Encoding::Rle,
        block_flags,
        null_count: zm.null_count,
        min_inline_len: zm.min_inline_len,
        max_inline_len: zm.max_inline_len,
        min_inline: zm.min_inline,
        max_inline: zm.max_inline,
        raw_body: raw,
        preferred_codec,
    }))
}

fn encode_rle_inner(
    run_values: &FlatColumn,
    dt: &DataType,
    profile: WriteProfile,
) -> Result<(Encoding, Vec<u8>), StatamError> {
    match dt {
        DataType::Bool => Ok((Encoding::BitpackBool, encode_plain_body(run_values)?)),
        DataType::I64 | DataType::TimestampNs { .. } | DataType::Date => {
            if let Some(fb) = encode_for_bitpack_body(run_values)? {
                Ok((Encoding::ForBitpack, fb))
            } else {
                Ok((Encoding::Plain, encode_plain_body(run_values)?))
            }
        }
        DataType::Decimal128 { .. } => {
            if let Some(fb) = encode_for_bitpack_body(run_values)? {
                Ok((Encoding::ForBitpack, fb))
            } else {
                Ok((Encoding::Plain, encode_plain_body(run_values)?))
            }
        }
        DataType::F64 => {
            if profile == WriteProfile::L1Compact
                && let FlatData::F64(vals) = run_values.data()
                && let Some(alp) = encode_alp_body(vals, run_values.nulls())?
            {
                return Ok((Encoding::Alp, alp));
            }
            Ok((Encoding::Plain, encode_plain_body(run_values)?))
        }
        DataType::Utf8 | DataType::Binary => {
            if profile == WriteProfile::L1Compact && !run_values.is_empty() {
                let raw_bytes = match run_values.data() {
                    FlatData::Utf8(u) => u.bytes().len(),
                    FlatData::Binary(b) => b.bytes().len(),
                    _ => 0,
                };
                if raw_bytes <= 16 * 1024 {
                    let slices = extract_varlen_slices(run_values)?;
                    if let Some(fsst) = encode_fsst_body(&slices)? {
                        return Ok((Encoding::Fsst, fsst));
                    }
                }
            }
            Ok((Encoding::Plain, encode_plain_body(run_values)?))
        }
        _ => Ok((Encoding::Plain, encode_plain_body(run_values)?)),
    }
}

#[inline]
fn count_transitions_with<F: FnMut(usize, usize) -> bool>(
    sample_n: usize,
    nulls: Option<&Bitmap>,
    mut eq_val: F,
) -> usize {
    let mut transitions = 0usize;
    let mut run_start = 0usize;
    if let Some(bm) = nulls {
        let mut run_is_null = !bm.get(0);
        for i in 1..sample_n {
            let is_null = !bm.get(i);
            if run_is_null {
                if is_null {
                    continue;
                }
            } else if !is_null && eq_val(run_start, i) {
                continue;
            }
            transitions += 1;
            run_start = i;
            run_is_null = is_null;
        }
    } else {
        for i in 1..sample_n {
            if eq_val(run_start, i) {
                continue;
            }
            transitions += 1;
            run_start = i;
        }
    }
    transitions
}

#[inline]
fn collect_runs_with<F: FnMut(usize, usize) -> bool>(
    n: usize,
    max_runs: usize,
    nulls: Option<&Bitmap>,
    mut eq_val: F,
) -> Option<(Vec<u32>, Vec<u32>)> {
    let mut run_ends = Vec::new();
    let mut rep_indices = Vec::new();
    let mut run_start = 0usize;
    if let Some(bm) = nulls {
        let mut run_is_null = !bm.get(0);
        for i in 1..n {
            let is_null = !bm.get(i);
            if run_is_null {
                if is_null {
                    continue;
                }
            } else if !is_null && eq_val(run_start, i) {
                continue;
            }
            rep_indices.push(run_start as u32);
            run_ends.push(i as u32);
            if run_ends.len() >= max_runs {
                return None;
            }
            run_start = i;
            run_is_null = is_null;
        }
    } else {
        for i in 1..n {
            if eq_val(run_start, i) {
                continue;
            }
            rep_indices.push(run_start as u32);
            run_ends.push(i as u32);
            if run_ends.len() >= max_runs {
                return None;
            }
            run_start = i;
        }
    }
    rep_indices.push(run_start as u32);
    run_ends.push(n as u32);
    Some((rep_indices, run_ends))
}

fn has_runs_sample_64(flat: &FlatColumn) -> bool {
    let sample_n = flat.len().min(64);
    if sample_n < 4 {
        return true;
    }
    let nulls = flat.nulls();
    let transitions = match flat.data() {
        FlatData::Bool(bm) => {
            count_transitions_with(sample_n, nulls, |a, b| bm.get(a) == bm.get(b))
        }
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => {
            count_transitions_with(sample_n, nulls, |a, b| v[a] == v[b])
        }
        FlatData::F64(v) => {
            count_transitions_with(sample_n, nulls, |a, b| v[a].to_bits() == v[b].to_bits())
        }
        FlatData::Decimal128 { values, .. } => {
            count_transitions_with(sample_n, nulls, |a, b| values[a] == values[b])
        }
        FlatData::Date(v) => count_transitions_with(sample_n, nulls, |a, b| v[a] == v[b]),
        FlatData::Interval(v) => count_transitions_with(sample_n, nulls, |a, b| v[a] == v[b]),
        FlatData::FixedBytes8(v) => count_transitions_with(sample_n, nulls, |a, b| v[a] == v[b]),
        FlatData::FixedBytes16(v) => count_transitions_with(sample_n, nulls, |a, b| v[a] == v[b]),
        FlatData::Utf8(u) => count_transitions_with(sample_n, nulls, |a, b| {
            crate::codec::utf8_bytes_at(u, a) == crate::codec::utf8_bytes_at(u, b)
        }),
        FlatData::Binary(bd) => {
            count_transitions_with(sample_n, nulls, |a, b| bd.get(a) == bd.get(b))
        }
        _ => count_transitions_with(sample_n, nulls, |_, _| false),
    };
    transitions * 4 <= sample_n
}

fn compute_flat_runs(flat: &FlatColumn) -> Result<Option<(FlatColumn, Vec<u32>)>, StatamError> {
    let n = flat.len();
    if n < 2 {
        return Ok(None);
    }

    // Early bail-out if runs exceed n / 4 after scanning.
    let max_runs = n / 4;
    if max_runs < 1 {
        return Ok(None);
    }

    let nulls = flat.nulls();
    let collected = match flat.data() {
        FlatData::Bool(bm) => collect_runs_with(n, max_runs, nulls, |a, b| bm.get(a) == bm.get(b)),
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => {
            collect_runs_with(n, max_runs, nulls, |a, b| v[a] == v[b])
        }
        FlatData::F64(v) => {
            collect_runs_with(n, max_runs, nulls, |a, b| v[a].to_bits() == v[b].to_bits())
        }
        FlatData::Decimal128 { values, .. } => {
            collect_runs_with(n, max_runs, nulls, |a, b| values[a] == values[b])
        }
        FlatData::Date(v) => collect_runs_with(n, max_runs, nulls, |a, b| v[a] == v[b]),
        FlatData::Interval(v) => collect_runs_with(n, max_runs, nulls, |a, b| v[a] == v[b]),
        FlatData::FixedBytes8(v) => collect_runs_with(n, max_runs, nulls, |a, b| v[a] == v[b]),
        FlatData::FixedBytes16(v) => collect_runs_with(n, max_runs, nulls, |a, b| v[a] == v[b]),
        FlatData::Utf8(u) => collect_runs_with(n, max_runs, nulls, |a, b| {
            crate::codec::utf8_bytes_at(u, a) == crate::codec::utf8_bytes_at(u, b)
        }),
        FlatData::Binary(bd) => {
            collect_runs_with(n, max_runs, nulls, |a, b| bd.get(a) == bd.get(b))
        }
        _ => collect_runs_with(n, max_runs, nulls, |_, _| false),
    };
    let Some((rep_indices, run_ends)) = collected else {
        return Ok(None);
    };

    let run_values = flat.take(&rep_indices, None)?;
    Ok(Some((run_values, run_ends)))
}

/// Encodes `ForBitpack` body (without leading row null bitmap).
pub(crate) fn encode_for_bitpack_body(flat: &FlatColumn) -> Result<Option<Vec<u8>>, StatamError> {
    let n = flat.len();
    let nulls = flat.nulls();
    match flat.data() {
        FlatData::I64(vals) | FlatData::TimestampNs { values: vals, .. } => {
            let mut min_v = i64::MAX;
            let mut any = false;
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    any = true;
                    min_v = min_v.min(v);
                }
            }
            if !any {
                min_v = 0;
            }
            let mut deltas = Vec::with_capacity(n);
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    deltas.push(0);
                } else {
                    deltas.push(v.wrapping_sub(min_v) as u64);
                }
            }
            let bw = min_bits_u64(&deltas);
            let mut out = Vec::with_capacity(9 + packed_bytes_len(n, bw)?);
            out.extend_from_slice(&min_v.to_le_bytes());
            out.push(bw);
            pack_u64(&deltas, bw, &mut out)?;
            Ok(Some(out))
        }
        FlatData::Date(vals) => {
            let mut min_v = i32::MAX;
            let mut any = false;
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    any = true;
                    min_v = min_v.min(v);
                }
            }
            if !any {
                min_v = 0;
            }
            let mut deltas = Vec::with_capacity(n);
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    deltas.push(0);
                } else {
                    deltas.push(v.wrapping_sub(min_v) as u32);
                }
            }
            let bw = min_bits_u32(&deltas);
            let mut out = Vec::with_capacity(5 + packed_bytes_len(n, bw)?);
            out.extend_from_slice(&min_v.to_le_bytes());
            out.push(bw);
            pack_u32(&deltas, bw, &mut out)?;
            Ok(Some(out))
        }
        FlatData::Decimal128 { values, .. } => {
            let mut min_v = i128::MAX;
            let mut max_v = i128::MIN;
            let mut any = false;
            for (i, &v) in values.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    any = true;
                    min_v = min_v.min(v);
                    max_v = max_v.max(v);
                }
            }
            if !any {
                min_v = 0;
                max_v = 0;
            }
            let Some(span) = max_v.checked_sub(min_v) else {
                return Ok(None);
            };
            if span > i128::from(u64::MAX) {
                return Ok(None);
            }
            let mut deltas = Vec::with_capacity(n);
            for (i, &v) in values.iter().enumerate() {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    deltas.push(0);
                } else {
                    deltas.push((v - min_v) as u64);
                }
            }
            let bw = min_bits_u64(&deltas);
            let mut out = Vec::with_capacity(17 + packed_bytes_len(n, bw)?);
            out.extend_from_slice(&min_v.to_le_bytes());
            out.push(bw);
            pack_u64(&deltas, bw, &mut out)?;
            Ok(Some(out))
        }
        _ => Ok(None),
    }
}

/// Encodes `DeltaBitpack` body (without leading row null bitmap) for `I64`/`TimestampNs`/`Date`.
pub(crate) fn encode_delta_bitpack_body(flat: &FlatColumn) -> Result<Option<Vec<u8>>, StatamError> {
    let n = flat.len();
    if n == 0 {
        return Ok(None);
    }
    let nulls = flat.nulls();

    let mut norm = Vec::with_capacity(n);
    let mut last_valid: Option<i64> = None;

    match flat.data() {
        FlatData::I64(vals) | FlatData::TimestampNs { values: vals, .. } => {
            let first_valid = (0..n)
                .find(|&i| nulls.is_none_or(|bm| bm.get(i)))
                .map_or(0, |i| vals[i]);
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    norm.push(last_valid.unwrap_or(first_valid));
                } else {
                    norm.push(v);
                    last_valid = Some(v);
                }
            }
        }
        FlatData::Date(vals) => {
            let first_valid = (0..n)
                .find(|&i| nulls.is_none_or(|bm| bm.get(i)))
                .map_or(0, |i| i64::from(vals[i]));
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    norm.push(last_valid.unwrap_or(first_valid));
                } else {
                    let iv = i64::from(v);
                    norm.push(iv);
                    last_valid = Some(iv);
                }
            }
        }
        _ => return Ok(None),
    }

    let mut bases = Vec::new();
    let mut raw_deltas = Vec::new();
    delta_encode_i64(&norm, &mut bases, &mut raw_deltas);

    let all_non_neg = raw_deltas.iter().all(|&d| d >= 0);
    let is_zigzag = u8::from(!all_non_neg);
    let mut packed_deltas = Vec::with_capacity(raw_deltas.len());
    if all_non_neg {
        for d in raw_deltas {
            packed_deltas.push(d as u64);
        }
    } else {
        for d in raw_deltas {
            packed_deltas.push(zigzag_encode_i64(d));
        }
    }

    let bw = min_bits_u64(&packed_deltas);
    let mut out = Vec::with_capacity(2 + bases.len() * 8 + packed_bytes_len(n, bw)?);
    out.push(is_zigzag);
    out.push(bw);
    for b in bases {
        out.extend_from_slice(&b.to_le_bytes());
    }
    pack_u64(&packed_deltas, bw, &mut out)?;
    Ok(Some(out))
}

/// Encodes `Plain` body (without leading row null bitmap).
pub(crate) fn encode_plain_body(flat: &FlatColumn) -> Result<Vec<u8>, StatamError> {
    let mut out = Vec::new();
    match flat.data() {
        FlatData::Null(_) => {}
        FlatData::Bool(bm) => {
            out.extend_from_slice(&bm.to_bytes());
        }
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => {
            out.reserve(v.len() * 8);
            for &x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        FlatData::F64(v) => {
            out.reserve(v.len() * 8);
            for &x in v {
                out.extend_from_slice(&x.to_bits().to_le_bytes());
            }
        }
        FlatData::Decimal128 { values, .. } => {
            out.reserve(values.len() * 16);
            for &x in values {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        FlatData::Date(v) => {
            out.reserve(v.len() * 4);
            for &x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        FlatData::Interval(v) => {
            out.reserve(v.len() * 16);
            for iv in v {
                out.extend_from_slice(&iv.months.to_le_bytes());
                out.extend_from_slice(&iv.days.to_le_bytes());
                out.extend_from_slice(&iv.nanos.to_le_bytes());
            }
        }
        FlatData::FixedBytes8(v) => {
            out.reserve(v.len() * 8);
            for x in v {
                out.extend_from_slice(x);
            }
        }
        FlatData::FixedBytes16(v) => {
            out.reserve(v.len() * 16);
            for x in v {
                out.extend_from_slice(x);
            }
        }
        FlatData::Utf8(u) => return Ok(encode_plain_utf8(u)),
        FlatData::Binary(b) => {
            out.reserve(b.offsets().len() * 4 + b.bytes().len());
            for &off in b.offsets() {
                out.extend_from_slice(&off.to_le_bytes());
            }
            out.extend_from_slice(b.bytes());
        }
        FlatData::MapUtf8(m) => {
            let offsets = m.offsets();
            let entry_count = m.entry_count();
            let keys = m.keys();
            let vals = m.values();
            out.reserve(
                offsets.len() * 4
                    + if entry_count > 0 {
                        5 + keys.offsets().len() * 4
                            + keys.bytes().len()
                            + vals.offsets().len() * 4
                            + vals.bytes().len()
                    } else {
                        0
                    },
            );
            for &off in offsets {
                out.extend_from_slice(&off.to_le_bytes());
            }
            if entry_count > 0 {
                if let Some(vn) = m.value_nulls() {
                    out.push(1);
                    out.extend_from_slice(&vn.to_bytes());
                } else {
                    out.push(0);
                }
                let keys_bytes_len =
                    u32::try_from(keys.bytes().len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "Plain MapUtf8 keys bytes exceeds u32::MAX",
                    })?;
                out.extend_from_slice(&keys_bytes_len.to_le_bytes());
                for &off in keys.offsets() {
                    out.extend_from_slice(&off.to_le_bytes());
                }
                out.extend_from_slice(keys.bytes());
                for &off in vals.offsets() {
                    out.extend_from_slice(&off.to_le_bytes());
                }
                out.extend_from_slice(vals.bytes());
            }
        }
        _ => {
            return Err(StatamError::SchemaMismatch {
                reason: "nested FlatData passed to encode_plain_body",
            });
        }
    }
    Ok(out)
}

/// Encodes a [`Utf8Data`] buffer directly as `Plain` without cloning into a `FlatColumn`.
pub(crate) fn encode_plain_utf8(u: &Utf8Data) -> Vec<u8> {
    let mut out = Vec::with_capacity(u.offsets().len() * 4 + u.bytes().len());
    for &off in u.offsets() {
        out.extend_from_slice(&off.to_le_bytes());
    }
    out.extend_from_slice(u.bytes());
    out
}

/// Sparsely decodes only `mask`-selected rows of an already-split inner block body
/// (`nulls` already separated from `body`), falling back to [`decode_inner_flat`] for
/// encodings without a sparse selected path.
pub(crate) fn decode_inner_flat_selected(
    body: &[u8],
    encoding: Encoding,
    row_count: usize,
    dt: &DataType,
    nulls: Option<Bitmap>,
    mask: &Bitmap,
) -> Result<FlatColumn, StatamError> {
    match (encoding, dt) {
        (Encoding::BitpackBool | Encoding::Plain, DataType::Bool) => {
            let expected = row_count.div_ceil(8);
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "bool block byte length mismatch",
                });
            }
            let mut bm = Bitmap::new_zeroed(row_count);
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|nbm| nbm.get(row))
                    && ((body[row / 8] >> (row % 8)) & 1) != 0
                {
                    let _ = bm.set(row, true);
                }
            }
            Ok(FlatColumn::try_new(FlatData::Bool(bm), nulls)?)
        }
        (Encoding::Plain, DataType::I64 | DataType::TimestampNs { .. }) => {
            let expected = check_mul(row_count, 8, "plain i64 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain i64 byte length mismatch",
                });
            }
            let mut vals = vec![0i64; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let off = row * 8;
                    vals[row] = i64::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                        body[off + 4],
                        body[off + 5],
                        body[off + 6],
                        body[off + 7],
                    ]);
                }
            }
            let data = match dt {
                DataType::I64 => FlatData::I64(vals),
                DataType::TimestampNs { utc } => FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                _ => unreachable!(),
            };
            Ok(FlatColumn::try_new(data, nulls)?)
        }
        (Encoding::Plain, DataType::F64) => {
            let expected = check_mul(row_count, 8, "plain f64 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain f64 byte length mismatch",
                });
            }
            let mut vals = vec![0.0f64; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let off = row * 8;
                    let bits = u64::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                        body[off + 4],
                        body[off + 5],
                        body[off + 6],
                        body[off + 7],
                    ]);
                    vals[row] = f64::from_bits(bits);
                }
            }
            Ok(FlatColumn::try_new(FlatData::F64(vals), nulls)?)
        }
        (Encoding::Plain, DataType::Date) => {
            let expected = check_mul(row_count, 4, "plain date size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain date byte length mismatch",
                });
            }
            let mut vals = vec![0i32; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let off = row * 4;
                    vals[row] = i32::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                    ]);
                }
            }
            Ok(FlatColumn::try_new(FlatData::Date(vals), nulls)?)
        }
        (Encoding::Plain, DataType::Decimal128 { precision, scale }) => {
            let expected = check_mul(row_count, 16, "plain decimal128 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain decimal128 byte length mismatch",
                });
            }
            let mut vals = vec![0i128; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let off = row * 16;
                    let arr: [u8; 16] =
                        body[off..off + 16]
                            .try_into()
                            .map_err(|_| StatamError::Corrupt {
                                reason: "plain decimal128 slice mismatch",
                            })?;
                    vals[row] = i128::from_le_bytes(arr);
                }
            }
            Ok(FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                nulls,
            )?)
        }
        (Encoding::Plain, DataType::Interval) => {
            let expected = check_mul(row_count, 16, "plain interval size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain interval byte length mismatch",
                });
            }
            let mut vals = vec![
                Interval {
                    months: 0,
                    days: 0,
                    nanos: 0,
                };
                row_count
            ];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let off = row * 16;
                    vals[row] = Interval {
                        months: i32::from_le_bytes([
                            body[off],
                            body[off + 1],
                            body[off + 2],
                            body[off + 3],
                        ]),
                        days: i32::from_le_bytes([
                            body[off + 4],
                            body[off + 5],
                            body[off + 6],
                            body[off + 7],
                        ]),
                        nanos: i64::from_le_bytes([
                            body[off + 8],
                            body[off + 9],
                            body[off + 10],
                            body[off + 11],
                            body[off + 12],
                            body[off + 13],
                            body[off + 14],
                            body[off + 15],
                        ]),
                    };
                }
            }
            Ok(FlatColumn::try_new(FlatData::Interval(vals), nulls)?)
        }
        (Encoding::Plain, DataType::FixedBytes8) => {
            let expected = check_mul(row_count, 8, "plain FixedBytes8 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes8 byte length mismatch",
                });
            }
            let mut vals = vec![[0u8; 8]; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let off = row * 8;
                    vals[row].copy_from_slice(&body[off..off + 8]);
                }
            }
            Ok(FlatColumn::try_new(FlatData::FixedBytes8(vals), nulls)?)
        }
        (Encoding::Plain, DataType::FixedBytes16) => {
            let expected = check_mul(row_count, 16, "plain FixedBytes16 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes16 byte length mismatch",
                });
            }
            let mut vals = vec![[0u8; 16]; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let off = row * 16;
                    vals[row].copy_from_slice(&body[off..off + 16]);
                }
            }
            Ok(FlatColumn::try_new(FlatData::FixedBytes16(vals), nulls)?)
        }
        (Encoding::Plain, DataType::Utf8) => {
            let (offsets, data_bytes) =
                decode_plain_varlen_selected(body, row_count, mask, nulls.as_ref())?;
            Ok(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_new(offsets, data_bytes)?),
                nulls,
            )?)
        }
        (Encoding::Plain, DataType::Binary) => {
            let (offsets, data_bytes) =
                decode_plain_varlen_selected(body, row_count, mask, nulls.as_ref())?;
            Ok(FlatColumn::try_new(
                FlatData::Binary(BinaryData::try_new(offsets, data_bytes)?),
                nulls,
            )?)
        }
        (Encoding::Plain, DataType::MapUtf8) => {
            let data = decode_plain_map_selected(body, row_count, mask)?;
            Ok(FlatColumn::try_new(data, nulls)?)
        }
        (Encoding::ForBitpack, DataType::I64 | DataType::TimestampNs { .. }) => {
            let (base, bw, packed) = parse_for_i64_header(body, row_count)?;
            let mut vals = vec![0i64; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let d = unpack_u64_at(packed, row_count, bw, row)?;
                    vals[row] = base.wrapping_add(d as i64);
                }
            }
            let data = match dt {
                DataType::I64 => FlatData::I64(vals),
                DataType::TimestampNs { utc } => FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                _ => unreachable!(),
            };
            Ok(FlatColumn::try_new(data, nulls)?)
        }
        (Encoding::ForBitpack, DataType::Date) => {
            let (base, bw, packed) = parse_for_date_header(body, row_count)?;
            let mut vals = vec![0i32; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let d = unpack_u32_at(packed, row_count, bw, row)?;
                    vals[row] = base.wrapping_add(d as i32);
                }
            }
            Ok(FlatColumn::try_new(FlatData::Date(vals), nulls)?)
        }
        (Encoding::ForBitpack, DataType::Decimal128 { precision, scale }) => {
            let (base, bw, packed) = parse_for_decimal128_header(body, row_count)?;
            let mut vals = vec![0i128; row_count];
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let d = unpack_u64_at(packed, row_count, bw, row)?;
                    vals[row] = base.wrapping_add(i128::from(d));
                }
            }
            Ok(FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                nulls,
            )?)
        }
        (Encoding::DeltaBitpack, DataType::I64 | DataType::TimestampNs { .. } | DataType::Date) => {
            let hdr = parse_delta_i64_header(body, row_count)?;
            let selected_rows: Vec<usize> =
                mask.iter_ones().take_while(|&r| r < row_count).collect();
            let decoded = delta_decode_i64_indices(
                &hdr.bases,
                hdr.packed,
                row_count,
                hdr.bw,
                hdr.is_zigzag,
                &selected_rows,
            )?;
            let data = match dt {
                DataType::I64 => {
                    let mut vals = vec![0i64; row_count];
                    for (&row, val) in selected_rows.iter().zip(decoded) {
                        if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                            vals[row] = val;
                        }
                    }
                    FlatData::I64(vals)
                }
                DataType::TimestampNs { utc } => {
                    let mut vals = vec![0i64; row_count];
                    for (&row, val) in selected_rows.iter().zip(decoded) {
                        if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                            vals[row] = val;
                        }
                    }
                    FlatData::TimestampNs {
                        values: vals,
                        utc: *utc,
                    }
                }
                DataType::Date => {
                    let mut vals = vec![0i32; row_count];
                    for (&row, val) in selected_rows.iter().zip(decoded) {
                        if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                            vals[row] = i32::try_from(val).map_err(|_| StatamError::Corrupt {
                                reason: "delta decoded Date out of i32 range",
                            })?;
                        }
                    }
                    FlatData::Date(vals)
                }
                _ => unreachable!(),
            };
            Ok(FlatColumn::try_new(data, nulls)?)
        }
        (Encoding::Alp, DataType::F64) => {
            let vals = decode_alp_selected(body, row_count, nulls.as_ref(), mask)?;
            Ok(FlatColumn::try_new(FlatData::F64(vals), nulls)?)
        }
        _ => decode_inner_flat(body, encoding, row_count, dt, nulls),
    }
}

/// Decodes a narrow scalar mini-block, decoding only `mask`-selected rows for
/// encodings that support sparse selective decode (`Fsst`, sparse `Plain`, `BitpackBool`,
/// `ForBitpack`, `DeltaBitpack`, and `Alp`).
pub(crate) fn decode_narrow_column_selected(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    mask: &Bitmap,
) -> Result<Column, StatamError> {
    if mask.len() != entry.row_count as usize {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        return decode_const_block(entry, dt);
    }
    let row_count = entry.row_count as usize;
    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty narrow block",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }

    if !mask.all_ones() {
        let selected_count = mask.count_ones();
        let is_plain_map = entry.encoding == Encoding::Plain && *dt == DataType::MapUtf8;

        // Zero-allocation all-null & single-row Column::Const short-circuit.
        if entry.encoding != Encoding::Rle
            && !is_plain_map
            && (selected_count <= 1 || selected_count.saturating_mul(16) < row_count)
        {
            let (null_bytes, body) =
                split_optional_null_bytes(raw_body, row_count, entry.block_flags)?;
            let any_valid = if selected_count == 0 {
                false
            } else if let Some(nb) = null_bytes {
                mask.iter_ones()
                    .take_while(|&r| r < row_count)
                    .any(|r| null_bytes_is_valid(Some(nb), r))
            } else {
                true
            };
            if !any_valid {
                let subchunked = (entry.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0;
                validate_inner_take_body(body, entry.encoding, subchunked, dt, row_count)?;
                return Ok(Column::Const {
                    value: ScalarValue::Null(dt.clone()),
                    len: row_count,
                });
            }
            if selected_count == 1
                && let Some(only_row) = mask.iter_ones().next()
                && let Some(col) = decode_narrow_column_take(raw_body, entry, dt, &[only_row])?
            {
                let scalar = col.get_scalar(0)?;
                return Ok(Column::Const {
                    value: scalar,
                    len: row_count,
                });
            }
        }

        if entry.encoding == Encoding::Fsst && matches!(dt, DataType::Utf8 | DataType::Binary) {
            let (nulls, body) = read_optional_nulls(raw_body, row_count, entry.block_flags)?;
            let subchunked = (entry.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0;
            let (data, out_nulls) = match dt {
                DataType::Utf8 => {
                    let (u, n) = decode_fsst_utf8_selected_with_flag(
                        body,
                        row_count,
                        mask,
                        nulls.as_ref(),
                        subchunked,
                        MAX_BLOCK_RAW_BYTES,
                    )?;
                    (FlatData::Utf8(u), n)
                }
                DataType::Binary => {
                    let (b, n) = decode_fsst_binary_selected_with_flag(
                        body,
                        row_count,
                        mask,
                        nulls.as_ref(),
                        subchunked,
                        MAX_BLOCK_RAW_BYTES,
                    )?;
                    (FlatData::Binary(b), n)
                }
                _ => unreachable!(),
            };
            return Ok(Column::Flat(FlatColumn::try_new(data, out_nulls)?));
        }

        if entry.encoding != Encoding::Rle && selected_count.saturating_mul(16) < row_count {
            let (nulls, body) = read_optional_nulls(raw_body, row_count, entry.block_flags)?;
            return Ok(Column::Flat(decode_inner_flat_selected(
                body,
                entry.encoding,
                row_count,
                dt,
                nulls,
                mask,
            )?));
        }
    }

    decode_narrow_column(raw_body, entry, dt)
}

#[derive(Clone, Copy)]
enum NullRef<'a> {
    None,
    Bytes(&'a [u8]),
    Bitmap(&'a Bitmap),
}

impl<'a> NullRef<'a> {
    #[inline]
    fn is_valid(self, row: usize) -> bool {
        match self {
            Self::None => true,
            Self::Bytes(nb) => null_bytes_is_valid(Some(nb), row),
            Self::Bitmap(bm) => bm.get(row),
        }
    }

    fn check_len(self, row_count: usize) -> Result<(), StatamError> {
        match self {
            Self::None => Ok(()),
            Self::Bytes(nb) if nb.len() == row_count.div_ceil(8) => Ok(()),
            Self::Bitmap(bm) if bm.len() == row_count => Ok(()),
            _ => Err(StatamError::Corrupt {
                reason: "null bitmap length mismatch",
            }),
        }
    }

    fn resolve_bitmap<'b>(
        self,
        has_mixed_nulls: bool,
        row_count: usize,
        owned: &'b mut Option<Bitmap>,
    ) -> Result<Option<&'b Bitmap>, StatamError>
    where
        'a: 'b,
    {
        if !has_mixed_nulls {
            return Ok(None);
        }
        match self {
            Self::None => Ok(None),
            Self::Bitmap(bm) => Ok(Some(bm)),
            Self::Bytes(nb) => {
                *owned = Some(Bitmap::from_bytes(nb, row_count)?);
                Ok(owned.as_ref())
            }
        }
    }
}

enum GatheredNulls {
    AllValid,
    AllNull,
    Mixed(Bitmap),
}

fn gather_nulls(nulls: NullRef<'_>, indices: &[usize]) -> GatheredNulls {
    if matches!(nulls, NullRef::None) || indices.is_empty() {
        return GatheredNulls::AllValid;
    }
    let mut any_valid = false;
    let mut any_null = false;
    for &row in indices {
        if nulls.is_valid(row) {
            any_valid = true;
        } else {
            any_null = true;
        }
        if any_valid && any_null {
            break;
        }
    }
    if !any_valid {
        return GatheredNulls::AllNull;
    }
    if !any_null {
        return GatheredNulls::AllValid;
    }
    let mut out = Bitmap::new_ones(indices.len());
    for (pos, &row) in indices.iter().enumerate() {
        if !nulls.is_valid(row) {
            let _ = out.set(pos, false);
        }
    }
    GatheredNulls::Mixed(out)
}

/// Validates an inner narrow block body's encoding, header, and byte length without decoding values.
pub(crate) fn validate_inner_take_body(
    body: &[u8],
    encoding: Encoding,
    subchunked_fsst: bool,
    dt: &DataType,
    row_count: usize,
) -> Result<(), StatamError> {
    match (encoding, dt) {
        (Encoding::Plain, DataType::Null) => {
            if !body.is_empty() {
                return Err(StatamError::Corrupt {
                    reason: "non-empty Plain Null block",
                });
            }
        }
        (Encoding::BitpackBool | Encoding::Plain, DataType::Bool) => {
            let expected = row_count.div_ceil(8);
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "bool block byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::I64 | DataType::TimestampNs { .. }) => {
            let expected = check_mul(row_count, 8, "plain i64 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain i64 byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::F64) => {
            let expected = check_mul(row_count, 8, "plain f64 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain f64 byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::Date) => {
            let expected = check_mul(row_count, 4, "plain date size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain date byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::Decimal128 { .. }) => {
            let expected = check_mul(row_count, 16, "plain decimal128 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain decimal128 byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::Interval) => {
            let expected = check_mul(row_count, 16, "plain interval size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain interval byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::FixedBytes8) => {
            let expected = check_mul(row_count, 8, "plain FixedBytes8 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes8 byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::FixedBytes16) => {
            let expected = check_mul(row_count, 16, "plain FixedBytes16 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes16 byte length mismatch",
                });
            }
        }
        (Encoding::Plain, DataType::Utf8 | DataType::Binary) => {
            let offsets_count = row_count
                .checked_add(1)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "plain varlen offsets count overflow",
                })?;
            let offsets_bytes = check_mul(offsets_count, 4, "plain varlen offsets size overflow")?;
            if body.len() < offsets_bytes {
                return Err(StatamError::Corrupt {
                    reason: "truncated plain varlen offsets",
                });
            }
            let data_len = body.len() - offsets_bytes;
            if data_len > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "plain varlen data exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let off_raw = &body[..offsets_bytes];
            validate_plain_offsets_raw(off_raw, data_len)?;
        }
        (Encoding::Fsst, DataType::Utf8 | DataType::Binary) => {
            if subchunked_fsst {
                let _ = FsstSubchunkedView::parse(body, row_count)?;
            } else {
                let _ = FsstBlockView::parse(body, row_count)?;
            }
        }
        (Encoding::ForBitpack, DataType::I64 | DataType::TimestampNs { .. }) => {
            let _ = parse_for_i64_header(body, row_count)?;
        }
        (Encoding::ForBitpack, DataType::Date) => {
            let _ = parse_for_date_header(body, row_count)?;
        }
        (Encoding::ForBitpack, DataType::Decimal128 { .. }) => {
            let _ = parse_for_decimal128_header(body, row_count)?;
        }
        (Encoding::DeltaBitpack, DataType::I64 | DataType::TimestampNs { .. } | DataType::Date) => {
            let _ = validate_delta_i64_header(body, row_count)?;
        }
        (Encoding::Alp, DataType::F64) => {
            validate_alp_body(body, row_count)?;
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "incompatible encoding for DataType",
            });
        }
    }
    Ok(())
}

fn decode_inner_body_take(
    body: &[u8],
    encoding: Encoding,
    subchunked_fsst: bool,
    row_count: usize,
    nulls: NullRef<'_>,
    dt: &DataType,
    indices: &[usize],
) -> Result<Column, StatamError> {
    if indices.is_empty() {
        validate_inner_take_body(body, encoding, subchunked_fsst, dt, row_count)?;
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }

    let compact_nulls = match gather_nulls(nulls, indices) {
        GatheredNulls::AllNull => {
            validate_inner_take_body(body, encoding, subchunked_fsst, dt, row_count)?;
            return Ok(Column::Const {
                value: ScalarValue::Null(dt.clone()),
                len: indices.len(),
            });
        }
        GatheredNulls::AllValid => None,
        GatheredNulls::Mixed(bm) => Some(bm),
    };
    let has_mixed_nulls = compact_nulls.is_some();
    let mut owned_full_nulls: Option<Bitmap> = None;

    let data = match (encoding, dt) {
        (Encoding::Plain, DataType::Null) => {
            if !body.is_empty() {
                return Err(StatamError::Corrupt {
                    reason: "non-empty Plain Null block",
                });
            }
            FlatData::Null(indices.len())
        }
        (Encoding::BitpackBool | Encoding::Plain, DataType::Bool) => {
            let expected = row_count.div_ceil(8);
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "bool block byte length mismatch",
                });
            }
            let mut bm = Bitmap::new_zeroed(indices.len());
            for (pos, &row) in indices.iter().enumerate() {
                if (!has_mixed_nulls || nulls.is_valid(row))
                    && ((body[row / 8] >> (row % 8)) & 1) != 0
                {
                    let _ = bm.set(pos, true);
                }
            }
            FlatData::Bool(bm)
        }
        (Encoding::Plain, DataType::I64 | DataType::TimestampNs { .. }) => {
            let expected = check_mul(row_count, 8, "plain i64 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain i64 byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(indices.len());
            for &row in indices {
                if has_mixed_nulls && !nulls.is_valid(row) {
                    vals.push(0);
                } else {
                    let off = row * 8;
                    vals.push(i64::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                        body[off + 4],
                        body[off + 5],
                        body[off + 6],
                        body[off + 7],
                    ]));
                }
            }
            match dt {
                DataType::I64 => FlatData::I64(vals),
                DataType::TimestampNs { utc } => FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                _ => unreachable!(),
            }
        }
        (Encoding::Plain, DataType::F64) => {
            let expected = check_mul(row_count, 8, "plain f64 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain f64 byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(indices.len());
            for &row in indices {
                if has_mixed_nulls && !nulls.is_valid(row) {
                    vals.push(0.0);
                } else {
                    let off = row * 8;
                    let bits = u64::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                        body[off + 4],
                        body[off + 5],
                        body[off + 6],
                        body[off + 7],
                    ]);
                    vals.push(f64::from_bits(bits));
                }
            }
            FlatData::F64(vals)
        }
        (Encoding::Plain, DataType::Date) => {
            let expected = check_mul(row_count, 4, "plain date size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain date byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(indices.len());
            for &row in indices {
                if has_mixed_nulls && !nulls.is_valid(row) {
                    vals.push(0);
                } else {
                    let off = row * 4;
                    vals.push(i32::from_le_bytes([
                        body[off],
                        body[off + 1],
                        body[off + 2],
                        body[off + 3],
                    ]));
                }
            }
            FlatData::Date(vals)
        }
        (Encoding::Plain, DataType::Decimal128 { precision, scale }) => {
            let expected = check_mul(row_count, 16, "plain decimal128 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain decimal128 byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(indices.len());
            for &row in indices {
                if has_mixed_nulls && !nulls.is_valid(row) {
                    vals.push(0);
                } else {
                    let off = row * 16;
                    let arr: [u8; 16] =
                        body[off..off + 16]
                            .try_into()
                            .map_err(|_| StatamError::Corrupt {
                                reason: "plain decimal128 slice mismatch",
                            })?;
                    vals.push(i128::from_le_bytes(arr));
                }
            }
            FlatData::Decimal128 {
                values: vals,
                precision: *precision,
                scale: *scale,
            }
        }
        (Encoding::Plain, DataType::Interval) => {
            let expected = check_mul(row_count, 16, "plain interval size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain interval byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(indices.len());
            for &row in indices {
                if has_mixed_nulls && !nulls.is_valid(row) {
                    vals.push(Interval {
                        months: 0,
                        days: 0,
                        nanos: 0,
                    });
                } else {
                    let off = row * 16;
                    vals.push(Interval {
                        months: i32::from_le_bytes([
                            body[off],
                            body[off + 1],
                            body[off + 2],
                            body[off + 3],
                        ]),
                        days: i32::from_le_bytes([
                            body[off + 4],
                            body[off + 5],
                            body[off + 6],
                            body[off + 7],
                        ]),
                        nanos: i64::from_le_bytes([
                            body[off + 8],
                            body[off + 9],
                            body[off + 10],
                            body[off + 11],
                            body[off + 12],
                            body[off + 13],
                            body[off + 14],
                            body[off + 15],
                        ]),
                    });
                }
            }
            FlatData::Interval(vals)
        }
        (Encoding::Plain, DataType::FixedBytes8) => {
            let expected = check_mul(row_count, 8, "plain FixedBytes8 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes8 byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(indices.len());
            for &row in indices {
                if has_mixed_nulls && !nulls.is_valid(row) {
                    vals.push([0u8; 8]);
                } else {
                    let off = row * 8;
                    let arr: [u8; 8] =
                        body[off..off + 8]
                            .try_into()
                            .map_err(|_| StatamError::Corrupt {
                                reason: "plain FixedBytes8 slice mismatch",
                            })?;
                    vals.push(arr);
                }
            }
            FlatData::FixedBytes8(vals)
        }
        (Encoding::Plain, DataType::FixedBytes16) => {
            let expected = check_mul(row_count, 16, "plain FixedBytes16 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes16 byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(indices.len());
            for &row in indices {
                if has_mixed_nulls && !nulls.is_valid(row) {
                    vals.push([0u8; 16]);
                } else {
                    let off = row * 16;
                    let arr: [u8; 16] =
                        body[off..off + 16]
                            .try_into()
                            .map_err(|_| StatamError::Corrupt {
                                reason: "plain FixedBytes16 slice mismatch",
                            })?;
                    vals.push(arr);
                }
            }
            FlatData::FixedBytes16(vals)
        }
        (Encoding::Plain, DataType::Utf8) => {
            let active_nulls = if has_mixed_nulls {
                nulls
            } else {
                NullRef::None
            };
            let (offsets, bytes) =
                decode_plain_varlen_take(body, row_count, indices, active_nulls)?;
            FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?)
        }
        (Encoding::Plain, DataType::Binary) => {
            let active_nulls = if has_mixed_nulls {
                nulls
            } else {
                NullRef::None
            };
            let (offsets, bytes) =
                decode_plain_varlen_take(body, row_count, indices, active_nulls)?;
            FlatData::Binary(BinaryData::try_new(offsets, bytes)?)
        }
        (Encoding::Fsst, DataType::Utf8) => {
            let full_nulls =
                nulls.resolve_bitmap(has_mixed_nulls, row_count, &mut owned_full_nulls)?;
            let (u, fsst_nulls) =
                decode_fsst_utf8_take(body, row_count, indices, full_nulls, subchunked_fsst)?;
            return Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(u),
                fsst_nulls,
            )?));
        }
        (Encoding::Fsst, DataType::Binary) => {
            let full_nulls =
                nulls.resolve_bitmap(has_mixed_nulls, row_count, &mut owned_full_nulls)?;
            let (b, fsst_nulls) =
                decode_fsst_binary_take(body, row_count, indices, full_nulls, subchunked_fsst)?;
            return Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Binary(b),
                fsst_nulls,
            )?));
        }
        (Encoding::Alp, DataType::F64) => {
            let full_nulls =
                nulls.resolve_bitmap(has_mixed_nulls, row_count, &mut owned_full_nulls)?;
            let vals = decode_alp_take(body, row_count, full_nulls, indices)?;
            FlatData::F64(vals)
        }
        (Encoding::ForBitpack, DataType::I64 | DataType::TimestampNs { .. }) => {
            let vals = if indices.len().saturating_mul(16) < row_count {
                let (base, bw, packed) = parse_for_i64_header(body, row_count)?;
                let mut out = Vec::with_capacity(indices.len());
                for &row in indices {
                    if has_mixed_nulls && !nulls.is_valid(row) {
                        out.push(0);
                    } else {
                        let d = unpack_u64_at(packed, row_count, bw, row)?;
                        out.push(base.wrapping_add(d as i64));
                    }
                }
                out
            } else {
                let full_nulls =
                    nulls.resolve_bitmap(has_mixed_nulls, row_count, &mut owned_full_nulls)?;
                let all = decode_for_i64_body(body, row_count, full_nulls)?;
                indices.iter().map(|&r| all[r]).collect()
            };
            match dt {
                DataType::I64 => FlatData::I64(vals),
                DataType::TimestampNs { utc } => FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                _ => unreachable!(),
            }
        }
        (Encoding::ForBitpack, DataType::Date) => {
            let vals = if indices.len().saturating_mul(16) < row_count {
                let (base, bw, packed) = parse_for_date_header(body, row_count)?;
                let mut out = Vec::with_capacity(indices.len());
                for &row in indices {
                    if has_mixed_nulls && !nulls.is_valid(row) {
                        out.push(0);
                    } else {
                        let d = unpack_u32_at(packed, row_count, bw, row)?;
                        out.push(base.wrapping_add(d as i32));
                    }
                }
                out
            } else {
                let full_nulls =
                    nulls.resolve_bitmap(has_mixed_nulls, row_count, &mut owned_full_nulls)?;
                let all = decode_for_date_body(body, row_count, full_nulls)?;
                indices.iter().map(|&r| all[r]).collect()
            };
            FlatData::Date(vals)
        }
        (Encoding::ForBitpack, DataType::Decimal128 { precision, scale }) => {
            let vals = if indices.len().saturating_mul(16) < row_count {
                let (base, bw, packed) = parse_for_decimal128_header(body, row_count)?;
                let mut out = Vec::with_capacity(indices.len());
                for &row in indices {
                    if has_mixed_nulls && !nulls.is_valid(row) {
                        out.push(0);
                    } else {
                        let d = unpack_u64_at(packed, row_count, bw, row)?;
                        out.push(base.wrapping_add(i128::from(d)));
                    }
                }
                out
            } else {
                let full_nulls =
                    nulls.resolve_bitmap(has_mixed_nulls, row_count, &mut owned_full_nulls)?;
                let all = decode_for_decimal128_body(body, row_count, full_nulls)?;
                indices.iter().map(|&r| all[r]).collect()
            };
            FlatData::Decimal128 {
                values: vals,
                precision: *precision,
                scale: *scale,
            }
        }
        (Encoding::DeltaBitpack, DataType::I64 | DataType::TimestampNs { .. } | DataType::Date) => {
            let decoded_i64 = if indices.len().saturating_mul(16) < row_count {
                let hdr = parse_delta_i64_header(body, row_count)?;
                let mut vals = delta_decode_i64_indices(
                    &hdr.bases,
                    hdr.packed,
                    row_count,
                    hdr.bw,
                    hdr.is_zigzag,
                    indices,
                )?;
                if has_mixed_nulls {
                    for (&row, slot) in indices.iter().zip(vals.iter_mut()) {
                        if !nulls.is_valid(row) {
                            *slot = 0;
                        }
                    }
                }
                vals
            } else {
                let full_nulls =
                    nulls.resolve_bitmap(has_mixed_nulls, row_count, &mut owned_full_nulls)?;
                let all = decode_delta_i64_body(body, row_count, full_nulls)?;
                indices.iter().map(|&r| all[r]).collect()
            };
            match dt {
                DataType::I64 => FlatData::I64(decoded_i64),
                DataType::TimestampNs { utc } => FlatData::TimestampNs {
                    values: decoded_i64,
                    utc: *utc,
                },
                DataType::Date => {
                    let mut dates = Vec::with_capacity(indices.len());
                    for (&row, &v) in indices.iter().zip(decoded_i64.iter()) {
                        if has_mixed_nulls && !nulls.is_valid(row) {
                            dates.push(0);
                        } else {
                            let d = i32::try_from(v).map_err(|_| StatamError::Corrupt {
                                reason: "delta decoded Date out of i32 range",
                            })?;
                            dates.push(d);
                        }
                    }
                    FlatData::Date(dates)
                }
                _ => unreachable!(),
            }
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "incompatible encoding for DataType",
            });
        }
    };

    Ok(Column::Flat(FlatColumn::try_new(data, compact_nulls)?))
}

/// Decodes only `run_indices` from an RLE inner value payload without unpacking untouched runs.
pub(crate) fn decode_inner_take(
    body: &[u8],
    inner_enc: Encoding,
    num_runs: usize,
    run_nulls: Option<&Bitmap>,
    dt: &DataType,
    run_indices: &[usize],
) -> Result<Column, StatamError> {
    let null_ref = match run_nulls {
        Some(bm) => NullRef::Bitmap(bm),
        None => NullRef::None,
    };
    decode_inner_body_take(body, inner_enc, false, num_runs, null_ref, dt, run_indices)
}

/// Decodes only `indices` of an `Encoding::Rle` mini-block directly into a compact
/// `indices.len()`-row [`Column`], bypassing `Bitmap`/`RleColumn`/`GlobalPool` allocation.
pub(crate) fn decode_rle_block_take(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    indices: &[usize],
) -> Result<Column, StatamError> {
    let row_count = entry.row_count as usize;
    if indices
        .len()
        .checked_mul(16)
        .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "narrow take indices exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    for &row in indices {
        if row >= row_count {
            return Err(StatamError::Corrupt {
                reason: "narrow take index out of bounds",
            });
        }
    }
    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty narrow block",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }

    let hdr = parse_rle_header(raw_body, row_count, entry.block_flags)?;
    let mut run_indices = Vec::with_capacity(indices.len());
    let mut run_idx = 0usize;
    let mut run_start = 0usize;
    for &row in indices {
        if row < run_start {
            run_idx = hdr.run_ends.partition_point(|&e| (e as usize) <= row);
        } else {
            while (hdr.run_ends[run_idx] as usize) <= row {
                run_idx += 1;
            }
        }
        run_start = if run_idx == 0 {
            0
        } else {
            hdr.run_ends[run_idx - 1] as usize
        };
        run_indices.push(run_idx);
    }

    decode_inner_take(
        hdr.inner_body,
        hdr.inner_enc,
        hdr.num_runs,
        hdr.run_nulls.as_ref(),
        dt,
        &run_indices,
    )
}

/// Decodes only `indices` of a narrow scalar mini-block directly into a compact
/// `indices.len()`-row [`Column`], returning `Ok(None)` when the encoding/type should
/// fall back to `decode_mini_block_selected` + `Column::take`.
pub(crate) fn decode_narrow_column_take(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    indices: &[usize],
) -> Result<Option<Column>, StatamError> {
    let row_count = entry.row_count as usize;
    if indices
        .len()
        .checked_mul(16)
        .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "narrow take indices exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    for &row in indices {
        if row >= row_count {
            return Err(StatamError::Corrupt {
                reason: "narrow take index out of bounds",
            });
        }
    }

    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        let col = decode_const_block(entry, dt)?;
        return match col {
            Column::Const { value, .. } => Ok(Some(Column::Const {
                value,
                len: indices.len(),
            })),
            other => Ok(Some(other)),
        };
    }

    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty narrow block",
            });
        }
        return Ok(Some(Column::Flat(FlatColumn::empty(dt))));
    }

    if entry.encoding == Encoding::Rle {
        return decode_rle_block_take(raw_body, entry, dt, indices).map(Some);
    }

    if entry.encoding == Encoding::Plain && *dt == DataType::MapUtf8 {
        return Ok(None);
    }

    let (null_bytes, body) = split_optional_null_bytes(raw_body, row_count, entry.block_flags)?;
    let null_ref = match null_bytes {
        Some(nb) => NullRef::Bytes(nb),
        None => NullRef::None,
    };
    let subchunked = (entry.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0;
    decode_inner_body_take(
        body,
        entry.encoding,
        subchunked,
        row_count,
        null_ref,
        dt,
        indices,
    )
    .map(Some)
}

/// Decodes a narrow scalar mini-block (`ConstNull`, `Const`, `Rle`, `ForBitpack`,
/// `DeltaBitpack`, `Alp`, `Fsst`, `BitpackBool`, `Plain`).
pub(crate) fn decode_narrow_column(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
) -> Result<Column, StatamError> {
    let row_count = entry.row_count as usize;
    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        return decode_const_block(entry, dt);
    }

    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty narrow block",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }

    if entry.encoding == Encoding::Rle {
        return decode_rle_block(raw_body, row_count, entry.block_flags, dt);
    }

    let (nulls, body) = read_optional_nulls(raw_body, row_count, entry.block_flags)?;
    if entry.encoding == Encoding::Fsst && (entry.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 {
        let data = match dt {
            DataType::Utf8 => FlatData::Utf8(decode_fsst_utf8_subchunked(
                body,
                row_count,
                MAX_BLOCK_RAW_BYTES,
            )?),
            DataType::Binary => FlatData::Binary(decode_fsst_binary_subchunked(
                body,
                row_count,
                MAX_BLOCK_RAW_BYTES,
            )?),
            _ => {
                return Err(StatamError::Corrupt {
                    reason: "incompatible encoding for DataType",
                });
            }
        };
        return Ok(Column::Flat(FlatColumn::try_new(data, nulls)?));
    }
    let flat = decode_inner_flat(body, entry.encoding, row_count, dt, nulls)?;
    Ok(Column::Flat(flat))
}

pub(crate) struct ParsedRleHeader<'a> {
    pub num_runs: usize,
    pub run_nulls: Option<Bitmap>,
    pub run_ends: Vec<u32>,
    pub inner_enc: Encoding,
    pub inner_body: &'a [u8],
}

pub(crate) fn parse_rle_header(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<ParsedRleHeader<'_>, StatamError> {
    if raw_body.len() < 4 {
        return Err(StatamError::Corrupt {
            reason: "truncated Rle num_runs",
        });
    }
    let num_runs =
        u32::from_le_bytes([raw_body[0], raw_body[1], raw_body[2], raw_body[3]]) as usize;
    if num_runs == 0 || num_runs > row_count {
        return Err(StatamError::Corrupt {
            reason: "invalid Rle num_runs",
        });
    }
    check_mul(num_runs, 4, "Rle num_runs exceeds MAX_BLOCK_RAW_BYTES")?;
    let (run_nulls, after_nulls) = read_optional_nulls(&raw_body[4..], num_runs, block_flags)?;
    let Some(&re_bw) = after_nulls.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated Rle run_ends bit_width",
        });
    };
    if re_bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid Rle run_ends bit_width",
        });
    }
    let re_bytes = packed_bytes_len(num_runs, re_bw)?;
    if after_nulls.len() < 1 + re_bytes + 1 {
        return Err(StatamError::Corrupt {
            reason: "truncated Rle body",
        });
    }
    let mut run_ends = Vec::with_capacity(num_runs);
    unpack_u32(
        &after_nulls[1..1 + re_bytes],
        num_runs,
        re_bw,
        &mut run_ends,
    )?;
    let mut prev = 0u32;
    for &end in &run_ends {
        if end <= prev || (end as usize) > row_count {
            return Err(StatamError::Corrupt {
                reason: "invalid Rle run_ends monotonicity",
            });
        }
        prev = end;
    }
    if prev as usize != row_count {
        return Err(StatamError::Corrupt {
            reason: "Rle run_ends do not sum to row_count",
        });
    }

    let inner_enc = Encoding::from_u8(after_nulls[1 + re_bytes])?;
    let inner_body = &after_nulls[1 + re_bytes + 1..];
    Ok(ParsedRleHeader {
        num_runs,
        run_nulls,
        run_ends,
        inner_enc,
        inner_body,
    })
}

pub(crate) fn decode_rle_block(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    dt: &DataType,
) -> Result<Column, StatamError> {
    let hdr = parse_rle_header(raw_body, row_count, block_flags)?;
    let run_values = decode_inner_flat(
        hdr.inner_body,
        hdr.inner_enc,
        hdr.num_runs,
        dt,
        hdr.run_nulls,
    )?;
    let rle = RleColumn::try_new(run_values, hdr.run_ends)?;
    Ok(Column::Rle(rle))
}

pub(crate) fn decode_inner_flat(
    body: &[u8],
    encoding: Encoding,
    row_count: usize,
    dt: &DataType,
    nulls: Option<Bitmap>,
) -> Result<FlatColumn, StatamError> {
    let data = decode_inner_data(body, encoding, row_count, dt, nulls.as_ref())?;
    Ok(FlatColumn::try_new(data, nulls)?)
}

pub(crate) fn decode_inner_data(
    body: &[u8],
    encoding: Encoding,
    row_count: usize,
    dt: &DataType,
    nulls: Option<&Bitmap>,
) -> Result<FlatData, StatamError> {
    let data = match (encoding, dt) {
        (Encoding::BitpackBool | Encoding::Plain, DataType::Bool) => {
            let expected = row_count.div_ceil(8);
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "bool block byte length mismatch",
                });
            }
            let mut bm = Bitmap::from_bytes(body, row_count)?;
            if let Some(nbm) = nulls {
                for i in 0..row_count {
                    if !nbm.get(i) {
                        let _ = bm.set(i, false);
                    }
                }
            }
            FlatData::Bool(bm)
        }
        (Encoding::ForBitpack, DataType::I64 | DataType::TimestampNs { .. }) => {
            let vals = decode_for_i64_body(body, row_count, nulls)?;
            match dt {
                DataType::I64 => FlatData::I64(vals),
                DataType::TimestampNs { utc } => FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                _ => unreachable!(),
            }
        }
        (Encoding::ForBitpack, DataType::Date) => {
            let vals = decode_for_date_body(body, row_count, nulls)?;
            FlatData::Date(vals)
        }
        (Encoding::ForBitpack, DataType::Decimal128 { precision, scale }) => {
            let vals = decode_for_decimal128_body(body, row_count, nulls)?;
            FlatData::Decimal128 {
                values: vals,
                precision: *precision,
                scale: *scale,
            }
        }
        (Encoding::DeltaBitpack, DataType::I64 | DataType::TimestampNs { .. } | DataType::Date) => {
            let vals = decode_delta_i64_body(body, row_count, nulls)?;
            match dt {
                DataType::I64 => FlatData::I64(vals),
                DataType::TimestampNs { utc } => FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                DataType::Date => {
                    let mut dates = Vec::with_capacity(row_count);
                    for (i, &v) in vals.iter().enumerate() {
                        if nulls.is_some_and(|bm| !bm.get(i)) {
                            dates.push(0);
                        } else {
                            let d = i32::try_from(v).map_err(|_| StatamError::Corrupt {
                                reason: "delta decoded Date out of i32 range",
                            })?;
                            dates.push(d);
                        }
                    }
                    FlatData::Date(dates)
                }
                _ => unreachable!(),
            }
        }
        (Encoding::Alp, DataType::F64) => {
            let vals = decode_alp_body(body, row_count, nulls)?;
            FlatData::F64(vals)
        }
        (Encoding::Fsst, DataType::Utf8) => {
            let u = decode_fsst_utf8(body, row_count)?;
            FlatData::Utf8(u)
        }
        (Encoding::Fsst, DataType::Binary) => {
            let b = decode_fsst_binary(body, row_count)?;
            FlatData::Binary(b)
        }
        (Encoding::Plain, DataType::Null) => {
            if !body.is_empty() {
                return Err(StatamError::Corrupt {
                    reason: "non-empty Plain Null block",
                });
            }
            FlatData::Null(row_count)
        }
        (Encoding::Plain, DataType::I64) => {
            let vals = decode_plain_i64(body, row_count)?;
            FlatData::I64(vals)
        }
        (Encoding::Plain, DataType::TimestampNs { utc }) => {
            let vals = decode_plain_i64(body, row_count)?;
            FlatData::TimestampNs {
                values: vals,
                utc: *utc,
            }
        }
        (Encoding::Plain, DataType::F64) => {
            let expected = check_mul(row_count, 8, "plain f64 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain f64 byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(row_count);
            for &chunk in body.as_chunks::<8>().0 {
                vals.push(f64::from_bits(u64::from_le_bytes(chunk)));
            }
            FlatData::F64(vals)
        }
        (Encoding::Plain, DataType::Decimal128 { precision, scale }) => {
            let expected = check_mul(row_count, 16, "plain decimal128 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain decimal128 byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(row_count);
            for &chunk in body.as_chunks::<16>().0 {
                vals.push(i128::from_le_bytes(chunk));
            }
            FlatData::Decimal128 {
                values: vals,
                precision: *precision,
                scale: *scale,
            }
        }
        (Encoding::Plain, DataType::Date) => {
            let expected = check_mul(row_count, 4, "plain date size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain date byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(row_count);
            for &chunk in body.as_chunks::<4>().0 {
                vals.push(i32::from_le_bytes(chunk));
            }
            FlatData::Date(vals)
        }
        (Encoding::Plain, DataType::Interval) => {
            let expected = check_mul(row_count, 16, "plain interval size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain interval byte length mismatch",
                });
            }
            let mut vals = Vec::with_capacity(row_count);
            for &chunk in body.as_chunks::<16>().0 {
                let months = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                let days = i32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
                let nanos = i64::from_le_bytes([
                    chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14],
                    chunk[15],
                ]);
                vals.push(Interval {
                    months,
                    days,
                    nanos,
                });
            }
            FlatData::Interval(vals)
        }
        (Encoding::Plain, DataType::FixedBytes8) => {
            let expected = check_mul(row_count, 8, "plain FixedBytes8 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes8 byte length mismatch",
                });
            }
            FlatData::FixedBytes8(body.as_chunks::<8>().0.to_vec())
        }
        (Encoding::Plain, DataType::FixedBytes16) => {
            let expected = check_mul(row_count, 16, "plain FixedBytes16 size overflow")?;
            if body.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "plain FixedBytes16 byte length mismatch",
                });
            }
            FlatData::FixedBytes16(body.as_chunks::<16>().0.to_vec())
        }
        (Encoding::Plain, DataType::Utf8) => {
            let (offsets, data_bytes) = decode_plain_varlen(body, row_count)?;
            FlatData::Utf8(Utf8Data::try_new(offsets, data_bytes)?)
        }
        (Encoding::Plain, DataType::Binary) => {
            let (offsets, data_bytes) = decode_plain_varlen(body, row_count)?;
            FlatData::Binary(BinaryData::try_new(offsets, data_bytes)?)
        }
        (Encoding::Plain, DataType::MapUtf8) => decode_plain_map(body, row_count)?,
        _ => {
            return Err(StatamError::Corrupt {
                reason: "incompatible encoding for DataType",
            });
        }
    };

    Ok(data)
}

fn check_mul(a: usize, b: usize, reason: &'static str) -> Result<usize, StatamError> {
    let total = a
        .checked_mul(b)
        .ok_or(StatamError::BoundsExceeded { reason })?;
    if total > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded { reason });
    }
    Ok(total)
}

fn decode_plain_i64(body: &[u8], row_count: usize) -> Result<Vec<i64>, StatamError> {
    let expected = check_mul(row_count, 8, "plain i64 size overflow")?;
    if body.len() != expected {
        return Err(StatamError::Corrupt {
            reason: "plain i64 byte length mismatch",
        });
    }
    let mut vals = Vec::with_capacity(row_count);
    for &chunk in body.as_chunks::<8>().0 {
        vals.push(i64::from_le_bytes(chunk));
    }
    Ok(vals)
}

fn decode_plain_varlen(body: &[u8], row_count: usize) -> Result<(Vec<u32>, Vec<u8>), StatamError> {
    let offsets_count = row_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain varlen offsets count overflow",
        })?;
    let offsets_bytes = check_mul(offsets_count, 4, "plain varlen offsets size overflow")?;
    if body.len() < offsets_bytes {
        return Err(StatamError::Corrupt {
            reason: "truncated plain varlen offsets",
        });
    }
    let mut offsets = Vec::with_capacity(offsets_count);
    for &chunk in body[..offsets_bytes].as_chunks::<4>().0 {
        offsets.push(u32::from_le_bytes(chunk));
    }
    let data_len = body.len() - offsets_bytes;
    if data_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain varlen data exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let data_bytes = body[offsets_bytes..].to_vec();
    Ok((offsets, data_bytes))
}

fn decode_plain_map(body: &[u8], row_count: usize) -> Result<FlatData, StatamError> {
    let offsets_count = row_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain map offsets count overflow",
        })?;
    let offsets_bytes = check_mul(offsets_count, 4, "plain map offsets size overflow")?;
    if body.len() < offsets_bytes {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map offsets",
        });
    }
    let mut offsets = Vec::with_capacity(offsets_count);
    let mut prev = 0u32;
    for (idx, &chunk) in body[..offsets_bytes].as_chunks::<4>().0.iter().enumerate() {
        let v = u32::from_le_bytes(chunk);
        if (idx == 0 && v != 0) || v < prev {
            return Err(StatamError::Corrupt {
                reason: "non-monotonic plain map offsets",
            });
        }
        prev = v;
        offsets.push(v);
    }
    let entry_count = prev as usize;
    if entry_count > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain map entry_count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let rest = &body[offsets_bytes..];
    if entry_count == 0 {
        if !rest.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty plain map",
            });
        }
        let map = MapData::try_new(offsets, Utf8Data::empty(), Utf8Data::empty(), None)?;
        return Ok(FlatData::MapUtf8(Box::new(map)));
    }

    let Some(&has_vn) = rest.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map value_nulls flag",
        });
    };
    let rest = &rest[1..];
    let (val_nulls, rest) = match has_vn {
        0 => (None, rest),
        1 => {
            let vn_bytes = entry_count.div_ceil(8);
            if rest.len() < vn_bytes {
                return Err(StatamError::Corrupt {
                    reason: "truncated plain map value_nulls bitmap",
                });
            }
            let bm = Bitmap::from_bytes(&rest[..vn_bytes], entry_count)?;
            (Some(bm), &rest[vn_bytes..])
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid plain map value_nulls flag",
            });
        }
    };

    if rest.len() < 4 {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map keys_bytes_len",
        });
    }
    let keys_bytes_len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    if keys_bytes_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain map keys_bytes_len exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let rest = &rest[4..];

    let kv_offsets_count = entry_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain map kv offsets overflow",
        })?;
    let kv_offsets_bytes = check_mul(kv_offsets_count, 4, "plain map kv offsets size overflow")?;
    let keys_total =
        kv_offsets_bytes
            .checked_add(keys_bytes_len)
            .ok_or(StatamError::BoundsExceeded {
                reason: "plain map keys section overflow",
            })?;
    let min_needed =
        keys_total
            .checked_add(kv_offsets_bytes)
            .ok_or(StatamError::BoundsExceeded {
                reason: "plain map total payload overflow",
            })?;
    if rest.len() < min_needed {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map keys/values payload",
        });
    }

    let mut key_offsets = Vec::with_capacity(kv_offsets_count);
    for &chunk in rest[..kv_offsets_bytes].as_chunks::<4>().0 {
        key_offsets.push(u32::from_le_bytes(chunk));
    }
    let key_bytes = rest[kv_offsets_bytes..keys_total].to_vec();
    let keys = Utf8Data::try_new(key_offsets, key_bytes)?;

    let v_rest = &rest[keys_total..];
    let val_bytes_len = v_rest.len() - kv_offsets_bytes;
    if val_bytes_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain map values exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    let mut val_offsets = Vec::with_capacity(kv_offsets_count);
    for &chunk in v_rest[..kv_offsets_bytes].as_chunks::<4>().0 {
        val_offsets.push(u32::from_le_bytes(chunk));
    }
    let val_bytes = v_rest[kv_offsets_bytes..].to_vec();
    let vals = Utf8Data::try_new(val_offsets, val_bytes)?;

    let map = MapData::try_new(offsets, keys, vals, val_nulls)?;
    Ok(FlatData::MapUtf8(Box::new(map)))
}

pub(crate) fn validate_plain_offsets_raw(
    offsets_bytes: &[u8],
    expected_last: usize,
) -> Result<(), StatamError> {
    let chunks = offsets_bytes.as_chunks::<4>().0;
    let mut prev = 0u32;
    for (idx, &chunk) in chunks.iter().enumerate() {
        let v = u32::from_le_bytes(chunk);
        if (idx == 0 && v != 0) || v < prev {
            return Err(StatamError::Corrupt {
                reason: "non-monotonic plain varlen offsets",
            });
        }
        prev = v;
    }
    if prev as usize != expected_last {
        return Err(StatamError::Corrupt {
            reason: "plain varlen last offset does not match data length",
        });
    }
    Ok(())
}

#[inline]
pub(crate) fn read_u32_le_at(raw: &[u8], idx: usize) -> usize {
    let off = idx * 4;
    u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]) as usize
}

pub(crate) fn decode_plain_varlen_selected(
    body: &[u8],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
) -> Result<(Vec<u32>, Vec<u8>), StatamError> {
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
    let offsets_count = row_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain varlen offsets count overflow",
        })?;
    let offsets_bytes = check_mul(offsets_count, 4, "plain varlen offsets size overflow")?;
    if body.len() < offsets_bytes {
        return Err(StatamError::Corrupt {
            reason: "truncated plain varlen offsets",
        });
    }
    let data_len = body.len() - offsets_bytes;
    if data_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain varlen data exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let off_raw = &body[..offsets_bytes];
    validate_plain_offsets_raw(off_raw, data_len)?;
    let data = &body[offsets_bytes..];

    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut out_bytes = Vec::new();
    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        let cur_off = out_bytes.len() as u32;
        if out_offsets.len() <= row {
            out_offsets.resize(row + 1, cur_off);
        }
        if nulls.is_none_or(|bm| bm.get(row)) {
            let start = read_u32_le_at(off_raw, row);
            let end = read_u32_le_at(off_raw, row + 1);
            out_bytes.extend_from_slice(&data[start..end]);
        }
        out_offsets.push(out_bytes.len() as u32);
    }
    let tail_off = out_bytes.len() as u32;
    if out_offsets.len() < row_count + 1 {
        out_offsets.resize(row_count + 1, tail_off);
    }
    Ok((out_offsets, out_bytes))
}

pub(crate) fn decode_plain_map_selected(
    body: &[u8],
    row_count: usize,
    mask: &Bitmap,
) -> Result<FlatData, StatamError> {
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    let offsets_count = row_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain map offsets count overflow",
        })?;
    let offsets_bytes = check_mul(offsets_count, 4, "plain map offsets size overflow")?;
    if body.len() < offsets_bytes {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map offsets",
        });
    }
    let row_off_raw = &body[..offsets_bytes];
    let mut prev = 0u32;
    for (idx, &chunk) in row_off_raw.as_chunks::<4>().0.iter().enumerate() {
        let v = u32::from_le_bytes(chunk);
        if (idx == 0 && v != 0) || v < prev {
            return Err(StatamError::Corrupt {
                reason: "non-monotonic plain map offsets",
            });
        }
        prev = v;
    }
    let entry_count = prev as usize;
    if entry_count > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain map entry_count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let rest = &body[offsets_bytes..];
    if entry_count == 0 {
        if !rest.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty plain map",
            });
        }
        let map = MapData::try_new(
            vec![0u32; row_count + 1],
            Utf8Data::empty(),
            Utf8Data::empty(),
            None,
        )?;
        return Ok(FlatData::MapUtf8(Box::new(map)));
    }

    let Some(&has_vn) = rest.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map value_nulls flag",
        });
    };
    let rest = &rest[1..];
    let (val_nulls, rest) = match has_vn {
        0 => (None, rest),
        1 => {
            let vn_bytes = entry_count.div_ceil(8);
            if rest.len() < vn_bytes {
                return Err(StatamError::Corrupt {
                    reason: "truncated plain map value_nulls bitmap",
                });
            }
            let bm = Bitmap::from_bytes(&rest[..vn_bytes], entry_count)?;
            (Some(bm), &rest[vn_bytes..])
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid plain map value_nulls flag",
            });
        }
    };

    if rest.len() < 4 {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map keys_bytes_len",
        });
    }
    let keys_bytes_len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    if keys_bytes_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain map keys_bytes_len exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let rest = &rest[4..];

    let kv_offsets_count = entry_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain map kv offsets overflow",
        })?;
    let kv_offsets_bytes = check_mul(kv_offsets_count, 4, "plain map kv offsets size overflow")?;
    let keys_total =
        kv_offsets_bytes
            .checked_add(keys_bytes_len)
            .ok_or(StatamError::BoundsExceeded {
                reason: "plain map keys section overflow",
            })?;
    let min_needed =
        keys_total
            .checked_add(kv_offsets_bytes)
            .ok_or(StatamError::BoundsExceeded {
                reason: "plain map total payload overflow",
            })?;
    if rest.len() < min_needed {
        return Err(StatamError::Corrupt {
            reason: "truncated plain map keys/values payload",
        });
    }

    let key_off_raw = &rest[..kv_offsets_bytes];
    validate_plain_offsets_raw(key_off_raw, keys_bytes_len)?;
    let key_blob = &rest[kv_offsets_bytes..keys_total];

    let v_rest = &rest[keys_total..];
    let val_bytes_len = v_rest.len() - kv_offsets_bytes;
    if val_bytes_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain map values exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    let val_off_raw = &v_rest[..kv_offsets_bytes];
    validate_plain_offsets_raw(val_off_raw, val_bytes_len)?;
    let val_blob = &v_rest[kv_offsets_bytes..];

    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut out_key_offsets = vec![0u32];
    let mut out_key_bytes = Vec::new();
    let mut out_val_offsets = vec![0u32];
    let mut out_val_bytes = Vec::new();
    let mut out_val_valid = Vec::new();

    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        let cur_entries = out_val_valid.len() as u32;
        if out_offsets.len() <= row {
            out_offsets.resize(row + 1, cur_entries);
        }
        let rs = read_u32_le_at(row_off_raw, row);
        let re = read_u32_le_at(row_off_raw, row + 1);
        for idx in rs..re {
            let ks = read_u32_le_at(key_off_raw, idx);
            let ke = read_u32_le_at(key_off_raw, idx + 1);
            out_key_bytes.extend_from_slice(&key_blob[ks..ke]);
            out_key_offsets.push(out_key_bytes.len() as u32);

            let vs = read_u32_le_at(val_off_raw, idx);
            let ve = read_u32_le_at(val_off_raw, idx + 1);
            out_val_bytes.extend_from_slice(&val_blob[vs..ve]);
            out_val_offsets.push(out_val_bytes.len() as u32);

            out_val_valid.push(val_nulls.as_ref().is_none_or(|bm| bm.get(idx)));
        }
        out_offsets.push(out_val_valid.len() as u32);
    }
    let tail_entries = out_val_valid.len() as u32;
    if out_offsets.len() < row_count + 1 {
        out_offsets.resize(row_count + 1, tail_entries);
    }

    let keys = Utf8Data::try_new(out_key_offsets, out_key_bytes)?;
    let vals = Utf8Data::try_new(out_val_offsets, out_val_bytes)?;
    let out_val_nulls = if out_val_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_val_valid))
    };
    let map = MapData::try_new(out_offsets, keys, vals, out_val_nulls)?;
    Ok(FlatData::MapUtf8(Box::new(map)))
}

fn decode_plain_varlen_take(
    body: &[u8],
    row_count: usize,
    indices: &[usize],
    nulls: NullRef<'_>,
) -> Result<(Vec<u32>, Vec<u8>), StatamError> {
    nulls.check_len(row_count)?;
    let offsets_count = row_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain varlen offsets count overflow",
        })?;
    let offsets_bytes = check_mul(offsets_count, 4, "plain varlen offsets size overflow")?;
    if body.len() < offsets_bytes {
        return Err(StatamError::Corrupt {
            reason: "truncated plain varlen offsets",
        });
    }
    let data_len = body.len() - offsets_bytes;
    if data_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain varlen data exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let off_raw = &body[..offsets_bytes];
    validate_plain_offsets_raw(off_raw, data_len)?;
    let data = &body[offsets_bytes..];

    let mut out_offsets = Vec::with_capacity(indices.len() + 1);
    out_offsets.push(0u32);
    let mut out_bytes = Vec::new();
    for &row in indices {
        if nulls.is_valid(row) {
            let start = read_u32_le_at(off_raw, row);
            let end = read_u32_le_at(off_raw, row + 1);
            if out_bytes
                .len()
                .checked_add(end - start)
                .is_none_or(|n| n > MAX_BLOCK_RAW_BYTES)
            {
                return Err(StatamError::BoundsExceeded {
                    reason: "plain varlen take exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            out_bytes.extend_from_slice(&data[start..end]);
        }
        let off = u32::try_from(out_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "plain varlen take offset exceeds u32::MAX",
        })?;
        out_offsets.push(off);
    }
    Ok((out_offsets, out_bytes))
}

fn parse_for_i64_header(body: &[u8], row_count: usize) -> Result<(i64, u8, &[u8]), StatamError> {
    check_mul(row_count, 8, "ForBitpack i64 exceeds MAX_BLOCK_RAW_BYTES")?;
    if body.len() < 9 {
        return Err(StatamError::Corrupt {
            reason: "truncated ForBitpack i64 header",
        });
    }
    let base = i64::from_le_bytes([
        body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
    ]);
    let bw = body[8];
    if bw > 64 {
        return Err(StatamError::Corrupt {
            reason: "invalid u64 bit_width",
        });
    }
    let needed = packed_bytes_len(row_count, bw)?;
    if body.len() != 9 + needed {
        return Err(StatamError::Corrupt {
            reason: "ForBitpack i64 length mismatch",
        });
    }
    Ok((base, bw, &body[9..]))
}

fn parse_for_date_header(body: &[u8], row_count: usize) -> Result<(i32, u8, &[u8]), StatamError> {
    check_mul(row_count, 4, "ForBitpack Date exceeds MAX_BLOCK_RAW_BYTES")?;
    if body.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated ForBitpack Date header",
        });
    }
    let base = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let bw = body[4];
    if bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid u32 bit_width",
        });
    }
    let needed = packed_bytes_len(row_count, bw)?;
    if body.len() != 5 + needed {
        return Err(StatamError::Corrupt {
            reason: "ForBitpack Date length mismatch",
        });
    }
    Ok((base, bw, &body[5..]))
}

fn parse_for_decimal128_header(
    body: &[u8],
    row_count: usize,
) -> Result<(i128, u8, &[u8]), StatamError> {
    check_mul(
        row_count,
        16,
        "ForBitpack Decimal128 exceeds MAX_BLOCK_RAW_BYTES",
    )?;
    if body.len() < 17 {
        return Err(StatamError::Corrupt {
            reason: "truncated ForBitpack Decimal128 header",
        });
    }
    let base_arr: [u8; 16] = body[0..16].try_into().map_err(|_| StatamError::Corrupt {
        reason: "invalid ForBitpack Decimal128 base",
    })?;
    let base = i128::from_le_bytes(base_arr);
    let bw = body[16];
    if bw > 64 {
        return Err(StatamError::Corrupt {
            reason: "invalid u64 bit_width",
        });
    }
    let needed = packed_bytes_len(row_count, bw)?;
    if body.len() != 17 + needed {
        return Err(StatamError::Corrupt {
            reason: "ForBitpack Decimal128 length mismatch",
        });
    }
    Ok((base, bw, &body[17..]))
}

struct DeltaI64HeaderMeta<'a> {
    is_zigzag: bool,
    bw: u8,
    num_bases: usize,
    bases_raw: &'a [u8],
    packed: &'a [u8],
}

fn validate_delta_i64_header(
    body: &[u8],
    row_count: usize,
) -> Result<DeltaI64HeaderMeta<'_>, StatamError> {
    check_mul(row_count, 8, "DeltaBitpack i64 exceeds MAX_BLOCK_RAW_BYTES")?;
    if body.len() < 2 {
        return Err(StatamError::Corrupt {
            reason: "truncated DeltaBitpack header",
        });
    }
    let is_zigzag = match body[0] {
        0 => false,
        1 => true,
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid DeltaBitpack is_zigzag flag",
            });
        }
    };
    let bw = body[1];
    if bw > 64 {
        return Err(StatamError::Corrupt {
            reason: "invalid DeltaBitpack bit_width",
        });
    }
    let num_bases = delta_bases_len(row_count);
    let bases_bytes = check_mul(num_bases, 8, "delta bases size overflow")?;
    let deltas_bytes = packed_bytes_len(row_count, bw)?;
    let expected_total = 2usize
        .checked_add(bases_bytes)
        .and_then(|v| v.checked_add(deltas_bytes))
        .ok_or(StatamError::BoundsExceeded {
            reason: "delta block length overflow",
        })?;
    if body.len() != expected_total {
        return Err(StatamError::Corrupt {
            reason: "DeltaBitpack byte length mismatch",
        });
    }
    Ok(DeltaI64HeaderMeta {
        is_zigzag,
        bw,
        num_bases,
        bases_raw: &body[2..2 + bases_bytes],
        packed: &body[2 + bases_bytes..],
    })
}

struct DeltaI64Header<'a> {
    is_zigzag: bool,
    bw: u8,
    bases: Vec<i64>,
    packed: &'a [u8],
}

fn parse_delta_i64_header(
    body: &[u8],
    row_count: usize,
) -> Result<DeltaI64Header<'_>, StatamError> {
    let meta = validate_delta_i64_header(body, row_count)?;
    let mut bases = Vec::with_capacity(meta.num_bases);
    for &chunk in meta.bases_raw.as_chunks::<8>().0 {
        bases.push(i64::from_le_bytes(chunk));
    }
    Ok(DeltaI64Header {
        is_zigzag: meta.is_zigzag,
        bw: meta.bw,
        bases,
        packed: meta.packed,
    })
}

fn decode_for_i64_body(
    body: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<i64>, StatamError> {
    let (base, bw, packed) = parse_for_i64_header(body, row_count)?;
    let mut deltas = Vec::with_capacity(row_count);
    unpack_u64(packed, row_count, bw, &mut deltas)?;
    let mut out = Vec::with_capacity(row_count);
    for (i, d) in deltas.into_iter().enumerate() {
        if nulls.is_some_and(|bm| !bm.get(i)) {
            out.push(0);
        } else {
            out.push(base.wrapping_add(d as i64));
        }
    }
    Ok(out)
}

fn decode_for_date_body(
    body: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<i32>, StatamError> {
    let (base, bw, packed) = parse_for_date_header(body, row_count)?;
    let mut deltas = Vec::with_capacity(row_count);
    unpack_u32(packed, row_count, bw, &mut deltas)?;
    let mut out = Vec::with_capacity(row_count);
    for (i, d) in deltas.into_iter().enumerate() {
        if nulls.is_some_and(|bm| !bm.get(i)) {
            out.push(0);
        } else {
            out.push(base.wrapping_add(d as i32));
        }
    }
    Ok(out)
}

fn decode_for_decimal128_body(
    body: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<i128>, StatamError> {
    let (base, bw, packed) = parse_for_decimal128_header(body, row_count)?;
    let mut deltas = Vec::with_capacity(row_count);
    unpack_u64(packed, row_count, bw, &mut deltas)?;
    let mut out = Vec::with_capacity(row_count);
    for (i, d) in deltas.into_iter().enumerate() {
        if nulls.is_some_and(|bm| !bm.get(i)) {
            out.push(0);
        } else {
            out.push(base.wrapping_add(i128::from(d)));
        }
    }
    Ok(out)
}

fn decode_delta_i64_body(
    body: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<i64>, StatamError> {
    let hdr = parse_delta_i64_header(body, row_count)?;
    let mut packed_deltas = Vec::with_capacity(row_count);
    unpack_u64(hdr.packed, row_count, hdr.bw, &mut packed_deltas)?;
    let mut deltas = Vec::with_capacity(row_count);
    if hdr.is_zigzag {
        for d in packed_deltas {
            deltas.push(zigzag_decode_i64(d));
        }
    } else {
        for d in packed_deltas {
            deltas.push(d as i64);
        }
    }

    let mut out = Vec::with_capacity(row_count);
    delta_decode_i64(&hdr.bases, &deltas, row_count, &mut out)?;
    if let Some(bm) = nulls {
        for (i, slot) in out.iter_mut().enumerate() {
            if !bm.get(i) {
                *slot = 0;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_and_delta_decoders_reject_huge_row_count_with_bw0_before_alloc() {
        let huge_rows = MAX_BLOCK_RAW_BYTES; // 64M * elem_bytes > 64 MiB for all 4/8/16-byte types
        let for_i64_bw0 = [0u8; 9];
        assert!(matches!(
            decode_for_i64_body(&for_i64_bw0, huge_rows, None),
            Err(StatamError::BoundsExceeded { .. })
        ));

        let for_date_bw0 = [0u8; 5];
        assert!(matches!(
            decode_for_date_body(&for_date_bw0, huge_rows, None),
            Err(StatamError::BoundsExceeded { .. })
        ));

        let for_dec_bw0 = [0u8; 17];
        assert!(matches!(
            decode_for_decimal128_body(&for_dec_bw0, huge_rows, None),
            Err(StatamError::BoundsExceeded { .. })
        ));

        let delta_bw0 = [0u8; 2];
        assert!(matches!(
            decode_delta_i64_body(&delta_bw0, huge_rows, None),
            Err(StatamError::BoundsExceeded { .. })
        ));
    }

    #[test]
    fn decode_const_block_rejects_inline_len_over_16() {
        let bad_entry = BlockEntry {
            first_row: 0,
            row_count: 4,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding: Encoding::Const,
            codec: Codec::None,
            block_flags: 0,
            min_inline_len: 17,
            max_inline_len: 17,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        assert!(matches!(
            decode_const_block(&bad_entry, &DataType::Utf8),
            Err(StatamError::Corrupt {
                reason: "Const inline length exceeds 16",
            })
        ));
    }

    #[test]
    fn decode_narrow_column_take_rejects_malformed_plain_varlen_offsets_on_all_null_and_empty() {
        // M2 regression: crafted Plain Utf8/Binary block with valid endpoints (0 and data_len)
        // but non-monotonic interior offsets must be rejected even on all-null or empty indices.
        let mut raw_body = Vec::new();
        // Null bitmap for 3 all-null rows (1 byte = 0x00).
        raw_body.push(0x00);
        // 4 offsets for 3 rows: [0, u32::MAX, 0, 4]
        for off in [0u32, u32::MAX, 0, 4] {
            raw_body.extend_from_slice(&off.to_le_bytes());
        }
        raw_body.extend_from_slice(b"abcd");

        let entry = BlockEntry {
            first_row: 0,
            row_count: 3,
            null_count: 3,
            offset: 0,
            stored_len: raw_body.len() as u32,
            raw_len: raw_body.len() as u32,
            encoding: Encoding::Plain,
            codec: Codec::None,
            block_flags: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };

        for dt in [DataType::Utf8, DataType::Binary] {
            for indices in [&[][..], &[0usize, 2][..]] {
                let err = decode_narrow_column_take(&raw_body, &entry, &dt, indices).unwrap_err();
                assert!(
                    matches!(
                        err,
                        StatamError::Corrupt {
                            reason: "non-monotonic plain varlen offsets",
                        }
                    ),
                    "expected non-monotonic offsets error for {dt:?}, indices={indices:?}, got {err:?}"
                );
            }
        }
    }

    #[test]
    fn pass16a_compute_flat_runs_and_sample_64_parity_95pct_null_and_all_variants() {
        // 95%-NULL wide-table pattern: long runs of NULLs interrupted by occasional values.
        let n = 128usize;
        let mut valid_bits = vec![false; n];
        valid_bits[10] = true;
        valid_bits[11] = true;
        valid_bits[50] = true;
        valid_bits[100] = true;
        valid_bits[101] = true;
        valid_bits[102] = true;
        let nulls = Bitmap::from_bools(&valid_bits);

        let verify_runs = |flat: &FlatColumn, expected_ends: &[u32]| {
            assert!(has_runs_sample_64(flat));
            let (run_vals, run_ends) = compute_flat_runs(flat).unwrap().unwrap();
            assert_eq!(run_ends, expected_ends);
            let rle = Column::Rle(RleColumn::try_new(run_vals, run_ends).unwrap());
            let mat = rle.materialize(None).unwrap();
            assert!(mat.semantic_eq(flat));
        };

        // Same value across [10, 11] and [100, 101, 102] -> 7 runs:
        // [0..10)=N, [10..12)=V, [12..50)=N, [50..51)=V, [51..100)=N, [100..103)=V, [103..128)=N.
        let expected_7 = [10u32, 12, 50, 51, 100, 103, 128];
        let mut i64s = vec![0i64; n];
        i64s[10] = 42;
        i64s[11] = 42;
        i64s[50] = 99;
        i64s[100] = -7;
        i64s[101] = -7;
        i64s[102] = -7;
        let flat_i64 = FlatColumn::try_new(FlatData::I64(i64s), Some(nulls.clone())).unwrap();
        verify_runs(&flat_i64, &expected_7);

        // Distinguish -0.0 vs +0.0 and treat NaN with identical bit patterns as equal runs.
        let mut f64s = vec![0.0f64; n];
        f64s[10] = -0.0;
        f64s[11] = 0.0; // splits run at 11!
        f64s[50] = f64::NAN;
        f64s[100] = f64::NAN;
        f64s[101] = f64::NAN;
        f64s[102] = f64::NAN;
        let flat_f64 = FlatColumn::try_new(FlatData::F64(f64s), Some(nulls.clone())).unwrap();
        verify_runs(&flat_f64, &[10, 11, 12, 50, 51, 100, 103, 128]);

        // Utf8 with nulls and no nulls.
        let mut strs = vec![""; n];
        strs[10] = "alpha";
        strs[11] = "alpha";
        strs[50] = "beta";
        strs[100] = "gamma";
        strs[101] = "gamma";
        strs[102] = "gamma";
        let flat_utf8 = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(strs).unwrap()),
            Some(nulls),
        )
        .unwrap();
        verify_runs(&flat_utf8, &expected_7);

        // Non-null alternating every row -> bails out cleanly in both sample_64 and compute_flat_runs.
        let alt_i64 = FlatColumn::try_new(
            FlatData::I64((0..n).map(|i| (i % 2) as i64).collect()),
            None,
        )
        .unwrap();
        assert!(!has_runs_sample_64(&alt_i64));
        assert!(compute_flat_runs(&alt_i64).unwrap().is_none());
    }
}
