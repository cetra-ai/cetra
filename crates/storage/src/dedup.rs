//! Collapsing duplicate rows before they are written to a file.
//!
//! Flush and compaction share these: a flush deduplicates the rows of one
//! frozen partition, a merge deduplicates each chunk of merged rows (spec 03
//! §2, §5).

use std::collections::HashMap;

use cetra_column::{Batch, Column, DataType, FlatData, ScalarValue, Selection};

use crate::error::DedupError;

/// Latest version of each span on a [`Batch`]: greatest `ingest_time` per
/// `(trace_id, span_id)`. An equal `ingest_time` keeps the later row. Kept rows
/// keep their input order. Handles all 4 [`Column`] encodings and any [`Selection`].
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

    use cetra_column::{
        Batch, Bitmap, Column, DictColumn, Field, FlatColumn, FlatData, RleColumn, ScalarValue,
        Schema, Selection, Utf8Data,
    };
    use cetra_core::schema::{scores, spans};

    use super::{latest_scores_batch, latest_spans_batch};
    use crate::wal::spans_keyed;

    fn names(batch: &Batch) -> Vec<String> {
        let column = batch.column_by_name(spans::NAME).unwrap();
        (0..batch.num_rows())
            .map(|row| match column.get_scalar(row).unwrap() {
                ScalarValue::Utf8(value) => value,
                other => panic!("expected utf8 name, got {other:?}"),
            })
            .collect()
    }

    fn starts(batch: &Batch) -> Vec<i64> {
        let column = batch.column_by_name(spans::START_TIME).unwrap();
        (0..batch.num_rows())
            .map(|row| match column.get_scalar(row).unwrap() {
                ScalarValue::TimestampNs { value, .. } => value,
                other => panic!("expected timestamp, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_greatest_ingest_time_wins_in_either_order() {
        let older = spans_keyed(&[10, 11], "older", 1);
        let newer = spans_keyed(&[50, 51], "newer", 1);
        let either = |first: &Batch, second: &Batch| {
            let batch = concat_col(first, second);
            names(&latest_spans_batch(&batch).unwrap())
        };
        assert_eq!(either(&older, &newer), vec!["newer".to_owned(); 2]);
        assert_eq!(either(&newer, &older), vec!["newer".to_owned(); 2]);
    }

    #[test]
    fn an_equal_ingest_time_keeps_the_later_row() {
        let first = spans_keyed(&[10, 11], "first", 1);
        let second = spans_keyed(&[10, 11], "second", 1);
        let batch = concat_col(&first, &second);
        assert_eq!(
            names(&latest_spans_batch(&batch).unwrap()),
            vec!["second".to_owned(); 2]
        );
    }

    #[test]
    fn kept_rows_stay_in_input_order() {
        let older = spans_keyed(&[50, 40], "a", 1);
        let newer = spans_keyed(&[45], "b", 2);
        let batch = concat_col(&older, &newer);
        let kept = latest_spans_batch(&batch).unwrap();
        // `45` (id 2, later) beats `40`, and the survivors keep their input
        // order, not start-time order.
        assert_eq!(starts(&kept), vec![50, 45]);
    }

    #[test]
    fn a_greater_updated_at_wins_and_ties_break_by_ingest_time() {
        let older = score_rows(&[(1, 1, 10, "older"), (2, 5, 50, "t")]);
        let newer = score_rows(&[(1, 1, 20, "ingest-wins"), (2, 6, 60, "t-newer")]);
        let combined = concat_col(&older, &newer);
        let kept = latest_scores_batch(&combined, false).unwrap();
        assert_eq!(
            names(&kept),
            vec!["ingest-wins".to_owned(), "t-newer".to_owned()]
        );

        // A greater `updated_at` beats a greater `ingest_time`.
        let earlier = score_rows(&[(1, 1, 90, "ingest-higher")]);
        let greatest = score_rows(&[(1, 3, 10, "updated-wins")]);
        let combined = concat_col(&earlier, &greatest);
        let kept = latest_scores_batch(&combined, false).unwrap();
        assert_eq!(names(&kept), vec!["updated-wins".to_owned()]);
    }

    #[test]
    fn a_full_tie_keeps_the_later_row() {
        let first = score_rows(&[(1, 1, 10, "first")]);
        let second = score_rows(&[(1, 1, 10, "second")]);
        let combined = concat_col(&first, &second);
        let kept = latest_scores_batch(&combined, false).unwrap();
        assert_eq!(names(&kept), vec!["second".to_owned()]);
    }

    #[test]
    fn a_winning_deleted_version_stays_or_goes_with_drop_deleted() {
        let older = score_rows(&[(1, 1, 10, "live-old")]);
        let deleted = score_rows(&[(1, 3, 30, "deleted-new")]);
        let combined = concat_col(&older, &deleted);

        let kept = latest_scores_batch(&combined, false).unwrap();
        assert_eq!(names(&kept), vec!["deleted-new".to_owned()]);

        // The older version already lost, so the score disappears entirely.
        let dropped = latest_scores_batch(&combined, true).unwrap();
        assert_eq!(dropped.num_rows(), 0);
    }

    /// `[(score_id, updated_at, ingest_time, name)]` as one scores batch.
    fn score_rows(rows: &[(u128, i64, i64, &str)]) -> Batch {
        use cetra_core::batch::scores_to_batch;
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
        scores_to_batch(&scores).unwrap()
    }

    fn concat_col(first: &Batch, second: &Batch) -> Batch {
        Batch::concat(Arc::clone(first.schema()), &[first, second], None).unwrap()
    }

    #[test]
    fn a_null_span_id_is_an_error() {
        let source = spans_keyed(&[10], "row", 1);
        let fields: Vec<Field> = source
            .schema()
            .fields()
            .iter()
            .map(|field| {
                if field.name == spans::SPAN_ID {
                    Field::new(field.name.clone(), field.data_type.clone(), true)
                } else {
                    field.clone()
                }
            })
            .collect();
        let schema = Arc::new(Schema::try_new(fields).unwrap());
        let mut columns = source.columns().to_vec();
        let index = schema.index_of(spans::SPAN_ID).unwrap();
        columns[index] = Arc::new(Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes8(vec![[0; 8]]),
                Some(Bitmap::from_bools(&[false])),
            )
            .unwrap(),
        ));
        let batch = Batch::try_new_shared(schema, columns, 1, Selection::All).unwrap();
        let error = latest_spans_batch(&batch).unwrap_err();
        assert_eq!(error.reason(), "null span identity");
    }

    #[test]
    fn latest_spans_batch_handles_all_encodings_and_selections() {
        let older = spans_keyed(&[50, 40], "a", 1);
        let newer = spans_keyed(&[45], "b", 2);
        let batch = concat_col(&older, &newer);

        // Test Const, Dict, Rle encodings and Selection::Indices / Selection::Mask.
        let schema = Arc::clone(batch.schema());
        let mut cols = batch.columns().to_vec();
        let trace_idx = schema.index_of(spans::TRACE_ID).unwrap();
        let span_idx = schema.index_of(spans::SPAN_ID).unwrap();
        let ingest_idx = schema.index_of(spans::INGEST_TIME).unwrap();

        let tid = [0x11_u8; 16];
        cols[trace_idx] =
            Arc::new(Column::try_new_const(ScalarValue::FixedBytes16(tid), 3).unwrap());
        let span_dict_vals = FlatColumn::try_new(
            FlatData::FixedBytes8(vec![[0, 0, 0, 0, 0, 0, 0, 1], [0, 0, 0, 0, 0, 0, 0, 2]]),
            None,
        )
        .unwrap();
        cols[span_idx] = Arc::new(Column::Dict(
            DictColumn::try_new(span_dict_vals, vec![0, 1, 0], None).unwrap(),
        ));
        let ingest_rle_vals = FlatColumn::try_new(
            FlatData::TimestampNs {
                values: vec![10, 20],
                utc: true,
            },
            None,
        )
        .unwrap();
        cols[ingest_idx] = Arc::new(Column::Rle(
            RleColumn::try_new(ingest_rle_vals, vec![1, 3]).unwrap(),
        ));

        // Selection::Indices([2, 1, 0]): row 0 and row 2 share span_id 1; row 0 (ingest=10) loses to row 2 (ingest=20),
        // and even if tied, row 0 comes AFTER row 2 in input order [2, 1, 0]!
        // Here row 2 has ingest=20 and row 0 has ingest=10, so row 2 wins and output order is [2, 1].
        let enc_batch = Batch::try_new_shared(
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
        let masked = Batch::try_new_shared(
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
    fn latest_scores_batch_handles_all_encodings_and_selections() {
        let older = score_rows(&[(1, 1, 10, "live-old"), (2, 5, 50, "t")]);
        let newer = score_rows(&[(1, 3, 30, "deleted-new"), (2, 5, 50, "t-tie")]);
        let col_batch = concat_col(&older, &newer);

        // Const + Dict + Rle + Selection::Mask
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
        cols[id_idx] = Arc::new(Column::Dict(
            DictColumn::try_new(id_vals, vec![0, 1, 0, 1], None).unwrap(),
        ));
        cols[upd_idx] = Arc::new(Column::Rle(
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
        ));
        cols[ing_idx] = Arc::new(
            Column::try_new_const(
                ScalarValue::TimestampNs {
                    value: 100,
                    utc: true,
                },
                4,
            )
            .unwrap(),
        );
        cols[del_idx] = Arc::new(Column::try_new_const(ScalarValue::Bool(true), 4).unwrap());

        let enc = Batch::try_new_shared(
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
