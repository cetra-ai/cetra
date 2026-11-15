//! Where does the time of a trace lookup / trace list go?
//!
//! Phases through DataFusion (sql → logical plan, physical planning,
//! execution) with per-operator and Parquet-scan metrics. Built to resist a
//! loaded machine: each figure is the
//! minimum wall time over N runs, process CPU time is reported next to it, and
//! the load average is printed before and after.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use cpu_time::ProcessTime;
use datafusion::physical_plan::{ExecutionPlan, collect};
use datafusion::prelude::SessionContext;

use crate::{Location, has_files, new_ctx, read_text};

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
    if line.is_empty() {
        line.push('-');
    }
    println!("      metrics/query (ms, KiB, rows): {line}");
}

/// Unused until the M7.2b runners convert trace ids.
#[allow(dead_code)]
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap_or(0)).collect()
}

pub async fn profile(loc: &Location, runs: usize, register: impl AsyncFn(&SessionContext, &str) -> R<()>) -> R<()> {
    let ids: Vec<String> = read_text(loc, "trace_ids.txt").await?.ok_or("no trace_ids.txt")?.lines().take(20).map(str::to_owned).collect();
    let light = "span_id, parent_span_id, name, start_time, end_time, status, model, total_tokens, cost_total";
    let t0 = fmt_testdata::datagen::DAY_START;
    let h = 3_600 * fmt_testdata::datagen::NS;
    let list = format!(
        "SELECT trace_id, span_id, name, start_time, duration_ns, status, model, total_tokens, cost_total, input_preview FROM t WHERE span_kind = 'llm' AND start_time >= to_timestamp_nanos({}) AND start_time < to_timestamp_nanos({}) ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50",
        t0 + 14 * h,
        t0 + 15 * h
    );
    println!("loadavg before: {}   runs per figure: {runs}", loadavg());
    let variants: Vec<String> = std::env::var("FMT_VARIANTS").map(|v| v.split(',').map(str::to_owned).collect()).unwrap_or_else(|_| vec!["pq_llmobs".into(), "pq_tuned".into(), "pq_default".into(), "vx_compact".into(), "st".into()]);
    for variant in variants.iter().map(String::as_str) {
        if !has_files(loc, variant).await? {
            continue;
        }
        println!("== {variant}");
        for partitions in [32usize, 1] {
            let ctx = new_ctx(loc, Some(partitions))?;
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
    }
    println!("loadavg after: {}", loadavg());
    let _ = Duration::ZERO;
    Ok(())
}
