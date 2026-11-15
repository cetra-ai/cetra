//! Scans of narrow columns: a predicate pruned by zones and dictionaries, late materialisation
//! of the projection, and a limit (design S7 § scan).
//!
//! A scan runs in waves. The first wave loads the metadata of every column it reads. The filter
//! wave fetches the surviving chunks of the predicate columns. The projection wave fetches the
//! projected chunks that hold a selected row. Each wave is one `fetch`, so it is counted once.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, RecordBatch, RecordBatchOptions, UInt32Array, new_empty_array,
};
use arrow::compute::{concat, take};
use arrow::datatypes::{Field, SchemaRef};

use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::narrow::{BlockEntry, Dictionary, decode_chunk};
use crate::predicate::{
    Bound, Predicate, bind, bound_columns, chunk_may_match, evaluate, matching_codes,
    pruned_columns,
};
use crate::segment::{Kind, SegmentRef};

/// Rows per output batch.
const BATCH_ROWS: usize = 8_192;

/// What to read from a file.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    /// Schema indexes of the output columns, in output order. May be empty.
    pub projection: Vec<usize>,
    /// Rows must satisfy it; `None` selects every row.
    pub predicate: Option<Predicate>,
    /// Restrict the scan to these rows: sorted, disjoint, non-empty ranges within the file.
    pub row_ranges: Option<Vec<Range<u32>>>,
    /// At most this many rows, the first ones in file order.
    pub limit: Option<usize>,
    /// How reads are coalesced.
    pub kind: ScanKind,
}

/// The decoded chunks read so far, by (directory id, chunk index).
type Decoded = HashMap<(usize, usize), ArrayRef>;

/// What a scan needs of one column: its field, its dictionary and its block-table entries.
struct ColumnInfo {
    field: Field,
    dict: Option<Arc<Dictionary>>,
    entries: Arc<Vec<BlockEntry>>,
}

/// Removes `remove` from `ranges`, which are sorted and disjoint. The result is sorted,
/// disjoint and has no empty range.
pub(crate) fn subtract(ranges: &[Range<u32>], remove: Range<u32>) -> Vec<Range<u32>> {
    let mut out = Vec::with_capacity(ranges.len() + 1);
    for range in ranges {
        if remove.end <= range.start || range.end <= remove.start {
            out.push(range.clone());
            continue;
        }
        if range.start < remove.start {
            out.push(range.start..remove.start);
        }
        if remove.end < range.end {
            out.push(remove.end..range.end);
        }
    }
    out
}

/// The indexes of the chunks in `entries` that overlap one of `ranges`, in order.
fn overlapping_chunks(entries: &[BlockEntry], ranges: &[Range<u32>]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let first = u64::from(entry.first_row);
            let end = first + u64::from(entry.n_rows);
            ranges
                .iter()
                .any(|range| u64::from(range.start) < end && first < u64::from(range.end))
        })
        .map(|(index, _)| index)
        .collect()
}

/// The index of the chunk of `entries` that holds `row`.
fn chunk_of(entries: &[BlockEntry], row: u32) -> Result<usize> {
    let corrupt = || StrataError::Schema {
        reason: "internal: row outside the chunks".to_string(),
    };
    let count = entries.partition_point(|entry| entry.first_row <= row);
    let index = count.checked_sub(1).ok_or_else(corrupt)?;
    let entry = entries.get(index).ok_or_else(corrupt)?;
    let end = u64::from(entry.first_row) + u64::from(entry.n_rows);
    if u64::from(row) < end {
        Ok(index)
    } else {
        Err(corrupt())
    }
}

/// The row `start + offset`, checked against overflow.
fn row_at(start: u32, offset: usize) -> Result<u32> {
    u32::try_from(offset)
        .ok()
        .and_then(|offset| start.checked_add(offset))
        .ok_or_else(|| StrataError::Corrupt {
            what: "row range",
            offset: u64::from(start),
        })
}

/// The decoded chunk `(column, index)`, which every wave decodes before it is used.
fn decoded_chunk(decoded: &Decoded, column: usize, index: usize) -> Result<&ArrayRef> {
    decoded
        .get(&(column, index))
        .ok_or_else(|| StrataError::Schema {
            reason: "internal: undecoded chunk".to_string(),
        })
}

/// The rows of the decoded `column` at `rows` (ascending, with each row in a chunk of `info`),
/// as one array of the column's type.
fn take_rows(
    decoded: &Decoded,
    info: &ColumnInfo,
    column: usize,
    rows: &[u32],
) -> Result<ArrayRef> {
    let mut pieces: Vec<ArrayRef> = Vec::new();
    let mut rest = rows;
    while let Some(&first) = rest.first() {
        let index = chunk_of(&info.entries, first)?;
        let entry = info.entries.get(index).ok_or_else(|| StrataError::Schema {
            reason: "internal: missing chunk".to_string(),
        })?;
        let end = u64::from(entry.first_row) + u64::from(entry.n_rows);
        let run = rest.iter().take_while(|&&row| u64::from(row) < end).count();
        let (head, tail) = rest
            .split_at_checked(run)
            .ok_or_else(|| StrataError::Schema {
                reason: "internal: run of rows".to_string(),
            })?;
        let local: Vec<u32> = head
            .iter()
            .map(|&row| {
                row.checked_sub(entry.first_row)
                    .ok_or_else(|| StrataError::Corrupt {
                        what: "chunk row",
                        offset: u64::from(row),
                    })
            })
            .collect::<Result<_>>()?;
        let chunk = decoded_chunk(decoded, column, index)?;
        pieces.push(take(chunk.as_ref(), &UInt32Array::from(local), None)?);
        rest = tail;
    }
    match pieces.len() {
        0 => Ok(new_empty_array(info.field.data_type())),
        1 => Ok(pieces.swap_remove(0)),
        _ => {
            let refs: Vec<&dyn Array> = pieces.iter().map(|piece| piece.as_ref()).collect();
            Ok(concat(&refs)?)
        }
    }
}

impl StrataFile {
    /// Rows of the file matching `req`, as record batches of at most 8,192 rows. The batches
    /// hold the projection, in output order, and the rows are in file order.
    ///
    /// The scan takes at most four waves after the metadata is cached: the metadata, the filter,
    /// and the projection. A wave with nothing to fetch makes no request. Scans of payload and
    /// map columns, `Contains` and `MapKeyEq` are not yet supported (M4.3).
    pub async fn scan(&self, req: &ScanRequest) -> Result<Vec<RecordBatch>> {
        // 1. Validate. Every error is found before any read.
        self.check_projection(&req.projection)?;
        let bound = match &req.predicate {
            Some(predicate) => Some(bind(predicate, self)?),
            None => None,
        };
        self.check_row_ranges(req.row_ranges.as_deref())?;
        let output: SchemaRef = Arc::new(self.schema().project(&req.projection)?);
        if req.limit == Some(0) {
            return Ok(Vec::new());
        }

        // 2. Metadata wave: the block tables and dictionaries of every column the scan reads.
        let predicate_columns = bound.as_ref().map(bound_columns).unwrap_or_default();
        let mut all_columns: BTreeSet<usize> = predicate_columns.iter().copied().collect();
        all_columns.extend(req.projection.iter().copied());
        let all_columns: Vec<usize> = all_columns.into_iter().collect();
        self.load_metadata(&all_columns, req.kind).await?;
        let mut infos: HashMap<usize, ColumnInfo> = HashMap::new();
        for &column in &all_columns {
            let info = ColumnInfo {
                field: self.field_of(column)?,
                dict: self.dictionary(column).await?,
                entries: self.block_entries(column).await?,
            };
            infos.insert(column, info);
        }

        // 3. Candidate rows, pruned by the zones and dictionaries of the predicate's columns.
        let rows = self.row_count();
        let mut candidates: Vec<Range<u32>> = match &req.row_ranges {
            Some(ranges) => ranges.clone(),
            None if rows == 0 => Vec::new(),
            None => {
                let end = u32::try_from(rows).map_err(|_| StrataError::Corrupt {
                    what: "row count",
                    offset: rows,
                })?;
                std::iter::once(0..end).collect()
            }
        };
        if let Some(b) = &bound {
            for column in pruned_columns(b) {
                let info = infos.get(&column).ok_or_else(|| StrataError::Schema {
                    reason: format!("internal: no info for column {column}"),
                })?;
                let codes = matching_codes(b, column, info.dict.as_deref())?;
                for entry in info.entries.iter() {
                    let may = chunk_may_match(
                        b,
                        column,
                        entry,
                        &info.field,
                        info.dict.as_deref(),
                        codes.as_ref(),
                    )?;
                    if !may {
                        let end = entry.first_row.checked_add(entry.n_rows).ok_or_else(|| {
                            StrataError::Corrupt {
                                what: "block row order",
                                offset: u64::from(entry.first_row),
                            }
                        })?;
                        candidates = subtract(&candidates, entry.first_row..end);
                    }
                }
                if candidates.is_empty() {
                    return Ok(Vec::new());
                }
            }
        }

        // 4. Filter wave: the surviving chunks of the predicate columns, then the mask.
        let mut decoded: Decoded = HashMap::new();
        let mut selection: Vec<u32> = match &bound {
            None => candidates.iter().flat_map(|range| range.clone()).collect(),
            Some(b) => {
                self.filter(
                    b,
                    &predicate_columns,
                    &infos,
                    &candidates,
                    req.kind,
                    &mut decoded,
                )
                .await?
            }
        };

        // 5. Limit.
        if let Some(limit) = req.limit {
            selection.truncate(limit);
        }
        if selection.is_empty() {
            return Ok(Vec::new());
        }

        // 6. Projection wave: the projected chunks that hold a selected row and are not decoded.
        let mut wanted: BTreeSet<(usize, usize)> = BTreeSet::new();
        for &column in &req.projection {
            let info = infos.get(&column).ok_or_else(|| StrataError::Schema {
                reason: format!("internal: no info for column {column}"),
            })?;
            for &row in &selection {
                let index = chunk_of(&info.entries, row)?;
                if !decoded.contains_key(&(column, index)) {
                    wanted.insert((column, index));
                }
            }
        }
        self.decode_wave(&infos, &wanted, req.kind, &mut decoded)
            .await?;

        // Take the rows of each output batch.
        let mut batches = Vec::new();
        for batch_rows in selection.chunks(BATCH_ROWS) {
            let mut columns: Vec<ArrayRef> = Vec::with_capacity(req.projection.len());
            for &column in &req.projection {
                let info = infos.get(&column).ok_or_else(|| StrataError::Schema {
                    reason: format!("internal: no info for column {column}"),
                })?;
                columns.push(take_rows(&decoded, info, column, batch_rows)?);
            }
            let options = RecordBatchOptions::new().with_row_count(Some(batch_rows.len()));
            batches.push(RecordBatch::try_new_with_options(
                Arc::clone(&output),
                columns,
                &options,
            )?);
        }
        Ok(batches)
    }

    /// Checks that every projected column is a narrow column of the file.
    fn check_projection(&self, projection: &[usize]) -> Result<()> {
        for &column in projection {
            if column >= self.schema().fields().len() {
                return Err(StrataError::Schema {
                    reason: format!("no column {column}"),
                });
            }
            let record = self
                .directory()
                .get(column)
                .ok_or_else(|| StrataError::Schema {
                    reason: format!("no column {column}"),
                })?;
            if record.layer != Layer::Narrow {
                return Err(StrataError::Unsupported {
                    what: format!("projection of column {column}"),
                });
            }
        }
        Ok(())
    }

    /// Checks that the row ranges are non-empty, inside the file, sorted and disjoint.
    fn check_row_ranges(&self, ranges: Option<&[Range<u32>]>) -> Result<()> {
        let Some(ranges) = ranges else {
            return Ok(());
        };
        let mut previous_end = 0u64;
        for range in ranges {
            let (start, end) = (u64::from(range.start), u64::from(range.end));
            if start >= end || end > self.row_count() || start < previous_end {
                return Err(StrataError::Schema {
                    reason: "row ranges".to_string(),
                });
            }
            previous_end = end;
        }
        Ok(())
    }

    /// The filter wave. Fetches the chunks of the predicate columns that overlap a candidate
    /// range, decodes them into `decoded`, and returns the rows of the candidates for which the
    /// predicate is TRUE, in order.
    async fn filter(
        &self,
        b: &Bound,
        columns: &[usize],
        infos: &HashMap<usize, ColumnInfo>,
        candidates: &[Range<u32>],
        kind: ScanKind,
        decoded: &mut Decoded,
    ) -> Result<Vec<u32>> {
        let mut wanted: Vec<(usize, usize)> = Vec::new();
        for &column in columns {
            let info = infos.get(&column).ok_or_else(|| StrataError::Schema {
                reason: format!("internal: no info for column {column}"),
            })?;
            for index in overlapping_chunks(&info.entries, candidates) {
                if !decoded.contains_key(&(column, index)) {
                    wanted.push((column, index));
                }
            }
        }
        let wanted: BTreeSet<(usize, usize)> = wanted.into_iter().collect();
        self.decode_wave(infos, &wanted, kind, decoded).await?;

        let n_fields = self.schema().fields().len();
        let mut selection = Vec::new();
        for range in candidates {
            let len = range
                .end
                .checked_sub(range.start)
                .and_then(|len| usize::try_from(len).ok())
                .ok_or_else(|| StrataError::Corrupt {
                    what: "row range",
                    offset: u64::from(range.start),
                })?;
            let mut arrays: Vec<Option<ArrayRef>> = vec![None; n_fields];
            for &column in columns {
                let info = infos.get(&column).ok_or_else(|| StrataError::Schema {
                    reason: format!("internal: no info for column {column}"),
                })?;
                let mut pieces: Vec<ArrayRef> = Vec::new();
                for index in overlapping_chunks(&info.entries, std::slice::from_ref(range)) {
                    let entry = info.entries.get(index).ok_or_else(|| StrataError::Schema {
                        reason: "internal: missing chunk".to_string(),
                    })?;
                    let first = entry.first_row;
                    let end =
                        first
                            .checked_add(entry.n_rows)
                            .ok_or_else(|| StrataError::Corrupt {
                                what: "block row order",
                                offset: u64::from(first),
                            })?;
                    let from = range.start.max(first);
                    let to = range.end.min(end);
                    let chunk = decoded_chunk(decoded, column, index)?;
                    let offset =
                        usize::try_from(from - first).map_err(|_| StrataError::Corrupt {
                            what: "chunk row",
                            offset: u64::from(first),
                        })?;
                    let length = usize::try_from(to - from).map_err(|_| StrataError::Corrupt {
                        what: "row range",
                        offset: u64::from(from),
                    })?;
                    pieces.push(chunk.slice(offset, length));
                }
                let refs: Vec<&dyn Array> = pieces.iter().map(|piece| piece.as_ref()).collect();
                let assembled = concat(&refs)?;
                if let Some(slot) = arrays.get_mut(column) {
                    *slot = Some(assembled);
                }
            }
            let mask = evaluate(b, &arrays, len)?;
            for i in 0..len {
                if mask.is_valid(i) && mask.value(i) {
                    selection.push(row_at(range.start, i)?);
                }
            }
        }
        Ok(selection)
    }

    /// Fetches, in one wave, the bodies of the chunks in `wanted` that are not decoded yet, and
    /// decodes them into `decoded`. Chunks without a segment are decoded from nothing. A wave
    /// with nothing to fetch makes no request.
    async fn decode_wave(
        &self,
        infos: &HashMap<usize, ColumnInfo>,
        wanted: &BTreeSet<(usize, usize)>,
        kind: ScanKind,
        decoded: &mut Decoded,
    ) -> Result<()> {
        let mut segments: Vec<(SegmentRef, Kind)> = Vec::new();
        let mut pending: Vec<(usize, usize, BlockEntry)> = Vec::new();
        for &(column, index) in wanted {
            let info = infos.get(&column).ok_or_else(|| StrataError::Schema {
                reason: format!("internal: no info for column {column}"),
            })?;
            let entry = *info.entries.get(index).ok_or_else(|| StrataError::Schema {
                reason: "internal: missing chunk".to_string(),
            })?;
            if entry.segment_len > 0 {
                let segment = SegmentRef {
                    offset: entry.segment_offset,
                    len: entry.segment_len,
                };
                segments.push((segment, Kind::NarrowChunk));
                pending.push((column, index, entry));
            } else {
                let array = decode_chunk(&entry, None, &info.field, info.dict.as_deref())?;
                decoded.insert((column, index), array);
            }
        }
        let bodies = self.fetch(&segments, kind).await?;
        if bodies.len() != pending.len() {
            return Err(StrataError::Corrupt {
                what: "segment range",
                offset: 0,
            });
        }
        for ((column, index, entry), body) in pending.into_iter().zip(&bodies) {
            let info = infos.get(&column).ok_or_else(|| StrataError::Schema {
                reason: format!("internal: no info for column {column}"),
            })?;
            let array = decode_chunk(
                &entry,
                Some(body.as_slice()),
                &info.field,
                info.dict.as_deref(),
            )?;
            decoded.insert((column, index), array);
        }
        Ok(())
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
    use crate::config::Config;
    use crate::narrow::Encoding;
    use crate::predicate::{CmpOp, Lit};
    use crate::reader::read_column;
    use crate::testgen::{assert_same, wide_batch, wide_schema};
    use crate::writer::StrataWriter;
    use arrow::array::BooleanArray;
    use arrow::compute::filter_record_batch;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use proptest::prelude::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// `wide_batch(rows)`, written with `config`, and opened.
    async fn open_wide(rows: usize, config: &Config) -> StrataFile {
        let mut w =
            StrataWriter::new(wide_schema(), &["trace_id", "start_time"], config.clone()).unwrap();
        w.write(&wide_batch(rows)).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("table.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap()
    }

    /// The column index of `name` in the wide schema.
    fn col(name: &str) -> usize {
        wide_schema().index_of(name).unwrap()
    }

    /// The rows of the full table that match `p` within `ranges`, projected onto `projection`.
    /// Evaluated with Arrow on the whole columns, through the same binding.
    async fn oracle(
        file: &StrataFile,
        p: Option<&Predicate>,
        ranges: Option<&[Range<u32>]>,
        projection: &[usize],
    ) -> Result<RecordBatch> {
        let schema = file.schema();
        let rows = usize::try_from(file.row_count()).unwrap();
        let mut columns: Vec<Option<ArrayRef>> = Vec::new();
        for c in 0..schema.fields().len() {
            columns.push(Some(read_column(file, c).await?));
        }
        let full = RecordBatch::try_new(
            Arc::clone(&schema),
            columns.iter().map(|c| c.clone().unwrap()).collect(),
        )?;
        let mut keep = vec![true; rows];
        if let Some(p) = p {
            let bound = bind(p, file)?;
            let mask = evaluate(&bound, &columns, rows)?;
            for (i, k) in keep.iter_mut().enumerate() {
                *k = *k && mask.is_valid(i) && mask.value(i);
            }
        }
        if let Some(ranges) = ranges {
            for (i, k) in keep.iter_mut().enumerate() {
                let in_range = ranges
                    .iter()
                    .any(|r| (r.start as usize..r.end as usize).contains(&i));
                *k = *k && in_range;
            }
        }
        let projected = full.project(projection)?;
        Ok(filter_record_batch(&projected, &BooleanArray::from(keep))?)
    }

    /// Concatenates the batches of a scan, or builds an empty batch of `schema`.
    fn joined(schema: &SchemaRef, batches: &[RecordBatch]) -> RecordBatch {
        if batches.is_empty() {
            return RecordBatch::new_empty(Arc::clone(schema));
        }
        arrow::compute::concat_batches(schema, batches).unwrap()
    }

    /// Asserts that `got` has the same rows and columns as `want`.
    fn assert_batches_equal(got: &RecordBatch, want: &RecordBatch) {
        assert_eq!(got.schema(), want.schema());
        assert_eq!(got.num_rows(), want.num_rows(), "row count");
        for c in 0..want.num_columns() {
            assert_same(got.column(c).as_ref(), want.column(c).as_ref());
        }
    }

    /// Runs `req` on `file` and compares the result with the oracle.
    async fn check(file: &StrataFile, p: Predicate, projection: &[usize]) {
        let req = ScanRequest {
            projection: projection.to_vec(),
            predicate: Some(p.clone()),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
        };
        let batches = file.scan(&req).await.unwrap();
        let want = oracle(file, Some(&p), None, projection).await.unwrap();
        let got = joined(&want.schema(), &batches);
        assert_batches_equal(&got, &want);
    }

    fn lit_int(v: i64) -> Lit {
        Lit::Int(v)
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn subtract_splits_trims_drops_and_keeps() {
        // A split.
        assert_eq!(subtract(&[0..10], 3..5), vec![0..3, 5..10]);
        // A trim at either end.
        assert_eq!(subtract(&[0..10], 0..4), vec![4..10]);
        assert_eq!(subtract(&[0..10], 7..12), vec![0..7]);
        // A dropped range.
        assert_eq!(subtract(&[0..5, 8..9], 0..5), vec![8..9]);
        // A no-op.
        assert_eq!(subtract(&[0..5], 7..9), vec![0..5]);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn overlapping_chunks_picks_the_chunks_of_the_ranges() {
        let entry = |first_row: u32, n_rows: u32| BlockEntry {
            first_row,
            n_rows,
            segment_offset: 0,
            segment_len: 0,
            encoding: Encoding::ConstNull,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let entries = [entry(0, 10), entry(10, 10), entry(20, 10)];
        assert_eq!(overlapping_chunks(&entries, &[5..12]), vec![0, 1]);
        assert_eq!(overlapping_chunks(&entries, &[20..21]), vec![2]);
        assert!(overlapping_chunks(&entries, &[30..40]).is_empty());
    }

    #[test]
    fn comparisons_and_null_handling_match_the_oracle() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let tokens = col("input_tokens");
            let cases: Vec<Predicate> = vec![
                Predicate::Cmp(tokens, CmpOp::Eq, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::NotEq, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::Lt, lit_int(-1)),
                Predicate::Cmp(tokens, CmpOp::Lt, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::LtEq, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::Gt, lit_int(10_000)),
                Predicate::Cmp(tokens, CmpOp::Gt, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::GtEq, lit_int(4_999)),
                Predicate::Cmp(tokens, CmpOp::Eq, Lit::Null),
                Predicate::Not(Box::new(Predicate::Cmp(tokens, CmpOp::Lt, lit_int(2_500)))),
                Predicate::IsNull(tokens),
                Predicate::IsNotNull(col("cost")),
                Predicate::Cmp(col("cost"), CmpOp::Gt, Lit::Float(100.0)),
                Predicate::Cmp(col("cost"), CmpOp::Lt, Lit::Float(0.5)),
                Predicate::Cmp(col("cost"), CmpOp::Eq, Lit::Float(f64::NAN)),
                Predicate::Cmp(col("f32"), CmpOp::Eq, Lit::Float(0.1)),
                Predicate::Cmp(col("f32"), CmpOp::Gt, Lit::Float(5_000.0)),
                Predicate::Cmp(col("f32"), CmpOp::Lt, Lit::Float(0.0)),
                Predicate::Cmp(
                    col("start_time"),
                    CmpOp::Gt,
                    Lit::Int(1_700_000_000_000_000_000 + 7_000 * 1_000_003),
                ),
                Predicate::Cmp(
                    col("start_time"),
                    CmpOp::LtEq,
                    Lit::Int(1_700_000_000_000_000_000 + 7_000 * 1_000_003),
                ),
                Predicate::Cmp(
                    col("trace_id"),
                    CmpOp::Eq,
                    Lit::Bytes(((7_000 / 5) as u128).to_be_bytes().to_vec()),
                ),
                Predicate::Cmp(
                    col("trace_id"),
                    CmpOp::Gt,
                    Lit::Bytes(((7_000 / 5) as u128).to_be_bytes().to_vec()),
                ),
                Predicate::Cmp(col("model"), CmpOp::Eq, Lit::Utf8("gpt-4o".into())),
                Predicate::Cmp(col("model"), CmpOp::Eq, Lit::Utf8("missing".into())),
                Predicate::In(tokens, vec![]),
                Predicate::In(tokens, vec![lit_int(2_500)]),
                Predicate::In(tokens, vec![lit_int(2_500), Lit::Null, lit_int(10)]),
                Predicate::IsNull(col("model")),
                Predicate::And(vec![
                    Predicate::Cmp(tokens, CmpOp::Gt, lit_int(1_000)),
                    Predicate::Cmp(col("status"), CmpOp::Eq, Lit::Utf8("ok".into())),
                ]),
                Predicate::Or(vec![
                    Predicate::Cmp(col("span_kind"), CmpOp::Eq, Lit::Utf8("llm".into())),
                    Predicate::Cmp(col("model"), CmpOp::Eq, Lit::Utf8("claude-3-haiku".into())),
                ]),
            ];
            let projection = [col("trace_id"), tokens, col("model")];
            for p in cases {
                check(&file, p.clone(), &projection).await;
            }
        });
    }

    #[test]
    fn a_float_zone_prunes_only_when_no_chunk_holds_nan() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let f32_col = col("f32");
            // Warm the metadata, so the counters below see only the filter wave.
            file.block_entries(f32_col).await.unwrap();
            file.reset_io_stats();
            let req = ScanRequest {
                projection: vec![f32_col],
                predicate: Some(Predicate::Cmp(f32_col, CmpOp::Gt, Lit::Float(5_000.0))),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
            };
            let batches = file.scan(&req).await.unwrap();
            let stats = file.io_stats();
            // Chunks 0 and 1 are pruned by their zones: only the last three chunks are read.
            assert_eq!(stats.waves, 1);
            assert!(stats.requests < 5, "{} requests", stats.requests);
            let want = oracle(&file, req.predicate.as_ref(), None, &[f32_col])
                .await
                .unwrap();
            assert!(want.num_rows() > 0);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);
        });
    }

    #[test]
    fn a_predicate_on_a_pruned_chunk_reads_only_the_candidates() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let time = col("start_time");
            // The `start_time` of row 19,000, from the generator's formula: the file is cold.
            let threshold = 1_700_000_000_000_000_000i64 + 19_000 * 1_000_003;
            file.reset_io_stats();
            let req = ScanRequest {
                projection: vec![time],
                predicate: Some(Predicate::Cmp(time, CmpOp::GtEq, Lit::Int(threshold))),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
            };
            let batches = file.scan(&req).await.unwrap();
            let stats = file.io_stats();
            // Metadata, then the filter. The projection is the predicate column: no third wave.
            assert_eq!(stats.waves, 2);
            assert_eq!(stats.requests, 2);
            let want = oracle(&file, req.predicate.as_ref(), None, &[time])
                .await
                .unwrap();
            assert_batches_equal(&joined(&want.schema(), &batches), &want);
        });
    }

    #[test]
    fn an_absent_dictionary_value_costs_only_the_metadata_wave() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let status = col("status");
            file.reset_io_stats();
            let req = ScanRequest {
                projection: vec![status],
                predicate: Some(Predicate::Cmp(
                    status,
                    CmpOp::Eq,
                    Lit::Utf8("missing".into()),
                )),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
            };
            let batches = file.scan(&req).await.unwrap();
            assert!(batches.is_empty());
            assert_eq!(file.io_stats().waves, 1);
        });
    }

    #[test]
    fn row_ranges_restrict_the_rows() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let projection = [col("trace_id"), col("input_tokens")];
            let ranges = vec![100..200, 5_000..5_010];
            let req = ScanRequest {
                projection: projection.to_vec(),
                predicate: None,
                row_ranges: Some(ranges.clone()),
                limit: None,
                kind: ScanKind::Scan,
            };
            let batches = file.scan(&req).await.unwrap();
            let want = oracle(&file, None, Some(ranges.as_slice()), &projection)
                .await
                .unwrap();
            assert_eq!(want.num_rows(), 110);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);

            // With a predicate, the result is the intersection with the oracle.
            let p = Predicate::Cmp(col("input_tokens"), CmpOp::Gt, lit_int(2_500));
            let req = ScanRequest {
                projection: projection.to_vec(),
                predicate: Some(p.clone()),
                row_ranges: Some(vec![100..9_000, 15_000..15_050]),
                limit: None,
                kind: ScanKind::Scan,
            };
            let batches = file.scan(&req).await.unwrap();
            let restricted = [100..9_000, 15_000..15_050];
            let want = oracle(&file, Some(&p), Some(restricted.as_slice()), &projection)
                .await
                .unwrap();
            assert!(want.num_rows() > 0);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);
        });
    }

    #[test]
    fn limits_take_the_first_rows_and_zero_reads_nothing() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let projection = [col("trace_id"), col("input_tokens")];
            let p = Predicate::Cmp(col("cost"), CmpOp::Gt, Lit::Float(1.0));
            let req = ScanRequest {
                projection: projection.to_vec(),
                predicate: Some(p.clone()),
                row_ranges: None,
                limit: Some(7),
                kind: ScanKind::Scan,
            };
            let batches = file.scan(&req).await.unwrap();
            let want = oracle(&file, Some(&p), None, &projection)
                .await
                .unwrap()
                .slice(0, 7);
            assert_eq!(want.num_rows(), 7);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);

            file.reset_io_stats();
            let zero = ScanRequest {
                limit: Some(0),
                ..req.clone()
            };
            assert!(file.scan(&zero).await.unwrap().is_empty());
            assert_eq!(file.io_stats().waves, 0);
        });
    }

    #[test]
    fn an_empty_projection_counts_the_matching_rows() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let p = Predicate::Cmp(col("input_tokens"), CmpOp::Gt, lit_int(2_500));
            let req = ScanRequest {
                projection: Vec::new(),
                predicate: Some(p.clone()),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
            };
            let batches = file.scan(&req).await.unwrap();
            assert!(batches.iter().all(|b| b.num_columns() == 0));
            let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
            let want = oracle(&file, Some(&p), None, &[]).await.unwrap();
            assert_eq!(rows, want.num_rows());
        });
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn bad_requests_are_schema_or_unsupported_errors() {
        runtime().block_on(async {
            let file = open_wide(2_000, &Config::default()).await;
            let scan = |projection: Vec<usize>,
                        predicate: Option<Predicate>,
                        ranges: Option<Vec<Range<u32>>>| {
                let req = ScanRequest {
                    projection,
                    predicate,
                    row_ranges: ranges,
                    limit: None,
                    kind: ScanKind::Scan,
                };
                let file = &file;
                async move { file.scan(&req).await }
            };
            let trace = col("trace_id");
            let tokens = col("input_tokens");

            assert!(matches!(
                scan(vec![99], None, None).await,
                Err(StrataError::Schema { .. })
            ));
            assert!(matches!(
                scan(vec![trace], None, Some(vec![5..9, 0..3])).await,
                Err(StrataError::Schema { .. })
            ));
            assert!(matches!(
                scan(vec![trace], None, Some(vec![5..5])).await,
                Err(StrataError::Schema { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(trace, CmpOp::Eq, Lit::Bytes(vec![0; 15]))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(tokens, CmpOp::Eq, Lit::Float(3.5))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(tokens, CmpOp::Eq, Lit::UInt(u64::MAX))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(tokens, CmpOp::Eq, Lit::Utf8("x".into()))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(col("attrs"), CmpOp::Eq, Lit::Null)),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(vec![col("attrs")], None, None).await,
                Err(StrataError::Unsupported { .. })
            ));
            let mixed = Predicate::And(vec![
                Predicate::Cmp(col("status"), CmpOp::Eq, Lit::Utf8("missing".into())),
                Predicate::Cmp(trace, CmpOp::Eq, Lit::Bytes(vec![0; 3])),
            ]);
            assert!(matches!(
                scan(vec![trace], Some(mixed), None).await,
                Err(StrataError::Unsupported { .. })
            ));
        });
    }

    fn lit_strategy() -> impl Strategy<Value = Lit> {
        prop_oneof![
            Just(Lit::Null),
            any::<bool>().prop_map(Lit::Bool),
            any::<i64>().prop_map(Lit::Int),
            any::<u64>().prop_map(Lit::UInt),
            any::<f64>().prop_map(Lit::Float),
            "[a-z0-9-]{0,12}".prop_map(Lit::Utf8),
            proptest::collection::vec(any::<u8>(), 0..20).prop_map(Lit::Bytes),
        ]
    }

    fn op_strategy() -> impl Strategy<Value = CmpOp> {
        prop_oneof![
            Just(CmpOp::Eq),
            Just(CmpOp::NotEq),
            Just(CmpOp::Lt),
            Just(CmpOp::LtEq),
            Just(CmpOp::Gt),
            Just(CmpOp::GtEq),
        ]
    }

    fn predicate_strategy() -> impl Strategy<Value = Predicate> {
        let leaf = prop_oneof![
            (0usize..16, op_strategy(), lit_strategy())
                .prop_map(|(c, op, lit)| Predicate::Cmp(c, op, lit)),
            (0usize..16, proptest::collection::vec(lit_strategy(), 0..3))
                .prop_map(|(c, lits)| Predicate::In(c, lits)),
            (0usize..16).prop_map(Predicate::IsNull),
            (0usize..16).prop_map(Predicate::IsNotNull),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..3).prop_map(Predicate::And),
                proptest::collection::vec(inner.clone(), 0..3).prop_map(Predicate::Or),
                inner.prop_map(|p| Predicate::Not(Box::new(p))),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn random_predicates_never_panic_and_agree_with_the_oracle(p in predicate_strategy()) {
            let file = runtime().block_on(open_wide(3_000, &Config::default()));
            runtime().block_on(async {
                let req = ScanRequest {
                    projection: vec![col("trace_id"), col("input_tokens")],
                    predicate: Some(p.clone()),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Scan,
                };
                let got = file.scan(&req).await;
                let want = oracle(&file, Some(&p), None, &req.projection).await;
                match (got, want) {
                    (Ok(batches), Ok(want)) => {
                        assert_batches_equal(&joined(&want.schema(), &batches), &want);
                    }
                    (Err(got), Err(want)) => {
                        assert_eq!(
                            std::mem::discriminant(&got),
                            std::mem::discriminant(&want),
                            "{got} vs {want}"
                        );
                    }
                    (got, want) => panic!("scan {got:?} but oracle {want:?}"),
                }
            });
        }
    }
}
