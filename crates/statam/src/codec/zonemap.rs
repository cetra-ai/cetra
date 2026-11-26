//! Sound mini-block zone-map construction and predicate pruning.
//!
//! Enforces every soundness invariant of `spec-v0.md` §3.4.4:
//! - `NaN` is excluded from `min_inline`/`max_inline` and sets `BLOCK_FLAG_HAS_NAN`;
//!   if all non-null values are `NaN`, `BLOCK_FLAG_MIN_MAX_VALID` is cleared.
//! - When `HAS_NAN == 1`, `>`, `>=`, `!=`, `= NaN`, and `IS NOT NULL` are NEVER pruned.
//! - `-0.0` is canonicalized to `+0.0`.
//! - `Utf8` / `Binary` bounds are raw-byte prefixes up to 16 B, with carry-increment
//!   on `max_inline` when true max length `> 16` (clearing `MIN_MAX_VALID` on
//!   all-`0xFF` overflow).
//! - `Decimal128` stores full 16 B `i128` LE bounds.
//! - `Interval`, `List`, `MapUtf8`, `Events`, `Links` always clear `MIN_MAX_VALID`.
//! - `Dict` range/prefix pruning requires `COL_FLAG_DICT_SORTED`.

use std::fmt;

use cetra_column::{Column, DataType, FlatColumn, FlatData, ScalarValue};

use crate::codec::dict::FileDict;
use crate::format::{
    BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_HAS_NAN, BLOCK_FLAG_HAS_NULLS, BLOCK_FLAG_MIN_MAX_VALID,
    BlockEntry, COL_FLAG_DICT_SORTED, Encoding,
};

/// Zone-map metadata computed for one mini-block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneMapSummary {
    /// `BLOCK_FLAG_*` bitmask (`HAS_NULLS`, `ALL_NULL`, `MIN_MAX_VALID`, `HAS_NAN`).
    pub block_flags: u8,
    /// Number of null rows in the mini-block.
    pub null_count: u16,
    /// Active prefix length of `min_inline` (`0..=16`).
    pub min_inline_len: u8,
    /// Active prefix length of `max_inline` (`0..=16`).
    pub max_inline_len: u8,
    /// Lower bound (or constant value when `Encoding::Const`).
    pub min_inline: [u8; 16],
    /// Upper bound.
    pub max_inline: [u8; 16],
}

impl ZoneMapSummary {
    fn empty() -> Self {
        Self {
            block_flags: 0,
            null_count: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            min_inline: [0u8; 16],
            max_inline: [0u8; 16],
        }
    }
}

/// Typed scalar literal for zone-map predicate pruning.
#[derive(Clone, PartialEq)]
pub enum ZoneValue {
    /// Boolean literal.
    Bool(bool),
    /// Signed 64-bit integer literal.
    I64(i64),
    /// IEEE-754 64-bit float literal.
    F64(f64),
    /// 128-bit decimal unscaled literal.
    Decimal128(i128),
    /// Nanosecond timestamp literal.
    TimestampNs(i64),
    /// Days-since-epoch date literal.
    Date(i32),
    /// Raw byte literal (`Utf8`, `Binary`, `FixedBytes8`, `FixedBytes16`).
    Bytes(Vec<u8>),
}

impl fmt::Debug for ZoneValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(v) => f.debug_tuple("Bool").field(v).finish(),
            Self::I64(v) => f.debug_tuple("I64").field(v).finish(),
            Self::F64(v) => f.debug_tuple("F64").field(v).finish(),
            Self::Decimal128(_) => f.write_str("Decimal128"),
            Self::TimestampNs(_) => f.write_str("TimestampNs"),
            Self::Date(v) => f.debug_tuple("Date").field(v).finish(),
            Self::Bytes(b) => f.debug_struct("Bytes").field("len", &b.len()).finish(),
        }
    }
}

/// Predicate evaluated against a mini-block's [`BlockEntry`] zone map.
#[derive(Clone, PartialEq)]
pub enum ZonePred {
    /// `col = v`
    Eq(ZoneValue),
    /// `col != v`
    Ne(ZoneValue),
    /// `col < v`
    Lt(ZoneValue),
    /// `col <= v`
    Le(ZoneValue),
    /// `col > v`
    Gt(ZoneValue),
    /// `col >= v`
    Ge(ZoneValue),
    /// `lo <= col && col <= hi`
    Between(ZoneValue, ZoneValue),
    /// `col IN (...)`
    In(Vec<ZoneValue>),
    /// `col IS NULL`
    IsNull,
    /// `col IS NOT NULL`
    IsNotNull,
    /// `col LIKE 'prefix%'` (raw byte prefix).
    Prefix(Vec<u8>),
}

impl fmt::Debug for ZonePred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eq(v) => f.debug_tuple("Eq").field(v).finish(),
            Self::Ne(v) => f.debug_tuple("Ne").field(v).finish(),
            Self::Lt(v) => f.debug_tuple("Lt").field(v).finish(),
            Self::Le(v) => f.debug_tuple("Le").field(v).finish(),
            Self::Gt(v) => f.debug_tuple("Gt").field(v).finish(),
            Self::Ge(v) => f.debug_tuple("Ge").field(v).finish(),
            Self::Between(lo, hi) => f.debug_tuple("Between").field(lo).field(hi).finish(),
            Self::In(vs) => f.debug_struct("In").field("count", &vs.len()).finish(),
            Self::IsNull => f.write_str("IsNull"),
            Self::IsNotNull => f.write_str("IsNotNull"),
            Self::Prefix(p) => f.debug_struct("Prefix").field("len", &p.len()).finish(),
        }
    }
}

#[inline]
fn canonicalize_f64(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { v }
}

/// Increments a 16-byte big-endian prefix by 1 with left-carry.
/// Returns `None` if all 16 bytes are `0xFF`.
pub(crate) fn carry_increment_16(prefix: &[u8; 16]) -> Option<[u8; 16]> {
    let mut out = *prefix;
    for b in out.iter_mut().rev() {
        if *b == 0xFF {
            *b = 0x00;
        } else {
            *b += 1;
            return Some(out);
        }
    }
    None
}

fn next_prefix(prefix: &[u8]) -> Option<Vec<u8>> {
    if prefix.is_empty() {
        return None;
    }
    let mut out = prefix.to_vec();
    for b in out.iter_mut().rev() {
        if *b == 0xFF {
            *b = 0x00;
        } else {
            *b += 1;
            return Some(out);
        }
    }
    None
}

fn count_nulls_col(col: &Column) -> usize {
    match col {
        Column::Flat(f) => f.nulls().map_or_else(
            || usize::from(matches!(f.data(), FlatData::Null(_))) * f.len(),
            |bm| bm.count_zeros(),
        ),
        Column::Const { value, len } => {
            if value.is_null() {
                *len
            } else {
                0
            }
        }
        Column::Dict(d) => {
            let mut n = 0usize;
            for i in 0..d.len() {
                if d.is_null(i) {
                    n += 1;
                }
            }
            n
        }
        Column::Rle(r) => {
            if !r.has_nulls() {
                return 0;
            }
            let mut n = 0usize;
            let mut prev = 0u32;
            for (run_idx, &end) in r.run_ends().iter().enumerate() {
                if r.values().is_null(run_idx) {
                    n += (end - prev) as usize;
                }
                prev = end;
            }
            n
        }
    }
}

/// Computes a sound [`ZoneMapSummary`] for `col` (at most 4,096 rows per mini-block).
pub fn compute_zone_map(col: &Column, _dict_sorted: bool) -> ZoneMapSummary {
    let len = col.len();
    if len == 0 {
        return ZoneMapSummary::empty();
    }

    let null_count_usize = count_nulls_col(col);
    let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
    let mut flags = if null_count_usize > 0 {
        BLOCK_FLAG_HAS_NULLS
    } else {
        0
    };
    if null_count_usize == len {
        flags |= BLOCK_FLAG_ALL_NULL;
        let mut s = ZoneMapSummary::empty();
        s.block_flags = flags;
        s.null_count = null_count;
        return s;
    }

    if let Column::Dict(d) = col {
        let mut min_code = u32::MAX;
        let mut max_code = 0u32;
        let mut any = false;
        for (i, &code) in d.codes().iter().enumerate() {
            if !d.is_null(i) {
                any = true;
                min_code = min_code.min(code);
                max_code = max_code.max(code);
            }
        }
        let mut s = ZoneMapSummary::empty();
        s.null_count = null_count;
        if any {
            s.block_flags = flags | BLOCK_FLAG_MIN_MAX_VALID;
            s.min_inline_len = 4;
            s.max_inline_len = 4;
            s.min_inline[0..4].copy_from_slice(&min_code.to_le_bytes());
            s.max_inline[0..4].copy_from_slice(&max_code.to_le_bytes());
        } else {
            s.block_flags = flags | BLOCK_FLAG_ALL_NULL;
        }
        return s;
    }

    if let Column::Const { value, .. } = col {
        return zone_map_from_scalar(value, null_count, flags);
    }

    if let Column::Rle(r) = col {
        return zone_map_from_flat(r.values(), null_count, flags);
    }

    if let Column::Flat(f) = col {
        return zone_map_from_flat(f, null_count, flags);
    }

    ZoneMapSummary::empty()
}

fn zone_map_from_scalar(value: &ScalarValue, null_count: u16, base_flags: u8) -> ZoneMapSummary {
    let mut s = ZoneMapSummary::empty();
    s.null_count = null_count;
    s.block_flags = base_flags;
    match value {
        ScalarValue::Null(_) => {
            s.block_flags |= BLOCK_FLAG_ALL_NULL;
        }
        ScalarValue::Bool(b) => {
            s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
            s.min_inline_len = 1;
            s.max_inline_len = 1;
            s.min_inline[0] = u8::from(*b);
            s.max_inline[0] = u8::from(*b);
        }
        ScalarValue::I64(v) | ScalarValue::TimestampNs { value: v, .. } => {
            s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
            s.min_inline_len = 8;
            s.max_inline_len = 8;
            s.min_inline[0..8].copy_from_slice(&v.to_le_bytes());
            s.max_inline[0..8].copy_from_slice(&v.to_le_bytes());
        }
        ScalarValue::F64(v) => {
            if v.is_nan() {
                s.block_flags |= BLOCK_FLAG_HAS_NAN;
                // Keep exact NaN bits in min_inline for Const decode, but MIN_MAX_VALID = 0.
                s.min_inline_len = 8;
                s.min_inline[0..8].copy_from_slice(&v.to_bits().to_le_bytes());
            } else {
                let c = canonicalize_f64(*v);
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 8;
                s.max_inline_len = 8;
                // Preserve exact bits in min_inline for Const decode (-0.0 vs +0.0);
                // zonemap reader canonicalizes -0.0 -> +0.0.
                s.min_inline[0..8].copy_from_slice(&v.to_bits().to_le_bytes());
                s.max_inline[0..8].copy_from_slice(&c.to_bits().to_le_bytes());
            }
        }
        ScalarValue::Decimal128 { value: v, .. } => {
            s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
            s.min_inline_len = 16;
            s.max_inline_len = 16;
            s.min_inline.copy_from_slice(&v.to_le_bytes());
            s.max_inline.copy_from_slice(&v.to_le_bytes());
        }
        ScalarValue::Date(v) => {
            s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
            s.min_inline_len = 4;
            s.max_inline_len = 4;
            s.min_inline[0..4].copy_from_slice(&v.to_le_bytes());
            s.max_inline[0..4].copy_from_slice(&v.to_le_bytes());
        }
        ScalarValue::FixedBytes8(b) => {
            s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
            s.min_inline_len = 8;
            s.max_inline_len = 8;
            s.min_inline[0..8].copy_from_slice(b);
            s.max_inline[0..8].copy_from_slice(b);
        }
        ScalarValue::FixedBytes16(b) => {
            s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
            s.min_inline_len = 16;
            s.max_inline_len = 16;
            s.min_inline.copy_from_slice(b);
            s.max_inline.copy_from_slice(b);
        }
        ScalarValue::Utf8(str_val) => {
            set_varbytes_bounds(str_val.as_bytes(), str_val.as_bytes(), &mut s);
        }
        ScalarValue::Binary(bytes) => {
            set_varbytes_bounds(bytes, bytes, &mut s);
        }
        ScalarValue::Interval(_)
        | ScalarValue::List { .. }
        | ScalarValue::MapUtf8(_)
        | ScalarValue::Events(_)
        | ScalarValue::Links(_) => {
            // MIN_MAX_VALID stays 0.
        }
    }
    s
}

fn zone_map_from_flat(flat: &FlatColumn, null_count: u16, base_flags: u8) -> ZoneMapSummary {
    let mut s = ZoneMapSummary::empty();
    s.null_count = null_count;
    s.block_flags = base_flags;
    let nulls = flat.nulls();
    let n = flat.len();

    match flat.data() {
        FlatData::Null(_) => {
            s.block_flags |= BLOCK_FLAG_ALL_NULL;
        }
        FlatData::Bool(bm) => {
            let mut min_v = true;
            let mut max_v = false;
            let mut any = false;
            for i in 0..n {
                if nulls.is_none_or(|nbm| nbm.get(i)) {
                    let b = bm.get(i);
                    any = true;
                    min_v &= b;
                    max_v |= b;
                }
            }
            if any {
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 1;
                s.max_inline_len = 1;
                s.min_inline[0] = u8::from(min_v);
                s.max_inline[0] = u8::from(max_v);
            }
        }
        FlatData::I64(vals) | FlatData::TimestampNs { values: vals, .. } => {
            let mut min_v = i64::MAX;
            let mut max_v = i64::MIN;
            let mut any = false;
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    any = true;
                    min_v = min_v.min(v);
                    max_v = max_v.max(v);
                }
            }
            if any {
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 8;
                s.max_inline_len = 8;
                s.min_inline[0..8].copy_from_slice(&min_v.to_le_bytes());
                s.max_inline[0..8].copy_from_slice(&max_v.to_le_bytes());
            }
        }
        FlatData::F64(vals) => {
            let mut has_nan = false;
            let mut min_v = f64::INFINITY;
            let mut max_v = f64::NEG_INFINITY;
            let mut any_non_nan = false;
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_some_and(|bm| !bm.get(i)) {
                    continue;
                }
                if v.is_nan() {
                    has_nan = true;
                } else {
                    let c = canonicalize_f64(v);
                    if !any_non_nan {
                        min_v = c;
                        max_v = c;
                        any_non_nan = true;
                    } else {
                        if c < min_v {
                            min_v = c;
                        }
                        if c > max_v {
                            max_v = c;
                        }
                    }
                }
            }
            if has_nan {
                s.block_flags |= BLOCK_FLAG_HAS_NAN;
            }
            if any_non_nan {
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 8;
                s.max_inline_len = 8;
                s.min_inline[0..8].copy_from_slice(&min_v.to_bits().to_le_bytes());
                s.max_inline[0..8].copy_from_slice(&max_v.to_bits().to_le_bytes());
            }
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
            if any {
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 16;
                s.max_inline_len = 16;
                s.min_inline.copy_from_slice(&min_v.to_le_bytes());
                s.max_inline.copy_from_slice(&max_v.to_le_bytes());
            }
        }
        FlatData::Date(vals) => {
            let mut min_v = i32::MAX;
            let mut max_v = i32::MIN;
            let mut any = false;
            for (i, &v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    any = true;
                    min_v = min_v.min(v);
                    max_v = max_v.max(v);
                }
            }
            if any {
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 4;
                s.max_inline_len = 4;
                s.min_inline[0..4].copy_from_slice(&min_v.to_le_bytes());
                s.max_inline[0..4].copy_from_slice(&max_v.to_le_bytes());
            }
        }
        FlatData::FixedBytes8(vals) => {
            let mut min_v = [0xFFu8; 8];
            let mut max_v = [0x00u8; 8];
            let mut any = false;
            for (i, v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    if !any {
                        min_v = *v;
                        max_v = *v;
                        any = true;
                    } else {
                        if *v < min_v {
                            min_v = *v;
                        }
                        if *v > max_v {
                            max_v = *v;
                        }
                    }
                }
            }
            if any {
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 8;
                s.max_inline_len = 8;
                s.min_inline[0..8].copy_from_slice(&min_v);
                s.max_inline[0..8].copy_from_slice(&max_v);
            }
        }
        FlatData::FixedBytes16(vals) => {
            let mut min_v = [0xFFu8; 16];
            let mut max_v = [0x00u8; 16];
            let mut any = false;
            for (i, v) in vals.iter().enumerate() {
                if nulls.is_none_or(|bm| bm.get(i)) {
                    if !any {
                        min_v = *v;
                        max_v = *v;
                        any = true;
                    } else {
                        if *v < min_v {
                            min_v = *v;
                        }
                        if *v > max_v {
                            max_v = *v;
                        }
                    }
                }
            }
            if any {
                s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
                s.min_inline_len = 16;
                s.max_inline_len = 16;
                s.min_inline.copy_from_slice(&min_v);
                s.max_inline.copy_from_slice(&max_v);
            }
        }
        FlatData::Utf8(u) => {
            let mut min_s: Option<&[u8]> = None;
            let mut max_s: Option<&[u8]> = None;
            for i in 0..n {
                if nulls.is_none_or(|bm| bm.get(i))
                    && let Some(val) = u.get(i)
                {
                    let b = val.as_bytes();
                    min_s = Some(match min_s {
                        Some(m) if m <= b => m,
                        _ => b,
                    });
                    max_s = Some(match max_s {
                        Some(m) if m >= b => m,
                        _ => b,
                    });
                }
            }
            if let (Some(mn), Some(mx)) = (min_s, max_s) {
                set_varbytes_bounds(mn, mx, &mut s);
            }
        }
        FlatData::Binary(bdata) => {
            let mut min_s: Option<&[u8]> = None;
            let mut max_s: Option<&[u8]> = None;
            for i in 0..n {
                if nulls.is_none_or(|bm| bm.get(i))
                    && let Some(b) = bdata.get(i)
                {
                    min_s = Some(match min_s {
                        Some(m) if m <= b => m,
                        _ => b,
                    });
                    max_s = Some(match max_s {
                        Some(m) if m >= b => m,
                        _ => b,
                    });
                }
            }
            if let (Some(mn), Some(mx)) = (min_s, max_s) {
                set_varbytes_bounds(mn, mx, &mut s);
            }
        }
        FlatData::Interval(_)
        | FlatData::List { .. }
        | FlatData::MapUtf8(_)
        | FlatData::Events(_)
        | FlatData::Links(_) => {
            // MIN_MAX_VALID stays 0.
        }
    }

    s
}

fn set_varbytes_bounds(min_bytes: &[u8], max_bytes: &[u8], s: &mut ZoneMapSummary) {
    let min_len = min_bytes.len().min(16);
    s.min_inline_len = min_len as u8;
    s.min_inline[..min_len].copy_from_slice(&min_bytes[..min_len]);

    if max_bytes.len() <= 16 {
        let max_len = max_bytes.len();
        s.max_inline_len = max_len as u8;
        s.max_inline[..max_len].copy_from_slice(max_bytes);
        s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
    } else {
        let mut prefix = [0u8; 16];
        prefix.copy_from_slice(&max_bytes[..16]);
        if let Some(upper) = carry_increment_16(&prefix) {
            s.max_inline_len = 16;
            s.max_inline.copy_from_slice(&upper);
            s.block_flags |= BLOCK_FLAG_MIN_MAX_VALID;
        } else {
            // All 16 prefix bytes are 0xFF: upper bound overflows -> clear MIN_MAX_VALID.
            s.block_flags &= !BLOCK_FLAG_MIN_MAX_VALID;
            s.max_inline_len = 0;
        }
    }
}

/// Evaluates whether a mini-block described by `entry` may contain any row matching `pred`.
///
/// Returning `true` is always safe; returning `false` means the mini-block is
/// guaranteed to have zero matching rows and may be skipped.
pub fn zone_map_may_match(
    entry: &BlockEntry,
    dt: &DataType,
    col_flags: u8,
    pred: &ZonePred,
    dict: Option<&FileDict>,
) -> bool {
    if entry.row_count == 0 {
        return false;
    }

    let has_nan = (entry.block_flags & BLOCK_FLAG_HAS_NAN) != 0;

    match pred {
        ZonePred::IsNull => return entry.null_count > 0,
        ZonePred::IsNotNull => {
            if matches!(dt, DataType::F64) && has_nan {
                return true;
            }
            return entry.null_count < entry.row_count;
        }
        _ => {}
    }

    if entry.null_count == entry.row_count || (entry.block_flags & BLOCK_FLAG_ALL_NULL) != 0 {
        return false;
    }

    if entry.encoding == Encoding::Dict {
        return dict_zone_map_may_match(entry, col_flags, pred, dict);
    }

    let min_max_valid = (entry.block_flags & BLOCK_FLAG_MIN_MAX_VALID) != 0;
    if !min_max_valid {
        return true;
    }

    match dt {
        DataType::Null
        | DataType::Interval
        | DataType::List(_)
        | DataType::MapUtf8
        | DataType::Events
        | DataType::Links => true,
        DataType::Bool => {
            if entry.min_inline_len < 1 || entry.max_inline_len < 1 {
                return true;
            }
            let min_b = entry.min_inline[0] != 0;
            let max_b = entry.max_inline[0] != 0;
            eval_ord_pred(min_b, max_b, pred, |v| match v {
                ZoneValue::Bool(b) => Some(*b),
                _ => None,
            })
        }
        DataType::I64 | DataType::TimestampNs { .. } => {
            if entry.min_inline_len < 8 || entry.max_inline_len < 8 {
                return true;
            }
            let min_v = read_i64_le(&entry.min_inline);
            let max_v = read_i64_le(&entry.max_inline);
            eval_ord_pred(min_v, max_v, pred, |v| match v {
                ZoneValue::I64(x) | ZoneValue::TimestampNs(x) => Some(*x),
                _ => None,
            })
        }
        DataType::Date => {
            if entry.min_inline_len < 4 || entry.max_inline_len < 4 {
                return true;
            }
            let min_v = read_i32_le(&entry.min_inline);
            let max_v = read_i32_le(&entry.max_inline);
            eval_ord_pred(min_v, max_v, pred, |v| match v {
                ZoneValue::Date(x) => Some(*x),
                _ => None,
            })
        }
        DataType::Decimal128 { .. } => {
            if entry.min_inline_len < 16 || entry.max_inline_len < 16 {
                return true;
            }
            let min_v = i128::from_le_bytes(entry.min_inline);
            let max_v = i128::from_le_bytes(entry.max_inline);
            eval_ord_pred(min_v, max_v, pred, |v| match v {
                ZoneValue::Decimal128(x) => Some(*x),
                _ => None,
            })
        }
        DataType::F64 => {
            if entry.min_inline_len < 8 || entry.max_inline_len < 8 {
                return true;
            }
            let min_f = canonicalize_f64(f64::from_bits(read_u64_le(&entry.min_inline)));
            let max_f = canonicalize_f64(f64::from_bits(read_u64_le(&entry.max_inline)));
            eval_f64_pred(min_f, max_f, has_nan, pred)
        }
        DataType::Utf8 | DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
            let min_b = &entry.min_inline[..usize::from(entry.min_inline_len)];
            let max_b = &entry.max_inline[..usize::from(entry.max_inline_len)];
            eval_bytes_pred(min_b, max_b, pred)
        }
    }
}

fn dict_zone_map_may_match(
    entry: &BlockEntry,
    col_flags: u8,
    pred: &ZonePred,
    dict: Option<&FileDict>,
) -> bool {
    let Some(dict) = dict else {
        return true;
    };
    let has_min_max = (entry.block_flags & BLOCK_FLAG_MIN_MAX_VALID) != 0
        && entry.min_inline_len >= 4
        && entry.max_inline_len >= 4;
    let min_code = u32::from_le_bytes([
        entry.min_inline[0],
        entry.min_inline[1],
        entry.min_inline[2],
        entry.min_inline[3],
    ]);
    let max_code = u32::from_le_bytes([
        entry.max_inline[0],
        entry.max_inline[1],
        entry.max_inline[2],
        entry.max_inline[3],
    ]);

    let dict_sorted = (col_flags & COL_FLAG_DICT_SORTED) != 0 && dict.is_sorted();

    match pred {
        ZonePred::Eq(ZoneValue::Bytes(target)) => {
            let Some(code) = dict.lookup_code(target) else {
                return false;
            };
            if has_min_max && (code < min_code || code > max_code) {
                return false;
            }
            true
        }
        ZonePred::In(vals) => vals.iter().any(|v| match v {
            ZoneValue::Bytes(target) => match dict.lookup_code(target) {
                None => false,
                Some(code) => !has_min_max || (code >= min_code && code <= max_code),
            },
            _ => true,
        }),
        ZonePred::Ne(ZoneValue::Bytes(target)) => {
            if has_min_max && min_code == max_code && dict.lookup_code(target) == Some(min_code) {
                return false;
            }
            true
        }
        ZonePred::Lt(_)
        | ZonePred::Le(_)
        | ZonePred::Gt(_)
        | ZonePred::Ge(_)
        | ZonePred::Between(_, _)
        | ZonePred::Prefix(_) => {
            if !dict_sorted || !has_min_max {
                return true;
            }
            let (Some(min_b), Some(max_b)) =
                (dict.entry_bytes(min_code), dict.entry_bytes(max_code))
            else {
                return true;
            };
            eval_bytes_pred(min_b, max_b, pred)
        }
        _ => true,
    }
}

fn eval_ord_pred<T: Ord + Copy>(
    min_v: T,
    max_v: T,
    pred: &ZonePred,
    extract: impl Fn(&ZoneValue) -> Option<T>,
) -> bool {
    match pred {
        ZonePred::Eq(v) => extract(v).is_none_or(|x| x >= min_v && x <= max_v),
        ZonePred::Ne(v) => extract(v).is_none_or(|x| !(min_v == max_v && min_v == x)),
        ZonePred::Lt(v) => extract(v).is_none_or(|x| min_v < x),
        ZonePred::Le(v) => extract(v).is_none_or(|x| min_v <= x),
        ZonePred::Gt(v) => extract(v).is_none_or(|x| max_v > x),
        ZonePred::Ge(v) => extract(v).is_none_or(|x| max_v >= x),
        ZonePred::Between(lo, hi) => match (extract(lo), extract(hi)) {
            (Some(l), Some(h)) => l <= h && h >= min_v && l <= max_v,
            _ => true,
        },
        ZonePred::In(vs) => vs
            .iter()
            .any(|v| extract(v).is_none_or(|x| x >= min_v && x <= max_v)),
        ZonePred::IsNull | ZonePred::IsNotNull | ZonePred::Prefix(_) => true,
    }
}

fn eval_f64_pred(min_f: f64, max_f: f64, has_nan: bool, pred: &ZonePred) -> bool {
    match pred {
        ZonePred::Eq(ZoneValue::F64(x)) => {
            if x.is_nan() {
                has_nan
            } else {
                let c = canonicalize_f64(*x);
                c >= min_f && c <= max_f
            }
        }
        ZonePred::Ne(ZoneValue::F64(x)) => {
            if has_nan {
                // Spec §3.4.4: NEVER prune `!=` when HAS_NAN == 1.
                true
            } else if x.is_nan() {
                true
            } else {
                let c = canonicalize_f64(*x);
                !(min_f == max_f && min_f == c)
            }
        }
        ZonePred::Lt(ZoneValue::F64(x)) => {
            if x.is_nan() {
                true
            } else {
                let c = canonicalize_f64(*x);
                min_f < c
            }
        }
        ZonePred::Le(ZoneValue::F64(x)) => {
            if x.is_nan() {
                true
            } else {
                let c = canonicalize_f64(*x);
                min_f <= c
            }
        }
        ZonePred::Gt(ZoneValue::F64(x)) => {
            if has_nan || x.is_nan() {
                // Spec §3.4.4: NEVER prune `>` when HAS_NAN == 1 (total order NaN > +inf).
                has_nan
            } else {
                let c = canonicalize_f64(*x);
                max_f > c
            }
        }
        ZonePred::Ge(ZoneValue::F64(x)) => {
            if has_nan || x.is_nan() {
                // Spec §3.4.4: NEVER prune `>=` when HAS_NAN == 1.
                has_nan
            } else {
                let c = canonicalize_f64(*x);
                max_f >= c
            }
        }
        ZonePred::Between(ZoneValue::F64(lo), ZoneValue::F64(hi)) => {
            if lo.is_nan() || hi.is_nan() {
                return true;
            }
            let l = canonicalize_f64(*lo);
            let h = canonicalize_f64(*hi);
            l <= h && h >= min_f && l <= max_f
        }
        ZonePred::In(vs) => vs.iter().any(|v| match v {
            ZoneValue::F64(x) => {
                if x.is_nan() {
                    has_nan
                } else {
                    let c = canonicalize_f64(*x);
                    c >= min_f && c <= max_f
                }
            }
            _ => true,
        }),
        _ => true,
    }
}

fn eval_bytes_pred(min_b: &[u8], max_b: &[u8], pred: &ZonePred) -> bool {
    match pred {
        ZonePred::Eq(ZoneValue::Bytes(t)) => {
            let t = t.as_slice();
            t >= min_b && t <= max_b
        }
        ZonePred::Ne(ZoneValue::Bytes(t)) => {
            let t = t.as_slice();
            !(min_b == max_b && min_b == t)
        }
        ZonePred::Lt(ZoneValue::Bytes(t)) => min_b < t.as_slice(),
        ZonePred::Le(ZoneValue::Bytes(t)) => min_b <= t.as_slice(),
        ZonePred::Gt(ZoneValue::Bytes(t)) => max_b > t.as_slice(),
        ZonePred::Ge(ZoneValue::Bytes(t)) => max_b >= t.as_slice(),
        ZonePred::Between(ZoneValue::Bytes(lo), ZoneValue::Bytes(hi)) => {
            let (l, h) = (lo.as_slice(), hi.as_slice());
            l <= h && h >= min_b && l <= max_b
        }
        ZonePred::In(vs) => vs.iter().any(|v| match v {
            ZoneValue::Bytes(t) => {
                let t = t.as_slice();
                t >= min_b && t <= max_b
            }
            _ => true,
        }),
        ZonePred::Prefix(p) => {
            if p.is_empty() {
                return true;
            }
            if max_b < p.as_slice() {
                return false;
            }
            if let Some(upper) = next_prefix(p)
                && min_b >= upper.as_slice()
            {
                return false;
            }
            true
        }
        _ => true,
    }
}

#[inline]
fn read_i32_le(buf: &[u8; 16]) -> i32 {
    i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
}

#[inline]
fn read_i64_le(buf: &[u8; 16]) -> i64 {
    i64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ])
}

#[inline]
fn read_u64_le(buf: &[u8; 16]) -> u64 {
    u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ])
}
