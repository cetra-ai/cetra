#![allow(dead_code)]

//! The bench dataset (`fmt_testdata::datagen`) as Arrow batches, cut into time slices, for the
//! DataFusion tests. It copies the data steps of `bench/src/main.rs`'s `generate`: the rare
//! needle, the trace-id sample, the time slices, and the sort by `(trace_id, start_time)`.

use arrow::array::RecordBatch;
use arrow::compute::{SortColumn, concat_batches, lexsort_to_indices, take_record_batch};
use fmt_testdata::datagen;
use llmobs_core::batch::spans_to_record_batch;
use llmobs_core::schema::spans_schema;

/// Time slices of one generated dataset.
pub struct Dataset {
    /// One entry per file: its batches, sorted by `(trace_id, start_time)`, in pieces of at most
    /// 8,192 rows.
    pub files: Vec<Vec<RecordBatch>>,
    /// 100 sampled trace ids, as lowercase hex.
    pub trace_ids: Vec<String>,
    /// The `user_id` of the first span that has one.
    pub user_id: String,
    /// The stored `customer_id` metadata value of the first span that has one, JSON-quoted as
    /// stored (`"cust-N"` with the quotes).
    pub customer_id: String,
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `traces` traces of the datagen world, in `files` equal time slices.
pub fn spans_dataset(traces: usize, files: usize) -> Dataset {
    let world = datagen::World::new(7);
    let mut rng = datagen::Rng::new(42);
    let mut spans = Vec::new();
    for _ in 0..traces {
        world.trace(&mut rng, &mut spans);
    }

    // Plant a rare needle in about 20 LLM inputs, for the substring query.
    let mut planted = 0;
    let step = spans.len() / 23;
    let mut i = 7;
    while planted < 20 && i < spans.len() {
        if let Some(input) = spans.get_mut(i).and_then(|s| s.input.as_mut())
            && let Some(p) = input.find("\"content\":\"")
        {
            input.insert_str(p + 11, "zqxjneedle ");
            planted += 1;
        }
        i += step.max(1);
    }

    // Sample 100 trace ids for point lookups, as bench does.
    let trace_ids: Vec<String> = (0..100)
        .filter_map(|k| spans.get((k * 7919 * 13) % spans.len()))
        .map(|s| hex(s.trace_id.as_bytes()))
        .collect();

    let user_id = spans
        .iter()
        .find_map(|s| s.user_id.clone())
        .unwrap_or_default();
    let customer_id = spans
        .iter()
        .find_map(|s| s.metadata.get("customer_id").cloned())
        .unwrap_or_default();

    // Sort by start_time, cut into time slices, and sort each slice by (trace_id, start_time).
    spans.sort_by_key(|s| s.start_time);
    let per = spans.len().div_ceil(files.max(1)).max(1);
    let mut slices = Vec::new();
    for chunk in spans.chunks(per) {
        let mut batches = Vec::new();
        for piece in chunk.chunks(8_192) {
            batches.push(spans_to_record_batch(piece).unwrap());
        }
        let batch = concat_batches(&spans_schema(), &batches).unwrap();
        let order = lexsort_to_indices(
            &[
                SortColumn {
                    values: batch.column_by_name("trace_id").unwrap().clone(),
                    options: None,
                },
                SortColumn {
                    values: batch.column_by_name("start_time").unwrap().clone(),
                    options: None,
                },
            ],
            None,
        )
        .unwrap();
        let sorted = take_record_batch(&batch, &order).unwrap();
        let mut pieces = Vec::new();
        let mut offset = 0;
        while offset < sorted.num_rows() {
            let len = (sorted.num_rows() - offset).min(8_192);
            pieces.push(sorted.slice(offset, len));
            offset += len;
        }
        slices.push(pieces);
    }

    Dataset {
        files: slices,
        trace_ids,
        user_id,
        customer_id,
    }
}
