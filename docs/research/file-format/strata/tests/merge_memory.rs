//! Peak memory of `merge` on 4 inputs of 1,000,000 rows in total (design S10). Run in release:
//! `cargo test --release -- --ignored merge_memory --nocapture`.

#![allow(dead_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../src/testgen.rs"]
mod testgen;

use arrow::array::{Array, RecordBatch, UInt32Array};
use arrow::compute::take_record_batch;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use strata::{Config, StrataFile, StrataWriter, merge, read_column};

/// Counts the bytes that are allocated and not yet freed, and the peak of that count.
struct Counting;

static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[allow(unsafe_code)]
// SAFETY: every method forwards its arguments to `System` unchanged, and only adds counters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let cur = CUR.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(cur, Ordering::Relaxed);
        // SAFETY: the caller's layout is forwarded to `System`, which has the same contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let cur = CUR.fetch_add(size, Ordering::Relaxed) + size;
        PEAK.fetch_max(cur, Ordering::Relaxed);
        // SAFETY: the caller's layout is forwarded to `System`, which has the same contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CUR.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: `ptr` and `layout` are the ones `System` gave out, as the caller guarantees.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        CUR.fetch_sub(layout.size(), Ordering::Relaxed);
        let cur = CUR.fetch_add(new_size, Ordering::Relaxed) + new_size;
        PEAK.fetch_max(cur, Ordering::Relaxed);
        // SAFETY: `ptr` and `layout` are the ones `System` gave out, as the caller guarantees.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

const SORT: [&str; 2] = ["trace_id", "start_time"];
const ROWS: usize = 1_000_000;

/// Writes `rows` with `SORT` and opens it from an in-memory store under `name`.
async fn open_part(
    store: &Arc<dyn ObjectStore>,
    name: &str,
    rows: &RecordBatch,
) -> Arc<StrataFile> {
    let mut w = StrataWriter::new(rows.schema(), &SORT, Config::default()).unwrap();
    w.write(rows).unwrap();
    let (bytes, _) = w.finish().unwrap();
    let path = Path::from(name);
    store.put(&path, PutPayload::from(bytes)).await.unwrap();
    Arc::new(
        StrataFile::open(Arc::clone(store), path, None, &Config::default())
            .await
            .unwrap(),
    )
}

#[tokio::test]
#[ignore]
async fn merge_memory_bound() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut inputs = Vec::new();
    {
        let rows = testgen::wide_batch(ROWS);
        for part in 0..4u32 {
            let picked: Vec<u32> = (0..ROWS as u32).filter(|i| i % 4 == part).collect();
            let piece = take_record_batch(&rows, &UInt32Array::from(picked)).unwrap();
            inputs.push(open_part(&store, &format!("part{part}.strata"), &piece).await);
        }
    }

    // The largest decoded column: the summed Arrow size of one column over all inputs.
    let mut largest = 0usize;
    let mut sizes = Vec::new();
    for column in 0..inputs[0].schema().fields().len() {
        let mut total = 0usize;
        for file in &inputs {
            let array = read_column(file, column).await.unwrap();
            total += array.get_array_memory_size();
        }
        sizes.push(total);
        largest = largest.max(total);
    }
    for (column, size) in sizes.iter().enumerate() {
        println!("column {column}: {size} bytes decoded over all inputs");
    }

    let base = CUR.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let (bytes, summary) = merge(&inputs, &SORT, Config::default()).await.unwrap();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    assert_eq!(summary.rows, ROWS as u64);

    let rows = ROWS;
    let two_largest = 2 * largest;
    let per_row = 48 * rows;
    let output = largest / 2;
    let bound = two_largest + per_row + output;
    println!("largest: {largest} bytes");
    println!("peak: {peak} bytes (output file {} bytes)", bytes.len());
    println!(
        "bound: {bound} bytes = 2 x largest {two_largest} + 48 x rows {per_row} + largest/2 {output}"
    );
    assert!(
        peak <= bound,
        "merge peak {peak} is above the bound {bound}; see the per-column sizes above"
    );
}
