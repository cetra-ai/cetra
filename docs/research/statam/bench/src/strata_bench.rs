use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use arrow::array::RecordBatch;
use bytes::Bytes;
use cetra_statam::{RangeReader, SimulatedS3Config, SimulatedS3Reader, SliceReader};
use futures::stream::{BoxStream, StreamExt};
use object_store::path::Path as ObjectPath;
use object_store::{
    Attributes, CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload,
    ObjectMeta, ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use strata::{
    CmpOp, Config as StrataConfig, Lit, Predicate, ScanKind, ScanRequest, StrataFile, StrataWriter,
};

use crate::queries::agg_pq_filtered_batch;
use crate::schema::{BenchResult, TableKindBench, arrow_spans_schema};
use crate::workloads::{ABSENT_NEEDLE, DAY_START, HOUR_NS, RARE_NEEDLE};

pub struct RangeObjectStore<R: RangeReader + 'static> {
    files: HashMap<ObjectPath, Arc<R>>,
}

pub type SliceObjectStore = RangeObjectStore<SliceReader>;
pub type SimS3ObjectStore = RangeObjectStore<SimulatedS3Reader>;

impl<R: RangeReader + 'static> RangeObjectStore<R> {
    pub fn new(files: HashMap<ObjectPath, Arc<R>>) -> Self {
        Self { files }
    }
}

impl<R: RangeReader + 'static> fmt::Debug for RangeObjectStore<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RangeObjectStore")
            .field("num_files", &self.files.len())
            .finish()
    }
}

impl<R: RangeReader + 'static> fmt::Display for RangeObjectStore<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RangeObjectStore({} files)", self.files.len())
    }
}

fn not_implemented(operation: &str) -> object_store::Error {
    object_store::Error::NotImplemented {
        operation: operation.to_owned(),
        implementer: "RangeObjectStore".to_owned(),
    }
}

#[async_trait::async_trait]
impl<R: RangeReader + 'static> ObjectStore for RangeObjectStore<R> {
    async fn put_opts(
        &self,
        _location: &ObjectPath,
        _payload: PutPayload,
        _opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        Err(not_implemented("put_opts"))
    }

    async fn put_multipart_opts(
        &self,
        _location: &ObjectPath,
        _opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Err(not_implemented("put_multipart_opts"))
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let reader = self
            .files
            .get(location)
            .ok_or_else(|| object_store::Error::NotFound {
                path: location.to_string(),
                source: "object not found in RangeObjectStore".into(),
            })?;
        let size = reader.file_len();
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
            });
        }
        let range = match options.range {
            Some(gr) => gr
                .as_range(size)
                .map_err(|e| object_store::Error::Generic {
                    store: "RangeObjectStore",
                    source: Box::new(e),
                })?,
            None => 0..size,
        };
        let bytes = reader
            .read_range(range.clone())
            .map_err(|e| object_store::Error::Generic {
                store: "RangeObjectStore",
                source: Box::new(e),
            })?;
        Ok(GetResult {
            payload: GetResultPayload::Stream(
                futures::stream::once(futures::future::ok(bytes)).boxed(),
            ),
            meta,
            range,
            attributes: Attributes::default(),
        })
    }

    async fn get_ranges(
        &self,
        location: &ObjectPath,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        let reader = self
            .files
            .get(location)
            .ok_or_else(|| object_store::Error::NotFound {
                path: location.to_string(),
                source: "object not found in RangeObjectStore".into(),
            })?;
        reader
            .read_ranges(ranges)
            .map_err(|e| object_store::Error::Generic {
                store: "RangeObjectStore",
                source: Box::new(e),
            })
    }

    fn delete_stream(
        &self,
        _locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        futures::stream::once(futures::future::err(not_implemented("delete_stream"))).boxed()
    }

    fn list(
        &self,
        _prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        futures::stream::once(futures::future::err(not_implemented("list"))).boxed()
    }

    async fn list_with_delimiter(
        &self,
        _prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        Err(not_implemented("list_with_delimiter"))
    }

    async fn copy_opts(
        &self,
        _from: &ObjectPath,
        _to: &ObjectPath,
        _options: CopyOptions,
    ) -> object_store::Result<()> {
        Err(not_implemented("copy_opts"))
    }
}

pub fn write_strata_files(
    table: TableKindBench,
    batches: &[RecordBatch],
) -> BenchResult<(Vec<Bytes>, f64)> {
    let sort_key: &[&str] = match table {
        TableKindBench::Spans => &["trace_id", "start_time"],
        TableKindBench::Scores => &["created_at"],
    };
    let mut cfg = StrataConfig::default();
    if table == TableKindBench::Scores {
        cfg.key_filter_columns.push("trace_id".to_owned());
    }
    let t0 = Instant::now();
    let mut out = Vec::with_capacity(batches.len());
    for batch in batches {
        let mut w = StrataWriter::new(batch.schema(), sort_key, cfg.clone())?;
        w.write(batch)?;
        let (buf, _summary) = w.finish()?;
        out.push(Bytes::from(buf));
    }
    Ok((out, t0.elapsed().as_secs_f64() * 1000.0))
}

fn object_paths(count: usize) -> BenchResult<Vec<ObjectPath>> {
    let mut paths = Vec::with_capacity(count);
    for i in 0..count {
        paths.push(ObjectPath::from(format!("part_{i:03}.strata")));
    }
    Ok(paths)
}

pub async fn open_strata_local(files: &[Bytes]) -> BenchResult<Vec<Arc<StrataFile>>> {
    let paths = object_paths(files.len())?;
    let mut map = HashMap::with_capacity(files.len());
    for (p, b) in paths.iter().zip(files) {
        map.insert(p.clone(), Arc::new(SliceReader::new(b.clone())));
    }
    let store: Arc<dyn ObjectStore> = Arc::new(SliceObjectStore::new(map));
    let cfg = StrataConfig::default();
    let mut opened = Vec::with_capacity(paths.len());
    for p in paths {
        let f = StrataFile::open(Arc::clone(&store), p, None, &cfg).await?;
        opened.push(Arc::new(f));
    }
    Ok(opened)
}

pub async fn run_strata_query_s3(
    files: &[Bytes],
    s3_cfg: SimulatedS3Config,
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<(usize, u64, u64, f64)> {
    let paths = object_paths(files.len())?;
    let s3_readers: Vec<Arc<SimulatedS3Reader>> = files
        .iter()
        .map(|b| Arc::new(SimulatedS3Reader::from_bytes(b.clone(), s3_cfg)))
        .collect();
    let mut map = HashMap::with_capacity(files.len());
    for (p, r) in paths.iter().zip(&s3_readers) {
        r.evict_all();
        r.reset_counters();
        map.insert(p.clone(), Arc::clone(r));
    }
    let store: Arc<dyn ObjectStore> = Arc::new(SimS3ObjectStore::new(map));
    let mut cfg = StrataConfig::default();
    cfg.point_gap = 256 * 1024;

    let mut opened = Vec::with_capacity(paths.len());
    for p in paths {
        let f = StrataFile::open(Arc::clone(&store), p, None, &cfg).await?;
        opened.push(Arc::new(f));
    }

    let matched = run_strata_query(&opened, table, qname, sample_tid).await?;
    let gets: u64 = s3_readers.iter().map(|r| r.counters().gets).sum();
    let bytes: u64 = s3_readers.iter().map(|r| r.counters().bytes_from_s3).sum();
    let s3_ms: f64 = s3_readers.iter().map(|r| r.simulated_latency_ms()).sum();
    Ok((matched, gets, bytes, s3_ms))
}

pub async fn run_strata_query(
    files: &[Arc<StrataFile>],
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<usize> {
    let (t_lo, t_hi) = (DAY_START + 8 * HOUR_NS, DAY_START + 14 * HOUR_NS);
    let (top_lo, top_hi) = (DAY_START + 6 * HOUR_NS, DAY_START + 18 * HOUR_NS);

    if qname == "cold_open_prune" {
        let absent = [0xAAu8; 16];
        return match table {
            TableKindBench::Spans => {
                for f in files {
                    let _ = f.lookup_scan(&absent, &[], false).await?;
                }
                Ok(0)
            }
            TableKindBench::Scores => {
                let req = make_scan_req(
                    vec![],
                    Some(Predicate::Cmp(3, CmpOp::Eq, Lit::Bytes(absent.to_vec()))),
                    ScanKind::Point,
                );
                collect_row_count(files, &req).await
            }
        };
    }

    match (table, qname) {
        (TableKindBench::Spans, "point_lookup_narrow") => {
            let proj = [
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
            ];
            let batch = strata::trace_detail(files, sample_tid, &proj).await?;
            Ok(batch.num_rows())
        }
        (TableKindBench::Spans, "point_lookup_full") => {
            let schema = arrow_spans_schema();
            let proj: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
            let batch = strata::trace_detail(files, sample_tid, &proj).await?;
            Ok(batch.num_rows())
        }
        (TableKindBench::Scores, "point_lookup_narrow") => {
            let req = make_scan_req(
                vec![1, 3, 7, 9, 10, 20],
                Some(Predicate::Cmp(
                    3,
                    CmpOp::Eq,
                    Lit::Bytes(sample_tid.to_vec()),
                )),
                ScanKind::Point,
            );
            collect_row_count(files, &req).await
        }
        (TableKindBench::Spans, "filtered_count") => {
            let pred = Predicate::And(vec![
                Predicate::Cmp(6, CmpOp::GtEq, Lit::Int(t_lo)),
                Predicate::Cmp(6, CmpOp::LtEq, Lit::Int(t_hi)),
                Predicate::Cmp(9, CmpOp::Eq, Lit::Utf8("error".to_owned())),
            ]);
            let req = make_scan_req(vec![], Some(pred), ScanKind::Scan);
            collect_row_count(files, &req).await
        }
        (TableKindBench::Scores, "filtered_count") => {
            let pred = Predicate::And(vec![
                Predicate::Cmp(20, CmpOp::GtEq, Lit::Int(t_lo)),
                Predicate::Cmp(20, CmpOp::LtEq, Lit::Int(t_hi)),
                Predicate::Cmp(7, CmpOp::Eq, Lit::Utf8("faithfulness".to_owned())),
            ]);
            let req = make_scan_req(vec![], Some(pred), ScanKind::Scan);
            collect_row_count(files, &req).await
        }
        (TableKindBench::Spans, "time_bucket_agg") => {
            let req = make_scan_req(
                vec![6, 21, 30, 33],
                Some(Predicate::Cmp(5, CmpOp::Eq, Lit::Utf8("llm".to_owned()))),
                ScanKind::Scan,
            );
            let mut agg: BTreeMap<(i64, String), (usize, f64, i64)> = BTreeMap::new();
            for f in files {
                let batches = f.scan(&req).await?;
                for batch in &batches {
                    agg_pq_filtered_batch(batch, &mut agg)?;
                }
            }
            Ok(agg.values().map(|v| v.0).sum())
        }
        (TableKindBench::Spans, "top_k_list") => {
            let proj = [
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
            ];
            let pred = Some(Predicate::Cmp(5, CmpOp::Eq, Lit::Utf8("llm".to_owned())));
            let batch = strata::trace_list(files, top_lo..(top_hi + 1), pred, 50, &proj).await?;
            Ok(batch.num_rows())
        }
        (TableKindBench::Spans, "map_key_lookup") => {
            let req = make_scan_req(
                vec![1, 6],
                Some(Predicate::MapKeyEq(
                    17,
                    "customer_id".to_owned(),
                    Lit::Utf8("cust-42".to_owned()),
                )),
                ScanKind::Sparse,
            );
            collect_row_count(files, &req).await
        }
        (TableKindBench::Scores, "map_key_lookup") => {
            let req = make_scan_req(
                vec![1, 20],
                Some(Predicate::MapKeyEq(
                    19,
                    "customer_id".to_owned(),
                    Lit::Utf8("cust-42".to_owned()),
                )),
                ScanKind::Sparse,
            );
            collect_row_count(files, &req).await
        }
        (TableKindBench::Spans, "substring_rare" | "substring_absent") => {
            let needle = if qname == "substring_rare" {
                RARE_NEEDLE
            } else {
                ABSENT_NEEDLE
            };
            let req = make_scan_req(
                vec![1],
                Some(Predicate::Contains(37, needle.as_bytes().to_vec())),
                ScanKind::Sparse,
            );
            collect_row_count(files, &req).await
        }
        _ => Ok(0),
    }
}

pub fn make_scan_req(
    projection: Vec<usize>,
    predicate: Option<Predicate>,
    kind: ScanKind,
) -> ScanRequest {
    ScanRequest {
        projection,
        predicate,
        row_ranges: None,
        limit: None,
        kind,
        views: false,
        top: None,
        row_filter: None,
    }
}

pub async fn collect_row_count(files: &[Arc<StrataFile>], req: &ScanRequest) -> BenchResult<usize> {
    let mut total = 0usize;
    for f in files {
        let batches = f.scan(req).await?;
        for batch in &batches {
            total += batch.num_rows();
        }
    }
    Ok(total)
}
