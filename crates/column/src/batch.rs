//! Columnar batch with attached selection vector.

use std::fmt;
use std::sync::Arc;

use crate::column::{FlatPiece, reconcile_reservation, rollback_reservation};
use crate::{
    Bitmap, Column, ColumnError, FlatColumn, FlatData, MAX_NESTING_DEPTH, Reservation, Schema,
    Selection,
};

struct SeenPtrs<T: Copy + PartialEq> {
    inline: [T; 64],
    inline_len: usize,
    overflow: Vec<T>,
}

impl<T: Copy + PartialEq> SeenPtrs<T> {
    fn new(zero: T) -> Self {
        Self {
            inline: [zero; 64],
            inline_len: 0,
            overflow: Vec::new(),
        }
    }

    fn insert(&mut self, ptr: T) -> bool {
        if self.inline[..self.inline_len].contains(&ptr) || self.overflow.contains(&ptr) {
            return false;
        }
        if self.inline_len < self.inline.len() {
            self.inline[self.inline_len] = ptr;
            self.inline_len += 1;
        } else {
            self.overflow.push(ptr);
        }
        true
    }
}

fn dedup_flat_column_bytes(
    fc: &FlatColumn,
    seen_flats: &mut SeenPtrs<*const FlatColumn>,
) -> Result<usize, ColumnError> {
    if let FlatData::List {
        offsets, values, ..
    } = fc.data()
    {
        let offsets_bytes = offsets.len().checked_mul(4).ok_or(ColumnError::Overflow {
            reason: "list column byte total overflow",
        })?;
        let nulls_bytes = fc.nulls().map_or(0, Bitmap::memory_bytes);
        let child_bytes = dedup_column_bytes(values, seen_flats)?;
        return offsets_bytes
            .checked_add(nulls_bytes)
            .and_then(|s| s.checked_add(child_bytes))
            .ok_or(ColumnError::Overflow {
                reason: "batch column byte total overflow",
            });
    }
    Ok(fc.memory_bytes())
}

fn dedup_column_bytes(
    col: &Column,
    seen_flats: &mut SeenPtrs<*const FlatColumn>,
) -> Result<usize, ColumnError> {
    match col {
        Column::Flat(fc) => dedup_flat_column_bytes(fc, seen_flats),
        Column::Const { value, .. } => Ok(value.heap_bytes()),
        Column::Dict(d) => {
            let codes_bytes = d
                .codes()
                .len()
                .checked_mul(4)
                .ok_or(ColumnError::Overflow {
                    reason: "dict column byte total overflow",
                })?;
            let base = codes_bytes
                .checked_add(d.nulls().map_or(0, Bitmap::memory_bytes))
                .ok_or(ColumnError::Overflow {
                    reason: "dict column byte total overflow",
                })?;
            if seen_flats.insert(Arc::as_ptr(d.values_arc())) {
                base.checked_add(dedup_flat_column_bytes(d.values(), seen_flats)?)
                    .ok_or(ColumnError::Overflow {
                        reason: "batch column byte total overflow",
                    })
            } else {
                Ok(base)
            }
        }
        Column::Rle(r) => {
            let base = r
                .run_ends()
                .len()
                .checked_mul(4)
                .ok_or(ColumnError::Overflow {
                    reason: "rle column byte total overflow",
                })?;
            if seen_flats.insert(Arc::as_ptr(r.values_arc())) {
                base.checked_add(dedup_flat_column_bytes(r.values(), seen_flats)?)
                    .ok_or(ColumnError::Overflow {
                        reason: "batch column byte total overflow",
                    })
            } else {
                Ok(base)
            }
        }
    }
}

/// Columnar batch of rows with an attached [`Selection`] filter.
#[derive(Clone, PartialEq)]
pub struct Batch {
    schema: Arc<Schema>,
    columns: Vec<Arc<Column>>,
    raw_len: usize,
    selection: Selection,
}

impl fmt::Debug for Batch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Batch")
            .field("schema", &self.schema)
            .field("columns", &self.columns)
            .field("raw_len", &self.raw_len)
            .field("selection", &self.selection)
            .finish()
    }
}

fn validate_non_null_fields(
    schema: &Schema,
    columns: &[Arc<Column>],
    selection: &Selection,
) -> Result<(), ColumnError> {
    for (field, col) in schema.fields().iter().zip(columns) {
        if !field.nullable && col.has_nulls() {
            let has_active_null = match selection {
                Selection::All => true,
                Selection::Mask(m) => m.iter_ones().any(|r| col.is_null(r)),
                Selection::Indices(idx) => idx.iter().any(|&r| col.is_null(r as usize)),
            };
            if has_active_null {
                return Err(ColumnError::InvalidBatch {
                    reason: "null value in non-nullable column",
                });
            }
        }
    }
    Ok(())
}

impl Batch {
    /// Validates `raw_len <= u32::MAX as usize`, column count == `schema.len()`, every column's
    /// `data_type()` and `len() == raw_len`, `Const` scalar validity, `selection.validate(raw_len)`,
    /// and non-nullable field constraints across active rows without allocating `to_indices` or
    /// looping `0..raw_len`.
    pub fn try_new(
        schema: Arc<Schema>,
        columns: Vec<Column>,
        raw_len: usize,
        selection: Selection,
    ) -> Result<Self, ColumnError> {
        let shared = columns.into_iter().map(Arc::new).collect();
        Self::try_new_shared(schema, shared, raw_len, selection)
    }

    /// Zero-copy shared-column variant of [`Self::try_new`] taking `Vec<Arc<Column>>`.
    pub fn try_new_shared(
        schema: Arc<Schema>,
        columns: Vec<Arc<Column>>,
        raw_len: usize,
        selection: Selection,
    ) -> Result<Self, ColumnError> {
        if raw_len > u32::MAX as usize {
            return Err(ColumnError::Overflow {
                reason: "batch raw_len exceeds u32::MAX",
            });
        }
        if columns.len() != schema.len() {
            return Err(ColumnError::InvalidBatch {
                reason: "column count does not match schema",
            });
        }
        selection.validate(raw_len)?;
        for (field, col) in schema.fields().iter().zip(&columns) {
            if col.len() != raw_len {
                return Err(ColumnError::InvalidBatch {
                    reason: "column length does not match batch raw_len",
                });
            }
            if col.data_type() != field.data_type {
                return Err(ColumnError::InvalidBatch {
                    reason: "column data type does not match schema field",
                });
            }
            col.validate_with_depth(MAX_NESTING_DEPTH)?;
        }
        validate_non_null_fields(&schema, &columns, &selection)?;
        Ok(Self {
            schema,
            columns,
            raw_len,
            selection,
        })
    }

    /// Convenience constructor with `Selection::All`.
    pub fn try_new_all(
        schema: Arc<Schema>,
        columns: Vec<Column>,
        raw_len: usize,
    ) -> Result<Self, ColumnError> {
        Self::try_new(schema, columns, raw_len, Selection::All)
    }

    /// Creates an empty batch (0 rows, empty `FlatColumn` per field, `Selection::All`).
    pub fn empty(schema: Arc<Schema>) -> Self {
        let columns = schema
            .fields()
            .iter()
            .map(|f| Arc::new(Column::Flat(FlatColumn::empty(&f.data_type))))
            .collect();
        Self {
            schema,
            columns,
            raw_len: 0,
            selection: Selection::All,
        }
    }

    /// Batch schema.
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// Consumes this batch and returns its raw `(schema, columns, raw_len, selection)` parts
    /// without cloning column buffers.
    pub fn into_parts(self) -> (Arc<Schema>, Vec<Arc<Column>>, usize, Selection) {
        (self.schema, self.columns, self.raw_len, self.selection)
    }

    /// All columns in schema order.
    pub fn columns(&self) -> &[Arc<Column>] {
        &self.columns
    }

    /// Column at index `index`.
    pub fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index).map(Arc::as_ref)
    }

    /// Shared `Arc<Column>` handle at index `index`.
    pub fn column_arc(&self, index: usize) -> Option<&Arc<Column>> {
        self.columns.get(index)
    }

    /// Column by exact field name.
    pub fn column_by_name(&self, name: &str) -> Option<&Column> {
        self.schema.index_of(name).and_then(|idx| self.column(idx))
    }

    /// Physical row count before applying `selection`.
    pub fn raw_len(&self) -> usize {
        self.raw_len
    }

    /// Active row count after applying `selection`.
    pub fn num_rows(&self) -> usize {
        self.selection.active_len(self.raw_len)
    }

    /// True when `num_rows() == 0`.
    pub fn is_empty(&self) -> bool {
        self.num_rows() == 0
    }

    /// Active selection vector.
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Replaces `selection` after validating against `raw_len` and non-null field constraints,
    /// sharing underlying `Arc<Column>` handles with zero buffer copying.
    pub fn with_selection(&self, selection: Selection) -> Result<Self, ColumnError> {
        selection.validate(self.raw_len)?;
        validate_non_null_fields(&self.schema, &self.columns, &selection)?;
        Ok(Self {
            schema: Arc::clone(&self.schema),
            columns: self.columns.clone(),
            raw_len: self.raw_len,
            selection,
        })
    }

    /// Intersects the current selection with `mask` (length `raw_len`) without copying column buffers.
    pub fn filter_mask(&self, mask: &Bitmap) -> Result<Self, ColumnError> {
        let next = self.selection.intersect_mask(self.raw_len, mask)?;
        self.with_selection(next)
    }

    /// Projects a subset of columns by index, keeping `selection` intact and sharing
    /// `Arc<Column>` handles with zero buffer copying (even for duplicate indices).
    pub fn project(&self, indices: &[usize]) -> Result<Self, ColumnError> {
        let new_schema = Arc::new(self.schema.project(indices)?);
        let new_columns = indices
            .iter()
            .map(|&i| Arc::clone(&self.columns[i]))
            .collect();
        Ok(Self {
            schema: new_schema,
            columns: new_columns,
            raw_len: self.raw_len,
            selection: self.selection.clone(),
        })
    }

    /// Consumes `self` and replaces `selection` without copying column buffers.
    pub fn into_with_selection(mut self, selection: Selection) -> Result<Self, ColumnError> {
        selection.validate(self.raw_len)?;
        validate_non_null_fields(&self.schema, &self.columns, &selection)?;
        self.selection = selection;
        Ok(self)
    }

    /// Consumes `self` and intersects the current selection with `mask` (length `raw_len`)
    /// without copying column buffers.
    pub fn into_filter_mask(self, mask: &Bitmap) -> Result<Self, ColumnError> {
        let next = self.selection.intersect_mask(self.raw_len, mask)?;
        self.into_with_selection(next)
    }

    /// Consumes `self` and projects a subset of columns by index, keeping `selection` intact
    /// and sharing `Arc<Column>` handles with zero buffer copying (even for duplicate indices).
    pub fn into_project(mut self, indices: &[usize]) -> Result<Self, ColumnError> {
        let new_schema = Arc::new(self.schema.project(indices)?);
        let new_columns = indices
            .iter()
            .map(|&i| Arc::clone(&self.columns[i]))
            .collect();
        self.schema = new_schema;
        self.columns = new_columns;
        Ok(self)
    }

    /// Slices physical rows `offset..offset + len`, slicing `selection` accordingly.
    /// Deduplicates shared `Arc<Column>` and `Arc<FlatColumn>` pointers and reserves the
    /// sliced batch's full `memory_bytes()` in `pool` when `Some` (rolling back on error).
    pub fn slice(
        &self,
        offset: usize,
        len: usize,
        mut pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        self.selection.validate(self.raw_len)?;
        if offset.checked_add(len).is_none_or(|end| end > self.raw_len) {
            return Err(ColumnError::InvalidBatch {
                reason: "selection slice out of bounds",
            });
        }

        let before = pool.as_ref().map_or(0, |p| p.size());
        let sel_bytes = match &self.selection {
            Selection::All => 0,
            Selection::Mask(_) => len
                .div_ceil(64)
                .checked_mul(8)
                .ok_or(ColumnError::Overflow {
                    reason: "selection slice bitmap overflow",
                })?,
            Selection::Indices(idx) => {
                let end = offset + len;
                let count = idx
                    .iter()
                    .filter(|&&i| (i as usize) >= offset && (i as usize) < end)
                    .count();
                count.checked_mul(4).ok_or(ColumnError::Overflow {
                    reason: "selection slice indices overflow",
                })?
            }
        };
        if let Some(res) = pool.as_deref_mut()
            && let Err(err) = res.try_grow(sel_bytes)
        {
            rollback_reservation(pool, before);
            return Err(err);
        }
        let new_sel = match self.selection.slice(self.raw_len, offset, len) {
            Ok(s) => s,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        debug_assert_eq!(new_sel.memory_bytes(), sel_bytes);

        let mut seen_inline: [(*const Column, usize); 64] = [(std::ptr::null(), 0); 64];
        let mut seen_inline_len = 0usize;
        let mut seen_overflow: Vec<(*const Column, usize)> = Vec::new();
        let mut seen_dict_values: SeenPtrs<*const FlatColumn> = SeenPtrs::new(std::ptr::null());
        let mut new_cols: Vec<Arc<Column>> = Vec::with_capacity(self.columns.len());

        for col in &self.columns {
            let ptr = Arc::as_ptr(col);
            let existing_idx = seen_inline[..seen_inline_len]
                .iter()
                .find_map(|&(p, idx)| (p == ptr).then_some(idx))
                .or_else(|| {
                    seen_overflow
                        .iter()
                        .find_map(|&(p, idx)| (p == ptr).then_some(idx))
                });

            if let Some(idx) = existing_idx {
                new_cols.push(Arc::clone(&new_cols[idx]));
                continue;
            }

            let slice_res = match col.as_ref() {
                Column::Dict(d) => {
                    let account_values = seen_dict_values.insert(Arc::as_ptr(d.values_arc()));
                    d.slice_with_accounting(offset, len, pool.as_deref_mut(), account_values)
                        .map(Column::Dict)
                }
                _ => col.slice(offset, len, pool.as_deref_mut()),
            };
            let sliced = match slice_res {
                Ok(c) => Arc::new(c),
                Err(err) => {
                    rollback_reservation(pool, before);
                    return Err(err);
                }
            };
            let out_idx = new_cols.len();
            if seen_inline_len < seen_inline.len() {
                seen_inline[seen_inline_len] = (ptr, out_idx);
                seen_inline_len += 1;
            } else {
                seen_overflow.push((ptr, out_idx));
            }
            new_cols.push(sliced);
        }

        let out = match Self::try_new_shared(Arc::clone(&self.schema), new_cols, len, new_sel) {
            Ok(batch) => batch,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        reconcile_reservation(pool, before, out.memory_bytes())?;
        Ok(out)
    }

    fn checked_dedup_columns_memory_bytes(&self) -> Result<usize, ColumnError> {
        let mut total = 0usize;
        let mut seen_cols: SeenPtrs<*const Column> = SeenPtrs::new(std::ptr::null());
        let mut seen_flats: SeenPtrs<*const FlatColumn> = SeenPtrs::new(std::ptr::null());

        for col in &self.columns {
            if seen_cols.insert(Arc::as_ptr(col)) {
                let col_bytes = dedup_column_bytes(col, &mut seen_flats)?;
                total = total.checked_add(col_bytes).ok_or(ColumnError::Overflow {
                    reason: "batch column byte total overflow",
                })?;
            }
        }
        Ok(total)
    }

    /// Materializes active rows (`Selection::All`, `raw_len == num_rows()`).
    /// When `self.selection.is_all(self.raw_len)`, preserves `Const`/`Dict`/`Rle` encodings and sets `Selection::All`;
    /// otherwise gathers active rows via `Column::take`. Reserves net output `memory_bytes()` from `pool`
    /// and rolls back all reservations on error.
    pub fn compact(&self, mut pool: Option<&mut Reservation>) -> Result<Self, ColumnError> {
        let before = pool.as_ref().map_or(0, |p| p.size());
        if self.selection.is_all(self.raw_len) {
            let target_bytes = self.checked_dedup_columns_memory_bytes()?;
            reconcile_reservation(pool, before, target_bytes)?;
            return Ok(Self {
                schema: Arc::clone(&self.schema),
                columns: self.columns.clone(),
                raw_len: self.raw_len,
                selection: Selection::All,
            });
        }

        let indices = match self.selection.to_indices(self.raw_len) {
            Ok(v) => v,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };

        let mut new_cols = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            match col.take(&indices, pool.as_deref_mut()) {
                Ok(flat) => new_cols.push(Column::Flat(flat)),
                Err(err) => {
                    rollback_reservation(pool, before);
                    return Err(err);
                }
            }
        }
        let out = match Self::try_new_all(Arc::clone(&self.schema), new_cols, indices.len()) {
            Ok(b) => b,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        reconcile_reservation(pool, before, out.memory_bytes())?;
        Ok(out)
    }

    /// Concatenates batches with identical schemas into one compacted `Selection::All` batch.
    /// Reserves exact final output `memory_bytes()` in `pool` (releasing any temporary scratch
    /// reservations before returning `Ok`, and rolling back everything on `Err`).
    pub fn concat(
        schema: Arc<Schema>,
        batches: &[&Batch],
        mut pool: Option<&mut Reservation>,
    ) -> Result<Self, ColumnError> {
        let before = pool.as_ref().map_or(0, |p| p.size());
        for b in batches {
            if b.schema().as_ref() != schema.as_ref() {
                return Err(ColumnError::InvalidBatch {
                    reason: "schema mismatch in Batch::concat",
                });
            }
        }

        let mut total_rows = 0usize;
        for b in batches {
            total_rows = total_rows
                .checked_add(b.num_rows())
                .ok_or(ColumnError::Overflow {
                    reason: "concat row count overflow",
                })?;
        }
        if total_rows > u32::MAX as usize {
            return Err(ColumnError::Overflow {
                reason: "concat row count exceeds u32::MAX",
            });
        }

        if total_rows == 0 {
            let out = Self::empty(schema);
            reconcile_reservation(pool, before, out.memory_bytes())?;
            return Ok(out);
        }

        let mut batch_indices: Vec<Option<Vec<u32>>> = Vec::with_capacity(batches.len());
        let mut active_batches: Vec<&Batch> = Vec::with_capacity(batches.len());
        for b in batches {
            if b.num_rows() == 0 {
                continue;
            }
            if b.selection.is_all(b.raw_len) {
                batch_indices.push(None);
            } else {
                let idx = match b.selection.to_indices(b.raw_len) {
                    Ok(v) => v,
                    Err(err) => {
                        rollback_reservation(pool, before);
                        return Err(err);
                    }
                };
                batch_indices.push(Some(idx));
            }
            active_batches.push(b);
        }

        let mut new_cols = Vec::with_capacity(schema.len());
        let mut running_bytes = 0usize;

        for (col_idx, field) in schema.fields().iter().enumerate() {
            let mut pieces: Vec<FlatPiece<'_>> = Vec::with_capacity(active_batches.len());
            for (b, maybe_idx) in active_batches.iter().zip(&batch_indices) {
                let col = b.columns[col_idx].as_ref();
                let piece = match (maybe_idx, col) {
                    (None, Column::Flat(fc)) => FlatPiece::Borrowed(fc),
                    (None, other) => match other.materialize(pool.as_deref_mut()) {
                        Ok(fc) => FlatPiece::Owned(fc),
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    },
                    (Some(idx), other) => match other.take(idx, pool.as_deref_mut()) {
                        Ok(fc) => FlatPiece::Owned(fc),
                        Err(err) => {
                            rollback_reservation(pool, before);
                            return Err(err);
                        }
                    },
                };
                pieces.push(piece);
            }

            let flat_refs: Vec<&FlatColumn> = pieces.iter().map(FlatPiece::as_flat).collect();
            let merged = match FlatColumn::concat_with_pool(
                &field.data_type,
                &flat_refs,
                total_rows,
                pool.as_deref_mut(),
            ) {
                Ok(fc) => fc,
                Err(err) => {
                    rollback_reservation(pool, before);
                    return Err(err);
                }
            };
            drop(pieces);

            running_bytes = match running_bytes.checked_add(merged.memory_bytes()) {
                Some(b) => b,
                None => {
                    rollback_reservation(pool, before);
                    return Err(ColumnError::Overflow {
                        reason: "concat byte total overflow",
                    });
                }
            };
            reconcile_reservation(pool.as_deref_mut(), before, running_bytes)?;
            new_cols.push(Column::Flat(merged));
        }

        let out = match Self::try_new_all(schema, new_cols, total_rows) {
            Ok(b) => b,
            Err(err) => {
                rollback_reservation(pool, before);
                return Err(err);
            }
        };
        reconcile_reservation(pool, before, out.memory_bytes())?;
        Ok(out)
    }

    /// NaN-aware, encoding-independent semantic equality over active rows.
    pub fn semantic_eq(&self, other: &Self) -> bool {
        if self.schema.as_ref() != other.schema.as_ref() || self.num_rows() != other.num_rows() {
            return false;
        }
        if self.selection.is_all(self.raw_len) && other.selection.is_all(other.raw_len) {
            return self
                .columns
                .iter()
                .zip(&other.columns)
                .all(|(a, b)| a.semantic_eq(b));
        }
        let Ok(idx_a) = self.selection.to_indices(self.raw_len) else {
            return false;
        };
        let Ok(idx_b) = other.selection.to_indices(other.raw_len) else {
            return false;
        };
        for (col_a, col_b) in self.columns.iter().zip(&other.columns) {
            for (&ra, &rb) in idx_a.iter().zip(&idx_b) {
                let Ok(sa) = col_a.get_scalar(ra as usize) else {
                    return false;
                };
                let Ok(sb) = col_b.get_scalar(rb as usize) else {
                    return false;
                };
                if !sa.semantic_eq(&sb) {
                    return false;
                }
            }
        }
        true
    }

    /// Deterministic logical heap bytes across unique columns (deduplicated by `Arc::as_ptr`
    /// for both `Arc<Column>` and inner `Dict`/`Rle` `Arc<FlatColumn>`) and selection.
    pub fn memory_bytes(&self) -> usize {
        self.selection.memory_bytes().saturating_add(
            self.checked_dedup_columns_memory_bytes()
                .unwrap_or(usize::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crate::{
        DataType, DictColumn, EventItem, Field, FlatData, GlobalPool, Interval, LinkItem,
        RleColumn, ScalarValue, Utf8Data,
    };

    use super::*;

    #[test]
    fn filter_mask_does_not_copy_and_compact_preserves_encodings_on_is_all() {
        let schema = Schema::try_new_arc(vec![
            Field::new("const_i64", DataType::I64, false),
            Field::new("dict_utf8", DataType::Utf8, true),
            Field::new("rle_f64", DataType::F64, false),
        ])
        .unwrap();

        let c0 = Column::try_new_const(ScalarValue::I64(7), 4).unwrap();
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["a", "b"]).unwrap()),
            None,
        )
        .unwrap();
        let c1 = Column::Dict(
            DictColumn::try_new(
                dict_vals,
                vec![0, 1, 0, 1],
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap(),
        );
        let rle_vals = FlatColumn::try_new(FlatData::F64(vec![1.5, f64::NAN]), None).unwrap();
        let c2 = Column::Rle(RleColumn::try_new(rle_vals, vec![2, 4]).unwrap());

        let batch = Batch::try_new_all(Arc::clone(&schema), vec![c0, c1, c2], 4).unwrap();

        // Mask that selects all rows preserves encodings on compact
        let batch_all_mask = batch
            .with_selection(Selection::Mask(Bitmap::new_ones(4)))
            .unwrap();
        let compacted_all = batch_all_mask.compact(None).unwrap();
        assert!(matches!(*compacted_all.columns()[0], Column::Const { .. }));
        assert!(matches!(*compacted_all.columns()[1], Column::Dict(_)));
        assert!(matches!(*compacted_all.columns()[2], Column::Rle(_)));
        assert_eq!(compacted_all.selection(), &Selection::All);

        // filter_mask keeps raw_len unchanged
        let filtered = batch
            .filter_mask(&Bitmap::from_bools(&[true, false, false, true]))
            .unwrap();
        assert_eq!(filtered.raw_len(), 4);
        assert_eq!(filtered.num_rows(), 2);

        // compact drops filtered rows and materializes
        let compacted = filtered.compact(None).unwrap();
        assert_eq!(compacted.raw_len(), 2);
        assert_eq!(compacted.num_rows(), 2);
        assert_eq!(compacted.selection(), &Selection::All);
        assert!(filtered.semantic_eq(&compacted));

        // project
        let projected = filtered.project(&[2, 0]).unwrap();
        assert_eq!(projected.schema().len(), 2);
        assert_eq!(projected.num_rows(), 2);
        assert_eq!(projected.raw_len(), 4);

        // slice
        let sliced = filtered.slice(1, 3, None).unwrap();
        assert_eq!(sliced.raw_len(), 3);
        assert_eq!(sliced.num_rows(), 1);
        assert!(
            !sliced
                .column_by_name("rle_f64")
                .unwrap()
                .get_scalar(2)
                .unwrap()
                .is_null()
        );

        // concat
        let merged = Batch::concat(Arc::clone(&schema), &[&filtered, &compacted], None).unwrap();
        assert_eq!(merged.num_rows(), 4);
        assert_eq!(merged.raw_len(), 4);
        assert_eq!(merged.selection(), &Selection::All);
    }

    fn arb_data_type(max_depth: usize) -> BoxedStrategy<DataType> {
        let leaf = prop_oneof![
            Just(DataType::Null),
            Just(DataType::Bool),
            Just(DataType::I64),
            Just(DataType::F64),
            Just(DataType::cost_decimal()),
            Just(DataType::timestamp_utc()),
            Just(DataType::Date),
            Just(DataType::Interval),
            Just(DataType::Utf8),
            Just(DataType::Binary),
            Just(DataType::FixedBytes8),
            Just(DataType::FixedBytes16),
            Just(DataType::MapUtf8),
            Just(DataType::Events),
            Just(DataType::Links),
        ];
        if max_depth == 0 {
            leaf.boxed()
        } else {
            leaf.prop_recursive(max_depth as u32, 8, 2, |inner| {
                inner.prop_map(|dt| DataType::List(Box::new(dt)))
            })
            .boxed()
        }
    }

    fn arb_scalar_for_type(dt: &DataType, nullable: bool) -> BoxedStrategy<ScalarValue> {
        let non_null: BoxedStrategy<ScalarValue> = match dt {
            DataType::Null => Just(ScalarValue::Null(DataType::Null)).boxed(),
            DataType::Bool => any::<bool>().prop_map(ScalarValue::Bool).boxed(),
            DataType::I64 => any::<i64>().prop_map(ScalarValue::I64).boxed(),
            DataType::F64 => prop_oneof![any::<f64>(), Just(f64::NAN), Just(0.0), Just(-0.0),]
                .prop_map(ScalarValue::F64)
                .boxed(),
            DataType::Decimal128 { precision, scale } => {
                let p = *precision;
                let s = *scale;
                (-1_000_000i128..=1_000_000i128)
                    .prop_map(move |value| ScalarValue::Decimal128 {
                        value,
                        precision: p,
                        scale: s,
                    })
                    .boxed()
            }
            DataType::TimestampNs { utc } => {
                let u = *utc;
                any::<i64>()
                    .prop_map(move |value| ScalarValue::TimestampNs { value, utc: u })
                    .boxed()
            }
            DataType::Date => any::<i32>().prop_map(ScalarValue::Date).boxed(),
            DataType::Interval => (any::<i32>(), any::<i32>(), any::<i64>())
                .prop_map(|(months, days, nanos)| {
                    ScalarValue::Interval(Interval {
                        months,
                        days,
                        nanos,
                    })
                })
                .boxed(),
            DataType::Utf8 => prop::string::string_regex("[a-zA-Z0-9_é🚀]{0,8}")
                .unwrap()
                .prop_map(ScalarValue::Utf8)
                .boxed(),
            DataType::Binary => prop::collection::vec(any::<u8>(), 0..=8)
                .prop_map(ScalarValue::Binary)
                .boxed(),
            DataType::FixedBytes8 => any::<[u8; 8]>().prop_map(ScalarValue::FixedBytes8).boxed(),
            DataType::FixedBytes16 => any::<[u8; 16]>()
                .prop_map(ScalarValue::FixedBytes16)
                .boxed(),
            DataType::List(inner) => {
                let inner_dt = (**inner).clone();
                let elem_strat = arb_scalar_for_type(&inner_dt, true);
                prop::collection::vec(elem_strat, 0..=4)
                    .prop_map(move |values| ScalarValue::List {
                        item_type: inner_dt.clone(),
                        values,
                    })
                    .boxed()
            }
            DataType::MapUtf8 => prop::collection::vec(
                (
                    prop::string::string_regex("[a-z]{1,4}").unwrap(),
                    prop::option::of(prop::string::string_regex("[a-z]{0,4}").unwrap()),
                ),
                0..=3,
            )
            .prop_map(ScalarValue::MapUtf8)
            .boxed(),
            DataType::Events => prop::collection::vec(
                (
                    any::<i64>(),
                    prop::string::string_regex("[a-z]{1,4}").unwrap(),
                    prop::option::of(prop::string::string_regex("[a-z]{0,4}").unwrap()),
                )
                    .prop_map(|(time, name, attributes)| EventItem {
                        time,
                        name,
                        attributes,
                    }),
                0..=2,
            )
            .prop_map(ScalarValue::Events)
            .boxed(),
            DataType::Links => prop::collection::vec(
                (
                    any::<[u8; 16]>(),
                    any::<[u8; 8]>(),
                    prop::option::of(prop::string::string_regex("[a-z]{0,4}").unwrap()),
                )
                    .prop_map(|(trace_id, span_id, attributes)| LinkItem {
                        trace_id,
                        span_id,
                        attributes,
                    }),
                0..=2,
            )
            .prop_map(ScalarValue::Links)
            .boxed(),
        };

        if nullable && !matches!(dt, DataType::Null) {
            let dt_c = dt.clone();
            prop_oneof![
                3 => non_null,
                1 => Just(ScalarValue::Null(dt_c)),
            ]
            .boxed()
        } else {
            non_null
        }
    }

    fn build_column_from_scalars(dt: &DataType, scalars: &[ScalarValue], encoding: u8) -> Column {
        // Build Flat first using Column::try_new_const + concat or direct construction
        let row_cols: Vec<FlatColumn> = scalars
            .iter()
            .map(|s| {
                Column::try_new_const(s.clone(), 1)
                    .unwrap()
                    .materialize(None)
                    .unwrap()
            })
            .collect();
        let flat = if scalars.is_empty() {
            FlatColumn::empty(dt)
        } else {
            let refs: Vec<&FlatColumn> = row_cols.iter().collect();
            FlatColumn::concat(dt, &refs, scalars.len()).unwrap()
        };

        match encoding % 4 {
            0 => Column::Flat(flat),
            1 => {
                if scalars.is_empty() {
                    Column::try_new_const(ScalarValue::Null(dt.clone()), 0).unwrap()
                } else {
                    Column::try_new_const(scalars[0].clone(), scalars.len()).unwrap()
                }
            }
            2 => {
                if scalars.is_empty() {
                    Column::Dict(DictColumn::try_new(FlatColumn::empty(dt), vec![], None).unwrap())
                } else {
                    let codes: Vec<u32> = (0..scalars.len() as u32).collect();
                    Column::Dict(DictColumn::try_new(flat, codes, None).unwrap())
                }
            }
            _ => {
                if scalars.is_empty() {
                    Column::Rle(RleColumn::try_new(FlatColumn::empty(dt), vec![]).unwrap())
                } else {
                    let run_ends: Vec<u32> = (1..=scalars.len() as u32).collect();
                    Column::Rle(RleColumn::try_new(flat, run_ends).unwrap())
                }
            }
        }
    }

    fn arb_batch() -> BoxedStrategy<Batch> {
        (
            0usize..=32,
            prop::collection::vec((arb_data_type(3), any::<bool>(), 0u8..4u8), 0..=6),
        )
            .prop_flat_map(|(raw_len, col_specs)| {
                let col_strats: Vec<BoxedStrategy<(Field, Column)>> = col_specs
                    .into_iter()
                    .enumerate()
                    .map(|(idx, (dt, mut nullable, enc))| {
                        if matches!(dt, DataType::Null) {
                            nullable = true;
                        }
                        let name = format!("c{idx}");
                        let field = Field::new(name, dt.clone(), nullable);
                        let row_strat = prop::collection::vec(
                            arb_scalar_for_type(&dt, nullable),
                            raw_len..=raw_len,
                        );
                        row_strat
                            .prop_map(move |scalars| {
                                let col = build_column_from_scalars(&dt, &scalars, enc);
                                (field.clone(), col)
                            })
                            .boxed()
                    })
                    .collect();

                let sel_strat = if raw_len == 0 {
                    prop_oneof![
                        Just(Selection::All),
                        Just(Selection::Mask(Bitmap::new_zeroed(0))),
                        Just(Selection::Indices(vec![])),
                    ]
                    .boxed()
                } else {
                    prop_oneof![
                        Just(Selection::All),
                        prop::collection::vec(any::<bool>(), raw_len..=raw_len)
                            .prop_map(|b| Selection::Mask(Bitmap::from_bools(&b))),
                        prop::collection::vec(0..raw_len as u32, 0..=raw_len)
                            .prop_map(Selection::Indices),
                    ]
                    .boxed()
                };

                (col_strats, sel_strat, Just(raw_len)).prop_map(|(pairs, sel, raw_len)| {
                    let mut fields = Vec::with_capacity(pairs.len());
                    let mut cols = Vec::with_capacity(pairs.len());
                    for (mut f, c) in pairs {
                        // Const encoding might pick a null at row 0 if nullable; ensure schema matches
                        if c.has_nulls() {
                            f.nullable = true;
                        }
                        fields.push(f);
                        cols.push(c);
                    }
                    let schema = Schema::try_new_arc(fields).unwrap();
                    Batch::try_new(schema, cols, raw_len, sel).unwrap()
                })
            })
            .boxed()
    }

    proptest! {
        #[test]
        fn proptest_take_and_slice_match_reference(
            batch in arb_batch(),
            slice_seed in any::<(usize, usize)>(),
        ) {
            let scope = GlobalPool::standalone(64 * 1024 * 1024);
            let mut res = scope.register("prop", true);

            let compacted = batch.compact(Some(&mut res)).unwrap();
            prop_assert_eq!(res.size(), compacted.memory_bytes());
            prop_assert!(batch.semantic_eq(&compacted));
            prop_assert_eq!(compacted.num_rows(), compacted.raw_len());
            prop_assert_eq!(compacted.selection(), &Selection::All);

            let raw_len = batch.raw_len();
            let offset = if raw_len == 0 { 0 } else { slice_seed.0 % (raw_len + 1) };
            let len = if raw_len == offset { 0 } else { slice_seed.1 % (raw_len - offset + 1) };

            let sliced = batch.slice(offset, len, None).unwrap();
            let sliced_compacted = sliced.compact(None).unwrap();
            prop_assert!(sliced.semantic_eq(&sliced_compacted));

            // Reference: compare each column via get_scalar over active indices of sliced
            let active = sliced.selection().to_indices(sliced.raw_len()).unwrap();
            prop_assert_eq!(active.len(), sliced_compacted.num_rows());
            for (c_idx, col) in sliced.columns().iter().enumerate() {
                for (out_pos, &raw_pos) in active.iter().enumerate() {
                    let expected = col.get_scalar(raw_pos as usize).unwrap();
                    let actual = sliced_compacted.columns()[c_idx].get_scalar(out_pos).unwrap();
                    prop_assert!(expected.semantic_eq(&actual));
                }
            }

            // Concat with itself matches twice the rows
            let doubled = Batch::concat(
                Arc::clone(batch.schema()),
                &[&batch, &compacted],
                Some(&mut res),
            ).unwrap();
            prop_assert_eq!(res.size(), compacted.memory_bytes() + doubled.memory_bytes());
            prop_assert_eq!(doubled.num_rows(), batch.num_rows() * 2);
        }
    }

    #[test]
    fn concat_and_compact_respect_tight_memory_pool_limits() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
        ])
        .unwrap();

        let id1 = Column::try_new_const(ScalarValue::I64(42), 4).unwrap();
        let tags1 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 2, 4, 6, 8],
                    values: Box::new(
                        Column::try_new_const(ScalarValue::Utf8("tag".into()), 8).unwrap(),
                    ),
                },
                None,
            )
            .unwrap(),
        );
        let b1 = Batch::try_new_all(Arc::clone(&schema), vec![id1, tags1], 4).unwrap();

        let id2 = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20]), None).unwrap());
        let tags2 = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 1, 2],
                    values: Box::new(Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(["aa", "bb"]).unwrap()),
                            None,
                        )
                        .unwrap(),
                    )),
                },
                None,
            )
            .unwrap(),
        );
        let b2 = Batch::try_new_all(Arc::clone(&schema), vec![id2, tags2], 2).unwrap();

        // Compact is_all fast path rejects before cloning when pool is too small
        let tiny_scope = GlobalPool::standalone(b2.memory_bytes() - 1);
        let mut tiny_res = tiny_scope.register("compact_tiny", true);
        let err_compact = b2.compact(Some(&mut tiny_res)).unwrap_err();
        assert!(matches!(err_compact, ColumnError::MemoryLimit { .. }));
        assert_eq!(tiny_res.size(), 0);

        // Compute exact merged size using an ample pool first
        let big_scope = GlobalPool::standalone(1024 * 1024);
        let mut big_res = big_scope.register("big", true);
        let merged_ok =
            Batch::concat(Arc::clone(&schema), &[&b1, &b2], Some(&mut big_res)).unwrap();
        let merged_bytes = merged_ok.memory_bytes();
        assert_eq!(big_res.size(), merged_bytes);

        // A pool smaller than merged_bytes (or smaller than peak live pieces + merged) fails cleanly and rolls back
        let tight_scope = GlobalPool::standalone(merged_bytes + 40);
        let mut tight_res = tight_scope.register("tight", true);
        tight_res.try_grow(40).unwrap();
        let err_concat =
            Batch::concat(Arc::clone(&schema), &[&b1, &b2], Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err_concat, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 40);

        // Also test borrowed flat-only concat rejecting before allocating merged when cap = merged_bytes - 1
        let b1_flat = b1.with_selection(Selection::All).unwrap();
        let b1_mat = Batch::try_new_all(
            Arc::clone(&schema),
            b1_flat
                .columns()
                .iter()
                .map(|c| Column::Flat(c.materialize(None).unwrap()))
                .collect(),
            b1_flat.raw_len(),
        )
        .unwrap();
        let flat_merged_bytes = Batch::concat(Arc::clone(&schema), &[&b1_mat, &b2], None)
            .unwrap()
            .memory_bytes();
        let exact_minus_one = GlobalPool::standalone(flat_merged_bytes - 1);
        let mut minus_one_res = exact_minus_one.register("minus1", true);
        let err_flat = Batch::concat(
            Arc::clone(&schema),
            &[&b1_mat, &b2],
            Some(&mut minus_one_res),
        )
        .unwrap_err();
        assert!(matches!(err_flat, ColumnError::MemoryLimit { .. }));
        assert_eq!(minus_one_res.size(), 0);
    }

    #[test]
    fn into_selection_and_into_project_move_without_cloning_unique_columns() {
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::I64, false),
            Field::new("b", DataType::I64, true),
            Field::new("c", DataType::I64, false),
        ])
        .unwrap();

        let c0 = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20, 30]), None).unwrap());
        let c1 = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![1, 2, 3]),
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let c2 = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![7, 8, 9]), None).unwrap());

        let ptr0 = match &c0 {
            Column::Flat(fc) => match fc.data() {
                FlatData::I64(v) => v.as_ptr(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let ptr2 = match &c2 {
            Column::Flat(fc) => match fc.data() {
                FlatData::I64(v) => v.as_ptr(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };

        let batch = Batch::try_new_all(Arc::clone(&schema), vec![c0, c1, c2], 3).unwrap();

        // Non-null check rejects null row in column b if we project/validate?
        // First test into_filter_mask + into_with_selection move c0 without reallocating
        let mask = Bitmap::from_bools(&[true, false, true]);
        let filtered = batch.into_filter_mask(&mask).unwrap();
        assert_eq!(filtered.num_rows(), 2);
        assert_eq!(filtered.raw_len(), 3);

        // into_project with duplicate index [2, 0, 2]: shares Arc<Column> across all slots
        // with zero buffer copying, even on duplicate indices.
        let projected = filtered.into_project(&[2, 0, 2]).unwrap();
        assert_eq!(projected.schema().len(), 3);
        assert_eq!(projected.num_rows(), 2);
        assert_eq!(projected.raw_len(), 3);

        let moved_ptr2 = match projected.column(0).unwrap() {
            Column::Flat(fc) => match fc.data() {
                FlatData::I64(v) => v.as_ptr(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let moved_ptr0 = match projected.column(1).unwrap() {
            Column::Flat(fc) => match fc.data() {
                FlatData::I64(v) => v.as_ptr(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let shared_ptr2 = match projected.column(2).unwrap() {
            Column::Flat(fc) => match fc.data() {
                FlatData::I64(v) => v.as_ptr(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        assert_eq!(moved_ptr2, ptr2);
        assert_eq!(moved_ptr0, ptr0);
        assert_eq!(shared_ptr2, ptr2);
        assert!(Arc::ptr_eq(
            projected.column_arc(0).unwrap(),
            projected.column_arc(2).unwrap()
        ));

        // Empty projection yields 0 columns while preserving raw_len and selection
        let empty_proj = projected.clone().into_project(&[]).unwrap();
        assert_eq!(empty_proj.schema().len(), 0);
        assert_eq!(empty_proj.columns().len(), 0);
        assert_eq!(empty_proj.raw_len(), 3);
        assert_eq!(empty_proj.num_rows(), 2);

        // Out-of-bounds projection fails cleanly
        assert!(matches!(
            projected.clone().into_project(&[99]),
            Err(ColumnError::InvalidBatch { .. })
        ));

        // into_with_selection validates selection bounds and non-null constraints
        let with_sel = projected
            .into_with_selection(Selection::Indices(vec![0]))
            .unwrap();
        assert_eq!(with_sel.num_rows(), 1);
        assert!(matches!(
            with_sel.into_with_selection(Selection::Indices(vec![5])),
            Err(ColumnError::InvalidBatch { .. })
        ));
    }

    #[test]
    fn arc_sharing_and_deduplicated_memory_bytes_and_slice_reservation() {
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::I64, false),
            Field::new("d", DataType::Utf8, false),
        ])
        .unwrap();

        let c0 = Arc::new(Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![10, 20, 30, 40]), None).unwrap(),
        ));
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["long_alpha", "long_beta"]).unwrap()),
            None,
        )
        .unwrap();
        let c1 = Arc::new(Column::Dict(
            DictColumn::try_new(dict_vals, vec![0, 1, 0, 1], None).unwrap(),
        ));

        let batch = Batch::try_new_shared(
            Arc::clone(&schema),
            vec![Arc::clone(&c0), Arc::clone(&c1)],
            4,
            Selection::All,
        )
        .unwrap();
        assert!(Arc::ptr_eq(batch.column_arc(0).unwrap(), &c0));
        assert!(Arc::ptr_eq(batch.column_arc(1).unwrap(), &c1));

        // with_selection shares Arcs
        let with_sel = batch
            .with_selection(Selection::Indices(vec![0, 2]))
            .unwrap();
        assert!(Arc::ptr_eq(with_sel.column_arc(0).unwrap(), &c0));
        assert!(Arc::ptr_eq(with_sel.column_arc(1).unwrap(), &c1));

        // filter_mask shares Arcs
        let mask = Bitmap::from_bools(&[true, false, true, false]);
        let filtered = batch.filter_mask(&mask).unwrap();
        assert!(Arc::ptr_eq(filtered.column_arc(0).unwrap(), &c0));
        assert!(Arc::ptr_eq(filtered.column_arc(1).unwrap(), &c1));

        // project (with duplicates) shares Arcs and deduplicates memory_bytes
        let proj = batch.project(&[0, 0, 1, 0]).unwrap();
        assert!(Arc::ptr_eq(proj.column_arc(0).unwrap(), &c0));
        assert!(Arc::ptr_eq(proj.column_arc(1).unwrap(), &c0));
        assert!(Arc::ptr_eq(proj.column_arc(2).unwrap(), &c1));
        assert!(Arc::ptr_eq(proj.column_arc(3).unwrap(), &c0));
        assert_eq!(proj.memory_bytes(), c0.memory_bytes() + c1.memory_bytes());

        // into_project (with duplicates) shares Arcs and deduplicates memory_bytes
        let into_proj = batch.into_project(&[1, 1]).unwrap();
        assert!(Arc::ptr_eq(into_proj.column_arc(0).unwrap(), &c1));
        assert!(Arc::ptr_eq(into_proj.column_arc(1).unwrap(), &c1));
        assert_eq!(into_proj.memory_bytes(), c1.memory_bytes());

        // slice with pool reserves full out.memory_bytes() and shares Dict values Arc across columns
        let scope = GlobalPool::standalone(4096);
        let mut res = scope.register("slice", true);
        let sliced = proj.slice(1, 2, Some(&mut res)).unwrap();
        assert!(Arc::ptr_eq(
            sliced.column_arc(0).unwrap(),
            sliced.column_arc(1).unwrap()
        ));
        assert!(Arc::ptr_eq(
            sliced.column_arc(0).unwrap(),
            sliced.column_arc(3).unwrap()
        ));
        let (Column::Dict(orig_dict), Column::Dict(sliced_dict)) = (
            proj.column_arc(2).unwrap().as_ref(),
            sliced.column_arc(2).unwrap().as_ref(),
        ) else {
            panic!("expected Dict columns");
        };
        assert!(Arc::ptr_eq(
            orig_dict.values_arc(),
            sliced_dict.values_arc()
        ));
        assert_eq!(res.size(), sliced.memory_bytes());
        res.shrink(sliced.memory_bytes());
        assert_eq!(res.size(), 0);

        // Tight pool smaller than sliced.memory_bytes() rejects and rolls back cleanly
        let tight = GlobalPool::standalone(sliced.memory_bytes() - 1);
        let mut tight_res = tight.register("tight_slice", true);
        let err = proj.slice(1, 2, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 0);
    }

    #[test]
    fn distinct_dict_columns_sharing_values_arc_deduplicate_memory_bytes_and_slice() {
        let schema = Schema::try_new_arc(vec![
            Field::new("d1", DataType::Utf8, false),
            Field::new("d2", DataType::Utf8, false),
        ])
        .unwrap();

        let shared_vals = Arc::new(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["long_alpha_payload", "long_beta_payload"]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let d1 = Arc::new(Column::Dict(
            DictColumn::try_new_shared(Arc::clone(&shared_vals), vec![0, 1, 0, 1], None).unwrap(),
        ));
        let d2 = Arc::new(Column::Dict(
            DictColumn::try_new_shared(Arc::clone(&shared_vals), vec![1, 0, 1, 0], None).unwrap(),
        ));
        assert!(!Arc::ptr_eq(&d1, &d2));

        let batch = Batch::try_new_shared(
            Arc::clone(&schema),
            vec![Arc::clone(&d1), Arc::clone(&d2)],
            4,
            Selection::Indices(vec![1, 2, 3]),
        )
        .unwrap();
        // shared_vals counted once + 2 * (4 * 4) codes + 3 * 4 selection indices
        let expected_batch_bytes = shared_vals.memory_bytes() + 16 + 16 + 12;
        assert_eq!(batch.memory_bytes(), expected_batch_bytes);

        // Exact tight pool for slice succeeds without double-counting shared_vals
        let expected_slice_bytes = shared_vals.memory_bytes() + 8 + 8 + 8;
        let scope = GlobalPool::standalone(expected_slice_bytes);
        let mut res = scope.register("distinct_dict_slice", true);
        let sliced = batch.slice(1, 2, Some(&mut res)).unwrap();
        assert_eq!(sliced.memory_bytes(), expected_slice_bytes);
        assert_eq!(res.size(), sliced.memory_bytes());

        // 1 byte tighter fails cleanly
        let too_tight = GlobalPool::standalone(expected_slice_bytes - 1);
        let mut tight_res = too_tight.register("too_tight", true);
        let err = batch.slice(1, 2, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(tight_res.size(), 0);
    }
}
