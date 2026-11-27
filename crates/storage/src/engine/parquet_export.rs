//! Zero-Arrow streaming Parquet export writer over `cetra_column::Batch`.
//!
//! Uses `parquet::file::writer::SerializedFileWriter` over a [`Reservation`]-backed
//! buffer with ZSTD level 1 compression and row-group buffering gated by [`Reservation`].

use std::fmt;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use cetra_column::{
    Batch, Column, DataType, EventItem, EventsData, FlatColumn, FlatData, LinkItem, LinksData,
    MapData, Reservation, ScalarValue, Schema, ScopedPool, Selection,
};
use cetra_core::schema::{event_field, link_field};
use cetra_query::QueryError;
use cetra_query::encode::{EXPORT_MAX_CHUNK_BYTES, format_interval};
use parquet::basic::{
    Compression, LogicalType, Repetition, TimeUnit, Type as PhysicalType, ZstdLevel,
};
use parquet::column::writer::ColumnWriter;
use parquet::data_type::{ByteArray, FixedLenByteArray};
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;
use parquet::file::writer::{
    SerializedColumnWriter, SerializedFileWriter, SerializedRowGroupWriter,
};
use parquet::schema::types::{Type, TypePtr};

/// Default maximum active rows buffered before flushing a Parquet row group.
pub(crate) const PARQUET_ROW_GROUP_ROWS: usize = 1_048_576;

/// Default maximum buffered batch bytes before flushing a Parquet row group.
pub(crate) const PARQUET_CHUNK_BYTES: usize = 32 * 1024 * 1024;

/// Maximum top-level rows buffered in [`LeafBuffer`] before calling `write_batch`.
const LEAF_CHUNK_ROWS: usize = 4_096;

/// `Vec<u8>` sink whose every [`Write::write`] reserves bytes against a [`Reservation`]
/// before growing the buffer, setting `oom` on pool exhaustion so callers always map to
/// [`QueryError::TooLarge`].
struct ReservedVec {
    buf: Vec<u8>,
    reservation: Reservation,
    oom: Arc<AtomicBool>,
}

impl ReservedVec {
    fn new(reservation: Reservation, oom: Arc<AtomicBool>) -> Self {
        Self {
            buf: Vec::new(),
            reservation,
            oom,
        }
    }

    fn take(&mut self) -> Vec<u8> {
        let buf = std::mem::take(&mut self.buf);
        self.reservation.free();
        buf
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.buf.shrink_to_fit();
        self.reservation.free();
    }

    fn into_vec(mut self) -> Vec<u8> {
        self.reservation.free();
        self.buf
    }

    fn reserved_bytes(&self) -> usize {
        self.reservation.size()
    }
}

impl Write for ReservedVec {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if !data.is_empty() && self.reservation.try_grow(data.len()).is_err() {
            self.oom.store(true, Ordering::Release);
            return Err(std::io::Error::other(
                "parquet export memory limit exceeded",
            ));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Streaming low-level Parquet exporter consuming [`Batch`]es and yielding
/// completed Parquet chunks on row-group flushes.
pub(crate) struct ParquetBatchExporter {
    schema: Arc<Schema>,
    writer: SerializedFileWriter<ReservedVec>,
    oom: Arc<AtomicBool>,
    buffered_batches: Vec<Batch>,
    buffered_rows: usize,
    buffered_bytes: usize,
    max_row_group_rows: usize,
    max_row_group_bytes: usize,
    max_chunk_bytes: usize,
    reservation: Reservation,
}

impl fmt::Debug for ParquetBatchExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParquetBatchExporter")
            .field("schema_len", &self.schema.len())
            .field("buffered_batches", &self.buffered_batches.len())
            .field("buffered_rows", &self.buffered_rows)
            .field("buffered_bytes", &self.buffered_bytes)
            .field(
                "writer_reserved_bytes",
                &self.writer.inner().reserved_bytes(),
            )
            .field("max_row_group_rows", &self.max_row_group_rows)
            .field("max_row_group_bytes", &self.max_row_group_bytes)
            .field("max_chunk_bytes", &self.max_chunk_bytes)
            .field("reserved_bytes", &self.reservation.size())
            .finish_non_exhaustive()
    }
}

fn split_bytes_chunks(buf: Vec<u8>, max_chunk_bytes: usize) -> Vec<Bytes> {
    if buf.is_empty() {
        return Vec::new();
    }
    let bytes = Bytes::from(buf);
    let step = max_chunk_bytes.max(1);
    if bytes.len() <= step {
        return vec![bytes];
    }
    let mut out = Vec::with_capacity(bytes.len().div_ceil(step));
    let mut off = 0usize;
    while off < bytes.len() {
        let end = (off + step).min(bytes.len());
        out.push(bytes.slice(off..end));
        off = end;
    }
    out
}

impl ParquetBatchExporter {
    /// Creates an exporter using [`PARQUET_ROW_GROUP_ROWS`] and [`PARQUET_CHUNK_BYTES`].
    #[cfg(test)]
    pub(crate) fn new(schema: Arc<Schema>, pool: &Arc<ScopedPool>) -> Result<Self, QueryError> {
        Self::with_row_group_limits(schema, pool, PARQUET_ROW_GROUP_ROWS, PARQUET_CHUNK_BYTES)
    }

    /// Creates an exporter with explicit row-group flush thresholds.
    pub(crate) fn with_row_group_limits(
        schema: Arc<Schema>,
        pool: &Arc<ScopedPool>,
        max_row_group_rows: usize,
        max_row_group_bytes: usize,
    ) -> Result<Self, QueryError> {
        if schema.is_empty() {
            return Err(QueryError::failed(
                "cannot export zero-column result to parquet",
            ));
        }
        if max_row_group_rows == 0 || max_row_group_bytes == 0 {
            return Err(QueryError::failed(
                "parquet row group limits must be greater than zero",
            ));
        }

        let parquet_schema = build_parquet_schema(&schema)?;
        let props = Arc::new(
            WriterProperties::builder()
                .set_compression(Compression::ZSTD(ZstdLevel::default()))
                .build(),
        );
        let oom = Arc::new(AtomicBool::new(false));
        let writer_res = pool.register("parquet_export_writer", false);
        let writer = SerializedFileWriter::new(
            ReservedVec::new(writer_res, Arc::clone(&oom)),
            parquet_schema,
            props,
        )
        .map_err(|e| map_parquet_err(&oom, &e))?;
        let reservation = pool.register("parquet_export", false);

        Ok(Self {
            schema,
            writer,
            oom,
            buffered_batches: Vec::new(),
            buffered_rows: 0,
            buffered_bytes: 0,
            max_row_group_rows,
            max_row_group_bytes,
            max_chunk_bytes: EXPORT_MAX_CHUNK_BYTES,
            reservation,
        })
    }

    /// Caps each emitted [`Bytes`] slice to at most `max_chunk_bytes` (clamped `>= 1`).
    pub(crate) fn with_chunk_bytes(mut self, max_chunk_bytes: usize) -> Self {
        self.max_chunk_bytes = max_chunk_bytes.max(1);
        self
    }

    fn reserve_batch_bytes(
        &mut self,
        batch_res: Option<&mut Reservation>,
        bytes: usize,
    ) -> Result<(), QueryError> {
        if let Some(src) = batch_res {
            super::transfer_or_grow(src, &mut self.reservation, bytes)?;
        } else {
            self.reservation.try_grow(bytes)?;
        }
        Ok(())
    }

    /// Buffers `batch` under the query memory pool (transferring ownership from `batch_res`
    /// via `transfer_or_grow` to avoid a 2x spike) and flushes a row group once buffered rows
    /// reach `max_row_group_rows` or buffered bytes reach `max_row_group_bytes`.
    pub(crate) fn push_batch(
        &mut self,
        batch: Batch,
        mut batch_res: Option<&mut Reservation>,
    ) -> Result<Vec<Bytes>, QueryError> {
        if !schemas_compatible(&self.schema, batch.schema()) {
            return Err(QueryError::failed(
                "batch schema does not match parquet export schema",
            ));
        }
        let rows = batch.num_rows();
        if rows == 0 {
            return Ok(Vec::new());
        }

        let bytes = batch.memory_bytes();
        let mut chunks = Vec::new();
        if let Err(err) = self.reserve_batch_bytes(batch_res.as_deref_mut(), bytes) {
            if self.buffered_batches.is_empty() {
                return Err(err);
            }
            chunks.extend(self.flush_row_group()?);
            self.reserve_batch_bytes(batch_res, bytes)?;
        }
        self.buffered_batches.push(batch);
        self.buffered_rows = self.buffered_rows.saturating_add(rows);
        self.buffered_bytes = self.buffered_bytes.saturating_add(bytes);

        if self.buffered_rows >= self.max_row_group_rows
            || self.buffered_bytes >= self.max_row_group_bytes
        {
            chunks.extend(self.flush_row_group()?);
        }
        Ok(chunks)
    }

    /// Flushes any remaining buffered rows, writes the Parquet footer, and
    /// returns the trailing file bytes split into `<= max_chunk_bytes` slices.
    pub(crate) fn finish(mut self) -> Result<Vec<Bytes>, QueryError> {
        if !self.buffered_batches.is_empty() {
            self.write_buffered_row_group()?;
        }
        let oom = Arc::clone(&self.oom);
        let reserved_vec = self
            .writer
            .into_inner()
            .map_err(|e| map_parquet_err(&oom, &e))?;
        let buf = reserved_vec.into_vec();
        self.reservation.free();
        Ok(split_bytes_chunks(buf, self.max_chunk_bytes))
    }

    fn flush_row_group(&mut self) -> Result<Vec<Bytes>, QueryError> {
        self.write_buffered_row_group()?;
        if let Err(err) = self.writer.flush() {
            self.writer.inner_mut().clear();
            return Err(map_io_err(&self.oom, &err));
        }
        let buf = self.writer.inner_mut().take();
        Ok(split_bytes_chunks(buf, self.max_chunk_bytes))
    }

    fn write_buffered_row_group(&mut self) -> Result<(), QueryError> {
        if self.buffered_batches.is_empty() {
            return Ok(());
        }

        let res: Result<(), QueryError> = (|| {
            let mut rg = self.writer.next_row_group().map_err(|e| parquet_err(&e))?;
            write_columns_for_batches(&mut rg, &self.schema, &self.buffered_batches)?;
            rg.close().map_err(|e| parquet_err(&e))?;
            Ok(())
        })();

        self.buffered_batches.clear();
        self.buffered_rows = 0;
        let freed_batches = std::mem::take(&mut self.buffered_bytes);
        self.reservation
            .shrink(freed_batches.min(self.reservation.size()));

        if let Err(err) = res {
            self.writer.inner_mut().clear();
            if self.oom.load(Ordering::Acquire) {
                return Err(QueryError::TooLarge);
            }
            return Err(err);
        }
        Ok(())
    }
}

fn schemas_compatible(expected: &Schema, actual: &Schema) -> bool {
    expected.len() == actual.len()
        && expected
            .fields()
            .iter()
            .zip(actual.fields())
            .all(|(exp, act)| {
                exp.name == act.name
                    && exp.data_type == act.data_type
                    && (exp.nullable || !act.nullable)
            })
}

fn parquet_err(err: &ParquetError) -> QueryError {
    QueryError::internal(format!("parquet export error: {err}"))
}

fn map_parquet_err(oom: &AtomicBool, err: &ParquetError) -> QueryError {
    if oom.load(Ordering::Acquire) {
        QueryError::TooLarge
    } else {
        parquet_err(err)
    }
}

fn io_err(err: &std::io::Error) -> QueryError {
    QueryError::internal(format!("parquet export io error: {err}"))
}

fn map_io_err(oom: &AtomicBool, err: &std::io::Error) -> QueryError {
    if oom.load(Ordering::Acquire) {
        QueryError::TooLarge
    } else {
        io_err(err)
    }
}

fn build_parquet_schema(schema: &Schema) -> Result<TypePtr, QueryError> {
    let mut fields = Vec::with_capacity(schema.len());
    for field in schema.fields() {
        fields.push(build_parquet_field(
            &field.name,
            &field.data_type,
            field.nullable,
        )?);
    }
    Ok(Arc::new(
        Type::group_type_builder("cetra_export")
            .with_fields(fields)
            .build()
            .map_err(|e| parquet_err(&e))?,
    ))
}

fn build_parquet_field(
    name: &str,
    data_type: &DataType,
    nullable: bool,
) -> Result<TypePtr, QueryError> {
    let rep = if nullable {
        Repetition::OPTIONAL
    } else {
        Repetition::REQUIRED
    };

    let ty = match data_type {
        DataType::Null => Type::primitive_type_builder(name, PhysicalType::BOOLEAN)
            .with_repetition(Repetition::OPTIONAL)
            .build()
            .map_err(|e| parquet_err(&e))?,
        DataType::Bool => Type::primitive_type_builder(name, PhysicalType::BOOLEAN)
            .with_repetition(rep)
            .build()
            .map_err(|e| parquet_err(&e))?,
        DataType::I64 => Type::primitive_type_builder(name, PhysicalType::INT64)
            .with_repetition(rep)
            .build()
            .map_err(|e| parquet_err(&e))?,
        DataType::F64 => Type::primitive_type_builder(name, PhysicalType::DOUBLE)
            .with_repetition(rep)
            .build()
            .map_err(|e| parquet_err(&e))?,
        DataType::Decimal128 { precision, scale } => {
            if *scale >= 0 {
                let p = i32::from(*precision);
                let s = i32::from(*scale);
                Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
                    .with_repetition(rep)
                    .with_length(16)
                    .with_precision(p)
                    .with_scale(s)
                    .with_logical_type(Some(LogicalType::decimal(s, p)))
                    .build()
                    .map_err(|e| parquet_err(&e))?
            } else {
                let rescaled_prec = i32::from(*precision) - i32::from(*scale);
                if rescaled_prec <= 38 {
                    Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
                        .with_repetition(rep)
                        .with_length(16)
                        .with_precision(rescaled_prec)
                        .with_scale(0)
                        .with_logical_type(Some(LogicalType::decimal(0, rescaled_prec)))
                        .build()
                        .map_err(|e| parquet_err(&e))?
                } else {
                    Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
                        .with_repetition(rep)
                        .with_logical_type(Some(LogicalType::String))
                        .build()
                        .map_err(|e| parquet_err(&e))?
                }
            }
        }
        DataType::TimestampNs { utc } => Type::primitive_type_builder(name, PhysicalType::INT64)
            .with_repetition(rep)
            .with_logical_type(Some(LogicalType::timestamp(*utc, TimeUnit::NANOS)))
            .build()
            .map_err(|e| parquet_err(&e))?,
        DataType::Date => Type::primitive_type_builder(name, PhysicalType::INT32)
            .with_repetition(rep)
            .with_logical_type(Some(LogicalType::Date))
            .build()
            .map_err(|e| parquet_err(&e))?,
        DataType::Interval | DataType::Utf8 => {
            Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
                .with_repetition(rep)
                .with_logical_type(Some(LogicalType::String))
                .build()
                .map_err(|e| parquet_err(&e))?
        }
        DataType::Binary => Type::primitive_type_builder(name, PhysicalType::BYTE_ARRAY)
            .with_repetition(rep)
            .build()
            .map_err(|e| parquet_err(&e))?,
        DataType::FixedBytes8 => {
            Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
                .with_repetition(rep)
                .with_length(8)
                .build()
                .map_err(|e| parquet_err(&e))?
        }
        DataType::FixedBytes16 => {
            Type::primitive_type_builder(name, PhysicalType::FIXED_LEN_BYTE_ARRAY)
                .with_repetition(rep)
                .with_length(16)
                .build()
                .map_err(|e| parquet_err(&e))?
        }
        DataType::List(inner) => {
            let elem = build_parquet_field("element", inner, true)?;
            let list_group = Arc::new(
                Type::group_type_builder("list")
                    .with_repetition(Repetition::REPEATED)
                    .with_fields(vec![elem])
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            Type::group_type_builder(name)
                .with_repetition(rep)
                .with_logical_type(Some(LogicalType::List))
                .with_fields(vec![list_group])
                .build()
                .map_err(|e| parquet_err(&e))?
        }
        DataType::MapUtf8 => {
            let key = Arc::new(
                Type::primitive_type_builder("key", PhysicalType::BYTE_ARRAY)
                    .with_repetition(Repetition::REQUIRED)
                    .with_logical_type(Some(LogicalType::String))
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let value = Arc::new(
                Type::primitive_type_builder("value", PhysicalType::BYTE_ARRAY)
                    .with_repetition(Repetition::OPTIONAL)
                    .with_logical_type(Some(LogicalType::String))
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let kv = Arc::new(
                Type::group_type_builder("key_value")
                    .with_repetition(Repetition::REPEATED)
                    .with_fields(vec![key, value])
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            Type::group_type_builder(name)
                .with_repetition(rep)
                .with_logical_type(Some(LogicalType::Map))
                .with_fields(vec![kv])
                .build()
                .map_err(|e| parquet_err(&e))?
        }
        DataType::Events => {
            let time_field = Arc::new(
                Type::primitive_type_builder(event_field::TIME, PhysicalType::INT64)
                    .with_repetition(Repetition::REQUIRED)
                    .with_logical_type(Some(LogicalType::timestamp(true, TimeUnit::NANOS)))
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let name_field = Arc::new(
                Type::primitive_type_builder(event_field::NAME, PhysicalType::BYTE_ARRAY)
                    .with_repetition(Repetition::REQUIRED)
                    .with_logical_type(Some(LogicalType::String))
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let attr_field = Arc::new(
                Type::primitive_type_builder(event_field::ATTRIBUTES, PhysicalType::BYTE_ARRAY)
                    .with_repetition(Repetition::OPTIONAL)
                    .with_logical_type(Some(LogicalType::String))
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let elem = Arc::new(
                Type::group_type_builder("element")
                    .with_repetition(Repetition::REQUIRED)
                    .with_fields(vec![time_field, name_field, attr_field])
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let list_group = Arc::new(
                Type::group_type_builder("list")
                    .with_repetition(Repetition::REPEATED)
                    .with_fields(vec![elem])
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            Type::group_type_builder(name)
                .with_repetition(rep)
                .with_logical_type(Some(LogicalType::List))
                .with_fields(vec![list_group])
                .build()
                .map_err(|e| parquet_err(&e))?
        }
        DataType::Links => {
            let trace_field = Arc::new(
                Type::primitive_type_builder(
                    link_field::TRACE_ID,
                    PhysicalType::FIXED_LEN_BYTE_ARRAY,
                )
                .with_repetition(Repetition::REQUIRED)
                .with_length(16)
                .build()
                .map_err(|e| parquet_err(&e))?,
            );
            let span_field = Arc::new(
                Type::primitive_type_builder(
                    link_field::SPAN_ID,
                    PhysicalType::FIXED_LEN_BYTE_ARRAY,
                )
                .with_repetition(Repetition::REQUIRED)
                .with_length(8)
                .build()
                .map_err(|e| parquet_err(&e))?,
            );
            let attr_field = Arc::new(
                Type::primitive_type_builder(link_field::ATTRIBUTES, PhysicalType::BYTE_ARRAY)
                    .with_repetition(Repetition::OPTIONAL)
                    .with_logical_type(Some(LogicalType::String))
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let elem = Arc::new(
                Type::group_type_builder("element")
                    .with_repetition(Repetition::REQUIRED)
                    .with_fields(vec![trace_field, span_field, attr_field])
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            let list_group = Arc::new(
                Type::group_type_builder("list")
                    .with_repetition(Repetition::REPEATED)
                    .with_fields(vec![elem])
                    .build()
                    .map_err(|e| parquet_err(&e))?,
            );
            Type::group_type_builder(name)
                .with_repetition(rep)
                .with_logical_type(Some(LogicalType::List))
                .with_fields(vec![list_group])
                .build()
                .map_err(|e| parquet_err(&e))?
        }
    };
    Ok(Arc::new(ty))
}

fn leaf_count(data_type: &DataType) -> usize {
    match data_type {
        DataType::List(inner) => leaf_count(inner),
        DataType::MapUtf8 => 2,
        DataType::Events | DataType::Links => 3,
        _ => 1,
    }
}

fn write_columns_for_batches<W: Write + Send>(
    rg: &mut SerializedRowGroupWriter<'_, W>,
    schema: &Schema,
    batches: &[Batch],
) -> Result<(), QueryError> {
    for (col_idx, field) in schema.fields().iter().enumerate() {
        let leaves = leaf_count(&field.data_type);
        for leaf_idx in 0..leaves {
            let mut col_writer = rg
                .next_column()
                .map_err(|e| parquet_err(&e))?
                .ok_or_else(|| QueryError::internal("parquet row group missing expected column"))?;
            let max_def = get_max_def_level(&mut col_writer);
            let max_rep = get_max_rep_level(&mut col_writer);
            let mut buf = LeafBuffer::for_writer(&mut col_writer, max_def > 0, max_rep > 0)?;

            for batch in batches {
                let col = batch
                    .column(col_idx)
                    .ok_or_else(|| QueryError::internal("batch missing expected column"))?;
                emit_column_for_selection_chunked(
                    col,
                    batch.raw_len(),
                    batch.selection(),
                    leaf_idx,
                    field.nullable,
                    &mut buf,
                    &mut col_writer,
                )?;
            }

            col_writer.close().map_err(|e| parquet_err(&e))?;
        }
    }
    Ok(())
}

fn get_max_def_level(writer: &mut SerializedColumnWriter<'_>) -> i16 {
    match writer.untyped() {
        ColumnWriter::BoolColumnWriter(w) => w.get_descriptor().max_def_level(),
        ColumnWriter::Int32ColumnWriter(w) => w.get_descriptor().max_def_level(),
        ColumnWriter::Int64ColumnWriter(w) => w.get_descriptor().max_def_level(),
        ColumnWriter::Int96ColumnWriter(w) => w.get_descriptor().max_def_level(),
        ColumnWriter::FloatColumnWriter(w) => w.get_descriptor().max_def_level(),
        ColumnWriter::DoubleColumnWriter(w) => w.get_descriptor().max_def_level(),
        ColumnWriter::ByteArrayColumnWriter(w) => w.get_descriptor().max_def_level(),
        ColumnWriter::FixedLenByteArrayColumnWriter(w) => w.get_descriptor().max_def_level(),
    }
}

fn get_max_rep_level(writer: &mut SerializedColumnWriter<'_>) -> i16 {
    match writer.untyped() {
        ColumnWriter::BoolColumnWriter(w) => w.get_descriptor().max_rep_level(),
        ColumnWriter::Int32ColumnWriter(w) => w.get_descriptor().max_rep_level(),
        ColumnWriter::Int64ColumnWriter(w) => w.get_descriptor().max_rep_level(),
        ColumnWriter::Int96ColumnWriter(w) => w.get_descriptor().max_rep_level(),
        ColumnWriter::FloatColumnWriter(w) => w.get_descriptor().max_rep_level(),
        ColumnWriter::DoubleColumnWriter(w) => w.get_descriptor().max_rep_level(),
        ColumnWriter::ByteArrayColumnWriter(w) => w.get_descriptor().max_rep_level(),
        ColumnWriter::FixedLenByteArrayColumnWriter(w) => w.get_descriptor().max_rep_level(),
    }
}

enum PhysicalValues {
    Bool(Vec<bool>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    ByteArray(Vec<ByteArray>),
    FixedLenByteArray(Vec<FixedLenByteArray>),
}

struct LeafBuffer {
    def_levels: Vec<i16>,
    rep_levels: Vec<i16>,
    values: PhysicalValues,
    has_def: bool,
    has_rep: bool,
}

impl LeafBuffer {
    fn for_writer(
        writer: &mut SerializedColumnWriter<'_>,
        has_def: bool,
        has_rep: bool,
    ) -> Result<Self, QueryError> {
        let values = match writer.untyped() {
            ColumnWriter::BoolColumnWriter(_) => PhysicalValues::Bool(Vec::new()),
            ColumnWriter::Int32ColumnWriter(_) => PhysicalValues::I32(Vec::new()),
            ColumnWriter::Int64ColumnWriter(_) => PhysicalValues::I64(Vec::new()),
            ColumnWriter::DoubleColumnWriter(_) => PhysicalValues::F64(Vec::new()),
            ColumnWriter::ByteArrayColumnWriter(_) => PhysicalValues::ByteArray(Vec::new()),
            ColumnWriter::FixedLenByteArrayColumnWriter(_) => {
                PhysicalValues::FixedLenByteArray(Vec::new())
            }
            ColumnWriter::Int96ColumnWriter(_) | ColumnWriter::FloatColumnWriter(_) => {
                return Err(QueryError::internal(
                    "unexpected physical parquet column writer",
                ));
            }
        };
        Ok(Self {
            def_levels: Vec::new(),
            rep_levels: Vec::new(),
            values,
            has_def,
            has_rep,
        })
    }

    fn clear(&mut self) {
        self.def_levels.clear();
        self.rep_levels.clear();
        match &mut self.values {
            PhysicalValues::Bool(v) => v.clear(),
            PhysicalValues::I32(v) => v.clear(),
            PhysicalValues::I64(v) => v.clear(),
            PhysicalValues::F64(v) => v.clear(),
            PhysicalValues::ByteArray(v) => v.clear(),
            PhysicalValues::FixedLenByteArray(v) => v.clear(),
        }
    }

    fn push_levels(&mut self, def: i16, rep: i16) {
        if self.has_def {
            self.def_levels.push(def);
        }
        if self.has_rep {
            self.rep_levels.push(rep);
        }
    }

    fn push_bool(&mut self, v: bool) -> Result<(), QueryError> {
        let PhysicalValues::Bool(buf) = &mut self.values else {
            return Err(QueryError::internal("parquet leaf buffer type mismatch"));
        };
        buf.push(v);
        Ok(())
    }

    fn push_i32(&mut self, v: i32) -> Result<(), QueryError> {
        let PhysicalValues::I32(buf) = &mut self.values else {
            return Err(QueryError::internal("parquet leaf buffer type mismatch"));
        };
        buf.push(v);
        Ok(())
    }

    fn push_i64(&mut self, v: i64) -> Result<(), QueryError> {
        let PhysicalValues::I64(buf) = &mut self.values else {
            return Err(QueryError::internal("parquet leaf buffer type mismatch"));
        };
        buf.push(v);
        Ok(())
    }

    fn push_f64(&mut self, v: f64) -> Result<(), QueryError> {
        let PhysicalValues::F64(buf) = &mut self.values else {
            return Err(QueryError::internal("parquet leaf buffer type mismatch"));
        };
        buf.push(v);
        Ok(())
    }

    fn push_byte_array(&mut self, v: ByteArray) -> Result<(), QueryError> {
        let PhysicalValues::ByteArray(buf) = &mut self.values else {
            return Err(QueryError::internal("parquet leaf buffer type mismatch"));
        };
        buf.push(v);
        Ok(())
    }

    fn push_fixed_bytes(&mut self, v: FixedLenByteArray) -> Result<(), QueryError> {
        let PhysicalValues::FixedLenByteArray(buf) = &mut self.values else {
            return Err(QueryError::internal("parquet leaf buffer type mismatch"));
        };
        buf.push(v);
        Ok(())
    }

    fn write_to(&self, col_writer: &mut SerializedColumnWriter<'_>) -> Result<(), QueryError> {
        let def_opt = self.has_def.then_some(self.def_levels.as_slice());
        let rep_opt = self.has_rep.then_some(self.rep_levels.as_slice());
        match (col_writer.untyped(), &self.values) {
            (ColumnWriter::BoolColumnWriter(w), PhysicalValues::Bool(vals)) => {
                w.write_batch(vals, def_opt, rep_opt)
                    .map_err(|e| parquet_err(&e))?;
            }
            (ColumnWriter::Int32ColumnWriter(w), PhysicalValues::I32(vals)) => {
                w.write_batch(vals, def_opt, rep_opt)
                    .map_err(|e| parquet_err(&e))?;
            }
            (ColumnWriter::Int64ColumnWriter(w), PhysicalValues::I64(vals)) => {
                w.write_batch(vals, def_opt, rep_opt)
                    .map_err(|e| parquet_err(&e))?;
            }
            (ColumnWriter::DoubleColumnWriter(w), PhysicalValues::F64(vals)) => {
                w.write_batch(vals, def_opt, rep_opt)
                    .map_err(|e| parquet_err(&e))?;
            }
            (ColumnWriter::ByteArrayColumnWriter(w), PhysicalValues::ByteArray(vals)) => {
                w.write_batch(vals, def_opt, rep_opt)
                    .map_err(|e| parquet_err(&e))?;
            }
            (
                ColumnWriter::FixedLenByteArrayColumnWriter(w),
                PhysicalValues::FixedLenByteArray(vals),
            ) => {
                w.write_batch(vals, def_opt, rep_opt)
                    .map_err(|e| parquet_err(&e))?;
            }
            _ => {
                return Err(QueryError::internal("parquet column writer type mismatch"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct EmitLevel {
    parent_def: i16,
    parent_rep: i16,
    entry_rep: i16,
    nullable: bool,
}

fn emit_column_for_selection_chunked(
    col: &Column,
    raw_len: usize,
    selection: &Selection,
    leaf_idx: usize,
    nullable: bool,
    buf: &mut LeafBuffer,
    col_writer: &mut SerializedColumnWriter<'_>,
) -> Result<(), QueryError> {
    let lvl = EmitLevel {
        parent_def: 0,
        parent_rep: 0,
        entry_rep: 0,
        nullable,
    };
    match selection {
        Selection::All => {
            let mut start = 0usize;
            while start < raw_len {
                let end = (start + LEAF_CHUNK_ROWS).min(raw_len);
                buf.clear();
                for row in start..end {
                    emit_column_row(col, row, leaf_idx, lvl, buf)?;
                }
                buf.write_to(col_writer)?;
                start = end;
            }
        }
        Selection::Mask(mask) => {
            buf.clear();
            let mut in_buf = 0usize;
            for row in mask.iter_ones() {
                emit_column_row(col, row, leaf_idx, lvl, buf)?;
                in_buf += 1;
                if in_buf >= LEAF_CHUNK_ROWS {
                    buf.write_to(col_writer)?;
                    buf.clear();
                    in_buf = 0;
                }
            }
            if in_buf > 0 {
                buf.write_to(col_writer)?;
            }
        }
        Selection::Indices(indices) => {
            for chunk in indices.chunks(LEAF_CHUNK_ROWS) {
                buf.clear();
                for &idx in chunk {
                    emit_column_row(col, idx as usize, leaf_idx, lvl, buf)?;
                }
                buf.write_to(col_writer)?;
            }
        }
    }
    Ok(())
}

fn emit_column_row(
    col: &Column,
    row: usize,
    leaf_idx: usize,
    lvl: EmitLevel,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    match col {
        Column::Flat(flat) => emit_flat_row(flat, row, leaf_idx, lvl, buf),
        Column::Const { value, len } => {
            if row >= *len {
                return Err(QueryError::internal("const column row out of bounds"));
            }
            emit_scalar_value(value, leaf_idx, lvl, buf)
        }
        Column::Dict(dict) => {
            if row >= dict.len() {
                return Err(QueryError::internal("dict column row out of bounds"));
            }
            if dict.nulls().is_some_and(|bm| !bm.get(row)) {
                return emit_null_slot(&dict.data_type(), lvl, buf);
            }
            let code = dict.codes()[row] as usize;
            emit_flat_row(dict.values(), code, leaf_idx, lvl, buf)
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return Err(QueryError::internal("rle column row out of bounds"));
            }
            let run_idx = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            emit_flat_row(rle.values(), run_idx, leaf_idx, lvl, buf)
        }
    }
}

fn emit_null_slot(
    data_type: &DataType,
    lvl: EmitLevel,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    if !lvl.nullable && !matches!(data_type, DataType::Null) {
        return Err(QueryError::internal(
            "null value encountered in non-nullable parquet export column",
        ));
    }
    buf.push_levels(lvl.parent_def, lvl.entry_rep);
    Ok(())
}

fn emit_flat_row(
    flat: &FlatColumn,
    row: usize,
    leaf_idx: usize,
    lvl: EmitLevel,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    if row >= flat.len() {
        return Err(QueryError::internal("flat column row out of bounds"));
    }
    if flat.is_null(row) {
        return emit_null_slot(&flat.data_type(), lvl, buf);
    }

    let present_def = if lvl.nullable {
        lvl.parent_def + 1
    } else {
        lvl.parent_def
    };
    let entry_rep = lvl.entry_rep;
    let parent_rep = lvl.parent_rep;

    match flat.data() {
        FlatData::Null(_) => {
            buf.push_levels(lvl.parent_def, entry_rep);
            Ok(())
        }
        FlatData::Bool(bm) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_bool(bm.get(row))
        }
        FlatData::I64(vals) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_i64(vals[row])
        }
        FlatData::F64(vals) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_f64(vals[row])
        }
        FlatData::Decimal128 {
            values,
            precision,
            scale,
        } => {
            buf.push_levels(present_def, entry_rep);
            push_decimal_value(values[row], *precision, *scale, buf)
        }
        FlatData::TimestampNs { values, .. } => {
            buf.push_levels(present_def, entry_rep);
            buf.push_i64(values[row])
        }
        FlatData::Date(vals) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_i32(vals[row])
        }
        FlatData::Interval(vals) => {
            buf.push_levels(present_def, entry_rep);
            let s = format_interval(vals[row]);
            buf.push_byte_array(ByteArray::from(s.as_str()))
        }
        FlatData::Utf8(u) => {
            let s = u
                .get(row)
                .ok_or_else(|| QueryError::internal("utf8 row out of bounds"))?;
            buf.push_levels(present_def, entry_rep);
            buf.push_byte_array(ByteArray::from(s))
        }
        FlatData::Binary(b) => {
            let bytes = b
                .get(row)
                .ok_or_else(|| QueryError::internal("binary row out of bounds"))?;
            buf.push_levels(present_def, entry_rep);
            buf.push_byte_array(ByteArray::from(bytes))
        }
        FlatData::FixedBytes8(vals) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(
                vals[row].as_slice(),
            )))
        }
        FlatData::FixedBytes16(vals) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(
                vals[row].as_slice(),
            )))
        }
        FlatData::List {
            offsets, values, ..
        } => {
            let start = offsets[row] as usize;
            let end = offsets[row + 1] as usize;
            if start == end {
                buf.push_levels(present_def, entry_rep);
                return Ok(());
            }
            let list_elem_def = present_def + 1;
            let list_elem_rep = parent_rep + 1;
            for idx in start..end {
                let item_rep = if idx == start {
                    entry_rep
                } else {
                    list_elem_rep
                };
                emit_column_row(
                    values,
                    idx,
                    leaf_idx,
                    EmitLevel {
                        parent_def: list_elem_def,
                        parent_rep: list_elem_rep,
                        entry_rep: item_rep,
                        nullable: true,
                    },
                    buf,
                )?;
            }
            Ok(())
        }
        FlatData::MapUtf8(map) => {
            emit_map_flat_row(map, row, leaf_idx, present_def, parent_rep, entry_rep, buf)
        }
        FlatData::Events(events) => emit_events_flat_row(
            events,
            row,
            leaf_idx,
            present_def,
            parent_rep,
            entry_rep,
            buf,
        ),
        FlatData::Links(links) => emit_links_flat_row(
            links,
            row,
            leaf_idx,
            present_def,
            parent_rep,
            entry_rep,
            buf,
        ),
    }
}

fn emit_map_flat_row(
    map: &MapData,
    row: usize,
    leaf_idx: usize,
    present_def: i16,
    parent_rep: i16,
    entry_rep: i16,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    let start = map.offsets()[row] as usize;
    let end = map.offsets()[row + 1] as usize;
    if start == end {
        buf.push_levels(present_def, entry_rep);
        return Ok(());
    }
    let kv_def = present_def + 1;
    let kv_rep = parent_rep + 1;
    for idx in start..end {
        let item_rep = if idx == start { entry_rep } else { kv_rep };
        match leaf_idx {
            0 => {
                let key = map
                    .keys()
                    .get(idx)
                    .ok_or_else(|| QueryError::internal("map key out of bounds"))?;
                buf.push_levels(kv_def, item_rep);
                buf.push_byte_array(ByteArray::from(key))?;
            }
            1 => {
                if map.value_nulls().is_some_and(|bm| !bm.get(idx)) {
                    buf.push_levels(kv_def, item_rep);
                } else {
                    let val = map
                        .values()
                        .get(idx)
                        .ok_or_else(|| QueryError::internal("map value out of bounds"))?;
                    buf.push_levels(kv_def + 1, item_rep);
                    buf.push_byte_array(ByteArray::from(val))?;
                }
            }
            _ => return Err(QueryError::internal("map leaf index out of bounds")),
        }
    }
    Ok(())
}

fn emit_events_flat_row(
    events: &EventsData,
    row: usize,
    leaf_idx: usize,
    present_def: i16,
    parent_rep: i16,
    entry_rep: i16,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    let start = events.offsets()[row] as usize;
    let end = events.offsets()[row + 1] as usize;
    if start == end {
        buf.push_levels(present_def, entry_rep);
        return Ok(());
    }
    let elem_def = present_def + 1;
    let list_rep = parent_rep + 1;
    for idx in start..end {
        let item_rep = if idx == start { entry_rep } else { list_rep };
        match leaf_idx {
            0 => {
                buf.push_levels(elem_def, item_rep);
                buf.push_i64(events.times()[idx])?;
            }
            1 => {
                let name = events
                    .names()
                    .get(idx)
                    .ok_or_else(|| QueryError::internal("event name out of bounds"))?;
                buf.push_levels(elem_def, item_rep);
                buf.push_byte_array(ByteArray::from(name))?;
            }
            2 => {
                if events.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
                    buf.push_levels(elem_def, item_rep);
                } else {
                    let attrs = events
                        .attributes()
                        .get(idx)
                        .ok_or_else(|| QueryError::internal("event attributes out of bounds"))?;
                    buf.push_levels(elem_def + 1, item_rep);
                    buf.push_byte_array(ByteArray::from(attrs))?;
                }
            }
            _ => return Err(QueryError::internal("events leaf index out of bounds")),
        }
    }
    Ok(())
}

fn emit_links_flat_row(
    links: &LinksData,
    row: usize,
    leaf_idx: usize,
    present_def: i16,
    parent_rep: i16,
    entry_rep: i16,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    let start = links.offsets()[row] as usize;
    let end = links.offsets()[row + 1] as usize;
    if start == end {
        buf.push_levels(present_def, entry_rep);
        return Ok(());
    }
    let elem_def = present_def + 1;
    let list_rep = parent_rep + 1;
    for idx in start..end {
        let item_rep = if idx == start { entry_rep } else { list_rep };
        match leaf_idx {
            0 => {
                buf.push_levels(elem_def, item_rep);
                buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(
                    links.trace_ids()[idx].as_slice(),
                )))?;
            }
            1 => {
                buf.push_levels(elem_def, item_rep);
                buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(
                    links.span_ids()[idx].as_slice(),
                )))?;
            }
            2 => {
                if links.attribute_nulls().is_some_and(|bm| !bm.get(idx)) {
                    buf.push_levels(elem_def, item_rep);
                } else {
                    let attrs = links
                        .attributes()
                        .get(idx)
                        .ok_or_else(|| QueryError::internal("link attributes out of bounds"))?;
                    buf.push_levels(elem_def + 1, item_rep);
                    buf.push_byte_array(ByteArray::from(attrs))?;
                }
            }
            _ => return Err(QueryError::internal("links leaf index out of bounds")),
        }
    }
    Ok(())
}

fn emit_scalar_value(
    value: &ScalarValue,
    leaf_idx: usize,
    lvl: EmitLevel,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    if let ScalarValue::Null(dt) = value {
        return emit_null_slot(dt, lvl, buf);
    }

    let present_def = if lvl.nullable {
        lvl.parent_def + 1
    } else {
        lvl.parent_def
    };
    let entry_rep = lvl.entry_rep;
    let parent_rep = lvl.parent_rep;

    match value {
        ScalarValue::Null(_) => unreachable!(),
        ScalarValue::Bool(v) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_bool(*v)
        }
        ScalarValue::I64(v) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_i64(*v)
        }
        ScalarValue::F64(v) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_f64(*v)
        }
        ScalarValue::Decimal128 {
            value,
            precision,
            scale,
        } => {
            buf.push_levels(present_def, entry_rep);
            push_decimal_value(*value, *precision, *scale, buf)
        }
        ScalarValue::TimestampNs { value, .. } => {
            buf.push_levels(present_def, entry_rep);
            buf.push_i64(*value)
        }
        ScalarValue::Date(days) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_i32(*days)
        }
        ScalarValue::Interval(iv) => {
            buf.push_levels(present_def, entry_rep);
            let s = format_interval(*iv);
            buf.push_byte_array(ByteArray::from(s.as_str()))
        }
        ScalarValue::Utf8(s) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_byte_array(ByteArray::from(s.as_str()))
        }
        ScalarValue::Binary(b) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_byte_array(ByteArray::from(b.as_slice()))
        }
        ScalarValue::FixedBytes8(b) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(b.as_slice())))
        }
        ScalarValue::FixedBytes16(b) => {
            buf.push_levels(present_def, entry_rep);
            buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(b.as_slice())))
        }
        ScalarValue::List { values, .. } => {
            if values.is_empty() {
                buf.push_levels(present_def, entry_rep);
                return Ok(());
            }
            let list_elem_def = present_def + 1;
            let list_elem_rep = parent_rep + 1;
            for (idx, item) in values.iter().enumerate() {
                let item_rep = if idx == 0 { entry_rep } else { list_elem_rep };
                emit_scalar_value(
                    item,
                    leaf_idx,
                    EmitLevel {
                        parent_def: list_elem_def,
                        parent_rep: list_elem_rep,
                        entry_rep: item_rep,
                        nullable: true,
                    },
                    buf,
                )?;
            }
            Ok(())
        }
        ScalarValue::MapUtf8(entries) => {
            emit_map_scalar_entries(entries, leaf_idx, present_def, parent_rep, entry_rep, buf)
        }
        ScalarValue::Events(items) => {
            emit_events_scalar_items(items, leaf_idx, present_def, parent_rep, entry_rep, buf)
        }
        ScalarValue::Links(items) => {
            emit_links_scalar_items(items, leaf_idx, present_def, parent_rep, entry_rep, buf)
        }
    }
}

fn emit_map_scalar_entries(
    entries: &[(String, Option<String>)],
    leaf_idx: usize,
    present_def: i16,
    parent_rep: i16,
    entry_rep: i16,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    if entries.is_empty() {
        buf.push_levels(present_def, entry_rep);
        return Ok(());
    }
    let kv_def = present_def + 1;
    let kv_rep = parent_rep + 1;
    for (idx, (k, v)) in entries.iter().enumerate() {
        let item_rep = if idx == 0 { entry_rep } else { kv_rep };
        match leaf_idx {
            0 => {
                buf.push_levels(kv_def, item_rep);
                buf.push_byte_array(ByteArray::from(k.as_str()))?;
            }
            1 => match v {
                None => buf.push_levels(kv_def, item_rep),
                Some(val) => {
                    buf.push_levels(kv_def + 1, item_rep);
                    buf.push_byte_array(ByteArray::from(val.as_str()))?;
                }
            },
            _ => return Err(QueryError::internal("map leaf index out of bounds")),
        }
    }
    Ok(())
}

fn emit_events_scalar_items(
    items: &[EventItem],
    leaf_idx: usize,
    present_def: i16,
    parent_rep: i16,
    entry_rep: i16,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    if items.is_empty() {
        buf.push_levels(present_def, entry_rep);
        return Ok(());
    }
    let elem_def = present_def + 1;
    let list_rep = parent_rep + 1;
    for (idx, ev) in items.iter().enumerate() {
        let item_rep = if idx == 0 { entry_rep } else { list_rep };
        match leaf_idx {
            0 => {
                buf.push_levels(elem_def, item_rep);
                buf.push_i64(ev.time)?;
            }
            1 => {
                buf.push_levels(elem_def, item_rep);
                buf.push_byte_array(ByteArray::from(ev.name.as_str()))?;
            }
            2 => match &ev.attributes {
                None => buf.push_levels(elem_def, item_rep),
                Some(attrs) => {
                    buf.push_levels(elem_def + 1, item_rep);
                    buf.push_byte_array(ByteArray::from(attrs.as_str()))?;
                }
            },
            _ => return Err(QueryError::internal("events leaf index out of bounds")),
        }
    }
    Ok(())
}

fn emit_links_scalar_items(
    items: &[LinkItem],
    leaf_idx: usize,
    present_def: i16,
    parent_rep: i16,
    entry_rep: i16,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    if items.is_empty() {
        buf.push_levels(present_def, entry_rep);
        return Ok(());
    }
    let elem_def = present_def + 1;
    let list_rep = parent_rep + 1;
    for (idx, lk) in items.iter().enumerate() {
        let item_rep = if idx == 0 { entry_rep } else { list_rep };
        match leaf_idx {
            0 => {
                buf.push_levels(elem_def, item_rep);
                buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(
                    lk.trace_id.as_slice(),
                )))?;
            }
            1 => {
                buf.push_levels(elem_def, item_rep);
                buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(
                    lk.span_id.as_slice(),
                )))?;
            }
            2 => match &lk.attributes {
                None => buf.push_levels(elem_def, item_rep),
                Some(attrs) => {
                    buf.push_levels(elem_def + 1, item_rep);
                    buf.push_byte_array(ByteArray::from(attrs.as_str()))?;
                }
            },
            _ => return Err(QueryError::internal("links leaf index out of bounds")),
        }
    }
    Ok(())
}

fn push_decimal_value(
    value: i128,
    precision: u8,
    scale: i8,
    buf: &mut LeafBuffer,
) -> Result<(), QueryError> {
    if scale >= 0 {
        let bytes = value.to_be_bytes();
        return buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(bytes.as_slice())));
    }
    let rescaled_prec = i32::from(precision) - i32::from(scale);
    if rescaled_prec <= 38 {
        let factor = 10_i128
            .checked_pow(u32::from(scale.unsigned_abs()))
            .ok_or_else(|| QueryError::failed("decimal scale power overflow"))?;
        let rescaled = value
            .checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal rescale overflow during parquet export"))?;
        let bytes = rescaled.to_be_bytes();
        buf.push_fixed_bytes(FixedLenByteArray::from(ByteArray::from(bytes.as_slice())))
    } else {
        let text = format_decimal_neg_scale(value, scale);
        buf.push_byte_array(ByteArray::from(text.as_str()))
    }
}

fn format_decimal_neg_scale(value: i128, scale: i8) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let mut s = value.to_string();
    let zeros = usize::from(scale.unsigned_abs());
    s.reserve(zeros);
    for _ in 0..zeros {
        s.push('0');
    }
    s
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use cetra_column::{
        Batch, BinaryData, Bitmap, Column, DataType, DictColumn, EventsData, Field, FlatColumn,
        FlatData, GlobalPool, Interval, LinksData, MapData, RleColumn, ScalarValue, Schema,
        ScopedPool, Selection, Utf8Data,
    };
    use parquet::basic::{Compression, LogicalType, TimeUnit};
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::{Field as ParquetField, Row, RowAccessor};

    use super::*;

    fn export_batches(
        schema: Arc<Schema>,
        batches: Vec<Batch>,
        pool: &Arc<ScopedPool>,
    ) -> Result<Vec<u8>, QueryError> {
        export_batches_with_limits(
            schema,
            batches,
            pool,
            PARQUET_ROW_GROUP_ROWS,
            PARQUET_CHUNK_BYTES,
        )
    }

    fn export_batches_with_limits(
        schema: Arc<Schema>,
        batches: Vec<Batch>,
        pool: &Arc<ScopedPool>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Vec<u8>, QueryError> {
        let mut exporter =
            ParquetBatchExporter::with_row_group_limits(schema, pool, max_rows, max_bytes)?;
        let mut out = Vec::new();
        for b in batches {
            for chunk in exporter.push_batch(b, None)? {
                out.extend_from_slice(&chunk);
            }
        }
        for tail in exporter.finish()? {
            out.extend_from_slice(&tail);
        }
        Ok(out)
    }

    fn read_low_level(bytes: &[u8]) -> SerializedFileReader<Bytes> {
        SerializedFileReader::new(Bytes::copy_from_slice(bytes))
            .expect("valid parquet for low-level reader")
    }

    fn read_rows(bytes: &[u8]) -> Vec<Row> {
        read_low_level(bytes)
            .get_row_iter(None)
            .expect("row iter")
            .map(|r| r.expect("row"))
            .collect()
    }

    fn field_at(row: &Row, idx: usize) -> &ParquetField {
        row.get_column_iter().nth(idx).expect("column at idx").1
    }

    fn decimal_i128(field: &ParquetField) -> i128 {
        let ParquetField::Decimal(dec) = field else {
            panic!("expected Decimal, got {field:?}");
        };
        let raw = dec.data();
        assert_eq!(raw.len(), 16, "expected 16-byte FixedLenByteArray decimal");
        i128::from_be_bytes(raw.try_into().unwrap())
    }

    fn timestamp_ns(field: &ParquetField) -> i64 {
        match field {
            ParquetField::Long(v) => *v,
            other => panic!("expected timestamp i64, got {other:?}"),
        }
    }

    fn date_val(field: &ParquetField) -> i32 {
        match field {
            ParquetField::Date(v) => *v,
            other => panic!("expected Date, got {other:?}"),
        }
    }

    fn list_elements(field: &ParquetField) -> &[ParquetField] {
        match field {
            ParquetField::ListInternal(list) => list.elements(),
            other => panic!("expected ListInternal, got {other:?}"),
        }
    }

    fn map_entries(field: &ParquetField) -> &[(ParquetField, ParquetField)] {
        match field {
            ParquetField::MapInternal(map) => map.entries(),
            other => panic!("expected MapInternal, got {other:?}"),
        }
    }

    fn struct_row(field: &ParquetField) -> &Row {
        match field {
            ParquetField::Group(row) => row,
            other => panic!("expected Group, got {other:?}"),
        }
    }

    fn str_val(field: &ParquetField) -> &str {
        match field {
            ParquetField::Str(s) => s.as_str(),
            other => panic!("expected Str, got {other:?}"),
        }
    }

    fn bytes_val(field: &ParquetField) -> &[u8] {
        match field {
            ParquetField::Bytes(b) => b.data(),
            other => panic!("expected Bytes, got {other:?}"),
        }
    }

    #[test]
    fn zero_column_schema_is_rejected() {
        let pool = GlobalPool::standalone(1024 * 1024);
        let schema = Schema::try_new_arc(vec![]).unwrap();
        let err = ParquetBatchExporter::new(schema, &pool).unwrap_err();
        assert!(matches!(err, QueryError::Failed(ref msg) if msg.contains("zero-column")));
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn zero_row_export_writes_valid_empty_parquet() {
        let pool = GlobalPool::standalone(1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
        ])
        .unwrap();

        let empty_batch = Batch::empty(Arc::clone(&schema));
        let bytes = export_batches(Arc::clone(&schema), vec![empty_batch], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);

        let ll = read_low_level(&bytes);
        assert_eq!(ll.metadata().num_row_groups(), 0);
        assert_eq!(ll.metadata().file_metadata().num_rows(), 0);
        assert_eq!(
            ll.metadata().file_metadata().schema_descr().num_columns(),
            3
        );
        assert!(read_rows(&bytes).is_empty());

        // Also verify 0 push_batch calls at all.
        let bytes_no_push = export_batches(schema, vec![], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);
        let ll2 = read_low_level(&bytes_no_push);
        assert_eq!(ll2.metadata().num_row_groups(), 0);
        assert!(read_rows(&bytes_no_push).is_empty());
    }

    #[test]
    fn round_trip_all_primitive_and_temporal_types() {
        let pool = GlobalPool::standalone(16 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("b", DataType::Bool, true),
            Field::new("i", DataType::I64, true),
            Field::new("f", DataType::F64, true),
            Field::new(
                "dec_pos",
                DataType::Decimal128 {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            Field::new(
                "dec_neg_rescale",
                DataType::Decimal128 {
                    precision: 10,
                    scale: -2,
                },
                true,
            ),
            Field::new(
                "dec_neg_str",
                DataType::Decimal128 {
                    precision: 38,
                    scale: -4,
                },
                true,
            ),
            Field::new("ts_utc", DataType::TimestampNs { utc: true }, true),
            Field::new("ts_naive", DataType::TimestampNs { utc: false }, true),
            Field::new("d", DataType::Date, true),
            Field::new("iv", DataType::Interval, true),
            Field::new("s", DataType::Utf8, true),
            Field::new("bin", DataType::Binary, true),
            Field::new("fb8", DataType::FixedBytes8, true),
            Field::new("fb16", DataType::FixedBytes16, true),
            Field::new("n", DataType::Null, true),
        ])
        .unwrap();

        let nulls = Some(Bitmap::from_bools(&[true, false, true, true]));
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Bool(Bitmap::from_bools(&[true, false, false, true])),
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![-42, 0, 99, i64::MAX]), nulls.clone())
                        .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![1.5, 0.0, f64::NAN, f64::INFINITY]),
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![12_345, 0, -98_765, 1],
                            precision: 10,
                            scale: 2,
                        },
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![42, 0, -7, 123],
                            precision: 10,
                            scale: -2,
                        },
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![42, 0, -9, 1_000_000_000_000],
                            precision: 38,
                            scale: -4,
                        },
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![1_700_000_000_123_456_789, 0, -1, 42],
                            utc: true,
                        },
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![1_700_000_000_000_000_000, 0, -100, 7],
                            utc: false,
                        },
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::Date(vec![0, 0, 19700, -1]), nulls.clone())
                        .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Interval(vec![
                            Interval {
                                months: 1,
                                days: -2,
                                nanos: 300,
                            },
                            Interval::default(),
                            Interval {
                                months: 0,
                                days: 0,
                                nanos: 0,
                            },
                            Interval {
                                months: -1,
                                days: -1,
                                nanos: -600_000_000_000,
                            },
                        ]),
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["hello", "", "cetra", "🚀"]).unwrap(),
                        ),
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Binary(
                            BinaryData::try_from_slices([
                                b"\x00\x01".as_slice(),
                                b"".as_slice(),
                                b"\xff\xfe\xfd".as_slice(),
                                b"abc".as_slice(),
                            ])
                            .unwrap(),
                        ),
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes8(vec![[1; 8], [0; 8], [2; 8], [3; 8]]),
                        nulls.clone(),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16(vec![[0xaa; 16], [0; 16], [0xbb; 16], [0xcc; 16]]),
                        nulls,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::Null(4), None).unwrap()),
            ],
            4,
        )
        .unwrap();

        let bytes = export_batches(schema, vec![batch], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);

        // Low-level Parquet reader verification (compression, schema metadata, row count, values).
        let ll = read_low_level(&bytes);
        assert_eq!(ll.metadata().num_row_groups(), 1);
        let rg_meta = ll.metadata().row_group(0);
        assert_eq!(rg_meta.num_rows(), 4);
        for c in rg_meta.columns() {
            assert_eq!(c.compression(), Compression::ZSTD(ZstdLevel::default()));
        }
        let schema_descr = ll.metadata().file_metadata().schema_descr();
        assert_eq!(
            schema_descr.column(3).logical_type_ref(),
            Some(&LogicalType::decimal(2, 10))
        );
        assert_eq!(
            schema_descr.column(4).logical_type_ref(),
            Some(&LogicalType::decimal(0, 12))
        );
        assert_eq!(
            schema_descr.column(6).logical_type_ref(),
            Some(&LogicalType::timestamp(true, TimeUnit::NANOS))
        );
        assert_eq!(
            schema_descr.column(7).logical_type_ref(),
            Some(&LogicalType::timestamp(false, TimeUnit::NANOS))
        );

        let rows = read_rows(&bytes);
        assert_eq!(rows.len(), 4);

        // col 0: bool
        assert!(rows[0].get_bool(0).unwrap());
        assert_eq!(field_at(&rows[1], 0), &ParquetField::Null);
        assert!(!rows[2].get_bool(0).unwrap());
        assert!(rows[3].get_bool(0).unwrap());

        // col 1: i64
        assert_eq!(rows[0].get_long(1).unwrap(), -42);
        assert_eq!(field_at(&rows[1], 1), &ParquetField::Null);
        assert_eq!(rows[2].get_long(1).unwrap(), 99);
        assert_eq!(rows[3].get_long(1).unwrap(), i64::MAX);

        // col 2: f64
        assert_eq!(rows[0].get_double(2).unwrap(), 1.5);
        assert_eq!(field_at(&rows[1], 2), &ParquetField::Null);
        assert!(rows[2].get_double(2).unwrap().is_nan());
        assert_eq!(rows[3].get_double(2).unwrap(), f64::INFINITY);

        // col 3: dec_pos (10, 2)
        assert_eq!(decimal_i128(field_at(&rows[0], 3)), 12_345);
        assert_eq!(field_at(&rows[1], 3), &ParquetField::Null);
        assert_eq!(decimal_i128(field_at(&rows[2], 3)), -98_765);

        // col 4: dec_neg_rescale (12, 0)
        assert_eq!(decimal_i128(field_at(&rows[0], 4)), 4200);
        assert_eq!(field_at(&rows[1], 4), &ParquetField::Null);
        assert_eq!(decimal_i128(field_at(&rows[2], 4)), -700);
        assert_eq!(decimal_i128(field_at(&rows[3], 4)), 12300);

        // col 5: dec_neg_str
        assert_eq!(rows[0].get_string(5).unwrap(), "420000");
        assert_eq!(field_at(&rows[1], 5), &ParquetField::Null);
        assert_eq!(rows[2].get_string(5).unwrap(), "-90000");
        assert_eq!(rows[3].get_string(5).unwrap(), "10000000000000000");

        // col 6: ts_utc
        assert_eq!(
            timestamp_ns(field_at(&rows[0], 6)),
            1_700_000_000_123_456_789
        );
        assert_eq!(field_at(&rows[1], 6), &ParquetField::Null);
        assert_eq!(timestamp_ns(field_at(&rows[2], 6)), -1);

        // col 7: ts_naive
        assert_eq!(timestamp_ns(field_at(&rows[2], 7)), -100);

        // col 8: date
        assert_eq!(date_val(field_at(&rows[0], 8)), 0);
        assert_eq!(field_at(&rows[1], 8), &ParquetField::Null);
        assert_eq!(date_val(field_at(&rows[2], 8)), 19700);
        assert_eq!(date_val(field_at(&rows[3], 8)), -1);

        // col 9: interval (formatted string)
        assert_eq!(
            rows[0].get_string(9).unwrap(),
            "1 mons -2 days 0.000000300 secs"
        );
        assert_eq!(field_at(&rows[1], 9), &ParquetField::Null);
        assert_eq!(rows[2].get_string(9).unwrap(), "0 secs");
        assert_eq!(rows[3].get_string(9).unwrap(), "-1 mons -1 days -10 mins");

        // col 10: utf8
        assert_eq!(rows[0].get_string(10).unwrap(), "hello");
        assert_eq!(field_at(&rows[1], 10), &ParquetField::Null);
        assert_eq!(rows[2].get_string(10).unwrap(), "cetra");
        assert_eq!(rows[3].get_string(10).unwrap(), "🚀");

        // col 11: binary
        assert_eq!(bytes_val(field_at(&rows[0], 11)), b"\x00\x01");
        assert_eq!(field_at(&rows[1], 11), &ParquetField::Null);
        assert_eq!(bytes_val(field_at(&rows[2], 11)), b"\xff\xfe\xfd");

        // col 12: fb8
        assert_eq!(bytes_val(field_at(&rows[0], 12)), &[1; 8]);
        assert_eq!(field_at(&rows[1], 12), &ParquetField::Null);
        assert_eq!(bytes_val(field_at(&rows[2], 12)), &[2; 8]);

        // col 13: fb16
        assert_eq!(bytes_val(field_at(&rows[0], 13)), &[0xaa; 16]);
        assert_eq!(field_at(&rows[1], 13), &ParquetField::Null);
        assert_eq!(bytes_val(field_at(&rows[3], 13)), &[0xcc; 16]);

        // col 14: null
        for row in &rows {
            assert_eq!(field_at(row, 14), &ParquetField::Null);
        }
    }

    #[test]
    fn round_trip_nested_lists_maps_events_and_links() {
        let pool = GlobalPool::standalone(16 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("list_utf8", DataType::List(Box::new(DataType::Utf8)), true),
            Field::new(
                "nested_list",
                DataType::List(Box::new(DataType::List(Box::new(DataType::Utf8)))),
                true,
            ),
            Field::new("attrs", DataType::MapUtf8, true),
            Field::new("events", DataType::Events, true),
            Field::new("links", DataType::Links, true),
        ])
        .unwrap();

        // 5 rows exercising: null, empty, mixed null elements, multi-element, nested empty/null.
        let row_nulls = Some(Bitmap::from_bools(&[false, true, true, true, true]));

        // 1. list_utf8: [NULL, [], [NULL], ["a", NULL, "b"], ["z"]]
        let list_inner = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["", "a", "", "b", "z"]).unwrap()),
                Some(Bitmap::from_bools(&[false, true, false, true, true])),
            )
            .unwrap(),
        );
        let list_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 0, 0, 1, 4, 5],
                    values: Box::new(list_inner),
                },
                row_nulls.clone(),
            )
            .unwrap(),
        );

        // 2. nested_list: [NULL, [], [NULL, []], [["x", NULL], [], ["y"]], [["z"]]]
        let inner_strs = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["x", "", "y", "z"]).unwrap()),
                Some(Bitmap::from_bools(&[true, false, true, true])),
            )
            .unwrap(),
        );
        // inner lists (6 slots): [NULL, [], ["x", NULL], [], ["y"], ["z"]]
        let inner_lists = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 0, 0, 2, 2, 3, 4],
                    values: Box::new(inner_strs),
                },
                Some(Bitmap::from_bools(&[false, true, true, true, true, true])),
            )
            .unwrap(),
        );
        let nested_list_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::List(Box::new(DataType::Utf8)),
                    offsets: vec![0, 0, 0, 2, 5, 6],
                    values: Box::new(inner_lists),
                },
                row_nulls.clone(),
            )
            .unwrap(),
        );

        // 3. attrs: [NULL, {}, {"k1": "v1", "k2": NULL}, {"a": "b"}, {}]
        let map_data = MapData::try_new(
            vec![0, 0, 0, 2, 3, 3],
            Utf8Data::try_from_slices(["k1", "k2", "a"]).unwrap(),
            Utf8Data::try_from_slices(["v1", "", "b"]).unwrap(),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let map_col = Column::Flat(
            FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), row_nulls.clone()).unwrap(),
        );

        // 4. events: [NULL, [], [ev1(no attrs), ev2(attrs)], [ev3], []]
        let events_data = EventsData::try_new(
            vec![0, 0, 0, 2, 3, 3],
            vec![100, 200, 300],
            Utf8Data::try_from_slices(["ev1", "ev2", "ev3"]).unwrap(),
            Utf8Data::try_from_slices(["", r#"{"x":1}"#, ""]).unwrap(),
            Some(Bitmap::from_bools(&[false, true, false])),
        )
        .unwrap();
        let events_col = Column::Flat(
            FlatColumn::try_new(FlatData::Events(Box::new(events_data)), row_nulls.clone())
                .unwrap(),
        );

        // 5. links: [NULL, [], [lk1(attrs), lk2(no attrs)], [], [lk3]]
        let links_data = LinksData::try_new(
            vec![0, 0, 0, 2, 2, 3],
            vec![[1; 16], [2; 16], [3; 16]],
            vec![[0x11; 8], [0x22; 8], [0x33; 8]],
            Utf8Data::try_from_slices([r#"{"r":"1"}"#, "", r#"{"r":"3"}"#]).unwrap(),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let links_col = Column::Flat(
            FlatColumn::try_new(FlatData::Links(Box::new(links_data)), row_nulls).unwrap(),
        );

        let batch = Batch::try_new_all(
            schema,
            vec![list_col, nested_list_col, map_col, events_col, links_col],
            5,
        )
        .unwrap();

        let bytes = export_batches(batch.schema().clone(), vec![batch], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);

        let ll = read_low_level(&bytes);
        assert_eq!(ll.metadata().row_group(0).num_rows(), 5);
        let rows = read_rows(&bytes);
        assert_eq!(rows.len(), 5);

        // Verify list_utf8: [NULL, [], [NULL], ["a", NULL, "b"], ["z"]]
        assert_eq!(field_at(&rows[0], 0), &ParquetField::Null);
        assert!(list_elements(field_at(&rows[1], 0)).is_empty());
        assert_eq!(list_elements(field_at(&rows[2], 0)), &[ParquetField::Null]);
        let r3 = list_elements(field_at(&rows[3], 0));
        assert_eq!(r3.len(), 3);
        assert_eq!(str_val(&r3[0]), "a");
        assert_eq!(r3[1], ParquetField::Null);
        assert_eq!(str_val(&r3[2]), "b");

        // Verify nested_list: [NULL, [], [NULL, []], [["x", NULL], [], ["y"]], [["z"]]]
        assert_eq!(field_at(&rows[0], 1), &ParquetField::Null);
        assert!(list_elements(field_at(&rows[1], 1)).is_empty());
        let l2_r2 = list_elements(field_at(&rows[2], 1));
        assert_eq!(l2_r2.len(), 2);
        assert_eq!(l2_r2[0], ParquetField::Null);
        assert!(list_elements(&l2_r2[1]).is_empty());

        let l2_r3 = list_elements(field_at(&rows[3], 1));
        assert_eq!(l2_r3.len(), 3);
        let r3_0 = list_elements(&l2_r3[0]);
        assert_eq!(str_val(&r3_0[0]), "x");
        assert_eq!(r3_0[1], ParquetField::Null);
        assert!(list_elements(&l2_r3[1]).is_empty());
        assert_eq!(str_val(&list_elements(&l2_r3[2])[0]), "y");

        // Verify MapUtf8: [NULL, {}, {"k1": "v1", "k2": NULL}, {"a": "b"}, {}]
        assert_eq!(field_at(&rows[0], 2), &ParquetField::Null);
        assert!(map_entries(field_at(&rows[1], 2)).is_empty());
        let m_r2 = map_entries(field_at(&rows[2], 2));
        assert_eq!(m_r2.len(), 2);
        assert_eq!(str_val(&m_r2[0].0), "k1");
        assert_eq!(str_val(&m_r2[0].1), "v1");
        assert_eq!(str_val(&m_r2[1].0), "k2");
        assert_eq!(m_r2[1].1, ParquetField::Null);

        // Verify Events: [NULL, [], [ev1(no attrs), ev2(attrs)], [ev3], []]
        assert_eq!(field_at(&rows[0], 3), &ParquetField::Null);
        assert!(list_elements(field_at(&rows[1], 3)).is_empty());
        let ev_r2 = list_elements(field_at(&rows[2], 3));
        assert_eq!(ev_r2.len(), 2);
        let ev0 = struct_row(&ev_r2[0]);
        assert_eq!(timestamp_ns(field_at(ev0, 0)), 100);
        assert_eq!(str_val(field_at(ev0, 1)), "ev1");
        assert_eq!(field_at(ev0, 2), &ParquetField::Null);
        let ev1 = struct_row(&ev_r2[1]);
        assert_eq!(timestamp_ns(field_at(ev1, 0)), 200);
        assert_eq!(str_val(field_at(ev1, 1)), "ev2");
        assert_eq!(str_val(field_at(ev1, 2)), r#"{"x":1}"#);

        // Verify Links: [NULL, [], [lk1(attrs), lk2(no attrs)], [], [lk3]]
        assert_eq!(field_at(&rows[0], 4), &ParquetField::Null);
        assert!(list_elements(field_at(&rows[1], 4)).is_empty());
        let lk_r2 = list_elements(field_at(&rows[2], 4));
        assert_eq!(lk_r2.len(), 2);
        let lk0 = struct_row(&lk_r2[0]);
        assert_eq!(bytes_val(field_at(lk0, 0)), &[1; 16]);
        assert_eq!(bytes_val(field_at(lk0, 1)), &[0x11; 8]);
        assert_eq!(str_val(field_at(lk0, 2)), r#"{"r":"1"}"#);
        let lk1 = struct_row(&lk_r2[1]);
        assert_eq!(bytes_val(field_at(lk1, 0)), &[2; 16]);
        assert_eq!(bytes_val(field_at(lk1, 1)), &[0x22; 8]);
        assert_eq!(field_at(lk1, 2), &ParquetField::Null);
    }

    #[test]
    fn round_trip_all_encodings_and_selections() {
        let pool = GlobalPool::standalone(16 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("flat_i64", DataType::I64, false),
            Field::new("const_str", DataType::Utf8, true),
            Field::new("dict_str", DataType::Utf8, true),
            Field::new("rle_i64", DataType::I64, true),
            Field::new("const_null", DataType::I64, true),
            Field::new(
                "const_list",
                DataType::List(Box::new(DataType::Utf8)),
                false,
            ),
        ])
        .unwrap();

        let raw_len = 6;
        let flat_col = Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![10, 20, 30, 40, 50, 60]), None).unwrap(),
        );
        let const_str = Column::try_new_const(ScalarValue::Utf8("shared".into()), raw_len).unwrap();
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
            None,
        )
        .unwrap();
        let dict_col = Column::Dict(
            DictColumn::try_new(
                dict_vals,
                vec![0, 1, 0, 0, 1, 0],
                Some(Bitmap::from_bools(&[true, false, true, true, true, false])),
            )
            .unwrap(),
        );
        let rle_vals = FlatColumn::try_new(
            FlatData::I64(vec![100, 0, 300]),
            Some(Bitmap::from_bools(&[true, false, true])),
        )
        .unwrap();
        let rle_col = Column::Rle(RleColumn::try_new(rle_vals, vec![2, 4, 6]).unwrap());
        let const_null = Column::try_new_const(ScalarValue::Null(DataType::I64), raw_len).unwrap();
        let const_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("x".into()),
                    ScalarValue::Null(DataType::Utf8),
                ],
            },
            raw_len,
        )
        .unwrap();

        let cols = vec![
            flat_col, const_str, dict_col, rle_col, const_null, const_list,
        ];

        // Batch 1: Selection::Mask keeping rows [0, 2, 4, 5] -> flat=[10, 30, 50, 60]
        let b_mask = Batch::try_new(
            Arc::clone(&schema),
            cols.clone(),
            raw_len,
            Selection::Mask(Bitmap::from_bools(&[true, false, true, false, true, true])),
        )
        .unwrap();

        // Batch 2: Selection::Indices [5, 1, 3] -> flat=[60, 20, 40]
        let b_indices = Batch::try_new(
            Arc::clone(&schema),
            cols,
            raw_len,
            Selection::Indices(vec![5, 1, 3]),
        )
        .unwrap();

        let bytes = export_batches(schema, vec![b_mask, b_indices], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);

        let rows = read_rows(&bytes);
        assert_eq!(rows.len(), 7);

        let flat_vals: Vec<i64> = rows.iter().map(|r| r.get_long(0).unwrap()).collect();
        assert_eq!(flat_vals, vec![10, 30, 50, 60, 60, 20, 40]);

        for row in &rows {
            assert_eq!(row.get_string(1).unwrap(), "shared");
        }

        // Selected rows: [0, 2, 4, 5] + [5, 1, 3]
        assert_eq!(rows[0].get_string(2).unwrap(), "alpha");
        assert_eq!(rows[1].get_string(2).unwrap(), "alpha");
        assert_eq!(rows[2].get_string(2).unwrap(), "beta");
        assert_eq!(field_at(&rows[3], 2), &ParquetField::Null);
        assert_eq!(field_at(&rows[4], 2), &ParquetField::Null);
        assert_eq!(field_at(&rows[5], 2), &ParquetField::Null);
        assert_eq!(rows[6].get_string(2).unwrap(), "alpha");

        // run 0..2 => 100, run 2..4 => NULL, run 4..6 => 300
        // rows [0, 2, 4, 5, 5, 1, 3] => [100, NULL, 300, 300, 300, 100, NULL]
        assert_eq!(rows[0].get_long(3).unwrap(), 100);
        assert_eq!(field_at(&rows[1], 3), &ParquetField::Null);
        assert_eq!(rows[2].get_long(3).unwrap(), 300);
        assert_eq!(rows[3].get_long(3).unwrap(), 300);
        assert_eq!(rows[4].get_long(3).unwrap(), 300);
        assert_eq!(rows[5].get_long(3).unwrap(), 100);
        assert_eq!(field_at(&rows[6], 3), &ParquetField::Null);

        for row in &rows {
            assert_eq!(field_at(row, 4), &ParquetField::Null);
        }

        for row in &rows {
            let elems = list_elements(field_at(row, 5));
            assert_eq!(elems.len(), 2);
            assert_eq!(str_val(&elems[0]), "x");
            assert_eq!(elems[1], ParquetField::Null);
        }
    }

    #[test]
    fn multi_row_group_streaming_flush_and_reservation_release() {
        let pool = GlobalPool::standalone(1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("n", DataType::I64, false),
            Field::new("s", DataType::Utf8, true),
        ])
        .unwrap();

        let make_batch = |start: i64, count: usize| {
            let nums: Vec<i64> = (start..start + count as i64).collect();
            let strs: Vec<String> = nums.iter().map(|n| format!("row-{n}")).collect();
            let str_refs: Vec<&str> = strs.iter().map(String::as_str).collect();
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(FlatColumn::try_new(FlatData::I64(nums), None).unwrap()),
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(str_refs).unwrap()),
                            None,
                        )
                        .unwrap(),
                    ),
                ],
                count,
            )
            .unwrap()
        };

        // Trigger row-group flush at 5 rows (while still under huge byte cap).
        let mut exporter =
            ParquetBatchExporter::with_row_group_limits(Arc::clone(&schema), &pool, 5, 1024 * 1024)
                .unwrap();
        let mut stream_bytes = Vec::new();

        // Push 3 rows -> buffered (< 5), reservation > 0, no chunk emitted.
        let c1 = exporter.push_batch(make_batch(0, 3), None).unwrap();
        assert!(c1.is_empty());
        assert!(pool.reserved() > 0);

        // Push 3 more rows -> total 6 >= 5 -> flushes row group 0 & releases reservation!
        let c2 = exporter.push_batch(make_batch(3, 3), None).unwrap();
        assert_eq!(c2.len(), 1);
        assert!(!c2[0].is_empty());
        assert_eq!(pool.reserved(), 0);
        stream_bytes.extend_from_slice(&c2[0]);

        // Push 2 rows -> buffered, then trigger byte threshold flush on a second exporter.
        let c3 = exporter.push_batch(make_batch(6, 2), None).unwrap();
        assert!(c3.is_empty());
        assert!(pool.reserved() > 0);

        let tails = exporter.finish().unwrap();
        assert_eq!(pool.reserved(), 0);
        for tail in tails {
            stream_bytes.extend_from_slice(&tail);
        }

        let ll = read_low_level(&stream_bytes);
        assert_eq!(ll.metadata().num_row_groups(), 2);
        assert_eq!(ll.metadata().row_group(0).num_rows(), 6);
        assert_eq!(ll.metadata().row_group(1).num_rows(), 2);

        let collected: Vec<i64> = read_rows(&stream_bytes)
            .iter()
            .map(|r| r.get_long(0).unwrap())
            .collect();
        assert_eq!(collected, (0..8).collect::<Vec<_>>());

        // Also test byte-threshold flush (max_rows = 1_000_000, max_bytes = 64).
        let b1 = make_batch(0, 4);
        let b2 = make_batch(4, 4);
        let bytes_triggered =
            export_batches_with_limits(schema, vec![b1, b2], &pool, 1_000_000, 64).unwrap();
        assert_eq!(pool.reserved(), 0);
        let ll_bytes = read_low_level(&bytes_triggered);
        assert_eq!(ll_bytes.metadata().num_row_groups(), 2);
    }

    #[test]
    fn memory_limit_too_large_releases_all_reservations() {
        // Pool cap smaller than a batch's memory_bytes.
        let pool = GlobalPool::standalone(32);
        let schema = Schema::try_new_arc(vec![Field::new("n", DataType::I64, false)]).unwrap();
        let big_batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1; 64]), None).unwrap(),
            )],
            64,
        )
        .unwrap();

        let mut exporter = ParquetBatchExporter::new(schema, &pool).unwrap();
        let err = exporter.push_batch(big_batch, None).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        drop(exporter);
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn batch_schema_mismatch_is_rejected() {
        let pool = GlobalPool::standalone(1024 * 1024);
        let export_schema =
            Schema::try_new_arc(vec![Field::new("a", DataType::I64, false)]).unwrap();
        let wrong_schema =
            Schema::try_new_arc(vec![Field::new("b", DataType::I64, false)]).unwrap();
        let batch = Batch::try_new_all(
            wrong_schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap(),
            )],
            1,
        )
        .unwrap();

        let mut exporter = ParquetBatchExporter::new(export_schema, &pool).unwrap();
        let err = exporter.push_batch(batch, None).unwrap_err();
        assert!(matches!(err, QueryError::Failed(_)));
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn required_columns_and_list_of_complex_types_round_trip() {
        let pool = GlobalPool::standalone(16 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("req_id", DataType::I64, false),
            Field::new(
                "req_dec38",
                DataType::Decimal128 {
                    precision: 38,
                    scale: 18,
                },
                false,
            ),
            Field::new(
                "list_map",
                DataType::List(Box::new(DataType::MapUtf8)),
                true,
            ),
            Field::new(
                "list_events",
                DataType::List(Box::new(DataType::Events)),
                true,
            ),
            Field::new(
                "list_links",
                DataType::List(Box::new(DataType::Links)),
                true,
            ),
            Field::new(
                "list_dec",
                DataType::List(Box::new(DataType::Decimal128 {
                    precision: 8,
                    scale: -1,
                })),
                true,
            ),
        ])
        .unwrap();

        let big_neg = -123_456_789_012_345_678_901_234_567_890_123_456_789_i128;
        let req_id = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2]), None).unwrap());
        let req_dec38 = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![big_neg, 42],
                    precision: 38,
                    scale: 18,
                },
                None,
            )
            .unwrap(),
        );

        // list_map: [[{}, {"a": "1", "b": NULL}], NULL]
        let inner_maps = Column::Flat(
            FlatColumn::try_new(
                FlatData::MapUtf8(Box::new(
                    MapData::try_new(
                        vec![0, 0, 2],
                        Utf8Data::try_from_slices(["a", "b"]).unwrap(),
                        Utf8Data::try_from_slices(["1", ""]).unwrap(),
                        Some(Bitmap::from_bools(&[true, false])),
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        );
        let list_map = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::MapUtf8,
                    offsets: vec![0, 2, 2],
                    values: Box::new(inner_maps),
                },
                Some(Bitmap::from_bools(&[true, false])),
            )
            .unwrap(),
        );

        // list_events: [[[{t: 10, name: "e", attr: None}]], []]
        let inner_events = Column::Flat(
            FlatColumn::try_new(
                FlatData::Events(Box::new(
                    EventsData::try_new(
                        vec![0, 1],
                        vec![10],
                        Utf8Data::try_from_slices(["e"]).unwrap(),
                        Utf8Data::try_from_slices([""]).unwrap(),
                        Some(Bitmap::from_bools(&[false])),
                    )
                    .unwrap(),
                )),
                None,
            )
            .unwrap(),
        );
        let list_events = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Events,
                    offsets: vec![0, 1, 1],
                    values: Box::new(inner_events),
                },
                None,
            )
            .unwrap(),
        );

        // list_links: [[[{trace: [7;16], span: [8;8], attr: Some("ok")}]], [NULL]]
        let inner_links = Column::Flat(
            FlatColumn::try_new(
                FlatData::Links(Box::new(
                    LinksData::try_new(
                        vec![0, 1, 1],
                        vec![[7; 16]],
                        vec![[8; 8]],
                        Utf8Data::try_from_slices(["ok"]).unwrap(),
                        None,
                    )
                    .unwrap(),
                )),
                Some(Bitmap::from_bools(&[true, false])),
            )
            .unwrap(),
        );
        let list_links = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Links,
                    offsets: vec![0, 1, 2],
                    values: Box::new(inner_links),
                },
                None,
            )
            .unwrap(),
        );

        // list_dec: [[5, NULL, -3], []] with scale -1 => [50, NULL, -30]
        let inner_dec = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![5, 0, -3],
                    precision: 8,
                    scale: -1,
                },
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let list_dec = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Decimal128 {
                        precision: 8,
                        scale: -1,
                    },
                    offsets: vec![0, 3, 3],
                    values: Box::new(inner_dec),
                },
                None,
            )
            .unwrap(),
        );

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                req_id,
                req_dec38,
                list_map,
                list_events,
                list_links,
                list_dec,
            ],
            2,
        )
        .unwrap();

        let bytes = export_batches(schema, vec![batch], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);

        let ll = read_low_level(&bytes);
        let schema_descr = ll.metadata().file_metadata().schema_descr();
        assert_eq!(schema_descr.column(0).max_def_level(), 0);
        assert_eq!(schema_descr.column(1).max_def_level(), 0);
        // Leaf for list_dec is the last leaf column in the schema.
        let last_leaf = schema_descr.column(schema_descr.num_columns() - 1);
        assert_eq!(
            last_leaf.logical_type_ref(),
            Some(&LogicalType::decimal(0, 9))
        );

        let rows = read_rows(&bytes);
        assert_eq!(rows.len(), 2);

        assert_eq!(decimal_i128(field_at(&rows[0], 1)), big_neg);
        assert_eq!(decimal_i128(field_at(&rows[1], 1)), 42);

        let lm_0 = list_elements(field_at(&rows[0], 2));
        assert_eq!(lm_0.len(), 2);
        assert!(map_entries(&lm_0[0]).is_empty());
        assert_eq!(map_entries(&lm_0[1]).len(), 2);
        assert_eq!(field_at(&rows[1], 2), &ParquetField::Null);

        let ld_0 = list_elements(field_at(&rows[0], 5));
        assert_eq!(ld_0.len(), 3);
        assert_eq!(decimal_i128(&ld_0[0]), 50);
        assert_eq!(ld_0[1], ParquetField::Null);
        assert_eq!(decimal_i128(&ld_0[2]), -30);
    }

    #[test]
    fn tight_pool_flushes_buffered_row_group_before_failing_and_allows_non_null_batch_in_nullable_schema()
     {
        let nullable_schema =
            Schema::try_new_arc(vec![Field::new("x", DataType::I64, true)]).unwrap();
        let non_null_schema =
            Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();

        let mk_batch = |schema: &Arc<Schema>, vals: Vec<i64>| {
            let len = vals.len();
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None).unwrap());
            Batch::try_new_all(Arc::clone(schema), vec![col], len).unwrap()
        };

        // Two batches of 128 i64s (~1024 B each). Pool fits 1 batch (1536 B), not 2 (2048 B).
        let b1 = mk_batch(&non_null_schema, (0..128).collect());
        let b2 = mk_batch(&non_null_schema, (128..256).collect());
        assert!(b1.memory_bytes() < 1536);
        assert!(b1.memory_bytes() + b2.memory_bytes() > 1536);

        let pool = GlobalPool::standalone(1536);
        let mut exporter = ParquetBatchExporter::with_row_group_limits(
            Arc::clone(&nullable_schema),
            &pool,
            10_000,
            10 * 1024 * 1024,
        )
        .unwrap();

        // Batch 1 is buffered without flushing (normal threshold not reached).
        let chunks1 = exporter.push_batch(b1, None).unwrap();
        assert!(chunks1.is_empty());

        // Batch 2 exceeds tight pool -> flushes batch 1's row group first, then buffers batch 2!
        let chunks2 = exporter.push_batch(b2, None).unwrap();
        assert_eq!(chunks2.len(), 1);

        let mut out = Vec::new();
        for c in chunks2 {
            out.extend_from_slice(&c);
        }
        for c in exporter.finish().unwrap() {
            out.extend_from_slice(&c);
        }
        assert_eq!(pool.reserved(), 0);

        assert_eq!(read_rows(&out).len(), 256);

        // Nullable batch into non-nullable exporter schema must still fail.
        let mut strict_exporter =
            ParquetBatchExporter::new(Arc::clone(&non_null_schema), &pool).unwrap();
        let bad_batch = mk_batch(&nullable_schema, vec![1, 2, 3]);
        assert!(strict_exporter.push_batch(bad_batch, None).is_err());
        drop(strict_exporter);
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn chunk_bytes_splits_both_flush_and_finish_and_transfers_pull_res_without_2x_spike() {
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let mk_batch = |vals: Vec<i64>| {
            let len = vals.len();
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None).unwrap());
            Batch::try_new_all(Arc::clone(&schema), vec![col], len).unwrap()
        };

        // Pool fits ONE batch + compressed row-group output (now reserved live via ReservedVec),
        // NOT 2x batch memory!
        let b1 = mk_batch((0..256).collect());
        let b2 = mk_batch((256..512).collect());
        let b1_bytes = b1.memory_bytes();
        let cap = b1_bytes + 1536;
        assert!(b1_bytes * 2 > cap);

        let pool = GlobalPool::standalone(cap);
        let max_chunk_bytes = 64usize;
        let mut exporter = ParquetBatchExporter::with_row_group_limits(
            Arc::clone(&schema),
            &pool,
            256,
            10 * 1024 * 1024,
        )
        .unwrap()
        .with_chunk_bytes(max_chunk_bytes);

        let mut pull_res = pool.register("pull", false);
        pull_res.try_grow(b1.memory_bytes()).unwrap();
        let flushed = exporter.push_batch(b1, Some(&mut pull_res)).unwrap();
        assert_eq!(pull_res.size(), 0);
        assert!(
            flushed.len() > 1,
            "flush_row_group must split into <=64B chunks"
        );
        for c in &flushed {
            assert!(c.len() <= max_chunk_bytes);
        }

        let b2_bytes = b2.memory_bytes();
        pull_res.try_grow(b2_bytes).unwrap();
        // Force finish() to write a second row group + footer while under tight cap.
        let mut exporter2 = ParquetBatchExporter::with_row_group_limits(
            Arc::clone(&schema),
            &pool,
            10_000,
            10 * 1024 * 1024,
        )
        .unwrap()
        .with_chunk_bytes(max_chunk_bytes);
        let c2 = exporter2.push_batch(b2, Some(&mut pull_res)).unwrap();
        assert!(c2.is_empty());
        assert_eq!(pull_res.size(), 0);
        assert_eq!(
            pool.reserved(),
            b2_bytes + exporter2.writer.inner().reserved_bytes()
        );

        let tail = exporter2.finish().unwrap();
        assert!(tail.len() > 1, "finish() must also split into <=64B chunks");
        for c in &tail {
            assert!(c.len() <= max_chunk_bytes);
        }
        assert_eq!(pool.reserved(), 0);

        // Verify the full file bytes from exporter (plus finishing it) parse cleanly.
        let mut full = Vec::new();
        for c in flushed {
            full.extend_from_slice(&c);
        }
        for c in exporter.finish().unwrap() {
            assert!(c.len() <= max_chunk_bytes);
            full.extend_from_slice(&c);
        }
        assert_eq!(pool.reserved(), 0);
        assert_eq!(read_rows(&full).len(), 256);
    }

    #[test]
    fn chunked_leaf_buffer_exports_large_const_and_masked_batches() {
        let schema = Schema::try_new_arc(vec![Field::new("c", DataType::I64, true)]).unwrap();
        let const_col = Column::try_new_const(ScalarValue::I64(42), 10_000).unwrap();
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![const_col], 10_000).unwrap();
        let pool = GlobalPool::standalone(1024 * 1024);
        let bytes = export_batches(Arc::clone(&schema), vec![batch], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);
        assert_eq!(read_rows(&bytes).len(), 10_000);
    }

    #[test]
    fn writer_oom_during_init_row_group_and_finish_maps_to_too_large_and_releases_pool() {
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();

        // 1. Pool too small even for the 4-byte Parquet magic header + empty footer -> TooLarge
        //    (not Internal). `SerializedFileWriter` uses an internal `BufWriter`, so the error
        //    surfaces on `finish()` when the header/footer flush hits `ReservedVec::write`.
        let tiny_pool = GlobalPool::standalone(2);
        let init_err = match ParquetBatchExporter::new(Arc::clone(&schema), &tiny_pool) {
            Ok(exp) => exp.finish().unwrap_err(),
            Err(err) => err,
        };
        assert!(
            matches!(init_err, QueryError::TooLarge),
            "expected TooLarge on header/footer OOM, got {init_err:?}"
        );
        assert_eq!(tiny_pool.reserved(), 0);

        // 2. Pool fits the batch (with high-entropy strings) + header, NOT the compressed row group + footer.
        let mut vals = Vec::with_capacity(64);
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..64 {
            let mut s = String::with_capacity(64);
            for _ in 0..16 {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let hex_nib = ((state >> 60) & 0xf) as u8;
                let ch = if hex_nib < 10 {
                    (b'0' + hex_nib) as char
                } else {
                    (b'a' + (hex_nib - 10)) as char
                };
                s.push(ch);
            }
            vals.push(s);
        }
        let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(refs).unwrap()),
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 64).unwrap();
        let batch_bytes = batch.memory_bytes();

        // Cap fits the buffered batch + 4-byte header + 16 bytes slack, so writing the
        // compressed row group / footer MUST trip ReservedVec::write OOM -> QueryError::TooLarge.
        let pool = GlobalPool::standalone(batch_bytes + 20);
        let mut exporter = ParquetBatchExporter::with_row_group_limits(
            Arc::clone(&schema),
            &pool,
            10_000,
            10 * 1024 * 1024,
        )
        .unwrap();
        assert!(exporter.push_batch(batch, None).unwrap().is_empty());
        let finish_err = exporter.finish().unwrap_err();
        assert!(
            matches!(finish_err, QueryError::TooLarge),
            "expected TooLarge on writer OOM, got {finish_err:?}"
        );
        assert_eq!(pool.reserved(), 0);
    }
}
