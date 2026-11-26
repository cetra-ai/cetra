//! External Parquet + Vortex Benchmark Suites:
//! - Suite A (`clickbench`): Real ClickBench `hits_1.parquet` (100k rows x 105 cols)
//! - Suite B (`row-filter`): Parquet's 6-Pattern Selectivity x Clustering Matrix (100k rows x 10 cols)
//! - Suite C (`random-access`): Vortex's scattered multi-row take (100 & 1,000 rows from 250k x 12 cols)

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
    StringBuilder,
};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use bytes::Bytes;
use cetra_column::Batch;
use cetra_statam::{
    CoalesceMode, IdLookup, Predicate as StatamPred, RangeReader, ScanRequest, SimulatedS3Config,
    SimulatedS3Reader, SliceReader, StatamReader, StatamWriter, SubstringQuery, TableKind,
    WriteOptions, WriteProfile, ZonePred, ZoneValue,
};
use futures::stream::TryStreamExt;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowPredicate, ArrowPredicateFn, ArrowReaderMetadata, ArrowReaderOptions,
    ParquetRecordBatchReaderBuilder, RowFilter, RowSelection, RowSelector,
};
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::bloom_filter::Sbbf;
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaData, RowGroupMetaData};
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::reader::ChunkReader;
use parquet::file::statistics::Statistics;
use parquet::schema::types::SchemaDescriptor;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use strata::{
    CmpOp, Config as StrataConfig, Lit, Predicate as StrataPred, ScanKind,
    ScanRequest as StrataScanRequest, StrataFile, StrataWriter,
};
use vortex::buffer::Buffer;
use vortex::expr::{Expression, and, col, eq, gt_eq, lit, lt_eq, not, root, select};
use vortex::file::{OpenOptionsSessionExt, VortexFile};
use vortex::session::VortexSession;

use statam_bench7::queries::{ALL_FORMATS, check_parity, measure_local_async_ms, measure_local_ms};
use statam_bench7::report::{QueryBenchRow, WriteBenchRow, format_kib};
use statam_bench7::s3_parquet::{SimS3ChunkReader, find_leaf_idx, open_parquet_builder};
use statam_bench7::schema::{
    BenchResult, ParquetVariant, arrow_to_cetra_batch, generic_parquet_writer_props,
    normalize_arrow_batch,
};
use statam_bench7::strata_bench::{SimS3ObjectStore, SliceObjectStore};
use statam_bench7::vortex_bench::{
    SimS3VortexRead, chunk_to_record_batch, new_vortex_session, open_vortex_local, str_value_at,
    write_vortex_files,
};
use statam_bench7::workloads::{BenchScale, DAY_START};

const DEFAULT_EXTERNAL_FORMATS: [&str; 6] = [
    "pq_default",
    "pq_tuned",
    "vortex",
    "strata",
    "statam_l0",
    "statam_l1",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuiteSelection {
    All,
    ClickBench,
    RowFilter,
    RandomAccess,
}

impl SuiteSelection {
    fn includes_clickbench(self) -> bool {
        matches!(self, Self::All | Self::ClickBench)
    }

    fn includes_row_filter(self) -> bool {
        matches!(self, Self::All | Self::RowFilter)
    }

    fn includes_random_access(self) -> bool {
        matches!(self, Self::All | Self::RandomAccess)
    }
}

struct CliConfig {
    suite: SuiteSelection,
    statam_write_threads: usize,
    statam_read_threads: usize,
    hits_parquet: PathBuf,
    formats: Vec<&'static str>,
}

fn default_hits_path() -> PathBuf {
    if let Ok(p) = std::env::var("STATAM_HITS_PARQUET")
        && !p.trim().is_empty()
    {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home)
        .join("statam-corpora")
        .join("hits_1.parquet")
}

fn parse_formats_csv(raw: &str) -> BenchResult<Vec<&'static str>> {
    let requested: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if requested.is_empty() {
        return Ok(DEFAULT_EXTERNAL_FORMATS.to_vec());
    }
    for r in &requested {
        if !ALL_FORMATS.contains(r) {
            return Err(format!(
                "unknown format '{r}' (expected subset of {:?})",
                ALL_FORMATS
            )
            .into());
        }
    }
    Ok(ALL_FORMATS
        .iter()
        .copied()
        .filter(|f| requested.contains(f))
        .collect())
}

fn parse_suite(val: &str) -> BenchResult<SuiteSelection> {
    match val.trim() {
        "all" => Ok(SuiteSelection::All),
        "clickbench" => Ok(SuiteSelection::ClickBench),
        "row-filter" | "row_filter" => Ok(SuiteSelection::RowFilter),
        "random-access" | "random_access" => Ok(SuiteSelection::RandomAccess),
        other => Err(format!(
            "unknown --suite '{other}' (expected all|clickbench|row-filter|random-access)"
        )
        .into()),
    }
}

fn parse_positive_threads(flag: &str, val: &str) -> BenchResult<usize> {
    let parsed = val
        .parse::<usize>()
        .map_err(|_| format!("invalid {flag} '{val}' (expected integer >= 1)"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be >= 1").into());
    }
    Ok(parsed)
}

fn parse_cli_args() -> BenchResult<CliConfig> {
    let mut suite = SuiteSelection::All;
    let mut statam_write_threads = 1usize;
    let mut statam_read_threads = 1usize;
    let mut hits_parquet = default_hits_path();
    let mut formats_override: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(val) = arg.strip_prefix("--suite=") {
            suite = parse_suite(val)?;
        } else if arg == "--suite" {
            let val = args.next().ok_or("--suite requires a value")?;
            suite = parse_suite(&val)?;
        } else if let Some(val) = arg.strip_prefix("--statam-write-threads=") {
            statam_write_threads = parse_positive_threads("--statam-write-threads", val)?;
        } else if arg == "--statam-write-threads" {
            let val = args
                .next()
                .ok_or("--statam-write-threads requires an integer")?;
            statam_write_threads = parse_positive_threads("--statam-write-threads", &val)?;
        } else if let Some(val) = arg.strip_prefix("--statam-read-threads=") {
            statam_read_threads = parse_positive_threads("--statam-read-threads", val)?;
        } else if arg == "--statam-read-threads" {
            let val = args
                .next()
                .ok_or("--statam-read-threads requires an integer")?;
            statam_read_threads = parse_positive_threads("--statam-read-threads", &val)?;
        } else if let Some(val) = arg.strip_prefix("--hits-parquet=") {
            hits_parquet = PathBuf::from(val);
        } else if arg == "--hits-parquet" {
            let val = args.next().ok_or("--hits-parquet requires a path")?;
            hits_parquet = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--formats=") {
            formats_override = Some(val.to_owned());
        } else if arg == "--formats" {
            let val = args.next().ok_or("--formats requires a csv list")?;
            formats_override = Some(val);
        } else {
            return Err(format!(
                "unknown argument '{arg}' (supported: --suite, --statam-write-threads, --statam-read-threads, --hits-parquet, --formats)"
            )
            .into());
        }
    }

    let formats = if let Some(raw) = formats_override {
        parse_formats_csv(&raw)?
    } else if let Ok(raw) = std::env::var("BENCH_FORMATS")
        && !raw.trim().is_empty()
    {
        parse_formats_csv(&raw)?
    } else {
        DEFAULT_EXTERNAL_FORMATS.to_vec()
    };

    Ok(CliConfig {
        suite,
        statam_write_threads,
        statam_read_threads,
        hits_parquet,
        formats,
    })
}

fn machine_summary() -> String {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let profile = if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    };
    format!("{os}/{arch}, {cpus} logical CPUs, profile={profile}")
}

fn mk_write_row(
    workload: &str,
    fmt: &str,
    raw_bytes: u64,
    stored_bytes: u64,
    ms: f64,
) -> WriteBenchRow {
    let mib = raw_bytes as f64 / (1024.0 * 1024.0);
    WriteBenchRow {
        workload: workload.to_owned(),
        format: fmt.to_owned(),
        raw_bytes,
        stored_bytes,
        write_ms: ms,
        write_mib_s: if ms > 0.0 { mib / (ms / 1000.0) } else { 0.0 },
    }
}

fn write_generic_parquet(
    variant: ParquetVariant,
    batch: &RecordBatch,
    bloom_cols: &[&str],
    sort_cols: &[&str],
    tuned_rg_rows: usize,
) -> BenchResult<(Bytes, f64)> {
    let schema = batch.schema();
    let t0 = Instant::now();
    let props = generic_parquet_writer_props(
        variant,
        &schema,
        batch.num_rows(),
        bloom_cols,
        sort_cols,
        tuned_rg_rows,
    )?;
    let mut buf = Vec::new();
    {
        let mut w = ArrowWriter::try_new(&mut buf, Arc::clone(&schema), Some(props))?;
        for off in (0..batch.num_rows()).step_by(8_192) {
            let len = 8_192.min(batch.num_rows() - off);
            w.write(&batch.slice(off, len))?;
            if variant == ParquetVariant::Cetra
                && (w.in_progress_size() >= 64 << 20 || w.in_progress_rows() >= 100_000)
            {
                w.flush()?;
            }
        }
        w.close()?;
    }
    Ok((Bytes::from(buf), t0.elapsed().as_secs_f64() * 1000.0))
}

fn write_generic_strata(
    batch: &RecordBatch,
    sort_key: &[&str],
    key_filter_cols: &[&str],
) -> BenchResult<(Bytes, f64)> {
    let mut cfg = StrataConfig::default();
    for &col in key_filter_cols {
        cfg.key_filter_columns.push(col.to_owned());
    }
    let t0 = Instant::now();
    let mut w = StrataWriter::new(batch.schema(), sort_key, cfg)?;
    w.write(batch)?;
    let (buf, _summary) = w.finish()?;
    Ok((Bytes::from(buf), t0.elapsed().as_secs_f64() * 1000.0))
}

fn write_generic_statam(
    batch: &Batch,
    profile: WriteProfile,
    sort_cols: &[u16],
    bloom_cols: &[&str],
    hot_bloom_col: Option<&str>,
    narrow_trigram_cols: &[&str],
    write_threads: usize,
) -> BenchResult<(Bytes, f64)> {
    let t0 = Instant::now();
    let mut opts = WriteOptions::for_table(TableKind::Generic, profile);
    opts.sort_cols = sort_cols.to_vec();
    opts.bloom_cols = bloom_cols.iter().map(|&s| s.to_owned()).collect();
    opts.hot_bloom_col = hot_bloom_col.map(str::to_owned);
    opts.narrow_trigram_cols = if profile == WriteProfile::L1Compact {
        narrow_trigram_cols.iter().map(|&s| s.to_owned()).collect()
    } else {
        Vec::new()
    };
    if write_threads > 1 {
        opts.write_threads = write_threads;
    }
    let mut buf = Vec::new();
    let mut w = StatamWriter::new(Arc::clone(batch.schema()), opts, None)?;
    w.write_batch(batch)?;
    w.finish(&mut buf)?;
    Ok((Bytes::from(buf), t0.elapsed().as_secs_f64() * 1000.0))
}

async fn open_single_strata_local(bytes: &Bytes) -> BenchResult<Arc<StrataFile>> {
    let path = ObjectPath::from("part_000.strata");
    let mut map = HashMap::with_capacity(1);
    map.insert(path.clone(), Arc::new(SliceReader::new(bytes.clone())));
    let store: Arc<dyn ObjectStore> = Arc::new(SliceObjectStore::new(map));
    let cfg = StrataConfig::default();
    let f = StrataFile::open(store, path, None, &cfg).await?;
    Ok(Arc::new(f))
}

async fn open_single_strata_s3(
    bytes: &Bytes,
    s3_cfg: SimulatedS3Config,
) -> BenchResult<(Arc<StrataFile>, Arc<SimulatedS3Reader>)> {
    let path = ObjectPath::from("part_000.strata");
    let s3 = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
    s3.evict_all();
    s3.reset_counters();
    let mut map = HashMap::with_capacity(1);
    map.insert(path.clone(), Arc::clone(&s3));
    let store: Arc<dyn ObjectStore> = Arc::new(SimS3ObjectStore::new(map));
    let mut cfg = StrataConfig::default();
    cfg.point_gap = 256 * 1024;
    let f = StrataFile::open(store, path, None, &cfg).await?;
    Ok((Arc::new(f), s3))
}

#[derive(Clone)]
enum GenPqPred {
    I64Eq { col: String, val: i64 },
    I64Ne { col: String, val: i64 },
    I64Between { col: String, lo: i64, hi: i64 },
    Utf8Eq { col: String, val: String },
    Utf8NonEmpty { col: String },
    Utf8Like { col: String, needle: String },
}

impl GenPqPred {
    fn col_name(&self) -> &str {
        match self {
            Self::I64Eq { col, .. }
            | Self::I64Ne { col, .. }
            | Self::I64Between { col, .. }
            | Self::Utf8Eq { col, .. }
            | Self::Utf8NonEmpty { col }
            | Self::Utf8Like { col, .. } => col.as_str(),
        }
    }
}

fn gen_rg_matches_filters(
    rg: &RowGroupMetaData,
    descr: &SchemaDescriptor,
    filters: &[GenPqPred],
) -> bool {
    for f in filters {
        let Some(leaf_idx) = find_leaf_idx(descr, f.col_name()) else {
            continue;
        };
        let Some(stats) = rg.column(leaf_idx).statistics() else {
            continue;
        };
        let num_rows = u64::try_from(rg.num_rows()).unwrap_or(0);
        let null_count = stats.null_count_opt().unwrap_or(0);
        if stats.null_count_opt() == Some(num_rows) {
            return false;
        }
        match (f, stats) {
            (GenPqPred::I64Eq { val, .. }, Statistics::Int64(s)) => {
                if let (Some(&min), Some(&max)) = (s.min_opt(), s.max_opt())
                    && (*val < min || *val > max)
                {
                    return false;
                }
            }
            (GenPqPred::I64Ne { val, .. }, Statistics::Int64(s)) => {
                if null_count == 0
                    && let (Some(&min), Some(&max)) = (s.min_opt(), s.max_opt())
                    && min == *val
                    && max == *val
                {
                    return false;
                }
            }
            (GenPqPred::I64Between { lo, hi, .. }, Statistics::Int64(s)) => {
                if let (Some(&min), Some(&max)) = (s.min_opt(), s.max_opt())
                    && (max < *lo || min > *hi)
                {
                    return false;
                }
            }
            (GenPqPred::Utf8Eq { val, .. }, Statistics::ByteArray(s)) => {
                let target = val.as_bytes();
                if let (Some(min), Some(max)) = (s.min_bytes_opt(), s.max_bytes_opt())
                    && (target < min || target > max)
                {
                    return false;
                }
            }
            (GenPqPred::Utf8NonEmpty { .. }, Statistics::ByteArray(s)) => {
                if null_count == 0
                    && let (Some(min), Some(max)) = (s.min_bytes_opt(), s.max_bytes_opt())
                    && min.is_empty()
                    && max.is_empty()
                {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

fn gen_page_matches_filter(
    col_idx: &ColumnIndexMetaData,
    page_idx: usize,
    filter: &GenPqPred,
) -> bool {
    if col_idx.is_null_page(page_idx) {
        return false;
    }
    match (filter, col_idx) {
        (GenPqPred::I64Eq { val, .. }, ColumnIndexMetaData::INT64(idx)) => {
            if let (Some(&min), Some(&max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return *val >= min && *val <= max;
            }
        }
        (GenPqPred::I64Ne { val, .. }, ColumnIndexMetaData::INT64(idx)) => {
            if idx.null_count(page_idx).unwrap_or(0) == 0
                && let (Some(&min), Some(&max)) = (idx.min_value(page_idx), idx.max_value(page_idx))
                && min == *val
                && max == *val
            {
                return false;
            }
        }
        (GenPqPred::I64Between { lo, hi, .. }, ColumnIndexMetaData::INT64(idx)) => {
            if let (Some(&min), Some(&max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return max >= *lo && min <= *hi;
            }
        }
        (GenPqPred::Utf8Eq { val, .. }, ColumnIndexMetaData::BYTE_ARRAY(idx)) => {
            let target = val.as_bytes();
            if let (Some(min), Some(max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return target >= min && target <= max;
            }
        }
        (GenPqPred::Utf8NonEmpty { .. }, ColumnIndexMetaData::BYTE_ARRAY(idx)) => {
            if idx.null_count(page_idx).unwrap_or(0) == 0
                && let (Some(min), Some(max)) = (idx.min_value(page_idx), idx.max_value(page_idx))
                && min.is_empty()
                && max.is_empty()
            {
                return false;
            }
        }
        _ => {}
    }
    true
}

fn intersect_ranges(a: &[Range<usize>], b: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let lo = a[i].start.max(b[j].start);
        let hi = a[i].end.min(b[j].end);
        if lo < hi {
            if let Some(last) = out.last_mut()
                && last.end == lo
            {
                last.end = hi;
            } else {
                out.push(lo..hi);
            }
        }
        if a[i].end < b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn prune_gen_rg_pages(
    meta: &ParquetMetaData,
    rg_idx: usize,
    filters: &[GenPqPred],
) -> Vec<Range<usize>> {
    let rg = meta.row_group(rg_idx);
    let num_rows = usize::try_from(rg.num_rows()).unwrap_or(0);
    if num_rows == 0 {
        return Vec::new();
    }
    let mut kept = vec![0..num_rows];
    let (Some(col_indexes), Some(off_indexes)) = (meta.column_index(), meta.offset_index()) else {
        return kept;
    };
    let (Some(rg_ci), Some(rg_oi)) = (col_indexes.get(rg_idx), off_indexes.get(rg_idx)) else {
        return kept;
    };
    let descr = rg.schema_descr();

    for f in filters {
        let Some(leaf_idx) = find_leaf_idx(descr, f.col_name()) else {
            continue;
        };
        let (Some(ci), Some(oi)) = (rg_ci.get(leaf_idx), rg_oi.get(leaf_idx)) else {
            continue;
        };
        let locs = oi.page_locations();
        if locs.is_empty() || ci.num_pages() as usize != locs.len() {
            continue;
        }
        let mut col_kept: Vec<Range<usize>> = Vec::new();
        for (p_idx, loc) in locs.iter().enumerate() {
            let start = usize::try_from(loc.first_row_index).unwrap_or(0);
            let end = locs.get(p_idx + 1).map_or(num_rows, |nxt| {
                usize::try_from(nxt.first_row_index).unwrap_or(num_rows)
            });
            if start < end && gen_page_matches_filter(ci, p_idx, f) {
                if let Some(last) = col_kept.last_mut()
                    && last.end == start
                {
                    last.end = end;
                } else {
                    col_kept.push(start..end);
                }
            }
        }
        kept = intersect_ranges(&kept, &col_kept);
        if kept.is_empty() {
            return Vec::new();
        }
    }
    kept
}

fn build_gen_row_preds(
    parquet_schema: &SchemaDescriptor,
    filters: &[GenPqPred],
) -> Vec<Box<dyn ArrowPredicate>> {
    let mut row_preds: Vec<Box<dyn ArrowPredicate>> = Vec::new();
    for f in filters {
        let Some(root_idx) = parquet_schema
            .root_schema()
            .get_fields()
            .iter()
            .position(|fld| fld.name() == f.col_name())
        else {
            continue;
        };
        let mask = ProjectionMask::roots(parquet_schema, [root_idx]);
        match f {
            GenPqPred::I64Eq { val, .. } => {
                let target = *val;
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Int64Array".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) == target))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::I64Ne { val, .. } => {
                let target = *val;
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Int64Array".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) != target))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::I64Between { lo, hi, .. } => {
                let (lo_v, hi_v) = (*lo, *hi);
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Int64Array".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| {
                            (!arr.is_null(i)).then(|| {
                                let v = arr.value(i);
                                v >= lo_v && v <= hi_v
                            })
                        })
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::Utf8Eq { val, .. } => {
                let target = val.clone();
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected StringArray".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) == target.as_str()))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::Utf8NonEmpty { .. } => {
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected StringArray".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| !arr.value(i).is_empty()))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::Utf8Like { needle, .. } => {
                let nd = needle.clone();
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected StringArray".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i).contains(nd.as_str())))
                        .collect();
                    Ok(bools)
                })));
            }
        }
    }
    row_preds
}

fn execute_gen_parquet_scan_with_builder<R: ChunkReader + 'static>(
    chunk_reader: &R,
    mut builder: ParquetRecordBatchReaderBuilder<R>,
    proj_roots: &[usize],
    filters: &[GenPqPred],
) -> BenchResult<usize> {
    let meta = Arc::clone(builder.metadata());
    let parquet_schema = meta.file_metadata().schema_descr_ptr();
    let descr = meta.file_metadata().schema_descr();

    let mut surviving = Vec::new();
    for rg_idx in 0..meta.num_row_groups() {
        let rg = meta.row_group(rg_idx);
        if !gen_rg_matches_filters(rg, descr, filters) {
            continue;
        }
        let mut bloom_ok = true;
        for f in filters {
            if let Some(leaf_idx) = find_leaf_idx(descr, f.col_name()) {
                let chunk = rg.column(leaf_idx);
                match f {
                    GenPqPred::I64Eq { val, .. } => {
                        if let Some(sbbf) = Sbbf::read_from_column_chunk(chunk, chunk_reader)?
                            && !sbbf.check(val)
                        {
                            bloom_ok = false;
                            break;
                        }
                    }
                    GenPqPred::Utf8Eq { val, .. } => {
                        if let Some(sbbf) = Sbbf::read_from_column_chunk(chunk, chunk_reader)?
                            && !sbbf.check(val.as_bytes())
                        {
                            bloom_ok = false;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !bloom_ok {
            continue;
        }
        let kept = prune_gen_rg_pages(&meta, rg_idx, filters);
        if !kept.is_empty() {
            surviving.push((rg_idx, kept));
        }
    }

    if surviving.is_empty() {
        return Ok(0);
    }

    let rgs: Vec<usize> = surviving.iter().map(|(rg, _)| *rg).collect();
    let mut all_full = true;
    let mut selectors = Vec::new();
    for (rg_idx, kept) in &surviving {
        let num_rows = usize::try_from(meta.row_group(*rg_idx).num_rows()).unwrap_or(0);
        if kept.len() != 1 || kept[0] != (0..num_rows) {
            all_full = false;
        }
        let mut cursor = 0usize;
        for r in kept {
            if r.start > cursor {
                selectors.push(RowSelector::skip(r.start - cursor));
            }
            selectors.push(RowSelector::select(r.end - r.start));
            cursor = r.end;
        }
        if cursor < num_rows {
            selectors.push(RowSelector::skip(num_rows - cursor));
        }
    }

    builder = builder
        .with_row_groups(rgs)
        .with_projection(ProjectionMask::roots(
            &parquet_schema,
            proj_roots.iter().copied(),
        ))
        .with_batch_size(16_384);

    if !all_full {
        builder = builder.with_row_selection(RowSelection::from(selectors));
    }
    let row_preds = build_gen_row_preds(&parquet_schema, filters);
    if !row_preds.is_empty() {
        builder = builder.with_row_filter(RowFilter::new(row_preds));
    }

    let reader = builder.build()?;
    let mut rows = 0usize;
    for b in reader {
        rows += b?.num_rows();
    }
    Ok(rows)
}

fn execute_gen_parquet_local(
    bytes: &Bytes,
    meta: &ArrowReaderMetadata,
    proj_roots: &[usize],
    filters: &[GenPqPred],
) -> BenchResult<usize> {
    let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(bytes.clone(), meta.clone());
    execute_gen_parquet_scan_with_builder(bytes, builder, proj_roots, filters)
}

fn execute_gen_parquet_s3(
    reader: &SimS3ChunkReader,
    with_pi: bool,
    proj_roots: &[usize],
    filters: &[GenPqPred],
) -> BenchResult<usize> {
    let builder = open_parquet_builder(reader, with_pi)?;
    execute_gen_parquet_scan_with_builder(reader, builder, proj_roots, filters)
}

async fn execute_vortex_scan(
    session: &VortexSession,
    file: &VortexFile,
    proj_expr: &Expression,
    filter_opt: Option<&Expression>,
    like_check: Option<(&str, &str)>,
) -> BenchResult<usize> {
    let projection = proj_expr.clone().bind(file.dtype())?.optimize_recursive()?;
    let mut builder = file.scan()?.with_projection(projection);
    if let Some(expr) = filter_opt {
        let filter = expr.clone().bind(file.dtype())?.optimize_recursive()?;
        builder = builder.with_filter(filter);
    }
    let mut stream = builder.into_array_stream()?;
    let mut total = 0usize;
    while let Some(chunk) = stream.try_next().await? {
        if chunk.is_empty() {
            continue;
        }
        if let Some((col_name, needle)) = like_check {
            let batch = chunk_to_record_batch(session, chunk)?;
            if let Some(arr) = batch.column_by_name(col_name) {
                for i in 0..batch.num_rows() {
                    if let Some(s) = str_value_at(arr.as_ref(), i)
                        && s.contains(needle)
                    {
                        total += 1;
                    }
                }
            }
        } else {
            let batch = chunk_to_record_batch(session, chunk)?;
            total += batch.num_rows();
        }
    }
    Ok(total)
}

fn execute_statam_scan(reader: &StatamReader, req: &ScanRequest) -> BenchResult<usize> {
    let batches = reader.scan(req)?;
    Ok(batches.iter().map(Batch::num_rows).sum())
}

// ============================================================================
// Suite A: ClickBench (`hits_1.parquet`, 100k rows x 105 cols)
// ============================================================================

struct ClickBenchParams {
    first_nonzero_uid: i64,
    most_freq_cid: i64,
    et_lo: i64,
    et_hi: i64,
    like_col: &'static str,
    like_needle: &'static str,
    adv_idx: usize,
    mpm_idx: usize,
    sp_idx: usize,
    uid_idx: usize,
    et_idx: usize,
    url_idx: usize,
    title_idx: usize,
    cid_idx: usize,
    all_cols: Vec<usize>,
}

fn load_clickbench_100k(path: &Path) -> BenchResult<RecordBatch> {
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(16_384)
        .with_limit(100_000)
        .build()?;
    let mut norm_batches = Vec::new();
    let mut total = 0usize;
    for batch_res in reader {
        let b = batch_res?;
        if b.num_rows() == 0 {
            continue;
        }
        let take = (100_000 - total).min(b.num_rows());
        let sliced = if take < b.num_rows() {
            b.slice(0, take)
        } else {
            b
        };
        norm_batches.push(normalize_arrow_batch(&sliced)?);
        total += take;
        if total >= 100_000 {
            break;
        }
    }
    if norm_batches.is_empty() {
        return Err(format!("hits parquet '{}' contained 0 rows", path.display()).into());
    }
    let norm_schema = norm_batches[0].schema();
    Ok(concat_batches(&norm_schema, &norm_batches)?)
}

fn col_idx(schema: &Schema, name: &str) -> BenchResult<usize> {
    schema
        .index_of(name)
        .map_err(|e| format!("ClickBench schema missing column '{name}': {e}").into())
}

fn discover_clickbench_params(batch: &RecordBatch) -> BenchResult<ClickBenchParams> {
    let schema = batch.schema();
    let adv_idx = col_idx(&schema, "AdvEngineID")?;
    let mpm_idx = col_idx(&schema, "MobilePhoneModel")?;
    let sp_idx = col_idx(&schema, "SearchPhrase")?;
    let uid_idx = col_idx(&schema, "UserID")?;
    let et_idx = col_idx(&schema, "EventTime")?;
    let url_idx = col_idx(&schema, "URL")?;
    let title_idx = col_idx(&schema, "Title")?;
    let cid_idx = col_idx(&schema, "CounterID")?;

    let uid_arr = batch
        .column(uid_idx)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or("expected Int64Array for UserID")?;
    let mut first_nonzero_uid = 1i64;
    for i in 0..uid_arr.len() {
        if !uid_arr.is_null(i) {
            let v = uid_arr.value(i);
            if v != 0 {
                first_nonzero_uid = v;
                break;
            }
        }
    }

    let cid_arr = batch
        .column(cid_idx)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or("expected Int64Array for CounterID")?;
    let mut cid_counts: HashMap<i64, usize> = HashMap::new();
    for i in 0..cid_arr.len() {
        if !cid_arr.is_null(i) {
            *cid_counts.entry(cid_arr.value(i)).or_insert(0) += 1;
        }
    }
    let most_freq_cid = cid_counts
        .into_iter()
        .max_by_key(|&(_, c)| c)
        .map_or(0, |(k, _)| k);

    let et_arr = batch
        .column(et_idx)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or("expected Int64Array for EventTime")?;
    let mut et_vals: Vec<i64> = (0..et_arr.len())
        .filter_map(|i| (!et_arr.is_null(i)).then(|| et_arr.value(i)))
        .collect();
    et_vals.sort_unstable();
    let (et_lo, et_hi) = if et_vals.is_empty() {
        (0, 0)
    } else {
        (
            et_vals[et_vals.len() * 3 / 8],
            et_vals[et_vals.len() * 5 / 8],
        )
    };

    let title_arr = batch
        .column(title_idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or("expected StringArray for Title")?;
    let url_arr = batch
        .column(url_idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or("expected StringArray for URL")?;
    let count_substr = |arr: &StringArray, nd: &str| -> usize {
        (0..arr.len())
            .filter(|&i| !arr.is_null(i) && arr.value(i).contains(nd))
            .count()
    };
    let (like_col, like_needle) = if count_substr(title_arr, "google") > 0 {
        ("Title", "google")
    } else if count_substr(title_arr, "Google") > 0 {
        ("Title", "Google")
    } else if count_substr(url_arr, "http") > 0 {
        ("URL", "http")
    } else {
        ("Title", "google")
    };

    Ok(ClickBenchParams {
        first_nonzero_uid,
        most_freq_cid,
        et_lo,
        et_hi,
        like_col,
        like_needle,
        adv_idx,
        mpm_idx,
        sp_idx,
        uid_idx,
        et_idx,
        url_idx,
        title_idx,
        cid_idx,
        all_cols: (0..schema.fields().len()).collect(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_clickbench_suite(
    hits_path: &Path,
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    if !hits_path.exists() {
        notes.push(format!(
            "Suite A (`clickbench`): SKIPPED (`{}` not found; pass `--hits-parquet` or set `STATAM_HITS_PARQUET`)",
            hits_path.display()
        ));
        return Ok(());
    }

    let norm_batch = load_clickbench_100k(hits_path)?;
    let cetra_batch = arrow_to_cetra_batch(&norm_batch)?;
    let params = discover_clickbench_params(&norm_batch)?;
    let raw_bytes = norm_batch.get_array_memory_size() as u64;
    let wl = "clickbench_100k";

    notes.push(format!(
        "Suite A (`clickbench`): {} rows x {} cols, `UserID={}`, `CounterID={}`, `EventTime=[{}, {}]`, `CB_Q23_like={}.contains(\"{}\")`",
        norm_batch.num_rows(),
        norm_batch.num_columns(),
        params.first_nonzero_uid,
        params.most_freq_cid,
        params.et_lo,
        params.et_hi,
        params.like_col,
        params.like_needle
    ));

    let bloom_names = ["UserID", "CounterID"];
    let pq_default = if enabled.contains(&"pq_default") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Default,
            &norm_batch,
            &bloom_names,
            &[],
            1_000_000,
        )?;
        write_rows.push(mk_write_row(
            wl,
            "pq_default",
            raw_bytes,
            b.len() as u64,
            ms,
        ));
        Some(b)
    } else {
        None
    };
    let pq_tuned = if enabled.contains(&"pq_tuned") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Tuned,
            &norm_batch,
            &bloom_names,
            &[],
            32_768,
        )?;
        write_rows.push(mk_write_row(wl, "pq_tuned", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let pq_cetra = if enabled.contains(&"pq_cetra") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Cetra,
            &norm_batch,
            &bloom_names,
            &[],
            100_000,
        )?;
        write_rows.push(mk_write_row(wl, "pq_cetra", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let vortex_bytes = if enabled.contains(&"vortex") {
        let (files, ms) = write_vortex_files(vortex_session, &[norm_batch.clone()]).await?;
        let b = files.into_iter().next().ok_or("missing vortex output")?;
        write_rows.push(mk_write_row(wl, "vortex", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let strata_bytes = if enabled.contains(&"strata") {
        match write_generic_strata(&norm_batch, &[], &bloom_names) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(wl, "strata", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite A (`clickbench`): `strata` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let statam_l0 = if enabled.contains(&"statam_l0") {
        let (b, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L0Fast,
            &[],
            &bloom_names,
            Some("UserID"),
            &[],
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let statam_l1 = if enabled.contains(&"statam_l1") {
        let (b, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L1Compact,
            &[],
            &bloom_names,
            Some("UserID"),
            &["Title", "URL", "SearchPhrase"],
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l1", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };

    let queries = [
        "CB_Q1",
        "CB_Q10",
        "CB_Q12",
        "CB_Q20",
        "CB_Q23_like",
        "CB_Q24",
        "CB_Q39_range",
        "CB_wide_point",
    ];
    let s3_cfg = SimulatedS3Config::default();

    let strata_file_local = match &strata_bytes {
        Some(b) => Some(open_single_strata_local(b).await?),
        None => None,
    };
    let vortex_file_local = match &vortex_bytes {
        Some(b) => Some(open_vortex_local(vortex_session, &[b.clone()]).await?),
        None => None,
    };
    let statam_l0_local = statam_l0
        .as_ref()
        .map(|b| StatamReader::open(Arc::new(SliceReader::new(b.clone())), None))
        .transpose()?;
    let statam_l1_local = statam_l1
        .as_ref()
        .map(|b| StatamReader::open(Arc::new(SliceReader::new(b.clone())), None))
        .transpose()?;

    for &qname in &queries {
        let mut expected_matches: Option<usize> = None;
        let (pq_proj, pq_preds) = clickbench_pq_spec(qname, &params);

        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &pq_default, false),
            ("pq_tuned", &pq_tuned, true),
            ("pq_cetra", &pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                PageIndexPolicy::Optional
            } else {
                PageIndexPolicy::Skip
            });
            let meta = ArrowReaderMetadata::load(bytes, opts)?;
            let (matched, local_ms, local_p95_ms) = measure_local_ms(
                || execute_gen_parquet_local(bytes, &meta, &pq_proj, &pq_preds),
                warm_runs,
            )?;

            let s3_reader = SimS3ChunkReader::new(Arc::new(SimulatedS3Reader::from_bytes(
                bytes.clone(),
                s3_cfg,
            )));
            s3_reader.s3().evict_all();
            s3_reader.s3().reset_counters();
            let s3_matched = execute_gen_parquet_s3(&s3_reader, with_pi, &pq_proj, &pq_preds)?;
            if s3_matched != matched {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.s3().simulated_latency_ms(),
                s3_gets: s3_reader.s3().counters().gets,
                s3_bytes: s3_reader.s3().counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let (proj, filt, like) = clickbench_vortex_spec(qname, &params);
            let (matched, local_ms, local_p95_ms) = measure_local_async_ms(
                || execute_vortex_scan(vortex_session, &local_files[0], &proj, filt.as_ref(), like),
                warm_runs,
            )
            .await?;

            let s3_reader = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
            s3_reader.evict_all();
            s3_reader.reset_counters();
            let vx_read = Arc::new(SimS3VortexRead::new(
                Arc::clone(&s3_reader) as Arc<dyn RangeReader>
            ));
            let s3_file = vortex_session.open_options().open_read(vx_read).await?;
            let s3_matched =
                execute_vortex_scan(vortex_session, &s3_file, &proj, filt.as_ref(), like).await?;
            if s3_matched != matched {
                return Err(format!("vortex {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, "vortex", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.simulated_latency_ms(),
                s3_gets: s3_reader.counters().gets,
                s3_bytes: s3_reader.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_file)) = (&strata_bytes, &strata_file_local) {
            let use_contains = if qname == "CB_Q23_like" {
                let probe = StrataScanRequest {
                    projection: vec![params.title_idx, params.url_idx],
                    predicate: Some(StrataPred::Contains(
                        if params.like_col == "Title" {
                            params.title_idx
                        } else {
                            params.url_idx
                        },
                        params.like_needle.as_bytes().to_vec(),
                    )),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Sparse,
                    views: false,
                    top: None,
                    row_filter: None,
                };
                local_file.scan(&probe).await.is_ok()
            } else {
                false
            };

            let (matched, local_ms, local_p95_ms) = measure_local_async_ms(
                || execute_clickbench_strata(local_file, qname, &params, use_contains),
                warm_runs,
            )
            .await?;

            let (s3_file, s3_reader) = open_single_strata_s3(bytes, s3_cfg).await?;
            let s3_matched =
                execute_clickbench_strata(&s3_file, qname, &params, use_contains).await?;
            if s3_matched != matched {
                return Err(format!("strata {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, "strata", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "strata".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.simulated_latency_ms(),
                s3_gets: s3_reader.counters().gets,
                s3_bytes: s3_reader.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        for (fmt, bytes_opt, local_opt) in [
            ("statam_l0", &statam_l0, &statam_l0_local),
            ("statam_l1", &statam_l1, &statam_l1_local),
        ] {
            let (Some(bytes), Some(local_reader)) = (bytes_opt, local_opt) else {
                continue;
            };
            let mut req = clickbench_statam_spec(qname, &params);
            req.read_threads = read_threads;
            let (matched, local_ms, local_p95_ms) =
                measure_local_ms(|| execute_statam_scan(local_reader, &req), warm_runs)?;

            let s3_file = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
            s3_file.evict_all();
            s3_file.reset_counters();
            let s3_reader = StatamReader::open(Arc::clone(&s3_file) as Arc<dyn RangeReader>, None)?;
            let s3_matched = execute_statam_scan(&s3_reader, &req)?;
            if s3_matched != matched {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_file.simulated_latency_ms(),
                s3_gets: s3_file.counters().gets,
                s3_bytes: s3_file.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }
    }

    Ok(())
}

fn clickbench_pq_spec(qname: &str, p: &ClickBenchParams) -> (Vec<usize>, Vec<GenPqPred>) {
    match qname {
        "CB_Q1" => (
            vec![p.adv_idx],
            vec![GenPqPred::I64Ne {
                col: "AdvEngineID".to_owned(),
                val: 0,
            }],
        ),
        "CB_Q10" => (
            vec![p.mpm_idx, p.uid_idx],
            vec![GenPqPred::Utf8NonEmpty {
                col: "MobilePhoneModel".to_owned(),
            }],
        ),
        "CB_Q12" => (
            vec![p.sp_idx, p.uid_idx],
            vec![GenPqPred::Utf8NonEmpty {
                col: "SearchPhrase".to_owned(),
            }],
        ),
        "CB_Q20" => (
            vec![p.uid_idx, p.et_idx, p.url_idx, p.title_idx],
            vec![GenPqPred::I64Eq {
                col: "UserID".to_owned(),
                val: p.first_nonzero_uid,
            }],
        ),
        "CB_Q23_like" => (
            vec![p.title_idx, p.url_idx],
            vec![GenPqPred::Utf8Like {
                col: p.like_col.to_owned(),
                needle: p.like_needle.to_owned(),
            }],
        ),
        "CB_Q24" => (
            vec![p.url_idx],
            vec![GenPqPred::Utf8NonEmpty {
                col: "URL".to_owned(),
            }],
        ),
        "CB_Q39_range" => (
            vec![p.cid_idx, p.et_idx, p.uid_idx],
            vec![
                GenPqPred::I64Eq {
                    col: "CounterID".to_owned(),
                    val: p.most_freq_cid,
                },
                GenPqPred::I64Between {
                    col: "EventTime".to_owned(),
                    lo: p.et_lo,
                    hi: p.et_hi,
                },
            ],
        ),
        "CB_wide_point" => (
            p.all_cols.clone(),
            vec![GenPqPred::I64Eq {
                col: "UserID".to_owned(),
                val: p.first_nonzero_uid,
            }],
        ),
        _ => (vec![], vec![]),
    }
}

fn clickbench_vortex_spec(
    qname: &str,
    p: &ClickBenchParams,
) -> (
    Expression,
    Option<Expression>,
    Option<(&'static str, &'static str)>,
) {
    match qname {
        "CB_Q1" => (
            select(["AdvEngineID"], root()),
            Some(not(eq(col("AdvEngineID"), lit(0i64)))),
            None,
        ),
        "CB_Q10" => (
            select(["MobilePhoneModel", "UserID"], root()),
            Some(not(eq(col("MobilePhoneModel"), lit("")))),
            None,
        ),
        "CB_Q12" => (
            select(["SearchPhrase", "UserID"], root()),
            Some(not(eq(col("SearchPhrase"), lit("")))),
            None,
        ),
        "CB_Q20" => (
            select(["UserID", "EventTime", "URL", "Title"], root()),
            Some(eq(col("UserID"), lit(p.first_nonzero_uid))),
            None,
        ),
        "CB_Q23_like" => (
            select(["Title", "URL"], root()),
            None,
            Some((p.like_col, p.like_needle)),
        ),
        "CB_Q24" => (
            select(["URL"], root()),
            Some(not(eq(col("URL"), lit("")))),
            None,
        ),
        "CB_Q39_range" => (
            select(["CounterID", "EventTime", "UserID"], root()),
            Some(and(
                eq(col("CounterID"), lit(p.most_freq_cid)),
                and(
                    gt_eq(col("EventTime"), lit(p.et_lo)),
                    lt_eq(col("EventTime"), lit(p.et_hi)),
                ),
            )),
            None,
        ),
        "CB_wide_point" => (
            root(),
            Some(eq(col("UserID"), lit(p.first_nonzero_uid))),
            None,
        ),
        _ => (root(), None, None),
    }
}

async fn execute_clickbench_strata(
    file: &Arc<StrataFile>,
    qname: &str,
    p: &ClickBenchParams,
    use_contains: bool,
) -> BenchResult<usize> {
    let req = match qname {
        "CB_Q1" => StrataScanRequest {
            projection: vec![p.adv_idx],
            predicate: None,
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
            row_filter: None,
        },
        "CB_Q10" => StrataScanRequest {
            projection: vec![p.mpm_idx, p.uid_idx],
            predicate: None,
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
            row_filter: None,
        },
        "CB_Q12" => StrataScanRequest {
            projection: vec![p.sp_idx, p.uid_idx],
            predicate: None,
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
            row_filter: None,
        },
        "CB_Q20" => StrataScanRequest {
            projection: vec![p.uid_idx, p.et_idx, p.url_idx, p.title_idx],
            predicate: Some(StrataPred::Cmp(
                p.uid_idx,
                CmpOp::Eq,
                Lit::Int(p.first_nonzero_uid),
            )),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Point,
            views: false,
            top: None,
            row_filter: None,
        },
        "CB_Q23_like" => StrataScanRequest {
            projection: vec![p.title_idx, p.url_idx],
            predicate: use_contains.then(|| {
                StrataPred::Contains(
                    if p.like_col == "Title" {
                        p.title_idx
                    } else {
                        p.url_idx
                    },
                    p.like_needle.as_bytes().to_vec(),
                )
            }),
            row_ranges: None,
            limit: None,
            kind: if use_contains {
                ScanKind::Sparse
            } else {
                ScanKind::Scan
            },
            views: false,
            top: None,
            row_filter: None,
        },
        "CB_Q24" => StrataScanRequest {
            projection: vec![p.url_idx],
            predicate: None,
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
            row_filter: None,
        },
        "CB_Q39_range" => StrataScanRequest {
            projection: vec![p.cid_idx, p.et_idx, p.uid_idx],
            predicate: Some(StrataPred::And(vec![
                StrataPred::Cmp(p.cid_idx, CmpOp::Eq, Lit::Int(p.most_freq_cid)),
                StrataPred::Cmp(p.et_idx, CmpOp::GtEq, Lit::Int(p.et_lo)),
                StrataPred::Cmp(p.et_idx, CmpOp::LtEq, Lit::Int(p.et_hi)),
            ])),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
            row_filter: None,
        },
        "CB_wide_point" => StrataScanRequest {
            projection: p.all_cols.clone(),
            predicate: Some(StrataPred::Cmp(
                p.uid_idx,
                CmpOp::Eq,
                Lit::Int(p.first_nonzero_uid),
            )),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Point,
            views: false,
            top: None,
            row_filter: None,
        },
        _ => return Ok(0),
    };

    let batches = file.scan(&req).await?;
    let mut matched = 0usize;
    for b in &batches {
        match qname {
            "CB_Q1" => {
                if let Some(arr) = b
                    .column_by_name("AdvEngineID")
                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) && arr.value(i) != 0 {
                            matched += 1;
                        }
                    }
                }
            }
            "CB_Q10" | "CB_Q12" | "CB_Q24" => {
                let col_name = match qname {
                    "CB_Q10" => "MobilePhoneModel",
                    "CB_Q12" => "SearchPhrase",
                    _ => "URL",
                };
                if let Some(arr) = b
                    .column_by_name(col_name)
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) && !arr.value(i).is_empty() {
                            matched += 1;
                        }
                    }
                }
            }
            "CB_Q23_like" if !use_contains => {
                if let Some(arr) = b
                    .column_by_name(p.like_col)
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>())
                {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) && arr.value(i).contains(p.like_needle) {
                            matched += 1;
                        }
                    }
                }
            }
            _ => matched += b.num_rows(),
        }
    }
    Ok(matched)
}

fn clickbench_statam_spec(qname: &str, p: &ClickBenchParams) -> ScanRequest {
    match qname {
        "CB_Q1" => ScanRequest {
            projection: Some(vec![p.adv_idx]),
            predicates: vec![StatamPred::new(
                "AdvEngineID",
                ZonePred::Ne(ZoneValue::I64(0)),
            )],
            ..ScanRequest::default()
        },
        "CB_Q10" => ScanRequest {
            projection: Some(vec![p.mpm_idx, p.uid_idx]),
            predicates: vec![StatamPred::new(
                "MobilePhoneModel",
                ZonePred::Ne(ZoneValue::Bytes(Vec::new())),
            )],
            ..ScanRequest::default()
        },
        "CB_Q12" => ScanRequest {
            projection: Some(vec![p.sp_idx, p.uid_idx]),
            predicates: vec![StatamPred::new(
                "SearchPhrase",
                ZonePred::Ne(ZoneValue::Bytes(Vec::new())),
            )],
            ..ScanRequest::default()
        },
        "CB_Q20" => ScanRequest {
            projection: Some(vec![p.uid_idx, p.et_idx, p.url_idx, p.title_idx]),
            id_lookups: vec![IdLookup::eq("UserID", p.first_nonzero_uid.to_le_bytes())],
            coalesce_mode: CoalesceMode::Point,
            ..ScanRequest::default()
        },
        "CB_Q23_like" => ScanRequest {
            projection: Some(vec![p.title_idx, p.url_idx]),
            substrings: vec![SubstringQuery::like(p.like_col, p.like_needle)],
            ..ScanRequest::default()
        },
        "CB_Q24" => ScanRequest {
            projection: Some(vec![p.url_idx]),
            predicates: vec![StatamPred::new(
                "URL",
                ZonePred::Ne(ZoneValue::Bytes(Vec::new())),
            )],
            ..ScanRequest::default()
        },
        "CB_Q39_range" => ScanRequest {
            projection: Some(vec![p.cid_idx, p.et_idx, p.uid_idx]),
            predicates: vec![
                StatamPred::new("CounterID", ZonePred::Eq(ZoneValue::I64(p.most_freq_cid))),
                StatamPred::new(
                    "EventTime",
                    ZonePred::Between(ZoneValue::I64(p.et_lo), ZoneValue::I64(p.et_hi)),
                ),
            ],
            ..ScanRequest::default()
        },
        "CB_wide_point" => ScanRequest {
            projection: None,
            id_lookups: vec![IdLookup::eq("UserID", p.first_nonzero_uid.to_le_bytes())],
            coalesce_mode: CoalesceMode::Point,
            ..ScanRequest::default()
        },
        _ => ScanRequest::default(),
    }
}

// ============================================================================
// Suite B: Row-Filter (Parquet's 6-Pattern Selectivity x Clustering Matrix)
// ============================================================================

fn rf_random_string(rng: &mut SmallRng) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let len = if rng.random_bool(0.5) {
        rng.random_range(13..21)
    } else {
        rng.random_range(3..12)
    };
    (0..len)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

fn build_row_filter_batch(size: usize) -> BenchResult<RecordBatch> {
    let mut rng_i = SmallRng::seed_from_u64(42);
    let mut int_vals: Vec<i64> = (0..size).map(|_| rng_i.random_range(0..100)).collect();
    if size > 0 {
        let uidx = rng_i.random_range(0..size);
        int_vals[uidx] = 9999;
    }

    let mut rng_f = SmallRng::seed_from_u64(43);
    let float_vals: Vec<f64> = (0..size).map(|_| rng_f.random_range(0.0..100.0)).collect();

    let mut rng_s = SmallRng::seed_from_u64(44);
    let mut utf8_b = StringBuilder::with_capacity(size, size * 8);
    let mut written = 0usize;
    while written < size {
        let run_len = rng_s.random_range(1..4).min(size - written);
        let choice = rng_s.random_range(0..100);
        if choice < 85 {
            for _ in 0..run_len {
                utf8_b.append_value("");
            }
        } else if choice < 90 {
            for _ in 0..run_len {
                utf8_b.append_value("const");
            }
        } else {
            for _ in 0..run_len {
                utf8_b.append_value(rf_random_string(&mut rng_s));
            }
        }
        written += run_len;
    }

    let ts_vals: Vec<i64> = (0..size).map(|i| (i % 10_000) as i64).collect();

    let mut pt_b = StringBuilder::with_capacity(size, size * 6);
    let mut sel_b = StringBuilder::with_capacity(size, size * 8);
    let mut mod_c_b = StringBuilder::with_capacity(size, size * 12);
    let mut mod_u_b = StringBuilder::with_capacity(size, size * 12);
    let mut unsel_u_b = StringBuilder::with_capacity(size, size * 16);
    let mut unsel_c_b = StringBuilder::with_capacity(size, size * 16);

    let pt_row = size / 2;
    for i in 0..size {
        pt_b.append_value(if i == pt_row { "unique_point" } else { "other" });
        sel_b.append_value(if i % 100 == 0 { "selected" } else { "other" });
        mod_c_b.append_value(if i % 10_000 < 1_000 {
            "mod_clustered"
        } else {
            "other"
        });
        mod_u_b.append_value(if i % 10 == 1 {
            "mod_unclustered"
        } else {
            "other"
        });
        unsel_u_b.append_value(if i % 100 != 0 {
            "unsel_unclustered"
        } else {
            "other"
        });
        unsel_c_b.append_value(if i % 10_000 >= 1_000 {
            "unsel_clustered"
        } else {
            "other"
        });
    }

    let schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("int64", DataType::Int64, false),
        Field::new("float64", DataType::Float64, false),
        Field::new("utf8", DataType::Utf8, false),
        Field::new("ts", DataType::Int64, false),
        Field::new("pt", DataType::Utf8, false),
        Field::new("sel", DataType::Utf8, false),
        Field::new("mod_clustered", DataType::Utf8, false),
        Field::new("mod_unclustered", DataType::Utf8, false),
        Field::new("unsel_unclustered", DataType::Utf8, false),
        Field::new("unsel_clustered", DataType::Utf8, false),
    ]));

    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(int_vals)),
        Arc::new(Float64Array::from(float_vals)),
        Arc::new(utf8_b.finish()),
        Arc::new(Int64Array::from(ts_vals)),
        Arc::new(pt_b.finish()),
        Arc::new(sel_b.finish()),
        Arc::new(mod_c_b.finish()),
        Arc::new(mod_u_b.finish()),
        Arc::new(unsel_u_b.finish()),
        Arc::new(unsel_c_b.finish()),
    ];
    Ok(RecordBatch::try_new(schema, cols)?)
}

async fn run_row_filter_suite(
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let batch = build_row_filter_batch(100_000)?;
    let cetra_batch = arrow_to_cetra_batch(&batch)?;
    let raw_bytes = batch.get_array_memory_size() as u64;
    let wl = "row_filter_100k";

    let pq_default = if enabled.contains(&"pq_default") {
        let (b, ms) = write_generic_parquet(ParquetVariant::Default, &batch, &[], &[], 1_000_000)?;
        write_rows.push(mk_write_row(
            wl,
            "pq_default",
            raw_bytes,
            b.len() as u64,
            ms,
        ));
        Some(b)
    } else {
        None
    };
    let pq_tuned = if enabled.contains(&"pq_tuned") {
        let (b, ms) = write_generic_parquet(ParquetVariant::Tuned, &batch, &[], &[], 32_768)?;
        write_rows.push(mk_write_row(wl, "pq_tuned", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let pq_cetra = if enabled.contains(&"pq_cetra") {
        let (b, ms) = write_generic_parquet(ParquetVariant::Cetra, &batch, &[], &[], 100_000)?;
        write_rows.push(mk_write_row(wl, "pq_cetra", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let vortex_bytes = if enabled.contains(&"vortex") {
        let (files, ms) = write_vortex_files(vortex_session, &[batch.clone()]).await?;
        let b = files.into_iter().next().ok_or("missing vortex output")?;
        write_rows.push(mk_write_row(wl, "vortex", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let strata_bytes = if enabled.contains(&"strata") {
        match write_generic_strata(&batch, &[], &[]) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(wl, "strata", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite B (`row-filter`): `strata` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let statam_l0 = if enabled.contains(&"statam_l0") {
        let (b, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L0Fast,
            &[],
            &[],
            None,
            &[],
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let statam_l1 = if enabled.contains(&"statam_l1") {
        let (b, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L1Compact,
            &[],
            &[],
            None,
            &[],
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l1", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };

    let patterns: [(&str, &str, usize, &str); 6] = [
        ("RF_pt_0_001pct", "pt", 4, "unique_point"),
        ("RF_sel_1pct_unclustered", "sel", 5, "selected"),
        (
            "RF_mod_10pct_clustered",
            "mod_clustered",
            6,
            "mod_clustered",
        ),
        (
            "RF_mod_10pct_unclustered",
            "mod_unclustered",
            7,
            "mod_unclustered",
        ),
        (
            "RF_unsel_99pct_unclustered",
            "unsel_unclustered",
            8,
            "unsel_unclustered",
        ),
        (
            "RF_unsel_90pct_clustered",
            "unsel_clustered",
            9,
            "unsel_clustered",
        ),
    ];

    let s3_cfg = SimulatedS3Config::default();
    let proj_cols = [0usize, 1, 2, 3];
    let strata_file_local = match &strata_bytes {
        Some(b) => Some(open_single_strata_local(b).await?),
        None => None,
    };
    let vortex_file_local = match &vortex_bytes {
        Some(b) => Some(open_vortex_local(vortex_session, &[b.clone()]).await?),
        None => None,
    };
    let statam_l0_local = statam_l0
        .as_ref()
        .map(|b| StatamReader::open(Arc::new(SliceReader::new(b.clone())), None))
        .transpose()?;
    let statam_l1_local = statam_l1
        .as_ref()
        .map(|b| StatamReader::open(Arc::new(SliceReader::new(b.clone())), None))
        .transpose()?;

    for &(qname, filter_col, filter_idx, filter_val) in &patterns {
        let mut expected_matches: Option<usize> = None;
        let pq_preds = [GenPqPred::Utf8Eq {
            col: filter_col.to_owned(),
            val: filter_val.to_owned(),
        }];

        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &pq_default, false),
            ("pq_tuned", &pq_tuned, true),
            ("pq_cetra", &pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                PageIndexPolicy::Optional
            } else {
                PageIndexPolicy::Skip
            });
            let meta = ArrowReaderMetadata::load(bytes, opts)?;
            let (matched, local_ms, local_p95_ms) = measure_local_ms(
                || execute_gen_parquet_local(bytes, &meta, &proj_cols, &pq_preds),
                warm_runs,
            )?;

            let s3_reader = SimS3ChunkReader::new(Arc::new(SimulatedS3Reader::from_bytes(
                bytes.clone(),
                s3_cfg,
            )));
            s3_reader.s3().evict_all();
            s3_reader.s3().reset_counters();
            let s3_matched = execute_gen_parquet_s3(&s3_reader, with_pi, &proj_cols, &pq_preds)?;
            if s3_matched != matched {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.s3().simulated_latency_ms(),
                s3_gets: s3_reader.s3().counters().gets,
                s3_bytes: s3_reader.s3().counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let proj = select(["int64", "float64", "utf8", "ts"], root());
            let filt = eq(col(filter_col), lit(filter_val));
            let (matched, local_ms, local_p95_ms) = measure_local_async_ms(
                || execute_vortex_scan(vortex_session, &local_files[0], &proj, Some(&filt), None),
                warm_runs,
            )
            .await?;

            let s3_reader = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
            s3_reader.evict_all();
            s3_reader.reset_counters();
            let vx_read = Arc::new(SimS3VortexRead::new(
                Arc::clone(&s3_reader) as Arc<dyn RangeReader>
            ));
            let s3_file = vortex_session.open_options().open_read(vx_read).await?;
            let s3_matched =
                execute_vortex_scan(vortex_session, &s3_file, &proj, Some(&filt), None).await?;
            if s3_matched != matched {
                return Err(format!("vortex {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, "vortex", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.simulated_latency_ms(),
                s3_gets: s3_reader.counters().gets,
                s3_bytes: s3_reader.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_file)) = (&strata_bytes, &strata_file_local) {
            let run_strata = |f: Arc<StrataFile>| async move {
                let req = StrataScanRequest {
                    projection: proj_cols.to_vec(),
                    predicate: Some(StrataPred::Cmp(
                        filter_idx,
                        CmpOp::Eq,
                        Lit::Utf8(filter_val.to_owned()),
                    )),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Scan,
                    views: false,
                    top: None,
                    row_filter: None,
                };
                let batches = f.scan(&req).await?;
                Ok::<usize, Box<dyn std::error::Error + Send + Sync>>(
                    batches.iter().map(RecordBatch::num_rows).sum(),
                )
            };
            let (matched, local_ms, local_p95_ms) =
                measure_local_async_ms(|| run_strata(Arc::clone(local_file)), warm_runs).await?;

            let (s3_file, s3_reader) = open_single_strata_s3(bytes, s3_cfg).await?;
            let s3_matched = run_strata(s3_file).await?;
            if s3_matched != matched {
                return Err(format!("strata {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, "strata", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "strata".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.simulated_latency_ms(),
                s3_gets: s3_reader.counters().gets,
                s3_bytes: s3_reader.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        for (fmt, bytes_opt, local_opt) in [
            ("statam_l0", &statam_l0, &statam_l0_local),
            ("statam_l1", &statam_l1, &statam_l1_local),
        ] {
            let (Some(bytes), Some(local_reader)) = (bytes_opt, local_opt) else {
                continue;
            };
            let req = ScanRequest {
                projection: Some(proj_cols.to_vec()),
                predicates: vec![StatamPred::new(
                    filter_col,
                    ZonePred::Eq(ZoneValue::Bytes(filter_val.as_bytes().to_vec())),
                )],
                read_threads,
                ..ScanRequest::default()
            };
            let (matched, local_ms, local_p95_ms) =
                measure_local_ms(|| execute_statam_scan(local_reader, &req), warm_runs)?;

            let s3_file = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
            s3_file.evict_all();
            s3_file.reset_counters();
            let s3_reader = StatamReader::open(Arc::clone(&s3_file) as Arc<dyn RangeReader>, None)?;
            let s3_matched = execute_statam_scan(&s3_reader, &req)?;
            if s3_matched != matched {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_file.simulated_latency_ms(),
                s3_gets: s3_file.counters().gets,
                s3_bytes: s3_file.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }
    }

    Ok(())
}

// ============================================================================
// Suite C: Random-Access (Vortex's scattered multi-row take: 100 & 1,000 rows)
// ============================================================================

fn build_random_access_batch(size: usize) -> BenchResult<RecordBatch> {
    let mut rng = SmallRng::seed_from_u64(99);
    let services = ["chat", "agent", "rag", "eval", "code", "support"];
    let envs = ["prod", "staging", "dev"];

    let mut row_id = Vec::with_capacity(size);
    let mut ts = Vec::with_capacity(size);
    let mut m_i1 = Vec::with_capacity(size);
    let mut m_i2 = Vec::with_capacity(size);
    let mut m_i3 = Vec::with_capacity(size);
    let mut m_i4 = Vec::with_capacity(size);
    let mut m_f1 = Vec::with_capacity(size);
    let mut m_f2 = Vec::with_capacity(size);
    let mut dim_svc = StringBuilder::with_capacity(size, size * 8);
    let mut dim_env = StringBuilder::with_capacity(size, size * 8);
    let mut msg = StringBuilder::with_capacity(size, size * 48);
    let mut payload_json = StringBuilder::with_capacity(size, size * 80);

    for i in 0..size {
        row_id.push(i as i64);
        ts.push(DAY_START + (i as i64) * 10_000_000);
        m_i1.push(rng.random_range(0..10_000));
        m_i2.push(rng.random_range(0..50_000));
        m_i3.push(rng.random_range(-1_000..1_000));
        m_i4.push(rng.random_range(1..1_000_000));
        m_f1.push(rng.random_range(0.0..1000.0));
        m_f2.push(rng.random_range(0.0..1.0));
        dim_svc.append_value(services[rng.random_range(0..services.len())]);
        dim_env.append_value(envs[rng.random_range(0..envs.len())]);
        let r_len = rng.random_range(20..60);
        let token = rng.random_range(1000..9999);
        msg.append_value(format!(
            "span-event-{i:06}-tok-{token}-pad-{}",
            "x".repeat(r_len)
        ));
        payload_json.append_value(format!(
            "{{\"row\":{i},\"tok\":{token},\"status\":\"ok\",\"note\":\"{}\"}}",
            "a".repeat(rng.random_range(16..48))
        ));
    }

    let schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("row_id", DataType::Int64, false),
        Field::new("ts", DataType::Int64, false),
        Field::new("m_i1", DataType::Int64, false),
        Field::new("m_i2", DataType::Int64, false),
        Field::new("m_i3", DataType::Int64, false),
        Field::new("m_i4", DataType::Int64, false),
        Field::new("m_f1", DataType::Float64, false),
        Field::new("m_f2", DataType::Float64, false),
        Field::new("dim_svc", DataType::Utf8, false),
        Field::new("dim_env", DataType::Utf8, false),
        Field::new("msg", DataType::Utf8, false),
        Field::new("payload_json", DataType::Utf8, false),
    ]));

    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(row_id)),
        Arc::new(Int64Array::from(ts)),
        Arc::new(Int64Array::from(m_i1)),
        Arc::new(Int64Array::from(m_i2)),
        Arc::new(Int64Array::from(m_i3)),
        Arc::new(Int64Array::from(m_i4)),
        Arc::new(Float64Array::from(m_f1)),
        Arc::new(Float64Array::from(m_f2)),
        Arc::new(dim_svc.finish()),
        Arc::new(dim_env.finish()),
        Arc::new(msg.finish()),
        Arc::new(payload_json.finish()),
    ];
    Ok(RecordBatch::try_new(schema, cols)?)
}

fn scattered_row_indices(total_rows: usize, count: usize, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut set = BTreeSet::new();
    while set.len() < count.min(total_rows) {
        set.insert(rng.random_range(0..total_rows) as u64);
    }
    set.into_iter().collect()
}

fn execute_parquet_row_selection<R: ChunkReader + 'static>(
    mut builder: ParquetRecordBatchReaderBuilder<R>,
    sorted_indices: &[u64],
) -> BenchResult<usize> {
    let meta = Arc::clone(builder.metadata());
    let mut rgs = Vec::new();
    let mut selectors = Vec::new();
    let mut rg_start = 0u64;
    let mut idx_pos = 0usize;

    for rg_idx in 0..meta.num_row_groups() {
        let rg_rows = u64::try_from(meta.row_group(rg_idx).num_rows()).unwrap_or(0);
        let rg_end = rg_start + rg_rows;
        while idx_pos < sorted_indices.len() && sorted_indices[idx_pos] < rg_start {
            idx_pos += 1;
        }
        let mut cursor = rg_start;
        let mut rg_has_hit = false;
        while idx_pos < sorted_indices.len() && sorted_indices[idx_pos] < rg_end {
            let target = sorted_indices[idx_pos];
            if !rg_has_hit {
                rgs.push(rg_idx);
                rg_has_hit = true;
            }
            if target > cursor {
                selectors.push(RowSelector::skip((target - cursor) as usize));
            }
            selectors.push(RowSelector::select(1));
            cursor = target + 1;
            idx_pos += 1;
        }
        if rg_has_hit && cursor < rg_end {
            selectors.push(RowSelector::skip((rg_end - cursor) as usize));
        }
        rg_start = rg_end;
    }

    if rgs.is_empty() {
        return Ok(0);
    }

    builder = builder
        .with_row_groups(rgs)
        .with_row_selection(RowSelection::from(selectors))
        .with_batch_size(16_384);

    let reader = builder.build()?;
    let mut rows = 0usize;
    for b in reader {
        rows += b?.num_rows();
    }
    Ok(rows)
}

async fn execute_vortex_take(
    session: &VortexSession,
    file: &VortexFile,
    sorted_indices: &[u64],
) -> BenchResult<usize> {
    let idx_buf = vortex::scan::strict_sorted_buffer::StrictSortedBuffer::try_new(
        Buffer::from_iter(sorted_indices.iter().copied()),
    )?;
    let projection = root().bind(file.dtype())?.optimize_recursive()?;
    let mut stream = file
        .scan()?
        .with_projection(projection)
        .with_row_indices(idx_buf)
        .into_array_stream()?;
    let mut total = 0usize;
    while let Some(chunk) = stream.try_next().await? {
        if chunk.is_empty() {
            continue;
        }
        let batch = chunk_to_record_batch(session, chunk)?;
        total += batch.num_rows();
    }
    Ok(total)
}

async fn run_random_access_suite(
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let total_rows = 250_000usize;
    let batch = build_random_access_batch(total_rows)?;
    let cetra_batch = arrow_to_cetra_batch(&batch)?;
    let raw_bytes = batch.get_array_memory_size() as u64;
    let wl = "random_access_250k";

    let pq_default = if enabled.contains(&"pq_default") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Default,
            &batch,
            &["row_id"],
            &["row_id"],
            1_000_000,
        )?;
        write_rows.push(mk_write_row(
            wl,
            "pq_default",
            raw_bytes,
            b.len() as u64,
            ms,
        ));
        Some(b)
    } else {
        None
    };
    let pq_tuned = if enabled.contains(&"pq_tuned") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Tuned,
            &batch,
            &["row_id"],
            &["row_id"],
            32_768,
        )?;
        write_rows.push(mk_write_row(wl, "pq_tuned", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let pq_cetra = if enabled.contains(&"pq_cetra") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Cetra,
            &batch,
            &["row_id"],
            &["row_id"],
            100_000,
        )?;
        write_rows.push(mk_write_row(wl, "pq_cetra", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let vortex_bytes = if enabled.contains(&"vortex") {
        let (files, ms) = write_vortex_files(vortex_session, &[batch.clone()]).await?;
        let b = files.into_iter().next().ok_or("missing vortex output")?;
        write_rows.push(mk_write_row(wl, "vortex", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    if enabled.contains(&"strata") {
        match write_generic_strata(&batch, &["row_id"], &["row_id"]) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(wl, "strata", raw_bytes, b.len() as u64, ms));
                notes.push("Suite C (`random-access`): `strata` query = N/A (scattered `row_indices` take not wired)".to_owned());
            }
            Err(e) => {
                notes.push(format!("Suite C (`random-access`): `strata` = N/A ({e})"));
            }
        }
    }
    let statam_l0 = if enabled.contains(&"statam_l0") {
        let (b, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L0Fast,
            &[0],
            &["row_id"],
            Some("row_id"),
            &[],
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let statam_l1 = if enabled.contains(&"statam_l1") {
        let (b, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L1Compact,
            &[0],
            &["row_id"],
            Some("row_id"),
            &[],
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l1", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };

    let take_100 = scattered_row_indices(total_rows, 100, 1001);
    let take_1000 = scattered_row_indices(total_rows, 1_000, 1002);
    let cases = [("RA_take_100", &take_100), ("RA_take_1000", &take_1000)];
    let s3_cfg = SimulatedS3Config::default();

    let vortex_file_local = match &vortex_bytes {
        Some(b) => Some(open_vortex_local(vortex_session, &[b.clone()]).await?),
        None => None,
    };
    let statam_l0_local = statam_l0
        .as_ref()
        .map(|b| StatamReader::open(Arc::new(SliceReader::new(b.clone())), None))
        .transpose()?;
    let statam_l1_local = statam_l1
        .as_ref()
        .map(|b| StatamReader::open(Arc::new(SliceReader::new(b.clone())), None))
        .transpose()?;

    for &(qname, indices) in &cases {
        let mut expected_matches: Option<usize> = Some(indices.len());

        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &pq_default, false),
            ("pq_tuned", &pq_tuned, true),
            ("pq_cetra", &pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                PageIndexPolicy::Optional
            } else {
                PageIndexPolicy::Skip
            });
            let meta = ArrowReaderMetadata::load(bytes, opts)?;
            let (matched, local_ms, local_p95_ms) = measure_local_ms(
                || {
                    let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(
                        bytes.clone(),
                        meta.clone(),
                    );
                    execute_parquet_row_selection(builder, indices)
                },
                warm_runs,
            )?;

            let s3_reader = SimS3ChunkReader::new(Arc::new(SimulatedS3Reader::from_bytes(
                bytes.clone(),
                s3_cfg,
            )));
            s3_reader.s3().evict_all();
            s3_reader.s3().reset_counters();
            let s3_builder = open_parquet_builder(&s3_reader, with_pi)?;
            let s3_matched = execute_parquet_row_selection(s3_builder, indices)?;
            if s3_matched != matched {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.s3().simulated_latency_ms(),
                s3_gets: s3_reader.s3().counters().gets,
                s3_bytes: s3_reader.s3().counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let (matched, local_ms, local_p95_ms) = measure_local_async_ms(
                || execute_vortex_take(vortex_session, &local_files[0], indices),
                warm_runs,
            )
            .await?;

            let s3_reader = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
            s3_reader.evict_all();
            s3_reader.reset_counters();
            let vx_read = Arc::new(SimS3VortexRead::new(
                Arc::clone(&s3_reader) as Arc<dyn RangeReader>
            ));
            let s3_file = vortex_session.open_options().open_read(vx_read).await?;
            let s3_matched = execute_vortex_take(vortex_session, &s3_file, indices).await?;
            if s3_matched != matched {
                return Err(format!("vortex {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, "vortex", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_reader.simulated_latency_ms(),
                s3_gets: s3_reader.counters().gets,
                s3_bytes: s3_reader.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        for (fmt, bytes_opt, local_opt) in [
            ("statam_l0", &statam_l0, &statam_l0_local),
            ("statam_l1", &statam_l1, &statam_l1_local),
        ] {
            let (Some(bytes), Some(local_reader)) = (bytes_opt, local_opt) else {
                continue;
            };
            let execute_take = |reader: &StatamReader| -> BenchResult<usize> {
                let batches = reader.take_rows(indices, None, read_threads)?;
                Ok(batches.iter().map(Batch::num_rows).sum())
            };
            let (matched, local_ms, local_p95_ms) =
                measure_local_ms(|| execute_take(local_reader), warm_runs)?;

            let s3_file = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
            s3_file.evict_all();
            s3_file.reset_counters();
            let s3_reader = StatamReader::open(Arc::clone(&s3_file) as Arc<dyn RangeReader>, None)?;
            let s3_matched = execute_take(&s3_reader)?;
            if s3_matched != matched {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
            }
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms: s3_file.simulated_latency_ms(),
                s3_gets: s3_file.counters().gets,
                s3_bytes: s3_file.counters().bytes_from_s3,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }
    }

    Ok(())
}

fn render_external_report(
    cli: &CliConfig,
    scale: BenchScale,
    write_rows: &[WriteBenchRow],
    query_rows: &[QueryBenchRow],
    notes: &[String],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n# External Parquet + Vortex Benchmark Report (`scale={}`)",
        scale.label()
    );
    let _ = writeln!(out, "\n- **Host:** {}", machine_summary());
    let _ = writeln!(
        out,
        "- **Statam Write Threads:** `{}`",
        cli.statam_write_threads
    );
    let _ = writeln!(
        out,
        "- **Statam Read Threads:** `{}`",
        cli.statam_read_threads
    );
    let _ = writeln!(out, "- **Formats:** `{}`", cli.formats.join("`, `"));
    let _ = writeln!(
        out,
        "- **Simulated S3 Model:** `1 MiB` `ReadCache` block rounding + run coalescing, `25.0 ms / GET`, `100 MiB/s`"
    );

    if !notes.is_empty() {
        let _ = writeln!(out, "\n## Notes");
        for n in notes {
            let _ = writeln!(out, "- {n}");
        }
    }

    let _ = writeln!(out, "\n## 1. Storage Size & Write Speed");
    let _ = writeln!(
        out,
        "\n| Workload | Format | Stored (KiB) | Raw (KiB) | Ratio | Write (ms) | Write (MiB/s) |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|---:|---:|");
    for r in write_rows {
        let ratio = if r.stored_bytes > 0 {
            r.raw_bytes as f64 / r.stored_bytes as f64
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.2}x | {:.2} | {:.1} |",
            r.workload,
            r.format,
            format_kib(r.stored_bytes),
            format_kib(r.raw_bytes),
            ratio,
            r.write_ms,
            r.write_mib_s
        );
    }

    let _ = writeln!(out, "\n## 2. Read Latency & S3 Cost");
    let _ = writeln!(
        out,
        "\n| Workload | Query | Format | Matched | Local Median (ms) | Local p95 (ms) | S3 (ms) | S3 GETs | S3 KiB |"
    );
    let _ = writeln!(out, "|---|---|---|---:|---:|---:|---:|---:|---:|");
    for r in query_rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.3} | {:.3} | {:.2} | {} | {} |",
            r.workload,
            r.query,
            r.format,
            r.rows_matched,
            r.local_ms,
            r.local_p95_ms,
            r.s3_ms,
            r.s3_gets,
            format_kib(r.s3_bytes)
        );
    }

    out
}

async fn run() -> BenchResult<()> {
    let cli = parse_cli_args()?;
    let scale = BenchScale::from_env();
    let warm_runs = if scale == BenchScale::Quick { 5 } else { 10 };
    let vortex_session = new_vortex_session();

    let mut write_rows = Vec::new();
    let mut query_rows = Vec::new();
    let mut notes = Vec::new();

    if cli.suite.includes_clickbench() {
        run_clickbench_suite(
            &cli.hits_parquet,
            &cli.formats,
            warm_runs,
            cli.statam_write_threads,
            cli.statam_read_threads,
            &vortex_session,
            &mut write_rows,
            &mut query_rows,
            &mut notes,
        )
        .await?;
    }

    if cli.suite.includes_row_filter() {
        run_row_filter_suite(
            &cli.formats,
            warm_runs,
            cli.statam_write_threads,
            cli.statam_read_threads,
            &vortex_session,
            &mut write_rows,
            &mut query_rows,
            &mut notes,
        )
        .await?;
    }

    if cli.suite.includes_random_access() {
        run_random_access_suite(
            &cli.formats,
            warm_runs,
            cli.statam_write_threads,
            cli.statam_read_threads,
            &vortex_session,
            &mut write_rows,
            &mut query_rows,
            &mut notes,
        )
        .await?;
    }

    let md = render_external_report(&cli, scale, &write_rows, &query_rows, &notes);
    println!("{md}");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    if let Err(err) = run().await {
        eprintln!("external_bench failed: {err}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
