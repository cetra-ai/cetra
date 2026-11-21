use std::sync::Arc;

use arrow::array::{BooleanArray, FixedSizeBinaryArray, Scalar};
use arrow::compute::{filter_record_batch, kernels::cmp::eq};
use arrow::array::RecordBatch;
use futures::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt};
use object_store::path::Path;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ArrowReaderOptions, RowSelection, RowSelector};
use parquet::arrow::async_reader::{ParquetObjectReader, ParquetRecordBatchStreamBuilder};
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaData};
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::statistics::Statistics;

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct PqFile {
    pub store: Arc<dyn ObjectStore>,
    pub path: Path,
    pub meta: ArrowReaderMetadata,
    file_size: u64,
    trace_root: usize,
    trace_leaf: usize,
}

/// Loads footer + page index once. Costs a HEAD plus 1-2 ranged reads.
pub async fn open(store: Arc<dyn ObjectStore>, path: Path) -> R<PqFile> {
    let file_size = store.head(&path).await?.size;
    let mut reader = ParquetObjectReader::new(store.clone(), path.clone()).with_file_size(file_size);
    let meta = ArrowReaderMetadata::load_async(&mut reader, ArrowReaderOptions::new().with_page_index_policy(PageIndexPolicy::Required)).await?;
    let trace_root = meta.schema().index_of("trace_id")?;
    let trace_leaf = meta
        .parquet_schema()
        .columns()
        .iter()
        .position(|c| c.path().string() == "trace_id")
        .ok_or("no trace_id column")?;
    Ok(PqFile { store, path, meta, file_size, trace_root, trace_leaf })
}

/// Row groups whose trace_id min/max may contain `key`, each with the row ranges of the
/// pages that may contain it (None = whole row group: no usable column/offset index).
fn candidates(md: &ParquetMetaData, leaf: usize, key: &[u8; 16]) -> Vec<(usize, Option<Vec<(usize, usize)>>)> {
    let mut out = Vec::new();
    for rg in 0..md.num_row_groups() {
        let rgm = md.row_group(rg);
        if let Some(Statistics::FixedLenByteArray(s)) = rgm.column(leaf).statistics() {
            if let (Some(min), Some(max)) = (s.min_bytes_opt(), s.max_bytes_opt()) {
                if key.as_slice() < min || key.as_slice() > max {
                    continue;
                }
            }
        }
        let nrows = rgm.num_rows() as usize;
        let pages = (|| {
            let ci = md.column_index()?.get(rg)?.get(leaf)?;
            let oi = md.offset_index()?.get(rg)?.get(leaf)?;
            let ColumnIndexMetaData::FIXED_LEN_BYTE_ARRAY(ci) = ci else { return None };
            let locs = oi.page_locations();
            if locs.len() as u64 != ci.num_pages() {
                return None;
            }
            let mut v = Vec::new();
            for (p, loc) in locs.iter().enumerate() {
                let start = loc.first_row_index as usize;
                let end = locs.get(p + 1).map_or(nrows, |n| n.first_row_index as usize);
                let hit = match (ci.min_value(p), ci.max_value(p)) {
                    (Some(min), Some(max)) => key.as_slice() >= min && key.as_slice() <= max,
                    _ => false, // all-null page
                };
                if hit {
                    v.push((start, end));
                }
            }
            Some(v)
        })();
        out.push((rg, pages));
    }
    out
}

pub async fn lookup(f: &PqFile, key: &[u8; 16], projection: &[usize]) -> R<Vec<RecordBatch>> {
    let md = f.meta.metadata();
    let cands = candidates(md, f.trace_leaf, key);
    // Row groups with a page list that is empty contribute nothing.
    let mut rgs = Vec::new();
    let mut selectors: Vec<RowSelector> = Vec::new();
    for (rg, pages) in cands {
        let nrows = md.row_group(rg).num_rows() as usize;
        match pages {
            Some(p) if p.is_empty() => continue,
            Some(p) => {
                let mut pos = 0;
                for (s, e) in p {
                    if s > pos {
                        selectors.push(RowSelector::skip(s - pos));
                    }
                    selectors.push(RowSelector::select(e - s));
                    pos = e;
                }
                if pos < nrows {
                    selectors.push(RowSelector::skip(nrows - pos));
                }
            }
            None => selectors.push(RowSelector::select(nrows)),
        }
        rgs.push(rg);
    }
    if rgs.is_empty() {
        return Ok(vec![]);
    }
    let want_trace = projection.contains(&f.trace_root);
    let mut roots: Vec<usize> = projection.to_vec();
    if !want_trace {
        roots.push(f.trace_root);
    }
    let reader = ParquetObjectReader::new(f.store.clone(), f.path.clone()).with_file_size(f.file_size);
    let mask = ProjectionMask::roots(f.meta.parquet_schema(), roots);
    let stream = ParquetRecordBatchStreamBuilder::new_with_metadata(reader, f.meta.clone())
        .with_row_groups(rgs)
        .with_row_selection(RowSelection::from(selectors))
        .with_projection(mask)
        .build()?;
    let batches: Vec<RecordBatch> = stream.try_collect().await?;
    let key_arr = Scalar::new(FixedSizeBinaryArray::try_from_iter(std::iter::once(key.as_slice()))?);
    let mut out = Vec::new();
    for b in batches {
        let ti = b.schema().index_of("trace_id")?;
        let m: BooleanArray = eq(b.column(ti), &key_arr)?;
        let mut fb = filter_record_batch(&b, &m)?;
        if fb.num_rows() == 0 {
            continue;
        }
        if !want_trace {
            let keep: Vec<usize> = (0..fb.num_columns()).filter(|i| *i != ti).collect();
            fb = fb.project(&keep)?;
        }
        out.push(fb);
    }
    Ok(out)
}
