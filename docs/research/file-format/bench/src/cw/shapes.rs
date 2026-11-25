//! `fmtbench cw` M4 queries on a project directory (plans/08-m4-query-shapes.md § M4.2): the trace
//! list in three sorts, and the sessions and users rollups, on the `traces` files of cetra's
//! Parquet and of their Strata conversions. Both formats run the same fold (`scan_fold`) with the
//! same units and parallelism, and the answers are encoded the same way.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use arrow::array::{Array, BooleanArray, FixedSizeBinaryArray, Float64Array, RecordBatch, StringArray};
use arrow::error::ArrowError;
use futures::{StreamExt, TryStreamExt};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowPredicate, ArrowPredicateFn, RowFilter};
use parquet::arrow::async_reader::{ParquetObjectReader, ParquetRecordBatchStreamBuilder};
use parquet::file::statistics::Statistics;
use strata::{CmpOp, Lit, Predicate, ScanKind, ScanRequest, StrataFile};

use crate::cw::many::{self, FileRef, Opened};
use crate::cw::read::Answer;
use crate::pqfast::PqFile;
use crate::R;

/// The trace list returns this many traces.
const TOP_TRACES: usize = 50;
/// Cost in micro-units (USD × 1e6, rounded per row).
const COST_SCALE: f64 = 1e6;
/// Trace scans read this much beyond the window on both sides (spec 03 § 6's known limit).
const PAD_NS: i64 = many::HOUR_NS;

/// The per-query input the M4 queries need from the project: its clock (`cw_clock.txt`) and its
/// needles (`cw_needles.txt`, M1's format).
pub(crate) struct ShapeCase {
    pub clock: i64,
    pub needles: Vec<(String, String)>,
}

/// One scan's row filter: `start_time` in `[from, to)`, `span_kind = 'llm'` when `llm_only`, and
/// `trace_id` in `ids` when given. The same rows on both formats.
pub(crate) struct Window {
    pub from: i64,
    pub to: i64,
    pub llm_only: bool,
    pub ids: Option<Arc<Vec<[u8; 16]>>>,
}

/// A per-unit result that merges with another's. Merges are commutative and associative, so the
/// order in which units finish does not matter.
pub(crate) trait Fold: Default + Send + 'static {
    /// Adds `other`, another unit's result, to `self`.
    fn merge(&mut self, other: Self);
}

/// What a unit does with each batch it reads.
pub(crate) type FoldFn<T> = Arc<dyn Fn(&RecordBatch, &mut T) -> R<()> + Send + Sync>;

impl Fold for u64 {
    fn merge(&mut self, other: Self) {
        *self += other;
    }
}

/// One trace's merged partials (spec 03 § 6): the min start, the max end, the cost sum in
/// micro-units, and the first `session_id` or `user_id` in byte order, if any partial has one.
#[derive(Clone, Debug, PartialEq)]
struct Part {
    start: i64,
    end: i64,
    cost: i64,
    extra: Option<String>,
}

impl Part {
    fn merge(&mut self, other: Part) {
        self.start = self.start.min(other.start);
        self.end = self.end.max(other.end);
        self.cost = self.cost.saturating_add(other.cost);
        self.extra = match (self.extra.take(), other.extra) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, None) => a,
            (None, b) => b,
        };
    }
}

/// The merged partials of each trace, by trace id.
#[derive(Default)]
struct Traces(BTreeMap<[u8; 16], Part>);

impl Traces {
    fn add(&mut self, id: [u8; 16], part: Part) {
        match self.0.entry(id) {
            Entry::Occupied(mut e) => e.get_mut().merge(part),
            Entry::Vacant(e) => {
                e.insert(part);
            }
        }
    }
}

impl Fold for Traces {
    fn merge(&mut self, other: Self) {
        for (id, part) in other.0 {
            self.add(id, part);
        }
    }
}

/// The M4 trace queries, which read the `traces` table: `trace_list:<sort>`, `sessions:7d` and
/// `users:30d`. M4.3 adds the spans queries.
pub(crate) fn is_shape(query: &str) -> bool {
    query.starts_with("trace_list:") || matches!(query, "sessions:7d" | "users:30d")
}

/// The table a query reads: `traces` for the trace list, sessions and users; `spans` for the rest.
pub(crate) fn table_of(query: &str) -> &'static str {
    if query.starts_with("trace_list:") || query.starts_with("sessions:") || query.starts_with("users:") {
        "traces"
    } else {
        "spans"
    }
}

/// The sort of `trace_list:<sort>`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sort {
    Start,
    Cost,
    Duration,
}

impl Sort {
    fn key(self, p: &Part) -> i64 {
        match self {
            Sort::Start => p.start,
            Sort::Cost => p.cost,
            Sort::Duration => p.end.saturating_sub(p.start),
        }
    }
}

fn sort_of(query: &str) -> R<Sort> {
    match query.strip_prefix("trace_list:") {
        Some("start_time") => Ok(Sort::Start),
        Some("cost_total") => Ok(Sort::Cost),
        Some("duration") => Ok(Sort::Duration),
        _ => Err(format!("unknown trace list sort in {query}: want start_time, cost_total or duration").into()),
    }
}

/// One M4 query's answer on the files of `table_of(query)`, `opened` index-aligned with `files`.
/// The answer's bytes are per query:
/// - `trace_list:<sort>` (window 7d): the 50 trace ids in order, 16 bytes each, then the row count
///   `u64` of those traces' `traces` rows in the padded window (rows = 50);
/// - `sessions:7d`, `users:30d`: per non-null `session_id` or `user_id` sorted bytewise: the name's
///   bytes, `0x00`, trace count `u64`, min start `i64`, max end `i64`, cost micro-units `i64`,
///   little-endian (rows = names).
pub(crate) async fn run(query: &str, files: &[FileRef], opened: &Opened, case: &ShapeCase) -> R<Answer> {
    if query.starts_with("trace_list:") {
        return trace_list(sort_of(query)?, files, opened, case.clock).await;
    }
    match query {
        "sessions:7d" => rollup("session_id", 7, files, opened, case.clock).await,
        "users:30d" => rollup("user_id", 30, files, opened, case.clock).await,
        _ => Err(format!("unknown M4 query {query}").into()),
    }
}

/// The window of `days` days ending at `clock`: `[clock − days, clock)`.
fn window_of(clock: i64, days: i64) -> (i64, i64) {
    (clock - days * many::DAY_NS, clock)
}

/// The days a scan reads: from the day of `from` minus one to the day of `to` plus one, as
/// `YYYY-MM-DD` texts (a file is read when its `day=` segment is one of them).
fn days_of(window: &Window) -> HashSet<String> {
    let first = window.from.div_euclid(many::DAY_NS) - 1;
    let last = window.to.div_euclid(many::DAY_NS) + 1;
    (first..=last).map(many::day_text).collect()
}

/// One unit of a scan: a Parquet row group, or a range of rows of a Strata file.
enum Unit<'a> {
    Parquet(&'a PqFile, usize),
    Strata(&'a StrataFile, Range<u64>),
}

/// Whether a Parquet row group may hold rows of `window`: not when its `start_time` min/max miss the
/// window. A row group without `start_time` statistics may.
fn row_group_may_hold(f: &PqFile, rg: usize, window: &Window) -> bool {
    let md = f.meta.metadata();
    let Some(leaf) = f.meta.parquet_schema().columns().iter().position(|c| c.path().string() == "start_time") else {
        return true;
    };
    if let Some(Statistics::Int64(s)) = md.row_group(rg).column(leaf).statistics() {
        if let (Some(min), Some(max)) = (s.min_opt(), s.max_opt()) {
            return *max >= window.from && *min < window.to;
        }
    }
    true
}

/// Scans `columns` of `files` (one table's files, `opened` index-aligned with them) with `window`,
/// unit by unit, `cores` units at a time. Each unit folds its batches with `fold`; the units'
/// results merge into one accumulator as they finish. Only the files whose day is in `days_of` are
/// read. The columns are looked up by name in the fold, since the two formats return them in
/// different orders.
pub(crate) async fn scan_fold<T: Fold>(
    files: &[FileRef],
    opened: &Opened,
    columns: &[&str],
    window: &Window,
    fold: FoldFn<T>,
) -> R<T> {
    let days = days_of(window);
    let ids: Option<Arc<HashSet<[u8; 16]>>> = window.ids.as_ref().map(|v| Arc::new(v.iter().copied().collect()));
    let mut units: Vec<Unit<'_>> = Vec::new();
    match opened {
        Opened::Parquet(v) => {
            if v.len() != files.len() {
                return Err(format!("{} opened files for {} listed", v.len(), files.len()).into());
            }
            for (f, file) in v.iter().zip(files) {
                if !days.contains(&file.day) {
                    continue;
                }
                for rg in 0..f.meta.metadata().num_row_groups() {
                    if row_group_may_hold(f, rg, window) {
                        units.push(Unit::Parquet(f, rg));
                    }
                }
            }
        }
        Opened::Strata(v) => {
            if v.len() != files.len() {
                return Err(format!("{} opened files for {} listed", v.len(), files.len()).into());
            }
            for (f, file) in v.iter().zip(files) {
                if !days.contains(&file.day) {
                    continue;
                }
                for rows in many::rollup_ranges(f.row_count()) {
                    units.push(Unit::Strata(f.as_ref(), rows));
                }
            }
        }
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    futures::stream::iter(units)
        .map(|unit| scan_unit(unit, columns, window, ids.clone(), fold.clone()))
        .buffer_unordered(cores)
        .try_fold(T::default(), |mut acc, part| {
            acc.merge(part);
            futures::future::ready(Ok(acc))
        })
        .await
}

/// Reads one scan unit and folds its rows into a fresh accumulator.
async fn scan_unit<T: Fold>(
    unit: Unit<'_>,
    columns: &[&str],
    window: &Window,
    ids: Option<Arc<HashSet<[u8; 16]>>>,
    fold: FoldFn<T>,
) -> R<T> {
    let mut acc = T::default();
    match unit {
        Unit::Parquet(f, rg) => {
            let schema = f.meta.schema();
            let cols: Vec<usize> = columns.iter().map(|n| schema.index_of(n)).collect::<Result<_, _>>()?;
            let mut keys = vec!["start_time"];
            if window.llm_only {
                keys.push("span_kind");
            }
            if window.ids.is_some() {
                keys.push("trace_id");
            }
            let key_cols: Vec<usize> = keys.iter().map(|n| schema.index_of(n)).collect::<Result<_, _>>()?;
            let (from, to, llm) = (window.from, window.to, window.llm_only);
            let predicate = ArrowPredicateFn::new(ProjectionMask::roots(f.meta.parquet_schema(), key_cols), move |batch: RecordBatch| {
                keep_rows(&batch, from, to, llm, ids.as_deref())
            });
            let predicates: Vec<Box<dyn ArrowPredicate>> = vec![Box::new(predicate)];
            let mut stream = ParquetRecordBatchStreamBuilder::new_with_metadata(
                ParquetObjectReader::new(f.store.clone(), f.path.clone()),
                f.meta.clone(),
            )
            .with_batch_size(many::BATCH_ROWS)
            .with_row_groups(vec![rg])
            .with_projection(ProjectionMask::roots(f.meta.parquet_schema(), cols))
            .with_row_filter(RowFilter::new(predicates))
            .build()?;
            while let Some(batch) = stream.try_next().await? {
                fold(&batch, &mut acc)?;
            }
        }
        Unit::Strata(f, rows) => {
            let schema = f.schema();
            let projection: Vec<usize> = columns.iter().map(|n| schema.index_of(n)).collect::<Result<_, _>>()?;
            let mut terms = vec![
                Predicate::Cmp(schema.index_of("start_time")?, CmpOp::GtEq, Lit::Int(window.from)),
                Predicate::Cmp(schema.index_of("start_time")?, CmpOp::Lt, Lit::Int(window.to)),
            ];
            if window.llm_only {
                terms.push(Predicate::Cmp(schema.index_of("span_kind")?, CmpOp::Eq, Lit::Utf8(many::LLM.to_owned())));
            }
            if let Some(ids) = &window.ids {
                let lits = ids.iter().map(|id| Lit::Bytes(id.to_vec())).collect();
                terms.push(Predicate::In(schema.index_of("trace_id")?, lits));
            }
            let req = ScanRequest {
                projection,
                predicate: Some(Predicate::And(terms)),
                row_ranges: Some(vec![u32::try_from(rows.start)?..u32::try_from(rows.end)?]),
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
                row_filter: None,
            };
            for batch in f.scan_batches(&req, 1).await? {
                fold(&batch?, &mut acc)?;
            }
        }
    }
    Ok(acc)
}

/// The rows of a Parquet batch that the window keeps, from its `start_time`, `span_kind` (when
/// `llm`) and `trace_id` (when `ids` is given) columns.
fn keep_rows(batch: &RecordBatch, from: i64, to: i64, llm: bool, ids: Option<&HashSet<[u8; 16]>>) -> Result<BooleanArray, ArrowError> {
    let start = many::int64_col(batch, "start_time").map_err(ArrowError::ExternalError)?;
    let kinds = if llm { Some(many::column::<StringArray>(batch, "span_kind").map_err(ArrowError::ExternalError)?) } else { None };
    let traces = if ids.is_some() { Some(many::column::<FixedSizeBinaryArray>(batch, "trace_id").map_err(ArrowError::ExternalError)?) } else { None };
    let mut keep = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let in_window = start.is_valid(row) && start.value(row) >= from && start.value(row) < to;
        let is_llm = kinds.is_none_or(|k| k.is_valid(row) && k.value(row) == many::LLM);
        let in_ids = match (ids, traces) {
            (Some(set), Some(tr)) => tr.value(row).try_into().is_ok_and(|id: [u8; 16]| set.contains(&id)),
            _ => true,
        };
        keep.push(in_window && is_llm && in_ids);
    }
    Ok(BooleanArray::from(keep))
}

/// The fold of `traces` partials into `Traces`: the columns `trace_id`, `start_time`, `end_time`,
/// `cost_total`, and `extra` (a `session_id` or `user_id` column) when given.
fn partial_fold(extra: Option<&'static str>) -> FoldFn<Traces> {
    Arc::new(move |batch: &RecordBatch, out: &mut Traces| -> R<()> {
        let ids = many::column::<FixedSizeBinaryArray>(batch, "trace_id")?;
        let start = many::int64_col(batch, "start_time")?;
        let end = many::int64_col(batch, "end_time")?;
        let cost = many::column::<Float64Array>(batch, "cost_total")?;
        let text = extra.map(|name| many::column::<StringArray>(batch, name)).transpose()?;
        for row in 0..batch.num_rows() {
            let id: [u8; 16] = ids.value(row).try_into()?;
            let micro = if cost.is_valid(row) { (cost.value(row) * COST_SCALE).round() as i64 } else { 0 };
            let extra = text.and_then(|t| t.is_valid(row).then(|| t.value(row).to_owned()));
            out.add(id, Part { start: start.value(row), end: end.value(row), cost: micro, extra });
        }
        Ok(())
    })
}

/// The fold that counts rows.
fn count_fold() -> FoldFn<u64> {
    Arc::new(|batch: &RecordBatch, n: &mut u64| -> R<()> {
        *n += batch.num_rows() as u64;
        Ok(())
    })
}

/// The merged partials of every trace whose partials lie in the padded window of `[from, to)`.
async fn traces_in(files: &[FileRef], opened: &Opened, extra: Option<&'static str>, from: i64, to: i64) -> R<Traces> {
    let window = Window { from: from - PAD_NS, to: to + PAD_NS, llm_only: false, ids: None };
    let mut columns = vec!["trace_id", "start_time", "end_time", "cost_total"];
    columns.extend(extra);
    scan_fold(files, opened, &columns, &window, partial_fold(extra)).await
}

/// The names of the columns of the first opened file (none when no file is opened).
fn column_names(opened: &Opened) -> Vec<String> {
    match opened {
        Opened::Parquet(v) => v.first().map_or_else(Vec::new, |f| f.meta.schema().fields().iter().map(|x| x.name().to_owned()).collect()),
        Opened::Strata(v) => v.first().map_or_else(Vec::new, |f| f.schema().fields().iter().map(|x| x.name().to_owned()).collect()),
    }
}

/// `trace_list:<sort>`: the 50 traces of the 7-day window that sort first, then the detail fetch,
/// which counts the rows of those traces in the padded window over every `traces` column.
async fn trace_list(sort: Sort, files: &[FileRef], opened: &Opened, clock: i64) -> R<Answer> {
    let (from, to) = window_of(clock, 7);
    let Traces(merged) = traces_in(files, opened, None, from, to).await?;
    let mut kept: Vec<([u8; 16], Part)> = merged.into_iter().filter(|(_, p)| p.start >= from && p.start < to).collect();
    kept.sort_by(|(ia, a), (ib, b)| (sort.key(b), ib).cmp(&(sort.key(a), ia)));
    kept.truncate(TOP_TRACES);
    let ids: Vec<[u8; 16]> = kept.iter().map(|(id, _)| *id).collect();
    let count = if ids.is_empty() {
        0
    } else {
        let names = column_names(opened);
        let columns: Vec<&str> = names.iter().map(String::as_str).collect();
        let window = Window { from: from - PAD_NS, to: to + PAD_NS, llm_only: false, ids: Some(Arc::new(ids.clone())) };
        scan_fold(files, opened, &columns, &window, count_fold()).await?
    };
    let mut bytes: Vec<u8> = ids.iter().flatten().copied().collect();
    bytes.extend_from_slice(&count.to_le_bytes());
    Ok(many::answer_of(&bytes, ids.len() as u64))
}

/// One name's group of `sessions` or `users`.
struct Group {
    count: u64,
    start: i64,
    end: i64,
    cost: i64,
}

/// `sessions:7d` (`column` = `session_id`) and `users:30d` (`user_id`): the traces of the window,
/// grouped by their non-null `column`.
async fn rollup(column: &'static str, days: i64, files: &[FileRef], opened: &Opened, clock: i64) -> R<Answer> {
    let (from, to) = window_of(clock, days);
    let Traces(merged) = traces_in(files, opened, Some(column), from, to).await?;
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for (_, p) in merged {
        if p.start < from || p.start >= to {
            continue;
        }
        let Some(name) = p.extra else { continue };
        match groups.entry(name) {
            Entry::Vacant(e) => {
                e.insert(Group { count: 1, start: p.start, end: p.end, cost: p.cost });
            }
            Entry::Occupied(mut e) => {
                let g = e.get_mut();
                g.count += 1;
                g.start = g.start.min(p.start);
                g.end = g.end.max(p.end);
                g.cost = many::add(g.cost, p.cost)?;
            }
        }
    }
    let mut bytes = Vec::new();
    for (name, g) in &groups {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&g.count.to_le_bytes());
        bytes.extend_from_slice(&g.start.to_le_bytes());
        bytes.extend_from_slice(&g.end.to_le_bytes());
        bytes.extend_from_slice(&g.cost.to_le_bytes());
    }
    Ok(many::answer_of(&bytes, groups.len() as u64))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;
    use crate::cw::cases;
    use crate::cw::many::{Listing, list, open, read_project_case};
    use crate::cw::project::project_cmd;
    use crate::Location;

    /// A 9-day project of 400 traces a day with payloads, in its own directory, removed on drop.
    struct Fixture {
        base: PathBuf,
        loc: Location,
        clock: i64,
        pq: Listing,
        st: Listing,
    }

    impl Fixture {
        async fn new(test: &str) -> Fixture {
            let base = std::env::temp_dir().join(format!("cw-test-shapes-{test}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir_all(&base).unwrap();
            let case = base.join("case");
            fs::create_dir_all(&case).unwrap();
            let (spans, files) = cases::gen_spans("text_unique", Some(200)).unwrap();
            cases::write_case(&case, spans, &files).unwrap();
            let dir = base.join("p");
            let args = vec![
                dir.display().to_string(),
                "--days".into(),
                "9".into(),
                "--traces-per-day".into(),
                "400".into(),
                "--payloads".into(),
                case.display().to_string(),
            ];
            project_cmd(&args).await.unwrap();
            let loc = crate::location(&dir.display().to_string()).unwrap();
            let clock = read_project_case(&loc).await.unwrap().clock;
            let pq = list(&loc, "pq_cetra").await.unwrap();
            let st = list(&loc, "st").await.unwrap();
            Fixture { base, loc, clock, pq, st }
        }

        fn dir(&self) -> PathBuf {
            self.base.join("p")
        }

        fn shape(&self) -> ShapeCase {
            ShapeCase { clock: self.clock, needles: Vec::new() }
        }

        /// Runs `query` on the `traces` files of both formats, and returns both answers.
        async fn both(&self, query: &str) -> (Answer, Answer) {
            let shape = self.shape();
            let opened = open(&self.loc, "pq_cetra", &self.pq.traces).await.unwrap();
            let pq = run(query, &self.pq.traces, &opened, &shape).await.unwrap();
            let opened = open(&self.loc, "st", &self.st.traces).await.unwrap();
            let st = run(query, &self.st.traces, &opened, &shape).await.unwrap();
            (pq, st)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Every file under `dir` with extension `parquet`.
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("parquet") {
                out.push(p);
            }
        }
    }

    /// Every `traces` row of the project's Parquet files, read in full: (trace_id, start, end,
    /// cost micro-units).
    fn trace_rows(dir: &Path) -> Vec<([u8; 16], i64, i64, i64)> {
        let mut files = Vec::new();
        walk(&dir.join("pq_cetra/traces"), &mut files);
        let mut out: Vec<([u8; 16], i64, i64, i64)> = Vec::new();
        for file in files {
            let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(file).unwrap()).unwrap().build().unwrap();
            for batch in reader {
                let b = batch.unwrap();
                let ids = many::column::<FixedSizeBinaryArray>(&b, "trace_id").unwrap();
                let start = many::int64_col(&b, "start_time").unwrap();
                let end = many::int64_col(&b, "end_time").unwrap();
                let cost = many::column::<Float64Array>(&b, "cost_total").unwrap();
                for row in 0..b.num_rows() {
                    let micro = if cost.is_valid(row) { (cost.value(row) * COST_SCALE).round() as i64 } else { 0 };
                    out.push((ids.value(row).try_into().unwrap(), start.value(row), end.value(row), micro));
                }
            }
        }
        out
    }

    /// The trace list computed from every `traces` row read in full, with the sort written out
    /// here independently of `trace_list`.
    fn reference_trace_list(dir: &Path, query: &str, clock: i64) -> Answer {
        let (from, to) = (clock - 7 * many::DAY_NS, clock);
        let rows = trace_rows(dir);
        let mut merged: BTreeMap<[u8; 16], (i64, i64, i64)> = BTreeMap::new();
        for (id, start, end, cost) in &rows {
            let e = merged.entry(*id).or_insert((*start, *end, 0));
            e.0 = e.0.min(*start);
            e.1 = e.1.max(*end);
            e.2 += *cost;
        }
        let mut kept: Vec<([u8; 16], i64, i64, i64)> = merged
            .into_iter()
            .filter(|(_, (s, _, _))| *s >= from && *s < to)
            .map(|(id, (s, e, c))| (id, s, e, c))
            .collect();
        let key = |s: i64, e: i64, c: i64| match query {
            "trace_list:start_time" => s,
            "trace_list:cost_total" => c,
            _ => e - s,
        };
        kept.sort_by(|a, b| (key(b.1, b.2, b.3), b.0).cmp(&(key(a.1, a.2, a.3), a.0)));
        let ids: Vec<[u8; 16]> = kept.iter().take(TOP_TRACES).map(|k| k.0).collect();
        let count = rows
            .iter()
            .filter(|(id, s, _, _)| ids.contains(id) && *s >= from - PAD_NS && *s < to + PAD_NS)
            .count() as u64;
        let mut bytes: Vec<u8> = ids.iter().flatten().copied().collect();
        bytes.extend_from_slice(&count.to_le_bytes());
        many::answer_of(&bytes, ids.len() as u64)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trace_lists_agree() {
        let f = Fixture::new("trace-lists").await;
        for query in ["trace_list:start_time", "trace_list:cost_total", "trace_list:duration"] {
            let (pq, st) = f.both(query).await;
            assert_eq!(pq, st, "{query} differs between the formats");
            assert_eq!(pq.rows, TOP_TRACES as u64, "{query} found too few traces");
            let reference = reference_trace_list(&f.dir(), query, f.clock);
            assert_eq!(pq, reference, "{query} differs from the reference");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_and_users_agree() {
        let f = Fixture::new("rollups").await;
        for query in ["sessions:7d", "users:30d"] {
            let (pq, st) = f.both(query).await;
            assert_eq!(pq, st, "{query} differs between the formats");
            assert!(pq.rows >= 1, "{query} has no names");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scan_fold_respects_the_window() {
        let f = Fixture::new("window").await;
        let (from, to) = window_of(f.clock, 7);
        let window = Window { from, to, llm_only: false, ids: None };
        let columns = ["trace_id", "start_time"];
        let mut counts = Vec::new();
        for (format, listing) in [("pq_cetra", &f.pq), ("st", &f.st)] {
            let opened = open(&f.loc, format, &listing.traces).await.unwrap();
            let n: u64 = scan_fold(&listing.traces, &opened, &columns, &window, count_fold()).await.unwrap();
            counts.push(n);
        }
        let expected = trace_rows(&f.dir()).iter().filter(|(_, s, _, _)| *s >= from && *s < to).count() as u64;
        assert!(expected > 0, "the window holds no traces");
        assert_eq!(counts[0], counts[1], "the formats count different rows");
        assert_eq!(counts[0], expected, "the scan differs from the reference count");
    }
}
