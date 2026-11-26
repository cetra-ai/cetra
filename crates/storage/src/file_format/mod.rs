//! Single internal Parquet file-format adapter (`BatchFileWriter` and zero-Arrow readers).
//!
//! Isolates Parquet encoding and low-level decoding behind [`BatchFileWriter`],
//! [`write_file_batch`], [`verify_parquet_file`], [`read_time_column`], and
//! [`read_trace_anchors`].

mod arrow_write;

use std::io::Write;
use std::ops::Range;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use arrow_write::{
    arrow_scores_schema, arrow_spans_schema, arrow_traces_schema, batch_to_record_batch,
};
use bytes::{Buf, Bytes};
use cetra_column::{
    Batch, Bitmap, Column, DataType, Field, FlatColumn, FlatData, ScalarValue, Schema, Selection,
};
use cetra_core::schema::{scores, spans, traces};
use cetra_query::Expr;
use cetra_query::exec::{SortKey, compare_rows};
use futures::FutureExt;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};
use parquet::DecodeResult;
use parquet::arrow::ArrowSchemaConverter;
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::basic::{
    Compression, ConvertedType, LogicalType, TimeUnit, Type as PhysicalType, ZstdLevel,
};
use parquet::column::reader::{ColumnReader, ColumnReaderImpl, get_column_reader};
use parquet::data_type::{ByteArrayType, FixedLenByteArrayType, Int64Type};
use parquet::errors::ParquetError;
use parquet::file::metadata::{
    KeyValue, PageIndexPolicy, ParquetMetaData, ParquetMetaDataPushDecoder, RowGroupMetaData,
    SortingColumn,
};
use parquet::file::properties::{EnabledStatistics, ReaderProperties, WriterProperties};
use parquet::file::reader::{ChunkReader, Length};
use parquet::file::serialized_reader::SerializedPageReader;
use parquet::file::statistics::Statistics;
use parquet::schema::types::{ColumnDescPtr, ColumnPath, SchemaDescriptor};

use crate::catalog::{FileMeta, FileTable};
use crate::error::{AnchorError, CompactionError, EngineError, ParquetWriteError};
use crate::format::FORMAT_VERSION;
use crate::unwind::{decoder_panicked, guard_sync};
use crate::verify::{Verdict, is_store_error};

/// `cetra.schema_version` written into every file: the row normalization
/// version (spec 01), **not** the file format version (`cetra.format_version`,
/// spec 03 § 12).
///
/// It follows the stored row `schema_version` (spec 01). Rows are version 1.
const SCHEMA_VERSION: &str = "1";

const KEY_TABLE: &str = "cetra.table";
const KEY_SCHEMA_VERSION: &str = "cetra.schema_version";
const KEY_FORMAT_VERSION: &str = "cetra.format_version";
const KEY_LEVEL: &str = "cetra.level";
const KEY_MIN_TIME: &str = "cetra.min_time";
const KEY_MAX_TIME: &str = "cetra.max_time";
const KEY_MIN_INGEST_TIME: &str = "cetra.min_ingest_time";
const KEY_MAX_INGEST_TIME: &str = "cetra.max_ingest_time";

/// Columns spec 01 marks `(dict)` for `spans`, in schema order.
///
/// `conventions` is `List(Utf8 dict)`: dictionary encoding applies to the list
/// values. Every other column, including `input`, `output` and `attributes`,
/// is plain.
pub(crate) const SPANS_DICTIONARY_COLUMNS: &[&str] = &[
    spans::PROJECT_ID,
    spans::SPAN_KIND,
    spans::STATUS,
    spans::SERVICE_NAME,
    spans::ENVIRONMENT,
    spans::RELEASE,
    spans::PROVIDER,
    spans::REQUEST_MODEL,
    spans::RESPONSE_MODEL,
    spans::MODEL,
    spans::PROMPT_NAME,
    spans::OPERATION,
    spans::COST_SOURCE,
    spans::INPUT_FORMAT,
    spans::OUTPUT_FORMAT,
    spans::SCOPE_NAME,
    spans::SCOPE_VERSION,
    spans::CONVENTIONS,
];

/// Columns spec 01 marks `(dict)` for `scores`, in schema order.
pub(crate) const SCORES_DICTIONARY_COLUMNS: &[&str] = &[
    scores::TARGET,
    scores::NAME,
    scores::DATA_TYPE,
    scores::SOURCE,
];

/// Low-cardinality strings of `traces`, in schema order. Spec 03 §3.
pub(crate) const TRACES_DICTIONARY_COLUMNS: &[&str] = &[
    traces::NAME,
    traces::STATUS,
    traces::ENVIRONMENT,
    traces::RELEASE,
];

const SPANS_BLOOM_COLUMNS: &[&str] = &[spans::TRACE_ID, spans::SESSION_ID, spans::USER_ID];
const TRACES_BLOOM_COLUMNS: &[&str] = &[traces::TRACE_ID, traces::SESSION_ID, traces::USER_ID];
const SCORES_BLOOM_COLUMNS: &[&str] = &[scores::TRACE_ID, scores::SCORE_ID];

/// One encoded Parquet object and the catalog fields that describe it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrittenFile {
    /// The complete file, including the footer.
    pub(crate) bytes: Bytes,
    /// Rows in the file.
    pub(crate) rows: u64,
    /// Footer metadata length plus the 8-byte trailer.
    ///
    /// A reader can open the file with one ranged read of this many trailing bytes.
    pub(crate) footer_size: u64,
    /// Smallest event time, nanoseconds. `start_time` for spans, `created_at` for scores.
    pub(crate) min_time: i64,
    /// Largest event time, nanoseconds.
    pub(crate) max_time: i64,
    /// Smallest `ingest_time`, nanoseconds.
    pub(crate) min_ingest_time: i64,
    /// Largest `ingest_time`, nanoseconds.
    pub(crate) max_ingest_time: i64,
}

/// What [`BatchFileWriter::finish`] measured. The footer size is computed by the
/// caller from the file's last 8 bytes ([`footer_size_from_tail`]), because
/// only the caller can read `W` back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileSummary {
    /// Rows written.
    pub(crate) rows: u64,
    /// Smallest event time, nanoseconds.
    pub(crate) min_time: i64,
    /// Largest event time, nanoseconds.
    pub(crate) max_time: i64,
    /// Smallest `ingest_time`, nanoseconds.
    pub(crate) min_ingest_time: i64,
    /// Largest `ingest_time`, nanoseconds.
    pub(crate) max_ingest_time: i64,
}

pub(crate) fn sort_columns(table: FileTable) -> &'static [&'static str] {
    match table {
        FileTable::Spans => &[spans::TRACE_ID, spans::START_TIME],
        FileTable::Traces => &[traces::START_TIME, traces::TRACE_ID],
        FileTable::Scores => &[scores::CREATED_AT],
    }
}

pub(crate) fn time_column(table: FileTable) -> &'static str {
    match table {
        FileTable::Spans => spans::START_TIME,
        FileTable::Traces => traces::START_TIME,
        FileTable::Scores => scores::CREATED_AT,
    }
}

pub(crate) fn ingest_column(table: FileTable) -> &'static str {
    match table {
        FileTable::Spans => spans::INGEST_TIME,
        FileTable::Traces => traces::LAST_INGEST_TIME,
        FileTable::Scores => scores::INGEST_TIME,
    }
}

/// Footer size (metadata length + 8) from a file's last 8 bytes and its length.
/// Errors unless the bytes end in `PAR1` and the footer fits in the file.
pub(crate) fn footer_size_from_tail(
    tail: &[u8; 8],
    file_len: u64,
) -> Result<u64, ParquetWriteError> {
    if &tail[4..] != b"PAR1" {
        return Err(ParquetWriteError::invalid("parquet footer"));
    }
    let mut length = [0_u8; 4];
    length.copy_from_slice(&tail[..4]);
    let metadata = u64::from(u32::from_le_bytes(length));
    let total = metadata + 8;
    if total > file_len {
        return Err(ParquetWriteError::invalid("parquet footer"));
    }
    Ok(total)
}

/// The `cetra.format_version` of a file, if it has one that parses.
#[cfg(test)]
pub(crate) fn file_format_version(metadata: &ParquetMetaData) -> Option<u16> {
    metadata
        .file_metadata()
        .key_value_metadata()?
        .iter()
        .find(|pair| pair.key == KEY_FORMAT_VERSION)?
        .value
        .as_deref()?
        .parse()
        .ok()
}

const ROW_GROUP_ROWS: usize = 100_000;
const ROW_GROUP_BYTES: usize = 64 * 1024 * 1024;
const WRITE_SLICE_ROWS: usize = 8_192;
const DATA_PAGE_BYTES: usize = 1024 * 1024;
const DATA_PAGE_ROWS: usize = 20_000;
const ZSTD_LEVEL: i32 = 3;
const BLOOM_FPP: f64 = 0.01;
const MAX_METADATA_DECODE_ROUNDS: usize = 8;
const VERIFY_COALESCE_BYTES: u64 = 16 * 1024 * 1024;
const VERIFY_READ_BATCH: usize = 4_096;
const READ_BATCH_ROWS: usize = 8_192;
const MAX_ROWS_PER_FILE_BYTE: u64 = 8_192;

#[derive(Debug, Clone, Copy)]
struct Bounds {
    min_time: i64,
    max_time: i64,
    min_ingest_time: i64,
    max_ingest_time: i64,
}

/// Sorts, encodes, and returns one Parquet file from `cetra_column::Batch` inputs.
///
/// CPU-bound: call from `spawn_blocking`. `level` is `0` or `1`.
pub(crate) fn write_file_batch(
    table: FileTable,
    level: u8,
    batches: &[Arc<Batch>],
) -> Result<WrittenFile, ParquetWriteError> {
    if level > 1 {
        return Err(ParquetWriteError::invalid("level must be 0 or 1"));
    }
    let col_schema = table.column_schema();
    let mut non_empty: Vec<&Batch> = Vec::new();
    for batch in batches {
        if batch.schema().fields() != col_schema.fields() {
            return Err(ParquetWriteError::invalid("batch schema"));
        }
        if batch.num_rows() > 0 {
            non_empty.push(batch.as_ref());
        }
    }
    if non_empty.is_empty() {
        return Err(ParquetWriteError::invalid("empty batch"));
    }

    let combined = Batch::concat(Arc::clone(&col_schema), &non_empty, None)
        .map_err(ParquetWriteError::from_column)?;
    let sorted = sort_batch(table, &combined)?;
    let rows =
        u64::try_from(sorted.num_rows()).map_err(|_| ParquetWriteError::invalid("row count"))?;
    let mut writer = BatchFileWriter::new(table, level, Vec::new(), rows)?;
    writer.write(&sorted)?;
    let (buffer, summary) = writer.finish()?;
    let bytes = Bytes::from(buffer);
    let file_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let tail_bytes = bytes
        .get(bytes.len().saturating_sub(8)..)
        .ok_or_else(|| ParquetWriteError::invalid("parquet footer"))?;
    let tail: &[u8; 8] = tail_bytes
        .try_into()
        .map_err(|_| ParquetWriteError::invalid("parquet footer"))?;
    let footer_size = footer_size_from_tail(tail, file_len)?;
    Ok(WrittenFile {
        bytes,
        rows: summary.rows,
        footer_size,
        min_time: summary.min_time,
        max_time: summary.max_time,
        min_ingest_time: summary.min_ingest_time,
        max_ingest_time: summary.max_ingest_time,
    })
}

/// Encodes one Parquet file from `cetra_column::Batch`es already in the table's sort order.
///
/// CPU-bound and blocking: call from `spawn_blocking`. Converts each 8,192-row slice to
/// Arrow strictly inside [`Self::write`] to bound peak conversion memory.
pub(crate) struct BatchFileWriter<W: Write + Send> {
    table: FileTable,
    level: u8,
    col_schema: Arc<Schema>,
    arrow_schema: SchemaRef,
    writer: ArrowWriter<W>,
    bounds: Option<Bounds>,
    rows: u64,
}

impl<W: Write + Send> std::fmt::Debug for BatchFileWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchFileWriter")
            .field("table", &self.table.as_str())
            .field("level", &self.level)
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Send> BatchFileWriter<W> {
    /// `expected_rows` sizes the bloom filters and is only a hint.
    pub(crate) fn new(
        table: FileTable,
        level: u8,
        sink: W,
        expected_rows: u64,
    ) -> Result<Self, ParquetWriteError> {
        if level > 1 {
            return Err(ParquetWriteError::invalid("level must be 0 or 1"));
        }
        let col_schema = table.column_schema();
        let arrow_schema = table_schema(table);
        let properties = writer_properties(table, expected_rows, arrow_schema.as_ref())?;
        let writer = ArrowWriter::try_new(sink, Arc::clone(&arrow_schema), Some(properties))
            .map_err(ParquetWriteError::parquet)?;
        Ok(Self {
            table,
            level,
            col_schema,
            arrow_schema,
            writer,
            bounds: None,
            rows: 0,
        })
    }

    /// Appends one batch. An empty batch is ignored.
    pub(crate) fn write(&mut self, batch: &Batch) -> Result<(), ParquetWriteError> {
        if batch.schema().fields() != self.col_schema.fields() {
            return Err(ParquetWriteError::invalid("batch schema"));
        }
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch_bounds = extract_bounds(self.table, batch)?;
        let owned_compacted;
        let dense = if batch.selection().is_all(batch.raw_len()) {
            batch
        } else {
            owned_compacted = batch
                .compact(None)
                .map_err(ParquetWriteError::from_column)?;
            &owned_compacted
        };

        let num_rows = dense.num_rows();
        let mut offset = 0usize;
        while offset < num_rows {
            let room = ROW_GROUP_ROWS.saturating_sub(self.writer.in_progress_rows());
            let len = WRITE_SLICE_ROWS.min(room.max(1)).min(num_rows - offset);
            let slice = dense
                .slice(offset, len)
                .map_err(ParquetWriteError::from_column)?;
            let arrow_slice = batch_to_record_batch(&slice, &self.arrow_schema)
                .map_err(ParquetWriteError::from_column)?;
            self.writer
                .write(&arrow_slice)
                .map_err(ParquetWriteError::parquet)?;
            offset += len;
            if self.writer.in_progress_rows() >= ROW_GROUP_ROWS
                || self.writer.in_progress_size() >= ROW_GROUP_BYTES
            {
                self.writer.flush().map_err(ParquetWriteError::parquet)?;
            }
        }

        let bounds = self.bounds.get_or_insert(batch_bounds);
        bounds.min_time = bounds.min_time.min(batch_bounds.min_time);
        bounds.max_time = bounds.max_time.max(batch_bounds.max_time);
        bounds.min_ingest_time = bounds.min_ingest_time.min(batch_bounds.min_ingest_time);
        bounds.max_ingest_time = bounds.max_ingest_time.max(batch_bounds.max_ingest_time);
        self.rows = self
            .rows
            .checked_add(
                u64::try_from(num_rows).map_err(|_| ParquetWriteError::invalid("row count"))?,
            )
            .ok_or_else(|| ParquetWriteError::invalid("row count"))?;
        Ok(())
    }

    /// Writes the footer and returns the sink and file summary.
    ///
    /// A file with no rows is an error.
    pub(crate) fn finish(mut self) -> Result<(W, FileSummary), ParquetWriteError> {
        let bounds = self
            .bounds
            .ok_or_else(|| ParquetWriteError::invalid("empty file"))?;
        for pair in file_metadata(self.table, self.level, &bounds) {
            self.writer.append_key_value_metadata(pair);
        }
        let summary = FileSummary {
            rows: self.rows,
            min_time: bounds.min_time,
            max_time: bounds.max_time,
            min_ingest_time: bounds.min_ingest_time,
            max_ingest_time: bounds.max_ingest_time,
        };
        let sink = self
            .writer
            .into_inner()
            .map_err(ParquetWriteError::parquet)?;
        Ok((sink, summary))
    }
}

fn sort_batch(table: FileTable, batch: &Batch) -> Result<Batch, ParquetWriteError> {
    let schema = batch.schema();
    let names = sort_columns(table);
    let mut keys = Vec::with_capacity(names.len());
    let mut key_cols = Vec::with_capacity(names.len());
    for name in names {
        let idx = schema
            .index_of(name)
            .ok_or_else(|| ParquetWriteError::invalid("missing column"))?;
        let col = batch
            .column(idx)
            .ok_or_else(|| ParquetWriteError::invalid("missing column"))?;
        let dt = col.data_type();
        keys.push(SortKey {
            expr: Expr::Column {
                index: idx,
                name: (*name).to_owned(),
                data_type: dt,
            },
            descending: false,
            nulls_first: false,
        });
        key_cols.push(col.clone());
    }

    let mut phys_indices = batch
        .selection()
        .to_indices(batch.raw_len())
        .map_err(ParquetWriteError::from_column)?;
    phys_indices.sort_by(|&a, &b| compare_rows(&key_cols, &keys, a as usize, b as usize));
    batch
        .with_selection(Selection::Indices(phys_indices))
        .and_then(|reordered| reordered.compact(None))
        .map_err(ParquetWriteError::from_column)
}

fn extract_bounds(table: FileTable, batch: &Batch) -> Result<Bounds, ParquetWriteError> {
    let (min_time, max_time) = column_ts_bounds(batch, time_column(table))?;
    let (min_ingest_time, max_ingest_time) = column_ts_bounds(batch, ingest_column(table))?;
    Ok(Bounds {
        min_time,
        max_time,
        min_ingest_time,
        max_ingest_time,
    })
}

fn column_ts_bounds(batch: &Batch, name: &str) -> Result<(i64, i64), ParquetWriteError> {
    let col = batch
        .column_by_name(name)
        .ok_or_else(|| ParquetWriteError::invalid("missing column"))?;
    if !matches!(col.data_type(), DataType::TimestampNs { .. }) {
        return Err(ParquetWriteError::invalid("invalid time column"));
    }

    if let Column::Const { value, len } = col {
        if *len == 0 || batch.num_rows() == 0 {
            return Err(ParquetWriteError::invalid("empty time column"));
        }
        return match value {
            ScalarValue::TimestampNs { value, .. } => Ok((*value, *value)),
            ScalarValue::Null(_) => Err(ParquetWriteError::invalid("empty time column")),
            _ => Err(ParquetWriteError::invalid("invalid time column")),
        };
    }

    let mut min_val: Option<i64> = None;
    let mut max_val: Option<i64> = None;
    let mut visit = |row: usize| {
        if let Some(v) = read_ts(col, row) {
            min_val = Some(min_val.map_or(v, |cur| cur.min(v)));
            max_val = Some(max_val.map_or(v, |cur| cur.max(v)));
        }
    };
    match batch.selection() {
        Selection::All => {
            for row in 0..batch.raw_len() {
                visit(row);
            }
        }
        Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                visit(row);
            }
        }
        Selection::Indices(indices) => {
            for &row in indices {
                visit(row as usize);
            }
        }
    }
    match (min_val, max_val) {
        (Some(lo), Some(hi)) => Ok((lo, hi)),
        _ => Err(ParquetWriteError::invalid("empty time column")),
    }
}

fn read_ts(col: &Column, row: usize) -> Option<i64> {
    match col {
        Column::Flat(flat) => {
            if flat.is_null(row) {
                return None;
            }
            match flat.data() {
                FlatData::TimestampNs { values, .. } => values.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        }),
        Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                return None;
            }
            match dict.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                return None;
            }
            match rle.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(run).copied(),
                _ => None,
            }
        }
    }
}

fn table_schema(table: FileTable) -> SchemaRef {
    match table {
        FileTable::Spans => arrow_spans_schema(),
        FileTable::Traces => arrow_traces_schema(),
        FileTable::Scores => arrow_scores_schema(),
    }
}

fn dictionary_columns(table: FileTable) -> &'static [&'static str] {
    match table {
        FileTable::Spans => SPANS_DICTIONARY_COLUMNS,
        FileTable::Traces => TRACES_DICTIONARY_COLUMNS,
        FileTable::Scores => SCORES_DICTIONARY_COLUMNS,
    }
}

fn bloom_columns(table: FileTable) -> &'static [&'static str] {
    match table {
        FileTable::Spans => SPANS_BLOOM_COLUMNS,
        FileTable::Traces => TRACES_BLOOM_COLUMNS,
        FileTable::Scores => SCORES_BLOOM_COLUMNS,
    }
}

fn writer_properties(
    table: FileTable,
    expected_rows: u64,
    schema: &arrow::datatypes::Schema,
) -> Result<WriterProperties, ParquetWriteError> {
    let zstd = ZstdLevel::try_new(ZSTD_LEVEL).map_err(ParquetWriteError::parquet)?;
    let descriptor = ArrowSchemaConverter::new()
        .convert(schema)
        .map_err(ParquetWriteError::parquet)?;
    let mut builder = WriterProperties::builder()
        .set_compression(Compression::ZSTD(zstd))
        .set_dictionary_enabled(false)
        .set_data_page_size_limit(DATA_PAGE_BYTES)
        .set_data_page_row_count_limit(DATA_PAGE_ROWS)
        .set_statistics_enabled(EnabledStatistics::Page)
        .set_offset_index_disabled(false)
        .set_max_row_group_row_count(None)
        .set_sorting_columns(Some(sorting_columns(table, &descriptor)?));
    for name in dictionary_columns(table) {
        let mut matched = false;
        for column in descriptor.columns() {
            if column.path().parts().first().map(String::as_str) == Some(*name) {
                builder = builder.set_column_dictionary_enabled(column.path().clone(), true);
                matched = true;
            }
        }
        if !matched {
            return Err(ParquetWriteError::invalid("dictionary column"));
        }
    }
    let max_ndv = expected_rows.clamp(1, ROW_GROUP_ROWS as u64);
    for name in bloom_columns(table) {
        let path = leaf_path(&descriptor, name)?;
        builder = builder
            .set_column_bloom_filter_enabled(path.clone(), true)
            .set_column_bloom_filter_fpp(path.clone(), BLOOM_FPP)
            .set_column_bloom_filter_max_ndv(path, max_ndv);
    }
    Ok(builder.build())
}

fn sorting_columns(
    table: FileTable,
    descriptor: &SchemaDescriptor,
) -> Result<Vec<SortingColumn>, ParquetWriteError> {
    let mut columns = Vec::new();
    for name in sort_columns(table) {
        columns.push(SortingColumn {
            column_idx: leaf_index(descriptor, name)?,
            descending: false,
            nulls_first: false,
        });
    }
    Ok(columns)
}

fn leaf_path(descriptor: &SchemaDescriptor, name: &str) -> Result<ColumnPath, ParquetWriteError> {
    let index = usize::try_from(leaf_index(descriptor, name)?)
        .map_err(|_| ParquetWriteError::invalid("column index"))?;
    descriptor
        .columns()
        .get(index)
        .map(|column| column.path().clone())
        .ok_or_else(|| ParquetWriteError::invalid("missing column"))
}

fn leaf_index(descriptor: &SchemaDescriptor, name: &str) -> Result<i32, ParquetWriteError> {
    let mut found = None;
    for (index, column) in descriptor.columns().iter().enumerate() {
        let parts = column.path().parts();
        if parts.len() == 1 && parts[0] == name {
            if found.is_some() {
                return Err(ParquetWriteError::invalid("duplicate column"));
            }
            found =
                Some(i32::try_from(index).map_err(|_| ParquetWriteError::invalid("column index"))?);
        }
    }
    found.ok_or_else(|| ParquetWriteError::invalid("missing column"))
}

fn file_metadata(table: FileTable, level: u8, bounds: &Bounds) -> Vec<KeyValue> {
    let pairs = [
        (KEY_TABLE, table.as_str().to_owned()),
        (KEY_SCHEMA_VERSION, SCHEMA_VERSION.to_owned()),
        (KEY_FORMAT_VERSION, FORMAT_VERSION.to_string()),
        (KEY_LEVEL, level.to_string()),
        (KEY_MIN_TIME, bounds.min_time.to_string()),
        (KEY_MAX_TIME, bounds.max_time.to_string()),
        (KEY_MIN_INGEST_TIME, bounds.min_ingest_time.to_string()),
        (KEY_MAX_INGEST_TIME, bounds.max_ingest_time.to_string()),
    ];
    pairs
        .into_iter()
        .map(|(key, value)| KeyValue::new(key.to_owned(), value))
        .collect()
}

// ---------------------------------------------------------------------------
// Zero-Arrow low-level Parquet readers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SliceChunkReader {
    file_len: u64,
    slices: Arc<[(u64, Bytes)]>,
}

impl SliceChunkReader {
    fn from_single(start: u64, bytes: Bytes, file_len: u64) -> Self {
        Self {
            file_len,
            slices: Arc::from([(start, bytes)]),
        }
    }

    fn from_ranges(file_len: u64, ranges: &[Range<u64>], bufs: Vec<Bytes>) -> Self {
        let slices: Vec<(u64, Bytes)> = ranges
            .iter()
            .zip(bufs)
            .map(|(range, bytes)| (range.start, bytes))
            .collect();
        Self {
            file_len,
            slices: slices.into(),
        }
    }

    fn find_slice(&self, start: u64, len: usize) -> parquet::errors::Result<Bytes> {
        let end = start
            .checked_add(len as u64)
            .ok_or_else(|| ParquetError::General("byte range overflow".to_owned()))?;
        let idx = self.slices.partition_point(|(s, _)| *s <= start);
        if idx == 0 {
            return Err(ParquetError::General(format!(
                "missing fetched byte range at {start}..{end}"
            )));
        }
        let (s, ref bytes) = self.slices[idx - 1];
        let slice_end = s + bytes.len() as u64;
        if end > slice_end {
            return Err(ParquetError::General(format!(
                "truncated fetched byte range at {start}..{end} (have {s}..{slice_end})"
            )));
        }
        let rel_start = (start - s) as usize;
        let rel_end = (end - s) as usize;
        Ok(bytes.slice(rel_start..rel_end))
    }
}

impl Length for SliceChunkReader {
    fn len(&self) -> u64 {
        self.file_len
    }
}

impl ChunkReader for SliceChunkReader {
    type T = bytes::buf::Reader<Bytes>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        let idx = self.slices.partition_point(|(s, _)| *s <= start);
        if idx == 0 {
            return Err(ParquetError::General(format!(
                "missing fetched byte range at {start}"
            )));
        }
        let (s, ref bytes) = self.slices[idx - 1];
        let slice_end = s + bytes.len() as u64;
        if start > slice_end {
            return Err(ParquetError::General(format!(
                "offset {start} outside fetched slice {s}..{slice_end}"
            )));
        }
        Ok(bytes.slice((start - s) as usize..).reader())
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        self.find_slice(start, length)
    }
}

async fn fetch_ranges_guarded(
    store: &Arc<dyn ObjectStore>,
    path: &ObjectPath,
    ranges: &[Range<u64>],
) -> Result<Vec<Bytes>, ParquetError> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    if ranges.iter().any(|r| r.start >= r.end) {
        return Err(ParquetError::General(
            "empty or inverted byte range".to_owned(),
        ));
    }
    let fut = store.get_ranges(path, ranges);
    let caught = AssertUnwindSafe(fut).catch_unwind().await;
    match caught {
        Ok(Ok(bufs)) => {
            if bufs.len() != ranges.len()
                || bufs
                    .iter()
                    .zip(ranges)
                    .any(|(b, r)| b.len() as u64 != r.end.saturating_sub(r.start))
            {
                return Err(ParquetError::General(
                    "fetched byte range length mismatch".to_owned(),
                ));
            }
            Ok(bufs)
        }
        Ok(Err(err)) => Err(ParquetError::External(Box::new(err))),
        Err(_) => Err(decoder_panicked()),
    }
}

async fn load_metadata(
    store: &Arc<dyn ObjectStore>,
    path: &ObjectPath,
    file_size: u64,
    footer_hint: u64,
    page_index_policy: PageIndexPolicy,
) -> Result<ParquetMetaData, ParquetError> {
    if file_size < 8 {
        return Err(ParquetError::General("parquet file too small".to_owned()));
    }
    let tail_len = file_size.min(footer_hint.max(8));
    let tail_range = (file_size - tail_len)..file_size;
    let tail_bufs = fetch_ranges_guarded(store, path, std::slice::from_ref(&tail_range)).await?;
    let [tail_bytes]: [Bytes; 1] = tail_bufs
        .try_into()
        .map_err(|_| ParquetError::General("unexpected tail buffer count".to_owned()))?;
    if tail_bytes.len() < 8 {
        return Err(ParquetError::General("parquet footer truncated".to_owned()));
    }
    let tail_8: [u8; 8] = tail_bytes[tail_bytes.len() - 8..]
        .try_into()
        .map_err(|_| ParquetError::General("parquet footer truncated".to_owned()))?;
    footer_size_from_tail(&tail_8, file_size)
        .map_err(|_| ParquetError::General("invalid parquet footer".to_owned()))?;

    let mut decoder = guard_sync(|| {
        let mut dec = ParquetMetaDataPushDecoder::try_new(file_size)?
            .with_page_index_policy(page_index_policy);
        dec.push_range(tail_range, tail_bytes)?;
        Ok(dec)
    })?;

    for _ in 0..MAX_METADATA_DECODE_ROUNDS {
        let step = guard_sync(|| decoder.try_decode())?;
        match step {
            DecodeResult::Data(meta) => return Ok(meta),
            DecodeResult::Finished => {
                return Err(ParquetError::General(
                    "metadata decoder finished without metadata".to_owned(),
                ));
            }
            DecodeResult::NeedsData(ranges) => {
                for r in &ranges {
                    if r.start >= r.end || r.end > file_size {
                        return Err(ParquetError::General(
                            "metadata range out of bounds".to_owned(),
                        ));
                    }
                }
                let bufs = fetch_ranges_guarded(store, path, &ranges).await?;
                guard_sync(|| decoder.push_ranges(ranges, bufs))?;
            }
        }
    }

    Err(ParquetError::General(
        "metadata decode exceeded maximum rounds".to_owned(),
    ))
}

/// Verifies `file` whole from `store`: head size, footer & page indexes (`PageIndexPolicy::Required`),
/// every leaf column of every row group via `SerializedPageReader` + `get_column_reader`, and row count.
pub(crate) async fn verify_parquet_file(store: &Arc<dyn ObjectStore>, file: &FileMeta) -> Verdict {
    let path = ObjectPath::from(file.path.as_str());
    let head_fut = store.head(&path);
    match AssertUnwindSafe(head_fut).catch_unwind().await {
        Ok(Ok(meta)) => {
            if meta.size != file.size {
                return Verdict::Corrupt {
                    reason: "size differs from the catalog",
                };
            }
        }
        Ok(Err(object_store::Error::NotFound { .. })) => {
            return Verdict::Corrupt {
                reason: "missing object",
            };
        }
        Ok(Err(_)) => return Verdict::Unavailable,
        Err(_) => {
            return Verdict::Corrupt {
                reason: "decode failed",
            };
        }
    }

    let failed = |error: &ParquetError| {
        if is_store_error(error) {
            Verdict::Unavailable
        } else {
            Verdict::Corrupt {
                reason: "decode failed",
            }
        }
    };

    let metadata = match load_metadata(
        store,
        &path,
        file.size,
        file.footer_size,
        PageIndexPolicy::Required,
    )
    .await
    {
        Ok(meta) => meta,
        Err(ref error) => return failed(error),
    };

    let descr = metadata.file_metadata().schema_descr_ptr();
    let mut total_rows = 0_u64;
    for rg_idx in 0..metadata.num_row_groups() {
        let rg = metadata.row_group(rg_idx);
        let Ok(rg_rows) = checked_rg_num_rows(rg, file.size) else {
            return Verdict::Corrupt {
                reason: "decode failed",
            };
        };
        let rg_rows_u64 = rg_rows as u64;
        let Some(next_total) = total_rows.checked_add(rg_rows_u64) else {
            return Verdict::Corrupt {
                reason: "row count differs from the catalog",
            };
        };
        if next_total > file.rows {
            return Verdict::Corrupt {
                reason: "row count differs from the catalog",
            };
        }
        total_rows = next_total;

        let num_cols = rg.num_columns();
        if num_cols != descr.num_columns() {
            return Verdict::Corrupt {
                reason: "decode failed",
            };
        }

        // Fetch column chunks in coalesced groups bounded by `VERIFY_COALESCE_BYTES`.
        let mut leaf_ranges: Vec<(usize, Range<u64>)> = Vec::with_capacity(num_cols);
        for leaf_idx in 0..num_cols {
            let range = match checked_column_byte_range(rg, leaf_idx, file.size) {
                Ok(range) => range,
                Err(ref error) => return failed(error),
            };
            if range.is_empty() {
                if rg_rows == 0 {
                    continue;
                }
                return Verdict::Corrupt {
                    reason: "decode failed",
                };
            }
            leaf_ranges.push((leaf_idx, range));
        }

        let mut cursor = 0usize;
        while cursor < leaf_ranges.len() {
            let mut batch_bytes = 0_u64;
            let mut fetch_ranges = Vec::new();
            let group_start = cursor;
            while cursor < leaf_ranges.len() {
                let range = &leaf_ranges[cursor].1;
                let len = range.end.saturating_sub(range.start);
                if !fetch_ranges.is_empty()
                    && batch_bytes.saturating_add(len) > VERIFY_COALESCE_BYTES
                {
                    break;
                }
                batch_bytes = batch_bytes.saturating_add(len);
                fetch_ranges.push(range.clone());
                cursor += 1;
            }
            let bufs = match fetch_ranges_guarded(store, &path, &fetch_ranges).await {
                Ok(bufs) => bufs,
                Err(ref error) => return failed(error),
            };
            for ((leaf_idx, range), bytes) in leaf_ranges[group_start..cursor].iter().zip(bufs) {
                let col_chunk = rg.column(*leaf_idx);
                let col_descr = descr.column(*leaf_idx);
                let chunk_reader =
                    Arc::new(SliceChunkReader::from_single(range.start, bytes, file.size));
                let res = guard_sync(|| {
                    let page_reader = SerializedPageReader::new_with_properties(
                        chunk_reader,
                        col_chunk,
                        rg_rows,
                        None,
                        Arc::new(ReaderProperties::builder().build()),
                    )?;
                    drain_any_column(&col_descr, page_reader, rg_rows)
                });
                if let Err(ref error) = res {
                    return failed(error);
                }
            }
        }
    }

    if total_rows != file.rows {
        return Verdict::Corrupt {
            reason: "row count differs from the catalog",
        };
    }
    Verdict::Healthy
}

fn checked_rg_num_rows(rg: &RowGroupMetaData, file_size: u64) -> Result<usize, ParquetError> {
    let n = rg.num_rows();
    if n < 0 || n > i64::from(u32::MAX) {
        return Err(ParquetError::General(
            "row group row count out of range".to_owned(),
        ));
    }
    let n_u64 = n as u64;
    if n_u64 > file_size.saturating_mul(MAX_ROWS_PER_FILE_BYTE) {
        return Err(ParquetError::General(
            "row group row count exceeds expansion bound".to_owned(),
        ));
    }
    usize::try_from(n_u64)
        .map_err(|_| ParquetError::General("row group row count out of range".to_owned()))
}

fn checked_column_byte_range(
    rg: &RowGroupMetaData,
    leaf_idx: usize,
    file_size: u64,
) -> Result<Range<u64>, ParquetError> {
    if leaf_idx >= rg.num_columns() {
        return Err(ParquetError::General("row group missing column".to_owned()));
    }
    let col = rg.column(leaf_idx);
    if col.data_page_offset() < 0
        || col.dictionary_page_offset().is_some_and(|off| off < 0)
        || col.compressed_size() < 0
    {
        return Err(ParquetError::General(
            "negative column chunk byte range".to_owned(),
        ));
    }
    let (chunk_start, chunk_len) = guard_sync(|| Ok(col.byte_range()))?;
    let chunk_end = chunk_start
        .checked_add(chunk_len)
        .ok_or_else(|| ParquetError::General("column chunk byte range overflow".to_owned()))?;
    if chunk_end > file_size {
        return Err(ParquetError::General(
            "column chunk extends past file end".to_owned(),
        ));
    }
    Ok(chunk_start..chunk_end)
}

fn is_timestamp_nanos(col: &ColumnDescPtr) -> bool {
    col.max_rep_level() == 0
        && col.physical_type() == PhysicalType::INT64
        && matches!(
            col.logical_type_ref(),
            Some(LogicalType::Timestamp(ts)) if ts.unit == TimeUnit::NANOS
        )
}

fn drain_any_column(
    descr: &ColumnDescPtr,
    page_reader: SerializedPageReader<SliceChunkReader>,
    expected_rows: usize,
) -> Result<(), ParquetError> {
    let check_utf8 = matches!(descr.logical_type_ref(), Some(LogicalType::String))
        || descr.converted_type() == ConvertedType::UTF8;
    match get_column_reader(descr.clone(), Box::new(page_reader)) {
        ColumnReader::BoolColumnReader(mut r) => drain_column_reader(&mut r, descr, expected_rows),
        ColumnReader::Int32ColumnReader(mut r) => drain_column_reader(&mut r, descr, expected_rows),
        ColumnReader::Int64ColumnReader(mut r) => drain_column_reader(&mut r, descr, expected_rows),
        ColumnReader::Int96ColumnReader(mut r) => drain_column_reader(&mut r, descr, expected_rows),
        ColumnReader::FloatColumnReader(mut r) => drain_column_reader(&mut r, descr, expected_rows),
        ColumnReader::DoubleColumnReader(mut r) => {
            drain_column_reader(&mut r, descr, expected_rows)
        }
        ColumnReader::ByteArrayColumnReader(mut r) => {
            drain_byte_array_reader(&mut r, descr, expected_rows, check_utf8)
        }
        ColumnReader::FixedLenByteArrayColumnReader(mut r) => {
            drain_column_reader(&mut r, descr, expected_rows)
        }
    }
}

fn drain_column_reader<T: parquet::data_type::DataType>(
    reader: &mut ColumnReaderImpl<T>,
    descr: &ColumnDescPtr,
    expected_rows: usize,
) -> Result<(), ParquetError> {
    let max_def = descr.max_def_level();
    let max_rep = descr.max_rep_level();
    let mut def_levels = Vec::new();
    let mut rep_levels = Vec::new();
    let mut values: Vec<T::T> = Vec::new();
    let mut total = 0usize;
    while total < expected_rows {
        def_levels.clear();
        rep_levels.clear();
        values.clear();
        let need = (expected_rows - total).min(VERIFY_READ_BATCH);
        let (records_read, _, _) = reader.read_records(
            need,
            (max_def > 0).then_some(&mut def_levels),
            (max_rep > 0).then_some(&mut rep_levels),
            &mut values,
        )?;
        if records_read == 0 {
            return Err(ParquetError::General(
                "unexpected EOF in parquet column".to_owned(),
            ));
        }
        total += records_read;
    }
    def_levels.clear();
    rep_levels.clear();
    values.clear();
    let (extra, extra_vals, extra_levels) = reader.read_records(
        1,
        (max_def > 0).then_some(&mut def_levels),
        (max_rep > 0).then_some(&mut rep_levels),
        &mut values,
    )?;
    if extra != 0 || extra_vals != 0 || extra_levels != 0 {
        return Err(ParquetError::General(
            "extra records beyond row group count".to_owned(),
        ));
    }
    Ok(())
}

fn drain_byte_array_reader(
    reader: &mut ColumnReaderImpl<ByteArrayType>,
    descr: &ColumnDescPtr,
    expected_rows: usize,
    check_utf8: bool,
) -> Result<(), ParquetError> {
    let max_def = descr.max_def_level();
    let max_rep = descr.max_rep_level();
    let mut def_levels = Vec::new();
    let mut rep_levels = Vec::new();
    let mut values = Vec::new();
    let mut total = 0usize;
    while total < expected_rows {
        def_levels.clear();
        rep_levels.clear();
        values.clear();
        let need = (expected_rows - total).min(VERIFY_READ_BATCH);
        let (records_read, _, _) = reader.read_records(
            need,
            (max_def > 0).then_some(&mut def_levels),
            (max_rep > 0).then_some(&mut rep_levels),
            &mut values,
        )?;
        if records_read == 0 {
            return Err(ParquetError::General(
                "unexpected EOF in parquet column".to_owned(),
            ));
        }
        if check_utf8 {
            for val in &values {
                std::str::from_utf8(val.data()).map_err(|_| {
                    ParquetError::General("invalid UTF-8 in parquet column".to_owned())
                })?;
            }
        }
        total += records_read;
    }
    def_levels.clear();
    rep_levels.clear();
    values.clear();
    let (extra, extra_vals, extra_levels) = reader.read_records(
        1,
        (max_def > 0).then_some(&mut def_levels),
        (max_rep > 0).then_some(&mut rep_levels),
        &mut values,
    )?;
    if extra != 0 || extra_vals != 0 || extra_levels != 0 {
        return Err(ParquetError::General(
            "extra records beyond row group count".to_owned(),
        ));
    }
    Ok(())
}

/// Projects one non-null timestamp column across `files`, returning values sorted ascending.
///
/// Preserves store-vs-corrupt distinction (`may_be_corrupt`) via `CompactionError::Read`.
pub(crate) async fn read_time_column(
    store: &Arc<dyn ObjectStore>,
    files: &[Arc<FileMeta>],
    column: &str,
) -> Result<Vec<i64>, CompactionError> {
    let mut values = Vec::new();
    for file in files {
        let path = ObjectPath::from(file.path.as_str());
        let meta = load_metadata(
            store,
            &path,
            file.size,
            file.footer_size,
            PageIndexPolicy::Optional,
        )
        .await
        .map_err(|e| compaction_parquet_error(&e))?;

        let descr = meta.file_metadata().schema_descr_ptr();
        let Some(leaf_idx) = descr
            .columns()
            .iter()
            .position(|col| col.path().parts() == [column] && is_timestamp_nanos(col))
        else {
            return Err(CompactionError::Input {
                reason: "missing time column",
            });
        };
        let col_descr = descr.column(leaf_idx);

        for rg_idx in 0..meta.num_row_groups() {
            let rg = meta.row_group(rg_idx);
            let rg_rows =
                checked_rg_num_rows(rg, file.size).map_err(|e| compaction_parquet_error(&e))?;
            let range = checked_column_byte_range(rg, leaf_idx, file.size)
                .map_err(|e| compaction_parquet_error(&e))?;
            if range.is_empty() {
                if rg_rows == 0 {
                    continue;
                }
                return Err(compaction_parquet_error(&ParquetError::General(
                    "empty column chunk".to_owned(),
                )));
            }
            let bufs = fetch_ranges_guarded(store, &path, std::slice::from_ref(&range))
                .await
                .map_err(|e| compaction_parquet_error(&e))?;
            let [bytes]: [Bytes; 1] = bufs.try_into().map_err(|_| {
                compaction_parquet_error(&ParquetError::General(
                    "unexpected range count".to_owned(),
                ))
            })?;
            let chunk_reader =
                Arc::new(SliceChunkReader::from_single(range.start, bytes, file.size));
            let col_chunk = rg.column(leaf_idx);
            let max_def = col_descr.max_def_level();

            enum RgTimeOutcome {
                Ok(Vec<i64>),
                NullTime,
            }
            let outcome = guard_sync(|| {
                let page_reader = SerializedPageReader::new_with_properties(
                    chunk_reader,
                    col_chunk,
                    rg_rows,
                    None,
                    Arc::new(ReaderProperties::builder().build()),
                )?;
                let ColumnReader::Int64ColumnReader(mut reader) =
                    get_column_reader(col_descr.clone(), Box::new(page_reader))
                else {
                    return Err(ParquetError::General(
                        "expected Int64 column reader".to_owned(),
                    ));
                };
                let initial_cap = rg_rows.min(READ_BATCH_ROWS);
                let mut def_levels = Vec::with_capacity(if max_def > 0 { initial_cap } else { 0 });
                let mut chunk_vals = Vec::with_capacity(initial_cap);
                let mut rg_vals = Vec::with_capacity(initial_cap);
                let mut total_read = 0usize;
                while total_read < rg_rows {
                    def_levels.clear();
                    chunk_vals.clear();
                    let need = (rg_rows - total_read).min(READ_BATCH_ROWS);
                    let (records_read, values_read, levels_read) = reader.read_records(
                        need,
                        (max_def > 0).then_some(&mut def_levels),
                        None,
                        &mut chunk_vals,
                    )?;
                    if records_read == 0 {
                        return Err(ParquetError::General(
                            "unexpected EOF reading time column".to_owned(),
                        ));
                    }
                    if max_def > 0 {
                        if levels_read != records_read || def_levels.len() != records_read {
                            return Err(ParquetError::General(
                                "def level count mismatch".to_owned(),
                            ));
                        }
                        if def_levels.iter().any(|&d| d < max_def) {
                            return Ok(RgTimeOutcome::NullTime);
                        }
                    }
                    if values_read != records_read || chunk_vals.len() != records_read {
                        return Err(ParquetError::General(
                            "time column value count mismatch".to_owned(),
                        ));
                    }
                    rg_vals.extend_from_slice(&chunk_vals);
                    total_read += records_read;
                }
                assert_no_trailing_records(&mut reader, max_def)?;
                if rg_vals.len() != rg_rows {
                    return Err(ParquetError::General(
                        "time column value count mismatch".to_owned(),
                    ));
                }
                Ok(RgTimeOutcome::Ok(rg_vals))
            })
            .map_err(|e| compaction_parquet_error(&e))?;

            match outcome {
                RgTimeOutcome::Ok(mut rg_vals) => values.append(&mut rg_vals),
                RgTimeOutcome::NullTime => {
                    return Err(CompactionError::Input {
                        reason: "missing time column",
                    });
                }
            }
        }
    }
    values.sort_unstable();
    Ok(values)
}

fn compaction_parquet_error(source: &ParquetError) -> CompactionError {
    CompactionError::Read {
        source: EngineError::from_parquet(source),
    }
}

/// Projects `[trace_id, start_time, end_time, last_ingest_time]` from `file` with
/// row-group `last_ingest_time >= since` statistics pruning.
///
/// Preserves [`AnchorError::Parquet`] + [`AnchorError::is_store_error`] distinction.
pub(crate) async fn read_trace_anchors(
    store: &Arc<dyn ObjectStore>,
    file: &FileMeta,
    since: i64,
) -> Result<Vec<Batch>, AnchorError> {
    let path = ObjectPath::from(file.path.as_str());
    let meta = load_metadata(
        store,
        &path,
        file.size,
        file.footer_size,
        PageIndexPolicy::Optional,
    )
    .await
    .map_err(AnchorError::parquet)?;

    let descr = meta.file_metadata().schema_descr_ptr();
    let trace_leaf = descr
        .columns()
        .iter()
        .position(|col| {
            col.path().parts() == [traces::TRACE_ID]
                && col.max_rep_level() == 0
                && col.physical_type() == PhysicalType::FIXED_LEN_BYTE_ARRAY
                && col.type_length() == 16
        })
        .ok_or_else(|| AnchorError::column(traces::TRACE_ID))?;
    let find_ts_leaf = |name: &'static str| -> Result<usize, AnchorError> {
        descr
            .columns()
            .iter()
            .position(|col| col.path().parts() == [name] && is_timestamp_nanos(col))
            .ok_or_else(|| AnchorError::column(name))
    };

    let start_leaf = find_ts_leaf(traces::START_TIME)?;
    let end_leaf = find_ts_leaf(traces::END_TIME)?;
    let ingest_leaf = find_ts_leaf(traces::LAST_INGEST_TIME)?;

    let groups = row_groups_to_read(&meta, since);
    if groups.is_empty() {
        return Ok(Vec::new());
    }

    let anchor_schema = Schema::try_new_arc(vec![
        Field::new(traces::TRACE_ID, DataType::FixedBytes16, true),
        Field::new(traces::START_TIME, DataType::timestamp_utc(), true),
        Field::new(traces::END_TIME, DataType::timestamp_utc(), true),
        Field::new(traces::LAST_INGEST_TIME, DataType::timestamp_utc(), true),
    ])
    .map_err(AnchorError::from_column)?;

    let mut batches = Vec::with_capacity(groups.len().min(READ_BATCH_ROWS));
    for rg_idx in groups {
        let rg = meta.row_group(rg_idx);
        let rg_rows = checked_rg_num_rows(rg, file.size).map_err(AnchorError::parquet)?;
        let leaves = [trace_leaf, start_leaf, end_leaf, ingest_leaf];
        let mut ranges = Vec::with_capacity(leaves.len());
        for &leaf_idx in &leaves {
            let range =
                checked_column_byte_range(rg, leaf_idx, file.size).map_err(AnchorError::parquet)?;
            if range.is_empty() {
                if rg_rows == 0 {
                    continue;
                }
                return Err(AnchorError::parquet(ParquetError::General(
                    "empty column chunk".to_owned(),
                )));
            }
            ranges.push(range);
        }
        if rg_rows == 0 {
            continue;
        }
        let mut sorted_ranges = ranges;
        sorted_ranges.sort_by_key(|r| r.start);
        let coalesced = coalesce_ranges(sorted_ranges);
        let bufs = fetch_ranges_guarded(store, &path, &coalesced)
            .await
            .map_err(AnchorError::parquet)?;
        let chunk_reader = Arc::new(SliceChunkReader::from_ranges(file.size, &coalesced, bufs));

        let mut rg_batches = guard_sync(|| {
            let trace_descr = descr.column(trace_leaf);
            let start_descr = descr.column(start_leaf);
            let end_descr = descr.column(end_leaf);
            let ingest_descr = descr.column(ingest_leaf);

            let mut trace_reader = open_fixed16_reader(
                rg,
                trace_leaf,
                &trace_descr,
                rg_rows,
                Arc::clone(&chunk_reader),
            )?;
            let mut start_reader = open_ts_reader(
                rg,
                start_leaf,
                &start_descr,
                rg_rows,
                Arc::clone(&chunk_reader),
            )?;
            let mut end_reader =
                open_ts_reader(rg, end_leaf, &end_descr, rg_rows, Arc::clone(&chunk_reader))?;
            let mut ingest_reader =
                open_ts_reader(rg, ingest_leaf, &ingest_descr, rg_rows, chunk_reader)?;

            let trace_max_def = trace_descr.max_def_level();
            let start_max_def = start_descr.max_def_level();
            let end_max_def = end_descr.max_def_level();
            let ingest_max_def = ingest_descr.max_def_level();

            let chunk_count = rg_rows.div_ceil(READ_BATCH_ROWS);
            let mut out_batches = Vec::with_capacity(chunk_count.min(READ_BATCH_ROWS));
            let mut remaining = rg_rows;
            while remaining > 0 {
                let chunk_rows = remaining.min(READ_BATCH_ROWS);
                let trace_col = decode_fixed16_chunk(&mut trace_reader, trace_max_def, chunk_rows)?;
                let start_col = decode_ts_chunk(&mut start_reader, start_max_def, chunk_rows)?;
                let end_col = decode_ts_chunk(&mut end_reader, end_max_def, chunk_rows)?;
                let ingest_col = decode_ts_chunk(&mut ingest_reader, ingest_max_def, chunk_rows)?;
                let batch = Batch::try_new_all(
                    Arc::clone(&anchor_schema),
                    vec![trace_col, start_col, end_col, ingest_col],
                    chunk_rows,
                )
                .map_err(|err| ParquetError::General(err.to_string()))?;
                out_batches.push(batch);
                remaining -= chunk_rows;
            }
            assert_no_trailing_records(&mut trace_reader, trace_max_def)?;
            assert_no_trailing_records(&mut start_reader, start_max_def)?;
            assert_no_trailing_records(&mut end_reader, end_max_def)?;
            assert_no_trailing_records(&mut ingest_reader, ingest_max_def)?;
            Ok(out_batches)
        })
        .map_err(AnchorError::parquet)?;
        batches.append(&mut rg_batches);
    }
    Ok(batches)
}

fn coalesce_ranges(ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
    let mut merged: Vec<Range<u64>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        if r.start >= r.end {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && r.start <= last.end
        {
            last.end = last.end.max(r.end);
            continue;
        }
        merged.push(r);
    }
    merged
}

fn open_fixed16_reader(
    rg: &RowGroupMetaData,
    leaf_idx: usize,
    descr: &ColumnDescPtr,
    rg_rows: usize,
    chunk_reader: Arc<SliceChunkReader>,
) -> Result<ColumnReaderImpl<FixedLenByteArrayType>, ParquetError> {
    let page_reader = SerializedPageReader::new_with_properties(
        chunk_reader,
        rg.column(leaf_idx),
        rg_rows,
        None,
        Arc::new(ReaderProperties::builder().build()),
    )?;
    let ColumnReader::FixedLenByteArrayColumnReader(reader) =
        get_column_reader(descr.clone(), Box::new(page_reader))
    else {
        return Err(ParquetError::General(
            "expected FixedLenByteArray column reader".to_owned(),
        ));
    };
    Ok(reader)
}

fn open_ts_reader(
    rg: &RowGroupMetaData,
    leaf_idx: usize,
    descr: &ColumnDescPtr,
    rg_rows: usize,
    chunk_reader: Arc<SliceChunkReader>,
) -> Result<ColumnReaderImpl<Int64Type>, ParquetError> {
    let page_reader = SerializedPageReader::new_with_properties(
        chunk_reader,
        rg.column(leaf_idx),
        rg_rows,
        None,
        Arc::new(ReaderProperties::builder().build()),
    )?;
    let ColumnReader::Int64ColumnReader(reader) =
        get_column_reader(descr.clone(), Box::new(page_reader))
    else {
        return Err(ParquetError::General(
            "expected Int64 column reader".to_owned(),
        ));
    };
    Ok(reader)
}

fn decode_fixed16_chunk(
    reader: &mut ColumnReaderImpl<FixedLenByteArrayType>,
    max_def: i16,
    chunk_rows: usize,
) -> Result<Column, ParquetError> {
    let (out, nulls) = decode_records_chunk(reader, max_def, chunk_rows, [0u8; 16], |item| {
        item.data()
            .try_into()
            .map_err(|_| ParquetError::General("expected 16-byte trace_id".to_owned()))
    })?;
    FlatColumn::try_new(FlatData::FixedBytes16(out), nulls)
        .map(Column::Flat)
        .map_err(|err| ParquetError::General(err.to_string()))
}

fn decode_ts_chunk(
    reader: &mut ColumnReaderImpl<Int64Type>,
    max_def: i16,
    chunk_rows: usize,
) -> Result<Column, ParquetError> {
    let (out, nulls) = decode_records_chunk(reader, max_def, chunk_rows, 0_i64, |&v| Ok(v))?;
    FlatColumn::try_new(
        FlatData::TimestampNs {
            values: out,
            utc: true,
        },
        nulls,
    )
    .map(Column::Flat)
    .map_err(|err| ParquetError::General(err.to_string()))
}

fn decode_records_chunk<T, U>(
    reader: &mut ColumnReaderImpl<T>,
    max_def: i16,
    chunk_rows: usize,
    default_val: U,
    mut map_val: impl FnMut(&T::T) -> Result<U, ParquetError>,
) -> Result<(Vec<U>, Option<Bitmap>), ParquetError>
where
    T: parquet::data_type::DataType,
    U: Copy,
{
    let mut def_levels = Vec::with_capacity(if max_def > 0 { chunk_rows } else { 0 });
    let mut dense = Vec::with_capacity(chunk_rows);
    let mut out = Vec::with_capacity(chunk_rows);
    let mut nulls = (max_def > 0).then(|| Bitmap::new_ones(chunk_rows));
    let mut read_so_far = 0usize;
    while read_so_far < chunk_rows {
        def_levels.clear();
        dense.clear();
        let need = chunk_rows - read_so_far;
        let (records_read, values_read, levels_read) = reader.read_records(
            need,
            (max_def > 0).then_some(&mut def_levels),
            None,
            &mut dense,
        )?;
        if records_read == 0 {
            return Err(ParquetError::General(
                "unexpected EOF reading anchor column".to_owned(),
            ));
        }
        if max_def == 0 {
            if values_read != records_read || dense.len() != records_read {
                return Err(ParquetError::General("anchor count mismatch".to_owned()));
            }
            for item in &dense {
                out.push(map_val(item)?);
            }
        } else {
            if levels_read != records_read || def_levels.len() != records_read {
                return Err(ParquetError::General("def level count mismatch".to_owned()));
            }
            let mut idx = 0usize;
            for (offset, &d) in def_levels.iter().enumerate() {
                if d == max_def {
                    let item = dense.get(idx).ok_or_else(|| {
                        ParquetError::General("missing non-null anchor value".to_owned())
                    })?;
                    idx += 1;
                    out.push(map_val(item)?);
                } else {
                    if let Some(ref mut bitmap) = nulls {
                        let _ = bitmap.set(read_so_far + offset, false);
                    }
                    out.push(default_val);
                }
            }
            if idx != dense.len() || idx != values_read {
                return Err(ParquetError::General("trailing anchor values".to_owned()));
            }
        }
        read_so_far += records_read;
    }
    if out.len() != chunk_rows {
        return Err(ParquetError::General(
            "anchor chunk length mismatch".to_owned(),
        ));
    }
    let nulls = nulls.filter(|b| !b.all_ones());
    Ok((out, nulls))
}

fn assert_no_trailing_records<T: parquet::data_type::DataType>(
    reader: &mut ColumnReaderImpl<T>,
    max_def: i16,
) -> Result<(), ParquetError> {
    let mut def_levels = Vec::new();
    let mut values = Vec::new();
    let (extra, extra_vals, extra_levels) = reader.read_records(
        1,
        (max_def > 0).then_some(&mut def_levels),
        None,
        &mut values,
    )?;
    if extra != 0 || extra_vals != 0 || extra_levels != 0 {
        return Err(ParquetError::General(
            "extra records beyond row group count".to_owned(),
        ));
    }
    Ok(())
}

/// Row groups whose `last_ingest_time` statistics have a max `>= since`.
/// A group without usable statistics is read, so the rebuild is conservative.
fn row_groups_to_read(metadata: &ParquetMetaData, since: i64) -> Vec<usize> {
    metadata
        .row_groups()
        .iter()
        .enumerate()
        .filter(|(_, group)| {
            let chunk = group.columns().iter().find(|column| {
                column
                    .column_descr()
                    .path()
                    .parts()
                    .first()
                    .map(String::as_str)
                    == Some(traces::LAST_INGEST_TIME)
            });
            match chunk.and_then(|column| column.statistics()) {
                Some(Statistics::Int64(stats)) => stats.max_opt().is_none_or(|max| *max >= since),
                _ => true,
            }
        })
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Arc;

    use arrow::array::RecordBatch;
    use arrow::compute::concat_batches;
    use bytes::Bytes;
    use cetra_core::schema::{scores, spans, traces};
    use chrono::NaiveDate;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use parquet::arrow::ArrowWriter;
    use parquet::arrow::arrow_reader::{ArrowReaderOptions, ParquetRecordBatchReaderBuilder};
    use parquet::basic::{Compression, Encoding};
    use parquet::bloom_filter::Sbbf;
    use parquet::file::metadata::{PageIndexPolicy, ParquetMetaDataReader};
    use parquet::file::properties::{EnabledStatistics, WriterProperties};

    use super::*;
    use crate::catalog::FileId;
    use crate::test_support::{
        Call, ChaosStore, Fault, score_row, score_rows, span_row, span_rows, trace_batch,
    };
    use crate::verify::may_be_corrupt;

    fn project() -> cetra_core::ids::ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 11).unwrap()
    }

    fn file_meta(table: FileTable, path: &str, written: &WrittenFile) -> FileMeta {
        FileMeta {
            id: FileId::new_v7(),
            table,
            project: project(),
            day: day(),
            level: 0,
            path: path.to_owned(),
            size: u64::try_from(written.bytes.len()).unwrap(),
            footer_size: written.footer_size,
            rows: written.rows,
            min_time: written.min_time,
            max_time: written.max_time,
            min_ingest_time: written.min_ingest_time,
            max_ingest_time: written.max_ingest_time,
            fully_merged: false,
            deletions_applied_seq: 0,
        }
    }

    fn read_arrow(bytes: &Bytes) -> RecordBatch {
        let options = ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required);
        let builder =
            ParquetRecordBatchReaderBuilder::try_new_with_options(bytes.clone(), options).unwrap();
        let schema = Arc::clone(builder.schema());
        let batches: Vec<_> = builder.build().unwrap().map(|r| r.unwrap()).collect();
        concat_batches(&schema, &batches).unwrap()
    }

    fn metadata(bytes: &Bytes) -> ParquetMetaData {
        let mut reader =
            ParquetMetaDataReader::new().with_page_index_policy(PageIndexPolicy::Required);
        reader.try_parse(&bytes.clone()).unwrap();
        reader.finish().unwrap()
    }

    fn dictionary_names_follow_the_schema(schema: &Schema, columns: &[&str]) {
        let positions: Vec<_> = columns
            .iter()
            .map(|name| schema.index_of(name).expect(name))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let mut unique = columns.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), columns.len());
    }

    #[test]
    fn dictionary_lists_match_the_schemas() {
        dictionary_names_follow_the_schema(
            cetra_core::schema::spans_schema().as_ref(),
            SPANS_DICTIONARY_COLUMNS,
        );
        dictionary_names_follow_the_schema(
            cetra_core::schema::traces_schema().as_ref(),
            TRACES_DICTIONARY_COLUMNS,
        );
        dictionary_names_follow_the_schema(
            cetra_core::schema::scores_schema().as_ref(),
            SCORES_DICTIONARY_COLUMNS,
        );
        assert_eq!(SCHEMA_VERSION, "1");
    }

    fn assert_file_settings(
        bytes: &Bytes,
        table: &str,
        level: &str,
        written: &WrittenFile,
        dictionary: &[&str],
        bloom: &[&str],
        sort: &[&str],
    ) {
        let file = metadata(bytes);
        assert!(file.column_index().is_some());
        assert!(file.offset_index().is_some());
        let pairs = file.file_metadata().key_value_metadata().unwrap();
        let value = |key: &str| {
            pairs
                .iter()
                .find(|pair| pair.key == key)
                .and_then(|pair| pair.value.clone())
                .unwrap()
        };
        assert_eq!(value("cetra.table"), table);
        assert_eq!(value("cetra.schema_version"), SCHEMA_VERSION);
        assert_eq!(value("cetra.format_version"), FORMAT_VERSION.to_string());
        assert_eq!(file_format_version(&file), Some(FORMAT_VERSION));
        assert_eq!(value("cetra.level"), level);
        assert_eq!(value("cetra.min_time"), written.min_time.to_string());
        assert_eq!(value("cetra.max_time"), written.max_time.to_string());
        assert_eq!(
            value("cetra.min_ingest_time"),
            written.min_ingest_time.to_string()
        );
        assert_eq!(
            value("cetra.max_ingest_time"),
            written.max_ingest_time.to_string()
        );
        let tail = &bytes[bytes.len() - 8..];
        let metadata_len = u32::from_le_bytes(tail[..4].try_into().unwrap());
        assert_eq!(&tail[4..], b"PAR1");
        assert_eq!(written.footer_size, u64::from(metadata_len) + 8);

        let dictionary: std::collections::HashSet<_> = dictionary.iter().copied().collect();
        let bloom: std::collections::HashSet<_> = bloom.iter().copied().collect();
        for group_index in 0..file.num_row_groups() {
            let group = file.row_group(group_index);
            let declared: Vec<_> = group
                .sorting_columns()
                .unwrap()
                .iter()
                .map(|column| {
                    group
                        .schema_descr()
                        .column(usize::try_from(column.column_idx).unwrap())
                        .name()
                        .to_owned()
                })
                .collect();
            assert_eq!(declared, sort);
            assert!(
                group
                    .sorting_columns()
                    .unwrap()
                    .iter()
                    .all(|column| !column.descending && !column.nulls_first)
            );
            for column_index in 0..group.num_columns() {
                let column = group.column(column_index);
                let root = column.column_path().parts()[0].as_str();
                assert!(
                    matches!(column.compression(), Compression::ZSTD(_)),
                    "{root}"
                );
                let encoded = column.encodings().any(|encoding| {
                    matches!(
                        encoding,
                        Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY
                    )
                });
                assert_eq!(encoded, dictionary.contains(root), "{root}");
                assert_eq!(
                    column.bloom_filter_offset().is_some(),
                    bloom.contains(root),
                    "{root}"
                );
                assert!(column.offset_index_offset().is_some(), "{root}");
                assert!(column.column_index_offset().is_some(), "{root}");
            }
        }
    }

    fn bloom_contains(bytes: &Bytes, column: &str, value: &[u8]) -> bool {
        let file = metadata(bytes);
        let cursor = bytes.clone();
        for group_index in 0..file.num_row_groups() {
            let group = file.row_group(group_index);
            for column_index in 0..group.num_columns() {
                let chunk = group.column(column_index);
                let parts = chunk.column_path().parts();
                if parts.len() == 1 && parts[0] == column {
                    let Some(filter) = Sbbf::read_from_column_chunk(chunk, &cursor).unwrap() else {
                        continue;
                    };
                    if filter.check(value) {
                        return true;
                    }
                }
            }
        }
        false
    }

    #[test]
    fn batch_file_writer_and_write_file_batch_round_trip() {
        let mut s1 = span_row(2, 1, 100, 30);
        s1.schema_version = 2;
        let s2 = span_row(1, 2, 50, 10);
        let s3 = span_row(1, 1, 20, 40);
        let col_batch = Arc::new(span_rows(&[s1, s2, s3]));

        let batch_written =
            write_file_batch(FileTable::Spans, 0, &[Arc::clone(&col_batch)]).unwrap();
        assert_eq!(batch_written.rows, 3);
        assert_eq!((batch_written.min_time, batch_written.max_time), (20, 100));
        assert_eq!(
            (batch_written.min_ingest_time, batch_written.max_ingest_time),
            (10, 40)
        );
        let rb = read_arrow(&batch_written.bytes);
        assert_eq!(rb.num_rows(), 3);
        assert_file_settings(
            &batch_written.bytes,
            "spans",
            "0",
            &batch_written,
            SPANS_DICTIONARY_COLUMNS,
            &[spans::TRACE_ID, spans::SESSION_ID, spans::USER_ID],
            &[spans::TRACE_ID, spans::START_TIME],
        );
        let trace_bytes = arrow::array::AsArray::as_fixed_size_binary(
            rb.column_by_name(spans::TRACE_ID).unwrap().as_ref(),
        )
        .value(0)
        .to_vec();
        assert!(bloom_contains(
            &batch_written.bytes,
            spans::TRACE_ID,
            &trace_bytes
        ));

        // Traces round-trip (checks Int32 partial_count, KV metadata, and bloom filter).
        let traces_col = Arc::new(trace_batch(&[
            span_row(2, 1, 100, 30),
            span_row(1, 2, 50, 10),
        ]));
        let traces_written = write_file_batch(FileTable::Traces, 0, &[traces_col]).unwrap();
        let tr_rb = read_arrow(&traces_written.bytes);
        assert_eq!(tr_rb.num_rows(), 2);
        assert_file_settings(
            &traces_written.bytes,
            "traces",
            "0",
            &traces_written,
            TRACES_DICTIONARY_COLUMNS,
            &[traces::TRACE_ID, traces::SESSION_ID, traces::USER_ID],
            &[traces::START_TIME, traces::TRACE_ID],
        );

        // Scores round-trip + streamed BatchFileWriter with Const & Selection::Mask.
        let sc1 = score_row(1, 0, 20, 21, 22, false, "a");
        let sc2 = score_row(2, 0, 10, 11, 12, false, "b");
        let scores_col = score_rows(&[sc1, sc2]);
        let scores_written =
            write_file_batch(FileTable::Scores, 1, &[Arc::new(scores_col.clone())]).unwrap();
        assert_file_settings(
            &scores_written.bytes,
            "scores",
            "1",
            &scores_written,
            SCORES_DICTIONARY_COLUMNS,
            &[scores::TRACE_ID, scores::SCORE_ID],
            &[scores::CREATED_AT],
        );
        let mut cols = scores_col.columns().to_vec();
        let p_idx = scores_col.schema().index_of(scores::PROJECT_ID).unwrap();
        cols[p_idx] = Column::try_new_const(ScalarValue::Utf8(project().to_string()), 2).unwrap();
        let masked = Batch::try_new(
            Arc::clone(scores_col.schema()),
            cols,
            2,
            Selection::Mask(Bitmap::from_bools(&[false, true])),
        )
        .unwrap();
        let mut writer = BatchFileWriter::new(FileTable::Scores, 1, Vec::new(), 1).unwrap();
        writer.write(&masked).unwrap();
        assert_eq!(writer.rows, 1);
        let (buf, summary) = writer.finish().unwrap();
        assert_eq!(summary.rows, 1);
        assert_eq!(summary.min_time, 10);
        assert_eq!(summary.max_time, 10);
        assert_eq!(summary.min_ingest_time, 12);
        assert_eq!(summary.max_ingest_time, 12);
        assert_eq!(read_arrow(&Bytes::from(buf)).num_rows(), 1);
    }

    #[test]
    fn many_small_rows_split_row_groups_and_both_writers_match() {
        let mut spans_vec = Vec::with_capacity(1_000);
        for i in 0..1_000_i64 {
            let mut s = span_row(1, 0, i, i);
            s.span_id = cetra_core::ids::SpanId::from_bytes((i + 1).to_be_bytes()).unwrap();
            spans_vec.push(s);
        }
        let chunk = Arc::new(span_rows(&spans_vec));
        let batches = vec![Arc::clone(&chunk); 250];
        let written = write_file_batch(FileTable::Spans, 0, &batches).unwrap();
        assert_eq!(written.rows, 250_000);
        assert_eq!(metadata(&written.bytes).num_row_groups(), 3);

        // Streamed BatchFileWriter produces identical output to write_file_batch for sorted inputs.
        let small: Vec<Arc<Batch>> = (0..3_i64)
            .map(|k| {
                let rows: Vec<_> = (0..1_000_i64)
                    .map(|i| {
                        let ts = k * 1_000 + i;
                        let mut s = span_row(1, 0, ts, ts);
                        s.span_id =
                            cetra_core::ids::SpanId::from_bytes((ts + 1).to_be_bytes()).unwrap();
                        s
                    })
                    .collect();
                Arc::new(span_rows(&rows))
            })
            .collect();
        let small_written = write_file_batch(FileTable::Spans, 0, &small).unwrap();
        let sink = tempfile::tempfile().unwrap();
        let mut writer = BatchFileWriter::new(FileTable::Spans, 0, sink, 3_000).unwrap();
        for b in &small {
            writer.write(b).unwrap();
        }
        assert_eq!(writer.rows, 3_000);
        let (mut sink, summary) = writer.finish().unwrap();
        sink.seek(SeekFrom::Start(0)).unwrap();
        let mut buffer = Vec::new();
        sink.read_to_end(&mut buffer).unwrap();
        let streamed = Bytes::from(buffer);
        assert_eq!(read_arrow(&streamed), read_arrow(&small_written.bytes));
        assert_eq!(summary.rows, small_written.rows);
        assert_eq!(summary.min_time, small_written.min_time);
        assert_eq!(summary.max_time, small_written.max_time);
        assert_eq!(summary.min_ingest_time, small_written.min_ingest_time);
        assert_eq!(summary.max_ingest_time, small_written.max_ingest_time);

        let kv_map = |bytes: &Bytes| {
            metadata(bytes)
                .file_metadata()
                .key_value_metadata()
                .unwrap()
                .iter()
                .map(|pair| (pair.key.clone(), pair.value.clone().unwrap()))
                .collect::<BTreeMap<String, String>>()
        };
        assert_eq!(kv_map(&small_written.bytes), kv_map(&streamed));
    }

    #[test]
    fn empty_and_foreign_batches_are_rejected() {
        let writer = BatchFileWriter::new(FileTable::Spans, 0, Vec::new(), 0).unwrap();
        let error = writer.finish().unwrap_err();
        assert!(matches!(
            error,
            ParquetWriteError::Invalid {
                reason: "empty file"
            }
        ));

        let scores = score_rows(&[score_row(1, 0, 1, 1, 1, false, "accuracy")]);
        let mut writer = BatchFileWriter::new(FileTable::Spans, 0, Vec::new(), 1).unwrap();
        assert!(matches!(
            writer.write(&scores).unwrap_err(),
            ParquetWriteError::Invalid { .. }
        ));
        assert!(matches!(
            BatchFileWriter::new(FileTable::Spans, 2, Vec::new(), 1),
            Err(ParquetWriteError::Invalid { .. })
        ));

        let spans = span_rows(&[span_row(1, 0, 1, 1)]);
        let empty = spans.slice(0, 0).unwrap();
        assert!(matches!(
            write_file_batch(FileTable::Spans, 0, &[Arc::new(empty)]).unwrap_err(),
            ParquetWriteError::Invalid { .. }
        ));
        assert!(matches!(
            write_file_batch(FileTable::Spans, 0, &[Arc::new(scores)]).unwrap_err(),
            ParquetWriteError::Invalid { .. }
        ));
        assert!(matches!(
            write_file_batch(FileTable::Spans, 2, &[Arc::new(spans)]).unwrap_err(),
            ParquetWriteError::Invalid { .. }
        ));
    }

    #[tokio::test]
    async fn verify_parquet_file_reports_all_verdicts_and_mid_page_corruption() {
        let col = Arc::new(span_rows(&[
            span_row(1, 1, 10, 100),
            span_row(2, 2, 20, 200),
            span_row(3, 3, 30, 300),
        ]));
        let written = write_file_batch(FileTable::Spans, 0, &[col]).unwrap();
        let path_str = "tables/spans/test.parquet";
        let meta = file_meta(FileTable::Spans, path_str, &written);

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from(path_str),
                PutPayload::from(written.bytes.clone()),
            )
            .await
            .unwrap();
        assert_eq!(verify_parquet_file(&store, &meta).await, Verdict::Healthy);

        // 1. Missing object.
        let mut missing_meta = meta.clone();
        missing_meta.path = "tables/spans/missing.parquet".to_owned();
        assert_eq!(
            verify_parquet_file(&store, &missing_meta).await,
            Verdict::Corrupt {
                reason: "missing object"
            }
        );

        // 2. Size differs from the catalog.
        let mut wrong_size = meta.clone();
        wrong_size.size += 1;
        assert_eq!(
            verify_parquet_file(&store, &wrong_size).await,
            Verdict::Corrupt {
                reason: "size differs from the catalog"
            }
        );

        // 3. Row count differs from the catalog.
        let mut wrong_rows = meta.clone();
        wrong_rows.rows += 1;
        assert_eq!(
            verify_parquet_file(&store, &wrong_rows).await,
            Verdict::Corrupt {
                reason: "row count differs from the catalog"
            }
        );

        // 4. Mid-page corruption -> decode failed.
        let mut reader =
            ParquetMetaDataReader::new().with_page_index_policy(PageIndexPolicy::Required);
        reader.try_parse(&written.bytes).unwrap();
        let pm = reader.finish().unwrap();
        let col0 = pm.row_group(0).column(0);
        let (start, len) = col0.byte_range();
        let mid = usize::try_from(start + len / 2).unwrap();
        let mut corrupted = written.bytes.to_vec();
        corrupted[mid] ^= 0xFF;
        let corrupt_path = "tables/spans/corrupt-mid.parquet";
        store
            .put(&ObjectPath::from(corrupt_path), PutPayload::from(corrupted))
            .await
            .unwrap();
        let mut corrupt_meta = meta.clone();
        corrupt_meta.path = corrupt_path.to_owned();
        assert_eq!(
            verify_parquet_file(&store, &corrupt_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );

        // 5. Corrupted page index -> decode failed (PageIndexPolicy::Required).
        let idx_offset = pm.row_group(0).column(0).column_index_offset().unwrap();
        let idx_length = pm.row_group(0).column(0).column_index_length().unwrap();
        let mut corrupt_idx_bytes = written.bytes.to_vec();
        let idx_start = usize::try_from(idx_offset).unwrap();
        let idx_end = idx_start + usize::try_from(idx_length).unwrap();
        for b in &mut corrupt_idx_bytes[idx_start..idx_end] {
            *b ^= 0xFF;
        }
        let corrupt_idx_path = "tables/spans/corrupt-page-index.parquet";
        store
            .put(
                &ObjectPath::from(corrupt_idx_path),
                PutPayload::from(corrupt_idx_bytes),
            )
            .await
            .unwrap();
        let corrupt_idx_meta = FileMeta {
            path: corrupt_idx_path.to_owned(),
            ..meta.clone()
        };
        assert_eq!(
            verify_parquet_file(&store, &corrupt_idx_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );

        // 6. Fault::Truncated -> Verdict::Unavailable.
        let chaos = Arc::new(ChaosStore::new(Arc::clone(&store)));
        for _ in 0..16 {
            chaos.push(Call::Get, Fault::Truncated);
        }
        let chaos_store: Arc<dyn ObjectStore> = chaos;
        assert_eq!(
            verify_parquet_file(&chaos_store, &meta).await,
            Verdict::Unavailable
        );
    }

    #[tokio::test]
    async fn read_time_column_sorts_and_distinguishes_store_vs_corrupt() {
        let f1 = write_file_batch(
            FileTable::Spans,
            0,
            &[Arc::new(span_rows(&[span_row(1, 1, 30, 100)]))],
        )
        .unwrap();
        let f2 = write_file_batch(
            FileTable::Spans,
            0,
            &[Arc::new(span_rows(&[
                span_row(2, 1, 10, 100),
                span_row(3, 1, 20, 100),
            ]))],
        )
        .unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let m1 = file_meta(FileTable::Spans, "tables/spans/f1.parquet", &f1);
        let m2 = file_meta(FileTable::Spans, "tables/spans/f2.parquet", &f2);
        store
            .put(
                &ObjectPath::from(m1.path.as_str()),
                PutPayload::from(f1.bytes),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjectPath::from(m2.path.as_str()),
                PutPayload::from(f2.bytes),
            )
            .await
            .unwrap();

        let files = [Arc::new(m1.clone()), Arc::new(m2)];
        let times = read_time_column(&store, &files, spans::START_TIME)
            .await
            .unwrap();
        assert_eq!(times, vec![10, 20, 30]);

        // Store error -> may_be_corrupt == false.
        let chaos = Arc::new(ChaosStore::new(Arc::clone(&store)));
        chaos.hold(&[Call::Get]);
        let chaos_store: Arc<dyn ObjectStore> = chaos.clone();
        let err = read_time_column(&chaos_store, &[Arc::new(m1.clone())], spans::START_TIME)
            .await
            .unwrap_err();
        match err {
            CompactionError::Read { ref source } => assert!(!may_be_corrupt(source)),
            other => panic!("expected Read error, got {other:?}"),
        }

        // Corrupt bytes (same file size) -> may_be_corrupt == true.
        let corrupt_bytes = vec![b'X'; usize::try_from(m1.size).unwrap()];
        store
            .put(
                &ObjectPath::from(m1.path.as_str()),
                PutPayload::from(corrupt_bytes),
            )
            .await
            .unwrap();
        let err = read_time_column(&store, &[Arc::new(m1)], spans::START_TIME)
            .await
            .unwrap_err();
        match err {
            CompactionError::Read { ref source } => assert!(may_be_corrupt(source)),
            other => panic!("expected Read error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_trace_anchors_prunes_row_groups_and_preserves_store_error() {
        let b1 = trace_batch(&[span_row(1, 1, 100, 10)]);
        let b2 = trace_batch(&[span_row(2, 2, 200, 500)]);
        let schema = arrow_traces_schema();
        let rb1 = batch_to_record_batch(&b1, &schema).unwrap();
        let rb2 = batch_to_record_batch(&b2, &schema).unwrap();
        let mut buf = Vec::new();
        let props = WriterProperties::builder()
            .set_statistics_enabled(EnabledStatistics::Page)
            .build();
        let mut aw = ArrowWriter::try_new(&mut buf, schema, Some(props)).unwrap();
        aw.write(&rb1).unwrap();
        aw.flush().unwrap();
        aw.write(&rb2).unwrap();
        aw.close().unwrap();
        let bytes = Bytes::from(buf);
        let meta = FileMeta {
            id: FileId::new_v7(),
            table: FileTable::Traces,
            project: project(),
            day: day(),
            level: 0,
            path: "tables/traces/rg.parquet".to_owned(),
            size: bytes.len() as u64,
            footer_size: 8,
            rows: 2,
            min_time: 100,
            max_time: 200,
            min_ingest_time: 10,
            max_ingest_time: 500,
            fully_merged: false,
            deletions_applied_seq: 0,
        };

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        store
            .put(
                &ObjectPath::from(meta.path.as_str()),
                PutPayload::from(bytes),
            )
            .await
            .unwrap();

        let all = read_trace_anchors(&store, &meta, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        let pruned = read_trace_anchors(&store, &meta, 100).await.unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(
            pruned[0]
                .column_by_name(traces::LAST_INGEST_TIME)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::TimestampNs {
                value: 500,
                utc: true
            }
        );
        assert!(
            read_trace_anchors(&store, &meta, 600)
                .await
                .unwrap()
                .is_empty()
        );

        // Store error -> AnchorError::is_store_error() == true.
        let chaos = Arc::new(ChaosStore::new(Arc::clone(&store)));
        chaos.hold(&[Call::Get]);
        let chaos_store: Arc<dyn ObjectStore> = chaos;
        let err = read_trace_anchors(&chaos_store, &meta, 0)
            .await
            .unwrap_err();
        assert!(err.is_store_error(), "expected store error, got {err:?}");
    }

    fn rewrite_parquet_metadata(
        base_bytes: &Bytes,
        mutate: impl FnOnce(ParquetMetaData) -> ParquetMetaData,
    ) -> (Bytes, u64) {
        let tail: &[u8; 8] = base_bytes[base_bytes.len() - 8..].try_into().unwrap();
        let old_footer_size =
            usize::try_from(footer_size_from_tail(tail, base_bytes.len() as u64).unwrap()).unwrap();
        let footer_start = base_bytes.len() - old_footer_size;

        let mut reader = ParquetMetaDataReader::new().with_page_index_policy(PageIndexPolicy::Skip);
        reader.try_parse(base_bytes).unwrap();
        let base_meta = reader.finish().unwrap();
        let new_meta = mutate(base_meta);

        let mut out = base_bytes[..footer_start].to_vec();
        parquet::file::metadata::ParquetMetaDataWriter::new(&mut out, &new_meta)
            .finish()
            .unwrap();
        let out_bytes = Bytes::from(out);
        let new_tail: &[u8; 8] = out_bytes[out_bytes.len() - 8..].try_into().unwrap();
        let new_footer_size = footer_size_from_tail(new_tail, out_bytes.len() as u64).unwrap();
        (out_bytes, new_footer_size)
    }

    fn meta_with_bytes(
        table: FileTable,
        path: &str,
        bytes: &Bytes,
        footer_size: u64,
        rows: u64,
    ) -> FileMeta {
        FileMeta {
            id: FileId::new_v7(),
            table,
            project: project(),
            day: day(),
            level: 0,
            path: path.to_owned(),
            size: u64::try_from(bytes.len()).unwrap(),
            footer_size,
            rows,
            min_time: 10,
            max_time: 100,
            min_ingest_time: 10,
            max_ingest_time: 500,
            fully_merged: false,
            deletions_applied_seq: 0,
        }
    }

    #[tokio::test]
    async fn negative_offsets_and_truncated_rg_columns_return_corrupt_without_panicking() {
        let traces_col = Arc::new(trace_batch(&[span_row(1, 1, 100, 10)]));
        let written = write_file_batch(FileTable::Traces, 0, &[traces_col]).unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // 1. Negative data_page_offset on start_time column -> Corrupt (no panic).
        let (neg_data_bytes, neg_data_footer) = rewrite_parquet_metadata(&written.bytes, |pm| {
            let mut rgs = pm.row_groups().to_vec();
            let rg = rgs.remove(0);
            let mut cols = rg.columns().to_vec();
            let start_idx = cols
                .iter()
                .position(|c| c.column_descr().path().parts() == [traces::START_TIME])
                .unwrap();
            cols[start_idx] = cols[start_idx]
                .clone()
                .into_builder()
                .set_data_page_offset(-1)
                .build()
                .unwrap();
            let new_rg = rg.into_builder().set_column_metadata(cols).build().unwrap();
            pm.into_builder().set_row_groups(vec![new_rg]).build()
        });
        let neg_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/neg-data.parquet",
            &neg_data_bytes,
            neg_data_footer,
            written.rows,
        );
        store
            .put(
                &ObjectPath::from(neg_meta.path.as_str()),
                PutPayload::from(neg_data_bytes),
            )
            .await
            .unwrap();
        assert_eq!(
            verify_parquet_file(&store, &neg_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );
        match read_time_column(&store, &[Arc::new(neg_meta.clone())], traces::START_TIME)
            .await
            .unwrap_err()
        {
            CompactionError::Read { ref source } => assert!(may_be_corrupt(source)),
            other => panic!("expected Read error, got {other:?}"),
        }
        let err = read_trace_anchors(&store, &neg_meta, 0).await.unwrap_err();
        assert!(!err.is_store_error(), "expected corrupt error, got {err:?}");

        // 2. Negative compressed_size -> Corrupt (no panic).
        let (neg_size_bytes, neg_size_footer) = rewrite_parquet_metadata(&written.bytes, |pm| {
            let mut rgs = pm.row_groups().to_vec();
            let rg = rgs.remove(0);
            let mut cols = rg.columns().to_vec();
            let start_idx = cols
                .iter()
                .position(|c| c.column_descr().path().parts() == [traces::START_TIME])
                .unwrap();
            cols[start_idx] = cols[start_idx]
                .clone()
                .into_builder()
                .set_total_compressed_size(-16)
                .build()
                .unwrap();
            let new_rg = rg.into_builder().set_column_metadata(cols).build().unwrap();
            pm.into_builder().set_row_groups(vec![new_rg]).build()
        });
        let neg_size_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/neg-size.parquet",
            &neg_size_bytes,
            neg_size_footer,
            written.rows,
        );
        store
            .put(
                &ObjectPath::from(neg_size_meta.path.as_str()),
                PutPayload::from(neg_size_bytes),
            )
            .await
            .unwrap();
        assert_eq!(
            verify_parquet_file(&store, &neg_size_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );

        // 3. Truncated rg.columns (fewer columns than schema) -> Corrupt (no panic).
        let (trunc_cols_bytes, trunc_cols_footer) =
            rewrite_parquet_metadata(&written.bytes, |pm| {
                let small_schema = Arc::new(SchemaDescriptor::new(Arc::new(
                    parquet::schema::types::Type::group_type_builder("schema")
                        .build()
                        .unwrap(),
                )));
                let new_rg = RowGroupMetaData::builder(small_schema)
                    .set_num_rows(1)
                    .set_column_metadata(Vec::new())
                    .build()
                    .unwrap();
                pm.into_builder().set_row_groups(vec![new_rg]).build()
            });
        let trunc_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/trunc-cols.parquet",
            &trunc_cols_bytes,
            trunc_cols_footer,
            written.rows,
        );
        store
            .put(
                &ObjectPath::from(trunc_meta.path.as_str()),
                PutPayload::from(trunc_cols_bytes),
            )
            .await
            .unwrap();
        assert_eq!(
            verify_parquet_file(&store, &trunc_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );
        match read_time_column(&store, &[Arc::new(trunc_meta.clone())], traces::START_TIME)
            .await
            .unwrap_err()
        {
            CompactionError::Read { ref source } => assert!(may_be_corrupt(source)),
            other => panic!("expected Read error, got {other:?}"),
        }
        let err = read_trace_anchors(&store, &trunc_meta, 0)
            .await
            .unwrap_err();
        assert!(!err.is_store_error(), "expected corrupt error, got {err:?}");
    }

    #[tokio::test]
    async fn zero_length_byte_range_and_rle_bomb_are_corrupt_not_unavailable() {
        let traces_col = Arc::new(trace_batch(&[span_row(1, 1, 100, 10)]));
        let written = write_file_batch(FileTable::Traces, 0, &[traces_col]).unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // 1. Non-empty row group with zero-length column chunk -> Corrupt (NOT Unavailable).
        let (zero_len_bytes, zero_len_footer) = rewrite_parquet_metadata(&written.bytes, |pm| {
            let mut rgs = pm.row_groups().to_vec();
            let rg = rgs.remove(0);
            let mut cols = rg.columns().to_vec();
            let start_idx = cols
                .iter()
                .position(|c| c.column_descr().path().parts() == [traces::START_TIME])
                .unwrap();
            cols[start_idx] = cols[start_idx]
                .clone()
                .into_builder()
                .set_dictionary_page_offset(None)
                .set_total_compressed_size(0)
                .build()
                .unwrap();
            let new_rg = rg.into_builder().set_column_metadata(cols).build().unwrap();
            pm.into_builder().set_row_groups(vec![new_rg]).build()
        });
        let zero_len_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/zero-len.parquet",
            &zero_len_bytes,
            zero_len_footer,
            written.rows,
        );
        store
            .put(
                &ObjectPath::from(zero_len_meta.path.as_str()),
                PutPayload::from(zero_len_bytes),
            )
            .await
            .unwrap();
        assert_eq!(
            verify_parquet_file(&store, &zero_len_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );
        match read_time_column(
            &store,
            &[Arc::new(zero_len_meta.clone())],
            traces::START_TIME,
        )
        .await
        .unwrap_err()
        {
            CompactionError::Read { ref source } => assert!(may_be_corrupt(source)),
            other => panic!("expected Read error, got {other:?}"),
        }
        let err = read_trace_anchors(&store, &zero_len_meta, 0)
            .await
            .unwrap_err();
        assert!(!err.is_store_error(), "expected corrupt error, got {err:?}");

        // 2. Zero-row row group with zero-length column chunks -> skips empty fetches cleanly.
        let (zero_rg_bytes, zero_rg_footer) = rewrite_parquet_metadata(&written.bytes, |pm| {
            let mut rgs = pm.row_groups().to_vec();
            let rg = rgs.remove(0);
            let cols = rg
                .columns()
                .iter()
                .map(|c| {
                    c.clone()
                        .into_builder()
                        .set_dictionary_page_offset(None)
                        .set_total_compressed_size(0)
                        .build()
                        .unwrap()
                })
                .collect();
            let new_rg = rg
                .into_builder()
                .set_num_rows(0)
                .set_column_metadata(cols)
                .build()
                .unwrap();
            pm.into_builder().set_row_groups(vec![new_rg]).build()
        });
        let zero_rg_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/zero-rg.parquet",
            &zero_rg_bytes,
            zero_rg_footer,
            0,
        );
        store
            .put(
                &ObjectPath::from(zero_rg_meta.path.as_str()),
                PutPayload::from(zero_rg_bytes),
            )
            .await
            .unwrap();
        assert_eq!(
            verify_parquet_file(&store, &zero_rg_meta).await,
            Verdict::Healthy
        );
        assert!(
            read_time_column(
                &store,
                &[Arc::new(zero_rg_meta.clone())],
                traces::START_TIME
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(
            read_trace_anchors(&store, &zero_rg_meta, 0)
                .await
                .unwrap()
                .is_empty()
        );

        // 3. RLE-bomb / absurd rg.num_rows() -> rejected immediately without OOM.
        let (bomb_bytes, bomb_footer) = rewrite_parquet_metadata(&written.bytes, |pm| {
            let mut rgs = pm.row_groups().to_vec();
            let rg = rgs.remove(0);
            let new_rg = rg
                .into_builder()
                .set_num_rows(i64::from(u32::MAX))
                .build()
                .unwrap();
            pm.into_builder().set_row_groups(vec![new_rg]).build()
        });
        let bomb_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/bomb.parquet",
            &bomb_bytes,
            bomb_footer,
            u64::from(u32::MAX),
        );
        store
            .put(
                &ObjectPath::from(bomb_meta.path.as_str()),
                PutPayload::from(bomb_bytes),
            )
            .await
            .unwrap();
        assert_eq!(
            verify_parquet_file(&store, &bomb_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );
        match read_time_column(&store, &[Arc::new(bomb_meta.clone())], traces::START_TIME)
            .await
            .unwrap_err()
        {
            CompactionError::Read { ref source } => assert!(may_be_corrupt(source)),
            other => panic!("expected Read error, got {other:?}"),
        }
        let err = read_trace_anchors(&store, &bomb_meta, 0).await.unwrap_err();
        assert!(!err.is_store_error(), "expected corrupt error, got {err:?}");
    }

    #[tokio::test]
    async fn readers_reject_non_nanos_timestamp_and_decode_multi_chunk_row_groups() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        // 1. Non-nanosecond timestamp logical type is rejected by both readers.
        let bad_schema = Arc::new(arrow::datatypes::Schema::new(vec![
            arrow::datatypes::Field::new(
                traces::TRACE_ID,
                arrow::datatypes::DataType::FixedSizeBinary(16),
                false,
            ),
            arrow::datatypes::Field::new(
                traces::START_TIME,
                arrow::datatypes::DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Millisecond,
                    Some("UTC".into()),
                ),
                false,
            ),
            arrow::datatypes::Field::new(
                traces::END_TIME,
                arrow::datatypes::DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Nanosecond,
                    Some("UTC".into()),
                ),
                true,
            ),
            arrow::datatypes::Field::new(
                traces::LAST_INGEST_TIME,
                arrow::datatypes::DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Nanosecond,
                    Some("UTC".into()),
                ),
                false,
            ),
        ]));
        let bad_rb = RecordBatch::try_new(
            Arc::clone(&bad_schema),
            vec![
                Arc::new(
                    arrow::array::FixedSizeBinaryArray::try_from_iter(std::iter::once(&[1u8; 16]))
                        .unwrap(),
                ),
                Arc::new(
                    arrow::array::TimestampMillisecondArray::from(vec![1_000]).with_timezone("UTC"),
                ),
                Arc::new(
                    arrow::array::TimestampNanosecondArray::from(vec![Some(2_000)])
                        .with_timezone("UTC"),
                ),
                Arc::new(
                    arrow::array::TimestampNanosecondArray::from(vec![3_000]).with_timezone("UTC"),
                ),
            ],
        )
        .unwrap();
        let mut bad_buf = Vec::new();
        let mut aw = ArrowWriter::try_new(&mut bad_buf, bad_schema, None).unwrap();
        aw.write(&bad_rb).unwrap();
        aw.close().unwrap();
        let bad_bytes = Bytes::from(bad_buf);
        let bad_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/bad-ts.parquet",
            &bad_bytes,
            8,
            1,
        );
        store
            .put(
                &ObjectPath::from(bad_meta.path.as_str()),
                PutPayload::from(bad_bytes),
            )
            .await
            .unwrap();
        assert!(matches!(
            read_time_column(&store, &[Arc::new(bad_meta.clone())], traces::START_TIME)
                .await
                .unwrap_err(),
            CompactionError::Input {
                reason: "missing time column"
            }
        ));
        assert!(matches!(
            read_trace_anchors(&store, &bad_meta, 0).await.unwrap_err(),
            AnchorError::Column {
                name: traces::START_TIME
            }
        ));

        // 2. Multi-chunk (> READ_BATCH_ROWS) single row group decodes in bounded batches.
        let total = READ_BATCH_ROWS + 808;
        let spans_vec: Vec<_> = (0..total)
            .map(|i| {
                span_row(
                    u16::try_from(i + 1).unwrap(),
                    1,
                    1_000 + i64::try_from(i).unwrap(),
                    2_000 + i64::try_from(i).unwrap(),
                )
            })
            .collect();
        let col_batch = Arc::new(trace_batch(&spans_vec));
        let written = write_file_batch(FileTable::Traces, 0, &[col_batch]).unwrap();
        let meta = file_meta(FileTable::Traces, "tables/traces/chunked.parquet", &written);
        store
            .put(
                &ObjectPath::from(meta.path.as_str()),
                PutPayload::from(written.bytes),
            )
            .await
            .unwrap();

        let times = read_time_column(&store, &[Arc::new(meta.clone())], traces::START_TIME)
            .await
            .unwrap();
        assert_eq!(times.len(), total);
        assert_eq!(times[0], 1_000);
        assert_eq!(times[total - 1], 1_000 + i64::try_from(total - 1).unwrap());

        let anchor_batches = read_trace_anchors(&store, &meta, 0).await.unwrap();
        assert_eq!(anchor_batches.len(), 2);
        assert_eq!(anchor_batches[0].num_rows(), READ_BATCH_ROWS);
        assert_eq!(anchor_batches[1].num_rows(), total - READ_BATCH_ROWS);
        let decoded_total: usize = anchor_batches.iter().map(Batch::num_rows).sum();
        assert_eq!(decoded_total, total);

        // 3. Multi-chunk (> READ_BATCH_ROWS) with nullable end_time decodes nulls properly.
        let nullable_schema = Arc::new(arrow::datatypes::Schema::new(vec![
            arrow::datatypes::Field::new(
                traces::TRACE_ID,
                arrow::datatypes::DataType::FixedSizeBinary(16),
                false,
            ),
            arrow::datatypes::Field::new(
                traces::START_TIME,
                arrow::datatypes::DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Nanosecond,
                    Some("UTC".into()),
                ),
                false,
            ),
            arrow::datatypes::Field::new(
                traces::END_TIME,
                arrow::datatypes::DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Nanosecond,
                    Some("UTC".into()),
                ),
                true,
            ),
            arrow::datatypes::Field::new(
                traces::LAST_INGEST_TIME,
                arrow::datatypes::DataType::Timestamp(
                    arrow::datatypes::TimeUnit::Nanosecond,
                    Some("UTC".into()),
                ),
                false,
            ),
        ]));
        let trace_ids: Vec<[u8; 16]> = (0..total)
            .map(|i| u128::try_from(i + 1).unwrap().to_be_bytes())
            .collect();
        let starts: Vec<i64> = (0..total)
            .map(|i| 1_000 + i64::try_from(i).unwrap())
            .collect();
        let ends: Vec<Option<i64>> = (0..total)
            .map(|i| (i % 3 != 0).then_some(1_100 + i64::try_from(i).unwrap()))
            .collect();
        let ingests: Vec<i64> = (0..total)
            .map(|i| 2_000 + i64::try_from(i).unwrap())
            .collect();
        let nullable_rb = RecordBatch::try_new(
            Arc::clone(&nullable_schema),
            vec![
                Arc::new(
                    arrow::array::FixedSizeBinaryArray::try_from_iter(
                        trace_ids.iter().map(|b| b.as_slice()),
                    )
                    .unwrap(),
                ),
                Arc::new(arrow::array::TimestampNanosecondArray::from(starts).with_timezone("UTC")),
                Arc::new(arrow::array::TimestampNanosecondArray::from(ends).with_timezone("UTC")),
                Arc::new(
                    arrow::array::TimestampNanosecondArray::from(ingests).with_timezone("UTC"),
                ),
            ],
        )
        .unwrap();
        let mut nullable_buf = Vec::new();
        let mut aw = ArrowWriter::try_new(&mut nullable_buf, nullable_schema, None).unwrap();
        aw.write(&nullable_rb).unwrap();
        aw.close().unwrap();
        let nullable_bytes = Bytes::from(nullable_buf);
        let nullable_meta = meta_with_bytes(
            FileTable::Traces,
            "tables/traces/chunked-nullable.parquet",
            &nullable_bytes,
            8,
            u64::try_from(total).unwrap(),
        );
        store
            .put(
                &ObjectPath::from(nullable_meta.path.as_str()),
                PutPayload::from(nullable_bytes),
            )
            .await
            .unwrap();
        let nullable_batches = read_trace_anchors(&store, &nullable_meta, 0).await.unwrap();
        assert_eq!(nullable_batches.len(), 2);
        assert_eq!(nullable_batches[0].num_rows(), READ_BATCH_ROWS);
        assert_eq!(nullable_batches[1].num_rows(), total - READ_BATCH_ROWS);
        assert!(
            nullable_batches[0]
                .column_by_name(traces::END_TIME)
                .unwrap()
                .is_null(0)
        );
        assert!(
            !nullable_batches[0]
                .column_by_name(traces::END_TIME)
                .unwrap()
                .is_null(1)
        );
        assert!(
            nullable_batches[1]
                .column_by_name(traces::END_TIME)
                .unwrap()
                .is_null(1)
        );
        assert!(
            !nullable_batches[1]
                .column_by_name(traces::END_TIME)
                .unwrap()
                .is_null(2)
        );
    }
}
