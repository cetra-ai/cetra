//! The writer: it checks and keeps batches in memory, then writes the whole file in
//! [`StrataWriter::finish`].

use std::future::Future;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, RecordBatch, new_empty_array};
use arrow::compute::concat;
use arrow::datatypes::{DataType, Schema, SchemaRef};
use arrow::row::{OwnedRow, RowConverter, SortField};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{
    MAX_CHUNK_ROWS, MAX_DICT_VALUES, MAX_PAGE_ENTRIES, MAX_ROWS, Out, VERSION, checksum32,
};
use crate::hot::{
    DirectoryRecord, HotTail, Layer, MapKey, MapLayout, PageRef, Postscript, write_file_header,
};
use crate::index::{KeyFilter, key_bytes};
use crate::narrow::{
    BlockEntry, Class, EncodedChunk, chunk_ranges, encode_chunk, for_column, is_utf8_map,
    numeric_of,
};
use crate::payload::{self, FRAME_ZSTD_LEVEL, PayloadStats};
use crate::segment::{Codec, Kind, SegmentCodec, SegmentRef, encode_segment};
use crate::shred::{Shredded, shred};

/// A shredded map column waiting for its children and residual to be written. `summary` is the
/// index of the map's row in the summaries.
struct PendingMap {
    column_id: u32,
    summary: usize,
    shredded: Shredded,
}

/// Writes one Strata file. Batches are checked as they arrive and kept until
/// [`StrataWriter::finish`].
pub struct StrataWriter {
    schema: SchemaRef,
    sort_key: Vec<usize>,
    config: Config,
    converter: RowConverter,
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
    pub encoding_counts: [u32; 12],
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
    /// One summary per column, in schema order.
    pub columns: Vec<ColumnSummary>,
}

impl StrataWriter {
    /// A writer for `schema`, sorted by `sort_key` (column names). The first sort-key
    /// column must be FixedSizeBinary, an integer or a timestamp.
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
        let (indexes, converter) = sort_key_indexes(&schema, sort_key)?;
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
        let rows = self.converter.convert_columns(&key_columns)?;
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
            |i| std::future::ready(self.column(i)),
        ))
    }

    /// The rows of one column, all batches concatenated.
    fn column(&self, index: usize) -> Result<ArrayRef> {
        let arrays: Vec<&dyn Array> = self
            .batches
            .iter()
            .map(|batch| batch.column(index).as_ref())
            .collect();
        if arrays.is_empty() {
            return Ok(new_empty_array(self.schema.field(index).data_type()));
        }
        Ok(concat(&arrays)?)
    }
}

/// The encoded key-filter body over `values`.
fn filter_body(values: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    KeyFilter::build(values).encode(&mut body);
    body
}

/// Writes a file of `rows` rows whose column `i` is `column(i).await`. The rows must already
/// be sorted by `sort_key` (schema indexes); this is not checked here.
pub(crate) async fn write_file<F, Fut>(
    schema: &SchemaRef,
    sort_key: &[usize],
    config: &Config,
    rows: u64,
    mut column: F,
) -> Result<(Vec<u8>, FileSummary)>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = Result<ArrayRef>>,
{
    let mut out = Out(Vec::new());
    write_file_header(&mut out);
    let mut codec = SegmentCodec::new(config.zstd_level)?;
    let mut frame_codec = SegmentCodec::new(FRAME_ZSTD_LEVEL)?;

    let mut directory = Vec::with_capacity(schema.fields().len());
    let mut pages = Out(Vec::new());
    let mut sort_entries = Out(Vec::new());
    let mut summaries = Vec::with_capacity(schema.fields().len());
    let mut sort_key_filter = Vec::new();
    let mut key_filters: Vec<(u32, SegmentRef)> = Vec::new();
    let mut pending_maps: Vec<PendingMap> = Vec::new();
    let first_sort_column = sort_key.first().copied();
    for (index, field) in schema.fields().iter().enumerate() {
        let array = column(index).await?;
        let column_id = u32::try_from(index).map_err(|_| StrataError::Unsupported {
            what: "more than 4 billion columns".to_string(),
        })?;
        if payload::is_payload(array.as_ref(), config) {
            let (header, stats) = payload::write_payload_column(
                array.as_ref(),
                config,
                &mut codec,
                &mut frame_codec,
                &mut out,
            )?;
            let pages_offset =
                u32::try_from(pages.len()).map_err(|_| StrataError::Unsupported {
                    what: "more than 4 GiB of block-table page references".to_string(),
                })?;
            directory.push(DirectoryRecord {
                column_id,
                parent: u32::MAX,
                layer: Layer::Payload,
                flags: 0,
                pages_offset,
                n_pages: 0,
                n_chunks: 0,
                dict_offset: 0,
                dict_len: 0,
                payload_offset: header.offset,
                payload_len: header.len,
                null_count: array.null_count() as u64,
            });
            summaries.push(ColumnSummary {
                name: field.name().clone(),
                encoding_counts: [0; 12],
                bytes: stats.bytes,
                chunks: 0,
                filter_bytes: 0,
                payload: Some(stats),
                shredded_keys: 0,
            });
            continue;
        }
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
                encoding_counts: [0; 12],
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
        let (record, summary, entries) =
            write_narrow(config, &array, column_id, &mut codec, &mut out, &mut pages)?;
        let mut filter_bytes = 0u64;
        if is_sort_key {
            sort_key_filter = filter.unwrap_or_default();
            filter_bytes = sort_key_filter.len() as u64;
        } else if let Some(body) = filter {
            let seg = encode_segment(Kind::KeyFilter, &body, Codec::Zstd, &mut codec, &mut out)?;
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
            let (mut record, part, _) =
                write_narrow(config, child, child_id, &mut codec, &mut out, &mut pages)?;
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
    let hot_start = out.len();
    let hot = HotTail {
        schema: Arc::clone(schema),
        directory,
        sort_key_entries: sort_entries.0,
        sort_key_filter,
        maps: layouts,
        key_filters,
        pages: pages.0,
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
    };
    postscript.encode(&mut out);

    let summary = FileSummary {
        rows,
        bytes: out.len() as u64,
        hot_len,
        columns: summaries,
    };
    Ok((out.0, summary))
}

/// Writes a narrow column: its dictionary, its chunks and its block-table pages, in that
/// order. The record has `layer = Narrow` and `parent = u32::MAX`; a caller that stores the
/// column as a map child or residual sets both. The summary's name is empty, for the caller
/// to set.
fn write_narrow(
    config: &Config,
    array: &ArrayRef,
    column_id: u32,
    codec: &mut SegmentCodec,
    out: &mut Out,
    pages: &mut Out,
) -> Result<(DirectoryRecord, ColumnSummary, Vec<BlockEntry>)> {
    let dict = for_column(array.as_ref(), config);
    let (dict_offset, dict_len) = match &dict {
        Some(dict) => {
            let mut body = Out(Vec::new());
            dict.encode(&mut body)?;
            let seg = encode_segment(Kind::Dictionary, &body.0, Codec::Zstd, codec, out)?;
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
    let mut counts = [0u32; 12];
    let mut bytes = u64::from(dict_len);
    for range in ranges {
        let slice = array.slice(range.start, range.len());
        let EncodedChunk {
            encoding,
            flags,
            const_len,
            min,
            max,
            body,
        } = encode_chunk(&slice, dict.as_ref(), config)?;
        let (segment_offset, segment_len) = match body {
            Some(body) => {
                let seg = encode_segment(Kind::NarrowChunk, &body, Codec::Zstd, codec, out)?;
                (seg.offset, seg.len)
            }
            None => (0, 0),
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
        let seg = encode_segment(Kind::BlockTablePage, &body.0, Codec::Raw, codec, out)?;
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
        flags: u8::from(dict.is_some()),
        pages_offset,
        n_pages,
        n_chunks,
        dict_offset,
        dict_len,
        payload_offset: 0,
        payload_len: 0,
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

    /// xxh3_64 of the file written from `wide_batch(20_000)` with `Config::default()`.
    /// Recorded on the M3.2b code, where the `attrs` map is shredded. A change to this value
    /// means the output changed.
    const GOLDEN: u64 = 0xfd4f_9db0_9179_db82;

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
