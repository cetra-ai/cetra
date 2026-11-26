//! Process-wide `TrackingAllocator` Peak Memory Benchmark for:
//! `pq_cetra`, `vortex`, `strata`, `statam_l0 (1T & 8T)`, `statam_l1 (1T & 8T)`
//! across Write (`W3_spans`, plus `W1_agents` if `STATAM_SWE_DIR` is set)
//! and Read (`full_scan`, `point_lookup`, `substring_search`).

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::alloc::{GlobalAlloc, Layout};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bytes::Bytes;
use cetra_column::Batch;
use cetra_statam::{
    ChunkPackBuilder, InMemoryPackResolver, PackResolver, PackWriteOptions, ScanRequest,
    SliceReader, StatamReader, StatamWriter, TableKind, WriteOptions, WriteProfile,
};
use futures::stream::TryStreamExt;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::file::metadata::PageIndexPolicy;
use strata::ScanKind;
use vortex::expr::{root, select};
use vortex::session::VortexSession;

use statam_bench7::queries::{
    ALL_FORMATS, PACK_ID, run_pq_query_local, run_statam_query, write_parquet_files,
    write_statam_files,
};
use statam_bench7::report::format_kib;
use statam_bench7::schema::{
    BenchResult, ParquetVariant, SpanRow, TableKindBench, arrow_spans_schema, sort_spans,
    spans_to_arrow, spans_to_cetra_batch,
};
use statam_bench7::strata_bench::{
    make_scan_req, open_strata_local, run_strata_query, write_strata_files,
};
use statam_bench7::vortex_bench::{
    chunk_to_record_batch, new_vortex_session, open_vortex_local, prepare_vortex_batches,
    run_vortex_query, write_vortex_prepared,
};
use statam_bench7::workloads::{
    BenchScale, generate_w3_narrow_spans, load_swe_trajectories, plant_rare_needle,
};

// ============================================================================
// Process-wide atomic TrackingAllocator (accurate across multi-threaded writers)
// ============================================================================

struct TrackingAllocator {
    inner: std::alloc::System,
}

#[global_allocator]
static GLOBAL: TrackingAllocator = TrackingAllocator {
    inner: std::alloc::System,
};

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn add_live(size: usize) {
    let prev = LIVE_BYTES.fetch_add(size, Ordering::Relaxed);
    let next = prev.saturating_add(size);
    PEAK_BYTES.fetch_max(next, Ordering::Relaxed);
}

#[inline]
fn sub_live(size: usize) {
    LIVE_BYTES.fetch_sub(size, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegates directly to `std::alloc::System` with identical `layout`.
        let ptr = unsafe { self.inner.alloc(layout) };
        if !ptr.is_null() {
            add_live(layout.size());
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegates directly to `std::alloc::System` with identical `layout`.
        let ptr = unsafe { self.inner.alloc_zeroed(layout) };
        if !ptr.is_null() {
            add_live(layout.size());
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        sub_live(layout.size());
        // SAFETY: `ptr` and `layout` come from a prior `alloc`/`alloc_zeroed`/`realloc` on `self.inner`.
        unsafe { self.inner.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `ptr`, `layout`, and `new_size` satisfy `GlobalAlloc::realloc` preconditions.
        let new_ptr = unsafe { self.inner.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            let old_size = layout.size();
            ALLOCATED_BYTES.fetch_add(new_size, Ordering::Relaxed);
            if new_size > old_size {
                add_live(new_size - old_size);
            } else {
                sub_live(old_size - new_size);
            }
        }
        new_ptr
    }
}

#[derive(Debug, Clone, Copy)]
struct MemSnapshot {
    peak_live_bytes: usize,
    peak_delta_bytes: usize,
    allocated_bytes: usize,
}

fn reset_peak() -> usize {
    std::sync::atomic::fence(Ordering::SeqCst);
    let live = LIVE_BYTES.load(Ordering::SeqCst);
    PEAK_BYTES.store(live, Ordering::SeqCst);
    ALLOCATED_BYTES.store(0, Ordering::SeqCst);
    live
}

fn snapshot(baseline_live: usize) -> MemSnapshot {
    std::sync::atomic::fence(Ordering::SeqCst);
    let peak_live = PEAK_BYTES.load(Ordering::SeqCst);
    let alloc = ALLOCATED_BYTES.load(Ordering::SeqCst);
    MemSnapshot {
        peak_live_bytes: peak_live,
        peak_delta_bytes: peak_live.saturating_sub(baseline_live),
        allocated_bytes: alloc,
    }
}

fn format_mib(bytes: usize) -> String {
    format!("{:.2}", bytes as f64 / (1024.0 * 1024.0))
}

struct WriteMemRow {
    workload: String,
    format: String,
    threads: usize,
    stored_bytes: u64,
    elapsed_ms: f64,
    mem: MemSnapshot,
}

struct ReadMemRow {
    workload: String,
    query: String,
    format: String,
    matched: usize,
    elapsed_ms: f64,
    mem: MemSnapshot,
}

const DEFAULT_PEAK_FORMATS: [&str; 5] = ["pq_cetra", "vortex", "strata", "statam_l0", "statam_l1"];

fn parse_cli_formats() -> BenchResult<Vec<&'static str>> {
    let mut override_csv: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(val) = arg.strip_prefix("--formats=") {
            override_csv = Some(val.to_owned());
        } else if arg == "--formats" {
            let val = args.next().ok_or("--formats requires a csv list")?;
            override_csv = Some(val);
        } else {
            return Err(format!("unknown argument '{arg}' (supported: --formats <csv>)").into());
        }
    }

    let raw = match override_csv {
        Some(v) => v,
        None => match std::env::var("BENCH_FORMATS") {
            Ok(v) if !v.trim().is_empty() => v,
            _ => return Ok(DEFAULT_PEAK_FORMATS.to_vec()),
        },
    };
    let requested: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if requested.is_empty() {
        return Ok(DEFAULT_PEAK_FORMATS.to_vec());
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

fn write_statam_with_options(
    profile: WriteProfile,
    batch: &Batch,
    use_pack: bool,
    write_threads: usize,
) -> BenchResult<(Bytes, Option<Bytes>, f64)> {
    let t0 = Instant::now();
    let mut pb = use_pack.then(|| {
        let mut pack_opts = PackWriteOptions::default();
        if write_threads > 1 {
            pack_opts.write_threads = write_threads;
        }
        ChunkPackBuilder::new(PACK_ID, pack_opts)
    });
    let mut opts = WriteOptions::for_table(TableKind::Spans, profile);
    opts.sort_cols = vec![1u16, 6u16];
    if write_threads > 1 {
        opts.write_threads = write_threads;
    }
    if pb.is_some() {
        opts.pack_id = Some(PACK_ID);
    }
    let mut buf = Vec::new();
    let mut w = StatamWriter::new(Arc::clone(batch.schema()), opts, pb.as_mut())?;
    w.write_batch(batch)?;
    w.finish(&mut buf)?;
    let pack = if let Some(builder) = pb {
        let mut pbuf = Vec::new();
        builder.finish(&mut pbuf)?;
        Some(Bytes::from(pbuf))
    } else {
        None
    };
    Ok((Bytes::from(buf), pack, t0.elapsed().as_secs_f64() * 1000.0))
}

async fn bench_workload_peak_mem(
    wl_name: &str,
    rows: &[SpanRow],
    use_pack_for_l1: bool,
    enabled: &[&str],
    vortex_session: &VortexSession,
    write_rows: &mut Vec<WriteMemRow>,
    read_rows: &mut Vec<ReadMemRow>,
    notes: &mut Vec<String>,
) -> BenchResult<()> {
    let sample_tid = rows.get(rows.len() / 2).map_or([1u8; 16], |r| r.trace_id);
    let arrow_schema = arrow_spans_schema();
    let arrow_batch = spans_to_arrow(rows)?;
    let cetra_batch = spans_to_cetra_batch(rows)?;
    let vortex_prepared = if enabled.contains(&"vortex") {
        Some(prepare_vortex_batches(
            vortex_session,
            std::slice::from_ref(&arrow_batch),
        )?)
    } else {
        None
    };

    let full_scan_cols: Vec<usize> = if use_pack_for_l1 {
        arrow_schema
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                !matches!(
                    f.name().as_str(),
                    "input" | "output" | "system_instructions" | "tool_definitions"
                )
            })
            .map(|(i, _)| i)
            .collect()
    } else {
        (0..arrow_schema.fields().len()).collect()
    };
    let full_scan_names: Vec<Arc<str>> = full_scan_cols
        .iter()
        .map(|&i| Arc::<str>::from(arrow_schema.field(i).name().as_str()))
        .collect();

    // 1. Parquet (pq_cetra)
    if enabled.contains(&"pq_cetra") {
        let base = reset_peak();
        let (files, w_ms) = write_parquet_files(
            ParquetVariant::Cetra,
            TableKindBench::Spans,
            &arrow_schema,
            std::slice::from_ref(&arrow_batch),
        )?;
        let w_mem = snapshot(base);
        write_rows.push(WriteMemRow {
            workload: wl_name.to_owned(),
            format: "pq_cetra".to_owned(),
            threads: 1,
            stored_bytes: files.iter().map(|b| b.len() as u64).sum(),
            elapsed_ms: w_ms,
            mem: w_mem,
        });

        let pq_bytes = &files[0];
        let meta = ArrowReaderMetadata::load(
            pq_bytes,
            ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Optional),
        )?;

        // full_scan
        {
            let base = reset_peak();
            let t0 = Instant::now();
            let proj = ProjectionMask::roots(meta.parquet_schema(), full_scan_cols.iter().copied());
            let reader =
                ParquetRecordBatchReaderBuilder::new_with_metadata(pq_bytes.clone(), meta.clone())
                    .with_projection(proj)
                    .with_batch_size(16_384)
                    .build()?;
            let mut matched = 0usize;
            for b in reader {
                matched += b?.num_rows();
            }
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mem = snapshot(base);
            read_rows.push(ReadMemRow {
                workload: wl_name.to_owned(),
                query: "full_scan".to_owned(),
                format: "pq_cetra".to_owned(),
                matched,
                elapsed_ms,
                mem,
            });
        }

        // point_lookup & substring_search
        for (qname, qkey) in [
            ("point_lookup", "point_lookup_full"),
            ("substring_search", "substring_rare"),
        ] {
            let base = reset_peak();
            let t0 = Instant::now();
            let matched = run_pq_query_local(
                std::slice::from_ref(pq_bytes),
                std::slice::from_ref(&meta),
                TableKindBench::Spans,
                qkey,
                sample_tid,
            )?;
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mem = snapshot(base);
            read_rows.push(ReadMemRow {
                workload: wl_name.to_owned(),
                query: qname.to_owned(),
                format: "pq_cetra".to_owned(),
                matched,
                elapsed_ms,
                mem,
            });
        }
    }

    // 2. Vortex
    if let Some(prepared) = vortex_prepared.as_ref() {
        let base = reset_peak();
        let (files, w_ms) = write_vortex_prepared(vortex_session, prepared).await?;
        let w_mem = snapshot(base);
        write_rows.push(WriteMemRow {
            workload: wl_name.to_owned(),
            format: "vortex".to_owned(),
            threads: 1,
            stored_bytes: files.iter().map(|b| b.len() as u64).sum(),
            elapsed_ms: w_ms,
            mem: w_mem,
        });

        let vx_files = open_vortex_local(vortex_session, &files).await?;

        // full_scan
        {
            let base = reset_peak();
            let t0 = Instant::now();
            let mut matched = 0usize;
            for f in &vx_files {
                let proj = select(full_scan_names.clone(), root())
                    .bind(f.dtype())?
                    .optimize_recursive()?;
                let mut stream = f.scan()?.with_projection(proj).into_array_stream()?;
                while let Some(chunk) = stream.try_next().await? {
                    if chunk.is_empty() {
                        continue;
                    }
                    let b = chunk_to_record_batch(vortex_session, chunk)?;
                    matched += b.num_rows();
                }
            }
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mem = snapshot(base);
            read_rows.push(ReadMemRow {
                workload: wl_name.to_owned(),
                query: "full_scan".to_owned(),
                format: "vortex".to_owned(),
                matched,
                elapsed_ms,
                mem,
            });
        }

        for (qname, qkey) in [
            ("point_lookup", "point_lookup_full"),
            ("substring_search", "substring_rare"),
        ] {
            let base = reset_peak();
            let t0 = Instant::now();
            let matched = run_vortex_query(
                vortex_session,
                &vx_files,
                TableKindBench::Spans,
                qkey,
                sample_tid,
            )
            .await?;
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mem = snapshot(base);
            read_rows.push(ReadMemRow {
                workload: wl_name.to_owned(),
                query: qname.to_owned(),
                format: "vortex".to_owned(),
                matched,
                elapsed_ms,
                mem,
            });
        }
    }

    // 3. Strata
    if enabled.contains(&"strata") {
        let base = reset_peak();
        let (files, w_ms) =
            write_strata_files(TableKindBench::Spans, std::slice::from_ref(&arrow_batch))?;
        let w_mem = snapshot(base);
        write_rows.push(WriteMemRow {
            workload: wl_name.to_owned(),
            format: "strata".to_owned(),
            threads: 20,
            stored_bytes: files.iter().map(|b| b.len() as u64).sum(),
            elapsed_ms: w_ms,
            mem: w_mem,
        });

        let st_files = open_strata_local(&files).await?;

        // full_scan
        {
            let base = reset_peak();
            let t0 = Instant::now();
            let req = make_scan_req(full_scan_cols.clone(), None, ScanKind::Scan);
            let mut matched = 0usize;
            for f in &st_files {
                let batches = f.scan(&req).await?;
                for b in &batches {
                    matched += b.num_rows();
                }
            }
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mem = snapshot(base);
            read_rows.push(ReadMemRow {
                workload: wl_name.to_owned(),
                query: "full_scan".to_owned(),
                format: "strata".to_owned(),
                matched,
                elapsed_ms,
                mem,
            });
        }

        for (qname, qkey) in [
            ("point_lookup", "point_lookup_full"),
            ("substring_search", "substring_rare"),
        ] {
            let base = reset_peak();
            let t0 = Instant::now();
            match run_strata_query(&st_files, TableKindBench::Spans, qkey, sample_tid).await {
                Ok(matched) => {
                    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                    let mem = snapshot(base);
                    read_rows.push(ReadMemRow {
                        workload: wl_name.to_owned(),
                        query: qname.to_owned(),
                        format: "strata".to_owned(),
                        matched,
                        elapsed_ms,
                        mem,
                    });
                }
                Err(e) => {
                    notes.push(format!("{wl_name} `strata` `{qname}` = N/A ({e})"));
                }
            }
        }
    }

    // 4..=5. Statam L0Fast & L1Compact (1T & 8T write, 1T read)
    for (fmt, profile, use_pack) in [
        ("statam_l0", WriteProfile::L0Fast, false),
        ("statam_l1", WriteProfile::L1Compact, use_pack_for_l1),
    ] {
        if !enabled.contains(&fmt) {
            continue;
        }

        // 8T write peak measurement (drop output immediately after measuring)
        {
            let base = reset_peak();
            let (file_8t, pack_8t, w_ms_8t) =
                write_statam_with_options(profile, &cetra_batch, use_pack, 8)?;
            let w_mem_8t = snapshot(base);
            let stored_8t = file_8t.len() as u64 + pack_8t.as_ref().map_or(0, |p| p.len() as u64);
            write_rows.push(WriteMemRow {
                workload: wl_name.to_owned(),
                format: fmt.to_owned(),
                threads: 8,
                stored_bytes: stored_8t,
                elapsed_ms: w_ms_8t,
                mem: w_mem_8t,
            });
        }

        // 1T write peak measurement (retain output for read measurements)
        let base = reset_peak();
        let (files, pack_opt, w_ms) = if use_pack {
            let (f, p, ms) = write_statam_with_options(profile, &cetra_batch, true, 1)?;
            (vec![f], p, ms)
        } else {
            write_statam_files(
                profile,
                TableKindBench::Spans,
                std::slice::from_ref(&cetra_batch),
                false,
                1,
            )?
        };
        let w_mem = snapshot(base);
        let stored_bytes = files.iter().map(|b| b.len() as u64).sum::<u64>()
            + pack_opt.as_ref().map_or(0, |p| p.len() as u64);
        write_rows.push(WriteMemRow {
            workload: wl_name.to_owned(),
            format: fmt.to_owned(),
            threads: 1,
            stored_bytes,
            elapsed_ms: w_ms,
            mem: w_mem,
        });

        let resolver: Option<Arc<dyn PackResolver>> = if let Some(pb) = &pack_opt {
            let mut res = InMemoryPackResolver::new();
            res.insert_bytes(PACK_ID, pb.clone())?;
            Some(Arc::new(res))
        } else {
            None
        };
        let readers: Vec<StatamReader> = files
            .iter()
            .map(|b| StatamReader::open(Arc::new(SliceReader::new(b.clone())), resolver.clone()))
            .collect::<Result<_, _>>()?;

        // full_scan
        {
            let base = reset_peak();
            let t0 = Instant::now();
            let req = ScanRequest {
                projection: Some(full_scan_cols.clone()),
                ..ScanRequest::default()
            };
            let mut matched = 0usize;
            for r in &readers {
                let batches = r.scan(&req)?;
                for b in &batches {
                    matched += b.num_rows();
                }
            }
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mem = snapshot(base);
            read_rows.push(ReadMemRow {
                workload: wl_name.to_owned(),
                query: "full_scan".to_owned(),
                format: fmt.to_owned(),
                matched,
                elapsed_ms,
                mem,
            });
        }

        for (qname, qkey) in [
            ("point_lookup", "point_lookup_full"),
            ("substring_search", "substring_rare"),
        ] {
            let base = reset_peak();
            let t0 = Instant::now();
            let matched = run_statam_query(&readers, TableKindBench::Spans, qkey, sample_tid, 1)?;
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let mem = snapshot(base);
            read_rows.push(ReadMemRow {
                workload: wl_name.to_owned(),
                query: qname.to_owned(),
                format: fmt.to_owned(),
                matched,
                elapsed_ms,
                mem,
            });
        }
    }

    Ok(())
}

fn render_peak_mem_report(
    scale: BenchScale,
    enabled: &[&str],
    write_rows: &[WriteMemRow],
    read_rows: &[ReadMemRow],
    notes: &[String],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\n# Peak Memory Benchmark Report (`TrackingAllocator`, `scale={}`)",
        scale.label()
    );
    let _ = writeln!(out, "\n- **Host:** {}", machine_summary());
    let _ = writeln!(out, "- **Formats:** `{}`", enabled.join("`, `"));
    let _ = writeln!(
        out,
        "- **Allocator:** Process-wide `AtomicUsize` `TrackingAllocator` (captures multi-threaded peak heap across Strata 20T and Statam 8T)"
    );
    for n in notes {
        let _ = writeln!(out, "- **Note:** {n}");
    }

    let _ = writeln!(out, "\n## 1. Write Peak Heap & Total Allocated");
    let _ = writeln!(
        out,
        "\n| Workload | Format | Threads | Stored (KiB) | Write (ms) | Peak Delta (MiB) | Peak Live (MiB) | Total Alloc (MiB) |"
    );
    let _ = writeln!(out, "|---|---|---:|---:|---:|---:|---:|---:|");
    for r in write_rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.2} | {} | {} | {} |",
            r.workload,
            r.format,
            r.threads,
            format_kib(r.stored_bytes),
            r.elapsed_ms,
            format_mib(r.mem.peak_delta_bytes),
            format_mib(r.mem.peak_live_bytes),
            format_mib(r.mem.allocated_bytes),
        );
    }

    let _ = writeln!(out, "\n## 2. Read Peak Heap & Total Allocated");
    let _ = writeln!(
        out,
        "\n| Workload | Query | Format | Matched | Read (ms) | Peak Delta (MiB) | Peak Live (MiB) | Total Alloc (MiB) |"
    );
    let _ = writeln!(out, "|---|---|---|---:|---:|---:|---:|---:|");
    for r in read_rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.3} | {} | {} | {} |",
            r.workload,
            r.query,
            r.format,
            r.matched,
            r.elapsed_ms,
            format_mib(r.mem.peak_delta_bytes),
            format_mib(r.mem.peak_live_bytes),
            format_mib(r.mem.allocated_bytes),
        );
    }

    out
}

async fn run() -> BenchResult<()> {
    let enabled = parse_cli_formats()?;
    let scale = BenchScale::from_env();
    let vortex_session = new_vortex_session();

    let (w3_n, w1_n) = match scale {
        BenchScale::Quick => (25_000usize, 1_800usize),
        BenchScale::Full => (250_000usize, 20_000usize),
    };

    let mut write_rows = Vec::new();
    let mut read_rows = Vec::new();
    let mut notes = Vec::new();

    // Workload 1: W3_spans (250k x 55 cols)
    {
        let mut w3_spans = generate_w3_narrow_spans(w3_n);
        sort_spans(&mut w3_spans);
        bench_workload_peak_mem(
            "W3_spans",
            &w3_spans,
            false,
            &enabled,
            &vortex_session,
            &mut write_rows,
            &mut read_rows,
            &mut notes,
        )
        .await?;
    }

    // Workload 2 (optional): W1_agents if STATAM_SWE_DIR is set
    if let Ok(dir) = std::env::var("STATAM_SWE_DIR")
        && !dir.trim().is_empty()
    {
        let mut w1_spans = load_swe_trajectories(Path::new(&dir), w1_n)?;
        plant_rare_needle(&mut w1_spans, 12);
        sort_spans(&mut w1_spans);
        bench_workload_peak_mem(
            "W1_agents",
            &w1_spans,
            true,
            &enabled,
            &vortex_session,
            &mut write_rows,
            &mut read_rows,
            &mut notes,
        )
        .await?;
    } else {
        notes.push("`W1_agents` skipped (`STATAM_SWE_DIR` not set)".to_owned());
    }

    let md = render_peak_mem_report(scale, &enabled, &write_rows, &read_rows, &notes);
    println!("{md}");
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    if let Err(err) = run().await {
        eprintln!("peak_mem_bench failed: {err}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
