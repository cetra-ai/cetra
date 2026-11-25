//! `fmtbench cw` many-file queries (plans/08-m3-many-files.md § M3.2): the M3 queries on a project
//! directory written by `cw project`, on cetra's Parquet files and on their Strata conversions.
//! Both formats run the same algorithm with the same parallelism and the same answer encoding.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{Array, FixedSizeBinaryArray, Float64Array, Int64Array, RecordBatch, Scalar, StringArray};
use arrow::compute::{cast, kernels::cmp::eq};
use arrow::datatypes::DataType;
use arrow::error::ArrowError;
use cpu_time::ProcessTime;
use futures::future::try_join_all;
use futures::{StreamExt, TryStreamExt};
use object_store::path::Path as ObjPath;
use object_store::{ObjectMeta, ObjectStore};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowPredicate, ArrowPredicateFn, RowFilter};
use parquet::arrow::async_reader::{ParquetObjectReader, ParquetRecordBatchStreamBuilder};
use parquet::file::statistics::Statistics;
use parquet::schema::types::SchemaDescriptor;
use strata::{CmpOp, Lit, Predicate, ScanKind, ScanRequest, StrataFile};
use xxhash_rust::xxh3::xxh3_64;

use crate::cw::read::Answer;
use crate::pqfast::{self, PqFile};
use crate::{Location, R, parse_id, read_text};

/// Nanoseconds per UTC day and per hour.
const DAY_NS: i64 = 86_400_000_000_000;
const HOUR_NS: i64 = 3_600_000_000_000;
/// Rows per Strata rollup range (one unit of work), and per Parquet batch.
const ROLLUP_RANGE_ROWS: u64 = 100_000;
const BATCH_ROWS: usize = 8_192;
/// `anchor_rebuild` keeps the `traces` rows last ingested within this window before the clock,
/// and whose start or end is within `MIDNIGHT_NS` of a UTC midnight (spec 03 § Trace anchor).
const ANCHOR_WINDOW_NS: i64 = 2 * HOUR_NS;
const MIDNIGHT_NS: i64 = HOUR_NS;
/// The `span_kind` the rollup keeps, and the columns it reads.
const LLM: &str = "llm";
const ROLLUP_COLUMNS: [&str; 3] = ["request_model", "cost_total", "total_tokens"];
/// The `traces` columns `anchor_rebuild` reads.
const ANCHOR_COLUMNS: [&str; 4] = ["trace_id", "start_time", "end_time", "last_ingest_time"];

/// A project's files of one format, per table, listed once per process (see `list`).
#[derive(Debug, Clone)]
pub(crate) struct Listing {
    pub spans: Vec<FileRef>,
    pub traces: Vec<FileRef>,
    pub scores: Vec<FileRef>,
}

/// One data file: its path, size, and day as the `YYYY-MM-DD` of its `day=…` path segment.
#[derive(Debug, Clone)]
pub(crate) struct FileRef {
    pub path: ObjPath,
    pub size: u64,
    pub day: String,
}

/// Opened files of one table.
pub(crate) enum Opened {
    Parquet(Vec<PqFile>),
    Strata(Vec<Arc<StrataFile>>),
}

/// The text files of a project: `cw_traces.txt`, `cw_around.txt`, `cw_clock.txt`.
pub(crate) struct ProjectCase {
    /// The trace ids, in file order.
    pub traces: Vec<[u8; 16]>,
    /// Each trace's root `start_time`, nanoseconds, in the same order.
    pub around: Vec<i64>,
    /// The largest span `ingest_time` written, nanoseconds: the anchor rebuild's clock.
    pub clock: i64,
}

/// Whether `query` is one of the M3 queries.
pub(crate) fn is_many(query: &str) -> bool {
    query.starts_with("open:")
        || matches!(query, "lookup_cold" | "lookup_around" | "rollup_30d" | "rollup_30d_cold" | "anchor_rebuild")
}

/// Cold queries open their files inside each run and take no warm-up (CW-24); only `rollup_30d`
/// keeps its `spans` files open.
pub(crate) fn is_cold(query: &str) -> bool {
    query != "rollup_30d"
}

/// The directory and object suffix of a format's data files.
fn format_dir(format: &str) -> R<(&'static str, &'static str)> {
    match format {
        "pq_cetra" => Ok(("pq_cetra", ".parquet")),
        "st" => Ok(("st", ".strata")),
        _ => Err(format!("unknown format {format}: want pq_cetra or st").into()),
    }
}

/// A project's files of one format, per table, as the location's store lists them. Format
/// `pq_cetra` lists `*.parquet` under `pq_cetra/<table>/`, format `st` lists `*.strata` under
/// `st/<table>/`. Sorted by path.
pub(crate) async fn list(loc: &Location, format: &str) -> R<Listing> {
    let (dir, ext) = format_dir(format)?;
    Ok(Listing {
        spans: table_files(loc, dir, ext, "spans").await?,
        traces: table_files(loc, dir, ext, "traces").await?,
        scores: table_files(loc, dir, ext, "scores").await?,
    })
}

/// The files of one table of `dir` with suffix `ext`.
async fn table_files(loc: &Location, dir: &str, ext: &str, table: &str) -> R<Vec<FileRef>> {
    let prefix = loc.prefix.clone().join(dir).join(table);
    let mut metas: Vec<ObjectMeta> = loc.store.list(Some(&prefix)).try_collect().await?;
    metas.retain(|m| m.location.as_ref().ends_with(ext));
    metas.sort_by(|a, b| a.location.cmp(&b.location));
    metas
        .into_iter()
        .map(|m| -> R<FileRef> {
            let day = m
                .location
                .as_ref()
                .split('/')
                .find_map(|p| p.strip_prefix("day="))
                .ok_or_else(|| format!("{} has no day= segment", m.location))?
                .to_owned();
            Ok(FileRef { path: m.location, size: m.size, day })
        })
        .collect()
}

/// Opens `files` (one table's files of `format`), all at once: Parquet with `pqfast::open`, Strata
/// with `StrataFile::open` and `crate::strata_config`, no tail hint.
pub(crate) async fn open(loc: &Location, format: &str, files: &[FileRef]) -> R<Opened> {
    let store: Arc<dyn ObjectStore> = loc.store.clone();
    match format {
        "pq_cetra" => {
            let v = try_join_all(files.iter().map(|f| pqfast::open(store.clone(), f.path.clone()))).await?;
            Ok(Opened::Parquet(v))
        }
        "st" => {
            let cfg = crate::strata_config();
            let v = try_join_all(files.iter().map(|f| StrataFile::open(store.clone(), f.path.clone(), None, &cfg))).await?;
            Ok(Opened::Strata(v.into_iter().map(Arc::new).collect()))
        }
        _ => Err(format!("unknown format {format}: want pq_cetra or st").into()),
    }
}

/// The non-empty trimmed lines of a text file; a missing file is an error.
fn text_lines(text: Option<String>, name: &str) -> R<Vec<String>> {
    let text = text.ok_or_else(|| format!("no {name} in the case"))?;
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

/// Reads `cw_traces.txt`, `cw_around.txt` and `cw_clock.txt` of the project through `loc`.
pub(crate) async fn read_project_case(loc: &Location) -> R<ProjectCase> {
    let mut traces = Vec::new();
    for l in text_lines(read_text(loc, "cw_traces.txt").await?, "cw_traces.txt")? {
        traces.push(parse_id(&l)?);
    }
    let mut around = Vec::new();
    for l in text_lines(read_text(loc, "cw_around.txt").await?, "cw_around.txt")? {
        around.push(l.parse::<i64>()?);
    }
    let clock_line = text_lines(read_text(loc, "cw_clock.txt").await?, "cw_clock.txt")?;
    let clock = clock_line.first().ok_or("cw_clock.txt is empty")?.parse::<i64>()?;
    if around.len() != traces.len() {
        return Err(format!("cw_around.txt has {} lines, cw_traces.txt {}", around.len(), traces.len()).into());
    }
    Ok(ProjectCase { traces, around, clock })
}

/// One M3 query's answer and, for the lookups, one (wall ms, CPU ms) per id. The other queries
/// return no per-id times: the caller times the whole run.
///
/// `warm` holds the opened `spans` files for `rollup_30d`; it is `None` for the cold queries,
/// which open what they read inside the run. The answer's bytes are per query:
/// - `lookup_cold`, `lookup_around`: for each id in order, its `span_id`s sorted, 8 bytes each
///   (rows = their number);
/// - `rollup_30d`, `rollup_30d_cold`: per `request_model` group sorted by name, the name's bytes,
///   `0x00`, count `u64`, cost micro-units `i64`, tokens `i64`, little-endian (rows = groups);
/// - `open:<table>`: the file count, `u64` (rows = the count);
/// - `anchor_rebuild`: the entries' `trace_id`s sorted, 16 bytes each (rows = entries).
pub(crate) async fn run(
    query: &str,
    format: &str,
    loc: &Location,
    listing: &Listing,
    warm: Option<&Opened>,
    case: &ProjectCase,
) -> R<(Answer, Vec<(f64, f64)>)> {
    match query {
        "lookup_cold" | "lookup_around" => lookups(query == "lookup_around", format, loc, listing, case).await,
        "rollup_30d" | "rollup_30d_cold" => Ok((rollup(format, loc, listing, warm).await?, Vec::new())),
        "anchor_rebuild" => Ok((anchor(format, loc, listing, case).await?, Vec::new())),
        _ => match query.strip_prefix("open:") {
            Some(table) => Ok((open_count(format, loc, files_of(listing, table)?).await?, Vec::new())),
            None => Err(format!("unknown many-file query {query}").into()),
        },
    }
}

/// The answer of `bytes`, which encodes `rows` rows.
fn answer_of(bytes: &[u8], rows: u64) -> Answer {
    Answer { rows, hash: xxh3_64(bytes) }
}

/// The files of `table` in `listing`.
fn files_of<'a>(listing: &'a Listing, table: &str) -> R<&'a [FileRef]> {
    match table {
        "spans" => Ok(&listing.spans),
        "traces" => Ok(&listing.traces),
        "scores" => Ok(&listing.scores),
        _ => Err(format!("unknown table {table}: want spans, traces or scores").into()),
    }
}

/// `open:<table>`: opens every file and keeps them open until it returns.
async fn open_count(format: &str, loc: &Location, files: &[FileRef]) -> R<Answer> {
    let opened = open(loc, format, files).await?;
    let count = match &opened {
        Opened::Parquet(v) => v.len(),
        Opened::Strata(v) => v.len(),
    };
    let n = count as u64;
    Ok(answer_of(&n.to_le_bytes(), n))
}

/// `lookup_cold` (every spans file) or `lookup_around` (the spans files within a day of the id's
/// root start): each id in turn, timed alone.
async fn lookups(around: bool, format: &str, loc: &Location, listing: &Listing, case: &ProjectCase) -> R<(Answer, Vec<(f64, f64)>)> {
    let mut rows = 0u64;
    let mut bytes = Vec::new();
    let mut times = Vec::with_capacity(case.traces.len());
    for (id, at) in case.traces.iter().zip(&case.around) {
        let files: Vec<FileRef> = if around {
            let near = days_near(*at);
            listing.spans.iter().filter(|f| near.contains(&f.day)).cloned().collect()
        } else {
            listing.spans.clone()
        };
        let t0 = Instant::now();
        let cpu0 = ProcessTime::now();
        let spans = find_spans(loc, format, &files, id).await?;
        times.push((t0.elapsed().as_secs_f64() * 1e3, cpu0.elapsed().as_secs_f64() * 1e3));
        rows += spans.len() as u64;
        for s in &spans {
            bytes.extend_from_slice(s);
        }
    }
    Ok((answer_of(&bytes, rows), times))
}

/// The UTC day texts of the day of `ns` and the days either side of it.
fn days_near(ns: i64) -> Vec<String> {
    let d = ns.div_euclid(DAY_NS);
    (d - 1..=d + 1).map(day_text).collect()
}

/// `YYYY-MM-DD` of the UTC day `z` days after 1970-01-01 (civil calendar, proleptic Gregorian).
fn day_text(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The index of the leaf column named `name` in a Parquet schema.
fn leaf_of(schema: &SchemaDescriptor, name: &str) -> R<usize> {
    schema
        .columns()
        .iter()
        .position(|c| c.path().string() == name)
        .ok_or_else(|| format!("no {name} column").into())
}

/// Whether the Parquet file may hold trace `id`: a row group whose `trace_id` min/max contain it
/// and whose bloom filter may contain it (an absent filter may). A row group the footer rules out
/// costs no read.
async fn parquet_may_hold(f: &PqFile, id: &[u8; 16]) -> R<bool> {
    let md = f.meta.metadata();
    let leaf = leaf_of(f.meta.parquet_schema(), "trace_id")?;
    let mut builder = ParquetRecordBatchStreamBuilder::new_with_metadata(
        ParquetObjectReader::new(f.store.clone(), f.path.clone()),
        f.meta.clone(),
    );
    for rg in 0..md.num_row_groups() {
        if let Some(Statistics::FixedLenByteArray(s)) = md.row_group(rg).column(leaf).statistics() {
            if let (Some(min), Some(max)) = (s.min_bytes_opt(), s.max_bytes_opt()) {
                if id.as_slice() < min || id.as_slice() > max {
                    continue;
                }
            }
        }
        match builder.get_row_group_column_bloom_filter(rg, leaf).await? {
            Some(filter) if !filter.check(&id[..]) => continue,
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// The span ids of trace `id` in `files` (one table's files of `format`), sorted. Each file is
/// opened, then read only if it may hold the id; Strata's `may_contain` gives the answer `Some(false)`
/// for a proven absence.
async fn find_spans(loc: &Location, format: &str, files: &[FileRef], id: &[u8; 16]) -> R<Vec<[u8; 8]>> {
    let opened = open(loc, format, files).await?;
    let batches: Vec<RecordBatch> = match &opened {
        Opened::Parquet(v) => {
            let may = try_join_all(v.iter().map(|f| parquet_may_hold(f, id))).await?;
            let holders: Vec<&PqFile> = v.iter().zip(may).filter(|(_, m)| *m).map(|(f, _)| f).collect();
            let per = try_join_all(holders.into_iter().map(|f| {
                let all = all_columns(f.meta.schema().fields().len());
                async move { pqfast::lookup(f, id, &all).await }
            }))
            .await?;
            per.into_iter().flatten().collect()
        }
        Opened::Strata(v) => {
            let may = try_join_all(v.iter().map(|f| f.may_contain("trace_id", id))).await?;
            let holders: Vec<&Arc<StrataFile>> = v.iter().zip(may).filter(|(_, m)| *m != Some(false)).map(|(f, _)| f).collect();
            let per = try_join_all(holders.into_iter().map(|f| {
                let all = all_columns(f.schema().fields().len());
                async move { f.lookup_scan(id, &all, false).await }
            }))
            .await?;
            per.into_iter().flatten().collect()
        }
    };
    let mut spans = Vec::new();
    for b in &batches {
        let col = column::<FixedSizeBinaryArray>(b, "span_id")?;
        for row in 0..col.len() {
            let span: [u8; 8] = col.value(row).try_into()?;
            spans.push(span);
        }
    }
    spans.sort_unstable();
    Ok(spans)
}

/// Every column index of a schema with `n` fields.
fn all_columns(n: usize) -> Vec<usize> {
    (0..n).collect()
}

/// The column `name` of `batch`, downcast to `T`.
fn column<'a, T: Array + 'static>(batch: &'a RecordBatch, name: &str) -> R<&'a T> {
    let c = batch.column_by_name(name).ok_or_else(|| format!("no {name} column"))?;
    c.as_any().downcast_ref::<T>().ok_or_else(|| format!("{name} has another type").into())
}

/// The `Group` of one `request_model`: rows, cost in micro-units, total tokens.
#[derive(Default)]
struct Group {
    count: u64,
    cost: i64,
    tokens: i64,
}

/// The rollup's groups by `request_model` (a null model is `""`).
type Groups = BTreeMap<String, Group>;

/// One unit of the rollup: a Parquet row group, or a range of rows of a Strata file.
enum RollUnit<'a> {
    Parquet(&'a PqFile, usize),
    Strata(&'a StrataFile, Range<u64>),
}

/// Ranges of `ROLLUP_RANGE_ROWS` rows covering `rows` rows.
fn rollup_ranges(rows: u64) -> Vec<Range<u64>> {
    let mut v = Vec::new();
    let mut s = 0;
    while s < rows {
        v.push(s..(s + ROLLUP_RANGE_ROWS).min(rows));
        s += ROLLUP_RANGE_ROWS;
    }
    v
}

/// `a + b` for the integer sums, which an overflow fails.
fn add(a: i64, b: i64) -> R<i64> {
    Ok(a.checked_add(b).ok_or("rollup sum overflows")?)
}

/// The rows with `span_kind = 'llm'` of a batch whose column 0 is `span_kind`.
fn llm_mask(batch: &RecordBatch) -> Result<arrow::array::BooleanArray, ArrowError> {
    let kinds = batch
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| ArrowError::InvalidArgumentError("span_kind is not Utf8".into()))?;
    eq(kinds, &Scalar::new(StringArray::from(vec![LLM])))
}

/// Adds the rows of a batch of the rollup's columns to `out`.
fn fold_rollup(batch: &RecordBatch, out: &mut Groups) -> R<()> {
    let model = column::<StringArray>(batch, "request_model")?;
    let cost = column::<Float64Array>(batch, "cost_total")?;
    let tokens = column::<Int64Array>(batch, "total_tokens")?;
    for row in 0..batch.num_rows() {
        let name = if model.is_valid(row) { model.value(row) } else { "" };
        if !out.contains_key(name) {
            out.insert(name.to_owned(), Group::default());
        }
        let g = out.get_mut(name).ok_or("rollup group vanished")?;
        g.count += 1;
        if cost.is_valid(row) {
            g.cost = add(g.cost, (cost.value(row) * 1e6).round() as i64)?;
        }
        if tokens.is_valid(row) {
            g.tokens = add(g.tokens, tokens.value(row))?;
        }
    }
    Ok(())
}

/// Reads one rollup unit and folds its rows.
async fn roll_unit(unit: RollUnit<'_>) -> R<Groups> {
    let mut out = Groups::new();
    match unit {
        RollUnit::Parquet(f, rg) => {
            let schema = f.meta.schema();
            let cols: Vec<usize> = ROLLUP_COLUMNS.iter().map(|n| schema.index_of(n)).collect::<Result<_, _>>()?;
            let kind = schema.index_of("span_kind")?;
            let predicate = ArrowPredicateFn::new(ProjectionMask::roots(f.meta.parquet_schema(), [kind]), |batch: RecordBatch| {
                llm_mask(&batch)
            });
            let predicates: Vec<Box<dyn ArrowPredicate>> = vec![Box::new(predicate)];
            let mut stream = ParquetRecordBatchStreamBuilder::new_with_metadata(
                ParquetObjectReader::new(f.store.clone(), f.path.clone()),
                f.meta.clone(),
            )
            .with_batch_size(BATCH_ROWS)
            .with_row_groups(vec![rg])
            .with_projection(ProjectionMask::roots(f.meta.parquet_schema(), cols))
            .with_row_filter(RowFilter::new(predicates))
            .build()?;
            while let Some(batch) = stream.try_next().await? {
                fold_rollup(&batch, &mut out)?;
            }
        }
        RollUnit::Strata(f, rows) => {
            let schema = f.schema();
            let cols: Vec<usize> = ROLLUP_COLUMNS.iter().map(|n| schema.index_of(n)).collect::<Result<_, _>>()?;
            let kind = schema.index_of("span_kind")?;
            let req = ScanRequest {
                projection: cols,
                predicate: Some(Predicate::Cmp(kind, CmpOp::Eq, Lit::Utf8(LLM.to_owned()))),
                row_ranges: Some(vec![u32::try_from(rows.start)?..u32::try_from(rows.end)?]),
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
                row_filter: None,
            };
            for batch in f.scan_batches(&req, 1).await? {
                fold_rollup(&batch?, &mut out)?;
            }
        }
    }
    Ok(out)
}

/// `rollup_30d` (on `warm`'s files) and `rollup_30d_cold` (on files opened here): the LLM spans
/// of every `spans` file, grouped by `request_model`. Units run `cores` at a time; their groups are
/// merged with integer sums, so the order does not matter.
async fn rollup(format: &str, loc: &Location, listing: &Listing, warm: Option<&Opened>) -> R<Answer> {
    let owned: Opened;
    let opened: &Opened = match warm {
        Some(o) => o,
        None => {
            owned = open(loc, format, &listing.spans).await?;
            &owned
        }
    };
    let mut units = Vec::new();
    match opened {
        Opened::Parquet(v) => {
            for f in v {
                for rg in 0..f.meta.metadata().num_row_groups() {
                    units.push(RollUnit::Parquet(f, rg));
                }
            }
        }
        Opened::Strata(v) => {
            for f in v {
                for rows in rollup_ranges(f.row_count()) {
                    units.push(RollUnit::Strata(f, rows));
                }
            }
        }
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let parts: Vec<Groups> = futures::stream::iter(units).map(roll_unit).buffer_unordered(cores).try_collect().await?;
    let mut total = Groups::new();
    for part in parts {
        for (name, g) in part {
            let t = total.entry(name).or_default();
            t.count = t.count.checked_add(g.count).ok_or("rollup count overflows")?;
            t.cost = add(t.cost, g.cost)?;
            t.tokens = add(t.tokens, g.tokens)?;
        }
    }
    let mut bytes = Vec::new();
    for (name, g) in &total {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&g.count.to_le_bytes());
        bytes.extend_from_slice(&g.cost.to_le_bytes());
        bytes.extend_from_slice(&g.tokens.to_le_bytes());
    }
    Ok(answer_of(&bytes, total.len() as u64))
}

/// Whether `t` (ns) is within `MIDNIGHT_NS` of a UTC midnight.
fn near_midnight(t: i64) -> bool {
    let r = t.rem_euclid(DAY_NS);
    r <= MIDNIGHT_NS || DAY_NS - r <= MIDNIGHT_NS
}

/// The column `name` of `batch` as `i64` values (a timestamp column is cast).
fn int64_col(batch: &RecordBatch, name: &str) -> R<Int64Array> {
    let c = batch.column_by_name(name).ok_or_else(|| format!("no {name} column"))?;
    let casted = cast(c.as_ref(), &DataType::Int64)?;
    casted.as_any().downcast_ref::<Int64Array>().cloned().ok_or_else(|| format!("{name} is not an integer column").into())
}

/// Appends the `trace_id`s of the rows of `batch` that `anchor_rebuild` keeps to `out`.
fn anchor_rows(batch: &RecordBatch, threshold: i64, out: &mut Vec<[u8; 16]>) -> R<()> {
    let traces = column::<FixedSizeBinaryArray>(batch, "trace_id")?;
    let start = int64_col(batch, "start_time")?;
    let end = int64_col(batch, "end_time")?;
    let last = int64_col(batch, "last_ingest_time")?;
    for row in 0..batch.num_rows() {
        if !last.is_valid(row) || last.value(row) < threshold {
            continue;
        }
        let near = |a: &Int64Array| a.is_valid(row) && near_midnight(a.value(row));
        if near(&start) || near(&end) {
            let id: [u8; 16] = traces.value(row).try_into()?;
            out.push(id);
        }
    }
    Ok(())
}

/// The largest `last_ingest_time` of a Parquet file: the largest row-group maximum of the column.
/// `None` when a row group has no maximum.
fn parquet_max_i64(f: &PqFile, leaf: usize) -> Option<i64> {
    let md = f.meta.metadata();
    let mut max: Option<i64> = None;
    for rg in 0..md.num_row_groups() {
        let Some(Statistics::Int64(s)) = md.row_group(rg).column(leaf).statistics() else {
            return None;
        };
        let m = *s.max_opt()?;
        max = Some(max.map_or(m, |x| x.max(m)));
    }
    max
}

/// The anchor entries of a Parquet `traces` file. A file whose largest `last_ingest_time` is under
/// the threshold is skipped before it is read.
async fn parquet_anchor(f: &PqFile, threshold: i64) -> R<Vec<[u8; 16]>> {
    let leaf = leaf_of(f.meta.parquet_schema(), "last_ingest_time")?;
    if let Some(max) = parquet_max_i64(f, leaf) {
        if max < threshold {
            return Ok(Vec::new());
        }
    }
    let proj: Vec<usize> = ANCHOR_COLUMNS.iter().map(|n| f.meta.schema().index_of(n)).collect::<Result<_, _>>()?;
    let batches: Vec<RecordBatch> = ParquetRecordBatchStreamBuilder::new_with_metadata(
        ParquetObjectReader::new(f.store.clone(), f.path.clone()),
        f.meta.clone(),
    )
    .with_projection(ProjectionMask::roots(f.meta.parquet_schema(), proj))
    .build()?
    .try_collect()
    .await?;
    let mut out = Vec::new();
    for b in &batches {
        anchor_rows(b, threshold, &mut out)?;
    }
    Ok(out)
}

/// A scan request with no row restriction, no limit and no top-k.
fn plain_request(projection: Vec<usize>, predicate: Option<Predicate>) -> ScanRequest {
    ScanRequest {
        projection,
        predicate,
        row_ranges: None,
        limit: None,
        kind: ScanKind::Scan,
        views: false,
        top: None,
        row_filter: None,
    }
}

/// The anchor entries of a Strata `traces` file. A file whose `last_ingest_time` range ends under
/// the threshold is skipped; a file with no range (every row null) is read.
async fn strata_anchor(f: &StrataFile, threshold: i64) -> R<Vec<[u8; 16]>> {
    let schema = f.schema();
    let last = schema.index_of("last_ingest_time")?;
    if let Some((_, max, _)) = f.int_range(last).await? {
        if max < threshold {
            return Ok(Vec::new());
        }
    }
    let proj: Vec<usize> = ANCHOR_COLUMNS.iter().map(|n| schema.index_of(n)).collect::<Result<_, _>>()?;
    let batches = f.scan(&plain_request(proj, None)).await?;
    let mut out = Vec::new();
    for b in &batches {
        anchor_rows(b, threshold, &mut out)?;
    }
    Ok(out)
}

/// `anchor_rebuild` (spec 03 § Trace anchor): the `traces` rows that the startup anchor is rebuilt
/// from, read from every `traces` file that may hold them. The answer's entries are sorted.
async fn anchor(format: &str, loc: &Location, listing: &Listing, case: &ProjectCase) -> R<Answer> {
    let threshold = case.clock - ANCHOR_WINDOW_NS;
    let opened = open(loc, format, &listing.traces).await?;
    let parts: Vec<Vec<[u8; 16]>> = match &opened {
        Opened::Parquet(v) => try_join_all(v.iter().map(|f| parquet_anchor(f, threshold))).await?,
        Opened::Strata(v) => try_join_all(v.iter().map(|f| strata_anchor(f, threshold))).await?,
    };
    let mut ids: Vec<[u8; 16]> = parts.into_iter().flatten().collect();
    ids.sort_unstable();
    let mut bytes = Vec::with_capacity(ids.len() * 16);
    for id in &ids {
        bytes.extend_from_slice(id);
    }
    Ok(answer_of(&bytes, ids.len() as u64))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;
    use crate::cw::project::project_cmd;

    /// The tables of a project.
    const TABLES: [&str; 3] = ["spans", "traces", "scores"];

    /// A 3-day project of 400 traces a day, in its own directory, removed on drop.
    struct Fixture {
        base: PathBuf,
        loc: Location,
        case: ProjectCase,
        pq: Listing,
        st: Listing,
    }

    impl Fixture {
        async fn new(test: &str) -> Fixture {
            let base = std::env::temp_dir().join(format!("cw-test-many-{test}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            let dir = base.join("p");
            let args = vec![dir.display().to_string(), "--days".into(), "3".into(), "--traces-per-day".into(), "400".into()];
            project_cmd(&args).await.unwrap();
            let loc = crate::location(&dir.display().to_string()).unwrap();
            let case = read_project_case(&loc).await.unwrap();
            let pq = list(&loc, "pq_cetra").await.unwrap();
            let st = list(&loc, "st").await.unwrap();
            Fixture { base, loc, case, pq, st }
        }

        fn listing(&self, format: &str) -> &Listing {
            if format == "st" { &self.st } else { &self.pq }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Runs `query` on both formats, with no warm files, and returns both answers.
    async fn both(f: &Fixture, query: &str) -> (Answer, Answer) {
        let pq = run(query, "pq_cetra", &f.loc, &f.pq, None, &f.case).await.unwrap().0;
        let st = run(query, "st", &f.loc, &f.st, None, &f.case).await.unwrap().0;
        (pq, st)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lookups_agree_across_formats() {
        let f = Fixture::new("lookups").await;
        for query in ["lookup_cold", "lookup_around"] {
            let (pq, st) = both(&f, query).await;
            assert_eq!(pq, st, "{query} differs between the formats");
            assert!(pq.rows >= f.case.traces.len() as u64, "{query} found too few spans");
        }
        // Every id is found, on both formats.
        for format in ["pq_cetra", "st"] {
            for id in &f.case.traces {
                let spans = find_spans(&f.loc, format, &f.listing(format).spans, id).await.unwrap();
                assert!(!spans.is_empty(), "{format}: an id is not found");
            }
        }
        // The around hint opens fewer files: fewer GETs than the cold lookup, on each format.
        for format in ["pq_cetra", "st"] {
            f.loc.store.reset();
            run("lookup_cold", format, &f.loc, f.listing(format), None, &f.case).await.unwrap();
            let cold = f.loc.store.stats().0;
            f.loc.store.reset();
            run("lookup_around", format, &f.loc, f.listing(format), None, &f.case).await.unwrap();
            let around = f.loc.store.stats().0;
            assert!(around < cold, "{format}: lookup_around made {around} GETs, lookup_cold {cold}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rollup_agrees_across_formats() {
        let f = Fixture::new("rollup").await;
        let mut answers = Vec::new();
        for format in ["pq_cetra", "st"] {
            let warm_files = open(&f.loc, format, &f.listing(format).spans).await.unwrap();
            let warm = run("rollup_30d", format, &f.loc, f.listing(format), Some(&warm_files), &f.case).await.unwrap().0;
            let cold = run("rollup_30d_cold", format, &f.loc, f.listing(format), None, &f.case).await.unwrap().0;
            assert_eq!(warm, cold, "{format}: warm and cold rollups differ");
            assert!(cold.rows > 0, "{format}: no groups");
            answers.push(cold);
        }
        assert_eq!(answers[0], answers[1], "rollups differ between the formats");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn anchor_rebuild_agrees() {
        let f = Fixture::new("anchor").await;
        let (pq, st) = both(&f, "anchor_rebuild").await;
        assert_eq!(pq, st, "anchor rebuilds differ between the formats");
        assert!(pq.rows >= 1, "the live part should give anchor entries");
        let reference = reference_anchor(&f.base.join("p"), f.case.clock);
        assert_eq!(pq, reference, "anchor rebuild differs from the reference");
    }

    /// The anchor computed from every `traces` row of every Parquet file, read in full.
    fn reference_anchor(dir: &Path, clock: i64) -> Answer {
        let threshold = clock - ANCHOR_WINDOW_NS;
        let mut files = Vec::new();
        walk(&dir.join("pq_cetra/traces"), "parquet", &mut files);
        let mut ids = Vec::new();
        for file in files {
            let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(file).unwrap()).unwrap().build().unwrap();
            for batch in reader {
                anchor_rows(&batch.unwrap(), threshold, &mut ids).unwrap();
            }
        }
        ids.sort_unstable();
        let bytes: Vec<u8> = ids.iter().flatten().copied().collect();
        answer_of(&bytes, ids.len() as u64)
    }

    /// Every file under `dir` with extension `ext`.
    fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                walk(&p, ext, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some(ext) {
                out.push(p);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rollup_sums_are_order_free() {
        let f = Fixture::new("order").await;
        for format in ["pq_cetra", "st"] {
            let forward = run("rollup_30d_cold", format, &f.loc, f.listing(format), None, &f.case).await.unwrap().0;
            let mut reversed = f.listing(format).clone();
            reversed.spans.reverse();
            let backward = run("rollup_30d_cold", format, &f.loc, &reversed, None, &f.case).await.unwrap().0;
            assert_eq!(forward, backward, "{format}: the rollup depends on the file order");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_counts_every_file() {
        let f = Fixture::new("open").await;
        for format in ["pq_cetra", "st"] {
            for table in TABLES {
                let answer = run(&format!("open:{table}"), format, &f.loc, f.listing(format), None, &f.case).await.unwrap().0;
                assert_eq!(answer.rows, files_of(f.listing(format), table).unwrap().len() as u64, "{format} {table}");
            }
        }
    }

    #[test]
    fn day_text_is_utc_civil_date() {
        assert_eq!(day_text(0), "1970-01-01");
        assert_eq!(day_text(59), "1970-03-01");
        assert_eq!(day_text(11_016), "2000-02-29");
    }
}
