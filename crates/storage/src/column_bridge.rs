//! Temporary conversion bridge between Arrow `RecordBatch` and `cetra_column::Batch`.
//!
//! Lives inside `cetra-storage` (D-QE-9) for M1–M6 differential testing while keeping
//! `cetra-column` free of `arrow`, `parquet`, `datafusion`, and `tokio` dependencies.

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, ArrowPrimitiveType, BinaryArray, BooleanArray, Date32Array, Decimal128Array,
    DictionaryArray, FixedSizeBinaryArray, Float32Array, Float64Array, GenericBinaryArray,
    GenericListArray, GenericStringArray, Int8Array, Int16Array, Int32Array, Int64Array,
    IntervalMonthDayNanoArray, LargeBinaryArray, LargeListArray, LargeStringArray, ListArray,
    MapArray, NullArray, OffsetSizeTrait, PrimitiveArray, RecordBatch, RecordBatchOptions,
    RunArray, StringArray, StringViewArray, StringViewBuilder, StructArray,
    TimestampNanosecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array, make_array,
};
use arrow::buffer::{BooleanBuffer, Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{
    ArrowDictionaryKeyType, DataType as ArrowDataType, Field as ArrowField, FieldRef, Fields,
    Int8Type, Int16Type, Int32Type, Int64Type, IntervalMonthDayNano, IntervalUnit, RunEndIndexType,
    Schema as ArrowSchema, TimeUnit, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow::error::ArrowError;
use cetra_column::{
    Batch, BinaryData, Bitmap, Column, ColumnError, DataType, DictColumn, EventsData, Field,
    FlatColumn, FlatData, Interval, LinksData, MAX_NESTING_DEPTH, MapData, RleColumn, Schema,
    Utf8Data,
};
use cetra_core::schema::{event_field, link_field};

fn invalid(reason: &'static str) -> ColumnError {
    ColumnError::InvalidBatch { reason }
}

fn overflow(reason: &'static str) -> ColumnError {
    ColumnError::Overflow { reason }
}

fn arrow_err(_err: ArrowError) -> ColumnError {
    invalid("arrow array construction failed")
}

/// Converts an Arrow `Schema` to a `cetra_column::Schema`.
///
/// Widens all 8 Arrow integer widths (`Int8..UInt64`) to `I64`, `Float32` to `F64`,
/// `LargeUtf8`/`Utf8View` to `Utf8`, `LargeBinary` to `Binary`, and recognizes
/// `spans.events` and `spans.links` `List<Struct>` shapes using
/// `cetra_core::schema::{event_field, link_field}` constants as `Events` / `Links`.
pub(crate) fn arrow_schema_to_column(schema: &ArrowSchema) -> Result<Schema, ColumnError> {
    let mut fields = Vec::with_capacity(schema.fields().len());
    for f in schema.fields() {
        let dt = arrow_type_to_column(f.data_type())?;
        fields.push(Field::new(f.name().clone(), dt, f.is_nullable()));
    }
    Schema::try_new(fields)
}

fn is_utf8_like(dt: &ArrowDataType) -> bool {
    matches!(
        dt,
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 | ArrowDataType::Utf8View
    )
}

fn is_integer_type(dt: &ArrowDataType) -> bool {
    matches!(
        dt,
        ArrowDataType::Int8
            | ArrowDataType::Int16
            | ArrowDataType::Int32
            | ArrowDataType::Int64
            | ArrowDataType::UInt8
            | ArrowDataType::UInt16
            | ArrowDataType::UInt32
            | ArrowDataType::UInt64
    )
}

fn is_events_struct(fields: &Fields) -> bool {
    fields.len() == 3
        && fields[0].name() == event_field::TIME
        && matches!(
            fields[0].data_type(),
            ArrowDataType::Timestamp(TimeUnit::Nanosecond, _)
        )
        && fields[1].name() == event_field::NAME
        && is_utf8_like(fields[1].data_type())
        && fields[2].name() == event_field::ATTRIBUTES
        && is_utf8_like(fields[2].data_type())
}

fn is_links_struct(fields: &Fields) -> bool {
    fields.len() == 3
        && fields[0].name() == link_field::TRACE_ID
        && matches!(fields[0].data_type(), ArrowDataType::FixedSizeBinary(16))
        && fields[1].name() == link_field::SPAN_ID
        && matches!(fields[1].data_type(), ArrowDataType::FixedSizeBinary(8))
        && fields[2].name() == link_field::ATTRIBUTES
        && is_utf8_like(fields[2].data_type())
}

fn arrow_type_to_column(dt: &ArrowDataType) -> Result<DataType, ColumnError> {
    match dt {
        ArrowDataType::Null => Ok(DataType::Null),
        ArrowDataType::Boolean => Ok(DataType::Bool),
        ArrowDataType::Int8
        | ArrowDataType::Int16
        | ArrowDataType::Int32
        | ArrowDataType::Int64
        | ArrowDataType::UInt8
        | ArrowDataType::UInt16
        | ArrowDataType::UInt32
        | ArrowDataType::UInt64 => Ok(DataType::I64),
        ArrowDataType::Float32 | ArrowDataType::Float64 => Ok(DataType::F64),
        ArrowDataType::Decimal128(precision, scale) => {
            let out = DataType::Decimal128 {
                precision: *precision,
                scale: *scale,
            };
            out.validate(MAX_NESTING_DEPTH)?;
            Ok(out)
        }
        ArrowDataType::Timestamp(TimeUnit::Nanosecond, tz) => {
            Ok(DataType::TimestampNs { utc: tz.is_some() })
        }
        ArrowDataType::Date32 => Ok(DataType::Date),
        ArrowDataType::Interval(IntervalUnit::MonthDayNano) => Ok(DataType::Interval),
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 | ArrowDataType::Utf8View => {
            Ok(DataType::Utf8)
        }
        ArrowDataType::Binary | ArrowDataType::LargeBinary => Ok(DataType::Binary),
        ArrowDataType::FixedSizeBinary(8) => Ok(DataType::FixedBytes8),
        ArrowDataType::FixedSizeBinary(16) => Ok(DataType::FixedBytes16),
        ArrowDataType::Dictionary(key_type, value_type) => {
            if !is_integer_type(key_type) {
                return Err(invalid("unsupported dictionary key type"));
            }
            arrow_type_to_column(value_type)
        }
        ArrowDataType::RunEndEncoded(run_ends_field, value_field) => {
            if !matches!(
                run_ends_field.data_type(),
                ArrowDataType::Int16 | ArrowDataType::Int32 | ArrowDataType::Int64
            ) {
                return Err(invalid("unsupported run-end index type"));
            }
            arrow_type_to_column(value_field.data_type())
        }
        ArrowDataType::Map(entries_field, _) => {
            let ArrowDataType::Struct(fields) = entries_field.data_type() else {
                return Err(invalid("map entries must be a struct"));
            };
            if fields.len() != 2
                || !is_utf8_like(fields[0].data_type())
                || !is_utf8_like(fields[1].data_type())
            {
                return Err(invalid("map must have utf8 key and value fields"));
            }
            Ok(DataType::MapUtf8)
        }
        ArrowDataType::List(item) | ArrowDataType::LargeList(item) => {
            if let ArrowDataType::Struct(fields) = item.data_type() {
                if is_events_struct(fields) {
                    return Ok(DataType::Events);
                }
                if is_links_struct(fields) {
                    return Ok(DataType::Links);
                }
            }
            Ok(DataType::List(Box::new(arrow_type_to_column(
                item.data_type(),
            )?)))
        }
        _ => Err(invalid("unsupported arrow data type")),
    }
}

/// Converts a `cetra_column::Schema` to an Arrow `Schema` using default Arrow types:
/// - `TimestampNs { utc: true }` → `Timestamp(Nanosecond, Some("UTC".into()))`
/// - `TimestampNs { utc: false }` → `Timestamp(Nanosecond, None)`
/// - `List(inner)` → `List(Field::new("item", inner, true))`
/// - `MapUtf8` → `Map(Struct(keys: Utf8 non-null, values: Utf8 nullable), false)`
/// - `Events` / `Links` → matching `spans` `List<Struct>` layout using `event_field`/`link_field`.
pub(crate) fn column_schema_to_arrow(schema: &Schema) -> ArrowSchema {
    let fields: Vec<ArrowField> = schema
        .fields()
        .iter()
        .map(|f| ArrowField::new(&f.name, column_type_to_arrow(&f.data_type), f.nullable))
        .collect();
    ArrowSchema::new(fields)
}

fn column_type_to_arrow(dt: &DataType) -> ArrowDataType {
    match dt {
        DataType::Null => ArrowDataType::Null,
        DataType::Bool => ArrowDataType::Boolean,
        DataType::I64 => ArrowDataType::Int64,
        DataType::F64 => ArrowDataType::Float64,
        DataType::Decimal128 { precision, scale } => ArrowDataType::Decimal128(*precision, *scale),
        DataType::TimestampNs { utc } => {
            let tz = if *utc { Some("UTC".into()) } else { None };
            ArrowDataType::Timestamp(TimeUnit::Nanosecond, tz)
        }
        DataType::Date => ArrowDataType::Date32,
        DataType::Interval => ArrowDataType::Interval(IntervalUnit::MonthDayNano),
        DataType::Utf8 => ArrowDataType::Utf8,
        DataType::Binary => ArrowDataType::Binary,
        DataType::FixedBytes8 => ArrowDataType::FixedSizeBinary(8),
        DataType::FixedBytes16 => ArrowDataType::FixedSizeBinary(16),
        DataType::List(inner) => ArrowDataType::List(Arc::new(ArrowField::new(
            "item",
            column_type_to_arrow(inner),
            true,
        ))),
        DataType::MapUtf8 => {
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
        DataType::Events => {
            let fields = Fields::from(vec![
                ArrowField::new(
                    event_field::TIME,
                    ArrowDataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                    false,
                ),
                ArrowField::new(event_field::NAME, ArrowDataType::Utf8, false),
                ArrowField::new(event_field::ATTRIBUTES, ArrowDataType::Utf8, true),
            ]);
            ArrowDataType::List(Arc::new(ArrowField::new(
                "item",
                ArrowDataType::Struct(fields),
                false,
            )))
        }
        DataType::Links => {
            let fields = Fields::from(vec![
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
            ]);
            ArrowDataType::List(Arc::new(ArrowField::new(
                "item",
                ArrowDataType::Struct(fields),
                false,
            )))
        }
    }
}

/// Converts an Arrow `RecordBatch` into a `cetra_column::Batch` (`Selection::All`),
/// preserving Arrow `DictionaryArray` (all 8 integer key types) as `Column::Dict` and
/// Arrow `RunArray` (all 3 run-end types `Int16`/`Int32`/`Int64`) as `Column::Rle`.
pub(crate) fn record_batch_to_column(batch: &RecordBatch) -> Result<Batch, ColumnError> {
    let raw_len = batch.num_rows();
    if raw_len > u32::MAX as usize {
        return Err(overflow("batch row count exceeds u32::MAX"));
    }
    let schema = Arc::new(arrow_schema_to_column(batch.schema_ref())?);
    let mut columns = Vec::with_capacity(schema.len());
    for (field, array) in schema.fields().iter().zip(batch.columns()) {
        columns.push(arrow_array_to_column(array.as_ref(), &field.data_type)?);
    }
    Batch::try_new_all(schema, columns, raw_len)
}

fn arrow_nulls_to_bitmap(array: &dyn Array) -> Option<Bitmap> {
    if array.null_count() == 0 {
        return None;
    }
    let len = array.len();
    let mut bm = Bitmap::new_ones(len);
    for i in 0..len {
        if !array.is_valid(i) {
            let _ = bm.set(i, false);
        }
    }
    if bm.all_ones() { None } else { Some(bm) }
}

enum StringAccessor<'a> {
    Utf8(&'a StringArray),
    LargeUtf8(&'a LargeStringArray),
    Utf8View(&'a StringViewArray),
}

impl<'a> StringAccessor<'a> {
    fn from_array(array: &'a dyn Array) -> Result<Self, ColumnError> {
        match array.data_type() {
            ArrowDataType::Utf8 => array
                .as_any()
                .downcast_ref::<StringArray>()
                .map(Self::Utf8)
                .ok_or_else(|| invalid("expected StringArray")),
            ArrowDataType::LargeUtf8 => array
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .map(Self::LargeUtf8)
                .ok_or_else(|| invalid("expected LargeStringArray")),
            ArrowDataType::Utf8View => array
                .as_any()
                .downcast_ref::<StringViewArray>()
                .map(Self::Utf8View)
                .ok_or_else(|| invalid("expected StringViewArray")),
            _ => Err(invalid("expected UTF-8 string array")),
        }
    }

    fn is_valid(&self, row: usize) -> bool {
        match self {
            Self::Utf8(a) => a.is_valid(row),
            Self::LargeUtf8(a) => a.is_valid(row),
            Self::Utf8View(a) => a.is_valid(row),
        }
    }

    fn value(&self, row: usize) -> &str {
        match self {
            Self::Utf8(a) => a.value(row),
            Self::LargeUtf8(a) => a.value(row),
            Self::Utf8View(a) => a.value(row),
        }
    }
}

fn arrow_array_to_column(array: &dyn Array, dt: &DataType) -> Result<Column, ColumnError> {
    match array.data_type() {
        ArrowDataType::Dictionary(key_type, _) => match key_type.as_ref() {
            ArrowDataType::Int8 => dict_to_column::<Int8Type>(array, dt),
            ArrowDataType::Int16 => dict_to_column::<Int16Type>(array, dt),
            ArrowDataType::Int32 => dict_to_column::<Int32Type>(array, dt),
            ArrowDataType::Int64 => dict_to_column::<Int64Type>(array, dt),
            ArrowDataType::UInt8 => dict_to_column::<UInt8Type>(array, dt),
            ArrowDataType::UInt16 => dict_to_column::<UInt16Type>(array, dt),
            ArrowDataType::UInt32 => dict_to_column::<UInt32Type>(array, dt),
            ArrowDataType::UInt64 => dict_to_column::<UInt64Type>(array, dt),
            _ => Err(invalid("unsupported dictionary key type")),
        },
        ArrowDataType::RunEndEncoded(run_ends_field, _) => match run_ends_field.data_type() {
            ArrowDataType::Int16 => run_to_column::<Int16Type>(array, dt),
            ArrowDataType::Int32 => run_to_column::<Int32Type>(array, dt),
            ArrowDataType::Int64 => run_to_column::<Int64Type>(array, dt),
            _ => Err(invalid("unsupported run-end index type")),
        },
        _ => Ok(Column::Flat(arrow_array_to_flat(array, dt)?)),
    }
}

trait DictKeyInt: ArrowDictionaryKeyType {
    fn key_to_u32(v: Self::Native) -> Result<u32, ColumnError>;
    fn u32_to_key(v: u32) -> Result<Self::Native, ColumnError>;
}

macro_rules! impl_signed_dict_key {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl DictKeyInt for $ty {
                fn key_to_u32(v: Self::Native) -> Result<u32, ColumnError> {
                    if v < 0 {
                        return Err(invalid("negative dictionary key"));
                    }
                    u32::try_from(v).map_err(|_| overflow("dictionary key exceeds u32::MAX"))
                }

                fn u32_to_key(v: u32) -> Result<Self::Native, ColumnError> {
                    Self::Native::try_from(v)
                        .map_err(|_| overflow("dictionary code exceeds target key type"))
                }
            }
        )+
    };
}

macro_rules! impl_unsigned_dict_key {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl DictKeyInt for $ty {
                fn key_to_u32(v: Self::Native) -> Result<u32, ColumnError> {
                    u32::try_from(v).map_err(|_| overflow("dictionary key exceeds u32::MAX"))
                }

                fn u32_to_key(v: u32) -> Result<Self::Native, ColumnError> {
                    Self::Native::try_from(v)
                        .map_err(|_| overflow("dictionary code exceeds target key type"))
                }
            }
        )+
    };
}

impl_signed_dict_key!(Int8Type, Int16Type, Int32Type, Int64Type);
impl_unsigned_dict_key!(UInt8Type, UInt16Type, UInt32Type, UInt64Type);

fn dict_to_column<K: DictKeyInt>(array: &dyn Array, dt: &DataType) -> Result<Column, ColumnError> {
    let dict = array
        .as_any()
        .downcast_ref::<DictionaryArray<K>>()
        .ok_or_else(|| invalid("expected DictionaryArray"))?;
    let values = arrow_array_to_column(dict.values().as_ref(), dt)?.materialize(None)?;
    let keys = dict.keys();
    let len = dict.len();
    let mut codes = Vec::with_capacity(len);
    let mut nulls = Bitmap::new_ones(len);
    for i in 0..len {
        if dict.is_valid(i) {
            codes.push(K::key_to_u32(keys.value(i))?);
        } else {
            let _ = nulls.set(i, false);
            codes.push(0);
        }
    }
    Ok(Column::Dict(DictColumn::try_new(
        values,
        codes,
        Some(nulls),
    )?))
}

trait RunEndInt: RunEndIndexType {
    fn end_to_usize(v: Self::Native) -> Result<usize, ColumnError>;
    fn usize_to_end(v: usize) -> Result<Self::Native, ColumnError>;
}

macro_rules! impl_run_end_int {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl RunEndInt for $ty {
                fn end_to_usize(v: Self::Native) -> Result<usize, ColumnError> {
                    if v <= 0 {
                        return Err(invalid("run end must be positive"));
                    }
                    usize::try_from(v).map_err(|_| overflow("run end out of range"))
                }

                fn usize_to_end(v: usize) -> Result<Self::Native, ColumnError> {
                    Self::Native::try_from(v)
                        .map_err(|_| overflow("run end exceeds target run-end type"))
                }
            }
        )+
    };
}

impl_run_end_int!(Int16Type, Int32Type, Int64Type);

fn run_to_column<R: RunEndInt>(array: &dyn Array, dt: &DataType) -> Result<Column, ColumnError> {
    let run = array
        .as_any()
        .downcast_ref::<RunArray<R>>()
        .ok_or_else(|| invalid("expected RunArray"))?;
    // Guard empty/sliced-empty RunArray before calling get_start_physical_index/get_end_physical_index.
    if run.is_empty() {
        return Ok(Column::Rle(RleColumn::try_new(
            FlatColumn::empty(dt),
            Vec::new(),
        )?));
    }
    let start_phys = run.get_start_physical_index();
    let end_phys = run.get_end_physical_index();
    let values_slice = run.values().slice(start_phys, end_phys - start_phys + 1);
    let values_flat = arrow_array_to_column(values_slice.as_ref(), dt)?.materialize(None)?;

    let offset = run.run_ends().offset();
    let end_row = offset
        .checked_add(run.len())
        .ok_or_else(|| overflow("run array end row overflow"))?;
    let raw_ends = &run.run_ends().values()[start_phys..=end_phys];
    let mut run_ends = Vec::with_capacity(raw_ends.len());
    for &raw_end in raw_ends {
        let abs_end = R::end_to_usize(raw_end)?;
        let clamped = abs_end.min(end_row);
        let rebased = clamped
            .checked_sub(offset)
            .ok_or_else(|| invalid("run end before slice offset"))?;
        if rebased == 0 {
            return Err(invalid("rebased run end must be positive"));
        }
        run_ends.push(u32::try_from(rebased).map_err(|_| overflow("run end exceeds u32::MAX"))?);
    }
    Ok(Column::Rle(RleColumn::try_new(values_flat, run_ends)?))
}

fn widen_int_array<T>(arr: &PrimitiveArray<T>) -> Result<FlatColumn, ColumnError>
where
    T: ArrowPrimitiveType,
    i64: TryFrom<T::Native>,
{
    let len = arr.len();
    let nulls = arrow_nulls_to_bitmap(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if arr.is_valid(i) {
            let v = i64::try_from(arr.value(i))
                .map_err(|_| overflow("integer value exceeds i64::MAX"))?;
            out.push(v);
        } else {
            out.push(0);
        }
    }
    FlatColumn::try_new(FlatData::I64(out), nulls)
}

fn checked_offset_usize<O: OffsetSizeTrait>(o: O) -> Result<usize, ColumnError> {
    o.to_isize()
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| invalid("negative Arrow offset"))
}

fn rebase_offsets<O: OffsetSizeTrait>(
    offsets: &[O],
    data_len: usize,
) -> Result<(usize, usize, Vec<u32>), ColumnError> {
    let base = checked_offset_usize(offsets[0])?;
    let end = checked_offset_usize(offsets[offsets.len() - 1])?;
    if end < base || end > data_len {
        return Err(invalid("Arrow offsets out of bounds"));
    }
    let mut out = Vec::with_capacity(offsets.len());
    let mut prev = 0u32;
    for &o in offsets {
        let pos = checked_offset_usize(o)?;
        let rel = pos
            .checked_sub(base)
            .ok_or_else(|| invalid("non-monotonic Arrow offsets"))?;
        let rel_u32 = u32::try_from(rel)
            .map_err(|_| overflow("rebased byte/list length exceeds u32::MAX"))?;
        if rel_u32 < prev {
            return Err(invalid("non-monotonic Arrow offsets"));
        }
        prev = rel_u32;
        out.push(rel_u32);
    }
    Ok((base, end, out))
}

fn generic_string_to_utf8<O: OffsetSizeTrait>(
    arr: &GenericStringArray<O>,
) -> Result<FlatColumn, ColumnError> {
    let len = arr.len();
    if len == 0 {
        return Ok(FlatColumn::empty(&DataType::Utf8));
    }
    let nulls = arrow_nulls_to_bitmap(arr);
    let utf8 = if arr.null_count() == 0 {
        let (start, end, offsets) = rebase_offsets(arr.value_offsets(), arr.value_data().len())?;
        let bytes = arr.value_data()[start..end].to_vec();
        Utf8Data::try_new(offsets, bytes)?
    } else {
        Utf8Data::try_from_slices(
            (0..len).map(|i| if arr.is_valid(i) { arr.value(i) } else { "" }),
        )?
    };
    FlatColumn::try_new(FlatData::Utf8(utf8), nulls)
}

fn generic_binary_to_binary<O: OffsetSizeTrait>(
    arr: &GenericBinaryArray<O>,
) -> Result<FlatColumn, ColumnError> {
    let len = arr.len();
    if len == 0 {
        return Ok(FlatColumn::empty(&DataType::Binary));
    }
    let nulls = arrow_nulls_to_bitmap(arr);
    let bin = if arr.null_count() == 0 {
        let (start, end, offsets) = rebase_offsets(arr.value_offsets(), arr.value_data().len())?;
        let bytes = arr.value_data()[start..end].to_vec();
        BinaryData::try_new(offsets, bytes)?
    } else {
        BinaryData::try_from_slices(
            (0..len).map(|i| if arr.is_valid(i) { arr.value(i) } else { &[] }),
        )?
    };
    FlatColumn::try_new(FlatData::Binary(bin), nulls)
}

fn generic_list_to_flat<O: OffsetSizeTrait>(
    list: &GenericListArray<O>,
    inner_dt: &DataType,
) -> Result<FlatColumn, ColumnError> {
    let len = list.len();
    if len == 0 {
        return Ok(FlatColumn::empty(&DataType::List(Box::new(
            inner_dt.clone(),
        ))));
    }
    let raw_offsets = list.value_offsets();
    let nulls = arrow_nulls_to_bitmap(list);

    if list.null_count() == 0 {
        let (start, end, offsets) = rebase_offsets(raw_offsets, list.values().len())?;
        let child_slice = list.values().slice(start, end - start);
        let child_col = arrow_array_to_column(child_slice.as_ref(), inner_dt)?;
        return FlatColumn::try_new(
            FlatData::List {
                item_type: inner_dt.clone(),
                offsets,
                values: Box::new(child_col),
            },
            None,
        );
    }

    // Build row-by-row skipping null parent rows so backing items behind null list slots are never copied.
    let mut out_offsets = Vec::with_capacity(len + 1);
    out_offsets.push(0u32);
    let mut active_ranges: Vec<(usize, usize)> = Vec::new();
    let mut total_child = 0usize;

    for row in 0..len {
        if list.is_valid(row) {
            let s = checked_offset_usize(raw_offsets[row])?;
            let e = checked_offset_usize(raw_offsets[row + 1])?;
            if e < s || e > list.values().len() {
                return Err(invalid("Arrow list offsets out of bounds"));
            }
            let count = e - s;
            total_child = total_child
                .checked_add(count)
                .ok_or_else(|| overflow("list child length overflow"))?;
            if count > 0 {
                if let Some(last) = active_ranges.last_mut()
                    && last.1 == s
                {
                    last.1 = e;
                } else {
                    active_ranges.push((s, e));
                }
            }
        }
        let end_u32 = u32::try_from(total_child)
            .map_err(|_| overflow("list child length exceeds u32::MAX"))?;
        out_offsets.push(end_u32);
    }

    let child_col = if active_ranges.is_empty() {
        Column::Flat(FlatColumn::empty(inner_dt))
    } else if active_ranges.len() == 1 {
        let (s, e) = active_ranges[0];
        let sliced = list.values().slice(s, e - s);
        arrow_array_to_column(sliced.as_ref(), inner_dt)?
    } else {
        let single_schema = Arc::new(Schema::try_new(vec![Field::new(
            "item",
            inner_dt.clone(),
            true,
        )])?);
        let mut batches = Vec::with_capacity(active_ranges.len());
        for (s, e) in active_ranges {
            let sliced = list.values().slice(s, e - s);
            let col = arrow_array_to_column(sliced.as_ref(), inner_dt)?;
            batches.push(Batch::try_new_all(
                Arc::clone(&single_schema),
                vec![col],
                e - s,
            )?);
        }
        let refs: Vec<&Batch> = batches.iter().collect();
        let merged = Batch::concat(single_schema, &refs, None)?;
        merged
            .columns()
            .first()
            .cloned()
            .ok_or_else(|| invalid("missing merged list column"))?
    };

    FlatColumn::try_new(
        FlatData::List {
            item_type: inner_dt.clone(),
            offsets: out_offsets,
            values: Box::new(child_col),
        },
        nulls,
    )
}

fn map_array_to_flat(map: &MapArray) -> Result<FlatColumn, ColumnError> {
    let len = map.len();
    let offsets = map.value_offsets();
    let keys = StringAccessor::from_array(map.keys().as_ref())?;
    let values = StringAccessor::from_array(map.values().as_ref())?;
    let child_len = map.keys().len();
    if map.values().len() != child_len {
        return Err(invalid("Arrow map keys/values length mismatch"));
    }

    let mut out_offsets = Vec::with_capacity(len + 1);
    out_offsets.push(0u32);
    let mut key_slices: Vec<&str> = Vec::new();
    let mut val_slices: Vec<&str> = Vec::new();
    let mut val_valid: Vec<bool> = Vec::new();
    let mut parent_valid = Bitmap::new_ones(len);

    for row in 0..len {
        if !map.is_valid(row) {
            let _ = parent_valid.set(row, false);
            let cur = u32::try_from(key_slices.len())
                .map_err(|_| overflow("map entry count exceeds u32::MAX"))?;
            out_offsets.push(cur);
            continue;
        }
        let s = checked_offset_usize(offsets[row])?;
        let e = checked_offset_usize(offsets[row + 1])?;
        if e < s || e > child_len {
            return Err(invalid("Arrow map offsets out of bounds"));
        }
        for idx in s..e {
            if !map.entries().is_valid(idx) || !keys.is_valid(idx) {
                return Err(invalid("map key cannot be null"));
            }
            key_slices.push(keys.value(idx));
            if values.is_valid(idx) {
                val_slices.push(values.value(idx));
                val_valid.push(true);
            } else {
                val_slices.push("");
                val_valid.push(false);
            }
        }
        let cur = u32::try_from(key_slices.len())
            .map_err(|_| overflow("map entry count exceeds u32::MAX"))?;
        out_offsets.push(cur);
    }

    let keys_data = Utf8Data::try_from_slices(key_slices)?;
    let vals_data = Utf8Data::try_from_slices(val_slices)?;
    let val_nulls = Bitmap::from_bools(&val_valid);
    let map_data = MapData::try_new(out_offsets, keys_data, vals_data, Some(val_nulls))?;
    FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), Some(parent_valid))
}

fn events_list_to_flat<O: OffsetSizeTrait>(
    list: &GenericListArray<O>,
) -> Result<FlatColumn, ColumnError> {
    let len = list.len();
    let offsets = list.value_offsets();
    let struct_arr = list
        .values()
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| invalid("events child must be StructArray"))?;
    if struct_arr.num_columns() != 3 {
        return Err(invalid("events struct must have 3 fields"));
    }
    let time_arr = struct_arr
        .column(0)
        .as_any()
        .downcast_ref::<TimestampNanosecondArray>()
        .ok_or_else(|| invalid("events time field must be TimestampNanosecondArray"))?;
    let name_arr = StringAccessor::from_array(struct_arr.column(1).as_ref())?;
    let attr_arr = StringAccessor::from_array(struct_arr.column(2).as_ref())?;

    let mut out_offsets = Vec::with_capacity(len + 1);
    out_offsets.push(0u32);
    let mut times = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    let mut attrs: Vec<&str> = Vec::new();
    let mut attr_valid: Vec<bool> = Vec::new();
    let mut parent_valid = Bitmap::new_ones(len);

    for row in 0..len {
        if !list.is_valid(row) {
            let _ = parent_valid.set(row, false);
            let cur = u32::try_from(times.len())
                .map_err(|_| overflow("events count exceeds u32::MAX"))?;
            out_offsets.push(cur);
            continue;
        }
        let s = checked_offset_usize(offsets[row])?;
        let e = checked_offset_usize(offsets[row + 1])?;
        if e < s || e > struct_arr.len() {
            return Err(invalid("Arrow events offsets out of bounds"));
        }
        for idx in s..e {
            if !struct_arr.is_valid(idx) {
                return Err(invalid("event struct item cannot be null"));
            }
            if !time_arr.is_valid(idx) {
                return Err(invalid("event time cannot be null"));
            }
            if !name_arr.is_valid(idx) {
                return Err(invalid("event name cannot be null"));
            }
            times.push(time_arr.value(idx));
            names.push(name_arr.value(idx));
            if attr_arr.is_valid(idx) {
                attrs.push(attr_arr.value(idx));
                attr_valid.push(true);
            } else {
                attrs.push("");
                attr_valid.push(false);
            }
        }
        let cur =
            u32::try_from(times.len()).map_err(|_| overflow("events count exceeds u32::MAX"))?;
        out_offsets.push(cur);
    }

    let names_data = Utf8Data::try_from_slices(names)?;
    let attrs_data = Utf8Data::try_from_slices(attrs)?;
    let attr_nulls = Bitmap::from_bools(&attr_valid);
    let events_data =
        EventsData::try_new(out_offsets, times, names_data, attrs_data, Some(attr_nulls))?;
    FlatColumn::try_new(FlatData::Events(Box::new(events_data)), Some(parent_valid))
}

fn links_list_to_flat<O: OffsetSizeTrait>(
    list: &GenericListArray<O>,
) -> Result<FlatColumn, ColumnError> {
    let len = list.len();
    let offsets = list.value_offsets();
    let struct_arr = list
        .values()
        .as_any()
        .downcast_ref::<StructArray>()
        .ok_or_else(|| invalid("links child must be StructArray"))?;
    if struct_arr.num_columns() != 3 {
        return Err(invalid("links struct must have 3 fields"));
    }
    let trace_id_arr = struct_arr
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .ok_or_else(|| invalid("links trace_id field must be FixedSizeBinaryArray"))?;
    let span_id_arr = struct_arr
        .column(1)
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .ok_or_else(|| invalid("links span_id field must be FixedSizeBinaryArray"))?;
    if trace_id_arr.value_length() != 16 || span_id_arr.value_length() != 8 {
        return Err(invalid("links id width mismatch"));
    }
    let attr_arr = StringAccessor::from_array(struct_arr.column(2).as_ref())?;

    let mut out_offsets = Vec::with_capacity(len + 1);
    out_offsets.push(0u32);
    let mut trace_ids = Vec::new();
    let mut span_ids = Vec::new();
    let mut attrs: Vec<&str> = Vec::new();
    let mut attr_valid: Vec<bool> = Vec::new();
    let mut parent_valid = Bitmap::new_ones(len);

    for row in 0..len {
        if !list.is_valid(row) {
            let _ = parent_valid.set(row, false);
            let cur = u32::try_from(trace_ids.len())
                .map_err(|_| overflow("links count exceeds u32::MAX"))?;
            out_offsets.push(cur);
            continue;
        }
        let s = checked_offset_usize(offsets[row])?;
        let e = checked_offset_usize(offsets[row + 1])?;
        if e < s || e > struct_arr.len() {
            return Err(invalid("Arrow links offsets out of bounds"));
        }
        for idx in s..e {
            if !struct_arr.is_valid(idx) {
                return Err(invalid("link struct item cannot be null"));
            }
            if !trace_id_arr.is_valid(idx) {
                return Err(invalid("link trace_id cannot be null"));
            }
            if !span_id_arr.is_valid(idx) {
                return Err(invalid("link span_id cannot be null"));
            }
            let tid = <[u8; 16]>::try_from(trace_id_arr.value(idx))
                .map_err(|_| invalid("link trace_id must be 16 bytes"))?;
            let sid = <[u8; 8]>::try_from(span_id_arr.value(idx))
                .map_err(|_| invalid("link span_id must be 8 bytes"))?;
            trace_ids.push(tid);
            span_ids.push(sid);
            if attr_arr.is_valid(idx) {
                attrs.push(attr_arr.value(idx));
                attr_valid.push(true);
            } else {
                attrs.push("");
                attr_valid.push(false);
            }
        }
        let cur =
            u32::try_from(trace_ids.len()).map_err(|_| overflow("links count exceeds u32::MAX"))?;
        out_offsets.push(cur);
    }

    let attrs_data = Utf8Data::try_from_slices(attrs)?;
    let attr_nulls = Bitmap::from_bools(&attr_valid);
    let links_data = LinksData::try_new(
        out_offsets,
        trace_ids,
        span_ids,
        attrs_data,
        Some(attr_nulls),
    )?;
    FlatColumn::try_new(FlatData::Links(Box::new(links_data)), Some(parent_valid))
}

fn fixed_bytes_8(arr: &FixedSizeBinaryArray) -> Result<FlatColumn, ColumnError> {
    if arr.value_length() != 8 {
        return Err(invalid("expected FixedSizeBinary(8)"));
    }
    let len = arr.len();
    let nulls = arrow_nulls_to_bitmap(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if arr.is_valid(i) {
            let b =
                <[u8; 8]>::try_from(arr.value(i)).map_err(|_| invalid("expected 8-byte slice"))?;
            out.push(b);
        } else {
            out.push([0u8; 8]);
        }
    }
    FlatColumn::try_new(FlatData::FixedBytes8(out), nulls)
}

fn fixed_bytes_16(arr: &FixedSizeBinaryArray) -> Result<FlatColumn, ColumnError> {
    if arr.value_length() != 16 {
        return Err(invalid("expected FixedSizeBinary(16)"));
    }
    let len = arr.len();
    let nulls = arrow_nulls_to_bitmap(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        if arr.is_valid(i) {
            let b = <[u8; 16]>::try_from(arr.value(i))
                .map_err(|_| invalid("expected 16-byte slice"))?;
            out.push(b);
        } else {
            out.push([0u8; 16]);
        }
    }
    FlatColumn::try_new(FlatData::FixedBytes16(out), nulls)
}

fn arrow_array_to_flat(array: &dyn Array, dt: &DataType) -> Result<FlatColumn, ColumnError> {
    let len = array.len();
    match (dt, array.data_type()) {
        (DataType::Null, ArrowDataType::Null) => {
            let _ = array
                .as_any()
                .downcast_ref::<NullArray>()
                .ok_or_else(|| invalid("expected NullArray"))?;
            FlatColumn::try_new(FlatData::Null(len), None)
        }
        (DataType::Bool, ArrowDataType::Boolean) => {
            let arr = array
                .as_any()
                .downcast_ref::<BooleanArray>()
                .ok_or_else(|| invalid("expected BooleanArray"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let mut bm = Bitmap::new_zeroed(len);
            for i in 0..len {
                if arr.is_valid(i) && arr.value(i) {
                    let _ = bm.set(i, true);
                }
            }
            FlatColumn::try_new(FlatData::Bool(bm), nulls)
        }
        (DataType::I64, ArrowDataType::Int8) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<Int8Array>()
                .ok_or_else(|| invalid("expected Int8Array"))?,
        ),
        (DataType::I64, ArrowDataType::Int16) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<Int16Array>()
                .ok_or_else(|| invalid("expected Int16Array"))?,
        ),
        (DataType::I64, ArrowDataType::Int32) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<Int32Array>()
                .ok_or_else(|| invalid("expected Int32Array"))?,
        ),
        (DataType::I64, ArrowDataType::Int64) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| invalid("expected Int64Array"))?,
        ),
        (DataType::I64, ArrowDataType::UInt8) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<UInt8Array>()
                .ok_or_else(|| invalid("expected UInt8Array"))?,
        ),
        (DataType::I64, ArrowDataType::UInt16) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<UInt16Array>()
                .ok_or_else(|| invalid("expected UInt16Array"))?,
        ),
        (DataType::I64, ArrowDataType::UInt32) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<UInt32Array>()
                .ok_or_else(|| invalid("expected UInt32Array"))?,
        ),
        (DataType::I64, ArrowDataType::UInt64) => widen_int_array(
            array
                .as_any()
                .downcast_ref::<UInt64Array>()
                .ok_or_else(|| invalid("expected UInt64Array"))?,
        ),
        (DataType::F64, ArrowDataType::Float32) => {
            let arr = array
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| invalid("expected Float32Array"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let out = (0..len)
                .map(|i| {
                    if arr.is_valid(i) {
                        f64::from(arr.value(i))
                    } else {
                        0.0
                    }
                })
                .collect();
            FlatColumn::try_new(FlatData::F64(out), nulls)
        }
        (DataType::F64, ArrowDataType::Float64) => {
            let arr = array
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| invalid("expected Float64Array"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let out = (0..len)
                .map(|i| if arr.is_valid(i) { arr.value(i) } else { 0.0 })
                .collect();
            FlatColumn::try_new(FlatData::F64(out), nulls)
        }
        (DataType::Decimal128 { precision, scale }, ArrowDataType::Decimal128(ap, as_)) => {
            if precision != ap || scale != as_ {
                return Err(invalid("decimal precision/scale mismatch"));
            }
            let arr = array
                .as_any()
                .downcast_ref::<Decimal128Array>()
                .ok_or_else(|| invalid("expected Decimal128Array"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let values = (0..len)
                .map(|i| if arr.is_valid(i) { arr.value(i) } else { 0 })
                .collect();
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values,
                    precision: *precision,
                    scale: *scale,
                },
                nulls,
            )
        }
        (DataType::TimestampNs { utc }, ArrowDataType::Timestamp(TimeUnit::Nanosecond, tz)) => {
            if *utc != tz.is_some() {
                return Err(invalid("timestamp utc flag mismatch"));
            }
            let arr = array
                .as_any()
                .downcast_ref::<TimestampNanosecondArray>()
                .ok_or_else(|| invalid("expected TimestampNanosecondArray"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let values = (0..len)
                .map(|i| if arr.is_valid(i) { arr.value(i) } else { 0 })
                .collect();
            FlatColumn::try_new(FlatData::TimestampNs { values, utc: *utc }, nulls)
        }
        (DataType::Date, ArrowDataType::Date32) => {
            let arr = array
                .as_any()
                .downcast_ref::<Date32Array>()
                .ok_or_else(|| invalid("expected Date32Array"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let values = (0..len)
                .map(|i| if arr.is_valid(i) { arr.value(i) } else { 0 })
                .collect();
            FlatColumn::try_new(FlatData::Date(values), nulls)
        }
        (DataType::Interval, ArrowDataType::Interval(IntervalUnit::MonthDayNano)) => {
            let arr = array
                .as_any()
                .downcast_ref::<IntervalMonthDayNanoArray>()
                .ok_or_else(|| invalid("expected IntervalMonthDayNanoArray"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let values = (0..len)
                .map(|i| {
                    if arr.is_valid(i) {
                        let v = arr.value(i);
                        Interval {
                            months: v.months,
                            days: v.days,
                            nanos: v.nanoseconds,
                        }
                    } else {
                        Interval::default()
                    }
                })
                .collect();
            FlatColumn::try_new(FlatData::Interval(values), nulls)
        }
        (DataType::Utf8, ArrowDataType::Utf8) => {
            let arr = array
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| invalid("expected StringArray"))?;
            generic_string_to_utf8(arr)
        }
        (DataType::Utf8, ArrowDataType::LargeUtf8) => {
            let arr = array
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .ok_or_else(|| invalid("expected LargeStringArray"))?;
            generic_string_to_utf8(arr)
        }
        (DataType::Utf8, ArrowDataType::Utf8View) => {
            let arr = array
                .as_any()
                .downcast_ref::<StringViewArray>()
                .ok_or_else(|| invalid("expected StringViewArray"))?;
            let nulls = arrow_nulls_to_bitmap(arr);
            let utf8 = Utf8Data::try_from_slices(
                (0..len).map(|i| if arr.is_valid(i) { arr.value(i) } else { "" }),
            )?;
            FlatColumn::try_new(FlatData::Utf8(utf8), nulls)
        }
        (DataType::Binary, ArrowDataType::Binary) => {
            let arr = array
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(|| invalid("expected BinaryArray"))?;
            generic_binary_to_binary(arr)
        }
        (DataType::Binary, ArrowDataType::LargeBinary) => {
            let arr = array
                .as_any()
                .downcast_ref::<LargeBinaryArray>()
                .ok_or_else(|| invalid("expected LargeBinaryArray"))?;
            generic_binary_to_binary(arr)
        }
        (DataType::FixedBytes8, ArrowDataType::FixedSizeBinary(8)) => {
            let arr = array
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .ok_or_else(|| invalid("expected FixedSizeBinaryArray"))?;
            fixed_bytes_8(arr)
        }
        (DataType::FixedBytes16, ArrowDataType::FixedSizeBinary(16)) => {
            let arr = array
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .ok_or_else(|| invalid("expected FixedSizeBinaryArray"))?;
            fixed_bytes_16(arr)
        }
        (DataType::List(inner), ArrowDataType::List(_)) => {
            let list = array
                .as_any()
                .downcast_ref::<ListArray>()
                .ok_or_else(|| invalid("expected ListArray"))?;
            generic_list_to_flat(list, inner)
        }
        (DataType::List(inner), ArrowDataType::LargeList(_)) => {
            let list = array
                .as_any()
                .downcast_ref::<LargeListArray>()
                .ok_or_else(|| invalid("expected LargeListArray"))?;
            generic_list_to_flat(list, inner)
        }
        (DataType::MapUtf8, ArrowDataType::Map(_, _)) => {
            let map = array
                .as_any()
                .downcast_ref::<MapArray>()
                .ok_or_else(|| invalid("expected MapArray"))?;
            map_array_to_flat(map)
        }
        (DataType::Events, ArrowDataType::List(_)) => {
            let list = array
                .as_any()
                .downcast_ref::<ListArray>()
                .ok_or_else(|| invalid("expected ListArray for Events"))?;
            events_list_to_flat(list)
        }
        (DataType::Events, ArrowDataType::LargeList(_)) => {
            let list = array
                .as_any()
                .downcast_ref::<LargeListArray>()
                .ok_or_else(|| invalid("expected LargeListArray for Events"))?;
            events_list_to_flat(list)
        }
        (DataType::Links, ArrowDataType::List(_)) => {
            let list = array
                .as_any()
                .downcast_ref::<ListArray>()
                .ok_or_else(|| invalid("expected ListArray for Links"))?;
            links_list_to_flat(list)
        }
        (DataType::Links, ArrowDataType::LargeList(_)) => {
            let list = array
                .as_any()
                .downcast_ref::<LargeListArray>()
                .ok_or_else(|| invalid("expected LargeListArray for Links"))?;
            links_list_to_flat(list)
        }
        _ => Err(invalid("column data type does not match Arrow array")),
    }
}

/// Converts a `cetra_column::Batch` (compacting active rows first) into an Arrow `RecordBatch`.
///
/// Encoding rules:
/// - When `target_schema` is `None`: uses `column_schema_to_arrow(batch.schema())` and
///   materializes every `Column` to flat Arrow arrays so the batch schema always matches.
/// - When `target_schema` is `Some(target)`: preserves exact target field names, timezones,
///   list/map nested field names and nullability; narrows `I64` to any of the 8 Arrow integer
///   types with checked `try_from` (returning `ColumnError::Overflow` on range failure);
///   emits Arrow `DictionaryArray` / `RunArray` when the target field's Arrow type is
///   `Dictionary` / `RunEndEncoded` (wrapping compacted `Flat` or `Const` into a trivial
///   1-to-1 `DictionaryArray` or 1-run/1-row-per-run `RunArray` as needed), and materializes
///   to flat Arrow arrays otherwise.
/// - Handles 0-column non-empty batches via `RecordBatch::try_new_with_options`.
pub(crate) fn column_to_record_batch(
    batch: &Batch,
    target_schema: Option<&ArrowSchema>,
) -> Result<RecordBatch, ColumnError> {
    let compacted = batch.compact(None)?;
    let out_schema = match target_schema {
        Some(target) => {
            if target.fields().len() != compacted.columns().len() {
                return Err(invalid("target schema column count mismatch"));
            }
            Arc::new(target.clone())
        }
        None => Arc::new(column_schema_to_arrow(compacted.schema())),
    };

    let mut arrays = Vec::with_capacity(compacted.columns().len());
    for (col, field) in compacted.columns().iter().zip(out_schema.fields()) {
        arrays.push(column_to_arrow_array(col, field.data_type())?);
    }

    let options = RecordBatchOptions::new().with_row_count(Some(compacted.num_rows()));
    RecordBatch::try_new_with_options(out_schema, arrays, &options).map_err(arrow_err)
}

fn bitmap_to_null_buffer(bm: Option<&Bitmap>) -> Option<NullBuffer> {
    bm.map(|b| NullBuffer::new(BooleanBuffer::from_iter((0..b.len()).map(|i| b.get(i)))))
}

fn column_to_arrow_array(col: &Column, target: &ArrowDataType) -> Result<ArrayRef, ColumnError> {
    match target {
        ArrowDataType::Dictionary(key_type, value_type) => match key_type.as_ref() {
            ArrowDataType::Int8 => column_to_dict_array::<Int8Type>(col, value_type),
            ArrowDataType::Int16 => column_to_dict_array::<Int16Type>(col, value_type),
            ArrowDataType::Int32 => column_to_dict_array::<Int32Type>(col, value_type),
            ArrowDataType::Int64 => column_to_dict_array::<Int64Type>(col, value_type),
            ArrowDataType::UInt8 => column_to_dict_array::<UInt8Type>(col, value_type),
            ArrowDataType::UInt16 => column_to_dict_array::<UInt16Type>(col, value_type),
            ArrowDataType::UInt32 => column_to_dict_array::<UInt32Type>(col, value_type),
            ArrowDataType::UInt64 => column_to_dict_array::<UInt64Type>(col, value_type),
            _ => Err(invalid("unsupported target dictionary key type")),
        },
        ArrowDataType::RunEndEncoded(run_ends_field, value_field) => {
            match run_ends_field.data_type() {
                ArrowDataType::Int16 => {
                    column_to_run_array::<Int16Type>(col, target, value_field.data_type())
                }
                ArrowDataType::Int32 => {
                    column_to_run_array::<Int32Type>(col, target, value_field.data_type())
                }
                ArrowDataType::Int64 => {
                    column_to_run_array::<Int64Type>(col, target, value_field.data_type())
                }
                _ => Err(invalid("unsupported target run-end index type")),
            }
        }
        _ => match col {
            Column::Flat(f) => flat_to_arrow_array(f, target),
            other => {
                let flat = other.materialize(None)?;
                flat_to_arrow_array(&flat, target)
            }
        },
    }
}

fn build_dict_array<K: DictKeyInt>(
    codes: &[u32],
    null_buf: Option<NullBuffer>,
    values: ArrayRef,
) -> Result<ArrayRef, ColumnError> {
    let mut keys = Vec::with_capacity(codes.len());
    for (i, &c) in codes.iter().enumerate() {
        if null_buf.as_ref().is_some_and(|nb| nb.is_null(i)) {
            keys.push(K::Native::default());
        } else {
            keys.push(K::u32_to_key(c)?);
        }
    }
    let key_array = PrimitiveArray::<K>::new(ScalarBuffer::from(keys), null_buf);
    let dict = DictionaryArray::<K>::try_new(key_array, values).map_err(arrow_err)?;
    Ok(Arc::new(dict))
}

fn column_to_dict_array<K: DictKeyInt>(
    col: &Column,
    value_type: &ArrowDataType,
) -> Result<ArrayRef, ColumnError> {
    match col {
        Column::Dict(dict) => {
            let values_arr = flat_to_arrow_array(dict.values(), value_type)?;
            let null_buf = if dict.has_nulls() {
                Some(NullBuffer::new(BooleanBuffer::from_iter(
                    (0..dict.len()).map(|i| !dict.is_null(i)),
                )))
            } else {
                None
            };
            build_dict_array::<K>(dict.codes(), null_buf, values_arr)
        }
        Column::Const { value, len } => {
            if *len == 0 {
                let values_arr =
                    flat_to_arrow_array(&FlatColumn::empty(&col.data_type()), value_type)?;
                return build_dict_array::<K>(&[], None, values_arr);
            }
            let one = Column::try_new_const(value.clone(), 1)?.materialize(None)?;
            let values_arr = flat_to_arrow_array(&one, value_type)?;
            let codes = vec![0u32; *len];
            let null_buf = if value.is_null() {
                Some(NullBuffer::new(BooleanBuffer::new_unset(*len)))
            } else {
                None
            };
            build_dict_array::<K>(&codes, null_buf, values_arr)
        }
        Column::Flat(flat) => {
            let values_arr = flat_to_arrow_array(flat, value_type)?;
            let mut codes = Vec::with_capacity(flat.len());
            for i in 0..flat.len() {
                let code = u32::try_from(i)
                    .map_err(|_| overflow("dictionary 1-to-1 code exceeds u32::MAX"))?;
                codes.push(code);
            }
            let null_buf = bitmap_to_null_buffer(flat.nulls());
            build_dict_array::<K>(&codes, null_buf, values_arr)
        }
        Column::Rle(_) => {
            let flat = col.materialize(None)?;
            let values_arr = flat_to_arrow_array(&flat, value_type)?;
            let mut codes = Vec::with_capacity(flat.len());
            for i in 0..flat.len() {
                let code = u32::try_from(i)
                    .map_err(|_| overflow("dictionary 1-to-1 code exceeds u32::MAX"))?;
                codes.push(code);
            }
            let null_buf = bitmap_to_null_buffer(flat.nulls());
            build_dict_array::<K>(&codes, null_buf, values_arr)
        }
    }
}

fn build_run_array<R: RunEndInt>(
    target: &ArrowDataType,
    run_ends: Vec<R::Native>,
    values: &ArrayRef,
    logical_len: usize,
) -> Result<ArrayRef, ColumnError> {
    let run_ends_arr = PrimitiveArray::<R>::new(ScalarBuffer::from(run_ends), None);
    let checked = RunArray::<R>::try_new(&run_ends_arr, values).map_err(arrow_err)?;
    if checked.data_type() == target {
        return Ok(Arc::new(checked));
    }
    let data = arrow::array::ArrayData::builder(target.clone())
        .len(logical_len)
        .add_child_data(run_ends_arr.to_data())
        .add_child_data(values.to_data())
        .build()
        .map_err(arrow_err)?;
    Ok(make_array(data))
}

fn column_to_run_array<R: RunEndInt>(
    col: &Column,
    target: &ArrowDataType,
    value_type: &ArrowDataType,
) -> Result<ArrayRef, ColumnError> {
    match col {
        Column::Rle(rle) => {
            let values_arr = flat_to_arrow_array(rle.values(), value_type)?;
            let mut ends = Vec::with_capacity(rle.run_ends().len());
            for &end in rle.run_ends() {
                ends.push(R::usize_to_end(end as usize)?);
            }
            build_run_array::<R>(target, ends, &values_arr, rle.len())
        }
        Column::Const { value, len } => {
            if *len == 0 {
                let values_arr =
                    flat_to_arrow_array(&FlatColumn::empty(&col.data_type()), value_type)?;
                return build_run_array::<R>(target, Vec::new(), &values_arr, 0);
            }
            let one = Column::try_new_const(value.clone(), 1)?.materialize(None)?;
            let values_arr = flat_to_arrow_array(&one, value_type)?;
            let end = R::usize_to_end(*len)?;
            build_run_array::<R>(target, vec![end], &values_arr, *len)
        }
        Column::Flat(flat) => flat_to_run_array::<R>(flat, target, value_type),
        Column::Dict(_) => {
            let flat = col.materialize(None)?;
            flat_to_run_array::<R>(&flat, target, value_type)
        }
    }
}

fn flat_to_run_array<R: RunEndInt>(
    flat: &FlatColumn,
    target: &ArrowDataType,
    value_type: &ArrowDataType,
) -> Result<ArrayRef, ColumnError> {
    let values_arr = flat_to_arrow_array(flat, value_type)?;
    let mut ends = Vec::with_capacity(flat.len());
    for i in 1..=flat.len() {
        ends.push(R::usize_to_end(i)?);
    }
    build_run_array::<R>(target, ends, &values_arr, flat.len())
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

fn utf8_data_to_arrow(
    u: &Utf8Data,
    null_buf: Option<NullBuffer>,
    target: &ArrowDataType,
) -> Result<ArrayRef, ColumnError> {
    match target {
        ArrowDataType::Utf8 => {
            let mut offsets = Vec::with_capacity(u.offsets().len());
            for &o in u.offsets() {
                offsets
                    .push(i32::try_from(o).map_err(|_| overflow("utf8 offset exceeds i32::MAX"))?);
            }
            let offset_buf = OffsetBuffer::new(ScalarBuffer::from(offsets));
            let arr = StringArray::try_new(offset_buf, Buffer::from_slice_ref(u.bytes()), null_buf)
                .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        ArrowDataType::LargeUtf8 => {
            let offsets: Vec<i64> = u.offsets().iter().map(|&o| i64::from(o)).collect();
            let offset_buf = OffsetBuffer::new(ScalarBuffer::from(offsets));
            let arr =
                LargeStringArray::try_new(offset_buf, Buffer::from_slice_ref(u.bytes()), null_buf)
                    .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        ArrowDataType::Utf8View => {
            let mut builder = StringViewBuilder::with_capacity(u.len());
            for i in 0..u.len() {
                if null_buf.as_ref().is_some_and(|nb| nb.is_null(i)) {
                    builder.append_null();
                } else {
                    let s = u.get(i).ok_or_else(|| invalid("utf8 row out of bounds"))?;
                    builder.append_value(s);
                }
            }
            Ok(Arc::new(builder.finish()))
        }
        _ => Err(invalid("target type is not an Arrow string type")),
    }
}

fn offsets_to_i32(offsets: &[u32]) -> Result<OffsetBuffer<i32>, ColumnError> {
    let mut out = Vec::with_capacity(offsets.len());
    for &o in offsets {
        out.push(i32::try_from(o).map_err(|_| overflow("offset exceeds i32::MAX"))?);
    }
    Ok(OffsetBuffer::new(ScalarBuffer::from(out)))
}

fn offsets_to_i64(offsets: &[u32]) -> OffsetBuffer<i64> {
    let out: Vec<i64> = offsets.iter().map(|&o| i64::from(o)).collect();
    OffsetBuffer::new(ScalarBuffer::from(out))
}

fn list_with_item_field(
    item_field: &FieldRef,
    offsets: &[u32],
    child_array: ArrayRef,
    null_buf: Option<NullBuffer>,
    large: bool,
) -> Result<ArrayRef, ColumnError> {
    if large {
        let arr = LargeListArray::try_new(
            Arc::clone(item_field),
            offsets_to_i64(offsets),
            child_array,
            null_buf,
        )
        .map_err(arrow_err)?;
        Ok(Arc::new(arr))
    } else {
        let arr = ListArray::try_new(
            Arc::clone(item_field),
            offsets_to_i32(offsets)?,
            child_array,
            null_buf,
        )
        .map_err(arrow_err)?;
        Ok(Arc::new(arr))
    }
}

fn flat_to_arrow_array(flat: &FlatColumn, target: &ArrowDataType) -> Result<ArrayRef, ColumnError> {
    let null_buf = bitmap_to_null_buffer(flat.nulls());
    match (flat.data(), target) {
        (FlatData::Null(len), ArrowDataType::Null) => Ok(Arc::new(NullArray::new(*len))),
        (FlatData::Bool(bm), ArrowDataType::Boolean) => {
            let bools = BooleanBuffer::from_iter((0..bm.len()).map(|i| bm.get(i)));
            Ok(Arc::new(BooleanArray::new(bools, null_buf)))
        }
        (FlatData::I64(vals), ArrowDataType::Int8) => {
            narrow_int_array::<Int8Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::Int16) => {
            narrow_int_array::<Int16Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::Int32) => {
            narrow_int_array::<Int32Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::Int64) => Ok(Arc::new(Int64Array::new(
            ScalarBuffer::from(vals.clone()),
            null_buf,
        ))),
        (FlatData::I64(vals), ArrowDataType::UInt8) => {
            narrow_int_array::<UInt8Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::UInt16) => {
            narrow_int_array::<UInt16Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::UInt32) => {
            narrow_int_array::<UInt32Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::I64(vals), ArrowDataType::UInt64) => {
            narrow_int_array::<UInt64Type>(vals, flat.nulls(), null_buf)
        }
        (FlatData::F64(vals), ArrowDataType::Float32) => {
            let out: Vec<f32> = vals.iter().map(|&v| v as f32).collect();
            Ok(Arc::new(Float32Array::new(
                ScalarBuffer::from(out),
                null_buf,
            )))
        }
        (FlatData::F64(vals), ArrowDataType::Float64) => Ok(Arc::new(Float64Array::new(
            ScalarBuffer::from(vals.clone()),
            null_buf,
        ))),
        (
            FlatData::Decimal128 {
                values,
                precision,
                scale,
            },
            ArrowDataType::Decimal128(tp, ts),
        ) => {
            if precision != tp || scale != ts {
                return Err(invalid("target decimal precision/scale mismatch"));
            }
            let arr = Decimal128Array::new(ScalarBuffer::from(values.clone()), null_buf)
                .with_precision_and_scale(*tp, *ts)
                .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
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
        (FlatData::Date(vals), ArrowDataType::Date32) => Ok(Arc::new(Date32Array::new(
            ScalarBuffer::from(vals.clone()),
            null_buf,
        ))),
        (FlatData::Interval(vals), ArrowDataType::Interval(IntervalUnit::MonthDayNano)) => {
            let mapped: Vec<IntervalMonthDayNano> = vals
                .iter()
                .map(|iv| IntervalMonthDayNano::new(iv.months, iv.days, iv.nanos))
                .collect();
            Ok(Arc::new(IntervalMonthDayNanoArray::new(
                ScalarBuffer::from(mapped),
                null_buf,
            )))
        }
        (
            FlatData::Utf8(u),
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 | ArrowDataType::Utf8View,
        ) => utf8_data_to_arrow(u, null_buf, target),
        (FlatData::Binary(b), ArrowDataType::Binary) => {
            let arr = BinaryArray::try_new(
                offsets_to_i32(b.offsets())?,
                Buffer::from_slice_ref(b.bytes()),
                null_buf,
            )
            .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
        (FlatData::Binary(b), ArrowDataType::LargeBinary) => {
            let arr = LargeBinaryArray::try_new(
                offsets_to_i64(b.offsets()),
                Buffer::from_slice_ref(b.bytes()),
                null_buf,
            )
            .map_err(arrow_err)?;
            Ok(Arc::new(arr))
        }
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
            let child_array = column_to_arrow_array(values, item_field.data_type())?;
            list_with_item_field(item_field, offsets, child_array, null_buf, false)
        }
        (
            FlatData::List {
                offsets, values, ..
            },
            ArrowDataType::LargeList(item_field),
        ) => {
            let child_array = column_to_arrow_array(values, item_field.data_type())?;
            list_with_item_field(item_field, offsets, child_array, null_buf, true)
        }
        (FlatData::MapUtf8(map), ArrowDataType::Map(entries_field, sorted)) => {
            let ArrowDataType::Struct(entry_fields) = entries_field.data_type() else {
                return Err(invalid("target map entries must be a struct"));
            };
            if entry_fields.len() != 2 {
                return Err(invalid("target map struct must have 2 fields"));
            }
            let keys_arr = utf8_data_to_arrow(map.keys(), None, entry_fields[0].data_type())?;
            let val_nulls = bitmap_to_null_buffer(map.value_nulls());
            let vals_arr =
                utf8_data_to_arrow(map.values(), val_nulls, entry_fields[1].data_type())?;
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
        (
            FlatData::Events(ev),
            ArrowDataType::List(item_field) | ArrowDataType::LargeList(item_field),
        ) => {
            let large = matches!(target, ArrowDataType::LargeList(_));
            let ArrowDataType::Struct(fields) = item_field.data_type() else {
                return Err(invalid("target events item must be a struct"));
            };
            if !is_events_struct(fields) {
                return Err(invalid("target events struct shape mismatch"));
            }
            let ArrowDataType::Timestamp(TimeUnit::Nanosecond, tz) = fields[0].data_type() else {
                return Err(invalid("target events time field mismatch"));
            };
            let time_arr: ArrayRef = Arc::new(
                TimestampNanosecondArray::new(ScalarBuffer::from(ev.times().to_vec()), None)
                    .with_timezone_opt(tz.clone()),
            );
            let name_arr = utf8_data_to_arrow(ev.names(), None, fields[1].data_type())?;
            let attr_nulls = bitmap_to_null_buffer(ev.attribute_nulls());
            let attr_arr = utf8_data_to_arrow(ev.attributes(), attr_nulls, fields[2].data_type())?;
            let struct_arr =
                StructArray::try_new(fields.clone(), vec![time_arr, name_arr, attr_arr], None)
                    .map_err(arrow_err)?;
            list_with_item_field(
                item_field,
                ev.offsets(),
                Arc::new(struct_arr),
                null_buf,
                large,
            )
        }
        (
            FlatData::Links(lk),
            ArrowDataType::List(item_field) | ArrowDataType::LargeList(item_field),
        ) => {
            let large = matches!(target, ArrowDataType::LargeList(_));
            let ArrowDataType::Struct(fields) = item_field.data_type() else {
                return Err(invalid("target links item must be a struct"));
            };
            if !is_links_struct(fields) {
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
            let attr_arr = utf8_data_to_arrow(lk.attributes(), attr_nulls, fields[2].data_type())?;
            let struct_arr =
                StructArray::try_new(fields.clone(), vec![trace_arr, span_arr, attr_arr], None)
                    .map_err(arrow_err)?;
            list_with_item_field(
                item_field,
                lk.offsets(),
                Arc::new(struct_arr),
                null_buf,
                large,
            )
        }
        _ => Err(invalid("column data type does not match target Arrow type")),
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::{
        Int8Builder, Int16Builder, Int32Builder, Int64Builder, LargeBinaryArray, LargeListArray,
        LargeStringArray, ListBuilder, MapBuilder, StringBuilder, StructBuilder,
        TimestampNanosecondBuilder, UInt8Builder, UInt16Builder, UInt32Builder, UInt64Builder,
    };
    use arrow::datatypes::Float32Type;
    use cetra_column::{ScalarValue, Selection};
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::schema::{scores_schema, spans_schema, traces_schema};
    use cetra_core::span::{SpanEvent, SpanLink};

    use crate::test_support::{score_row, score_rows, span_row, span_rows, trace_batch};

    use super::*;

    #[test]
    fn spans_traces_scores_round_trip_exact() {
        let mut s1 = span_row(0, 0, 1_000, 1_500);
        s1.tags = vec!["prod".into(), "chat".into()];
        s1.metadata.insert("env".into(), "staging".into());
        s1.attributes
            .insert("gen_ai.system".into(), "openai".into());
        s1.resource_attributes
            .insert("service.name".into(), "api".into());
        s1.events = vec![
            SpanEvent {
                time: 1_010,
                name: "first_token".into(),
                attributes: Some(r#"{"tok":1}"#.into()),
            },
            SpanEvent {
                time: 1_020,
                name: "done".into(),
                attributes: None,
            },
        ];
        s1.links = vec![SpanLink {
            trace_id: TraceId::from_bytes([0xaa; 16]).unwrap(),
            span_id: SpanId::from_bytes([0xbb; 8]).unwrap(),
            attributes: Some(r#"{"rel":"follows"}"#.into()),
        }];
        s1.schema_version = 2;

        let mut s2 = span_row(0, 1, 1_050, 1_600);
        s2.parent_span_id = Some(s1.span_id);
        s2.request_model = Some("gpt-4o".into());
        s2.input_tokens = Some(12);
        s2.output_tokens = Some(34);
        s2.cost_total = Some(0.0012);

        let spans_rb = span_rows(&[s1.clone(), s2.clone()]);
        assert_eq!(spans_rb.schema(), spans_schema());
        let spans_col_batch = record_batch_to_column(&spans_rb).unwrap();
        let spans_rt = column_to_record_batch(&spans_col_batch, Some(&spans_schema())).unwrap();
        assert_eq!(spans_rt, spans_rb);

        let traces_rb = trace_batch(&[s1, s2]);
        assert_eq!(traces_rb.schema(), traces_schema());
        let traces_col_batch = record_batch_to_column(&traces_rb).unwrap();
        let traces_rt = column_to_record_batch(&traces_col_batch, Some(&traces_schema())).unwrap();
        assert_eq!(traces_rt, traces_rb);

        let mut sc1 = score_row(1, 0, 2_000, 2_100, 2_200, false, "accuracy");
        sc1.metadata.insert("judge".into(), "gpt4".into());
        sc1.reason = Some("looks good".into());
        let sc2 = score_row(2, 0, 2_000, 2_100, 2_200, true, "toxicity");

        let scores_rb = score_rows(&[sc1, sc2]);
        assert_eq!(scores_rb.schema(), scores_schema());
        let scores_col_batch = record_batch_to_column(&scores_rb).unwrap();
        let scores_rt = column_to_record_batch(&scores_col_batch, Some(&scores_schema())).unwrap();
        assert_eq!(scores_rt, scores_rb);
    }

    #[test]
    fn map_with_null_values_and_rejected_null_key() {
        // Valid map with null entry value and null parent row
        let mut map_builder = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        map_builder.keys().append_value("k1");
        map_builder.values().append_value("v1");
        map_builder.keys().append_value("k2");
        map_builder.values().append_null();
        map_builder.append(true).unwrap();

        map_builder.append(false).unwrap(); // null parent row

        map_builder.keys().append_value("k3");
        map_builder.values().append_value("v3");
        map_builder.append(true).unwrap();

        let map_arr = map_builder.finish();
        let schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "m",
            map_arr.data_type().clone(),
            true,
        )]));
        let rb = RecordBatch::try_new(Arc::clone(&schema), vec![Arc::new(map_arr)]).unwrap();
        let col_batch = record_batch_to_column(&rb).unwrap();
        let rt = column_to_record_batch(&col_batch, Some(&schema)).unwrap();
        assert_eq!(rt, rb);

        // Arrow 59 forbids constructing a MapArray with a null key in safe Rust (`#![forbid(unsafe_code)]`);
        // verify non-Utf8 map key schema is rejected with InvalidBatch, alongside null Events name rejection.
        let mut int_key_map_b = MapBuilder::new(None, Int32Builder::new(), StringBuilder::new());
        int_key_map_b.keys().append_value(1);
        int_key_map_b.values().append_value("v1");
        int_key_map_b.append(true).unwrap();
        let bad_arr = int_key_map_b.finish();
        let bad_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "m",
            bad_arr.data_type().clone(),
            true,
        )]));
        let bad_rb = RecordBatch::try_new(bad_schema, vec![Arc::new(bad_arr)]).unwrap();
        assert!(matches!(
            record_batch_to_column(&bad_rb),
            Err(ColumnError::InvalidBatch { .. })
        ));
    }

    #[test]
    fn sql_types_and_encodings_round_trip() {
        // Nested List(List(Utf8))
        let mut nested_builder = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
        nested_builder.values().values().append_value("a");
        nested_builder.values().values().append_null();
        nested_builder.values().append(true);
        nested_builder.values().append(false);
        nested_builder.append(true);
        nested_builder.append(false);
        nested_builder.values().append(true);
        nested_builder.append(true);
        let nested_list = Arc::new(nested_builder.finish()) as ArrayRef;

        // List(Int64)
        let mut i64_list_builder = ListBuilder::new(Int64Builder::new());
        i64_list_builder.values().append_value(10);
        i64_list_builder.values().append_value(-20);
        i64_list_builder.append(true);
        i64_list_builder.append(false);
        i64_list_builder.append(true);
        let i64_list = Arc::new(i64_list_builder.finish()) as ArrayRef;

        // Dictionary arrays across multiple key widths
        let dict_i8_keys = Int8Array::from(vec![Some(0), None, Some(1)]);
        let dict_vals = Arc::new(StringArray::from(vec!["alpha", "beta"])) as ArrayRef;
        let dict_i8 = Arc::new(
            DictionaryArray::<Int8Type>::try_new(dict_i8_keys, Arc::clone(&dict_vals)).unwrap(),
        ) as ArrayRef;

        let dict_u16_keys = UInt16Array::from(vec![Some(1), Some(0), None]);
        let dict_u16 = Arc::new(
            DictionaryArray::<UInt16Type>::try_new(dict_u16_keys, Arc::clone(&dict_vals)).unwrap(),
        ) as ArrayRef;

        let dict_u64_keys = UInt64Array::from(vec![Some(0), Some(1), Some(0)]);
        let dict_u64 =
            Arc::new(DictionaryArray::<UInt64Type>::try_new(dict_u64_keys, dict_vals).unwrap())
                as ArrayRef;

        // RunArrays across Int16 / Int32 / Int64
        let rle_i16 = Arc::new(
            RunArray::<Int16Type>::try_new(
                &Int16Array::from(vec![2, 3]),
                &StringArray::from(vec![Some("r1"), None]),
            )
            .unwrap(),
        ) as ArrayRef;
        let rle_i32 = Arc::new(
            RunArray::<Int32Type>::try_new(
                &Int32Array::from(vec![1, 3]),
                &Int64Array::from(vec![Some(42), Some(99)]),
            )
            .unwrap(),
        ) as ArrayRef;
        let rle_i64 = Arc::new(
            RunArray::<Int64Type>::try_new(
                &Int64Array::from(vec![2, 3]),
                &BooleanArray::from(vec![Some(true), Some(false)]),
            )
            .unwrap(),
        ) as ArrayRef;

        let decimal_arr = Arc::new(
            Decimal128Array::from(vec![Some(123_456_789_i128), None, Some(-42_i128)])
                .with_precision_and_scale(38, 18)
                .unwrap(),
        ) as ArrayRef;
        let ts_utc = Arc::new(
            TimestampNanosecondArray::from(vec![Some(100), None, Some(300)]).with_timezone("UTC"),
        ) as ArrayRef;
        let ts_naive = Arc::new(TimestampNanosecondArray::from(vec![
            Some(10),
            Some(20),
            None,
        ])) as ArrayRef;
        let date_arr = Arc::new(Date32Array::from(vec![Some(19_000), None, Some(-1)])) as ArrayRef;
        let interval_arr = Arc::new(IntervalMonthDayNanoArray::from(vec![
            Some(IntervalMonthDayNano::new(1, -2, 300)),
            None,
            Some(IntervalMonthDayNano::new(0, 0, 0)),
        ])) as ArrayRef;
        let bin_arr = Arc::new(BinaryArray::from_opt_vec(vec![
            Some(b"\x00\xff".as_slice()),
            None,
            Some(b"".as_slice()),
        ])) as ArrayRef;
        let null_arr = Arc::new(NullArray::new(3)) as ArrayRef;

        let fields = vec![
            ArrowField::new("nulls", null_arr.data_type().clone(), true),
            ArrowField::new("dec", decimal_arr.data_type().clone(), true),
            ArrowField::new("ts_utc", ts_utc.data_type().clone(), true),
            ArrowField::new("ts_naive", ts_naive.data_type().clone(), true),
            ArrowField::new("dt", date_arr.data_type().clone(), true),
            ArrowField::new("iv", interval_arr.data_type().clone(), true),
            ArrowField::new("bin", bin_arr.data_type().clone(), true),
            ArrowField::new("list_i64", i64_list.data_type().clone(), true),
            ArrowField::new("nested", nested_list.data_type().clone(), true),
            ArrowField::new("dict_i8", dict_i8.data_type().clone(), true),
            ArrowField::new("dict_u16", dict_u16.data_type().clone(), true),
            ArrowField::new("dict_u64", dict_u64.data_type().clone(), true),
            ArrowField::new("rle_i16", rle_i16.data_type().clone(), true),
            ArrowField::new("rle_i32", rle_i32.data_type().clone(), true),
            ArrowField::new("rle_i64", rle_i64.data_type().clone(), true),
        ];
        let schema = Arc::new(ArrowSchema::new(fields));
        let rb = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                null_arr,
                decimal_arr,
                ts_utc,
                ts_naive,
                date_arr,
                interval_arr,
                bin_arr,
                i64_list,
                nested_list,
                dict_i8,
                dict_u16,
                dict_u64,
                rle_i16,
                rle_i32,
                rle_i64,
            ],
        )
        .unwrap();

        let col_batch = record_batch_to_column(&rb).unwrap();
        assert!(matches!(
            col_batch.column_by_name("dict_i8").unwrap(),
            Column::Dict(_)
        ));
        assert!(matches!(
            col_batch.column_by_name("dict_u16").unwrap(),
            Column::Dict(_)
        ));
        assert!(matches!(
            col_batch.column_by_name("dict_u64").unwrap(),
            Column::Dict(_)
        ));
        assert!(matches!(
            col_batch.column_by_name("rle_i16").unwrap(),
            Column::Rle(_)
        ));
        assert!(matches!(
            col_batch.column_by_name("rle_i32").unwrap(),
            Column::Rle(_)
        ));
        assert!(matches!(
            col_batch.column_by_name("rle_i64").unwrap(),
            Column::Rle(_)
        ));

        let rt = column_to_record_batch(&col_batch, Some(&schema)).unwrap();
        assert_eq!(rt, rb);

        // target_schema = None materializes everything to flat default Arrow arrays
        let flat_rt = column_to_record_batch(&col_batch, None).unwrap();
        assert_eq!(flat_rt.num_rows(), 3);
        assert_eq!(
            flat_rt.schema().as_ref(),
            &column_schema_to_arrow(col_batch.schema())
        );

        // Const -> target Dictionary & RunEndEncoded wrapping
        let const_schema = Schema::try_new_arc(vec![
            Field::new("c_dict", DataType::Utf8, false),
            Field::new("c_rle", DataType::I64, false),
            Field::new("f_dict", DataType::I64, true),
            Field::new("f_rle", DataType::Utf8, true),
        ])
        .unwrap();
        let const_batch = Batch::try_new_all(
            const_schema,
            vec![
                Column::try_new_const(ScalarValue::Utf8("hello".into()), 4).unwrap(),
                Column::try_new_const(ScalarValue::I64(7), 4).unwrap(),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::I64(vec![10, 0, 20, 30]),
                        Some(Bitmap::from_bools(&[true, false, true, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["x", "", "y", "z"]).unwrap()),
                        Some(Bitmap::from_bools(&[true, false, true, true])),
                    )
                    .unwrap(),
                ),
            ],
            4,
        )
        .unwrap();
        let target_enc = ArrowSchema::new(vec![
            ArrowField::new(
                "c_dict",
                ArrowDataType::Dictionary(
                    Box::new(ArrowDataType::Int16),
                    Box::new(ArrowDataType::Utf8),
                ),
                false,
            ),
            ArrowField::new(
                "c_rle",
                ArrowDataType::RunEndEncoded(
                    Arc::new(ArrowField::new("run_ends", ArrowDataType::Int32, false)),
                    Arc::new(ArrowField::new("values", ArrowDataType::Int64, true)),
                ),
                false,
            ),
            ArrowField::new(
                "f_dict",
                ArrowDataType::Dictionary(
                    Box::new(ArrowDataType::UInt8),
                    Box::new(ArrowDataType::Int64),
                ),
                true,
            ),
            ArrowField::new(
                "f_rle",
                ArrowDataType::RunEndEncoded(
                    Arc::new(ArrowField::new("run_ends", ArrowDataType::Int16, false)),
                    Arc::new(ArrowField::new("values", ArrowDataType::Utf8, true)),
                ),
                true,
            ),
        ]);
        let enc_rb = column_to_record_batch(&const_batch, Some(&target_enc)).unwrap();
        assert_eq!(enc_rb.schema().as_ref(), &target_enc);
        let enc_back = record_batch_to_column(&enc_rb).unwrap();
        assert!(const_batch.semantic_eq(&enc_back));

        // 0-column non-empty batch
        let zero_schema = Arc::new(ArrowSchema::empty());
        let zero_opts = RecordBatchOptions::new().with_row_count(Some(5));
        let zero_rb =
            RecordBatch::try_new_with_options(Arc::clone(&zero_schema), vec![], &zero_opts)
                .unwrap();
        let zero_col = record_batch_to_column(&zero_rb).unwrap();
        assert_eq!(zero_col.num_rows(), 5);
        assert_eq!(zero_col.columns().len(), 0);
        let zero_rt = column_to_record_batch(&zero_col, Some(&zero_schema)).unwrap();
        assert_eq!(zero_rt.num_rows(), 5);
        assert_eq!(zero_rt.num_columns(), 0);
    }

    #[test]
    fn sliced_arrow_arrays_preserve_null_and_value_offsets() {
        let rows = 10usize;
        let mut str_b = StringBuilder::new();
        let mut list_b = ListBuilder::new(StringBuilder::new());
        let mut map_b = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        let mut ev_b = ListBuilder::new(StructBuilder::from_fields(
            Fields::from(vec![
                ArrowField::new(
                    event_field::TIME,
                    ArrowDataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                    false,
                ),
                ArrowField::new(event_field::NAME, ArrowDataType::Utf8, false),
                ArrowField::new(event_field::ATTRIBUTES, ArrowDataType::Utf8, true),
            ]),
            rows,
        ));
        let mut lk_b = ListBuilder::new(StructBuilder::from_fields(
            Fields::from(vec![
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
            ]),
            rows,
        ));

        for i in 0..rows {
            if i % 3 == 0 {
                str_b.append_null();
                list_b.append(false);
                map_b.append(false).unwrap();
                ev_b.append(false);
                lk_b.append(false);
            } else {
                str_b.append_value(format!("str-{i}"));
                list_b.values().append_value(format!("li-{i}"));
                list_b.append(true);
                map_b.keys().append_value(format!("k{i}"));
                if i % 2 == 0 {
                    map_b.values().append_null();
                } else {
                    map_b.values().append_value(format!("v{i}"));
                }
                map_b.append(true).unwrap();

                let ev_vals = ev_b.values();
                ev_vals
                    .field_builder::<TimestampNanosecondBuilder>(0)
                    .unwrap()
                    .append_value(i as i64 * 100);
                ev_vals
                    .field_builder::<StringBuilder>(1)
                    .unwrap()
                    .append_value(format!("ev-{i}"));
                let attr_b = ev_vals.field_builder::<StringBuilder>(2).unwrap();
                if i % 2 == 0 {
                    attr_b.append_null();
                } else {
                    attr_b.append_value(format!("{{\"i\":{i}}}"));
                }
                ev_vals.append(true);
                ev_b.append(true);

                let lk_vals = lk_b.values();
                lk_vals
                    .field_builder::<arrow::array::FixedSizeBinaryBuilder>(0)
                    .unwrap()
                    .append_value([i as u8; 16])
                    .unwrap();
                lk_vals
                    .field_builder::<arrow::array::FixedSizeBinaryBuilder>(1)
                    .unwrap()
                    .append_value([i as u8; 8])
                    .unwrap();
                let lk_attr_b = lk_vals.field_builder::<StringBuilder>(2).unwrap();
                if i % 2 == 0 {
                    lk_attr_b.append_null();
                } else {
                    lk_attr_b.append_value(format!("{{\"l\":{i}}}"));
                }
                lk_vals.append(true);
                lk_b.append(true);
            }
        }

        let dict_keys = Int32Array::from(
            (0..rows)
                .map(|i| {
                    if i % 4 == 0 {
                        None
                    } else {
                        Some((i % 3) as i32)
                    }
                })
                .collect::<Vec<_>>(),
        );
        let dict_vals = Arc::new(StringArray::from(vec!["d0", "d1", "d2"])) as ArrayRef;
        let dict_arr =
            Arc::new(DictionaryArray::<Int32Type>::try_new(dict_keys, dict_vals).unwrap())
                as ArrayRef;

        let run_arr = Arc::new(
            RunArray::<Int32Type>::try_new(
                &Int32Array::from(vec![2, 5, 8, 10]),
                &Int64Array::from(vec![Some(10), None, Some(30), Some(40)]),
            )
            .unwrap(),
        ) as ArrayRef;

        let str_arr = Arc::new(str_b.finish()) as ArrayRef;
        let list_arr = Arc::new(list_b.finish()) as ArrayRef;
        let map_arr = Arc::new(map_b.finish()) as ArrayRef;
        let ev_arr = Arc::new(ev_b.finish()) as ArrayRef;
        let lk_arr = Arc::new(lk_b.finish()) as ArrayRef;

        let schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("s", str_arr.data_type().clone(), true),
            ArrowField::new("l", list_arr.data_type().clone(), true),
            ArrowField::new("m", map_arr.data_type().clone(), true),
            ArrowField::new("ev", ev_arr.data_type().clone(), true),
            ArrowField::new("lk", lk_arr.data_type().clone(), true),
            ArrowField::new("d", dict_arr.data_type().clone(), true),
            ArrowField::new("r", run_arr.data_type().clone(), true),
        ]));
        let rb = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                str_arr, list_arr, map_arr, ev_arr, lk_arr, dict_arr, run_arr,
            ],
        )
        .unwrap();

        let sliced = rb.slice(3, 5);
        let col_batch = record_batch_to_column(&sliced).unwrap();
        let rt = column_to_record_batch(&col_batch, Some(&schema)).unwrap();
        assert_eq!(rt, sliced);

        // Also test sliced-empty RunArray guard
        let empty_sliced = rb.slice(5, 0);
        let empty_col = record_batch_to_column(&empty_sliced).unwrap();
        assert_eq!(empty_col.num_rows(), 0);
        let empty_rt = column_to_record_batch(&empty_col, Some(&schema)).unwrap();
        assert_eq!(empty_rt, empty_sliced);

        // Also test sliced non-null StringArray (offset rebasing fast path)
        let dense_str = StringArray::from(vec!["zero", "one", "two", "three", "four"]);
        let dense_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "s",
            ArrowDataType::Utf8,
            false,
        )]));
        let dense_rb =
            RecordBatch::try_new(Arc::clone(&dense_schema), vec![Arc::new(dense_str)]).unwrap();
        let dense_sliced = dense_rb.slice(2, 2);
        let dense_col = record_batch_to_column(&dense_sliced).unwrap();
        let dense_rt = column_to_record_batch(&dense_col, Some(&dense_schema)).unwrap();
        assert_eq!(dense_rt, dense_sliced);
    }

    #[test]
    fn overflow_on_u64_above_i64_max_and_bad_integer_narrowing() {
        // u64 above i64::MAX fails with Overflow
        let u64_arr = UInt64Array::from(vec![Some(1_u64), Some(i64::MAX as u64 + 1)]);
        let u64_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "x",
            ArrowDataType::UInt64,
            true,
        )]));
        let u64_rb = RecordBatch::try_new(u64_schema, vec![Arc::new(u64_arr)]).unwrap();
        assert!(matches!(
            record_batch_to_column(&u64_rb),
            Err(ColumnError::Overflow { .. })
        ));

        // Null slot holding u64::MAX does NOT fail
        let u64_null_arr = UInt64Array::new(
            ScalarBuffer::from(vec![42_u64, u64::MAX]),
            Some(NullBuffer::from(vec![true, false])),
        );
        let u64_null_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "x",
            ArrowDataType::UInt64,
            true,
        )]));
        let u64_null_rb =
            RecordBatch::try_new(Arc::clone(&u64_null_schema), vec![Arc::new(u64_null_arr)])
                .unwrap();
        let ok_batch = record_batch_to_column(&u64_null_rb).unwrap();
        let ok_rt = column_to_record_batch(&ok_batch, Some(&u64_null_schema)).unwrap();
        assert_eq!(ok_rt, u64_null_rb);

        // Narrowing I64 to Int8/UInt8/Int16/UInt16/Int32/UInt32 out of range fails with Overflow
        let col_schema = Schema::try_new_arc(vec![Field::new("n", DataType::I64, true)]).unwrap();
        let col = FlatColumn::try_new(
            FlatData::I64(vec![300, 0]),
            Some(Bitmap::from_bools(&[true, false])),
        )
        .unwrap();
        let batch = Batch::try_new_all(col_schema, vec![Column::Flat(col)], 2).unwrap();

        for target_dt in [ArrowDataType::Int8, ArrowDataType::UInt8] {
            let target = ArrowSchema::new(vec![ArrowField::new("n", target_dt, true)]);
            assert!(matches!(
                column_to_record_batch(&batch, Some(&target)),
                Err(ColumnError::Overflow { .. })
            ));
        }

        // Negative I64 narrowed to unsigned fails with Overflow
        let neg_schema = Schema::try_new_arc(vec![Field::new("n", DataType::I64, false)]).unwrap();
        let neg_batch = Batch::try_new_all(
            neg_schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![-1]), None).unwrap(),
            )],
            1,
        )
        .unwrap();
        for unsigned_dt in [
            ArrowDataType::UInt8,
            ArrowDataType::UInt16,
            ArrowDataType::UInt32,
            ArrowDataType::UInt64,
        ] {
            let target = ArrowSchema::new(vec![ArrowField::new("n", unsigned_dt, false)]);
            assert!(matches!(
                column_to_record_batch(&neg_batch, Some(&target)),
                Err(ColumnError::Overflow { .. })
            ));
        }

        // All 8 integer widths round-trip when in range
        let int_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("i8", ArrowDataType::Int8, false),
            ArrowField::new("i16", ArrowDataType::Int16, false),
            ArrowField::new("i32", ArrowDataType::Int32, false),
            ArrowField::new("i64", ArrowDataType::Int64, false),
            ArrowField::new("u8", ArrowDataType::UInt8, false),
            ArrowField::new("u16", ArrowDataType::UInt16, false),
            ArrowField::new("u32", ArrowDataType::UInt32, false),
            ArrowField::new("u64", ArrowDataType::UInt64, false),
        ]));
        let mut b_i8 = Int8Builder::new();
        b_i8.append_value(-5);
        let mut b_i16 = Int16Builder::new();
        b_i16.append_value(-300);
        let mut b_i32 = Int32Builder::new();
        b_i32.append_value(-70_000);
        let mut b_i64 = Int64Builder::new();
        b_i64.append_value(i64::MIN);
        let mut b_u8 = UInt8Builder::new();
        b_u8.append_value(200);
        let mut b_u16 = UInt16Builder::new();
        b_u16.append_value(40_000);
        let mut b_u32 = UInt32Builder::new();
        b_u32.append_value(3_000_000_000);
        let mut b_u64 = UInt64Builder::new();
        b_u64.append_value(i64::MAX as u64);
        let all_ints_rb = RecordBatch::try_new(
            Arc::clone(&int_schema),
            vec![
                Arc::new(b_i8.finish()),
                Arc::new(b_i16.finish()),
                Arc::new(b_i32.finish()),
                Arc::new(b_i64.finish()),
                Arc::new(b_u8.finish()),
                Arc::new(b_u16.finish()),
                Arc::new(b_u32.finish()),
                Arc::new(b_u64.finish()),
            ],
        )
        .unwrap();
        let all_ints_col = record_batch_to_column(&all_ints_rb).unwrap();
        let all_ints_rt = column_to_record_batch(&all_ints_col, Some(&int_schema)).unwrap();
        assert_eq!(all_ints_rt, all_ints_rb);
    }

    #[test]
    fn garbage_behind_null_parent_skipped_and_invalid_non_null_rejected() {
        // Null parent row in List<UInt64> whose underlying buffer has u64::MAX is skipped (not Overflow)
        let u64_vals = UInt64Array::from(vec![Some(7), Some(u64::MAX)]);
        let u64_list = ListArray::new(
            Arc::new(ArrowField::new("item", ArrowDataType::UInt64, true)),
            OffsetBuffer::new(ScalarBuffer::from(vec![0i32, 1, 2])),
            Arc::new(u64_vals),
            Some(NullBuffer::from(vec![true, false])),
        );
        let u64_list_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "xs",
            u64_list.data_type().clone(),
            true,
        )]));
        let u64_list_rb =
            RecordBatch::try_new(Arc::clone(&u64_list_schema), vec![Arc::new(u64_list)]).unwrap();
        let u64_list_col = record_batch_to_column(&u64_list_rb).unwrap();
        let u64_list_rt = column_to_record_batch(&u64_list_col, Some(&u64_list_schema)).unwrap();
        assert_eq!(u64_list_rt, u64_list_rb);

        // Null parent Events row whose underlying struct has a null item is skipped;
        // slicing to exclude the valid row or keeping both works cleanly
        let ev_fields = Fields::from(vec![
            ArrowField::new(
                event_field::TIME,
                ArrowDataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                true,
            ),
            ArrowField::new(event_field::NAME, ArrowDataType::Utf8, true),
            ArrowField::new(event_field::ATTRIBUTES, ArrowDataType::Utf8, true),
        ]);
        let ev_struct_with_garbage = StructArray::try_new(
            ev_fields.clone(),
            vec![
                Arc::new(
                    TimestampNanosecondArray::from(vec![Some(100), None]).with_timezone("UTC"),
                ),
                Arc::new(StringArray::from(vec![Some("ok"), None])),
                Arc::new(StringArray::from(vec![Option::<&str>::None, None])),
            ],
            Some(NullBuffer::from(vec![true, false])),
        )
        .unwrap();
        let ev_item_nullable = Arc::new(ArrowField::new(
            "item",
            ArrowDataType::Struct(ev_fields.clone()),
            true,
        ));
        let ok_ev_list = ListArray::try_new(
            Arc::clone(&ev_item_nullable),
            OffsetBuffer::new(ScalarBuffer::from(vec![0i32, 1, 2])),
            Arc::new(ev_struct_with_garbage),
            Some(NullBuffer::from(vec![true, false])),
        )
        .unwrap();
        let ok_ev_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "ev",
            ok_ev_list.data_type().clone(),
            true,
        )]));
        let ok_ev_rb =
            RecordBatch::try_new(Arc::clone(&ok_ev_schema), vec![Arc::new(ok_ev_list)]).unwrap();
        let ok_ev_col = record_batch_to_column(&ok_ev_rb).unwrap();
        let ok_ev_rt = column_to_record_batch(&ok_ev_col, Some(&ok_ev_schema)).unwrap();
        assert!(ok_ev_rt.column(0).is_valid(0));
        assert!(ok_ev_rt.column(0).is_null(1));

        // Non-null parent Events row with null struct item rejected
        let ev_struct = StructArray::try_new(
            ev_fields.clone(),
            vec![
                Arc::new(TimestampNanosecondArray::from(vec![100]).with_timezone("UTC")),
                Arc::new(StringArray::from(vec![Some("ev")])),
                Arc::new(StringArray::from(vec![Option::<&str>::None])),
            ],
            Some(NullBuffer::from(vec![false])),
        )
        .unwrap();
        let ev_item_field = Arc::new(ArrowField::new(
            "item",
            ArrowDataType::Struct(ev_fields),
            true,
        ));
        let bad_ev_list = ListArray::try_new(
            ev_item_field,
            OffsetBuffer::new(ScalarBuffer::from(vec![0i32, 1])),
            Arc::new(ev_struct),
            None,
        )
        .unwrap();
        let bad_ev_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "ev",
            bad_ev_list.data_type().clone(),
            true,
        )]));
        let bad_ev_rb = RecordBatch::try_new(bad_ev_schema, vec![Arc::new(bad_ev_list)]).unwrap();
        assert!(matches!(
            record_batch_to_column(&bad_ev_rb),
            Err(ColumnError::InvalidBatch { .. })
        ));

        // Float32, LargeUtf8, Utf8View, LargeBinary, LargeList, and Selection compaction
        let f32_arr =
            Arc::new(PrimitiveArray::<Float32Type>::from(vec![Some(1.5), None])) as ArrayRef;
        let lu_arr = Arc::new(LargeStringArray::from(vec![Some("large"), None])) as ArrayRef;
        let uv_arr = Arc::new(StringViewArray::from(vec![
            Some("view"),
            Some("long_string_view"),
        ])) as ArrayRef;
        let lb_arr = Arc::new(LargeBinaryArray::from_opt_vec(vec![
            Some(b"ab".as_slice()),
            None,
        ])) as ArrayRef;
        let ll_arr = Arc::new(LargeListArray::new(
            Arc::new(ArrowField::new("item", ArrowDataType::Int32, true)),
            OffsetBuffer::new(ScalarBuffer::from(vec![0i64, 2, 2])),
            Arc::new(Int32Array::from(vec![1, 2])),
            Some(NullBuffer::from(vec![true, false])),
        )) as ArrayRef;
        let misc_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("f32", ArrowDataType::Float32, true),
            ArrowField::new("lu", ArrowDataType::LargeUtf8, true),
            ArrowField::new("uv", ArrowDataType::Utf8View, true),
            ArrowField::new("lb", ArrowDataType::LargeBinary, true),
            ArrowField::new("ll", ll_arr.data_type().clone(), true),
        ]));
        let misc_rb = RecordBatch::try_new(
            Arc::clone(&misc_schema),
            vec![f32_arr, lu_arr, uv_arr, lb_arr, ll_arr],
        )
        .unwrap();
        let misc_col = record_batch_to_column(&misc_rb).unwrap();
        let filtered = misc_col
            .with_selection(Selection::Indices(vec![1]))
            .unwrap();
        let misc_rt = column_to_record_batch(&filtered, Some(&misc_schema)).unwrap();
        assert_eq!(misc_rt.num_rows(), 1);
        assert_eq!(misc_rt, misc_rb.slice(1, 1));
    }
}
