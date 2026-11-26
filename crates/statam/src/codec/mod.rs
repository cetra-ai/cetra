//! Columnar mini-block codecs (`L0Fast` & `L1Compact`) for all 16 `cetra-column` types
//! plus sound mini-block zone maps.

pub mod alp;
pub mod dict;
pub mod fsst;
pub mod nested;
pub mod numeric;
pub mod zonemap;

use std::fmt;

use cetra_column::{Bitmap, Column, DataType, FlatColumn};

use crate::StatamError;
use crate::codec::dict::{FileDict, decode_dict_column, encode_dict_codes_body};
use crate::codec::nested::{decode_nested_column, encode_nested_column};
use crate::codec::numeric::{
    decode_const_block, decode_narrow_column, decode_narrow_column_selected,
    encode_narrow_column_with_locality, read_optional_nulls, try_encode_const_or_null,
    write_optional_nulls,
};
use crate::codec::zonemap::compute_zone_map;
use crate::format::{
    BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_HAS_NULLS, BLOCK_FLAG_MIN_MAX_VALID, BlockEntry, Codec,
    Encoding, MAX_NESTING_DEPTH, write_framed_block,
};

pub use alp::{ALP_MAX_EXP, decode_alp_body, encode_alp_body};
pub use dict::{
    DICT_HEADER_LEN, DICT_PASSTHROUGH_MAX_BYTES, DictBuildMode, DictBuilder, DictValueKind,
    FileDict as CodecFileDict, L0_DICT_MAX_ENTRIES, decode_dict_codes_body,
    encode_dict_codes_body as encode_dict_codes,
};
pub use fsst::{
    FSST_ESCAPE, FSST_MAX_SYM_LEN, FSST_MAX_SYMBOLS, FsstBlockView, FsstTable, decode_fsst_binary,
    decode_fsst_utf8, encode_fsst_body,
};
pub use nested::{
    ShreddedMapBlocks, ShreddedSubCol, decode_shredded_residual_block, decode_shredded_subcol,
    decode_single_map_key, encode_shredded_residual_block, encode_shredded_subcol,
    select_shredded_keys, shred_map, unshred_map,
};
pub use zonemap::{
    ZoneMapSummary, ZonePred, ZoneValue, compute_zone_map as block_zone_map, zone_map_may_match,
};

/// Encoding and compression profile controlling speed vs. density trade-offs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteProfile {
    /// Fast self-contained single-pass profile (`Const`, `Rle`, `ForBitpack`/`DeltaBitpack` via
    /// 32-element monotonicity sample, `BitpackBool`, bounded `Dict <= 4096`, `Plain + Zstd(-3)/Lz4`).
    L0Fast,
    /// Full compression cascade (`+ sorted Dict`, `Alp`, `Fsst`, `ShreddedMap`, `Zstd(3)`).
    L1Compact,
}

#[cfg(not(miri))]
#[inline]
pub(crate) fn zstd_or_lz4_codec() -> Codec {
    Codec::Zstd
}

#[cfg(miri)]
#[inline]
pub(crate) fn zstd_or_lz4_codec() -> Codec {
    Codec::Lz4
}

#[inline]
pub(crate) fn preferred_block_codec(
    profile: WriteProfile,
    raw_len: usize,
    compress_in_l0: bool,
) -> Codec {
    match profile {
        WriteProfile::L1Compact => zstd_or_lz4_codec(),
        WriteProfile::L0Fast => {
            if !compress_in_l0 || raw_len < 64 {
                Codec::None
            } else {
                Codec::Lz4
            }
        }
    }
}

/// Encoded mini-block payload and metadata prior to framing and outer compression.
#[derive(Clone)]
pub struct EncodedMiniBlock {
    /// Chosen column encoding (`0..=13`).
    pub encoding: Encoding,
    /// `BLOCK_FLAG_*` bitmask.
    pub block_flags: u8,
    /// Number of null rows (`<= row_count`).
    pub null_count: u16,
    /// Active length of `min_inline` (`0..=16`).
    pub min_inline_len: u8,
    /// Active length of `max_inline` (`0..=16`).
    pub max_inline_len: u8,
    /// Inline lower bound (or constant value for `Encoding::Const`).
    pub min_inline: [u8; 16],
    /// Inline upper bound.
    pub max_inline: [u8; 16],
    /// Uncompressed encoded block body (empty for `ConstNull`/`Const`).
    pub raw_body: Vec<u8>,
    /// Preferred outer compression codec (`Codec::None` for inline blocks).
    pub preferred_codec: Codec,
}

impl fmt::Debug for EncodedMiniBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedMiniBlock")
            .field("encoding", &self.encoding)
            .field("block_flags", &self.block_flags)
            .field("null_count", &self.null_count)
            .field("min_inline_len", &self.min_inline_len)
            .field("max_inline_len", &self.max_inline_len)
            .field("raw_body_len", &self.raw_body.len())
            .field("preferred_codec", &self.preferred_codec)
            .finish()
    }
}

impl EncodedMiniBlock {
    /// Frames this mini-block into `out` (unless inline `ConstNull`/`Const`) and
    /// returns the completed [`BlockEntry`].
    pub fn write_framed_entry(
        &self,
        first_row: u32,
        row_count: u16,
        base_offset: u64,
        zstd_level: i32,
        out: &mut Vec<u8>,
    ) -> Result<BlockEntry, StatamError> {
        if self.encoding == Encoding::ConstNull || self.encoding == Encoding::Const {
            return Ok(BlockEntry {
                first_row,
                row_count,
                null_count: self.null_count,
                offset: 0,
                stored_len: 0,
                raw_len: 0,
                encoding: self.encoding,
                codec: Codec::None,
                block_flags: self.block_flags,
                min_inline_len: self.min_inline_len,
                max_inline_len: self.max_inline_len,
                reserved: [0; 3],
                min_inline: self.min_inline,
                max_inline: self.max_inline,
            });
        }

        let offset = base_offset + out.len() as u64;
        let info = write_framed_block(
            self.encoding,
            &self.raw_body,
            self.preferred_codec,
            zstd_level,
            out,
        )?;
        Ok(BlockEntry {
            first_row,
            row_count,
            null_count: self.null_count,
            offset,
            stored_len: info.stored_len,
            raw_len: info.raw_len,
            encoding: self.encoding,
            codec: info.codec,
            block_flags: self.block_flags,
            min_inline_len: self.min_inline_len,
            max_inline_len: self.max_inline_len,
            reserved: [0; 3],
            min_inline: self.min_inline,
            max_inline: self.max_inline,
        })
    }
}

/// Encodes one mini-block (`col.len() <= u16::MAX`) for any of the 16 `cetra-column` types.
///
/// If `file_dict` is `Some`, attempts dictionary encoding using `file_dict` first
/// (falling back cleanly to FSST/Plain if any value in `col` is absent from `file_dict`).
pub fn encode_mini_block(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    file_dict: Option<&FileDict>,
) -> Result<EncodedMiniBlock, StatamError> {
    encode_mini_block_with_locality(col, dt, profile, file_dict, false)
}

pub(crate) fn encode_mini_block_with_locality(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    file_dict: Option<&FileDict>,
    is_wide_row: bool,
) -> Result<EncodedMiniBlock, StatamError> {
    dt.validate(MAX_NESTING_DEPTH)?;
    if col.data_type() != *dt {
        return Err(StatamError::SchemaMismatch {
            reason: "column data_type does not match target DataType",
        });
    }
    if col.len() > u16::MAX as usize {
        return Err(StatamError::BoundsExceeded {
            reason: "mini-block row count exceeds u16::MAX",
        });
    }

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

    let zm = compute_zone_map(col, file_dict.is_some_and(FileDict::is_sorted));
    if let Some(inline) = try_encode_const_or_null(col, dt, &zm) {
        return Ok(inline);
    }

    if let Some(dict) = file_dict
        && let Some(dict_block) = try_encode_with_file_dict(col, dict, profile, &zm)?
    {
        return Ok(dict_block);
    }

    if matches!(
        dt,
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links
    ) {
        encode_nested_column(col, dt, profile, MAX_NESTING_DEPTH, Some(&zm))
    } else {
        encode_narrow_column_with_locality(col, dt, profile, Some(&zm), is_wide_row)
    }
}

fn try_encode_with_file_dict(
    col: &Column,
    dict: &FileDict,
    profile: WriteProfile,
    zm: &ZoneMapSummary,
) -> Result<Option<EncodedMiniBlock>, StatamError> {
    if col.data_type() != dict.values().data_type() {
        return Ok(None);
    }

    let owned_flat;
    let (codes, nulls_opt) = match col {
        Column::Dict(d)
            if d.values() == dict.values() && d.nulls().is_none()
                || matches!(col, Column::Dict(d) if d.values() == dict.values()) =>
        {
            let Column::Dict(d) = col else { unreachable!() };
            (d.codes().to_vec(), d.nulls().cloned())
        }
        _ => {
            let flat = match col {
                Column::Flat(f) => f,
                other => {
                    owned_flat = other.materialize(None)?;
                    &owned_flat
                }
            };
            let Some(mapped) = dict.try_map_column(flat)? else {
                return Ok(None);
            };
            (mapped, flat.nulls().cloned())
        }
    };

    Ok(Some(encode_dict_codes_precomputed(
        &codes,
        nulls_opt.as_ref(),
        profile,
        zm.block_flags & !BLOCK_FLAG_MIN_MAX_VALID,
    )?))
}

pub(crate) fn encode_dict_codes_precomputed(
    codes: &[u32],
    nulls_opt: Option<&Bitmap>,
    profile: WriteProfile,
    base_block_flags: u8,
) -> Result<EncodedMiniBlock, StatamError> {
    let n = codes.len();
    let null_count_usize = nulls_opt.map_or(0, |bm| n.saturating_sub(bm.count_ones()));
    let null_count = u16::try_from(null_count_usize).map_err(|_| StatamError::BoundsExceeded {
        reason: "mini-block null_count overflows u16",
    })?;
    if null_count_usize == n {
        return Ok(EncodedMiniBlock {
            encoding: Encoding::ConstNull,
            block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
            null_count,
            min_inline_len: 0,
            max_inline_len: 0,
            min_inline: [0u8; 16],
            max_inline: [0u8; 16],
            raw_body: Vec::new(),
            preferred_codec: Codec::None,
        });
    }

    let mut min_code = u32::MAX;
    let mut max_code = 0u32;
    let mut any_valid = false;
    for (i, &c) in codes.iter().enumerate() {
        if nulls_opt.is_none_or(|bm| bm.get(i)) {
            any_valid = true;
            min_code = min_code.min(c);
            max_code = max_code.max(c);
        }
    }

    let mut raw = Vec::new();
    write_optional_nulls(nulls_opt, &mut raw);
    raw.extend_from_slice(&encode_dict_codes_body(codes, nulls_opt)?);

    let mut min_inline = [0u8; 16];
    let mut max_inline = [0u8; 16];
    let mut block_flags = base_block_flags & !BLOCK_FLAG_MIN_MAX_VALID;
    if null_count > 0 {
        block_flags |= BLOCK_FLAG_HAS_NULLS;
    }
    let (min_len, max_len) = if any_valid {
        block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
        min_inline[0..4].copy_from_slice(&min_code.to_le_bytes());
        max_inline[0..4].copy_from_slice(&max_code.to_le_bytes());
        (4u8, 4u8)
    } else {
        (0u8, 0u8)
    };

    let preferred_codec = preferred_block_codec(profile, raw.len(), false);
    Ok(EncodedMiniBlock {
        encoding: Encoding::Dict,
        block_flags,
        null_count,
        min_inline_len: min_len,
        max_inline_len: max_len,
        min_inline,
        max_inline,
        raw_body: raw,
        preferred_codec,
    })
}

/// Decodes one mini-block from its decompressed `raw_body` and [`BlockEntry`].
///
/// For `Dict` blocks, emits `Column::Dict` directly when `dict.memory_bytes() <= 256 KiB`,
/// or materializes to `Column::Flat` otherwise.
pub fn decode_mini_block(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    file_dict: Option<&FileDict>,
) -> Result<Column, StatamError> {
    dt.validate(MAX_NESTING_DEPTH)?;

    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        return decode_const_block(entry, dt);
    }

    let row_count = entry.row_count as usize;
    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty mini-block",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }

    if entry.encoding == Encoding::Dict {
        let Some(dict) = file_dict else {
            return Err(StatamError::Corrupt {
                reason: "Dict mini-block requires FileDict",
            });
        };
        if dict.values().data_type() != *dt {
            return Err(StatamError::SchemaMismatch {
                reason: "FileDict DataType mismatch",
            });
        }
        let (nulls, after_nulls) = read_optional_nulls(raw_body, row_count, entry.block_flags)?;
        return decode_dict_column(after_nulls, row_count, nulls, dict);
    }

    if matches!(
        entry.encoding,
        Encoding::ListEncoded
            | Encoding::ShreddedMap
            | Encoding::EventsStruct
            | Encoding::LinksStruct
    ) {
        return decode_nested_column(raw_body, entry, dt, MAX_NESTING_DEPTH);
    }

    decode_narrow_column(raw_body, entry, dt)
}

/// Decodes one mini-block, only materializing `mask`-selected rows when the encoding
/// supports sparse decode (such as `Fsst`).
pub fn decode_mini_block_selected(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    file_dict: Option<&FileDict>,
    mask: &Bitmap,
) -> Result<Column, StatamError> {
    if entry.encoding == Encoding::Fsst && !mask.all_ones() {
        dt.validate(MAX_NESTING_DEPTH)?;
        return decode_narrow_column_selected(raw_body, entry, dt, mask);
    }
    decode_mini_block(raw_body, entry, dt, file_dict)
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        BinaryData, Bitmap, Column, DataType, DictColumn, EventsData, FlatColumn, FlatData,
        Interval, LinksData, MapData, RleColumn, ScalarValue, Utf8Data,
    };

    use super::*;
    use crate::codec::dict::{DICT_PASSTHROUGH_MAX_BYTES, DictBuilder, FileDict};
    use crate::codec::fsst::{FsstBlockView, encode_fsst_body};
    use crate::codec::nested::{
        decode_shredded_residual_block, decode_shredded_subcol, decode_single_map_key,
        encode_shredded_residual_block, encode_shredded_subcol, select_shredded_keys, shred_map,
        unshred_map,
    };
    use crate::codec::zonemap::{ZonePred, ZoneValue, zone_map_may_match};
    use crate::format::{
        BLOCK_FLAG_HAS_NAN, BLOCK_FLAG_MIN_MAX_VALID, COL_FLAG_DICT_SORTED, MAX_BLOCK_RAW_BYTES,
        MAX_SHREDDED_KEYS, read_framed_block,
    };

    fn roundtrip_via_frame(
        col: &Column,
        dt: &DataType,
        profile: WriteProfile,
        dict: Option<&FileDict>,
    ) -> (EncodedMiniBlock, BlockEntry, Column) {
        let enc = encode_mini_block(col, dt, profile, dict).unwrap();
        let mut framed = Vec::new();
        let entry = enc
            .write_framed_entry(0, col.len() as u16, 0, 3, &mut framed)
            .unwrap();
        let raw_body = if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const
        {
            assert!(framed.is_empty());
            Vec::new()
        } else {
            let (dec_enc, _, body) = read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap();
            assert_eq!(dec_enc, entry.encoding);
            body
        };
        let decoded = decode_mini_block(&raw_body, &entry, dt, dict).unwrap();
        (enc, entry, decoded)
    }

    #[test]
    fn alp_bit_exact_roundtrip_costs_neg_zero_nan_payloads_inf_subnormals() {
        let mut vals = Vec::new();
        // Realistic LLM decimal costs (exact under decimal scaling).
        for i in 0..512 {
            vals.push(((i % 97) as f64) * 0.000_025);
        }
        // Inject every adversarial IEEE-754 edge pattern.
        vals[0] = -0.0;
        vals[1] = 0.0;
        vals[2] = f64::from_bits(0x7ff8_0000_0000_0001); // quiet NaN payload 1
        vals[3] = f64::from_bits(0x7ff0_0000_0000_0042); // signaling NaN payload 0x42
        vals[4] = f64::from_bits(0xfff8_1234_5678_9abc); // negative NaN payload
        vals[5] = f64::INFINITY;
        vals[6] = f64::NEG_INFINITY;
        vals[7] = f64::MIN_POSITIVE;
        vals[8] = 1e-310; // subnormal
        vals[9] = f64::from_bits(1); // smallest positive subnormal
        vals[10] = f64::from_bits(0x8000_0000_0000_0001); // smallest negative subnormal
        vals[11] = std::f64::consts::PI;

        let encoded = encode_alp_body(&vals, None)
            .unwrap()
            .expect("costs with <25% exceptions must compress with ALP");
        assert!(encoded.len() < vals.len() * 8 / 2);

        let decoded = decode_alp_body(&encoded, vals.len(), None).unwrap();
        assert_eq!(decoded.len(), vals.len());
        for (i, (&orig, &dec)) in vals.iter().zip(&decoded).enumerate() {
            assert_eq!(
                dec.to_bits(),
                orig.to_bits(),
                "bit-exact mismatch at index {i}"
            );
        }

        // Verify `(0..=100) / 100.0` (exact IEEE-754 division mode) produces zero exceptions.
        let dec_div: Vec<f64> = (0..1024).map(|i| ((i % 101) as f64) / 100.0).collect();
        let enc_div = encode_alp_body(&dec_div, None)
            .unwrap()
            .expect("decimal fractions / 100.0 must compress with ALP");
        assert_eq!(u16::from_le_bytes([enc_div[2], enc_div[3]]), 0);
        let dec_out = decode_alp_body(&enc_div, dec_div.len(), None).unwrap();
        for (i, (&orig, &dec)) in dec_div.iter().zip(&dec_out).enumerate() {
            assert_eq!(dec.to_bits(), orig.to_bits(), "div_mode mismatch at {i}");
        }

        // Verify full mini-block cascade selects Plain for L0Fast and ALP for L1Compact.
        let col = Column::Flat(FlatColumn::try_new(FlatData::F64(vals.clone()), None).unwrap());
        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let (enc, _, dec_col) = roundtrip_via_frame(&col, &DataType::F64, profile, None);
            let expected_enc = match profile {
                WriteProfile::L0Fast => Encoding::Plain,
                WriteProfile::L1Compact => Encoding::Alp,
            };
            assert_eq!(enc.encoding, expected_enc);
            let Column::Flat(dec_flat) = dec_col else {
                panic!("expected Flat F64");
            };
            let FlatData::F64(dec_vals) = dec_flat.data() else {
                panic!("expected F64");
            };
            for (i, (&o, &d)) in vals.iter().zip(dec_vals).enumerate() {
                assert_eq!(d.to_bits(), o.to_bits(), "cascade mismatch at {i}");
            }
        }

        // High-entropy incompressible floats (> 25% exceptions) fall back to Plain.
        let mut high_entropy = Vec::with_capacity(256);
        let mut seed = 0x1234_5678_9abc_def0u64;
        for _ in 0..256 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            // Force normalized high-precision mantissas.
            let bits = (0x3ff0_0000_0000_0000u64) | (seed >> 12);
            high_entropy.push(f64::from_bits(bits));
        }
        assert!(encode_alp_body(&high_entropy, None).unwrap().is_none());
        let he_col =
            Column::Flat(FlatColumn::try_new(FlatData::F64(high_entropy.clone()), None).unwrap());
        let (enc, _, dec_he) =
            roundtrip_via_frame(&he_col, &DataType::F64, WriteProfile::L1Compact, None);
        assert_eq!(enc.encoding, Encoding::Plain);
        assert!(dec_he.semantic_eq(&he_col));
    }

    #[test]
    fn fsst_roundtrip_and_incompressible_fallback() {
        let mut strings = Vec::new();
        for i in 0..256 {
            strings.push(format!(
                "{{\"gen_ai.system\":\"openai\",\"model\":\"gpt-4o-mini\",\"request_id\":\"req_{i:04}\",\"status\":\"ok\"}}"
            ));
        }
        let slices: Vec<&[u8]> = strings.iter().map(String::as_bytes).collect();
        let encoded = encode_fsst_body(&slices)
            .unwrap()
            .expect("repetitive JSON strings must compress with FSST");
        let raw_total: usize = slices.iter().map(|s| s.len()).sum();
        assert!(encoded.len() < raw_total);

        let utf8 = decode_fsst_utf8(&encoded, strings.len()).unwrap();
        for (i, expected) in strings.iter().enumerate() {
            assert_eq!(utf8.get(i), Some(expected.as_str()));
        }

        let bin = decode_fsst_binary(&encoded, strings.len()).unwrap();
        for (i, expected) in strings.iter().enumerate() {
            assert_eq!(bin.get(i), Some(expected.as_bytes()));
        }

        // Verify O(1) single-row decode_row matches every row.
        let view = FsstBlockView::parse(&encoded, strings.len()).unwrap();
        let mut row_buf = Vec::new();
        for (i, expected) in strings.iter().enumerate() {
            row_buf.clear();
            view.decode_row(i, &mut row_buf).unwrap();
            assert_eq!(row_buf.as_slice(), expected.as_bytes());
        }

        // Incompressible pseudo-random strings (< 1.15x) must fall back to Plain.
        let mut random_strs = Vec::new();
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..128 {
            let mut item = String::with_capacity(48);
            for _ in 0..48 {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let ch = 33u8 + ((s >> 56) % 94) as u8;
                item.push(ch as char);
            }
            random_strs.push(item);
        }
        let rand_slices: Vec<&[u8]> = random_strs.iter().map(String::as_bytes).collect();
        assert!(encode_fsst_body(&rand_slices).unwrap().is_none());

        let u_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(random_strs.iter().map(String::as_str)).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let (enc, _, dec) =
            roundtrip_via_frame(&u_col, &DataType::Utf8, WriteProfile::L1Compact, None);
        assert_eq!(enc.encoding, Encoding::Plain);
        assert!(dec.semantic_eq(&u_col));

        // Corrupt escape at end of row must be rejected.
        let mut corrupt = vec![1u8, 1u8, b'a', 0u8]; // 1 sym 'a', bit_width=0 -> wait, use 1 row len 1
        corrupt.clear();
        corrupt.push(0); // sym_count = 0
        corrupt.push(1); // len_bw = 1
        corrupt.push(1); // row_lens[0] = 1
        corrupt.push(0xFF); // truncated escape!
        assert!(decode_fsst_binary(&corrupt, 1).is_err());
    }

    #[test]
    fn fsst_decode_selected_matches_full_decode_on_active_rows() {
        let mut vals: Vec<Option<String>> = Vec::with_capacity(200);
        for i in 0..200 {
            if i % 7 == 0 {
                vals.push(None);
            } else {
                vals.push(Some(format!(
                    "{{\"role\":\"assistant\",\"step\":{i},\"model\":\"claude-3-5-sonnet\",\"id\":\"018f6b4e-{i:08x}\"}}"
                )));
            }
        }
        let mut nulls = Bitmap::new_ones(vals.len());
        let mut raw_strs = Vec::with_capacity(vals.len());
        for (i, opt) in vals.iter().enumerate() {
            match opt {
                Some(s) => raw_strs.push(s.as_str()),
                None => {
                    let _ = nulls.set(i, false);
                    raw_strs.push("");
                }
            }
        }
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(raw_strs).unwrap()),
                Some(nulls),
            )
            .unwrap(),
        );
        let enc = encode_mini_block(&col, &DataType::Utf8, WriteProfile::L1Compact, None).unwrap();
        assert_eq!(enc.encoding, Encoding::Fsst);
        let mut framed = Vec::new();
        let entry = enc
            .write_framed_entry(0, vals.len() as u16, 0, 3, &mut framed)
            .unwrap();
        let (_, _, raw_body) = read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap();

        let mut mask = Bitmap::new_zeroed(vals.len());
        for idx in [1usize, 7, 15, 64, 140, 199] {
            let _ = mask.set(idx, true);
        }
        let sel_col = decode_mini_block_selected(&raw_body, &entry, &DataType::Utf8, None, &mask)
            .unwrap()
            .materialize(None)
            .unwrap();
        let full_col = decode_mini_block(&raw_body, &entry, &DataType::Utf8, None)
            .unwrap()
            .materialize(None)
            .unwrap();
        let FlatData::Utf8(sel_u) = sel_col.data() else {
            panic!("expected Utf8");
        };
        let FlatData::Utf8(full_u) = full_col.data() else {
            panic!("expected Utf8");
        };
        for r in 0..vals.len() {
            let sel_valid = sel_col.nulls().is_none_or(|b| b.get(r));
            let full_valid = full_col.nulls().is_none_or(|b| b.get(r));
            if mask.get(r) {
                assert_eq!(sel_valid, full_valid);
                if sel_valid {
                    assert_eq!(sel_u.get(r), full_u.get(r));
                }
            } else {
                assert!(!sel_valid);
            }
        }
    }

    fn build_all_16_types_columns() -> Vec<(DataType, Column)> {
        let nulls_4 = Bitmap::from_bools(&[true, false, true, true]);
        vec![
            (
                DataType::Null,
                Column::Flat(FlatColumn::try_new(FlatData::Null(4), None).unwrap()),
            ),
            (
                DataType::Bool,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Bool(Bitmap::from_bools(&[true, false, false, true])),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::I64,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::I64(vec![i64::MIN, 0, 42, i64::MAX]),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::F64,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![-0.0, 0.0, f64::NAN, 12.5]),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::Decimal128 {
                    precision: 38,
                    scale: 18,
                },
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![1_000_000_000_000, 0, -500_000, 999_999_999_999],
                            precision: 38,
                            scale: 18,
                        },
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::TimestampNs { utc: true },
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![
                                1_700_000_000_000_000_000,
                                0,
                                1_700_000_001_000_000_000,
                                1_700_000_002_000_000_000,
                            ],
                            utc: true,
                        },
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::Date,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Date(vec![-100, 0, 19_500, 20_000]),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::Interval,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Interval(vec![
                            Interval {
                                months: 1,
                                days: -2,
                                nanos: 300,
                            },
                            Interval::default(),
                            Interval {
                                months: -12,
                                days: 30,
                                nanos: -999,
                            },
                            Interval {
                                months: 0,
                                days: 1,
                                nanos: 86_400_000_000_000,
                            },
                        ]),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::Utf8,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([
                                "hello 🌍",
                                "",
                                "openai/gpt-4o",
                                "x".repeat(32).as_str(),
                            ])
                            .unwrap(),
                        ),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::Binary,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Binary(
                            BinaryData::try_from_slices([
                                b"\x00\xff\x01".as_slice(),
                                b"".as_slice(),
                                b"binary-payload-repeat-binary-payload-repeat".as_slice(),
                                b"\xde\xad\xbe\xef".as_slice(),
                            ])
                            .unwrap(),
                        ),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::FixedBytes8,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes8(vec![[1; 8], [0; 8], [2; 8], [3; 8]]),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::FixedBytes16,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16(vec![[10; 16], [0; 16], [20; 16], [30; 16]]),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::List(Box::new(DataType::Utf8)),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::List {
                            item_type: DataType::Utf8,
                            offsets: vec![0, 3, 3, 5, 7],
                            values: Box::new(Column::Flat(
                                FlatColumn::try_new(
                                    FlatData::Utf8(
                                        Utf8Data::try_from_slices([
                                            "tag_a", "tag_b", "tag_a", "tag_b", "tag_a", "tag_b",
                                            "tag_a",
                                        ])
                                        .unwrap(),
                                    ),
                                    None,
                                )
                                .unwrap(),
                            )),
                        },
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::MapUtf8,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::MapUtf8(Box::new(
                            MapData::try_new(
                                vec![0, 2, 2, 3, 4],
                                Utf8Data::try_from_slices(["a", "b", "a", "c"]).unwrap(),
                                Utf8Data::try_from_slices(["1", "", "2", "3"]).unwrap(),
                                Some(Bitmap::from_bools(&[true, false, true, true])),
                            )
                            .unwrap(),
                        )),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::Events,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Events(Box::new(
                            EventsData::try_new(
                                vec![0, 2, 2, 3, 3],
                                vec![100, 250, 500],
                                Utf8Data::try_from_slices([
                                    "tool_call",
                                    "tool_result",
                                    "tool_call",
                                ])
                                .unwrap(),
                                Utf8Data::try_from_slices(["{\"k\":1}", "", "{\"k\":2}"]).unwrap(),
                                Some(Bitmap::from_bools(&[true, false, true])),
                            )
                            .unwrap(),
                        )),
                        Some(nulls_4.clone()),
                    )
                    .unwrap(),
                ),
            ),
            (
                DataType::Links,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Links(Box::new(
                            LinksData::try_new(
                                vec![0, 2, 2, 3, 3],
                                vec![[1; 16], [2; 16], [3; 16]],
                                vec![[4; 8], [5; 8], [6; 8]],
                                Utf8Data::try_from_slices(["{\"l\":1}", "", "{\"l\":2}"]).unwrap(),
                                Some(Bitmap::from_bools(&[true, false, true])),
                            )
                            .unwrap(),
                        )),
                        Some(nulls_4),
                    )
                    .unwrap(),
                ),
            ),
        ]
    }

    #[test]
    fn all_16_data_types_roundtrip_l0_and_l1_with_nulls_empty_and_edge_cases() {
        let cols = build_all_16_types_columns();
        assert_eq!(cols.len(), 16);

        for (dt, col) in &cols {
            for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
                let (_, _, decoded) = roundtrip_via_frame(col, dt, profile, None);
                assert!(
                    decoded.semantic_eq(col),
                    "roundtrip mismatch for {dt:?} under {profile:?}"
                );

                // Empty 0-row column roundtrip.
                let empty_col = Column::Flat(FlatColumn::empty(dt));
                let (_, _, dec_empty) = roundtrip_via_frame(&empty_col, dt, profile, None);
                assert!(dec_empty.semantic_eq(&empty_col));

                // All-null ConstNull roundtrip.
                let all_null = Column::Const {
                    value: ScalarValue::Null(dt.clone()),
                    len: 17,
                };
                let (enc_null, _, dec_null) = roundtrip_via_frame(&all_null, dt, profile, None);
                assert_eq!(enc_null.encoding, Encoding::ConstNull);
                assert!(dec_null.semantic_eq(&all_null));
            }
        }

        // Empty collection Const roundtrip (`List`, `MapUtf8`, `Events`, `Links`).
        for (dt, empty_scalar) in [
            (
                DataType::List(Box::new(DataType::Utf8)),
                ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: vec![],
                },
            ),
            (DataType::MapUtf8, ScalarValue::MapUtf8(vec![])),
            (DataType::Events, ScalarValue::Events(vec![])),
            (DataType::Links, ScalarValue::Links(vec![])),
        ] {
            let c = Column::Const {
                value: empty_scalar,
                len: 128,
            };
            let (enc, _, dec) = roundtrip_via_frame(&c, &dt, WriteProfile::L1Compact, None);
            assert_eq!(enc.encoding, Encoding::Const);
            assert!(dec.semantic_eq(&c));
        }

        // Nested List depth == MAX_NESTING_DEPTH (8) succeeds; depth == 9 rejected.
        let mut dt_8 = DataType::I64;
        let mut col_8 = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![7, 8]), None).unwrap());
        for _ in 0..8 {
            col_8 = Column::Flat(
                FlatColumn::try_new(
                    FlatData::List {
                        item_type: dt_8.clone(),
                        offsets: vec![0, 1, 2],
                        values: Box::new(col_8),
                    },
                    None,
                )
                .unwrap(),
            );
            dt_8 = DataType::List(Box::new(dt_8));
        }
        let (_, _, dec_8) = roundtrip_via_frame(&col_8, &dt_8, WriteProfile::L1Compact, None);
        assert!(dec_8.semantic_eq(&col_8));

        let dt_9 = DataType::List(Box::new(dt_8));
        assert!(dt_9.validate(MAX_NESTING_DEPTH).is_err());

        // Huge Decimal128 span > u64::MAX falls back to Plain cleanly.
        let dec_huge = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![i128::MIN, 0, i128::MAX],
                    precision: 38,
                    scale: 0,
                },
                None,
            )
            .unwrap(),
        );
        let dec_dt = DataType::Decimal128 {
            precision: 38,
            scale: 0,
        };
        let (enc_dec, _, dec_out) =
            roundtrip_via_frame(&dec_huge, &dec_dt, WriteProfile::L1Compact, None);
        assert_eq!(enc_dec.encoding, Encoding::Plain);
        assert!(dec_out.semantic_eq(&dec_huge));
    }

    #[test]
    fn shredded_map_roundtrip_null_values_duplicate_keys_and_w4_5000_keys_cap() {
        // 1. Hot keys (>= 5% presence), null values on promoted key, and duplicate promoted keys.
        let mut offsets = vec![0u32];
        let mut keys = Vec::new();
        let mut vals = Vec::new();
        let mut val_valid = Vec::new();

        for row in 0..40 {
            if row == 10 {
                // Row 10 will be SQL NULL map.
                offsets.push(keys.len() as u32);
                continue;
            }
            if row == 5 {
                // Duplicate promoted key "env" in same row (unsorted/duplicate test).
                keys.push("env");
                vals.push("prod");
                val_valid.push(true);
                keys.push("env");
                vals.push("staging");
                val_valid.push(true);
            } else {
                // Unique promoted key "env", with a SQL NULL value on row 7!
                keys.push("env");
                if row == 7 {
                    vals.push("");
                    val_valid.push(false);
                } else {
                    vals.push("prod");
                    val_valid.push(true);
                }
            }
            // Second promoted key "model" present on >= 50% of rows.
            if row % 2 == 0 {
                keys.push("model");
                vals.push("gpt-4o");
                val_valid.push(true);
            }
            // Rare key (< 5%: 1 row out of 40 = 2.5%).
            if row == 3 {
                keys.push("rare_trace_flag");
                vals.push("true");
                val_valid.push(true);
            }
            offsets.push(keys.len() as u32);
        }

        let mut row_nulls = Bitmap::new_ones(40);
        row_nulls.set(10, false).unwrap();

        let map_data = MapData::try_new(
            offsets,
            Utf8Data::try_from_slices(keys).unwrap(),
            Utf8Data::try_from_slices(vals).unwrap(),
            Some(Bitmap::from_bools(&val_valid)),
        )
        .unwrap();
        let col =
            FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), Some(row_nulls)).unwrap();

        let FlatData::MapUtf8(md) = col.data() else {
            unreachable!();
        };
        let promoted = select_shredded_keys(md);
        assert_eq!(promoted, vec!["env".to_string(), "model".to_string()]);

        let shredded = shred_map(&col, &promoted).unwrap();
        // "env" spilled on row 5 (duplicate), "model" had residual_count == 0!
        assert_eq!(shredded.residual_counts, vec![1, 0]);
        // Null value on row 7 stayed in "env" sub-column (did NOT spill to residual).
        assert!(shredded.sub_cols[0].in_map.get(7));
        assert!(!shredded.sub_cols[0].value_null.get(7));

        // Round-trip sub-columns and residual through binary encode/decode.
        let mut dec_subs = Vec::new();
        for sub in &shredded.sub_cols {
            let bytes = encode_shredded_subcol(sub, WriteProfile::L1Compact).unwrap();
            dec_subs.push(decode_shredded_subcol(&bytes, col.len()).unwrap());
        }
        let res_enc =
            encode_shredded_residual_block(&shredded.residual, WriteProfile::L1Compact).unwrap();
        let mut res_framed = Vec::new();
        let res_entry = res_enc
            .write_framed_entry(0, col.len() as u16, 0, 3, &mut res_framed)
            .unwrap();
        let (_, _, res_raw) = read_framed_block(&res_framed, MAX_BLOCK_RAW_BYTES).unwrap();
        let dec_res = decode_shredded_residual_block(&res_raw, &res_entry).unwrap();

        let sub_refs: Vec<(&str, &ShreddedSubCol)> = promoted
            .iter()
            .map(String::as_str)
            .zip(dec_subs.iter())
            .collect();
        let reconstructed = unshred_map(col.len(), col.nulls(), Some(&dec_res), &sub_refs).unwrap();
        assert!(reconstructed.semantic_eq(&col));

        // Direct single-key projection: "model" has residual_count == 0 -> ignores residual completely!
        let model_only =
            decode_single_map_key(col.len(), col.nulls(), "model", &dec_subs[1], 0, None).unwrap();
        for r in 0..col.len() {
            if r == 10 {
                assert!(model_only.is_null(r));
            } else if r % 2 == 0 {
                assert_eq!(
                    model_only.get_scalar(r).unwrap(),
                    ScalarValue::MapUtf8(vec![("model".into(), Some("gpt-4o".into()))])
                );
            } else {
                assert_eq!(
                    model_only.get_scalar(r).unwrap(),
                    ScalarValue::MapUtf8(vec![])
                );
            }
        }

        // Direct single-key projection on "env" (residual_count == 1) includes row 5 duplicates!
        let env_only = decode_single_map_key(
            col.len(),
            col.nulls(),
            "env",
            &dec_subs[0],
            shredded.residual_counts[0],
            Some(&dec_res),
        )
        .unwrap();
        assert_eq!(
            env_only.get_scalar(5).unwrap(),
            ScalarValue::MapUtf8(vec![
                ("env".into(), Some("prod".into())),
                ("env".into(), Some("staging".into())),
            ])
        );
        assert_eq!(
            env_only.get_scalar(7).unwrap(),
            ScalarValue::MapUtf8(vec![("env".into(), None)])
        );

        // 2. W4 5,000-unique-key map promotes 0 keys!
        let mut w4_offsets = vec![0u32];
        let mut w4_keys = Vec::with_capacity(5_000);
        let mut w4_vals = Vec::with_capacity(5_000);
        for i in 0..5_000 {
            w4_keys.push(format!("dyn_key_{i:04}"));
            w4_vals.push("v");
            if (i + 1) % 5 == 0 {
                w4_offsets.push(w4_keys.len() as u32);
            }
        }
        let w4_map = MapData::try_new(
            w4_offsets,
            Utf8Data::try_from_slices(w4_keys.iter().map(String::as_str)).unwrap(),
            Utf8Data::try_from_slices(w4_vals).unwrap(),
            None,
        )
        .unwrap();
        assert!(select_shredded_keys(&w4_map).is_empty());

        // 3. > 64 hot keys (>= 5% presence) capped strictly at MAX_SHREDDED_KEYS = 64.
        let mut cap_offsets = vec![0u32];
        let mut cap_keys = Vec::new();
        let mut cap_vals = Vec::new();
        for _row in 0..10 {
            for k in 0..100 {
                cap_keys.push(format!("hot_{k:03}"));
                cap_vals.push("1");
            }
            cap_offsets.push(cap_keys.len() as u32);
        }
        let cap_map = MapData::try_new(
            cap_offsets,
            Utf8Data::try_from_slices(cap_keys.iter().map(String::as_str)).unwrap(),
            Utf8Data::try_from_slices(cap_vals).unwrap(),
            None,
        )
        .unwrap();
        let cap_selected = select_shredded_keys(&cap_map);
        assert_eq!(cap_selected.len(), MAX_SHREDDED_KEYS);
    }

    #[test]
    fn zonemap_pruning_soundness_nan_utf8_carry_overflow_and_unsorted_dict() {
        // 1. F64 with NaN + -0.0:
        let f_col = Column::Flat(
            FlatColumn::try_new(FlatData::F64(vec![-0.0, 10.0, f64::NAN, 5.0]), None).unwrap(),
        );
        let zm = compute_zone_map(&f_col, false);
        assert_ne!(zm.block_flags & BLOCK_FLAG_HAS_NAN, 0);
        assert_ne!(zm.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);
        let f_entry = BlockEntry {
            first_row: 0,
            row_count: 4,
            null_count: zm.null_count,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding: Encoding::Alp,
            codec: Codec::None,
            block_flags: zm.block_flags,
            min_inline_len: zm.min_inline_len,
            max_inline_len: zm.max_inline_len,
            reserved: [0; 3],
            min_inline: zm.min_inline,
            max_inline: zm.max_inline,
        };
        // -0.0 canonicalized to +0.0.
        assert_eq!(
            f64::from_le_bytes(f_entry.min_inline[0..8].try_into().unwrap()).to_bits(),
            0.0f64.to_bits()
        );
        // HAS_NAN == 1 NEVER prunes >, >=, !=, = NaN, or IS NOT NULL:
        assert!(zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Gt(ZoneValue::F64(1000.0)),
            None
        ));
        assert!(zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Ge(ZoneValue::F64(1000.0)),
            None
        ));
        assert!(zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Ne(ZoneValue::F64(5.0)),
            None
        ));
        assert!(zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Eq(ZoneValue::F64(f64::NAN)),
            None
        ));
        assert!(zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::IsNotNull,
            None
        ));
        // Only prunes < x, <= x, = x (finite x) when x < min (or <= min for <):
        assert!(!zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Lt(ZoneValue::F64(0.0)),
            None
        ));
        assert!(!zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Le(ZoneValue::F64(-1.0)),
            None
        ));
        assert!(!zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Eq(ZoneValue::F64(-1.0)),
            None
        ));
        assert!(zone_map_may_match(
            &f_entry,
            &DataType::F64,
            0,
            &ZonePred::Eq(ZoneValue::F64(-0.0)),
            None
        ));

        // All-NaN F64 clears MIN_MAX_VALID:
        let nan_col = Column::Flat(
            FlatColumn::try_new(FlatData::F64(vec![f64::NAN, f64::NAN]), None).unwrap(),
        );
        let nan_zm = compute_zone_map(&nan_col, false);
        assert_ne!(nan_zm.block_flags & BLOCK_FLAG_HAS_NAN, 0);
        assert_eq!(nan_zm.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);

        // 2. Utf8/Binary >16 B carry increment and all-0xFF overflow:
        let long_s = format!("{}zzzz", "a".repeat(15)); // 19 bytes, prefix "aaaaaaaaaaaaaaaz"
        let u_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["aaaa", long_s.as_str()]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let u_zm = compute_zone_map(&u_col, false);
        assert_ne!(u_zm.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);
        // Carry increment on "aaaaaaaaaaaaaaaz" -> "aaaaaaaaaaaaaab{..."? Actually 'z' + 1 = '{' (0x7B).
        assert_eq!(&u_zm.max_inline[..16], b"aaaaaaaaaaaaaaa{");
        let u_entry = BlockEntry {
            first_row: 0,
            row_count: 2,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding: Encoding::Plain,
            codec: Codec::None,
            block_flags: u_zm.block_flags,
            min_inline_len: u_zm.min_inline_len,
            max_inline_len: u_zm.max_inline_len,
            reserved: [0; 3],
            min_inline: u_zm.min_inline,
            max_inline: u_zm.max_inline,
        };
        assert!(zone_map_may_match(
            &u_entry,
            &DataType::Utf8,
            0,
            &ZonePred::Eq(ZoneValue::Bytes(long_s.into_bytes())),
            None
        ));
        assert!(!zone_map_may_match(
            &u_entry,
            &DataType::Utf8,
            0,
            &ZonePred::Gt(ZoneValue::Bytes(b"aaaaaaaaaaaaaaa|".to_vec())),
            None
        ));

        // All-0xFF >16 B binary clears MIN_MAX_VALID:
        let ff_bytes = vec![0xFFu8; 20];
        let b_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Binary(
                    BinaryData::try_from_slices([b"aa".as_slice(), ff_bytes.as_slice()]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let b_zm = compute_zone_map(&b_col, false);
        assert_eq!(b_zm.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);

        // 3. Unsorted L0Fast Dict vs Sorted L1Compact Dict:
        let raw_strs = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["zebra", "apple", "zebra"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let Column::Flat(raw_flat) = &raw_strs else {
            unreachable!();
        };
        let (l0_dict, _) = DictBuilder::l0_fast().build(raw_flat).unwrap().unwrap();
        let (l0_enc, l0_entry, _) = roundtrip_via_frame(
            &raw_strs,
            &DataType::Utf8,
            WriteProfile::L0Fast,
            Some(&l0_dict),
        );
        assert_eq!(l0_enc.encoding, Encoding::Dict);
        // Unsorted dict prunes Eq("mango") (absent from dict), MUST NOT prune Lt/Prefix!
        assert!(!zone_map_may_match(
            &l0_entry,
            &DataType::Utf8,
            0,
            &ZonePred::Eq(ZoneValue::Bytes(b"mango".to_vec())),
            Some(&l0_dict)
        ));
        assert!(zone_map_may_match(
            &l0_entry,
            &DataType::Utf8,
            0,
            &ZonePred::Lt(ZoneValue::Bytes(b"apple".to_vec())),
            Some(&l0_dict)
        ));
        assert!(zone_map_may_match(
            &l0_entry,
            &DataType::Utf8,
            0,
            &ZonePred::Prefix(b"ab".to_vec()),
            Some(&l0_dict)
        ));

        // Sorted dict MAY prune range and Prefix!
        let (l1_dict, _) = DictBuilder::l1_compact().build(raw_flat).unwrap().unwrap();
        let (l1_enc, l1_entry, _) = roundtrip_via_frame(
            &raw_strs,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            Some(&l1_dict),
        );
        assert_eq!(l1_enc.encoding, Encoding::Dict);
        assert!(!zone_map_may_match(
            &l1_entry,
            &DataType::Utf8,
            COL_FLAG_DICT_SORTED,
            &ZonePred::Lt(ZoneValue::Bytes(b"apple".to_vec())),
            Some(&l1_dict)
        ));
        assert!(!zone_map_may_match(
            &l1_entry,
            &DataType::Utf8,
            COL_FLAG_DICT_SORTED,
            &ZonePred::Prefix(b"ab".to_vec()),
            Some(&l1_dict)
        ));
        assert!(zone_map_may_match(
            &l1_entry,
            &DataType::Utf8,
            COL_FLAG_DICT_SORTED,
            &ZonePred::Prefix(b"ap".to_vec()),
            Some(&l1_dict)
        ));

        // 4. Interval always clears MIN_MAX_VALID:
        let iv_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Interval(vec![Interval {
                    months: 1,
                    days: 2,
                    nanos: 3,
                }]),
                None,
            )
            .unwrap(),
        );
        let iv_zm = compute_zone_map(&iv_col, false);
        assert_eq!(iv_zm.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);
    }

    #[test]
    fn rle_and_dict_passthrough_emit_encoded_cetra_columns() {
        // 1. RleColumn with run-level validity bitmap emits Column::Rle directly.
        let run_vals = FlatColumn::try_new(
            FlatData::I64(vec![100, 0, 200]),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let rle_col = Column::Rle(RleColumn::try_new(run_vals, vec![30, 60, 120]).unwrap());
        let (rle_enc, _, rle_dec) =
            roundtrip_via_frame(&rle_col, &DataType::I64, WriteProfile::L0Fast, None);
        assert_eq!(rle_enc.encoding, Encoding::Rle);
        assert!(matches!(rle_dec, Column::Rle(_)));
        assert!(rle_dec.semantic_eq(&rle_col));

        // 2. Small FileDict (<= 256 KiB) emits Column::Dict directly (zero string expansion).
        let small_strs: Vec<String> = (0..10).map(|i| format!("model_{i}")).collect();
        let mut col_strs = Vec::with_capacity(200);
        for i in 0..200 {
            col_strs.push(small_strs[i % 10].as_str());
        }
        let flat_small = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(col_strs).unwrap()),
            None,
        )
        .unwrap();
        let (small_dict, _) = DictBuilder::l1_compact()
            .build(&flat_small)
            .unwrap()
            .unwrap();
        assert!(small_dict.memory_bytes() <= DICT_PASSTHROUGH_MAX_BYTES);

        // Also test DT01 wire roundtrip!
        let dt01 = small_dict.encode(Codec::Zstd, 3).unwrap();
        let small_dict_rt = FileDict::decode(&dt01, true).unwrap();
        assert_eq!(small_dict_rt, small_dict);

        let small_col = Column::Flat(flat_small);
        let (d_enc, _, d_dec) = roundtrip_via_frame(
            &small_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            Some(&small_dict_rt),
        );
        assert_eq!(d_enc.encoding, Encoding::Dict);
        assert!(matches!(d_dec, Column::Dict(DictColumn { .. })));
        assert!(d_dec.semantic_eq(&small_col));

        // 3. Large FileDict (> 256 KiB) materializes directly to Column::Flat.
        let big_entry = "X".repeat(256);
        let big_dict_strs: Vec<String> =
            (0..1_200).map(|i| format!("{big_entry}_{i:04}")).collect();
        let big_dict_flat = FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices(big_dict_strs.iter().map(String::as_str)).unwrap(),
            ),
            None,
        )
        .unwrap();
        let big_dict = FileDict::from_flat(big_dict_flat, false).unwrap();
        assert!(big_dict.memory_bytes() > DICT_PASSTHROUGH_MAX_BYTES);

        let sample_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(big_dict_strs[0..32].iter().map(String::as_str))
                        .unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let (big_enc, _, big_dec) = roundtrip_via_frame(
            &sample_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            Some(&big_dict),
        );
        assert_eq!(big_enc.encoding, Encoding::Dict);
        assert!(matches!(big_dec, Column::Flat(_)));
        assert!(big_dec.semantic_eq(&sample_col));

        // 4. L0Fast DictBuilder bails out cleanly if > 4096 distinct entries.
        let over_4096: Vec<String> = (0..4_100).map(|i| format!("u_{i}")).collect();
        let over_flat = FlatColumn::try_new(
            FlatData::Utf8(
                Utf8Data::try_from_slices(over_4096.iter().map(String::as_str)).unwrap(),
            ),
            None,
        )
        .unwrap();
        assert!(DictBuilder::l0_fast().build(&over_flat).unwrap().is_none());
    }

    #[test]
    fn full_4096_mini_block_and_dict_list_leaf() {
        // 4,096-row TimestampNs monotonic (DeltaBitpack) + I64 zigzag + Decimal128 ForBitpack + List(Utf8) dict leaf.
        let ts: Vec<i64> = (0..4096)
            .map(|i| 1_700_000_000_000_000_000 + i * 1_000_000)
            .collect();
        let ts_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: ts,
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        let (ts_enc, _, ts_dec) = roundtrip_via_frame(
            &ts_col,
            &DataType::timestamp_utc(),
            WriteProfile::L0Fast,
            None,
        );
        assert_eq!(ts_enc.encoding, Encoding::DeltaBitpack);
        assert!(ts_dec.semantic_eq(&ts_col));

        // Decimal128 with span <= u64::MAX selects ForBitpack.
        let dec_vals: Vec<i128> = (0..4096).map(|i| -1_000_000_000_000 + i * 1_000).collect();
        let dec_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: dec_vals,
                    precision: 38,
                    scale: 18,
                },
                None,
            )
            .unwrap(),
        );
        let (d_enc, _, d_dec) = roundtrip_via_frame(
            &dec_col,
            &DataType::cost_decimal(),
            WriteProfile::L1Compact,
            None,
        );
        assert_eq!(d_enc.encoding, Encoding::ForBitpack);
        assert!(d_dec.semantic_eq(&dec_col));

        // List(Utf8) leaf with repeated tags emits Column::Dict child.
        let mut offsets = vec![0u32];
        let mut child_tags = Vec::new();
        for i in 0..512 {
            child_tags.push(if i % 2 == 0 { "prod" } else { "staging" });
            child_tags.push("openai");
            offsets.push(child_tags.len() as u32);
        }
        let list_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets,
                    values: Box::new(Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(child_tags).unwrap()),
                            None,
                        )
                        .unwrap(),
                    )),
                },
                None,
            )
            .unwrap(),
        );
        let (l_enc, _, l_dec) = roundtrip_via_frame(
            &list_col,
            &DataType::List(Box::new(DataType::Utf8)),
            WriteProfile::L1Compact,
            None,
        );
        assert_eq!(l_enc.encoding, Encoding::ListEncoded);
        assert!(l_dec.semantic_eq(&list_col));
        let Column::Flat(lf) = &l_dec else {
            panic!("expected Flat List");
        };
        let FlatData::List { values, .. } = lf.data() else {
            panic!("expected List");
        };
        assert!(matches!(values.as_ref(), Column::Dict(_)));
    }

    #[test]
    fn rejects_corrupt_codec_inputs_without_oom_or_panic() {
        // Invalid ALP exp > 18 and exc_count > count.
        let bad_alp_exp = [19u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(decode_alp_body(&bad_alp_exp, 1, None).is_err());
        let bad_alp_exc = [0u8, 0, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(decode_alp_body(&bad_alp_exc, 2, None).is_err());

        // Invalid FSST symbol len 0 or 9.
        assert!(decode_fsst_binary(&[1, 0, 0], 1).is_err());
        assert!(decode_fsst_binary(&[1, 9, 0], 1).is_err());
        // Out-of-bounds FSST symbol code.
        let mut bad_code = vec![1, 1, b'a', 1, 1, 5]; // sym_count=1, len_bw=1, row_len=1, code=5
        assert!(decode_fsst_binary(&bad_code, 1).is_err());
        bad_code[5] = 0xFF; // truncated escape
        assert!(decode_fsst_binary(&bad_code, 1).is_err());

        // Corrupt DT01 CRC and invalid kind.
        let flat = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["a", "b"]).unwrap()),
            None,
        )
        .unwrap();
        let (dict, _) = DictBuilder::l1_compact().build(&flat).unwrap().unwrap();
        let mut dt01 = dict.encode(Codec::None, 0).unwrap();
        let last = dt01.len() - 1;
        dt01[last] ^= 0xFF;
        assert!(FileDict::decode(&dt01, true).is_err());

        // Corrupt RLE num_runs == 0 or > row_count.
        let bad_rle_entry = BlockEntry {
            first_row: 0,
            row_count: 4,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding: Encoding::Rle,
            codec: Codec::None,
            block_flags: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        assert!(decode_mini_block(&[0, 0, 0, 0], &bad_rle_entry, &DataType::I64, None).is_err());
        assert!(decode_mini_block(&[10, 0, 0, 0], &bad_rle_entry, &DataType::I64, None).is_err());
    }

    #[test]
    fn const_preserves_nan_neg_zero_and_all_same_dict_values_bit_exact() {
        let qnan = f64::from_bits(0x7ff8_0000_0000_1234);
        let neg_zero = -0.0f64;

        // 1. All-NaN Flat & Rle F64 round-trip bit-exact via Encoding::Const.
        let nan_flat =
            Column::Flat(FlatColumn::try_new(FlatData::F64(vec![qnan; 16]), None).unwrap());
        let nan_rle = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(FlatData::F64(vec![qnan]), None).unwrap(),
                vec![16],
            )
            .unwrap(),
        );
        for col in [&nan_flat, &nan_rle] {
            let (enc, entry, dec) =
                roundtrip_via_frame(col, &DataType::F64, WriteProfile::L1Compact, None);
            assert_eq!(enc.encoding, Encoding::Const);
            assert_ne!(entry.block_flags & BLOCK_FLAG_HAS_NAN, 0);
            assert_eq!(entry.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);
            let Column::Const {
                value: ScalarValue::F64(got),
                len,
            } = dec
            else {
                panic!("expected Const F64");
            };
            assert_eq!(len, 16);
            assert_eq!(got.to_bits(), qnan.to_bits());
        }

        // 2. All--0.0 Flat & Rle F64 round-trip bit-exact via Encoding::Const + zone map matches 0.0.
        let nz_flat =
            Column::Flat(FlatColumn::try_new(FlatData::F64(vec![neg_zero; 16]), None).unwrap());
        let nz_rle = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(FlatData::F64(vec![neg_zero]), None).unwrap(),
                vec![16],
            )
            .unwrap(),
        );
        for col in [&nz_flat, &nz_rle] {
            let (enc, entry, dec) =
                roundtrip_via_frame(col, &DataType::F64, WriteProfile::L0Fast, None);
            assert_eq!(enc.encoding, Encoding::Const);
            let Column::Const {
                value: ScalarValue::F64(got),
                ..
            } = dec
            else {
                panic!("expected Const F64");
            };
            assert_eq!(got.to_bits(), neg_zero.to_bits());
            assert!(zone_map_may_match(
                &entry,
                &DataType::F64,
                0,
                &ZonePred::Eq(ZoneValue::F64(0.0)),
                None
            ));
        }

        // 3. All-same-code Column::Dict (Utf8, FixedBytes8, FixedBytes16) round-trip bit-exact.
        let utf8_dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["unused", "openai"]).unwrap()),
            None,
        )
        .unwrap();
        let utf8_dict_col =
            Column::Dict(DictColumn::try_new(utf8_dict_vals, vec![1; 12], None).unwrap());
        let (u_enc, u_entry, u_dec) = roundtrip_via_frame(
            &utf8_dict_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            None,
        );
        assert_eq!(u_enc.encoding, Encoding::Const);
        assert!(u_dec.semantic_eq(&utf8_dict_col));
        assert!(zone_map_may_match(
            &u_entry,
            &DataType::Utf8,
            0,
            &ZonePred::Eq(ZoneValue::Bytes(b"openai".to_vec())),
            None
        ));
        assert!(!zone_map_may_match(
            &u_entry,
            &DataType::Utf8,
            0,
            &ZonePred::Eq(ZoneValue::Bytes(b"anthropic".to_vec())),
            None
        ));

        let f8_val = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let f8_dict_vals =
            FlatColumn::try_new(FlatData::FixedBytes8(vec![[0u8; 8], f8_val]), None).unwrap();
        let f8_dict_col =
            Column::Dict(DictColumn::try_new(f8_dict_vals, vec![1; 10], None).unwrap());
        let (f8_enc, _, f8_dec) = roundtrip_via_frame(
            &f8_dict_col,
            &DataType::FixedBytes8,
            WriteProfile::L0Fast,
            None,
        );
        assert_eq!(f8_enc.encoding, Encoding::Const);
        assert!(f8_dec.semantic_eq(&f8_dict_col));

        let f16_val = [0xAAu8; 16];
        let f16_dict_vals =
            FlatColumn::try_new(FlatData::FixedBytes16(vec![[0u8; 16], f16_val]), None).unwrap();
        let f16_dict_col =
            Column::Dict(DictColumn::try_new(f16_dict_vals, vec![1; 10], None).unwrap());
        let (f16_enc, _, f16_dec) = roundtrip_via_frame(
            &f16_dict_col,
            &DataType::FixedBytes16,
            WriteProfile::L0Fast,
            None,
        );
        assert_eq!(f16_enc.encoding, Encoding::Const);
        assert!(f16_dec.semantic_eq(&f16_dict_col));
    }

    #[test]
    fn list_with_over_65535_child_items_roundtrips_cleanly() {
        // 1,000 list rows * 70 items = 70,000 child items (> u16::MAX = 65,535).
        let rows = 1_000usize;
        let per_row = 70usize;
        let total = rows * per_row;
        let offsets: Vec<u32> = (0..=rows).map(|r| (r * per_row) as u32).collect();

        // 1. List(I64) with Rle child (> 65,535 items)
        let rle_child = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(FlatData::I64(vec![10, 20]), None).unwrap(),
                vec![35_000, total as u32],
            )
            .unwrap(),
        );
        let list_i64 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::I64,
                    offsets: offsets.clone(),
                    values: Box::new(rle_child),
                },
                None,
            )
            .unwrap(),
        );
        let (_, _, dec_i64) = roundtrip_via_frame(
            &list_i64,
            &DataType::List(Box::new(DataType::I64)),
            WriteProfile::L0Fast,
            None,
        );
        assert!(dec_i64.semantic_eq(&list_i64));

        // 2. List(Utf8) with 70,000 child items
        let strs: Vec<&str> = (0..total)
            .map(|i| if i % 2 == 0 { "tag_a" } else { "tag_b" })
            .collect();
        let utf8_child = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(strs).unwrap()),
                None,
            )
            .unwrap(),
        );
        let list_utf8 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets,
                    values: Box::new(utf8_child),
                },
                None,
            )
            .unwrap(),
        );
        let (_, _, dec_utf8) = roundtrip_via_frame(
            &list_utf8,
            &DataType::List(Box::new(DataType::Utf8)),
            WriteProfile::L1Compact,
            None,
        );
        assert!(dec_utf8.semantic_eq(&list_utf8));
    }

    #[test]
    fn rejects_dict_residual_and_events_amplification_bombs_before_take() {
        // 1. Large FileDict (> 256 KiB) referencing 300 KiB entry 300 times => 90 MiB > MAX_BLOCK_RAW_BYTES.
        let big_str = "A".repeat(300 * 1024);
        let big_dict_flat = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices([big_str.as_str()]).unwrap()),
            None,
        )
        .unwrap();
        let big_dict = FileDict::from_flat(big_dict_flat, false).unwrap();
        assert!(big_dict.memory_bytes() > DICT_PASSTHROUGH_MAX_BYTES);
        let codes = vec![0u32; 300];
        let mut dict_body = Vec::new();
        dict_body.extend_from_slice(&encode_dict_codes_body(&codes, None).unwrap());
        let err = decode_dict_column(&dict_body, 300, None, &big_dict).unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));

        // 2. ShreddedMap residual bomb: 1 key of 100 KiB repeated 1,000 times => 100 MiB > 64 MiB.
        let big_key = "K".repeat(100 * 1024);
        let mut res_raw = Vec::new();
        // 1 row with count = 1000
        res_raw.push(10); // cnt_bw = 10
        crate::simd::bitpack::pack_u32(&[1000], 10, &mut res_raw).unwrap();
        res_raw.extend_from_slice(&1u32.to_le_bytes()); // num_keys = 1
        res_raw.push(17); // klen_bw = 17
        crate::simd::bitpack::pack_u32(&[big_key.len() as u32], 17, &mut res_raw).unwrap();
        res_raw.extend_from_slice(&(big_key.len() as u32).to_le_bytes());
        res_raw.extend_from_slice(big_key.as_bytes());
        res_raw.push(0); // kcode_bw = 0 (all code 0!)
        res_raw.push(0); // val_nulls flag = 0
        res_raw.push(Encoding::Plain as u8);
        let empty_vals = vec![0u8; 1001 * 4]; // 1000 empty strings -> offsets all 0
        res_raw.extend_from_slice(&(empty_vals.len() as u32).to_le_bytes());
        res_raw.extend_from_slice(&empty_vals);
        let res_entry = BlockEntry {
            first_row: 0,
            row_count: 1,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: res_raw.len() as u32,
            encoding: Encoding::ShreddedMap,
            codec: Codec::None,
            block_flags: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        let err = decode_shredded_residual_block(&res_raw, &res_entry).unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));

        // 3. EventsStruct names dict bomb: 1 name of 100 KiB repeated 1,000 times => 100 MiB > 64 MiB.
        let mut ev_raw = Vec::new();
        ev_raw.push(10); // len_bw = 10
        crate::simd::bitpack::pack_u32(&[1000], 10, &mut ev_raw).unwrap();
        // times: Plain 1000 * 8 bytes of 0
        ev_raw.push(Encoding::Plain as u8);
        ev_raw.extend_from_slice(&((1000 * 8) as u32).to_le_bytes());
        ev_raw.extend(std::iter::repeat_n(0u8, 1000 * 8));
        // names: Dict with 100 KiB entry, codes all 0
        let ev_dict_flat = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices([big_key.as_str()]).unwrap()),
            None,
        )
        .unwrap();
        let ev_dict = FileDict::from_flat(ev_dict_flat, false).unwrap();
        let ev_dict_bytes = ev_dict.encode(Codec::None, 0).unwrap();
        let ev_codes = encode_dict_codes_body(&vec![0u32; 1000], None).unwrap();
        ev_raw.push(Encoding::Dict as u8);
        ev_raw.push(0); // sorted = false
        ev_raw.extend_from_slice(&(ev_dict_bytes.len() as u32).to_le_bytes());
        ev_raw.extend_from_slice(&ev_dict_bytes);
        ev_raw.extend_from_slice(&(ev_codes.len() as u32).to_le_bytes());
        ev_raw.extend_from_slice(&ev_codes);
        ev_raw.push(0); // attr_nulls flag = 0
        ev_raw.push(Encoding::Plain as u8);
        ev_raw.extend_from_slice(&(empty_vals.len() as u32).to_le_bytes());
        ev_raw.extend_from_slice(&empty_vals);
        let ev_entry = BlockEntry {
            encoding: Encoding::EventsStruct,
            raw_len: ev_raw.len() as u32,
            ..res_entry
        };
        let err = decode_mini_block(&ev_raw, &ev_entry, &DataType::Events, None).unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));
    }

    #[test]
    fn debug_redacts_all_payload_and_inline_bytes() {
        use crate::io::SliceReader;
        use crate::payload::PayloadRefRow;
        use crate::payload::pack::{ChunkPackBuilder, PackWriteOptions};
        use crate::simd::{Sbbf, TrigramBloom};

        let secret = b"SUPER_SECRET_PII";
        let mut inline16 = [0u8; 16];
        inline16.copy_from_slice(secret);

        let emb = EncodedMiniBlock {
            encoding: Encoding::Const,
            block_flags: BLOCK_FLAG_MIN_MAX_VALID,
            null_count: 0,
            min_inline_len: 16,
            max_inline_len: 16,
            min_inline: inline16,
            max_inline: inline16,
            raw_body: secret.to_vec(),
            preferred_codec: Codec::None,
        };
        let be = BlockEntry {
            first_row: 0,
            row_count: 1,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding: Encoding::Const,
            codec: Codec::None,
            block_flags: BLOCK_FLAG_MIN_MAX_VALID,
            min_inline_len: 16,
            max_inline_len: 16,
            reserved: [0; 3],
            min_inline: inline16,
            max_inline: inline16,
        };
        let zm = ZoneMapSummary {
            block_flags: BLOCK_FLAG_MIN_MAX_VALID,
            null_count: 0,
            min_inline_len: 16,
            max_inline_len: 16,
            min_inline: inline16,
            max_inline: inline16,
        };
        let pr = PayloadRefRow::Inline(secret.to_vec());
        let mut builder = ChunkPackBuilder::new([1u8; 16], PackWriteOptions::default());
        builder.intern_chunk(secret).unwrap();
        let sr = SliceReader::new(secret.to_vec());

        // Construct Sbbf & TrigramBloom directly containing the marker bytes.
        let mut bf_raw = Vec::new();
        bf_raw.extend_from_slice(&crate::BLOOM_MAGIC);
        bf_raw.extend_from_slice(&1u32.to_le_bytes());
        let mut bf_payload = Vec::with_capacity(32);
        bf_payload.extend_from_slice(secret);
        bf_payload.extend_from_slice(secret);
        let bf_crc = crc32c::crc32c(&bf_payload);
        bf_raw.extend_from_slice(&bf_crc.to_le_bytes());
        bf_raw.extend_from_slice(&bf_payload);
        let sbbf = Sbbf::from_bytes(&bf_raw).unwrap();

        let mut tb_bytes = Vec::with_capacity(256);
        for _ in 0..16 {
            tb_bytes.extend_from_slice(secret);
        }
        let tb = TrigramBloom::from_bytes(tb_bytes).unwrap();

        let outputs = [
            format!("{emb:?}"),
            format!("{be:?}"),
            format!("{zm:?}"),
            format!("{pr:?}"),
            format!("{builder:?}"),
            format!("{sr:?}"),
            format!("{sbbf:?}"),
            format!("{tb:?}"),
        ];

        for dbg in outputs {
            assert!(!dbg.contains("SUPER_SECRET_PII"), "leaked text in {dbg}");
            assert!(
                !dbg.contains("83, 85, 80, 69, 82"),
                "leaked decimal ASCII bytes in {dbg}"
            );
        }
    }

    fn map_rows_logical_eq(a: &FlatColumn, b: &FlatColumn) -> bool {
        if a.len() != b.len() {
            return false;
        }
        for row in 0..a.len() {
            if a.is_null(row) != b.is_null(row) {
                return false;
            }
            if a.is_null(row) {
                continue;
            }
            let (Ok(ScalarValue::MapUtf8(mut ea)), Ok(ScalarValue::MapUtf8(mut eb))) =
                (a.get_scalar(row), b.get_scalar(row))
            else {
                return false;
            };
            ea.sort();
            eb.sort();
            if ea != eb {
                return false;
            }
        }
        true
    }

    #[test]
    fn shredded_map_sorted_exact_semantic_eq_unsorted_logical_eq_and_validation() {
        // 1. Sorted map (like Cetra's BTreeMap-backed attributes) round-trips with exact semantic_eq.
        let sorted_map = MapData::try_new(
            vec![0, 3, 5, 7],
            Utf8Data::try_from_slices(["env", "model", "rare_a", "env", "model", "env", "rare_b"])
                .unwrap(),
            Utf8Data::try_from_slices(["prod", "gpt-4o", "1", "dev", "claude", "prod", "2"])
                .unwrap(),
            None,
        )
        .unwrap();
        let sorted_col =
            FlatColumn::try_new(FlatData::MapUtf8(Box::new(sorted_map)), None).unwrap();
        let promoted = vec!["env".to_owned(), "model".to_owned()];
        let shredded = shred_map(&sorted_col, &promoted).unwrap();
        let sub_refs: Vec<(&str, &ShreddedSubCol)> = promoted
            .iter()
            .map(String::as_str)
            .zip(shredded.sub_cols.iter())
            .collect();
        let unshredded = unshred_map(
            sorted_col.len(),
            sorted_col.nulls(),
            Some(&shredded.residual),
            &sub_refs,
        )
        .unwrap();
        assert!(unshredded.semantic_eq(&sorted_col));

        // 2. Unsorted map preserves logical entries (map_rows_logical_eq).
        let unsorted_map = MapData::try_new(
            vec![0, 3],
            Utf8Data::try_from_slices(["rare_z", "model", "env"]).unwrap(),
            Utf8Data::try_from_slices(["9", "gpt-4o", "prod"]).unwrap(),
            None,
        )
        .unwrap();
        let unsorted_col =
            FlatColumn::try_new(FlatData::MapUtf8(Box::new(unsorted_map)), None).unwrap();
        let sh_u = shred_map(&unsorted_col, &promoted).unwrap();
        let sub_u_refs: Vec<(&str, &ShreddedSubCol)> = promoted
            .iter()
            .map(String::as_str)
            .zip(sh_u.sub_cols.iter())
            .collect();
        let un_u = unshred_map(
            unsorted_col.len(),
            unsorted_col.nulls(),
            Some(&sh_u.residual),
            &sub_u_refs,
        )
        .unwrap();
        assert!(map_rows_logical_eq(&un_u, &unsorted_col));

        // 3. M2: decode_single_map_key rejects residual_count > 0 && residual.is_none(),
        //    row_nulls len mismatch, and residual len mismatch.
        assert!(matches!(
            decode_single_map_key(
                sorted_col.len(),
                None,
                "env",
                &shredded.sub_cols[0],
                1,
                None
            ),
            Err(StatamError::Corrupt { .. })
        ));
        let bad_nulls = Bitmap::new_ones(sorted_col.len() + 1);
        assert!(matches!(
            decode_single_map_key(
                sorted_col.len(),
                Some(&bad_nulls),
                "env",
                &shredded.sub_cols[0],
                0,
                None
            ),
            Err(StatamError::Corrupt { .. })
        ));
        let bad_res = FlatColumn::empty(&DataType::MapUtf8);
        assert!(matches!(
            decode_single_map_key(
                sorted_col.len(),
                None,
                "env",
                &shredded.sub_cols[0],
                1,
                Some(&bad_res)
            ),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn child_dict_type_mismatch_rejected_and_const_long_string_uses_1_run_rle_and_l0fast_choices() {
        // 1. M3: ShreddedSubCol with Dict whose inline dict is FixedBytes8 instead of Utf8 must fail!
        let sub = ShreddedSubCol {
            in_map: Bitmap::new_ones(4),
            value_null: Bitmap::new_ones(4),
            values: Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["a", "b", "a", "b"]).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        };
        let encoded_sub = encode_shredded_subcol(&sub, WriteProfile::L0Fast).unwrap();
        // Rebuild payload with FixedBytes8 inline dict instead of Utf8.
        let bad_dict_vals =
            FlatColumn::try_new(FlatData::FixedBytes8(vec![[1u8; 8], [2u8; 8]]), None).unwrap();
        let bad_dict = FileDict::from_flat(bad_dict_vals, false).unwrap();
        let bad_dict_bytes = bad_dict.encode(Codec::None, 0).unwrap();
        let codes_bytes = encode_dict_codes_body(&[0, 1, 0, 1], None).unwrap();
        let mut forged = vec![
            1, // in_map all ones
            1, // value_null all ones
            Encoding::Dict as u8,
            0, // flags
            0, // min_inline_len
        ];
        forged.extend_from_slice(&[0u8; 16]);
        forged.push(1); // has_dict = 1
        forged.push(0); // sorted = false
        forged.extend_from_slice(&(bad_dict_bytes.len() as u32).to_le_bytes());
        forged.extend_from_slice(&bad_dict_bytes);
        forged.extend_from_slice(&(codes_bytes.len() as u32).to_le_bytes());
        forged.extend_from_slice(&codes_bytes);
        assert_ne!(encoded_sub, forged);
        assert!(matches!(
            decode_shredded_subcol(&forged, 4),
            Err(StatamError::SchemaMismatch { .. })
        ));

        // 2. M4: 4,096-row constant 32-byte string encodes as 1-run Rle (both Flat and Const).
        let long_s = "constant_string_exceeding_16_byt";
        assert_eq!(long_s.len(), 32);
        let flat_long = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(vec![long_s; 4096]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let const_long = Column::Const {
            value: ScalarValue::Utf8(long_s.to_owned()),
            len: 4096,
        };
        for col in [&flat_long, &const_long] {
            let (enc, _, dec) =
                roundtrip_via_frame(col, &DataType::Utf8, WriteProfile::L0Fast, None);
            assert_eq!(enc.encoding, Encoding::Rle);
            assert!(enc.raw_body.len() < 128);
            assert!(dec.semantic_eq(col));
        }

        // 3. M5: L0Fast picks ForBitpack for unsorted I64 and DeltaBitpack for monotonic I64.
        let unsorted: Vec<i64> = (0..64)
            .map(|i| if i % 2 == 0 { 1000 + i } else { 10 + i })
            .collect();
        let unsorted_col =
            Column::Flat(FlatColumn::try_new(FlatData::I64(unsorted), None).unwrap());
        let (u_enc, _, u_dec) =
            roundtrip_via_frame(&unsorted_col, &DataType::I64, WriteProfile::L0Fast, None);
        assert_eq!(u_enc.encoding, Encoding::ForBitpack);
        assert!(u_dec.semantic_eq(&unsorted_col));
    }
}
