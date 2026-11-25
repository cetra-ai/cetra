//! Spillable Grace `HashJoin`, `LeftSemi`, `LeftAnti`, and bounded `CrossJoin` execution.

use std::cmp::Ordering;
use std::collections::{BTreeSet, VecDeque};
use std::mem::size_of;
use std::sync::Arc;

use cetra_column::{
    Batch, Bitmap, CancelToken, Column, ColumnError, DataType, Field, Reservation, ScalarValue,
    Schema,
};

use crate::error::QueryError;
use crate::eval::{
    ElemRef, EventsRef, LinksRef, MapRef, bitmap_mem_bytes, build_flat_from_sources,
    compare_decimals, eval_expr, eval_filter, f64_canonical_bits, f64_cmp, list_ref_at,
    list_ref_elem, list_ref_len,
};
use crate::exec::{
    ExecContext, PartitionStream, SpillFile, SpillManager, SpillReader, SpillWriter,
};
use crate::ir::{EvalContext, Expr};

/// Number of hash partitions created on each Grace spill pass.
pub const NUM_JOIN_PARTITIONS: usize = 32;

/// Maximum recursive salted re-partitioning depth before failing with [`QueryError::TooLarge`].
pub const MAX_JOIN_SPILL_DEPTH: usize = 4;

const CANCEL_CHECK_MASK: usize = 1023;
const SPILL_FRAME_MIN_BYTES: usize = 4096;

/// Physical join type executed by [`execute_hash_join`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    /// Inner equi-join.
    Inner,
    /// Left outer join (unmatched left rows padded with `NULL`s on the right).
    Left,
    /// Right outer join (unmatched right rows padded with `NULL`s on the left).
    Right,
    /// Full outer join (unmatched rows on either side padded with `NULL`s).
    Full,
    /// Left semi-join (emits each matching left row at most once; output schema is `left` only).
    LeftSemi,
    /// Left anti-join (emits each non-matching left row once; output schema is `left` only).
    LeftAnti,
    /// Cartesian product (empty keys; unspillable build buffer).
    Cross,
}

/// Specification for [`execute_hash_join`].
#[derive(Debug, Clone, PartialEq)]
pub struct HashJoinSpec {
    /// Join variant.
    pub join_type: JoinType,
    /// Equi-join key expressions evaluated on the left (probe) side (empty iff [`JoinType::Cross`]).
    pub left_keys: Vec<Expr>,
    /// Equi-join key expressions evaluated on the right (build) side (empty iff [`JoinType::Cross`]).
    pub right_keys: Vec<Expr>,
    /// Whether `NULL` keys compare equal (`false` for SQL `ON a = b`, `true` for `INTERSECT`/`EXCEPT`).
    pub null_equals_null: bool,
    /// Optional non-equi residual predicate bound against combined `(left_cols..., right_cols...)` schema.
    pub residual_filter: Option<Expr>,
}

/// Computes the output schema of a join between `left` and `right`.
///
/// - [`JoinType::LeftSemi`] / [`JoinType::LeftAnti`]: returns `left` schema only.
/// - [`JoinType::Inner`] / [`JoinType::Cross`]: concatenates `left` + `right` unchanged.
/// - [`JoinType::Left`] / [`JoinType::Right`] / [`JoinType::Full`]: concatenates `left` + `right`,
///   widening non-matching side(s) to `nullable: true`.
pub fn join_output_schema(join_type: JoinType, left: &Schema, right: &Schema) -> Arc<Schema> {
    if matches!(join_type, JoinType::LeftSemi | JoinType::LeftAnti) {
        return Arc::new(left.clone());
    }
    let widen_left = matches!(join_type, JoinType::Right | JoinType::Full);
    let widen_right = matches!(join_type, JoinType::Left | JoinType::Full);
    let mut fields = Vec::with_capacity(left.len() + right.len());
    for f in left.fields() {
        fields.push(Field::new(
            f.name.clone(),
            f.data_type.clone(),
            f.nullable || widen_left,
        ));
    }
    for f in right.fields() {
        fields.push(Field::new(
            f.name.clone(),
            f.data_type.clone(),
            f.nullable || widen_right,
        ));
    }
    Schema::try_new_arc(fields).unwrap_or_else(|_| Arc::new(left.clone()))
}

/// Executes a join between `left` (probe) and `right` (build) partition streams, returning
/// a single [`PartitionStream`] with schema [`join_output_schema`].
pub fn execute_hash_join(
    left: Vec<Box<dyn PartitionStream>>,
    left_schema: Arc<Schema>,
    right: Vec<Box<dyn PartitionStream>>,
    right_schema: Arc<Schema>,
    spec: HashJoinSpec,
    ctx: &ExecContext,
) -> Result<Box<dyn PartitionStream>, QueryError> {
    validate_spec(&spec, &left_schema, &right_schema)?;

    let output_schema = join_output_schema(spec.join_type, &left_schema, &right_schema);
    let combined_schema = if matches!(spec.join_type, JoinType::LeftSemi | JoinType::LeftAnti) {
        join_output_schema(JoinType::Inner, &left_schema, &right_schema)
    } else {
        Arc::clone(&output_schema)
    };

    let left_key_evals = compile_key_evals(&spec.left_keys, &left_schema)?;
    let right_key_evals = compile_key_evals(&spec.right_keys, &right_schema)?;
    let batch_size = ctx.batch_size.max(1);

    let phase = if spec.join_type == JoinType::Cross {
        let build_res = ctx.pool.register("cross_join_build", false);
        Phase::Cross(Box::new(CrossState {
            right_streams: right.into(),
            left_streams: left.into(),
            right_batches: Vec::new(),
            total_right_rows: 0,
            build_res,
            built: false,
            cur_left: None,
            left_row: 0,
            right_batch_idx: 0,
            right_row: 0,
        }))
    } else {
        let build_res = ctx.pool.register("hash_join_build", true);
        Phase::BuildRight(Box::new(BuildRightState {
            right_streams: right.into(),
            left_streams: Some(left.into()),
            buffered_batches: Vec::new(),
            buffered_computed_keys: Vec::new(),
            buffered_rows: 0,
            max_batch_bytes: 0,
            spilled: false,
            build_writers: new_writer_array(),
            build_res,
        }))
    };

    let pull_res = ctx.pool.register("hash_join_pull", false);
    let scratch_res = ctx.pool.register("hash_join_scratch", false);

    Ok(Box::new(HashJoinStream {
        spec,
        left_schema,
        right_schema,
        output_schema,
        combined_schema,
        left_key_evals,
        right_key_evals,
        ctx: ctx.clone(),
        batch_size,
        pull_res,
        scratch_res,
        pending_out: VecDeque::new(),
        phase,
    }))
}

fn validate_spec(
    spec: &HashJoinSpec,
    left_schema: &Schema,
    right_schema: &Schema,
) -> Result<(), QueryError> {
    if spec.join_type == JoinType::Cross {
        if !spec.left_keys.is_empty() || !spec.right_keys.is_empty() {
            return Err(QueryError::failed("cross join cannot have equi-join keys"));
        }
    } else if spec.left_keys.is_empty() || spec.left_keys.len() != spec.right_keys.len() {
        return Err(QueryError::failed(
            "hash join requires equal non-empty left and right key lists",
        ));
    }

    for (lk, rk) in spec.left_keys.iter().zip(&spec.right_keys) {
        if !keys_type_compatible(&lk.data_type(), &rk.data_type()) {
            return Err(QueryError::failed(
                "incompatible join key types between left and right",
            ));
        }
    }

    if let Some(pred) = &spec.residual_filter {
        if !matches!(pred.data_type(), DataType::Bool | DataType::Null) {
            return Err(QueryError::failed(
                "join residual filter must evaluate to boolean",
            ));
        }
        let mut refs = BTreeSet::new();
        pred.referenced_columns(&mut refs);
        let total_cols = left_schema.len() + right_schema.len();
        if let Some(&oob) = refs.iter().find(|&&idx| idx >= total_cols) {
            return Err(QueryError::internal(format!(
                "join residual filter column index {oob} out of bounds"
            )));
        }
    }
    Ok(())
}

fn keys_type_compatible(a: &DataType, b: &DataType) -> bool {
    match (a, b) {
        (DataType::Null, _) | (_, DataType::Null) => true,
        (DataType::Decimal128 { .. }, DataType::Decimal128 { .. }) => true,
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => true,
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => true,
        (DataType::List(ia), DataType::List(ib)) => keys_type_compatible(ia, ib),
        _ => a == b,
    }
}

#[derive(Clone)]
enum KeyEval {
    Column { index: usize, data_type: DataType },
    Computed(Expr),
}

fn compile_key_evals(exprs: &[Expr], schema: &Schema) -> Result<Vec<KeyEval>, QueryError> {
    let mut out = Vec::with_capacity(exprs.len());
    for expr in exprs {
        if let Expr::Column {
            index, data_type, ..
        } = expr
        {
            let field = schema.fields().get(*index).ok_or_else(|| {
                QueryError::internal(format!("join key column index {index} out of bounds"))
            })?;
            if &field.data_type != data_type {
                return Err(QueryError::failed("join key column type mismatch"));
            }
            out.push(KeyEval::Column {
                index: *index,
                data_type: data_type.clone(),
            });
        } else {
            out.push(KeyEval::Computed(expr.clone()));
        }
    }
    Ok(out)
}

fn evaluate_owned_keys(
    evals: &[KeyEval],
    batch: &Batch,
    eval_ctx: &EvalContext,
    mut res: Option<&mut Reservation>,
) -> Result<Vec<Option<Column>>, QueryError> {
    let mut out = Vec::with_capacity(evals.len());
    for ev in evals {
        match ev {
            KeyEval::Column { index, data_type } => {
                let col = batch.column(*index).ok_or(ColumnError::InvalidBatch {
                    reason: "join key column index out of bounds",
                })?;
                if &col.data_type() != data_type {
                    return Err(ColumnError::InvalidBatch {
                        reason: "join key column type mismatch",
                    }
                    .into());
                }
                out.push(None);
            }
            KeyEval::Computed(expr) => {
                let col = eval_expr(expr, batch, eval_ctx, res.as_deref_mut())?;
                out.push(Some(col));
            }
        }
    }
    Ok(out)
}

fn resolve_key_refs<'a>(
    evals: &[KeyEval],
    batch: &'a Batch,
    computed: &'a [Option<Column>],
) -> Result<Vec<&'a Column>, QueryError> {
    let mut refs = Vec::with_capacity(evals.len());
    for (k, ev) in evals.iter().enumerate() {
        match ev {
            KeyEval::Column { index, .. } => {
                let col = batch.column(*index).ok_or(ColumnError::InvalidBatch {
                    reason: "join key column index out of bounds",
                })?;
                refs.push(col);
            }
            KeyEval::Computed(_) => {
                let col = computed
                    .get(k)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| QueryError::internal("missing computed join key column"))?;
                refs.push(col);
            }
        }
    }
    Ok(refs)
}

// ---------------------------------------------------------------------------
// Key hashing and equality
// ---------------------------------------------------------------------------

#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[inline]
fn hash_seed(depth: usize) -> u64 {
    splitmix64(
        (depth as u64)
            .wrapping_add(1)
            .wrapping_mul(0x517C_C1B7_2722_0A95),
    )
}

#[inline]
fn mix_u64(state: u64, v: u64) -> u64 {
    splitmix64(state ^ v)
}

fn mix_bytes(mut state: u64, bytes: &[u8]) -> u64 {
    state = mix_u64(state, bytes.len() as u64);
    let (chunks, rem) = bytes.as_chunks::<8>();
    for chunk in chunks {
        state = mix_u64(state, u64::from_le_bytes(*chunk));
    }
    if !rem.is_empty() {
        let mut tail = [0u8; 8];
        tail[..rem.len()].copy_from_slice(rem);
        state = mix_u64(state, u64::from_le_bytes(tail));
    }
    state
}

#[inline]
fn canonical_decimal(mut value: i128, mut scale: i8) -> (i128, i8) {
    if value == 0 {
        return (0, 0);
    }
    while value % 10 == 0 && scale > i8::MIN {
        value /= 10;
        scale -= 1;
    }
    (value, scale)
}

fn hash_elem_ref(mut state: u64, elem: ElemRef<'_>) -> u64 {
    if elem.is_null() {
        return mix_u64(state, 0xA5A5_5A5A_0000_0000);
    }
    state = mix_u64(state, 1);
    match elem.data_type() {
        DataType::Null => mix_u64(state, 0),
        DataType::Bool => mix_u64(state, u64::from(elem.read_bool().unwrap_or(false))),
        DataType::I64 => mix_u64(state, elem.read_i64().unwrap_or(0) as u64),
        DataType::F64 => {
            let bits = f64_canonical_bits(elem.read_f64().unwrap_or(0.0));
            mix_u64(state, bits)
        }
        DataType::Decimal128 { .. } => {
            let (v, s) = elem.read_decimal().unwrap_or((0, 0));
            let (cv, cs) = canonical_decimal(v, s);
            state = mix_u64(state, cv as u64);
            state = mix_u64(state, (cv >> 64) as u64);
            mix_u64(state, cs as i64 as u64)
        }
        DataType::TimestampNs { .. } => mix_u64(state, elem.read_timestamp().unwrap_or(0) as u64),
        DataType::Date => mix_u64(state, elem.read_date().unwrap_or(0) as i64 as u64),
        DataType::Interval => {
            let iv = elem.read_interval().unwrap_or_default();
            state = mix_u64(state, iv.months as i64 as u64);
            state = mix_u64(state, iv.days as i64 as u64);
            mix_u64(state, iv.nanos as u64)
        }
        DataType::Utf8 => mix_bytes(state, elem.read_str().unwrap_or("").as_bytes()),
        DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
            mix_bytes(state, elem.read_bytes().unwrap_or(&[]))
        }
        DataType::List(_) => {
            if let Some(lref) = elem.as_list_ref() {
                let len = list_ref_len(lref);
                state = mix_u64(state, len as u64);
                for i in 0..len {
                    state = hash_elem_ref(state, list_ref_elem(lref, i));
                }
            }
            state
        }
        DataType::MapUtf8 => {
            if let Some(mref) = elem.as_map_ref() {
                match mref {
                    MapRef::Const(entries) => {
                        state = mix_u64(state, entries.len() as u64);
                        for (k, v) in entries {
                            state = mix_bytes(state, k.as_bytes());
                            match v {
                                Some(vs) => {
                                    state = mix_u64(state, 1);
                                    state = mix_bytes(state, vs.as_bytes());
                                }
                                None => state = mix_u64(state, 0),
                            }
                        }
                    }
                    MapRef::Flat { data, start, end } => {
                        state = mix_u64(state, (end - start) as u64);
                        for idx in start..end {
                            state = mix_bytes(state, data.keys().get(idx).unwrap_or("").as_bytes());
                            if data.value_nulls().is_some_and(|bm| !bm.get(idx)) {
                                state = mix_u64(state, 0);
                            } else {
                                state = mix_u64(state, 1);
                                state = mix_bytes(
                                    state,
                                    data.values().get(idx).unwrap_or("").as_bytes(),
                                );
                            }
                        }
                    }
                }
            }
            state
        }
        DataType::Events => {
            if let Some(eref) = elem.as_events_ref() {
                match eref {
                    EventsRef::Const(evs) => {
                        state = mix_u64(state, evs.len() as u64);
                        for ev in evs {
                            state = mix_u64(state, ev.time as u64);
                            state = mix_bytes(state, ev.name.as_bytes());
                            match &ev.attributes {
                                Some(a) => {
                                    state = mix_u64(state, 1);
                                    state = mix_bytes(state, a.as_bytes());
                                }
                                None => state = mix_u64(state, 0),
                            }
                        }
                    }
                    EventsRef::Flat { data, start, end } => {
                        state = mix_u64(state, (end - start) as u64);
                        for idx in start..end {
                            state = mix_u64(state, data.times()[idx] as u64);
                            state =
                                mix_bytes(state, data.names().get(idx).unwrap_or("").as_bytes());
                            if data.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
                                state = mix_u64(state, 0);
                            } else {
                                state = mix_u64(state, 1);
                                state = mix_bytes(
                                    state,
                                    data.attributes().get(idx).unwrap_or("").as_bytes(),
                                );
                            }
                        }
                    }
                }
            }
            state
        }
        DataType::Links => {
            if let Some(lref) = elem.as_links_ref() {
                match lref {
                    LinksRef::Const(lks) => {
                        state = mix_u64(state, lks.len() as u64);
                        for lk in lks {
                            state = mix_bytes(state, &lk.trace_id);
                            state = mix_bytes(state, &lk.span_id);
                            match &lk.attributes {
                                Some(a) => {
                                    state = mix_u64(state, 1);
                                    state = mix_bytes(state, a.as_bytes());
                                }
                                None => state = mix_u64(state, 0),
                            }
                        }
                    }
                    LinksRef::Flat { data, start, end } => {
                        state = mix_u64(state, (end - start) as u64);
                        for idx in start..end {
                            state = mix_bytes(state, &data.trace_ids()[idx]);
                            state = mix_bytes(state, &data.span_ids()[idx]);
                            if data.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
                                state = mix_u64(state, 0);
                            } else {
                                state = mix_u64(state, 1);
                                state = mix_bytes(
                                    state,
                                    data.attributes().get(idx).unwrap_or("").as_bytes(),
                                );
                            }
                        }
                    }
                }
            }
            state
        }
    }
}

fn hash_key_row(cols: &[&Column], row: usize, seed: u64) -> u64 {
    let mut state = seed;
    for col in cols {
        let elem = if col.is_null(row) {
            ElemRef::Null
        } else {
            ElemRef::Col(col, row)
        };
        state = hash_elem_ref(state, elem);
    }
    splitmix64(state)
}

fn row_has_null_key(cols: &[&Column], row: usize) -> bool {
    cols.iter().any(|c| c.is_null(row))
}

fn elem_eq(ea: ElemRef<'_>, eb: ElemRef<'_>) -> Result<bool, QueryError> {
    match (ea.is_null(), eb.is_null()) {
        (true, true) => return Ok(true),
        (true, false) | (false, true) => return Ok(false),
        (false, false) => {}
    }
    match (ea.data_type(), eb.data_type()) {
        (DataType::Null, DataType::Null) => Ok(true),
        (DataType::Bool, DataType::Bool) => Ok(ea.read_bool() == eb.read_bool()),
        (DataType::I64, DataType::I64) => Ok(ea.read_i64() == eb.read_i64()),
        (DataType::F64, DataType::F64) => match (ea.read_f64(), eb.read_f64()) {
            (Some(x), Some(y)) => Ok(f64_cmp(x, y) == Ordering::Equal),
            _ => Ok(false),
        },
        (DataType::Decimal128 { .. }, DataType::Decimal128 { .. }) => {
            match (ea.read_decimal(), eb.read_decimal()) {
                (Some((x, s1)), Some((y, s2))) => {
                    Ok(compare_decimals(x, s1, y, s2)? == Ordering::Equal)
                }
                _ => Ok(false),
            }
        }
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => {
            Ok(ea.read_timestamp() == eb.read_timestamp())
        }
        (DataType::Date, DataType::Date) => Ok(ea.read_date() == eb.read_date()),
        (DataType::Interval, DataType::Interval) => Ok(ea.read_interval() == eb.read_interval()),
        (DataType::Utf8, DataType::Utf8) => Ok(ea.read_str() == eb.read_str()),
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => Ok(ea.read_bytes() == eb.read_bytes()),
        (DataType::List(_), DataType::List(_)) => {
            let (Some(la), Some(lb)) = (
                match ea {
                    ElemRef::Col(c, r) => list_ref_at(c, r),
                    _ => ea.as_list_ref(),
                },
                match eb {
                    ElemRef::Col(c, r) => list_ref_at(c, r),
                    _ => eb.as_list_ref(),
                },
            ) else {
                return Ok(false);
            };
            let len_a = list_ref_len(la);
            let len_b = list_ref_len(lb);
            if len_a != len_b {
                return Ok(false);
            }
            for i in 0..len_a {
                if !elem_eq(list_ref_elem(la, i), list_ref_elem(lb, i))? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(ea.to_scalar().semantic_eq(&eb.to_scalar())),
    }
}

fn keys_equal(
    left_cols: &[&Column],
    left_row: usize,
    right_cols: &[&Column],
    right_row: usize,
    null_equals_null: bool,
) -> Result<bool, QueryError> {
    for (lc, rc) in left_cols.iter().zip(right_cols) {
        let ln = lc.is_null(left_row);
        let rn = rc.is_null(right_row);
        if ln || rn {
            if !(ln && rn && null_equals_null) {
                return Ok(false);
            }
            continue;
        }
        if !elem_eq(ElemRef::Col(lc, left_row), ElemRef::Col(rc, right_row))? {
            return Ok(false);
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// BuildTable & Grace partitioning helpers
// ---------------------------------------------------------------------------

struct BuildTable {
    batches: Vec<Batch>,
    computed_keys: Vec<Vec<Option<Column>>>,
    row_locs: Vec<(u32, u32)>,
    row_hashes: Vec<u64>,
    buckets: Vec<u32>,
    next: Vec<u32>,
    matched: Option<Bitmap>,
}

struct BuildTableConfig<'a> {
    total_rows: usize,
    num_buckets: usize,
    key_evals: &'a [KeyEval],
    null_equals_null: bool,
    track_matched: bool,
    seed: u64,
}

impl BuildTable {
    fn key_cols_for_batch<'a>(
        &'a self,
        batch_idx: usize,
        key_evals: &[KeyEval],
    ) -> Result<Vec<&'a Column>, QueryError> {
        resolve_key_refs(
            key_evals,
            &self.batches[batch_idx],
            &self.computed_keys[batch_idx],
        )
    }

    fn index_bytes(total_rows: usize, track_matched: bool) -> (usize, usize) {
        let num_buckets = (total_rows.saturating_mul(2)).next_power_of_two().max(16);
        let matched_bytes = if track_matched {
            bitmap_mem_bytes(total_rows)
        } else {
            0
        };
        let bytes = total_rows
            .saturating_mul(size_of::<(u32, u32)>() + size_of::<u64>() + 4)
            .saturating_add(num_buckets.saturating_mul(4))
            .saturating_add(matched_bytes);
        (bytes, num_buckets)
    }

    fn try_reserve_index(
        total_rows: usize,
        track_matched: bool,
        headroom: usize,
        build_res: &mut Reservation,
    ) -> Result<(usize, usize), ColumnError> {
        let (index_bytes, num_buckets) = Self::index_bytes(total_rows, track_matched);
        build_res.try_grow(index_bytes.saturating_add(headroom))?;
        if headroom > 0 {
            build_res.shrink(headroom);
        }
        Ok((index_bytes, num_buckets))
    }

    fn populate_from_buffers(
        batches: Vec<Batch>,
        computed_keys: Vec<Vec<Option<Column>>>,
        cfg: &BuildTableConfig<'_>,
        cancel: &CancelToken,
    ) -> Result<Self, QueryError> {
        let mut row_locs = Vec::with_capacity(cfg.total_rows);
        let mut row_hashes = Vec::with_capacity(cfg.total_rows);
        let mut buckets = vec![0u32; cfg.num_buckets];
        let mut next = Vec::with_capacity(cfg.total_rows);
        let mask = cfg.num_buckets - 1;

        for (b_idx, batch) in batches.iter().enumerate() {
            let kcols = resolve_key_refs(cfg.key_evals, batch, &computed_keys[b_idx])?;
            for r in 0..batch.num_rows() {
                if (r & CANCEL_CHECK_MASK) == 0 && cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }
                if !cfg.null_equals_null && row_has_null_key(&kcols, r) {
                    row_locs.push((b_idx as u32, r as u32));
                    row_hashes.push(0);
                    next.push(0);
                    continue;
                }
                let h = hash_key_row(&kcols, r, cfg.seed);
                let flat_id = row_locs.len();
                row_locs.push((b_idx as u32, r as u32));
                row_hashes.push(h);
                let slot = (h as usize) & mask;
                let prev_head = buckets[slot];
                next.push(prev_head);
                buckets[slot] = (flat_id as u32) + 1;
            }
        }

        let matched = cfg
            .track_matched
            .then(|| Bitmap::new_zeroed(cfg.total_rows));
        Ok(Self {
            batches,
            computed_keys,
            row_locs,
            row_hashes,
            buckets,
            next,
            matched,
        })
    }
}

fn new_writer_array() -> [Option<SpillWriter>; NUM_JOIN_PARTITIONS] {
    std::array::from_fn(|_| None)
}

fn new_file_array() -> [Option<SpillFile>; NUM_JOIN_PARTITIONS] {
    std::array::from_fn(|_| None)
}

fn finish_writers(
    writers: &mut [Option<SpillWriter>; NUM_JOIN_PARTITIONS],
) -> Result<[Option<SpillFile>; NUM_JOIN_PARTITIONS], QueryError> {
    let mut files = new_file_array();
    for (p, slot) in writers.iter_mut().enumerate() {
        if let Some(w) = slot.take() {
            files[p] = Some(w.finish()?);
        }
    }
    Ok(files)
}

fn write_chunked_sub_batch(
    writer: &mut SpillWriter,
    batch: &Batch,
    indices: &[u32],
    frame_budget: usize,
) -> Result<(), QueryError> {
    if indices.is_empty() {
        return Ok(());
    }
    let rows_per_row_est = (batch.memory_bytes() / batch.raw_len().max(1)).max(1);
    let max_rows = (frame_budget / rows_per_row_est).max(1).min(indices.len());
    let mut start = 0usize;
    while start < indices.len() {
        let end = (start + max_rows).min(indices.len());
        let sub_idx = &indices[start..end];
        let sub = take_batch_rows(batch, sub_idx)?;
        if sub.memory_bytes() > frame_budget && sub_idx.len() > 1 {
            let half = sub_idx.len() / 2;
            write_chunked_sub_batch(writer, batch, &sub_idx[..half], frame_budget)?;
            write_chunked_sub_batch(writer, batch, &sub_idx[half..], frame_budget)?;
        } else {
            writer.write_batch(&sub)?;
        }
        start = end;
    }
    Ok(())
}

fn take_batch_rows(batch: &Batch, indices: &[u32]) -> Result<Batch, QueryError> {
    let mut cols = Vec::with_capacity(batch.columns().len());
    for col in batch.columns() {
        cols.push(Column::Flat(col.take(indices, None)?));
    }
    Ok(Batch::try_new_all(
        Arc::clone(batch.schema()),
        cols,
        indices.len(),
    )?)
}

struct PartitionSink<'a> {
    null_equals_null: bool,
    seed: u64,
    writers: &'a mut [Option<SpillWriter>; NUM_JOIN_PARTITIONS],
    spill: &'a SpillManager,
    frame_budget: usize,
    allowed_partitions: Option<&'a [bool; NUM_JOIN_PARTITIONS]>,
}

fn partition_batch_into_writers(
    batch: &Batch,
    key_cols: &[&Column],
    sink: &mut PartitionSink<'_>,
) -> Result<Vec<u32>, QueryError> {
    let mut part_indices: [Vec<u32>; NUM_JOIN_PARTITIONS] = std::array::from_fn(|_| Vec::new());
    let mut bypassed = Vec::new();

    for r in 0..batch.num_rows() {
        if !sink.null_equals_null && row_has_null_key(key_cols, r) {
            bypassed.push(r as u32);
            continue;
        }
        let h = hash_key_row(key_cols, r, sink.seed);
        let p = (h as usize) & (NUM_JOIN_PARTITIONS - 1);
        if let Some(allowed) = sink.allowed_partitions
            && !allowed[p]
        {
            bypassed.push(r as u32);
            continue;
        }
        part_indices[p].push(r as u32);
    }

    for (p, indices) in part_indices.iter().enumerate() {
        if indices.is_empty() {
            continue;
        }
        if sink.writers[p].is_none() {
            sink.writers[p] = Some(sink.spill.create_file()?);
        }
        if let Some(w) = sink.writers[p].as_mut() {
            write_chunked_sub_batch(w, batch, indices, sink.frame_budget)?;
        }
    }

    Ok(bypassed)
}

fn spill_buffered_batches(
    batches: &mut Vec<Batch>,
    computed_keys: &mut Vec<Vec<Option<Column>>>,
    key_evals: &[KeyEval],
    sink: &mut PartitionSink<'_>,
) -> Result<(), QueryError> {
    let drained_batches = std::mem::take(batches);
    let drained_keys = std::mem::take(computed_keys);
    for (batch, comp) in drained_batches.into_iter().zip(drained_keys) {
        let krefs = resolve_key_refs(key_evals, &batch, &comp)?;
        let _ = partition_batch_into_writers(&batch, &krefs, sink)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Output batch construction helpers
// ---------------------------------------------------------------------------

fn build_unmatched_left_batch(
    left_batch: &Batch,
    left_indices: &[u32],
    join_type: JoinType,
    output_schema: &Arc<Schema>,
    mut res: Option<&mut Reservation>,
) -> Result<Batch, QueryError> {
    let before = res.as_ref().map_or(0, |r| r.size());
    let n = left_indices.len();
    let mut cols = Vec::with_capacity(output_schema.len());
    for c in left_batch.columns() {
        cols.push(Column::Flat(c.take(left_indices, res.as_deref_mut())?));
    }
    if !matches!(join_type, JoinType::LeftSemi | JoinType::LeftAnti) {
        for f in &output_schema.fields()[left_batch.columns().len()..] {
            cols.push(Column::try_new_const(
                ScalarValue::Null(f.data_type.clone()),
                n,
            )?);
        }
    }
    let out = Batch::try_new_all(Arc::clone(output_schema), cols, n)?;
    reconcile_res(res, before, out.memory_bytes())?;
    Ok(out)
}

fn build_unmatched_right_batch(
    right_batch: &Batch,
    right_indices: &[u32],
    left_len: usize,
    output_schema: &Arc<Schema>,
    mut res: Option<&mut Reservation>,
) -> Result<Batch, QueryError> {
    let before = res.as_ref().map_or(0, |r| r.size());
    let n = right_indices.len();
    let mut cols = Vec::with_capacity(output_schema.len());
    for f in &output_schema.fields()[..left_len] {
        cols.push(Column::try_new_const(
            ScalarValue::Null(f.data_type.clone()),
            n,
        )?);
    }
    for c in right_batch.columns() {
        cols.push(Column::Flat(c.take(right_indices, res.as_deref_mut())?));
    }
    let out = Batch::try_new_all(Arc::clone(output_schema), cols, n)?;
    reconcile_res(res, before, out.memory_bytes())?;
    Ok(out)
}

fn build_unmatched_build_table_batch(
    table: &BuildTable,
    build_ids: &[u32],
    left_len: usize,
    output_schema: &Arc<Schema>,
    eval_ctx: &EvalContext,
    mut res: Option<&mut Reservation>,
) -> Result<Batch, QueryError> {
    let before = res.as_ref().map_or(0, |r| r.size());
    let n = build_ids.len();
    let mut cols = Vec::with_capacity(output_schema.len());
    for f in &output_schema.fields()[..left_len] {
        cols.push(Column::try_new_const(
            ScalarValue::Null(f.data_type.clone()),
            n,
        )?);
    }
    let right_fields = &output_schema.fields()[left_len..];
    for (col_idx, field) in right_fields.iter().enumerate() {
        let mut sources = Vec::with_capacity(n);
        for &bid in build_ids {
            let (b_idx, r_idx) = table.row_locs[bid as usize];
            sources.push(Some((
                &table.batches[b_idx as usize].columns()[col_idx],
                r_idx as usize,
            )));
        }
        cols.push(build_flat_from_sources(
            &field.data_type,
            &sources,
            &mut res,
            eval_ctx,
            before,
        )?);
    }
    let out = Batch::try_new_all(Arc::clone(output_schema), cols, n)?;
    reconcile_res(res, before, out.memory_bytes())?;
    Ok(out)
}

fn build_paired_batch(
    left_batch: &Batch,
    table: Option<&BuildTable>,
    pairs: &[(u32, Option<u32>)],
    schema: &Arc<Schema>,
    include_right: bool,
    eval_ctx: &EvalContext,
    mut res: Option<&mut Reservation>,
) -> Result<Batch, QueryError> {
    let before = res.as_ref().map_or(0, |r| r.size());
    let n = pairs.len();
    let left_len = left_batch.columns().len();
    let mut cols = Vec::with_capacity(schema.len());

    let l_idx: Vec<u32> = pairs.iter().map(|(l, _)| *l).collect();
    for c in left_batch.columns() {
        cols.push(Column::Flat(c.take(&l_idx, res.as_deref_mut())?));
    }

    if include_right {
        let right_fields = &schema.fields()[left_len..];
        let all_right_none = pairs.iter().all(|(_, r)| r.is_none());
        if all_right_none {
            for f in right_fields {
                cols.push(Column::try_new_const(
                    ScalarValue::Null(f.data_type.clone()),
                    n,
                )?);
            }
        } else if let Some(tbl) = table {
            for (col_idx, field) in right_fields.iter().enumerate() {
                let mut sources = Vec::with_capacity(n);
                for (_, r) in pairs {
                    match r {
                        Some(bid) => {
                            let (b_idx, r_idx) = tbl.row_locs[*bid as usize];
                            sources.push(Some((
                                &tbl.batches[b_idx as usize].columns()[col_idx],
                                r_idx as usize,
                            )));
                        }
                        None => sources.push(None),
                    }
                }
                cols.push(build_flat_from_sources(
                    &field.data_type,
                    &sources,
                    &mut res,
                    eval_ctx,
                    before,
                )?);
            }
        }
    }

    let out = Batch::try_new_all(Arc::clone(schema), cols, n)?;
    reconcile_res(res, before, out.memory_bytes())?;
    Ok(out)
}

fn rollback_res(res: Option<&mut Reservation>, before: usize) {
    if let Some(r) = res {
        let cur = r.size();
        if cur > before {
            r.shrink(cur - before);
        }
    }
}

fn reconcile_res(
    res: Option<&mut Reservation>,
    before: usize,
    target_bytes: usize,
) -> Result<(), QueryError> {
    let Some(r) = res else {
        return Ok(());
    };
    let Some(target) = before.checked_add(target_bytes) else {
        rollback_res(Some(r), before);
        return Err(QueryError::TooLarge);
    };
    let cur = r.size();
    if target > cur {
        if let Err(err) = r.try_grow(target - cur) {
            rollback_res(Some(r), before);
            return Err(err.into());
        }
    } else if target < cur {
        r.shrink(cur - target);
    }
    Ok(())
}

fn compact_for_join(batch: Batch, pull_res: &mut Reservation) -> Result<Batch, QueryError> {
    if batch.selection().is_all(batch.raw_len()) {
        return Ok(batch);
    }
    let compacted = batch.compact(Some(pull_res))?;
    drop(batch);
    let target = compacted.memory_bytes();
    if pull_res.size() > target {
        pull_res.shrink(pull_res.size() - target);
    } else if pull_res.size() < target {
        pull_res.try_grow(target - pull_res.size())?;
    }
    Ok(compacted)
}

fn pull_next_non_empty(
    streams: &mut VecDeque<Box<dyn PartitionStream>>,
    cancel: &CancelToken,
    pull_res: &mut Reservation,
) -> Result<Option<Batch>, QueryError> {
    while let Some(front) = streams.front_mut() {
        if cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        match front.next_batch(cancel, Some(pull_res))? {
            Some(batch) => {
                if batch.is_empty() {
                    drop(batch);
                    pull_res.free();
                    continue;
                }
                return compact_for_join(batch, pull_res).map(Some);
            }
            None => {
                pull_res.free();
                streams.pop_front();
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Stream state machine
// ---------------------------------------------------------------------------

struct HashJoinStream {
    spec: HashJoinSpec,
    left_schema: Arc<Schema>,
    right_schema: Arc<Schema>,
    output_schema: Arc<Schema>,
    combined_schema: Arc<Schema>,
    left_key_evals: Vec<KeyEval>,
    right_key_evals: Vec<KeyEval>,
    ctx: ExecContext,
    batch_size: usize,
    pull_res: Reservation,
    scratch_res: Reservation,
    pending_out: VecDeque<Batch>,
    phase: Phase,
}

enum Phase {
    Cross(Box<CrossState>),
    BuildRight(Box<BuildRightState>),
    PassThroughLeft(VecDeque<Box<dyn PartitionStream>>),
    SpillPartitionLeft(Box<SpillPartitionLeftState>),
    ProcessSpillTasks(SpillTasksState),
    ProbeInMemory(Box<ProbeState>),
    EmitUnmatchedBuild(EmitUnmatchedBuildState),
    EmitUnmatchedBuildFile(EmitUnmatchedBuildFileState),
    Done,
}

struct CrossState {
    right_streams: VecDeque<Box<dyn PartitionStream>>,
    left_streams: VecDeque<Box<dyn PartitionStream>>,
    right_batches: Vec<Batch>,
    total_right_rows: usize,
    build_res: Reservation,
    built: bool,
    cur_left: Option<Batch>,
    left_row: usize,
    right_batch_idx: usize,
    right_row: usize,
}

struct BuildRightState {
    right_streams: VecDeque<Box<dyn PartitionStream>>,
    left_streams: Option<VecDeque<Box<dyn PartitionStream>>>,
    buffered_batches: Vec<Batch>,
    buffered_computed_keys: Vec<Vec<Option<Column>>>,
    buffered_rows: usize,
    max_batch_bytes: usize,
    spilled: bool,
    build_writers: [Option<SpillWriter>; NUM_JOIN_PARTITIONS],
    build_res: Reservation,
}

struct SpillPartitionLeftState {
    left_streams: VecDeque<Box<dyn PartitionStream>>,
    build_files: [Option<SpillFile>; NUM_JOIN_PARTITIONS],
    probe_writers: [Option<SpillWriter>; NUM_JOIN_PARTITIONS],
    allowed_partitions: [bool; NUM_JOIN_PARTITIONS],
    build_res: Reservation,
}

enum PartitionTask {
    Join {
        build_file: SpillFile,
        probe_file: SpillFile,
        depth: usize,
    },
    EmitUnmatchedBuild(SpillFile),
}

struct SpillTasksState {
    tasks: Vec<PartitionTask>,
    build_res: Reservation,
}

struct ProbeBatchState {
    batch: Batch,
    key_cols: Vec<Option<Column>>,
    next_row: usize,
    active_bucket_next: Option<u32>,
    active_hash: u64,
    unmatched_scan_pos: usize,
    row_matched: Vec<bool>,
    row_emitted: Vec<bool>,
    row_chain_done: Vec<bool>,
}

enum ProbeSource {
    Streams(VecDeque<Box<dyn PartitionStream>>),
    Spill {
        reader: SpillReader,
        _file: SpillFile,
    },
}

struct ProbeState {
    table: BuildTable,
    source: ProbeSource,
    cur: Option<ProbeBatchState>,
    seed: u64,
    build_res: Reservation,
    remaining_tasks: Option<Vec<PartitionTask>>,
}

struct EmitUnmatchedBuildState {
    table: BuildTable,
    next_id: usize,
    build_res: Reservation,
    remaining_tasks: Option<Vec<PartitionTask>>,
}

struct RepartitionInput {
    next_depth: usize,
    frame_budget: usize,
    buffered_batches: Vec<Batch>,
    buffered_keys: Vec<Vec<Option<Column>>>,
    cur_batch: Option<(Batch, Vec<Option<Column>>)>,
    build_reader: Option<SpillReader>,
    build_file: SpillFile,
    probe_file: SpillFile,
}

struct EmitUnmatchedBuildFileState {
    reader: SpillReader,
    _file: SpillFile,
    build_res: Reservation,
    remaining_tasks: Vec<PartitionTask>,
}

impl HashJoinStream {
    fn check_cancel(&self, cancel: &CancelToken) -> Result<(), QueryError> {
        if cancel.is_cancelled() || self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        Ok(())
    }

    fn eval_ctx(&self, cancel: &CancelToken) -> EvalContext {
        EvalContext {
            now_ns: self.ctx.eval.now_ns,
            cancel: if cancel.is_cancelled() {
                cancel.clone()
            } else {
                self.ctx.eval.cancel.clone()
            },
        }
    }

    fn frame_budget(&self) -> usize {
        (self.ctx.pool.spill_share() / 8).max(SPILL_FRAME_MIN_BYTES)
    }

    fn pull_headroom(max_batch_bytes: usize, frame_budget: usize) -> usize {
        max_batch_bytes
            .max(frame_budget)
            .saturating_mul(2)
            .saturating_add(1024)
    }

    fn queue_sliced_batch(&mut self, batch: Batch) -> Result<(), QueryError> {
        if batch.is_empty() {
            return Ok(());
        }
        if batch.num_rows() <= self.batch_size {
            self.pending_out.push_back(batch);
            return Ok(());
        }
        let mut offset = 0usize;
        while offset < batch.num_rows() {
            let len = (batch.num_rows() - offset).min(self.batch_size);
            self.pending_out.push_back(batch.slice(offset, len)?);
            offset += len;
        }
        Ok(())
    }

    fn pop_pending(&mut self, res: Option<&mut Reservation>) -> Result<Option<Batch>, QueryError> {
        let Some(b) = self.pending_out.pop_front() else {
            return Ok(None);
        };
        let before = res.as_ref().map_or(0, |r| r.size());
        reconcile_res(res, before, b.memory_bytes())?;
        Ok(Some(b))
    }

    fn cleanup_reservations(&mut self) {
        self.pull_res.free();
        self.scratch_res.free();
        self.pending_out.clear();
    }

    fn step_cross(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let Phase::Cross(mut st) = std::mem::replace(&mut self.phase, Phase::Done) else {
            return Ok(None);
        };
        let eval_ctx = self.eval_ctx(cancel);

        if !st.built {
            while let Some(batch) =
                pull_next_non_empty(&mut st.right_streams, cancel, &mut self.pull_res)?
            {
                self.check_cancel(cancel)?;
                let bytes = batch.memory_bytes();
                self.pull_res.free();
                st.build_res.try_grow(bytes)?;
                st.total_right_rows = st.total_right_rows.saturating_add(batch.num_rows());
                st.right_batches.push(batch);
            }
            st.built = true;
            if st.total_right_rows == 0 {
                st.build_res.free();
                self.phase = Phase::Done;
                return Ok(None);
            }
        }

        loop {
            self.check_cancel(cancel)?;
            if st.cur_left.is_none() {
                match pull_next_non_empty(&mut st.left_streams, cancel, &mut self.pull_res)? {
                    Some(lb) => {
                        st.cur_left = Some(lb);
                        st.left_row = 0;
                        st.right_batch_idx = 0;
                        st.right_row = 0;
                    }
                    None => {
                        st.build_res.free();
                        self.phase = Phase::Done;
                        return Ok(None);
                    }
                }
            }

            let Some(lb) = st.cur_left.as_ref() else {
                continue;
            };
            let mut left_idx = Vec::with_capacity(self.batch_size);
            let mut right_refs: Vec<(usize, usize)> = Vec::with_capacity(self.batch_size);

            while left_idx.len() < self.batch_size && st.left_row < lb.num_rows() {
                let rb = &st.right_batches[st.right_batch_idx];
                while left_idx.len() < self.batch_size && st.right_row < rb.num_rows() {
                    left_idx.push(st.left_row as u32);
                    right_refs.push((st.right_batch_idx, st.right_row));
                    st.right_row += 1;
                }
                if st.right_row >= rb.num_rows() {
                    st.right_row = 0;
                    st.right_batch_idx += 1;
                    if st.right_batch_idx >= st.right_batches.len() {
                        st.right_batch_idx = 0;
                        st.left_row += 1;
                    }
                }
            }

            let left_done = st.left_row >= lb.num_rows();
            let out =
                self.build_cross_batch(lb, &left_idx, &st.right_batches, &right_refs, &eval_ctx)?;
            if left_done {
                st.cur_left = None;
                self.pull_res.free();
            }

            let final_out = if let Some(pred) = &self.spec.residual_filter {
                let mask = eval_filter(pred, &out, &eval_ctx, Some(&mut self.scratch_res))?;
                let filtered = out.into_filter_mask(&mask)?;
                drop(mask);
                self.scratch_res.free();
                if filtered.is_empty() {
                    continue;
                }
                filtered.compact(Some(&mut self.scratch_res))?
            } else {
                out
            };
            self.scratch_res.free();

            let before = res.as_ref().map_or(0, |r| r.size());
            reconcile_res(res, before, final_out.memory_bytes())?;
            self.phase = Phase::Cross(st);
            return Ok(Some(final_out));
        }
    }

    fn build_cross_batch(
        &mut self,
        left_batch: &Batch,
        left_idx: &[u32],
        right_batches: &[Batch],
        right_refs: &[(usize, usize)],
        eval_ctx: &EvalContext,
    ) -> Result<Batch, QueryError> {
        let before = self.scratch_res.size();
        let n = left_idx.len();
        let mut cols = Vec::with_capacity(self.output_schema.len());
        for c in left_batch.columns() {
            cols.push(Column::Flat(c.take(left_idx, Some(&mut self.scratch_res))?));
        }
        let right_fields = &self.output_schema.fields()[left_batch.columns().len()..];
        for (col_idx, field) in right_fields.iter().enumerate() {
            let mut sources = Vec::with_capacity(n);
            for &(rb_idx, r_row) in right_refs {
                sources.push(Some((&right_batches[rb_idx].columns()[col_idx], r_row)));
            }
            let mut sref = Some(&mut self.scratch_res);
            cols.push(build_flat_from_sources(
                &field.data_type,
                &sources,
                &mut sref,
                eval_ctx,
                before,
            )?);
        }
        Ok(Batch::try_new_all(
            Arc::clone(&self.output_schema),
            cols,
            n,
        )?)
    }

    fn step_build_right(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let Phase::BuildRight(mut st) = std::mem::replace(&mut self.phase, Phase::Done) else {
            return Ok(());
        };
        let eval_ctx = self.eval_ctx(cancel);
        let seed = hash_seed(0);
        let frame_budget = self.frame_budget();
        let emit_null_build = matches!(self.spec.join_type, JoinType::Right | JoinType::Full);

        while let Some(batch) =
            pull_next_non_empty(&mut st.right_streams, cancel, &mut self.pull_res)?
        {
            self.check_cancel(cancel)?;
            let batch_bytes = batch.memory_bytes();
            st.max_batch_bytes = st.max_batch_bytes.max(batch_bytes);

            let key_cols_eval = evaluate_owned_keys(
                &self.right_key_evals,
                &batch,
                &eval_ctx,
                Some(&mut self.scratch_res),
            )?;

            let (non_null_batch, non_null_keys) = if !self.spec.null_equals_null {
                let krefs = resolve_key_refs(&self.right_key_evals, &batch, &key_cols_eval)?;
                let mut null_idx = Vec::new();
                let mut non_null_idx = Vec::new();
                for r in 0..batch.num_rows() {
                    if row_has_null_key(&krefs, r) {
                        null_idx.push(r as u32);
                    } else {
                        non_null_idx.push(r as u32);
                    }
                }

                if emit_null_build && !null_idx.is_empty() {
                    let unmatched = build_unmatched_right_batch(
                        &batch,
                        &null_idx,
                        self.left_schema.len(),
                        &self.output_schema,
                        Some(&mut self.scratch_res),
                    )?;
                    self.queue_sliced_batch(unmatched)?;
                }

                if non_null_idx.is_empty() {
                    drop(key_cols_eval);
                    drop(batch);
                    self.scratch_res.free();
                    self.pull_res.free();
                    if !self.pending_out.is_empty() {
                        self.phase = Phase::BuildRight(st);
                        return Ok(());
                    }
                    continue;
                } else if null_idx.is_empty() {
                    (batch, key_cols_eval)
                } else {
                    let sub_b = take_batch_rows(&batch, &non_null_idx)?;
                    drop(batch);
                    drop(key_cols_eval);
                    let sub_keys = evaluate_owned_keys(
                        &self.right_key_evals,
                        &sub_b,
                        &eval_ctx,
                        Some(&mut self.scratch_res),
                    )?;
                    (sub_b, sub_keys)
                }
            } else {
                (batch, key_cols_eval)
            };

            let nn_bytes = non_null_batch.memory_bytes();
            let nn_key_bytes: usize = non_null_keys
                .iter()
                .filter_map(Option::as_ref)
                .map(Column::memory_bytes)
                .sum();
            let needed = nn_bytes.saturating_add(nn_key_bytes);

            self.scratch_res.free();
            self.pull_res.free();

            if !st.spilled {
                let headroom = if st.buffered_rows > 0 {
                    Self::pull_headroom(st.max_batch_bytes, frame_budget)
                } else {
                    0
                };
                match st.build_res.try_grow(needed.saturating_add(headroom)) {
                    Ok(()) => {
                        if headroom > 0 {
                            st.build_res.shrink(headroom);
                        }
                        st.buffered_rows =
                            st.buffered_rows.saturating_add(non_null_batch.num_rows());
                        st.buffered_batches.push(non_null_batch);
                        st.buffered_computed_keys.push(non_null_keys);
                    }
                    Err(ColumnError::MemoryLimit { .. }) if st.buffered_rows > 0 => {
                        st.spilled = true;
                        let mut sink = PartitionSink {
                            null_equals_null: self.spec.null_equals_null,
                            seed,
                            writers: &mut st.build_writers,
                            spill: &self.ctx.spill,
                            frame_budget,
                            allowed_partitions: None,
                        };
                        spill_buffered_batches(
                            &mut st.buffered_batches,
                            &mut st.buffered_computed_keys,
                            &self.right_key_evals,
                            &mut sink,
                        )?;
                        st.build_res.free();
                        st.buffered_rows = 0;

                        let krefs = resolve_key_refs(
                            &self.right_key_evals,
                            &non_null_batch,
                            &non_null_keys,
                        )?;
                        let _ = partition_batch_into_writers(&non_null_batch, &krefs, &mut sink)?;
                    }
                    Err(err) => return Err(err.into()),
                }
            } else {
                let krefs =
                    resolve_key_refs(&self.right_key_evals, &non_null_batch, &non_null_keys)?;
                let mut sink = PartitionSink {
                    null_equals_null: self.spec.null_equals_null,
                    seed,
                    writers: &mut st.build_writers,
                    spill: &self.ctx.spill,
                    frame_budget,
                    allowed_partitions: None,
                };
                let _ = partition_batch_into_writers(&non_null_batch, &krefs, &mut sink)?;
            }

            if !self.pending_out.is_empty() {
                self.phase = Phase::BuildRight(st);
                return Ok(());
            }
        }

        // Right streams exhausted.
        let left_streams = st.left_streams.take().unwrap_or_default();
        if !st.spilled {
            if st.buffered_rows == 0 {
                st.build_res.free();
                if matches!(
                    self.spec.join_type,
                    JoinType::Left | JoinType::Full | JoinType::LeftAnti
                ) {
                    self.phase = Phase::PassThroughLeft(left_streams);
                } else {
                    self.phase = Phase::Done;
                }
                return Ok(());
            }

            let track_matched = matches!(self.spec.join_type, JoinType::Right | JoinType::Full);
            match BuildTable::try_reserve_index(
                st.buffered_rows,
                track_matched,
                Self::pull_headroom(st.max_batch_bytes, frame_budget),
                &mut st.build_res,
            ) {
                Ok((_, num_buckets)) => {
                    let cfg = BuildTableConfig {
                        total_rows: st.buffered_rows,
                        num_buckets,
                        key_evals: &self.right_key_evals,
                        null_equals_null: self.spec.null_equals_null,
                        track_matched,
                        seed,
                    };
                    let table = BuildTable::populate_from_buffers(
                        std::mem::take(&mut st.buffered_batches),
                        std::mem::take(&mut st.buffered_computed_keys),
                        &cfg,
                        cancel,
                    )?;
                    self.phase = Phase::ProbeInMemory(Box::new(ProbeState {
                        table,
                        source: ProbeSource::Streams(left_streams),
                        cur: None,
                        seed,
                        build_res: st.build_res,
                        remaining_tasks: None,
                    }));
                }
                Err(ColumnError::MemoryLimit { .. }) if st.buffered_rows > 0 => {
                    let mut sink = PartitionSink {
                        null_equals_null: self.spec.null_equals_null,
                        seed,
                        writers: &mut st.build_writers,
                        spill: &self.ctx.spill,
                        frame_budget,
                        allowed_partitions: None,
                    };
                    spill_buffered_batches(
                        &mut st.buffered_batches,
                        &mut st.buffered_computed_keys,
                        &self.right_key_evals,
                        &mut sink,
                    )?;
                    st.build_res.free();
                    let build_files = finish_writers(&mut st.build_writers)?;
                    let mut allowed = [false; NUM_JOIN_PARTITIONS];
                    for (p, f) in build_files.iter().enumerate() {
                        allowed[p] = f.is_some();
                    }
                    self.phase = Phase::SpillPartitionLeft(Box::new(SpillPartitionLeftState {
                        left_streams,
                        build_files,
                        probe_writers: new_writer_array(),
                        allowed_partitions: allowed,
                        build_res: st.build_res,
                    }));
                }
                Err(err) => return Err(err.into()),
            }
        } else {
            let build_files = finish_writers(&mut st.build_writers)?;
            let mut allowed = [false; NUM_JOIN_PARTITIONS];
            for (p, f) in build_files.iter().enumerate() {
                allowed[p] = f.is_some();
            }
            self.phase = Phase::SpillPartitionLeft(Box::new(SpillPartitionLeftState {
                left_streams,
                build_files,
                probe_writers: new_writer_array(),
                allowed_partitions: allowed,
                build_res: st.build_res,
            }));
        }
        Ok(())
    }

    fn step_pass_through_left(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let Phase::PassThroughLeft(mut streams) = std::mem::replace(&mut self.phase, Phase::Done)
        else {
            return Ok(());
        };
        if let Some(batch) = pull_next_non_empty(&mut streams, cancel, &mut self.pull_res)? {
            let indices: Vec<u32> = (0..batch.num_rows() as u32).collect();
            let out = build_unmatched_left_batch(
                &batch,
                &indices,
                self.spec.join_type,
                &self.output_schema,
                Some(&mut self.scratch_res),
            )?;
            drop(batch);
            self.pull_res.free();
            self.scratch_res.free();
            self.queue_sliced_batch(out)?;
            self.phase = Phase::PassThroughLeft(streams);
        } else {
            self.phase = Phase::Done;
        }
        Ok(())
    }

    fn step_spill_partition_left(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let Phase::SpillPartitionLeft(mut st) = std::mem::replace(&mut self.phase, Phase::Done)
        else {
            return Ok(());
        };
        let eval_ctx = self.eval_ctx(cancel);
        let seed = hash_seed(0);
        let frame_budget = self.frame_budget();
        let emit_unmatched_left = matches!(
            self.spec.join_type,
            JoinType::Left | JoinType::Full | JoinType::LeftAnti
        );

        while let Some(batch) =
            pull_next_non_empty(&mut st.left_streams, cancel, &mut self.pull_res)?
        {
            self.check_cancel(cancel)?;
            let key_cols_eval = evaluate_owned_keys(
                &self.left_key_evals,
                &batch,
                &eval_ctx,
                Some(&mut self.scratch_res),
            )?;
            let krefs = resolve_key_refs(&self.left_key_evals, &batch, &key_cols_eval)?;
            let mut sink = PartitionSink {
                null_equals_null: self.spec.null_equals_null,
                seed,
                writers: &mut st.probe_writers,
                spill: &self.ctx.spill,
                frame_budget,
                allowed_partitions: Some(&st.allowed_partitions),
            };
            let bypassed = partition_batch_into_writers(&batch, &krefs, &mut sink)?;
            drop(key_cols_eval);
            if emit_unmatched_left && !bypassed.is_empty() {
                let out = build_unmatched_left_batch(
                    &batch,
                    &bypassed,
                    self.spec.join_type,
                    &self.output_schema,
                    Some(&mut self.scratch_res),
                )?;
                self.queue_sliced_batch(out)?;
            }
            drop(batch);
            self.scratch_res.free();
            self.pull_res.free();
            if !self.pending_out.is_empty() {
                self.phase = Phase::SpillPartitionLeft(st);
                return Ok(());
            }
        }

        let mut probe_files = finish_writers(&mut st.probe_writers)?;
        let mut tasks = Vec::new();
        let emit_unmatched_build = matches!(self.spec.join_type, JoinType::Right | JoinType::Full);
        for p in (0..NUM_JOIN_PARTITIONS).rev() {
            let Some(bf) = st.build_files[p].take() else {
                continue;
            };
            if let Some(pf) = probe_files[p].take() {
                tasks.push(PartitionTask::Join {
                    build_file: bf,
                    probe_file: pf,
                    depth: 1,
                });
            } else if emit_unmatched_build {
                tasks.push(PartitionTask::EmitUnmatchedBuild(bf));
            }
        }

        self.phase = Phase::ProcessSpillTasks(SpillTasksState {
            tasks,
            build_res: st.build_res,
        });
        Ok(())
    }

    fn repartition_spill_task(
        &mut self,
        cancel: &CancelToken,
        eval_ctx: &EvalContext,
        mut input: RepartitionInput,
        build_res: &mut Reservation,
        tasks: &mut Vec<PartitionTask>,
    ) -> Result<(), QueryError> {
        let next_depth = input.next_depth;
        let frame_budget = input.frame_budget;
        let next_seed = hash_seed(next_depth);
        let mut sub_build_writers = new_writer_array();
        {
            let mut build_sink = PartitionSink {
                null_equals_null: self.spec.null_equals_null,
                seed: next_seed,
                writers: &mut sub_build_writers,
                spill: &self.ctx.spill,
                frame_budget,
                allowed_partitions: None,
            };
            spill_buffered_batches(
                &mut input.buffered_batches,
                &mut input.buffered_keys,
                &self.right_key_evals,
                &mut build_sink,
            )?;
            build_res.free();

            if let Some((batch, key_cols_eval)) = input.cur_batch {
                let krefs = resolve_key_refs(&self.right_key_evals, &batch, &key_cols_eval)?;
                let _ = partition_batch_into_writers(&batch, &krefs, &mut build_sink)?;
            }

            if let Some(reader) = input.build_reader.as_mut() {
                while let Some(rem) = reader.next_batch(Some(&mut self.pull_res))? {
                    self.check_cancel(cancel)?;
                    if rem.is_empty() {
                        drop(rem);
                        self.pull_res.free();
                        continue;
                    }
                    let rem = compact_for_join(rem, &mut self.pull_res)?;
                    let rem_keys = evaluate_owned_keys(
                        &self.right_key_evals,
                        &rem,
                        eval_ctx,
                        Some(&mut self.scratch_res),
                    )?;
                    let krefs = resolve_key_refs(&self.right_key_evals, &rem, &rem_keys)?;
                    let _ = partition_batch_into_writers(&rem, &krefs, &mut build_sink)?;
                    drop(rem_keys);
                    drop(rem);
                    self.scratch_res.free();
                    self.pull_res.free();
                }
            }
        }

        drop(input.build_reader);
        drop(input.build_file);
        let mut sub_build_files = finish_writers(&mut sub_build_writers)?;
        let mut allowed = [false; NUM_JOIN_PARTITIONS];
        for (p, f) in sub_build_files.iter().enumerate() {
            allowed[p] = f.is_some();
        }

        let emit_unmatched_left = matches!(
            self.spec.join_type,
            JoinType::Left | JoinType::Full | JoinType::LeftAnti
        );
        let emit_unmatched_build = matches!(self.spec.join_type, JoinType::Right | JoinType::Full);

        let mut probe_reader = input
            .probe_file
            .open_reader(Arc::clone(&self.left_schema))?;
        let mut sub_probe_writers = new_writer_array();
        while let Some(pbatch) = probe_reader.next_batch(Some(&mut self.pull_res))? {
            self.check_cancel(cancel)?;
            if pbatch.is_empty() {
                drop(pbatch);
                self.pull_res.free();
                continue;
            }
            let pbatch = compact_for_join(pbatch, &mut self.pull_res)?;
            let pkeys = evaluate_owned_keys(
                &self.left_key_evals,
                &pbatch,
                eval_ctx,
                Some(&mut self.scratch_res),
            )?;
            let krefs = resolve_key_refs(&self.left_key_evals, &pbatch, &pkeys)?;
            let mut probe_sink = PartitionSink {
                null_equals_null: self.spec.null_equals_null,
                seed: next_seed,
                writers: &mut sub_probe_writers,
                spill: &self.ctx.spill,
                frame_budget,
                allowed_partitions: Some(&allowed),
            };
            let bypassed = partition_batch_into_writers(&pbatch, &krefs, &mut probe_sink)?;
            drop(pkeys);
            if emit_unmatched_left && !bypassed.is_empty() {
                let out = build_unmatched_left_batch(
                    &pbatch,
                    &bypassed,
                    self.spec.join_type,
                    &self.output_schema,
                    Some(&mut self.scratch_res),
                )?;
                self.queue_sliced_batch(out)?;
            }
            drop(pbatch);
            self.scratch_res.free();
            self.pull_res.free();
        }
        drop(probe_reader);
        drop(input.probe_file);
        let mut sub_probe_files = finish_writers(&mut sub_probe_writers)?;

        for p in (0..NUM_JOIN_PARTITIONS).rev() {
            let Some(sbf) = sub_build_files[p].take() else {
                continue;
            };
            if let Some(spf) = sub_probe_files[p].take() {
                tasks.push(PartitionTask::Join {
                    build_file: sbf,
                    probe_file: spf,
                    depth: next_depth,
                });
            } else if emit_unmatched_build {
                tasks.push(PartitionTask::EmitUnmatchedBuild(sbf));
            }
        }
        Ok(())
    }

    fn step_process_spill_tasks(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let Phase::ProcessSpillTasks(mut st) = std::mem::replace(&mut self.phase, Phase::Done)
        else {
            return Ok(());
        };
        let eval_ctx = self.eval_ctx(cancel);
        let frame_budget = self.frame_budget();
        let emit_unmatched_build = matches!(self.spec.join_type, JoinType::Right | JoinType::Full);
        let track_matched = emit_unmatched_build;

        while let Some(task) = st.tasks.pop() {
            self.check_cancel(cancel)?;
            match task {
                PartitionTask::EmitUnmatchedBuild(bf) => {
                    let reader = bf.open_reader(Arc::clone(&self.right_schema))?;
                    self.phase = Phase::EmitUnmatchedBuildFile(EmitUnmatchedBuildFileState {
                        reader,
                        _file: bf,
                        build_res: st.build_res,
                        remaining_tasks: st.tasks,
                    });
                    return Ok(());
                }
                PartitionTask::Join {
                    build_file,
                    probe_file,
                    depth,
                } => {
                    let mut reader = build_file.open_reader(Arc::clone(&self.right_schema))?;
                    let mut buffered_batches = Vec::new();
                    let mut buffered_keys = Vec::new();
                    let mut buffered_rows = 0usize;
                    let mut max_batch_bytes = 0usize;
                    let mut overflow_cur: Option<(Batch, Vec<Option<Column>>)> = None;

                    while let Some(batch) = reader.next_batch(Some(&mut self.pull_res))? {
                        self.check_cancel(cancel)?;
                        if batch.is_empty() {
                            drop(batch);
                            self.pull_res.free();
                            continue;
                        }
                        let batch = compact_for_join(batch, &mut self.pull_res)?;
                        max_batch_bytes = max_batch_bytes.max(batch.memory_bytes());
                        let key_cols_eval = evaluate_owned_keys(
                            &self.right_key_evals,
                            &batch,
                            &eval_ctx,
                            Some(&mut self.scratch_res),
                        )?;
                        let needed = batch.memory_bytes().saturating_add(
                            key_cols_eval
                                .iter()
                                .filter_map(Option::as_ref)
                                .map(Column::memory_bytes)
                                .sum(),
                        );
                        self.scratch_res.free();
                        self.pull_res.free();

                        let headroom = if buffered_rows > 0 {
                            Self::pull_headroom(max_batch_bytes, frame_budget)
                        } else {
                            0
                        };
                        match st.build_res.try_grow(needed.saturating_add(headroom)) {
                            Ok(()) => {
                                if headroom > 0 {
                                    st.build_res.shrink(headroom);
                                }
                                buffered_rows = buffered_rows.saturating_add(batch.num_rows());
                                buffered_batches.push(batch);
                                buffered_keys.push(key_cols_eval);
                            }
                            Err(ColumnError::MemoryLimit { .. }) if buffered_rows > 0 => {
                                overflow_cur = Some((batch, key_cols_eval));
                                break;
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }

                    if let Some(cur_pair) = overflow_cur {
                        if depth >= MAX_JOIN_SPILL_DEPTH {
                            return Err(QueryError::TooLarge);
                        }
                        self.repartition_spill_task(
                            cancel,
                            &eval_ctx,
                            RepartitionInput {
                                next_depth: depth + 1,
                                frame_budget,
                                buffered_batches,
                                buffered_keys,
                                cur_batch: Some(cur_pair),
                                build_reader: Some(reader),
                                build_file,
                                probe_file,
                            },
                            &mut st.build_res,
                            &mut st.tasks,
                        )?;
                        if !self.pending_out.is_empty() {
                            self.phase = Phase::ProcessSpillTasks(st);
                            return Ok(());
                        }
                        continue;
                    }

                    drop(reader);
                    if buffered_rows == 0 {
                        drop(build_file);
                        drop(probe_file);
                        continue;
                    }

                    match BuildTable::try_reserve_index(
                        buffered_rows,
                        track_matched,
                        Self::pull_headroom(max_batch_bytes, frame_budget),
                        &mut st.build_res,
                    ) {
                        Ok((_, num_buckets)) => {
                            drop(build_file);
                            let seed = hash_seed(depth);
                            let cfg = BuildTableConfig {
                                total_rows: buffered_rows,
                                num_buckets,
                                key_evals: &self.right_key_evals,
                                null_equals_null: self.spec.null_equals_null,
                                track_matched,
                                seed,
                            };
                            let table = BuildTable::populate_from_buffers(
                                buffered_batches,
                                buffered_keys,
                                &cfg,
                                cancel,
                            )?;
                            let probe_reader =
                                probe_file.open_reader(Arc::clone(&self.left_schema))?;
                            self.phase = Phase::ProbeInMemory(Box::new(ProbeState {
                                table,
                                source: ProbeSource::Spill {
                                    reader: probe_reader,
                                    _file: probe_file,
                                },
                                cur: None,
                                seed,
                                build_res: st.build_res,
                                remaining_tasks: Some(st.tasks),
                            }));
                            return Ok(());
                        }
                        Err(ColumnError::MemoryLimit { .. }) if buffered_rows > 0 => {
                            if depth >= MAX_JOIN_SPILL_DEPTH {
                                return Err(QueryError::TooLarge);
                            }
                            self.repartition_spill_task(
                                cancel,
                                &eval_ctx,
                                RepartitionInput {
                                    next_depth: depth + 1,
                                    frame_budget,
                                    buffered_batches,
                                    buffered_keys,
                                    cur_batch: None,
                                    build_reader: None,
                                    build_file,
                                    probe_file,
                                },
                                &mut st.build_res,
                                &mut st.tasks,
                            )?;
                            if !self.pending_out.is_empty() {
                                self.phase = Phase::ProcessSpillTasks(st);
                                return Ok(());
                            }
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
            }
        }

        st.build_res.free();
        self.phase = Phase::Done;
        Ok(())
    }

    fn step_probe_in_memory(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let Phase::ProbeInMemory(mut st) = std::mem::replace(&mut self.phase, Phase::Done) else {
            return Ok(());
        };
        let eval_ctx = self.eval_ctx(cancel);
        let include_right = !matches!(self.spec.join_type, JoinType::LeftSemi | JoinType::LeftAnti);
        let emit_unmatched_left = matches!(
            self.spec.join_type,
            JoinType::Left | JoinType::Full | JoinType::LeftAnti
        );

        loop {
            self.check_cancel(cancel)?;
            if st.cur.is_none() {
                let next_batch = match &mut st.source {
                    ProbeSource::Streams(streams) => {
                        pull_next_non_empty(streams, cancel, &mut self.pull_res)?
                    }
                    ProbeSource::Spill { reader, .. } => {
                        let mut got = None;
                        while let Some(b) = reader.next_batch(Some(&mut self.pull_res))? {
                            if b.is_empty() {
                                drop(b);
                                self.pull_res.free();
                                continue;
                            }
                            got = Some(compact_for_join(b, &mut self.pull_res)?);
                            break;
                        }
                        got
                    }
                };

                let Some(batch) = next_batch else {
                    if matches!(self.spec.join_type, JoinType::Right | JoinType::Full) {
                        self.phase = Phase::EmitUnmatchedBuild(EmitUnmatchedBuildState {
                            table: st.table,
                            next_id: 0,
                            build_res: st.build_res,
                            remaining_tasks: st.remaining_tasks,
                        });
                    } else {
                        drop(st.table);
                        st.build_res.free();
                        if let Some(tasks) = st.remaining_tasks {
                            self.phase = Phase::ProcessSpillTasks(SpillTasksState {
                                tasks,
                                build_res: st.build_res,
                            });
                        } else {
                            self.phase = Phase::Done;
                        }
                    }
                    return Ok(());
                };

                let key_cols = evaluate_owned_keys(
                    &self.left_key_evals,
                    &batch,
                    &eval_ctx,
                    Some(&mut self.scratch_res),
                )?;
                let n = batch.num_rows();
                st.cur = Some(ProbeBatchState {
                    batch,
                    key_cols,
                    next_row: 0,
                    active_bucket_next: None,
                    active_hash: 0,
                    unmatched_scan_pos: 0,
                    row_matched: vec![false; n],
                    row_emitted: vec![false; n],
                    row_chain_done: vec![false; n],
                });
            }

            let Some(cur) = st.cur.as_mut() else {
                continue;
            };
            let l_len = cur.batch.num_rows();
            let l_krefs = resolve_key_refs(&self.left_key_evals, &cur.batch, &cur.key_cols)?;
            let bucket_mask = st.table.buckets.len() - 1;

            if self.spec.residual_filter.is_none() {
                let mut pairs: Vec<(u32, Option<u32>)> = Vec::with_capacity(self.batch_size);
                while cur.next_row < l_len && pairs.len() < self.batch_size {
                    let lr = cur.next_row;
                    if (lr & CANCEL_CHECK_MASK) == 0 {
                        self.check_cancel(cancel)?;
                    }
                    if cur.active_bucket_next.is_none() {
                        if !self.spec.null_equals_null && row_has_null_key(&l_krefs, lr) {
                            if emit_unmatched_left {
                                pairs.push((lr as u32, None));
                            }
                            cur.next_row += 1;
                            continue;
                        }
                        let h = hash_key_row(&l_krefs, lr, st.seed);
                        let head = st.table.buckets[(h as usize) & bucket_mask];
                        if head == 0 {
                            if emit_unmatched_left {
                                pairs.push((lr as u32, None));
                            }
                            cur.next_row += 1;
                            continue;
                        }
                        cur.active_hash = h;
                        cur.active_bucket_next = Some(head);
                    }

                    let h = cur.active_hash;
                    while let Some(entry_plus_one) = cur.active_bucket_next {
                        if pairs.len() >= self.batch_size {
                            break;
                        }
                        let bid = (entry_plus_one - 1) as usize;
                        let nxt = st.table.next[bid];
                        cur.active_bucket_next = if nxt == 0 { None } else { Some(nxt) };

                        if st.table.row_hashes[bid] != h {
                            continue;
                        }
                        let (b_idx, r_idx) = st.table.row_locs[bid];
                        let r_krefs = st
                            .table
                            .key_cols_for_batch(b_idx as usize, &self.right_key_evals)?;
                        if !keys_equal(
                            &l_krefs,
                            lr,
                            &r_krefs,
                            r_idx as usize,
                            self.spec.null_equals_null,
                        )? {
                            continue;
                        }

                        cur.row_matched[lr] = true;
                        if let Some(bm) = st.table.matched.as_mut() {
                            let _ = bm.set(bid, true);
                        }
                        match self.spec.join_type {
                            JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Full => {
                                pairs.push((lr as u32, Some(bid as u32)));
                            }
                            JoinType::LeftSemi => {
                                pairs.push((lr as u32, None));
                                cur.active_bucket_next = None;
                                break;
                            }
                            JoinType::LeftAnti => {
                                cur.active_bucket_next = None;
                                break;
                            }
                            JoinType::Cross => {}
                        }
                    }

                    if cur.active_bucket_next.is_none() {
                        if !cur.row_matched[lr] && emit_unmatched_left {
                            pairs.push((lr as u32, None));
                        }
                        cur.next_row += 1;
                    }
                }

                let batch_done = cur.next_row >= l_len;
                if !pairs.is_empty() {
                    let out = build_paired_batch(
                        &cur.batch,
                        Some(&st.table),
                        &pairs,
                        &self.output_schema,
                        include_right,
                        &eval_ctx,
                        Some(&mut self.scratch_res),
                    )?;
                    self.queue_sliced_batch(out)?;
                }
                if batch_done {
                    st.cur = None;
                    self.scratch_res.free();
                    self.pull_res.free();
                }
            } else {
                // With residual filter: collect up to `batch_size` candidate key-matching pairs.
                let mut cand_pairs: Vec<(u32, Option<u32>)> = Vec::with_capacity(self.batch_size);
                while cur.next_row < l_len && cand_pairs.len() < self.batch_size {
                    let lr = cur.next_row;
                    if (lr & CANCEL_CHECK_MASK) == 0 {
                        self.check_cancel(cancel)?;
                    }
                    if matches!(self.spec.join_type, JoinType::LeftSemi | JoinType::LeftAnti)
                        && cur.row_matched[lr]
                    {
                        cur.active_bucket_next = None;
                        cur.row_chain_done[lr] = true;
                        cur.next_row += 1;
                        continue;
                    }
                    if cur.active_bucket_next.is_none() {
                        if !self.spec.null_equals_null && row_has_null_key(&l_krefs, lr) {
                            cur.row_chain_done[lr] = true;
                            cur.next_row += 1;
                            continue;
                        }
                        let h = hash_key_row(&l_krefs, lr, st.seed);
                        let head = st.table.buckets[(h as usize) & bucket_mask];
                        if head == 0 {
                            cur.row_chain_done[lr] = true;
                            cur.next_row += 1;
                            continue;
                        }
                        cur.active_hash = h;
                        cur.active_bucket_next = Some(head);
                    }

                    let h = cur.active_hash;
                    while let Some(entry_plus_one) = cur.active_bucket_next {
                        if cand_pairs.len() >= self.batch_size {
                            break;
                        }
                        let bid = (entry_plus_one - 1) as usize;
                        let nxt = st.table.next[bid];
                        cur.active_bucket_next = if nxt == 0 { None } else { Some(nxt) };

                        if st.table.row_hashes[bid] != h {
                            continue;
                        }
                        let (b_idx, r_idx) = st.table.row_locs[bid];
                        let r_krefs = st
                            .table
                            .key_cols_for_batch(b_idx as usize, &self.right_key_evals)?;
                        if !keys_equal(
                            &l_krefs,
                            lr,
                            &r_krefs,
                            r_idx as usize,
                            self.spec.null_equals_null,
                        )? {
                            continue;
                        }
                        cand_pairs.push((lr as u32, Some(bid as u32)));
                    }

                    if cur.active_bucket_next.is_none() {
                        cur.row_chain_done[lr] = true;
                        cur.next_row += 1;
                    }
                }

                if !cand_pairs.is_empty()
                    && let Some(pred) = &self.spec.residual_filter
                {
                    let cand_batch = build_paired_batch(
                        &cur.batch,
                        Some(&st.table),
                        &cand_pairs,
                        &self.combined_schema,
                        true,
                        &eval_ctx,
                        Some(&mut self.scratch_res),
                    )?;
                    let mask =
                        eval_filter(pred, &cand_batch, &eval_ctx, Some(&mut self.scratch_res))?;
                    let mut keep_mask = Bitmap::new_zeroed(cand_pairs.len());
                    let mut semi_left = Vec::new();
                    for (idx, &(lr, br_opt)) in cand_pairs.iter().enumerate() {
                        if !mask.get(idx) {
                            continue;
                        }
                        let l_usize = lr as usize;
                        cur.row_matched[l_usize] = true;
                        if let Some(br) = br_opt
                            && let Some(bm) = st.table.matched.as_mut()
                        {
                            let _ = bm.set(br as usize, true);
                        }
                        match self.spec.join_type {
                            JoinType::Inner | JoinType::Left | JoinType::Right | JoinType::Full => {
                                let _ = keep_mask.set(idx, true);
                            }
                            JoinType::LeftSemi => {
                                if !cur.row_emitted[l_usize] {
                                    cur.row_emitted[l_usize] = true;
                                    semi_left.push(lr);
                                }
                            }
                            JoinType::LeftAnti | JoinType::Cross => {}
                        }
                    }

                    if include_right {
                        if !keep_mask.all_zeros() {
                            let kept = cand_batch.into_filter_mask(&keep_mask)?;
                            let compacted = kept.compact(Some(&mut self.scratch_res))?;
                            self.queue_sliced_batch(compacted)?;
                        }
                    } else if !semi_left.is_empty() {
                        drop(cand_batch);
                        let semi_batch = build_unmatched_left_batch(
                            &cur.batch,
                            &semi_left,
                            self.spec.join_type,
                            &self.output_schema,
                            Some(&mut self.scratch_res),
                        )?;
                        self.queue_sliced_batch(semi_batch)?;
                    }
                }

                // Emit any left rows whose chains are completely done and had zero passing matches.
                let mut unmatched_left = Vec::new();
                while cur.unmatched_scan_pos < cur.next_row {
                    let lr = cur.unmatched_scan_pos;
                    cur.unmatched_scan_pos += 1;
                    if cur.row_chain_done[lr] && !cur.row_matched[lr] && !cur.row_emitted[lr] {
                        cur.row_emitted[lr] = true;
                        if emit_unmatched_left {
                            unmatched_left.push(lr as u32);
                        }
                    }
                }
                if !unmatched_left.is_empty() {
                    let unmatched_batch = build_unmatched_left_batch(
                        &cur.batch,
                        &unmatched_left,
                        self.spec.join_type,
                        &self.output_schema,
                        Some(&mut self.scratch_res),
                    )?;
                    self.queue_sliced_batch(unmatched_batch)?;
                }

                if cur.next_row >= l_len {
                    st.cur = None;
                    self.scratch_res.free();
                    self.pull_res.free();
                } else {
                    self.scratch_res.free();
                }
            }

            if !self.pending_out.is_empty() {
                self.phase = Phase::ProbeInMemory(st);
                return Ok(());
            }
        }
    }

    fn step_emit_unmatched_build(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let Phase::EmitUnmatchedBuild(mut st) = std::mem::replace(&mut self.phase, Phase::Done)
        else {
            return Ok(());
        };
        let eval_ctx = self.eval_ctx(cancel);
        let total = st.table.row_locs.len();
        let mut ids = Vec::new();
        while st.next_id < total && ids.len() < self.batch_size {
            let id = st.next_id;
            st.next_id += 1;
            let was_matched = st.table.matched.as_ref().is_some_and(|bm| bm.get(id));
            if !was_matched {
                ids.push(id as u32);
            }
        }

        if !ids.is_empty() {
            let out = build_unmatched_build_table_batch(
                &st.table,
                &ids,
                self.left_schema.len(),
                &self.output_schema,
                &eval_ctx,
                Some(&mut self.scratch_res),
            )?;
            self.scratch_res.free();
            self.queue_sliced_batch(out)?;
        }

        if st.next_id < total {
            self.phase = Phase::EmitUnmatchedBuild(st);
        } else {
            drop(st.table);
            st.build_res.free();
            if let Some(tasks) = st.remaining_tasks {
                self.phase = Phase::ProcessSpillTasks(SpillTasksState {
                    tasks,
                    build_res: st.build_res,
                });
            } else {
                self.phase = Phase::Done;
            }
        }
        Ok(())
    }

    fn step_emit_unmatched_build_file(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let Phase::EmitUnmatchedBuildFile(mut st) = std::mem::replace(&mut self.phase, Phase::Done)
        else {
            return Ok(());
        };
        while let Some(batch) = st.reader.next_batch(Some(&mut self.pull_res))? {
            self.check_cancel(cancel)?;
            if batch.is_empty() {
                drop(batch);
                self.pull_res.free();
                continue;
            }
            let batch = compact_for_join(batch, &mut self.pull_res)?;
            let indices: Vec<u32> = (0..batch.num_rows() as u32).collect();
            let out = build_unmatched_right_batch(
                &batch,
                &indices,
                self.left_schema.len(),
                &self.output_schema,
                Some(&mut self.scratch_res),
            )?;
            drop(batch);
            self.pull_res.free();
            self.scratch_res.free();
            self.queue_sliced_batch(out)?;
            self.phase = Phase::EmitUnmatchedBuildFile(st);
            return Ok(());
        }

        self.phase = Phase::ProcessSpillTasks(SpillTasksState {
            tasks: st.remaining_tasks,
            build_res: st.build_res,
        });
        Ok(())
    }
}

impl PartitionStream for HashJoinStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let step_res = self.next_batch_inner(cancel, res.as_deref_mut());
        match step_res {
            Ok(Some(batch)) => {
                if let Err(err) = reconcile_res(res.as_deref_mut(), before, batch.memory_bytes()) {
                    rollback_res(res, before);
                    self.cleanup_reservations();
                    return Err(err);
                }
                Ok(Some(batch))
            }
            Ok(None) => {
                rollback_res(res, before);
                Ok(None)
            }
            Err(err) => {
                rollback_res(res, before);
                self.cleanup_reservations();
                Err(err)
            }
        }
    }
}

impl HashJoinStream {
    fn next_batch_inner(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        loop {
            self.check_cancel(cancel)?;
            if let Some(batch) = self.pop_pending(res.as_deref_mut())? {
                return Ok(Some(batch));
            }
            match &self.phase {
                Phase::Cross(_) => return self.step_cross(cancel, res),
                Phase::BuildRight(_) => self.step_build_right(cancel)?,
                Phase::PassThroughLeft(_) => self.step_pass_through_left(cancel)?,
                Phase::SpillPartitionLeft(_) => self.step_spill_partition_left(cancel)?,
                Phase::ProcessSpillTasks(_) => self.step_process_spill_tasks(cancel)?,
                Phase::ProbeInMemory(_) => self.step_probe_in_memory(cancel)?,
                Phase::EmitUnmatchedBuild(_) => self.step_emit_unmatched_build(cancel)?,
                Phase::EmitUnmatchedBuildFile(_) => self.step_emit_unmatched_build_file(cancel)?,
                Phase::Done => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{read_i64, read_str};
    use crate::exec::{BatchSource, ExecMetrics, ScanSource, ScanSpec, SpillManager, WorkerPool};
    use crate::ir::BinaryOp;
    use cetra_column::{FlatColumn, FlatData, GlobalPool, Utf8Data};

    fn make_ctx(cap: usize, spill_cap: u64) -> ExecContext {
        let dir = tempfile::tempdir().unwrap();
        ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: EvalContext {
                now_ns: 0,
                cancel: CancelToken::new(),
            },
            spill: Arc::new(SpillManager::with_fixed_cap(dir.keep(), spill_cap)),
            workers: Arc::new(WorkerPool::new(2).unwrap()),
            batch_size: 8192,
            target_partitions: 2,
            metrics: ExecMetrics::new(),
        }
    }

    fn col_expr(index: usize, name: &str, data_type: DataType) -> Expr {
        Expr::Column {
            index,
            name: name.to_owned(),
            data_type,
        }
    }

    fn drain_stream(
        mut stream: Box<dyn PartitionStream>,
        cancel: &CancelToken,
    ) -> Result<Vec<Batch>, QueryError> {
        let mut out = Vec::new();
        while let Some(b) = stream.next_batch(cancel, None)? {
            out.push(b);
        }
        Ok(out)
    }

    type FourI64Rows = Vec<(Option<i64>, Option<i64>, Option<i64>, Option<i64>)>;

    fn extract_four_col_rows(batches: &[Batch]) -> FourI64Rows {
        let mut rows = Vec::new();
        for b in batches {
            for r in 0..b.num_rows() {
                let a = (!b.column(0).unwrap().is_null(r))
                    .then(|| read_i64(b.column(0).unwrap(), r).unwrap());
                let b_val = (!b.column(1).unwrap().is_null(r))
                    .then(|| read_i64(b.column(1).unwrap(), r).unwrap());
                let c = (!b.column(2).unwrap().is_null(r))
                    .then(|| read_i64(b.column(2).unwrap(), r).unwrap());
                let d = (!b.column(3).unwrap().is_null(r))
                    .then(|| read_i64(b.column(3).unwrap(), r).unwrap());
                rows.push((a, b_val, c, d));
            }
        }
        rows.sort();
        rows
    }

    fn extract_two_col_rows(batches: &[Batch]) -> Vec<(Option<i64>, Option<i64>)> {
        let mut rows = Vec::new();
        for b in batches {
            for r in 0..b.num_rows() {
                let a = (!b.column(0).unwrap().is_null(r))
                    .then(|| read_i64(b.column(0).unwrap(), r).unwrap());
                let b_val = (!b.column(1).unwrap().is_null(r))
                    .then(|| read_i64(b.column(1).unwrap(), r).unwrap());
                rows.push((a, b_val));
            }
        }
        rows.sort();
        rows
    }

    fn make_two_i64_batch(
        schema: &Arc<Schema>,
        c0: Vec<Option<i64>>,
        c1: Vec<Option<i64>>,
    ) -> Batch {
        let n = c0.len();
        let make_col = |vals: Vec<Option<i64>>| {
            let mut raw = Vec::with_capacity(vals.len());
            let mut valid = Bitmap::new_ones(vals.len());
            for (i, v) in vals.into_iter().enumerate() {
                match v {
                    Some(x) => raw.push(x),
                    None => {
                        raw.push(0);
                        valid.set(i, false).unwrap();
                    }
                }
            }
            let nulls = if valid.all_ones() { None } else { Some(valid) };
            Column::Flat(FlatColumn::try_new(FlatData::I64(raw), nulls).unwrap())
        };
        Batch::try_new_all(Arc::clone(schema), vec![make_col(c0), make_col(c1)], n).unwrap()
    }

    #[test]
    fn all_join_types_null_keys_and_residual_filter() {
        let ctx = make_ctx(4 * 1024 * 1024, 4 * 1024 * 1024);

        let left_schema = Schema::try_new_arc(vec![
            Field::new("lk", DataType::I64, true),
            Field::new("lv", DataType::I64, false),
        ])
        .unwrap();
        let right_schema = Schema::try_new_arc(vec![
            Field::new("rk", DataType::I64, true),
            Field::new("rv", DataType::I64, false),
        ])
        .unwrap();

        // Output schema nullability widening checks.
        let s_left = join_output_schema(JoinType::Left, &left_schema, &right_schema);
        assert!(s_left.fields()[0].nullable);
        assert!(!s_left.fields()[1].nullable);
        assert!(s_left.fields()[2].nullable);
        assert!(s_left.fields()[3].nullable);

        let s_right = join_output_schema(JoinType::Right, &left_schema, &right_schema);
        assert!(s_right.fields()[0].nullable);
        assert!(s_right.fields()[1].nullable);
        assert!(!s_right.fields()[3].nullable);

        let s_full = join_output_schema(JoinType::Full, &left_schema, &right_schema);
        assert!(s_full.fields().iter().all(|f| f.nullable));

        let s_semi = join_output_schema(JoinType::LeftSemi, &left_schema, &right_schema);
        assert_eq!(s_semi.as_ref(), left_schema.as_ref());

        // Left: (1, 10), (2, 20), (NULL, 30)
        let left_batch = make_two_i64_batch(
            &left_schema,
            vec![Some(1), Some(2), None],
            vec![Some(10), Some(20), Some(30)],
        );
        // Right: (1, 5), (1, 15), (3, 300), (NULL, 40)
        let right_batch = make_two_i64_batch(
            &right_schema,
            vec![Some(1), Some(1), Some(3), None],
            vec![Some(5), Some(15), Some(300), Some(40)],
        );

        let left_src =
            BatchSource::new(Arc::clone(&left_schema), vec![left_batch.clone()]).unwrap();
        let right_src = BatchSource::new(Arc::clone(&right_schema), vec![right_batch]).unwrap();

        let run = |join_type: JoinType,
                   null_equals_null: bool,
                   residual_filter: Option<Expr>|
         -> Vec<Batch> {
            let l_parts = left_src
                .open_partitions(&ScanSpec::default(), &ctx)
                .unwrap();
            let r_parts = right_src
                .open_partitions(&ScanSpec::default(), &ctx)
                .unwrap();
            let (left_keys, right_keys) = if join_type == JoinType::Cross {
                (vec![], vec![])
            } else {
                (
                    vec![col_expr(0, "lk", DataType::I64)],
                    vec![col_expr(0, "rk", DataType::I64)],
                )
            };
            let stream = execute_hash_join(
                l_parts,
                Arc::clone(&left_schema),
                r_parts,
                Arc::clone(&right_schema),
                HashJoinSpec {
                    join_type,
                    left_keys,
                    right_keys,
                    null_equals_null,
                    residual_filter,
                },
                &ctx,
            )
            .unwrap();
            drain_stream(stream, &ctx.eval.cancel).unwrap()
        };

        // 1. Equi-joins without residual filter, null_equals_null = false
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Inner, false, None)),
            vec![
                (Some(1), Some(10), Some(1), Some(5)),
                (Some(1), Some(10), Some(1), Some(15)),
            ]
        );
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Left, false, None)),
            vec![
                (None, Some(30), None, None),
                (Some(1), Some(10), Some(1), Some(5)),
                (Some(1), Some(10), Some(1), Some(15)),
                (Some(2), Some(20), None, None),
            ]
        );
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Right, false, None)),
            vec![
                (None, None, None, Some(40)),
                (None, None, Some(3), Some(300)),
                (Some(1), Some(10), Some(1), Some(5)),
                (Some(1), Some(10), Some(1), Some(15)),
            ]
        );
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Full, false, None)),
            vec![
                (None, None, None, Some(40)),
                (None, None, Some(3), Some(300)),
                (None, Some(30), None, None),
                (Some(1), Some(10), Some(1), Some(5)),
                (Some(1), Some(10), Some(1), Some(15)),
                (Some(2), Some(20), None, None),
            ]
        );
        // LeftSemi emits (1, 10) ONCE despite two right matches.
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftSemi, false, None)),
            vec![(Some(1), Some(10))]
        );
        // LeftAnti emits (2, 20) and (NULL, 30).
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftAnti, false, None)),
            vec![(None, Some(30)), (Some(2), Some(20))]
        );

        // 2. null_equals_null = true (NULL matches NULL)
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Inner, true, None)),
            vec![
                (None, Some(30), None, Some(40)),
                (Some(1), Some(10), Some(1), Some(5)),
                (Some(1), Some(10), Some(1), Some(15)),
            ]
        );
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftSemi, true, None)),
            vec![(None, Some(30)), (Some(1), Some(10))]
        );
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftAnti, true, None)),
            vec![(Some(2), Some(20))]
        );

        // 3. Residual filter edge cases:
        //    Case A: `rv > lv` (combined cols: lk=0, lv=1, rk=2, rv=3).
        //    Only (1, 10, 1, 15) passes; (1, 10, 1, 5) fails -> (1, 5) is unmatched on Right/Full!
        let rv_gt_lv = Expr::Binary {
            left: Box::new(col_expr(3, "rv", DataType::I64)),
            op: BinaryOp::Gt,
            right: Box::new(col_expr(1, "lv", DataType::I64)),
            data_type: DataType::Bool,
        };
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Inner, false, Some(rv_gt_lv.clone()))),
            vec![(Some(1), Some(10), Some(1), Some(15))]
        );
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Right, false, Some(rv_gt_lv.clone()))),
            vec![
                (None, None, None, Some(40)),
                (None, None, Some(1), Some(5)),
                (None, None, Some(3), Some(300)),
                (Some(1), Some(10), Some(1), Some(15)),
            ]
        );
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Full, false, Some(rv_gt_lv.clone()))),
            vec![
                (None, None, None, Some(40)),
                (None, None, Some(1), Some(5)),
                (None, None, Some(3), Some(300)),
                (None, Some(30), None, None),
                (Some(1), Some(10), Some(1), Some(15)),
                (Some(2), Some(20), None, None),
            ]
        );
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftSemi, false, Some(rv_gt_lv.clone()))),
            vec![(Some(1), Some(10))]
        );
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftAnti, false, Some(rv_gt_lv))),
            vec![(None, Some(30)), (Some(2), Some(20))]
        );

        //    Case B: `rv > 100` (all key-matching build rows for lk=1 FAIL residual_filter!).
        //    Left/Full MUST emit (1, 10, NULL, NULL); LeftSemi emits empty; LeftAnti emits (1, 10).
        let rv_gt_100 = Expr::Binary {
            left: Box::new(col_expr(3, "rv", DataType::I64)),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(ScalarValue::I64(100))),
            data_type: DataType::Bool,
        };
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Left, false, Some(rv_gt_100.clone()))),
            vec![
                (None, Some(30), None, None),
                (Some(1), Some(10), None, None),
                (Some(2), Some(20), None, None),
            ]
        );
        assert_eq!(
            extract_four_col_rows(&run(JoinType::Full, false, Some(rv_gt_100.clone()))),
            vec![
                (None, None, None, Some(40)),
                (None, None, Some(1), Some(5)),
                (None, None, Some(1), Some(15)),
                (None, None, Some(3), Some(300)),
                (None, Some(30), None, None),
                (Some(1), Some(10), None, None),
                (Some(2), Some(20), None, None),
            ]
        );
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftSemi, false, Some(rv_gt_100.clone()))),
            vec![]
        );
        assert_eq!(
            extract_two_col_rows(&run(JoinType::LeftAnti, false, Some(rv_gt_100))),
            vec![(None, Some(30)), (Some(1), Some(10)), (Some(2), Some(20))]
        );

        // 4. CrossJoin
        let cross_batches = run(JoinType::Cross, false, None);
        let total_cross_rows: usize = cross_batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total_cross_rows, 3 * 4);
        assert_eq!(ctx.pool.reserved(), 0);
        assert_eq!(ctx.spill.spilled_bytes(), 0);

        // 5. CrossJoin under tight unspillable reservation -> QueryError::TooLarge
        let tiny_ctx = make_ctx(16, 1024 * 1024);
        let l_parts = left_src
            .open_partitions(&ScanSpec::default(), &tiny_ctx)
            .unwrap();
        let r_parts = right_src
            .open_partitions(&ScanSpec::default(), &tiny_ctx)
            .unwrap();
        let mut cross_stream = execute_hash_join(
            l_parts,
            Arc::clone(&left_schema),
            r_parts,
            Arc::clone(&right_schema),
            HashJoinSpec {
                join_type: JoinType::Cross,
                left_keys: vec![],
                right_keys: vec![],
                null_equals_null: false,
                residual_filter: None,
            },
            &tiny_ctx,
        )
        .unwrap();
        let err = cross_stream
            .next_batch(&tiny_ctx.eval.cancel, None)
            .unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        drop(cross_stream);
        assert_eq!(tiny_ctx.pool.reserved(), 0);

        // 6. F64 (-0.0 == +0.0, NaN == NaN) and Decimal128 across different scales (1.50 == 1.500)
        let fl_schema = Schema::try_new_arc(vec![
            Field::new("f", DataType::F64, true),
            Field::new(
                "d",
                DataType::Decimal128 {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
        ])
        .unwrap();
        let fr_schema = Schema::try_new_arc(vec![
            Field::new("f", DataType::F64, true),
            Field::new(
                "d",
                DataType::Decimal128 {
                    precision: 10,
                    scale: 3,
                },
                true,
            ),
        ])
        .unwrap();
        let fl_b = Batch::try_new_all(
            Arc::clone(&fl_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::F64(vec![-0.0, f64::NAN]), None).unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![150, 200],
                            precision: 10,
                            scale: 2,
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();
        let fr_b = Batch::try_new_all(
            Arc::clone(&fr_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::F64(vec![0.0, f64::NAN]), None).unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![1500, 2000],
                            precision: 10,
                            scale: 3,
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();
        let fl_src = BatchSource::new(Arc::clone(&fl_schema), vec![fl_b]).unwrap();
        let fr_src = BatchSource::new(Arc::clone(&fr_schema), vec![fr_b]).unwrap();
        let f_stream = execute_hash_join(
            fl_src.open_partitions(&ScanSpec::default(), &ctx).unwrap(),
            Arc::clone(&fl_schema),
            fr_src.open_partitions(&ScanSpec::default(), &ctx).unwrap(),
            Arc::clone(&fr_schema),
            HashJoinSpec {
                join_type: JoinType::Inner,
                left_keys: vec![
                    col_expr(0, "f", DataType::F64),
                    col_expr(
                        1,
                        "d",
                        DataType::Decimal128 {
                            precision: 10,
                            scale: 2,
                        },
                    ),
                ],
                right_keys: vec![
                    col_expr(0, "f", DataType::F64),
                    col_expr(
                        1,
                        "d",
                        DataType::Decimal128 {
                            precision: 10,
                            scale: 3,
                        },
                    ),
                ],
                null_equals_null: false,
                residual_filter: None,
            },
            &ctx,
        )
        .unwrap();
        let f_out = drain_stream(f_stream, &ctx.eval.cancel).unwrap();
        let f_rows: usize = f_out.iter().map(Batch::num_rows).sum();
        assert_eq!(f_rows, 2);

        // 7. CrossJoin cancellation mid-stream frees all reservations
        let cancel_ctx = make_ctx(4 * 1024 * 1024, 4 * 1024 * 1024);
        let cancel_tok = CancelToken::new();
        let mut cross_cancel = execute_hash_join(
            left_src
                .open_partitions(&ScanSpec::default(), &cancel_ctx)
                .unwrap(),
            Arc::clone(&left_schema),
            right_src
                .open_partitions(&ScanSpec::default(), &cancel_ctx)
                .unwrap(),
            Arc::clone(&right_schema),
            HashJoinSpec {
                join_type: JoinType::Cross,
                left_keys: vec![],
                right_keys: vec![],
                null_equals_null: false,
                residual_filter: None,
            },
            &ExecContext {
                batch_size: 2,
                ..cancel_ctx.clone()
            },
        )
        .unwrap();
        let first = cross_cancel.next_batch(&cancel_tok, None).unwrap().unwrap();
        assert_eq!(first.num_rows(), 2);
        cancel_tok.cancel();
        let err = cross_cancel.next_batch(&cancel_tok, None).unwrap_err();
        assert!(matches!(err, QueryError::Cancelled));
        drop(cross_cancel);
        assert_eq!(cancel_ctx.pool.reserved(), 0);

        // 8. Computed join keys (`lk + 1 = rk`) and Selection::Mask input batches + empty right
        let masked_left = left_batch
            .filter_mask(&Bitmap::from_bools(&[true, false, false]))
            .unwrap();
        let masked_l_src = BatchSource::new(Arc::clone(&left_schema), vec![masked_left]).unwrap();
        let lk_plus_1 = Expr::Binary {
            left: Box::new(col_expr(0, "lk", DataType::I64)),
            op: BinaryOp::Add,
            right: Box::new(Expr::Literal(ScalarValue::I64(1))),
            data_type: DataType::I64,
        };
        let comp_stream = execute_hash_join(
            masked_l_src
                .open_partitions(&ScanSpec::default(), &ctx)
                .unwrap(),
            Arc::clone(&left_schema),
            right_src
                .open_partitions(&ScanSpec::default(), &ctx)
                .unwrap(),
            Arc::clone(&right_schema),
            HashJoinSpec {
                join_type: JoinType::Inner,
                left_keys: vec![lk_plus_1],
                right_keys: vec![col_expr(0, "rk", DataType::I64)],
                null_equals_null: false,
                residual_filter: None,
            },
            &ctx,
        )
        .unwrap();
        // lk=1 -> lk+1=2 (no match in right [1,1,3,NULL]); verify 0 rows.
        assert_eq!(
            drain_stream(comp_stream, &ctx.eval.cancel)
                .unwrap()
                .iter()
                .map(Batch::num_rows)
                .sum::<usize>(),
            0
        );

        // Empty right with LeftAnti returns all active left rows.
        let empty_r_src = BatchSource::empty(Arc::clone(&right_schema));
        let anti_empty_r = execute_hash_join(
            left_src
                .open_partitions(&ScanSpec::default(), &ctx)
                .unwrap(),
            Arc::clone(&left_schema),
            empty_r_src
                .open_partitions(&ScanSpec::default(), &ctx)
                .unwrap(),
            Arc::clone(&right_schema),
            HashJoinSpec {
                join_type: JoinType::LeftAnti,
                left_keys: vec![col_expr(0, "lk", DataType::I64)],
                right_keys: vec![col_expr(0, "rk", DataType::I64)],
                null_equals_null: false,
                residual_filter: None,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(
            extract_two_col_rows(&drain_stream(anti_empty_r, &ctx.eval.cancel).unwrap()),
            vec![(None, Some(30)), (Some(1), Some(10)), (Some(2), Some(20))]
        );
    }

    #[test]
    fn grace_hash_join_spills_under_tight_memory() {
        let left_schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, true),
            Field::new("lpayload", DataType::Utf8, false),
        ])
        .unwrap();
        let right_schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, true),
            Field::new("rpayload", DataType::Utf8, false),
        ])
        .unwrap();

        let make_batches = |schema: &Arc<Schema>,
                            num_batches: usize,
                            rows_per_batch: usize,
                            key_offset: i64,
                            include_nulls: bool,
                            tag: &str|
         -> Vec<Batch> {
            let mut out = Vec::with_capacity(num_batches);
            for b in 0..num_batches {
                let mut keys = Vec::with_capacity(rows_per_batch);
                let mut valid = Bitmap::new_ones(rows_per_batch);
                let mut texts = Vec::with_capacity(rows_per_batch);
                for r in 0..rows_per_batch {
                    let g = (b * rows_per_batch + r) as i64;
                    if include_nulls && g % 17 == 0 {
                        keys.push(0);
                        valid.set(r, false).unwrap();
                    } else {
                        keys.push(key_offset + (g % 250));
                    }
                    texts.push(format!("{tag}-{g:05}-padding-payload-for-spill"));
                }
                let nulls = if valid.all_ones() { None } else { Some(valid) };
                let c0 = Column::Flat(FlatColumn::try_new(FlatData::I64(keys), nulls).unwrap());
                let c1 = Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(texts.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                );
                out.push(
                    Batch::try_new_all(Arc::clone(schema), vec![c0, c1], rows_per_batch).unwrap(),
                );
            }
            out
        };

        // ~2000 rows * ~40 bytes = ~80 KiB per side, split across 20 small batches (~4 KiB each).
        // Tight 32 KiB ScopedPool guarantees Grace spill!
        let left_batches = make_batches(&left_schema, 20, 100, 0, true, "L");
        let right_batches = make_batches(&right_schema, 20, 100, 100, true, "R");

        let left_src = BatchSource::new(Arc::clone(&left_schema), left_batches).unwrap();
        let right_src = BatchSource::new(Arc::clone(&right_schema), right_batches).unwrap();

        let big_ctx = make_ctx(16 * 1024 * 1024, 32 * 1024 * 1024);
        let tight_ctx = make_ctx(32 * 1024, 32 * 1024 * 1024);

        let collect_sorted = |ctx: &ExecContext,
                              join_type: JoinType|
         -> Vec<(Option<i64>, String, Option<i64>, String)> {
            let l_parts = left_src.open_partitions(&ScanSpec::default(), ctx).unwrap();
            let r_parts = right_src
                .open_partitions(&ScanSpec::default(), ctx)
                .unwrap();
            let mut stream = execute_hash_join(
                l_parts,
                Arc::clone(&left_schema),
                r_parts,
                Arc::clone(&right_schema),
                HashJoinSpec {
                    join_type,
                    left_keys: vec![col_expr(0, "k", DataType::I64)],
                    right_keys: vec![col_expr(0, "k", DataType::I64)],
                    null_equals_null: false,
                    residual_filter: None,
                },
                ctx,
            )
            .unwrap();

            // Verify PartitionStream::next_batch Reservation postcondition when res is passed.
            let mut res = ctx.pool.register("caller_scan", false);
            let mut rows = Vec::new();
            while let Some(b) = stream.next_batch(&ctx.eval.cancel, Some(&mut res)).unwrap() {
                assert_eq!(res.size(), b.memory_bytes());
                let is_semi_anti = matches!(join_type, JoinType::LeftSemi | JoinType::LeftAnti);
                for r in 0..b.num_rows() {
                    let lk = (!b.column(0).unwrap().is_null(r))
                        .then(|| read_i64(b.column(0).unwrap(), r).unwrap());
                    let lp = read_str(b.column(1).unwrap(), r)
                        .unwrap_or("<NULL>")
                        .to_owned();
                    let (rk, rp) = if is_semi_anti {
                        (None, String::new())
                    } else {
                        let rk = (!b.column(2).unwrap().is_null(r))
                            .then(|| read_i64(b.column(2).unwrap(), r).unwrap());
                        let rp = read_str(b.column(3).unwrap(), r)
                            .unwrap_or("<NULL>")
                            .to_owned();
                        (rk, rp)
                    };
                    rows.push((lk, lp, rk, rp));
                }
                res.free();
            }
            assert_eq!(res.size(), 0);
            drop(res);
            drop(stream);
            assert_eq!(ctx.spill.spilled_bytes(), 0);
            assert_eq!(ctx.pool.reserved(), 0);
            rows.sort();
            rows
        };

        for join_type in [
            JoinType::Inner,
            JoinType::Left,
            JoinType::Right,
            JoinType::Full,
            JoinType::LeftSemi,
            JoinType::LeftAnti,
        ] {
            let expected = collect_sorted(&big_ctx, join_type);
            let actual = collect_sorted(&tight_ctx, join_type);
            assert_eq!(
                actual, expected,
                "Grace spill parity mismatch for {join_type:?}"
            );
        }

        // Severe skew (all rows share identical key `k = 42` > pool cap) hits depth 4 -> TooLarge
        // and cleans up all spill files and reservations.
        let skew_left = make_batches(&left_schema, 1, 10, 42, false, "SL");
        let skew_right = make_batches(&right_schema, 30, 100, 42, false, "SR");
        // Rewrite right keys to all 42.
        let skew_right: Vec<Batch> = skew_right
            .into_iter()
            .map(|b| {
                let n = b.num_rows();
                let k =
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vec![42; n]), None).unwrap());
                Batch::try_new_all(
                    Arc::clone(&right_schema),
                    vec![k, b.columns()[1].clone()],
                    n,
                )
                .unwrap()
            })
            .collect();
        let skew_l_src = BatchSource::new(Arc::clone(&left_schema), skew_left).unwrap();
        let skew_r_src = BatchSource::new(Arc::clone(&right_schema), skew_right).unwrap();
        let skew_stream = execute_hash_join(
            skew_l_src
                .open_partitions(&ScanSpec::default(), &tight_ctx)
                .unwrap(),
            Arc::clone(&left_schema),
            skew_r_src
                .open_partitions(&ScanSpec::default(), &tight_ctx)
                .unwrap(),
            Arc::clone(&right_schema),
            HashJoinSpec {
                join_type: JoinType::Inner,
                left_keys: vec![col_expr(0, "k", DataType::I64)],
                right_keys: vec![col_expr(0, "k", DataType::I64)],
                null_equals_null: false,
                residual_filter: None,
            },
            &tight_ctx,
        )
        .unwrap();
        let err = drain_stream(skew_stream, &tight_ctx.eval.cancel).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge), "got {err:?}");
        assert_eq!(tight_ctx.spill.spilled_bytes(), 0);
        assert_eq!(tight_ctx.pool.reserved(), 0);

        // Depth-2 salted re-partitioning success: pick 80 distinct keys that all collide into
        // partition 0 at depth 0, with enough rows per key to overflow depth 1, then succeed at depth 2.
        let seed0 = hash_seed(0);
        let mut colliding_keys = Vec::with_capacity(80);
        let mut cand: i64 = 0;
        while colliding_keys.len() < 80 {
            let kcol = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![cand]), None).unwrap());
            let h = hash_key_row(&[&kcol], 0, seed0);
            if (h as usize) & (NUM_JOIN_PARTITIONS - 1) == 0 {
                colliding_keys.push(cand);
            }
            cand += 1;
        }
        let mut depth2_right = Vec::new();
        for chunk in colliding_keys.chunks(4) {
            let mut ks = Vec::new();
            let mut ps = Vec::new();
            for &k in chunk {
                for rep in 0..25 {
                    ks.push(k);
                    ps.push(format!("DR-{k}-{rep}-padding-payload-for-depth2"));
                }
            }
            let n = ks.len();
            let c0 = Column::Flat(FlatColumn::try_new(FlatData::I64(ks), None).unwrap());
            let c1 = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(ps.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            );
            depth2_right
                .push(Batch::try_new_all(Arc::clone(&right_schema), vec![c0, c1], n).unwrap());
        }
        let d2_l_batch = {
            let n = colliding_keys.len();
            let ps: Vec<String> = colliding_keys.iter().map(|k| format!("DL-{k}")).collect();
            let c0 =
                Column::Flat(FlatColumn::try_new(FlatData::I64(colliding_keys), None).unwrap());
            let c1 = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(
                        Utf8Data::try_from_slices(ps.iter().map(String::as_str)).unwrap(),
                    ),
                    None,
                )
                .unwrap(),
            );
            Batch::try_new_all(Arc::clone(&left_schema), vec![c0, c1], n).unwrap()
        };
        let d2_l_src = BatchSource::new(Arc::clone(&left_schema), vec![d2_l_batch]).unwrap();
        let d2_r_src = BatchSource::new(Arc::clone(&right_schema), depth2_right).unwrap();
        let d2_stream = execute_hash_join(
            d2_l_src
                .open_partitions(&ScanSpec::default(), &tight_ctx)
                .unwrap(),
            Arc::clone(&left_schema),
            d2_r_src
                .open_partitions(&ScanSpec::default(), &tight_ctx)
                .unwrap(),
            Arc::clone(&right_schema),
            HashJoinSpec {
                join_type: JoinType::Inner,
                left_keys: vec![col_expr(0, "k", DataType::I64)],
                right_keys: vec![col_expr(0, "k", DataType::I64)],
                null_equals_null: false,
                residual_filter: None,
            },
            &tight_ctx,
        )
        .unwrap();
        let d2_rows: usize = drain_stream(d2_stream, &tight_ctx.eval.cancel)
            .unwrap()
            .iter()
            .map(Batch::num_rows)
            .sum();
        assert_eq!(d2_rows, 80 * 25);
        assert_eq!(tight_ctx.spill.spilled_bytes(), 0);
        assert_eq!(tight_ctx.pool.reserved(), 0);
    }
}
