//! External Parquet + Vortex + Lance + Strata + Statam Benchmark Suites:
//! - Suite A (`clickbench`): Real ClickBench `hits_1.parquet` (100k rows x 105 cols)
//! - Suite B (`row-filter`): Parquet's 6-Pattern Selectivity x Clustering Matrix (100k rows x 10 cols)
//! - Suite C (`random-access`): Vortex's scattered multi-row take (100 & 1,000 rows from 250k x 12 cols)
//! - Suite D (`jsonbench`): Real Bluesky Jetstream JSON (`bluesky_0001.json.gz`, 100k events, ShreddedMap + `.stpk`)
//! - Suite E (`wildchat`): Real WildChat-1M (`wildchat_00000.parquet`, 15k multilingual turns + Unicode `ILIKE`)
//! - Suite F (`hermes`): Real Hermes Function Calling (`hermes_fc.parquet`, cross-trace `tool_definitions` dedup)

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, LargeListArray, LargeStringArray,
    ListArray, MapArray, MapBuilder, RecordBatch, StringArray, StringBuilder, StringViewArray,
    StructArray, TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray,
};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Fields, Schema, SchemaRef, TimeUnit};
use arrow_array_60::Array as Array60;
use arrow_array_60::cast::AsArray as AsArray60;
use arrow_array_60::{ListArray as ListArray60, MapArray as MapArray60};
use bytes::Bytes;
use cetra_column::{
    Batch, Bitmap, Column, DataType as ColType, Field as ColField, FlatColumn, FlatData, MapData,
    Schema as ColSchema, Utf8Data,
};
use cetra_statam::{
    ChunkPackBuilder, ChunkPackReader, CoalesceMode, IdLookup, InMemoryPackResolver, PackResolver,
    PackWriteOptions, Predicate as StatamPred, RangeReader, ScanRequest, SimulatedS3Config,
    SliceReader, StatamReader, StatamWriter, SubstringQuery, TableKind, WriteOptions, WriteProfile,
    ZonePred, ZoneValue,
};
use flate2::read::GzDecoder;
use futures::stream::TryStreamExt;
use lance_file::reader::FileReader as LanceFileReader;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowPredicate, ArrowPredicateFn, ArrowReaderMetadata, ArrowReaderOptions,
    ParquetRecordBatchReaderBuilder, RowFilter, RowSelection, RowSelector,
};
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::arrow::async_reader::ParquetRecordBatchStreamBuilder;
use parquet::bloom_filter::Sbbf;
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaData, RowGroupMetaData};
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::reader::ChunkReader;
use parquet::file::statistics::Statistics;
use parquet::schema::types::SchemaDescriptor;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use serde_json::Value;
use strata::{
    CmpOp, Config as StrataConfig, Lit, Predicate as StrataPred, ScanKind,
    ScanRequest as StrataScanRequest, StrataFile, StrataWriter,
};
use vortex::buffer::Buffer;
use vortex::expr::{Expression, and, col, eq, gt_eq, lit, lt_eq, not, root, select};
use vortex::file::{OpenOptionsSessionExt, VortexFile};
use vortex::session::VortexSession;

use statam_bench7::lance_bench::{
    lance_scan_projected, lance_take_indices, open_lance_from_bench_s3, open_lance_local,
    prepare_lance_batches, write_lance_bytes,
};
use statam_bench7::queries::{
    PACK_ID, SimS3PackResolver, check_parity, measure_local_async_ms, measure_local_ms,
};
use statam_bench7::real_s3::{
    BenchS3ParquetAsyncReader, BenchS3Reader, RealS3Mode, run_s3_adaptive, s3_only_mode,
    sim_s3_header_summary,
};
use statam_bench7::report::{QueryBenchRow, WriteBenchRow, format_kib};
use statam_bench7::s3_parquet::{find_leaf_idx, open_parquet_async_builder};
use statam_bench7::schema::{
    BenchResult, ParquetVariant, arrow_to_cetra_batch, generic_parquet_writer_props,
    normalize_arrow_batch,
};
use statam_bench7::strata_bench::{SimS3ObjectStore, SliceObjectStore};
use statam_bench7::vortex_bench::{
    SimS3VortexRead, chunk_to_record_batch, new_vortex_session, open_vortex_local,
    str_value_at as vortex_str_value_at, write_vortex_files,
};
use statam_bench7::workloads::{BenchScale, DAY_START};

const DEFAULT_EXTERNAL_FORMATS: [&str; 7] = [
    "pq_default",
    "pq_tuned",
    "vortex",
    "lance",
    "strata",
    "statam_l0",
    "statam_l1",
];

const ALL_EXTERNAL_FORMATS: [&str; 8] = [
    "pq_default",
    "pq_tuned",
    "pq_cetra",
    "vortex",
    "lance",
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
    JsonBench,
    WildChat,
    Hermes,
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

    fn includes_jsonbench(self) -> bool {
        matches!(self, Self::All | Self::JsonBench)
    }

    fn includes_wildchat(self) -> bool {
        matches!(self, Self::All | Self::WildChat)
    }

    fn includes_hermes(self) -> bool {
        matches!(self, Self::All | Self::Hermes)
    }
}

struct CliConfig {
    suite: SuiteSelection,
    statam_write_threads: usize,
    statam_read_threads: usize,
    hits_parquet: PathBuf,
    bluesky_json: PathBuf,
    wildchat_parquet: PathBuf,
    hermes_parquet: PathBuf,
    formats: Vec<&'static str>,
}

fn default_corpus_path(env_var: &str, filename: &str) -> PathBuf {
    if let Ok(p) = std::env::var(env_var)
        && !p.trim().is_empty()
    {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    PathBuf::from(home).join("statam-corpora").join(filename)
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
        if !ALL_EXTERNAL_FORMATS.contains(r) {
            return Err(format!(
                "unknown format '{r}' (expected subset of {:?})",
                ALL_EXTERNAL_FORMATS
            )
            .into());
        }
    }
    Ok(ALL_EXTERNAL_FORMATS
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
        "jsonbench" | "json-bench" | "json_bench" => Ok(SuiteSelection::JsonBench),
        "wildchat" | "wild-chat" | "wild_chat" => Ok(SuiteSelection::WildChat),
        "hermes" => Ok(SuiteSelection::Hermes),
        other => Err(format!(
            "unknown --suite '{other}' (expected all|clickbench|row-filter|random-access|jsonbench|wildchat|hermes)"
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
    let mut hits_parquet = default_corpus_path("STATAM_HITS_PARQUET", "hits_1.parquet");
    let mut bluesky_json = default_corpus_path("STATAM_BLUESKY_JSON", "bluesky_0001.json.gz");
    let mut wildchat_parquet =
        default_corpus_path("STATAM_WILDCHAT_PARQUET", "wildchat_00000.parquet");
    let mut hermes_parquet = default_corpus_path("STATAM_HERMES_PARQUET", "hermes_fc.parquet");
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
        } else if let Some(val) = arg.strip_prefix("--bluesky-json=") {
            bluesky_json = PathBuf::from(val);
        } else if arg == "--bluesky-json" {
            let val = args.next().ok_or("--bluesky-json requires a path")?;
            bluesky_json = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--wildchat-parquet=") {
            wildchat_parquet = PathBuf::from(val);
        } else if arg == "--wildchat-parquet" {
            let val = args.next().ok_or("--wildchat-parquet requires a path")?;
            wildchat_parquet = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--hermes-parquet=") {
            hermes_parquet = PathBuf::from(val);
        } else if arg == "--hermes-parquet" {
            let val = args.next().ok_or("--hermes-parquet requires a path")?;
            hermes_parquet = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--formats=") {
            formats_override = Some(val.to_owned());
        } else if arg == "--formats" {
            let val = args.next().ok_or("--formats requires a csv list")?;
            formats_override = Some(val);
        } else {
            return Err(format!(
                "unknown argument '{arg}' (supported: --suite, --statam-write-threads, --statam-read-threads, --hits-parquet, --bluesky-json, --wildchat-parquet, --hermes-parquet, --formats)"
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
        bluesky_json,
        wildchat_parquet,
        hermes_parquet,
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

#[allow(clippy::too_many_arguments)]
fn write_generic_statam(
    batch: &Batch,
    profile: WriteProfile,
    sort_cols: &[u16],
    bloom_cols: &[&str],
    hot_bloom_col: Option<&str>,
    narrow_trigram_cols: &[&str],
    payload_cols: &[&str],
    use_pack: bool,
    write_threads: usize,
) -> BenchResult<(Bytes, Option<Bytes>, f64)> {
    let t0 = Instant::now();
    let mut pack_opts = PackWriteOptions::default();
    if write_threads > 1 {
        pack_opts.write_threads = write_threads;
    }
    let mut pb = use_pack.then(|| ChunkPackBuilder::new(PACK_ID, pack_opts));

    let mut opts = WriteOptions::for_table(TableKind::Generic, profile);
    opts.sort_cols = sort_cols.to_vec();
    opts.bloom_cols = bloom_cols.iter().map(|&s| s.to_owned()).collect();
    opts.hot_bloom_col = hot_bloom_col.map(str::to_owned);
    opts.payload_cols = payload_cols.iter().map(|&s| s.to_owned()).collect();
    opts.narrow_trigram_cols = if profile == WriteProfile::L1Compact {
        narrow_trigram_cols.iter().map(|&s| s.to_owned()).collect()
    } else {
        Vec::new()
    };
    if pb.is_some() {
        opts.pack_id = Some(PACK_ID);
    }
    if write_threads > 1 {
        opts.write_threads = write_threads;
    }

    let mut buf = Vec::new();
    let mut w = StatamWriter::new(
        std::io::Cursor::new(&mut buf),
        Arc::clone(batch.schema()),
        opts,
        pb.as_mut(),
    )?;
    w.write_batch(batch)?;
    w.finish()?;
    let pack_bytes = if let Some(builder) = pb {
        let mut pbuf = Vec::new();
        builder.finish(&mut pbuf)?;
        Some(Bytes::from(pbuf))
    } else {
        None
    };
    let statam_bytes = Bytes::from(buf);
    if std::env::var_os("STATAM_DUMP_STATS").is_some() && profile == WriteProfile::L1Compact {
        dump_statam_breakdown(&statam_bytes, pack_bytes.as_ref())?;
    }
    Ok((
        statam_bytes,
        pack_bytes,
        t0.elapsed().as_secs_f64() * 1000.0,
    ))
}

fn dump_statam_breakdown(statam_bytes: &Bytes, pack_bytes: Option<&Bytes>) -> BenchResult<()> {
    let reader = StatamReader::open(Arc::new(SliceReader::new(statam_bytes.clone())), None)?;
    let meta = reader.meta();
    let mut by_enc: BTreeMap<String, (usize, u64, u64)> = BTreeMap::new();
    let mut by_col: Vec<(String, u64, u64, usize)> = Vec::new();
    for c in &meta.col_dir {
        let name = meta
            .schema_meta
            .schema
            .fields()
            .get(usize::from(c.logical_col_id))
            .map_or_else(|| format!("phys_{}", c.phys_col_id), |f| f.name.clone());
        let start = c.first_block_idx as usize;
        let end = start + c.num_blocks as usize;
        let mut col_stored = 0u64;
        let mut col_raw = 0u64;
        for b in &meta.block_dir[start..end] {
            let e = by_enc
                .entry(format!("{:?}/{:?}", b.encoding, b.codec))
                .or_insert((0, 0, 0));
            e.0 += 1;
            e.1 += u64::from(b.stored_len);
            e.2 += u64::from(b.raw_len);
            col_stored += u64::from(b.stored_len);
            col_raw += u64::from(b.raw_len);
        }
        by_col.push((
            format!("{name}(p{})", c.phys_col_id),
            col_stored,
            col_raw,
            c.num_blocks as usize,
        ));
    }
    by_col.sort_by(|a, b| b.1.cmp(&a.1));
    let mut lazy_by_kind: BTreeMap<String, u64> = BTreeMap::new();
    for le in &meta.lazy_dir {
        *lazy_by_kind.entry(format!("{:?}", le.kind)).or_insert(0) += u64::from(le.len);
    }
    let hot_tail_len = meta.file_len.saturating_sub(meta.hot_tail_start);
    eprintln!(
        "[STATAM_STATS] .statam={} KiB | rows={} rgs={} blocks={} | hot_tail={} KiB (hot_bloom={} KiB, hot_dict={} KiB) | lazy={:?}",
        statam_bytes.len() / 1024,
        meta.postscript.row_count,
        meta.postscript.num_row_groups,
        meta.postscript.total_blocks,
        hot_tail_len / 1024,
        meta.hot_bloom_bytes.len() / 1024,
        meta.hot_dict_bytes.len() / 1024,
        lazy_by_kind
            .iter()
            .map(|(k, v)| format!("{k}={}KiB", v / 1024))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "[STATAM_STATS]   encodings: {:?}",
        by_enc
            .iter()
            .map(|(k, (n, s, r))| format!("{k}:{n}blk/{}KiB(raw {}KiB)", s / 1024, r / 1024))
            .collect::<Vec<_>>()
    );
    eprintln!(
        "[STATAM_STATS]   top_cols: {:?}",
        by_col
            .iter()
            .take(6)
            .map(|(n, s, r, nb)| format!("{n}={}KiB(raw {}KiB,{nb}mb)", s / 1024, r / 1024))
            .collect::<Vec<_>>()
    );
    if let Some(pb) = pack_bytes {
        let pack = ChunkPackReader::open(Arc::new(SliceReader::new(pb.clone())))?;
        let ps = pack.postscript();
        let frame_stored: u64 = pack
            .toc()
            .frames
            .iter()
            .map(|f| u64::from(f.stored_len))
            .sum();
        let frame_raw: u64 = pack.toc().frames.iter().map(|f| u64::from(f.raw_len)).sum();
        let hash_bytes = u64::from(ps.chunk_count) * 16;
        eprintln!(
            "[STATAM_STATS] .stpk={} KiB | frames={} chunks={} | frame_data={} KiB (raw {} KiB) | hash_tbl={} KiB | edge_tbl={} KiB | trigram={} KiB | dict={} KiB | toc={} KiB",
            pb.len() / 1024,
            ps.frame_count,
            ps.chunk_count,
            frame_stored / 1024,
            frame_raw / 1024,
            hash_bytes / 1024,
            ps.edge_table_len / 1024,
            ps.trigram_len / 1024,
            ps.dict_len / 1024,
            ps.toc_stored_len / 1024,
        );
    }
    Ok(())
}

async fn try_write_lance(
    wl: &str,
    suite_label: &str,
    batch: &RecordBatch,
    raw_bytes: u64,
    write_rows: &mut Vec<WriteBenchRow>,
    notes: &mut Vec<String>,
) -> Option<Bytes> {
    let prepared = match prepare_lance_batches(std::slice::from_ref(batch)) {
        Ok(p) => p,
        Err(e) => {
            notes.push(format!("{suite_label}: `lance` = N/A ({e})"));
            return None;
        }
    };
    match write_lance_bytes(&prepared).await {
        Ok((b, ms)) => {
            write_rows.push(mk_write_row(wl, "lance", raw_bytes, b.len() as u64, ms));
            Some(b)
        }
        Err(e) => {
            notes.push(format!("{suite_label}: `lance` = N/A ({e})"));
            None
        }
    }
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

async fn open_single_strata_from_bench_s3(s3: Arc<BenchS3Reader>) -> BenchResult<Arc<StrataFile>> {
    let path = ObjectPath::from("part_000.strata");
    let mut map = HashMap::with_capacity(1);
    map.insert(path.clone(), s3);
    let store: Arc<dyn ObjectStore> = Arc::new(SimS3ObjectStore::new(map));
    let mut cfg = StrataConfig::default();
    cfg.point_gap = 256 * 1024;
    let f = StrataFile::open(store, path, None, &cfg).await?;
    Ok(Arc::new(f))
}

#[derive(Clone)]
enum GenPqPred {
    I64Eq {
        col: String,
        val: i64,
    },
    I64Ne {
        col: String,
        val: i64,
    },
    I64Between {
        col: String,
        lo: i64,
        hi: i64,
    },
    Utf8Eq {
        col: String,
        val: String,
    },
    Utf8NonEmpty {
        col: String,
    },
    Utf8Like {
        col: String,
        needle: String,
    },
    Utf8Ilike {
        col: String,
        needle_lower: String,
    },
    MapKeyEq {
        col: String,
        key: String,
        val: String,
    },
}

impl GenPqPred {
    fn col_name(&self) -> &str {
        match self {
            Self::I64Eq { col, .. }
            | Self::I64Ne { col, .. }
            | Self::I64Between { col, .. }
            | Self::Utf8Eq { col, .. }
            | Self::Utf8NonEmpty { col }
            | Self::Utf8Like { col, .. }
            | Self::Utf8Ilike { col, .. }
            | Self::MapKeyEq { col, .. } => col.as_str(),
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
            GenPqPred::Utf8Ilike { needle_lower, .. } => {
                let nd = needle_lower.clone();
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
                        .map(|i| {
                            (!arr.is_null(i))
                                .then(|| arr.value(i).to_lowercase().contains(nd.as_str()))
                        })
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::MapKeyEq { key, val, .. } => {
                let target_k = key.clone();
                let target_v = val.clone();
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let ma = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<MapArray>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected MapArray".to_owned(),
                            )
                        })?;
                    let ks = ma
                        .keys()
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected StringArray map keys".to_owned(),
                            )
                        })?;
                    let vs = ma
                        .values()
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected StringArray map values".to_owned(),
                            )
                        })?;
                    let offsets = ma.value_offsets();
                    let bools: BooleanArray = (0..ma.len())
                        .map(|r| {
                            if ma.is_null(r) {
                                return Some(false);
                            }
                            let s = offsets[r] as usize;
                            let e = offsets[r + 1] as usize;
                            let mut hit = false;
                            for j in s..e {
                                if !ks.is_null(j)
                                    && ks.value(j) == target_k.as_str()
                                    && !vs.is_null(j)
                                    && vs.value(j) == target_v.as_str()
                                {
                                    hit = true;
                                    break;
                                }
                            }
                            Some(hit)
                        })
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

async fn execute_gen_parquet_s3(
    reader: &Arc<BenchS3Reader>,
    with_pi: bool,
    proj_roots: &[usize],
    filters: &[GenPqPred],
) -> BenchResult<usize> {
    let mut builder = open_parquet_async_builder(reader, with_pi).await?;
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
                match f {
                    GenPqPred::I64Eq { val, .. } => {
                        if let Some(sbbf) = builder
                            .get_row_group_column_bloom_filter(rg_idx, leaf_idx)
                            .await?
                            && !sbbf.check(val)
                        {
                            bloom_ok = false;
                            break;
                        }
                    }
                    GenPqPred::Utf8Eq { val, .. } => {
                        if let Some(sbbf) = builder
                            .get_row_group_column_bloom_filter(rg_idx, leaf_idx)
                            .await?
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

    let stream = builder.build()?;
    let batches: Vec<RecordBatch> = stream.try_collect().await?;
    Ok(batches.iter().map(RecordBatch::num_rows).sum())
}

#[derive(Clone, Copy)]
enum VortexPostFilter<'a> {
    None,
    Like {
        col: &'a str,
        needle: &'a str,
    },
    Ilike {
        col: &'a str,
        needle_lower: &'a str,
    },
    MapKeyEq {
        col: &'a str,
        key: &'a str,
        val: &'a str,
    },
}

fn eval_vortex_map_key_eq(
    b: &arrow_array_60::RecordBatch,
    col_name: &str,
    key: &str,
    val: &str,
) -> usize {
    let Some(col) = b.column_by_name(col_name) else {
        return 0;
    };
    let mut matched = 0usize;
    if let Some(ma) = col.as_any().downcast_ref::<MapArray60>() {
        let keys = ma.keys();
        let vals = ma.values();
        let offsets = ma.value_offsets();
        for r in 0..Array60::len(ma) {
            if Array60::is_null(ma, r) {
                continue;
            }
            let s = offsets[r] as usize;
            let e = offsets[r + 1] as usize;
            for j in s..e {
                if vortex_str_value_at(keys.as_ref(), j) == Some(key)
                    && vortex_str_value_at(vals.as_ref(), j) == Some(val)
                {
                    matched += 1;
                    break;
                }
            }
        }
    } else if let Some(la) = col.as_any().downcast_ref::<ListArray60>() {
        let entries = la.values().as_struct();
        let keys = entries.column(0);
        let vals = entries.column(1);
        let offsets = la.value_offsets();
        for r in 0..Array60::len(la) {
            if Array60::is_null(la, r) {
                continue;
            }
            let s = offsets[r] as usize;
            let e = offsets[r + 1] as usize;
            for j in s..e {
                if vortex_str_value_at(keys.as_ref(), j) == Some(key)
                    && vortex_str_value_at(vals.as_ref(), j) == Some(val)
                {
                    matched += 1;
                    break;
                }
            }
        }
    }
    matched
}

async fn execute_vortex_scan_with_post(
    session: &VortexSession,
    file: &VortexFile,
    proj_expr: &Expression,
    filter_opt: Option<&Expression>,
    post: VortexPostFilter<'_>,
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
        let batch = chunk_to_record_batch(session, chunk)?;
        match post {
            VortexPostFilter::None => {
                total += batch.num_rows();
            }
            VortexPostFilter::Like { col, needle } => {
                if let Some(arr) = batch.column_by_name(col) {
                    for i in 0..batch.num_rows() {
                        if let Some(s) = vortex_str_value_at(arr.as_ref(), i)
                            && s.contains(needle)
                        {
                            total += 1;
                        }
                    }
                }
            }
            VortexPostFilter::Ilike { col, needle_lower } => {
                if let Some(arr) = batch.column_by_name(col) {
                    for i in 0..batch.num_rows() {
                        if let Some(s) = vortex_str_value_at(arr.as_ref(), i)
                            && s.to_lowercase().contains(needle_lower)
                        {
                            total += 1;
                        }
                    }
                }
            }
            VortexPostFilter::MapKeyEq { col, key, val } => {
                total += eval_vortex_map_key_eq(&batch, col, key, val);
            }
        }
    }
    Ok(total)
}

async fn execute_vortex_scan(
    session: &VortexSession,
    file: &VortexFile,
    proj_expr: &Expression,
    filter_opt: Option<&Expression>,
    like_check: Option<(&str, &str)>,
) -> BenchResult<usize> {
    let post = match like_check {
        Some((col, needle)) => VortexPostFilter::Like { col, needle },
        None => VortexPostFilter::None,
    };
    execute_vortex_scan_with_post(session, file, proj_expr, filter_opt, post).await
}

fn execute_statam_scan(reader: &StatamReader, req: &ScanRequest) -> BenchResult<usize> {
    let batches = reader.scan(req)?;
    Ok(batches.iter().map(Batch::num_rows).sum())
}

fn extract_str_at<'a>(arr: &'a dyn Array, idx: usize) -> Option<&'a str> {
    if arr.is_null(idx) {
        return None;
    }
    if let Some(sa) = arr.as_any().downcast_ref::<StringArray>() {
        Some(sa.value(idx))
    } else if let Some(sa) = arr.as_any().downcast_ref::<LargeStringArray>() {
        Some(sa.value(idx))
    } else if let Some(sa) = arr.as_any().downcast_ref::<StringViewArray>() {
        Some(sa.value(idx))
    } else {
        None
    }
}

fn extract_ts_ns_at(arr: &dyn Array, idx: usize) -> i64 {
    if arr.is_null(idx) {
        return 0;
    }
    if let Some(a) = arr.as_any().downcast_ref::<TimestampNanosecondArray>() {
        a.value(idx)
    } else if let Some(a) = arr.as_any().downcast_ref::<TimestampMicrosecondArray>() {
        a.value(idx).saturating_mul(1_000)
    } else if let Some(a) = arr.as_any().downcast_ref::<TimestampMillisecondArray>() {
        a.value(idx).saturating_mul(1_000_000)
    } else if let Some(a) = arr.as_any().downcast_ref::<TimestampSecondArray>() {
        a.value(idx).saturating_mul(1_000_000_000)
    } else if let Some(a) = arr.as_any().downcast_ref::<Int64Array>() {
        a.value(idx)
    } else {
        0
    }
}

fn eval_arrow_map_key_eq(batch: &RecordBatch, col_name: &str, key: &str, val: &str) -> usize {
    let Some(ma) = batch
        .column_by_name(col_name)
        .and_then(|c| c.as_any().downcast_ref::<MapArray>())
    else {
        return 0;
    };
    let keys = ma.keys();
    let vals = ma.values();
    let offsets = ma.value_offsets();
    let mut matched = 0usize;
    for r in 0..ma.len() {
        if ma.is_null(r) {
            continue;
        }
        let s = offsets[r] as usize;
        let e = offsets[r + 1] as usize;
        for j in s..e {
            if extract_str_at(keys.as_ref(), j) == Some(key)
                && extract_str_at(vals.as_ref(), j) == Some(val)
            {
                matched += 1;
                break;
            }
        }
    }
    matched
}

#[allow(clippy::too_many_arguments)]
async fn run_parquet_bench_query(
    wl: &str,
    qname: &str,
    pq_default: &Option<Bytes>,
    pq_tuned: &Option<Bytes>,
    pq_cetra: &Option<Bytes>,
    pq_proj: &[usize],
    pq_preds: &[GenPqPred],
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
) -> BenchResult<()> {
    let s3_only = s3_only_mode();
    for (fmt, bytes_opt, with_pi) in [
        ("pq_default", pq_default, false),
        ("pq_tuned", pq_tuned, true),
        ("pq_cetra", pq_cetra, true),
    ] {
        let Some(bytes) = bytes_opt else { continue };
        let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
            (None, 0.0, 0.0)
        } else {
            let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                PageIndexPolicy::Optional
            } else {
                PageIndexPolicy::Skip
            });
            let meta = ArrowReaderMetadata::load(bytes, opts)?;
            let (m, med, p95) = measure_local_ms(
                || execute_gen_parquet_local(bytes, &meta, pq_proj, pq_preds),
                warm_runs,
            )?;
            (Some(m), med, p95)
        };

        let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
        let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
            let s3_reader = Arc::clone(&s3_reader);
            async move {
                s3_reader.evict_all();
                s3_reader.reset_counters();
                let m = execute_gen_parquet_s3(&s3_reader, with_pi, pq_proj, pq_preds).await?;
                Ok((
                    m,
                    s3_reader.counters().gets,
                    s3_reader.counters().bytes_from_s3,
                    s3_reader.simulated_latency_ms(),
                ))
            }
        })
        .await?;
        if let Some(lm) = local_matched_opt
            && s3_matched != lm
        {
            return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
        }
        let matched = s3_matched;
        check_parity(expected_matches, wl, qname, fmt, matched)?;
        query_rows.push(QueryBenchRow {
            workload: wl.to_owned(),
            query: qname.to_owned(),
            format: fmt.to_owned(),
            rows_matched: matched,
            local_ms,
            local_p95_ms,
            s3_ms,
            s3_gets,
            s3_bytes,
            cold_open_gets: None,
            cold_open_tail_bytes: None,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_vortex_bench_query(
    wl: &str,
    qname: &str,
    vortex_session: &VortexSession,
    bytes: &Bytes,
    local_file: &VortexFile,
    proj: &Expression,
    filt: Option<&Expression>,
    post: VortexPostFilter<'_>,
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
) -> BenchResult<()> {
    let (local_matched_opt, local_ms, local_p95_ms) = if s3_only_mode() {
        (None, 0.0, 0.0)
    } else {
        let (m, med, p95) = measure_local_async_ms(
            || execute_vortex_scan_with_post(vortex_session, local_file, proj, filt, post),
            warm_runs,
        )
        .await?;
        (Some(m), med, p95)
    };

    let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
    let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
        let s3_reader = Arc::clone(&s3_reader);
        async move {
            s3_reader.evict_all();
            s3_reader.reset_counters();
            let vx_read = Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
            let s3_file = vortex_session.open_options().open_read(vx_read).await?;
            let m =
                execute_vortex_scan_with_post(vortex_session, &s3_file, proj, filt, post).await?;
            Ok((
                m,
                s3_reader.counters().gets,
                s3_reader.counters().bytes_from_s3,
                s3_reader.simulated_latency_ms(),
            ))
        }
    })
    .await?;
    if let Some(lm) = local_matched_opt
        && s3_matched != lm
    {
        return Err(format!("vortex {qname}: s3 {s3_matched} != local {lm}").into());
    }
    let matched = s3_matched;
    check_parity(expected_matches, wl, qname, "vortex", matched)?;
    query_rows.push(QueryBenchRow {
        workload: wl.to_owned(),
        query: qname.to_owned(),
        format: "vortex".to_owned(),
        rows_matched: matched,
        local_ms,
        local_p95_ms,
        s3_ms,
        s3_gets,
        s3_bytes,
        cold_open_gets: None,
        cold_open_tail_bytes: None,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_lance_bench_query<F>(
    wl: &str,
    qname: &str,
    bytes: &Bytes,
    local_reader: &LanceFileReader,
    exec: F,
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
) -> BenchResult<()>
where
    F: AsyncFn(&LanceFileReader) -> BenchResult<usize>,
{
    let (local_matched_opt, local_ms, local_p95_ms) = if s3_only_mode() {
        (None, 0.0, 0.0)
    } else {
        let mut samples = Vec::with_capacity(warm_runs.max(1));
        let mut matched = 0usize;
        let runs = warm_runs.max(1);
        for i in 0..=runs {
            let t0 = Instant::now();
            let n = exec(local_reader).await?;
            let ms = t0.elapsed().as_secs_f64() * 1_000.0;
            if i == 0 {
                matched = n;
            } else {
                samples.push(ms);
            }
        }
        samples.sort_by(f64::total_cmp);
        let local_ms = samples[samples.len() / 2];
        let p95_idx = ((samples.len() as f64 * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(samples.len() - 1);
        let local_p95_ms = samples[p95_idx];
        (Some(matched), local_ms, local_p95_ms)
    };

    let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
    let exec_ref = &exec;
    let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
        let s3_file = Arc::clone(&s3_file);
        async move {
            s3_file.evict_all();
            s3_file.reset_counters();
            let s3_reader = open_lance_from_bench_s3(Arc::clone(&s3_file)).await?;
            let m = exec_ref(&s3_reader).await?;
            Ok((
                m,
                s3_file.counters().gets,
                s3_file.counters().bytes_from_s3,
                s3_file.simulated_latency_ms(),
            ))
        }
    })
    .await?;
    if let Some(lm) = local_matched_opt
        && s3_matched != lm
    {
        return Err(format!("lance {qname}: s3 {s3_matched} != local {lm}").into());
    }
    let matched = s3_matched;
    check_parity(expected_matches, wl, qname, "lance", matched)?;
    query_rows.push(QueryBenchRow {
        workload: wl.to_owned(),
        query: qname.to_owned(),
        format: "lance".to_owned(),
        rows_matched: matched,
        local_ms,
        local_p95_ms,
        s3_ms,
        s3_gets,
        s3_bytes,
        cold_open_gets: None,
        cold_open_tail_bytes: None,
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_statam_bench_query(
    wl: &str,
    qname: &str,
    statam_l0: &Option<Bytes>,
    statam_l1: &Option<Bytes>,
    statam_l1_pack: Option<&Bytes>,
    req: &ScanRequest,
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
) -> BenchResult<()> {
    if let Ok(only) = std::env::var("STATAM_ONLY_QUERY")
        && only != qname
    {
        return Ok(());
    }
    let s3_only = s3_only_mode();
    for (fmt, bytes_opt, pack_opt) in [
        ("statam_l0", statam_l0, None),
        ("statam_l1", statam_l1, statam_l1_pack),
    ] {
        let Some(bytes) = bytes_opt else { continue };
        let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
            (None, 0.0, 0.0)
        } else {
            let warm_pack: Option<Arc<dyn PackResolver>> = if let Some(pb) = pack_opt {
                let mut res = InMemoryPackResolver::new();
                res.insert_bytes(PACK_ID, pb.clone())?;
                Some(Arc::new(res))
            } else {
                None
            };
            let local_reader =
                StatamReader::open(Arc::new(SliceReader::new(bytes.clone())), warm_pack)?;
            let (m, med, p95) =
                measure_local_ms(|| execute_statam_scan(&local_reader, req), warm_runs)?;
            (Some(m), med, p95)
        };

        let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
        let s3_pack = pack_opt.map(|pb| BenchS3Reader::from_bytes(pb.clone(), s3_cfg));
        let s3_resolver = s3_pack
            .as_ref()
            .map(|ps3| Arc::new(SimS3PackResolver::new(PACK_ID, Arc::clone(ps3))));

        let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
            let s3_file = Arc::clone(&s3_file);
            let s3_pack = s3_pack.clone();
            let s3_resolver = s3_resolver.clone();
            async move {
                s3_file.evict_all();
                s3_file.reset_counters();
                if let Some(ps3) = &s3_pack {
                    ps3.evict_all();
                    ps3.reset_counters();
                }
                if let Some(res) = &s3_resolver {
                    res.reset_opened();
                }
                let pack_dyn = s3_resolver
                    .as_ref()
                    .map(|r| Arc::clone(r) as Arc<dyn PackResolver>);
                let s3_reader =
                    StatamReader::open(Arc::clone(&s3_file) as Arc<dyn RangeReader>, pack_dyn)?;
                let m = execute_statam_scan(&s3_reader, req)?;
                let mut gets = s3_file.counters().gets;
                let mut s3_bytes = s3_file.counters().bytes_from_s3;
                let mut s3_ms = s3_file.simulated_latency_ms();
                if let Some(ps3) = &s3_pack {
                    gets += ps3.counters().gets;
                    s3_bytes += ps3.counters().bytes_from_s3;
                    s3_ms += ps3.simulated_latency_ms();
                }
                Ok((m, gets, s3_bytes, s3_ms))
            }
        })
        .await?;
        if let Some(lm) = local_matched_opt
            && s3_matched != lm
        {
            return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
        }
        let matched = s3_matched;

        check_parity(expected_matches, wl, qname, fmt, matched)?;
        query_rows.push(QueryBenchRow {
            workload: wl.to_owned(),
            query: qname.to_owned(),
            format: fmt.to_owned(),
            rows_matched: matched,
            local_ms,
            local_p95_ms,
            s3_ms,
            s3_gets,
            s3_bytes,
            cold_open_gets: None,
            cold_open_tail_bytes: None,
        });
    }
    Ok(())
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
    let lance_bytes = if enabled.contains(&"lance") {
        try_write_lance(
            wl,
            "Suite A (`clickbench`)",
            &norm_batch,
            raw_bytes,
            write_rows,
            notes,
        )
        .await
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
        let (b, _, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L0Fast,
            &[],
            &bloom_names,
            Some("UserID"),
            &[],
            &[],
            false,
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let statam_l1 = if enabled.contains(&"statam_l1") {
        let (b, _, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L1Compact,
            &[],
            &bloom_names,
            Some("UserID"),
            &["Title", "URL", "SearchPhrase"],
            &[],
            false,
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
    let lance_file_local = match &lance_bytes {
        Some(b) => Some(open_lance_local(b).await?),
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

        let s3_only = s3_only_mode();
        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &pq_default, false),
            ("pq_tuned", &pq_tuned, true),
            ("pq_cetra", &pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                    PageIndexPolicy::Optional
                } else {
                    PageIndexPolicy::Skip
                });
                let meta = ArrowReaderMetadata::load(bytes, opts)?;
                let (m, med, p95) = measure_local_ms(
                    || execute_gen_parquet_local(bytes, &meta, &pq_proj, &pq_preds),
                    warm_runs,
                )?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                let pq_proj = &pq_proj;
                let pq_preds = &pq_preds;
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let m = execute_gen_parquet_s3(&s3_reader, with_pi, pq_proj, pq_preds).await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let (proj, filt, like) = clickbench_vortex_spec(qname, &params);
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) = measure_local_async_ms(
                    || {
                        execute_vortex_scan(
                            vortex_session,
                            &local_files[0],
                            &proj,
                            filt.as_ref(),
                            like,
                        )
                    },
                    warm_runs,
                )
                .await?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                let proj = &proj;
                let filt = filt.as_ref();
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let vx_read = Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
                    let s3_file = vortex_session.open_options().open_read(vx_read).await?;
                    let m = execute_vortex_scan(vortex_session, &s3_file, proj, filt, like).await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("vortex {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, "vortex", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_reader)) = (&lance_bytes, &lance_file_local) {
            run_lance_bench_query(
                wl,
                qname,
                bytes,
                local_reader,
                async |r: &LanceFileReader| execute_clickbench_lance(r, qname, &params).await,
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
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

            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) = measure_local_async_ms(
                    || execute_clickbench_strata(local_file, qname, &params, use_contains),
                    warm_runs,
                )
                .await?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                let params = &params;
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let s3_file = open_single_strata_from_bench_s3(Arc::clone(&s3_reader)).await?;
                    let m =
                        execute_clickbench_strata(&s3_file, qname, params, use_contains).await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("strata {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, "strata", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "strata".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let Ok(only) = std::env::var("STATAM_ONLY_QUERY")
            && only != qname
        {
            continue;
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
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) =
                    measure_local_ms(|| execute_statam_scan(local_reader, &req), warm_runs)?;
                (Some(m), med, p95)
            };

            let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_file = Arc::clone(&s3_file);
                let req = &req;
                async move {
                    s3_file.evict_all();
                    s3_file.reset_counters();
                    let s3_reader =
                        StatamReader::open(Arc::clone(&s3_file) as Arc<dyn RangeReader>, None)?;
                    let m = execute_statam_scan(&s3_reader, req)?;
                    Ok((
                        m,
                        s3_file.counters().gets,
                        s3_file.counters().bytes_from_s3,
                        s3_file.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
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

async fn execute_clickbench_lance(
    reader: &LanceFileReader,
    qname: &str,
    p: &ClickBenchParams,
) -> BenchResult<usize> {
    let proj_cols: Option<&[&str]> = match qname {
        "CB_Q1" => Some(&["AdvEngineID"]),
        "CB_Q10" => Some(&["MobilePhoneModel", "UserID"]),
        "CB_Q12" => Some(&["SearchPhrase", "UserID"]),
        "CB_Q20" => Some(&["UserID", "EventTime", "URL", "Title"]),
        "CB_Q23_like" => Some(&["Title", "URL"]),
        "CB_Q24" => Some(&["URL"]),
        "CB_Q39_range" => Some(&["CounterID", "EventTime", "UserID"]),
        "CB_wide_point" => None,
        _ => return Ok(0),
    };
    let batches = lance_scan_projected(reader, proj_cols).await?;
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
                if let Some(arr) = b.column_by_name(col_name) {
                    for i in 0..b.num_rows() {
                        if let Some(s) = extract_str_at(arr.as_ref(), i)
                            && !s.is_empty()
                        {
                            matched += 1;
                        }
                    }
                }
            }
            "CB_Q20" | "CB_wide_point" => {
                if let Some(arr) = b
                    .column_by_name("UserID")
                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) && arr.value(i) == p.first_nonzero_uid {
                            matched += 1;
                        }
                    }
                }
            }
            "CB_Q23_like" => {
                if let Some(arr) = b.column_by_name(p.like_col) {
                    for i in 0..b.num_rows() {
                        if let Some(s) = extract_str_at(arr.as_ref(), i)
                            && s.contains(p.like_needle)
                        {
                            matched += 1;
                        }
                    }
                }
            }
            "CB_Q39_range" => {
                let cid = b
                    .column_by_name("CounterID")
                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
                let et = b
                    .column_by_name("EventTime")
                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
                if let (Some(cid), Some(et)) = (cid, et) {
                    for i in 0..b.num_rows() {
                        if !cid.is_null(i)
                            && cid.value(i) == p.most_freq_cid
                            && !et.is_null(i)
                            && et.value(i) >= p.et_lo
                            && et.value(i) <= p.et_hi
                        {
                            matched += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(matched)
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
    let lance_bytes = if enabled.contains(&"lance") {
        try_write_lance(
            wl,
            "Suite B (`row-filter`)",
            &batch,
            raw_bytes,
            write_rows,
            notes,
        )
        .await
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
        let (b, _, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L0Fast,
            &[],
            &[],
            None,
            &[],
            &[],
            false,
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let statam_l1 = if enabled.contains(&"statam_l1") {
        let (b, _, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L1Compact,
            &[],
            &[],
            None,
            &[],
            &[],
            false,
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
    let lance_file_local = match &lance_bytes {
        Some(b) => Some(open_lance_local(b).await?),
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

        let s3_only = s3_only_mode();
        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &pq_default, false),
            ("pq_tuned", &pq_tuned, true),
            ("pq_cetra", &pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                    PageIndexPolicy::Optional
                } else {
                    PageIndexPolicy::Skip
                });
                let meta = ArrowReaderMetadata::load(bytes, opts)?;
                let (m, med, p95) = measure_local_ms(
                    || execute_gen_parquet_local(bytes, &meta, &proj_cols, &pq_preds),
                    warm_runs,
                )?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                let pq_preds = &pq_preds;
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let m =
                        execute_gen_parquet_s3(&s3_reader, with_pi, &proj_cols, pq_preds).await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let proj = select(["int64", "float64", "utf8", "ts"], root());
            let filt = eq(col(filter_col), lit(filter_val));
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) = measure_local_async_ms(
                    || {
                        execute_vortex_scan(
                            vortex_session,
                            &local_files[0],
                            &proj,
                            Some(&filt),
                            None,
                        )
                    },
                    warm_runs,
                )
                .await?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                let proj = &proj;
                let filt = &filt;
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let vx_read = Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
                    let s3_file = vortex_session.open_options().open_read(vx_read).await?;
                    let m = execute_vortex_scan(vortex_session, &s3_file, proj, Some(filt), None)
                        .await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("vortex {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, "vortex", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_reader)) = (&lance_bytes, &lance_file_local) {
            let proj = ["int64", "float64", "utf8", "ts", filter_col];
            run_lance_bench_query(
                wl,
                qname,
                bytes,
                local_reader,
                async |r: &LanceFileReader| {
                    let batches = lance_scan_projected(r, Some(&proj)).await?;
                    let mut matched = 0usize;
                    for b in &batches {
                        if let Some(arr) = b.column_by_name(filter_col) {
                            for i in 0..b.num_rows() {
                                if extract_str_at(arr.as_ref(), i) == Some(filter_val) {
                                    matched += 1;
                                }
                            }
                        }
                    }
                    Ok(matched)
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
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
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) =
                    measure_local_async_ms(|| run_strata(Arc::clone(local_file)), warm_runs)
                        .await?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let run_strata_ref = &run_strata;
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let s3_file = open_single_strata_from_bench_s3(Arc::clone(&s3_reader)).await?;
                    let m = run_strata_ref(s3_file).await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("strata {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, "strata", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "strata".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let Ok(only) = std::env::var("STATAM_ONLY_QUERY")
            && only != qname
        {
            continue;
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
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) =
                    measure_local_ms(|| execute_statam_scan(local_reader, &req), warm_runs)?;
                (Some(m), med, p95)
            };

            let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_file = Arc::clone(&s3_file);
                let req = &req;
                async move {
                    s3_file.evict_all();
                    s3_file.reset_counters();
                    let s3_reader =
                        StatamReader::open(Arc::clone(&s3_file) as Arc<dyn RangeReader>, None)?;
                    let m = execute_statam_scan(&s3_reader, req)?;
                    Ok((
                        m,
                        s3_file.counters().gets,
                        s3_file.counters().bytes_from_s3,
                        s3_file.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
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

async fn execute_parquet_row_selection_async(
    mut builder: ParquetRecordBatchStreamBuilder<BenchS3ParquetAsyncReader>,
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

    let stream = builder.build()?;
    let batches: Vec<RecordBatch> = stream.try_collect().await?;
    Ok(batches.iter().map(RecordBatch::num_rows).sum())
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
    let lance_bytes = if enabled.contains(&"lance") {
        try_write_lance(
            wl,
            "Suite C (`random-access`)",
            &batch,
            raw_bytes,
            write_rows,
            notes,
        )
        .await
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
        let (b, _, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L0Fast,
            &[0],
            &["row_id"],
            Some("row_id"),
            &[],
            &[],
            false,
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let statam_l1 = if enabled.contains(&"statam_l1") {
        let (b, _, ms) = write_generic_statam(
            &cetra_batch,
            WriteProfile::L1Compact,
            &[0],
            &["row_id"],
            Some("row_id"),
            &[],
            &[],
            false,
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
    let lance_file_local = match &lance_bytes {
        Some(b) => Some(open_lance_local(b).await?),
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

        let s3_only = s3_only_mode();
        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &pq_default, false),
            ("pq_tuned", &pq_tuned, true),
            ("pq_cetra", &pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                    PageIndexPolicy::Optional
                } else {
                    PageIndexPolicy::Skip
                });
                let meta = ArrowReaderMetadata::load(bytes, opts)?;
                let (m, med, p95) = measure_local_ms(
                    || {
                        let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(
                            bytes.clone(),
                            meta.clone(),
                        );
                        execute_parquet_row_selection(builder, indices)
                    },
                    warm_runs,
                )?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let s3_builder = open_parquet_async_builder(&s3_reader, with_pi).await?;
                    let m = execute_parquet_row_selection_async(s3_builder, indices).await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) = measure_local_async_ms(
                    || execute_vortex_take(vortex_session, &local_files[0], indices),
                    warm_runs,
                )
                .await?;
                (Some(m), med, p95)
            };

            let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_reader = Arc::clone(&s3_reader);
                async move {
                    s3_reader.evict_all();
                    s3_reader.reset_counters();
                    let vx_read = Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
                    let s3_file = vortex_session.open_options().open_read(vx_read).await?;
                    let m = execute_vortex_take(vortex_session, &s3_file, indices).await?;
                    Ok((
                        m,
                        s3_reader.counters().gets,
                        s3_reader.counters().bytes_from_s3,
                        s3_reader.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("vortex {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, "vortex", matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }

        if let (Some(bytes), Some(local_reader)) = (&lance_bytes, &lance_file_local) {
            let u32_indices: Vec<u32> = indices.iter().map(|&i| i as u32).collect();
            let u32_ref = u32_indices.as_slice();
            run_lance_bench_query(
                wl,
                qname,
                bytes,
                local_reader,
                async |r: &LanceFileReader| {
                    let batches = lance_take_indices(r, u32_ref, None).await?;
                    Ok(batches.iter().map(RecordBatch::num_rows).sum())
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        if let Ok(only) = std::env::var("STATAM_ONLY_QUERY")
            && only != qname
        {
            continue;
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
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) = measure_local_ms(|| execute_take(local_reader), warm_runs)?;
                (Some(m), med, p95)
            };

            let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
            let execute_take_ref = &execute_take;
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_file = Arc::clone(&s3_file);
                async move {
                    s3_file.evict_all();
                    s3_file.reset_counters();
                    let s3_reader =
                        StatamReader::open(Arc::clone(&s3_file) as Arc<dyn RangeReader>, None)?;
                    let m = execute_take_ref(&s3_reader)?;
                    Ok((
                        m,
                        s3_file.counters().gets,
                        s3_file.counters().bytes_from_s3,
                        s3_file.simulated_latency_ms(),
                    ))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            let matched = s3_matched;
            check_parity(&mut expected_matches, wl, qname, fmt, matched)?;
            query_rows.push(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            });
        }
    }

    Ok(())
}

// ============================================================================
// Shared Helpers for Real-World Suites (D, E, F)
// ============================================================================

fn col_utf8_from_strings(vals: &[String]) -> BenchResult<Column> {
    let data = Utf8Data::try_from_slices(vals.iter().map(String::as_str))?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(data),
        None,
    )?))
}

fn col_i64_from_slice(vals: &[i64]) -> BenchResult<Column> {
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::I64(vals.to_vec()),
        None,
    )?))
}

fn col_ts_ns_from_slice(vals: &[i64]) -> BenchResult<Column> {
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs {
            values: vals.to_vec(),
            utc: true,
        },
        None,
    )?))
}

fn col_map_utf8_from_btreemaps(maps: &[BTreeMap<String, Option<String>>]) -> BenchResult<Column> {
    let mut offsets = vec![0u32];
    let mut keys: Vec<&str> = Vec::new();
    let mut vals: Vec<&str> = Vec::new();
    let mut valid: Vec<bool> = Vec::new();
    let mut any_null = false;
    for m in maps {
        for (k, v) in m {
            keys.push(k.as_str());
            match v {
                Some(s) => {
                    vals.push(s.as_str());
                    valid.push(true);
                }
                None => {
                    vals.push("");
                    valid.push(false);
                    any_null = true;
                }
            }
        }
        offsets.push(u32::try_from(keys.len())?);
    }
    let kd = Utf8Data::try_from_slices(keys)?;
    let vd = Utf8Data::try_from_slices(vals)?;
    let vnulls = any_null.then(|| Bitmap::from_bools(&valid));
    let md = MapData::try_new(offsets, kd, vd, vnulls)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(md)),
        None,
    )?))
}

fn extract_list_parts(arr: &dyn Array) -> BenchResult<(Vec<usize>, &StructArray)> {
    if let Some(la) = arr.as_any().downcast_ref::<ListArray>() {
        let offsets: Vec<usize> = la.offsets().iter().map(|&o| o as usize).collect();
        let sa = la
            .values()
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or("expected StructArray inside ListArray")?;
        Ok((offsets, sa))
    } else if let Some(la) = arr.as_any().downcast_ref::<LargeListArray>() {
        let offsets: Vec<usize> = la.offsets().iter().map(|&o| o as usize).collect();
        let sa = la
            .values()
            .as_any()
            .downcast_ref::<StructArray>()
            .ok_or("expected StructArray inside LargeListArray")?;
        Ok((offsets, sa))
    } else {
        Err(format!(
            "expected ListArray or LargeListArray, got {}",
            arr.data_type()
        )
        .into())
    }
}

fn pick_frequent_string(vals: &[String], exclude_ci: Option<&str>) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for v in vals {
        if v.is_empty() {
            continue;
        }
        if let Some(ex) = exclude_ci
            && v.eq_ignore_ascii_case(ex)
        {
            continue;
        }
        *counts.entry(v.as_str()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by(|(k1, c1), (k2, c2)| c1.cmp(c2).then_with(|| k2.cmp(k1)))
        .map_or_else(String::new, |(k, _)| k.to_owned())
}

fn pick_ascii_needle_in_range(
    vals: &[String],
    preferred: Option<&str>,
    min_hits: usize,
    max_hits: usize,
) -> String {
    if let Some(pref) = preferred {
        let hits = vals.iter().filter(|s| s.contains(pref)).count();
        if hits > 0 {
            return pref.to_owned();
        }
    }
    let mut doc_freq: BTreeMap<String, usize> = BTreeMap::new();
    for s in vals.iter().take(5_000) {
        if s.is_empty() {
            continue;
        }
        let mut seen = BTreeSet::new();
        for tok in s.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if tok.len() >= 4 && tok.len() <= 20 && seen.insert(tok) {
                *doc_freq.entry(tok.to_owned()).or_insert(0) += 1;
            }
            if seen.len() >= 24 {
                break;
            }
        }
    }
    if let Some((tok, _)) = doc_freq
        .iter()
        .filter(|&(_, &c)| c >= min_hits && c <= max_hits)
        .min_by(|(k1, c1), (k2, c2)| c1.cmp(c2).then_with(|| k1.cmp(k2)))
    {
        return tok.clone();
    }
    if let Some((tok, _)) = doc_freq
        .iter()
        .filter(|&(_, &c)| c >= 1)
        .min_by(|(k1, c1), (k2, c2)| c1.cmp(c2).then_with(|| k1.cmp(k2)))
    {
        return tok.clone();
    }
    "function".to_owned()
}

fn pick_unicode_ilike_needle(vals: &[String]) -> (String, usize) {
    let mut candidates: BTreeMap<String, (usize, bool)> = BTreeMap::new();
    for s in vals {
        if s.is_ascii() {
            continue;
        }
        let mut seen = BTreeSet::new();
        for tok in s.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation()) {
            if tok.is_ascii() || tok.len() < 6 {
                continue;
            }
            let char_len = tok.chars().count();
            if !(3..=10).contains(&char_len) {
                continue;
            }
            let lower = tok.to_lowercase();
            let has_cased_non_ascii = tok.chars().any(|c| {
                !c.is_ascii() && c.to_lowercase().to_string() != c.to_uppercase().to_string()
            });
            if seen.insert(lower.clone()) {
                let entry = candidates
                    .entry(tok.to_owned())
                    .or_insert((0, has_cased_non_ascii));
                entry.0 += 1;
            }
            if seen.len() >= 16 {
                break;
            }
        }
        if candidates.len() >= 2_000 {
            break;
        }
    }

    let mut ranked: Vec<(String, usize, bool)> = candidates
        .into_iter()
        .map(|(tok, (approx, cased))| (tok, approx, cased))
        .collect();
    ranked.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut best_fallback: Option<(String, usize)> = None;
    for (tok, _, _) in ranked.into_iter().take(200) {
        let lower = tok.to_lowercase();
        let exact_hits = vals
            .iter()
            .filter(|s| s.to_lowercase().contains(&lower))
            .count();
        if (3..=200).contains(&exact_hits) {
            return (tok, exact_hits);
        }
        if exact_hits > 0 && best_fallback.as_ref().is_none_or(|(_, c)| exact_hits < *c) {
            best_fallback = Some((tok, exact_hits));
        }
    }

    if let Some(best) = best_fallback {
        return best;
    }

    for fallback in ["привет", "café", "模型"] {
        let lower = fallback.to_lowercase();
        let hits = vals
            .iter()
            .filter(|s| s.to_lowercase().contains(&lower))
            .count();
        if hits > 0 {
            return (fallback.to_owned(), hits);
        }
    }

    for s in vals {
        if let Some(ch) = s.chars().find(|c| !c.is_ascii() && !c.is_whitespace()) {
            let tok = ch.to_string();
            let lower = tok.to_lowercase();
            let hits = vals
                .iter()
                .filter(|x| x.to_lowercase().contains(&lower))
                .count();
            if hits > 0 {
                return (tok, hits);
            }
        }
    }

    ("café".to_owned(), 0)
}

#[allow(clippy::too_many_arguments)]
async fn run_strata_bench_query<F, Fut>(
    wl: &str,
    qname: &str,
    bytes: &Bytes,
    local_file: &Arc<StrataFile>,
    exec: F,
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
) -> BenchResult<()>
where
    F: Fn(Arc<StrataFile>) -> Fut,
    Fut: std::future::Future<Output = BenchResult<usize>>,
{
    let (local_matched_opt, local_ms, local_p95_ms) = if s3_only_mode() {
        (None, 0.0, 0.0)
    } else {
        let (m, med, p95) =
            measure_local_async_ms(|| exec(Arc::clone(local_file)), warm_runs).await?;
        (Some(m), med, p95)
    };

    let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
    let exec_ref = &exec;
    let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
        let s3_reader = Arc::clone(&s3_reader);
        async move {
            s3_reader.evict_all();
            s3_reader.reset_counters();
            let s3_file = open_single_strata_from_bench_s3(Arc::clone(&s3_reader)).await?;
            let m = exec_ref(s3_file).await?;
            Ok((
                m,
                s3_reader.counters().gets,
                s3_reader.counters().bytes_from_s3,
                s3_reader.simulated_latency_ms(),
            ))
        }
    })
    .await?;
    if let Some(lm) = local_matched_opt
        && s3_matched != lm
    {
        return Err(format!("strata {qname}: s3 {s3_matched} != local {lm}").into());
    }
    let matched = s3_matched;
    check_parity(expected_matches, wl, qname, "strata", matched)?;
    query_rows.push(QueryBenchRow {
        workload: wl.to_owned(),
        query: qname.to_owned(),
        format: "strata".to_owned(),
        rows_matched: matched,
        local_ms,
        local_p95_ms,
        s3_ms,
        s3_gets,
        s3_bytes,
        cold_open_gets: None,
        cold_open_tail_bytes: None,
    });
    Ok(())
}

// ============================================================================
// Suite D: JSONBench (Bluesky Jetstream JSON, 100k events)
// ============================================================================

fn flatten_json_record(
    prefix: &str,
    val: &Value,
    out: &mut BTreeMap<String, Option<String>>,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    match val {
        Value::Object(map) => {
            for (k, v) in map {
                let next_key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_json_record(&next_key, v, out, depth + 1);
            }
        }
        Value::String(s) => {
            if !prefix.is_empty() {
                out.insert(prefix.to_owned(), Some(s.clone()));
            }
        }
        Value::Number(n) => {
            if !prefix.is_empty() {
                out.insert(prefix.to_owned(), Some(n.to_string()));
            }
        }
        Value::Bool(b) => {
            if !prefix.is_empty() {
                out.insert(
                    prefix.to_owned(),
                    Some(if *b { "true" } else { "false" }.to_owned()),
                );
            }
        }
        Value::Null => {
            if !prefix.is_empty() {
                out.insert(prefix.to_owned(), None);
            }
        }
        Value::Array(arr) => {
            if !prefix.is_empty()
                && let Ok(s) = serde_json::to_string(arr)
            {
                out.insert(prefix.to_owned(), Some(s));
            }
        }
    }
}

struct JsonBenchData {
    arrow_batch: RecordBatch,
    cetra_batch: Batch,
    top_did: String,
    like_needle: String,
}

fn load_jsonbench_100k(path: &Path) -> BenchResult<JsonBenchData> {
    let file = File::open(path)?;
    let reader = BufReader::new(GzDecoder::new(file));

    let cap = 100_000usize;
    let mut dids = Vec::with_capacity(cap);
    let mut times_us = Vec::with_capacity(cap);
    let mut kinds = Vec::with_capacity(cap);
    let mut operations = Vec::with_capacity(cap);
    let mut collections = Vec::with_capacity(cap);
    let mut rkeys = Vec::with_capacity(cap);
    let mut langs = Vec::with_capacity(cap);
    let mut texts = Vec::with_capacity(cap);
    let mut raw_jsons = Vec::with_capacity(cap);
    let mut attrs_list = Vec::with_capacity(cap);

    for line_res in reader.lines() {
        let line = line_res?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let did = val
            .get("did")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let time_us = val.get("time_us").and_then(Value::as_i64).unwrap_or(0);
        let kind = val
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let commit = val.get("commit");
        let operation = commit
            .and_then(|c| c.get("operation"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let collection = commit
            .and_then(|c| c.get("collection"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let rkey = commit
            .and_then(|c| c.get("rkey"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let record = commit.and_then(|c| c.get("record"));
        let lang = record
            .and_then(|r| r.get("langs"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let text = record
            .and_then(|r| r.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        let mut attrs = BTreeMap::new();
        if let Some(rec) = record {
            flatten_json_record("", rec, &mut attrs, 0);
        }

        dids.push(did);
        times_us.push(time_us);
        kinds.push(kind);
        operations.push(operation);
        collections.push(collection);
        rkeys.push(rkey);
        langs.push(lang);
        texts.push(text);
        raw_jsons.push(trimmed.to_owned());
        attrs_list.push(attrs);

        if dids.len() >= cap {
            break;
        }
    }

    if dids.is_empty() {
        return Err(format!("bluesky JSON '{}' contained 0 valid events", path.display()).into());
    }

    let top_did = pick_frequent_string(&dids, None);
    let like_needle = pick_ascii_needle_in_range(&texts, Some("http"), 5, 5000);

    let mut did_b = StringBuilder::with_capacity(dids.len(), dids.len() * 32);
    let mut kind_b = StringBuilder::with_capacity(kinds.len(), kinds.len() * 8);
    let mut op_b = StringBuilder::with_capacity(operations.len(), operations.len() * 8);
    let mut col_b = StringBuilder::with_capacity(collections.len(), collections.len() * 20);
    let mut rkey_b = StringBuilder::with_capacity(rkeys.len(), rkeys.len() * 14);
    let mut lang_b = StringBuilder::with_capacity(langs.len(), langs.len() * 4);
    let mut text_b = StringBuilder::with_capacity(texts.len(), texts.len() * 48);
    let mut raw_b = StringBuilder::with_capacity(raw_jsons.len(), raw_jsons.len() * 256);
    let mut map_b = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new())
        .with_values_field(Field::new("values", DataType::Utf8, true));

    for i in 0..dids.len() {
        did_b.append_value(&dids[i]);
        kind_b.append_value(&kinds[i]);
        op_b.append_value(&operations[i]);
        col_b.append_value(&collections[i]);
        rkey_b.append_value(&rkeys[i]);
        lang_b.append_value(&langs[i]);
        text_b.append_value(&texts[i]);
        raw_b.append_value(&raw_jsons[i]);
        for (k, v) in &attrs_list[i] {
            map_b.keys().append_value(k);
            match v {
                Some(s) => map_b.values().append_value(s),
                None => map_b.values().append_null(),
            }
        }
        map_b.append(true)?;
    }

    let map_entries = Field::new(
        "entries",
        DataType::Struct(Fields::from(vec![
            Field::new("keys", DataType::Utf8, false),
            Field::new("values", DataType::Utf8, true),
        ])),
        false,
    );
    let arrow_schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("did", DataType::Utf8, false),
        Field::new("time_us", DataType::Int64, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("operation", DataType::Utf8, false),
        Field::new("collection", DataType::Utf8, false),
        Field::new("rkey", DataType::Utf8, false),
        Field::new("lang", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        Field::new("raw_json", DataType::Utf8, false),
        Field::new("attrs", DataType::Map(Arc::new(map_entries), false), false),
    ]));
    let arrow_batch = RecordBatch::try_new(
        arrow_schema,
        vec![
            Arc::new(did_b.finish()),
            Arc::new(Int64Array::from(times_us.clone())),
            Arc::new(kind_b.finish()),
            Arc::new(op_b.finish()),
            Arc::new(col_b.finish()),
            Arc::new(rkey_b.finish()),
            Arc::new(lang_b.finish()),
            Arc::new(text_b.finish()),
            Arc::new(raw_b.finish()),
            Arc::new(map_b.finish()),
        ],
    )?;

    let col_schema = ColSchema::try_new_arc(vec![
        ColField::new("did", ColType::Utf8, false),
        ColField::new("time_us", ColType::I64, false),
        ColField::new("kind", ColType::Utf8, false),
        ColField::new("operation", ColType::Utf8, false),
        ColField::new("collection", ColType::Utf8, false),
        ColField::new("rkey", ColType::Utf8, false),
        ColField::new("lang", ColType::Utf8, false),
        ColField::new("text", ColType::Utf8, false),
        ColField::new("raw_json", ColType::Utf8, false),
        ColField::new("attrs", ColType::MapUtf8, false),
    ])?;
    let cetra_batch = Batch::try_new_all(
        col_schema,
        vec![
            col_utf8_from_strings(&dids)?,
            col_i64_from_slice(&times_us)?,
            col_utf8_from_strings(&kinds)?,
            col_utf8_from_strings(&operations)?,
            col_utf8_from_strings(&collections)?,
            col_utf8_from_strings(&rkeys)?,
            col_utf8_from_strings(&langs)?,
            col_utf8_from_strings(&texts)?,
            col_utf8_from_strings(&raw_jsons)?,
            col_map_utf8_from_btreemaps(&attrs_list)?,
        ],
        dids.len(),
    )?;

    Ok(JsonBenchData {
        arrow_batch,
        cetra_batch,
        top_did,
        like_needle,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_jsonbench_suite(
    bluesky_path: &Path,
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    if !bluesky_path.exists() {
        notes.push(format!(
            "Suite D (`jsonbench`): SKIPPED (`{}` not found; pass `--bluesky-json` or set `STATAM_BLUESKY_JSON`)",
            bluesky_path.display()
        ));
        return Ok(());
    }

    let data = load_jsonbench_100k(bluesky_path)?;
    let raw_bytes = data.arrow_batch.get_array_memory_size() as u64;
    let wl = "jsonbench_100k";

    notes.push(format!(
        "Suite D (`jsonbench`): {} rows x {} cols, `top_did={}`, `JB_Q4_text_like.contains(\"{}\")`",
        data.arrow_batch.num_rows(),
        data.arrow_batch.num_columns(),
        data.top_did,
        data.like_needle
    ));

    let bloom_names = ["did"];
    let pq_default = if enabled.contains(&"pq_default") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Default,
            &data.arrow_batch,
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
            &data.arrow_batch,
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
            &data.arrow_batch,
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
        match write_vortex_files(vortex_session, &[data.arrow_batch.clone()]).await {
            Ok((files, ms)) => {
                let b = files.into_iter().next().ok_or("missing vortex output")?;
                write_rows.push(mk_write_row(wl, "vortex", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite D (`jsonbench`): `vortex` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let lance_bytes = if enabled.contains(&"lance") {
        try_write_lance(
            wl,
            "Suite D (`jsonbench`)",
            &data.arrow_batch,
            raw_bytes,
            write_rows,
            notes,
        )
        .await
    } else {
        None
    };
    let strata_bytes = if enabled.contains(&"strata") {
        match write_generic_strata(&data.arrow_batch, &[], &bloom_names) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(wl, "strata", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite D (`jsonbench`): `strata` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let statam_l0 = if enabled.contains(&"statam_l0") {
        let (b, _, ms) = write_generic_statam(
            &data.cetra_batch,
            WriteProfile::L0Fast,
            &[],
            &bloom_names,
            Some("did"),
            &[],
            &["raw_json"],
            false,
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let (statam_l1, statam_l1_pack) = if enabled.contains(&"statam_l1") {
        let (b, pack, ms) = write_generic_statam(
            &data.cetra_batch,
            WriteProfile::L1Compact,
            &[],
            &bloom_names,
            Some("did"),
            &["text"],
            &["raw_json"],
            true,
            write_threads,
        )?;
        let stored = b.len() as u64 + pack.as_ref().map_or(0, |p| p.len() as u64);
        write_rows.push(mk_write_row(wl, "statam_l1", raw_bytes, stored, ms));
        (Some(b), pack)
    } else {
        (None, None)
    };

    let s3_cfg = SimulatedS3Config::default();
    let vortex_file_local = match &vortex_bytes {
        Some(b) => Some(open_vortex_local(vortex_session, &[b.clone()]).await?),
        None => None,
    };
    let lance_file_local = match &lance_bytes {
        Some(b) => Some(open_lance_local(b).await?),
        None => None,
    };
    let strata_file_local = match &strata_bytes {
        Some(b) => Some(open_single_strata_local(b).await?),
        None => None,
    };

    let queries = [
        "JB_Q1_posts",
        "JB_Q2_did_pt",
        "JB_Q3_map_type",
        "JB_Q4_text_like",
        "JB_Q5_did_full",
    ];
    for &qname in &queries {
        let mut expected_matches: Option<usize> = None;

        let (pq_proj, pq_preds): (Vec<usize>, Vec<GenPqPred>) = match qname {
            "JB_Q1_posts" => (
                vec![4, 3],
                vec![
                    GenPqPred::Utf8Eq {
                        col: "collection".to_owned(),
                        val: "app.bsky.feed.post".to_owned(),
                    },
                    GenPqPred::Utf8Eq {
                        col: "operation".to_owned(),
                        val: "create".to_owned(),
                    },
                ],
            ),
            "JB_Q2_did_pt" => (
                vec![0, 1, 4, 7],
                vec![GenPqPred::Utf8Eq {
                    col: "did".to_owned(),
                    val: data.top_did.clone(),
                }],
            ),
            "JB_Q3_map_type" => (
                vec![0, 1, 9],
                vec![GenPqPred::MapKeyEq {
                    col: "attrs".to_owned(),
                    key: "$type".to_owned(),
                    val: "app.bsky.feed.post".to_owned(),
                }],
            ),
            "JB_Q4_text_like" => (
                vec![0, 7],
                vec![GenPqPred::Utf8Like {
                    col: "text".to_owned(),
                    needle: data.like_needle.clone(),
                }],
            ),
            "JB_Q5_did_full" => (
                (0..10).collect(),
                vec![GenPqPred::Utf8Eq {
                    col: "did".to_owned(),
                    val: data.top_did.clone(),
                }],
            ),
            _ => (vec![], vec![]),
        };
        run_parquet_bench_query(
            wl,
            qname,
            &pq_default,
            &pq_tuned,
            &pq_cetra,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected_matches,
            query_rows,
        )
        .await?;

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let (proj, filt, post): (Expression, Option<Expression>, VortexPostFilter<'_>) =
                match qname {
                    "JB_Q1_posts" => (
                        select(["collection", "operation"], root()),
                        Some(and(
                            eq(col("collection"), lit("app.bsky.feed.post")),
                            eq(col("operation"), lit("create")),
                        )),
                        VortexPostFilter::None,
                    ),
                    "JB_Q2_did_pt" => (
                        select(["did", "time_us", "collection", "text"], root()),
                        Some(eq(col("did"), lit(data.top_did.as_str()))),
                        VortexPostFilter::None,
                    ),
                    "JB_Q3_map_type" => (
                        select(["did", "time_us", "attrs"], root()),
                        None,
                        VortexPostFilter::MapKeyEq {
                            col: "attrs",
                            key: "$type",
                            val: "app.bsky.feed.post",
                        },
                    ),
                    "JB_Q4_text_like" => (
                        select(["did", "text"], root()),
                        None,
                        VortexPostFilter::Like {
                            col: "text",
                            needle: data.like_needle.as_str(),
                        },
                    ),
                    "JB_Q5_did_full" => (
                        root(),
                        Some(eq(col("did"), lit(data.top_did.as_str()))),
                        VortexPostFilter::None,
                    ),
                    _ => (root(), None, VortexPostFilter::None),
                };
            run_vortex_bench_query(
                wl,
                qname,
                vortex_session,
                bytes,
                &local_files[0],
                &proj,
                filt.as_ref(),
                post,
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_reader)) = (&lance_bytes, &lance_file_local) {
            let top_did = data.top_did.as_str();
            let like_needle = data.like_needle.as_str();
            run_lance_bench_query(
                wl,
                qname,
                bytes,
                local_reader,
                async |r: &LanceFileReader| {
                    let proj_cols: Option<&[&str]> = match qname {
                        "JB_Q1_posts" => Some(&["collection", "operation"]),
                        "JB_Q2_did_pt" => Some(&["did", "time_us", "collection", "text"]),
                        "JB_Q3_map_type" => Some(&["did", "time_us", "attrs"]),
                        "JB_Q4_text_like" => Some(&["did", "text"]),
                        "JB_Q5_did_full" => None,
                        _ => return Ok(0),
                    };
                    let batches = lance_scan_projected(r, proj_cols).await?;
                    let mut matched = 0usize;
                    for b in &batches {
                        match qname {
                            "JB_Q1_posts" => {
                                if let (Some(c), Some(o)) = (
                                    b.column_by_name("collection"),
                                    b.column_by_name("operation"),
                                ) {
                                    for i in 0..b.num_rows() {
                                        if extract_str_at(c.as_ref(), i)
                                            == Some("app.bsky.feed.post")
                                            && extract_str_at(o.as_ref(), i) == Some("create")
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "JB_Q2_did_pt" | "JB_Q5_did_full" => {
                                if let Some(d) = b.column_by_name("did") {
                                    for i in 0..b.num_rows() {
                                        if extract_str_at(d.as_ref(), i) == Some(top_did) {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "JB_Q3_map_type" => {
                                matched += eval_arrow_map_key_eq(
                                    b,
                                    "attrs",
                                    "$type",
                                    "app.bsky.feed.post",
                                );
                            }
                            "JB_Q4_text_like" => {
                                if let Some(t) = b.column_by_name("text") {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(t.as_ref(), i)
                                            && s.contains(like_needle)
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(matched)
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_file)) = (&strata_bytes, &strata_file_local) {
            let use_contains = if qname == "JB_Q4_text_like" {
                let probe = StrataScanRequest {
                    projection: vec![0, 7],
                    predicate: Some(StrataPred::Contains(
                        7,
                        data.like_needle.as_bytes().to_vec(),
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
            let top_did = data.top_did.clone();
            let like_needle = data.like_needle.clone();
            run_strata_bench_query(
                wl,
                qname,
                bytes,
                local_file,
                move |f| {
                    let top_did = top_did.clone();
                    let like_needle = like_needle.clone();
                    async move {
                        let req = match qname {
                            "JB_Q1_posts" => StrataScanRequest {
                                projection: vec![4, 3],
                                predicate: Some(StrataPred::And(vec![
                                    StrataPred::Cmp(
                                        4,
                                        CmpOp::Eq,
                                        Lit::Utf8("app.bsky.feed.post".to_owned()),
                                    ),
                                    StrataPred::Cmp(3, CmpOp::Eq, Lit::Utf8("create".to_owned())),
                                ])),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Scan,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "JB_Q2_did_pt" => StrataScanRequest {
                                projection: vec![0, 1, 4, 7],
                                predicate: Some(StrataPred::Cmp(
                                    0,
                                    CmpOp::Eq,
                                    Lit::Utf8(top_did.clone()),
                                )),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Point,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "JB_Q3_map_type" => StrataScanRequest {
                                projection: vec![0, 1, 9],
                                predicate: Some(StrataPred::MapKeyEq(
                                    9,
                                    "$type".to_owned(),
                                    Lit::Utf8("app.bsky.feed.post".to_owned()),
                                )),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Sparse,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "JB_Q4_text_like" => StrataScanRequest {
                                projection: vec![0, 7],
                                predicate: use_contains.then(|| {
                                    StrataPred::Contains(7, like_needle.as_bytes().to_vec())
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
                            "JB_Q5_did_full" => StrataScanRequest {
                                projection: (0..10).collect(),
                                predicate: Some(StrataPred::Cmp(
                                    0,
                                    CmpOp::Eq,
                                    Lit::Utf8(top_did.clone()),
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
                        let batches = f.scan(&req).await?;
                        let mut matched = 0usize;
                        if qname == "JB_Q4_text_like" && !use_contains {
                            for b in &batches {
                                if let Some(t) = b.column_by_name("text") {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(t.as_ref(), i)
                                            && s.contains(like_needle.as_str())
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                        } else {
                            for b in &batches {
                                matched += b.num_rows();
                            }
                        }
                        Ok(matched)
                    }
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        let mut statam_req = match qname {
            "JB_Q1_posts" => ScanRequest {
                projection: Some(vec![4, 3]),
                predicates: vec![
                    StatamPred::new(
                        "collection",
                        ZonePred::Eq(ZoneValue::Bytes(b"app.bsky.feed.post".to_vec())),
                    ),
                    StatamPred::new(
                        "operation",
                        ZonePred::Eq(ZoneValue::Bytes(b"create".to_vec())),
                    ),
                ],
                ..ScanRequest::default()
            },
            "JB_Q2_did_pt" => ScanRequest {
                projection: Some(vec![0, 1, 4, 7]),
                id_lookups: vec![IdLookup::eq("did", data.top_did.as_bytes().to_vec())],
                coalesce_mode: CoalesceMode::Point,
                ..ScanRequest::default()
            },
            "JB_Q3_map_type" => ScanRequest {
                projection: Some(vec![0, 1]),
                map_keys: vec![("attrs".to_owned(), "$type".to_owned())],
                predicates: vec![StatamPred::map_key(
                    "attrs",
                    "$type",
                    ZonePred::Eq(ZoneValue::Bytes(b"app.bsky.feed.post".to_vec())),
                )],
                ..ScanRequest::default()
            },
            "JB_Q4_text_like" => ScanRequest {
                projection: Some(vec![0, 7]),
                substrings: vec![SubstringQuery::like("text", data.like_needle.as_str())],
                ..ScanRequest::default()
            },
            "JB_Q5_did_full" => ScanRequest {
                projection: None,
                id_lookups: vec![IdLookup::eq("did", data.top_did.as_bytes().to_vec())],
                coalesce_mode: CoalesceMode::Point,
                ..ScanRequest::default()
            },
            _ => ScanRequest::default(),
        };
        statam_req.read_threads = read_threads;
        run_statam_bench_query(
            wl,
            qname,
            &statam_l0,
            &statam_l1,
            statam_l1_pack.as_ref(),
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected_matches,
            query_rows,
        )
        .await?;
    }

    Ok(())
}

// ============================================================================
// Suite E: WildChat-1M (`wildchat_00000.parquet`, 15k multilingual turns + Unicode ILIKE)
// ============================================================================

struct WildChatData {
    arrow_batch: RecordBatch,
    cetra_batch: Batch,
    top_model: String,
    top_non_en_lang: String,
    multi_turn_tid: String,
    unicode_needle: String,
    unicode_needle_hits: usize,
}

fn load_wildchat_15k(path: &Path) -> BenchResult<WildChatData> {
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(4_096)
        .build()?;

    let max_rows = 15_000usize;
    let mut trace_ids = Vec::with_capacity(max_rows);
    let mut start_times = Vec::with_capacity(max_rows);
    let mut models = Vec::with_capacity(max_rows);
    let mut languages = Vec::with_capacity(max_rows);
    let mut countries = Vec::with_capacity(max_rows);
    let mut turn_idxs = Vec::with_capacity(max_rows);
    let mut inputs = Vec::with_capacity(max_rows);
    let mut outputs = Vec::with_capacity(max_rows);

    for batch_res in reader {
        let batch = batch_res?;
        let hash_col = batch
            .column_by_name("conversation_hash")
            .ok_or("wildchat missing conversation_hash")?;
        let model_col = batch
            .column_by_name("model")
            .ok_or("wildchat missing model")?;
        let ts_col = batch
            .column_by_name("timestamp")
            .ok_or("wildchat missing timestamp")?;
        let conv_col = batch
            .column_by_name("conversation")
            .ok_or("wildchat missing conversation")?;
        let row_lang_col = batch.column_by_name("language");
        let row_country_col = batch.column_by_name("country");

        let (offsets, turns_struct) = extract_list_parts(conv_col.as_ref())?;
        let turn_role_col = turns_struct.column_by_name("role");
        let turn_content_col = turns_struct.column_by_name("content");
        let turn_lang_col = turns_struct.column_by_name("language");
        let turn_country_col = turns_struct.column_by_name("country");

        for r in 0..batch.num_rows() {
            if trace_ids.len() >= max_rows {
                break;
            }
            if conv_col.is_null(r) {
                continue;
            }
            let tid = extract_str_at(hash_col.as_ref(), r).unwrap_or("");
            if tid.is_empty() {
                continue;
            }
            let model = extract_str_at(model_col.as_ref(), r).unwrap_or("");
            let ts_ns = extract_ts_ns_at(ts_col.as_ref(), r);
            let row_lang = row_lang_col
                .and_then(|c| extract_str_at(c.as_ref(), r))
                .unwrap_or("");
            let row_country = row_country_col
                .and_then(|c| extract_str_at(c.as_ref(), r))
                .unwrap_or("");

            let s = offsets[r];
            let e = offsets[r + 1];
            let mut prefix_turns: Vec<Value> = Vec::new();
            for (t_idx, elem_idx) in (s..e).enumerate() {
                let role = turn_role_col
                    .and_then(|c| extract_str_at(c.as_ref(), elem_idx))
                    .unwrap_or("");
                let content = turn_content_col
                    .and_then(|c| extract_str_at(c.as_ref(), elem_idx))
                    .unwrap_or("");
                let t_lang = turn_lang_col
                    .and_then(|c| extract_str_at(c.as_ref(), elem_idx))
                    .filter(|x| !x.is_empty())
                    .unwrap_or(row_lang);
                let t_country = turn_country_col
                    .and_then(|c| extract_str_at(c.as_ref(), elem_idx))
                    .filter(|x| !x.is_empty())
                    .unwrap_or(row_country);

                if role == "assistant" {
                    let input_json = serde_json::to_string(&prefix_turns)?;
                    trace_ids.push(tid.to_owned());
                    start_times.push(ts_ns);
                    models.push(model.to_owned());
                    languages.push(t_lang.to_owned());
                    countries.push(t_country.to_owned());
                    turn_idxs.push(t_idx as i64);
                    inputs.push(input_json);
                    outputs.push(content.to_owned());
                    if trace_ids.len() >= max_rows {
                        break;
                    }
                }
                prefix_turns.push(serde_json::json!({
                    "role": role,
                    "content": content,
                }));
            }
        }
        if trace_ids.len() >= max_rows {
            break;
        }
    }

    if trace_ids.is_empty() {
        return Err(format!("wildchat '{}' produced 0 assistant turns", path.display()).into());
    }

    let top_model = pick_frequent_string(&models, None);
    let non_en = pick_frequent_string(&languages, Some("English"));
    let top_non_en_lang = if non_en.is_empty() {
        pick_frequent_string(&languages, None)
    } else {
        non_en
    };

    let mut tid_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for tid in &trace_ids {
        *tid_counts.entry(tid.as_str()).or_insert(0) += 1;
    }
    let multi_turn_tid = tid_counts
        .into_iter()
        .filter(|&(_, c)| c >= 2)
        .max_by(|(k1, c1), (k2, c2)| c1.cmp(c2).then_with(|| k2.cmp(k1)))
        .map_or_else(|| trace_ids[0].clone(), |(k, _)| k.to_owned());

    let (unicode_needle, unicode_needle_hits) = pick_unicode_ilike_needle(&outputs);

    let mut tid_b = StringBuilder::with_capacity(trace_ids.len(), trace_ids.len() * 32);
    let mut model_b = StringBuilder::with_capacity(models.len(), models.len() * 16);
    let mut lang_b = StringBuilder::with_capacity(languages.len(), languages.len() * 10);
    let mut country_b = StringBuilder::with_capacity(countries.len(), countries.len() * 16);
    let mut input_b = StringBuilder::with_capacity(inputs.len(), inputs.len() * 256);
    let mut output_b = StringBuilder::with_capacity(outputs.len(), outputs.len() * 256);
    for i in 0..trace_ids.len() {
        tid_b.append_value(&trace_ids[i]);
        model_b.append_value(&models[i]);
        lang_b.append_value(&languages[i]);
        country_b.append_value(&countries[i]);
        input_b.append_value(&inputs[i]);
        output_b.append_value(&outputs[i]);
    }
    let ts_arr = TimestampNanosecondArray::from(start_times.clone()).with_timezone("UTC");

    let arrow_schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::Utf8, false),
        Field::new(
            "start_time",
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            false,
        ),
        Field::new("model", DataType::Utf8, false),
        Field::new("language", DataType::Utf8, false),
        Field::new("country", DataType::Utf8, false),
        Field::new("turn_idx", DataType::Int64, false),
        Field::new("input", DataType::Utf8, false),
        Field::new("output", DataType::Utf8, false),
    ]));
    let arrow_batch = RecordBatch::try_new(
        arrow_schema,
        vec![
            Arc::new(tid_b.finish()),
            Arc::new(ts_arr),
            Arc::new(model_b.finish()),
            Arc::new(lang_b.finish()),
            Arc::new(country_b.finish()),
            Arc::new(Int64Array::from(turn_idxs.clone())),
            Arc::new(input_b.finish()),
            Arc::new(output_b.finish()),
        ],
    )?;

    let col_schema = ColSchema::try_new_arc(vec![
        ColField::new("trace_id", ColType::Utf8, false),
        ColField::new("start_time", ColType::TimestampNs { utc: true }, false),
        ColField::new("model", ColType::Utf8, false),
        ColField::new("language", ColType::Utf8, false),
        ColField::new("country", ColType::Utf8, false),
        ColField::new("turn_idx", ColType::I64, false),
        ColField::new("input", ColType::Utf8, false),
        ColField::new("output", ColType::Utf8, false),
    ])?;
    let cetra_batch = Batch::try_new_all(
        col_schema,
        vec![
            col_utf8_from_strings(&trace_ids)?,
            col_ts_ns_from_slice(&start_times)?,
            col_utf8_from_strings(&models)?,
            col_utf8_from_strings(&languages)?,
            col_utf8_from_strings(&countries)?,
            col_i64_from_slice(&turn_idxs)?,
            col_utf8_from_strings(&inputs)?,
            col_utf8_from_strings(&outputs)?,
        ],
        trace_ids.len(),
    )?;

    Ok(WildChatData {
        arrow_batch,
        cetra_batch,
        top_model,
        top_non_en_lang,
        multi_turn_tid,
        unicode_needle,
        unicode_needle_hits,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_wildchat_suite(
    wildchat_path: &Path,
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    if !wildchat_path.exists() {
        notes.push(format!(
            "Suite E (`wildchat`): SKIPPED (`{}` not found; pass `--wildchat-parquet` or set `STATAM_WILDCHAT_PARQUET`)",
            wildchat_path.display()
        ));
        return Ok(());
    }

    let data = load_wildchat_15k(wildchat_path)?;
    if data.unicode_needle_hits == 0 {
        return Err(
            "Suite E (`wildchat`): failed to discover a matching Unicode ILIKE needle".into(),
        );
    }
    let raw_bytes = data.arrow_batch.get_array_memory_size() as u64;
    let wl = "wildchat_15k";
    let unicode_lower = data.unicode_needle.to_lowercase();
    let absent_needle = "NEVER_IN_WILDCHAT_XYZ_999";

    notes.push(format!(
        "Suite E (`wildchat`): {} rows x {} cols, `model={}`, `language={}`, `trace_id={}`, `WC_Q4_unicode_ilike=\"{}\" ({} hits)`",
        data.arrow_batch.num_rows(),
        data.arrow_batch.num_columns(),
        data.top_model,
        data.top_non_en_lang,
        data.multi_turn_tid,
        data.unicode_needle,
        data.unicode_needle_hits
    ));

    let bloom_names = ["trace_id", "model", "language"];
    let pq_default = if enabled.contains(&"pq_default") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Default,
            &data.arrow_batch,
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
            &data.arrow_batch,
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
            &data.arrow_batch,
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
        match write_vortex_files(vortex_session, &[data.arrow_batch.clone()]).await {
            Ok((files, ms)) => {
                let b = files.into_iter().next().ok_or("missing vortex output")?;
                write_rows.push(mk_write_row(wl, "vortex", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite E (`wildchat`): `vortex` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let lance_bytes = if enabled.contains(&"lance") {
        try_write_lance(
            wl,
            "Suite E (`wildchat`)",
            &data.arrow_batch,
            raw_bytes,
            write_rows,
            notes,
        )
        .await
    } else {
        None
    };
    let strata_bytes = if enabled.contains(&"strata") {
        match write_generic_strata(&data.arrow_batch, &[], &bloom_names) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(wl, "strata", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite E (`wildchat`): `strata` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let statam_l0 = if enabled.contains(&"statam_l0") {
        let (b, _, ms) = write_generic_statam(
            &data.cetra_batch,
            WriteProfile::L0Fast,
            &[],
            &bloom_names,
            Some("trace_id"),
            &[],
            &["input", "output"],
            false,
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let (statam_l1, statam_l1_pack) = if enabled.contains(&"statam_l1") {
        let (b, pack, ms) = write_generic_statam(
            &data.cetra_batch,
            WriteProfile::L1Compact,
            &[],
            &bloom_names,
            Some("trace_id"),
            &[],
            &["input", "output"],
            true,
            write_threads,
        )?;
        let stored = b.len() as u64 + pack.as_ref().map_or(0, |p| p.len() as u64);
        write_rows.push(mk_write_row(wl, "statam_l1", raw_bytes, stored, ms));
        (Some(b), pack)
    } else {
        (None, None)
    };

    let s3_cfg = SimulatedS3Config::default();
    let vortex_file_local = match &vortex_bytes {
        Some(b) => Some(open_vortex_local(vortex_session, &[b.clone()]).await?),
        None => None,
    };
    let lance_file_local = match &lance_bytes {
        Some(b) => Some(open_lance_local(b).await?),
        None => None,
    };
    let strata_file_local = match &strata_bytes {
        Some(b) => Some(open_single_strata_local(b).await?),
        None => None,
    };

    let queries = [
        "WC_Q1_model_cnt",
        "WC_Q2_lang_filter",
        "WC_Q3_trace_full",
        "WC_Q4_unicode_ilike",
        "WC_Q5_input_absent",
    ];
    for &qname in &queries {
        let mut expected_matches: Option<usize> = None;

        let (pq_proj, pq_preds): (Vec<usize>, Vec<GenPqPred>) = match qname {
            "WC_Q1_model_cnt" => (
                vec![2],
                vec![GenPqPred::Utf8Eq {
                    col: "model".to_owned(),
                    val: data.top_model.clone(),
                }],
            ),
            "WC_Q2_lang_filter" => (
                vec![0, 2, 3],
                vec![GenPqPred::Utf8Eq {
                    col: "language".to_owned(),
                    val: data.top_non_en_lang.clone(),
                }],
            ),
            "WC_Q3_trace_full" => (
                (0..8).collect(),
                vec![GenPqPred::Utf8Eq {
                    col: "trace_id".to_owned(),
                    val: data.multi_turn_tid.clone(),
                }],
            ),
            "WC_Q4_unicode_ilike" => (
                vec![0, 7],
                vec![GenPqPred::Utf8Ilike {
                    col: "output".to_owned(),
                    needle_lower: unicode_lower.clone(),
                }],
            ),
            "WC_Q5_input_absent" => (
                vec![0, 6],
                vec![GenPqPred::Utf8Like {
                    col: "input".to_owned(),
                    needle: absent_needle.to_owned(),
                }],
            ),
            _ => (vec![], vec![]),
        };
        run_parquet_bench_query(
            wl,
            qname,
            &pq_default,
            &pq_tuned,
            &pq_cetra,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected_matches,
            query_rows,
        )
        .await?;

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let (proj, filt, post): (Expression, Option<Expression>, VortexPostFilter<'_>) =
                match qname {
                    "WC_Q1_model_cnt" => (
                        select(["model"], root()),
                        Some(eq(col("model"), lit(data.top_model.as_str()))),
                        VortexPostFilter::None,
                    ),
                    "WC_Q2_lang_filter" => (
                        select(["trace_id", "model", "language"], root()),
                        Some(eq(col("language"), lit(data.top_non_en_lang.as_str()))),
                        VortexPostFilter::None,
                    ),
                    "WC_Q3_trace_full" => (
                        root(),
                        Some(eq(col("trace_id"), lit(data.multi_turn_tid.as_str()))),
                        VortexPostFilter::None,
                    ),
                    "WC_Q4_unicode_ilike" => (
                        select(["trace_id", "output"], root()),
                        None,
                        VortexPostFilter::Ilike {
                            col: "output",
                            needle_lower: unicode_lower.as_str(),
                        },
                    ),
                    "WC_Q5_input_absent" => (
                        select(["trace_id", "input"], root()),
                        None,
                        VortexPostFilter::Like {
                            col: "input",
                            needle: absent_needle,
                        },
                    ),
                    _ => (root(), None, VortexPostFilter::None),
                };
            run_vortex_bench_query(
                wl,
                qname,
                vortex_session,
                bytes,
                &local_files[0],
                &proj,
                filt.as_ref(),
                post,
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_reader)) = (&lance_bytes, &lance_file_local) {
            let top_model = data.top_model.as_str();
            let top_lang = data.top_non_en_lang.as_str();
            let tid = data.multi_turn_tid.as_str();
            let uni_lower = unicode_lower.as_str();
            run_lance_bench_query(
                wl,
                qname,
                bytes,
                local_reader,
                async |r: &LanceFileReader| {
                    let proj_cols: Option<&[&str]> = match qname {
                        "WC_Q1_model_cnt" => Some(&["model"]),
                        "WC_Q2_lang_filter" => Some(&["trace_id", "model", "language"]),
                        "WC_Q3_trace_full" => None,
                        "WC_Q4_unicode_ilike" => Some(&["trace_id", "output"]),
                        "WC_Q5_input_absent" => Some(&["trace_id", "input"]),
                        _ => return Ok(0),
                    };
                    let batches = lance_scan_projected(r, proj_cols).await?;
                    let mut matched = 0usize;
                    for b in &batches {
                        match qname {
                            "WC_Q1_model_cnt" => {
                                if let Some(m) = b.column_by_name("model") {
                                    for i in 0..b.num_rows() {
                                        if extract_str_at(m.as_ref(), i) == Some(top_model) {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "WC_Q2_lang_filter" => {
                                if let Some(l) = b.column_by_name("language") {
                                    for i in 0..b.num_rows() {
                                        if extract_str_at(l.as_ref(), i) == Some(top_lang) {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "WC_Q3_trace_full" => {
                                if let Some(t) = b.column_by_name("trace_id") {
                                    for i in 0..b.num_rows() {
                                        if extract_str_at(t.as_ref(), i) == Some(tid) {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "WC_Q4_unicode_ilike" => {
                                if let Some(o) = b.column_by_name("output") {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(o.as_ref(), i)
                                            && s.to_lowercase().contains(uni_lower)
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "WC_Q5_input_absent" => {
                                if let Some(inp) = b.column_by_name("input") {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(inp.as_ref(), i)
                                            && s.contains(absent_needle)
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(matched)
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_file)) = (&strata_bytes, &strata_file_local) {
            let use_contains = if qname == "WC_Q5_input_absent" {
                let probe = StrataScanRequest {
                    projection: vec![0, 6],
                    predicate: Some(StrataPred::Contains(6, absent_needle.as_bytes().to_vec())),
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
            let top_model = data.top_model.clone();
            let top_lang = data.top_non_en_lang.clone();
            let tid = data.multi_turn_tid.clone();
            let uni_lower = unicode_lower.clone();
            run_strata_bench_query(
                wl,
                qname,
                bytes,
                local_file,
                move |f| {
                    let top_model = top_model.clone();
                    let top_lang = top_lang.clone();
                    let tid = tid.clone();
                    let uni_lower = uni_lower.clone();
                    async move {
                        let req = match qname {
                            "WC_Q1_model_cnt" => StrataScanRequest {
                                projection: vec![2],
                                predicate: Some(StrataPred::Cmp(
                                    2,
                                    CmpOp::Eq,
                                    Lit::Utf8(top_model.clone()),
                                )),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Scan,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "WC_Q2_lang_filter" => StrataScanRequest {
                                projection: vec![0, 2, 3],
                                predicate: Some(StrataPred::Cmp(
                                    3,
                                    CmpOp::Eq,
                                    Lit::Utf8(top_lang.clone()),
                                )),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Scan,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "WC_Q3_trace_full" => StrataScanRequest {
                                projection: (0..8).collect(),
                                predicate: Some(StrataPred::Cmp(
                                    0,
                                    CmpOp::Eq,
                                    Lit::Utf8(tid.clone()),
                                )),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Point,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "WC_Q4_unicode_ilike" => StrataScanRequest {
                                projection: vec![0, 7],
                                predicate: None,
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Scan,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "WC_Q5_input_absent" => StrataScanRequest {
                                projection: vec![0, 6],
                                predicate: use_contains.then(|| {
                                    StrataPred::Contains(6, absent_needle.as_bytes().to_vec())
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
                            _ => return Ok(0),
                        };
                        let batches = f.scan(&req).await?;
                        let mut matched = 0usize;
                        match qname {
                            "WC_Q4_unicode_ilike" => {
                                for b in &batches {
                                    if let Some(o) = b.column_by_name("output") {
                                        for i in 0..b.num_rows() {
                                            if let Some(s) = extract_str_at(o.as_ref(), i)
                                                && s.to_lowercase().contains(uni_lower.as_str())
                                            {
                                                matched += 1;
                                            }
                                        }
                                    }
                                }
                            }
                            "WC_Q5_input_absent" if !use_contains => {
                                for b in &batches {
                                    if let Some(inp) = b.column_by_name("input") {
                                        for i in 0..b.num_rows() {
                                            if let Some(s) = extract_str_at(inp.as_ref(), i)
                                                && s.contains(absent_needle)
                                            {
                                                matched += 1;
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {
                                for b in &batches {
                                    matched += b.num_rows();
                                }
                            }
                        }
                        Ok(matched)
                    }
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        let mut statam_req = match qname {
            "WC_Q1_model_cnt" => ScanRequest {
                projection: Some(vec![2]),
                predicates: vec![StatamPred::new(
                    "model",
                    ZonePred::Eq(ZoneValue::Bytes(data.top_model.as_bytes().to_vec())),
                )],
                ..ScanRequest::default()
            },
            "WC_Q2_lang_filter" => ScanRequest {
                projection: Some(vec![0, 2, 3]),
                predicates: vec![StatamPred::new(
                    "language",
                    ZonePred::Eq(ZoneValue::Bytes(data.top_non_en_lang.as_bytes().to_vec())),
                )],
                ..ScanRequest::default()
            },
            "WC_Q3_trace_full" => ScanRequest {
                projection: None,
                id_lookups: vec![IdLookup::eq(
                    "trace_id",
                    data.multi_turn_tid.as_bytes().to_vec(),
                )],
                coalesce_mode: CoalesceMode::Point,
                ..ScanRequest::default()
            },
            "WC_Q4_unicode_ilike" => ScanRequest {
                projection: Some(vec![0, 7]),
                substrings: vec![SubstringQuery::ilike(
                    "output",
                    data.unicode_needle.as_str(),
                )],
                ..ScanRequest::default()
            },
            "WC_Q5_input_absent" => ScanRequest {
                projection: Some(vec![0, 6]),
                substrings: vec![SubstringQuery::like("input", absent_needle)],
                ..ScanRequest::default()
            },
            _ => ScanRequest::default(),
        };
        statam_req.read_threads = read_threads;
        run_statam_bench_query(
            wl,
            qname,
            &statam_l0,
            &statam_l1,
            statam_l1_pack.as_ref(),
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected_matches,
            query_rows,
        )
        .await?;
    }

    Ok(())
}

// ============================================================================
// Suite F: Hermes Function Calling (`hermes_fc.parquet`, cross-trace dedup)
// ============================================================================

struct HermesData {
    arrow_batch: RecordBatch,
    cetra_batch: Batch,
    top_category: String,
    sample_id: String,
    tool_needle: String,
}

fn load_hermes_all(path: &Path) -> BenchResult<HermesData> {
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(4_096)
        .build()?;

    let mut ids = Vec::new();
    let mut categories = Vec::new();
    let mut subcategories = Vec::new();
    let mut tasks = Vec::new();
    let mut tool_defs = Vec::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();

    for batch_res in reader {
        let batch = batch_res?;
        let id_col = batch.column_by_name("id").ok_or("hermes missing id")?;
        let conv_col = batch
            .column_by_name("conversations")
            .ok_or("hermes missing conversations")?;
        let tools_col = batch
            .column_by_name("tools")
            .ok_or("hermes missing tools")?;
        let cat_col = batch
            .column_by_name("category")
            .ok_or("hermes missing category")?;
        let subcat_col = batch
            .column_by_name("subcategory")
            .ok_or("hermes missing subcategory")?;
        let task_col = batch.column_by_name("task").ok_or("hermes missing task")?;

        let (offsets, turns_struct) = extract_list_parts(conv_col.as_ref())?;
        let from_col = turns_struct.column_by_name("from");
        let val_col = turns_struct.column_by_name("value");

        for r in 0..batch.num_rows() {
            let id = extract_str_at(id_col.as_ref(), r).unwrap_or("").to_owned();
            let category = extract_str_at(cat_col.as_ref(), r).unwrap_or("").to_owned();
            let subcategory = extract_str_at(subcat_col.as_ref(), r)
                .unwrap_or("")
                .to_owned();
            let task = extract_str_at(task_col.as_ref(), r)
                .unwrap_or("")
                .to_owned();
            let tools = extract_str_at(tools_col.as_ref(), r)
                .unwrap_or("")
                .to_owned();

            let mut turns: Vec<(&str, &str)> = Vec::new();
            if !conv_col.is_null(r) {
                let s = offsets[r];
                let e = offsets[r + 1];
                for elem_idx in s..e {
                    let from = from_col
                        .and_then(|c| extract_str_at(c.as_ref(), elem_idx))
                        .unwrap_or("");
                    let val = val_col
                        .and_then(|c| extract_str_at(c.as_ref(), elem_idx))
                        .unwrap_or("");
                    turns.push((from, val));
                }
            }

            let out_idx = turns
                .iter()
                .rposition(|&(from, _)| from == "gpt" || from == "assistant")
                .or_else(|| (!turns.is_empty()).then_some(turns.len() - 1));
            let (input_json, output_str) = match out_idx {
                Some(idx) => {
                    let prefix: Vec<Value> = turns[..idx]
                        .iter()
                        .map(|&(from, val)| serde_json::json!({"from": from, "value": val}))
                        .collect();
                    (serde_json::to_string(&prefix)?, turns[idx].1.to_owned())
                }
                None => ("[]".to_owned(), String::new()),
            };

            ids.push(id);
            categories.push(category);
            subcategories.push(subcategory);
            tasks.push(task);
            tool_defs.push(tools);
            inputs.push(input_json);
            outputs.push(output_str);
        }
    }

    if ids.is_empty() {
        return Err(format!("hermes '{}' contained 0 rows", path.display()).into());
    }

    let top_category = pick_frequent_string(&categories, None);
    let sample_id = ids[ids.len() / 2].clone();
    let tool_needle = pick_ascii_needle_in_range(&tool_defs, None, 5, 500);

    let mut id_b = StringBuilder::with_capacity(ids.len(), ids.len() * 24);
    let mut cat_b = StringBuilder::with_capacity(categories.len(), categories.len() * 16);
    let mut subcat_b = StringBuilder::with_capacity(subcategories.len(), subcategories.len() * 16);
    let mut task_b = StringBuilder::with_capacity(tasks.len(), tasks.len() * 16);
    let mut tools_b = StringBuilder::with_capacity(tool_defs.len(), tool_defs.len() * 256);
    let mut input_b = StringBuilder::with_capacity(inputs.len(), inputs.len() * 256);
    let mut output_b = StringBuilder::with_capacity(outputs.len(), outputs.len() * 128);
    for i in 0..ids.len() {
        id_b.append_value(&ids[i]);
        cat_b.append_value(&categories[i]);
        subcat_b.append_value(&subcategories[i]);
        task_b.append_value(&tasks[i]);
        tools_b.append_value(&tool_defs[i]);
        input_b.append_value(&inputs[i]);
        output_b.append_value(&outputs[i]);
    }

    let arrow_schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("category", DataType::Utf8, false),
        Field::new("subcategory", DataType::Utf8, false),
        Field::new("task", DataType::Utf8, false),
        Field::new("tool_definitions", DataType::Utf8, false),
        Field::new("input", DataType::Utf8, false),
        Field::new("output", DataType::Utf8, false),
    ]));
    let arrow_batch = RecordBatch::try_new(
        arrow_schema,
        vec![
            Arc::new(id_b.finish()),
            Arc::new(cat_b.finish()),
            Arc::new(subcat_b.finish()),
            Arc::new(task_b.finish()),
            Arc::new(tools_b.finish()),
            Arc::new(input_b.finish()),
            Arc::new(output_b.finish()),
        ],
    )?;

    let col_schema = ColSchema::try_new_arc(vec![
        ColField::new("id", ColType::Utf8, false),
        ColField::new("category", ColType::Utf8, false),
        ColField::new("subcategory", ColType::Utf8, false),
        ColField::new("task", ColType::Utf8, false),
        ColField::new("tool_definitions", ColType::Utf8, false),
        ColField::new("input", ColType::Utf8, false),
        ColField::new("output", ColType::Utf8, false),
    ])?;
    let cetra_batch = Batch::try_new_all(
        col_schema,
        vec![
            col_utf8_from_strings(&ids)?,
            col_utf8_from_strings(&categories)?,
            col_utf8_from_strings(&subcategories)?,
            col_utf8_from_strings(&tasks)?,
            col_utf8_from_strings(&tool_defs)?,
            col_utf8_from_strings(&inputs)?,
            col_utf8_from_strings(&outputs)?,
        ],
        ids.len(),
    )?;

    Ok(HermesData {
        arrow_batch,
        cetra_batch,
        top_category,
        sample_id,
        tool_needle,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_hermes_suite(
    hermes_path: &Path,
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    if !hermes_path.exists() {
        notes.push(format!(
            "Suite F (`hermes`): SKIPPED (`{}` not found; pass `--hermes-parquet` or set `STATAM_HERMES_PARQUET`)",
            hermes_path.display()
        ));
        return Ok(());
    }

    let data = load_hermes_all(hermes_path)?;
    let raw_bytes = data.arrow_batch.get_array_memory_size() as u64;
    let wl = "hermes_fc";
    let absent_needle = "NEVER_IN_HERMES_XYZ_999";

    notes.push(format!(
        "Suite F (`hermes`): {} rows x {} cols, `category={}`, `sample_id={}`, `HF_Q3_tool_like.contains(\"{}\")`",
        data.arrow_batch.num_rows(),
        data.arrow_batch.num_columns(),
        data.top_category,
        data.sample_id,
        data.tool_needle
    ));

    let bloom_names = ["id", "category"];
    let pq_default = if enabled.contains(&"pq_default") {
        let (b, ms) = write_generic_parquet(
            ParquetVariant::Default,
            &data.arrow_batch,
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
            &data.arrow_batch,
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
            &data.arrow_batch,
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
        match write_vortex_files(vortex_session, &[data.arrow_batch.clone()]).await {
            Ok((files, ms)) => {
                let b = files.into_iter().next().ok_or("missing vortex output")?;
                write_rows.push(mk_write_row(wl, "vortex", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite F (`hermes`): `vortex` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let lance_bytes = if enabled.contains(&"lance") {
        try_write_lance(
            wl,
            "Suite F (`hermes`)",
            &data.arrow_batch,
            raw_bytes,
            write_rows,
            notes,
        )
        .await
    } else {
        None
    };
    let strata_bytes = if enabled.contains(&"strata") {
        match write_generic_strata(&data.arrow_batch, &[], &bloom_names) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(wl, "strata", raw_bytes, b.len() as u64, ms));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("Suite F (`hermes`): `strata` = N/A ({e})"));
                None
            }
        }
    } else {
        None
    };
    let statam_l0 = if enabled.contains(&"statam_l0") {
        let (b, _, ms) = write_generic_statam(
            &data.cetra_batch,
            WriteProfile::L0Fast,
            &[],
            &bloom_names,
            Some("id"),
            &[],
            &["tool_definitions", "input", "output"],
            false,
            write_threads,
        )?;
        write_rows.push(mk_write_row(wl, "statam_l0", raw_bytes, b.len() as u64, ms));
        Some(b)
    } else {
        None
    };
    let (statam_l1, statam_l1_pack) = if enabled.contains(&"statam_l1") {
        let (b, pack, ms) = write_generic_statam(
            &data.cetra_batch,
            WriteProfile::L1Compact,
            &[],
            &bloom_names,
            Some("id"),
            &[],
            &["tool_definitions", "input", "output"],
            true,
            write_threads,
        )?;
        let stored = b.len() as u64 + pack.as_ref().map_or(0, |p| p.len() as u64);
        write_rows.push(mk_write_row(wl, "statam_l1", raw_bytes, stored, ms));
        (Some(b), pack)
    } else {
        (None, None)
    };

    let s3_cfg = SimulatedS3Config::default();
    let vortex_file_local = match &vortex_bytes {
        Some(b) => Some(open_vortex_local(vortex_session, &[b.clone()]).await?),
        None => None,
    };
    let lance_file_local = match &lance_bytes {
        Some(b) => Some(open_lance_local(b).await?),
        None => None,
    };
    let strata_file_local = match &strata_bytes {
        Some(b) => Some(open_single_strata_local(b).await?),
        None => None,
    };

    let queries = [
        "HF_Q1_cat_cnt",
        "HF_Q2_pt_full",
        "HF_Q3_tool_like",
        "HF_Q4_out_absent",
    ];
    for &qname in &queries {
        let mut expected_matches: Option<usize> = None;

        let (pq_proj, pq_preds): (Vec<usize>, Vec<GenPqPred>) = match qname {
            "HF_Q1_cat_cnt" => (
                vec![1],
                vec![GenPqPred::Utf8Eq {
                    col: "category".to_owned(),
                    val: data.top_category.clone(),
                }],
            ),
            "HF_Q2_pt_full" => (
                (0..7).collect(),
                vec![GenPqPred::Utf8Eq {
                    col: "id".to_owned(),
                    val: data.sample_id.clone(),
                }],
            ),
            "HF_Q3_tool_like" => (
                vec![0, 1],
                vec![GenPqPred::Utf8Like {
                    col: "tool_definitions".to_owned(),
                    needle: data.tool_needle.clone(),
                }],
            ),
            "HF_Q4_out_absent" => (
                vec![0],
                vec![GenPqPred::Utf8Like {
                    col: "output".to_owned(),
                    needle: absent_needle.to_owned(),
                }],
            ),
            _ => (vec![], vec![]),
        };
        run_parquet_bench_query(
            wl,
            qname,
            &pq_default,
            &pq_tuned,
            &pq_cetra,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected_matches,
            query_rows,
        )
        .await?;

        if let (Some(bytes), Some(local_files)) = (&vortex_bytes, &vortex_file_local) {
            let (proj, filt, post): (Expression, Option<Expression>, VortexPostFilter<'_>) =
                match qname {
                    "HF_Q1_cat_cnt" => (
                        select(["category"], root()),
                        Some(eq(col("category"), lit(data.top_category.as_str()))),
                        VortexPostFilter::None,
                    ),
                    "HF_Q2_pt_full" => (
                        root(),
                        Some(eq(col("id"), lit(data.sample_id.as_str()))),
                        VortexPostFilter::None,
                    ),
                    "HF_Q3_tool_like" => (
                        select(["id", "category", "tool_definitions"], root()),
                        None,
                        VortexPostFilter::Like {
                            col: "tool_definitions",
                            needle: data.tool_needle.as_str(),
                        },
                    ),
                    "HF_Q4_out_absent" => (
                        select(["id", "output"], root()),
                        None,
                        VortexPostFilter::Like {
                            col: "output",
                            needle: absent_needle,
                        },
                    ),
                    _ => (root(), None, VortexPostFilter::None),
                };
            run_vortex_bench_query(
                wl,
                qname,
                vortex_session,
                bytes,
                &local_files[0],
                &proj,
                filt.as_ref(),
                post,
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_reader)) = (&lance_bytes, &lance_file_local) {
            let top_cat = data.top_category.as_str();
            let sample_id = data.sample_id.as_str();
            let tool_needle = data.tool_needle.as_str();
            run_lance_bench_query(
                wl,
                qname,
                bytes,
                local_reader,
                async |r: &LanceFileReader| {
                    let proj_cols: Option<&[&str]> = match qname {
                        "HF_Q1_cat_cnt" => Some(&["category"]),
                        "HF_Q2_pt_full" => None,
                        "HF_Q3_tool_like" => Some(&["id", "category", "tool_definitions"]),
                        "HF_Q4_out_absent" => Some(&["id", "output"]),
                        _ => return Ok(0),
                    };
                    let batches = lance_scan_projected(r, proj_cols).await?;
                    let mut matched = 0usize;
                    for b in &batches {
                        match qname {
                            "HF_Q1_cat_cnt" => {
                                if let Some(c) = b.column_by_name("category") {
                                    for i in 0..b.num_rows() {
                                        if extract_str_at(c.as_ref(), i) == Some(top_cat) {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "HF_Q2_pt_full" => {
                                if let Some(id_c) = b.column_by_name("id") {
                                    for i in 0..b.num_rows() {
                                        if extract_str_at(id_c.as_ref(), i) == Some(sample_id) {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "HF_Q3_tool_like" => {
                                if let Some(t) = b.column_by_name("tool_definitions") {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(t.as_ref(), i)
                                            && s.contains(tool_needle)
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            "HF_Q4_out_absent" => {
                                if let Some(o) = b.column_by_name("output") {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(o.as_ref(), i)
                                            && s.contains(absent_needle)
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(matched)
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_file)) = (&strata_bytes, &strata_file_local) {
            let (probe_col, probe_needle) = if qname == "HF_Q3_tool_like" {
                (4usize, data.tool_needle.as_str())
            } else {
                (6usize, absent_needle)
            };
            let use_contains = if matches!(qname, "HF_Q3_tool_like" | "HF_Q4_out_absent") {
                let probe = StrataScanRequest {
                    projection: if qname == "HF_Q3_tool_like" {
                        vec![0, 1]
                    } else {
                        vec![0]
                    },
                    predicate: Some(StrataPred::Contains(
                        probe_col,
                        probe_needle.as_bytes().to_vec(),
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
            let top_cat = data.top_category.clone();
            let sample_id = data.sample_id.clone();
            let tool_needle = data.tool_needle.clone();
            run_strata_bench_query(
                wl,
                qname,
                bytes,
                local_file,
                move |f| {
                    let top_cat = top_cat.clone();
                    let sample_id = sample_id.clone();
                    let tool_needle = tool_needle.clone();
                    async move {
                        let req = match qname {
                            "HF_Q1_cat_cnt" => StrataScanRequest {
                                projection: vec![1],
                                predicate: Some(StrataPred::Cmp(
                                    1,
                                    CmpOp::Eq,
                                    Lit::Utf8(top_cat.clone()),
                                )),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Scan,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "HF_Q2_pt_full" => StrataScanRequest {
                                projection: (0..7).collect(),
                                predicate: Some(StrataPred::Cmp(
                                    0,
                                    CmpOp::Eq,
                                    Lit::Utf8(sample_id.clone()),
                                )),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Point,
                                views: false,
                                top: None,
                                row_filter: None,
                            },
                            "HF_Q3_tool_like" => StrataScanRequest {
                                projection: if use_contains {
                                    vec![0, 1]
                                } else {
                                    vec![0, 1, 4]
                                },
                                predicate: use_contains.then(|| {
                                    StrataPred::Contains(4, tool_needle.as_bytes().to_vec())
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
                            "HF_Q4_out_absent" => StrataScanRequest {
                                projection: if use_contains { vec![0] } else { vec![0, 6] },
                                predicate: use_contains.then(|| {
                                    StrataPred::Contains(6, absent_needle.as_bytes().to_vec())
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
                            _ => return Ok(0),
                        };
                        let batches = f.scan(&req).await?;
                        let mut matched = 0usize;
                        if matches!(qname, "HF_Q3_tool_like" | "HF_Q4_out_absent") && !use_contains
                        {
                            let (col_name, nd) = if qname == "HF_Q3_tool_like" {
                                ("tool_definitions", tool_needle.as_str())
                            } else {
                                ("output", absent_needle)
                            };
                            for b in &batches {
                                if let Some(arr) = b.column_by_name(col_name) {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(arr.as_ref(), i)
                                            && s.contains(nd)
                                        {
                                            matched += 1;
                                        }
                                    }
                                }
                            }
                        } else {
                            for b in &batches {
                                matched += b.num_rows();
                            }
                        }
                        Ok(matched)
                    }
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
            )
            .await?;
        }

        let mut statam_req = match qname {
            "HF_Q1_cat_cnt" => ScanRequest {
                projection: Some(vec![1]),
                predicates: vec![StatamPred::new(
                    "category",
                    ZonePred::Eq(ZoneValue::Bytes(data.top_category.as_bytes().to_vec())),
                )],
                ..ScanRequest::default()
            },
            "HF_Q2_pt_full" => ScanRequest {
                projection: None,
                id_lookups: vec![IdLookup::eq("id", data.sample_id.as_bytes().to_vec())],
                coalesce_mode: CoalesceMode::Point,
                ..ScanRequest::default()
            },
            "HF_Q3_tool_like" => ScanRequest {
                projection: Some(vec![0, 1]),
                substrings: vec![SubstringQuery::like(
                    "tool_definitions",
                    data.tool_needle.as_str(),
                )],
                ..ScanRequest::default()
            },
            "HF_Q4_out_absent" => ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![SubstringQuery::like("output", absent_needle)],
                ..ScanRequest::default()
            },
            _ => ScanRequest::default(),
        };
        statam_req.read_threads = read_threads;
        run_statam_bench_query(
            wl,
            qname,
            &statam_l0,
            &statam_l1,
            statam_l1_pack.as_ref(),
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected_matches,
            query_rows,
        )
        .await?;
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
        "\n# External Parquet + Vortex + Lance + Strata + Statam Benchmark Report (`scale={}`)",
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
    let s3_mode = RealS3Mode::from_env();
    if s3_mode.is_real() {
        let _ = writeln!(
            out,
            "- **S3 Mode:** `{}` (real remote S3, `s3_only={}`)",
            s3_mode.label(),
            s3_only_mode()
        );
    } else {
        let _ = writeln!(
            out,
            "- **Simulated S3 Model:** {}",
            sim_s3_header_summary(s3_mode)
        );
    }

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
    let s3_mode = RealS3Mode::from_env();
    if s3_mode.is_real() {
        eprintln!(
            "[external_bench] S3 mode: {} (s3_only={})",
            s3_mode.label(),
            s3_only_mode()
        );
    } else {
        eprintln!(
            "[external_bench] Simulated S3: {} (s3_only={})",
            sim_s3_header_summary(s3_mode),
            s3_only_mode()
        );
    }
    let scale = BenchScale::from_env();
    let warm_runs = std::env::var("STATAM_WARM_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(if scale == BenchScale::Quick { 5 } else { 10 });
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

    if cli.suite.includes_jsonbench() {
        run_jsonbench_suite(
            &cli.bluesky_json,
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

    if cli.suite.includes_wildchat() {
        run_wildchat_suite(
            &cli.wildchat_parquet,
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

    if cli.suite.includes_hermes() {
        run_hermes_suite(
            &cli.hermes_parquet,
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
