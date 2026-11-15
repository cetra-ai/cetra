//! Substring search over a payload column (S4 § `Contains(needle)`).
//!
//! The filters segment holds a rollup over the trigrams of the whole column and one trigram
//! filter per frame. A needle is first looked up in the rollup; then only the frames whose
//! filters hold every trigram of the needle are decoded. A needle that crosses a chunk boundary
//! is matched in the last bytes of the left chunk (its tail, from the edge groups) followed by
//! the first bytes of the right chunk (its head, from its frame).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use memchr::memmem::Finder;

use super::frames::{decode_fetched_frame, frame_of};
use super::header::PayloadHeader;
use super::read::{read_header, read_rows};
use super::trigram::{add_trigrams, filter_bit, get_bit};
use super::{EDGE, EDGE_ENTRY_LEN, EDGE_GROUP_CHUNKS, FILTER_MIN_BITS, MAX_NEEDLE, ROLLUP_BITS};
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::format::Cursor;
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::segment::{Kind, SegmentRef};

/// The decoded body of one frame: its raw bytes and the ranges of its chunks in them.
type Decoded = (Vec<u8>, Vec<Range<usize>>);

/// Frame-table offset of a frame without a filter.
const NO_FILTER: u32 = u32::MAX;

/// The filters of one payload column: the rollup, then one filter per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Filters {
    /// The rollup bitmap, `ROLLUP_BITS / 8` bytes; `None` when it was dropped.
    pub rollup: Option<Vec<u8>>,
    /// One entry per frame, in frame order; `None` when the frame has no filter.
    pub frames: Vec<Option<FrameFilter>>,
}

/// The trigram filter of one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameFilter {
    /// Number of bits, a multiple of 64 and at least `FILTER_MIN_BITS`.
    pub bits: u32,
    /// `bits / 8` bytes.
    pub bitmap: Vec<u8>,
}

impl Filters {
    /// Writes the body of a filters segment: the rollup, the frame table, then the bitmaps of
    /// the frames in frame order.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let unsupported = |what: &str| StrataError::Unsupported {
            what: what.to_string(),
        };
        match &self.rollup {
            Some(bitmap) => {
                if bitmap.len() != (ROLLUP_BITS / 8) as usize {
                    return Err(unsupported("rollup bitmap length"));
                }
                out.extend_from_slice(&ROLLUP_BITS.to_le_bytes());
                out.extend_from_slice(bitmap);
            }
            None => out.extend_from_slice(&0u32.to_le_bytes()),
        }
        let count = u32::try_from(self.frames.len())
            .map_err(|_| unsupported("more than 4 billion frames"))?;
        out.extend_from_slice(&count.to_le_bytes());

        let mut table = Vec::with_capacity(8 * self.frames.len());
        let mut area = Vec::new();
        for frame in &self.frames {
            match frame {
                Some(filter) => {
                    if u64::try_from(filter.bitmap.len()).ok() != Some(u64::from(filter.bits) / 8) {
                        return Err(unsupported("filter bitmap length"));
                    }
                    let offset = u32::try_from(area.len())
                        .ok()
                        .filter(|&o| o != NO_FILTER)
                        .ok_or_else(|| unsupported("payload filters of 4 GiB"))?;
                    table.extend_from_slice(&offset.to_le_bytes());
                    table.extend_from_slice(&filter.bits.to_le_bytes());
                    area.extend_from_slice(&filter.bitmap);
                }
                None => {
                    table.extend_from_slice(&NO_FILTER.to_le_bytes());
                    table.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        out.extend_from_slice(&table);
        out.extend_from_slice(&area);
        Ok(())
    }

    /// Decodes and checks a filters body. `n_frames` is the frame count of the header.
    pub(crate) fn decode(bytes: &[u8], offset: u64, n_frames: u32) -> Result<Filters> {
        let corrupt = move || StrataError::Corrupt {
            what: "payload filters",
            offset,
        };
        let mut c = Cursor::new(bytes, offset);
        let rollup = match c.u32("payload filters")? {
            0 => None,
            ROLLUP_BITS => Some(
                c.bytes((ROLLUP_BITS / 8) as usize, "payload filters")?
                    .to_vec(),
            ),
            _ => return Err(corrupt()),
        };
        let n = c.u32("payload filters")?;
        if n != n_frames {
            return Err(corrupt());
        }
        let n = n as usize;
        // The table must fit in the input before anything is allocated for it.
        if n.checked_mul(8).is_none_or(|table| table > c.remaining()) {
            return Err(corrupt());
        }
        let mut table = Vec::with_capacity(n);
        for _ in 0..n {
            let bitmap_offset = c.u32("payload filters")?;
            let bits = c.u32("payload filters")?;
            table.push((bitmap_offset, bits));
        }
        let rest = c.remaining();
        let area = c.bytes(rest, "payload filters")?;

        let mut frames = Vec::with_capacity(n);
        let mut next: usize = 0;
        for (bitmap_offset, bits) in table {
            if bitmap_offset == NO_FILTER {
                if bits != 0 {
                    return Err(corrupt());
                }
                frames.push(None);
                continue;
            }
            if bits < FILTER_MIN_BITS
                || !bits.is_multiple_of(64)
                || u64::from(bitmap_offset) != next as u64
            {
                return Err(corrupt());
            }
            let len = (bits / 8) as usize;
            let end = next.checked_add(len).ok_or_else(corrupt)?;
            let bitmap = area.get(next..end).ok_or_else(corrupt)?;
            frames.push(Some(FrameFilter {
                bits,
                bitmap: bitmap.to_vec(),
            }));
            next = end;
        }
        // Bytes past the last bitmap are not part of any filter.
        if next != area.len() {
            return Err(corrupt());
        }
        Ok(Filters { rollup, frames })
    }
}

/// Writes the body of one edge group: one 64-byte entry per tail, `u8 tail_len`, the tail,
/// then zero padding up to 64 bytes. Tails are at most `EDGE` bytes long by construction.
pub(crate) fn encode_edge_group(tails: &[Vec<u8>], out: &mut Vec<u8>) {
    for tail in tails {
        let kept = tail.get(..tail.len().min(EDGE)).unwrap_or_default();
        out.push(kept.len() as u8);
        out.extend_from_slice(kept);
        out.resize(out.len() + (EDGE - kept.len()), 0);
    }
}

/// Decodes an edge group of `n` entries. `offset` is the group's file offset, used in errors.
pub(crate) fn decode_edge_group(bytes: &[u8], offset: u64, n: usize) -> Result<Vec<Vec<u8>>> {
    let corrupt = move || StrataError::Corrupt {
        what: "payload edge group",
        offset,
    };
    if n.checked_mul(EDGE_ENTRY_LEN) != Some(bytes.len()) {
        return Err(corrupt());
    }
    let (entries, rest) = bytes.as_chunks::<EDGE_ENTRY_LEN>();
    if !rest.is_empty() {
        return Err(corrupt());
    }
    let mut tails = Vec::with_capacity(n);
    for entry in entries {
        let (len, rest) = entry.split_first().ok_or_else(corrupt)?;
        let len = usize::from(*len);
        if len > EDGE {
            return Err(corrupt());
        }
        tails.push(rest.get(..len).ok_or_else(corrupt)?.to_vec());
    }
    Ok(tails)
}

/// Result of a substring search over one payload column.
pub(crate) struct ContainsResult {
    /// One flag per row: whether its value contains the needle. Null rows are false.
    pub rows: Vec<bool>,
    /// Frames whose body was decoded.
    #[cfg_attr(not(test), allow(dead_code))]
    pub frames_read: u32,
    /// Edge groups that were read.
    #[cfg_attr(not(test), allow(dead_code))]
    pub edge_groups_read: u32,
}

/// Finds the rows of payload column `column` whose value contains `needle`. Only the frames
/// whose filters can hold the needle are read, and only the edge groups of the boundaries they
/// need. It is four waves at most: the filters, the refs pages, the candidate frames and the
/// edge groups.
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
    let corrupt = move || StrataError::Corrupt {
        what: "payload chunk id",
        offset: at,
    };

    let header = read_header(file, column).await?;
    if header.filters_len == 0 {
        return Err(StrataError::Corrupt {
            what: "payload filters",
            offset: at,
        });
    }
    let filters_ref = SegmentRef {
        offset: header.filters_offset,
        len: header.filters_len,
    };
    let body = file
        .read_segment(&filters_ref, Kind::PayloadFilters)
        .await?;
    let n_frames = u32::try_from(header.frames.len()).map_err(|_| StrataError::Corrupt {
        what: "payload header",
        offset: at,
    })?;
    let filters = Filters::decode(&body, header.filters_offset, n_frames)?;
    let n_rows = header.n_rows as usize;

    if needle.is_empty() {
        let rows = read_rows(file, &header, at).await?;
        return Ok(ContainsResult {
            rows: rows.iter().map(Option::is_some).collect(),
            frames_read: 0,
            edge_groups_read: 0,
        });
    }

    let mut trigram_set = HashSet::new();
    add_trigrams(needle, &mut trigram_set);
    let tris: Vec<u32> = trigram_set.into_iter().collect();

    // A trigram whose rollup bit is clear occurs in no value of the column.
    let rollup_misses = filters.rollup.as_ref().is_some_and(|rollup| {
        tris.iter()
            .any(|&h| !get_bit(rollup, h & (ROLLUP_BITS - 1)))
    });
    if rollup_misses {
        // `n_rows` is bounded by the header: the refs pages hold one 16-byte page reference per
        // 4,096 rows (checked in `PayloadHeader::decode`), so this allocation is at most 256 times
        // the header's size.
        return Ok(ContainsResult {
            rows: vec![false; n_rows],
            frames_read: 0,
            edge_groups_read: 0,
        });
    }

    let rows = read_rows(file, &header, at).await?;
    // Every chunk id of the column must name a chunk of a frame, whether or not that frame is
    // read.
    for ids in rows.iter().flatten() {
        if ids.iter().any(|&id| frame_of(&header.frames, id).is_none()) {
            return Err(corrupt());
        }
    }

    // A frame is a candidate when it has no filter, or when its filter holds every trigram.
    let candidates: Vec<usize> = (0..header.frames.len())
        .filter(|&f| match filters.frames.get(f) {
            Some(Some(filter)) => tris
                .iter()
                .all(|&h| get_bit(&filter.bitmap, filter_bit(h, filter.bits))),
            _ => true,
        })
        .collect();
    // One point wave for the candidate frames.
    let frame_corrupt = || StrataError::Corrupt {
        what: "payload frame",
        offset: at,
    };
    let mut frame_segments = Vec::with_capacity(candidates.len());
    for &f in &candidates {
        let entry = header.frames.get(f).ok_or_else(frame_corrupt)?;
        let segment = SegmentRef {
            offset: entry.offset,
            len: entry.len,
        };
        frame_segments.push((segment, Kind::PayloadFrame));
    }
    let raws = file.fetch(&frame_segments, ScanKind::Point).await?;
    let mut decoded: HashMap<usize, Decoded> = HashMap::with_capacity(candidates.len());
    for (&f, raw) in candidates.iter().zip(raws) {
        let entry = header.frames.get(f).ok_or_else(frame_corrupt)?;
        decoded.insert(f, decode_fetched_frame(entry, raw)?);
    }
    let frames_read = u32::try_from(candidates.len()).unwrap_or(u32::MAX);

    // Matches inside one chunk.
    let finder = Finder::new(needle);
    let mut matched: HashSet<u32> = HashSet::new();
    for (&f, (raw, ranges)) in &decoded {
        let entry = header.frames.get(f).ok_or_else(corrupt)?;
        for (local, range) in ranges.iter().enumerate() {
            let chunk = raw.get(range.clone()).ok_or_else(corrupt)?;
            if finder.find(chunk).is_some() {
                let id = u32::try_from(local)
                    .ok()
                    .and_then(|l| entry.first_chunk.checked_add(l))
                    .ok_or_else(corrupt)?;
                matched.insert(id);
            }
        }
    }

    // Adjacent chunk pairs whose right chunk lies in a candidate frame. A needle of 64 bytes or
    // fewer spans at most two chunks, so only these pairs can hold a boundary match.
    let mut boundary: HashSet<(u32, u32)> = HashSet::new();
    if needle.len() >= 2 {
        for ids in rows.iter().flatten() {
            for pair in ids.windows(2) {
                if let &[l, r] = pair {
                    let f = frame_of(&header.frames, r).ok_or_else(corrupt)?;
                    if decoded.contains_key(&f) {
                        boundary.insert((l, r));
                    }
                }
            }
        }
    }

    let groups_needed: BTreeSet<usize> = boundary
        .iter()
        .map(|&(l, _)| l as usize / EDGE_GROUP_CHUNKS)
        .collect();
    let group_list: Vec<usize> = groups_needed.into_iter().collect();
    // One point wave for the edge groups needed; each entry is (group offset, tail count).
    let mut group_segments = Vec::with_capacity(group_list.len());
    let mut group_meta = Vec::with_capacity(group_list.len());
    for &g in &group_list {
        match (
            header.edge_groups.get(g),
            edge_group_len(header.n_chunks, g),
        ) {
            (Some(group), Some(count)) => {
                let segment = SegmentRef {
                    offset: group.offset,
                    len: group.len,
                };
                group_segments.push((segment, Kind::PayloadEdges));
                group_meta.push((group.offset, count));
            }
            _ => {
                return Err(StrataError::Corrupt {
                    what: "payload chunk id",
                    offset: at,
                });
            }
        }
    }
    let bodies = file.fetch(&group_segments, ScanKind::Point).await?;
    let mut groups: HashMap<usize, Vec<Vec<u8>>> = HashMap::with_capacity(group_list.len());
    for ((&g, body), (offset, count)) in group_list.iter().zip(&bodies).zip(group_meta) {
        groups.insert(g, decode_edge_group(body, offset, count)?);
    }
    let edge_groups_read = u32::try_from(groups.len()).unwrap_or(u32::MAX);

    // The window is the last `k` bytes of the left tail followed by the first `k` bytes of the
    // right chunk, with `k = needle.len() - 1`. A needle that crosses the boundary has at most
    // `k` bytes on each side, so it lies inside the window; and a match inside one side alone
    // is too short to be a needle.
    let k = needle.len() - 1;
    let mut pair_hit: HashMap<(u32, u32), bool> = HashMap::new();
    for &(l, r) in &boundary {
        let tail = tail_of(&groups, l, at)?;
        let chunk = chunk_of(&header, &decoded, r, at)?;
        let tail_part = tail.get(tail.len().saturating_sub(k)..).unwrap_or_default();
        let head_part = chunk.get(..k.min(chunk.len())).unwrap_or_default();
        let window: Vec<u8> = tail_part.iter().chain(head_part).copied().collect();
        pair_hit.insert((l, r), finder.find(&window).is_some());
    }

    let rows_out: Vec<bool> = rows
        .iter()
        .map(|row| {
            match row {
            None => false,
            Some(ids) => {
                ids.iter().any(|id| matched.contains(id))
                    || ids.windows(2).any(|pair| {
                        matches!(pair, &[l, r] if pair_hit.get(&(l, r)).copied().unwrap_or(false))
                    })
            }
        }
        })
        .collect();
    Ok(ContainsResult {
        rows: rows_out,
        frames_read,
        edge_groups_read,
    })
}

/// Number of chunks in edge group `g` of a column with `n_chunks` chunks, or `None` when the
/// group starts past the last chunk.
fn edge_group_len(n_chunks: u32, g: usize) -> Option<usize> {
    let first = g.checked_mul(EDGE_GROUP_CHUNKS)?;
    (n_chunks as usize)
        .checked_sub(first)
        .map(|rest| rest.min(EDGE_GROUP_CHUNKS))
}

/// The tail of chunk `l`: entry `l % EDGE_GROUP_CHUNKS` of its edge group.
fn tail_of(groups: &HashMap<usize, Vec<Vec<u8>>>, l: u32, at: u64) -> Result<&[u8]> {
    let l = l as usize;
    groups
        .get(&(l / EDGE_GROUP_CHUNKS))
        .and_then(|tails| tails.get(l % EDGE_GROUP_CHUNKS))
        .map(Vec::as_slice)
        .ok_or(StrataError::Corrupt {
            what: "payload chunk id",
            offset: at,
        })
}

/// The bytes of chunk `id`, from its decoded frame.
fn chunk_of<'a>(
    header: &PayloadHeader,
    decoded: &'a HashMap<usize, Decoded>,
    id: u32,
    at: u64,
) -> Result<&'a [u8]> {
    let corrupt = move || StrataError::Corrupt {
        what: "payload chunk id",
        offset: at,
    };
    let f = frame_of(&header.frames, id).ok_or_else(corrupt)?;
    let entry = header.frames.get(f).ok_or_else(corrupt)?;
    let local = id.checked_sub(entry.first_chunk).ok_or_else(corrupt)? as usize;
    let (raw, ranges) = decoded.get(&f).ok_or_else(corrupt)?;
    let range = ranges.get(local).ok_or_else(corrupt)?;
    raw.get(range.clone()).ok_or_else(corrupt)
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
    use proptest::prelude::*;
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

    #[test]
    fn filters_round_trip() {
        let cases = [
            Filters {
                rollup: Some(vec![0xA5; (ROLLUP_BITS / 8) as usize]),
                frames: vec![
                    Some(FrameFilter {
                        bits: 1024,
                        bitmap: vec![1; 128],
                    }),
                    None,
                    Some(FrameFilter {
                        bits: 2048,
                        bitmap: vec![2; 256],
                    }),
                ],
            },
            Filters {
                rollup: None,
                frames: vec![None, None],
            },
            Filters {
                rollup: None,
                frames: Vec::new(),
            },
        ];
        for filters in cases {
            let mut bytes = Vec::new();
            filters.encode(&mut bytes).unwrap();
            let n = filters.frames.len() as u32;
            assert_eq!(Filters::decode(&bytes, 0, n).unwrap(), filters);
        }
    }

    /// Hand-built filters body: rollup bits, rollup bytes (none), frame count, the table
    /// entries given, then the area.
    fn raw_filters(rollup_bits: u32, entries: &[(u32, u32)], area: &[u8]) -> Vec<u8> {
        let mut bytes = rollup_bits.to_le_bytes().to_vec();
        if rollup_bits != 0 {
            bytes.extend(std::iter::repeat_n(0u8, (rollup_bits / 8) as usize));
        }
        bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (offset, bits) in entries {
            bytes.extend_from_slice(&offset.to_le_bytes());
            bytes.extend_from_slice(&bits.to_le_bytes());
        }
        bytes.extend_from_slice(area);
        bytes
    }

    #[test]
    fn filters_reject_bad_layouts() {
        let corrupt = |bytes: &[u8], n: u32| {
            matches!(
                Filters::decode(bytes, 0, n),
                Err(StrataError::Corrupt { .. })
            )
        };
        // A rollup size other than 0 or ROLLUP_BITS.
        assert!(corrupt(&raw_filters(5, &[], &[]), 0));
        // A bit count that is not a multiple of 64.
        assert!(corrupt(&raw_filters(0, &[(0, 1000)], &[0; 125]), 1));
        // A bit count below the minimum.
        assert!(corrupt(&raw_filters(0, &[(0, 512)], &[0; 64]), 1));
        // A bitmap offset that is not the running sum.
        assert!(corrupt(
            &raw_filters(0, &[(0, 1024), (999, 1024)], &[0; 256]),
            2
        ));
        // An absent filter with a nonzero bit count.
        assert!(corrupt(&raw_filters(0, &[(NO_FILTER, 64)], &[]), 1));
        // Bytes after the last bitmap.
        assert!(corrupt(&raw_filters(0, &[(0, 1024)], &[0; 129]), 1));
        // A frame count that does not match the header.
        assert!(corrupt(&raw_filters(0, &[(0, 1024)], &[0; 128]), 2));
        // A count of u32::MAX in a 12-byte input: rejected before anything is allocated.
        let mut huge = 0u32.to_le_bytes().to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(corrupt(&huge, u32::MAX));
    }

    #[test]
    fn edge_groups_round_trip() {
        let tails = vec![Vec::new(), vec![7u8; 63], b"abc".to_vec()];
        let mut bytes = Vec::new();
        encode_edge_group(&tails, &mut bytes);
        assert_eq!(bytes.len(), 3 * EDGE_ENTRY_LEN);
        assert_eq!(decode_edge_group(&bytes, 0, 3).unwrap(), tails);
        // A tail length of 64 is corrupt.
        let mut bad = vec![0u8; EDGE_ENTRY_LEN];
        bad[0] = 64;
        assert!(matches!(
            decode_edge_group(&bad, 0, 1),
            Err(StrataError::Corrupt { .. })
        ));
        // A wrong entry count is corrupt.
        assert!(matches!(
            decode_edge_group(&bytes, 0, 2),
            Err(StrataError::Corrupt { .. })
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn random_bytes_never_panic(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
            n in 0u32..16,
        ) {
            let _ = Filters::decode(&bytes, 0, n);
            let _ = decode_edge_group(&bytes, 0, n as usize);
        }
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
        println!(
            "corpus: frames {}, frames without filter {}, filter bytes {}, edge bytes {}, rollup {}",
            stats.frames,
            stats.frames_without_filter,
            stats.filter_bytes,
            stats.edge_bytes,
            stats.rollup
        );
        assert!(stats.frames >= 10, "{} frames", stats.frames);
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

        // Absent needles: uppercase and digits, which never occur in the corpus.
        let mut tested = 0;
        let mut no_frame_read = 0;
        let mut fewer_than_all = 0;
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
            if got.frames_read == 0 {
                no_frame_read += 1;
            }
            if got.frames_read < stats.frames {
                fewer_than_all += 1;
            }
        }
        if stats.rollup {
            assert!(no_frame_read >= 95, "{no_frame_read} of 100 read no frame");
        } else {
            println!("rollup dropped: {fewer_than_all} of 100 absent needles read fewer frames");
            assert!(
                fewer_than_all >= 95,
                "{fewer_than_all} of 100 read fewer frames"
            );
        }

        // Boundary needles: a needle that spans the end of chunk j.
        let multi: Vec<usize> = non_null
            .iter()
            .copied()
            .filter(|&i| chunk_value(bytes[i].unwrap()).len() >= 2)
            .collect();
        let (mut long_a, mut short_a, mut short_b) = (0, 0, 0);
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
                _ => rng.between(5, a_max),
            }
            .min(a_max);
            let b = if i % 5 == 0 { 1 } else { rng.between(1, b_max) };
            if a >= 5 {
                long_a += 1;
            }
            if a == 1 {
                short_a += 1;
            }
            if b == 1 {
                short_b += 1;
            }
            let needle = &value[p - a..p + b];
            let got = contains(&file, needle).await;
            assert_eq!(
                got.rows,
                brute(&bytes, needle),
                "boundary needle {needle:?}"
            );
        }
        assert!(long_a >= 30 && short_a >= 10 && short_b >= 10);

        // Filter coverage: at least 80% of the frames have a filter.
        assert!(stats.frames >= 5, "{} frames", stats.frames);
        assert!(
            stats.frames_without_filter * 5 <= stats.frames,
            "frames {}, frames without filter {}, filter bytes {}",
            stats.frames,
            stats.frames_without_filter,
            stats.filter_bytes
        );
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
        let needles: [&[u8]; 4] = [b"a", b"", b"abcab", b"zz"];
        for needle in needles {
            let got = contains(&file, needle).await;
            assert_eq!(got.rows, brute(&bytes, needle), "needle {needle:?}");
        }
        assert_eq!(
            contains(&file, b"").await.rows,
            vec![false, true, true, false, true]
        );
        assert_eq!(
            contains(&file, b"a").await.rows,
            vec![false, false, true, false, false]
        );
    }

    #[tokio::test]
    async fn boundary_set_is_required() {
        let mut rng = Rng::new(0x0B00_DA21);
        let msg = |t: &str| format!(r#"{{"role":"user","content":"{t}"}}"#);
        let mut column: Vec<Option<String>> = Vec::with_capacity(400);
        for _ in 0..400 {
            let t1 = String::from_utf8(rng.bytes(300, ALNUM)).unwrap();
            let t2 = String::from_utf8(rng.bytes(300, ALNUM)).unwrap();
            column.push(Some(format!("[{},{}]", msg(&t1), msg(&t2))));
        }
        for value in &column {
            assert_eq!(chunk_value(value.as_ref().unwrap().as_bytes()).len(), 2);
        }
        let batch = table(&column);
        let (file, summary) = write_and_open(&batch, payload_config()).await;
        let stats = stats_of(&summary);
        assert!(stats.frames >= 4, "{} frames", stats.frames);
        let bytes = as_bytes(&column);

        for (i, value) in column.iter().enumerate().step_by(4) {
            let value = value.as_ref().unwrap().as_bytes();
            let ranges = chunk_value(value);
            let end0 = ranges[0].end;
            // The last 12 bytes of chunk 0 followed by the first 12 bytes of chunk 1.
            let needle = &value[end0 - 12..end0 + 12];
            let got = contains(&file, needle).await;
            assert_eq!(got.rows, brute(&bytes, needle), "value {i}");
            assert!(got.frames_read < stats.frames, "value {i} read every frame");
            // The swapped needle is absent from every value.
            let swapped: Vec<u8> = value[end0..end0 + 12]
                .iter()
                .chain(&value[end0 - 12..end0])
                .copied()
                .collect();
            assert!(brute(&bytes, &swapped).iter().all(|&m| !m));
            assert!(contains(&file, &swapped).await.rows.iter().all(|&m| !m));
        }
    }

    #[tokio::test]
    async fn unfiltered_frame_is_always_scanned() {
        let column = vec![Some("abc".repeat(100))];
        let batch = table(&column);
        let (file, summary) = write_and_open(&batch, Config::default()).await;
        let stats = stats_of(&summary);
        assert_eq!(stats.frames_without_filter, 1);
        let bytes = as_bytes(&column);

        let present = contains(&file, b"abcab").await;
        assert_eq!(present.rows, brute(&bytes, b"abcab"));
        assert_eq!(present.rows, vec![true]);
        let present = contains(&file, b"ca").await;
        assert_eq!(present.rows, vec![true]);

        // The rollup excludes a three-letter needle that never occurs.
        let absent = contains(&file, b"xyz").await;
        assert_eq!(absent.rows, vec![false]);
        assert_eq!(absent.frames_read, 0);

        // A two-byte needle has no trigram: the unfiltered frame is scanned.
        let short = contains(&file, b"ac").await;
        assert_eq!(short.rows, vec![false]);
        assert_eq!(short.frames_read, 1);
    }

    #[tokio::test]
    async fn dropped_rollup_still_exact() {
        let mut rng = Rng::new(0xD20F_1A77);
        let values: Vec<Option<Vec<u8>>> = (0..300)
            .map(|_| Some((0..4096).map(|_| rng.next_u64() as u8).collect()))
            .collect();
        let batch = binary_table(&values);
        let (file, summary) = write_and_open(&batch, payload_config()).await;
        let stats = stats_of(&summary);
        assert!(!stats.rollup, "the rollup should have been dropped");
        let bytes: Vec<Option<&[u8]>> = values.iter().map(|v| v.as_deref()).collect();

        for _ in 0..50 {
            let row = rng.below(values.len());
            let value = values[row].as_ref().unwrap();
            let len = rng.between(8, 32);
            let start = rng.below(value.len() - len + 1);
            let needle = &value[start..start + len];
            let got = contains(&file, needle).await;
            assert_eq!(got.rows, brute(&bytes, needle));
        }
        let mut tested = 0;
        while tested < 50 {
            let n = rng.between(8, 32);
            let needle: Vec<u8> = (0..n).map(|_| rng.next_u64() as u8).collect();
            if brute(&bytes, &needle).iter().any(|&m| m) {
                continue;
            }
            tested += 1;
            let got = contains(&file, &needle).await;
            assert!(got.rows.iter().all(|&m| !m));
        }
    }

    #[tokio::test]
    async fn needles_across_the_first_edge_group_boundary_match_brute_force() {
        let mut rng = Rng::new(0xED6E_0001);
        let column: Vec<Option<String>> = (0..1_500)
            .map(|_| Some(String::from_utf8(rng.bytes(1_000, LOWER)).unwrap()))
            .collect();
        let batch = table(&column);
        let (file, _) = write_and_open(&batch, Config::default()).await;
        let record = &file.directory()[1];
        assert_eq!(record.layer, Layer::Payload);
        let header = read_header(&file, 1).await.unwrap();
        assert!(header.n_chunks > 1_024, "{} chunks", header.n_chunks);
        let bytes = as_bytes(&column);

        // The rows whose chunk ids include chunk 1,023 or 1,024: the last chunk of the first edge
        // group and the first chunk of the second.
        let rows = read_rows(&file, &header, record.payload_offset)
            .await
            .unwrap();
        let picked: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, ids)| {
                ids.as_ref()
                    .is_some_and(|ids| ids.iter().any(|&id| id == 1_023 || id == 1_024))
            })
            .map(|(r, _)| r)
            .collect();
        assert!(!picked.is_empty());

        // Every 16-byte window of those rows, at offsets 0, 8, 16, ... (at most 2,000 needles).
        let mut needles: Vec<Vec<u8>> = Vec::new();
        for &r in &picked {
            let value = bytes[r].unwrap();
            for start in (0..=value.len() - 16).step_by(8) {
                needles.push(value[start..start + 16].to_vec());
            }
        }
        needles.truncate(2_000);
        // Thirty random 16-byte substrings of other rows.
        let others: Vec<usize> = (0..column.len()).filter(|r| !picked.contains(r)).collect();
        for _ in 0..30 {
            let value = bytes[others[rng.below(others.len())]].unwrap();
            let start = rng.below(value.len() - 16 + 1);
            needles.push(value[start..start + 16].to_vec());
        }

        let mut edge_groups_read = 0;
        for needle in &needles {
            let got = contains(&file, needle).await;
            assert_eq!(got.rows, brute(&bytes, needle), "needle {needle:?}");
            edge_groups_read += got.edge_groups_read;
        }
        assert!(edge_groups_read > 0, "no needle read an edge group");
    }

    #[tokio::test]
    async fn empty_values_contain_no_needle_and_round_trip() {
        let column: Vec<Option<String>> = (0..50)
            .map(|i| {
                if i % 10 == 0 {
                    None
                } else {
                    Some(String::new())
                }
            })
            .collect();
        let batch = table(&column);
        let (file, _) = write_and_open(&batch, payload_config()).await;
        assert_eq!(file.directory()[1].layer, Layer::Payload);
        let bytes = as_bytes(&column);
        for needle in [b"a".as_slice(), b"ab", b"abc"] {
            let got = contains(&file, needle).await;
            assert!(got.rows.iter().all(|&m| !m), "needle {needle:?}");
            assert_eq!(got.rows, brute(&bytes, needle));
        }
        let decoded = read_column(&file, 1).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(1).as_ref());
    }

    /// The payload column of the SWE-agent trajectories, when `FMT_SWE_SHARDS` names its
    /// Parquet shards (comma-separated paths). Otherwise it is skipped.
    #[tokio::test]
    #[ignore]
    async fn filter_coverage_swe() {
        let Ok(shards) = std::env::var("FMT_SWE_SHARDS") else {
            println!("filter_coverage_swe skipped: FMT_SWE_SHARDS is not set");
            return;
        };
        let paths: Vec<std::path::PathBuf> = shards
            .split(',')
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .collect();
        let payloads = fmt_testdata::swe::payloads(&paths, 64 << 20, 1).unwrap();
        let input = payloads.chunks.first().map(|c| c[0].clone()).unwrap();
        let column: Vec<Option<String>> = input
            .iter()
            .map(|v| Some(String::from_utf8_lossy(v).into_owned()))
            .collect();
        let batch = table(&column);
        let (file, summary) = write_and_open(&batch, payload_config()).await;
        let stats = stats_of(&summary);
        println!(
            "swe: frames {}, frames without filter {}, filter bytes {}, edge bytes {}, rollup {}",
            stats.frames,
            stats.frames_without_filter,
            stats.filter_bytes,
            stats.edge_bytes,
            stats.rollup
        );
        assert!(stats.frames >= 5, "{} frames", stats.frames);
        assert!(stats.frames_without_filter * 5 <= stats.frames);
        let bytes = as_bytes(&column);
        let got = contains(&file, &payloads.needle).await;
        assert_eq!(got.rows, brute(&bytes, &payloads.needle));
    }
}
