//! Substring search over a payload column (S4 § `Contains(needle)`).
//!
//! 1. For each run of the column's store, the gram index ([`super::gram`]) gives the frames
//!    that may hold an occurrence's first byte: one wave for the index rows of every run.
//! 2. The refs pages and those frames: one wave. In each frame, a chunk is a hit when it holds
//!    the needle, and a partial hit when it ends with a proper prefix of the needle.
//! 3. A row matches when it refers to a hit, or to a partial hit followed by chunks that hold
//!    the rest of the needle. Those chunks may sit in frames not read yet: one more wave.
//!
//! Every occurrence starts in some chunk, whose frame the index returns, so the search is exact.

use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use memchr::memmem::Finder;

use super::MAX_NEEDLE;
use super::gram::{GRAM, frames_in_all, needle_rows};
use super::header::PayloadHeader;
use super::read::{ids_of, read_header, refs_segments};
use super::store::decode_fetched_frame;
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::segment::{Kind, SegmentRef};

/// The decoded body of one frame: its raw bytes and the ranges of its chunks in them.
type Decoded = (Vec<u8>, Vec<Range<usize>>);

/// Result of a substring search over one payload column.
pub(crate) struct ContainsResult {
    /// One flag per row: whether its value contains the needle. Null rows are false.
    pub rows: Vec<bool>,
    /// Frames whose body was decoded.
    #[cfg_attr(not(test), allow(dead_code))]
    pub frames_read: u32,
}

/// One bit per chunk id of a store.
struct ChunkBits(Vec<u64>);

impl ChunkBits {
    fn new(n_chunks: u32) -> Self {
        Self(vec![0; (n_chunks as usize).div_ceil(64)])
    }

    fn set(&mut self, id: u32) {
        if let Some(word) = self.0.get_mut((id / 64) as usize) {
            *word |= 1 << (id % 64);
        }
    }

    fn get(&self, id: u32) -> bool {
        self.0
            .get((id / 64) as usize)
            .is_some_and(|word| word & (1 << (id % 64)) != 0)
    }
}

/// The frames of `header`'s store that may hold the first byte of an occurrence of `needle`,
/// ascending: from the gram index of each run that has one, and every frame of the others.
async fn candidate_frames(
    file: &StrataFile,
    header: &PayloadHeader,
    needle: &[u8],
) -> Result<Vec<usize>> {
    let store = &header.store;
    // The index rows each run needs, and the pages that hold them.
    let mut wanted: Vec<(usize, Vec<u32>)> = Vec::new();
    let mut segments: Vec<(SegmentRef, Kind)> = Vec::new();
    let mut page_at: HashMap<(usize, usize), usize> = HashMap::new();
    let mut frames = Vec::new();
    for (r, run) in store.runs.iter().enumerate() {
        match &run.index {
            Some(index) if needle.len() >= GRAM => {
                let rows = needle_rows(needle, index.rows);
                for &row in &rows {
                    let (page, _) = index.locate(row);
                    if let std::collections::hash_map::Entry::Vacant(e) = page_at.entry((r, page)) {
                        let seg = index.pages.get(page).ok_or(StrataError::Corrupt {
                            what: "payload index page",
                            offset: 0,
                        })?;
                        e.insert(segments.len());
                        segments.push((*seg, Kind::PayloadIndex));
                    }
                }
                wanted.push((r, rows));
            }
            _ => frames.extend(store.run_frames(r)),
        }
    }
    let bodies = file.fetch(&segments, ScanKind::Point).await?;
    for (r, rows) in wanted {
        let (Some(run), frame_range) = (store.runs.get(r), store.run_frames(r)) else {
            continue;
        };
        let Some(index) = &run.index else { continue };
        let mut slices: Vec<&[u8]> = Vec::with_capacity(rows.len());
        for &row in &rows {
            let (page, range) = index.locate(row);
            let corrupt = || StrataError::Corrupt {
                what: "payload index page",
                offset: index.pages.get(page).map_or(0, |p| p.offset),
            };
            let body = page_at
                .get(&(r, page))
                .and_then(|&at| bodies.get(at))
                .ok_or_else(corrupt)?;
            if body.len() != index.page_len(page) {
                return Err(corrupt());
            }
            slices.push(body.get(range).ok_or_else(corrupt)?);
        }
        for f in frames_in_all(&slices, index.frames as usize, 0)? {
            frames.push(frame_range.start + f);
        }
    }
    frames.sort_unstable();
    frames.dedup();
    Ok(frames)
}

/// Fetches and decodes the frames `indexes` of `header`'s store into `decoded`.
async fn read_frames(
    file: &StrataFile,
    header: &PayloadHeader,
    indexes: &[usize],
    decoded: &mut HashMap<usize, Decoded>,
) -> Result<()> {
    let corrupt = || StrataError::Corrupt {
        what: "payload frame",
        offset: 0,
    };
    let segments: Vec<(SegmentRef, Kind)> = indexes
        .iter()
        .map(|&f| {
            header
                .store
                .frame_segment(f)
                .map(|s| (s, Kind::PayloadFrame))
                .ok_or_else(corrupt)
        })
        .collect::<Result<_>>()?;
    let bodies = file.fetch(&segments, ScanKind::Point).await?;
    for (&f, raw) in indexes.iter().zip(bodies) {
        let entry = header.store.frames.get(f).ok_or_else(corrupt)?;
        decoded.insert(f, decode_fetched_frame(entry, raw)?);
    }
    Ok(())
}

/// The bytes of chunk `id`, when its frame is decoded.
fn chunk_bytes<'a>(
    header: &PayloadHeader,
    decoded: &'a HashMap<usize, Decoded>,
    id: u32,
) -> Result<std::result::Result<&'a [u8], usize>> {
    let corrupt = || StrataError::Corrupt {
        what: "payload chunk id",
        offset: 0,
    };
    let f = header.store.frame_of(id).ok_or_else(corrupt)?;
    let Some((raw, ranges)) = decoded.get(&f) else {
        return Ok(Err(f));
    };
    let entry = header.store.frames.get(f).ok_or_else(corrupt)?;
    let range = ranges
        .get((id - entry.first_chunk) as usize)
        .ok_or_else(corrupt)?;
    Ok(Ok(raw.get(range.clone()).ok_or_else(corrupt)?))
}

/// Whether the needle's rest `rest` starts at chunk `ids[0]` and runs on through the next ids.
/// `Err(frame)` when a chunk it needs is in a frame not decoded yet.
fn continues(
    header: &PayloadHeader,
    decoded: &HashMap<usize, Decoded>,
    ids: &[u32],
    mut rest: &[u8],
) -> Result<std::result::Result<bool, usize>> {
    for &id in ids {
        let chunk = match chunk_bytes(header, decoded, id)? {
            Ok(chunk) => chunk,
            Err(frame) => return Ok(Err(frame)),
        };
        if chunk.len() >= rest.len() {
            return Ok(Ok(chunk.starts_with(rest)));
        }
        match rest.strip_prefix(chunk) {
            Some(r) => rest = r,
            None => return Ok(Ok(false)),
        }
    }
    Ok(Ok(false))
}

/// Finds the rows of payload column `column` whose value contains `needle`. It is three waves
/// at most after the header: the index rows, then the refs pages with the candidate frames, then
/// the frames that continuations need.
pub(crate) async fn payload_contains(
    file: &StrataFile,
    column: usize,
    needle: &[u8],
) -> Result<ContainsResult> {
    let record = file
        .directory()
        .get(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("no column {column}"),
        })?;
    if record.layer != Layer::Payload {
        return Err(StrataError::Unsupported {
            what: "contains on a non-payload column".to_string(),
        });
    }
    if needle.len() > MAX_NEEDLE {
        return Err(StrataError::Unsupported {
            what: "needle longer than 64 bytes".to_string(),
        });
    }
    let at = record.payload_offset;
    let header = read_header(file, column).await?;
    let rows: Vec<u32> = (0..header.n_rows).collect();
    if needle.is_empty() {
        let (segments, pages) = refs_segments(&header, &rows)?;
        let bodies = file.fetch(&segments, ScanKind::Scan).await?;
        let ids = ids_of(&header, &pages, &bodies, &rows, at)?;
        return Ok(ContainsResult {
            rows: ids.iter().map(Option::is_some).collect(),
            frames_read: 0,
        });
    }

    let frames = candidate_frames(file, &header, needle).await?;
    // The refs pages (every row) and the candidate frames, fetched together. The frames are
    // scattered, so they are coalesced like point reads, without the scan's large gaps.
    let (ref_segments, pages) = refs_segments(&header, &rows)?;
    let mut frame_segments = Vec::with_capacity(frames.len());
    for &f in &frames {
        let seg = header.store.frame_segment(f).ok_or(StrataError::Corrupt {
            what: "payload frame",
            offset: at,
        })?;
        frame_segments.push((seg, Kind::PayloadFrame));
    }
    let (bodies, frame_bodies) = futures::try_join!(
        file.fetch(&ref_segments, ScanKind::Scan),
        file.fetch(&frame_segments, ScanKind::Point)
    )?;
    let ids = ids_of(&header, &pages, &bodies, &rows, at)?;
    let mut decoded: HashMap<usize, Decoded> = HashMap::with_capacity(frames.len());
    for (&f, raw) in frames.iter().zip(frame_bodies) {
        let entry = header.store.frames.get(f).ok_or(StrataError::Corrupt {
            what: "payload frame",
            offset: at,
        })?;
        decoded.insert(f, decode_fetched_frame(entry, raw)?);
    }

    // Hits and partial hits among the chunks of the candidate frames.
    let finder = Finder::new(needle);
    let mut hits = ChunkBits::new(header.store.n_chunks);
    let mut partial: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut partial_bits = ChunkBits::new(header.store.n_chunks);
    for &f in &frames {
        let (Some((raw, ranges)), Some(entry)) = (decoded.get(&f), header.store.frames.get(f))
        else {
            continue;
        };
        for (local, range) in ranges.iter().enumerate() {
            let id = entry.first_chunk + local as u32;
            let chunk = raw.get(range.clone()).unwrap_or_default();
            if finder.find(chunk).is_some() {
                hits.set(id);
                continue;
            }
            let ks: Vec<usize> = (1..needle.len().min(chunk.len() + 1))
                .filter(|&k| needle.get(..k).is_some_and(|p| chunk.ends_with(p)))
                .collect();
            if !ks.is_empty() {
                partial_bits.set(id);
                partial.insert(id, ks);
            }
        }
    }

    // Rows: a hit, or a partial hit whose rest follows. A rest that needs an unread frame
    // leaves the row undecided until that frame is read.
    let mut result = vec![false; ids.len()];
    let mut undecided: Vec<usize> = Vec::new();
    let mut missing: BTreeSet<usize> = BTreeSet::new();
    let decide = |row_ids: &[u32],
                  decoded: &HashMap<usize, Decoded>,
                  missing: &mut BTreeSet<usize>|
     -> Result<Option<bool>> {
        let mut unknown = false;
        for (i, &id) in row_ids.iter().enumerate() {
            if hits.get(id) {
                return Ok(Some(true));
            }
            if !partial_bits.get(id) {
                continue;
            }
            if let Some(ks) = partial.get(&id) {
                for &k in ks {
                    let rest = needle.get(k..).unwrap_or_default();
                    let next = row_ids.get(i + 1..).unwrap_or_default();
                    match continues(&header, decoded, next, rest)? {
                        Ok(true) => return Ok(Some(true)),
                        Ok(false) => {}
                        Err(frame) => {
                            missing.insert(frame);
                            unknown = true;
                        }
                    }
                }
            }
        }
        Ok(if unknown { None } else { Some(false) })
    };
    for (row, row_ids) in ids.iter().enumerate() {
        let Some(row_ids) = row_ids else { continue };
        match decide(row_ids, &decoded, &mut missing)? {
            Some(m) => {
                if let Some(slot) = result.get_mut(row) {
                    *slot = m;
                }
            }
            None => undecided.push(row),
        }
    }
    let mut frames_read = frames.len();
    if !missing.is_empty() {
        let more: Vec<usize> = missing.into_iter().collect();
        frames_read += more.len();
        read_frames(file, &header, &more, &mut decoded).await?;
        let mut still: BTreeSet<usize> = BTreeSet::new();
        for row in undecided {
            let row_ids = ids.get(row).and_then(Option::as_ref);
            let m = match row_ids {
                Some(r) => decide(r, &decoded, &mut still)?,
                None => Some(false),
            };
            // Every chunk a continuation needs is in a frame now read.
            let m = m.ok_or(StrataError::Corrupt {
                what: "payload chunk id",
                offset: at,
            })?;
            if let Some(slot) = result.get_mut(row) {
                *slot = m;
            }
        }
    }
    Ok(ContainsResult {
        rows: result,
        frames_read: u32::try_from(frames_read).unwrap_or(u32::MAX),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::payload::chunk::chunk_value;
    use crate::payload::write::PayloadStats;
    use crate::reader::read_column;
    use crate::testgen::assert_same;
    use crate::writer::{FileSummary, StrataWriter};
    use arrow::array::{BinaryArray, FixedSizeBinaryArray, RecordBatch, StringArray};
    use arrow::buffer::Buffer;
    use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use std::sync::Arc;

    /// A seeded xorshift64 generator.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// A value in `0..n`.
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }

        /// A value in `lo..=hi`.
        fn between(&mut self, lo: usize, hi: usize) -> usize {
            lo + self.below(hi - lo + 1)
        }

        /// `n` bytes drawn from `alphabet`.
        fn bytes(&mut self, n: usize, alphabet: &[u8]) -> Vec<u8> {
            (0..n)
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }
    }

    const LOWER: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    const UPPER_DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    /// `[trace_id FixedSizeBinary(16) non-null, v Utf8 nullable]`.
    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("v", DataType::Utf8, true),
        ]))
    }

    /// One batch of the schema with all-zero trace ids and the given text values.
    fn table(values: &[Option<String>]) -> RecordBatch {
        let ids =
            FixedSizeBinaryArray::try_new(16, Buffer::from_vec(vec![0u8; 16 * values.len()]), None)
                .unwrap();
        let strings: Vec<Option<&str>> = values.iter().map(|v| v.as_deref()).collect();
        let v = StringArray::from(strings);
        RecordBatch::try_new(schema(), vec![Arc::new(ids), Arc::new(v)]).unwrap()
    }

    /// One batch of the same shape with a Binary column.
    fn binary_table(values: &[Option<Vec<u8>>]) -> RecordBatch {
        let ids =
            FixedSizeBinaryArray::try_new(16, Buffer::from_vec(vec![0u8; 16 * values.len()]), None)
                .unwrap();
        let bytes: Vec<Option<&[u8]>> = values.iter().map(|v| v.as_deref()).collect();
        let v = BinaryArray::from(bytes);
        let schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("v", DataType::Binary, true),
        ]));
        RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(v)]).unwrap()
    }

    /// Writes `batch` with `config`, then opens the bytes from an in-memory store.
    async fn write_and_open(batch: &RecordBatch, config: Config) -> (StrataFile, FileSummary) {
        let mut w = StrataWriter::new(batch.schema(), &["trace_id"], config).unwrap();
        w.write(batch).unwrap();
        let (bytes, summary) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("table.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        (file, summary)
    }

    /// The payload statistics of column 1.
    fn stats_of(summary: &FileSummary) -> PayloadStats {
        summary.columns[1].payload.clone().unwrap()
    }

    /// The expected answer, by brute force over the values. The empty needle matches every
    /// non-null value.
    fn brute(values: &[Option<&[u8]>], needle: &[u8]) -> Vec<bool> {
        values
            .iter()
            .map(|v| match v {
                None => false,
                Some(bytes) => needle.is_empty() || memchr::memmem::find(bytes, needle).is_some(),
            })
            .collect()
    }

    /// The values of a Utf8 column as bytes, `None` for a null.
    fn as_bytes(values: &[Option<String>]) -> Vec<Option<&[u8]>> {
        values
            .iter()
            .map(|v| v.as_deref().map(str::as_bytes))
            .collect()
    }

    /// Searches column 1 of `file` for `needle`.
    async fn contains(file: &StrataFile, needle: &[u8]) -> ContainsResult {
        payload_contains(file, 1, needle).await.unwrap()
    }

    /// Config that stores every Utf8 or Binary column with the payload layer.
    fn payload_config() -> Config {
        Config {
            payload_min_avg: 0,
            ..Config::default()
        }
    }

    /// Role of the `k`-th message of an array.
    fn role(k: usize) -> &'static str {
        if k.is_multiple_of(2) {
            "user"
        } else {
            "assistant"
        }
    }

    /// The seeded vocabulary of the corpus: 40 lowercase words of 3 to 9 letters.
    fn vocabulary(rng: &mut Rng) -> Vec<String> {
        (0..40)
            .map(|_| {
                let n = rng.between(3, 9);
                String::from_utf8(rng.bytes(n, LOWER)).unwrap()
            })
            .collect()
    }

    /// One message `{"role":"<role>","content":"<text>"}` of 50 to 3,000 bytes. The text is
    /// vocabulary words, with a random token of 10 to 16 characters after every 8th word.
    fn message(rng: &mut Rng, vocab: &[String], role: &str) -> String {
        let target = rng.between(50, 3000);
        // `{"role":"` (9 bytes), the role, `","content":"` (13 bytes) and `"}` (2 bytes).
        let budget = target - (9 + role.len() + 13 + 2);
        let mut text = String::new();
        let mut words = 0usize;
        while text.len() < budget {
            if words > 0 {
                text.push(' ');
            }
            text.push_str(&vocab[rng.below(vocab.len())]);
            words += 1;
            if words.is_multiple_of(8) {
                text.push(' ');
                let n = rng.between(10, 16);
                text.push_str(&String::from_utf8(rng.bytes(n, ALNUM)).unwrap());
            }
        }
        text.truncate(budget);
        format!("{{\"role\":\"{role}\",\"content\":\"{text}\"}}")
    }

    /// The corpus: 400 values. Each is a fresh array of 3 to 12 messages, or (with probability
    /// 70%) the previous value with its final `]` replaced by `,`, 1 to 3 new messages and `]`.
    fn corpus(rng: &mut Rng, vocab: &[String]) -> Vec<String> {
        let mut values: Vec<String> = Vec::with_capacity(400);
        for i in 0..400 {
            let extend = i > 0 && rng.below(100) < 70;
            let value = if extend {
                let extra: Vec<String> = (0..rng.between(1, 3))
                    .map(|k| message(rng, vocab, role(k)))
                    .collect();
                let prev = &values[i - 1];
                format!("{},{}]", &prev[..prev.len() - 1], extra.join(","))
            } else {
                let msgs: Vec<String> = (0..rng.between(3, 12))
                    .map(|k| message(rng, vocab, role(k)))
                    .collect();
                format!("[{}]", msgs.join(","))
            };
            values.push(value);
        }
        values
    }

    /// The corpus as a column: every 37th value is null.
    fn corpus_column(values: &[String]) -> Vec<Option<String>> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| if i % 37 == 0 { None } else { Some(v.clone()) })
            .collect()
    }

    #[tokio::test]
    async fn contains_matches_brute_force_on_the_corpus() {
        let mut rng = Rng::new(0x5EED_0001);
        let vocab = vocabulary(&mut rng);
        let values = corpus(&mut rng, &vocab);
        let column = corpus_column(&values);
        let batch = table(&column);
        let (file, summary) = write_and_open(&batch, payload_config()).await;
        let stats = stats_of(&summary);
        assert!(stats.frames >= 10, "{} frames", stats.frames);
        assert!(stats.index_bytes > 0);
        let bytes = as_bytes(&column);
        assert_eq!(file.row_count(), column.len() as u64);

        // Present needles: substrings of random non-null values.
        let non_null: Vec<usize> = (0..column.len()).filter(|&i| column[i].is_some()).collect();
        for _ in 0..100 {
            let row = non_null[rng.below(non_null.len())];
            let value = bytes[row].unwrap();
            let len = rng.between(1, 64.min(value.len()));
            let start = rng.below(value.len() - len + 1);
            let needle = &value[start..start + len];
            let got = contains(&file, needle).await;
            assert_eq!(got.rows, brute(&bytes, needle), "needle {needle:?}");
        }

        // Absent needles: uppercase and digits, which never occur in the corpus. The index
        // keeps most of them from reading most frames.
        let mut tested = 0;
        let mut frames_read = 0u32;
        while tested < 100 {
            let n = rng.between(8, 32);
            let needle = rng.bytes(n, UPPER_DIGITS);
            if brute(&bytes, &needle).iter().any(|&m| m) {
                continue;
            }
            tested += 1;
            let got = contains(&file, &needle).await;
            assert!(
                got.rows.iter().all(|&m| !m),
                "absent needle {needle:?} matched"
            );
            frames_read += got.frames_read;
        }
        assert!(
            frames_read * 10 < 100 * stats.frames,
            "absent needles read {frames_read} frames of {} each",
            stats.frames
        );

        // Boundary needles: a needle that spans the end of chunk j.
        let multi: Vec<usize> = non_null
            .iter()
            .copied()
            .filter(|&i| chunk_value(bytes[i].unwrap()).len() >= 2)
            .collect();
        for i in 0..100 {
            let row = multi[rng.below(multi.len())];
            let value = bytes[row].unwrap();
            let ranges = chunk_value(value);
            let j = rng.below(ranges.len() - 1);
            let p = ranges[j].end;
            let a_max = (p - ranges[j].start).min(32);
            let b_max = (value.len() - p).min(32);
            let a = match i % 4 {
                0 => 1,
                1 => 5,
                2 => rng.between(1, a_max),
                _ => rng.between(a_max.min(5), a_max),
            }
            .min(a_max);
            let b = if i % 5 == 0 { 1 } else { rng.between(1, b_max) };
            let needle = &value[p - a..p + b];
            let got = contains(&file, needle).await;
            assert_eq!(
                got.rows,
                brute(&bytes, needle),
                "boundary needle {needle:?}"
            );
        }
    }

    #[tokio::test]
    async fn long_empty_and_wrong_layer_needles() {
        let batch = table(&[Some("abc".to_string()), None]);
        let (file, _) = write_and_open(&batch, payload_config()).await;
        let long = vec![b'a'; MAX_NEEDLE + 1];
        assert!(matches!(
            payload_contains(&file, 1, &long).await,
            Err(StrataError::Unsupported { .. })
        ));
        let empty = contains(&file, b"").await;
        assert_eq!(empty.rows, vec![true, false]);
        assert_eq!(empty.frames_read, 0);
        // Column 0 is narrow.
        assert_eq!(file.directory()[0].layer, Layer::Narrow);
        assert!(matches!(
            payload_contains(&file, 0, b"ab").await,
            Err(StrataError::Unsupported { .. })
        ));
    }

    #[tokio::test]
    async fn nulls_never_match_and_empty_values_match_only_the_empty_needle() {
        let column = vec![
            None,
            Some(String::new()),
            Some("abc".repeat(100)),
            None,
            Some(String::new()),
        ];
        let batch = table(&column);
        let (file, _) = write_and_open(&batch, payload_config()).await;
        let bytes = as_bytes(&column);
        let needles: [&[u8]; 5] = [b"a", b"", b"abcab", b"zz", b"cabcabca"];
        for needle in needles {
            let got = contains(&file, needle).await;
            assert_eq!(got.rows, brute(&bytes, needle), "needle {needle:?}");
        }
    }

    #[tokio::test]
    async fn needles_across_shared_chunks_match_in_every_row() {
        // Values reuse each other's chunks in other orders, so a chunk is followed by different
        // chunks in different rows (pairs that are not consecutive ids).
        let parts: Vec<String> = (0..12u64)
            .map(|k| {
                let mut rng = Rng::new(k + 11);
                format!(
                    "{{\"t\":\"{}\"}}",
                    String::from_utf8(rng.bytes(700, LOWER)).unwrap()
                )
            })
            .collect();
        let mut rng = Rng::new(99);
        let column: Vec<Option<String>> = (0..150)
            .map(|_| {
                let n = rng.between(2, 6);
                let picked: Vec<&str> = (0..n)
                    .map(|_| parts[rng.below(parts.len())].as_str())
                    .collect();
                Some(format!("[{}]", picked.join(",")))
            })
            .collect();
        let batch = table(&column);
        let (file, _) = write_and_open(&batch, payload_config()).await;
        let bytes = as_bytes(&column);
        for _ in 0..200 {
            let value = bytes[rng.below(bytes.len())].unwrap();
            let ranges = chunk_value(value);
            if ranges.len() < 2 {
                continue;
            }
            let p = ranges[rng.below(ranges.len() - 1)].end;
            let a = rng.between(1, 30).min(p);
            let b = rng.between(1, 30).min(value.len() - p);
            let needle = &value[p - a..p + b];
            assert_eq!(
                contains(&file, needle).await.rows,
                brute(&bytes, needle),
                "{needle:?}"
            );
        }
    }

    #[tokio::test]
    async fn binary_columns_are_searched_too() {
        let mut rng = Rng::new(5);
        let column: Vec<Option<Vec<u8>>> = (0..80)
            .map(|i| {
                let n = rng.between(300, 3_000);
                (i % 9 != 0).then(|| rng.bytes(n, ALNUM))
            })
            .collect();
        let batch = binary_table(&column);
        let (file, _) = write_and_open(&batch, payload_config()).await;
        let bytes: Vec<Option<&[u8]>> = column.iter().map(|v| v.as_deref()).collect();
        for _ in 0..50 {
            let value = column[1 + rng.below(7)].as_ref().unwrap();
            let len = rng.between(1, 40);
            let start = rng.below(value.len() - len);
            let needle = &value[start..start + len];
            assert_eq!(contains(&file, needle).await.rows, brute(&bytes, needle));
        }
    }

    #[tokio::test]
    async fn the_column_reads_back_after_a_search() {
        let mut rng = Rng::new(0x5EED_0002);
        let vocab = vocabulary(&mut rng);
        let values = corpus(&mut rng, &vocab);
        let column = corpus_column(&values[..100]);
        let batch = table(&column);
        let (file, _) = write_and_open(&batch, payload_config()).await;
        let _ = contains(&file, b"role").await;
        let back = read_column(&file, 1).await.unwrap();
        assert_same(back.as_ref(), batch.column(1).as_ref());
    }
}
