//! Deterministic test batches, and the array comparison the tests share. M1.3 adds the
//! string and list columns.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, FixedSizeBinaryArray, Float64Array, Int64Array, ListArray,
    RecordBatch, StringArray, TimestampNanosecondArray,
};
use arrow::buffer::{Buffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};

const SPAN_KINDS: [&str; 5] = ["llm", "tool", "agent", "chain", "span"];
const MODELS: [&str; 6] = [
    "gpt-4o",
    "gpt-4o-mini",
    "claude-3-5-sonnet",
    "claude-3-haiku",
    "llama-3-70b",
    "gemini-1.5-pro",
];
const TAGS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];

/// The schema of [`batch`].
pub(crate) fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::FixedSizeBinary(16), false),
        Field::new(
            "start_time",
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            false,
        ),
        Field::new("input_tokens", DataType::Int64, true),
        Field::new("cost", DataType::Float64, true),
        Field::new("flag", DataType::Boolean, false),
        Field::new("span_kind", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("model", DataType::Utf8, true),
        Field::new("environment", DataType::Utf8, false),
        Field::new(
            "tags",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            false,
        ),
    ]))
}

/// `n` rows sorted by `(trace_id, start_time)`. Row `i` is built from `i` alone.
pub(crate) fn batch(n: usize) -> RecordBatch {
    let mut ids = Vec::with_capacity(n * 16);
    for i in 0..n {
        ids.extend_from_slice(&((i / 5) as u128).to_be_bytes());
    }
    let trace_ids = FixedSizeBinaryArray::try_new(16, Buffer::from_vec(ids), None).unwrap();
    let start_times: Vec<i64> = (0..n)
        .map(|i| 1_700_000_000_000_000_000 + i as i64 * 1_000_003)
        .collect();
    let start_times = TimestampNanosecondArray::from(start_times).with_timezone("UTC");
    let input_tokens = Int64Array::from_iter(
        (0..n).map(|i| (!i.is_multiple_of(7)).then(|| (i * 37 % 5000) as i64)),
    );
    let cost = Float64Array::from_iter((0..n).map(|i| {
        if i.is_multiple_of(11) {
            None
        } else if i.is_multiple_of(997) {
            Some(f64::NAN)
        } else {
            Some(i as f64 * 0.001)
        }
    }));
    let flag = BooleanArray::from((0..n).map(|i| i.is_multiple_of(3)).collect::<Vec<bool>>());
    let span_kind = StringArray::from_iter_values((0..n).map(|i| SPAN_KINDS[i % 5]));
    let status = StringArray::from_iter_values(
        (0..n).map(|i| if i.is_multiple_of(2) { "ok" } else { "error" }),
    );
    let model =
        StringArray::from_iter((0..n).map(|i| (!i.is_multiple_of(4)).then(|| MODELS[i % 6])));
    let environment = StringArray::from_iter_values((0..n).map(|_| "prod"));

    // Row i holds i % 4 tags, tag k being TAGS[(i + k) % 8].
    let mut offsets = Vec::with_capacity(n + 1);
    offsets.push(0i32);
    let mut tag_values: Vec<&str> = Vec::new();
    for i in 0..n {
        for k in 0..i % 4 {
            tag_values.push(TAGS[(i + k) % 8]);
        }
        offsets.push(tag_values.len() as i32);
    }
    let tags = ListArray::try_new(
        Arc::new(Field::new("item", DataType::Utf8, true)),
        OffsetBuffer::new(ScalarBuffer::from(offsets)),
        Arc::new(StringArray::from(tag_values)),
        None,
    )
    .unwrap();

    let columns: Vec<ArrayRef> = vec![
        Arc::new(trace_ids),
        Arc::new(start_times),
        Arc::new(input_tokens),
        Arc::new(cost),
        Arc::new(flag),
        Arc::new(span_kind),
        Arc::new(status),
        Arc::new(model),
        Arc::new(environment),
        Arc::new(tags),
    ];
    RecordBatch::try_new(schema(), columns).unwrap()
}

/// Asserts that `actual` equals `expected` bitwise: the type, the length, the validity of
/// every row, and for floats the bits of every valid value (so NaN and -0.0 compare).
pub(crate) fn assert_same(actual: &dyn Array, expected: &dyn Array) {
    use arrow::array::AsArray;
    use arrow::datatypes::{Float32Type, Float64Type};

    assert_eq!(actual.data_type(), expected.data_type());
    assert_eq!(actual.len(), expected.len());
    for i in 0..actual.len() {
        assert_eq!(
            actual.is_valid(i),
            expected.is_valid(i),
            "validity of row {i}"
        );
    }
    match actual.data_type() {
        DataType::Float64 => {
            let a = actual.as_primitive::<Float64Type>();
            let e = expected.as_primitive::<Float64Type>();
            for i in 0..a.len() {
                if a.is_valid(i) {
                    assert_eq!(a.value(i).to_bits(), e.value(i).to_bits(), "row {i}");
                }
            }
        }
        DataType::Float32 => {
            let a = actual.as_primitive::<Float32Type>();
            let e = expected.as_primitive::<Float32Type>();
            for i in 0..a.len() {
                if a.is_valid(i) {
                    assert_eq!(a.value(i).to_bits(), e.value(i).to_bits(), "row {i}");
                }
            }
        }
        _ => assert!(actual.to_data() == expected.to_data(), "arrays differ"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::AsArray;
    use arrow::datatypes::TimestampNanosecondType;

    #[test]
    fn rows_are_sorted_by_the_sort_key() {
        let b = batch(1_000);
        let ids = b.column(0).as_fixed_size_binary();
        let times = b.column(1).as_primitive::<TimestampNanosecondType>();
        for i in 1..1_000 {
            let prev = (ids.value(i - 1), times.value(i - 1));
            let cur = (ids.value(i), times.value(i));
            assert!(prev <= cur, "row {i}");
        }
    }
}
