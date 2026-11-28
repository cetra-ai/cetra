//! Native late-materialized Top-K pushdown (`ORDER BY col DESC|ASC LIMIT k`, `NULLS LAST`).
//!
//! Evaluates Phase-1 filters and the order-by column only, dynamically prunes Row Groups and
//! mini-blocks whose zone-map bounds cannot beat the current `k`-th heap cutoff, and materializes
//! Phase-2 projected columns strictly for the winning `<= k` rows.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use cetra_column::{
    Batch, Bitmap, CancelToken, Column, DataType, FlatData, Reservation, ScalarValue, Schema,
    Selection,
};

use crate::StatamError;
use crate::codec::decode_mini_block_selected;
use crate::format::{
    BLOCK_FLAG_HAS_NAN, BLOCK_FLAG_MIN_MAX_VALID, BLOCK_FRAME_HEADER_LEN, BlockEntry,
    COL_FLAG_HAS_FILE_DICT, Encoding, LocalityGroup,
};
use crate::payload::search::RetainedPackChunks;

use super::payload::ParsedL0Payload;
use super::plan::{BlockCacheReader, DecodedPhase1, ResolvedFilterExpr};
use super::{
    IO_WAVE_MAX_BYTES, IdLookup, MapReadMode, Predicate, ScanRequest, StatamReader, SubstringQuery,
    TimeRange,
};

/// Late-materialized Top-K pushdown specification for [`ScanRequest::top_k`].
///
/// Orders surviving rows by `col_idx` (`NULLS LAST`; ties break deterministically by ascending
/// 0-based file row index) and materializes only the winning `<= limit` rows in Phase 2,
/// skipping Row Groups and mini-blocks whose zone-map bounds cannot enter the top-K heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopKSpec {
    /// Logical column index to order by (`I64`, `TimestampNs`, `Date`, `F64`).
    pub col_idx: usize,
    /// `true` for `ORDER BY col DESC LIMIT k`, `false` for `ASC`.
    pub descending: bool,
    /// Maximum number of rows to return (`0` returns empty).
    pub limit: usize,
}

type TopKScanOutput = (Vec<Batch>, Vec<u64>, Vec<Option<TopKKey>>);

impl TopKSpec {
    /// Creates a descending Top-K specification (`ORDER BY col_idx DESC LIMIT limit`).
    pub fn desc(col_idx: usize, limit: usize) -> Self {
        Self {
            col_idx,
            descending: true,
            limit,
        }
    }

    /// Creates an ascending Top-K specification (`ORDER BY col_idx ASC LIMIT limit`).
    pub fn asc(col_idx: usize, limit: usize) -> Self {
        Self {
            col_idx,
            descending: false,
            limit,
        }
    }
}

/// Returns `true` if `dt` is supported by [`TopKSpec`].
pub(super) fn is_supported_top_k_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::I64 | DataType::TimestampNs { .. } | DataType::Date | DataType::F64
    )
}

#[derive(Debug, Clone, Copy)]
enum TopKKey {
    I64(i64),
    F64(f64),
}

impl PartialEq for TopKKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for TopKKey {}

impl PartialOrd for TopKKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopKKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (*self, *other) {
            (Self::I64(a), Self::I64(b)) => a.cmp(&b),
            (Self::F64(a), Self::F64(b)) => a.total_cmp(&b),
            // Keys within one query share a single column DataType; fallback variant order keeps
            // `Ord` total without panicking.
            (Self::I64(_), Self::F64(_)) => Ordering::Less,
            (Self::F64(_), Self::I64(_)) => Ordering::Greater,
        }
    }
}

/// Heap element ordered so that the *worst* element currently in the Top-K sits at the max-heap
/// root (`.peek()`), enabling O(1) cutoff checks and O(log K) eviction.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct TopKHeapEntry {
    key: TopKKey,
    global_row: u64,
    descending: bool,
}

impl TopKHeapEntry {
    /// Returns `true` if `(cand_key, cand_row)` strictly beats `self` under `self.descending`.
    #[inline]
    fn beaten_by(&self, cand_key: TopKKey, cand_row: u64) -> bool {
        let key_ord = if self.descending {
            cand_key.cmp(&self.key)
        } else {
            self.key.cmp(&cand_key)
        };
        match key_ord {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => cand_row < self.global_row,
        }
    }
}

impl PartialOrd for TopKHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TopKHeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Worse item compares GREATER so Rust's max-heap puts the worst item at `.peek()`.
        if self.descending {
            (Reverse(self.key), self.global_row).cmp(&(Reverse(other.key), other.global_row))
        } else {
            (self.key, self.global_row).cmp(&(other.key, other.global_row))
        }
    }
}

/// Decodes exact scalar `(min, max)` zone-map bounds for `blk` when valid and `NaN`-free.
fn block_top_k_bounds(blk: &BlockEntry, dt: &DataType) -> Option<(TopKKey, TopKKey)> {
    if blk.encoding == Encoding::Dict
        || blk.encoding == Encoding::ConstNull
        || blk.null_count >= blk.row_count
        || (blk.block_flags & BLOCK_FLAG_MIN_MAX_VALID) == 0
    {
        return None;
    }
    match dt {
        DataType::I64 | DataType::TimestampNs { .. } => {
            if blk.min_inline_len != 8 || blk.max_inline_len != 8 {
                return None;
            }
            let lo = i64::from_le_bytes(blk.min_inline[0..8].try_into().ok()?);
            let hi = i64::from_le_bytes(blk.max_inline[0..8].try_into().ok()?);
            (lo <= hi).then_some((TopKKey::I64(lo), TopKKey::I64(hi)))
        }
        DataType::Date => {
            if blk.min_inline_len != 4 || blk.max_inline_len != 4 {
                return None;
            }
            let lo = i32::from_le_bytes(blk.min_inline[0..4].try_into().ok()?);
            let hi = i32::from_le_bytes(blk.max_inline[0..4].try_into().ok()?);
            (lo <= hi).then_some((TopKKey::I64(i64::from(lo)), TopKKey::I64(i64::from(hi))))
        }
        DataType::F64 => {
            if (blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0
                || blk.min_inline_len != 8
                || blk.max_inline_len != 8
            {
                return None;
            }
            let lo_raw = f64::from_le_bytes(blk.min_inline[0..8].try_into().ok()?);
            let hi = f64::from_le_bytes(blk.max_inline[0..8].try_into().ok()?);
            if lo_raw.is_nan() || hi.is_nan() || lo_raw.total_cmp(&hi) == Ordering::Greater {
                return None;
            }
            // Zone maps canonicalize `-0.0` to `+0.0`, while `f64::total_cmp` orders
            // `-0.0 < +0.0`. Widen `+0.0` lower bound to `-0.0` so `ASC` never prunes `-0.0`.
            let lo = if lo_raw == 0.0 { -0.0 } else { lo_raw };
            Some((TopKKey::F64(lo), TopKKey::F64(hi)))
        }
        _ => None,
    }
}

/// Returns `true` if `blk` definitely cannot enter the full Top-K heap `heap`.
fn block_pruned_by_heap(
    blk: &BlockEntry,
    dt: &DataType,
    descending: bool,
    k: usize,
    heap: &BinaryHeap<TopKHeapEntry>,
) -> bool {
    if heap.len() < k {
        return false;
    }
    if blk.encoding == Encoding::ConstNull || blk.null_count >= blk.row_count {
        // Heap already holds `k` non-null winners; NULLs sort last.
        return true;
    }
    let Some(worst) = heap.peek() else {
        return false;
    };
    let Some((lo, hi)) = block_top_k_bounds(blk, dt) else {
        return false;
    };
    let best_possible = if descending { hi } else { lo };
    !worst.beaten_by(best_possible, u64::from(blk.first_row))
}

#[inline]
fn push_non_null_candidate(
    heap: &mut BinaryHeap<TopKHeapEntry>,
    key: TopKKey,
    global_row: u64,
    descending: bool,
    k: usize,
) {
    if heap.len() < k {
        heap.push(TopKHeapEntry {
            key,
            global_row,
            descending,
        });
    } else if let Some(worst) = heap.peek()
        && worst.beaten_by(key, global_row)
    {
        heap.pop();
        heap.push(TopKHeapEntry {
            key,
            global_row,
            descending,
        });
    }
}

#[inline]
fn push_null_candidate(null_rows: &mut BinaryHeap<u64>, global_row: u64, k: usize) {
    if null_rows.len() < k {
        null_rows.push(global_row);
    } else if let Some(&worst_row) = null_rows.peek()
        && global_row < worst_row
    {
        null_rows.pop();
        null_rows.push(global_row);
    }
}

fn push_column_top_k(
    col: &Column,
    mask: &Bitmap,
    mb_first_row: u64,
    descending: bool,
    k: usize,
    heap: &mut BinaryHeap<TopKHeapEntry>,
    null_rows: &mut BinaryHeap<u64>,
) -> Result<(), StatamError> {
    match col {
        Column::Const { value, .. } => {
            let key_opt = match value {
                ScalarValue::Null(_) => None,
                ScalarValue::I64(v) | ScalarValue::TimestampNs { value: v, .. } => {
                    Some(TopKKey::I64(*v))
                }
                ScalarValue::Date(v) => Some(TopKKey::I64(i64::from(*v))),
                ScalarValue::F64(v) => Some(TopKKey::F64(*v)),
                _ => {
                    return Err(StatamError::SchemaMismatch {
                        reason: "unsupported TopK order-by column data type",
                    });
                }
            };
            match key_opt {
                Some(key) => {
                    for r in mask.iter_ones() {
                        push_non_null_candidate(
                            heap,
                            key,
                            mb_first_row.saturating_add(r as u64),
                            descending,
                            k,
                        );
                    }
                }
                None => {
                    if heap.len() < k {
                        for r in mask.iter_ones() {
                            push_null_candidate(
                                null_rows,
                                mb_first_row.saturating_add(r as u64),
                                k,
                            );
                        }
                    }
                }
            }
            Ok(())
        }
        Column::Flat(fc) => {
            let nulls = fc.nulls();
            match fc.data() {
                FlatData::I64(vals) | FlatData::TimestampNs { values: vals, .. } => {
                    for r in mask.iter_ones() {
                        let grow = mb_first_row.saturating_add(r as u64);
                        if nulls.is_some_and(|bm| !bm.get(r)) {
                            if heap.len() < k {
                                push_null_candidate(null_rows, grow, k);
                            }
                        } else {
                            push_non_null_candidate(
                                heap,
                                TopKKey::I64(vals[r]),
                                grow,
                                descending,
                                k,
                            );
                        }
                    }
                }
                FlatData::Date(vals) => {
                    for r in mask.iter_ones() {
                        let grow = mb_first_row.saturating_add(r as u64);
                        if nulls.is_some_and(|bm| !bm.get(r)) {
                            if heap.len() < k {
                                push_null_candidate(null_rows, grow, k);
                            }
                        } else {
                            push_non_null_candidate(
                                heap,
                                TopKKey::I64(i64::from(vals[r])),
                                grow,
                                descending,
                                k,
                            );
                        }
                    }
                }
                FlatData::F64(vals) => {
                    for r in mask.iter_ones() {
                        let grow = mb_first_row.saturating_add(r as u64);
                        if nulls.is_some_and(|bm| !bm.get(r)) {
                            if heap.len() < k {
                                push_null_candidate(null_rows, grow, k);
                            }
                        } else {
                            push_non_null_candidate(
                                heap,
                                TopKKey::F64(vals[r]),
                                grow,
                                descending,
                                k,
                            );
                        }
                    }
                }
                _ => {
                    return Err(StatamError::SchemaMismatch {
                        reason: "unsupported TopK order-by column data type",
                    });
                }
            }
            Ok(())
        }
        Column::Dict(_) | Column::Rle(_) => {
            let flat = col.materialize(None)?;
            push_column_top_k(
                &Column::Flat(flat),
                mask,
                mb_first_row,
                descending,
                k,
                heap,
                null_rows,
            )
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileTopKBound {
    Bounded(TopKKey),
    Unbounded,
    AllNull,
}

impl StatamReader {
    /// Validates `spec` and all [`ScanRequest`] fields (`projection`, `id_lookups`, `predicates`,
    /// `filter_exprs`, `substrings`, `map_projections`) against this file's schema, returning
    /// `(order_col_data_type, projected_schema)`.
    fn validate_top_k_request(
        &self,
        req: &ScanRequest,
        spec: TopKSpec,
    ) -> Result<(DataType, Schema), StatamError> {
        self.validate_top_k_spec(spec)?;
        let schema = &self.meta.schema_meta.schema;
        let num_logical = schema.len();
        let proj_indices: Vec<usize> = match &req.projection {
            Some(v) => {
                for &idx in v {
                    if idx >= num_logical {
                        return Err(StatamError::SchemaMismatch {
                            reason: "projection column index out of bounds",
                        });
                    }
                }
                v.clone()
            }
            None => (0..num_logical).collect(),
        };
        let proj_schema = schema.project(&proj_indices)?;
        for lk in &req.id_lookups {
            let cidx = schema
                .index_of(&lk.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "id_lookup column not found in schema",
                })?;
            if !super::eval::is_supported_id_lookup_type(&schema.fields()[cidx].data_type)
                || self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            {
                return Err(StatamError::SchemaMismatch {
                    reason: "id_lookup on unsupported column data type or ExternalPayload column",
                });
            }
        }
        for pred in &req.predicates {
            let cidx = schema
                .index_of(&pred.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "predicate column not found in schema",
                })?;
            if pred.map_key.is_some() && schema.fields()[cidx].data_type != DataType::MapUtf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "map_key predicate on non-MapUtf8 column",
                });
            }
            if self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                && !matches!(
                    pred.pred,
                    crate::codec::ZonePred::IsNull | crate::codec::ZonePred::IsNotNull
                )
            {
                return Err(StatamError::SchemaMismatch {
                    reason: "value predicate not supported on ExternalPayload column",
                });
            }
        }
        for expr in &req.filter_exprs {
            let _ = self.resolve_filter_expr(expr)?;
        }
        for clause in &req.substrings {
            for sq in clause {
                let cidx = schema
                    .index_of(&sq.column)
                    .ok_or(StatamError::SchemaMismatch {
                        reason: "substring query column not found in schema",
                    })?;
                if schema.fields()[cidx].data_type != DataType::Utf8 {
                    return Err(StatamError::SchemaMismatch {
                        reason: "substring query on non-Utf8 column",
                    });
                }
            }
        }
        for col_name in req.map_projections.keys() {
            let cidx = schema
                .index_of(col_name)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "map_projections column not found in schema",
                })?;
            if schema.fields()[cidx].data_type != DataType::MapUtf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "map_projections specified for non-MapUtf8 column",
                });
            }
        }
        Ok((schema.fields()[spec.col_idx].data_type.clone(), proj_schema))
    }

    /// Executes a late-materialized Top-K scan across multiple [`StatamReader`] files, pruning
    /// entire files whose zone-map bounds on `top_k.col_idx` cannot enter the running global Top-K
    /// heap before issuing any data I/O on dominated files.
    ///
    /// When `req.top_k` is `None`, scans readers in order honoring `req.limit` across the set.
    pub fn scan_top_k_files(
        readers: &[&StatamReader],
        req: &ScanRequest,
    ) -> Result<Vec<Batch>, StatamError> {
        let Some(spec) = req.top_k else {
            let mut out = Vec::new();
            let mut remaining = req.limit;
            for r in readers {
                if remaining == Some(0) {
                    break;
                }
                let batches = if remaining == req.limit {
                    r.scan(req)?
                } else {
                    let mut sub_req = req.clone();
                    sub_req.limit = remaining;
                    r.scan(&sub_req)?
                };
                if let Some(rem) = remaining.as_mut() {
                    for b in &batches {
                        *rem = rem.saturating_sub(b.num_rows());
                    }
                }
                out.extend(batches);
            }
            return Ok(out);
        };
        if readers.is_empty() {
            return Ok(Vec::new());
        }
        let (expected_order_dt, expected_proj_schema) =
            readers[0].validate_top_k_request(req, spec)?;
        for r in &readers[1..] {
            let (order_dt, proj_schema) = r.validate_top_k_request(req, spec)?;
            if order_dt != expected_order_dt {
                return Err(StatamError::SchemaMismatch {
                    reason: "top_k order-by column data type mismatch across files",
                });
            }
            if proj_schema != expected_proj_schema {
                return Err(StatamError::SchemaMismatch {
                    reason: "projected schema mismatch across files",
                });
            }
        }
        let effective_k = effective_top_k_limit(req, spec);
        if effective_k == 0 || req.time_range.is_empty() {
            return Ok(Vec::new());
        }
        if readers.len() == 1 {
            return readers[0].scan(req);
        }

        // Compute each file's best order-by zone-map bound across metadata-surviving RGs/MBs.
        struct FileCandidate {
            file_idx: usize,
            best_bound: FileTopKBound,
        }
        let mut file_cands: Vec<FileCandidate> = Vec::with_capacity(readers.len());
        for (file_idx, r) in readers.iter().enumerate() {
            if r.meta.postscript.row_count == 0 {
                continue;
            }
            let Some(bound) = r.file_top_k_best_bound(req, spec)? else {
                // Entire file pruned by metadata (time_range / predicates / filter_exprs / rg_indices).
                continue;
            };
            file_cands.push(FileCandidate {
                file_idx,
                best_bound: bound,
            });
        }
        if file_cands.is_empty() {
            return Ok(Vec::new());
        }

        // Visit files with the strongest bounded zone maps first, then unbounded, then all-null.
        file_cands.sort_by(|a, b| match (a.best_bound, b.best_bound) {
            (FileTopKBound::Bounded(ka), FileTopKBound::Bounded(kb)) => {
                if spec.descending {
                    kb.cmp(&ka).then_with(|| a.file_idx.cmp(&b.file_idx))
                } else {
                    ka.cmp(&kb).then_with(|| a.file_idx.cmp(&b.file_idx))
                }
            }
            (FileTopKBound::Bounded(_), _) => Ordering::Less,
            (_, FileTopKBound::Bounded(_)) => Ordering::Greater,
            (FileTopKBound::Unbounded, FileTopKBound::AllNull) => Ordering::Less,
            (FileTopKBound::AllNull, FileTopKBound::Unbounded) => Ordering::Greater,
            (FileTopKBound::Unbounded, FileTopKBound::Unbounded)
            | (FileTopKBound::AllNull, FileTopKBound::AllNull) => a.file_idx.cmp(&b.file_idx),
        });

        // Track the `effective_k`-th best non-null key across files scanned so far.
        // Stored in a max-heap of worst keys (`Reverse` for DESC, normal for ASC) so `.peek()`
        // is always the cutoff threshold.
        let mut cutoff_heap: BinaryHeap<TopKHeapEntry> =
            BinaryHeap::with_capacity(effective_k.min(65_536).saturating_add(1));
        let mut per_file_results: Vec<(usize, Batch, Vec<Option<TopKKey>>)> = Vec::new();

        for fc in file_cands {
            if cutoff_heap.len() >= effective_k {
                match fc.best_bound {
                    FileTopKBound::AllNull => continue,
                    FileTopKBound::Bounded(bound) => {
                        if let Some(worst) = cutoff_heap.peek() {
                            let strictly_dominated = if spec.descending {
                                bound < worst.key
                            } else {
                                bound > worst.key
                            };
                            if strictly_dominated {
                                continue;
                            }
                        }
                    }
                    FileTopKBound::Unbounded => {}
                }
            }

            let r = readers[fc.file_idx];
            let (batches, keys) = r.scan_top_k_returning_keys(req, spec, effective_k)?;
            if let Some(batch) = batches.into_iter().next()
                && !batch.is_empty()
            {
                for (row_in_batch, key_opt) in keys.iter().enumerate() {
                    if let Some(key) = *key_opt {
                        push_non_null_candidate(
                            &mut cutoff_heap,
                            key,
                            ((fc.file_idx as u64) << 32) | (row_in_batch as u64),
                            spec.descending,
                            effective_k,
                        );
                    }
                }
                per_file_results.push((fc.file_idx, batch, keys));
            }
        }

        if per_file_results.is_empty() {
            return Ok(Vec::new());
        }
        if per_file_results.len() == 1 {
            return Ok(vec![per_file_results.remove(0).1]);
        }

        // Merge surviving files' top-K batches in exact global `(key, file_idx, row_in_batch)` order.
        let proj_schema = Arc::clone(per_file_results[0].1.schema());
        let batch_refs: Vec<&Batch> = per_file_results.iter().map(|(_, b, _)| b).collect();
        let combined = Batch::concat(Arc::clone(&proj_schema), &batch_refs, None)?;

        let mut merged: Vec<(Option<TopKKey>, usize, usize, u32)> = Vec::new();
        let mut base = 0u32;
        for (file_idx, batch, keys) in &per_file_results {
            for (local_i, &key_opt) in keys.iter().enumerate() {
                merged.push((key_opt, *file_idx, local_i, base + local_i as u32));
            }
            base = base.saturating_add(batch.num_rows() as u32);
        }
        merged.sort_unstable_by(|a, b| match (a.0, b.0) {
            (Some(ka), Some(kb)) => {
                let ord = if spec.descending {
                    kb.cmp(&ka)
                } else {
                    ka.cmp(&kb)
                };
                ord.then_with(|| a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2))
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)),
        });
        merged.truncate(effective_k);

        let take_indices: Vec<u32> = merged.into_iter().map(|(_, _, _, idx)| idx).collect();
        let out_batch = if combined.columns().is_empty() {
            Batch::try_new(proj_schema, Vec::new(), take_indices.len(), Selection::All)?
        } else {
            let mut out_cols = Vec::with_capacity(combined.columns().len());
            for col in combined.columns() {
                out_cols.push(Column::Flat(col.take(&take_indices, None)?));
            }
            Batch::try_new(proj_schema, out_cols, take_indices.len(), Selection::All)?
        };
        Ok(vec![out_batch])
    }

    /// Computes this file's best `(min|max)` zone-map bound on `spec.col_idx` across RGs/MBs that
    /// survive metadata-only pruning (`time_range`, `predicates`, `filter_exprs`, `rg_indices`).
    ///
    /// Returns:
    /// - `Ok(None)` if the entire file is pruned by metadata (`0` surviving MBs),
    /// - `Ok(Some(FileTopKBound::Bounded(key)))` if every surviving non-null MB has valid bounds,
    /// - `Ok(Some(FileTopKBound::AllNull))` if every surviving MB is all-null,
    /// - `Ok(Some(FileTopKBound::Unbounded))` if any surviving MB lacks usable bounds.
    fn file_top_k_best_bound(
        &self,
        req: &ScanRequest,
        spec: TopKSpec,
    ) -> Result<Option<FileTopKBound>, StatamError> {
        let schema = &self.meta.schema_meta.schema;
        let time_col_idx = if req.time_range.is_all() {
            None
        } else {
            super::plan::find_time_column(schema)
        };
        let mut resolved_preds = Vec::with_capacity(req.predicates.len());
        for pred in &req.predicates {
            let cidx = schema
                .index_of(&pred.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "predicate column not found in schema",
                })?;
            if super::eval::is_always_false_pred(&pred.pred) {
                return Ok(None);
            }
            resolved_preds.push((cidx, pred));
        }
        let mut resolved_filter_exprs = Vec::with_capacity(req.filter_exprs.len());
        for expr in &req.filter_exprs {
            resolved_filter_exprs.push(self.resolve_filter_expr(expr)?);
        }
        let mut rg_order = self.candidate_rg_range(
            time_col_idx,
            req.time_range,
            &resolved_preds,
            &resolved_filter_exprs,
        );
        if let Some(ref allowed_rgs) = req.rg_indices {
            let allowed: std::collections::HashSet<usize> = allowed_rgs.iter().copied().collect();
            rg_order.retain(|rg_idx| allowed.contains(rg_idx));
        }
        if rg_order.is_empty() {
            return Ok(None);
        }
        let order_dt = &schema.fields()[spec.col_idx].data_type;
        let empty_lookups: [(usize, &IdLookup); 0] = [];
        let empty_subs: [Vec<(usize, SubstringQuery)>; 0] = [];
        let empty_bitmaps: [Vec<Option<Bitmap>>; 0] = [];
        let mut any_mb = false;
        let mut all_bounded = true;
        let mut best_bound: Option<TopKKey> = None;

        for rg_idx in rg_order {
            let mb_count = self.meta.rg_blocks(0, rg_idx).len();
            if mb_count == 0 {
                continue;
            }
            let surviving_mbs = self.prune_rg_mini_blocks(
                rg_idx,
                mb_count,
                time_col_idx,
                req.time_range,
                &empty_lookups,
                &resolved_preds,
                &resolved_filter_exprs,
                &empty_subs,
                &empty_bitmaps,
            )?;
            if surviving_mbs.is_empty() {
                continue;
            }
            any_mb = true;
            let order_blks = self.meta.rg_blocks(spec.col_idx, rg_idx);
            for &mb_idx in &surviving_mbs {
                let blk = &order_blks[mb_idx];
                if blk.encoding == Encoding::ConstNull || blk.null_count >= blk.row_count {
                    continue;
                }
                if let Some((lo, hi)) = block_top_k_bounds(blk, order_dt) {
                    let b = if spec.descending { hi } else { lo };
                    best_bound = Some(match best_bound {
                        None => b,
                        Some(cur) => {
                            if spec.descending {
                                cur.max(b)
                            } else {
                                cur.min(b)
                            }
                        }
                    });
                } else {
                    all_bounded = false;
                }
            }
        }
        if !any_mb {
            return Ok(None);
        }
        Ok(Some(if !all_bounded {
            FileTopKBound::Unbounded
        } else if let Some(b) = best_bound {
            FileTopKBound::Bounded(b)
        } else {
            FileTopKBound::AllNull
        }))
    }

    fn scan_top_k_returning_keys(
        &self,
        req: &ScanRequest,
        spec: TopKSpec,
        effective_k: usize,
    ) -> Result<(Vec<Batch>, Vec<Option<TopKKey>>), StatamError> {
        let _exec_guard =
            crate::parallel::enter_executor_scope(req.executor.as_ref().or(self.executor.as_ref()));
        let schema = &self.meta.schema_meta.schema;
        let num_logical = schema.len();
        let proj_indices: Vec<usize> = match &req.projection {
            Some(v) => {
                for &idx in v {
                    if idx >= num_logical {
                        return Err(StatamError::SchemaMismatch {
                            reason: "projection column index out of bounds",
                        });
                    }
                }
                v.clone()
            }
            None => (0..num_logical).collect(),
        };
        let proj_schema = Arc::new(schema.project(&proj_indices)?);
        let time_col_idx = if req.time_range.is_all() {
            None
        } else {
            super::plan::find_time_column(schema)
        };
        let mut early_empty =
            req.time_range.is_empty() || self.meta.postscript.row_count == 0 || effective_k == 0;
        let mut resolved_lookups = Vec::with_capacity(req.id_lookups.len());
        for lk in &req.id_lookups {
            let cidx = schema
                .index_of(&lk.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "id_lookup column not found in schema",
                })?;
            if !super::eval::is_supported_id_lookup_type(&schema.fields()[cidx].data_type)
                || self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            {
                return Err(StatamError::SchemaMismatch {
                    reason: "id_lookup on unsupported column data type or ExternalPayload column",
                });
            }
            if lk.values.is_empty() {
                early_empty = true;
            }
            resolved_lookups.push((cidx, lk));
        }
        let mut resolved_preds = Vec::with_capacity(req.predicates.len());
        for pred in &req.predicates {
            let cidx = schema
                .index_of(&pred.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "predicate column not found in schema",
                })?;
            if pred.map_key.is_some() && schema.fields()[cidx].data_type != DataType::MapUtf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "map_key predicate on non-MapUtf8 column",
                });
            }
            if self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                && !matches!(
                    pred.pred,
                    crate::codec::ZonePred::IsNull | crate::codec::ZonePred::IsNotNull
                )
            {
                return Err(StatamError::SchemaMismatch {
                    reason: "value predicate not supported on ExternalPayload column",
                });
            }
            if super::eval::is_always_false_pred(&pred.pred) {
                early_empty = true;
            }
            resolved_preds.push((cidx, pred));
        }
        let mut resolved_filter_exprs = Vec::with_capacity(req.filter_exprs.len());
        for expr in &req.filter_exprs {
            resolved_filter_exprs.push(self.resolve_filter_expr(expr)?);
        }
        let mut resolved_subs: Vec<Vec<(usize, SubstringQuery)>> =
            Vec::with_capacity(req.substrings.len());
        for clause in &req.substrings {
            if clause.is_empty() {
                early_empty = true;
            }
            let mut rc = Vec::with_capacity(clause.len());
            for sq in clause {
                let cidx = schema
                    .index_of(&sq.column)
                    .ok_or(StatamError::SchemaMismatch {
                        reason: "substring query column not found in schema",
                    })?;
                if schema.fields()[cidx].data_type != DataType::Utf8 {
                    return Err(StatamError::SchemaMismatch {
                        reason: "substring query on non-Utf8 column",
                    });
                }
                rc.push((cidx, sq.clone()));
            }
            resolved_subs.push(rc);
        }
        let mut map_proj_by_col: HashMap<usize, MapReadMode> = HashMap::new();
        for (col_name, mode) in &req.map_projections {
            let cidx = schema
                .index_of(col_name)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "map_projections column not found in schema",
                })?;
            if schema.fields()[cidx].data_type != DataType::MapUtf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "map_projections specified for non-MapUtf8 column",
                });
            }
            match mode {
                MapReadMode::Full => {}
                MapReadMode::KeysOnly => {
                    map_proj_by_col.insert(cidx, MapReadMode::KeysOnly);
                }
                MapReadMode::Keys(keys) => {
                    let mut deduped = keys.clone();
                    deduped.sort_unstable();
                    deduped.dedup();
                    map_proj_by_col.insert(cidx, MapReadMode::Keys(deduped));
                }
            }
        }
        let map_proj_by_col = Arc::new(map_proj_by_col);
        if early_empty {
            return Ok((Vec::new(), Vec::new()));
        }

        let lookup_hashes: Vec<(usize, Vec<u64>)> = resolved_lookups
            .iter()
            .map(|&(cidx, lk)| {
                let hashes = lk
                    .values
                    .iter()
                    .map(|v| crate::simd::bloom::statam_bloom_hash(v))
                    .collect();
                (cidx, hashes)
            })
            .collect();
        let mut rg_order = self.candidate_rg_range(
            time_col_idx,
            req.time_range,
            &resolved_preds,
            &resolved_filter_exprs,
        );
        if let Some(ref allowed_rgs) = req.rg_indices {
            let allowed: std::collections::HashSet<usize> = allowed_rgs.iter().copied().collect();
            rg_order.retain(|rg_idx| allowed.contains(rg_idx));
        }
        if rg_order.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let bloom_cols: Vec<usize> = lookup_hashes.iter().map(|&(cidx, _)| cidx).collect();
        let mut sub_cols_with_trigram: Vec<usize> = resolved_subs
            .iter()
            .flat_map(|cl| cl.iter())
            .filter_map(|(cidx, _)| {
                ((self.meta.col_dir[*cidx].col_flags & crate::format::COL_FLAG_HAS_TRIGRAM_INDEX)
                    != 0)
                    .then_some(*cidx)
            })
            .collect();
        sub_cols_with_trigram.sort_unstable();
        sub_cols_with_trigram.dedup();

        let mut preload_dict_cols = Vec::new();
        if let Some(tc) = time_col_idx {
            preload_dict_cols.push(tc);
        }
        for &(cidx, _) in &resolved_lookups {
            preload_dict_cols.push(cidx);
        }
        for &(cidx, pred) in &resolved_preds {
            preload_dict_cols.push(cidx);
            if let Some(key) = &pred.map_key
                && let Some((sub_pidx, _)) = self.find_shredded_subcol(cidx, key)
            {
                preload_dict_cols.push(sub_pidx);
            }
        }
        for expr in &resolved_filter_exprs {
            self.collect_filter_expr_dict_cols(expr, &mut preload_dict_cols);
        }
        for clause in &resolved_subs {
            for (cidx, _) in clause {
                preload_dict_cols.push(*cidx);
            }
        }
        if !bloom_cols.is_empty() || !sub_cols_with_trigram.is_empty() {
            self.ensure_lazy_indexes_loaded(
                &bloom_cols,
                &sub_cols_with_trigram,
                &preload_dict_cols,
            )?;
            if !lookup_hashes.is_empty() && !self.blooms_may_match(&lookup_hashes) {
                return Ok((Vec::new(), Vec::new()));
            }
        } else if !preload_dict_cols.is_empty() {
            self.load_inline_lazy_indexes(&[], &[], &preload_dict_cols)?;
        }
        let mut dict_sub_bitmaps = self.build_dict_sub_bitmaps(&resolved_subs);

        let (batches, _row_ids, keys) = self.scan_top_k_with_limits_and_keys(
            spec,
            effective_k,
            &rg_order,
            &proj_indices,
            &proj_schema,
            &map_proj_by_col,
            time_col_idx,
            req.time_range,
            &resolved_lookups,
            &lookup_hashes,
            &resolved_preds,
            &resolved_filter_exprs,
            &resolved_subs,
            &mut dict_sub_bitmaps,
            req.read_threads,
            None,
            None,
        )?;
        Ok((batches, keys))
    }

    /// Executes a late-materialized Top-K scan, returning the sorted winning batch (at most one
    /// non-empty [`Batch`]) and its exact 0-based file-global row indices.
    ///
    /// Respects `cancel` cooperatively and maintains exact `Reservation` accounting.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn scan_top_k_with_limits(
        &self,
        spec: TopKSpec,
        effective_k: usize,
        rg_order: &[usize],
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_proj_by_col: &Arc<HashMap<usize, MapReadMode>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        lookup_hashes: &[(usize, Vec<u64>)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &mut Vec<Vec<Option<Bitmap>>>,
        read_threads: usize,
        cancel: Option<&CancelToken>,
        res: Option<&mut Reservation>,
    ) -> Result<(Vec<Batch>, Vec<u64>), StatamError> {
        let (batches, row_ids, _) = self.scan_top_k_with_limits_and_keys(
            spec,
            effective_k,
            rg_order,
            proj_indices,
            proj_schema,
            map_proj_by_col,
            time_col_idx,
            time_range,
            lookups,
            lookup_hashes,
            preds,
            filter_exprs,
            subs,
            dict_sub_bitmaps,
            read_threads,
            cancel,
            res,
        )?;
        Ok((batches, row_ids))
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_top_k_with_limits_and_keys(
        &self,
        spec: TopKSpec,
        effective_k: usize,
        rg_order: &[usize],
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_proj_by_col: &Arc<HashMap<usize, MapReadMode>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        lookup_hashes: &[(usize, Vec<u64>)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &mut Vec<Vec<Option<Bitmap>>>,
        read_threads: usize,
        cancel: Option<&CancelToken>,
        mut res: Option<&mut Reservation>,
    ) -> Result<TopKScanOutput, StatamError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(StatamError::Cancelled);
        }
        let max_file_rows = usize::try_from(self.meta.postscript.row_count).unwrap_or(usize::MAX);
        let effective_k = effective_k.min(max_file_rows);
        if effective_k == 0 || rg_order.is_empty() {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }

        let order_col = spec.col_idx;
        let order_dt = &self.meta.schema_meta.schema.fields()[order_col].data_type;
        let allow_pre_wave1_pack_open = preds.is_empty()
            && filter_exprs.is_empty()
            && time_col_idx.is_none()
            && lookups.is_empty();

        // 1. Metadata-prune RGs and mini-blocks, computing each RG's best & worst order-by bounds.
        struct CandidateRg {
            rg_idx: usize,
            surviving_mbs: Vec<usize>,
            best_bound: Option<TopKKey>,
            worst_bound: Option<TopKKey>,
        }

        let mut candidates: Vec<CandidateRg> = Vec::with_capacity(rg_order.len());
        for &rg_idx in rg_order {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(StatamError::Cancelled);
            }
            if !lookup_hashes.is_empty() && !self.rg_blooms_may_match(rg_idx, lookup_hashes) {
                continue;
            }
            let mb_count = self.meta.rg_blocks(0, rg_idx).len();
            if mb_count == 0 {
                continue;
            }
            let surviving_mbs = self.prune_rg_mini_blocks(
                rg_idx,
                mb_count,
                time_col_idx,
                time_range,
                lookups,
                preds,
                filter_exprs,
                subs,
                dict_sub_bitmaps,
            )?;
            if surviving_mbs.is_empty() {
                continue;
            }
            if self.rg_pruned_by_external_packs(
                rg_idx,
                &surviving_mbs,
                subs,
                allow_pre_wave1_pack_open,
            )? {
                continue;
            }

            let order_blks = self.meta.rg_blocks(order_col, rg_idx);
            let mut best_bound: Option<TopKKey> = None;
            let mut worst_bound: Option<TopKKey> = None;
            let mut all_bounded = true;
            for &mb_idx in &surviving_mbs {
                let blk = &order_blks[mb_idx];
                if blk.encoding == Encoding::ConstNull || blk.null_count >= blk.row_count {
                    continue;
                }
                if let Some((lo, hi)) = block_top_k_bounds(blk, order_dt) {
                    let (b, w) = if spec.descending { (hi, lo) } else { (lo, hi) };
                    best_bound = Some(match best_bound {
                        None => b,
                        Some(cur) => {
                            if spec.descending {
                                cur.max(b)
                            } else {
                                cur.min(b)
                            }
                        }
                    });
                    worst_bound = Some(match worst_bound {
                        None => w,
                        Some(cur) => {
                            if spec.descending {
                                cur.min(w)
                            } else {
                                cur.max(w)
                            }
                        }
                    });
                } else {
                    all_bounded = false;
                }
            }
            candidates.push(CandidateRg {
                rg_idx,
                surviving_mbs,
                best_bound: if all_bounded { best_bound } else { None },
                worst_bound: if all_bounded { worst_bound } else { None },
            });
        }

        if candidates.is_empty() {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }

        // Order RGs with the most promising zone-map bounds first so the cutoff tightens quickly.
        candidates.sort_by(|a, b| match (a.best_bound, b.best_bound) {
            (Some(ka), Some(kb)) => {
                if spec.descending {
                    kb.cmp(&ka).then_with(|| a.rg_idx.cmp(&b.rg_idx))
                } else {
                    ka.cmp(&kb).then_with(|| a.rg_idx.cmp(&b.rg_idx))
                }
            }
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.rg_idx.cmp(&b.rg_idx),
        });

        // 2. Phase 1 only: evaluate filters + order-by column, dynamically pruning MBs and RGs
        //    against the running Top-K heap cutoff.
        let mut heap: BinaryHeap<TopKHeapEntry> =
            BinaryHeap::with_capacity(effective_k.min(65_536).saturating_add(1));
        let mut null_rows: BinaryHeap<u64> = BinaryHeap::new();
        let empty_map_proj: Arc<HashMap<usize, MapReadMode>> = Arc::new(HashMap::new());
        let p1_proj = [order_col];
        let has_payload_sub = subs.iter().any(|cl| {
            cl.iter().any(|(cidx, _)| {
                self.meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload
            })
        });

        let prebuilt_plans: Vec<Arc<super::plan::RgIoPlan>> = candidates
            .iter()
            .map(|cand| {
                Arc::new(self.build_rg_io_plan(
                    cand.rg_idx,
                    &cand.surviving_mbs,
                    &p1_proj,
                    &empty_map_proj,
                    time_col_idx,
                    time_range,
                    lookups,
                    preds,
                    filter_exprs,
                    subs,
                ))
            })
            .collect();

        // Unconditionally coalesce Phase-1 blocks into ONE `read_ranges` call across RGs whose
        // order-by bounds overlap `candidates[0]` (cannot be pruned by `candidates[0]` alone),
        // bounded by `IO_WAVE_MAX_BYTES` on remote readers or `TOP_K_LOCAL_OVERLAP_COALESCE_MAX_BYTES`
        // (`8 MiB`) on local readers, including uncached non-inline lazy dict bytes.
        const TOP_K_LOCAL_OVERLAP_COALESCE_MAX_BYTES: u64 = 8 * 1024 * 1024;
        let (shared_remote_p1_cache, in_shared_p1) = if candidates.len() >= 2 {
            let anchor_worst = candidates[0].worst_bound;
            let mut in_shared = vec![false; candidates.len()];
            let mut coalesced_count = 0usize;
            for (ci, cand) in candidates.iter().enumerate() {
                let overlaps_anchor = ci == 0
                    || match (anchor_worst, cand.best_bound) {
                        (None, _) | (_, None) => true,
                        (Some(aw), Some(cb)) => {
                            if spec.descending {
                                cb >= aw
                            } else {
                                cb <= aw
                            }
                        }
                    };
                if overlaps_anchor {
                    in_shared[ci] = true;
                    coalesced_count += 1;
                }
            }

            if coalesced_count >= 2 {
                let mut seen_ranges: std::collections::HashSet<(u64, u64)> =
                    std::collections::HashSet::new();
                let mut all_ranges: Vec<(u64, u64)> = Vec::new();
                let mut all_dicts: Vec<usize> = Vec::new();
                let mut total_bytes: u64 = 0;
                for (ci, (cand, plan)) in candidates.iter().zip(prebuilt_plans.iter()).enumerate() {
                    if !in_shared[ci] {
                        continue;
                    }
                    let rg = &self.meta.rg_dir[cand.rg_idx];
                    for &pidx in plan.all_phys_cols() {
                        let blks = self.meta.rg_blocks(pidx, cand.rg_idx);
                        if (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                            && cand
                                .surviving_mbs
                                .iter()
                                .any(|&mb| blks[mb].encoding == Encoding::Dict)
                        {
                            all_dicts.push(pidx);
                        }
                        for &mb_idx in &cand.surviving_mbs {
                            let blk = &blks[mb_idx];
                            if blk.stored_len > 0 {
                                let total = (BLOCK_FRAME_HEADER_LEN as u64)
                                    .checked_add(u64::from(blk.stored_len))
                                    .ok_or(StatamError::BoundsExceeded {
                                        reason: "top_k phase1 block length overflow",
                                    })?;
                                let end = blk.offset.checked_add(total).ok_or(
                                    StatamError::BoundsExceeded {
                                        reason: "top_k phase1 block end overflow",
                                    },
                                )?;
                                if seen_ranges.insert((blk.offset, end)) {
                                    all_ranges.push((blk.offset, end));
                                    total_bytes = total_bytes.saturating_add(total);
                                }
                            }
                        }
                    }
                    if rg.l0_payload_len > 0 && has_payload_sub {
                        let l0_end = rg
                            .l0_payload_offset
                            .checked_add(u64::from(rg.l0_payload_len))
                            .ok_or(StatamError::BoundsExceeded {
                                reason: "top_k phase1 L0 end overflow",
                            })?;
                        if seen_ranges.insert((rg.l0_payload_offset, l0_end)) {
                            all_ranges.push((rg.l0_payload_offset, l0_end));
                            total_bytes = total_bytes.saturating_add(u64::from(rg.l0_payload_len));
                        }
                    }
                }

                let max_coalesce_bytes = if self.reader.is_remote() {
                    IO_WAVE_MAX_BYTES
                } else {
                    TOP_K_LOCAL_OVERLAP_COALESCE_MAX_BYTES
                };
                let dict_bytes = self.uncached_non_inline_lazy_dict_bytes(&all_dicts);
                let p1_total_bytes = total_bytes.saturating_add(dict_bytes);
                if p1_total_bytes <= max_coalesce_bytes {
                    if cancel.is_some_and(CancelToken::is_cancelled) {
                        return Err(StatamError::Cancelled);
                    }
                    let cache = self.fetch_blocks_and_lazy_maybe_prefetch_packs(
                        &all_ranges,
                        &[],
                        &[],
                        &all_dicts,
                        false,
                    )?;
                    self.refresh_dict_sub_bitmaps(subs, dict_sub_bitmaps);
                    (Some(Arc::new(cache)), in_shared)
                } else {
                    (None, Vec::new())
                }
            } else {
                (None, Vec::new())
            }
        } else {
            (None, Vec::new())
        };

        for (ci, (mut cand, plan)) in candidates.into_iter().zip(prebuilt_plans).enumerate() {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(StatamError::Cancelled);
            }
            let order_blks = self.meta.rg_blocks(order_col, cand.rg_idx);
            cand.surviving_mbs.retain(|&mb_idx| {
                !block_pruned_by_heap(
                    &order_blks[mb_idx],
                    order_dt,
                    spec.descending,
                    effective_k,
                    &heap,
                )
            });
            if cand.surviving_mbs.is_empty() {
                continue;
            }

            let rg = &self.meta.rg_dir[cand.rg_idx];
            let p1_l0_needed = rg.l0_payload_len > 0 && has_payload_sub;
            let p1_l0_end = rg
                .l0_payload_offset
                .checked_add(u64::from(rg.l0_payload_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "top_k phase1 L0 end overflow",
                })?;

            let p1_cache = if in_shared_p1.get(ci).copied().unwrap_or(false)
                && let Some(ref shared) = shared_remote_p1_cache
            {
                Arc::clone(shared)
            } else {
                // Fetch ONLY Phase-1 filter columns + `order_col` (never wide Phase-2 columns).
                let mut p1_ranges: Vec<(u64, u64)> = Vec::new();
                let mut dict_cols: Vec<usize> = Vec::new();
                for &pidx in plan.all_phys_cols() {
                    let blks = self.meta.rg_blocks(pidx, cand.rg_idx);
                    if (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                        && cand
                            .surviving_mbs
                            .iter()
                            .any(|&mb| blks[mb].encoding == Encoding::Dict)
                    {
                        dict_cols.push(pidx);
                    }
                    for &mb_idx in &cand.surviving_mbs {
                        let blk = &blks[mb_idx];
                        if blk.stored_len > 0 {
                            let total = (BLOCK_FRAME_HEADER_LEN as u64)
                                .checked_add(u64::from(blk.stored_len))
                                .ok_or(StatamError::BoundsExceeded {
                                    reason: "top_k phase1 block length overflow",
                                })?;
                            let end = blk.offset.checked_add(total).ok_or(
                                StatamError::BoundsExceeded {
                                    reason: "top_k phase1 block end overflow",
                                },
                            )?;
                            p1_ranges.push((blk.offset, end));
                        }
                    }
                }
                if p1_l0_needed {
                    p1_ranges.push((rg.l0_payload_offset, p1_l0_end));
                }

                let cache = Arc::new(self.fetch_blocks_and_lazy_maybe_prefetch_packs(
                    &p1_ranges,
                    &[],
                    &[],
                    &dict_cols,
                    false,
                )?);
                self.refresh_dict_sub_bitmaps(subs, dict_sub_bitmaps);
                cache
            };

            let l0_parsed = if p1_l0_needed {
                p1_cache
                    .get(&(rg.l0_payload_offset, p1_l0_end))
                    .map(|b| ParsedL0Payload::parse(b.clone()))
                    .transpose()?
            } else {
                None
            };
            let block_reader = BlockCacheReader::Map {
                primary: Arc::clone(&p1_cache),
                fallback: None,
                missing_reason: "missing top_k phase1 block",
                overflow_reason: "top_k phase1 block key overflow",
            };
            let retained = RetainedPackChunks::default();
            let active_mbs = self.eval_phase1_mbs(
                cand.rg_idx,
                &cand.surviving_mbs,
                &plan,
                &p1_proj,
                time_col_idx,
                time_range,
                lookups,
                preds,
                filter_exprs,
                subs,
                dict_sub_bitmaps,
                l0_parsed.as_ref(),
                &block_reader,
                None,
                false,
                read_threads,
                &retained,
                cancel,
            )?;
            let _ = retained;

            let order_dict = self.get_cached_dict(order_col);
            for (mb_idx, mask, mut decoded_p1) in active_mbs {
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(StatamError::Cancelled);
                }
                let blk = &order_blks[mb_idx];
                if block_pruned_by_heap(blk, order_dt, spec.descending, effective_k, &heap) {
                    continue;
                }
                let mb_first_row = u64::from(blk.first_row);
                let col = match decoded_p1.remove(&order_col) {
                    Some(DecodedPhase1::Col(c)) => c,
                    _ => {
                        let raw = if blk.stored_len > 0 {
                            block_reader.read_block(blk)?
                        } else {
                            std::borrow::Cow::Borrowed(&[][..])
                        };
                        decode_mini_block_selected(
                            &raw,
                            blk,
                            order_dt,
                            order_dict.as_deref(),
                            &mask,
                        )?
                    }
                };
                push_column_top_k(
                    &col,
                    &mask,
                    mb_first_row,
                    spec.descending,
                    effective_k,
                    &mut heap,
                    &mut null_rows,
                )?;
            }
        }
        drop(shared_remote_p1_cache);

        // 3. Finalize winning global row IDs in exact ORDER BY order (`NULLS LAST`).
        let mut non_null_winners: Vec<TopKHeapEntry> = heap.into_vec();
        // `TopKHeapEntry::Ord` orders better items as `Less`, so ascending sort puts best first.
        non_null_winners.sort_unstable();
        let initial_cap = non_null_winners
            .len()
            .saturating_add(null_rows.len())
            .min(effective_k);
        let mut ordered_rows: Vec<u64> = Vec::with_capacity(initial_cap);
        let mut ordered_keys: Vec<Option<TopKKey>> = Vec::with_capacity(initial_cap);
        for e in non_null_winners {
            ordered_rows.push(e.global_row);
            ordered_keys.push(Some(e.key));
        }
        if ordered_rows.len() < effective_k && !null_rows.is_empty() {
            let mut nulls_sorted = null_rows.into_vec();
            nulls_sorted.sort_unstable();
            let take_nulls = effective_k - ordered_rows.len();
            for grow in nulls_sorted.into_iter().take(take_nulls) {
                ordered_rows.push(grow);
                ordered_keys.push(None);
            }
        }

        if ordered_rows.is_empty() {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }

        // 4. Phase 2: Materialize ONLY the winning `<= effective_k` rows via `take_rows_inner`
        //    (preserving the caller's active `ScanRequest::executor`), then reorder into exact
        //    `ordered_rows` order while enforcing exact `Reservation` tracking & error rollback.
        let mut sorted_rows = ordered_rows.clone();
        sorted_rows.sort_unstable();

        let res_before = res.as_ref().map(|r| r.size());
        let phase2 = (|| -> Result<Vec<Batch>, StatamError> {
            let taken = self.take_rows_inner(
                &sorted_rows,
                Some(proj_indices),
                map_proj_by_col,
                read_threads,
                cancel,
                res.as_deref_mut(),
                res_before,
            )?;

            if taken.is_empty() {
                return Ok(Vec::new());
            }

            let already_in_order = ordered_rows == sorted_rows;
            if already_in_order && taken.len() == 1 {
                return Ok(taken);
            }

            let combined = if taken.len() == 1 {
                taken.into_iter().next().ok_or(StatamError::Corrupt {
                    reason: "missing single top_k batch",
                })?
            } else {
                let refs: Vec<&Batch> = taken.iter().collect();
                let concat_batch =
                    Batch::concat(Arc::clone(proj_schema), &refs, res.as_deref_mut())?;
                drop(taken);
                if let (Some(r), Some(before)) = (res.as_deref_mut(), res_before) {
                    let cur = r.size();
                    let target = before.saturating_add(concat_batch.memory_bytes());
                    if cur > target {
                        r.shrink(cur - target);
                    }
                }
                concat_batch
            };

            let out_batch = if already_in_order || proj_indices.is_empty() {
                combined
            } else {
                let mut reorder_idx = Vec::with_capacity(ordered_rows.len());
                for &grow in &ordered_rows {
                    let pos =
                        sorted_rows
                            .binary_search(&grow)
                            .map_err(|_| StatamError::Corrupt {
                                reason: "top_k winner row missing from taken batch",
                            })?;
                    let pos_u32 = u32::try_from(pos).map_err(|_| StatamError::BoundsExceeded {
                        reason: "top_k reorder index exceeds u32::MAX",
                    })?;
                    reorder_idx.push(pos_u32);
                }
                let mut out_cols = Vec::with_capacity(combined.columns().len());
                for col in combined.columns() {
                    let taken_col = Column::Flat(col.take(&reorder_idx, None)?);
                    if let Some(r) = res.as_deref_mut() {
                        let col_bytes = taken_col.memory_bytes();
                        if col_bytes > 0 {
                            r.try_grow(col_bytes)
                                .map_err(|_| StatamError::MemoryExceeded)?;
                        }
                    }
                    out_cols.push(taken_col);
                }
                drop(combined);
                Batch::try_new(
                    Arc::clone(proj_schema),
                    out_cols,
                    reorder_idx.len(),
                    Selection::All,
                )?
            };

            // Reconcile `Reservation` to exact final output bytes after releasing intermediates.
            if let (Some(r), Some(before)) = (res.as_deref_mut(), res_before) {
                let cur = r.size();
                if cur > before {
                    r.shrink(cur - before);
                }
                let final_bytes = out_batch.memory_bytes();
                if final_bytes > 0 {
                    r.try_grow(final_bytes)
                        .map_err(|_| StatamError::MemoryExceeded)?;
                }
            }

            Ok(vec![out_batch])
        })();

        match phase2 {
            Ok(batches) => Ok((batches, ordered_rows, ordered_keys)),
            Err(e) => {
                if let (Some(r), Some(before)) = (res, res_before)
                    && r.size() > before
                {
                    r.shrink(r.size() - before);
                }
                Err(e)
            }
        }
    }

    /// Validates a [`TopKSpec`] against this file's schema.
    pub(super) fn validate_top_k_spec(&self, spec: TopKSpec) -> Result<(), StatamError> {
        let schema = &self.meta.schema_meta.schema;
        let Some(field) = schema.fields().get(spec.col_idx) else {
            return Err(StatamError::SchemaMismatch {
                reason: "top_k column index out of bounds",
            });
        };
        if self.meta.col_dir[spec.col_idx].locality_group == LocalityGroup::ExternalPayload
            || !is_supported_top_k_type(&field.data_type)
        {
            return Err(StatamError::SchemaMismatch {
                reason: "top_k on unsupported column data type or ExternalPayload column",
            });
        }
        Ok(())
    }
}

/// Helper used by `StatamReader::scan` / `StatamScanIter` when a `ScanRequest` has `top_k`.
pub(super) fn effective_top_k_limit(req: &ScanRequest, spec: TopKSpec) -> usize {
    req.limit.map_or(spec.limit, |lim| lim.min(spec.limit))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use cetra_column::{
        Batch, Bitmap, CancelToken, Column, DataType, Field, FlatColumn, FlatData, GlobalPool,
        ScalarValue, Schema, Selection, Utf8Data,
    };

    use crate::StatamError;
    use crate::codec::{WriteProfile, ZonePred, ZoneValue};
    use crate::format::{Codec, TableKind};
    use crate::io::SliceReader;
    use crate::payload::pack::{ChunkPackBuilder, PackWriteOptions};
    use crate::reader::{InMemoryPackResolver, Predicate, ScanRequest, StatamReader, TopKSpec};
    use crate::writer::{StatamWriter, WriteOptions};

    type TestRow = (u64, Option<i64>, Option<f64>, String, String);

    fn make_test_file() -> (Arc<StatamReader>, Vec<TestRow>) {
        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, true),
            Field::new("score", DataType::F64, true),
            Field::new("kind", DataType::Utf8, false),
            Field::new("payload", DataType::Utf8, false),
        ])
        .unwrap();

        let pack_id = [0x42u8; 16];
        let opts = WriteOptions {
            profile: WriteProfile::L1Compact,
            table_kind: TableKind::Generic,
            row_group_size: 64,
            mini_block_size: 32,
            payload_cols: vec!["payload".to_owned()],
            pack_id: Some(pack_id),
            ..WriteOptions::default()
        };

        let pack_opts = PackWriteOptions {
            #[cfg(miri)]
            codec: Codec::Lz4,
            #[cfg(not(miri))]
            codec: Codec::Zstd,
            zstd_level: 3,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            write_threads: 1,
            ..PackWriteOptions::default()
        };

        let mut expected_rows = Vec::new();
        let mut statam_bytes = Vec::new();
        let mut pack_bytes = Vec::new();
        {
            let mut pb = ChunkPackBuilder::new(pack_id, pack_opts);
            let mut w = StatamWriter::new(
                Cursor::new(&mut statam_bytes),
                Arc::clone(&schema),
                opts,
                Some(&mut pb),
            )
            .unwrap();

            // 192 rows across 3 RGs (64 rows each), with deliberate RG ranges:
            // RG0: ts around 1_000..1_150, RG1: ts around 5_000..5_150, RG2: ts around 2_000..2_150
            let bases = [1_000i64, 5_000i64, 2_000i64];
            let mut grow = 0u64;
            for (rg_i, &base) in bases.iter().enumerate() {
                let mut ts_vals = Vec::with_capacity(64);
                let mut ts_valid = Vec::with_capacity(64);
                let mut sc_vals = Vec::with_capacity(64);
                let mut sc_valid = Vec::with_capacity(64);
                let mut kinds = Vec::with_capacity(64);
                let mut payloads = Vec::with_capacity(64);

                for i in 0..64usize {
                    let is_null_ts = i % 19 == 0;
                    // Create ties every 4 rows
                    let ts_v = base + ((i / 4) as i64) * 10;
                    ts_vals.push(if is_null_ts { 0 } else { ts_v });
                    ts_valid.push(!is_null_ts);

                    let (sc_opt, sc_v) = if i == 7 && rg_i == 2 {
                        (Some(f64::NAN), f64::NAN)
                    } else if i % 17 == 0 {
                        (None, 0.0)
                    } else {
                        let v = (rg_i as f64) * 100.0 + (i as f64) * 0.5;
                        (Some(v), v)
                    };
                    sc_vals.push(sc_v);
                    sc_valid.push(sc_opt.is_some());

                    let kind = if i % 2 == 0 { "llm" } else { "tool" }.to_owned();
                    let payload = format!("payload-rg{rg_i}-row{grow:04}-{}", "z".repeat(400));
                    expected_rows.push((
                        grow,
                        (!is_null_ts).then_some(ts_v),
                        sc_opt,
                        kind.clone(),
                        payload.clone(),
                    ));
                    kinds.push(kind);
                    payloads.push(payload);
                    grow += 1;
                }

                let kind_refs: Vec<&str> = kinds.iter().map(String::as_str).collect();
                let pay_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
                let batch = Batch::try_new(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::TimestampNs {
                                    values: ts_vals,
                                    utc: true,
                                },
                                Some(Bitmap::from_bools(&ts_valid)),
                            )
                            .unwrap(),
                        ),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::F64(sc_vals),
                                Some(Bitmap::from_bools(&sc_valid)),
                            )
                            .unwrap(),
                        ),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(Utf8Data::try_from_slices(kind_refs).unwrap()),
                                None,
                            )
                            .unwrap(),
                        ),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(Utf8Data::try_from_slices(pay_refs).unwrap()),
                                None,
                            )
                            .unwrap(),
                        ),
                    ],
                    64,
                    Selection::All,
                )
                .unwrap();
                w.write_batch(&batch).unwrap();
            }
            w.finish().unwrap();
            pb.finish(&mut pack_bytes).unwrap();
        }

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert_bytes(pack_id, pack_bytes).unwrap();
        let reader = Arc::new(
            StatamReader::open(
                Arc::new(SliceReader::new(statam_bytes)),
                Some(Arc::new(resolver)),
            )
            .unwrap(),
        );
        (reader, expected_rows)
    }

    #[test]
    fn top_k_desc_asc_nulls_ties_nan_and_stream_parity() {
        let (reader, rows) = make_test_file();

        // 1. DESC top-10 with predicate `kind == "llm"` and ExternalPayload projection.
        let req_desc = ScanRequest {
            projection: Some(vec![0, 3]),
            predicates: vec![Predicate::new(
                "kind",
                ZonePred::Eq(ZoneValue::Bytes(b"llm".to_vec())),
            )],
            top_k: Some(TopKSpec::desc(0, 10)),
            read_threads: 2,
            ..ScanRequest::default()
        };
        let mut expected_desc: Vec<_> = rows.iter().filter(|r| r.3 == "llm").collect();
        expected_desc.sort_by(|a, b| match (a.1, b.1) {
            (Some(ka), Some(kb)) => kb.cmp(&ka).then_with(|| a.0.cmp(&b.0)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.0.cmp(&b.0),
        });
        expected_desc.truncate(10);

        let batches = reader.scan(&req_desc).unwrap();
        assert_eq!(batches.len(), 1);
        let b = &batches[0];
        assert_eq!(b.num_rows(), 10);
        for (i, exp) in expected_desc.iter().enumerate() {
            let ts = b.column(0).unwrap().get_scalar(i).unwrap();
            let pay = b.column(1).unwrap().get_scalar(i).unwrap();
            assert_eq!(
                ts,
                match exp.1 {
                    Some(v) => ScalarValue::TimestampNs {
                        value: v,
                        utc: true
                    },
                    None => ScalarValue::Null(DataType::TimestampNs { utc: true }),
                }
            );
            assert_eq!(pay, ScalarValue::Utf8(exp.4.clone()));
        }

        // 2. ASC with large limit so NULLs appear strictly at the tail (`NULLS LAST`).
        let req_asc = ScanRequest {
            projection: Some(vec![0]),
            top_k: Some(TopKSpec::asc(0, 200)),
            ..ScanRequest::default()
        };
        let mut expected_asc: Vec<_> = rows.iter().collect();
        expected_asc.sort_by(|a, b| match (a.1, b.1) {
            (Some(ka), Some(kb)) => ka.cmp(&kb).then_with(|| a.0.cmp(&b.0)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.0.cmp(&b.0),
        });
        let asc_batches = reader.scan(&req_asc).unwrap();
        assert_eq!(asc_batches.len(), 1);
        assert_eq!(asc_batches[0].num_rows(), rows.len());
        for (i, exp) in expected_asc.iter().enumerate() {
            let ts = asc_batches[0].column(0).unwrap().get_scalar(i).unwrap();
            assert_eq!(
                ts,
                match exp.1 {
                    Some(v) => ScalarValue::TimestampNs {
                        value: v,
                        utc: true
                    },
                    None => ScalarValue::Null(DataType::TimestampNs { utc: true }),
                }
            );
        }

        // 3. F64 DESC must NOT prune the RG containing `NaN` (`total_cmp`: +NaN > finite).
        let req_f64 = ScanRequest {
            projection: Some(vec![1]),
            top_k: Some(TopKSpec::desc(1, 5)),
            ..ScanRequest::default()
        };
        let f64_batches = reader.scan(&req_f64).unwrap();
        assert_eq!(f64_batches.len(), 1);
        assert_eq!(f64_batches[0].num_rows(), 5);
        match f64_batches[0].column(0).unwrap().get_scalar(0).unwrap() {
            ScalarValue::F64(v) => {
                assert!(v.is_nan(), "expected NaN first under F64 total_cmp DESC");
            }
            other => panic!("unexpected scalar: {other:?}"),
        }

        // 4. `scan_iter` + `next_batch_with_row_ids` + `Reservation` + `CancelToken`.
        let pool = GlobalPool::standalone(64 * 1024 * 1024);
        let mut res = pool.register("top_k", false);
        let mut iter = reader.scan_iter(req_desc.clone()).unwrap();
        let (sb, sids) = iter
            .next_batch_with_row_ids(None, Some(&mut res))
            .unwrap()
            .unwrap();
        assert_eq!(res.size(), sb.memory_bytes());
        let exp_ids: Vec<u64> = expected_desc.iter().map(|r| r.0).collect();
        assert_eq!(sids, exp_ids);
        assert!(
            iter.next_batch_with_row_ids(None, Some(&mut res))
                .unwrap()
                .is_none()
        );

        let cancel = CancelToken::new();
        cancel.cancel();
        let mut iter_c = reader.scan_iter(req_desc).unwrap();
        assert!(matches!(
            iter_c.next_batch(Some(&cancel), None),
            Err(StatamError::Cancelled)
        ));

        // 5. Validation & limit=0.
        let req_zero = ScanRequest {
            top_k: Some(TopKSpec::desc(0, 0)),
            ..ScanRequest::default()
        };
        assert!(reader.scan(&req_zero).unwrap().is_empty());

        let req_bad_type = ScanRequest {
            top_k: Some(TopKSpec::desc(2, 5)),
            ..ScanRequest::default()
        };
        assert!(reader.scan(&req_bad_type).is_err());

        let req_oob = ScanRequest {
            top_k: Some(TopKSpec::desc(99, 5)),
            ..ScanRequest::default()
        };
        assert!(reader.scan(&req_oob).is_err());

        // 6. Huge limit (`usize::MAX`) must not overflow or OOM.
        let req_huge = ScanRequest {
            projection: Some(vec![0]),
            top_k: Some(TopKSpec::desc(0, usize::MAX)),
            ..ScanRequest::default()
        };
        let huge_batches = reader.scan(&req_huge).unwrap();
        assert_eq!(huge_batches.len(), 1);
        assert_eq!(huge_batches[0].num_rows(), rows.len());
    }

    #[test]
    fn top_k_f64_neg_zero_asc_and_shredded_map_projections() {
        use cetra_column::MapData;

        let schema = Schema::try_new_arc(vec![
            Field::new("score", DataType::F64, false),
            Field::new("metadata", DataType::MapUtf8, false),
        ])
        .unwrap();

        let opts = WriteOptions {
            profile: WriteProfile::L1Compact,
            table_kind: TableKind::Generic,
            row_group_size: 32,
            mini_block_size: 32,
            ..WriteOptions::default()
        };

        let mut statam_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(
                Cursor::new(&mut statam_bytes),
                Arc::clone(&schema),
                opts,
                None,
            )
            .unwrap();

            // RG0: all `+0.0`, RG1: contains `-0.0` at row 35 (must NOT be pruned under ASC!).
            for rg_i in 0..2usize {
                let mut scores = Vec::with_capacity(32);
                let mut offsets = Vec::with_capacity(33);
                let mut keys: Vec<&str> = Vec::new();
                let mut vals: Vec<String> = Vec::new();
                offsets.push(0u32);
                for i in 0..32usize {
                    let s = if rg_i == 1 && i == 3 { -0.0f64 } else { 0.0f64 };
                    scores.push(s);
                    keys.push("tenant");
                    vals.push(format!("t-{rg_i}-{i}"));
                    keys.push("env");
                    vals.push("prod".to_owned());
                    offsets.push(keys.len() as u32);
                }
                let val_refs: Vec<&str> = vals.iter().map(String::as_str).collect();
                let map_data = MapData::try_new(
                    offsets,
                    Utf8Data::try_from_slices(keys).unwrap(),
                    Utf8Data::try_from_slices(val_refs).unwrap(),
                    None,
                )
                .unwrap();
                let batch = Batch::try_new(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(FlatColumn::try_new(FlatData::F64(scores), None).unwrap()),
                        Column::Flat(
                            FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), None)
                                .unwrap(),
                        ),
                    ],
                    32,
                    Selection::All,
                )
                .unwrap();
                w.write_batch(&batch).unwrap();
            }
            w.finish().unwrap();
        }

        let reader =
            Arc::new(StatamReader::open(Arc::new(SliceReader::new(statam_bytes)), None).unwrap());

        // 1. ASC top-2 on F64 must find `-0.0` from RG1 first despite RG0 having `+0.0`.
        let mut map_keys = std::collections::HashMap::new();
        map_keys.insert(
            "metadata".to_owned(),
            crate::reader::MapReadMode::Keys(vec!["tenant".to_owned()]),
        );
        let req_keys = ScanRequest {
            projection: Some(vec![0, 1]),
            map_projections: map_keys,
            top_k: Some(TopKSpec::asc(0, 2)),
            ..ScanRequest::default()
        };
        let b_keys = reader.scan(&req_keys).unwrap();
        assert_eq!(b_keys.len(), 1);
        assert_eq!(b_keys[0].num_rows(), 2);
        match b_keys[0].column(0).unwrap().get_scalar(0).unwrap() {
            ScalarValue::F64(v) => assert!(v == 0.0 && v.is_sign_negative(), "expected -0.0 first"),
            other => panic!("unexpected scalar: {other:?}"),
        }
        match b_keys[0].column(1).unwrap().get_scalar(0).unwrap() {
            ScalarValue::MapUtf8(pairs) => {
                assert_eq!(pairs, vec![("tenant".to_owned(), Some("t-1-3".to_owned()))]);
            }
            other => panic!("unexpected map scalar: {other:?}"),
        }

        // 2. KeysOnly on shredded MapUtf8 must succeed without missing-block error.
        let mut map_ko = std::collections::HashMap::new();
        map_ko.insert("metadata".to_owned(), crate::reader::MapReadMode::KeysOnly);
        let req_ko = ScanRequest {
            projection: Some(vec![0, 1]),
            map_projections: map_ko,
            top_k: Some(TopKSpec::asc(0, 2)),
            ..ScanRequest::default()
        };
        let b_ko = reader.scan(&req_ko).unwrap();
        assert_eq!(b_ko.len(), 1);
        assert_eq!(b_ko[0].num_rows(), 2);
        match b_ko[0].column(1).unwrap().get_scalar(0).unwrap() {
            ScalarValue::MapUtf8(pairs) => {
                let mut got_keys: Vec<String> = pairs.into_iter().map(|(k, _)| k).collect();
                got_keys.sort();
                assert_eq!(got_keys, vec!["env".to_owned(), "tenant".to_owned()]);
            }
            other => panic!("unexpected keys_only map scalar: {other:?}"),
        }
    }

    #[test]
    fn scan_top_k_files_prunes_dominated_files_and_merges_ties() {
        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, true),
            Field::new("tag", DataType::Utf8, false),
        ])
        .unwrap();

        let make_file = |ts_vals: Vec<Option<i64>>, prefix: &str| -> StatamReader {
            let mut raw_ts = Vec::with_capacity(ts_vals.len());
            let mut valid = Vec::with_capacity(ts_vals.len());
            let mut tags = Vec::with_capacity(ts_vals.len());
            for (i, v) in ts_vals.into_iter().enumerate() {
                raw_ts.push(v.unwrap_or(0));
                valid.push(v.is_some());
                tags.push(format!("{prefix}-{i}"));
            }
            let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
            let batch = Batch::try_new(
                Arc::clone(&schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::TimestampNs {
                                values: raw_ts,
                                utc: true,
                            },
                            Some(Bitmap::from_bools(&valid)),
                        )
                        .unwrap(),
                    ),
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(tag_refs).unwrap()),
                            None,
                        )
                        .unwrap(),
                    ),
                ],
                tags.len(),
                Selection::All,
            )
            .unwrap();
            let mut bytes = Vec::new();
            let opts = WriteOptions {
                profile: WriteProfile::L1Compact,
                table_kind: TableKind::Generic,
                row_group_size: 32,
                mini_block_size: 32,
                ..WriteOptions::default()
            };
            let mut w = StatamWriter::new(Cursor::new(&mut bytes), Arc::clone(&schema), opts, None)
                .unwrap();
            w.write_batch(&batch).unwrap();
            w.finish().unwrap();
            StatamReader::open(Arc::new(SliceReader::new(bytes)), None).unwrap()
        };

        // File 0: strictly dominated by File 2 (`10..15`).
        // File 1: overlaps with File 2 at tie key `100`, plus a NULL row.
        // File 2: highest timestamps (`100..105`).
        let f0 = make_file(vec![Some(10), Some(12), Some(15)], "f0");
        let f1 = make_file(vec![Some(90), Some(100), None], "f1");
        let f2 = make_file(vec![Some(100), Some(102), Some(105)], "f2");

        // Project ONLY `tag` (col 1) without projecting `ts` (col 0) to verify key return works
        // even when the order-by column is not projected.
        let req = ScanRequest {
            projection: Some(vec![1]),
            top_k: Some(TopKSpec::desc(0, 4)),
            ..ScanRequest::default()
        };
        let batches = StatamReader::scan_top_k_files(&[&f0, &f1, &f2], &req).unwrap();
        assert_eq!(batches.len(), 1);
        let b = &batches[0];
        assert_eq!(b.num_rows(), 4);
        let got: Vec<String> = (0..4)
            .map(|i| match b.column(0).unwrap().get_scalar(i).unwrap() {
                ScalarValue::Utf8(s) => s,
                other => panic!("unexpected scalar: {other:?}"),
            })
            .collect();
        // Order: 105 (f2-2), 102 (f2-1), tie 100 breaks by file_idx -> f1-1 before f2-0!
        assert_eq!(got, vec!["f2-2", "f2-1", "f1-1", "f2-0"]);

        // All-null file: skipped when cutoff heap is full (`NULLS LAST`), included when `k` exceeds
        // non-null count.
        let f_all_null = make_file(vec![None, None], "fnull");
        let batches_full = StatamReader::scan_top_k_files(
            &[&f_all_null, &f2],
            &ScanRequest {
                projection: Some(vec![1]),
                top_k: Some(TopKSpec::desc(0, 3)),
                ..ScanRequest::default()
            },
        )
        .unwrap();
        assert_eq!(batches_full.len(), 1);
        assert_eq!(batches_full[0].num_rows(), 3);
        let batches_with_nulls = StatamReader::scan_top_k_files(
            &[&f_all_null, &f2],
            &ScanRequest {
                projection: Some(vec![1]),
                top_k: Some(TopKSpec::desc(0, 5)),
                ..ScanRequest::default()
            },
        )
        .unwrap();
        assert_eq!(batches_with_nulls[0].num_rows(), 5);

        // Upfront validation rejects invalid projection even when `limit == 0`.
        let err = StatamReader::scan_top_k_files(
            &[&f0, &f2],
            &ScanRequest {
                projection: Some(vec![99]),
                top_k: Some(TopKSpec::desc(0, 0)),
                ..ScanRequest::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, StatamError::SchemaMismatch { .. }));

        // Cross-file `order_col` DataType mismatch (`TimestampNs` vs `I64`) is rejected even when
        // only the shared `tag` column is projected.
        let i64_schema = Arc::new(
            Schema::try_new(vec![
                Field::new("ts", DataType::I64, true),
                Field::new("tag", DataType::Utf8, false),
            ])
            .unwrap(),
        );
        let i64_batch = Batch::try_new(
            Arc::clone(&i64_schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(vec!["a", "b"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
            Selection::All,
        )
        .unwrap();
        let mut i64_bytes = Vec::new();
        let mut w = StatamWriter::new(
            Cursor::new(&mut i64_bytes),
            Arc::clone(&i64_schema),
            WriteOptions::default(),
            None,
        )
        .unwrap();
        w.write_batch(&i64_batch).unwrap();
        w.finish().unwrap();
        let f_i64 = StatamReader::open(Arc::new(SliceReader::new(i64_bytes)), None).unwrap();
        let err = StatamReader::scan_top_k_files(
            &[&f2, &f_i64],
            &ScanRequest {
                projection: Some(vec![1]),
                top_k: Some(TopKSpec::desc(0, 2)),
                ..ScanRequest::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, StatamError::SchemaMismatch { .. }));

        // `top_k: None` fallback honors global `req.limit` across multiple files.
        let fallback_batches = StatamReader::scan_top_k_files(
            &[&f0, &f2],
            &ScanRequest {
                projection: Some(vec![1]),
                limit: Some(4),
                ..ScanRequest::default()
            },
        )
        .unwrap();
        let total_rows: usize = fallback_batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total_rows, 4);
    }

    struct RecordingRangeReader {
        data: bytes::Bytes,
        remote: bool,
        calls: std::sync::Mutex<Vec<Vec<std::ops::Range<u64>>>>,
    }

    impl RecordingRangeReader {
        fn new(data: Vec<u8>, remote: bool) -> Self {
            Self {
                data: bytes::Bytes::from(data),
                remote,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn take_calls(&self) -> Vec<Vec<std::ops::Range<u64>>> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl crate::io::RangeReader for RecordingRangeReader {
        fn file_len(&self) -> u64 {
            self.data.len() as u64
        }

        fn is_remote(&self) -> bool {
            self.remote
        }

        fn read_range(&self, range: std::ops::Range<u64>) -> Result<bytes::Bytes, StatamError> {
            self.calls.lock().unwrap().push(vec![range.clone()]);
            Ok(self.data.slice(range.start as usize..range.end as usize))
        }

        fn read_ranges(
            &self,
            ranges: &[std::ops::Range<u64>],
        ) -> Result<Vec<bytes::Bytes>, StatamError> {
            self.calls.lock().unwrap().push(ranges.to_vec());
            Ok(ranges
                .iter()
                .map(|r| self.data.slice(r.start as usize..r.end as usize))
                .collect())
        }
    }

    fn make_incompressible_bytes(seed: &mut u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut s = *seed;
        while out.len() < len {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            out.extend_from_slice(&s.to_le_bytes());
        }
        out.truncate(len);
        *seed = s;
        out
    }

    #[test]
    fn sorted_multi_rg_filtered_top_k_prunes_dominated_rgs_on_direct_s3() {
        use cetra_column::BinaryData;

        use crate::format::BLOCK_FRAME_HEADER_LEN;
        use crate::io::{RangeReader, SimulatedS3Config, SimulatedS3Reader};

        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("kind", DataType::Utf8, false),
            Field::new("pad", DataType::Binary, false),
        ])
        .unwrap();

        let opts = WriteOptions {
            profile: WriteProfile::L1Compact,
            table_kind: TableKind::Generic,
            row_group_size: 64,
            mini_block_size: 32,
            ..WriteOptions::default()
        };

        let mut statam_bytes = Vec::new();
        let mut rng = 0x27d3_0001_cafe_beefu64;
        {
            let mut w = StatamWriter::new(
                Cursor::new(&mut statam_bytes),
                Arc::clone(&schema),
                opts,
                None,
            )
            .unwrap();

            // 4 disjoint time-sorted RGs; RG 3 has large `pad` (> DEFAULT_TAIL_HINT) so its
            // `ts`/`kind` blocks stay below `tail_start` and must be fetched via Direct S3.
            for rg_i in 0..4usize {
                let base_ts = ((rg_i as i64) + 1) * 1_000;
                let pad_len = if rg_i == 3 { 4 * 1024 } else { 1536 };
                let mut ts_vals = Vec::with_capacity(64);
                let mut kinds = Vec::with_capacity(64);
                let mut pads = Vec::with_capacity(64);
                for i in 0..64usize {
                    ts_vals.push(base_ts + i as i64);
                    kinds.push(if i % 2 == 0 { "llm" } else { "tool" });
                    pads.push(make_incompressible_bytes(&mut rng, pad_len));
                }
                let pad_refs: Vec<&[u8]> = pads.iter().map(Vec::as_slice).collect();
                let batch = Batch::try_new(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::TimestampNs {
                                    values: ts_vals,
                                    utc: true,
                                },
                                None,
                            )
                            .unwrap(),
                        ),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(Utf8Data::try_from_slices(kinds).unwrap()),
                                None,
                            )
                            .unwrap(),
                        ),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Binary(BinaryData::try_from_slices(pad_refs).unwrap()),
                                None,
                            )
                            .unwrap(),
                        ),
                    ],
                    64,
                    Selection::All,
                )
                .unwrap();
                w.write_batch(&batch).unwrap();
            }
            w.finish().unwrap();
        }

        let recording = Arc::new(RecordingRangeReader::new(statam_bytes, true));
        let direct_cfg = SimulatedS3Config::default().with_direct_mode(0);
        let s3 = Arc::new(
            SimulatedS3Reader::new(Arc::clone(&recording) as Arc<dyn RangeReader>, direct_cfg)
                .with_remote_hint(true),
        );
        let reader = StatamReader::open(s3 as Arc<dyn RangeReader>, None).unwrap();
        let _ = recording.take_calls();

        let req = ScanRequest {
            projection: Some(vec![0, 1]),
            predicates: vec![Predicate::new(
                "kind",
                ZonePred::Eq(ZoneValue::Bytes(b"llm".to_vec())),
            )],
            top_k: Some(TopKSpec::desc(0, 10)),
            ..ScanRequest::default()
        };
        let batches = reader.scan(&req).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 10);
        for i in 0..10usize {
            let expected_ts = 4_062i64 - (i as i64) * 2;
            assert_eq!(
                batches[0].column(0).unwrap().get_scalar(i).unwrap(),
                ScalarValue::TimestampNs {
                    value: expected_ts,
                    utc: true,
                }
            );
            assert_eq!(
                batches[0].column(1).unwrap().get_scalar(i).unwrap(),
                ScalarValue::Utf8("llm".to_owned())
            );
        }

        let calls = recording.take_calls();
        assert!(
            !calls.is_empty(),
            "expected direct S3 range fetches for winning RG 3"
        );
        let num_cols = reader.meta().col_dir.len();
        for rg_idx in 0..3usize {
            for pidx in 0..num_cols {
                for blk in reader.meta().rg_blocks(pidx, rg_idx) {
                    if blk.stored_len == 0 {
                        continue;
                    }
                    let blk_start = blk.offset;
                    let blk_end = blk
                        .offset
                        .checked_add(
                            (BLOCK_FRAME_HEADER_LEN as u64)
                                .checked_add(u64::from(blk.stored_len))
                                .unwrap(),
                        )
                        .unwrap();
                    for wave in &calls {
                        for r in wave {
                            assert!(
                                r.end <= blk_start || r.start >= blk_end,
                                "fetched range {r:?} overlapped dominated RG {rg_idx} col {pidx} block {blk_start}..{blk_end}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn overlapping_multi_rg_top_k_coalesces_phase1_regardless_of_remote_hint() {
        use cetra_column::BinaryData;

        use crate::format::BLOCK_FRAME_HEADER_LEN;
        use crate::io::RangeReader;

        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("pad", DataType::Binary, false),
        ])
        .unwrap();

        let opts = WriteOptions {
            profile: WriteProfile::L1Compact,
            table_kind: TableKind::Generic,
            row_group_size: 64,
            mini_block_size: 32,
            ..WriteOptions::default()
        };

        let mut statam_bytes = Vec::new();
        let mut rng = 0x27d3_0002_dead_beefu64;
        {
            let mut w = StatamWriter::new(
                Cursor::new(&mut statam_bytes),
                Arc::clone(&schema),
                opts,
                None,
            )
            .unwrap();

            // RG0: ts 1_000..1_630, RG1: ts 1_300..1_930 (bounds overlap).
            // Large `pad` keeps both RGs' `ts` blocks strictly below `tail_start`.
            let configs = [(1_000i64, 2560usize), (1_300i64, 4 * 1024)];
            for (base_ts, pad_len) in configs {
                let mut ts_vals = Vec::with_capacity(64);
                let mut pads = Vec::with_capacity(64);
                for i in 0..64usize {
                    ts_vals.push(base_ts + (i as i64) * 10);
                    pads.push(make_incompressible_bytes(&mut rng, pad_len));
                }
                let pad_refs: Vec<&[u8]> = pads.iter().map(Vec::as_slice).collect();
                let batch = Batch::try_new(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::TimestampNs {
                                    values: ts_vals,
                                    utc: true,
                                },
                                None,
                            )
                            .unwrap(),
                        ),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Binary(BinaryData::try_from_slices(pad_refs).unwrap()),
                                None,
                            )
                            .unwrap(),
                        ),
                    ],
                    64,
                    Selection::All,
                )
                .unwrap();
                w.write_batch(&batch).unwrap();
            }
            w.finish().unwrap();
        }

        for is_remote in [false, true] {
            let recording = Arc::new(RecordingRangeReader::new(statam_bytes.clone(), is_remote));
            let reader =
                StatamReader::open(Arc::clone(&recording) as Arc<dyn RangeReader>, None).unwrap();
            let _ = recording.take_calls();

            let req = ScanRequest {
                projection: Some(vec![0]),
                top_k: Some(TopKSpec::desc(0, 10)),
                ..ScanRequest::default()
            };
            let batches = reader.scan(&req).unwrap();
            assert_eq!(batches.len(), 1);
            assert_eq!(batches[0].num_rows(), 10);
            for i in 0..10usize {
                assert_eq!(
                    batches[0].column(0).unwrap().get_scalar(i).unwrap(),
                    ScalarValue::TimestampNs {
                        value: 1_930i64 - (i as i64) * 10,
                        utc: true,
                    }
                );
            }

            let calls = recording.take_calls();
            assert_eq!(
                calls.len(),
                2,
                "expected 1 coalesced Phase-1 read_ranges wave + 1 Phase-2 take wave (is_remote={is_remote}); got {calls:?}"
            );
            let p1_wave = &calls[0];
            for rg_idx in 0..2usize {
                for blk in reader.meta().rg_blocks(0, rg_idx) {
                    if blk.stored_len == 0 {
                        continue;
                    }
                    let blk_start = blk.offset;
                    let blk_end = blk
                        .offset
                        .checked_add(
                            (BLOCK_FRAME_HEADER_LEN as u64)
                                .checked_add(u64::from(blk.stored_len))
                                .unwrap(),
                        )
                        .unwrap();
                    assert!(
                        p1_wave
                            .iter()
                            .any(|r| r.start <= blk_start && blk_end <= r.end),
                        "Phase-1 wave did not cover RG {rg_idx} order block {blk_start}..{blk_end} (is_remote={is_remote}): {p1_wave:?}"
                    );
                }
            }
        }
    }
}
