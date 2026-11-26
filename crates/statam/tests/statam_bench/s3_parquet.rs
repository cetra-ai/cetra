use std::io::{Read, Result as IoResult};
use std::ops::Range;
use std::sync::Arc;

use arrow::array::{
    Array, BooleanArray, FixedSizeBinaryArray, RecordBatch, StringArray, TimestampNanosecondArray,
};
use cetra_statam::{RangeReader, S3_CACHE_BLOCK_BYTES, SimulatedS3Reader};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{
    ArrowPredicateFn, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowFilter, RowSelection,
    RowSelector,
};
use parquet::bloom_filter::Sbbf;
use parquet::errors::ParquetError;
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaData, RowGroupMetaData};
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::reader::{ChunkReader, Length};
use parquet::file::statistics::Statistics;
use parquet::schema::types::SchemaDescriptor;

use crate::schema::BenchResult;

/// Wraps [`SimulatedS3Reader`] for [`parquet::file::reader::ChunkReader`], ensuring Parquet
/// footer/index/bloom/page reads go through the exact same 1 MiB `ReadCache` and S3 cost model.
#[derive(Clone)]
pub(crate) struct SimS3ChunkReader {
    inner: Arc<SimulatedS3Reader>,
}

impl SimS3ChunkReader {
    pub(crate) fn new(inner: Arc<SimulatedS3Reader>) -> Self {
        Self { inner }
    }

    pub(crate) fn s3(&self) -> &SimulatedS3Reader {
        &self.inner
    }
}

impl Length for SimS3ChunkReader {
    fn len(&self) -> u64 {
        self.inner.file_len()
    }
}

pub(crate) struct SimS3Read {
    inner: Arc<SimulatedS3Reader>,
    pos: u64,
    file_len: u64,
    buf: bytes::Bytes,
    buf_pos: usize,
}

impl Read for SimS3Read {
    fn read(&mut self, dst: &mut [u8]) -> IoResult<usize> {
        if dst.is_empty() || self.pos >= self.file_len {
            return Ok(0);
        }
        if self.buf_pos >= self.buf.len() {
            let block_end = ((self.pos / S3_CACHE_BLOCK_BYTES) + 1) * S3_CACHE_BLOCK_BYTES;
            let wanted_end = self
                .pos
                .saturating_add(dst.len() as u64)
                .max(block_end)
                .min(self.file_len);
            self.buf = self
                .inner
                .read_range(self.pos..wanted_end)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            self.buf_pos = 0;
            if self.buf.is_empty() {
                return Ok(0);
            }
        }
        let avail = self.buf.len() - self.buf_pos;
        let take = avail.min(dst.len());
        dst[..take].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + take]);
        self.buf_pos += take;
        self.pos += take as u64;
        Ok(take)
    }
}

impl ChunkReader for SimS3ChunkReader {
    type T = SimS3Read;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        let file_len = self.inner.file_len();
        if start > file_len {
            return Err(ParquetError::EOF(format!(
                "start offset {start} past file_len {file_len}"
            )));
        }
        Ok(SimS3Read {
            inner: Arc::clone(&self.inner),
            pos: start,
            file_len,
            buf: bytes::Bytes::new(),
            buf_pos: 0,
        })
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<bytes::Bytes> {
        let end = start.saturating_add(length as u64);
        self.inner
            .read_range(start..end)
            .map_err(|e| ParquetError::General(e.to_string()))
    }
}

#[derive(Clone)]
pub(crate) enum PqFilter {
    EqBytes { col: String, val: Vec<u8> },
    EqStr { col: String, val: String },
    I64Between { col: String, lo: i64, hi: i64 },
}

pub(crate) fn open_parquet_builder(
    reader: &SimS3ChunkReader,
    with_page_index: bool,
) -> BenchResult<ParquetRecordBatchReaderBuilder<SimS3ChunkReader>> {
    let opts = ArrowReaderOptions::new().with_page_index_policy(if with_page_index {
        PageIndexPolicy::Optional
    } else {
        PageIndexPolicy::Skip
    });
    Ok(ParquetRecordBatchReaderBuilder::try_new_with_options(
        reader.clone(),
        opts,
    )?)
}

fn find_leaf_idx(descr: &SchemaDescriptor, name: &str) -> Option<usize> {
    descr
        .columns()
        .iter()
        .position(|c| c.path().parts().len() == 1 && c.path().parts()[0] == name)
}

fn rg_matches_filters(
    rg: &RowGroupMetaData,
    descr: &SchemaDescriptor,
    filters: &[PqFilter],
) -> bool {
    for f in filters {
        let col_name = match f {
            PqFilter::EqBytes { col, .. }
            | PqFilter::EqStr { col, .. }
            | PqFilter::I64Between { col, .. } => col.as_str(),
        };
        let Some(leaf_idx) = find_leaf_idx(descr, col_name) else {
            continue;
        };
        let Some(stats) = rg.column(leaf_idx).statistics() else {
            continue;
        };
        let num_rows = u64::try_from(rg.num_rows()).unwrap_or(0);
        if stats.null_count_opt() == Some(num_rows) {
            return false;
        }
        match (f, stats) {
            (PqFilter::EqBytes { val, .. }, Statistics::FixedLenByteArray(s)) => {
                if let (Some(min), Some(max)) = (s.min_bytes_opt(), s.max_bytes_opt()) {
                    if val.as_slice() < min || val.as_slice() > max {
                        return false;
                    }
                }
            }
            (PqFilter::EqBytes { .. }, Statistics::ByteArray(s))
            | (PqFilter::EqStr { .. }, Statistics::ByteArray(s)) => {
                let target = match f {
                    PqFilter::EqBytes { val, .. } => val.as_slice(),
                    PqFilter::EqStr { val, .. } => val.as_bytes(),
                    _ => &[],
                };
                if let (Some(min), Some(max)) = (s.min_bytes_opt(), s.max_bytes_opt()) {
                    if target < min || target > max {
                        return false;
                    }
                }
            }
            (PqFilter::I64Between { lo, hi, .. }, Statistics::Int64(s)) => {
                if let (Some(&min), Some(&max)) = (s.min_opt(), s.max_opt()) {
                    if max < *lo || min > *hi {
                        return false;
                    }
                }
            }
            _ => {}
        }
    }
    true
}

fn page_matches_filters(col_idx: &ColumnIndexMetaData, page_idx: usize, filter: &PqFilter) -> bool {
    if col_idx.is_null_page(page_idx) {
        return false;
    }
    match (filter, col_idx) {
        (PqFilter::EqBytes { val, .. }, ColumnIndexMetaData::FIXED_LEN_BYTE_ARRAY(idx)) => {
            if let (Some(min), Some(max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return val.as_slice() >= min && val.as_slice() <= max;
            }
        }
        (
            PqFilter::EqBytes { .. } | PqFilter::EqStr { .. },
            ColumnIndexMetaData::BYTE_ARRAY(idx),
        ) => {
            let target = match filter {
                PqFilter::EqBytes { val, .. } => val.as_slice(),
                PqFilter::EqStr { val, .. } => val.as_bytes(),
                _ => &[],
            };
            if let (Some(min), Some(max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return target >= min && target <= max;
            }
        }
        (PqFilter::I64Between { lo, hi, .. }, ColumnIndexMetaData::INT64(idx)) => {
            if let (Some(&min), Some(&max)) = (idx.min_value(page_idx), idx.max_value(page_idx)) {
                return max >= *lo && min <= *hi;
            }
        }
        _ => {}
    }
    true
}

fn prune_rg_pages(
    meta: &ParquetMetaData,
    rg_idx: usize,
    filters: &[PqFilter],
) -> Vec<Range<usize>> {
    let rg = meta.row_group(rg_idx);
    let num_rows = usize::try_from(rg.num_rows()).unwrap_or(0);
    if num_rows == 0 {
        return Vec::new();
    }
    let mut kept = vec![0..num_rows];
    let (Some(col_indexes), Some(off_indexes)) = (meta.column_index(), meta.offset_index()) else {
        return kept;
    };
    let (Some(rg_ci), Some(rg_oi)) = (col_indexes.get(rg_idx), off_indexes.get(rg_idx)) else {
        return kept;
    };
    let descr = rg.schema_descr();

    for f in filters {
        let col_name = match f {
            PqFilter::EqBytes { col, .. }
            | PqFilter::EqStr { col, .. }
            | PqFilter::I64Between { col, .. } => col.as_str(),
        };
        let Some(leaf_idx) = find_leaf_idx(descr, col_name) else {
            continue;
        };
        let (Some(ci), Some(oi)) = (rg_ci.get(leaf_idx), rg_oi.get(leaf_idx)) else {
            continue;
        };
        let locs = oi.page_locations();
        if locs.is_empty() || ci.num_pages() as usize != locs.len() {
            continue;
        }
        let mut col_kept: Vec<Range<usize>> = Vec::new();
        for (p_idx, loc) in locs.iter().enumerate() {
            let start = usize::try_from(loc.first_row_index).unwrap_or(0);
            let end = locs.get(p_idx + 1).map_or(num_rows, |nxt| {
                usize::try_from(nxt.first_row_index).unwrap_or(num_rows)
            });
            if start < end && page_matches_filters(ci, p_idx, f) {
                if let Some(last) = col_kept.last_mut()
                    && last.end == start
                {
                    last.end = end;
                } else {
                    col_kept.push(start..end);
                }
            }
        }
        kept = intersect_ranges(&kept, &col_kept);
        if kept.is_empty() {
            return Vec::new();
        }
    }
    kept
}

fn intersect_ranges(a: &[Range<usize>], b: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let lo = a[i].start.max(b[j].start);
        let hi = a[i].end.min(b[j].end);
        if lo < hi {
            if let Some(last) = out.last_mut()
                && last.end == lo
            {
                last.end = hi;
            } else {
                out.push(lo..hi);
            }
        }
        if a[i].end < b[j].end {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// Checks RG min/max + SBBF bloom filters and returns surviving `(rg_idx, kept_ranges)`.
pub(crate) fn prune_parquet_rgs_and_pages(
    reader: &SimS3ChunkReader,
    meta: &ParquetMetaData,
    filters: &[PqFilter],
) -> BenchResult<Vec<(usize, Vec<Range<usize>>)>> {
    let descr = meta.file_metadata().schema_descr();
    let mut surviving = Vec::new();
    for rg_idx in 0..meta.num_row_groups() {
        let rg = meta.row_group(rg_idx);
        if !rg_matches_filters(rg, descr, filters) {
            continue;
        }
        let mut bloom_ok = true;
        for f in filters {
            let (col_name, key): (&str, &[u8]) = match f {
                PqFilter::EqBytes { col, val } => (col.as_str(), val.as_slice()),
                PqFilter::EqStr { col, val } => (col.as_str(), val.as_bytes()),
                PqFilter::I64Between { .. } => continue,
            };
            if let Some(leaf_idx) = find_leaf_idx(descr, col_name) {
                let chunk = rg.column(leaf_idx);
                if let Some(sbbf) = Sbbf::read_from_column_chunk(chunk, reader)?
                    && !sbbf.check(key)
                {
                    bloom_ok = false;
                    break;
                }
            }
        }
        if !bloom_ok {
            continue;
        }
        let kept = prune_rg_pages(meta, rg_idx, filters);
        if !kept.is_empty() {
            surviving.push((rg_idx, kept));
        }
    }
    Ok(surviving)
}

pub(crate) fn execute_parquet_scan(
    reader: &SimS3ChunkReader,
    with_page_index: bool,
    proj_roots: &[usize],
    filters: &[PqFilter],
    batch_size: usize,
) -> BenchResult<Vec<RecordBatch>> {
    let mut builder = open_parquet_builder(reader, with_page_index)?;
    let meta = Arc::clone(builder.metadata());
    let parquet_schema = meta.file_metadata().schema_descr_ptr();

    let surviving = prune_parquet_rgs_and_pages(reader, &meta, filters)?;
    if surviving.is_empty() {
        return Ok(Vec::new());
    }

    let rgs: Vec<usize> = surviving.iter().map(|(rg, _)| *rg).collect();
    let mut all_full = true;
    let mut selectors = Vec::new();
    for (rg_idx, kept) in &surviving {
        let num_rows = usize::try_from(meta.row_group(*rg_idx).num_rows()).unwrap_or(0);
        if kept.len() != 1 || kept[0] != (0..num_rows) {
            all_full = false;
        }
        let mut cursor = 0usize;
        for r in kept {
            if r.start > cursor {
                selectors.push(RowSelector::skip(r.start - cursor));
            }
            selectors.push(RowSelector::select(r.end - r.start));
            cursor = r.end;
        }
        if cursor < num_rows {
            selectors.push(RowSelector::skip(num_rows - cursor));
        }
    }

    builder = builder
        .with_row_groups(rgs)
        .with_projection(ProjectionMask::roots(
            &parquet_schema,
            proj_roots.iter().copied(),
        ))
        .with_batch_size(batch_size);

    if !all_full {
        builder = builder.with_row_selection(RowSelection::from(selectors));
    }

    let mut row_preds: Vec<Box<dyn parquet::arrow::arrow_reader::ArrowPredicate>> = Vec::new();
    for f in filters {
        match f {
            PqFilter::EqBytes { col, val } => {
                if let Some(root_idx) = parquet_schema
                    .root_schema()
                    .get_fields()
                    .iter()
                    .position(|fld| fld.name() == col)
                {
                    let target = val.clone();
                    let mask = ProjectionMask::roots(&parquet_schema, [root_idx]);
                    let pred = ArrowPredicateFn::new(mask, move |batch| {
                        let arr = batch.column(0);
                        let fsb = arr
                            .as_any()
                            .downcast_ref::<FixedSizeBinaryArray>()
                            .ok_or_else(|| {
                                arrow::error::ArrowError::InvalidArgumentError(
                                    "expected FixedSizeBinaryArray".to_owned(),
                                )
                            })?;
                        let bools: BooleanArray = (0..fsb.len())
                            .map(|i| (!fsb.is_null(i)).then(|| fsb.value(i) == target.as_slice()))
                            .collect();
                        Ok(bools)
                    });
                    row_preds.push(Box::new(pred));
                }
            }
            PqFilter::EqStr { col, val } => {
                if let Some(root_idx) = parquet_schema
                    .root_schema()
                    .get_fields()
                    .iter()
                    .position(|fld| fld.name() == col)
                {
                    let target = val.clone();
                    let mask = ProjectionMask::roots(&parquet_schema, [root_idx]);
                    let pred = ArrowPredicateFn::new(mask, move |batch| {
                        let arr = batch.column(0);
                        let sa = arr.as_any().downcast_ref::<StringArray>().ok_or_else(|| {
                            arrow::error::ArrowError::InvalidArgumentError(
                                "expected StringArray".to_owned(),
                            )
                        })?;
                        let bools: BooleanArray = (0..sa.len())
                            .map(|i| (!sa.is_null(i)).then(|| sa.value(i) == target.as_str()))
                            .collect();
                        Ok(bools)
                    });
                    row_preds.push(Box::new(pred));
                }
            }
            PqFilter::I64Between { col, lo, hi } => {
                if let Some(root_idx) = parquet_schema
                    .root_schema()
                    .get_fields()
                    .iter()
                    .position(|fld| fld.name() == col)
                {
                    let (lo_v, hi_v) = (*lo, *hi);
                    let mask = ProjectionMask::roots(&parquet_schema, [root_idx]);
                    let pred = ArrowPredicateFn::new(mask, move |batch| {
                        let arr = batch.column(0);
                        let ta = arr
                            .as_any()
                            .downcast_ref::<TimestampNanosecondArray>()
                            .ok_or_else(|| {
                                arrow::error::ArrowError::InvalidArgumentError(
                                    "expected TimestampNanosecondArray".to_owned(),
                                )
                            })?;
                        let bools: BooleanArray = (0..ta.len())
                            .map(|i| {
                                (!ta.is_null(i)).then(|| {
                                    let v = ta.value(i);
                                    v >= lo_v && v <= hi_v
                                })
                            })
                            .collect();
                        Ok(bools)
                    });
                    row_preds.push(Box::new(pred));
                }
            }
        }
    }

    if !row_preds.is_empty() {
        builder = builder.with_row_filter(RowFilter::new(row_preds));
    }

    let reader_iter = builder.build()?;
    let mut batches = Vec::new();
    for b in reader_iter {
        let b = b?;
        if b.num_rows() > 0 {
            batches.push(b);
        }
    }
    Ok(batches)
}
