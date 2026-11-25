//! `fmtbench cw` M4 queries on a project directory (plans/08-m4-query-shapes.md § M4.2 and § M4.3):
//! the trace list in three sorts, the sessions and users rollups (on the `traces` files), and the
//! facets, the dashboard series and the windowed search (on the `spans` files), of cetra's Parquet
//! and of their Strata conversions. Both formats run the same fold (`scan_fold`) with the same units
//! and parallelism, and the answers are encoded the same way.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use arrow::array::{
    Array, BooleanArray, FixedSizeBinaryArray, Float64Array, ListArray, MapArray, RecordBatch,
    StringArray, StructArray,
};
use arrow::error::ArrowError;
use futures::{StreamExt, TryStreamExt};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowPredicate, ArrowPredicateFn, RowFilter};
use parquet::arrow::async_reader::{ParquetObjectReader, ParquetRecordBatchStreamBuilder};
use parquet::file::statistics::Statistics;
use strata::{CmpOp, Lit, Predicate, ScanKind, ScanRequest, StrataFile};

use crate::R;
use crate::cw::many::{self, FileRef, Opened};
use crate::cw::read::{self, Answer};
use crate::pqfast::PqFile;

/// The trace list returns this many traces.
const TOP_TRACES: usize = 50;
/// Cost in micro-units (USD × 1e6, rounded per row).
const COST_SCALE: f64 = 1e6;
/// Trace scans read this much beyond the window on both sides (spec 03 § 6's known limit).
const PAD_NS: i64 = many::HOUR_NS;
/// The `distinct` facets (`user_id`, `session_id`) report this many of their largest counts.
const TOP_FACET_VALUES: usize = 20;
/// `bucket=auto` (spec 05 § Dashboard metrics): the widths in order, and at most this many buckets.
const BUCKET_WIDTHS: [i64; 9] = [
    60_000_000_000,
    5 * 60_000_000_000,
    10 * 60_000_000_000,
    15 * 60_000_000_000,
    30 * 60_000_000_000,
    many::HOUR_NS,
    6 * many::HOUR_NS,
    12 * many::HOUR_NS,
    many::DAY_NS,
];
const MAX_BUCKETS: i64 = 120;
/// The string facets of `facets:7d`: a null value counts as `""`.
const FACET_STRINGS: [&str; 5] = [
    "status",
    "span_kind",
    "environment",
    "request_model",
    "service_name",
];
/// The facets of `facets:7d` that count non-null values only and report a distinct count and a top list.
const DISTINCT_FACETS: [&str; 2] = ["user_id", "session_id"];
/// The columns `facets:7d` reads.
const FACET_COLUMNS: [&str; 12] = [
    "status",
    "span_kind",
    "environment",
    "request_model",
    "service_name",
    "user_id",
    "session_id",
    "tags",
    "duration_ns",
    "cost_total",
    "attributes",
    "metadata",
];
/// The columns `dashboard:<days>d` reads.
const DASHBOARD_COLUMNS: [&str; 7] = [
    "start_time",
    "request_model",
    "status",
    "cost_total",
    "duration_ns",
    "tags",
    "metadata",
];
/// The columns `search_1d:<name>` reads: the `SEARCH` columns of `read` and the row's ids.
const SEARCH_COLUMNS: [&str; 6] = [
    "trace_id",
    "span_id",
    "input",
    "output",
    "name",
    "system_instructions",
];

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

/// The M4 queries: the trace queries (`trace_list:<sort>`, `sessions:7d`, `users:30d`, on `traces`)
/// and the spans queries (`facets:7d`, `dashboard:7d`, `dashboard:30d`, `search_1d:<name>`, on `spans`).
pub(crate) fn is_shape(query: &str) -> bool {
    query.starts_with("trace_list:")
        || query.starts_with("search_1d:")
        || matches!(
            query,
            "sessions:7d" | "users:30d" | "facets:7d" | "dashboard:7d" | "dashboard:30d"
        )
}

/// The table a query reads: `traces` for the trace list, sessions and users; `spans` for the rest.
pub(crate) fn table_of(query: &str) -> &'static str {
    if query.starts_with("trace_list:")
        || query.starts_with("sessions:")
        || query.starts_with("users:")
    {
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
        _ => Err(format!(
            "unknown trace list sort in {query}: want start_time, cost_total or duration"
        )
        .into()),
    }
}

/// One M4 query's answer on the files of `table_of(query)`, `opened` index-aligned with `files`.
/// The answer's bytes are per query:
/// - `trace_list:<sort>` (window 7d): the 50 trace ids in order, 16 bytes each, then the row count
///   `u64` of those traces' `traces` rows in the padded window (rows = 50);
/// - `sessions:7d`, `users:30d`: per non-null `session_id` or `user_id` sorted bytewise: the name's
///   bytes, `0x00`, trace count `u64`, min start `i64`, max end `i64`, cost micro-units `i64`,
///   little-endian (rows = names);
/// - `facets:7d`, `dashboard:7d`, `dashboard:30d`: their lines, sorted bytewise and joined by `\n`
///   (rows = lines), as `facet_lines` and `dashboard_lines` describe them;
/// - `search_1d:<name>`: the `(trace_id, span_id)` pairs of the rows of the last day that match the
///   needle `<name>` of `case.needles`, sorted, 24 bytes each (rows = pairs).
pub(crate) async fn run(
    query: &str,
    files: &[FileRef],
    opened: &Opened,
    case: &ShapeCase,
) -> R<Answer> {
    if query.starts_with("trace_list:") {
        return trace_list(sort_of(query)?, files, opened, case.clock).await;
    }
    if let Some(name) = query.strip_prefix("search_1d:") {
        return search_1d(name, files, opened, case).await;
    }
    match query {
        "sessions:7d" => rollup("session_id", 7, files, opened, case.clock).await,
        "users:30d" => rollup("user_id", 30, files, opened, case.clock).await,
        "facets:7d" => Ok(lines_answer(&facets(files, opened, case.clock).await?)),
        "dashboard:7d" => Ok(lines_answer(
            &dashboard(7, files, opened, case.clock).await?,
        )),
        "dashboard:30d" => Ok(lines_answer(
            &dashboard(30, files, opened, case.clock).await?,
        )),
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
    let Some(leaf) = f
        .meta
        .parquet_schema()
        .columns()
        .iter()
        .position(|c| c.path().string() == "start_time")
    else {
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
    let ids: Option<Arc<HashSet<[u8; 16]>>> = window
        .ids
        .as_ref()
        .map(|v| Arc::new(v.iter().copied().collect()));
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
            let cols: Vec<usize> = columns
                .iter()
                .map(|n| schema.index_of(n))
                .collect::<Result<_, _>>()?;
            let mut keys = vec!["start_time"];
            if window.llm_only {
                keys.push("span_kind");
            }
            if window.ids.is_some() {
                keys.push("trace_id");
            }
            let key_cols: Vec<usize> = keys
                .iter()
                .map(|n| schema.index_of(n))
                .collect::<Result<_, _>>()?;
            let (from, to, llm) = (window.from, window.to, window.llm_only);
            let predicate = ArrowPredicateFn::new(
                ProjectionMask::roots(f.meta.parquet_schema(), key_cols),
                move |batch: RecordBatch| keep_rows(&batch, from, to, llm, ids.as_deref()),
            );
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
            let projection: Vec<usize> = columns
                .iter()
                .map(|n| schema.index_of(n))
                .collect::<Result<_, _>>()?;
            let mut terms = vec![
                Predicate::Cmp(
                    schema.index_of("start_time")?,
                    CmpOp::GtEq,
                    Lit::Int(window.from),
                ),
                Predicate::Cmp(
                    schema.index_of("start_time")?,
                    CmpOp::Lt,
                    Lit::Int(window.to),
                ),
            ];
            if window.llm_only {
                terms.push(Predicate::Cmp(
                    schema.index_of("span_kind")?,
                    CmpOp::Eq,
                    Lit::Utf8(many::LLM.to_owned()),
                ));
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
fn keep_rows(
    batch: &RecordBatch,
    from: i64,
    to: i64,
    llm: bool,
    ids: Option<&HashSet<[u8; 16]>>,
) -> Result<BooleanArray, ArrowError> {
    let start = many::int64_col(batch, "start_time").map_err(ArrowError::ExternalError)?;
    let kinds = if llm {
        Some(many::column::<StringArray>(batch, "span_kind").map_err(ArrowError::ExternalError)?)
    } else {
        None
    };
    let traces = if ids.is_some() {
        Some(
            many::column::<FixedSizeBinaryArray>(batch, "trace_id")
                .map_err(ArrowError::ExternalError)?,
        )
    } else {
        None
    };
    let mut keep = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        let in_window = start.is_valid(row) && start.value(row) >= from && start.value(row) < to;
        let is_llm = kinds.is_none_or(|k| k.is_valid(row) && k.value(row) == many::LLM);
        let in_ids = match (ids, traces) {
            (Some(set), Some(tr)) => tr
                .value(row)
                .try_into()
                .is_ok_and(|id: [u8; 16]| set.contains(&id)),
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
        let text = extra
            .map(|name| many::column::<StringArray>(batch, name))
            .transpose()?;
        for row in 0..batch.num_rows() {
            let id: [u8; 16] = ids.value(row).try_into()?;
            let micro = if cost.is_valid(row) {
                (cost.value(row) * COST_SCALE).round() as i64
            } else {
                0
            };
            let extra = text.and_then(|t| t.is_valid(row).then(|| t.value(row).to_owned()));
            out.add(
                id,
                Part {
                    start: start.value(row),
                    end: end.value(row),
                    cost: micro,
                    extra,
                },
            );
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
async fn traces_in(
    files: &[FileRef],
    opened: &Opened,
    extra: Option<&'static str>,
    from: i64,
    to: i64,
) -> R<Traces> {
    let window = Window {
        from: from - PAD_NS,
        to: to + PAD_NS,
        llm_only: false,
        ids: None,
    };
    let mut columns = vec!["trace_id", "start_time", "end_time", "cost_total"];
    columns.extend(extra);
    scan_fold(files, opened, &columns, &window, partial_fold(extra)).await
}

/// The names of the columns of the first opened file (none when no file is opened).
fn column_names(opened: &Opened) -> Vec<String> {
    match opened {
        Opened::Parquet(v) => v.first().map_or_else(Vec::new, |f| {
            f.meta
                .schema()
                .fields()
                .iter()
                .map(|x| x.name().to_owned())
                .collect()
        }),
        Opened::Strata(v) => v.first().map_or_else(Vec::new, |f| {
            f.schema()
                .fields()
                .iter()
                .map(|x| x.name().to_owned())
                .collect()
        }),
    }
}

/// `trace_list:<sort>`: the 50 traces of the 7-day window that sort first, then the detail fetch,
/// which counts the rows of those traces in the padded window over every `traces` column.
async fn trace_list(sort: Sort, files: &[FileRef], opened: &Opened, clock: i64) -> R<Answer> {
    let (from, to) = window_of(clock, 7);
    let Traces(merged) = traces_in(files, opened, None, from, to).await?;
    let mut kept: Vec<([u8; 16], Part)> = merged
        .into_iter()
        .filter(|(_, p)| p.start >= from && p.start < to)
        .collect();
    kept.sort_by(|(ia, a), (ib, b)| (sort.key(b), ib).cmp(&(sort.key(a), ia)));
    kept.truncate(TOP_TRACES);
    let ids: Vec<[u8; 16]> = kept.iter().map(|(id, _)| *id).collect();
    let count = if ids.is_empty() {
        0
    } else {
        let names = column_names(opened);
        let columns: Vec<&str> = names.iter().map(String::as_str).collect();
        let window = Window {
            from: from - PAD_NS,
            to: to + PAD_NS,
            llm_only: false,
            ids: Some(Arc::new(ids.clone())),
        };
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
async fn rollup(
    column: &'static str,
    days: i64,
    files: &[FileRef],
    opened: &Opened,
    clock: i64,
) -> R<Answer> {
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
                e.insert(Group {
                    count: 1,
                    start: p.start,
                    end: p.end,
                    cost: p.cost,
                });
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

/// The answer of sorted `lines`, joined by `\n`: rows = lines.
fn lines_answer(lines: &[String]) -> Answer {
    many::answer_of(lines.join("\n").as_bytes(), lines.len() as u64)
}

/// The text of a row of a nullable string column: `""` for null.
fn text_at(col: &StringArray, row: usize) -> &str {
    if col.is_valid(row) {
        col.value(row)
    } else {
        ""
    }
}

/// The non-null keys of the entries of one `Map<Utf8, Utf8>` row, one per entry.
fn map_keys(entries: &StructArray) -> R<Vec<String>> {
    let keys = entries
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or("map keys are not Utf8")?;
    Ok((0..keys.len())
        .filter(|&i| keys.is_valid(i))
        .map(|i| keys.value(i).to_owned())
        .collect())
}

/// The value of `key` in the entries of one `Map<Utf8, Utf8>` row; `None` when the key is absent or
/// its value is null. The first entry with the key counts.
fn map_text(entries: &StructArray, key: &str) -> R<Option<String>> {
    let keys = entries
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or("map keys are not Utf8")?;
    let values = entries
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or("map values are not Utf8")?;
    let found = (0..keys.len()).find(|&i| keys.is_valid(i) && keys.value(i) == key);
    Ok(found.and_then(|i| values.is_valid(i).then(|| values.value(i).to_owned())))
}

/// Adds one to the count of `(facet, value)`.
fn bump(counts: &mut BTreeMap<(&'static str, String), u64>, facet: &'static str, value: String) {
    *counts.entry((facet, value)).or_insert(0) += 1;
}

/// The log2 bucket of a duration: `floor(log2 v)` for `v > 0`, −1 for null or `v ≤ 0`.
fn duration_log2(v: Option<i64>) -> i64 {
    match v {
        Some(v) if v > 0 => 63 - i64::from(v.leading_zeros()),
        _ => -1,
    }
}

/// The decade bucket of a cost: `floor(log10 c)` for `c > 0`, −99 for null or `c ≤ 0`.
fn cost_decade(c: Option<f64>) -> i64 {
    match c {
        Some(c) if c > 0.0 => c.log10().floor() as i64,
        _ => -99,
    }
}

/// A cost in micro-units: USD × 1e6, rounded.
fn micro(cost: f64) -> i64 {
    (cost * COST_SCALE).round() as i64
}

/// The counts of `facets:7d`, keyed by facet name and value (histogram buckets as decimal text).
#[derive(Default)]
struct Facets(BTreeMap<(&'static str, String), u64>);

impl Fold for Facets {
    fn merge(&mut self, other: Self) {
        for (key, n) in other.0 {
            *self.0.entry(key).or_insert(0) += n;
        }
    }
}

/// The fold of `facets:7d`: every batch's rows, counted per facet (see `facet_lines`).
fn facets_fold() -> FoldFn<Facets> {
    Arc::new(|batch: &RecordBatch, out: &mut Facets| -> R<()> {
        for facet in FACET_STRINGS {
            let col = many::column::<StringArray>(batch, facet)?;
            for row in 0..batch.num_rows() {
                bump(&mut out.0, facet, text_at(col, row).to_owned());
            }
        }
        for facet in DISTINCT_FACETS {
            let col = many::column::<StringArray>(batch, facet)?;
            for row in 0..batch.num_rows() {
                if col.is_valid(row) {
                    bump(&mut out.0, facet, col.value(row).to_owned());
                }
            }
        }
        let tags = many::column::<ListArray>(batch, "tags")?;
        for row in 0..batch.num_rows() {
            if tags.is_valid(row) {
                let items = tags.value(row);
                let items = items
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or("tags are not Utf8")?;
                for i in 0..items.len() {
                    if items.is_valid(i) {
                        bump(&mut out.0, "tag", items.value(i).to_owned());
                    }
                }
            }
        }
        let durations = many::int64_col(batch, "duration_ns")?;
        let costs = many::column::<Float64Array>(batch, "cost_total")?;
        for row in 0..batch.num_rows() {
            let d = durations.is_valid(row).then(|| durations.value(row));
            bump(&mut out.0, "duration_log2", duration_log2(d).to_string());
            let c = costs.is_valid(row).then(|| costs.value(row));
            bump(&mut out.0, "cost_decade", cost_decade(c).to_string());
        }
        let attrs = many::column::<MapArray>(batch, "attributes")?;
        let meta = many::column::<MapArray>(batch, "metadata")?;
        for row in 0..batch.num_rows() {
            if attrs.is_valid(row) {
                for key in map_keys(&attrs.value(row))? {
                    bump(&mut out.0, "attr_key", key);
                }
            }
            if meta.is_valid(row) {
                for key in map_keys(&meta.value(row))? {
                    bump(&mut out.0, "meta_key", key);
                }
            }
        }
        Ok(())
    })
}

/// The lines of `facets:7d`, sorted bytewise. Each counted value is `<facet>\t<value>\t<count>`
/// (a null string value is `""`). For `user_id` and `session_id` instead: `<facet>\t#distinct\t<n>`
/// and `<facet>\ttop\t<rank:02>\t<value>\t<count>` for the 20 largest counts (count descending, then
/// value ascending; rank 01 is the largest).
fn facet_lines(counts: &Facets) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for ((facet, value), n) in &counts.0 {
        if !DISTINCT_FACETS.contains(facet) {
            lines.push(format!("{facet}\t{value}\t{n}"));
        }
    }
    for facet in DISTINCT_FACETS {
        let mut values: Vec<(&str, u64)> = counts
            .0
            .iter()
            .filter(|((f, _), _)| *f == facet)
            .map(|((_, v), n)| (v.as_str(), *n))
            .collect();
        lines.push(format!("{facet}\t#distinct\t{}", values.len()));
        values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        for (rank, (value, n)) in values.iter().take(TOP_FACET_VALUES).enumerate() {
            lines.push(format!("{facet}\ttop\t{:02}\t{value}\t{n}", rank + 1));
        }
    }
    lines.sort_unstable();
    lines
}

/// `facets:7d`: the facets of the spans of the 7-day window, unpadded (spans are not padded).
async fn facets(files: &[FileRef], opened: &Opened, clock: i64) -> R<Vec<String>> {
    let (from, to) = window_of(clock, 7);
    let window = Window {
        from,
        to,
        llm_only: false,
        ids: None,
    };
    let counts = scan_fold(files, opened, &FACET_COLUMNS, &window, facets_fold()).await?;
    Ok(facet_lines(&counts))
}

/// `bucket=auto` (spec 05 § Dashboard metrics): the smallest width whose buckets, aligned to the
/// epoch, number at most 120 over `[from, to)`; else one day.
fn bucket_width(from: i64, to: i64) -> i64 {
    let buckets = |w: i64| (to - 1).div_euclid(w) - from.div_euclid(w) + 1;
    BUCKET_WIDTHS
        .into_iter()
        .find(|&w| buckets(w) <= MAX_BUCKETS)
        .unwrap_or(many::DAY_NS)
}

/// One (bucket, `request_model`) group of the dashboard.
#[derive(Default)]
struct Model {
    count: u64,
    errors: u64,
    cost: i64,
    durations: Vec<i64>,
}

/// The dashboard's partials: the model groups, the tag counts and the plan counts, by bucket start.
#[derive(Default)]
struct Dash {
    models: BTreeMap<(i64, String), Model>,
    tags: BTreeMap<(i64, String), u64>,
    plans: BTreeMap<(i64, String), u64>,
}

impl Fold for Dash {
    fn merge(&mut self, other: Self) {
        for (key, m) in other.models {
            let into = self.models.entry(key).or_default();
            into.count += m.count;
            into.errors += m.errors;
            into.cost = into.cost.saturating_add(m.cost);
            into.durations.extend(m.durations);
        }
        for (key, n) in other.tags {
            *self.tags.entry(key).or_insert(0) += n;
        }
        for (key, n) in other.plans {
            *self.plans.entry(key).or_insert(0) += n;
        }
    }
}

/// The fold of the dashboard for buckets of `width`: each LLM row goes to its bucket's groups.
fn dashboard_fold(width: i64) -> FoldFn<Dash> {
    Arc::new(move |batch: &RecordBatch, out: &mut Dash| -> R<()> {
        let start = many::int64_col(batch, "start_time")?;
        let models = many::column::<StringArray>(batch, "request_model")?;
        let status = many::column::<StringArray>(batch, "status")?;
        let costs = many::column::<Float64Array>(batch, "cost_total")?;
        let durations = many::int64_col(batch, "duration_ns")?;
        let tags = many::column::<ListArray>(batch, "tags")?;
        let metadata = many::column::<MapArray>(batch, "metadata")?;
        for row in 0..batch.num_rows() {
            let bucket = start.value(row).div_euclid(width) * width;
            let m = out
                .models
                .entry((bucket, text_at(models, row).to_owned()))
                .or_default();
            m.count += 1;
            if status.is_valid(row) && status.value(row) == "error" {
                m.errors += 1;
            }
            if costs.is_valid(row) {
                m.cost = m.cost.saturating_add(micro(costs.value(row)));
            }
            if durations.is_valid(row) {
                m.durations.push(durations.value(row));
            }
            if tags.is_valid(row) {
                let items = tags.value(row);
                let items = items
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or("tags are not Utf8")?;
                for i in 0..items.len() {
                    if items.is_valid(i) {
                        *out.tags
                            .entry((bucket, items.value(i).to_owned()))
                            .or_insert(0) += 1;
                    }
                }
            }
            let plan = if metadata.is_valid(row) {
                map_text(&metadata.value(row), "plan")?.unwrap_or_default()
            } else {
                String::new()
            };
            *out.plans.entry((bucket, plan)).or_insert(0) += 1;
        }
        Ok(())
    })
}

/// The lines of a dashboard, sorted bytewise: `m\t<bucket start>\t<model>\t<count>\t<errors>\t<cost>\t<p95>`
/// (p95 by nearest rank over the sorted durations, `-` when there are none), `t\t<bucket start>\t<tag>\t<count>`
/// and `k\t<bucket start>\t<plan>\t<count>`.
fn dashboard_lines(dash: Dash) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for ((bucket, model), mut m) in dash.models {
        m.durations.sort_unstable();
        let n = m.durations.len();
        let p95 = if n == 0 {
            "-".to_owned()
        } else {
            m.durations[(95 * n).div_ceil(100) - 1].to_string()
        };
        lines.push(format!(
            "m\t{bucket}\t{model}\t{}\t{}\t{}\t{p95}",
            m.count, m.errors, m.cost
        ));
    }
    for ((bucket, tag), n) in dash.tags {
        lines.push(format!("t\t{bucket}\t{tag}\t{n}"));
    }
    for ((bucket, plan), n) in dash.plans {
        lines.push(format!("k\t{bucket}\t{plan}\t{n}"));
    }
    lines.sort_unstable();
    lines
}

/// `dashboard:<days>d`: the LLM spans of the `days`-day window, unpadded, in buckets of `bucket_width`.
async fn dashboard(days: i64, files: &[FileRef], opened: &Opened, clock: i64) -> R<Vec<String>> {
    let (from, to) = window_of(clock, days);
    let window = Window {
        from,
        to,
        llm_only: true,
        ids: None,
    };
    let dash = scan_fold(
        files,
        opened,
        &DASHBOARD_COLUMNS,
        &window,
        dashboard_fold(bucket_width(from, to)),
    )
    .await?;
    Ok(dashboard_lines(dash))
}

/// The (trace_id, span_id) pairs of one scan's matching rows.
#[derive(Default)]
struct Pairs(Vec<([u8; 16], [u8; 8])>);

impl Fold for Pairs {
    fn merge(&mut self, other: Self) {
        self.0.extend(other.0);
    }
}

/// The fold of `search_1d`: the rows of each batch that `read::search_mask` keeps for `pattern`.
fn search_fold(pattern: String) -> FoldFn<Pairs> {
    Arc::new(move |batch: &RecordBatch, out: &mut Pairs| -> R<()> {
        let mask = read::search_mask(batch, &pattern)?;
        let traces = many::column::<FixedSizeBinaryArray>(batch, "trace_id")?;
        let spans = many::column::<FixedSizeBinaryArray>(batch, "span_id")?;
        for row in 0..batch.num_rows() {
            if mask.is_valid(row) && mask.value(row) {
                out.0
                    .push((traces.value(row).try_into()?, spans.value(row).try_into()?));
            }
        }
        Ok(())
    })
}

/// The answer of sorted `(trace_id, span_id)` pairs: 24 bytes each, rows = pairs.
fn pairs_answer(pairs: &[([u8; 16], [u8; 8])]) -> Answer {
    let bytes: Vec<u8> = pairs
        .iter()
        .flat_map(|(t, s)| t.iter().chain(s.iter()).copied())
        .collect();
    many::answer_of(&bytes, pairs.len() as u64)
}

/// `search_1d:<name>`: the spans of the last day whose `SEARCH` columns match the needle `name` of
/// `case.needles`, scanned on both formats (no index). An unknown name is an error.
async fn search_1d(name: &str, files: &[FileRef], opened: &Opened, case: &ShapeCase) -> R<Answer> {
    let (_, needle) = case
        .needles
        .iter()
        .find(|(n, _)| n.as_str() == name)
        .ok_or_else(|| format!("no needle named {name} in cw_needles.txt"))?;
    let (from, to) = window_of(case.clock, 1);
    let window = Window {
        from,
        to,
        llm_only: false,
        ids: None,
    };
    let Pairs(mut pairs) = scan_fold(
        files,
        opened,
        &SEARCH_COLUMNS,
        &window,
        search_fold(read::pattern(needle)),
    )
    .await?;
    pairs.sort_unstable();
    Ok(pairs_answer(&pairs))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use arrow::array::Scalar;
    use arrow::compute::kernels::comparison::ilike;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;
    use crate::Location;
    use crate::cw::cases;
    use crate::cw::harness::read_case;
    use crate::cw::many::{Listing, list, open, read_project_case};
    use crate::cw::project::project_cmd;

    /// A 9-day project of 400 traces a day with payloads, in its own directory, removed on drop.
    struct Fixture {
        base: PathBuf,
        loc: Location,
        clock: i64,
        needles: Vec<(String, String)>,
        pq: Listing,
        st: Listing,
    }

    impl Fixture {
        async fn new(test: &str) -> Fixture {
            let base =
                std::env::temp_dir().join(format!("cw-test-shapes-{test}-{}", std::process::id()));
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
            let needles = read_case(&loc).await.unwrap().needles;
            let pq = list(&loc, "pq_cetra").await.unwrap();
            let st = list(&loc, "st").await.unwrap();
            Fixture {
                base,
                loc,
                clock,
                needles,
                pq,
                st,
            }
        }

        fn dir(&self) -> PathBuf {
            self.base.join("p")
        }

        fn shape(&self) -> ShapeCase {
            ShapeCase {
                clock: self.clock,
                needles: self.needles.clone(),
            }
        }

        /// Runs `query` on the files of its table (`table_of`) of both formats, and returns both answers.
        async fn both(&self, query: &str) -> (Answer, Answer) {
            let shape = self.shape();
            let (pq_files, st_files) = if table_of(query) == "traces" {
                (&self.pq.traces, &self.st.traces)
            } else {
                (&self.pq.spans, &self.st.spans)
            };
            let opened = open(&self.loc, "pq_cetra", pq_files).await.unwrap();
            let pq = run(query, pq_files, &opened, &shape).await.unwrap();
            let opened = open(&self.loc, "st", st_files).await.unwrap();
            let st = run(query, st_files, &opened, &shape).await.unwrap();
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
            let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(file).unwrap())
                .unwrap()
                .build()
                .unwrap();
            for batch in reader {
                let b = batch.unwrap();
                let ids = many::column::<FixedSizeBinaryArray>(&b, "trace_id").unwrap();
                let start = many::int64_col(&b, "start_time").unwrap();
                let end = many::int64_col(&b, "end_time").unwrap();
                let cost = many::column::<Float64Array>(&b, "cost_total").unwrap();
                for row in 0..b.num_rows() {
                    let micro = if cost.is_valid(row) {
                        (cost.value(row) * COST_SCALE).round() as i64
                    } else {
                        0
                    };
                    out.push((
                        ids.value(row).try_into().unwrap(),
                        start.value(row),
                        end.value(row),
                        micro,
                    ));
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
        for query in [
            "trace_list:start_time",
            "trace_list:cost_total",
            "trace_list:duration",
        ] {
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
        let window = Window {
            from,
            to,
            llm_only: false,
            ids: None,
        };
        let columns = ["trace_id", "start_time"];
        let mut counts = Vec::new();
        for (format, listing) in [("pq_cetra", &f.pq), ("st", &f.st)] {
            let opened = open(&f.loc, format, &listing.traces).await.unwrap();
            let n: u64 = scan_fold(&listing.traces, &opened, &columns, &window, count_fold())
                .await
                .unwrap();
            counts.push(n);
        }
        let expected = trace_rows(&f.dir())
            .iter()
            .filter(|(_, s, _, _)| *s >= from && *s < to)
            .count() as u64;
        assert!(expected > 0, "the window holds no traces");
        assert_eq!(counts[0], counts[1], "the formats count different rows");
        assert_eq!(
            counts[0], expected,
            "the scan differs from the reference count"
        );
    }

    #[test]
    fn bucket_width_follows_the_rule() {
        assert_eq!(bucket_width(0, 7 * many::DAY_NS), 6 * many::HOUR_NS);
        assert_eq!(
            bucket_width(many::HOUR_NS, 30 * many::DAY_NS + many::HOUR_NS),
            12 * many::HOUR_NS
        );
        assert_eq!(bucket_width(0, 30 * many::DAY_NS), 6 * many::HOUR_NS);
        // 121 six-hour buckets do not fit, so the width doubles.
        assert_eq!(bucket_width(0, 121 * 6 * many::HOUR_NS), 12 * many::HOUR_NS);
        // More than 120 days of one-day buckets: still one day.
        assert_eq!(bucket_width(0, 200 * many::DAY_NS), many::DAY_NS);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn facets_agree() {
        let f = Fixture::new("facets").await;
        let (pq, st) = f.both("facets:7d").await;
        assert_eq!(pq, st, "facets:7d differs between the formats");
        let opened = open(&f.loc, "pq_cetra", &f.pq.spans).await.unwrap();
        let lines = facets(&f.pq.spans, &opened, f.clock).await.unwrap();
        assert_eq!(lines.len() as u64, pq.rows);
        let (from, to) = window_of(f.clock, 7);
        let window = Window {
            from,
            to,
            llm_only: false,
            ids: None,
        };
        let spans: u64 = scan_fold(&f.pq.spans, &opened, &["start_time"], &window, count_fold())
            .await
            .unwrap();
        assert!(spans > 0, "the window holds no spans");
        let by_kind: u64 = lines
            .iter()
            .filter_map(|l| l.strip_prefix("span_kind\t"))
            .map(|rest| rest.rsplit('\t').next().unwrap().parse::<u64>().unwrap())
            .sum();
        assert_eq!(
            by_kind, spans,
            "the span_kind counts differ from the window's span count"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dashboards_agree() {
        let f = Fixture::new("dashboards").await;
        for (query, days) in [("dashboard:7d", 7), ("dashboard:30d", 30)] {
            let (pq, st) = f.both(query).await;
            assert_eq!(pq, st, "{query} differs between the formats");
            let opened = open(&f.loc, "pq_cetra", &f.pq.spans).await.unwrap();
            let lines = dashboard(days, &f.pq.spans, &opened, f.clock)
                .await
                .unwrap();
            assert_eq!(lines.len() as u64, pq.rows);
            let (from, to) = window_of(f.clock, days);
            let width = bucket_width(from, to);
            let models: Vec<&String> = lines.iter().filter(|l| l.starts_with("m\t")).collect();
            assert!(!models.is_empty(), "{query} has no model line");
            for line in models {
                let bucket: i64 = line.split('\t').nth(1).unwrap().parse().unwrap();
                assert_eq!(
                    bucket.rem_euclid(width),
                    0,
                    "{query}: a bucket start is not on the width"
                );
            }
        }
    }

    /// The reference for `search_1d`: every `spans` row of the last day read in full, matched per
    /// row and per `SEARCH` column with `ilike` on a one-value array.
    fn reference_search(dir: &Path, needle: &str, clock: i64) -> Answer {
        let (from, to) = window_of(clock, 1);
        let pattern = Scalar::new(StringArray::from(vec![read::pattern(needle)]));
        let mut files = Vec::new();
        walk(&dir.join("pq_cetra/spans"), &mut files);
        let mut pairs: Vec<([u8; 16], [u8; 8])> = Vec::new();
        for file in files {
            let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(file).unwrap())
                .unwrap()
                .build()
                .unwrap();
            for batch in reader {
                let b = batch.unwrap();
                let traces = many::column::<FixedSizeBinaryArray>(&b, "trace_id").unwrap();
                let spans = many::column::<FixedSizeBinaryArray>(&b, "span_id").unwrap();
                let start = many::int64_col(&b, "start_time").unwrap();
                let cols: Vec<&StringArray> = ["input", "output", "name", "system_instructions"]
                    .iter()
                    .map(|c| many::column::<StringArray>(&b, c).unwrap())
                    .collect();
                for row in 0..b.num_rows() {
                    if !(start.is_valid(row) && start.value(row) >= from && start.value(row) < to) {
                        continue;
                    }
                    let hit = cols.iter().any(|col| {
                        let one =
                            StringArray::from(vec![col.is_valid(row).then(|| col.value(row))]);
                        let res = ilike(&one, &pattern).unwrap();
                        res.is_valid(0) && res.value(0)
                    });
                    if hit {
                        pairs.push((
                            traces.value(row).try_into().unwrap(),
                            spans.value(row).try_into().unwrap(),
                        ));
                    }
                }
            }
        }
        pairs.sort_unstable();
        pairs_answer(&pairs)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn window_searches_agree() {
        let f = Fixture::new("searches").await;
        for name in ["bug", "common", "absent"] {
            let query = format!("search_1d:{name}");
            let (pq, st) = f.both(&query).await;
            assert_eq!(pq, st, "{query} differs between the formats");
            let needle = &f.needles.iter().find(|(n, _)| n == name).unwrap().1;
            assert_eq!(
                pq,
                reference_search(&f.dir(), needle, f.clock),
                "{query} differs from the reference"
            );
            if name == "absent" {
                assert_eq!(pq.rows, 0, "the absent needle matched");
            } else {
                assert!(pq.rows > 0, "{query} matched no span");
            }
        }
        let shape = f.shape();
        let opened = open(&f.loc, "pq_cetra", &f.pq.spans).await.unwrap();
        assert!(
            run("search_1d:no-such-needle", &f.pq.spans, &opened, &shape)
                .await
                .is_err()
        );
    }
}
