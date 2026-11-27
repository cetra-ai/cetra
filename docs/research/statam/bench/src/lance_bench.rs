use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::fmt;
use std::io::Cursor;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use arrow::array::RecordBatch as RecordBatch59;
use arrow::compute::concat_batches;
use arrow_array_58::{
    Array as Array58, FixedSizeBinaryArray as FixedSizeBinaryArray58,
    Float64Array as Float64Array58, Int64Array as Int64Array58, MapArray as MapArray58,
    RecordBatch as RecordBatch58, StringArray as StringArray58,
    TimestampNanosecondArray as TimestampNanosecondArray58, UInt32Array as UInt32Array58,
};
use bytes::Bytes;
use cetra_statam::{RangeReader, SimulatedS3Config};
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use lance_core::cache::LanceCache;
use lance_core::datatypes::Schema as LanceSchema;
use lance_encoding::decoder::{DecoderPlugins, FilterExpression};
use lance_file::reader::{FileReader, FileReaderOptions};
use lance_file::version::ConcreteFileVersion;
use lance_file::versions::{create_writer, reader_projection_from_column_names};
use lance_file::writer::FileWriterOptions;
use lance_io::ReadBatchParams;
use lance_io::object_store::ObjectStore as LanceObjectStore;
use lance_io::scheduler::{ScanScheduler, SchedulerConfig};
use lance_io::utils::CachedFileSize;
use object_store_14::path::Path as ObjectPath14;
use object_store_14::{
    Attributes, CopyOptions, Extensions, GetOptions, GetResult, GetResultPayload, ListResult,
    MultipartUpload, ObjectMeta, ObjectStore as ObjectStore14, ObjectStoreExt as ObjectStoreExt14,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

use crate::queries::{contains_ascii_ci, push_topk};
use crate::real_s3::{BenchS3Reader, run_s3_adaptive};
use crate::schema::{BenchResult, TableKindBench};
use crate::workloads::{ABSENT_NEEDLE, DAY_START, HOUR_NS, RARE_NEEDLE};

fn test_cache() -> LanceCache {
    LanceCache::no_cache()
}

/// Minimal `object_store 0.14` adapter backed by [`BenchS3Reader`] so Lance reads go through
/// the exact same S3 backend (simulated or real S3) as Parquet, Vortex, Strata, and Statam.
pub struct SimulatedS3ObjectStore14 {
    path: ObjectPath14,
    reader: Arc<BenchS3Reader>,
}

impl SimulatedS3ObjectStore14 {
    pub fn new(path: ObjectPath14, reader: Arc<BenchS3Reader>) -> Self {
        Self { path, reader }
    }
}

impl fmt::Debug for SimulatedS3ObjectStore14 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimulatedS3ObjectStore14")
            .field("file_len", &self.reader.file_len())
            .finish()
    }
}

impl fmt::Display for SimulatedS3ObjectStore14 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SimulatedS3ObjectStore14({} bytes)",
            self.reader.file_len()
        )
    }
}

fn not_implemented_14(operation: &str) -> object_store_14::Error {
    object_store_14::Error::NotImplemented {
        operation: operation.to_owned(),
        implementer: "SimulatedS3ObjectStore14".to_owned(),
    }
}

#[async_trait::async_trait]
impl ObjectStore14 for SimulatedS3ObjectStore14 {
    async fn put_opts(
        &self,
        _location: &ObjectPath14,
        _payload: PutPayload,
        _opts: PutOptions,
    ) -> object_store_14::Result<PutResult> {
        Err(not_implemented_14("put_opts"))
    }

    async fn put_multipart_opts(
        &self,
        _location: &ObjectPath14,
        _opts: PutMultipartOptions,
    ) -> object_store_14::Result<Box<dyn MultipartUpload>> {
        Err(not_implemented_14("put_multipart_opts"))
    }

    async fn get_opts(
        &self,
        location: &ObjectPath14,
        options: GetOptions,
    ) -> object_store_14::Result<GetResult> {
        if location != &self.path {
            return Err(object_store_14::Error::NotFound {
                path: location.to_string(),
                source: "object not found in SimulatedS3ObjectStore14".into(),
            });
        }
        let size = self.reader.file_len();
        let meta = ObjectMeta {
            location: location.clone(),
            last_modified: SystemTime::UNIX_EPOCH.into(),
            size,
            e_tag: None,
            version: None,
        };
        if options.head {
            return Ok(GetResult {
                payload: GetResultPayload::Stream(futures::stream::empty().boxed()),
                meta,
                range: 0..0,
                attributes: Attributes::default(),
                extensions: Extensions::default(),
            });
        }
        let range = match options.range {
            Some(gr) => gr
                .as_range(size)
                .map_err(|e| object_store_14::Error::Generic {
                    store: "SimulatedS3ObjectStore14",
                    source: Box::new(e),
                })?,
            None => 0..size,
        };
        let bytes = self
            .reader
            .read_range_async(range.clone())
            .await
            .map_err(|e| object_store_14::Error::Generic {
                store: "SimulatedS3ObjectStore14",
                source: Box::new(e),
            })?;
        Ok(GetResult {
            payload: GetResultPayload::Stream(
                futures::stream::once(futures::future::ok(bytes)).boxed(),
            ),
            meta,
            range,
            attributes: Attributes::default(),
            extensions: Extensions::default(),
        })
    }

    async fn get_ranges(
        &self,
        location: &ObjectPath14,
        ranges: &[Range<u64>],
    ) -> object_store_14::Result<Vec<Bytes>> {
        if location != &self.path {
            return Err(object_store_14::Error::NotFound {
                path: location.to_string(),
                source: "object not found in SimulatedS3ObjectStore14".into(),
            });
        }
        self.reader
            .read_ranges_async(ranges)
            .await
            .map_err(|e| object_store_14::Error::Generic {
                store: "SimulatedS3ObjectStore14",
                source: Box::new(e),
            })
    }

    fn delete_stream(
        &self,
        _locations: BoxStream<'static, object_store_14::Result<ObjectPath14>>,
    ) -> BoxStream<'static, object_store_14::Result<ObjectPath14>> {
        futures::stream::once(futures::future::err(not_implemented_14("delete_stream"))).boxed()
    }

    fn list(
        &self,
        _prefix: Option<&ObjectPath14>,
    ) -> BoxStream<'static, object_store_14::Result<ObjectMeta>> {
        futures::stream::once(futures::future::err(not_implemented_14("list"))).boxed()
    }

    async fn list_with_delimiter(
        &self,
        _prefix: Option<&ObjectPath14>,
    ) -> object_store_14::Result<ListResult> {
        Err(not_implemented_14("list_with_delimiter"))
    }

    async fn copy_opts(
        &self,
        _from: &ObjectPath14,
        _to: &ObjectPath14,
        _options: CopyOptions,
    ) -> object_store_14::Result<()> {
        Err(not_implemented_14("copy_opts"))
    }
}

/// Converts an Arrow 59 `RecordBatch` to an Arrow 58 `RecordBatch` via IPC stream (outside timed loops).
pub fn arrow59_to_arrow58(batch59: &RecordBatch59) -> BenchResult<RecordBatch58> {
    let mut ipc_buf = Vec::new();
    {
        let mut writer =
            arrow::ipc::writer::StreamWriter::try_new(&mut ipc_buf, batch59.schema().as_ref())?;
        writer.write(batch59)?;
        writer.finish()?;
    }
    let reader = arrow_ipc_58::reader::StreamReader::try_new(Cursor::new(ipc_buf), None)?;
    let batches: Vec<RecordBatch58> = reader.collect::<Result<_, _>>()?;
    if batches.len() == 1 {
        Ok(batches.into_iter().next().ok_or("missing arrow-58 batch")?)
    } else if batches.is_empty() {
        let mut schema_buf = Vec::new();
        {
            let mut writer = arrow::ipc::writer::StreamWriter::try_new(
                &mut schema_buf,
                batch59.schema().as_ref(),
            )?;
            writer.finish()?;
        }
        let empty_reader =
            arrow_ipc_58::reader::StreamReader::try_new(Cursor::new(schema_buf), None)?;
        Ok(RecordBatch58::new_empty(empty_reader.schema()))
    } else {
        Err(format!(
            "arrow59_to_arrow58 expected 1 IPC batch, got {}",
            batches.len()
        )
        .into())
    }
}

/// Converts an Arrow 58 `RecordBatch` to an Arrow 59 `RecordBatch` via IPC stream.
pub fn arrow58_to_arrow59(batch58: &RecordBatch58) -> BenchResult<RecordBatch59> {
    let mut ipc_buf = Vec::new();
    {
        let mut writer =
            arrow_ipc_58::writer::StreamWriter::try_new(&mut ipc_buf, batch58.schema().as_ref())?;
        writer.write(batch58)?;
        writer.finish()?;
    }
    let reader = arrow::ipc::reader::StreamReader::try_new(Cursor::new(ipc_buf), None)?;
    let schema59 = reader.schema();
    let batches: Vec<RecordBatch59> = reader.collect::<Result<_, _>>()?;
    if batches.len() == 1 {
        Ok(batches.into_iter().next().ok_or("missing arrow-59 batch")?)
    } else if batches.is_empty() {
        Ok(RecordBatch59::new_empty(schema59))
    } else {
        Ok(concat_batches(&schema59, &batches)?)
    }
}

/// Prepares Arrow 58 batches outside the timed Lance write loop.
pub fn prepare_lance_batches(batches59: &[RecordBatch59]) -> BenchResult<Vec<RecordBatch58>> {
    let mut prepared = Vec::with_capacity(batches59.len());
    for b in batches59 {
        prepared.push(arrow59_to_arrow58(b)?);
    }
    Ok(prepared)
}

/// Writes prepared Arrow 58 batches into a Lance V2.2 file in memory and returns `(bytes, elapsed_ms)`.
pub async fn write_lance_bytes(batches58: &[RecordBatch58]) -> BenchResult<(Bytes, f64)> {
    let first = batches58
        .first()
        .ok_or("write_lance_bytes requires at least one batch")?;
    let lance_schema = LanceSchema::try_from(first.schema().as_ref())?;

    let t0 = Instant::now();
    let store = LanceObjectStore::memory();
    let path = ObjectPath14::from("bench.lance");
    let object_writer = store.create(&path).await?;
    let mut writer = create_writer(
        ConcreteFileVersion::V2_2,
        object_writer,
        lance_schema,
        FileWriterOptions::default(),
    )?;
    for batch in batches58 {
        writer.write_batch(batch).await?;
    }
    writer.finish().await?;
    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let bytes = store.inner.get(&path).await?.bytes().await?;
    Ok((bytes, elapsed_ms))
}

/// Opens an in-memory Lance V2.2 reader for local warm CPU measurements.
pub async fn open_lance_local(bytes: &Bytes) -> BenchResult<FileReader> {
    let store = LanceObjectStore::memory();
    let path = ObjectPath14::from("bench.lance");
    store
        .inner
        .put(&path, PutPayload::from_bytes(bytes.clone()))
        .await?;
    let scheduler = ScanScheduler::new(Arc::new(store), SchedulerConfig::default_for_testing());
    let file_scheduler = scheduler
        .open_file(&path, &CachedFileSize::new(bytes.len() as u64))
        .await?;
    let reader = FileReader::try_open(
        file_scheduler,
        None,
        Arc::<DecoderPlugins>::default(),
        &test_cache(),
        FileReaderOptions::default(),
    )
    .await?;
    Ok(reader)
}

/// Opens a Lance V2.2 reader backed by an existing [`BenchS3Reader`].
/// Passes known file size to avoid an unfair extra HEAD round-trip, and uses
/// `SchedulerConfig::max_bandwidth` on real S3.
pub async fn open_lance_from_bench_s3(s3_reader: Arc<BenchS3Reader>) -> BenchResult<FileReader> {
    let path = ObjectPath14::from("bench.lance");
    let file_len = s3_reader.file_len();
    let is_real = s3_reader.is_real_s3();
    let mut store = LanceObjectStore::memory();
    store.inner = Arc::new(SimulatedS3ObjectStore14::new(path.clone(), s3_reader));
    let store = Arc::new(store);
    let sched_cfg = if is_real {
        SchedulerConfig::max_bandwidth(&store)
    } else {
        SchedulerConfig::default_for_testing()
    };
    let scheduler = ScanScheduler::new(store, sched_cfg);
    let file_scheduler = scheduler
        .open_file(&path, &CachedFileSize::new(file_len))
        .await?;
    let reader = FileReader::try_open(
        file_scheduler,
        None,
        Arc::<DecoderPlugins>::default(),
        &test_cache(),
        FileReaderOptions::default(),
    )
    .await?;
    Ok(reader)
}

/// Opens a Lance V2.2 reader backed by [`BenchS3Reader`] with cold cache & zeroed counters.
pub async fn open_lance_s3(
    bytes: &Bytes,
    s3_cfg: SimulatedS3Config,
) -> BenchResult<(FileReader, Arc<BenchS3Reader>)> {
    let s3_reader = BenchS3Reader::from_bytes(bytes.clone(), s3_cfg);
    s3_reader.evict_all();
    s3_reader.reset_counters();
    let reader = open_lance_from_bench_s3(Arc::clone(&s3_reader)).await?;
    Ok((reader, s3_reader))
}

async fn lance_read_params(
    reader: &FileReader,
    params: ReadBatchParams,
    proj_cols: Option<&[&str]>,
) -> BenchResult<Vec<RecordBatch59>> {
    let batches58: Vec<RecordBatch58> = if let Some(cols) = proj_cols {
        let projection = reader_projection_from_column_names(
            reader.metadata().version(),
            reader.schema(),
            cols,
        )?;
        let stream = reader
            .read_stream_projected(params, 16_384, 1, projection, FilterExpression::no_filter())
            .await?;
        stream.try_collect().await?
    } else {
        let stream = reader
            .read_stream(params, 16_384, 1, FilterExpression::no_filter())
            .await?;
        stream.try_collect().await?
    };

    let mut out = Vec::with_capacity(batches58.len());
    for b58 in &batches58 {
        if b58.num_rows() > 0 {
            out.push(arrow58_to_arrow59(b58)?);
        }
    }
    Ok(out)
}

/// Scans all rows of a Lance file with optional column projection.
pub async fn lance_scan_projected(
    reader: &FileReader,
    proj_cols: Option<&[&str]>,
) -> BenchResult<Vec<RecordBatch59>> {
    lance_read_params(reader, ReadBatchParams::RangeFull, proj_cols).await
}

/// Scans all rows of a Lance file with optional column projection.
pub async fn lance_scan_all(
    reader: &FileReader,
    proj_cols: Option<&[&str]>,
) -> BenchResult<Vec<RecordBatch59>> {
    lance_scan_projected(reader, proj_cols).await
}

/// Random-access take of specific row indices from a Lance file with optional column projection.
pub async fn lance_take_indices(
    reader: &FileReader,
    indices: &[u32],
    proj_cols: Option<&[&str]>,
) -> BenchResult<Vec<RecordBatch59>> {
    lance_read_params(
        reader,
        ReadBatchParams::Indices(UInt32Array58::from(indices.to_vec())),
        proj_cols,
    )
    .await
}

async fn lance_read_params_58(
    reader: &FileReader,
    params: ReadBatchParams,
    proj_cols: Option<&[&str]>,
) -> BenchResult<Vec<RecordBatch58>> {
    let batches58: Vec<RecordBatch58> = if let Some(cols) = proj_cols {
        let projection = reader_projection_from_column_names(
            reader.metadata().version(),
            reader.schema(),
            cols,
        )?;
        let stream = reader
            .read_stream_projected(params, 16_384, 1, projection, FilterExpression::no_filter())
            .await?;
        stream.try_collect().await?
    } else {
        let stream = reader
            .read_stream(params, 16_384, 1, FilterExpression::no_filter())
            .await?;
        stream.try_collect().await?
    };
    Ok(batches58)
}

/// Writes one Lance V2.2 file per batch (Arrow 59 -> 58 conversion done outside the timer).
pub async fn write_lance_files(batches59: &[RecordBatch59]) -> BenchResult<(Vec<Bytes>, f64)> {
    let prepared = prepare_lance_batches(batches59)?;
    let mut out = Vec::with_capacity(prepared.len());
    let mut total_ms = 0.0f64;
    for b58 in &prepared {
        let (bytes, ms) = write_lance_bytes(std::slice::from_ref(b58)).await?;
        out.push(bytes);
        total_ms += ms;
    }
    Ok((out, total_ms))
}

/// Opens multiple in-memory Lance V2.2 files for local warm CPU measurements.
pub async fn open_lance_local_files(files: &[Bytes]) -> BenchResult<Vec<FileReader>> {
    let mut opened = Vec::with_capacity(files.len());
    for b in files {
        opened.push(open_lance_local(b).await?);
    }
    Ok(opened)
}

/// Runs a `W1`–`W4` benchmark query over Lance V2.2 files backed by [`BenchS3Reader`].
pub async fn run_lance_query_s3(
    files: &[Bytes],
    s3_cfg: SimulatedS3Config,
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<(usize, u64, u64, f64)> {
    let s3_readers: Vec<Arc<BenchS3Reader>> = files
        .iter()
        .map(|b| BenchS3Reader::from_bytes(b.clone(), s3_cfg))
        .collect();

    run_s3_adaptive(|| {
        let s3_readers = s3_readers.clone();
        async move {
            let mut readers = Vec::with_capacity(s3_readers.len());
            for s3 in &s3_readers {
                s3.evict_all();
                s3.reset_counters();
                readers.push(open_lance_from_bench_s3(Arc::clone(s3)).await?);
            }
            let matched = run_lance_query(&readers, table, qname, sample_tid).await?;
            let gets: u64 = s3_readers.iter().map(|r| r.counters().gets).sum();
            let bytes: u64 = s3_readers.iter().map(|r| r.counters().bytes_from_s3).sum();
            let s3_ms: f64 = s3_readers.iter().map(|r| r.simulated_latency_ms()).sum();
            Ok((matched, gets, bytes, s3_ms))
        }
    })
    .await
}

/// Evaluates a `W1`–`W4` benchmark query natively on Arrow 58 `RecordBatch`es from Lance V2.2
/// (zero IPC conversion overhead, using two-phase `take` for point lookups & `top_k_list`).
pub async fn run_lance_query(
    files: &[FileReader],
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<usize> {
    let (t_lo, t_hi) = (DAY_START + 8 * HOUR_NS, DAY_START + 14 * HOUR_NS);
    let (top_lo, top_hi) = (DAY_START + 6 * HOUR_NS, DAY_START + 18 * HOUR_NS);

    if qname == "cold_open_prune" {
        let absent = [0xAAu8; 16];
        let mut matched = 0usize;
        for f in files {
            let batches =
                lance_read_params_58(f, ReadBatchParams::RangeFull, Some(&["trace_id"])).await?;
            for b in &batches {
                if let Some(arr) = b
                    .column_by_name("trace_id")
                    .and_then(|c| c.as_any().downcast_ref::<FixedSizeBinaryArray58>())
                {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) && arr.value(i) == absent.as_slice() {
                            matched += 1;
                        }
                    }
                }
            }
        }
        return Ok(matched);
    }

    let mut total = 0usize;
    let mut agg: BTreeMap<(i64, String), (usize, f64, i64)> = BTreeMap::new();
    let mut topk: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();

    for f in files {
        match (table, qname) {
            (_, "point_lookup_narrow" | "point_lookup_full") => {
                // Phase 1: scan trace_id only, then use Lance's O(1) random-access take.
                let id_batches =
                    lance_read_params_58(f, ReadBatchParams::RangeFull, Some(&["trace_id"]))
                        .await?;
                let mut indices = Vec::new();
                let mut row_base = 0u32;
                for b in &id_batches {
                    if let Some(arr) = b
                        .column_by_name("trace_id")
                        .and_then(|c| c.as_any().downcast_ref::<FixedSizeBinaryArray58>())
                    {
                        for i in 0..arr.len() {
                            if !arr.is_null(i) && arr.value(i) == sample_tid.as_slice() {
                                indices.push(row_base + i as u32);
                            }
                        }
                    }
                    row_base += b.num_rows() as u32;
                }
                if !indices.is_empty() {
                    let proj: Option<&[&str]> = match (table, qname) {
                        (TableKindBench::Spans, "point_lookup_narrow") => Some(&[
                            "trace_id",
                            "span_id",
                            "parent_span_id",
                            "name",
                            "start_time",
                            "end_time",
                            "status",
                            "model",
                            "total_tokens",
                            "cost_total",
                        ]),
                        (TableKindBench::Scores, "point_lookup_narrow") => Some(&[
                            "score_id",
                            "trace_id",
                            "name",
                            "value_num",
                            "value_str",
                            "created_at",
                        ]),
                        _ => None,
                    };
                    let taken = lance_read_params_58(
                        f,
                        ReadBatchParams::Indices(UInt32Array58::from(indices)),
                        proj,
                    )
                    .await?;
                    total += taken.iter().map(RecordBatch58::num_rows).sum::<usize>();
                }
            }
            (TableKindBench::Spans, "filtered_count") => {
                let batches = lance_read_params_58(
                    f,
                    ReadBatchParams::RangeFull,
                    Some(&["start_time", "status"]),
                )
                .await?;
                for b in &batches {
                    let ts = b
                        .column_by_name("start_time")
                        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray58>());
                    let st = b
                        .column_by_name("status")
                        .and_then(|c| c.as_any().downcast_ref::<StringArray58>());
                    if let (Some(ts), Some(st)) = (ts, st) {
                        for i in 0..b.num_rows() {
                            let t = ts.value(i);
                            if t >= t_lo && t <= t_hi && st.value(i) == "error" {
                                total += 1;
                            }
                        }
                    }
                }
            }
            (TableKindBench::Scores, "filtered_count") => {
                let batches = lance_read_params_58(
                    f,
                    ReadBatchParams::RangeFull,
                    Some(&["created_at", "name"]),
                )
                .await?;
                for b in &batches {
                    let ts = b
                        .column_by_name("created_at")
                        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray58>());
                    let nm = b
                        .column_by_name("name")
                        .and_then(|c| c.as_any().downcast_ref::<StringArray58>());
                    if let (Some(ts), Some(nm)) = (ts, nm) {
                        for i in 0..b.num_rows() {
                            let t = ts.value(i);
                            if t >= t_lo && t <= t_hi && nm.value(i) == "faithfulness" {
                                total += 1;
                            }
                        }
                    }
                }
            }
            (TableKindBench::Spans, "time_bucket_agg") => {
                let batches = lance_read_params_58(
                    f,
                    ReadBatchParams::RangeFull,
                    Some(&[
                        "span_kind",
                        "start_time",
                        "model",
                        "total_tokens",
                        "cost_total",
                    ]),
                )
                .await?;
                for b in &batches {
                    let sk = b
                        .column_by_name("span_kind")
                        .and_then(|c| c.as_any().downcast_ref::<StringArray58>());
                    let ts = b
                        .column_by_name("start_time")
                        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray58>());
                    let md = b
                        .column_by_name("model")
                        .and_then(|c| c.as_any().downcast_ref::<StringArray58>());
                    let tok = b
                        .column_by_name("total_tokens")
                        .and_then(|c| c.as_any().downcast_ref::<Int64Array58>());
                    let cost = b
                        .column_by_name("cost_total")
                        .and_then(|c| c.as_any().downcast_ref::<Float64Array58>());
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
                            let entry =
                                agg.entry((bucket, model.to_owned())).or_insert((0, 0.0, 0));
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
            }
            (TableKindBench::Spans, "top_k_list") => {
                // Phase 1: scan start_time & span_kind, find top-50 row indices, Phase 2: take 10 cols.
                let filter_batches = lance_read_params_58(
                    f,
                    ReadBatchParams::RangeFull,
                    Some(&["start_time", "span_kind"]),
                )
                .await?;
                let mut file_topk: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();
                let mut row_base = 0usize;
                for b in &filter_batches {
                    let ts = b
                        .column_by_name("start_time")
                        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray58>());
                    let sk = b
                        .column_by_name("span_kind")
                        .and_then(|c| c.as_any().downcast_ref::<StringArray58>());
                    if let (Some(ts), Some(sk)) = (ts, sk) {
                        for i in 0..b.num_rows() {
                            let t = ts.value(i);
                            if sk.value(i) == "llm" && t >= top_lo && t <= top_hi {
                                push_topk(&mut file_topk, t, row_base + i);
                                push_topk(&mut topk, t, row_base + i);
                            }
                        }
                    }
                    row_base += b.num_rows();
                }
                if !file_topk.is_empty() {
                    let mut idxs: Vec<u32> = file_topk
                        .into_iter()
                        .map(|Reverse((_, r))| r as u32)
                        .collect();
                    idxs.sort_unstable();
                    let _ = lance_read_params_58(
                        f,
                        ReadBatchParams::Indices(UInt32Array58::from(idxs)),
                        Some(&[
                            "trace_id",
                            "span_id",
                            "name",
                            "start_time",
                            "duration_ns",
                            "status",
                            "model",
                            "total_tokens",
                            "cost_total",
                            "input_preview",
                        ]),
                    )
                    .await?;
                }
            }
            (_, "map_key_lookup") => {
                let cols: &[&str] = match table {
                    TableKindBench::Spans => &["trace_id", "start_time", "metadata"],
                    TableKindBench::Scores => &["score_id", "metadata", "created_at"],
                };
                let batches =
                    lance_read_params_58(f, ReadBatchParams::RangeFull, Some(cols)).await?;
                for b in &batches {
                    if let Some(ma) = b
                        .column_by_name("metadata")
                        .and_then(|c| c.as_any().downcast_ref::<MapArray58>())
                    {
                        let keys = ma.keys().as_any().downcast_ref::<StringArray58>();
                        let vals = ma.values().as_any().downcast_ref::<StringArray58>();
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
                                        total += 1;
                                        break;
                                    }
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
                let batches = lance_read_params_58(
                    f,
                    ReadBatchParams::RangeFull,
                    Some(&["trace_id", "input"]),
                )
                .await?;
                for b in &batches {
                    if let Some(inp) = b
                        .column_by_name("input")
                        .and_then(|c| c.as_any().downcast_ref::<StringArray58>())
                    {
                        for i in 0..inp.len() {
                            if !inp.is_null(i) && contains_ascii_ci(inp.value(i).as_bytes(), needle)
                            {
                                total += 1;
                            }
                        }
                    }
                }
            }
            _ => {}
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
