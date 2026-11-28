//! Two UI queries without DataFusion: every span of one trace (`trace_detail`), and the latest
//! spans of a time window (`trace_list`), both over Strata files that share one schema (design
//! S9).
//!
//! Both return the table schema: the file schema cast as `StrataTable` casts it.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, FixedSizeBinaryArray, RecordBatch, RecordBatchOptions,
    TimestampNanosecondArray,
};
use arrow::compute::{concat_batches, interleave};
use arrow::datatypes::{DataType, SchemaRef, TimeUnit};
use futures::future::try_join_all;

use crate::datafusion::{table_schema, to_table_batch};
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::io::ScanKind;
use crate::narrow::{BlockEntry, flags};
use crate::predicate::{CmpOp, Lit, Predicate, zone_i64};
use crate::scan::ScanRequest;

/// Every span of one trace across `files`, with the named columns, in the table schema.
pub async fn trace_detail(
    files: &[Arc<StrataFile>],
    trace_id: [u8; 16],
    projection: &[&str],
) -> Result<RecordBatch> {
    let schema = shared_schema(files)?;
    let columns = indexes(&schema, projection)?;
    let file_schema: SchemaRef = Arc::new(schema.project(&columns)?);
    let per_file = try_join_all(
        files
            .iter()
            .map(|file| file.lookup_scan(&trace_id, &columns, false)),
    )
    .await?;
    let batches: Vec<RecordBatch> = per_file.into_iter().flatten().collect();
    let concatenated = concat_batches(&file_schema, &batches)?;
    to_table_batch(&concatenated, &table_schema(&file_schema))
}

/// The `k` latest spans with `start_time` in `time` and matching `predicate`, ordered by
/// (start_time DESC, trace_id DESC, span_id DESC), with the named columns, in the table schema.
///
/// Each visited chunk costs at least one wave. The early stop only helps when `start_time`
/// zones are narrow, that is, when the file is sorted by time.
pub async fn trace_list(
    files: &[Arc<StrataFile>],
    time: Range<i64>,
    predicate: Option<Predicate>,
    k: usize,
    projection: &[&str],
) -> Result<RecordBatch> {
    let schema = shared_schema(files)?;
    let st = index_of(&schema, "start_time")?;
    let tid = index_of(&schema, "trace_id")?;
    let sid = index_of(&schema, "span_id")?;
    if !matches!(
        schema.field(st).data_type(),
        DataType::Timestamp(TimeUnit::Nanosecond, _)
    ) {
        return Err(StrataError::Schema {
            reason: "start_time is not a nanosecond timestamp".to_string(),
        });
    }
    let requested = indexes(&schema, projection)?;
    let file_schema: SchemaRef = Arc::new(schema.project(&requested)?);
    let out = table_schema(&file_schema);
    if k == 0 {
        return Ok(RecordBatch::new_empty(out));
    }

    // The chunks that may hold a row of the window, latest `max` first.
    let mut candidates: Vec<(i64, usize, u32, u32)> = Vec::new();
    for (f, file) in files.iter().enumerate() {
        for entry in file.block_entries(st).await?.iter() {
            let (min, max) = time_zone(entry);
            if max >= time.start && min < time.end {
                candidates.push((max, f, entry.first_row, entry.n_rows));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    let window = Predicate::And(vec![
        Predicate::Cmp(st, CmpOp::GtEq, Lit::Int(time.start)),
        Predicate::Cmp(st, CmpOp::Lt, Lit::Int(time.end)),
    ]);
    let predicate = Some(match predicate {
        Some(p) => Predicate::And(vec![window, p]),
        None => window,
    });
    // The scan reads the three key columns first, then the requested ones.
    let mut scan_projection = vec![st, tid, sid];
    scan_projection.extend(&requested);

    let mut heap: BinaryHeap<Reverse<ListKey>> = BinaryHeap::new();
    let mut kept: Vec<RecordBatch> = Vec::new();
    for &(max, f, first_row, n_rows) in &candidates {
        if heap.len() >= k
            && let Some(Reverse(worst)) = heap.peek()
            && max < worst.0
        {
            break;
        }
        let file = files.get(f).ok_or(StrataError::Corrupt {
            what: "trace_list file",
            offset: 0,
        })?;
        let end = first_row.checked_add(n_rows).ok_or(StrataError::Corrupt {
            what: "trace_list chunk",
            offset: 0,
        })?;
        let batches = file
            .scan(&ScanRequest {
                projection: scan_projection.clone(),
                predicate: predicate.clone(),
                // One range: the chunk's rows. `once` avoids clippy's single-range `vec!` lint.
                row_ranges: Some(std::iter::once(first_row..end).collect()),
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
                row_filter: None,
            })
            .await?;
        for batch in batches {
            let batch_index = kept.len();
            let starts = key_array::<TimestampNanosecondArray>(&batch, 0)?;
            let trace_ids = key_array::<FixedSizeBinaryArray>(&batch, 1)?;
            let span_ids = key_array::<FixedSizeBinaryArray>(&batch, 2)?;
            for row in 0..batch.num_rows() {
                let key: ListKey = (
                    starts.value(row),
                    fixed_bytes(trace_ids, row)?,
                    fixed_bytes(span_ids, row)?,
                    batch_index,
                    row,
                );
                heap.push(Reverse(key));
                if heap.len() > k {
                    heap.pop();
                }
            }
            kept.push(batch);
        }
        // The kept batches hold every row of the window seen so far. Compact them once they hold
        // much more than the heap needs, so the memory stays proportional to `k`.
        let held: usize = kept.iter().map(RecordBatch::num_rows).sum();
        if held > k.saturating_mul(2).saturating_add(8_192) {
            compact(&mut kept, &mut heap)?;
        }
    }

    let mut rows: Vec<ListKey> = heap.into_iter().map(|Reverse(key)| key).collect();
    if rows.is_empty() {
        return Ok(RecordBatch::new_empty(out));
    }
    rows.sort_unstable_by(|a, b| b.cmp(a));
    let indices: Vec<(usize, usize)> = rows.iter().map(|key| (key.3, key.4)).collect();
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(requested.len());
    for j in 0..requested.len() {
        let arrays: Vec<&dyn Array> = kept
            .iter()
            .map(|batch| {
                batch
                    .columns()
                    .get(3 + j)
                    .map(|column| column.as_ref())
                    .ok_or(StrataError::Corrupt {
                        what: "trace_list columns",
                        offset: 0,
                    })
            })
            .collect::<Result<_>>()?;
        columns.push(interleave(&arrays, &indices)?);
    }
    // With no column requested, the row count alone sizes the batch.
    let batch = RecordBatch::try_new_with_options(
        file_schema,
        columns,
        &RecordBatchOptions::new().with_row_count(Some(indices.len())),
    )?;
    to_table_batch(&batch, &out)
}

/// Replaces `kept` with one batch holding the rows that `heap` points at, all columns of the
/// kept batches, and points each heap entry at its row of that batch. The rows keep their
/// (batch, row) order, so ties on the sort key resolve as they would without compaction.
fn compact(kept: &mut Vec<RecordBatch>, heap: &mut BinaryHeap<Reverse<ListKey>>) -> Result<()> {
    let Some(first) = kept.first() else {
        return Ok(());
    };
    let schema = first.schema();
    let mut entries: Vec<ListKey> = std::mem::take(heap)
        .into_iter()
        .map(|Reverse(key)| key)
        .collect();
    entries.sort_unstable_by_key(|key| (key.3, key.4));
    let indices: Vec<(usize, usize)> = entries.iter().map(|key| (key.3, key.4)).collect();
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
    for position in 0..schema.fields().len() {
        let arrays: Vec<&dyn Array> = kept
            .iter()
            .map(|batch| {
                batch
                    .columns()
                    .get(position)
                    .map(|column| column.as_ref())
                    .ok_or(StrataError::Corrupt {
                        what: "trace_list compaction",
                        offset: 0,
                    })
            })
            .collect::<Result<_>>()?;
        columns.push(interleave(&arrays, &indices)?);
    }
    let batch = RecordBatch::try_new_with_options(
        schema,
        columns,
        &RecordBatchOptions::new().with_row_count(Some(entries.len())),
    )?;
    *kept = vec![batch];
    heap.extend(
        entries
            .into_iter()
            .enumerate()
            .map(|(row, key)| Reverse((key.0, key.1, key.2, 0, row))),
    );
    Ok(())
}

/// The sort key of a `trace_list` row, then its (batch, row) in `kept`.
type ListKey = (i64, [u8; 16], [u8; 8], usize, usize);

/// The schema shared by `files`. `Schema` when there is no file or the files differ.
fn shared_schema(files: &[Arc<StrataFile>]) -> Result<SchemaRef> {
    let first = files.first().ok_or_else(|| StrataError::Schema {
        reason: "no file".to_string(),
    })?;
    let schema = first.schema();
    for file in files {
        if *file.schema() != *schema {
            return Err(StrataError::Schema {
                reason: "files have different schemas".to_string(),
            });
        }
    }
    Ok(schema)
}

/// The index of column `name`, or `Schema` when there is none.
fn index_of(schema: &SchemaRef, name: &str) -> Result<usize> {
    schema.index_of(name).map_err(|_| StrataError::Schema {
        reason: format!("no column {name}"),
    })
}

/// The indexes of the named columns, in order.
fn indexes(schema: &SchemaRef, names: &[&str]) -> Result<Vec<usize>> {
    names.iter().map(|name| index_of(schema, name)).collect()
}

/// The zone of a block entry as (min, max). A missing bound, or a chunk without a zone, is
/// unbounded.
fn time_zone(e: &BlockEntry) -> (i64, i64) {
    if e.flags & flags::HAS_ZONE != 0 {
        (
            zone_i64(&e.min).unwrap_or(i64::MIN),
            zone_i64(&e.max).unwrap_or(i64::MAX),
        )
    } else {
        (i64::MIN, i64::MAX)
    }
}

/// Output column `position` of a scan batch, downcast to `T`.
fn key_array<T: 'static>(batch: &RecordBatch, position: usize) -> Result<&T> {
    batch
        .columns()
        .get(position)
        .and_then(|column| column.as_any().downcast_ref::<T>())
        .ok_or(StrataError::Corrupt {
            what: "trace_list key",
            offset: 0,
        })
}

/// The `N` bytes of row `row`, or `Corrupt` when the key has another width.
fn fixed_bytes<const N: usize>(array: &FixedSizeBinaryArray, row: usize) -> Result<[u8; N]> {
    array
        .value(row)
        .try_into()
        .map_err(|_| StrataError::Corrupt {
            what: "trace_list key",
            offset: 0,
        })
}
