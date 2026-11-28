//! Parquet vs Vortex on LLM-shaped spans, through DataFusion.
//!
//! fmtbench gen <dir> <traces> <files>
//! fmtbench query <dir|s3://bucket/key> [runs]
//! fmtbench profile <dir|s3://bucket/key> [runs]
//! fmtbench probe <s3://bucket/key> [reps]
//! fmtbench upload <dir> <s3://bucket/key> [variant...]

mod counting;
mod cw;
mod payload;
mod pqfast;
mod hedge;
mod probe;
mod profile;
mod series;
mod sim;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::compute::{SortColumn, cast, concat_batches, lexsort_to_indices, take_record_batch};
use arrow::datatypes::{DataType, Field, Fields, Schema, SchemaRef};
use cpu_time::ProcessTime;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::execution::SessionStateBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use futures::TryStreamExt;
use futures::future::try_join_all;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjPath;
use object_store::{ObjectMeta, ObjectStore, ObjectStoreExt};
use strata::{CmpOp, Config, Lit, Predicate, StrataFile, StrataTable, StrataWriter, trace_detail, trace_list};
use fmt_testdata::datagen;
use cetra_core::batch::spans_to_record_batch;
use cetra_core::enums::SpanKind;
use cetra_core::span::Span;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;
use vortex::VortexSessionDefault;
use vortex::array::IntoArray;
use vortex::array::arrays::ChunkedArray;
use vortex::file::WriteOptionsSessionExt;
use vortex::io::session::RuntimeSessionExt;
use vortex::session::VortexSession;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DICT: &[&str] = &[
    "project_id", "span_kind", "status", "service_name", "environment", "release", "provider",
    "request_model", "response_model", "model", "operation", "cost_source", "input_format",
    "output_format", "scope_name", "scope_version", "conventions",
];
const BLOOM: &[&str] = &["trace_id", "session_id", "user_id"];

#[tokio::main(flavor = "multi_thread")]
async fn main() -> R<()> {
    let guard = match std::env::var("FMT_PPROF") {
        Ok(_) => Some(pprof::ProfilerGuardBuilder::default().frequency(997).build()?),
        Err(_) => None,
    };
    let r = run().await;
    if let Some(g) = guard {
        print_profile(&g.report().build()?);
    }
    r
}

async fn run() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => {
            let dir = PathBuf::from(&args[2]);
            let traces: usize = args[3].parse()?;
            let files: usize = args[4].parse()?;
            let swe = match args.get(5).map(String::as_str) {
                None => None,
                Some(flag @ ("--swe" | "--convo")) => {
                    let max: usize = args.get(6).ok_or(USAGE)?.parse()?;
                    let shards = args.get(7..).unwrap_or_default();
                    if shards.is_empty() {
                        return Err(USAGE.into());
                    }
                    Some((flag == "--convo", max, shards))
                }
                Some(_) => return Err(USAGE.into()),
            };
            generate(&dir, traces, files, swe).await
        }
        Some("query") => {
            let runs: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(5);
            let only = args.get(4).cloned();
            query(&args[2], runs, only).await
        }
        Some("profile") => {
            let loc = location(&args[2])?;
            let runs: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(30);
            profile::profile(&loc, runs, async |ctx: &SessionContext, variant: &str| {
                table(ctx, &loc, variant).await.map(drop)
            })
            .await
        }
        Some("merge") => merge_cmd(&args[2]).await,
        Some("probe") => {
            let reps: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(50);
            probe::probe(args.get(2).ok_or(USAGE)?, reps).await
        }
        Some("download") => probe::download(args.get(2).ok_or(USAGE)?, Path::new(args.get(3).ok_or(USAGE)?)).await,
        Some("upload") => probe::upload(Path::new(args.get(2).ok_or(USAGE)?), args.get(3).ok_or(USAGE)?, args.get(4..).unwrap_or_default()).await,
        Some("series") => {
            let runs: usize = args.get(4).map(|s| s.parse()).transpose()?.unwrap_or(5);
            series::series_cmd(Path::new(args.get(2).ok_or(USAGE)?), Path::new(args.get(3).ok_or(USAGE)?), runs, args.get(5..).unwrap_or_default()).await
        }
        Some("zexp") => zexp_cmd(&args[2..]),
        Some("gexp") => gexp_cmd(&args[2], &args[3..]),
        Some("lookupcols") => lookup_cols(args.get(2).ok_or(USAGE)?).await,
        Some("restrata") => restrata_cmd(Path::new(args.get(2).ok_or(USAGE)?), args.get(3).map_or("st", String::as_str)),
        Some("swe") => {
            let budget: usize = args[2].parse()?;
            swe_cmd(budget, &args[3..])
        }
        Some("payload") => {
            let dir = PathBuf::from(&args[2]);
            payload_cmd(&dir)
        }
        Some("fineweb") => {
            let dir = PathBuf::from(args.get(2).ok_or(USAGE)?);
            let spans: usize = args.get(3).ok_or(USAGE)?.parse()?;
            let per_file: usize = args.get(4).ok_or(USAGE)?.parse()?;
            let paths: Vec<PathBuf> = args.get(5..).unwrap_or_default().iter().map(PathBuf::from).collect();
            if paths.is_empty() {
                return Err(USAGE.into());
            }
            fineweb_cmd(&dir, spans, per_file, paths).await
        }
        Some("cw") => cw::cmd(&args[2..]).await,
        _ => Err(USAGE.into()),
    }
}

const USAGE: &str = "usage: fmtbench gen <dir> <traces> <files> [--swe <max_trajectories> <shard>... | --convo <max_conversations> <file>...] | query <dir|s3://bucket/key> [runs] [only] | profile <dir|s3://bucket/key> [runs] | probe <s3://bucket/key> [reps] | upload <dir> <s3://bucket/key> [variant...] | download <s3://bucket/key> <dir> | merge <dir> (local only) | restrata <dir> [out] (local only) | series <data> <out> [runs] [datasets...] (local only) | fineweb <dir> <spans> <spans_per_file> <fineweb.parquet>... (local only; FMT_QUERYSET=braintrust for its queries) | cw gen <case> <dir> [--traces N] | cw import <src-dir> <dir> [name=needle...] | cw run <dir|s3://bucket/key> [--rounds R] [--runs K] [--queries a,b] | cw split <dir> <n> | cw flush-input <dir> | cw project <dir> --days D --traces-per-day N [--live-hours H] [--payloads <case-dir>,<case-dir>] | cw project <dir> --small R --flushes K | cw layout <dir> | cw needles <dir>\n  s3 needs AWS_ENDPOINT, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_DEFAULT_REGION (R2: auto), and AWS_ALLOW_HTTP=true for a plain-HTTP endpoint";

// ------------------------------------------------------------------ generate

/// Drops a span's payloads for the light dataset: no input, output, system instructions,
/// tool definitions or event attributes, and previews cut to 64 chars.
fn strip_payloads(s: &mut Span) {
    s.input = None;
    s.output = None;
    s.system_instructions = None;
    s.tool_definitions = None;
    s.input_preview = s.input_preview.take().map(|p| p.chars().take(64).collect());
    s.output_preview = s.output_preview.take().map(|p| p.chars().take(64).collect());
    for e in &mut s.events {
        e.attributes = None;
    }
}

/// `swe` = (convo, maximum traces, input paths): spans come from the SWE-agent
/// shards, or from files written by `convert_convos.py` when `convo` is set,
/// instead of synthetic traces, and `traces` is ignored.
async fn generate(dir: &Path, traces: usize, files: usize, swe: Option<(bool, usize, &[String])>) -> R<()> {
    let t0 = Instant::now();
    let world = datagen::World::new(7);
    let mut rng = datagen::Rng::new(42);
    let (mut spans, needles) = match swe {
        Some((convo, max, shards)) => {
            println!("traces argument ignored: spans come from {} input files", shards.len());
            let paths: Vec<PathBuf> = shards.iter().map(PathBuf::from).collect();
            let spans = if convo {
                fmt_testdata::convo::spans(&paths, max, &world, &mut rng)?
            } else {
                fmt_testdata::swe::spans(&paths, max, &world, &mut rng)?
            };
            let needles = pick_needles(&spans);
            (spans, needles)
        }
        None => {
            let light = std::env::var_os("FMT_LIGHT").is_some();
            let mut spans = Vec::new();
            for _ in 0..traces {
                let start = spans.len();
                world.trace(&mut rng, &mut spans);
                // Strip each trace as it is made, so the light dataset never holds full payloads.
                if light {
                    spans[start..].iter_mut().for_each(strip_payloads);
                }
            }
            (spans, Vec::new())
        }
    };
    if swe.is_some() && std::env::var_os("FMT_LIGHT").is_some() {
        spans.iter_mut().for_each(strip_payloads);
    }
    // Plant a rare needle in ~20 LLM inputs for the substring query (synthetic traces only).
    if swe.is_none() {
        let mut planted = 0;
        let step = spans.len() / 23;
        let mut i = 7;
        while planted < 20 && i < spans.len() {
            if let Some(inp) = spans[i].input.as_mut() {
                if let Some(p) = inp.find("\"content\":\"") {
                    inp.insert_str(p + 11, "zqxjneedle ");
                    planted += 1;
                }
            }
            i += step.max(1);
        }
    }
    // Sample trace ids for point lookups.
    let mut ids = Vec::new();
    for k in 0..100 {
        let s = &spans[(k * 7919 * 13) % spans.len()];
        ids.push(hex(s.trace_id.as_bytes()));
    }
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("trace_ids.txt"), ids.join("\n"))?;
    if swe.is_some() {
        let lines: Vec<String> = needles.iter().map(|n| hex(n)).collect();
        std::fs::write(dir.join("needles.txt"), lines.join("\n"))?;
    }
    println!("generated {} spans from {traces} traces in {:?}", spans.len(), t0.elapsed());

    // Arrow batches, sorted by start_time, cut into `files` time slices,
    // each sorted by (trace_id, start_time) — spec 03 §3 / §5.
    spans.sort_by_key(|s| s.start_time);
    let per = spans.len().div_ceil(files);
    let mut raw = 0usize;
    let mut slices = Vec::new();
    for chunk in spans.chunks(per) {
        let mut batches = Vec::new();
        for c in chunk.chunks(8192) {
            batches.push(spans_to_record_batch(c)?);
        }
        let b = concat_batches(&batches[0].schema(), &batches)?;
        let idx = lexsort_to_indices(
            &[
                SortColumn { values: b.column_by_name("trace_id").unwrap().clone(), options: None },
                SortColumn { values: b.column_by_name("start_time").unwrap().clone(), options: None },
            ],
            None,
        )?;
        let b = take_record_batch(&b, &idx)?;
        raw += b.get_array_memory_size();
        slices.push(b);
    }
    drop(spans);
    let payload_bytes: usize = slices
        .iter()
        .map(|b| {
            ["input", "output", "system_instructions", "tool_definitions"]
                .iter()
                .map(|c| {
                    let a = b.column_by_name(c).unwrap();
                    a.as_any()
                        .downcast_ref::<arrow::array::StringArray>()
                        .map(|s| s.values().len())
                        .unwrap_or(0)
                })
                .sum::<usize>()
        })
        .sum();
    println!("arrow in-memory {} MiB, payload strings {} MiB", raw >> 20, payload_bytes >> 20);

    for variant in PQ_VARIANTS {
        let sub = dir.join(variant);
        std::fs::create_dir_all(&sub)?;
        let t = Instant::now();
        let mut bytes = 0u64;
        for (i, b) in slices.iter().enumerate() {
            bytes += write_parquet(variant, b, &sub.join(format!("{i:03}.parquet")))?;
        }
        let el = t.elapsed();
        println!("{variant:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", bytes >> 20, el, raw as f64 / 1048576.0 / el.as_secs_f64());
    }

    for variant in VX_VARIANTS {
        let sub = dir.join(variant);
        std::fs::create_dir_all(&sub)?;
        let t = Instant::now();
        let mut bytes = 0u64;
        for (i, b) in slices.iter().enumerate() {
            bytes += write_vortex(variant, b, &sub.join(format!("{i:03}.vortex"))).await?;
        }
        let el = t.elapsed();
        println!("{variant:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", bytes >> 20, el, raw as f64 / 1048576.0 / el.as_secs_f64());
    }

    let st_dir = dir.join("st");
    std::fs::create_dir_all(&st_dir)?;
    let t = Instant::now();
    let mut bytes = 0u64;
    let mut summaries = Vec::new();
    for (i, b) in slices.iter().enumerate() {
        let (len, summary) = write_strata(b, &st_dir.join(format!("{i:03}.strata")))?;
        bytes += len;
        summaries.push(summary);
    }
    let el = t.elapsed();
    println!("{:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", "st", bytes >> 20, el, raw as f64 / 1048576.0 / el.as_secs_f64());

    payload_table(dir, &summaries)?;
    Ok(())
}

/// Writes `b` as the Parquet file of `variant` at `path`, 8,192 rows per write call. Returns
/// the file's size.
fn write_parquet(variant: &str, b: &RecordBatch, path: &Path) -> R<u64> {
    let f = std::fs::File::create(path)?;
    let mut w = ArrowWriter::try_new(f, b.schema(), Some(parquet_props(variant, &b.schema(), b.num_rows())))?;
    let rg_rows = if variant == "pq_cetra" { 100_000 } else { usize::MAX };
    for off in (0..b.num_rows()).step_by(8192) {
        w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
        if (variant == "pq_cetra" && w.in_progress_size() >= 64 << 20) || w.in_progress_rows() >= rg_rows {
            w.flush()?;
        }
    }
    w.close()?;
    Ok(std::fs::metadata(path)?.len())
}

/// Writes `b` as the Vortex file of `variant` at `path`, in chunks of 8,192 rows. Returns the
/// file's size.
async fn write_vortex(variant: &str, b: &RecordBatch, path: &Path) -> R<u64> {
    let session = VortexSession::default().with_tokio();
    let b = for_vortex(b)?;
    let chunks: Vec<_> = (0..b.num_rows())
        .step_by(8192)
        .map(|off| vortex_arrow::convert::from_arrow_batch(&b.slice(off, 8192.min(b.num_rows() - off)), false))
        .collect::<Result<_, _>>()?;
    let arr = ChunkedArray::from_iter(chunks).into_array();
    let mut f = tokio::fs::File::create(path).await?;
    let mut opts = session.write_options();
    if variant == "vx_compact" {
        let strategy = vortex::file::WriteStrategyBuilder::from_session(&session)
            .with_btrblocks_builder(vortex::compressor::BtrBlocksCompressorBuilder::from_session(&session).with_compact())
            .build();
        opts = opts.with_strategy(strategy);
    }
    opts.write(&mut f, arr.to_array_stream()).await?;
    Ok(std::fs::metadata(path)?.len())
}

/// Writes `b`, sorted by (trace_id, start_time), as a Strata file at `path` with the default
/// config, 8,192 rows per write call. Returns the file's size and summary.
fn write_strata(b: &RecordBatch, path: &Path) -> R<(u64, strata::FileSummary)> {
    let mut w = StrataWriter::new(b.schema(), &["trace_id", "start_time"], Config::default())?;
    for off in (0..b.num_rows()).step_by(8192) {
        w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
    }
    let (file, summary) = w.finish()?;
    std::fs::write(path, &file)?;
    Ok((file.len() as u64, summary))
}

/// Payload columns: Strata bytes and their parts beside the Parquet column bytes, then the
/// G6 total of the four payload columns against the best Parquet variant.
fn payload_table(dir: &Path, summaries: &[strata::FileSummary]) -> R<()> {
    let names = ["input", "output", "system_instructions", "tool_definitions"];
    let mut pq = Vec::new();
    // A dataset may hold only some of the variants (fineweb: pq_default).
    for v in PQ_VARIANTS.iter().filter(|v| dir.join(v).is_dir()) {
        pq.push((*v, column_sizes_parquet(&dir.join(v), v)?));
    }
    println!("payload columns (KiB; a store's frames, index and store segment count in its first column):");
    let mut st_total = 0u64;
    for n in names {
        let (mut st_bytes, mut frames, mut has_payload) = (0u64, 0u64, false);
        let (mut frame_bytes, mut ref_bytes, mut index_bytes, mut store_bytes, mut dict_bytes) = (0u64, 0u64, 0u64, 0u64, 0u64);
        let (mut unique_chunks, mut chunk_refs) = (0u64, 0u64);
        for s in summaries {
            if let Some(c) = s.columns.iter().find(|c| c.name == n) {
                st_bytes += c.bytes;
                if let Some(p) = &c.payload {
                    has_payload = true;
                    frames += u64::from(p.frames);
                    frame_bytes += p.frame_bytes;
                    ref_bytes += p.ref_bytes;
                    index_bytes += p.index_bytes;
                    store_bytes += p.store_bytes;
                    dict_bytes += p.dict_bytes;
                    unique_chunks += u64::from(p.unique_chunks);
                    chunk_refs += p.chunk_refs;
                }
            }
        }
        st_total += st_bytes;
        let mut line = format!("  {n:22} st {:>9}  frames {:>8}", st_bytes >> 10, if has_payload { frames.to_string() } else { "-".into() });
        for (v, sizes) in &pq {
            let b = sizes.iter().find(|(c, _)| c == n).map_or(0, |(_, b)| *b);
            line.push_str(&format!("  {v} {:>9}", b >> 10));
        }
        println!("{line}");
        if has_payload {
            println!(
                "  {:22}    frames {:>9} KiB  refs {:>8} KiB  index {:>8} KiB  store {:>7} KiB (dict {:>5})  unique chunks {unique_chunks:>9}  chunk refs {chunk_refs:>10}",
                "",
                frame_bytes >> 10,
                ref_bytes >> 10,
                index_bytes >> 10,
                store_bytes >> 10,
                dict_bytes >> 10
            );
        }
    }
    let best = pq
        .iter()
        .map(|(v, sizes)| (*v, sizes.iter().filter(|(c, _)| names.contains(&c.as_str())).map(|(_, b)| *b).sum::<u64>()))
        .min_by_key(|(_, b)| *b);
    if let Some((v, b)) = best {
        println!("G6 payload columns: st {} KiB, best parquet {v} {} KiB, ratio {:.3}", st_total >> 10, b >> 10, st_total as f64 / b.max(1) as f64);
    }
    Ok(())
}

/// Twenty substrings of LLM outputs that occur in 1 to 1,000 span inputs, for the
/// SWE substring needles. Picks use `Rng::new(77)`; gives up after 2,000 attempts.
fn pick_needles(spans: &[Span]) -> Vec<Vec<u8>> {
    let mut needles: Vec<Vec<u8>> = Vec::new();
    if spans.is_empty() {
        return needles;
    }
    let mut rng = datagen::Rng::new(77);
    let mut attempts = 0usize;
    while needles.len() < 20 && attempts < 2000 {
        attempts += 1;
        let s = &spans[rng.below(spans.len())];
        if !matches!(s.span_kind, SpanKind::Llm) {
            continue;
        }
        let Some(out) = s.output.as_deref() else {
            continue;
        };
        let mut start = out.len() / 2;
        while start < out.len() && !out.is_char_boundary(start) {
            start += 1;
        }
        let want = rng.range(8, 32);
        let mut end = (start + want).min(out.len());
        while end > start && !out.is_char_boundary(end) {
            end -= 1;
        }
        let Some(sub) = out.get(start..end) else {
            continue;
        };
        if sub.len() < 8 || sub.contains(['%', '_', '\\', '\'', '\n', '\r']) {
            continue;
        }
        let finder = memchr::memmem::Finder::new(sub.as_bytes());
        let mut hits = 0usize;
        for sp in spans {
            if sp.input.as_deref().is_some_and(|inp| finder.find(inp.as_bytes()).is_some()) {
                hits += 1;
                if hits > 1000 {
                    break;
                }
            }
        }
        if (1..=1000).contains(&hits) {
            println!("needle {sub:?}: {hits} hits");
            needles.push(sub.as_bytes().to_vec());
        }
    }
    needles
}

const PQ_VARIANTS: &[&str] = &["pq_default", "pq_tuned", "pq_cetra"];
const VX_VARIANTS: &[&str] = &["vx_default", "vx_compact"];

fn parquet_props(variant: &str, schema: &Schema, rows: usize) -> WriterProperties {
    let zstd = Compression::ZSTD(ZstdLevel::try_new(3).unwrap());
    if variant == "pq_default" {
        return WriterProperties::builder().set_compression(zstd).build();
    }
    if variant == "pq_tuned" {
        let mut b = WriterProperties::builder()
            .set_writer_version(parquet::file::properties::WriterVersion::PARQUET_2_0)
            .set_compression(zstd)
            .set_data_page_size_limit(64 << 10)
            .set_data_page_row_count_limit(8192)
            .set_statistics_enabled(EnabledStatistics::Page)
            .set_max_row_group_row_count(Some(128 * 1024));
        for name in BLOOM {
            let p = ColumnPath::from(*name);
            b = b
                .set_column_bloom_filter_enabled(p.clone(), true)
                .set_column_bloom_filter_fpp(p.clone(), 0.01)
                .set_column_bloom_filter_max_ndv(p, (rows / 4).clamp(1, 128 * 1024) as u64);
        }
        for name in ["input", "output", "system_instructions", "tool_definitions", "attributes", "resource_attributes", "input_preview", "output_preview", "events"] {
            let desc = parquet::arrow::ArrowSchemaConverter::new().convert(schema).unwrap();
            for c in desc.columns() {
                if c.path().parts()[0] == name {
                    b = b.set_column_dictionary_enabled(c.path().clone(), false);
                }
            }
        }
        return b.build();
    }
    let mut b = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .set_dictionary_enabled(false)
        .set_data_page_size_limit(1 << 20)
        .set_data_page_row_count_limit(20_000)
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_offset_index_disabled(false)
        .set_max_row_group_row_count(None);
    let desc = parquet::arrow::ArrowSchemaConverter::new().convert(schema).unwrap();
    for c in desc.columns() {
        let top = c.path().parts()[0].as_str();
        if DICT.contains(&top) {
            b = b.set_column_dictionary_enabled(c.path().clone(), true);
        }
    }
    for name in BLOOM {
        let p = ColumnPath::from(*name);
        b = b
            .set_column_bloom_filter_enabled(p.clone(), true)
            .set_column_bloom_filter_fpp(p.clone(), 0.01)
            .set_column_bloom_filter_max_ndv(p, rows.clamp(1, 100_000) as u64);
    }
    b.build()
}

/// Vortex has no FixedSizeBinary: map it to Binary, recursively.
fn for_vortex(b: &RecordBatch) -> R<RecordBatch> {
    fn map_type(t: &DataType) -> DataType {
        match t {
            DataType::FixedSizeBinary(_) => DataType::Binary,
            DataType::List(f) => DataType::List(Arc::new(map_field(f))),
            DataType::Struct(fs) => DataType::Struct(Fields::from(fs.iter().map(|f| map_field(f)).collect::<Vec<_>>())),
            other => other.clone(),
        }
    }
    fn map_field(f: &Field) -> Field {
        f.clone().with_data_type(map_type(f.data_type()))
    }
    let fields: Vec<Field> = b.schema().fields().iter().map(|f| map_field(f)).collect();
    let cols: Vec<ArrayRef> = b
        .columns()
        .iter()
        .zip(&fields)
        .map(|(c, f)| if c.data_type() == f.data_type() { Ok(c.clone()) } else { cast(c, f.data_type()) })
        .collect::<Result<_, _>>()?;
    Ok(RecordBatch::try_new(Arc::new(Schema::new(fields)), cols)?)
}

/// Compressed bytes per top-level column of one Parquet variant, largest first.
/// Prints the top 20 for `pq_tuned`.
fn column_sizes_parquet(dir: &Path, variant: &str) -> R<Vec<(String, u64)>> {
    use std::collections::BTreeMap;
    let mut sizes: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for e in std::fs::read_dir(dir)? {
        let f = std::fs::File::open(e?.path())?;
        let r = parquet::file::serialized_reader::SerializedFileReader::new(f)?;
        use parquet::file::reader::FileReader;
        for rg in r.metadata().row_groups() {
            for c in rg.columns() {
                let top = c.column_path().parts()[0].clone();
                let e = sizes.entry(top).or_default();
                e.0 += c.compressed_size();
                e.1 += c.uncompressed_size();
            }
        }
    }
    let mut v: Vec<_> = sizes.into_iter().collect();
    v.sort_by_key(|(_, (c, _))| -c);
    if variant == "pq_tuned" {
        println!("parquet column sizes (compressed / uncompressed KiB):");
        for (n, (c, u)) in v.iter() {
            println!("  {n:24} {:>10} {:>10}  x{:.1}", c >> 10, u >> 10, *u as f64 / (*c).max(1) as f64);
        }
    }
    Ok(v.into_iter().map(|(n, (c, _))| (n, c as u64)).collect())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ------------------------------------------------------------------ query

// ------------------------------------------------------------------ storage

/// A dataset: its store (local or S3), the root URL DataFusion knows that store by, and
/// the dataset's path under that root. Local: root `file:///`, path = the absolute
/// directory. S3: root `s3://bucket/`, path = the key.
struct Location {
    store: Arc<counting::Counting>,
    url: url::Url,
    prefix: ObjPath,
}

/// A new S3 client for `bucket`, from the `AWS_*` variables (see USAGE). Each call makes its own
/// HTTP client, so its first request opens a new connection.
fn s3_store(bucket: &str) -> R<Arc<dyn ObjectStore>> {
    Ok(Arc::new(
        AmazonS3Builder::from_env()
            .with_bucket_name(bucket)
            .with_virtual_hosted_style_request(false)
            .build()?,
    ))
}

/// `dir` is a local directory, or `s3://bucket/key` (AWS settings: see USAGE).
fn location(dir: &str) -> R<Location> {
    if dir.starts_with("s3://") {
        let (bucket, key) = probe::split_url(dir)?;
        let mut store = s3_store(&bucket)?;
        // FMT_HEDGE_MS=n: a GET still running after n ms gets a second copy (hedge.rs).
        if let Some(ms) = std::env::var("FMT_HEDGE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            store = Arc::new(hedge::Hedge::new(store, std::time::Duration::from_millis(ms)));
            println!("hedged GETs after {ms} ms");
        }
        return Ok(Location {
            store: Arc::new(counting::Counting::new(store)),
            url: url::Url::parse(&format!("s3://{bucket}/"))?,
            prefix: ObjPath::parse(&key)?,
        });
    }
    let abs = std::fs::canonicalize(dir)?;
    let mut inner: Arc<dyn ObjectStore> = Arc::new(LocalFileSystem::new());
    // FMT_SIM_S3=1: local reads take as long as the S3 model says (sim.rs).
    if std::env::var_os("FMT_SIM_S3").is_some() {
        let model = sim::Model::from_env();
        println!("simulated S3: {model}");
        inner = Arc::new(sim::SimS3::new(inner, model));
    }
    Ok(Location {
        store: Arc::new(counting::Counting::new(inner)),
        url: url::Url::parse("file:///")?,
        prefix: ObjPath::from_filesystem_path(&abs)?,
    })
}

/// A session context with the Parquet pushdown settings, reading `loc`'s store.
fn new_ctx(loc: &Location, partitions: Option<usize>) -> R<SessionContext> {
    let mut cfg = SessionConfig::new()
        .set_bool("datafusion.execution.parquet.pushdown_filters", true)
        .set_bool("datafusion.execution.parquet.reorder_filters", true);
    if let Some(n) = partitions {
        cfg = cfg.with_target_partitions(n);
    }
    // Strata's aggregate rule (strata/src/aggregate.rs) groups on dictionary codes;
    // FMT_NO_AGG_RULE=1 leaves every aggregate to DataFusion, for comparison.
    let mut builder = SessionStateBuilder::new().with_config(cfg).with_default_features();
    if std::env::var_os("FMT_NO_AGG_RULE").is_none() {
        builder = builder.with_physical_optimizer_rule(Arc::new(strata::StrataAggregateRule));
    }
    let ctx = SessionContext::new_with_state(builder.build());
    ctx.register_object_store(&loc.url, loc.store.clone());
    Ok(ctx)
}

/// The text of `name` in the dataset directory, or None when there is no such object.
async fn read_text(loc: &Location, name: &str) -> R<Option<String>> {
    match loc.store.get(&loc.prefix.clone().join(name)).await {
        Ok(r) => Ok(Some(String::from_utf8(r.bytes().await?.to_vec())?)),
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether the variant directory holds at least one object.
async fn has_files(loc: &Location, variant: &str) -> R<bool> {
    let mut objects = loc.store.list(Some(&loc.prefix.clone().join(variant)));
    Ok(objects.try_next().await?.is_some())
}

/// Each file's tail length from its first open, given back as the open's hint when
/// `FMT_OPEN_HINT` is set, as a catalog would (`FileSummary::tail_len`).
static TAIL_HINTS: std::sync::Mutex<Option<std::collections::HashMap<ObjPath, u64>>> = std::sync::Mutex::new(None);

/// Strata's `Config` for the bench: the default, with the `FMT_OPEN_SUFFIX`, `FMT_POINT_GAP` and
/// `FMT_POINT_MAX_RANGE` overrides.
fn strata_config() -> Config {
    let mut cfg = Config::default();
    if let Some(n) = std::env::var("FMT_OPEN_SUFFIX").ok().and_then(|v| v.parse().ok()) {
        cfg.open_default_suffix = n;
    }
    // FMT_POINT_GAP: the largest gap a lookup's read merges (`Config::point_gap`).
    if let Some(n) = std::env::var("FMT_POINT_GAP").ok().and_then(|v| v.parse().ok()) {
        cfg.point_gap = n;
    }
    // FMT_POINT_MAX_RANGE: the largest range a lookup's read merges into.
    if let Some(n) = std::env::var("FMT_POINT_MAX_RANGE").ok().and_then(|v| v.parse().ok()) {
        cfg.point_max_range = n;
    }
    cfg
}

/// The `.strata` files of a variant, sorted by path, opened concurrently, with `strata_config`.
/// `FMT_OPEN_HINT` gives each file its first open's tail length as the next open's hint.
async fn open_strata(loc: &Location, variant: &str) -> R<Vec<Arc<StrataFile>>> {
    let mut metas: Vec<ObjectMeta> = loc.store.list(Some(&loc.prefix.clone().join(variant))).try_collect().await?;
    metas.retain(|m| m.location.to_string().ends_with(".strata"));
    metas.sort_by(|a, b| a.location.cmp(&b.location));
    let cfg = strata_config();
    let use_hints = std::env::var_os("FMT_OPEN_HINT").is_some();
    let hint = |p: &ObjPath| {
        let hints = TAIL_HINTS.lock().unwrap_or_else(|e| e.into_inner());
        if use_hints { hints.as_ref().and_then(|h| h.get(p).copied()) } else { None }
    };
    let store: Arc<dyn ObjectStore> = loc.store.clone();
    let paths: Vec<ObjPath> = metas.into_iter().map(|m| m.location).collect();
    let files = try_join_all(paths.iter().map(|p| StrataFile::open(store.clone(), p.clone(), hint(p), &cfg))).await?;
    if use_hints {
        let mut hints = TAIL_HINTS.lock().unwrap_or_else(|e| e.into_inner());
        let map = hints.get_or_insert_with(std::collections::HashMap::new);
        for (p, f) in paths.iter().zip(&files) {
            map.entry(p.clone()).or_insert_with(|| f.tail_len());
        }
    }
    Ok(files.into_iter().map(Arc::new).collect())
}

/// The on-disk format of a directory variant (`pq_*`, `vx_*`, `st`).
enum Format {
    Parquet,
    Vortex,
    Strata,
}

impl Format {
    fn of(variant: &str) -> R<Self> {
        match variant.split('_').next() {
            Some("pq") => Ok(Self::Parquet),
            Some("vx") => Ok(Self::Vortex),
            Some("st") => Ok(Self::Strata),
            _ => Err(format!("unknown directory variant {variant}").into()),
        }
    }
}

/// Registers the variant's directory as table `t`. Returns the Strata files (empty for
/// the other formats).
async fn table(ctx: &SessionContext, loc: &Location, variant: &str) -> R<Vec<Arc<StrataFile>>> {
    match Format::of(variant)? {
        Format::Strata => {
            let files = open_strata(loc, variant).await?;
            ctx.register_table("t", Arc::new(StrataTable::new(files.clone())?))?;
            Ok(files)
        }
        format => {
            let url = ListingTableUrl::parse(&format!("{}{}/", loc.url, loc.prefix.clone().join(variant)))?;
            let opts = if matches!(format, Format::Vortex) {
                let s = VortexSession::default().with_tokio();
                ListingOptions::new(Arc::new(vortex_datafusion::VortexFormat::new(s))).with_file_extension(".vortex")
            } else {
                ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet")
            };
            let cfg = ListingTableConfig::new(url).with_listing_options(opts).infer_schema(&ctx.state()).await?;
            ctx.register_table("t", Arc::new(ListingTable::try_new(cfg)?))?;
            Ok(Vec::new())
        }
    }
}

// ------------------------------------------------------------------ query

/// Columns of the 9-column light trace lookup (shared with `profile`).
pub const LIGHT_COLS: [&str; 9] = ["span_id", "parent_span_id", "name", "start_time", "end_time", "status", "model", "total_tokens", "cost_total"];
/// Columns of the top-50 LLM list (shared with `profile`).
pub const LIST_COLS: [&str; 10] = ["trace_id", "span_id", "name", "start_time", "duration_ns", "status", "model", "total_tokens", "cost_total", "input_preview"];

#[derive(Clone, Copy)]
enum Kind {
    Sql,
    TraceFull,
    TraceLight,
    ListLlm,
}

/// One query. `sql` is the statement for `Sql` and `ListLlm`; trace kinds build one per id.
struct Query {
    name: &'static str,
    kind: Kind,
    sql: String,
}

/// How a variant answers the queries: DataFusion over the registered table (with the Strata
/// files it reads, for waves), the Strata fast paths, or the Parquet lookups.
enum Runner {
    Sql(SessionContext, Vec<Arc<StrataFile>>),
    StFast(Vec<Arc<StrataFile>>),
    PqFast(Vec<pqfast::PqFile>),
}

fn parse_id(s: &str) -> R<[u8; 16]> {
    let k: [u8; 16] = profile::unhex(s).try_into().map_err(|_| "bad trace id")?;
    Ok(k)
}

fn first_line(e: impl std::fmt::Display) -> String {
    e.to_string().lines().next().unwrap_or("").to_owned()
}

/// Runs one query (all its statements, or all its lookups) and returns the row count, or None
/// when the runner does not support the query.
async fn run_query(r: &Runner, q: &Query, ids: &[[u8; 16]]) -> R<Option<usize>> {
    let t0 = datagen::DAY_START;
    let h = 3_600 * datagen::NS;
    match r {
        Runner::Sql(ctx, _) => {
            let sqls: Vec<String> = match q.kind {
                Kind::TraceFull => ids.iter().map(|id| format!("SELECT * FROM t WHERE trace_id = X'{}'", hex(id))).collect(),
                Kind::TraceLight => ids.iter().map(|id| format!("SELECT {} FROM t WHERE trace_id = X'{}'", LIGHT_COLS.join(", "), hex(id))).collect(),
                Kind::Sql | Kind::ListLlm => vec![q.sql.clone()],
            };
            let mut rows = 0;
            for s in &sqls {
                rows += ctx.sql(s).await?.collect().await?.iter().map(RecordBatch::num_rows).sum::<usize>();
            }
            Ok(Some(rows))
        }
        Runner::StFast(files) => {
            let schema = files.first().ok_or("no strata files")?.schema();
            match q.kind {
                Kind::TraceFull => {
                    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
                    let mut rows = 0;
                    for id in ids {
                        rows += trace_detail(files, *id, &names).await?.num_rows();
                    }
                    Ok(Some(rows))
                }
                Kind::TraceLight => {
                    let mut rows = 0;
                    for id in ids {
                        rows += trace_detail(files, *id, &LIGHT_COLS).await?.num_rows();
                    }
                    Ok(Some(rows))
                }
                Kind::ListLlm => {
                    let pred = Predicate::Cmp(schema.index_of("span_kind")?, CmpOp::Eq, Lit::Utf8("llm".into()));
                    let b = trace_list(files, (t0 + 14 * h)..(t0 + 15 * h), Some(pred), 50, &LIST_COLS).await?;
                    Ok(Some(b.num_rows()))
                }
                Kind::Sql => Ok(None),
            }
        }
        Runner::PqFast(files) => {
            let first = files.first().ok_or("no parquet files")?;
            let names: Vec<&str> = match q.kind {
                Kind::TraceFull => first.meta.schema().fields().iter().map(|f| f.name().as_str()).collect(),
                Kind::TraceLight => LIGHT_COLS.to_vec(),
                Kind::Sql | Kind::ListLlm => return Ok(None),
            };
            let mut proj: Vec<usize> = names.iter().map(|n| first.meta.schema().index_of(n)).collect::<Result<_, _>>()?;
            proj.sort_unstable();
            let mut rows = 0;
            for id in ids {
                for batches in try_join_all(files.iter().map(|f| pqfast::lookup(f, id, &proj))).await? {
                    rows += batches.iter().map(RecordBatch::num_rows).sum::<usize>();
                }
            }
            Ok(Some(rows))
        }
    }
}

/// The Strata files a runner reads (empty for Parquet lookups and non-Strata SQL).
fn strata_files(r: &Runner) -> &[Arc<StrataFile>] {
    match r {
        Runner::Sql(_, files) | Runner::StFast(files) => files.as_slice(),
        Runner::PqFast(_) => &[],
    }
}

/// I/O of one measured run. Trace queries divide every figure by the number of ids.
struct Io {
    reqs: f64,
    kib: f64,
    wmax: Option<f64>,
    wsum: Option<f64>,
}

fn reset_io(loc: &Location, r: &Runner) {
    loc.store.reset();
    for f in strata_files(r) {
        f.reset_io_stats();
    }
}

fn read_io(loc: &Location, r: &Runner, per: f64) -> Io {
    let (reqs, bytes) = loc.store.stats();
    let waves: Vec<u64> = strata_files(r).iter().map(|f| f.io_stats().waves).collect();
    if std::env::var("FMT_KINDS").is_ok() && !strata_files(r).is_empty() {
        const NAMES: [&str; 10] = ["-", "narrow", "frame", "refs", "header", "filters", "edges", "blocks", "dict", "keyfilter"];
        let mut by_kind = [0u64; 10];
        for f in strata_files(r) {
            for (k, b) in f.io_stats().by_kind.iter().enumerate() {
                by_kind[k] += b;
            }
        }
        let parts: Vec<String> = NAMES.iter().zip(by_kind).filter(|(_, b)| *b > 0).map(|(n, b)| format!("{n} {:.1}", b as f64 / 1024.0 / per)).collect();
        println!("    KiB by segment kind: {}", parts.join(", "));
    }
    Io {
        reqs: reqs as f64 / per,
        kib: bytes as f64 / 1024.0 / per,
        wmax: waves.iter().max().map(|w| *w as f64 / per),
        wsum: (!waves.is_empty()).then(|| waves.iter().sum::<u64>() as f64 / per),
    }
}

fn fmt_wave(w: Option<f64>) -> String {
    w.map_or("-".to_owned(), |w| format!("{w:.1}"))
}

/// Runs the queries on one runner: a warm-up run (lazy metadata loads), then `runs` measured
/// runs. The I/O figures come from the first measured run.
async fn measure(loc: &Location, label: &str, r: &Runner, queries: &[Query], ids: &[[u8; 16]], runs: usize, only: Option<&str>) -> R<()> {
    for q in queries {
        if only.is_some_and(|o| o != q.name) {
            continue;
        }
        match run_query(r, q, ids).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                println!("{label:16} {:20} n/a", q.name);
                continue;
            }
            Err(e) => {
                println!("{label:16} {:20} ERROR {}", q.name, first_line(&e));
                continue;
            }
        }
        let per = if matches!(q.kind, Kind::TraceFull | Kind::TraceLight) { ids.len() as f64 } else { 1.0 };
        let mut times = Vec::new();
        let mut rows = 0;
        let mut io = None;
        let mut cpus = Vec::new();
        let mut failed = None;
        for run in 0..runs {
            if run == 0 {
                reset_io(loc, r);
            }
            let c0 = ProcessTime::now();
            let t = Instant::now();
            let res = run_query(r, q, ids).await;
            let el = t.elapsed().as_secs_f64() * 1e3 / per;
            let cpu = c0.elapsed().as_secs_f64() * 1e3 / per;
            match res {
                Ok(Some(n)) => {
                    rows = n;
                    times.push(el);
                    cpus.push(cpu);
                }
                Ok(None) => {
                    failed = Some("unsupported".to_owned());
                    break;
                }
                Err(e) => {
                    failed = Some(first_line(&e));
                    break;
                }
            }
            if run == 0 {
                io = Some(read_io(loc, r, per));
                // FMT_TIMELINE=1: every request of the first measured run.
                let events = loc.store.timeline();
                if !events.is_empty() {
                    println!("timeline {label} {}: {:.1} ms; kind, object, KiB, start, first byte, end (ms)", q.name, el * per);
                    for e in &events {
                        println!("  {:6} {:24} {:>8.1} {:>7.1} {:>7.1} {:>7.1}", e.kind, e.name, e.bytes as f64 / 1024.0, e.start, e.first, e.end);
                    }
                }
            }
        }
        if let Some(e) = failed {
            println!("{label:16} {:20} ERROR {e}", q.name);
            continue;
        }
        let Some(io) = io else { continue };
        times.sort_by(f64::total_cmp);
        cpus.sort_by(f64::total_cmp);
        println!(
            "{label:16} {:20} median {:>9.2} ms  min {:>9.2} ms  cpu med {:>9.2} ms  rows {rows:>6}  reqs {:>8.1}  KiB {:>10.1}  waves_max {:>6}  waves_sum {:>6}",
            q.name,
            times[times.len() / 2],
            times[0],
            cpus[cpus.len() / 2],
            io.reqs,
            io.kib,
            fmt_wave(io.wmax),
            fmt_wave(io.wsum)
        );
    }
    Ok(())
}

/// The `.parquet` objects of a variant, sorted by path, opened concurrently.
async fn open_pq_fast(loc: &Location, variant: &str) -> R<Vec<pqfast::PqFile>> {
    let mut metas: Vec<ObjectMeta> = loc.store.list(Some(&loc.prefix.clone().join(variant))).try_collect().await?;
    metas.retain(|m| m.location.to_string().ends_with(".parquet"));
    metas.sort_by(|a, b| a.location.cmp(&b.location));
    let store: Arc<dyn ObjectStore> = loc.store.clone();
    try_join_all(metas.into_iter().map(|m| pqfast::open(store.clone(), m.location))).await
}

fn needle_sql(n: &str) -> String {
    format!("SELECT count(*) FROM t WHERE input LIKE '%{n}%'")
}

/// G7: one `LIKE` count per needle on each directory variant. Time is the minimum over `runs`
/// of the mean per needle, in one warm context. Cold bytes are the mean per needle over fresh
/// contexts.
async fn substring_needles(loc: &Location, variants: &[&str], needles: &[String], runs: usize) -> R<()> {
    let per = needles.len() as f64;
    for &v in variants {
        let ctx = new_ctx(loc, Some(1))?;
        table(&ctx, loc, v).await?;
        for n in needles {
            ctx.sql(&needle_sql(n)).await?.collect().await?;
        }
        let mut best = f64::INFINITY;
        for _ in 0..runs {
            let t = Instant::now();
            for n in needles {
                ctx.sql(&needle_sql(n)).await?.collect().await?;
            }
            best = best.min(t.elapsed().as_secs_f64() * 1e3 / per);
        }
        let mut kib = 0.0;
        for n in needles {
            let ctx = new_ctx(loc, Some(1))?;
            table(&ctx, loc, v).await?;
            loc.store.reset();
            ctx.sql(&needle_sql(n)).await?.collect().await?;
            kib += loc.store.stats().1 as f64 / 1024.0;
        }
        println!("substring_needles {v:16} min {best:>9.2} ms per needle  cold KiB {:>10.1} per needle  ({} needles, {runs} runs)", kib / per, needles.len());
    }
    Ok(())
}

async fn query(dir: &str, runs: usize, only: Option<String>) -> R<()> {
    let loc = location(dir)?;
    println!("loadavg before: {}", profile::loadavg());
    let ids: Vec<[u8; 16]> = read_text(&loc, "trace_ids.txt").await?.ok_or("no trace_ids.txt")?.lines().take(20).map(parse_id).collect::<R<_>>()?;
    let t0 = datagen::DAY_START;
    let h = 3_600 * datagen::NS;
    let ts = |ns: i64| format!("to_timestamp_nanos({ns})");
    // FMT_LIST_COLS=a,b: the list query's columns, to see what each one costs.
    let list_cols = std::env::var("FMT_LIST_COLS").map_or_else(|_| LIST_COLS.join(", "), |v| v.replace(',', ", "));
    let braintrust = std::env::var("FMT_QUERYSET").is_ok_and(|v| v == "braintrust");
    let queries = if braintrust { braintrust_queries() } else { vec![
        Query { name: "count_errors_6h", kind: Kind::Sql, sql: format!(
            "SELECT count(*) FROM t WHERE status = 'error' AND start_time >= {} AND start_time < {}", ts(t0 + 10 * h), ts(t0 + 16 * h)) },
        Query { name: "model_rollup_day", kind: Kind::Sql, sql: "SELECT model, count(*), sum(total_tokens), sum(cost_total), avg(duration_ns) FROM t WHERE span_kind = 'llm' GROUP BY model ORDER BY model".to_owned() },
        Query { name: "list_llm_1h", kind: Kind::ListLlm, sql: format!(
            "SELECT {list_cols} FROM t WHERE span_kind = 'llm' AND start_time >= {} AND start_time < {} ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50", ts(t0 + 14 * h), ts(t0 + 15 * h)) },
        Query { name: "user_filter", kind: Kind::Sql, sql: "SELECT count(*), sum(cost_total) FROM t WHERE user_id = 'user-1234'".to_owned() },
        Query { name: "metadata_filter", kind: Kind::Sql, sql: "SELECT count(*) FROM t WHERE metadata['customer_id'] = '\"cust-42\"'".to_owned() },
        Query { name: "trace_lookup_full", kind: Kind::TraceFull, sql: String::new() },
        Query { name: "trace_lookup_light", kind: Kind::TraceLight, sql: String::new() },
        Query { name: "substring_input", kind: Kind::Sql, sql: "SELECT count(*) FROM t WHERE input LIKE '%zqxjneedle%'".to_owned() },
        Query { name: "scan_all_payload", kind: Kind::Sql, sql: "SELECT sum(length(input)) + sum(length(output)) FROM t".to_owned() },
    ] };
    let only = only.as_deref();
    let needle_runs: usize = std::env::var("FMT_NEEDLE_RUNS").ok().map(|s| s.parse()).transpose()?.unwrap_or(runs);
    println!("{:16} {:20} {:>9} {:>9} {:>12} {:>6}  {:>8} {:>10}  {:>10} {:>10}", "variant", "query", "median", "min", "cpu med", "rows", "reqs", "KiB", "waves_max", "waves_sum");
    println!("(times are per query, or per id for trace queries; cpu is process CPU time; trace queries: reqs, KiB and waves per id; waves are for st and st_fast only; rows are totals)");

    let wanted: Option<Vec<String>> = std::env::var("FMT_VARIANTS").ok().map(|v| v.split(',').map(str::to_owned).collect());
    let wants = |v: &str| wanted.as_ref().is_none_or(|w| w.iter().any(|x| x == v));
    let variants: Vec<&str> = PQ_VARIANTS.iter().chain(VX_VARIANTS).chain(&["st"]).copied().filter(|v| wants(v)).collect();
    let mut dirs: Vec<&str> = Vec::new();
    for variant in variants {
        if !has_files(&loc, variant).await? {
            continue;
        }
        let ctx = new_ctx(&loc, None)?;
        let files = table(&ctx, &loc, variant).await?;
        if braintrust {
            braintrust_counts(&ctx, variant).await?;
        }
        measure(&loc, variant, &Runner::Sql(ctx, files), &queries, &ids, runs, only).await?;
        dirs.push(variant);
    }
    if wants("st_fast") && has_files(&loc, "st").await? {
        let files = open_strata(&loc, "st").await?;
        measure(&loc, "st_fast", &Runner::StFast(files), &queries, &ids, runs, only).await?;
    }
    for variant in PQ_VARIANTS {
        if wants(&format!("pq_fast/{variant}")) && has_files(&loc, variant).await? {
            let files = open_pq_fast(&loc, variant).await?;
            measure(&loc, &format!("pq_fast/{variant}"), &Runner::PqFast(files), &queries, &ids, runs, only).await?;
        }
    }
    if only.is_none_or(|o| o == "substring_needles") {
        if let Some(text) = read_text(&loc, "needles.txt").await? {
            let mut needles = Vec::new();
            for line in text.lines() {
                needles.push(String::from_utf8(profile::unhex(line))?);
            }
            if !needles.is_empty() {
                substring_needles(&loc, &dirs, &needles, needle_runs).await?;
            }
        }
    }
    if only.is_none_or(|o| o == "trace_lookup_cold") {
        trace_lookup_cold(&loc, &ids, runs).await?;
    }
    println!("loadavg after: {}", profile::loadavg());
    Ok(())
}

// ------------------------------------------------------------------ Braintrust's search benchmark

/// Braintrust's search benchmark (blog post "Nitro", 2026-09-24) on FineWeb: 4 million spans of
/// 10 KB, a rare phrase planted in 4,000 of them and a longer one in one span. These are its
/// phrases; `BT_NONE` occurs nowhere.
const BT_RARE: &str = "red car under the yellow bridge";
const BT_SINGLE: &str = "when the red car slowly went under the yellow bridge";
const BT_NONE: &str = "under the red bridge the yellow car slowly went home";
/// Bytes of FineWeb text per span.
const BT_SPAN_BYTES: usize = 10_000;
/// The spans' start times are spread evenly over this many days.
const BT_DAYS: i64 = 3;
/// The formats written for the comparison.
const BT_VARIANTS: [&str; 3] = ["pq_default", "vx_compact", "st"];

/// The queries of Braintrust's post that it gives numbers for: four searches and one dashboard
/// aggregate. Its phrase search matches words; `LIKE` matches bytes, so `help me` also counts
/// `help meet` and not `Help me`.
fn braintrust_queries() -> Vec<Query> {
    let like = |phrase: &str| format!("input LIKE '%{phrase}%'");
    let end = datagen::DAY_START + BT_DAYS * 86_400 * datagen::NS;
    let last = |secs: i64| format!("start_time >= to_timestamp_nanos({})", end - secs * datagen::NS);
    vec![
        Query { name: "bt_phrase_none", kind: Kind::Sql, sql: format!("SELECT count(*) FROM t WHERE {}", like(BT_NONE)) },
        Query { name: "bt_help_me", kind: Kind::Sql, sql: format!("SELECT count(*) FROM t WHERE {}", like("help me")) },
        Query { name: "bt_newest_help_me", kind: Kind::Sql, sql: format!("SELECT input FROM t WHERE {} ORDER BY start_time DESC LIMIT 100", like("help me")) },
        Query { name: "bt_newest_rare", kind: Kind::Sql, sql: format!("SELECT input FROM t WHERE {} ORDER BY start_time DESC LIMIT 100", like(BT_RARE)) },
        Query { name: "bt_single", kind: Kind::Sql, sql: format!("SELECT input FROM t WHERE {} ORDER BY start_time DESC LIMIT 100", like(BT_SINGLE)) },
        // A time range ending at the newest span, as a UI's time picker would select.
        Query { name: "bt_help_me_1d", kind: Kind::Sql, sql: format!("SELECT count(*) FROM t WHERE {} AND {}", like("help me"), last(86_400)) },
        Query { name: "bt_help_me_1h", kind: Kind::Sql, sql: format!("SELECT count(*) FROM t WHERE {} AND {}", like("help me"), last(3_600)) },
        Query { name: "bt_newest_help_me_5m", kind: Kind::Sql, sql: format!("SELECT input FROM t WHERE {} AND {} ORDER BY start_time DESC LIMIT 100", like("help me"), last(300)) },
        Query { name: "bt_newest_rare_1h", kind: Kind::Sql, sql: format!("SELECT input FROM t WHERE {} AND {} ORDER BY start_time DESC LIMIT 100", like(BT_RARE), last(3_600)) },
        Query { name: "bt_spans_per_hour", kind: Kind::Sql, sql: "SELECT date_trunc('hour', start_time) AS h, count(*) FROM t GROUP BY h ORDER BY h".to_owned() },
    ]
}

/// Prints the match counts of `variant` for every phrase, so the variants can be checked
/// against each other.
async fn braintrust_counts(ctx: &SessionContext, variant: &str) -> R<()> {
    let mut counts = Vec::new();
    for phrase in [BT_NONE, "help me", BT_RARE, BT_SINGLE] {
        let batches = ctx.sql(&format!("SELECT count(*) FROM t WHERE input LIKE '%{phrase}%'")).await?.collect().await?;
        let n = batches
            .first()
            .and_then(|b| b.column(0).as_any().downcast_ref::<arrow::array::Int64Array>())
            .map_or(-1, |a| a.value(0));
        counts.push(format!("{phrase:?} {n}"));
    }
    println!("braintrust counts {variant}: {}", counts.join(", "));
    Ok(())
}

/// Inserts ` phrase` after the first space past the middle of `text`, or at its end.
fn plant(text: &mut String, phrase: &str) {
    let mid = text.floor_char_boundary(text.len() / 2);
    let at = text[mid..].find(' ').map_or(text.len(), |p| mid + p);
    text.insert_str(at, &format!(" {phrase}"));
}

/// Writes Braintrust's benchmark data: `spans` spans of `BT_SPAN_BYTES` of FineWeb text from
/// `paths`, one trace each, with start times spread over `BT_DAYS` days, as files of `per_file`
/// spans in `BT_VARIANTS`. The files are time slices, each sorted by (trace_id, start_time),
/// like `gen`'s. Spans are made and written one file at a time, so memory holds one file.
async fn fineweb_cmd(dir: &Path, spans: usize, per_file: usize, paths: Vec<PathBuf>) -> R<()> {
    let world = datagen::World::new(7);
    let mut rng = datagen::Rng::new(42);
    let mut pieces = fmt_testdata::fineweb::Pieces::new(paths, BT_SPAN_BYTES);
    let step = BT_DAYS * 86_400 * datagen::NS / spans.max(1) as i64;
    let single = spans * 2 / 3;
    for v in BT_VARIANTS {
        std::fs::create_dir_all(dir.join(v))?;
    }
    let (mut bytes, mut times) = ([0u64; 3], [std::time::Duration::ZERO; 3]);
    let (mut made, mut file, mut raw) = (0usize, 0usize, 0usize);
    let mut ids = Vec::new();
    let mut planted = 0usize;
    let t0 = Instant::now();
    'files: while made < spans {
        let mut out: Vec<Span> = Vec::with_capacity(per_file);
        while out.len() < per_file && made < spans {
            let Some(mut text) = pieces.next_piece()? else {
                if out.is_empty() {
                    break 'files;
                }
                break;
            };
            if made % 1000 == 500 {
                plant(&mut text, BT_RARE);
                planted += 1;
            }
            if made == single {
                plant(&mut text, BT_SINGLE);
            }
            let mut trace = Vec::new();
            fmt_testdata::swe::trace_spans(made, &world, &mut rng, Some(text), None, Vec::new(), &mut trace);
            let mut s = trace.swap_remove(0);
            s.system_instructions = None;
            s.tool_definitions = None;
            let duration = s.end_time - s.start_time;
            s.start_time = datagen::DAY_START + made as i64 * step;
            s.end_time = s.start_time + duration;
            if made % (spans / 100).max(1) == 0 {
                ids.push(hex(s.trace_id.as_bytes()));
            }
            out.push(s);
            made += 1;
        }
        let short = out.len() < per_file && made < spans;
        let mut batches = Vec::new();
        for c in out.chunks(8192) {
            batches.push(spans_to_record_batch(c)?);
        }
        drop(out);
        let b = concat_batches(&batches[0].schema(), &batches)?;
        drop(batches);
        let idx = lexsort_to_indices(
            &[
                SortColumn { values: b.column_by_name("trace_id").ok_or("no trace_id")?.clone(), options: None },
                SortColumn { values: b.column_by_name("start_time").ok_or("no start_time")?.clone(), options: None },
            ],
            None,
        )?;
        let b = take_record_batch(&b, &idx)?;
        raw += b.get_array_memory_size();
        for (k, v) in BT_VARIANTS.iter().enumerate() {
            let t = Instant::now();
            bytes[k] += match *v {
                "st" => write_strata(&b, &dir.join(v).join(format!("{file:03}.strata")))?.0,
                "vx_compact" => write_vortex(v, &b, &dir.join(v).join(format!("{file:03}.vortex"))).await?,
                _ => write_parquet(v, &b, &dir.join(v).join(format!("{file:03}.parquet")))?,
            };
            times[k] += t.elapsed();
        }
        file += 1;
        println!("file {file}: {made} spans, {:.0?}", t0.elapsed());
        if short {
            break;
        }
    }
    std::fs::write(dir.join("trace_ids.txt"), ids.join("\n"))?;
    println!("fineweb: {made} spans in {file} files ({BT_RARE:?} in {planted}, {BT_SINGLE:?} in span {single}), arrow {} MiB", raw >> 20);
    for (k, v) in BT_VARIANTS.iter().enumerate() {
        println!("{v:12} {:>7} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", bytes[k] >> 20, times[k], raw as f64 / 1048576.0 / times[k].as_secs_f64());
    }
    Ok(())
}

// ------------------------------------------------------------------ cold lookups

/// Max over files of the waves each has run so far (0 with no files).
fn max_waves(files: &[Arc<StrataFile>]) -> u64 {
    files.iter().map(|f| f.io_stats().waves).max().unwrap_or(0)
}

/// Waves of a Strata open: 1 when it issued one request per file, else 2.
fn open_waves(open_reqs: u64, files: usize) -> u64 {
    if open_reqs == files as u64 { 1 } else { 2 }
}

/// One lookup of `id` from nothing: a fresh context and fresh files, then the lookup. Returns
/// the row count and, for Strata, the waves of the open plus the lookup.
async fn cold_one(loc: &Location, label: &str, id: &[u8; 16]) -> R<(usize, Option<u64>)> {
    if label == "st_fast" {
        let files = open_strata(loc, "st").await?;
        let open = open_waves(loc.store.stats().0, files.len());
        let schema = files.first().ok_or("no strata files")?.schema();
        let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        let rows = trace_detail(&files, *id, &names).await?.num_rows();
        return Ok((rows, Some(open + max_waves(&files))));
    }
    if let Some(v) = label.strip_prefix("pq_fast/") {
        let files = open_pq_fast(loc, v).await?;
        let width = files.first().ok_or("no parquet files")?.meta.schema().fields().len();
        let proj: Vec<usize> = (0..width).collect();
        let mut rows = 0;
        for batches in try_join_all(files.iter().map(|f| pqfast::lookup(f, id, &proj))).await? {
            rows += batches.iter().map(RecordBatch::num_rows).sum::<usize>();
        }
        return Ok((rows, None));
    }
    let ctx = new_ctx(loc, None)?;
    let files = table(&ctx, loc, label).await?;
    let open_reqs = loc.store.stats().0;
    let sql = format!("SELECT * FROM t WHERE trace_id = X'{}'", hex(id));
    let rows = ctx.sql(&sql).await?.collect().await?.iter().map(RecordBatch::num_rows).sum::<usize>();
    let waves = matches!(Format::of(label)?, Format::Strata).then(|| open_waves(open_reqs, files.len()) + max_waves(&files));
    Ok((rows, waves))
}

/// G1b: every id looked up from nothing, one pass per run. There is no in-process cache: each
/// lookup builds a new context (SQL) or opens new files (Strata, pq_fast). On local disk the OS
/// page cache stays warm. Bytes and requests come from `Counting` after each lookup, so they
/// include the open; HEAD and LIST requests are not counted. Time, bytes, requests and waves are
/// means per id; rows are the sum over the ids of one pass.
async fn trace_lookup_cold(loc: &Location, ids: &[[u8; 16]], runs: usize) -> R<()> {
    let n = (runs * ids.len()) as f64;
    let mut base = Vec::new();
    for _ in 0..runs * ids.len() {
        let t = Instant::now();
        let ctx = new_ctx(loc, None)?;
        ctx.sql("SELECT 1").await?.collect().await?;
        base.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("SELECT 1 in a fresh context: mean {:.2} ms over {} runs", base.iter().sum::<f64>() / n, base.len());
    // FMT_OPEN_HINT: learn each file's tail length before timing, as a catalog would hold it.
    if std::env::var_os("FMT_OPEN_HINT").is_some() && has_files(loc, "st").await? {
        open_strata(loc, "st").await?;
        println!("Strata opens with each file's tail length as the hint");
    }
    println!("trace_lookup_cold: {} ids, {runs} passes; reqs exclude HEAD and LIST and count ranges (object_store merges ranges under 1 MiB apart on S3); waves = open + lookup (st, st_fast); local disk: OS page cache warm", ids.len());
    println!("{:16} {:>10} {:>10} {:>10} {:>8} {:>8}", "variant", "mean ms", "KiB", "reqs", "waves", "rows");
    let mut plan: Vec<(String, String)> = PQ_VARIANTS.iter().chain(VX_VARIANTS).map(|v| (v.to_string(), v.to_string())).collect();
    plan.push(("st".into(), "st".into()));
    plan.push(("st_fast".into(), "st".into()));
    plan.extend(PQ_VARIANTS.iter().map(|v| (format!("pq_fast/{v}"), v.to_string())));
    let wanted: Option<Vec<String>> = std::env::var("FMT_VARIANTS").ok().map(|v| v.split(',').map(str::to_owned).collect());
    for (label, dir) in &plan {
        if wanted.as_ref().is_some_and(|w| !w.iter().any(|x| x == label)) || !has_files(loc, dir).await? {
            continue;
        }
        let (mut ms, mut kib, mut reqs, mut waves, mut rows) = (0.0, 0.0, 0.0, None::<u64>, 0usize);
        let mut failed = None;
        let mut shown = false;
        hedge::HEDGED.store(0, std::sync::atomic::Ordering::Relaxed);
        'passes: for _ in 0..runs {
            for id in ids {
                loc.store.reset();
                let t = Instant::now();
                match cold_one(loc, label, id).await {
                    Ok((r, w)) => {
                        ms += t.elapsed().as_secs_f64() * 1e3;
                        // FMT_TIMELINE=1: every request of the variant's first lookup.
                        let events = loc.store.timeline();
                        if !shown && !events.is_empty() {
                            shown = true;
                            println!("timeline {label}: {:.1} ms; kind, object, KiB, start, first byte, end (ms)", t.elapsed().as_secs_f64() * 1e3);
                            for e in &events {
                                println!("  {:6} {:24} {:>8.1} {:>7.1} {:>7.1} {:>7.1}", e.kind, e.name, e.bytes as f64 / 1024.0, e.start, e.first, e.end);
                            }
                        }
                        let (q, b) = loc.store.stats();
                        reqs += q as f64;
                        kib += b as f64 / 1024.0;
                        rows += r;
                        if let Some(w) = w {
                            waves = Some(waves.unwrap_or(0) + w);
                        }
                    }
                    Err(e) => {
                        failed = Some(first_line(&e));
                        break 'passes;
                    }
                }
            }
        }
        if let Some(e) = failed {
            println!("{label:16} ERROR {e}");
            continue;
        }
        let hedged = hedge::HEDGED.swap(0, std::sync::atomic::Ordering::Relaxed);
        if hedged > 0 {
            println!("{label:16} hedged {:.1} GETs per lookup", hedged as f64 / n);
        }
        let w = waves.map_or("-".to_owned(), |w| format!("{:.1}", w as f64 / n));
        println!("{label:16} {:>10.2} {:>10.1} {:>10.1} {w:>8} {:>8}", ms / n, kib / n, reqs / n, rows / runs);
    }
    Ok(())
}

/// Prints the top functions of a CPU profile by self and by inclusive samples.
fn print_profile(report: &pprof::Report) {
    use std::collections::HashMap;
    let mut own: HashMap<String, isize> = HashMap::new();
    let mut incl: HashMap<String, isize> = HashMap::new();
    let mut total = 0isize;
    for (frames, count) in &report.data {
        total += count;
        let names: Vec<String> = frames
            .frames
            .iter()
            .map(|f| f.first().map_or_else(|| "?".to_owned(), |s| s.name()))
            .map(|n| n.split("::h").next().unwrap_or(&n).to_owned())
            .collect();
        if let Some(top) = names.first() {
            *own.entry(top.clone()).or_default() += count;
        }
        let mut seen = std::collections::HashSet::new();
        for n in names {
            if seen.insert(n.clone()) {
                *incl.entry(n).or_default() += count;
            }
        }
    }
    let show = |title: &str, m: HashMap<String, isize>, n: usize, filter: bool| {
        let mut v: Vec<_> = m.into_iter().filter(|(k, _)| !filter || k.contains("strata") || k.contains("arrow") || k.contains("datafusion") || k.contains("zstd")).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        println!("== profile: {title} ({total} samples)");
        for (k, c) in v.into_iter().take(n) {
            println!("  {:5.1}%  {}", 100.0 * c as f64 / total.max(1) as f64, k.chars().take(150).collect::<String>());
        }
    };
    show("self", own, 40, false);
    show("inclusive, strata/arrow/datafusion/zstd", incl, 60, true);
    // FMT_PPROF_UNDER=<name>: the samples under the innermost frame whose name contains `name`,
    // by the frame it calls directly.
    if let Ok(target) = std::env::var("FMT_PPROF_UNDER") {
        let mut callees: HashMap<String, isize> = HashMap::new();
        for (frames, count) in &report.data {
            let names: Vec<String> = frames
                .frames
                .iter()
                .map(|f| f.first().map_or_else(|| "?".to_owned(), |s| s.name()))
                .map(|n| n.split("::h").next().unwrap_or(&n).to_owned())
                .collect();
            if let Some(p) = names.iter().position(|n| n.contains(&target)) {
                let callee = if p == 0 { "(self)".to_owned() } else { names[p - 1].clone() };
                *callees.entry(callee).or_default() += count;
            }
        }
        show(&format!("under {target}"), callees, 40, false);
    }
    // FMT_PPROF_CALLERS=<name>: the samples whose leaf frame contains `name`, by the first two
    // frames above it.
    if let Ok(target) = std::env::var("FMT_PPROF_CALLERS") {
        let mut callers: HashMap<String, isize> = HashMap::new();
        for (frames, count) in &report.data {
            let names: Vec<String> = frames
                .frames
                .iter()
                .map(|f| f.first().map_or_else(|| "?".to_owned(), |s| s.name()))
                .map(|n| n.split("::h").next().unwrap_or(&n).chars().take(70).collect())
                .collect();
            if names.first().is_some_and(|n| n.contains(&target)) {
                let depth: usize = std::env::var("FMT_PPROF_DEPTH").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
                let up: Vec<&str> = names.iter().skip(1).take(depth).map(String::as_str).collect();
                *callers.entry(up.join(" < ")).or_default() += count;
            }
        }
        let mut v: Vec<_> = callers.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        println!("== profile: callers of {target} ({total} samples)");
        for (k, c) in v.into_iter().take(12) {
            println!("  {:5.1}%  {}", 100.0 * c as f64 / total.max(1) as f64, k.replace(" < ", "\n          < "));
        }
    }
}

// ------------------------------------------------------------------ lookupcols

/// The narrow chunk bytes a lookup of each of the 20 trace ids reads, by column (mean per id).
async fn lookup_cols(dir: &str) -> R<()> {
    let loc = location(dir)?;
    let ids: Vec<[u8; 16]> = read_text(&loc, "trace_ids.txt").await?.ok_or("no trace_ids.txt")?.lines().take(20).map(parse_id).collect::<R<_>>()?;
    let files = open_strata(&loc, "st").await?;
    let mut by_column: std::collections::BTreeMap<String, (u64, u64)> = std::collections::BTreeMap::new();
    for file in &files {
        let names: Vec<String> = (0..file.directory().len())
            .map(|c| file.schema().fields().get(c).map_or_else(|| format!("#{c}"), |f| f.name().clone()))
            .collect();
        for id in &ids {
            let ranges = file.lookup(id).await?;
            if ranges.is_empty() {
                continue;
            }
            for (c, name) in names.iter().enumerate() {
                let entries = file.block_entries(c).await?;
                for e in entries.iter() {
                    let end = e.first_row + e.n_rows;
                    if ranges.iter().any(|r| r.start < end && e.first_row < r.end) {
                        let slot = by_column.entry(name.clone()).or_default();
                        slot.0 += u64::from(e.segment_len);
                        slot.1 += u64::from(e.n_rows);
                    }
                }
            }
        }
    }
    let mut v: Vec<_> = by_column.into_iter().collect();
    v.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    let total: u64 = v.iter().map(|(_, (b, _))| b).sum();
    println!("narrow bytes per lookup: {:.1} KiB", total as f64 / 1024.0 / ids.len() as f64);
    for (name, (bytes, rows)) in v.iter().take(25) {
        println!("  {name:24} {:>8.1} KiB  {:>8.0} rows/chunk-read", *bytes as f64 / 1024.0 / ids.len() as f64, *rows as f64 / ids.len() as f64);
    }
    Ok(())
}

// ------------------------------------------------------------------ restrata

/// Rewrites the Strata files of a dataset from its `pq_default` files, which hold the same
/// slices, into `<dir>/<out>`. Used to measure a format change without regenerating the
/// other variants. `FMT_COLD=1` writes them with `Config::cold()`, as compaction would rewrite
/// cold files.
fn restrata_cmd(dir: &Path, out: &str) -> R<()> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.join("pq_default"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.sort();
    let out_dir = dir.join(out);
    std::fs::create_dir_all(&out_dir)?;
    let (mut raw, mut bytes, mut write_time) = (0u64, 0u64, std::time::Duration::ZERO);
    let (mut pq_bytes, mut pq_time) = (0u64, std::time::Duration::ZERO);
    let mut summaries = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(p)?)?.with_batch_size(65536).build()?;
        let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>()?;
        let schema = batches.first().ok_or("empty parquet file")?.schema();
        let b = concat_batches(&schema, &batches)?;
        raw += b.get_array_memory_size() as u64;
        // FMT_REPEAT=n: each format writes the file n times, interleaved, and the fastest time
        // of each counts, which keeps bursts of load on a shared machine out of G8.
        let repeat: usize = std::env::var("FMT_REPEAT").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
        let (mut st_best, mut pq_best) = (std::time::Duration::MAX, std::time::Duration::MAX);
        let mut written = None;
        let mut last_pq_len = 0u64;
        for _ in 0..repeat {
            let t = Instant::now();
            let config = if std::env::var_os("FMT_COLD").is_some() { Config::cold() } else { Config::default() };
            let mut w = StrataWriter::new(b.schema(), &["trace_id", "start_time"], config)?;
            for off in (0..b.num_rows()).step_by(8192) {
                w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
            }
            let (file, summary) = w.finish()?;
            st_best = st_best.min(t.elapsed());
            written = Some((file, summary));
            // G8's reference: the same rows written by `pq_default` into memory, 8,192 rows per
            // write call, one thread.
            let t = Instant::now();
            let mut pq = ArrowWriter::try_new(Vec::new(), b.schema(), Some(parquet_props("pq_default", &b.schema(), b.num_rows())))?;
            for off in (0..b.num_rows()).step_by(8192) {
                pq.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
            }
            let len = pq.into_inner()?.len() as u64;
            pq_best = pq_best.min(t.elapsed());
            last_pq_len = len;
        }
        pq_bytes += last_pq_len;
        write_time += st_best;
        pq_time += pq_best;
        let (file, summary) = written.ok_or("no write")?;
        std::fs::write(out_dir.join(format!("{i:03}.strata")), &file)?;
        bytes += file.len() as u64;
        summaries.push(summary);
    }
    println!("{out:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", bytes >> 20, write_time, raw as f64 / 1048576.0 / write_time.as_secs_f64());
    println!("{:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", "pq_default", pq_bytes >> 20, pq_time, raw as f64 / 1048576.0 / pq_time.as_secs_f64());
    println!("G8 write throughput: strata / pq_default = {:.3}", pq_time.as_secs_f64() / write_time.as_secs_f64());
    let mut payload_columns: Vec<(String, u64, u64)> = Vec::new();
    for s in &summaries {
        for c in &s.columns {
            if let Some(p) = &c.payload {
                match payload_columns.iter_mut().find(|(n, _, _)| *n == c.name) {
                    Some(entry) => {
                        entry.1 += c.bytes;
                        entry.2 += p.frame_bytes;
                    }
                    None => payload_columns.push((c.name.clone(), c.bytes, p.frame_bytes)),
                }
            }
        }
    }
    let total: u64 = payload_columns.iter().map(|(_, b, _)| b).sum();
    println!("payload-layer columns (KiB total, of which shared frames): {}; all {} KiB",
        payload_columns.iter().map(|(n, b, f)| format!("{n} {} ({})", b >> 10, f >> 10)).collect::<Vec<_>>().join(", "), total >> 10);
    payload_table(dir, &summaries)?;
    narrow_table(&summaries);
    Ok(())
}

/// The narrow columns of the Strata files: bytes, key-filter bytes, chunks and chunks per
/// encoding, summed over the files, largest first.
fn narrow_table(summaries: &[strata::FileSummary]) {
    const ENCODINGS: [&str; 14] = ["const", "null", "for", "plain", "fsb", "fsb_rle", "dict", "varbin", "list_dict", "bool", "ipc", "map", "float_dict", "for_delta"];
    let mut columns: Vec<(String, u64, u64, u32, [u32; 14])> = Vec::new();
    for s in summaries {
        for c in s.columns.iter().filter(|c| c.payload.is_none()) {
            let i = match columns.iter().position(|e| e.0 == c.name) {
                Some(i) => i,
                None => {
                    columns.push((c.name.clone(), 0, 0, 0, [0; 14]));
                    columns.len() - 1
                }
            };
            let e = &mut columns[i];
            e.1 += c.bytes;
            e.2 += c.filter_bytes;
            e.3 += c.chunks;
            for (k, n) in c.encoding_counts.iter().enumerate() {
                e.4[k] += n;
            }
        }
    }
    columns.sort_by(|a, b| b.1.cmp(&a.1));
    let total: u64 = columns.iter().map(|c| c.1 + c.2).sum();
    println!("strata narrow columns (KiB; all {} KiB): bytes, key filter, chunks, encodings", total >> 10);
    for (name, bytes, filter, chunks, enc) in &columns {
        let encs: Vec<String> = enc.iter().enumerate().filter(|(_, n)| **n > 0).map(|(k, n)| format!("{} {n}", ENCODINGS[k])).collect();
        println!("  {name:28} {:>8} {:>6} {:>6}  {}", bytes >> 10, filter >> 10, chunks, encs.join(", "));
    }
}

// ------------------------------------------------------------------ zexp

/// The raw bodies of every payload frame segment of a Strata file, in file order.
fn frame_bodies(bytes: &[u8]) -> R<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut at = 8usize;
    while at + 16 <= bytes.len() {
        let h = &bytes[at..at + 16];
        if u16::from_le_bytes([h[0], h[1]]) != 0x5347 {
            break;
        }
        let (kind, codec) = (h[2], h[3]);
        let raw = u32::from_le_bytes(h[4..8].try_into()?) as usize;
        let stored = u32::from_le_bytes(h[8..12].try_into()?) as usize;
        let body = &bytes[at + 16..at + 16 + stored];
        if kind == 2 {
            out.push(if codec == 1 { zstd::bulk::decompress(body, raw)? } else { body.to_vec() });
        }
        at += 16 + stored;
    }
    Ok(out)
}

/// Compression experiment on the payload frames of Strata files: size and speed per zstd
/// level, with and without a dictionary trained on a sample of the frames, and with frames
/// merged `m` at a time.
fn zexp_cmd(files: &[String]) -> R<()> {
    let mut frames = Vec::new();
    for f in files {
        frames.extend(frame_bodies(&std::fs::read(f)?)?);
    }
    let raw: usize = frames.iter().map(Vec::len).sum();
    println!("{} frames, {} KiB raw", frames.len(), raw >> 10);
    use zstd::zstd_safe::{CParameter, DictAttachPref};
    let budget = 256usize << 10;
    let step = (frames.len() * 4096 / budget).max(1);
    let samples: Vec<&[u8]> = frames.iter().step_by(step).map(|f| &f[..f.len().min(4096)]).collect();
    let dict = zstd::dict::from_samples(&samples, 64 << 10)?;
    let variants: Vec<(&str, i32, usize, Vec<CParameter>)> = vec![
        ("L3", 3, 1, vec![]),
        ("L3 hash15", 3, 1, vec![CParameter::HashLog(15), CParameter::ChainLog(15)]),
        ("L3 hash14", 3, 1, vec![CParameter::HashLog(14), CParameter::ChainLog(14)]),
        ("L3 min6", 3, 1, vec![CParameter::MinMatch(6)]),
        ("L3 128K", 3, 2, vec![]),
        ("L2", 2, 1, vec![]),
        ("L1", 1, 1, vec![]),
    ];
    for (name, level, merge, params) in variants {
        let groups: Vec<Vec<u8>> = frames.chunks(merge).map(|g| g.concat()).collect();
        let enc = zstd::dict::EncoderDictionary::copy(&dict, level);
        let mut c = zstd::bulk::Compressor::with_prepared_dictionary(&enc)?;
        c.set_parameter(CParameter::ForceAttachDict(DictAttachPref::ForceAttach))?;
        for p in params {
            c.set_parameter(p)?;
        }
        let mut best = f64::INFINITY;
        let mut total = 0usize;
        for _ in 0..3 {
            let t = Instant::now();
            total = 0;
            for g in &groups {
                total += c.compress(g)?.len();
            }
            best = best.min(t.elapsed().as_secs_f64());
        }
        println!("{name:12} {:>8} KiB  x{:.2}  {:>6.0} MB/s", (total + dict.len()) >> 10, raw as f64 / total as f64, raw as f64 / 1e6 / best);
    }
    Ok(())
}

/// Index experiment: per group of `g` frames, the distinct n-grams, and for each needle the
/// groups holding all its n-grams against the groups holding the needle itself.
fn gexp_cmd(needles: &str, files: &[String]) -> R<()> {
    use std::collections::HashSet;
    let needles: Vec<Vec<u8>> = std::fs::read_to_string(needles)?.lines().map(profile::unhex).collect();
    let mut frames = Vec::new();
    for f in files {
        frames.extend(frame_bodies(&std::fs::read(f)?)?);
    }
    for g in [1usize, 4, 16] {
        let groups: Vec<Vec<u8>> = frames.chunks(g).map(|c| c.concat()).collect();
        for n in [3usize, 4, 5, 6] {
            let key = |w: &[u8]| -> u64 { w.iter().fold(0u64, |h, &b| (h << 8) | u64::from(b)) };
            let sets: Vec<HashSet<u64>> = groups.iter().map(|t| t.windows(n).map(key).collect()).collect();
            let distinct: usize = sets.iter().map(HashSet::len).sum();
            let (mut cand, mut real) = (0usize, 0usize);
            for nd in &needles {
                let grams: Vec<u64> = nd.windows(n).map(key).collect();
                let finder = memchr::memmem::Finder::new(nd);
                for (t, s) in groups.iter().zip(&sets) {
                    if grams.iter().all(|k| s.contains(k)) {
                        cand += 1;
                    }
                    if finder.find(t).is_some() {
                        real += 1;
                    }
                }
            }
            let raw: usize = groups.iter().map(Vec::len).sum();
            println!("group {g:>2} ({} groups)  {n}-grams: {:>10} postings ({:.3} per raw byte)  needles: {:.1} candidate groups vs {:.1} real per needle",
                groups.len(), distinct, distinct as f64 / raw as f64, cand as f64 / needles.len() as f64, real as f64 / needles.len() as f64);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ merge

/// The Parquet side of G9: reads the `pq_default` files fully, sorts the rows by (trace_id,
/// start_time) with Arrow kernels and writes one file. Returns the row count.
fn merge_pq(dir: &Path, paths: &[PathBuf]) -> R<usize> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let mut batches = Vec::new();
    for p in paths {
        let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(p)?)?.with_batch_size(65536).build()?;
        for b in reader {
            batches.push(b?);
        }
    }
    let schema = batches.first().ok_or("no pq_default files")?.schema();
    let b = concat_batches(&schema, &batches)?;
    drop(batches);
    let idx = lexsort_to_indices(
        &[
            SortColumn { values: b.column_by_name("trace_id").ok_or("no trace_id column")?.clone(), options: None },
            SortColumn { values: b.column_by_name("start_time").ok_or("no start_time column")?.clone(), options: None },
        ],
        None,
    )?;
    let b = take_record_batch(&b, &idx)?;
    let f = std::fs::File::create(dir.join("merged.parquet"))?;
    let mut w = ArrowWriter::try_new(f, b.schema(), Some(parquet_props("pq_default", &b.schema(), b.num_rows())))?;
    for off in (0..b.num_rows()).step_by(8192) {
        w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
    }
    w.close()?;
    Ok(b.num_rows())
}

/// G9: merges the 4 `st` files with `strata::merge` and the 4 `pq_default` files with Arrow
/// (`merge_pq`), each timed once for wall and process CPU time. Local only.
async fn merge_cmd(dir: &str) -> R<()> {
    if dir.starts_with("s3://") {
        return Err("merge is local-only: give a local directory".into());
    }
    let dir = std::fs::canonicalize(dir)?;
    let loc = location(&dir.to_string_lossy())?;
    println!("loadavg before: {}", profile::loadavg());
    let files = open_strata(&loc, "st").await?;
    println!("strata: {} input files; the object store's I/O runs on blocking threads, inside the timed merge", files.len());
    let (w0, c0) = (Instant::now(), ProcessTime::now());
    let (bytes, _) = strata::merge(&files, &["trace_id", "start_time"], Config::default()).await?;
    std::fs::write(dir.join("merged.strata"), &bytes)?;
    let (st_wall, st_cpu) = (w0.elapsed().as_secs_f64() * 1e3, c0.elapsed().as_secs_f64() * 1e3);
    println!("strata  merge {st_wall:>10.1} ms wall  {st_cpu:>10.1} ms cpu  {:>8} KiB out", bytes.len() >> 10);

    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.join("pq_default"))?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
    paths.sort();
    let (w0, c0) = (Instant::now(), ProcessTime::now());
    match merge_pq(&dir, &paths) {
        Ok(rows) => {
            let (pq_wall, pq_cpu) = (w0.elapsed().as_secs_f64() * 1e3, c0.elapsed().as_secs_f64() * 1e3);
            println!("parquet merge {pq_wall:>10.1} ms wall  {pq_cpu:>10.1} ms cpu  {rows} rows");
            println!("ratio strata / parquet: wall {:.2}  cpu {:.2}", st_wall / pq_wall, st_cpu / pq_cpu);
        }
        Err(e) => println!("parquet merge ERROR {}", first_line(&e)),
    }
    println!("loadavg after: {}", profile::loadavg());
    Ok(())
}

#[allow(dead_code)]
fn _unused(_: SchemaRef) {}

fn payload_cmd(dir: &Path) -> R<()> {
    use arrow::array::AsArray;
    use parquet::arrow::ProjectionMask;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let names = ["input", "output", "system_instructions", "tool_definitions"];
    let mut paths: Vec<_> = std::fs::read_dir(dir.join("pq_default"))?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
    paths.sort();
    let mut per_col: Vec<Vec<payload::Column>> = names.iter().map(|_| Vec::new()).collect();
    for p in paths {
        let b = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&p)?)?;
        let schema = b.parquet_schema();
        let idx: Vec<usize> = (0..schema.num_columns()).filter(|&i| names.contains(&schema.column(i).name())).collect();
        let mask = ProjectionMask::leaves(schema, idx);
        let reader = b.with_projection(mask).with_batch_size(65536).build()?;
        let mut cols: Vec<payload::Column> = names.iter().map(|n| payload::Column { name: n.to_string(), values: Vec::new() }).collect();
        for batch in reader {
            let batch = batch?;
            for (i, n) in names.iter().enumerate() {
                let a = batch.column_by_name(n).unwrap();
                let a = a.as_string::<i32>();
                for j in 0..a.len() {
                    cols[i].values.push(if a.is_null(j) { Vec::new() } else { a.value(j).as_bytes().to_vec() });
                }
            }
        }
        for (i, c) in cols.into_iter().enumerate() {
            per_col[i].push(c);
        }
    }
    for (i, n) in names.iter().enumerate() {
        println!("== column {n} ({} files)", per_col[i].len());
        payload::run(&per_col[i], b"zqxjneedle");
    }
    let all: Vec<payload::Column> = per_col.into_iter().flatten().collect();
    println!("== all payload columns");
    payload::run(&all, b"zqxjneedle");
    Ok(())
}

/// Real data: SWE-agent trajectories (nebius/SWE-agent-trajectories parquet).
/// Each assistant turn becomes one LLM span: input = all previous non-system
/// messages as GenAI message JSON, system_instructions = the system prompt,
/// output = the assistant message. Rows of one trajectory are adjacent (as
/// in a trace-sorted file). Column chunks ("files") hold ~`budget` MiB of raw
/// input each; the first 4 chunks are measured.
fn swe_cmd(budget_mib: usize, files: &[String]) -> R<()> {
    let paths: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    let swe = fmt_testdata::swe::payloads(&paths, budget_mib << 20, 4)?;
    let needle = swe.needle;
    println!("swe: {} trajectories, {} LLM spans, {} column chunks; needle {:?}", swe.trajectories, swe.spans, swe.chunks.len(), String::from_utf8_lossy(&needle));
    let names = ["input", "output", "system_instructions"];
    let mut per_col: Vec<Vec<payload::Column>> = vec![Vec::new(), Vec::new(), Vec::new()];
    for c in swe.chunks {
        for (i, values) in c.into_iter().enumerate() {
            per_col[i].push(payload::Column { name: names[i].into(), values });
        }
    }
    for cols in &per_col {
        println!("== column {} ({} chunks)", cols[0].name, cols.len());
        payload::run(cols, &needle);
    }
    let all: Vec<payload::Column> = per_col.into_iter().flatten().collect();
    println!("== all payload columns");
    payload::run(&all, &needle);
    println!("== all payload columns, absent needle");
    payload::run(&all, b"zqxjneedle");
    Ok(())
}
