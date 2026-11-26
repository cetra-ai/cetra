//! Private write-only Arrow schemas and `cetra_column::Batch` -> `RecordBatch` encoder
//! for `file_format::BatchFileWriter` (`AGENTS.md`, `07-m7-cutover.md` § Invariants).
//!
//! All `arrow` crate usage in `cetra-storage` is isolated inside `file_format/`.

use std::sync::{Arc, LazyLock};

use arrow::array::{
    ArrayRef, ArrowPrimitiveType, BooleanArray, FixedSizeBinaryArray, Float64Array, Int64Array,
    ListArray, MapArray, PrimitiveArray, RecordBatch, RecordBatchOptions, StringArray, StructArray,
    TimestampNanosecondArray,
};
use arrow::buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{
    DataType as ArrowDataType, Field as ArrowField, Fields, Int32Type, Schema as ArrowSchema,
    SchemaRef, TimeUnit, UInt16Type,
};
use arrow::error::ArrowError;
use cetra_column::{Batch, Bitmap, Column, ColumnError, FlatColumn, FlatData, Utf8Data};
use cetra_core::schema::{event_field, link_field, scores, spans, traces};

fn invalid(reason: &'static str) -> ColumnError {
    ColumnError::InvalidBatch { reason }
}

fn overflow(reason: &'static str) -> ColumnError {
    ColumnError::Overflow { reason }
}

fn arrow_err(_err: ArrowError) -> ColumnError {
    invalid("arrow array construction failed")
}

fn timestamp_ns() -> ArrowDataType {
    ArrowDataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into()))
}

fn utf8_list() -> ArrowDataType {
    ArrowDataType::List(Arc::new(ArrowField::new(
        "item",
        ArrowDataType::Utf8,
        false,
    )))
}

fn map_utf8() -> ArrowDataType {
    let entries = ArrowField::new(
        "entries",
        ArrowDataType::Struct(Fields::from(vec![
            ArrowField::new("keys", ArrowDataType::Utf8, false),
            ArrowField::new("values", ArrowDataType::Utf8, true),
        ])),
        false,
    );
    ArrowDataType::Map(Arc::new(entries), false)
}

fn event_struct() -> ArrowDataType {
    ArrowDataType::Struct(Fields::from(vec![
        ArrowField::new(event_field::TIME, timestamp_ns(), false),
        ArrowField::new(event_field::NAME, ArrowDataType::Utf8, false),
        ArrowField::new(event_field::ATTRIBUTES, ArrowDataType::Utf8, true),
    ]))
}

fn link_struct() -> ArrowDataType {
    ArrowDataType::Struct(Fields::from(vec![
        ArrowField::new(
            link_field::TRACE_ID,
            ArrowDataType::FixedSizeBinary(16),
            false,
        ),
        ArrowField::new(
            link_field::SPAN_ID,
            ArrowDataType::FixedSizeBinary(8),
            false,
        ),
        ArrowField::new(link_field::ATTRIBUTES, ArrowDataType::Utf8, true),
    ]))
}

fn struct_list(item: ArrowDataType) -> ArrowDataType {
    ArrowDataType::List(Arc::new(ArrowField::new("item", item, false)))
}

fn field(name: &'static str, data_type: ArrowDataType, nullable: bool) -> ArrowField {
    ArrowField::new(name, data_type, nullable)
}

static ARROW_SPANS_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(ArrowSchema::new(vec![
        field(spans::PROJECT_ID, ArrowDataType::Utf8, false),
        field(spans::TRACE_ID, ArrowDataType::FixedSizeBinary(16), false),
        field(spans::SPAN_ID, ArrowDataType::FixedSizeBinary(8), false),
        field(
            spans::PARENT_SPAN_ID,
            ArrowDataType::FixedSizeBinary(8),
            true,
        ),
        field(spans::NAME, ArrowDataType::Utf8, false),
        field(spans::SPAN_KIND, ArrowDataType::Utf8, false),
        field(spans::START_TIME, timestamp_ns(), false),
        field(spans::END_TIME, timestamp_ns(), false),
        field(spans::DURATION_NS, ArrowDataType::Int64, false),
        field(spans::STATUS, ArrowDataType::Utf8, false),
        field(spans::STATUS_MESSAGE, ArrowDataType::Utf8, true),
        field(spans::SERVICE_NAME, ArrowDataType::Utf8, true),
        field(spans::SESSION_ID, ArrowDataType::Utf8, true),
        field(spans::USER_ID, ArrowDataType::Utf8, true),
        field(spans::ENVIRONMENT, ArrowDataType::Utf8, true),
        field(spans::RELEASE, ArrowDataType::Utf8, true),
        field(spans::TAGS, utf8_list(), false),
        field(spans::METADATA, map_utf8(), false),
        field(spans::PROVIDER, ArrowDataType::Utf8, true),
        field(spans::REQUEST_MODEL, ArrowDataType::Utf8, true),
        field(spans::RESPONSE_MODEL, ArrowDataType::Utf8, true),
        field(spans::MODEL, ArrowDataType::Utf8, true),
        field(spans::PROMPT_NAME, ArrowDataType::Utf8, true),
        field(spans::PROMPT_VERSION, ArrowDataType::Int64, true),
        field(spans::OPERATION, ArrowDataType::Utf8, true),
        field(spans::INPUT_TOKENS, ArrowDataType::Int64, true),
        field(spans::OUTPUT_TOKENS, ArrowDataType::Int64, true),
        field(spans::CACHE_READ_TOKENS, ArrowDataType::Int64, true),
        field(spans::CACHE_WRITE_TOKENS, ArrowDataType::Int64, true),
        field(spans::REASONING_TOKENS, ArrowDataType::Int64, true),
        field(spans::TOTAL_TOKENS, ArrowDataType::Int64, true),
        field(spans::COST_INPUT, ArrowDataType::Float64, true),
        field(spans::COST_OUTPUT, ArrowDataType::Float64, true),
        field(spans::COST_TOTAL, ArrowDataType::Float64, true),
        field(spans::COST_SOURCE, ArrowDataType::Utf8, true),
        field(spans::TTFT_NS, ArrowDataType::Int64, true),
        field(spans::REQUEST_PARAMS, ArrowDataType::Utf8, true),
        field(spans::INPUT, ArrowDataType::Utf8, true),
        field(spans::INPUT_FORMAT, ArrowDataType::Utf8, true),
        field(spans::OUTPUT, ArrowDataType::Utf8, true),
        field(spans::OUTPUT_FORMAT, ArrowDataType::Utf8, true),
        field(spans::INPUT_PREVIEW, ArrowDataType::Utf8, true),
        field(spans::OUTPUT_PREVIEW, ArrowDataType::Utf8, true),
        field(spans::SYSTEM_INSTRUCTIONS, ArrowDataType::Utf8, true),
        field(spans::TOOL_DEFINITIONS, ArrowDataType::Utf8, true),
        field(spans::ATTRIBUTES, map_utf8(), false),
        field(spans::RESOURCE_ATTRIBUTES, map_utf8(), false),
        field(spans::SCOPE_NAME, ArrowDataType::Utf8, true),
        field(spans::SCOPE_VERSION, ArrowDataType::Utf8, true),
        field(spans::EVENTS, struct_list(event_struct()), false),
        field(spans::LINKS, struct_list(link_struct()), false),
        field(spans::MEDIA_REFS, utf8_list(), false),
        field(spans::TRUNCATED, utf8_list(), false),
        field(spans::CONVENTIONS, utf8_list(), false),
        field(spans::INGEST_TIME, timestamp_ns(), false),
        field(spans::SCHEMA_VERSION, ArrowDataType::UInt16, false),
    ]))
});

static ARROW_TRACES_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(ArrowSchema::new(vec![
        field(traces::PROJECT_ID, ArrowDataType::Utf8, false),
        field(traces::TRACE_ID, ArrowDataType::FixedSizeBinary(16), false),
        field(traces::START_TIME, timestamp_ns(), false),
        field(traces::END_TIME, timestamp_ns(), false),
        field(traces::NAME, ArrowDataType::Utf8, false),
        field(
            traces::ROOT_SPAN_ID,
            ArrowDataType::FixedSizeBinary(8),
            true,
        ),
        field(traces::HAS_ROOT, ArrowDataType::Boolean, false),
        field(traces::STATUS, ArrowDataType::Utf8, false),
        field(traces::SESSION_ID, ArrowDataType::Utf8, true),
        field(traces::USER_ID, ArrowDataType::Utf8, true),
        field(traces::ENVIRONMENT, ArrowDataType::Utf8, true),
        field(traces::RELEASE, ArrowDataType::Utf8, true),
        field(traces::TAGS, utf8_list(), false),
        field(traces::METADATA, map_utf8(), false),
        field(traces::INPUT_PREVIEW, ArrowDataType::Utf8, true),
        field(traces::OUTPUT_PREVIEW, ArrowDataType::Utf8, true),
        field(traces::SPAN_COUNT, ArrowDataType::Int64, false),
        field(traces::ERROR_COUNT, ArrowDataType::Int64, false),
        field(traces::LLM_COUNT, ArrowDataType::Int64, false),
        field(traces::INPUT_TOKENS, ArrowDataType::Int64, true),
        field(traces::OUTPUT_TOKENS, ArrowDataType::Int64, true),
        field(traces::CACHE_READ_TOKENS, ArrowDataType::Int64, true),
        field(traces::CACHE_WRITE_TOKENS, ArrowDataType::Int64, true),
        field(traces::REASONING_TOKENS, ArrowDataType::Int64, true),
        field(traces::TOTAL_TOKENS, ArrowDataType::Int64, true),
        field(traces::COST_TOTAL, ArrowDataType::Float64, true),
        field(traces::MODELS, utf8_list(), false),
        field(traces::PROMPTS, utf8_list(), false),
        field(traces::LAST_INGEST_TIME, timestamp_ns(), false),
        field(traces::PARTIAL_COUNT, ArrowDataType::Int32, false),
    ]))
});

static ARROW_SCORES_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(ArrowSchema::new(vec![
        field(scores::PROJECT_ID, ArrowDataType::Utf8, false),
        field(scores::SCORE_ID, ArrowDataType::Utf8, false),
        field(scores::TARGET, ArrowDataType::Utf8, false),
        field(scores::TRACE_ID, ArrowDataType::FixedSizeBinary(16), true),
        field(scores::SPAN_ID, ArrowDataType::FixedSizeBinary(8), true),
        field(scores::SESSION_ID, ArrowDataType::Utf8, true),
        field(scores::USER_ID, ArrowDataType::Utf8, true),
        field(scores::NAME, ArrowDataType::Utf8, false),
        field(scores::DATA_TYPE, ArrowDataType::Utf8, false),
        field(scores::VALUE_NUM, ArrowDataType::Float64, true),
        field(scores::VALUE_STR, ArrowDataType::Utf8, true),
        field(scores::REASON, ArrowDataType::Utf8, true),
        field(scores::SOURCE, ArrowDataType::Utf8, false),
        field(scores::EVALUATOR_ID, ArrowDataType::Utf8, true),
        field(scores::RULE_ID, ArrowDataType::Utf8, true),
        field(scores::EXPERIMENT_ID, ArrowDataType::Utf8, true),
        field(scores::DATASET_ITEM_ID, ArrowDataType::Utf8, true),
        field(scores::QUEUE_ID, ArrowDataType::Utf8, true),
        field(scores::AUTHOR_USER_ID, ArrowDataType::Utf8, true),
        field(scores::METADATA, map_utf8(), false),
        field(scores::CREATED_AT, timestamp_ns(), false),
        field(scores::UPDATED_AT, timestamp_ns(), false),
        field(scores::DELETED, ArrowDataType::Boolean, false),
        field(scores::INGEST_TIME, timestamp_ns(), false),
    ]))
});

pub(super) fn arrow_spans_schema() -> SchemaRef {
    Arc::clone(&ARROW_SPANS_SCHEMA)
}

pub(super) fn arrow_traces_schema() -> SchemaRef {
    Arc::clone(&ARROW_TRACES_SCHEMA)
}

pub(super) fn arrow_scores_schema() -> SchemaRef {
    Arc::clone(&ARROW_SCORES_SCHEMA)
}

/// Converts a `cetra_column::Batch` into an Arrow `RecordBatch` conforming to `target_schema`.
pub(super) fn batch_to_record_batch(
    batch: &Batch,
    target_schema: &SchemaRef,
) -> Result<RecordBatch, ColumnError> {
    let compacted = batch.compact(None)?;
    if target_schema.fields().len() != compacted.columns().len() {
        return Err(invalid("target schema column count mismatch"));
    }
    let mut arrays = Vec::with_capacity(compacted.columns().len());
    for (col, field) in compacted.columns().iter().zip(target_schema.fields()) {
        if !field.is_nullable() && col.has_nulls() {
            return Err(invalid("null values in non-nullable target field"));
        }
        arrays.push(column_to_arrow_array(col, field.data_type())?);
    }
    let options = RecordBatchOptions::new().with_row_count(Some(compacted.num_rows()));
    RecordBatch::try_new_with_options(Arc::clone(target_schema), arrays, &options)
        .map_err(arrow_err)
}

fn bitmap_to_null_buffer(bm: Option<&Bitmap>) -> Option<NullBuffer> {
    bm.map(|b| NullBuffer::new(BooleanBuffer::from_iter((0..b.len()).map(|i| b.get(i)))))
}

fn column_to_arrow_array(col: &Column, target: &ArrowDataType) -> Result<ArrayRef, ColumnError> {
    match col {
        Column::Flat(f) => flat_to_arrow_array(f, target),
        other => {
            let flat = other.materialize(None)?;
            flat_to_arrow_array(&flat, target)
        }
    }
}

fn narrow_int_array<T>(
    vals: &[i64],
    nulls: Option<&Bitmap>,
    null_buf: Option<NullBuffer>,
) -> Result<ArrayRef, ColumnError>
where
    T: ArrowPrimitiveType,
    T::Native: TryFrom<i64> + Default,
{
    let mut out = Vec::with_capacity(vals.len());
    for (i, &v) in vals.iter().enumerate() {
        if nulls.is_some_and(|bm| !bm.get(i)) {
            out.push(T::Native::default());
        } else {
            let narrowed = T::Native::try_from(v)
                .map_err(|_| overflow("integer value out of range for target Arrow type"))?;
            out.push(narrowed);
        }
    }
    Ok(Arc::new(PrimitiveArray::<T>::new(
        ScalarBuffer::from(out),
        null_buf,
    )))
}

fn offsets_to_i32(offsets: &[u32]) -> Result<OffsetBuffer<i32>, ColumnError> {
    let mut out = Vec::with_capacity(offsets.len());
    for &o in offsets {
        out.push(i32::try_from(o).map_err(|_| overflow("offset exceeds i32::MAX"))?);
    }
    Ok(OffsetBuffer::new(ScalarBuffer::from(out)))
}

fn utf8_to_arrow(u: &Utf8Data, null_buf: Option<NullBuffer>) -> Result<ArrayRef, ColumnError> {
    let offset_buf = offsets_to_i32(u.offsets())?;
    let arr = StringArray::try_new(offset_buf, Buffer::from_slice_ref(u.bytes()), null_buf)
        .map_err(arrow_err)?;
    Ok(Arc::new(arr))
}

fn flat_to_arrow_array(flat: &FlatColumn, target: &ArrowDataType) -> Result<ArrayRef, ColumnError> {
    let null_buf = bitmap_to_null_buffer(flat.nulls());
    match (flat.data(), target) {
        (FlatData::Bool(bm), ArrowDataType::Boolean) => {
            let bools = BooleanBuffer::from_iter((0..bm.len()).map(|i| bm.get(i)));
            Ok(Arc::new(BooleanArray::new(bools, null_buf)))
        }
        (FlatData::I64(vals), ArrowDataType::UInt16) => {
            narrow_int_array::<UInt16Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::Int32) => {
            narrow_int_array::<Int32Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::Int64) => Ok(Arc::new(Int64Array::new(
            ScalarBuffer::from(vals.clone()),
            null_buf,
        ))),
        (FlatData::F64(vals), ArrowDataType::Float64) => Ok(Arc::new(Float64Array::new(
            ScalarBuffer::from(vals.clone()),
            null_buf,
        ))),
        (
            FlatData::TimestampNs { values, utc },
            ArrowDataType::Timestamp(TimeUnit::Nanosecond, tz),
        ) => {
            if *utc != tz.is_some() {
                return Err(invalid("target timestamp timezone mismatch"));
            }
            let arr = TimestampNanosecondArray::new(ScalarBuffer::from(values.clone()), null_buf)
                .with_timezone_opt(tz.clone());
            Ok(Arc::new(arr))
        }
        (FlatData::Utf8(u), ArrowDataType::Utf8) => utf8_to_arrow(u, null_buf),
        (FlatData::FixedBytes8(vals), ArrowDataType::FixedSizeBinary(8)) => {
            let mut raw = Vec::with_capacity(vals.len() * 8);
            for v in vals {
                raw.extend_from_slice(v);
            }
            let arr = FixedSizeBinaryArray::try_new(8, Buffer::from_vec(raw), null_buf)
                .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        (FlatData::FixedBytes16(vals), ArrowDataType::FixedSizeBinary(16)) => {
            let mut raw = Vec::with_capacity(vals.len() * 16);
            for v in vals {
                raw.extend_from_slice(v);
            }
            let arr = FixedSizeBinaryArray::try_new(16, Buffer::from_vec(raw), null_buf)
                .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        (
            FlatData::List {
                offsets, values, ..
            },
            ArrowDataType::List(item_field),
        ) => {
            if !item_field.is_nullable() && values.has_nulls() {
                return Err(invalid("null values in non-nullable list item"));
            }
            let child_array = column_to_arrow_array(values, item_field.data_type())?;
            let arr = ListArray::try_new(
                Arc::clone(item_field),
                offsets_to_i32(offsets)?,
                child_array,
                null_buf,
            )
            .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        (FlatData::MapUtf8(map), ArrowDataType::Map(entries_field, sorted)) => {
            let ArrowDataType::Struct(entry_fields) = entries_field.data_type() else {
                return Err(invalid("target map entries must be a struct"));
            };
            if entry_fields.len() != 2 {
                return Err(invalid("target map struct must have 2 fields"));
            }
            let keys_arr = utf8_to_arrow(map.keys(), None)?;
            let val_nulls = bitmap_to_null_buffer(map.value_nulls());
            let vals_arr = utf8_to_arrow(map.values(), val_nulls)?;
            let struct_arr =
                StructArray::try_new(entry_fields.clone(), vec![keys_arr, vals_arr], None)
                    .map_err(arrow_err)?;
            let map_arr = MapArray::try_new(
                Arc::clone(entries_field),
                offsets_to_i32(map.offsets())?,
                struct_arr,
                null_buf,
                *sorted,
            )
            .map_err(arrow_err)?;
            Ok(Arc::new(map_arr))
        }
        (FlatData::Events(ev), ArrowDataType::List(item_field)) => {
            let ArrowDataType::Struct(fields) = item_field.data_type() else {
                return Err(invalid("target events item must be a struct"));
            };
            if fields.len() != 3 {
                return Err(invalid("target events struct shape mismatch"));
            }
            let ArrowDataType::Timestamp(TimeUnit::Nanosecond, tz) = fields[0].data_type() else {
                return Err(invalid("target events time field mismatch"));
            };
            let time_arr: ArrayRef = Arc::new(
                TimestampNanosecondArray::new(ScalarBuffer::from(ev.times().to_vec()), None)
                    .with_timezone_opt(tz.clone()),
            );
            let name_arr = utf8_to_arrow(ev.names(), None)?;
            let attr_nulls = bitmap_to_null_buffer(ev.attribute_nulls());
            let attr_arr = utf8_to_arrow(ev.attributes(), attr_nulls)?;
            let struct_arr =
                StructArray::try_new(fields.clone(), vec![time_arr, name_arr, attr_arr], None)
                    .map_err(arrow_err)?;
            let arr = ListArray::try_new(
                Arc::clone(item_field),
                offsets_to_i32(ev.offsets())?,
                Arc::new(struct_arr),
                null_buf,
            )
            .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        (FlatData::Links(lk), ArrowDataType::List(item_field)) => {
            let ArrowDataType::Struct(fields) = item_field.data_type() else {
                return Err(invalid("target links item must be a struct"));
            };
            if fields.len() != 3 {
                return Err(invalid("target links struct shape mismatch"));
            }
            let mut raw_traces = Vec::with_capacity(lk.trace_ids().len() * 16);
            for tid in lk.trace_ids() {
                raw_traces.extend_from_slice(tid);
            }
            let trace_arr: ArrayRef = Arc::new(
                FixedSizeBinaryArray::try_new(16, Buffer::from_vec(raw_traces), None)
                    .map_err(arrow_err)?,
            );
            let mut raw_spans = Vec::with_capacity(lk.span_ids().len() * 8);
            for sid in lk.span_ids() {
                raw_spans.extend_from_slice(sid);
            }
            let span_arr: ArrayRef = Arc::new(
                FixedSizeBinaryArray::try_new(8, Buffer::from_vec(raw_spans), None)
                    .map_err(arrow_err)?,
            );
            let attr_nulls = bitmap_to_null_buffer(lk.attribute_nulls());
            let attr_arr = utf8_to_arrow(lk.attributes(), attr_nulls)?;
            let struct_arr =
                StructArray::try_new(fields.clone(), vec![trace_arr, span_arr, attr_arr], None)
                    .map_err(arrow_err)?;
            let arr = ListArray::try_new(
                Arc::clone(item_field),
                offsets_to_i32(lk.offsets())?,
                Arc::new(struct_arr),
                null_buf,
            )
            .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        _ => Err(invalid("column data type does not match target Arrow type")),
    }
}
