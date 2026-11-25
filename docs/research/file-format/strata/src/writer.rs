//! The writer: it checks and keeps batches in memory, then writes the whole file in
//! [`StrataWriter::finish`].

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, RecordBatch, new_empty_array};
use arrow::compute::concat;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::row::{OwnedRow, RowConverter, SortField};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{
    Cursor, MAX_CHUNK_ROWS, MAX_DICT_VALUES, MAX_PAGE_ENTRIES, MAX_ROWS, Out, VERSION, checksum32,
};
use crate::hot::{
    DirectoryRecord, HotTail, Layer, MapKey, MapLayout, PageRef, Postscript, write_file_header,
};
use crate::index::{KeyFilter, key_bytes};
use crate::narrow::Encoding;
use crate::narrow::{
    BlockEntry, Class, ENCODINGS, EncodedChunk, chunk_ranges, encode_chunk_candidates, for_column,
    is_utf8_map, numeric_of,
};
use crate::payload::{self, PayloadStats};
use crate::postings::ChunkPostings;
use crate::segment::{
    Codec, Kind, SegmentCodec, SegmentRef, compress_for, encode_segment, write_body, write_segment,
};
use crate::shred::{Shredded, shred};

/// A shredded map column waiting for its children and residual to be written. `summary` is the
/// index of the map's row in the summaries.
struct PendingMap {
    column_id: u32,
    summary: usize,
    shredded: Shredded,
}

/// Whether a later chunk candidate stored in `stored` bytes replaces the best so far, stored in
/// `best` bytes: it must save at least 30%, the same bar as zstd on a narrow chunk. Plain
/// integer chunks that were only slightly smaller than bit-packed ones made narrow queries up to
/// 19% slower, since they are decompressed on every scan.
fn replaces(stored: usize, best: usize) -> bool {
    stored * 100 <= best * 70
}

/// Directory-record flag bit of a narrow column with a chunk dictionary.
pub(crate) const FLAG_CHUNK_DICT: u8 = 2;
/// A narrow column trains a chunk dictionary when its `Varbin`, `Map` and `Ipc` chunks hold at
/// least this many raw body bytes …
const CHUNK_DICT_MIN_RAW: usize = 1 << 20;
/// … on whole bodies of evenly spread chunks, about this many bytes in all.
const CHUNK_DICT_SAMPLE_BYTES: usize = 1 << 20;
/// The chunk dictionary is kept only when the chunks it compresses, plus the dictionary, take
/// at most this share (in percent) of what they take without it.
const CHUNK_DICT_MAX_SHARE: usize = 95;

/// The compressed form of a chunk body and its codec (zstd), if any.
type Compressed = Option<(Codec, Vec<u8>)>;

/// One encoded chunk of a narrow column: its rows, its chunk and the compressed form of its body
/// that the writer stores when it has no chunk dictionary.
type Encoded = (std::ops::Range<usize>, EncodedChunk, Compressed);

/// A chunk dictionary, and each chunk's body compressed with it (`None` for a chunk stored
/// without it).
type ChunkDict = (Vec<u8>, Vec<Option<Vec<u8>>>);

/// The chunk dictionary of a narrow column whose chunks are `chunks`, and each chunk's body
/// compressed with it at zstd `level` (`None` for a chunk stored as before). Text-like chunks
/// (`Varbin`, `Map`, `Ipc`) are short, so zstd finds little to match within one; a dictionary
/// trained on the column gives it the column's common strings. `None` when those chunks are
/// too small to train on, or when the dictionary does not save `CHUNK_DICT_MAX_SHARE`.
fn chunk_dictionary(chunks: &[Encoded], level: i32) -> Result<Option<ChunkDict>> {
    let bodies: Vec<(usize, &[u8])> = chunks
        .iter()
        .enumerate()
        .filter(|(_, (_, chunk, _))| {
            matches!(
                chunk.encoding,
                Encoding::Varbin | Encoding::Map | Encoding::Ipc
            )
        })
        .filter_map(|(i, (_, chunk, _))| Some((i, chunk.body.as_deref()?)))
        .collect();
    let raw_total: usize = bodies.iter().map(|(_, body)| body.len()).sum();
    if raw_total < CHUNK_DICT_MIN_RAW {
        return Ok(None);
    }
    let step = raw_total.div_ceil(CHUNK_DICT_SAMPLE_BYTES).max(1);
    let samples: Vec<&[u8]> = bodies.iter().step_by(step).map(|(_, body)| *body).collect();
    // A failed training (too few or too uniform samples) means no dictionary.
    let Ok(raw) = zstd::dict::from_samples(&samples, payload::DICT_MAX) else {
        return Ok(None);
    };
    let zstd_error = |source| StrataError::Io {
        action: "zstd init",
        source,
    };
    let encoder = zstd::dict::EncoderDictionary::copy(&raw, level);
    let mut compressor =
        zstd::bulk::Compressor::with_prepared_dictionary(&encoder).map_err(zstd_error)?;
    // Attach the dictionary's tables instead of copying them into every chunk.
    compressor
        .set_parameter(zstd::zstd_safe::CParameter::ForceAttachDict(
            zstd::zstd_safe::DictAttachPref::ForceAttach,
        ))
        .map_err(zstd_error)?;
    let mut packed: Vec<Option<Vec<u8>>> = vec![None; chunks.len()];
    let (mut with, mut without) = (raw.len(), 0usize);
    for (i, body) in bodies {
        let stored = chunks
            .get(i)
            .and_then(|(_, _, compressed)| compressed.as_ref())
            .map_or(body.len(), |(_, c)| c.len());
        without += stored;
        let c = compressor
            .compress(body)
            .map_err(|source| StrataError::Io {
                action: "zstd compress",
                source,
            })?;
        // The same 30% bar as zstd without a dictionary, and smaller than that.
        if c.len() * 100 <= body.len() * 70 && c.len() < stored {
            with += c.len();
            if let Some(slot) = packed.get_mut(i) {
                *slot = Some(c);
            }
        } else {
            with += stored;
        }
    }
    if with * 100 > without * CHUNK_DICT_MAX_SHARE {
        return Ok(None);
    }
    Ok(Some((raw, packed)))
}

/// Writes one Strata file. Batches are checked as they arrive and kept until
/// [`StrataWriter::finish`].
pub struct StrataWriter {
    schema: SchemaRef,
    sort_key: Vec<usize>,
    config: Config,
    /// `None` when the sort key is empty.
    converter: Option<RowConverter>,
    last_key: Option<OwnedRow>,
    batches: Vec<RecordBatch>,
    rows: u64,
}

/// What the writer stored for one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSummary {
    /// The column name.
    pub name: String,
    /// Chunks per encoding, indexed by the encoding's value (0 to 11).
    pub encoding_counts: [u32; ENCODINGS],
    /// Bytes of the column's segments, block-table pages included.
    pub bytes: u64,
    /// Number of chunks.
    pub chunks: u32,
    /// Bytes of the column's key filter (section-4 body or key-filter segment), not included in
    /// `bytes`. 0 when there is none.
    pub filter_bytes: u64,
    /// Payload-layer statistics; `None` for narrow columns.
    pub payload: Option<PayloadStats>,
    /// Number of promoted keys of a shredded map; 0 otherwise.
    pub shredded_keys: u32,
}

/// What the writer stored for the whole file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSummary {
    /// Rows in the file.
    pub rows: u64,
    /// Length of the file in bytes.
    pub bytes: u64,
    /// Length of the hot tail in bytes.
    pub hot_len: u32,
    /// Length of the tail: the metadata section, the hot tail and the postscript. It is the
    /// hint that lets [`StrataFile::open`](crate::StrataFile::open) read the tail at once.
    pub tail_len: u64,
    /// One summary per column, in schema order.
    pub columns: Vec<ColumnSummary>,
}

impl StrataWriter {
    /// A writer for `schema`, sorted by `sort_key` (column names). The first sort-key
    /// column must be FixedSizeBinary, an integer or a timestamp. An empty `sort_key` writes
    /// the rows in the order given, in a file with no sort key: no lookup by key, and no
    /// merge.
    pub fn new(schema: SchemaRef, sort_key: &[&str], config: Config) -> Result<Self> {
        if config.chunk_max_rows > MAX_CHUNK_ROWS {
            return Err(StrataError::Schema {
                reason: format!(
                    "chunk_max_rows {} is above {MAX_CHUNK_ROWS}",
                    config.chunk_max_rows
                ),
            });
        }
        if config.page_max_entries == 0 || config.page_max_entries > MAX_PAGE_ENTRIES {
            return Err(StrataError::Schema {
                reason: format!(
                    "page_max_entries {} is not in 1..={MAX_PAGE_ENTRIES}",
                    config.page_max_entries
                ),
            });
        }
        if config.dict_max_values > MAX_DICT_VALUES {
            return Err(StrataError::Schema {
                reason: format!(
                    "dict_max_values {} is above {MAX_DICT_VALUES}",
                    config.dict_max_values
                ),
            });
        }
        let (indexes, converter) = if sort_key.is_empty() {
            (Vec::new(), None)
        } else {
            let (indexes, converter) = sort_key_indexes(&schema, sort_key)?;
            (indexes, Some(converter))
        };
        Ok(Self {
            schema,
            sort_key: indexes,
            config,
            converter,
            last_key: None,
            batches: Vec::new(),
            rows: 0,
        })
    }

    /// Adds a batch. Its schema must equal the writer's schema, and its rows must continue
    /// the sort order of the rows before it. Equal keys are allowed.
    pub fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        check_schema(&self.schema, &batch.schema())?;
        let n = batch.num_rows();
        if n == 0 {
            return Ok(());
        }
        let total = self.rows + n as u64;
        if total > MAX_ROWS {
            return Err(StrataError::TooManyRows { rows: total });
        }

        let Some(converter) = &self.converter else {
            self.rows = total;
            self.batches.push(batch.clone());
            return Ok(());
        };
        let mut key_columns: Vec<ArrayRef> = Vec::with_capacity(self.sort_key.len());
        for index in &self.sort_key {
            let column = batch.column(*index);
            if column.null_count() > 0 {
                return Err(StrataError::Schema {
                    reason: format!(
                        "sort-key column {} has null values",
                        self.schema.field(*index).name()
                    ),
                });
            }
            key_columns.push(Arc::clone(column));
        }
        let rows = converter.convert_columns(&key_columns)?;
        for i in 0..n {
            let row = rows.row(i);
            let in_order = if i == 0 {
                self.last_key.as_ref().is_none_or(|prev| prev.row() <= row)
            } else {
                rows.row(i - 1) <= row
            };
            if !in_order {
                return Err(StrataError::Unsorted {
                    row: self.rows + i as u64,
                });
            }
        }

        self.last_key = Some(rows.row(n - 1).owned());
        self.rows = total;
        self.batches.push(batch.clone());
        Ok(())
    }

    /// Writes the file. Returns its bytes and a summary.
    pub fn finish(self) -> Result<(Vec<u8>, FileSummary)> {
        futures::executor::block_on(write_file(
            &self.schema,
            &self.sort_key,
            &self.config,
            self.rows,
            None,
            |i| std::future::ready(self.column(i)),
        ))
    }

    /// The rows of one column, all batches concatenated.
    /// Column `index` of the buffered batches, one array per batch.
    fn column(&self, index: usize) -> Result<Vec<ArrayRef>> {
        Ok(self
            .batches
            .iter()
            .map(|batch| Arc::clone(batch.column(index)))
            .collect())
    }
}

/// The encoded key-filter body over `values`.
fn filter_body(values: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    KeyFilter::build(values).encode(&mut body);
    body
}

/// The body of a key-filter segment: `u32` filter length, the filter, then the column's chunk
/// postings when it has a dictionary (nothing otherwise).
fn key_index_body(filter: &[u8], postings: Option<&ChunkPostings>) -> Result<Vec<u8>> {
    let len = u32::try_from(filter.len()).map_err(|_| StrataError::Unsupported {
        what: "a key filter of more than 4 GiB".to_string(),
    })?;
    let mut out = Out(Vec::with_capacity(4 + filter.len()));
    out.u32(len);
    out.bytes(filter);
    if let Some(postings) = postings {
        postings.encode(&mut out);
    }
    Ok(out.0)
}

/// The chunk postings of a Utf8 column written with a dictionary: for each chunk of
/// `entries`, the codes of its rows. `None` when the column has no dictionary.
fn chunk_postings(
    config: &Config,
    array: &dyn Array,
    entries: &[BlockEntry],
) -> Result<Option<ChunkPostings>> {
    let (Some(dict), Some(strings)) = (
        for_column(array, config),
        array.as_any().downcast_ref::<arrow::array::StringArray>(),
    ) else {
        return Ok(None);
    };
    let mut chunks = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut codes: Vec<u32> = Vec::new();
        let start = entry.first_row as usize;
        for row in start..start + entry.n_rows as usize {
            if strings.is_valid(row)
                && let Some(code) = dict.code_of(strings.value(row).as_bytes())
            {
                codes.push(code);
            }
        }
        codes.sort_unstable();
        codes.dedup();
        chunks.push(codes);
    }
    Ok(Some(ChunkPostings::build(dict.len(), &chunks)?))
}

/// The directory record and summary of a payload column: its store and header segments, its
/// statistics and null count. `pages_len` is the length of the page references so far.
fn payload_record(
    column_id: u32,
    field: &Field,
    (store, header, stats, null_count): (SegmentRef, SegmentRef, PayloadStats, u64),
    pages_len: usize,
) -> Result<(DirectoryRecord, ColumnSummary)> {
    let pages_offset = u32::try_from(pages_len).map_err(|_| StrataError::Unsupported {
        what: "more than 4 GiB of block-table page references".to_string(),
    })?;
    let record = DirectoryRecord {
        column_id,
        parent: u32::MAX,
        layer: Layer::Payload,
        flags: 0,
        pages_offset,
        n_pages: 0,
        n_chunks: 0,
        dict_offset: store.offset,
        dict_len: store.len,
        payload_offset: header.offset,
        payload_len: header.len,
        null_count,
    };
    let summary = ColumnSummary {
        name: field.name().clone(),
        encoding_counts: [0; ENCODINGS],
        bytes: stats.bytes,
        chunks: 0,
        filter_bytes: 0,
        payload: Some(stats),
        shredded_keys: 0,
    };
    Ok((record, summary))
}

/// Writes a file of `rows` rows whose column `i` is `column(i).await`, given in parts (arrays to
/// be read one after the other). The rows must already
/// be sorted by `sort_key` (schema indexes); this is not checked here. Columns are asked for
/// in schema order, except that the first payload column makes the writer ask for every later
/// Utf8 or Binary column at once, to write all payload columns with one chunk store.
///
/// With `copy` (a merge), the payload columns are not asked for: their stores are copied from
/// the merge inputs and only their refs are written again.
pub(crate) async fn write_file<F, Fut>(
    schema: &SchemaRef,
    sort_key: &[usize],
    config: &Config,
    rows: u64,
    copy: Option<&payload::MergeCopy<'_>>,
    mut column: F,
) -> Result<(Vec<u8>, FileSummary)>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<Vec<ArrayRef>>>,
{
    let mut out = Out(Vec::new());
    write_file_header(&mut out);
    // The metadata segments, written apart and placed after the data, before the hot tail
    // (`Postscript::meta_offset`). Their references are relative to `meta` until then.
    let mut meta = Out(Vec::new());
    let mut codec = SegmentCodec::new(config.zstd_level)?;

    let mut directory = Vec::with_capacity(schema.fields().len());
    let mut pages = Out(Vec::new());
    let mut sort_entries = Out(Vec::new());
    let mut summaries = Vec::with_capacity(schema.fields().len());
    let mut sort_key_filter = Vec::new();
    let mut key_filters: Vec<(u32, SegmentRef)> = Vec::new();
    let mut pending_maps: Vec<PendingMap> = Vec::new();
    let first_sort_column = sort_key.first().copied();
    // Columns fetched ahead of their turn, and the written payload columns: all the payload
    // columns are written together, with one chunk store, when the first one is reached.
    let mut fetched: HashMap<usize, Vec<ArrayRef>> = HashMap::new();
    let mut payloads: HashMap<usize, (SegmentRef, SegmentRef, PayloadStats, u64)> = HashMap::new();
    let mut payloads_written = false;
    if let Some(copy) = copy {
        payloads_written = true;
        for c in payload::copy_stores(copy, &mut codec, &mut out, &mut meta).await? {
            payloads.insert(c.column, (c.store, c.header, c.stats, c.null_count));
        }
    }
    for (index, field) in schema.fields().iter().enumerate() {
        let column_id = u32::try_from(index).map_err(|_| StrataError::Unsupported {
            what: "more than 4 billion columns".to_string(),
        })?;
        if let Some(entry) = payloads.remove(&index) {
            let (record, summary) = payload_record(column_id, field, entry, pages.len())?;
            directory.push(record);
            summaries.push(summary);
            continue;
        }
        let parts = match fetched.remove(&index) {
            Some(parts) => parts,
            None => column(index).await?,
        };
        if !payloads_written && payload::is_payload(&parts, config) {
            payloads_written = true;
            // This and every later payload column: one shared store, except the columns that
            // get a store of their own. Payload columns are read in their parts, never
            // concatenated.
            let mut group: Vec<(usize, Vec<ArrayRef>)> = vec![(index, parts.clone())];
            for (later, later_field) in schema.fields().iter().enumerate().skip(index + 1) {
                if !matches!(later_field.data_type(), DataType::Utf8 | DataType::Binary) {
                    continue;
                }
                let later_parts = column(later).await?;
                if payload::is_payload(&later_parts, config) {
                    group.push((later, later_parts.clone()));
                }
                fetched.insert(later, later_parts);
            }
            let own_store = |i: usize| {
                schema
                    .fields()
                    .get(i)
                    .is_some_and(|f| config.own_store_columns.iter().any(|n| n == f.name()))
            };
            // The stores, each with its columns and their parts: one shared store, then one
            // store per own-store column.
            type Columns = Vec<(usize, Vec<ArrayRef>)>;
            let mut stores: Vec<Columns> = Vec::new();
            let (own, shared): (Vec<_>, Vec<_>) =
                group.into_iter().partition(|(i, _)| own_store(*i));
            if !shared.is_empty() {
                stores.push(shared);
            }
            stores.extend(own.into_iter().map(|column| vec![column]));
            for store in stores {
                let refs: Vec<&[ArrayRef]> = store.iter().map(|(_, a)| a.as_slice()).collect();
                let (store_seg, written) = payload::write_payload_columns(
                    &refs,
                    config.payload_zstd_level,
                    config.write_threads,
                    &mut codec,
                    &mut out,
                    &mut meta,
                )?;
                for ((i, parts), (header, stats)) in store.iter().zip(written) {
                    let nulls = parts.iter().map(|a| a.null_count() as u64).sum();
                    payloads.insert(*i, (store_seg, header, stats, nulls));
                }
            }
        }
        if let Some(entry) = payloads.remove(&index) {
            let (record, summary) = payload_record(column_id, field, entry, pages.len())?;
            directory.push(record);
            summaries.push(summary);
            continue;
        }
        let array: ArrayRef = match parts.as_slice() {
            [] => new_empty_array(field.data_type()),
            [one] => Arc::clone(one),
            many => {
                let arrays: Vec<&dyn Array> = many.iter().map(|a| a.as_ref()).collect();
                concat(&arrays)?
            }
        };
        // A `Map<Utf8, Utf8>` column is shredded: its parent is written here, and its
        // children and residual after the loop, once every top-level column has its record.
        if is_utf8_map(field.data_type()) {
            let map = array.as_map_opt().ok_or_else(|| StrataError::Schema {
                reason: format!("column {} is not a map array", field.name()),
            })?;
            let shredded = shred(map)?;
            let pages_offset =
                u32::try_from(pages.len()).map_err(|_| StrataError::Unsupported {
                    what: "more than 4 GiB of block-table page references".to_string(),
                })?;
            directory.push(DirectoryRecord {
                column_id,
                parent: u32::MAX,
                layer: Layer::MapParent,
                flags: 0,
                pages_offset,
                n_pages: 0,
                n_chunks: 0,
                dict_offset: 0,
                dict_len: 0,
                payload_offset: 0,
                payload_len: 0,
                null_count: array.null_count() as u64,
            });
            summaries.push(ColumnSummary {
                name: field.name().clone(),
                encoding_counts: [0; ENCODINGS],
                bytes: 0,
                chunks: 0,
                filter_bytes: 0,
                payload: None,
                shredded_keys: u32::try_from(shredded.keys.len()).unwrap_or(u32::MAX),
            });
            pending_maps.push(PendingMap {
                column_id,
                summary: summaries.len() - 1,
                shredded,
            });
            continue;
        }
        // Key filters: the first sort-key column (section 4) and the configured columns
        // (section 6). Payload columns were handled above, and maps have no key bytes.
        let is_sort_key = Some(index) == first_sort_column;
        let filter_wanted = !is_sort_key
            && !matches!(field.data_type(), DataType::Map(_, _))
            && config
                .key_filter_columns
                .iter()
                .any(|name| name == field.name());
        let filter_values: Option<Vec<Vec<u8>>> = if is_sort_key || filter_wanted {
            key_bytes(array.as_ref()).map(|keys| keys.into_iter().flatten().collect())
        } else {
            None
        };
        let filter = filter_values.as_deref().map(filter_body);
        let (record, summary, entries) = write_narrow(
            config, &array, column_id, &mut codec, &mut out, &mut meta, &mut pages,
        )?;
        let mut filter_bytes = 0u64;
        if is_sort_key {
            sort_key_filter = filter.unwrap_or_default();
            filter_bytes = sort_key_filter.len() as u64;
        } else if let Some(filter) = filter {
            let postings = chunk_postings(config, array.as_ref(), &entries)?;
            let body = key_index_body(&filter, postings.as_ref())?;
            let seg = encode_segment(Kind::KeyFilter, &body, Codec::Zstd, &mut codec, &mut meta)?;
            filter_bytes = u64::from(seg.len);
            key_filters.push((column_id, seg));
        }

        if is_sort_key {
            for entry in &entries {
                entry.encode(&mut sort_entries);
            }
        }
        directory.push(record);
        summaries.push(ColumnSummary {
            name: field.name().clone(),
            filter_bytes,
            ..summary
        });
    }

    // The children and the residual of each shredded map, in schema order: the children in
    // key order, then the residual. Each takes the next directory id.
    let mut layouts = Vec::with_capacity(pending_maps.len());
    for pending in pending_maps {
        let PendingMap {
            column_id: map,
            summary,
            shredded,
        } = pending;
        let mut keys = Vec::with_capacity(shredded.keys.len());
        for ((key, child), hits) in shredded
            .keys
            .iter()
            .zip(&shredded.children)
            .zip(&shredded.residual_hits)
        {
            let child_id = next_column_id(&directory)?;
            let (mut record, part, _) = write_narrow(
                config, child, child_id, &mut codec, &mut out, &mut meta, &mut pages,
            )?;
            record.layer = Layer::MapChild;
            record.parent = map;
            add_part(&mut summaries, summary, &part);
            directory.push(record);
            keys.push(MapKey {
                child: child_id,
                residual_hits: *hits,
                key: key.clone(),
            });
        }
        let residual_id = next_column_id(&directory)?;
        let (mut record, part, _) = write_narrow(
            config,
            &shredded.residual,
            residual_id,
            &mut codec,
            &mut out,
            &mut meta,
            &mut pages,
        )?;
        record.layer = Layer::Residual;
        record.parent = map;
        add_part(&mut summaries, summary, &part);
        directory.push(record);
        layouts.push(MapLayout {
            map,
            residual: residual_id,
            keys,
        });
    }

    let n_columns = next_column_id(&directory)?;
    // The metadata section, then the references into it made absolute.
    let meta_offset = out.len() as u64;
    out.bytes(&meta.0);
    for record in &mut directory {
        place_metadata(record, meta_offset)?;
    }
    for (_, segment) in &mut key_filters {
        segment.offset = at_meta(segment.offset, meta_offset)?;
    }
    let mut placed_pages = Out(Vec::with_capacity(pages.len()));
    let mut cursor = Cursor::new(&pages.0, 0);
    while cursor.remaining() > 0 {
        let mut page = PageRef::decode(&mut cursor)?;
        page.offset = at_meta(page.offset, meta_offset)?;
        page.encode(&mut placed_pages);
    }
    let hot_start = out.len();
    let hot = HotTail {
        schema: Arc::clone(schema),
        directory,
        sort_key_entries: sort_entries.0,
        sort_key_filter,
        maps: layouts,
        key_filters,
        pages: placed_pages.0,
    };
    hot.encode(&mut out)?;
    let hot_len = u32::try_from(out.len() - hot_start).map_err(|_| StrataError::Unsupported {
        what: "a hot tail of more than 4 GiB".to_string(),
    })?;
    let hot_checksum = checksum32(&[out.0.get(hot_start..).unwrap_or_default()]);
    let sort_key_column = match first_sort_column {
        Some(index) => u32::try_from(index).map_err(|_| StrataError::Unsupported {
            what: "more than 4 billion columns".to_string(),
        })?,
        None => u32::MAX,
    };
    let postscript = Postscript {
        hot_offset: hot_start as u64,
        hot_len,
        hot_checksum,
        row_count: rows,
        n_columns,
        version: VERSION,
        flags: 0,
        sort_key_column,
        meta_offset,
    };
    postscript.encode(&mut out);

    let summary = FileSummary {
        rows,
        bytes: out.len() as u64,
        hot_len,
        tail_len: out.len() as u64 - meta_offset,
        columns: summaries,
    };
    Ok((out.0, summary))
}

/// The absolute offset of `relative`, an offset in the metadata section that starts at
/// `meta_offset`.
fn at_meta(relative: u64, meta_offset: u64) -> Result<u64> {
    relative
        .checked_add(meta_offset)
        .ok_or_else(|| StrataError::Unsupported {
            what: "a file of more than 2^64 bytes".to_string(),
        })
}

/// Makes the metadata references of `record` absolute: a payload column's store and header, a
/// narrow column's dictionary and chunk dictionary when its flags say it has them.
fn place_metadata(record: &mut DirectoryRecord, meta_offset: u64) -> Result<()> {
    match record.layer {
        Layer::Payload => {
            record.dict_offset = at_meta(record.dict_offset, meta_offset)?;
            record.payload_offset = at_meta(record.payload_offset, meta_offset)?;
        }
        Layer::MapParent => {}
        Layer::Narrow | Layer::MapChild | Layer::Residual => {
            if record.flags & 1 != 0 {
                record.dict_offset = at_meta(record.dict_offset, meta_offset)?;
            }
            if record.flags & FLAG_CHUNK_DICT != 0 {
                record.payload_offset = at_meta(record.payload_offset, meta_offset)?;
            }
        }
    }
    Ok(())
}

/// Writes a narrow column: its chunks to `out`, and its dictionary, chunk dictionary and
/// block-table pages to `meta`, with offsets relative to `meta`. The record has `layer = Narrow` and `parent = u32::MAX`; a caller that stores the
/// column as a map child or residual sets both. The summary's name is empty, for the caller
/// to set.
fn write_narrow(
    config: &Config,
    array: &ArrayRef,
    column_id: u32,
    codec: &mut SegmentCodec,
    out: &mut Out,
    meta: &mut Out,
    pages: &mut Out,
) -> Result<(DirectoryRecord, ColumnSummary, Vec<BlockEntry>)> {
    let dict = for_column(array.as_ref(), config);
    let (dict_offset, dict_len) = match &dict {
        Some(dict) => {
            let mut body = Out(Vec::new());
            dict.encode(&mut body)?;
            let seg = encode_segment(Kind::Dictionary, &body.0, Codec::Zstd, codec, meta)?;
            (seg.offset, seg.len)
        }
        None => (0, 0),
    };
    let ranges = chunk_ranges(
        array.as_ref(),
        config.chunk_max_rows,
        config.chunk_max_bytes,
        dict.is_some(),
    )?;
    let mut entries = Vec::with_capacity(ranges.len());
    let mut counts = [0u32; ENCODINGS];
    let mut bytes = u64::from(dict_len);
    // Every chunk is encoded first: the chunk dictionary is trained on their bodies.
    let mut encoded: Vec<Encoded> = Vec::with_capacity(ranges.len());
    for range in ranges {
        let slice = array.slice(range.start, range.len());
        // The first candidate decodes fastest; a later one must save at least 30% to replace it.
        let mut best: Option<(EncodedChunk, Compressed, usize)> = None;
        for candidate in encode_chunk_candidates(&slice, dict.as_ref(), config)? {
            let (compressed, stored) = match &candidate.body {
                // A delta chunk is stored as is: on vortex-queries, zstd on them saved a tenth of
                // the files' size (6.1 GB to 5.5 GB) and its decompression took a quarter of a
                // full scan (`real-data.md`).
                Some(body) if candidate.encoding == Encoding::ForDelta => (None, body.len()),
                Some(body) => {
                    let c = compress_for(Kind::NarrowChunk, body, Codec::Zstd, codec)?;
                    let stored = c.as_ref().map_or(body.len(), |(_, c)| c.len());
                    (c, stored)
                }
                None => (None, 0),
            };
            if best
                .as_ref()
                .is_none_or(|(_, _, len)| replaces(stored, *len))
            {
                best = Some((candidate, compressed, stored));
            }
        }
        let Some((chunk, compressed, _)) = best else {
            return Err(StrataError::Unsupported {
                what: "a chunk without an encoding".to_string(),
            });
        };
        encoded.push((range, chunk, compressed));
    }
    let chunk_dict = chunk_dictionary(&encoded, config.zstd_level)?;
    let (chunk_dict_offset, chunk_dict_len, mut packed) = match chunk_dict {
        Some((raw, packed)) => {
            let seg = write_segment(Kind::ChunkDictionary, Codec::Raw, raw.len(), &raw, meta)?;
            bytes += u64::from(seg.len);
            (seg.offset, seg.len, packed)
        }
        None => (0, 0, Vec::new()),
    };
    for (i, (range, chunk, compressed)) in encoded.into_iter().enumerate() {
        let with_dict = packed.get_mut(i).and_then(Option::take);
        let EncodedChunk {
            encoding,
            flags,
            const_len,
            min,
            max,
            body,
        } = chunk;
        let (segment_offset, segment_len) = match (body, with_dict) {
            (Some(body), Some(packed)) => {
                let seg =
                    write_segment(Kind::NarrowChunk, Codec::ZstdDict, body.len(), &packed, out)?;
                (seg.offset, seg.len)
            }
            (Some(body), None) => {
                let seg = write_body(
                    Kind::NarrowChunk,
                    &body,
                    compressed.as_ref().map(|(c, b)| (*c, b.as_slice())),
                    out,
                )?;
                (seg.offset, seg.len)
            }
            (None, _) => (0, 0),
        };
        bytes += u64::from(segment_len);
        if let Some(count) = counts.get_mut(encoding as usize) {
            *count += 1;
        }
        entries.push(BlockEntry {
            first_row: u32::try_from(range.start).map_err(|_| StrataError::TooManyRows {
                rows: range.start as u64,
            })?,
            n_rows: u32::try_from(range.len()).map_err(|_| StrataError::TooManyRows {
                rows: range.len() as u64,
            })?,
            segment_offset,
            segment_len,
            encoding,
            flags,
            const_len,
            min,
            max,
        });
    }

    let pages_offset = u32::try_from(pages.len()).map_err(|_| StrataError::Unsupported {
        what: "more than 4 GiB of block-table page references".to_string(),
    })?;
    let mut n_pages = 0u32;
    for page in entries.chunks(config.page_max_entries) {
        let mut body = Out(Vec::with_capacity(page.len() * 56));
        for entry in page {
            entry.encode(&mut body);
        }
        let seg = encode_segment(Kind::BlockTablePage, &body.0, Codec::Raw, codec, meta)?;
        let first_row = page.first().map_or(0, |entry| entry.first_row);
        PageRef {
            first_row,
            offset: seg.offset,
            len: seg.len,
        }
        .encode(pages);
        bytes += u64::from(seg.len);
        n_pages += 1;
    }

    let n_chunks = u32::try_from(entries.len()).map_err(|_| StrataError::Unsupported {
        what: "more than 4 billion chunks".to_string(),
    })?;
    let record = DirectoryRecord {
        column_id,
        parent: u32::MAX,
        layer: Layer::Narrow,
        // Bit 0 says the column has a dictionary.
        flags: u8::from(dict.is_some())
            | if chunk_dict_len > 0 {
                FLAG_CHUNK_DICT
            } else {
                0
            },
        pages_offset,
        n_pages,
        n_chunks,
        dict_offset,
        dict_len,
        payload_offset: chunk_dict_offset,
        payload_len: chunk_dict_len,
        null_count: array.null_count() as u64,
    };
    let summary = ColumnSummary {
        name: String::new(),
        encoding_counts: counts,
        bytes,
        chunks: n_chunks,
        filter_bytes: 0,
        payload: None,
        shredded_keys: 0,
    };
    Ok((record, summary, entries))
}

/// Checks the sort key of `schema` and returns the indexes of its columns and the converter
/// that orders their rows.
pub(crate) fn sort_key_indexes(
    schema: &Schema,
    sort_key: &[&str],
) -> Result<(Vec<usize>, RowConverter)> {
    if sort_key.is_empty() {
        return Err(StrataError::Schema {
            reason: "the sort key is empty".to_string(),
        });
    }
    let mut indexes = Vec::with_capacity(sort_key.len());
    let mut sort_fields = Vec::with_capacity(sort_key.len());
    for name in sort_key {
        let index = schema.index_of(name).map_err(|_| StrataError::Schema {
            reason: format!("unknown sort-key column {name}"),
        })?;
        let dt = schema.field(index).data_type().clone();
        if indexes.is_empty() && !sortable_first(&dt) {
            return Err(StrataError::Schema {
                reason: format!(
                    "sort-key column {name} must be FixedSizeBinary, an integer or a timestamp"
                ),
            });
        }
        sort_fields.push(SortField::new(dt));
        indexes.push(index);
    }
    let converter = RowConverter::new(sort_fields)?;
    Ok((indexes, converter))
}

/// The id of the next directory record, which is also its column id.
fn next_column_id(directory: &[DirectoryRecord]) -> Result<u32> {
    u32::try_from(directory.len()).map_err(|_| StrataError::Unsupported {
        what: "more than 4 billion columns".to_string(),
    })
}

/// Adds the rows and chunks of a map's child or residual to the map's summary, at `index`.
fn add_part(summaries: &mut [ColumnSummary], index: usize, part: &ColumnSummary) {
    if let Some(parent) = summaries.get_mut(index) {
        for (total, count) in parent.encoding_counts.iter_mut().zip(part.encoding_counts) {
            *total = total.saturating_add(count);
        }
        parent.bytes = parent.bytes.saturating_add(part.bytes);
        parent.chunks = parent.chunks.saturating_add(part.chunks);
    }
}

/// Whether a first sort-key column of type `dt` is allowed in M1.
fn sortable_first(dt: &DataType) -> bool {
    matches!(dt, DataType::FixedSizeBinary(_) | DataType::Timestamp(_, _))
        || numeric_of(dt).is_some_and(|num| num.class != Class::Float)
}

/// Checks that `actual` has the names, types and nullability of `expected`.
fn check_schema(expected: &Schema, actual: &Schema) -> Result<()> {
    if expected.fields().len() != actual.fields().len() {
        return Err(StrataError::Schema {
            reason: format!(
                "expected {} columns, got {}",
                expected.fields().len(),
                actual.fields().len()
            ),
        });
    }
    for (want, got) in expected.fields().iter().zip(actual.fields()) {
        if want.name() != got.name()
            || want.data_type() != got.data_type()
            || want.is_nullable() != got.is_nullable()
        {
            return Err(StrataError::Schema {
                reason: format!("column {} does not match the schema", want.name()),
            });
        }
    }
    Ok(())
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
    use crate::file::StrataFile;
    use crate::narrow::Encoding;
    use crate::testgen::{assert_same, batch, schema, wide_batch, wide_schema};
    use arrow::array::{FixedSizeBinaryArray, new_null_array};
    use arrow::datatypes::Field;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

    const SORT: [&str; 2] = ["trace_id", "start_time"];

    #[test]
    fn page_and_dictionary_limits_are_schema_errors() {
        let too_few_entries = Config {
            page_max_entries: 0,
            ..Config::default()
        };
        let too_many_entries = Config {
            page_max_entries: MAX_PAGE_ENTRIES + 1,
            ..Config::default()
        };
        let too_many_values = Config {
            dict_max_values: MAX_DICT_VALUES + 1,
            ..Config::default()
        };
        for config in [too_few_entries, too_many_entries, too_many_values] {
            assert!(matches!(
                StrataWriter::new(schema(), &SORT, config),
                Err(StrataError::Schema { .. })
            ));
        }
        let at_limits = Config {
            page_max_entries: MAX_PAGE_ENTRIES,
            dict_max_values: MAX_DICT_VALUES,
            ..Config::default()
        };
        assert!(StrataWriter::new(schema(), &SORT, at_limits).is_ok());
    }

    #[test]
    fn equal_sort_keys_across_a_batch_boundary_are_accepted() {
        let rows = batch(10);
        let mut w = StrataWriter::new(schema(), &SORT, Config::default()).unwrap();
        w.write(&rows).unwrap();
        // The first row of the next batch has the sort key of the last row of the previous one.
        w.write(&rows.slice(9, 1)).unwrap();
        let (_, summary) = w.finish().unwrap();
        assert_eq!(summary.rows, 11);
    }

    #[test]
    fn null_in_a_sort_key_column_is_a_schema_error() {
        let nullable = Arc::new(Schema::new(vec![Field::new(
            "trace_id",
            DataType::FixedSizeBinary(16),
            true,
        )]));
        let ids = FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            vec![Some([1u8; 16]), None].into_iter(),
            16,
        )
        .unwrap();
        let rows = RecordBatch::try_new(Arc::clone(&nullable), vec![Arc::new(ids)]).unwrap();
        let mut w = StrataWriter::new(nullable, &["trace_id"], Config::default()).unwrap();
        assert!(matches!(w.write(&rows), Err(StrataError::Schema { .. })));
    }

    #[test]
    fn map_column_is_shredded() {
        let rows = wide_batch(10_000);
        let mut w = StrataWriter::new(wide_schema(), &SORT, Config::default()).unwrap();
        w.write(&rows).unwrap();
        let (_, summary) = w.finish().unwrap();
        let attrs = &summary.columns[6];
        assert_eq!(attrs.name, "attrs");
        assert_eq!(attrs.shredded_keys, 3);
        assert!(attrs.chunks > 0);
        assert!(attrs.encoding_counts[Encoding::Map as usize] > 0);
    }

    #[tokio::test]
    async fn all_null_utf8_column_writes_no_dictionary() {
        let table = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let ids = FixedSizeBinaryArray::try_from_sparse_iter_with_size(
            vec![Some([0u8; 16]); 4].into_iter(),
            16,
        )
        .unwrap();
        let names = new_null_array(&DataType::Utf8, 4);
        let rows = RecordBatch::try_new(Arc::clone(&table), vec![Arc::new(ids), names]).unwrap();
        let mut w = StrataWriter::new(table, &["trace_id"], Config::default()).unwrap();
        w.write(&rows).unwrap();
        let (bytes, _) = w.finish().unwrap();

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("table.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        let record = &file.directory()[1];
        assert_eq!(record.dict_offset, 0);
        assert_eq!(record.flags & 1, 0);
    }

    #[tokio::test]
    async fn golden_columns_round_trip() {
        let rows = wide_batch(20_000);
        let mut w = StrataWriter::new(wide_schema(), &SORT, Config::default()).unwrap();
        w.write(&rows).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("golden.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        for column in 0..rows.num_columns() {
            let decoded = crate::reader::read_column(&file, column).await.unwrap();
            assert_same(decoded.as_ref(), rows.column(column).as_ref());
        }
    }

    /// A stack-trace column, as the Parquet reader gives it: lists of frames whose fields are
    /// dictionary-encoded over the whole file. Each `IPC` chunk keeps only the dictionary values
    /// its rows use (Polar Signals took 32 KB a row with every chunk holding every value), and
    /// the column reads back the same.
    #[tokio::test]
    async fn ipc_chunks_keep_only_the_dictionary_values_they_use() {
        use arrow::array::{
            DictionaryArray, ListArray, StringArray, StructArray, UInt32Array, UInt64Array,
        };
        use arrow::buffer::OffsetBuffer;
        use arrow::datatypes::{Fields, UInt32Type};

        let n = 4_096usize;
        let distinct = 50_000usize;
        // Two frames a row, every tenth row null; frame k refers to value (k * 7_919) % distinct.
        let frames = 2 * n;
        let key = |k: usize| u32::try_from(k * 7_919 % distinct).unwrap();
        let address = DictionaryArray::<UInt32Type>::try_new(
            UInt32Array::from_iter_values((0..frames).map(key)),
            Arc::new(UInt64Array::from_iter_values(
                (0..distinct as u64).map(|v| v * 0x9e37_79b9),
            )),
        )
        .unwrap();
        let function = DictionaryArray::<UInt32Type>::try_new(
            UInt32Array::from_iter((0..frames).map(|k| (k % 13 != 0).then(|| key(k)))),
            Arc::new(StringArray::from_iter_values(
                (0..distinct).map(|v| format!("crate::module::function_{v}")),
            )),
        )
        .unwrap();
        let fields = Fields::from(vec![
            Field::new("address", address.data_type().clone(), false),
            Field::new("function", function.data_type().clone(), true),
        ]);
        let frame = StructArray::try_new(
            fields.clone(),
            vec![Arc::new(address), Arc::new(function)],
            None,
        )
        .unwrap();
        let item = Arc::new(Field::new("item", DataType::Struct(fields), true));
        let nulls = arrow::buffer::NullBuffer::from_iter((0..n).map(|i| i % 10 != 0));
        let locations = ListArray::try_new(
            Arc::clone(&item),
            OffsetBuffer::from_lengths(std::iter::repeat_n(2, n)),
            Arc::new(frame),
            Some(nulls),
        )
        .unwrap();
        let schema = Arc::new(arrow::datatypes::Schema::new(vec![Field::new(
            "locations",
            DataType::List(item),
            true,
        )]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(locations)]).unwrap();

        let (file, _, summary) = written(&batch, Config::default()).await;
        let column = &summary.columns[0];
        assert!(column.encoding_counts[Encoding::Ipc as usize] > 1);
        // Each dictionary alone takes over 1 MB; the chunks hold only their 8,192 frames.
        assert!(column.bytes < 1 << 20, "{} bytes", column.bytes);
        let decoded = crate::reader::read_column(&file, 0).await.unwrap();
        assert_eq!(decoded.as_ref(), batch.column(0).as_ref());
    }

    /// The 30% bar holds at its boundary, and an empty best is replaced only by an empty one.
    #[test]
    fn a_later_candidate_must_save_30_percent() {
        assert!(replaces(70, 100));
        assert!(!replaces(71, 100));
        assert!(!replaces(100, 100));
        assert!(replaces(0, 0));
        assert!(!replaces(1, 0));
    }

    /// Integers in long runs (an ingest time shared by a batch of rows) compress better plain
    /// than bit-packed, and are stored plain; small spread-out integers stay bit-packed. Both
    /// read back unchanged.
    #[tokio::test]
    async fn an_integer_chunk_is_stored_in_its_smallest_encoding() {
        use arrow::array::{Int64Array, RecordBatch};
        use arrow::datatypes::{DataType, Schema};
        let n = 8_192i64;
        let schema = Arc::new(Schema::new(vec![
            Field::new("runs", DataType::Int64, false),
            Field::new("small", DataType::Int64, false),
        ]));
        // Runs of 6 equal values, far apart; small pseudo-random values.
        let runs = Int64Array::from_iter_values((0..n).map(|i| (i / 6) * 1_000_000_007));
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let small = Int64Array::from_iter_values((0..n).map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % 97) as i64
        }));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(runs), Arc::new(small)]).unwrap();
        let mut w = StrataWriter::new(schema, &[], Config::default()).unwrap();
        w.write(&batch).unwrap();
        let (bytes, summary) = w.finish().unwrap();
        let counts = |name: &str| {
            summary
                .columns
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .encoding_counts
        };
        let (runs, small) = (counts("runs"), counts("small"));
        // Runs of equal values far apart: zstd on plain values, or steps between them, beat
        // bit-packing their range.
        assert!(
            runs[Encoding::Plain as usize] + runs[Encoding::ForDelta as usize] > 0,
            "{runs:?}"
        );
        assert_eq!(runs[Encoding::ForBitpack as usize], 0, "{runs:?}");
        assert!(small[Encoding::ForBitpack as usize] > 0, "{small:?}");
        assert_eq!(small[Encoding::Plain as usize], 0, "{small:?}");

        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("ints.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        for column in 0..2 {
            let decoded = crate::reader::read_column(&file, column).await.unwrap();
            assert_same(decoded.as_ref(), batch.column(column).as_ref());
        }
    }

    /// Writes `batch` with `config` and no sort key, and opens it. Returns the file, its bytes
    /// and its summary.
    async fn written(batch: &RecordBatch, config: Config) -> (StrataFile, Vec<u8>, FileSummary) {
        let mut w = StrataWriter::new(batch.schema(), &[], config).unwrap();
        w.write(batch).unwrap();
        let (bytes, summary) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("written.strata");
        store
            .put(&path, PutPayload::from(bytes.clone()))
            .await
            .unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        (file, bytes, summary)
    }

    /// Rows of a number and two sentences out of 400, which a chunk of 2 KiB holds only a few
    /// of: a chunk dictionary trained on the column holds most of them. The rows are distinct
    /// and too many for a value dictionary, so the chunks are `Varbin`.
    fn sentences(n: usize) -> arrow::array::StringArray {
        let sentence = |k: usize| {
            format!(
                "Sentence {k} of the shared set talks about topic {} with the words {} and {}. ",
                k * 7 % 13,
                ["alpha", "bravo", "charlie", "delta", "echo"][k % 5],
                ["red", "green", "blue", "orange"][k % 4],
            )
        };
        arrow::array::StringArray::from_iter_values(
            (0..n)
                .map(|i| format!("Row {i}. ") + &sentence(i * 17 % 400) + &sentence(i * 29 % 400)),
        )
    }

    /// `n` pseudo-random Binary values of 150 bytes, which no dictionary compresses.
    fn noise(n: usize) -> arrow::array::BinaryArray {
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let values: Vec<Vec<u8>> = (0..n)
            .map(|_| {
                (0..150)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        x as u8
                    })
                    .collect()
            })
            .collect();
        arrow::array::BinaryArray::from_iter_values(values.iter())
    }

    /// The thread count does not change the file: a payload column of about 6 MiB (a trained
    /// dictionary, about a hundred frames) and a column of noise, on 1 and 7 threads.
    #[tokio::test]
    async fn the_writer_threads_do_not_change_the_bytes() {
        let n = 30_000;
        let text: Vec<String> = sentences(n)
            .iter()
            .map(|v| v.unwrap_or_default().repeat(2))
            .collect();
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new("blob", DataType::Binary, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(arrow::array::StringArray::from(text)),
                Arc::new(noise(n)),
            ],
        )
        .unwrap();
        let config = |threads| Config {
            payload_min_avg: 100,
            write_threads: threads,
            ..Config::default()
        };
        let (_, one, summary) = written(&batch, config(1)).await;
        let (file, seven, _) = written(&batch, config(7)).await;
        assert!(summary.columns.iter().all(|c| c.payload.is_some()));
        assert!(summary.columns[0].payload.as_ref().unwrap().frames > 50);
        assert!(one == seven, "the bytes differ");
        let back = crate::reader::read_column(&file, 0).await.unwrap();
        assert_eq!(back.len(), n);
    }

    /// A text column of small chunks gets a chunk dictionary, its chunks are stored with it and
    /// read back; a column of random bytes gets none. At zstd level 19 the text takes less.
    #[tokio::test]
    async fn a_text_column_gets_a_chunk_dictionary_and_noise_does_not() {
        let n = 16_000;
        let schema = Arc::new(Schema::new(vec![
            Field::new("text", DataType::Utf8, false),
            Field::new("noise", DataType::Binary, false),
        ]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(sentences(n)), Arc::new(noise(n))]).unwrap();
        let small_chunks = Config {
            chunk_max_bytes: 2 * 1024,
            ..Config::default()
        };
        let (file, bytes, summary) = written(&batch, small_chunks.clone()).await;
        let text = &file.directory()[0];
        assert_ne!(text.flags & FLAG_CHUNK_DICT, 0);
        assert!(text.payload_offset > 0 && text.payload_len > 0);
        assert_eq!(file.directory()[1].flags & FLAG_CHUNK_DICT, 0);
        // Byte 3 of a segment header is its codec.
        let entries = file.block_entries(0).await.unwrap();
        assert!(entries.iter().all(|e| {
            bytes[e.segment_offset as usize + 3] == crate::segment::Codec::ZstdDict as u8
        }));
        for column in 0..2 {
            let decoded = crate::reader::read_column(&file, column).await.unwrap();
            assert_same(decoded.as_ref(), batch.column(column).as_ref());
        }

        let cold = Config {
            chunk_max_bytes: 2 * 1024,
            ..Config::cold()
        };
        let (cold_file, _, cold_summary) = written(&batch, cold).await;
        assert!(cold_summary.columns[0].bytes < summary.columns[0].bytes);
        let decoded = crate::reader::read_column(&cold_file, 0).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(0).as_ref());
    }

    /// A text column under `CHUNK_DICT_MIN_RAW` gets no chunk dictionary.
    #[tokio::test]
    async fn a_small_text_column_gets_no_chunk_dictionary() {
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(sentences(1_000))]).unwrap();
        let (file, _, _) = written(&batch, Config::default()).await;
        assert_eq!(file.directory()[0].flags & FLAG_CHUNK_DICT, 0);
        assert_eq!(file.directory()[0].payload_offset, 0);
    }

    /// The `cost` column of `batch` repeats 500 values, so it is stored as dictionary codes;
    /// floats that never repeat are stored plain. Both read back bit for bit.
    #[tokio::test]
    async fn a_float_column_whose_values_repeat_is_stored_as_codes() {
        let rows = batch(20_000);
        let unique = arrow::array::Float64Array::from_iter_values(
            (0..20_000).map(|i| f64::from(i) * 1.000_001),
        );
        let schema = Arc::new(Schema::new(vec![
            Field::new("cost", DataType::Float64, true),
            Field::new("unique", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::clone(rows.column_by_name("cost").unwrap()),
                Arc::new(unique),
            ],
        )
        .unwrap();
        let (file, _, summary) = written(&batch, Config::default()).await;
        let cost = summary.columns[0].encoding_counts;
        assert!(cost[Encoding::FloatDict as usize] > 0, "{cost:?}");
        assert_ne!(file.directory()[0].flags & 1, 0);
        let unique = summary.columns[1].encoding_counts;
        assert_eq!(unique[Encoding::FloatDict as usize], 0, "{unique:?}");
        assert_eq!(file.directory()[1].flags & 1, 0);
        for column in 0..2 {
            let decoded = crate::reader::read_column(&file, column).await.unwrap();
            assert_same(decoded.as_ref(), batch.column(column).as_ref());
        }
    }

    #[tokio::test]
    async fn an_empty_sort_key_keeps_unsorted_rows_in_the_order_given() {
        let sorted = wide_batch(5_000);
        let reversed: Vec<u32> = (0..5_000u32).rev().collect();
        let rows =
            arrow::compute::take_record_batch(&sorted, &arrow::array::UInt32Array::from(reversed))
                .unwrap();
        // The sort key refuses these rows; an empty one takes them as they are.
        let mut keyed = StrataWriter::new(wide_schema(), &SORT, Config::default()).unwrap();
        assert!(matches!(
            keyed.write(&rows),
            Err(StrataError::Unsorted { .. })
        ));
        let mut w = StrataWriter::new(wide_schema(), &[], Config::default()).unwrap();
        w.write(&rows.slice(0, 2_000)).unwrap();
        w.write(&rows.slice(2_000, 3_000)).unwrap();
        let (bytes, summary) = w.finish().unwrap();
        assert_eq!(summary.rows, 5_000);
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("unsorted.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        assert_eq!(file.sort_key_column(), None);
        for column in 0..rows.num_columns() {
            let decoded = crate::reader::read_column(&file, column).await.unwrap();
            assert_same(decoded.as_ref(), rows.column(column).as_ref());
        }
    }

    /// xxh3_64 of the file written from `wide_batch(20_000)` with `Config::default()`.
    /// Recorded after narrow chunks dropped LZ4. A change to
    /// this value means the output changed.
    const GOLDEN: u64 = 0x6c01_c2fa_8705_5f0e;

    #[test]
    fn golden_output_hash() {
        let rows = wide_batch(20_000);
        let mut w = StrataWriter::new(wide_schema(), &SORT, Config::default()).unwrap();
        w.write(&rows).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let hash = xxhash_rust::xxh3::xxh3_64(&bytes);
        println!("golden hash: {hash:#018x}");
        assert_eq!(hash, GOLDEN);
    }

    /// Writes 200,000 wide rows and prints the throughput. MB is 10^6 bytes of
    /// `get_array_memory_size`; the time covers `write` and `finish`. Nothing is asserted.
    #[test]
    #[ignore]
    fn writer_throughput() {
        let rows = wide_batch(200_000);
        let mb = rows.get_array_memory_size() as f64 / 1e6;
        let start = std::time::Instant::now();
        let mut w = StrataWriter::new(wide_schema(), &SORT, Config::default()).unwrap();
        w.write(&rows).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let secs = start.elapsed().as_secs_f64();
        println!(
            "writer_throughput: {:.1} MB/s ({:.1} MB in {:.3} s, {} bytes out)",
            mb / secs,
            mb,
            secs,
            bytes.len()
        );
    }
}
