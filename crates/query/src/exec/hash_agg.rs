//! Spillable partitioned `HashAgg`, bounded-memory `DdSketch`, and `sessions` aggregates.

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::sync::Arc;

use cetra_column::{
    Batch, Bitmap, COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, CancelToken, Column, ColumnError,
    DataType, Field, Interval, MAX_NESTING_DEPTH, Reservation, ScalarValue, Schema, Selection,
};

use crate::error::{InvalidQuery, QueryError};
use crate::eval::{
    self, ElemRef, build_flat_from_elem_refs, compare_decimals, eval_expr, eval_filter,
    f64_canonical_bits, f64_cmp, list_ref_elem, list_ref_len, read_bool, read_date, read_decimal,
    read_f64, read_i64, read_interval, read_scalar, read_str, read_timestamp, reconcile_output,
};
use crate::ir::{EvalContext, Expr};

use super::pipeline::{ControlFlow, ExecContext, PushSink, run_pipeline};
use super::sort_key::SortKey;
use super::source::{PartitionStream, ScanSource, ScanSpec};
use super::spill::{SpillFile, SpillWriter};

const NUM_SPILL_PARTITIONS: usize = 32;
const MAX_REPARTITION_DEPTH: usize = 4;
const DDSKETCH_ALPHA: f64 = 0.005;
const DDSKETCH_MAX_BUCKETS: usize = 2048;
const DDSKETCH_MAGIC_VERSION: u8 = 1;
const DDSKETCH_HEADER_BYTES: usize = 33;
const EMPTY_SLOT: u32 = u32::MAX;
const CANCEL_CHECK_MASK: usize = 1023;
const MIN_SPILL_FRAME_BYTES: usize = 4096;

fn gamma() -> f64 {
    (1.0 + DDSKETCH_ALPHA) / (1.0 - DDSKETCH_ALPHA)
}

fn ln_gamma() -> f64 {
    gamma().ln()
}

/// Bounded-memory logarithmic quantile sketch (`alpha = 0.005`, `<= 0.5%` relative error,
/// `max_buckets = 2048` collapsing lowest-value buckets).
///
/// Handles positive, zero, and negative finite `f64` values and ignores `NaN` / non-finite inputs.
#[derive(Clone, PartialEq)]
pub struct DdSketch {
    pos: Vec<(i32, u64)>,
    neg: Vec<(i32, u64)>,
    zero_count: u64,
    total_count: u64,
    min: f64,
    max: f64,
}

impl fmt::Debug for DdSketch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdSketch")
            .field("count", &self.total_count)
            .field("buckets", &(self.pos.len() + self.neg.len()))
            .finish()
    }
}

impl Default for DdSketch {
    fn default() -> Self {
        Self::new()
    }
}

impl DdSketch {
    /// Creates an empty [`DdSketch`].
    pub fn new() -> Self {
        Self {
            pos: Vec::new(),
            neg: Vec::new(),
            zero_count: 0,
            total_count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Total finite values recorded in this sketch.
    pub fn count(&self) -> u64 {
        self.total_count
    }

    /// Inserts one value, ignoring `NaN` and non-finite floats.
    pub fn add(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        let v = if value == 0.0 { 0.0 } else { value };
        if self.total_count == 0 {
            self.min = v;
            self.max = v;
        } else {
            if v < self.min {
                self.min = v;
            }
            if v > self.max {
                self.max = v;
            }
        }
        self.total_count = self.total_count.saturating_add(1);

        if v == 0.0 {
            self.zero_count = self.zero_count.saturating_add(1);
            return;
        }

        let lg = ln_gamma();
        if v > 0.0 {
            let key = (v.ln() / lg).floor() as i32;
            insert_bucket(&mut self.pos, key, 1);
        } else {
            let key = ((-v).ln() / lg).floor() as i32;
            insert_bucket(&mut self.neg, key, 1);
        }
        self.collapse_if_needed();
    }

    /// Merges `other` into `self`, collapsing lowest-value buckets if total buckets exceed 2048.
    pub fn merge(&mut self, other: &DdSketch) {
        if other.total_count == 0 {
            return;
        }
        if self.total_count == 0 {
            *self = other.clone();
            return;
        }
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }
        self.total_count = self.total_count.saturating_add(other.total_count);
        self.zero_count = self.zero_count.saturating_add(other.zero_count);
        merge_bucket_vecs(&mut self.pos, &other.pos);
        merge_bucket_vecs(&mut self.neg, &other.neg);
        self.collapse_if_needed();
    }

    /// Estimates the quantile `p` in `[0.0, 1.0]`.
    ///
    /// Returns `Ok(None)` when empty and `Err(QueryError::Failed)` when `p` is invalid.
    pub fn quantile(&self, p: f64) -> Result<Option<f64>, QueryError> {
        if !p.is_finite() || !(0.0..=1.0).contains(&p) {
            return Err(QueryError::failed(
                "percentile quantile must be a finite number between 0.0 and 1.0",
            ));
        }
        if self.total_count == 0 {
            return Ok(None);
        }
        if p == 0.0 {
            return Ok(Some(self.min));
        }
        if p == 1.0 {
            return Ok(Some(self.max));
        }

        let target = ((p * (self.total_count - 1) as f64).round() as u64).min(self.total_count - 1);
        let mut seen: u64 = 0;
        let g = gamma();
        let lg = ln_gamma();
        let factor = (2.0 * g) / (g + 1.0);

        for &(k, cnt) in self.neg.iter().rev() {
            seen = seen.saturating_add(cnt);
            if seen > target {
                let mag = (f64::from(k) * lg).exp() * factor;
                let est = if mag.is_finite() { -mag } else { self.min };
                return Ok(Some(est.clamp(self.min, self.max)));
            }
        }

        if self.zero_count > 0 {
            seen = seen.saturating_add(self.zero_count);
            if seen > target {
                return Ok(Some(0.0f64.clamp(self.min, self.max)));
            }
        }

        for &(k, cnt) in &self.pos {
            seen = seen.saturating_add(cnt);
            if seen > target {
                let val = (f64::from(k) * lg).exp() * factor;
                let est = if val.is_finite() { val } else { self.max };
                return Ok(Some(est.clamp(self.min, self.max)));
            }
        }

        Ok(Some(self.max))
    }

    /// Encodes the sketch into a compact little-endian byte buffer for partial-state batches.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(DDSKETCH_HEADER_BYTES + (self.pos.len() + self.neg.len()) * 12);
        out.push(DDSKETCH_MAGIC_VERSION);
        out.extend_from_slice(&self.zero_count.to_le_bytes());
        out.extend_from_slice(&self.min.to_le_bytes());
        out.extend_from_slice(&self.max.to_le_bytes());
        out.extend_from_slice(&(self.pos.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.neg.len() as u32).to_le_bytes());
        for &(k, cnt) in &self.pos {
            out.extend_from_slice(&k.to_le_bytes());
            out.extend_from_slice(&cnt.to_le_bytes());
        }
        for &(k, cnt) in &self.neg {
            out.extend_from_slice(&k.to_le_bytes());
            out.extend_from_slice(&cnt.to_le_bytes());
        }
        out
    }

    /// Decodes a sketch from bytes previously produced by [`DdSketch::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, QueryError> {
        if bytes.len() < DDSKETCH_HEADER_BYTES {
            return Err(QueryError::internal("corrupt DDSketch partial state"));
        }
        if bytes[0] != DDSKETCH_MAGIC_VERSION {
            return Err(QueryError::internal("unsupported DDSketch state version"));
        }
        let zero_count = u64::from_le_bytes(
            bytes[1..9]
                .try_into()
                .map_err(|_| QueryError::internal("corrupt DDSketch header"))?,
        );
        let min = f64::from_le_bytes(
            bytes[9..17]
                .try_into()
                .map_err(|_| QueryError::internal("corrupt DDSketch header"))?,
        );
        let max = f64::from_le_bytes(
            bytes[17..25]
                .try_into()
                .map_err(|_| QueryError::internal("corrupt DDSketch header"))?,
        );
        let pos_len = u32::from_le_bytes(
            bytes[25..29]
                .try_into()
                .map_err(|_| QueryError::internal("corrupt DDSketch header"))?,
        ) as usize;
        let neg_len = u32::from_le_bytes(
            bytes[29..33]
                .try_into()
                .map_err(|_| QueryError::internal("corrupt DDSketch header"))?,
        ) as usize;

        let total_buckets = pos_len
            .checked_add(neg_len)
            .ok_or_else(|| QueryError::internal("DDSketch bucket count overflow"))?;
        if total_buckets > DDSKETCH_MAX_BUCKETS {
            return Err(QueryError::internal(
                "DDSketch bucket count exceeds max_buckets",
            ));
        }
        let expected_len = total_buckets
            .checked_mul(12)
            .and_then(|n| n.checked_add(DDSKETCH_HEADER_BYTES))
            .ok_or_else(|| QueryError::internal("DDSketch byte length overflow"))?;
        if bytes.len() != expected_len {
            return Err(QueryError::internal("corrupt DDSketch payload length"));
        }

        let mut offset = DDSKETCH_HEADER_BYTES;
        let mut total_count = zero_count;
        let pos = decode_buckets(bytes, &mut offset, pos_len, &mut total_count)?;
        let neg = decode_buckets(bytes, &mut offset, neg_len, &mut total_count)?;

        if total_count > 0 && (!min.is_finite() || !max.is_finite() || min > max) {
            return Err(QueryError::internal("corrupt DDSketch min/max bounds"));
        }

        Ok(Self {
            pos,
            neg,
            zero_count,
            total_count,
            min,
            max,
        })
    }

    fn collapse_if_needed(&mut self) {
        while self.pos.len() + self.neg.len() > DDSKETCH_MAX_BUCKETS {
            if self.neg.len() > 1 {
                // Largest key in `neg` corresponds to the lowest (most-negative) value.
                if let Some((_, cnt)) = self.neg.pop()
                    && let Some(last) = self.neg.last_mut()
                {
                    last.1 = last.1.saturating_add(cnt);
                }
            } else if self.pos.len() > 1 {
                let first_cnt = self.pos[0].1;
                self.pos.remove(0);
                self.pos[0].1 = self.pos[0].1.saturating_add(first_cnt);
            } else {
                break;
            }
        }
    }

    fn heap_bytes(&self) -> usize {
        (self.pos.capacity() + self.neg.capacity()) * size_of::<(i32, u64)>()
    }
}

fn decode_buckets(
    bytes: &[u8],
    offset: &mut usize,
    len: usize,
    total_count: &mut u64,
) -> Result<Vec<(i32, u64)>, QueryError> {
    let mut out = Vec::with_capacity(len);
    let mut prev_key: Option<i32> = None;
    for _ in 0..len {
        let key = i32::from_le_bytes(
            bytes[*offset..*offset + 4]
                .try_into()
                .map_err(|_| QueryError::internal("corrupt DDSketch bucket key"))?,
        );
        let cnt = u64::from_le_bytes(
            bytes[*offset + 4..*offset + 12]
                .try_into()
                .map_err(|_| QueryError::internal("corrupt DDSketch bucket count"))?,
        );
        *offset += 12;
        if let Some(pk) = prev_key
            && key <= pk
        {
            return Err(QueryError::internal("unsorted DDSketch buckets"));
        }
        prev_key = Some(key);
        *total_count = total_count.saturating_add(cnt);
        out.push((key, cnt));
    }
    Ok(out)
}

fn insert_bucket(buckets: &mut Vec<(i32, u64)>, key: i32, count: u64) {
    match buckets.binary_search_by_key(&key, |(k, _)| *k) {
        Ok(idx) => {
            buckets[idx].1 = buckets[idx].1.saturating_add(count);
        }
        Err(idx) => {
            buckets.insert(idx, (key, count));
        }
    }
}

fn merge_bucket_vecs(dst: &mut Vec<(i32, u64)>, src: &[(i32, u64)]) {
    if src.is_empty() {
        return;
    }
    if dst.is_empty() {
        dst.extend_from_slice(src);
        return;
    }
    let mut merged = Vec::with_capacity((dst.len() + src.len()).min(DDSKETCH_MAX_BUCKETS + 4));
    let mut i = 0;
    let mut j = 0;
    while i < dst.len() && j < src.len() {
        match dst[i].0.cmp(&src[j].0) {
            Ordering::Less => {
                merged.push(dst[i]);
                i += 1;
            }
            Ordering::Greater => {
                merged.push(src[j]);
                j += 1;
            }
            Ordering::Equal => {
                merged.push((dst[i].0, dst[i].1.saturating_add(src[j].1)));
                i += 1;
                j += 1;
            }
        }
    }
    merged.extend_from_slice(&dst[i..]);
    merged.extend_from_slice(&src[j..]);
    *dst = merged;
}

/// Supported aggregate functions in [`HashAgg`].
#[derive(Debug, Clone, PartialEq)]
pub enum AggFunc {
    /// `COUNT(*)` (`I64`, non-null, 0 on empty).
    CountStar,
    /// `COUNT(expr)` (`I64`, non-null, 0 on empty).
    Count(Expr),
    /// `COUNT(DISTINCT expr)` (`I64`, non-null, 0 on empty; spillable partial state).
    CountDistinct(Expr),
    /// `SUM(expr)` (`I64` checked overflow -> `Failed`, `F64`, or `Decimal128(38, s)`; `NULL` on empty).
    Sum(Expr),
    /// Deterministic `Decimal128(38, 18)` cost sum (`D-A2-4`); rejects `NaN`/`±inf`/out-of-range `F64`.
    SumCostDecimal(Expr),
    /// `AVG(expr)` (`F64`, or `Decimal128` on `Decimal128` input; `NULL` on empty).
    Avg(Expr),
    /// `MIN(expr)` (total order with `f64_cmp`; `NULL` on empty).
    Min(Expr),
    /// `MAX(expr)` (total order with `f64_cmp`; `NULL` on empty).
    Max(Expr),
    /// `BOOL_OR(expr)` (`Bool`; `NULL` on empty).
    BoolOr(Expr),
    /// `BOOL_AND(expr)` (`Bool`; `NULL` on empty).
    BoolAnd(Expr),
    /// `approx_percentile_cont(expr, quantile)` backed by [`DdSketch`] (`F64`; `NULL` on empty).
    ApproxPercentileCont {
        /// Numeric input expression.
        expr: Expr,
        /// Target quantile in `[0.0, 1.0]`.
        quantile: f64,
    },
    /// Exact `percentile_cont(quantile) WITHIN GROUP (ORDER BY expr)` (`F64`, unspillable `Reservation`; `NULL` on empty).
    PercentileCont {
        /// Numeric input expression.
        expr: Expr,
        /// Target quantile in `[0.0, 1.0]`.
        quantile: f64,
    },
    /// `ARRAY_AGG([DISTINCT] expr)` (`List(input_type)`, unspillable `Reservation`; `NULL` on empty).
    ArrayAgg {
        /// Element expression.
        expr: Expr,
        /// True for `ARRAY_AGG(DISTINCT ...)`.
        distinct: bool,
    },
    /// Dedicated `sessions` internal aggregate: first non-null `value` ordered by `order_by`.
    FirstNonNullByOrder {
        /// Value expression (e.g. `user_id`).
        value: Expr,
        /// Sort keys (e.g. `(start_time ASC, trace_id ASC)`).
        order_by: Vec<SortKey>,
    },
    /// First `value` ordered by `order_by`, preserving `NULL` when the winning row's `value` is `NULL`.
    FirstByOrder {
        /// Value expression.
        value: Expr,
        /// Sort keys.
        order_by: Vec<SortKey>,
    },
    /// Dedicated `sessions` internal aggregate: sorted distinct union of `List(Utf8)` (empty `[]` on empty).
    TagUnion(Expr),
}

/// One aggregate output column in [`HashAggSpec`].
#[derive(Debug, Clone, PartialEq)]
pub struct AggExpr {
    /// Aggregate function.
    pub func: AggFunc,
    /// Optional per-row `FILTER (WHERE ...)` predicate.
    pub filter: Option<Expr>,
    /// Output field name.
    pub output_name: String,
}

/// Specification for [`HashAgg`].
#[derive(Debug, Clone, PartialEq)]
pub struct HashAggSpec {
    /// Grouping expressions `(output_name, expr)` (empty for global aggregation).
    pub group_by: Vec<(String, Expr)>,
    /// Aggregate output expressions.
    pub aggs: Vec<AggExpr>,
}

// ---------------------------------------------------------------------------
// Canonical hashable / comparable key atom
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum KeyAtom {
    Null,
    Bool(bool),
    I64(i64),
    F64(u64),
    Decimal128(i128),
    TimestampNs(i64),
    Date(i32),
    Interval(Interval),
    Utf8(String),
    Binary(Vec<u8>),
    FixedBytes8([u8; 8]),
    FixedBytes16([u8; 16]),
    Scalar(ScalarValue),
}

impl PartialEq for KeyAtom {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::I64(a), Self::I64(b)) => a == b,
            (Self::F64(a), Self::F64(b)) => a == b,
            (Self::Decimal128(a), Self::Decimal128(b)) => a == b,
            (Self::TimestampNs(a), Self::TimestampNs(b)) => a == b,
            (Self::Date(a), Self::Date(b)) => a == b,
            (Self::Interval(a), Self::Interval(b)) => a == b,
            (Self::Utf8(a), Self::Utf8(b)) => a == b,
            (Self::Binary(a), Self::Binary(b)) => a == b,
            (Self::FixedBytes8(a), Self::FixedBytes8(b)) => a == b,
            (Self::FixedBytes16(a), Self::FixedBytes16(b)) => a == b,
            (Self::Scalar(a), Self::Scalar(b)) => a.semantic_eq(b),
            _ => false,
        }
    }
}

impl Eq for KeyAtom {}

impl Hash for KeyAtom {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Null => {}
            Self::Bool(b) => b.hash(state),
            Self::I64(v) | Self::TimestampNs(v) => v.hash(state),
            Self::F64(bits) => bits.hash(state),
            Self::Decimal128(v) => v.hash(state),
            Self::Date(d) => d.hash(state),
            Self::Interval(iv) => iv.hash(state),
            Self::Utf8(s) => s.hash(state),
            Self::Binary(b) => b.hash(state),
            Self::FixedBytes8(b) => b.hash(state),
            Self::FixedBytes16(b) => b.hash(state),
            Self::Scalar(sv) => hash_scalar_value(sv, state),
        }
    }
}

impl KeyAtom {
    fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::Utf8(s) => s.capacity(),
            Self::Binary(b) => b.capacity(),
            Self::Scalar(sv) => sv.heap_bytes(),
            _ => 0,
        }
    }

    fn to_scalar(&self, dt: &DataType) -> ScalarValue {
        match self {
            Self::Null => ScalarValue::Null(dt.clone()),
            Self::Bool(b) => ScalarValue::Bool(*b),
            Self::I64(v) => ScalarValue::I64(*v),
            Self::F64(bits) => ScalarValue::F64(f64::from_bits(*bits)),
            Self::Decimal128(v) => match dt {
                DataType::Decimal128 { precision, scale } => ScalarValue::Decimal128 {
                    value: *v,
                    precision: *precision,
                    scale: *scale,
                },
                _ => ScalarValue::Null(dt.clone()),
            },
            Self::TimestampNs(v) => match dt {
                DataType::TimestampNs { utc } => ScalarValue::TimestampNs {
                    value: *v,
                    utc: *utc,
                },
                _ => ScalarValue::Null(dt.clone()),
            },
            Self::Date(d) => ScalarValue::Date(*d),
            Self::Interval(iv) => ScalarValue::Interval(*iv),
            Self::Utf8(s) => ScalarValue::Utf8(s.clone()),
            Self::Binary(b) => ScalarValue::Binary(b.clone()),
            Self::FixedBytes8(b) => ScalarValue::FixedBytes8(*b),
            Self::FixedBytes16(b) => ScalarValue::FixedBytes16(*b),
            Self::Scalar(sv) => sv.clone(),
        }
    }
}

fn hash_scalar_value<H: Hasher>(sv: &ScalarValue, state: &mut H) {
    std::mem::discriminant(sv).hash(state);
    match sv {
        ScalarValue::Null(_) => {}
        ScalarValue::Bool(b) => b.hash(state),
        ScalarValue::I64(v) => v.hash(state),
        ScalarValue::F64(v) => f64_canonical_bits(*v).hash(state),
        ScalarValue::Decimal128 { value, scale, .. } => {
            value.hash(state);
            scale.hash(state);
        }
        ScalarValue::TimestampNs { value, utc } => {
            value.hash(state);
            utc.hash(state);
        }
        ScalarValue::Date(d) => d.hash(state),
        ScalarValue::Interval(iv) => iv.hash(state),
        ScalarValue::Utf8(s) => s.hash(state),
        ScalarValue::Binary(b) => b.hash(state),
        ScalarValue::FixedBytes8(b) => b.hash(state),
        ScalarValue::FixedBytes16(b) => b.hash(state),
        ScalarValue::List { values, .. } => {
            values.len().hash(state);
            for v in values {
                hash_scalar_value(v, state);
            }
        }
        ScalarValue::MapUtf8(entries) => {
            entries.len().hash(state);
            for (k, v) in entries {
                k.hash(state);
                v.hash(state);
            }
        }
        ScalarValue::Events(evs) => {
            evs.len().hash(state);
            for ev in evs {
                ev.time.hash(state);
                ev.name.hash(state);
                ev.attributes.hash(state);
            }
        }
        ScalarValue::Links(lks) => {
            lks.len().hash(state);
            for lk in lks {
                lk.trace_id.hash(state);
                lk.span_id.hash(state);
                lk.attributes.hash(state);
            }
        }
    }
}

fn read_key_atom(col: &Column, row: usize) -> KeyAtom {
    if col.is_null(row) {
        return KeyAtom::Null;
    }
    match col.data_type() {
        DataType::Null => KeyAtom::Null,
        DataType::Bool => read_bool(col, row).map_or(KeyAtom::Null, KeyAtom::Bool),
        DataType::I64 => read_i64(col, row).map_or(KeyAtom::Null, KeyAtom::I64),
        DataType::F64 => {
            read_f64(col, row).map_or(KeyAtom::Null, |v| KeyAtom::F64(f64_canonical_bits(v)))
        }
        DataType::Decimal128 { .. } => {
            read_decimal(col, row).map_or(KeyAtom::Null, KeyAtom::Decimal128)
        }
        DataType::TimestampNs { .. } => {
            read_timestamp(col, row).map_or(KeyAtom::Null, KeyAtom::TimestampNs)
        }
        DataType::Date => read_date(col, row).map_or(KeyAtom::Null, KeyAtom::Date),
        DataType::Interval => read_interval(col, row).map_or(KeyAtom::Null, KeyAtom::Interval),
        DataType::Utf8 => read_str(col, row).map_or(KeyAtom::Null, |s| KeyAtom::Utf8(s.to_owned())),
        DataType::Binary => {
            eval::read_bytes(col, row).map_or(KeyAtom::Null, |b| KeyAtom::Binary(b.to_vec()))
        }
        DataType::FixedBytes8 => eval::read_bytes(col, row)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .map_or(KeyAtom::Null, KeyAtom::FixedBytes8),
        DataType::FixedBytes16 => eval::read_bytes(col, row)
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .map_or(KeyAtom::Null, KeyAtom::FixedBytes16),
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links => {
            let sv = read_scalar(col, row);
            if sv.is_null() {
                KeyAtom::Null
            } else {
                KeyAtom::Scalar(sv)
            }
        }
    }
}

fn col_row_eq_atom(col: &Column, row: usize, atom: &KeyAtom) -> bool {
    if col.is_null(row) {
        return atom.is_null();
    }
    match (col.data_type(), atom) {
        (DataType::Null, KeyAtom::Null) => true,
        (DataType::Bool, KeyAtom::Bool(b)) => read_bool(col, row) == Some(*b),
        (DataType::I64, KeyAtom::I64(v)) => read_i64(col, row) == Some(*v),
        (DataType::F64, KeyAtom::F64(bits)) => {
            read_f64(col, row).is_some_and(|v| f64_canonical_bits(v) == *bits)
        }
        (DataType::Decimal128 { .. }, KeyAtom::Decimal128(v)) => read_decimal(col, row) == Some(*v),
        (DataType::TimestampNs { .. }, KeyAtom::TimestampNs(v)) => {
            read_timestamp(col, row) == Some(*v)
        }
        (DataType::Date, KeyAtom::Date(d)) => read_date(col, row) == Some(*d),
        (DataType::Interval, KeyAtom::Interval(iv)) => read_interval(col, row) == Some(*iv),
        (DataType::Utf8, KeyAtom::Utf8(s)) => read_str(col, row) == Some(s.as_str()),
        (DataType::Binary, KeyAtom::Binary(b)) => eval::read_bytes(col, row) == Some(b.as_slice()),
        (DataType::FixedBytes8, KeyAtom::FixedBytes8(b)) => {
            eval::read_bytes(col, row) == Some(b.as_slice())
        }
        (DataType::FixedBytes16, KeyAtom::FixedBytes16(b)) => {
            eval::read_bytes(col, row) == Some(b.as_slice())
        }
        (
            DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links,
            KeyAtom::Scalar(sv),
        ) => read_scalar(col, row).semantic_eq(sv),
        _ => false,
    }
}

fn hash_col_row<H: Hasher>(col: &Column, row: usize, state: &mut H) {
    if col.is_null(row) {
        0u8.hash(state);
        return;
    }
    1u8.hash(state);
    match col.data_type() {
        DataType::Null => {}
        DataType::Bool => {
            if let Some(b) = read_bool(col, row) {
                b.hash(state);
            }
        }
        DataType::I64 => {
            if let Some(v) = read_i64(col, row) {
                v.hash(state);
            }
        }
        DataType::F64 => {
            if let Some(v) = read_f64(col, row) {
                f64_canonical_bits(v).hash(state);
            }
        }
        DataType::Decimal128 { .. } => {
            if let Some(v) = read_decimal(col, row) {
                v.hash(state);
            }
        }
        DataType::TimestampNs { .. } => {
            if let Some(v) = read_timestamp(col, row) {
                v.hash(state);
            }
        }
        DataType::Date => {
            if let Some(d) = read_date(col, row) {
                d.hash(state);
            }
        }
        DataType::Interval => {
            if let Some(iv) = read_interval(col, row) {
                iv.hash(state);
            }
        }
        DataType::Utf8 => {
            if let Some(s) = read_str(col, row) {
                s.hash(state);
            }
        }
        DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
            if let Some(b) = eval::read_bytes(col, row) {
                b.hash(state);
            }
        }
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links => {
            let sv = read_scalar(col, row);
            hash_scalar_value(&sv, state);
        }
    }
}

fn mix64(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    x
}

fn hash_cols_row(cols: &[&Column], row: usize, salt: u64) -> u64 {
    let mut h = DefaultHasher::new();
    salt.hash(&mut h);
    for col in cols {
        hash_col_row(col, row, &mut h);
    }
    mix64(h.finish() ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn hash_atoms(atoms: &[KeyAtom], salt: u64) -> u64 {
    let mut h = DefaultHasher::new();
    salt.hash(&mut h);
    for atom in atoms {
        if atom.is_null() {
            0u8.hash(&mut h);
        } else {
            1u8.hash(&mut h);
            match atom {
                KeyAtom::Null => {}
                KeyAtom::Bool(b) => b.hash(&mut h),
                KeyAtom::I64(v) | KeyAtom::TimestampNs(v) => v.hash(&mut h),
                KeyAtom::F64(bits) => bits.hash(&mut h),
                KeyAtom::Decimal128(v) => v.hash(&mut h),
                KeyAtom::Date(d) => d.hash(&mut h),
                KeyAtom::Interval(iv) => iv.hash(&mut h),
                KeyAtom::Utf8(s) => s.hash(&mut h),
                KeyAtom::Binary(b) => b.hash(&mut h),
                KeyAtom::FixedBytes8(b) => b.as_slice().hash(&mut h),
                KeyAtom::FixedBytes16(b) => b.as_slice().hash(&mut h),
                KeyAtom::Scalar(sv) => hash_scalar_value(sv, &mut h),
            }
        }
    }
    mix64(h.finish() ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn compare_non_null_atoms(a: &KeyAtom, b: &KeyAtom, dt: &DataType) -> Result<Ordering, QueryError> {
    match (a, b) {
        (KeyAtom::Bool(x), KeyAtom::Bool(y)) => Ok(x.cmp(y)),
        (KeyAtom::I64(x), KeyAtom::I64(y)) | (KeyAtom::TimestampNs(x), KeyAtom::TimestampNs(y)) => {
            Ok(x.cmp(y))
        }
        (KeyAtom::F64(bx), KeyAtom::F64(by)) => {
            Ok(f64_cmp(f64::from_bits(*bx), f64::from_bits(*by)))
        }
        (KeyAtom::Decimal128(x), KeyAtom::Decimal128(y)) => {
            let DataType::Decimal128 { scale, .. } = dt else {
                return Ok(x.cmp(y));
            };
            compare_decimals(*x, *scale, *y, *scale)
        }
        (KeyAtom::Date(x), KeyAtom::Date(y)) => Ok(x.cmp(y)),
        (KeyAtom::Interval(x), KeyAtom::Interval(y)) => {
            Ok((x.months, x.days, x.nanos).cmp(&(y.months, y.days, y.nanos)))
        }
        (KeyAtom::Utf8(x), KeyAtom::Utf8(y)) => Ok(x.cmp(y)),
        (KeyAtom::Binary(x), KeyAtom::Binary(y)) => Ok(x.cmp(y)),
        (KeyAtom::FixedBytes8(x), KeyAtom::FixedBytes8(y)) => Ok(x.cmp(y)),
        (KeyAtom::FixedBytes16(x), KeyAtom::FixedBytes16(y)) => Ok(x.cmp(y)),
        (KeyAtom::Scalar(x), KeyAtom::Scalar(y)) => compare_scalars_for_agg(x, y),
        _ => Err(QueryError::internal("mismatched key atom types")),
    }
}

fn compare_scalars_for_agg(a: &ScalarValue, b: &ScalarValue) -> Result<Ordering, QueryError> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Less),
        (false, true) => return Ok(Ordering::Greater),
        (false, false) => {}
    }
    match (a, b) {
        (ScalarValue::Bool(x), ScalarValue::Bool(y)) => Ok(x.cmp(y)),
        (ScalarValue::I64(x), ScalarValue::I64(y)) => Ok(x.cmp(y)),
        (ScalarValue::F64(x), ScalarValue::F64(y)) => Ok(f64_cmp(*x, *y)),
        (
            ScalarValue::Decimal128 {
                value: x,
                scale: s1,
                ..
            },
            ScalarValue::Decimal128 {
                value: y,
                scale: s2,
                ..
            },
        ) => compare_decimals(*x, *s1, *y, *s2),
        (ScalarValue::TimestampNs { value: x, .. }, ScalarValue::TimestampNs { value: y, .. }) => {
            Ok(x.cmp(y))
        }
        (ScalarValue::Date(x), ScalarValue::Date(y)) => Ok(x.cmp(y)),
        (ScalarValue::Interval(x), ScalarValue::Interval(y)) => {
            Ok((x.months, x.days, x.nanos).cmp(&(y.months, y.days, y.nanos)))
        }
        (ScalarValue::Utf8(x), ScalarValue::Utf8(y)) => Ok(x.cmp(y)),
        (ScalarValue::Binary(x), ScalarValue::Binary(y)) => Ok(x.cmp(y)),
        (ScalarValue::FixedBytes8(x), ScalarValue::FixedBytes8(y)) => Ok(x.cmp(y)),
        (ScalarValue::FixedBytes16(x), ScalarValue::FixedBytes16(y)) => Ok(x.cmp(y)),
        (ScalarValue::List { values: va, .. }, ScalarValue::List { values: vb, .. }) => {
            for (ea, eb) in va.iter().zip(vb.iter()) {
                let ord = compare_scalars_for_agg(ea, eb)?;
                if ord != Ordering::Equal {
                    return Ok(ord);
                }
            }
            Ok(va.len().cmp(&vb.len()))
        }
        _ => Ok(Ordering::Equal),
    }
}

fn compare_order_keys(
    incoming_cols: &[&Column],
    row: usize,
    existing: &[KeyAtom],
    keys: &[SortKey],
) -> Result<Ordering, QueryError> {
    for (idx, key) in keys.iter().enumerate() {
        let Some(col) = incoming_cols.get(idx) else {
            continue;
        };
        let Some(ex) = existing.get(idx) else {
            continue;
        };
        let null_in = col.is_null(row);
        let null_ex = ex.is_null();
        match (null_in, null_ex) {
            (true, true) => continue,
            (true, false) => {
                return Ok(if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                });
            }
            (false, true) => {
                return Ok(if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                });
            }
            (false, false) => {}
        }
        let in_atom = read_key_atom(col, row);
        let dt = key.expr.data_type();
        let raw = compare_non_null_atoms(&in_atom, ex, &dt)?;
        let ord = if key.descending { raw.reverse() } else { raw };
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

// ---------------------------------------------------------------------------
// Decimal helpers (matching `eval.rs`)
// ---------------------------------------------------------------------------

fn check_decimal_bounds(val: i128, precision: u8) -> Result<i128, QueryError> {
    if precision == 0 || precision > 38 {
        return Err(QueryError::failed("decimal precision out of range"));
    }
    let limit = 10u128.pow(u32::from(precision));
    if val.unsigned_abs() >= limit {
        return Err(QueryError::failed("decimal overflow"));
    }
    Ok(val)
}

fn f64_to_decimal(v: f64, precision: u8, scale: i8) -> Result<i128, QueryError> {
    if !v.is_finite() {
        return Err(QueryError::failed(
            "cannot convert non-finite float to decimal",
        ));
    }
    let scaled = (v * 10f64.powi(i32::from(scale))).round();
    if !scaled.is_finite() || scaled.abs() >= 10f64.powi(i32::from(precision)) {
        return Err(QueryError::failed("float out of range for decimal"));
    }
    if scaled <= (i128::MIN as f64) || scaled >= (i128::MAX as f64) {
        return Err(QueryError::failed("decimal overflow"));
    }
    check_decimal_bounds(scaled as i128, precision)
}

fn i64_to_decimal(v: i64, precision: u8, scale: i8) -> Result<i128, QueryError> {
    let scaled = if scale >= 0 {
        let factor = 10i128
            .checked_pow(scale as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        i128::from(v)
            .checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    } else {
        let denom = 10i128
            .checked_pow(u32::from(scale.unsigned_abs()))
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        i128::from(v) / denom
    };
    check_decimal_bounds(scaled, precision)
}

fn rescale_decimal(v: i128, s1: i8, p2: u8, s2: i8) -> Result<i128, QueryError> {
    let res = if s2 >= s1 {
        let diff = (s2 as i32 - s1 as i32) as u32;
        let factor = 10i128
            .checked_pow(diff)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        v.checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    } else {
        let diff = (s1 as i32 - s2 as i32) as u32;
        let denom = 10u128
            .checked_pow(diff)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        let neg = v < 0;
        let abs = v.unsigned_abs();
        let mut q = abs / denom;
        let r = abs % denom;
        if r >= denom.div_ceil(2) {
            q = q
                .checked_add(1)
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
        }
        let mag = i128::try_from(q).map_err(|_| QueryError::failed("decimal overflow"))?;
        if neg {
            mag.checked_neg()
                .ok_or_else(|| QueryError::failed("decimal overflow"))?
        } else {
            mag
        }
    };
    check_decimal_bounds(res, p2)
}

fn decimal_avg_finalize(
    sum: i128,
    in_scale: i8,
    out_precision: u8,
    out_scale: i8,
    count: i64,
) -> Result<i128, QueryError> {
    if count <= 0 {
        return Err(QueryError::internal("invalid decimal avg count"));
    }
    let diff = (i32::from(out_scale) - i32::from(in_scale)) as u32;
    let factor = 10u128
        .checked_pow(diff)
        .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
    let neg = sum < 0;
    let (hi, lo) = u256_mul(sum.unsigned_abs(), factor);
    let denom = count as u128;
    let (mut q, rem) =
        u256_div_rem(hi, lo, denom).ok_or_else(|| QueryError::failed("decimal avg overflow"))?;
    if rem >= denom.div_ceil(2) {
        q = q
            .checked_add(1)
            .ok_or_else(|| QueryError::failed("decimal avg overflow"))?;
    }
    let mag = i128::try_from(q).map_err(|_| QueryError::failed("decimal avg overflow"))?;
    let signed = if neg {
        mag.checked_neg()
            .ok_or_else(|| QueryError::failed("decimal avg overflow"))?
    } else {
        mag
    };
    check_decimal_bounds(signed, out_precision)
}

fn u256_mul(a: u128, b: u128) -> (u128, u128) {
    let a_lo = a & 0xffff_ffff_ffff_ffff;
    let a_hi = a >> 64;
    let b_lo = b & 0xffff_ffff_ffff_ffff;
    let b_hi = b >> 64;

    let p0 = a_lo * b_lo;
    let p1 = a_lo * b_hi;
    let p2 = a_hi * b_lo;
    let p3 = a_hi * b_hi;

    let mid1 = (p0 >> 64) + (p1 & 0xffff_ffff_ffff_ffff) + (p2 & 0xffff_ffff_ffff_ffff);
    let lo = (p0 & 0xffff_ffff_ffff_ffff) | (mid1 << 64);
    let hi = p3 + (p1 >> 64) + (p2 >> 64) + (mid1 >> 64);
    (hi, lo)
}

fn u256_div_rem(hi: u128, lo: u128, d: u128) -> Option<(u128, u128)> {
    if d == 0 || hi >= d {
        return None;
    }
    let mut q = 0u128;
    let mut r = hi;
    for bit in (0..128).rev() {
        let r_high = r >> 127;
        r = (r << 1) | ((lo >> bit) & 1);
        if r_high != 0 || r >= d {
            r = r.wrapping_sub(d);
            q |= 1u128 << bit;
        }
    }
    Some((q, r))
}

// ---------------------------------------------------------------------------
// Accumulator specifications & states
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum AccKind {
    CountStar,
    Count,
    CountDistinct(DataType),
    SumI64,
    SumF64,
    SumDecimal {
        scale: i8,
    },
    SumCostDecimal(DataType),
    AvgFloat(DataType),
    AvgDecimal {
        in_scale: i8,
        out_precision: u8,
        out_scale: i8,
    },
    Min(DataType),
    Max(DataType),
    BoolOr,
    BoolAnd,
    ApproxPercentile {
        input_type: DataType,
        quantile: f64,
    },
    Percentile {
        input_type: DataType,
        quantile: f64,
    },
    ArrayAgg {
        input_type: DataType,
        distinct: bool,
    },
    FirstNonNullByOrder {
        value_type: DataType,
        order_by: Vec<SortKey>,
    },
    FirstByOrder {
        value_type: DataType,
        order_by: Vec<SortKey>,
    },
    TagUnion,
}

impl AccKind {
    fn output_field(&self, name: &str) -> Field {
        match self {
            Self::CountStar | Self::Count | Self::CountDistinct(_) => {
                Field::new(name, DataType::I64, false)
            }
            Self::SumI64 => Field::new(name, DataType::I64, true),
            Self::SumF64 => Field::new(name, DataType::F64, true),
            Self::SumDecimal { scale } => Field::new(
                name,
                DataType::Decimal128 {
                    precision: 38,
                    scale: *scale,
                },
                true,
            ),
            Self::SumCostDecimal(_) => Field::new(name, DataType::cost_decimal(), true),
            Self::AvgFloat(_) => Field::new(name, DataType::F64, true),
            Self::AvgDecimal {
                out_precision,
                out_scale,
                ..
            } => Field::new(
                name,
                DataType::Decimal128 {
                    precision: *out_precision,
                    scale: *out_scale,
                },
                true,
            ),
            Self::Min(dt) | Self::Max(dt) => Field::new(name, dt.clone(), true),
            Self::BoolOr | Self::BoolAnd => Field::new(name, DataType::Bool, true),
            Self::ApproxPercentile { .. } | Self::Percentile { .. } => {
                Field::new(name, DataType::F64, true)
            }
            Self::ArrayAgg { input_type, .. } => {
                Field::new(name, DataType::List(Box::new(input_type.clone())), true)
            }
            Self::FirstNonNullByOrder { value_type, .. }
            | Self::FirstByOrder { value_type, .. } => Field::new(name, value_type.clone(), true),
            Self::TagUnion => Field::new(name, DataType::List(Box::new(DataType::Utf8)), false),
        }
    }

    fn partial_fields(&self, agg_idx: usize) -> Vec<Field> {
        match self {
            Self::CountStar | Self::Count => {
                vec![Field::new(format!("a_{agg_idx}_cnt"), DataType::I64, false)]
            }
            Self::CountDistinct(dt) => vec![Field::new(
                format!("a_{agg_idx}_cd"),
                DataType::List(Box::new(dt.clone())),
                false,
            )],
            Self::SumI64 => vec![Field::new(format!("a_{agg_idx}_sum"), DataType::I64, true)],
            Self::SumF64 => vec![Field::new(format!("a_{agg_idx}_sum"), DataType::F64, true)],
            Self::SumDecimal { scale } => vec![Field::new(
                format!("a_{agg_idx}_sum"),
                DataType::Decimal128 {
                    precision: 38,
                    scale: *scale,
                },
                true,
            )],
            Self::SumCostDecimal(_) => vec![Field::new(
                format!("a_{agg_idx}_cost"),
                DataType::cost_decimal(),
                true,
            )],
            Self::AvgFloat(_) => vec![
                Field::new(format!("a_{agg_idx}_sum"), DataType::F64, true),
                Field::new(format!("a_{agg_idx}_cnt"), DataType::I64, false),
            ],
            Self::AvgDecimal { in_scale, .. } => vec![
                Field::new(
                    format!("a_{agg_idx}_sum"),
                    DataType::Decimal128 {
                        precision: 38,
                        scale: *in_scale,
                    },
                    true,
                ),
                Field::new(format!("a_{agg_idx}_cnt"), DataType::I64, false),
            ],
            Self::Min(dt) | Self::Max(dt) => {
                vec![Field::new(format!("a_{agg_idx}_mm"), dt.clone(), true)]
            }
            Self::BoolOr | Self::BoolAnd => {
                vec![Field::new(format!("a_{agg_idx}_b"), DataType::Bool, true)]
            }
            Self::ApproxPercentile { .. } => {
                vec![Field::new(
                    format!("a_{agg_idx}_dd"),
                    DataType::Binary,
                    false,
                )]
            }
            Self::Percentile { .. } => vec![Field::new(
                format!("a_{agg_idx}_pct"),
                DataType::List(Box::new(DataType::F64)),
                false,
            )],
            Self::ArrayAgg { input_type, .. } => vec![Field::new(
                format!("a_{agg_idx}_arr"),
                DataType::List(Box::new(input_type.clone())),
                true,
            )],
            Self::FirstNonNullByOrder {
                value_type,
                order_by,
            } => {
                let mut fields = Vec::with_capacity(1 + order_by.len());
                fields.push(Field::new(
                    format!("a_{agg_idx}_val"),
                    value_type.clone(),
                    true,
                ));
                for (k_idx, key) in order_by.iter().enumerate() {
                    fields.push(Field::new(
                        format!("a_{agg_idx}_k_{k_idx}"),
                        key.expr.data_type(),
                        true,
                    ));
                }
                fields
            }
            Self::FirstByOrder {
                value_type,
                order_by,
            } => {
                let mut fields = Vec::with_capacity(2 + order_by.len());
                fields.push(Field::new(
                    format!("a_{agg_idx}_has"),
                    DataType::Bool,
                    false,
                ));
                fields.push(Field::new(
                    format!("a_{agg_idx}_val"),
                    value_type.clone(),
                    true,
                ));
                for (k_idx, key) in order_by.iter().enumerate() {
                    fields.push(Field::new(
                        format!("a_{agg_idx}_k_{k_idx}"),
                        key.expr.data_type(),
                        true,
                    ));
                }
                fields
            }
            Self::TagUnion => vec![Field::new(
                format!("a_{agg_idx}_tags"),
                DataType::List(Box::new(DataType::Utf8)),
                false,
            )],
        }
    }

    fn partial_col_count(&self) -> usize {
        match self {
            Self::AvgFloat(_) | Self::AvgDecimal { .. } => 2,
            Self::FirstNonNullByOrder { order_by, .. } => 1 + order_by.len(),
            Self::FirstByOrder { order_by, .. } => 2 + order_by.len(),
            _ => 1,
        }
    }

    fn make_state(&self) -> AccState {
        match self {
            Self::CountStar | Self::Count => AccState::Count(0),
            Self::CountDistinct(_) => AccState::CountDistinct {
                set: HashSet::new(),
                heap_bytes: 0,
            },
            Self::SumI64 => AccState::SumI64(None),
            Self::SumF64 => AccState::SumF64(None),
            Self::SumDecimal { .. } => AccState::SumDecimal(None),
            Self::SumCostDecimal(_) => AccState::SumCostDecimal(None),
            Self::AvgFloat(_) => AccState::AvgFloat { sum: 0.0, count: 0 },
            Self::AvgDecimal { .. } => AccState::AvgDecimal { sum: 0, count: 0 },
            Self::Min(_) => AccState::Min(None),
            Self::Max(_) => AccState::Max(None),
            Self::BoolOr => AccState::BoolOr(None),
            Self::BoolAnd => AccState::BoolAnd(None),
            Self::ApproxPercentile { .. } => AccState::ApproxPercentile(DdSketch::new()),
            Self::Percentile { .. } => AccState::Percentile(Vec::new()),
            Self::ArrayAgg { distinct, .. } => AccState::ArrayAgg {
                seen_any: false,
                values: Vec::new(),
                set: distinct.then(HashSet::new),
                heap_bytes: 0,
            },
            Self::FirstNonNullByOrder { .. } => AccState::FirstNonNullByOrder {
                best: None,
                heap_bytes: 0,
            },
            Self::FirstByOrder { .. } => AccState::FirstByOrder {
                best: None,
                heap_bytes: 0,
            },
            Self::TagUnion => AccState::TagUnion {
                tags: BTreeSet::new(),
                heap_bytes: 0,
            },
        }
    }
}

#[derive(Clone)]
enum AccState {
    Count(i64),
    CountDistinct {
        set: HashSet<KeyAtom>,
        heap_bytes: usize,
    },
    SumI64(Option<i64>),
    SumF64(Option<f64>),
    SumDecimal(Option<i128>),
    SumCostDecimal(Option<i128>),
    AvgFloat {
        sum: f64,
        count: i64,
    },
    AvgDecimal {
        sum: i128,
        count: i64,
    },
    Min(Option<KeyAtom>),
    Max(Option<KeyAtom>),
    BoolOr(Option<bool>),
    BoolAnd(Option<bool>),
    ApproxPercentile(DdSketch),
    Percentile(Vec<f64>),
    ArrayAgg {
        seen_any: bool,
        values: Vec<KeyAtom>,
        set: Option<HashSet<KeyAtom>>,
        heap_bytes: usize,
    },
    FirstNonNullByOrder {
        best: Option<(KeyAtom, Vec<KeyAtom>)>,
        heap_bytes: usize,
    },
    FirstByOrder {
        best: Option<(KeyAtom, Vec<KeyAtom>)>,
        heap_bytes: usize,
    },
    TagUnion {
        tags: BTreeSet<String>,
        heap_bytes: usize,
    },
}

impl AccState {
    fn spillable_heap_bytes(&self) -> usize {
        match self {
            Self::CountDistinct { set, heap_bytes } => {
                set.capacity() * (size_of::<KeyAtom>() + 8) + *heap_bytes
            }
            Self::Min(Some(k)) | Self::Max(Some(k)) => k.heap_bytes(),
            Self::ApproxPercentile(sk) => sk.heap_bytes(),
            Self::FirstNonNullByOrder { best, heap_bytes }
            | Self::FirstByOrder { best, heap_bytes } => {
                best.as_ref()
                    .map_or(0, |(_, ks)| ks.capacity() * size_of::<KeyAtom>())
                    + *heap_bytes
            }
            Self::TagUnion { heap_bytes, .. } => *heap_bytes,
            _ => 0,
        }
    }

    fn unspillable_heap_bytes(&self) -> usize {
        match self {
            Self::Percentile(v) => v.capacity() * size_of::<f64>(),
            Self::ArrayAgg {
                values,
                set,
                heap_bytes,
                ..
            } => {
                values.capacity() * size_of::<KeyAtom>()
                    + set
                        .as_ref()
                        .map_or(0, |s| s.capacity() * (size_of::<KeyAtom>() + 8))
                    + *heap_bytes
            }
            _ => 0,
        }
    }

    /// Upper-bound additional `(spillable, unspillable)` bytes that folding `row` could allocate.
    fn estimate_fold_growth(
        &self,
        kind: &AccKind,
        inputs: &[&Column],
        row: usize,
    ) -> (usize, usize) {
        match (self, kind) {
            (Self::CountDistinct { .. }, _) => {
                let col = inputs[0];
                if col.is_null(row) {
                    (0, 0)
                } else {
                    (
                        col_row_heap_estimate(col, row) + size_of::<KeyAtom>() + 64,
                        0,
                    )
                }
            }
            (Self::Min(_) | Self::Max(_), _) => {
                let col = inputs[0];
                if col.is_null(row) {
                    (0, 0)
                } else {
                    (col_row_heap_estimate(col, row), 0)
                }
            }
            (Self::ApproxPercentile(_), _) => (size_of::<(i32, u64)>() * 2, 0),
            (Self::Percentile(_), _) => {
                let col = inputs[0];
                if col.is_null(row) {
                    (0, 0)
                } else {
                    (0, size_of::<f64>() * 2)
                }
            }
            (Self::ArrayAgg { .. }, _) => {
                let col = inputs[0];
                let item = if col.is_null(row) {
                    0
                } else {
                    col_row_heap_estimate(col, row)
                };
                (0, item + size_of::<KeyAtom>() * 2 + 64)
            }
            (Self::FirstNonNullByOrder { .. }, AccKind::FirstNonNullByOrder { order_by, .. }) => {
                let val_col = inputs[0];
                if val_col.is_null(row) {
                    (0, 0)
                } else {
                    (
                        estimate_first_by_order_bytes(val_col, &inputs[1..], row, order_by.len()),
                        0,
                    )
                }
            }
            (Self::FirstByOrder { .. }, AccKind::FirstByOrder { order_by, .. }) => (
                estimate_first_by_order_bytes(inputs[0], &inputs[1..], row, order_by.len()),
                0,
            ),
            (Self::TagUnion { .. }, _) => {
                let col = inputs[0];
                if col.is_null(row) {
                    (0, 0)
                } else {
                    let mut est = 0usize;
                    if let Some(lref) = eval::list_ref_at(col, row) {
                        let n = list_ref_len(lref);
                        for i in 0..n {
                            if let Some(s) = list_ref_elem(lref, i).read_str() {
                                est = est.saturating_add(s.len() + 48);
                            }
                        }
                    }
                    (est, 0)
                }
            }
            _ => (0, 0),
        }
    }

    /// Upper-bound additional `(spillable, unspillable)` bytes that merging `partial_cols[row]` could allocate.
    fn estimate_merge_growth(&self, partial_cols: &[&Column], row: usize) -> (usize, usize) {
        match self {
            Self::CountDistinct { .. } => {
                let col = partial_cols[0];
                if col.is_null(row) {
                    (0, 0)
                } else if let Some(lref) = eval::list_ref_at(col, row) {
                    let n = list_ref_len(lref);
                    (
                        n * (size_of::<KeyAtom>() + 48) + col_row_heap_estimate(col, row),
                        0,
                    )
                } else {
                    (0, 0)
                }
            }
            Self::Min(_) | Self::Max(_) => {
                let col = partial_cols[0];
                if col.is_null(row) {
                    (0, 0)
                } else {
                    (col_row_heap_estimate(col, row), 0)
                }
            }
            Self::ApproxPercentile(_) => {
                let col = partial_cols[0];
                (eval::read_bytes(col, row).map_or(0, <[u8]>::len) * 2, 0)
            }
            Self::Percentile(_) => {
                let col = partial_cols[0];
                if let Some(lref) = eval::list_ref_at(col, row) {
                    (0, list_ref_len(lref) * size_of::<f64>() * 2)
                } else {
                    (0, 0)
                }
            }
            Self::ArrayAgg { .. } => {
                let col = partial_cols[0];
                if col.is_null(row) {
                    (0, 0)
                } else if let Some(lref) = eval::list_ref_at(col, row) {
                    let n = list_ref_len(lref);
                    (
                        0,
                        n * (size_of::<KeyAtom>() * 2 + 48) + col_row_heap_estimate(col, row),
                    )
                } else {
                    (0, 0)
                }
            }
            Self::FirstNonNullByOrder { .. } => {
                let val_col = partial_cols[0];
                if val_col.is_null(row) {
                    (0, 0)
                } else {
                    (
                        estimate_first_by_order_bytes(
                            val_col,
                            &partial_cols[1..],
                            row,
                            partial_cols.len().saturating_sub(1),
                        ),
                        0,
                    )
                }
            }
            Self::FirstByOrder { .. } => {
                if read_bool(partial_cols[0], row) != Some(true) {
                    (0, 0)
                } else {
                    (
                        estimate_first_by_order_bytes(
                            partial_cols[1],
                            &partial_cols[2..],
                            row,
                            partial_cols.len().saturating_sub(2),
                        ),
                        0,
                    )
                }
            }
            Self::TagUnion { .. } => {
                let col = partial_cols[0];
                if let Some(lref) = eval::list_ref_at(col, row) {
                    let n = list_ref_len(lref);
                    let mut est = 0usize;
                    for i in 0..n {
                        if let Some(s) = list_ref_elem(lref, i).read_str() {
                            est = est.saturating_add(s.len() + 48);
                        }
                    }
                    (est, 0)
                } else {
                    (0, 0)
                }
            }
            _ => (0, 0),
        }
    }

    fn fold_row(
        &mut self,
        kind: &AccKind,
        inputs: &[&Column],
        row: usize,
    ) -> Result<(), QueryError> {
        match (self, kind) {
            (Self::Count(cnt), AccKind::CountStar) => {
                *cnt = cnt
                    .checked_add(1)
                    .ok_or_else(|| QueryError::failed("count overflow"))?;
            }
            (Self::Count(cnt), AccKind::Count) => {
                if !inputs[0].is_null(row) {
                    *cnt = cnt
                        .checked_add(1)
                        .ok_or_else(|| QueryError::failed("count overflow"))?;
                }
            }
            (Self::CountDistinct { set, heap_bytes }, AccKind::CountDistinct(_)) => {
                let col = inputs[0];
                if !col.is_null(row) {
                    let atom = read_key_atom(col, row);
                    let b = atom.heap_bytes();
                    if set.insert(atom) {
                        *heap_bytes += b;
                    }
                }
            }
            (Self::SumI64(acc), AccKind::SumI64) => {
                if let Some(v) = read_i64(inputs[0], row) {
                    *acc = Some(match *acc {
                        Some(cur) => cur
                            .checked_add(v)
                            .ok_or_else(|| QueryError::failed("integer sum overflow"))?,
                        None => v,
                    });
                }
            }
            (Self::SumF64(acc), AccKind::SumF64) => {
                if let Some(v) = read_f64(inputs[0], row) {
                    *acc = Some(match *acc {
                        Some(cur) => cur + v,
                        None => v,
                    });
                }
            }
            (Self::SumDecimal(acc), AccKind::SumDecimal { .. }) => {
                if let Some(v) = read_decimal(inputs[0], row) {
                    let next = match *acc {
                        Some(cur) => cur
                            .checked_add(v)
                            .ok_or_else(|| QueryError::failed("decimal sum overflow"))?,
                        None => v,
                    };
                    *acc = Some(check_decimal_bounds(next, 38)?);
                }
            }
            (Self::SumCostDecimal(acc), AccKind::SumCostDecimal(in_dt)) => {
                let col = inputs[0];
                if !col.is_null(row) {
                    let scaled = read_cost_decimal_value(col, row, in_dt)?;
                    let next = match *acc {
                        Some(cur) => cur
                            .checked_add(scaled)
                            .ok_or_else(|| QueryError::failed("cost decimal sum overflow"))?,
                        None => scaled,
                    };
                    *acc = Some(check_decimal_bounds(next, COST_DECIMAL_PRECISION)?);
                }
            }
            (Self::AvgFloat { sum, count }, AccKind::AvgFloat(in_dt)) => {
                if let Some(v) = read_numeric_as_f64(inputs[0], row, in_dt) {
                    *sum += v;
                    *count += 1;
                }
            }
            (Self::AvgDecimal { sum, count }, AccKind::AvgDecimal { .. }) => {
                if let Some(v) = read_decimal(inputs[0], row) {
                    let next = sum
                        .checked_add(v)
                        .ok_or_else(|| QueryError::failed("decimal avg sum overflow"))?;
                    *sum = check_decimal_bounds(next, 38)?;
                    *count += 1;
                }
            }
            (Self::Min(best), AccKind::Min(dt)) => {
                let col = inputs[0];
                if !col.is_null(row) {
                    let cand = read_key_atom(col, row);
                    let replace = match best {
                        None => true,
                        Some(cur) => compare_non_null_atoms(&cand, cur, dt)? == Ordering::Less,
                    };
                    if replace {
                        *best = Some(cand);
                    }
                }
            }
            (Self::Max(best), AccKind::Max(dt)) => {
                let col = inputs[0];
                if !col.is_null(row) {
                    let cand = read_key_atom(col, row);
                    let replace = match best {
                        None => true,
                        Some(cur) => compare_non_null_atoms(&cand, cur, dt)? == Ordering::Greater,
                    };
                    if replace {
                        *best = Some(cand);
                    }
                }
            }
            (Self::BoolOr(acc), AccKind::BoolOr) => {
                if let Some(b) = read_bool(inputs[0], row) {
                    *acc = Some(acc.unwrap_or(false) || b);
                }
            }
            (Self::BoolAnd(acc), AccKind::BoolAnd) => {
                if let Some(b) = read_bool(inputs[0], row) {
                    *acc = Some(acc.unwrap_or(true) && b);
                }
            }
            (Self::ApproxPercentile(sketch), AccKind::ApproxPercentile { input_type, .. }) => {
                if let Some(v) = read_numeric_as_f64(inputs[0], row, input_type) {
                    sketch.add(v);
                }
            }
            (Self::Percentile(values), AccKind::Percentile { input_type, .. }) => {
                if let Some(v) = read_numeric_as_f64(inputs[0], row, input_type) {
                    values.push(v);
                }
            }
            (
                Self::ArrayAgg {
                    seen_any,
                    values,
                    set,
                    heap_bytes,
                },
                AccKind::ArrayAgg { .. },
            ) => {
                *seen_any = true;
                let atom = read_key_atom(inputs[0], row);
                if let Some(distinct_set) = set {
                    if distinct_set.insert(atom.clone()) {
                        *heap_bytes += atom.heap_bytes() * 2;
                        values.push(atom);
                    }
                } else {
                    *heap_bytes += atom.heap_bytes();
                    values.push(atom);
                }
            }
            (
                Self::FirstNonNullByOrder { best, heap_bytes },
                AccKind::FirstNonNullByOrder { order_by, .. },
            ) => {
                let val_col = inputs[0];
                if !val_col.is_null(row) {
                    update_first_by_order(best, heap_bytes, val_col, &inputs[1..], row, order_by)?;
                }
            }
            (Self::FirstByOrder { best, heap_bytes }, AccKind::FirstByOrder { order_by, .. }) => {
                update_first_by_order(best, heap_bytes, inputs[0], &inputs[1..], row, order_by)?;
            }
            (Self::TagUnion { tags, heap_bytes }, AccKind::TagUnion) => {
                let col = inputs[0];
                if let Some(lref) = eval::list_ref_at(col, row) {
                    let n = list_ref_len(lref);
                    for i in 0..n {
                        if let Some(s) = list_ref_elem(lref, i).read_str()
                            && !tags.contains(s)
                        {
                            *heap_bytes += s.len() + 32;
                            tags.insert(s.to_owned());
                        }
                    }
                }
            }
            _ => return Err(QueryError::internal("accumulator kind mismatch")),
        }
        Ok(())
    }

    fn merge_row(
        &mut self,
        kind: &AccKind,
        partial_cols: &[&Column],
        row: usize,
    ) -> Result<(), QueryError> {
        match (self, kind) {
            (Self::Count(cnt), AccKind::CountStar | AccKind::Count) => {
                let add = read_i64(partial_cols[0], row).unwrap_or(0);
                *cnt = cnt
                    .checked_add(add)
                    .ok_or_else(|| QueryError::failed("count overflow"))?;
            }
            (Self::CountDistinct { set, heap_bytes }, AccKind::CountDistinct(_)) => {
                if let Some(lref) = eval::list_ref_at(partial_cols[0], row) {
                    let n = list_ref_len(lref);
                    for i in 0..n {
                        let elem = list_ref_elem(lref, i);
                        if !elem.is_null() {
                            let atom = elem_ref_to_key_atom(elem);
                            let b = atom.heap_bytes();
                            if set.insert(atom) {
                                *heap_bytes += b;
                            }
                        }
                    }
                }
            }
            (Self::SumI64(acc), AccKind::SumI64) => {
                if let Some(v) = read_i64(partial_cols[0], row) {
                    *acc = Some(match *acc {
                        Some(cur) => cur
                            .checked_add(v)
                            .ok_or_else(|| QueryError::failed("integer sum overflow"))?,
                        None => v,
                    });
                }
            }
            (Self::SumF64(acc), AccKind::SumF64) => {
                if let Some(v) = read_f64(partial_cols[0], row) {
                    *acc = Some(match *acc {
                        Some(cur) => cur + v,
                        None => v,
                    });
                }
            }
            (Self::SumDecimal(acc), AccKind::SumDecimal { .. }) => {
                if let Some(v) = read_decimal(partial_cols[0], row) {
                    let next = match *acc {
                        Some(cur) => cur
                            .checked_add(v)
                            .ok_or_else(|| QueryError::failed("decimal sum overflow"))?,
                        None => v,
                    };
                    *acc = Some(check_decimal_bounds(next, 38)?);
                }
            }
            (Self::SumCostDecimal(acc), AccKind::SumCostDecimal(_)) => {
                if let Some(v) = read_decimal(partial_cols[0], row) {
                    let next = match *acc {
                        Some(cur) => cur
                            .checked_add(v)
                            .ok_or_else(|| QueryError::failed("cost decimal sum overflow"))?,
                        None => v,
                    };
                    *acc = Some(check_decimal_bounds(next, COST_DECIMAL_PRECISION)?);
                }
            }
            (Self::AvgFloat { sum, count }, AccKind::AvgFloat(_)) => {
                let c = read_i64(partial_cols[1], row).unwrap_or(0);
                if c > 0 {
                    let s = read_f64(partial_cols[0], row).unwrap_or(0.0);
                    *sum += s;
                    *count += c;
                }
            }
            (Self::AvgDecimal { sum, count }, AccKind::AvgDecimal { .. }) => {
                let c = read_i64(partial_cols[1], row).unwrap_or(0);
                if c > 0 {
                    let s = read_decimal(partial_cols[0], row).unwrap_or(0);
                    let next = sum
                        .checked_add(s)
                        .ok_or_else(|| QueryError::failed("decimal avg sum overflow"))?;
                    *sum = check_decimal_bounds(next, 38)?;
                    *count += c;
                }
            }
            (Self::Min(best), AccKind::Min(dt)) => {
                let col = partial_cols[0];
                if !col.is_null(row) {
                    let cand = read_key_atom(col, row);
                    let replace = match best {
                        None => true,
                        Some(cur) => compare_non_null_atoms(&cand, cur, dt)? == Ordering::Less,
                    };
                    if replace {
                        *best = Some(cand);
                    }
                }
            }
            (Self::Max(best), AccKind::Max(dt)) => {
                let col = partial_cols[0];
                if !col.is_null(row) {
                    let cand = read_key_atom(col, row);
                    let replace = match best {
                        None => true,
                        Some(cur) => compare_non_null_atoms(&cand, cur, dt)? == Ordering::Greater,
                    };
                    if replace {
                        *best = Some(cand);
                    }
                }
            }
            (Self::BoolOr(acc), AccKind::BoolOr) => {
                if let Some(b) = read_bool(partial_cols[0], row) {
                    *acc = Some(acc.unwrap_or(false) || b);
                }
            }
            (Self::BoolAnd(acc), AccKind::BoolAnd) => {
                if let Some(b) = read_bool(partial_cols[0], row) {
                    *acc = Some(acc.unwrap_or(true) && b);
                }
            }
            (Self::ApproxPercentile(sketch), AccKind::ApproxPercentile { .. }) => {
                let bytes = eval::read_bytes(partial_cols[0], row)
                    .ok_or_else(|| QueryError::internal("missing DDSketch partial bytes"))?;
                let other = DdSketch::from_bytes(bytes)?;
                sketch.merge(&other);
            }
            (Self::Percentile(values), AccKind::Percentile { .. }) => {
                if let Some(lref) = eval::list_ref_at(partial_cols[0], row) {
                    let n = list_ref_len(lref);
                    values.reserve(n);
                    for i in 0..n {
                        if let Some(v) = list_ref_elem(lref, i).read_f64() {
                            values.push(v);
                        }
                    }
                }
            }
            (
                Self::ArrayAgg {
                    seen_any,
                    values,
                    set,
                    heap_bytes,
                },
                AccKind::ArrayAgg { .. },
            ) => {
                let col = partial_cols[0];
                if !col.is_null(row) {
                    *seen_any = true;
                    if let Some(lref) = eval::list_ref_at(col, row) {
                        let n = list_ref_len(lref);
                        for i in 0..n {
                            let atom = elem_ref_to_key_atom(list_ref_elem(lref, i));
                            if let Some(distinct_set) = set {
                                if distinct_set.insert(atom.clone()) {
                                    *heap_bytes += atom.heap_bytes() * 2;
                                    values.push(atom);
                                }
                            } else {
                                *heap_bytes += atom.heap_bytes();
                                values.push(atom);
                            }
                        }
                    }
                }
            }
            (
                Self::FirstNonNullByOrder { best, heap_bytes },
                AccKind::FirstNonNullByOrder { order_by, .. },
            ) => {
                let val_col = partial_cols[0];
                if !val_col.is_null(row) {
                    update_first_by_order(
                        best,
                        heap_bytes,
                        val_col,
                        &partial_cols[1..],
                        row,
                        order_by,
                    )?;
                }
            }
            (Self::FirstByOrder { best, heap_bytes }, AccKind::FirstByOrder { order_by, .. }) => {
                if read_bool(partial_cols[0], row) == Some(true) {
                    update_first_by_order(
                        best,
                        heap_bytes,
                        partial_cols[1],
                        &partial_cols[2..],
                        row,
                        order_by,
                    )?;
                }
            }
            (Self::TagUnion { tags, heap_bytes }, AccKind::TagUnion) => {
                if let Some(lref) = eval::list_ref_at(partial_cols[0], row) {
                    let n = list_ref_len(lref);
                    for i in 0..n {
                        if let Some(s) = list_ref_elem(lref, i).read_str()
                            && !tags.contains(s)
                        {
                            *heap_bytes += s.len() + 32;
                            tags.insert(s.to_owned());
                        }
                    }
                }
            }
            _ => return Err(QueryError::internal("partial state kind mismatch")),
        }
        Ok(())
    }

    fn finalize_scalar(&self, kind: &AccKind) -> Result<ScalarValue, QueryError> {
        match (self, kind) {
            (Self::Count(c), AccKind::CountStar | AccKind::Count) => Ok(ScalarValue::I64(*c)),
            (Self::CountDistinct { set, .. }, AccKind::CountDistinct(_)) => {
                let len = i64::try_from(set.len())
                    .map_err(|_| QueryError::failed("count distinct overflow"))?;
                Ok(ScalarValue::I64(len))
            }
            (Self::SumI64(v), AccKind::SumI64) => {
                Ok(v.map_or(ScalarValue::Null(DataType::I64), ScalarValue::I64))
            }
            (Self::SumF64(v), AccKind::SumF64) => {
                Ok(v.map_or(ScalarValue::Null(DataType::F64), ScalarValue::F64))
            }
            (Self::SumDecimal(v), AccKind::SumDecimal { scale }) => Ok(match v {
                Some(val) => ScalarValue::Decimal128 {
                    value: *val,
                    precision: 38,
                    scale: *scale,
                },
                None => ScalarValue::Null(DataType::Decimal128 {
                    precision: 38,
                    scale: *scale,
                }),
            }),
            (Self::SumCostDecimal(v), AccKind::SumCostDecimal(_)) => Ok(match v {
                Some(val) => ScalarValue::Decimal128 {
                    value: *val,
                    precision: COST_DECIMAL_PRECISION,
                    scale: COST_DECIMAL_SCALE,
                },
                None => ScalarValue::Null(DataType::cost_decimal()),
            }),
            (Self::AvgFloat { sum, count }, AccKind::AvgFloat(_)) => {
                if *count == 0 {
                    Ok(ScalarValue::Null(DataType::F64))
                } else {
                    Ok(ScalarValue::F64(*sum / (*count as f64)))
                }
            }
            (
                Self::AvgDecimal { sum, count },
                AccKind::AvgDecimal {
                    in_scale,
                    out_precision,
                    out_scale,
                },
            ) => {
                if *count == 0 {
                    Ok(ScalarValue::Null(DataType::Decimal128 {
                        precision: *out_precision,
                        scale: *out_scale,
                    }))
                } else {
                    let avg =
                        decimal_avg_finalize(*sum, *in_scale, *out_precision, *out_scale, *count)?;
                    Ok(ScalarValue::Decimal128 {
                        value: avg,
                        precision: *out_precision,
                        scale: *out_scale,
                    })
                }
            }
            (Self::Min(v) | Self::Max(v), AccKind::Min(dt) | AccKind::Max(dt)) => Ok(match v {
                Some(atom) => atom.to_scalar(dt),
                None => ScalarValue::Null(dt.clone()),
            }),
            (Self::BoolOr(v) | Self::BoolAnd(v), AccKind::BoolOr | AccKind::BoolAnd) => {
                Ok(v.map_or(ScalarValue::Null(DataType::Bool), ScalarValue::Bool))
            }
            (Self::ApproxPercentile(sk), AccKind::ApproxPercentile { quantile, .. }) => Ok(sk
                .quantile(*quantile)?
                .map_or(ScalarValue::Null(DataType::F64), ScalarValue::F64)),
            (Self::Percentile(vals), AccKind::Percentile { quantile, .. }) => {
                if vals.is_empty() {
                    Ok(ScalarValue::Null(DataType::F64))
                } else {
                    let mut sorted = vals.clone();
                    sorted.sort_by(|a, b| f64_cmp(*a, *b));
                    let res = exact_percentile_sorted(&sorted, *quantile)?;
                    Ok(ScalarValue::F64(res))
                }
            }
            (
                Self::ArrayAgg {
                    seen_any, values, ..
                },
                AccKind::ArrayAgg { input_type, .. },
            ) => {
                if !*seen_any {
                    Ok(ScalarValue::Null(DataType::List(Box::new(
                        input_type.clone(),
                    ))))
                } else {
                    let items = values.iter().map(|a| a.to_scalar(input_type)).collect();
                    Ok(ScalarValue::List {
                        item_type: input_type.clone(),
                        values: items,
                    })
                }
            }
            (
                Self::FirstNonNullByOrder { best, .. },
                AccKind::FirstNonNullByOrder { value_type, .. },
            )
            | (Self::FirstByOrder { best, .. }, AccKind::FirstByOrder { value_type, .. }) => {
                Ok(match best {
                    Some((val, _)) => val.to_scalar(value_type),
                    None => ScalarValue::Null(value_type.clone()),
                })
            }
            (Self::TagUnion { tags, .. }, AccKind::TagUnion) => {
                let items = tags.iter().cloned().map(ScalarValue::Utf8).collect();
                Ok(ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: items,
                })
            }
            _ => Err(QueryError::internal("accumulator finalize mismatch")),
        }
    }

    fn encode_partial_scalars(&self, kind: &AccKind, out: &mut Vec<ScalarValue>) {
        match (self, kind) {
            (Self::Count(c), AccKind::CountStar | AccKind::Count) => {
                out.push(ScalarValue::I64(*c));
            }
            (Self::CountDistinct { set, .. }, AccKind::CountDistinct(dt)) => {
                let items = set.iter().map(|a| a.to_scalar(dt)).collect();
                out.push(ScalarValue::List {
                    item_type: dt.clone(),
                    values: items,
                });
            }
            (Self::SumI64(v), AccKind::SumI64) => {
                out.push(v.map_or(ScalarValue::Null(DataType::I64), ScalarValue::I64));
            }
            (Self::SumF64(v), AccKind::SumF64) => {
                out.push(v.map_or(ScalarValue::Null(DataType::F64), ScalarValue::F64));
            }
            (Self::SumDecimal(v), AccKind::SumDecimal { scale }) => {
                out.push(match v {
                    Some(val) => ScalarValue::Decimal128 {
                        value: *val,
                        precision: 38,
                        scale: *scale,
                    },
                    None => ScalarValue::Null(DataType::Decimal128 {
                        precision: 38,
                        scale: *scale,
                    }),
                });
            }
            (Self::SumCostDecimal(v), AccKind::SumCostDecimal(_)) => {
                out.push(match v {
                    Some(val) => ScalarValue::Decimal128 {
                        value: *val,
                        precision: COST_DECIMAL_PRECISION,
                        scale: COST_DECIMAL_SCALE,
                    },
                    None => ScalarValue::Null(DataType::cost_decimal()),
                });
            }
            (Self::AvgFloat { sum, count }, AccKind::AvgFloat(_)) => {
                out.push(if *count == 0 {
                    ScalarValue::Null(DataType::F64)
                } else {
                    ScalarValue::F64(*sum)
                });
                out.push(ScalarValue::I64(*count));
            }
            (Self::AvgDecimal { sum, count }, AccKind::AvgDecimal { in_scale, .. }) => {
                out.push(if *count == 0 {
                    ScalarValue::Null(DataType::Decimal128 {
                        precision: 38,
                        scale: *in_scale,
                    })
                } else {
                    ScalarValue::Decimal128 {
                        value: *sum,
                        precision: 38,
                        scale: *in_scale,
                    }
                });
                out.push(ScalarValue::I64(*count));
            }
            (Self::Min(v) | Self::Max(v), AccKind::Min(dt) | AccKind::Max(dt)) => {
                out.push(match v {
                    Some(atom) => atom.to_scalar(dt),
                    None => ScalarValue::Null(dt.clone()),
                });
            }
            (Self::BoolOr(v) | Self::BoolAnd(v), AccKind::BoolOr | AccKind::BoolAnd) => {
                out.push(v.map_or(ScalarValue::Null(DataType::Bool), ScalarValue::Bool));
            }
            (Self::ApproxPercentile(sk), AccKind::ApproxPercentile { .. }) => {
                out.push(ScalarValue::Binary(sk.to_bytes()));
            }
            (Self::Percentile(vals), AccKind::Percentile { .. }) => {
                let items = vals.iter().copied().map(ScalarValue::F64).collect();
                out.push(ScalarValue::List {
                    item_type: DataType::F64,
                    values: items,
                });
            }
            (
                Self::ArrayAgg {
                    seen_any, values, ..
                },
                AccKind::ArrayAgg { input_type, .. },
            ) => {
                if !*seen_any {
                    out.push(ScalarValue::Null(DataType::List(Box::new(
                        input_type.clone(),
                    ))));
                } else {
                    let items = values.iter().map(|a| a.to_scalar(input_type)).collect();
                    out.push(ScalarValue::List {
                        item_type: input_type.clone(),
                        values: items,
                    });
                }
            }
            (
                Self::FirstNonNullByOrder { best, .. },
                AccKind::FirstNonNullByOrder {
                    value_type,
                    order_by,
                },
            ) => match best {
                Some((val, keys)) => {
                    out.push(val.to_scalar(value_type));
                    for (k_atom, key_spec) in keys.iter().zip(order_by) {
                        out.push(k_atom.to_scalar(&key_spec.expr.data_type()));
                    }
                }
                None => {
                    out.push(ScalarValue::Null(value_type.clone()));
                    for key_spec in order_by {
                        out.push(ScalarValue::Null(key_spec.expr.data_type()));
                    }
                }
            },
            (
                Self::FirstByOrder { best, .. },
                AccKind::FirstByOrder {
                    value_type,
                    order_by,
                },
            ) => match best {
                Some((val, keys)) => {
                    out.push(ScalarValue::Bool(true));
                    out.push(val.to_scalar(value_type));
                    for (k_atom, key_spec) in keys.iter().zip(order_by) {
                        out.push(k_atom.to_scalar(&key_spec.expr.data_type()));
                    }
                }
                None => {
                    out.push(ScalarValue::Bool(false));
                    out.push(ScalarValue::Null(value_type.clone()));
                    for key_spec in order_by {
                        out.push(ScalarValue::Null(key_spec.expr.data_type()));
                    }
                }
            },
            (Self::TagUnion { tags, .. }, AccKind::TagUnion) => {
                let items = tags.iter().cloned().map(ScalarValue::Utf8).collect();
                out.push(ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: items,
                });
            }
            _ => {}
        }
    }
}

fn estimate_first_by_order_bytes(
    val_col: &Column,
    order_cols: &[&Column],
    row: usize,
    key_count: usize,
) -> usize {
    let mut est = key_count * size_of::<KeyAtom>();
    if !val_col.is_null(row) {
        est += col_row_heap_estimate(val_col, row);
    }
    for col in order_cols {
        if !col.is_null(row) {
            est += col_row_heap_estimate(col, row);
        }
    }
    est
}

fn update_first_by_order(
    best: &mut Option<(KeyAtom, Vec<KeyAtom>)>,
    heap_bytes: &mut usize,
    val_col: &Column,
    order_cols: &[&Column],
    row: usize,
    order_by: &[SortKey],
) -> Result<(), QueryError> {
    let replace = match best {
        None => true,
        Some((_, cur_keys)) => {
            compare_order_keys(order_cols, row, cur_keys, order_by)? == Ordering::Less
        }
    };
    if replace {
        let new_val = read_key_atom(val_col, row);
        let mut new_keys = Vec::with_capacity(order_cols.len());
        let mut new_bytes = new_val.heap_bytes();
        for oc in order_cols {
            let ka = read_key_atom(oc, row);
            new_bytes += ka.heap_bytes();
            new_keys.push(ka);
        }
        *best = Some((new_val, new_keys));
        *heap_bytes = new_bytes;
    }
    Ok(())
}

fn exact_percentile_sorted(sorted: &[f64], quantile: f64) -> Result<f64, QueryError> {
    if !quantile.is_finite() || !(0.0..=1.0).contains(&quantile) {
        return Err(QueryError::failed(
            "percentile quantile must be between 0.0 and 1.0",
        ));
    }
    let n = sorted.len();
    if n == 0 {
        return Err(QueryError::internal("empty slice in percentile_cont"));
    }
    if n == 1 {
        return Ok(sorted[0]);
    }
    let pos = quantile * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    if lo == hi || frac == 0.0 || sorted[lo] == sorted[hi] {
        Ok(sorted[lo])
    } else {
        Ok(sorted[lo] + frac * (sorted[hi] - sorted[lo]))
    }
}

fn elem_ref_to_key_atom(elem: ElemRef<'_>) -> KeyAtom {
    if elem.is_null() {
        return KeyAtom::Null;
    }
    match elem {
        ElemRef::Null => KeyAtom::Null,
        ElemRef::Col(col, row) => read_key_atom(col, row),
        ElemRef::Scalar(sv) => match sv {
            ScalarValue::Null(_) => KeyAtom::Null,
            ScalarValue::Bool(b) => KeyAtom::Bool(*b),
            ScalarValue::I64(v) => KeyAtom::I64(*v),
            ScalarValue::F64(v) => KeyAtom::F64(f64_canonical_bits(*v)),
            ScalarValue::Decimal128 { value, .. } => KeyAtom::Decimal128(*value),
            ScalarValue::TimestampNs { value, .. } => KeyAtom::TimestampNs(*value),
            ScalarValue::Date(d) => KeyAtom::Date(*d),
            ScalarValue::Interval(iv) => KeyAtom::Interval(*iv),
            ScalarValue::Utf8(s) => KeyAtom::Utf8(s.clone()),
            ScalarValue::Binary(b) => KeyAtom::Binary(b.clone()),
            ScalarValue::FixedBytes8(b) => KeyAtom::FixedBytes8(*b),
            ScalarValue::FixedBytes16(b) => KeyAtom::FixedBytes16(*b),
            other => KeyAtom::Scalar(other.clone()),
        },
    }
}

fn col_row_heap_estimate(col: &Column, row: usize) -> usize {
    match col.data_type() {
        DataType::Utf8 => read_str(col, row).map_or(0, str::len),
        DataType::Binary => eval::read_bytes(col, row).map_or(0, <[u8]>::len),
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links => {
            read_scalar(col, row).heap_bytes()
        }
        _ => 0,
    }
}

fn read_numeric_as_f64(col: &Column, row: usize, dt: &DataType) -> Option<f64> {
    if col.is_null(row) {
        return None;
    }
    match dt {
        DataType::I64 => read_i64(col, row).map(|v| v as f64),
        DataType::F64 => read_f64(col, row),
        DataType::Decimal128 { scale, .. } => {
            read_decimal(col, row).map(|v| (v as f64) / 10f64.powi(i32::from(*scale)))
        }
        _ => None,
    }
}

fn read_cost_decimal_value(col: &Column, row: usize, dt: &DataType) -> Result<i128, QueryError> {
    match dt {
        DataType::F64 => {
            let v = read_f64(col, row)
                .ok_or_else(|| QueryError::internal("expected f64 value for SumCostDecimal"))?;
            f64_to_decimal(v, COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE)
        }
        DataType::I64 => {
            let v = read_i64(col, row)
                .ok_or_else(|| QueryError::internal("expected i64 value for SumCostDecimal"))?;
            i64_to_decimal(v, COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE)
        }
        DataType::Decimal128 { scale, .. } => {
            let v = read_decimal(col, row)
                .ok_or_else(|| QueryError::internal("expected decimal value for SumCostDecimal"))?;
            rescale_decimal(v, *scale, COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE)
        }
        _ => Err(QueryError::failed(
            "unsupported input type for SumCostDecimal",
        )),
    }
}

// ---------------------------------------------------------------------------
// In-memory open-addressing hash table for grouped aggregation
// ---------------------------------------------------------------------------

struct GroupTable {
    slots: Vec<u32>,
    hashes: Vec<u64>,
    keys: Vec<Vec<KeyAtom>>,
    states: Vec<Vec<AccState>>,
    acc_kinds: Vec<AccKind>,
    spillable_bytes: usize,
    unspillable_bytes: usize,
}

impl GroupTable {
    fn new(acc_kinds: Vec<AccKind>) -> Self {
        Self {
            slots: Vec::new(),
            hashes: Vec::new(),
            keys: Vec::new(),
            states: Vec::new(),
            acc_kinds,
            spillable_bytes: 0,
            unspillable_bytes: 0,
        }
    }

    fn num_groups(&self) -> usize {
        self.keys.len()
    }

    fn lookup_by_cols(&self, hash: u64, group_cols: &[&Column], row: usize) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut idx = (hash as usize) & mask;
        loop {
            let slot = self.slots[idx];
            if slot == EMPTY_SLOT {
                return None;
            }
            let gi = slot as usize;
            if self.hashes[gi] == hash && keys_eq_cols(&self.keys[gi], group_cols, row) {
                return Some(gi);
            }
            idx = (idx + 1) & mask;
        }
    }

    fn new_group_overhead(&self, extra_key_heap: usize) -> usize {
        let mut bytes = 8
            + size_of::<Vec<KeyAtom>>()
            + size_of::<Vec<AccState>>()
            + self.acc_kinds.len() * size_of::<AccState>()
            + extra_key_heap;
        if self.slots.is_empty() {
            bytes += 16 * size_of::<u32>();
        } else if (self.keys.len() + 1) * 10 >= self.slots.len() * 7 {
            bytes += self.slots.len() * size_of::<u32>();
        }
        bytes
    }

    fn insert_group(&mut self, hash: u64, key: Vec<KeyAtom>) -> usize {
        if self.slots.is_empty() {
            self.slots = vec![EMPTY_SLOT; 16];
            self.spillable_bytes = self
                .spillable_bytes
                .saturating_add(self.slots.capacity() * size_of::<u32>());
        } else if (self.keys.len() + 1) * 10 >= self.slots.len() * 7 {
            self.rehash(self.slots.len() * 2);
        }

        let gi = self.keys.len();
        let key_heap: usize = key.capacity() * size_of::<KeyAtom>()
            + key.iter().map(KeyAtom::heap_bytes).sum::<usize>();
        let states: Vec<AccState> = self.acc_kinds.iter().map(AccKind::make_state).collect();
        let states_heap: usize = states.capacity() * size_of::<AccState>()
            + states
                .iter()
                .map(AccState::spillable_heap_bytes)
                .sum::<usize>();
        let states_unsp: usize = states.iter().map(AccState::unspillable_heap_bytes).sum();

        let old_outer = self.hashes.capacity() * size_of::<u64>()
            + self.keys.capacity() * size_of::<Vec<KeyAtom>>()
            + self.states.capacity() * size_of::<Vec<AccState>>();

        self.hashes.push(hash);
        self.keys.push(key);
        self.states.push(states);

        let new_outer = self.hashes.capacity() * size_of::<u64>()
            + self.keys.capacity() * size_of::<Vec<KeyAtom>>()
            + self.states.capacity() * size_of::<Vec<AccState>>();

        let mask = self.slots.len() - 1;
        let mut idx = (hash as usize) & mask;
        while self.slots[idx] != EMPTY_SLOT {
            idx = (idx + 1) & mask;
        }
        self.slots[idx] = gi as u32;

        self.spillable_bytes = self
            .spillable_bytes
            .saturating_sub(old_outer)
            .saturating_add(new_outer)
            .saturating_add(key_heap)
            .saturating_add(states_heap);
        self.unspillable_bytes = self.unspillable_bytes.saturating_add(states_unsp);
        gi
    }

    #[cfg(test)]
    fn compute_spillable_bytes(&self) -> usize {
        let mut total = self.slots.capacity() * size_of::<u32>()
            + self.hashes.capacity() * size_of::<u64>()
            + self.keys.capacity() * size_of::<Vec<KeyAtom>>()
            + self.states.capacity() * size_of::<Vec<AccState>>();
        for key in &self.keys {
            total += key.capacity() * size_of::<KeyAtom>();
            for atom in key {
                total += atom.heap_bytes();
            }
        }
        for row_states in &self.states {
            total += row_states.capacity() * size_of::<AccState>();
            for st in row_states {
                total += st.spillable_heap_bytes();
            }
        }
        total
    }

    fn rehash(&mut self, new_cap: usize) {
        let old_slots_bytes = self.slots.capacity() * size_of::<u32>();
        self.slots = vec![EMPTY_SLOT; new_cap];
        let new_slots_bytes = self.slots.capacity() * size_of::<u32>();
        self.spillable_bytes = self
            .spillable_bytes
            .saturating_sub(old_slots_bytes)
            .saturating_add(new_slots_bytes);
        let mask = new_cap - 1;
        for (gi, &h) in self.hashes.iter().enumerate() {
            let mut idx = (h as usize) & mask;
            while self.slots[idx] != EMPTY_SLOT {
                idx = (idx + 1) & mask;
            }
            self.slots[idx] = gi as u32;
        }
    }

    fn clear_and_free(&mut self, res: &mut Reservation, unspillable_res: &mut Reservation) {
        self.slots = Vec::new();
        self.hashes = Vec::new();
        self.keys = Vec::new();
        self.states = Vec::new();
        self.spillable_bytes = 0;
        self.unspillable_bytes = 0;
        res.free();
        unspillable_res.free();
    }
}

fn keys_eq_cols(key: &[KeyAtom], cols: &[&Column], row: usize) -> bool {
    key.iter()
        .zip(cols)
        .all(|(atom, col)| col_row_eq_atom(col, row, atom))
}

// ---------------------------------------------------------------------------
// Schema & expression validation helpers
// ---------------------------------------------------------------------------

fn validate_expr_against_schema(expr: &Expr, schema: &Schema) -> Result<(), QueryError> {
    match expr {
        Expr::Column {
            index, data_type, ..
        } => {
            let field = schema
                .fields()
                .get(*index)
                .ok_or_else(|| InvalidQuery::new("expression column index out of bounds", None))?;
            if field.data_type != *data_type {
                return Err(InvalidQuery::new("expression column type mismatch", None).into());
            }
            Ok(())
        }
        Expr::Literal(_) => Ok(()),
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => {
            validate_expr_against_schema(expr, schema)
        }
        Expr::Binary { left, right, .. } => {
            validate_expr_against_schema(left, schema)?;
            validate_expr_against_schema(right, schema)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            validate_expr_against_schema(expr, schema)?;
            validate_expr_against_schema(low, schema)?;
            validate_expr_against_schema(high, schema)
        }
        Expr::InList { expr, list, .. } => {
            validate_expr_against_schema(expr, schema)?;
            for item in list {
                validate_expr_against_schema(item, schema)?;
            }
            Ok(())
        }
        Expr::Like { expr, pattern, .. } => {
            validate_expr_against_schema(expr, schema)?;
            validate_expr_against_schema(pattern, schema)
        }
        Expr::Case {
            operand,
            when_then,
            else_expr,
            ..
        } => {
            if let Some(op) = operand {
                validate_expr_against_schema(op, schema)?;
            }
            for (w, t) in when_then {
                validate_expr_against_schema(w, schema)?;
                validate_expr_against_schema(t, schema)?;
            }
            if let Some(el) = else_expr {
                validate_expr_against_schema(el, schema)?;
            }
            Ok(())
        }
        Expr::MapIndex { map, key } => {
            validate_expr_against_schema(map, schema)?;
            validate_expr_against_schema(key, schema)
        }
        Expr::MakeArray { elems, .. } => {
            for e in elems {
                validate_expr_against_schema(e, schema)?;
            }
            Ok(())
        }
        Expr::Call { args, .. } => {
            for a in args {
                validate_expr_against_schema(a, schema)?;
            }
            Ok(())
        }
    }
}

fn is_sortable_type(dt: &DataType) -> bool {
    match dt {
        DataType::MapUtf8 | DataType::Events | DataType::Links => false,
        DataType::List(inner) => is_sortable_type(inner),
        _ => true,
    }
}

fn resolve_acc_kind(agg: &AggExpr, schema: &Schema) -> Result<AccKind, QueryError> {
    if agg.output_name.is_empty() {
        return Err(InvalidQuery::new("aggregate output_name cannot be empty", None).into());
    }
    if let Some(filter) = &agg.filter {
        validate_expr_against_schema(filter, schema)?;
        if !matches!(filter.data_type(), DataType::Bool | DataType::Null) {
            return Err(
                InvalidQuery::new("aggregate FILTER predicate must be boolean", None).into(),
            );
        }
    }

    match &agg.func {
        AggFunc::CountStar => Ok(AccKind::CountStar),
        AggFunc::Count(expr) => {
            validate_expr_against_schema(expr, schema)?;
            Ok(AccKind::Count)
        }
        AggFunc::CountDistinct(expr) => {
            validate_expr_against_schema(expr, schema)?;
            let dt = expr.data_type();
            DataType::List(Box::new(dt.clone())).validate(MAX_NESTING_DEPTH)?;
            Ok(AccKind::CountDistinct(dt))
        }
        AggFunc::Sum(expr) => {
            validate_expr_against_schema(expr, schema)?;
            match expr.data_type() {
                DataType::I64 => Ok(AccKind::SumI64),
                DataType::F64 => Ok(AccKind::SumF64),
                DataType::Decimal128 { scale, .. } => Ok(AccKind::SumDecimal { scale }),
                _ => Err(InvalidQuery::new("SUM requires numeric input", None).into()),
            }
        }
        AggFunc::SumCostDecimal(expr) => {
            validate_expr_against_schema(expr, schema)?;
            let dt = expr.data_type();
            if !matches!(
                dt,
                DataType::F64 | DataType::I64 | DataType::Decimal128 { .. }
            ) {
                return Err(
                    InvalidQuery::new("SumCostDecimal requires numeric input", None).into(),
                );
            }
            Ok(AccKind::SumCostDecimal(dt))
        }
        AggFunc::Avg(expr) => {
            validate_expr_against_schema(expr, schema)?;
            let dt = expr.data_type();
            match dt {
                DataType::I64 | DataType::F64 => Ok(AccKind::AvgFloat(dt)),
                DataType::Decimal128 { precision, scale } => {
                    let out_precision = precision.saturating_add(4).min(38);
                    let out_scale = (i16::from(scale) + 4).min(i16::from(out_precision)) as i8;
                    Ok(AccKind::AvgDecimal {
                        in_scale: scale,
                        out_precision,
                        out_scale,
                    })
                }
                _ => Err(InvalidQuery::new("AVG requires numeric input", None).into()),
            }
        }
        AggFunc::Min(expr) => {
            validate_expr_against_schema(expr, schema)?;
            let dt = expr.data_type();
            if !is_sortable_type(&dt) {
                return Err(InvalidQuery::new("MIN input type is not orderable", None).into());
            }
            Ok(AccKind::Min(dt))
        }
        AggFunc::Max(expr) => {
            validate_expr_against_schema(expr, schema)?;
            let dt = expr.data_type();
            if !is_sortable_type(&dt) {
                return Err(InvalidQuery::new("MAX input type is not orderable", None).into());
            }
            Ok(AccKind::Max(dt))
        }
        AggFunc::BoolOr(expr) => {
            validate_expr_against_schema(expr, schema)?;
            if expr.data_type() != DataType::Bool {
                return Err(InvalidQuery::new("BOOL_OR requires boolean input", None).into());
            }
            Ok(AccKind::BoolOr)
        }
        AggFunc::BoolAnd(expr) => {
            validate_expr_against_schema(expr, schema)?;
            if expr.data_type() != DataType::Bool {
                return Err(InvalidQuery::new("BOOL_AND requires boolean input", None).into());
            }
            Ok(AccKind::BoolAnd)
        }
        AggFunc::ApproxPercentileCont { expr, quantile } => {
            validate_expr_against_schema(expr, schema)?;
            if !quantile.is_finite() || !(0.0..=1.0).contains(quantile) {
                return Err(QueryError::failed(
                    "approx_percentile_cont quantile must be between 0.0 and 1.0",
                ));
            }
            let dt = expr.data_type();
            if !matches!(
                dt,
                DataType::I64 | DataType::F64 | DataType::Decimal128 { .. }
            ) {
                return Err(InvalidQuery::new(
                    "approx_percentile_cont requires numeric input",
                    None,
                )
                .into());
            }
            Ok(AccKind::ApproxPercentile {
                input_type: dt,
                quantile: *quantile,
            })
        }
        AggFunc::PercentileCont { expr, quantile } => {
            validate_expr_against_schema(expr, schema)?;
            if !quantile.is_finite() || !(0.0..=1.0).contains(quantile) {
                return Err(QueryError::failed(
                    "percentile_cont quantile must be between 0.0 and 1.0",
                ));
            }
            let dt = expr.data_type();
            if !matches!(
                dt,
                DataType::I64 | DataType::F64 | DataType::Decimal128 { .. }
            ) {
                return Err(
                    InvalidQuery::new("percentile_cont requires numeric input", None).into(),
                );
            }
            Ok(AccKind::Percentile {
                input_type: dt,
                quantile: *quantile,
            })
        }
        AggFunc::ArrayAgg { expr, distinct } => {
            validate_expr_against_schema(expr, schema)?;
            let dt = expr.data_type();
            DataType::List(Box::new(dt.clone())).validate(MAX_NESTING_DEPTH)?;
            Ok(AccKind::ArrayAgg {
                input_type: dt,
                distinct: *distinct,
            })
        }
        AggFunc::FirstNonNullByOrder { value, order_by } => {
            validate_expr_against_schema(value, schema)?;
            for key in order_by {
                validate_expr_against_schema(&key.expr, schema)?;
                if !is_sortable_type(&key.expr.data_type()) {
                    return Err(InvalidQuery::new(
                        "FirstNonNullByOrder sort key is not orderable",
                        None,
                    )
                    .into());
                }
            }
            Ok(AccKind::FirstNonNullByOrder {
                value_type: value.data_type(),
                order_by: order_by.clone(),
            })
        }
        AggFunc::FirstByOrder { value, order_by } => {
            validate_expr_against_schema(value, schema)?;
            for key in order_by {
                validate_expr_against_schema(&key.expr, schema)?;
                if !is_sortable_type(&key.expr.data_type()) {
                    return Err(
                        InvalidQuery::new("FirstByOrder sort key is not orderable", None).into(),
                    );
                }
            }
            Ok(AccKind::FirstByOrder {
                value_type: value.data_type(),
                order_by: order_by.clone(),
            })
        }
        AggFunc::TagUnion(expr) => {
            validate_expr_against_schema(expr, schema)?;
            let DataType::List(inner) = expr.data_type() else {
                return Err(InvalidQuery::new("TagUnion requires List(Utf8) input", None).into());
            };
            if *inner != DataType::Utf8 {
                return Err(InvalidQuery::new("TagUnion requires List(Utf8) input", None).into());
            }
            Ok(AccKind::TagUnion)
        }
    }
}

// ---------------------------------------------------------------------------
// HashAgg operator (`PushSink` + spill + combine)
// ---------------------------------------------------------------------------

/// Partitioned spillable hash aggregation sink supporting Shape B SQL and `sessions` aggregates.
pub struct HashAgg {
    spec: HashAggSpec,
    acc_kinds: Vec<AccKind>,
    group_types: Vec<DataType>,
    input_schema: Arc<Schema>,
    output_schema: Arc<Schema>,
    partial_schema: Arc<Schema>,
    ctx: ExecContext,
    global_states: Option<Vec<AccState>>,
    table: GroupTable,
    spilled: bool,
    partition_writers: Vec<Option<SpillWriter>>,
    res: Reservation,
    unspillable_res: Reservation,
}

impl fmt::Debug for HashAgg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashAgg")
            .field("group_by_cols", &self.spec.group_by.len())
            .field("aggs", &self.spec.aggs.len())
            .field("buffered_groups", &self.table.num_groups())
            .field("spilled", &self.spilled)
            .field("reserved_bytes", &self.res.size())
            .field("unspillable_bytes", &self.unspillable_res.size())
            .finish()
    }
}

impl HashAgg {
    /// Creates a new [`HashAgg`] sink validated against `input_schema`.
    pub fn new(
        spec: HashAggSpec,
        input_schema: Arc<Schema>,
        ctx: ExecContext,
    ) -> Result<Self, QueryError> {
        let mut group_types = Vec::with_capacity(spec.group_by.len());
        let mut out_fields = Vec::with_capacity(spec.group_by.len() + spec.aggs.len());
        let mut partial_fields = Vec::with_capacity(spec.group_by.len() + spec.aggs.len() * 2);

        for (i, (name, expr)) in spec.group_by.iter().enumerate() {
            if name.is_empty() {
                return Err(InvalidQuery::new("group_by output name cannot be empty", None).into());
            }
            validate_expr_against_schema(expr, &input_schema)?;
            let dt = expr.data_type();
            let nullable = match expr {
                Expr::Column { index, .. } => {
                    input_schema.fields().get(*index).is_none_or(|f| f.nullable)
                }
                _ => true,
            };
            group_types.push(dt.clone());
            out_fields.push(Field::new(name.clone(), dt.clone(), nullable));
            partial_fields.push(Field::new(format!("g_{i}"), dt, true));
        }

        let mut acc_kinds = Vec::with_capacity(spec.aggs.len());
        for (agg_idx, agg) in spec.aggs.iter().enumerate() {
            let kind = resolve_acc_kind(agg, &input_schema)?;
            out_fields.push(kind.output_field(&agg.output_name));
            partial_fields.extend(kind.partial_fields(agg_idx));
            acc_kinds.push(kind);
        }

        let output_schema = Arc::new(Schema::try_new(out_fields)?);
        let partial_schema = Arc::new(Schema::try_new(partial_fields)?);

        let is_grouped = !spec.group_by.is_empty();
        let global_states = if is_grouped {
            None
        } else {
            Some(acc_kinds.iter().map(AccKind::make_state).collect())
        };

        let table = GroupTable::new(acc_kinds.clone());
        let res = ctx.pool.register("hash_agg", is_grouped);
        let unspillable_res = ctx.pool.register("hash_agg_unspillable", false);

        Ok(Self {
            spec,
            acc_kinds,
            group_types,
            input_schema,
            output_schema,
            partial_schema,
            ctx,
            global_states,
            table,
            spilled: false,
            partition_writers: Vec::new(),
            res,
            unspillable_res,
        })
    }

    /// Combines multiple per-worker [`HashAgg`] sinks into finalized output [`Batch`]es.
    ///
    /// When `sinks` is empty and `spec.group_by.is_empty()`, emits the 1-row default global-agg batch.
    pub fn merge_sinks(
        sinks: Vec<Self>,
        spec: &HashAggSpec,
        input_schema: &Schema,
        ctx: &ExecContext,
    ) -> Result<Vec<Batch>, QueryError> {
        let combined = Self::combine_sinks_into(sinks, spec, input_schema, ctx)?;
        combined.into_batches()
    }

    /// Combines multiple per-worker [`HashAgg`] sinks into a streaming [`PartitionStream`] of
    /// finalized output [`Batch`]es (one spill partition at a time without materializing all groups).
    pub fn combine_sinks_to_stream(
        sinks: Vec<Self>,
        spec: &HashAggSpec,
        input_schema: &Schema,
        ctx: &ExecContext,
    ) -> Result<Box<dyn PartitionStream>, QueryError> {
        let combined = Self::combine_sinks_into(sinks, spec, input_schema, ctx)?;
        combined.into_stream()
    }

    fn combine_sinks_into(
        mut sinks: Vec<Self>,
        spec: &HashAggSpec,
        input_schema: &Schema,
        ctx: &ExecContext,
    ) -> Result<Self, QueryError> {
        for sink in &sinks {
            if sink.input_schema.as_ref() != input_schema {
                return Err(ColumnError::InvalidBatch {
                    reason: "HashAgg sink input schema does not match",
                }
                .into());
            }
            if sink.spec != *spec {
                return Err(InvalidQuery::new("HashAgg sink spec does not match", None).into());
            }
        }
        let Some(mut combined) = sinks.pop() else {
            return Self::new(spec.clone(), Arc::new(input_schema.clone()), ctx.clone());
        };
        if sinks.is_empty() {
            return Ok(combined);
        }

        let partial_schema = combined.partial_schema();
        while let Some(sink) = sinks.pop() {
            if !sink.spilled {
                sink.drain_in_memory_into(&mut combined)?;
            } else {
                let spill_files = sink.finish_into_spill_files()?;
                let mut reader_res = ctx.pool.register("hash_agg_combine_read", false);
                let mut max_frame_bytes = target_spill_frame_bytes(ctx);
                for file in spill_files {
                    let mut reader = file.open_reader(Arc::clone(&partial_schema))?;
                    loop {
                        if combined.table.num_groups() > 0 {
                            if let Err(ColumnError::MemoryLimit { .. }) =
                                reader_res.try_grow(max_frame_bytes)
                            {
                                reader_res.free();
                                combined.drain_table_to_writers(0)?;
                            } else {
                                reader_res.shrink(max_frame_bytes);
                            }
                        }
                        let Some(b) = reader.next_batch(Some(&mut reader_res))? else {
                            break;
                        };
                        max_frame_bytes = max_frame_bytes.max(b.memory_bytes());
                        combined.consume_partial_batch(&b)?;
                        drop(b);
                        reader_res.free();
                    }
                }
            }
        }
        Ok(combined)
    }

    /// Consumes this [`HashAgg`] sink and streams finalized groups as a [`PartitionStream`].
    pub fn into_stream(mut self) -> Result<Box<dyn PartitionStream>, QueryError> {
        let batch_size = self.ctx.batch_size.max(1);
        if let Some(states) = self.global_states.take() {
            return Ok(Box::new(HashAggStream {
                group_types: self.group_types,
                acc_kinds: self.acc_kinds,
                partial_schema: self.partial_schema,
                output_schema: self.output_schema,
                ctx: self.ctx,
                batch_size,
                active: Some(HashAggInMemoryRows {
                    keys: Vec::new(),
                    states: vec![Some(states)],
                    pos: 0,
                    res: self.res,
                    unspillable_res: self.unspillable_res,
                }),
                tasks: VecDeque::new(),
                done: false,
            }));
        }

        if !self.spilled {
            self.table.slots = Vec::new();
            self.table.hashes = Vec::new();
            let keys: Vec<Option<Vec<KeyAtom>>> = std::mem::take(&mut self.table.keys)
                .into_iter()
                .map(Some)
                .collect();
            let states: Vec<Option<Vec<AccState>>> = std::mem::take(&mut self.table.states)
                .into_iter()
                .map(Some)
                .collect();
            self.table.spillable_bytes = 0;
            self.table.unspillable_bytes = 0;
            let active = if states.is_empty() {
                self.res.free();
                self.unspillable_res.free();
                None
            } else {
                shrink_res_to_remaining(&keys, &states, &mut self.res, &mut self.unspillable_res);
                Some(HashAggInMemoryRows {
                    keys,
                    states,
                    pos: 0,
                    res: self.res,
                    unspillable_res: self.unspillable_res,
                })
            };
            return Ok(Box::new(HashAggStream {
                group_types: self.group_types,
                acc_kinds: self.acc_kinds,
                partial_schema: self.partial_schema,
                output_schema: self.output_schema,
                ctx: self.ctx,
                batch_size,
                active,
                tasks: VecDeque::new(),
                done: false,
            }));
        }

        if self.table.num_groups() > 0 {
            self.drain_table_to_writers(0)?;
        }
        let writers = std::mem::take(&mut self.partition_writers);
        let mut tasks = VecDeque::new();
        for w in writers.into_iter().flatten() {
            let f = w.finish()?;
            if f.byte_len() > 0 {
                tasks.push_back(HashAggSpillTask { file: f, depth: 0 });
            }
        }
        self.res.free();
        self.unspillable_res.free();

        Ok(Box::new(HashAggStream {
            group_types: self.group_types,
            acc_kinds: self.acc_kinds,
            partial_schema: self.partial_schema,
            output_schema: self.output_schema,
            ctx: self.ctx,
            batch_size,
            active: None,
            tasks,
            done: false,
        }))
    }

    /// Output schema of finalized batches produced by [`HashAgg::into_batches`].
    pub fn output_schema(&self) -> Arc<Schema> {
        Arc::clone(&self.output_schema)
    }

    /// Partial-state batch schema used for spilling and multi-worker combine.
    pub fn partial_schema(&self) -> Arc<Schema> {
        Arc::clone(&self.partial_schema)
    }

    /// Finalizes all buffered or spilled groups and emits output [`Batch`]es.
    ///
    /// For global aggregation (`group_by.is_empty()`), always emits 1 row even on 0 input rows.
    /// For spilled grouped aggregation, merges one partition at a time with salted re-partitioning
    /// up to depth 4 (`QueryError::TooLarge` on exceeding depth 4).
    pub fn into_batches(mut self) -> Result<Vec<Batch>, QueryError> {
        if let Some(states) = self.global_states.take() {
            let batch = build_output_batch(
                Arc::clone(&self.output_schema),
                &self.group_types,
                &self.acc_kinds,
                &[],
                &[states],
                &self.ctx.eval,
                &mut self.res,
            )?;
            self.res.free();
            self.unspillable_res.free();
            return Ok(vec![batch]);
        }

        if !self.spilled {
            let keys = std::mem::take(&mut self.table.keys);
            let states = std::mem::take(&mut self.table.states);
            self.table
                .clear_and_free(&mut self.res, &mut self.unspillable_res);
            if keys.is_empty() {
                return Ok(Vec::new());
            }
            let layout = AggLayout {
                group_types: &self.group_types,
                acc_kinds: &self.acc_kinds,
                partial_schema: &self.partial_schema,
                output_schema: &self.output_schema,
            };
            return emit_chunked_output_batches(
                &layout,
                &keys,
                &states,
                self.ctx.batch_size.max(1),
                &self.ctx.eval,
                &mut self.res,
            );
        }

        // Spilled path: flush any leftover in-memory rows, close writers, and merge each partition.
        if self.table.num_groups() > 0 {
            self.drain_table_to_writers(0)?;
        }
        let writers = std::mem::take(&mut self.partition_writers);
        let mut files: Vec<SpillFile> = Vec::with_capacity(writers.len());
        for w in writers.into_iter().flatten() {
            let f = w.finish()?;
            if f.byte_len() > 0 {
                files.push(f);
            }
        }
        self.res.free();
        self.unspillable_res.free();

        let mut out = Vec::new();
        for file in files {
            self.merge_spill_partition(file, 0, &mut out)?;
        }
        Ok(out)
    }

    fn merge_spill_partition(
        &mut self,
        file: SpillFile,
        depth: usize,
        out: &mut Vec<Batch>,
    ) -> Result<(), QueryError> {
        let layout = AggLayout {
            group_types: &self.group_types,
            acc_kinds: &self.acc_kinds,
            partial_schema: &self.partial_schema,
            output_schema: &self.output_schema,
        };
        match fold_spill_partition_or_repartition(&layout, &self.ctx, file, depth)? {
            FoldPartitionResult::Empty => Ok(()),
            FoldPartitionResult::Ready(mut mem) => {
                let keys: Vec<Vec<KeyAtom>> = mem.keys.into_iter().flatten().collect();
                let states: Vec<Vec<AccState>> = mem.states.into_iter().flatten().collect();
                mem.unspillable_res.free();
                mem.res.free();
                let batches = emit_chunked_output_batches(
                    &layout,
                    &keys,
                    &states,
                    self.ctx.batch_size.max(1),
                    &self.ctx.eval,
                    &mut mem.res,
                )?;
                out.extend(batches);
                Ok(())
            }
            FoldPartitionResult::Repartitioned(sub_tasks) => {
                for task in sub_tasks {
                    self.merge_spill_partition(task.file, task.depth, out)?;
                }
                Ok(())
            }
        }
    }

    fn ensure_partition_writers(&mut self) {
        if self.partition_writers.is_empty() {
            self.partition_writers = (0..NUM_SPILL_PARTITIONS).map(|_| None).collect();
        }
    }

    fn drain_table_to_writers(&mut self, salt: u64) -> Result<(), QueryError> {
        self.ensure_partition_writers();
        let frame_cap = target_spill_frame_bytes(&self.ctx);
        let layout = AggLayout {
            group_types: &self.group_types,
            acc_kinds: &self.acc_kinds,
            partial_schema: &self.partial_schema,
            output_schema: &self.output_schema,
        };
        let mut group_state = GroupTableState {
            table: &mut self.table,
            res: &mut self.res,
            unspillable_res: &mut self.unspillable_res,
        };
        drain_group_table_to_writers(
            &layout,
            &mut group_state,
            salt,
            frame_cap,
            &self.ctx,
            &mut self.partition_writers,
        )?;
        self.spilled = true;
        Ok(())
    }

    fn consume_partial_batch(&mut self, batch: &Batch) -> Result<(), QueryError> {
        if batch.is_empty() {
            return Ok(());
        }
        if let Some(states) = self.global_states.as_mut() {
            let cols: Vec<&Column> = batch.columns().iter().collect();
            let num_rows = batch.num_rows();
            for row in 0..num_rows {
                let mut offset = 0usize;
                for (st, kind) in states.iter_mut().zip(&self.acc_kinds) {
                    let width = kind.partial_col_count();
                    let (d_sp, d_unsp) =
                        st.estimate_merge_growth(&cols[offset..offset + width], row);
                    if d_sp > 0 {
                        self.res.try_grow(d_sp)?;
                    }
                    if d_unsp > 0 {
                        self.unspillable_res.try_grow(d_unsp)?;
                    }
                    st.merge_row(kind, &cols[offset..offset + width], row)?;
                    offset += width;
                }
            }
            let target_sp: usize = states.iter().map(AccState::spillable_heap_bytes).sum();
            let target_unsp: usize = states.iter().map(AccState::unspillable_heap_bytes).sum();
            self.res.resize(target_sp)?;
            self.unspillable_res.resize(target_unsp)?;
            return Ok(());
        }

        let mut start_row = 0usize;
        if !self.spilled {
            let layout = AggLayout {
                group_types: &self.group_types,
                acc_kinds: &self.acc_kinds,
                partial_schema: &self.partial_schema,
                output_schema: &self.output_schema,
            };
            let mut group_state = GroupTableState {
                table: &mut self.table,
                res: &mut self.res,
                unspillable_res: &mut self.unspillable_res,
            };
            match fold_partial_batch_into_table(
                batch,
                &layout,
                &mut group_state,
                0,
                0,
                &self.ctx.eval,
            )? {
                None => return Ok(()),
                Some(failed_row) => {
                    self.drain_table_to_writers(0)?;
                    start_row = failed_row;
                }
            }
        }

        // Already spilled: route remaining unfolded rows directly into the 32 partial spill writers.
        self.ensure_partition_writers();
        let frame_cap = target_spill_frame_bytes(&self.ctx);
        route_partial_batch_to_writers(
            batch,
            start_row,
            self.group_types.len(),
            0,
            frame_cap,
            &self.ctx,
            &mut self.partition_writers,
        )
    }

    fn drain_in_memory_into(mut self, combined: &mut HashAgg) -> Result<(), QueryError> {
        let mut scratch = self.ctx.pool.register("hash_agg_combine_drain", false);
        if let Some(states) = self.global_states.take() {
            let batch = build_partial_batch(
                Arc::clone(&self.partial_schema),
                &self.group_types,
                &self.acc_kinds,
                &[],
                &[states],
                &self.ctx.eval,
                &mut scratch,
            )?;
            self.res.free();
            self.unspillable_res.free();
            combined.consume_partial_batch(&batch)?;
            drop(batch);
            scratch.free();
            return Ok(());
        }

        self.table.slots = Vec::new();
        self.table.hashes = Vec::new();
        let mut keys: Vec<Option<Vec<KeyAtom>>> = std::mem::take(&mut self.table.keys)
            .into_iter()
            .map(Some)
            .collect();
        let mut states: Vec<Option<Vec<AccState>>> = std::mem::take(&mut self.table.states)
            .into_iter()
            .map(Some)
            .collect();
        self.table.spillable_bytes = 0;
        self.table.unspillable_bytes = 0;
        shrink_res_to_remaining(&keys, &states, &mut self.res, &mut self.unspillable_res);

        if keys.is_empty() {
            self.res.free();
            self.unspillable_res.free();
            return Ok(());
        }

        let frame_cap = target_spill_frame_bytes(&self.ctx);
        let max_rows = self.ctx.batch_size.max(1);
        let mut pos = 0usize;
        while pos < keys.len() {
            let mut chunk_end = pos;
            let mut est_bytes = 0usize;
            while chunk_end < keys.len() && chunk_end - pos < max_rows {
                if let (Some(k), Some(s)) = (&keys[chunk_end], &states[chunk_end]) {
                    let row_est = estimate_partial_group_bytes(k, s);
                    if chunk_end > pos && est_bytes.saturating_add(row_est) > frame_cap {
                        break;
                    }
                    est_bytes = est_bytes.saturating_add(row_est);
                }
                chunk_end += 1;
            }
            let mut chunk_keys = Vec::with_capacity(chunk_end - pos);
            let mut chunk_states = Vec::with_capacity(chunk_end - pos);
            for gi in pos..chunk_end {
                if let (Some(k), Some(s)) = (keys[gi].take(), states[gi].take()) {
                    chunk_keys.push(k);
                    chunk_states.push(s);
                }
            }
            let (chunk_sp, chunk_unsp) = chunk_owned_bytes(&chunk_keys, &chunk_states);
            let batch = match build_partial_batch(
                Arc::clone(&self.partial_schema),
                &self.group_types,
                &self.acc_kinds,
                &chunk_keys,
                &chunk_states,
                &self.ctx.eval,
                &mut scratch,
            ) {
                Ok(b) => b,
                Err(QueryError::TooLarge)
                    if !combined.spilled && combined.table.num_groups() > 0 =>
                {
                    scratch.free();
                    combined.drain_table_to_writers(0)?;
                    build_partial_batch(
                        Arc::clone(&self.partial_schema),
                        &self.group_types,
                        &self.acc_kinds,
                        &chunk_keys,
                        &chunk_states,
                        &self.ctx.eval,
                        &mut scratch,
                    )?
                }
                Err(err) => {
                    scratch.free();
                    return Err(err);
                }
            };
            drop(chunk_keys);
            drop(chunk_states);
            self.res.shrink(chunk_sp);
            self.unspillable_res.shrink(chunk_unsp);
            combined.consume_partial_batch(&batch)?;
            drop(batch);
            scratch.free();
            pos = chunk_end;
        }
        drop(keys);
        drop(states);
        self.res.free();
        self.unspillable_res.free();
        Ok(())
    }

    fn finish_into_spill_files(mut self) -> Result<Vec<SpillFile>, QueryError> {
        if self.table.num_groups() > 0 {
            self.drain_table_to_writers(0)?;
        }
        let writers = std::mem::take(&mut self.partition_writers);
        self.res.free();
        self.unspillable_res.free();
        let mut files = Vec::new();
        for w in writers.into_iter().flatten() {
            let f = w.finish()?;
            if f.byte_len() > 0 {
                files.push(f);
            }
        }
        Ok(files)
    }
}

impl PushSink for HashAgg {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        if self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if batch.schema().as_ref() != self.input_schema.as_ref() {
            return Err(ColumnError::InvalidBatch {
                reason: "batch schema does not match HashAgg input schema",
            }
            .into());
        }
        if batch.is_empty() {
            return Ok(ControlFlow::Continue);
        }

        // Compact sparse batches upfront per Cross-cutting invariant #7.
        let batch = if batch.num_rows().saturating_mul(4) < batch.raw_len() {
            let mut compact_res = self.ctx.pool.register("hash_agg_compact", false);
            match batch.compact(Some(&mut compact_res)) {
                Ok(compacted) => {
                    compact_res.free();
                    compacted
                }
                Err(ColumnError::MemoryLimit { .. }) if self.table.num_groups() > 0 => {
                    compact_res.free();
                    self.drain_table_to_writers(0)?;
                    batch
                }
                Err(ColumnError::MemoryLimit { .. }) => {
                    compact_res.free();
                    batch
                }
                Err(err) => return Err(err.into()),
            }
        } else {
            batch
        };

        let mut eval_res = self.ctx.pool.register("hash_agg_eval", false);
        let evaled = match evaluate_batch_inputs(&batch, &self.spec, &self.ctx.eval, &mut eval_res)
        {
            Ok(ev) => ev,
            Err(QueryError::TooLarge)
                if !self.spec.group_by.is_empty() && self.table.num_groups() > 0 =>
            {
                eval_res.free();
                self.drain_table_to_writers(0)?;
                evaluate_batch_inputs(&batch, &self.spec, &self.ctx.eval, &mut eval_res)?
            }
            Err(err) => {
                eval_res.free();
                return Err(err);
            }
        };

        let active_rows = collect_active_rows(&batch);
        let group_cols: Vec<&Column> = evaled
            .group_cols
            .iter()
            .map(|c| resolve_eval_col(&batch, c))
            .collect();
        let agg_inputs: Vec<Vec<&Column>> = evaled
            .agg_inputs
            .iter()
            .map(|cols| cols.iter().map(|c| resolve_eval_col(&batch, c)).collect())
            .collect();
        let filter_masks = &evaled.filter_masks;

        if let Some(states) = self.global_states.as_mut() {
            for (idx, &row) in active_rows.iter().enumerate() {
                if (idx & CANCEL_CHECK_MASK) == 0 && self.ctx.eval.cancel.is_cancelled() {
                    eval_res.free();
                    return Err(QueryError::Cancelled);
                }
                // Pre-reserve worst-case growth for this row across all aggregates.
                let mut need_sp = 0usize;
                let mut need_unsp = 0usize;
                for (agg_idx, kind) in self.acc_kinds.iter().enumerate() {
                    if let Some(mask) = &filter_masks[agg_idx]
                        && !mask.get(row)
                    {
                        continue;
                    }
                    let (ds, du) =
                        states[agg_idx].estimate_fold_growth(kind, &agg_inputs[agg_idx], row);
                    need_sp = need_sp.saturating_add(ds);
                    need_unsp = need_unsp.saturating_add(du);
                }
                let cur_sp: usize = states.iter().map(AccState::spillable_heap_bytes).sum();
                let cur_unsp: usize = states.iter().map(AccState::unspillable_heap_bytes).sum();
                if cur_sp.saturating_add(need_sp) > self.res.size() {
                    let add = cur_sp.saturating_add(need_sp) - self.res.size();
                    if let Err(err) = self.res.try_grow(add) {
                        eval_res.free();
                        return Err(err.into());
                    }
                }
                if cur_unsp.saturating_add(need_unsp) > self.unspillable_res.size() {
                    let add = cur_unsp.saturating_add(need_unsp) - self.unspillable_res.size();
                    if let Err(err) = self.unspillable_res.try_grow(add) {
                        eval_res.free();
                        return Err(err.into());
                    }
                }

                for (agg_idx, kind) in self.acc_kinds.iter().enumerate() {
                    if let Some(mask) = &filter_masks[agg_idx]
                        && !mask.get(row)
                    {
                        continue;
                    }
                    states[agg_idx].fold_row(kind, &agg_inputs[agg_idx], row)?;
                }
            }
            let cur_sp: usize = states.iter().map(AccState::spillable_heap_bytes).sum();
            let cur_unsp: usize = states.iter().map(AccState::unspillable_heap_bytes).sum();
            self.res.resize(cur_sp)?;
            self.unspillable_res.resize(cur_unsp)?;
            eval_res.free();
            return Ok(ControlFlow::Continue);
        }

        // Grouped aggregation: fold rows into `self.table`, spilling on MemoryLimit when `buffered_rows > 0`.
        let layout = AggLayout {
            group_types: &self.group_types,
            acc_kinds: &self.acc_kinds,
            partial_schema: &self.partial_schema,
            output_schema: &self.output_schema,
        };
        let inputs = GroupedBatchInputs {
            group_cols: &group_cols,
            agg_inputs: &agg_inputs,
            filter_masks,
        };
        for (idx, &row) in active_rows.iter().enumerate() {
            if (idx & CANCEL_CHECK_MASK) == 0 && self.ctx.eval.cancel.is_cancelled() {
                eval_res.free();
                return Err(QueryError::Cancelled);
            }
            let mut group_state = GroupTableState {
                table: &mut self.table,
                res: &mut self.res,
                unspillable_res: &mut self.unspillable_res,
            };
            let mut spill_sink = SpillPartitionSink {
                spilled: &mut self.spilled,
                writers: &mut self.partition_writers,
            };
            let res = fold_grouped_row_with_spill(
                row,
                &inputs,
                &layout,
                &mut group_state,
                &mut spill_sink,
                &self.ctx,
            );
            if let Err(err) = res {
                eval_res.free();
                return Err(err);
            }
        }

        drop(group_cols);
        drop(agg_inputs);
        drop(evaled);
        drop(batch);
        eval_res.free();

        // Keep reservation synced cleanly, and if already spilled, drain `table` to release 100% of HashMap RAM.
        if self.spilled && self.table.num_groups() > 0 {
            self.drain_table_to_writers(0)?;
        } else {
            match self.res.resize(self.table.spillable_bytes) {
                Ok(()) => {}
                Err(ColumnError::MemoryLimit { .. }) if self.table.num_groups() > 0 => {
                    self.drain_table_to_writers(0)?;
                    return Ok(ControlFlow::Continue);
                }
                Err(err) => return Err(err.into()),
            }
            self.unspillable_res.resize(self.table.unspillable_bytes)?;
        }

        Ok(ControlFlow::Continue)
    }

    fn finish(&mut self) -> Result<(), QueryError> {
        if self.spilled && self.table.num_groups() > 0 {
            self.drain_table_to_writers(0)?;
        }
        Ok(())
    }
}

/// Runs `source` across parallel partitions into per-worker [`HashAgg`] sinks and combines
/// their partial states cleanly.
pub fn execute_hash_agg(
    source: &dyn ScanSource,
    scan_spec: &ScanSpec,
    agg_spec: HashAggSpec,
    ctx: &ExecContext,
) -> Result<Vec<Batch>, QueryError> {
    let partitions = source.open_partitions(scan_spec, ctx)?;
    let input_schema = source.projected_schema(scan_spec)?;

    if partitions.is_empty() {
        let agg = HashAgg::new(agg_spec, input_schema, ctx.clone())?;
        return agg.into_batches();
    }

    let sinks = run_pipeline(ctx, partitions, |_| Ok(Vec::new()), {
        let spec_c = agg_spec.clone();
        let schema_c = Arc::clone(&input_schema);
        let ctx_c = ctx.clone();
        move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_c.clone())
    })?;

    HashAgg::merge_sinks(sinks, &agg_spec, &input_schema, ctx)
}

/// Runs arbitrary input `partitions` in parallel into per-worker [`HashAgg`] sinks and returns a
/// streaming [`PartitionStream`] of finalized groups.
pub fn execute_hash_agg_streams(
    partitions: Vec<Box<dyn PartitionStream>>,
    input_schema: Arc<Schema>,
    spec: HashAggSpec,
    ctx: &ExecContext,
) -> Result<Box<dyn PartitionStream>, QueryError> {
    if partitions.is_empty() {
        let agg = HashAgg::new(spec, input_schema, ctx.clone())?;
        return agg.into_stream();
    }

    let sinks = run_pipeline(ctx, partitions, |_| Ok(Vec::new()), {
        let spec_c = spec.clone();
        let schema_c = Arc::clone(&input_schema);
        let ctx_c = ctx.clone();
        move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_c.clone())
    })?;

    HashAgg::combine_sinks_to_stream(sinks, &spec, &input_schema, ctx)
}

// ---------------------------------------------------------------------------
// Input evaluation & active row folding helpers
// ---------------------------------------------------------------------------

enum EvalCol {
    Borrowed(usize),
    Owned(Column),
}

struct EvaluatedBatchInputs {
    group_cols: Vec<EvalCol>,
    agg_inputs: Vec<Vec<EvalCol>>,
    filter_masks: Vec<Option<Bitmap>>,
}

fn resolve_eval_col<'a>(batch: &'a Batch, col: &'a EvalCol) -> &'a Column {
    match col {
        EvalCol::Borrowed(idx) => &batch.columns()[*idx],
        EvalCol::Owned(c) => c,
    }
}

fn eval_one_input(
    expr: &Expr,
    batch: &Batch,
    active_mask: Option<&Bitmap>,
    eval_ctx: &EvalContext,
    eval_res: &mut Reservation,
) -> Result<EvalCol, QueryError> {
    if let Expr::Column { index, .. } = expr
        && *index < batch.columns().len()
    {
        return Ok(EvalCol::Borrowed(*index));
    }
    let col = match active_mask {
        Some(m) => eval::eval_expr_masked(expr, batch, Some(m), eval_ctx, Some(eval_res))?,
        None => eval_expr(expr, batch, eval_ctx, Some(eval_res))?,
    };
    Ok(EvalCol::Owned(col))
}

fn evaluate_batch_inputs(
    batch: &Batch,
    spec: &HashAggSpec,
    eval_ctx: &EvalContext,
    eval_res: &mut Reservation,
) -> Result<EvaluatedBatchInputs, QueryError> {
    let mut group_cols = Vec::with_capacity(spec.group_by.len());
    for (_, expr) in &spec.group_by {
        group_cols.push(eval_one_input(expr, batch, None, eval_ctx, eval_res)?);
    }

    let mut filter_masks = Vec::with_capacity(spec.aggs.len());
    let mut agg_inputs = Vec::with_capacity(spec.aggs.len());

    for agg in &spec.aggs {
        let mask = match &agg.filter {
            Some(f) => Some(eval_filter(f, batch, eval_ctx, Some(eval_res))?),
            None => None,
        };
        let active_mask = mask.as_ref().filter(|m| !m.all_ones());

        let mut cols = Vec::new();
        match &agg.func {
            AggFunc::CountStar => {}
            AggFunc::Count(e)
            | AggFunc::CountDistinct(e)
            | AggFunc::Sum(e)
            | AggFunc::SumCostDecimal(e)
            | AggFunc::Avg(e)
            | AggFunc::Min(e)
            | AggFunc::Max(e)
            | AggFunc::BoolOr(e)
            | AggFunc::BoolAnd(e)
            | AggFunc::ApproxPercentileCont { expr: e, .. }
            | AggFunc::PercentileCont { expr: e, .. }
            | AggFunc::ArrayAgg { expr: e, .. }
            | AggFunc::TagUnion(e) => {
                cols.push(eval_one_input(e, batch, active_mask, eval_ctx, eval_res)?);
            }
            AggFunc::FirstNonNullByOrder { value, order_by }
            | AggFunc::FirstByOrder { value, order_by } => {
                cols.push(eval_one_input(
                    value,
                    batch,
                    active_mask,
                    eval_ctx,
                    eval_res,
                )?);
                for k in order_by {
                    cols.push(eval_one_input(
                        &k.expr,
                        batch,
                        active_mask,
                        eval_ctx,
                        eval_res,
                    )?);
                }
            }
        }
        filter_masks.push(mask);
        agg_inputs.push(cols);
    }

    Ok(EvaluatedBatchInputs {
        group_cols,
        agg_inputs,
        filter_masks,
    })
}

fn collect_active_rows(batch: &Batch) -> Vec<usize> {
    let raw_len = batch.raw_len();
    match batch.selection() {
        Selection::All => (0..raw_len).collect(),
        Selection::Mask(bm) => bm.iter_ones().collect(),
        Selection::Indices(idx) => idx.iter().map(|&i| i as usize).collect(),
    }
}

struct AggLayout<'a> {
    group_types: &'a [DataType],
    acc_kinds: &'a [AccKind],
    partial_schema: &'a Arc<Schema>,
    output_schema: &'a Arc<Schema>,
}

struct GroupTableState<'a> {
    table: &'a mut GroupTable,
    res: &'a mut Reservation,
    unspillable_res: &'a mut Reservation,
}

struct GroupedBatchInputs<'a> {
    group_cols: &'a [&'a Column],
    agg_inputs: &'a [Vec<&'a Column>],
    filter_masks: &'a [Option<Bitmap>],
}

struct SpillPartitionSink<'a> {
    spilled: &'a mut bool,
    writers: &'a mut Vec<Option<SpillWriter>>,
}

fn fold_grouped_row_with_spill(
    row: usize,
    inputs: &GroupedBatchInputs<'_>,
    layout: &AggLayout<'_>,
    state: &mut GroupTableState<'_>,
    spill_sink: &mut SpillPartitionSink<'_>,
    ctx: &ExecContext,
) -> Result<(), QueryError> {
    let hash = hash_cols_row(inputs.group_cols, row, 0);
    for attempt in 0..2 {
        let existing_gi = state.table.lookup_by_cols(hash, inputs.group_cols, row);
        let mut need_sp = 0usize;
        let mut need_unsp = 0usize;
        if existing_gi.is_none() {
            let key_heap: usize = inputs
                .group_cols
                .iter()
                .map(|c| col_row_heap_estimate(c, row))
                .sum::<usize>()
                + inputs.group_cols.len() * size_of::<KeyAtom>();
            need_sp = need_sp.saturating_add(state.table.new_group_overhead(key_heap));
        }
        for (agg_idx, kind) in layout.acc_kinds.iter().enumerate() {
            if let Some(mask) = &inputs.filter_masks[agg_idx]
                && !mask.get(row)
            {
                continue;
            }
            let (ds, du) = match existing_gi {
                Some(gi) => state.table.states[gi][agg_idx].estimate_fold_growth(
                    kind,
                    &inputs.agg_inputs[agg_idx],
                    row,
                ),
                None => {
                    kind.make_state()
                        .estimate_fold_growth(kind, &inputs.agg_inputs[agg_idx], row)
                }
            };
            need_sp = need_sp.saturating_add(ds);
            need_unsp = need_unsp.saturating_add(du);
        }

        let sp_needed_total = state.table.spillable_bytes.saturating_add(need_sp);
        let unsp_needed_total = state.table.unspillable_bytes.saturating_add(need_unsp);

        let sp_ok = if sp_needed_total > state.res.size() {
            state.res.try_grow(sp_needed_total - state.res.size())
        } else {
            Ok(())
        };
        let unsp_ok = if sp_ok.is_ok() && unsp_needed_total > state.unspillable_res.size() {
            state
                .unspillable_res
                .try_grow(unsp_needed_total - state.unspillable_res.size())
        } else {
            Ok(())
        };

        let combined_res = sp_ok.and(unsp_ok);
        if let Err(ColumnError::MemoryLimit { .. }) = combined_res {
            let _ = state.res.resize(state.table.spillable_bytes);
            let _ = state.unspillable_res.resize(state.table.unspillable_bytes);
            if attempt == 0 && state.table.num_groups() > 0 {
                if spill_sink.writers.is_empty() {
                    *spill_sink.writers = (0..NUM_SPILL_PARTITIONS).map(|_| None).collect();
                }
                let frame_cap = target_spill_frame_bytes(ctx);
                drain_group_table_to_writers(layout, state, 0, frame_cap, ctx, spill_sink.writers)?;
                *spill_sink.spilled = true;
                continue;
            }
            return Err(QueryError::TooLarge);
        } else if let Err(other) = combined_res {
            return Err(other.into());
        }

        let gi = match existing_gi {
            Some(gi) => gi,
            None => {
                let key: Vec<KeyAtom> = inputs
                    .group_cols
                    .iter()
                    .map(|c| read_key_atom(c, row))
                    .collect();
                state.table.insert_group(hash, key)
            }
        };

        let mut delta_sp_before = 0usize;
        let mut delta_unsp_before = 0usize;
        for (agg_idx, kind) in layout.acc_kinds.iter().enumerate() {
            if let Some(mask) = &inputs.filter_masks[agg_idx]
                && !mask.get(row)
            {
                continue;
            }
            let st = &mut state.table.states[gi][agg_idx];
            delta_sp_before += st.spillable_heap_bytes();
            delta_unsp_before += st.unspillable_heap_bytes();
            st.fold_row(kind, &inputs.agg_inputs[agg_idx], row)?;
            state.table.spillable_bytes = state
                .table
                .spillable_bytes
                .saturating_sub(delta_sp_before)
                .saturating_add(st.spillable_heap_bytes());
            state.table.unspillable_bytes = state
                .table
                .unspillable_bytes
                .saturating_sub(delta_unsp_before)
                .saturating_add(st.unspillable_heap_bytes());
            delta_sp_before = 0;
            delta_unsp_before = 0;
        }
        return Ok(());
    }
    Err(QueryError::TooLarge)
}

fn fold_partial_batch_into_table(
    batch: &Batch,
    layout: &AggLayout<'_>,
    state: &mut GroupTableState<'_>,
    salt: u64,
    headroom: usize,
    eval_ctx: &EvalContext,
) -> Result<Option<usize>, QueryError> {
    let num_group_cols = layout.group_types.len();
    let cols: Vec<&Column> = batch.columns().iter().collect();
    let group_cols = &cols[..num_group_cols];
    let num_rows = batch.num_rows();

    for row in 0..num_rows {
        if (row & CANCEL_CHECK_MASK) == 0 && eval_ctx.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        let hash = hash_cols_row(group_cols, row, salt);
        let existing_gi = state.table.lookup_by_cols(hash, group_cols, row);

        let mut need_sp = 0usize;
        let mut need_unsp = 0usize;
        if existing_gi.is_none() {
            let key_heap: usize = group_cols
                .iter()
                .map(|c| col_row_heap_estimate(c, row))
                .sum::<usize>()
                + group_cols.len() * size_of::<KeyAtom>();
            need_sp = need_sp.saturating_add(state.table.new_group_overhead(key_heap));
        }

        let mut offset = num_group_cols;
        for (agg_idx, kind) in layout.acc_kinds.iter().enumerate() {
            let width = kind.partial_col_count();
            let slice = &cols[offset..offset + width];
            let (ds, du) = match existing_gi {
                Some(gi) => state.table.states[gi][agg_idx].estimate_merge_growth(slice, row),
                None => kind.make_state().estimate_merge_growth(slice, row),
            };
            need_sp = need_sp.saturating_add(ds);
            need_unsp = need_unsp.saturating_add(du);
            offset += width;
        }

        let sp_needed = state.table.spillable_bytes.saturating_add(need_sp);
        let unsp_needed = state.table.unspillable_bytes.saturating_add(need_unsp);
        let eff_headroom = if state.table.num_groups() > 0 {
            headroom
        } else {
            0
        };
        let before_sp = state.res.size();
        let sp_with_headroom = sp_needed.saturating_add(eff_headroom);
        let sp_ok = if sp_with_headroom > before_sp {
            match state.res.try_grow(sp_with_headroom - before_sp) {
                Ok(()) => {
                    let keep = sp_needed.max(before_sp);
                    if state.res.size() > keep {
                        state.res.shrink(state.res.size() - keep);
                    }
                    Ok(())
                }
                Err(err) => Err(err),
            }
        } else {
            Ok(())
        };
        let unsp_ok = if sp_ok.is_ok() && unsp_needed > state.unspillable_res.size() {
            state
                .unspillable_res
                .try_grow(unsp_needed - state.unspillable_res.size())
        } else {
            Ok(())
        };
        let combined = sp_ok.and(unsp_ok);
        if let Err(ColumnError::MemoryLimit { .. }) = combined {
            let _ = state.res.resize(state.table.spillable_bytes);
            let _ = state.unspillable_res.resize(state.table.unspillable_bytes);
            if state.table.num_groups() > 0 {
                return Ok(Some(row));
            }
            return Err(QueryError::TooLarge);
        } else if let Err(other) = combined {
            return Err(other.into());
        }

        let gi = match existing_gi {
            Some(gi) => gi,
            None => {
                let key: Vec<KeyAtom> = group_cols.iter().map(|c| read_key_atom(c, row)).collect();
                state.table.insert_group(hash, key)
            }
        };

        let mut offset = num_group_cols;
        for (agg_idx, kind) in layout.acc_kinds.iter().enumerate() {
            let width = kind.partial_col_count();
            let st = &mut state.table.states[gi][agg_idx];
            let sp_before = st.spillable_heap_bytes();
            let unsp_before = st.unspillable_heap_bytes();
            st.merge_row(kind, &cols[offset..offset + width], row)?;
            state.table.spillable_bytes = state
                .table
                .spillable_bytes
                .saturating_sub(sp_before)
                .saturating_add(st.spillable_heap_bytes());
            state.table.unspillable_bytes = state
                .table
                .unspillable_bytes
                .saturating_sub(unsp_before)
                .saturating_add(st.unspillable_heap_bytes());
            offset += width;
        }
    }

    match state.res.resize(state.table.spillable_bytes) {
        Ok(()) => {}
        Err(ColumnError::MemoryLimit { .. }) if state.table.num_groups() > 0 => {
            return Ok(Some(num_rows));
        }
        Err(err) => return Err(err.into()),
    }
    if headroom > 0 && state.table.num_groups() > 0 {
        match state.res.try_grow(headroom) {
            Ok(()) => state.res.shrink(headroom),
            Err(ColumnError::MemoryLimit { .. }) => return Ok(Some(num_rows)),
            Err(err) => return Err(err.into()),
        }
    }
    state
        .unspillable_res
        .resize(state.table.unspillable_bytes)?;
    Ok(None)
}

// ---------------------------------------------------------------------------
// Spill partitioning & partial/output batch construction
// ---------------------------------------------------------------------------

fn target_spill_frame_bytes(ctx: &ExecContext) -> usize {
    (ctx.pool.spill_share() / 4).max(MIN_SPILL_FRAME_BYTES)
}

fn chunk_owned_bytes(keys: &[Vec<KeyAtom>], states: &[Vec<AccState>]) -> (usize, usize) {
    let mut sp = 0usize;
    let mut unsp = 0usize;
    for k in keys {
        sp = sp
            .saturating_add(k.capacity() * size_of::<KeyAtom>())
            .saturating_add(k.iter().map(KeyAtom::heap_bytes).sum::<usize>());
    }
    for s in states {
        sp = sp
            .saturating_add(s.capacity() * size_of::<AccState>())
            .saturating_add(s.iter().map(AccState::spillable_heap_bytes).sum::<usize>());
        unsp = unsp.saturating_add(
            s.iter()
                .map(AccState::unspillable_heap_bytes)
                .sum::<usize>(),
        );
    }
    (sp, unsp)
}

fn shrink_res_to_remaining(
    keys: &[Option<Vec<KeyAtom>>],
    states: &[Option<Vec<AccState>>],
    res: &mut Reservation,
    unspillable_res: &mut Reservation,
) {
    let mut rem_sp = keys.len().saturating_mul(size_of::<Option<Vec<KeyAtom>>>())
        + states
            .len()
            .saturating_mul(size_of::<Option<Vec<AccState>>>());
    let mut rem_unsp = 0usize;
    for k in keys.iter().flatten() {
        rem_sp = rem_sp
            .saturating_add(k.capacity() * size_of::<KeyAtom>())
            .saturating_add(k.iter().map(KeyAtom::heap_bytes).sum::<usize>());
    }
    for s in states.iter().flatten() {
        rem_sp = rem_sp
            .saturating_add(s.capacity() * size_of::<AccState>())
            .saturating_add(s.iter().map(AccState::spillable_heap_bytes).sum::<usize>());
        rem_unsp = rem_unsp.saturating_add(
            s.iter()
                .map(AccState::unspillable_heap_bytes)
                .sum::<usize>(),
        );
    }
    if res.size() > rem_sp {
        res.shrink(res.size() - rem_sp);
    }
    if unspillable_res.size() > rem_unsp {
        unspillable_res.shrink(unspillable_res.size() - rem_unsp);
    }
}

struct PartialChunkCtx<'a> {
    writer: &'a mut SpillWriter,
    frame_cap_bytes: usize,
    eval_ctx: &'a EvalContext,
    scratch: &'a mut Reservation,
}

fn write_partial_chunk(
    layout: &AggLayout<'_>,
    chunk_ctx: &mut PartialChunkCtx<'_>,
    mut chunk_keys: Vec<Vec<KeyAtom>>,
    mut chunk_states: Vec<Vec<AccState>>,
    state: &mut GroupTableState<'_>,
) -> Result<(), QueryError> {
    let n = chunk_keys.len();
    if n == 0 {
        return Ok(());
    }
    let batch = match build_partial_batch(
        Arc::clone(layout.partial_schema),
        layout.group_types,
        layout.acc_kinds,
        &chunk_keys,
        &chunk_states,
        chunk_ctx.eval_ctx,
        chunk_ctx.scratch,
    ) {
        Ok(b) => b,
        Err(QueryError::TooLarge) if n > 1 => {
            chunk_ctx.scratch.free();
            let half = n / 2;
            let right_keys = chunk_keys.split_off(half);
            let right_states = chunk_states.split_off(half);
            write_partial_chunk(layout, chunk_ctx, chunk_keys, chunk_states, state)?;
            return write_partial_chunk(layout, chunk_ctx, right_keys, right_states, state);
        }
        Err(err) => {
            chunk_ctx.scratch.free();
            return Err(err);
        }
    };
    if batch.memory_bytes() > chunk_ctx.frame_cap_bytes && n > 1 {
        drop(batch);
        chunk_ctx.scratch.free();
        let half = n / 2;
        let right_keys = chunk_keys.split_off(half);
        let right_states = chunk_states.split_off(half);
        write_partial_chunk(layout, chunk_ctx, chunk_keys, chunk_states, state)?;
        return write_partial_chunk(layout, chunk_ctx, right_keys, right_states, state);
    }
    let (chunk_sp, chunk_unsp) = chunk_owned_bytes(&chunk_keys, &chunk_states);
    drop(chunk_keys);
    drop(chunk_states);
    state.res.shrink(chunk_sp);
    state.unspillable_res.shrink(chunk_unsp);
    chunk_ctx.writer.write_batch(&batch)?;
    drop(batch);
    chunk_ctx.scratch.free();
    Ok(())
}

fn drain_group_table_to_writers(
    layout: &AggLayout<'_>,
    state: &mut GroupTableState<'_>,
    salt: u64,
    frame_cap_bytes: usize,
    ctx: &ExecContext,
    writers: &mut [Option<SpillWriter>],
) -> Result<(), QueryError> {
    state.table.slots = Vec::new();
    state.table.hashes = Vec::new();
    let mut keys: Vec<Option<Vec<KeyAtom>>> = std::mem::take(&mut state.table.keys)
        .into_iter()
        .map(Some)
        .collect();
    let mut states: Vec<Option<Vec<AccState>>> = std::mem::take(&mut state.table.states)
        .into_iter()
        .map(Some)
        .collect();
    state.table.spillable_bytes = 0;
    state.table.unspillable_bytes = 0;
    shrink_res_to_remaining(&keys, &states, state.res, state.unspillable_res);

    let mut buckets: Vec<Vec<usize>> = (0..NUM_SPILL_PARTITIONS).map(|_| Vec::new()).collect();
    for (gi, key) in keys.iter().enumerate() {
        if let Some(k) = key {
            let h = hash_atoms(k, salt);
            let p = (h as usize) & (NUM_SPILL_PARTITIONS - 1);
            buckets[p].push(gi);
        }
    }

    let mut scratch = ctx.pool.register("hash_agg_spill_build", false);
    for (p, indices) in buckets.iter().enumerate() {
        if indices.is_empty() {
            continue;
        }
        let writer = match &mut writers[p] {
            Some(w) => w,
            slot @ None => {
                let w = ctx.spill.create_file()?;
                slot.insert(w)
            }
        };

        let mut pos = 0usize;
        while pos < indices.len() {
            // Write small chunks bounded by `frame_cap_bytes` so SpillReader always fits in tight pools.
            let mut chunk_end = pos;
            let mut est_bytes = 0usize;
            while chunk_end < indices.len() && chunk_end - pos < ctx.batch_size.max(1) {
                let gi = indices[chunk_end];
                if let (Some(k), Some(s)) = (&keys[gi], &states[gi]) {
                    let row_est = estimate_partial_group_bytes(k, s);
                    if chunk_end > pos && est_bytes.saturating_add(row_est) > frame_cap_bytes {
                        break;
                    }
                    est_bytes = est_bytes.saturating_add(row_est);
                }
                chunk_end += 1;
            }
            let chunk_indices = &indices[pos..chunk_end];
            let mut chunk_keys = Vec::with_capacity(chunk_indices.len());
            let mut chunk_states = Vec::with_capacity(chunk_indices.len());
            for &gi in chunk_indices {
                if let (Some(k), Some(s)) = (keys[gi].take(), states[gi].take()) {
                    chunk_keys.push(k);
                    chunk_states.push(s);
                }
            }
            let mut chunk_ctx = PartialChunkCtx {
                writer,
                frame_cap_bytes,
                eval_ctx: &ctx.eval,
                scratch: &mut scratch,
            };
            write_partial_chunk(layout, &mut chunk_ctx, chunk_keys, chunk_states, state)?;
            pos = chunk_end;
        }
    }
    drop(keys);
    drop(states);
    state.res.free();
    state.unspillable_res.free();
    Ok(())
}

fn take_partial_batch_rows(
    batch: &Batch,
    indices: &[u32],
    res: &mut Reservation,
) -> Result<Batch, QueryError> {
    let mut cols = Vec::with_capacity(batch.columns().len());
    for col in batch.columns() {
        cols.push(Column::Flat(col.take(indices, Some(res))?));
    }
    Ok(Batch::try_new_all(
        Arc::clone(batch.schema()),
        cols,
        indices.len(),
    )?)
}

fn write_chunked_partial_rows(
    writer: &mut SpillWriter,
    batch: &Batch,
    indices: &[u32],
    frame_cap_bytes: usize,
    res: &mut Reservation,
) -> Result<(), QueryError> {
    if indices.is_empty() {
        return Ok(());
    }
    let rows_per_row_est = (batch.memory_bytes() / batch.raw_len().max(1)).max(1);
    let max_rows = (frame_cap_bytes / rows_per_row_est)
        .max(1)
        .min(indices.len());
    let mut start = 0usize;
    while start < indices.len() {
        let end = (start + max_rows).min(indices.len());
        let sub_idx = &indices[start..end];
        let sub = match take_partial_batch_rows(batch, sub_idx, res) {
            Ok(s) => s,
            Err(QueryError::TooLarge) if sub_idx.len() > 1 => {
                res.free();
                let half = sub_idx.len() / 2;
                write_chunked_partial_rows(writer, batch, &sub_idx[..half], frame_cap_bytes, res)?;
                write_chunked_partial_rows(writer, batch, &sub_idx[half..], frame_cap_bytes, res)?;
                start = end;
                continue;
            }
            Err(err) => {
                res.free();
                return Err(err);
            }
        };
        if sub.memory_bytes() > frame_cap_bytes && sub_idx.len() > 1 {
            drop(sub);
            res.free();
            let half = sub_idx.len() / 2;
            write_chunked_partial_rows(writer, batch, &sub_idx[..half], frame_cap_bytes, res)?;
            write_chunked_partial_rows(writer, batch, &sub_idx[half..], frame_cap_bytes, res)?;
        } else {
            writer.write_batch(&sub)?;
            drop(sub);
            res.free();
        }
        start = end;
    }
    Ok(())
}

fn route_partial_batch_to_writers(
    batch: &Batch,
    start_row: usize,
    num_group_cols: usize,
    salt: u64,
    frame_cap_bytes: usize,
    ctx: &ExecContext,
    writers: &mut [Option<SpillWriter>],
) -> Result<(), QueryError> {
    let cols: Vec<&Column> = batch.columns().iter().collect();
    let group_cols = &cols[..num_group_cols];
    let num_rows = batch.num_rows();
    if start_row >= num_rows {
        return Ok(());
    }
    let mut part_rows: Vec<Vec<u32>> = (0..NUM_SPILL_PARTITIONS).map(|_| Vec::new()).collect();
    for row in start_row..num_rows {
        if (row & CANCEL_CHECK_MASK) == 0 && ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        let h = hash_cols_row(group_cols, row, salt);
        let p = (h as usize) & (NUM_SPILL_PARTITIONS - 1);
        part_rows[p].push(row as u32);
    }

    let mut scratch = ctx.pool.register("hash_agg_route_take", false);
    for (p, rows) in part_rows.into_iter().enumerate() {
        if rows.is_empty() {
            continue;
        }
        let writer = match &mut writers[p] {
            Some(w) => w,
            slot @ None => {
                let w = ctx.spill.create_file()?;
                slot.insert(w)
            }
        };
        write_chunked_partial_rows(writer, batch, &rows, frame_cap_bytes, &mut scratch)?;
    }
    Ok(())
}

fn estimate_partial_group_bytes(key: &[KeyAtom], states: &[AccState]) -> usize {
    let mut total = key.len() * 16 + key.iter().map(KeyAtom::heap_bytes).sum::<usize>();
    for st in states {
        total += 32 + st.spillable_heap_bytes() + st.unspillable_heap_bytes();
    }
    total
}

fn build_partial_batch(
    partial_schema: Arc<Schema>,
    group_types: &[DataType],
    acc_kinds: &[AccKind],
    keys: &[Vec<KeyAtom>],
    states: &[Vec<AccState>],
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Batch, QueryError> {
    let num_rows = states.len();
    let mut columns = Vec::with_capacity(partial_schema.len());

    // Build group-by key columns.
    for (g_idx, dt) in group_types.iter().enumerate() {
        let scalars: Vec<ScalarValue> = keys.iter().map(|k| k[g_idx].to_scalar(dt)).collect();
        let col = build_column_from_scalars(dt, &scalars, eval_ctx, res)?;
        columns.push(col);
    }

    // Build partial-state accumulator columns.
    let mut row_encoded: Vec<Vec<ScalarValue>> = Vec::with_capacity(num_rows);
    for row_states in states {
        let mut vals = Vec::with_capacity(partial_schema.len() - group_types.len());
        for (st, kind) in row_states.iter().zip(acc_kinds) {
            st.encode_partial_scalars(kind, &mut vals);
        }
        row_encoded.push(vals);
    }

    let partial_acc_fields = &partial_schema.fields()[group_types.len()..];
    for (col_idx, field) in partial_acc_fields.iter().enumerate() {
        let scalars: Vec<ScalarValue> = row_encoded.iter().map(|r| r[col_idx].clone()).collect();
        let col = build_column_from_scalars(&field.data_type, &scalars, eval_ctx, res)?;
        columns.push(col);
    }

    Ok(Batch::try_new_all(partial_schema, columns, num_rows)?)
}

fn emit_chunked_output_batches(
    layout: &AggLayout<'_>,
    keys: &[Vec<KeyAtom>],
    states: &[Vec<AccState>],
    batch_size: usize,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Vec<Batch>, QueryError> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < states.len() {
        let end = (start + batch_size).min(states.len());
        let b = build_output_batch(
            Arc::clone(layout.output_schema),
            layout.group_types,
            layout.acc_kinds,
            &keys[start..end],
            &states[start..end],
            eval_ctx,
            res,
        )?;
        out.push(b);
        start = end;
    }
    Ok(out)
}

fn build_output_batch(
    output_schema: Arc<Schema>,
    group_types: &[DataType],
    acc_kinds: &[AccKind],
    keys: &[Vec<KeyAtom>],
    states: &[Vec<AccState>],
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Batch, QueryError> {
    build_output_batch_opt(
        output_schema,
        group_types,
        acc_kinds,
        keys,
        states,
        eval_ctx,
        Some(res),
    )
}

fn build_output_batch_opt(
    output_schema: Arc<Schema>,
    group_types: &[DataType],
    acc_kinds: &[AccKind],
    keys: &[Vec<KeyAtom>],
    states: &[Vec<AccState>],
    eval_ctx: &EvalContext,
    mut res: Option<&mut Reservation>,
) -> Result<Batch, QueryError> {
    let num_rows = states.len();
    let mut columns = Vec::with_capacity(output_schema.len());

    for (g_idx, dt) in group_types.iter().enumerate() {
        let scalars: Vec<ScalarValue> = keys.iter().map(|k| k[g_idx].to_scalar(dt)).collect();
        let col = build_column_from_scalars_opt(dt, &scalars, eval_ctx, res.as_deref_mut())?;
        columns.push(col);
    }

    for (agg_idx, kind) in acc_kinds.iter().enumerate() {
        let field = &output_schema.fields()[group_types.len() + agg_idx];
        let mut scalars = Vec::with_capacity(num_rows);
        for row_states in states {
            scalars.push(row_states[agg_idx].finalize_scalar(kind)?);
        }
        let col = build_column_from_scalars_opt(
            &field.data_type,
            &scalars,
            eval_ctx,
            res.as_deref_mut(),
        )?;
        columns.push(col);
    }

    Ok(Batch::try_new_all(output_schema, columns, num_rows)?)
}

fn build_column_from_scalars(
    dt: &DataType,
    scalars: &[ScalarValue],
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Column, QueryError> {
    build_column_from_scalars_opt(dt, scalars, eval_ctx, Some(res))
}

fn build_column_from_scalars_opt(
    dt: &DataType,
    scalars: &[ScalarValue],
    eval_ctx: &EvalContext,
    mut res: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = res.as_ref().map_or(0, |r| r.size());
    let refs: Vec<ElemRef<'_>> = scalars
        .iter()
        .map(|sv| {
            if sv.is_null() {
                ElemRef::Null
            } else {
                ElemRef::Scalar(sv)
            }
        })
        .collect();
    let col = build_flat_from_elem_refs(dt, &refs, &mut res, eval_ctx, before)?;
    reconcile_output(&mut res, before, col.memory_bytes())?;
    Ok(col)
}

struct HashAggSpillTask {
    file: SpillFile,
    depth: usize,
}

struct HashAggInMemoryRows {
    keys: Vec<Option<Vec<KeyAtom>>>,
    states: Vec<Option<Vec<AccState>>>,
    pos: usize,
    res: Reservation,
    unspillable_res: Reservation,
}

enum FoldPartitionResult {
    Empty,
    Ready(HashAggInMemoryRows),
    Repartitioned(Vec<HashAggSpillTask>),
}

fn fold_spill_partition_or_repartition(
    layout: &AggLayout<'_>,
    ctx: &ExecContext,
    file: SpillFile,
    depth: usize,
) -> Result<FoldPartitionResult, QueryError> {
    let mut reader = file.open_reader(Arc::clone(layout.partial_schema))?;
    let mut reader_res = ctx.pool.register("hash_agg_merge_read", false);
    let mut local_table = GroupTable::new(layout.acc_kinds.to_vec());
    let mut local_res = ctx.pool.register("hash_agg_merge", true);
    let mut local_unsp = ctx.pool.register("hash_agg_merge_unsp", false);
    let frame_cap = target_spill_frame_bytes(ctx);
    let mut max_batch_bytes = frame_cap;

    while let Some(batch) = reader.next_batch(Some(&mut reader_res))? {
        if ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        max_batch_bytes = max_batch_bytes.max(batch.memory_bytes());
        let headroom = max_batch_bytes.saturating_sub(reader_res.size());
        let mut group_state = GroupTableState {
            table: &mut local_table,
            res: &mut local_res,
            unspillable_res: &mut local_unsp,
        };
        let fold_res = fold_partial_batch_into_table(
            &batch,
            layout,
            &mut group_state,
            depth as u64,
            headroom,
            &ctx.eval,
        );
        match fold_res {
            Ok(None) => {
                drop(batch);
                reader_res.free();
            }
            Ok(Some(failed_row)) => {
                if depth >= MAX_REPARTITION_DEPTH {
                    return Err(QueryError::TooLarge);
                }
                let next_depth = depth + 1;
                let salt = next_depth as u64;
                let mut sub_writers: Vec<Option<SpillWriter>> =
                    (0..NUM_SPILL_PARTITIONS).map(|_| None).collect();

                if local_table.num_groups() > 0 {
                    let mut group_state = GroupTableState {
                        table: &mut local_table,
                        res: &mut local_res,
                        unspillable_res: &mut local_unsp,
                    };
                    drain_group_table_to_writers(
                        layout,
                        &mut group_state,
                        salt,
                        frame_cap,
                        ctx,
                        &mut sub_writers,
                    )?;
                }
                route_partial_batch_to_writers(
                    &batch,
                    failed_row,
                    layout.group_types.len(),
                    salt,
                    frame_cap,
                    ctx,
                    &mut sub_writers,
                )?;
                drop(batch);
                reader_res.free();

                while let Some(rem_batch) = reader.next_batch(Some(&mut reader_res))? {
                    route_partial_batch_to_writers(
                        &rem_batch,
                        0,
                        layout.group_types.len(),
                        salt,
                        frame_cap,
                        ctx,
                        &mut sub_writers,
                    )?;
                    drop(rem_batch);
                    reader_res.free();
                }

                drop(reader);
                drop(file);
                drop(local_res);
                drop(local_unsp);
                drop(reader_res);

                let mut sub_tasks = Vec::new();
                for w in sub_writers.into_iter().flatten() {
                    let f = w.finish()?;
                    if f.byte_len() > 0 {
                        sub_tasks.push(HashAggSpillTask {
                            file: f,
                            depth: next_depth,
                        });
                    }
                }
                return Ok(FoldPartitionResult::Repartitioned(sub_tasks));
            }
            Err(err) => return Err(err),
        }
    }

    drop(reader);
    drop(file);
    drop(reader_res);

    local_table.slots = Vec::new();
    local_table.hashes = Vec::new();
    let keys: Vec<Option<Vec<KeyAtom>>> = std::mem::take(&mut local_table.keys)
        .into_iter()
        .map(Some)
        .collect();
    let states: Vec<Option<Vec<AccState>>> = std::mem::take(&mut local_table.states)
        .into_iter()
        .map(Some)
        .collect();
    local_table.spillable_bytes = 0;
    local_table.unspillable_bytes = 0;
    if states.is_empty() {
        local_res.free();
        local_unsp.free();
        return Ok(FoldPartitionResult::Empty);
    }
    shrink_res_to_remaining(&keys, &states, &mut local_res, &mut local_unsp);
    Ok(FoldPartitionResult::Ready(HashAggInMemoryRows {
        keys,
        states,
        pos: 0,
        res: local_res,
        unspillable_res: local_unsp,
    }))
}

struct HashAggStream {
    group_types: Vec<DataType>,
    acc_kinds: Vec<AccKind>,
    partial_schema: Arc<Schema>,
    output_schema: Arc<Schema>,
    ctx: ExecContext,
    batch_size: usize,
    active: Option<HashAggInMemoryRows>,
    tasks: VecDeque<HashAggSpillTask>,
    done: bool,
}

impl fmt::Debug for HashAggStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashAggStream")
            .field("groups", &self.group_types.len())
            .field("aggs", &self.acc_kinds.len())
            .field("pending_tasks", &self.tasks.len())
            .field("done", &self.done)
            .finish()
    }
}

impl HashAggStream {
    fn cleanup(&mut self) {
        if let Some(mut mem) = self.active.take() {
            mem.keys.clear();
            mem.states.clear();
            mem.res.free();
            mem.unspillable_res.free();
        }
        self.tasks.clear();
        self.done = true;
    }
}

impl PartitionStream for HashAggStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        loop {
            if cancel.is_cancelled() {
                self.cleanup();
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if self.done {
                rollback_res(res, before);
                return Ok(None);
            }
            if let Some(mem) = self.active.as_mut() {
                if mem.pos < mem.states.len() {
                    let end = (mem.pos + self.batch_size).min(mem.states.len());
                    let is_last = end == mem.states.len();
                    let mut chunk_keys = if self.group_types.is_empty() {
                        Vec::new()
                    } else {
                        Vec::with_capacity(end - mem.pos)
                    };
                    let mut chunk_states = Vec::with_capacity(end - mem.pos);
                    for i in mem.pos..end {
                        if !self.group_types.is_empty()
                            && let Some(k) = mem.keys[i].take()
                        {
                            chunk_keys.push(k);
                        }
                        if let Some(s) = mem.states[i].take() {
                            chunk_states.push(s);
                        }
                    }
                    mem.pos = end;

                    let (chunk_sp, chunk_unsp) = chunk_owned_bytes(&chunk_keys, &chunk_states);
                    let built = build_output_batch_opt(
                        Arc::clone(&self.output_schema),
                        &self.group_types,
                        &self.acc_kinds,
                        &chunk_keys,
                        &chunk_states,
                        &self.ctx.eval,
                        None,
                    );
                    drop(chunk_keys);
                    drop(chunk_states);

                    if is_last {
                        mem.res.free();
                        mem.unspillable_res.free();
                        self.active = None;
                    } else {
                        mem.res.shrink(chunk_sp);
                        mem.unspillable_res.shrink(chunk_unsp);
                    }

                    let batch = match built {
                        Ok(b) => b,
                        Err(err) => {
                            self.cleanup();
                            rollback_res(res, before);
                            return Err(err);
                        }
                    };
                    if let Err(err) = reconcile_output(&mut res, before, batch.memory_bytes()) {
                        self.cleanup();
                        rollback_res(res, before);
                        return Err(err);
                    }
                    return Ok(Some(batch));
                }
                mem.res.free();
                mem.unspillable_res.free();
                self.active = None;
            }

            let Some(task) = self.tasks.pop_front() else {
                self.done = true;
                rollback_res(res, before);
                return Ok(None);
            };
            let layout = AggLayout {
                group_types: &self.group_types,
                acc_kinds: &self.acc_kinds,
                partial_schema: &self.partial_schema,
                output_schema: &self.output_schema,
            };
            match fold_spill_partition_or_repartition(&layout, &self.ctx, task.file, task.depth) {
                Ok(FoldPartitionResult::Empty) => {}
                Ok(FoldPartitionResult::Ready(mem)) => {
                    self.active = Some(mem);
                }
                Ok(FoldPartitionResult::Repartitioned(sub_tasks)) => {
                    for st in sub_tasks.into_iter().rev() {
                        self.tasks.push_front(st);
                    }
                }
                Err(err) => {
                    self.cleanup();
                    rollback_res(res, before);
                    return Err(err);
                }
            }
        }
    }
}

fn rollback_res(res: Option<&mut Reservation>, before: usize) {
    if let Some(r) = res {
        let cur = r.size();
        if cur > before {
            r.shrink(cur - before);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{BatchSource, ExecMetrics, SpillManager, WorkerPool};
    use crate::ir::BinaryOp;
    use cetra_column::{BinaryData, CancelToken, FlatColumn, FlatData, GlobalPool, Utf8Data};

    fn test_ctx(cap: usize) -> (ExecContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: EvalContext {
                now_ns: 0,
                cancel: CancelToken::new(),
            },
            spill: Arc::new(SpillManager::with_fixed_cap(
                dir.path().to_path_buf(),
                16 * 1024 * 1024,
            )),
            workers: Arc::new(WorkerPool::new(2).unwrap()),
            batch_size: 128,
            target_partitions: 2,
            metrics: ExecMetrics::new(),
        };
        (ctx, dir)
    }

    fn col_ref(index: usize, name: &str, data_type: DataType) -> Expr {
        Expr::Column {
            index,
            name: name.to_owned(),
            data_type,
        }
    }

    #[test]
    fn ddsketch_accuracy_and_merge_within_one_percent() {
        let mut empty = DdSketch::new();
        assert_eq!(empty.count(), 0);
        assert_eq!(empty.quantile(0.5).unwrap(), None);
        assert!(empty.quantile(-0.01).is_err());
        assert!(empty.quantile(1.01).is_err());
        assert!(empty.quantile(f64::NAN).is_err());
        assert!(empty.quantile(f64::INFINITY).is_err());

        empty.add(f64::NAN);
        empty.add(f64::INFINITY);
        empty.add(f64::NEG_INFINITY);
        assert_eq!(empty.count(), 0);

        // Positive + zero + negative finite values across 2 sketches, merged and round-tripped.
        let mut s1 = DdSketch::new();
        let mut s2 = DdSketch::new();
        let mut all_vals = Vec::with_capacity(10_001);

        for i in 0..5000 {
            let v = (i as f64 - 2000.0) * 1.25;
            s1.add(v);
            all_vals.push(if v == 0.0 { 0.0 } else { v });
        }
        s1.add(-0.0);
        all_vals.push(0.0);
        for i in 5000..10_000 {
            let v = (i as f64 - 2000.0) * 1.25;
            s2.add(v);
            all_vals.push(v);
        }

        // Round-trip binary serialization before merging.
        let bytes1 = s1.to_bytes();
        let bytes2 = s2.to_bytes();
        let mut merged = DdSketch::from_bytes(&bytes1).unwrap();
        let decoded2 = DdSketch::from_bytes(&bytes2).unwrap();
        merged.merge(&decoded2);

        assert_eq!(merged.count(), 10_001);
        all_vals.sort_by(|a, b| a.total_cmp(b));

        for p in [0.0, 0.25, 0.5, 0.9, 0.95, 0.99, 1.0] {
            let est = merged.quantile(p).unwrap().unwrap();
            let idx = (p * (all_vals.len() - 1) as f64).round() as usize;
            let truth = all_vals[idx];
            let rel_err = if truth.abs() < 1e-9 {
                est.abs()
            } else {
                (est - truth).abs() / truth.abs()
            };
            assert!(
                rel_err <= 0.01,
                "p={p}: est={est}, truth={truth}, rel_err={rel_err}"
            );
        }

        // High-dynamic-range input exceeding max_buckets=2048 collapses lowest buckets cleanly.
        let mut wide = DdSketch::new();
        for exp in -1500..1500 {
            let v = 1.02f64.powi(exp);
            if v.is_finite() {
                wide.add(v);
            }
        }
        assert!(wide.pos.len() + wide.neg.len() <= DDSKETCH_MAX_BUCKETS);
        let p99 = wide.quantile(0.99).unwrap().unwrap();
        assert!(p99.is_finite() && p99 > 0.0);

        // Corrupt bytes rejected cleanly.
        assert!(DdSketch::from_bytes(&[1, 2, 3]).is_err());
        let mut bad_ver = bytes1;
        bad_ver[0] = 99;
        assert!(DdSketch::from_bytes(&bad_ver).is_err());
    }

    #[test]
    fn all_aggregates_filter_where_decimal_cost_and_sessions() {
        let (ctx, _dir) = test_ctx(8 * 1024 * 1024);

        // Schema representing trace rows for sessions & general aggregates.
        let schema = Schema::try_new_arc(vec![
            Field::new("grp", DataType::Utf8, true),
            Field::new("start_time", DataType::TimestampNs { utc: true }, false),
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("tokens", DataType::I64, true),
            Field::new("cost", DataType::F64, true),
            Field::new("flag", DataType::Bool, true),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
            Field::new(
                "dec",
                DataType::Decimal128 {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ])
        .unwrap();

        // Build 2 batches so `execute_hash_agg` exercises 2 parallel partitions + combine.
        let tags_col_1 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 2, 2, 4],
                    values: Box::new(Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(
                                Utf8Data::try_from_slices(["prod", "eval", "alpha", "prod"])
                                    .unwrap(),
                            ),
                            None,
                        )
                        .unwrap(),
                    )),
                },
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["s1", "s2", "s1"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![300, 100, 100],
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16(vec![[3; 16], [1; 16], [2; 16]]),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["u_later", "", "u_tie_2"]).unwrap(),
                        ),
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::I64(vec![10, 0, 20]),
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![0.1, 99.0, 0.2]),
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Bool(Bitmap::from_bools(&[true, false, false])),
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                ),
                tags_col_1,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![100, 0, 200],
                            precision: 10,
                            scale: 2,
                        },
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                ),
            ],
            3,
        )
        .unwrap();

        // Batch 2: s1 gets earlier tie trace_id=[1;16] -> "u_first", s2 gets empty tags `[]`,
        // plus a NaN cost row in s2 that is excluded by `FILTER (WHERE tokens > 0)`.
        let tags_col_2 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 1, 1],
                    values: Box::new(Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(["zeta"]).unwrap()),
                            None,
                        )
                        .unwrap(),
                    )),
                },
                None,
            )
            .unwrap(),
        );
        let b2 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["s1", "s2"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![100, 200],
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::FixedBytes16(vec![[1; 16], [2; 16]]), None)
                        .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["u_first", ""]).unwrap()),
                        Some(Bitmap::from_bools(&[true, false])),
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 0]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(FlatData::F64(vec![0.3, f64::NAN]), None).unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&[true, false])), None)
                        .unwrap(),
                ),
                tags_col_2,
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![300, 0],
                            precision: 10,
                            scale: 2,
                        },
                        Some(Bitmap::from_bools(&[true, false])),
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();

        let filter_tok_pos = Expr::Binary {
            left: Box::new(col_ref(4, "tokens", DataType::I64)),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(ScalarValue::I64(0))),
            data_type: DataType::Bool,
        };

        let agg_spec = HashAggSpec {
            group_by: vec![("grp".to_owned(), col_ref(0, "grp", DataType::Utf8))],
            aggs: vec![
                AggExpr {
                    func: AggFunc::CountStar,
                    filter: None,
                    output_name: "cnt_all".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Count(col_ref(4, "tokens", DataType::I64)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "cnt_pos".to_owned(),
                },
                AggExpr {
                    func: AggFunc::CountDistinct(col_ref(4, "tokens", DataType::I64)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "cd_tok".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Sum(col_ref(4, "tokens", DataType::I64)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "sum_tok".to_owned(),
                },
                AggExpr {
                    func: AggFunc::SumCostDecimal(col_ref(5, "cost", DataType::F64)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "sum_cost".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Avg(col_ref(4, "tokens", DataType::I64)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "avg_tok".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Avg(col_ref(
                        8,
                        "dec",
                        DataType::Decimal128 {
                            precision: 10,
                            scale: 2,
                        },
                    )),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "avg_dec".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Min(col_ref(4, "tokens", DataType::I64)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "min_tok".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Max(col_ref(4, "tokens", DataType::I64)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "max_tok".to_owned(),
                },
                AggExpr {
                    func: AggFunc::BoolOr(col_ref(6, "flag", DataType::Bool)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "b_or".to_owned(),
                },
                AggExpr {
                    func: AggFunc::BoolAnd(col_ref(6, "flag", DataType::Bool)),
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "b_and".to_owned(),
                },
                AggExpr {
                    func: AggFunc::PercentileCont {
                        expr: col_ref(4, "tokens", DataType::I64),
                        quantile: 0.5,
                    },
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "p50_exact".to_owned(),
                },
                AggExpr {
                    func: AggFunc::ApproxPercentileCont {
                        expr: col_ref(4, "tokens", DataType::I64),
                        quantile: 0.5,
                    },
                    filter: Some(filter_tok_pos.clone()),
                    output_name: "p50_approx".to_owned(),
                },
                AggExpr {
                    func: AggFunc::ArrayAgg {
                        expr: col_ref(4, "tokens", DataType::I64),
                        distinct: true,
                    },
                    filter: Some(filter_tok_pos),
                    output_name: "arr_tok".to_owned(),
                },
                AggExpr {
                    func: AggFunc::FirstNonNullByOrder {
                        value: col_ref(3, "user_id", DataType::Utf8),
                        order_by: vec![
                            SortKey {
                                expr: col_ref(1, "start_time", DataType::TimestampNs { utc: true }),
                                descending: false,
                                nulls_first: true,
                            },
                            SortKey {
                                expr: col_ref(2, "trace_id", DataType::FixedBytes16),
                                descending: false,
                                nulls_first: true,
                            },
                        ],
                    },
                    filter: None,
                    output_name: "first_user".to_owned(),
                },
                AggExpr {
                    func: AggFunc::TagUnion(col_ref(
                        7,
                        "tags",
                        DataType::List(Box::new(DataType::Utf8)),
                    )),
                    filter: None,
                    output_name: "tags_union".to_owned(),
                },
            ],
        };

        let src = BatchSource::new(Arc::clone(&schema), vec![b1, b2.clone()]).unwrap();
        let out_batches = execute_hash_agg(&src, &ScanSpec::default(), agg_spec, &ctx).unwrap();

        let mut rows_by_grp = std::collections::HashMap::new();
        for b in &out_batches {
            for r in 0..b.num_rows() {
                let g = match b.column(0).unwrap().get_scalar(r).unwrap() {
                    ScalarValue::Utf8(s) => s,
                    other => panic!("unexpected grp {other:?}"),
                };
                let vals: Vec<ScalarValue> = (1..b.columns().len())
                    .map(|c| b.column(c).unwrap().get_scalar(r).unwrap())
                    .collect();
                rows_by_grp.insert(g, vals);
            }
        }
        assert_eq!(rows_by_grp.len(), 2);

        // Verify s1: 3 rows, tokens [10, 20, 10], cost 0.1+0.2+0.3 = 0.6 exact decimal
        let s1 = &rows_by_grp["s1"];
        assert_eq!(s1[0], ScalarValue::I64(3)); // cnt_all
        assert_eq!(s1[1], ScalarValue::I64(3)); // cnt_pos
        assert_eq!(s1[2], ScalarValue::I64(2)); // cd_tok
        assert_eq!(s1[3], ScalarValue::I64(40)); // sum_tok
        assert_eq!(
            s1[4],
            ScalarValue::Decimal128 {
                value: 600_000_000_000_000_000,
                precision: 38,
                scale: 18,
            }
        ); // exact 0.6 in Decimal128(38, 18)!
        assert_eq!(s1[5], ScalarValue::F64(40.0 / 3.0)); // avg_tok
        assert_eq!(
            s1[6],
            ScalarValue::Decimal128 {
                value: 2_000_000,
                precision: 14,
                scale: 6,
            }
        ); // avg(1.00, 2.00, 3.00) = 2.000000
        assert_eq!(s1[7], ScalarValue::I64(10)); // min_tok
        assert_eq!(s1[8], ScalarValue::I64(20)); // max_tok
        assert_eq!(s1[9], ScalarValue::Bool(true)); // bool_or
        assert_eq!(s1[10], ScalarValue::Bool(false)); // bool_and
        assert_eq!(s1[11], ScalarValue::F64(10.0)); // exact p50([10, 10, 20]) = 10.0
        assert_eq!(
            s1[13],
            ScalarValue::List {
                item_type: DataType::I64,
                values: vec![ScalarValue::I64(10), ScalarValue::I64(20)],
            }
        ); // array_agg distinct
        assert_eq!(s1[14], ScalarValue::Utf8("u_first".to_owned())); // FirstNonNullByOrder
        assert_eq!(
            s1[15],
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("alpha".to_owned()),
                    ScalarValue::Utf8("eval".to_owned()),
                    ScalarValue::Utf8("prod".to_owned()),
                    ScalarValue::Utf8("zeta".to_owned()),
                ],
            }
        ); // TagUnion sorted distinct

        // Verify s2: 2 rows, both excluded by FILTER (tokens = NULL / 0), tags all empty/null -> []
        let s2 = &rows_by_grp["s2"];
        assert_eq!(s2[0], ScalarValue::I64(2)); // cnt_all
        assert_eq!(s2[1], ScalarValue::I64(0)); // cnt_pos
        assert_eq!(s2[2], ScalarValue::I64(0)); // cd_tok
        assert_eq!(s2[3], ScalarValue::Null(DataType::I64)); // sum_tok
        assert_eq!(s2[4], ScalarValue::Null(DataType::cost_decimal())); // sum_cost
        assert_eq!(s2[14], ScalarValue::Null(DataType::Utf8)); // first_user (all null)
        assert_eq!(
            s2[15],
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![],
            }
        ); // TagUnion returns non-null empty []!

        // Without FILTER, NaN cost in b2 causes SumCostDecimal to fail with QueryError::Failed.
        let bad_spec = HashAggSpec {
            group_by: vec![],
            aggs: vec![AggExpr {
                func: AggFunc::SumCostDecimal(col_ref(5, "cost", DataType::F64)),
                filter: None,
                output_name: "sum_cost".to_owned(),
            }],
        };
        let mut bad_agg = HashAgg::new(bad_spec, Arc::clone(&schema), ctx.clone()).unwrap();
        let err = bad_agg.consume(b2).unwrap_err();
        assert!(matches!(err, QueryError::Failed(_)));

        // Global agg on empty input emits 1 row.
        let empty_src = BatchSource::empty(Arc::clone(&schema));
        let empty_spec = HashAggSpec {
            group_by: vec![],
            aggs: vec![
                AggExpr {
                    func: AggFunc::CountStar,
                    filter: None,
                    output_name: "c".to_owned(),
                },
                AggExpr {
                    func: AggFunc::SumCostDecimal(col_ref(5, "cost", DataType::F64)),
                    filter: None,
                    output_name: "cost".to_owned(),
                },
                AggExpr {
                    func: AggFunc::TagUnion(col_ref(
                        7,
                        "tags",
                        DataType::List(Box::new(DataType::Utf8)),
                    )),
                    filter: None,
                    output_name: "tags".to_owned(),
                },
            ],
        };
        let empty_out =
            execute_hash_agg(&empty_src, &ScanSpec::default(), empty_spec, &ctx).unwrap();
        assert_eq!(empty_out.len(), 1);
        assert_eq!(empty_out[0].num_rows(), 1);
        assert_eq!(
            empty_out[0].column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(0)
        );
        assert_eq!(
            empty_out[0].column(1).unwrap().get_scalar(0).unwrap(),
            ScalarValue::Null(DataType::cost_decimal())
        );
        assert_eq!(
            empty_out[0].column(2).unwrap().get_scalar(0).unwrap(),
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![],
            }
        );

        assert_eq!(ctx.spill.spilled_bytes(), 0);
        assert_eq!(ctx.pool.reserved(), 0);
    }

    #[test]
    fn hash_agg_spills_under_tight_memory_with_count_distinct() {
        let schema = Schema::try_new_arc(vec![
            Field::new("grp", DataType::Utf8, false),
            Field::new("val", DataType::Utf8, false),
            Field::new("num", DataType::I64, false),
        ])
        .unwrap();

        let mut batches = Vec::new();
        for b_idx in 0..8 {
            let mut grps = Vec::with_capacity(128);
            let mut vals = Vec::with_capacity(128);
            let mut nums = Vec::with_capacity(128);
            for r in 0..128 {
                let row_id = b_idx * 128 + r;
                grps.push(format!("group-{:04}", row_id % 400));
                vals.push(format!("distinct-val-{:04}", row_id % 600));
                nums.push((row_id as i64) + 1);
            }
            let c0 = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(grps.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            );
            let c1 = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(vals.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            );
            let c2 = Column::Flat(FlatColumn::try_new(FlatData::I64(nums), None).unwrap());
            batches.push(Batch::try_new_all(Arc::clone(&schema), vec![c0, c1, c2], 128).unwrap());
        }
        let source = BatchSource::new(Arc::clone(&schema), batches.clone()).unwrap();

        let agg_spec = HashAggSpec {
            group_by: vec![("grp".to_owned(), col_ref(0, "grp", DataType::Utf8))],
            aggs: vec![
                AggExpr {
                    func: AggFunc::CountDistinct(col_ref(1, "val", DataType::Utf8)),
                    filter: None,
                    output_name: "cd".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Sum(col_ref(2, "num", DataType::I64)),
                    filter: None,
                    output_name: "sum_num".to_owned(),
                },
                AggExpr {
                    func: AggFunc::ApproxPercentileCont {
                        expr: col_ref(2, "num", DataType::I64),
                        quantile: 0.9,
                    },
                    filter: None,
                    output_name: "p90".to_owned(),
                },
            ],
        };

        // Reference unspilled run under generous memory.
        let (big_ctx, _big_dir) = test_ctx(32 * 1024 * 1024);
        let expected_batches =
            execute_hash_agg(&source, &ScanSpec::default(), agg_spec.clone(), &big_ctx).unwrap();

        // Tight 64 KiB pool forcing multiple spills during single-sink and multi-partition runs.
        let (tight_ctx, _tight_dir) = test_ctx(64 * 1024);
        let tight_ctx_single = ExecContext {
            target_partitions: 1,
            ..tight_ctx.clone()
        };
        let mut sink =
            HashAgg::new(agg_spec.clone(), Arc::clone(&schema), tight_ctx_single).unwrap();
        for b in &batches {
            sink.consume(b.clone()).unwrap();
        }
        sink.finish().unwrap();
        assert!(sink.spilled, "expected HashAgg to spill under 64 KiB cap");
        assert!(tight_ctx.spill.spilled_bytes() > 0);

        let actual_batches = sink.into_batches().unwrap();
        assert_eq!(tight_ctx.spill.spilled_bytes(), 0);
        assert_eq!(tight_ctx.pool.reserved(), 0);

        let to_map = |bs: &[Batch]| -> std::collections::BTreeMap<String, (i64, i64)> {
            let mut m = std::collections::BTreeMap::new();
            for b in bs {
                for r in 0..b.num_rows() {
                    let ScalarValue::Utf8(g) = b.column(0).unwrap().get_scalar(r).unwrap() else {
                        panic!("expected utf8 group");
                    };
                    let ScalarValue::I64(cd) = b.column(1).unwrap().get_scalar(r).unwrap() else {
                        panic!("expected i64 cd");
                    };
                    let ScalarValue::I64(sum) = b.column(2).unwrap().get_scalar(r).unwrap() else {
                        panic!("expected i64 sum");
                    };
                    m.insert(g, (cd, sum));
                }
            }
            m
        };

        let expected_map = to_map(&expected_batches);
        let actual_map = to_map(&actual_batches);
        assert_eq!(expected_map.len(), 400);
        assert_eq!(actual_map, expected_map);

        // Also test parallel 2-partition execution under tight 96 KiB pool.
        let (par_ctx, _par_dir) = test_ctx(96 * 1024);
        let par_batches =
            execute_hash_agg(&source, &ScanSpec::default(), agg_spec, &par_ctx).unwrap();
        assert_eq!(to_map(&par_batches), expected_map);
        assert_eq!(par_ctx.spill.spilled_bytes(), 0);
        assert_eq!(par_ctx.pool.reserved(), 0);
        assert_eq!(big_ctx.pool.reserved(), 0);

        // Also check I64 Sum overflow -> QueryError::Failed and BinaryData type compilation.
        let ovf_schema = Schema::try_new_arc(vec![
            Field::new("b", DataType::Binary, false),
            Field::new("n", DataType::I64, false),
        ])
        .unwrap();
        let ovf_batch = Batch::try_new_all(
            Arc::clone(&ovf_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Binary(
                            BinaryData::try_from_slices([b"x".as_slice(), b"x"]).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![i64::MAX, 1]), None).unwrap()),
            ],
            2,
        )
        .unwrap();
        let ovf_spec = HashAggSpec {
            group_by: vec![("b".to_owned(), col_ref(0, "b", DataType::Binary))],
            aggs: vec![AggExpr {
                func: AggFunc::Sum(col_ref(1, "n", DataType::I64)),
                filter: None,
                output_name: "s".to_owned(),
            }],
        };
        let mut ovf_agg = HashAgg::new(ovf_spec, ovf_schema, big_ctx).unwrap();
        assert!(matches!(
            ovf_agg.consume(ovf_batch),
            Err(QueryError::Failed(_))
        ));

        // Unspillable global PercentileCont hits TooLarge under a tight 2 KiB pool.
        let (tiny_ctx, _tiny_dir) = test_ctx(2 * 1024);
        let pct_spec = HashAggSpec {
            group_by: vec![],
            aggs: vec![AggExpr {
                func: AggFunc::PercentileCont {
                    expr: col_ref(2, "num", DataType::I64),
                    quantile: 0.5,
                },
                filter: None,
                output_name: "p50".to_owned(),
            }],
        };
        let mut pct_agg = HashAgg::new(pct_spec, Arc::clone(&schema), tiny_ctx.clone()).unwrap();
        let mut hit_too_large = false;
        for b in &batches {
            if let Err(QueryError::TooLarge) = pct_agg.consume(b.clone()) {
                hit_too_large = true;
                break;
            }
        }
        assert!(
            hit_too_large,
            "expected unspillable global PercentileCont to hit TooLarge"
        );
        drop(pct_agg);
        assert_eq!(tiny_ctx.pool.reserved(), 0);
        assert_eq!(tiny_ctx.spill.spilled_bytes(), 0);
    }

    #[test]
    fn hash_cols_row_matches_hash_atoms_every_datatype() {
        use cetra_column::{DictColumn, EventItem, LinkItem, RleColumn};
        let (ctx, _dir) = test_ctx(4 * 1024 * 1024);
        let mut res = ctx.pool.register("hash_parity", false);

        let cols: Vec<Column> = vec![
            Column::try_new_const(ScalarValue::Null(DataType::Null), 2).unwrap(),
            build_column_from_scalars(
                &DataType::Bool,
                &[
                    ScalarValue::Bool(true),
                    ScalarValue::Bool(false),
                    ScalarValue::Null(DataType::Bool),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::I64,
                &[
                    ScalarValue::I64(-42),
                    ScalarValue::I64(99),
                    ScalarValue::Null(DataType::I64),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::F64,
                &[
                    ScalarValue::F64(0.0),
                    ScalarValue::F64(-0.0),
                    ScalarValue::F64(f64::NAN),
                    ScalarValue::F64(f64::INFINITY),
                    ScalarValue::Null(DataType::F64),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::Decimal128 {
                    precision: 12,
                    scale: 3,
                },
                &[
                    ScalarValue::Decimal128 {
                        value: 123_456,
                        precision: 12,
                        scale: 3,
                    },
                    ScalarValue::Null(DataType::Decimal128 {
                        precision: 12,
                        scale: 3,
                    }),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::TimestampNs { utc: true },
                &[
                    ScalarValue::TimestampNs {
                        value: 1_700_000_000_000_000_000,
                        utc: true,
                    },
                    ScalarValue::Null(DataType::TimestampNs { utc: true }),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::Date,
                &[ScalarValue::Date(19_700), ScalarValue::Null(DataType::Date)],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::Interval,
                &[
                    ScalarValue::Interval(Interval {
                        months: 2,
                        days: 3,
                        nanos: 4_000,
                    }),
                    ScalarValue::Null(DataType::Interval),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::Utf8,
                &[
                    ScalarValue::Utf8("hello".into()),
                    ScalarValue::Null(DataType::Utf8),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::Binary,
                &[
                    ScalarValue::Binary(vec![1, 2, 3, 4]),
                    ScalarValue::Null(DataType::Binary),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::FixedBytes8,
                &[
                    ScalarValue::FixedBytes8([1, 2, 3, 4, 5, 6, 7, 8]),
                    ScalarValue::Null(DataType::FixedBytes8),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::FixedBytes16,
                &[
                    ScalarValue::FixedBytes16([9; 16]),
                    ScalarValue::Null(DataType::FixedBytes16),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::List(Box::new(DataType::I64)),
                &[
                    ScalarValue::List {
                        item_type: DataType::I64,
                        values: vec![ScalarValue::I64(1), ScalarValue::I64(2)],
                    },
                    ScalarValue::Null(DataType::List(Box::new(DataType::I64))),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::MapUtf8,
                &[
                    ScalarValue::MapUtf8(vec![("k".into(), Some("v".into()))]),
                    ScalarValue::Null(DataType::MapUtf8),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::Events,
                &[
                    ScalarValue::Events(vec![EventItem {
                        time: 10,
                        name: "ev".into(),
                        attributes: Some("{}".into()),
                    }]),
                    ScalarValue::Null(DataType::Events),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            build_column_from_scalars(
                &DataType::Links,
                &[
                    ScalarValue::Links(vec![LinkItem {
                        trace_id: [1; 16],
                        span_id: [2; 8],
                        attributes: None,
                    }]),
                    ScalarValue::Null(DataType::Links),
                ],
                &ctx.eval,
                &mut res,
            )
            .unwrap(),
            Column::Dict(
                DictColumn::try_new(
                    FlatColumn::try_new(FlatData::FixedBytes8(vec![[1; 8], [2; 8]]), None).unwrap(),
                    vec![0, 1, 0],
                    Some(Bitmap::from_bools(&[true, false, true])),
                )
                .unwrap(),
            ),
            Column::Rle(
                RleColumn::try_new(
                    FlatColumn::try_new(FlatData::FixedBytes16(vec![[3; 16], [4; 16]]), None)
                        .unwrap(),
                    vec![2, 4],
                )
                .unwrap(),
            ),
        ];

        for col in &cols {
            for r in 0..col.len() {
                for salt in [0u64, 1, 2, 42] {
                    let h_col = hash_cols_row(&[col], r, salt);
                    let atom = read_key_atom(col, r);
                    let h_atom = hash_atoms(&[atom], salt);
                    assert_eq!(
                        h_col,
                        h_atom,
                        "hash mismatch for {:?} row {r} salt {salt}",
                        col.data_type()
                    );
                }
            }
        }
        res.free();
    }

    #[test]
    fn group_by_fixed_bytes16_trace_id_spill_exact() {
        let schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("val", DataType::I64, false),
        ])
        .unwrap();

        let mut batches = Vec::new();
        for b in 0..8 {
            let mut tids = Vec::with_capacity(200);
            let mut vals = Vec::with_capacity(200);
            for i in 0..200 {
                let g = (b * 50 + i) % 400;
                let mut tid = [0u8; 16];
                tid[..8].copy_from_slice(&(g as u64).to_le_bytes());
                tid[8..].copy_from_slice(&((g as u64) ^ 0xA5A5_5A5A).to_le_bytes());
                tids.push(tid);
                vals.push((g % 13 + 1) as i64);
            }
            batches.push(
                Batch::try_new_all(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(
                            FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap(),
                        ),
                        Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None).unwrap()),
                    ],
                    200,
                )
                .unwrap(),
            );
        }

        let source = BatchSource::new(Arc::clone(&schema), batches).unwrap();
        let agg_spec = HashAggSpec {
            group_by: vec![(
                "trace_id".to_owned(),
                col_ref(0, "trace_id", DataType::FixedBytes16),
            )],
            aggs: vec![
                AggExpr {
                    func: AggFunc::CountStar,
                    filter: None,
                    output_name: "cnt".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Sum(col_ref(1, "val", DataType::I64)),
                    filter: None,
                    output_name: "sum".to_owned(),
                },
            ],
        };

        let to_map = |bs: &[Batch]| -> std::collections::BTreeMap<[u8; 16], (i64, i64)> {
            let mut m = std::collections::BTreeMap::new();
            for b in bs {
                for r in 0..b.num_rows() {
                    let ScalarValue::FixedBytes16(tid) =
                        b.column(0).unwrap().get_scalar(r).unwrap()
                    else {
                        panic!("expected FixedBytes16");
                    };
                    let ScalarValue::I64(cnt) = b.column(1).unwrap().get_scalar(r).unwrap() else {
                        panic!("expected i64 cnt");
                    };
                    let ScalarValue::I64(sum) = b.column(2).unwrap().get_scalar(r).unwrap() else {
                        panic!("expected i64 sum");
                    };
                    assert!(
                        m.insert(tid, (cnt, sum)).is_none(),
                        "duplicate group after spill combine!"
                    );
                }
            }
            m
        };

        let (big_ctx, _big_dir) = test_ctx(8 * 1024 * 1024);
        let expected = to_map(
            &execute_hash_agg(&source, &ScanSpec::default(), agg_spec.clone(), &big_ctx).unwrap(),
        );
        assert_eq!(expected.len(), 400);

        let (tight_ctx, _tight_dir) = test_ctx(64 * 1024);
        let actual =
            to_map(&execute_hash_agg(&source, &ScanSpec::default(), agg_spec, &tight_ctx).unwrap());
        assert_eq!(actual, expected);
        assert_eq!(tight_ctx.pool.reserved(), 0);
        assert_eq!(tight_ctx.spill.spilled_bytes(), 0);
    }

    #[test]
    fn group_table_incremental_bytes_match_full_scan() {
        let kinds = vec![
            AccKind::CountDistinct(DataType::Utf8),
            AccKind::Percentile {
                input_type: DataType::I64,
                quantile: 0.5,
            },
        ];
        let mut table = GroupTable::new(kinds.clone());
        assert_eq!(table.spillable_bytes, table.compute_spillable_bytes());

        let str_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta-longer"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let num_col = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20]), None).unwrap());

        for g in 0..500usize {
            let key = vec![KeyAtom::Utf8(format!("group-{g:04}"))];
            let h = hash_atoms(&key, 0);
            let gi = table.insert_group(h, key);
            assert_eq!(table.spillable_bytes, table.compute_spillable_bytes());

            for r in 0..2 {
                let st0 = &mut table.states[gi][0];
                let sp_before = st0.spillable_heap_bytes();
                st0.fold_row(&kinds[0], &[&str_col], r).unwrap();
                table.spillable_bytes = table
                    .spillable_bytes
                    .saturating_sub(sp_before)
                    .saturating_add(st0.spillable_heap_bytes());

                let st1 = &mut table.states[gi][1];
                let unsp_before = st1.unspillable_heap_bytes();
                st1.fold_row(&kinds[1], &[&num_col], r).unwrap();
                table.unspillable_bytes = table
                    .unspillable_bytes
                    .saturating_sub(unsp_before)
                    .saturating_add(st1.unspillable_heap_bytes());

                assert_eq!(table.spillable_bytes, table.compute_spillable_bytes());
            }
        }
    }

    #[test]
    fn filter_where_computed_expr_no_fault_and_dict_rle_const_encodings() {
        use cetra_column::{DictColumn, RleColumn};

        // FILTER (WHERE denom != 0) with SUM(100 / denom) must not fault on denom == 0 rows
        // and must work cleanly with zero batch.clone() under a tight pool.
        let schema = Schema::try_new_arc(vec![
            Field::new("grp", DataType::Utf8, false),
            Field::new("denom", DataType::I64, false),
            Field::new("tag", DataType::Utf8, false),
            Field::new("const_v", DataType::I64, false),
        ])
        .unwrap();

        let grp_dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["g0", "g1"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![0, 1, 0, 1, 0, 1],
                None,
            )
            .unwrap(),
        );
        let denom_rle = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(FlatData::I64(vec![0, 2, 5]), None).unwrap(),
                vec![2, 4, 6],
            )
            .unwrap(),
        );
        let tag_flat = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["a", "b", "c", "d", "e", "f"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let const_v = Column::try_new_const(ScalarValue::I64(7), 6).unwrap();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![grp_dict, denom_rle, tag_flat, const_v],
            6,
        )
        .unwrap();
        let src = BatchSource::new(Arc::clone(&schema), vec![batch]).unwrap();

        let denom_nz = Expr::Binary {
            left: Box::new(col_ref(1, "denom", DataType::I64)),
            op: BinaryOp::NotEq,
            right: Box::new(Expr::Literal(ScalarValue::I64(0))),
            data_type: DataType::Bool,
        };
        let div_expr = Expr::Binary {
            left: Box::new(Expr::Literal(ScalarValue::I64(100))),
            op: BinaryOp::Div,
            right: Box::new(col_ref(1, "denom", DataType::I64)),
            data_type: DataType::I64,
        };
        let agg_spec = HashAggSpec {
            group_by: vec![
                ("grp".to_owned(), col_ref(0, "grp", DataType::Utf8)),
                ("const_v".to_owned(), col_ref(3, "const_v", DataType::I64)),
            ],
            aggs: vec![
                AggExpr {
                    func: AggFunc::Sum(div_expr),
                    filter: Some(denom_nz.clone()),
                    output_name: "sum_div".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Count(col_ref(2, "tag", DataType::Utf8)),
                    filter: Some(denom_nz),
                    output_name: "cnt_tag".to_owned(),
                },
                AggExpr {
                    func: AggFunc::Sum(col_ref(3, "const_v", DataType::I64)),
                    filter: None,
                    output_name: "sum_const".to_owned(),
                },
            ],
        };

        let (ctx, _dir) = test_ctx(128 * 1024);
        let out = execute_hash_agg(&src, &ScanSpec::default(), agg_spec, &ctx).unwrap();
        assert_eq!(ctx.pool.reserved(), 0);

        let mut map = std::collections::BTreeMap::new();
        for b in &out {
            for r in 0..b.num_rows() {
                let ScalarValue::Utf8(g) = b.column(0).unwrap().get_scalar(r).unwrap() else {
                    panic!("expected utf8");
                };
                let ScalarValue::I64(cv) = b.column(1).unwrap().get_scalar(r).unwrap() else {
                    panic!("expected i64");
                };
                let ScalarValue::I64(sd) = b.column(2).unwrap().get_scalar(r).unwrap() else {
                    panic!("expected i64");
                };
                let ScalarValue::I64(ct) = b.column(3).unwrap().get_scalar(r).unwrap() else {
                    panic!("expected i64");
                };
                let ScalarValue::I64(sc) = b.column(4).unwrap().get_scalar(r).unwrap() else {
                    panic!("expected i64");
                };
                map.insert((g, cv), (sd, ct, sc));
            }
        }
        // Rows:
        // 0: g0, denom=0 (filtered out), const=7
        // 1: g1, denom=0 (filtered out), const=7
        // 2: g0, denom=2 -> 50, const=7
        // 3: g1, denom=2 -> 50, const=7
        // 4: g0, denom=5 -> 20, const=7
        // 5: g1, denom=5 -> 20, const=7
        assert_eq!(map.get(&("g0".to_owned(), 7)), Some(&(70, 2, 21)));
        assert_eq!(map.get(&("g1".to_owned(), 7)), Some(&(70, 2, 21)));

        // Equal +/-inf in exact_percentile_sorted does not produce NaN.
        assert_eq!(
            exact_percentile_sorted(&[f64::INFINITY, f64::INFINITY], 0.5).unwrap(),
            f64::INFINITY
        );
        assert_eq!(
            exact_percentile_sorted(&[f64::NEG_INFINITY, f64::NEG_INFINITY], 0.25).unwrap(),
            f64::NEG_INFINITY
        );
    }

    #[test]
    fn first_by_order_preserves_null_and_combines_sinks_to_stream() {
        let schema = Schema::try_new_arc(vec![
            Field::new("grp", DataType::Utf8, false),
            Field::new("ord", DataType::I64, true),
            Field::new("val", DataType::Utf8, true),
            Field::new("ok", DataType::Bool, false),
        ])
        .unwrap();

        let make_batch = |rows: &[(&str, Option<i64>, Option<&str>, bool)]| -> Batch {
            let grps: Vec<&str> = rows.iter().map(|r| r.0).collect();
            let ords: Vec<i64> = rows.iter().map(|r| r.1.unwrap_or(0)).collect();
            let ord_nulls: Vec<bool> = rows.iter().map(|r| r.1.is_some()).collect();
            let vals: Vec<&str> = rows.iter().map(|r| r.2.unwrap_or("")).collect();
            let val_nulls: Vec<bool> = rows.iter().map(|r| r.2.is_some()).collect();
            let oks: Vec<bool> = rows.iter().map(|r| r.3).collect();
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(grps).unwrap()),
                            None,
                        )
                        .unwrap(),
                    ),
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::I64(ords),
                            Some(Bitmap::from_bools(&ord_nulls)),
                        )
                        .unwrap(),
                    ),
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(vals).unwrap()),
                            Some(Bitmap::from_bools(&val_nulls)),
                        )
                        .unwrap(),
                    ),
                    Column::Flat(
                        FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&oks)), None)
                            .unwrap(),
                    ),
                ],
                rows.len(),
            )
            .unwrap()
        };

        // Partition 1:
        // - "g_null_win": ord=1 has val=NULL (must win over ord=2!)
        // - "g_val_win": ord=2 has val=NULL
        // - "g_filt": ok=false (no matching row in partition 1, so state has best=None)
        // - "g_null_ord_win": ord=NULL (nulls_first=true) has val=NULL (must beat ord=10 in P2!)
        let b1 = make_batch(&[
            ("g_null_win", Some(1), None, true),
            ("g_null_win", Some(2), Some("later"), true),
            ("g_val_win", Some(2), None, true),
            ("g_filt", Some(1), Some("ignored"), false),
            ("g_null_ord_win", None, None, true),
        ]);
        // Partition 2:
        // - "g_null_win": ord=0? No, ord=5 has val="even_later"
        // - "g_val_win": ord=1 has val="winner"
        // - "g_filt": ok=true, ord=5 has val="from_p2" (must NOT be beaten by P1's None!)
        // - "g_null_ord_win": ord=10 has val="ten"
        let b2 = make_batch(&[
            ("g_null_win", Some(5), Some("even_later"), true),
            ("g_val_win", Some(1), Some("winner"), true),
            ("g_filt", Some(5), Some("from_p2"), true),
            ("g_null_ord_win", Some(10), Some("ten"), true),
        ]);

        let spec = HashAggSpec {
            group_by: vec![("grp".to_owned(), col_ref(0, "grp", DataType::Utf8))],
            aggs: vec![
                AggExpr {
                    func: AggFunc::FirstByOrder {
                        value: col_ref(2, "val", DataType::Utf8),
                        order_by: vec![SortKey {
                            expr: col_ref(1, "ord", DataType::I64),
                            descending: false,
                            nulls_first: true,
                        }],
                    },
                    filter: None,
                    output_name: "first_any".to_owned(),
                },
                AggExpr {
                    func: AggFunc::FirstByOrder {
                        value: col_ref(2, "val", DataType::Utf8),
                        order_by: vec![SortKey {
                            expr: col_ref(1, "ord", DataType::I64),
                            descending: false,
                            nulls_first: true,
                        }],
                    },
                    filter: Some(col_ref(3, "ok", DataType::Bool)),
                    output_name: "first_filt".to_owned(),
                },
            ],
        };

        let (ctx, _dir) = test_ctx(8 * 1024 * 1024);
        let src = BatchSource::new(Arc::clone(&schema), vec![b1, b2]).unwrap();
        let parts = src.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        let mut stream =
            execute_hash_agg_streams(parts, Arc::clone(&schema), spec.clone(), &ctx).unwrap();

        let mut res = ctx.pool.register("test_stream", false);
        res.try_grow(128).unwrap();
        let before = res.size();
        let mut out_batches = Vec::new();
        while let Some(b) = stream.next_batch(&ctx.eval.cancel, Some(&mut res)).unwrap() {
            assert_eq!(res.size(), before + b.memory_bytes());
            res.shrink(b.memory_bytes());
            out_batches.push(b);
        }
        assert_eq!(res.size(), before);
        res.free();
        assert_eq!(ctx.pool.reserved(), 0);

        let mut map = std::collections::BTreeMap::new();
        for b in &out_batches {
            for r in 0..b.num_rows() {
                let ScalarValue::Utf8(g) = b.column(0).unwrap().get_scalar(r).unwrap() else {
                    panic!("expected grp");
                };
                let v1 = b.column(1).unwrap().get_scalar(r).unwrap();
                let v2 = b.column(2).unwrap().get_scalar(r).unwrap();
                map.insert(g, (v1, v2));
            }
        }
        assert_eq!(
            map.get("g_null_win"),
            Some(&(
                ScalarValue::Null(DataType::Utf8),
                ScalarValue::Null(DataType::Utf8),
            ))
        );
        assert_eq!(
            map.get("g_val_win"),
            Some(&(
                ScalarValue::Utf8("winner".to_owned()),
                ScalarValue::Utf8("winner".to_owned()),
            ))
        );
        assert_eq!(
            map.get("g_filt"),
            Some(&(
                ScalarValue::Utf8("ignored".to_owned()),
                ScalarValue::Utf8("from_p2".to_owned()),
            ))
        );
        assert_eq!(
            map.get("g_null_ord_win"),
            Some(&(
                ScalarValue::Null(DataType::Utf8),
                ScalarValue::Null(DataType::Utf8),
            ))
        );

        // Empty sinks on global agg emits 1-row default global-agg batch via both merge_sinks and combine_sinks_to_stream.
        let global_spec = HashAggSpec {
            group_by: Vec::new(),
            aggs: vec![
                AggExpr {
                    func: AggFunc::CountStar,
                    filter: None,
                    output_name: "cnt".to_owned(),
                },
                AggExpr {
                    func: AggFunc::FirstByOrder {
                        value: col_ref(2, "val", DataType::Utf8),
                        order_by: vec![SortKey {
                            expr: col_ref(1, "ord", DataType::I64),
                            descending: false,
                            nulls_first: false,
                        }],
                    },
                    filter: None,
                    output_name: "first_v".to_owned(),
                },
            ],
        };
        let merged = HashAgg::merge_sinks(Vec::new(), &global_spec, &schema, &ctx).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].num_rows(), 1);
        assert_eq!(
            merged[0].column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(0)
        );
        assert_eq!(
            merged[0].column(1).unwrap().get_scalar(0).unwrap(),
            ScalarValue::Null(DataType::Utf8)
        );

        let mut g_stream =
            HashAgg::combine_sinks_to_stream(Vec::new(), &global_spec, &schema, &ctx).unwrap();
        let g_b = g_stream
            .next_batch(&ctx.eval.cancel, None)
            .unwrap()
            .unwrap();
        assert_eq!(g_b.num_rows(), 1);
        assert!(
            g_stream
                .next_batch(&ctx.eval.cancel, None)
                .unwrap()
                .is_none()
        );
        drop(merged);
        drop(g_b);
        drop(g_stream);

        // Spilled streaming path: high-cardinality groups under tight pool + small batch_size.
        let (tight_ctx, _tight_dir) = test_ctx(96 * 1024);
        let mut tight_ctx = tight_ctx;
        tight_ctx.batch_size = 64;
        let big_rows: Vec<(String, Option<i64>, Option<String>, bool)> = (0..1500)
            .map(|i| {
                (
                    format!("g_{:04}", i % 400),
                    Some(1500 - i),
                    if i % 3 == 0 {
                        None
                    } else {
                        Some(format!("v_{i}"))
                    },
                    true,
                )
            })
            .collect();
        let big_refs: Vec<(&str, Option<i64>, Option<&str>, bool)> = big_rows
            .iter()
            .map(|(g, o, v, ok)| (g.as_str(), *o, v.as_deref(), *ok))
            .collect();
        let big_batches: Vec<Batch> = big_refs.chunks(250).map(make_batch).collect();
        let big_src = BatchSource::new(Arc::clone(&schema), big_batches).unwrap();
        let big_parts = big_src
            .open_partitions(&ScanSpec::default(), &tight_ctx)
            .unwrap();
        let mut big_stream =
            execute_hash_agg_streams(big_parts, Arc::clone(&schema), spec, &tight_ctx).unwrap();
        let mut total_groups = 0usize;
        let mut g0_vals = None;
        while let Some(b) = big_stream.next_batch(&tight_ctx.eval.cancel, None).unwrap() {
            total_groups += b.num_rows();
            for r in 0..b.num_rows() {
                if b.column(0).unwrap().get_scalar(r).unwrap()
                    == ScalarValue::Utf8("g_0000".to_owned())
                {
                    g0_vals = Some(b.column(1).unwrap().get_scalar(r).unwrap());
                }
            }
        }
        assert_eq!(total_groups, 400);
        // For g_0000, i in {0, 400, 800, 1200}: smallest ord = 1500 - 1200 = 300 (i=1200, 1200 % 3 == 0 -> NULL!).
        assert_eq!(g0_vals, Some(ScalarValue::Null(DataType::Utf8)));
        drop(big_stream);
        assert_eq!(tight_ctx.pool.reserved(), 0);
        assert_eq!(tight_ctx.spill.spilled_bytes(), 0);
    }
}
