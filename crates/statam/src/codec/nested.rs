//! Structured & nested column codecs: `ListEncoded`, `EventsStruct`, `LinksStruct`,
//! and `ShreddedMap` (with promoted key sub-columns, key-dict residual, and
//! `decode_single_map_key` fast path).

use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasherDefault;

use cetra_column::{
    BinaryData, Bitmap, Column, DataType, DictColumn, EventsData, FlatColumn, FlatData, LinksData,
    MapData, RleColumn, ScalarValue, Utf8Data,
};

use crate::codec::dict::{
    DICT_PASSTHROUGH_MAX_BYTES, DictBuilder, DictValueKind, FastSliceHasher, FileDict,
    decode_dict_codes_body, decode_dict_column, decode_dict_selected,
    dict_logical_materialized_bytes, encode_dict_codes_body, materialize_from_dict,
    sum_varlen_take_bytes,
};
use crate::codec::fsst::{
    FsstBlockView, FsstSubchunkedView, decode_fsst_binary_subchunked, decode_fsst_utf8,
    decode_fsst_utf8_selected_with_flag, decode_fsst_utf8_subchunked, encode_fsst_body,
};
use crate::codec::numeric::{
    decode_const_block, decode_inner_data, decode_plain_map_selected, decode_plain_varlen_selected,
    decode_rle_block, encode_delta_bitpack_body, encode_for_bitpack_body, encode_plain_body,
    encode_plain_utf8, read_optional_nulls, read_u32_le_at, try_encode_const_or_null,
    validate_plain_offsets_raw, write_optional_nulls,
};
use crate::codec::zonemap::{ZoneMapSummary, compute_zone_map, count_col_nulls};
use crate::codec::{EncodedMiniBlock, WriteProfile, preferred_block_codec};
use crate::format::{
    BLOCK_FLAG_FSST_SUBCHUNKED, BLOCK_FLAG_HAS_NULLS, BlockEntry, Codec, Encoding,
    KNOWN_BLOCK_FLAGS, MAX_SHREDDED_KEYS,
};
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

    let mut counts: HashMap<&str, (usize, usize), BuildHasherDefault<FastSliceHasher>> =
        HashMap::default();
    let offsets = map.offsets();
    let keys = map.keys();

    let step = if total_rows > 8_192 {
        (total_rows / 4_096).max(1)
    } else {
        1
    };
    let mut sampled_rows = 0usize;
    for row in (0..total_rows).step_by(step) {
        sampled_rows += 1;
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
            (cnt.saturating_mul(SHRED_MIN_PCT_DENOM) >= sampled_rows).then_some((k, cnt))
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
    let mut key_to_idx: HashMap<&str, usize, BuildHasherDefault<FastSliceHasher>> =
        HashMap::with_capacity_and_hasher(k_count, BuildHasherDefault::default());
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
        let mut any_spill = false;

        for idx in start..end {
            let k = map_keys.get(idx).unwrap_or("");
            if let Some(&pidx) = key_to_idx.get(k) {
                if row_counts[pidx] == 0 {
                    touched.push(pidx);
                    first_entry_idx[pidx] = idx;
                }
                row_counts[pidx] = row_counts[pidx].saturating_add(1);
                if row_counts[pidx] > 1 {
                    any_spill = true;
                }
            } else {
                any_spill = true;
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
        if any_spill {
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

/// Zero-materialization borrowed view over a `Utf8` [`Column`] for `O(1)` per-row indexing
/// across `Flat`, `Dict`, `Rle`, and `Const` sub-columns.
enum SubColUtf8Ref<'a> {
    Flat(&'a Utf8Data, Option<&'a Bitmap>),
    Dict {
        dict_vals: &'a Utf8Data,
        dict_nulls: Option<&'a Bitmap>,
        codes: &'a [u32],
        code_nulls: Option<&'a Bitmap>,
    },
    Rle {
        run_vals: &'a Utf8Data,
        run_nulls: Option<&'a Bitmap>,
        run_ends: &'a [u32],
    },
    Const(Option<&'a str>),
}

impl<'a> SubColUtf8Ref<'a> {
    fn from_column(col: &'a Column) -> Result<Self, StatamError> {
        match col {
            Column::Flat(fc) => {
                let FlatData::Utf8(u) = fc.data() else {
                    return Err(StatamError::SchemaMismatch {
                        reason: "shredded sub-column values must be Utf8",
                    });
                };
                Ok(Self::Flat(u, fc.nulls()))
            }
            Column::Dict(dc) => {
                let FlatData::Utf8(u) = dc.values().data() else {
                    return Err(StatamError::SchemaMismatch {
                        reason: "shredded sub-column values must be Utf8",
                    });
                };
                Ok(Self::Dict {
                    dict_vals: u,
                    dict_nulls: dc.values().nulls(),
                    codes: dc.codes(),
                    code_nulls: dc.nulls(),
                })
            }
            Column::Rle(rc) => {
                let FlatData::Utf8(u) = rc.values().data() else {
                    return Err(StatamError::SchemaMismatch {
                        reason: "shredded sub-column values must be Utf8",
                    });
                };
                Ok(Self::Rle {
                    run_vals: u,
                    run_nulls: rc.values().nulls(),
                    run_ends: rc.run_ends(),
                })
            }
            Column::Const { value, .. } => match value {
                ScalarValue::Utf8(s) => Ok(Self::Const(Some(s.as_str()))),
                ScalarValue::Null(DataType::Utf8) => Ok(Self::Const(None)),
                _ => Err(StatamError::SchemaMismatch {
                    reason: "shredded sub-column values must be Utf8",
                }),
            },
        }
    }

    #[inline]
    fn get(&self, row: usize) -> &'a str {
        match self {
            Self::Flat(u, nulls) => {
                if nulls.is_some_and(|bm| !bm.get(row)) {
                    ""
                } else {
                    u.get(row).unwrap_or("")
                }
            }
            Self::Dict {
                dict_vals,
                dict_nulls,
                codes,
                code_nulls,
            } => {
                if code_nulls.is_some_and(|bm| !bm.get(row)) {
                    return "";
                }
                let code = codes[row] as usize;
                if dict_nulls.is_some_and(|bm| !bm.get(code)) {
                    ""
                } else {
                    dict_vals.get(code).unwrap_or("")
                }
            }
            Self::Rle {
                run_vals,
                run_nulls,
                run_ends,
            } => {
                let run_idx = run_ends.partition_point(|&end| (end as usize) <= row);
                if run_nulls.is_some_and(|bm| !bm.get(run_idx)) {
                    ""
                } else {
                    run_vals.get(run_idx).unwrap_or("")
                }
            }
            Self::Const(opt) => opt.unwrap_or(""),
        }
    }
}

type ResidualMapRef<'a> = (&'a MapData, Option<&'a Bitmap>);
type UnshredInputs<'a> = (Option<ResidualMapRef<'a>>, Vec<SubColUtf8Ref<'a>>);
type SelectedVarlenResult = (Vec<u32>, Vec<u8>, Option<Bitmap>);

fn validate_unshred_inputs<'a>(
    row_count: usize,
    row_nulls: Option<&Bitmap>,
    residual: Option<&'a FlatColumn>,
    subcols: &[(&'a str, &'a ShreddedSubCol)],
) -> Result<UnshredInputs<'a>, StatamError> {
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

    let mut sub_refs = Vec::with_capacity(subcols.len());
    for (_, sub) in subcols {
        sub_refs.push(SubColUtf8Ref::from_column(&sub.values)?);
    }

    Ok((res_map, sub_refs))
}

#[inline]
fn merge_unshred_row<'a>(
    row: usize,
    subcols: &[(&'a str, &ShreddedSubCol)],
    sub_refs: &[SubColUtf8Ref<'a>],
    res_map: Option<(&'a MapData, Option<&Bitmap>)>,
    out_keys: &mut Vec<&'a str>,
    out_vals: &mut Vec<&'a str>,
    out_val_valid: &mut Vec<bool>,
) {
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
            let v = if valid { sub_refs[p_idx].get(row) } else { "" };
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
            let v = if valid { sub_refs[p_idx].get(row) } else { "" };
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
    let (res_map, sub_refs) = validate_unshred_inputs(row_count, row_nulls, residual, subcols)?;

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

        merge_unshred_row(
            row,
            subcols,
            &sub_refs,
            res_map,
            &mut out_keys,
            &mut out_vals,
            &mut out_val_valid,
        );

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

/// Reconstructs a `MapUtf8` [`FlatColumn`] from an optional residual map and promoted
/// sub-columns, merging only rows selected by `mask` when `mask` is sparse
/// (`!mask.all_ones() && mask.count_ones().saturating_mul(16) < row_count`).
///
/// Inactive rows receive empty map slices and remain valid.
pub fn unshred_map_selected(
    row_count: usize,
    row_nulls: Option<&Bitmap>,
    residual: Option<&FlatColumn>,
    subcols: &[(&str, &ShreddedSubCol)],
    mask: &Bitmap,
) -> Result<FlatColumn, StatamError> {
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if mask.all_ones() || mask.count_ones().saturating_mul(16) >= row_count {
        return unshred_map(row_count, row_nulls, residual, subcols);
    }

    let (res_map, sub_refs) = validate_unshred_inputs(row_count, row_nulls, residual, subcols)?;

    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut out_keys: Vec<&str> = Vec::new();
    let mut out_vals: Vec<&str> = Vec::new();
    let mut out_val_valid: Vec<bool> = Vec::new();
    let mut out_nulls: Option<Bitmap> = None;

    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        let cur_off = out_keys.len() as u32;
        if out_offsets.len() <= row {
            out_offsets.resize(row + 1, cur_off);
        }

        let is_row_null = row_nulls.is_some_and(|bm| !bm.get(row))
            || res_map.is_some_and(|(_, rn)| rn.is_some_and(|bm| !bm.get(row)));
        if is_row_null {
            let bm = out_nulls.get_or_insert_with(|| Bitmap::new_ones(row_count));
            let _ = bm.set(row, false);
            out_offsets.push(cur_off);
            continue;
        }

        merge_unshred_row(
            row,
            subcols,
            &sub_refs,
            res_map,
            &mut out_keys,
            &mut out_vals,
            &mut out_val_valid,
        );

        let end_u32 = u32::try_from(out_keys.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "unshred_map entry count exceeds u32::MAX",
        })?;
        out_offsets.push(end_u32);
    }

    let tail_off = out_keys.len() as u32;
    if out_offsets.len() < row_count + 1 {
        out_offsets.resize(row_count + 1, tail_off);
    }

    let keys_data = Utf8Data::try_from_slices(out_keys)?;
    let vals_data = Utf8Data::try_from_slices(out_vals)?;
    let val_nulls = if out_val_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_val_valid))
    };
    let map = MapData::try_new(out_offsets, keys_data, vals_data, val_nulls)?;
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

    let sub_ref = SubColUtf8Ref::from_column(&subcol.values)?;

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
            let v = if valid { sub_ref.get(row) } else { "" };
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
/// (`Lz4` in `L0Fast` when `raw_len >= 64`, `Zstd`/`Lz4` in `L1Compact`, else `Codec::None`).
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

    let codec = if child_payload.has_subchunked_fsst {
        Codec::None
    } else {
        preferred_block_codec(profile, out.len(), true)
    };
    Ok((codec, out))
}

struct ParsedSubColHeader<'a> {
    in_map: Bitmap,
    value_null: Bitmap,
    enc: Encoding,
    flags: u8,
    min_inline_len: u8,
    min_inline: [u8; 16],
    dict: Option<FileDict>,
    body: &'a [u8],
}

fn parse_shredded_subcol_header(
    src: &[u8],
    row_count: usize,
) -> Result<ParsedSubColHeader<'_>, StatamError> {
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
    if (flags & !KNOWN_BLOCK_FLAGS) != 0 {
        return Err(StatamError::Corrupt {
            reason: "unknown shredded subcol flags",
        });
    }
    if (flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 && enc != Encoding::Fsst {
        return Err(StatamError::Corrupt {
            reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst shredded subcol",
        });
    }
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
    Ok(ParsedSubColHeader {
        in_map,
        value_null,
        enc,
        flags,
        min_inline_len,
        min_inline,
        dict,
        body: &rest[pos..],
    })
}

/// Decodes a [`ShreddedSubCol`] encoded by [`encode_shredded_subcol`].
pub fn decode_shredded_subcol(src: &[u8], row_count: usize) -> Result<ShreddedSubCol, StatamError> {
    let hdr = parse_shredded_subcol_header(src, row_count)?;
    let mut budget = MAX_BLOCK_RAW_BYTES;
    let values = decode_child_column(
        hdr.body,
        hdr.enc,
        hdr.flags,
        row_count,
        None,
        hdr.min_inline_len,
        hdr.min_inline,
        &DataType::Utf8,
        hdr.dict.as_ref(),
        1,
        &mut budget,
    )?;

    Ok(ShreddedSubCol {
        in_map: hdr.in_map,
        value_null: hdr.value_null,
        values,
    })
}

/// Decodes a [`ShreddedSubCol`] decoding only `mask`-selected rows when sparse
/// (`!mask.all_ones() && mask.count_ones().saturating_mul(16) < row_count`).
///
/// `Dict` dispatches to `decode_dict_selected` (avoiding full-block string materialization on
/// large dictionaries); `Rle`, `Const`, and `ConstNull` pass through directly; `Fsst` and `Plain`
/// decode only active rows.
pub fn decode_shredded_subcol_selected(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
) -> Result<ShreddedSubCol, StatamError> {
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if mask.all_ones() || mask.count_ones().saturating_mul(16) >= row_count {
        return decode_shredded_subcol(src, row_count);
    }

    let hdr = parse_shredded_subcol_header(src, row_count)?;
    let values = if row_count == 0 {
        if !hdr.body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty child column",
            });
        }
        Column::Flat(FlatColumn::empty(&DataType::Utf8))
    } else if hdr.enc == Encoding::Fsst {
        let mut budget = MAX_BLOCK_RAW_BYTES;
        let (nulls, after_nulls) = read_optional_nulls(hdr.body, row_count, hdr.flags)?;
        let base_bytes = row_count
            .div_ceil(8)
            .saturating_mul(2 + usize::from(nulls.is_some()))
            .saturating_add(row_count.saturating_add(1).saturating_mul(4));
        debit_budget(
            &mut budget,
            base_bytes,
            "shredded subcol exceeds MAX_BLOCK_RAW_BYTES",
        )?;
        let subchunked = (hdr.flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0;
        let (u, out_nulls) = decode_fsst_utf8_selected_with_flag(
            after_nulls,
            row_count,
            mask,
            nulls.as_ref(),
            subchunked,
            budget,
        )?;
        Column::Flat(FlatColumn::try_new(FlatData::Utf8(u), out_nulls)?)
    } else if hdr.enc == Encoding::Plain {
        let (nulls, after_nulls) = read_optional_nulls(hdr.body, row_count, hdr.flags)?;
        let (offsets, bytes) =
            decode_plain_varlen_selected(after_nulls, row_count, mask, nulls.as_ref())?;
        Column::Flat(FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?),
            nulls,
        )?)
    } else if hdr.enc == Encoding::Dict {
        let Some(dict) = hdr.dict.as_ref() else {
            return Err(StatamError::Corrupt {
                reason: "shredded subcol Dict missing inline dict",
            });
        };
        if dict.values().data_type() != DataType::Utf8 {
            return Err(StatamError::SchemaMismatch {
                reason: "child FileDict DataType mismatch",
            });
        }
        let (nulls, after_nulls) = read_optional_nulls(hdr.body, row_count, hdr.flags)?;
        decode_dict_selected(after_nulls, row_count, nulls, dict, mask)?
    } else {
        let mut budget = MAX_BLOCK_RAW_BYTES;
        decode_child_column(
            hdr.body,
            hdr.enc,
            hdr.flags,
            row_count,
            None,
            hdr.min_inline_len,
            hdr.min_inline,
            &DataType::Utf8,
            hdr.dict.as_ref(),
            1,
            &mut budget,
        )?
    };

    Ok(ShreddedSubCol {
        in_map: hdr.in_map,
        value_null: hdr.value_null,
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
            owned_zm = compute_zone_map(&as_col);
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
        let mut key_map: HashMap<&str, u32, BuildHasherDefault<FastSliceHasher>> =
            HashMap::default();
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
    if entry.encoding != Encoding::ShreddedMap {
        return Err(StatamError::Corrupt {
            reason: "incompatible encoding for ShreddedMap residual block",
        });
    }
    decode_shredded_residual_with_count(raw_body, row_count, entry.block_flags)
}

/// Decodes a `ShreddedMap` (or sparse `Plain` `MapUtf8`) residual block, materializing
/// entries only for rows selected by `mask` when `mask` is sparse
/// (`!mask.all_ones() && mask.count_ones().saturating_mul(16) < row_count`).
pub fn decode_shredded_residual_block_selected(
    raw_body: &[u8],
    entry: &BlockEntry,
    mask: &Bitmap,
) -> Result<FlatColumn, StatamError> {
    if mask.len() != entry.row_count as usize {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if entry.encoding == Encoding::ConstNull || entry.encoding == Encoding::Const {
        return Ok(decode_const_block(entry, &DataType::MapUtf8)?.materialize(None)?);
    }
    let row_count = entry.row_count as usize;
    if row_count == 0 || mask.all_ones() || mask.count_ones().saturating_mul(16) >= row_count {
        return decode_shredded_residual_block(raw_body, entry);
    }
    if entry.encoding == Encoding::Plain {
        let (nulls, body) = read_optional_nulls(raw_body, row_count, entry.block_flags)?;
        let data = decode_plain_map_selected(body, row_count, mask)?;
        return Ok(FlatColumn::try_new(data, nulls)?);
    }
    if entry.encoding != Encoding::ShreddedMap {
        return Err(StatamError::Corrupt {
            reason: "incompatible encoding for ShreddedMap residual block",
        });
    }
    decode_shredded_residual_with_count_selected(raw_body, row_count, entry.block_flags, mask)
}

struct ParsedShreddedResidual<'a> {
    row_nulls: Option<Bitmap>,
    offsets: Vec<u32>,
    entry_count: usize,
    key_offsets: Vec<u32>,
    keys_blob: &'a [u8],
    key_codes: Vec<u32>,
    val_nulls: Option<Bitmap>,
    val_src: &'a [u8],
}

fn debit_budget(budget: &mut usize, bytes: usize, reason: &'static str) -> Result<(), StatamError> {
    if bytes > *budget {
        return Err(StatamError::BoundsExceeded { reason });
    }
    *budget -= bytes;
    Ok(())
}

fn empty_map_with_rows(rows: usize) -> Result<MapData, StatamError> {
    Ok(MapData::try_new(
        vec![0u32; rows + 1],
        Utf8Data::empty(),
        Utf8Data::empty(),
        None,
    )?)
}

fn parse_shredded_residual_parts<'a>(
    raw_body: &'a [u8],
    row_count: usize,
    block_flags: u8,
    budget: &mut usize,
) -> Result<ParsedShreddedResidual<'a>, StatamError> {
    let (row_nulls, rest) = read_optional_nulls(raw_body, row_count, block_flags)?;
    if row_nulls.is_some() {
        debit_budget(
            budget,
            row_count.div_ceil(8),
            "shredded residual allocations exceed MAX_BLOCK_RAW_BYTES",
        )?;
    }
    let counts_bytes = row_count
        .checked_mul(4)
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded residual row_count overflow",
        })?;
    let offsets_bytes = row_count
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded residual offsets overflow",
        })?;
    debit_budget(
        budget,
        counts_bytes
            .checked_add(offsets_bytes)
            .ok_or(StatamError::BoundsExceeded {
                reason: "shredded residual allocation overflow",
            })?,
        "shredded residual allocations exceed MAX_BLOCK_RAW_BYTES",
    )?;
    let (counts, mut rest) = read_packed_counts(rest, row_count)?;
    let (offsets, entry_count) = counts_to_offsets(&counts)?;

    if entry_count == 0 {
        if !rest.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty shredded residual",
            });
        }
        return Ok(ParsedShreddedResidual {
            row_nulls,
            offsets,
            entry_count: 0,
            key_offsets: Vec::new(),
            keys_blob: &[],
            key_codes: Vec::new(),
            val_nulls: None,
            val_src: &[],
        });
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
    if klen_bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid shredded residual key length bit_width",
        });
    }
    rest = &rest[5..];
    let klen_packed = packed_bytes_len(num_keys, klen_bw)?;
    if rest.len() < klen_packed + 4 {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded residual key lengths",
        });
    }
    let keys_raw_bytes = u32::from_le_bytes([
        rest[klen_packed],
        rest[klen_packed + 1],
        rest[klen_packed + 2],
        rest[klen_packed + 3],
    ]) as usize;
    if keys_raw_bytes > MAX_BLOCK_RAW_BYTES || rest.len() < klen_packed + 4 + keys_raw_bytes + 1 {
        return Err(StatamError::Corrupt {
            reason: "truncated or oversized shredded residual keys",
        });
    }
    let kcode_bw_off = klen_packed + 4 + keys_raw_bytes;
    let kcode_bw = rest[kcode_bw_off];
    if kcode_bw > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid shredded residual key code bit_width",
        });
    }
    let kcode_packed = packed_bytes_len(entry_count, kcode_bw)?;
    if rest.len() < kcode_bw_off + 1 + kcode_packed + 1 {
        return Err(StatamError::Corrupt {
            reason: "truncated shredded residual key codes",
        });
    }

    // Bound combined allocations (key_offsets, keys_blob, key_codes, and entry offsets) BEFORE allocating.
    let key_offsets_bytes = num_keys
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded key_offsets size overflow",
        })?;
    let key_codes_bytes = entry_count
        .checked_mul(4)
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded key_codes size overflow",
        })?;
    let val_offsets_bytes = entry_count
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded value offsets size overflow",
        })?;
    let combined = key_offsets_bytes
        .checked_add(keys_raw_bytes)
        .and_then(|n| n.checked_add(key_codes_bytes))
        .and_then(|n| n.checked_add(val_offsets_bytes))
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded residual allocations overflow",
        })?;
    debit_budget(
        budget,
        combined,
        "shredded residual allocations exceed MAX_BLOCK_RAW_BYTES",
    )?;

    let mut key_offsets = Vec::with_capacity(num_keys + 1);
    key_offsets.push(0u32);
    unpack_u32(&rest[..klen_packed], num_keys, klen_bw, &mut key_offsets)?;
    rest = &rest[klen_packed + 4..];

    let keys_blob = &rest[..keys_raw_bytes];
    rest = &rest[keys_raw_bytes..];

    let mut running = 0usize;
    for off_slot in &mut key_offsets[1..] {
        running = running
            .checked_add(*off_slot as usize)
            .ok_or(StatamError::BoundsExceeded {
                reason: "shredded key offset overflow",
            })?;
        *off_slot = u32::try_from(running).map_err(|_| StatamError::BoundsExceeded {
            reason: "shredded key offset exceeds u32::MAX",
        })?;
    }
    if running != keys_raw_bytes {
        return Err(StatamError::Corrupt {
            reason: "shredded key lengths do not match blob length",
        });
    }

    rest = &rest[1..]; // skip already-validated kcode_bw
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

    let (val_nulls, val_src) = read_flag_bitmap(rest, entry_count)?;
    if val_nulls.is_some() {
        debit_budget(
            budget,
            entry_count.div_ceil(8),
            "shredded residual allocations exceed MAX_BLOCK_RAW_BYTES",
        )?;
    }

    Ok(ParsedShreddedResidual {
        row_nulls,
        offsets,
        entry_count,
        key_offsets,
        keys_blob,
        key_codes,
        val_nulls,
        val_src,
    })
}

fn decode_shredded_residual_with_count(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
) -> Result<FlatColumn, StatamError> {
    let mut budget = MAX_BLOCK_RAW_BYTES;
    decode_shredded_residual_with_count_budgeted(raw_body, row_count, block_flags, &mut budget)
}

fn decode_shredded_residual_with_count_budgeted(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    budget: &mut usize,
) -> Result<FlatColumn, StatamError> {
    if row_count == 0 {
        return Ok(FlatColumn::empty(&DataType::MapUtf8));
    }
    let parsed = parse_shredded_residual_parts(raw_body, row_count, block_flags, budget)?;
    if parsed.entry_count == 0 {
        return Ok(FlatColumn::try_new(
            FlatData::MapUtf8(Box::new(empty_map_with_rows(row_count)?)),
            parsed.row_nulls,
        )?);
    }

    let key_dict = Utf8Data::try_new(parsed.key_offsets, parsed.keys_blob.to_vec())?;
    let keys_mat_bytes =
        sum_varlen_take_bytes(key_dict.offsets(), &parsed.key_codes, None, *budget)?;
    let keys_offs_bytes = parsed
        .entry_count
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded key offsets overflow",
        })?;
    debit_budget(
        budget,
        keys_mat_bytes
            .checked_add(keys_offs_bytes)
            .ok_or(StatamError::BoundsExceeded {
                reason: "shredded key bytes overflow",
            })?,
        "shredded residual keys exceed MAX_BLOCK_RAW_BYTES",
    )?;
    let keys_data = key_dict.take(&parsed.key_codes, None)?;
    let vals_data = decode_utf8_fsst_or_plain(parsed.val_src, parsed.entry_count)?;
    debit_budget(
        budget,
        vals_data.bytes().len(),
        "shredded residual values exceed MAX_BLOCK_RAW_BYTES",
    )?;

    let map = MapData::try_new(parsed.offsets, keys_data, vals_data, parsed.val_nulls)?;
    Ok(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(map)),
        parsed.row_nulls,
    )?)
}

fn decode_shredded_residual_with_count_selected(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    mask: &Bitmap,
) -> Result<FlatColumn, StatamError> {
    if row_count == 0 {
        return Ok(FlatColumn::empty(&DataType::MapUtf8));
    }
    let mut budget = MAX_BLOCK_RAW_BYTES;
    let parsed = parse_shredded_residual_parts(raw_body, row_count, block_flags, &mut budget)?;
    if parsed.entry_count == 0 {
        return Ok(FlatColumn::try_new(
            FlatData::MapUtf8(Box::new(empty_map_with_rows(row_count)?)),
            parsed.row_nulls,
        )?);
    }

    // Validate key dictionary UTF-8 boundaries without cloning keys_blob.
    let keys_str = std::str::from_utf8(parsed.keys_blob)
        .map_err(|_| StatamError::Column(cetra_column::ColumnError::InvalidUtf8))?;
    for &o in &parsed.key_offsets {
        if !keys_str.is_char_boundary(o as usize) {
            return Err(StatamError::Column(cetra_column::ColumnError::InvalidUtf8));
        }
    }

    if parsed.val_src.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated nested utf8 header",
        });
    }
    let val_enc = Encoding::from_u8(parsed.val_src[0])?;
    let val_len = u32::from_le_bytes([
        parsed.val_src[1],
        parsed.val_src[2],
        parsed.val_src[3],
        parsed.val_src[4],
    ]) as usize;
    let val_body = &parsed.val_src[5..];
    if val_body.len() != val_len {
        return Err(StatamError::Corrupt {
            reason: "nested utf8 payload length mismatch",
        });
    }

    enum ValSource<'a> {
        Fsst(Box<FsstBlockView<'a>>),
        Plain { off_raw: &'a [u8], data: &'a [u8] },
    }

    let val_source = match val_enc {
        Encoding::Fsst => ValSource::Fsst(Box::new(FsstBlockView::parse(
            val_body,
            parsed.entry_count,
        )?)),
        Encoding::Plain => {
            let off_count =
                parsed
                    .entry_count
                    .checked_add(1)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "plain varlen offsets count overflow",
                    })?;
            let off_bytes = off_count
                .checked_mul(4)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "plain varlen offsets size overflow",
                })?;
            if off_bytes > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "plain varlen offsets size overflow",
                });
            }
            if val_body.len() < off_bytes {
                return Err(StatamError::Corrupt {
                    reason: "truncated plain varlen offsets",
                });
            }
            let data_len = val_body.len() - off_bytes;
            if data_len > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "plain varlen data exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let off_raw = &val_body[..off_bytes];
            validate_plain_offsets_raw(off_raw, data_len)?;
            ValSource::Plain {
                off_raw,
                data: &val_body[off_bytes..],
            }
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "unsupported nested utf8 encoding",
            });
        }
    };

    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut out_key_offsets = vec![0u32];
    let mut out_key_bytes = Vec::new();
    let mut out_val_offsets = vec![0u32];
    let mut out_val_bytes = Vec::new();
    let mut out_val_pos = 0usize;
    let mut out_val_valid = Vec::new();

    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        let cur_entries = out_val_valid.len() as u32;
        if out_offsets.len() <= row {
            out_offsets.resize(row + 1, cur_entries);
        }
        let rs = parsed.offsets[row] as usize;
        let re = parsed.offsets[row + 1] as usize;
        for idx in rs..re {
            let kc = parsed.key_codes[idx] as usize;
            let ks = parsed.key_offsets[kc] as usize;
            let ke = parsed.key_offsets[kc + 1] as usize;
            let klen = ke - ks;
            if out_key_bytes.len().saturating_add(klen) > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "shredded residual keys exceed MAX_BLOCK_RAW_BYTES",
                });
            }
            out_key_bytes.extend_from_slice(&parsed.keys_blob[ks..ke]);
            out_key_offsets.push(out_key_bytes.len() as u32);

            match &val_source {
                ValSource::Fsst(view) => {
                    view.decode_row_at(idx, &mut out_val_bytes, &mut out_val_pos)?;
                }
                ValSource::Plain { off_raw, data } => {
                    let vs = read_u32_le_at(off_raw, idx);
                    let ve = read_u32_le_at(off_raw, idx + 1);
                    let vlen = ve - vs;
                    if out_val_pos.saturating_add(vlen) > MAX_BLOCK_RAW_BYTES {
                        return Err(StatamError::BoundsExceeded {
                            reason: "shredded residual values exceed MAX_BLOCK_RAW_BYTES",
                        });
                    }
                    out_val_bytes.extend_from_slice(&data[vs..ve]);
                    out_val_pos += vlen;
                }
            }
            out_val_offsets.push(out_val_pos as u32);

            out_val_valid.push(parsed.val_nulls.as_ref().is_none_or(|bm| bm.get(idx)));
        }
        out_offsets.push(out_val_valid.len() as u32);
    }
    out_val_bytes.truncate(out_val_pos);

    let tail_entries = out_val_valid.len() as u32;
    if out_offsets.len() < row_count + 1 {
        out_offsets.resize(row_count + 1, tail_entries);
    }

    let keys_data = Utf8Data::try_new(out_key_offsets, out_key_bytes)?;
    let vals_data = Utf8Data::try_new(out_val_offsets, out_val_bytes)?;
    let out_val_nulls = if out_val_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_val_valid))
    };
    let map = MapData::try_new(out_offsets, keys_data, vals_data, out_val_nulls)?;
    Ok(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(map)),
        parsed.row_nulls,
    )?)
}

/// Encodes `List(T)`, `Events`, or `Links` into [`EncodedMiniBlock`].
pub(crate) fn encode_nested_column(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    remaining_depth: usize,
    precomputed_zm: Option<&ZoneMapSummary>,
) -> Result<EncodedMiniBlock, StatamError> {
    encode_nested_column_inner(col, dt, profile, remaining_depth, precomputed_zm)
        .map(|(block, _)| block)
}

fn encode_nested_column_inner(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    remaining_depth: usize,
    precomputed_zm: Option<&ZoneMapSummary>,
) -> Result<(EncodedMiniBlock, bool), StatamError> {
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
        return Ok((
            EncodedMiniBlock {
                encoding: enc,
                block_flags: 0,
                null_count: 0,
                min_inline_len: 0,
                max_inline_len: 0,
                min_inline: [0u8; 16],
                max_inline: [0u8; 16],
                raw_body: Vec::new(),
                preferred_codec: Codec::None,
            },
            false,
        ));
    }

    let owned_zm;
    let zm = match precomputed_zm {
        Some(z) => z,
        None => {
            owned_zm = compute_zone_map(col);
            if let Some(inline) = try_encode_const_or_null(col, dt, &owned_zm) {
                return Ok((inline, false));
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
            let child_null_count = u32::try_from(count_col_nulls(values)).map_err(|_| {
                StatamError::BoundsExceeded {
                    reason: "ListEncoded child null_count exceeds u32::MAX",
                }
            })?;

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

            let has_subchunked_fsst = child_payload.has_subchunked_fsst;
            let preferred_codec = if has_subchunked_fsst {
                Codec::None
            } else {
                preferred_block_codec(profile, raw.len(), true)
            };
            Ok((
                EncodedMiniBlock {
                    encoding: Encoding::ListEncoded,
                    block_flags: zm.block_flags,
                    null_count: zm.null_count,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    min_inline: [0u8; 16],
                    max_inline: [0u8; 16],
                    raw_body: raw,
                    preferred_codec,
                },
                has_subchunked_fsst,
            ))
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
                Ok((
                    EncodedMiniBlock {
                        encoding: Encoding::Plain,
                        block_flags: zm.block_flags,
                        null_count: zm.null_count,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        min_inline: [0u8; 16],
                        max_inline: [0u8; 16],
                        raw_body: raw,
                        preferred_codec,
                    },
                    false,
                ))
            } else {
                Ok((
                    encode_shredded_residual_block_with_zm(flat, profile, Some(zm))?,
                    false,
                ))
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
            Ok((
                EncodedMiniBlock {
                    encoding: Encoding::EventsStruct,
                    block_flags: zm.block_flags,
                    null_count: zm.null_count,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    min_inline: [0u8; 16],
                    max_inline: [0u8; 16],
                    raw_body: raw,
                    preferred_codec,
                },
                false,
            ))
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
            Ok((
                EncodedMiniBlock {
                    encoding: Encoding::LinksStruct,
                    block_flags: zm.block_flags,
                    null_count: zm.null_count,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    min_inline: [0u8; 16],
                    max_inline: [0u8; 16],
                    raw_body: raw,
                    preferred_codec,
                },
                false,
            ))
        }
        _ => Err(StatamError::SchemaMismatch {
            reason: "narrow DataType passed to encode_nested_column",
        }),
    }
}

/// Decodes a `ListEncoded`, `EventsStruct`, `LinksStruct`, or `ShreddedMap` mini-block,
/// decoding only `mask`-selected rows when `mask` is sparse
/// (`!mask.all_ones() && mask.count_ones().saturating_mul(16) < row_count`).
pub fn decode_nested_column_selected(
    raw_body: &[u8],
    entry: &BlockEntry,
    dt: &DataType,
    max_depth: usize,
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
    if row_count == 0 || mask.all_ones() || mask.count_ones().saturating_mul(16) >= row_count {
        return decode_nested_column(raw_body, entry, dt, max_depth);
    }

    match (entry.encoding, dt) {
        (Encoding::ListEncoded, DataType::List(inner)) => decode_list_body_selected(
            raw_body,
            row_count,
            entry.block_flags,
            inner,
            max_depth,
            mask,
        ),
        (Encoding::ShreddedMap | Encoding::Plain, DataType::MapUtf8) => Ok(Column::Flat(
            decode_shredded_residual_block_selected(raw_body, entry, mask)?,
        )),
        (Encoding::EventsStruct, DataType::Events) => {
            decode_events_body_selected(raw_body, row_count, entry.block_flags, mask)
        }
        (Encoding::LinksStruct, DataType::Links) => {
            decode_links_body_selected(raw_body, row_count, entry.block_flags, mask)
        }
        _ => Err(StatamError::Corrupt {
            reason: "incompatible nested encoding for DataType",
        }),
    }
}

fn build_active_nested_offsets(
    offsets: &[u32],
    row_count: usize,
    mask: &Bitmap,
) -> (Vec<u32>, usize) {
    let mut out_offsets = Vec::with_capacity(row_count + 1);
    out_offsets.push(0u32);
    let mut active_total = 0u32;
    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        if out_offsets.len() <= row {
            out_offsets.resize(row + 1, active_total);
        }
        active_total = active_total.saturating_add(offsets[row + 1] - offsets[row]);
        out_offsets.push(active_total);
    }
    if out_offsets.len() < row_count + 1 {
        out_offsets.resize(row_count + 1, active_total);
    }
    (out_offsets, active_total as usize)
}

#[allow(clippy::too_many_arguments)]
fn decode_selected_utf8_or_binary(
    body: &[u8],
    enc: Encoding,
    flags: u8,
    total_len: usize,
    offsets: &[u32],
    row_count: usize,
    mask: &Bitmap,
    nulls: Option<&Bitmap>,
    budget: &mut usize,
) -> Result<SelectedVarlenResult, StatamError> {
    if enc == Encoding::Fsst && (flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 {
        let active_count: usize = mask
            .iter_ones()
            .take_while(|&r| r < row_count)
            .map(|r| (offsets[r + 1] - offsets[r]) as usize)
            .sum();
        let offs_bytes = active_count
            .checked_add(1)
            .and_then(|n| n.checked_mul(4))
            .ok_or(StatamError::BoundsExceeded {
                reason: "active varlen offsets overflow",
            })?;
        let nulls_bytes = if nulls.is_some() {
            active_count.div_ceil(8)
        } else {
            0
        };
        let base_bytes =
            offs_bytes
                .checked_add(nulls_bytes)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "active varlen metadata overflow",
                })?;
        debit_budget(
            budget,
            base_bytes,
            "active varlen bytes exceed MAX_BLOCK_RAW_BYTES",
        )?;
        let view = FsstSubchunkedView::parse(body, total_len)?;
        let (out_offsets, out_bytes, out_nulls) =
            view.decode_ranges(offsets, row_count, mask, nulls, *budget)?;
        debit_budget(
            budget,
            out_bytes.len(),
            "active varlen bytes exceed MAX_BLOCK_RAW_BYTES",
        )?;
        return Ok((out_offsets, out_bytes, out_nulls));
    }

    enum Source<'a> {
        Fsst(Box<FsstBlockView<'a>>),
        Plain { off_raw: &'a [u8], data: &'a [u8] },
    }

    let source = match enc {
        Encoding::Fsst => Source::Fsst(Box::new(FsstBlockView::parse(body, total_len)?)),
        Encoding::Plain => {
            let off_count = total_len
                .checked_add(1)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "plain varlen offsets count overflow",
                })?;
            let off_bytes = off_count
                .checked_mul(4)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "plain varlen offsets size overflow",
                })?;
            if off_bytes > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "plain varlen offsets size overflow",
                });
            }
            if body.len() < off_bytes {
                return Err(StatamError::Corrupt {
                    reason: "truncated plain varlen offsets",
                });
            }
            let data_len = body.len() - off_bytes;
            if data_len > MAX_BLOCK_RAW_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "plain varlen data exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let off_raw = &body[..off_bytes];
            validate_plain_offsets_raw(off_raw, data_len)?;
            Source::Plain {
                off_raw,
                data: &body[off_bytes..],
            }
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "unsupported varlen encoding",
            });
        }
    };

    let mut out_offsets = vec![0u32];
    let mut out_bytes = Vec::new();
    let mut out_pos = 0usize;
    let mut out_nulls: Option<Bitmap> = None;
    let mut active_idx = 0usize;

    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        let rs = offsets[row] as usize;
        let re = offsets[row + 1] as usize;
        for idx in rs..re {
            let valid = nulls.is_none_or(|bm| bm.get(idx));
            if valid {
                match &source {
                    Source::Fsst(view) => {
                        view.decode_row_at(idx, &mut out_bytes, &mut out_pos)?;
                    }
                    Source::Plain { off_raw, data } => {
                        let s = read_u32_le_at(off_raw, idx);
                        let e = read_u32_le_at(off_raw, idx + 1);
                        let vlen = e - s;
                        if out_pos.saturating_add(vlen) > MAX_BLOCK_RAW_BYTES {
                            return Err(StatamError::BoundsExceeded {
                                reason: "active varlen bytes exceed MAX_BLOCK_RAW_BYTES",
                            });
                        }
                        out_bytes.extend_from_slice(&data[s..e]);
                        out_pos += vlen;
                    }
                }
            } else if out_nulls.is_none() {
                // Lazy-allocate only when an active element is null.
                let total_active: usize = mask
                    .iter_ones()
                    .take_while(|&r| r < row_count)
                    .map(|r| (offsets[r + 1] - offsets[r]) as usize)
                    .sum();
                let mut bm = Bitmap::new_ones(total_active);
                let _ = bm.set(active_idx, false);
                out_nulls = Some(bm);
            } else if let Some(bm) = out_nulls.as_mut() {
                let _ = bm.set(active_idx, false);
            }
            out_offsets.push(out_pos as u32);
            active_idx += 1;
        }
    }
    out_bytes.truncate(out_pos);

    Ok((out_offsets, out_bytes, out_nulls))
}

fn validate_list_child_header(
    child_enc: Encoding,
    child_len: usize,
    inner: &DataType,
    dict: Option<&FileDict>,
) -> Result<(), StatamError> {
    if child_len == 0 && child_enc == Encoding::Plain {
        return Ok(());
    }
    if child_enc == Encoding::Dict {
        let Some(d) = dict else {
            return Err(StatamError::Corrupt {
                reason: "ListEncoded Dict child missing inline dict",
            });
        };
        if d.values().data_type() != *inner {
            return Err(StatamError::SchemaMismatch {
                reason: "child FileDict DataType mismatch",
            });
        }
        return Ok(());
    }
    let valid = matches!(
        (child_enc, inner),
        (Encoding::ConstNull | Encoding::Const, _)
            | (Encoding::ListEncoded, DataType::List(_))
            | (Encoding::ShreddedMap | Encoding::Plain, DataType::MapUtf8)
            | (Encoding::EventsStruct, DataType::Events)
            | (Encoding::LinksStruct, DataType::Links)
            | (
                Encoding::BitpackBool | Encoding::Plain | Encoding::Rle,
                DataType::Bool
            )
            | (
                Encoding::Plain | Encoding::ForBitpack | Encoding::DeltaBitpack | Encoding::Rle,
                DataType::I64 | DataType::TimestampNs { .. } | DataType::Date,
            )
            | (
                Encoding::Plain | Encoding::ForBitpack | Encoding::Rle,
                DataType::Decimal128 { .. },
            )
            | (
                Encoding::Plain | Encoding::Alp | Encoding::Rle,
                DataType::F64
            )
            | (
                Encoding::Plain | Encoding::Fsst | Encoding::Rle,
                DataType::Utf8 | DataType::Binary,
            )
            | (
                Encoding::Plain | Encoding::Rle,
                DataType::Null
                    | DataType::Interval
                    | DataType::FixedBytes8
                    | DataType::FixedBytes16,
            )
    );
    if !valid {
        return Err(StatamError::Corrupt {
            reason: "incompatible child encoding for ListEncoded DataType",
        });
    }
    Ok(())
}

struct ParsedListChildHeader<'a> {
    child_enc: Encoding,
    child_flags: u8,
    child_len: usize,
    child_null_count: u32,
    child_min_inline_len: u8,
    child_min_inline: [u8; 16],
    dict: Option<FileDict>,
    child_body: &'a [u8],
}

fn parse_list_child_header(
    rest: &[u8],
    total_child: usize,
) -> Result<ParsedListChildHeader<'_>, StatamError> {
    if rest.len() < 28 {
        return Err(StatamError::Corrupt {
            reason: "truncated ListEncoded child header",
        });
    }
    let child_enc = Encoding::from_u8(rest[0])?;
    let child_flags = rest[1];
    if (child_flags & !KNOWN_BLOCK_FLAGS) != 0 {
        return Err(StatamError::Corrupt {
            reason: "unknown ListEncoded child flags",
        });
    }
    if (child_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 && child_enc != Encoding::Fsst {
        return Err(StatamError::Corrupt {
            reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst ListEncoded child",
        });
    }
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
    Ok(ParsedListChildHeader {
        child_enc,
        child_flags,
        child_len,
        child_null_count,
        child_min_inline_len,
        child_min_inline,
        dict,
        child_body: &rest[pos..],
    })
}

fn debit_list_level_budget(
    budget: &mut usize,
    row_count: usize,
    has_row_nulls: bool,
) -> Result<(), StatamError> {
    let counts_bytes = row_count
        .checked_mul(4)
        .ok_or(StatamError::BoundsExceeded {
            reason: "ListEncoded row_count overflow",
        })?;
    let offsets_bytes = row_count
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .ok_or(StatamError::BoundsExceeded {
            reason: "ListEncoded offsets overflow",
        })?;
    let nulls_bytes = if has_row_nulls {
        row_count.div_ceil(8)
    } else {
        0
    };
    let level_bytes = counts_bytes
        .checked_add(offsets_bytes)
        .and_then(|n| n.checked_add(nulls_bytes))
        .ok_or(StatamError::BoundsExceeded {
            reason: "ListEncoded allocation overflow",
        })?;
    debit_budget(
        budget,
        level_bytes,
        "ListEncoded allocations exceed MAX_BLOCK_RAW_BYTES",
    )
}

fn logical_const_null_bytes(dt: &DataType, len: usize) -> Option<usize> {
    let nulls_bytes = len.div_ceil(8);
    let data_bytes = match dt {
        DataType::Null => 0,
        DataType::Bool => len.div_ceil(8),
        DataType::Date => len.checked_mul(4)?,
        DataType::I64 | DataType::TimestampNs { .. } | DataType::F64 | DataType::FixedBytes8 => {
            len.checked_mul(8)?
        }
        DataType::Decimal128 { .. } | DataType::Interval | DataType::FixedBytes16 => {
            len.checked_mul(16)?
        }
        DataType::Utf8
        | DataType::Binary
        | DataType::List(_)
        | DataType::MapUtf8
        | DataType::Events
        | DataType::Links => len.checked_add(1)?.checked_mul(4)?,
    };
    data_bytes.checked_add(nulls_bytes)
}

fn logical_const_value_bytes(dt: &DataType, inline_len: usize, len: usize) -> Option<usize> {
    match dt {
        DataType::Null => Some(0),
        DataType::Bool => Some(len.div_ceil(8)),
        DataType::Date => len.checked_mul(4),
        DataType::I64 | DataType::TimestampNs { .. } | DataType::F64 | DataType::FixedBytes8 => {
            len.checked_mul(8)
        }
        DataType::Decimal128 { .. } | DataType::Interval | DataType::FixedBytes16 => {
            len.checked_mul(16)
        }
        DataType::Utf8 | DataType::Binary => {
            let offs = len.checked_add(1)?.checked_mul(4)?;
            let data = len.checked_mul(inline_len)?;
            offs.checked_add(data)
        }
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links => {
            len.checked_add(1)?.checked_mul(4)
        }
    }
}

fn logical_rle_bytes(rle: &RleColumn, dt: &DataType) -> Option<usize> {
    let total_len = rle.len();
    let nulls_bytes = if rle.has_nulls() {
        total_len.div_ceil(8)
    } else {
        0
    };
    let data_bytes = match rle.values().data() {
        FlatData::Utf8(u) => {
            let mut sum = 4usize;
            let mut prev = 0u32;
            let run_nulls = rle.values().nulls();
            for (i, &end) in rle.run_ends().iter().enumerate() {
                let run_len = (end - prev) as usize;
                let val_len = if run_nulls.is_some_and(|bm| !bm.get(i)) {
                    0usize
                } else {
                    u.get(i).map_or(0, str::len)
                };
                let per_row = val_len.checked_add(4)?;
                sum = sum.checked_add(run_len.checked_mul(per_row)?)?;
                prev = end;
            }
            sum
        }
        FlatData::Binary(b) => {
            let mut sum = 4usize;
            let mut prev = 0u32;
            let run_nulls = rle.values().nulls();
            for (i, &end) in rle.run_ends().iter().enumerate() {
                let run_len = (end - prev) as usize;
                let val_len = if run_nulls.is_some_and(|bm| !bm.get(i)) {
                    0usize
                } else {
                    b.get(i).map_or(0, <[u8]>::len)
                };
                let per_row = val_len.checked_add(4)?;
                sum = sum.checked_add(run_len.checked_mul(per_row)?)?;
                prev = end;
            }
            sum
        }
        _ => logical_const_value_bytes(dt, 0, total_len)?,
    };
    data_bytes.checked_add(nulls_bytes)
}

fn decode_list_body_selected(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    inner: &DataType,
    remaining_depth: usize,
    mask: &Bitmap,
) -> Result<Column, StatamError> {
    if remaining_depth == 0 {
        return Err(StatamError::BoundsExceeded {
            reason: "DataType::List nesting exceeds MAX_NESTING_DEPTH",
        });
    }

    let mut budget = MAX_BLOCK_RAW_BYTES;
    let (row_nulls, rest) = read_optional_nulls(raw_body, row_count, block_flags)?;
    debit_list_level_budget(&mut budget, row_count, row_nulls.is_some())?;
    let (counts, rest) = read_packed_counts(rest, row_count)?;
    let (offsets, total_child) = counts_to_offsets(&counts)?;

    let hdr = parse_list_child_header(rest, total_child)?;
    let child_enc = hdr.child_enc;
    let child_flags = hdr.child_flags;
    let child_len = hdr.child_len;
    let child_null_count = hdr.child_null_count;
    let child_min_inline_len = hdr.child_min_inline_len;
    let child_min_inline = hdr.child_min_inline;
    let dict = hdr.dict;
    let child_body = hdr.child_body;

    validate_list_child_header(child_enc, child_len, inner, dict.as_ref())?;
    if (child_len == 0 || matches!(child_enc, Encoding::ConstNull | Encoding::Const))
        && !child_body.is_empty()
    {
        return Err(StatamError::Corrupt {
            reason: "trailing bytes in ListEncoded inline child column",
        });
    }
    if child_enc == Encoding::Const
        && (child_null_count != 0 || (child_flags & BLOCK_FLAG_HAS_NULLS) != 0)
    {
        return Err(StatamError::Corrupt {
            reason: "Const child column cannot have nulls",
        });
    }

    let (out_offsets, active_child_count) = build_active_nested_offsets(&offsets, row_count, mask);
    if active_child_count == 0 {
        let list_flat = FlatColumn::try_new(
            FlatData::List {
                item_type: inner.clone(),
                offsets: out_offsets,
                values: Box::new(Column::Flat(FlatColumn::empty(inner))),
            },
            row_nulls,
        )?;
        return Ok(Column::Flat(list_flat));
    }

    let compact_child: Option<Column> = match (child_enc, inner) {
        (Encoding::ConstNull, _) => {
            let logical = logical_const_null_bytes(inner, active_child_count).ok_or(
                StatamError::BoundsExceeded {
                    reason: "ListEncoded ConstNull child exceeds MAX_BLOCK_RAW_BYTES",
                },
            )?;
            debit_budget(
                &mut budget,
                logical,
                "ListEncoded ConstNull child exceeds MAX_BLOCK_RAW_BYTES",
            )?;
            Some(Column::Const {
                value: ScalarValue::Null(inner.clone()),
                len: active_child_count,
            })
        }
        (Encoding::Const, _) => {
            let mut c = decode_child_column(
                child_body,
                child_enc,
                child_flags,
                active_child_count,
                Some(child_null_count),
                child_min_inline_len,
                child_min_inline,
                inner,
                dict.as_ref(),
                remaining_depth - 1,
                &mut budget,
            )?;
            if let Column::Const { len, .. } = &mut c {
                *len = active_child_count;
            }
            Some(c)
        }
        (Encoding::Dict, _) => {
            let Some(d) = dict.as_ref() else {
                return Err(StatamError::Corrupt {
                    reason: "ListEncoded Dict child missing inline dict",
                });
            };
            if d.values().data_type() != *inner {
                return Err(StatamError::SchemaMismatch {
                    reason: "child FileDict DataType mismatch",
                });
            }
            debit_budget(
                &mut budget,
                d.memory_bytes(),
                "ListEncoded Dict child exceeds MAX_BLOCK_RAW_BYTES",
            )?;
            let (c_nulls, after_nulls) = read_optional_nulls(child_body, child_len, child_flags)?;
            let full_codes =
                decode_dict_codes_body(after_nulls, child_len, d.len(), c_nulls.as_ref())?;
            let mut active_codes = Vec::with_capacity(active_child_count);
            let mut active_nulls = c_nulls
                .as_ref()
                .map(|_| Bitmap::new_ones(active_child_count));
            let mut aidx = 0usize;
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                let rs = offsets[row] as usize;
                let re = offsets[row + 1] as usize;
                for (offset_in_row, &code) in full_codes[rs..re].iter().enumerate() {
                    let idx = rs + offset_in_row;
                    if let Some(bm) = c_nulls.as_ref()
                        && !bm.get(idx)
                    {
                        active_codes.push(0);
                        if let Some(abm) = active_nulls.as_mut() {
                            let _ = abm.set(aidx, false);
                        }
                    } else {
                        active_codes.push(code);
                    }
                    aidx += 1;
                }
            }
            let logical = dict_logical_materialized_bytes(
                d.values(),
                &active_codes,
                active_nulls.as_ref(),
                budget,
            )?;
            debit_budget(
                &mut budget,
                logical,
                "ListEncoded Dict child exceeds MAX_BLOCK_RAW_BYTES",
            )?;
            if d.memory_bytes() <= DICT_PASSTHROUGH_MAX_BYTES {
                let dc = DictColumn::try_new(d.values().clone(), active_codes, active_nulls)?;
                Some(Column::Dict(dc))
            } else {
                Some(Column::Flat(materialize_from_dict(
                    d.values(),
                    &active_codes,
                    active_nulls,
                )?))
            }
        }
        (Encoding::Plain | Encoding::Fsst, DataType::Utf8) => {
            let (c_nulls, after_nulls) = read_optional_nulls(child_body, child_len, child_flags)?;
            let (co, cb, cn) = decode_selected_utf8_or_binary(
                after_nulls,
                child_enc,
                child_flags,
                child_len,
                &offsets,
                row_count,
                mask,
                c_nulls.as_ref(),
                &mut budget,
            )?;
            Some(Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_new(co, cb)?),
                cn,
            )?))
        }
        (Encoding::Plain | Encoding::Fsst, DataType::Binary) => {
            let (c_nulls, after_nulls) = read_optional_nulls(child_body, child_len, child_flags)?;
            let (co, cb, cn) = decode_selected_utf8_or_binary(
                after_nulls,
                child_enc,
                child_flags,
                child_len,
                &offsets,
                row_count,
                mask,
                c_nulls.as_ref(),
                &mut budget,
            )?;
            Some(Column::Flat(FlatColumn::try_new(
                FlatData::Binary(BinaryData::try_new(co, cb)?),
                cn,
            )?))
        }
        (Encoding::Plain, dt)
            if matches!(
                dt,
                DataType::I64
                    | DataType::TimestampNs { .. }
                    | DataType::F64
                    | DataType::Decimal128 { .. }
                    | DataType::Date
                    | DataType::Interval
                    | DataType::FixedBytes8
                    | DataType::FixedBytes16
                    | DataType::Null
            ) =>
        {
            let (c_nulls, after_nulls) = read_optional_nulls(child_body, child_len, child_flags)?;
            let elem_bytes = match dt {
                DataType::Null => 0usize,
                DataType::Date => 4,
                DataType::I64
                | DataType::TimestampNs { .. }
                | DataType::F64
                | DataType::FixedBytes8 => 8,
                DataType::Decimal128 { .. } | DataType::Interval | DataType::FixedBytes16 => 16,
                _ => unreachable!(),
            };
            let expected =
                child_len
                    .checked_mul(elem_bytes)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "ListEncoded child byte size overflow",
                    })?;
            if expected > MAX_BLOCK_RAW_BYTES || after_nulls.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "ListEncoded plain child byte length mismatch",
                });
            }
            let mut compact_body = Vec::with_capacity(active_child_count * elem_bytes);
            let mut active_nulls = c_nulls
                .as_ref()
                .map(|_| Bitmap::new_ones(active_child_count));
            let mut aidx = 0usize;
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                for idx in offsets[row] as usize..offsets[row + 1] as usize {
                    if let Some(bm) = c_nulls.as_ref()
                        && !bm.get(idx)
                        && let Some(abm) = active_nulls.as_mut()
                    {
                        let _ = abm.set(aidx, false);
                    }
                    if elem_bytes > 0 {
                        let s = idx * elem_bytes;
                        compact_body.extend_from_slice(&after_nulls[s..s + elem_bytes]);
                    }
                    aidx += 1;
                }
            }
            let data = decode_inner_data(
                &compact_body,
                Encoding::Plain,
                active_child_count,
                dt,
                active_nulls.as_ref(),
            )?;
            Some(Column::Flat(FlatColumn::try_new(data, active_nulls)?))
        }
        (Encoding::BitpackBool | Encoding::Plain, DataType::Bool) => {
            let (c_nulls, after_nulls) = read_optional_nulls(child_body, child_len, child_flags)?;
            let expected = child_len.div_ceil(8);
            if after_nulls.len() != expected {
                return Err(StatamError::Corrupt {
                    reason: "bool block byte length mismatch",
                });
            }
            let full_bm = Bitmap::from_bytes(after_nulls, child_len)?;
            let mut active_bm = Bitmap::new_zeroed(active_child_count);
            let mut active_nulls = c_nulls
                .as_ref()
                .map(|_| Bitmap::new_ones(active_child_count));
            let mut aidx = 0usize;
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                for idx in offsets[row] as usize..offsets[row + 1] as usize {
                    let is_valid = c_nulls.as_ref().is_none_or(|bm| bm.get(idx));
                    if !is_valid {
                        if let Some(abm) = active_nulls.as_mut() {
                            let _ = abm.set(aidx, false);
                        }
                    } else if full_bm.get(idx) {
                        let _ = active_bm.set(aidx, true);
                    }
                    aidx += 1;
                }
            }
            Some(Column::Flat(FlatColumn::try_new(
                FlatData::Bool(active_bm),
                active_nulls,
            )?))
        }
        _ => None,
    };

    let (final_offsets, child_col) = if let Some(c) = compact_child {
        (out_offsets, c)
    } else {
        let full_child = decode_child_column(
            child_body,
            child_enc,
            child_flags,
            child_len,
            Some(child_null_count),
            child_min_inline_len,
            child_min_inline,
            inner,
            dict.as_ref(),
            remaining_depth - 1,
            &mut budget,
        )?;
        (offsets, full_child)
    };

    let list_flat = FlatColumn::try_new(
        FlatData::List {
            item_type: inner.clone(),
            offsets: final_offsets,
            values: Box::new(child_col),
        },
        row_nulls,
    )?;
    Ok(Column::Flat(list_flat))
}

fn decode_events_body_selected(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    mask: &Bitmap,
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

    // 1. Parse times section header & slice.
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
    let t_body = &rest[..t_len];
    rest = &rest[t_len..];

    // 2. Parse names section header & slice.
    if rest.is_empty() {
        return Err(StatamError::Corrupt {
            reason: "truncated EventsStruct names header",
        });
    }
    let n_enc = Encoding::from_u8(rest[0])?;
    rest = &rest[1..];
    enum NamesSection<'a> {
        Dict { dict: FileDict, codes_raw: &'a [u8] },
        Varlen { enc: Encoding, body: &'a [u8] },
    }
    let names_sec = if n_enc == Encoding::Dict {
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
        let codes_raw = &rest[..c_len];
        rest = &rest[c_len..];
        NamesSection::Dict { dict, codes_raw }
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
        let n_body = &rest[..n_len];
        rest = &rest[n_len..];
        NamesSection::Varlen {
            enc: n_enc,
            body: n_body,
        }
    };

    // 3. Parse attr_nulls & attributes header.
    let (attr_nulls, rest) = read_flag_bitmap(rest, event_count)?;
    if rest.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated EventsStruct attrs header",
        });
    }
    let a_enc = Encoding::from_u8(rest[0])?;
    let a_len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
    let a_body = &rest[5..];
    if a_body.len() != a_len {
        return Err(StatamError::Corrupt {
            reason: "EventsStruct attrs length mismatch",
        });
    }

    if !matches!(
        t_enc,
        Encoding::Plain | Encoding::ForBitpack | Encoding::DeltaBitpack
    ) {
        return Err(StatamError::Corrupt {
            reason: "unsupported EventsStruct times encoding",
        });
    }
    if !matches!(a_enc, Encoding::Fsst | Encoding::Plain) {
        return Err(StatamError::Corrupt {
            reason: "unsupported EventsStruct attrs encoding",
        });
    }
    match &names_sec {
        NamesSection::Dict { dict, .. } => {
            if dict.values().data_type() != DataType::Utf8 {
                return Err(StatamError::Corrupt {
                    reason: "EventsStruct names dict is not Utf8",
                });
            }
        }
        NamesSection::Varlen { enc, .. } => {
            if !matches!(enc, Encoding::Fsst | Encoding::Plain) {
                return Err(StatamError::Corrupt {
                    reason: "unsupported EventsStruct names encoding",
                });
            }
        }
    }

    let (out_offsets, active_event_count) = build_active_nested_offsets(&offsets, row_count, mask);
    if active_event_count == 0 {
        let ev = EventsData::try_new(
            out_offsets,
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

    let active_times = if t_enc == Encoding::Plain {
        let expected = event_count
            .checked_mul(8)
            .ok_or(StatamError::BoundsExceeded {
                reason: "EventsStruct times overflow",
            })?;
        if expected > MAX_BLOCK_RAW_BYTES || t_body.len() != expected {
            return Err(StatamError::Corrupt {
                reason: "EventsStruct plain times byte length mismatch",
            });
        }
        let mut out = Vec::with_capacity(active_event_count);
        for row in mask.iter_ones() {
            if row >= row_count {
                break;
            }
            for idx in offsets[row] as usize..offsets[row + 1] as usize {
                let s = idx * 8;
                out.push(i64::from_le_bytes([
                    t_body[s],
                    t_body[s + 1],
                    t_body[s + 2],
                    t_body[s + 3],
                    t_body[s + 4],
                    t_body[s + 5],
                    t_body[s + 6],
                    t_body[s + 7],
                ]));
            }
        }
        out
    } else {
        let times_data = decode_inner_data(t_body, t_enc, event_count, &DataType::I64, None)?;
        let FlatData::I64(all_times) = times_data else {
            unreachable!();
        };
        let mut out = Vec::with_capacity(active_event_count);
        for row in mask.iter_ones() {
            if row >= row_count {
                break;
            }
            out.extend_from_slice(&all_times[offsets[row] as usize..offsets[row + 1] as usize]);
        }
        out
    };

    let active_names = match names_sec {
        NamesSection::Dict { dict, codes_raw } => {
            let codes = decode_dict_codes_body(codes_raw, event_count, dict.len(), None)?;
            let FlatData::Utf8(dict_u) = dict.values().data() else {
                return Err(StatamError::Corrupt {
                    reason: "EventsStruct names dict is not Utf8",
                });
            };
            let mut n_offs = vec![0u32];
            let mut n_bytes = Vec::new();
            for row in mask.iter_ones() {
                if row >= row_count {
                    break;
                }
                let rs = offsets[row] as usize;
                let re = offsets[row + 1] as usize;
                for &code in &codes[rs..re] {
                    let s = dict_u.get(code as usize).unwrap_or("");
                    if n_bytes.len().saturating_add(s.len()) > MAX_BLOCK_RAW_BYTES {
                        return Err(StatamError::BoundsExceeded {
                            reason: "EventsStruct active names exceed MAX_BLOCK_RAW_BYTES",
                        });
                    }
                    n_bytes.extend_from_slice(s.as_bytes());
                    n_offs.push(n_bytes.len() as u32);
                }
            }
            Utf8Data::try_new(n_offs, n_bytes)?
        }
        NamesSection::Varlen { enc, body } => {
            let mut budget = MAX_BLOCK_RAW_BYTES;
            let (no, nb, _) = decode_selected_utf8_or_binary(
                body,
                enc,
                0,
                event_count,
                &offsets,
                row_count,
                mask,
                None,
                &mut budget,
            )?;
            Utf8Data::try_new(no, nb)?
        }
    };

    let mut attr_budget = MAX_BLOCK_RAW_BYTES;
    let (ao, ab, _) = decode_selected_utf8_or_binary(
        a_body,
        a_enc,
        0,
        event_count,
        &offsets,
        row_count,
        mask,
        None,
        &mut attr_budget,
    )?;
    let active_attrs = Utf8Data::try_new(ao, ab)?;
    let active_attr_nulls = if let Some(an) = attr_nulls {
        let mut out_an = Bitmap::new_ones(active_event_count);
        let mut any_null = false;
        let mut aidx = 0usize;
        for row in mask.iter_ones() {
            if row >= row_count {
                break;
            }
            for idx in offsets[row] as usize..offsets[row + 1] as usize {
                if !an.get(idx) {
                    let _ = out_an.set(aidx, false);
                    any_null = true;
                }
                aidx += 1;
            }
        }
        if any_null { Some(out_an) } else { None }
    } else {
        None
    };

    let ev = EventsData::try_new(
        out_offsets,
        active_times,
        active_names,
        active_attrs,
        active_attr_nulls,
    )?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Events(Box::new(ev)),
        row_nulls,
    )?))
}

fn decode_links_body_selected(
    raw_body: &[u8],
    row_count: usize,
    block_flags: u8,
    mask: &Bitmap,
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

    let tids_slice = rest[..tid_bytes].as_chunks::<16>().0;
    rest = &rest[tid_bytes..];
    let sids_slice = rest[..sid_bytes].as_chunks::<8>().0;
    rest = &rest[sid_bytes..];

    let (attr_nulls, rest) = read_flag_bitmap(rest, link_count)?;
    if rest.len() < 5 {
        return Err(StatamError::Corrupt {
            reason: "truncated LinksStruct attrs header",
        });
    }
    let a_enc = Encoding::from_u8(rest[0])?;
    let a_len = u32::from_le_bytes([rest[1], rest[2], rest[3], rest[4]]) as usize;
    let a_body = &rest[5..];
    if a_body.len() != a_len {
        return Err(StatamError::Corrupt {
            reason: "LinksStruct attrs length mismatch",
        });
    }

    let (out_offsets, active_link_count) = build_active_nested_offsets(&offsets, row_count, mask);
    if active_link_count == 0 {
        if !matches!(a_enc, Encoding::Fsst | Encoding::Plain) {
            return Err(StatamError::Corrupt {
                reason: "unsupported LinksStruct attrs encoding",
            });
        }
        let lk = LinksData::try_new(out_offsets, Vec::new(), Vec::new(), Utf8Data::empty(), None)?;
        return Ok(Column::Flat(FlatColumn::try_new(
            FlatData::Links(Box::new(lk)),
            row_nulls,
        )?));
    }

    let mut active_tids = Vec::with_capacity(active_link_count);
    let mut active_sids = Vec::with_capacity(active_link_count);
    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        let rs = offsets[row] as usize;
        let re = offsets[row + 1] as usize;
        active_tids.extend_from_slice(&tids_slice[rs..re]);
        active_sids.extend_from_slice(&sids_slice[rs..re]);
    }

    let mut attr_budget = MAX_BLOCK_RAW_BYTES;
    let (ao, ab, _) = decode_selected_utf8_or_binary(
        a_body,
        a_enc,
        0,
        link_count,
        &offsets,
        row_count,
        mask,
        None,
        &mut attr_budget,
    )?;
    let active_attrs = Utf8Data::try_new(ao, ab)?;
    let active_attr_nulls = if let Some(an) = attr_nulls {
        let mut out_an = Bitmap::new_ones(active_link_count);
        let mut any_null = false;
        let mut aidx = 0usize;
        for row in mask.iter_ones() {
            if row >= row_count {
                break;
            }
            for idx in offsets[row] as usize..offsets[row + 1] as usize {
                if !an.get(idx) {
                    let _ = out_an.set(aidx, false);
                    any_null = true;
                }
                aidx += 1;
            }
        }
        if any_null { Some(out_an) } else { None }
    } else {
        None
    };

    let lk = LinksData::try_new(
        out_offsets,
        active_tids,
        active_sids,
        active_attrs,
        active_attr_nulls,
    )?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Links(Box::new(lk)),
        row_nulls,
    )?))
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
        (Encoding::ListEncoded, DataType::List(inner)) => {
            let mut budget = MAX_BLOCK_RAW_BYTES;
            decode_list_body(
                raw_body,
                row_count,
                entry.block_flags,
                inner,
                remaining_depth,
                &mut budget,
            )
        }
        (Encoding::ShreddedMap | Encoding::Plain, DataType::MapUtf8) => Ok(Column::Flat(
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
    budget: &mut usize,
) -> Result<Column, StatamError> {
    if remaining_depth == 0 {
        return Err(StatamError::BoundsExceeded {
            reason: "DataType::List nesting exceeds MAX_NESTING_DEPTH",
        });
    }

    let (row_nulls, rest) = read_optional_nulls(raw_body, row_count, block_flags)?;
    debit_list_level_budget(budget, row_count, row_nulls.is_some())?;
    let (counts, rest) = read_packed_counts(rest, row_count)?;
    let (offsets, total_child) = counts_to_offsets(&counts)?;

    let hdr = parse_list_child_header(rest, total_child)?;
    let child_col = decode_child_column(
        hdr.child_body,
        hdr.child_enc,
        hdr.child_flags,
        hdr.child_len,
        Some(hdr.child_null_count),
        hdr.child_min_inline_len,
        hdr.child_min_inline,
        inner,
        hdr.dict.as_ref(),
        remaining_depth - 1,
        budget,
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
    null_count: Option<u32>,
    min_inline_len: u8,
    min_inline: [u8; 16],
    dt: &DataType,
    dict: Option<&FileDict>,
    remaining_depth: usize,
    budget: &mut usize,
) -> Result<Column, StatamError> {
    if len == 0 {
        if !body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty child column",
            });
        }
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }
    if enc == Encoding::ConstNull {
        if !body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in ConstNull child column",
            });
        }
        let logical = logical_const_null_bytes(dt, len).ok_or(StatamError::BoundsExceeded {
            reason: "ConstNull child exceeds MAX_BLOCK_RAW_BYTES",
        })?;
        debit_budget(
            budget,
            logical,
            "ConstNull child exceeds MAX_BLOCK_RAW_BYTES",
        )?;
        return Ok(Column::Const {
            value: ScalarValue::Null(dt.clone()),
            len,
        });
    }
    if enc == Encoding::Const {
        if !body.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in Const child column",
            });
        }
        if null_count.is_some_and(|n| n != 0) || (flags & BLOCK_FLAG_HAS_NULLS) != 0 {
            return Err(StatamError::Corrupt {
                reason: "Const child column cannot have nulls",
            });
        }
        let logical = logical_const_value_bytes(dt, usize::from(min_inline_len), len).ok_or(
            StatamError::BoundsExceeded {
                reason: "Const child exceeds MAX_BLOCK_RAW_BYTES",
            },
        )?;
        debit_budget(budget, logical, "Const child exceeds MAX_BLOCK_RAW_BYTES")?;
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
        // Debit dictionary heap bytes before decoding codes, then debit logical materialized
        // bytes so a small dict cannot expand to multi-GiB/TiB downstream.
        let dict_mem = d.memory_bytes();
        let codes_bytes = len.checked_mul(4).ok_or(StatamError::BoundsExceeded {
            reason: "Dict child codes exceed MAX_BLOCK_RAW_BYTES",
        })?;
        if dict_mem
            .checked_add(codes_bytes)
            .is_none_or(|b| b > *budget)
        {
            return Err(StatamError::BoundsExceeded {
                reason: "Dict child exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        debit_budget(budget, dict_mem, "Dict child exceeds MAX_BLOCK_RAW_BYTES")?;
        let (nulls, after_nulls) = read_optional_nulls(body, len, flags)?;
        let col = decode_dict_column(after_nulls, len, nulls, d)?;
        let logical = match &col {
            Column::Dict(dc) => {
                dict_logical_materialized_bytes(dc.values(), dc.codes(), dc.nulls(), *budget)?
            }
            other => other.memory_bytes(),
        };
        debit_budget(
            budget,
            logical.max(codes_bytes),
            "Dict child exceeds MAX_BLOCK_RAW_BYTES",
        )?;
        return Ok(col);
    }

    match dt {
        DataType::List(inner) if enc == Encoding::ListEncoded => {
            decode_list_body(body, len, flags, inner, remaining_depth, budget)
        }
        DataType::MapUtf8 if enc == Encoding::ShreddedMap => Ok(Column::Flat(
            decode_shredded_residual_with_count_budgeted(body, len, flags, budget)?,
        )),
        DataType::MapUtf8 if enc == Encoding::Plain => {
            let (nulls, after_nulls) = read_optional_nulls(body, len, flags)?;
            let data = decode_inner_data(after_nulls, Encoding::Plain, len, dt, nulls.as_ref())?;
            let flat = FlatColumn::try_new(data, nulls)?;
            debit_budget(
                budget,
                flat.memory_bytes(),
                "nested child exceeds MAX_BLOCK_RAW_BYTES",
            )?;
            Ok(Column::Flat(flat))
        }
        DataType::Events if enc == Encoding::EventsStruct => {
            let col = decode_events_body(body, len, flags)?;
            debit_budget(
                budget,
                col.memory_bytes(),
                "nested child exceeds MAX_BLOCK_RAW_BYTES",
            )?;
            Ok(col)
        }
        DataType::Links if enc == Encoding::LinksStruct => {
            let col = decode_links_body(body, len, flags)?;
            debit_budget(
                budget,
                col.memory_bytes(),
                "nested child exceeds MAX_BLOCK_RAW_BYTES",
            )?;
            Ok(col)
        }
        DataType::MapUtf8 | DataType::Events | DataType::Links => Err(StatamError::Corrupt {
            reason: "incompatible nested encoding for DataType",
        }),
        _ => {
            if enc == Encoding::Rle {
                let col = decode_rle_block(body, len, flags, dt)?;
                let logical = match &col {
                    Column::Rle(r) => {
                        logical_rle_bytes(r, dt).ok_or(StatamError::BoundsExceeded {
                            reason: "Rle child logical size exceeds MAX_BLOCK_RAW_BYTES",
                        })?
                    }
                    other => other.memory_bytes(),
                };
                debit_budget(
                    budget,
                    logical,
                    "Rle child logical size exceeds MAX_BLOCK_RAW_BYTES",
                )?;
                return Ok(col);
            }
            if let Some(min_logical) = logical_const_value_bytes(dt, 0, len)
                && min_logical > *budget
            {
                return Err(StatamError::BoundsExceeded {
                    reason: "nested child exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let (nulls, after_nulls) = read_optional_nulls(body, len, flags)?;
            if enc == Encoding::Fsst && (flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 {
                let nulls_bytes = if nulls.is_some() { len.div_ceil(8) } else { 0 };
                let offs_bytes = len.checked_add(1).and_then(|n| n.checked_mul(4)).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "nested child exceeds MAX_BLOCK_RAW_BYTES",
                    },
                )?;
                let base_bytes =
                    nulls_bytes
                        .checked_add(offs_bytes)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "nested child exceeds MAX_BLOCK_RAW_BYTES",
                        })?;
                debit_budget(
                    budget,
                    base_bytes,
                    "nested child exceeds MAX_BLOCK_RAW_BYTES",
                )?;
                let (data, data_bytes) = match dt {
                    DataType::Utf8 => {
                        let u = decode_fsst_utf8_subchunked(after_nulls, len, *budget)?;
                        let n = u.bytes().len();
                        (FlatData::Utf8(u), n)
                    }
                    DataType::Binary => {
                        let b = decode_fsst_binary_subchunked(after_nulls, len, *budget)?;
                        let n = b.bytes().len();
                        (FlatData::Binary(b), n)
                    }
                    _ => {
                        return Err(StatamError::Corrupt {
                            reason: "incompatible encoding for DataType",
                        });
                    }
                };
                debit_budget(
                    budget,
                    data_bytes,
                    "nested child exceeds MAX_BLOCK_RAW_BYTES",
                )?;
                return Ok(Column::Flat(FlatColumn::try_new(data, nulls)?));
            }
            let data = decode_inner_data(after_nulls, enc, len, dt, nulls.as_ref())?;
            let flat = FlatColumn::try_new(data, nulls)?;
            debit_budget(
                budget,
                flat.memory_bytes(),
                "nested child exceeds MAX_BLOCK_RAW_BYTES",
            )?;
            Ok(Column::Flat(flat))
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
    has_subchunked_fsst: bool,
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
            has_subchunked_fsst: false,
        });
    }

    // Try Dict encoding for Dict column or Utf8/Binary/FixedBytes leaf inside List/SubCol.
    if let Column::Dict(d) = col
        && DictValueKind::from_data_type(&d.values().data_type()).is_some()
        && !d.values().has_nulls()
        && let Ok(dict) = FileDict::from_flat(d.values().clone(), false)
        && let Ok(dict_bytes) = dict.encode(Codec::None, 0)
    {
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
            has_subchunked_fsst: false,
        });
    }

    if profile == WriteProfile::L1Compact
        && matches!(
            dt,
            DataType::Utf8 | DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16
        )
        && let Column::Flat(f) = col
        && f.len() >= 4
        && !f.nulls().is_some_and(Bitmap::all_zeros)
    {
        let builder = DictBuilder::l1_compact();
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
                has_subchunked_fsst: false,
            });
        }
    }

    let (enc_block, has_subchunked_fsst) = if matches!(
        dt,
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links
    ) {
        encode_nested_column_inner(col, dt, profile, remaining_depth, None)?
    } else {
        let block = crate::codec::numeric::encode_narrow_column(col, dt, profile, None)?;
        let sub = (block.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0;
        (block, sub)
    };
    Ok(ChildEncodedPayload {
        encoding: enc_block.encoding,
        block_flags: enc_block.block_flags,
        min_inline_len: enc_block.min_inline_len,
        min_inline: enc_block.min_inline,
        inline_dict: None,
        body: enc_block.raw_body,
        has_subchunked_fsst,
    })
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
        let _ = sum_varlen_take_bytes(dict_u.offsets(), &codes, None, MAX_BLOCK_RAW_BYTES)?;
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
    if row_count
        .checked_mul(4)
        .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "packed counts row_count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
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
    if counts
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "nested offsets length exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let mut offsets = Vec::with_capacity(counts.len() + 1);
    offsets.push(0u32);
    let mut total = 0usize;
    for &c in counts {
        total = total
            .checked_add(c as usize)
            .ok_or(StatamError::BoundsExceeded {
                reason: "nested total item count overflow",
            })?;
        if total.checked_mul(4).is_none_or(|b| b > MAX_BLOCK_RAW_BYTES) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_nested_zero_active_validates_headers() {
        let mut mask = Bitmap::new_zeroed(2);
        let _ = mask.set(1, true);

        // 1. ListEncoded with 0 active children (row 0 has items, row 1 is empty, mask selects row 1).
        // row_nulls=0, count_bw=1, count_bytes=1 ([0x01] -> counts [1, 0]), child_enc=Dict(6), ...
        let mut crafted = vec![0u8]; // row_nulls = 0
        crafted.push(1); // bw = 1
        crafted.push(0x01); // row 0 count=1, row 1 count=0
        // 28-byte child header:
        crafted.push(Encoding::Dict as u8); // child_enc = Dict
        crafted.push(0); // child_flags
        crafted.extend_from_slice(&1u32.to_le_bytes()); // child_len = 1
        crafted.extend_from_slice(&0u32.to_le_bytes()); // child_null_count
        crafted.push(0); // child_min_inline_len
        crafted.extend_from_slice(&[0u8; 16]); // child_min_inline
        crafted.push(0); // has_dict = 0 (missing inline dict!)
        crafted.extend_from_slice(&0u32.to_le_bytes()); // child_body_len = 0
        assert!(matches!(
            decode_list_body_selected(&crafted, 2, 0, &DataType::Utf8, 4, &mask),
            Err(StatamError::Corrupt { .. })
        ));

        // Incompatible child_enc (ForBitpack for Utf8) with 0 active children.
        crafted[3] = Encoding::ForBitpack as u8;
        assert!(matches!(
            decode_list_body_selected(&crafted, 2, 0, &DataType::Utf8, 4, &mask),
            Err(StatamError::Corrupt { .. })
        ));

        // 2. EventsStruct with 0 active events (row 0 has 1 event, row 1 has 0 events, mask selects row 1).
        let mut bad_t = vec![0u8, 1u8, 0x01]; // row_nulls=0, bw=1, counts=[1,0]
        bad_t.push(Encoding::Fsst as u8); // invalid t_enc!
        bad_t.extend_from_slice(&8u32.to_le_bytes());
        bad_t.extend_from_slice(&0i64.to_le_bytes());
        bad_t.push(Encoding::Plain as u8); // n_enc
        bad_t.extend_from_slice(&0u32.to_le_bytes());
        bad_t.push(0); // attr_nulls flag = 0
        bad_t.push(Encoding::Plain as u8); // a_enc
        bad_t.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode_events_body_selected(&bad_t, 2, 0, &mask),
            Err(StatamError::Corrupt { .. })
        ));

        // Non-Utf8 NamesSection::Dict with 0 active events:
        let fixed8_dict = FileDict::from_flat(
            FlatColumn::try_new(FlatData::FixedBytes8(vec![[42u8; 8]]), None).unwrap(),
            false,
        )
        .unwrap()
        .encode(Codec::None, 0)
        .unwrap();
        let mut bad_dict = vec![0u8, 1u8, 0x01];
        bad_dict.push(Encoding::Plain as u8);
        bad_dict.extend_from_slice(&8u32.to_le_bytes());
        bad_dict.extend_from_slice(&0i64.to_le_bytes());
        bad_dict.push(Encoding::Dict as u8);
        bad_dict.push(0); // sorted=false
        bad_dict.extend_from_slice(&(fixed8_dict.len() as u32).to_le_bytes());
        bad_dict.extend_from_slice(&fixed8_dict);
        bad_dict.extend_from_slice(&0u32.to_le_bytes()); // c_len = 0
        bad_dict.push(0); // attr_nulls flag = 0
        bad_dict.push(Encoding::Plain as u8);
        bad_dict.extend_from_slice(&0u32.to_le_bytes());
        assert!(matches!(
            decode_events_body_selected(&bad_dict, 2, 0, &mask),
            Err(StatamError::Corrupt { .. })
        ));

        // EventsStruct with Rle times encoding must be rejected (m14).
        let mut rle_t = bad_t.clone();
        rle_t[3] = Encoding::Rle as u8;
        assert!(matches!(
            decode_events_body_selected(&rle_t, 2, 0, &mask),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn nested_bombs_return_bounds_exceeded_or_corrupt_immediately() {
        // 1. 30-byte ShreddedMap residual bomb: 1 row claiming 16,000,000 entries, num_keys=1,
        //    klen_bw=0, keys_raw_bytes=0, kcode_bw=0.
        let mut res_bomb = Vec::new();
        let entry_count = 16_000_000u32;
        let cnt_bw = min_bits_u32(&[entry_count]);
        res_bomb.push(cnt_bw);
        pack_u32(&[entry_count], cnt_bw, &mut res_bomb).unwrap();
        res_bomb.extend_from_slice(&1u32.to_le_bytes()); // num_keys = 1
        res_bomb.push(0); // klen_bw = 0
        res_bomb.extend_from_slice(&0u32.to_le_bytes()); // keys_raw_bytes = 0
        res_bomb.push(0); // kcode_bw = 0
        res_bomb.push(0); // val_nulls = None
        res_bomb.push(Encoding::Fsst as u8);
        res_bomb.extend_from_slice(&2u32.to_le_bytes());
        res_bomb.extend_from_slice(&[0u8, 0u8]); // sym_count=0, len_bw=0
        assert!(res_bomb.len() <= 30);
        assert!(matches!(
            decode_shredded_residual_with_count(&res_bomb, 1, 0),
            Err(StatamError::BoundsExceeded { .. } | StatamError::Corrupt { .. })
        ));

        // 2. 8-level List<List<...<I64>>> bomb where each level claims millions of elements via bw=0 / small counts.
        let mut dt = DataType::I64;
        // Innermost leaf: ConstNull of 4,000,000 I64s (32 MiB) inside 7 outer list levels each with 4,000,000 offsets (32 MiB/level).
        let level_n = 4_000_000u32;
        let mut child_body = Vec::new();
        let mut child_enc = Encoding::ConstNull;
        for _ in 0..8 {
            let mut body = Vec::new();
            let bw = min_bits_u32(&[level_n]);
            body.push(bw);
            pack_u32(&[level_n], bw, &mut body).unwrap();
            body.push(child_enc as u8);
            body.push(0); // child_flags
            body.extend_from_slice(&level_n.to_le_bytes()); // child_len
            body.extend_from_slice(&0u32.to_le_bytes()); // child_null_count
            body.push(0); // min_inline_len
            body.extend_from_slice(&[0u8; 16]);
            body.push(0); // has_dict = 0
            body.extend_from_slice(&(child_body.len() as u32).to_le_bytes());
            body.extend_from_slice(&child_body);

            child_body = body;
            child_enc = Encoding::ListEncoded;
            dt = DataType::List(Box::new(dt));
        }
        let DataType::List(outer_inner) = &dt else {
            unreachable!();
        };
        let mut budget = MAX_BLOCK_RAW_BYTES;
        assert!(matches!(
            decode_list_body(&child_body, 1, 0, outer_inner, 8, &mut budget),
            Err(StatamError::BoundsExceeded { .. } | StatamError::Corrupt { .. })
        ));

        // 3. List<Utf8> Rle petabyte bomb: 1 run repeating a 1 MiB string 16,000,000 times (~16 TB).
        let big_str = "x".repeat(1024 * 1024);
        let rle_len = 16_000_000u32;
        let mut rle_body = Vec::new();
        rle_body.extend_from_slice(&1u32.to_le_bytes()); // num_runs = 1
        let re_bw = min_bits_u32(&[rle_len]);
        rle_body.push(re_bw);
        pack_u32(&[rle_len], re_bw, &mut rle_body).unwrap();
        rle_body.push(Encoding::Plain as u8);
        rle_body.extend_from_slice(&0u32.to_le_bytes());
        rle_body.extend_from_slice(&(big_str.len() as u32).to_le_bytes());
        rle_body.extend_from_slice(big_str.as_bytes());

        let mut list_rle = Vec::new();
        let l_bw = min_bits_u32(&[rle_len]);
        list_rle.push(l_bw);
        pack_u32(&[rle_len], l_bw, &mut list_rle).unwrap();
        list_rle.push(Encoding::Rle as u8);
        list_rle.push(0); // child_flags
        list_rle.extend_from_slice(&rle_len.to_le_bytes());
        list_rle.extend_from_slice(&0u32.to_le_bytes());
        list_rle.push(0);
        list_rle.extend_from_slice(&[0u8; 16]);
        list_rle.push(0); // has_dict = 0
        list_rle.extend_from_slice(&(rle_body.len() as u32).to_le_bytes());
        list_rle.extend_from_slice(&rle_body);
        let mut budget = MAX_BLOCK_RAW_BYTES;
        assert!(matches!(
            decode_list_body(&list_rle, 1, 0, &DataType::Utf8, 4, &mut budget),
            Err(StatamError::BoundsExceeded { .. } | StatamError::Corrupt { .. })
        ));

        // 4. List<Utf8> small-dict 3 TiB bomb: 1-entry 200 KiB dict (<= 256 KiB passthrough)
        //    with 16,000,000 zero codes (bw=0).
        let big_entry = "z".repeat(200 * 1024);
        let small_dict = FileDict::from_flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([big_entry.as_str()]).unwrap()),
                None,
            )
            .unwrap(),
            true,
        )
        .unwrap();
        assert!(small_dict.memory_bytes() <= DICT_PASSTHROUGH_MAX_BYTES);
        let dict_bytes = small_dict.encode(Codec::None, 0).unwrap();
        let dict_codes_body = [0u8, 0, 0, 0, 0, 0]; // sub_enc=ForBitpack, base=0, bw=0 (0 packed bytes)
        let child_n = 16_000_000u32;
        let mut list_dict = Vec::new();
        let ld_bw = min_bits_u32(&[child_n]);
        list_dict.push(ld_bw);
        pack_u32(&[child_n], ld_bw, &mut list_dict).unwrap();
        list_dict.push(Encoding::Dict as u8);
        list_dict.push(0); // child_flags
        list_dict.extend_from_slice(&child_n.to_le_bytes());
        list_dict.extend_from_slice(&0u32.to_le_bytes());
        list_dict.push(0);
        list_dict.extend_from_slice(&[0u8; 16]);
        list_dict.push(1); // has_dict = 1
        list_dict.push(1); // sorted = true
        list_dict.extend_from_slice(&(dict_bytes.len() as u32).to_le_bytes());
        list_dict.extend_from_slice(&dict_bytes);
        list_dict.extend_from_slice(&(dict_codes_body.len() as u32).to_le_bytes());
        list_dict.extend_from_slice(&dict_codes_body);
        let mut budget = MAX_BLOCK_RAW_BYTES;
        assert!(matches!(
            decode_list_body(&list_dict, 1, 0, &DataType::Utf8, 4, &mut budget),
            Err(StatamError::BoundsExceeded { .. } | StatamError::Corrupt { .. })
        ));

        // 5. ConstNull(FixedBytes16) 16M-elem 256 MiB bomb rejected by logical budget.
        let mut list_cnull = Vec::new();
        list_cnull.push(ld_bw);
        pack_u32(&[child_n], ld_bw, &mut list_cnull).unwrap();
        list_cnull.push(Encoding::ConstNull as u8);
        list_cnull.push(0);
        list_cnull.extend_from_slice(&child_n.to_le_bytes());
        list_cnull.extend_from_slice(&child_n.to_le_bytes());
        list_cnull.push(0);
        list_cnull.extend_from_slice(&[0u8; 16]);
        list_cnull.push(0);
        list_cnull.extend_from_slice(&0u32.to_le_bytes());
        let mut budget = MAX_BLOCK_RAW_BYTES;
        assert!(matches!(
            decode_list_body(&list_cnull, 1, 0, &DataType::FixedBytes16, 4, &mut budget),
            Err(StatamError::BoundsExceeded { .. })
        ));
    }

    #[test]
    fn list_with_non_byte_and_nullable_dict_children_falls_back_cleanly() {
        // 1. List<I64> whose child is Column::Dict<I64> (unsupported DictValueKind):
        let i64_vals = FlatColumn::try_new(FlatData::I64(vec![10, 20, 30]), None).unwrap();
        let i64_dict =
            Column::Dict(DictColumn::try_new(i64_vals, vec![0, 1, 2, 1, 0], None).unwrap());
        let list_i64 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::I64,
                    offsets: vec![0, 2, 5],
                    values: Box::new(i64_dict),
                },
                None,
            )
            .unwrap(),
        );
        let dt_i64 = DataType::List(Box::new(DataType::I64));
        let enc_i64 =
            encode_nested_column(&list_i64, &dt_i64, WriteProfile::L1Compact, 4, None).unwrap();
        let entry_i64 = BlockEntry {
            first_row: 0,
            row_count: 2,
            null_count: enc_i64.null_count,
            offset: 0,
            stored_len: 0,
            raw_len: enc_i64.raw_body.len() as u32,
            encoding: enc_i64.encoding,
            codec: Codec::None,
            block_flags: enc_i64.block_flags,
            min_inline_len: enc_i64.min_inline_len,
            max_inline_len: enc_i64.max_inline_len,
            reserved: [0; 3],
            min_inline: enc_i64.min_inline,
            max_inline: enc_i64.max_inline,
        };
        let dec_i64 = decode_nested_column(&enc_i64.raw_body, &entry_i64, &dt_i64, 4).unwrap();
        assert!(dec_i64.semantic_eq(&list_i64));

        // 2. List<Utf8> whose child is Column::Dict<Utf8> with nulls inside dict.values():
        let dict_vals_null = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["ok", "", "done"]).unwrap()),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let utf8_dict =
            Column::Dict(DictColumn::try_new(dict_vals_null, vec![0, 1, 2, 0], None).unwrap());
        let list_u = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 2, 4],
                    values: Box::new(utf8_dict),
                },
                None,
            )
            .unwrap(),
        );
        let dt_u = DataType::List(Box::new(DataType::Utf8));
        let enc_u = encode_nested_column(&list_u, &dt_u, WriteProfile::L1Compact, 4, None).unwrap();
        let entry_u = BlockEntry {
            first_row: 0,
            row_count: 2,
            null_count: enc_u.null_count,
            offset: 0,
            stored_len: 0,
            raw_len: enc_u.raw_body.len() as u32,
            encoding: enc_u.encoding,
            codec: Codec::None,
            block_flags: enc_u.block_flags,
            min_inline_len: enc_u.min_inline_len,
            max_inline_len: enc_u.max_inline_len,
            reserved: [0; 3],
            min_inline: enc_u.min_inline,
            max_inline: enc_u.max_inline,
        };
        let dec_u = decode_nested_column(&enc_u.raw_body, &entry_u, &dt_u, 4).unwrap();
        assert!(dec_u.semantic_eq(&list_u));
    }

    #[test]
    fn nested_minors_plain_map_large_dict_subcol_and_const_child_validation() {
        // m15: L0Fast Plain MapUtf8 routes cleanly through decode_nested_column & decode_nested_column_selected.
        let map = MapData::try_new(
            vec![0, 1, 2, 2, 3],
            Utf8Data::try_from_slices(["a", "b", "c"]).unwrap(),
            Utf8Data::try_from_slices(["1", "2", "3"]).unwrap(),
            None,
        )
        .unwrap();
        let map_col =
            Column::Flat(FlatColumn::try_new(FlatData::MapUtf8(Box::new(map)), None).unwrap());
        let enc_map =
            encode_nested_column(&map_col, &DataType::MapUtf8, WriteProfile::L0Fast, 4, None)
                .unwrap();
        assert_eq!(enc_map.encoding, Encoding::Plain);
        let entry_map = BlockEntry {
            first_row: 0,
            row_count: 4,
            null_count: enc_map.null_count,
            offset: 0,
            stored_len: 0,
            raw_len: enc_map.raw_body.len() as u32,
            encoding: enc_map.encoding,
            codec: Codec::None,
            block_flags: enc_map.block_flags,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        let dec_full =
            decode_nested_column(&enc_map.raw_body, &entry_map, &DataType::MapUtf8, 4).unwrap();
        assert!(dec_full.semantic_eq(&map_col));
        let mut mask = Bitmap::new_zeroed(4);
        let _ = mask.set(0, true);
        let dec_sel = decode_nested_column_selected(
            &enc_map.raw_body,
            &entry_map,
            &DataType::MapUtf8,
            4,
            &mask,
        )
        .unwrap();
        assert_eq!(
            dec_sel.get_scalar(0).unwrap(),
            map_col.get_scalar(0).unwrap()
        );

        // m16: large-dict (> 256 KiB) ShreddedSubCol dispatches to decode_dict_selected.
        let pad = "p".repeat(4096);
        let dict_vals: Vec<String> = (0..80).map(|k| format!("k{k:03}-{pad}")).collect();
        let row_count = 256usize;
        let row_strs: Vec<&str> = (0..row_count).map(|i| dict_vals[i % 80].as_str()).collect();
        let sub = ShreddedSubCol {
            in_map: Bitmap::new_ones(row_count),
            value_null: Bitmap::new_ones(row_count),
            values: Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(row_strs.iter().copied()).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        };
        let encoded_sub = encode_shredded_subcol(&sub, WriteProfile::L1Compact).unwrap();
        let mut sub_mask = Bitmap::new_zeroed(row_count);
        let _ = sub_mask.set(7, true);
        let _ = sub_mask.set(199, true);
        let dec_sub = decode_shredded_subcol_selected(&encoded_sub, row_count, &sub_mask).unwrap();
        assert_eq!(
            dec_sub.values.get_scalar(7).unwrap(),
            sub.values.get_scalar(7).unwrap()
        );
        assert_eq!(
            dec_sub.values.get_scalar(199).unwrap(),
            sub.values.get_scalar(199).unwrap()
        );
        assert!(dec_sub.values.is_null(0));

        // m14: decode_child_column rejects trailing bytes on Const/ConstNull and nulls on Const.
        let mut budget = MAX_BLOCK_RAW_BYTES;
        assert!(matches!(
            decode_child_column(
                &[1u8],
                Encoding::Const,
                0,
                4,
                Some(0),
                1,
                [1u8; 16],
                &DataType::Bool,
                None,
                2,
                &mut budget,
            ),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_child_column(
                &[],
                Encoding::Const,
                0,
                4,
                Some(1),
                1,
                [1u8; 16],
                &DataType::Bool,
                None,
                2,
                &mut budget,
            ),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn nested_fsst_sparse_decode_20k_100b_is_fast_and_exact() {
        // B9: 20,000 x ~100 B FSST-encoded strings inside a List<Utf8> (row 0 holds all 20,000 items,
        // rows 1..64 are empty; selecting only row 0 triggers sparse decode_selected_utf8_or_binary).
        let item_count = 20_000usize;
        let row_count = 64usize;
        let items: Vec<String> = (0..item_count)
            .map(|i| {
                format!(
                    "https://observability.cetra.sh/api/v1/spans/trace_{i:06}/events/item_{:04}/payload_end",
                    i % 997
                )
            })
            .collect();
        let child_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(items.iter().map(String::as_str)).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let mut offsets = Vec::with_capacity(row_count + 1);
        offsets.push(0u32);
        offsets.resize(row_count + 1, item_count as u32);
        let list_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets,
                    values: Box::new(child_col),
                },
                None,
            )
            .unwrap(),
        );
        let dt = DataType::List(Box::new(DataType::Utf8));
        let enc = encode_nested_column(&list_col, &dt, WriteProfile::L1Compact, 4, None).unwrap();
        let entry = BlockEntry {
            first_row: 0,
            row_count: row_count as u16,
            null_count: enc.null_count,
            offset: 0,
            stored_len: 0,
            raw_len: enc.raw_body.len() as u32,
            encoding: enc.encoding,
            codec: Codec::None,
            block_flags: enc.block_flags,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        let mut mask = Bitmap::new_zeroed(row_count);
        let _ = mask.set(0, true);
        let dec = decode_nested_column_selected(&enc.raw_body, &entry, &dt, 4, &mask).unwrap();
        let Column::Flat(fc) = &dec else {
            panic!("expected Flat List");
        };
        let FlatData::List {
            offsets: dec_offs,
            values: dec_vals,
            ..
        } = fc.data()
        else {
            panic!("expected FlatData::List");
        };
        assert_eq!(dec_offs[0], 0);
        assert_eq!(dec_offs[1] as usize, item_count);
        let Column::Flat(vf) = dec_vals.as_ref() else {
            panic!("expected Flat child");
        };
        let FlatData::Utf8(u) = vf.data() else {
            panic!("expected Utf8 child");
        };
        assert_eq!(u.len(), item_count);
        assert_eq!(u.get(0), Some(items[0].as_str()));
        assert_eq!(u.get(item_count - 1), Some(items[item_count - 1].as_str()));
    }
}
