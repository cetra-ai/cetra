use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;

use arrow::record_batch::RecordBatch as RecordBatch59;
use arrow_array_60::cast::AsArray as AsArray60;
use arrow_array_60::{
    Array as Array60, ArrayRef as ArrayRef60, Float64Array as Float64Array60,
    Int64Array as Int64Array60, ListArray as ListArray60, MapArray as MapArray60,
    RecordBatch as RecordBatch60, StringArray as StringArray60,
    StringViewArray as StringViewArray60, TimestampNanosecondArray as TimestampNanosecondArray60,
};
use arrow_cast_60::cast as cast60;
use arrow_schema_60::{
    DataType as DataType60, Field as Field60, Fields as Fields60, Schema as Schema60,
    TimeUnit as TimeUnit60,
};
use bytes::Bytes;
use cetra_statam::{RangeReader, SimulatedS3Config, SimulatedS3Reader, SliceReader};
use futures::future::BoxFuture;
use futures::stream::TryStreamExt;
use vortex::VortexSessionDefault;
use vortex::array::arrays::ChunkedArray;
use vortex::array::buffer::BufferHandle;
use vortex::array::{IntoArray, VortexSessionExecute};
use vortex::arrow::{ArrowSessionExt, ExecuteArrowExt};
use vortex::buffer::{Alignment, ByteBuffer, ByteBufferMut};
use vortex::compressor::BtrBlocksCompressorBuilder;
use vortex::dtype::Nullability;
use vortex::error::{VortexResult, vortex_err};
use vortex::expr::{Expression, and, col, eq, gt_eq, lit, lt_eq, root, select};
use vortex::file::{
    OpenOptionsSessionExt, VortexFile, WriteOptionsSessionExt, WriteStrategyBuilder,
};
use vortex::io::session::RuntimeSessionExt;
use vortex::scalar::Scalar;
use vortex::session::VortexSession;
use vortex_io::VortexReadAt;

use crate::queries::{contains_ascii_ci, push_topk};
use crate::schema::{BenchResult, TableKindBench};
use crate::workloads::{ABSENT_NEEDLE, DAY_START, HOUR_NS, RARE_NEEDLE};

pub struct SimS3VortexRead {
    reader: Arc<dyn RangeReader>,
}

impl SimS3VortexRead {
    pub fn new(reader: Arc<dyn RangeReader>) -> Self {
        Self { reader }
    }
}

impl VortexReadAt for SimS3VortexRead {
    fn uri(&self) -> Option<&Arc<str>> {
        None
    }

    fn concurrency(&self) -> usize {
        16
    }

    fn size(&self) -> BoxFuture<'static, VortexResult<u64>> {
        Box::pin(futures::future::ok(self.reader.file_len()))
    }

    fn read_at(
        &self,
        offset: u64,
        length: usize,
        alignment: Alignment,
    ) -> BoxFuture<'static, VortexResult<BufferHandle>> {
        let reader = Arc::clone(&self.reader);
        Box::pin(async move {
            let end = offset.saturating_add(length as u64);
            let bytes = reader
                .read_range(offset..end)
                .map_err(|e| vortex_err!("vortex range read failed: {e}"))?;
            Ok(BufferHandle::new_host(ByteBuffer::copy_from_aligned(
                &bytes, alignment,
            )))
        })
    }
}

pub fn new_vortex_session() -> VortexSession {
    VortexSession::default().with_tokio()
}

pub fn arrow59_to_arrow60(batch59: &RecordBatch59) -> BenchResult<RecordBatch60> {
    let mut ipc_buf = Vec::new();
    {
        let mut writer =
            arrow::ipc::writer::StreamWriter::try_new(&mut ipc_buf, batch59.schema().as_ref())?;
        writer.write(batch59)?;
        writer.finish()?;
    }
    let reader = arrow_ipc_60::reader::StreamReader::try_new(Cursor::new(ipc_buf), None)?;
    let batches: Vec<RecordBatch60> = reader.collect::<Result<_, _>>()?;
    if batches.len() == 1 {
        Ok(batches.into_iter().next().ok_or("missing batch")?)
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
            arrow_ipc_60::reader::StreamReader::try_new(Cursor::new(schema_buf), None)?;
        Ok(RecordBatch60::new_empty(empty_reader.schema()))
    } else {
        let schema = batches[0].schema();
        Ok(arrow_select_concat(&schema, &batches)?)
    }
}

fn arrow_select_concat(
    schema: &Arc<Schema60>,
    batches: &[RecordBatch60],
) -> Result<RecordBatch60, arrow_schema_60::ArrowError> {
    let mut cols: Vec<ArrayRef60> = Vec::with_capacity(schema.fields().len());
    for col_idx in 0..schema.fields().len() {
        let arrays: Vec<&dyn Array60> =
            batches.iter().map(|b| b.column(col_idx).as_ref()).collect();
        let combined = arrow_select_60_concat(&arrays)?;
        cols.push(combined);
    }
    RecordBatch60::try_new(Arc::clone(schema), cols)
}

fn arrow_select_60_concat(
    arrays: &[&dyn Array60],
) -> Result<ArrayRef60, arrow_schema_60::ArrowError> {
    // Only reached if an IPC stream unexpectedly splits one RecordBatch.
    if let Some(first) = arrays.first() {
        Ok(first.slice(0, first.len()))
    } else {
        Err(arrow_schema_60::ArrowError::InvalidArgumentError(
            "empty batch list".to_owned(),
        ))
    }
}

fn map_fsb_type(t: &DataType60) -> DataType60 {
    match t {
        DataType60::FixedSizeBinary(_) => DataType60::Binary,
        DataType60::List(f) => DataType60::List(Arc::new(map_fsb_field(f))),
        DataType60::Struct(fs) => DataType60::Struct(Fields60::from(
            fs.iter().map(|f| map_fsb_field(f)).collect::<Vec<_>>(),
        )),
        other => other.clone(),
    }
}

fn map_fsb_field(f: &Field60) -> Field60 {
    f.as_ref()
        .clone()
        .with_data_type(map_fsb_type(f.data_type()))
}

fn replace_maps_with_lists(batch: &RecordBatch60) -> BenchResult<RecordBatch60> {
    let mut fields: Vec<Field60> = Vec::with_capacity(batch.schema().fields().len());
    let mut cols: Vec<ArrayRef60> = Vec::with_capacity(batch.num_columns());
    for (field, col) in batch.schema().fields().iter().zip(batch.columns()) {
        if let DataType60::Map(entries_field, _) = field.data_type() {
            let ma = col
                .as_any()
                .downcast_ref::<MapArray60>()
                .ok_or("expected MapArray")?;
            let (_, offsets, entries, nulls, _) = ma.clone().into_parts();
            let item_field = Arc::new(Field60::new(
                "item",
                entries_field.data_type().clone(),
                false,
            ));
            let list_arr =
                ListArray60::new(Arc::clone(&item_field), offsets, Arc::new(entries), nulls);
            fields.push(
                field
                    .as_ref()
                    .clone()
                    .with_data_type(DataType60::List(item_field)),
            );
            cols.push(Arc::new(list_arr));
        } else {
            fields.push(field.as_ref().clone());
            cols.push(Arc::clone(col));
        }
    }
    Ok(RecordBatch60::try_new(
        Arc::new(Schema60::new(fields)),
        cols,
    )?)
}

pub fn vortex_compatible_batch(
    session: &VortexSession,
    batch: &RecordBatch60,
) -> BenchResult<RecordBatch60> {
    let fields: Vec<Field60> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| map_fsb_field(f))
        .collect();
    let cols: Vec<ArrayRef60> = batch
        .columns()
        .iter()
        .zip(&fields)
        .map(|(c, f)| {
            if c.data_type() == f.data_type() {
                Ok(Arc::clone(c))
            } else {
                cast60(c, f.data_type())
            }
        })
        .collect::<Result<_, _>>()?;
    let no_fsb = RecordBatch60::try_new(Arc::new(Schema60::new(fields)), cols)?;
    if session
        .arrow()
        .from_arrow_schema(no_fsb.schema().as_ref())
        .is_ok()
    {
        Ok(no_fsb)
    } else {
        replace_maps_with_lists(&no_fsb)
    }
}

pub fn prepare_vortex_batches(
    session: &VortexSession,
    batches59: &[RecordBatch59],
) -> BenchResult<Vec<RecordBatch60>> {
    let mut prepared = Vec::with_capacity(batches59.len());
    for b59 in batches59 {
        let b60 = arrow59_to_arrow60(b59)?;
        prepared.push(vortex_compatible_batch(session, &b60)?);
    }
    Ok(prepared)
}

pub async fn write_vortex_prepared(
    session: &VortexSession,
    prepared: &[RecordBatch60],
) -> BenchResult<(Vec<Bytes>, f64)> {
    let t0 = Instant::now();
    let mut out = Vec::with_capacity(prepared.len());
    for b60 in prepared {
        let schema = b60.schema();
        let dtype = session.arrow().from_arrow_schema(schema.as_ref())?;
        let mut chunks = Vec::new();
        for off in (0..b60.num_rows()).step_by(8_192) {
            let len = 8_192.min(b60.num_rows() - off);
            let slice = b60.slice(off, len);
            chunks.push(session.arrow().from_arrow_record_batch(slice, &schema)?);
        }
        let vx_array = ChunkedArray::try_new(chunks, dtype)?.into_array();
        let strategy = WriteStrategyBuilder::from_session(session)
            .with_btrblocks_builder(
                BtrBlocksCompressorBuilder::from_session(session).with_compact(),
            )
            .build();
        let mut out_buf = ByteBufferMut::empty();
        session
            .write_options()
            .with_strategy(strategy)
            .write(&mut out_buf, vx_array.to_array_stream())
            .await?;
        out.push(Bytes::copy_from_slice(out_buf.as_ref()));
    }
    Ok((out, t0.elapsed().as_secs_f64() * 1000.0))
}

pub async fn write_vortex_files(
    session: &VortexSession,
    batches59: &[RecordBatch59],
) -> BenchResult<(Vec<Bytes>, f64)> {
    let prepared = prepare_vortex_batches(session, batches59)?;
    write_vortex_prepared(session, &prepared).await
}

pub async fn open_vortex_local(
    session: &VortexSession,
    files: &[Bytes],
) -> BenchResult<Vec<VortexFile>> {
    let mut opened = Vec::with_capacity(files.len());
    for b in files {
        let reader = Arc::new(SimS3VortexRead::new(
            Arc::new(SliceReader::new(b.clone())) as Arc<dyn RangeReader>
        ));
        let vxf = session.open_options().open_read(reader).await?;
        opened.push(vxf);
    }
    Ok(opened)
}

pub async fn run_vortex_query_s3(
    session: &VortexSession,
    files: &[Bytes],
    s3_cfg: SimulatedS3Config,
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<(usize, u64, u64, f64)> {
    let s3_readers: Vec<Arc<SimulatedS3Reader>> = files
        .iter()
        .map(|b| Arc::new(SimulatedS3Reader::from_bytes(b.clone(), s3_cfg)))
        .collect();
    for r in &s3_readers {
        r.evict_all();
        r.reset_counters();
    }

    let mut opened = Vec::with_capacity(s3_readers.len());
    for r in &s3_readers {
        let vx_reader = Arc::new(SimS3VortexRead::new(Arc::clone(r) as Arc<dyn RangeReader>));
        let vxf = session.open_options().open_read(vx_reader).await?;
        opened.push(vxf);
    }

    let matched = run_vortex_query(session, &opened, table, qname, sample_tid).await?;
    let gets: u64 = s3_readers.iter().map(|r| r.counters().gets).sum();
    let bytes: u64 = s3_readers.iter().map(|r| r.counters().bytes_from_s3).sum();
    let s3_ms: f64 = s3_readers.iter().map(|r| r.simulated_latency_ms()).sum();
    Ok((matched, gets, bytes, s3_ms))
}

fn ts_scalar(session: &VortexSession, ns: i64) -> BenchResult<Scalar> {
    let arr: ArrayRef60 = Arc::new(TimestampNanosecondArray60::from(vec![ns]).with_timezone("UTC"));
    let field = Field60::new(
        "",
        DataType60::Timestamp(TimeUnit60::Nanosecond, Some("UTC".into())),
        false,
    );
    let vx_arr = session.arrow().from_arrow_array(arr, &field)?;
    let mut ctx = session.create_execution_ctx();
    Ok(vx_arr.execute_scalar(0, &mut ctx)?)
}

fn binary_scalar(bytes: &[u8]) -> Scalar {
    Scalar::binary(ByteBuffer::copy_from(bytes), Nullability::NonNullable)
}

pub fn chunk_to_record_batch(
    session: &VortexSession,
    chunk: vortex::array::ArrayRef,
) -> BenchResult<RecordBatch60> {
    let mut ctx = session.create_execution_ctx();
    let arrow = chunk.execute_arrow(None, &mut ctx)?;
    Ok(RecordBatch60::from(arrow.as_struct().clone()))
}

pub fn str_value_at<'a>(arr: &'a dyn Array60, idx: usize) -> Option<&'a str> {
    if arr.is_null(idx) {
        return None;
    }
    if let Some(s) = arr.as_any().downcast_ref::<StringViewArray60>() {
        Some(s.value(idx))
    } else if let Some(s) = arr.as_any().downcast_ref::<StringArray60>() {
        Some(s.value(idx))
    } else {
        None
    }
}

pub async fn run_vortex_query(
    session: &VortexSession,
    files: &[VortexFile],
    table: TableKindBench,
    qname: &str,
    sample_tid: [u8; 16],
) -> BenchResult<usize> {
    let (t_lo, t_hi) = (DAY_START + 8 * HOUR_NS, DAY_START + 14 * HOUR_NS);
    let (top_lo, top_hi) = (DAY_START + 6 * HOUR_NS, DAY_START + 18 * HOUR_NS);

    if qname == "cold_open_prune" {
        let expr = eq(col("trace_id"), lit(binary_scalar(&[0xAAu8; 16])));
        let proj_expr = select([] as [&str; 0], root());
        for f in files {
            let filter = expr.clone().bind(f.dtype())?.optimize_recursive()?;
            let projection = proj_expr.clone().bind(f.dtype())?.optimize_recursive()?;
            let mut stream = f
                .scan()?
                .with_projection(projection)
                .with_filter(filter)
                .into_array_stream()?;
            while let Some(_chunk) = stream.try_next().await? {}
        }
        return Ok(0);
    }

    let (proj_expr, filter_opt): (Expression, Option<Expression>) = match (table, qname) {
        (TableKindBench::Spans, "point_lookup_narrow") => (
            select(
                [
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
                ],
                root(),
            ),
            Some(eq(col("trace_id"), lit(binary_scalar(&sample_tid)))),
        ),
        (TableKindBench::Spans, "point_lookup_full") => (
            root(),
            Some(eq(col("trace_id"), lit(binary_scalar(&sample_tid)))),
        ),
        (TableKindBench::Scores, "point_lookup_narrow") => (
            select(
                [
                    "score_id",
                    "trace_id",
                    "name",
                    "value_num",
                    "value_str",
                    "created_at",
                ],
                root(),
            ),
            Some(eq(col("trace_id"), lit(binary_scalar(&sample_tid)))),
        ),
        (TableKindBench::Spans, "filtered_count") => (
            select([] as [&str; 0], root()),
            Some(and(
                and(
                    gt_eq(col("start_time"), lit(ts_scalar(session, t_lo)?)),
                    lt_eq(col("start_time"), lit(ts_scalar(session, t_hi)?)),
                ),
                eq(col("status"), lit("error")),
            )),
        ),
        (TableKindBench::Scores, "filtered_count") => (
            select([] as [&str; 0], root()),
            Some(and(
                and(
                    gt_eq(col("created_at"), lit(ts_scalar(session, t_lo)?)),
                    lt_eq(col("created_at"), lit(ts_scalar(session, t_hi)?)),
                ),
                eq(col("name"), lit("faithfulness")),
            )),
        ),
        (TableKindBench::Spans, "time_bucket_agg") => (
            select(
                ["start_time", "model", "total_tokens", "cost_total"],
                root(),
            ),
            Some(eq(col("span_kind"), lit("llm"))),
        ),
        (TableKindBench::Spans, "top_k_list") => (
            select(
                [
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
                ],
                root(),
            ),
            Some(and(
                and(
                    gt_eq(col("start_time"), lit(ts_scalar(session, top_lo)?)),
                    lt_eq(col("start_time"), lit(ts_scalar(session, top_hi)?)),
                ),
                eq(col("span_kind"), lit("llm")),
            )),
        ),
        (TableKindBench::Spans, "map_key_lookup") => {
            (select(["trace_id", "start_time", "metadata"], root()), None)
        }
        (TableKindBench::Scores, "map_key_lookup") => {
            (select(["score_id", "metadata", "created_at"], root()), None)
        }
        (TableKindBench::Spans, "substring_rare" | "substring_absent") => {
            (select(["trace_id", "input"], root()), None)
        }
        _ => (root(), None),
    };

    let mut total = 0usize;
    let mut agg: BTreeMap<(i64, String), (usize, f64, i64)> = BTreeMap::new();
    let mut topk: BinaryHeap<Reverse<(i64, usize)>> = BinaryHeap::new();

    for f in files {
        let projection = proj_expr.clone().bind(f.dtype())?.optimize_recursive()?;
        let mut builder = f.scan()?.with_projection(projection);
        if let Some(expr) = &filter_opt {
            let filter = expr.clone().bind(f.dtype())?.optimize_recursive()?;
            builder = builder.with_filter(filter);
        }
        let mut stream = builder.into_array_stream()?;
        while let Some(chunk) = stream.try_next().await? {
            if chunk.is_empty() {
                continue;
            }
            match qname {
                "filtered_count" => {
                    total += chunk.len();
                }
                "point_lookup_narrow" | "point_lookup_full" => {
                    let batch = chunk_to_record_batch(session, chunk)?;
                    total += batch.num_rows();
                }
                "time_bucket_agg" => {
                    let batch = chunk_to_record_batch(session, chunk)?;
                    agg_vortex_batch(&batch, &mut agg)?;
                }
                "top_k_list" => {
                    let batch = chunk_to_record_batch(session, chunk)?;
                    topk_vortex_batch(&batch, &mut topk)?;
                }
                "map_key_lookup" => {
                    let batch = chunk_to_record_batch(session, chunk)?;
                    total += eval_vortex_map_key(&batch)?;
                }
                "substring_rare" | "substring_absent" => {
                    let needle = if qname == "substring_rare" {
                        RARE_NEEDLE.as_bytes()
                    } else {
                        ABSENT_NEEDLE.as_bytes()
                    };
                    let batch = chunk_to_record_batch(session, chunk)?;
                    if let Some(inp) = batch.column_by_name("input") {
                        for i in 0..batch.num_rows() {
                            if let Some(s) = str_value_at(inp.as_ref(), i)
                                && contains_ascii_ci(s.as_bytes(), needle)
                            {
                                total += 1;
                            }
                        }
                    }
                }
                _ => {}
            }
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

fn agg_vortex_batch(
    b: &RecordBatch60,
    agg: &mut BTreeMap<(i64, String), (usize, f64, i64)>,
) -> BenchResult<()> {
    let ts = b
        .column_by_name("start_time")
        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray60>());
    let md = b.column_by_name("model");
    let tok = b
        .column_by_name("total_tokens")
        .and_then(|c| c.as_any().downcast_ref::<Int64Array60>());
    let cost = b
        .column_by_name("cost_total")
        .and_then(|c| c.as_any().downcast_ref::<Float64Array60>());
    if let (Some(ts), Some(md), Some(tok), Some(cost)) = (ts, md, tok, cost) {
        for i in 0..b.num_rows() {
            let bucket = ts.value(i) / HOUR_NS;
            let model = str_value_at(md.as_ref(), i).unwrap_or("unknown");
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

fn topk_vortex_batch(
    b: &RecordBatch60,
    topk: &mut BinaryHeap<Reverse<(i64, usize)>>,
) -> BenchResult<()> {
    if let Some(ts) = b
        .column_by_name("start_time")
        .and_then(|c| c.as_any().downcast_ref::<TimestampNanosecondArray60>())
    {
        for i in 0..b.num_rows() {
            push_topk(topk, ts.value(i), i);
        }
    }
    Ok(())
}

fn eval_vortex_map_key(b: &RecordBatch60) -> BenchResult<usize> {
    let Some(col) = b.column_by_name("metadata") else {
        return Ok(0);
    };
    let mut matched = 0usize;
    if let Some(ma) = col.as_any().downcast_ref::<MapArray60>() {
        let keys = ma.keys();
        let vals = ma.values();
        let offsets = ma.value_offsets();
        for r in 0..ma.len() {
            if ma.is_null(r) {
                continue;
            }
            let s = offsets[r] as usize;
            let e = offsets[r + 1] as usize;
            for j in s..e {
                if str_value_at(keys.as_ref(), j) == Some("customer_id")
                    && str_value_at(vals.as_ref(), j) == Some("cust-42")
                {
                    matched += 1;
                    break;
                }
            }
        }
    } else if let Some(la) = col.as_any().downcast_ref::<ListArray60>() {
        let entries = la.values().as_struct();
        let keys = entries.column(0);
        let vals = entries.column(1);
        let offsets = la.value_offsets();
        for r in 0..la.len() {
            if la.is_null(r) {
                continue;
            }
            let s = offsets[r] as usize;
            let e = offsets[r + 1] as usize;
            for j in s..e {
                if str_value_at(keys.as_ref(), j) == Some("customer_id")
                    && str_value_at(vals.as_ref(), j) == Some("cust-42")
                {
                    matched += 1;
                    break;
                }
            }
        }
    }
    Ok(matched)
}
