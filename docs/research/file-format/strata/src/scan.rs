//! Scans and point lookups over a file: a predicate pruned by zones and dictionaries, late
//! materialisation of the projection (narrow, payload and map columns), `Contains`, `MapKeyEq`,
//! a limit, and the lookup of one sort key (design S7 § scan and § lookup_scan).
//!
//! A scan runs in waves. The first wave loads the metadata of every column it reads: block
//! tables, dictionaries and payload headers. Each `Contains` term then takes four waves of its
//! own. The filter wave fetches the surviving chunks of the predicate columns. The projection
//! fetches the narrow chunks that hold a selected row, then the refs pages and the frames of the
//! payload columns. Each wave is one `fetch`, so it is counted once.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, BooleanArray, RecordBatch, RecordBatchOptions, UInt32Array, make_comparator,
    new_empty_array,
};
use arrow::compute::{SortOptions, cast, concat, sort_to_indices, take};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use crate::error::{Result, StrataError};
use crate::file::{SCAN_PARALLEL_BYTES, StrataFile};
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::narrow::{BlockEntry, DictKind, Dictionary, decode_chunk_view, flags};
use crate::payload::{
    FrameBodies, FrameEntry, PayloadHeader, build_payload_values, ids_of, payload_contains,
    payload_contains_rows, read_header, refs_segments,
};
use crate::predicate::{
    Bound, MaskSource, Predicate, bind, bound_columns, chunk_may_match, evaluate, layout_of,
    map_key_mask, matching_codes, never_true, pruned_columns, zone_i64,
};
use crate::segment::{Kind, SegmentRef, Segments};
use crate::shred::unshred;

/// Chunk postings prune a column only when its predicate matches at most this many codes.
const MAX_POSTING_CODES: usize = 256;

/// Rows per output batch.
const BATCH_ROWS: usize = 8_192;

/// Rows evaluated per window by the filter: a candidate range longer than this is evaluated in
/// windows, so no evaluation holds more rows than this.
const FILTER_WINDOW: u32 = 65_536;

/// What to read from a file.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    /// Schema indexes of the output columns, in output order. May be empty.
    pub projection: Vec<usize>,
    /// Rows must satisfy it; `None` selects every row.
    pub predicate: Option<Predicate>,
    /// Restrict the scan to these rows: sorted, disjoint, non-empty ranges within the file.
    pub row_ranges: Option<Vec<Range<u32>>>,
    /// At most this many rows, the first ones in file order.
    pub limit: Option<usize>,
    /// How reads are coalesced.
    pub kind: ScanKind,
    /// Top-level Utf8 columns come out as `Utf8View`, which avoids copying their strings.
    pub views: bool,
    /// Keep only the selected rows that rank in the first `k` by one narrow column, and the
    /// rows tied with the k-th. Cannot be combined with `limit`.
    pub top: Option<TopRows>,
}

/// The rows a scan keeps when a sort and a limit sit above it: those that rank in the first `k`
/// by `column` under `options`, and every row tied with the k-th. Any query that sorts by
/// `column` first (other columns break ties) and keeps `k` rows finds them all here, so the
/// projection reads only the chunks that hold these rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopRows {
    /// The schema index of a narrow column.
    pub column: usize,
    pub options: SortOptions,
    pub k: usize,
}

/// `schema` with its top-level Utf8 fields as `Utf8View` when `views` is set.
pub(crate) fn scan_schema(schema: Schema, views: bool) -> SchemaRef {
    if !views {
        return Arc::new(schema);
    }
    let fields: Vec<Field> = schema
        .fields()
        .iter()
        .map(|field| match field.data_type() {
            DataType::Utf8 => field.as_ref().clone().with_data_type(DataType::Utf8View),
            _ => field.as_ref().clone(),
        })
        .collect();
    Arc::new(Schema::new_with_metadata(fields, schema.metadata().clone()))
}

/// The decoded chunks read so far, by (directory id, chunk index).
type Decoded = HashMap<(usize, usize), ArrayRef>;

/// What a scan needs of one chunked column: its field, its dictionary and its block-table
/// entries.
struct ColumnInfo {
    field: Field,
    dict: Option<Arc<Dictionary>>,
    entries: Arc<Vec<BlockEntry>>,
}

/// The masks of a bound predicate: the source of each `Mask` leaf, and for each `Contains`
/// source the full-column mask once it is computed.
struct Masks {
    sources: Vec<MaskSource>,
    full: Vec<Option<BooleanArray>>,
}

/// What the filter wave evaluates: the bound predicate, its masks, and the columns it reads;
/// and on how many threads.
struct FilterPlan<'a> {
    bound: &'a Bound,
    masks: &'a Masks,
    columns: &'a [usize],
    /// The scan's threads, for the filter wave.
    threads: usize,
}

/// One payload column of a read: its header, and what its refs pages and frames hold.
struct PayloadRead {
    column: usize,
    /// The file offset of the column's payload header, used in errors.
    at: u64,
    header: Arc<PayloadHeader>,
    /// Indexes into `header.ref_pages` of the pages that were fetched, in order.
    pages: Vec<usize>,
    /// Segments of `pages` in the wave that fetched them.
    n_pages: usize,
    /// The chunk ids of the rows read, `None` for a null row.
    ids: Vec<Option<Vec<u32>>>,
}

/// Removes every range of `remove` from `ranges`. Both lists are sorted and disjoint. The
/// result is sorted, disjoint and has no empty range. A single merge pass: linear in the
/// number of ranges in both lists.
pub(crate) fn subtract_all(ranges: &[Range<u32>], remove: &[Range<u32>]) -> Vec<Range<u32>> {
    let mut out = Vec::with_capacity(ranges.len());
    // The first removal that may still overlap a range: earlier ones end before every later
    // range starts, because the ranges are sorted.
    let mut first = 0usize;
    for range in ranges {
        let mut start = range.start;
        while remove.get(first).is_some_and(|r| r.end <= start) {
            first += 1;
        }
        let mut index = first;
        while let Some(r) = remove.get(index) {
            if r.start >= range.end {
                break;
            }
            if r.start > start {
                out.push(start..r.start);
            }
            start = start.max(r.end);
            index += 1;
        }
        if start < range.end {
            out.push(start..range.end);
        }
    }
    out
}

/// The indexes of the chunks in `entries` that overlap one of `ranges`, in order. `entries` is
/// sorted and disjoint, so each range is found by binary search and walked forward.
fn overlapping_chunks(entries: &[BlockEntry], ranges: &[Range<u32>]) -> Vec<usize> {
    let mut out = Vec::new();
    for range in ranges {
        if range.start >= range.end {
            continue;
        }
        let start = u64::from(range.start);
        let end = u64::from(range.end);
        let mut index =
            entries.partition_point(|e| u64::from(e.first_row) + u64::from(e.n_rows) <= start);
        while let Some(entry) = entries.get(index) {
            if u64::from(entry.first_row) >= end {
                break;
            }
            out.push(index);
            index += 1;
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The index of the chunk of `entries` that holds `row`.
fn chunk_of(entries: &[BlockEntry], row: u32) -> Result<usize> {
    let corrupt = || StrataError::Schema {
        reason: "internal: row outside the chunks".to_string(),
    };
    let count = entries.partition_point(|entry| entry.first_row <= row);
    let index = count.checked_sub(1).ok_or_else(corrupt)?;
    let entry = entries.get(index).ok_or_else(corrupt)?;
    let end = u64::from(entry.first_row) + u64::from(entry.n_rows);
    if u64::from(row) < end {
        Ok(index)
    } else {
        Err(corrupt())
    }
}

/// The indexes of the chunks of `entries` that hold at least one of `rows` (ascending). It
/// costs one binary search per chunk, not one per row.
fn chunks_with_rows(entries: &[BlockEntry], rows: &[u32]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut rest = rows;
    for (index, entry) in entries.iter().enumerate() {
        let Some(&first) = rest.first() else { break };
        let end = u64::from(entry.first_row) + u64::from(entry.n_rows);
        if u64::from(first) >= end {
            continue;
        }
        let inside = rest.partition_point(|&row| u64::from(row) < end);
        if inside > 0 && first >= entry.first_row {
            out.push(index);
        }
        rest = rest.get(inside..).unwrap_or_default();
    }
    out
}

/// The row `start + offset`, checked against overflow.
fn row_at(start: u32, offset: usize) -> Result<u32> {
    u32::try_from(offset)
        .ok()
        .and_then(|offset| start.checked_add(offset))
        .ok_or_else(|| StrataError::Corrupt {
            what: "row range",
            offset: u64::from(start),
        })
}

/// The decoded chunk `(column, index)`, which every wave decodes before it is used.
fn decoded_chunk(decoded: &Decoded, column: usize, index: usize) -> Result<&ArrayRef> {
    decoded
        .get(&(column, index))
        .ok_or_else(|| StrataError::Schema {
            reason: "internal: undecoded chunk".to_string(),
        })
}

/// The information of a chunked column, which every wave builds before it is used.
fn info_of(infos: &HashMap<usize, ColumnInfo>, column: usize) -> Result<&ColumnInfo> {
    infos.get(&column).ok_or_else(|| StrataError::Schema {
        reason: format!("internal: no info for column {column}"),
    })
}

/// A directory id that a map layout names, as an index.
fn index_of(id: u32) -> Result<usize> {
    usize::try_from(id).map_err(|_| StrataError::Corrupt {
        what: "map layout",
        offset: u64::from(id),
    })
}

/// The rows of the decoded `column` at `rows` (ascending, with each row in a chunk of `info`),
/// as one array of the column's type.
fn take_rows(
    decoded: &Decoded,
    info: &ColumnInfo,
    column: usize,
    rows: &[u32],
) -> Result<ArrayRef> {
    let mut pieces: Vec<ArrayRef> = Vec::new();
    let mut rest = rows;
    while let Some(&first) = rest.first() {
        let index = chunk_of(&info.entries, first)?;
        let entry = info.entries.get(index).ok_or_else(|| StrataError::Schema {
            reason: "internal: missing chunk".to_string(),
        })?;
        let end = u64::from(entry.first_row) + u64::from(entry.n_rows);
        let run = rest.partition_point(|&row| u64::from(row) < end);
        let (head, tail) = rest
            .split_at_checked(run)
            .ok_or_else(|| StrataError::Schema {
                reason: "internal: run of rows".to_string(),
            })?;
        let chunk = decoded_chunk(decoded, column, index)?;
        let corrupt_row = |row: u32| StrataError::Corrupt {
            what: "chunk row",
            offset: u64::from(row),
        };
        let (Some(&lo), Some(&hi)) = (head.first(), head.last()) else {
            return Err(StrataError::Schema {
                reason: "internal: empty run of rows".to_string(),
            });
        };
        let start = lo
            .checked_sub(entry.first_row)
            .ok_or_else(|| corrupt_row(lo))?;
        // Rows are ascending and distinct, so a run whose span equals its length is contiguous:
        // a slice, with no copy.
        if (hi - lo) as usize + 1 == head.len() {
            pieces.push(chunk.slice(start as usize, head.len()));
        } else {
            let local: Vec<u32> = head.iter().map(|&row| row - entry.first_row).collect();
            pieces.push(take(chunk.as_ref(), &UInt32Array::from(local), None)?);
        }
        rest = tail;
    }
    match pieces.len() {
        0 => Ok(new_empty_array(info.field.data_type())),
        1 => Ok(pieces.swap_remove(0)),
        _ => {
            let refs: Vec<&dyn Array> = pieces.iter().map(|piece| piece.as_ref()).collect();
            Ok(concat(&refs)?)
        }
    }
}

/// The positions in `values` that rank in the first `k` under `options`, and those tied with the
/// k-th (nulls are tied with nulls), in ascending order. Every position when there are at most
/// `k`.
fn top_positions(values: &dyn Array, options: SortOptions, k: usize) -> Result<Vec<usize>> {
    let order = sort_to_indices(values, Some(options), None)?;
    let order = order.values();
    let Some(&kth) = k.checked_sub(1).and_then(|last| order.get(last)) else {
        return Ok((0..values.len()).collect());
    };
    let compare = make_comparator(values, values, options)?;
    let (head, tail) = order.split_at(k);
    let mut keep: Vec<usize> = head.iter().map(|&i| i as usize).collect();
    keep.extend(
        tail.iter()
            .take_while(|&&i| compare(kth as usize, i as usize).is_eq())
            .map(|&i| i as usize),
    );
    keep.sort_unstable();
    Ok(keep)
}

/// When every `Contains` mask of `b` is a conjunct of the whole predicate (nested `AND`s
/// included) and appears nowhere else: the predicate without them, and their mask indexes in
/// order. `None` when there is no `Contains` mask, when one sits under an `OR` or a `NOT`, or
/// when there is no other conjunct.
/// A `Contains` term is never NULL, so a row passes `b` exactly when it passes the rest and
/// every one of them.
fn late_contains(b: &Bound, sources: &[MaskSource]) -> Option<(Bound, Vec<usize>)> {
    fn conjuncts<'a>(b: &'a Bound, out: &mut Vec<&'a Bound>) {
        match b {
            Bound::And(terms) => terms.iter().for_each(|term| conjuncts(term, out)),
            other => out.push(other),
        }
    }
    fn mentions(b: &Bound, is_contains: &dyn Fn(usize) -> bool) -> bool {
        match b {
            Bound::Mask(i) => is_contains(*i),
            Bound::And(terms) | Bound::Or(terms) => terms.iter().any(|t| mentions(t, is_contains)),
            Bound::Not(inner) => mentions(inner, is_contains),
            _ => false,
        }
    }
    let is_contains = |i: usize| matches!(sources.get(i), Some(MaskSource::Contains(..)));
    let total = (0..sources.len()).filter(|&i| is_contains(i)).count();
    if total == 0 {
        return None;
    }
    let mut terms = Vec::new();
    conjuncts(b, &mut terms);
    let mut late = Vec::new();
    let mut rest = Vec::new();
    for term in terms {
        match term {
            Bound::Mask(i) if is_contains(*i) => late.push(*i),
            other if mentions(other, &is_contains) => return None,
            other => rest.push(other.clone()),
        }
    }
    let mut unique = late.clone();
    unique.sort_unstable();
    unique.dedup();
    // With no other conjunct, every row is searched either way.
    (!rest.is_empty() && unique.len() == total && unique.len() == late.len())
        .then_some((Bound::And(rest), late))
}

/// The rows of the (sorted, disjoint) ranges, in order, or only the first `limit` of them. It
/// never builds more rows than it returns.
fn rows_in(ranges: &[Range<u32>], limit: Option<usize>) -> Vec<u32> {
    let rows = ranges.iter().flat_map(|range| range.clone());
    match limit {
        Some(n) => rows.take(n).collect(),
        None => rows.collect(),
    }
}

/// The segments of the chunks of `candidates` that have one, in order.
fn candidate_segments(candidates: &[BlockEntry]) -> Vec<(SegmentRef, Kind)> {
    candidates
        .iter()
        .filter(|entry| entry.segment_len > 0)
        .map(|entry| {
            let segment = SegmentRef {
                offset: entry.segment_offset,
                len: entry.segment_len,
            };
            (segment, Kind::NarrowChunk)
        })
        .collect()
}

/// The fetched bodies of `candidates` in candidate order: `None` for a chunk with no segment.
fn with_gaps(candidates: &[BlockEntry], bodies: Vec<Vec<u8>>) -> Result<Vec<Option<Vec<u8>>>> {
    let mut fetched = bodies.into_iter();
    let mut out = Vec::with_capacity(candidates.len());
    for entry in candidates {
        out.push(if entry.segment_len == 0 {
            None
        } else {
            Some(fetched.next().ok_or_else(|| StrataError::Corrupt {
                what: "segment range",
                offset: entry.segment_offset,
            })?)
        });
    }
    Ok(out)
}

/// The rows of the candidate chunks from their first row to their end.
fn span_of(candidates: &[BlockEntry]) -> Result<Range<u32>> {
    let corrupt = || StrataError::Corrupt {
        what: "sort-key block table",
        offset: 0,
    };
    let first = candidates.first().ok_or_else(corrupt)?;
    let last = candidates.last().ok_or_else(corrupt)?;
    let end = last
        .first_row
        .checked_add(last.n_rows)
        .ok_or_else(corrupt)?;
    Ok(first.first_row..end)
}

/// Chunks of one `plan_chunks` call that wait for their segment bodies.
type Pending = Vec<(usize, usize, BlockEntry)>;

/// Plans the fetch of the chunks in `wanted` that are not decoded yet. Chunks with no segment
/// are decoded into `decoded` at once. Returns the segments to fetch and, in the same order,
/// the chunks that wait for their bodies.
fn plan_chunks(
    infos: &HashMap<usize, ColumnInfo>,
    wanted: &BTreeSet<(usize, usize)>,
    decoded: &mut Decoded,
) -> Result<(Segments, Pending)> {
    let mut segments = Vec::new();
    let mut pending = Vec::new();
    for &(column, index) in wanted {
        if decoded.contains_key(&(column, index)) {
            continue;
        }
        let info = info_of(infos, column)?;
        let entry = *info.entries.get(index).ok_or_else(|| StrataError::Schema {
            reason: "internal: missing chunk".to_string(),
        })?;
        if entry.segment_len > 0 {
            let segment = SegmentRef {
                offset: entry.segment_offset,
                len: entry.segment_len,
            };
            segments.push((segment, Kind::NarrowChunk));
            pending.push((column, index, entry));
        } else {
            let array = decode_chunk_view(&entry, None, &info.field, info.dict.as_deref())?;
            decoded.insert((column, index), array);
        }
    }
    Ok((segments, pending))
}

/// Decodes the fetched bodies of the chunks planned by [`plan_chunks`] into `decoded`, on
/// `threads` threads once the bodies reach `SCAN_PARALLEL_BYTES`.
fn decode_chunks(
    infos: &HashMap<usize, ColumnInfo>,
    pending: Vec<(usize, usize, BlockEntry)>,
    bodies: Vec<Vec<u8>>,
    decoded: &mut Decoded,
    threads: usize,
) -> Result<()> {
    if bodies.len() != pending.len() {
        return Err(StrataError::Corrupt {
            what: "segment range",
            offset: 0,
        });
    }
    let decode = |i: &usize| -> Result<ArrayRef> {
        let ((column, _, entry), body) =
            pending
                .get(*i)
                .zip(bodies.get(*i))
                .ok_or(StrataError::Corrupt {
                    what: "segment range",
                    offset: 0,
                })?;
        let info = info_of(infos, *column)?;
        decode_chunk_view(
            entry,
            Some(body.as_slice()),
            &info.field,
            info.dict.as_deref(),
        )
    };
    let indexes: Vec<usize> = (0..pending.len()).collect();
    let total: usize = bodies.iter().map(Vec::len).sum();
    let arrays = if threads < 2 || total < SCAN_PARALLEL_BYTES {
        indexes.iter().map(decode).collect::<Result<Vec<_>>>()?
    } else {
        crate::parallel::map_in_order(
            &indexes,
            |&i| bodies.get(i).map_or(0, Vec::len),
            threads,
            decode,
        )?
    };
    for ((column, index, _), array) in pending.into_iter().zip(arrays) {
        decoded.insert((column, index), array);
    }
    Ok(())
}

/// The directory ids of the narrow-layout columns that `projection` reads: the narrow columns
/// themselves, and the children and residual of every projected map.
fn narrow_reads(file: &StrataFile, projection: &[usize]) -> Result<BTreeSet<usize>> {
    let mut out = BTreeSet::new();
    for &column in projection {
        match file.directory().get(column).map(|record| record.layer) {
            Some(Layer::Narrow) => {
                out.insert(column);
            }
            Some(Layer::MapParent) => {
                let layout = layout_of(file, column)?;
                out.insert(index_of(layout.residual)?);
                for key in &layout.keys {
                    out.insert(index_of(key.child)?);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Whether directory record `column` holds block-table chunks: a narrow column, a map child or
/// a residual.
fn is_chunked(file: &StrataFile, column: usize) -> bool {
    matches!(
        file.directory().get(column).map(|record| record.layer),
        Some(Layer::Narrow | Layer::MapChild | Layer::Residual)
    )
}

/// The projected payload columns, once each, in ascending order.
fn payload_projection(file: &StrataFile, projection: &[usize]) -> Vec<usize> {
    projection
        .iter()
        .copied()
        .filter(|&column| {
            file.directory()
                .get(column)
                .is_some_and(|record| record.layer == Layer::Payload)
        })
        .collect::<BTreeSet<usize>>()
        .into_iter()
        .collect()
}

/// How one output column of a scan is built.
enum OutputColumn {
    /// A narrow column: taken from its decoded chunks.
    Narrow(usize),
    /// A payload column: built from the frames, by its index in [`PayloadValues::reads`].
    Payload(usize),
    /// A map: rebuilt from its key children and its residual.
    Map {
        keys: Vec<Vec<u8>>,
        children: Vec<usize>,
        residual: usize,
        field: Field,
    },
}

/// How each column of `projection` is built. `reads` are the payload reads of the scan.
fn output_columns(
    file: &StrataFile,
    projection: &[usize],
    reads: &[PayloadRead],
) -> Result<Vec<OutputColumn>> {
    projection
        .iter()
        .map(|&column| {
            let layer = file.directory().get(column).map(|record| record.layer);
            Ok(match layer {
                Some(Layer::Narrow) => OutputColumn::Narrow(column),
                Some(Layer::Payload) => {
                    let index = reads
                        .iter()
                        .position(|read| read.column == column)
                        .ok_or_else(|| StrataError::Schema {
                            reason: format!("internal: no values of column {column}"),
                        })?;
                    OutputColumn::Payload(index)
                }
                Some(Layer::MapParent) => {
                    let layout = layout_of(file, column)?;
                    OutputColumn::Map {
                        keys: layout.keys.iter().map(|key| key.key.clone()).collect(),
                        children: layout
                            .keys
                            .iter()
                            .map(|key| index_of(key.child))
                            .collect::<Result<_>>()?,
                        residual: index_of(layout.residual)?,
                        field: file.field_of(column)?,
                    }
                }
                _ => {
                    return Err(StrataError::Schema {
                        reason: format!("internal: projection of column {column}"),
                    });
                }
            })
        })
        .collect()
}

/// The payload columns of a scan: their chunk ids, and their frames as fetched, decoded when a
/// batch first needs them and dropped after the last batch that does.
struct PayloadValues {
    reads: Vec<PayloadRead>,
    fields: Vec<Field>,
    frames: FrameBodies,
    /// For each batch, the frames (by offset) that no later batch uses.
    release: Vec<Vec<u64>>,
    views: bool,
}

/// The batches of a scan whose reads are done, built as they are asked for: a few at a time,
/// one per thread, at most 8,192 rows each, in file order. The frames that no later batch
/// needs are dropped after each step, so a scan holds the frames of the batches being built and
/// of those that share them, not the whole file's. Building needs no I/O.
pub struct ScanBatches {
    columns: Vec<OutputColumn>,
    selection: Vec<u32>,
    infos: HashMap<usize, ColumnInfo>,
    decoded: Decoded,
    payloads: Option<PayloadValues>,
    output: SchemaRef,
    /// The index of the next batch to build.
    next: usize,
    ready: std::collections::VecDeque<RecordBatch>,
    /// Batches built at a time, each on its own thread.
    threads: usize,
}

impl ScanBatches {
    fn empty(output: SchemaRef) -> Self {
        Self {
            columns: Vec::new(),
            selection: Vec::new(),
            infos: HashMap::new(),
            decoded: HashMap::new(),
            payloads: None,
            output,
            next: 0,
            ready: std::collections::VecDeque::new(),
            threads: 1,
        }
    }

    /// The schema of the batches.
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.output)
    }

    /// The batches with every narrow Utf8 column that has a file dictionary as
    /// `Dictionary(UInt32, Utf8)`, the arrays the scan decodes, instead of strings: an
    /// aggregate grouped on such a column groups on its codes (`aggregate.rs`).
    pub(crate) fn keep_dictionaries(mut self) -> Self {
        let mut fields: Vec<Field> = self
            .output
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        for (field, column) in fields.iter_mut().zip(&self.columns) {
            if let OutputColumn::Narrow(c) = column
                && let Some(info) = self.infos.get(c)
                && info
                    .dict
                    .as_ref()
                    .is_some_and(|d| d.kind() == DictKind::Utf8)
            {
                *field = field.clone().with_data_type(DataType::Dictionary(
                    Box::new(DataType::UInt32),
                    Box::new(DataType::Utf8),
                ));
            }
        }
        self.output = Arc::new(Schema::new_with_metadata(
            fields,
            self.output.metadata().clone(),
        ));
        self
    }

    /// The frames held for the batches not built yet.
    #[cfg(test)]
    fn frames_held(&self) -> usize {
        self.payloads
            .as_ref()
            .map_or(0, |values| values.frames.len())
    }

    fn batch_count(&self) -> usize {
        self.selection.len().div_ceil(BATCH_ROWS)
    }

    /// The steps of batches left to build: each builds up to `threads` batches at once.
    pub(crate) fn steps_left(&self) -> usize {
        self.batch_count()
            .saturating_sub(self.next)
            .div_ceil(self.threads.max(1))
    }

    /// Builds batch `index`: the rows of `selection` from `index * BATCH_ROWS`.
    fn build(&self, index: usize) -> Result<RecordBatch> {
        let start = index * BATCH_ROWS;
        let end = (start + BATCH_ROWS).min(self.selection.len());
        let rows = self.selection.get(start..end).unwrap_or_default();
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(self.columns.len());
        for column in &self.columns {
            let array = match column {
                OutputColumn::Narrow(column) => {
                    take_rows(&self.decoded, info_of(&self.infos, *column)?, *column, rows)?
                }
                OutputColumn::Payload(read) => {
                    let missing = || StrataError::Schema {
                        reason: "internal: no payload values".to_string(),
                    };
                    let values = self.payloads.as_ref().ok_or_else(missing)?;
                    let read_ = values.reads.get(*read).ok_or_else(missing)?;
                    let field = values.fields.get(*read).ok_or_else(missing)?;
                    let ids = read_.ids.get(start..end).ok_or_else(missing)?;
                    build_payload_values(
                        &read_.header,
                        &values.frames,
                        ids,
                        field,
                        read_.at,
                        values.views,
                    )?
                }
                OutputColumn::Map {
                    keys,
                    children,
                    residual,
                    field,
                } => {
                    // The scan decodes Utf8 children as dictionary arrays; `unshred` takes Utf8.
                    let children = children
                        .iter()
                        .map(|&child| {
                            let array = take_rows(
                                &self.decoded,
                                info_of(&self.infos, child)?,
                                child,
                                rows,
                            )?;
                            match array.data_type() {
                                DataType::Dictionary(..) => Ok(cast(&array, &DataType::Utf8)?),
                                _ => Ok(array),
                            }
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let residual = take_rows(
                        &self.decoded,
                        info_of(&self.infos, *residual)?,
                        *residual,
                        rows,
                    )?;
                    unshred(field.data_type(), keys, &children, &residual)?
                }
            };
            let field = self.output.field(columns.len());
            let array = if array.data_type() == field.data_type() {
                array
            } else {
                cast(&array, field.data_type())?
            };
            columns.push(array);
        }
        let options = RecordBatchOptions::new().with_row_count(Some(rows.len()));
        Ok(RecordBatch::try_new_with_options(
            Arc::clone(&self.output),
            columns,
            &options,
        )?)
    }

    /// Builds the batches `range` on several threads, then drops the frames that no later
    /// batch needs.
    fn build_step(&mut self, range: Range<usize>) -> Result<Vec<RecordBatch>> {
        let indexes: Vec<usize> = range.clone().collect();
        let batches = if indexes.len() < 2 {
            indexes
                .iter()
                .map(|&i| self.build(i))
                .collect::<Result<Vec<_>>>()?
        } else {
            crate::parallel::map_in_order(&indexes, |_| 1, indexes.len(), |&i| self.build(i))?
        };
        if let Some(values) = &mut self.payloads {
            for index in range {
                for offset in values
                    .release
                    .get_mut(index)
                    .map(std::mem::take)
                    .unwrap_or_default()
                {
                    values.frames.release(offset);
                }
            }
        }
        Ok(batches)
    }
}

impl Iterator for ScanBatches {
    type Item = Result<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ready.is_empty() {
            let count = self.batch_count();
            if self.next >= count {
                return None;
            }
            let end = self.next.saturating_add(self.threads).min(count);
            let step = self.next..end;
            self.next = end;
            match self.build_step(step) {
                Ok(batches) => self.ready.extend(batches),
                Err(e) => {
                    // A failed scan ends with its error.
                    self.next = count;
                    return Some(Err(e));
                }
            }
        }
        self.ready.pop_front().map(Ok)
    }
}

impl StrataFile {
    /// Rows of the file matching `req`, as record batches of at most 8,192 rows. The batches
    /// hold the projection, in output order, and the rows are in file order.
    ///
    /// The scan takes one metadata wave, then four waves for each `Contains` term, then the
    /// filter wave, then the projection: the narrow chunks, then for payload columns the refs
    /// pages and the frames. A wave with nothing to fetch makes no request.
    /// `MapKeyEq` and map or payload projections are supported; `Cmp`, `In`, `IsNull` and
    /// `IsNotNull` take narrow columns only.
    ///
    /// Without a predicate, `limit` bounds the rows built and fetched. With a predicate, `limit`
    /// bounds the evaluation and the output, but not the bytes fetched: the filter wave still
    /// fetches every surviving predicate chunk.
    pub async fn scan(&self, req: &ScanRequest) -> Result<Vec<RecordBatch>> {
        self.scan_batches(req, crate::parallel::cores())
            .await?
            .collect()
    }

    /// The reads of [`Self::scan`], with the batches left to build as they are asked for. All
    /// the I/O is done when it returns; see [`ScanBatches`].
    ///
    /// The scan uses at most `threads` threads (at least one): to decompress and decode the
    /// chunks of a wave of at least `SCAN_PARALLEL_BYTES`, and to build that many batches at a
    /// time. [`Self::scan`] gives it every core; a caller that runs several scans at once gives
    /// each its share of the cores.
    pub async fn scan_batches(&self, req: &ScanRequest, threads: usize) -> Result<ScanBatches> {
        let threads = threads.max(1);
        // 1. Validate. Every error is found before any read.
        self.check_projection(&req.projection)?;
        let (bound, sources) = match &req.predicate {
            Some(predicate) => {
                let (bound, sources) = bind(predicate, self)?;
                (Some(bound), sources)
            }
            None => (None, Vec::new()),
        };
        self.check_row_ranges(req.row_ranges.as_deref())?;
        if let Some(top) = &req.top {
            if req.limit.is_some() {
                return Err(StrataError::Unsupported {
                    what: "a scan with both a limit and top rows".to_string(),
                });
            }
            let layer = self.directory().get(top.column).map(|record| record.layer);
            if !matches!(layer, Some(Layer::Narrow)) {
                return Err(StrataError::Unsupported {
                    what: format!("top rows by column {}, which is not narrow", top.column),
                });
            }
        }
        let output: SchemaRef = scan_schema(self.schema().project(&req.projection)?, req.views);
        if req.limit == Some(0) || req.top.is_some_and(|top| top.k == 0) {
            return Ok(ScanBatches::empty(output));
        }

        // 2. Metadata wave: the block tables, dictionaries and payload headers that the scan
        // reads, including the residuals of `MapKeyEq` masks and the payload of `Contains`.
        let predicate_columns = bound.as_ref().map(bound_columns).unwrap_or_default();
        let mut filter_columns: BTreeSet<usize> = predicate_columns.iter().copied().collect();
        let mut all_columns: BTreeSet<usize> = BTreeSet::new();
        for source in &sources {
            match source {
                MaskSource::MapKey(residual, _, _) => {
                    filter_columns.insert(*residual);
                }
                MaskSource::Contains(column, _) => {
                    all_columns.insert(*column);
                }
            }
        }
        all_columns.extend(filter_columns.iter().copied());
        all_columns.extend(narrow_reads(self, &req.projection)?);
        all_columns.extend(payload_projection(self, &req.projection));
        all_columns.extend(req.top.map(|top| top.column));
        let filter_columns: Vec<usize> = filter_columns.into_iter().collect();
        let all_columns: Vec<usize> = all_columns.into_iter().collect();
        self.load_metadata(&all_columns, req.kind).await?;
        let mut infos: HashMap<usize, ColumnInfo> = HashMap::new();
        for &column in &all_columns {
            if is_chunked(self, column) {
                let info = ColumnInfo {
                    field: self.field_of(column)?,
                    dict: self.dictionary(column).await?,
                    entries: self.block_entries(column).await?,
                };
                infos.insert(column, info);
            }
        }

        // 3. Candidate rows, pruned by the zones and dictionaries of the predicate's columns.
        let rows = self.row_count();
        let mut candidates: Vec<Range<u32>> = match &req.row_ranges {
            Some(ranges) => ranges.clone(),
            None if rows == 0 => Vec::new(),
            None => {
                let end = u32::try_from(rows).map_err(|_| StrataError::Corrupt {
                    what: "row count",
                    offset: rows,
                })?;
                std::iter::once(0..end).collect()
            }
        };
        // A top-level FALSE or NULL conjunct selects no row: every chunk is pruned.
        if let Some(b) = &bound
            && never_true(b)
        {
            return Ok(ScanBatches::empty(output));
        }
        if let Some(b) = &bound {
            for column in pruned_columns(b) {
                let info = info_of(&infos, column)?;
                let codes = matching_codes(b, column, info.dict.as_deref())?;
                // The chunk postings, when the column has them, name the only chunks that hold
                // one of the matching codes.
                let mut allowed: Option<Vec<u32>> = None;
                if let Some(codes) = &codes
                    && codes.len() <= MAX_POSTING_CODES
                    && let Some(key) = self.key_index(column).await?
                    && let Some(postings) = &key.postings
                {
                    allowed = Some(postings.chunks_for(codes, info.entries.len())?);
                }
                let mut pruned: Vec<Range<u32>> = Vec::new();
                for (index, entry) in info.entries.iter().enumerate() {
                    if let Some(allowed) = &allowed
                        && u32::try_from(index).is_ok_and(|i| allowed.binary_search(&i).is_err())
                    {
                        let end = entry.first_row.saturating_add(entry.n_rows);
                        pruned.push(entry.first_row..end);
                        continue;
                    }
                    let may = chunk_may_match(
                        b,
                        column,
                        entry,
                        &info.field,
                        info.dict.as_deref(),
                        codes.as_ref(),
                    )?;
                    if !may {
                        let end = entry.first_row.checked_add(entry.n_rows).ok_or_else(|| {
                            StrataError::Corrupt {
                                what: "block row order",
                                offset: u64::from(entry.first_row),
                            }
                        })?;
                        pruned.push(entry.first_row..end);
                    }
                }
                candidates = subtract_all(&candidates, &pruned);
                if candidates.is_empty() {
                    return Ok(ScanBatches::empty(output));
                }
            }
        }

        // 4–5. The selected rows. When every `Contains` term is a conjunct of the whole
        // predicate, the other conjuncts are evaluated first, and each search then reads only the
        // frames of the rows they keep: a time range searches the text of its rows only. Else
        // the masks of the `Contains` terms are computed over the whole file before the filter
        // wave, which evaluates the predicate with them.
        let late = bound.as_ref().and_then(|b| late_contains(b, &sources));
        let mut decoded: Decoded = HashMap::new();
        let mut selection: Vec<u32> = match (&bound, late) {
            (None, _) => rows_in(&candidates, req.limit),
            (Some(_), Some((rest, late))) => {
                // `rest` reads no `Contains` mask; theirs are null placeholders.
                let full = sources
                    .iter()
                    .map(|source| match source {
                        MaskSource::Contains(..) => Some(BooleanArray::new_null(
                            usize::try_from(rows).unwrap_or(usize::MAX),
                        )),
                        MaskSource::MapKey(..) => None,
                    })
                    .collect();
                let masks = Masks { full, sources };
                let plan = FilterPlan {
                    bound: &rest,
                    masks: &masks,
                    columns: &filter_columns,
                    threads,
                };
                let mut selection = self
                    .filter(&plan, &infos, &candidates, req.kind, None, &mut decoded)
                    .await?;
                for index in late {
                    if selection.is_empty() {
                        break;
                    }
                    if let Some(MaskSource::Contains(column, needle)) = masks.sources.get(index) {
                        let found =
                            payload_contains_rows(self, *column, needle, &selection).await?;
                        if found.rows.len() != selection.len() {
                            return Err(StrataError::Schema {
                                reason: "internal: a search of the wrong row count".to_string(),
                            });
                        }
                        selection = selection
                            .into_iter()
                            .zip(found.rows)
                            .filter_map(|(row, hit)| hit.then_some(row))
                            .collect();
                    }
                }
                selection
            }
            (Some(b), None) => {
                let mut masks = Masks {
                    full: vec![None; sources.len()],
                    sources,
                };
                if !candidates.is_empty() {
                    for (index, source) in masks.sources.iter().enumerate() {
                        if let MaskSource::Contains(column, needle) = source {
                            let result = payload_contains(self, *column, needle).await?;
                            if let Some(slot) = masks.full.get_mut(index) {
                                *slot = Some(BooleanArray::from(result.rows));
                            }
                        }
                    }
                }
                let plan = FilterPlan {
                    bound: b,
                    masks: &masks,
                    columns: &filter_columns,
                    threads,
                };
                self.filter(
                    &plan,
                    &infos,
                    &candidates,
                    req.kind,
                    req.limit,
                    &mut decoded,
                )
                .await?
            }
        };

        // 6. Limit, or the top rows.
        if let Some(limit) = req.limit {
            selection.truncate(limit);
        }
        if let Some(top) = &req.top {
            selection = self
                .top_rows(top, &infos, selection, req.kind, &mut decoded, threads)
                .await?;
        }
        if selection.is_empty() {
            return Ok(ScanBatches::empty(output));
        }

        // 7. Projection: the narrow chunks, then the payload columns (two waves).
        let narrow = narrow_reads(self, &req.projection)?;
        let mut wanted: BTreeSet<(usize, usize)> = BTreeSet::new();
        for &column in &narrow {
            let info = info_of(&infos, column)?;
            for index in chunks_with_rows(&info.entries, &selection) {
                wanted.insert((column, index));
            }
        }
        self.decode_wave(&infos, &wanted, req.kind, &mut decoded, threads)
            .await?;
        let payload = payload_projection(self, &req.projection);
        let payloads = self
            .payload_values(&payload, &selection, req.kind, req.views)
            .await?;
        let mut batches =
            self.batches(&req.projection, selection, infos, decoded, payloads, output)?;
        batches.threads = threads;
        Ok(batches)
    }

    /// Whether some row of the file may satisfy `predicate`, from the zones, dictionaries and
    /// chunk postings of its pruning columns. `false` is a proof that none does. Loads the
    /// metadata of those columns (one wave when it is not cached).
    pub async fn may_match(&self, predicate: &Predicate) -> Result<bool> {
        let (bound, _) = bind(predicate, self)?;
        if never_true(&bound) {
            return Ok(false);
        }
        let columns: Vec<usize> = pruned_columns(&bound)
            .into_iter()
            .filter(|&column| is_chunked(self, column))
            .collect();
        self.load_metadata(&columns, ScanKind::Point).await?;
        for column in columns {
            let field = self.field_of(column)?;
            let dict = self.dictionary(column).await?;
            let entries = self.block_entries(column).await?;
            let codes = matching_codes(&bound, column, dict.as_deref())?;
            if let Some(codes) = &codes
                && codes.len() <= MAX_POSTING_CODES
                && let Some(key) = self.key_index(column).await?
                && let Some(postings) = &key.postings
                && postings.chunks_for(codes, entries.len())?.is_empty()
            {
                return Ok(false);
            }
            let mut any = false;
            for entry in entries.iter() {
                if chunk_may_match(
                    &bound,
                    column,
                    entry,
                    &field,
                    dict.as_deref(),
                    codes.as_ref(),
                )? {
                    any = true;
                    break;
                }
            }
            if !any {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The smallest and largest value of the signed integer or timestamp column `column`, from
    /// its chunk zones (one wave when the metadata is not cached), and whether a row is null.
    /// `None` when the column is of another type or not chunked, when every row is null, or when
    /// a chunk with a valid value has no zone.
    pub async fn int_range(&self, column: usize) -> Result<Option<(i64, i64, bool)>> {
        let field = self.field_of(column)?;
        let int = matches!(
            field.data_type(),
            DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::Timestamp(_, _)
        );
        if !int || !is_chunked(self, column) {
            return Ok(None);
        }
        self.load_metadata(&[column], ScanKind::Point).await?;
        let entries = self.block_entries(column).await?;
        let mut range: Option<(i64, i64)> = None;
        let mut nulls = false;
        for entry in entries.iter() {
            nulls |= entry.flags & (flags::HAS_NULLS | flags::ALL_NULL) != 0;
            if entry.flags & flags::ALL_NULL != 0 {
                continue;
            }
            if entry.flags & flags::HAS_ZONE == 0 {
                return Ok(None);
            }
            let (Some(lo), Some(hi)) = (zone_i64(&entry.min), zone_i64(&entry.max)) else {
                return Ok(None);
            };
            range = Some(match range {
                None => (lo, hi),
                Some((a, b)) => (a.min(lo), b.max(hi)),
            });
        }
        Ok(range.map(|(lo, hi)| (lo, hi, nulls)))
    }

    /// Rows whose first sort-key column equals `key`, with the projected columns. It runs in
    /// waves, with the sort-key chunks of the first wave: with the metadata cached, or kept by
    /// the open, one wave, or two with a payload column; else one more. A key the sort-key
    /// filter or the zones rule out costs no wave. With `views`, top-level Utf8 columns come out as
    /// `Utf8View`.
    pub async fn lookup_scan(
        &self,
        key: &[u8],
        projection: &[usize],
        views: bool,
    ) -> Result<Vec<RecordBatch>> {
        self.check_projection(projection)?;
        let output: SchemaRef = scan_schema(self.schema().project(projection)?, views);
        let Some(candidates) = self.lookup_candidates(key)? else {
            return Ok(Vec::new());
        };
        let kind = ScanKind::Point;
        let sort = self
            .sort_key_column()
            .ok_or_else(|| StrataError::Unsupported {
                what: "lookup on this sort key".to_string(),
            })?;
        let span = span_of(&candidates)?;
        let narrow = narrow_reads(self, projection)?;
        let payload = payload_projection(self, projection);
        let mut meta_columns: BTreeSet<usize> = narrow.clone();
        meta_columns.extend(payload.iter().copied());
        let meta_columns: Vec<usize> = meta_columns.into_iter().collect();
        if self.has_metadata() {
            // Served from the metadata section the open kept: no request, so the lookup takes
            // the path of a file whose metadata is cached.
            self.load_metadata(&meta_columns, kind).await?;
        }
        let (meta_segments, meta_plan) = self.metadata_segments(&meta_columns)?;
        let cold = !meta_plan.is_empty();
        let narrow_vec: Vec<usize> = narrow.iter().copied().collect();

        let mut decoded: Decoded = HashMap::new();
        let mut infos: HashMap<usize, ColumnInfo>;
        let exact: Vec<u32>;
        let mut reads: Vec<PayloadRead>;

        if cold {
            // W1: the candidate sort-key chunks and the metadata, in one wave.
            let cand_segments = candidate_segments(&candidates);
            let n_cand = cand_segments.len();
            let mut segments = cand_segments;
            segments.extend(meta_segments);
            let mut bodies = self.fetch(&segments, kind).await?;
            let meta_bodies = bodies.split_off(n_cand);
            self.store_metadata(meta_plan, &meta_bodies)?;
            let cand_bodies = with_gaps(&candidates, bodies)?;
            let exact_ranges = self.rows_of_key(&candidates, &cand_bodies, key)?;
            exact = rows_in(&exact_ranges, None);
            infos = HashMap::new();
            for &column in &narrow_vec {
                infos.insert(
                    column,
                    ColumnInfo {
                        field: self.field_of(column)?,
                        dict: self.dictionary(column).await?,
                        entries: self.block_entries(column).await?,
                    },
                );
            }
            // A projected sort-key column comes from the same candidate bodies.
            if let Some(info) = infos.get(&sort) {
                for (entry, body) in candidates.iter().zip(&cand_bodies) {
                    let index = info
                        .entries
                        .binary_search_by_key(&entry.first_row, |e| e.first_row)
                        .map_err(|_| StrataError::Corrupt {
                            what: "sort-key block table",
                            offset: entry.segment_offset,
                        })?;
                    let array = decode_chunk_view(
                        entry,
                        body.as_deref(),
                        &info.field,
                        info.dict.as_deref(),
                    )?;
                    decoded.insert((sort, index), array);
                }
            }
            // W2: the narrow chunks that overlap the exact rows, and the refs pages of those rows.
            let mut wanted: BTreeSet<(usize, usize)> = BTreeSet::new();
            for (&column, info) in &infos {
                for index in overlapping_chunks(&info.entries, &exact_ranges) {
                    wanted.insert((column, index));
                }
            }
            let (narrow_segments, pending) = plan_chunks(&infos, &wanted, &mut decoded)?;
            let n_narrow = narrow_segments.len();
            let (planned, refs) = self.plan_refs(&payload, &exact).await?;
            reads = planned;
            let mut segments = narrow_segments;
            segments.extend(refs);
            let mut bodies = self.fetch(&segments, kind).await?;
            let refs_bodies = bodies.split_off(n_narrow);
            // A lookup's chunks are few: one thread.
            decode_chunks(&infos, pending, bodies, &mut decoded, 1)?;
            Self::take_ids(&mut reads, refs_bodies, &exact)?;
        } else {
            // The metadata is cached. W1: the candidate chunks, the narrow chunks that overlap
            // the span, and the refs pages of the span.
            infos = HashMap::new();
            for &column in &narrow_vec {
                infos.insert(
                    column,
                    ColumnInfo {
                        field: self.field_of(column)?,
                        dict: self.dictionary(column).await?,
                        entries: self.block_entries(column).await?,
                    },
                );
            }
            let mut wanted: BTreeSet<(usize, usize)> = BTreeSet::new();
            for (&column, info) in &infos {
                for index in overlapping_chunks(&info.entries, std::slice::from_ref(&span)) {
                    wanted.insert((column, index));
                }
            }
            let cand_segments = candidate_segments(&candidates);
            let n_cand = cand_segments.len();
            let (narrow_segments, pending) = plan_chunks(&infos, &wanted, &mut decoded)?;
            let n_narrow = narrow_segments.len();
            let span_rows: Vec<u32> = (span.start..span.end).collect();
            let (planned, refs) = self.plan_refs(&payload, &span_rows).await?;
            reads = planned;
            let mut segments = cand_segments;
            segments.extend(narrow_segments);
            segments.extend(refs);
            let mut bodies = self.fetch(&segments, kind).await?;
            let mut rest = bodies.split_off(n_cand);
            let refs_bodies = rest.split_off(n_narrow);
            let cand_bodies = with_gaps(&candidates, bodies)?;
            let exact_ranges = self.rows_of_key(&candidates, &cand_bodies, key)?;
            exact = rows_in(&exact_ranges, None);
            decode_chunks(&infos, pending, rest, &mut decoded, 1)?;
            Self::take_ids(&mut reads, refs_bodies, &exact)?;
        }

        // W2 (warm) or W3 (cold): the frames of the payload columns.
        let payloads = self.finish_payloads(reads, kind, views).await?;
        self.batches(projection, exact, infos, decoded, payloads, output)?
            .collect()
    }

    /// Checks that every projected column is a narrow, payload or map column of the file.
    fn check_projection(&self, projection: &[usize]) -> Result<()> {
        for &column in projection {
            if column >= self.schema().fields().len() {
                return Err(StrataError::Schema {
                    reason: format!("no column {column}"),
                });
            }
            let record = self
                .directory()
                .get(column)
                .ok_or_else(|| StrataError::Schema {
                    reason: format!("no column {column}"),
                })?;
            if !matches!(
                record.layer,
                Layer::Narrow | Layer::Payload | Layer::MapParent
            ) {
                return Err(StrataError::Unsupported {
                    what: format!("projection of column {column}"),
                });
            }
        }
        Ok(())
    }

    /// Checks that the row ranges are non-empty, inside the file, sorted and disjoint.
    fn check_row_ranges(&self, ranges: Option<&[Range<u32>]>) -> Result<()> {
        let Some(ranges) = ranges else {
            return Ok(());
        };
        let mut previous_end = 0u64;
        for range in ranges {
            let (start, end) = (u64::from(range.start), u64::from(range.end));
            if start >= end || end > self.row_count() || start < previous_end {
                return Err(StrataError::Schema {
                    reason: "row ranges".to_string(),
                });
            }
            previous_end = end;
        }
        Ok(())
    }

    /// The filter wave. Fetches the chunks of the predicate columns that overlap a candidate
    /// range, decodes them into `decoded`, and returns the rows of the candidates for which the
    /// predicate is TRUE, in order.
    async fn filter(
        &self,
        plan: &FilterPlan<'_>,
        infos: &HashMap<usize, ColumnInfo>,
        candidates: &[Range<u32>],
        kind: ScanKind,
        limit: Option<usize>,
        decoded: &mut Decoded,
    ) -> Result<Vec<u32>> {
        let (b, masks, columns) = (plan.bound, plan.masks, plan.columns);
        let mut wanted: BTreeSet<(usize, usize)> = BTreeSet::new();
        for &column in columns {
            let info = info_of(infos, column)?;
            for index in overlapping_chunks(&info.entries, candidates) {
                wanted.insert((column, index));
            }
        }
        self.decode_wave(infos, &wanted, kind, decoded, plan.threads)
            .await?;

        let n_dirs = self.directory().len();
        let mut selection = Vec::new();
        let mut arrays: Vec<Option<ArrayRef>> = vec![None; n_dirs];
        for candidate in candidates {
            let mut window_start = candidate.start;
            while window_start < candidate.end {
                if limit.is_some_and(|n| selection.len() >= n) {
                    return Ok(selection);
                }
                // The rows of this window: at most `FILTER_WINDOW` rows of the candidate.
                let window_end = candidate
                    .end
                    .min(window_start.saturating_add(FILTER_WINDOW));
                let range = window_start..window_end;
                let len = usize::try_from(window_end - window_start).map_err(|_| {
                    StrataError::Corrupt {
                        what: "row range",
                        offset: u64::from(window_start),
                    }
                })?;
                arrays.fill(None);
                for &column in columns {
                    let info = info_of(infos, column)?;
                    let mut pieces: Vec<ArrayRef> = Vec::new();
                    for index in overlapping_chunks(&info.entries, std::slice::from_ref(&range)) {
                        let entry = info.entries.get(index).ok_or_else(|| StrataError::Schema {
                            reason: "internal: missing chunk".to_string(),
                        })?;
                        let first = entry.first_row;
                        let end = first.checked_add(entry.n_rows).ok_or_else(|| {
                            StrataError::Corrupt {
                                what: "block row order",
                                offset: u64::from(first),
                            }
                        })?;
                        let from = range.start.max(first);
                        let to = range.end.min(end);
                        let chunk = decoded_chunk(decoded, column, index)?;
                        let offset =
                            usize::try_from(from - first).map_err(|_| StrataError::Corrupt {
                                what: "chunk row",
                                offset: u64::from(first),
                            })?;
                        let length =
                            usize::try_from(to - from).map_err(|_| StrataError::Corrupt {
                                what: "row range",
                                offset: u64::from(from),
                            })?;
                        pieces.push(chunk.slice(offset, length));
                    }
                    let refs: Vec<&dyn Array> = pieces.iter().map(|piece| piece.as_ref()).collect();
                    let assembled = concat(&refs)?;
                    if let Some(slot) = arrays.get_mut(column) {
                        *slot = Some(assembled);
                    }
                }

                // The masks of this range: `Contains` masks are sliced, `MapKeyEq` masks are built
                // from the residual's rows.
                let mut range_masks: Vec<BooleanArray> = Vec::with_capacity(masks.sources.len());
                for (index, source) in masks.sources.iter().enumerate() {
                    let mask = match source {
                        MaskSource::Contains(..) => {
                            let full = masks.full.get(index).and_then(Option::as_ref).ok_or_else(
                                || StrataError::Schema {
                                    reason: format!("internal: missing contains mask {index}"),
                                },
                            )?;
                            let offset =
                                usize::try_from(range.start).map_err(|_| StrataError::Corrupt {
                                    what: "row range",
                                    offset: u64::from(range.start),
                                })?;
                            full.slice(offset, len)
                        }
                        MaskSource::MapKey(residual, key, lit) => {
                            let map = arrays.get(*residual).and_then(Option::as_ref).ok_or_else(
                                || StrataError::Schema {
                                    reason: format!("internal: missing column {residual}"),
                                },
                            )?;
                            map_key_mask(map.as_ref(), key, lit.as_ref())?
                        }
                    };
                    range_masks.push(mask);
                }
                let mask = evaluate(b, &arrays, &range_masks, len)?;
                for i in 0..len {
                    if mask.is_valid(i) && mask.value(i) {
                        selection.push(row_at(range.start, i)?);
                    }
                }
                window_start = window_end;
            }
        }
        Ok(selection)
    }

    /// Fetches, in one wave, the bodies of the chunks in `wanted` that are not decoded yet, and
    /// decodes them into `decoded`, on `threads` threads when the wave is large enough. A wave
    /// with nothing to fetch makes no request.
    /// The rows of `selection` (ascending) that rank in the first `top.k` by `top.column`, and
    /// the rows tied with the k-th, in ascending order. One wave decodes the column's chunks
    /// that hold a selected row, unless the filter wave already decoded them all.
    async fn top_rows(
        &self,
        top: &TopRows,
        infos: &HashMap<usize, ColumnInfo>,
        selection: Vec<u32>,
        kind: ScanKind,
        decoded: &mut Decoded,
        threads: usize,
    ) -> Result<Vec<u32>> {
        if selection.len() <= top.k {
            return Ok(selection);
        }
        let info = info_of(infos, top.column)?;
        let wanted: BTreeSet<(usize, usize)> = chunks_with_rows(&info.entries, &selection)
            .into_iter()
            .map(|index| (top.column, index))
            .filter(|key| !decoded.contains_key(key))
            .collect();
        if !wanted.is_empty() {
            self.decode_wave(infos, &wanted, kind, decoded, threads)
                .await?;
        }
        let values = take_rows(decoded, info, top.column, &selection)?;
        top_positions(values.as_ref(), top.options, top.k)?
            .into_iter()
            .map(|position| {
                selection
                    .get(position)
                    .copied()
                    .ok_or_else(|| StrataError::Schema {
                        reason: "internal: top row outside the selection".to_string(),
                    })
            })
            .collect()
    }

    async fn decode_wave(
        &self,
        infos: &HashMap<usize, ColumnInfo>,
        wanted: &BTreeSet<(usize, usize)>,
        kind: ScanKind,
        decoded: &mut Decoded,
        threads: usize,
    ) -> Result<()> {
        let (segments, pending) = plan_chunks(infos, wanted, decoded)?;
        let bodies = self.fetch_on(&segments, kind, threads).await?;
        decode_chunks(infos, pending, bodies, decoded, threads)
    }

    /// Plans the refs pages of the payload columns `columns` for `rows` (sorted, unique): one
    /// read per column, and the segments of all of them in one list.
    async fn plan_refs(
        &self,
        columns: &[usize],
        rows: &[u32],
    ) -> Result<(Vec<PayloadRead>, Vec<(SegmentRef, Kind)>)> {
        let mut reads = Vec::with_capacity(columns.len());
        let mut segments = Vec::new();
        for &column in columns {
            let record = self
                .directory()
                .get(column)
                .ok_or_else(|| StrataError::Schema {
                    reason: format!("no column {column}"),
                })?;
            let header = read_header(self, column).await?;
            let (page_segments, pages) = refs_segments(&header, rows)?;
            reads.push(PayloadRead {
                column,
                at: record.payload_offset,
                header,
                pages,
                n_pages: page_segments.len(),
                ids: Vec::new(),
            });
            segments.extend(page_segments);
        }
        Ok((reads, segments))
    }

    /// Decodes the fetched refs pages of `reads` (their bodies in the order of `plan_refs`) into
    /// the chunk ids of `rows`.
    fn take_ids(reads: &mut [PayloadRead], bodies: Vec<Vec<u8>>, rows: &[u32]) -> Result<()> {
        let mut bodies = bodies.into_iter();
        for read in reads.iter_mut() {
            let pages: Vec<Vec<u8>> = bodies.by_ref().take(read.n_pages).collect();
            read.ids = ids_of(&read.header, &read.pages, &pages, rows, read.at)?;
        }
        Ok(())
    }

    /// Plans the frames that the chunk ids of `reads` need. A frame is fetched and decoded once
    /// even when several columns share its store. Returns the frames' segments and entries, by
    /// file offset, and for each output batch the frames that no later batch uses.
    fn plan_frames(reads: &[PayloadRead]) -> Result<(Segments, Vec<FrameEntry>, Vec<Vec<u64>>)> {
        // The last batch that uses each frame, by offset.
        let mut last: BTreeMap<u64, (usize, FrameEntry)> = BTreeMap::new();
        for read in reads {
            let corrupt = || StrataError::Corrupt {
                what: "payload chunk id",
                offset: read.at,
            };
            let store = &read.header.store;
            for (batch, rows) in read.ids.chunks(BATCH_ROWS).enumerate() {
                // Most ids fall in the frame of the id before them: that frame is tried first,
                // and recorded once per batch.
                let mut current: Option<(u32, u32)> = None;
                for &id in rows.iter().flatten().flatten() {
                    if let Some((first, n)) = current
                        && id >= first
                        && id - first < n
                    {
                        continue;
                    }
                    let index = store.frame_of(id).ok_or_else(corrupt)?;
                    let entry = store.frames.get(index).ok_or_else(corrupt)?;
                    current = Some((entry.first_chunk, entry.n_chunks));
                    last.entry(entry.offset)
                        .and_modify(|(used, _)| *used = (*used).max(batch))
                        .or_insert_with(|| (batch, entry.clone()));
                }
            }
        }
        let batches = reads
            .iter()
            .map(|read| read.ids.len().div_ceil(BATCH_ROWS))
            .max()
            .unwrap_or(0);
        let mut release: Vec<Vec<u64>> = vec![Vec::new(); batches];
        let mut entries = Vec::with_capacity(last.len());
        let mut segments = Vec::with_capacity(last.len());
        for (offset, (batch, entry)) in last {
            if let Some(slot) = release.get_mut(batch) {
                slot.push(offset);
            }
            segments.push((
                SegmentRef {
                    offset: entry.offset,
                    len: entry.len,
                },
                Kind::PayloadFrame,
            ));
            entries.push(entry);
        }
        Ok((segments, entries, release))
    }

    /// The frame wave of `reads`: one fetch for the frames their chunk ids need, kept as stored
    /// until a batch needs them.
    async fn finish_payloads(
        &self,
        reads: Vec<PayloadRead>,
        kind: ScanKind,
        views: bool,
    ) -> Result<PayloadValues> {
        let (segments, entries, release) = Self::plan_frames(&reads)?;
        let stored = self.fetch_stored(&segments, kind).await?;
        let dicts = entries.iter().map(|e| self.frame_dict(e.offset)).collect();
        let frames = FrameBodies::stored(entries, stored, dicts)?;
        let fields = reads
            .iter()
            .map(|read| self.field_of(read.column))
            .collect::<Result<_>>()?;
        Ok(PayloadValues {
            reads,
            fields,
            frames,
            release,
            views,
        })
    }

    /// The payload columns `columns` over `rows` (sorted, unique): the refs pages in one wave,
    /// then the frames in one wave.
    async fn payload_values(
        &self,
        columns: &[usize],
        rows: &[u32],
        kind: ScanKind,
        views: bool,
    ) -> Result<PayloadValues> {
        let (mut reads, segments) = self.plan_refs(columns, rows).await?;
        let bodies = self.fetch(&segments, kind).await?;
        Self::take_ids(&mut reads, bodies, rows)?;
        self.finish_payloads(reads, kind, views).await
    }

    /// The batches of `selection` (ascending rows) for `projection`, from the decoded narrow
    /// chunks and the payload values, built on every core.
    fn batches(
        &self,
        projection: &[usize],
        selection: Vec<u32>,
        infos: HashMap<usize, ColumnInfo>,
        decoded: Decoded,
        payloads: PayloadValues,
        output: SchemaRef,
    ) -> Result<ScanBatches> {
        let columns = output_columns(self, projection, &payloads.reads)?;
        Ok(ScanBatches {
            columns,
            selection,
            infos,
            decoded,
            payloads: Some(payloads),
            output,
            next: 0,
            ready: std::collections::VecDeque::new(),
            threads: crate::parallel::cores(),
        })
    }
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
    use crate::narrow::Encoding;
    use crate::predicate::{CmpOp, Lit, literal};
    use crate::reader::read_column;
    use crate::testgen::{assert_same, wide_batch, wide_schema};
    use crate::writer::StrataWriter;
    use arrow::array::{BinaryArray, BooleanArray, StringArray};
    use arrow::compute::{and_kleene, filter_record_batch, not, or_kleene};
    use arrow::datatypes::DataType;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use proptest::prelude::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// `wide_batch(rows)`, written with `config`, and opened.
    async fn open_wide(rows: usize, config: &Config) -> StrataFile {
        open_wide_with(rows, config, &Config::default()).await
    }

    /// `wide_batch(rows)` written with `config` and opened with `open`.
    async fn open_wide_with(rows: usize, config: &Config, open: &Config) -> StrataFile {
        let mut w =
            StrataWriter::new(wide_schema(), &["trace_id", "start_time"], config.clone()).unwrap();
        w.write(&wide_batch(rows)).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("table.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        StrataFile::open(store, path, None, open).await.unwrap()
    }

    /// An open that keeps no metadata section: queries read the metadata they need, in a wave
    /// of their own, as for a section over `open_metadata_max`.
    fn lazy() -> Config {
        Config {
            open_metadata_max: 0,
            ..Config::default()
        }
    }

    /// The column index of `name` in the wide schema.
    fn col(name: &str) -> usize {
        wide_schema().index_of(name).unwrap()
    }

    /// The bytes of row `i` of a Utf8 or Binary array, or `None` for a null row.
    fn bytes_at(array: &dyn Array, i: usize) -> Option<&[u8]> {
        if let Some(binaries) = array.as_any().downcast_ref::<BinaryArray>() {
            return binaries.is_valid(i).then(|| binaries.value(i));
        }
        let strings = array.as_any().downcast_ref::<StringArray>()?;
        strings.is_valid(i).then(|| strings.value(i).as_bytes())
    }

    /// Whether `hay` contains `needle`. The empty needle is in every value.
    fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
        needle.is_empty() || hay.windows(needle.len()).any(|window| window == needle)
    }

    /// The mask of `p` over whole decoded columns, evaluated leaf by leaf with the same kernels
    /// and literal conversion as the engine, but without pruning, shredding or payload reads:
    /// `MapKeyEq` runs over the whole map, and `Contains` by brute force.
    fn oracle_mask(
        p: &Predicate,
        file: &StrataFile,
        columns: &[Option<ArrayRef>],
        rows: usize,
    ) -> Result<BooleanArray> {
        let leaf = |b: Bound| evaluate(&b, columns, &[], rows);
        let column = |c: usize| -> Result<ArrayRef> {
            columns
                .get(c)
                .and_then(Option::clone)
                .ok_or_else(|| StrataError::Schema {
                    reason: format!("no column {c}"),
                })
        };
        let data_type = |c: usize| file.schema().field(c).data_type().clone();
        Ok(match p {
            Predicate::And(terms) => {
                let mut acc = BooleanArray::from(vec![true; rows]);
                for term in terms {
                    acc = and_kleene(&acc, &oracle_mask(term, file, columns, rows)?)?;
                }
                acc
            }
            Predicate::Or(terms) => {
                let mut acc = BooleanArray::from(vec![false; rows]);
                for term in terms {
                    acc = or_kleene(&acc, &oracle_mask(term, file, columns, rows)?)?;
                }
                acc
            }
            Predicate::Not(inner) => not(&oracle_mask(inner, file, columns, rows)?)?,
            Predicate::Cmp(c, op, lit) => {
                leaf(Bound::Cmp(*c, *op, literal(*c, &data_type(*c), lit)?))?
            }
            Predicate::In(c, lits) => {
                let mut acc = BooleanArray::from(vec![false; rows]);
                for lit in lits {
                    let term = Bound::Cmp(*c, CmpOp::Eq, literal(*c, &data_type(*c), lit)?);
                    acc = or_kleene(&acc, &leaf(term)?)?;
                }
                acc
            }
            Predicate::IsNull(c) => leaf(Bound::IsNull(*c))?,
            Predicate::IsNotNull(c) => leaf(Bound::IsNotNull(*c))?,
            Predicate::Const(v) => leaf(Bound::Const(*v))?,
            Predicate::MapKeyEq(c, key, lit) => {
                let value = literal(*c, &DataType::Utf8, lit)?;
                map_key_mask(column(*c)?.as_ref(), key, value.as_ref())?
            }
            Predicate::Contains(c, needle) => {
                let values = column(*c)?;
                let found: Vec<bool> = (0..rows)
                    .map(|i| {
                        bytes_at(values.as_ref(), i).is_some_and(|v| contains_bytes(v, needle))
                    })
                    .collect();
                BooleanArray::from(found)
            }
        })
    }

    /// The rows of the full table that match `p` within `ranges`, projected onto `projection`.
    /// `p` is bound first, so its errors are those of `bind`.
    async fn oracle(
        file: &StrataFile,
        p: Option<&Predicate>,
        ranges: Option<&[Range<u32>]>,
        projection: &[usize],
    ) -> Result<RecordBatch> {
        let schema = file.schema();
        let rows = usize::try_from(file.row_count()).unwrap();
        let mut columns: Vec<Option<ArrayRef>> = Vec::new();
        for c in 0..schema.fields().len() {
            columns.push(Some(read_column(file, c).await?));
        }
        let full = RecordBatch::try_new(
            Arc::clone(&schema),
            columns.iter().map(|c| c.clone().unwrap()).collect(),
        )?;
        let mut keep = vec![true; rows];
        if let Some(p) = p {
            bind(p, file)?;
            let mask = oracle_mask(p, file, &columns, rows)?;
            for (i, k) in keep.iter_mut().enumerate() {
                *k = *k && mask.is_valid(i) && mask.value(i);
            }
        }
        if let Some(ranges) = ranges {
            for (i, k) in keep.iter_mut().enumerate() {
                let in_range = ranges
                    .iter()
                    .any(|r| (r.start as usize..r.end as usize).contains(&i));
                *k = *k && in_range;
            }
        }
        let projected = full.project(projection)?;
        Ok(filter_record_batch(&projected, &BooleanArray::from(keep))?)
    }

    /// Concatenates the batches of a scan, or builds an empty batch of `schema`.
    fn joined(schema: &SchemaRef, batches: &[RecordBatch]) -> RecordBatch {
        if batches.is_empty() {
            return RecordBatch::new_empty(Arc::clone(schema));
        }
        arrow::compute::concat_batches(schema, batches).unwrap()
    }

    /// Asserts that `got` has the same rows and columns as `want`.
    fn assert_batches_equal(got: &RecordBatch, want: &RecordBatch) {
        assert_eq!(got.schema(), want.schema());
        assert_eq!(got.num_rows(), want.num_rows(), "row count");
        for c in 0..want.num_columns() {
            assert_same(got.column(c).as_ref(), want.column(c).as_ref());
        }
    }

    fn entry(first_row: u32, n_rows: u32) -> BlockEntry {
        BlockEntry {
            first_row,
            n_rows,
            segment_offset: 0,
            segment_len: 0,
            encoding: crate::narrow::Encoding::ConstNull,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        }
    }

    #[test]
    fn chunks_with_rows_finds_each_chunk_holding_a_row() {
        let entries = vec![entry(0, 10), entry(10, 10), entry(20, 5), entry(25, 100)];
        assert_eq!(chunks_with_rows(&entries, &[]), Vec::<usize>::new());
        assert_eq!(chunks_with_rows(&entries, &[0, 9]), vec![0]);
        assert_eq!(chunks_with_rows(&entries, &[3, 21, 124]), vec![0, 2, 3]);
        assert_eq!(chunks_with_rows(&entries, &[10, 11, 12, 30]), vec![1, 3]);
        let all: Vec<u32> = (0..125).collect();
        assert_eq!(chunks_with_rows(&entries, &all), vec![0, 1, 2, 3]);
    }

    #[test]
    fn scan_batches_stream_and_drop_frames_after_their_last_batch() {
        runtime().block_on(async {
            // 30,000 rows: four batches. Values repeat or extend values of earlier batches, so
            // frames are shared across batches.
            let n = 30_000usize;
            let schema = Arc::new(Schema::new(vec![
                Field::new("trace_id", DataType::FixedSizeBinary(16), false),
                Field::new("text", DataType::Utf8, true),
            ]));
            let ids: Vec<[u8; 16]> = (0..n).map(|i| (i as u128).to_be_bytes()).collect();
            let mut text: Vec<Option<String>> = Vec::with_capacity(n);
            for i in 0..n {
                let value = match i % 5 {
                    0 if i > 9_000 => text[i - 8_999].clone(),
                    1 if i > 20_000 => text[i - 19_001].clone().map(|v| format!("{v} and {i}")),
                    2 => None,
                    _ => Some(format!("row {i} {}", "é".repeat(200 + i % 300))),
                };
                text.push(value);
            }
            let batch = RecordBatch::try_new(
                Arc::clone(&schema),
                vec![
                    Arc::new(
                        arrow::array::FixedSizeBinaryArray::try_from_iter(ids.iter()).unwrap(),
                    ),
                    Arc::new(StringArray::from(text.clone())),
                ],
            )
            .unwrap();
            let mut w = StrataWriter::new(schema, &["trace_id"], Config::default()).unwrap();
            w.write(&batch).unwrap();
            let (bytes, _) = w.finish().unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let path = Path::from("text.strata");
            store.put(&path, PutPayload::from(bytes)).await.unwrap();
            let file = StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap();
            assert_eq!(
                file.directory()[1].layer,
                Layer::Payload,
                "the text column is a payload column"
            );

            for views in [false, true] {
                for threads in [1, 2, 3, 8] {
                    let req = ScanRequest {
                        projection: vec![1, 0],
                        predicate: None,
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Scan,
                        views,
                        top: None,
                    };
                    let mut batches = file.scan_batches(&req, threads).await.unwrap();
                    // Four batches, built `threads` at a time.
                    assert_eq!(batches.steps_left(), 4usize.div_ceil(threads));
                    let all = batches.frames_held();
                    let mut got: Vec<Option<String>> = Vec::with_capacity(n);
                    let mut held = Vec::new();
                    while let Some(batch) = batches.next() {
                        let batch = batch.unwrap();
                        let values = cast(batch.column(0), &DataType::Utf8).unwrap();
                        let values = arrow::array::AsArray::as_string::<i32>(values.as_ref());
                        got.extend(values.iter().map(|v| v.map(str::to_string)));
                        held.push(batches.frames_held());
                    }
                    assert_eq!(got, text, "views {views}, {threads} threads");
                    assert_eq!(held.len(), 4);
                    assert_eq!(batches.frames_held(), 0);
                    assert_eq!(batches.steps_left(), 0);
                    if threads == 1 {
                        // Each batch drops the frames no later batch uses. The first batch's
                        // frames are almost all used again by the second.
                        assert!(
                            held.windows(2).all(|w| w[0] > w[1]) && held[0] <= all,
                            "{all} then {held:?}"
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn a_views_scan_matches_a_plain_scan() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let names = ["span_kind", "status", "model", "environment", "cost", "uid"];
            let projection: Vec<usize> = names
                .iter()
                .filter_map(|n| wide_schema().index_of(n).ok())
                .collect();
            let predicates = [
                None,
                Some(Predicate::Cmp(
                    col("status"),
                    CmpOp::Eq,
                    Lit::Utf8("error".into()),
                )),
                Some(Predicate::Cmp(
                    col("model"),
                    CmpOp::NotEq,
                    Lit::Utf8("gpt-4o".into()),
                )),
            ];
            for predicate in predicates {
                let req = |views| ScanRequest {
                    projection: projection.clone(),
                    predicate: predicate.clone(),
                    row_ranges: Some(vec![100..7_000, 9_000..19_500]),
                    limit: None,
                    kind: ScanKind::Scan,
                    views,
                    top: None,
                };
                let plain = file.scan(&req(false)).await.unwrap();
                let viewed = file.scan(&req(true)).await.unwrap();
                assert_eq!(plain.len(), viewed.len());
                for (p, v) in plain.iter().zip(&viewed) {
                    assert_eq!(p.num_rows(), v.num_rows());
                    for (c, field) in v.schema().fields().iter().enumerate() {
                        let want = p.column(c);
                        if want.data_type() == &DataType::Utf8 {
                            assert_eq!(field.data_type(), &DataType::Utf8View);
                            let back = cast(v.column(c), &DataType::Utf8).unwrap();
                            assert_eq!(back.as_ref(), want.as_ref());
                        } else {
                            assert_eq!(v.column(c).as_ref(), want.as_ref());
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn may_match_rules_out_a_file_only_when_no_row_can_match() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let t0: i64 = 1_700_000_000_000_000_000;
            let time = |op, ns: i64| Predicate::Cmp(col("start_time"), op, Lit::Int(ns));
            let cases = [
                (time(CmpOp::Lt, t0), false),
                (time(CmpOp::GtEq, t0 + 20_000 * 1_000_003), false),
                (time(CmpOp::GtEq, t0 + 10_000 * 1_000_003), true),
                (
                    Predicate::Cmp(col("status"), CmpOp::Eq, Lit::Utf8("no such status".into())),
                    false,
                ),
                (
                    Predicate::Cmp(col("status"), CmpOp::Eq, Lit::Utf8("error".into())),
                    true,
                ),
            ];
            for (predicate, want) in cases {
                assert_eq!(
                    file.may_match(&predicate).await.unwrap(),
                    want,
                    "{predicate:?}"
                );
                if !want {
                    let req = ScanRequest {
                        projection: vec![0],
                        predicate: Some(predicate.clone()),
                        row_ranges: None,
                        limit: None,
                        kind: ScanKind::Scan,
                        views: false,
                        top: None,
                    };
                    let rows: usize = file
                        .scan(&req)
                        .await
                        .unwrap()
                        .iter()
                        .map(RecordBatch::num_rows)
                        .sum();
                    assert_eq!(rows, 0, "{predicate:?}");
                }
            }
        });
    }

    /// Runs `req` on `file` and compares the result with the oracle.
    async fn check(file: &StrataFile, p: Predicate, projection: &[usize]) {
        let req = ScanRequest {
            projection: projection.to_vec(),
            predicate: Some(p.clone()),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
        };
        let batches = file.scan(&req).await.unwrap();
        let want = oracle(file, Some(&p), None, projection).await.unwrap();
        let got = joined(&want.schema(), &batches);
        assert_batches_equal(&got, &want);
    }

    fn lit_int(v: i64) -> Lit {
        Lit::Int(v)
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn subtract_all_splits_trims_drops_and_keeps() {
        // A split.
        assert_eq!(subtract_all(&[0..10], &[3..5]), vec![0..3, 5..10]);
        // A trim at either end.
        assert_eq!(subtract_all(&[0..10], &[0..4]), vec![4..10]);
        assert_eq!(subtract_all(&[0..10], &[7..12]), vec![0..7]);
        // A dropped range.
        assert_eq!(subtract_all(&[0..5, 8..9], &[0..5]), vec![8..9]);
        // A no-op.
        assert_eq!(subtract_all(&[0..5], &[7..9]), vec![0..5]);
        // One removal spanning several candidate ranges, and several removals in one range.
        assert_eq!(
            subtract_all(&[0..4, 6..10, 12..14], &[2..13]),
            vec![0..2, 13..14]
        );
        assert_eq!(
            subtract_all(&[0..10, 20..30], &[1..2, 4..5, 8..25]),
            vec![0..1, 2..4, 5..8, 25..30]
        );
        assert!(subtract_all(&[3..6], &[0..10]).is_empty());
        assert_eq!(subtract_all(&[0..10], &[]), vec![0..10]);
    }

    #[test]
    fn subtract_all_matches_a_brute_force_removal() {
        let ranges = vec![0..3, 5..9, 10..11, 14..20];
        let remove = vec![2..6, 7..8, 9..15];
        let want: Vec<u32> = (0..20u32)
            .filter(|row| {
                ranges.iter().any(|r| r.contains(row)) && !remove.iter().any(|r| r.contains(row))
            })
            .collect();
        let got: Vec<u32> = subtract_all(&ranges, &remove)
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn overlapping_chunks_picks_the_chunks_of_the_ranges() {
        let entry = |first_row: u32, n_rows: u32| BlockEntry {
            first_row,
            n_rows,
            segment_offset: 0,
            segment_len: 0,
            encoding: Encoding::ConstNull,
            flags: 0,
            const_len: 0,
            min: [0; 16],
            max: [0; 16],
        };
        let entries = [entry(0, 10), entry(10, 10), entry(20, 10)];
        assert_eq!(overlapping_chunks(&entries, &[5..12]), vec![0, 1]);
        assert_eq!(overlapping_chunks(&entries, &[20..21]), vec![2]);
        assert!(overlapping_chunks(&entries, &[30..40]).is_empty());
    }

    #[test]
    fn comparisons_and_null_handling_match_the_oracle() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let tokens = col("input_tokens");
            let cases: Vec<Predicate> = vec![
                Predicate::Cmp(tokens, CmpOp::Eq, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::NotEq, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::Lt, lit_int(-1)),
                Predicate::Cmp(tokens, CmpOp::Lt, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::LtEq, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::Gt, lit_int(10_000)),
                Predicate::Cmp(tokens, CmpOp::Gt, lit_int(2_500)),
                Predicate::Cmp(tokens, CmpOp::GtEq, lit_int(4_999)),
                Predicate::Cmp(tokens, CmpOp::Eq, Lit::Null),
                Predicate::Not(Box::new(Predicate::Cmp(tokens, CmpOp::Lt, lit_int(2_500)))),
                Predicate::IsNull(tokens),
                Predicate::IsNotNull(col("cost")),
                Predicate::Cmp(col("cost"), CmpOp::Gt, Lit::Float(100.0)),
                Predicate::Cmp(col("cost"), CmpOp::Lt, Lit::Float(0.5)),
                Predicate::Cmp(col("cost"), CmpOp::Eq, Lit::Float(f64::NAN)),
                Predicate::Cmp(col("f32"), CmpOp::Eq, Lit::Float(0.1)),
                Predicate::Cmp(col("f32"), CmpOp::Gt, Lit::Float(5_000.0)),
                Predicate::Cmp(col("f32"), CmpOp::Lt, Lit::Float(0.0)),
                Predicate::Cmp(
                    col("start_time"),
                    CmpOp::Gt,
                    Lit::Int(1_700_000_000_000_000_000 + 7_000 * 1_000_003),
                ),
                Predicate::Cmp(
                    col("start_time"),
                    CmpOp::LtEq,
                    Lit::Int(1_700_000_000_000_000_000 + 7_000 * 1_000_003),
                ),
                Predicate::Cmp(
                    col("trace_id"),
                    CmpOp::Eq,
                    Lit::Bytes(((7_000 / 5) as u128).to_be_bytes().to_vec()),
                ),
                Predicate::Cmp(
                    col("trace_id"),
                    CmpOp::Gt,
                    Lit::Bytes(((7_000 / 5) as u128).to_be_bytes().to_vec()),
                ),
                Predicate::Cmp(col("model"), CmpOp::Eq, Lit::Utf8("gpt-4o".into())),
                Predicate::Cmp(col("model"), CmpOp::Eq, Lit::Utf8("missing".into())),
                Predicate::In(tokens, vec![]),
                Predicate::In(tokens, vec![lit_int(2_500)]),
                Predicate::In(tokens, vec![lit_int(2_500), Lit::Null, lit_int(10)]),
                Predicate::IsNull(col("model")),
                Predicate::And(vec![
                    Predicate::Cmp(tokens, CmpOp::Gt, lit_int(1_000)),
                    Predicate::Cmp(col("status"), CmpOp::Eq, Lit::Utf8("ok".into())),
                ]),
                Predicate::Or(vec![
                    Predicate::Cmp(col("span_kind"), CmpOp::Eq, Lit::Utf8("llm".into())),
                    Predicate::Cmp(col("model"), CmpOp::Eq, Lit::Utf8("claude-3-haiku".into())),
                ]),
            ];
            let projection = [col("trace_id"), tokens, col("model")];
            for p in cases {
                check(&file, p.clone(), &projection).await;
            }
        });
    }

    #[test]
    fn a_float_zone_prunes_only_when_no_chunk_holds_nan() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let f32_col = col("f32");
            // Warm the metadata, so the counters below see only the filter wave.
            file.block_entries(f32_col).await.unwrap();
            file.reset_io_stats();
            let req = ScanRequest {
                projection: vec![f32_col],
                predicate: Some(Predicate::Cmp(f32_col, CmpOp::Gt, Lit::Float(5_000.0))),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            let batches = file.scan(&req).await.unwrap();
            let stats = file.io_stats();
            // Chunks 0 and 1 are pruned by their zones: only the last three chunks are read.
            assert_eq!(stats.waves, 1);
            assert!(stats.requests < 5, "{} requests", stats.requests);
            let want = oracle(&file, req.predicate.as_ref(), None, &[f32_col])
                .await
                .unwrap();
            assert!(want.num_rows() > 0);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);
        });
    }

    #[test]
    fn a_predicate_on_a_pruned_chunk_reads_only_the_candidates() {
        runtime().block_on(async {
            for (open, metadata) in [(Config::default(), 0), (lazy(), 1)] {
                let file = open_wide_with(20_000, &Config::default(), &open).await;
                let time = col("start_time");
                // The `start_time` of row 19,000, from the generator's formula: the file is cold.
                let threshold = 1_700_000_000_000_000_000i64 + 19_000 * 1_000_003;
                file.reset_io_stats();
                let req = ScanRequest {
                    projection: vec![time],
                    predicate: Some(Predicate::Cmp(time, CmpOp::GtEq, Lit::Int(threshold))),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Scan,
                    views: false,
                    top: None,
                };
                let batches = file.scan(&req).await.unwrap();
                let stats = file.io_stats();
                // The metadata when the open did not keep it, then the filter. The projection is
                // the predicate column: no more waves.
                assert_eq!(stats.waves, 1 + metadata);
                assert_eq!(stats.requests, 1 + metadata);
                let want = oracle(&file, req.predicate.as_ref(), None, &[time])
                    .await
                    .unwrap();
                assert_batches_equal(&joined(&want.schema(), &batches), &want);
            }
        });
    }

    #[test]
    fn an_absent_dictionary_value_costs_only_the_metadata() {
        runtime().block_on(async {
            for (open, metadata) in [(Config::default(), 0), (lazy(), 1)] {
                let file = open_wide_with(20_000, &Config::default(), &open).await;
                let status = col("status");
                file.reset_io_stats();
                let req = ScanRequest {
                    projection: vec![status],
                    predicate: Some(Predicate::Cmp(
                        status,
                        CmpOp::Eq,
                        Lit::Utf8("missing".into()),
                    )),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Scan,
                    views: false,
                    top: None,
                };
                let batches = file.scan(&req).await.unwrap();
                assert!(batches.is_empty());
                assert_eq!(file.io_stats().waves, metadata);
            }
        });
    }

    #[test]
    fn row_ranges_restrict_the_rows() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let projection = [col("trace_id"), col("input_tokens")];
            let ranges = vec![100..200, 5_000..5_010];
            let req = ScanRequest {
                projection: projection.to_vec(),
                predicate: None,
                row_ranges: Some(ranges.clone()),
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            let batches = file.scan(&req).await.unwrap();
            let want = oracle(&file, None, Some(ranges.as_slice()), &projection)
                .await
                .unwrap();
            assert_eq!(want.num_rows(), 110);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);

            // With a predicate, the result is the intersection with the oracle.
            let p = Predicate::Cmp(col("input_tokens"), CmpOp::Gt, lit_int(2_500));
            let req = ScanRequest {
                projection: projection.to_vec(),
                predicate: Some(p.clone()),
                row_ranges: Some(vec![100..9_000, 15_000..15_050]),
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            let batches = file.scan(&req).await.unwrap();
            let restricted = [100..9_000, 15_000..15_050];
            let want = oracle(&file, Some(&p), Some(restricted.as_slice()), &projection)
                .await
                .unwrap();
            assert!(want.num_rows() > 0);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);
        });
    }

    #[test]
    fn limits_take_the_first_rows_and_zero_reads_nothing() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let projection = [col("trace_id"), col("input_tokens")];
            let p = Predicate::Cmp(col("cost"), CmpOp::Gt, Lit::Float(1.0));
            let req = ScanRequest {
                projection: projection.to_vec(),
                predicate: Some(p.clone()),
                row_ranges: None,
                limit: Some(7),
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            let batches = file.scan(&req).await.unwrap();
            let want = oracle(&file, Some(&p), None, &projection)
                .await
                .unwrap()
                .slice(0, 7);
            assert_eq!(want.num_rows(), 7);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);

            file.reset_io_stats();
            let zero = ScanRequest {
                limit: Some(0),
                ..req.clone()
            };
            assert!(file.scan(&zero).await.unwrap().is_empty());
            assert_eq!(file.io_stats().waves, 0);
        });
    }

    #[test]
    fn an_empty_projection_counts_the_matching_rows() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let p = Predicate::Cmp(col("input_tokens"), CmpOp::Gt, lit_int(2_500));
            let req = ScanRequest {
                projection: Vec::new(),
                predicate: Some(p.clone()),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            let batches = file.scan(&req).await.unwrap();
            assert!(batches.iter().all(|b| b.num_columns() == 0));
            let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
            let want = oracle(&file, Some(&p), None, &[]).await.unwrap();
            assert_eq!(rows, want.num_rows());
        });
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn bad_requests_are_schema_or_unsupported_errors() {
        runtime().block_on(async {
            let file = open_wide(2_000, &Config::default()).await;
            let scan = |projection: Vec<usize>,
                        predicate: Option<Predicate>,
                        ranges: Option<Vec<Range<u32>>>| {
                let req = ScanRequest {
                    projection,
                    predicate,
                    row_ranges: ranges,
                    limit: None,
                    kind: ScanKind::Scan,
                    views: false,
                    top: None,
                };
                let file = &file;
                async move { file.scan(&req).await }
            };
            let trace = col("trace_id");
            let tokens = col("input_tokens");

            assert!(matches!(
                scan(vec![99], None, None).await,
                Err(StrataError::Schema { .. })
            ));
            assert!(matches!(
                scan(vec![trace], None, Some(vec![5..9, 0..3])).await,
                Err(StrataError::Schema { .. })
            ));
            assert!(matches!(
                scan(vec![trace], None, Some(vec![5..5])).await,
                Err(StrataError::Schema { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(trace, CmpOp::Eq, Lit::Bytes(vec![0; 15]))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(tokens, CmpOp::Eq, Lit::Float(3.5))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(tokens, CmpOp::Eq, Lit::UInt(u64::MAX))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(tokens, CmpOp::Eq, Lit::Utf8("x".into()))),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(matches!(
                scan(
                    vec![trace],
                    Some(Predicate::Cmp(col("attrs"), CmpOp::Eq, Lit::Null)),
                    None
                )
                .await,
                Err(StrataError::Unsupported { .. })
            ));
            assert!(scan(vec![col("attrs")], None, None).await.is_ok());
            let mixed = Predicate::And(vec![
                Predicate::Cmp(col("status"), CmpOp::Eq, Lit::Utf8("missing".into())),
                Predicate::Cmp(trace, CmpOp::Eq, Lit::Bytes(vec![0; 3])),
            ]);
            assert!(matches!(
                scan(vec![trace], Some(mixed), None).await,
                Err(StrataError::Unsupported { .. })
            ));
        });
    }

    fn lit_strategy() -> impl Strategy<Value = Lit> {
        prop_oneof![
            Just(Lit::Null),
            any::<bool>().prop_map(Lit::Bool),
            any::<i64>().prop_map(Lit::Int),
            any::<u64>().prop_map(Lit::UInt),
            any::<f64>().prop_map(Lit::Float),
            "[a-z0-9-]{0,12}".prop_map(Lit::Utf8),
            proptest::collection::vec(any::<u8>(), 0..20).prop_map(Lit::Bytes),
        ]
    }

    fn op_strategy() -> impl Strategy<Value = CmpOp> {
        prop_oneof![
            Just(CmpOp::Eq),
            Just(CmpOp::NotEq),
            Just(CmpOp::Lt),
            Just(CmpOp::LtEq),
            Just(CmpOp::Gt),
            Just(CmpOp::GtEq),
        ]
    }

    fn predicate_strategy() -> impl Strategy<Value = Predicate> {
        let leaf = prop_oneof![
            (0usize..16, op_strategy(), lit_strategy())
                .prop_map(|(c, op, lit)| Predicate::Cmp(c, op, lit)),
            (0usize..16, proptest::collection::vec(lit_strategy(), 0..3))
                .prop_map(|(c, lits)| Predicate::In(c, lits)),
            (0usize..16).prop_map(Predicate::IsNull),
            (0usize..16).prop_map(Predicate::IsNotNull),
            prop_oneof![Just(Some(true)), Just(Some(false)), Just(None)].prop_map(Predicate::Const),
        ];
        leaf.prop_recursive(3, 16, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..3).prop_map(Predicate::And),
                proptest::collection::vec(inner.clone(), 0..3).prop_map(Predicate::Or),
                inner.prop_map(|p| Predicate::Not(Box::new(p))),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn random_predicates_never_panic_and_agree_with_the_oracle(p in predicate_strategy()) {
            let file = runtime().block_on(open_wide(3_000, &Config::default()));
            runtime().block_on(async {
                let req = ScanRequest {
                    projection: vec![col("trace_id"), col("input_tokens")],
                    predicate: Some(p.clone()),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Scan,
                    views: false,
                    top: None,
                };
                let got = file.scan(&req).await;
                let want = oracle(&file, Some(&p), None, &req.projection).await;
                match (got, want) {
                    (Ok(batches), Ok(want)) => {
                        assert_batches_equal(&joined(&want.schema(), &batches), &want);
                    }
                    (Err(got), Err(want)) => {
                        assert_eq!(
                            std::mem::discriminant(&got),
                            std::mem::discriminant(&want),
                            "{got} vs {want}"
                        );
                    }
                    (got, want) => panic!("scan {got:?} but oracle {want:?}"),
                }
            });
        }
    }

    /// `wide_batch` with every Utf8 and Binary column stored as payload.
    fn payload_config() -> Config {
        Config {
            payload_min_avg: 0,
            ..Config::default()
        }
    }

    /// `batch(rows)`, written with the default config, and opened.
    async fn open_batch(rows: usize) -> StrataFile {
        open_batch_with(rows, &Config::default()).await
    }

    /// `testgen::batch(rows)` written with the default config and opened with `open`.
    async fn open_batch_with(rows: usize, open: &Config) -> StrataFile {
        let mut w = StrataWriter::new(
            crate::testgen::schema(),
            &["trace_id", "start_time"],
            Config::default(),
        )
        .unwrap();
        w.write(&crate::testgen::batch(rows)).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("batch.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        StrataFile::open(store, path, None, open).await.unwrap()
    }

    /// The rows whose `trace_id` (column 0) is `key`, from a scan: the oracle of the lookups.
    async fn trace_rows(file: &StrataFile, key: u128, projection: &[usize]) -> RecordBatch {
        let req = ScanRequest {
            projection: projection.to_vec(),
            predicate: Some(Predicate::Cmp(
                0,
                CmpOp::Eq,
                Lit::Bytes(key.to_be_bytes().to_vec()),
            )),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
            top: None,
        };
        let batches = file.scan(&req).await.unwrap();
        let schema = Arc::new(file.schema().project(projection).unwrap());
        joined(&schema, &batches)
    }

    /// `traces` traces of the datagen, sorted, written with the default config, and opened.
    async fn open_datagen(traces: usize) -> StrataFile {
        let world = fmt_testdata::datagen::World::new(7);
        let mut rng = fmt_testdata::datagen::Rng::new(42);
        let mut spans = Vec::new();
        for _ in 0..traces {
            world.trace(&mut rng, &mut spans);
        }
        spans.sort_by_key(|s| (s.trace_id, s.start_time));
        let schema = cetra_core::schema::spans_schema();
        let mut w =
            StrataWriter::new(schema, &["trace_id", "start_time"], Config::default()).unwrap();
        for chunk in spans.chunks(8192) {
            w.write(&cetra_core::batch::spans_to_record_batch(chunk).unwrap())
                .unwrap();
        }
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("datagen.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap()
    }

    #[test]
    fn map_key_predicates_match_the_oracle_and_a_promoted_key_takes_two_waves() {
        runtime().block_on(async {
            let lazy_file = open_wide_with(20_000, &Config::default(), &lazy()).await;
            let file = open_wide(20_000, &Config::default()).await;
            let (attrs, uid) = (col("attrs"), col("uid"));
            let key = |name: &str, lit: Lit| Predicate::MapKeyEq(attrs, name.to_string(), lit);

            // `k0` is promoted: it binds to its child column, and a cold scan takes two waves,
            // the filter and the projection, or three when the open kept no metadata.
            let promoted = key("k0", Lit::Utf8("v5-0".into()));
            assert!(matches!(
                bind(&promoted, &file).unwrap().0,
                Bound::Cmp(_, CmpOp::Eq, _)
            ));
            let req = ScanRequest {
                projection: vec![uid],
                predicate: Some(promoted.clone()),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            lazy_file.scan(&req).await.unwrap();
            assert_eq!(lazy_file.io_stats().waves, 3);
            file.reset_io_stats();
            let batches = file.scan(&req).await.unwrap();
            assert_eq!(file.io_stats().waves, 2);
            let want = oracle(&file, Some(&promoted), None, &[uid]).await.unwrap();
            assert_batches_equal(&joined(&want.schema(), &batches), &want);

            // An absent key selects nothing, and so does its negation: NULL is not TRUE.
            let absent = key("k9", Lit::Utf8("x".into()));
            check(&file, absent.clone(), &[uid, attrs]).await;
            check(&file, Predicate::Not(Box::new(absent)), &[uid]).await;
            // A NULL literal is NULL for every row.
            check(&file, key("k0", Lit::Null), &[uid]).await;
            check(
                &file,
                Predicate::Not(Box::new(key("k0", Lit::Null))),
                &[uid],
            )
            .await;
        });
    }

    #[test]
    fn map_and_payload_projections_match_the_oracle() {
        runtime().block_on(async {
            let file = open_wide(20_000, &Config::default()).await;
            let (attrs, uid) = (col("attrs"), col("uid"));
            let ranges = 10..60u32;
            let req = ScanRequest {
                projection: vec![attrs, uid],
                predicate: None,
                row_ranges: Some(vec![ranges.clone()]),
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            let batches = file.scan(&req).await.unwrap();
            let want = oracle(
                &file,
                None,
                Some(std::slice::from_ref(&ranges)),
                &[attrs, uid],
            )
            .await
            .unwrap();
            assert_eq!(want.num_rows(), 50);
            assert_batches_equal(&joined(&want.schema(), &batches), &want);

            let payload = open_wide(20_000, &payload_config()).await;
            let (blob, tokens) = (col("blob"), col("input_tokens"));
            check(
                &payload,
                Predicate::Cmp(tokens, CmpOp::Gt, lit_int(2_500)),
                &[blob, uid],
            )
            .await;
            check(&payload, Predicate::IsNull(tokens), &[blob]).await;
        });
    }

    /// A search that is a conjunct of the predicate runs after the other conjuncts, on the rows
    /// they keep: the rows are the brute force's for every shape (nested, under OR or NOT, two
    /// searches), and a selective conjunct reads fewer frame bytes than the search alone.
    #[test]
    fn a_search_after_narrow_conjuncts_reads_only_their_rows_frames() {
        runtime().block_on(async {
            let file = open_wide(20_000, &payload_config()).await;
            let (blob, uid, tokens) = (col("blob"), col("uid"), col("input_tokens"));
            let needle = Predicate::Contains(blob, vec![7u8; 2]);
            let other = Predicate::Contains(blob, vec![7u8; 1]);
            // About 0.2% of the rows, spread over the file: a frame holds many small values, so
            // a wider filter would still touch most frames.
            let narrow = Predicate::Cmp(tokens, CmpOp::Lt, lit_int(10));
            let and = |terms: Vec<Predicate>| Predicate::And(terms);
            for p in [
                and(vec![narrow.clone(), needle.clone()]),
                and(vec![needle.clone(), and(vec![narrow.clone()])]),
                and(vec![narrow.clone(), needle.clone(), other.clone()]),
                Predicate::Or(vec![narrow.clone(), needle.clone()]),
                and(vec![
                    narrow.clone(),
                    Predicate::Not(Box::new(needle.clone())),
                ]),
                and(vec![Predicate::Const(Some(false)), needle.clone()]),
            ] {
                check(&file, p, &[uid]).await;
            }

            let frame_bytes = |req: ScanRequest| {
                let file = &file;
                async move {
                    file.reset_io_stats();
                    file.scan(&req).await.unwrap();
                    file.io_stats().by_kind[Kind::PayloadFrame as usize]
                }
            };
            let req = |predicate: Predicate| ScanRequest {
                projection: vec![uid],
                predicate: Some(predicate),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            let whole = frame_bytes(req(needle.clone())).await;
            let part = frame_bytes(req(and(vec![narrow.clone(), needle.clone()]))).await;
            assert!(whole > 0);
            assert!(part * 4 < whole, "{part} of {whole} frame bytes");
        });
    }

    #[test]
    fn contains_matches_brute_force_and_null_rows_are_not_matches() {
        runtime().block_on(async {
            let file = open_wide(20_000, &payload_config()).await;
            let (blob, uid) = (col("blob"), col("uid"));
            let needle = Predicate::Contains(blob, vec![7u8; 4]);
            check(&file, needle.clone(), &[uid]).await;
            // `Not(Contains)` selects the rows where `blob` is null.
            check(&file, Predicate::Not(Box::new(needle)), &[uid]).await;
            check(&file, Predicate::Contains(blob, Vec::new()), &[uid]).await;

            // Only payload columns take `Contains`, and needles of at most 64 bytes.
            let narrow = open_wide(2_000, &Config::default()).await;
            let result = narrow
                .scan(&ScanRequest {
                    projection: vec![uid],
                    predicate: Some(Predicate::Contains(uid, b"x".to_vec())),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Scan,
                    views: false,
                    top: None,
                })
                .await;
            assert!(matches!(result, Err(StrataError::Unsupported { .. })));
            let long = file
                .scan(&ScanRequest {
                    projection: vec![uid],
                    predicate: Some(Predicate::Contains(blob, vec![1u8; 65])),
                    row_ranges: None,
                    limit: None,
                    kind: ScanKind::Scan,
                    views: false,
                    top: None,
                })
                .await;
            assert!(matches!(long, Err(StrataError::Unsupported { .. })));
        });
    }

    #[test]
    fn a_candidate_longer_than_the_filter_window_matches_the_oracle() {
        runtime().block_on(async {
            // One candidate range of 150,000 rows: the filter evaluates it in three windows.
            let file = open_batch(150_000).await;
            let (start, input) = (col("start_time"), col("input_tokens"));
            let at = |row: i64| Lit::Int(1_700_000_000_000_000_000 + row * 1_000_003);
            let rows_either_side = [65_535, 65_536, 131_071, 131_072];
            let mut terms: Vec<Predicate> = rows_either_side
                .iter()
                .map(|&row| Predicate::Cmp(start, CmpOp::Eq, at(row)))
                .collect();
            terms.push(Predicate::Cmp(start, CmpOp::Gt, at(140_000)));
            check(&file, Predicate::Or(terms), &[input, start]).await;
        });
    }

    #[test]
    fn a_contains_scan_takes_at_most_seven_waves() {
        runtime().block_on(async {
            let file = open_wide(20_000, &payload_config()).await;
            let (blob, uid) = (col("blob"), col("uid"));
            let needle = Predicate::Contains(blob, vec![7u8; 4]);
            file.reset_io_stats();
            let req = ScanRequest {
                projection: vec![uid],
                predicate: Some(needle.clone()),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            file.scan(&req).await.unwrap();
            // Metadata, up to four `Contains` waves, the filter and the projection.
            let waves = file.io_stats().waves;
            println!("Contains scan on blob: {waves} waves");
            assert!(waves <= 1 + 4 + 2, "{waves} waves");
            check(&file, needle, &[uid]).await;
        });
    }

    #[test]
    fn lookup_scan_waves_and_results() {
        runtime().block_on(async {
            let trace = |row: u64| u128::from(row / 5);

            // Cold, without the metadata section: the metadata and the narrow chunks, two waves.
            let lazy_file = open_batch_with(20_000, &lazy()).await;
            let all: Vec<usize> = (0..lazy_file.schema().fields().len()).collect();
            let got = lazy_file
                .lookup_scan(&trace(1_000).to_be_bytes(), &all, false)
                .await
                .unwrap();
            assert_eq!(lazy_file.io_stats().waves, 2);
            let want = trace_rows(&lazy_file, trace(1_000), &all).await;
            assert_batches_equal(&joined(&want.schema(), &got), &want);

            // Cold, with the metadata section the open kept: one wave.
            let file = open_batch(20_000).await;
            file.reset_io_stats();
            let got = file
                .lookup_scan(&trace(1_000).to_be_bytes(), &all, false)
                .await
                .unwrap();
            assert_eq!(file.io_stats().waves, 1);
            let want = trace_rows(&file, trace(1_000), &all).await;
            assert_batches_equal(&joined(&want.schema(), &got), &want);

            // Warm: one wave.
            file.reset_io_stats();
            let got = file
                .lookup_scan(&trace(15_000).to_be_bytes(), &all, false)
                .await
                .unwrap();
            assert_eq!(file.io_stats().waves, 1);
            let want = trace_rows(&file, trace(15_000), &all).await;
            assert_batches_equal(&joined(&want.schema(), &got), &want);

            // A key above every zone costs nothing; [0; 16] is present, in rows 0 to 4.
            file.reset_io_stats();
            assert!(
                file.lookup_scan(&[0xFF; 16], &all, false)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(file.io_stats().waves, 0);
            let present = file.lookup_scan(&[0u8; 16], &all, false).await.unwrap();
            let rows: usize = present.iter().map(RecordBatch::num_rows).sum();
            assert_eq!(rows, 5);
        });
    }

    #[test]
    fn a_frame_shared_by_two_columns_is_fetched_once() {
        runtime().block_on(async {
            // Columns `a` and `b` hold the same values, so they share every frame of the store.
            let n = 2_000usize;
            let schema = Arc::new(Schema::new(vec![
                Field::new("trace_id", DataType::FixedSizeBinary(16), false),
                Field::new("a", DataType::Utf8, false),
                Field::new("b", DataType::Utf8, false),
            ]));
            let ids: Vec<[u8; 16]> = (0..n).map(|i| ((i / 5) as u128).to_be_bytes()).collect();
            let text: Vec<String> = (0..n)
                .map(|i| format!("row {i} says {} ", i * 7919).repeat(40))
                .collect();
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(
                        arrow::array::FixedSizeBinaryArray::try_from_iter(ids.iter()).unwrap(),
                    ),
                    Arc::new(StringArray::from(text.clone())),
                    Arc::new(StringArray::from(text.clone())),
                ],
            )
            .unwrap();
            let mut w = StrataWriter::new(schema, &["trace_id"], Config::default()).unwrap();
            w.write(&batch).unwrap();
            let (bytes, _) = w.finish().unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let path = Path::from("shared.strata");
            store.put(&path, PutPayload::from(bytes)).await.unwrap();
            let file = StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap();
            let key = 200u128.to_be_bytes();
            file.lookup_scan(&key, &[1, 2], false).await.unwrap();

            let frame_bytes = |projection: Vec<usize>| {
                let file = &file;
                async move {
                    file.reset_io_stats();
                    let got = file.lookup_scan(&key, &projection, false).await.unwrap();
                    let frames = file.io_stats().by_kind[Kind::PayloadFrame as usize];
                    (got, frames)
                }
            };
            let (_, one) = frame_bytes(vec![1]).await;
            let (got, two) = frame_bytes(vec![1, 2]).await;
            assert!(one > 0);
            assert_eq!(one, two);
            let got = arrow::compute::concat_batches(&got[0].schema(), &got).unwrap();
            assert_eq!(got.num_rows(), 5);
            for (k, row) in (1_000..1_005).enumerate() {
                for column in 0..2 {
                    assert_eq!(
                        bytes_at(got.column(column).as_ref(), k),
                        Some(text[row].as_bytes())
                    );
                }
            }
        });
    }

    #[test]
    #[ignore]
    fn scan_datagen_map_pruning() {
        runtime().block_on(async {
            let file = open_datagen(3_000).await;
            let attrs = file.schema().index_of("resource_attributes").unwrap();
            let trace_id = file.schema().index_of("trace_id").unwrap();
            let layout = file
                .hot
                .maps
                .iter()
                .find(|layout| usize::try_from(layout.map).ok() == Some(attrs))
                .unwrap();
            assert!(!layout.keys.is_empty(), "no promoted key");
            let first = &layout.keys[0];
            let child = usize::try_from(first.child).unwrap();
            assert!(
                file.directory()[child].flags & 1 != 0,
                "the first promoted key has no dictionary"
            );
            let key = String::from_utf8(first.key.clone()).unwrap();

            // The JSON-quoted value is absent, so the child's dictionary prunes every chunk: the
            // metadata wave is the only wave.
            file.reset_io_stats();
            let req = ScanRequest {
                projection: vec![trace_id],
                predicate: Some(Predicate::MapKeyEq(
                    attrs,
                    key,
                    Lit::Utf8("\"nope\"".into()),
                )),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            };
            assert!(file.scan(&req).await.unwrap().is_empty());
            assert_eq!(file.io_stats().waves, 1);
        });
    }

    #[test]
    fn top_positions_keep_the_first_k_and_their_ties() {
        use arrow::array::Int64Array;
        let desc = SortOptions {
            descending: true,
            nulls_first: true,
        };
        let asc = SortOptions {
            descending: false,
            nulls_first: false,
        };
        let values = Int64Array::from(vec![Some(5), None, Some(9), Some(5), Some(1), Some(9)]);
        // Descending, nulls first: null, 9, 9, then 5 twice tied at the fourth.
        assert_eq!(top_positions(&values, desc, 1).unwrap(), vec![1]);
        assert_eq!(top_positions(&values, desc, 2).unwrap(), vec![1, 2, 5]);
        assert_eq!(
            top_positions(&values, desc, 4).unwrap(),
            vec![0, 1, 2, 3, 5]
        );
        // Ascending, nulls last: 1, 5, 5, 9, 9, null.
        assert_eq!(top_positions(&values, asc, 2).unwrap(), vec![0, 3, 4]);
        assert_eq!(
            top_positions(&values, asc, 6).unwrap(),
            vec![0, 1, 2, 3, 4, 5]
        );
        // More than there are, or nulls tied with nulls.
        assert_eq!(
            top_positions(&values, asc, 9).unwrap(),
            vec![0, 1, 2, 3, 4, 5]
        );
        let nulls = Int64Array::from(vec![None, Some(3), None]);
        assert_eq!(top_positions(&nulls, desc, 1).unwrap(), vec![0, 2]);
        assert!(
            top_positions(&Int64Array::from(Vec::<i64>::new()), asc, 3)
                .unwrap()
                .is_empty()
        );
    }

    /// A scan with top rows returns the rows of the full scan that rank in the first `k` (and
    /// their ties), and reads only the chunks that hold them.
    #[test]
    fn a_scan_with_top_rows_reads_only_their_chunks() {
        runtime().block_on(async {
            let file = open_wide(40_000, &Config::default()).await;
            let projection = vec![col("start_time"), col("uid"), col("input_tokens")];
            let predicate = Some(Predicate::Cmp(
                col("span_kind"),
                CmpOp::Eq,
                Lit::Utf8("llm".into()),
            ));
            let req = |top| ScanRequest {
                projection: projection.clone(),
                predicate: predicate.clone(),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top,
            };
            let schema = scan_schema(file.schema().project(&projection).unwrap(), false);
            file.reset_io_stats();
            let full = joined(&schema, &file.scan(&req(None)).await.unwrap());
            let full_bytes = file.io_stats().bytes;
            for (column, descending, k) in [
                (col("start_time"), true, 50),
                (col("start_time"), false, 1),
                (col("input_tokens"), true, 30),
                (col("input_tokens"), false, 9_000),
            ] {
                let options = SortOptions {
                    descending,
                    nulls_first: descending,
                };
                file.reset_io_stats();
                let top = TopRows { column, options, k };
                let got = joined(&schema, &file.scan(&req(Some(top))).await.unwrap());
                let bytes = file.io_stats().bytes;
                let position = projection.iter().position(|&c| c == column).unwrap();
                let keep = top_positions(full.column(position).as_ref(), options, k).unwrap();
                let indices = UInt32Array::from_iter_values(keep.iter().map(|&i| i as u32));
                let want = arrow::compute::take_record_batch(&full, &indices).unwrap();
                assert_eq!(got, want, "column {column}, k {k}");
                assert!(got.num_rows() >= k.min(full.num_rows()));
                // Start times rise with the row: the top rows sit in a few chunks.
                if column == col("start_time") {
                    assert!(bytes * 2 < full_bytes, "{bytes} of {full_bytes} bytes");
                }
            }
        });
    }

    #[test]
    fn top_rows_refuse_a_limit_and_a_column_that_is_not_narrow() {
        runtime().block_on(async {
            let config = Config {
                payload_min_avg: 1,
                ..Config::default()
            };
            let file = open_wide(2_000, &config).await;
            let top = |column| TopRows {
                column,
                options: SortOptions::default(),
                k: 5,
            };
            let req = |top, limit| ScanRequest {
                projection: vec![col("start_time")],
                predicate: None,
                row_ranges: None,
                limit,
                kind: ScanKind::Scan,
                views: false,
                top: Some(top),
            };
            let with_limit = file.scan(&req(top(col("start_time")), Some(3))).await;
            assert!(matches!(with_limit, Err(StrataError::Unsupported { .. })));
            assert_eq!(
                file.directory()[col("uid")].layer,
                Layer::Payload,
                "uid should be a payload column with payload_min_avg 1"
            );
            let payload = file.scan(&req(top(col("uid")), None)).await;
            assert!(matches!(payload, Err(StrataError::Unsupported { .. })));
            // k = 0 selects nothing and reads nothing.
            file.reset_io_stats();
            let none = TopRows {
                k: 0,
                ..top(col("start_time"))
            };
            assert!(file.scan(&req(none, None)).await.unwrap().is_empty());
            assert_eq!(file.io_stats().bytes, 0);
        });
    }
}
