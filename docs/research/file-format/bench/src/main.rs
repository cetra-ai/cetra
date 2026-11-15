//! Parquet vs Vortex on LLM-shaped spans, through DataFusion.
//!
//! fmtbench gen <dir> <traces> <files>
//! fmtbench query <dir> [runs]

mod counting;
mod payload;
mod profile;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::compute::{SortColumn, cast, concat_batches, lexsort_to_indices, take_record_batch};
use arrow::datatypes::{DataType, Field, Fields, Schema, SchemaRef};
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::prelude::{SessionConfig, SessionContext};
use fmt_testdata::datagen;
use llmobs_core::batch::spans_to_record_batch;
use llmobs_core::enums::SpanKind;
use llmobs_core::span::Span;
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
            let dir = PathBuf::from(&args[2]);
            let runs: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(5);
            let only = args.get(4).cloned();
            query(&dir, runs, only).await
        }
        Some("profile") => {
            let dir = std::fs::canonicalize(&args[2])?;
            let runs: usize = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(30);
            let d2 = dir.clone();
            profile::profile(&dir, runs, async move |ctx: &SessionContext, variant: &str| {
                table(ctx, "t", &format!("file://{}/{variant}/", d2.display()), variant.starts_with("vx")).await
            })
            .await
        }
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

const USAGE: &str = "usage: fmtbench gen <dir> <traces> <files> [--swe <max_trajectories> <shard>...] | query <dir> [runs] [only]";

// ------------------------------------------------------------------ generate

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
            let mut spans = Vec::new();
            for _ in 0..traces {
                world.trace(&mut rng, &mut spans);
            }
            (spans, Vec::new())
        }
    };
    if std::env::var_os("FMT_LIGHT").is_some() {
        for s in &mut spans {
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
            let rg_rows = if *variant == "pq_llmobs" { 100_000 } else { usize::MAX };
            for off in (0..b.num_rows()).step_by(8192) {
                w.write(&b.slice(off, 8192.min(b.num_rows() - off)))?;
                if (*variant == "pq_llmobs" && w.in_progress_size() >= 64 << 20) || w.in_progress_rows() >= rg_rows {
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

    column_sizes_parquet(&dir.join("pq_tuned"))?;
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

const PQ_VARIANTS: &[&str] = &["pq_default", "pq_tuned", "pq_llmobs"];
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

fn column_sizes_parquet(dir: &Path) -> R<()> {
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
    println!("parquet column sizes (compressed / uncompressed KiB):");
    for (n, (c, u)) in v.iter().take(20) {
        println!("  {n:24} {:>10} {:>10}  x{:.1}", c >> 10, u >> 10, *u as f64 / (*c).max(1) as f64);
    }
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ------------------------------------------------------------------ query

async fn table(ctx: &SessionContext, name: &str, url: &str, vortex: bool) -> R<()> {
    let url = ListingTableUrl::parse(url)?;
    let opts = if vortex {
        let s = VortexSession::default().with_tokio();
        ListingOptions::new(Arc::new(vortex_datafusion::VortexFormat::new(s))).with_file_extension(".vortex")
    } else {
        ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet")
    };
    let cfg = ListingTableConfig::new(url).with_listing_options(opts).infer_schema(&ctx.state()).await?;
    ctx.register_table(name, Arc::new(ListingTable::try_new(cfg)?))?;
    Ok(())
}

async fn query(dir: &Path, runs: usize, only: Option<String>) -> R<()> {
    let dir = std::fs::canonicalize(dir)?;
    let ids: Vec<String> = std::fs::read_to_string(dir.join("trace_ids.txt"))?.lines().map(str::to_owned).collect();
    let t0 = datagen::DAY_START;
    let h = 3_600 * datagen::NS;
    let ts = |ns: i64| format!("to_timestamp_nanos({ns})");
    let queries: Vec<(&str, String)> = vec![
        ("count_errors_6h", format!(
            "SELECT count(*) FROM t WHERE status = 'error' AND start_time >= {} AND start_time < {}", ts(t0 + 10 * h), ts(t0 + 16 * h))),
        ("model_rollup_day", "SELECT model, count(*), sum(total_tokens), sum(cost_total), avg(duration_ns) FROM t WHERE span_kind = 'llm' GROUP BY model ORDER BY model".to_owned()),
        ("list_llm_1h", format!(
            "SELECT trace_id, span_id, name, start_time, duration_ns, status, model, total_tokens, cost_total, input_preview FROM t WHERE span_kind = 'llm' AND start_time >= {} AND start_time < {} ORDER BY start_time DESC LIMIT 50", ts(t0 + 14 * h), ts(t0 + 15 * h))),
        ("user_filter", "SELECT count(*), sum(cost_total) FROM t WHERE user_id = 'user-1234'".to_owned()),
        ("metadata_filter", "SELECT count(*) FROM t WHERE metadata['customer_id'] = '\"cust-42\"'".to_owned()),
        ("trace_lookup_full", "TRACE".to_owned()),
        ("trace_lookup_light", "TRACE_LIGHT".to_owned()),
        ("substring_input", "SELECT count(*) FROM t WHERE input LIKE '%zqxjneedle%'".to_owned()),
        ("scan_all_payload", "SELECT sum(length(input)) + sum(length(output)) FROM t".to_owned()),
    ];
    for format in PQ_VARIANTS.iter().chain(VX_VARIANTS) {
        let vortex = format.starts_with("vx");
        let store = Arc::new(counting::Counting::new());
        let cfg = SessionConfig::new()
            .set_bool("datafusion.execution.parquet.pushdown_filters", true)
            .set_bool("datafusion.execution.parquet.reorder_filters", true);
        let ctx = SessionContext::new_with_config(cfg);
        ctx.register_object_store(&url::Url::parse("file://")?, store.clone());
        table(&ctx, "t", &format!("file://{}/{format}/", dir.display()), vortex).await?;
        for (name, sql) in &queries {
            if only.as_deref().is_some_and(|o| o != *name) {
                continue;
            }
            let mut times = Vec::new();
            let mut first_io = (0, 0);
            let mut rows = 0;
            let mut value = String::new();
            for r in 0..runs {
                let sqls: Vec<String> = match sql.as_str() {
                    "TRACE" => ids.iter().take(20).map(|id| format!("SELECT * FROM t WHERE trace_id = X'{id}'")).collect(),
                    "TRACE_LIGHT" => ids.iter().take(20).map(|id| format!("SELECT span_id, parent_span_id, name, start_time, end_time, status, model, total_tokens, cost_total FROM t WHERE trace_id = X'{id}'")).collect(),
                    s => vec![s.to_owned()],
                };
                store.reset();
                let t = Instant::now();
                rows = 0;
                let mut failed = None;
                for s in &sqls {
                    match async { ctx.sql(s).await?.collect().await }.await {
                        Ok(batches) => {
                            rows += batches.iter().map(RecordBatch::num_rows).sum::<usize>();
                            if sqls.len() == 1 && r == 0 {
                                value = arrow::util::pretty::pretty_format_batches(&batches)
                                    .map(|t| t.to_string().lines().nth(3).unwrap_or("").replace(' ', ""))
                                    .unwrap_or_default();
                                value.truncate(60);
                            }
                        }
                        Err(e) => {
                            failed = Some(e.to_string());
                            break;
                        }
                    }
                }
                if let Some(e) = failed {
                    println!("{format:10} {name:20} ERROR {}", e.lines().next().unwrap_or(""));
                    times.clear();
                    break;
                }
                let el = t.elapsed().as_secs_f64() * 1000.0 / sqls.len() as f64;
                times.push(el);
                if r == 0 {
                    first_io = store.stats();
                }
            }
            if times.is_empty() {
                continue;
            }
            times.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "{format:10} {name:20} median {:>9.2} ms  min {:>9.2} ms  rows {rows:>5}  io: {:>6} reqs {:>9} KiB  {value}",
                times[times.len() / 2], times[0], first_io.0, first_io.1 >> 10
            );
        }
    }
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
