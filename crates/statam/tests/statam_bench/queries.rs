use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arrow::array::{
    Array, FixedSizeBinaryArray, Float64Array, Int64Array, MapArray, RecordBatch, StringArray,
    TimestampNanosecondArray,
};
use arrow::datatypes::SchemaRef;
use bytes::Bytes;
use cetra_column::{Batch, Column, FlatData, Selection};
use cetra_statam::{
    ChunkPackBuilder, ChunkPackReader, CoalesceMode, IdLookup, InMemoryPackResolver, PackResolver,
    PackWriteOptions, Predicate, RangeReader, S3_CACHE_BLOCK_BYTES, ScanRequest, SimulatedS3Config,
    SimulatedS3Reader, SliceReader, StatamError, StatamReader, StatamWriter, SubstringQuery,
    TableKind, TimeRange, WriteOptions, WriteProfile, ZonePred, ZoneValue,
};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
};
use parquet::arrow::arrow_writer::ArrowWriter;

use crate::report::{QueryBenchRow, WriteBenchRow};
use crate::s3_parquet::{
    PqFilter, SimS3ChunkReader, execute_parquet_scan, open_parquet_builder,
    prune_parquet_rgs_and_pages,
};
use crate::schema::{
    BenchResult, ParquetVariant, ScoreRow, SpanRow, TableKindBench, arrow_scores_schema,
    arrow_spans_schema, parquet_writer_props, scores_to_arrow, scores_to_cetra_batch,
    spans_to_arrow, spans_to_cetra_batch,
};
use crate::workloads::{ABSENT_NEEDLE, DAY_START, HOUR_NS, RARE_NEEDLE};

const PACK_ID: [u8; 16] = *b"statam_bench_pk1";

#[derive(Clone)]
struct SimS3PackResolver {
    pack_id: [u8; 16],
    s3: Arc<SimulatedS3Reader>,
    opened: Arc<Mutex<Option<Arc<ChunkPackReader>>>>,
}

impl SimS3PackResolver {
    fn new(pack_id: [u8; 16], s3: Arc<SimulatedS3Reader>) -> Self {
        Self {
            pack_id,
            s3,
            opened: Arc::new(Mutex::new(None)),
        }
    }

    fn reset_opened(&self) {
        let mut guard = self.opened.lock().unwrap_or_else(|p| p.into_inner());
        *guard = None;
    }
}

impl PackResolver for SimS3PackResolver {
    fn resolve(&self, pack_id: [u8; 16]) -> Result<Arc<ChunkPackReader>, StatamError> {
        if pack_id != self.pack_id {
            return Err(StatamError::MissingPack { pack_id });
        }
        let mut guard = self.opened.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(p) = guard.as_ref() {
            return Ok(Arc::clone(p));
        }
        let reader: Arc<dyn RangeReader> = Arc::clone(&self.s3) as Arc<dyn RangeReader>;
        let pack = Arc::new(ChunkPackReader::open(reader)?);
        *guard = Some(Arc::clone(&pack));
        Ok(pack)
    }
}

pub(crate) struct EncodedWorkload {
    pub(crate) name: String,
    pub(crate) table: TableKindBench,
    pub(crate) pq_default: Vec<Bytes>,
    pub(crate) pq_tuned: Vec<Bytes>,
    pub(crate) pq_cetra: Vec<Bytes>,
    pub(crate) statam_l0: Vec<Bytes>,
    pub(crate) statam_l1: Vec<Bytes>,
    pub(crate) statam_l1_pack: Option<Bytes>,
    pub(crate) sample_trace_id: [u8; 16],
    pub(crate) write_rows: Vec<WriteBenchRow>,
}

fn spans_raw_bytes(slices: &[&[SpanRow]]) -> u64 {
    let mut sum = 0u64;
    for s in slices {
        for r in *s {
            sum += 128
                + r.project_id.len() as u64
                + r.name.len() as u64
                + r.input.as_ref().map_or(0, |x| x.len() as u64)
                + r.output.as_ref().map_or(0, |x| x.len() as u64)
                + r.system_instructions.as_ref().map_or(0, |x| x.len() as u64)
                + r.tool_definitions.as_ref().map_or(0, |x| x.len() as u64);
        }
    }
    sum
}

fn scores_raw_bytes(rows: &[ScoreRow]) -> u64 {
    rows.iter()
        .map(|r| {
            112 + r.project_id.len() as u64
                + r.score_id.len() as u64
                + r.name.len() as u64
                + r.reason.as_ref().map_or(0, |x| x.len() as u64)
        })
        .sum()
}

fn write_parquet_files(
    variant: ParquetVariant,
    table: TableKindBench,
    schema: &SchemaRef,
    batches: &[RecordBatch],
) -> BenchResult<(Vec<Bytes>, f64)> {
    let t0 = Instant::now();
    let mut out = Vec::with_capacity(batches.len());
    for b in batches {
        let props = parquet_writer_props(variant, table, schema, b.num_rows())?;
        let mut buf = Vec::new();
        {
            let mut w = ArrowWriter::try_new(&mut buf, Arc::clone(schema), Some(props))?;
            let max_rg_rows = if variant == ParquetVariant::Cetra {
                100_000
            } else {
                usize::MAX
            };
            for off in (0..b.num_rows()).step_by(8_192) {
                let len = 8_192.min(b.num_rows() - off);
                w.write(&b.slice(off, len))?;
                if variant == ParquetVariant::Cetra
                    && (w.in_progress_size() >= 64 << 20 || w.in_progress_rows() >= max_rg_rows)
                {
                    w.flush()?;
                }
            }
            w.close()?;
        }
        out.push(Bytes::from(buf));
    }
    Ok((out, t0.elapsed().as_secs_f64() * 1000.0))
}

fn write_statam_files(
    profile: WriteProfile,
    table: TableKindBench,
    batches: &[Batch],
    use_pack: bool,
) -> BenchResult<(Vec<Bytes>, Option<Bytes>, f64)> {
    let t0 = Instant::now();
    let table_kind = match table {
        TableKindBench::Spans => TableKind::Spans,
        TableKindBench::Scores => TableKind::Scores,
    };
    let sort_cols = match table {
        TableKindBench::Spans => vec![1u16, 6u16], // trace_id, start_time
        TableKindBench::Scores => vec![20u16],     // created_at
    };

    let mut pb = use_pack.then(|| ChunkPackBuilder::new(PACK_ID, PackWriteOptions::default()));
    let mut files = Vec::with_capacity(batches.len());
    for b in batches {
        let mut opts = WriteOptions::for_table(table_kind, profile);
        opts.sort_cols = sort_cols.clone();
        if pb.is_some() {
            opts.pack_id = Some(PACK_ID);
        }
        let mut buf = Vec::new();
        let mut w = StatamWriter::new(
            std::io::Cursor::new(&mut buf),
            Arc::clone(b.schema()),
            opts,
            pb.as_mut(),
        )?;
        w.write_batch(b)?;
        w.finish()?;
        files.push(Bytes::from(buf));
    }
    let pack_bytes = if let Some(builder) = pb {
        let mut pbuf = Vec::new();
        builder.finish(&mut pbuf)?;
        Some(Bytes::from(pbuf))
    } else {
        None
    };
    Ok((files, pack_bytes, t0.elapsed().as_secs_f64() * 1000.0))
}

pub(crate) fn encode_spans_workload(
    name: &str,
    slices: &[&[SpanRow]],
) -> BenchResult<EncodedWorkload> {
    let raw_bytes = spans_raw_bytes(slices);
    let arrow_schema = arrow_spans_schema();
    let mut arrow_batches = Vec::with_capacity(slices.len());
    let mut cetra_batches = Vec::with_capacity(slices.len());
    for s in slices {
        arrow_batches.push(spans_to_arrow(s)?);
        cetra_batches.push(spans_to_cetra_batch(s)?);
    }

    let sample_trace_id = slices
        .first()
        .and_then(|s| s.get(s.len() / 2))
        .map_or([1u8; 16], |r| r.trace_id);

    let (pq_default, pq_def_ms) = write_parquet_files(
        ParquetVariant::Default,
        TableKindBench::Spans,
        &arrow_schema,
        &arrow_batches,
    )?;
    let (pq_tuned, pq_tun_ms) = write_parquet_files(
        ParquetVariant::Tuned,
        TableKindBench::Spans,
        &arrow_schema,
        &arrow_batches,
    )?;
    let (pq_cetra, pq_cet_ms) = write_parquet_files(
        ParquetVariant::Cetra,
        TableKindBench::Spans,
        &arrow_schema,
        &arrow_batches,
    )?;
    let (statam_l0, _, l0_ms) = write_statam_files(
        WriteProfile::L0Fast,
        TableKindBench::Spans,
        &cetra_batches,
        false,
    )?;
    let (statam_l1, statam_l1_pack, l1_ms) = write_statam_files(
        WriteProfile::L1Compact,
        TableKindBench::Spans,
        &cetra_batches,
        true,
    )?;

    let write_rows = build_write_rows(
        name,
        raw_bytes,
        (&pq_default, pq_def_ms),
        (&pq_tuned, pq_tun_ms),
        (&pq_cetra, pq_cet_ms),
        (&statam_l0, l0_ms),
        (&statam_l1, statam_l1_pack.as_ref(), l1_ms),
    );

    Ok(EncodedWorkload {
        name: name.to_owned(),
        table: TableKindBench::Spans,
        pq_default,
        pq_tuned,
        pq_cetra,
        statam_l0,
        statam_l1,
        statam_l1_pack,
        sample_trace_id,
        write_rows,
    })
}

pub(crate) fn encode_scores_workload(
    name: &str,
    rows: &[ScoreRow],
) -> BenchResult<EncodedWorkload> {
    let raw_bytes = scores_raw_bytes(rows);
    let arrow_schema = arrow_scores_schema();
    let arrow_batches = vec![scores_to_arrow(rows)?];
    let cetra_batches = vec![scores_to_cetra_batch(rows)?];
    let sample_trace_id = rows.iter().find_map(|r| r.trace_id).unwrap_or([1u8; 16]);

    let (pq_default, pq_def_ms) = write_parquet_files(
        ParquetVariant::Default,
        TableKindBench::Scores,
        &arrow_schema,
        &arrow_batches,
    )?;
    let (pq_tuned, pq_tun_ms) = write_parquet_files(
        ParquetVariant::Tuned,
        TableKindBench::Scores,
        &arrow_schema,
        &arrow_batches,
    )?;
    let (pq_cetra, pq_cet_ms) = write_parquet_files(
        ParquetVariant::Cetra,
        TableKindBench::Scores,
        &arrow_schema,
        &arrow_batches,
    )?;
    let (statam_l0, _, l0_ms) = write_statam_files(
        WriteProfile::L0Fast,
        TableKindBench::Scores,
        &cetra_batches,
        false,
    )?;
    let (statam_l1, statam_l1_pack, l1_ms) = write_statam_files(
        WriteProfile::L1Compact,
        TableKindBench::Scores,
        &cetra_batches,
        false,
    )?;

    let write_rows = build_write_rows(
        name,
        raw_bytes,
        (&pq_default, pq_def_ms),
        (&pq_tuned, pq_tun_ms),
        (&pq_cetra, pq_cet_ms),
        (&statam_l0, l0_ms),
        (&statam_l1, statam_l1_pack.as_ref(), l1_ms),
    );

    Ok(EncodedWorkload {
        name: name.to_owned(),
        table: TableKindBench::Scores,
        pq_default,
        pq_tuned,
        pq_cetra,
        statam_l0,
        statam_l1,
        statam_l1_pack,
        sample_trace_id,
        write_rows,
    })
}

fn build_write_rows(
    name: &str,
    raw_bytes: u64,
    pq_default: (&[Bytes], f64),
    pq_tuned: (&[Bytes], f64),
    pq_cetra: (&[Bytes], f64),
    l0: (&[Bytes], f64),
    l1: (&[Bytes], Option<&Bytes>, f64),
) -> Vec<WriteBenchRow> {
    let mib = raw_bytes as f64 / (1024.0 * 1024.0);
    let mk = |fmt: &str, stored: u64, ms: f64| WriteBenchRow {
        workload: name.to_owned(),
        format: fmt.to_owned(),
        raw_bytes,
        stored_bytes: stored,
        write_ms: ms,
        write_mib_s: if ms > 0.0 { mib / (ms / 1000.0) } else { 0.0 },
    };
    let sum_len = |bs: &[Bytes]| -> u64 { bs.iter().map(|b| b.len() as u64).sum() };
    let l1_total = sum_len(l1.0) + l1.1.map_or(0, |p| p.len() as u64);
    vec![
        mk("pq_default", sum_len(pq_default.0), pq_default.1),
        mk("pq_tuned", sum_len(pq_tuned.0), pq_tuned.1),
        mk("pq_cetra", sum_len(pq_cetra.0), pq_cetra.1),
        mk("statam_l0", sum_len(l0.0), l0.1),
        mk("statam_l1", l1_total, l1.2),
    ]
}

pub(crate) fn bench_tiny_flushes(flushes: &[Vec<SpanRow>]) -> BenchResult<Vec<WriteBenchRow>> {
    let n = flushes.len() as u64;
    let mut raw_sum = 0u64;
    let mut sz = [0u64; 5];
    let mut ms = [0.0f64; 5];
    let arrow_schema = arrow_spans_schema();

    for rows in flushes {
        raw_sum += spans_raw_bytes(&[rows.as_slice()]);
        let ab = [spans_to_arrow(rows)?];
        let cb = [spans_to_cetra_batch(rows)?];
        let (d, d_ms) = write_parquet_files(
            ParquetVariant::Default,
            TableKindBench::Spans,
            &arrow_schema,
            &ab,
        )?;
        let (t, t_ms) = write_parquet_files(
            ParquetVariant::Tuned,
            TableKindBench::Spans,
            &arrow_schema,
            &ab,
        )?;
        let (c, c_ms) = write_parquet_files(
            ParquetVariant::Cetra,
            TableKindBench::Spans,
            &arrow_schema,
            &ab,
        )?;
        let (l0, _, l0_ms) =
            write_statam_files(WriteProfile::L0Fast, TableKindBench::Spans, &cb, false)?;
        let (l1, p1, l1_ms) =
            write_statam_files(WriteProfile::L1Compact, TableKindBench::Spans, &cb, true)?;
        sz[0] += d[0].len() as u64;
        ms[0] += d_ms;
        sz[1] += t[0].len() as u64;
        ms[1] += t_ms;
        sz[2] += c[0].len() as u64;
        ms[2] += c_ms;
        sz[3] += l0[0].len() as u64;
        ms[3] += l0_ms;
        sz[4] += l1[0].len() as u64 + p1.map_or(0, |p| p.len() as u64);
        ms[4] += l1_ms;
    }

    let raw_avg = raw_sum / n;
    let mib_avg = raw_avg as f64 / (1024.0 * 1024.0);
    let fmts = [
        "pq_default",
        "pq_tuned",
        "pq_cetra",
        "statam_l0",
        "statam_l1",
    ];
    Ok(fmts
        .iter()
        .enumerate()
        .map(|(i, fmt)| {
            let avg_ms = ms[i] / n as f64;
            WriteBenchRow {
                workload: "W4_1row_avg".to_owned(),
                format: (*fmt).to_owned(),
                raw_bytes: raw_avg,
                stored_bytes: sz[i] / n,
                write_ms: avg_ms,
                write_mib_s: if avg_ms > 0.0 {
                    mib_avg / (avg_ms / 1000.0)
                } else {
                    0.0
                },
            }
        })
        .collect())
}

pub(crate) fn run_all_queries(
    wl: &EncodedWorkload,
    include_substrings: bool,
) -> BenchResult<Vec<QueryBenchRow>> {
    let mut rows = Vec::new();
    match wl.table {
        TableKindBench::Spans => {
            rows.extend(run_query_across_formats(wl, "cold_open_prune", true)?);
            rows.extend(run_query_across_formats(wl, "point_lookup_narrow", false)?);
            rows.extend(run_query_across_formats(wl, "point_lookup_full", false)?);
            rows.extend(run_query_across_formats(wl, "filtered_count", false)?);
            rows.extend(run_query_across_formats(wl, "time_bucket_agg", false)?);
            rows.extend(run_query_across_formats(wl, "top_k_list", false)?);
            rows.extend(run_query_across_formats(wl, "map_key_lookup", false)?);
            if include_substrings {
                rows.extend(run_query_across_formats(wl, "substring_rare", false)?);
                rows.extend(run_query_across_formats(wl, "substring_absent", false)?);
            }
        }
        TableKindBench::Scores => {
            rows.extend(run_query_across_formats(wl, "cold_open_prune", true)?);
            rows.extend(run_query_across_formats(wl, "point_lookup_narrow", false)?);
            rows.extend(run_query_across_formats(wl, "filtered_count", false)?);
            rows.extend(run_query_across_formats(wl, "map_key_lookup", false)?);
        }
    }
    Ok(rows)
}

fn median_ms<F: FnMut() -> BenchResult<usize>>(mut f: F, runs: usize) -> BenchResult<(usize, f64)> {
    let expected = f()?; // 1 warmup
    let mut times = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t0 = Instant::now();
        let got = f()?;
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
        if got != expected {
            return Err(format!("inconsistent warmup vs run count: {got} != {expected}").into());
        }
    }
    times.sort_by(|a, b| a.total_cmp(b));
    Ok((expected, times[times.len() / 2]))
}

fn run_query_across_formats(
    wl: &EncodedWorkload,
    qname: &str,
    is_cold_open: bool,
) -> BenchResult<Vec<QueryBenchRow>> {
    let s3_cfg = SimulatedS3Config::default();
    let warm_runs = 5usize;
    let mut results = Vec::with_capacity(5);
    let mut expected_matches: Option<usize> = None;

    // 1..=3: Parquet formats
    for (fmt, files, with_pi) in [
        ("pq_default", &wl.pq_default, false),
        ("pq_tuned", &wl.pq_tuned, true),
        ("pq_cetra", &wl.pq_cetra, true),
    ] {
        // Local warm CPU
        let warm_metas: Vec<ArrowReaderMetadata> = files
            .iter()
            .map(|b| {
                let opts = ArrowReaderOptions::new().with_page_index_policy(if with_pi {
                    parquet::file::metadata::PageIndexPolicy::Optional
                } else {
                    parquet::file::metadata::PageIndexPolicy::Skip
                });
                ArrowReaderMetadata::load(b, opts)
            })
            .collect::<Result<_, _>>()?;
        let (matched, local_ms) = median_ms(
            || run_pq_query_local(files, &warm_metas, wl.table, qname, wl.sample_trace_id),
            warm_runs,
        )?;

        // Cold S3 simulation
        let s3_readers: Vec<SimS3ChunkReader> = files
            .iter()
            .map(|b| {
                SimS3ChunkReader::new(Arc::new(SimulatedS3Reader::from_bytes(b.clone(), s3_cfg)))
            })
            .collect();
        for r in &s3_readers {
            r.s3().evict_all();
            r.s3().reset_counters();
        }
        let s3_matched =
            run_pq_query_s3(&s3_readers, with_pi, wl.table, qname, wl.sample_trace_id)?;
        if s3_matched != matched {
            return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
        }
        let gets: u64 = s3_readers.iter().map(|r| r.s3().counters().gets).sum();
        let bytes: u64 = s3_readers
            .iter()
            .map(|r| r.s3().counters().bytes_from_s3)
            .sum();
        let s3_ms: f64 = s3_readers
            .iter()
            .map(|r| r.s3().simulated_latency_ms())
            .sum();

        match expected_matches {
            None => expected_matches = Some(matched),
            Some(exp) if exp != matched => {
                return Err(format!(
                    "parity mismatch on {} {qname} ({fmt}): got {matched}, expected {exp}",
                    wl.name
                )
                .into());
            }
            _ => {}
        }

        results.push(QueryBenchRow {
            workload: wl.name.clone(),
            query: qname.to_owned(),
            format: fmt.to_owned(),
            rows_matched: matched,
            local_ms,
            s3_ms,
            s3_gets: gets,
            s3_bytes: bytes,
            cold_open_gets: is_cold_open.then_some(gets),
            cold_open_tail_bytes: is_cold_open.then_some(bytes),
        });
    }

    // 4..=5: Statam L0Fast & L1Compact
    for (fmt, files, pack_opt) in [
        ("statam_l0", &wl.statam_l0, None),
        ("statam_l1", &wl.statam_l1, wl.statam_l1_pack.as_ref()),
    ] {
        // Local warm CPU
        let warm_pack_resolver: Option<Arc<dyn PackResolver>> = if let Some(pb) = pack_opt {
            let mut res = InMemoryPackResolver::new();
            res.insert_bytes(PACK_ID, pb.clone())?;
            Some(Arc::new(res))
        } else {
            None
        };
        let warm_readers: Vec<StatamReader> = files
            .iter()
            .map(|b| {
                StatamReader::open(
                    Arc::new(SliceReader::new(b.clone())),
                    warm_pack_resolver.clone(),
                )
            })
            .collect::<Result<_, _>>()?;
        let (matched, local_ms) = median_ms(
            || run_statam_query(&warm_readers, wl.table, qname, wl.sample_trace_id),
            warm_runs,
        )?;

        // Cold S3 simulation
        let s3_files: Vec<Arc<SimulatedS3Reader>> = files
            .iter()
            .map(|b| Arc::new(SimulatedS3Reader::from_bytes(b.clone(), s3_cfg)))
            .collect();
        let s3_pack =
            pack_opt.map(|pb| Arc::new(SimulatedS3Reader::from_bytes(pb.clone(), s3_cfg)));
        let s3_resolver = s3_pack
            .as_ref()
            .map(|ps3| Arc::new(SimS3PackResolver::new(PACK_ID, Arc::clone(ps3))));

        for f in &s3_files {
            f.evict_all();
            f.reset_counters();
        }
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
        let s3_statam_readers: Vec<StatamReader> = s3_files
            .iter()
            .map(|f| StatamReader::open(Arc::clone(f) as Arc<dyn RangeReader>, pack_dyn.clone()))
            .collect::<Result<_, _>>()?;

        let s3_matched = run_statam_query(&s3_statam_readers, wl.table, qname, wl.sample_trace_id)?;
        if s3_matched != matched {
            return Err(format!("{fmt} {qname}: s3 {s3_matched} != local {matched}").into());
        }

        let mut gets: u64 = s3_files.iter().map(|f| f.counters().gets).sum();
        let mut bytes: u64 = s3_files.iter().map(|f| f.counters().bytes_from_s3).sum();
        let mut s3_ms: f64 = s3_files.iter().map(|f| f.simulated_latency_ms()).sum();
        if let Some(ps3) = &s3_pack {
            gets += ps3.counters().gets;
            bytes += ps3.counters().bytes_from_s3;
            s3_ms += ps3.simulated_latency_ms();
        }
        let tail_bytes: Option<u64> = is_cold_open.then(|| {
            s3_statam_readers
                .iter()
                .map(|r| u64::from(r.meta().postscript.hot_tail_len).min(S3_CACHE_BLOCK_BYTES))
                .max()
                .unwrap_or(0)
        });

        if let Some(exp) = expected_matches
            && exp != matched
        {
            return Err(format!(
                "parity mismatch on {} {qname} ({fmt}): got {matched}, expected {exp}",
                wl.name
            )
            .into());
        }

        results.push(QueryBenchRow {
            workload: wl.name.clone(),
            query: qname.to_owned(),
            format: fmt.to_owned(),
            rows_matched: matched,
            local_ms,
            s3_ms,
            s3_gets: gets,
            s3_bytes: bytes,
            cold_open_gets: is_cold_open.then_some(gets),
            cold_open_tail_bytes: tail_bytes,
        });
    }

    Ok(results)
}

fn run_pq_query_local(
    files: &[Bytes],
    metas: &[ArrowReaderMetadata],
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<usize> {
    if qname == "cold_open_prune" {
        let absent = [0xAAu8; 16];
        let mut hits = 0usize;
        for (b, m) in files.iter().zip(metas) {
            let descr = m.metadata().file_metadata().schema_descr();
            if let Some(leaf) = descr
                .columns()
                .iter()
                .position(|c| c.path().parts().len() == 1 && c.path().parts()[0] == "trace_id")
            {
                for rg_idx in 0..m.metadata().num_row_groups() {
                    let chunk = m.metadata().row_group(rg_idx).column(leaf);
                    if let Some(sbbf) =
                        parquet::bloom_filter::Sbbf::read_from_column_chunk(chunk, b)?
                        && sbbf.check(absent.as_slice())
                    {
                        hits += 1;
                    }
                }
            }
        }
        let _ = hits;
        return Ok(0);
    }

    let mut total = 0usize;
    let mut agg: BTreeMap<(i64, String), (usize, f64, i64)> = BTreeMap::new();
    let mut topk: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();

    for (b, m) in files.iter().zip(metas) {
        let mut builder = ParquetRecordBatchReaderBuilder::new_with_metadata(b.clone(), m.clone());
        let pq_schema = builder.parquet_schema().clone();
        let proj = pq_projection(table, qname);
        if let Some(cols) = proj {
            builder = builder.with_projection(ProjectionMask::roots(&pq_schema, cols));
        }
        let batches: Vec<RecordBatch> = builder
            .with_batch_size(16_384)
            .build()?
            .collect::<Result<_, _>>()?;
        total += eval_pq_batches(table, qname, sample_tid, &batches, &mut agg, &mut topk)?;
    }

    if qname == "time_bucket_agg" {
        return Ok(agg.values().map(|v| v.0).sum());
    }
    if qname == "top_k_list" {
        return Ok(topk.len());
    }
    Ok(total)
}

fn run_pq_query_s3(
    readers: &[SimS3ChunkReader],
    with_pi: bool,
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<usize> {
    if qname == "cold_open_prune" {
        let absent = [0xAAu8; 16];
        let filters = [PqFilter::EqBytes {
            col: "trace_id".to_owned(),
            val: absent.to_vec(),
        }];
        for r in readers {
            let builder = open_parquet_builder(r, with_pi)?;
            let meta = builder.metadata();
            let _ = prune_parquet_rgs_and_pages(r, meta, &filters)?;
        }
        return Ok(0);
    }

    let mut total = 0usize;
    let mut agg: BTreeMap<(i64, String), (usize, f64, i64)> = BTreeMap::new();
    let mut topk: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();

    let (t_lo, t_hi) = (DAY_START + 8 * HOUR_NS, DAY_START + 14 * HOUR_NS);
    let (top_lo, top_hi) = (DAY_START + 6 * HOUR_NS, DAY_START + 18 * HOUR_NS);

    for r in readers {
        let (proj, filters): (Vec<usize>, Vec<PqFilter>) = match (table, qname) {
            (TableKindBench::Spans, "point_lookup_narrow") => (
                vec![1, 2, 3, 4, 6, 7, 9, 21, 30, 33],
                vec![PqFilter::EqBytes {
                    col: "trace_id".to_owned(),
                    val: sample_tid.to_vec(),
                }],
            ),
            (TableKindBench::Spans, "point_lookup_full") => (
                (0..56).collect(),
                vec![PqFilter::EqBytes {
                    col: "trace_id".to_owned(),
                    val: sample_tid.to_vec(),
                }],
            ),
            (TableKindBench::Scores, "point_lookup_narrow") => (
                vec![1, 3, 7, 9, 10, 20],
                vec![PqFilter::EqBytes {
                    col: "trace_id".to_owned(),
                    val: sample_tid.to_vec(),
                }],
            ),
            (TableKindBench::Spans, "filtered_count") => (
                vec![],
                vec![
                    PqFilter::I64Between {
                        col: "start_time".to_owned(),
                        lo: t_lo,
                        hi: t_hi,
                    },
                    PqFilter::EqStr {
                        col: "status".to_owned(),
                        val: "error".to_owned(),
                    },
                ],
            ),
            (TableKindBench::Scores, "filtered_count") => (
                vec![],
                vec![
                    PqFilter::I64Between {
                        col: "created_at".to_owned(),
                        lo: t_lo,
                        hi: t_hi,
                    },
                    PqFilter::EqStr {
                        col: "name".to_owned(),
                        val: "faithfulness".to_owned(),
                    },
                ],
            ),
            (TableKindBench::Spans, "time_bucket_agg") => (
                vec![6, 21, 30, 33],
                vec![PqFilter::EqStr {
                    col: "span_kind".to_owned(),
                    val: "llm".to_owned(),
                }],
            ),
            (TableKindBench::Spans, "top_k_list") => (
                vec![1, 2, 4, 6, 8, 9, 21, 30, 33, 41],
                vec![
                    PqFilter::I64Between {
                        col: "start_time".to_owned(),
                        lo: top_lo,
                        hi: top_hi,
                    },
                    PqFilter::EqStr {
                        col: "span_kind".to_owned(),
                        val: "llm".to_owned(),
                    },
                ],
            ),
            (TableKindBench::Spans, "map_key_lookup") => (vec![1, 6, 17], vec![]),
            (TableKindBench::Scores, "map_key_lookup") => (vec![1, 19, 20], vec![]),
            (TableKindBench::Spans, "substring_rare" | "substring_absent") => (vec![1, 37], vec![]),
            _ => ((0..56).collect(), vec![]),
        };

        let batches = execute_parquet_scan(r, with_pi, &proj, &filters, 16_384)?;
        if !filters.is_empty()
            && matches!(
                qname,
                "point_lookup_narrow" | "point_lookup_full" | "filtered_count"
            )
        {
            total += batches.iter().map(RecordBatch::num_rows).sum::<usize>();
        } else if qname == "time_bucket_agg" {
            for b in &batches {
                agg_pq_filtered_batch(b, &mut agg)?;
            }
        } else if qname == "top_k_list" {
            for b in &batches {
                topk_pq_filtered_batch(b, &mut topk)?;
            }
        } else {
            total += eval_pq_batches(table, qname, sample_tid, &batches, &mut agg, &mut topk)?;
        }
    }

    if qname == "time_bucket_agg" {
        return Ok(agg.values().map(|v| v.0).sum());
    }
    if qname == "top_k_list" {
        return Ok(topk.len());
    }
    Ok(total)
}

fn pq_projection(table: TableKindBench, qname: &str) -> Option<Vec<usize>> {
    match (table, qname) {
        (TableKindBench::Spans, "point_lookup_narrow") => {
            Some(vec![1, 2, 3, 4, 6, 7, 9, 21, 30, 33])
        }
        (TableKindBench::Spans, "point_lookup_full") => None,
        (TableKindBench::Spans, "filtered_count") => Some(vec![6, 9]),
        (TableKindBench::Spans, "time_bucket_agg") => Some(vec![5, 6, 21, 30, 33]),
        (TableKindBench::Spans, "top_k_list") => Some(vec![1, 2, 4, 5, 6, 8, 9, 21, 30, 33, 41]),
        (TableKindBench::Spans, "map_key_lookup") => Some(vec![1, 6, 17]),
        (TableKindBench::Spans, "substring_rare" | "substring_absent") => Some(vec![1, 37]),
        (TableKindBench::Scores, "point_lookup_narrow") => Some(vec![1, 3, 7, 9, 10, 20]),
        (TableKindBench::Scores, "filtered_count") => Some(vec![7, 20]),
        (TableKindBench::Scores, "map_key_lookup") => Some(vec![1, 19, 20]),
        _ => None,
    }
}

fn eval_pq_batches(
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
    batches: &[RecordBatch],
    agg: &mut BTreeMap<(i64, String), (usize, f64, i64)>,
    topk: &mut BinaryHeap<Reverse<(i64, usize)>>,
) -> BenchResult<usize> {
    let mut matched = 0usize;
    let (t_lo, t_hi) = (DAY_START + 8 * HOUR_NS, DAY_START + 14 * HOUR_NS);
    let (top_lo, top_hi) = (DAY_START + 6 * HOUR_NS, DAY_START + 18 * HOUR_NS);

    for b in batches {
        match (table, qname) {
            (_, "point_lookup_narrow" | "point_lookup_full") => {
                let tid_col = b
                    .column_by_name("trace_id")
                    .and_then(|c| c.as_any().downcast_ref::<FixedSizeBinaryArray>());
                if let Some(arr) = tid_col {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) && arr.value(i) == sample_tid.as_slice() {
                            matched += 1;
                        }
                    }
                }
            }
            (TableKindBench::Spans, "filtered_count") => {
                let ts = b
                    .column_by_name("start_time")
                    .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray>());
                let st = b
                    .column_by_name("status")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>());
                if let (Some(ts), Some(st)) = (ts, st) {
                    for i in 0..b.num_rows() {
                        let t = ts.value(i);
                        if t >= t_lo && t <= t_hi && st.value(i) == "error" {
                            matched += 1;
                        }
                    }
                }
            }
            (TableKindBench::Scores, "filtered_count") => {
                let ts = b
                    .column_by_name("created_at")
                    .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray>());
                let nm = b
                    .column_by_name("name")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>());
                if let (Some(ts), Some(nm)) = (ts, nm) {
                    for i in 0..b.num_rows() {
                        let t = ts.value(i);
                        if t >= t_lo && t <= t_hi && nm.value(i) == "faithfulness" {
                            matched += 1;
                        }
                    }
                }
            }
            (TableKindBench::Spans, "time_bucket_agg") => {
                let sk = b
                    .column_by_name("span_kind")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>());
                let ts = b
                    .column_by_name("start_time")
                    .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray>());
                let md = b
                    .column_by_name("model")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>());
                let tok = b
                    .column_by_name("total_tokens")
                    .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
                let cost = b
                    .column_by_name("cost_total")
                    .and_then(|c| c.as_any().downcast_ref::<Float64Array>());
                if let (Some(sk), Some(ts), Some(md), Some(tok), Some(cost)) =
                    (sk, ts, md, tok, cost)
                {
                    for i in 0..b.num_rows() {
                        if sk.value(i) != "llm" {
                            continue;
                        }
                        let bucket = ts.value(i) / HOUR_NS;
                        let model = if md.is_null(i) {
                            "unknown"
                        } else {
                            md.value(i)
                        };
                        let entry = agg.entry((bucket, model.to_owned())).or_insert((0, 0.0, 0));
                        entry.0 += 1;
                        if !cost.is_null(i) {
                            let c = cost.value(i);
                            if !c.is_nan() {
                                entry.1 += c;
                            }
                        }
                        if !tok.is_null(i) {
                            entry.2 += tok.value(i);
                        }
                    }
                }
            }
            (TableKindBench::Spans, "top_k_list") => {
                let sk = b
                    .column_by_name("span_kind")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>());
                let ts = b
                    .column_by_name("start_time")
                    .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray>());
                if let (Some(sk), Some(ts)) = (sk, ts) {
                    for i in 0..b.num_rows() {
                        let t = ts.value(i);
                        if sk.value(i) == "llm" && t >= top_lo && t <= top_hi {
                            push_topk(topk, t, i);
                        }
                    }
                }
            }
            (_, "map_key_lookup") => {
                let map_col = b
                    .column_by_name("metadata")
                    .and_then(|c| c.as_any().downcast_ref::<MapArray>());
                if let Some(ma) = map_col {
                    let keys = ma.keys().as_any().downcast_ref::<StringArray>();
                    let vals = ma.values().as_any().downcast_ref::<StringArray>();
                    if let (Some(ks), Some(vs)) = (keys, vals) {
                        let offsets = ma.value_offsets();
                        for r in 0..ma.len() {
                            if ma.is_null(r) {
                                continue;
                            }
                            let s = offsets[r] as usize;
                            let e = offsets[r + 1] as usize;
                            for j in s..e {
                                if ks.value(j) == "customer_id"
                                    && !vs.is_null(j)
                                    && vs.value(j) == "cust-42"
                                {
                                    matched += 1;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            (TableKindBench::Spans, "substring_rare" | "substring_absent") => {
                let needle = if qname == "substring_rare" {
                    RARE_NEEDLE.as_bytes()
                } else {
                    ABSENT_NEEDLE.as_bytes()
                };
                let inp = b
                    .column_by_name("input")
                    .and_then(|c| c.as_any().downcast_ref::<StringArray>());
                if let Some(arr) = inp {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) && contains_ascii_ci(arr.value(i).as_bytes(), needle) {
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

fn agg_pq_filtered_batch(
    b: &RecordBatch,
    agg: &mut BTreeMap<(i64, String), (usize, f64, i64)>,
) -> BenchResult<()> {
    let ts = b
        .column_by_name("start_time")
        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray>());
    let md = b
        .column_by_name("model")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let tok = b
        .column_by_name("total_tokens")
        .and_then(|c| c.as_any().downcast_ref::<Int64Array>());
    let cost = b
        .column_by_name("cost_total")
        .and_then(|c| c.as_any().downcast_ref::<Float64Array>());
    if let (Some(ts), Some(md), Some(tok), Some(cost)) = (ts, md, tok, cost) {
        for i in 0..b.num_rows() {
            let bucket = ts.value(i) / HOUR_NS;
            let model = if md.is_null(i) {
                "unknown"
            } else {
                md.value(i)
            };
            let entry = agg.entry((bucket, model.to_owned())).or_insert((0, 0.0, 0));
            entry.0 += 1;
            if !cost.is_null(i) {
                let c = cost.value(i);
                if !c.is_nan() {
                    entry.1 += c;
                }
            }
            if !tok.is_null(i) {
                entry.2 += tok.value(i);
            }
        }
    }
    Ok(())
}

fn topk_pq_filtered_batch(
    b: &RecordBatch,
    topk: &mut BinaryHeap<Reverse<(i64, usize)>>,
) -> BenchResult<()> {
    if let Some(ts) = b
        .column_by_name("start_time")
        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray>())
    {
        for i in 0..b.num_rows() {
            push_topk(topk, ts.value(i), i);
        }
    }
    Ok(())
}

fn push_topk(heap: &mut BinaryHeap<Reverse<(i64, usize)>>, ts: i64, row_id: usize) {
    if heap.len() < 50 {
        heap.push(Reverse((ts, row_id)));
    } else if let Some(&Reverse((min_ts, _))) = heap.peek()
        && ts > min_ts
    {
        heap.pop();
        heap.push(Reverse((ts, row_id)));
    }
}

fn contains_ascii_ci(haystack: &[u8], needle_lower: &[u8]) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.len() < needle_lower.len() {
        return false;
    }
    let first = needle_lower[0];
    let limit = haystack.len() - needle_lower.len();
    for i in 0..=limit {
        if haystack[i].to_ascii_lowercase() == first
            && haystack[i..i + needle_lower.len()].eq_ignore_ascii_case(needle_lower)
        {
            return true;
        }
    }
    false
}

fn run_statam_query(
    readers: &[StatamReader],
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<usize> {
    let (t_lo, t_hi) = (DAY_START + 8 * HOUR_NS, DAY_START + 14 * HOUR_NS);
    let (top_lo, top_hi) = (DAY_START + 6 * HOUR_NS, DAY_START + 18 * HOUR_NS);

    let req = match (table, qname) {
        (_, "cold_open_prune") => ScanRequest {
            projection: Some(vec![]),
            id_lookups: vec![IdLookup::eq("trace_id", [0xAAu8; 16])],
            coalesce_mode: CoalesceMode::Point,
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "point_lookup_narrow") => ScanRequest {
            projection: Some(vec![1, 2, 3, 4, 6, 7, 9, 21, 30, 33]),
            id_lookups: vec![IdLookup::eq("trace_id", sample_tid)],
            coalesce_mode: CoalesceMode::Point,
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "point_lookup_full") => ScanRequest {
            projection: None,
            id_lookups: vec![IdLookup::eq("trace_id", sample_tid)],
            coalesce_mode: CoalesceMode::Point,
            ..ScanRequest::default()
        },
        (TableKindBench::Scores, "point_lookup_narrow") => ScanRequest {
            projection: Some(vec![1, 3, 7, 9, 10, 20]),
            id_lookups: vec![IdLookup::eq("trace_id", sample_tid)],
            coalesce_mode: CoalesceMode::Point,
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "filtered_count") => ScanRequest {
            projection: Some(vec![]),
            time_range: TimeRange::new(t_lo, t_hi),
            predicates: vec![Predicate::new(
                "status",
                ZonePred::Eq(ZoneValue::Bytes(b"error".to_vec())),
            )],
            ..ScanRequest::default()
        },
        (TableKindBench::Scores, "filtered_count") => ScanRequest {
            projection: Some(vec![]),
            time_range: TimeRange::new(t_lo, t_hi),
            predicates: vec![Predicate::new(
                "name",
                ZonePred::Eq(ZoneValue::Bytes(b"faithfulness".to_vec())),
            )],
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "time_bucket_agg") => ScanRequest {
            projection: Some(vec![6, 21, 30, 33]),
            predicates: vec![Predicate::new(
                "span_kind",
                ZonePred::Eq(ZoneValue::Bytes(b"llm".to_vec())),
            )],
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "top_k_list") => ScanRequest {
            projection: Some(vec![1, 2, 4, 6, 8, 9, 21, 30, 33, 41]),
            time_range: TimeRange::new(top_lo, top_hi),
            predicates: vec![Predicate::new(
                "span_kind",
                ZonePred::Eq(ZoneValue::Bytes(b"llm".to_vec())),
            )],
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "map_key_lookup") => ScanRequest {
            projection: Some(vec![1, 6]),
            map_keys: vec![("metadata".to_owned(), "customer_id".to_owned())],
            predicates: vec![Predicate::map_key(
                "metadata",
                "customer_id",
                ZonePred::Eq(ZoneValue::Bytes(b"cust-42".to_vec())),
            )],
            ..ScanRequest::default()
        },
        (TableKindBench::Scores, "map_key_lookup") => ScanRequest {
            projection: Some(vec![1, 20]),
            map_keys: vec![("metadata".to_owned(), "customer_id".to_owned())],
            predicates: vec![Predicate::map_key(
                "metadata",
                "customer_id",
                ZonePred::Eq(ZoneValue::Bytes(b"cust-42".to_vec())),
            )],
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "substring_rare") => ScanRequest {
            projection: Some(vec![1]),
            substrings: vec![SubstringQuery::ilike("input", RARE_NEEDLE)],
            ..ScanRequest::default()
        },
        (TableKindBench::Spans, "substring_absent") => ScanRequest {
            projection: Some(vec![1]),
            substrings: vec![SubstringQuery::ilike("input", ABSENT_NEEDLE)],
            ..ScanRequest::default()
        },
        _ => ScanRequest::default(),
    };

    let mut total = 0usize;
    let mut agg: BTreeMap<(i64, String), (usize, f64, i64)> = BTreeMap::new();
    let mut topk: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();

    for r in readers {
        let batches = r.scan(&req)?;
        if qname == "time_bucket_agg" {
            for b in &batches {
                agg_statam_batch(b, &mut agg);
            }
        } else if qname == "top_k_list" {
            for b in &batches {
                topk_statam_batch(b, &mut topk);
            }
        } else {
            total += batches.iter().map(Batch::num_rows).sum::<usize>();
        }
    }

    if qname == "time_bucket_agg" {
        return Ok(agg.values().map(|v| v.0).sum());
    }
    if qname == "top_k_list" {
        return Ok(topk.len());
    }
    Ok(total)
}

enum I64ColView<'a> {
    Const(Option<i64>),
    FlatNonNull(&'a [i64]),
    FlatNull(&'a [i64], &'a cetra_column::Bitmap),
    Other(&'a Column),
}

impl<'a> I64ColView<'a> {
    fn from_col(col: &'a Column) -> Self {
        match col {
            Column::Const { value, .. } => match value {
                cetra_column::ScalarValue::I64(v) => Self::Const(Some(*v)),
                cetra_column::ScalarValue::TimestampNs { value, .. } => Self::Const(Some(*value)),
                _ => Self::Const(None),
            },
            Column::Flat(f) => {
                let vals = match f.data() {
                    FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => v.as_slice(),
                    _ => return Self::Other(col),
                };
                match f.nulls() {
                    None => Self::FlatNonNull(vals),
                    Some(bm) => Self::FlatNull(vals, bm),
                }
            }
            _ => Self::Other(col),
        }
    }

    #[inline]
    fn get_or_zero(&self, row: usize) -> i64 {
        match self {
            Self::Const(v) => v.unwrap_or(0),
            Self::FlatNonNull(v) => v[row],
            Self::FlatNull(v, bm) => {
                if bm.get(row) {
                    v[row]
                } else {
                    0
                }
            }
            Self::Other(c) => read_i64_at(c, row).unwrap_or(0),
        }
    }
}

enum F64ColView<'a> {
    Const(f64),
    FlatNonNull(&'a [f64]),
    FlatNull(&'a [f64], &'a cetra_column::Bitmap),
    Other(&'a Column),
}

impl<'a> F64ColView<'a> {
    fn from_col(col: &'a Column) -> Self {
        match col {
            Column::Const { value, .. } => match value {
                cetra_column::ScalarValue::F64(v) if !v.is_nan() => Self::Const(*v),
                _ => Self::Const(0.0),
            },
            Column::Flat(f) => {
                let FlatData::F64(vals) = f.data() else {
                    return Self::Other(col);
                };
                match f.nulls() {
                    None => Self::FlatNonNull(vals.as_slice()),
                    Some(bm) => Self::FlatNull(vals.as_slice(), bm),
                }
            }
            _ => Self::Other(col),
        }
    }

    #[inline]
    fn get_valid_or_zero(&self, row: usize) -> f64 {
        match self {
            Self::Const(v) => *v,
            Self::FlatNonNull(v) => {
                let c = v[row];
                if c.is_nan() { 0.0 } else { c }
            }
            Self::FlatNull(v, bm) => {
                if bm.get(row) {
                    let c = v[row];
                    if c.is_nan() { 0.0 } else { c }
                } else {
                    0.0
                }
            }
            Self::Other(c) => read_f64_at(c, row).filter(|v| !v.is_nan()).unwrap_or(0.0),
        }
    }
}

fn agg_statam_batch(b: &Batch, agg: &mut BTreeMap<(i64, String), (usize, f64, i64)>) {
    let ts_view = I64ColView::from_col(&b.columns()[0]);
    let model_col: &Column = &b.columns()[1];
    let tok_view = I64ColView::from_col(&b.columns()[2]);
    let cost_view = F64ColView::from_col(&b.columns()[3]);

    match model_col {
        Column::Dict(d) => {
            if let FlatData::Utf8(dict_utf8) = d.values().data() {
                let codes = d.codes();
                let nulls = d.nulls();
                let mut local: HashMap<(i64, u32), (usize, f64, i64)> = HashMap::new();
                for_each_active_row(b.selection(), b.raw_len(), |row| {
                    let bucket = ts_view.get_or_zero(row) / HOUR_NS;
                    let code = if nulls.is_some_and(|bm| !bm.get(row)) {
                        u32::MAX
                    } else {
                        codes[row]
                    };
                    let tok = tok_view.get_or_zero(row);
                    let cost = cost_view.get_valid_or_zero(row);
                    let entry = local.entry((bucket, code)).or_insert((0, 0.0, 0));
                    entry.0 += 1;
                    entry.1 += cost;
                    entry.2 += tok;
                });

                let mut resolved_names: Vec<Option<String>> = vec![None; dict_utf8.len()];
                for ((bucket, code), (cnt, cost_sum, tok_sum)) in local {
                    let model_name = if code == u32::MAX {
                        "unknown".to_owned()
                    } else {
                        let idx = code as usize;
                        if let Some(slot) = resolved_names.get_mut(idx) {
                            if let Some(cached) = slot {
                                cached.clone()
                            } else {
                                let s = dict_utf8.get(idx).unwrap_or("unknown").to_owned();
                                *slot = Some(s.clone());
                                s
                            }
                        } else {
                            "unknown".to_owned()
                        }
                    };
                    let entry = agg.entry((bucket, model_name)).or_insert((0, 0.0, 0));
                    entry.0 += cnt;
                    entry.1 += cost_sum;
                    entry.2 += tok_sum;
                }
                return;
            }
        }
        Column::Const { value, .. } => {
            let model_str = match value {
                cetra_column::ScalarValue::Utf8(s) => s.as_str(),
                _ => "unknown",
            };
            let mut local: HashMap<i64, (usize, f64, i64)> = HashMap::new();
            for_each_active_row(b.selection(), b.raw_len(), |row| {
                let bucket = ts_view.get_or_zero(row) / HOUR_NS;
                let tok = tok_view.get_or_zero(row);
                let cost = cost_view.get_valid_or_zero(row);
                let entry = local.entry(bucket).or_insert((0, 0.0, 0));
                entry.0 += 1;
                entry.1 += cost;
                entry.2 += tok;
            });
            for (bucket, (cnt, cost_sum, tok_sum)) in local {
                let entry = agg
                    .entry((bucket, model_str.to_owned()))
                    .or_insert((0, 0.0, 0));
                entry.0 += cnt;
                entry.1 += cost_sum;
                entry.2 += tok_sum;
            }
            return;
        }
        _ => {}
    }

    let mut local: HashMap<(i64, &str), (usize, f64, i64)> = HashMap::new();
    for_each_active_row(b.selection(), b.raw_len(), |row| {
        let bucket = ts_view.get_or_zero(row) / HOUR_NS;
        let model = read_str_at(model_col, row).unwrap_or("unknown");
        let tok = tok_view.get_or_zero(row);
        let cost = cost_view.get_valid_or_zero(row);
        let entry = local.entry((bucket, model)).or_insert((0, 0.0, 0));
        entry.0 += 1;
        entry.1 += cost;
        entry.2 += tok;
    });
    for ((bucket, model), (cnt, cost_sum, tok_sum)) in local {
        let entry = agg.entry((bucket, model.to_owned())).or_insert((0, 0.0, 0));
        entry.0 += cnt;
        entry.1 += cost_sum;
        entry.2 += tok_sum;
    }
}

fn topk_statam_batch(b: &Batch, topk: &mut BinaryHeap<Reverse<(i64, usize)>>) {
    let ts_col = &b.columns()[3];
    for_each_active_row(b.selection(), b.raw_len(), |row| {
        if let Some(ts) = read_i64_at(ts_col, row) {
            push_topk(topk, ts, row);
        }
    });
}

fn for_each_active_row<F: FnMut(usize)>(sel: &Selection, raw_len: usize, mut f: F) {
    match sel {
        Selection::All => {
            for r in 0..raw_len {
                f(r);
            }
        }
        Selection::Mask(bm) => {
            for r in bm.iter_ones() {
                f(r);
            }
        }
        Selection::Indices(idx) => {
            for &r in idx {
                f(r as usize);
            }
        }
    }
}

fn read_i64_at(col: &Column, row: usize) -> Option<i64> {
    match col {
        Column::Const { value, .. } => match value {
            cetra_column::ScalarValue::I64(v) => Some(*v),
            cetra_column::ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        },
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::I64(v) => Some(v[row]),
                FlatData::TimestampNs { values, .. } => Some(values[row]),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = d.codes()[row] as usize;
            match d.values().data() {
                FlatData::I64(v) => Some(v[code]),
                FlatData::TimestampNs { values, .. } => Some(values[code]),
                _ => None,
            }
        }
        Column::Rle(r) => {
            let run_idx = r.run_ends().partition_point(|&end| (end as usize) <= row);
            if r.values().is_null(run_idx) {
                return None;
            }
            match r.values().data() {
                FlatData::I64(v) => Some(v[run_idx]),
                FlatData::TimestampNs { values, .. } => Some(values[run_idx]),
                _ => None,
            }
        }
    }
}

fn read_f64_at(col: &Column, row: usize) -> Option<f64> {
    match col {
        Column::Const { value, .. } => match value {
            cetra_column::ScalarValue::F64(v) => Some(*v),
            _ => None,
        },
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::F64(v) => Some(v[row]),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = d.codes()[row] as usize;
            match d.values().data() {
                FlatData::F64(v) => Some(v[code]),
                _ => None,
            }
        }
        Column::Rle(r) => {
            let run_idx = r.run_ends().partition_point(|&end| (end as usize) <= row);
            if r.values().is_null(run_idx) {
                return None;
            }
            match r.values().data() {
                FlatData::F64(v) => Some(v[run_idx]),
                _ => None,
            }
        }
    }
}

fn read_str_at<'a>(col: &'a Column, row: usize) -> Option<&'a str> {
    match col {
        Column::Const { value, .. } => match value {
            cetra_column::ScalarValue::Utf8(s) => Some(s.as_str()),
            _ => None,
        },
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::Utf8(u) => u.get(row),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = d.codes()[row] as usize;
            match d.values().data() {
                FlatData::Utf8(u) => u.get(code),
                _ => None,
            }
        }
        Column::Rle(r) => {
            let run_idx = r.run_ends().partition_point(|&end| (end as usize) <= row);
            if r.values().is_null(run_idx) {
                return None;
            }
            match r.values().data() {
                FlatData::Utf8(u) => u.get(run_idx),
                _ => None,
            }
        }
    }
}
