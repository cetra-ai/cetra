use std::fmt;
use std::io::Cursor;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use arrow::array::RecordBatch as RecordBatch59;
use arrow::compute::concat_batches;
use arrow_array_58::{RecordBatch as RecordBatch58, UInt32Array as UInt32Array58};
use bytes::Bytes;
use cetra_statam::{RangeReader, SimulatedS3Config, SimulatedS3Reader};
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

use crate::schema::BenchResult;

fn test_cache() -> LanceCache {
    LanceCache::no_cache()
}

/// Minimal `object_store 0.14` adapter backed by [`SimulatedS3Reader`] so Lance reads go through
/// the exact same 1 MiB `ReadCache` and S3 cost model as Parquet, Vortex, Strata, and Statam.
pub struct SimulatedS3ObjectStore14 {
    path: ObjectPath14,
    reader: Arc<SimulatedS3Reader>,
}

impl SimulatedS3ObjectStore14 {
    pub fn new(path: ObjectPath14, reader: Arc<SimulatedS3Reader>) -> Self {
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
        let bytes =
            self.reader
                .read_range(range.clone())
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
            .read_ranges(ranges)
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

/// Opens a Lance V2.2 reader backed by [`SimulatedS3Reader`] with cold cache & zeroed counters.
pub async fn open_lance_s3(
    bytes: &Bytes,
    s3_cfg: SimulatedS3Config,
) -> BenchResult<(FileReader, Arc<SimulatedS3Reader>)> {
    let s3_reader = Arc::new(SimulatedS3Reader::from_bytes(bytes.clone(), s3_cfg));
    s3_reader.evict_all();
    s3_reader.reset_counters();

    let path = ObjectPath14::from("bench.lance");
    let mut store = LanceObjectStore::memory();
    store.inner = Arc::new(SimulatedS3ObjectStore14::new(
        path.clone(),
        Arc::clone(&s3_reader),
    ));
    let scheduler = ScanScheduler::new(Arc::new(store), SchedulerConfig::default_for_testing());
    let file_scheduler = scheduler
        .open_file(&path, &CachedFileSize::unknown())
        .await?;
    let reader = FileReader::try_open(
        file_scheduler,
        None,
        Arc::<DecoderPlugins>::default(),
        &test_cache(),
        FileReaderOptions::default(),
    )
    .await?;
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
