//! Lance / Vortex / F3 Extended Benchmark Suites (Suites G–L) across all 7 formats:
//! `pq_default`, `pq_tuned`, `vortex`, `lance`, `strata`, `statam_l0`, `statam_l1`.
//!
//! - Suite G (`blobs`): Multimodal Blobs (2,000 rows x 6 cols, 48–320 KiB binary blobs, ~360 MiB raw)
//! - Suite H (`wide-1000`): Ultra-Wide Sparse Feature Table (25,000 rows x 1,000 cols, 95% NULL)
//! - Suite I (`fineweb`): Real FineWeb-Edu English Prose (40,000 rows, ExternalPayload `.stpk` + substring search)
//! - Suite J (`nested`): Deeply Nested & Sparse Structural Data (100,000 rows x 7 cols: `List<i64>`, `List<List<i64>>`, `List<Utf8>`, `Map<Utf8, Utf8>`)
//! - Suite K (`tpch-bi`): Real TPC-H `lineitem` (500,000 rows, native `Date32` + `Decimal128`/`Float64`)
//! - Suite L (`dataloader`): Shuffled Epoch DataLoader (100 x 256-row random mini-batches = 25,600 rows, local & S3 cached)

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{
    Array, ArrayRef, BinaryArray, BinaryBuilder, BinaryViewArray, BooleanArray, Date32Array,
    Decimal128Array, Float64Array, Float64Builder, Int64Array, Int64Builder, LargeBinaryArray,
    LargeStringArray, ListBuilder, MapArray, MapBuilder, RecordBatch, StringArray, StringBuilder,
    StringViewArray, UInt64Array,
};
use arrow::compute::concat_batches;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow_array_60::cast::AsArray as AsArray60;
use arrow_array_60::{
    Array as Array60, ArrayRef as ArrayRef60, BinaryArray as BinaryArray60,
    BinaryViewArray as BinaryViewArray60, Date32Array as Date32Array60,
    Decimal128Array as Decimal128Array60, Float64Array as Float64Array60,
    Int64Array as Int64Array60, LargeBinaryArray as LargeBinaryArray60, ListArray as ListArray60,
    MapArray as MapArray60, RecordBatch as RecordBatch60,
};
use arrow_schema_60::{DataType as DataType60, Field as Field60};
use bytes::Bytes;
use cetra_column::{Batch, FlatData};
use cetra_statam::{
    ChunkPackBuilder, CoalesceMode, IdLookup, InMemoryPackResolver, PackResolver, PackWriteOptions,
    Predicate as StatamPred, RangeReader, ScanRequest, SimulatedS3Config, SliceReader,
    StatamReader, StatamWriter, SubstringQuery, TableKind, WriteOptions, WriteProfile, ZonePred,
    ZoneValue,
};
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
use strata::{
    CmpOp, Config as StrataConfig, Lit, Predicate as StrataPred, ScanKind,
    ScanRequest as StrataScanRequest, StrataFile, StrataWriter,
};
use vortex::array::VortexSessionExecute;
use vortex::arrow::ArrowSessionExt;
use vortex::buffer::Buffer;
use vortex::expr::{Expression, and, col, eq, gt_eq, lit, lt_eq, not, root, select};
use vortex::file::{OpenOptionsSessionExt, VortexFile};
use vortex::scalar::Scalar;
use vortex::session::VortexSession;

use statam_bench7::lance_bench::{
    lance_scan_projected, lance_take_indices, open_lance_from_bench_s3, open_lance_local,
    prepare_lance_batches, write_lance_bytes,
};
use statam_bench7::queries::{
    PACK_ID, SimS3PackResolver, check_parity, measure_local_async_ms, measure_local_ms,
    summarize_timings,
};
use statam_bench7::real_s3::{
    BenchS3ParquetAsyncReader, BenchS3Reader, RealS3Mode, run_s3_adaptive, s3_only_mode,
    sim_s3_header_summary,
};
use statam_bench7::report::{QueryBenchRow, WriteBenchRow, format_kib};
use statam_bench7::s3_parquet::{find_leaf_idx, open_parquet_async_builder};
use statam_bench7::schema::{
    BenchResult, ParquetVariant, arrow_to_cetra_batch, generic_parquet_writer_props,
};
use statam_bench7::strata_bench::{SimS3ObjectStore, SliceObjectStore};
use statam_bench7::vortex_bench::{
    SimS3VortexRead, chunk_to_record_batch, new_vortex_session, open_vortex_local,
    str_value_at as vortex_str_value_at, write_vortex_files,
};
use statam_bench7::workloads::DAY_START;

const DEFAULT_FORMATS: [&str; 7] = [
    "pq_default",
    "pq_tuned",
    "vortex",
    "lance",
    "strata",
    "statam_l0",
    "statam_l1",
];

const ALL_SUPPORTED_FORMATS: [&str; 8] = [
    "pq_default",
    "pq_tuned",
    "pq_cetra",
    "vortex",
    "lance",
    "strata",
    "statam_l0",
    "statam_l1",
];

/// TPC-H epoch day constants (days since 1970-01-01):
/// - `1994-01-01`: 24 * 365 + 6 leap days (1972,76,80,84,88,92) = 8766
/// - `1994-12-31`: 8766 + 364 = 9130 (`< 1995-01-01`, which is 9131)
/// - `1998-09-02`: 1998-01-01 (10227) + 243 + 1 = 10471
const TPCH_1994_01_01: i32 = 8_766;
const TPCH_1994_12_31: i32 = 9_130;
const TPCH_1998_09_02: i32 = 10_471;

// ============================================================================
// CLI & Configuration
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuiteSelection {
    All,
    Blobs,
    Wide1000,
    FineWeb,
    Nested,
    TpchBi,
    DataLoader,
}

impl SuiteSelection {
    fn includes_blobs(self) -> bool {
        matches!(self, Self::All | Self::Blobs)
    }

    fn includes_wide_1000(self) -> bool {
        matches!(self, Self::All | Self::Wide1000)
    }

    fn includes_fineweb(self) -> bool {
        matches!(self, Self::All | Self::FineWeb)
    }

    fn includes_nested(self) -> bool {
        matches!(self, Self::All | Self::Nested)
    }

    fn includes_tpch_bi(self) -> bool {
        matches!(self, Self::All | Self::TpchBi)
    }

    fn includes_dataloader(self) -> bool {
        matches!(self, Self::All | Self::DataLoader)
    }
}

struct CliConfig {
    suite: SuiteSelection,
    statam_write_threads: usize,
    statam_read_threads: usize,
    fineweb_parquet: PathBuf,
    lineitem_parquet: PathBuf,
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
        return Ok(DEFAULT_FORMATS.to_vec());
    }
    for r in &requested {
        if !ALL_SUPPORTED_FORMATS.contains(r) {
            return Err(format!(
                "unknown format '{r}' (expected subset of {:?})",
                ALL_SUPPORTED_FORMATS
            )
            .into());
        }
    }
    Ok(ALL_SUPPORTED_FORMATS
        .iter()
        .copied()
        .filter(|f| requested.contains(f))
        .collect())
}

fn parse_suite(val: &str) -> BenchResult<SuiteSelection> {
    match val.trim() {
        "all" => Ok(SuiteSelection::All),
        "blobs" => Ok(SuiteSelection::Blobs),
        "wide-1000" | "wide_1000" => Ok(SuiteSelection::Wide1000),
        "fineweb" | "fineweb-edu" | "fineweb_edu" => Ok(SuiteSelection::FineWeb),
        "nested" => Ok(SuiteSelection::Nested),
        "tpch-bi" | "tpch_bi" | "lineitem" => Ok(SuiteSelection::TpchBi),
        "dataloader" | "data-loader" | "data_loader" => Ok(SuiteSelection::DataLoader),
        other => Err(format!(
            "unknown --suite '{other}' (expected all|blobs|wide-1000|fineweb|nested|tpch-bi|dataloader)"
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
    let mut fineweb_parquet = default_corpus_path("STATAM_FINEWEB_PARQUET", "fineweb_edu.parquet");
    let mut lineitem_parquet = default_corpus_path("STATAM_LINEITEM_PARQUET", "lineitem.parquet");
    let mut formats_override: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(val) = arg.strip_prefix("--suite=") {
            suite = parse_suite(val)?;
        } else if arg == "--suite" {
            let val = args.next().ok_or("--suite requires a value")?;
            suite = parse_suite(&val)?;
        } else if let Some(val) = arg.strip_prefix("--formats=") {
            formats_override = Some(val.to_owned());
        } else if arg == "--formats" {
            let val = args.next().ok_or("--formats requires a csv list")?;
            formats_override = Some(val);
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
        } else if let Some(val) = arg.strip_prefix("--fineweb-parquet=") {
            fineweb_parquet = PathBuf::from(val);
        } else if arg == "--fineweb-parquet" {
            let val = args.next().ok_or("--fineweb-parquet requires a path")?;
            fineweb_parquet = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--lineitem-parquet=") {
            lineitem_parquet = PathBuf::from(val);
        } else if arg == "--lineitem-parquet" {
            let val = args.next().ok_or("--lineitem-parquet requires a path")?;
            lineitem_parquet = PathBuf::from(val);
        } else {
            return Err(format!(
                "unknown argument '{arg}' (supported: --suite, --formats, --statam-write-threads, --statam-read-threads, --fineweb-parquet, --lineitem-parquet)"
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
        DEFAULT_FORMATS.to_vec()
    };

    Ok(CliConfig {
        suite,
        statam_write_threads,
        statam_read_threads,
        fineweb_parquet,
        lineitem_parquet,
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

// ============================================================================
// Shared Format Write / Open / Scan / Take Helpers
// ============================================================================

struct EncodedSuiteFiles {
    pq_default: Option<Bytes>,
    pq_tuned: Option<Bytes>,
    pq_cetra: Option<Bytes>,
    vortex: Option<Bytes>,
    lance: Option<Bytes>,
    strata: Option<Bytes>,
    statam_l0: Option<Bytes>,
    statam_l1: Option<Bytes>,
    statam_l1_pack: Option<Bytes>,
}

struct SuiteEncodeOpts<'a> {
    wl: &'a str,
    suite_label: &'a str,
    bloom_cols: &'a [&'a str],
    hot_bloom_col: Option<&'a str>,
    sort_col_names: &'a [&'a str],
    statam_sort_cols: &'a [u16],
    narrow_trigram_cols: &'a [&'a str],
    payload_cols: &'a [&'a str],
    l1_use_pack: bool,
    tuned_rg_rows: usize,
    pq_step_rows: usize,
    write_threads: usize,
}

fn write_generic_parquet(
    variant: ParquetVariant,
    batch: &RecordBatch,
    bloom_cols: &[&str],
    sort_cols: &[&str],
    tuned_rg_rows: usize,
    step_rows: usize,
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
        let step = step_rows.max(1);
        for off in (0..batch.num_rows()).step_by(step) {
            let len = step.min(batch.num_rows() - off);
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
    Ok((
        Bytes::from(buf),
        pack_bytes,
        t0.elapsed().as_secs_f64() * 1000.0,
    ))
}

async fn encode_all_formats(
    arrow_batch: &RecordBatch,
    cetra_batch_res: Result<&Batch, &str>,
    raw_bytes: u64,
    enabled: &[&str],
    vortex_session: &VortexSession,
    opts: &SuiteEncodeOpts<'_>,
    write_rows: &mut Vec<WriteBenchRow>,
    notes: &mut Vec<String>,
) -> EncodedSuiteFiles {
    let pq_default = if enabled.contains(&"pq_default") {
        match write_generic_parquet(
            ParquetVariant::Default,
            arrow_batch,
            opts.bloom_cols,
            opts.sort_col_names,
            1_000_000,
            opts.pq_step_rows,
        ) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(
                    opts.wl,
                    "pq_default",
                    raw_bytes,
                    b.len() as u64,
                    ms,
                ));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("{}: `pq_default` = N/A ({e})", opts.suite_label));
                None
            }
        }
    } else {
        None
    };

    let pq_tuned = if enabled.contains(&"pq_tuned") {
        match write_generic_parquet(
            ParquetVariant::Tuned,
            arrow_batch,
            opts.bloom_cols,
            opts.sort_col_names,
            opts.tuned_rg_rows,
            opts.pq_step_rows,
        ) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(
                    opts.wl,
                    "pq_tuned",
                    raw_bytes,
                    b.len() as u64,
                    ms,
                ));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("{}: `pq_tuned` = N/A ({e})", opts.suite_label));
                None
            }
        }
    } else {
        None
    };

    let pq_cetra = if enabled.contains(&"pq_cetra") {
        match write_generic_parquet(
            ParquetVariant::Cetra,
            arrow_batch,
            opts.bloom_cols,
            opts.sort_col_names,
            100_000,
            opts.pq_step_rows,
        ) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(
                    opts.wl,
                    "pq_cetra",
                    raw_bytes,
                    b.len() as u64,
                    ms,
                ));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("{}: `pq_cetra` = N/A ({e})", opts.suite_label));
                None
            }
        }
    } else {
        None
    };

    let vortex = if enabled.contains(&"vortex") {
        match write_vortex_files(vortex_session, std::slice::from_ref(arrow_batch)).await {
            Ok((files, ms)) => {
                if let Some(b) = files.into_iter().next() {
                    write_rows.push(mk_write_row(
                        opts.wl,
                        "vortex",
                        raw_bytes,
                        b.len() as u64,
                        ms,
                    ));
                    Some(b)
                } else {
                    notes.push(format!(
                        "{}: `vortex` = N/A (empty output)",
                        opts.suite_label
                    ));
                    None
                }
            }
            Err(e) => {
                notes.push(format!("{}: `vortex` = N/A ({e})", opts.suite_label));
                None
            }
        }
    } else {
        None
    };

    let lance = if enabled.contains(&"lance") {
        match prepare_lance_batches(std::slice::from_ref(arrow_batch)) {
            Ok(prepared) => match write_lance_bytes(&prepared).await {
                Ok((b, ms)) => {
                    write_rows.push(mk_write_row(
                        opts.wl,
                        "lance",
                        raw_bytes,
                        b.len() as u64,
                        ms,
                    ));
                    Some(b)
                }
                Err(e) => {
                    notes.push(format!("{}: `lance` = N/A ({e})", opts.suite_label));
                    None
                }
            },
            Err(e) => {
                notes.push(format!("{}: `lance` = N/A ({e})", opts.suite_label));
                None
            }
        }
    } else {
        None
    };

    let strata = if enabled.contains(&"strata") {
        match write_generic_strata(arrow_batch, opts.sort_col_names, opts.bloom_cols) {
            Ok((b, ms)) => {
                write_rows.push(mk_write_row(
                    opts.wl,
                    "strata",
                    raw_bytes,
                    b.len() as u64,
                    ms,
                ));
                Some(b)
            }
            Err(e) => {
                notes.push(format!("{}: `strata` = N/A ({e})", opts.suite_label));
                None
            }
        }
    } else {
        None
    };

    let statam_l0 = if enabled.contains(&"statam_l0") {
        match cetra_batch_res {
            Ok(cb) => match write_generic_statam(
                cb,
                WriteProfile::L0Fast,
                opts.statam_sort_cols,
                opts.bloom_cols,
                opts.hot_bloom_col,
                &[],
                opts.payload_cols,
                false,
                opts.write_threads,
            ) {
                Ok((b, _, ms)) => {
                    write_rows.push(mk_write_row(
                        opts.wl,
                        "statam_l0",
                        raw_bytes,
                        b.len() as u64,
                        ms,
                    ));
                    Some(b)
                }
                Err(e) => {
                    notes.push(format!("{}: `statam_l0` = N/A ({e})", opts.suite_label));
                    None
                }
            },
            Err(msg) => {
                notes.push(format!("{}: `statam_l0` = N/A ({msg})", opts.suite_label));
                None
            }
        }
    } else {
        None
    };

    let (statam_l1, statam_l1_pack) = if enabled.contains(&"statam_l1") {
        match cetra_batch_res {
            Ok(cb) => match write_generic_statam(
                cb,
                WriteProfile::L1Compact,
                opts.statam_sort_cols,
                opts.bloom_cols,
                opts.hot_bloom_col,
                opts.narrow_trigram_cols,
                opts.payload_cols,
                opts.l1_use_pack,
                opts.write_threads,
            ) {
                Ok((b, pack, ms)) => {
                    let stored = b.len() as u64 + pack.as_ref().map_or(0, |p| p.len() as u64);
                    write_rows.push(mk_write_row(opts.wl, "statam_l1", raw_bytes, stored, ms));
                    (Some(b), pack)
                }
                Err(e) => {
                    notes.push(format!("{}: `statam_l1` = N/A ({e})", opts.suite_label));
                    (None, None)
                }
            },
            Err(msg) => {
                notes.push(format!("{}: `statam_l1` = N/A ({msg})", opts.suite_label));
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    EncodedSuiteFiles {
        pq_default,
        pq_tuned,
        pq_cetra,
        vortex,
        lance,
        strata,
        statam_l0,
        statam_l1,
        statam_l1_pack,
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

fn open_statam_local(bytes: &Bytes, pack_opt: Option<&Bytes>) -> BenchResult<StatamReader> {
    let warm_pack: Option<Arc<dyn PackResolver>> = if let Some(pb) = pack_opt {
        let mut res = InMemoryPackResolver::new();
        res.insert_bytes(PACK_ID, pb.clone())?;
        Some(Arc::new(res))
    } else {
        None
    };
    Ok(StatamReader::open(
        Arc::new(SliceReader::new(bytes.clone())),
        warm_pack,
    )?)
}

fn create_statam_s3_readers(
    bytes: &Bytes,
    pack_opt: Option<&Bytes>,
    s3_cfg: SimulatedS3Config,
) -> (
    Arc<BenchS3Reader>,
    Option<Arc<BenchS3Reader>>,
    Option<Arc<SimS3PackResolver>>,
) {
    let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
    let s3_pack = pack_opt.map(|pb| BenchS3Reader::from_bytes(pb.clone(), s3_cfg));
    let s3_resolver = s3_pack
        .as_ref()
        .map(|ps3| Arc::new(SimS3PackResolver::new(PACK_ID, Arc::clone(ps3))));
    (s3_file, s3_pack, s3_resolver)
}

fn open_statam_from_bench_s3(
    s3_file: &Arc<BenchS3Reader>,
    s3_pack: Option<&Arc<BenchS3Reader>>,
    s3_resolver: Option<&Arc<SimS3PackResolver>>,
) -> BenchResult<StatamReader> {
    s3_file.evict_all();
    s3_file.reset_counters();
    if let Some(ps3) = s3_pack {
        ps3.evict_all();
        ps3.reset_counters();
    }
    if let Some(res) = s3_resolver {
        res.reset_opened();
    }
    let pack_dyn = s3_resolver.map(|r| Arc::clone(r) as Arc<dyn PackResolver>);
    Ok(StatamReader::open(
        Arc::clone(s3_file) as Arc<dyn RangeReader>,
        pack_dyn,
    )?)
}

fn statam_s3_totals(
    s3_file: &BenchS3Reader,
    s3_pack: Option<&Arc<BenchS3Reader>>,
) -> (f64, u64, u64) {
    let mut ms = s3_file.simulated_latency_ms();
    let mut gets = s3_file.counters().gets;
    let mut bytes = s3_file.counters().bytes_from_s3;
    if let Some(ps3) = s3_pack {
        ms += ps3.simulated_latency_ms();
        gets += ps3.counters().gets;
        bytes += ps3.counters().bytes_from_s3;
    }
    (ms, gets, bytes)
}

// ============================================================================
// Generic Parquet Predicate & Pruning Engine
// ============================================================================

#[derive(Clone)]
enum GenPqPred {
    I64Eq {
        col: String,
        val: i64,
    },
    I64Ge {
        col: String,
        lo: i64,
    },
    I64Le {
        col: String,
        hi: i64,
    },
    I64Between {
        col: String,
        lo: i64,
        hi: i64,
    },
    F64Ge {
        col: String,
        lo: f64,
    },
    F64Between {
        col: String,
        lo: f64,
        hi: f64,
    },
    F64Lt {
        col: String,
        hi: f64,
    },
    Date32Between {
        col: String,
        lo: i32,
        hi: i32,
    },
    Date32Le {
        col: String,
        hi: i32,
    },
    Dec128Between {
        col: String,
        lo: i128,
        hi: i128,
    },
    Dec128Lt {
        col: String,
        hi: i128,
    },
    Utf8Eq {
        col: String,
        val: String,
    },
    Utf8Like {
        col: String,
        needle: String,
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
            | Self::I64Ge { col, .. }
            | Self::I64Le { col, .. }
            | Self::I64Between { col, .. }
            | Self::F64Ge { col, .. }
            | Self::F64Between { col, .. }
            | Self::F64Lt { col, .. }
            | Self::Date32Between { col, .. }
            | Self::Date32Le { col, .. }
            | Self::Dec128Between { col, .. }
            | Self::Dec128Lt { col, .. }
            | Self::Utf8Eq { col, .. }
            | Self::Utf8Like { col, .. }
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
            (GenPqPred::I64Ge { lo, .. }, Statistics::Int64(s)) => {
                if let Some(&max) = s.max_opt()
                    && max < *lo
                {
                    return false;
                }
            }
            (GenPqPred::I64Le { hi, .. }, Statistics::Int64(s)) => {
                if let Some(&min) = s.min_opt()
                    && min > *hi
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
            (GenPqPred::F64Ge { lo, .. }, Statistics::Double(s)) => {
                if let Some(&max) = s.max_opt()
                    && !max.is_nan()
                    && max < *lo
                {
                    return false;
                }
            }
            (GenPqPred::F64Between { lo, hi, .. }, Statistics::Double(s)) => {
                if let (Some(&min), Some(&max)) = (s.min_opt(), s.max_opt())
                    && !min.is_nan()
                    && !max.is_nan()
                    && (max < *lo || min > *hi)
                {
                    return false;
                }
            }
            (GenPqPred::F64Lt { hi, .. }, Statistics::Double(s)) => {
                if let Some(&min) = s.min_opt()
                    && !min.is_nan()
                    && min >= *hi
                {
                    return false;
                }
            }
            (GenPqPred::Date32Between { lo, hi, .. }, Statistics::Int32(s)) => {
                if let (Some(&min), Some(&max)) = (s.min_opt(), s.max_opt())
                    && (max < *lo || min > *hi)
                {
                    return false;
                }
            }
            (GenPqPred::Date32Le { hi, .. }, Statistics::Int32(s)) => {
                if let Some(&min) = s.min_opt()
                    && min > *hi
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
        (GenPqPred::I64Ge { lo, .. }, ColumnIndexMetaData::INT64(idx)) => {
            if let Some(&max) = idx.max_value(page_idx) {
                return max >= *lo;
            }
        }
        (GenPqPred::I64Le { hi, .. }, ColumnIndexMetaData::INT64(idx)) => {
            if let Some(&min) = idx.min_value(page_idx) {
                return min <= *hi;
            }
        }
        (GenPqPred::I64Between { lo, hi, .. }, ColumnIndexMetaData::INT64(idx)) => {
            if let (Some(&min), Some(&max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return max >= *lo && min <= *hi;
            }
        }
        (GenPqPred::F64Ge { lo, .. }, ColumnIndexMetaData::DOUBLE(idx)) => {
            if let Some(&max) = idx.max_value(page_idx)
                && !max.is_nan()
            {
                return max >= *lo;
            }
        }
        (GenPqPred::F64Between { lo, hi, .. }, ColumnIndexMetaData::DOUBLE(idx)) => {
            if let (Some(&min), Some(&max)) = (idx.min_value(page_idx), idx.max_value(page_idx))
                && !min.is_nan()
                && !max.is_nan()
            {
                return max >= *lo && min <= *hi;
            }
        }
        (GenPqPred::F64Lt { hi, .. }, ColumnIndexMetaData::DOUBLE(idx)) => {
            if let Some(&min) = idx.min_value(page_idx)
                && !min.is_nan()
            {
                return min < *hi;
            }
        }
        (GenPqPred::Date32Between { lo, hi, .. }, ColumnIndexMetaData::INT32(idx)) => {
            if let (Some(&min), Some(&max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return max >= *lo && min <= *hi;
            }
        }
        (GenPqPred::Date32Le { hi, .. }, ColumnIndexMetaData::INT32(idx)) => {
            if let Some(&min) = idx.min_value(page_idx) {
                return min <= *hi;
            }
        }
        (GenPqPred::Utf8Eq { val, .. }, ColumnIndexMetaData::BYTE_ARRAY(idx)) => {
            let target = val.as_bytes();
            if let (Some(min), Some(max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return target >= min && target <= max;
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
            GenPqPred::I64Ge { lo, .. } => {
                let lo_v = *lo;
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
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) >= lo_v))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::I64Le { hi, .. } => {
                let hi_v = *hi;
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
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) <= hi_v))
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
            GenPqPred::F64Ge { lo, .. } => {
                let lo_v = *lo;
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Float64Array".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) >= lo_v))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::F64Between { lo, hi, .. } => {
                let (lo_v, hi_v) = (*lo, *hi);
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Float64Array".to_owned(),
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
            GenPqPred::F64Lt { hi, .. } => {
                let hi_v = *hi;
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Float64Array".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) < hi_v))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::Date32Between { lo, hi, .. } => {
                let (lo_v, hi_v) = (*lo, *hi);
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Date32Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Date32Array".to_owned(),
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
            GenPqPred::Date32Le { hi, .. } => {
                let hi_v = *hi;
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Date32Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Date32Array".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) <= hi_v))
                        .collect();
                    Ok(bools)
                })));
            }
            GenPqPred::Dec128Between { lo, hi, .. } => {
                let (lo_v, hi_v) = (*lo, *hi);
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Decimal128Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Decimal128Array".to_owned(),
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
            GenPqPred::Dec128Lt { hi, .. } => {
                let hi_v = *hi;
                row_preds.push(Box::new(ArrowPredicateFn::new(mask, move |batch| {
                    let arr = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Decimal128Array>()
                        .ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected Decimal128Array".to_owned(),
                            )
                        })?;
                    let bools: BooleanArray = (0..arr.len())
                        .map(|i| (!arr.is_null(i)).then(|| arr.value(i) < hi_v))
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

fn execute_parquet_row_selection_batches<R: ChunkReader + 'static>(
    mut builder: ParquetRecordBatchReaderBuilder<R>,
    sorted_indices: &[u64],
    proj_roots: Option<&[usize]>,
) -> BenchResult<Vec<RecordBatch>> {
    let meta = Arc::clone(builder.metadata());
    let parquet_schema = meta.file_metadata().schema_descr_ptr();
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
        return Ok(Vec::new());
    }

    builder = builder
        .with_row_groups(rgs)
        .with_row_selection(RowSelection::from(selectors))
        .with_batch_size(16_384);
    if let Some(roots) = proj_roots {
        builder = builder.with_projection(ProjectionMask::roots(
            &parquet_schema,
            roots.iter().copied(),
        ));
    }

    let reader = builder.build()?;
    let mut out = Vec::new();
    for b in reader {
        let b = b?;
        if b.num_rows() > 0 {
            out.push(b);
        }
    }
    Ok(out)
}

fn execute_parquet_row_selection<R: ChunkReader + 'static>(
    builder: ParquetRecordBatchReaderBuilder<R>,
    sorted_indices: &[u64],
) -> BenchResult<usize> {
    let batches = execute_parquet_row_selection_batches(builder, sorted_indices, None)?;
    Ok(batches.iter().map(RecordBatch::num_rows).sum())
}

async fn execute_gen_parquet_scan_async(
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

async fn execute_parquet_row_selection_batches_async(
    mut builder: ParquetRecordBatchStreamBuilder<BenchS3ParquetAsyncReader>,
    sorted_indices: &[u64],
    proj_roots: Option<&[usize]>,
) -> BenchResult<Vec<RecordBatch>> {
    let meta = Arc::clone(builder.metadata());
    let parquet_schema = meta.file_metadata().schema_descr_ptr();
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
        return Ok(Vec::new());
    }

    builder = builder
        .with_row_groups(rgs)
        .with_row_selection(RowSelection::from(selectors))
        .with_batch_size(16_384);
    if let Some(roots) = proj_roots {
        builder = builder.with_projection(ProjectionMask::roots(
            &parquet_schema,
            roots.iter().copied(),
        ));
    }

    let stream = builder.build()?;
    let batches: Vec<RecordBatch> = stream.try_collect().await?;
    Ok(batches.into_iter().filter(|b| b.num_rows() > 0).collect())
}

async fn execute_parquet_row_selection_async(
    builder: ParquetRecordBatchStreamBuilder<BenchS3ParquetAsyncReader>,
    sorted_indices: &[u64],
) -> BenchResult<usize> {
    let batches =
        execute_parquet_row_selection_batches_async(builder, sorted_indices, None).await?;
    Ok(batches.iter().map(RecordBatch::num_rows).sum())
}

// ============================================================================
// Shared Arrow / Vortex / Cetra Value Extraction Helpers
// ============================================================================

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

fn sum_arrow59_binary_bytes(
    batches: &[RecordBatch],
    col_name: &str,
) -> BenchResult<(usize, usize)> {
    let mut rows = 0usize;
    let mut bytes = 0usize;
    for b in batches {
        rows += b.num_rows();
        let arr = b
            .column_by_name(col_name)
            .ok_or_else(|| format!("missing binary column '{col_name}'"))?;
        if let Some(ba) = arr.as_any().downcast_ref::<BinaryArray>() {
            for i in 0..ba.len() {
                if !ba.is_null(i) {
                    bytes += ba.value(i).len();
                }
            }
        } else if let Some(ba) = arr.as_any().downcast_ref::<LargeBinaryArray>() {
            for i in 0..ba.len() {
                if !ba.is_null(i) {
                    bytes += ba.value(i).len();
                }
            }
        } else if let Some(ba) = arr.as_any().downcast_ref::<BinaryViewArray>() {
            for i in 0..ba.len() {
                if !ba.is_null(i) {
                    bytes += ba.value(i).len();
                }
            }
        } else {
            return Err(format!(
                "expected Binary array for '{col_name}', got {}",
                arr.data_type()
            )
            .into());
        }
    }
    Ok((rows, bytes))
}

fn sum_arrow60_binary_bytes(batch: &RecordBatch60, col_name: &str) -> BenchResult<usize> {
    let arr = batch
        .column_by_name(col_name)
        .ok_or_else(|| format!("missing arrow60 binary column '{col_name}'"))?;
    let mut bytes = 0usize;
    if let Some(ba) = arr.as_any().downcast_ref::<BinaryArray60>() {
        for i in 0..Array60::len(ba) {
            if !Array60::is_null(ba, i) {
                bytes += ba.value(i).len();
            }
        }
    } else if let Some(ba) = arr.as_any().downcast_ref::<LargeBinaryArray60>() {
        for i in 0..Array60::len(ba) {
            if !Array60::is_null(ba, i) {
                bytes += ba.value(i).len();
            }
        }
    } else if let Some(ba) = arr.as_any().downcast_ref::<BinaryViewArray60>() {
        for i in 0..Array60::len(ba) {
            if !Array60::is_null(ba, i) {
                bytes += ba.value(i).len();
            }
        }
    } else {
        return Err(format!(
            "expected arrow60 Binary array for '{col_name}', got {}",
            arr.data_type()
        )
        .into());
    }
    Ok(bytes)
}

fn sum_cetra_binary_bytes(batches: &[Batch], col_idx: usize) -> BenchResult<(usize, usize)> {
    let mut rows = 0usize;
    let mut bytes = 0usize;
    for b in batches {
        let compacted = if b.selection().is_all(b.raw_len()) {
            None
        } else {
            Some(b.compact(None)?)
        };
        let b_ref = compacted.as_ref().unwrap_or(b);
        rows += b_ref.num_rows();
        let flat = b_ref
            .column(col_idx)
            .ok_or("missing binary column in batch")?
            .materialize(None)?;
        if let FlatData::Binary(bin) = flat.data() {
            let nulls = flat.nulls();
            for i in 0..bin.len() {
                if nulls.is_none_or(|bm| bm.get(i))
                    && let Some(slice) = bin.get(i)
                {
                    bytes += slice.len();
                }
            }
        } else {
            return Err(format!("expected FlatData::Binary at col {col_idx}").into());
        }
    }
    Ok((rows, bytes))
}

fn eval_arrow_map_key_eq_mask(
    batch: &RecordBatch,
    col_name: &str,
    key: &str,
    val: &str,
) -> Vec<bool> {
    let Some(ma) = batch
        .column_by_name(col_name)
        .and_then(|c| c.as_any().downcast_ref::<MapArray>())
    else {
        return vec![false; batch.num_rows()];
    };
    let keys = ma.keys();
    let vals = ma.values();
    let offsets = ma.value_offsets();
    let mut mask = vec![false; ma.len()];
    for (r, slot) in mask.iter_mut().enumerate() {
        if ma.is_null(r) {
            continue;
        }
        let s = offsets[r] as usize;
        let e = offsets[r + 1] as usize;
        for j in s..e {
            if extract_str_at(keys.as_ref(), j) == Some(key)
                && extract_str_at(vals.as_ref(), j) == Some(val)
            {
                *slot = true;
                break;
            }
        }
    }
    mask
}

fn eval_vortex_map_key_eq(b: &RecordBatch60, col_name: &str, key: &str, val: &str) -> usize {
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

#[derive(Clone, Copy)]
enum VortexPostFilter<'a> {
    None,
    Like {
        col: &'a str,
        needle: &'a str,
    },
    MapKeyEq {
        col: &'a str,
        key: &'a str,
        val: &'a str,
    },
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
            VortexPostFilter::MapKeyEq { col, key, val } => {
                total += eval_vortex_map_key_eq(&batch, col, key, val);
            }
        }
    }
    Ok(total)
}

async fn execute_vortex_take_count(
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

async fn execute_vortex_take_binary_bytes(
    session: &VortexSession,
    file: &VortexFile,
    sorted_indices: &[u64],
    blob_col: &str,
) -> BenchResult<(usize, usize)> {
    let idx_buf = vortex::scan::strict_sorted_buffer::StrictSortedBuffer::try_new(
        Buffer::from_iter(sorted_indices.iter().copied()),
    )?;
    let projection = root().bind(file.dtype())?.optimize_recursive()?;
    let mut stream = file
        .scan()?
        .with_projection(projection)
        .with_row_indices(idx_buf)
        .into_array_stream()?;
    let mut rows = 0usize;
    let mut bytes = 0usize;
    while let Some(chunk) = stream.try_next().await? {
        if chunk.is_empty() {
            continue;
        }
        let batch = chunk_to_record_batch(session, chunk)?;
        rows += batch.num_rows();
        bytes += sum_arrow60_binary_bytes(&batch, blob_col)?;
    }
    Ok((rows, bytes))
}

fn date32_vortex_scalar(session: &VortexSession, days: i32) -> BenchResult<Scalar> {
    let arr: ArrayRef60 = Arc::new(Date32Array60::from(vec![days]));
    let field = Field60::new("", DataType60::Date32, false);
    let vx_arr = session.arrow().from_arrow_array(arr, &field)?;
    let mut ctx = session.create_execution_ctx();
    Ok(vx_arr.execute_scalar(0, &mut ctx)?)
}

fn dec128_vortex_scalar(
    session: &VortexSession,
    val: i128,
    precision: u8,
    scale: i8,
) -> BenchResult<Scalar> {
    let arr: ArrayRef60 =
        Arc::new(Decimal128Array60::from(vec![val]).with_precision_and_scale(precision, scale)?);
    let field = Field60::new("", DataType60::Decimal128(precision, scale), false);
    let vx_arr = session.arrow().from_arrow_array(arr, &field)?;
    let mut ctx = session.create_execution_ctx();
    Ok(vx_arr.execute_scalar(0, &mut ctx)?)
}

fn scattered_row_indices(total_rows: usize, count: usize, seed: u64) -> Vec<u64> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut set = BTreeSet::new();
    while set.len() < count.min(total_rows) {
        set.insert(rng.random_range(0..total_rows) as u64);
    }
    set.into_iter().collect()
}

fn execute_statam_scan(reader: &StatamReader, req: &ScanRequest) -> BenchResult<usize> {
    let batches = reader.scan(req)?;
    Ok(batches.iter().map(Batch::num_rows).sum())
}

// ============================================================================
// Generic Query Runners (with clean per-format error isolation)
// ============================================================================

#[allow(clippy::too_many_arguments)]
async fn run_parquet_scan_query(
    wl: &str,
    qname: &str,
    files: &EncodedSuiteFiles,
    pq_proj: &[usize],
    pq_preds: &[GenPqPred],
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let s3_only = s3_only_mode();
    for (fmt, bytes_opt, with_pi) in [
        ("pq_default", &files.pq_default, false),
        ("pq_tuned", &files.pq_tuned, true),
        ("pq_cetra", &files.pq_cetra, true),
    ] {
        let Some(bytes) = bytes_opt else { continue };
        let res: BenchResult<QueryBenchRow> = async {
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
                        execute_gen_parquet_scan_with_builder(bytes, builder, pq_proj, pq_preds)
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
                    let m = execute_gen_parquet_scan_async(&s3_reader, with_pi, pq_proj, pq_preds)
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
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            Ok(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: s3_matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            })
        }
        .await;

        match res {
            Ok(row) => {
                check_parity(expected_matches, wl, qname, fmt, row.rows_matched)?;
                query_rows.push(row);
            }
            Err(e) => {
                notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_vortex_scan_query(
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
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let res: BenchResult<QueryBenchRow> = async {
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
                let m = execute_vortex_scan_with_post(vortex_session, &s3_file, proj, filt, post)
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
        Ok(QueryBenchRow {
            workload: wl.to_owned(),
            query: qname.to_owned(),
            format: "vortex".to_owned(),
            rows_matched: s3_matched,
            local_ms,
            local_p95_ms,
            s3_ms,
            s3_gets,
            s3_bytes,
            cold_open_gets: None,
            cold_open_tail_bytes: None,
        })
    }
    .await;

    match res {
        Ok(row) => {
            check_parity(expected_matches, wl, qname, "vortex", row.rows_matched)?;
            query_rows.push(row);
        }
        Err(e) => {
            notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})"));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_lance_query_generic<F>(
    wl: &str,
    qname: &str,
    bytes: &Bytes,
    local_reader: &LanceFileReader,
    exec: F,
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()>
where
    F: AsyncFn(&LanceFileReader) -> BenchResult<usize>,
{
    let res: BenchResult<QueryBenchRow> = async {
        let (local_matched_opt, local_ms, local_p95_ms) = if s3_only_mode() {
            (None, 0.0, 0.0)
        } else {
            let runs = warm_runs.max(1);
            let mut samples = Vec::with_capacity(runs);
            let mut matched = 0usize;
            for i in 0..=runs {
                let t0 = Instant::now();
                let n = exec(local_reader).await?;
                let ms = t0.elapsed().as_secs_f64() * 1_000.0;
                if i == 0 {
                    matched = n;
                } else {
                    if n != matched {
                        return Err(format!("lance {qname}: warmup {matched} != run {n}").into());
                    }
                    samples.push(ms);
                }
            }
            let (local_ms, local_p95_ms) = summarize_timings(&mut samples);
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
        Ok(QueryBenchRow {
            workload: wl.to_owned(),
            query: qname.to_owned(),
            format: "lance".to_owned(),
            rows_matched: s3_matched,
            local_ms,
            local_p95_ms,
            s3_ms,
            s3_gets,
            s3_bytes,
            cold_open_gets: None,
            cold_open_tail_bytes: None,
        })
    }
    .await;

    match res {
        Ok(row) => {
            check_parity(expected_matches, wl, qname, "lance", row.rows_matched)?;
            query_rows.push(row);
        }
        Err(e) => {
            notes.push(format!("{wl} `{qname}` `lance` = N/A ({e})"));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_strata_query_generic<F, Fut>(
    wl: &str,
    qname: &str,
    bytes: &Bytes,
    local_file: &Arc<StrataFile>,
    exec: F,
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()>
where
    F: Fn(Arc<StrataFile>) -> Fut,
    Fut: std::future::Future<Output = BenchResult<usize>>,
{
    let res: BenchResult<QueryBenchRow> = async {
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
        Ok(QueryBenchRow {
            workload: wl.to_owned(),
            query: qname.to_owned(),
            format: "strata".to_owned(),
            rows_matched: s3_matched,
            local_ms,
            local_p95_ms,
            s3_ms,
            s3_gets,
            s3_bytes,
            cold_open_gets: None,
            cold_open_tail_bytes: None,
        })
    }
    .await;

    match res {
        Ok(row) => {
            check_parity(expected_matches, wl, qname, "strata", row.rows_matched)?;
            query_rows.push(row);
        }
        Err(e) => {
            notes.push(format!("{wl} `{qname}` `strata` = N/A ({e})"));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_statam_scan_query(
    wl: &str,
    qname: &str,
    files: &EncodedSuiteFiles,
    req: &ScanRequest,
    warm_runs: usize,
    s3_cfg: SimulatedS3Config,
    expected_matches: &mut Option<usize>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let s3_only = s3_only_mode();
    for (fmt, bytes_opt, pack_opt) in [
        ("statam_l0", &files.statam_l0, None),
        ("statam_l1", &files.statam_l1, files.statam_l1_pack.as_ref()),
    ] {
        let Some(bytes) = bytes_opt else { continue };
        let res: BenchResult<QueryBenchRow> = async {
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let local_reader = open_statam_local(bytes, pack_opt)?;
                let (m, med, p95) =
                    measure_local_ms(|| execute_statam_scan(&local_reader, req), warm_runs)?;
                (Some(m), med, p95)
            };

            let (s3_file, s3_pack, s3_resolver) = create_statam_s3_readers(bytes, pack_opt, s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_file = Arc::clone(&s3_file);
                let s3_pack = s3_pack.clone();
                let s3_resolver = s3_resolver.clone();
                async move {
                    let s3_reader = open_statam_from_bench_s3(
                        &s3_file,
                        s3_pack.as_ref(),
                        s3_resolver.as_ref(),
                    )?;
                    let m = execute_statam_scan(&s3_reader, req)?;
                    let (ms, gets, bytes) = statam_s3_totals(&s3_file, s3_pack.as_ref());
                    Ok((m, gets, bytes, ms))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            Ok(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: s3_matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            })
        }
        .await;

        match res {
            Ok(row) => {
                check_parity(expected_matches, wl, qname, fmt, row.rows_matched)?;
                query_rows.push(row);
            }
            Err(e) => {
                notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_take_query_all_formats(
    wl: &str,
    qname: &str,
    files: &EncodedSuiteFiles,
    vortex_session: &VortexSession,
    vortex_local: Option<&VortexFile>,
    lance_local: Option<&LanceFileReader>,
    indices: &[u64],
    warm_runs: usize,
    read_threads: usize,
    s3_cfg: SimulatedS3Config,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let mut expected_matches: Option<usize> = Some(indices.len());
    let s3_only = s3_only_mode();

    // Parquet
    for (fmt, bytes_opt, with_pi) in [
        ("pq_default", &files.pq_default, false),
        ("pq_tuned", &files.pq_tuned, true),
        ("pq_cetra", &files.pq_cetra, true),
    ] {
        let Some(bytes) = bytes_opt else { continue };
        let res: BenchResult<QueryBenchRow> = async {
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
            Ok(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: s3_matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            })
        }
        .await;

        match res {
            Ok(row) => {
                check_parity(&mut expected_matches, wl, qname, fmt, row.rows_matched)?;
                query_rows.push(row);
            }
            Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
        }
    }

    // Vortex
    if let (Some(bytes), Some(local_file)) = (&files.vortex, vortex_local) {
        let res: BenchResult<QueryBenchRow> = async {
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let (m, med, p95) = measure_local_async_ms(
                    || execute_vortex_take_count(vortex_session, local_file, indices),
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
                    let m = execute_vortex_take_count(vortex_session, &s3_file, indices).await?;
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
            Ok(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                rows_matched: s3_matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            })
        }
        .await;

        match res {
            Ok(row) => {
                check_parity(&mut expected_matches, wl, qname, "vortex", row.rows_matched)?;
                query_rows.push(row);
            }
            Err(e) => notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})")),
        }
    }

    // Lance
    if let (Some(bytes), Some(local_reader)) = (&files.lance, lance_local) {
        let u32_indices: Vec<u32> = indices.iter().map(|&i| i as u32).collect();
        let u32_ref = u32_indices.as_slice();
        run_lance_query_generic(
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
            notes,
        )
        .await?;
    }

    // Statam
    for (fmt, bytes_opt, pack_opt) in [
        ("statam_l0", &files.statam_l0, None),
        ("statam_l1", &files.statam_l1, files.statam_l1_pack.as_ref()),
    ] {
        let Some(bytes) = bytes_opt else { continue };
        let res: BenchResult<QueryBenchRow> = async {
            let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                (None, 0.0, 0.0)
            } else {
                let local_reader = open_statam_local(bytes, pack_opt)?;
                let (m, med, p95) = measure_local_ms(
                    || {
                        let batches = local_reader.take_rows(indices, None, read_threads)?;
                        Ok(batches.iter().map(Batch::num_rows).sum())
                    },
                    warm_runs,
                )?;
                (Some(m), med, p95)
            };

            let (s3_file, s3_pack, s3_resolver) = create_statam_s3_readers(bytes, pack_opt, s3_cfg);
            let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                let s3_file = Arc::clone(&s3_file);
                let s3_pack = s3_pack.clone();
                let s3_resolver = s3_resolver.clone();
                async move {
                    let s3_reader = open_statam_from_bench_s3(
                        &s3_file,
                        s3_pack.as_ref(),
                        s3_resolver.as_ref(),
                    )?;
                    let s3_batches = s3_reader.take_rows(indices, None, read_threads)?;
                    let m: usize = s3_batches.iter().map(Batch::num_rows).sum();
                    let (ms, gets, bytes) = statam_s3_totals(&s3_file, s3_pack.as_ref());
                    Ok((m, gets, bytes, ms))
                }
            })
            .await?;
            if let Some(lm) = local_matched_opt
                && s3_matched != lm
            {
                return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
            }
            Ok(QueryBenchRow {
                workload: wl.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                rows_matched: s3_matched,
                local_ms,
                local_p95_ms,
                s3_ms,
                s3_gets,
                s3_bytes,
                cold_open_gets: None,
                cold_open_tail_bytes: None,
            })
        }
        .await;

        match res {
            Ok(row) => {
                check_parity(&mut expected_matches, wl, qname, fmt, row.rows_matched)?;
                query_rows.push(row);
            }
            Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
        }
    }

    Ok(())
}

// ============================================================================
// Suite G: `blobs` — Multimodal Blobs ("Lance Blob V2" style)
// ============================================================================

fn fill_high_entropy(dst: &mut [u8], seed: u64) {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut pos = 0usize;
    while pos + 8 <= dst.len() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let word = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        dst[pos..pos + 8].copy_from_slice(&word);
        pos += 8;
    }
    if pos < dst.len() {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let word = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes();
        let rem = dst.len() - pos;
        dst[pos..].copy_from_slice(&word[..rem]);
    }
}

fn fill_compressible_blob(dst: &mut [u8], row: usize, is_pdf: bool) {
    let header: &[u8] = if is_pdf {
        b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n"
    } else {
        b"ID3\x03\x00\x00\x00\x00\x00\x20{\"codec\":\"mp3\",\"bitrate\":192,\"frames\":["
    };
    let hlen = header.len().min(dst.len());
    dst[..hlen].copy_from_slice(&header[..hlen]);

    let pattern = if is_pdf {
        format!(
            "{{\"doc\":{row},\"section\":\"llm-eval-report\",\"page\":{},\"text\":\"The quick brown fox jumps over the lazy dog while evaluating trace latency and throughput.\"}}\n",
            row % 64
        )
    } else {
        format!(
            "{{\"track\":{row},\"frame\":{},\"pcm_tag\":\"audio-segment-normal-gain-0db\",\"crc\":{}}}\n",
            row % 128,
            (row * 31) % 997
        )
    };
    let pbytes = pattern.as_bytes();
    let mut pos = hlen;
    let mut block_idx = 0u8;
    while pos < dst.len() {
        let take = pbytes.len().min(dst.len() - pos);
        dst[pos..pos + take].copy_from_slice(&pbytes[..take]);
        if take > 4 {
            dst[pos + take - 2] = b'0' + (block_idx % 10);
        }
        pos += take;
        block_idx = block_idx.wrapping_add(1);
    }
}

fn build_blobs_batch(num_rows: usize) -> BenchResult<(RecordBatch, Vec<usize>)> {
    let mut rng = SmallRng::seed_from_u64(707);
    let mimes = ["image/png", "image/jpeg", "application/pdf", "audio/mpeg"];

    let mut row_id = Vec::with_capacity(num_rows);
    let mut ts = Vec::with_capacity(num_rows);
    let mut mime_b = StringBuilder::with_capacity(num_rows, num_rows * 16);
    let mut trace_b = StringBuilder::with_capacity(num_rows, num_rows * 14);
    let mut blob_size = Vec::with_capacity(num_rows);
    let mut blob_lens = Vec::with_capacity(num_rows);
    let mut blob_b = BinaryBuilder::with_capacity(num_rows, num_rows * 184 * 1024);

    let mut scratch = vec![0u8; 320 * 1024];
    for i in 0..num_rows {
        let mime = mimes[i % 4];
        let len: usize = rng.random_range((48 * 1024)..=(320 * 1024));
        let buf = &mut scratch[..len];
        match i % 4 {
            0 => {
                // Incompressible pseudo-PNG
                let png_magic = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
                buf[..png_magic.len()].copy_from_slice(png_magic);
                fill_high_entropy(
                    &mut buf[png_magic.len()..],
                    (i as u64 + 1) * 0x517C_C1B7_2722_0A95,
                );
            }
            1 => {
                // Incompressible pseudo-JPEG
                let jpg_magic = b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01";
                buf[..jpg_magic.len()].copy_from_slice(jpg_magic);
                fill_high_entropy(
                    &mut buf[jpg_magic.len()..len - 2],
                    (i as u64 + 1) * 0x94D0_49BB_1331_11EB,
                );
                buf[len - 2] = 0xFF;
                buf[len - 1] = 0xD9;
            }
            2 => fill_compressible_blob(buf, i, true),
            _ => fill_compressible_blob(buf, i, false),
        }

        row_id.push(i as i64);
        ts.push(DAY_START + (i as i64) * 1_000_000_000);
        mime_b.append_value(mime);
        trace_b.append_value(format!("trace-{i:06}"));
        blob_size.push(len as i64);
        blob_lens.push(len);
        blob_b.append_value(buf);
    }

    let schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("row_id", DataType::Int64, false),
        Field::new("ts", DataType::Int64, false),
        Field::new("mime_type", DataType::Utf8, false),
        Field::new("trace_id", DataType::Utf8, false),
        Field::new("blob_size", DataType::Int64, false),
        Field::new("blob", DataType::Binary, false),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(row_id)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(mime_b.finish()),
            Arc::new(trace_b.finish()),
            Arc::new(Int64Array::from(blob_size)),
            Arc::new(blob_b.finish()),
        ],
    )?;
    Ok((batch, blob_lens))
}

#[allow(clippy::too_many_arguments)]
async fn run_blobs_suite(
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let num_rows = 2_000usize;
    let (arrow_batch, blob_lens) = build_blobs_batch(num_rows)?;
    let cetra_batch = arrow_to_cetra_batch(&arrow_batch)?;
    let raw_bytes = arrow_batch.get_array_memory_size() as u64;
    let wl = "blobs_2k";

    let total_blob_mib: f64 = blob_lens.iter().sum::<usize>() as f64 / (1024.0 * 1024.0);
    notes.push(format!(
        "Suite G (`blobs`): {num_rows} rows x 6 cols, {total_blob_mib:.1} MiB binary blobs (50% incompressible PNG/JPEG, 50% compressible PDF/audio)"
    ));

    let enc_opts = SuiteEncodeOpts {
        wl,
        suite_label: "Suite G (`blobs`)",
        bloom_cols: &["trace_id"],
        hot_bloom_col: Some("trace_id"),
        sort_col_names: &["row_id"],
        statam_sort_cols: &[0],
        narrow_trigram_cols: &[],
        payload_cols: &["blob"],
        l1_use_pack: true,
        tuned_rg_rows: 256,
        pq_step_rows: 256,
        write_threads,
    };
    let files = encode_all_formats(
        &arrow_batch,
        Ok(&cetra_batch),
        raw_bytes,
        enabled,
        vortex_session,
        &enc_opts,
        write_rows,
        notes,
    )
    .await;

    if let Some(l1) = &files.statam_l1
        && let Ok(r) = open_statam_local(l1, files.statam_l1_pack.as_ref())
    {
        let pack_kib = files
            .statam_l1_pack
            .as_ref()
            .map_or(0, |p| p.len() as u64 / 1024);
        notes.push(format!(
            "Suite G (`blobs`): `statam_l1` wrote {} row group(s) with `.stpk` external blob pack ({pack_kib} KiB)",
            r.meta().rg_dir.len()
        ));
    }

    let s3_cfg = SimulatedS3Config::default();
    let vortex_local = match &files.vortex {
        Some(b) => match open_vortex_local(vortex_session, std::slice::from_ref(b)).await {
            Ok(mut v) => v.pop(),
            Err(e) => {
                notes.push(format!("Suite G (`blobs`): `vortex` open failed ({e})"));
                None
            }
        },
        None => None,
    };
    let lance_local = match &files.lance {
        Some(b) => match open_lance_local(b).await {
            Ok(r) => Some(r),
            Err(e) => {
                notes.push(format!("Suite G (`blobs`): `lance` open failed ({e})"));
                None
            }
        },
        None => None,
    };
    let strata_local = match &files.strata {
        Some(b) => match open_single_strata_local(b).await {
            Ok(f) => Some(f),
            Err(e) => {
                notes.push(format!("Suite G (`blobs`): `strata` open failed ({e})"));
                None
            }
        },
        None => None,
    };

    // Query 1: BL_meta_scan — filter mime_type = 'application/pdf', project [row_id, mime_type, blob_size]
    {
        let qname = "BL_meta_scan";
        let mut expected: Option<usize> = None;
        let pq_proj = [0usize, 2, 4];
        let pq_preds = [GenPqPred::Utf8Eq {
            col: "mime_type".to_owned(),
            val: "application/pdf".to_owned(),
        }];
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let proj = select(["row_id", "mime_type", "blob_size"], root());
            let filt = eq(col("mime_type"), lit("application/pdf"));
            run_vortex_scan_query(
                wl,
                qname,
                vortex_session,
                bytes,
                local_f,
                &proj,
                Some(&filt),
                VortexPostFilter::None,
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches =
                        lance_scan_projected(r, Some(&["row_id", "mime_type", "blob_size"]))
                            .await?;
                    let mut matched = 0usize;
                    for b in &batches {
                        if let Some(m) = b.column_by_name("mime_type") {
                            for i in 0..b.num_rows() {
                                if extract_str_at(m.as_ref(), i) == Some("application/pdf") {
                                    matched += 1;
                                }
                            }
                        }
                    }
                    Ok(matched)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                |f| async move {
                    let req = StrataScanRequest {
                        projection: vec![0, 2, 4],
                        predicate: Some(StrataPred::Cmp(
                            2,
                            CmpOp::Eq,
                            Lit::Utf8("application/pdf".to_owned()),
                        )),
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Scan,
                        views: false,
                        top: None,
                        row_filter: None,
                    };
                    let batches = f.scan(&req).await?;
                    Ok(batches.iter().map(RecordBatch::num_rows).sum())
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: Some(vec![0, 2, 4]),
            predicates: vec![StatamPred::new(
                "mime_type",
                ZonePred::Eq(ZoneValue::Bytes(b"application/pdf".to_vec())),
            )],
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Queries 2..4: BL_take_1, BL_take_10, BL_take_50 — verify BOTH row count AND exact total blob bytes!
    let take_1 = scattered_row_indices(num_rows, 1, 7071);
    let take_10 = scattered_row_indices(num_rows, 10, 7072);
    let take_50 = scattered_row_indices(num_rows, 50, 7073);

    if files.strata.is_some() {
        notes.push(
            "Suite G (`blobs`): `strata` multi-row take (`BL_take_10`, `BL_take_50`) = N/A (scattered `row_indices` take not wired); `BL_take_1` uses `row_id = id` point scan".to_owned(),
        );
    }

    for (qname, indices) in [
        ("BL_take_1", &take_1),
        ("BL_take_10", &take_10),
        ("BL_take_50", &take_50),
    ] {
        let expected_rows = indices.len();
        let expected_blob_bytes: usize = indices.iter().map(|&idx| blob_lens[idx as usize]).sum();
        let mut expected_matches: Option<usize> = Some(expected_rows);

        // Parquet
        let s3_only = s3_only_mode();
        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &files.pq_default, false),
            ("pq_tuned", &files.pq_tuned, true),
            ("pq_cetra", &files.pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = async {
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
                            let batches =
                                execute_parquet_row_selection_batches(builder, indices, None)?;
                            let (r, b_sum) = sum_arrow59_binary_bytes(&batches, "blob")?;
                            if b_sum != expected_blob_bytes {
                                return Err(format!(
                                    "{fmt} {qname}: blob bytes {b_sum} != expected {expected_blob_bytes}"
                                )
                                .into());
                            }
                            Ok(r)
                        },
                        warm_runs,
                    )?;
                    (Some(m), med, p95)
                };

                let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
                let (s3_r, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_reader = Arc::clone(&s3_reader);
                    async move {
                        s3_reader.evict_all();
                        s3_reader.reset_counters();
                        let s3_builder = open_parquet_async_builder(&s3_reader, with_pi).await?;
                        let s3_batches =
                            execute_parquet_row_selection_batches_async(s3_builder, indices, None)
                                .await?;
                        let (r, s3_b) = sum_arrow59_binary_bytes(&s3_batches, "blob")?;
                        if s3_b != expected_blob_bytes {
                            return Err(format!(
                                "{fmt} {qname}: s3 blob bytes {s3_b} != expected {expected_blob_bytes}"
                            )
                            .into());
                        }
                        Ok((
                            r,
                            s3_reader.counters().gets,
                            s3_reader.counters().bytes_from_s3,
                            s3_reader.simulated_latency_ms(),
                        ))
                    }
                })
                .await?;
                if let Some(lm) = local_matched_opt
                    && s3_r != lm
                {
                    return Err(format!("{fmt} {qname}: s3 {s3_r} rows != local {lm} rows").into());
                }
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: s3_r,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;

            match res {
                Ok(row) => {
                    check_parity(&mut expected_matches, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }

        // Vortex
        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let res: BenchResult<QueryBenchRow> = async {
                let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                    (None, 0.0, 0.0)
                } else {
                    let (m, med, p95) = measure_local_async_ms(
                        || async {
                            let (r, b_sum) = execute_vortex_take_binary_bytes(
                                vortex_session,
                                local_f,
                                indices,
                                "blob",
                            )
                            .await?;
                            if b_sum != expected_blob_bytes {
                                return Err(format!(
                                    "vortex {qname}: blob bytes {b_sum} != expected {expected_blob_bytes}"
                                )
                                .into());
                            }
                            Ok(r)
                        },
                        warm_runs,
                    )
                    .await?;
                    (Some(m), med, p95)
                };

                let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
                let (s3_r, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_reader = Arc::clone(&s3_reader);
                    async move {
                        s3_reader.evict_all();
                        s3_reader.reset_counters();
                        let vx_read = Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
                        let s3_file = vortex_session.open_options().open_read(vx_read).await?;
                        let (r, s3_b) = execute_vortex_take_binary_bytes(
                            vortex_session,
                            &s3_file,
                            indices,
                            "blob",
                        )
                        .await?;
                        if s3_b != expected_blob_bytes {
                            return Err(format!(
                                "vortex {qname}: s3 blob bytes {s3_b} != expected {expected_blob_bytes}"
                            )
                            .into());
                        }
                        Ok((
                            r,
                            s3_reader.counters().gets,
                            s3_reader.counters().bytes_from_s3,
                            s3_reader.simulated_latency_ms(),
                        ))
                    }
                })
                .await?;
                if let Some(lm) = local_matched_opt
                    && s3_r != lm
                {
                    return Err(
                        format!("vortex {qname}: s3 {s3_r} rows != local {lm} rows").into(),
                    );
                }
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "vortex".to_owned(),
                    rows_matched: s3_r,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;

            match res {
                Ok(row) => {
                    check_parity(&mut expected_matches, wl, qname, "vortex", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})")),
            }
        }

        // Lance
        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            let u32_indices: Vec<u32> = indices.iter().map(|&i| i as u32).collect();
            let u32_ref = u32_indices.as_slice();
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches = lance_take_indices(r, u32_ref, None).await?;
                    let (rows, b_sum) = sum_arrow59_binary_bytes(&batches, "blob")?;
                    if b_sum != expected_blob_bytes {
                        return Err(format!(
                            "lance {qname}: blob bytes {b_sum} != expected {expected_blob_bytes}"
                        )
                        .into());
                    }
                    Ok(rows)
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
                notes,
            )
            .await?;
        }

        // Strata (BL_take_1 via sorted row_id point lookup)
        if qname == "BL_take_1"
            && let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local)
        {
            let target_id = indices[0] as i64;
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                move |f| async move {
                    let req = StrataScanRequest {
                        projection: (0..6).collect(),
                        predicate: Some(StrataPred::Cmp(0, CmpOp::Eq, Lit::Int(target_id))),
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Point,
                        views: false,
                        top: None,
                        row_filter: None,
                    };
                    let batches = f.scan(&req).await?;
                    let (rows, b_sum) = sum_arrow59_binary_bytes(&batches, "blob")?;
                    if b_sum != expected_blob_bytes {
                        return Err(format!(
                            "strata {qname}: blob bytes {b_sum} != expected {expected_blob_bytes}"
                        )
                        .into());
                    }
                    Ok(rows)
                },
                warm_runs,
                s3_cfg,
                &mut expected_matches,
                query_rows,
                notes,
            )
            .await?;
        }

        // Statam
        for (fmt, bytes_opt, pack_opt) in [
            ("statam_l0", &files.statam_l0, None),
            ("statam_l1", &files.statam_l1, files.statam_l1_pack.as_ref()),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = async {
                let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                    (None, 0.0, 0.0)
                } else {
                    let local_reader = open_statam_local(bytes, pack_opt)?;
                    let (m, med, p95) = measure_local_ms(
                        || {
                            let batches = local_reader.take_rows(indices, None, read_threads)?;
                            let (r, b_sum) = sum_cetra_binary_bytes(&batches, 5)?;
                            if b_sum != expected_blob_bytes {
                                return Err(format!(
                                    "{fmt} {qname}: blob bytes {b_sum} != expected {expected_blob_bytes}"
                                )
                                .into());
                            }
                            Ok(r)
                        },
                        warm_runs,
                    )?;
                    (Some(m), med, p95)
                };

                let (s3_file, s3_pack, s3_resolver) =
                    create_statam_s3_readers(bytes, pack_opt, s3_cfg);
                let (s3_r, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_file = Arc::clone(&s3_file);
                    let s3_pack = s3_pack.clone();
                    let s3_resolver = s3_resolver.clone();
                    async move {
                        let s3_reader = open_statam_from_bench_s3(
                            &s3_file,
                            s3_pack.as_ref(),
                            s3_resolver.as_ref(),
                        )?;
                        let s3_batches = s3_reader.take_rows(indices, None, read_threads)?;
                        let (r, s3_b) = sum_cetra_binary_bytes(&s3_batches, 5)?;
                        if s3_b != expected_blob_bytes {
                            return Err(format!(
                                "{fmt} {qname}: s3 blob bytes {s3_b} != expected {expected_blob_bytes}"
                            )
                            .into());
                        }
                        let (ms, gets, bytes) = statam_s3_totals(&s3_file, s3_pack.as_ref());
                        Ok((r, gets, bytes, ms))
                    }
                })
                .await?;
                if let Some(lm) = local_matched_opt
                    && s3_r != lm
                {
                    return Err(format!("{fmt} {qname}: s3 {s3_r} rows != local {lm} rows").into());
                }
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: s3_r,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;

            match res {
                Ok(row) => {
                    check_parity(&mut expected_matches, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }

        notes.push(format!(
            "Suite G (`blobs`): `{qname}` verified exact blob payload parity = {} KiB across all participating formats",
            format_kib(expected_blob_bytes as u64)
        ));
    }

    Ok(())
}

// ============================================================================
// Suite H: `wide-1000` — Ultra-Wide Sparse Feature Table (25k rows x 1,000 cols, 95% NULL)
// ============================================================================

fn build_wide_1000_batch(num_rows: usize) -> BenchResult<RecordBatch> {
    let mut rng = SmallRng::seed_from_u64(808);

    let mut fields = Vec::with_capacity(1_000);
    let mut cols: Vec<ArrayRef> = Vec::with_capacity(1_000);

    // Core 4 columns
    let row_id: Vec<i64> = (0..num_rows as i64).collect();
    let ts: Vec<i64> = (0..num_rows as i64)
        .map(|i| DAY_START + i * 1_000_000)
        .collect();
    let mut ent_b = StringBuilder::with_capacity(num_rows, num_rows * 10);
    let mut bucket = Vec::with_capacity(num_rows);
    for i in 0..num_rows {
        ent_b.append_value(format!("ent-{i:06}"));
        bucket.push(rng.random_range(0..100i64));
    }

    fields.push(Field::new("row_id", DataType::Int64, false));
    cols.push(Arc::new(Int64Array::from(row_id)));
    fields.push(Field::new("ts", DataType::Int64, false));
    cols.push(Arc::new(Int64Array::from(ts)));
    fields.push(Field::new("entity_id", DataType::Utf8, false));
    cols.push(Arc::new(ent_b.finish()));
    fields.push(Field::new("bucket", DataType::Int64, false));
    cols.push(Arc::new(Int64Array::from(bucket)));

    // 500 Float64 columns: f_000..f_499 (95% NULL)
    for c in 0..500usize {
        let mut fb = Float64Builder::with_capacity(num_rows);
        for _ in 0..num_rows {
            if rng.random_range(0u8..20) == 0 {
                let scaled = rng.random_range(-50_000i64..=50_000) as f64 / 100.0;
                fb.append_value(scaled);
            } else {
                fb.append_null();
            }
        }
        fields.push(Field::new(format!("f_{c:03}"), DataType::Float64, true));
        cols.push(Arc::new(fb.finish()));
    }

    // 300 Int64 columns: i_000..i_299 (95% NULL)
    for c in 0..300usize {
        let mut ib = Int64Builder::with_capacity(num_rows);
        for _ in 0..num_rows {
            if rng.random_range(0u8..20) == 0 {
                ib.append_value(rng.random_range(0..10_000i64));
            } else {
                ib.append_null();
            }
        }
        fields.push(Field::new(format!("i_{c:03}"), DataType::Int64, true));
        cols.push(Arc::new(ib.finish()));
    }

    // 196 Utf8 columns: s_000..s_195 (95% NULL, mix of low-card tags and medium-card strings)
    let low_tags = [
        "alpha", "beta", "gamma", "delta", "prod", "canary", "shadow", "holdout",
    ];
    for c in 0..196usize {
        let mut sb = StringBuilder::with_capacity(num_rows, num_rows / 2);
        let is_low_card = c % 2 == 0;
        for _ in 0..num_rows {
            if rng.random_range(0u8..20) == 0 {
                if is_low_card {
                    sb.append_value(low_tags[rng.random_range(0..low_tags.len())]);
                } else {
                    sb.append_value(format!("v-{:04}", rng.random_range(0..500u16)));
                }
            } else {
                sb.append_null();
            }
        }
        fields.push(Field::new(format!("s_{c:03}"), DataType::Utf8, true));
        cols.push(Arc::new(sb.finish()));
    }

    Ok(RecordBatch::try_new(Arc::new(Schema::new(fields)), cols)?)
}

#[allow(clippy::too_many_arguments)]
async fn run_wide_1000_suite(
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let num_rows = 25_000usize;
    let arrow_batch = build_wide_1000_batch(num_rows)?;
    let cetra_batch = arrow_to_cetra_batch(&arrow_batch)?;
    let raw_bytes = arrow_batch.get_array_memory_size() as u64;
    let wl = "wide_1000_25k";

    notes.push(format!(
        "Suite H (`wide-1000`): {num_rows} rows x {} cols (996 nullable cols at 95% NULL)",
        arrow_batch.num_columns()
    ));

    let enc_opts = SuiteEncodeOpts {
        wl,
        suite_label: "Suite H (`wide-1000`)",
        bloom_cols: &["entity_id"],
        hot_bloom_col: Some("entity_id"),
        sort_col_names: &["row_id"],
        statam_sort_cols: &[0],
        narrow_trigram_cols: &[],
        payload_cols: &[],
        l1_use_pack: false,
        tuned_rg_rows: 8_192,
        pq_step_rows: 8_192,
        write_threads,
    };
    let files = encode_all_formats(
        &arrow_batch,
        Ok(&cetra_batch),
        raw_bytes,
        enabled,
        vortex_session,
        &enc_opts,
        write_rows,
        notes,
    )
    .await;

    let s3_cfg = SimulatedS3Config::default();

    // Query 1: WD_cold_open — open file from scratch + pruned `row_id = -1` scan
    {
        let qname = "WD_cold_open";
        let mut expected: Option<usize> = Some(0);
        let s3_only = s3_only_mode();
        let pq_preds = [GenPqPred::I64Eq {
            col: "row_id".to_owned(),
            val: -1,
        }];

        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &files.pq_default, false),
            ("pq_tuned", &files.pq_tuned, true),
            ("pq_cetra", &files.pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = async {
                let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                    (None, 0.0, 0.0)
                } else {
                    let (m, med, p95) = measure_local_ms(
                        || {
                            let opts =
                                ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                                    PageIndexPolicy::Optional
                                } else {
                                    PageIndexPolicy::Skip
                                });
                            let meta = ArrowReaderMetadata::load(bytes, opts)?;
                            let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(
                                bytes.clone(),
                                meta,
                            );
                            execute_gen_parquet_scan_with_builder(bytes, builder, &[0], &pq_preds)
                        },
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
                        let m = execute_gen_parquet_scan_async(&s3_reader, with_pi, &[0], pq_preds)
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
                    return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
                }
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: s3_matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: Some(s3_gets),
                    cold_open_tail_bytes: Some(s3_bytes),
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }

        if let Some(bytes) = &files.vortex {
            let proj = select(["row_id"], root());
            let filt = eq(col("row_id"), lit(-1i64));
            let res: BenchResult<QueryBenchRow> = async {
                let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                    (None, 0.0, 0.0)
                } else {
                    let (m, med, p95) = measure_local_async_ms(
                        || async {
                            let opened =
                                open_vortex_local(vortex_session, std::slice::from_ref(bytes))
                                    .await?;
                            execute_vortex_scan_with_post(
                                vortex_session,
                                &opened[0],
                                &proj,
                                Some(&filt),
                                VortexPostFilter::None,
                            )
                            .await
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
                        let m = execute_vortex_scan_with_post(
                            vortex_session,
                            &s3_file,
                            proj,
                            Some(filt),
                            VortexPostFilter::None,
                        )
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
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "vortex".to_owned(),
                    rows_matched: s3_matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: Some(s3_gets),
                    cold_open_tail_bytes: Some(s3_bytes),
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, "vortex", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})")),
            }
        }

        if let Some(bytes) = &files.lance {
            let res: BenchResult<QueryBenchRow> = async {
                let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                    (None, 0.0, 0.0)
                } else {
                    let (m, med, p95) = measure_local_async_ms(
                        || async {
                            let r = open_lance_local(bytes).await?;
                            let batches = lance_scan_projected(&r, Some(&["row_id"])).await?;
                            let mut hits = 0usize;
                            for b in &batches {
                                if let Some(ia) = b
                                    .column_by_name("row_id")
                                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                                {
                                    for i in 0..ia.len() {
                                        if !ia.is_null(i) && ia.value(i) == -1 {
                                            hits += 1;
                                        }
                                    }
                                }
                            }
                            Ok(hits)
                        },
                        warm_runs,
                    )
                    .await?;
                    (Some(m), med, p95)
                };

                let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
                let (s3_hits, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_file = Arc::clone(&s3_file);
                    async move {
                        s3_file.evict_all();
                        s3_file.reset_counters();
                        let s3_r = open_lance_from_bench_s3(Arc::clone(&s3_file)).await?;
                        let batches = lance_scan_projected(&s3_r, Some(&["row_id"])).await?;
                        let mut hits = 0usize;
                        for b in &batches {
                            if let Some(ia) = b
                                .column_by_name("row_id")
                                .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                            {
                                for i in 0..ia.len() {
                                    if !ia.is_null(i) && ia.value(i) == -1 {
                                        hits += 1;
                                    }
                                }
                            }
                        }
                        Ok((
                            hits,
                            s3_file.counters().gets,
                            s3_file.counters().bytes_from_s3,
                            s3_file.simulated_latency_ms(),
                        ))
                    }
                })
                .await?;
                if let Some(lm) = local_matched_opt
                    && s3_hits != lm
                {
                    return Err(format!("lance {qname}: s3 {s3_hits} != local {lm}").into());
                }
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "lance".to_owned(),
                    rows_matched: s3_hits,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: Some(s3_gets),
                    cold_open_tail_bytes: Some(s3_bytes),
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, "lance", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `lance` = N/A ({e})")),
            }
        }

        if let Some(bytes) = &files.strata {
            let res: BenchResult<QueryBenchRow> = async {
                let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                    (None, 0.0, 0.0)
                } else {
                    let (m, med, p95) = measure_local_async_ms(
                        || async {
                            let f = open_single_strata_local(bytes).await?;
                            let req = StrataScanRequest {
                                projection: vec![0],
                                predicate: Some(StrataPred::Cmp(0, CmpOp::Eq, Lit::Int(-1))),
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Point,
                                views: false,
                                top: None,
                                row_filter: None,
                            };
                            let batches = f.scan(&req).await?;
                            Ok(batches.iter().map(RecordBatch::num_rows).sum())
                        },
                        warm_runs,
                    )
                    .await?;
                    (Some(m), med, p95)
                };

                let s3_r = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
                let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_r = Arc::clone(&s3_r);
                    async move {
                        s3_r.evict_all();
                        s3_r.reset_counters();
                        let s3_f = open_single_strata_from_bench_s3(Arc::clone(&s3_r)).await?;
                        let req = StrataScanRequest {
                            projection: vec![0],
                            predicate: Some(StrataPred::Cmp(0, CmpOp::Eq, Lit::Int(-1))),
                            row_ranges: None,
                            limit: None,
                            kind: ScanKind::Point,
                            views: false,
                            top: None,
                            row_filter: None,
                        };
                        let s3_batches = s3_f.scan(&req).await?;
                        let m: usize = s3_batches.iter().map(RecordBatch::num_rows).sum();
                        Ok((
                            m,
                            s3_r.counters().gets,
                            s3_r.counters().bytes_from_s3,
                            s3_r.simulated_latency_ms(),
                        ))
                    }
                })
                .await?;
                if let Some(lm) = local_matched_opt
                    && s3_matched != lm
                {
                    return Err(format!("strata {qname}: s3 {s3_matched} != local {lm}").into());
                }
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "strata".to_owned(),
                    rows_matched: s3_matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: Some(s3_gets),
                    cold_open_tail_bytes: Some(s3_bytes),
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, "strata", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `strata` = N/A ({e})")),
            }
        }

        let statam_cold_req = ScanRequest {
            projection: Some(vec![0]),
            predicates: vec![StatamPred::new("row_id", ZonePred::Eq(ZoneValue::I64(-1)))],
            read_threads,
            ..ScanRequest::default()
        };
        for (fmt, bytes_opt) in [
            ("statam_l0", &files.statam_l0),
            ("statam_l1", &files.statam_l1),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = async {
                let (local_matched_opt, local_ms, local_p95_ms) = if s3_only {
                    (None, 0.0, 0.0)
                } else {
                    let (m, med, p95) = measure_local_ms(
                        || {
                            let r = StatamReader::open(
                                Arc::new(SliceReader::new(bytes.clone())),
                                None,
                            )?;
                            execute_statam_scan(&r, &statam_cold_req)
                        },
                        warm_runs,
                    )?;
                    (Some(m), med, p95)
                };

                let (s3_file, s3_pack, s3_resolver) = create_statam_s3_readers(bytes, None, s3_cfg);
                let (s3_matched, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_file = Arc::clone(&s3_file);
                    let s3_pack = s3_pack.clone();
                    let s3_resolver = s3_resolver.clone();
                    let statam_cold_req = &statam_cold_req;
                    async move {
                        let s3_r = open_statam_from_bench_s3(
                            &s3_file,
                            s3_pack.as_ref(),
                            s3_resolver.as_ref(),
                        )?;
                        let m = execute_statam_scan(&s3_r, statam_cold_req)?;
                        let (ms, gets, bytes) = statam_s3_totals(&s3_file, None);
                        Ok((m, gets, bytes, ms))
                    }
                })
                .await?;
                if let Some(lm) = local_matched_opt
                    && s3_matched != lm
                {
                    return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {lm}").into());
                }
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: s3_matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: Some(s3_gets),
                    cold_open_tail_bytes: Some(s3_bytes),
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }
    }

    // Open warm local readers for Queries 2..4
    let vortex_local = match &files.vortex {
        Some(b) => open_vortex_local(vortex_session, std::slice::from_ref(b))
            .await
            .ok()
            .and_then(|mut v| v.pop()),
        None => None,
    };
    let lance_local = match &files.lance {
        Some(b) => open_lance_local(b).await.ok(),
        None => None,
    };
    let strata_local = match &files.strata {
        Some(b) => open_single_strata_local(b).await.ok(),
        None => None,
    };

    // Query 2: WD_narrow_3col — filter bucket = 42, project [row_id, bucket, f_000]
    {
        let qname = "WD_narrow_3col";
        let mut expected: Option<usize> = None;
        let pq_proj = [0usize, 3, 4];
        let pq_preds = [GenPqPred::I64Eq {
            col: "bucket".to_owned(),
            val: 42,
        }];
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let proj = select(["row_id", "bucket", "f_000"], root());
            let filt = eq(col("bucket"), lit(42i64));
            run_vortex_scan_query(
                wl,
                qname,
                vortex_session,
                bytes,
                local_f,
                &proj,
                Some(&filt),
                VortexPostFilter::None,
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches =
                        lance_scan_projected(r, Some(&["row_id", "bucket", "f_000"])).await?;
                    let mut hits = 0usize;
                    for b in &batches {
                        if let Some(ia) = b
                            .column_by_name("bucket")
                            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                        {
                            for i in 0..ia.len() {
                                if !ia.is_null(i) && ia.value(i) == 42 {
                                    hits += 1;
                                }
                            }
                        }
                    }
                    Ok(hits)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                |f| async move {
                    let req = StrataScanRequest {
                        projection: vec![0, 3, 4],
                        predicate: Some(StrataPred::Cmp(3, CmpOp::Eq, Lit::Int(42))),
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Scan,
                        views: false,
                        top: None,
                        row_filter: None,
                    };
                    let batches = f.scan(&req).await?;
                    Ok(batches.iter().map(RecordBatch::num_rows).sum())
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: Some(vec![0, 3, 4]),
            predicates: vec![StatamPred::new("bucket", ZonePred::Eq(ZoneValue::I64(42)))],
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Query 3: WD_point_all_1000 — point lookup entity_id = 'ent-012345', project ALL 1,000 cols
    {
        let qname = "WD_point_all_1000";
        let target_ent = "ent-012345";
        let mut expected: Option<usize> = Some(1);
        let all_cols: Vec<usize> = (0..1_000).collect();
        let pq_preds = [GenPqPred::Utf8Eq {
            col: "entity_id".to_owned(),
            val: target_ent.to_owned(),
        }];
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &all_cols,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let proj = root();
            let filt = eq(col("entity_id"), lit(target_ent));
            run_vortex_scan_query(
                wl,
                qname,
                vortex_session,
                bytes,
                local_f,
                &proj,
                Some(&filt),
                VortexPostFilter::None,
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            // Two-phase: scan entity_id only, then take matching row across all 1,000 cols
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let id_batches = lance_scan_projected(r, Some(&["entity_id"])).await?;
                    let mut idxs = Vec::new();
                    let mut row_base = 0u32;
                    for b in &id_batches {
                        if let Some(arr) = b.column_by_name("entity_id") {
                            for i in 0..b.num_rows() {
                                if extract_str_at(arr.as_ref(), i) == Some(target_ent) {
                                    idxs.push(row_base + i as u32);
                                }
                            }
                        }
                        row_base += b.num_rows() as u32;
                    }
                    if idxs.is_empty() {
                        return Ok(0);
                    }
                    let taken = lance_take_indices(r, &idxs, None).await?;
                    Ok(taken.iter().map(RecordBatch::num_rows).sum())
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                |f| async move {
                    let req = StrataScanRequest {
                        projection: (0..1_000).collect(),
                        predicate: Some(StrataPred::Cmp(
                            2,
                            CmpOp::Eq,
                            Lit::Utf8(target_ent.to_owned()),
                        )),
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Point,
                        views: false,
                        top: None,
                        row_filter: None,
                    };
                    let batches = f.scan(&req).await?;
                    Ok(batches.iter().map(RecordBatch::num_rows).sum())
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: None,
            id_lookups: vec![IdLookup::eq("entity_id", target_ent.as_bytes().to_vec())],
            coalesce_mode: CoalesceMode::Point,
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Query 4: WD_take_20_1000 — scattered take 20 rows, project ALL 1,000 cols
    {
        let take_20 = scattered_row_indices(num_rows, 20, 8081);
        if files.strata.is_some() {
            notes.push(
                "Suite H (`wide-1000`): `strata` `WD_take_20_1000` = N/A (scattered `row_indices` take not wired)".to_owned(),
            );
        }
        run_take_query_all_formats(
            wl,
            "WD_take_20_1000",
            &files,
            vortex_session,
            vortex_local.as_ref(),
            lance_local.as_ref(),
            &take_20,
            warm_runs,
            read_threads,
            s3_cfg,
            query_rows,
            notes,
        )
        .await?;
    }

    Ok(())
}

// ============================================================================
// Suite I: `fineweb` — Real FineWeb-Edu English Prose (40,000 rows)
// ============================================================================

enum FineWebScoreFilter {
    IntGe {
        col_idx: usize,
        col_name: String,
        lo: i64,
    },
    F64Ge {
        col_idx: usize,
        col_name: String,
        lo: f64,
    },
}

struct FineWebData {
    arrow_batch: RecordBatch,
    cetra_batch: Batch,
    text_idx: usize,
    id_idx: usize,
    id_col_name: String,
    url_idx: usize,
    url_col_name: String,
    score_filter: FineWebScoreFilter,
    rare_needle: String,
    rare_hits: usize,
}

fn normalize_fineweb_batch(batch: &RecordBatch) -> BenchResult<RecordBatch> {
    let schema = batch.schema();
    let mut out_fields = Vec::with_capacity(schema.fields().len());
    let mut out_cols = Vec::with_capacity(batch.num_columns());

    for (field, col) in schema.fields().iter().zip(batch.columns()) {
        let (target_dt, casted_col): (DataType, ArrayRef) = match field.data_type() {
            DataType::Boolean => (DataType::Boolean, Arc::clone(col)),
            DataType::Int64 => (DataType::Int64, Arc::clone(col)),
            DataType::Float64 => (DataType::Float64, Arc::clone(col)),
            DataType::Utf8 => (DataType::Utf8, Arc::clone(col)),
            DataType::Utf8View | DataType::LargeUtf8 => {
                (DataType::Utf8, arrow::compute::cast(col, &DataType::Utf8)?)
            }
            DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::Date32
            | DataType::Date64
            | DataType::Timestamp(_, _) => (
                DataType::Int64,
                arrow::compute::cast(col, &DataType::Int64)?,
            ),
            DataType::UInt64 => {
                let u64_arr = col
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or("expected UInt64Array")?;
                let mut b = Int64Builder::with_capacity(u64_arr.len());
                for i in 0..u64_arr.len() {
                    if u64_arr.is_null(i) {
                        b.append_null();
                    } else {
                        b.append_value(u64_arr.value(i) as i64);
                    }
                }
                (DataType::Int64, Arc::new(b.finish()))
            }
            DataType::Float32 => (
                DataType::Float64,
                arrow::compute::cast(col, &DataType::Float64)?,
            ),
            DataType::Binary | DataType::LargeBinary | DataType::BinaryView => {
                let bin_cast = if *field.data_type() == DataType::Binary {
                    Arc::clone(col)
                } else {
                    arrow::compute::cast(col, &DataType::Binary)?
                };
                let ba = bin_cast
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .ok_or("expected BinaryArray")?;
                let mut sb = StringBuilder::with_capacity(ba.len(), ba.value_data().len());
                for i in 0..ba.len() {
                    if ba.is_null(i) {
                        sb.append_null();
                    } else {
                        sb.append_value(String::from_utf8_lossy(ba.value(i)));
                    }
                }
                (DataType::Utf8, Arc::new(sb.finish()))
            }
            _ => continue,
        };
        out_fields.push(Field::new(field.name(), target_dt, field.is_nullable()));
        out_cols.push(casted_col);
    }

    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(out_fields)),
        out_cols,
    )?)
}

fn pick_fineweb_rare_needle(text_arr: &StringArray) -> (String, usize) {
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    let sample_step = (text_arr.len() / 2_500).max(1);
    for r in (0..text_arr.len()).step_by(sample_step) {
        if text_arr.is_null(r) {
            continue;
        }
        let s = text_arr.value(r);
        let words: Vec<&str> = s
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| w.len() >= 4 && w.len() <= 16)
            .collect();
        for w in words.windows(2) {
            let phrase = format!("{} {}", w[0], w[1]);
            if (12..=20).contains(&phrase.len()) && phrase.is_ascii() {
                candidates.insert(phrase);
                if candidates.len() >= 400 {
                    break;
                }
            }
        }
        if candidates.len() >= 400 {
            break;
        }
    }

    let count_hits = |needle: &str| -> usize {
        (0..text_arr.len())
            .filter(|&i| !text_arr.is_null(i) && text_arr.value(i).contains(needle))
            .count()
    };

    let mut fallback: Option<(String, usize)> = None;
    for cand in &candidates {
        let hits = count_hits(cand);
        if (5..=50).contains(&hits) {
            return (cand.clone(), hits);
        }
        if hits > 0 {
            let better = match &fallback {
                None => true,
                Some((_, best_h)) => hits.abs_diff(20) < best_h.abs_diff(20),
            };
            if better {
                fallback = Some((cand.clone(), hits));
            }
        }
    }

    if let Some(fb) = fallback {
        return fb;
    }

    for default_nd in [
        "educational content",
        "understanding the",
        "important to note",
    ] {
        let hits = count_hits(default_nd);
        if hits > 0 {
            return (default_nd.to_owned(), hits);
        }
    }
    ("the".to_owned(), count_hits("the"))
}

fn load_fineweb_40k(path: &Path) -> BenchResult<FineWebData> {
    let max_rows = 40_000usize;
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(4_096)
        .with_limit(max_rows)
        .build()?;

    let mut norm_batches = Vec::new();
    let mut total = 0usize;
    for batch_res in reader {
        let b = batch_res?;
        if b.num_rows() == 0 {
            continue;
        }
        let take = (max_rows - total).min(b.num_rows());
        let sliced = if take < b.num_rows() {
            b.slice(0, take)
        } else {
            b
        };
        norm_batches.push(normalize_fineweb_batch(&sliced)?);
        total += take;
        if total >= max_rows {
            break;
        }
    }
    if norm_batches.is_empty() {
        return Err(format!("fineweb parquet '{}' contained 0 rows", path.display()).into());
    }
    let norm_schema = norm_batches[0].schema();
    let arrow_batch = concat_batches(&norm_schema, &norm_batches)?;
    let cetra_batch = arrow_to_cetra_batch(&arrow_batch)?;

    let text_idx = norm_schema
        .index_of("text")
        .map_err(|e| format!("fineweb missing 'text' column: {e}"))?;
    let id_idx = norm_schema.index_of("id").unwrap_or(0);
    let id_col_name = norm_schema.field(id_idx).name().clone();
    let url_idx = norm_schema.index_of("url").unwrap_or(id_idx);
    let url_col_name = norm_schema.field(url_idx).name().clone();

    let score_filter = if let Ok(idx) = norm_schema.index_of("int_score")
        && *norm_schema.field(idx).data_type() == DataType::Int64
    {
        FineWebScoreFilter::IntGe {
            col_idx: idx,
            col_name: "int_score".to_owned(),
            lo: 4,
        }
    } else if let Ok(idx) = norm_schema.index_of("score")
        && *norm_schema.field(idx).data_type() == DataType::Float64
    {
        FineWebScoreFilter::F64Ge {
            col_idx: idx,
            col_name: "score".to_owned(),
            lo: 3.5,
        }
    } else if let Ok(idx) = norm_schema.index_of("token_count")
        && *norm_schema.field(idx).data_type() == DataType::Int64
    {
        FineWebScoreFilter::IntGe {
            col_idx: idx,
            col_name: "token_count".to_owned(),
            lo: 500,
        }
    } else {
        return Err("fineweb missing int_score, score, and token_count columns".into());
    };

    let text_arr = arrow_batch
        .column(text_idx)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or("fineweb 'text' column is not StringArray")?;
    let (rare_needle, rare_hits) = pick_fineweb_rare_needle(text_arr);

    Ok(FineWebData {
        arrow_batch,
        cetra_batch,
        text_idx,
        id_idx,
        id_col_name,
        url_idx,
        url_col_name,
        score_filter,
        rare_needle,
        rare_hits,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_fineweb_suite(
    fineweb_path: &Path,
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    if !fineweb_path.exists() {
        notes.push(format!(
            "Suite I (`fineweb`): SKIPPED (`{}` not found; pass `--fineweb-parquet` or set `STATAM_FINEWEB_PARQUET`)",
            fineweb_path.display()
        ));
        return Ok(());
    }

    let data = load_fineweb_40k(fineweb_path)?;
    let raw_bytes = data.arrow_batch.get_array_memory_size() as u64;
    let wl = "fineweb_40k";
    let absent_needle = "zz_nonexistent_fineweb_xyz_999";

    let score_desc = match &data.score_filter {
        FineWebScoreFilter::IntGe { col_name, lo, .. } => format!("{col_name} >= {lo}"),
        FineWebScoreFilter::F64Ge { col_name, lo, .. } => format!("{col_name} >= {lo}"),
    };
    notes.push(format!(
        "Suite I (`fineweb`): {} rows x {} cols, `FW_score_cnt`=`{score_desc}`, `FW_substr_rare`=`\"{}\"` ({} hits)",
        data.arrow_batch.num_rows(),
        data.arrow_batch.num_columns(),
        data.rare_needle,
        data.rare_hits
    ));

    let bloom_cols: Vec<&str> = vec![data.id_col_name.as_str()];
    let enc_opts = SuiteEncodeOpts {
        wl,
        suite_label: "Suite I (`fineweb`)",
        bloom_cols: &bloom_cols,
        hot_bloom_col: Some(data.id_col_name.as_str()),
        sort_col_names: &[],
        statam_sort_cols: &[],
        narrow_trigram_cols: &[],
        payload_cols: &["text"],
        l1_use_pack: true,
        tuned_rg_rows: 16_384,
        pq_step_rows: 8_192,
        write_threads,
    };
    let files = encode_all_formats(
        &data.arrow_batch,
        Ok(&data.cetra_batch),
        raw_bytes,
        enabled,
        vortex_session,
        &enc_opts,
        write_rows,
        notes,
    )
    .await;

    let s3_cfg = SimulatedS3Config::default();
    let vortex_local = match &files.vortex {
        Some(b) => open_vortex_local(vortex_session, std::slice::from_ref(b))
            .await
            .ok()
            .and_then(|mut v| v.pop()),
        None => None,
    };
    let lance_local = match &files.lance {
        Some(b) => open_lance_local(b).await.ok(),
        None => None,
    };
    let strata_local = match &files.strata {
        Some(b) => open_single_strata_local(b).await.ok(),
        None => None,
    };

    // Query 1: FW_score_cnt
    {
        let qname = "FW_score_cnt";
        let mut expected: Option<usize> = None;
        let (score_idx, score_col_name, pq_pred, statam_pred, vx_filt) = match &data.score_filter {
            FineWebScoreFilter::IntGe {
                col_idx,
                col_name,
                lo,
            } => (
                *col_idx,
                col_name.as_str(),
                GenPqPred::I64Ge {
                    col: col_name.clone(),
                    lo: *lo,
                },
                StatamPred::new(col_name.as_str(), ZonePred::Ge(ZoneValue::I64(*lo))),
                gt_eq(col(col_name.as_str()), lit(*lo)),
            ),
            FineWebScoreFilter::F64Ge {
                col_idx,
                col_name,
                lo,
            } => (
                *col_idx,
                col_name.as_str(),
                GenPqPred::F64Ge {
                    col: col_name.clone(),
                    lo: *lo,
                },
                StatamPred::new(col_name.as_str(), ZonePred::Ge(ZoneValue::F64(*lo))),
                gt_eq(col(col_name.as_str()), lit(*lo)),
            ),
        };

        let pq_proj = if data.id_idx == score_idx {
            vec![data.id_idx]
        } else {
            vec![data.id_idx, score_idx]
        };
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &pq_proj,
            std::slice::from_ref(&pq_pred),
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let proj_names: Vec<Arc<str>> = if data.id_col_name == score_col_name {
                vec![Arc::from(data.id_col_name.as_str())]
            } else {
                vec![
                    Arc::from(data.id_col_name.as_str()),
                    Arc::from(score_col_name),
                ]
            };
            let proj = select(proj_names, root());
            run_vortex_scan_query(
                wl,
                qname,
                vortex_session,
                bytes,
                local_f,
                &proj,
                Some(&vx_filt),
                VortexPostFilter::None,
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            let cols_vec: Vec<&str> = if data.id_col_name == score_col_name {
                vec![data.id_col_name.as_str()]
            } else {
                vec![data.id_col_name.as_str(), score_col_name]
            };
            let cols_ref = cols_vec.as_slice();
            let sf = &data.score_filter;
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches = lance_scan_projected(r, Some(cols_ref)).await?;
                    let mut hits = 0usize;
                    for b in &batches {
                        match sf {
                            FineWebScoreFilter::IntGe { col_name, lo, .. } => {
                                if let Some(ia) = b
                                    .column_by_name(col_name)
                                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                                {
                                    for i in 0..ia.len() {
                                        if !ia.is_null(i) && ia.value(i) >= *lo {
                                            hits += 1;
                                        }
                                    }
                                }
                            }
                            FineWebScoreFilter::F64Ge { col_name, lo, .. } => {
                                if let Some(fa) = b
                                    .column_by_name(col_name)
                                    .and_then(|c| c.as_any().downcast_ref::<Float64Array>())
                                {
                                    for i in 0..fa.len() {
                                        if !fa.is_null(i) && fa.value(i) >= *lo {
                                            hits += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(hits)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            let id_idx = data.id_idx;
            let (is_int, lo_i, lo_f, col_name_owned) = match &data.score_filter {
                FineWebScoreFilter::IntGe { col_name, lo, .. } => {
                    (true, *lo, 0.0, col_name.clone())
                }
                FineWebScoreFilter::F64Ge { col_name, lo, .. } => (false, 0, *lo, col_name.clone()),
            };
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                move |f| {
                    let col_name_owned = col_name_owned.clone();
                    async move {
                        let proj = if id_idx == score_idx {
                            vec![id_idx]
                        } else {
                            vec![id_idx, score_idx]
                        };
                        let pred =
                            is_int.then(|| StrataPred::Cmp(score_idx, CmpOp::GtEq, Lit::Int(lo_i)));
                        let req = StrataScanRequest {
                            projection: proj,
                            predicate: pred,
                            row_ranges: None,
                            limit: None,
                            kind: ScanKind::Scan,
                            views: false,
                            top: None,
                            row_filter: None,
                        };
                        let batches = f.scan(&req).await?;
                        if is_int {
                            Ok(batches.iter().map(RecordBatch::num_rows).sum())
                        } else {
                            let mut hits = 0usize;
                            for b in &batches {
                                if let Some(fa) = b
                                    .column_by_name(&col_name_owned)
                                    .and_then(|c| c.as_any().downcast_ref::<Float64Array>())
                                {
                                    for i in 0..fa.len() {
                                        if !fa.is_null(i) && fa.value(i) >= lo_f {
                                            hits += 1;
                                        }
                                    }
                                }
                            }
                            Ok(hits)
                        }
                    }
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: Some(pq_proj),
            predicates: vec![statam_pred],
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Query 2: FW_take_100 — scattered take 100 rows, project all columns (including text)
    {
        let take_100 = scattered_row_indices(data.arrow_batch.num_rows(), 100, 9091);
        if files.strata.is_some() {
            notes.push(
                "Suite I (`fineweb`): `strata` `FW_take_100` = N/A (scattered `row_indices` take not wired)".to_owned(),
            );
        }
        run_take_query_all_formats(
            wl,
            "FW_take_100",
            &files,
            vortex_session,
            vortex_local.as_ref(),
            lance_local.as_ref(),
            &take_100,
            warm_runs,
            read_threads,
            s3_cfg,
            query_rows,
            notes,
        )
        .await?;
    }

    // Queries 3 & 4: FW_substr_rare & FW_substr_absent
    for (qname, needle, include_url) in [
        ("FW_substr_rare", data.rare_needle.as_str(), true),
        ("FW_substr_absent", absent_needle, false),
    ] {
        let mut expected: Option<usize> = if qname == "FW_substr_rare" {
            Some(data.rare_hits)
        } else {
            Some(0)
        };

        let pq_proj = if include_url && data.url_idx != data.id_idx {
            vec![data.id_idx, data.url_idx]
        } else {
            vec![data.id_idx]
        };
        let pq_preds = [GenPqPred::Utf8Like {
            col: "text".to_owned(),
            needle: needle.to_owned(),
        }];
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let mut proj_names: Vec<Arc<str>> = vec![Arc::from(data.id_col_name.as_str())];
            if include_url && data.url_col_name != data.id_col_name {
                proj_names.push(Arc::from(data.url_col_name.as_str()));
            }
            if data.id_col_name != "text" && data.url_col_name != "text" {
                proj_names.push(Arc::from("text"));
            }
            let proj = select(proj_names, root());
            run_vortex_scan_query(
                wl,
                qname,
                vortex_session,
                bytes,
                local_f,
                &proj,
                None,
                VortexPostFilter::Like {
                    col: "text",
                    needle,
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            let mut cols: Vec<&str> = vec![data.id_col_name.as_str()];
            if include_url && data.url_col_name != data.id_col_name {
                cols.push(data.url_col_name.as_str());
            }
            if data.id_col_name != "text" && data.url_col_name != "text" {
                cols.push("text");
            }
            let cols_ref = cols.as_slice();
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches = lance_scan_projected(r, Some(cols_ref)).await?;
                    let mut hits = 0usize;
                    for b in &batches {
                        if let Some(arr) = b.column_by_name("text") {
                            for i in 0..b.num_rows() {
                                if let Some(s) = extract_str_at(arr.as_ref(), i)
                                    && s.contains(needle)
                                {
                                    hits += 1;
                                }
                            }
                        }
                    }
                    Ok(hits)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            let probe = StrataScanRequest {
                projection: pq_proj.clone(),
                predicate: Some(StrataPred::Contains(
                    data.text_idx,
                    needle.as_bytes().to_vec(),
                )),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Sparse,
                views: false,
                top: None,
                row_filter: None,
            };
            let use_contains = local_f.scan(&probe).await.is_ok();
            let text_idx = data.text_idx;
            let base_proj = pq_proj.clone();
            let nd_owned = needle.to_owned();
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                move |f| {
                    let base_proj = base_proj.clone();
                    let nd_owned = nd_owned.clone();
                    async move {
                        let mut proj = base_proj;
                        if !use_contains && !proj.contains(&text_idx) {
                            proj.push(text_idx);
                        }
                        let req = StrataScanRequest {
                            projection: proj,
                            predicate: use_contains.then(|| {
                                StrataPred::Contains(text_idx, nd_owned.as_bytes().to_vec())
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
                        };
                        let batches = f.scan(&req).await?;
                        if use_contains {
                            Ok(batches.iter().map(RecordBatch::num_rows).sum())
                        } else {
                            let mut hits = 0usize;
                            for b in &batches {
                                if let Some(arr) = b.column_by_name("text") {
                                    for i in 0..b.num_rows() {
                                        if let Some(s) = extract_str_at(arr.as_ref(), i)
                                            && s.contains(nd_owned.as_str())
                                        {
                                            hits += 1;
                                        }
                                    }
                                }
                            }
                            Ok(hits)
                        }
                    }
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: Some(pq_proj),
            substrings: vec![SubstringQuery::like("text", needle)],
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    Ok(())
}

// ============================================================================
// Suite J: `nested` — Deeply Nested & Sparse Structural Data (100k rows x 7 cols)
// ============================================================================

fn build_nested_batch(num_rows: usize) -> BenchResult<RecordBatch> {
    let mut rng = SmallRng::seed_from_u64(1010);
    let categories = [
        "agent", "chat", "rag", "code", "eval", "search", "tool", "embed", "vision", "audio",
        "rerank", "guard",
    ];
    let tag_vocab: Vec<String> = (0..50).map(|i| format!("tag_{i:02}")).collect();
    let envs = ["prod", "staging", "dev"];
    let regions = ["us-east", "us-west", "eu-west", "ap-south"];
    let tiers = ["free", "pro", "enterprise"];
    let sparse_keys: Vec<String> = (0..200).map(|i| format!("k_{i:03}")).collect();
    let sparse_vals: Vec<String> = (0..50).map(|i| format!("v_{i:02}")).collect();

    let mut row_id = Vec::with_capacity(num_rows);
    let mut ts = Vec::with_capacity(num_rows);
    let mut cat_b = StringBuilder::with_capacity(num_rows, num_rows * 8);
    let mut scores_b = ListBuilder::new(Int64Builder::new());
    let mut nested_ids_b = ListBuilder::new(ListBuilder::new(Int64Builder::new()));
    let mut tags_b = ListBuilder::new(StringBuilder::new());
    let mut attrs_b = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new())
        .with_values_field(Field::new("values", DataType::Utf8, true));

    let mut row_map: BTreeMap<&str, &str> = BTreeMap::new();
    for i in 0..num_rows {
        row_id.push(i as i64);
        ts.push(DAY_START + (i as i64) * 1_000_000);
        cat_b.append_value(categories[rng.random_range(0..categories.len())]);

        // scores: List<Int64> (15% null, else 0..=12 items)
        if rng.random_range(0u8..100) < 15 {
            scores_b.append_null();
        } else {
            let n = rng.random_range(0..=12usize);
            for _ in 0..n {
                scores_b
                    .values()
                    .append_value(rng.random_range(0..1_000i64));
            }
            scores_b.append(true);
        }

        // nested_ids: List<List<Int64>> (20% null outer, else 0..=4 sublists x 0..=6 items)
        if rng.random_range(0u8..100) < 20 {
            nested_ids_b.append_null();
        } else {
            let n_sub = rng.random_range(0..=4usize);
            for _ in 0..n_sub {
                let inner = nested_ids_b.values();
                let n_items = rng.random_range(0..=6usize);
                for _ in 0..n_items {
                    inner.values().append_value(rng.random_range(0..100_000i64));
                }
                inner.append(true);
            }
            nested_ids_b.append(true);
        }

        // tags: List<Utf8> (15% null, else 0..=8 tags from 50-tag vocab)
        if rng.random_range(0u8..100) < 15 {
            tags_b.append_null();
        } else {
            let n = rng.random_range(0..=8usize);
            for _ in 0..n {
                let t = &tag_vocab[rng.random_range(0..tag_vocab.len())];
                tags_b.values().append_value(t);
            }
            tags_b.append(true);
        }

        // attrs: Map<Utf8, Utf8> (3 hot keys + 0..=5 sparse random keys, sorted unique keys)
        row_map.clear();
        row_map.insert("env", envs[rng.random_range(0..envs.len())]);
        row_map.insert("region", regions[rng.random_range(0..regions.len())]);
        row_map.insert("tier", tiers[rng.random_range(0..tiers.len())]);
        let n_sparse = rng.random_range(0..=5usize);
        for _ in 0..n_sparse {
            let k = sparse_keys[rng.random_range(0..sparse_keys.len())].as_str();
            let v = sparse_vals[rng.random_range(0..sparse_vals.len())].as_str();
            row_map.insert(k, v);
        }
        for (&k, &v) in &row_map {
            attrs_b.keys().append_value(k);
            attrs_b.values().append_value(v);
        }
        attrs_b.append(true)?;
    }

    let scores_arr: ArrayRef = Arc::new(scores_b.finish());
    let nested_ids_arr: ArrayRef = Arc::new(nested_ids_b.finish());
    let tags_arr: ArrayRef = Arc::new(tags_b.finish());
    let attrs_arr: ArrayRef = Arc::new(attrs_b.finish());

    let schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("row_id", DataType::Int64, false),
        Field::new("ts", DataType::Int64, false),
        Field::new("category", DataType::Utf8, false),
        Field::new("scores", scores_arr.data_type().clone(), true),
        Field::new("nested_ids", nested_ids_arr.data_type().clone(), true),
        Field::new("tags", tags_arr.data_type().clone(), true),
        Field::new("attrs", attrs_arr.data_type().clone(), false),
    ]));

    Ok(RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(row_id)),
            Arc::new(Int64Array::from(ts)),
            Arc::new(cat_b.finish()),
            scores_arr,
            nested_ids_arr,
            tags_arr,
            attrs_arr,
        ],
    )?)
}

#[allow(clippy::too_many_arguments)]
async fn run_nested_suite(
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let num_rows = 100_000usize;
    let arrow_batch = build_nested_batch(num_rows)?;
    let cetra_batch_res = arrow_to_cetra_batch(&arrow_batch);
    let cetra_err_str = cetra_batch_res.as_ref().err().map(ToString::to_string);
    let cetra_ref = match (&cetra_batch_res, &cetra_err_str) {
        (Ok(b), _) => Ok(b),
        (Err(_), Some(s)) => Err(s.as_str()),
        (Err(_), None) => Err("cetra batch conversion failed"),
    };
    let raw_bytes = arrow_batch.get_array_memory_size() as u64;
    let wl = "nested_100k";

    notes.push(format!(
        "Suite J (`nested`): {num_rows} rows x 7 cols (`List<i64>`, `List<List<i64>>`, `List<Utf8>`, `Map<Utf8, Utf8>`)"
    ));

    let enc_opts = SuiteEncodeOpts {
        wl,
        suite_label: "Suite J (`nested`)",
        bloom_cols: &[],
        hot_bloom_col: None,
        sort_col_names: &["row_id"],
        statam_sort_cols: &[0],
        narrow_trigram_cols: &[],
        payload_cols: &[],
        l1_use_pack: false,
        tuned_rg_rows: 32_768,
        pq_step_rows: 8_192,
        write_threads,
    };
    let files = encode_all_formats(
        &arrow_batch,
        cetra_ref,
        raw_bytes,
        enabled,
        vortex_session,
        &enc_opts,
        write_rows,
        notes,
    )
    .await;

    let s3_cfg = SimulatedS3Config::default();
    let vortex_local = match &files.vortex {
        Some(b) => match open_vortex_local(vortex_session, std::slice::from_ref(b)).await {
            Ok(mut v) => v.pop(),
            Err(e) => {
                notes.push(format!("Suite J (`nested`): `vortex` open failed ({e})"));
                None
            }
        },
        None => None,
    };
    let lance_local = match &files.lance {
        Some(b) => match open_lance_local(b).await {
            Ok(r) => Some(r),
            Err(e) => {
                notes.push(format!("Suite J (`nested`): `lance` open failed ({e})"));
                None
            }
        },
        None => None,
    };
    let strata_local = match &files.strata {
        Some(b) => match open_single_strata_local(b).await {
            Ok(f) => Some(f),
            Err(e) => {
                notes.push(format!("Suite J (`nested`): `strata` open failed ({e})"));
                None
            }
        },
        None => None,
    };

    // Query 1: NS_scan_full — full scan of all 7 columns (`row_id >= 0`)
    {
        let qname = "NS_scan_full";
        let mut expected: Option<usize> = Some(num_rows);
        let all_cols: Vec<usize> = (0..7).collect();
        let pq_preds = [GenPqPred::I64Ge {
            col: "row_id".to_owned(),
            lo: 0,
        }];
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &all_cols,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let proj = root();
            let filt = gt_eq(col("row_id"), lit(0i64));
            run_vortex_scan_query(
                wl,
                qname,
                vortex_session,
                bytes,
                local_f,
                &proj,
                Some(&filt),
                VortexPostFilter::None,
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches = lance_scan_projected(r, None).await?;
                    let mut hits = 0usize;
                    for b in &batches {
                        if let Some(ia) = b
                            .column_by_name("row_id")
                            .and_then(|c| c.as_any().downcast_ref::<Int64Array>())
                        {
                            for i in 0..ia.len() {
                                if !ia.is_null(i) && ia.value(i) >= 0 {
                                    hits += 1;
                                }
                            }
                        }
                    }
                    Ok(hits)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                |f| async move {
                    let req = StrataScanRequest {
                        projection: (0..7).collect(),
                        predicate: Some(StrataPred::Cmp(0, CmpOp::GtEq, Lit::Int(0))),
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Scan,
                        views: false,
                        top: None,
                        row_filter: None,
                    };
                    let batches = f.scan(&req).await?;
                    Ok(batches.iter().map(RecordBatch::num_rows).sum())
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: None,
            predicates: vec![StatamPred::new("row_id", ZonePred::Ge(ZoneValue::I64(0)))],
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Query 2: NS_filter_map — attrs['env'] = 'prod' AND category = 'agent', project [row_id, category, tags]
    {
        let qname = "NS_filter_map";
        let mut expected: Option<usize> = None;
        let pq_proj = [0usize, 2, 5];
        let pq_preds = [
            GenPqPred::Utf8Eq {
                col: "category".to_owned(),
                val: "agent".to_owned(),
            },
            GenPqPred::MapKeyEq {
                col: "attrs".to_owned(),
                key: "env".to_owned(),
                val: "prod".to_owned(),
            },
        ];
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let proj = select(["row_id", "category", "tags", "attrs"], root());
            let filt = eq(col("category"), lit("agent"));
            run_vortex_scan_query(
                wl,
                qname,
                vortex_session,
                bytes,
                local_f,
                &proj,
                Some(&filt),
                VortexPostFilter::MapKeyEq {
                    col: "attrs",
                    key: "env",
                    val: "prod",
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches =
                        lance_scan_projected(r, Some(&["row_id", "category", "tags", "attrs"]))
                            .await?;
                    let mut hits = 0usize;
                    for b in &batches {
                        let map_mask = eval_arrow_map_key_eq_mask(b, "attrs", "env", "prod");
                        if let Some(cat) = b.column_by_name("category") {
                            for (i, &env_ok) in map_mask.iter().enumerate() {
                                if env_ok && extract_str_at(cat.as_ref(), i) == Some("agent") {
                                    hits += 1;
                                }
                            }
                        }
                    }
                    Ok(hits)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                |f| async move {
                    let req = StrataScanRequest {
                        projection: vec![0, 2, 5],
                        predicate: Some(StrataPred::And(vec![
                            StrataPred::Cmp(2, CmpOp::Eq, Lit::Utf8("agent".to_owned())),
                            StrataPred::MapKeyEq(6, "env".to_owned(), Lit::Utf8("prod".to_owned())),
                        ])),
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Sparse,
                        views: false,
                        top: None,
                        row_filter: None,
                    };
                    let batches = f.scan(&req).await?;
                    Ok(batches.iter().map(RecordBatch::num_rows).sum())
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: Some(vec![0, 2, 5]),
            predicates: vec![
                StatamPred::new(
                    "category",
                    ZonePred::Eq(ZoneValue::Bytes(b"agent".to_vec())),
                ),
                StatamPred::map_key(
                    "attrs",
                    "env",
                    ZonePred::Eq(ZoneValue::Bytes(b"prod".to_vec())),
                ),
            ],
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Query 3: NS_take_100 — scattered take 100 rows, project all 7 nested cols
    {
        let take_100 = scattered_row_indices(num_rows, 100, 10101);
        if files.strata.is_some() {
            notes.push(
                "Suite J (`nested`): `strata` `NS_take_100` = N/A (scattered `row_indices` take not wired)".to_owned(),
            );
        }
        run_take_query_all_formats(
            wl,
            "NS_take_100",
            &files,
            vortex_session,
            vortex_local.as_ref(),
            lance_local.as_ref(),
            &take_100,
            warm_runs,
            read_threads,
            s3_cfg,
            query_rows,
            notes,
        )
        .await?;
    }

    Ok(())
}

// ============================================================================
// Suite K: `tpch-bi` — Real TPC-H `lineitem` (500k rows)
// ============================================================================

#[derive(Clone, Copy)]
enum NumericKind {
    Decimal128 { precision: u8, scale: i8 },
    Float64,
    Int64,
}

fn decimal_from_hundredths(hundredths: i128, scale: i8) -> i128 {
    if scale >= 2 {
        hundredths * 10i128.pow((scale - 2) as u32)
    } else {
        hundredths / 10i128.pow((2 - scale) as u32)
    }
}

struct TpchLineitemData {
    arrow_batch: RecordBatch,
    cetra_batch: Batch,
    shipdate_idx: usize,
    shipdate_name: String,
    shipdate_is_date32: bool,
    discount_idx: usize,
    discount_name: String,
    discount_kind: NumericKind,
    quantity_idx: usize,
    quantity_name: String,
    quantity_kind: NumericKind,
    extprice_idx: usize,
    extprice_name: String,
    returnflag_idx: usize,
    returnflag_name: String,
    linestatus_idx: usize,
    linestatus_name: String,
    tax_idx: usize,
    tax_name: String,
}

fn normalize_tpch_batch(batch: &RecordBatch) -> BenchResult<RecordBatch> {
    let schema = batch.schema();
    let mut out_fields = Vec::with_capacity(schema.fields().len());
    let mut out_cols = Vec::with_capacity(batch.num_columns());

    for (field, col) in schema.fields().iter().zip(batch.columns()) {
        let (target_dt, casted_col): (DataType, ArrayRef) = match field.data_type() {
            DataType::Boolean => (DataType::Boolean, Arc::clone(col)),
            DataType::Int64 => (DataType::Int64, Arc::clone(col)),
            DataType::Float64 => (DataType::Float64, Arc::clone(col)),
            DataType::Utf8 => (DataType::Utf8, Arc::clone(col)),
            DataType::Date32 => (DataType::Date32, Arc::clone(col)),
            DataType::Date64 => (
                DataType::Date32,
                arrow::compute::cast(col, &DataType::Date32)?,
            ),
            DataType::Decimal128(p, s) => (DataType::Decimal128(*p, *s), Arc::clone(col)),
            DataType::Decimal256(_, _) => (
                DataType::Float64,
                arrow::compute::cast(col, &DataType::Float64)?,
            ),
            DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32 => (
                DataType::Int64,
                arrow::compute::cast(col, &DataType::Int64)?,
            ),
            DataType::UInt64 => {
                let u64_arr = col
                    .as_any()
                    .downcast_ref::<UInt64Array>()
                    .ok_or("expected UInt64Array")?;
                let mut b = Int64Builder::with_capacity(u64_arr.len());
                for i in 0..u64_arr.len() {
                    if u64_arr.is_null(i) {
                        b.append_null();
                    } else {
                        b.append_value(u64_arr.value(i) as i64);
                    }
                }
                (DataType::Int64, Arc::new(b.finish()))
            }
            DataType::Float32 => (
                DataType::Float64,
                arrow::compute::cast(col, &DataType::Float64)?,
            ),
            DataType::Utf8View | DataType::LargeUtf8 => {
                (DataType::Utf8, arrow::compute::cast(col, &DataType::Utf8)?)
            }
            DataType::Timestamp(_, _) => (
                DataType::Date32,
                arrow::compute::cast(col, &DataType::Date32)?,
            ),
            other => {
                return Err(format!(
                    "lineitem unsupported Arrow type '{other}' on column '{}'",
                    field.name()
                )
                .into());
            }
        };
        out_fields.push(Field::new(field.name(), target_dt, field.is_nullable()));
        out_cols.push(casted_col);
    }

    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(out_fields)),
        out_cols,
    )?)
}

fn find_col_ci(schema: &Schema, target: &str) -> BenchResult<(usize, String)> {
    for (idx, f) in schema.fields().iter().enumerate() {
        if f.name().eq_ignore_ascii_case(target) {
            return Ok((idx, f.name().clone()));
        }
    }
    Err(format!("lineitem schema missing column '{target}'").into())
}

fn detect_numeric_kind(dt: &DataType, name: &str) -> BenchResult<NumericKind> {
    match dt {
        DataType::Decimal128(precision, scale) => Ok(NumericKind::Decimal128 {
            precision: *precision,
            scale: *scale,
        }),
        DataType::Float64 => Ok(NumericKind::Float64),
        DataType::Int64 => Ok(NumericKind::Int64),
        other => {
            Err(format!("lineitem column '{name}' has unexpected numeric type {other}").into())
        }
    }
}

fn load_tpch_lineitem_500k(path: &Path) -> BenchResult<TpchLineitemData> {
    let max_rows = 500_000usize;
    let file = File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(16_384)
        .with_limit(max_rows)
        .build()?;

    let mut norm_batches = Vec::new();
    let mut total = 0usize;
    for batch_res in reader {
        let b = batch_res?;
        if b.num_rows() == 0 {
            continue;
        }
        let take = (max_rows - total).min(b.num_rows());
        let sliced = if take < b.num_rows() {
            b.slice(0, take)
        } else {
            b
        };
        norm_batches.push(normalize_tpch_batch(&sliced)?);
        total += take;
        if total >= max_rows {
            break;
        }
    }
    if norm_batches.is_empty() {
        return Err(format!("lineitem parquet '{}' contained 0 rows", path.display()).into());
    }
    let schema = norm_batches[0].schema();
    let arrow_batch = concat_batches(&schema, &norm_batches)?;
    let cetra_batch = arrow_to_cetra_batch(&arrow_batch)?;

    let (shipdate_idx, shipdate_name) = find_col_ci(&schema, "l_shipdate")?;
    let shipdate_is_date32 = match schema.field(shipdate_idx).data_type() {
        DataType::Date32 => true,
        DataType::Int64 => false,
        other => {
            return Err(format!(
                "lineitem column '{shipdate_name}' expected Date32 or Int64, got {other}"
            )
            .into());
        }
    };
    let (discount_idx, discount_name) = find_col_ci(&schema, "l_discount")?;
    let discount_kind =
        detect_numeric_kind(schema.field(discount_idx).data_type(), &discount_name)?;
    let (quantity_idx, quantity_name) = find_col_ci(&schema, "l_quantity")?;
    let quantity_kind =
        detect_numeric_kind(schema.field(quantity_idx).data_type(), &quantity_name)?;
    let (extprice_idx, extprice_name) = find_col_ci(&schema, "l_extendedprice")?;
    let (returnflag_idx, returnflag_name) = find_col_ci(&schema, "l_returnflag")?;
    let (linestatus_idx, linestatus_name) = find_col_ci(&schema, "l_linestatus")?;
    let (tax_idx, tax_name) = find_col_ci(&schema, "l_tax")?;

    Ok(TpchLineitemData {
        arrow_batch,
        cetra_batch,
        shipdate_idx,
        shipdate_name,
        shipdate_is_date32,
        discount_idx,
        discount_name,
        discount_kind,
        quantity_idx,
        quantity_name,
        quantity_kind,
        extprice_idx,
        extprice_name,
        returnflag_idx,
        returnflag_name,
        linestatus_idx,
        linestatus_name,
        tax_idx,
        tax_name,
    })
}

fn eval_tpch_q6_arrow59(batch: &RecordBatch, data: &TpchLineitemData) -> BenchResult<usize> {
    let sd_arr = batch
        .column_by_name(&data.shipdate_name)
        .ok_or("missing shipdate")?;
    let disc_arr = batch
        .column_by_name(&data.discount_name)
        .ok_or("missing discount")?;
    let qty_arr = batch
        .column_by_name(&data.quantity_name)
        .ok_or("missing quantity")?;

    let mut hits = 0usize;
    for i in 0..batch.num_rows() {
        if sd_arr.is_null(i) || disc_arr.is_null(i) || qty_arr.is_null(i) {
            continue;
        }
        let sd_ok = if data.shipdate_is_date32 {
            let v = sd_arr
                .as_any()
                .downcast_ref::<Date32Array>()
                .ok_or("expected Date32Array")?
                .value(i);
            (TPCH_1994_01_01..=TPCH_1994_12_31).contains(&v)
        } else {
            let v = sd_arr
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or("expected Int64Array")?
                .value(i);
            (i64::from(TPCH_1994_01_01)..=i64::from(TPCH_1994_12_31)).contains(&v)
        };
        if !sd_ok {
            continue;
        }
        let disc_ok = match data.discount_kind {
            NumericKind::Decimal128 { scale, .. } => {
                let lo = decimal_from_hundredths(5, scale);
                let hi = decimal_from_hundredths(7, scale);
                let v = disc_arr
                    .as_any()
                    .downcast_ref::<Decimal128Array>()
                    .ok_or("expected Decimal128Array")?
                    .value(i);
                (lo..=hi).contains(&v)
            }
            NumericKind::Float64 => {
                let v = disc_arr
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or("expected Float64Array")?
                    .value(i);
                (0.05..=0.07).contains(&v)
            }
            NumericKind::Int64 => {
                let v = disc_arr
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or("expected Int64Array")?
                    .value(i);
                (5..=7).contains(&v)
            }
        };
        if !disc_ok {
            continue;
        }
        let qty_ok = match data.quantity_kind {
            NumericKind::Decimal128 { scale, .. } => {
                let hi = decimal_from_hundredths(2_400, scale);
                let v = qty_arr
                    .as_any()
                    .downcast_ref::<Decimal128Array>()
                    .ok_or("expected Decimal128Array")?
                    .value(i);
                v < hi
            }
            NumericKind::Float64 => {
                let v = qty_arr
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or("expected Float64Array")?
                    .value(i);
                v < 24.0
            }
            NumericKind::Int64 => {
                let v = qty_arr
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or("expected Int64Array")?
                    .value(i);
                v < 24
            }
        };
        if qty_ok {
            hits += 1;
        }
    }
    Ok(hits)
}

fn eval_tpch_q1_arrow59(batch: &RecordBatch, data: &TpchLineitemData) -> BenchResult<usize> {
    let sd_arr = batch
        .column_by_name(&data.shipdate_name)
        .ok_or("missing shipdate")?;
    let mut hits = 0usize;
    if data.shipdate_is_date32 {
        let da = sd_arr
            .as_any()
            .downcast_ref::<Date32Array>()
            .ok_or("expected Date32Array")?;
        for i in 0..da.len() {
            if !da.is_null(i) && da.value(i) <= TPCH_1998_09_02 {
                hits += 1;
            }
        }
    } else {
        let ia = sd_arr
            .as_any()
            .downcast_ref::<Int64Array>()
            .ok_or("expected Int64Array")?;
        for i in 0..ia.len() {
            if !ia.is_null(i) && ia.value(i) <= i64::from(TPCH_1998_09_02) {
                hits += 1;
            }
        }
    }
    Ok(hits)
}

fn eval_tpch_q6_arrow60(batch: &RecordBatch60, data: &TpchLineitemData) -> BenchResult<usize> {
    let sd_arr = batch
        .column_by_name(&data.shipdate_name)
        .ok_or("missing arrow60 shipdate")?;
    let disc_arr = batch
        .column_by_name(&data.discount_name)
        .ok_or("missing arrow60 discount")?;
    let qty_arr = batch
        .column_by_name(&data.quantity_name)
        .ok_or("missing arrow60 quantity")?;

    let mut hits = 0usize;
    for i in 0..batch.num_rows() {
        if sd_arr.is_null(i) || disc_arr.is_null(i) || qty_arr.is_null(i) {
            continue;
        }
        let sd_ok = if data.shipdate_is_date32 {
            let v = sd_arr
                .as_any()
                .downcast_ref::<Date32Array60>()
                .ok_or("expected Date32Array60")?
                .value(i);
            (TPCH_1994_01_01..=TPCH_1994_12_31).contains(&v)
        } else {
            let v = sd_arr
                .as_any()
                .downcast_ref::<Int64Array60>()
                .ok_or("expected Int64Array60")?
                .value(i);
            (i64::from(TPCH_1994_01_01)..=i64::from(TPCH_1994_12_31)).contains(&v)
        };
        if !sd_ok {
            continue;
        }
        let disc_ok = match data.discount_kind {
            NumericKind::Decimal128 { scale, .. } => {
                let lo = decimal_from_hundredths(5, scale);
                let hi = decimal_from_hundredths(7, scale);
                let v = disc_arr
                    .as_any()
                    .downcast_ref::<Decimal128Array60>()
                    .ok_or("expected Decimal128Array60")?
                    .value(i);
                (lo..=hi).contains(&v)
            }
            NumericKind::Float64 => {
                let v = disc_arr
                    .as_any()
                    .downcast_ref::<Float64Array60>()
                    .ok_or("expected Float64Array60")?
                    .value(i);
                (0.05..=0.07).contains(&v)
            }
            NumericKind::Int64 => {
                let v = disc_arr
                    .as_any()
                    .downcast_ref::<Int64Array60>()
                    .ok_or("expected Int64Array60")?
                    .value(i);
                (5..=7).contains(&v)
            }
        };
        if !disc_ok {
            continue;
        }
        let qty_ok = match data.quantity_kind {
            NumericKind::Decimal128 { scale, .. } => {
                let hi = decimal_from_hundredths(2_400, scale);
                let v = qty_arr
                    .as_any()
                    .downcast_ref::<Decimal128Array60>()
                    .ok_or("expected Decimal128Array60")?
                    .value(i);
                v < hi
            }
            NumericKind::Float64 => {
                let v = qty_arr
                    .as_any()
                    .downcast_ref::<Float64Array60>()
                    .ok_or("expected Float64Array60")?
                    .value(i);
                v < 24.0
            }
            NumericKind::Int64 => {
                let v = qty_arr
                    .as_any()
                    .downcast_ref::<Int64Array60>()
                    .ok_or("expected Int64Array60")?
                    .value(i);
                v < 24
            }
        };
        if qty_ok {
            hits += 1;
        }
    }
    Ok(hits)
}

fn eval_tpch_q1_arrow60(batch: &RecordBatch60, data: &TpchLineitemData) -> BenchResult<usize> {
    let sd_arr = batch
        .column_by_name(&data.shipdate_name)
        .ok_or("missing arrow60 shipdate")?;
    let mut hits = 0usize;
    if data.shipdate_is_date32 {
        let da = sd_arr
            .as_any()
            .downcast_ref::<Date32Array60>()
            .ok_or("expected Date32Array60")?;
        for i in 0..Array60::len(da) {
            if !Array60::is_null(da, i) && da.value(i) <= TPCH_1998_09_02 {
                hits += 1;
            }
        }
    } else {
        let ia = sd_arr
            .as_any()
            .downcast_ref::<Int64Array60>()
            .ok_or("expected Int64Array60")?;
        for i in 0..Array60::len(ia) {
            if !Array60::is_null(ia, i) && ia.value(i) <= i64::from(TPCH_1998_09_02) {
                hits += 1;
            }
        }
    }
    Ok(hits)
}

async fn execute_vortex_tpch_q6_fallback(
    vortex_session: &VortexSession,
    f: &VortexFile,
    proj: &Expression,
    data: &TpchLineitemData,
) -> BenchResult<usize> {
    let bound = proj.clone().bind(f.dtype())?.optimize_recursive()?;
    let mut stream = f.scan()?.with_projection(bound).into_array_stream()?;
    let mut total = 0usize;
    while let Some(chunk) = stream.try_next().await? {
        if chunk.is_empty() {
            continue;
        }
        let b = chunk_to_record_batch(vortex_session, chunk)?;
        total += eval_tpch_q6_arrow60(&b, data)?;
    }
    Ok(total)
}

async fn execute_vortex_tpch_q1_fallback(
    vortex_session: &VortexSession,
    f: &VortexFile,
    proj: &Expression,
    data: &TpchLineitemData,
) -> BenchResult<usize> {
    let bound = proj.clone().bind(f.dtype())?.optimize_recursive()?;
    let mut stream = f.scan()?.with_projection(bound).into_array_stream()?;
    let mut total = 0usize;
    while let Some(chunk) = stream.try_next().await? {
        if chunk.is_empty() {
            continue;
        }
        let b = chunk_to_record_batch(vortex_session, chunk)?;
        total += eval_tpch_q1_arrow60(&b, data)?;
    }
    Ok(total)
}

#[allow(clippy::too_many_arguments)]
async fn run_tpch_bi_suite(
    lineitem_path: &Path,
    enabled: &[&str],
    warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    if !lineitem_path.exists() {
        notes.push(format!(
            "Suite K (`tpch-bi`): SKIPPED (`{}` not found; pass `--lineitem-parquet` or set `STATAM_LINEITEM_PARQUET`)",
            lineitem_path.display()
        ));
        return Ok(());
    }

    let data = load_tpch_lineitem_500k(lineitem_path)?;
    let raw_bytes = data.arrow_batch.get_array_memory_size() as u64;
    let wl = "tpch_lineitem_500k";

    let schema = data.arrow_batch.schema();
    notes.push(format!(
        "Suite K (`tpch-bi`): {} rows x {} cols (`{}`={}, `{}`={}, `{}`={}, `{}`={})",
        data.arrow_batch.num_rows(),
        data.arrow_batch.num_columns(),
        data.shipdate_name,
        schema.field(data.shipdate_idx).data_type(),
        data.discount_name,
        schema.field(data.discount_idx).data_type(),
        data.quantity_name,
        schema.field(data.quantity_idx).data_type(),
        data.extprice_name,
        schema.field(data.extprice_idx).data_type(),
    ));

    let enc_opts = SuiteEncodeOpts {
        wl,
        suite_label: "Suite K (`tpch-bi`)",
        bloom_cols: &[],
        hot_bloom_col: None,
        sort_col_names: &[],
        statam_sort_cols: &[],
        narrow_trigram_cols: &[],
        payload_cols: &[],
        l1_use_pack: false,
        tuned_rg_rows: 65_536,
        pq_step_rows: 16_384,
        write_threads,
    };
    let files = encode_all_formats(
        &data.arrow_batch,
        Ok(&data.cetra_batch),
        raw_bytes,
        enabled,
        vortex_session,
        &enc_opts,
        write_rows,
        notes,
    )
    .await;

    let s3_cfg = SimulatedS3Config::default();
    let vortex_local = match &files.vortex {
        Some(b) => match open_vortex_local(vortex_session, std::slice::from_ref(b)).await {
            Ok(mut v) => v.pop(),
            Err(e) => {
                notes.push(format!("Suite K (`tpch-bi`): `vortex` open failed ({e})"));
                None
            }
        },
        None => None,
    };
    let lance_local = match &files.lance {
        Some(b) => match open_lance_local(b).await {
            Ok(r) => Some(r),
            Err(e) => {
                notes.push(format!("Suite K (`tpch-bi`): `lance` open failed ({e})"));
                None
            }
        },
        None => None,
    };
    let strata_local = match &files.strata {
        Some(b) => match open_single_strata_local(b).await {
            Ok(f) => Some(f),
            Err(e) => {
                notes.push(format!("Suite K (`tpch-bi`): `strata` open failed ({e})"));
                None
            }
        },
        None => None,
    };

    // Query 1: TP_Q6_filter — shipdate in [1994-01-01, 1995-01-01), discount in [0.05, 0.07], quantity < 24
    // Project: [l_extendedprice, l_discount]
    {
        let qname = "TP_Q6_filter";
        let mut expected: Option<usize> = None;

        let mut pq_preds = Vec::with_capacity(3);
        let mut statam_preds = Vec::with_capacity(3);

        if data.shipdate_is_date32 {
            pq_preds.push(GenPqPred::Date32Between {
                col: data.shipdate_name.clone(),
                lo: TPCH_1994_01_01,
                hi: TPCH_1994_12_31,
            });
            statam_preds.push(StatamPred::new(
                data.shipdate_name.as_str(),
                ZonePred::Between(
                    ZoneValue::Date(TPCH_1994_01_01),
                    ZoneValue::Date(TPCH_1994_12_31),
                ),
            ));
        } else {
            pq_preds.push(GenPqPred::I64Between {
                col: data.shipdate_name.clone(),
                lo: i64::from(TPCH_1994_01_01),
                hi: i64::from(TPCH_1994_12_31),
            });
            statam_preds.push(StatamPred::new(
                data.shipdate_name.as_str(),
                ZonePred::Between(
                    ZoneValue::I64(i64::from(TPCH_1994_01_01)),
                    ZoneValue::I64(i64::from(TPCH_1994_12_31)),
                ),
            ));
        }

        match data.discount_kind {
            NumericKind::Decimal128 { scale, .. } => {
                let lo = decimal_from_hundredths(5, scale);
                let hi = decimal_from_hundredths(7, scale);
                pq_preds.push(GenPqPred::Dec128Between {
                    col: data.discount_name.clone(),
                    lo,
                    hi,
                });
                statam_preds.push(StatamPred::new(
                    data.discount_name.as_str(),
                    ZonePred::Between(ZoneValue::Decimal128(lo), ZoneValue::Decimal128(hi)),
                ));
            }
            NumericKind::Float64 => {
                pq_preds.push(GenPqPred::F64Between {
                    col: data.discount_name.clone(),
                    lo: 0.05,
                    hi: 0.07,
                });
                statam_preds.push(StatamPred::new(
                    data.discount_name.as_str(),
                    ZonePred::Between(ZoneValue::F64(0.05), ZoneValue::F64(0.07)),
                ));
            }
            NumericKind::Int64 => {
                pq_preds.push(GenPqPred::I64Between {
                    col: data.discount_name.clone(),
                    lo: 5,
                    hi: 7,
                });
                statam_preds.push(StatamPred::new(
                    data.discount_name.as_str(),
                    ZonePred::Between(ZoneValue::I64(5), ZoneValue::I64(7)),
                ));
            }
        }

        match data.quantity_kind {
            NumericKind::Decimal128 { scale, .. } => {
                let hi = decimal_from_hundredths(2_400, scale);
                pq_preds.push(GenPqPred::Dec128Lt {
                    col: data.quantity_name.clone(),
                    hi,
                });
                statam_preds.push(StatamPred::new(
                    data.quantity_name.as_str(),
                    ZonePred::Lt(ZoneValue::Decimal128(hi)),
                ));
            }
            NumericKind::Float64 => {
                pq_preds.push(GenPqPred::F64Lt {
                    col: data.quantity_name.clone(),
                    hi: 24.0,
                });
                statam_preds.push(StatamPred::new(
                    data.quantity_name.as_str(),
                    ZonePred::Lt(ZoneValue::F64(24.0)),
                ));
            }
            NumericKind::Int64 => {
                pq_preds.push(GenPqPred::I64Le {
                    col: data.quantity_name.clone(),
                    hi: 23,
                });
                statam_preds.push(StatamPred::new(
                    data.quantity_name.as_str(),
                    ZonePred::Lt(ZoneValue::I64(24)),
                ));
            }
        }

        let pq_proj = [data.extprice_idx, data.discount_idx];
        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            // Try pushdown expression first; if Vortex fails on Date32/Decimal128 expression pushdown, fall back to column projection + post-filter.
            let vx_pushdown: BenchResult<Expression> = (|| {
                let sd_expr = if data.shipdate_is_date32 {
                    let lo = date32_vortex_scalar(vortex_session, TPCH_1994_01_01)?;
                    let hi = date32_vortex_scalar(vortex_session, TPCH_1994_12_31)?;
                    and(
                        gt_eq(col(data.shipdate_name.as_str()), lit(lo)),
                        lt_eq(col(data.shipdate_name.as_str()), lit(hi)),
                    )
                } else {
                    and(
                        gt_eq(
                            col(data.shipdate_name.as_str()),
                            lit(i64::from(TPCH_1994_01_01)),
                        ),
                        lt_eq(
                            col(data.shipdate_name.as_str()),
                            lit(i64::from(TPCH_1994_12_31)),
                        ),
                    )
                };
                let disc_expr = match data.discount_kind {
                    NumericKind::Decimal128 { precision, scale } => {
                        let lo = dec128_vortex_scalar(
                            vortex_session,
                            decimal_from_hundredths(5, scale),
                            precision,
                            scale,
                        )?;
                        let hi = dec128_vortex_scalar(
                            vortex_session,
                            decimal_from_hundredths(7, scale),
                            precision,
                            scale,
                        )?;
                        and(
                            gt_eq(col(data.discount_name.as_str()), lit(lo)),
                            lt_eq(col(data.discount_name.as_str()), lit(hi)),
                        )
                    }
                    NumericKind::Float64 => and(
                        gt_eq(col(data.discount_name.as_str()), lit(0.05f64)),
                        lt_eq(col(data.discount_name.as_str()), lit(0.07f64)),
                    ),
                    NumericKind::Int64 => and(
                        gt_eq(col(data.discount_name.as_str()), lit(5i64)),
                        lt_eq(col(data.discount_name.as_str()), lit(7i64)),
                    ),
                };
                let qty_expr = match data.quantity_kind {
                    NumericKind::Decimal128 { precision, scale } => {
                        let hi = dec128_vortex_scalar(
                            vortex_session,
                            decimal_from_hundredths(2_400, scale),
                            precision,
                            scale,
                        )?;
                        not(gt_eq(col(data.quantity_name.as_str()), lit(hi)))
                    }
                    NumericKind::Float64 => {
                        not(gt_eq(col(data.quantity_name.as_str()), lit(24.0f64)))
                    }
                    NumericKind::Int64 => not(gt_eq(col(data.quantity_name.as_str()), lit(24i64))),
                };
                Ok(and(and(sd_expr, disc_expr), qty_expr))
            })();

            let pushdown_proj = select(
                vec![
                    Arc::<str>::from(data.extprice_name.as_str()),
                    Arc::<str>::from(data.discount_name.as_str()),
                ],
                root(),
            );
            let pushdown_ok = match &vx_pushdown {
                Ok(expr) => execute_vortex_scan_with_post(
                    vortex_session,
                    local_f,
                    &pushdown_proj,
                    Some(expr),
                    VortexPostFilter::None,
                )
                .await
                .is_ok(),
                Err(_) => false,
            };

            if pushdown_ok && let Ok(expr) = vx_pushdown {
                run_vortex_scan_query(
                    wl,
                    qname,
                    vortex_session,
                    bytes,
                    local_f,
                    &pushdown_proj,
                    Some(&expr),
                    VortexPostFilter::None,
                    warm_runs,
                    s3_cfg,
                    &mut expected,
                    query_rows,
                    notes,
                )
                .await?;
            } else {
                let fallback_proj = select(
                    vec![
                        Arc::<str>::from(data.shipdate_name.as_str()),
                        Arc::<str>::from(data.discount_name.as_str()),
                        Arc::<str>::from(data.quantity_name.as_str()),
                        Arc::<str>::from(data.extprice_name.as_str()),
                    ],
                    root(),
                );
                let res: BenchResult<QueryBenchRow> = async {
                    let (local_matched_opt, local_ms, local_p95_ms) = if s3_only_mode() {
                        (None, 0.0, 0.0)
                    } else {
                        let (m, med, p95) = measure_local_async_ms(
                            || {
                                execute_vortex_tpch_q6_fallback(
                                    vortex_session,
                                    local_f,
                                    &fallback_proj,
                                    &data,
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
                        let fallback_proj = &fallback_proj;
                        let data = &data;
                        async move {
                            s3_reader.evict_all();
                            s3_reader.reset_counters();
                            let vx_read =
                                Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
                            let s3_file = vortex_session.open_options().open_read(vx_read).await?;
                            let m = execute_vortex_tpch_q6_fallback(
                                vortex_session,
                                &s3_file,
                                fallback_proj,
                                data,
                            )
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
                    Ok(QueryBenchRow {
                        workload: wl.to_owned(),
                        query: qname.to_owned(),
                        format: "vortex".to_owned(),
                        rows_matched: s3_matched,
                        local_ms,
                        local_p95_ms,
                        s3_ms,
                        s3_gets,
                        s3_bytes,
                        cold_open_gets: None,
                        cold_open_tail_bytes: None,
                    })
                }
                .await;
                match res {
                    Ok(row) => {
                        check_parity(&mut expected, wl, qname, "vortex", row.rows_matched)?;
                        query_rows.push(row);
                    }
                    Err(e) => notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})")),
                }
            }
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            let cols = [
                data.shipdate_name.as_str(),
                data.discount_name.as_str(),
                data.quantity_name.as_str(),
                data.extprice_name.as_str(),
            ];
            let data_ref = &data;
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches = lance_scan_projected(r, Some(&cols)).await?;
                    let mut hits = 0usize;
                    for b in &batches {
                        hits += eval_tpch_q6_arrow59(b, data_ref)?;
                    }
                    Ok(hits)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            let proj = vec![
                data.shipdate_idx,
                data.discount_idx,
                data.quantity_idx,
                data.extprice_idx,
            ];
            let data_ref = &data;
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                move |f| {
                    let proj = proj.clone();
                    async move {
                        let req = StrataScanRequest {
                            projection: proj,
                            predicate: None,
                            row_ranges: None,
                            limit: None,
                            kind: ScanKind::Scan,
                            views: false,
                            top: None,
                            row_filter: None,
                        };
                        let batches = f.scan(&req).await?;
                        let mut hits = 0usize;
                        for b in &batches {
                            hits += eval_tpch_q6_arrow59(b, data_ref)?;
                        }
                        Ok(hits)
                    }
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_req = ScanRequest {
            projection: Some(vec![data.extprice_idx, data.discount_idx]),
            predicates: statam_preds,
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Query 2: TP_Q1_scan — l_shipdate <= 1998-09-02,
    // Project: [l_returnflag, l_linestatus, l_quantity, l_extendedprice, l_discount, l_tax]
    {
        let qname = "TP_Q1_scan";
        let mut expected: Option<usize> = None;

        let pq_proj = [
            data.returnflag_idx,
            data.linestatus_idx,
            data.quantity_idx,
            data.extprice_idx,
            data.discount_idx,
            data.tax_idx,
        ];
        let pq_preds = if data.shipdate_is_date32 {
            vec![GenPqPred::Date32Le {
                col: data.shipdate_name.clone(),
                hi: TPCH_1998_09_02,
            }]
        } else {
            vec![GenPqPred::I64Le {
                col: data.shipdate_name.clone(),
                hi: i64::from(TPCH_1998_09_02),
            }]
        };

        run_parquet_scan_query(
            wl,
            qname,
            &files,
            &pq_proj,
            &pq_preds,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;

        if let (Some(bytes), Some(local_f)) = (&files.vortex, &vortex_local) {
            let pushdown_proj = select(
                vec![
                    Arc::<str>::from(data.returnflag_name.as_str()),
                    Arc::<str>::from(data.linestatus_name.as_str()),
                    Arc::<str>::from(data.quantity_name.as_str()),
                    Arc::<str>::from(data.extprice_name.as_str()),
                    Arc::<str>::from(data.discount_name.as_str()),
                    Arc::<str>::from(data.tax_name.as_str()),
                ],
                root(),
            );
            let vx_filt_res: BenchResult<Expression> = if data.shipdate_is_date32 {
                date32_vortex_scalar(vortex_session, TPCH_1998_09_02)
                    .map(|hi| lt_eq(col(data.shipdate_name.as_str()), lit(hi)))
            } else {
                Ok(lt_eq(
                    col(data.shipdate_name.as_str()),
                    lit(i64::from(TPCH_1998_09_02)),
                ))
            };
            let pushdown_ok = match &vx_filt_res {
                Ok(expr) => execute_vortex_scan_with_post(
                    vortex_session,
                    local_f,
                    &pushdown_proj,
                    Some(expr),
                    VortexPostFilter::None,
                )
                .await
                .is_ok(),
                Err(_) => false,
            };

            if pushdown_ok && let Ok(expr) = vx_filt_res {
                run_vortex_scan_query(
                    wl,
                    qname,
                    vortex_session,
                    bytes,
                    local_f,
                    &pushdown_proj,
                    Some(&expr),
                    VortexPostFilter::None,
                    warm_runs,
                    s3_cfg,
                    &mut expected,
                    query_rows,
                    notes,
                )
                .await?;
            } else {
                let fallback_proj = select(
                    vec![
                        Arc::<str>::from(data.shipdate_name.as_str()),
                        Arc::<str>::from(data.returnflag_name.as_str()),
                        Arc::<str>::from(data.linestatus_name.as_str()),
                        Arc::<str>::from(data.quantity_name.as_str()),
                        Arc::<str>::from(data.extprice_name.as_str()),
                        Arc::<str>::from(data.discount_name.as_str()),
                        Arc::<str>::from(data.tax_name.as_str()),
                    ],
                    root(),
                );
                let res: BenchResult<QueryBenchRow> = async {
                    let (local_matched_opt, local_ms, local_p95_ms) = if s3_only_mode() {
                        (None, 0.0, 0.0)
                    } else {
                        let (m, med, p95) = measure_local_async_ms(
                            || {
                                execute_vortex_tpch_q1_fallback(
                                    vortex_session,
                                    local_f,
                                    &fallback_proj,
                                    &data,
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
                        let fallback_proj = &fallback_proj;
                        let data = &data;
                        async move {
                            s3_reader.evict_all();
                            s3_reader.reset_counters();
                            let vx_read =
                                Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
                            let s3_file = vortex_session.open_options().open_read(vx_read).await?;
                            let m = execute_vortex_tpch_q1_fallback(
                                vortex_session,
                                &s3_file,
                                fallback_proj,
                                data,
                            )
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
                    Ok(QueryBenchRow {
                        workload: wl.to_owned(),
                        query: qname.to_owned(),
                        format: "vortex".to_owned(),
                        rows_matched: s3_matched,
                        local_ms,
                        local_p95_ms,
                        s3_ms,
                        s3_gets,
                        s3_bytes,
                        cold_open_gets: None,
                        cold_open_tail_bytes: None,
                    })
                }
                .await;
                match res {
                    Ok(row) => {
                        check_parity(&mut expected, wl, qname, "vortex", row.rows_matched)?;
                        query_rows.push(row);
                    }
                    Err(e) => notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})")),
                }
            }
        }

        if let (Some(bytes), Some(local_r)) = (&files.lance, &lance_local) {
            let cols = [
                data.shipdate_name.as_str(),
                data.returnflag_name.as_str(),
                data.linestatus_name.as_str(),
                data.quantity_name.as_str(),
                data.extprice_name.as_str(),
                data.discount_name.as_str(),
                data.tax_name.as_str(),
            ];
            let data_ref = &data;
            run_lance_query_generic(
                wl,
                qname,
                bytes,
                local_r,
                async |r: &LanceFileReader| {
                    let batches = lance_scan_projected(r, Some(&cols)).await?;
                    let mut hits = 0usize;
                    for b in &batches {
                        hits += eval_tpch_q1_arrow59(b, data_ref)?;
                    }
                    Ok(hits)
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        if let (Some(bytes), Some(local_f)) = (&files.strata, &strata_local) {
            let proj = vec![
                data.shipdate_idx,
                data.returnflag_idx,
                data.linestatus_idx,
                data.quantity_idx,
                data.extprice_idx,
                data.discount_idx,
                data.tax_idx,
            ];
            let data_ref = &data;
            run_strata_query_generic(
                wl,
                qname,
                bytes,
                local_f,
                move |f| {
                    let proj = proj.clone();
                    async move {
                        let req = StrataScanRequest {
                            projection: proj,
                            predicate: None,
                            row_ranges: None,
                            limit: None,
                            kind: ScanKind::Scan,
                            views: false,
                            top: None,
                            row_filter: None,
                        };
                        let batches = f.scan(&req).await?;
                        let mut hits = 0usize;
                        for b in &batches {
                            hits += eval_tpch_q1_arrow59(b, data_ref)?;
                        }
                        Ok(hits)
                    }
                },
                warm_runs,
                s3_cfg,
                &mut expected,
                query_rows,
                notes,
            )
            .await?;
        }

        let statam_pred = if data.shipdate_is_date32 {
            StatamPred::new(
                data.shipdate_name.as_str(),
                ZonePred::Le(ZoneValue::Date(TPCH_1998_09_02)),
            )
        } else {
            StatamPred::new(
                data.shipdate_name.as_str(),
                ZonePred::Le(ZoneValue::I64(i64::from(TPCH_1998_09_02))),
            )
        };
        let statam_req = ScanRequest {
            projection: Some(pq_proj.to_vec()),
            predicates: vec![statam_pred],
            read_threads,
            ..ScanRequest::default()
        };
        run_statam_scan_query(
            wl,
            qname,
            &files,
            &statam_req,
            warm_runs,
            s3_cfg,
            &mut expected,
            query_rows,
            notes,
        )
        .await?;
    }

    // Query 3: TP_take_100 — scattered take 100 rows, project all 16 columns (including l_comment)
    {
        let take_100 = scattered_row_indices(data.arrow_batch.num_rows(), 100, 11011);
        if files.strata.is_some() {
            notes.push(
                "Suite K (`tpch-bi`): `strata` `TP_take_100` = N/A (scattered `row_indices` take not wired)".to_owned(),
            );
        }
        run_take_query_all_formats(
            wl,
            "TP_take_100",
            &files,
            vortex_session,
            vortex_local.as_ref(),
            lance_local.as_ref(),
            &take_100,
            warm_runs,
            read_threads,
            s3_cfg,
            query_rows,
            notes,
        )
        .await?;
    }

    Ok(())
}

// ============================================================================
// Suite L: `dataloader` — Shuffled Epoch DataLoader (100 x 256-row mini-batches)
// ============================================================================

fn build_dataloader_batch(size: usize) -> BenchResult<RecordBatch> {
    let mut rng = SmallRng::seed_from_u64(1212);
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
    let mut msg = StringBuilder::with_capacity(size, size * 65);
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

fn generate_shuffled_epoch_batches(
    total_rows: usize,
    num_batches: usize,
    batch_size: usize,
    seed: u64,
) -> Vec<Vec<u64>> {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut perm: Vec<u64> = (0..total_rows as u64).collect();
    let needed = (num_batches * batch_size).min(total_rows);
    // Partial Fisher-Yates shuffle for the first `needed` unique row indices.
    for i in 0..needed {
        let j = rng.random_range(i..total_rows);
        perm.swap(i, j);
    }
    let mut out = Vec::with_capacity(num_batches);
    for b in 0..num_batches {
        let start = b * batch_size;
        let end = (start + batch_size).min(needed);
        let mut slice = perm[start..end].to_vec();
        slice.sort_unstable();
        out.push(slice);
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn run_dataloader_suite(
    enabled: &[&str],
    epoch_warm_runs: usize,
    write_threads: usize,
    read_threads: usize,
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteBenchRow>,
    query_rows: &mut Vec<QueryBenchRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let total_rows = 100_000usize;
    let num_batches = 100usize;
    let batch_size = 256usize;
    let expected_total = num_batches * batch_size;

    let arrow_batch = build_dataloader_batch(total_rows)?;
    let cetra_batch = arrow_to_cetra_batch(&arrow_batch)?;
    let raw_bytes = arrow_batch.get_array_memory_size() as u64;
    let wl = "dataloader_100k";

    let mini_batches = generate_shuffled_epoch_batches(total_rows, num_batches, batch_size, 12121);
    let mini_batches_u32: Vec<Vec<u32>> = mini_batches
        .iter()
        .map(|b| b.iter().map(|&x| x as u32).collect())
        .collect();

    notes.push(format!(
        "Suite L (`dataloader`): {total_rows} rows x 12 cols, {num_batches} x {batch_size}-row shuffled mini-batches ({expected_total} unique rows). `DL_epoch_local` reports total warm local epoch ms ({epoch_warm_runs} runs); `DL_epoch_s3_cached` runs all {num_batches} batches sequentially on one opened S3 reader sharing the 1 MiB `LruBlockCache`."
    ));

    let enc_opts = SuiteEncodeOpts {
        wl,
        suite_label: "Suite L (`dataloader`)",
        bloom_cols: &["row_id"],
        hot_bloom_col: Some("row_id"),
        sort_col_names: &["row_id"],
        statam_sort_cols: &[0],
        narrow_trigram_cols: &[],
        payload_cols: &[],
        l1_use_pack: false,
        tuned_rg_rows: 32_768,
        pq_step_rows: 8_192,
        write_threads,
    };
    let files = encode_all_formats(
        &arrow_batch,
        Ok(&cetra_batch),
        raw_bytes,
        enabled,
        vortex_session,
        &enc_opts,
        write_rows,
        notes,
    )
    .await;

    if files.strata.is_some() {
        notes.push(
            "Suite L (`dataloader`): `strata` epoch queries = N/A (scattered `row_indices` take not wired)".to_owned(),
        );
    }

    let s3_cfg = SimulatedS3Config::default();
    let vortex_local = match &files.vortex {
        Some(b) => open_vortex_local(vortex_session, std::slice::from_ref(b))
            .await
            .ok()
            .and_then(|mut v| v.pop()),
        None => None,
    };
    let lance_local = match &files.lance {
        Some(b) => open_lance_local(b).await.ok(),
        None => None,
    };

    // Query 1: DL_epoch_local — 100 x 256-row takes sequentially on one opened local reader
    if !s3_only_mode() {
        let qname = "DL_epoch_local";
        let mut expected: Option<usize> = Some(expected_total);

        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &files.pq_default, false),
            ("pq_tuned", &files.pq_tuned, true),
            ("pq_cetra", &files.pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = (|| {
                let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                    PageIndexPolicy::Optional
                } else {
                    PageIndexPolicy::Skip
                });
                let meta = ArrowReaderMetadata::load(bytes, opts)?;
                let (matched, local_ms, local_p95_ms) = measure_local_ms(
                    || {
                        let mut sum = 0usize;
                        for idxs in &mini_batches {
                            let builder = ParquetRecordBatchReaderBuilder::new_with_metadata(
                                bytes.clone(),
                                meta.clone(),
                            );
                            sum += execute_parquet_row_selection(builder, idxs)?;
                        }
                        Ok(sum)
                    },
                    epoch_warm_runs,
                )?;
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms: 0.0,
                    s3_gets: 0,
                    s3_bytes: 0,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            })();
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }

        if let Some(local_f) = &vortex_local {
            let res: BenchResult<QueryBenchRow> = async {
                let (matched, local_ms, local_p95_ms) = measure_local_async_ms(
                    || async {
                        let mut sum = 0usize;
                        for idxs in &mini_batches {
                            sum += execute_vortex_take_count(vortex_session, local_f, idxs).await?;
                        }
                        Ok(sum)
                    },
                    epoch_warm_runs,
                )
                .await?;
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "vortex".to_owned(),
                    rows_matched: matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms: 0.0,
                    s3_gets: 0,
                    s3_bytes: 0,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, "vortex", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})")),
            }
        }

        if let Some(local_r) = &lance_local {
            let res: BenchResult<QueryBenchRow> = async {
                let (matched, local_ms, local_p95_ms) = measure_local_async_ms(
                    || async {
                        let mut sum = 0usize;
                        for idxs in &mini_batches_u32 {
                            let batches = lance_take_indices(local_r, idxs, None).await?;
                            sum += batches.iter().map(RecordBatch::num_rows).sum::<usize>();
                        }
                        Ok(sum)
                    },
                    epoch_warm_runs,
                )
                .await?;
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "lance".to_owned(),
                    rows_matched: matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms: 0.0,
                    s3_gets: 0,
                    s3_bytes: 0,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, "lance", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `lance` = N/A ({e})")),
            }
        }

        for (fmt, bytes_opt) in [
            ("statam_l0", &files.statam_l0),
            ("statam_l1", &files.statam_l1),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = (|| {
                let local_reader = open_statam_local(bytes, None)?;
                let (matched, local_ms, local_p95_ms) = measure_local_ms(
                    || {
                        let mut sum = 0usize;
                        for idxs in &mini_batches {
                            let batches = local_reader.take_rows(idxs, None, read_threads)?;
                            sum += batches.iter().map(Batch::num_rows).sum::<usize>();
                        }
                        Ok(sum)
                    },
                    epoch_warm_runs,
                )?;
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: matched,
                    local_ms,
                    local_p95_ms,
                    s3_ms: 0.0,
                    s3_gets: 0,
                    s3_bytes: 0,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            })();
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }
    }

    // Query 2: DL_epoch_s3_cached — 100 x 256-row takes sequentially on ONE opened S3 reader
    {
        let qname = "DL_epoch_s3_cached";
        let mut expected: Option<usize> = Some(expected_total);
        let s3_only = s3_only_mode();

        for (fmt, bytes_opt, with_pi) in [
            ("pq_default", &files.pq_default, false),
            ("pq_tuned", &files.pq_tuned, true),
            ("pq_cetra", &files.pq_cetra, true),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = async {
                let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
                let t0 = Instant::now();
                let (sum, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_reader = Arc::clone(&s3_reader);
                    let mini_batches = &mini_batches;
                    async move {
                        s3_reader.evict_all();
                        s3_reader.reset_counters();
                        let mut async_reader =
                            BenchS3ParquetAsyncReader::new(Arc::clone(&s3_reader), with_pi);
                        let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                            PageIndexPolicy::Optional
                        } else {
                            PageIndexPolicy::Skip
                        });
                        let s3_meta =
                            ArrowReaderMetadata::load_async(&mut async_reader, opts).await?;
                        let mut sum = 0usize;
                        for idxs in mini_batches {
                            let builder = ParquetRecordBatchStreamBuilder::new_with_metadata(
                                async_reader.clone(),
                                s3_meta.clone(),
                            );
                            sum += execute_parquet_row_selection_async(builder, idxs).await?;
                        }
                        Ok((
                            sum,
                            s3_reader.counters().gets,
                            s3_reader.counters().bytes_from_s3,
                            s3_reader.simulated_latency_ms(),
                        ))
                    }
                })
                .await?;
                let cpu_ms = if s3_only {
                    0.0
                } else {
                    t0.elapsed().as_secs_f64() * 1000.0
                };
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: sum,
                    local_ms: cpu_ms,
                    local_p95_ms: cpu_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }

        if let Some(bytes) = &files.vortex {
            let res: BenchResult<QueryBenchRow> = async {
                let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
                let t0 = Instant::now();
                let (sum, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_reader = Arc::clone(&s3_reader);
                    let mini_batches = &mini_batches;
                    async move {
                        s3_reader.evict_all();
                        s3_reader.reset_counters();
                        let vx_read = Arc::new(SimS3VortexRead::from_s3(Arc::clone(&s3_reader)));
                        let s3_file = vortex_session.open_options().open_read(vx_read).await?;
                        let mut sum = 0usize;
                        for idxs in mini_batches {
                            sum +=
                                execute_vortex_take_count(vortex_session, &s3_file, idxs).await?;
                        }
                        Ok((
                            sum,
                            s3_reader.counters().gets,
                            s3_reader.counters().bytes_from_s3,
                            s3_reader.simulated_latency_ms(),
                        ))
                    }
                })
                .await?;
                let cpu_ms = if s3_only {
                    0.0
                } else {
                    t0.elapsed().as_secs_f64() * 1000.0
                };
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "vortex".to_owned(),
                    rows_matched: sum,
                    local_ms: cpu_ms,
                    local_p95_ms: cpu_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, "vortex", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `vortex` = N/A ({e})")),
            }
        }

        if let Some(bytes) = &files.lance {
            let res: BenchResult<QueryBenchRow> = async {
                let s3_file = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
                let t0 = Instant::now();
                let (sum, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_file = Arc::clone(&s3_file);
                    let mini_batches_u32 = &mini_batches_u32;
                    async move {
                        s3_file.evict_all();
                        s3_file.reset_counters();
                        let s3_reader = open_lance_from_bench_s3(Arc::clone(&s3_file)).await?;
                        let mut sum = 0usize;
                        for idxs in mini_batches_u32 {
                            let batches = lance_take_indices(&s3_reader, idxs, None).await?;
                            sum += batches.iter().map(RecordBatch::num_rows).sum::<usize>();
                        }
                        Ok((
                            sum,
                            s3_file.counters().gets,
                            s3_file.counters().bytes_from_s3,
                            s3_file.simulated_latency_ms(),
                        ))
                    }
                })
                .await?;
                let cpu_ms = if s3_only {
                    0.0
                } else {
                    t0.elapsed().as_secs_f64() * 1000.0
                };
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: "lance".to_owned(),
                    rows_matched: sum,
                    local_ms: cpu_ms,
                    local_p95_ms: cpu_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, "lance", row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `lance` = N/A ({e})")),
            }
        }

        for (fmt, bytes_opt) in [
            ("statam_l0", &files.statam_l0),
            ("statam_l1", &files.statam_l1),
        ] {
            let Some(bytes) = bytes_opt else { continue };
            let res: BenchResult<QueryBenchRow> = async {
                let (s3_file, s3_pack, s3_resolver) = create_statam_s3_readers(bytes, None, s3_cfg);
                let t0 = Instant::now();
                let (sum, s3_gets, s3_bytes, s3_ms) = run_s3_adaptive(|| {
                    let s3_file = Arc::clone(&s3_file);
                    let s3_pack = s3_pack.clone();
                    let s3_resolver = s3_resolver.clone();
                    let mini_batches = &mini_batches;
                    async move {
                        let s3_reader = open_statam_from_bench_s3(
                            &s3_file,
                            s3_pack.as_ref(),
                            s3_resolver.as_ref(),
                        )?;
                        let mut sum = 0usize;
                        for idxs in mini_batches {
                            let batches = s3_reader.take_rows(idxs, None, read_threads)?;
                            sum += batches.iter().map(Batch::num_rows).sum::<usize>();
                        }
                        let (ms, gets, bytes) = statam_s3_totals(&s3_file, None);
                        Ok((sum, gets, bytes, ms))
                    }
                })
                .await?;
                let cpu_ms = if s3_only {
                    0.0
                } else {
                    t0.elapsed().as_secs_f64() * 1000.0
                };
                Ok(QueryBenchRow {
                    workload: wl.to_owned(),
                    query: qname.to_owned(),
                    format: fmt.to_owned(),
                    rows_matched: sum,
                    local_ms: cpu_ms,
                    local_p95_ms: cpu_ms,
                    s3_ms,
                    s3_gets,
                    s3_bytes,
                    cold_open_gets: None,
                    cold_open_tail_bytes: None,
                })
            }
            .await;
            match res {
                Ok(row) => {
                    check_parity(&mut expected, wl, qname, fmt, row.rows_matched)?;
                    query_rows.push(row);
                }
                Err(e) => notes.push(format!("{wl} `{qname}` `{fmt}` = N/A ({e})")),
            }
        }
    }

    Ok(())
}

// ============================================================================
// Report Rendering & Entrypoint
// ============================================================================

fn render_report(
    cli: &CliConfig,
    write_rows: &[WriteBenchRow],
    query_rows: &[QueryBenchRow],
    notes: &[String],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n# Lance / Vortex / F3 Extended Benchmark Report (Suites G–L)"
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

    let _ = writeln!(out, "\n## 1. Write Throughput & Storage Size");
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
            "[lance_suites_bench] S3 mode: {} (s3_only={})",
            s3_mode.label(),
            s3_only_mode()
        );
    } else {
        eprintln!(
            "[lance_suites_bench] Simulated S3: {} (s3_only={})",
            sim_s3_header_summary(s3_mode),
            s3_only_mode()
        );
    }
    let env_warm_runs = std::env::var("STATAM_WARM_RUNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0);
    let warm_runs = env_warm_runs.unwrap_or(5);
    let epoch_warm_runs = env_warm_runs.unwrap_or(3);
    let vortex_session = new_vortex_session();

    let mut write_rows = Vec::new();
    let mut query_rows = Vec::new();
    let mut notes = Vec::new();

    if cli.suite.includes_blobs() {
        run_blobs_suite(
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

    if cli.suite.includes_wide_1000() {
        run_wide_1000_suite(
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

    if cli.suite.includes_fineweb() {
        run_fineweb_suite(
            &cli.fineweb_parquet,
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

    if cli.suite.includes_nested() {
        run_nested_suite(
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

    if cli.suite.includes_tpch_bi() {
        run_tpch_bi_suite(
            &cli.lineitem_parquet,
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

    if cli.suite.includes_dataloader() {
        run_dataloader_suite(
            &cli.formats,
            epoch_warm_runs,
            cli.statam_write_threads,
            cli.statam_read_threads,
            &vortex_session,
            &mut write_rows,
            &mut query_rows,
            &mut notes,
        )
        .await?;
    }

    let md = render_report(&cli, &write_rows, &query_rows, &notes);
    println!("{md}");
    Ok(())
}

fn main() -> std::process::ExitCode {
    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    rt_builder.enable_all();
    if let Ok(raw) = std::env::var("STATAM_BENCH_THREADS")
        && let Ok(n) = raw.trim().parse::<usize>()
        && n > 0
    {
        rt_builder.worker_threads(n);
    }
    let rt = match rt_builder.build() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("lance_suites_bench failed to build tokio runtime: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if let Err(err) = rt.block_on(run()) {
        eprintln!("lance_suites_bench failed: {err}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
