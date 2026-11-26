//! SQL console JSON result encoder (`SqlJsonResult` + `encode_json_result`).

use std::fmt;
use std::ops::Range;

use cetra_column::{Batch, Reservation, Schema};
use serde_json::{Map, Value};

use crate::error::QueryError;
use crate::sql::{SqlColumn, columns_from_schema};

use super::size::{
    CappedBuffer, JSON_ARRAY_BYTES, JSON_ROW_SEPARATOR_BYTES, batch_row_sizes_prefix, exceeds,
};
use super::{
    BufferedSink, SQL_MAX_ROWS, encode_json_keys, for_each_active_row, verify_batch_schema,
    write_json_row,
};

/// JSON console result returned by [`encode_json_result`].
///
/// Implements a manual privacy-safe [`fmt::Debug`] that logs only column count, row count,
/// and `truncated` (never column names or row payloads).
pub struct SqlJsonResult {
    /// Output columns in schema order.
    pub columns: Vec<SqlColumn>,
    /// Decoded row objects, with keys ordered by [`Self::columns`].
    pub rows: Vec<Map<String, Value>>,
    /// True when more rows matched than `rows` holds (due to `limit`, `max_bytes`, or
    /// `already_truncated`).
    pub truncated: bool,
}

impl fmt::Debug for SqlJsonResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlJsonResult")
            .field("columns", &self.columns.len())
            .field("rows", &self.rows.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// Encodes `batches` into a [`SqlJsonResult`] enforcing `limit` (`1..=SQL_MAX_ROWS`),
/// `max_bytes`, and `Reservation` caps.
///
/// - If `0` rows fit within `max_bytes` (when `batches` has at least one row), returns
///   [`QueryError::TooLarge`].
/// - If only a prefix fits within `limit` or `max_bytes`, encodes that prefix and sets
///   `truncated = true`.
/// - Reserves `encoded.len()` while encoding, then `2 * encoded.len()` extra (`3x` total)
///   while parsing into `Vec<Map<String, Value>>` via `serde_json::from_slice`.
pub fn encode_json_result(
    schema: &Schema,
    batches: &[Batch],
    limit: usize,
    max_bytes: usize,
    already_truncated: bool,
    mut res: Option<&mut Reservation>,
) -> Result<SqlJsonResult, QueryError> {
    if !(1..=SQL_MAX_ROWS).contains(&limit) {
        return Err(QueryError::invalid_request(
            "limit must be between 1 and 10000",
        ));
    }
    for batch in batches {
        verify_batch_schema(schema, batch)?;
    }

    let columns = columns_from_schema(schema);
    let mut est_bytes = JSON_ARRAY_BYTES;
    let mut rows_kept = 0usize;
    let mut truncated = already_truncated;
    let mut selections: Vec<(usize, Range<usize>)> = Vec::new();

    for (batch_idx, batch) in batches.iter().enumerate() {
        let n = batch.num_rows();
        if n == 0 {
            continue;
        }
        let room = limit - rows_kept;
        if room == 0 {
            truncated = true;
            break;
        }
        let take_n = if n > room {
            truncated = true;
            room
        } else {
            n
        };

        let sizes = batch_row_sizes_prefix(batch, take_n)?;
        let mut fits = 0usize;
        for &sz in &sizes {
            let next = est_bytes
                .saturating_add(sz)
                .saturating_add(JSON_ROW_SEPARATOR_BYTES);
            if exceeds(next, max_bytes) {
                break;
            }
            est_bytes = next;
            fits += 1;
        }
        if fits == 0 {
            if rows_kept == 0 {
                return Err(QueryError::TooLarge);
            }
            truncated = true;
            break;
        }
        selections.push((batch_idx, 0..fits));
        rows_kept += fits;
        if fits < take_n || (rows_kept == limit && n > room) {
            truncated = true;
            break;
        }
    }

    if rows_kept == 0 {
        return Ok(SqlJsonResult {
            columns,
            rows: Vec::new(),
            truncated,
        });
    }

    let before = res.as_ref().map_or(0, |r| r.size());
    let encoded_keys = encode_json_keys(schema)?;
    let encoded = {
        let mut sink = CappedBuffer::new(max_bytes, res.as_deref_mut());
        let mut out = BufferedSink::new(&mut sink);
        out.push_byte(b'[')?;
        let mut first_row = true;
        for (batch_idx, range) in selections {
            let batch = &batches[batch_idx];
            for_each_active_row(batch, range, |phys_row| {
                if !first_row {
                    out.push_byte(b',')?;
                }
                first_row = false;
                write_json_row(batch, phys_row, &encoded_keys, &mut out)
            })?;
        }
        out.push_byte(b']')?;
        out.flush()?;
        sink.into_bytes()
    };

    // Keep 3x encoded length reserved while the parsed `serde_json::Map` rows live in memory.
    if let Some(r) = res.as_deref_mut() {
        let extra = encoded.len().saturating_mul(2);
        if r.try_grow(extra).is_err() {
            rollback(r, before);
            return Err(QueryError::TooLarge);
        }
    }

    let rows: Vec<Map<String, Value>> = match serde_json::from_slice(&encoded) {
        Ok(rows) => rows,
        Err(_) => {
            if let Some(r) = res {
                rollback(r, before);
            }
            return Err(QueryError::internal("failed to read encoded JSON rows"));
        }
    };

    Ok(SqlJsonResult {
        columns,
        rows,
        truncated,
    })
}

fn rollback(res: &mut Reservation, before: usize) {
    let cur = res.size();
    if cur > before {
        res.shrink(cur - before);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{
        Bitmap, Column, DataType, Field, FlatColumn, FlatData, GlobalPool, Schema, Selection,
        Utf8Data,
    };

    use super::*;
    use crate::encode::SQL_MAX_RESULT_BYTES;

    fn i64_batch(values: Vec<i64>) -> (Arc<Schema>, Batch) {
        let schema = Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let len = values.len();
        let col = Column::Flat(FlatColumn::try_new(FlatData::I64(values), None).unwrap());
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], len).unwrap();
        (schema, batch)
    }

    #[test]
    fn limit_validation_and_empty_batches() {
        let (schema, batch) = i64_batch(vec![]);
        let err0 =
            encode_json_result(&schema, &[], 0, SQL_MAX_RESULT_BYTES, false, None).unwrap_err();
        assert!(matches!(err0, QueryError::InvalidRequest(_)));

        let err_hi = encode_json_result(
            &schema,
            &[],
            SQL_MAX_ROWS + 1,
            SQL_MAX_RESULT_BYTES,
            false,
            None,
        )
        .unwrap_err();
        assert!(matches!(err_hi, QueryError::InvalidRequest(_)));

        let res =
            encode_json_result(&schema, &[batch], 10, SQL_MAX_RESULT_BYTES, true, None).unwrap();
        assert!(res.rows.is_empty());
        assert!(res.truncated);
        assert_eq!(res.columns.len(), 1);
        assert_eq!(res.columns[0].type_name(), "integer");
    }

    #[test]
    fn byte_cap_truncates_partial_or_rejects_first_row() {
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let pad = "a".repeat(100);
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([pad.as_str(), pad.as_str()]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 2).unwrap();

        // 0 rows fit -> TooLarge.
        let err = encode_json_result(&schema, std::slice::from_ref(&batch), 10, 50, false, None)
            .unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));

        // 1 row fits, 2nd exceeds cap -> truncated = true.
        let partial = encode_json_result(&schema, &[batch], 10, 150, false, None).unwrap();
        assert_eq!(partial.rows.len(), 1);
        assert!(partial.truncated);
    }

    #[test]
    fn json_32_mib_cap_truncates_cleanly() {
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let big = "x".repeat(12 * 1024 * 1024);
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices([big.as_str(), big.as_str(), big.as_str()]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 3).unwrap();
        let res =
            encode_json_result(&schema, &[batch], 10, SQL_MAX_RESULT_BYTES, false, None).unwrap();
        assert_eq!(res.rows.len(), 2);
        assert!(res.truncated);
    }

    #[test]
    fn escape_overflow_and_3x_reservation_accounting() {
        // Estimate counts unescaped length (~8 KB < 10 KB), but `\u0001` escapes 6x (~48 KB > 10 KB).
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let ctrl = "\u{1}".repeat(1_000);
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([ctrl.as_str(); 8]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 8).unwrap();
        let err = encode_json_result(&schema, &[batch], 8, 10_000, false, None).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));

        // 3,000 integers encode to ~28 KB, which fits 64 KiB, but 3x reservation (~84 KB) fails 64 KiB pool.
        let (int_schema, int_batch) = i64_batch((0..3_000).collect());
        let global = GlobalPool::new(1 << 20);
        let tight = global.try_admit(0, 64 * 1024).unwrap();
        let mut res_tight = tight.register("json", false);
        let err = encode_json_result(
            &int_schema,
            std::slice::from_ref(&int_batch),
            SQL_MAX_ROWS,
            SQL_MAX_RESULT_BYTES,
            false,
            Some(&mut res_tight),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(res_tight.size(), 0);

        // With plenty of memory, reservation holds 3x encoded length and releases on drop.
        let roomy = global.try_admit(0, 1 << 20).unwrap();
        {
            let mut res_roomy = roomy.register("json", false);
            let out = encode_json_result(
                &int_schema,
                &[int_batch],
                SQL_MAX_ROWS,
                SQL_MAX_RESULT_BYTES,
                false,
                Some(&mut res_roomy),
            )
            .unwrap();
            assert_eq!(out.rows.len(), 3_000);
            assert!(!out.truncated);
            assert!(res_roomy.size() > 64 * 1024);
            let dbg = format!("{out:?}");
            assert_eq!(
                dbg,
                "SqlJsonResult { columns: 1, rows: 3000, truncated: false }"
            );
        }
        assert_eq!(roomy.reserved(), 0);
    }

    #[test]
    fn selection_mask_and_limit_across_multiple_batches() {
        let (schema, b1) = i64_batch(vec![10, 20, 30, 40]);
        let b1 = b1
            .with_selection(Selection::Mask(Bitmap::from_bools(&[
                false, true, false, true,
            ])))
            .unwrap();
        let (_, b2) = i64_batch(vec![50, 60]);
        let out =
            encode_json_result(&schema, &[b1, b2], 3, SQL_MAX_RESULT_BYTES, false, None).unwrap();
        assert!(out.truncated);
        let vals: Vec<i64> = out.rows.iter().map(|m| m["v"].as_i64().unwrap()).collect();
        assert_eq!(vals, vec![20, 40, 50]);
    }
}
