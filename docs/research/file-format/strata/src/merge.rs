//! Merging files (design S10): a k-way merge of the inputs' sort keys, then every column
//! decoded, interleaved and written again by the writer's own code.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, new_empty_array};
use arrow::compute::interleave;
use arrow::row::{OwnedRow, Rows};
use futures::future::try_join_all;

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::format::MAX_ROWS;
use crate::reader::read_column;
use crate::writer::{FileSummary, StrataWriter, sort_key_indexes, write_file};

/// Merges `inputs` into one file sorted by `sort_key` (column names). Rows with equal keys keep
/// the order of their input, then their row. Returns the file bytes and its summary.
///
/// The inputs must share one schema and be sorted by the first sort-key column; both are
/// checked. Empty inputs are skipped. The row-source map costs 16 bytes per output row, and each
/// column is decoded from every input, then encoded again under the output's own decisions.
pub async fn merge(
    inputs: &[Arc<StrataFile>],
    sort_key: &[&str],
    config: Config,
) -> Result<(Vec<u8>, FileSummary)> {
    let Some(first) = inputs.first() else {
        return Err(schema_error("no input"));
    };
    let schema = first.schema();
    if inputs.iter().any(|file| file.schema() != schema) {
        return Err(schema_error("inputs have different schemas"));
    }
    // Runs every config and sort-key check of the writer.
    StrataWriter::new(Arc::clone(&schema), sort_key, config.clone())?;
    let (key_indexes, converter) = sort_key_indexes(&schema, sort_key)?;
    let first_key = key_indexes.first().copied();
    if inputs
        .iter()
        .any(|file| file.sort_key_column() != first_key)
    {
        return Err(schema_error("input sorted by another key"));
    }
    let total = inputs
        .iter()
        .fold(0u64, |sum, file| sum.saturating_add(file.row_count()));
    if total > MAX_ROWS {
        return Err(StrataError::TooManyRows { rows: total });
    }

    let kept: Vec<Arc<StrataFile>> = inputs
        .iter()
        .filter(|file| file.row_count() > 0)
        .cloned()
        .collect();
    if kept.is_empty() {
        return write_file(&schema, &key_indexes, &config, 0, |i| {
            std::future::ready(Ok(new_empty_array(schema.field(i).data_type())))
        })
        .await;
    }

    // One `Rows` per kept input, from the same converter, so the keys compare across inputs.
    let mut key_rows: Vec<Rows> = Vec::with_capacity(kept.len());
    for file in &kept {
        let mut keys: Vec<ArrayRef> = Vec::with_capacity(key_indexes.len());
        for &index in &key_indexes {
            keys.push(read_column(file, index).await?);
        }
        key_rows.push(converter.convert_columns(&keys)?);
    }

    // The heap holds the next row of each input, so its size is bounded by the input count.
    // Ties compare the input index, then the row, which keeps the input order.
    let mut heap: BinaryHeap<Reverse<(OwnedRow, usize, usize)>> = BinaryHeap::new();
    for (input, rows) in key_rows.iter().enumerate() {
        if rows.num_rows() > 0 {
            heap.push(Reverse((rows.row(0).owned(), input, 0)));
        }
    }
    // Not pre-sized: the counts in the files are not trusted for allocation.
    let mut sources: Vec<(usize, usize)> = Vec::new();
    while let Some(Reverse((_, input, row))) = heap.pop() {
        sources.push((input, row));
        let rows = key_rows.get(input).ok_or_else(row_count_error)?;
        let next = row + 1;
        if next < rows.num_rows() {
            let key = rows.row(next);
            if key < rows.row(row) {
                return Err(StrataError::Unsorted {
                    row: sources.len() as u64,
                });
            }
            heap.push(Reverse((key.owned(), input, next)));
        }
    }
    if sources.len() as u64 != total {
        return Err(row_count_error());
    }
    drop(key_rows);

    let live: &[Arc<StrataFile>] = &kept;
    let sources = Arc::new(sources);
    write_file(&schema, &key_indexes, &config, total, move |i| {
        let sources = Arc::clone(&sources);
        async move {
            let arrays = try_join_all(live.iter().map(|file| read_column(file, i))).await?;
            let refs: Vec<&dyn Array> = arrays.iter().map(|array| array.as_ref()).collect();
            Ok(interleave(&refs, &sources)?)
        }
    })
    .await
}

/// A schema-level input error with the given reason.
fn schema_error(reason: &str) -> StrataError {
    StrataError::Schema {
        reason: reason.to_string(),
    }
}

/// The error for a row count that does not match the files' claims.
fn row_count_error() -> StrataError {
    StrataError::Corrupt {
        what: "merge row count",
        offset: 0,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::testgen::{assert_same, batch, wide_batch};
    use arrow::array::{RecordBatch, UInt32Array};
    use arrow::compute::take_record_batch;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

    const SORT: [&str; 2] = ["trace_id", "start_time"];

    async fn open(bytes: Vec<u8>) -> Arc<StrataFile> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("merge.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        Arc::new(
            StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap(),
        )
    }

    /// Writes `rows` in one go with `sort` and `config`.
    fn write(rows: &RecordBatch, sort: &[&str], config: Config) -> (Vec<u8>, FileSummary) {
        let mut w = StrataWriter::new(rows.schema(), sort, config).unwrap();
        w.write(rows).unwrap();
        w.finish().unwrap()
    }

    /// The rows `i % parts == part` of `rows`, written with `SORT` and `config`, then opened.
    async fn part_of(
        rows: &RecordBatch,
        parts: u32,
        part: u32,
        config: &Config,
    ) -> Arc<StrataFile> {
        let picked: Vec<u32> = (0..rows.num_rows() as u32)
            .filter(|i| i % parts == part)
            .collect();
        let mask = UInt32Array::from(picked);
        let rows = take_record_batch(rows, &mask).unwrap();
        open(write(&rows, &SORT, config.clone()).0).await
    }

    #[tokio::test]
    async fn merged_columns_equal_a_one_go_write() {
        let rows = wide_batch(8_000);
        let config = Config::default();
        let mut inputs = Vec::new();
        for part in 0..4 {
            inputs.push(part_of(&rows, 4, part, &config).await);
        }
        let (bytes, summary) = merge(&inputs, &SORT, config.clone()).await.unwrap();
        assert_eq!(summary.rows, 8_000);
        let merged = open(bytes).await;
        let one_go = open(write(&rows, &SORT, config.clone()).0).await;
        for column in 0..rows.num_columns() {
            let got = read_column(&merged, column).await.unwrap();
            let want = read_column(&one_go, column).await.unwrap();
            assert_same(got.as_ref(), want.as_ref());
        }
    }

    #[tokio::test]
    async fn merged_bytes_equal_a_one_go_write() {
        let rows = wide_batch(8_000);
        let config = Config::default();
        let mut inputs = Vec::new();
        for part in 0..4 {
            inputs.push(part_of(&rows, 4, part, &config).await);
        }
        let (bytes, summary) = merge(&inputs, &SORT, config.clone()).await.unwrap();
        let (expected, one_go) = write(&rows, &SORT, config);
        if bytes != expected {
            let first = summary
                .columns
                .iter()
                .zip(&one_go.columns)
                .position(|(got, want)| got.bytes != want.bytes);
            panic!(
                "merged bytes differ from the one-go write; first column with other bytes: {first:?}"
            );
        }
    }

    #[tokio::test]
    async fn empty_inputs_are_skipped() {
        let rows = wide_batch(500);
        let config = Config::default();
        let a = open(write(&rows, &SORT, config.clone()).0).await;
        let empty = open(write(&rows.slice(0, 0), &SORT, config.clone()).0).await;

        let (with_empty, _) = merge(&[Arc::clone(&a), Arc::clone(&empty)], &SORT, config.clone())
            .await
            .unwrap();
        let (only_a, _) = merge(std::slice::from_ref(&a), &SORT, config.clone())
            .await
            .unwrap();
        assert_eq!(with_empty, only_a);

        let (none, summary) = merge(
            &[Arc::clone(&empty), Arc::clone(&empty)],
            &SORT,
            config.clone(),
        )
        .await
        .unwrap();
        assert_eq!(summary.rows, 0);
        assert_eq!(open(none).await.row_count(), 0);
    }

    #[tokio::test]
    async fn merge_without_input_is_a_schema_error() {
        assert!(matches!(
            merge(&[], &SORT, Config::default()).await,
            Err(StrataError::Schema { .. })
        ));
    }

    #[tokio::test]
    async fn inputs_with_different_schemas_are_a_schema_error() {
        let a = open(write(&wide_batch(10), &SORT, Config::default()).0).await;
        let other = open(write(&batch(10), &SORT, Config::default()).0).await;
        assert!(matches!(
            merge(&[a, other], &SORT, Config::default()).await,
            Err(StrataError::Schema { .. })
        ));
    }

    #[tokio::test]
    async fn sort_key_naming_a_missing_column_is_a_schema_error() {
        let a = open(write(&wide_batch(10), &SORT, Config::default()).0).await;
        assert!(matches!(
            merge(
                std::slice::from_ref(&a),
                &["trace_id", "missing"],
                Config::default()
            )
            .await,
            Err(StrataError::Schema { .. })
        ));
    }

    #[tokio::test]
    async fn input_sorted_by_another_key_is_a_schema_error() {
        let by_start = open(write(&wide_batch(10), &["start_time"], Config::default()).0).await;
        assert!(matches!(
            merge(std::slice::from_ref(&by_start), &SORT, Config::default()).await,
            Err(StrataError::Schema { .. })
        ));
    }

    #[tokio::test]
    async fn input_out_of_order_for_the_merge_key_is_unsorted() {
        // Both rows share trace id 0, so the writer accepts them under `["trace_id"]`, but the
        // second row has the earlier start time.
        let picked = UInt32Array::from(vec![1u32, 0]);
        let rows = take_record_batch(&wide_batch(5), &picked).unwrap();
        let file = open(write(&rows, &["trace_id"], Config::default()).0).await;
        assert!(matches!(
            merge(std::slice::from_ref(&file), &SORT, Config::default()).await,
            Err(StrataError::Unsorted { .. })
        ));
    }

    #[test]
    fn merge_future_is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let future = merge(&[], &SORT, Config::default());
        assert_send(&future);
    }
}
