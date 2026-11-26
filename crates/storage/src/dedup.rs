//! Collapsing duplicate rows before they are written to a file.
//!
//! Flush and compaction share these: a flush deduplicates the rows of one
//! frozen partition, a merge deduplicates each chunk of merged rows (spec 03
//! §2, §5).

use std::collections::HashMap;

use arrow::array::{Array, AsArray, RecordBatch, UInt32Array};
use arrow::compute::take_record_batch;
use arrow::datatypes::TimestampNanosecondType;
use cetra_column::{Batch, Column, DataType, FlatData, ScalarValue, Selection};

use crate::error::DedupError;

/// Latest version of each span: the greatest `ingest_time` per `(trace_id, span_id)`.
/// An equal `ingest_time` keeps the later row. Kept rows keep their input order.
pub(crate) fn latest_spans(batch: &RecordBatch) -> Result<RecordBatch, DedupError> {
    let trace = fixed(batch, cetra_core::schema::spans::TRACE_ID)?;
    let span = fixed(batch, cetra_core::schema::spans::SPAN_ID)?;
    let ingest = batch
        .column_by_name(cetra_core::schema::spans::INGEST_TIME)
        .and_then(|column| column.as_primitive_opt::<TimestampNanosecondType>())
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let mut best: HashMap<(Vec<u8>, Vec<u8>), (i64, usize)> = HashMap::new();
    for row in 0..batch.num_rows() {
        if trace.is_null(row) || span.is_null(row) || ingest.is_null(row) {
            return Err(DedupError::new("null span identity"));
        }
        let key = (trace.value(row).to_vec(), span.value(row).to_vec());
        let time = ingest.value(row);
        match best.get(&key) {
            Some((kept, _)) if *kept > time => {}
            _ => {
                best.insert(key, (time, row));
            }
        }
    }
    let mut rows = Vec::with_capacity(best.len());
    for (_, row) in best.into_values() {
        rows.push(u32::try_from(row).map_err(|_| DedupError::new("row index"))?);
    }
    rows.sort_unstable();
    take_record_batch(batch, &UInt32Array::from(rows)).map_err(|source| {
        // The error text can name columns, never values.
        tracing::warn!(%source, "taking deduplicated rows failed");
        DedupError::new("arrow")
    })
}

fn fixed<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a arrow::array::FixedSizeBinaryArray, DedupError> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_fixed_size_binary_opt())
        .ok_or_else(|| DedupError::new("missing identity column"))
}

/// Latest version of each score: the greatest `(updated_at, ingest_time)` per
/// `score_id`. A full tie keeps the later row. With `drop_deleted`, a kept row
/// with `deleted = true` is then removed. Kept rows keep their input order.
pub(crate) fn latest_scores(
    batch: &RecordBatch,
    drop_deleted: bool,
) -> Result<RecordBatch, DedupError> {
    let score_id = batch
        .column_by_name(cetra_core::schema::scores::SCORE_ID)
        .and_then(|column| column.as_string_opt::<i32>())
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let updated = i64_column(batch, cetra_core::schema::scores::UPDATED_AT)?;
    let ingest = i64_column(batch, cetra_core::schema::scores::INGEST_TIME)?;
    let deleted = batch
        .column_by_name(cetra_core::schema::scores::DELETED)
        .and_then(|column| column.as_boolean_opt())
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let mut best: HashMap<&str, (i64, i64, usize)> = HashMap::new();
    for row in 0..batch.num_rows() {
        if score_id.is_null(row)
            || updated.is_null(row)
            || ingest.is_null(row)
            || deleted.is_null(row)
        {
            return Err(DedupError::new("null score identity"));
        }
        let time = (updated.value(row), ingest.value(row));
        match best.get(score_id.value(row)) {
            Some((kept_updated, kept_ingest, _)) if (*kept_updated, *kept_ingest) > time => {}
            _ => {
                best.insert(score_id.value(row), (time.0, time.1, row));
            }
        }
    }
    let mut rows = Vec::new();
    for (_, _, row) in best.values() {
        if drop_deleted && deleted.value(*row) {
            continue;
        }
        rows.push(u32::try_from(*row).map_err(|_| DedupError::new("row index"))?);
    }
    rows.sort_unstable();
    take_record_batch(batch, &UInt32Array::from(rows)).map_err(|source| {
        // The error text can name columns, never values.
        tracing::warn!(%source, "taking deduplicated rows failed");
        DedupError::new("arrow")
    })
}

fn i64_column<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a arrow::array::TimestampNanosecondArray, DedupError> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_primitive_opt::<TimestampNanosecondType>())
        .ok_or_else(|| DedupError::new("missing identity column"))
}

/// Latest version of each span on a [`Batch`]: greatest `ingest_time` per
/// `(trace_id, span_id)`. An equal `ingest_time` keeps the later row. Kept rows
/// keep their input order. Handles all 4 [`Column`] encodings and any [`Selection`].
#[allow(dead_code)]
pub(crate) fn latest_spans_batch(batch: &Batch) -> Result<Batch, DedupError> {
    struct BestSpan {
        ingest: i64,
        pos: usize,
        phys: u32,
    }

    let trace = batch
        .column_by_name(cetra_core::schema::spans::TRACE_ID)
        .filter(|col| col.data_type() == DataType::FixedBytes16)
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let span = batch
        .column_by_name(cetra_core::schema::spans::SPAN_ID)
        .filter(|col| col.data_type() == DataType::FixedBytes8)
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let ingest = batch
        .column_by_name(cetra_core::schema::spans::INGEST_TIME)
        .filter(|col| matches!(col.data_type(), DataType::TimestampNs { .. }))
        .ok_or_else(|| DedupError::new("missing identity column"))?;

    let mut best: HashMap<([u8; 16], [u8; 8]), BestSpan> = HashMap::new();
    for_each_active(batch, |pos, phys| {
        let row = phys as usize;
        let Some(trace_bytes) = read_fixed16(trace, row) else {
            return Err(DedupError::new("null span identity"));
        };
        let Some(span_bytes) = read_fixed8(span, row) else {
            return Err(DedupError::new("null span identity"));
        };
        let Some(time) = read_timestamp(ingest, row) else {
            return Err(DedupError::new("null span identity"));
        };
        let key = (trace_bytes, span_bytes);
        match best.get(&key) {
            Some(kept) if kept.ingest > time => {}
            _ => {
                best.insert(
                    key,
                    BestSpan {
                        ingest: time,
                        pos,
                        phys,
                    },
                );
            }
        }
        Ok(())
    })?;

    let mut kept: Vec<(usize, u32)> = best.into_values().map(|b| (b.pos, b.phys)).collect();
    kept.sort_unstable_by_key(|&(pos, _)| pos);
    let phys_rows: Vec<u32> = kept.into_iter().map(|(_, phys)| phys).collect();
    batch
        .with_selection(Selection::Indices(phys_rows))
        .and_then(|selected| selected.compact(None))
        .map_err(|_| DedupError::new("column"))
}

/// Latest version of each score on a [`Batch`]: greatest `(updated_at, ingest_time)`
/// per `score_id`. A full tie keeps the later row. With `drop_deleted`, a kept
/// row with `deleted = true` is then removed. Kept rows keep their input order.
/// Handles all 4 [`Column`] encodings and any [`Selection`].
#[allow(dead_code)]
pub(crate) fn latest_scores_batch(batch: &Batch, drop_deleted: bool) -> Result<Batch, DedupError> {
    let score_id = batch
        .column_by_name(cetra_core::schema::scores::SCORE_ID)
        .filter(|col| col.data_type() == DataType::Utf8)
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let updated = batch
        .column_by_name(cetra_core::schema::scores::UPDATED_AT)
        .filter(|col| matches!(col.data_type(), DataType::TimestampNs { .. }))
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let ingest = batch
        .column_by_name(cetra_core::schema::scores::INGEST_TIME)
        .filter(|col| matches!(col.data_type(), DataType::TimestampNs { .. }))
        .ok_or_else(|| DedupError::new("missing identity column"))?;
    let deleted = batch
        .column_by_name(cetra_core::schema::scores::DELETED)
        .filter(|col| col.data_type() == DataType::Bool)
        .ok_or_else(|| DedupError::new("missing identity column"))?;

    let mut best: HashMap<&str, (i64, i64, usize, u32, bool)> = HashMap::new();
    for_each_active(batch, |pos, phys| {
        let row = phys as usize;
        let Some(id) = read_str(score_id, row) else {
            return Err(DedupError::new("null score identity"));
        };
        let Some(upd) = read_timestamp(updated, row) else {
            return Err(DedupError::new("null score identity"));
        };
        let Some(ing) = read_timestamp(ingest, row) else {
            return Err(DedupError::new("null score identity"));
        };
        let Some(del) = read_bool(deleted, row) else {
            return Err(DedupError::new("null score identity"));
        };
        let time = (upd, ing);
        match best.get(id) {
            Some((kept_u, kept_i, _, _, _)) if (*kept_u, *kept_i) > time => {}
            _ => {
                best.insert(id, (upd, ing, pos, phys, del));
            }
        }
        Ok(())
    })?;

    let mut kept: Vec<(usize, u32)> = best
        .into_values()
        .filter(|&(_, _, _, _, is_deleted)| !(drop_deleted && is_deleted))
        .map(|(_, _, pos, phys, _)| (pos, phys))
        .collect();
    kept.sort_unstable_by_key(|&(pos, _)| pos);
    let phys_rows: Vec<u32> = kept.into_iter().map(|(_, phys)| phys).collect();
    batch
        .with_selection(Selection::Indices(phys_rows))
        .and_then(|selected| selected.compact(None))
        .map_err(|_| DedupError::new("column"))
}

#[allow(dead_code)]
fn for_each_active(
    batch: &Batch,
    mut f: impl FnMut(usize, u32) -> Result<(), DedupError>,
) -> Result<(), DedupError> {
    let raw_len = batch.raw_len();
    match batch.selection() {
        Selection::All => {
            for row in 0..raw_len {
                let phys = u32::try_from(row).map_err(|_| DedupError::new("row index"))?;
                f(row, phys)?;
            }
        }
        Selection::Mask(mask) => {
            for (pos, row) in mask.iter_ones().enumerate() {
                let phys = u32::try_from(row).map_err(|_| DedupError::new("row index"))?;
                f(pos, phys)?;
            }
        }
        Selection::Indices(indices) => {
            for (pos, &phys) in indices.iter().enumerate() {
                f(pos, phys)?;
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn resolve_flat_row(col: &Column, row: usize) -> Option<(&cetra_column::FlatColumn, usize)> {
    match col {
        Column::Flat(flat) => {
            if flat.is_null(row) {
                None
            } else {
                Some((flat, row))
            }
        }
        Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                None
            } else {
                Some((dict.values(), code))
            }
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                None
            } else {
                Some((rle.values(), run))
            }
        }
        Column::Const { .. } => None,
    }
}

#[allow(dead_code)]
fn read_fixed16(col: &Column, row: usize) -> Option<[u8; 16]> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::FixedBytes16(bytes) => Some(*bytes),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::FixedBytes16(values) => values.get(idx).copied(),
        _ => None,
    }
}

#[allow(dead_code)]
fn read_fixed8(col: &Column, row: usize) -> Option<[u8; 8]> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::FixedBytes8(bytes) => Some(*bytes),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::FixedBytes8(values) => values.get(idx).copied(),
        _ => None,
    }
}

#[allow(dead_code)]
fn read_timestamp(col: &Column, row: usize) -> Option<i64> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::TimestampNs { values, .. } => values.get(idx).copied(),
        _ => None,
    }
}

#[allow(dead_code)]
fn read_str(col: &Column, row: usize) -> Option<&str> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::Utf8(text) => Some(text.as_str()),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::Utf8(utf8) => utf8.get(idx),
        _ => None,
    }
}

#[allow(dead_code)]
fn read_bool(col: &Column, row: usize) -> Option<bool> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::Bool(b) => Some(*b),
            _ => None,
        });
    }
    let (flat, idx) = resolve_flat_row(col, row)?;
    match flat.data() {
        FlatData::Bool(bitmap) => (idx < bitmap.len()).then(|| bitmap.get(idx)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{Array, AsArray, FixedSizeBinaryBuilder, RecordBatch};
    use cetra_core::schema::{spans, spans_schema};

    use super::{latest_scores, latest_scores_batch, latest_spans, latest_spans_batch};
    use crate::wal::spans_keyed;

    fn names(batch: &RecordBatch) -> Vec<String> {
        let column = batch
            .column_by_name(spans::NAME)
            .unwrap()
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap();
        (0..column.len())
            .map(|index| column.value(index).to_owned())
            .collect()
    }

    fn starts(batch: &RecordBatch) -> Vec<i64> {
        let column = batch
            .column_by_name(spans::START_TIME)
            .unwrap()
            .as_primitive_opt::<arrow::datatypes::TimestampNanosecondType>()
            .unwrap();
        (0..column.len()).map(|index| column.value(index)).collect()
    }

    #[test]
    fn the_greatest_ingest_time_wins_in_either_order() {
        let older = spans_keyed(&[10, 11], "older", 1);
        let newer = spans_keyed(&[50, 51], "newer", 1);
        let either = |first: &RecordBatch, second: &RecordBatch| {
            let schema = first.schema();
            let combined = arrow::compute::concat_batches(&schema, [first, second]).unwrap();
            names(&latest_spans(&combined).unwrap())
        };
        assert_eq!(either(&older, &newer), vec!["newer".to_owned(); 2]);
        assert_eq!(either(&newer, &older), vec!["newer".to_owned(); 2]);
    }

    #[test]
    fn an_equal_ingest_time_keeps_the_later_row() {
        let first = spans_keyed(&[10, 11], "first", 1);
        let second = spans_keyed(&[10, 11], "second", 1);
        let schema = first.schema();
        let combined = arrow::compute::concat_batches(&schema, [&first, &second]).unwrap();
        assert_eq!(
            names(&latest_spans(&combined).unwrap()),
            vec!["second".to_owned(); 2]
        );
    }

    #[test]
    fn kept_rows_stay_in_input_order() {
        let older = spans_keyed(&[50, 40], "a", 1);
        let newer = spans_keyed(&[45], "b", 2);
        let schema = older.schema();
        let combined = arrow::compute::concat_batches(&schema, [&older, &newer]).unwrap();
        let kept = latest_spans(&combined).unwrap();
        // `45` (id 2, later) beats `40`, and the survivors keep their input
        // order, not start-time order.
        assert_eq!(starts(&kept), vec![50, 45]);
    }

    #[test]
    fn a_greater_updated_at_wins_and_ties_break_by_ingest_time() {
        let older = score_rows(&[(1, 1, 10, "older"), (2, 5, 50, "t")]);
        let newer = score_rows(&[(1, 1, 20, "ingest-wins"), (2, 6, 60, "t-newer")]);
        let combined = concat(&older, &newer);
        let kept = latest_scores(&combined, false).unwrap();
        assert_eq!(
            names(&kept),
            vec!["ingest-wins".to_owned(), "t-newer".to_owned()]
        );

        // A greater `updated_at` beats a greater `ingest_time`.
        let earlier = score_rows(&[(1, 1, 90, "ingest-higher")]);
        let greatest = score_rows(&[(1, 3, 10, "updated-wins")]);
        let combined = concat(&earlier, &greatest);
        let kept = latest_scores(&combined, false).unwrap();
        assert_eq!(names(&kept), vec!["updated-wins".to_owned()]);
    }

    #[test]
    fn a_full_tie_keeps_the_later_row() {
        let first = score_rows(&[(1, 1, 10, "first")]);
        let second = score_rows(&[(1, 1, 10, "second")]);
        let combined = concat(&first, &second);
        let kept = latest_scores(&combined, false).unwrap();
        assert_eq!(names(&kept), vec!["second".to_owned()]);
    }

    #[test]
    fn a_winning_deleted_version_stays_or_goes_with_drop_deleted() {
        let older = score_rows(&[(1, 1, 10, "live-old")]);
        let deleted = score_rows(&[(1, 3, 30, "deleted-new")]);
        let combined = concat(&older, &deleted);

        let kept = latest_scores(&combined, false).unwrap();
        assert_eq!(names(&kept), vec!["deleted-new".to_owned()]);

        // The older version already lost, so the score disappears entirely.
        let dropped = latest_scores(&combined, true).unwrap();
        assert_eq!(dropped.num_rows(), 0);
    }

    /// `[(score_id, updated_at, ingest_time, name)]` as one scores batch.
    fn score_rows(rows: &[(u128, i64, i64, &str)]) -> RecordBatch {
        use cetra_core::batch::scores_to_record_batch;
        use cetra_core::enums::ScoreSource;
        use cetra_core::score::{Score, ScoreTarget, ScoreValue};

        let scores: Vec<Score> = rows
            .iter()
            .map(|(id, updated, ingest, name)| Score {
                project_id: "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap(),
                score_id: uuid::Uuid::from_u128(*id),
                target: ScoreTarget::Trace {
                    trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".parse().unwrap(),
                },
                session_id: None,
                user_id: None,
                name: (*name).to_owned(),
                value: ScoreValue::Numeric(0.5),
                reason: None,
                source: ScoreSource::Api,
                evaluator_id: None,
                rule_id: None,
                experiment_id: None,
                dataset_item_id: None,
                queue_id: None,
                author_user_id: None,
                metadata: Default::default(),
                created_at: 0,
                updated_at: *updated,
                deleted: name.starts_with("deleted"),
                ingest_time: *ingest,
            })
            .collect();
        scores_to_record_batch(&scores).unwrap()
    }

    fn concat(first: &RecordBatch, second: &RecordBatch) -> RecordBatch {
        let schema = first.schema();
        arrow::compute::concat_batches(&schema, [first, second]).unwrap()
    }

    #[test]
    fn a_null_span_id_is_an_error() {
        // input, so `span_id` is declared nullable to build it.
        let fields: Vec<_> = spans_schema()
            .fields()
            .iter()
            .map(|field| {
                if field.name() == spans::SPAN_ID {
                    field.as_ref().clone().with_nullable(true)
                } else {
                    field.as_ref().clone()
                }
            })
            .collect();
        let schema = Arc::new(arrow::datatypes::Schema::new(fields));
        let source = spans_keyed(&[10], "row", 1);
        let mut columns = source.columns().to_vec();
        let index = source.schema().index_of(spans::SPAN_ID).unwrap();
        let mut nulls = FixedSizeBinaryBuilder::with_capacity(1, 8);
        nulls.append_null();
        columns[index] = Arc::new(nulls.finish());
        let batch = RecordBatch::try_new(schema, columns).unwrap();
        let error = latest_spans(&batch).unwrap_err();
        assert_eq!(error.reason(), "null span identity");
    }

    #[test]
    fn latest_spans_batch_matches_arrow_and_handles_all_encodings_and_selections() {
        use cetra_column::{
            Batch, Bitmap, Column, DictColumn, FlatColumn, FlatData, RleColumn, ScalarValue,
            Selection,
        };

        let older = spans_keyed(&[50, 40], "a", 1);
        let newer = spans_keyed(&[45], "b", 2);
        let combined = concat(&older, &newer);
        let expected = latest_spans(&combined).unwrap();
        let batch = crate::column_bridge::record_batch_to_column(&combined).unwrap();
        let actual = latest_spans_batch(&batch).unwrap();
        assert!(
            actual.semantic_eq(&crate::column_bridge::record_batch_to_column(&expected).unwrap())
        );

        // Test Const, Dict, Rle encodings and Selection::Indices / Selection::Mask.
        let schema = Arc::clone(batch.schema());
        let mut cols = batch.columns().to_vec();
        let trace_idx = schema.index_of(spans::TRACE_ID).unwrap();
        let span_idx = schema.index_of(spans::SPAN_ID).unwrap();
        let ingest_idx = schema.index_of(spans::INGEST_TIME).unwrap();

        let tid = [0x11_u8; 16];
        cols[trace_idx] = Column::try_new_const(ScalarValue::FixedBytes16(tid), 3).unwrap();
        let span_dict_vals = FlatColumn::try_new(
            FlatData::FixedBytes8(vec![[0, 0, 0, 0, 0, 0, 0, 1], [0, 0, 0, 0, 0, 0, 0, 2]]),
            None,
        )
        .unwrap();
        cols[span_idx] =
            Column::Dict(DictColumn::try_new(span_dict_vals, vec![0, 1, 0], None).unwrap());
        let ingest_rle_vals = FlatColumn::try_new(
            FlatData::TimestampNs {
                values: vec![10, 20],
                utc: true,
            },
            None,
        )
        .unwrap();
        cols[ingest_idx] = Column::Rle(RleColumn::try_new(ingest_rle_vals, vec![1, 3]).unwrap());

        // Selection::Indices([2, 1, 0]): row 0 and row 2 share span_id 1; row 0 (ingest=10) loses to row 2 (ingest=20),
        // and even if tied, row 0 comes AFTER row 2 in input order [2, 1, 0]!
        // Here row 2 has ingest=20 and row 0 has ingest=10, so row 2 wins and output order is [2, 1].
        let enc_batch = Batch::try_new(
            Arc::clone(&schema),
            cols.clone(),
            3,
            Selection::Indices(vec![2, 1, 0]),
        )
        .unwrap();
        let deduped = latest_spans_batch(&enc_batch).unwrap();
        assert_eq!(deduped.num_rows(), 2);
        assert_eq!(
            deduped
                .column_by_name(spans::START_TIME)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            batch
                .column_by_name(spans::START_TIME)
                .unwrap()
                .get_scalar(2)
                .unwrap()
        );
        assert_eq!(
            deduped
                .column_by_name(spans::START_TIME)
                .unwrap()
                .get_scalar(1)
                .unwrap(),
            batch
                .column_by_name(spans::START_TIME)
                .unwrap()
                .get_scalar(1)
                .unwrap()
        );

        // Selection::Mask([true, false, true]): keeps rows 0 and 2 -> row 2 wins.
        let masked = Batch::try_new(
            schema,
            cols,
            3,
            Selection::Mask(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let masked_deduped = latest_spans_batch(&masked).unwrap();
        assert_eq!(masked_deduped.num_rows(), 1);
        assert_eq!(
            masked_deduped
                .column_by_name(spans::START_TIME)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            batch
                .column_by_name(spans::START_TIME)
                .unwrap()
                .get_scalar(2)
                .unwrap()
        );
    }

    #[test]
    fn latest_scores_batch_matches_arrow_and_handles_all_encodings_and_selections() {
        use cetra_column::{
            Batch, Bitmap, Column, DictColumn, FlatColumn, FlatData, RleColumn, ScalarValue,
            Selection, Utf8Data,
        };
        use cetra_core::schema::scores;

        let older = score_rows(&[(1, 1, 10, "live-old"), (2, 5, 50, "t")]);
        let newer = score_rows(&[(1, 3, 30, "deleted-new"), (2, 5, 50, "t-tie")]);
        let combined = concat(&older, &newer);

        let col_batch = crate::column_bridge::record_batch_to_column(&combined).unwrap();
        for drop_deleted in [false, true] {
            let expected = latest_scores(&combined, drop_deleted).unwrap();
            let expected_col = crate::column_bridge::record_batch_to_column(&expected).unwrap();
            let actual = latest_scores_batch(&col_batch, drop_deleted).unwrap();
            assert!(actual.semantic_eq(&expected_col));
        }

        // Const + Dict + Rle + Selection::Indices
        let schema = Arc::clone(col_batch.schema());
        let mut cols = col_batch.columns().to_vec();
        let id_idx = schema.index_of(scores::SCORE_ID).unwrap();
        let upd_idx = schema.index_of(scores::UPDATED_AT).unwrap();
        let ing_idx = schema.index_of(scores::INGEST_TIME).unwrap();
        let del_idx = schema.index_of(scores::DELETED).unwrap();

        let id_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["s1", "s2"]).unwrap()),
            None,
        )
        .unwrap();
        cols[id_idx] = Column::Dict(DictColumn::try_new(id_vals, vec![0, 1, 0, 1], None).unwrap());
        cols[upd_idx] = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vec![1, 3],
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
                vec![2, 4],
            )
            .unwrap(),
        );
        cols[ing_idx] = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: 100,
                utc: true,
            },
            4,
        )
        .unwrap();
        cols[del_idx] = Column::try_new_const(ScalarValue::Bool(true), 4).unwrap();

        let enc = Batch::try_new(
            schema,
            cols,
            4,
            Selection::Mask(Bitmap::from_bools(&[true, true, true, false])),
        )
        .unwrap();
        assert_eq!(latest_scores_batch(&enc, false).unwrap().num_rows(), 2);
        assert_eq!(latest_scores_batch(&enc, true).unwrap().num_rows(), 0);
    }
}
