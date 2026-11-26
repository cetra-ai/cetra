use std::fmt::Write;

#[derive(Debug, Clone)]
pub(crate) struct WriteBenchRow {
    pub(crate) workload: String,
    pub(crate) format: String,
    pub(crate) raw_bytes: u64,
    pub(crate) stored_bytes: u64,
    pub(crate) write_ms: f64,
    pub(crate) write_mib_s: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct QueryBenchRow {
    pub(crate) workload: String,
    pub(crate) query: String,
    pub(crate) format: String,
    pub(crate) rows_matched: usize,
    pub(crate) local_ms: f64,
    pub(crate) local_p95_ms: f64,
    pub(crate) s3_ms: f64,
    pub(crate) s3_gets: u64,
    pub(crate) s3_bytes: u64,
    pub(crate) cold_open_gets: Option<u64>,
    pub(crate) cold_open_tail_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct GateCheck {
    pub(crate) id: u8,
    pub(crate) name: &'static str,
    pub(crate) passed: bool,
    pub(crate) detail: String,
}

pub(crate) fn format_kib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1024.0)
}

pub(crate) fn render_markdown_report(
    scale: &str,
    machine_info: &str,
    statam_write_threads: usize,
    enabled_formats: &[&str],
    write_rows: &[WriteBenchRow],
    query_rows: &[QueryBenchRow],
    gates: &[GateCheck],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n# Statam 7-Format Benchmark Report (`scale={scale}`)"
    );
    let _ = writeln!(out, "\n- **Host:** {machine_info}");
    let _ = writeln!(out, "- **Statam Write Threads:** `{statam_write_threads}`");
    let _ = writeln!(out, "- **Formats:** `{}`", enabled_formats.join("`, `"));
    let _ = writeln!(
        out,
        "- **Simulated S3 Model:** `1 MiB` `ReadCache` block rounding + run coalescing, `25.0 ms / GET`, `100 MiB/s`"
    );

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

    let _ = writeln!(out, "\n## 2. Read Latency (Local Warm CPU & Simulated S3)");
    let _ = writeln!(
        out,
        "\n| Workload | Query | Format | Matched | Local Median (ms) | Local p95 (ms) | S3 (ms) | S3 GETs | S3 KiB | Cold-Open GETs |"
    );
    let _ = writeln!(out, "|---|---|---|---:|---:|---:|---:|---:|---:|---:|");
    for r in query_rows {
        let cold = match r.cold_open_gets {
            Some(g) => format!("{g}"),
            None => "-".to_owned(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.3} | {:.3} | {:.2} | {} | {} | {} |",
            r.workload,
            r.query,
            r.format,
            r.rows_matched,
            r.local_ms,
            r.local_p95_ms,
            r.s3_ms,
            r.s3_gets,
            format_kib(r.s3_bytes),
            cold
        );
    }

    let _ = writeln!(out, "\n## 3. Go/No-Go Scorecard & Cross-Format Parity");
    let _ = writeln!(out, "\n| Gate | Requirement | Status | Observed |");
    let _ = writeln!(out, "|---:|---|---|---|");
    for g in gates {
        let status = if g.passed { "**PASS**" } else { "**FAIL**" };
        let _ = writeln!(out, "| {} | {} | {} | {} |", g.id, g.name, status, g.detail);
    }
    out
}
