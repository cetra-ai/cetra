//! Zero-Arrow low-level Parquet `ScanSource` with metadata caching, bloom/min-max/page
//! pruning, late materialization, inline tombstones, `DictColumn` preservation, and
//! corrupt-file `Suspects` reporting.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use cetra_column::{
    Batch, BinaryData, Bitmap, CancelToken, Column, DataType, DictColumn, EventsData, Field,
    FlatColumn, FlatData, LinksData, MapData, Reservation, ScalarValue, Schema, Selection,
    Utf8Data,
};
use cetra_core::schema::{event_field, link_field, scores, spans, traces};
use cetra_query::exec::{ExecContext, IdLookupHint, PartitionStream, ScanSource, ScanSpec};
use cetra_query::{BinaryOp, EvalContext, Expr, QueryError, UnaryOp, covered_predicate};
use chrono::NaiveDate;
use object_store::ObjectStore;
use object_store::path::Path as ObjectPath;
use parquet::DecodeResult;
use parquet::basic::{ConvertedType, Encoding, LogicalType, Repetition};
use parquet::bloom_filter::Sbbf;
use parquet::column::page::{Page, PageMetadata, PageReader};
use parquet::column::reader::{ColumnReader, ColumnReaderImpl, get_column_reader};
use parquet::data_type::{
    BoolType, ByteArray, ByteArrayType, DoubleType, FixedLenByteArrayType, FloatType, Int32Type,
    Int64Type,
};
use parquet::errors::ParquetError;
use parquet::file::metadata::{
    PageIndexPolicy, ParquetMetaData, ParquetMetaDataPushDecoder, RowGroupMetaData,
};
use parquet::file::page_index::column_index::ColumnIndexMetaData;
use parquet::file::page_index::offset_index::PageLocation;
use parquet::file::properties::ReaderProperties;
use parquet::file::reader::{ChunkReader, Length};
use parquet::file::serialized_reader::SerializedPageReader;
use parquet::file::statistics::Statistics;
use parquet::schema::types::{ColumnDescPtr, SchemaDescriptor, Type as SchemaType};

use crate::catalog::{FileId, FileMeta, FileTable};
use crate::deletion::{PendingDeletion, applicable};
use crate::file_format::footer_size_from_tail;
use crate::unwind::guard_sync;
use crate::verify::Suspects;

use super::metadata_cache::MetadataCache;
use super::{
    DAY_NS, apply_tombstones_and_filters, day_of_ns, reconcile_res, rollback_res,
    to_tombstone_specs, tombstone_table,
};

const MAX_METADATA_DECODE_ROUNDS: usize = 4;
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(1);
const BLOOM_COLUMNS: &[&str] = &["trace_id", "session_id", "user_id", "score_id"];

/// Zero-Arrow low-level Parquet [`ScanSource`].
#[derive(Clone)]
pub(crate) struct ParquetSource {
    store: Arc<dyn ObjectStore>,
    table: FileTable,
    schema: Arc<Schema>,
    files: Vec<Arc<FileMeta>>,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<MetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
}

impl fmt::Debug for ParquetSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParquetSource")
            .field("table", &self.table.as_str())
            .field("files", &self.files.len())
            .field("deletions", &self.deletions.len())
            .field("schema_len", &self.schema.len())
            .finish_non_exhaustive()
    }
}

impl ParquetSource {
    /// Creates a [`ParquetSource`] over `files` stored in `store`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<dyn ObjectStore>,
        table: FileTable,
        schema: Arc<Schema>,
        files: Vec<Arc<FileMeta>>,
        deletions: Arc<[Arc<PendingDeletion>]>,
        metadata_cache: Arc<MetadataCache>,
        suspects: Suspects,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            store,
            table,
            schema,
            files,
            deletions,
            metadata_cache,
            suspects,
            runtime,
        }
    }

    fn prune_files(&self, spec: &ScanSpec) -> Vec<Arc<FileMeta>> {
        let min_ns = spec.time_range.min_ns;
        let max_ns = spec.time_range.max_ns;
        if let (Some(lo), Some(hi)) = (min_ns, max_ns)
            && lo > hi
        {
            return Vec::new();
        }
        if spec.id_lookups.iter().any(id_lookup_is_empty) {
            return Vec::new();
        }

        let (day_start, day_end) = file_day_bounds(self.table, min_ns, max_ns);

        // `around_ns` applies ONLY to `Spans`/`Traces` when `spec.id_lookups` contains a
        // `trace_id` `Fixed16` lookup and `time_range` is open (D154, `query/mod.rs:513`).
        let has_trace_lookup = spec.id_lookups.iter().any(|h| {
            matches!(
                h,
                IdLookupHint::Fixed16 { column, .. } if column == traces::TRACE_ID
            )
        });
        let around_window = if self.table != FileTable::Scores
            && has_trace_lookup
            && min_ns.is_none()
            && max_ns.is_none()
        {
            spec.time_range
                .around_ns
                .map(|t| (t.saturating_sub(DAY_NS), t.saturating_add(DAY_NS)))
        } else {
            None
        };

        self.files
            .iter()
            .filter(|file| {
                if file.rows == 0 {
                    return false;
                }
                if file.day < day_start || file.day > day_end {
                    return false;
                }
                if !file_overlaps(file, min_ns, max_ns) {
                    return false;
                }
                if let Some((lo, hi)) = around_window
                    && !file_overlaps(file, Some(lo), Some(hi))
                {
                    return false;
                }
                true
            })
            .cloned()
            .collect()
    }
}

impl ScanSource for ParquetSource {
    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
        match &spec.projection {
            Some(indices) => Ok(Arc::new(self.schema.project(indices)?)),
            None => Ok(Arc::clone(&self.schema)),
        }
    }

    fn open_partitions(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let _ = self.projected_schema(spec)?;
        let kept_files = self.prune_files(spec);
        if kept_files.is_empty() {
            return Ok(Vec::new());
        }
        self.suspects.record(&kept_files);

        let plan = Arc::new(ScanPlan::compile(self.table, &self.schema, spec)?);

        let groups = distribute_files(kept_files, ctx.target_partitions.max(1));
        let batch_size = ctx.batch_size.max(1);
        let streams = groups
            .into_iter()
            .map(|files| {
                Box::new(ParquetPartitionStream {
                    store: Arc::clone(&self.store),
                    table: self.table,
                    schema: Arc::clone(&self.schema),
                    files: files.into(),
                    deletions: Arc::clone(&self.deletions),
                    metadata_cache: Arc::clone(&self.metadata_cache),
                    suspects: self.suspects.clone(),
                    runtime: self.runtime.clone(),
                    plan: Arc::clone(&plan),
                    batch_size,
                    now_ns: ctx.eval.now_ns,
                    active_file: None,
                }) as Box<dyn PartitionStream>
            })
            .collect();
        Ok(streams)
    }

    fn explain_summary(&self, spec: &ScanSpec) -> String {
        format!(
            "ParquetSource(table={}, files={}, filters={})",
            self.table.as_str(),
            self.files.len(),
            spec.filters.len()
        )
    }
}

fn id_lookup_is_empty(hint: &IdLookupHint) -> bool {
    match hint {
        IdLookupHint::Fixed16 { values, .. } => values.is_empty(),
        IdLookupHint::Fixed8 { values, .. } => values.is_empty(),
        IdLookupHint::Utf8 { values, .. } => values.is_empty(),
    }
}

fn file_day_bounds(
    table: FileTable,
    min_ns: Option<i64>,
    max_ns: Option<i64>,
) -> (NaiveDate, NaiveDate) {
    let start = min_ns.and_then(day_of_ns).unwrap_or(NaiveDate::MIN);
    let end = max_ns.and_then(day_of_ns).unwrap_or(NaiveDate::MAX);
    if start > end {
        return (NaiveDate::MIN, NaiveDate::MAX);
    }
    match table {
        FileTable::Spans | FileTable::Traces => (
            start.pred_opt().unwrap_or(NaiveDate::MIN),
            end.succ_opt().unwrap_or(NaiveDate::MAX),
        ),
        FileTable::Scores => (start, end),
    }
}

fn file_overlaps(file: &FileMeta, min_ns: Option<i64>, max_ns: Option<i64>) -> bool {
    if let Some(min) = min_ns
        && file.max_time < min
    {
        return false;
    }
    if let Some(max) = max_ns
        && file.min_time > max
    {
        return false;
    }
    true
}

fn distribute_files(mut files: Vec<Arc<FileMeta>>, target: usize) -> Vec<Vec<Arc<FileMeta>>> {
    let slots = target.max(1).min(files.len().max(1));
    files.sort_by(|left, right| {
        right
            .size
            .cmp(&left.size)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut groups: Vec<(u64, Vec<Arc<FileMeta>>)> = (0..slots).map(|_| (0, Vec::new())).collect();
    for file in files {
        let mut best = 0;
        for (idx, (sz, _)) in groups.iter().enumerate().skip(1) {
            if *sz < groups[best].0 {
                best = idx;
            }
        }
        groups[best].0 = groups[best].0.saturating_add(file.size);
        groups[best].1.push(file);
    }
    groups
        .into_iter()
        .filter(|(_, g)| !g.is_empty())
        .map(|(_, g)| g)
        .collect()
}

fn table_time_column(table: FileTable) -> &'static str {
    match table {
        FileTable::Spans => spans::START_TIME,
        FileTable::Traces => traces::START_TIME,
        FileTable::Scores => scores::CREATED_AT,
    }
}

/// Precompiled projection, constraints, and bloom-filter hints shared by all partitions of a scan.
struct ScanPlan {
    projection: Option<Vec<usize>>,
    projected_schema: Arc<Schema>,
    output_cols: Vec<usize>,
    filter_cols: BTreeSet<usize>,
    filters: Vec<Expr>,
    constraints: BTreeMap<String, Vec<ColConstraint>>,
    bloom_lookups: Vec<BloomLookup>,
}

impl fmt::Debug for ScanPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScanPlan")
            .field("projection_len", &self.projection.as_ref().map(Vec::len))
            .field("projected_schema_len", &self.projected_schema.len())
            .field("output_cols_len", &self.output_cols.len())
            .field("filter_cols_len", &self.filter_cols.len())
            .field("filters_len", &self.filters.len())
            .field("constraints_len", &self.constraints.len())
            .field("bloom_lookups_len", &self.bloom_lookups.len())
            .finish()
    }
}

#[derive(Debug, Clone)]
enum ColConstraint {
    Range {
        min: Option<BoundValue>,
        max: Option<BoundValue>,
    },
    In(Vec<ConstraintValue>),
    IsNull,
    IsNotNull,
    Contradiction,
}

#[derive(Debug, Clone)]
struct BoundValue {
    value: ConstraintValue,
    inclusive: bool,
}

#[derive(Clone, PartialEq)]
enum ConstraintValue {
    Bool(bool),
    I64(i64),
    F64(f64),
    Bytes(Vec<u8>),
}

impl fmt::Debug for ConstraintValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool(_) => f.write_str("Bool(<redacted>)"),
            Self::I64(_) => f.write_str("I64(<redacted>)"),
            Self::F64(_) => f.write_str("F64(<redacted>)"),
            Self::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
        }
    }
}

#[derive(Clone)]
struct BloomLookup {
    column: String,
    keys: Vec<Vec<u8>>,
}

impl fmt::Debug for BloomLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BloomLookup")
            .field("column", &self.column)
            .field("keys_len", &self.keys.len())
            .finish()
    }
}

impl ScanPlan {
    fn compile(
        table: FileTable,
        schema: &Arc<Schema>,
        spec: &ScanSpec,
    ) -> Result<Self, QueryError> {
        let projected_schema = match &spec.projection {
            Some(indices) => Arc::new(schema.project(indices)?),
            None => Arc::clone(schema),
        };
        let output_cols: Vec<usize> = match &spec.projection {
            Some(indices) => {
                let mut set = BTreeSet::new();
                for &idx in indices {
                    if idx >= schema.len() {
                        return Err(cetra_column::ColumnError::InvalidBatch {
                            reason: "projection index out of bounds",
                        }
                        .into());
                    }
                    set.insert(idx);
                }
                set.into_iter().collect()
            }
            None => (0..schema.len()).collect(),
        };

        let mut filter_cols = BTreeSet::new();
        for filter in &spec.filters {
            filter.referenced_columns(&mut filter_cols);
        }

        let mut constraints: BTreeMap<String, Vec<ColConstraint>> = BTreeMap::new();
        let mut bloom_map: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();

        // Time range constraint on table's time column.
        let time_col = table_time_column(table);
        if spec.time_range.min_ns.is_some() || spec.time_range.max_ns.is_some() {
            constraints
                .entry(time_col.to_owned())
                .or_default()
                .push(ColConstraint::Range {
                    min: spec.time_range.min_ns.map(|v| BoundValue {
                        value: ConstraintValue::I64(v),
                        inclusive: true,
                    }),
                    max: spec.time_range.max_ns.map(|v| BoundValue {
                        value: ConstraintValue::I64(v),
                        inclusive: true,
                    }),
                });
        }

        // ID lookup hints.
        for hint in &spec.id_lookups {
            let (col, byte_vals): (&str, Vec<Vec<u8>>) = match hint {
                IdLookupHint::Fixed16 { column, values } => {
                    (column, values.iter().map(|v| v.to_vec()).collect())
                }
                IdLookupHint::Fixed8 { column, values } => {
                    (column, values.iter().map(|v| v.to_vec()).collect())
                }
                IdLookupHint::Utf8 { column, values } => (
                    column,
                    values.iter().map(|s| s.as_bytes().to_vec()).collect(),
                ),
            };
            if byte_vals.is_empty() {
                constraints
                    .entry(col.to_owned())
                    .or_default()
                    .push(ColConstraint::Contradiction);
            } else {
                let cvals = byte_vals
                    .iter()
                    .cloned()
                    .map(ConstraintValue::Bytes)
                    .collect();
                constraints
                    .entry(col.to_owned())
                    .or_default()
                    .push(ColConstraint::In(cvals));
                if BLOOM_COLUMNS.contains(&col) {
                    bloom_map
                        .entry(col.to_owned())
                        .or_default()
                        .extend(byte_vals);
                }
            }
        }

        // Conjunctive predicates from `spec.filters`.
        for filter in &spec.filters {
            extract_expr_constraints(filter, &mut constraints, &mut bloom_map);
        }

        let bloom_lookups = bloom_map
            .into_iter()
            .filter(|(_, keys)| !keys.is_empty())
            .map(|(column, keys)| BloomLookup { column, keys })
            .collect();

        Ok(Self {
            projection: spec.projection.clone(),
            projected_schema,
            output_cols,
            filter_cols,
            filters: spec.filters.clone(),
            constraints,
            bloom_lookups,
        })
    }
}

fn scalar_to_constraint(sv: &ScalarValue) -> Option<ConstraintValue> {
    match sv {
        ScalarValue::Bool(b) => Some(ConstraintValue::Bool(*b)),
        ScalarValue::I64(v) => Some(ConstraintValue::I64(*v)),
        ScalarValue::TimestampNs { value, .. } => Some(ConstraintValue::I64(*value)),
        ScalarValue::Date(d) => Some(ConstraintValue::I64(i64::from(*d))),
        ScalarValue::F64(f) => (!f.is_nan()).then_some(ConstraintValue::F64(*f)),
        ScalarValue::Utf8(s) => Some(ConstraintValue::Bytes(s.as_bytes().to_vec())),
        ScalarValue::Binary(b) => Some(ConstraintValue::Bytes(b.clone())),
        ScalarValue::FixedBytes8(b) => Some(ConstraintValue::Bytes(b.to_vec())),
        ScalarValue::FixedBytes16(b) => Some(ConstraintValue::Bytes(b.to_vec())),
        _ => None,
    }
}

fn flip_op(op: BinaryOp) -> Option<BinaryOp> {
    match op {
        BinaryOp::Eq => Some(BinaryOp::Eq),
        BinaryOp::Lt => Some(BinaryOp::Gt),
        BinaryOp::LtEq => Some(BinaryOp::GtEq),
        BinaryOp::Gt => Some(BinaryOp::Lt),
        BinaryOp::GtEq => Some(BinaryOp::LtEq),
        _ => None,
    }
}

fn extract_expr_constraints(
    expr: &Expr,
    constraints: &mut BTreeMap<String, Vec<ColConstraint>>,
    bloom_map: &mut BTreeMap<String, Vec<Vec<u8>>>,
) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            extract_expr_constraints(left, constraints, bloom_map);
            extract_expr_constraints(right, constraints, bloom_map);
        }
        Expr::Binary {
            left, op, right, ..
        } => {
            let (col_name, cmp_op, lit_val) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column { name, .. }, Expr::Literal(sv)) => {
                    (name.as_str(), *op, scalar_to_constraint(sv))
                }
                (Expr::Literal(sv), Expr::Column { name, .. }) => {
                    let Some(flipped) = flip_op(*op) else {
                        return;
                    };
                    (name.as_str(), flipped, scalar_to_constraint(sv))
                }
                _ => return,
            };
            let Some(val) = lit_val else {
                return;
            };
            match cmp_op {
                BinaryOp::Eq => {
                    if let ConstraintValue::Bytes(b) = &val
                        && BLOOM_COLUMNS.contains(&col_name)
                    {
                        bloom_map
                            .entry(col_name.to_owned())
                            .or_default()
                            .push(b.clone());
                    }
                    constraints
                        .entry(col_name.to_owned())
                        .or_default()
                        .push(ColConstraint::In(vec![val]));
                }
                BinaryOp::Gt => {
                    constraints.entry(col_name.to_owned()).or_default().push(
                        ColConstraint::Range {
                            min: Some(BoundValue {
                                value: val,
                                inclusive: false,
                            }),
                            max: None,
                        },
                    );
                }
                BinaryOp::GtEq => {
                    constraints.entry(col_name.to_owned()).or_default().push(
                        ColConstraint::Range {
                            min: Some(BoundValue {
                                value: val,
                                inclusive: true,
                            }),
                            max: None,
                        },
                    );
                }
                BinaryOp::Lt => {
                    constraints.entry(col_name.to_owned()).or_default().push(
                        ColConstraint::Range {
                            min: None,
                            max: Some(BoundValue {
                                value: val,
                                inclusive: false,
                            }),
                        },
                    );
                }
                BinaryOp::LtEq => {
                    constraints.entry(col_name.to_owned()).or_default().push(
                        ColConstraint::Range {
                            min: None,
                            max: Some(BoundValue {
                                value: val,
                                inclusive: true,
                            }),
                        },
                    );
                }
                _ => {}
            }
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            let Expr::Column { name, .. } = expr.as_ref() else {
                return;
            };
            let min = match low.as_ref() {
                Expr::Literal(sv) => scalar_to_constraint(sv).map(|v| BoundValue {
                    value: v,
                    inclusive: true,
                }),
                _ => None,
            };
            let max = match high.as_ref() {
                Expr::Literal(sv) => scalar_to_constraint(sv).map(|v| BoundValue {
                    value: v,
                    inclusive: true,
                }),
                _ => None,
            };
            if min.is_some() || max.is_some() {
                constraints
                    .entry(name.clone())
                    .or_default()
                    .push(ColConstraint::Range { min, max });
            }
        }
        Expr::InList {
            expr,
            list,
            negated: false,
            ..
        } => {
            let Expr::Column { name, .. } = expr.as_ref() else {
                return;
            };
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                let Expr::Literal(sv) = item else {
                    return;
                };
                if sv.is_null() {
                    continue;
                }
                let Some(cv) = scalar_to_constraint(sv) else {
                    return;
                };
                vals.push(cv);
            }
            if vals.is_empty() {
                constraints
                    .entry(name.clone())
                    .or_default()
                    .push(ColConstraint::Contradiction);
            } else {
                if BLOOM_COLUMNS.contains(&name.as_str()) {
                    let bytes: Vec<Vec<u8>> = vals
                        .iter()
                        .filter_map(|v| match v {
                            ConstraintValue::Bytes(b) => Some(b.clone()),
                            _ => None,
                        })
                        .collect();
                    if bytes.len() == vals.len() {
                        bloom_map.entry(name.clone()).or_default().extend(bytes);
                    }
                }
                constraints
                    .entry(name.clone())
                    .or_default()
                    .push(ColConstraint::In(vals));
            }
        }
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr,
            ..
        } => {
            if let Expr::Column { name, .. } = expr.as_ref() {
                constraints
                    .entry(name.clone())
                    .or_default()
                    .push(ColConstraint::IsNotNull);
            }
        }
        Expr::Unary {
            op: UnaryOp::IsNull,
            expr,
            ..
        } => {
            if let Expr::Column { name, .. } = expr.as_ref() {
                constraints
                    .entry(name.clone())
                    .or_default()
                    .push(ColConstraint::IsNull);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Cancellation-aware I/O bridge + metadata loading
// ---------------------------------------------------------------------------

enum FetchError {
    Cancelled,
    Corrupt,
    Store,
    Runtime,
}

fn fetch_ranges(
    store: &Arc<dyn ObjectStore>,
    path: &ObjectPath,
    ranges: &[Range<u64>],
    cancel: &CancelToken,
    runtime: &tokio::runtime::Handle,
) -> Result<Vec<Bytes>, FetchError> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    if cancel.is_cancelled() {
        return Err(FetchError::Cancelled);
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(FetchError::Runtime);
    }

    let store = Arc::clone(store);
    let path = path.clone();
    let ranges_owned = ranges.to_vec();
    let cancel_token = cancel.clone();

    let caught = catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(async move {
            let fut = store.get_ranges(&path, &ranges_owned);
            tokio::pin!(fut);
            let mut interval = tokio::time::interval(CANCEL_POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = interval.tick() => {
                        if cancel_token.is_cancelled() {
                            return Err(FetchError::Cancelled);
                        }
                    }
                    res = &mut fut => {
                        return res.map_err(|err| match err {
                            object_store::Error::NotFound { .. } => FetchError::Corrupt,
                            _ => FetchError::Store,
                        });
                    }
                }
            }
        })
    }));

    match caught {
        Ok(res) => res,
        Err(_) => Err(FetchError::Corrupt),
    }
}

fn map_fetch_err(err: &FetchError, file_id: FileId, suspects: &Suspects) -> QueryError {
    match err {
        FetchError::Cancelled => QueryError::Cancelled,
        FetchError::Corrupt => {
            suspects.report_corrupt(file_id);
            QueryError::internal("corrupt parquet file")
        }
        FetchError::Store => QueryError::internal("storage read error"),
        FetchError::Runtime => {
            QueryError::internal("cannot block on parquet I/O inside tokio runtime")
        }
    }
}

fn corrupt_err(file_id: FileId, suspects: &Suspects) -> QueryError {
    suspects.report_corrupt(file_id);
    QueryError::internal("corrupt parquet file")
}

fn load_metadata(
    store: &Arc<dyn ObjectStore>,
    file: &FileMeta,
    cache: &MetadataCache,
    suspects: &Suspects,
    cancel: &CancelToken,
    runtime: &tokio::runtime::Handle,
) -> Result<Arc<ParquetMetaData>, QueryError> {
    if let Some(meta) = cache.get(file.id) {
        return Ok(meta);
    }
    if file.size < 8 {
        return Err(corrupt_err(file.id, suspects));
    }
    let path = ObjectPath::from(file.path.as_str());
    let tail_len = file.size.min(file.footer_size.max(8));
    let tail_range = (file.size - tail_len)..file.size;
    let tail_bufs = fetch_ranges(
        store,
        &path,
        std::slice::from_ref(&tail_range),
        cancel,
        runtime,
    )
    .map_err(|e| map_fetch_err(&e, file.id, suspects))?;
    let [tail_bytes]: [Bytes; 1] = tail_bufs
        .try_into()
        .map_err(|_| corrupt_err(file.id, suspects))?;
    if tail_bytes.len() as u64 != tail_len || tail_bytes.len() < 8 {
        return Err(corrupt_err(file.id, suspects));
    }
    let tail_8: [u8; 8] = tail_bytes[tail_bytes.len() - 8..]
        .try_into()
        .map_err(|_| corrupt_err(file.id, suspects))?;
    if footer_size_from_tail(&tail_8, file.size).is_err() {
        return Err(corrupt_err(file.id, suspects));
    }

    let mut decoder = guard_sync(|| {
        let mut dec = ParquetMetaDataPushDecoder::try_new(file.size)?
            .with_page_index_policy(PageIndexPolicy::Optional);
        dec.push_range(tail_range, tail_bytes)?;
        Ok(dec)
    })
    .map_err(|_| corrupt_err(file.id, suspects))?;

    for _ in 0..MAX_METADATA_DECODE_ROUNDS {
        let step =
            guard_sync(|| decoder.try_decode()).map_err(|_| corrupt_err(file.id, suspects))?;
        match step {
            DecodeResult::Data(meta) => {
                let meta = Arc::new(meta);
                cache.insert(file.id, Arc::clone(&meta));
                return Ok(meta);
            }
            DecodeResult::Finished => {
                return Err(corrupt_err(file.id, suspects));
            }
            DecodeResult::NeedsData(ranges) => {
                for r in &ranges {
                    if r.start > r.end || r.end > file.size {
                        return Err(corrupt_err(file.id, suspects));
                    }
                }
                let bufs = fetch_ranges(store, &path, &ranges, cancel, runtime)
                    .map_err(|e| map_fetch_err(&e, file.id, suspects))?;
                if bufs.len() != ranges.len()
                    || bufs
                        .iter()
                        .zip(&ranges)
                        .any(|(b, r)| b.len() as u64 != r.end - r.start)
                {
                    return Err(corrupt_err(file.id, suspects));
                }
                guard_sync(|| decoder.push_ranges(ranges, bufs))
                    .map_err(|_| corrupt_err(file.id, suspects))?;
            }
        }
    }

    Err(corrupt_err(file.id, suspects))
}

// ---------------------------------------------------------------------------
// Row-group & page pruning
// ---------------------------------------------------------------------------

fn find_scalar_leaf(descr: &SchemaDescriptor, name: &str) -> Option<usize> {
    descr
        .columns()
        .iter()
        .position(|col| col.path().parts() == [name] && col.max_rep_level() == 0)
}

fn unsigned_int32(col: &ColumnDescPtr) -> Option<u8> {
    match col.logical_type_ref() {
        Some(LogicalType::Integer(int)) if !int.is_signed => Some(int.bit_width as u8),
        _ => match col.converted_type() {
            ConvertedType::UINT_8 => Some(8),
            ConvertedType::UINT_16 => Some(16),
            ConvertedType::UINT_32 => Some(32),
            _ => None,
        },
    }
}

fn widen_int32(col: &ColumnDescPtr, v: i32) -> i64 {
    match unsigned_int32(col) {
        Some(8) => i64::from(v as u8),
        Some(16) => i64::from(v as u16),
        Some(32) => i64::from(v as u32),
        _ => i64::from(v),
    }
}

fn min_max_satisfies_constraints(
    min: Option<&ConstraintValue>,
    max: Option<&ConstraintValue>,
    all_null: bool,
    null_count: Option<u64>,
    constraints: &[ColConstraint],
) -> bool {
    for c in constraints {
        match c {
            ColConstraint::Contradiction => return false,
            ColConstraint::IsNotNull => {
                if all_null {
                    return false;
                }
            }
            ColConstraint::IsNull => {
                if null_count == Some(0) {
                    return false;
                }
            }
            ColConstraint::In(vals) => {
                if all_null || vals.is_empty() {
                    return false;
                }
                let any_hit = vals.iter().any(|v| value_in_bounds(v, min, max));
                if !any_hit {
                    return false;
                }
            }
            ColConstraint::Range {
                min: q_min,
                max: q_max,
            } => {
                if all_null {
                    return false;
                }
                if let (Some(q_lo), Some(d_hi)) = (q_min, max)
                    && !bounds_overlap_lo(q_lo, d_hi)
                {
                    return false;
                }
                if let (Some(q_hi), Some(d_lo)) = (q_max, min)
                    && !bounds_overlap_hi(q_hi, d_lo)
                {
                    return false;
                }
            }
        }
    }
    true
}

fn cmp_constraint(a: &ConstraintValue, b: &ConstraintValue) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (ConstraintValue::Bool(x), ConstraintValue::Bool(y)) => Some(x.cmp(y)),
        (ConstraintValue::I64(x), ConstraintValue::I64(y)) => Some(x.cmp(y)),
        (ConstraintValue::F64(x), ConstraintValue::F64(y)) => {
            if x.is_nan() || y.is_nan() {
                None
            } else {
                x.partial_cmp(y)
            }
        }
        (ConstraintValue::Bytes(x), ConstraintValue::Bytes(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn value_in_bounds(
    v: &ConstraintValue,
    min: Option<&ConstraintValue>,
    max: Option<&ConstraintValue>,
) -> bool {
    if let Some(lo) = min
        && cmp_constraint(v, lo).is_some_and(|o| o == std::cmp::Ordering::Less)
    {
        return false;
    }
    if let Some(hi) = max
        && cmp_constraint(v, hi).is_some_and(|o| o == std::cmp::Ordering::Greater)
    {
        return false;
    }
    true
}

fn bounds_overlap_lo(q_lo: &BoundValue, data_hi: &ConstraintValue) -> bool {
    if matches!(q_lo.value, ConstraintValue::F64(_)) {
        // Parquet float statistics exclude NaN, whereas `cetra-query` orders `NaN > +inf`.
        return true;
    }
    let Some(ord) = cmp_constraint(data_hi, &q_lo.value) else {
        return true;
    };
    if q_lo.inclusive {
        ord != std::cmp::Ordering::Less
    } else {
        ord == std::cmp::Ordering::Greater
    }
}

fn bounds_overlap_hi(q_hi: &BoundValue, data_lo: &ConstraintValue) -> bool {
    let Some(ord) = cmp_constraint(data_lo, &q_hi.value) else {
        return true;
    };
    if q_hi.inclusive {
        ord != std::cmp::Ordering::Greater
    } else {
        ord == std::cmp::Ordering::Less
    }
}

fn stats_bounds(
    col: &ColumnDescPtr,
    stats: &Statistics,
) -> (Option<ConstraintValue>, Option<ConstraintValue>) {
    match stats {
        Statistics::Boolean(s) => (
            s.min_opt().copied().map(ConstraintValue::Bool),
            s.max_opt().copied().map(ConstraintValue::Bool),
        ),
        Statistics::Int32(s) => (
            s.min_opt()
                .map(|&v| ConstraintValue::I64(widen_int32(col, v))),
            s.max_opt()
                .map(|&v| ConstraintValue::I64(widen_int32(col, v))),
        ),
        Statistics::Int64(s) => (
            s.min_opt().copied().map(ConstraintValue::I64),
            s.max_opt().copied().map(ConstraintValue::I64),
        ),
        Statistics::Float(s) => (
            s.min_opt()
                .filter(|v| !v.is_nan())
                .map(|&v| ConstraintValue::F64(f64::from(v))),
            s.max_opt()
                .filter(|v| !v.is_nan())
                .map(|&v| ConstraintValue::F64(f64::from(v))),
        ),
        Statistics::Double(s) => (
            s.min_opt()
                .filter(|v| !v.is_nan())
                .copied()
                .map(ConstraintValue::F64),
            s.max_opt()
                .filter(|v| !v.is_nan())
                .copied()
                .map(ConstraintValue::F64),
        ),
        Statistics::ByteArray(s) => (
            s.min_opt()
                .map(|b| ConstraintValue::Bytes(b.data().to_vec())),
            s.max_opt()
                .map(|b| ConstraintValue::Bytes(b.data().to_vec())),
        ),
        Statistics::FixedLenByteArray(s) => (
            s.min_opt()
                .map(|b| ConstraintValue::Bytes(b.data().to_vec())),
            s.max_opt()
                .map(|b| ConstraintValue::Bytes(b.data().to_vec())),
        ),
        Statistics::Int96(_) => (None, None),
    }
}

fn rg_matches_statistics(rg: &RowGroupMetaData, plan: &ScanPlan) -> bool {
    let num_rows = u64::try_from(rg.num_rows()).unwrap_or(0);
    if num_rows == 0 {
        return false;
    }
    let descr = rg.schema_descr();
    for (col_name, constraints) in &plan.constraints {
        if constraints
            .iter()
            .any(|c| matches!(c, ColConstraint::Contradiction))
        {
            return false;
        }
        let Some(leaf_idx) = find_scalar_leaf(descr, col_name) else {
            continue;
        };
        let chunk = rg.column(leaf_idx);
        let Some(stats) = chunk.statistics() else {
            continue;
        };
        let null_count = stats.null_count_opt();
        let all_null = null_count == Some(num_rows);
        let (min, max) = stats_bounds(&descr.column(leaf_idx), stats);
        if !min_max_satisfies_constraints(
            min.as_ref(),
            max.as_ref(),
            all_null,
            null_count,
            constraints,
        ) {
            return false;
        }
    }
    true
}

fn column_index_page_matches(
    col: &ColumnDescPtr,
    col_idx: &ColumnIndexMetaData,
    page_idx: usize,
    constraints: &[ColConstraint],
) -> bool {
    let is_null_page = col_idx.is_null_page(page_idx);
    let null_count = col_idx
        .null_count(page_idx)
        .and_then(|c| u64::try_from(c).ok());
    let (min, max) = match col_idx {
        ColumnIndexMetaData::NONE => return true,
        ColumnIndexMetaData::BOOLEAN(idx) => (
            idx.min_value(page_idx).copied().map(ConstraintValue::Bool),
            idx.max_value(page_idx).copied().map(ConstraintValue::Bool),
        ),
        ColumnIndexMetaData::INT32(idx) => (
            idx.min_value(page_idx)
                .map(|&v| ConstraintValue::I64(widen_int32(col, v))),
            idx.max_value(page_idx)
                .map(|&v| ConstraintValue::I64(widen_int32(col, v))),
        ),
        ColumnIndexMetaData::INT64(idx) => (
            idx.min_value(page_idx).copied().map(ConstraintValue::I64),
            idx.max_value(page_idx).copied().map(ConstraintValue::I64),
        ),
        ColumnIndexMetaData::FLOAT(idx) => (
            idx.min_value(page_idx)
                .filter(|v| !v.is_nan())
                .map(|&v| ConstraintValue::F64(f64::from(v))),
            idx.max_value(page_idx)
                .filter(|v| !v.is_nan())
                .map(|&v| ConstraintValue::F64(f64::from(v))),
        ),
        ColumnIndexMetaData::DOUBLE(idx) => (
            idx.min_value(page_idx)
                .filter(|v| !v.is_nan())
                .copied()
                .map(ConstraintValue::F64),
            idx.max_value(page_idx)
                .filter(|v| !v.is_nan())
                .copied()
                .map(ConstraintValue::F64),
        ),
        ColumnIndexMetaData::BYTE_ARRAY(idx) | ColumnIndexMetaData::FIXED_LEN_BYTE_ARRAY(idx) => (
            idx.min_value(page_idx)
                .map(|b| ConstraintValue::Bytes(b.to_vec())),
            idx.max_value(page_idx)
                .map(|b| ConstraintValue::Bytes(b.to_vec())),
        ),
        ColumnIndexMetaData::INT96(_) => return true,
    };
    min_max_satisfies_constraints(
        min.as_ref(),
        max.as_ref(),
        is_null_page,
        null_count,
        constraints,
    )
}

fn checked_rg_num_rows(rg: &RowGroupMetaData) -> Result<usize, ParquetError> {
    let n = rg.num_rows();
    if n < 0 || n > i64::from(u32::MAX) {
        return Err(ParquetError::General(
            "row group row count out of range".to_owned(),
        ));
    }
    Ok(n as usize)
}

fn validate_page_locations(
    locs: &[PageLocation],
    num_rows: usize,
    file_size: u64,
) -> Result<(), ParquetError> {
    if let Some(first) = locs.first()
        && first.first_row_index != 0
    {
        return Err(ParquetError::General(
            "first page location does not start at row 0".to_owned(),
        ));
    }
    let mut prev_row = -1_i64;
    for loc in locs {
        if loc.offset < 0 || loc.compressed_page_size <= 0 || loc.first_row_index < 0 {
            return Err(ParquetError::General("negative page location".to_owned()));
        }
        if loc.first_row_index <= prev_row || (loc.first_row_index as usize) >= num_rows {
            return Err(ParquetError::General(
                "invalid page first_row_index".to_owned(),
            ));
        }
        prev_row = loc.first_row_index;
        let start = loc.offset as u64;
        let end = start
            .checked_add(loc.compressed_page_size as u64)
            .ok_or_else(|| ParquetError::General("page end overflow".to_owned()))?;
        if end > file_size {
            return Err(ParquetError::General(
                "page extends past file end".to_owned(),
            ));
        }
    }
    Ok(())
}

fn page_row_ranges(locs: &[PageLocation], num_rows: usize) -> Vec<Range<usize>> {
    let mut ranges = Vec::with_capacity(locs.len());
    for (idx, loc) in locs.iter().enumerate() {
        let start = loc.first_row_index as usize;
        let end = locs
            .get(idx + 1)
            .map_or(num_rows, |next| next.first_row_index as usize);
        ranges.push(start..end);
    }
    ranges
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

fn prune_pages_for_row_group(
    meta: &ParquetMetaData,
    rg_idx: usize,
    plan: &ScanPlan,
    file_size: u64,
) -> Result<Vec<Range<usize>>, ParquetError> {
    let rg = meta.row_group(rg_idx);
    let num_rows = checked_rg_num_rows(rg)?;
    if num_rows == 0 {
        return Ok(Vec::new());
    }
    let mut kept: Vec<Range<usize>> = std::iter::once(0..num_rows).collect();
    let (Some(col_indexes), Some(off_indexes)) = (meta.column_index(), meta.offset_index()) else {
        return Ok(kept);
    };
    let Some(rg_col_idx) = col_indexes.get(rg_idx) else {
        return Ok(kept);
    };
    let Some(rg_off_idx) = off_indexes.get(rg_idx) else {
        return Ok(kept);
    };

    let descr = rg.schema_descr();
    for (col_name, constraints) in &plan.constraints {
        let Some(leaf_idx) = find_scalar_leaf(descr, col_name) else {
            continue;
        };
        let Some(col_index) = rg_col_idx.get(leaf_idx) else {
            continue;
        };
        let Some(offset_index) = rg_off_idx.get(leaf_idx) else {
            continue;
        };
        let locs = offset_index.page_locations();
        if locs.is_empty() {
            continue;
        }
        validate_page_locations(locs, num_rows, file_size)?;
        let num_pages = locs.len();
        if col_index.num_pages() as usize != num_pages {
            continue;
        }
        let col_descr = descr.column(leaf_idx);
        let p_ranges = page_row_ranges(locs, num_rows);
        let mut col_kept: Vec<Range<usize>> = Vec::new();
        for (p_idx, p_range) in p_ranges.into_iter().enumerate() {
            if column_index_page_matches(&col_descr, col_index, p_idx, constraints) {
                if let Some(last) = col_kept.last_mut()
                    && last.end == p_range.start
                {
                    last.end = p_range.end;
                } else {
                    col_kept.push(p_range);
                }
            }
        }
        kept = intersect_ranges(&kept, &col_kept);
        if kept.is_empty() {
            return Ok(Vec::new());
        }
    }
    Ok(kept)
}

type RgKeptRows = (usize, Vec<Range<usize>>);

fn filter_row_groups(
    store: &Arc<dyn ObjectStore>,
    file: &FileMeta,
    meta: &ParquetMetaData,
    plan: &ScanPlan,
    suspects: &Suspects,
    cancel: &CancelToken,
    runtime: &tokio::runtime::Handle,
) -> Result<Vec<RgKeptRows>, QueryError> {
    let path = ObjectPath::from(file.path.as_str());
    let mut stat_surviving = Vec::new();
    for rg_idx in 0..meta.num_row_groups() {
        let rg = meta.row_group(rg_idx);
        if checked_rg_num_rows(rg).is_err() {
            return Err(corrupt_err(file.id, suspects));
        }
        if rg_matches_statistics(rg, plan) {
            stat_surviving.push(rg_idx);
        }
    }
    if stat_surviving.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-fetch bloom filters for surviving row groups.
    let mut bloom_requests: Vec<(usize, usize, Range<u64>)> = Vec::new();
    if !plan.bloom_lookups.is_empty() {
        let descr = meta.row_group(0).schema_descr();
        for (lookup_idx, lookup) in plan.bloom_lookups.iter().enumerate() {
            let Some(leaf_idx) = find_scalar_leaf(descr, &lookup.column) else {
                continue;
            };
            for &rg_idx in &stat_surviving {
                let chunk = meta.row_group(rg_idx).column(leaf_idx);
                if let (Some(off), Some(len)) =
                    (chunk.bloom_filter_offset(), chunk.bloom_filter_length())
                    && off >= 0
                    && len > 0
                {
                    let start = off as u64;
                    let end = start.saturating_add(len as u64);
                    if end <= file.size {
                        bloom_requests.push((rg_idx, lookup_idx, start..end));
                    }
                }
            }
        }
    }

    let mut pruned_by_bloom: BTreeSet<usize> = BTreeSet::new();
    if !bloom_requests.is_empty() {
        let ranges: Vec<Range<u64>> = bloom_requests.iter().map(|(_, _, r)| r.clone()).collect();
        let bufs = fetch_ranges(store, &path, &ranges, cancel, runtime)
            .map_err(|e| map_fetch_err(&e, file.id, suspects))?;
        if bufs.len() != ranges.len()
            || bufs
                .iter()
                .zip(&ranges)
                .any(|(b, r)| b.len() as u64 != r.end - r.start)
        {
            return Err(corrupt_err(file.id, suspects));
        }
        for ((rg_idx, lookup_idx, _), bytes) in bloom_requests.into_iter().zip(bufs) {
            if pruned_by_bloom.contains(&rg_idx) {
                continue;
            }
            let sbbf = guard_sync(|| Sbbf::from_bytes(&bytes))
                .map_err(|_| corrupt_err(file.id, suspects))?;
            if sbbf.num_blocks() == 0 {
                return Err(corrupt_err(file.id, suspects));
            }
            let keys = &plan.bloom_lookups[lookup_idx].keys;
            let matched = guard_sync(|| Ok(keys.iter().any(|k| sbbf.check(k.as_slice()))))
                .map_err(|_| corrupt_err(file.id, suspects))?;
            if !matched {
                pruned_by_bloom.insert(rg_idx);
            }
        }
    }

    let mut result = Vec::new();
    for rg_idx in stat_surviving {
        if pruned_by_bloom.contains(&rg_idx) {
            continue;
        }
        let kept = guard_sync(|| prune_pages_for_row_group(meta, rg_idx, plan, file.size))
            .map_err(|_| corrupt_err(file.id, suspects))?;
        if !kept.is_empty() {
            result.push((rg_idx, kept));
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// SliceChunkReader & range coalescing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SliceChunkReader {
    file_len: u64,
    slices: Arc<[(u64, Bytes)]>,
}

impl SliceChunkReader {
    fn from_fetched(file_len: u64, merged_ranges: Vec<Range<u64>>, bufs: Vec<Bytes>) -> Self {
        let slices: Vec<(u64, Bytes)> = merged_ranges
            .into_iter()
            .zip(bufs)
            .map(|(r, b)| (r.start, b))
            .collect();
        Self {
            file_len,
            slices: slices.into(),
        }
    }

    fn find_slice(&self, start: u64, len: usize) -> parquet::errors::Result<Bytes> {
        let end = start
            .checked_add(len as u64)
            .ok_or_else(|| ParquetError::General("byte range overflow".to_owned()))?;
        let idx = self.slices.partition_point(|(s, _)| *s <= start);
        if idx == 0 {
            return Err(ParquetError::General(format!(
                "missing fetched byte range at {start}..{end}"
            )));
        }
        let (s, ref bytes) = self.slices[idx - 1];
        let slice_end = s + bytes.len() as u64;
        if end > slice_end {
            return Err(ParquetError::General(format!(
                "truncated fetched byte range at {start}..{end} (have {s}..{slice_end})"
            )));
        }
        let rel_start = (start - s) as usize;
        let rel_end = (end - s) as usize;
        Ok(bytes.slice(rel_start..rel_end))
    }
}

impl Length for SliceChunkReader {
    fn len(&self) -> u64 {
        self.file_len
    }
}

impl ChunkReader for SliceChunkReader {
    type T = bytes::buf::Reader<Bytes>;

    fn get_read(&self, start: u64) -> parquet::errors::Result<Self::T> {
        let idx = self.slices.partition_point(|(s, _)| *s <= start);
        if idx == 0 {
            return Err(ParquetError::General(format!(
                "missing fetched byte range at {start}"
            )));
        }
        let (s, ref bytes) = self.slices[idx - 1];
        let slice_end = s + bytes.len() as u64;
        if start > slice_end {
            return Err(ParquetError::General(format!(
                "offset {start} outside fetched slice {s}..{slice_end}"
            )));
        }
        Ok(bytes.slice((start - s) as usize..).reader())
    }

    fn get_bytes(&self, start: u64, length: usize) -> parquet::errors::Result<Bytes> {
        self.find_slice(start, length)
    }
}

fn coalesce_ranges(mut ranges: Vec<Range<u64>>) -> Vec<Range<u64>> {
    ranges.retain(|r| r.start < r.end);
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut merged = Vec::with_capacity(ranges.len());
    let mut cur = ranges[0].clone();
    for r in ranges.into_iter().skip(1) {
        if r.start <= cur.end {
            cur.end = cur.end.max(r.end);
        } else {
            merged.push(cur);
            cur = r;
        }
    }
    merged.push(cur);
    merged
}

// ---------------------------------------------------------------------------
// RLE / bit-packing & dictionary decoding (for DictColumn preservation)
// ---------------------------------------------------------------------------

struct BitStream<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitStream<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    fn align_to_byte(&mut self) {
        if self.bit_pos != 0 {
            self.byte_pos += 1;
            self.bit_pos = 0;
        }
    }

    fn read_vlq_u64(&mut self) -> Result<Option<u64>, ParquetError> {
        self.align_to_byte();
        if self.byte_pos >= self.data.len() {
            return Ok(None);
        }
        let mut shift = 0u32;
        let mut val = 0u64;
        for i in 0..10 {
            let Some(&b) = self.data.get(self.byte_pos + i) else {
                return Err(ParquetError::General("truncated VLQ integer".to_owned()));
            };
            val |= u64::from(b & 0x7F) << shift;
            if (b & 0x80) == 0 {
                self.byte_pos += i + 1;
                return Ok(Some(val));
            }
            shift += 7;
        }
        Err(ParquetError::General(
            "VLQ integer exceeds 10 bytes".to_owned(),
        ))
    }

    fn read_aligned_u64(&mut self, num_bytes: usize) -> Result<u64, ParquetError> {
        self.align_to_byte();
        if num_bytes > 8 || self.byte_pos + num_bytes > self.data.len() {
            return Err(ParquetError::General("truncated RLE value".to_owned()));
        }
        let mut buf = [0u8; 8];
        buf[..num_bytes].copy_from_slice(&self.data[self.byte_pos..self.byte_pos + num_bytes]);
        self.byte_pos += num_bytes;
        Ok(u64::from_le_bytes(buf))
    }

    fn remaining_bits(&self) -> usize {
        self.data
            .len()
            .saturating_sub(self.byte_pos)
            .saturating_mul(8)
            .saturating_sub(usize::from(self.bit_pos))
    }

    fn read_bits(&mut self, num_bits: u8) -> Option<u32> {
        if num_bits == 0 {
            return Some(0);
        }
        let needed = usize::from(num_bits);
        let avail = self.remaining_bits();
        if avail < needed {
            return None;
        }
        let mut out = 0u64;
        let mut read = 0usize;
        while read < needed {
            let cur_byte = self.data[self.byte_pos];
            let rem_in_byte = 8 - usize::from(self.bit_pos);
            let take = (needed - read).min(rem_in_byte);
            let mask = ((1u16 << take) - 1) as u8;
            let bits = (cur_byte >> self.bit_pos) & mask;
            out |= u64::from(bits) << read;
            read += take;
            self.bit_pos += take as u8;
            if self.bit_pos == 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
        }
        Some(out as u32)
    }
}

fn decode_rle_hybrid(data: &[u8], bit_width: u8, count: usize) -> Result<Vec<u32>, ParquetError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if bit_width > 32 {
        return Err(ParquetError::General("RLE bit_width > 32".to_owned()));
    }
    if bit_width == 0 {
        let mut out = Vec::new();
        out.try_reserve_exact(count)
            .map_err(|_| ParquetError::General("RLE allocation failed".to_owned()))?;
        out.resize(count, 0);
        return Ok(out);
    }
    let mut stream = BitStream::new(data);
    let mut out = Vec::with_capacity(count.min(4096));
    let aligned_bytes = usize::from(bit_width).div_ceil(8);

    while out.len() < count {
        let Some(header) = stream.read_vlq_u64()? else {
            return Err(ParquetError::General("truncated RLE stream".to_owned()));
        };
        if header == 0 {
            return Err(ParquetError::General("zero RLE header".to_owned()));
        }
        if (header & 1) == 1 {
            let num_groups = header >> 1;
            let run_vals = usize::try_from(num_groups.saturating_mul(8))
                .map_err(|_| ParquetError::General("RLE run length overflow".to_owned()))?;
            let to_read = run_vals.min(count - out.len());
            if to_read
                .checked_mul(usize::from(bit_width))
                .is_none_or(|bits| bits > stream.remaining_bits())
            {
                return Err(ParquetError::General("truncated bit-packed run".to_owned()));
            }
            out.try_reserve(to_read)
                .map_err(|_| ParquetError::General("RLE allocation failed".to_owned()))?;
            for _ in 0..to_read {
                let v = stream
                    .read_bits(bit_width)
                    .ok_or_else(|| ParquetError::General("truncated bit-packed run".to_owned()))?;
                out.push(v);
            }
            // If `to_read < run_vals`, skip remaining padding bits only if more runs follow
            if out.len() < count && to_read < run_vals {
                for _ in to_read..run_vals {
                    let _ = stream.read_bits(bit_width);
                }
            }
        } else {
            let repeat = usize::try_from(header >> 1)
                .map_err(|_| ParquetError::General("RLE repeat length overflow".to_owned()))?;
            let val = stream.read_aligned_u64(aligned_bytes)?;
            let val_u32 = u32::try_from(val)
                .map_err(|_| ParquetError::General("RLE value exceeds u32".to_owned()))?;
            let take = repeat.min(count - out.len());
            out.try_reserve(take)
                .map_err(|_| ParquetError::General("RLE allocation failed".to_owned()))?;
            out.resize(out.len() + take, val_u32);
        }
    }
    Ok(out)
}

fn bits_required(max_val: i16) -> u8 {
    if max_val <= 0 {
        0
    } else {
        (64 - (max_val as u64).leading_zeros()) as u8
    }
}

fn decode_levels(
    max_level: i16,
    num_values: usize,
    encoding: Encoding,
    data: &[u8],
) -> Result<Vec<i16>, ParquetError> {
    if max_level <= 0 {
        return Ok(Vec::new());
    }
    let bit_width = bits_required(max_level);
    match encoding {
        Encoding::RLE => {
            let raw = decode_rle_hybrid(data, bit_width, num_values)?;
            let mut out = Vec::new();
            out.try_reserve_exact(raw.len())
                .map_err(|_| ParquetError::General("level allocation failed".to_owned()))?;
            for v in raw {
                let lvl = i16::try_from(v)
                    .map_err(|_| ParquetError::General("level value exceeds i16".to_owned()))?;
                if lvl > max_level {
                    return Err(ParquetError::General("level exceeds max_level".to_owned()));
                }
                out.push(lvl);
            }
            Ok(out)
        }
        #[allow(deprecated)]
        Encoding::BIT_PACKED => {
            let needed_bits = num_values
                .checked_mul(usize::from(bit_width))
                .ok_or_else(|| {
                    ParquetError::General("bit-packed level count overflow".to_owned())
                })?;
            let avail_bits = data
                .len()
                .checked_mul(8)
                .ok_or_else(|| ParquetError::General("bit-packed data size overflow".to_owned()))?;
            if needed_bits > avail_bits {
                return Err(ParquetError::General(
                    "truncated bit-packed levels".to_owned(),
                ));
            }
            let mut stream = BitStream::new(data);
            let mut out = Vec::new();
            out.try_reserve_exact(num_values)
                .map_err(|_| ParquetError::General("level allocation failed".to_owned()))?;
            for _ in 0..num_values {
                let v = stream.read_bits(bit_width).ok_or_else(|| {
                    ParquetError::General("truncated bit-packed levels".to_owned())
                })?;
                let lvl = i16::try_from(v)
                    .map_err(|_| ParquetError::General("level value exceeds i16".to_owned()))?;
                if lvl > max_level {
                    return Err(ParquetError::General("level exceeds max_level".to_owned()));
                }
                out.push(lvl);
            }
            Ok(out)
        }
        other => Err(ParquetError::General(format!(
            "unsupported level encoding: {other}"
        ))),
    }
}

fn split_v1_level_slice(
    max_level: i16,
    num_values: u32,
    encoding: Encoding,
    buf: &[u8],
) -> Result<(usize, &[u8]), ParquetError> {
    match encoding {
        Encoding::RLE => {
            if buf.len() < 4 {
                return Err(ParquetError::General(
                    "truncated V1 level length".to_owned(),
                ));
            }
            let len = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if len < 0 {
                return Err(ParquetError::General("negative V1 level length".to_owned()));
            }
            let end = 4usize
                .checked_add(len as usize)
                .ok_or_else(|| ParquetError::General("V1 level length overflow".to_owned()))?;
            if end > buf.len() {
                return Err(ParquetError::General("truncated V1 level data".to_owned()));
            }
            Ok((end, &buf[4..end]))
        }
        #[allow(deprecated)]
        Encoding::BIT_PACKED => {
            let bw = usize::from(bits_required(max_level));
            let bits = (num_values as usize)
                .checked_mul(bw)
                .ok_or_else(|| ParquetError::General("V1 bit-packed level overflow".to_owned()))?;
            let num_bytes = bits.div_ceil(8);
            if num_bytes > buf.len() {
                return Err(ParquetError::General(
                    "truncated V1 bit-packed level data".to_owned(),
                ));
            }
            Ok((num_bytes, &buf[..num_bytes]))
        }
        other => Err(ParquetError::General(format!(
            "invalid V1 level encoding: {other}"
        ))),
    }
}

fn decode_plain_utf8_dict(buf: &[u8], num_values: u32) -> Result<FlatColumn, ParquetError> {
    let count = num_values as usize;
    if count > buf.len() / 4 {
        return Err(ParquetError::General(
            "dictionary value count exceeds buffer capacity".to_owned(),
        ));
    }
    let cap = count
        .checked_add(1)
        .ok_or_else(|| ParquetError::General("dictionary offset count overflow".to_owned()))?;
    let mut offsets = Vec::new();
    offsets
        .try_reserve_exact(cap)
        .map_err(|_| ParquetError::General("dictionary offsets allocation failed".to_owned()))?;
    offsets.push(0u32);
    let mut bytes = Vec::new();
    let mut pos = 0usize;
    for _ in 0..count {
        if pos + 4 > buf.len() {
            return Err(ParquetError::General(
                "truncated dictionary string length".to_owned(),
            ));
        }
        let len_u32 = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]);
        pos += 4;
        let len = len_u32 as usize;
        let end = pos
            .checked_add(len)
            .ok_or_else(|| ParquetError::General("dictionary string overflow".to_owned()))?;
        if end > buf.len() {
            return Err(ParquetError::General(
                "truncated dictionary string bytes".to_owned(),
            ));
        }
        let s = std::str::from_utf8(&buf[pos..end])
            .map_err(|_| ParquetError::General("invalid UTF-8 in dictionary".to_owned()))?;
        bytes.extend_from_slice(s.as_bytes());
        let next_off = u32::try_from(bytes.len())
            .map_err(|_| ParquetError::General("dictionary exceeds u32::MAX bytes".to_owned()))?;
        offsets.push(next_off);
        pos = end;
    }
    let utf8 = Utf8Data::try_new(offsets, bytes)
        .map_err(|_| ParquetError::General("invalid dictionary Utf8Data".to_owned()))?;
    FlatColumn::try_new(FlatData::Utf8(utf8), None)
        .map_err(|_| ParquetError::General("invalid dictionary FlatColumn".to_owned()))
}

// ---------------------------------------------------------------------------
// Per-leaf and per-column decoders
// ---------------------------------------------------------------------------

struct FallbackPageReader {
    queued: VecDeque<Page>,
    rest: SerializedPageReader<SliceChunkReader>,
}

impl Iterator for FallbackPageReader {
    type Item = parquet::errors::Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next_page().transpose()
    }
}

impl PageReader for FallbackPageReader {
    fn get_next_page(&mut self) -> parquet::errors::Result<Option<Page>> {
        if let Some(p) = self.queued.pop_front() {
            return Ok(Some(p));
        }
        self.rest.get_next_page()
    }

    fn peek_next_page(&mut self) -> parquet::errors::Result<Option<PageMetadata>> {
        if let Some(p) = self.queued.front() {
            return Ok(Some(match p {
                Page::DictionaryPage { .. } => PageMetadata {
                    num_rows: None,
                    num_levels: None,
                    is_dict: true,
                },
                Page::DataPage { num_values, .. } => PageMetadata {
                    num_rows: None,
                    num_levels: Some(*num_values as usize),
                    is_dict: false,
                },
                Page::DataPageV2 {
                    num_values,
                    num_rows,
                    ..
                } => PageMetadata {
                    num_rows: Some(*num_rows as usize),
                    num_levels: Some(*num_values as usize),
                    is_dict: false,
                },
            }));
        }
        self.rest.peek_next_page()
    }

    fn skip_next_page(&mut self) -> parquet::errors::Result<()> {
        if self.queued.pop_front().is_some() {
            return Ok(());
        }
        self.rest.skip_next_page()
    }

    fn at_record_boundary(&mut self) -> parquet::errors::Result<bool> {
        if let Some(p) = self.queued.front() {
            return Ok(matches!(p, Page::DataPageV2 { .. }));
        }
        self.rest.at_record_boundary()
    }
}

struct DecodedDictPage {
    def_levels: Vec<i16>,
    num_levels: usize,
    codes: Vec<u32>,
    record_starts: Vec<usize>,
    record_cursor: usize,
    code_cursor: usize,
}

impl DecodedDictPage {
    fn records_left(&self) -> usize {
        self.record_starts.len().saturating_sub(self.record_cursor)
    }
}

enum DictUtf8Mode {
    Dict {
        page_reader: SerializedPageReader<SliceChunkReader>,
        dict_page: Option<Page>,
        dict_flat: Option<FlatColumn>,
        dict_gen: u64,
        current_page: Option<DecodedDictPage>,
    },
    Flat(ColumnReaderImpl<ByteArrayType>),
    Transitioning,
}

struct DictUtf8LeafDecoder {
    descr: ColumnDescPtr,
    rg_num_rows: usize,
    mode: DictUtf8Mode,
}

impl DictUtf8LeafDecoder {
    fn new(
        descr: ColumnDescPtr,
        page_reader: SerializedPageReader<SliceChunkReader>,
        rg_num_rows: usize,
    ) -> Self {
        Self {
            descr,
            rg_num_rows,
            mode: DictUtf8Mode::Dict {
                page_reader,
                dict_page: None,
                dict_flat: None,
                dict_gen: 0,
                current_page: None,
            },
        }
    }

    fn skip_records(&mut self, n: usize) -> Result<(), ParquetError> {
        let mut remaining = n;
        while remaining > 0 {
            match &mut self.mode {
                DictUtf8Mode::Transitioning => {
                    return Err(ParquetError::General("invalid dict state".to_owned()));
                }
                DictUtf8Mode::Flat(reader) => {
                    let skipped = reader.skip_records(remaining)?;
                    if skipped < remaining {
                        return Err(ParquetError::General(
                            "truncated column during skip".to_owned(),
                        ));
                    }
                    return Ok(());
                }
                DictUtf8Mode::Dict {
                    page_reader,
                    dict_page,
                    dict_flat,
                    dict_gen,
                    current_page,
                } => {
                    if let Some(cur) = current_page {
                        let avail = cur.records_left();
                        if avail > 0 {
                            let take = remaining.min(avail);
                            let end_rec = cur.record_cursor + take;
                            let end_lvl = if end_rec < cur.record_starts.len() {
                                cur.record_starts[end_rec]
                            } else {
                                cur.num_levels
                            };
                            let start_lvl = cur.record_starts[cur.record_cursor];
                            let max_def = self.descr.max_def_level();
                            let skipped_vals = if max_def == 0 {
                                end_lvl - start_lvl
                            } else {
                                cur.def_levels[start_lvl..end_lvl]
                                    .iter()
                                    .filter(|&&d| d == max_def)
                                    .count()
                            };
                            cur.record_cursor = end_rec;
                            cur.code_cursor += skipped_vals;
                            remaining -= take;
                            if cur.records_left() == 0 {
                                *current_page = None;
                            }
                            continue;
                        }
                        *current_page = None;
                    }

                    // Always consume dictionary page via `get_next_page()` BEFORE `skip_next_page()`.
                    let Some(meta) = page_reader.peek_next_page()? else {
                        return Err(ParquetError::General(
                            "unexpected EOF skipping dict column".to_owned(),
                        ));
                    };
                    if meta.is_dict {
                        let Some(page) = page_reader.get_next_page()? else {
                            return Err(ParquetError::General("missing dict page".to_owned()));
                        };
                        if let Page::DictionaryPage {
                            ref buf,
                            num_values,
                            encoding,
                            ..
                        } = page
                        {
                            if !matches!(
                                encoding,
                                Encoding::PLAIN
                                    | Encoding::PLAIN_DICTIONARY
                                    | Encoding::RLE_DICTIONARY
                            ) {
                                return Err(ParquetError::General(
                                    "unsupported dict page encoding".to_owned(),
                                ));
                            }
                            let flat = decode_plain_utf8_dict(buf, num_values)?;
                            *dict_flat = Some(flat);
                            *dict_page = Some(page);
                            *dict_gen = dict_gen.wrapping_add(1);
                            continue;
                        }
                        return Err(ParquetError::General("expected dictionary page".to_owned()));
                    }

                    let page_rows = meta.num_rows.or_else(|| {
                        (self.descr.max_rep_level() == 0)
                            .then_some(meta.num_levels)
                            .flatten()
                    });
                    if let Some(rows) = page_rows
                        && rows <= remaining
                    {
                        page_reader.skip_next_page()?;
                        remaining -= rows;
                        continue;
                    }

                    // Need to load the next page (partially skipped or multi-page).
                    if !self.load_next_page_for_dict()? {
                        return Err(ParquetError::General(
                            "unexpected EOF loading dict page".to_owned(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn load_next_page_for_dict(&mut self) -> Result<bool, ParquetError> {
        let other_page = {
            let DictUtf8Mode::Dict {
                page_reader,
                dict_page,
                dict_flat,
                dict_gen,
                current_page,
            } = &mut self.mode
            else {
                return Ok(true);
            };

            loop {
                let Some(page) = page_reader.get_next_page()? else {
                    return Ok(false);
                };
                match page {
                    Page::DictionaryPage {
                        ref buf,
                        num_values,
                        encoding,
                        ..
                    } => {
                        if !matches!(
                            encoding,
                            Encoding::PLAIN | Encoding::PLAIN_DICTIONARY | Encoding::RLE_DICTIONARY
                        ) {
                            return Err(ParquetError::General(
                                "unsupported dict page encoding".to_owned(),
                            ));
                        }
                        let flat = decode_plain_utf8_dict(buf, num_values)?;
                        *dict_flat = Some(flat);
                        *dict_page = Some(page);
                        *dict_gen = dict_gen.wrapping_add(1);
                    }
                    Page::DataPage {
                        ref buf,
                        num_values,
                        encoding,
                        def_level_encoding,
                        rep_level_encoding,
                        ..
                    } if matches!(
                        encoding,
                        Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY
                    ) && dict_flat.is_some() =>
                    {
                        let max_rep = self.descr.max_rep_level();
                        let max_def = self.descr.max_def_level();
                        let n = num_values as usize;
                        if max_rep == 0 && n > self.rg_num_rows {
                            return Err(ParquetError::General(
                                "page num_values exceeds row group row count".to_owned(),
                            ));
                        }
                        let mut offset = 0usize;
                        let rep_levels = if max_rep > 0 {
                            let (used, slice) = split_v1_level_slice(
                                max_rep,
                                num_values,
                                rep_level_encoding,
                                &buf[offset..],
                            )?;
                            offset += used;
                            decode_levels(max_rep, n, rep_level_encoding, slice)?
                        } else {
                            Vec::new()
                        };
                        let def_levels = if max_def > 0 {
                            let (used, slice) = split_v1_level_slice(
                                max_def,
                                num_values,
                                def_level_encoding,
                                &buf[offset..],
                            )?;
                            offset += used;
                            decode_levels(max_def, n, def_level_encoding, slice)?
                        } else {
                            Vec::new()
                        };
                        let non_null = if max_def > 0 {
                            def_levels.iter().filter(|&&d| d == max_def).count()
                        } else {
                            n
                        };
                        let val_slice = &buf[offset..];
                        let codes = if non_null == 0 {
                            Vec::new()
                        } else {
                            if val_slice.is_empty() {
                                return Err(ParquetError::General(
                                    "empty dictionary data".to_owned(),
                                ));
                            }
                            decode_rle_hybrid(&val_slice[1..], val_slice[0], non_null)?
                        };
                        let dict_len = dict_flat.as_ref().map_or(0, FlatColumn::len);
                        for &c in &codes {
                            if (c as usize) >= dict_len {
                                return Err(ParquetError::General(
                                    "dictionary code out of bounds".to_owned(),
                                ));
                            }
                        }
                        let record_starts = compute_record_starts(&rep_levels, n)?;
                        *current_page = Some(DecodedDictPage {
                            def_levels,
                            num_levels: n,
                            codes,
                            record_starts,
                            record_cursor: 0,
                            code_cursor: 0,
                        });
                        return Ok(true);
                    }
                    Page::DataPageV2 {
                        ref buf,
                        num_values,
                        encoding,
                        num_nulls,
                        def_levels_byte_len,
                        rep_levels_byte_len,
                        ..
                    } if matches!(
                        encoding,
                        Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY
                    ) && dict_flat.is_some() =>
                    {
                        if num_nulls > num_values {
                            return Err(ParquetError::General("more nulls than values".to_owned()));
                        }
                        let max_rep = self.descr.max_rep_level();
                        let max_def = self.descr.max_def_level();
                        if max_def == 0 && num_nulls != 0 {
                            return Err(ParquetError::General(
                                "non-zero nulls on required column".to_owned(),
                            ));
                        }
                        let n = num_values as usize;
                        if max_rep == 0 && n > self.rg_num_rows {
                            return Err(ParquetError::General(
                                "page num_values exceeds row group row count".to_owned(),
                            ));
                        }
                        let rep_len = rep_levels_byte_len as usize;
                        let def_len = def_levels_byte_len as usize;
                        if rep_len
                            .checked_add(def_len)
                            .is_none_or(|sum| sum > buf.len())
                        {
                            return Err(ParquetError::General(
                                "truncated V2 level lengths".to_owned(),
                            ));
                        }
                        let rep_levels = if max_rep > 0 {
                            decode_levels(max_rep, n, Encoding::RLE, &buf[..rep_len])?
                        } else {
                            Vec::new()
                        };
                        let def_levels = if max_def > 0 {
                            decode_levels(
                                max_def,
                                n,
                                Encoding::RLE,
                                &buf[rep_len..rep_len + def_len],
                            )?
                        } else {
                            Vec::new()
                        };
                        let non_null = (num_values - num_nulls) as usize;
                        if max_def > 0 {
                            let actual_non_null =
                                def_levels.iter().filter(|&&d| d == max_def).count();
                            if non_null != actual_non_null {
                                return Err(ParquetError::General(
                                    "DataPageV2 non-null count mismatch".to_owned(),
                                ));
                            }
                        }
                        let val_slice = &buf[rep_len + def_len..];
                        let codes = if non_null == 0 {
                            Vec::new()
                        } else {
                            if val_slice.is_empty() {
                                return Err(ParquetError::General(
                                    "empty dictionary data".to_owned(),
                                ));
                            }
                            decode_rle_hybrid(&val_slice[1..], val_slice[0], non_null)?
                        };
                        let dict_len = dict_flat.as_ref().map_or(0, FlatColumn::len);
                        for &c in &codes {
                            if (c as usize) >= dict_len {
                                return Err(ParquetError::General(
                                    "dictionary code out of bounds".to_owned(),
                                ));
                            }
                        }
                        let record_starts = compute_record_starts(&rep_levels, n)?;
                        *current_page = Some(DecodedDictPage {
                            def_levels,
                            num_levels: n,
                            codes,
                            record_starts,
                            record_cursor: 0,
                            code_cursor: 0,
                        });
                        return Ok(true);
                    }
                    other => break other,
                }
            }
        };

        // Transition cleanly to `ColumnReaderImpl<ByteArrayType>` on non-dict page.
        let old = std::mem::replace(&mut self.mode, DictUtf8Mode::Transitioning);
        let DictUtf8Mode::Dict {
            page_reader,
            mut dict_page,
            ..
        } = old
        else {
            return Err(ParquetError::General(
                "invalid dict state transition".to_owned(),
            ));
        };
        let mut queued = VecDeque::new();
        if let Some(dp) = dict_page.take() {
            queued.push_back(dp);
        }
        queued.push_back(other_page);
        let fallback = Box::new(FallbackPageReader {
            queued,
            rest: page_reader,
        });
        let col_reader = match get_column_reader(self.descr.clone(), fallback) {
            ColumnReader::ByteArrayColumnReader(r) => r,
            _ => {
                return Err(ParquetError::General(
                    "expected ByteArray column reader".to_owned(),
                ));
            }
        };
        self.mode = DictUtf8Mode::Flat(col_reader);
        Ok(true)
    }

    fn read_scalar_utf8_ranges(
        &mut self,
        ranges: &[Range<usize>],
        current_row: &mut usize,
    ) -> Result<Column, ParquetError> {
        let total_rows: usize = ranges.iter().map(|r| r.len()).sum();
        if total_rows == 0 {
            return Ok(Column::Flat(FlatColumn::empty(&DataType::Utf8)));
        }

        let mut dict_parts: Option<(u64, FlatColumn, Vec<u32>, Vec<bool>)> = None;
        let mut flat_parts: Vec<FlatColumn> = Vec::new();
        let max_def = self.descr.max_def_level();

        for range in ranges {
            if range.start < *current_row {
                return Err(ParquetError::General(
                    "non-monotonic row range in utf8 reader".to_owned(),
                ));
            }
            if range.start > *current_row {
                self.skip_records(range.start - *current_row)?;
                *current_row = range.start;
            }
            let mut remaining = range.len();
            while remaining > 0 {
                match &mut self.mode {
                    DictUtf8Mode::Transitioning => {
                        return Err(ParquetError::General("invalid dict state".to_owned()));
                    }
                    DictUtf8Mode::Flat(reader) => {
                        if let Some((_, df, codes, valid)) = dict_parts.take() {
                            flat_parts.push(materialize_dict_parts(df, codes, &valid)?);
                        }
                        let chunk = read_flat_utf8_scalar(&self.descr, reader, remaining)?;
                        flat_parts.push(chunk);
                        *current_row += remaining;
                        remaining = 0;
                    }
                    DictUtf8Mode::Dict { .. } => {
                        let has_cur = matches!(
                            &self.mode,
                            DictUtf8Mode::Dict {
                                current_page: Some(_),
                                ..
                            }
                        );
                        if !has_cur && !self.load_next_page_for_dict()? {
                            return Err(ParquetError::General(
                                "unexpected EOF reading utf8".to_owned(),
                            ));
                        }
                        let DictUtf8Mode::Dict {
                            dict_flat: Some(df),
                            dict_gen,
                            current_page: cur_opt,
                            ..
                        } = &mut self.mode
                        else {
                            continue;
                        };
                        let Some(cur) = cur_opt.as_mut() else {
                            continue;
                        };

                        if let Some((started_gen, _, _, _)) = dict_parts
                            && started_gen != *dict_gen
                            && let Some((_, old_df, old_codes, old_valid)) = dict_parts.take()
                        {
                            flat_parts.push(materialize_dict_parts(old_df, old_codes, &old_valid)?);
                        }

                        let avail = cur.records_left();
                        let take = remaining.min(avail);
                        let (_, _, codes, valid) = dict_parts.get_or_insert_with(|| {
                            (
                                *dict_gen,
                                df.clone(),
                                Vec::with_capacity(total_rows),
                                Vec::with_capacity(total_rows),
                            )
                        });
                        for r_offset in 0..take {
                            let rec = cur.record_cursor + r_offset;
                            let lvl = cur.record_starts[rec];
                            let is_valid = max_def == 0 || cur.def_levels[lvl] == max_def;
                            if is_valid {
                                codes.push(cur.codes[cur.code_cursor]);
                                cur.code_cursor += 1;
                                valid.push(true);
                            } else {
                                codes.push(0);
                                valid.push(false);
                            }
                        }
                        cur.record_cursor += take;
                        *current_row += take;
                        remaining -= take;
                        if cur.records_left() == 0 {
                            *cur_opt = None;
                        }
                    }
                }
            }
        }

        if flat_parts.is_empty() {
            let Some((_, df, codes, valid)) = dict_parts else {
                return Ok(Column::Flat(FlatColumn::empty(&DataType::Utf8)));
            };
            let nulls = if valid.iter().all(|&v| v) {
                None
            } else {
                Some(Bitmap::from_bools(&valid))
            };
            return Ok(Column::Dict(
                DictColumn::try_new(df, codes, nulls)
                    .map_err(|e| ParquetError::General(e.to_string()))?,
            ));
        }

        if let Some((_, df, codes, valid)) = dict_parts.take() {
            flat_parts.push(materialize_dict_parts(df, codes, &valid)?);
        }
        concat_flat_columns(DataType::Utf8, flat_parts)
    }

    fn read_list_utf8_ranges(
        &mut self,
        ranges: &[Range<usize>],
        current_row: &mut usize,
        outer_non_null_def: i16,
        elem_present_def: i16,
    ) -> Result<Column, ParquetError> {
        let list_dt = DataType::List(Box::new(DataType::Utf8));
        let total_rows: usize = ranges.iter().map(|r| r.len()).sum();
        if total_rows == 0 {
            return Ok(Column::Flat(FlatColumn::empty(&list_dt)));
        }

        struct DictListAccum {
            dict_gen: u64,
            dict_flat: FlatColumn,
            offsets: Vec<u32>,
            codes: Vec<u32>,
            item_valid: Vec<bool>,
            parent_valid: Vec<bool>,
        }

        let mut dict_parts: Option<DictListAccum> = None;
        let mut flat_parts: Vec<FlatColumn> = Vec::new();
        let max_def = self.descr.max_def_level();

        for range in ranges {
            if range.start < *current_row {
                return Err(ParquetError::General(
                    "non-monotonic row range in list utf8 reader".to_owned(),
                ));
            }
            if range.start > *current_row {
                self.skip_records(range.start - *current_row)?;
                *current_row = range.start;
            }
            let mut remaining = range.len();
            while remaining > 0 {
                match &mut self.mode {
                    DictUtf8Mode::Transitioning => {
                        return Err(ParquetError::General("invalid dict state".to_owned()));
                    }
                    DictUtf8Mode::Flat(reader) => {
                        if let Some(acc) = dict_parts.take() {
                            flat_parts.push(build_dict_list_flat(
                                acc.dict_flat,
                                acc.offsets,
                                acc.codes,
                                &acc.item_valid,
                                &acc.parent_valid,
                                true,
                            )?);
                        }
                        let chunk = read_flat_list_utf8(
                            &self.descr,
                            reader,
                            remaining,
                            outer_non_null_def,
                            elem_present_def,
                        )?;
                        flat_parts.push(chunk);
                        *current_row += remaining;
                        remaining = 0;
                    }
                    DictUtf8Mode::Dict { .. } => {
                        let has_cur = matches!(
                            &self.mode,
                            DictUtf8Mode::Dict {
                                current_page: Some(_),
                                ..
                            }
                        );
                        if !has_cur && !self.load_next_page_for_dict()? {
                            return Err(ParquetError::General(
                                "unexpected EOF reading list utf8".to_owned(),
                            ));
                        }
                        let DictUtf8Mode::Dict {
                            dict_flat: Some(df),
                            dict_gen,
                            current_page: cur_opt,
                            ..
                        } = &mut self.mode
                        else {
                            continue;
                        };
                        let Some(cur) = cur_opt.as_mut() else {
                            continue;
                        };

                        if let Some(acc) = &dict_parts
                            && acc.dict_gen != *dict_gen
                            && let Some(old) = dict_parts.take()
                        {
                            flat_parts.push(build_dict_list_flat(
                                old.dict_flat,
                                old.offsets,
                                old.codes,
                                &old.item_valid,
                                &old.parent_valid,
                                true,
                            )?);
                        }

                        let avail = cur.records_left();
                        let take = remaining.min(avail);
                        let acc = dict_parts.get_or_insert_with(|| {
                            let mut offsets = Vec::with_capacity(total_rows + 1);
                            offsets.push(0);
                            DictListAccum {
                                dict_gen: *dict_gen,
                                dict_flat: df.clone(),
                                offsets,
                                codes: Vec::new(),
                                item_valid: Vec::new(),
                                parent_valid: Vec::with_capacity(total_rows),
                            }
                        });
                        for r_offset in 0..take {
                            let rec = cur.record_cursor + r_offset;
                            let lvl_start = cur.record_starts[rec];
                            let lvl_end = if rec + 1 < cur.record_starts.len() {
                                cur.record_starts[rec + 1]
                            } else {
                                cur.def_levels.len()
                            };
                            let first_def = cur.def_levels[lvl_start];
                            if first_def < outer_non_null_def {
                                acc.parent_valid.push(false);
                            } else if first_def < elem_present_def {
                                acc.parent_valid.push(true);
                            } else {
                                acc.parent_valid.push(true);
                                for lvl_idx in lvl_start..lvl_end {
                                    let d = cur.def_levels[lvl_idx];
                                    if d < elem_present_def {
                                        return Err(ParquetError::General(
                                            "invalid nested list def level".to_owned(),
                                        ));
                                    }
                                    if d == max_def {
                                        acc.codes.push(cur.codes[cur.code_cursor]);
                                        cur.code_cursor += 1;
                                        acc.item_valid.push(true);
                                    } else {
                                        acc.codes.push(0);
                                        acc.item_valid.push(false);
                                    }
                                }
                            }
                            let end_off = u32::try_from(acc.codes.len()).map_err(|_| {
                                ParquetError::General("list items exceed u32::MAX".to_owned())
                            })?;
                            acc.offsets.push(end_off);
                        }
                        cur.record_cursor += take;
                        *current_row += take;
                        remaining -= take;
                        if cur.records_left() == 0 {
                            *cur_opt = None;
                        }
                    }
                }
            }
        }

        if flat_parts.is_empty() {
            let Some(acc) = dict_parts else {
                return Ok(Column::Flat(FlatColumn::empty(&list_dt)));
            };
            return Ok(Column::Flat(build_dict_list_flat(
                acc.dict_flat,
                acc.offsets,
                acc.codes,
                &acc.item_valid,
                &acc.parent_valid,
                false,
            )?));
        }

        if let Some(acc) = dict_parts.take() {
            flat_parts.push(build_dict_list_flat(
                acc.dict_flat,
                acc.offsets,
                acc.codes,
                &acc.item_valid,
                &acc.parent_valid,
                true,
            )?);
        }
        concat_flat_columns(list_dt, flat_parts)
    }
}

fn concat_flat_columns(dt: DataType, parts: Vec<FlatColumn>) -> Result<Column, ParquetError> {
    if parts.is_empty() {
        return Ok(Column::Flat(FlatColumn::empty(&dt)));
    }
    if parts.len() == 1 {
        let first = parts
            .into_iter()
            .next()
            .ok_or_else(|| ParquetError::General("empty concat parts".to_owned()))?;
        return Ok(Column::Flat(first));
    }
    let schema = Schema::try_new_arc(vec![Field::new("col", dt, true)])
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let mut batches = Vec::with_capacity(parts.len());
    for part in parts {
        let len = part.len();
        let batch = Batch::try_new(
            Arc::clone(&schema),
            vec![Column::Flat(part)],
            len,
            Selection::All,
        )
        .map_err(|e| ParquetError::General(e.to_string()))?;
        batches.push(batch);
    }
    let refs: Vec<&Batch> = batches.iter().collect();
    let combined =
        Batch::concat(schema, &refs, None).map_err(|e| ParquetError::General(e.to_string()))?;
    combined
        .columns()
        .first()
        .cloned()
        .ok_or_else(|| ParquetError::General("empty concat batch".to_owned()))
}

fn materialize_dict_parts(
    dict_flat: FlatColumn,
    codes: Vec<u32>,
    valid: &[bool],
) -> Result<FlatColumn, ParquetError> {
    let nulls = if valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(valid))
    };
    let dict_col = Column::Dict(
        DictColumn::try_new(dict_flat, codes, nulls)
            .map_err(|e| ParquetError::General(e.to_string()))?,
    );
    dict_col
        .materialize(None)
        .map_err(|e| ParquetError::General(e.to_string()))
}

fn compute_record_starts(
    rep_levels: &[i16],
    num_values: usize,
) -> Result<Vec<usize>, ParquetError> {
    if rep_levels.is_empty() {
        let mut starts = Vec::new();
        starts
            .try_reserve_exact(num_values)
            .map_err(|_| ParquetError::General("record starts allocation failed".to_owned()))?;
        starts.extend(0..num_values);
        return Ok(starts);
    }
    if rep_levels.len() != num_values {
        return Err(ParquetError::General(
            "repetition level length mismatch".to_owned(),
        ));
    }
    if num_values > 0 && rep_levels[0] != 0 {
        return Err(ParquetError::General(
            "page does not start at a record boundary".to_owned(),
        ));
    }
    let count = rep_levels.iter().filter(|&&r| r == 0).count();
    let mut starts = Vec::new();
    starts
        .try_reserve_exact(count)
        .map_err(|_| ParquetError::General("record starts allocation failed".to_owned()))?;
    for (idx, &r) in rep_levels.iter().enumerate() {
        if r == 0 {
            starts.push(idx);
        }
    }
    Ok(starts)
}

fn build_dict_list_flat(
    dict_flat: FlatColumn,
    offsets: Vec<u32>,
    codes: Vec<u32>,
    item_valid: &[bool],
    parent_valid: &[bool],
    materialize_inner: bool,
) -> Result<FlatColumn, ParquetError> {
    let elem_nulls = if item_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(item_valid))
    };
    let dict_col = DictColumn::try_new(dict_flat, codes, elem_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let values_col = if materialize_inner {
        Column::Flat(
            Column::Dict(dict_col)
                .materialize(None)
                .map_err(|e| ParquetError::General(e.to_string()))?,
        )
    } else {
        Column::Dict(dict_col)
    };
    let parent_nulls = if parent_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(parent_valid))
    };
    FlatColumn::try_new(
        FlatData::List {
            item_type: DataType::Utf8,
            offsets,
            values: Box::new(values_col),
        },
        parent_nulls,
    )
    .map_err(|e| ParquetError::General(e.to_string()))
}

struct CollectedLeaf<V> {
    row_count: usize,
    def_levels: Vec<i16>,
    rep_levels: Vec<i16>,
    values: Vec<V>,
}

fn collect_leaf_ranges<T: parquet::data_type::DataType>(
    reader: &mut ColumnReaderImpl<T>,
    descr: &ColumnDescPtr,
    ranges: &[Range<usize>],
    current_row: &mut usize,
) -> Result<CollectedLeaf<T::T>, ParquetError> {
    let total_rows: usize = ranges.iter().map(|r| r.len()).sum();
    let max_def = descr.max_def_level();
    let max_rep = descr.max_rep_level();
    let mut def_levels = Vec::new();
    let mut rep_levels = Vec::new();
    let mut values = Vec::new();

    for range in ranges {
        if range.start < *current_row {
            return Err(ParquetError::General(
                "non-monotonic row range in leaf reader".to_owned(),
            ));
        }
        if range.start > *current_row {
            let gap = range.start - *current_row;
            let skipped = reader.skip_records(gap)?;
            if skipped < gap {
                return Err(ParquetError::General(
                    "unexpected EOF skipping parquet records".to_owned(),
                ));
            }
            *current_row = range.start;
        }
        let need = range.len();
        if need == 0 {
            continue;
        }
        let (records_read, _, _) = reader.read_records(
            need,
            (max_def > 0).then_some(&mut def_levels),
            (max_rep > 0).then_some(&mut rep_levels),
            &mut values,
        )?;
        if records_read != need {
            return Err(ParquetError::General(
                "unexpected EOF reading parquet records".to_owned(),
            ));
        }
        *current_row = range.end;
    }

    Ok(CollectedLeaf {
        row_count: total_rows,
        def_levels,
        rep_levels,
        values,
    })
}

fn scatter_fixed<V: Copy + Default>(
    row_count: usize,
    def_levels: &[i16],
    max_def: i16,
    dense_vals: &[V],
) -> Result<(Vec<V>, Option<Bitmap>), ParquetError> {
    if max_def == 0 {
        if dense_vals.len() != row_count {
            return Err(ParquetError::General(
                "scalar value count mismatch".to_owned(),
            ));
        }
        return Ok((dense_vals.to_vec(), None));
    }
    if def_levels.len() != row_count {
        return Err(ParquetError::General("def level count mismatch".to_owned()));
    }
    let mut out = Vec::with_capacity(row_count);
    let mut idx = 0usize;
    let mut all_valid = true;
    let mut valid = Vec::with_capacity(row_count);
    for &d in def_levels {
        if d == max_def {
            let v = *dense_vals
                .get(idx)
                .ok_or_else(|| ParquetError::General("missing non-null scalar".to_owned()))?;
            idx += 1;
            out.push(v);
            valid.push(true);
        } else {
            all_valid = false;
            out.push(V::default());
            valid.push(false);
        }
    }
    if idx != dense_vals.len() {
        return Err(ParquetError::General(
            "trailing dense scalar values".to_owned(),
        ));
    }
    let nulls = if all_valid {
        None
    } else {
        Some(Bitmap::from_bools(&valid))
    };
    Ok((out, nulls))
}

fn byte_arrays_to_utf8(
    count: usize,
    def_levels: &[i16],
    max_def: i16,
    values: &[ByteArray],
) -> Result<(Utf8Data, Option<Bitmap>), ParquetError> {
    let mut offsets = Vec::with_capacity(count + 1);
    offsets.push(0u32);
    let mut bytes = Vec::new();
    let mut val_idx = 0usize;
    let mut valid = Vec::with_capacity(count);
    let mut all_valid = true;

    if max_def > 0 && def_levels.len() < count {
        return Err(ParquetError::General("def level count mismatch".to_owned()));
    }
    for row_idx in 0..count {
        let present = max_def == 0 || def_levels.get(row_idx).is_some_and(|&d| d == max_def);
        if present {
            let ba = values
                .get(val_idx)
                .ok_or_else(|| ParquetError::General("missing byte array value".to_owned()))?;
            val_idx += 1;
            let s = std::str::from_utf8(ba.data())
                .map_err(|_| ParquetError::General("invalid UTF-8 in parquet column".to_owned()))?;
            bytes.extend_from_slice(s.as_bytes());
            valid.push(true);
        } else {
            all_valid = false;
            valid.push(false);
        }
        let off = u32::try_from(bytes.len())
            .map_err(|_| ParquetError::General("utf8 column exceeds u32::MAX bytes".to_owned()))?;
        offsets.push(off);
    }
    if val_idx != values.len() {
        return Err(ParquetError::General(
            "trailing utf8 byte array values".to_owned(),
        ));
    }
    let utf8 =
        Utf8Data::try_new(offsets, bytes).map_err(|e| ParquetError::General(e.to_string()))?;
    let nulls = if all_valid {
        None
    } else {
        Some(Bitmap::from_bools(&valid))
    };
    Ok((utf8, nulls))
}

fn read_flat_utf8_scalar(
    descr: &ColumnDescPtr,
    reader: &mut ColumnReaderImpl<ByteArrayType>,
    n: usize,
) -> Result<FlatColumn, ParquetError> {
    let max_def = descr.max_def_level();
    let mut def_levels = Vec::new();
    let mut values = Vec::new();
    let (records_read, _, _) = reader.read_records(
        n,
        (max_def > 0).then_some(&mut def_levels),
        None,
        &mut values,
    )?;
    if records_read != n {
        return Err(ParquetError::General(
            "unexpected EOF reading flat utf8".to_owned(),
        ));
    }
    let (utf8, nulls) = byte_arrays_to_utf8(n, &def_levels, max_def, &values)?;
    FlatColumn::try_new(FlatData::Utf8(utf8), nulls)
        .map_err(|e| ParquetError::General(e.to_string()))
}

fn read_flat_list_utf8(
    descr: &ColumnDescPtr,
    reader: &mut ColumnReaderImpl<ByteArrayType>,
    n: usize,
    outer_non_null_def: i16,
    elem_present_def: i16,
) -> Result<FlatColumn, ParquetError> {
    let max_def = descr.max_def_level();
    let mut def_levels = Vec::new();
    let mut rep_levels = Vec::new();
    let mut values = Vec::new();
    let (records_read, _, _) = reader.read_records(
        n,
        (max_def > 0).then_some(&mut def_levels),
        Some(&mut rep_levels),
        &mut values,
    )?;
    if records_read != n {
        return Err(ParquetError::General(
            "unexpected EOF reading flat list utf8".to_owned(),
        ));
    }
    let starts = compute_record_starts(&rep_levels, def_levels.len())?;
    if starts.len() != n {
        return Err(ParquetError::General(
            "list record count mismatch".to_owned(),
        ));
    }

    let mut list_offsets = Vec::with_capacity(n + 1);
    list_offsets.push(0u32);
    let mut parent_valid = Vec::with_capacity(n);
    let mut str_offsets = Vec::new();
    str_offsets.push(0u32);
    let mut str_bytes = Vec::new();
    let mut item_valid = Vec::new();
    let mut val_idx = 0usize;

    for rec in 0..n {
        let lvl_start = starts[rec];
        let lvl_end = if rec + 1 < starts.len() {
            starts[rec + 1]
        } else {
            def_levels.len()
        };
        let first_def = def_levels[lvl_start];
        if first_def < outer_non_null_def {
            parent_valid.push(false);
        } else if first_def < elem_present_def {
            parent_valid.push(true);
        } else {
            parent_valid.push(true);
            for &d in &def_levels[lvl_start..lvl_end] {
                if d < elem_present_def {
                    return Err(ParquetError::General(
                        "invalid nested list def level".to_owned(),
                    ));
                }
                if d == max_def {
                    let ba = values.get(val_idx).ok_or_else(|| {
                        ParquetError::General("missing list utf8 element".to_owned())
                    })?;
                    val_idx += 1;
                    let s = std::str::from_utf8(ba.data()).map_err(|_| {
                        ParquetError::General("invalid UTF-8 in list element".to_owned())
                    })?;
                    str_bytes.extend_from_slice(s.as_bytes());
                    item_valid.push(true);
                } else {
                    item_valid.push(false);
                }
                let so = u32::try_from(str_bytes.len()).map_err(|_| {
                    ParquetError::General("list utf8 bytes exceed u32::MAX".to_owned())
                })?;
                str_offsets.push(so);
            }
        }
        let lo = u32::try_from(item_valid.len())
            .map_err(|_| ParquetError::General("list items exceed u32::MAX".to_owned()))?;
        list_offsets.push(lo);
    }
    if val_idx != values.len() {
        return Err(ParquetError::General(
            "trailing list utf8 values".to_owned(),
        ));
    }

    let utf8 = Utf8Data::try_new(str_offsets, str_bytes)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let elem_nulls = if item_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&item_valid))
    };
    let elem_col = FlatColumn::try_new(FlatData::Utf8(utf8), elem_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let parent_nulls = if parent_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&parent_valid))
    };
    FlatColumn::try_new(
        FlatData::List {
            item_type: DataType::Utf8,
            offsets: list_offsets,
            values: Box::new(Column::Flat(elem_col)),
        },
        parent_nulls,
    )
    .map_err(|e| ParquetError::General(e.to_string()))
}

enum ScalarReader {
    Bool(ColumnReaderImpl<BoolType>),
    Int32(ColumnReaderImpl<Int32Type>),
    Int64(ColumnReaderImpl<Int64Type>),
    Float(ColumnReaderImpl<FloatType>),
    Double(ColumnReaderImpl<DoubleType>),
    ByteArray(ColumnReaderImpl<ByteArrayType>),
    FixedLen(ColumnReaderImpl<FixedLenByteArrayType>),
}

enum ColumnDecoder {
    MissingNull(Field),
    Scalar {
        descr: ColumnDescPtr,
        data_type: DataType,
        reader: ScalarReader,
    },
    Utf8Dict(DictUtf8LeafDecoder),
    ListUtf8Dict {
        decoder: DictUtf8LeafDecoder,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
    MapUtf8 {
        key_descr: ColumnDescPtr,
        key_reader: ColumnReaderImpl<ByteArrayType>,
        val_descr: ColumnDescPtr,
        val_reader: ColumnReaderImpl<ByteArrayType>,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
    Events {
        time_descr: ColumnDescPtr,
        time_reader: ColumnReaderImpl<Int64Type>,
        name_descr: ColumnDescPtr,
        name_reader: ColumnReaderImpl<ByteArrayType>,
        attr_descr: ColumnDescPtr,
        attr_reader: ColumnReaderImpl<ByteArrayType>,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
    Links {
        trace_descr: ColumnDescPtr,
        trace_reader: ColumnReaderImpl<FixedLenByteArrayType>,
        span_descr: ColumnDescPtr,
        span_reader: ColumnReaderImpl<FixedLenByteArrayType>,
        attr_descr: ColumnDescPtr,
        attr_reader: ColumnReaderImpl<ByteArrayType>,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
}

impl ColumnDecoder {
    fn read_ranges(
        &mut self,
        ranges: &[Range<usize>],
        current_row: &mut usize,
    ) -> Result<Column, ParquetError> {
        let total_rows: usize = ranges.iter().map(|r| r.len()).sum();
        match self {
            Self::MissingNull(field) => {
                *current_row = ranges.last().map_or(*current_row, |r| r.end);
                Ok(Column::Const {
                    value: ScalarValue::Null(field.data_type.clone()),
                    len: total_rows,
                })
            }
            Self::Utf8Dict(dec) => dec.read_scalar_utf8_ranges(ranges, current_row),
            Self::ListUtf8Dict {
                decoder,
                outer_non_null_def,
                elem_present_def,
            } => decoder.read_list_utf8_ranges(
                ranges,
                current_row,
                *outer_non_null_def,
                *elem_present_def,
            ),
            Self::Scalar {
                descr,
                data_type,
                reader,
            } => read_scalar_ranges(descr, data_type, reader, ranges, current_row),
            Self::MapUtf8 {
                key_descr,
                key_reader,
                val_descr,
                val_reader,
                outer_non_null_def,
                elem_present_def,
            } => read_map_utf8_ranges(
                key_descr,
                key_reader,
                val_descr,
                val_reader,
                *outer_non_null_def,
                *elem_present_def,
                ranges,
                current_row,
            ),
            Self::Events {
                time_descr,
                time_reader,
                name_descr,
                name_reader,
                attr_descr,
                attr_reader,
                outer_non_null_def,
                elem_present_def,
            } => read_events_ranges(
                time_descr,
                time_reader,
                name_descr,
                name_reader,
                attr_descr,
                attr_reader,
                *outer_non_null_def,
                *elem_present_def,
                ranges,
                current_row,
            ),
            Self::Links {
                trace_descr,
                trace_reader,
                span_descr,
                span_reader,
                attr_descr,
                attr_reader,
                outer_non_null_def,
                elem_present_def,
            } => read_links_ranges(
                trace_descr,
                trace_reader,
                span_descr,
                span_reader,
                attr_descr,
                attr_reader,
                *outer_non_null_def,
                *elem_present_def,
                ranges,
                current_row,
            ),
        }
    }
}

fn read_scalar_ranges(
    descr: &ColumnDescPtr,
    data_type: &DataType,
    reader: &mut ScalarReader,
    ranges: &[Range<usize>],
    current_row: &mut usize,
) -> Result<Column, ParquetError> {
    let max_def = descr.max_def_level();
    match (data_type, reader) {
        (DataType::Bool, ScalarReader::Bool(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let (vals, nulls) =
                scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &leaf.values)?;
            let col = FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&vals)), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::I64, ScalarReader::Int64(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let (vals, nulls) =
                scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &leaf.values)?;
            let col = FlatColumn::try_new(FlatData::I64(vals), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::I64, ScalarReader::Int32(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let widened: Vec<i64> = leaf.values.iter().map(|&v| widen_int32(descr, v)).collect();
            let (vals, nulls) = scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &widened)?;
            let col = FlatColumn::try_new(FlatData::I64(vals), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::F64, ScalarReader::Double(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let (vals, nulls) =
                scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &leaf.values)?;
            let col = FlatColumn::try_new(FlatData::F64(vals), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::F64, ScalarReader::Float(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let widened: Vec<f64> = leaf.values.iter().map(|&v| f64::from(v)).collect();
            let (vals, nulls) = scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &widened)?;
            let col = FlatColumn::try_new(FlatData::F64(vals), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::TimestampNs { utc }, ScalarReader::Int64(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let (vals, nulls) =
                scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &leaf.values)?;
            let col = FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                nulls,
            )
            .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::Date, ScalarReader::Int32(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let (vals, nulls) =
                scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &leaf.values)?;
            let col = FlatColumn::try_new(FlatData::Date(vals), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::Decimal128 { precision, scale }, ScalarReader::Int64(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let widened: Vec<i128> = leaf.values.iter().map(|&v| i128::from(v)).collect();
            let (vals, nulls) = scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &widened)?;
            let col = FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                nulls,
            )
            .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::Decimal128 { precision, scale }, ScalarReader::Int32(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let widened: Vec<i128> = leaf.values.iter().map(|&v| i128::from(v)).collect();
            let (vals, nulls) = scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &widened)?;
            let col = FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                nulls,
            )
            .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::Decimal128 { precision, scale }, ScalarReader::FixedLen(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let decoded: Vec<i128> = leaf
                .values
                .iter()
                .map(|b| decode_be_signed_i128(b.data()))
                .collect::<Result<_, _>>()?;
            let (vals, nulls) = scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &decoded)?;
            let col = FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                nulls,
            )
            .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::Binary, ScalarReader::ByteArray(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            if max_def > 0 && leaf.def_levels.len() != leaf.row_count {
                return Err(ParquetError::General("def level count mismatch".to_owned()));
            }
            let mut offsets = Vec::with_capacity(leaf.row_count + 1);
            offsets.push(0u32);
            let mut bytes = Vec::new();
            let mut val_idx = 0usize;
            let mut valid = Vec::with_capacity(leaf.row_count);
            let mut all_valid = true;
            for row in 0..leaf.row_count {
                if max_def == 0 || leaf.def_levels[row] == max_def {
                    let ba = leaf
                        .values
                        .get(val_idx)
                        .ok_or_else(|| ParquetError::General("missing binary value".to_owned()))?;
                    val_idx += 1;
                    bytes.extend_from_slice(ba.data());
                    valid.push(true);
                } else {
                    all_valid = false;
                    valid.push(false);
                }
                let off = u32::try_from(bytes.len()).map_err(|_| {
                    ParquetError::General("binary column exceeds u32::MAX bytes".to_owned())
                })?;
                offsets.push(off);
            }
            if val_idx != leaf.values.len() {
                return Err(ParquetError::General(
                    "trailing binary column values".to_owned(),
                ));
            }
            let bin = BinaryData::try_new(offsets, bytes)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            let nulls = if all_valid {
                None
            } else {
                Some(Bitmap::from_bools(&valid))
            };
            let col = FlatColumn::try_new(FlatData::Binary(bin), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::FixedBytes8, ScalarReader::FixedLen(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let fixed: Vec<[u8; 8]> = leaf
                .values
                .iter()
                .map(|b| copy_fixed::<8>(b.data()))
                .collect::<Result<_, _>>()?;
            let (vals, nulls) = scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &fixed)?;
            let col = FlatColumn::try_new(FlatData::FixedBytes8(vals), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::FixedBytes16, ScalarReader::FixedLen(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let fixed: Vec<[u8; 16]> = leaf
                .values
                .iter()
                .map(|b| copy_fixed::<16>(b.data()))
                .collect::<Result<_, _>>()?;
            let (vals, nulls) = scatter_fixed(leaf.row_count, &leaf.def_levels, max_def, &fixed)?;
            let col = FlatColumn::try_new(FlatData::FixedBytes16(vals), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (DataType::Utf8, ScalarReader::ByteArray(r)) => {
            let leaf = collect_leaf_ranges(r, descr, ranges, current_row)?;
            let (utf8, nulls) =
                byte_arrays_to_utf8(leaf.row_count, &leaf.def_levels, max_def, &leaf.values)?;
            let col = FlatColumn::try_new(FlatData::Utf8(utf8), nulls)
                .map_err(|e| ParquetError::General(e.to_string()))?;
            Ok(Column::Flat(col))
        }
        (dt, _) => Err(ParquetError::General(format!(
            "unsupported parquet column type mapping for {}",
            dt.display_name()
        ))),
    }
}

fn decode_be_signed_i128(bytes: &[u8]) -> Result<i128, ParquetError> {
    if bytes.is_empty() || bytes.len() > 16 {
        return Err(ParquetError::General(
            "invalid decimal byte length".to_owned(),
        ));
    }
    let fill = if bytes[0] & 0x80 != 0 { 0xFF } else { 0x00 };
    let mut buf = [fill; 16];
    let start = 16 - bytes.len();
    buf[start..].copy_from_slice(bytes);
    Ok(i128::from_be_bytes(buf))
}

fn copy_fixed<const N: usize>(bytes: &[u8]) -> Result<[u8; N], ParquetError> {
    bytes
        .try_into()
        .map_err(|_| ParquetError::General(format!("expected {N} fixed bytes")))
}

#[allow(clippy::too_many_arguments)]
fn read_map_utf8_ranges(
    key_descr: &ColumnDescPtr,
    key_reader: &mut ColumnReaderImpl<ByteArrayType>,
    val_descr: &ColumnDescPtr,
    val_reader: &mut ColumnReaderImpl<ByteArrayType>,
    outer_non_null_def: i16,
    elem_present_def: i16,
    ranges: &[Range<usize>],
    current_row: &mut usize,
) -> Result<Column, ParquetError> {
    let mut row_a = *current_row;
    let mut row_b = *current_row;
    let keys_leaf = collect_leaf_ranges(key_reader, key_descr, ranges, &mut row_a)?;
    let vals_leaf = collect_leaf_ranges(val_reader, val_descr, ranges, &mut row_b)?;
    if row_a != row_b || keys_leaf.rep_levels != vals_leaf.rep_levels {
        return Err(ParquetError::General(
            "map key/value leaves out of lockstep".to_owned(),
        ));
    }
    *current_row = row_a;

    let n = keys_leaf.row_count;
    let starts = compute_record_starts(&keys_leaf.rep_levels, keys_leaf.def_levels.len())?;
    if starts.len() != n {
        return Err(ParquetError::General(
            "map record count mismatch".to_owned(),
        ));
    }

    let key_max_def = key_descr.max_def_level();
    let val_max_def = val_descr.max_def_level();
    let mut map_offsets = Vec::with_capacity(n + 1);
    map_offsets.push(0u32);
    let mut parent_valid = Vec::with_capacity(n);
    let mut key_offsets = vec![0u32];
    let mut key_bytes = Vec::new();
    let mut val_offsets = vec![0u32];
    let mut val_bytes = Vec::new();
    let mut val_valid = Vec::new();
    let mut k_idx = 0usize;
    let mut v_idx = 0usize;

    for rec in 0..n {
        let lvl_start = starts[rec];
        let lvl_end = if rec + 1 < starts.len() {
            starts[rec + 1]
        } else {
            keys_leaf.def_levels.len()
        };
        let first_def = keys_leaf.def_levels[lvl_start];
        if first_def < outer_non_null_def {
            parent_valid.push(false);
        } else if first_def < elem_present_def {
            parent_valid.push(true);
        } else {
            parent_valid.push(true);
            for lvl in lvl_start..lvl_end {
                let kd = keys_leaf.def_levels[lvl];
                let vd = vals_leaf.def_levels[lvl];
                if kd != key_max_def {
                    return Err(ParquetError::General("unexpected null map key".to_owned()));
                }
                let kb = keys_leaf
                    .values
                    .get(k_idx)
                    .ok_or_else(|| ParquetError::General("missing map key".to_owned()))?;
                k_idx += 1;
                let ks = std::str::from_utf8(kb.data())
                    .map_err(|_| ParquetError::General("invalid UTF-8 map key".to_owned()))?;
                key_bytes.extend_from_slice(ks.as_bytes());
                let koff = u32::try_from(key_bytes.len())
                    .map_err(|_| ParquetError::General("map keys exceed u32::MAX".to_owned()))?;
                key_offsets.push(koff);

                if vd == val_max_def {
                    let vb = vals_leaf
                        .values
                        .get(v_idx)
                        .ok_or_else(|| ParquetError::General("missing map value".to_owned()))?;
                    v_idx += 1;
                    let vs = std::str::from_utf8(vb.data())
                        .map_err(|_| ParquetError::General("invalid UTF-8 map value".to_owned()))?;
                    val_bytes.extend_from_slice(vs.as_bytes());
                    val_valid.push(true);
                } else {
                    val_valid.push(false);
                }
                let voff = u32::try_from(val_bytes.len())
                    .map_err(|_| ParquetError::General("map values exceed u32::MAX".to_owned()))?;
                val_offsets.push(voff);
            }
        }
        let moff = u32::try_from(val_valid.len())
            .map_err(|_| ParquetError::General("map entries exceed u32::MAX".to_owned()))?;
        map_offsets.push(moff);
    }

    if k_idx != keys_leaf.values.len() || v_idx != vals_leaf.values.len() {
        return Err(ParquetError::General(
            "trailing map key/value entries".to_owned(),
        ));
    }

    let keys = Utf8Data::try_new(key_offsets, key_bytes)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let values = Utf8Data::try_new(val_offsets, val_bytes)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let value_nulls = if val_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&val_valid))
    };
    let map = MapData::try_new(map_offsets, keys, values, value_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let parent_nulls = if parent_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&parent_valid))
    };
    let col = FlatColumn::try_new(FlatData::MapUtf8(Box::new(map)), parent_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    Ok(Column::Flat(col))
}

#[allow(clippy::too_many_arguments)]
fn read_events_ranges(
    time_descr: &ColumnDescPtr,
    time_reader: &mut ColumnReaderImpl<Int64Type>,
    name_descr: &ColumnDescPtr,
    name_reader: &mut ColumnReaderImpl<ByteArrayType>,
    attr_descr: &ColumnDescPtr,
    attr_reader: &mut ColumnReaderImpl<ByteArrayType>,
    outer_non_null_def: i16,
    elem_present_def: i16,
    ranges: &[Range<usize>],
    current_row: &mut usize,
) -> Result<Column, ParquetError> {
    let mut row_a = *current_row;
    let mut row_b = *current_row;
    let mut row_c = *current_row;
    let times_leaf = collect_leaf_ranges(time_reader, time_descr, ranges, &mut row_a)?;
    let names_leaf = collect_leaf_ranges(name_reader, name_descr, ranges, &mut row_b)?;
    let attrs_leaf = collect_leaf_ranges(attr_reader, attr_descr, ranges, &mut row_c)?;
    if row_a != row_b
        || row_a != row_c
        || times_leaf.rep_levels != names_leaf.rep_levels
        || times_leaf.rep_levels != attrs_leaf.rep_levels
    {
        return Err(ParquetError::General(
            "events leaves out of lockstep".to_owned(),
        ));
    }
    *current_row = row_a;

    let n = times_leaf.row_count;
    let starts = compute_record_starts(&times_leaf.rep_levels, times_leaf.def_levels.len())?;
    if starts.len() != n {
        return Err(ParquetError::General(
            "events record count mismatch".to_owned(),
        ));
    }

    let time_max_def = time_descr.max_def_level();
    let name_max_def = name_descr.max_def_level();
    let attr_max_def = attr_descr.max_def_level();
    let mut offsets = Vec::with_capacity(n + 1);
    offsets.push(0u32);
    let mut parent_valid = Vec::with_capacity(n);
    let mut times = Vec::new();
    let mut name_offsets = vec![0u32];
    let mut name_bytes = Vec::new();
    let mut attr_offsets = vec![0u32];
    let mut attr_bytes = Vec::new();
    let mut attr_valid = Vec::new();
    let (mut t_idx, mut n_idx, mut a_idx) = (0usize, 0usize, 0usize);

    for rec in 0..n {
        let lvl_start = starts[rec];
        let lvl_end = if rec + 1 < starts.len() {
            starts[rec + 1]
        } else {
            times_leaf.def_levels.len()
        };
        let first_def = times_leaf.def_levels[lvl_start];
        if first_def < outer_non_null_def {
            parent_valid.push(false);
        } else if first_def < elem_present_def {
            parent_valid.push(true);
        } else {
            parent_valid.push(true);
            for lvl in lvl_start..lvl_end {
                if times_leaf.def_levels[lvl] != time_max_def
                    || names_leaf.def_levels[lvl] != name_max_def
                {
                    return Err(ParquetError::General(
                        "unexpected null event time or name".to_owned(),
                    ));
                }
                let tv = *times_leaf
                    .values
                    .get(t_idx)
                    .ok_or_else(|| ParquetError::General("missing event time".to_owned()))?;
                t_idx += 1;
                times.push(tv);

                let nb = names_leaf
                    .values
                    .get(n_idx)
                    .ok_or_else(|| ParquetError::General("missing event name".to_owned()))?;
                n_idx += 1;
                let ns = std::str::from_utf8(nb.data())
                    .map_err(|_| ParquetError::General("invalid UTF-8 event name".to_owned()))?;
                name_bytes.extend_from_slice(ns.as_bytes());
                let noff = u32::try_from(name_bytes.len())
                    .map_err(|_| ParquetError::General("event names exceed u32::MAX".to_owned()))?;
                name_offsets.push(noff);

                if attrs_leaf.def_levels[lvl] == attr_max_def {
                    let ab = attrs_leaf
                        .values
                        .get(a_idx)
                        .ok_or_else(|| ParquetError::General("missing event attr".to_owned()))?;
                    a_idx += 1;
                    let ast = std::str::from_utf8(ab.data()).map_err(|_| {
                        ParquetError::General("invalid UTF-8 event attr".to_owned())
                    })?;
                    attr_bytes.extend_from_slice(ast.as_bytes());
                    attr_valid.push(true);
                } else {
                    attr_valid.push(false);
                }
                let aoff = u32::try_from(attr_bytes.len())
                    .map_err(|_| ParquetError::General("event attrs exceed u32::MAX".to_owned()))?;
                attr_offsets.push(aoff);
            }
        }
        let eoff = u32::try_from(times.len())
            .map_err(|_| ParquetError::General("events exceed u32::MAX".to_owned()))?;
        offsets.push(eoff);
    }
    if t_idx != times_leaf.values.len()
        || n_idx != names_leaf.values.len()
        || a_idx != attrs_leaf.values.len()
    {
        return Err(ParquetError::General("trailing events values".to_owned()));
    }

    let names = Utf8Data::try_new(name_offsets, name_bytes)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let attributes = Utf8Data::try_new(attr_offsets, attr_bytes)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let attribute_nulls = if attr_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&attr_valid))
    };
    let events = EventsData::try_new(offsets, times, names, attributes, attribute_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let parent_nulls = if parent_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&parent_valid))
    };
    let col = FlatColumn::try_new(FlatData::Events(Box::new(events)), parent_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    Ok(Column::Flat(col))
}

#[allow(clippy::too_many_arguments)]
fn read_links_ranges(
    trace_descr: &ColumnDescPtr,
    trace_reader: &mut ColumnReaderImpl<FixedLenByteArrayType>,
    span_descr: &ColumnDescPtr,
    span_reader: &mut ColumnReaderImpl<FixedLenByteArrayType>,
    attr_descr: &ColumnDescPtr,
    attr_reader: &mut ColumnReaderImpl<ByteArrayType>,
    outer_non_null_def: i16,
    elem_present_def: i16,
    ranges: &[Range<usize>],
    current_row: &mut usize,
) -> Result<Column, ParquetError> {
    let mut row_a = *current_row;
    let mut row_b = *current_row;
    let mut row_c = *current_row;
    let traces_leaf = collect_leaf_ranges(trace_reader, trace_descr, ranges, &mut row_a)?;
    let spans_leaf = collect_leaf_ranges(span_reader, span_descr, ranges, &mut row_b)?;
    let attrs_leaf = collect_leaf_ranges(attr_reader, attr_descr, ranges, &mut row_c)?;
    if row_a != row_b
        || row_a != row_c
        || traces_leaf.rep_levels != spans_leaf.rep_levels
        || traces_leaf.rep_levels != attrs_leaf.rep_levels
    {
        return Err(ParquetError::General(
            "links leaves out of lockstep".to_owned(),
        ));
    }
    *current_row = row_a;

    let n = traces_leaf.row_count;
    let starts = compute_record_starts(&traces_leaf.rep_levels, traces_leaf.def_levels.len())?;
    if starts.len() != n {
        return Err(ParquetError::General(
            "links record count mismatch".to_owned(),
        ));
    }

    let trace_max_def = trace_descr.max_def_level();
    let span_max_def = span_descr.max_def_level();
    let attr_max_def = attr_descr.max_def_level();
    let mut offsets = Vec::with_capacity(n + 1);
    offsets.push(0u32);
    let mut parent_valid = Vec::with_capacity(n);
    let mut trace_ids = Vec::new();
    let mut span_ids = Vec::new();
    let mut attr_offsets = vec![0u32];
    let mut attr_bytes = Vec::new();
    let mut attr_valid = Vec::new();
    let (mut t_idx, mut s_idx, mut a_idx) = (0usize, 0usize, 0usize);

    for rec in 0..n {
        let lvl_start = starts[rec];
        let lvl_end = if rec + 1 < starts.len() {
            starts[rec + 1]
        } else {
            traces_leaf.def_levels.len()
        };
        let first_def = traces_leaf.def_levels[lvl_start];
        if first_def < outer_non_null_def {
            parent_valid.push(false);
        } else if first_def < elem_present_def {
            parent_valid.push(true);
        } else {
            parent_valid.push(true);
            for lvl in lvl_start..lvl_end {
                if traces_leaf.def_levels[lvl] != trace_max_def
                    || spans_leaf.def_levels[lvl] != span_max_def
                {
                    return Err(ParquetError::General(
                        "unexpected null link trace_id or span_id".to_owned(),
                    ));
                }
                let tb = traces_leaf
                    .values
                    .get(t_idx)
                    .ok_or_else(|| ParquetError::General("missing link trace_id".to_owned()))?;
                t_idx += 1;
                trace_ids.push(copy_fixed::<16>(tb.data())?);

                let sb = spans_leaf
                    .values
                    .get(s_idx)
                    .ok_or_else(|| ParquetError::General("missing link span_id".to_owned()))?;
                s_idx += 1;
                span_ids.push(copy_fixed::<8>(sb.data())?);

                if attrs_leaf.def_levels[lvl] == attr_max_def {
                    let ab = attrs_leaf
                        .values
                        .get(a_idx)
                        .ok_or_else(|| ParquetError::General("missing link attr".to_owned()))?;
                    a_idx += 1;
                    let ast = std::str::from_utf8(ab.data())
                        .map_err(|_| ParquetError::General("invalid UTF-8 link attr".to_owned()))?;
                    attr_bytes.extend_from_slice(ast.as_bytes());
                    attr_valid.push(true);
                } else {
                    attr_valid.push(false);
                }
                let aoff = u32::try_from(attr_bytes.len())
                    .map_err(|_| ParquetError::General("link attrs exceed u32::MAX".to_owned()))?;
                attr_offsets.push(aoff);
            }
        }
        let loff = u32::try_from(trace_ids.len())
            .map_err(|_| ParquetError::General("links exceed u32::MAX".to_owned()))?;
        offsets.push(loff);
    }
    if t_idx != traces_leaf.values.len()
        || s_idx != spans_leaf.values.len()
        || a_idx != attrs_leaf.values.len()
    {
        return Err(ParquetError::General("trailing links values".to_owned()));
    }

    let attributes = Utf8Data::try_new(attr_offsets, attr_bytes)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let attribute_nulls = if attr_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&attr_valid))
    };
    let links = LinksData::try_new(offsets, trace_ids, span_ids, attributes, attribute_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    let parent_nulls = if parent_valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&parent_valid))
    };
    let col = FlatColumn::try_new(FlatData::Links(Box::new(links)), parent_nulls)
        .map_err(|e| ParquetError::General(e.to_string()))?;
    Ok(Column::Flat(col))
}

fn repeated_def_bounds(
    root: &SchemaType,
    leaf_path: &[String],
) -> Result<(i16, i16), ParquetError> {
    let mut cur = root;
    let mut def = 0i16;
    let mut outer_non_null_def = None;
    let mut elem_present_def = None;
    for (depth, part) in leaf_path.iter().enumerate() {
        let child = match cur {
            SchemaType::GroupType { fields, .. } => fields
                .iter()
                .find(|f| f.name() == part)
                .ok_or_else(|| ParquetError::General("leaf path missing in schema".to_owned()))?,
            SchemaType::PrimitiveType { .. } => {
                return Err(ParquetError::General("invalid group traversal".to_owned()));
            }
        };
        let rep = child.get_basic_info().repetition();
        if rep != Repetition::REQUIRED {
            def += 1;
        }
        if depth == 0 {
            outer_non_null_def = Some(def);
        }
        if rep == Repetition::REPEATED && elem_present_def.is_none() {
            elem_present_def = Some(def);
        }
        cur = child;
    }
    Ok((
        outer_non_null_def.unwrap_or(0),
        elem_present_def
            .ok_or_else(|| ParquetError::General("expected REPEATED node".to_owned()))?,
    ))
}

enum ResolvedColumnPlan {
    MissingNull(Field),
    Scalar {
        leaf_idx: usize,
        data_type: DataType,
        dict_utf8: bool,
    },
    ListUtf8 {
        leaf_idx: usize,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
    MapUtf8 {
        key_leaf: usize,
        val_leaf: usize,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
    Events {
        time_leaf: usize,
        name_leaf: usize,
        attr_leaf: usize,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
    Links {
        trace_leaf: usize,
        span_leaf: usize,
        attr_leaf: usize,
        outer_non_null_def: i16,
        elem_present_def: i16,
    },
}

impl ResolvedColumnPlan {
    fn leaf_indices(&self) -> Vec<usize> {
        match self {
            Self::MissingNull(_) => Vec::new(),
            Self::Scalar { leaf_idx, .. } | Self::ListUtf8 { leaf_idx, .. } => vec![*leaf_idx],
            Self::MapUtf8 {
                key_leaf, val_leaf, ..
            } => vec![*key_leaf, *val_leaf],
            Self::Events {
                time_leaf,
                name_leaf,
                attr_leaf,
                ..
            } => vec![*time_leaf, *name_leaf, *attr_leaf],
            Self::Links {
                trace_leaf,
                span_leaf,
                attr_leaf,
                ..
            } => vec![*trace_leaf, *span_leaf, *attr_leaf],
        }
    }
}

fn resolve_column_plan(
    descr: &SchemaDescriptor,
    field: &Field,
) -> Result<ResolvedColumnPlan, ParquetError> {
    let leaves: Vec<usize> = descr
        .columns()
        .iter()
        .enumerate()
        .filter_map(|(idx, c)| {
            (c.path().parts().first().map(String::as_str) == Some(&field.name)).then_some(idx)
        })
        .collect();

    if leaves.is_empty() {
        if field.nullable {
            return Ok(ResolvedColumnPlan::MissingNull(field.clone()));
        }
        return Err(ParquetError::General(format!(
            "missing required column in parquet schema: {}",
            field.name
        )));
    }

    match &field.data_type {
        DataType::List(inner) if inner.as_ref() == &DataType::Utf8 => {
            if leaves.len() != 1 {
                return Err(ParquetError::General(
                    "expected 1 leaf for List(Utf8)".to_owned(),
                ));
            }
            let leaf_idx = leaves[0];
            let (outer_non_null_def, elem_present_def) =
                repeated_def_bounds(descr.root_schema(), descr.column(leaf_idx).path().parts())?;
            Ok(ResolvedColumnPlan::ListUtf8 {
                leaf_idx,
                outer_non_null_def,
                elem_present_def,
            })
        }
        DataType::MapUtf8 => {
            if leaves.len() != 2 {
                return Err(ParquetError::General(
                    "expected 2 leaves for MapUtf8".to_owned(),
                ));
            }
            let key_leaf = leaves
                .iter()
                .copied()
                .find(|&i| matches!(descr.column(i).name(), "key" | "keys"))
                .unwrap_or(leaves[0]);
            let val_leaf = leaves
                .iter()
                .copied()
                .find(|&i| matches!(descr.column(i).name(), "value" | "values"))
                .unwrap_or(leaves[1]);
            let (outer_non_null_def, elem_present_def) =
                repeated_def_bounds(descr.root_schema(), descr.column(key_leaf).path().parts())?;
            Ok(ResolvedColumnPlan::MapUtf8 {
                key_leaf,
                val_leaf,
                outer_non_null_def,
                elem_present_def,
            })
        }
        DataType::Events => {
            if leaves.len() != 3 {
                return Err(ParquetError::General(
                    "expected 3 leaves for Events".to_owned(),
                ));
            }
            let find = |name: &str, fallback: usize| {
                leaves
                    .iter()
                    .copied()
                    .find(|&i| descr.column(i).name() == name)
                    .unwrap_or(fallback)
            };
            let time_leaf = find(event_field::TIME, leaves[0]);
            let name_leaf = find(event_field::NAME, leaves[1]);
            let attr_leaf = find(event_field::ATTRIBUTES, leaves[2]);
            let (outer_non_null_def, elem_present_def) =
                repeated_def_bounds(descr.root_schema(), descr.column(time_leaf).path().parts())?;
            Ok(ResolvedColumnPlan::Events {
                time_leaf,
                name_leaf,
                attr_leaf,
                outer_non_null_def,
                elem_present_def,
            })
        }
        DataType::Links => {
            if leaves.len() != 3 {
                return Err(ParquetError::General(
                    "expected 3 leaves for Links".to_owned(),
                ));
            }
            let find = |name: &str, fallback: usize| {
                leaves
                    .iter()
                    .copied()
                    .find(|&i| descr.column(i).name() == name)
                    .unwrap_or(fallback)
            };
            let trace_leaf = find(link_field::TRACE_ID, leaves[0]);
            let span_leaf = find(link_field::SPAN_ID, leaves[1]);
            let attr_leaf = find(link_field::ATTRIBUTES, leaves[2]);
            let (outer_non_null_def, elem_present_def) =
                repeated_def_bounds(descr.root_schema(), descr.column(trace_leaf).path().parts())?;
            Ok(ResolvedColumnPlan::Links {
                trace_leaf,
                span_leaf,
                attr_leaf,
                outer_non_null_def,
                elem_present_def,
            })
        }
        dt => {
            if leaves.len() != 1 {
                return Err(ParquetError::General("expected 1 scalar leaf".to_owned()));
            }
            let leaf_idx = leaves[0];
            Ok(ResolvedColumnPlan::Scalar {
                leaf_idx,
                data_type: dt.clone(),
                dict_utf8: *dt == DataType::Utf8,
            })
        }
    }
}

type LeafPagePlan = (Vec<Range<u64>>, Option<Vec<PageLocation>>);
type LeafReader = (Option<Vec<PageLocation>>, Arc<SliceChunkReader>);

fn page_ranges_for_leaf(
    meta: &ParquetMetaData,
    rg_idx: usize,
    leaf_idx: usize,
    kept_rows: &[Range<usize>],
    file_size: u64,
) -> Result<LeafPagePlan, ParquetError> {
    let rg = meta.row_group(rg_idx);
    let col_chunk = rg.column(leaf_idx);
    let (chunk_start, chunk_len) = col_chunk.byte_range();
    let chunk_end = chunk_start
        .checked_add(chunk_len)
        .ok_or_else(|| ParquetError::General("column chunk byte range overflow".to_owned()))?;
    if chunk_end > file_size {
        return Err(ParquetError::General(
            "column chunk extends past file end".to_owned(),
        ));
    }

    let num_rows = checked_rg_num_rows(rg)?;

    let full_locs = meta
        .offset_index()
        .and_then(|oi| oi.get(rg_idx))
        .and_then(|rg_oi| rg_oi.get(leaf_idx))
        .map(|col_oi| col_oi.page_locations.clone());

    let Some(locs) = full_locs else {
        return Ok((std::iter::once(chunk_start..chunk_end).collect(), None));
    };

    if locs.is_empty() {
        return Ok((std::iter::once(chunk_start..chunk_end).collect(), None));
    }

    validate_page_locations(&locs, num_rows, file_size)?;
    let p_ranges = page_row_ranges(&locs, num_rows);
    let mut byte_ranges = Vec::new();
    let first_data_off = locs[0].offset as u64;
    if first_data_off < chunk_start
        || first_data_off > chunk_end
        || first_data_off - chunk_start > i32::MAX as u64
    {
        return Err(ParquetError::General(
            "invalid first data page offset in offset index".to_owned(),
        ));
    }
    if first_data_off > chunk_start {
        byte_ranges.push(chunk_start..first_data_off);
    }
    for (p_idx, p_range) in p_ranges.iter().enumerate() {
        let overlaps = kept_rows
            .iter()
            .any(|kr| kr.start < p_range.end && kr.end > p_range.start);
        if overlaps {
            let loc = &locs[p_idx];
            let s = loc.offset as u64;
            let e = s + loc.compressed_page_size as u64;
            if s < chunk_start || e > chunk_end {
                return Err(ParquetError::General(
                    "page byte range outside column chunk".to_owned(),
                ));
            }
            byte_ranges.push(s..e);
        }
    }
    Ok((coalesce_ranges(byte_ranges), Some(locs)))
}

fn open_page_reader(
    meta: &ParquetMetaData,
    rg_idx: usize,
    leaf_idx: usize,
    page_locations: Option<Vec<PageLocation>>,
    reader: Arc<SliceChunkReader>,
) -> Result<SerializedPageReader<SliceChunkReader>, ParquetError> {
    let rg = meta.row_group(rg_idx);
    let col_chunk = rg.column(leaf_idx);
    let total_rows = checked_rg_num_rows(rg)?;
    SerializedPageReader::new_with_properties(
        reader,
        col_chunk,
        total_rows,
        page_locations,
        Arc::new(ReaderProperties::builder().build()),
    )
}

fn open_scalar_reader(
    descr: &ColumnDescPtr,
    page_reader: SerializedPageReader<SliceChunkReader>,
) -> Result<ScalarReader, ParquetError> {
    match get_column_reader(descr.clone(), Box::new(page_reader)) {
        ColumnReader::BoolColumnReader(r) => Ok(ScalarReader::Bool(r)),
        ColumnReader::Int32ColumnReader(r) => Ok(ScalarReader::Int32(r)),
        ColumnReader::Int64ColumnReader(r) => Ok(ScalarReader::Int64(r)),
        ColumnReader::FloatColumnReader(r) => Ok(ScalarReader::Float(r)),
        ColumnReader::DoubleColumnReader(r) => Ok(ScalarReader::Double(r)),
        ColumnReader::ByteArrayColumnReader(r) => Ok(ScalarReader::ByteArray(r)),
        ColumnReader::FixedLenByteArrayColumnReader(r) => Ok(ScalarReader::FixedLen(r)),
        ColumnReader::Int96ColumnReader(_) => Err(ParquetError::General(
            "INT96 parquet columns are not supported".to_owned(),
        )),
    }
}

fn open_byte_array_reader(
    descr: &ColumnDescPtr,
    page_reader: SerializedPageReader<SliceChunkReader>,
) -> Result<ColumnReaderImpl<ByteArrayType>, ParquetError> {
    match get_column_reader(descr.clone(), Box::new(page_reader)) {
        ColumnReader::ByteArrayColumnReader(r) => Ok(r),
        _ => Err(ParquetError::General(
            "expected ByteArray column reader".to_owned(),
        )),
    }
}

fn open_int64_reader(
    descr: &ColumnDescPtr,
    page_reader: SerializedPageReader<SliceChunkReader>,
) -> Result<ColumnReaderImpl<Int64Type>, ParquetError> {
    match get_column_reader(descr.clone(), Box::new(page_reader)) {
        ColumnReader::Int64ColumnReader(r) => Ok(r),
        _ => Err(ParquetError::General(
            "expected Int64 column reader".to_owned(),
        )),
    }
}

fn open_fixed_reader(
    descr: &ColumnDescPtr,
    page_reader: SerializedPageReader<SliceChunkReader>,
) -> Result<ColumnReaderImpl<FixedLenByteArrayType>, ParquetError> {
    match get_column_reader(descr.clone(), Box::new(page_reader)) {
        ColumnReader::FixedLenByteArrayColumnReader(r) => Ok(r),
        _ => Err(ParquetError::General(
            "expected FixedLenByteArray column reader".to_owned(),
        )),
    }
}

fn build_column_decoder(
    meta: &ParquetMetaData,
    rg_idx: usize,
    plan: &ResolvedColumnPlan,
    leaf_readers: &BTreeMap<usize, LeafReader>,
) -> Result<ColumnDecoder, ParquetError> {
    let rg_num_rows = checked_rg_num_rows(meta.row_group(rg_idx))?;
    let descr = meta.file_metadata().schema_descr_ptr();
    let get_leaf = |leaf_idx: usize| -> Result<
        (ColumnDescPtr, SerializedPageReader<SliceChunkReader>),
        ParquetError,
    > {
        let (locs, reader) = leaf_readers
            .get(&leaf_idx)
            .ok_or_else(|| ParquetError::General("missing leaf byte reader".to_owned()))?;
        let pr = open_page_reader(meta, rg_idx, leaf_idx, locs.clone(), reader.clone())?;
        Ok((descr.column(leaf_idx), pr))
    };

    match plan {
        ResolvedColumnPlan::MissingNull(field) => Ok(ColumnDecoder::MissingNull(field.clone())),
        ResolvedColumnPlan::Scalar {
            leaf_idx,
            data_type,
            dict_utf8,
        } => {
            let (col_descr, pr) = get_leaf(*leaf_idx)?;
            if *dict_utf8 {
                Ok(ColumnDecoder::Utf8Dict(DictUtf8LeafDecoder::new(
                    col_descr,
                    pr,
                    rg_num_rows,
                )))
            } else {
                let reader = open_scalar_reader(&col_descr, pr)?;
                Ok(ColumnDecoder::Scalar {
                    descr: col_descr,
                    data_type: data_type.clone(),
                    reader,
                })
            }
        }
        ResolvedColumnPlan::ListUtf8 {
            leaf_idx,
            outer_non_null_def,
            elem_present_def,
        } => {
            let (col_descr, pr) = get_leaf(*leaf_idx)?;
            Ok(ColumnDecoder::ListUtf8Dict {
                decoder: DictUtf8LeafDecoder::new(col_descr, pr, rg_num_rows),
                outer_non_null_def: *outer_non_null_def,
                elem_present_def: *elem_present_def,
            })
        }
        ResolvedColumnPlan::MapUtf8 {
            key_leaf,
            val_leaf,
            outer_non_null_def,
            elem_present_def,
        } => {
            let (kd, kpr) = get_leaf(*key_leaf)?;
            let (vd, vpr) = get_leaf(*val_leaf)?;
            Ok(ColumnDecoder::MapUtf8 {
                key_reader: open_byte_array_reader(&kd, kpr)?,
                key_descr: kd,
                val_reader: open_byte_array_reader(&vd, vpr)?,
                val_descr: vd,
                outer_non_null_def: *outer_non_null_def,
                elem_present_def: *elem_present_def,
            })
        }
        ResolvedColumnPlan::Events {
            time_leaf,
            name_leaf,
            attr_leaf,
            outer_non_null_def,
            elem_present_def,
        } => {
            let (td, tpr) = get_leaf(*time_leaf)?;
            let (nd, npr) = get_leaf(*name_leaf)?;
            let (ad, apr) = get_leaf(*attr_leaf)?;
            Ok(ColumnDecoder::Events {
                time_reader: open_int64_reader(&td, tpr)?,
                time_descr: td,
                name_reader: open_byte_array_reader(&nd, npr)?,
                name_descr: nd,
                attr_reader: open_byte_array_reader(&ad, apr)?,
                attr_descr: ad,
                outer_non_null_def: *outer_non_null_def,
                elem_present_def: *elem_present_def,
            })
        }
        ResolvedColumnPlan::Links {
            trace_leaf,
            span_leaf,
            attr_leaf,
            outer_non_null_def,
            elem_present_def,
        } => {
            let (td, tpr) = get_leaf(*trace_leaf)?;
            let (sd, spr) = get_leaf(*span_leaf)?;
            let (ad, apr) = get_leaf(*attr_leaf)?;
            Ok(ColumnDecoder::Links {
                trace_reader: open_fixed_reader(&td, tpr)?,
                trace_descr: td,
                span_reader: open_fixed_reader(&sd, spr)?,
                span_descr: sd,
                attr_reader: open_byte_array_reader(&ad, apr)?,
                attr_descr: ad,
                outer_non_null_def: *outer_non_null_def,
                elem_present_def: *elem_present_def,
            })
        }
    }
}

struct ActiveRowGroup {
    rg_idx: usize,
    kept_ranges: Vec<Range<usize>>,
    range_idx: usize,
    range_offset: usize,
    phase1_indices: Vec<usize>,
    phase1_plans: Vec<ResolvedColumnPlan>,
    phase1_decoders: Option<Vec<ColumnDecoder>>,
    phase1_row: usize,
    phase2_indices: Vec<usize>,
    phase2_plans: Vec<ResolvedColumnPlan>,
    phase2_decoders: Option<Vec<ColumnDecoder>>,
    phase2_row: usize,
}

impl ActiveRowGroup {
    fn next_chunk(&mut self, batch_size: usize) -> Option<Vec<Range<usize>>> {
        let cap = batch_size.max(1);
        let mut chunk = Vec::new();
        let mut remaining = cap;
        while self.range_idx < self.kept_ranges.len() && remaining > 0 {
            let r = &self.kept_ranges[self.range_idx];
            let pos = r.start + self.range_offset;
            if pos >= r.end {
                self.range_idx += 1;
                self.range_offset = 0;
                continue;
            }
            let avail = r.end - pos;
            let take = remaining.min(avail);
            chunk.push(pos..pos + take);
            remaining -= take;
            if take == avail {
                self.range_idx += 1;
                self.range_offset = 0;
            } else {
                self.range_offset += take;
            }
        }
        if chunk.is_empty() { None } else { Some(chunk) }
    }
}

struct ActiveFileState {
    file: Arc<FileMeta>,
    meta: Arc<ParquetMetaData>,
    phase1_schema: Arc<Schema>,
    phase1_filters: Vec<Expr>,
    tombstone_pred: Option<Expr>,
    row_groups: VecDeque<(usize, Vec<Range<usize>>)>,
    current_rg: Option<ActiveRowGroup>,
    phase1_indices: Vec<usize>,
    phase2_indices: Vec<usize>,
}

struct ParquetPartitionStream {
    store: Arc<dyn ObjectStore>,
    table: FileTable,
    schema: Arc<Schema>,
    files: VecDeque<Arc<FileMeta>>,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<MetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
    plan: Arc<ScanPlan>,
    batch_size: usize,
    now_ns: i64,
    active_file: Option<ActiveFileState>,
}

impl ParquetPartitionStream {
    fn open_next_file(&mut self, cancel: &CancelToken) -> Result<bool, QueryError> {
        let Some(file) = self.files.pop_front() else {
            return Ok(false);
        };

        let min_ingest = if self.table == FileTable::Traces {
            i64::MIN
        } else {
            file.min_ingest_time
        };
        let app_del = applicable(
            &self.deletions,
            Some(file.deletions_applied_seq),
            min_ingest,
        );
        let tombstone_pred = if app_del.is_empty() {
            None
        } else {
            let specs = to_tombstone_specs(&app_del);
            covered_predicate(tombstone_table(self.table), &self.schema, &specs)?
        };

        let meta = load_metadata(
            &self.store,
            &file,
            &self.metadata_cache,
            &self.suspects,
            cancel,
            &self.runtime,
        )?;

        let row_groups = filter_row_groups(
            &self.store,
            &file,
            &meta,
            &self.plan,
            &self.suspects,
            cancel,
            &self.runtime,
        )?;

        let mut phase1_set = self.plan.filter_cols.clone();
        if let Some(tp) = &tombstone_pred {
            tp.referenced_columns(&mut phase1_set);
        }
        let phase1_indices: Vec<usize> = phase1_set.iter().copied().collect();
        let phase1_schema = Arc::new(self.schema.project(&phase1_indices)?);
        let mut col_map = vec![None; self.schema.len()];
        for (new_idx, &old_idx) in phase1_indices.iter().enumerate() {
            if let Some(slot) = col_map.get_mut(old_idx) {
                *slot = Some(new_idx);
            }
        }
        let phase1_filters = self
            .plan
            .filters
            .iter()
            .map(|f| f.remap_columns(&col_map))
            .collect::<Result<Vec<_>, _>>()?;
        let remapped_tombstone = tombstone_pred
            .as_ref()
            .map(|tp| tp.remap_columns(&col_map))
            .transpose()?;

        let phase2_indices: Vec<usize> = self
            .plan
            .output_cols
            .iter()
            .copied()
            .filter(|idx| !phase1_set.contains(idx))
            .collect();

        self.active_file = Some(ActiveFileState {
            file,
            meta,
            phase1_schema,
            phase1_filters,
            tombstone_pred: remapped_tombstone,
            row_groups: row_groups.into(),
            current_rg: None,
            phase1_indices,
            phase2_indices,
        });
        Ok(true)
    }

    fn prepare_next_row_group(&mut self, cancel: &CancelToken) -> Result<bool, QueryError> {
        let Some(af) = self.active_file.as_mut() else {
            return Ok(false);
        };
        let descr = af.meta.file_metadata().schema_descr_ptr();

        while let Some((rg_idx, kept_ranges)) = af.row_groups.pop_front() {
            if !kept_ranges.iter().any(|r| !r.is_empty()) {
                continue;
            }

            let phase1_plans = guard_parquet(af.file.id, &self.suspects, || {
                af.phase1_indices
                    .iter()
                    .map(|&idx| resolve_column_plan(&descr, &self.schema.fields()[idx]))
                    .collect::<Result<Vec<_>, _>>()
            })?;

            let phase2_plans = guard_parquet(af.file.id, &self.suspects, || {
                af.phase2_indices
                    .iter()
                    .map(|&idx| resolve_column_plan(&descr, &self.schema.fields()[idx]))
                    .collect::<Result<Vec<_>, _>>()
            })?;

            let mut rg_state = ActiveRowGroup {
                rg_idx,
                kept_ranges,
                range_idx: 0,
                range_offset: 0,
                phase1_indices: af.phase1_indices.clone(),
                phase1_plans,
                phase1_decoders: None,
                phase1_row: 0,
                phase2_indices: af.phase2_indices.clone(),
                phase2_plans,
                phase2_decoders: None,
                phase2_row: 0,
            };

            if !rg_state.phase1_plans.is_empty() {
                let mut leaf_indices = Vec::new();
                for p in &rg_state.phase1_plans {
                    leaf_indices.extend(p.leaf_indices());
                }
                let readers = fetch_leaf_readers(
                    &self.store,
                    &self.suspects,
                    &self.runtime,
                    &af.file,
                    &af.meta,
                    rg_idx,
                    &leaf_indices,
                    &rg_state.kept_ranges,
                    cancel,
                )?;
                let decoders = guard_parquet(af.file.id, &self.suspects, || {
                    rg_state
                        .phase1_plans
                        .iter()
                        .map(|p| build_column_decoder(&af.meta, rg_idx, p, &readers))
                        .collect::<Result<Vec<_>, _>>()
                })?;
                rg_state.phase1_decoders = Some(decoders);
            }

            af.current_rg = Some(rg_state);
            return Ok(true);
        }

        Ok(false)
    }
}

#[allow(clippy::too_many_arguments)]
fn fetch_leaf_readers(
    store: &Arc<dyn ObjectStore>,
    suspects: &Suspects,
    runtime: &tokio::runtime::Handle,
    file: &FileMeta,
    meta: &ParquetMetaData,
    rg_idx: usize,
    leaf_indices: &[usize],
    kept_ranges: &[Range<usize>],
    cancel: &CancelToken,
) -> Result<BTreeMap<usize, LeafReader>, QueryError> {
    let mut unique_leaves = BTreeSet::new();
    for &l in leaf_indices {
        unique_leaves.insert(l);
    }

    let mut per_leaf_plan: Vec<(usize, LeafPagePlan)> = Vec::with_capacity(unique_leaves.len());
    let mut all_ranges = Vec::new();
    for leaf_idx in unique_leaves {
        let (ranges, locs) = guard_parquet(file.id, suspects, || {
            page_ranges_for_leaf(meta, rg_idx, leaf_idx, kept_ranges, file.size)
        })?;
        all_ranges.extend(ranges.iter().cloned());
        per_leaf_plan.push((leaf_idx, (ranges, locs)));
    }

    let coalesced = coalesce_ranges(all_ranges);
    let fetched = if coalesced.is_empty() {
        Vec::new()
    } else {
        let path = ObjectPath::from(file.path.as_str());
        let bufs = fetch_ranges(store, &path, &coalesced, cancel, runtime)
            .map_err(|e| map_fetch_err(&e, file.id, suspects))?;
        if bufs.len() != coalesced.len()
            || bufs
                .iter()
                .zip(&coalesced)
                .any(|(b, r)| b.len() as u64 != r.end - r.start)
        {
            return Err(corrupt_err(file.id, suspects));
        }
        bufs
    };
    let chunk_reader = Arc::new(SliceChunkReader::from_fetched(
        file.size, coalesced, fetched,
    ));

    let mut out = BTreeMap::new();
    for (leaf_idx, (_, locs)) in per_leaf_plan {
        out.insert(leaf_idx, (locs, Arc::clone(&chunk_reader)));
    }
    Ok(out)
}

fn guard_parquet<T, F>(file_id: FileId, suspects: &Suspects, f: F) -> Result<T, QueryError>
where
    F: FnOnce() -> Result<T, ParquetError>,
{
    guard_sync(f).map_err(|_| corrupt_err(file_id, suspects))
}

impl PartitionStream for ParquetPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_deref().map_or(0, Reservation::size);
        let eval_ctx = EvalContext {
            now_ns: self.now_ns,
            cancel: cancel.clone(),
        };

        loop {
            if cancel.is_cancelled() {
                rollback_res(res.as_deref_mut(), before);
                return Err(QueryError::Cancelled);
            }

            let has_rg = self
                .active_file
                .as_ref()
                .is_some_and(|af| af.current_rg.is_some());
            if !has_rg {
                if self.active_file.is_some()
                    && match self.prepare_next_row_group(cancel) {
                        Ok(found) => found,
                        Err(e) => {
                            rollback_res(res.as_deref_mut(), before);
                            return Err(e);
                        }
                    }
                {
                    continue;
                }
                self.active_file = None;
                match self.open_next_file(cancel) {
                    Ok(true) => continue,
                    Ok(false) => {
                        rollback_res(res.as_deref_mut(), before);
                        return Ok(None);
                    }
                    Err(e) => {
                        rollback_res(res.as_deref_mut(), before);
                        return Err(e);
                    }
                }
            }

            let batch_size = self.batch_size;
            let chunk_opt = self
                .active_file
                .as_mut()
                .and_then(|f| f.current_rg.as_mut())
                .and_then(|rg| rg.next_chunk(batch_size));
            let Some(chunk_ranges) = chunk_opt else {
                if let Some(af) = self.active_file.as_mut() {
                    af.current_rg = None;
                }
                continue;
            };
            let raw_len: usize = chunk_ranges.iter().map(Range::len).sum();
            if raw_len == 0 {
                continue;
            }

            let (
                file_id,
                meta,
                rg_idx,
                kept_ranges,
                phase1_schema,
                phase1_filters,
                tombstone_pred,
                has_phase1,
            ) = {
                let Some(af) = self.active_file.as_ref() else {
                    continue;
                };
                let Some(rg) = af.current_rg.as_ref() else {
                    continue;
                };
                (
                    af.file.id,
                    Arc::clone(&af.meta),
                    rg.rg_idx,
                    rg.kept_ranges.clone(),
                    Arc::clone(&af.phase1_schema),
                    af.phase1_filters.clone(),
                    af.tombstone_pred.clone(),
                    rg.phase1_decoders.is_some(),
                )
            };

            let (sel, mut p1_map) = if has_phase1 {
                let (p1_indices, p1_cols) = {
                    let Some(af) = self.active_file.as_mut() else {
                        continue;
                    };
                    let Some(rg) = af.current_rg.as_mut() else {
                        continue;
                    };
                    let Some(decs) = rg.phase1_decoders.as_mut() else {
                        continue;
                    };
                    let mut row_cursor = rg.phase1_row;
                    let cols = match guard_parquet(file_id, &self.suspects, || {
                        let mut out = Vec::with_capacity(decs.len());
                        for dec in decs {
                            let mut r = rg.phase1_row;
                            let c = dec.read_ranges(&chunk_ranges, &mut r)?;
                            row_cursor = r;
                            out.push(c);
                        }
                        Ok(out)
                    }) {
                        Ok(c) => c,
                        Err(e) => {
                            rollback_res(res.as_deref_mut(), before);
                            return Err(e);
                        }
                    };
                    rg.phase1_row = row_cursor;
                    (rg.phase1_indices.clone(), cols)
                };

                let p1_batch = match Batch::try_new(phase1_schema, p1_cols, raw_len, Selection::All)
                {
                    Ok(b) => b,
                    Err(_) => {
                        rollback_res(res.as_deref_mut(), before);
                        return Err(corrupt_err(file_id, &self.suspects));
                    }
                };
                reconcile_res(res.as_deref_mut(), before, p1_batch.memory_bytes())?;
                let filtered = apply_tombstones_and_filters(
                    p1_batch,
                    tombstone_pred.as_ref(),
                    &phase1_filters,
                    &eval_ctx,
                    res.as_deref_mut(),
                    before,
                )?;
                if filtered.is_empty() {
                    reconcile_res(res.as_deref_mut(), before, 0)?;
                    continue;
                }
                let (_, filtered_cols, _, sel) = filtered.into_parts();
                let mut p1_map = BTreeMap::new();
                for (col_idx, col) in p1_indices.into_iter().zip(filtered_cols) {
                    p1_map.insert(col_idx, col);
                }
                (sel, p1_map)
            } else {
                (Selection::All, BTreeMap::new())
            };

            let need_phase2_fetch = {
                let Some(af) = self.active_file.as_ref() else {
                    continue;
                };
                let Some(rg) = af.current_rg.as_ref() else {
                    continue;
                };
                !rg.phase2_plans.is_empty() && rg.phase2_decoders.is_none()
            };

            if need_phase2_fetch {
                let (file_copy, leaf_indices) = {
                    let Some(af) = self.active_file.as_ref() else {
                        continue;
                    };
                    let Some(rg) = af.current_rg.as_ref() else {
                        continue;
                    };
                    let mut leaves = Vec::new();
                    for p in &rg.phase2_plans {
                        leaves.extend(p.leaf_indices());
                    }
                    (Arc::clone(&af.file), leaves)
                };

                let readers = match fetch_leaf_readers(
                    &self.store,
                    &self.suspects,
                    &self.runtime,
                    &file_copy,
                    &meta,
                    rg_idx,
                    &leaf_indices,
                    &kept_ranges,
                    cancel,
                ) {
                    Ok(r) => r,
                    Err(e) => {
                        rollback_res(res.as_deref_mut(), before);
                        return Err(e);
                    }
                };

                let Some(af) = self.active_file.as_mut() else {
                    continue;
                };
                let Some(rg) = af.current_rg.as_mut() else {
                    continue;
                };
                let decs = match guard_parquet(file_id, &self.suspects, || {
                    rg.phase2_plans
                        .iter()
                        .map(|p| build_column_decoder(&meta, rg_idx, p, &readers))
                        .collect::<Result<Vec<_>, _>>()
                }) {
                    Ok(d) => d,
                    Err(e) => {
                        rollback_res(res.as_deref_mut(), before);
                        return Err(e);
                    }
                };
                rg.phase2_decoders = Some(decs);
            }

            let mut p2_map = BTreeMap::new();
            {
                let Some(af) = self.active_file.as_mut() else {
                    continue;
                };
                let Some(rg) = af.current_rg.as_mut() else {
                    continue;
                };
                if let Some(p2_decs) = rg.phase2_decoders.as_mut() {
                    let mut row_cursor = rg.phase2_row;
                    let decoded_p2 = match guard_parquet(file_id, &self.suspects, || {
                        let mut out = Vec::with_capacity(p2_decs.len());
                        for dec in p2_decs {
                            let mut r = rg.phase2_row;
                            let c = dec.read_ranges(&chunk_ranges, &mut r)?;
                            row_cursor = r;
                            out.push(c);
                        }
                        Ok(out)
                    }) {
                        Ok(v) => v,
                        Err(e) => {
                            rollback_res(res.as_deref_mut(), before);
                            return Err(e);
                        }
                    };
                    rg.phase2_row = row_cursor;
                    for (&idx, col) in rg.phase2_indices.iter().zip(decoded_p2) {
                        p2_map.insert(idx, col);
                    }
                }
            }

            let proj_schema = Arc::clone(&self.plan.projected_schema);
            let assembled = match &self.plan.projection {
                Some(indices) => {
                    let mut cols = Vec::with_capacity(indices.len());
                    let mut first_pos: BTreeMap<usize, usize> = BTreeMap::new();
                    for &idx in indices {
                        if let Some(c) = p1_map.remove(&idx).or_else(|| p2_map.remove(&idx)) {
                            first_pos.insert(idx, cols.len());
                            cols.push(c);
                        } else if let Some(&pos) = first_pos.get(&idx) {
                            cols.push(cols[pos].clone());
                        } else {
                            rollback_res(res.as_deref_mut(), before);
                            return Err(QueryError::internal("missing projected column slot"));
                        }
                    }
                    cols
                }
                None => {
                    let mut cols = Vec::with_capacity(self.schema.len());
                    for idx in 0..self.schema.len() {
                        if let Some(c) = p1_map.remove(&idx).or_else(|| p2_map.remove(&idx)) {
                            cols.push(c);
                        } else {
                            rollback_res(res.as_deref_mut(), before);
                            return Err(QueryError::internal("missing projected column slot"));
                        }
                    }
                    cols
                }
            };

            let batch = match Batch::try_new(proj_schema, assembled, raw_len, sel) {
                Ok(b) => b,
                Err(_) => {
                    rollback_res(res.as_deref_mut(), before);
                    return Err(corrupt_err(file_id, &self.suspects));
                }
            };
            reconcile_res(res.as_deref_mut(), before, batch.memory_bytes())?;
            return Ok(Some(batch));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use arrow::array::Array;
    use arrow::record_batch::RecordBatch;
    use async_trait::async_trait;
    use cetra_column::{Batch, Column, DataType, Field, GlobalPool, ScalarValue, Schema};
    use cetra_core::enums::{Convention, SpanStatus};
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::schema::{scores_schema, spans, spans_schema, traces_schema};
    use cetra_core::span::{SpanEvent, SpanLink};
    use cetra_query::exec::{
        CollectSink, ExecContext, ExecMetrics, IdLookupHint, ScanSource, ScanSpec, SpillManager,
        WorkerPool, run_pipeline,
    };
    use cetra_query::{BinaryOp, Expr, QueryError};
    use chrono::NaiveDate;
    use futures::stream::BoxStream;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;

    use super::*;
    use crate::catalog::{FileId, FileMeta, FileTable};
    use crate::engine::day_start_ns;
    use crate::file_format::{footer_size_from_tail, write_file_batch};
    use crate::test_support::{score_row, score_rows, span_row, span_rows, trace_batch};
    use crate::verify::{Suspects, VerifyQueue};

    /// Reads one stored Parquet object (`spans`, `traces`, or `scores`) into a single `Batch`.
    pub(crate) fn read_stored_parquet(
        bytes: Bytes,
        table: FileTable,
        schema: Arc<Schema>,
    ) -> Batch {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("rt");
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let size = bytes.len() as u64;
            let meta = parquet::file::metadata::ParquetMetaDataReader::new()
                .parse_and_finish(&bytes)
                .ok();
            let num_rows = meta
                .as_ref()
                .map(|m| u64::try_from(m.file_metadata().num_rows()).unwrap_or(0))
                .unwrap_or(1);
            let footer_size = if bytes.len() >= 8 {
                let tail: [u8; 8] = bytes[bytes.len() - 8..].try_into().expect("tail");
                footer_size_from_tail(&tail, size).unwrap_or(8)
            } else {
                8
            };
            rt.block_on(store.put(&ObjectPath::from("test.parquet"), PutPayload::from(bytes)))
                .expect("put");
            let fm = Arc::new(FileMeta {
                id: FileId::new_v7(),
                table,
                project: "018f6b4e-7c1a-7c1a-8c1a-000000000001"
                    .parse()
                    .expect("project"),
                day: NaiveDate::from_ymd_opt(2026, 1, 1).expect("day"),
                level: 0,
                path: "test.parquet".to_owned(),
                size,
                footer_size: footer_size.max(8),
                rows: num_rows,
                min_time: i64::MIN,
                max_time: i64::MAX,
                min_ingest_time: i64::MIN,
                max_ingest_time: i64::MAX,
                fully_merged: false,
                deletions_applied_seq: 0,
            });
            let cache = Arc::new(MetadataCache::new(1024 * 1024));
            let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
            let source = ParquetSource::new(
                store,
                table,
                Arc::clone(&schema),
                vec![fm],
                Arc::from([]),
                cache,
                suspects,
                rt.handle().clone(),
            );
            let exec = test_ctx(512 * 1024 * 1024);
            let batches = drain_on_workers(&source, &ScanSpec::default(), &exec).expect("scan");
            if batches.is_empty() {
                return Batch::empty(schema);
            }
            let refs: Vec<&Batch> = batches.iter().collect();
            Batch::concat(schema, &refs, None).expect("concat")
        })
        .join()
        .expect("parquet reader thread")
    }

    #[derive(Debug)]
    struct CountingStore {
        inner: Arc<dyn ObjectStore>,
        byte_ranges_fetched: AtomicUsize,
        transient_fail: AtomicBool,
        hang_get_ranges: AtomicBool,
    }

    impl CountingStore {
        fn new(inner: Arc<dyn ObjectStore>) -> Self {
            Self {
                inner,
                byte_ranges_fetched: AtomicUsize::new(0),
                transient_fail: AtomicBool::new(false),
                hang_get_ranges: AtomicBool::new(false),
            }
        }
    }

    impl fmt::Display for CountingStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "CountingStore")
        }
    }

    #[async_trait]
    impl ObjectStore for CountingStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if self.transient_fail.load(Ordering::Relaxed) {
                return Err(object_store::Error::Generic {
                    store: "counting",
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "transient timeout",
                    )),
                });
            }
            self.inner.get_opts(location, options).await
        }

        async fn get_ranges(
            &self,
            location: &ObjectPath,
            ranges: &[Range<u64>],
        ) -> object_store::Result<Vec<Bytes>> {
            if self.hang_get_ranges.load(Ordering::Relaxed) {
                futures::future::pending::<()>().await;
            }
            if self.transient_fail.load(Ordering::Relaxed) {
                return Err(object_store::Error::Generic {
                    store: "counting",
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "transient timeout",
                    )),
                });
            }
            let bytes: usize = ranges.iter().map(|r| (r.end - r.start) as usize).sum();
            self.byte_ranges_fetched.fetch_add(bytes, Ordering::Relaxed);
            self.inner.get_ranges(location, ranges).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<ObjectPath>>,
        ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    fn test_ctx(cap: usize) -> ExecContext {
        let dir = tempfile::tempdir().expect("tempdir");
        ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: EvalContext {
                now_ns: 0,
                cancel: CancelToken::new(),
            },
            spill: Arc::new(SpillManager::with_fixed_cap(dir.keep(), 1024 * 1024)),
            workers: Arc::new(WorkerPool::new(2).expect("worker pool")),
            batch_size: 8192,
            target_partitions: 2,
            metrics: ExecMetrics::default(),
        }
    }

    fn drain_on_workers(
        source: &ParquetSource,
        spec: &ScanSpec,
        exec: &ExecContext,
    ) -> Result<Vec<Batch>, QueryError> {
        let partitions = source.open_partitions(spec, exec)?;
        if partitions.is_empty() {
            return Ok(Vec::new());
        }
        let pool = Arc::clone(&exec.pool);
        let sinks = run_pipeline(
            exec,
            partitions,
            |_| Ok(Vec::new()),
            move |_| Ok(CollectSink::new(&pool)),
        )?;
        Ok(sinks
            .into_iter()
            .flat_map(CollectSink::into_batches)
            .collect())
    }

    fn write_parquet_into_store(
        rt: &tokio::runtime::Runtime,
        store: &Arc<dyn ObjectStore>,
        path: &str,
        file_id: FileId,
        table: FileTable,
        day: NaiveDate,
        batch: Batch,
    ) -> Arc<FileMeta> {
        let project: cetra_core::ids::ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001"
            .parse()
            .expect("project");
        let written = write_file_batch(table, 0, &[Arc::new(batch)]).expect("write_file");
        let size = written.bytes.len() as u64;
        let obj_path = ObjectPath::from(path);
        rt.block_on(store.put(&obj_path, PutPayload::from(written.bytes)))
            .expect("put parquet");

        Arc::new(FileMeta {
            id: file_id,
            table,
            project,
            day,
            level: 0,
            path: path.to_owned(),
            size,
            footer_size: written.footer_size.max(8),
            rows: written.rows,
            min_time: written.min_time,
            max_time: written.max_time,
            min_ingest_time: written.min_ingest_time,
            max_ingest_time: written.max_ingest_time,
            fully_merged: false,
            deletions_applied_seq: 0,
        })
    }

    #[test]
    fn reads_all_stored_types_prunes_bloom_and_schema_evolves() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        let mem_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Arc::new(CountingStore::new(mem_store));
        let obj_store: Arc<dyn ObjectStore> = store;
        let cache = Arc::new(MetadataCache::new(1024 * 1024));
        let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

        let day = NaiveDate::from_ymd_opt(2026, 3, 15).expect("day");
        let base_ns = day_start_ns(day).expect("day start");

        // 1. Spans file covering DictColumn (`status`), List(Utf8) (`conventions`),
        // UInt16 (`schema_version`), MapUtf8 (`attributes`), Events, Links, FixedBytes8/16.
        let mut s1 = span_row(1, 0, base_ns + 1_000, base_ns + 2_000);
        s1.status = SpanStatus::Ok;
        s1.conventions = vec![Convention::Genai, Convention::Cetra];
        s1.schema_version = 42;
        s1.attributes.insert("env".to_owned(), "prod".to_owned());
        s1.events.push(SpanEvent {
            time: base_ns + 1_500,
            name: "token".to_owned(),
            attributes: Some("{\"n\":1}".to_owned()),
        });
        s1.links.push(SpanLink {
            trace_id: TraceId::from_bytes([0xBB; 16]).expect("trace"),
            span_id: SpanId::from_bytes([0x09; 8]).expect("span"),
            attributes: Some("{}".to_owned()),
        });

        let mut s2 = span_row(2, 1, base_ns + 5_000, base_ns + 6_000);
        s2.status = SpanStatus::Error;
        s2.conventions = vec![Convention::Openinference];
        s2.schema_version = 7;

        let trace1_bytes = *s1.trace_id.as_bytes();
        let span_rb = span_rows(&[s1.clone(), s2.clone()]);
        let span_schema = spans_schema();
        let span_file = write_parquet_into_store(
            &rt,
            &obj_store,
            "spans/part1.parquet",
            FileId::new_v7(),
            FileTable::Spans,
            day,
            span_rb,
        );

        let source = ParquetSource::new(
            obj_store.clone(),
            FileTable::Spans,
            Arc::clone(&span_schema),
            vec![Arc::clone(&span_file)],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );

        let mut exec = test_ctx(64 * 1024 * 1024);
        exec.batch_size = 128;

        let batches =
            drain_on_workers(&source, &ScanSpec::default(), &exec).expect("scan all span types");
        let total_rows: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total_rows, 2);
        let b = &batches[0];
        let status_idx = span_schema.index_of(spans::STATUS).expect("status col");
        assert!(matches!(b.columns()[status_idx], Column::Dict(_)));
        let sv_idx = span_schema
            .index_of(spans::SCHEMA_VERSION)
            .expect("schema_version");
        assert_eq!(
            b.columns()[sv_idx].get_scalar(0).expect("sv"),
            ScalarValue::I64(42)
        );
        let conv_idx = span_schema
            .index_of(spans::CONVENTIONS)
            .expect("conventions");
        assert_ne!(
            b.columns()[conv_idx].get_scalar(0).expect("conv"),
            ScalarValue::Null(DataType::List(Box::new(DataType::Utf8)))
        );

        // Empty projection (`Some(vec![])`) returns row counts with minimal I/O.
        let empty_spec = ScanSpec {
            projection: Some(Vec::new()),
            ..Default::default()
        };
        let empty_batches =
            drain_on_workers(&source, &empty_spec, &exec).expect("empty projection");
        assert_eq!(empty_batches.len(), 1);
        assert_eq!(empty_batches[0].schema().len(), 0);
        assert_eq!(empty_batches[0].num_rows(), 2);

        // Bloom filter pruning on non-existent `trace_id`: row group skipped cleanly.
        let mut miss_spec = ScanSpec::default();
        miss_spec.id_lookups.push(IdLookupHint::Fixed16 {
            column: spans::TRACE_ID.to_owned(),
            values: vec![[0xFF; 16]],
        });
        let miss_batches = drain_on_workers(&source, &miss_spec, &exec).expect("bloom prune");
        assert!(miss_batches.is_empty());

        // Matching `trace_id` bloom lookup keeps the row group and filters via filter expr.
        let trace_idx = span_schema.index_of(spans::TRACE_ID).expect("trace_id");
        let mut hit_spec = ScanSpec::default();
        hit_spec.id_lookups.push(IdLookupHint::Fixed16 {
            column: spans::TRACE_ID.to_owned(),
            values: vec![trace1_bytes],
        });
        hit_spec.filters.push(Expr::Binary {
            left: Box::new(Expr::Column {
                index: trace_idx,
                name: spans::TRACE_ID.to_owned(),
                data_type: DataType::FixedBytes16,
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(trace1_bytes))),
            data_type: DataType::Bool,
        });
        let hit_batches = drain_on_workers(&source, &hit_spec, &exec).expect("bloom hit");
        assert_eq!(hit_batches.iter().map(Batch::num_rows).sum::<usize>(), 1);

        // Missing nullable column (schema evolution) -> `Column::Const(Null, len)`.
        let mut evolved_fields = span_schema.fields().to_vec();
        evolved_fields.push(Field::new("future_nullable_tag", DataType::Utf8, true));
        let evolved_schema = Schema::try_new_arc(evolved_fields).expect("evolved schema");
        let evolved_source = ParquetSource::new(
            obj_store.clone(),
            FileTable::Spans,
            Arc::clone(&evolved_schema),
            vec![span_file],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );
        let evolved_batches =
            drain_on_workers(&evolved_source, &ScanSpec::default(), &exec).expect("evolved schema");
        let future_idx = evolved_schema
            .index_of("future_nullable_tag")
            .expect("future col");
        assert!(matches!(
            evolved_batches[0].columns()[future_idx],
            Column::Const {
                value: ScalarValue::Null(DataType::Utf8),
                len: 2
            }
        ));

        // 2. Multi-page file (> DATA_PAGE_ROWS = 20_000) exercises page pruning.
        let mut paged_rows = Vec::with_capacity(21_000);
        for i in 0..21_000_i64 {
            let r = span_row(
                u16::try_from((i % 100) + 1).expect("u16"),
                u8::try_from(i % 200).expect("u8"),
                base_ns + i * 1_000,
                base_ns + i * 1_000,
            );
            paged_rows.push(r);
        }
        let paged_rb = span_rows(&paged_rows);
        let paged_file = write_parquet_into_store(
            &rt,
            &obj_store,
            "spans/paged.parquet",
            FileId::new_v7(),
            FileTable::Spans,
            day,
            paged_rb,
        );
        let paged_source = ParquetSource::new(
            obj_store.clone(),
            FileTable::Spans,
            Arc::clone(&span_schema),
            vec![paged_file],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );
        let start_idx = span_schema.index_of(spans::START_TIME).expect("start_time");
        let mut page_spec = ScanSpec::default();
        page_spec.filters.push(Expr::Binary {
            left: Box::new(Expr::Column {
                index: start_idx,
                name: spans::START_TIME.to_owned(),
                data_type: DataType::timestamp_utc(),
            }),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: base_ns + 20_995 * 1_000,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
        let page_batches =
            drain_on_workers(&paged_source, &page_spec, &exec).expect("page pruned scan");
        let page_hits: usize = page_batches.iter().map(Batch::num_rows).sum();
        assert_eq!(page_hits, 5);

        // 3. Traces & Scores tables also decode cleanly.
        let trace_rb = trace_batch(&[s1, s2]);
        let trace_schema = traces_schema();
        let trace_file = write_parquet_into_store(
            &rt,
            &obj_store,
            "traces/part1.parquet",
            FileId::new_v7(),
            FileTable::Traces,
            day,
            trace_rb,
        );
        let trace_source = ParquetSource::new(
            obj_store.clone(),
            FileTable::Traces,
            trace_schema,
            vec![trace_file],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );
        let trace_batches =
            drain_on_workers(&trace_source, &ScanSpec::default(), &exec).expect("trace scan");
        assert_eq!(trace_batches.iter().map(Batch::num_rows).sum::<usize>(), 2);

        let sc = score_row(
            1,
            1,
            base_ns + 3_000,
            base_ns + 3_000,
            base_ns + 3_500,
            false,
            "accuracy",
        );
        let score_rb = score_rows(&[sc]);
        let score_schema = scores_schema();
        let score_file = write_parquet_into_store(
            &rt,
            &obj_store,
            "scores/part1.parquet",
            FileId::new_v7(),
            FileTable::Scores,
            day,
            score_rb,
        );
        let score_source = ParquetSource::new(
            obj_store,
            FileTable::Scores,
            score_schema,
            vec![score_file],
            Arc::from([]),
            cache,
            suspects,
            rt.handle().clone(),
        );
        let score_batches =
            drain_on_workers(&score_source, &ScanSpec::default(), &exec).expect("score scan");
        assert_eq!(score_batches.iter().map(Batch::num_rows).sum::<usize>(), 1);
    }

    #[test]
    fn corrupt_eight_byte_file_reports_suspect_without_panicking() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        let mem_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let counting = Arc::new(CountingStore::new(mem_store));
        let obj_store: Arc<dyn ObjectStore> = counting.clone();
        let queue = Arc::new(VerifyQueue::default());
        let suspects = Suspects::new(Arc::clone(&queue));
        let cache = Arc::new(MetadataCache::new(1024 * 1024));
        let project: cetra_core::ids::ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001"
            .parse()
            .expect("project");

        // Exact 8-byte fuzz regression payload: [0x2f, 0x15, 0x00, 0x00, b'P', b'A', b'R', b'1'].
        let corrupt_bytes = Bytes::from_static(b"\x2f\x15\x00\x00PAR1");
        let path = "spans/corrupt8.parquet";
        rt.block_on(obj_store.put(&ObjectPath::from(path), PutPayload::from(corrupt_bytes)))
            .expect("put corrupt");

        let day = NaiveDate::from_ymd_opt(2026, 3, 15).expect("day");
        let bad_id = FileId::new_v7();
        let bad_file = Arc::new(FileMeta {
            id: bad_id,
            table: FileTable::Spans,
            project,
            day,
            level: 0,
            path: path.to_owned(),
            size: 8,
            footer_size: 8,
            rows: 1,
            min_time: 0,
            max_time: 100,
            min_ingest_time: 0,
            max_ingest_time: 100,
            fully_merged: false,
            deletions_applied_seq: 0,
        });

        let schema = Schema::try_new_arc(vec![Field::new(spans::NAME, DataType::Utf8, false)])
            .expect("schema");

        let source = ParquetSource::new(
            Arc::clone(&obj_store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&bad_file)],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );

        let exec = test_ctx(16 * 1024 * 1024);
        let err = drain_on_workers(&source, &ScanSpec::default(), &exec)
            .expect_err("corrupt 8-byte file must error without panicking");
        assert!(matches!(err, QueryError::Internal(_)));

        let queued = queue.take(100);
        assert!(
            queued.contains(&bad_id),
            "corrupt file must be reported to VerifyQueue"
        );

        // Transient store errors MUST NOT quarantine the file as corrupt.
        let transient_id = FileId::new_v7();
        let transient_file = Arc::new(FileMeta {
            id: transient_id,
            table: FileTable::Spans,
            project,
            day,
            level: 0,
            path: "spans/transient.parquet".to_owned(),
            size: 1024,
            footer_size: 128,
            rows: 1,
            min_time: 0,
            max_time: 100,
            min_ingest_time: 0,
            max_ingest_time: 100,
            fully_merged: false,
            deletions_applied_seq: 0,
        });
        counting.transient_fail.store(true, Ordering::Relaxed);
        let transient_source = ParquetSource::new(
            Arc::clone(&obj_store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![transient_file],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );
        let transient_exec = test_ctx(16 * 1024 * 1024);
        let err = drain_on_workers(&transient_source, &ScanSpec::default(), &transient_exec)
            .expect_err("transient store error");
        assert!(
            matches!(err, QueryError::Internal(_)),
            "unexpected error for transient store failure: {err:?}"
        );
        assert!(
            queue.take(100).is_empty(),
            "transient I/O errors must not quarantine files"
        );

        // Cancellation during stalled ObjectStore I/O returns Cancelled without quarantining.
        counting.transient_fail.store(false, Ordering::Relaxed);
        counting.hang_get_ranges.store(true, Ordering::Relaxed);
        let cancel_source = ParquetSource::new(
            obj_store,
            FileTable::Spans,
            schema,
            vec![bad_file],
            Arc::from([]),
            cache,
            suspects,
            rt.handle().clone(),
        );
        let cancel_exec = test_ctx(16 * 1024 * 1024);
        let cancel = cancel_exec.eval.cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            cancel.cancel();
        });
        let err = drain_on_workers(&cancel_source, &ScanSpec::default(), &cancel_exec)
            .expect_err("cancelled stalled I/O");
        assert!(matches!(err, QueryError::Cancelled));
        assert!(queue.take(100).is_empty());
    }

    fn write_custom_parquet(
        rt: &tokio::runtime::Runtime,
        store: &Arc<dyn ObjectStore>,
        path: &str,
        table: FileTable,
        day: NaiveDate,
        batches: &[RecordBatch],
        props: WriterProperties,
    ) -> Arc<FileMeta> {
        let file_id = FileId::new_v7();
        let mut buf = Vec::new();
        let schema = batches[0].schema();
        let mut writer = ArrowWriter::try_new(&mut buf, schema, Some(props)).expect("arrow writer");
        let mut total_rows = 0u64;
        for rb in batches {
            total_rows += rb.num_rows() as u64;
            writer.write(rb).expect("write batch");
        }
        let meta = writer.close().expect("close writer");
        let size = buf.len() as u64;
        let footer_size = if buf.len() >= 8 {
            let tail: [u8; 8] = buf[buf.len() - 8..].try_into().expect("tail");
            footer_size_from_tail(&tail, size).unwrap_or(8)
        } else {
            8
        };
        let _ = meta;
        rt.block_on(store.put(&ObjectPath::from(path), PutPayload::from(buf)))
            .expect("put custom parquet");

        let project: cetra_core::ids::ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001"
            .parse()
            .expect("project");
        let t = day_start_ns(day).expect("day start");
        Arc::new(FileMeta {
            id: file_id,
            table,
            project,
            day,
            level: 0,
            path: path.to_owned(),
            size,
            footer_size: footer_size.max(8),
            rows: total_rows,
            min_time: t,
            max_time: t + 1_000,
            min_ingest_time: t,
            max_ingest_time: t + 1_000,
            fully_merged: false,
            deletions_applied_seq: 0,
        })
    }

    #[test]
    fn dict_bit_width_ge_8_and_plain_fallback_and_nested_nulls() {
        use arrow::array::{ListBuilder, MapBuilder, StringBuilder};
        use arrow::datatypes::{
            DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema,
        };
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::{EnabledStatistics, WriterProperties};

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache = Arc::new(MetadataCache::new(1024 * 1024));
        let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
        let day = NaiveDate::from_ymd_opt(2026, 3, 15).expect("day");
        let exec = test_ctx(32 * 1024 * 1024);

        // (a) DictColumn with bit_width >= 8 (>=260 distinct values).
        let dict_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "tag",
            ArrowDataType::Utf8,
            true,
        )]));
        let mut sb = StringBuilder::new();
        for i in 0..300 {
            if i % 50 == 0 {
                sb.append_null();
            } else {
                sb.append_value(format!("distinct_value_{i:04}"));
            }
        }
        let dict_rb = RecordBatch::try_new(Arc::clone(&dict_schema), vec![Arc::new(sb.finish())])
            .expect("rb");
        let col_schema = Schema::try_new_arc(vec![Field::new("tag", DataType::Utf8, true)])
            .expect("convert dict schema");
        let props_dict = WriterProperties::builder()
            .set_dictionary_enabled(true)
            .build();
        let dict_file = {
            let mut buf = Vec::new();
            let mut w = ArrowWriter::try_new(&mut buf, Arc::clone(&dict_schema), Some(props_dict))
                .expect("writer");
            w.write(&dict_rb).expect("write");
            w.close().expect("close");
            let size = buf.len() as u64;
            rt.block_on(store.put(&ObjectPath::from("dict9.parquet"), PutPayload::from(buf)))
                .expect("put");
            Arc::new(FileMeta {
                id: FileId::new_v7(),
                table: FileTable::Spans,
                project: "018f6b4e-7c1a-7c1a-8c1a-000000000001"
                    .parse()
                    .expect("project"),
                day,
                level: 0,
                path: "dict9.parquet".to_owned(),
                size,
                footer_size: size.min(4096),
                rows: 300,
                min_time: 0,
                max_time: 1000,
                min_ingest_time: 0,
                max_ingest_time: 1000,
                fully_merged: false,
                deletions_applied_seq: 0,
            })
        };
        let dict_source = ParquetSource::new(
            Arc::clone(&store),
            FileTable::Spans,
            col_schema,
            vec![dict_file],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );
        let batches =
            drain_on_workers(&dict_source, &ScanSpec::default(), &exec).expect("dict >= 260");
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 300);
        assert!(matches!(batches[0].columns()[0], Column::Dict(_)));
        assert_eq!(
            batches[0].columns()[0].get_scalar(0).expect("row 0"),
            ScalarValue::Null(DataType::Utf8)
        );
        assert_eq!(
            batches[0].columns()[0].get_scalar(299).expect("row 299"),
            ScalarValue::Utf8("distinct_value_0299".to_owned())
        );

        // (b) RLE_DICTIONARY -> PLAIN page fallback within a row group.
        let mut fb_sb = StringBuilder::new();
        for i in 0..80 {
            if i < 20 {
                fb_sb.append_value(if i % 2 == 0 { "aa" } else { "bb" });
            } else {
                fb_sb.append_value(format!("fallback_long_unique_string_entry_{i:04}"));
            }
        }
        let fb_rb = RecordBatch::try_new(Arc::clone(&dict_schema), vec![Arc::new(fb_sb.finish())])
            .expect("fb rb");
        let fb_props = WriterProperties::builder()
            .set_dictionary_enabled(true)
            .set_dictionary_page_size_limit(64)
            .set_data_page_row_count_limit(20)
            .set_write_batch_size(10)
            .build();
        let fb_file = {
            let mut buf = Vec::new();
            let mut w = ArrowWriter::try_new(&mut buf, Arc::clone(&dict_schema), Some(fb_props))
                .expect("writer");
            w.write(&fb_rb).expect("write");
            w.close().expect("close");
            let size = buf.len() as u64;
            rt.block_on(store.put(&ObjectPath::from("fallback.parquet"), PutPayload::from(buf)))
                .expect("put");
            Arc::new(FileMeta {
                id: FileId::new_v7(),
                table: FileTable::Spans,
                project: "018f6b4e-7c1a-7c1a-8c1a-000000000001"
                    .parse()
                    .expect("project"),
                day,
                level: 0,
                path: "fallback.parquet".to_owned(),
                size,
                footer_size: size.min(4096),
                rows: 80,
                min_time: 0,
                max_time: 1000,
                min_ingest_time: 0,
                max_ingest_time: 1000,
                fully_merged: false,
                deletions_applied_seq: 0,
            })
        };
        let fb_col_schema = Schema::try_new_arc(vec![Field::new("tag", DataType::Utf8, true)])
            .expect("convert fb schema");
        let fb_source = ParquetSource::new(
            Arc::clone(&store),
            FileTable::Spans,
            fb_col_schema,
            vec![fb_file],
            Arc::from([]),
            Arc::clone(&cache),
            suspects.clone(),
            rt.handle().clone(),
        );
        let fb_batches =
            drain_on_workers(&fb_source, &ScanSpec::default(), &exec).expect("fallback scan");
        assert_eq!(fb_batches.iter().map(Batch::num_rows).sum::<usize>(), 80);
        assert_eq!(
            fb_batches[0].columns()[0].get_scalar(0).expect("r0"),
            ScalarValue::Utf8("aa".to_owned())
        );
        assert_eq!(
            fb_batches[0].columns()[0].get_scalar(79).expect("r79"),
            ScalarValue::Utf8("fallback_long_unique_string_entry_0079".to_owned())
        );

        // (c) Empty/null List, empty Map, null map values, all-null dictionary page.
        let mut list_builder = ListBuilder::new(StringBuilder::new());
        list_builder.values().append_value("x");
        list_builder.append(true); // ["x"]
        list_builder.append(true); // []
        list_builder.append_null(); // null list
        list_builder.values().append_value("y");
        list_builder.append(true); // ["y"]
        let list_arr = Arc::new(list_builder.finish());

        let mut map_builder = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        map_builder.keys().append_value("k1");
        map_builder.values().append_value("v1");
        map_builder.keys().append_value("k2");
        map_builder.values().append_null();
        map_builder.append(true).expect("map row 0");
        map_builder.append(true).expect("map row 1"); // {}
        map_builder.append(false).expect("map row 2"); // null map
        map_builder.keys().append_value("k3");
        map_builder.values().append_value("v3");
        map_builder.append(true).expect("map row 3");
        let map_arr = Arc::new(map_builder.finish());

        let mut all_null_sb = StringBuilder::new();
        for _ in 0..4 {
            all_null_sb.append_null();
        }
        let all_null_arr = Arc::new(all_null_sb.finish());

        let nested_schema = Arc::new(ArrowSchema::new(vec![
            ArrowField::new("tags", list_arr.data_type().clone(), true),
            ArrowField::new("attrs", map_arr.data_type().clone(), true),
            ArrowField::new("all_null", ArrowDataType::Utf8, true),
        ]));
        let nested_rb = RecordBatch::try_new(
            Arc::clone(&nested_schema),
            vec![list_arr, map_arr, all_null_arr],
        )
        .expect("nested rb");
        let nested_col_schema = Schema::try_new_arc(vec![
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
            Field::new("attrs", DataType::MapUtf8, true),
            Field::new("all_null", DataType::Utf8, true),
        ])
        .expect("nested col schema");
        let nested_props = WriterProperties::builder()
            .set_dictionary_enabled(true)
            .set_statistics_enabled(EnabledStatistics::Page)
            .build();
        let nested_file = write_custom_parquet(
            &rt,
            &store,
            "nested_nulls.parquet",
            FileTable::Spans,
            day,
            &[nested_rb],
            nested_props,
        );
        let nested_source = ParquetSource::new(
            store,
            FileTable::Spans,
            nested_col_schema,
            vec![nested_file],
            Arc::from([]),
            cache,
            suspects,
            rt.handle().clone(),
        );
        let nested_batches =
            drain_on_workers(&nested_source, &ScanSpec::default(), &exec).expect("nested scan");
        assert_eq!(nested_batches.iter().map(Batch::num_rows).sum::<usize>(), 4);
        let nb = &nested_batches[0];
        assert_eq!(
            nb.columns()[0].get_scalar(1).expect("empty list"),
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![],
            }
        );
        assert_eq!(
            nb.columns()[0].get_scalar(2).expect("null list"),
            ScalarValue::Null(DataType::List(Box::new(DataType::Utf8)))
        );
        assert_eq!(
            nb.columns()[1].get_scalar(1).expect("empty map"),
            ScalarValue::MapUtf8(vec![])
        );
        assert_eq!(
            nb.columns()[1].get_scalar(2).expect("null map"),
            ScalarValue::Null(DataType::MapUtf8)
        );
        assert_eq!(
            nb.columns()[1].get_scalar(0).expect("map with null value"),
            ScalarValue::MapUtf8(vec![
                ("k1".to_owned(), Some("v1".to_owned())),
                ("k2".to_owned(), None),
            ])
        );
        for r in 0..4 {
            assert_eq!(
                nb.columns()[2].get_scalar(r).expect("all null"),
                ScalarValue::Null(DataType::Utf8)
            );
        }
    }

    #[test]
    fn around_ns_and_file_day_bounds_pruning_rules() {
        use cetra_query::exec::TimeRangeHint;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache = Arc::new(MetadataCache::new(1024));
        let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
        let schema = Schema::try_new_arc(vec![Field::new(spans::NAME, DataType::Utf8, false)])
            .expect("schema");
        let project: cetra_core::ids::ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001"
            .parse()
            .expect("project");

        let d0 = NaiveDate::from_ymd_opt(2026, 3, 10).expect("d0");
        let d5 = NaiveDate::from_ymd_opt(2026, 3, 15).expect("d5");
        let d9 = NaiveDate::from_ymd_opt(2026, 3, 19).expect("d9");
        let make_file = |day: NaiveDate| {
            let t = day_start_ns(day).expect("start");
            Arc::new(FileMeta {
                id: FileId::new_v7(),
                table: FileTable::Spans,
                project,
                day,
                level: 0,
                path: format!("f_{day}.parquet"),
                size: 100,
                footer_size: 32,
                rows: 10,
                min_time: t,
                max_time: t + 1_000,
                min_ingest_time: t,
                max_ingest_time: t + 1_000,
                fully_merged: false,
                deletions_applied_seq: 0,
            })
        };
        let files = vec![make_file(d0), make_file(d5), make_file(d9)];
        let d5_ns = day_start_ns(d5).expect("d5 ns");

        // (e) `file_day_bounds`: ±1 day for Spans/Traces, exact for Scores.
        assert_eq!(
            file_day_bounds(FileTable::Spans, Some(d5_ns), Some(d5_ns)),
            (d5.pred_opt().expect("pred"), d5.succ_opt().expect("succ"))
        );
        assert_eq!(
            file_day_bounds(FileTable::Traces, Some(d5_ns), Some(d5_ns)),
            (d5.pred_opt().expect("pred"), d5.succ_opt().expect("succ"))
        );
        assert_eq!(
            file_day_bounds(FileTable::Scores, Some(d5_ns), Some(d5_ns)),
            (d5, d5)
        );

        // (d) `around_ns` applies on Spans/Traces with `trace_id` + open time range.
        for table in [FileTable::Spans, FileTable::Traces] {
            let src = ParquetSource::new(
                Arc::clone(&store),
                table,
                Arc::clone(&schema),
                files.clone(),
                Arc::from([]),
                Arc::clone(&cache),
                suspects.clone(),
                rt.handle().clone(),
            );
            let spec_trace_open = ScanSpec {
                time_range: TimeRangeHint {
                    min_ns: None,
                    max_ns: None,
                    around_ns: Some(d5_ns),
                },
                id_lookups: vec![IdLookupHint::Fixed16 {
                    column: traces::TRACE_ID.to_owned(),
                    values: vec![[1u8; 16]],
                }],
                ..Default::default()
            };
            let kept = src.prune_files(&spec_trace_open);
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].day, d5);

            // Ignored when time_range is closed.
            let spec_closed = ScanSpec {
                time_range: TimeRangeHint {
                    min_ns: Some(day_start_ns(d0).expect("d0")),
                    max_ns: None,
                    around_ns: Some(d5_ns),
                },
                id_lookups: vec![IdLookupHint::Fixed16 {
                    column: traces::TRACE_ID.to_owned(),
                    values: vec![[1u8; 16]],
                }],
                ..Default::default()
            };
            assert_eq!(src.prune_files(&spec_closed).len(), 3);

            // Ignored on `user_id` and `session_id` lookups.
            for col in [spans::USER_ID, spans::SESSION_ID] {
                let spec_other_id = ScanSpec {
                    time_range: TimeRangeHint {
                        min_ns: None,
                        max_ns: None,
                        around_ns: Some(d5_ns),
                    },
                    id_lookups: vec![IdLookupHint::Utf8 {
                        column: col.to_owned(),
                        values: vec!["u1".to_owned()],
                    }],
                    ..Default::default()
                };
                assert_eq!(src.prune_files(&spec_other_id).len(), 3);
            }
        }

        // Ignored on `Scores` even with `trace_id` + open time range.
        let scores_src = ParquetSource::new(
            store,
            FileTable::Scores,
            schema,
            files,
            Arc::from([]),
            cache,
            suspects,
            rt.handle().clone(),
        );
        let spec_scores = ScanSpec {
            time_range: TimeRangeHint {
                min_ns: None,
                max_ns: None,
                around_ns: Some(d5_ns),
            },
            id_lookups: vec![IdLookupHint::Fixed16 {
                column: scores::TRACE_ID.to_owned(),
                values: vec![[1u8; 16]],
            }],
            ..Default::default()
        };
        assert_eq!(scores_src.prune_files(&spec_scores).len(), 3);
    }

    #[test]
    fn zero_block_bloom_filter_quarantines_without_panicking() {
        use parquet::file::metadata::{ParquetMetaDataReader, ParquetMetaDataWriter};

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache = Arc::new(MetadataCache::new(1024 * 1024));
        let queue = Arc::new(VerifyQueue::default());
        let suspects = Suspects::new(Arc::clone(&queue));
        let day = NaiveDate::from_ymd_opt(2026, 3, 15).expect("day");
        let base_ns = day_start_ns(day).expect("day start");

        // Valid Parquet bloom filter header with `numBytes = 0` (0 blocks).
        const ZERO_BLOCK_BLOOM: [u8; 15] = [
            0x15, 0x00, 0x1c, 0x1c, 0x00, 0x00, 0x1c, 0x1c, 0x00, 0x00, 0x1c, 0x1c, 0x00, 0x00,
            0x00,
        ];
        assert_eq!(
            Sbbf::from_bytes(&ZERO_BLOCK_BLOOM)
                .expect("zero-block header parses")
                .num_blocks(),
            0
        );

        let s1 = span_row(1, 0, base_ns + 1_000, base_ns + 2_000);
        let trace_bytes = *s1.trace_id.as_bytes();
        let rb = span_rows(&[s1]);
        let schema = spans_schema();
        let written = write_file_batch(FileTable::Spans, 0, &[Arc::new(rb)]).expect("write_file");
        let mut raw = written.bytes.to_vec();
        let meta = ParquetMetaDataReader::new()
            .parse_and_finish(&Bytes::from(raw.clone()))
            .expect("parse meta");

        let trace_leaf = find_scalar_leaf(meta.file_metadata().schema_descr(), spans::TRACE_ID)
            .expect("trace_id leaf");
        let bloom_off = meta
            .row_group(0)
            .column(trace_leaf)
            .bloom_filter_offset()
            .expect("bloom offset") as usize;
        raw[bloom_off..bloom_off + ZERO_BLOCK_BLOOM.len()].copy_from_slice(&ZERO_BLOCK_BLOOM);

        let mut rgs = meta.row_groups().to_vec();
        let mut cols = rgs[0].columns().to_vec();
        cols[trace_leaf] = cols[trace_leaf]
            .clone()
            .into_builder()
            .set_bloom_filter_length(Some(ZERO_BLOCK_BLOOM.len() as i32))
            .build()
            .expect("col chunk");
        rgs[0] = rgs[0]
            .clone()
            .into_builder()
            .set_column_metadata(cols)
            .build()
            .expect("rg");
        let patched_meta = ParquetMetaData::new(meta.file_metadata().clone(), rgs);
        let mut patched_buf = raw;
        ParquetMetaDataWriter::new(&mut patched_buf, &patched_meta)
            .finish()
            .expect("write patched meta");

        let size = patched_buf.len() as u64;
        let tail: [u8; 8] = patched_buf[patched_buf.len() - 8..]
            .try_into()
            .expect("tail");
        let footer_size = footer_size_from_tail(&tail, size).expect("footer size");
        let file_id = FileId::new_v7();
        let path = "spans/zero_bloom.parquet";
        rt.block_on(store.put(&ObjectPath::from(path), PutPayload::from(patched_buf)))
            .expect("put");
        let project: cetra_core::ids::ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001"
            .parse()
            .expect("project");
        let fm = Arc::new(FileMeta {
            id: file_id,
            table: FileTable::Spans,
            project,
            day,
            level: 0,
            path: path.to_owned(),
            size,
            footer_size,
            rows: 1,
            min_time: written.min_time,
            max_time: written.max_time,
            min_ingest_time: written.min_ingest_time,
            max_ingest_time: written.max_ingest_time,
            fully_merged: false,
            deletions_applied_seq: 0,
        });

        let source = ParquetSource::new(
            store,
            FileTable::Spans,
            schema,
            vec![fm],
            Arc::from([]),
            cache,
            suspects,
            rt.handle().clone(),
        );
        let mut spec = ScanSpec::default();
        spec.id_lookups.push(IdLookupHint::Fixed16 {
            column: spans::TRACE_ID.to_owned(),
            values: vec![trace_bytes],
        });
        let exec = test_ctx(16 * 1024 * 1024);
        let err = drain_on_workers(&source, &spec, &exec)
            .expect_err("0-block bloom filter must fail safely");
        assert!(matches!(err, QueryError::Internal(_)));
        assert!(
            queue.take(10).contains(&file_id),
            "0-block bloom filter file must be quarantined"
        );
    }

    #[test]
    fn float_nan_not_falsely_pruned_and_privacy_safe_debug() {
        use arrow::array::Float64Array;
        use arrow::datatypes::{
            DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema,
        };
        use parquet::file::properties::{EnabledStatistics, WriterProperties};

        use crate::deletion::PendingDeletion;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("rt");
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache = Arc::new(MetadataCache::new(1024 * 1024));
        let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
        let day = NaiveDate::from_ymd_opt(2026, 3, 15).expect("day");

        // (g) Float NaN in row group [1.0, NaN] has Parquet stats min=1.0, max=1.0,
        // but cetra-query orders NaN > +inf so `val > 5.0` must NOT prune it.
        let arrow_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "val",
            ArrowDataType::Float64,
            false,
        )]));
        let rb_nan = RecordBatch::try_new(
            Arc::clone(&arrow_schema),
            vec![Arc::new(Float64Array::from(vec![1.0, f64::NAN]))],
        )
        .expect("rb_nan");
        let rb_hi = RecordBatch::try_new(
            Arc::clone(&arrow_schema),
            vec![Arc::new(Float64Array::from(vec![10.0, 20.0]))],
        )
        .expect("rb_hi");
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(2))
            .set_statistics_enabled(EnabledStatistics::Page)
            .build();
        let file = write_custom_parquet(
            &rt,
            &store,
            "nan.parquet",
            FileTable::Spans,
            day,
            &[rb_nan, rb_hi],
            props,
        );
        let col_schema =
            Schema::try_new_arc(vec![Field::new("val", DataType::F64, false)]).expect("col schema");
        let source = ParquetSource::new(
            store,
            FileTable::Spans,
            col_schema,
            vec![file],
            Arc::from([Arc::new(PendingDeletion {
                id: uuid::Uuid::from_u128(1),
                seq: 1,
                project: "018f6b4e-7c1a-7c1a-8c1a-000000000001"
                    .parse()
                    .expect("project"),
                cutoff: 100,
                registered_at: 200,
                trace_ids: std::collections::HashSet::new(),
                users: ["secret-erased-user-99".to_owned()].into_iter().collect(),
                sessions: ["secret-erased-sess-88".to_owned()].into_iter().collect(),
            })]),
            cache,
            suspects,
            rt.handle().clone(),
        );

        let exec = test_ctx(16 * 1024 * 1024);
        let gt_spec = ScanSpec {
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: 0,
                    name: "val".to_owned(),
                    data_type: DataType::F64,
                }),
                op: BinaryOp::Gt,
                right: Box::new(Expr::Literal(ScalarValue::F64(5.0))),
                data_type: DataType::Bool,
            }],
            ..Default::default()
        };
        let batches = drain_on_workers(&source, &gt_spec, &exec).expect("gt 5.0 scan");
        let total: usize = batches.iter().map(Batch::num_rows).sum();
        // NaN, 10.0, 20.0 must all survive `val > 5.0`.
        assert_eq!(total, 3);

        // `val < 0.0` CAN safely prune [1.0, NaN] against min = 1.0 (since NaN is not < 0.0).
        let lt_spec = ScanSpec {
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: 0,
                    name: "val".to_owned(),
                    data_type: DataType::F64,
                }),
                op: BinaryOp::Lt,
                right: Box::new(Expr::Literal(ScalarValue::F64(0.0))),
                data_type: DataType::Bool,
            }],
            ..Default::default()
        };
        let lt_batches = drain_on_workers(&source, &lt_spec, &exec).expect("lt 0.0 scan");
        assert!(lt_batches.is_empty());

        // (h) Privacy-safe Debug on ParquetSource, ScanPlan, ConstraintValue, BloomLookup.
        let src_dbg = format!("{source:?}");
        assert!(src_dbg.contains("ParquetSource"));
        assert!(src_dbg.contains("files: 1"));
        assert!(src_dbg.contains("deletions: 1"));
        assert!(!src_dbg.contains("secret-erased-user-99"));
        assert!(!src_dbg.contains("secret-erased-sess-88"));

        let secret_spec = ScanSpec {
            id_lookups: vec![IdLookupHint::Utf8 {
                column: spans::USER_ID.to_owned(),
                values: vec!["secret-user-lookup@example.com".to_owned()],
            }],
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: 0,
                    name: "val".to_owned(),
                    data_type: DataType::Utf8,
                }),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::Utf8(
                    "secret-predicate-value".to_owned(),
                ))),
                data_type: DataType::Bool,
            }],
            ..Default::default()
        };
        let plan =
            ScanPlan::compile(FileTable::Spans, &source.schema(), &secret_spec).expect("plan");
        let plan_dbg = format!("{plan:?}");
        assert!(plan_dbg.contains("ScanPlan"));
        assert!(!plan_dbg.contains("secret-user-lookup"));
        assert!(!plan_dbg.contains("secret-predicate-value"));
        let cv_dbg = format!("{:?}", ConstraintValue::Bytes(b"secret-bytes".to_vec()));
        assert!(!cv_dbg.contains("secret-bytes"));
        assert!(cv_dbg.contains("12 bytes"));
        let bl_dbg = format!(
            "{:?}",
            BloomLookup {
                column: spans::USER_ID.to_owned(),
                keys: vec![b"secret-bloom-key".to_vec()],
            }
        );
        assert!(!bl_dbg.contains("secret-bloom-key"));
        assert!(bl_dbg.contains("keys_len: 1"));
    }

    #[test]
    fn corrupt_headers_and_runtime_guard_are_rejected_safely() {
        // Unbounded allocation guards on decode_plain_utf8_dict, decode_rle_hybrid, decode_levels.
        assert!(decode_plain_utf8_dict(&[0u8; 8], 3).is_err());
        assert!(decode_rle_hybrid(&[0x03, 0x00], 8, 1_000_000).is_err());
        #[allow(deprecated)]
        let bit_packed_enc = Encoding::BIT_PACKED;
        assert!(decode_levels(3, 1_000_000, bit_packed_enc, &[0u8; 4]).is_err());

        // OffsetIndex validation: first_row_index != 0 is rejected.
        let bad_locs = vec![PageLocation {
            offset: 10,
            compressed_page_size: 50,
            first_row_index: 1,
        }];
        assert!(validate_page_locations(&bad_locs, 100, 1000).is_err());

        // `fetch_ranges` inside a Tokio runtime returns non-quarantining Runtime error.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let queue = Arc::new(VerifyQueue::default());
        let suspects = Suspects::new(Arc::clone(&queue));
        let file_id = FileId::new_v7();
        let handle = rt.handle().clone();
        rt.block_on(async move {
            let range = 0..8;
            let err = fetch_ranges(
                &store,
                &ObjectPath::from("missing.parquet"),
                std::slice::from_ref(&range),
                &CancelToken::new(),
                &handle,
            )
            .expect_err("must reject inside tokio runtime");
            assert!(matches!(err, FetchError::Runtime));
            let qerr = map_fetch_err(&err, file_id, &suspects);
            assert!(matches!(qerr, QueryError::Internal(_)));
        });
        assert!(queue.take(10).is_empty());
    }
}
