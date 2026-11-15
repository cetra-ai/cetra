//! Encoding for SQL console rows (D98).
//!
//! Binary is lowercase hex. Maps are objects. Lists are arrays. NaN and ±inf
//! are null. Timestamps with a time zone are UTC with a `Z` suffix; naive
//! timestamps have no suffix. Both use 9 fractional digits.
//!
//! CSV writes those JSON values as RFC 4180 fields. NDJSON writes one object
//! per line. Parquet writes the Arrow batches with ZSTD level 1.

use std::io::{self, Write};
use std::sync::Arc;

use arrow::array::{
    Array, AsArray, FixedSizeListArray, GenericListArray, MapArray, OffsetSizeTrait, RunArray,
};
use arrow::compute::cast;
use arrow::datatypes::{
    DataType, Field, Int16Type, Int32Type, Int64Type, IntervalUnit, RunEndIndexType, Schema,
    TimeUnit,
};
use arrow::error::ArrowError;
use arrow::json::{ArrayWriter, LineDelimitedWriter, WriterBuilder};
use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use datafusion::error::DataFusionError;
use datafusion::execution::memory_pool::{MemoryConsumer, MemoryPool, MemoryReservation};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde_json::{Map, Value};

use crate::query_error::RunError;

/// Flush a parquet row group once its encoded size passes this.
const PARQUET_CHUNK_BYTES: usize = 32 * 1024 * 1024;

/// Rows per Parquet row group in exports (parquet's default).
const PARQUET_ROW_GROUP_ROWS: usize = 1024 * 1024;

/// The output limit of one encoding pass.
///
/// Output past `max_bytes` is refused, and so is a reservation that `pool`
/// refuses. Both surface as [`RunError::TooLarge`].
pub(crate) struct Budget {
    /// Largest output, in bytes.
    pub(crate) max_bytes: usize,
    /// Pool the output bytes are reserved from while they are built.
    pub(crate) pool: Arc<dyn MemoryPool>,
}

/// JSON bytes of a `null`.
const NULL_BYTES: u64 = 4;
/// Bound on the JSON text of a number, bool or temporal value, quotes included.
/// The longest is an ISO 8601 duration, about 36 bytes.
const VALUE_BYTES: u64 = 40;
/// Bound on the quoted text of a day-time interval. Its day count, hour count,
/// minutes and seconds are each at most ten characters with their sign.
const DAY_TIME_INTERVAL_BYTES: u64 = 64;
/// Bound on the quoted text of a month-day-nano interval. It writes an i32 month
/// count, an i32 day count and an i64 nanosecond count, each in its display form.
const MONTH_DAY_NANO_INTERVAL_BYTES: u64 = 96;
/// The two quotes around a string.
const QUOTES: u64 = 2;
/// The brackets of an array or an object.
const BRACKETS: u64 = 2;
/// The quoted name of a member, its colon and its comma.
const MEMBER_BYTES: u64 = 4;
/// The comma after an element of an array.
const ELEMENT_BYTES: u64 = 1;

/// Estimate of the JSON bytes each row of `batch` encodes to: the value of
/// every cell, the column name that keys it, the punctuation around both, and
/// the row's braces. Each row is sized on its own, so a caller can cut a batch
/// at any row, and a nested value counts only the children its rows reach.
///
/// The estimate counts a string's bytes, not the escapes JSON writes for it, so
/// it can be lower than the output for control characters and quotes. The
/// encoder refuses output past the budget either way.
///
/// # Errors
///
/// Returns [`RunError::Internal`] when a column cannot be sized. The message
/// carries no values.
pub(crate) fn batch_row_sizes(batch: &RecordBatch) -> Result<Vec<u64>, RunError> {
    let schema = batch.schema();
    let mut totals = vec![BRACKETS; batch.num_rows()];
    let fields = schema.fields().iter().map(|field| field.as_ref());
    let columns = batch.columns().iter().map(|column| column.as_ref());
    add_members(&mut totals, fields.zip(columns))?;
    Ok(totals)
}

/// Whether an estimate is past `limit` bytes.
pub(crate) fn exceeds(estimate: u64, limit: usize) -> bool {
    estimate > to_u64(limit)
}

/// Estimated JSON bytes of each row of `array` as a value: `null` for a null
/// row, and the sized value otherwise.
///
/// # Errors
///
/// Returns [`RunError::Internal`] when the layout is inconsistent. The message
/// carries no values.
pub(crate) fn array_row_sizes(array: &dyn Array) -> Result<Vec<u64>, RunError> {
    let mut sizes = value_sizes(array)?;
    if sizes.len() != array.len() {
        return Err(sizing_failure());
    }
    if let Some(nulls) = array.nulls() {
        for (index, size) in sizes.iter_mut().enumerate() {
            if nulls.is_null(index) {
                *size = NULL_BYTES;
            }
        }
    }
    Ok(sizes)
}

fn sizing_failure() -> RunError {
    RunError::Internal(DataFusionError::Internal(
        "failed to size a result column".into(),
    ))
}

fn to_u64(number: usize) -> u64 {
    u64::try_from(number).unwrap_or(u64::MAX)
}

/// Bound on the text of a decimal: a sign, its digits, and a point when the
/// scale is positive. A negative scale writes zeros after the digits. A scale
/// past the precision writes a leading zero before the point.
fn decimal_bytes(precision: u8, scale: i8) -> u64 {
    let precision = u64::from(precision);
    let magnitude = u64::from(scale.unsigned_abs());
    if scale <= 0 {
        return 1 + precision + magnitude;
    }
    precision.max(magnitude) + 3
}

/// Sizes of the values of `array` ignoring its nulls. The caller applies them.
fn value_sizes(array: &dyn Array) -> Result<Vec<u64>, RunError> {
    let len = array.len();
    Ok(match array.data_type() {
        DataType::Null => vec![NULL_BYTES; len],
        DataType::Boolean
        | DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64
        | DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Date32
        | DataType::Date64
        | DataType::Time32(_)
        | DataType::Time64(_)
        | DataType::Timestamp(..)
        | DataType::Duration(_)
        | DataType::Interval(IntervalUnit::YearMonth) => vec![VALUE_BYTES; len],
        DataType::Interval(IntervalUnit::DayTime) => vec![DAY_TIME_INTERVAL_BYTES; len],
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            vec![MONTH_DAY_NANO_INTERVAL_BYTES; len]
        }
        DataType::Decimal32(precision, scale)
        | DataType::Decimal64(precision, scale)
        | DataType::Decimal128(precision, scale)
        | DataType::Decimal256(precision, scale) => vec![decimal_bytes(*precision, *scale); len],
        DataType::Utf8 => offset_sizes(array.as_string::<i32>().value_offsets(), 1, QUOTES),
        DataType::LargeUtf8 => offset_sizes(array.as_string::<i64>().value_offsets(), 1, QUOTES),
        // Binary is lowercase hex: two characters a byte.
        DataType::Binary => offset_sizes(array.as_binary::<i32>().value_offsets(), 2, QUOTES),
        DataType::LargeBinary => offset_sizes(array.as_binary::<i64>().value_offsets(), 2, QUOTES),
        DataType::FixedSizeBinary(width) => {
            let width = u64::try_from(*width).unwrap_or(0);
            vec![width.saturating_mul(2).saturating_add(QUOTES); len]
        }
        // The low 32 bits of a view are its length (`ByteView::as_u128`). The
        // view holds the length, so the bytes are not read.
        DataType::Utf8View => array
            .as_string_view()
            .views()
            .iter()
            .map(|view| view_length(*view).saturating_add(QUOTES))
            .collect(),
        DataType::BinaryView => array
            .as_binary_view()
            .views()
            .iter()
            .map(|view| view_length(*view).saturating_mul(2).saturating_add(QUOTES))
            .collect(),
        DataType::List(_) => list_sizes(array.as_list::<i32>())?,
        DataType::LargeList(_) => list_sizes(array.as_list::<i64>())?,
        DataType::FixedSizeList(..) => {
            let list = array
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .ok_or_else(sizing_failure)?;
            fixed_list_sizes(list)?
        }
        DataType::Map(..) => map_sizes(array)?,
        DataType::Struct(_) => {
            let strukt = array.as_struct();
            let mut totals = vec![BRACKETS; len];
            let fields = strukt.fields().iter().map(|field| field.as_ref());
            let columns = strukt.columns().iter().map(|column| column.as_ref());
            add_members(&mut totals, fields.zip(columns))?;
            totals
        }
        DataType::Dictionary(..) => dictionary_sizes(array)?,
        DataType::RunEndEncoded(ends, _) => match ends.data_type() {
            DataType::Int16 => run_sizes::<Int16Type>(array)?,
            DataType::Int32 => run_sizes::<Int32Type>(array)?,
            DataType::Int64 => run_sizes::<Int64Type>(array)?,
            _ => memory_sizes(array),
        },
        _ => memory_sizes(array),
    })
}

/// Byte length of a string or binary view.
fn view_length(view: u128) -> u64 {
    u64::from(view as u32)
}

/// Each value's bytes times `scale`, plus `extra`, for every row of `offsets`.
fn offset_sizes<O: OffsetSizeTrait>(offsets: &[O], scale: u64, extra: u64) -> Vec<u64> {
    offsets
        .windows(2)
        .map(|pair| {
            let length = pair[1]
                .to_usize()
                .unwrap_or(0)
                .saturating_sub(pair[0].to_usize().unwrap_or(0));
            to_u64(length).saturating_mul(scale).saturating_add(extra)
        })
        .collect()
}

/// Adds every member's sizes to `totals`, with the member's name and punctuation.
fn add_members<'a>(
    totals: &mut [u64],
    members: impl IntoIterator<Item = (&'a Field, &'a dyn Array)>,
) -> Result<(), RunError> {
    for (field, column) in members {
        let member = to_u64(field.name().len()).saturating_add(MEMBER_BYTES);
        for (total, size) in totals.iter_mut().zip(array_row_sizes(column)?) {
            *total = total.saturating_add(size).saturating_add(member);
        }
    }
    Ok(())
}

/// Where a list's child values lie: the first child index its rows reach,
/// how many child values they span, and each row's `(lo, hi)` range relative
/// to that first index.
struct ChildWindow {
    start: usize,
    length: usize,
    ranges: Vec<(usize, usize)>,
}

/// The [`ChildWindow`] of a list whose offsets are `offsets`. A malformed
/// offset list is an error.
fn child_window<O: OffsetSizeTrait>(
    offsets: &[O],
    child_len: usize,
) -> Result<ChildWindow, RunError> {
    let (Some(first), Some(last)) = (offsets.first(), offsets.last()) else {
        return Ok(ChildWindow {
            start: 0,
            length: 0,
            ranges: Vec::new(),
        });
    };
    let first = first.to_usize().ok_or_else(sizing_failure)?;
    let last = last.to_usize().ok_or_else(sizing_failure)?;
    if first > last || last > child_len {
        return Err(sizing_failure());
    }
    let mut ranges = Vec::with_capacity(offsets.len() - 1);
    for pair in offsets.windows(2) {
        let lo = pair[0].to_usize().ok_or_else(sizing_failure)?;
        let hi = pair[1].to_usize().ok_or_else(sizing_failure)?;
        if lo < first || lo > hi || hi > last {
            return Err(sizing_failure());
        }
        ranges.push((lo - first, hi - first));
    }
    Ok(ChildWindow {
        start: first,
        length: last - first,
        ranges,
    })
}

/// `prefix[k]` is the sum of the first `k` sizes, saturating.
fn prefix_sums(sizes: &[u64]) -> Vec<u64> {
    let mut prefix = Vec::with_capacity(sizes.len() + 1);
    let mut total = 0u64;
    prefix.push(total);
    for size in sizes {
        total = total.saturating_add(*size);
        prefix.push(total);
    }
    prefix
}

/// The sum over each `(lo, hi)` range, read from `prefix`.
fn range_sums(prefix: &[u64], ranges: &[(usize, usize)]) -> Result<Vec<u64>, RunError> {
    ranges
        .iter()
        .map(|&(lo, hi)| {
            let (Some(low), Some(high)) = (prefix.get(lo), prefix.get(hi)) else {
                return Err(sizing_failure());
            };
            Ok(high.saturating_sub(*low))
        })
        .collect()
}

/// A list row is its brackets, its elements and one comma per element. Only
/// the child values the rows reach are sized.
fn list_sizes<O: OffsetSizeTrait>(list: &GenericListArray<O>) -> Result<Vec<u64>, RunError> {
    let window = child_window(list.value_offsets(), list.values().len())?;
    let child = list.values().slice(window.start, window.length);
    let prefix = prefix_sums(&array_row_sizes(child.as_ref())?);
    let sums = range_sums(&prefix, &window.ranges)?;
    Ok(sums
        .into_iter()
        .zip(window.ranges.iter())
        .map(|(sum, &(lo, hi))| {
            BRACKETS
                .saturating_add(sum)
                .saturating_add(ELEMENT_BYTES.saturating_mul(to_u64(hi - lo)))
        })
        .collect())
}

/// A fixed-size list row is its brackets, its `value_length` elements and one
/// comma per element. Its child is sliced with the array, so it is all used.
fn fixed_list_sizes(list: &FixedSizeListArray) -> Result<Vec<u64>, RunError> {
    let size = usize::try_from(list.value_length()).unwrap_or(0);
    let prefix = prefix_sums(&array_row_sizes(list.values().as_ref())?);
    let ranges: Vec<(usize, usize)> = (0..list.len())
        .map(|row| (row * size, (row + 1) * size))
        .collect();
    let sums = range_sums(&prefix, &ranges)?;
    Ok(sums
        .into_iter()
        .map(|sum| {
            BRACKETS
                .saturating_add(sum)
                .saturating_add(ELEMENT_BYTES.saturating_mul(to_u64(size)))
        })
        .collect())
}

/// A map row is an object: each entry's key, value, and member punctuation.
fn map_sizes(array: &dyn Array) -> Result<Vec<u64>, RunError> {
    let map = array
        .as_any()
        .downcast_ref::<MapArray>()
        .ok_or_else(sizing_failure)?;
    if map.keys().len() != map.values().len() {
        return Err(sizing_failure());
    }
    let window = child_window(map.value_offsets(), map.keys().len())?;
    let keys = prefix_sums(&array_row_sizes(
        map.keys().slice(window.start, window.length).as_ref(),
    )?);
    let values = prefix_sums(&array_row_sizes(
        map.values().slice(window.start, window.length).as_ref(),
    )?);
    let key_sums = range_sums(&keys, &window.ranges)?;
    let value_sums = range_sums(&values, &window.ranges)?;
    Ok(key_sums
        .iter()
        .zip(value_sums.iter())
        .zip(window.ranges.iter())
        .map(|((key, value), &(lo, hi))| {
            BRACKETS
                .saturating_add(*key)
                .saturating_add(*value)
                .saturating_add(MEMBER_BYTES.saturating_mul(to_u64(hi - lo)))
        })
        .collect())
}

/// A dictionary row is the size of its dictionary value, by its key. A null
/// row is `null`, and its key is never read: Arrow allows a null slot's key to
/// point past the values.
fn dictionary_sizes(array: &dyn Array) -> Result<Vec<u64>, RunError> {
    let dictionary = array.as_any_dictionary();
    let values = dictionary.values();
    if values.is_empty() {
        // An empty dictionary has no value to point at, so every row is null.
        return Ok(vec![NULL_BYTES; array.len()]);
    }
    let sizes = array_row_sizes(values.as_ref())?;
    let nulls = dictionary.nulls();
    dictionary
        .normalized_keys()
        .into_iter()
        .enumerate()
        .map(|(row, key)| {
            if nulls.is_some_and(|nulls| nulls.is_null(row)) {
                return Ok(NULL_BYTES);
            }
            sizes.get(key).copied().ok_or_else(sizing_failure)
        })
        .collect()
}

/// A run-end-encoded row is the size of the run's value that holds it.
fn run_sizes<R: RunEndIndexType>(array: &dyn Array) -> Result<Vec<u64>, RunError> {
    let run = array
        .as_any()
        .downcast_ref::<RunArray<R>>()
        .ok_or_else(sizing_failure)?;
    let sizes = array_row_sizes(run.values().as_ref())?;
    (0..run.len())
        .map(|row| {
            sizes
                .get(run.get_physical_index(row))
                .copied()
                .ok_or_else(sizing_failure)
        })
        .collect()
}

/// A type without a per-row rule gets its whole memory spread evenly over its
/// rows. This is conservative and not exact. `get_array_memory_size` counts the
/// whole buffers, so a slice of a larger array charges each of its rows for
/// bytes that belong to the rows outside the slice. The estimate can only read
/// high, which is safe: the caller stops earlier, never later.
fn memory_sizes(array: &dyn Array) -> Vec<u64> {
    let len = array.len();
    if len == 0 {
        return Vec::new();
    }
    let per_row = to_u64(array.get_array_memory_size()).div_ceil(to_u64(len));
    vec![per_row; len]
}

/// A reservation of no bytes, for the output that is about to be built.
fn reserve(budget: &Budget) -> MemoryReservation {
    MemoryConsumer::new("sql result encoding").register(&budget.pool)
}

/// Output buffer of one encoding pass. A write that the budget or the pool
/// refuses fails and sets `refused`, which tells that apart from a real
/// encoding failure.
struct CappedBuffer<'a> {
    bytes: Vec<u8>,
    budget: &'a Budget,
    reservation: MemoryReservation,
    refused: bool,
}

impl Write for CappedBuffer<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let fits = self.bytes.len().saturating_add(buf.len()) <= self.budget.max_bytes;
        if !fits || self.reservation.try_grow(buf.len()).is_err() {
            self.refused = true;
            return Err(io::Error::other("encoded output is over its budget"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Run `write` into a fresh [`CappedBuffer`] and return the bytes it wrote,
/// with the reservation that covers them. The caller keeps the reservation
/// for as long as it holds the output.
///
/// # Errors
///
/// [`RunError::TooLarge`] when the budget or the pool refuses a write; otherwise
/// [`RunError::Internal`] with `message`.
fn encode_within(
    budget: &Budget,
    message: &str,
    write: impl FnOnce(&mut CappedBuffer<'_>) -> Result<(), ArrowError>,
) -> Result<(Vec<u8>, MemoryReservation), RunError> {
    let mut sink = CappedBuffer {
        bytes: Vec::new(),
        budget,
        reservation: reserve(budget),
        refused: false,
    };
    match write(&mut sink) {
        Ok(()) => Ok((sink.bytes, sink.reservation)),
        Err(_) if sink.refused => Err(RunError::TooLarge),
        Err(error) => Err(encoding_failure(message, error)),
    }
}

/// Encode `batches` as JSON objects, one per row, in column order.
///
/// The returned reservation covers the parsed rows, which take about three
/// times their encoded bytes. The caller holds it while it holds the rows.
///
/// # Errors
///
/// Returns [`RunError::TooLarge`] when the output passes `budget`, or when the
/// pool refuses the reservation for the parsed rows. Returns
/// [`RunError::Internal`] when a timestamp cannot be normalized or the JSON
/// writer fails. The message does not include cell values.
pub(crate) fn json_rows(
    batches: &[RecordBatch],
    schema: &Schema,
    budget: &Budget,
) -> Result<(Vec<Map<String, Value>>, MemoryReservation), RunError> {
    if batches.iter().all(|batch| batch.num_rows() == 0) {
        return Ok((Vec::new(), reserve(budget)));
    }
    let normalized = batches
        .iter()
        .map(|batch| normalize_timestamps(batch, schema))
        .collect::<Result<Vec<_>, _>>()?;
    let (buffer, reservation) = encode_within(budget, "failed to encode rows", |sink| {
        let mut writer: ArrayWriter<&mut CappedBuffer> = json_writer().build(sink);
        for batch in &normalized {
            writer.write(batch)?;
        }
        writer.finish()
    })?;
    let rows: Vec<Map<String, Value>> = serde_json::from_slice(&buffer).map_err(|_| {
        RunError::Internal(DataFusionError::Internal(
            "failed to read encoded rows".into(),
        ))
    })?;
    // The encoded bytes are already reserved. Two more times their length
    // covers the parsed rows.
    reservation
        .try_grow(buffer.len().saturating_mul(2))
        .map_err(|_| RunError::TooLarge)?;
    Ok((rows, reservation))
}

/// Cast every timestamp column to nanoseconds. A zoned value becomes UTC.
fn normalize_timestamps(batch: &RecordBatch, schema: &Schema) -> Result<RecordBatch, RunError> {
    if batch.schema().fields().len() != schema.fields().len() {
        return Err(RunError::Internal(DataFusionError::Internal(
            "encoded batch does not match the output schema".into(),
        )));
    }
    let mut fields: Vec<Arc<Field>> = schema.fields().iter().cloned().collect();
    let mut columns = batch.columns().to_vec();
    for (index, field) in schema.fields().iter().enumerate() {
        let DataType::Timestamp(_, zone) = field.data_type() else {
            continue;
        };
        let target = DataType::Timestamp(
            TimeUnit::Nanosecond,
            zone.as_ref().map(|_| Arc::<str>::from("UTC")),
        );
        if field.data_type() == &target {
            continue;
        }
        let casted = cast(columns[index].as_ref(), &target)
            .map_err(|error| encoding_failure("failed to normalize a timestamp", error))?;
        fields[index] = Arc::new(
            Field::new(field.name(), target, field.is_nullable())
                .with_metadata(field.metadata().clone()),
        );
        columns[index] = casted;
    }
    let schema = Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()));
    RecordBatch::try_new(schema, columns)
        .map_err(|error| encoding_failure("failed to rebuild a batch", error))
}

fn encoding_failure(message: &str, error: ArrowError) -> RunError {
    RunError::Internal(
        DataFusionError::ArrowError(Box::new(error), None).context(message.to_owned()),
    )
}

fn json_writer() -> WriterBuilder {
    WriterBuilder::new()
        .with_explicit_nulls(true)
        .with_timestamp_tz_format("%Y-%m-%dT%H:%M:%S%.9fZ".to_owned())
        .with_timestamp_format("%Y-%m-%dT%H:%M:%S%.9f".to_owned())
}

/// Header line for a CSV export. Names are RFC 4180-quoted.
pub(crate) fn csv_header(schema: &Schema) -> Bytes {
    let mut line = String::new();
    for (index, field) in schema.fields().iter().enumerate() {
        if index > 0 {
            line.push(',');
        }
        push_csv_field(&mut line, &guard_formula(field.name()));
    }
    line.push_str("\r\n");
    Bytes::from(line)
}

/// One CSV chunk for `batch`. Null is an empty field, strings are their text,
/// numbers and bools are JSON text, and arrays and objects are JSON text.
///
/// # Errors
///
/// Returns [`RunError::Internal`] when JSON encoding fails.
pub(crate) fn csv_rows(
    batch: &RecordBatch,
    schema: &Schema,
    budget: &Budget,
) -> Result<Option<Bytes>, RunError> {
    let (rows, _reservation) = json_rows(std::slice::from_ref(batch), schema, budget)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let names: Vec<&str> = schema
        .fields()
        .iter()
        .map(|field| field.name().as_str())
        .collect();
    let mut body = String::new();
    for row in &rows {
        for (index, name) in names.iter().enumerate() {
            if index > 0 {
                body.push(',');
            }
            push_csv_field(&mut body, &csv_text(&row[*name]));
        }
        body.push_str("\r\n");
    }
    Ok(Some(Bytes::from(body)))
}

fn csv_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => guard_formula(text),
        Value::Number(number) => number.to_string(),
        Value::Bool(value) => value.to_string(),
        other => other.to_string(),
    }
}

/// Stop a spreadsheet from running a text cell as a formula (OWASP). A string
/// that starts with `=`, `+`, `-`, `@`, tab or CR gets a leading `'`, which
/// spreadsheets show as text. Only CSV string cells and header names pass
/// through here; numbers, bools, nulls and JSON arrays and objects do not.
fn guard_formula(text: &str) -> String {
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{text}")
    } else {
        text.to_owned()
    }
}

fn push_csv_field(out: &mut String, text: &str) {
    if text.contains(['"', ',', '\r', '\n']) {
        out.push('"');
        out.push_str(&text.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(text);
    }
}

/// One NDJSON chunk for `batch`, using the same timestamp normalization as JSON rows.
///
/// # Errors
///
/// Returns [`RunError::Internal`] when encoding fails.
pub(crate) fn ndjson_rows(
    batch: &RecordBatch,
    schema: &Schema,
    budget: &Budget,
) -> Result<Option<Bytes>, RunError> {
    if batch.num_rows() == 0 {
        return Ok(None);
    }
    let batch = normalize_timestamps(batch, schema)?;
    let (buffer, _reservation) = encode_within(budget, "failed to encode rows", |sink| {
        let mut writer: LineDelimitedWriter<&mut CappedBuffer> = json_writer().build(sink);
        writer.write(&batch)?;
        writer.finish()
    })?;
    Ok(Some(Bytes::from(buffer)))
}

/// Parquet writer that yields a chunk after each flushed row group.
pub(crate) struct ParquetEncoder {
    writer: ArrowWriter<Vec<u8>>,
    /// Row groups whose bytes were already returned.
    taken_row_groups: usize,
}

impl ParquetEncoder {
    /// # Errors
    ///
    /// Returns [`RunError::Internal`] when the Parquet writer cannot be created.
    pub(crate) fn new(schema: &Schema) -> Result<Self, RunError> {
        Self::with_row_group_rows(schema, PARQUET_ROW_GROUP_ROWS)
    }

    /// # Errors
    ///
    /// Returns [`RunError::Internal`] when the Parquet writer cannot be created.
    fn with_row_group_rows(schema: &Schema, rows: usize) -> Result<Self, RunError> {
        // Callers pass `PARQUET_ROW_GROUP_ROWS` or a test value; `set_max_row_group_row_count` panics on 0.
        debug_assert!(rows > 0);
        // `ZstdLevel::default` is level 1.
        let properties = WriterProperties::builder()
            .set_compression(Compression::ZSTD(ZstdLevel::default()))
            .set_max_row_group_row_count(Some(rows))
            .build();
        let writer = ArrowWriter::try_new(Vec::new(), Arc::new(schema.clone()), Some(properties))
            .map_err(parquet_failure)?;
        Ok(Self {
            writer,
            taken_row_groups: 0,
        })
    }

    /// Write `batch`. Returns the bytes of every row group completed since the
    /// last chunk: one closed by the writer at the row limit, or the open one
    /// once it passes 32 MiB.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Internal`] when the Parquet writer fails.
    pub(crate) fn push(&mut self, batch: &RecordBatch) -> Result<Option<Bytes>, RunError> {
        self.writer.write(batch).map_err(parquet_failure)?;
        if self.writer.in_progress_size() > PARQUET_CHUNK_BYTES {
            self.writer.flush().map_err(parquet_failure)?;
        }
        if self.writer.flushed_row_groups().len() == self.taken_row_groups {
            return Ok(None);
        }
        self.taken_row_groups = self.writer.flushed_row_groups().len();
        self.take_bytes()
    }

    /// Write the footer and return every byte still buffered.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Internal`] when the footer cannot be written.
    pub(crate) fn finish(mut self) -> Result<Bytes, RunError> {
        // `close` consumes the writer, so the footer is written with `finish`
        // (the same work) and the buffer is then taken. `flush` of a row group
        // does not flush `TrackedWrite`'s `BufWriter`; `sync` does that first,
        // otherwise `inner_mut` is missing the tail of the chunk.
        self.writer.finish().map_err(parquet_failure)?;
        self.writer.sync().map_err(io_failure)?;
        Ok(Bytes::from(std::mem::take(self.writer.inner_mut())))
    }

    fn take_bytes(&mut self) -> Result<Option<Bytes>, RunError> {
        self.writer.sync().map_err(io_failure)?;
        let bytes = std::mem::take(self.writer.inner_mut());
        Ok((!bytes.is_empty()).then(|| Bytes::from(bytes)))
    }
}

fn parquet_failure(error: parquet::errors::ParquetError) -> RunError {
    RunError::Internal(DataFusionError::ParquetError(Box::new(error)))
}

fn io_failure(error: io::Error) -> RunError {
    RunError::Internal(DataFusionError::IoError(error))
}

#[cfg(test)]
mod tests {
    use arrow::array::{
        Decimal32Array, Decimal128Array, Decimal256Array, DictionaryArray, Int32Array, Int64Array,
        IntervalDayTimeArray, IntervalMonthDayNanoArray, IntervalYearMonthArray, ListArray,
        StringArray, StringViewArray,
    };
    use arrow::buffer::{NullBuffer, OffsetBuffer};
    use arrow::datatypes::{Int32Type, IntervalDayTime, IntervalMonthDayNano, i256};
    use datafusion::execution::memory_pool::UnboundedMemoryPool;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    use super::*;
    use crate::test_support::{example_query, rows};
    use crate::views::QueryContext;

    fn sizes(array: &dyn Array) -> Vec<u64> {
        array_row_sizes(array).expect("sizes")
    }

    #[test]
    fn a_utf8_view_counts_its_data_not_its_view() {
        let big = "x".repeat(8 << 20);
        let array = StringViewArray::from(vec![Some(big.as_str()), Some("a"), None]);
        assert_eq!(sizes(&array), vec![(8 << 20) + 2, 3, NULL_BYTES]);
    }

    #[test]
    fn a_dictionary_counts_each_row_by_its_key() {
        let long = "x".repeat(1000);
        let values = StringArray::from(vec![Some(long.as_str()), Some("y")]);
        let keys = Int32Array::from(vec![Some(0), Some(1), Some(0), None]);
        let dictionary =
            DictionaryArray::<Int32Type>::try_new(keys, Arc::new(values)).expect("dictionary");
        assert_eq!(sizes(&dictionary), vec![1002, 3, 1002, NULL_BYTES]);
    }

    #[test]
    fn a_sliced_list_is_sized_by_the_rows_it_keeps() {
        let big = "z".repeat(1 << 20);
        let child = StringArray::from(vec![big.as_str(), "a", "b"]);
        let list = ListArray::new(
            Arc::new(Field::new("item", DataType::Utf8, true)),
            OffsetBuffer::from_lengths(vec![1, 1, 1]),
            Arc::new(child),
            None,
        );
        assert_eq!(
            sizes(&list)[0],
            BRACKETS + (1 << 20) + QUOTES + ELEMENT_BYTES
        );
        // The rows that remain hold "a" and "b". The 1 MiB element stays in the
        // child buffer, and the estimate does not count it.
        let kept = list.slice(1, 2);
        assert_eq!(sizes(&kept), vec![6, 6]);
    }

    #[tokio::test]
    async fn the_estimate_bounds_the_json_text_of_real_rows() {
        let ctx = QueryContext::new(example_query(), None).expect("context");
        let mut checked = 0;
        for batch in rows(&ctx, "SELECT * FROM spans").await {
            let schema = batch.schema();
            let estimate: u64 = batch_row_sizes(&batch).expect("sizes").into_iter().sum();
            let budget = Budget {
                max_bytes: usize::MAX,
                pool: Arc::new(UnboundedMemoryPool::default()),
            };
            let text = ndjson_rows(&batch, schema.as_ref(), &budget)
                .expect("ndjson")
                .expect("rows");
            // NDJSON ends every row with a newline, which the estimate leaves out.
            let json = text.len() - batch.num_rows();
            assert!(estimate >= json as u64, "estimate {estimate} < {json}");
            checked += batch.num_rows();
        }
        assert!(checked > 0);
    }

    /// Encodes `array` as one NDJSON column and checks that each row's estimate
    /// is at least the row's text, which is `{"v":<value>}`.
    fn assert_bounds_text(array: Arc<dyn Array>) {
        let schema = Schema::new(vec![Field::new("v", array.data_type().clone(), true)]);
        let batch = RecordBatch::try_new(Arc::new(schema.clone()), vec![array]).expect("batch");
        let estimates = batch_row_sizes(&batch).expect("sizes");
        let budget = Budget {
            max_bytes: usize::MAX,
            pool: Arc::new(UnboundedMemoryPool::default()),
        };
        let text = ndjson_rows(&batch, &schema, &budget)
            .expect("ndjson")
            .expect("rows");
        let text = String::from_utf8(text.to_vec()).expect("utf8");
        for (row, line) in text.lines().enumerate() {
            assert!(
                estimates[row] >= line.len() as u64,
                "{:?} row {row}: estimate {} < {line}",
                schema.field(0).data_type(),
                estimates[row]
            );
        }
    }

    #[test]
    fn decimals_and_intervals_are_sized_by_their_text() {
        let nine = |digits: u32| 10_i128.pow(digits) - 1;
        assert_bounds_text(Arc::new(
            Decimal128Array::from(vec![-nine(38), nine(38)])
                .with_precision_and_scale(38, 37)
                .expect("decimal128"),
        ));
        assert_bounds_text(Arc::new(
            Decimal128Array::from(vec![-nine(38), nine(38)])
                .with_precision_and_scale(38, -20)
                .expect("decimal128 with a negative scale"),
        ));
        let max_76 = i256::from_string(&format!("-{}", "9".repeat(76))).expect("i256");
        assert_bounds_text(Arc::new(
            Decimal256Array::from(vec![max_76])
                .with_precision_and_scale(76, 0)
                .expect("decimal256"),
        ));
        assert_bounds_text(Arc::new(Decimal32Array::from(vec![i32::MIN, i32::MAX])));
        assert_bounds_text(Arc::new(IntervalYearMonthArray::from(vec![
            i32::MIN,
            i32::MAX,
        ])));
        assert_bounds_text(Arc::new(IntervalDayTimeArray::from(vec![
            IntervalDayTime::new(i32::MIN, i32::MIN),
            IntervalDayTime::new(i32::MAX, i32::MAX),
        ])));
        assert_bounds_text(Arc::new(IntervalMonthDayNanoArray::from(vec![
            IntervalMonthDayNano::new(i32::MIN, i32::MIN, i64::MIN),
            IntervalMonthDayNano::new(i32::MAX, i32::MAX, i64::MAX),
        ])));
    }

    #[test]
    fn a_null_dictionary_slot_is_null_whatever_its_key() {
        let values = StringArray::from(vec![Some("a"), Some("b")]);
        // The null slot's key is 99, past the two values. Arrow allows it.
        let keys = Int32Array::new(
            vec![0, 99, 1].into(),
            Some(NullBuffer::from(vec![true, false, true])),
        );
        let dictionary =
            DictionaryArray::<Int32Type>::try_new(keys, Arc::new(values)).expect("dictionary");
        assert_eq!(sizes(&dictionary), vec![3, NULL_BYTES, 3]);
    }

    fn int_schema() -> Schema {
        Schema::new(vec![Field::new("n", DataType::Int64, false)])
    }

    fn int_batch(schema: &Schema, values: std::ops::Range<i64>) -> RecordBatch {
        let array = Int64Array::from(values.collect::<Vec<_>>());
        RecordBatch::try_new(Arc::new(schema.clone()), vec![Arc::new(array)]).expect("batch")
    }

    fn read_back(file: Bytes) -> (usize, Vec<i64>) {
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("parquet");
        let groups = builder.metadata().num_row_groups();
        let values = builder
            .build()
            .expect("reader")
            .map(|batch| batch.expect("batch"))
            .flat_map(|batch| {
                batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .expect("int64")
                    .values()
                    .to_vec()
            })
            .collect();
        (groups, values)
    }

    #[test]
    fn parquet_chunks_follow_row_groups() {
        let schema = int_schema();
        let mut encoder = ParquetEncoder::with_row_group_rows(&schema, 1_000).expect("encoder");
        let mut file = Vec::new();
        for index in 0..10 {
            let batch = int_batch(&schema, index * 1_000..index * 1_000 + 1_000);
            let chunk = encoder
                .push(&batch)
                .expect("push")
                .expect("each batch completes one row group");
            assert!(!chunk.is_empty());
            file.extend_from_slice(&chunk);
        }
        file.extend_from_slice(&encoder.finish().expect("finish"));

        let (groups, values) = read_back(Bytes::from(file));
        assert_eq!(groups, 10);
        assert_eq!(values, (0..10_000).collect::<Vec<i64>>());
    }

    #[test]
    fn a_partial_row_group_waits_for_finish() {
        let schema = int_schema();
        let mut encoder = ParquetEncoder::with_row_group_rows(&schema, 1_000).expect("encoder");
        assert!(
            encoder
                .push(&int_batch(&schema, 0..999))
                .expect("push")
                .is_none()
        );
        let chunk = encoder
            .push(&int_batch(&schema, 999..1_001))
            .expect("push")
            .expect("the group closes at 1,000 rows");
        let mut file = chunk.to_vec();
        file.extend_from_slice(&encoder.finish().expect("finish"));

        let (groups, values) = read_back(Bytes::from(file));
        assert_eq!(groups, 2);
        assert_eq!(values, (0..1_001).collect::<Vec<i64>>());
    }
}
