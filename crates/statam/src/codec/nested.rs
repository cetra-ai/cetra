//! Structured & nested column codecs: `ListEncoded`, `EventsStruct`, `LinksStruct`,
//! and `ShreddedMap` (with promoted key sub-columns, key-dict residual, and
//! `decode_single_map_key` fast path).

use std::collections::HashMap;
use std::fmt;

use cetra_column::{
    Bitmap, Column, DataType, EventsData, FlatColumn, FlatData, LinksData, MapData, ScalarValue,
    Utf8Data,
};

use crate::codec::dict::{
    DictBuilder, FileDict, check_varlen_take_bytes, decode_dict_codes_body, decode_dict_column,
    encode_dict_codes_body,
};
use crate::codec::fsst::{decode_fsst_utf8, encode_fsst_body};
use crate::codec::numeric::{
    decode_const_block, decode_inner_data, decode_rle_block, encode_delta_bitpack_body,
    encode_for_bitpack_body, encode_plain_body, encode_plain_utf8, read_optional_nulls,
    try_encode_const_or_null, write_optional_nulls,
};
use crate::codec::zonemap::{ZoneMapSummary, compute_zone_map};
use crate::codec::{EncodedMiniBlock, WriteProfile, preferred_block_codec};
use crate::format::{BLOCK_FLAG_HAS_NULLS, BlockEntry, Codec, Encoding, MAX_SHREDDED_KEYS};
use crate::simd::bitpack::{min_bits_u32, pack_u32, packed_bytes_len, unpack_u32};
use crate::{MAX_BLOCK_RAW_BYTES, MAX_DICT_BYTES, StatamError};

/// Minimum fraction of rows (`1 / 20` = 5%) required to promote a `MapUtf8` key.
const SHRED_MIN_PCT_DENOM: usize = 20;

/// One promoted key sub-column produced by [`shred_map`].
#[derive(Clone, PartialEq)]
pub struct ShreddedSubCol {
    /// Whether row `r` has a unique occurrence of this promoted key in the sub-column.
    pub in_map: Bitmap,
    /// Validity bitmap of the key's value (`1` = non-null string, `0` = SQL NULL value).
    pub value_null: Bitmap,
    /// Typed `Utf8` values column (`Const`, `Dict`, `Rle`, or `Flat`).
    pub values: Column,
}

impl fmt::Debug for ShreddedSubCol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShreddedSubCol")
            .field("len", &self.in_map.len())
            .field("in_map_ones", &self.in_map.count_ones())
            .field("value_null_ones", &self.value_null.count_ones())
            .field("values", &self.values)
            .finish()
    }
}

/// Output of shredding a `MapUtf8` [`FlatColumn`] against a list of promoted keys.
#[derive(Clone)]
pub struct ShreddedMapBlocks {
    /// Promoted key names in sub-column order.
    pub promoted_keys: Vec<String>,
    /// One [`ShreddedSubCol`] per entry in `promoted_keys`.
    pub sub_cols: Vec<ShreddedSubCol>,
    /// Residual `MapUtf8` column holding only rare & duplicate keys.
    pub residual: FlatColumn,
    /// Number of rows where promoted key `i` spilled to `residual` (e.g. duplicate keys).
    pub residual_counts: Vec<u32>,
}

impl fmt::Debug for ShreddedMapBlocks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShreddedMapBlocks")
            .field("promoted_count", &self.promoted_keys.len())
            .field("residual", &self.residual)
            .field("residual_counts", &self.residual_counts)
            .finish()
    }
}

/// Selects up to [`MAX_SHREDDED_KEYS`] (`64`) keys present in `>= 5%` of rows in `map`,
/// returned in deterministic lexicographical order.
///
/// If a key appears multiple times in the same row, it counts as 1 row for presence.
/// For high-cardinality dynamic maps (e.g. `W4` with 5,000 distinct keys), returns `vec![]`.
pub fn select_shredded_keys(map: &MapData) -> Vec<String> {
    let total_rows = map.len();
    if total_rows == 0 || map.entry_count() == 0 {
        return Vec::new();
    }

    let mut counts: HashMap<&str, (usize, usize)> = HashMap::new();
    let offsets = map.offsets();
    let keys = map.keys();

    for row in 0..total_rows {
        let start = offsets[row] as usize;
        let end = offsets[row + 1] as usize;
        for idx in start..end {
            if let Some(k) = keys.get(idx) {
                match counts.get_mut(k) {
                    Some((cnt, last_row)) => {
                        if *last_row != row {
                            *cnt += 1;
                            *last_row = row;
                        }
                    }
                    None => {
                        counts.insert(k, (1, row));
                    }
                }
            }
        }
    }

    let mut hot: Vec<(&str, usize)> = counts
        .into_iter()
        .filter_map(|(k, (cnt, _))| {
            (cnt.saturating_mul(SHRED_MIN_PCT_DENOM) >= total_rows).then_some((k, cnt))
        })
        .collect();

    hot.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    hot.truncate(MAX_SHREDDED_KEYS);

    let mut selected: Vec<String> = hot.into_iter().map(|(k, _)| k.to_owned()).collect();
    selected.sort_unstable();
    selected
}

/// Shreds a `MapUtf8` [`FlatColumn`] into `promoted_keys.len()` [`ShreddedSubCol`]s
/// plus a compact residual `MapUtf8` column containing only rare and duplicate keys.
///
/// `ShreddedMap` preserves logical map entries per row (lexicographically sorted promoted
/// keys merged with residual entries). Sorted input maps (such as Cetra's `BTreeMap`-backed
/// `attributes`, `metadata`, and `resource_attributes`) round-trip with exact
/// `FlatColumn::semantic_eq` when `promoted_keys` is sorted; unsorted input maps preserve
/// logical key-value pairs per row.
pub fn shred_map(
    col: &FlatColumn,
    promoted_keys: &[String],
) -> Result<ShreddedMapBlocks, StatamError> {
    if col.data_type() != DataType::MapUtf8 {
        return Err(StatamError::SchemaMismatch {
            reason: "shred_map expects DataType::MapUtf8",
        });
    }
    if promoted_keys.len() > MAX_SHREDDED_KEYS {
        return Err(StatamError::BoundsExceeded {
            reason: "promoted_keys exceeds MAX_SHREDDED_KEYS",
        });
    }
    let FlatData::MapUtf8(map) = col.data() else {
        return Err(StatamError::SchemaMismatch {
            reason: "shred_map expected FlatData::MapUtf8",
        });
    };

    let row_count = col.len();
    let k_count = promoted_keys.len();
    let mut key_to_idx: HashMap<&str, usize> = HashMap::with_capacity(k_count);
    for (idx, k) in promoted_keys.iter().enumerate() {
        if key_to_idx.insert(k.as_str(), idx).is_some() {
            return Err(StatamError::SchemaMismatch {
                reason: "duplicate key in promoted_keys",
            });
        }
    }

    let mut in_maps: Vec<Bitmap> = (0..k_count)
        .map(|_| Bitmap::new_zeroed(row_count))
        .collect();
    let mut value_nulls: Vec<Bitmap> = (0..k_count)
        .map(|_| Bitmap::new_zeroed(row_count))
        .collect();
    let mut sub_strings: Vec<Vec<Option<&str>>> = vec![vec![None; row_count]; k_count];
    let mut residual_counts = vec![0u32; k_count];

    let mut res_offsets = Vec::with_capacity(row_count + 1);
    res_offsets.push(0u32);
    let mut res_keys: Vec<&str> = Vec::new();
    let mut res_vals: Vec<&str> = Vec::new();
    let mut res_val_valid: Vec<bool> = Vec::new();

    let map_offsets = map.offsets();
    let map_keys = map.keys();
    let map_vals = map.values();
    let map_val_nulls = map.value_nulls();
    let row_nulls = col.nulls();

    // Per-row scratch state for duplicate detection among promoted keys.
    let mut row_counts = vec![0u16; k_count];
    let mut first_entry_idx = vec![0usize; k_count];
    let mut touched = Vec::with_capacity(k_count);

    for row in 0..row_count {
        if row_nulls.is_some_and(|bm| !bm.get(row)) {
            res_offsets.push(res_keys.len() as u32);
            continue;
        }

        let start = map_offsets[row] as usize;
        let end = map_offsets[row + 1] as usize;
        touched.clear();

        for idx in start..end {
            let k = map_keys.get(idx).unwrap_or("");
            if let Some(&pidx) = key_to_idx.get(k) {
                if row_counts[pidx] == 0 {
                    touched.push(pidx);
                    first_entry_idx[pidx] = idx;
                }
                row_counts[pidx] = row_counts[pidx].saturating_add(1);
            }
        }

        // Assign unique promoted entries to sub-columns; mark duplicates for residual.
        for &pidx in &touched {
            if row_counts[pidx] == 1 {
                let idx = first_entry_idx[pidx];
                let _ = in_maps[pidx].set(row, true);
                let is_valid = map_val_nulls.is_none_or(|bm| bm.get(idx));
                if is_valid {
                    let _ = value_nulls[pidx].set(row, true);
                    sub_strings[pidx][row] = Some(map_vals.get(idx).unwrap_or(""));
                } else {
                    let _ = value_nulls[pidx].set(row, false);
                }
            } else {
                residual_counts[pidx] = residual_counts[pidx].saturating_add(1);
            }
        }

        // Emit rare keys and duplicate promoted keys into residual in original order.
        for idx in start..end {
            let k = map_keys.get(idx).unwrap_or("");
            let spill = match key_to_idx.get(k) {
                None => true,
                Some(&pidx) => row_counts[pidx] > 1,
            };
            if spill {
                let is_valid = map_val_nulls.is_none_or(|bm| bm.get(idx));
                let v = if is_valid {
                    map_vals.get(idx).unwrap_or("")
                } else {
                    ""
                };
                res_keys.push(k);
                res_vals.push(v);
                res_val_valid.push(is_valid);
            }
        }

        for &pidx in &touched {
            row_counts[pidx] = 0;
        }

        let end_u32 = u32::try_from(res_keys.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "shredded residual entry count exceeds u32::MAX",
        })?;
        res_offsets.push(end_u32);
    }

    let mut sub_cols = Vec::with_capacity(k_count);
    for pidx in 0..k_count {
        let utf8 =
            Utf8Data::try_from_slices(sub_strings[pidx].iter().map(|opt| opt.unwrap_or("")))?;
        let flat_vals = FlatColumn::try_new(FlatData::Utf8(utf8), None)?;
        sub_cols.push(ShreddedSubCol {
            in_map: std::mem::replace(&mut in_maps[pidx], Bitmap::new_zeroed(0)),
            value_null: std::mem::replace(&mut value_nulls[pidx], Bitmap::new_zeroed(0)),
            values: Column::Flat(flat_vals),
        });
    }

    let res_keys_data = Utf8Data::try_from_slices(res_keys)?;
    let res_vals_data = Utf8Data::try_from_slices(res_vals)?;
    let res_val_nulls = if res_val_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&res_val_valid))
    };
    let residual_map = MapData::try_new(res_offsets, res_keys_data, res_vals_data, res_val_nulls)?;
    let residual = FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(residual_map)),
        row_nulls.cloned(),
    )?;

    Ok(ShreddedMapBlocks {
        promoted_keys: promoted_keys.to_vec(),
        sub_cols,
        residual,
        residual_counts,
    })
}

/// Reconstructs a full `MapUtf8` [`FlatColumn`] from an optional residual map and
/// promoted sub-columns.
///
/// `ShreddedMap` preserves logical map entries per row (merging sorted promoted keys
/// with residual entries). When the original map's keys were sorted (e.g. `BTreeMap`-backed
/// Cetra maps) and `subcols` is in lexicographical key order, the reconstructed column
/// matches the original under `FlatColumn::semantic_eq`.
pub fn unshred_map(
    row_count: usize,
    row_nulls: Option<&Bitmap>,
    residual: Option<&FlatColumn>,
    subcols: &[(&str, &ShreddedSubCol)],
) -> Result<FlatColumn, StatamError> {
    if let Some(bm) = row_nulls
        && bm.len() != row_count
    {
        return Err(StatamError::Corrupt {
            reason: "unshred_map row_nulls length mismatch",
        });
    }
    for (_, sub) in subcols {
        if sub.in_map.len() != row_count
            || sub.value_null.len() != row_count
            || sub.values.len() != row_count
        {
            return Err(StatamError::Corrupt {
                reason: "shredded sub-column length mismatch",
            });
        }
    }

    let res_map = match residual {
        Some(fc) => {
            if fc.len() != row_count {
                return Err(StatamError::Corrupt {
                    reason: "shredded residual row count mismatch",
                });
            }
            let FlatData::MapUtf8(m) = fc.data() else {
                return Err(StatamError::SchemaMismatch {
                    reason: "shredded residual is not MapUtf8",
                });
            };
            Some((m.as_ref(), fc.nulls()))
        }
        None => None,
    };

    // Pre-materialize sub-column values for O(1) row lookup.
    let mut flat_subs: Vec<FlatColumn> = Vec::with_capacity(subcols.len());
    for (_, sub) in subcols {
        flat_subs.push(sub.values.materialize(None)?);
    }
    let sub_utf8s: Vec<&Utf8Data> = flat_subs
        .iter()
        .map(|fc| match fc.data() {
            FlatData::Utf8(u) => Ok(u),
            _ => Err(StatamError::SchemaMismatch {
                reason: "shredded sub-column values must be Utf8",
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut out_keys: Vec<&str> = Vec::new();
    let mut out_vals: Vec<&str> = Vec::new();
    let mut out_val_valid: Vec<bool> = Vec::new();
    let mut out_row_valid = vec![true; row_count];

    for (row, valid_slot) in out_row_valid.iter_mut().enumerate().take(row_count) {
        let is_row_null = row_nulls.is_some_and(|bm| !bm.get(row))
            || res_map.is_some_and(|(_, rn)| rn.is_some_and(|bm| !bm.get(row)));
        if is_row_null {
            *valid_slot = false;
            out_offsets.push(out_keys.len() as u32);
            continue;
        }

        let (res_range_start, res_range_end) = if let Some((rm, _)) = res_map {
            (rm.offsets()[row] as usize, rm.offsets()[row + 1] as usize)
        } else {
            (0, 0)
        };

        // Merge promoted keys (lexicographically ordered) with residual entries.
        let mut p_idx = 0usize;
        let mut r_idx = res_range_start;

        while p_idx < subcols.len() && r_idx < res_range_end {
            if !subcols[p_idx].1.in_map.get(row) {
                p_idx += 1;
                continue;
            }
            let pk = subcols[p_idx].0;
            let rk = res_map
                .and_then(|(rm, _)| rm.keys().get(r_idx))
                .unwrap_or("");
            if pk <= rk {
                let valid = subcols[p_idx].1.value_null.get(row);
                let v = if valid {
                    sub_utf8s[p_idx].get(row).unwrap_or("")
                } else {
                    ""
                };
                out_keys.push(pk);
                out_vals.push(v);
                out_val_valid.push(valid);
                p_idx += 1;
            } else if let Some((rm, _)) = res_map {
                let valid = rm.value_nulls().is_none_or(|bm| bm.get(r_idx));
                let v = if valid {
                    rm.values().get(r_idx).unwrap_or("")
                } else {
                    ""
                };
                out_keys.push(rk);
                out_vals.push(v);
                out_val_valid.push(valid);
                r_idx += 1;
            }
        }
        while p_idx < subcols.len() {
            if subcols[p_idx].1.in_map.get(row) {
                let pk = subcols[p_idx].0;
                let valid = subcols[p_idx].1.value_null.get(row);
                let v = if valid {
                    sub_utf8s[p_idx].get(row).unwrap_or("")
                } else {
                    ""
                };
                out_keys.push(pk);
                out_vals.push(v);
                out_val_valid.push(valid);
            }
            p_idx += 1;
        }
        if let Some((rm, _)) = res_map {
            while r_idx < res_range_end {
                let rk = rm.keys().get(r_idx).unwrap_or("");
                let valid = rm.value_nulls().is_none_or(|bm| bm.get(r_idx));
                let v = if valid {
                    rm.values().get(r_idx).unwrap_or("")
                } else {
                    ""
                };
                out_keys.push(rk);
                out_vals.push(v);
                out_val_valid.push(valid);
                r_idx += 1;
            }
        }

        let end_u32 = u32::try_from(out_keys.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "unshred_map entry count exceeds u32::MAX",
        })?;
        out_offsets.push(end_u32);
    }

    let keys_data = Utf8Data::try_from_slices(out_keys)?;
    let vals_data = Utf8Data::try_from_slices(out_vals)?;
    let val_nulls = if out_val_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_val_valid))
    };
    let map = MapData::try_new(out_offsets, keys_data, vals_data, val_nulls)?;
    let out_nulls = if out_row_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_row_valid))
    };
    Ok(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(map)),
        out_nulls,
    )?)
}

/// Fast single-key `MapUtf8` projection (`map_keys` optimization).
///
/// When `residual_count == 0`, `residual` may be `None` and is never touched.
pub fn decode_single_map_key(
    row_count: usize,
    row_nulls: Option<&Bitmap>,
    key: &str,
    subcol: &ShreddedSubCol,
    residual_count: u32,
    residual: Option<&FlatColumn>,
) -> Result<FlatColumn, StatamError> {
    if let Some(bm) = row_nulls
        && bm.len() != row_count
    {
        return Err(StatamError::Corrupt {
            reason: "decode_single_map_key row_nulls length mismatch",
        });
    }
    if subcol.in_map.len() != row_count
        || subcol.value_null.len() != row_count
        || subcol.values.len() != row_count
    {
        return Err(StatamError::Corrupt {
            reason: "shredded sub-column length mismatch",
        });
    }

    let res_map = if residual_count > 0 {
        let fc = residual.ok_or(StatamError::Corrupt {
            reason: "missing residual map for key with non-zero residual_count",
        })?;
        if fc.len() != row_count {
            return Err(StatamError::Corrupt {
                reason: "shredded residual row count mismatch",
            });
        }
        let FlatData::MapUtf8(m) = fc.data() else {
            return Err(StatamError::SchemaMismatch {
                reason: "shredded residual is not MapUtf8",
            });
        };
        Some((m.as_ref(), fc.nulls()))
    } else {
        None
    };

    let flat_vals = subcol.values.materialize(None)?;
    let FlatData::Utf8(sub_utf8) = flat_vals.data() else {
        return Err(StatamError::SchemaMismatch {
            reason: "shredded sub-column values must be Utf8",
        });
    };

    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut out_keys: Vec<&str> = Vec::new();
    let mut out_vals: Vec<&str> = Vec::new();
    let mut out_val_valid: Vec<bool> = Vec::new();
    let mut out_row_valid = vec![true; row_count];

    for (row, valid_slot) in out_row_valid.iter_mut().enumerate().take(row_count) {
        let is_row_null = row_nulls.is_some_and(|bm| !bm.get(row))
            || res_map.is_some_and(|(_, rn)| rn.is_some_and(|bm| !bm.get(row)));
        if is_row_null {
            *valid_slot = false;
            out_offsets.push(out_keys.len() as u32);
            continue;
        }

        if subcol.in_map.get(row) {
            let valid = subcol.value_null.get(row);
            let v = if valid {
                sub_utf8.get(row).unwrap_or("")
            } else {
                ""
            };
            out_keys.push(key);
            out_vals.push(v);
            out_val_valid.push(valid);
        } else if let Some((rm, _)) = res_map {
            let start = rm.offsets()[row] as usize;
            let end = rm.offsets()[row + 1] as usize;
            for idx in start..end {
                if rm.keys().get(idx) == Some(key) {
                    let valid = rm.value_nulls().is_none_or(|bm| bm.get(idx));
                    let v = if valid {
                        rm.values().get(idx).unwrap_or("")
                    } else {
                        ""
                    };
                    out_keys.push(key);
                    out_vals.push(v);
                    out_val_valid.push(valid);
                }
            }
        }
        out_offsets.push(out_keys.len() as u32);
    }

    let keys_data = Utf8Data::try_from_slices(out_keys)?;
    let vals_data = Utf8Data::try_from_slices(out_vals)?;
    let val_nulls = if out_val_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_val_valid))
    };
    let map = MapData::try_new(out_offsets, keys_data, vals_data, val_nulls)?;
    let out_nulls = if out_row_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_row_valid))
    };
    Ok(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(map)),
        out_nulls,
    )?)
}

fn encode_bitmap_mode(bm: &Bitmap, out: &mut Vec<u8>) {
    if bm.all_ones() {
        out.push(1);
    } else if bm.all_zeros() {
        out.push(2);
    } else {
        out.push(0);
        out.extend_from_slice(&bm.to_bytes());
    }
}

fn decode_bitmap_mode(src: &[u8], len: usize) -> Result<(Bitmap, &[u8]), StatamError> {
    let Some(&mode) = src.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded bitmap mode",
        });
    };
    let rest = &src[1..];
    match mode {
        1 => Ok((Bitmap::new_ones(len), rest)),
        2 => Ok((Bitmap::new_zeroed(len), rest)),
        0 => {
            let byte_len = len.div_ceil(8);
            if rest.len() < byte_len {
                return Err(StatamError::Corrupt {
                    reason: "truncated shredded bitmap bytes",
                });
            }
            let bm = Bitmap::from_bytes(&rest[..byte_len], len)?;
            Ok((bm, &rest[byte_len..]))
        }
        _ => Err(StatamError::Corrupt {
            reason: "invalid shredded bitmap mode",
        }),
    }
}

/// Encodes one [`ShreddedSubCol`] into raw bytes (`in_map` + `value_null` + typed `Utf8` values).
pub fn encode_shredded_subcol(
    sub: &ShreddedSubCol,
    profile: WriteProfile,
) -> Result<Vec<u8>, StatamError> {
    let (_, bytes) = encode_shredded_subcol_with_codec(sub, profile)?;
    Ok(bytes)
}

/// Encodes one [`ShreddedSubCol`] and returns its preferred outer compression codec
/// (`Codec::None` when `Dict`/`Const`/`ConstNull`/`Rle`, else `Lz4`/`Zstd`).
pub(crate) fn encode_shredded_subcol_with_codec(
    sub: &ShreddedSubCol,
    profile: WriteProfile,
) -> Result<(Codec, Vec<u8>), StatamError> {
    let mut out = Vec::new();
    encode_bitmap_mode(&sub.in_map, &mut out);
    encode_bitmap_mode(&sub.value_null, &mut out);

    let child_payload = encode_child_column_payload(&sub.values, &DataType::Utf8, profile, 1)?;
    out.push(child_payload.encoding as u8);
    out.push(child_payload.block_flags);
    out.push(child_payload.min_inline_len);
    out.extend_from_slice(&child_payload.min_inline);
    if let Some((sorted, dict_bytes)) = child_payload.inline_dict {
        out.push(1);
        out.push(u8::from(sorted));
        out.extend_from_slice(&(dict_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&dict_bytes);
    } else {
        out.push(0);
    }
    out.extend_from_slice(&(child_payload.body.len() as u32).to_le_bytes());
    out.extend_from_slice(&child_payload.body);

    let codec = preferred_block_codec(profile, out.len(), true);
    Ok((codec, out))
}

/// Decodes a [`ShreddedSubCol`] encoded by [`encode_shredded_subcol`].
pub fn decode_shredded_subcol(src: &[u8], row_count: usize) -> Result<ShreddedSubCol, StatamError> {
    if row_count
        .checked_mul(4)
        .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "shredded subcol row_count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let (in_map, rest) = decode_bitmap_mode(src, row_count)?;
    let (value_null, rest) = decode_bitmap_mode(rest, row_count)?;
    if rest.len() < 20 {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded subcol value header",
        });
    }
    let enc = Encoding::from_u8(rest[0])?;
    let flags = rest[1];
    let min_inline_len = rest[2];
    if min_inline_len > 16 {
        return Err(StatamError::Corrupt {
            reason: "shredded subcol min_inline_len exceeds 16",
        });
    }
    let mut min_inline = [0u8; 16];
    min_inline.copy_from_slice(&rest[3..19]);
    let has_dict = rest[19];
    let mut pos = 20usize;

    let dict = match has_dict {
        0 => None,
        1 => {
            if rest.len() < pos + 5 {
                return Err(StatamError::Corrupt {
                    reason: "truncated shredded subcol inline dict header",
                });
            }
            let sorted = rest[pos] != 0;
            let dict_len =
                u32::from_le_bytes([rest[pos + 1], rest[pos + 2], rest[pos + 3], rest[pos + 4]])
                    as usize;
            pos += 5;
            if dict_len > MAX_DICT_BYTES || rest.len().saturating_sub(pos) < dict_len {
                return Err(StatamError::Corrupt {
                    reason: "truncated or oversized shredded subcol inline dict",
                });
            }
            let d = FileDict::decode(&rest[pos..pos + dict_len], sorted)?;
            pos += dict_len;
            Some(d)
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid shredded subcol has_dict flag",
            });
        }
    };

    if rest.len().saturating_sub(pos) < 4 {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded subcol body len",
        });
    }
    let body_len =
        u32::from_le_bytes([rest[pos], rest[pos + 1], rest[pos + 2], rest[pos + 3]]) as usize;
    pos += 4;
    if rest.len().saturating_sub(pos) != body_len {
        return Err(StatamError::Corrupt {
            reason: "shredded subcol body length mismatch",
        });
    }
    let body = &rest[pos..];

    let values = decode_child_column(
        body,
        enc,
        flags,
        row_count,
        min_inline_len,
        min_inline,
        &DataType::Utf8,
        dict.as_ref(),
        1,
    )?;

    Ok(ShreddedSubCol {
        in_map,
        value_null,
        values,
    })
}

/// Encodes a residual `MapUtf8` [`FlatColumn`] using `Encoding::ShreddedMap` (compact key-dict).
pub fn encode_shredded_residual_block(
    col: &FlatColumn,
    profile: WriteProfile,
) -> Result<EncodedMiniBlock, StatamError> {
    encode_shredded_residual_block_with_zm(col, profile, None)
}

fn encode_shredded_residual_block_with_zm(
    col: &FlatColumn,
    profile: WriteProfile,
    precomputed_zm: Option<&ZoneMapSummary>,
) -> Result<EncodedMiniBlock, StatamError> {
    if col.is_empty() {
        return Ok(EncodedMiniBlock {
            encoding: Encoding::ShreddedMap,
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
            let as_col = Column::Flat(col.clone());
            owned_zm = compute_zone_map(&as_col, false);
            if let Some(inline) = try_encode_const_or_null(&as_col, &DataType::MapUtf8, &owned_zm) {
                return Ok(inline);
            }
            &owned_zm
        }
    };

    let FlatData::MapUtf8(map) = col.data() else {
        return Err(StatamError::SchemaMismatch {
            reason: "expected FlatData::MapUtf8",
        });
    };

    let row_count = col.len();
    let offsets = map.offsets();
    let mut counts = Vec::with_capacity(row_count);
    for r in 0..row_count {
        counts.push(offsets[r + 1] - offsets[r]);
    }
    let cnt_bw = min_bits_u32(&counts);

    let mut raw = Vec::new();
    write_optional_nulls(col.nulls(), &mut raw);
    raw.push(cnt_bw);
    pack_u32(&counts, cnt_bw, &mut raw)?;

    let entry_count = map.entry_count();
    if entry_count > 0 {
        // Build compact key dictionary for this mini-block.
        let mut key_map: HashMap<&str, u32> = HashMap::new();
        let mut unique_keys: Vec<&str> = Vec::new();
        let mut key_codes = Vec::with_capacity(entry_count);
        for idx in 0..entry_count {
            let k = map.keys().get(idx).unwrap_or("");
            if let Some(&code) = key_map.get(k) {
                key_codes.push(code);
            } else {
                let code = unique_keys.len() as u32;
                key_map.insert(k, code);
                unique_keys.push(k);
                key_codes.push(code);
            }
        }
        let key_lens: Vec<u32> = unique_keys.iter().map(|k| k.len() as u32).collect();
        let klen_bw = min_bits_u32(&key_lens);
        let kcode_bw = min_bits_u32(&key_codes);

        raw.extend_from_slice(&(unique_keys.len() as u32).to_le_bytes());
        raw.push(klen_bw);
        pack_u32(&key_lens, klen_bw, &mut raw)?;
        let keys_raw_bytes: usize = unique_keys.iter().map(|k| k.len()).sum();
        raw.extend_from_slice(&(keys_raw_bytes as u32).to_le_bytes());
        for k in unique_keys {
            raw.extend_from_slice(k.as_bytes());
        }
        raw.push(kcode_bw);
        pack_u32(&key_codes, kcode_bw, &mut raw)?;

        if let Some(vn) = map.value_nulls() {
            raw.push(1);
            raw.extend_from_slice(&vn.to_bytes());
        } else {
            raw.push(0);
        }

        let (val_enc, val_bytes) = encode_utf8_fsst_or_plain(map.values(), profile)?;
        raw.push(val_enc as u8);
        raw.extend_from_slice(&(val_bytes.len() as u32).to_le_bytes());
        raw.extend_from_slice(&val_bytes);
    }

    let preferred_codec = preferred_block_codec(profile, raw.len(), true);

    Ok(EncodedMiniBlock {
        encoding: Encoding::ShreddedMap,
        block_flags: zm.block_flags,
        null_count: zm.null_count,
        min_inline_len: 0,
        max_inline_len: 0,
        min_inline: [0u8; 16],
        max_inline: [0u8; 16],
        raw_body: raw,
        preferred_codec,
    })
}

/// Decodes a `ShreddedMap` residual block into a `MapUtf8` [`FlatColumn`].
pub fn decode_shredded_residual_block(
    raw_body: &[u8],
    entry: &BlockEntry,
) -> Result<FlatColumn, StatamError> {
    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        return Ok(decode_const_block(entry, &DataType::MapUtf8)?.materialize(None)?);
    }
    let row_count = entry.row_count as usize;
    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty shredded residual",
            });
        }
        return Ok(FlatColumn::empty(&DataType::MapUtf8));
    }
    if entry.encoding == Encoding::Plain {
        let (nulls, body) = read_optional_nulls(raw_body, row_count, entry.block_flags)?;
        let data = decode_inner_data(
            body,
            Encoding::Plain,
            row_count,
            &DataType::MapUtf8,
            nulls.as_ref(),
        )?;
        return Ok(FlatColumn::try_new(data, nulls)?);
    }
    decode_shredded_residual_with_count(raw_body, row_count, entry.block_flags)
}

fn decode_shredded_residual_with_count(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<FlatColumn, StatamError> {
    if row_count == 0 {
        return Ok(FlatColumn::empty(&DataType::MapUtf8));
    }

    let (row_nulls, rest) = read_optional_nulls(raw_body, row_count, block_flags)?;
    let (counts, mut rest) = read_packed_counts(rest, row_count)?;
    let (offsets, entry_count) = counts_to_offsets(&counts)?;

    if entry_count == 0 {
        if !rest.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty shredded residual",
            });
        }
        return Ok(FlatColumn::try_new(
            FlatData::MapUtf8(Box::new(MapData::empty_with_rows(row_count)?)),
            row_nulls,
        )?);
    }

    if rest.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded residual key dict header",
        });
    }
    let num_keys = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    if num_keys == 0 || num_keys > entry_count {
        return Err(StatamError::Corrupt {
            reason: "invalid shredded residual num_keys",
        });
    }
    let klen_bw = rest[4];
    rest = &rest[5..];
    let klen_packed = packed_bytes_len(num_keys, klen_bw)?;
    if rest.len() < klen_packed + 4 {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded residual key lengths",
        });
    }
    let mut key_lens = Vec::with_capacity(num_keys);
    unpack_u32(&rest[..klen_packed], num_keys, klen_bw, &mut key_lens)?;
    rest = &rest[klen_packed..];

    let keys_raw_bytes = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    rest = &rest[4..];
    if keys_raw_bytes > MAX_BLOCK_RAW_BYTES || rest.len() < keys_raw_bytes + 1 {
        return Err(StatamError::Corrupt {
            reason: "truncated or oversized shredded residual keys",
        });
    }
    let keys_blob = &rest[..keys_raw_bytes];
    rest = &rest[keys_raw_bytes..];

    let mut key_offsets = Vec::with_capacity(num_keys + 1);
    key_offsets.push(0u32);
    let mut running = 0usize;
    for &kl in &key_lens {
        running = running
            .checked_add(kl as usize)
            .ok_or(StatamError::BoundsExceeded {
                reason: "shredded key offset overflow",
            })?;
        key_offsets.push(
            u32::try_from(running).map_err(|_| StatamError::BoundsExceeded {
                reason: "shredded key offset exceeds u32::MAX",
            })?,
        );
    }
    if running != keys_raw_bytes {
        return Err(StatamError::Corrupt {
            reason: "shredded key lengths do not match blob length",
        });
    }
    let key_dict = Utf8Data::try_new(key_offsets, keys_blob.to_vec())?;

    let kcode_bw = rest[0];
    rest = &rest[1..];
    let kcode_packed = packed_bytes_len(entry_count, kcode_bw)?;
    if rest.len() < kcode_packed + 1 {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded residual key codes",
        });
    }
    let mut key_codes = Vec::with_capacity(entry_count);
    unpack_u32(&rest[..kcode_packed], entry_count, kcode_bw, &mut key_codes)?;
    for &c in &key_codes {
        if (c as usize) >= num_keys {
            return Err(StatamError::Corrupt {
                reason: "shredded residual key code out of bounds",
            });
        }
    }
    rest = &rest[kcode_packed..];

    let (val_nulls, rest) = read_flag_bitmap(rest, entry_count)?;
    check_varlen_take_bytes(key_dict.offsets(), &key_codes, None)?;
    let keys_data = key_dict.take(&key_codes, None)?;
    let vals_data = decode_utf8_fsst_or_plain(rest, entry_count)?;

    let map = MapData::try_new(offsets, keys_data, vals_data, val_nulls)?;
    Ok(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(map)),
        row_nulls,
    )?)
}

trait MapDataExt {
    fn empty_with_rows(rows: usize) -> Result<MapData, StatamError>;
}

impl MapDataExt for MapData {
    fn empty_with_rows(rows: usize) -> Result<MapData, StatamError> {
        Ok(MapData::try_new(
            vec![0u32; rows + 1],
            Utf8Data::empty(),
            Utf8Data::empty(),
            None,
        )?)
    }
}

/// Encodes `List(T)`, `Events`, or `Links` into [`EncodedMiniBlock`].
pub(crate) fn encode_nested_column(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    remaining_depth: usize,
    precomputed_zm: Option<&ZoneMapSummary>,
) -> Result<EncodedMiniBlock, StatamError> {
    if col.is_empty() {
        let enc = match dt {
            DataType::List(_) => Encoding::ListEncoded,
            DataType::MapUtf8 => Encoding::ShreddedMap,
            DataType::Events => Encoding::EventsStruct,
            DataType::Links => Encoding::LinksStruct,
            _ => {
                return Err(StatamError::SchemaMismatch {
                    reason: "unexpected DataType in encode_nested_column",
                });
            }
        };
        return Ok(EncodedMiniBlock {
            encoding: enc,
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

    let owned_flat;
    let flat = match col {
        Column::Flat(f) => f,
        other => {
            owned_flat = other.materialize(None)?;
            &owned_flat
        }
    };

    match dt {
        DataType::List(inner) => {
            if remaining_depth == 0 {
                return Err(StatamError::BoundsExceeded {
                    reason: "DataType::List nesting exceeds MAX_NESTING_DEPTH",
                });
            }
            let FlatData::List {
                offsets, values, ..
            } = flat.data()
            else {
                return Err(StatamError::SchemaMismatch {
                    reason: "expected FlatData::List",
                });
            };
            let row_count = flat.len();
            let mut counts = Vec::with_capacity(row_count);
            for r in 0..row_count {
                counts.push(offsets[r + 1] - offsets[r]);
            }
            let len_bw = min_bits_u32(&counts);

            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.push(len_bw);
            pack_u32(&counts, len_bw, &mut raw)?;

            let child_len = values.len() as u32;
            let child_payload =
                encode_child_column_payload(values, inner, profile, remaining_depth - 1)?;
            let child_null_count = count_col_nulls_u32(values);

            raw.push(child_payload.encoding as u8);
            raw.push(child_payload.block_flags);
            raw.extend_from_slice(&child_len.to_le_bytes());
            raw.extend_from_slice(&child_null_count.to_le_bytes());
            raw.push(child_payload.min_inline_len);
            raw.extend_from_slice(&child_payload.min_inline);
            if let Some((sorted, dict_bytes)) = child_payload.inline_dict {
                raw.push(1);
                raw.push(u8::from(sorted));
                raw.extend_from_slice(&(dict_bytes.len() as u32).to_le_bytes());
                raw.extend_from_slice(&dict_bytes);
            } else {
                raw.push(0);
            }
            raw.extend_from_slice(&(child_payload.body.len() as u32).to_le_bytes());
            raw.extend_from_slice(&child_payload.body);

            let preferred_codec = preferred_block_codec(profile, raw.len(), true);
            Ok(EncodedMiniBlock {
                encoding: Encoding::ListEncoded,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: 0,
                max_inline_len: 0,
                min_inline: [0u8; 16],
                max_inline: [0u8; 16],
                raw_body: raw,
                preferred_codec,
            })
        }
        DataType::MapUtf8 => {
            if profile == WriteProfile::L0Fast {
                let raw = if let Some(bm) = flat.nulls() {
                    let bm_bytes = bm.to_bytes();
                    let body = encode_plain_body(flat)?;
                    let mut out = Vec::with_capacity(bm_bytes.len() + body.len());
                    out.extend_from_slice(&bm_bytes);
                    out.extend_from_slice(&body);
                    out
                } else {
                    encode_plain_body(flat)?
                };
                let preferred_codec = preferred_block_codec(profile, raw.len(), true);
                Ok(EncodedMiniBlock {
                    encoding: Encoding::Plain,
                    block_flags: zm.block_flags,
                    null_count: zm.null_count,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    min_inline: [0u8; 16],
                    max_inline: [0u8; 16],
                    raw_body: raw,
                    preferred_codec,
                })
            } else {
                encode_shredded_residual_block_with_zm(flat, profile, Some(zm))
            }
        }
        DataType::Events => {
            let FlatData::Events(ev) = flat.data() else {
                return Err(StatamError::SchemaMismatch {
                    reason: "expected FlatData::Events",
                });
            };
            let row_count = flat.len();
            let offsets = ev.offsets();
            let mut counts = Vec::with_capacity(row_count);
            for r in 0..row_count {
                counts.push(offsets[r + 1] - offsets[r]);
            }
            let len_bw = min_bits_u32(&counts);

            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.push(len_bw);
            pack_u32(&counts, len_bw, &mut raw)?;

            let event_count = ev.event_count();
            if event_count > 0 {
                let times_flat = FlatColumn::try_new(FlatData::I64(ev.times().to_vec()), None)?;
                let (t_enc, t_bytes) = if let Some(db) = encode_delta_bitpack_body(&times_flat)? {
                    if let Some(fb) = encode_for_bitpack_body(&times_flat)?
                        && fb.len() < db.len()
                    {
                        (Encoding::ForBitpack, fb)
                    } else {
                        (Encoding::DeltaBitpack, db)
                    }
                } else {
                    (Encoding::Plain, encode_plain_body(&times_flat)?)
                };
                raw.push(t_enc as u8);
                raw.extend_from_slice(&(t_bytes.len() as u32).to_le_bytes());
                raw.extend_from_slice(&t_bytes);

                let names_flat = FlatColumn::try_new(FlatData::Utf8(ev.names().clone()), None)?;
                let builder = match profile {
                    WriteProfile::L0Fast => None,
                    WriteProfile::L1Compact => Some(DictBuilder::l1_compact()),
                };
                if let Some(b) = builder
                    && let Some((dict, codes)) = b.build(&names_flat)?
                    && dict.len() * 2 <= event_count
                {
                    let dict_bytes = dict.encode(Codec::None, 0)?;
                    let codes_bytes = encode_dict_codes_body(&codes, None)?;
                    raw.push(Encoding::Dict as u8);
                    raw.push(u8::from(dict.is_sorted()));
                    raw.extend_from_slice(&(dict_bytes.len() as u32).to_le_bytes());
                    raw.extend_from_slice(&dict_bytes);
                    raw.extend_from_slice(&(codes_bytes.len() as u32).to_le_bytes());
                    raw.extend_from_slice(&codes_bytes);
                } else {
                    let (n_enc, n_bytes) = encode_utf8_fsst_or_plain(ev.names(), profile)?;
                    raw.push(n_enc as u8);
                    raw.extend_from_slice(&(n_bytes.len() as u32).to_le_bytes());
                    raw.extend_from_slice(&n_bytes);
                }

                if let Some(an) = ev.attribute_nulls() {
                    raw.push(1);
                    raw.extend_from_slice(&an.to_bytes());
                } else {
                    raw.push(0);
                }

                let (a_enc, a_bytes) = encode_utf8_fsst_or_plain(ev.attributes(), profile)?;
                raw.push(a_enc as u8);
                raw.extend_from_slice(&(a_bytes.len() as u32).to_le_bytes());
                raw.extend_from_slice(&a_bytes);
            }

            let preferred_codec = preferred_block_codec(profile, raw.len(), true);
            Ok(EncodedMiniBlock {
                encoding: Encoding::EventsStruct,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: 0,
                max_inline_len: 0,
                min_inline: [0u8; 16],
                max_inline: [0u8; 16],
                raw_body: raw,
                preferred_codec,
            })
        }
        DataType::Links => {
            let FlatData::Links(lk) = flat.data() else {
                return Err(StatamError::SchemaMismatch {
                    reason: "expected FlatData::Links",
                });
            };
            let row_count = flat.len();
            let offsets = lk.offsets();
            let mut counts = Vec::with_capacity(row_count);
            for r in 0..row_count {
                counts.push(offsets[r + 1] - offsets[r]);
            }
            let len_bw = min_bits_u32(&counts);

            let mut raw = Vec::new();
            write_optional_nulls(flat.nulls(), &mut raw);
            raw.push(len_bw);
            pack_u32(&counts, len_bw, &mut raw)?;

            let link_count = lk.link_count();
            if link_count > 0 {
                for tid in lk.trace_ids() {
                    raw.extend_from_slice(tid);
                }
                for sid in lk.span_ids() {
                    raw.extend_from_slice(sid);
                }
                if let Some(an) = lk.attribute_nulls() {
                    raw.push(1);
                    raw.extend_from_slice(&an.to_bytes());
                } else {
                    raw.push(0);
                }
                let (a_enc, a_bytes) = encode_utf8_fsst_or_plain(lk.attributes(), profile)?;
                raw.push(a_enc as u8);
                raw.extend_from_slice(&(a_bytes.len() as u32).to_le_bytes());
                raw.extend_from_slice(&a_bytes);
            }

            let preferred_codec = preferred_block_codec(profile, raw.len(), true);
            Ok(EncodedMiniBlock {
                encoding: Encoding::LinksStruct,
                block_flags: zm.block_flags,
                null_count: zm.null_count,
                min_inline_len: 0,
                max_inline_len: 0,
                min_inline: [0u8; 16],
                max_inline: [0u8; 16],
                raw_body: raw,
                preferred_codec,
            })
        }
        _ => Err(StatamError::SchemaMismatch {
            reason: "narrow DataType passed to encode_nested_column",
        }),
    }
}

/// Decodes a `ListEncoded`, `EventsStruct`, `LinksStruct`, or `ShreddedMap` mini-block.
pub(crate) fn decode_nested_column(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    remaining_depth: usize,
) -> Result<Column, StatamError> {
    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        return decode_const_block(entry, dt);
    }

    let row_count = entry.row_count as usize;
    if row_count == 0 {
        if !raw_body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty nested block",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }

    match (entry.encoding, dt) {
        (Encoding::ListEncoded, DataType::List(inner)) => decode_list_body(
            raw_body,
            row_count,
            entry.block_flags,
            inner,
            remaining_depth,
        ),
        (Encoding::ShreddedMap, DataType::MapUtf8) => Ok(Column::Flat(
            decode_shredded_residual_block(raw_body, entry)?,
        )),
        (Encoding::EventsStruct, DataType::Events) => {
            decode_events_body(raw_body, row_count, entry.block_flags)
        }
        (Encoding::LinksStruct, DataType::Links) => {
            decode_links_body(raw_body, row_count, entry.block_flags)
        }
        _ => Err(StatamError::Corrupt {
            reason: "incompatible nested encoding for DataType",
        }),
    }
}

fn decode_list_body(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    inner: &DataType,
    remaining_depth: usize,
) -> Result<Column, StatamError> {
    if remaining_depth == 0 {
        return Err(StatamError::BoundsExceeded {
            reason: "DataType::List nesting exceeds MAX_NESTING_DEPTH",
        });
    }

    let (row_nulls, rest) = read_optional_nulls(raw_body, row_count, block_flags)?;
    let (counts, rest) = read_packed_counts(rest, row_count)?;
    let (offsets, total_child) = counts_to_offsets(&counts)?;

    if rest.len() < 28 {
        return Err(StatamError::Corrupt {
            reason: "truncated ListEncoded child header",
        });
    }
    let child_enc = Encoding::from_u8(rest[0])?;
    let child_flags = rest[1];
    let child_len = u32::from_le_bytes([rest[2], rest[3], rest[4], rest[5]]) as usize;
    let child_null_count = u32::from_le_bytes([rest[6], rest[7], rest[8], rest[9]]);
    let child_min_inline_len = rest[10];
    if child_len != total_child {
        return Err(StatamError::Corrupt {
            reason: "ListEncoded child_len mismatch with sum of counts",
        });
    }
    if child_null_count as usize > child_len || child_min_inline_len > 16 {
        return Err(StatamError::Corrupt {
            reason: "invalid ListEncoded child metadata",
        });
    }
    let mut child_min_inline = [0u8; 16];
    child_min_inline.copy_from_slice(&rest[11..27]);
    let has_dict = rest[27];
    let mut pos = 28usize;

    let dict = match has_dict {
        0 => None,
        1 => {
            if rest.len() < pos + 5 {
                return Err(StatamError::Corrupt {
                    reason: "truncated ListEncoded inline dict header",
                });
            }
            let sorted = rest[pos] != 0;
            let dict_len =
                u32::from_le_bytes([rest[pos + 1], rest[pos + 2], rest[pos + 3], rest[pos + 4]])
                    as usize;
            pos += 5;
            if dict_len > MAX_DICT_BYTES || rest.len().saturating_sub(pos) < dict_len {
                return Err(StatamError::Corrupt {
                    reason: "truncated or oversized ListEncoded inline dict",
                });
            }
            let d = FileDict::decode(&rest[pos..pos + dict_len], sorted)?;
            pos += dict_len;
            Some(d)
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid ListEncoded has_dict flag",
            });
        }
    };

    if rest.len().saturating_sub(pos) < 4 {
        return Err(StatamError::Corrupt {
            reason: "truncated ListEncoded child_body_len",
        });
    }
    let child_body_len =
        u32::from_le_bytes([rest[pos], rest[pos + 1], rest[pos + 2], rest[pos + 3]]) as usize;
    pos += 4;
    if rest.len().saturating_sub(pos) != child_body_len {
        return Err(StatamError::Corrupt {
            reason: "ListEncoded child_body_len mismatch",
        });
    }
    let child_body = &rest[pos..];

    let child_col = decode_child_column(
        child_body,
        child_enc,
        child_flags,
        child_len,
        child_min_inline_len,
        child_min_inline,
        inner,
        dict.as_ref(),
        remaining_depth - 1,
    )?;

    let list_flat = FlatColumn::try_new(
        FlatData::List {
            item_type: inner.clone(),
            offsets,
            values: Box::new(child_col),
        },
        row_nulls,
    )?;
    Ok(Column::Flat(list_flat))
}

#[allow(clippy::too_many_arguments)]
fn decode_child_column(
    body: &[u8],
    enc: Encoding,
    flags: u8,
    len: usize,
    min_inline_len: u8,
    min_inline: [u8; 16],
    dt: &DataType,
    dict: Option<&FileDict>,
    remaining_depth: usize,
) -> Result<Column, StatamError> {
    if len == 0 {
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }
    if enc == Encoding::ConstNull {
        return Ok(Column::Const {
            value: ScalarValue::Null(dt.clone()),
            len,
        });
    }
    if enc == Encoding::Const {
        let fake = BlockEntry {
            first_row: 0,
            row_count: 1,
            null_count: 0,
            offset: 0,
            stored_len: 0,
            raw_len: 0,
            encoding: Encoding::Const,
            codec: Codec::None,
            block_flags: flags,
            min_inline_len,
            max_inline_len: min_inline_len,
            reserved: [0; 3],
            min_inline,
            max_inline: min_inline,
        };
        let mut c = decode_const_block(&fake, dt)?;
        if let Column::Const { len: l, .. } = &mut c {
            *l = len;
        }
        return Ok(c);
    }

    if enc == Encoding::Dict {
        let Some(d) = dict else {
            return Err(StatamError::Corrupt {
                reason: "ListEncoded Dict child missing inline dict",
            });
        };
        if d.values().data_type() != *dt {
            return Err(StatamError::SchemaMismatch {
                reason: "child FileDict DataType mismatch",
            });
        }
        let (nulls, after_nulls) = read_optional_nulls(body, len, flags)?;
        return decode_dict_column(after_nulls, len, nulls, d);
    }

    match dt {
        DataType::List(inner) if enc == Encoding::ListEncoded => {
            decode_list_body(body, len, flags, inner, remaining_depth)
        }
        DataType::MapUtf8 if enc == Encoding::ShreddedMap => Ok(Column::Flat(
            decode_shredded_residual_with_count(body, len, flags)?,
        )),
        DataType::MapUtf8 if enc == Encoding::Plain => {
            let (nulls, after_nulls) = read_optional_nulls(body, len, flags)?;
            let data = decode_inner_data(after_nulls, Encoding::Plain, len, dt, nulls.as_ref())?;
            Ok(Column::Flat(FlatColumn::try_new(data, nulls)?))
        }
        DataType::Events if enc == Encoding::EventsStruct => decode_events_body(body, len, flags),
        DataType::Links if enc == Encoding::LinksStruct => decode_links_body(body, len, flags),
        DataType::MapUtf8 | DataType::Events | DataType::Links => Err(StatamError::Corrupt {
            reason: "incompatible nested encoding for DataType",
        }),
        _ => {
            if enc == Encoding::Rle {
                return decode_rle_block(body, len, flags, dt);
            }
            let (nulls, after_nulls) = read_optional_nulls(body, len, flags)?;
            let data = decode_inner_data(after_nulls, enc, len, dt, nulls.as_ref())?;
            Ok(Column::Flat(FlatColumn::try_new(data, nulls)?))
        }
    }
}

struct ChildEncodedPayload {
    encoding: Encoding,
    block_flags: u8,
    min_inline_len: u8,
    min_inline: [u8; 16],
    inline_dict: Option<(bool, Vec<u8>)>,
    body: Vec<u8>,
}

fn encode_child_column_payload(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    remaining_depth: usize,
) -> Result<ChildEncodedPayload, StatamError> {
    if col.is_empty() {
        return Ok(ChildEncodedPayload {
            encoding: Encoding::Plain,
            block_flags: 0,
            min_inline_len: 0,
            min_inline: [0u8; 16],
            inline_dict: None,
            body: Vec::new(),
        });
    }

    // Try Dict encoding for Dict column or Utf8/Binary/FixedBytes leaf inside List/SubCol.
    if let Column::Dict(d) = col {
        let dict = FileDict::from_flat(d.values().clone(), false)?;
        let dict_bytes = dict.encode(Codec::None, 0)?;
        let mut body = Vec::new();
        write_optional_nulls(d.nulls(), &mut body);
        body.extend_from_slice(&encode_dict_codes_body(d.codes(), d.nulls())?);
        let flags = if d.nulls().is_some() {
            BLOCK_FLAG_HAS_NULLS
        } else {
            0
        };
        return Ok(ChildEncodedPayload {
            encoding: Encoding::Dict,
            block_flags: flags,
            min_inline_len: 0,
            min_inline: [0u8; 16],
            inline_dict: Some((dict.is_sorted(), dict_bytes)),
            body,
        });
    }

    if profile == WriteProfile::L1Compact
        && matches!(
            dt,
            DataType::Utf8 | DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16
        )
        && let Column::Flat(f) = col
        && f.len() >= 4
        && !f.all_null_check()
    {
        let builder = match profile {
            WriteProfile::L0Fast => DictBuilder::l0_fast(),
            WriteProfile::L1Compact => DictBuilder::l1_compact(),
        };
        if let Some((dict, codes)) = builder.build(f)?
            && dict.len() * 2 <= f.len()
        {
            let dict_bytes = dict.encode(Codec::None, 0)?;
            let mut body = Vec::new();
            write_optional_nulls(f.nulls(), &mut body);
            body.extend_from_slice(&encode_dict_codes_body(&codes, f.nulls())?);
            let flags = if f.nulls().is_some() {
                BLOCK_FLAG_HAS_NULLS
            } else {
                0
            };
            return Ok(ChildEncodedPayload {
                encoding: Encoding::Dict,
                block_flags: flags,
                min_inline_len: 0,
                min_inline: [0u8; 16],
                inline_dict: Some((dict.is_sorted(), dict_bytes)),
                body,
            });
        }
    }

    let enc_block = if matches!(
        dt,
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links
    ) {
        encode_nested_column(col, dt, profile, remaining_depth, None)?
    } else {
        crate::codec::numeric::encode_narrow_column(col, dt, profile, None)?
    };
    Ok(ChildEncodedPayload {
        encoding: enc_block.encoding,
        block_flags: enc_block.block_flags,
        min_inline_len: enc_block.min_inline_len,
        min_inline: enc_block.min_inline,
        inline_dict: None,
        body: enc_block.raw_body,
    })
}

trait AllNullHelper {
    fn all_null_check(&self) -> bool;
}

impl AllNullHelper for FlatColumn {
    fn all_null_check(&self) -> bool {
        self.nulls().is_some_and(|bm| bm.all_zeros())
    }
}

fn count_col_nulls_u32(col: &Column) -> u32 {
    match col {
        Column::Flat(f) => f.nulls().map_or_else(
            || {
                if matches!(f.data(), FlatData::Null(_)) {
                    f.len() as u32
                } else {
                    0
                }
            },
            |bm| bm.count_zeros() as u32,
        ),
        Column::Const { value, len } => {
            if value.is_null() {
                *len as u32
            } else {
                0
            }
        }
        Column::Dict(d) => (0..d.len()).filter(|&i| d.is_null(i)).count() as u32,
        Column::Rle(r) => {
            if !r.has_nulls() {
                return 0;
            }
            let mut n = 0u32;
            let mut prev = 0u32;
            for (i, &end) in r.run_ends().iter().enumerate() {
                if r.values().is_null(i) {
                    n += end - prev;
                }
                prev = end;
            }
            n
        }
    }
}

fn decode_events_body(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<Column, StatamError> {
    let (row_nulls, rest) = read_optional_nulls(raw_body, row_count, block_flags)?;
    let (counts, mut rest) = read_packed_counts(rest, row_count)?;
    let (offsets, event_count) = counts_to_offsets(&counts)?;

    if event_count == 0 {
        if !rest.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty EventsStruct",
            });
        }
        let ev = EventsData::try_new(
            offsets,
            Vec::new(),
            Utf8Data::empty(),
            Utf8Data::empty(),
            None,
        )?;
        return Ok(Column::Flat(FlatColumn::try_new(
            FlatData::Events(Box::new(ev)),
            row_nulls,
        )?));
    }

    // 1. times
    if rest.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated EventsStruct times header",
        });
    }
    let t_enc = Encoding::from_u8(rest[0])?;
    let t_len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
    rest = &rest[5..];
    if rest.len() < t_len {
        return Err(StatamError::Corrupt {
            reason: "truncated EventsStruct times payload",
        });
    }
    let times_data = decode_inner_data(&rest[..t_len], t_enc, event_count, &DataType::I64, None)?;
    let FlatData::I64(times) = times_data else {
        unreachable!();
    };
    rest = &rest[t_len..];

    // 2. names
    if rest.is_empty() {
        return Err(StatamError::Corrupt {
            reason: "truncated EventsStruct names header",
        });
    }
    let n_enc = Encoding::from_u8(rest[0])?;
    rest = &rest[1..];
    let names = if n_enc == Encoding::Dict {
        if rest.len() < 5 {
            return Err(StatamError::Corrupt {
                reason: "truncated EventsStruct names dict header",
            });
        }
        let sorted = rest[0] != 0;
        let d_len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
        rest = &rest[5..];
        if d_len > MAX_DICT_BYTES || rest.len() < d_len + 4 {
            return Err(StatamError::Corrupt {
                reason: "truncated EventsStruct names dict bytes",
            });
        }
        let dict = FileDict::decode(&rest[..d_len], sorted)?;
        rest = &rest[d_len..];
        let c_len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        rest = &rest[4..];
        if rest.len() < c_len {
            return Err(StatamError::Corrupt {
                reason: "truncated EventsStruct names codes",
            });
        }
        let codes = decode_dict_codes_body(&rest[..c_len], event_count, dict.len(), None)?;
        rest = &rest[c_len..];
        let FlatData::Utf8(dict_u) = dict.values().data() else {
            return Err(StatamError::Corrupt {
                reason: "EventsStruct names dict is not Utf8",
            });
        };
        check_varlen_take_bytes(dict_u.offsets(), &codes, None)?;
        dict_u.take(&codes, None)?
    } else {
        if rest.len() < 4 {
            return Err(StatamError::Corrupt {
                reason: "truncated EventsStruct names length",
            });
        }
        let n_len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        rest = &rest[4..];
        if rest.len() < n_len {
            return Err(StatamError::Corrupt {
                reason: "truncated EventsStruct names payload",
            });
        }
        let n_data = decode_inner_data(&rest[..n_len], n_enc, event_count, &DataType::Utf8, None)?;
        rest = &rest[n_len..];
        let FlatData::Utf8(u) = n_data else {
            unreachable!();
        };
        u
    };

    // 3. attr_nulls & attributes
    let (attr_nulls, rest) = read_flag_bitmap(rest, event_count)?;
    if rest.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated EventsStruct attrs header",
        });
    }
    let a_enc = Encoding::from_u8(rest[0])?;
    let a_len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
    let rest = &rest[5..];
    if rest.len() != a_len {
        return Err(StatamError::Corrupt {
            reason: "EventsStruct attrs length mismatch",
        });
    }
    let a_data = decode_inner_data(rest, a_enc, event_count, &DataType::Utf8, None)?;
    let FlatData::Utf8(attributes) = a_data else {
        unreachable!();
    };

    let ev = EventsData::try_new(offsets, times, names, attributes, attr_nulls)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Events(Box::new(ev)),
        row_nulls,
    )?))
}

fn decode_links_body(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<Column, StatamError> {
    let (row_nulls, rest) = read_optional_nulls(raw_body, row_count, block_flags)?;
    let (counts, mut rest) = read_packed_counts(rest, row_count)?;
    let (offsets, link_count) = counts_to_offsets(&counts)?;

    if link_count == 0 {
        if !rest.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty LinksStruct",
            });
        }
        let lk = LinksData::try_new(offsets, Vec::new(), Vec::new(), Utf8Data::empty(), None)?;
        return Ok(Column::Flat(FlatColumn::try_new(
            FlatData::Links(Box::new(lk)),
            row_nulls,
        )?));
    }

    let tid_bytes = link_count
        .checked_mul(16)
        .ok_or(StatamError::BoundsExceeded {
            reason: "LinksStruct trace_ids overflow",
        })?;
    let sid_bytes = link_count
        .checked_mul(8)
        .ok_or(StatamError::BoundsExceeded {
            reason: "LinksStruct span_ids overflow",
        })?;
    let ids_total = tid_bytes
        .checked_add(sid_bytes)
        .ok_or(StatamError::BoundsExceeded {
            reason: "LinksStruct ids total overflow",
        })?;
    if ids_total > MAX_BLOCK_RAW_BYTES || rest.len() < ids_total {
        return Err(StatamError::Corrupt {
            reason: "truncated LinksStruct ids",
        });
    }

    let trace_ids = rest[..tid_bytes].as_chunks::<16>().0.to_vec();
    rest = &rest[tid_bytes..];

    let span_ids = rest[..sid_bytes].as_chunks::<8>().0.to_vec();
    rest = &rest[sid_bytes..];

    let (attr_nulls, rest) = read_flag_bitmap(rest, link_count)?;
    if rest.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated LinksStruct attrs header",
        });
    }
    let a_enc = Encoding::from_u8(rest[0])?;
    let a_len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
    let rest = &rest[5..];
    if rest.len() != a_len {
        return Err(StatamError::Corrupt {
            reason: "LinksStruct attrs length mismatch",
        });
    }
    let a_data = decode_inner_data(rest, a_enc, link_count, &DataType::Utf8, None)?;
    let FlatData::Utf8(attributes) = a_data else {
        unreachable!();
    };

    let lk = LinksData::try_new(offsets, trace_ids, span_ids, attributes, attr_nulls)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Links(Box::new(lk)),
        row_nulls,
    )?))
}

fn read_packed_counts(src: &[u8], row_count: usize) -> Result<(Vec<u32>, &[u8]), StatamError> {
    let Some(&bw) = src.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated packed counts bit_width",
        });
    };
    if bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid packed counts bit_width",
        });
    }
    let packed_len = packed_bytes_len(row_count, bw)?;
    let rest = &src[1..];
    if rest.len() < packed_len {
        return Err(StatamError::Corrupt {
            reason: "truncated packed counts",
        });
    }
    let mut counts = Vec::with_capacity(row_count);
    unpack_u32(&rest[..packed_len], row_count, bw, &mut counts)?;
    Ok((counts, &rest[packed_len..]))
}

fn counts_to_offsets(counts: &[u32]) -> Result<(Vec<u32>, usize), StatamError> {
    let mut offsets = Vec::with_capacity(counts.len() + 1);
    offsets.push(0u32);
    let mut total = 0usize;
    for &c in counts {
        total = total
            .checked_add(c as usize)
            .ok_or(StatamError::BoundsExceeded {
                reason: "nested total item count overflow",
            })?;
        if total > MAX_BLOCK_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "nested total item count exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        offsets.push(
            u32::try_from(total).map_err(|_| StatamError::BoundsExceeded {
                reason: "nested offset exceeds u32::MAX",
            })?,
        );
    }
    Ok((offsets, total))
}

fn read_flag_bitmap(src: &[u8], len: usize) -> Result<(Option<Bitmap>, &[u8]), StatamError> {
    let Some(&flag) = src.first() else {
        return Err(StatamError::Corrupt {
            reason: "truncated nested bitmap flag",
        });
    };
    let rest = &src[1..];
    match flag {
        0 => Ok((None, rest)),
        1 => {
            let byte_len = len.div_ceil(8);
            if rest.len() < byte_len {
                return Err(StatamError::Corrupt {
                    reason: "truncated nested bitmap bytes",
                });
            }
            Ok((
                Some(Bitmap::from_bytes(&rest[..byte_len], len)?),
                &rest[byte_len..],
            ))
        }
        _ => Err(StatamError::Corrupt {
            reason: "invalid nested bitmap flag",
        }),
    }
}

fn encode_utf8_fsst_or_plain(
    u: &Utf8Data,
    profile: WriteProfile,
) -> Result<(Encoding, Vec<u8>), StatamError> {
    if profile == WriteProfile::L1Compact && u.bytes().len() <= 16 * 1024 {
        let mut slices = Vec::with_capacity(u.len());
        for i in 0..u.len() {
            slices.push(u.get(i).map_or(&[][..], str::as_bytes));
        }
        if let Some(fsst) = encode_fsst_body(&slices)? {
            return Ok((Encoding::Fsst, fsst));
        }
    }
    Ok((Encoding::Plain, encode_plain_utf8(u)))
}

fn decode_utf8_fsst_or_plain(src: &[u8], row_count: usize) -> Result<Utf8Data, StatamError> {
    if src.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated nested utf8 header",
        });
    }
    let enc = Encoding::from_u8(src[0])?;
    let len = u32::from_le_bytes([src[1], src[2], src[3], src[4]]) as usize;
    let body = &src[5..];
    if body.len() != len {
        return Err(StatamError::Corrupt {
            reason: "nested utf8 payload length mismatch",
        });
    }
    match enc {
        Encoding::Fsst => decode_fsst_utf8(body, row_count),
        Encoding::Plain => {
            let data = decode_inner_data(body, Encoding::Plain, row_count, &DataType::Utf8, None)?;
            let FlatData::Utf8(u) = data else {
                unreachable!();
            };
            Ok(u)
        }
        _ => Err(StatamError::Corrupt {
            reason: "unsupported nested utf8 encoding",
        }),
    }
}
