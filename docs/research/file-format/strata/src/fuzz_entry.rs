//! Fuzz entry points. Each takes arbitrary bytes and must not panic.
//!
//! Every array a target decodes must also pass arrow's full validation (offsets, views,
//! nulls and UTF-8): the string decoders build some arrays without arrow's own UTF-8 check,
//! so a wrong shortcut there shows up as a panic here, not as undefined behaviour later.
//!
//! - [`file`]: a whole Strata file, opened and read through every read path.
//! - [`narrow_chunk`]: one narrow chunk body, with its block entry and dictionary.
//! - [`payload_values`]: a payload store, its decoded frames, and rows of chunk ids.
//!
//! Under `cfg(fuzzing)` the segment and hot-tail checksums are not compared, so a mutated file
//! reaches the decoders instead of stopping at its first checksum.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // a panic is the finding

use std::sync::Arc;

use arrow::array::{Array, RecordBatch};
use arrow::datatypes::{DataType, Field};
use futures::executor::block_on;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::format::Cursor;
use crate::hot::Layer;
use crate::narrow::{BlockEntry, DictKind, Dictionary, decode_chunk, decode_chunk_view};
use crate::payload::{FrameBodies, PayloadHeader, PayloadStore, build_payload_values};
use crate::{Config, Predicate, ScanKind, ScanRequest, StrataFile, read_column};

/// Largest number of rows of chunk ids [`payload_values`] builds.
const MAX_PAYLOAD_ROWS: usize = 4096;
/// Largest total raw frame bytes [`payload_values`] takes.
const MAX_PAYLOAD_RAW: u64 = 4 << 20;

/// Silences the panic hook that libFuzzer installs. That hook aborts on every panic, including
/// the ones [`crate::format::read_ipc_stream`] catches, so a caught decoder panic would look
/// like a crash. An uncaught panic still aborts: the entries are called from libFuzzer's
/// `extern "C"` wrapper, which cannot unwind. A no-op outside fuzzing builds.
fn quiet_caught_panics() {
    #[cfg(fuzzing)]
    {
        static HOOK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        HOOK.get_or_init(|| std::panic::set_hook(Box::new(|_| {})));
    }
}

/// Panics unless `array` passes arrow's full validation.
fn check(what: &str, array: &dyn Array) {
    if let Err(e) = array.to_data().validate_full() {
        panic!("{what}: a decoded array is invalid: {e}");
    }
}

fn check_batches(what: &str, batches: &[RecordBatch]) {
    for batch in batches {
        for column in batch.columns() {
            check(what, column.as_ref());
        }
    }
}

/// Reads input fields; missing bytes read as zero, so every input has every field.
struct Input<'a> {
    data: &'a [u8],
}

impl<'a> Input<'a> {
    fn bytes(&mut self, n: usize) -> &'a [u8] {
        let (head, rest) = self.data.split_at(n.min(self.data.len()));
        self.data = rest;
        head
    }

    fn u8(&mut self) -> u8 {
        self.bytes(1).first().copied().unwrap_or(0)
    }

    fn u32(&mut self) -> u32 {
        let mut buf = [0; 4];
        for (b, v) in buf.iter_mut().zip(self.bytes(4)) {
            *b = *v;
        }
        u32::from_le_bytes(buf)
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Opens `data` as a Strata file and reads it through every read path: each column, a full
/// scan with and without views, a `Contains` scan of each payload column, and a key lookup.
pub fn file(data: &[u8]) {
    quiet_caught_panics();
    let _ = file_decodes(data);
}

/// [`file`]; `true` when the file opened and every column read.
fn file_decodes(data: &[u8]) -> bool {
    block_on(async {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("fuzz.strata");
        store
            .put(&path, PutPayload::from(data.to_vec()))
            .await
            .expect("an in-memory put succeeds");
        let Ok(file) = StrataFile::open(store, path, None, &Config::default()).await else {
            return false;
        };
        let n_columns = file.schema().fields().len();
        let mut all_read = true;
        for column in 0..n_columns {
            match read_column(&file, column).await {
                Ok(array) => check("read_column", array.as_ref()),
                Err(_) => all_read = false,
            }
        }
        let projection: Vec<usize> = (0..n_columns).collect();
        for views in [false, true] {
            let req = ScanRequest {
                projection: projection.clone(),
                predicate: None,
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views,
                top: None,
                row_filter: None,
            };
            if let Ok(batches) = file.scan(&req).await {
                check_batches("scan", &batches);
            }
        }
        let payload_columns: Vec<usize> = file
            .directory()
            .iter()
            .enumerate()
            .filter(|(i, record)| *i < n_columns && record.layer == Layer::Payload)
            .map(|(i, _)| i)
            .collect();
        for column in payload_columns {
            let req = ScanRequest {
                projection: vec![column],
                predicate: Some(Predicate::Contains(column, b"e".to_vec())),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Point,
                views: true,
                top: None,
                row_filter: None,
            };
            if let Ok(batches) = file.scan(&req).await {
                check_batches("contains scan", &batches);
            }
        }
        let _ = file.lookup(&[0; 16]).await;
        all_read
    })
}

/// Decodes one narrow chunk. Layout: byte 0 picks a field of the test schema, byte 1 has
/// flags (bit 0: give a dictionary, bit 1: no body), then a 56-byte block entry, a `u32`
/// dictionary length and the dictionary bytes, and the rest is the chunk body.
pub fn narrow_chunk(data: &[u8]) {
    quiet_caught_panics();
    let _ = narrow_chunk_decodes(data);
}

/// [`narrow_chunk`]; `true` when the chunk decoded.
fn narrow_chunk_decodes(data: &[u8]) -> bool {
    let mut input = Input { data };
    let fields = narrow_fields();
    let Some(field) = fields.get(usize::from(input.u8()) % fields.len()) else {
        return false;
    };
    let flags = input.u8();
    let Ok(entry) = BlockEntry::decode(&mut Cursor::new(input.bytes(56), 0)) else {
        return false;
    };
    let dict_len = input.u32() as usize;
    let dict_bytes = input.bytes(dict_len);
    let dict = if flags & 1 != 0 {
        let Some(kind) = DictKind::of(field.data_type()) else {
            return false;
        };
        match Dictionary::decode(dict_bytes, 0, kind) {
            Ok(dict) => Some(dict),
            Err(_) => return false,
        }
    } else {
        None
    };
    let body = (flags & 2 == 0).then_some(input.data);
    let mut decodes = true;
    for view in [false, true] {
        let decoded = if view {
            decode_chunk_view(&entry, body, field, dict.as_ref())
        } else {
            decode_chunk(&entry, body, field, dict.as_ref())
        };
        match decoded {
            Ok(array) => {
                check("narrow chunk", array.as_ref());
                assert_eq!(array.len(), entry.n_rows as usize, "narrow chunk row count");
            }
            Err(_) => decodes = false,
        }
    }
    decodes
}

/// The fields [`narrow_chunk`] decodes as: those of the test schema, which cover every narrow
/// type the writer stores.
fn narrow_fields() -> Vec<Field> {
    crate::testgen::wide_schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect()
}

/// Rebuilds payload values from decoded frames. Layout: byte 0 has flags (bit 0: `Utf8View`
/// output, bit 1: a Binary column instead of Utf8), then a `u32` store length and the store
/// body, then each frame's raw body in store order (`raw_len` bytes each), then the rows: a
/// byte `t` per row, a null row when `t` is 255, else `t % 8` chunk ids of 4 bytes each.
pub fn payload_values(data: &[u8]) {
    quiet_caught_panics();
    let _ = payload_values_decode(data);
}

/// [`payload_values`]; `true` when the values were built.
fn payload_values_decode(data: &[u8]) -> bool {
    let mut input = Input { data };
    let flags = input.u8();
    let store_len = input.u32() as usize;
    let Ok(store) = PayloadStore::decode(input.bytes(store_len), 0) else {
        return false;
    };
    let raw_total: u64 = store.frames.iter().map(|f| u64::from(f.raw_len)).sum();
    if raw_total > MAX_PAYLOAD_RAW {
        return false;
    }
    let bodies: Vec<Vec<u8>> = store
        .frames
        .iter()
        .map(|f| input.bytes(f.raw_len as usize).to_vec())
        .collect();
    let indexes: Vec<usize> = (0..store.frames.len()).collect();
    let mut ids: Vec<Option<Vec<u32>>> = Vec::new();
    while !input.is_empty() && ids.len() < MAX_PAYLOAD_ROWS {
        let t = input.u8();
        if t == 255 {
            ids.push(None);
        } else {
            ids.push(Some((0..t % 8).map(|_| input.u32()).collect()));
        }
    }
    let header = PayloadHeader {
        n_rows: u32::try_from(ids.len()).unwrap_or(u32::MAX),
        ref_pages: Vec::new(),
        store: Arc::new(store),
    };
    let Ok(frames) = FrameBodies::decoded(&header, &indexes, bodies) else {
        return false;
    };
    let data_type = if flags & 2 != 0 {
        DataType::Binary
    } else {
        DataType::Utf8
    };
    let field = Field::new("payload", data_type, true);
    let views = flags & 1 != 0;
    match build_payload_values(&header, &frames, &ids, &field, 0, views) {
        Ok(array) => {
            check("payload values", array.as_ref());
            assert_eq!(array.len(), ids.len(), "payload row count");
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::StrataWriter;
    use crate::format::Out;
    use crate::payload::{frame_segments_of, ids_of, read_header, refs_segments};
    use crate::segment::{Kind, SegmentRef};

    /// A fuzz target: the name of its `fuzz/fuzz_targets` file, and its entry point.
    type Target = (&'static str, fn(&[u8]));

    const TARGETS: [Target; 3] = [
        ("file", file),
        ("narrow_chunk", narrow_chunk),
        ("payload_values", payload_values),
    ];

    fn fuzz_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz")
    }

    /// The files in `dir`, sorted by name. A missing directory has none.
    fn sorted_files(dir: &std::path::Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        files
    }

    /// Every corpus and regression input of every target runs without a panic. The corpus of
    /// each target must not be empty.
    #[test]
    fn the_fuzz_corpora_and_regressions_replay() {
        for (target, entry) in TARGETS {
            let corpus = sorted_files(&fuzz_dir().join("corpus").join(target));
            assert!(!corpus.is_empty(), "no corpus for {target}");
            let regressions = sorted_files(&fuzz_dir().join("regressions").join(target));
            for path in corpus.into_iter().chain(regressions) {
                let data = std::fs::read(&path).unwrap();
                let outcome = std::panic::catch_unwind(|| entry(&data));
                assert!(outcome.is_ok(), "{target}: {} panicked", path.display());
            }
        }
    }

    /// Seed files: small files of the test schema, one with the default chunking and one cut
    /// into many chunks, and one with payload columns.
    fn seed_files() -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for (rows, chunk_max_rows) in [(40, 2_048), (300, 64)] {
            let batch = crate::testgen::wide_batch(rows);
            let config = Config {
                chunk_max_rows,
                ..Config::default()
            };
            let mut w =
                StrataWriter::new(batch.schema(), &["trace_id", "start_time"], config).unwrap();
            w.write(&batch).unwrap();
            out.push(w.finish().unwrap().0.to_vec());
        }
        out.push(payload_file());
        out
    }

    /// A file with a Utf8 and a Binary payload column: new values, repeats and extensions of
    /// earlier values, non-ASCII text, empty values and nulls, over several frames.
    fn payload_file() -> Vec<u8> {
        use arrow::array::{BinaryArray, FixedSizeBinaryArray, StringArray};
        use arrow::datatypes::Schema;

        let n = 160usize;
        let schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("text", DataType::Utf8, true),
            Field::new("blob", DataType::Binary, true),
        ]));
        let ids: Vec<[u8; 16]> = (0..n).map(|i| (i as u128).to_be_bytes()).collect();
        let mut text: Vec<Option<String>> = Vec::with_capacity(n);
        for i in 0..n {
            let value = match i % 6 {
                0 => Some(format!("new {i} {}", "ab".repeat(i * 17 % 1_500))),
                1 if i > 12 => text[i - 7].clone(),
                2 if i > 12 => text[i - 11]
                    .clone()
                    .map(|v| format!("{v} then {i} {}", "é日😀".repeat(i % 300))),
                3 => Some(String::new()),
                4 => None,
                _ => Some(format!("short {i} ünïcode {}", "x".repeat(300))),
            };
            text.push(value);
        }
        let blobs: Vec<Option<Vec<u8>>> = (0..n)
            .map(|i| (i % 5 != 4).then(|| (0..300 + i * 13).map(|j| (j * 7 + i) as u8).collect()))
            .collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(FixedSizeBinaryArray::try_from_iter(ids.iter()).unwrap()),
                Arc::new(StringArray::from(text)),
                Arc::new(BinaryArray::from_iter(blobs)),
            ],
        )
        .unwrap();
        let mut w = StrataWriter::new(schema, &["trace_id"], Config::default()).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap().0
    }

    async fn open(bytes: &[u8]) -> StrataFile {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("seed.strata");
        store
            .put(&path, PutPayload::from(bytes.to_vec()))
            .await
            .unwrap();
        StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap()
    }

    /// The `narrow_chunk` inputs of every chunk of every narrow column of `file`.
    async fn narrow_seeds(file: &StrataFile, out: &mut Vec<Vec<u8>>) {
        let fields = narrow_fields();
        for (column, record) in file.directory().iter().enumerate() {
            if record.layer != Layer::Narrow || column >= file.schema().fields().len() {
                continue;
            }
            let field = file.schema().field(column).clone();
            let index = fields.iter().position(|f| f == &field).unwrap();
            let dict = file.dictionary(column).await.unwrap();
            let mut dict_bytes = Out(Vec::new());
            if let Some(dict) = &dict {
                dict.encode(&mut dict_bytes).unwrap();
            }
            for entry in file.block_entries(column).await.unwrap().iter() {
                let mut seed = vec![u8::try_from(index).unwrap()];
                seed.push(u8::from(dict.is_some()) | (u8::from(entry.segment_len == 0) << 1));
                let mut e = Out(Vec::new());
                entry.encode(&mut e);
                seed.extend_from_slice(&e.0);
                seed.extend_from_slice(&u32::try_from(dict_bytes.0.len()).unwrap().to_le_bytes());
                seed.extend_from_slice(&dict_bytes.0);
                if entry.segment_len > 0 {
                    let segment = SegmentRef {
                        offset: entry.segment_offset,
                        len: entry.segment_len,
                    };
                    let body = file
                        .fetch(&[(segment, Kind::NarrowChunk)], ScanKind::Scan)
                        .await
                        .unwrap();
                    seed.extend_from_slice(&body[0]);
                }
                out.push(seed);
            }
        }
    }

    /// The `payload_values` input of every payload column of `file`: its store, its frames and
    /// its rows, for the column's own type, with and without views.
    async fn payload_seeds(file: &StrataFile, out: &mut Vec<Vec<u8>>) {
        for (column, record) in file.directory().iter().enumerate() {
            if record.layer != Layer::Payload {
                continue;
            }
            let header = read_header(file, column).await.unwrap();
            let rows: Vec<u32> = (0..header.n_rows).collect();
            let (segments, pages) = refs_segments(&header, &rows).unwrap();
            let page_bodies = file.fetch(&segments, ScanKind::Scan).await.unwrap();
            let ids = ids_of(&header, &pages, &page_bodies, &rows, 0).unwrap();
            let indexes: Vec<usize> = (0..header.store.frames.len()).collect();
            let frames = file
                .fetch(
                    &frame_segments_of(&header, &indexes).unwrap(),
                    ScanKind::Scan,
                )
                .await
                .unwrap();
            let mut store = Out(Vec::new());
            header.store.encode(&mut store).unwrap();
            let binary = file.schema().field(column).data_type() == &DataType::Binary;
            for views in [false, true] {
                let mut seed = vec![u8::from(views) | (u8::from(binary) << 1)];
                seed.extend_from_slice(&u32::try_from(store.0.len()).unwrap().to_le_bytes());
                seed.extend_from_slice(&store.0);
                for f in &frames {
                    seed.extend_from_slice(f);
                }
                for row in &ids {
                    match row {
                        None => seed.push(255),
                        Some(row) => {
                            // Longer rows are cut to the 7 ids the layout can say.
                            let row = &row[..row.len().min(7)];
                            seed.push(u8::try_from(row.len()).unwrap());
                            for id in row {
                                seed.extend_from_slice(&id.to_le_bytes());
                            }
                        }
                    }
                }
                out.push(seed);
            }
        }
    }

    /// The seeds of every target, from [`seed_files`].
    fn seeds() -> [(&'static str, Vec<Vec<u8>>); 3] {
        let mut seeds = [
            ("file", Vec::new()),
            ("narrow_chunk", Vec::new()),
            ("payload_values", Vec::new()),
        ];
        for bytes in seed_files() {
            let file = block_on(open(&bytes));
            block_on(narrow_seeds(&file, &mut seeds[1].1));
            block_on(payload_seeds(&file, &mut seeds[2].1));
            seeds[0].1.push(bytes);
        }
        seeds
    }

    /// Writes the seed corpus of every target into `fuzz/corpus/<target>/seed-<n>`. Run by
    /// hand: `cargo test --lib -- --ignored write_seed_corpus`.
    #[test]
    #[ignore = "writes the seed corpus"]
    fn write_seed_corpus() {
        for (target, inputs) in seeds() {
            let dir = fuzz_dir().join("corpus").join(target);
            std::fs::create_dir_all(&dir).unwrap();
            for (i, input) in inputs.iter().enumerate() {
                std::fs::write(dir.join(format!("seed-{i:03}")), input).unwrap();
            }
        }
    }

    /// The seeds are valid inputs: each target decodes them without an error, so the fuzzer
    /// starts from inputs that reach the decoders.
    #[test]
    fn the_seeds_decode() {
        let [(_, files), (_, narrow), (_, payload)] = seeds();
        for (i, seed) in files.iter().enumerate() {
            assert!(file_decodes(seed), "file seed {i}");
        }
        assert!(narrow.len() > 20, "{} narrow seeds", narrow.len());
        for (i, seed) in narrow.iter().enumerate() {
            assert!(narrow_chunk_decodes(seed), "narrow seed {i}");
        }
        assert!(payload.len() >= 4, "{} payload seeds", payload.len());
        for (i, seed) in payload.iter().enumerate() {
            assert!(payload_values_decode(seed), "payload seed {i}");
        }
    }
}
