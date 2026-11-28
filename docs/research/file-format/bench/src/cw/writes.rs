//! `fmtbench cw run … --queries compact,flush:<n>`: the write queries of M2 (plans/08-m2-writes-compaction.md
//! § M2.2). `compact` merges a case's files into one per format; `flush:<n>` has `n` writers
//! write the same batch at once.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Instant;

use arrow::array::{Array, ArrayRef, FixedSizeBinaryArray, RecordBatch, TimestampNanosecondArray};
use arrow::compute::{concat_batches, interleave_record_batch};
use arrow::datatypes::{DataType, Schema};
use arrow::ipc::reader::FileReader;
use arrow::row::{RowConverter, Rows, SortField};
use cpu_time::ProcessTime;
use parquet::arrow::ArrowWriter;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use xxhash_rust::xxh3::{Xxh3, xxh3_64};

use crate::cw::harness::{ProcResult, check_format, proc_status_bytes, start_peak_rss};
use crate::{R, location, open_strata, write_parquet, write_strata};

/// Rows per batch the merge reads and writes (plans/08-m2-writes-compaction.md § M2.2).
const BATCH_ROWS: usize = 8192;
/// The `pq_cetra` row-group rule of `write_parquet`: flush at 64 MiB in progress or 100,000 rows.
const ROW_GROUP_BYTES: usize = 64 << 20;
const ROW_GROUP_ROWS: usize = 100_000;
/// The columns the merge sorts by (cetra's order, spec 03 § 5).
const KEY_COLUMNS: [&str; 2] = ["trace_id", "start_time"];
/// The columns the answer hashes, in row order.
const ANSWER_COLUMNS: [&str; 3] = ["trace_id", "start_time", "span_id"];

/// One Parquet input of the merge: its reader, the batches it read since the last output flush
/// (the current one last), and the current batch's converted keys.
struct Input {
    reader: ParquetRecordBatchReader,
    batches: Vec<RecordBatch>,
    /// Index of the current batch in `batches`.
    cur: usize,
    /// The sort keys of the current batch, row-converted.
    keys: Rows,
}

impl Input {
    /// Makes the next non-empty batch of the reader the current one. False at the end.
    fn advance(&mut self, conv: &RowConverter) -> R<bool> {
        while let Some(batch) = self.reader.next() {
            let batch = batch?;
            if batch.num_rows() == 0 {
                continue;
            }
            self.keys = conv.convert_columns(&key_columns(&batch)?)?;
            self.batches.push(batch);
            self.cur = self.batches.len() - 1;
            return Ok(true);
        }
        Ok(false)
    }

    /// Rows in the current batch.
    fn cur_rows(&self) -> usize {
        self.batches[self.cur].num_rows()
    }

    /// Keeps only the current batch, which becomes batch 0.
    fn truncate_to_current(&mut self) {
        if let Some(last) = self.batches.pop() {
            self.batches.clear();
            self.batches.push(last);
        }
        self.cur = 0;
    }
}

/// The key columns of `batch`, in `KEY_COLUMNS` order.
fn key_columns(batch: &RecordBatch) -> R<Vec<ArrayRef>> {
    let mut cols = Vec::with_capacity(KEY_COLUMNS.len());
    for name in KEY_COLUMNS {
        let col = batch.column_by_name(name).ok_or_else(|| format!("no {name} column"))?;
        cols.push(col.clone());
    }
    Ok(cols)
}

/// The data type of column `name` in `schema`.
fn field_type(schema: &Schema, name: &str) -> R<DataType> {
    Ok(schema.field_with_name(name)?.data_type().clone())
}

/// Merges the Parquet files `inputs`, each sorted by `(trace_id, start_time)`, into one file at
/// `out` with cetra's Parquet settings, streaming in batches of 8,192 rows. The synchronous
/// reader loads one row group's column chunks per input at a time (`pq_cetra` row groups are up
/// to 64 MiB or 100,000 rows), so memory is about one row group per input, as cetra's streaming
/// merge holds one batch per input (spec 03 § 5). Rows with equal keys keep their input order,
/// then their row order. Returns (rows, bytes written).
pub fn merge_parquet(inputs: &[PathBuf], out: &Path) -> R<(u64, u64)> {
    let mut schema = None;
    let mut total_rows = 0u64;
    let mut readers = Vec::new();
    for path in inputs {
        let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
        let rows = u64::try_from(builder.metadata().file_metadata().num_rows())?;
        total_rows += rows;
        schema.get_or_insert_with(|| builder.schema().clone());
        if rows > 0 {
            readers.push(builder.with_batch_size(BATCH_ROWS).build()?);
        }
    }
    let schema = schema.ok_or("merge_parquet: no input files")?;
    let conv = RowConverter::new(vec![
        SortField::new(field_type(&schema, KEY_COLUMNS[0])?),
        SortField::new(field_type(&schema, KEY_COLUMNS[1])?),
    ])?;
    let mut ins: Vec<Input> = readers
        .into_iter()
        .map(|reader| Input { reader, batches: Vec::new(), cur: 0, keys: conv.empty_rows(0, 0) })
        .collect();
    // Ordered by key, then input index, then row: equal keys keep their input order.
    let mut heap = BinaryHeap::new();
    for (i, inp) in ins.iter_mut().enumerate() {
        if inp.advance(&conv)? {
            heap.push(Reverse((inp.keys.row(0).owned(), i, 0usize)));
        }
    }
    let props = crate::parquet_props("pq_cetra", &schema, usize::try_from(total_rows)?);
    let mut w = ArrowWriter::try_new(File::create(out)?, schema.clone(), Some(props))?;
    let mut pending: Vec<(usize, usize, usize)> = Vec::with_capacity(BATCH_ROWS);
    let mut written = 0u64;
    while let Some(Reverse((_, i, r))) = heap.pop() {
        // (input, batch index in `batches`, row in that batch)
        pending.push((i, ins[i].cur, r));
        let inp = &mut ins[i];
        if r + 1 < inp.cur_rows() {
            heap.push(Reverse((inp.keys.row(r + 1).owned(), i, r + 1)));
        } else if inp.advance(&conv)? {
            heap.push(Reverse((inp.keys.row(0).owned(), i, 0)));
        }
        if pending.len() == BATCH_ROWS || heap.is_empty() {
            written += write_pending(&mut pending, &mut ins, &mut w)?;
        }
    }
    w.close()?;
    Ok((written, std::fs::metadata(out)?.len()))
}

/// Writes the pending rows as one batch (`interleave_record_batch` over the batches they come
/// from), applies the row-group rule, then drops every input's batches but its current one.
/// Returns the rows written. `pending` is not empty.
fn write_pending(pending: &mut Vec<(usize, usize, usize)>, ins: &mut [Input], w: &mut ArrowWriter<File>) -> R<u64> {
    // The distinct (input, batch index) pairs of the pending rows, in order of first use.
    let mut slot_of: HashMap<(usize, usize), usize> = HashMap::new();
    let mut slots: Vec<(usize, usize)> = Vec::new();
    let mut picks: Vec<(usize, usize)> = Vec::with_capacity(pending.len());
    for &(i, b, r) in pending.iter() {
        let slot = *slot_of.entry((i, b)).or_insert_with(|| {
            slots.push((i, b));
            slots.len() - 1
        });
        picks.push((slot, r));
    }
    let refs: Vec<&RecordBatch> = slots.iter().map(|&(i, b)| &ins[i].batches[b]).collect();
    let batch = interleave_record_batch(&refs, &picks)?;
    w.write(&batch)?;
    if w.in_progress_size() >= ROW_GROUP_BYTES || w.in_progress_rows() >= ROW_GROUP_ROWS {
        w.flush()?;
    }
    pending.clear();
    for inp in ins.iter_mut() {
        inp.truncate_to_current();
    }
    Ok(batch.num_rows() as u64)
}

/// The files of `dir` with extension `ext`, sorted by name. A missing directory has none.
fn case_files(dir: &Path, ext: &str) -> R<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

/// The file extension of `format`'s output files.
fn ext_of(format: &str) -> &'static str {
    if format == "pq_cetra" { "parquet" } else { "strata" }
}

/// The `(trace_id, start_time, span_id)` columns of an answer, as typed arrays.
fn answer_arrays<'a>(
    trace: &'a dyn Array,
    start: &'a dyn Array,
    span: &'a dyn Array,
) -> R<(&'a FixedSizeBinaryArray, &'a TimestampNanosecondArray, &'a FixedSizeBinaryArray)> {
    let trace = trace.as_any().downcast_ref::<FixedSizeBinaryArray>().ok_or("trace_id is not fixed-size binary")?;
    let start = start.as_any().downcast_ref::<TimestampNanosecondArray>().ok_or("start_time is not a nanosecond timestamp")?;
    let span = span.as_any().downcast_ref::<FixedSizeBinaryArray>().ok_or("span_id is not fixed-size binary")?;
    Ok((trace, start, span))
}

/// Feeds the rows' `(trace_id, start_time, span_id)` bytes to `h`: 16, 8 and 8 bytes a row, the
/// start time little-endian. Returns the rows fed.
fn hash_rows(h: &mut Xxh3, trace: &dyn Array, start: &dyn Array, span: &dyn Array) -> R<u64> {
    let (trace, start, span) = answer_arrays(trace, start, span)?;
    for i in 0..trace.len() {
        h.update(trace.value(i));
        h.update(&start.value(i).to_le_bytes());
        h.update(span.value(i));
    }
    Ok(trace.len() as u64)
}

/// The answer of the Parquet file `path`: its rows and the hash of its rows in file order.
fn answer_parquet(path: &Path) -> R<(u64, u64)> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    let mut roots = Vec::with_capacity(ANSWER_COLUMNS.len());
    for name in ANSWER_COLUMNS {
        roots.push(builder.schema().index_of(name)?);
    }
    let mask = ProjectionMask::roots(builder.parquet_schema(), roots);
    let mut h = Xxh3::new();
    let mut rows = 0u64;
    for batch in builder.with_projection(mask).build()? {
        let b = batch?;
        let col = |name: &str| -> R<&ArrayRef> { b.column_by_name(name).ok_or_else(|| format!("no {name} column").into()) };
        rows += hash_rows(&mut h, col("trace_id")?.as_ref(), col("start_time")?.as_ref(), col("span_id")?.as_ref())?;
    }
    Ok((rows, h.digest()))
}

/// The answer of the single Strata file of `compact-st` in the local case `dir`: its rows and
/// the hash, read back column by column.
async fn answer_strata(dir: &str) -> R<(u64, u64)> {
    let loc = location(dir)?;
    let files = open_strata(&loc, "compact-st").await?;
    let [file] = files.as_slice() else {
        return Err(format!("compact-st holds {} files, want 1", files.len()).into());
    };
    let schema = file.schema();
    let mut cols = Vec::with_capacity(ANSWER_COLUMNS.len());
    for name in ANSWER_COLUMNS {
        cols.push(strata::read_column(file, schema.index_of(name)?).await?);
    }
    let mut h = Xxh3::new();
    let rows = hash_rows(&mut h, cols[0].as_ref(), cols[1].as_ref(), cols[2].as_ref())?;
    Ok((rows, h.digest()))
}

/// `compact` for `format` on the local case `dir`: merges `<dir>/<format>/*` into
/// `<dir>/compact-<format>/000.<ext>`, `runs` times. The `write` query must have run first.
/// Each run's output is removed and recreated before its clocks start; the clocks stop after
/// the output is synced (cetra's compaction makes each output durable, spec 03 § 5). The answer
/// reads the last output back, after `vmhwm` is read.
pub async fn compact_format(dir: &Path, format: &str, runs: usize) -> R<ProcResult> {
    check_format(format)?;
    if runs == 0 {
        return Err("runs must be at least 1".into());
    }
    let pq = format == "pq_cetra";
    let ext = ext_of(format);
    let inputs = case_files(&dir.join(format), ext)?;
    if inputs.is_empty() {
        return Err("run the write query first".into());
    }
    let dir_str = dir.to_str().ok_or("non-UTF-8 path")?;
    let out_dir = dir.join(format!("compact-{format}"));
    let out = out_dir.join(format!("000.{ext}"));
    let rss_start = start_peak_rss()?;
    let (mut runs_ms, mut cpu_ms) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    let (mut rows, mut bytes_written) = (0u64, 0u64);
    for _ in 0..runs {
        if out_dir.exists() {
            std::fs::remove_dir_all(&out_dir)?;
        }
        std::fs::create_dir_all(&out_dir)?;
        let cpu0 = ProcessTime::now();
        let t0 = Instant::now();
        if pq {
            rows = merge_parquet(&inputs, &out)?.0;
        } else {
            let files = open_strata(&location(dir_str)?, "st").await?;
            let (bytes, summary) = strata::merge(&files, &KEY_COLUMNS, strata::Config::default()).await?;
            std::fs::write(&out, &bytes)?;
            rows = summary.rows;
        }
        File::open(&out)?.sync_all()?;
        runs_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        cpu_ms.push(cpu0.elapsed().as_secs_f64() * 1e3);
        bytes_written = std::fs::metadata(&out)?.len();
    }
    let vmhwm = proc_status_bytes("VmHWM")?;
    let (answer_rows, hash) = if pq { answer_parquet(&out)? } else { answer_strata(dir_str).await? };
    if answer_rows != rows {
        return Err(format!("{format}: the output holds {answer_rows} rows, the merge wrote {rows}").into());
    }
    Ok(ProcResult {
        query: "compact".to_owned(),
        format: format.to_owned(),
        runs_ms,
        cpu_ms,
        gets: 0,
        bytes: 0,
        waves: None,
        rows,
        answer: format!("{hash:016x}"),
        rss_start,
        vmhwm,
        bytes_written: Some(bytes_written),
        tail_bytes: None,
    })
}

/// The one batch `cw flush-input` wrote to `<dir>/flush.arrow`.
fn read_flush_input(dir: &Path) -> R<RecordBatch> {
    let path = dir.join("flush.arrow");
    if !path.is_file() {
        return Err("run cw flush-input first".into());
    }
    let reader = FileReader::try_new(File::open(&path)?, None)?;
    let schema = reader.schema();
    let batches = reader.collect::<Result<Vec<_>, _>>()?;
    Ok(concat_batches(&schema, &batches)?)
}

/// Writes `batch` as `writers` files `w00.<ext>`, … in `out_dir` at once, one thread each, and
/// syncs each. Returns each file's size.
fn write_concurrently(batch: &RecordBatch, out_dir: &Path, format: &str, writers: usize) -> R<Vec<u64>> {
    let ext = ext_of(format);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let path = out_dir.join(format!("w{w:02}.{ext}"));
                s.spawn(move || -> R<u64> {
                    let len = if format == "pq_cetra" { write_parquet(format, batch, &path)? } else { write_strata(batch, &path)?.0 };
                    File::open(&path)?.sync_all()?;
                    Ok(len)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().map_err(|_| -> Box<dyn std::error::Error + Send + Sync> { "a writer thread panicked".into() })?)
            .collect()
    })
}

/// `flush:<writers>` for `format` on the local case `dir`: `writers` threads each write the
/// batch of `<dir>/flush.arrow` (M2.1's `flush-input`) to `<dir>/flush-<format>/wNN.<ext>` at
/// once, `runs` times. Every writer writes the same rows; formats keep no state across files, so
/// this costs what `writers` different projects' flushes would. Each run's clocks cover the
/// writes and their syncs; `vmhwm` is read after the runs, and the output is removed after that.
pub fn flush_format(dir: &Path, format: &str, writers: usize, runs: usize) -> R<ProcResult> {
    check_format(format)?;
    if writers == 0 || runs == 0 {
        return Err("writers and runs must be at least 1".into());
    }
    let batch = read_flush_input(dir)?;
    let out_dir = dir.join(format!("flush-{format}"));
    let rss_start = start_peak_rss()?;
    let (mut runs_ms, mut cpu_ms) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    let mut bytes_written = 0u64;
    for _ in 0..runs {
        if out_dir.exists() {
            std::fs::remove_dir_all(&out_dir)?;
        }
        std::fs::create_dir_all(&out_dir)?;
        let cpu0 = ProcessTime::now();
        let t0 = Instant::now();
        let sizes = write_concurrently(&batch, &out_dir, format, writers)?;
        runs_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        cpu_ms.push(cpu0.elapsed().as_secs_f64() * 1e3);
        bytes_written = sizes.iter().sum();
    }
    let vmhwm = proc_status_bytes("VmHWM")?;
    std::fs::remove_dir_all(&out_dir)?;
    Ok(ProcResult {
        query: format!("flush:{writers}"),
        format: format.to_owned(),
        runs_ms,
        cpu_ms,
        gets: 0,
        bytes: 0,
        waves: None,
        rows: batch.num_rows() as u64 * writers as u64,
        answer: format!("{:016x}", xxh3_64(b"")),
        rss_start,
        vmhwm,
        bytes_written: Some(bytes_written),
        tail_bytes: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cw::cases;
    use crate::cw::harness::write_format;
    use arrow::array::UInt32Array;
    use arrow::compute::{SortColumn, lexsort_to_indices, take_record_batch};
    use cetra_core::batch::spans_to_record_batch;
    use cetra_core::ids::SpanId;
    use cetra_core::span::Span;
    use fmt_testdata::datagen::{Rng, World};
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder as Reader;

    /// A clean directory for a test, under the temp directory.
    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cw-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `spans` as one batch sorted by (trace_id, start_time).
    fn sorted_batch(spans: &[Span]) -> R<RecordBatch> {
        let batch = spans_to_record_batch(spans)?;
        let order = lexsort_to_indices(
            &[
                SortColumn { values: batch.column_by_name("trace_id").ok_or("no trace_id column")?.clone(), options: None },
                SortColumn { values: batch.column_by_name("start_time").ok_or("no start_time column")?.clone(), options: None },
            ],
            None,
        )?;
        Ok(take_record_batch(&batch, &order)?)
    }

    /// Writes `spans` as `n` Parquet files `NNN.parquet`: slices of consecutive start times, each
    /// sorted by (trace_id, start_time), as a case's input files are.
    fn pq_slices(dir: &Path, mut spans: Vec<Span>, n: usize) -> R<Vec<PathBuf>> {
        spans.sort_by_key(|s| s.start_time);
        let per = spans.len().div_ceil(n).max(1);
        let mut paths = Vec::new();
        for (i, slice) in spans.chunks(per).enumerate() {
            let path = dir.join(format!("{i:03}.parquet"));
            write_parquet("pq_cetra", &sorted_batch(slice)?, &path)?;
            paths.push(path);
        }
        Ok(paths)
    }

    /// The rows of a Parquet file as (trace_id, start_time, span_id), in file order.
    fn rows_of(path: &Path) -> R<Vec<(Vec<u8>, i64, Vec<u8>)>> {
        let mut out = Vec::new();
        for batch in Reader::try_new(File::open(path)?)?.build()? {
            let b = batch?;
            let col = |name: &str| -> R<&ArrayRef> { b.column_by_name(name).ok_or_else(|| format!("no {name} column").into()) };
            let (trace, start, span) = answer_arrays(col("trace_id")?.as_ref(), col("start_time")?.as_ref(), col("span_id")?.as_ref())?;
            for i in 0..b.num_rows() {
                out.push((trace.value(i).to_vec(), start.value(i), span.value(i).to_vec()));
            }
        }
        Ok(out)
    }

    /// Whether the rows are sorted by (trace_id, start_time).
    fn is_sorted(rows: &[(Vec<u8>, i64, Vec<u8>)]) -> bool {
        rows.windows(2).all(|w| (&w[0].0, w[0].1) <= (&w[1].0, w[1].1))
    }

    /// The sorted (trace_id, span_id) pairs of the rows.
    fn pairs(rows: &[(Vec<u8>, i64, Vec<u8>)]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut v: Vec<(Vec<u8>, Vec<u8>)> = rows.iter().map(|r| (r.0.clone(), r.2.clone())).collect();
        v.sort();
        v
    }

    /// Writes `spans` as the Parquet file `name` in `dir`, sorted, as `pq_slices` does for one file.
    fn pq_file(dir: &Path, name: &str, spans: &[Span]) -> R<PathBuf> {
        let path = dir.join(name);
        write_parquet("pq_cetra", &sorted_batch(spans)?, &path)?;
        Ok(path)
    }

    #[test]
    fn merge_parquet_merges_sorted_files() {
        let dir = temp_dir("merge_sorted");
        let world = World::new(3);
        let mut rng = Rng::new(1);
        let mut spans = Vec::new();
        for _ in 0..30 {
            world.trace(&mut rng, &mut spans);
        }
        let inputs = pq_slices(&dir, spans.clone(), 3).unwrap();
        let out = dir.join("out.parquet");
        let (rows, _) = merge_parquet(&inputs, &out).unwrap();
        let merged = rows_of(&out).unwrap();
        assert_eq!(rows as usize, spans.len());
        assert_eq!(merged.len(), spans.len());
        assert!(is_sorted(&merged));
        let mut want = Vec::new();
        for p in &inputs {
            want.extend(rows_of(p).unwrap());
        }
        assert_eq!(pairs(&merged), pairs(&want));
    }

    #[test]
    fn merge_parquet_keeps_input_order_on_ties() {
        let dir = temp_dir("merge_ties");
        let world = World::new(5);
        let mut rng = Rng::new(9);
        let mut spans = Vec::new();
        world.trace(&mut rng, &mut spans);
        let a = spans[0].clone();
        let mut b = a.clone();
        b.span_id = SpanId::from_bytes([7u8; 8]).unwrap();
        let p0 = pq_file(&dir, "000.parquet", &[a]).unwrap();
        let p1 = pq_file(&dir, "001.parquet", &[b]).unwrap();
        let out = dir.join("out.parquet");
        merge_parquet(&[p0.clone(), p1], &out).unwrap();
        let merged = rows_of(&out).unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].2, rows_of(&p0).unwrap()[0].2);
        assert_eq!(merged[0].0, merged[1].0);
        assert_eq!(merged[0].1, merged[1].1);
    }

    #[test]
    fn merge_parquet_crosses_batch_boundaries() {
        let dir = temp_dir("merge_cross");
        let world = World::new(11);
        let mut rng = Rng::new(12);
        let mut spans = Vec::new();
        while spans.len() < 40_000 {
            world.trace(&mut rng, &mut spans);
        }
        spans.truncate(40_000);
        for s in &mut spans {
            crate::strip_payloads(s);
        }
        // Alternate the sorted rows between two inputs, so the keys interleave.
        let sorted = sorted_batch(&spans).unwrap();
        let take = |rows: Vec<usize>| -> R<RecordBatch> {
            let idx = UInt32Array::from_iter_values(rows.into_iter().map(|r| r as u32));
            Ok(take_record_batch(&sorted, &idx)?)
        };
        let n = sorted.num_rows();
        let even = take((0..n).step_by(2).collect()).unwrap();
        let odd = take((1..n).step_by(2).collect()).unwrap();
        let p0 = dir.join("000.parquet");
        let p1 = dir.join("001.parquet");
        write_parquet("pq_cetra", &even, &p0).unwrap();
        write_parquet("pq_cetra", &odd, &p1).unwrap();
        let out = dir.join("out.parquet");
        let (rows, _) = merge_parquet(&[p0.clone(), p1.clone()], &out).unwrap();
        assert_eq!(rows as usize, n);
        let merged = rows_of(&out).unwrap();
        assert!(is_sorted(&merged));
        let mut want = rows_of(&p0).unwrap();
        want.extend(rows_of(&p1).unwrap());
        assert_eq!(pairs(&merged), pairs(&want));
    }

    #[test]
    fn merge_parquet_streams() {
        let dir = temp_dir("merge_streams");
        let world = World::new(21);
        let mut rng = Rng::new(22);
        let mut spans = Vec::new();
        while spans.len() < 120_000 {
            world.trace(&mut rng, &mut spans);
        }
        spans.truncate(120_000);
        for s in &mut spans {
            crate::strip_payloads(s);
        }
        let mut inputs = Vec::new();
        for (i, chunk) in spans.chunks(30_000).enumerate() {
            inputs.push(pq_file(&dir, &format!("{i:03}.parquet"), chunk).unwrap());
        }
        let out = dir.join("out.parquet");
        let (rows, _) = merge_parquet(&inputs, &out).unwrap();
        assert_eq!(rows, 120_000);
        let groups = Reader::try_new(File::open(&out).unwrap()).unwrap().metadata().num_row_groups();
        assert!(groups >= 2, "row groups: {groups}");
    }

    #[test]
    fn merge_parquet_skips_empty_inputs() {
        let dir = temp_dir("merge_empty");
        let world = World::new(31);
        let mut rng = Rng::new(32);
        let mut spans = Vec::new();
        for _ in 0..10 {
            world.trace(&mut rng, &mut spans);
        }
        let half = spans.len() / 2;
        let a = pq_file(&dir, "000.parquet", &spans[..half]).unwrap();
        let empty = pq_file(&dir, "001.parquet", &[]).unwrap();
        let b = pq_file(&dir, "002.parquet", &spans[half..]).unwrap();
        let out = dir.join("out.parquet");
        let (rows, _) = merge_parquet(&[a.clone(), empty, b.clone()], &out).unwrap();
        assert_eq!(rows as usize, spans.len());
        let merged = rows_of(&out).unwrap();
        let mut want = rows_of(&a).unwrap();
        want.extend(rows_of(&b).unwrap());
        assert_eq!(pairs(&merged), pairs(&want));
    }

    #[tokio::test]
    async fn compact_agrees_across_formats() {
        let dir = temp_dir("compact_agrees");
        let (spans, files) = cases::gen_spans("text_unique", Some(60)).unwrap();
        cases::write_case(&dir, spans, &files).unwrap();
        for format in ["pq_cetra", "st"] {
            write_format(&dir, format, 1).unwrap();
        }
        let pq = compact_format(&dir, "pq_cetra", 1).await.unwrap();
        let st = compact_format(&dir, "st", 1).await.unwrap();
        assert_eq!(pq.rows, st.rows);
        assert_eq!(pq.answer, st.answer);
        assert!(dir.join("compact-pq_cetra/000.parquet").is_file());
        assert!(dir.join("compact-st/000.strata").is_file());
    }

    #[test]
    fn flush_writes_one_file_per_writer() {
        let dir = temp_dir("flush_one_file");
        let (spans, files) = cases::gen_spans("text_unique", Some(200)).unwrap();
        cases::write_case(&dir, spans, &files).unwrap();
        cases::flush_input_cmd(&[dir.to_str().unwrap().to_owned()]).unwrap();
        let batch = read_flush_input(&dir).unwrap();
        let single = dir.join("single");
        for format in ["pq_cetra", "st"] {
            let size = if format == "pq_cetra" {
                write_parquet(format, &batch, &single).unwrap()
            } else {
                write_strata(&batch, &single).unwrap().0
            };
            let r = flush_format(&dir, format, 3, 1).unwrap();
            assert_eq!(r.bytes_written, Some(3 * size), "{format} bytes_written");
            assert_eq!(r.rows, 3 * batch.num_rows() as u64, "{format} rows");
            assert!(!dir.join(format!("flush-{format}")).exists(), "{format} output removed");
        }
    }
}
