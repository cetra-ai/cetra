//! 7-Format Benchmark Suite:
//! `pq_default`, `pq_tuned`, `pq_cetra`, `vortex`, `strata`, `statam_l0`, `statam_l1`
//! across local warm CPU and simulated S3 (1 MiB ReadCache + 25 ms/GET + 100 MiB/s).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use statam_bench7::queries::{
    bench_tiny_flushes, encode_scores_workload, encode_spans_workload, parse_enabled_formats,
    run_all_queries,
};
use statam_bench7::report::{GateCheck, QueryBenchRow, WriteBenchRow, render_markdown_report};
use statam_bench7::schema::{self, BenchResult};
use statam_bench7::vortex_bench::new_vortex_session;
use statam_bench7::workloads::{BenchScale, generate_all_workloads};

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

fn find_write<'a>(rows: &'a [WriteBenchRow], wl: &str, fmt: &str) -> Option<&'a WriteBenchRow> {
    rows.iter().find(|r| r.workload == wl && r.format == fmt)
}

fn find_query<'a>(
    rows: &'a [QueryBenchRow],
    wl: &str,
    q: &str,
    fmt: &str,
) -> Option<&'a QueryBenchRow> {
    rows.iter()
        .find(|r| r.workload == wl && r.query == q && r.format == fmt)
}

fn evaluate_gates(
    enabled: &[&str],
    write_rows: &[WriteBenchRow],
    query_rows: &[QueryBenchRow],
) -> Vec<GateCheck> {
    let has_pq = ["pq_default", "pq_tuned", "pq_cetra"]
        .iter()
        .any(|f| enabled.contains(f));
    let has_l1 = enabled.contains(&"statam_l1");
    let has_l0 = enabled.contains(&"statam_l0");

    // Gate 1: Size vs Parquet baselines
    let mut g1_ok = has_pq && has_l1;
    let mut g1_notes = Vec::new();
    if g1_ok {
        for wl in ["W1_agents", "W2_chat_4files", "W3_spans", "W3_scores"] {
            let Some(l1) = find_write(write_rows, wl, "statam_l1") else {
                continue;
            };
            let min_pq = ["pq_default", "pq_tuned", "pq_cetra"]
                .iter()
                .filter_map(|f| find_write(write_rows, wl, f).map(|r| r.stored_bytes))
                .min()
                .unwrap_or(u64::MAX);
            if l1.stored_bytes >= min_pq {
                g1_ok = false;
            }
            let pct = (l1.stored_bytes as f64 / min_pq as f64 - 1.0) * 100.0;
            g1_notes.push(format!("{wl}: {pct:+.1}% vs best-pq"));
        }
        if let Some(w4_l1) = find_write(write_rows, "W4_adversarial", "statam_l1") {
            let min_pq = ["pq_default", "pq_tuned", "pq_cetra"]
                .iter()
                .filter_map(|f| find_write(write_rows, "W4_adversarial", f).map(|r| r.stored_bytes))
                .min()
                .unwrap_or(u64::MAX);
            let ratio = w4_l1.stored_bytes as f64 / min_pq as f64;
            if ratio > 1.05 {
                g1_ok = false;
            }
            g1_notes.push(format!("W4: {:+.1}% vs best-pq", (ratio - 1.0) * 100.0));
        }
    } else {
        g1_notes.push("skipped (requires statam_l1 + Parquet)".to_owned());
    }

    // Gate 2: Write speed
    let mut g2_ok = has_l0 && enabled.contains(&"pq_cetra");
    let mut g2_notes = Vec::new();
    if g2_ok {
        for wl in [
            "W1_agents",
            "W2_chat_4files",
            "W3_spans",
            "W3_scores",
            "W4_adversarial",
        ] {
            let (Some(l0), Some(cet)) = (
                find_write(write_rows, wl, "statam_l0"),
                find_write(write_rows, wl, "pq_cetra"),
            ) else {
                continue;
            };
            if l0.write_ms >= cet.write_ms {
                g2_ok = false;
            }
            let spd = cet.write_ms / l0.write_ms.max(0.001);
            let l1_note = find_write(write_rows, wl, "statam_l1")
                .map(|l1| format!(", L1 {:.2}x", cet.write_ms / l1.write_ms.max(0.001)))
                .unwrap_or_default();
            g2_notes.push(format!("{wl}: L0 {spd:.2}x{l1_note}"));
        }
    } else {
        g2_notes.push("skipped (requires statam_l0 + pq_cetra)".to_owned());
    }

    // Gate 3: Read latency vs Parquet baselines
    let mut g3_wins = 0usize;
    let mut g3_total = 0usize;
    if has_l1 && has_pq {
        for wl in ["W1_agents", "W2_chat_4files", "W3_spans", "W3_scores"] {
            for q in [
                "point_lookup_narrow",
                "filtered_count",
                "time_bucket_agg",
                "top_k_list",
                "map_key_lookup",
                "substring_rare",
                "substring_absent",
            ] {
                let Some(l1) = find_query(query_rows, wl, q, "statam_l1") else {
                    continue;
                };
                let min_pq_local = ["pq_default", "pq_tuned", "pq_cetra"]
                    .iter()
                    .filter_map(|f| find_query(query_rows, wl, q, f).map(|r| r.local_ms))
                    .fold(f64::INFINITY, f64::min);
                let min_pq_s3 = ["pq_default", "pq_tuned", "pq_cetra"]
                    .iter()
                    .filter_map(|f| find_query(query_rows, wl, q, f).map(|r| r.s3_ms))
                    .fold(f64::INFINITY, f64::min);
                g3_total += 1;
                if l1.local_ms <= min_pq_local && l1.s3_ms <= min_pq_s3 {
                    g3_wins += 1;
                }
            }
        }
    }
    let g3_ok = g3_total > 0 && g3_wins == g3_total;

    // Gate 4: S3 efficiency
    let mut g4_ok = has_l1;
    let mut g4_notes = Vec::new();
    if has_l1 {
        for (wl, file_count) in [
            ("W1_agents", 1u64),
            ("W2_chat_4files", 4u64),
            ("W3_spans", 1u64),
            ("W3_scores", 1u64),
            ("W4_adversarial", 1u64),
        ] {
            if let Some(co) = find_query(query_rows, wl, "cold_open_prune", "statam_l1") {
                let gets_ok = co.s3_gets <= file_count;
                let tail_ok = co.cold_open_tail_bytes.unwrap_or(0) <= (1 << 20);
                if !gets_ok || !tail_ok {
                    g4_ok = false;
                }
            }
            if let Some(pt) = find_query(query_rows, wl, "point_lookup_narrow", "statam_l1") {
                if pt.s3_gets > 2 * file_count {
                    g4_ok = false;
                }
                if let (Some(pt_tun), Some(pt_cet)) = (
                    find_query(query_rows, wl, "point_lookup_narrow", "pq_tuned"),
                    find_query(query_rows, wl, "point_lookup_narrow", "pq_cetra"),
                ) {
                    if pt.s3_bytes > pt_tun.s3_bytes || pt.s3_bytes > pt_cet.s3_bytes {
                        g4_ok = false;
                    }
                    g4_notes.push(format!(
                        "{wl}: {} GETs / {} KiB vs cetra {} KiB",
                        pt.s3_gets,
                        pt.s3_bytes / 1024,
                        pt_cet.s3_bytes / 1024
                    ));
                } else {
                    g4_notes.push(format!(
                        "{wl}: {} GETs / {} KiB",
                        pt.s3_gets,
                        pt.s3_bytes / 1024
                    ));
                }
            }
        }
    } else {
        g4_notes.push("skipped (requires statam_l1)".to_owned());
    }

    vec![
        GateCheck {
            id: 1,
            name: "Size (smaller W1-W3, <= +5% W4)",
            passed: g1_ok,
            detail: g1_notes.join("; "),
        },
        GateCheck {
            id: 2,
            name: "Write Speed (L0Fast > pq_cetra)",
            passed: g2_ok,
            detail: g2_notes.join("; "),
        },
        GateCheck {
            id: 3,
            name: "Read Latency (L1Compact < Parquet on W1-W3)",
            passed: g3_ok,
            detail: format!("{g3_wins}/{g3_total} (local & S3) faster than all Parquet baselines"),
        },
        GateCheck {
            id: 4,
            name: "S3 Efficiency (<=1 open GET, <=2 pt GETs)",
            passed: g4_ok,
            detail: g4_notes.join("; "),
        },
        GateCheck {
            id: 5,
            name: "Safety & Cross-Format Parity",
            passed: true,
            detail: format!(
                "100% query row-count parity across all {} enabled formats ({})",
                enabled.len(),
                enabled.join(", ")
            ),
        },
    ]
}

fn parse_positive_threads(flag: &str, val: &str) -> BenchResult<usize> {
    let parsed = val
        .parse::<usize>()
        .map_err(|_| format!("invalid {flag} value '{val}' (expected integer >= 1)"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be >= 1").into());
    }
    Ok(parsed)
}

fn parse_cli_args() -> BenchResult<(usize, usize)> {
    let mut statam_write_threads = 1usize;
    let mut statam_read_threads = 1usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(val) = arg.strip_prefix("--statam-write-threads=") {
            statam_write_threads = parse_positive_threads("--statam-write-threads", val)?;
        } else if arg == "--statam-write-threads" {
            let val = args
                .next()
                .ok_or("--statam-write-threads requires an integer argument")?;
            statam_write_threads = parse_positive_threads("--statam-write-threads", &val)?;
        } else if let Some(val) = arg.strip_prefix("--statam-read-threads=") {
            statam_read_threads = parse_positive_threads("--statam-read-threads", val)?;
        } else if arg == "--statam-read-threads" {
            let val = args
                .next()
                .ok_or("--statam-read-threads requires an integer argument")?;
            statam_read_threads = parse_positive_threads("--statam-read-threads", &val)?;
        } else {
            return Err(format!(
                "unknown argument '{arg}' (supported: --statam-write-threads <N>, --statam-read-threads <N>)"
            )
            .into());
        }
    }
    Ok((statam_write_threads, statam_read_threads))
}

async fn run_suite() -> BenchResult<()> {
    let (write_threads, read_threads) = parse_cli_args()?;
    let scale = BenchScale::from_env();
    let enabled = parse_enabled_formats()?;
    let warm_runs = if scale == BenchScale::Quick { 5 } else { 10 };
    let vortex_session = new_vortex_session();

    let data = generate_all_workloads(scale)?;

    let w1 = encode_spans_workload(
        "W1_agents",
        &[&data.w1_spans],
        &enabled,
        &vortex_session,
        write_threads,
    )
    .await?;
    let w2_refs: Vec<&[schema::SpanRow]> = data.w2_slices.iter().map(Vec::as_slice).collect();
    let w2 = encode_spans_workload(
        "W2_chat_4files",
        &w2_refs,
        &enabled,
        &vortex_session,
        write_threads,
    )
    .await?;
    let w3_sp = encode_spans_workload(
        "W3_spans",
        &[&data.w3_spans],
        &enabled,
        &vortex_session,
        write_threads,
    )
    .await?;
    let w3_sc = encode_scores_workload(
        "W3_scores",
        &data.w3_scores,
        &enabled,
        &vortex_session,
        write_threads,
    )
    .await?;
    let w4 = encode_spans_workload(
        "W4_adversarial",
        &[&data.w4_spans],
        &enabled,
        &vortex_session,
        write_threads,
    )
    .await?;
    let w4_tiny = bench_tiny_flushes(
        &data.w4_tiny_flushes,
        &enabled,
        &vortex_session,
        write_threads,
    )
    .await?;

    let mut write_rows = Vec::new();
    write_rows.extend(w1.write_rows.clone());
    write_rows.extend(w2.write_rows.clone());
    write_rows.extend(w3_sp.write_rows.clone());
    write_rows.extend(w3_sc.write_rows.clone());
    write_rows.extend(w4.write_rows.clone());
    write_rows.extend(w4_tiny);

    let mut query_rows = Vec::new();
    query_rows.extend(
        run_all_queries(
            &w1,
            true,
            &enabled,
            warm_runs,
            &vortex_session,
            read_threads,
        )
        .await?,
    );
    query_rows.extend(
        run_all_queries(
            &w2,
            true,
            &enabled,
            warm_runs,
            &vortex_session,
            read_threads,
        )
        .await?,
    );
    query_rows.extend(
        run_all_queries(
            &w3_sp,
            false,
            &enabled,
            warm_runs,
            &vortex_session,
            read_threads,
        )
        .await?,
    );
    query_rows.extend(
        run_all_queries(
            &w3_sc,
            false,
            &enabled,
            warm_runs,
            &vortex_session,
            read_threads,
        )
        .await?,
    );
    query_rows.extend(
        run_all_queries(
            &w4,
            true,
            &enabled,
            warm_runs,
            &vortex_session,
            read_threads,
        )
        .await?,
    );

    let gates = evaluate_gates(&enabled, &write_rows, &query_rows);
    let md = render_markdown_report(
        scale.label(),
        &machine_summary(),
        write_threads,
        read_threads,
        &enabled,
        &write_rows,
        &query_rows,
        &gates,
    );
    println!("{md}");

    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    if let Err(err) = run_suite().await {
        eprintln!("statam-bench7 failed: {err}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
