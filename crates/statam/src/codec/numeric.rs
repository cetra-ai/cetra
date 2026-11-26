//! Narrow scalar column encoders and decoders (`ConstNull`, `Const`, `Rle`,
//! `ForBitpack`, `DeltaBitpack`, `Alp`, `BitpackBool`, `Plain`).

use cetra_column::{
    BinaryData, Bitmap, Column, DataType, FlatColumn, FlatData, Interval, RleColumn, ScalarValue,
    Utf8Data,
};

use crate::codec::alp::{decode_alp_body, encode_alp_body};
use crate::codec::fsst::{decode_fsst_binary, decode_fsst_utf8, encode_fsst_body};
use crate::codec::zonemap::{ZoneMapSummary, compute_zone_map};
use crate::codec::{EncodedMiniBlock, WriteProfile};
use crate::format::{
    BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_CONST_EMPTY_COLLECTION, BLOCK_FLAG_HAS_NULLS, BlockEntry,
    Codec, Encoding,
};
use crate::simd::bitpack::{
    delta_bases_len, delta_decode_i64, delta_encode_i64, min_bits_u32, min_bits_u64, pack_u32,
    pack_u64, packed_bytes_len, unpack_u32, unpack_u64, zigzag_decode_i64, zigzag_encode_i64,
};
use crate::{MAX_BLOCK_RAW_BYTES, StatamError};

/// Appends `nulls` bitmap bytes (`len.div_ceil(8)`) when `nulls` is `Some`.
pub(crate) fn write_optional_nulls(nulls: Option<&Bitmap>, out: &mut Vec<u8>) {
    if let Some(bm) = nulls {
        out.extend_from_slice(&bm.to_bytes());
    }
}

/// Reads the optional leading row null bitmap if `BLOCK_FLAG_HAS_NULLS` is set.
pub(crate) fn read_optional_nulls(
    src: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<(Option<Bitmap>, &[u8]), StatamError> {
    if block_flags & BLOCK_FLAG_HAS_NULLS == 0 {
        return Ok((None, src));
    }
    let byte_len = row_count.div_ceil(8);
    if src.len() < byte_len {
        return Err(StatamError::Corrupt {
            reason: "truncated mini-block null bitmap",
        });
    }
    let bm = Bitmap::from_bytes(&src[..byte_len], row_count)?;
    Ok((Some(bm), &src[byte_len..]))
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
    let from_dict = matches!(col, Column::Dict(_));
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
            if from_dict {
                max_inline = min_inline;
                max_inline_len = min_inline_len;
            }
        }
        ScalarValue::FixedBytes16(b) => {
            min_inline.copy_from_slice(b);
            min_inline_len = 16;
            if from_dict {
                max_inline = min_inline;
                max_inline_len = min_inline_len;
            }
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
            if from_dict {
                max_inline = min_inline;
                max_inline_len = min_inline_len;
            }
        }
        ScalarValue::Binary(b) if b.len() <= 16 => {
            min_inline[..b.len()].copy_from_slice(b);
            min_inline_len = b.len() as u8;
            if from_dict {
                max_inline = min_inline;
                max_inline_len = min_inline_len;
            }
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
            let first = u.get(0)?;
            if first.len() <= 16 && (1..n).all(|i| u.get(i) == Some(first)) {
                Some(ScalarValue::Utf8(first.to_owned()))
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
            owned_zm = compute_zone_map(col, false);
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

    let plain_codec = match profile {
        WriteProfile::L0Fast => Codec::Lz4,
        WriteProfile::L1Compact => Codec::Zstd,
    };

    match dt {
        DataType::Bool => {
            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            if let FlatData::Bool(bm) = flat.data() {
                raw.extend_from_slice(&bm.to_bytes());
            }
            Ok(EncodedMiniBlock {
                encoding: Encoding::BitpackBool,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec: Codec::None,
            })
        }
        DataType::I64 | DataType::TimestampNs { .. } | DataType::Date => {
            let (enc, inner_body) = match profile {
                WriteProfile::L0Fast => {
                    if is_monotonic_sample_32(flat) {
                        if let Some(db) = encode_delta_bitpack_body(flat)? {
                            (Encoding::DeltaBitpack, db)
                        } else {
                            (Encoding::Plain, encode_plain_body(flat)?)
                        }
                    } else if let Some(fb) = encode_for_bitpack_body(flat)? {
                        (Encoding::ForBitpack, fb)
                    } else {
                        (Encoding::Plain, encode_plain_body(flat)?)
                    }
                }
                WriteProfile::L1Compact => {
                    let for_body = encode_for_bitpack_body(flat)?;
                    let delta_body = encode_delta_bitpack_body(flat)?;

                    match (for_body, delta_body) {
                        (Some(fb), Some(db)) if db.len() < fb.len() => (Encoding::DeltaBitpack, db),
                        (Some(fb), _) => (Encoding::ForBitpack, fb),
                        (None, Some(db)) => (Encoding::DeltaBitpack, db),
                        (None, None) => (Encoding::Plain, encode_plain_body(flat)?),
                    }
                }
            };

            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.extend_from_slice(&inner_body);

            let block_codec = if enc == Encoding::Plain {
                plain_codec
            } else {
                Codec::None
            };
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
            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.extend_from_slice(&inner_body);
            let block_codec = if enc == Encoding::Plain {
                plain_codec
            } else {
                Codec::None
            };
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
            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.extend_from_slice(&inner_body);
            let block_codec = if enc == Encoding::Plain {
                plain_codec
            } else {
                Codec::None
            };
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
            let (enc, inner_body) = if profile == WriteProfile::L1Compact
                && !is_wide_row
                && raw_varlen_bytes <= 16 * 1024
            {
                let slices = extract_varlen_slices(flat)?;
                if let Some(fsst) = encode_fsst_body(&slices)? {
                    (Encoding::Fsst, fsst)
                } else {
                    (Encoding::Plain, encode_plain_body(flat)?)
                }
            } else {
                (Encoding::Plain, encode_plain_body(flat)?)
            };
            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.extend_from_slice(&inner_body);
            let block_codec = if enc == Encoding::Plain {
                plain_codec
            } else {
                Codec::None
            };
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
        DataType::Interval | DataType::FixedBytes8 | DataType::FixedBytes16 | DataType::Null => {
            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.extend_from_slice(&encode_plain_body(flat)?);
            Ok(EncodedMiniBlock {
                encoding: Encoding::Plain,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: zm.min_inline_len,
                max_inline_len: zm.max_inline_len,
                min_inline: zm.min_inline,
                max_inline: zm.max_inline,
                raw_body: raw,
                preferred_codec: plain_codec,
            })
        }
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links => {
            Err(StatamError::SchemaMismatch {
                reason: "nested DataType passed to narrow encoder",
            })
        }
    }
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
                    out.push(u.get(i).map_or(&[][..], str::as_bytes));
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
            let single = col.slice(0, 1)?.materialize(None)?;
            (single, vec![*len as u32])
        }
        Column::Flat(f) => {
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

    Ok(Some(EncodedMiniBlock {
        encoding: Encoding::Rle,
        block_flags,
        null_count: zm.null_count,
        min_inline_len: zm.min_inline_len,
        max_inline_len: zm.max_inline_len,
        min_inline: zm.min_inline,
        max_inline: zm.max_inline,
        raw_body: raw,
        preferred_codec: Codec::None,
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

fn compute_flat_runs(flat: &FlatColumn) -> Result<Option<(FlatColumn, Vec<u32>)>, StatamError> {
    let n = flat.len();
    if n < 2 {
        return Ok(None);
    }

    let mut run_ends = Vec::new();
    let mut rep_indices = Vec::new();
    let mut run_start = 0usize;

    // Early bail-out if runs exceed n / 4 after scanning.
    let max_runs = n / 4;
    if max_runs < 1 {
        return Ok(None);
    }

    for i in 1..n {
        if !flat_rows_equal(flat, run_start, i) {
            rep_indices.push(run_start as u32);
            run_ends.push(i as u32);
            if run_ends.len() >= max_runs {
                return Ok(None);
            }
            run_start = i;
        }
    }
    rep_indices.push(run_start as u32);
    run_ends.push(n as u32);

    let run_values = flat.take(&rep_indices, None)?;
    Ok(Some((run_values, run_ends)))
}

fn flat_rows_equal(flat: &FlatColumn, a: usize, b: usize) -> bool {
    let na = flat.is_null(a);
    let nb = flat.is_null(b);
    if na != nb {
        return false;
    }
    if na {
        return true;
    }
    match flat.data() {
        FlatData::Bool(bm) => bm.get(a) == bm.get(b),
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => v[a] == v[b],
        FlatData::F64(v) => v[a].to_bits() == v[b].to_bits(),
        FlatData::Decimal128 { values, .. } => values[a] == values[b],
        FlatData::Date(v) => v[a] == v[b],
        FlatData::Interval(v) => v[a] == v[b],
        FlatData::FixedBytes8(v) => v[a] == v[b],
        FlatData::FixedBytes16(v) => v[a] == v[b],
        FlatData::Utf8(u) => u.get(a) == u.get(b),
        FlatData::Binary(bd) => bd.get(a) == bd.get(b),
        _ => false,
    }
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
    let flat = decode_inner_flat(body, entry.encoding, row_count, dt, nulls)?;
    Ok(Column::Flat(flat))
}

pub(crate) fn decode_rle_block(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    dt: &DataType,
) -> Result<Column, StatamError> {
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
    let run_values = decode_inner_flat(inner_body, inner_enc, num_runs, dt, run_nulls)?;
    let rle = RleColumn::try_new(run_values, run_ends)?;
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

fn decode_for_i64_body(
    body: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<i64>, StatamError> {
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
    let needed = packed_bytes_len(row_count, bw)?;
    if body.len() != 9 + needed {
        return Err(StatamError::Corrupt {
            reason: "ForBitpack i64 length mismatch",
        });
    }
    let mut deltas = Vec::with_capacity(row_count);
    unpack_u64(&body[9..], row_count, bw, &mut deltas)?;
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
    check_mul(row_count, 4, "ForBitpack Date exceeds MAX_BLOCK_RAW_BYTES")?;
    if body.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated ForBitpack Date header",
        });
    }
    let base = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let bw = body[4];
    let needed = packed_bytes_len(row_count, bw)?;
    if body.len() != 5 + needed {
        return Err(StatamError::Corrupt {
            reason: "ForBitpack Date length mismatch",
        });
    }
    let mut deltas = Vec::with_capacity(row_count);
    unpack_u32(&body[5..], row_count, bw, &mut deltas)?;
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
    let needed = packed_bytes_len(row_count, bw)?;
    if body.len() != 17 + needed {
        return Err(StatamError::Corrupt {
            reason: "ForBitpack Decimal128 length mismatch",
        });
    }
    let mut deltas = Vec::with_capacity(row_count);
    unpack_u64(&body[17..], row_count, bw, &mut deltas)?;
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

    let mut bases = Vec::with_capacity(num_bases);
    for &chunk in body[2..2 + bases_bytes].as_chunks::<8>().0 {
        bases.push(i64::from_le_bytes(chunk));
    }
    let mut packed_deltas = Vec::with_capacity(row_count);
    unpack_u64(&body[2 + bases_bytes..], row_count, bw, &mut packed_deltas)?;
    let mut deltas = Vec::with_capacity(row_count);
    if is_zigzag {
        for d in packed_deltas {
            deltas.push(zigzag_decode_i64(d));
        }
    } else {
        for d in packed_deltas {
            deltas.push(d as i64);
        }
    }

    let mut out = Vec::with_capacity(row_count);
    delta_decode_i64(&bases, &deltas, row_count, &mut out)?;
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
}
