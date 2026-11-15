//! The writer: it checks and keeps batches in memory, then writes the whole file in
//! [`StrataWriter::finish`].

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, RecordBatch, new_empty_array};
use arrow::compute::concat;
use arrow::datatypes::{DataType, Schema, SchemaRef};
use arrow::row::{OwnedRow, RowConverter, SortField};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{Out, VERSION, checksum32};
use crate::hot::{DirectoryRecord, HotTail, Layer, PageRef, Postscript, write_file_header};
use crate::narrow::{
    BlockEntry, Class, EncodedChunk, MAX_CHUNK_ROWS, chunk_ranges, encode_chunk, for_column,
    numeric_of,
};
use crate::segment::{Codec, Kind, encode_segment};

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
    /// Chunks per encoding, indexed by the encoding's value.
    pub encoding_counts: [u32; 11],
    /// Bytes of the column's segments, block-table pages included.
    pub bytes: u64,
    /// Number of chunks.
    pub chunks: u32,
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
        if config.page_max_entries == 0 {
            return Err(StrataError::Schema {
                reason: "page_max_entries must be at least 1".to_string(),
            });
        }
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
        if total >= 1 << 32 {
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
        let mut out = Out(Vec::new());
        write_file_header(&mut out);

        let mut directory = Vec::with_capacity(self.schema.fields().len());
        let mut pages = Out(Vec::new());
        let mut sort_entries = Out(Vec::new());
        let mut summaries = Vec::with_capacity(self.schema.fields().len());
        let first_sort_column = self.sort_key.first().copied();
        for (index, field) in self.schema.fields().iter().enumerate() {
            let array = self.column(index)?;
            let dict = for_column(array.as_ref(), &self.config);
            let (dict_offset, dict_len) = match &dict {
                Some(dict) => {
                    let mut body = Out(Vec::new());
                    dict.encode(&mut body)?;
                    let seg = encode_segment(
                        Kind::Dictionary,
                        &body.0,
                        Codec::Zstd,
                        self.config.zstd_level,
                        &mut out,
                    )?;
                    (seg.offset, seg.len)
                }
                None => (0, 0),
            };
            let ranges = chunk_ranges(
                array.as_ref(),
                self.config.chunk_max_rows,
                self.config.chunk_max_bytes,
                dict.is_some(),
            )?;
            let mut entries = Vec::with_capacity(ranges.len());
            let mut counts = [0u32; 11];
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
                } = encode_chunk(&slice, dict.as_ref(), &self.config)?;
                let (segment_offset, segment_len) = match body {
                    Some(body) => {
                        let seg = encode_segment(
                            Kind::NarrowChunk,
                            &body,
                            Codec::Zstd,
                            self.config.zstd_level,
                            &mut out,
                        )?;
                        (seg.offset, seg.len)
                    }
                    None => (0, 0),
                };
                bytes += u64::from(segment_len);
                if let Some(count) = counts.get_mut(encoding as usize) {
                    *count += 1;
                }
                entries.push(BlockEntry {
                    first_row: u32::try_from(range.start).map_err(|_| {
                        StrataError::TooManyRows {
                            rows: range.start as u64,
                        }
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

            let pages_offset =
                u32::try_from(pages.len()).map_err(|_| StrataError::Unsupported {
                    what: "more than 4 GiB of block-table page references".to_string(),
                })?;
            let mut n_pages = 0u32;
            for page in entries.chunks(self.config.page_max_entries) {
                let mut body = Out(Vec::with_capacity(page.len() * 56));
                for entry in page {
                    entry.encode(&mut body);
                }
                let seg = encode_segment(
                    Kind::BlockTablePage,
                    &body.0,
                    Codec::Raw,
                    self.config.zstd_level,
                    &mut out,
                )?;
                let first_row = page.first().map_or(0, |entry| entry.first_row);
                PageRef {
                    first_row,
                    offset: seg.offset,
                    len: seg.len,
                }
                .encode(&mut pages);
                bytes += u64::from(seg.len);
                n_pages += 1;
            }

            if Some(index) == first_sort_column {
                for entry in &entries {
                    entry.encode(&mut sort_entries);
                }
            }
            let n_chunks = u32::try_from(entries.len()).map_err(|_| StrataError::Unsupported {
                what: "more than 4 billion chunks".to_string(),
            })?;
            directory.push(DirectoryRecord {
                column_id: u32::try_from(index).map_err(|_| StrataError::Unsupported {
                    what: "more than 4 billion columns".to_string(),
                })?,
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
            });
            summaries.push(ColumnSummary {
                name: field.name().clone(),
                encoding_counts: counts,
                bytes,
                chunks: n_chunks,
            });
        }

        let hot_start = out.len();
        let hot = HotTail {
            schema: Arc::clone(&self.schema),
            directory,
            sort_key_entries: sort_entries.0,
            sort_key_filter: Vec::new(),
            map_keys: Vec::new(),
            key_filter_index: Vec::new(),
            pages: pages.0,
        };
        hot.encode(&mut out)?;
        let hot_len =
            u32::try_from(out.len() - hot_start).map_err(|_| StrataError::Unsupported {
                what: "a hot tail of more than 4 GiB".to_string(),
            })?;
        let hot_checksum = checksum32(&[out.0.get(hot_start..).unwrap_or_default()]);
        let postscript = Postscript {
            hot_offset: hot_start as u64,
            hot_len,
            hot_checksum,
            row_count: self.rows,
            n_columns: u32::try_from(self.schema.fields().len()).map_err(|_| {
                StrataError::Unsupported {
                    what: "more than 4 billion columns".to_string(),
                }
            })?,
            version: VERSION,
            flags: 0,
        };
        postscript.encode(&mut out);

        let summary = FileSummary {
            rows: self.rows,
            bytes: out.len() as u64,
            hot_len,
            columns: summaries,
        };
        Ok((out.0, summary))
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
