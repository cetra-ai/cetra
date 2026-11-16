//! Where does the time of a trace lookup / trace list go?
//!
//! Phases through DataFusion (sql → logical plan, physical planning,
//! execution) with per-operator and Parquet-scan metrics, plus a direct
//! arrow-rs read (cached metadata + page-index pruning, no DataFusion) as the
//! "format only" floor. Built to resist a loaded machine: each figure is the
//! minimum wall time over N runs, process CPU time is reported next to it, and
//! the load average is printed before and after.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::{Array, AsArray, RecordBatch};
use cpu_time::ProcessTime;
use datafusion::physical_plan::{ExecutionPlan, collect};
use datafusion::prelude::SessionContext;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection, RowSelector};
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::metadata::PageIndexPolicy;

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg").unwrap_or_default().split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

#[derive(Default, Clone)]
struct Phase {
    sql: Vec<f64>,
    phys: Vec<f64>,
    exec: Vec<f64>,
    total: Vec<f64>,
    cpu: Vec<f64>,
    metrics: BTreeMap<String, f64>, // from the min-total run, ms (times) or raw counts
}

fn min(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::INFINITY, f64::min)
}
fn med(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

fn plan_metrics(plan: &Arc<dyn ExecutionPlan>, out: &mut BTreeMap<String, f64>) {
    if let Some(m) = plan.metrics() {
        for (name, scale) in [
            ("elapsed_compute", 1e-6),
            ("time_elapsed_opening", 1e-6),
            ("time_elapsed_scanning_total", 1e-6),
            ("time_elapsed_processing", 1e-6),
            ("metadata_load_time", 1e-6),
            ("page_index_eval_time", 1e-6),
            ("bloom_filter_eval_time", 1e-6),
            ("statistics_eval_time", 1e-6),
            ("row_pushdown_eval_time", 1e-6),
            ("bytes_scanned", 1.0 / 1024.0),
            ("output_rows", 1.0),
        ] {
            if let Some(v) = m.sum_by_name(name) {
                let key = if name == "elapsed_compute" || name == "output_rows" { format!("{}.{name}", plan.name()) } else { name.to_owned() };
                *out.entry(key).or_default() += v.as_usize() as f64 * scale;
            }
        }
    }
    for c in plan.children() {
        plan_metrics(c, out);
    }
}

async fn run_df(ctx: &SessionContext, sqls: &[String], runs: usize) -> R<Phase> {
    let mut p = Phase::default();
    let mut best = f64::INFINITY;
    for _ in 0..runs {
        let (mut s, mut ph, mut ex) = (0.0, 0.0, 0.0);
        let mut metrics = BTreeMap::new();
        let cpu0 = ProcessTime::now();
        let t0 = Instant::now();
        for sql in sqls {
            let t = Instant::now();
            let df = ctx.sql(sql).await?;
            s += t.elapsed().as_secs_f64();
            let t = Instant::now();
            let plan = df.create_physical_plan().await?;
            ph += t.elapsed().as_secs_f64();
            let t = Instant::now();
            let _batches: Vec<RecordBatch> = collect(plan.clone(), ctx.task_ctx()).await?;
            ex += t.elapsed().as_secs_f64();
            plan_metrics(&plan, &mut metrics);
        }
        let total = t0.elapsed().as_secs_f64();
        let cpu = cpu0.elapsed().as_secs_f64();
        let n = sqls.len() as f64;
        p.sql.push(s / n * 1e3);
        p.phys.push(ph / n * 1e3);
        p.exec.push(ex / n * 1e3);
        p.total.push(total / n * 1e3);
        p.cpu.push(cpu / n * 1e3);
        if total < best {
            best = total;
            p.metrics = metrics.into_iter().map(|(k, v)| (k, v / n)).collect();
        }
    }
    Ok(p)
}

fn print_phase(label: &str, p: &Phase) {
    println!(
        "  {label:34} total min {:>7.3} ms (med {:>7.3})  cpu med {:>7.3} ms | sql+logical {:>6.3}  physical {:>6.3}  execute {:>7.3}",
        min(&p.total), med(&p.total), med(&p.cpu), min(&p.sql), min(&p.phys), min(&p.exec)
    );
    let mut line = String::new();
    for (k, v) in &p.metrics {
        if *v >= 0.001 {
            line.push_str(&format!("{k}={v:.3} "));
        }
    }
    println!("      metrics/query (ms, KiB, rows): {line}");
}

/// Direct arrow-rs: cached metadata with page index; prune row groups by
/// stats and pages by the trace_id column index; read the projection with a
/// RowSelection; keep rows equal to the key.
struct Direct {
    files: Vec<(std::path::PathBuf, ArrowReaderMetadata, usize)>, // path, metadata, trace_id leaf
}

impl Direct {
    fn open(dir: &Path) -> R<Self> {
        let mut files = Vec::new();
        let mut paths: Vec<_> = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<Result<_, _>>()?;
        paths.sort();
        for p in paths {
            let f = std::fs::File::open(&p)?;
            let md = ArrowReaderMetadata::load(&f, ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required))?;
            let leaf = (0..md.metadata().file_metadata().schema_descr().num_columns())
                .find(|&i| md.metadata().file_metadata().schema_descr().column(i).name() == "trace_id")
                .ok_or("no trace_id")?;
            files.push((p, md, leaf));
        }
        Ok(Self { files })
    }

    fn lookup(&self, key: &[u8], columns: Option<&[&str]>) -> R<usize> {
        let mut rows = 0;
        for (path, md, leaf) in &self.files {
            let meta = md.metadata();
            let ci = meta.column_index().ok_or("no column index")?;
            let oi = meta.offset_index().ok_or("no offset index")?;
            let mut rgs = Vec::new();
            let mut selectors = Vec::new();
            for (rg_i, rg) in meta.row_groups().iter().enumerate() {
                let n = rg.num_rows() as usize;
                let idx = &ci[rg_i][*leaf];
                let ColumnIndexMetaData::FIXED_LEN_BYTE_ARRAY(ix) = idx else { return Err("trace_id index type".into()) };
                let locs = oi[rg_i][*leaf].page_locations();
                let mut any = false;
                let mut sel = Vec::new();
                for p in 0..locs.len() {
                    let start = locs[p].first_row_index as usize;
                    let end = locs.get(p + 1).map_or(n, |l| l.first_row_index as usize);
                    let hit = matches!((ix.min_value(p), ix.max_value(p)), (Some(lo), Some(hi)) if lo <= key && key <= hi);
                    sel.push(if hit { RowSelector::select(end - start) } else { RowSelector::skip(end - start) });
                    any |= hit;
                }
                if any {
                    rgs.push(rg_i);
                    selectors.extend(sel);
                }
            }
            if rgs.is_empty() {
                continue;
            }
            let schema = meta.file_metadata().schema_descr();
            let mask = match columns {
                Some(cols) => ProjectionMask::roots(schema, (0..md.schema().fields().len()).filter(|&i| cols.contains(&md.schema().field(i).name().as_str()) || md.schema().field(i).name() == "trace_id")),
                None => ProjectionMask::all(),
            };
            let reader = ParquetRecordBatchReaderBuilder::new_with_metadata(std::fs::File::open(path)?, md.clone())
                .with_row_groups(rgs)
                .with_row_selection(RowSelection::from(selectors))
                .with_projection(mask)
                .with_batch_size(8192)
                .build()?;
            for b in reader {
                let b = b?;
                let t = b.column_by_name("trace_id").ok_or("trace_id")?.as_fixed_size_binary();
                rows += (0..t.len()).filter(|&i| t.value(i) == key).count();
            }
        }
        Ok(rows)
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0)).collect()
}

pub async fn profile(dir: &Path, runs: usize, register: impl AsyncFn(&SessionContext, &str) -> R<()>) -> R<()> {
    let ids: Vec<String> = std::fs::read_to_string(dir.join("trace_ids.txt"))?.lines().take(20).map(str::to_owned).collect();
    let light = "span_id, parent_span_id, name, start_time, end_time, status, model, total_tokens, cost_total";
    let t0 = crate::datagen::DAY_START;
    let h = 3_600 * crate::datagen::NS;
    let list = format!(
        "SELECT trace_id, span_id, name, start_time, duration_ns, status, model, total_tokens, cost_total, input_preview FROM t WHERE span_kind = 'llm' AND start_time >= to_timestamp_nanos({}) AND start_time < to_timestamp_nanos({}) ORDER BY start_time DESC LIMIT 50",
        t0 + 14 * h,
        t0 + 15 * h
    );
    println!("loadavg before: {}   runs per figure: {runs}", loadavg());
    let variants: Vec<String> = std::env::var("FMT_VARIANTS").map(|v| v.split(',').map(str::to_owned).collect()).unwrap_or_else(|_| vec!["pq_cetra".into(), "pq_tuned".into(), "pq_default".into(), "vx_compact".into()]);
    for variant in variants.iter().map(String::as_str) {
        if !dir.join(variant).exists() {
            continue;
        }
        println!("== {variant}");
        for partitions in [32usize, 1] {
            let cfg = datafusion::prelude::SessionConfig::new()
                .with_target_partitions(partitions)
                .set_bool("datafusion.execution.parquet.pushdown_filters", true)
                .set_bool("datafusion.execution.parquet.reorder_filters", true);
            let ctx = SessionContext::new_with_config(cfg);
            register(&ctx, variant).await?;
            // Warm: metadata caches, page cache.
            for id in &ids {
                ctx.sql(&format!("SELECT * FROM t WHERE trace_id = X'{id}'")).await?.collect().await?;
            }
            let full: Vec<String> = ids.iter().map(|id| format!("SELECT * FROM t WHERE trace_id = X'{id}'")).collect();
            let lightq: Vec<String> = ids.iter().map(|id| format!("SELECT {light} FROM t WHERE trace_id = X'{id}'")).collect();
            let trivial = vec!["SELECT 1".to_owned()];
            println!(" target_partitions={partitions}");
            print_phase("trace lookup, all columns", &run_df(&ctx, &full, runs).await?);
            print_phase("trace lookup, 9 light columns", &run_df(&ctx, &lightq, runs).await?);
            print_phase("trace list (top 50 in 1 h)", &run_df(&ctx, &[list.clone()], runs).await?);
            print_phase("SELECT 1 (engine floor)", &run_df(&ctx, &trivial, runs).await?);
        }
        // The direct arrow-rs floor is disabled: the synchronous reader fetches
        // whole column chunks, so it is not a valid lower bound (see plan).
        if variant.starts_with("pq") && std::env::var_os("FMT_DIRECT").is_some() {
            let d = Direct::open(&dir.join(variant))?;
            let keys: Vec<Vec<u8>> = ids.iter().map(|s| unhex(s)).collect();
            let cols: Vec<&str> = light.split(", ").collect();
            for (label, proj) in [("direct arrow-rs, all columns", None), ("direct arrow-rs, 9 light columns", Some(cols.as_slice()))] {
                let mut walls = Vec::new();
                let mut cpus = Vec::new();
                let mut rows = 0;
                for _ in 0..runs {
                    let c = ProcessTime::now();
                    let t = Instant::now();
                    rows = 0;
                    for k in &keys {
                        rows += d.lookup(k, proj)?;
                    }
                    walls.push(t.elapsed().as_secs_f64() * 1e3 / keys.len() as f64);
                    cpus.push(c.elapsed().as_secs_f64() * 1e3 / keys.len() as f64);
                }
                println!("  {label:34} total min {:>7.3} ms (med {:>7.3})  cpu med {:>7.3} ms | rows/20 lookups {rows}", min(&walls), med(&walls), med(&cpus));
            }
        }
    }
    println!("loadavg after: {}", loadavg());
    let _ = Duration::ZERO;
    Ok(())
}
