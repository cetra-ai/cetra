//! Columnar mini-block codecs (`L0Fast` & `L1Compact`) for all 16 `cetra-column` types
//! plus sound mini-block zone maps.

pub mod alp;
pub mod dict;
pub mod fsst;
pub mod nested;
pub mod numeric;
pub mod zonemap;

use std::fmt;

use bytes::Bytes;
use cetra_column::{
    BinaryData, Bitmap, Column, DataType, FlatColumn, FlatData, RleColumn, ScalarValue, Utf8Data,
};

use crate::StatamError;
use crate::codec::dict::{FileDict, decode_dict_column, decode_dict_take, encode_dict_codes_body};
use crate::codec::nested::{decode_nested_column, encode_nested_column};
use crate::codec::numeric::{
    PlainSubchunkedView, decode_const_block, decode_inner_flat, decode_inner_flat_selected,
    decode_inner_take, decode_narrow_column, decode_narrow_column_selected,
    decode_narrow_column_take, decode_plain_varlen_selected, encode_narrow_column_with_locality,
    parse_rle_header, read_optional_nulls, split_optional_null_bytes, try_encode_const_or_null,
    validate_inner_take_body, write_optional_nulls,
};
use crate::codec::zonemap::{compute_zone_map, count_col_nulls};
use crate::format::{
    BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_FSST_SUBCHUNKED, BLOCK_FLAG_HAS_NULLS,
    BLOCK_FLAG_MIN_MAX_VALID, BLOCK_FLAG_NO_INLINE_PAYLOADS, BLOCK_FLAG_PLAIN_SUBCHUNKED,
    BlockEntry, Codec, Encoding, MAX_NESTING_DEPTH, write_framed_block_adaptive,
    write_framed_block_with_min_savings,
};

pub use alp::{
    ALP_MAX_EXP, decode_alp_body, decode_alp_selected, decode_alp_take, encode_alp_body,
};
pub use dict::{
    DICT_HEADER_LEN, DICT_PASSTHROUGH_MAX_BYTES, DictBuildMode, DictBuilder, DictValueKind,
    FileDict as CodecFileDict, L0_DICT_MAX_ENTRIES, decode_dict_codes_body, decode_dict_selected,
    encode_dict_codes_body as encode_dict_codes,
};
pub use fsst::{
    FSST_ESCAPE, FSST_MAX_SYM_LEN, FSST_MAX_SYMBOLS, FSST_SUBCHUNK_ROWS, FsstBlockView, FsstTable,
    decode_fsst_binary, decode_fsst_binary_selected, decode_fsst_utf8, decode_fsst_utf8_selected,
    encode_fsst_body,
};
pub use nested::{
    ShreddedMapBlocks, ShreddedSubCol, decode_nested_column_selected,
    decode_shredded_residual_block, decode_shredded_residual_block_keys_only,
    decode_shredded_residual_block_selected, decode_shredded_subcol,
    decode_shredded_subcol_in_map_only, decode_shredded_subcol_selected, decode_single_map_key,
    encode_shredded_residual_block, encode_shredded_subcol, select_shredded_keys, shred_map,
    unshred_map, unshred_map_keys_only_selected, unshred_map_selected,
};
pub use zonemap::{
    ZoneMapSummary, ZonePred, ZoneValue, compute_zone_map as block_zone_map,
    zone_map_always_matches, zone_map_may_match,
};

/// Returns the raw UTF-8 bytes for `row` in `u` in `O(1)` without re-running `std::str::from_utf8`.
#[inline]
pub(crate) fn utf8_bytes_at(u: &Utf8Data, row: usize) -> Option<&[u8]> {
    let offs = u.offsets();
    let start = *offs.get(row)? as usize;
    let end = *offs.get(row + 1)? as usize;
    u.bytes().get(start..end)
}

/// Returns the byte length of `row` in `u` in `O(1)` without UTF-8 re-validation.
#[inline]
pub(crate) fn utf8_len_at(u: &Utf8Data, row: usize) -> usize {
    let offs = u.offsets();
    let Some(&start) = offs.get(row) else {
        return 0;
    };
    let Some(&end) = row.checked_add(1).and_then(|next| offs.get(next)) else {
        return 0;
    };
    end.saturating_sub(start) as usize
}

/// Whole-buffer validated `&str` view over a [`Utf8Data`], enabling `O(1)` per-row `&str`
/// slicing without repeated per-row `std::str::from_utf8` scans.
#[derive(Clone, Copy)]
pub(crate) struct FastUtf8Ref<'a> {
    s: &'a str,
    offsets: &'a [u32],
}

impl<'a> FastUtf8Ref<'a> {
    pub(crate) fn new(u: &'a Utf8Data) -> Result<Self, StatamError> {
        let s = std::str::from_utf8(u.bytes())
            .map_err(|_| StatamError::Column(cetra_column::ColumnError::InvalidUtf8))?;
        Ok(Self {
            s,
            offsets: u.offsets(),
        })
    }

    #[inline]
    pub(crate) fn get(&self, row: usize) -> Option<&'a str> {
        let start = *self.offsets.get(row)? as usize;
        let end = *self.offsets.get(row + 1)? as usize;
        self.s.get(start..end)
    }
}

/// Encoding and compression profile controlling speed vs. density trade-offs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteProfile {
    /// Fast self-contained single-pass profile (`Const`, `Rle`, `ForBitpack`/`DeltaBitpack` via
    /// 32-element monotonicity sample, `BitpackBool`, bounded `Dict <= 512`, `Plain + Zstd(-5)/Lz4`).
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
        self.write_framed_entry_with_min_savings(
            first_row,
            row_count,
            base_offset,
            zstd_level,
            0,
            out,
        )
    }

    /// Frames this mini-block into `out` (unless inline `ConstNull`/`Const`) using single-pass
    /// adaptive LZ4-vs-Zstd selection when `preferred_codec == Codec::Zstd && lz4_pct_cap > 0`.
    #[allow(clippy::too_many_arguments)]
    pub fn write_framed_entry_adaptive(
        &self,
        first_row: u32,
        row_count: u16,
        base_offset: u64,
        zstd_level: i32,
        lz4_pct_cap: u16,
        min_savings_pct: u8,
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
        let info = write_framed_block_adaptive(
            self.encoding,
            &self.raw_body,
            self.preferred_codec,
            zstd_level,
            lz4_pct_cap,
            min_savings_pct,
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

    /// Frames this mini-block into `out` (unless inline `ConstNull`/`Const`), requiring at
    /// least `min_savings_pct` compression savings when `min_savings_pct > 0 && raw_body.len() >= 1024`.
    pub fn write_framed_entry_with_min_savings(
        &self,
        first_row: u32,
        row_count: u16,
        base_offset: u64,
        zstd_level: i32,
        min_savings_pct: u8,
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
        let info = write_framed_block_with_min_savings(
            self.encoding,
            &self.raw_body,
            self.preferred_codec,
            zstd_level,
            min_savings_pct,
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

    if matches!(
        dt,
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links
    ) {
        let len = col.len();
        let null_count_usize = count_col_nulls(col);
        let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
        let mut block_flags = if null_count_usize > 0 {
            BLOCK_FLAG_HAS_NULLS
        } else {
            0
        };
        if null_count_usize == len {
            block_flags |= BLOCK_FLAG_ALL_NULL;
        }
        let zm = ZoneMapSummary {
            block_flags,
            null_count,
            min_inline_len: 0,
            max_inline_len: 0,
            min_inline: [0u8; 16],
            max_inline: [0u8; 16],
        };
        if let Some(inline) = try_encode_const_or_null(col, dt, &zm) {
            return Ok(inline);
        }
        return encode_nested_column(col, dt, profile, MAX_NESTING_DEPTH, Some(&zm));
    }

    let zm = compute_zone_map(col);
    if let Some(inline) = try_encode_const_or_null(col, dt, &zm) {
        return Ok(inline);
    }

    if let Some(dict) = file_dict
        && let Some(dict_block) = try_encode_with_file_dict(col, dict, profile, &zm)?
    {
        return Ok(dict_block);
    }

    encode_narrow_column_with_locality(col, dt, profile, Some(&zm), is_wide_row)
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
        Column::Dict(d) if d.values() == dict.values() => (d.codes().to_vec(), d.nulls().cloned()),
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

#[inline]
fn validate_block_entry_flags(entry: &BlockEntry, dt: &DataType) -> Result<(), StatamError> {
    if (entry.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 && entry.encoding != Encoding::Fsst {
        return Err(StatamError::Corrupt {
            reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst encoding",
        });
    }
    if (entry.block_flags & BLOCK_FLAG_PLAIN_SUBCHUNKED) != 0 {
        if entry.encoding != Encoding::Plain || entry.codec != Codec::None {
            return Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED requires Plain encoding with Codec::None",
            });
        }
        if !matches!(dt, DataType::Utf8 | DataType::Binary) {
            return Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED only valid for Utf8/Binary",
            });
        }
    }
    if (entry.block_flags & BLOCK_FLAG_NO_INLINE_PAYLOADS) != 0
        && entry.encoding != Encoding::PayloadRefs
    {
        return Err(StatamError::Corrupt {
            reason: "BLOCK_FLAG_NO_INLINE_PAYLOADS set on non-PayloadRefs encoding",
        });
    }
    Ok(())
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
    validate_block_entry_flags(entry, dt)?;
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
/// supports sparse decode (`Fsst`, `Dict`, `Rle`, sparse `Plain` varlen/map, and nested blocks).
pub fn decode_mini_block_selected(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    file_dict: Option<&FileDict>,
    mask: &Bitmap,
) -> Result<Column, StatamError> {
    validate_block_entry_flags(entry, dt)?;
    let row_count = entry.row_count as usize;
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if !mask.all_ones() {
        if entry.encoding == Encoding::Dict {
            dt.validate(MAX_NESTING_DEPTH)?;
            if row_count == 0 {
                if !raw_body.is_empty() {
                    return Err(StatamError::Corrupt {
                        reason: "trailing bytes in empty mini-block",
                    });
                }
                return Ok(Column::Flat(FlatColumn::empty(dt)));
            }
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
            return decode_dict_selected(after_nulls, row_count, nulls, dict, mask);
        }
        if entry.encoding == Encoding::Rle && mask.count_ones().saturating_mul(16) < row_count {
            dt.validate(MAX_NESTING_DEPTH)?;
            return decode_rle_block_selected(raw_body, entry, dt, mask);
        }
        if matches!(
            entry.encoding,
            Encoding::ListEncoded
                | Encoding::ShreddedMap
                | Encoding::EventsStruct
                | Encoding::LinksStruct
        ) && mask.count_ones().saturating_mul(16) < row_count
        {
            dt.validate(MAX_NESTING_DEPTH)?;
            return decode_nested_column_selected(raw_body, entry, dt, MAX_NESTING_DEPTH, mask);
        }
        if entry.encoding == Encoding::Fsst
            || (entry.encoding == Encoding::Plain
                && (entry.block_flags & BLOCK_FLAG_PLAIN_SUBCHUNKED) != 0)
            || (matches!(
                entry.encoding,
                Encoding::Plain
                    | Encoding::BitpackBool
                    | Encoding::ForBitpack
                    | Encoding::DeltaBitpack
                    | Encoding::Alp
            ) && mask.count_ones().saturating_mul(16) < row_count)
        {
            dt.validate(MAX_NESTING_DEPTH)?;
            return decode_narrow_column_selected(raw_body, entry, dt, mask);
        }
    }
    decode_mini_block(raw_body, entry, dt, file_dict)
}

/// Decodes one mini-block from owned zero-copy `raw_body: Bytes`, parallelizing sub-chunked
/// `Fsst` and `Plain` (`Utf8` / `Binary`) decode across `threads` when eligible and outside a
/// parallel scope.
pub(crate) fn decode_mini_block_selected_parallel_bytes(
    raw_body: Bytes,
    entry: &BlockEntry,
    dt: &DataType,
    file_dict: Option<&FileDict>,
    mask: &Bitmap,
    threads: usize,
) -> Result<Column, StatamError> {
    validate_block_entry_flags(entry, dt)?;
    dt.validate(MAX_NESTING_DEPTH)?;
    let row_count = entry.row_count as usize;
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if threads > 1
        && !crate::parallel::in_parallel_scope()
        && row_count > 0
        && entry.encoding == Encoding::Plain
        && (entry.block_flags & BLOCK_FLAG_PLAIN_SUBCHUNKED) != 0
        && matches!(dt, DataType::Utf8 | DataType::Binary)
        && (mask.all_ones() || mask.count_ones() >= 1024)
    {
        let (nulls, after_nulls) = read_optional_nulls(&raw_body, row_count, entry.block_flags)?;
        let nulls_prefix_len = raw_body.len() - after_nulls.len();
        let (offsets, bytes) = if mask.all_ones() {
            PlainSubchunkedView::decode_all_parallel_bytes(
                raw_body,
                nulls_prefix_len,
                row_count,
                crate::MAX_BLOCK_RAW_BYTES,
                threads,
            )?
        } else {
            PlainSubchunkedView::decode_selected_parallel_bytes(
                raw_body,
                nulls_prefix_len,
                row_count,
                mask,
                nulls.as_ref(),
                crate::MAX_BLOCK_RAW_BYTES,
                threads,
            )?
        };
        let data = match dt {
            DataType::Utf8 => FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?),
            DataType::Binary => FlatData::Binary(BinaryData::try_new(offsets, bytes)?),
            _ => unreachable!(),
        };
        return Ok(Column::Flat(FlatColumn::try_new(data, nulls)?));
    }
    if threads > 1
        && !crate::parallel::in_parallel_scope()
        && row_count > 0
        && entry.encoding == Encoding::Fsst
        && (entry.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0
        && matches!(dt, DataType::Utf8 | DataType::Binary)
        && (mask.all_ones() || mask.count_ones() >= 1024)
    {
        let (nulls, after_nulls) = read_optional_nulls(&raw_body, row_count, entry.block_flags)?;
        let nulls_prefix_len = raw_body.len() - after_nulls.len();
        let (data, out_nulls) = if mask.all_ones() {
            let data = match dt {
                DataType::Utf8 => FlatData::Utf8(fsst::decode_fsst_utf8_subchunked_parallel(
                    raw_body,
                    nulls_prefix_len,
                    row_count,
                    crate::MAX_BLOCK_RAW_BYTES,
                    threads,
                )?),
                DataType::Binary => FlatData::Binary(fsst::decode_fsst_binary_subchunked_parallel(
                    raw_body,
                    nulls_prefix_len,
                    row_count,
                    crate::MAX_BLOCK_RAW_BYTES,
                    threads,
                )?),
                _ => unreachable!(),
            };
            (data, nulls)
        } else {
            match dt {
                DataType::Utf8 => {
                    let (u, n) = fsst::decode_fsst_utf8_selected_subchunked_parallel(
                        raw_body,
                        nulls_prefix_len,
                        row_count,
                        mask,
                        nulls.as_ref(),
                        crate::MAX_BLOCK_RAW_BYTES,
                        threads,
                    )?;
                    (FlatData::Utf8(u), n)
                }
                DataType::Binary => {
                    let (b, n) = fsst::decode_fsst_binary_selected_subchunked_parallel(
                        raw_body,
                        nulls_prefix_len,
                        row_count,
                        mask,
                        nulls.as_ref(),
                        crate::MAX_BLOCK_RAW_BYTES,
                        threads,
                    )?;
                    (FlatData::Binary(b), n)
                }
                _ => unreachable!(),
            }
        };
        return Ok(Column::Flat(FlatColumn::try_new(data, out_nulls)?));
    }
    decode_mini_block_selected(&raw_body, entry, dt, file_dict, mask)
}

/// Parallel `Bytes`-backed variant of [`decode_mini_block_take`] that parallelizes
/// `BLOCK_FLAG_PLAIN_SUBCHUNKED` (`Utf8` / `Binary`) sub-chunk decompression across `threads`
/// when `>= 2` sub-chunks are touched outside an existing parallel scope.
pub(crate) fn decode_mini_block_take_parallel_bytes(
    raw_body: Bytes,
    entry: &BlockEntry,
    dt: &DataType,
    file_dict: Option<&FileDict>,
    indices: &[usize],
    threads: usize,
) -> Result<Column, StatamError> {
    if threads > 1
        && !crate::parallel::in_parallel_scope()
        && entry.row_count > 0
        && indices.len() >= 2
        && entry.encoding == Encoding::Plain
        && (entry.block_flags & BLOCK_FLAG_PLAIN_SUBCHUNKED) != 0
        && matches!(dt, DataType::Utf8 | DataType::Binary)
    {
        validate_block_entry_flags(entry, dt)?;
        dt.validate(MAX_NESTING_DEPTH)?;
        let row_count = entry.row_count as usize;
        if indices
            .len()
            .checked_mul(16)
            .is_none_or(|b| b > crate::MAX_BLOCK_RAW_BYTES)
        {
            return Err(StatamError::BoundsExceeded {
                reason: "mini-block take indices exceed MAX_BLOCK_RAW_BYTES",
            });
        }
        for &row in indices {
            if row >= row_count {
                return Err(StatamError::Corrupt {
                    reason: "mini-block take index out of bounds",
                });
            }
        }
        let (full_nulls, after_nulls) =
            read_optional_nulls(&raw_body, row_count, entry.block_flags)?;
        let nulls_prefix_len = raw_body.len() - after_nulls.len();
        let compact_nulls = if let Some(ref bm) = full_nulls {
            let mut out = Bitmap::new_zeroed(indices.len());
            let mut valid_cnt = 0usize;
            for (pos, &row) in indices.iter().enumerate() {
                if bm.get(row) {
                    let _ = out.set(pos, true);
                    valid_cnt += 1;
                }
            }
            if valid_cnt == 0 {
                let _ = PlainSubchunkedView::parse(after_nulls, row_count)?;
                return Ok(Column::Const {
                    value: ScalarValue::Null(dt.clone()),
                    len: indices.len(),
                });
            }
            if valid_cnt == indices.len() {
                None
            } else {
                Some(out)
            }
        } else {
            None
        };
        let (offsets, bytes) = PlainSubchunkedView::decode_take_parallel_bytes(
            raw_body,
            nulls_prefix_len,
            row_count,
            indices,
            compact_nulls.as_ref(),
            crate::MAX_BLOCK_RAW_BYTES,
            threads,
        )?;
        let data = match dt {
            DataType::Utf8 => FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?),
            DataType::Binary => FlatData::Binary(BinaryData::try_new(offsets, bytes)?),
            _ => unreachable!(),
        };
        return Ok(Column::Flat(FlatColumn::try_new(data, compact_nulls)?));
    }
    decode_mini_block_take(&raw_body, entry, dt, file_dict, indices)
}

/// Decodes only `indices` of one mini-block into a compact `indices.len()`-row [`Column`].
pub(crate) fn decode_mini_block_take(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    file_dict: Option<&FileDict>,
    indices: &[usize],
) -> Result<Column, StatamError> {
    validate_block_entry_flags(entry, dt)?;
    dt.validate(MAX_NESTING_DEPTH)?;
    let row_count = entry.row_count as usize;
    if indices
        .len()
        .checked_mul(16)
        .is_none_or(|b| b > crate::MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "mini-block take indices exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    for &row in indices {
        if row >= row_count {
            return Err(StatamError::Corrupt {
                reason: "mini-block take index out of bounds",
            });
        }
    }

    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        let col = decode_const_block(entry, dt)?;
        return match col {
            Column::Const { value, .. } => Ok(Column::Const {
                value,
                len: indices.len(),
            }),
            other => Ok(other),
        };
    }

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
        let (null_bytes, after_nulls) =
            split_optional_null_bytes(raw_body, row_count, entry.block_flags)?;
        return decode_dict_take(after_nulls, row_count, null_bytes, dict, indices);
    }

    if !matches!(
        entry.encoding,
        Encoding::ListEncoded
            | Encoding::ShreddedMap
            | Encoding::EventsStruct
            | Encoding::LinksStruct
    ) && let Some(col) = decode_narrow_column_take(raw_body, entry, dt, indices)?
    {
        return Ok(col);
    }

    let mut mask = Bitmap::new_zeroed(row_count);
    let mut u32_indices = Vec::with_capacity(indices.len());
    for &row in indices {
        let _ = mask.set(row, true);
        let idx_u32 = u32::try_from(row).map_err(|_| StatamError::BoundsExceeded {
            reason: "mini-block take index overflows u32",
        })?;
        u32_indices.push(idx_u32);
    }
    let col = decode_mini_block_selected(raw_body, entry, dt, file_dict, &mask)?;
    let pool = cetra_column::GlobalPool::standalone(crate::MAX_BATCH_DECODED_BYTES);
    let mut res = pool.register("decode_mini_block_take", false);
    let taken = col
        .take(&u32_indices, Some(&mut res))
        .map_err(|e| match e {
            cetra_column::ColumnError::MemoryLimit { .. } => StatamError::BoundsExceeded {
                reason: "mini-block take exceeds MAX_BATCH_DECODED_BYTES",
            },
            other => StatamError::Column(other),
        })?;
    Ok(Column::Flat(taken))
}

fn decode_rle_block_selected(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    mask: &Bitmap,
) -> Result<Column, StatamError> {
    let row_count = entry.row_count as usize;
    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty narrow block",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }
    let hdr = parse_rle_header(raw_body, row_count, entry.block_flags)?;
    let num_runs = hdr.num_runs;
    let run_nulls = hdr.run_nulls;
    let run_ends = hdr.run_ends;
    let inner_enc = hdr.inner_enc;
    let inner_body = hdr.inner_body;

    let mut run_mask = Bitmap::new_zeroed(num_runs);
    let mut run_idx = 0usize;
    let mut selected_runs = 0usize;
    let mut first_selected_run = 0usize;
    let mut any_valid_run = false;
    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        while (run_ends[run_idx] as usize) <= row {
            run_idx += 1;
        }
        if !run_mask.get(run_idx) {
            let _ = run_mask.set(run_idx, true);
            if selected_runs == 0 {
                first_selected_run = run_idx;
            }
            selected_runs += 1;
            if run_nulls.as_ref().is_none_or(|bm| bm.get(run_idx)) {
                any_valid_run = true;
            }
        }
    }

    if selected_runs == 0 || !any_valid_run {
        validate_inner_take_body(inner_body, inner_enc, 0, dt, num_runs)?;
        return Ok(Column::Const {
            value: ScalarValue::Null(dt.clone()),
            len: row_count,
        });
    }

    if selected_runs == 1 {
        let single = decode_inner_take(
            inner_body,
            inner_enc,
            num_runs,
            run_nulls.as_ref(),
            dt,
            &[first_selected_run],
        )?;
        let scalar = single.get_scalar(0)?;
        return Ok(Column::Const {
            value: scalar,
            len: row_count,
        });
    }

    let run_values = match (inner_enc, dt) {
        (Encoding::Fsst, DataType::Utf8) => {
            let (u, n) =
                decode_fsst_utf8_selected(inner_body, num_runs, &run_mask, run_nulls.as_ref())?;
            FlatColumn::try_new(FlatData::Utf8(u), n)?
        }
        (Encoding::Fsst, DataType::Binary) => {
            let (b, n) =
                decode_fsst_binary_selected(inner_body, num_runs, &run_mask, run_nulls.as_ref())?;
            FlatColumn::try_new(FlatData::Binary(b), n)?
        }
        (Encoding::Plain, DataType::Utf8) if selected_runs.saturating_mul(4) < num_runs => {
            let (offsets, data_bytes) =
                decode_plain_varlen_selected(inner_body, num_runs, &run_mask, run_nulls.as_ref())?;
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_new(offsets, data_bytes)?),
                run_nulls,
            )?
        }
        (Encoding::Plain, DataType::Binary) if selected_runs.saturating_mul(4) < num_runs => {
            let (offsets, data_bytes) =
                decode_plain_varlen_selected(inner_body, num_runs, &run_mask, run_nulls.as_ref())?;
            FlatColumn::try_new(
                FlatData::Binary(BinaryData::try_new(offsets, data_bytes)?),
                run_nulls,
            )?
        }
        _ if selected_runs.saturating_mul(16) < num_runs => {
            decode_inner_flat_selected(inner_body, inner_enc, num_runs, dt, run_nulls, &run_mask)?
        }
        _ => decode_inner_flat(inner_body, inner_enc, num_runs, dt, run_nulls)?,
    };

    let rle = RleColumn::try_new(run_values, run_ends)?;
    Ok(Column::Rle(rle))
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
    use crate::simd::bitpack::packed_bytes_len;

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
        let zm = compute_zone_map(&f_col);
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
        let nan_zm = compute_zone_map(&nan_col);
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
        let u_zm = compute_zone_map(&u_col);
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
        let b_zm = compute_zone_map(&b_col);
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
        let iv_zm = compute_zone_map(&iv_col);
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
        #[cfg(miri)]
        let dt01 = small_dict.encode(Codec::Lz4, 0).unwrap();
        #[cfg(not(miri))]
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

    fn frame_raw_body(enc: &EncodedMiniBlock, row_count: usize) -> (BlockEntry, Vec<u8>) {
        let mut framed = Vec::new();
        let entry = enc
            .write_framed_entry(0, row_count as u16, 0, 3, &mut framed)
            .unwrap();
        let raw_body = if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const
        {
            Vec::new()
        } else {
            let (_, _, body) = read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap();
            body
        };
        (entry, raw_body)
    }

    fn assert_sparse_scalar_parity(col: &Column, full: &Column, sel: &Column, mask: &Bitmap) {
        let full_flat = full.materialize(None).unwrap();
        let sel_flat = sel.materialize(None).unwrap();
        let orig_flat = col.materialize(None).unwrap();
        assert!(full_flat.semantic_eq(&orig_flat));
        for row in mask.iter_ones() {
            assert_eq!(sel_flat.is_null(row), full_flat.is_null(row), "row {row}");
            if !full_flat.is_null(row) {
                assert_eq!(
                    sel_flat.get_scalar(row).unwrap(),
                    full_flat.get_scalar(row).unwrap(),
                    "row {row}"
                );
            }
        }
    }

    #[test]
    fn sparse_decode_mini_block_selected_plain_and_nested_parity() {
        let n = 128usize;
        let active_rows = [3usize, 27, 64, 120];
        let mut mask = Bitmap::new_zeroed(n);
        for &r in &active_rows {
            let _ = mask.set(r, true);
        }
        assert!(mask.count_ones() * 16 < n);

        // 1. Plain Utf8 (L0Fast) with nulls.
        let mut utf8_strs = Vec::with_capacity(n);
        let mut utf8_valid = Vec::with_capacity(n);
        for i in 0..n {
            if i % 7 == 0 {
                utf8_strs.push(String::new());
                utf8_valid.push(false);
            } else {
                utf8_strs.push(format!("span-name-{i:04}-abcdef"));
                utf8_valid.push(true);
            }
        }
        let utf8_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(utf8_strs.iter().map(String::as_str)).unwrap(),
                ),
                Some(Bitmap::from_bools(&utf8_valid)),
            )
            .unwrap(),
        );
        let enc_u =
            encode_mini_block(&utf8_col, &DataType::Utf8, WriteProfile::L0Fast, None).unwrap();
        assert_eq!(enc_u.encoding, Encoding::Plain);
        let (entry_u, raw_u) = frame_raw_body(&enc_u, n);
        let full_u = decode_mini_block(&raw_u, &entry_u, &DataType::Utf8, None).unwrap();
        let sel_u =
            decode_mini_block_selected(&raw_u, &entry_u, &DataType::Utf8, None, &mask).unwrap();
        assert_sparse_scalar_parity(&utf8_col, &full_u, &sel_u, &mask);

        // 2. Plain Binary (L0Fast).
        let bin_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Binary(
                    BinaryData::try_from_slices(utf8_strs.iter().map(|s| s.as_bytes())).unwrap(),
                ),
                Some(Bitmap::from_bools(&utf8_valid)),
            )
            .unwrap(),
        );
        let enc_b =
            encode_mini_block(&bin_col, &DataType::Binary, WriteProfile::L0Fast, None).unwrap();
        assert_eq!(enc_b.encoding, Encoding::Plain);
        let (entry_b, raw_b) = frame_raw_body(&enc_b, n);
        let full_b = decode_mini_block(&raw_b, &entry_b, &DataType::Binary, None).unwrap();
        let sel_b =
            decode_mini_block_selected(&raw_b, &entry_b, &DataType::Binary, None, &mask).unwrap();
        assert_sparse_scalar_parity(&bin_col, &full_b, &sel_b, &mask);

        // 3. Plain MapUtf8 (L0Fast).
        let mut map_offs = Vec::with_capacity(n + 1);
        map_offs.push(0u32);
        let mut m_keys = Vec::new();
        let mut m_vals = Vec::new();
        let mut m_val_valid = Vec::new();
        for i in 0..n {
            if i % 5 != 0 {
                m_keys.push("env".to_owned());
                m_vals.push(if i % 2 == 0 {
                    "prod".to_owned()
                } else {
                    "staging".to_owned()
                });
                m_val_valid.push(i % 11 != 0);
                m_keys.push(format!("k{i}"));
                m_vals.push(format!("v{i}"));
                m_val_valid.push(true);
            }
            map_offs.push(m_keys.len() as u32);
        }
        let map_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::MapUtf8(Box::new(
                    MapData::try_new(
                        map_offs,
                        Utf8Data::try_from_slices(m_keys.iter().map(String::as_str)).unwrap(),
                        Utf8Data::try_from_slices(m_vals.iter().map(String::as_str)).unwrap(),
                        Some(Bitmap::from_bools(&m_val_valid)),
                    )
                    .unwrap(),
                )),
                Some(Bitmap::from_bools(&utf8_valid)),
            )
            .unwrap(),
        );
        let enc_m =
            encode_mini_block(&map_col, &DataType::MapUtf8, WriteProfile::L0Fast, None).unwrap();
        assert_eq!(enc_m.encoding, Encoding::Plain);
        let (entry_m, raw_m) = frame_raw_body(&enc_m, n);
        let full_m = decode_mini_block(&raw_m, &entry_m, &DataType::MapUtf8, None).unwrap();
        let sel_m =
            decode_mini_block_selected(&raw_m, &entry_m, &DataType::MapUtf8, None, &mask).unwrap();
        assert_sparse_scalar_parity(&map_col, &full_m, &sel_m, &mask);

        // 4. List(Utf8) in both L0Fast (Plain) and L1Compact (Dict/Fsst), plus 0-active-item mask.
        let mut list_offs = Vec::with_capacity(n + 1);
        list_offs.push(0u32);
        let mut l_items = Vec::new();
        for i in 0..n {
            if !active_rows.contains(&i) || i == 64 {
                l_items.push(if i % 2 == 0 {
                    "tag_alpha".to_owned()
                } else {
                    "tag_beta".to_owned()
                });
                l_items.push(format!("item_{i:03}_payload"));
            }
            list_offs.push(l_items.len() as u32);
        }
        let list_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: list_offs,
                    values: Box::new(Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(
                                Utf8Data::try_from_slices(l_items.iter().map(String::as_str))
                                    .unwrap(),
                            ),
                            None,
                        )
                        .unwrap(),
                    )),
                },
                None,
            )
            .unwrap(),
        );
        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let enc_l = encode_mini_block(
                &list_col,
                &DataType::List(Box::new(DataType::Utf8)),
                profile,
                None,
            )
            .unwrap();
            let (entry_l, raw_l) = frame_raw_body(&enc_l, n);
            let full_l = decode_mini_block(
                &raw_l,
                &entry_l,
                &DataType::List(Box::new(DataType::Utf8)),
                None,
            )
            .unwrap();
            let sel_l = decode_mini_block_selected(
                &raw_l,
                &entry_l,
                &DataType::List(Box::new(DataType::Utf8)),
                None,
                &mask,
            )
            .unwrap();
            assert_sparse_scalar_parity(&list_col, &full_l, &sel_l, &mask);
        }

        // 0 active child items fast path (only rows 3 & 27 have 0 items).
        let mut zero_list_mask = Bitmap::new_zeroed(n);
        let _ = zero_list_mask.set(3, true);
        let _ = zero_list_mask.set(27, true);
        let enc_l = encode_mini_block(
            &list_col,
            &DataType::List(Box::new(DataType::Utf8)),
            WriteProfile::L1Compact,
            None,
        )
        .unwrap();
        let (entry_l, raw_l) = frame_raw_body(&enc_l, n);
        let full_l = decode_mini_block(
            &raw_l,
            &entry_l,
            &DataType::List(Box::new(DataType::Utf8)),
            None,
        )
        .unwrap();
        let sel_zero_l = decode_mini_block_selected(
            &raw_l,
            &entry_l,
            &DataType::List(Box::new(DataType::Utf8)),
            None,
            &zero_list_mask,
        )
        .unwrap();
        assert_sparse_scalar_parity(&list_col, &full_l, &sel_zero_l, &zero_list_mask);

        // 5. EventsStruct in L0Fast and L1Compact (including 0-active-events mask).
        let mut ev_offs = Vec::with_capacity(n + 1);
        ev_offs.push(0u32);
        let mut ev_times = Vec::new();
        let mut ev_names = Vec::new();
        let mut ev_attrs = Vec::new();
        let mut ev_anulls = Vec::new();
        for i in 0..n {
            if i != 3 && i != 27 {
                ev_times.push(1_700_000_000_000 + i as i64 * 10);
                ev_names.push(if i % 2 == 0 { "llm.token" } else { "tool.call" });
                ev_attrs.push(format!("{{\"idx\":{i},\"status\":\"ok\"}}"));
                ev_anulls.push(i % 4 != 0);
            }
            ev_offs.push(ev_times.len() as u32);
        }
        let ev_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(
                        ev_offs,
                        ev_times,
                        Utf8Data::try_from_slices(ev_names).unwrap(),
                        Utf8Data::try_from_slices(ev_attrs.iter().map(String::as_str)).unwrap(),
                        Some(Bitmap::from_bools(&ev_anulls)),
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        );
        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let enc_e = encode_mini_block(&ev_col, &DataType::Events, profile, None).unwrap();
            let (entry_e, raw_e) = frame_raw_body(&enc_e, n);
            let full_e = decode_mini_block(&raw_e, &entry_e, &DataType::Events, None).unwrap();
            let sel_e =
                decode_mini_block_selected(&raw_e, &entry_e, &DataType::Events, None, &mask)
                    .unwrap();
            assert_sparse_scalar_parity(&ev_col, &full_e, &sel_e, &mask);

            let sel_zero_e = decode_mini_block_selected(
                &raw_e,
                &entry_e,
                &DataType::Events,
                None,
                &zero_list_mask,
            )
            .unwrap();
            assert_sparse_scalar_parity(&ev_col, &full_e, &sel_zero_e, &zero_list_mask);
        }

        // 6. LinksStruct in L0Fast and L1Compact (including 0-active-links mask).
        let mut lk_offs = Vec::with_capacity(n + 1);
        lk_offs.push(0u32);
        let mut lk_tids = Vec::new();
        let mut lk_sids = Vec::new();
        let mut lk_attrs = Vec::new();
        let mut lk_anulls = Vec::new();
        for i in 0..n {
            if i != 3 && i != 27 {
                lk_tids.push([i as u8; 16]);
                lk_sids.push([i as u8; 8]);
                lk_attrs.push(format!("{{\"link\":{i},\"kind\":\"follows_from\"}}"));
                lk_anulls.push(i % 3 != 0);
            }
            lk_offs.push(lk_tids.len() as u32);
        }
        let lk_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(
                        lk_offs,
                        lk_tids,
                        lk_sids,
                        Utf8Data::try_from_slices(lk_attrs.iter().map(String::as_str)).unwrap(),
                        Some(Bitmap::from_bools(&lk_anulls)),
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        );
        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let enc_k = encode_mini_block(&lk_col, &DataType::Links, profile, None).unwrap();
            let (entry_k, raw_k) = frame_raw_body(&enc_k, n);
            let full_k = decode_mini_block(&raw_k, &entry_k, &DataType::Links, None).unwrap();
            let sel_k = decode_mini_block_selected(&raw_k, &entry_k, &DataType::Links, None, &mask)
                .unwrap();
            assert_sparse_scalar_parity(&lk_col, &full_k, &sel_k, &mask);

            let sel_zero_k = decode_mini_block_selected(
                &raw_k,
                &entry_k,
                &DataType::Links,
                None,
                &zero_list_mask,
            )
            .unwrap();
            assert_sparse_scalar_parity(&lk_col, &full_k, &sel_zero_k, &zero_list_mask);
        }
    }

    #[test]
    fn sparse_shredded_subcol_residual_and_unshred_selected_parity() {
        let n = 128usize;
        let active_rows = [2usize, 31, 95];
        let mut mask = Bitmap::new_zeroed(n);
        for &r in &active_rows {
            let _ = mask.set(r, true);
        }

        let mut offs = Vec::with_capacity(n + 1);
        offs.push(0u32);
        let mut keys = Vec::new();
        let mut vals = Vec::new();
        let mut val_valid = Vec::new();
        let mut row_valid = Vec::with_capacity(n);
        for i in 0..n {
            if i == 95 {
                row_valid.push(false);
                offs.push(keys.len() as u32);
                continue;
            }
            row_valid.push(true);
            keys.push("env".to_owned());
            vals.push(if i % 2 == 0 {
                "prod".to_owned()
            } else {
                "dev".to_owned()
            });
            val_valid.push(true);

            keys.push("model".to_owned());
            vals.push(format!("gpt-4o-mini-variant-{i:03}-long-prefix-for-fsst"));
            val_valid.push(i != 31);

            if i % 3 == 0 || i == 2 || i == 31 {
                keys.push(format!("rare_{i}"));
                vals.push(format!("rare_value_{i:03}_abcdefghijklmnopqrstuvwxyz"));
                val_valid.push(true);
            }
            offs.push(keys.len() as u32);
        }
        let map_col = FlatColumn::try_new(
            FlatData::MapUtf8(Box::new(
                MapData::try_new(
                    offs,
                    Utf8Data::try_from_slices(keys.iter().map(String::as_str)).unwrap(),
                    Utf8Data::try_from_slices(vals.iter().map(String::as_str)).unwrap(),
                    Some(Bitmap::from_bools(&val_valid)),
                )
                .unwrap(),
            )),
            Some(Bitmap::from_bools(&row_valid)),
        )
        .unwrap();

        let promoted = vec!["env".to_owned(), "model".to_owned()];
        let shredded = shred_map(&map_col, &promoted).unwrap();

        let sub0_bytes =
            encode_shredded_subcol(&shredded.sub_cols[0], WriteProfile::L1Compact).unwrap();
        let sub1_bytes =
            encode_shredded_subcol(&shredded.sub_cols[1], WriteProfile::L1Compact).unwrap();
        let sub0_full = decode_shredded_subcol(&sub0_bytes, n).unwrap();
        let sub1_full = decode_shredded_subcol(&sub1_bytes, n).unwrap();
        let sub0_sel = decode_shredded_subcol_selected(&sub0_bytes, n, &mask).unwrap();
        let sub1_sel = decode_shredded_subcol_selected(&sub1_bytes, n, &mask).unwrap();

        let res_enc =
            encode_shredded_residual_block(&shredded.residual, WriteProfile::L1Compact).unwrap();
        let (res_entry, res_raw) = frame_raw_body(&res_enc, n);
        let res_full = decode_shredded_residual_block(&res_raw, &res_entry).unwrap();
        let res_sel = decode_shredded_residual_block_selected(&res_raw, &res_entry, &mask).unwrap();

        let full_unshred = unshred_map(
            n,
            res_full.nulls(),
            Some(&res_full),
            &[("env", &sub0_full), ("model", &sub1_full)],
        )
        .unwrap();
        assert!(full_unshred.semantic_eq(&map_col));

        let sel_unshred = unshred_map_selected(
            n,
            res_sel.nulls(),
            Some(&res_sel),
            &[("env", &sub0_sel), ("model", &sub1_sel)],
            &mask,
        )
        .unwrap();
        for &r in &active_rows {
            assert_eq!(sel_unshred.is_null(r), full_unshred.is_null(r), "row {r}");
            if !full_unshred.is_null(r) {
                assert_eq!(
                    sel_unshred.get_scalar(r).unwrap(),
                    full_unshred.get_scalar(r).unwrap(),
                    "row {r}"
                );
            }
        }

        // Also exercise Rle and Const sub-columns inside unshred_map_selected.
        let rle_sub = ShreddedSubCol {
            in_map: Bitmap::new_ones(n),
            value_null: Bitmap::new_ones(n),
            values: Column::Rle(
                RleColumn::try_new(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["r0", "r1"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                    vec![64, n as u32],
                )
                .unwrap(),
            ),
        };
        let const_sub = ShreddedSubCol {
            in_map: Bitmap::new_ones(n),
            value_null: Bitmap::new_ones(n),
            values: Column::Const {
                value: ScalarValue::Utf8("const_val".to_owned()),
                len: n,
            },
        };
        let rle_full = unshred_map(n, None, None, &[("a", &const_sub), ("b", &rle_sub)]).unwrap();
        let rle_sel =
            unshred_map_selected(n, None, None, &[("a", &const_sub), ("b", &rle_sub)], &mask)
                .unwrap();
        for &r in &active_rows {
            assert_eq!(
                rle_sel.get_scalar(r).unwrap(),
                rle_full.get_scalar(r).unwrap()
            );
        }

        // Validation: mismatched mask length must return Err(Corrupt).
        let bad_mask = Bitmap::new_zeroed(n - 1);
        assert!(matches!(
            decode_shredded_subcol_selected(&sub0_bytes, n, &bad_mask),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_shredded_residual_block_selected(&res_raw, &res_entry, &bad_mask),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            unshred_map_selected(n, None, None, &[("a", &const_sub)], &bad_mask),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn sparse_decode_mini_block_selected_dict_and_rle_parity() {
        let n = 512usize;
        let empty_mask = Bitmap::new_zeroed(n);
        let mut one_mask = Bitmap::new_zeroed(n);
        let _ = one_mask.set(173, true);
        let mut scattered_mask = Bitmap::new_zeroed(n);
        for idx in [0usize, 17, 128, 333, 511] {
            let _ = scattered_mask.set(idx, true);
        }
        let all_mask = Bitmap::new_ones(n);

        // 1. Dict (Utf8) with large dictionary (> 256 KiB) and nulls.
        let pad = "d".repeat(4096);
        let dict_vals: Vec<String> = (0..70).map(|k| format!("dict-val-{k:03}-{pad}")).collect();
        let big_dict = FileDict::from_flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(dict_vals.iter().map(String::as_str)).unwrap(),
                ),
                None,
            )
            .unwrap(),
            true,
        )
        .unwrap();
        assert!(big_dict.memory_bytes() > DICT_PASSTHROUGH_MAX_BYTES);

        let mut dict_rows = Vec::with_capacity(n);
        let mut dict_valid = Vec::with_capacity(n);
        for i in 0..n {
            if i % 9 == 0 {
                dict_rows.push(String::new());
                dict_valid.push(false);
            } else {
                dict_rows.push(dict_vals[i % dict_vals.len()].clone());
                dict_valid.push(true);
            }
        }
        let dict_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(dict_rows.iter().map(String::as_str)).unwrap(),
                ),
                Some(Bitmap::from_bools(&dict_valid)),
            )
            .unwrap(),
        );
        let enc_dict = encode_mini_block(
            &dict_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            Some(&big_dict),
        )
        .unwrap();
        assert_eq!(enc_dict.encoding, Encoding::Dict);
        let (entry_dict, raw_dict) = frame_raw_body(&enc_dict, n);
        let full_dict =
            decode_mini_block(&raw_dict, &entry_dict, &DataType::Utf8, Some(&big_dict)).unwrap();
        for mask in [&empty_mask, &one_mask, &scattered_mask, &all_mask] {
            let sel = decode_mini_block_selected(
                &raw_dict,
                &entry_dict,
                &DataType::Utf8,
                Some(&big_dict),
                mask,
            )
            .unwrap();
            assert_sparse_scalar_parity(&dict_col, &full_dict, &sel, mask);
        }

        // 2. Rle(Utf8) with Plain inner (L0Fast) and Fsst inner (L1Compact), including null runs.
        let num_runs = 64usize;
        let run_len = (n / num_runs) as u32;
        let mut run_strs = Vec::with_capacity(num_runs);
        let mut run_valid = Vec::with_capacity(num_runs);
        let mut run_ends = Vec::with_capacity(num_runs);
        for r in 0..num_runs {
            run_ends.push(((r + 1) as u32) * run_len);
            if r % 8 == 0 {
                run_strs.push(String::new());
                run_valid.push(false);
            } else {
                run_strs.push(format!(
                    "{{\"run\":{r},\"provider\":\"anthropic\",\"model\":\"claude-3-5-sonnet\",\"tag\":\"rle-{r:03}\"}}"
                ));
                run_valid.push(true);
            }
        }
        let rle_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(run_strs.iter().map(String::as_str)).unwrap()),
            Some(Bitmap::from_bools(&run_valid)),
        )
        .unwrap();
        let rle_col = Column::Rle(RleColumn::try_new(rle_vals, run_ends).unwrap());
        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let enc_rle = encode_mini_block(&rle_col, &DataType::Utf8, profile, None).unwrap();
            assert_eq!(enc_rle.encoding, Encoding::Rle);
            let (entry_rle, raw_rle) = frame_raw_body(&enc_rle, n);
            let full_rle = decode_mini_block(&raw_rle, &entry_rle, &DataType::Utf8, None).unwrap();
            for mask in [&empty_mask, &one_mask, &scattered_mask, &all_mask] {
                let sel =
                    decode_mini_block_selected(&raw_rle, &entry_rle, &DataType::Utf8, None, mask)
                        .unwrap();
                assert_sparse_scalar_parity(&rle_col, &full_rle, &sel, mask);
            }
        }
    }

    #[test]
    fn decode_rle_block_selected_empty_mask_validates_inner_enc_and_body() {
        let row_count = 32usize;
        let empty_mask = Bitmap::new_zeroed(row_count);
        let run_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta", "gamma", "delta"]).unwrap()),
            None,
        )
        .unwrap();
        let rle_col = Column::Rle(RleColumn::try_new(run_vals, vec![8, 16, 24, 32]).unwrap());
        let enc_rle =
            encode_mini_block(&rle_col, &DataType::Utf8, WriteProfile::L0Fast, None).unwrap();
        let mut framed = Vec::new();
        let entry = enc_rle
            .write_framed_entry(0, row_count as u16, 0, 3, &mut framed)
            .unwrap();
        let (_, _, raw) = read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap();

        // Find the inner_enc byte position in raw: 4 (num_runs) + 1 (re_bw) + packed_bytes(4, re_bw).
        let re_bw = raw[4];
        let re_bytes = packed_bytes_len(4, re_bw).unwrap();
        let inner_enc_pos = 4 + 1 + re_bytes;

        // 1. Incompatible inner_enc for Utf8 must be rejected even with empty mask.
        let mut bad_enc = raw.clone();
        bad_enc[inner_enc_pos] = Encoding::Alp as u8;
        assert!(matches!(
            decode_mini_block_selected(&bad_enc, &entry, &DataType::Utf8, None, &empty_mask),
            Err(StatamError::Corrupt { .. })
        ));

        // 2. Truncated inner_body must be rejected even with empty mask.
        let truncated = &raw[..inner_enc_pos + 3];
        assert!(matches!(
            decode_mini_block_selected(truncated, &entry, &DataType::Utf8, None, &empty_mask),
            Err(StatamError::Corrupt { .. })
        ));
    }

    fn assert_selected_and_take_parity_raw(
        col: &Column,
        dt: &DataType,
        dict: Option<&FileDict>,
        entry: &BlockEntry,
        raw: &[u8],
        sparse_indices: &[usize],
        dense_indices: &[usize],
    ) {
        let n = col.len();
        let full = decode_mini_block(raw, entry, dt, dict).unwrap();
        let full_flat = full.materialize(None).unwrap();

        for indices in [&[][..], sparse_indices, dense_indices] {
            let mut mask = Bitmap::new_zeroed(n);
            for &r in indices {
                let _ = mask.set(r, true);
            }
            let sel = decode_mini_block_selected(raw, entry, dt, dict, &mask).unwrap();
            assert_sparse_scalar_parity(col, &full, &sel, &mask);

            let taken = decode_mini_block_take(raw, entry, dt, dict, indices).unwrap();
            assert_eq!(taken.len(), indices.len(), "compact take len mismatch");
            let taken_flat = taken.materialize(None).unwrap();
            for (out_pos, &row) in indices.iter().enumerate() {
                assert_eq!(
                    taken_flat.is_null(out_pos),
                    full_flat.is_null(row),
                    "take null mismatch at row {row} ({dt:?})"
                );
                if !full_flat.is_null(row) {
                    assert_eq!(
                        taken_flat.get_scalar(out_pos).unwrap(),
                        full_flat.get_scalar(row).unwrap(),
                        "take scalar mismatch at row {row} ({dt:?})"
                    );
                }
            }
        }

        // Corrupt-input rejection: truncated body and out-of-bounds take index.
        if !raw.is_empty() {
            let mut empty_mask = Bitmap::new_zeroed(n);
            let _ = empty_mask.set(0, false);
            assert!(
                decode_mini_block_selected(&raw[..raw.len() - 1], entry, dt, dict, &empty_mask)
                    .is_err()
            );
            assert!(decode_mini_block_take(&raw[..raw.len() - 1], entry, dt, dict, &[0]).is_err());
        }
        assert!(decode_mini_block_take(raw, entry, dt, dict, &[n]).is_err());
    }

    fn assert_selected_and_take_parity(
        col: &Column,
        dt: &DataType,
        profile: WriteProfile,
        dict: Option<&FileDict>,
        expected_enc: Option<Encoding>,
        sparse_indices: &[usize],
        dense_indices: &[usize],
    ) {
        let n = col.len();
        let enc = encode_mini_block(col, dt, profile, dict).unwrap();
        if let Some(exp) = expected_enc {
            assert_eq!(
                enc.encoding, exp,
                "unexpected encoding for {dt:?} ({profile:?})"
            );
        }
        let (entry, raw) = frame_raw_body(&enc, n);
        assert_selected_and_take_parity_raw(
            col,
            dt,
            dict,
            &entry,
            &raw,
            sparse_indices,
            dense_indices,
        );
    }

    fn assert_plain_selected_and_take_parity(
        flat: &FlatColumn,
        dt: &DataType,
        sparse_indices: &[usize],
        dense_indices: &[usize],
    ) {
        let n = flat.len();
        let raw = numeric::prepend_optional_nulls(
            flat.nulls(),
            numeric::encode_plain_body(flat).unwrap(),
        );
        let null_count = flat.nulls().map_or(0, |bm| (n - bm.count_ones()) as u16);
        let entry = BlockEntry {
            first_row: 0,
            row_count: n as u16,
            null_count,
            offset: 0,
            stored_len: raw.len() as u32,
            raw_len: raw.len() as u32,
            encoding: Encoding::Plain,
            codec: Codec::None,
            block_flags: if null_count > 0 {
                BLOCK_FLAG_HAS_NULLS
            } else {
                0
            },
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        let col = Column::Flat(flat.clone());
        assert_selected_and_take_parity_raw(
            &col,
            dt,
            None,
            &entry,
            &raw,
            sparse_indices,
            dense_indices,
        );
    }

    #[test]
    fn sparse_selected_and_compact_take_parity_all_numeric_fsst_and_dict() {
        let n = 1300usize; // > 1024 so both FastLanes tile and scalar tail are covered
        let sparse = [0usize, 5, 1023, 1024, 1025, n - 1];
        let dense: Vec<usize> = (0..n).step_by(3).collect();

        let nulls = {
            let mut v = Vec::with_capacity(n);
            for i in 0..n {
                v.push(i % 9 != 0);
            }
            Bitmap::from_bools(&v)
        };

        for with_nulls in [false, true] {
            let opt_nulls = with_nulls.then(|| nulls.clone());

            // 1. Plain & BitpackBool (Bool)
            let bool_bm = Bitmap::from_bools(&(0..n).map(|i| (i * 7) % 3 == 0).collect::<Vec<_>>());
            let bool_flat =
                FlatColumn::try_new(FlatData::Bool(bool_bm), opt_nulls.clone()).unwrap();
            assert_plain_selected_and_take_parity(&bool_flat, &DataType::Bool, &sparse, &dense);
            let bool_col = Column::Flat(bool_flat);
            assert_selected_and_take_parity(
                &bool_col,
                &DataType::Bool,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::BitpackBool),
                &sparse,
                &dense,
            );

            // 2. Plain fixed-width: I64, TimestampNs, F64, Date, Decimal128, Interval, FixedBytes8, FixedBytes16
            let plain_i64_vals: Vec<i64> = (0..n)
                .map(|i| {
                    if i % 2 == 0 {
                        i64::MIN + (i as i64) * 1_000_003
                    } else {
                        i64::MAX - (i as i64) * 1_000_007
                    }
                })
                .collect();
            let plain_i64_flat =
                FlatColumn::try_new(FlatData::I64(plain_i64_vals), opt_nulls.clone()).unwrap();
            assert_plain_selected_and_take_parity(&plain_i64_flat, &DataType::I64, &sparse, &dense);

            let ts_dt = DataType::TimestampNs { utc: true };
            let plain_ts_vals: Vec<i64> = (0..n)
                .map(|i| {
                    if i % 2 == 0 {
                        i64::MIN + (i as i64) * 10_000_000_000_000
                    } else {
                        i64::MAX - (i as i64) * 10_000_000_000_003
                    }
                })
                .collect();
            let plain_ts_flat = FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: plain_ts_vals,
                    utc: true,
                },
                opt_nulls.clone(),
            )
            .unwrap();
            assert_plain_selected_and_take_parity(&plain_ts_flat, &ts_dt, &sparse, &dense);

            let f64_vals: Vec<f64> = (0..n)
                .map(|i| ((i * 97 + 13) as f64) * 0.123456789)
                .collect();
            let f64_col = Column::Flat(
                FlatColumn::try_new(FlatData::F64(f64_vals), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &f64_col,
                &DataType::F64,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::Plain),
                &sparse,
                &dense,
            );

            // ALP (L1Compact): decimal costs + adversarial IEEE-754 exceptions across tile & tail
            let mut alp_vals: Vec<f64> = (0..n).map(|i| ((i % 97) as f64) * 0.000_025).collect();
            alp_vals[1] = -0.0;
            alp_vals[5] = f64::NEG_INFINITY;
            alp_vals[1023] = f64::INFINITY;
            alp_vals[1024] = 1e-310;
            alp_vals[n - 1] = std::f64::consts::PI;
            let alp_col = Column::Flat(
                FlatColumn::try_new(FlatData::F64(alp_vals), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &alp_col,
                &DataType::F64,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::Alp),
                &sparse,
                &dense,
            );

            let plain_date_vals: Vec<i32> = (0..n)
                .map(|i| {
                    if i % 2 == 0 {
                        i32::MIN + (i as i32) * 997
                    } else {
                        i32::MAX - (i as i32) * 991
                    }
                })
                .collect();
            let plain_date_flat =
                FlatColumn::try_new(FlatData::Date(plain_date_vals), opt_nulls.clone()).unwrap();
            assert_plain_selected_and_take_parity(
                &plain_date_flat,
                &DataType::Date,
                &sparse,
                &dense,
            );

            let dec_dt = DataType::Decimal128 {
                precision: 38,
                scale: 6,
            };
            let plain_dec_vals: Vec<i128> = (0..n)
                .map(|i| {
                    if i % 2 == 0 {
                        -10_000_000_000_000_000_000_000_000i128 + (i as i128) * 123_456_789
                    } else {
                        10_000_000_000_000_000_000_000_000i128 - (i as i128) * 987_654_321
                    }
                })
                .collect();
            let plain_dec_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: plain_dec_vals,
                        precision: 38,
                        scale: 6,
                    },
                    opt_nulls.clone(),
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &plain_dec_col,
                &dec_dt,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::Plain),
                &sparse,
                &dense,
            );

            let iv_vals: Vec<Interval> = (0..n)
                .map(|i| Interval {
                    months: (i % 12) as i32,
                    days: (i % 30) as i32,
                    nanos: (i as i64) * 1000,
                })
                .collect();
            let iv_col = Column::Flat(
                FlatColumn::try_new(FlatData::Interval(iv_vals), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &iv_col,
                &DataType::Interval,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::Plain),
                &sparse,
                &dense,
            );

            let f8_vals: Vec<[u8; 8]> = (0..n).map(|i| (i as u64).to_le_bytes()).collect();
            let f8_col = Column::Flat(
                FlatColumn::try_new(FlatData::FixedBytes8(f8_vals), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &f8_col,
                &DataType::FixedBytes8,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::Plain),
                &sparse,
                &dense,
            );

            let f16_vals: Vec<[u8; 16]> = (0..n).map(|i| (i as u128).to_le_bytes()).collect();
            let f16_col = Column::Flat(
                FlatColumn::try_new(FlatData::FixedBytes16(f16_vals), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &f16_col,
                &DataType::FixedBytes16,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::Plain),
                &sparse,
                &dense,
            );

            // 3. ForBitpack (L1Compact): I64, TimestampNs, Date, Decimal128
            let i64_vals: Vec<i64> = (0..n).map(|i| ((i * 7919) % 10_000) as i64 - 500).collect();
            let i64_col = Column::Flat(
                FlatColumn::try_new(FlatData::I64(i64_vals), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &i64_col,
                &DataType::I64,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::ForBitpack),
                &sparse,
                &dense,
            );
            let ts_for_vals: Vec<i64> = (0..n)
                .map(|i| 1_700_000_000_000 + (((i * 7919) % 10_000) as i64))
                .collect();
            let ts_for_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts_for_vals,
                        utc: false,
                    },
                    opt_nulls.clone(),
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &ts_for_col,
                &DataType::TimestampNs { utc: false },
                WriteProfile::L1Compact,
                None,
                Some(Encoding::ForBitpack),
                &sparse,
                &dense,
            );
            let date_vals: Vec<i32> = (0..n).map(|i| 19000 + ((i * 313) % 500) as i32).collect();
            let date_col = Column::Flat(
                FlatColumn::try_new(FlatData::Date(date_vals), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &date_col,
                &DataType::Date,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::ForBitpack),
                &sparse,
                &dense,
            );
            let dec_vals: Vec<i128> = (0..n)
                .map(|i| 1_000_000_000_000i128 + ((i * 997) % 50_000) as i128)
                .collect();
            let dec_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: dec_vals,
                        precision: 38,
                        scale: 6,
                    },
                    opt_nulls.clone(),
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &dec_col,
                &dec_dt,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::ForBitpack),
                &sparse,
                &dense,
            );

            // 4. DeltaBitpack (L1Compact): monotonic I64, TimestampNs, Date
            let mono_i64: Vec<i64> = (0..n).map(|i| 1_000_000_000 + (i as i64) * 17).collect();
            let mono_i64_col = Column::Flat(
                FlatColumn::try_new(FlatData::I64(mono_i64), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &mono_i64_col,
                &DataType::I64,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::DeltaBitpack),
                &sparse,
                &dense,
            );
            let ts_vals: Vec<i64> = (0..n)
                .map(|i| 1_700_000_000_000 + (i as i64) * 1_000_003)
                .collect();
            let ts_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: ts_vals,
                        utc: true,
                    },
                    opt_nulls.clone(),
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &ts_col,
                &ts_dt,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::DeltaBitpack),
                &sparse,
                &dense,
            );
            let mono_date: Vec<i32> = (0..n).map(|i| 19000 + (i / 2) as i32).collect();
            let mono_date_col = Column::Flat(
                FlatColumn::try_new(FlatData::Date(mono_date), opt_nulls.clone()).unwrap(),
            );
            assert_selected_and_take_parity(
                &mono_date_col,
                &DataType::Date,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::DeltaBitpack),
                &sparse,
                &dense,
            );

            // 5. Plain & FSST (Utf8 & Binary)
            let strs: Vec<String> = (0..n)
                .map(|i| {
                    format!(
                        "{{\"service\":\"checkout-api\",\"trace\":\"{:08x}\",\"idx\":{i}}}",
                        i * 131
                    )
                })
                .collect();
            let utf8_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(strs.iter().map(String::as_str)).unwrap(),
                    ),
                    opt_nulls.clone(),
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &utf8_col,
                &DataType::Utf8,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::Plain),
                &sparse,
                &dense,
            );
            assert_selected_and_take_parity(
                &utf8_col,
                &DataType::Utf8,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::Fsst),
                &sparse,
                &dense,
            );

            let bin_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Binary(
                        BinaryData::try_from_slices(strs.iter().map(String::as_bytes)).unwrap(),
                    ),
                    opt_nulls.clone(),
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &bin_col,
                &DataType::Binary,
                WriteProfile::L0Fast,
                None,
                Some(Encoding::Plain),
                &sparse,
                &dense,
            );
            assert_selected_and_take_parity(
                &bin_col,
                &DataType::Binary,
                WriteProfile::L1Compact,
                None,
                Some(Encoding::Fsst),
                &sparse,
                &dense,
            );

            // 6. Dict (both ForBitpack & Rle sub-encodings)
            let dict_vals: Vec<String> = (0..12).map(|k| format!("service-{k:02}")).collect();
            let file_dict = FileDict::from_flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(dict_vals.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
                true,
            )
            .unwrap();
            let fb_strs: Vec<&str> = (0..n).map(|i| dict_vals[i % 12].as_str()).collect();
            let fb_dict_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(fb_strs).unwrap()),
                    opt_nulls.clone(),
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &fb_dict_col,
                &DataType::Utf8,
                WriteProfile::L1Compact,
                Some(&file_dict),
                Some(Encoding::Dict),
                &sparse,
                &dense,
            );

            let rle_strs: Vec<&str> = (0..n).map(|i| dict_vals[(i / 128) % 12].as_str()).collect();
            let rle_dict_col = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(rle_strs).unwrap()),
                    opt_nulls,
                )
                .unwrap(),
            );
            assert_selected_and_take_parity(
                &rle_dict_col,
                &DataType::Utf8,
                WriteProfile::L1Compact,
                Some(&file_dict),
                Some(Encoding::Dict),
                &sparse,
                &dense,
            );
        }
    }

    #[test]
    fn column_dict_value_space_zonemap_non_const_and_const_soundness() {
        // 1. Non-constant Column::Dict<Utf8> encoded with file_dict = None (narrow fallback):
        // dict values are ["zebra", "apple"], codes = [0, 1, 0, 1].
        // Value-space bounds MUST be ["apple", "zebra"], NOT u32 codes [0..1].
        let utf8_dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["zebra", "apple", "unused"]).unwrap()),
            None,
        )
        .unwrap();
        let utf8_dict_col =
            Column::Dict(DictColumn::try_new(utf8_dict_vals, vec![0, 1, 0, 1], None).unwrap());
        let (enc_u, entry_u, dec_u) =
            roundtrip_via_frame(&utf8_dict_col, &DataType::Utf8, WriteProfile::L0Fast, None);
        assert_ne!(enc_u.encoding, Encoding::Dict);
        assert!(dec_u.semantic_eq(&utf8_dict_col));
        assert!(zone_map_may_match(
            &entry_u,
            &DataType::Utf8,
            0,
            &ZonePred::Eq(ZoneValue::Bytes(b"apple".to_vec())),
            None,
        ));
        assert!(zone_map_may_match(
            &entry_u,
            &DataType::Utf8,
            0,
            &ZonePred::Eq(ZoneValue::Bytes(b"zebra".to_vec())),
            None,
        ));
        assert!(!zone_map_may_match(
            &entry_u,
            &DataType::Utf8,
            0,
            &ZonePred::Eq(ZoneValue::Bytes(b"aardvark".to_vec())),
            None,
        ));
        assert!(!zone_map_may_match(
            &entry_u,
            &DataType::Utf8,
            0,
            &ZonePred::Gt(ZoneValue::Bytes(b"zzz".to_vec())),
            None,
        ));
        assert!(zone_map_always_matches(
            &entry_u,
            &DataType::Utf8,
            &ZonePred::Between(
                ZoneValue::Bytes(b"apple".to_vec()),
                ZoneValue::Bytes(b"zebra".to_vec()),
            ),
        ));
        // Unused dict entry "unused" doesn't affect bounds, and false-positive tautology on code [0..1] is gone:
        assert!(!zone_map_always_matches(
            &entry_u,
            &DataType::Utf8,
            &ZonePred::Lt(ZoneValue::Bytes(b"banana".to_vec())),
        ));

        // 2. Non-constant Column::Dict<Date> with narrow fallback:
        let date_vals =
            FlatColumn::try_new(FlatData::Date(vec![20_000, 19_000, 99_999]), None).unwrap();
        let date_dict_col =
            Column::Dict(DictColumn::try_new(date_vals, vec![0, 1, 0, 1], None).unwrap());
        let (enc_d, entry_d, dec_d) =
            roundtrip_via_frame(&date_dict_col, &DataType::Date, WriteProfile::L0Fast, None);
        assert_ne!(enc_d.encoding, Encoding::Dict);
        assert!(dec_d.semantic_eq(&date_dict_col));
        assert!(zone_map_may_match(
            &entry_d,
            &DataType::Date,
            0,
            &ZonePred::Eq(ZoneValue::Date(19_500)),
            None,
        ));
        assert!(!zone_map_may_match(
            &entry_d,
            &DataType::Date,
            0,
            &ZonePred::Gt(ZoneValue::Date(20_000)),
            None,
        ));
        assert!(zone_map_always_matches(
            &entry_d,
            &DataType::Date,
            &ZonePred::Between(ZoneValue::Date(19_000), ZoneValue::Date(20_000)),
        ));

        // 3. Non-constant Column::Dict<Bool> with narrow fallback:
        let bool_vals =
            FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&[true, false])), None).unwrap();
        let bool_dict_col =
            Column::Dict(DictColumn::try_new(bool_vals, vec![0, 1, 0, 1], None).unwrap());
        let (_, entry_b, dec_b) =
            roundtrip_via_frame(&bool_dict_col, &DataType::Bool, WriteProfile::L0Fast, None);
        assert!(dec_b.semantic_eq(&bool_dict_col));
        assert!(zone_map_may_match(
            &entry_b,
            &DataType::Bool,
            0,
            &ZonePred::Eq(ZoneValue::Bool(true)),
            None,
        ));
        assert!(zone_map_may_match(
            &entry_b,
            &DataType::Bool,
            0,
            &ZonePred::Eq(ZoneValue::Bool(false)),
            None,
        ));
        assert!(!zone_map_always_matches(
            &entry_b,
            &DataType::Bool,
            &ZonePred::Eq(ZoneValue::Bool(true)),
        ));

        // 4. Non-constant Column::Dict<F64> containing NaN + -0.0:
        let f64_vals =
            FlatColumn::try_new(FlatData::F64(vec![-0.0, f64::NAN, 100.0]), None).unwrap();
        let f64_dict_col =
            Column::Dict(DictColumn::try_new(f64_vals, vec![0, 1, 0, 1], None).unwrap());
        let (_, entry_f, dec_f) =
            roundtrip_via_frame(&f64_dict_col, &DataType::F64, WriteProfile::L1Compact, None);
        assert!(dec_f.semantic_eq(&f64_dict_col));
        assert_ne!(entry_f.block_flags & BLOCK_FLAG_HAS_NAN, 0);
        assert_ne!(entry_f.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);
        // -0.0 canonicalized to +0.0, unused 100.0 excluded:
        assert_eq!(
            f64::from_le_bytes(entry_f.min_inline[0..8].try_into().unwrap()).to_bits(),
            0.0f64.to_bits()
        );
        assert_eq!(
            f64::from_le_bytes(entry_f.max_inline[0..8].try_into().unwrap()).to_bits(),
            0.0f64.to_bits()
        );
        assert!(zone_map_may_match(
            &entry_f,
            &DataType::F64,
            0,
            &ZonePred::Eq(ZoneValue::F64(f64::NAN)),
            None,
        ));
        assert!(zone_map_may_match(
            &entry_f,
            &DataType::F64,
            0,
            &ZonePred::Gt(ZoneValue::F64(10.0)),
            None,
        ));
        assert!(!zone_map_always_matches(
            &entry_f,
            &DataType::F64,
            &ZonePred::Eq(ZoneValue::F64(0.0)),
        ));

        // 5. Non-constant Column::Dict<Interval> clears MIN_MAX_VALID:
        let iv_vals = FlatColumn::try_new(
            FlatData::Interval(vec![
                Interval {
                    months: 1,
                    days: 2,
                    nanos: 3,
                },
                Interval {
                    months: 4,
                    days: 5,
                    nanos: 6,
                },
            ]),
            None,
        )
        .unwrap();
        let iv_dict_col =
            Column::Dict(DictColumn::try_new(iv_vals, vec![0, 1, 0, 1], None).unwrap());
        let (_, entry_iv, dec_iv) = roundtrip_via_frame(
            &iv_dict_col,
            &DataType::Interval,
            WriteProfile::L0Fast,
            None,
        );
        assert!(dec_iv.semantic_eq(&iv_dict_col));
        assert_eq!(entry_iv.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);

        // 6. Constant Column::Dict encoded as Encoding::Const for Date, Bool, F64(NaN), Utf8:
        let const_date_vals = FlatColumn::try_new(FlatData::Date(vec![10, 20_123]), None).unwrap();
        let const_date_dict =
            Column::Dict(DictColumn::try_new(const_date_vals, vec![1, 1, 1, 1], None).unwrap());
        let (enc_cd, entry_cd, dec_cd) = roundtrip_via_frame(
            &const_date_dict,
            &DataType::Date,
            WriteProfile::L0Fast,
            None,
        );
        assert_eq!(enc_cd.encoding, Encoding::Const);
        assert!(dec_cd.semantic_eq(&const_date_dict));
        assert!(zone_map_always_matches(
            &entry_cd,
            &DataType::Date,
            &ZonePred::Eq(ZoneValue::Date(20_123)),
        ));
        assert!(!zone_map_may_match(
            &entry_cd,
            &DataType::Date,
            0,
            &ZonePred::Eq(ZoneValue::Date(1)),
            None,
        ));

        let const_bool_vals =
            FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&[false, true])), None).unwrap();
        let const_bool_dict =
            Column::Dict(DictColumn::try_new(const_bool_vals, vec![1, 1, 1, 1], None).unwrap());
        let (enc_cb, entry_cb, dec_cb) = roundtrip_via_frame(
            &const_bool_dict,
            &DataType::Bool,
            WriteProfile::L0Fast,
            None,
        );
        assert_eq!(enc_cb.encoding, Encoding::Const);
        assert!(dec_cb.semantic_eq(&const_bool_dict));
        assert!(zone_map_always_matches(
            &entry_cb,
            &DataType::Bool,
            &ZonePred::Eq(ZoneValue::Bool(true)),
        ));
        assert!(!zone_map_may_match(
            &entry_cb,
            &DataType::Bool,
            0,
            &ZonePred::Eq(ZoneValue::Bool(false)),
            None,
        ));

        let const_nan_vals = FlatColumn::try_new(FlatData::F64(vec![1.0, f64::NAN]), None).unwrap();
        let const_nan_dict =
            Column::Dict(DictColumn::try_new(const_nan_vals, vec![1, 1, 1, 1], None).unwrap());
        let (enc_cn, entry_cn, dec_cn) =
            roundtrip_via_frame(&const_nan_dict, &DataType::F64, WriteProfile::L0Fast, None);
        assert_eq!(enc_cn.encoding, Encoding::Const);
        assert!(dec_cn.semantic_eq(&const_nan_dict));
        assert_ne!(entry_cn.block_flags & BLOCK_FLAG_HAS_NAN, 0);
        assert_eq!(entry_cn.block_flags & BLOCK_FLAG_MIN_MAX_VALID, 0);
        assert!(zone_map_may_match(
            &entry_cn,
            &DataType::F64,
            0,
            &ZonePred::Eq(ZoneValue::F64(f64::NAN)),
            None,
        ));
        assert!(!zone_map_always_matches(
            &entry_cn,
            &DataType::F64,
            &ZonePred::IsNotNull,
        ));
    }

    #[test]
    fn decode_mini_block_take_caps_amplification_via_reservation() {
        // 1-row Rle<Utf8> with a 1 MiB string; taking row 0 300 times would amplify to 300 MiB > 256 MiB.
        let big = "a".repeat(1024 * 1024);
        let run_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices([big.as_str(), "b"]).unwrap()),
            None,
        )
        .unwrap();
        let rle_col = Column::Rle(RleColumn::try_new(run_vals, vec![10, 20]).unwrap());
        let enc = encode_mini_block(&rle_col, &DataType::Utf8, WriteProfile::L0Fast, None).unwrap();
        assert_eq!(enc.encoding, Encoding::Rle);
        let mut framed = Vec::new();
        let entry = enc.write_framed_entry(0, 20, 0, 3, &mut framed).unwrap();
        let (_, _, raw_body) = read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap();

        let dup_indices = vec![0usize; 300];
        let err = decode_mini_block_take(&raw_body, &entry, &DataType::Utf8, None, &dup_indices);
        assert!(matches!(err, Err(StatamError::BoundsExceeded { .. })));
    }

    #[test]
    fn subchunked_fsst_nested_and_shredded_subcol_roundtrip_and_flag_validation() {
        let n = 1_200usize;
        let strings: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "https://observability.cetra.sh/api/v1/spans/trace_{i:06}/events/node_{:03}/req_{:08x}/meta_{:06x}",
                    i % 89,
                    (i as u32).wrapping_mul(2654435761),
                    (i as u32).wrapping_mul(2246822519) & 0x00FF_FFFF
                )
            })
            .collect();
        let str_refs: Vec<&str> = strings.iter().map(String::as_str).collect();
        let nulls = Bitmap::from_bools(&(0..n).map(|i| i % 23 != 0).collect::<Vec<_>>());

        // 1. Top-level narrow Utf8: sets BLOCK_FLAG_FSST_SUBCHUNKED and outer Codec::None.
        let utf8_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(str_refs.clone()).unwrap()),
                Some(nulls.clone()),
            )
            .unwrap(),
        );
        let (enc_u, entry_u, dec_u) =
            roundtrip_via_frame(&utf8_col, &DataType::Utf8, WriteProfile::L1Compact, None);
        assert_eq!(enc_u.encoding, Encoding::Fsst);
        assert_ne!(enc_u.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED, 0);
        assert_eq!(entry_u.codec, Codec::None);
        assert_eq!(entry_u.raw_len, entry_u.stored_len);
        assert!(dec_u.semantic_eq(&utf8_col));

        // 2. List<Utf8> and List<List<Utf8>>: outer preferred_codec == Codec::None, sparse & take parity.
        let list_offsets: Vec<u32> = (0..=n as u32).collect();
        let list_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: list_offsets.clone(),
                    values: Box::new(utf8_col.clone()),
                },
                None,
            )
            .unwrap(),
        );
        let list_dt = DataType::List(Box::new(DataType::Utf8));
        let (enc_l, entry_l, dec_l) =
            roundtrip_via_frame(&list_col, &list_dt, WriteProfile::L1Compact, None);
        assert_eq!(enc_l.encoding, Encoding::ListEncoded);
        assert_eq!(enc_l.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED, 0);
        assert_eq!(entry_l.codec, Codec::None);
        assert_eq!(entry_l.raw_len, entry_l.stored_len);
        assert!(dec_l.semantic_eq(&list_col));

        let mut sparse_mask = Bitmap::new_zeroed(n);
        for r in [0usize, 23, 255, 256, 512, 1029] {
            let _ = sparse_mask.set(r, true);
        }
        let sel_l =
            decode_mini_block_selected(&enc_l.raw_body, &entry_l, &list_dt, None, &sparse_mask)
                .unwrap();
        let sel_l_flat = sel_l.materialize(None).unwrap();
        let orig_l_flat = list_col.materialize(None).unwrap();
        for r in [0usize, 23, 255, 256, 512, 1029] {
            assert_eq!(
                sel_l_flat.get_scalar(r).unwrap(),
                orig_l_flat.get_scalar(r).unwrap(),
                "List<Utf8> sparse mismatch at row {r}"
            );
        }

        // Nested List<List<Utf8>> also propagates outer Codec::None.
        let list2_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: list_dt.clone(),
                    offsets: list_offsets,
                    values: Box::new(list_col),
                },
                None,
            )
            .unwrap(),
        );
        let list2_dt = DataType::List(Box::new(list_dt.clone()));
        let (enc_l2, entry_l2, dec_l2) =
            roundtrip_via_frame(&list2_col, &list2_dt, WriteProfile::L1Compact, None);
        assert_eq!(enc_l2.encoding, Encoding::ListEncoded);
        assert_eq!(entry_l2.codec, Codec::None);
        assert!(dec_l2.semantic_eq(&list2_col));

        // 3. ShreddedSubCol with sub-chunked FSST values: outer codec == Codec::None + selected parity.
        let sub = ShreddedSubCol {
            in_map: Bitmap::new_ones(n),
            value_null: nulls,
            values: utf8_col,
        };
        let (sub_codec, sub_raw) =
            nested::encode_shredded_subcol_with_codec(&sub, WriteProfile::L1Compact).unwrap();
        assert_eq!(sub_codec, Codec::None);
        let dec_sub = decode_shredded_subcol(&sub_raw, n).unwrap();
        assert!(dec_sub.values.semantic_eq(&sub.values));
        let dec_sub_sel = decode_shredded_subcol_selected(&sub_raw, n, &sparse_mask).unwrap();
        let sel_sub_flat = dec_sub_sel.values.materialize(None).unwrap();
        let orig_sub_flat = sub.values.materialize(None).unwrap();
        for r in [0usize, 23, 255, 256, 512, 1029] {
            assert_eq!(
                sel_sub_flat.get_scalar(r).unwrap(),
                orig_sub_flat.get_scalar(r).unwrap(),
                "ShreddedSubCol sparse mismatch at row {r}"
            );
        }

        // 4. Reject BLOCK_FLAG_FSST_SUBCHUNKED on non-Fsst encodings everywhere.
        let bad_entry = BlockEntry {
            first_row: 0,
            row_count: 4,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding: Encoding::Plain,
            codec: Codec::None,
            block_flags: BLOCK_FLAG_FSST_SUBCHUNKED,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        let encoded_be = bad_entry.encode();
        assert!(matches!(
            BlockEntry::decode(&encoded_be),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst encoding",
            })
        ));
        assert!(matches!(
            decode_mini_block(&[], &bad_entry, &DataType::Utf8, None),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst encoding",
            })
        ));
        assert!(matches!(
            decode_mini_block_selected(
                &[],
                &bad_entry,
                &DataType::Utf8,
                None,
                &Bitmap::new_ones(4)
            ),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst encoding",
            })
        ));
        assert!(matches!(
            decode_mini_block_take(&[], &bad_entry, &DataType::Utf8, None, &[0, 1]),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst encoding",
            })
        ));

        // ListEncoded child header with BLOCK_FLAG_FSST_SUBCHUNKED on Plain child -> Corrupt.
        let mut bad_list = vec![1u8, 0x01]; // count_bw=1, counts=[1, 0]
        bad_list.push(Encoding::Plain as u8);
        bad_list.push(BLOCK_FLAG_FSST_SUBCHUNKED);
        bad_list.extend_from_slice(&1u32.to_le_bytes()); // child_len=1
        bad_list.extend_from_slice(&0u32.to_le_bytes()); // child_null_count=0
        bad_list.push(0); // min_inline_len
        bad_list.extend_from_slice(&[0u8; 16]);
        bad_list.push(0); // has_dict=0
        bad_list.extend_from_slice(&0u32.to_le_bytes()); // body_len=0
        let list_entry = BlockEntry {
            first_row: 0,
            row_count: 2,
            null_count: 0,
            offset: 0,
            stored_len: bad_list.len() as u32,
            raw_len: bad_list.len() as u32,
            encoding: Encoding::ListEncoded,
            codec: Codec::None,
            block_flags: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        assert!(matches!(
            decode_mini_block(&bad_list, &list_entry, &list_dt, None),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst ListEncoded child",
            })
        ));

        // ShreddedSubCol header with BLOCK_FLAG_FSST_SUBCHUNKED on Plain or unknown flag bit -> Corrupt.
        let build_bad_subcol = |enc: u8, flags: u8| -> Vec<u8> {
            let mut buf = vec![1u8, 1u8]; // in_map=all_ones, value_null=all_ones
            buf.push(enc);
            buf.push(flags);
            buf.push(0); // min_inline_len
            buf.extend_from_slice(&[0u8; 16]);
            buf.push(0); // has_dict=0
            buf.extend_from_slice(&0u32.to_le_bytes()); // body_len=0
            buf
        };
        assert!(matches!(
            decode_shredded_subcol(
                &build_bad_subcol(Encoding::Plain as u8, BLOCK_FLAG_FSST_SUBCHUNKED),
                2
            ),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst shredded subcol",
            })
        ));
        assert!(matches!(
            decode_shredded_subcol(&build_bad_subcol(Encoding::Fsst as u8, 0x40), 2),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED set on non-Plain shredded subcol",
            })
        ));
        assert!(matches!(
            decode_shredded_subcol(&build_bad_subcol(Encoding::Plain as u8, 0x80), 2),
            Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_NO_INLINE_PAYLOADS forbidden on shredded subcol",
            })
        ));
    }

    #[test]
    fn all_null_take_short_circuit_parity_and_corrupt_rejection() {
        let n = 1_200usize;
        // Alternating null bitmap: rows 1, 3, 5, ..., 1199 are NULL, while >16 transitions in the
        // first 64 rows keep `try_encode_rle` from intercepting the target encodings.
        let half_nulls = Bitmap::from_bools(&(0..n).map(|i| i % 2 == 0).collect::<Vec<_>>());
        let null_indices_1 = [1usize];
        let null_indices_many = [1usize, 3, 19, 513, 1, 1199];
        let mixed_indices = [0usize, 1, 20, 513, 40, 1199];

        let assert_all_null_short_circuit =
            |col: &Column,
             dt: &DataType,
             profile: WriteProfile,
             dict: Option<&FileDict>,
             expected_enc: Encoding,
             expect_subchunked: Option<bool>,
             row_n: usize,
             null_1: &[usize],
             null_many: &[usize],
             mixed_idx: &[usize]| {
                let enc = encode_mini_block(col, dt, profile, dict).unwrap();
                assert_eq!(enc.encoding, expected_enc, "encoding mismatch for {dt:?}");
                if let Some(sub) = expect_subchunked {
                    assert_eq!(
                        (enc.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0,
                        sub,
                        "subchunked flag mismatch for {dt:?}"
                    );
                }
                let (entry, raw) = frame_raw_body(&enc, row_n);
                let full_flat = decode_mini_block(&raw, &entry, dt, dict)
                    .unwrap()
                    .materialize(None)
                    .unwrap();

                for indices in [null_1, null_many] {
                    let taken = decode_mini_block_take(&raw, &entry, dt, dict, indices).unwrap();
                    assert!(
                        matches!(
                            &taken,
                            Column::Const { value, len }
                                if value.is_null() && value.data_type() == *dt && *len == indices.len()
                        ),
                        "expected Column::Const(Null) short-circuit for {expected_enc:?}/{dt:?}"
                    );
                    let taken_flat = taken.materialize(None).unwrap();
                    for (out_pos, &row) in indices.iter().enumerate() {
                        assert!(taken_flat.is_null(out_pos));
                        assert!(full_flat.is_null(row));
                    }
                }

                // Mixed null + non-null indices must match full decode.
                let mixed = decode_mini_block_take(&raw, &entry, dt, dict, mixed_idx).unwrap();
                let mixed_flat = mixed.materialize(None).unwrap();
                for (out_pos, &row) in mixed_idx.iter().enumerate() {
                    assert_eq!(mixed_flat.is_null(out_pos), full_flat.is_null(row));
                    if !full_flat.is_null(row) {
                        assert_eq!(
                            mixed_flat.get_scalar(out_pos).unwrap(),
                            full_flat.get_scalar(row).unwrap()
                        );
                    }
                }

                // Corrupt body rejection STILL triggers even when every requested row is NULL.
                assert!(
                    decode_mini_block_take(&raw[..raw.len() - 1], &entry, dt, dict, null_1)
                        .is_err(),
                    "truncated body must be rejected for {expected_enc:?}/{dt:?}"
                );
                let mut trailing = raw.clone();
                trailing.push(0xAA);
                assert!(
                    decode_mini_block_take(&trailing, &entry, dt, dict, null_1).is_err(),
                    "trailing byte must be rejected for {expected_enc:?}/{dt:?}"
                );
                // Truncated null bitmap itself.
                let null_bytes_len = row_n.div_ceil(8);
                assert!(
                    decode_mini_block_take(&raw[..null_bytes_len - 1], &entry, dt, dict, null_1)
                        .is_err(),
                    "truncated null bitmap must be rejected for {expected_enc:?}/{dt:?}"
                );

                (entry, raw)
            };

        // 1. BitpackBool
        let bool_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Bool(Bitmap::from_bools(
                    &(0..n).map(|i| (i / 2) % 2 == 0).collect::<Vec<_>>(),
                )),
                Some(half_nulls.clone()),
            )
            .unwrap(),
        );
        assert_all_null_short_circuit(
            &bool_col,
            &DataType::Bool,
            WriteProfile::L0Fast,
            None,
            Encoding::BitpackBool,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );

        // 2. Plain fixed-width (F64, FixedBytes16) & Plain varlen (Utf8)
        let plain_f64_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64((0..n).map(|i| (i as f64) * 0.123456789).collect()),
                Some(half_nulls.clone()),
            )
            .unwrap(),
        );
        assert_all_null_short_circuit(
            &plain_f64_col,
            &DataType::F64,
            WriteProfile::L0Fast,
            None,
            Encoding::Plain,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );

        let plain_f16_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16((0..n).map(|i| (i as u128).to_le_bytes()).collect()),
                Some(half_nulls.clone()),
            )
            .unwrap(),
        );
        assert_all_null_short_circuit(
            &plain_f16_col,
            &DataType::FixedBytes16,
            WriteProfile::L0Fast,
            None,
            Encoding::Plain,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );

        let short_strs: Vec<String> = (0..n).map(|i| format!("row-{i:04}-payload")).collect();
        let plain_utf8_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(short_strs.iter().map(String::as_str)).unwrap(),
                ),
                Some(half_nulls.clone()),
            )
            .unwrap(),
        );
        assert_all_null_short_circuit(
            &plain_utf8_col,
            &DataType::Utf8,
            WriteProfile::L0Fast,
            None,
            Encoding::Plain,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );

        // 3. ForBitpack (I64) + invalid bit_width rejection on all-null take
        let for_i64_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64((0..n).map(|i| ((i * 7919) % 5000) as i64).collect()),
                Some(half_nulls.clone()),
            )
            .unwrap(),
        );
        let (for_entry, mut for_raw) = assert_all_null_short_circuit(
            &for_i64_col,
            &DataType::I64,
            WriteProfile::L1Compact,
            None,
            Encoding::ForBitpack,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );
        let null_bytes_len = n.div_ceil(8);
        for_raw[null_bytes_len + 8] = 65; // invalid bw > 64
        assert!(
            decode_mini_block_take(&for_raw, &for_entry, &DataType::I64, None, &null_indices_1)
                .is_err()
        );

        // 4. DeltaBitpack (TimestampNs) + invalid bw rejection on all-null take
        let ts_dt = DataType::TimestampNs { utc: true };
        let delta_ts_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: (0..n)
                        .map(|i| 1_700_000_000_000 + (i as i64) * 1_000_003)
                        .collect(),
                    utc: true,
                },
                Some(half_nulls.clone()),
            )
            .unwrap(),
        );
        let (delta_entry, mut delta_raw) = assert_all_null_short_circuit(
            &delta_ts_col,
            &ts_dt,
            WriteProfile::L1Compact,
            None,
            Encoding::DeltaBitpack,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );
        delta_raw[null_bytes_len + 1] = 65; // invalid bw > 64
        assert!(
            decode_mini_block_take(&delta_raw, &delta_entry, &ts_dt, None, &null_indices_1)
                .is_err()
        );

        // 5. Alp (F64) + invalid exp rejection on all-null take
        let alp_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64((0..n).map(|i| ((i % 97) as f64) * 0.000_025).collect()),
                Some(half_nulls.clone()),
            )
            .unwrap(),
        );
        let (alp_entry, mut alp_raw) = assert_all_null_short_circuit(
            &alp_col,
            &DataType::F64,
            WriteProfile::L1Compact,
            None,
            Encoding::Alp,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );
        alp_raw[null_bytes_len] = 25; // invalid exp > 18
        assert!(
            decode_mini_block_take(&alp_raw, &alp_entry, &DataType::F64, None, &null_indices_1)
                .is_err()
        );

        // 6. Dict (both ForBitpack and Rle sub-encodings) + bad sub_enc/bw rejection on all-null take
        let dict_vals: Vec<String> = (0..12).map(|k| format!("svc-{k:02}")).collect();
        let file_dict = FileDict::from_flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(dict_vals.iter().map(String::as_str)).unwrap(),
                ),
                None,
            )
            .unwrap(),
            true,
        )
        .unwrap();
        let dict_fb_strs: Vec<&str> = (0..n).map(|i| dict_vals[(i / 2) % 12].as_str()).collect();
        let dict_fb_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(dict_fb_strs).unwrap()),
                Some(half_nulls),
            )
            .unwrap(),
        );
        let (dict_fb_entry, mut dict_fb_raw) = assert_all_null_short_circuit(
            &dict_fb_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            Some(&file_dict),
            Encoding::Dict,
            None,
            n,
            &null_indices_1,
            &null_indices_many,
            &mixed_indices,
        );
        assert_eq!(dict_fb_raw[null_bytes_len], 0);
        dict_fb_raw[null_bytes_len] = 99; // unknown sub_enc
        assert!(
            decode_mini_block_take(
                &dict_fb_raw,
                &dict_fb_entry,
                &DataType::Utf8,
                Some(&file_dict),
                &null_indices_1
            )
            .is_err()
        );

        // Dict Rle sub-encoding: leading 200 NULL rows followed by 4 clean runs of 250 rows each.
        let rle_nulls = Bitmap::from_bools(&(0..n).map(|i| i >= 200).collect::<Vec<_>>());
        let dict_rle_strs: Vec<&str> = (0..n).map(|i| dict_vals[(i / 250) % 12].as_str()).collect();
        let dict_rle_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(dict_rle_strs).unwrap()),
                Some(rle_nulls),
            )
            .unwrap(),
        );
        let (dict_rle_entry, mut dict_rle_raw) = assert_all_null_short_circuit(
            &dict_rle_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            Some(&file_dict),
            Encoding::Dict,
            None,
            n,
            &[0],
            &[0, 1, 199, 0],
            &[0, 200, 100, 500, 199, 900],
        );
        assert_eq!(dict_rle_raw[null_bytes_len], 1);
        dict_rle_raw[null_bytes_len + 5] = 33; // invalid re_bw > 32
        assert!(
            decode_mini_block_take(
                &dict_rle_raw,
                &dict_rle_entry,
                &DataType::Utf8,
                Some(&file_dict),
                &[0]
            )
            .is_err()
        );

        // 7. Fsst single-chunk (row_count <= 512 -> never sub-chunked) + bad symbol len rejection
        let single_n = 400usize;
        let single_nulls =
            Bitmap::from_bools(&(0..single_n).map(|i| i % 2 == 0).collect::<Vec<_>>());
        let fsst_single_strs: Vec<String> = (0..single_n)
            .map(|i| {
                format!(
                    "{{\"service\":\"checkout-api\",\"environment\":\"production\",\"trace\":\"{:08x}\",\"idx\":{i}}}",
                    i * 131
                )
            })
            .collect();
        let fsst_single_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(fsst_single_strs.iter().map(String::as_str)).unwrap(),
                ),
                Some(single_nulls),
            )
            .unwrap(),
        );
        let (fsst_s_entry, mut fsst_s_raw) = assert_all_null_short_circuit(
            &fsst_single_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            None,
            Encoding::Fsst,
            Some(false),
            single_n,
            &[1],
            &[1, 3, 199, 399],
            &[0, 1, 20, 199, 398, 399],
        );
        // Corrupt first symbol length to 0 (valid is 1..=8).
        fsst_s_raw[single_n.div_ceil(8) + 1] = 0;
        assert!(
            decode_mini_block_take(&fsst_s_raw, &fsst_s_entry, &DataType::Utf8, None, &[1])
                .is_err()
        );

        // 8. Fsst sub-chunked (`BLOCK_FLAG_FSST_SUBCHUNKED` set) + bad header rejection on all-null take
        let fsst_sub_strs: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "https://observability.cetra.sh/api/v1/spans/trace_{i:06}/events/node_{:03}/req_{:08x}/meta_{:06x}/extra_payload_chunk_{:08x}_{:08x}_{:08x}",
                    i % 89,
                    (i as u32).wrapping_mul(2654435761),
                    (i as u32).wrapping_mul(2246822519) & 0x00FF_FFFF,
                    (i as u32).wrapping_mul(3266489917),
                    (i as u32).wrapping_mul(668265263),
                    (i as u32).wrapping_mul(374761393),
                )
            })
            .collect();
        let fsst_sub_nulls = Bitmap::from_bools(&(0..n).map(|i| i % 5 != 0).collect::<Vec<_>>());
        let fsst_sub_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(fsst_sub_strs.iter().map(String::as_str)).unwrap(),
                ),
                Some(fsst_sub_nulls),
            )
            .unwrap(),
        );
        let (sub_entry, mut sub_raw) = assert_all_null_short_circuit(
            &fsst_sub_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            None,
            Encoding::Fsst,
            Some(true),
            n,
            &[0],
            &[0, 5, 515, 1195],
            &[0, 1, 5, 512, 515, 1199],
        );
        sub_raw[null_bytes_len + 1] = 9; // invalid symbol len > 8
        assert!(decode_mini_block_take(&sub_raw, &sub_entry, &DataType::Utf8, None, &[0]).is_err());
    }

    #[test]
    fn pass16a_decode_rle_block_take_and_selected_parity_all_inner_encodings() {
        // 80 runs * 16 rows = 1280 rows so both sparse (`selected_runs * 16 < num_runs`, e.g. 1..4 runs)
        // and dense (`>= 5` runs) inner-take / inner-selected paths are exercised!
        let num_runs = 80usize;
        let run_len = 16usize;
        let n = num_runs * run_len;
        let run_ends: Vec<u32> = (1..=num_runs).map(|k| (k * run_len) as u32).collect();
        // ~50% null runs (even runs are null, odd runs valid; plus run 0 null, run 1 valid).
        let run_nulls = Bitmap::from_bools(&(0..num_runs).map(|r| r % 2 == 1).collect::<Vec<_>>());

        let assert_rle_case = |label: &str,
                               dt: &DataType,
                               run_values: FlatColumn,
                               profile: WriteProfile,
                               expected_inner_enc: Encoding| {
            let rle_col = Column::Rle(RleColumn::try_new(run_values, run_ends.clone()).unwrap());
            let enc = encode_mini_block(&rle_col, dt, profile, None).unwrap();
            assert_eq!(enc.encoding, Encoding::Rle, "{label}: must encode as Rle");
            let (entry, raw) = frame_raw_body(&enc, n);
            let hdr = parse_rle_header(&raw, n, entry.block_flags).unwrap();
            assert_eq!(
                hdr.inner_enc, expected_inner_enc,
                "{label}: unexpected RLE inner_enc"
            );

            let full = decode_mini_block(&raw, &entry, dt, None).unwrap();
            let full_flat = full.materialize(None).unwrap();

            // 1. decode_rle_block_take (via decode_mini_block_take & direct):
            //    - empty indices
            //    - all-null taken rows (hits only even runs) -> Column::Const(Null)
            //    - single valid row
            //    - sparse sorted mixed null/valid (3 runs: 3*16 < 80)
            //    - unsorted with duplicates across boundaries
            //    - dense taken rows (every run hit: 80*16 >= 80)
            let empty_idx: Vec<usize> = vec![];
            let all_null_idx = vec![0usize, 15, 32, 64, n - 17]; // runs 0, 0, 2, 4, 78 (all even -> null)
            let single_valid_idx = vec![20usize]; // run 1 (valid)
            let sparse_mixed_idx = vec![0usize, 20, 35, 50, 700]; // runs 0(N), 1(V), 2(N), 3(V), 43(V)
            let unsorted_dup_idx = vec![n - 1, 0, 20, n - 1, 50, 16, 15, 0, 700, 20];
            let dense_idx: Vec<usize> =
                (0..num_runs).map(|r| r * run_len + (r % run_len)).collect();

            for (case_name, indices) in [
                ("empty", &empty_idx),
                ("all_null", &all_null_idx),
                ("single_valid", &single_valid_idx),
                ("sparse_mixed", &sparse_mixed_idx),
                ("unsorted_dup", &unsorted_dup_idx),
                ("dense", &dense_idx),
            ] {
                let taken = decode_mini_block_take(&raw, &entry, dt, None, indices).unwrap();
                let direct = numeric::decode_rle_block_take(&raw, &entry, dt, indices).unwrap();
                assert_eq!(taken.len(), indices.len(), "{label}/{case_name}");
                assert_eq!(direct.len(), indices.len(), "{label}/{case_name}");
                if case_name == "all_null" {
                    assert!(
                        matches!(taken, Column::Const { ref value, len } if value.is_null() && len == indices.len()),
                        "{label}: all-null take must short-circuit to Column::Const(Null)"
                    );
                }
                let taken_flat = taken.materialize(None).unwrap();
                let direct_flat = direct.materialize(None).unwrap();
                assert!(taken_flat.semantic_eq(&direct_flat), "{label}/{case_name}");
                for (pos, &row) in indices.iter().enumerate() {
                    assert_eq!(
                        taken_flat.is_null(pos),
                        full_flat.is_null(row),
                        "{label}/{case_name} row {row}"
                    );
                    if !full_flat.is_null(row) {
                        assert_eq!(
                            taken_flat.get_scalar(pos).unwrap(),
                            full_flat.get_scalar(row).unwrap(),
                            "{label}/{case_name} row {row}"
                        );
                    }
                }
            }

            // 2. decode_rle_block_selected:
            //    - empty mask -> Column::Const(Null)
            //    - all-null active rows (multiple even runs) -> Column::Const(Null)
            //    - single non-null run (multiple rows inside run 1) -> Column::Const(scalar)
            //    - multi-selected-run sparse (3 runs: 3*16 < 80) -> Column::Rle
            let empty_mask = Bitmap::new_zeroed(n);
            let sel_empty =
                decode_mini_block_selected(&raw, &entry, dt, None, &empty_mask).unwrap();
            assert!(
                matches!(sel_empty, Column::Const { ref value, len } if value.is_null() && len == n),
                "{label}: empty mask must return Const(Null)"
            );

            let mut null_runs_mask = Bitmap::new_zeroed(n);
            for &r in &all_null_idx {
                let _ = null_runs_mask.set(r, true);
            }
            let sel_all_null =
                decode_mini_block_selected(&raw, &entry, dt, None, &null_runs_mask).unwrap();
            assert!(
                matches!(sel_all_null, Column::Const { ref value, len } if value.is_null() && len == n),
                "{label}: all-null selected runs must return Const(Null)"
            );

            let mut one_run_mask = Bitmap::new_zeroed(n);
            for r in [16usize, 20, 31] {
                let _ = one_run_mask.set(r, true); // all in run 1
            }
            let sel_one_run =
                decode_mini_block_selected(&raw, &entry, dt, None, &one_run_mask).unwrap();
            assert!(
                matches!(sel_one_run, Column::Const { ref value, len } if !value.is_null() && len == n),
                "{label}: single selected valid run must short-circuit to Column::Const(scalar)"
            );
            assert_sparse_scalar_parity(&rle_col, &full, &sel_one_run, &one_run_mask);

            let mut multi_sparse_mask = Bitmap::new_zeroed(n);
            for &r in &[0usize, 20, 50, 700] {
                let _ = multi_sparse_mask.set(r, true);
            }
            let sel_multi =
                decode_mini_block_selected(&raw, &entry, dt, None, &multi_sparse_mask).unwrap();
            assert_sparse_scalar_parity(&rle_col, &full, &sel_multi, &multi_sparse_mask);

            // 3. Corrupt-input rejection even when all taken rows are null or empty:
            //    truncate inner_body by 1 byte -> must return Err!
            let trunc = &raw[..raw.len() - 1];
            assert!(
                decode_mini_block_take(trunc, &entry, dt, None, &all_null_idx).is_err(),
                "{label}: truncated inner body must fail even on all-null take"
            );
            assert!(
                decode_mini_block_take(trunc, &entry, dt, None, &empty_idx).is_err(),
                "{label}: truncated inner body must fail on empty take"
            );
            assert!(
                decode_mini_block_selected(trunc, &entry, dt, None, &null_runs_mask).is_err(),
                "{label}: truncated inner body must fail on all-null selected"
            );
            assert!(
                decode_mini_block_selected(trunc, &entry, dt, None, &one_run_mask).is_err(),
                "{label}: truncated inner body must fail on single-run selected"
            );
        };

        // Bool -> BitpackBool
        let bool_vals = FlatColumn::try_new(
            FlatData::Bool(Bitmap::from_bools(
                &(0..num_runs).map(|r| (r / 2) % 2 == 0).collect::<Vec<_>>(),
            )),
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "Bool",
            &DataType::Bool,
            bool_vals,
            WriteProfile::L0Fast,
            Encoding::BitpackBool,
        );

        // I64 -> ForBitpack
        let i64_vals = FlatColumn::try_new(
            FlatData::I64((0..num_runs).map(|r| 1000 + (r as i64) * 7).collect()),
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "I64",
            &DataType::I64,
            i64_vals,
            WriteProfile::L0Fast,
            Encoding::ForBitpack,
        );

        // TimestampNs -> ForBitpack
        let ts_dt = DataType::TimestampNs { utc: true };
        let ts_vals = FlatColumn::try_new(
            FlatData::TimestampNs {
                values: (0..num_runs)
                    .map(|r| 1_700_000_000_000_000_000i64 + (r as i64) * 1_000)
                    .collect(),
                utc: true,
            },
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "TimestampNs",
            &ts_dt,
            ts_vals,
            WriteProfile::L0Fast,
            Encoding::ForBitpack,
        );

        // Date -> ForBitpack
        let date_vals = FlatColumn::try_new(
            FlatData::Date((0..num_runs).map(|r| 19_000 + r as i32).collect()),
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "Date",
            &DataType::Date,
            date_vals,
            WriteProfile::L0Fast,
            Encoding::ForBitpack,
        );

        // Decimal128 (narrow range -> ForBitpack, wide range -> Plain)
        let dec_dt = DataType::Decimal128 {
            precision: 38,
            scale: 6,
        };
        let dec_for = FlatColumn::try_new(
            FlatData::Decimal128 {
                values: (0..num_runs)
                    .map(|r| 500_000i128 + r as i128 * 13)
                    .collect(),
                precision: 38,
                scale: 6,
            },
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "Decimal128_For",
            &dec_dt,
            dec_for,
            WriteProfile::L0Fast,
            Encoding::ForBitpack,
        );

        let dec_plain = FlatColumn::try_new(
            FlatData::Decimal128 {
                values: (0..num_runs)
                    .map(|r| {
                        if r % 4 == 1 {
                            i128::MIN / 4
                        } else {
                            i128::MAX / 4
                        }
                    })
                    .collect(),
                precision: 38,
                scale: 6,
            },
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "Decimal128_Plain",
            &dec_dt,
            dec_plain,
            WriteProfile::L0Fast,
            Encoding::Plain,
        );

        // F64 -> Plain (L0Fast) & Alp (L1Compact)
        let f64_alp_vals: Vec<f64> = (0..num_runs)
            .map(|r| {
                if r == 43 {
                    std::f64::consts::PI
                } else {
                    (r as f64) * 0.25
                }
            })
            .collect();
        let f64_flat =
            FlatColumn::try_new(FlatData::F64(f64_alp_vals), Some(run_nulls.clone())).unwrap();
        assert_rle_case(
            "F64_Plain",
            &DataType::F64,
            f64_flat.clone(),
            WriteProfile::L0Fast,
            Encoding::Plain,
        );
        assert_rle_case(
            "F64_Alp",
            &DataType::F64,
            f64_flat,
            WriteProfile::L1Compact,
            Encoding::Alp,
        );

        // Interval, FixedBytes8, FixedBytes16 -> Plain
        let iv_vals = FlatColumn::try_new(
            FlatData::Interval(
                (0..num_runs)
                    .map(|r| Interval {
                        months: r as i32,
                        days: (r * 2) as i32,
                        nanos: (r as i64) * 1000,
                    })
                    .collect(),
            ),
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "Interval",
            &DataType::Interval,
            iv_vals,
            WriteProfile::L0Fast,
            Encoding::Plain,
        );

        let fb8_vals = FlatColumn::try_new(
            FlatData::FixedBytes8((0..num_runs).map(|r| (r as u64).to_le_bytes()).collect()),
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "FixedBytes8",
            &DataType::FixedBytes8,
            fb8_vals,
            WriteProfile::L0Fast,
            Encoding::Plain,
        );

        let fb16_vals = FlatColumn::try_new(
            FlatData::FixedBytes16((0..num_runs).map(|r| (r as u128).to_le_bytes()).collect()),
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "FixedBytes16",
            &DataType::FixedBytes16,
            fb16_vals,
            WriteProfile::L0Fast,
            Encoding::Plain,
        );

        // Utf8 & Binary -> Plain (L0Fast) & Fsst (L1Compact)
        let strs: Vec<String> = (0..num_runs)
            .map(|r| {
                format!("https://observability.cetra.sh/api/v1/spans/service_{r:03}/endpoint_ok")
            })
            .collect();
        let utf8_flat = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(strs.iter().map(String::as_str)).unwrap()),
            Some(run_nulls.clone()),
        )
        .unwrap();
        assert_rle_case(
            "Utf8_Plain",
            &DataType::Utf8,
            utf8_flat.clone(),
            WriteProfile::L0Fast,
            Encoding::Plain,
        );
        assert_rle_case(
            "Utf8_Fsst",
            &DataType::Utf8,
            utf8_flat,
            WriteProfile::L1Compact,
            Encoding::Fsst,
        );

        let bins: Vec<Vec<u8>> = strs.iter().map(|s| s.as_bytes().to_vec()).collect();
        let bin_flat = FlatColumn::try_new(
            FlatData::Binary(BinaryData::try_from_slices(bins.iter().map(Vec::as_slice)).unwrap()),
            Some(run_nulls),
        )
        .unwrap();
        assert_rle_case(
            "Binary_Plain",
            &DataType::Binary,
            bin_flat.clone(),
            WriteProfile::L0Fast,
            Encoding::Plain,
        );
        assert_rle_case(
            "Binary_Fsst",
            &DataType::Binary,
            bin_flat,
            WriteProfile::L1Compact,
            Encoding::Fsst,
        );
    }

    #[test]
    fn pass16a_decode_narrow_column_selected_all_null_and_single_row_const_short_circuit() {
        let n = 256usize;
        let nulls = Bitmap::from_bools(&(0..n).map(|i| i % 4 != 0).collect::<Vec<_>>());
        // Row 0, 4, 8 are NULL; Row 1 is valid.
        let mut all_null_mask = Bitmap::new_zeroed(n);
        for r in [0usize, 4, 8] {
            let _ = all_null_mask.set(r, true);
        }
        let mut single_valid_mask = Bitmap::new_zeroed(n);
        let _ = single_valid_mask.set(1, true);
        let empty_mask = Bitmap::new_zeroed(n);

        let check_case = |label: &str,
                          col: &Column,
                          dt: &DataType,
                          profile: WriteProfile,
                          want_enc: Encoding| {
            let enc = encode_mini_block(col, dt, profile, None).unwrap();
            assert_eq!(enc.encoding, want_enc, "{label}");
            let (entry, raw) = frame_raw_body(&enc, n);
            let full = decode_mini_block(&raw, &entry, dt, None).unwrap();

            for (mname, mask) in [("empty", &empty_mask), ("all_null", &all_null_mask)] {
                let sel = decode_narrow_column_selected(&raw, &entry, dt, mask).unwrap();
                assert!(
                    matches!(sel, Column::Const { ref value, len } if value.is_null() && len == n),
                    "{label}/{mname}: must short-circuit to Column::Const(Null)"
                );
                assert_sparse_scalar_parity(col, &full, &sel, mask);
            }

            let sel_one =
                decode_narrow_column_selected(&raw, &entry, dt, &single_valid_mask).unwrap();
            assert!(
                matches!(sel_one, Column::Const { ref value, len } if !value.is_null() && len == n),
                "{label}/single_valid: must short-circuit to Column::Const(scalar)"
            );
            assert_sparse_scalar_parity(col, &full, &sel_one, &single_valid_mask);

            // Corrupt truncated body must still be rejected on empty, all-null, and single-row masks!
            let trunc = &raw[..raw.len() - 1];
            for (mname, mask) in [
                ("empty", &empty_mask),
                ("all_null", &all_null_mask),
                ("single_valid", &single_valid_mask),
            ] {
                assert!(
                    decode_narrow_column_selected(trunc, &entry, dt, mask).is_err(),
                    "{label}/{mname}: truncated body must fail"
                );
            }
        };

        // 1. BitpackBool
        let bool_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Bool(Bitmap::from_bools(
                    &(0..n).map(|i| i % 3 == 1).collect::<Vec<_>>(),
                )),
                Some(nulls.clone()),
            )
            .unwrap(),
        );
        check_case(
            "BitpackBool",
            &bool_col,
            &DataType::Bool,
            WriteProfile::L0Fast,
            Encoding::BitpackBool,
        );

        // 2. ForBitpack (non-monotonic I64)
        let for_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64((0..n).map(|i| 500 + ((i * 37) % 100) as i64).collect()),
                Some(nulls.clone()),
            )
            .unwrap(),
        );
        check_case(
            "ForBitpack",
            &for_col,
            &DataType::I64,
            WriteProfile::L0Fast,
            Encoding::ForBitpack,
        );

        // 3. DeltaBitpack (monotonic I64)
        let delta_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64((0..n).map(|i| 10_000 + (i as i64) * 5).collect()),
                Some(nulls.clone()),
            )
            .unwrap(),
        );
        check_case(
            "DeltaBitpack",
            &delta_col,
            &DataType::I64,
            WriteProfile::L0Fast,
            Encoding::DeltaBitpack,
        );

        // 4. Plain (F64 in L0Fast) & Alp (F64 in L1Compact)
        let f64_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64((0..n).map(|i| ((i % 53) as f64) * 0.125).collect()),
                Some(nulls.clone()),
            )
            .unwrap(),
        );
        check_case(
            "Plain_F64",
            &f64_col,
            &DataType::F64,
            WriteProfile::L0Fast,
            Encoding::Plain,
        );
        check_case(
            "Alp_F64",
            &f64_col,
            &DataType::F64,
            WriteProfile::L1Compact,
            Encoding::Alp,
        );

        // 5. Fsst (Utf8 in L1Compact)
        let fsst_strs: Vec<String> = (0..n)
            .map(|i| format!("https://observability.cetra.sh/api/v1/spans/item_{i:04}/status=ok"))
            .collect();
        let fsst_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(fsst_strs.iter().map(String::as_str)).unwrap(),
                ),
                Some(nulls),
            )
            .unwrap(),
        );
        check_case(
            "Fsst_Utf8",
            &fsst_col,
            &DataType::Utf8,
            WriteProfile::L1Compact,
            Encoding::Fsst,
        );
    }

    #[test]
    fn pass26_plain_subchunked_rejected_on_map_list_map_and_empty_non_varlen() {
        let make_plain_sub_entry = |row_count: u16, encoding: Encoding| BlockEntry {
            first_row: 0,
            row_count,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding,
            codec: Codec::None,
            block_flags: BLOCK_FLAG_PLAIN_SUBCHUNKED,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };

        // 1. `row_count == 0` non-Utf8/Binary blocks rejected across ALL 4 mini-block decoders!
        let empty_mask = Bitmap::new_zeroed(0);
        for dt in [DataType::I64, DataType::MapUtf8, DataType::Bool] {
            let entry0 = make_plain_sub_entry(0, Encoding::Plain);
            assert!(matches!(
                decode_mini_block(&[], &entry0, &dt, None),
                Err(StatamError::Corrupt {
                    reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED only valid for Utf8/Binary",
                })
            ));
            assert!(matches!(
                decode_mini_block_selected(&[], &entry0, &dt, None, &empty_mask),
                Err(StatamError::Corrupt {
                    reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED only valid for Utf8/Binary",
                })
            ));
            assert!(matches!(
                decode_mini_block_selected_parallel_bytes(
                    Bytes::new(),
                    &entry0,
                    &dt,
                    None,
                    &empty_mask,
                    2,
                ),
                Err(StatamError::Corrupt {
                    reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED only valid for Utf8/Binary",
                })
            ));
            assert!(matches!(
                decode_mini_block_take(&[], &entry0, &dt, None, &[]),
                Err(StatamError::Corrupt {
                    reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED only valid for Utf8/Binary",
                })
            ));
        }

        // 2. Shredded residual decoders reject `BLOCK_FLAG_PLAIN_SUBCHUNKED` (both row_count == 0
        //    and row_count > 0, both Plain and ShreddedMap encodings).
        for row_count in [0u16, 2u16] {
            let mask = Bitmap::new_ones(row_count as usize);
            for enc in [Encoding::Plain, Encoding::ShreddedMap] {
                let entry = make_plain_sub_entry(row_count, enc);
                assert!(matches!(
                    nested::decode_shredded_residual_block(&[], &entry),
                    Err(StatamError::Corrupt {
                        reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED forbidden on MapUtf8 residual",
                    })
                ));
                assert!(matches!(
                    decode_shredded_residual_block_selected(&[], &entry, &mask),
                    Err(StatamError::Corrupt {
                        reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED forbidden on MapUtf8 residual",
                    })
                ));
                assert!(matches!(
                    decode_shredded_residual_block_keys_only(&[], &entry, &mask),
                    Err(StatamError::Corrupt {
                        reason: "BLOCK_FLAG_PLAIN_SUBCHUNKED forbidden on MapUtf8 residual",
                    })
                ));
            }
        }

        // 3. `List<MapUtf8>` with `BLOCK_FLAG_PLAIN_SUBCHUNKED` on child rejected consistently by
        //    BOTH full `decode_mini_block` AND sparse `decode_mini_block_selected` (even when child len == 0!).
        let list_map_dt = DataType::List(Box::new(DataType::MapUtf8));
        // Build a minimal valid ListEncoded payload with 32 rows (all empty lists so total_elems == 0)
        // so sparse mask (1/32 < 1/16) takes the sparse nested decode path while full mask takes full decode.
        let rows = 32usize;
        let mut list_body = Vec::new();
        // offsets: len_bw = 0 (all zero lengths -> 32 empty lists, total_elems = 0)
        list_body.push(0u8);
        // child header (32 bytes): enc=Plain, flags=BLOCK_FLAG_PLAIN_SUBCHUNKED, child_len=0 (u32),
        // child_null_count=0 (u32), min_inline_len=0, min_inline=[0;16], has_dict=0, body_len=0 (u32).
        list_body.push(Encoding::Plain as u8);
        list_body.push(BLOCK_FLAG_PLAIN_SUBCHUNKED);
        list_body.extend_from_slice(&0u32.to_le_bytes());
        list_body.extend_from_slice(&0u32.to_le_bytes());
        list_body.push(0u8);
        list_body.extend_from_slice(&[0u8; 16]);
        list_body.push(0u8);
        list_body.extend_from_slice(&0u32.to_le_bytes());
        let list_entry = BlockEntry {
            first_row: 0,
            row_count: rows as u16,
            null_count: 0,
            offset: 0,
            stored_len: list_body.len() as u32,
            raw_len: list_body.len() as u32,
            encoding: Encoding::ListEncoded,
            codec: Codec::None,
            block_flags: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        let mut sparse_mask = Bitmap::new_zeroed(rows);
        let _ = sparse_mask.set(0, true);
        assert!(matches!(
            decode_mini_block(&list_body, &list_entry, &list_map_dt, None),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_mini_block_selected(&list_body, &list_entry, &list_map_dt, None, &sparse_mask),
            Err(StatamError::Corrupt { .. })
        ));
    }
}
