//! Streaming push operators (`FilterOp`, `ProjectOp`, `LimitOp`, `UnnestOp`), `UnionSource`,
//! and bounded-memory `TopKSink`.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use cetra_column::{
    Batch, CancelToken, Column, ColumnError, DataType, Field, Reservation, Schema, Selection,
};

use crate::error::QueryError;
use crate::eval::{
    ElemRef, ListRef, build_flat_from_elem_refs, build_flat_from_sources, eval_expr, eval_filter,
    list_ref_at, list_ref_elem, list_ref_len,
};
use crate::ir::Expr;

use super::pipeline::{ControlFlow, ExecContext, PushOperator, PushSink};
use super::sort_key::{SortKey, compare_rows_across};
use super::source::{IdLookupHint, PartitionStream, ScanSource, ScanSpec, SortHint, SubstringHint};

fn reconcile_res(res: &mut Reservation, target_bytes: usize) -> Result<(), QueryError> {
    let cur = res.size();
    if target_bytes > cur {
        res.try_grow(target_bytes - cur)?;
    } else if target_bytes < cur {
        res.shrink(cur - target_bytes);
    }
    Ok(())
}

fn for_each_active_row(
    batch: &Batch,
    mut f: impl FnMut(usize) -> Result<bool, QueryError>,
) -> Result<(), QueryError> {
    let raw_len = batch.raw_len();
    match batch.selection() {
        Selection::All => {
            for row in 0..raw_len {
                if !f(row)? {
                    break;
                }
            }
        }
        Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                if !f(row)? {
                    break;
                }
            }
        }
        Selection::Indices(indices) => {
            for &row in indices {
                if !f(row as usize)? {
                    break;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FilterOp
// ---------------------------------------------------------------------------

/// Streaming filter operator (`WHERE` / `HAVING`) using zero-copy `Batch::into_filter_mask`.
///
/// Drops 0-row batches and compacts via [`Batch::compact`] only when `active * 4 < raw_len`.
#[derive(Debug)]
pub struct FilterOp {
    predicate: Expr,
    ctx: ExecContext,
    scratch: Reservation,
}

impl FilterOp {
    /// Creates a [`FilterOp`] evaluating `predicate` under `ctx`.
    pub fn new(predicate: Expr, ctx: ExecContext) -> Self {
        let scratch = ctx.pool.register("filter", false);
        Self {
            predicate,
            ctx,
            scratch,
        }
    }
}

impl PushOperator for FilterOp {
    fn name(&self) -> &'static str {
        "Filter"
    }

    fn push(
        &mut self,
        batch: Batch,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError> {
        if self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if batch.is_empty() {
            return Ok(ControlFlow::Continue);
        }

        let mask = match eval_filter(
            &self.predicate,
            &batch,
            &self.ctx.eval,
            Some(&mut self.scratch),
        ) {
            Ok(m) => m,
            Err(err) => {
                self.scratch.free();
                return Err(err);
            }
        };

        let filtered = match batch.into_filter_mask(&mask) {
            Ok(b) => b,
            Err(err) => {
                self.scratch.free();
                return Err(err.into());
            }
        };
        drop(mask);

        if filtered.is_empty() {
            self.scratch.free();
            return Ok(ControlFlow::Continue);
        }

        let active = filtered.num_rows();
        let raw_len = filtered.raw_len();
        let out = if active.saturating_mul(4) < raw_len {
            self.scratch.free();
            match filtered.compact(Some(&mut self.scratch)) {
                Ok(b) => b,
                Err(err) => {
                    self.scratch.free();
                    return Err(err.into());
                }
            }
        } else {
            filtered
        };

        let flow = emit(out);
        self.scratch.free();
        flow
    }
}

// ---------------------------------------------------------------------------
// ProjectOp
// ---------------------------------------------------------------------------

/// Streaming projection operator evaluating expressions while preserving `batch.selection()`.
#[derive(Debug)]
pub struct ProjectOp {
    exprs: Vec<(String, Expr)>,
    ctx: ExecContext,
    scratch: Reservation,
    output_schema: Option<Arc<Schema>>,
}

impl ProjectOp {
    /// Creates a [`ProjectOp`] computing `(output_name, expr)` columns under `ctx`.
    pub fn new(exprs: Vec<(String, Expr)>, ctx: ExecContext) -> Self {
        let scratch = ctx.pool.register("project", false);
        Self {
            exprs,
            ctx,
            scratch,
            output_schema: None,
        }
    }

    /// Creates a [`ProjectOp`] whose output batches conform to `schema` (preserving explicit field nullability).
    pub fn with_schema(
        exprs: Vec<Expr>,
        schema: Arc<Schema>,
        ctx: ExecContext,
    ) -> Result<Self, QueryError> {
        if exprs.len() != schema.fields().len() {
            return Err(ColumnError::InvalidBatch {
                reason: "projection expression count does not match schema",
            }
            .into());
        }
        let mut named = Vec::with_capacity(exprs.len());
        for (expr, field) in exprs.into_iter().zip(schema.fields()) {
            if expr.data_type() != field.data_type {
                return Err(ColumnError::InvalidBatch {
                    reason: "projection expression type does not match schema field",
                }
                .into());
            }
            named.push((field.name.clone(), expr));
        }
        let scratch = ctx.pool.register("project", false);
        Ok(Self {
            exprs: named,
            ctx,
            scratch,
            output_schema: Some(schema),
        })
    }

    fn output_schema(&mut self) -> Result<Arc<Schema>, QueryError> {
        if let Some(schema) = &self.output_schema {
            return Ok(Arc::clone(schema));
        }
        let fields = self
            .exprs
            .iter()
            .map(|(name, expr)| Field::new(name.clone(), expr.data_type(), true))
            .collect();
        let schema = Schema::try_new_arc(fields)?;
        self.output_schema = Some(Arc::clone(&schema));
        Ok(schema)
    }
}

impl PushOperator for ProjectOp {
    fn name(&self) -> &'static str {
        "Project"
    }

    fn push(
        &mut self,
        batch: Batch,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError> {
        if self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        let schema = self.output_schema()?;
        let mut cols = Vec::with_capacity(self.exprs.len());
        for (_, expr) in &self.exprs {
            match eval_expr(expr, &batch, &self.ctx.eval, Some(&mut self.scratch)) {
                Ok(col) => cols.push(col),
                Err(err) => {
                    self.scratch.free();
                    return Err(err);
                }
            }
        }

        let raw_len = batch.raw_len();
        let selection = batch.selection().clone();
        drop(batch);

        let out = match Batch::try_new(schema, cols, raw_len, selection) {
            Ok(b) => b,
            Err(err) => {
                self.scratch.free();
                return Err(err.into());
            }
        };
        if let Err(err) = reconcile_res(&mut self.scratch, out.memory_bytes()) {
            self.scratch.free();
            return Err(err);
        }

        let flow = emit(out);
        self.scratch.free();
        flow
    }
}

// ---------------------------------------------------------------------------
// LimitOp
// ---------------------------------------------------------------------------

/// Streaming `LIMIT` / `OFFSET` push operator using zero-copy [`Batch::into_with_selection`].
///
/// When `shared_remaining` is `Some`, uses lock-free CAS (`try_update`) so concurrent
/// partitions never overshoot the global limit.
#[derive(Debug)]
pub struct LimitOp {
    offset_remaining: usize,
    limit_remaining: Option<usize>,
    shared_remaining: Option<Arc<AtomicUsize>>,
}

impl LimitOp {
    /// Creates a [`LimitOp`] skipping `offset` rows and emitting at most `limit` rows.
    pub fn new(
        offset: usize,
        limit: Option<usize>,
        shared_remaining: Option<Arc<AtomicUsize>>,
    ) -> Self {
        Self {
            offset_remaining: offset,
            limit_remaining: limit,
            shared_remaining,
        }
    }
}

fn slice_active_batch(batch: Batch, skip: usize, take: usize) -> Result<Batch, QueryError> {
    let active = batch.num_rows();
    if skip == 0 && take == active {
        return Ok(batch);
    }
    let new_sel = match batch.selection() {
        Selection::All => {
            let start = u32::try_from(skip).map_err(|_| ColumnError::Overflow {
                reason: "limit offset exceeds u32::MAX",
            })?;
            let end = u32::try_from(skip + take).map_err(|_| ColumnError::Overflow {
                reason: "limit end exceeds u32::MAX",
            })?;
            Selection::Indices((start..end).collect())
        }
        Selection::Mask(mask) => {
            let indices: Vec<u32> = mask
                .iter_ones()
                .skip(skip)
                .take(take)
                .map(|i| i as u32)
                .collect();
            Selection::Indices(indices)
        }
        Selection::Indices(idx) => Selection::Indices(idx[skip..skip + take].to_vec()),
    };
    Ok(batch.into_with_selection(new_sel)?)
}

impl PushOperator for LimitOp {
    fn name(&self) -> &'static str {
        "Limit"
    }

    fn push(
        &mut self,
        batch: Batch,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError> {
        if self.limit_remaining == Some(0) {
            return Ok(ControlFlow::Stop);
        }

        let active = batch.num_rows();
        if active == 0 {
            return Ok(ControlFlow::Continue);
        }

        if self.offset_remaining >= active {
            self.offset_remaining -= active;
            return Ok(ControlFlow::Continue);
        }
        let skip = self.offset_remaining;
        self.offset_remaining = 0;
        let avail = active - skip;

        let (take, exhausted) = if let Some(rem) = &self.shared_remaining {
            let max_claim = match self.limit_remaining {
                Some(l) => avail.min(l),
                None => avail,
            };
            let mut claimed = 0usize;
            let mut global_exhausted = false;
            let _ = rem.try_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |cur| {
                if cur == 0 {
                    claimed = 0;
                    global_exhausted = true;
                    None
                } else {
                    let take = max_claim.min(cur);
                    claimed = take;
                    global_exhausted = take == cur;
                    Some(cur - take)
                }
            });
            if claimed == 0 {
                return Ok(ControlFlow::Stop);
            }
            if let Some(local) = &mut self.limit_remaining {
                *local -= claimed;
                if *local == 0 {
                    global_exhausted = true;
                }
            }
            (claimed, global_exhausted)
        } else {
            match &mut self.limit_remaining {
                Some(rem) => {
                    let take = avail.min(*rem);
                    *rem -= take;
                    (take, *rem == 0)
                }
                None => (avail, false),
            }
        };

        if take == 0 {
            return Ok(ControlFlow::Stop);
        }

        let out = slice_active_batch(batch, skip, take)?;
        let flow = emit(out)?;
        if flow == ControlFlow::Stop || exhausted {
            Ok(ControlFlow::Stop)
        } else {
            Ok(ControlFlow::Continue)
        }
    }
}

// ---------------------------------------------------------------------------
// UnnestOp
// ---------------------------------------------------------------------------

/// Streaming `UNNEST` operator expanding one or more `List(T)` columns with zipping semantics.
///
/// For each active row, repeats `max(list_len)` times across `unnest_cols`, padding shorter
/// lists with `NULL`. When all lists for a row are empty or null:
/// - `preserve_nulls = true` emits 1 row with `NULL` in all unnested columns;
/// - `preserve_nulls = false` drops the row (0 rows emitted).
#[derive(Debug)]
pub struct UnnestOp {
    unnest_cols: Vec<usize>,
    preserve_nulls: bool,
    ctx: ExecContext,
    scratch: Reservation,
    cached_plan: Option<UnnestSchemaPlan>,
}

#[derive(Debug, Clone)]
struct UnnestSchemaPlan {
    input_schema: Arc<Schema>,
    output_schema: Arc<Schema>,
    /// For each column in input/output schema: `Some(pos_in_unnest_cols)` or `None`.
    col_to_unnest: Vec<Option<usize>>,
    inner_types: Vec<DataType>,
}

impl UnnestOp {
    /// Creates an [`UnnestOp`] expanding `unnest_cols` in-place under `ctx`.
    pub fn new(unnest_cols: Vec<usize>, preserve_nulls: bool, ctx: ExecContext) -> Self {
        let scratch = ctx.pool.register("unnest", false);
        Self {
            unnest_cols,
            preserve_nulls,
            ctx,
            scratch,
            cached_plan: None,
        }
    }

    fn plan_for(&mut self, input_schema: &Arc<Schema>) -> Result<UnnestSchemaPlan, QueryError> {
        if let Some(plan) = &self.cached_plan
            && plan.input_schema.as_ref() == input_schema.as_ref()
        {
            return Ok(plan.clone());
        }

        let num_cols = input_schema.len();
        let mut col_to_unnest = vec![None; num_cols];
        let mut inner_types = Vec::with_capacity(self.unnest_cols.len());

        for (u_idx, &col_idx) in self.unnest_cols.iter().enumerate() {
            let field = input_schema
                .fields()
                .get(col_idx)
                .ok_or(ColumnError::InvalidBatch {
                    reason: "unnest column index out of bounds",
                })?;
            let DataType::List(inner) = &field.data_type else {
                return Err(QueryError::failed(format!(
                    "unnest column '{}' must have List type, got {:?}",
                    field.name, field.data_type
                )));
            };
            inner_types.push((**inner).clone());
            if col_to_unnest[col_idx].is_none() {
                col_to_unnest[col_idx] = Some(u_idx);
            }
        }

        let out_fields: Vec<Field> = input_schema
            .fields()
            .iter()
            .enumerate()
            .map(|(col_idx, f)| match col_to_unnest[col_idx] {
                Some(u_idx) => Field::new(f.name.clone(), inner_types[u_idx].clone(), true),
                None => f.clone(),
            })
            .collect();
        let output_schema = Schema::try_new_arc(out_fields)?;

        let plan = UnnestSchemaPlan {
            input_schema: Arc::clone(input_schema),
            output_schema,
            col_to_unnest,
            inner_types,
        };
        self.cached_plan = Some(plan.clone());
        Ok(plan)
    }

    fn flush_chunk(
        &mut self,
        batch: &Batch,
        plan: &UnnestSchemaPlan,
        chunk_rows: &[u32],
        unnest_refs: &[Vec<ElemRef<'_>>],
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError> {
        let chunk_len = chunk_rows.len();
        if chunk_len == 0 {
            return Ok(ControlFlow::Continue);
        }

        let mut out_cols = Vec::with_capacity(plan.output_schema.len());
        for col_idx in 0..plan.output_schema.len() {
            let col = match plan.col_to_unnest[col_idx] {
                Some(u_idx) => {
                    let before = self.scratch.size();
                    build_flat_from_elem_refs(
                        &plan.inner_types[u_idx],
                        &unnest_refs[u_idx],
                        &mut Some(&mut self.scratch),
                        &self.ctx.eval,
                        before,
                    )?
                }
                None => Column::Flat(
                    batch
                        .column(col_idx)
                        .ok_or(ColumnError::InvalidBatch {
                            reason: "unnest column missing from batch",
                        })?
                        .take(chunk_rows, Some(&mut self.scratch))?,
                ),
            };
            out_cols.push(col);
        }

        let out_batch = Batch::try_new_all(Arc::clone(&plan.output_schema), out_cols, chunk_len)?;
        reconcile_res(&mut self.scratch, out_batch.memory_bytes())?;
        let flow = emit(out_batch);
        self.scratch.free();
        flow
    }
}

impl PushOperator for UnnestOp {
    fn name(&self) -> &'static str {
        "Unnest"
    }

    fn push(
        &mut self,
        batch: Batch,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError> {
        if self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        let plan = self.plan_for(batch.schema())?;
        if batch.is_empty() {
            return Ok(ControlFlow::Continue);
        }
        if self.unnest_cols.is_empty() {
            return emit(batch);
        }

        let chunk_limit = self.ctx.batch_size.max(1);
        let num_unnest = self.unnest_cols.len();
        let mut chunk_rows: Vec<u32> = Vec::with_capacity(chunk_limit.min(batch.num_rows()));
        let mut unnest_refs: Vec<Vec<ElemRef<'_>>> = (0..num_unnest)
            .map(|_| Vec::with_capacity(chunk_limit.min(batch.num_rows())))
            .collect();
        let mut row_lists: Vec<Option<ListRef<'_>>> = Vec::with_capacity(num_unnest);
        let mut stopped = false;

        let res = for_each_active_row(&batch, |row_idx| {
            if self.ctx.eval.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            row_lists.clear();
            let mut max_len = 0usize;
            for &col_idx in &self.unnest_cols {
                let col = batch.column(col_idx).ok_or(ColumnError::InvalidBatch {
                    reason: "unnest column missing from batch",
                })?;
                if col.is_null(row_idx) {
                    row_lists.push(None);
                } else {
                    let lref = list_ref_at(col, row_idx);
                    if let Some(lr) = lref {
                        max_len = max_len.max(list_ref_len(lr));
                    }
                    row_lists.push(lref);
                }
            }

            let repeats = if max_len == 0 {
                usize::from(self.preserve_nulls)
            } else {
                max_len
            };

            for elem_pos in 0..repeats {
                chunk_rows.push(row_idx as u32);
                for (u_idx, lref_opt) in row_lists.iter().enumerate() {
                    let elem = match lref_opt {
                        Some(lr) if elem_pos < list_ref_len(*lr) => list_ref_elem(*lr, elem_pos),
                        _ => ElemRef::Null,
                    };
                    unnest_refs[u_idx].push(elem);
                }
                if chunk_rows.len() == chunk_limit {
                    let flow = self.flush_chunk(&batch, &plan, &chunk_rows, &unnest_refs, emit)?;
                    chunk_rows.clear();
                    for refs in &mut unnest_refs {
                        refs.clear();
                    }
                    if flow == ControlFlow::Stop {
                        stopped = true;
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        });

        if let Err(err) = res {
            self.scratch.free();
            return Err(err);
        }

        if !stopped && !chunk_rows.is_empty() {
            let flow = self
                .flush_chunk(&batch, &plan, &chunk_rows, &unnest_refs, emit)
                .inspect_err(|_| self.scratch.free())?;
            if flow == ControlFlow::Stop {
                stopped = true;
            }
        }

        Ok(if stopped {
            ControlFlow::Stop
        } else {
            ControlFlow::Continue
        })
    }
}

// ---------------------------------------------------------------------------
// UnionSource
// ---------------------------------------------------------------------------

/// `UNION ALL` [`ScanSource`] concatenating child source partitions.
#[derive(Debug)]
pub struct UnionSource {
    schema: Arc<Schema>,
    sources: Vec<Arc<dyn ScanSource>>,
}

impl UnionSource {
    /// Builds a [`UnionSource`] over `sources`, validating that every child matches `schema`
    /// in column count, column types, and non-nullability constraints.
    pub fn new(schema: Arc<Schema>, sources: Vec<Arc<dyn ScanSource>>) -> Result<Self, QueryError> {
        for src in &sources {
            let child_schema = src.schema();
            if child_schema.len() != schema.len() {
                return Err(ColumnError::InvalidBatch {
                    reason: "UnionSource child column count does not match union schema",
                }
                .into());
            }
            for (union_f, child_f) in schema.fields().iter().zip(child_schema.fields()) {
                if union_f.data_type != child_f.data_type {
                    return Err(ColumnError::InvalidBatch {
                        reason: "UnionSource child column type does not match union schema",
                    }
                    .into());
                }
                if !union_f.nullable && child_f.nullable {
                    return Err(ColumnError::InvalidBatch {
                        reason: "UnionSource nullable child cannot feed non-nullable union column",
                    }
                    .into());
                }
            }
        }
        Ok(Self { schema, sources })
    }
}

impl ScanSource for UnionSource {
    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
        match &spec.projection {
            Some(indices) => Ok(Arc::new(self.schema.project(indices)?)),
            None => Ok(Arc::clone(&self.schema)),
        }
    }

    fn open_partitions(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let target_proj_schema = self.projected_schema(spec)?;
        let mut all_parts: Vec<Box<dyn PartitionStream>> = Vec::new();
        for src in &self.sources {
            let child_full_schema = src.schema();
            let names_differ = child_full_schema
                .fields()
                .iter()
                .zip(self.schema.fields())
                .any(|(c, u)| c.name != u.name);
            let remapped = if names_differ {
                Some(remap_scan_spec_hints(
                    spec,
                    &self.schema,
                    &child_full_schema,
                ))
            } else {
                None
            };
            let effective_spec = remapped.as_ref().unwrap_or(spec);
            let child_proj_schema = src.projected_schema(effective_spec)?;
            let needs_retag = child_proj_schema.as_ref() != target_proj_schema.as_ref();
            for part in src.open_partitions(effective_spec, ctx)? {
                if needs_retag {
                    all_parts.push(Box::new(RetagPartitionStream {
                        inner: part,
                        schema: Arc::clone(&target_proj_schema),
                    }));
                } else {
                    all_parts.push(part);
                }
            }
        }

        let max_parts = ctx.target_partitions.max(1);
        if all_parts.len() <= max_parts {
            return Ok(all_parts);
        }

        let mut grouped: Vec<Vec<Box<dyn PartitionStream>>> =
            (0..max_parts).map(|_| Vec::new()).collect();
        for (idx, part) in all_parts.into_iter().enumerate() {
            grouped[idx % max_parts].push(part);
        }
        Ok(grouped
            .into_iter()
            .filter(|g| !g.is_empty())
            .map(|streams| {
                Box::new(ChainedPartitionStream { streams, cursor: 0 }) as Box<dyn PartitionStream>
            })
            .collect())
    }

    fn explain_summary(&self, _spec: &ScanSpec) -> String {
        format!("UnionSource(sources={})", self.sources.len())
    }
}

fn remap_col_name(name: &str, union_schema: &Schema, child_schema: &Schema) -> String {
    match union_schema.index_of(name) {
        Some(idx) => child_schema.fields()[idx].name.clone(),
        None => name.to_owned(),
    }
}

fn remap_scan_spec_hints(
    spec: &ScanSpec,
    union_schema: &Schema,
    child_schema: &Schema,
) -> ScanSpec {
    let mut out = spec.clone();
    out.map_keys = spec
        .map_keys
        .iter()
        .map(|(col, k)| (remap_col_name(col, union_schema, child_schema), k.clone()))
        .collect();
    out.id_lookups = spec
        .id_lookups
        .iter()
        .map(|hint| match hint {
            IdLookupHint::Fixed16 { column, values } => IdLookupHint::Fixed16 {
                column: remap_col_name(column, union_schema, child_schema),
                values: values.clone(),
            },
            IdLookupHint::Fixed8 { column, values } => IdLookupHint::Fixed8 {
                column: remap_col_name(column, union_schema, child_schema),
                values: values.clone(),
            },
            IdLookupHint::Utf8 { column, values } => IdLookupHint::Utf8 {
                column: remap_col_name(column, union_schema, child_schema),
                values: values.clone(),
            },
        })
        .collect();
    out.substrings = spec
        .substrings
        .iter()
        .map(|s| SubstringHint {
            column: remap_col_name(&s.column, union_schema, child_schema),
            needle: s.needle.clone(),
            case_insensitive: s.case_insensitive,
        })
        .collect();
    out.sort_hints = spec
        .sort_hints
        .iter()
        .map(|s| SortHint {
            column: remap_col_name(&s.column, union_schema, child_schema),
            descending: s.descending,
        })
        .collect();
    out
}

struct RetagPartitionStream {
    inner: Box<dyn PartitionStream>,
    schema: Arc<Schema>,
}

impl PartitionStream for RetagPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let Some(batch) = self.inner.next_batch(cancel, res.as_deref_mut())? else {
            return Ok(None);
        };
        if batch.schema().as_ref() == self.schema.as_ref() {
            return Ok(Some(batch));
        }
        let (_old_schema, columns, raw_len, selection) = batch.into_parts();
        let retagged = match Batch::try_new(Arc::clone(&self.schema), columns, raw_len, selection) {
            Ok(b) => b,
            Err(err) => {
                if let Some(r) = res {
                    let cur = r.size();
                    if cur > before {
                        r.shrink(cur - before);
                    }
                }
                return Err(err.into());
            }
        };
        Ok(Some(retagged))
    }
}

struct ChainedPartitionStream {
    streams: Vec<Box<dyn PartitionStream>>,
    cursor: usize,
}

impl PartitionStream for ChainedPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        while self.cursor < self.streams.len() {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            if let Some(batch) = self.streams[self.cursor].next_batch(cancel, res.as_deref_mut())? {
                return Ok(Some(batch));
            }
            self.cursor += 1;
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// TopKSink
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum HeapSource {
    Stored { batch_idx: usize, row_idx: usize },
    Incoming { row_idx: usize },
}

#[derive(Debug, Clone, Copy)]
struct HeapItem {
    source: HeapSource,
    seq: u64,
}

fn sift_up<F>(heap: &mut [HeapItem], mut idx: usize, mut cmp: F)
where
    F: FnMut(&HeapItem, &HeapItem) -> Ordering,
{
    while idx > 0 {
        let parent = (idx - 1) / 2;
        if cmp(&heap[idx], &heap[parent]) == Ordering::Greater {
            heap.swap(idx, parent);
            idx = parent;
        } else {
            break;
        }
    }
}

fn sift_down<F>(heap: &mut [HeapItem], mut idx: usize, mut cmp: F)
where
    F: FnMut(&HeapItem, &HeapItem) -> Ordering,
{
    let len = heap.len();
    loop {
        let left = 2 * idx + 1;
        if left >= len {
            break;
        }
        let right = left + 1;
        let mut largest = left;
        if right < len && cmp(&heap[right], &heap[left]) == Ordering::Greater {
            largest = right;
        }
        if cmp(&heap[largest], &heap[idx]) == Ordering::Greater {
            heap.swap(idx, largest);
            idx = largest;
        } else {
            break;
        }
    }
}

fn compare_heap_items(
    a: &HeapItem,
    b: &HeapItem,
    key_batches: &[Vec<Column>],
    incoming_keys: &[Column],
    keys: &[SortKey],
) -> Ordering {
    let (cols_a, row_a) = match a.source {
        HeapSource::Stored { batch_idx, row_idx } => (key_batches[batch_idx].as_slice(), row_idx),
        HeapSource::Incoming { row_idx } => (incoming_keys, row_idx),
    };
    let (cols_b, row_b) = match b.source {
        HeapSource::Stored { batch_idx, row_idx } => (key_batches[batch_idx].as_slice(), row_idx),
        HeapSource::Incoming { row_idx } => (incoming_keys, row_idx),
    };
    compare_rows_across(cols_a, row_a, cols_b, row_b, keys).then_with(|| a.seq.cmp(&b.seq))
}

fn kept_bytes(row_batches: &[Batch], key_batches: &[Vec<Column>]) -> usize {
    let batch_bytes: usize = row_batches.iter().map(Batch::memory_bytes).sum();
    let key_bytes: usize = key_batches
        .iter()
        .flat_map(|cols| cols.iter())
        .map(Column::memory_bytes)
        .sum();
    batch_bytes + key_bytes
}

/// Bounded-memory `ORDER BY ... LIMIT ... OFFSET ...` partition sink (`PushSink`).
///
/// Maintains a bounded binary max-heap of size `offset.saturating_add(limit)` using
/// [`compare_rows_across`], tracking kept-row bytes in an unspillable [`Reservation`].
pub struct TopKSink {
    schema: Arc<Schema>,
    keys: Vec<SortKey>,
    limit: usize,
    offset: usize,
    keep: usize,
    ctx: ExecContext,
    heap: Vec<HeapItem>,
    row_batches: Vec<Batch>,
    key_batches: Vec<Vec<Column>>,
    stored_rows: usize,
    next_seq: u64,
    res: Reservation,
    scratch: Reservation,
}

impl fmt::Debug for TopKSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TopKSink")
            .field("schema", &self.schema)
            .field("keys_len", &self.keys.len())
            .field("limit", &self.limit)
            .field("offset", &self.offset)
            .field("kept_rows", &self.heap.len())
            .field("stored_rows", &self.stored_rows)
            .field("reserved_bytes", &self.res.size())
            .finish()
    }
}

impl TopKSink {
    /// Creates a [`TopKSink`] retaining at most `offset.saturating_add(limit)` best rows.
    pub fn new(
        schema: Arc<Schema>,
        keys: Vec<SortKey>,
        limit: usize,
        offset: usize,
        ctx: ExecContext,
    ) -> Self {
        let keep = offset.saturating_add(limit);
        let res = ctx.pool.register("topk", false);
        let scratch = ctx.pool.register("topk_scratch", false);
        Self {
            schema,
            keys,
            limit,
            offset,
            keep,
            ctx,
            heap: Vec::new(),
            row_batches: Vec::new(),
            key_batches: Vec::new(),
            stored_rows: 0,
            next_seq: 0,
            res,
            scratch,
        }
    }

    /// Bytes currently reserved for kept rows and their sort keys.
    pub fn reserved_bytes(&self) -> usize {
        self.res.size()
    }

    fn compact_kept(&mut self) -> Result<(), QueryError> {
        let live_refs: Vec<(usize, usize)> = self
            .heap
            .iter()
            .filter_map(|item| match item.source {
                HeapSource::Stored { batch_idx, row_idx } => Some((batch_idx, row_idx)),
                HeapSource::Incoming { .. } => None,
            })
            .collect();
        let live_stored = live_refs.len();
        if live_stored == 0 {
            self.row_batches.clear();
            self.key_batches.clear();
            self.stored_rows = 0;
            reconcile_res(&mut self.res, 0)?;
            return Ok(());
        }
        if self.stored_rows <= live_stored {
            return Ok(());
        }

        let before = self.res.size();
        let built = (|| -> Result<(Batch, Vec<Column>), QueryError> {
            let mut new_cols = Vec::with_capacity(self.schema.len());
            for (col_idx, field) in self.schema.fields().iter().enumerate() {
                let sources: Vec<Option<(&Column, usize)>> = live_refs
                    .iter()
                    .map(|&(b, r)| Some((&self.row_batches[b].columns()[col_idx], r)))
                    .collect();
                let col = build_flat_from_sources(
                    &field.data_type,
                    &sources,
                    &mut Some(&mut self.res),
                    &self.ctx.eval,
                    before,
                )?;
                new_cols.push(col);
            }
            let new_batch = Batch::try_new_all(Arc::clone(&self.schema), new_cols, live_stored)?;

            let mut new_key_cols = Vec::with_capacity(self.keys.len());
            let key_batch_sample = live_refs[0].0;
            for k_idx in 0..self.keys.len() {
                let dt = self.key_batches[key_batch_sample][k_idx].data_type();
                let sources: Vec<Option<(&Column, usize)>> = live_refs
                    .iter()
                    .map(|&(b, r)| Some((&self.key_batches[b][k_idx], r)))
                    .collect();
                let col = build_flat_from_sources(
                    &dt,
                    &sources,
                    &mut Some(&mut self.res),
                    &self.ctx.eval,
                    before,
                )?;
                new_key_cols.push(col);
            }
            Ok((new_batch, new_key_cols))
        })();

        let (new_batch, new_key_cols) = match built {
            Ok(pair) => pair,
            Err(err) => {
                let cur = self.res.size();
                if cur > before {
                    self.res.shrink(cur - before);
                }
                return Err(err);
            }
        };

        let target_bytes =
            new_batch.memory_bytes() + new_key_cols.iter().map(Column::memory_bytes).sum::<usize>();
        if let Err(err) = reconcile_res(&mut self.res, target_bytes) {
            let cur = self.res.size();
            if cur > before {
                self.res.shrink(cur - before);
            }
            return Err(err);
        }

        self.row_batches = vec![new_batch];
        self.key_batches = vec![new_key_cols];
        self.stored_rows = live_stored;
        let mut pos = 0usize;
        for item in &mut self.heap {
            if matches!(item.source, HeapSource::Stored { .. }) {
                item.source = HeapSource::Stored {
                    batch_idx: 0,
                    row_idx: pos,
                };
                pos += 1;
            }
        }
        Ok(())
    }

    fn take_winners(
        &mut self,
        batch: &Batch,
        key_cols: &[Column],
        winner_rows: &[u32],
    ) -> Result<(Batch, Vec<Column>), QueryError> {
        let before = self.res.size();
        let mut out_cols = Vec::with_capacity(self.schema.len());
        for col in batch.columns() {
            match col.take(winner_rows, Some(&mut self.res)) {
                Ok(fc) => out_cols.push(Column::Flat(fc)),
                Err(err) => {
                    let cur = self.res.size();
                    if cur > before {
                        self.res.shrink(cur - before);
                    }
                    return Err(err.into());
                }
            }
        }
        let out_batch =
            match Batch::try_new_all(Arc::clone(&self.schema), out_cols, winner_rows.len()) {
                Ok(b) => b,
                Err(err) => {
                    let cur = self.res.size();
                    if cur > before {
                        self.res.shrink(cur - before);
                    }
                    return Err(err.into());
                }
            };

        let mut out_keys = Vec::with_capacity(key_cols.len());
        for kcol in key_cols {
            match kcol.take(winner_rows, Some(&mut self.res)) {
                Ok(fc) => out_keys.push(Column::Flat(fc)),
                Err(err) => {
                    let cur = self.res.size();
                    if cur > before {
                        self.res.shrink(cur - before);
                    }
                    return Err(err.into());
                }
            }
        }
        Ok((out_batch, out_keys))
    }

    /// Merges per-partition [`TopKSink`] heaps, skips `offset`, and emits sorted batches of at
    /// most `ctx.batch_size` rows (up to `limit` total rows).
    pub fn merge(
        mut sinks: Vec<TopKSink>,
        schema: Arc<Schema>,
        keys: &[SortKey],
        offset: usize,
        limit: usize,
        ctx: &ExecContext,
    ) -> Result<Vec<Batch>, QueryError> {
        if ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        for sink in &sinks {
            if sink.schema.as_ref() != schema.as_ref() {
                return Err(ColumnError::InvalidBatch {
                    reason: "TopKSink schema does not match merge schema",
                }
                .into());
            }
        }

        #[derive(Clone, Copy)]
        struct MergeEntry {
            sink_idx: usize,
            batch_idx: usize,
            row_idx: usize,
            seq: u64,
        }

        let total_kept: usize = sinks.iter().map(|s| s.heap.len()).sum();
        if total_kept <= offset {
            return Ok(Vec::new());
        }

        let mut entries = Vec::with_capacity(total_kept);
        for (sink_idx, sink) in sinks.iter().enumerate() {
            for item in &sink.heap {
                if let HeapSource::Stored { batch_idx, row_idx } = item.source {
                    entries.push(MergeEntry {
                        sink_idx,
                        batch_idx,
                        row_idx,
                        seq: item.seq,
                    });
                }
            }
        }

        if entries.len() <= offset {
            return Ok(Vec::new());
        }

        let keep_total = offset.saturating_add(limit).min(entries.len());
        let cmp = |a: &MergeEntry, b: &MergeEntry| {
            compare_rows_across(
                &sinks[a.sink_idx].key_batches[a.batch_idx],
                a.row_idx,
                &sinks[b.sink_idx].key_batches[b.batch_idx],
                b.row_idx,
                keys,
            )
            .then_with(|| (a.sink_idx, a.seq).cmp(&(b.sink_idx, b.seq)))
        };

        if keep_total < entries.len() {
            entries.select_nth_unstable_by(keep_total, cmp);
            entries.truncate(keep_total);
        }
        entries.sort_by(cmp);

        // Drop sort-key columns across all sinks before allocating output batches.
        for sink in &mut sinks {
            sink.key_batches.clear();
            let batch_bytes: usize = sink.row_batches.iter().map(Batch::memory_bytes).sum();
            reconcile_res(&mut sink.res, batch_bytes)?;
        }

        let result_entries = &entries[offset..];
        let chunk_size = ctx.batch_size.max(1);
        let mut out_batches = Vec::with_capacity(result_entries.len().div_ceil(chunk_size));
        let mut merge_res = ctx.pool.register("topk_merge", false);
        let mut schema_opt = Some(schema);
        let mut chunks = result_entries.chunks(chunk_size).peekable();

        while let Some(chunk) = chunks.next() {
            if ctx.eval.cancel.is_cancelled() {
                merge_res.free();
                return Err(QueryError::Cancelled);
            }
            let chunk_schema = if chunks.peek().is_none() {
                schema_opt
                    .take()
                    .ok_or_else(|| QueryError::internal("missing merge schema"))?
            } else {
                schema_opt
                    .as_ref()
                    .map(Arc::clone)
                    .ok_or_else(|| QueryError::internal("missing merge schema"))?
            };
            let before = merge_res.size();
            let mut cols = Vec::with_capacity(chunk_schema.len());
            for (col_idx, field) in chunk_schema.fields().iter().enumerate() {
                let sources: Vec<Option<(&Column, usize)>> = chunk
                    .iter()
                    .map(|e| {
                        Some((
                            &sinks[e.sink_idx].row_batches[e.batch_idx].columns()[col_idx],
                            e.row_idx,
                        ))
                    })
                    .collect();
                let col = match build_flat_from_sources(
                    &field.data_type,
                    &sources,
                    &mut Some(&mut merge_res),
                    &ctx.eval,
                    before,
                ) {
                    Ok(c) => c,
                    Err(err) => {
                        merge_res.free();
                        return Err(err);
                    }
                };
                cols.push(col);
            }
            let batch = match Batch::try_new_all(chunk_schema, cols, chunk.len()) {
                Ok(b) => b,
                Err(err) => {
                    merge_res.free();
                    return Err(err.into());
                }
            };
            out_batches.push(batch);
        }

        merge_res.free();
        drop(sinks);
        Ok(out_batches)
    }
}

impl PushSink for TopKSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        if self.limit == 0 {
            return Ok(ControlFlow::Stop);
        }
        if self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if batch.schema().as_ref() != self.schema.as_ref() {
            return Err(ColumnError::InvalidBatch {
                reason: "batch schema does not match TopKSink schema",
            }
            .into());
        }
        if batch.is_empty() {
            return Ok(ControlFlow::Continue);
        }
        if self.keys.is_empty() && self.heap.len() >= self.keep {
            return Ok(ControlFlow::Stop);
        }

        let mut key_cols = Vec::with_capacity(self.keys.len());
        for k in &self.keys {
            match eval_expr(&k.expr, &batch, &self.ctx.eval, Some(&mut self.scratch)) {
                Ok(col) => key_cols.push(col),
                Err(err) => {
                    self.scratch.free();
                    return Err(err);
                }
            }
        }

        // Scan active rows against the bounded max-heap.
        let keep = self.keep;
        let keys = &self.keys;
        let key_batches = &self.key_batches;
        let heap = &mut self.heap;
        let next_seq = &mut self.next_seq;
        let cancel = &self.ctx.eval.cancel;

        let scan_res = for_each_active_row(&batch, |row_idx| {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let seq = *next_seq;
            *next_seq = seq.wrapping_add(1);
            let cand = HeapItem {
                source: HeapSource::Incoming { row_idx },
                seq,
            };
            if heap.len() < keep {
                heap.push(cand);
                let last = heap.len() - 1;
                sift_up(heap, last, |a, b| {
                    compare_heap_items(a, b, key_batches, &key_cols, keys)
                });
                if keys.is_empty() && heap.len() == keep {
                    return Ok(false);
                }
            } else if let Some(worst) = heap.first()
                && compare_heap_items(&cand, worst, key_batches, &key_cols, keys) == Ordering::Less
            {
                heap[0] = cand;
                sift_down(heap, 0, |a, b| {
                    compare_heap_items(a, b, key_batches, &key_cols, keys)
                });
            }
            Ok(true)
        });
        if let Err(err) = scan_res {
            self.scratch.free();
            return Err(err);
        }

        // Materialize only surviving rows from `batch`.
        let mut winner_rows = Vec::new();
        for item in &self.heap {
            if let HeapSource::Incoming { row_idx } = item.source {
                winner_rows.push(row_idx as u32);
            }
        }

        if !winner_rows.is_empty() {
            let live_stored_count = self
                .heap
                .iter()
                .filter(|item| matches!(item.source, HeapSource::Stored { .. }))
                .count();
            let (won_batch, won_keys) = match self.take_winners(&batch, &key_cols, &winner_rows) {
                Ok(pair) => pair,
                Err(QueryError::TooLarge) if self.stored_rows > live_stored_count => {
                    // Evicted rows are holding memory; compact them before failing.
                    self.compact_kept().inspect_err(|_| self.scratch.free())?;
                    self.take_winners(&batch, &key_cols, &winner_rows)
                        .inspect_err(|_| self.scratch.free())?
                }
                Err(err) => {
                    self.scratch.free();
                    return Err(err);
                }
            };

            let updated_batch_idx = self.row_batches.len();
            self.row_batches.push(won_batch);
            self.key_batches.push(won_keys);
            self.stored_rows += winner_rows.len();

            let mut pos = 0usize;
            for item in &mut self.heap {
                if matches!(item.source, HeapSource::Incoming { .. }) {
                    item.source = HeapSource::Stored {
                        batch_idx: updated_batch_idx,
                        row_idx: pos,
                    };
                    pos += 1;
                }
            }
        }

        self.scratch.free();
        drop(key_cols);
        drop(batch);

        if self.stored_rows > self.keep.saturating_mul(2).max(64) {
            self.compact_kept()?;
        } else {
            reconcile_res(
                &mut self.res,
                kept_bytes(&self.row_batches, &self.key_batches),
            )?;
        }

        if self.keys.is_empty() && self.heap.len() >= self.keep {
            Ok(ControlFlow::Stop)
        } else {
            Ok(ControlFlow::Continue)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{
        BatchSource, CollectSink, ExecMetrics, SpillManager, WorkerPool, run_pipeline,
    };
    use crate::ir::{BinaryOp, EvalContext};
    use cetra_column::{Bitmap, FlatColumn, FlatData, GlobalPool, ScalarValue, Utf8Data};

    fn test_ctx(cap: usize, target_partitions: usize) -> (ExecContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: EvalContext {
                now_ns: 0,
                cancel: CancelToken::new(),
            },
            spill: Arc::new(SpillManager::with_fixed_cap(
                dir.path().to_path_buf(),
                1024 * 1024,
            )),
            workers: Arc::new(WorkerPool::new(4).unwrap()),
            batch_size: 8192,
            target_partitions,
            metrics: ExecMetrics::new(),
        };
        (ctx, dir)
    }

    fn col_expr(index: usize, name: &str, data_type: DataType) -> Expr {
        Expr::Column {
            index,
            name: name.to_owned(),
            data_type,
        }
    }

    fn gt_i64(col: Expr, val: i64) -> Expr {
        Expr::Binary {
            left: Box::new(col),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(ScalarValue::I64(val))),
            data_type: DataType::Bool,
        }
    }

    #[test]
    fn filter_project_limit_zero_copy_and_parallel_cas() {
        let (ctx, _dir) = test_ctx(1024 * 1024, 4);
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("tag", DataType::Utf8, false),
        ])
        .unwrap();

        // 1. Zero-copy Selection::Mask vs compaction threshold (active * 4 < raw_len)
        let b_dense = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![1, 2, 3, 4, 5, 6, 7, 8]), None).unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["a", "b", "c", "d", "e", "f", "g", "h"])
                                .unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            8,
        )
        .unwrap();

        // Filter id > 4 -> 4 active out of 8 (4 * 4 < 8 is false -> keeps Selection::Mask, zero-copy)
        let mut f_dense = FilterOp::new(gt_i64(col_expr(0, "id", DataType::I64), 4), ctx.clone());
        let mut dense_out = Vec::new();
        let flow = f_dense
            .push(b_dense.clone(), &mut |out| {
                dense_out.push(out);
                Ok(ControlFlow::Continue)
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue);
        assert_eq!(dense_out.len(), 1);
        assert!(matches!(dense_out[0].selection(), Selection::Mask(_)));
        assert_eq!(dense_out[0].raw_len(), 8);
        assert_eq!(dense_out[0].num_rows(), 4);
        assert_eq!(ctx.pool.reserved(), 0);

        // Filter id > 7 -> 1 active out of 8 (1 * 4 < 8 is true -> compacted to Selection::All)
        let mut f_sparse = FilterOp::new(gt_i64(col_expr(0, "id", DataType::I64), 7), ctx.clone());
        let mut sparse_out = Vec::new();
        f_sparse
            .push(b_dense.clone(), &mut |out| {
                sparse_out.push(out);
                Ok(ControlFlow::Continue)
            })
            .unwrap();
        assert_eq!(sparse_out.len(), 1);
        assert_eq!(sparse_out[0].selection(), &Selection::All);
        assert_eq!(sparse_out[0].raw_len(), 1);
        assert_eq!(sparse_out[0].num_rows(), 1);
        assert_eq!(ctx.pool.reserved(), 0);

        // Filter id > 100 -> 0 active -> dropped
        let mut f_empty = FilterOp::new(gt_i64(col_expr(0, "id", DataType::I64), 100), ctx.clone());
        let mut empty_out = Vec::new();
        f_empty
            .push(b_dense.clone(), &mut |out| {
                empty_out.push(out);
                Ok(ControlFlow::Continue)
            })
            .unwrap();
        assert!(empty_out.is_empty());
        assert_eq!(ctx.pool.reserved(), 0);

        // 2. ProjectOp preserves selection and frees scratch Reservation after emit
        let mut proj = ProjectOp::new(
            vec![(
                "id_plus_10".to_owned(),
                Expr::Binary {
                    left: Box::new(col_expr(0, "id", DataType::I64)),
                    op: BinaryOp::Add,
                    right: Box::new(Expr::Literal(ScalarValue::I64(10))),
                    data_type: DataType::I64,
                },
            )],
            ctx.clone(),
        );
        let mut proj_out = Vec::new();
        proj.push(dense_out.pop().unwrap(), &mut |out| {
            // While inside emit, scratch reservation is active.
            assert!(ctx.pool.reserved() >= out.memory_bytes());
            proj_out.push(out);
            Ok(ControlFlow::Continue)
        })
        .unwrap();
        assert_eq!(ctx.pool.reserved(), 0);
        assert_eq!(proj_out.len(), 1);
        assert!(matches!(proj_out[0].selection(), Selection::Mask(_)));
        assert_eq!(proj_out[0].num_rows(), 4);
        let compacted_proj = proj_out[0].compact(None).unwrap();
        assert_eq!(
            compacted_proj.column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(15)
        );
        assert_eq!(
            compacted_proj.column(0).unwrap().get_scalar(3).unwrap(),
            ScalarValue::I64(18)
        );

        // 3. LimitOp: limit == Some(0) stops immediately; offset + limit slices zero-copy
        let mut lim_zero = LimitOp::new(2, Some(0), None);
        let mut zero_out = Vec::new();
        let flow_zero = lim_zero
            .push(b_dense.clone(), &mut |out| {
                zero_out.push(out);
                Ok(ControlFlow::Continue)
            })
            .unwrap();
        assert_eq!(flow_zero, ControlFlow::Stop);
        assert!(zero_out.is_empty());

        let mut lim_off = LimitOp::new(3, Some(2), None);
        let mut off_out = Vec::new();
        let flow_off = lim_off
            .push(b_dense, &mut |out| {
                off_out.push(out);
                Ok(ControlFlow::Continue)
            })
            .unwrap();
        assert_eq!(flow_off, ControlFlow::Stop);
        assert_eq!(off_out.len(), 1);
        assert_eq!(off_out[0].raw_len(), 8);
        assert_eq!(off_out[0].num_rows(), 2);
        let off_compacted = off_out[0].compact(None).unwrap();
        assert_eq!(
            off_compacted.column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(4)
        );
        assert_eq!(
            off_compacted.column(0).unwrap().get_scalar(1).unwrap(),
            ScalarValue::I64(5)
        );

        // 4. Parallel LimitOp with shared_remaining CAS across 4 partitions never overshoots
        let batches: Vec<Batch> = (0..16)
            .map(|chunk| {
                let start = chunk * 50;
                let vals: Vec<i64> = (start..start + 50).collect();
                let tags: Vec<String> = vals.iter().map(|v| format!("t{v}")).collect();
                let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
                Batch::try_new_all(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None).unwrap()),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(Utf8Data::try_from_slices(tag_refs).unwrap()),
                                None,
                            )
                            .unwrap(),
                        ),
                    ],
                    50,
                )
                .unwrap()
            })
            .collect();
        let source = BatchSource::new(Arc::clone(&schema), batches).unwrap();
        let shared = Arc::new(AtomicUsize::new(73));
        let partitions = source.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        assert_eq!(partitions.len(), 4);

        let pool_for_sink = Arc::clone(&ctx.pool);
        let shared_for_chain = Arc::clone(&shared);
        let sinks = run_pipeline(
            &ctx,
            partitions,
            move |_| {
                Ok(vec![Box::new(LimitOp::new(
                    0,
                    Some(73),
                    Some(Arc::clone(&shared_for_chain)),
                ))])
            },
            move |_| Ok(CollectSink::new(&pool_for_sink)),
        )
        .unwrap();

        let total_rows: usize = sinks
            .iter()
            .flat_map(|s| s.batches())
            .map(Batch::num_rows)
            .sum();
        assert_eq!(total_rows, 73);
        assert_eq!(shared.load(AtomicOrdering::Acquire), 0);
        drop(sinks);
        assert_eq!(ctx.pool.reserved(), 0);
    }

    #[test]
    fn topk_total_order_nulls_first_last_and_multi_partition_merge() {
        let (ctx, _dir) = test_ctx(1024 * 1024, 3);
        let schema = Schema::try_new_arc(vec![
            Field::new("score", DataType::F64, true),
            Field::new("tie", DataType::I64, false),
            Field::new("label", DataType::Utf8, false),
        ])
        .unwrap();

        // 1. limit == 0 returns ControlFlow::Stop immediately
        let mut sink_zero = TopKSink::new(
            Arc::clone(&schema),
            vec![SortKey {
                expr: col_expr(0, "score", DataType::F64),
                descending: false,
                nulls_first: false,
            }],
            0,
            2,
            ctx.clone(),
        );
        let b_one = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::F64(vec![1.0]), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["a"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            1,
        )
        .unwrap();
        assert_eq!(sink_zero.consume(b_one).unwrap(), ControlFlow::Stop);
        assert_eq!(sink_zero.reserved_bytes(), 0);
        let merged_zero =
            TopKSink::merge(vec![sink_zero], Arc::clone(&schema), &[], 2, 0, &ctx).unwrap();
        assert!(merged_zero.is_empty());

        // 2. Multi-partition merge with F64 total order (-inf, -0.0, +0.0, +inf, NaN, NULL) + tie-breaker + offset/limit
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![f64::NAN, 0.0, 0.0, f64::NEG_INFINITY]),
                        Some(Bitmap::from_bools(&[true, false, true, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![10, 99, 20, 5]), None).unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["nan_10", "null_99", "pos0_20", "neginf_5"])
                                .unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            4,
        )
        .unwrap();
        let b2 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![-0.0, f64::INFINITY, 0.0, 5.0]),
                        Some(Bitmap::from_bools(&[true, true, false, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 1, 50, 2]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["neg0_10", "inf_1", "null_50", "five_2"])
                                .unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            4,
        )
        .unwrap();
        let b3 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::F64(vec![f64::NAN, 5.0]), None).unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![3, 1]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["nan_3", "five_1"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();

        let source = BatchSource::new(Arc::clone(&schema), vec![b1, b2, b3]).unwrap();

        // Case A: score ASC NULLS FIRST, tie DESC -> total order of all 10 rows:
        // null_99, null_50, neginf_5, pos0_20 (-0.0/0.0 tie: 20 before 10), neg0_10, five_2, five_1, inf_1, nan_10, nan_3
        let keys_nulls_first = vec![
            SortKey {
                expr: col_expr(0, "score", DataType::F64),
                descending: false,
                nulls_first: true,
            },
            SortKey {
                expr: col_expr(1, "tie", DataType::I64),
                descending: true,
                nulls_first: false,
            },
        ];

        let parts = source.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        let schema_c = Arc::clone(&schema);
        let keys_c = keys_nulls_first.clone();
        let ctx_c = ctx.clone();
        let sinks = run_pipeline(
            &ctx,
            parts,
            |_| Ok(vec![]),
            move |_| {
                Ok(TopKSink::new(
                    Arc::clone(&schema_c),
                    keys_c.clone(),
                    6,
                    1,
                    ctx_c.clone(),
                ))
            },
        )
        .unwrap();

        let out =
            TopKSink::merge(sinks, Arc::clone(&schema), &keys_nulls_first, 1, 6, &ctx).unwrap();
        let labels: Vec<String> = out
            .iter()
            .flat_map(|b| {
                let col = b.column(2).unwrap();
                (0..b.num_rows()).map(move |r| match col.get_scalar(r).unwrap() {
                    ScalarValue::Utf8(s) => s,
                    other => panic!("unexpected scalar: {other:?}"),
                })
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "null_50", "neginf_5", "pos0_20", "neg0_10", "five_2", "five_1"
            ]
        );
        drop(out);
        assert_eq!(ctx.pool.reserved(), 0);

        // Case B: score DESC NULLS LAST, tie ASC -> total order:
        // nan_3, nan_10, inf_1, five_1, five_2, neg0_10, pos0_20, neginf_5, null_50, null_99
        let keys_desc_nulls_last = vec![
            SortKey {
                expr: col_expr(0, "score", DataType::F64),
                descending: true,
                nulls_first: false,
            },
            SortKey {
                expr: col_expr(1, "tie", DataType::I64),
                descending: false,
                nulls_first: false,
            },
        ];
        let parts_b = source.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        let schema_c = Arc::clone(&schema);
        let keys_c = keys_desc_nulls_last.clone();
        let ctx_c = ctx.clone();
        let sinks_b = run_pipeline(
            &ctx,
            parts_b,
            |_| Ok(vec![]),
            move |_| {
                Ok(TopKSink::new(
                    Arc::clone(&schema_c),
                    keys_c.clone(),
                    5,
                    6,
                    ctx_c.clone(),
                ))
            },
        )
        .unwrap();
        let out_b = TopKSink::merge(
            sinks_b,
            Arc::clone(&schema),
            &keys_desc_nulls_last,
            6,
            5,
            &ctx,
        )
        .unwrap();
        let labels_b: Vec<String> = out_b
            .iter()
            .flat_map(|b| {
                let col = b.column(2).unwrap();
                (0..b.num_rows()).map(move |r| match col.get_scalar(r).unwrap() {
                    ScalarValue::Utf8(s) => s,
                    other => panic!("unexpected scalar: {other:?}"),
                })
            })
            .collect();
        assert_eq!(labels_b, vec!["pos0_20", "neginf_5", "null_50", "null_99"]);
        drop(out_b);
        assert_eq!(ctx.pool.reserved(), 0);

        // 3. Eviction compaction bounds memory across many descending batches for Top-3
        let small_schema =
            Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let small_keys = vec![SortKey {
            expr: col_expr(0, "v", DataType::I64),
            descending: false,
            nulls_first: false,
        }];
        let mut sink_compact = TopKSink::new(
            Arc::clone(&small_schema),
            small_keys.clone(),
            3,
            0,
            ctx.clone(),
        );
        for round in (0..200).rev() {
            let b = Batch::try_new_all(
                Arc::clone(&small_schema),
                vec![Column::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![round]), None).unwrap(),
                )],
                1,
            )
            .unwrap();
            assert_eq!(sink_compact.consume(b).unwrap(), ControlFlow::Continue);
        }
        assert!(sink_compact.stored_rows <= 64);
        let merged_small = TopKSink::merge(
            vec![sink_compact],
            Arc::clone(&small_schema),
            &small_keys,
            0,
            3,
            &ctx,
        )
        .unwrap();
        let vals: Vec<i64> = merged_small
            .iter()
            .flat_map(|b| {
                let col = b.column(0).unwrap();
                (0..b.num_rows()).map(move |r| match col.get_scalar(r).unwrap() {
                    ScalarValue::I64(v) => v,
                    other => panic!("unexpected scalar: {other:?}"),
                })
            })
            .collect();
        assert_eq!(vals, vec![0, 1, 2]);
        drop(merged_small);
        assert_eq!(ctx.pool.reserved(), 0);
    }

    #[test]
    fn unnest_preserve_nulls_true_and_false_with_multi_list_zip() {
        let (mut ctx, _dir) = test_ctx(1024 * 1024, 1);
        ctx.batch_size = 3; // Exercise chunked output emission

        let schema = Schema::try_new_arc(vec![
            Field::new("row_id", DataType::I64, false),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
            Field::new("nums", DataType::List(Box::new(DataType::I64)), true),
        ])
        .unwrap();

        // Rows:
        // 0: tags=["a", "b", "c"], nums=[10]          -> max=3: (0,"a",10), (0,"b",NULL), (0,"c",NULL)
        // 1: tags=[],              nums=[]            -> max=0: preserve_nulls=true -> (1,NULL,NULL); false -> dropped
        // 2: tags=NULL,            nums=[20, 30]      -> max=2: (2,NULL,20), (2,NULL,30)
        // 3: tags=NULL,            nums=NULL          -> max=0: preserve_nulls=true -> (3,NULL,NULL); false -> dropped
        // 4: (filtered out by Selection::Mask!)
        let tags_values = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["a", "b", "c", "skip"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let tags_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 3, 3, 3, 3, 4],
                    values: Box::new(tags_values),
                },
                Some(Bitmap::from_bools(&[true, true, false, false, true])),
            )
            .unwrap(),
        );

        let nums_values =
            Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20, 30, 999]), None).unwrap());
        let nums_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::I64,
                    offsets: vec![0, 1, 1, 3, 3, 4],
                    values: Box::new(nums_values),
                },
                Some(Bitmap::from_bools(&[true, true, true, false, true])),
            )
            .unwrap(),
        );

        let batch = Batch::try_new(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![0, 1, 2, 3, 4]), None).unwrap(),
                ),
                tags_col,
                nums_col,
            ],
            5,
            Selection::Mask(Bitmap::from_bools(&[true, true, true, true, false])),
        )
        .unwrap();

        let collect_rows = |batches: &[Batch]| -> Vec<(i64, Option<String>, Option<i64>)> {
            batches
                .iter()
                .flat_map(|b| {
                    (0..b.num_rows()).map(move |r| {
                        let id = match b.column(0).unwrap().get_scalar(r).unwrap() {
                            ScalarValue::I64(v) => v,
                            other => panic!("unexpected id: {other:?}"),
                        };
                        let tag = match b.column(1).unwrap().get_scalar(r).unwrap() {
                            ScalarValue::Utf8(s) => Some(s),
                            ScalarValue::Null(_) => None,
                            other => panic!("unexpected tag: {other:?}"),
                        };
                        let num = match b.column(2).unwrap().get_scalar(r).unwrap() {
                            ScalarValue::I64(v) => Some(v),
                            ScalarValue::Null(_) => None,
                            other => panic!("unexpected num: {other:?}"),
                        };
                        (id, tag, num)
                    })
                })
                .collect()
        };

        // preserve_nulls = true -> 3 + 1 + 2 + 1 = 7 rows across chunk_size=3 batches (3, 3, 1)
        let mut op_preserve = UnnestOp::new(vec![1, 2], true, ctx.clone());
        let mut out_preserve = Vec::new();
        op_preserve
            .push(batch.clone(), &mut |b| {
                assert!(ctx.pool.reserved() >= b.memory_bytes());
                assert!(b.num_rows() <= 3);
                out_preserve.push(b);
                Ok(ControlFlow::Continue)
            })
            .unwrap();
        assert_eq!(ctx.pool.reserved(), 0);
        assert_eq!(out_preserve.len(), 3);
        assert_eq!(
            collect_rows(&out_preserve),
            vec![
                (0, Some("a".to_owned()), Some(10)),
                (0, Some("b".to_owned()), None),
                (0, Some("c".to_owned()), None),
                (1, None, None),
                (2, None, Some(20)),
                (2, None, Some(30)),
                (3, None, None),
            ]
        );

        // preserve_nulls = false -> 3 + 0 + 2 + 0 = 5 rows
        let mut op_drop = UnnestOp::new(vec![1, 2], false, ctx.clone());
        let mut out_drop = Vec::new();
        op_drop
            .push(batch, &mut |b| {
                assert!(ctx.pool.reserved() >= b.memory_bytes());
                assert!(b.num_rows() <= 3);
                out_drop.push(b);
                Ok(ControlFlow::Continue)
            })
            .unwrap();
        assert_eq!(ctx.pool.reserved(), 0);
        assert_eq!(
            collect_rows(&out_drop),
            vec![
                (0, Some("a".to_owned()), Some(10)),
                (0, Some("b".to_owned()), None),
                (0, Some("c".to_owned()), None),
                (2, None, Some(20)),
                (2, None, Some(30)),
            ]
        );
    }

    #[test]
    fn union_source_concatenates_and_validates_schemas() {
        let (ctx, _dir) = test_ctx(1024 * 1024, 2);
        let union_schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("note", DataType::Utf8, true),
        ])
        .unwrap();
        let child_schema_non_null = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("other_name", DataType::Utf8, false),
        ])
        .unwrap();
        let bad_type_schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::F64, false),
            Field::new("note", DataType::Utf8, true),
        ])
        .unwrap();

        let s_bad = Arc::new(BatchSource::empty(bad_type_schema));
        assert!(UnionSource::new(Arc::clone(&union_schema), vec![s_bad]).is_err());

        let b1 = Batch::try_new_all(
            Arc::clone(&union_schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["x", "y"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();
        let b2 = Batch::try_new_all(
            Arc::clone(&child_schema_non_null),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![3, 4]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["z", "w"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();

        let src1: Arc<dyn ScanSource> =
            Arc::new(BatchSource::new(Arc::clone(&union_schema), vec![b1]).unwrap());
        let src2: Arc<dyn ScanSource> =
            Arc::new(BatchSource::new(child_schema_non_null, vec![b2]).unwrap());
        let union = UnionSource::new(Arc::clone(&union_schema), vec![src1, src2]).unwrap();

        let spec = ScanSpec {
            filters: vec![gt_i64(col_expr(0, "id", DataType::I64), 1)],
            projection: Some(vec![0]),
            ..ScanSpec::default()
        };
        let proj_schema = union.projected_schema(&spec).unwrap();
        let parts = union.open_partitions(&spec, &ctx).unwrap();
        assert!(parts.len() <= ctx.target_partitions);

        let pool_c = Arc::clone(&ctx.pool);
        let sinks = run_pipeline(
            &ctx,
            parts,
            |_| Ok(vec![]),
            move |_| Ok(CollectSink::new(&pool_c)),
        )
        .unwrap();
        let mut ids: Vec<i64> = sinks
            .iter()
            .flat_map(|s| s.batches())
            .flat_map(|b| {
                assert_eq!(b.schema().as_ref(), proj_schema.as_ref());
                let compacted = b.compact(None).unwrap();
                let col = compacted.column(0).unwrap();
                (0..compacted.num_rows())
                    .map(|r| match col.get_scalar(r).unwrap() {
                        ScalarValue::I64(v) => v,
                        other => panic!("unexpected scalar: {other:?}"),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![2, 3, 4]);
        drop(sinks);
        assert_eq!(ctx.pool.reserved(), 0);
    }

    #[test]
    fn topk_tight_pool_eviction_compaction_never_returns_internal() {
        let schema = Schema::try_new_arc(vec![
            Field::new("v", DataType::I64, false),
            Field::new("pad", DataType::Utf8, false),
        ])
        .unwrap();
        let keys = vec![SortKey {
            expr: col_expr(0, "v", DataType::I64),
            descending: false,
            nulls_first: false,
        }];
        let make_batch = |vals: &[i64]| {
            let pads: Vec<String> = vals.iter().map(|v| format!("{v:0>200}")).collect();
            let pad_refs: Vec<&str> = pads.iter().map(String::as_str).collect();
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vals.to_vec()), None).unwrap()),
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(pad_refs).unwrap()),
                            None,
                        )
                        .unwrap(),
                    ),
                ],
                vals.len(),
            )
            .unwrap()
        };

        // Measure memory footprint of 3 stored rows (2 dead + 1 live) + 1 live row + 2 new rows.
        let (probe_ctx, _probe_dir) = test_ctx(1024 * 1024, 1);
        let mut probe = TopKSink::new(Arc::clone(&schema), keys.clone(), 3, 0, probe_ctx.clone());
        probe.consume(make_batch(&[50, 40, 30])).unwrap();
        probe.consume(make_batch(&[20])).unwrap();
        let bytes_before_b3 = probe.reserved_bytes(); // 4 stored rows: 50 (dead), 40, 30, 20
        assert_eq!(probe.stored_rows, 4);
        probe.consume(make_batch(&[15])).unwrap();
        let bytes_5_rows = probe.reserved_bytes(); // 5 stored rows (30..50 dead, 20 & 15 live)
        drop(probe);

        // Case 1: Pool fits (4 stored rows + 1 compacted live row), so take_winners on [10, 5]
        // fails with TooLarge, compact_kept compacts the 1 live stored row (20) while leaving
        // Incoming [10, 5] untouched, freeing enough memory for take_winners to succeed.
        let one_row_est = bytes_5_rows - bytes_before_b3;
        let tight_ok_cap = bytes_before_b3 + one_row_est + 64;
        let (ok_ctx, _d_ok) = test_ctx(tight_ok_cap, 1);
        let mut sink_compact_ok =
            TopKSink::new(Arc::clone(&schema), keys.clone(), 3, 0, ok_ctx.clone());
        sink_compact_ok.consume(make_batch(&[50, 40, 30])).unwrap();
        sink_compact_ok.consume(make_batch(&[20])).unwrap();
        assert_eq!(sink_compact_ok.stored_rows, 4);
        let res_b3 = sink_compact_ok.consume(make_batch(&[10, 5]));
        assert!(
            matches!(res_b3, Ok(ControlFlow::Continue)),
            "expected Ok after compaction, got {res_b3:?}"
        );
        assert_eq!(sink_compact_ok.stored_rows, 3);
        let merged = TopKSink::merge(
            vec![sink_compact_ok],
            Arc::clone(&schema),
            &keys,
            0,
            3,
            &probe_ctx,
        )
        .unwrap();
        let got: Vec<i64> = merged
            .iter()
            .flat_map(|b| {
                let col = b.column(0).unwrap();
                (0..b.num_rows()).map(move |r| match col.get_scalar(r).unwrap() {
                    ScalarValue::I64(v) => v,
                    other => panic!("unexpected scalar: {other:?}"),
                })
            })
            .collect();
        assert_eq!(got, vec![5, 10, 20]);
        drop(merged);
        assert_eq!(ok_ctx.pool.reserved(), 0);

        // Case 2: All stored rows are evicted by incoming batch (live_stored == 0) under tight cap
        let (zero_live_ctx, _d_zero) = test_ctx(bytes_before_b3 + 64, 1);
        let mut sink_zero_live = TopKSink::new(
            Arc::clone(&schema),
            keys.clone(),
            3,
            0,
            zero_live_ctx.clone(),
        );
        sink_zero_live.consume(make_batch(&[50, 40, 30])).unwrap();
        sink_zero_live.consume(make_batch(&[20])).unwrap();
        let res_evict_all = sink_zero_live.consume(make_batch(&[3, 2, 1]));
        assert!(
            matches!(res_evict_all, Ok(ControlFlow::Continue)),
            "expected Ok when all stored rows evicted, got {res_evict_all:?}"
        );
        assert_eq!(sink_zero_live.stored_rows, 3);
        drop(sink_zero_live);
        assert_eq!(zero_live_ctx.pool.reserved(), 0);

        // Case 3: Ultra-tight cap where even compact_kept cannot fit -> clean TooLarge, NEVER Internal
        let (too_tight_ctx, _d_tight) = test_ctx(bytes_before_b3 + 16, 1);
        let mut sink_too_tight =
            TopKSink::new(Arc::clone(&schema), keys, 3, 0, too_tight_ctx.clone());
        sink_too_tight.consume(make_batch(&[50, 40, 30])).unwrap();
        sink_too_tight.consume(make_batch(&[20])).unwrap();
        let err = sink_too_tight.consume(make_batch(&[10, 5])).unwrap_err();
        assert!(
            matches!(err, QueryError::TooLarge),
            "expected QueryError::TooLarge, got {err:?}"
        );
        drop(sink_too_tight);
        assert_eq!(too_tight_ctx.pool.reserved(), 0);
    }

    #[test]
    fn union_source_remaps_scan_spec_hints_for_differing_child_column_names() {
        use std::sync::Mutex;

        #[derive(Debug)]
        struct CaptureHintSource {
            inner: BatchSource,
            captured: Arc<Mutex<Option<ScanSpec>>>,
        }

        impl ScanSource for CaptureHintSource {
            fn schema(&self) -> Arc<Schema> {
                self.inner.schema()
            }

            fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
                self.inner.projected_schema(spec)
            }

            fn open_partitions(
                &self,
                spec: &ScanSpec,
                ctx: &ExecContext,
            ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
                *self.captured.lock().unwrap() = Some(spec.clone());
                self.inner.open_partitions(spec, ctx)
            }

            fn explain_summary(&self, spec: &ScanSpec) -> String {
                self.inner.explain_summary(spec)
            }
        }

        let (ctx, _dir) = test_ctx(1024 * 1024, 1);
        let union_schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("note", DataType::Utf8, true),
        ])
        .unwrap();
        let child_schema = Schema::try_new_arc(vec![
            Field::new("child_id", DataType::I64, false),
            Field::new("child_note", DataType::Utf8, true),
        ])
        .unwrap();

        let captured = Arc::new(Mutex::new(None));
        let child = Arc::new(CaptureHintSource {
            inner: BatchSource::empty(child_schema),
            captured: Arc::clone(&captured),
        });
        let union = UnionSource::new(union_schema, vec![child]).unwrap();

        let spec = ScanSpec {
            projection: Some(vec![0, 1]),
            map_keys: vec![("note".to_owned(), "k".to_owned())],
            id_lookups: vec![IdLookupHint::Utf8 {
                column: "note".to_owned(),
                values: vec!["u1".to_owned()],
            }],
            substrings: vec![SubstringHint {
                column: "note".to_owned(),
                needle: "abc".to_owned(),
                case_insensitive: true,
            }],
            sort_hints: vec![SortHint {
                column: "id".to_owned(),
                descending: true,
            }],
            ..ScanSpec::default()
        };

        let _ = union.open_partitions(&spec, &ctx).unwrap();
        let got = captured.lock().unwrap().take().unwrap();
        assert_eq!(
            got.map_keys,
            vec![("child_note".to_owned(), "k".to_owned())]
        );
        assert_eq!(
            got.id_lookups,
            vec![IdLookupHint::Utf8 {
                column: "child_note".to_owned(),
                values: vec!["u1".to_owned()],
            }]
        );
        assert_eq!(
            got.substrings,
            vec![SubstringHint {
                column: "child_note".to_owned(),
                needle: "abc".to_owned(),
                case_insensitive: true,
            }]
        );
        assert_eq!(
            got.sort_hints,
            vec![SortHint {
                column: "child_id".to_owned(),
                descending: true,
            }]
        );
    }

    #[test]
    fn project_op_with_schema_preserves_nullability() {
        let (ctx, _dir) = test_ctx(1024 * 1024, 1);
        let in_schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::I64, false),
            Field::new("b", DataType::Utf8, true),
        ])
        .unwrap();
        let out_schema = Schema::try_new_arc(vec![
            Field::new("a_out", DataType::I64, false),
            Field::new("b_out", DataType::Utf8, true),
        ])
        .unwrap();

        // Validation errors: count mismatch & type mismatch.
        assert!(ProjectOp::with_schema(vec![], Arc::clone(&out_schema), ctx.clone()).is_err());
        assert!(
            ProjectOp::with_schema(
                vec![
                    Expr::Column {
                        index: 0,
                        name: "a".to_owned(),
                        data_type: DataType::I64,
                    },
                    Expr::Column {
                        index: 0,
                        name: "a".to_owned(),
                        data_type: DataType::I64,
                    },
                ],
                Arc::clone(&out_schema),
                ctx.clone(),
            )
            .is_err()
        );

        let mut op = ProjectOp::with_schema(
            vec![
                Expr::Column {
                    index: 0,
                    name: "a".to_owned(),
                    data_type: DataType::I64,
                },
                Expr::Column {
                    index: 1,
                    name: "b".to_owned(),
                    data_type: DataType::Utf8,
                },
            ],
            Arc::clone(&out_schema),
            ctx.clone(),
        )
        .unwrap();

        let batch = Batch::try_new_all(
            Arc::clone(&in_schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["x", ""]).unwrap()),
                        Some(Bitmap::from_bools(&[true, false])),
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();

        let mut emitted = Vec::new();
        op.push(batch, &mut |b| {
            emitted.push(b);
            Ok(ControlFlow::Continue)
        })
        .unwrap();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].schema().as_ref(), out_schema.as_ref());
        assert!(!emitted[0].schema().fields()[0].nullable);
        assert!(emitted[0].schema().fields()[1].nullable);

        // Non-null schema field rejects a null value at runtime.
        let strict_schema =
            Schema::try_new_arc(vec![Field::new("b_strict", DataType::Utf8, false)]).unwrap();
        let mut strict_op = ProjectOp::with_schema(
            vec![Expr::Column {
                index: 1,
                name: "b".to_owned(),
                data_type: DataType::Utf8,
            }],
            strict_schema,
            ctx,
        )
        .unwrap();
        let batch_with_null = Batch::try_new_all(
            in_schema,
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices([""]).unwrap()),
                        Some(Bitmap::from_bools(&[false])),
                    )
                    .unwrap(),
                ),
            ],
            1,
        )
        .unwrap();
        let err = strict_op
            .push(batch_with_null, &mut |_| Ok(ControlFlow::Continue))
            .unwrap_err();
        assert!(matches!(err, QueryError::Column(_)));
    }
}
