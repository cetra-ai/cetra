//! Parquet vs Vortex on LLM-shaped spans, through DataFusion.
//!
//! fmtbench gen <dir> <traces> <files>
//! fmtbench query <dir|s3://bucket/key> [runs]
//! fmtbench profile <dir|s3://bucket/key> [runs]

mod counting;
mod payload;
mod pqfast;
mod profile;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::compute::{SortColumn, cast, concat_batches, lexsort_to_indices, take_record_batch};
use arrow::datatypes::{DataType, Field, Fields, Schema, SchemaRef};
use cpu_time::ProcessTime;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
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
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("gen") => {
            let dir = PathBuf::from(&args[2]);
            let traces: usize = args[3].parse()?;
            let files: usize = args[4].parse()?;
            let swe = match args.get(5).map(String::as_str) {
                None => None,
                Some("--swe") => {
                    let max: usize = args.get(6).ok_or(USAGE)?.parse()?;
                    let shards = args.get(7..).unwrap_or_default();
                    if shards.is_empty() {
                        return Err(USAGE.into());
                    }
                    Some((max, shards))
                }
                Some(_) => return Err(USAGE.into()),
            };
            generate(&dir, traces, files, swe).await
        }
        Some("query") => {
            let runs: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(5);
            let only = args.get(4).cloned();
            let guard = match std::env::var("FMT_PPROF") {
                Ok(_) => Some(pprof::ProfilerGuardBuilder::default().frequency(997).build()?),
                Err(_) => None,
            };
            let r = query(&args[2], runs, only).await;
            if let Some(g) = guard {
                print_profile(&g.report().build()?);
            }
            r
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
        Some("dedup") => dedup_cmd(Path::new(args.get(2).ok_or(USAGE)?)),
        Some("restrata") => restrata_cmd(Path::new(args.get(2).ok_or(USAGE)?), args.get(3).map_or("st", String::as_str)),
        Some("swe") => {
            let budget: usize = args[2].parse()?;
            swe_cmd(budget, &args[3..])
        }
        Some("payload") => {
            let dir = PathBuf::from(&args[2]);
            payload_cmd(&dir)
        }
        _ => Err(USAGE.into()),
    }
}

const USAGE: &str = "usage: fmtbench gen <dir> <traces> <files> [--swe <max_trajectories> <shard>...] | query <dir|s3://bucket/key> [runs] [only] | profile <dir|s3://bucket/key> [runs] | merge <dir> (local only) | dedup <dir> (local only) | restrata <dir> [out] (local only)\n  s3 needs AWS_ENDPOINT, AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_DEFAULT_REGION and AWS_ALLOW_HTTP=true";

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

/// `swe` = (maximum trajectories, shard paths): spans come from the SWE-agent
/// shards instead of synthetic traces, and `traces` is ignored.
async fn generate(dir: &Path, traces: usize, files: usize, swe: Option<(usize, &[String])>) -> R<()> {
    let t0 = Instant::now();
    let world = datagen::World::new(7);
    let mut rng = datagen::Rng::new(42);
    let (mut spans, needles) = match swe {
        Some((max, shards)) => {
            println!("traces argument ignored: spans come from {} SWE shards", shards.len());
            let paths: Vec<PathBuf> = shards.iter().map(PathBuf::from).collect();
            let spans = fmt_testdata::swe::spans(&paths, max, &world, &mut rng)?;
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
            let path = sub.join(format!("{i:03}.parquet"));
            let f = std::fs::File::create(&path)?;
            let mut w = ArrowWriter::try_new(f, b.schema(), Some(parquet_props(variant, &b.schema(), b.num_rows())))?;
            let rg_rows = if *variant == "pq_cetra" { 100_000 } else { usize::MAX };
            for off in (0..b.num_rows()).step_by(8192) {
                w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
                if (*variant == "pq_cetra" && w.in_progress_size() >= 64 << 20) || w.in_progress_rows() >= rg_rows {
                    w.flush()?;
                }
            }
            w.close()?;
            bytes += std::fs::metadata(&path)?.len();
        }
        let el = t.elapsed();
        println!("{variant:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", bytes >> 20, el, raw as f64 / 1048576.0 / el.as_secs_f64());
    }

    for variant in VX_VARIANTS {
        let sub = dir.join(variant);
        std::fs::create_dir_all(&sub)?;
        let session = VortexSession::default().with_tokio();
        let t = Instant::now();
        let mut bytes = 0u64;
        for (i, b) in slices.iter().enumerate() {
            let b = for_vortex(b)?;
            let path = sub.join(format!("{i:03}.vortex"));
            let chunks: Vec<_> = (0..b.num_rows())
                .step_by(8192)
                .map(|off| vortex_arrow::convert::from_arrow_batch(&b.slice(off, 8192.min(b.num_rows() - off)), false))
                .collect::<Result<_, _>>()?;
            let arr = ChunkedArray::from_iter(chunks).into_array();
            let mut f = tokio::fs::File::create(&path).await?;
            let mut opts = session.write_options();
            if *variant == "vx_compact" {
                let strategy = vortex::file::WriteStrategyBuilder::from_session(&session)
                    .with_btrblocks_builder(vortex::compressor::BtrBlocksCompressorBuilder::from_session(&session).with_compact())
                    .build();
                opts = opts.with_strategy(strategy);
            }
            opts.write(&mut f, arr.to_array_stream()).await?;
            bytes += std::fs::metadata(&path)?.len();
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
        let mut w = StrataWriter::new(b.schema(), &["trace_id", "start_time"], Config::default())?;
        for off in (0..b.num_rows()).step_by(8192) {
            w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
        }
        let (file, summary) = w.finish()?;
        std::fs::write(st_dir.join(format!("{i:03}.strata")), &file)?;
        bytes += file.len() as u64;
        summaries.push(summary);
    }
    let el = t.elapsed();
    println!("{:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", "st", bytes >> 20, el, raw as f64 / 1048576.0 / el.as_secs_f64());

    payload_table(dir, &summaries)?;
    Ok(())
}

/// Payload columns: Strata bytes and frame counts beside the Parquet column bytes.
fn payload_table(dir: &Path, summaries: &[strata::FileSummary]) -> R<()> {
    let names = ["input", "output", "system_instructions", "tool_definitions"];
    let mut pq = Vec::new();
    for v in PQ_VARIANTS {
        pq.push((*v, column_sizes_parquet(&dir.join(v), v)?));
    }
    println!("payload columns (KiB; st frames: total, without filter, percent with filter):");
    for n in names {
        let (mut st_bytes, mut frames, mut no_filter, mut has_payload) = (0u64, 0u64, 0u64, false);
        let (mut filter_bytes, mut edge_bytes, mut unique_chunks, mut chunk_refs) = (0u64, 0u64, 0u64, 0u64);
        let (mut frame_bytes, mut ref_bytes) = (0u64, 0u64);
        for s in summaries {
            if let Some(c) = s.columns.iter().find(|c| c.name == n) {
                st_bytes += c.bytes;
                if let Some(p) = &c.payload {
                    has_payload = true;
                    frames += u64::from(p.frames);
                    no_filter += u64::from(p.frames_without_filter);
                    filter_bytes += p.filter_bytes;
                    frame_bytes += p.frame_bytes;
                    ref_bytes += p.ref_bytes;
                    edge_bytes += p.edge_bytes;
                    unique_chunks += u64::from(p.unique_chunks);
                    chunk_refs += p.chunk_refs;
                }
            }
        }
        let (fr, nf, pct) = if has_payload {
            let pct = 100.0 * (frames - no_filter) as f64 / frames.max(1) as f64;
            (frames.to_string(), no_filter.to_string(), format!("{pct:.1}"))
        } else {
            ("-".into(), "-".into(), "-".into())
        };
        let mut line = format!("  {n:22} st {:>9}  frames {fr:>8}  without filter {nf:>8}  filtered {pct:>5}%", st_bytes >> 10);
        for (v, sizes) in &pq {
            let b = sizes.iter().find(|(c, _)| c == n).map_or(0, |(_, b)| *b);
            line.push_str(&format!("  {v} {:>9}", b >> 10));
        }
        println!("{line}");
        if has_payload {
            println!(
                "  {:22}    frames {:>9} KiB  refs {:>9} KiB  filters {:>9} KiB  edges {:>9} KiB  unique chunks {unique_chunks:>9}  chunk refs {chunk_refs:>10}",
                "",
                frame_bytes >> 10,
                ref_bytes >> 10,
                filter_bytes >> 10,
                edge_bytes >> 10
            );
        }
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
        for (n, (c, u)) in v.iter().take(20) {
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

/// `dir` is a local directory, or `s3://bucket/key` (AWS settings: see USAGE).
fn location(dir: &str) -> R<Location> {
    if let Some(rest) = dir.strip_prefix("s3://") {
        let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
        let inner: Arc<dyn ObjectStore> = Arc::new(
            AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .with_virtual_hosted_style_request(false)
                .build()?,
        );
        return Ok(Location {
            store: Arc::new(counting::Counting::new(inner)),
            url: url::Url::parse(&format!("s3://{bucket}/"))?,
            prefix: ObjPath::parse(key.trim_matches('/'))?,
        });
    }
    let abs = std::fs::canonicalize(dir)?;
    let inner: Arc<dyn ObjectStore> = Arc::new(LocalFileSystem::new());
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
    let ctx = SessionContext::new_with_config(cfg);
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

/// The `.strata` files of a variant, sorted by path, opened concurrently.
async fn open_strata(loc: &Location, variant: &str) -> R<Vec<Arc<StrataFile>>> {
    let mut metas: Vec<ObjectMeta> = loc.store.list(Some(&loc.prefix.clone().join(variant))).try_collect().await?;
    metas.retain(|m| m.location.to_string().ends_with(".strata"));
    metas.sort_by(|a, b| a.location.cmp(&b.location));
    let cfg = Config::default();
    let store: Arc<dyn ObjectStore> = loc.store.clone();
    let files = try_join_all(metas.into_iter().map(|m| StrataFile::open(store.clone(), m.location, None, &cfg))).await?;
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
    let list_cols = LIST_COLS.join(", ");
    let queries = vec![
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
    ];
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
    println!("trace_lookup_cold: {} ids, {runs} passes; reqs exclude HEAD and LIST; waves = open + lookup (st, st_fast); local disk: OS page cache warm", ids.len());
    println!("{:16} {:>10} {:>10} {:>10} {:>8} {:>8}", "variant", "mean ms", "KiB", "reqs", "waves", "rows");
    let mut plan: Vec<(String, String)> = PQ_VARIANTS.iter().chain(VX_VARIANTS).map(|v| (v.to_string(), v.to_string())).collect();
    plan.push(("st".into(), "st".into()));
    plan.push(("st_fast".into(), "st".into()));
    plan.extend(PQ_VARIANTS.iter().map(|v| (format!("pq_fast/{v}"), v.to_string())));
    for (label, dir) in &plan {
        if !has_files(loc, dir).await? {
            continue;
        }
        let (mut ms, mut kib, mut reqs, mut waves, mut rows) = (0.0, 0.0, 0.0, None::<u64>, 0usize);
        let mut failed = None;
        'passes: for _ in 0..runs {
            for id in ids {
                loc.store.reset();
                let t = Instant::now();
                match cold_one(loc, label, id).await {
                    Ok((r, w)) => {
                        ms += t.elapsed().as_secs_f64() * 1e3;
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
}

// ------------------------------------------------------------------ restrata

/// Rewrites the Strata files of a dataset from its `pq_default` files, which hold the same
/// slices, into `<dir>/<out>`. Used to measure a format change without regenerating the
/// other variants.
fn restrata_cmd(dir: &Path, out: &str) -> R<()> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.join("pq_default"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.sort();
    let out_dir = dir.join(out);
    std::fs::create_dir_all(&out_dir)?;
    let (mut raw, mut bytes, mut write_time) = (0u64, 0u64, std::time::Duration::ZERO);
    let mut summaries = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let reader = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(p)?)?.with_batch_size(65536).build()?;
        let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>()?;
        let schema = batches.first().ok_or("empty parquet file")?.schema();
        let b = concat_batches(&schema, &batches)?;
        raw += b.get_array_memory_size() as u64;
        let t = Instant::now();
        let mut w = StrataWriter::new(b.schema(), &["trace_id", "start_time"], Config::default())?;
        for off in (0..b.num_rows()).step_by(8192) {
            w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
        }
        let (file, summary) = w.finish()?;
        write_time += t.elapsed();
        std::fs::write(out_dir.join(format!("{i:03}.strata")), &file)?;
        bytes += file.len() as u64;
        summaries.push(summary);
    }
    println!("{out:12} {:>6} MiB  write {:>8.2?}  ({:.0} MiB/s of arrow)", bytes >> 20, write_time, raw as f64 / 1048576.0 / write_time.as_secs_f64());
    payload_table(dir, &summaries)?;
    Ok(())
}

// ------------------------------------------------------------------ dedup

/// Survey experiment E2: per `pq_default` file, the payload bytes with one chunk store per
/// column (as Strata writes them) against one store shared by all payload columns.
fn dedup_cmd(dir: &Path) -> R<()> {
    use arrow::array::AsArray;
    use parquet::arrow::ProjectionMask;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    const NAMES: [&str; 4] = ["input", "output", "system_instructions", "tool_definitions"];
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir.join("pq_default"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    paths.sort();
    let mut totals: Vec<(u64, u64, u64, u64, u64)> = vec![(0, 0, 0, 0, 0); NAMES.len()];
    let (mut own_total, mut shared_total, mut pq_total) = (0u64, 0u64, 0u64);
    for p in &paths {
        let b = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(p)?)?;
        let pq_sizes: Vec<u64> = NAMES
            .iter()
            .map(|n| {
                b.metadata()
                    .row_groups()
                    .iter()
                    .flat_map(|rg| rg.columns())
                    .filter(|c| c.column_path().parts().first().map(String::as_str) == Some(*n))
                    .map(|c| c.compressed_size() as u64)
                    .sum()
            })
            .collect();
        let mask = ProjectionMask::columns(b.parquet_schema(), NAMES);
        let reader = b.with_projection(mask).with_batch_size(4096).build()?;
        let mut scope = strata::DedupScope::new(&NAMES)?;
        for batch in reader {
            let batch = batch?;
            let cols: Vec<ArrayRef> = NAMES
                .iter()
                .map(|n| -> R<ArrayRef> { Ok(cast(batch.column_by_name(n).ok_or("missing payload column")?, &DataType::LargeBinary)?) })
                .collect::<R<_>>()?;
            let bins: Vec<_> = cols.iter().map(|c| c.as_binary::<i64>()).collect();
            let mut row: Vec<Option<&[u8]>> = vec![None; NAMES.len()];
            for r in 0..batch.num_rows() {
                for (c, b) in bins.iter().enumerate() {
                    row[c] = if b.is_null(r) { None } else { Some(b.value(r)) };
                }
                scope.add_row(&row)?;
            }
        }
        let rep = scope.finish()?;
        println!("== {}", p.display());
        println!("  {:22} {:>10} {:>9} {:>10} {:>10} {:>10} {:>12} {:>10}", "column (KiB)", "raw", "chunks", "own uniq", "own frames", "pq_default", "in others", "new shared");
        let mut own = 0u64;
        for (c, col) in rep.columns.iter().enumerate() {
            println!(
                "  {:22} {:>10} {:>9} {:>10} {:>10} {:>10} {:>12} {:>10}",
                col.name,
                col.raw_bytes >> 10,
                col.own_chunks,
                col.own_unique_bytes >> 10,
                col.own_frame_bytes >> 10,
                pq_sizes[c] >> 10,
                col.shared_with_others_bytes >> 10,
                col.new_in_shared_bytes >> 10
            );
            own += col.own_frame_bytes;
            let t = &mut totals[c];
            t.0 += col.own_unique_bytes;
            t.1 += col.own_frame_bytes;
            t.2 += pq_sizes[c];
            t.3 += col.shared_with_others_bytes;
            t.4 += col.new_in_shared_bytes;
        }
        let pq: u64 = pq_sizes.iter().sum();
        println!(
            "  frames: own stores {} KiB, shared store {} KiB ({} chunks, {} KiB unique); pq_default {} KiB",
            own >> 10,
            rep.shared_frame_bytes >> 10,
            rep.shared_chunks,
            rep.shared_unique_bytes >> 10,
            pq >> 10
        );
        own_total += own;
        shared_total += rep.shared_frame_bytes;
        pq_total += pq;
    }
    println!("== total over {} files", paths.len());
    for (n, t) in NAMES.iter().zip(&totals) {
        println!(
            "  {n:22} own uniq {:>9} KiB  own frames {:>9} KiB  pq_default {:>9} KiB  in others {:>9} KiB  new shared {:>9} KiB",
            t.0 >> 10,
            t.1 >> 10,
            t.2 >> 10,
            t.3 >> 10,
            t.4 >> 10
        );
    }
    let ratio = |x: u64| x as f64 / pq_total.max(1) as f64;
    println!(
        "  frames: own {} KiB ({:.2}x pq_default), shared {} KiB ({:.2}x); pq_default {} KiB",
        own_total >> 10,
        ratio(own_total),
        shared_total >> 10,
        ratio(shared_total),
        pq_total >> 10
    );
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
