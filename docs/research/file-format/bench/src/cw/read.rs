//! `fmtbench cw` readers and queries (plans/08-m1-likely-failures.md § M1.3): every M1 query on
//! Strata and on cetra's Parquet files, with the same algorithm, the same thread budget and the
//! same canonical encoding of the answer, so that both formats can be compared row for row.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use arrow::array::{
    Array, BooleanArray, FixedSizeBinaryArray, MapArray, RecordBatch, Scalar, StringArray,
    StructArray,
};
use arrow::compute::kernels::comparison::ilike;
use arrow::compute::{filter_record_batch, or, prep_null_mask_filter};
use arrow::datatypes::SchemaRef;
use arrow::error::ArrowError;
use futures::TryStreamExt;
use futures::future::try_join_all;
use object_store::ObjectStore;
use object_store::path::Path as ObjPath;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowPredicate, ArrowPredicateFn, ArrowReaderMetadata, RowFilter, RowSelection,
};
use parquet::arrow::async_reader::{ParquetObjectReader, ParquetRecordBatchStreamBuilder};
use strata::{Lit, Predicate, ScanKind, ScanRequest, StrataFile};
use xxhash_rust::xxh3::xxh3_64;

use crate::R;
use crate::pqfast;

/// Rows per batch, on both formats.
const BATCH_ROWS: usize = 8_192;
/// The columns `search` looks at (spec 05 § Full-text search).
const SEARCH: [&str; 4] = ["input", "output", "name", "system_instructions"];
/// The columns `trace_light` leaves out of a lookup.
const HEAVY: [&str; 9] = [
    "input",
    "output",
    "system_instructions",
    "tool_definitions",
    "attributes",
    "resource_attributes",
    "events",
    "input_preview",
    "output_preview",
];
/// The longest needle `Predicate::Contains` takes.
const MAX_CONTAINS: usize = 64;

/// A (trace_id, span_id) pair, as the 16 and 8 bytes of the two ids.
type Pair = ([u8; 16], [u8; 8]);
/// A fold over one batch into a task's partial result.
type Fold<T> = fn(&RecordBatch, &mut T) -> R<()>;
/// A row mask over a batch: TRUE keeps a row.
type KeepFn = Arc<dyn Fn(&RecordBatch) -> Result<BooleanArray, ArrowError> + Send + Sync>;

/// A query's result, compared across formats: a row count and a 64-bit hash of a canonical
/// encoding of the rows (plans/08-m1-likely-failures.md § M1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// The number of rows the query found, in the query's own sense (see `run`).
    pub rows: u64,
    /// `xxh3_64` of the canonical encoding of those rows.
    pub hash: u64,
}

/// The files of one format, opened once per process.
pub enum Files {
    /// cetra's Parquet files, in path order.
    Parquet(Vec<pqfast::PqFile>),
    /// Strata files, in path order.
    Strata(Vec<Arc<StrataFile>>),
}

/// The inputs of the queries, from the case's text files.
pub struct Case {
    /// `cw_traces.txt`.
    pub traces: Vec<[u8; 16]>,
    /// `cw_spans.txt`.
    pub spans: Vec<([u8; 16], [u8; 8])>,
    /// `cw_needles.txt`: (name, needle).
    pub needles: Vec<(String, String)>,
    /// `cw_attr.txt`: (key, value).
    pub attr: Option<(String, String)>,
}

/// How a Strata search ran on one file: with the n-gram index on its payload columns, or as a
/// plain scan of every `SEARCH` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPath {
    /// The n-gram index on the payload columns, and a scan of the other `SEARCH` columns.
    Index,
    /// A scan of every `SEARCH` column.
    Scan,
}

/// What a scan reads. The same for both formats.
#[derive(Clone)]
struct Read {
    /// Output columns, by schema index.
    projection: Vec<usize>,
    /// The rows to keep, as a mask over `keep.columns`. Parquet evaluates it as a row filter;
    /// Strata applies it to each batch after the scan, unless `predicate` is set.
    keep: Option<Keep>,
    /// Strata's predicate for the same rows, evaluated inside its scan. Parquet ignores it.
    predicate: Option<Predicate>,
}

/// A row mask and the columns it reads.
#[derive(Clone)]
struct Keep {
    /// Schema indexes of the columns `mask` reads.
    columns: Vec<usize>,
    /// The mask: TRUE keeps a row.
    mask: KeepFn,
}

impl Read {
    /// The Strata request for this read. When there is no predicate, the mask's columns come out
    /// too, so that the mask can be applied to each batch.
    fn strata_request(&self) -> ScanRequest {
        let mut projection = self.projection.clone();
        if self.predicate.is_none() {
            if let Some(keep) = &self.keep {
                for &c in &keep.columns {
                    if !projection.contains(&c) {
                        projection.push(c);
                    }
                }
            }
        }
        ScanRequest {
            projection,
            predicate: self.predicate.clone(),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
            row_filter: None,
        }
    }
}

/// One unit of a Parquet file: the row groups it reads and the rows of them it keeps.
struct Unit {
    /// The file's store.
    store: Arc<dyn ObjectStore>,
    /// The file's path.
    path: ObjPath,
    /// The file's metadata, read at open.
    meta: ArrowReaderMetadata,
    /// The row groups the unit reads, in order.
    row_groups: Vec<usize>,
    /// The rows of those row groups the unit keeps.
    selection: RowSelection,
}

/// Runs `read` over every file, in parallel, and folds each piece with `fold`. A Strata file is
/// one piece, a scan with the file's share of the cores; a Parquet file is one piece per unit of
/// `parquet_units`. Returns the pieces' results in file order, then unit order.
async fn scan<T: Default + Send + 'static>(files: &Files, read: Read, fold: Fold<T>) -> R<Vec<T>> {
    let read = Arc::new(read);
    let mut tasks = Vec::new();
    match files {
        Files::Parquet(v) => {
            let workers = workers_for(v.len());
            for f in v {
                let sizes: Vec<usize> = f
                    .meta
                    .metadata()
                    .row_groups()
                    .iter()
                    .map(|g| g.num_rows() as usize)
                    .collect();
                for (row_groups, selection) in parquet_units(&sizes, workers) {
                    let unit = Unit {
                        store: f.store.clone(),
                        path: f.path.clone(),
                        meta: f.meta.clone(),
                        row_groups,
                        selection,
                    };
                    let read = read.clone();
                    tasks.push(tokio::spawn(async move { parquet_part(unit, &read, fold).await }));
                }
            }
        }
        Files::Strata(v) => {
            let workers = workers_for(v.len());
            for f in v {
                let (f, read) = (f.clone(), read.clone());
                tasks.push(tokio::spawn(async move { strata_part(&f, &read, workers, fold).await }));
            }
        }
    }
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(t.await??);
    }
    Ok(out)
}

/// The threads each file gets: the cores shared among the files, at least one each.
fn workers_for(files: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores / files.max(1)).max(1)
}

/// Reads one Parquet unit and folds its batches.
async fn parquet_part<T: Default>(unit: Unit, read: &Read, fold: Fold<T>) -> R<T> {
    let Unit { store, path, meta, row_groups, selection } = unit;
    let schema = meta.parquet_schema();
    let reader = ParquetObjectReader::new(store, path);
    let mut builder = ParquetRecordBatchStreamBuilder::new_with_metadata(reader, meta.clone())
        .with_batch_size(BATCH_ROWS)
        .with_row_groups(row_groups)
        .with_row_selection(selection)
        .with_projection(ProjectionMask::roots(schema, read.projection.clone()));
    if let Some(keep) = &read.keep {
        let predicate = ArrowPredicateFn::new(
            ProjectionMask::roots(schema, keep.columns.clone()),
            {
                let mask = keep.mask.clone();
                move |batch: RecordBatch| (*mask)(&batch)
            },
        );
        let predicates: Vec<Box<dyn ArrowPredicate>> = vec![Box::new(predicate)];
        builder = builder.with_row_filter(RowFilter::new(predicates));
    }
    let mut stream = builder.build()?;
    let mut acc = T::default();
    while let Some(batch) = stream.try_next().await? {
        fold(&batch, &mut acc)?;
    }
    Ok(acc)
}

/// Reads one Strata file and folds its batches. A predicate is evaluated by the scan; else the
/// mask is applied to each batch.
async fn strata_part<T: Default>(
    file: &StrataFile,
    read: &Read,
    threads: usize,
    fold: Fold<T>,
) -> R<T> {
    let request = read.strata_request();
    let post = if read.predicate.is_none() { read.keep.as_ref() } else { None };
    let mut acc = T::default();
    for batch in file.scan_batches(&request, threads).await? {
        let batch = batch?;
        let batch = match post {
            Some(keep) => {
                let mask = (*keep.mask)(&batch)?;
                filter_record_batch(&batch, &mask)?
            }
            None => batch,
        };
        fold(&batch, &mut acc)?;
    }
    Ok(acc)
}

/// Parquet's units for one file: `workers` contiguous row ranges of about equal size, each as
/// (the row groups it overlaps, a `RowSelection` over the rows of those row groups). Empty
/// ranges are left out.
fn parquet_units(row_group_rows: &[usize], workers: usize) -> Vec<(Vec<usize>, RowSelection)> {
    let total: usize = row_group_rows.iter().sum();
    let workers = workers.max(1);
    let mut starts = Vec::with_capacity(row_group_rows.len());
    let mut acc = 0;
    for &n in row_group_rows {
        starts.push(acc);
        acc += n;
    }
    let mut out = Vec::new();
    for w in 0..workers {
        let lo = w * total / workers;
        let hi = (w + 1) * total / workers;
        if lo == hi {
            continue;
        }
        let groups: Vec<usize> = (0..row_group_rows.len())
            .filter(|&g| starts[g] < hi && starts[g] + row_group_rows[g] > lo)
            .collect();
        // The overlapping row groups are contiguous, and the first one starts at or before `lo`.
        let base = starts[groups[0]];
        let rows: usize = groups.iter().map(|&g| row_group_rows[g]).sum();
        let selection = RowSelection::from_consecutive_ranges(
            std::iter::once((lo - base)..(hi - base)),
            rows,
        );
        out.push((groups, selection));
    }
    out
}

/// The search pattern of `needle` for `ilike`: `%`, the needle with `\`, `%` and `_` escaped, `%`.
/// Arrow's LIKE uses `\` as its escape character.
pub fn pattern(needle: &str) -> String {
    let mut p = String::with_capacity(needle.len() + 2);
    p.push('%');
    for c in needle.chars() {
        if matches!(c, '\\' | '%' | '_') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

/// The rows of `batch` that match `pattern` on any `SEARCH` column it holds (found by name): `ilike`
/// per column, nulls made false, then OR'd. A batch with no `SEARCH` column matches nothing.
pub fn search_mask(batch: &RecordBatch, pattern: &str) -> Result<BooleanArray, ArrowError> {
    let scalar = Scalar::new(StringArray::from(vec![pattern]));
    let mut mask = BooleanArray::from(vec![false; batch.num_rows()]);
    for name in SEARCH {
        let Some(column) = batch.column_by_name(name) else { continue };
        let column: &dyn Array = column.as_ref();
        let hits = ilike(&column, &scalar)?;
        // `prep_null_mask_filter` unwraps the null buffer, which `ilike` leaves out when the
        // column has no nulls; such a result has no null to make false.
        let hits = if hits.nulls().is_some() { prep_null_mask_filter(&hits) } else { hits };
        mask = or(&mask, &hits)?;
    }
    Ok(mask)
}

/// The mask of `search_mask` for `pat`, as a read's `Keep`.
fn pattern_mask(pat: &str) -> KeepFn {
    let pat = pat.to_owned();
    Arc::new(move |batch: &RecordBatch| search_mask(batch, &pat))
}

/// Whether `s` has a character with a case (lower and upper differ). Strata's index is
/// case-sensitive, so only needles without one may use it.
fn has_cased(s: &str) -> bool {
    s.chars().any(|c| c.to_lowercase().ne(c.to_uppercase()))
}

/// The schema of the first file of `files`.
fn schema_of(files: &Files) -> R<SchemaRef> {
    match files {
        Files::Parquet(v) => v
            .first()
            .map(|f| f.meta.schema().clone())
            .ok_or_else(|| "no Parquet files".into()),
        Files::Strata(v) => v
            .first()
            .map(|f| f.schema())
            .ok_or_else(|| "no Strata files".into()),
    }
}

/// The schema index of column `name`.
fn idx(schema: &SchemaRef, name: &str) -> R<usize> {
    Ok(schema.index_of(name)?)
}

/// The `SEARCH` columns of `schema`, by schema index.
fn search_cols(schema: &SchemaRef) -> Vec<usize> {
    SEARCH.iter().filter_map(|n| schema.index_of(n).ok()).collect()
}

/// The Utf8 column `name` of `batch`.
fn utf8_col<'a>(batch: &'a RecordBatch, name: &str) -> R<&'a StringArray> {
    let column = batch
        .column_by_name(name)
        .ok_or_else(|| format!("no {name} column"))?;
    match column.as_any().downcast_ref::<StringArray>() {
        Some(a) => Ok(a),
        None => Err(format!("{name} is not a Utf8 column").into()),
    }
}

/// The fixed-size binary column `name` of `batch`.
fn fixed_col<'a>(batch: &'a RecordBatch, name: &str) -> R<&'a FixedSizeBinaryArray> {
    let column = batch
        .column_by_name(name)
        .ok_or_else(|| format!("no {name} column"))?;
    match column.as_any().downcast_ref::<FixedSizeBinaryArray>() {
        Some(a) => Ok(a),
        None => Err(format!("{name} is not a fixed-size binary column").into()),
    }
}

/// The map column `name` of `batch`.
fn map_col<'a>(batch: &'a RecordBatch, name: &str) -> R<&'a MapArray> {
    let column = batch
        .column_by_name(name)
        .ok_or_else(|| format!("no {name} column"))?;
    match column.as_any().downcast_ref::<MapArray>() {
        Some(a) => Ok(a),
        None => Err(format!("{name} is not a map column").into()),
    }
}

/// The 64-bit hash of a payload value, or of the null marker `0xff` for a null.
fn payload_hash(column: &StringArray, row: usize) -> u64 {
    if column.is_valid(row) {
        xxh3_64(column.value(row).as_bytes())
    } else {
        xxh3_64(b"\xff")
    }
}

/// Appends the (trace_id, span_id) pairs of `batch` to `out`.
fn collect_pairs(batch: &RecordBatch, out: &mut Vec<Pair>) -> R<()> {
    let traces = fixed_col(batch, "trace_id")?;
    let spans = fixed_col(batch, "span_id")?;
    for row in 0..batch.num_rows() {
        let trace: [u8; 16] = traces.value(row).try_into()?;
        let span: [u8; 8] = spans.value(row).try_into()?;
        out.push((trace, span));
    }
    Ok(())
}

/// The answer of a set of pairs: the pairs sorted by (trace_id, span_id), each as its 16 and 8
/// bytes, hashed.
fn pair_answer(mut pairs: Vec<Pair>) -> Answer {
    pairs.sort_unstable();
    let mut bytes = Vec::with_capacity(pairs.len() * 24);
    for (trace, span) in &pairs {
        bytes.extend_from_slice(trace);
        bytes.extend_from_slice(span);
    }
    Answer { rows: pairs.len() as u64, hash: xxh3_64(&bytes) }
}

/// The search of `needle`, with the path each Strata file took (empty for Parquet). Matches are
/// the rows whose `SEARCH` text matches `pattern(needle)` (CW-17).
pub async fn search(files: &Files, needle: &str) -> R<(Answer, Vec<SearchPath>)> {
    let pat = pattern(needle);
    let schema = schema_of(files)?;
    match files {
        Files::Parquet(_) => {
            let read = scan_read(&schema, search_cols(&schema), &pat)?;
            let parts: Vec<Vec<Pair>> = scan(files, read, collect_pairs).await?;
            Ok((pair_answer(parts.concat()), Vec::new()))
        }
        Files::Strata(v) => {
            let threads = workers_for(v.len());
            let mut tasks = Vec::with_capacity(v.len());
            for f in v {
                let (f, needle, pat, schema) =
                    (f.clone(), needle.to_owned(), pat.clone(), schema.clone());
                tasks.push(tokio::spawn(async move {
                    strata_search(&f, &needle, &pat, &schema, threads).await
                }));
            }
            let mut pairs = Vec::new();
            let mut paths = Vec::with_capacity(tasks.len());
            for t in tasks {
                let (found, path) = t.await??;
                pairs.extend(found);
                paths.push(path);
            }
            Ok((pair_answer(pairs), paths))
        }
    }
}

/// The read of a search by scan: the ids of the rows whose `columns` match `pat`.
fn scan_read(schema: &SchemaRef, columns: Vec<usize>, pat: &str) -> R<Read> {
    Ok(Read {
        projection: vec![idx(schema, "trace_id")?, idx(schema, "span_id")?],
        keep: Some(Keep { columns, mask: pattern_mask(pat) }),
        predicate: None,
    })
}

/// One Strata file's search. It takes the index path when the needle has no cased character,
/// is at most `MAX_CONTAINS` bytes long, and the file has `input`, `output` or
/// `system_instructions` as a payload column; else it scans. The index path matches the payload
/// columns with `Contains` (one `Predicate::Or`) and scans the other `SEARCH` columns. Its matches
/// are deduplicated; the scan path's are not (it has no duplicates to remove).
async fn strata_search(
    file: &StrataFile,
    needle: &str,
    pat: &str,
    schema: &SchemaRef,
    threads: usize,
) -> R<(Vec<Pair>, SearchPath)> {
    let payload_ids: HashSet<usize> = file
        .directory()
        .iter()
        .filter(|r| format!("{:?}", r.layer) == "Payload")
        .map(|r| r.column_id as usize)
        .collect();
    let search = search_cols(schema);
    let payload: Vec<usize> = search.iter().copied().filter(|c| payload_ids.contains(c)).collect();
    let index_ok = !has_cased(needle)
        && needle.len() <= MAX_CONTAINS
        && payload.iter().any(|&c| {
            matches!(schema.field(c).name().as_str(), "input" | "output" | "system_instructions")
        });
    if !index_ok {
        let read = scan_read(schema, search, pat)?;
        return Ok((strata_part(file, &read, threads, collect_pairs).await?, SearchPath::Scan));
    }
    let terms: Vec<Predicate> = payload
        .iter()
        .map(|&c| Predicate::Contains(c, needle.as_bytes().to_vec()))
        .collect();
    let index_read = Read {
        projection: vec![idx(schema, "trace_id")?, idx(schema, "span_id")?],
        keep: None,
        predicate: Some(Predicate::Or(terms)),
    };
    let mut pairs: Vec<Pair> = strata_part(file, &index_read, threads, collect_pairs).await?;
    let others: Vec<usize> = search.iter().copied().filter(|c| !payload.contains(c)).collect();
    if !others.is_empty() {
        let scan = scan_read(schema, others, pat)?;
        pairs.extend(strata_part(file, &scan, threads, collect_pairs).await?);
    }
    pairs.sort_unstable();
    pairs.dedup();
    Ok((pairs, SearchPath::Index))
}

/// The mask of `attr_filter`: a row matches when its `attributes` map holds `key` exactly once,
/// with a non-null value equal to `value` (`Predicate::MapKeyEq`).
fn attr_mask(key: String, value: String) -> KeepFn {
    Arc::new(move |batch: &RecordBatch| -> Result<BooleanArray, ArrowError> {
        let maps = batch
            .column_by_name("attributes")
            .ok_or_else(|| ArrowError::SchemaError("no attributes column".into()))?;
        let maps = maps
            .as_any()
            .downcast_ref::<MapArray>()
            .ok_or_else(|| ArrowError::SchemaError("attributes is not a map".into()))?;
        let mut out = Vec::with_capacity(maps.len());
        for row in 0..maps.len() {
            let hit = !maps.is_null(row) && key_holds_value_once(&maps.value(row), &key, &value)?;
            out.push(hit);
        }
        Ok(BooleanArray::from(out))
    })
}

/// Whether the map entries `entries` hold `key` exactly once, with a non-null value `value`.
fn key_holds_value_once(entries: &StructArray, key: &str, value: &str) -> Result<bool, ArrowError> {
    let keys = entries
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| ArrowError::SchemaError("map keys are not Utf8".into()))?;
    let values = entries
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| ArrowError::SchemaError("map values are not Utf8".into()))?;
    let mut hits = 0;
    let mut hit_value = None;
    for i in 0..keys.len() {
        if keys.value(i) == key {
            hits += 1;
            hit_value = if values.is_valid(i) { Some(values.value(i)) } else { None };
        }
    }
    Ok(hits == 1 && hit_value == Some(value))
}

/// Counts, for every key, the rows whose `attributes` map holds it at least once.
fn count_keys(batch: &RecordBatch, counts: &mut BTreeMap<String, u64>) -> R<()> {
    let maps = map_col(batch, "attributes")?;
    for row in 0..maps.len() {
        if maps.is_null(row) {
            continue;
        }
        let entries = maps.value(row);
        let keys = entries
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("map keys are not Utf8")?;
        let mut seen: Vec<&str> = (0..keys.len()).map(|i| keys.value(i)).collect();
        seen.sort_unstable();
        seen.dedup();
        for key in seen {
            *counts.entry(key.to_owned()).or_insert(0) += 1;
        }
    }
    Ok(())
}

/// The non-null payload count and the hash sequence of a set of payload values.
#[derive(Default)]
struct Payloads {
    /// Non-null values seen.
    rows: u64,
    /// Each row's `xxh3_64(input)` then `xxh3_64(output)`, little-endian.
    bytes: Vec<u8>,
}

/// Folds each row's input and output hashes into `out`.
fn hash_payloads(batch: &RecordBatch, out: &mut Payloads) -> R<()> {
    let input = utf8_col(batch, "input")?;
    let output = utf8_col(batch, "output")?;
    for row in 0..batch.num_rows() {
        for column in [input, output] {
            out.rows += u64::from(column.is_valid(row));
            out.bytes.extend_from_slice(&payload_hash(column, row).to_le_bytes());
        }
    }
    Ok(())
}

/// `attr_filter`: the matching spans.
async fn attr_filter(files: &Files, case: &Case) -> R<Answer> {
    let (key, value) = case.attr.clone().ok_or("the case has no cw_attr.txt")?;
    let schema = schema_of(files)?;
    let attr = idx(&schema, "attributes")?;
    let read = Read {
        projection: vec![idx(&schema, "trace_id")?, idx(&schema, "span_id")?],
        keep: Some(Keep { columns: vec![attr], mask: attr_mask(key.clone(), value.clone()) }),
        predicate: Some(Predicate::MapKeyEq(attr, key, Lit::Utf8(value))),
    };
    let parts: Vec<Vec<Pair>> = scan(files, read, collect_pairs).await?;
    Ok(pair_answer(parts.concat()))
}

/// `attr_keys`: the distinct keys, each with the number of rows that hold it.
async fn attr_keys(files: &Files) -> R<Answer> {
    let schema = schema_of(files)?;
    let read = Read {
        projection: vec![idx(&schema, "attributes")?],
        keep: None,
        predicate: None,
    };
    let parts: Vec<BTreeMap<String, u64>> = scan(files, read, count_keys).await?;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for part in parts {
        for (key, n) in part {
            *counts.entry(key).or_insert(0) += n;
        }
    }
    let mut bytes = Vec::new();
    for (key, n) in &counts {
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&n.to_le_bytes());
    }
    Ok(Answer { rows: counts.len() as u64, hash: xxh3_64(&bytes) })
}

/// `scan_payload`: the non-null `input` and `output` values, hashed in file and row order.
async fn scan_payload(files: &Files) -> R<Answer> {
    let schema = schema_of(files)?;
    let read = Read {
        projection: vec![idx(&schema, "input")?, idx(&schema, "output")?],
        keep: None,
        predicate: None,
    };
    let parts: Vec<Payloads> = scan(files, read, hash_payloads).await?;
    let mut rows = 0;
    let mut bytes = Vec::new();
    for part in parts {
        rows += part.rows;
        bytes.extend(part.bytes);
    }
    Ok(Answer { rows, hash: xxh3_64(&bytes) })
}

/// The rows of trace `id` on every file, with the columns `projection` (schema indexes), read
/// on all files at once.
async fn lookup_batches(
    files: &Files,
    id: &[u8; 16],
    projection: &[usize],
) -> R<Vec<RecordBatch>> {
    let per_file: Vec<Vec<RecordBatch>> = match files {
        Files::Parquet(v) => try_join_all(v.iter().map(|f| pqfast::lookup(f, id, projection)))
            .await?,
        Files::Strata(v) => {
            try_join_all(v.iter().map(|f| f.lookup_scan(id, projection, false))).await?
        }
    };
    Ok(per_file.into_iter().flatten().collect())
}

/// `trace_light`: for each trace of the case, its `span_id`s, sorted, from the light columns.
async fn trace_light(files: &Files, case: &Case) -> R<Answer> {
    let schema = schema_of(files)?;
    let light: Vec<usize> = (0..schema.fields().len())
        .filter(|&i| !HEAVY.contains(&schema.field(i).name().as_str()))
        .collect();
    let mut rows = 0;
    let mut bytes = Vec::new();
    for trace in &case.traces {
        let batches = lookup_batches(files, trace, &light).await?;
        let mut spans: Vec<[u8; 8]> = Vec::new();
        for batch in &batches {
            let column = fixed_col(batch, "span_id")?;
            for row in 0..batch.num_rows() {
                let span: [u8; 8] = column.value(row).try_into()?;
                spans.push(span);
            }
        }
        spans.sort_unstable();
        rows += spans.len() as u64;
        for span in &spans {
            bytes.extend_from_slice(span);
        }
    }
    Ok(Answer { rows, hash: xxh3_64(&bytes) })
}

/// The input and output hashes of the row of `batches` whose `span_id` is `span`, if any.
fn find_payloads(batches: &[RecordBatch], span: &[u8; 8]) -> R<Option<(u64, u64)>> {
    for batch in batches {
        let spans = fixed_col(batch, "span_id")?;
        let input = utf8_col(batch, "input")?;
        let output = utf8_col(batch, "output")?;
        for row in 0..batch.num_rows() {
            if spans.value(row) == span.as_slice() {
                return Ok(Some((payload_hash(input, row), payload_hash(output, row))));
            }
        }
    }
    Ok(None)
}

/// `span_heavy`: for each pair of the case that is found, its input and output hashes.
async fn span_heavy(files: &Files, case: &Case) -> R<Answer> {
    let schema = schema_of(files)?;
    let columns = [idx(&schema, "span_id")?, idx(&schema, "input")?, idx(&schema, "output")?];
    let mut rows = 0;
    let mut bytes = Vec::new();
    for (trace, span) in &case.spans {
        let batches = lookup_batches(files, trace, &columns).await?;
        if let Some((input, output)) = find_payloads(&batches, span)? {
            rows += 1;
            bytes.extend_from_slice(&input.to_le_bytes());
            bytes.extend_from_slice(&output.to_le_bytes());
        }
    }
    Ok(Answer { rows, hash: xxh3_64(&bytes) })
}

/// Runs `query` on `files`. Queries: `search:<name>` (the needle is `case.needles`' entry of
/// that name), `attr_filter`, `attr_keys`, `scan_payload`, `trace_light` and `span_heavy`.
/// A Strata search prints to stderr how many files took each `SearchPath`.
pub async fn run(query: &str, files: &Files, case: &Case) -> R<Answer> {
    if let Some(name) = query.strip_prefix("search:") {
        let needle = case
            .needles
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| format!("no needle named {name}"))?;
        let (answer, paths) = search(files, needle).await?;
        if matches!(files, Files::Strata(_)) {
            let index = paths.iter().filter(|p| **p == SearchPath::Index).count();
            eprintln!(
                "{query}: {index} file(s) on the index path, {} on the scan path",
                paths.len() - index
            );
        }
        return Ok(answer);
    }
    match query {
        "attr_filter" => attr_filter(files, case).await,
        "attr_keys" => attr_keys(files).await,
        "scan_payload" => scan_payload(files).await,
        "trace_light" => trace_light(files, case).await,
        "span_heavy" => span_heavy(files, case).await,
        other => Err(format!("unknown query {other}").into()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use arrow::array::{Array, Scalar, StringArray};
    use arrow::compute::kernels::comparison::ilike;
    use cetra_core::batch::spans_to_record_batch;
    use cetra_core::enums::PayloadFormat;
    use cetra_core::span::Span;
    use fmt_testdata::datagen::{Rng, World};

    use super::*;

    /// The two files of a fixture: the spans sorted by start time and cut in half, each half
    /// sorted by (trace_id, start_time). Both formats and the references use this order.
    fn layout(mut spans: Vec<Span>) -> Vec<Vec<Span>> {
        spans.sort_by_key(|s| s.start_time);
        let cut = spans.len().div_ceil(2);
        let second = spans.split_off(cut);
        vec![spans, second]
            .into_iter()
            .map(|mut half| {
                half.sort_by_key(|s| (*s.trace_id.as_bytes(), s.start_time));
                half
            })
            .collect()
    }

    /// Every row of `spans`, in the order both formats store them.
    fn file_rows(spans: &[Span]) -> Vec<Span> {
        layout(spans.to_vec()).concat()
    }

    /// Writes `spans` as a fixture directory `cw-test-<name>-<pid>` in the temp dir: two files
    /// per format, `pq_cetra/NNN.parquet` and `st/NNN.strata`, built as `layout` describes.
    fn fixture(name: &str, spans: Vec<Span>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cw-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pq_cetra")).unwrap();
        std::fs::create_dir_all(dir.join("st")).unwrap();
        for (i, half) in layout(spans).iter().enumerate() {
            let batch = spans_to_record_batch(half).unwrap();
            crate::write_parquet(
                "pq_cetra",
                &batch,
                &dir.join("pq_cetra").join(format!("{i:03}.parquet")),
            )
            .unwrap();
            crate::write_strata(&batch, &dir.join("st").join(format!("{i:03}.strata"))).unwrap();
        }
        dir
    }

    /// Both formats of the fixture at `dir`, opened as the harness opens them.
    async fn open_both(dir: &Path) -> (Files, Files) {
        let loc = crate::location(dir.to_str().unwrap()).unwrap();
        let pq = crate::open_pq_fast(&loc, "pq_cetra").await.unwrap();
        let st = crate::open_strata(&loc, "st").await.unwrap();
        (Files::Parquet(pq), Files::Strata(st))
    }

    /// `spans` from `traces` traces of the test world (`World::new(7)`, `Rng::new(42)`).
    fn world_spans(traces: usize) -> Vec<Span> {
        let world = World::new(7);
        let mut rng = Rng::new(42);
        let mut spans = Vec::new();
        for _ in 0..traces {
            world.trace(&mut rng, &mut spans);
        }
        spans
    }

    /// Whether `pattern` matches `text` under `ilike`, one value at a time (the reference's
    /// kernel call; a null text does not match).
    fn matches_one(text: Option<&str>, pattern: &str) -> bool {
        let Some(text) = text else { return false };
        let value: &dyn Array = &StringArray::from(vec![text]);
        let pat = Scalar::new(StringArray::from(vec![pattern]));
        let hits = ilike(&value, &pat).unwrap();
        hits.is_valid(0) && hits.value(0)
    }

    /// The reference search: the (trace, span) pairs of the spans whose `SEARCH` texts match.
    fn reference_search(rows: &[Span], needle: &str) -> Answer {
        let pat = pattern(needle);
        let pairs: Vec<Pair> = rows
            .iter()
            .filter(|s| {
                [s.input.as_deref(), s.output.as_deref(), Some(s.name.as_str()), s.system_instructions.as_deref()]
                    .iter()
                    .any(|text| matches_one(*text, &pat))
            })
            .map(|s| (*s.trace_id.as_bytes(), *s.span_id.as_bytes()))
            .collect();
        pair_answer(pairs)
    }

    /// Prepends `prefix` to a present text.
    fn prepend(text: &mut Option<String>, prefix: &str) {
        if let Some(t) = text {
            *t = format!("{prefix}{t}");
        }
    }

    /// Repeats every present, non-empty text until it is at least 300 bytes.
    fn pad_payloads(spans: &mut [Span]) {
        for s in spans {
            for field in [&mut s.input, &mut s.output] {
                if let Some(t) = field {
                    if !t.is_empty() {
                        *t = t.repeat(300usize.div_ceil(t.len()));
                    }
                }
            }
        }
    }

    /// The search fixture: 60 world traces, payloads padded to 300 bytes, and the needles planted.
    fn search_spans() -> Vec<Span> {
        let mut spans = world_spans(60);
        pad_payloads(&mut spans);
        let inputs: Vec<usize> = (0..spans.len()).filter(|&i| spans[i].input.is_some()).collect();
        for &i in &[inputs[0], inputs[inputs.len() / 2]] {
            prepend(&mut spans[i].input, "zqxjrareneedle ");
        }
        let outputs: Vec<usize> = (0..spans.len()).filter(|&i| spans[i].output.is_some()).collect();
        for (k, prefix) in ["bug ", "Bug ", "BUG "].iter().enumerate() {
            prepend(&mut spans[outputs[k * 7]].output, prefix);
        }
        spans[3].name = format!("404 {}", spans[3].name);
        let sys = (0..spans.len()).find(|&i| spans[i].system_instructions.is_some()).unwrap();
        prepend(&mut spans[sys].system_instructions, "SQL ");
        spans
    }

    /// The payload-hash sequence of `rows` in the order the readers hash it.
    fn reference_payloads(rows: &[Span]) -> Answer {
        let mut rows_found = 0;
        let mut bytes = Vec::new();
        for s in rows {
            for value in [s.input.as_deref(), s.output.as_deref()] {
                match value {
                    Some(v) => {
                        rows_found += 1;
                        bytes.extend_from_slice(&xxh3_64(v.as_bytes()).to_le_bytes());
                    }
                    None => bytes.extend_from_slice(&xxh3_64(b"\xff").to_le_bytes()),
                }
            }
        }
        Answer { rows: rows_found, hash: xxh3_64(&bytes) }
    }

    #[tokio::test]
    async fn search_agrees_with_reference() {
        let spans = search_spans();
        let rows = file_rows(&spans);
        let dir = fixture("search_agrees", spans);
        let (pq, st) = open_both(&dir).await;
        for (needle, min) in [
            ("qzxjabsentneedle", 0),
            ("zqxjrareneedle", 2),
            ("bug", 3),
            ("404", 1),
            ("SQL", 1),
        ] {
            let want = reference_search(&rows, needle);
            assert!(want.rows >= min, "{needle}: {} rows", want.rows);
            let (got_pq, _) = search(&pq, needle).await.unwrap();
            let (got_st, _) = search(&st, needle).await.unwrap();
            assert_eq!(got_pq, want, "{needle} on parquet");
            assert_eq!(got_st, want, "{needle} on strata");
        }
        assert_eq!(reference_search(&rows, "qzxjabsentneedle").rows, 0);
        assert_eq!(reference_search(&rows, "zqxjrareneedle").rows, 2);
    }

    #[tokio::test]
    async fn search_matches_despite_nulls() {
        let mut spans = world_spans(5);
        for s in spans.iter_mut().take(3) {
            s.input = Some("zqxjrareneedle lives here".to_owned());
            s.input_format = Some(PayloadFormat::Text);
            s.output = None;
            s.output_format = None;
            s.output_preview = None;
            s.system_instructions = None;
        }
        let rows = file_rows(&spans);
        let dir = fixture("search_nulls", spans);
        let (pq, st) = open_both(&dir).await;
        let want = reference_search(&rows, "zqxjrareneedle");
        assert_eq!(want.rows, 3);
        assert_eq!(search(&pq, "zqxjrareneedle").await.unwrap().0, want);
        assert_eq!(search(&st, "zqxjrareneedle").await.unwrap().0, want);
    }

    #[tokio::test]
    async fn search_agrees_on_non_ascii() {
        let mut spans = world_spans(20);
        let texts = ["CAFÉ CRÈME", "Le café", "東京の天気", "🚀 done", "Ошибка базы"];
        let inputs: Vec<usize> = (0..spans.len()).filter(|&i| spans[i].input.is_some()).collect();
        for (k, text) in texts.iter().enumerate() {
            spans[inputs[k]].input = Some((*text).to_owned());
            spans[inputs[k]].input_format = Some(PayloadFormat::Text);
        }
        let rows = file_rows(&spans);
        let dir = fixture("search_intl", spans);
        let (pq, st) = open_both(&dir).await;
        println!(
            "ilike: 'café' matches 'CAFÉ CRÈME': {}",
            matches_one(Some("CAFÉ CRÈME"), &pattern("café"))
        );
        for needle in ["café", "東京", "🚀", "ошибка"] {
            let want = reference_search(&rows, needle);
            assert_eq!(search(&pq, needle).await.unwrap().0, want, "{needle} on parquet");
            assert_eq!(search(&st, needle).await.unwrap().0, want, "{needle} on strata");
        }
    }

    #[tokio::test]
    async fn search_escapes_like_characters() {
        let mut spans = world_spans(3);
        for s in &mut spans {
            s.input = None;
            s.input_format = None;
            s.input_preview = None;
        }
        spans[0].input = Some("price 50%_off\\x now".to_owned());
        spans[0].input_format = Some(PayloadFormat::Text);
        let dir = fixture("search_escape", spans);
        let (pq, st) = open_both(&dir).await;
        for files in [&pq, &st] {
            assert_eq!(search(files, "50%_off\\x").await.unwrap().0.rows, 1);
            assert_eq!(search(files, "50%off").await.unwrap().0.rows, 0);
        }
    }

    #[tokio::test]
    async fn search_takes_the_index_only_without_cased_characters() {
        let dir = fixture("search_paths", search_spans());
        let (_, st) = open_both(&dir).await;
        let (_, digits) = search(&st, "404").await.unwrap();
        assert_eq!(digits, vec![SearchPath::Index, SearchPath::Index]);
        let (_, cased) = search(&st, "bug").await.unwrap();
        assert_eq!(cased, vec![SearchPath::Scan, SearchPath::Scan]);
        let long = "1".repeat(65);
        let (_, too_long) = search(&st, &long).await.unwrap();
        assert_eq!(too_long, vec![SearchPath::Scan, SearchPath::Scan]);
    }

    #[tokio::test]
    async fn attr_queries_agree() {
        let world = World::new(7);
        let mut rng = Rng::new(42);
        let mut spans = Vec::new();
        for _ in 0..200 {
            world.trace(&mut rng, &mut spans);
        }
        for s in &mut spans {
            let mut attrs = BTreeMap::new();
            while attrs.len() < 20 {
                let key = format!("tool.arg.{}", rng.below(100));
                attrs.insert(key, format!("\"v{}\"", rng.below(50)));
            }
            s.attributes = attrs;
        }
        let rows = file_rows(&spans);
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for s in &rows {
            for key in s.attributes.keys() {
                *counts.entry(key.clone()).or_insert(0) += 1;
            }
        }
        let want_filter = pair_answer(
            rows.iter()
                .filter(|s| s.attributes.get("tool.arg.2").map(String::as_str) == Some("\"v3\""))
                .map(|s| (*s.trace_id.as_bytes(), *s.span_id.as_bytes()))
                .collect(),
        );
        assert!(want_filter.rows > 0);
        let mut bytes = Vec::new();
        for (key, n) in &counts {
            bytes.extend_from_slice(key.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        let want_keys = Answer { rows: counts.len() as u64, hash: xxh3_64(&bytes) };

        let dir = fixture("attr_agree", spans);
        let (pq, st) = open_both(&dir).await;
        let case = Case {
            traces: Vec::new(),
            spans: Vec::new(),
            needles: Vec::new(),
            attr: Some(("tool.arg.2".to_owned(), "\"v3\"".to_owned())),
        };
        for files in [&pq, &st] {
            assert_eq!(run("attr_filter", files, &case).await.unwrap(), want_filter);
            assert_eq!(run("attr_keys", files, &case).await.unwrap(), want_keys);
        }
    }

    #[tokio::test]
    async fn attr_filter_ignores_other_keys() {
        let mut spans = world_spans(3);
        assert!(spans.len() >= 3);
        for s in &mut spans {
            s.attributes.clear();
        }
        spans[0].attributes.insert("tool.arg.5".to_owned(), "\"v3\"".to_owned());
        spans[1].attributes.insert("tool.arg.2".to_owned(), "\"v4\"".to_owned());
        spans[2].attributes.insert("tool.arg.2".to_owned(), "\"v3\"".to_owned());
        let want = pair_answer(vec![(*spans[2].trace_id.as_bytes(), *spans[2].span_id.as_bytes())]);
        let dir = fixture("attr_other", spans);
        let (pq, st) = open_both(&dir).await;
        let case = Case {
            traces: Vec::new(),
            spans: Vec::new(),
            needles: Vec::new(),
            attr: Some(("tool.arg.2".to_owned(), "\"v3\"".to_owned())),
        };
        assert_eq!(run("attr_filter", &pq, &case).await.unwrap(), want);
        assert_eq!(run("attr_filter", &st, &case).await.unwrap(), want);
    }

    #[tokio::test]
    async fn payload_and_lookups_agree() {
        let spans = world_spans(80);
        let rows = file_rows(&spans);
        let traces: Vec<[u8; 16]> = (0..5)
            .map(|k| *spans[k * spans.len() / 5].trace_id.as_bytes())
            .collect();
        let picked: Vec<&Span> = spans
            .iter()
            .filter(|s| traces.contains(s.trace_id.as_bytes()))
            .collect();
        let pairs: Vec<Pair> = picked
            .iter()
            .step_by(picked.len() / 10)
            .take(10)
            .map(|s| (*s.trace_id.as_bytes(), *s.span_id.as_bytes()))
            .collect();
        assert_eq!(pairs.len(), 10);

        let want_payloads = reference_payloads(&rows);
        let mut light = Vec::new();
        for trace in &traces {
            let mut ids: Vec<[u8; 8]> = rows
                .iter()
                .filter(|s| s.trace_id.as_bytes() == trace)
                .map(|s| *s.span_id.as_bytes())
                .collect();
            ids.sort_unstable();
            light.extend(ids);
        }
        let want_light = Answer {
            rows: light.len() as u64,
            hash: xxh3_64(&light.iter().flatten().copied().collect::<Vec<u8>>()),
        };
        let mut heavy_bytes = Vec::new();
        let mut heavy_rows = 0;
        for (trace, span) in &pairs {
            let found = rows
                .iter()
                .find(|s| s.trace_id.as_bytes() == trace && s.span_id.as_bytes() == span);
            if let Some(s) = found {
                heavy_rows += 1;
                heavy_bytes.extend_from_slice(&payload_hash_of(s.input.as_deref()).to_le_bytes());
                heavy_bytes.extend_from_slice(&payload_hash_of(s.output.as_deref()).to_le_bytes());
            }
        }
        let want_heavy = Answer { rows: heavy_rows, hash: xxh3_64(&heavy_bytes) };
        assert_eq!(want_heavy.rows, 10);

        let dir = fixture("payload_lookups", spans);
        let (pq, st) = open_both(&dir).await;
        let case = Case { traces, spans: pairs, needles: Vec::new(), attr: None };
        for files in [&pq, &st] {
            assert_eq!(run("scan_payload", files, &case).await.unwrap(), want_payloads);
            assert_eq!(run("trace_light", files, &case).await.unwrap(), want_light);
            assert_eq!(run("span_heavy", files, &case).await.unwrap(), want_heavy);
        }
    }

    /// The payload hash of an optional value, as `payload_hash` computes it.
    fn payload_hash_of(value: Option<&str>) -> u64 {
        match value {
            Some(v) => xxh3_64(v.as_bytes()),
            None => xxh3_64(b"\xff"),
        }
    }

    #[test]
    fn parquet_units_cover_every_row() {
        for rows in [vec![100_000usize, 100_000, 37], vec![5]] {
            let total: usize = rows.iter().sum();
            let mut starts = Vec::new();
            let mut acc = 0;
            for &n in &rows {
                starts.push(acc);
                acc += n;
            }
            for workers in [1, 3, 8] {
                let mut seen: Vec<usize> = Vec::new();
                for (groups, selection) in parquet_units(&rows, workers) {
                    let group_rows: Vec<usize> = groups
                        .iter()
                        .flat_map(|&g| starts[g]..starts[g] + rows[g])
                        .collect();
                    let mut pos = 0;
                    for selector in selection.iter() {
                        if !selector.skip {
                            seen.extend_from_slice(&group_rows[pos..pos + selector.row_count]);
                        }
                        pos += selector.row_count;
                    }
                    assert_eq!(pos, group_rows.len());
                }
                assert_eq!(seen, (0..total).collect::<Vec<usize>>(), "rows {rows:?}, workers {workers}");
            }
        }
    }
}
