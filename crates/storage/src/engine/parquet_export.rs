//! Zero-Arrow streaming Parquet export writer over `cetra_column::Batch`.
//!
//! Uses `parquet::file::writer::SerializedFileWriter<Vec<u8>>` directly with
//! ZSTD level 1 compression and row-group buffering gated by [`Reservation`].

use std::fmt;
use std::sync::Arc;

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

/// Streaming low-level Parquet exporter consuming [`Batch`]es and yielding
/// completed Parquet chunks on row-group flushes.
pub(crate) struct ParquetBatchExporter {
    schema: Arc<Schema>,
    writer: SerializedFileWriter<Vec<u8>>,
    buffered_batches: Vec<Batch>,
    buffered_rows: usize,
    buffered_bytes: usize,
    writer_reserved_bytes: usize,
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
            .field("writer_reserved_bytes", &self.writer_reserved_bytes)
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
        let writer = SerializedFileWriter::new(Vec::new(), parquet_schema, props)
            .map_err(|e| parquet_err(&e))?;
        let reservation = pool.register("parquet_export", false);

        Ok(Self {
            schema,
            writer,
            buffered_batches: Vec::new(),
            buffered_rows: 0,
            buffered_bytes: 0,
            writer_reserved_bytes: 0,
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
        let prev_len = self.writer.inner_mut().len();
        let buf = self.writer.into_inner().map_err(|e| parquet_err(&e))?;
        let delta = buf.len().saturating_sub(prev_len);
        if delta > 0 {
            self.reservation.try_grow(delta)?;
        }
        self.reservation.free();
        self.writer_reserved_bytes = 0;
        Ok(split_bytes_chunks(buf, self.max_chunk_bytes))
    }

    fn flush_row_group(&mut self) -> Result<Vec<Bytes>, QueryError> {
        self.write_buffered_row_group()?;
        let prev_len = self.writer.inner_mut().len();
        self.writer.flush().map_err(|e| io_err(&e))?;
        let delta = self.writer.inner_mut().len().saturating_sub(prev_len);
        if delta > 0 {
            self.reservation.try_grow(delta)?;
            self.writer_reserved_bytes = self.writer_reserved_bytes.saturating_add(delta);
        }
        let buf = std::mem::take(self.writer.inner_mut());
        let to_shrink = self.writer_reserved_bytes.min(self.reservation.size());
        self.reservation.shrink(to_shrink);
        self.writer_reserved_bytes = 0;
        Ok(split_bytes_chunks(buf, self.max_chunk_bytes))
    }

    fn write_buffered_row_group(&mut self) -> Result<(), QueryError> {
        if self.buffered_batches.is_empty() {
            return Ok(());
        }

        let prev_writer_len = self.writer.inner_mut().len();
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
        res?;

        let writer_delta = self
            .writer
            .inner_mut()
            .len()
            .saturating_sub(prev_writer_len);
        if writer_delta > 0 {
            self.reservation.try_grow(writer_delta)?;
            self.writer_reserved_bytes = self.writer_reserved_bytes.saturating_add(writer_delta);
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

fn io_err(err: &std::io::Error) -> QueryError {
    QueryError::internal(format!("parquet export io error: {err}"))
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

fn write_columns_for_batches(
    rg: &mut SerializedRowGroupWriter<'_, Vec<u8>>,
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

    use arrow::array::{
        Array, BinaryArray, BooleanArray, Date32Array, Decimal128Array, FixedSizeBinaryArray,
        Float64Array, Int64Array, ListArray, MapArray, StringArray, StructArray,
        TimestampNanosecondArray,
    };
    use arrow::datatypes::{DataType as ArrowDataType, TimeUnit as ArrowTimeUnit};
    use bytes::Bytes;
    use cetra_column::{
        Batch, BinaryData, Bitmap, Column, DataType, DictColumn, EventsData, Field, FlatColumn,
        FlatData, GlobalPool, Interval, LinksData, MapData, RleColumn, ScalarValue, Schema,
        ScopedPool, Selection, Utf8Data,
    };
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::basic::Compression;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use parquet::record::RowAccessor;

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

    fn read_arrow(bytes: &[u8]) -> Vec<arrow::record_batch::RecordBatch> {
        let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
            .expect("valid parquet for arrow reader");
        builder
            .build()
            .expect("build arrow reader")
            .map(|r| r.expect("record batch"))
            .collect()
    }

    fn read_low_level(bytes: &[u8]) -> SerializedFileReader<Bytes> {
        SerializedFileReader::new(Bytes::copy_from_slice(bytes))
            .expect("valid parquet for low-level reader")
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

        let arrow_batches = read_arrow(&bytes);
        let total_rows: usize = arrow_batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 0);

        // Also verify 0 push_batch calls at all.
        let bytes_no_push = export_batches(schema, vec![], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);
        let ll2 = read_low_level(&bytes_no_push);
        assert_eq!(ll2.metadata().num_row_groups(), 0);
        assert!(read_arrow(&bytes_no_push).is_empty());
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

        // Low-level Parquet reader verification (compression, row count, types, values).
        let ll = read_low_level(&bytes);
        assert_eq!(ll.metadata().num_row_groups(), 1);
        let rg_meta = ll.metadata().row_group(0);
        assert_eq!(rg_meta.num_rows(), 4);
        for c in rg_meta.columns() {
            assert_eq!(c.compression(), Compression::ZSTD(ZstdLevel::default()));
        }
        let ll_rows: Vec<_> = ll.get_row_iter(None).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(ll_rows.len(), 4);
        assert!(ll_rows[0].get_bool(0).unwrap());
        assert_eq!(ll_rows[0].get_long(1).unwrap(), -42);
        assert_eq!(ll_rows[2].get_string(10).unwrap(), "cetra");

        // Arrow reader verification (DuckDB/Polars/Arrow interop).
        let arrow_batches = read_arrow(&bytes);
        assert_eq!(arrow_batches.len(), 1);
        let ab = &arrow_batches[0];
        assert_eq!(ab.num_rows(), 4);

        let b_arr = ab
            .column(0)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert!(b_arr.value(0));
        assert!(b_arr.is_null(1));
        assert!(!b_arr.value(2));
        assert!(b_arr.value(3));

        let i_arr = ab.column(1).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(i_arr.value(0), -42);
        assert!(i_arr.is_null(1));
        assert_eq!(i_arr.value(3), i64::MAX);

        let f_arr = ab
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        assert_eq!(f_arr.value(0), 1.5);
        assert!(f_arr.is_null(1));
        assert!(f_arr.value(2).is_nan());
        assert_eq!(f_arr.value(3), f64::INFINITY);

        let dp_arr = ab
            .column(3)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(dp_arr.data_type(), &ArrowDataType::Decimal128(10, 2));
        assert_eq!(dp_arr.value(0), 12_345);
        assert!(dp_arr.is_null(1));
        assert_eq!(dp_arr.value(2), -98_765);

        let dnr_arr = ab
            .column(4)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(dnr_arr.data_type(), &ArrowDataType::Decimal128(12, 0));
        assert_eq!(dnr_arr.value(0), 4200);
        assert!(dnr_arr.is_null(1));
        assert_eq!(dnr_arr.value(2), -700);
        assert_eq!(dnr_arr.value(3), 12300);

        let dns_arr = ab.column(5).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(dns_arr.value(0), "420000");
        assert!(dns_arr.is_null(1));
        assert_eq!(dns_arr.value(2), "-90000");
        assert_eq!(dns_arr.value(3), "10000000000000000");

        let ts_utc_arr = ab
            .column(6)
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .unwrap();
        assert_eq!(
            ts_utc_arr.data_type(),
            &ArrowDataType::Timestamp(ArrowTimeUnit::Nanosecond, Some("UTC".into()))
        );
        assert_eq!(ts_utc_arr.value(0), 1_700_000_000_123_456_789);
        assert!(ts_utc_arr.is_null(1));
        assert_eq!(ts_utc_arr.value(2), -1);

        let ts_naive_arr = ab
            .column(7)
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .unwrap();
        assert_eq!(
            ts_naive_arr.data_type(),
            &ArrowDataType::Timestamp(ArrowTimeUnit::Nanosecond, None)
        );
        assert_eq!(ts_naive_arr.value(2), -100);

        let d_arr = ab.column(8).as_any().downcast_ref::<Date32Array>().unwrap();
        assert_eq!(d_arr.value(0), 0);
        assert!(d_arr.is_null(1));
        assert_eq!(d_arr.value(2), 19700);
        assert_eq!(d_arr.value(3), -1);

        let iv_arr = ab.column(9).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(iv_arr.value(0), "1 mons -2 days 0.000000300 secs");
        assert!(iv_arr.is_null(1));
        assert_eq!(iv_arr.value(2), "0 secs");
        assert_eq!(iv_arr.value(3), "-1 mons -1 days -10 mins");

        let s_arr = ab
            .column(10)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(s_arr.value(0), "hello");
        assert!(s_arr.is_null(1));
        assert_eq!(s_arr.value(3), "🚀");

        let bin_arr = ab
            .column(11)
            .as_any()
            .downcast_ref::<BinaryArray>()
            .unwrap();
        assert_eq!(bin_arr.value(0), b"\x00\x01");
        assert!(bin_arr.is_null(1));
        assert_eq!(bin_arr.value(2), b"\xff\xfe\xfd");

        let fb8_arr = ab
            .column(12)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        assert_eq!(fb8_arr.value(0), &[1; 8]);
        assert!(fb8_arr.is_null(1));
        assert_eq!(fb8_arr.value(2), &[2; 8]);

        let fb16_arr = ab
            .column(13)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        assert_eq!(fb16_arr.value(0), &[0xaa; 16]);
        assert!(fb16_arr.is_null(1));
        assert_eq!(fb16_arr.value(3), &[0xcc; 16]);

        let n_arr = ab
            .column(14)
            .as_any()
            .downcast_ref::<BooleanArray>()
            .unwrap();
        assert_eq!(n_arr.null_count(), 4);
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
        assert_eq!(ll.get_row_iter(None).unwrap().count(), 5);

        let arrow_batches = read_arrow(&bytes);
        assert_eq!(arrow_batches.len(), 1);
        let ab = &arrow_batches[0];
        assert_eq!(ab.num_rows(), 5);

        // Verify list_utf8
        let l1 = ab.column(0).as_any().downcast_ref::<ListArray>().unwrap();
        assert!(l1.is_null(0));
        assert!(!l1.is_null(1) && l1.value(1).is_empty());
        let r2 = l1.value(2);
        assert_eq!(r2.len(), 1);
        assert!(r2.is_null(0));
        let r3 = l1.value(3);
        let r3_s = r3.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(r3_s.len(), 3);
        assert_eq!(r3_s.value(0), "a");
        assert!(r3_s.is_null(1));
        assert_eq!(r3_s.value(2), "b");

        // Verify nested_list: [NULL, [], [NULL, []], [["x", NULL], [], ["y"]], [["z"]]]
        let l2 = ab.column(1).as_any().downcast_ref::<ListArray>().unwrap();
        assert!(l2.is_null(0));
        assert!(!l2.is_null(1) && l2.value(1).is_empty());
        let l2_r2 = l2.value(2);
        let l2_r2_list = l2_r2.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(l2_r2_list.len(), 2);
        assert!(l2_r2_list.is_null(0));
        assert!(!l2_r2_list.is_null(1) && l2_r2_list.value(1).is_empty());

        let l2_r3 = l2.value(3);
        let l2_r3_list = l2_r3.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(l2_r3_list.len(), 3);
        let r3_0 = l2_r3_list.value(0);
        let r3_0_s = r3_0.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(r3_0_s.value(0), "x");
        assert!(r3_0_s.is_null(1));
        assert!(l2_r3_list.value(1).is_empty());
        assert_eq!(
            l2_r3_list
                .value(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "y"
        );

        // Verify MapUtf8
        let m_arr = ab.column(2).as_any().downcast_ref::<MapArray>().unwrap();
        assert!(m_arr.is_null(0));
        assert!(!m_arr.is_null(1) && m_arr.value(1).is_empty());
        let m_r2 = m_arr.value(2);
        assert_eq!(m_r2.len(), 2);
        let m_r2_keys = m_r2
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let m_r2_vals = m_r2
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(m_r2_keys.value(0), "k1");
        assert_eq!(m_r2_vals.value(0), "v1");
        assert_eq!(m_r2_keys.value(1), "k2");
        assert!(m_r2_vals.is_null(1));

        // Verify Events
        let ev_arr = ab.column(3).as_any().downcast_ref::<ListArray>().unwrap();
        assert!(ev_arr.is_null(0));
        assert!(!ev_arr.is_null(1) && ev_arr.value(1).is_empty());
        let ev_r2 = ev_arr.value(2);
        let ev_struct = ev_r2.as_any().downcast_ref::<StructArray>().unwrap();
        assert_eq!(ev_struct.len(), 2);
        let ev_times = ev_struct
            .column(0)
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .unwrap();
        let ev_names = ev_struct
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let ev_attrs = ev_struct
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(ev_times.value(0), 100);
        assert_eq!(ev_names.value(0), "ev1");
        assert!(ev_attrs.is_null(0));
        assert_eq!(ev_times.value(1), 200);
        assert_eq!(ev_names.value(1), "ev2");
        assert_eq!(ev_attrs.value(1), r#"{"x":1}"#);

        // Verify Links
        let lk_arr = ab.column(4).as_any().downcast_ref::<ListArray>().unwrap();
        assert!(lk_arr.is_null(0));
        assert!(!lk_arr.is_null(1) && lk_arr.value(1).is_empty());
        let lk_r2 = lk_arr.value(2);
        let lk_struct = lk_r2.as_any().downcast_ref::<StructArray>().unwrap();
        assert_eq!(lk_struct.len(), 2);
        let lk_traces = lk_struct
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        let lk_spans = lk_struct
            .column(1)
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        let lk_attrs = lk_struct
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(lk_traces.value(0), &[1; 16]);
        assert_eq!(lk_spans.value(0), &[0x11; 8]);
        assert_eq!(lk_attrs.value(0), r#"{"r":"1"}"#);
        assert_eq!(lk_traces.value(1), &[2; 16]);
        assert_eq!(lk_spans.value(1), &[0x22; 8]);
        assert!(lk_attrs.is_null(1));
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

        let arrow_batches = read_arrow(&bytes);
        assert_eq!(arrow_batches.len(), 1);
        let ab = &arrow_batches[0];
        assert_eq!(ab.num_rows(), 7);

        let flat_arr = ab.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(flat_arr.values(), &[10, 30, 50, 60, 60, 20, 40]);

        let const_arr = ab.column(1).as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..7 {
            assert_eq!(const_arr.value(i), "shared");
        }

        let dict_arr = ab.column(2).as_any().downcast_ref::<StringArray>().unwrap();
        // Selected rows: [0, 2, 4, 5] + [5, 1, 3]
        assert_eq!(dict_arr.value(0), "alpha");
        assert_eq!(dict_arr.value(1), "alpha");
        assert_eq!(dict_arr.value(2), "beta");
        assert!(dict_arr.is_null(3));
        assert!(dict_arr.is_null(4));
        assert!(dict_arr.is_null(5));
        assert_eq!(dict_arr.value(6), "alpha");

        let rle_arr = ab.column(3).as_any().downcast_ref::<Int64Array>().unwrap();
        // run 0..2 => 100, run 2..4 => NULL, run 4..6 => 300
        // rows [0, 2, 4, 5, 5, 1, 3] => [100, NULL, 300, 300, 300, 100, NULL]
        assert_eq!(rle_arr.value(0), 100);
        assert!(rle_arr.is_null(1));
        assert_eq!(rle_arr.value(2), 300);
        assert_eq!(rle_arr.value(3), 300);
        assert_eq!(rle_arr.value(4), 300);
        assert_eq!(rle_arr.value(5), 100);
        assert!(rle_arr.is_null(6));

        let cnull_arr = ab.column(4).as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(cnull_arr.null_count(), 7);

        let clist_arr = ab.column(5).as_any().downcast_ref::<ListArray>().unwrap();
        for i in 0..7 {
            let item = clist_arr.value(i);
            let s = item.as_any().downcast_ref::<StringArray>().unwrap();
            assert_eq!(s.len(), 2);
            assert_eq!(s.value(0), "x");
            assert!(s.is_null(1));
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

        let arrow_batches = read_arrow(&stream_bytes);
        let collected: Vec<i64> = arrow_batches
            .iter()
            .flat_map(|b| {
                b.column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .iter()
                    .copied()
            })
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

        let arrow_batches = read_arrow(&bytes);
        assert_eq!(arrow_batches.len(), 1);
        let ab = &arrow_batches[0];
        assert_eq!(ab.num_rows(), 2);

        let dec38 = ab
            .column(1)
            .as_any()
            .downcast_ref::<Decimal128Array>()
            .unwrap();
        assert_eq!(dec38.value(0), big_neg);
        assert_eq!(dec38.value(1), 42);

        let lm = ab.column(2).as_any().downcast_ref::<ListArray>().unwrap();
        assert!(!lm.is_null(0));
        assert!(lm.is_null(1));
        let lm_0 = lm.value(0);
        let lm_0_map = lm_0.as_any().downcast_ref::<MapArray>().unwrap();
        assert_eq!(lm_0_map.len(), 2);
        assert!(lm_0_map.value(0).is_empty());
        assert_eq!(lm_0_map.value(1).len(), 2);

        let ld = ab.column(5).as_any().downcast_ref::<ListArray>().unwrap();
        let ld_0 = ld.value(0);
        let ld_0_d = ld_0.as_any().downcast_ref::<Decimal128Array>().unwrap();
        assert_eq!(ld_0_d.data_type(), &ArrowDataType::Decimal128(9, 0));
        assert_eq!(ld_0_d.value(0), 50);
        assert!(ld_0_d.is_null(1));
        assert_eq!(ld_0_d.value(2), -30);
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

        let arrow_batches = read_arrow(&out);
        let total_rows: usize = arrow_batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 256);

        // Nullable batch into non-nullable exporter schema must still fail.
        let mut strict_exporter =
            ParquetBatchExporter::new(Arc::clone(&non_null_schema), &pool).unwrap();
        let bad_batch = mk_batch(&nullable_schema, vec![1, 2, 3]);
        assert!(strict_exporter.push_batch(bad_batch, None).is_err());
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

        // Pool fits ONE batch + small writer overhead, NOT 2x batch memory!
        let b1 = mk_batch((0..256).collect());
        let b2 = mk_batch((256..512).collect());
        let b1_bytes = b1.memory_bytes();
        let cap = b1_bytes + 512;
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
        assert_eq!(pool.reserved(), b2_bytes);

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
        let arrow_batches = read_arrow(&full);
        let total_rows: usize = arrow_batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 256);
    }

    #[test]
    fn chunked_leaf_buffer_exports_large_const_and_masked_batches() {
        let schema = Schema::try_new_arc(vec![Field::new("c", DataType::I64, true)]).unwrap();
        let const_col = Column::try_new_const(ScalarValue::I64(42), 10_000).unwrap();
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![const_col], 10_000).unwrap();
        let pool = GlobalPool::standalone(1024 * 1024);
        let bytes = export_batches(Arc::clone(&schema), vec![batch], &pool).unwrap();
        assert_eq!(pool.reserved(), 0);
        let arrow_batches = read_arrow(&bytes);
        let total_rows: usize = arrow_batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 10_000);
    }
}
