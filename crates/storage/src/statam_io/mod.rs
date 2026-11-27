//! Concrete Statam file I/O adapters (`io`, `BatchFileWriter`,
//! `write_file_batch`, and low-level maintenance readers/verifiers).

pub(crate) mod io;

use std::io::{Cursor, Seek, Write};
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{
    Batch, CancelToken, Column, DataType, FlatData, ScalarValue, Schema, Selection,
};
use cetra_core::schema::{scores, spans, traces};
use cetra_query::Expr;
use cetra_query::exec::{SortKey, compare_rows};
use cetra_statam::{
    ChunkPackBuilder, ChunkPackReader, Predicate, ScanRequest, StatamError, StatamFileMeta,
    StatamReader, StatamWriter, TableKind, WriteOptions, WriteProfile, ZonePred, ZoneValue,
};
use object_store::ObjectStoreExt;
use object_store::path::Path as ObjectPath;

use crate::catalog::{FileMeta, FileTable};
use crate::error::{AnchorError, CompactionError, EngineError, FileWriteError};
use crate::objects::StorageIo;
use crate::statam_io::io::{open_range_reader, run_blocking_io};
use crate::unwind::guard_sync;
use crate::verify::{Verdict, is_corrupt_store_error};

/// Maps a catalog [`FileTable`] to its Statam [`TableKind`].
pub(crate) fn table_kind(table: FileTable) -> TableKind {
    match table {
        FileTable::Spans => TableKind::Spans,
        FileTable::Traces => TableKind::Traces,
        FileTable::Scores => TableKind::Scores,
    }
}

/// Logical sort-column indices for `table`, matching [`sort_columns`].
pub(crate) fn sort_col_indices(table: FileTable) -> Vec<u16> {
    let schema = table.column_schema();
    sort_columns(table)
        .iter()
        .filter_map(|name| {
            schema
                .index_of(name)
                .and_then(|idx| u16::try_from(idx).ok())
        })
        .collect()
}

/// One encoded Statam object and the catalog fields that describe it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrittenFile {
    /// The complete `.statam` file bytes, including Hot Tail + Postscript.
    pub(crate) bytes: Bytes,
    /// Rows in the file.
    pub(crate) rows: u64,
    /// Stored Hot Tail length (including the 112-byte Postscript).
    pub(crate) footer_size: u64,
    /// Smallest event time, nanoseconds. `start_time` for spans/traces, `created_at` for scores.
    pub(crate) min_time: i64,
    /// Largest event time, nanoseconds.
    pub(crate) max_time: i64,
    /// Smallest `ingest_time`, nanoseconds.
    pub(crate) min_ingest_time: i64,
    /// Largest `ingest_time`, nanoseconds.
    pub(crate) max_ingest_time: i64,
}

/// What [`BatchFileWriter::finish`] measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileSummary {
    /// Rows written.
    pub(crate) rows: u64,
    /// Stored Hot Tail length (including Postscript), for 1-GET remote tail prefetch.
    pub(crate) footer_size: u64,
    /// Smallest event time, nanoseconds.
    pub(crate) min_time: i64,
    /// Largest event time, nanoseconds.
    pub(crate) max_time: i64,
    /// Smallest `ingest_time`, nanoseconds.
    pub(crate) min_ingest_time: i64,
    /// Largest `ingest_time`, nanoseconds.
    pub(crate) max_ingest_time: i64,
}

pub(crate) fn sort_columns(table: FileTable) -> &'static [&'static str] {
    match table {
        FileTable::Spans => &[spans::TRACE_ID, spans::START_TIME],
        FileTable::Traces => &[traces::START_TIME, traces::TRACE_ID],
        FileTable::Scores => &[scores::CREATED_AT],
    }
}

pub(crate) fn time_column(table: FileTable) -> &'static str {
    match table {
        FileTable::Spans => spans::START_TIME,
        FileTable::Traces => traces::START_TIME,
        FileTable::Scores => scores::CREATED_AT,
    }
}

pub(crate) fn ingest_column(table: FileTable) -> &'static str {
    match table {
        FileTable::Spans => spans::INGEST_TIME,
        FileTable::Traces => traces::LAST_INGEST_TIME,
        FileTable::Scores => scores::INGEST_TIME,
    }
}

/// Event-time and ingest-time bounds measured across batches of a file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bounds {
    pub(crate) min_time: i64,
    pub(crate) max_time: i64,
    pub(crate) min_ingest_time: i64,
    pub(crate) max_ingest_time: i64,
}

impl Bounds {
    pub(crate) fn merge(&mut self, other: Self) {
        self.min_time = self.min_time.min(other.min_time);
        self.max_time = self.max_time.max(other.max_time);
        self.min_ingest_time = self.min_ingest_time.min(other.min_ingest_time);
        self.max_ingest_time = self.max_ingest_time.max(other.max_ingest_time);
    }
}

/// Sorts, encodes, and returns one self-contained Statam file from `cetra_column::Batch` inputs.
///
/// CPU-bound: call from `spawn_blocking`. `level` is `0` or `1`.
pub(crate) fn write_file_batch(
    table: FileTable,
    level: u8,
    batches: &[Arc<Batch>],
) -> Result<WrittenFile, FileWriteError> {
    if level > 1 {
        return Err(FileWriteError::invalid("level must be 0 or 1"));
    }
    let col_schema = table.column_schema();
    let mut non_empty: Vec<&Batch> = Vec::new();
    for batch in batches {
        if batch.schema().fields() != col_schema.fields() {
            return Err(FileWriteError::invalid("batch schema"));
        }
        if batch.num_rows() > 0 {
            non_empty.push(batch.as_ref());
        }
    }
    if non_empty.is_empty() {
        return Err(FileWriteError::invalid("empty batch"));
    }

    let combined = Batch::concat(Arc::clone(&col_schema), &non_empty, None)
        .map_err(FileWriteError::from_column)?;
    let sorted = sort_batch(table, &combined)?;
    let avg_row_bytes = sorted.memory_bytes() / sorted.num_rows().max(1);
    let mini_block_size = if avg_row_bytes > 1024 {
        Some(((1 << 20) / avg_row_bytes).clamp(64, 8_192))
    } else {
        None
    };
    let mut writer = BatchFileWriter::new_with_pack_and_mb(
        table,
        level,
        Cursor::new(Vec::new()),
        None::<ChunkPackBuilder<Cursor<Vec<u8>>>>,
        mini_block_size,
    )?;
    writer.write(&sorted)?;
    let (cursor, summary, _pack) = writer.finish()?;
    let bytes = Bytes::from(cursor.into_inner());
    Ok(WrittenFile {
        bytes,
        rows: summary.rows,
        footer_size: summary.footer_size,
        min_time: summary.min_time,
        max_time: summary.max_time,
        min_ingest_time: summary.min_ingest_time,
        max_ingest_time: summary.max_ingest_time,
    })
}

/// Encodes one Statam file from `cetra_column::Batch`es already in the table's sort order,
/// optionally streaming external payload chunks into an owned [`ChunkPackBuilder`].
///
/// CPU-bound and blocking: call from `spawn_blocking`.
pub(crate) struct BatchFileWriter<
    W: Write + Seek + Send,
    PW: Write + Seek + Send + 'static = Cursor<Vec<u8>>,
> {
    table: FileTable,
    level: u8,
    col_schema: Arc<Schema>,
    writer: StatamWriter<'static, W, PW>,
    bounds: Option<Bounds>,
    rows: u64,
}

impl<W: Write + Seek + Send, PW: Write + Seek + Send + 'static> std::fmt::Debug
    for BatchFileWriter<W, PW>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchFileWriter")
            .field("table", &self.table.as_str())
            .field("level", &self.level)
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

impl<W: Write + Seek + Send> BatchFileWriter<W, Cursor<Vec<u8>>> {
    /// Creates a self-contained [`BatchFileWriter`] without an external `.stpk` pack.
    pub(crate) fn new(
        table: FileTable,
        level: u8,
        sink: W,
        _expected_rows: u64,
    ) -> Result<Self, FileWriteError> {
        Self::new_with_pack(table, level, sink, None)
    }
}

impl<W: Write + Seek + Send, PW: Write + Seek + Send + 'static> BatchFileWriter<W, PW> {
    /// Creates a [`BatchFileWriter`] with an optional streaming [`ChunkPackBuilder`].
    pub(crate) fn new_with_pack(
        table: FileTable,
        level: u8,
        sink: W,
        pack: Option<ChunkPackBuilder<PW>>,
    ) -> Result<Self, FileWriteError> {
        Self::new_with_pack_and_mb(table, level, sink, pack, None)
    }

    fn new_with_pack_and_mb(
        table: FileTable,
        level: u8,
        sink: W,
        pack: Option<ChunkPackBuilder<PW>>,
        mini_block_size: Option<usize>,
    ) -> Result<Self, FileWriteError> {
        if level > 1 {
            return Err(FileWriteError::invalid("level must be 0 or 1"));
        }
        let col_schema = table.column_schema();
        let profile = if level == 0 {
            WriteProfile::L0Fast
        } else {
            WriteProfile::L1Compact
        };
        let mut opts = WriteOptions::for_table(table_kind(table), profile);
        opts.sort_cols = sort_col_indices(table);
        if let Some(mb) = mini_block_size {
            opts.mini_block_size = mb;
        }
        let writer = StatamWriter::new_with_owned_pack(sink, Arc::clone(&col_schema), opts, pack)
            .map_err(FileWriteError::statam)?;
        Ok(Self {
            table,
            level,
            col_schema,
            writer,
            bounds: None,
            rows: 0,
        })
    }

    /// Appends one batch. An empty batch is ignored.
    pub(crate) fn write(&mut self, batch: &Batch) -> Result<(), FileWriteError> {
        if batch.schema().fields() != self.col_schema.fields() {
            return Err(FileWriteError::invalid("batch schema"));
        }
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch_bounds = extract_bounds(self.table, batch)?;
        let owned_compacted;
        let dense = if batch.selection().is_all(batch.raw_len()) {
            batch
        } else {
            owned_compacted = batch.compact(None).map_err(FileWriteError::from_column)?;
            &owned_compacted
        };

        let num_rows = dense.num_rows();
        self.writer
            .write_batch(dense)
            .map_err(FileWriteError::statam)?;

        match &mut self.bounds {
            Some(b) => b.merge(batch_bounds),
            None => self.bounds = Some(batch_bounds),
        }
        self.rows = self
            .rows
            .checked_add(u64::try_from(num_rows).map_err(|_| FileWriteError::invalid("row count"))?)
            .ok_or_else(|| FileWriteError::invalid("row count"))?;
        Ok(())
    }

    /// Writes the Hot Tail + Postscript and returns `(sink, summary, owned_pack)`.
    ///
    /// A file with no rows is an error.
    #[allow(clippy::type_complexity)]
    pub(crate) fn finish(
        self,
    ) -> Result<(W, FileSummary, Option<ChunkPackBuilder<PW>>), FileWriteError> {
        let bounds = self
            .bounds
            .ok_or_else(|| FileWriteError::invalid("empty file"))?;
        let (sink, stats, pack) = self
            .writer
            .finish_into_inner()
            .map_err(FileWriteError::statam)?;
        let summary = FileSummary {
            rows: self.rows,
            footer_size: u64::from(stats.hot_tail_bytes),
            min_time: bounds.min_time,
            max_time: bounds.max_time,
            min_ingest_time: bounds.min_ingest_time,
            max_ingest_time: bounds.max_ingest_time,
        };
        Ok((sink, summary, pack))
    }
}

fn sort_batch(table: FileTable, batch: &Batch) -> Result<Batch, FileWriteError> {
    let schema = batch.schema();
    let names = sort_columns(table);
    let mut keys = Vec::with_capacity(names.len());
    let mut key_cols = Vec::with_capacity(names.len());
    for name in names {
        let idx = schema
            .index_of(name)
            .ok_or_else(|| FileWriteError::invalid("missing column"))?;
        let col = batch
            .column(idx)
            .ok_or_else(|| FileWriteError::invalid("missing column"))?;
        let dt = col.data_type();
        keys.push(SortKey {
            expr: Expr::Column {
                index: idx,
                name: (*name).to_owned(),
                data_type: dt,
            },
            descending: false,
            nulls_first: false,
        });
        key_cols.push(col.clone());
    }

    let mut phys_indices = batch
        .selection()
        .to_indices(batch.raw_len())
        .map_err(FileWriteError::from_column)?;
    phys_indices.sort_by(|&a, &b| compare_rows(&key_cols, &keys, a as usize, b as usize));
    batch
        .with_selection(Selection::Indices(phys_indices))
        .and_then(|reordered| reordered.compact(None))
        .map_err(FileWriteError::from_column)
}

pub(crate) fn extract_bounds(table: FileTable, batch: &Batch) -> Result<Bounds, FileWriteError> {
    let (min_time, max_time) = column_ts_bounds(batch, time_column(table))?;
    let (min_ingest_time, max_ingest_time) = column_ts_bounds(batch, ingest_column(table))?;
    Ok(Bounds {
        min_time,
        max_time,
        min_ingest_time,
        max_ingest_time,
    })
}

fn column_ts_bounds(batch: &Batch, name: &str) -> Result<(i64, i64), FileWriteError> {
    let col = batch
        .column_by_name(name)
        .ok_or_else(|| FileWriteError::invalid("missing column"))?;
    if !matches!(col.data_type(), DataType::TimestampNs { .. }) {
        return Err(FileWriteError::invalid("invalid time column"));
    }

    if let Column::Const { value, len } = col {
        if *len == 0 || batch.num_rows() == 0 {
            return Err(FileWriteError::invalid("empty time column"));
        }
        return match value {
            ScalarValue::TimestampNs { value, .. } => Ok((*value, *value)),
            ScalarValue::Null(_) => Err(FileWriteError::invalid("empty time column")),
            _ => Err(FileWriteError::invalid("invalid time column")),
        };
    }

    let mut min_val: Option<i64> = None;
    let mut max_val: Option<i64> = None;
    let mut visit = |row: usize| {
        if let Some(v) = read_ts(col, row) {
            min_val = Some(min_val.map_or(v, |cur| cur.min(v)));
            max_val = Some(max_val.map_or(v, |cur| cur.max(v)));
        }
    };
    match batch.selection() {
        Selection::All => {
            for row in 0..batch.raw_len() {
                visit(row);
            }
        }
        Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                visit(row);
            }
        }
        Selection::Indices(indices) => {
            for &row in indices {
                visit(row as usize);
            }
        }
    }
    match (min_val, max_val) {
        (Some(lo), Some(hi)) => Ok((lo, hi)),
        _ => Err(FileWriteError::invalid("empty time column")),
    }
}

fn read_ts(col: &Column, row: usize) -> Option<i64> {
    match col {
        Column::Flat(flat) => {
            if flat.is_null(row) {
                return None;
            }
            match flat.data() {
                FlatData::TimestampNs { values, .. } => values.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        }),
        Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                return None;
            }
            match dict.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                return None;
            }
            match rle.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(run).copied(),
                _ => None,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Low-level Statam maintenance readers & verifier
// ---------------------------------------------------------------------------

/// Reads every non-null timestamp value of `column` across `files`, sorted ascending.
pub(crate) async fn read_time_column(
    io: &StorageIo,
    files: &[Arc<FileMeta>],
    column: &str,
) -> Result<Vec<i64>, CompactionError> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let io = io.clone();
    let files: Vec<Arc<FileMeta>> = files.to_vec();
    let column = column.to_owned();
    let handle = tokio::runtime::Handle::current();

    run_blocking_io(handle.clone(), move || {
        let mut values = Vec::new();
        for file in &files {
            let res: Result<(), StatamError> = guard_sync(|| {
                let rr = open_range_reader(
                    &io,
                    &file.path,
                    file.size,
                    handle.clone(),
                    CancelToken::new(),
                )?;
                let meta = Arc::new(StatamFileMeta::read_from_with_hint(
                    rr.as_ref(),
                    Some(file.footer_size),
                )?);
                let col_idx = meta.schema_meta.schema.index_of(&column).ok_or(
                    StatamError::SchemaMismatch {
                        reason: "time column missing from statam file",
                    },
                )?;
                let reader = Arc::new(StatamReader::open_with_shared_meta(rr, meta, None)?);
                let mut iter = reader.scan_iter(ScanRequest {
                    projection: Some(vec![col_idx]),
                    read_threads: 1,
                    ..ScanRequest::default()
                })?;
                while let Some(batch) = iter.next_batch(None, None)? {
                    let col = batch.column(0).ok_or(StatamError::SchemaMismatch {
                        reason: "projected time column missing",
                    })?;
                    match batch.selection() {
                        Selection::All => {
                            for row in 0..batch.raw_len() {
                                if let Some(ts) = read_ts(col, row) {
                                    values.push(ts);
                                }
                            }
                        }
                        Selection::Mask(mask) => {
                            for row in mask.iter_ones() {
                                if let Some(ts) = read_ts(col, row) {
                                    values.push(ts);
                                }
                            }
                        }
                        Selection::Indices(indices) => {
                            for &row in indices {
                                if let Some(ts) = read_ts(col, row as usize) {
                                    values.push(ts);
                                }
                            }
                        }
                    }
                }
                Ok(())
            });
            res.map_err(|err| CompactionError::Read {
                source: EngineError::from_statam(&err),
            })?;
        }
        values.sort_unstable();
        Ok(values)
    })
    .await
    .map_err(|()| CompactionError::Task)?
}

/// Reads the anchor columns (`trace_id`, `start_time`, `end_time`, `last_ingest_time`)
/// of one `traces` Statam file, pruning mini-blocks with `last_ingest_time < since`.
pub(crate) async fn read_trace_anchors(
    io: &StorageIo,
    file: &FileMeta,
    since: i64,
) -> Result<Vec<Batch>, AnchorError> {
    let io = io.clone();
    let path = file.path.clone();
    let size = file.size;
    let footer_size = file.footer_size;
    let handle = tokio::runtime::Handle::current();

    run_blocking_io(handle.clone(), move || {
        let res: Result<Vec<Batch>, StatamError> = guard_sync(|| {
            let rr = open_range_reader(&io, &path, size, handle.clone(), CancelToken::new())?;
            let meta = Arc::new(StatamFileMeta::read_from_with_hint(
                rr.as_ref(),
                Some(footer_size),
            )?);
            let schema = &meta.schema_meta.schema;
            let mut proj = Vec::with_capacity(4);
            for name in [
                traces::TRACE_ID,
                traces::START_TIME,
                traces::END_TIME,
                traces::LAST_INGEST_TIME,
            ] {
                let idx = schema.index_of(name).ok_or(StatamError::SchemaMismatch {
                    reason: "trace anchor column missing",
                })?;
                proj.push(idx);
            }
            let reader = Arc::new(StatamReader::open_with_shared_meta(rr, meta, None)?);
            let mut iter = reader.scan_iter(ScanRequest {
                projection: Some(proj),
                predicates: vec![Predicate::new(
                    traces::LAST_INGEST_TIME,
                    ZonePred::Ge(ZoneValue::TimestampNs(since)),
                )],
                read_threads: 1,
                ..ScanRequest::default()
            })?;
            let mut out = Vec::new();
            while let Some(batch) = iter.next_batch(None, None)? {
                if batch.num_rows() > 0 {
                    out.push(batch);
                }
            }
            Ok(out)
        });
        res.map_err(AnchorError::statam)
    })
    .await
    .map_err(|()| AnchorError::statam(crate::unwind::decoder_panicked()))?
}

/// Whole-file integrity check of a Statam file and its optional sibling `.stpk` pack.
pub(crate) async fn verify_statam_file(io: &StorageIo, file: &FileMeta) -> Verdict {
    let Ok(statam_key) = ObjectPath::parse(&file.path) else {
        return Verdict::Corrupt {
            reason: "decode failed",
        };
    };
    match io.store().head(&statam_key).await {
        Ok(meta) => {
            if meta.size != file.size {
                return Verdict::Corrupt {
                    reason: "size differs from the catalog",
                };
            }
        }
        Err(err) if is_corrupt_store_error(&err) => {
            return Verdict::Corrupt {
                reason: "missing object",
            };
        }
        Err(_) => return Verdict::Unavailable,
    }

    if let Some(pack) = &file.pack {
        let Ok(pack_key) = ObjectPath::parse(&pack.path) else {
            return Verdict::Corrupt {
                reason: "decode failed",
            };
        };
        match io.store().head(&pack_key).await {
            Ok(meta) => {
                if meta.size != pack.size {
                    return Verdict::Corrupt {
                        reason: "pack size differs from the catalog",
                    };
                }
            }
            Err(err) if is_corrupt_store_error(&err) => {
                return Verdict::Corrupt {
                    reason: "missing pack object",
                };
            }
            Err(_) => return Verdict::Unavailable,
        }
    }

    let io = io.clone();
    let file = file.clone();
    let handle = tokio::runtime::Handle::current();
    let joined = run_blocking_io(handle.clone(), move || {
        guard_sync(|| {
            let statam_rr = open_range_reader(
                &io,
                &file.path,
                file.size,
                handle.clone(),
                CancelToken::new(),
            )?;
            let statam = StatamReader::open(statam_rr, None)?;
            if let Some(pack) = &file.pack {
                let pack_rr = open_range_reader(
                    &io,
                    &pack.path,
                    pack.size,
                    handle.clone(),
                    CancelToken::new(),
                )?;
                let pack_reader = ChunkPackReader::open(pack_rr)?;
                statam.verify_full_with_pack(
                    Some((pack.id, &pack_reader)),
                    file.rows,
                    table_kind(file.table),
                )
            } else {
                statam.verify_full_with_pack(None, file.rows, table_kind(file.table))
            }
        })
    })
    .await;

    match joined {
        Ok(Ok(())) => Verdict::Healthy,
        Ok(Err(err))
            if err.is_store_error()
                || matches!(&err, StatamError::Io(_) if !err.is_corrupt())
                || matches!(err, StatamError::Cancelled | StatamError::MemoryExceeded) =>
        {
            Verdict::Unavailable
        }
        Err(()) => Verdict::Unavailable,
        Ok(Err(_)) => Verdict::Corrupt {
            reason: "decode failed",
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::sync::Arc;

    use bytes::Bytes;
    use cetra_column::{Batch, Bitmap, Column, ScalarValue, Selection};
    use cetra_core::schema::{scores, traces};
    use cetra_statam::{ChunkPackBuilder, PackWriteOptions, SliceReader, StatamFileMeta};
    use chrono::NaiveDate;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use uuid::Uuid;

    use super::*;
    use crate::catalog::{FileId, PackMeta};
    use crate::engine::statam_source::tests::read_stored_statam;
    use crate::test_support::{
        Call, ChaosStore, Fault, score_row, score_rows, span_row, span_rows, trace_batch,
    };

    fn project() -> cetra_core::ids::ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 11).unwrap()
    }

    fn file_meta(table: FileTable, path: &str, written: &WrittenFile) -> FileMeta {
        FileMeta {
            id: FileId::new_v7(),
            table,
            project: project(),
            day: day(),
            level: 0,
            path: path.to_owned(),
            size: written.bytes.len() as u64,
            footer_size: written.footer_size,
            rows: written.rows,
            min_time: written.min_time,
            max_time: written.max_time,
            min_ingest_time: written.min_ingest_time,
            max_ingest_time: written.max_ingest_time,
            fully_merged: false,
            deletions_applied_seq: 0,
            pack: None,
        }
    }

    #[test]
    fn batch_file_writer_and_write_file_batch_round_trip() {
        let mut s1 = span_row(2, 1, 100, 30);
        s1.schema_version = 2;
        let s2 = span_row(1, 2, 50, 10);
        let s3 = span_row(1, 1, 20, 40);
        let col_batch = Arc::new(span_rows(&[s1, s2, s3]));

        let written = write_file_batch(FileTable::Spans, 0, &[Arc::clone(&col_batch)]).unwrap();
        assert_eq!(written.rows, 3);
        assert_eq!((written.min_time, written.max_time), (20, 100));
        assert_eq!((written.min_ingest_time, written.max_ingest_time), (10, 40));
        let meta =
            StatamFileMeta::read_from(&SliceReader::new(written.bytes.clone()), None).unwrap();
        assert_eq!(meta.postscript.row_count, 3);
        assert_eq!(meta.schema_meta.table_kind, TableKind::Spans);
        assert_eq!(
            meta.schema_meta.sort_cols,
            sort_col_indices(FileTable::Spans)
        );
        let back = read_stored_statam(written.bytes, None, FileTable::Spans.column_schema());
        assert_eq!(back.num_rows(), 3);

        // Traces round-trip.
        let traces_col = Arc::new(trace_batch(&[
            span_row(2, 1, 100, 30),
            span_row(1, 2, 50, 10),
        ]));
        let traces_written = write_file_batch(FileTable::Traces, 0, &[traces_col]).unwrap();
        let tr_back = read_stored_statam(
            traces_written.bytes,
            None,
            FileTable::Traces.column_schema(),
        );
        assert_eq!(tr_back.num_rows(), 2);

        // Scores round-trip + streamed BatchFileWriter with Const & Selection::Mask.
        let sc1 = score_row(1, 0, 20, 21, 22, false, "a");
        let sc2 = score_row(2, 0, 10, 11, 12, false, "b");
        let scores_col = score_rows(&[sc1, sc2]);
        let mut cols = scores_col.columns().to_vec();
        let p_idx = scores_col.schema().index_of(scores::PROJECT_ID).unwrap();
        cols[p_idx] =
            Arc::new(Column::try_new_const(ScalarValue::Utf8(project().to_string()), 2).unwrap());
        let masked = Batch::try_new_shared(
            Arc::clone(scores_col.schema()),
            cols,
            2,
            Selection::Mask(Bitmap::from_bools(&[false, true])),
        )
        .unwrap();
        let mut writer =
            BatchFileWriter::new(FileTable::Scores, 1, Cursor::new(Vec::new()), 1).unwrap();
        writer.write(&masked).unwrap();
        let (buf, summary, pack) = writer.finish().unwrap();
        assert!(pack.is_none());
        assert_eq!(summary.rows, 1);
        assert_eq!(summary.min_time, 10);
        assert_eq!(summary.max_time, 10);
        assert_eq!(summary.min_ingest_time, 12);
        assert_eq!(summary.max_ingest_time, 12);
        assert!(summary.footer_size > 0);
        let sc_back = read_stored_statam(
            Bytes::from(buf.into_inner()),
            None,
            FileTable::Scores.column_schema(),
        );
        assert_eq!(sc_back.num_rows(), 1);
    }

    #[test]
    fn l1_spans_creates_pack_for_large_payloads_and_omits_when_empty() {
        // Large payload (> inline threshold) produces chunks in the pack.
        let mut big = span_row(1, 0, 100, 101);
        big.input = Some("payload-content-".repeat(200));
        let pack_id = *Uuid::now_v7().as_bytes();
        let pb = ChunkPackBuilder::new_streaming(
            pack_id,
            PackWriteOptions::default(),
            Cursor::new(Vec::new()),
        )
        .unwrap();
        let mut writer =
            BatchFileWriter::new_with_pack(FileTable::Spans, 1, Cursor::new(Vec::new()), Some(pb))
                .unwrap();
        writer.write(&span_rows(&[big])).unwrap();
        let (statam_cursor, _summary, pb_opt) = writer.finish().unwrap();
        let pb = pb_opt.unwrap();
        assert!(pb.chunk_count() > 0);
        let (pack_cursor, pstats) = pb.finish_into_inner().unwrap();
        assert!(pstats.toc_stored_bytes > 0);
        let back = read_stored_statam(
            Bytes::from(statam_cursor.into_inner()),
            Some((pack_id, Bytes::from(pack_cursor.into_inner()))),
            FileTable::Spans.column_schema(),
        );
        assert_eq!(back.num_rows(), 1);

        // Small inline-only payload produces 0 pack chunks -> caller omits pack.
        let small = span_row(1, 0, 100, 101);
        let pack_id2 = *Uuid::now_v7().as_bytes();
        let pb2 = ChunkPackBuilder::new_streaming(
            pack_id2,
            PackWriteOptions::default(),
            Cursor::new(Vec::new()),
        )
        .unwrap();
        let mut writer2 =
            BatchFileWriter::new_with_pack(FileTable::Spans, 1, Cursor::new(Vec::new()), Some(pb2))
                .unwrap();
        writer2.write(&span_rows(&[small])).unwrap();
        let (_statam_cursor2, _summary2, pb_opt2) = writer2.finish().unwrap();
        assert_eq!(pb_opt2.unwrap().chunk_count(), 0);
    }

    #[test]
    fn empty_and_foreign_batches_are_rejected() {
        let writer = BatchFileWriter::new(FileTable::Spans, 0, Cursor::new(Vec::new()), 0).unwrap();
        let error = writer.finish().unwrap_err();
        assert!(matches!(
            error,
            FileWriteError::Invalid {
                reason: "empty file"
            }
        ));

        let scores = score_rows(&[score_row(1, 0, 1, 1, 1, false, "accuracy")]);
        let mut writer =
            BatchFileWriter::new(FileTable::Spans, 0, Cursor::new(Vec::new()), 1).unwrap();
        assert!(matches!(
            writer.write(&scores).unwrap_err(),
            FileWriteError::Invalid { .. }
        ));
    }

    #[test]
    fn streamed_tempfile_writer_matches_write_file_batch() {
        let mut rows_vec = Vec::new();
        for i in 0..120_i64 {
            let mut s = span_row(1, 0, i, i + 5);
            s.span_id = cetra_core::ids::SpanId::from_bytes((i + 1).to_be_bytes()).unwrap();
            rows_vec.push(s);
        }
        let batch = Arc::new(span_rows(&rows_vec));
        let in_mem = write_file_batch(FileTable::Spans, 0, &[Arc::clone(&batch)]).unwrap();
        let sink = tempfile::tempfile().unwrap();
        let mut writer = BatchFileWriter::new(FileTable::Spans, 0, sink, 120).unwrap();
        writer.write(&batch).unwrap();
        let (mut sink, summary, _) = writer.finish().unwrap();
        sink.seek(SeekFrom::Start(0)).unwrap();
        let mut buf = Vec::new();
        sink.read_to_end(&mut buf).unwrap();
        assert_eq!(summary.rows, in_mem.rows);
        assert_eq!(summary.min_time, in_mem.min_time);
        assert_eq!(summary.max_time, in_mem.max_time);
        assert_eq!(summary.footer_size, in_mem.footer_size);
        assert_eq!(
            read_stored_statam(Bytes::from(buf), None, FileTable::Spans.column_schema()).num_rows(),
            120
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn verify_statam_file_covers_all_verdicts_and_pack_corruption() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let io = StorageIo::from_store(Arc::clone(&store));

        // 1. Self-contained L0 spans file.
        let batch = span_rows(&[span_row(1, 0, 10, 11), span_row(2, 0, 20, 21)]);
        let written = write_file_batch(FileTable::Spans, 0, &[Arc::new(batch)]).unwrap();
        let path = "tables/spans/test-l0.statam";
        store
            .put(
                &ObjectPath::from(path),
                PutPayload::from(written.bytes.clone()),
            )
            .await
            .unwrap();
        let meta = file_meta(FileTable::Spans, path, &written);
        assert_eq!(verify_statam_file(&io, &meta).await, Verdict::Healthy);

        // Wrong size.
        let mut wrong_size = meta.clone();
        wrong_size.size += 1;
        assert_eq!(
            verify_statam_file(&io, &wrong_size).await,
            Verdict::Corrupt {
                reason: "size differs from the catalog"
            }
        );

        // Wrong row count.
        let mut wrong_rows = meta.clone();
        wrong_rows.rows += 1;
        assert_eq!(
            verify_statam_file(&io, &wrong_rows).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );

        // Missing object.
        let mut missing = meta.clone();
        missing.path = "tables/spans/does-not-exist.statam".to_owned();
        assert_eq!(
            verify_statam_file(&io, &missing).await,
            Verdict::Corrupt {
                reason: "missing object"
            }
        );

        // Corrupt mid-file byte.
        let mut damaged = written.bytes.to_vec();
        let mid = damaged.len() / 2;
        damaged[mid] ^= 0xFF;
        let bad_path = "tables/spans/corrupt-mid.statam";
        store
            .put(
                &ObjectPath::from(bad_path),
                PutPayload::from(Bytes::from(damaged)),
            )
            .await
            .unwrap();
        let mut bad_meta = meta.clone();
        bad_meta.path = bad_path.to_owned();
        assert_eq!(
            verify_statam_file(&io, &bad_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );

        // Transient ChaosStore error -> Unavailable.
        let chaos = Arc::new(ChaosStore::new(Arc::clone(&store)));
        chaos.push(
            Call::Get,
            Fault::Before(crate::test_support::Status::Internal),
        );
        let chaos_io = StorageIo::from_store(chaos.clone() as Arc<dyn ObjectStore>);
        assert_eq!(
            verify_statam_file(&chaos_io, &meta).await,
            Verdict::Unavailable
        );

        // 2. L1 spans file with sibling .stpk pack.
        let mut big = span_row(1, 0, 100, 101);
        big.input = Some("large-payload-block-".repeat(250));
        let pack_id = *Uuid::now_v7().as_bytes();
        let pb = ChunkPackBuilder::new_streaming(
            pack_id,
            PackWriteOptions::default(),
            Cursor::new(Vec::new()),
        )
        .unwrap();
        let mut writer =
            BatchFileWriter::new_with_pack(FileTable::Spans, 1, Cursor::new(Vec::new()), Some(pb))
                .unwrap();
        writer.write(&span_rows(&[big])).unwrap();
        let (st_buf, summary, pb_opt) = writer.finish().unwrap();
        let (pk_buf, pstats) = pb_opt.unwrap().finish_into_inner().unwrap();
        let st_bytes = Bytes::from(st_buf.into_inner());
        let pk_bytes = Bytes::from(pk_buf.into_inner());
        let st_path = "tables/spans/l1.statam";
        let pk_path = "tables/spans/l1.stpk";
        store
            .put(
                &ObjectPath::from(st_path),
                PutPayload::from(st_bytes.clone()),
            )
            .await
            .unwrap();
        store
            .put(
                &ObjectPath::from(pk_path),
                PutPayload::from(pk_bytes.clone()),
            )
            .await
            .unwrap();
        let l1_meta = FileMeta {
            id: FileId::new_v7(),
            table: FileTable::Spans,
            project: project(),
            day: day(),
            level: 1,
            path: st_path.to_owned(),
            size: st_bytes.len() as u64,
            footer_size: summary.footer_size,
            rows: summary.rows,
            min_time: summary.min_time,
            max_time: summary.max_time,
            min_ingest_time: summary.min_ingest_time,
            max_ingest_time: summary.max_ingest_time,
            fully_merged: true,
            deletions_applied_seq: 0,
            pack: Some(PackMeta {
                id: pack_id,
                path: pk_path.to_owned(),
                size: pk_bytes.len() as u64,
                toc_size: u64::from(pstats.toc_stored_bytes),
            }),
        };
        assert_eq!(verify_statam_file(&io, &l1_meta).await, Verdict::Healthy);

        // Corrupt .stpk pack header -> decode failed.
        let mut bad_pk = pk_bytes.to_vec();
        bad_pk[0] ^= 0xFF;
        store
            .put(
                &ObjectPath::from(pk_path),
                PutPayload::from(Bytes::from(bad_pk)),
            )
            .await
            .unwrap();
        assert_eq!(
            verify_statam_file(&io, &l1_meta).await,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );

        // Missing .stpk pack -> missing pack object.
        store.delete(&ObjectPath::from(pk_path)).await.unwrap();
        assert_eq!(
            verify_statam_file(&io, &l1_meta).await,
            Verdict::Corrupt {
                reason: "missing pack object"
            }
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_time_column_and_read_trace_anchors_work() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let io = StorageIo::from_store(Arc::clone(&store));

        let tb = trace_batch(&[
            span_row(1, 0, 100, 1000),
            span_row(2, 0, 200, 2000),
            span_row(3, 0, 300, 3000),
        ]);
        let written = write_file_batch(FileTable::Traces, 0, &[Arc::new(tb)]).unwrap();
        let path = "tables/traces/anchors.statam";
        store
            .put(
                &ObjectPath::from(path),
                PutPayload::from(written.bytes.clone()),
            )
            .await
            .unwrap();
        let meta = file_meta(FileTable::Traces, path, &written);

        let times = read_time_column(&io, &[Arc::new(meta.clone())], traces::START_TIME)
            .await
            .unwrap();
        assert_eq!(times, vec![100, 200, 300]);

        let batches = read_trace_anchors(&io, &meta, 2000).await.unwrap();
        let total: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total, 2, "only traces with last_ingest_time >= 2000");
        assert_eq!(batches[0].schema().len(), 4);
    }
}
