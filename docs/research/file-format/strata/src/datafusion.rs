//! DataFusion integration: a `TableProvider` over Strata files with exact filter pushdown, and
//! the `ExecutionPlan` that reads them (design S8).
//!
//! `translate` decides, for each filter, whether Strata evaluates it exactly, as a superset, or
//! not at all. `scan` and `supports_filters_pushdown` both call it, so they agree on the filters
//! DataFusion passes to `scan`. DataFusion may rewrite a filter after accepting it (`i IN (1,
//! NULL)` becomes `i = 1 OR NULL`), so `scan` translates the rewritten form as well. A filter
//! that `scan` cannot translate, or a set of pushed filters that exceeds the predicate size
//! limit together, is a `Plan` error and never a dropped filter: a dropped filter returns wrong
//! rows.

use std::ops::Range;
use std::sync::Arc;

use arrow::array::{AsArray, BooleanArray, RecordBatch, RecordBatchOptions};
use arrow::compute::{SortColumn, concat_batches, lexsort_to_indices, take_record_batch};
use arrow::compute::{SortOptions, cast};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use async_trait::async_trait;
use futures::future::try_join_all;
use futures::{StreamExt as _, TryStreamExt};
use object_store::ObjectStore;
use object_store::path::Path;

use ::datafusion::catalog::Session;
use ::datafusion::common::stats::Precision;
use ::datafusion::common::tree_node::TreeNode;
use ::datafusion::common::tree_node::TreeNodeRecursion;
use ::datafusion::common::utils::project_schema;
use ::datafusion::common::{ColumnStatistics, DFSchema, DataFusionError, ScalarValue, Statistics};
use ::datafusion::datasource::TableProvider;
use ::datafusion::error::Result as DfResult;
use ::datafusion::execution::TaskContext;
use ::datafusion::logical_expr::expr::{Between, BinaryExpr, InList, Like, ScalarFunction};
use ::datafusion::logical_expr::expr_rewriter::unnormalize_col;
use ::datafusion::logical_expr::utils::conjunction;
use ::datafusion::logical_expr::{Expr, Operator, TableProviderFilterPushDown, TableType};
use ::datafusion::physical_expr::expressions::{
    BinaryExpr as PhysicalBinaryExpr, Column as PhysicalColumn,
};
use ::datafusion::physical_expr::utils::{collect_columns, reassign_expr_columns};
use ::datafusion::physical_expr::{DynamicFilterTracking, PhysicalExpr};
use ::datafusion::physical_expr::{EquivalenceProperties, PhysicalSortExpr};
use ::datafusion::physical_plan::execution_plan::{Boundedness, EmissionType, SchedulingType};
use ::datafusion::physical_plan::filter_pushdown::{
    ChildPushdownResult, FilterPushdownPhase, FilterPushdownPropagation, PushedDown,
};
use ::datafusion::physical_plan::metrics::{ExecutionPlanMetricsSet, MetricBuilder, MetricsSet};
use ::datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use ::datafusion::physical_plan::{
    ChildrenPropertiesMode, DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning,
    PlanProperties, ReplaceChildrenOptions, SendableRecordBatchStream, SortOrderPushdownResult,
    StatisticsArgs,
};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::Layer;
use crate::io::{IoStats, ScanKind};
use crate::predicate::{self, CmpOp, Lit, Predicate, layout_of};
use crate::scan::{RowFilter, ScanRequest, TopRows};
use ::datafusion::common::runtime::SpawnedTask;
use ::datafusion::config::ConfigOptions;

/// A DataFusion table over Strata files that share one schema.
#[derive(Debug)]
pub struct StrataTable {
    files: Vec<Arc<StrataFile>>,
    /// The table schema: the file schema with top-level Utf8 → Utf8View and Binary → BinaryView.
    schema: SchemaRef,
}

impl StrataTable {
    /// Opens every file in `paths` from `store`, concurrently.
    pub async fn open(
        store: Arc<dyn ObjectStore>,
        paths: Vec<Path>,
        config: &Config,
    ) -> Result<StrataTable> {
        let files = try_join_all(
            paths
                .into_iter()
                .map(|path| StrataFile::open(Arc::clone(&store), path, None, config)),
        )
        .await?;
        StrataTable::new(files.into_iter().map(Arc::new).collect())
    }

    /// A table over already-open files. They must share one schema and one sort key.
    pub fn new(files: Vec<Arc<StrataFile>>) -> Result<StrataTable> {
        let first = files.first().ok_or_else(|| StrataError::Schema {
            reason: "no file".to_string(),
        })?;
        let file_schema = first.schema();
        let sort_key = first.sort_key_column();
        for file in &files {
            if *file.schema() != *file_schema {
                return Err(StrataError::Schema {
                    reason: "files have different schemas".to_string(),
                });
            }
            if file.sort_key_column() != sort_key {
                return Err(StrataError::Schema {
                    reason: "files have different sort keys".to_string(),
                });
            }
        }
        let schema = table_schema(&file_schema);
        Ok(StrataTable { files, schema })
    }

    /// Whether, in every file, `column` is a shredded map whose key `key` is promoted with
    /// `residual_hits == 0`. Then `map['key'] = v` is exactly `Predicate::MapKeyEq`.
    pub fn exact_map_key(&self, column: &str, key: &str) -> bool {
        let Ok(index) = self.schema.index_of(column) else {
            return false;
        };
        !self.files.is_empty()
            && self.files.iter().all(|file| {
                layout_of(file, index).is_ok_and(|layout| {
                    layout
                        .keys
                        .iter()
                        .any(|k| k.key == key.as_bytes() && k.residual_hits == 0)
                })
            })
    }

    /// The sort-key equality `column = X'…'` that `scan` answers with `lookup_scan`: the sort-key
    /// column's name and the key bytes. `None` for any other filter.
    fn sort_key_equality(&self, filter: &Expr) -> Option<(String, Vec<u8>)> {
        let Expr::BinaryExpr(BinaryExpr {
            left,
            op: Operator::Eq,
            right,
        }) = filter
        else {
            return None;
        };
        let (
            Expr::Column(column),
            Expr::Literal(ScalarValue::FixedSizeBinary(width, Some(bytes)), _),
        ) = (left.as_ref(), right.as_ref())
        else {
            return None;
        };
        let index = self.schema.index_of(&column.name).ok()?;
        if self.files.first()?.sort_key_column() != Some(index) {
            return None;
        }
        match file_type(self, index)? {
            DataType::FixedSizeBinary(w) if w == *width && (1..=16).contains(&w) => {
                Some((column.name.clone(), bytes.clone()))
            }
            _ => None,
        }
    }
}

/// The table schema of a file schema: top-level Utf8 → Utf8View and Binary → BinaryView, other
/// fields unchanged (names, nullability and metadata kept).
pub fn table_schema(file_schema: &Schema) -> SchemaRef {
    let fields: Vec<Field> = file_schema
        .fields()
        .iter()
        .map(|field| {
            let data_type = match field.data_type() {
                DataType::Utf8 => DataType::Utf8View,
                DataType::Binary => DataType::BinaryView,
                other => other.clone(),
            };
            field.as_ref().clone().with_data_type(data_type)
        })
        .collect();
    Arc::new(Schema::new_with_metadata(
        fields,
        file_schema.metadata().clone(),
    ))
}

/// Casts a batch in a file (sub)schema to the matching table (sub)schema.
pub(crate) fn to_table_batch(batch: &RecordBatch, table_schema: &SchemaRef) -> Result<RecordBatch> {
    if batch.num_columns() != table_schema.fields().len() {
        return Err(StrataError::Schema {
            reason: "batch and table schema have different column counts".to_string(),
        });
    }
    let mut columns = Vec::with_capacity(batch.num_columns());
    for (field, column) in table_schema.fields().iter().zip(batch.columns()) {
        columns.push(cast(column, field.data_type())?);
    }
    Ok(RecordBatch::try_new_with_options(
        Arc::clone(table_schema),
        columns,
        &RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
    )?)
}

/// The batches of `batches`, built on a blocking thread up to `ahead` batches ahead of the
/// consumer, so that a partition builds its next batches while DataFusion works on the ones
/// before: built in the consumer's task, the build threads waited for it between steps
/// (`real-data.md` § Payload scans). Dropping the stream stops the building.
fn built_ahead(
    batches: Box<dyn Iterator<Item = Result<RecordBatch>> + Send>,
    ahead: usize,
) -> impl futures::Stream<Item = Result<RecordBatch>> + Send {
    let (tx, rx) = tokio::sync::mpsc::channel(ahead.max(1));
    let task = SpawnedTask::spawn_blocking(move || {
        for batch in batches {
            // The receiver is gone: the stream was dropped.
            if tx.blocking_send(batch).is_err() {
                break;
            }
        }
    });
    // The task is kept with the receiver: dropping it aborts the building.
    futures::stream::unfold((rx, task), |(mut rx, task)| async move {
        rx.recv().await.map(|batch| (batch, (rx, task)))
    })
}

/// Whether `projection` holds a payload column of one of `files`.
fn reads_payload(files: &[Arc<StrataFile>], projection: &[usize]) -> bool {
    files.iter().any(|file| {
        projection.iter().any(|&column| {
            file.directory()
                .get(column)
                .is_some_and(|record| record.layer == Layer::Payload)
        })
    })
}

/// Fewest rows in one partition's range when a file is split.
const MIN_PARTITION_ROWS: u64 = 65_536;

/// Splits each file into contiguous row ranges of at least `MIN_PARTITION_ROWS` rows. A file
/// that is not split reads all its rows (`None`).
pub(crate) fn split_files(
    files: &[Arc<StrataFile>],
    target: usize,
) -> Vec<(usize, Option<Range<u32>>)> {
    let per_file = ranges_per_file(files.len(), target) as u64;
    let mut parts = Vec::new();
    for (index, file) in files.iter().enumerate() {
        for rows in split_rows(file.row_count(), per_file) {
            parts.push((index, rows));
        }
    }
    parts
}

/// How many row ranges each of `files` files is split into for `target` partitions: about
/// `target / files`, raised (to at most four times that) until the ranges are a multiple of
/// `target`. Partitions run one per core, so 5 files on 8 cores as 10 ranges would run 8, then
/// 2 alone; as 40 ranges they keep every core busy to the end.
fn ranges_per_file(files: usize, target: usize) -> usize {
    let files = files.max(1);
    let target = target.max(1);
    let base = target.div_ceil(files);
    (base..=base * 4)
        .find(|per| (files * per).is_multiple_of(target))
        .unwrap_or(base * 4)
}

/// The row ranges of one file of `rows` rows split into at most `parts` ranges of at least
/// `MIN_PARTITION_ROWS` rows: one `None` (all rows) when it is not split.
fn split_rows(rows: u64, parts: u64) -> Vec<Option<Range<u32>>> {
    let n = parts.min(rows / MIN_PARTITION_ROWS).max(1);
    match u32::try_from(rows) {
        Ok(rows) if n > 1 => (0..n)
            .map(|k| {
                let bound = |k: u64| (u64::from(rows) * k / n) as u32;
                Some(bound(k)..bound(k + 1))
            })
            .collect(),
        _ => vec![None],
    }
}

/// Most rows one scan call of a partition reads (see [`pieces`]). Tests use small pieces, so
/// their scans of a few thousand rows go through the pieces too.
#[cfg(not(test))]
const PIECE_ROWS: u32 = 1 << 20;
#[cfg(test)]
const PIECE_ROWS: u32 = 1_000;

/// The row ranges of a partition (`None`: the whole file of `rows` rows) cut into pieces of at
/// most `PIECE_ROWS` rows, in order. Empty when the rows do not fit a `u32`.
fn pieces(ranges: Option<&[Range<u32>]>, rows: u64) -> Vec<Range<u32>> {
    let whole;
    let ranges = match ranges {
        Some(ranges) => ranges,
        None => match u32::try_from(rows) {
            Ok(end) => {
                whole = 0..end;
                std::slice::from_ref(&whole)
            }
            Err(_) => return Vec::new(),
        },
    };
    let mut out = Vec::new();
    for range in ranges {
        let mut start = range.start;
        while start < range.end {
            let end = start.saturating_add(PIECE_ROWS).min(range.end);
            out.push(start..end);
            start = end;
        }
    }
    out
}

/// Maps a Strata error to DataFusion's external error.
fn df(e: StrataError) -> DataFusionError {
    DataFusionError::External(Box::new(e))
}

/// How one filter expression is pushed down.
#[derive(Debug, PartialEq)]
enum Pushed {
    /// Strata evaluates it exactly.
    Exact(Predicate),
    /// Strata applies it as a superset; DataFusion re-checks.
    Inexact(Predicate),
    /// Not pushed.
    No,
}

/// Translates one filter. It is pure: `supports_filters_pushdown` and `scan` both call it.
fn translate(expr: &Expr, table: &StrataTable) -> Pushed {
    let pushed = translate_inner(expr, table);
    let fits = |p: &Predicate| predicate::check_size(p).is_ok();
    match pushed {
        Pushed::Exact(p) if fits(&p) => Pushed::Exact(p),
        Pushed::Inexact(p) if fits(&p) => Pushed::Inexact(p),
        _ => Pushed::No,
    }
}

/// The row filter of DataFusion's physical filters `filters`, over the scan output (`schema`,
/// the table columns `projection`): their conjunction, evaluated on a batch of the table
/// columns they read. `None` when one reads no column, reads a column the output does not
/// have, or holds a dynamic filter.
fn physical_row_filter(
    filters: &[Arc<dyn PhysicalExpr>],
    projection: &[usize],
    schema: &SchemaRef,
) -> Option<RowFilter> {
    // The output position of each table column the filters read.
    let mut read: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for filter in filters {
        if DynamicFilterTracking::classify(filter).contains_dynamic_filter() {
            return None;
        }
        let columns = collect_columns(filter);
        if columns.is_empty() {
            return None;
        }
        for column in columns {
            let field = schema.fields().get(column.index())?;
            if field.name() != column.name() {
                return None;
            }
            read.insert(*projection.get(column.index())?, column.index());
        }
    }
    let fields: Vec<Field> = read
        .values()
        .filter_map(|&position| schema.fields().get(position))
        .map(|field| field.as_ref().clone())
        .collect();
    let filter_schema: SchemaRef = Arc::new(Schema::new(fields));
    let mut conjunction: Option<Arc<dyn PhysicalExpr>> = None;
    for filter in filters {
        let filter = reassign_expr_columns(Arc::clone(filter), &filter_schema).ok()?;
        conjunction = Some(match conjunction {
            Some(all) => Arc::new(PhysicalBinaryExpr::new(all, Operator::And, filter)),
            None => filter,
        });
    }
    let physical = conjunction?;
    let external = |e: DataFusionError| StrataError::from(arrow::error::ArrowError::from(e));
    let mask = move |batch: &RecordBatch| -> Result<BooleanArray> {
        let batch = to_table_batch(batch, &filter_schema)?;
        let value = physical
            .evaluate(&batch)
            .and_then(|value| value.into_array(batch.num_rows()))
            .map_err(external)?;
        value
            .as_boolean_opt()
            .cloned()
            .ok_or_else(|| StrataError::Schema {
                reason: format!("a row filter of type {}", value.data_type()),
            })
    };
    Some(RowFilter {
        columns: read.into_keys().collect(),
        mask: Arc::new(mask),
    })
}

/// Whether the scan can run `expr` as a row filter when Strata cannot translate it: it reads
/// columns of the table, by name, and has no volatile function, subquery, aggregate, window
/// function or placeholder, which a scan cannot evaluate row by row on its own.
fn row_filterable(expr: &Expr, table: &StrataTable) -> bool {
    let columns = expr.column_refs();
    let unsupported = expr
        .exists(|e| {
            Ok(matches!(
                e,
                Expr::ScalarSubquery(_)
                    | Expr::Exists(_)
                    | Expr::InSubquery(_)
                    | Expr::SetComparison(_)
                    | Expr::AggregateFunction(_)
                    | Expr::WindowFunction(_)
                    | Expr::OuterReferenceColumn(..)
                    | Expr::Placeholder(_)
            ))
        })
        .unwrap_or(true);
    !columns.is_empty()
        && columns
            .iter()
            .all(|column| table.schema.index_of(&column.name).is_ok())
        && !expr.is_volatile()
        && !unsupported
}

/// The row filter of `filters` (each [`row_filterable`]), their conjunction: DataFusion's
/// physical expression over the table columns they read, evaluated by the scan on the rows its
/// predicate selects. A NULL result drops the row, as `WHERE` does. `None` without a filter.
fn row_filter(
    state: &dyn Session,
    table: &StrataTable,
    filters: &[Expr],
) -> DfResult<Option<RowFilter>> {
    let Some(expr) = conjunction(filters.iter().cloned().map(unnormalize_col)) else {
        return Ok(None);
    };
    let mut columns = expr
        .column_refs()
        .iter()
        .map(|column| table.schema.index_of(&column.name))
        .collect::<std::result::Result<Vec<usize>, _>>()?;
    columns.sort_unstable();
    columns.dedup();
    let schema: SchemaRef = Arc::new(table.schema.project(&columns)?);
    let physical = state.create_physical_expr(expr, &DFSchema::try_from(Arc::clone(&schema))?)?;
    let external = |e: DataFusionError| StrataError::from(arrow::error::ArrowError::from(e));
    let mask = move |batch: &RecordBatch| -> Result<BooleanArray> {
        let batch = to_table_batch(batch, &schema)?;
        let value = physical
            .evaluate(&batch)
            .and_then(|value| value.into_array(batch.num_rows()))
            .map_err(external)?;
        value
            .as_boolean_opt()
            .cloned()
            .ok_or_else(|| StrataError::Schema {
                reason: format!("a row filter of type {}", value.data_type()),
            })
    };
    Ok(Some(RowFilter {
        columns,
        mask: Arc::new(mask),
    }))
}

/// How many times the stored bytes of the filter columns the other projected columns must
/// hold for a row filter to pay (see [`row_filter_pays`]).
const ROW_FILTER_GAIN: u64 = 4;

/// The stored bytes a row the other projected columns must hold for a row filter to pay (see
/// [`row_filter_pays`]).
const ROW_FILTER_MIN_ROW_BYTES: u64 = 16;

/// Whether a row filter on `filter_columns` pays for a scan projecting `projection`. It reads
/// the filter columns once more and costs a little for each row it tests, and saves the other
/// projected columns' rows it drops. So it pays when those columns are wide: at least
/// [`ROW_FILTER_MIN_ROW_BYTES`] stored bytes a row, and [`ROW_FILTER_GAIN`] times the filter
/// columns' bytes, as a payload column next to a filter on a label is. Narrow columns decode
/// faster than the filter tests their rows (TPC-H q19 was 1.4 times slower with one). Without
/// a row filter the filter runs in DataFusion's `FilterExec` above the scan, which the
/// pushdown keeps (Inexact).
async fn row_filter_pays(
    files: &[Arc<StrataFile>],
    projection: &[usize],
    filter_columns: &[usize],
) -> DfResult<bool> {
    let (mut filter, mut other) = (0u64, 0u64);
    for &column in filter_columns {
        filter = filter.saturating_add(stored_bytes(files, column).await.map_err(df)?);
    }
    for &column in projection.iter().filter(|c| !filter_columns.contains(c)) {
        other = other.saturating_add(stored_bytes(files, column).await.map_err(df)?);
    }
    let rows: u64 = files.iter().map(|file| file.row_count()).sum();
    Ok(
        other >= rows.saturating_mul(ROW_FILTER_MIN_ROW_BYTES).max(1)
            && other >= filter.saturating_mul(ROW_FILTER_GAIN),
    )
}

/// The stored bytes of schema column `column` in `files`: its chunks, dictionary and payload
/// segments, and those of its map children and residual.
async fn stored_bytes(files: &[Arc<StrataFile>], column: usize) -> Result<u64> {
    let mut total = 0u64;
    for file in files {
        for (id, record) in file.directory().iter().enumerate() {
            let own = usize::try_from(record.column_id).is_ok_and(|c| c == column);
            let child = usize::try_from(record.parent).is_ok_and(|p| p == column);
            if !own && !child {
                continue;
            }
            let chunks: u64 = file
                .block_entries(id)
                .await?
                .iter()
                .map(|e| u64::from(e.segment_len))
                .sum();
            total = total
                .saturating_add(chunks)
                .saturating_add(u64::from(record.dict_len))
                .saturating_add(u64::from(record.payload_len));
        }
    }
    Ok(total)
}

fn translate_inner(expr: &Expr, table: &StrataTable) -> Pushed {
    match expr {
        Expr::BinaryExpr(BinaryExpr { left, op, right }) => binary(left, *op, right, table),
        Expr::InList(InList {
            expr,
            list,
            negated,
        }) => in_list(expr, list, *negated, table),
        Expr::IsNull(inner) => null_test(inner, table, Predicate::IsNull),
        Expr::IsNotNull(inner) => null_test(inner, table, Predicate::IsNotNull),
        Expr::Between(Between {
            expr,
            negated,
            low,
            high,
        }) => between(expr, *negated, low, high, table),
        // `Contains` is FALSE on a null row where `LIKE` is NULL; the two differ only under NOT.
        Expr::Not(inner) => match translate_inner(inner, table) {
            Pushed::Exact(p) if !has_contains(&p) => Pushed::Exact(Predicate::Not(Box::new(p))),
            _ => Pushed::No,
        },
        Expr::Like(like) => like_expr(like, table),
        Expr::Literal(ScalarValue::Boolean(v), _) => Pushed::Exact(Predicate::Const(*v)),
        _ => Pushed::No,
    }
}

/// Whether `p` has a `Contains` term.
fn has_contains(p: &Predicate) -> bool {
    match p {
        Predicate::Contains(..) => true,
        Predicate::And(terms) | Predicate::Or(terms) => terms.iter().any(has_contains),
        Predicate::Not(inner) => has_contains(inner),
        _ => false,
    }
}

/// The table index of a column expression, by name.
fn column_index(table: &StrataTable, expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Column(column) => table.schema.index_of(&column.name).ok(),
        _ => None,
    }
}

/// The table index and the file type of a column expression.
fn typed_column(table: &StrataTable, expr: &Expr) -> Option<(usize, DataType)> {
    let index = column_index(table, expr)?;
    Some((index, file_type(table, index)?))
}

/// The type of schema column `index` in the files. The files share one schema.
fn file_type(table: &StrataTable, index: usize) -> Option<DataType> {
    let schema = table.files.first()?.schema();
    schema
        .fields()
        .get(index)
        .map(|field| field.data_type().clone())
}

/// Whether `p` binds in every file: its columns are narrow and comparable, and its literals
/// convert to the column types. This is M4.2's `bind`, which decides what Strata can evaluate.
fn binds(p: &Predicate, table: &StrataTable) -> bool {
    table
        .files
        .iter()
        .all(|file| predicate::bind(p, file).is_ok())
}

/// `Exact(p)` when `p` binds in every file, otherwise `No`.
fn exact(table: &StrataTable, p: Predicate) -> Pushed {
    if binds(&p, table) {
        Pushed::Exact(p)
    } else {
        Pushed::No
    }
}

/// The `Lit` of a literal of type `value`, for a column of file type `dt`. `None` when the types
/// do not match, which keeps the filter unpushed. Float columns are never pushed: DataFusion
/// compares floats with `-0.0 == 0.0`, while Arrow's kernels, which Strata uses, order them by
/// the IEEE total order, so the two would disagree.
fn lit_of(value: &ScalarValue, dt: &DataType) -> Option<Lit> {
    fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Lit) -> Lit {
        v.map_or(Lit::Null, f)
    }
    let lit = match (dt, value) {
        (DataType::Boolean, ScalarValue::Boolean(v)) => opt(*v, Lit::Bool),
        (DataType::Int8, ScalarValue::Int8(v)) => opt(*v, |v| Lit::Int(i64::from(v))),
        (DataType::Int16, ScalarValue::Int16(v)) => opt(*v, |v| Lit::Int(i64::from(v))),
        (DataType::Int32, ScalarValue::Int32(v)) => opt(*v, |v| Lit::Int(i64::from(v))),
        (DataType::Int64, ScalarValue::Int64(v)) => opt(*v, Lit::Int),
        (DataType::UInt8, ScalarValue::UInt8(v)) => opt(*v, |v| Lit::UInt(u64::from(v))),
        (DataType::UInt16, ScalarValue::UInt16(v)) => opt(*v, |v| Lit::UInt(u64::from(v))),
        (DataType::UInt32, ScalarValue::UInt32(v)) => opt(*v, |v| Lit::UInt(u64::from(v))),
        (DataType::UInt64, ScalarValue::UInt64(v)) => opt(*v, Lit::UInt),
        // A timestamp's value is UTC whatever its zone, so the zones need not match.
        (DataType::Timestamp(TimeUnit::Second, _), ScalarValue::TimestampSecond(v, _))
        | (
            DataType::Timestamp(TimeUnit::Millisecond, _),
            ScalarValue::TimestampMillisecond(v, _),
        )
        | (
            DataType::Timestamp(TimeUnit::Microsecond, _),
            ScalarValue::TimestampMicrosecond(v, _),
        )
        | (DataType::Timestamp(TimeUnit::Nanosecond, _), ScalarValue::TimestampNanosecond(v, _))
        | (DataType::Date64, ScalarValue::Date64(v)) => opt(*v, Lit::Int),
        (DataType::Date32, ScalarValue::Date32(v)) => opt(*v, |v| Lit::Int(i64::from(v))),
        (DataType::Utf8, ScalarValue::Utf8View(v) | ScalarValue::Utf8(v)) => {
            opt(v.clone(), Lit::Utf8)
        }
        (DataType::Binary, ScalarValue::BinaryView(v) | ScalarValue::Binary(v)) => {
            opt(v.clone(), Lit::Bytes)
        }
        (DataType::FixedSizeBinary(w), ScalarValue::FixedSizeBinary(vw, v)) if w == vw => {
            opt(v.clone(), Lit::Bytes)
        }
        _ => return None,
    };
    Some(lit)
}

/// The comparison operator of a comparison `Operator`, or `None`.
fn cmp_of(op: Operator) -> Option<CmpOp> {
    match op {
        Operator::Eq => Some(CmpOp::Eq),
        Operator::NotEq => Some(CmpOp::NotEq),
        Operator::Lt => Some(CmpOp::Lt),
        Operator::LtEq => Some(CmpOp::LtEq),
        Operator::Gt => Some(CmpOp::Gt),
        Operator::GtEq => Some(CmpOp::GtEq),
        _ => None,
    }
}

/// The operator with its sides swapped: `lit < col` is `col > lit`.
fn flipped(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::LtEq => CmpOp::GtEq,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::GtEq => CmpOp::LtEq,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::NotEq => CmpOp::NotEq,
    }
}

fn binary(left: &Expr, op: Operator, right: &Expr, table: &StrataTable) -> Pushed {
    match op {
        Operator::And | Operator::Or => {
            let (a, b) = (translate_inner(left, table), translate_inner(right, table));
            combine(op == Operator::And, a, b)
        }
        Operator::Eq if matches!(left, Expr::ScalarFunction(_)) => map_key_eq(left, right, table),
        _ => comparison(left, op, right, table),
    }
}

/// `AND` or `OR` of two translations. A `No` on either side makes the whole `No`.
fn combine(and: bool, a: Pushed, b: Pushed) -> Pushed {
    fn parts(p: Pushed) -> Option<(Predicate, bool)> {
        match p {
            Pushed::Exact(p) => Some((p, true)),
            Pushed::Inexact(p) => Some((p, false)),
            Pushed::No => None,
        }
    }
    let (Some((pa, exact_a)), Some((pb, exact_b))) = (parts(a), parts(b)) else {
        return Pushed::No;
    };
    let p = if and {
        Predicate::And(vec![pa, pb])
    } else {
        Predicate::Or(vec![pa, pb])
    };
    if exact_a && exact_b {
        Pushed::Exact(p)
    } else {
        Pushed::Inexact(p)
    }
}

/// Whether `kept` followed by `p` still satisfies the predicate size limit, as one `And`. Pushdown
/// and `scan` both use it, so they accept the same filters.
fn fits_with(kept: &[Predicate], p: &Predicate) -> bool {
    let mut all = Vec::with_capacity(kept.len() + 1);
    all.extend(kept.iter().cloned());
    all.push(p.clone());
    predicate::check_size(&Predicate::And(all)).is_ok()
}

/// `column op literal` or `literal op column`.
fn comparison(left: &Expr, op: Operator, right: &Expr, table: &StrataTable) -> Pushed {
    let Some(cmp) = cmp_of(op) else {
        return Pushed::No;
    };
    let (column, value, cmp) = match (left, right) {
        (Expr::Column(_), Expr::Literal(value, _)) => (left, value, cmp),
        (Expr::Literal(value, _), Expr::Column(_)) => (right, value, flipped(cmp)),
        _ => return Pushed::No,
    };
    let Some((index, dt)) = typed_column(table, column) else {
        return Pushed::No;
    };
    let Some(lit) = lit_of(value, &dt) else {
        return Pushed::No;
    };
    exact(table, Predicate::Cmp(index, cmp, lit))
}

/// `column IN (literals)`, and `NOT IN`.
fn in_list(expr: &Expr, list: &[Expr], negated: bool, table: &StrataTable) -> Pushed {
    let Some((index, dt)) = typed_column(table, expr) else {
        return Pushed::No;
    };
    let lits: Option<Vec<Lit>> = list
        .iter()
        .map(|item| match item {
            Expr::Literal(value, _) => lit_of(value, &dt),
            _ => None,
        })
        .collect();
    let Some(lits) = lits else {
        return Pushed::No;
    };
    let p = Predicate::In(index, lits);
    exact(
        table,
        if negated {
            Predicate::Not(Box::new(p))
        } else {
            p
        },
    )
}

/// `column IS NULL` or `column IS NOT NULL`.
fn null_test(inner: &Expr, table: &StrataTable, make: fn(usize) -> Predicate) -> Pushed {
    match column_index(table, inner) {
        Some(index) => exact(table, make(index)),
        None => Pushed::No,
    }
}

/// `column BETWEEN low AND high`, with literal bounds.
fn between(expr: &Expr, negated: bool, low: &Expr, high: &Expr, table: &StrataTable) -> Pushed {
    let Some((index, dt)) = typed_column(table, expr) else {
        return Pushed::No;
    };
    let (Expr::Literal(low, _), Expr::Literal(high, _)) = (low, high) else {
        return Pushed::No;
    };
    let (Some(low), Some(high)) = (lit_of(low, &dt), lit_of(high, &dt)) else {
        return Pushed::No;
    };
    let p = Predicate::And(vec![
        Predicate::Cmp(index, CmpOp::GtEq, low),
        Predicate::Cmp(index, CmpOp::LtEq, high),
    ]);
    exact(
        table,
        if negated {
            Predicate::Not(Box::new(p))
        } else {
            p
        },
    )
}

/// `get_field(map, 'key') = literal`. Exact only when the key is promoted with `residual_hits ==
/// 0` in every file: `get_field` returns the first entry with the key, while `MapKeyEq` is NULL
/// for a duplicated key, and the two agree only when the key never reaches the residual.
fn map_key_eq(left: &Expr, right: &Expr, table: &StrataTable) -> Pushed {
    let Expr::ScalarFunction(ScalarFunction { func, args }) = left else {
        return Pushed::No;
    };
    if func.name() != "get_field" {
        return Pushed::No;
    }
    let [
        Expr::Column(map),
        Expr::Literal(ScalarValue::Utf8(Some(key)), _),
    ] = args.as_slice()
    else {
        return Pushed::No;
    };
    let Expr::Literal(ScalarValue::Utf8(value), _) = right else {
        return Pushed::No;
    };
    let Ok(index) = table.schema.index_of(&map.name) else {
        return Pushed::No;
    };
    if !table.exact_map_key(&map.name, key) {
        return Pushed::No;
    }
    let lit = value.clone().map_or(Lit::Null, Lit::Utf8);
    exact(table, Predicate::MapKeyEq(index, key.clone(), lit))
}

/// The substring `lit` of a pattern `%lit%`: 1 to 64 bytes, with no `%`, `_` or `\`.
fn needle(pattern: &str) -> Option<&str> {
    let lit = pattern.strip_prefix('%')?.strip_suffix('%')?;
    ((1..=64).contains(&lit.len()) && !lit.contains(['%', '_', '\\'])).then_some(lit)
}

/// `payload LIKE '%lit%'`: pushed as `Contains`, which is exact (the search checks the bytes).
fn like_expr(like: &Like, table: &StrataTable) -> Pushed {
    let Like {
        negated,
        expr,
        pattern,
        escape_char,
        case_insensitive,
    } = like;
    if *negated || *case_insensitive || escape_char.is_some() {
        return Pushed::No;
    }
    let Expr::Literal(ScalarValue::Utf8View(Some(pattern)) | ScalarValue::Utf8(Some(pattern)), _) =
        pattern.as_ref()
    else {
        return Pushed::No;
    };
    let Some(lit) = needle(pattern) else {
        return Pushed::No;
    };
    let Some(index) = column_index(table, expr) else {
        return Pushed::No;
    };
    let is_payload = table.files.iter().all(|file| {
        file.directory()
            .get(index)
            .is_some_and(|record| record.layer == Layer::Payload)
    });
    if !is_payload {
        return Pushed::No;
    }
    Pushed::Exact(Predicate::Contains(index, lit.as_bytes().to_vec()))
}

#[async_trait]
impl TableProvider for StrataTable {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DfResult<Vec<TableProviderFilterPushDown>> {
        // A filter is accepted only while the predicates accepted before it still fit together.
        let mut kept: Vec<Predicate> = Vec::new();
        Ok(filters
            .iter()
            .map(|filter| {
                let (p, answer) = match translate(filter, self) {
                    Pushed::Exact(p) => (p, TableProviderFilterPushDown::Exact),
                    Pushed::Inexact(p) => (p, TableProviderFilterPushDown::Inexact),
                    // The scan runs it on the rows its predicate selects ([`row_filter`]), and
                    // DataFusion's `FilterExec` above checks the rows it keeps again: the filter
                    // stays in the plan, so the plan's row estimates are those of a filter.
                    Pushed::No if row_filterable(filter, self) => {
                        return TableProviderFilterPushDown::Inexact;
                    }
                    Pushed::No => return TableProviderFilterPushDown::Unsupported,
                };
                if !fits_with(&kept, &p) {
                    return TableProviderFilterPushDown::Unsupported;
                }
                kept.push(p);
                answer
            })
            .collect())
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        // Translate the filters. DataFusion passes only the filters that `supports_filters_pushdown`
        // accepted, so a filter that does not translate here is an error, not a filter to skip.
        let mut kept: Vec<Predicate> = Vec::new();
        let mut residual: Vec<Expr> = Vec::new();
        let mut key = None;
        for filter in filters {
            let (p, exact) = match translate(filter, self) {
                Pushed::Exact(p) => (p, true),
                Pushed::Inexact(p) => (p, false),
                Pushed::No if row_filterable(filter, self) => {
                    residual.push(filter.clone());
                    continue;
                }
                Pushed::No => {
                    return Err(DataFusionError::Plan(format!(
                        "StrataTable cannot evaluate a pushed filter: {filter}"
                    )));
                }
            };
            if !fits_with(&kept, &p) {
                return Err(DataFusionError::Plan(format!(
                    "StrataTable cannot evaluate the pushed filters together: {filter}"
                )));
            }
            if exact && key.is_none() {
                key = self.sort_key_equality(filter);
            }
            kept.push(p);
        }

        // Prune the files whose sort-key filter proves the key absent.
        let mut files = Vec::new();
        for file in &self.files {
            let keep = match &key {
                Some((name, bytes)) => {
                    file.may_contain(name, bytes).await.map_err(df)? != Some(false)
                }
                None => true,
            };
            if keep {
                files.push(Arc::clone(file));
            }
        }

        // Prune the files where no row can match: their partitions would read nothing, and
        // the remaining files get the partitions instead.
        // A key lookup is already pruned by the key filter above.
        if !kept.is_empty() && key.is_none() {
            let predicate = match kept.len() {
                1 => kept.first().cloned(),
                _ => Some(Predicate::And(kept.clone())),
            };
            if let Some(predicate) = predicate {
                let mut kept_files = Vec::with_capacity(files.len());
                for file in files {
                    if file.may_match(&predicate).await.map_err(df)? {
                        kept_files.push(file);
                    }
                }
                files = kept_files;
            }
        }

        let indexes: Vec<usize> = projection
            .cloned()
            .unwrap_or_else(|| (0..self.schema.fields().len()).collect());
        let row_filter = match row_filter(state, self, &residual)? {
            Some(filter) if row_filter_pays(&files, &indexes, &filter.columns).await? => {
                Some(filter)
            }
            _ => None,
        };
        // Lookup mode: the key filter is the only filter and there is no limit.
        let lookup = match key {
            Some((_, bytes)) if kept.len() == 1 && limit.is_none() && row_filter.is_none() => {
                Some(bytes)
            }
            _ => None,
        };
        let predicate = match (lookup.is_some(), kept.len()) {
            (true, _) | (_, 0) => None,
            (false, 1) => kept.into_iter().next(),
            _ => Some(Predicate::And(kept)),
        };

        let schema = project_schema(&self.schema, projection)?;
        let target = state.config().target_partitions();
        let mut column_stats = Vec::with_capacity(indexes.len());
        for (&column, field) in indexes.iter().zip(schema.fields()) {
            column_stats.push(column_statistics(&files, column, field.data_type()).await?);
        }
        let mut exec = StrataExec::new(files, indexes, predicate, lookup, limit, schema, target);
        exec.column_stats = column_stats;
        exec.row_filter = row_filter;
        exec.selectivity = state.config_options().optimizer.default_filter_selectivity;
        Ok(Arc::new(exec))
    }
}

/// The statistics of `column` over `files`: for a signed integer or timestamp column, the
/// minimum and maximum of its chunk zones ([`StrataFile::int_range`]) and a null count of 0
/// when no chunk has a null; unknown otherwise. DataFusion derives a join key's distinct count
/// from its range, and without it estimates a join's output as its smaller input's row count,
/// so it builds hash tables on joins of millions of rows (TPC-DS query 72 used 64 GB).
async fn column_statistics(
    files: &[Arc<StrataFile>],
    column: usize,
    data_type: &DataType,
) -> DfResult<ColumnStatistics> {
    if matches!(data_type, DataType::Float32 | DataType::Float64) {
        return float_statistics(files, column, data_type).await;
    }
    let mut range: Option<(i64, i64)> = None;
    let mut nulls = false;
    for file in files {
        let Some((lo, hi, has_nulls)) = file.int_range(column).await.map_err(df)? else {
            return Ok(ColumnStatistics::new_unknown());
        };
        nulls |= has_nulls;
        range = Some(match range {
            None => (lo, hi),
            Some((a, b)) => (a.min(lo), b.max(hi)),
        });
    }
    let Some((lo, hi)) = range else {
        return Ok(ColumnStatistics::new_unknown());
    };
    let (Some(min), Some(max)) = (int_scalar(data_type, lo), int_scalar(data_type, hi)) else {
        return Ok(ColumnStatistics::new_unknown());
    };
    let mut stats = ColumnStatistics::new_unknown()
        .with_min_value(Precision::Exact(min))
        .with_max_value(Precision::Exact(max));
    if !nulls {
        stats = stats.with_null_count(Precision::Exact(0));
    }
    Ok(stats)
}

/// The statistics of the float `column` over `files`, from the zones of its non-NaN values:
/// inexact bounds, since a NaN sorts above every value and DataFusion answers `min` and `max`
/// from exact bounds; they let it estimate a range filter's rows as it does on Parquet (Appian
/// query 4 ordered its joins differently without them, and ran out of memory).
async fn float_statistics(
    files: &[Arc<StrataFile>],
    column: usize,
    data_type: &DataType,
) -> DfResult<ColumnStatistics> {
    let mut range: Option<(f64, f64)> = None;
    let mut nulls = false;
    for file in files {
        let Some((lo, hi, has_nulls)) = file.float_range(column).await.map_err(df)? else {
            return Ok(ColumnStatistics::new_unknown());
        };
        nulls |= has_nulls;
        range = Some(match range {
            None => (lo, hi),
            Some((a, b)) => (a.min(lo), b.max(hi)),
        });
    }
    let Some((lo, hi)) = range else {
        return Ok(ColumnStatistics::new_unknown());
    };
    // A Float32 zone holds the f32 values widened, so narrowing them back is exact.
    let (min, max) = match data_type {
        DataType::Float32 => (
            ScalarValue::Float32(Some(lo as f32)),
            ScalarValue::Float32(Some(hi as f32)),
        ),
        _ => (
            ScalarValue::Float64(Some(lo)),
            ScalarValue::Float64(Some(hi)),
        ),
    };
    let mut stats = ColumnStatistics::new_unknown()
        .with_min_value(Precision::Inexact(min))
        .with_max_value(Precision::Inexact(max));
    if !nulls {
        stats = stats.with_null_count(Precision::Exact(0));
    }
    Ok(stats)
}

/// `v` as a scalar of the signed integer or timestamp type `data_type`, or `None` for another
/// type or a value out of its range.
fn int_scalar(data_type: &DataType, v: i64) -> Option<ScalarValue> {
    Some(match data_type {
        DataType::Int8 => ScalarValue::Int8(Some(i8::try_from(v).ok()?)),
        DataType::Int16 => ScalarValue::Int16(Some(i16::try_from(v).ok()?)),
        DataType::Int32 => ScalarValue::Int32(Some(i32::try_from(v).ok()?)),
        DataType::Int64 => ScalarValue::Int64(Some(v)),
        DataType::Timestamp(unit, tz) => match unit {
            TimeUnit::Second => ScalarValue::TimestampSecond(Some(v), tz.clone()),
            TimeUnit::Millisecond => ScalarValue::TimestampMillisecond(Some(v), tz.clone()),
            TimeUnit::Microsecond => ScalarValue::TimestampMicrosecond(Some(v), tz.clone()),
            TimeUnit::Nanosecond => ScalarValue::TimestampNanosecond(Some(v), tz.clone()),
        },
        _ => return None,
    })
}

/// The files, projection and predicate of a plain scan ([`StrataExec::plain_scan`]).
pub(crate) type PlainScan<'a> = (&'a [Arc<StrataFile>], &'a [usize], Option<&'a Predicate>);

/// Reads Strata files for one table scan.
#[derive(Debug, Clone)]
pub struct StrataExec {
    files: Vec<Arc<StrataFile>>,
    /// One entry per partition: the file index and the rows it reads (`None`: all of them).
    parts: Vec<(usize, Option<Range<u32>>)>,
    projection: Vec<usize>,
    predicate: Option<Predicate>,
    lookup: Option<Vec<u8>>,
    limit: Option<usize>,
    /// The filters Strata does not translate, run by the scan ([`row_filter`]).
    row_filter: Option<RowFilter>,
    /// The row filter is DataFusion's whole `FilterExec` above, which the plan then drops
    /// ([`physical_row_filter`]).
    absorbed: bool,
    /// The order each partition outputs its rows in, when DataFusion pushed a sort down
    /// (`try_pushdown_sort`). Every key is a projected column.
    order: Option<Vec<PhysicalSortExpr>>,
    /// The rows each partition keeps of `order`, from the sort's limit (`with_fetch`).
    fetch: Option<usize>,
    /// The scan is one partition that reads the files in stages, best first ([`staged_top`]):
    /// set with a sort, a limit and a filter (a predicate or a row filter), when the first key
    /// is a signed integer or a timestamp.
    staged: bool,
    schema: SchemaRef,
    /// Statistics of the output columns: the minimum and maximum of integer and timestamp
    /// columns from their chunk zones, unknown for the others ([`column_statistics`]).
    column_stats: Vec<ColumnStatistics>,
    /// Percent of the rows a predicate or a lookup is estimated to keep: DataFusion's
    /// `default_filter_selectivity`, which a `FilterExec` above a plain scan would apply. Without
    /// it a filtered scan reports every row, and a small filtered table loses its place as the
    /// build side of a join (JOB query 56 ran out of memory).
    selectivity: u8,
    props: Arc<PlanProperties>,
    metrics: ExecutionPlanMetricsSet,
}

/// The output position and options of each key of `order`, when every key is a column of the
/// output (`width` columns) and `order` is not empty.
fn sort_keys(order: &[PhysicalSortExpr], width: usize) -> Option<Vec<(usize, SortOptions)>> {
    if order.is_empty() {
        return None;
    }
    order
        .iter()
        .map(|key| {
            let column = key.expr.downcast_ref::<PhysicalColumn>()?;
            (column.index() < width).then_some((column.index(), key.options))
        })
        .collect()
}

/// The plan properties of a scan with `partitions` partitions. Its rows are in `order` when it
/// is given; else in sort-key order when the key is projected, since each partition reads one
/// contiguous row range of one file, which is sorted by the key.
fn plan_properties(
    files: &[Arc<StrataFile>],
    projection: &[usize],
    schema: &SchemaRef,
    partitions: usize,
    order: Option<&[PhysicalSortExpr]>,
) -> PlanProperties {
    let mut eq = EquivalenceProperties::new(Arc::clone(schema));
    match order {
        Some(order) => {
            eq.add_ordering(order.iter().cloned());
        }
        None => {
            let sort_key = files.first().and_then(|file| file.sort_key_column());
            if let Some(key) = sort_key
                && let Some(position) = projection.iter().position(|&c| c == key)
                && let Some(field) = schema.fields().get(position)
            {
                eq.add_ordering([PhysicalSortExpr::new(
                    Arc::new(PhysicalColumn::new(field.name(), position)),
                    SortOptions {
                        descending: false,
                        nulls_first: false,
                    },
                )]);
            }
        }
    }
    // Prototype shortcut: the batch stream does not consume DataFusion's cooperative
    // budget. `Cooperative` avoids the `CooperativeExec` wrapper.
    PlanProperties::new(
        eq,
        Partitioning::UnknownPartitioning(partitions),
        EmissionType::Incremental,
        Boundedness::Bounded,
    )
    .with_scheduling_type(SchedulingType::Cooperative)
}

/// Sorts the rows of `batches` (all of `schema`) by `keys` and keeps the first `fetch`.
fn sort_rows(
    batches: Vec<RecordBatch>,
    schema: &SchemaRef,
    keys: &[(usize, SortOptions)],
    fetch: Option<usize>,
) -> Result<Option<RecordBatch>> {
    if batches.iter().all(|batch| batch.num_rows() == 0) {
        return Ok(None);
    }
    let all = concat_batches(schema, &batches)?;
    let columns = keys
        .iter()
        .map(|&(position, options)| {
            let values = all
                .columns()
                .get(position)
                .ok_or_else(|| StrataError::Schema {
                    reason: format!("internal: no sort column {position}"),
                })?;
            Ok(SortColumn {
                values: Arc::clone(values),
                options: Some(options),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let indices = lexsort_to_indices(&columns, fetch)?;
    Ok(Some(take_record_batch(&all, &indices)?))
}

/// How much each stage of [`staged_top`] grows: 1, 4, 16, … files, so that a needle found in
/// the first file reads one file, and one found nowhere takes few stages.
const STAGE_GROWTH: usize = 4;

/// The first `fetch` rows of `files` by `keys`, for a scan whose first sort key is the signed
/// integer or timestamp column `top.column`, filtered by `predicate` and `row_filter`. Also returns the I/O of
/// every file read.
///
/// The files are ranked by the best value their zones allow for the first key (the largest
/// for a descending sort), and read in stages of 1, 4, 16, … files, the files of a stage at
/// once, each in its own task. Once `fetch` rows are kept, the k-th one's first key `v` bounds
/// the rest: later stages read with `key >= v` (`<=` ascending) added to the predicate, so a
/// file whose zones rule it out reads no payload. Rows tied with `v` pass the bound, and the
/// kept rows are sorted by every key each time, so the result is the full sort's. With files
/// that are time slices, a needle found in the newest file reads that file only.
async fn staged_top(
    files: &[Arc<StrataFile>],
    projection: &[usize],
    predicate: Option<&Predicate>,
    row_filter: Option<&RowFilter>,
    top: TopRows,
    keys: &[(usize, SortOptions)],
    fetch: usize,
) -> DfResult<(Option<RecordBatch>, IoStats)> {
    let descending = top.options.descending;
    // Files without a range sort last; they are read, and checked against the bound, as any.
    // A file may hold nulls unless its zones say otherwise.
    let mut ranked: Vec<(Option<i64>, bool, &Arc<StrataFile>)> = Vec::with_capacity(files.len());
    for file in files {
        let range = file.int_range(top.column).await.map_err(df)?;
        let best = range.map(|(lo, hi, _)| if descending { hi } else { lo });
        let nulls = range.is_none_or(|(_, _, nulls)| nulls);
        ranked.push((best, nulls, file));
    }
    ranked.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) if descending => y.cmp(&x),
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let first = keys
        .first()
        .map(|&(position, _)| position)
        .ok_or_else(|| DataFusionError::Internal("a staged scan without sort keys".to_string()))?;
    let mut io = IoStats::default();
    let mut best: Option<RecordBatch> = None;
    let mut schema: Option<SchemaRef> = None;
    let mut next = 0;
    let mut stage = 1;
    while next < ranked.len() {
        let bound = match &best {
            Some(batch) if batch.num_rows() >= fetch => {
                kth_int(batch, first, fetch - 1).map_err(df)?
            }
            _ => None,
        };
        let end = next.saturating_add(stage).min(ranked.len());
        let group = ranked.get(next..end).unwrap_or_default();
        let threads = crate::parallel::cores().div_ceil(group.len().max(1));
        // Each file of the stage is its own task, so their decoding runs in parallel.
        let tasks: Vec<_> = group
            .iter()
            .map(|&(_, nulls, file)| {
                // Rows that cannot pass the k-th kept row are left out. A null key ranks first
                // when nulls sort first, so it stays in, but only in a file that may hold one:
                // the zones cannot prune an `OR`.
                let predicate = match bound {
                    Some(v) => {
                        let op = if descending { CmpOp::GtEq } else { CmpOp::LtEq };
                        let cmp = Predicate::Cmp(top.column, op, Lit::Int(v));
                        let bound = if top.options.nulls_first && nulls {
                            Predicate::Or(vec![Predicate::IsNull(top.column), cmp])
                        } else {
                            cmp
                        };
                        match predicate {
                            Some(p) => Some(Predicate::And(vec![p.clone(), bound])),
                            None => Some(bound),
                        }
                    }
                    None => predicate.cloned(),
                };
                let file = Arc::clone(file);
                let projection = projection.to_vec();
                let row_filter = row_filter.cloned();
                SpawnedTask::spawn(async move {
                    let before = file.io_stats();
                    let scanned = file
                        .scan_batches(
                            &ScanRequest {
                                projection,
                                predicate,
                                row_ranges: None,
                                limit: None,
                                kind: ScanKind::Scan,
                                views: true,
                                top: Some(top),
                                row_filter,
                            },
                            threads,
                        )
                        .await?;
                    let scan_schema = scanned.schema();
                    let rows = scanned.collect::<Result<Vec<_>>>()?;
                    let after = file.io_stats();
                    Ok::<_, StrataError>((scan_schema, rows, before, after))
                })
            })
            .collect();
        let mut rows: Vec<RecordBatch> = best.take().into_iter().collect();
        for task in tasks {
            let (scan_schema, batches, before, after) = task
                .join_unwind()
                .await
                .map_err(|e| DataFusionError::ExecutionJoin(Box::new(e)))?
                .map_err(df)?;
            io.requests += after.requests.saturating_sub(before.requests);
            io.bytes += after.bytes.saturating_sub(before.bytes);
            io.waves += after.waves.saturating_sub(before.waves);
            schema.get_or_insert(scan_schema);
            rows.extend(batches);
        }
        if let Some(schema) = &schema {
            best = sort_rows(rows, schema, keys, Some(fetch)).map_err(df)?;
        }
        next = end;
        stage = stage.saturating_mul(STAGE_GROWTH);
    }
    Ok((best, io))
}

/// Row `row` of column `position` of `batch` as an `i64`, or `None` when it is null.
fn kth_int(batch: &RecordBatch, position: usize, row: usize) -> Result<Option<i64>> {
    let column = batch
        .columns()
        .get(position)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("internal: no sort column {position}"),
        })?;
    if row >= column.len() {
        return Ok(None);
    }
    let value = cast(&column.slice(row, 1), &DataType::Int64)?;
    let ints = value
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>()
        .ok_or_else(|| StrataError::Schema {
            reason: "internal: sort key is not an integer".to_string(),
        })?;
    Ok(arrow::array::Array::is_valid(ints, 0).then(|| ints.value(0)))
}

impl StrataExec {
    /// An exec over `files`. `schema` is the output schema, the projection of the table schema.
    /// A scan of narrow columns splits each file into row ranges so that there are about
    /// `target_partitions` partitions. A lookup, and a scan that reads a payload column, read
    /// each file whole: a row's value shares chunks with earlier rows, so the partitions of one
    /// file would fetch and decode the same frames, 25% more on WildChat (`real-data.md`
    /// § Payload scans). Such a partition builds its batches on its share of the cores.
    fn new(
        files: Vec<Arc<StrataFile>>,
        projection: Vec<usize>,
        predicate: Option<Predicate>,
        lookup: Option<Vec<u8>>,
        limit: Option<usize>,
        schema: SchemaRef,
        target_partitions: usize,
    ) -> StrataExec {
        let whole = lookup.is_some() || reads_payload(&files, &projection);
        let parts = split_files(&files, if whole { 1 } else { target_partitions });
        let props = plan_properties(&files, &projection, &schema, parts.len().max(1), None);
        StrataExec {
            files,
            parts,
            projection,
            predicate,
            lookup,
            limit,
            row_filter: None,
            absorbed: false,
            order: None,
            fetch: None,
            staged: false,
            column_stats: Statistics::unknown_column(&schema),
            selectivity: 100,
            schema,
            props: Arc::new(props),
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }

    /// This scan with each partition's rows sorted by `order`, keeping the first `fetch`.
    fn ordered(&self, order: Vec<PhysicalSortExpr>, fetch: Option<usize>) -> StrataExec {
        let first_int = sort_keys(&order, self.projection.len())
            .and_then(|keys| keys.first().copied())
            .and_then(|(position, _)| self.schema.fields().get(position))
            .is_some_and(|field| {
                matches!(
                    field.data_type(),
                    DataType::Int8
                        | DataType::Int16
                        | DataType::Int32
                        | DataType::Int64
                        | DataType::Timestamp(_, _)
                )
            });
        let staged =
            fetch.is_some() && first_int && (self.row_filter.is_some() || self.predicate.is_some());
        let partitions = if staged { 1 } else { self.parts.len().max(1) };
        let props = plan_properties(
            &self.files,
            &self.projection,
            &self.schema,
            partitions,
            Some(&order),
        );
        StrataExec {
            files: self.files.clone(),
            parts: self.parts.clone(),
            projection: self.projection.clone(),
            predicate: self.predicate.clone(),
            lookup: self.lookup.clone(),
            limit: self.limit,
            row_filter: self.row_filter.clone(),
            absorbed: self.absorbed,
            order: Some(order),
            fetch,
            staged,
            column_stats: self.column_stats.clone(),
            selectivity: self.selectivity,
            schema: Arc::clone(&self.schema),
            props: Arc::new(props),
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }

    /// The files, projection and predicate of a plain scan: no lookup, limit, row filter or
    /// pushed sort. `None` otherwise. An aggregate over a plain scan may run in Strata
    /// (`aggregate.rs`).
    pub(crate) fn plain_scan(&self) -> Option<PlainScan<'_>> {
        (self.lookup.is_none()
            && self.limit.is_none()
            && self.row_filter.is_none()
            && self.order.is_none())
        .then_some((
            self.files.as_slice(),
            self.projection.as_slice(),
            self.predicate.as_ref(),
        ))
    }

    /// The number of partitions: one per row range, and one when there is no file.
    fn partitions(&self) -> usize {
        if self.staged {
            1
        } else {
            self.parts.len().max(1)
        }
    }
}

impl DisplayAs for StrataExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let predicate = match &self.predicate {
            Some(p) => format!("{p:?}"),
            None => "none".to_string(),
        };
        write!(
            f,
            "StrataExec: files={}, partitions={}, lookup={}, predicate={predicate}",
            self.files.len(),
            self.partitions(),
            self.lookup.is_some()
        )?;
        if let Some(filter) = &self.row_filter {
            write!(f, ", row_filter={:?}", filter.columns)?;
        }
        if let Some(order) = &self.order {
            let keys: Vec<String> = order.iter().map(ToString::to_string).collect();
            write!(f, ", order=[{}], fetch={:?}", keys.join(", "), self.fetch)?;
            if self.staged {
                write!(f, ", staged")?;
            }
        }
        Ok(())
    }
}

impl ExecutionPlan for StrataExec {
    fn name(&self) -> &str {
        "StrataExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.props
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    /// The row count, from the files' postscripts: exact for a plain scan, so DataFusion
    /// answers `count(*)` without reading a column; an estimate with a predicate, a lookup or a
    /// limit. Column statistics: the zone ranges of [`column_statistics`], inexact when rows
    /// are filtered out.
    fn statistics_from_inputs(
        &self,
        _input_stats: &[Arc<Statistics>],
        args: &StatisticsArgs,
    ) -> DfResult<Arc<Statistics>> {
        let rows_of = |(file, range): &(usize, Option<Range<u32>>)| -> u64 {
            match range {
                Some(r) => u64::from(r.end.saturating_sub(r.start)),
                None => self.files.get(*file).map_or(0, |f| f.row_count()),
            }
        };
        let rows: u64 = match args.partition() {
            Some(i) if !self.staged => self.parts.get(i).map_or(0, rows_of),
            _ => self.parts.iter().map(rows_of).sum(),
        };
        let mut rows = usize::try_from(rows).unwrap_or(usize::MAX);
        // How a limit splits across partitions is not modelled: it only caps the estimate.
        let cap = self.limit.or(self.fetch);
        // A row filter is counted only when the plan has no `FilterExec` above to estimate its
        // rows.
        let selected = self.predicate.is_some() || self.lookup.is_some() || self.absorbed;
        let filtered = selected || cap.is_some();
        if selected && rows > 0 {
            let kept = rows as f64 * f64::from(self.selectivity.min(100)) / 100.0;
            // `kept` is at most `rows`, so the cast back cannot overflow.
            rows = (kept.ceil() as usize).max(1);
        }
        if let Some(limit) = cap {
            rows = rows.min(limit);
        }
        let rows = if filtered {
            Precision::Inexact(rows)
        } else {
            Precision::Exact(rows)
        };
        let mut stats = Statistics::new_unknown(&self.schema).with_num_rows(rows);
        // Rows times the fixed width of the projected columns, absent with a variable-width
        // column, as a Parquet scan reports it: DataFusion picks a hash join's build side by
        // bytes when both inputs have them, so without them a Strata plan can differ from
        // Parquet's (Appian query 4 ran out of memory).
        stats.calculate_total_byte_size(&self.schema);
        if self.column_stats.len() == stats.column_statistics.len() {
            stats.column_statistics = if filtered {
                self.column_stats
                    .iter()
                    .map(|c| c.clone().to_inexact())
                    .collect()
            } else {
                self.column_stats.clone()
            };
        }
        Ok(Arc::new(stats))
    }

    /// A sort whose first key is a narrow column of every file, and whose other keys are
    /// projected columns, is done here exactly: each partition outputs its rows in that order,
    /// so DataFusion drops its `SortExec` and passes the sort's limit to `with_fetch`. With a
    /// limit, a partition keeps only the rows that can rank in it ([`TopRows`]) before it reads
    /// the rest of the projection. Not for a lookup, a scan with a limit of its own, or a
    /// nested first key.
    fn try_pushdown_sort(
        &self,
        order: &[PhysicalSortExpr],
    ) -> DfResult<SortOrderPushdownResult<Arc<dyn ExecutionPlan>>> {
        if self.lookup.is_some() || self.limit.is_some() || self.order.is_some() {
            return Ok(SortOrderPushdownResult::Unsupported);
        }
        let Some(keys) = sort_keys(order, self.projection.len()) else {
            return Ok(SortOrderPushdownResult::Unsupported);
        };
        let first = keys.first().and_then(|&(position, _)| {
            Some((
                *self.projection.get(position)?,
                self.schema.fields().get(position)?,
            ))
        });
        let Some((column, field)) = first else {
            return Ok(SortOrderPushdownResult::Unsupported);
        };
        let narrow = self.files.iter().all(|file| {
            file.directory()
                .get(column)
                .is_some_and(|record| record.layer == Layer::Narrow)
        });
        if !narrow || field.data_type().is_nested() {
            return Ok(SortOrderPushdownResult::Unsupported);
        }
        Ok(SortOrderPushdownResult::Exact {
            inner: Arc::new(self.ordered(order.to_vec(), None)),
        })
    }

    /// Keeps the first `limit` rows of each partition's order; only a scan with a pushed sort
    /// takes a limit this way.
    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        let order = self.order.clone()?;
        Some(Arc::new(self.ordered(order, limit)))
    }

    fn fetch(&self) -> Option<usize> {
        self.fetch
    }

    /// Before the plan is optimized, takes the filters of a `FilterExec` above as its row
    /// filter when it runs one already, all or none ([`physical_row_filter`]), and reports them
    /// supported, so DataFusion drops the `FilterExec`. Every other filter, in every other
    /// phase, is reported unsupported, with no updated node.
    fn handle_child_pushdown_result(
        &self,
        phase: FilterPushdownPhase,
        child_pushdown_result: ChildPushdownResult,
        _config: &ConfigOptions,
    ) -> DfResult<FilterPushdownPropagation<Arc<dyn ExecutionPlan>>> {
        let filters: Vec<Arc<dyn PhysicalExpr>> = child_pushdown_result
            .parent_filters
            .into_iter()
            .map(|f| f.filter)
            .collect();
        let unsupported = vec![PushedDown::No; filters.len()];
        if matches!(phase, FilterPushdownPhase::Pre) {
            // A scan that runs a row filter takes the whole `FilterExec` over: its rows reach
            // the operators above as the scan finds them, where the `FilterExec` held them
            // until it had a full batch, and a sort above can be pushed into the scan.
            let takes = self.row_filter.is_some()
                && self.lookup.is_none()
                && self.limit.is_none()
                && self.order.is_none()
                && !self.staged
                && !filters.is_empty();
            if takes
                && let Some(row_filter) =
                    physical_row_filter(&filters, &self.projection, &self.schema)
            {
                let mut exec = self.clone();
                exec.row_filter = Some(row_filter);
                exec.absorbed = true;
                exec.metrics = ExecutionPlanMetricsSet::new();
                let supported = vec![PushedDown::Yes; filters.len()];
                return Ok(
                    FilterPushdownPropagation::with_parent_pushdown_result(supported)
                        .with_updated_node(Arc::new(exec)),
                );
            }
            return Ok(FilterPushdownPropagation::with_parent_pushdown_result(
                unsupported,
            ));
        }
        Ok(FilterPushdownPropagation::with_parent_pushdown_result(
            unsupported,
        ))
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(
            &Arc<dyn ::datafusion::physical_expr::PhysicalExpr>,
        ) -> DfResult<TreeNodeRecursion>,
    ) -> DfResult<TreeNodeRecursion> {
        Ok(TreeNodeRecursion::Continue)
    }

    fn replace_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
        _options: ReplaceChildrenOptions,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        self.replace_children(
            children,
            ReplaceChildrenOptions::new(ChildrenPropertiesMode::Recompute),
        )
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> DfResult<SendableRecordBatchStream> {
        if partition >= self.partitions() {
            return Err(DataFusionError::Internal(format!(
                "StrataExec partition {partition} out of range"
            )));
        }
        let output_rows = MetricBuilder::new(&self.metrics).output_rows(partition);
        let elapsed = MetricBuilder::new(&self.metrics).elapsed_compute(partition);
        let bytes = MetricBuilder::new(&self.metrics).counter("strata_bytes_fetched", partition);
        let requests = MetricBuilder::new(&self.metrics).counter("strata_requests", partition);
        let waves = MetricBuilder::new(&self.metrics).counter("strata_waves", partition);

        let (file, row_ranges) = match self.parts.get(partition) {
            Some((f, rows)) => (
                self.files.get(*f).map(Arc::clone),
                rows.clone().map(|r| vec![r]),
            ),
            None => (None, None),
        };
        let projection = self.projection.clone();
        let predicate = self.predicate.clone();
        let row_filter = self.row_filter.clone();
        let lookup = self.lookup.clone();
        let limit = self.limit;
        let keys = match &self.order {
            Some(order) => Some(sort_keys(order, self.projection.len()).ok_or_else(|| {
                DataFusionError::Internal("StrataExec sort keys are not columns".to_string())
            })?),
            None => None,
        };
        let fetch = self.fetch;
        // The first sort key bounds the rows a partition keeps: those that can rank in `fetch`.
        let top = match (&keys, fetch) {
            (Some(keys), Some(k)) => keys.first().and_then(|&(position, options)| {
                Some(TopRows {
                    column: *self.projection.get(position)?,
                    options,
                    k,
                })
            }),
            _ => None,
        };
        let schema = Arc::clone(&self.schema);
        if self.staged
            && let (Some(keys), Some(top), Some(fetch)) = (keys.clone(), top, fetch)
        {
            let files = self.files.clone();
            let (predicate, row_filter) = (predicate.clone(), row_filter.clone());
            let fut = async move {
                let (best, io) = staged_top(
                    &files,
                    &projection,
                    predicate.as_ref(),
                    row_filter.as_ref(),
                    top,
                    &keys,
                    fetch,
                )
                .await?;
                bytes.add(usize::try_from(io.bytes).unwrap_or(usize::MAX));
                requests.add(usize::try_from(io.requests).unwrap_or(usize::MAX));
                waves.add(usize::try_from(io.waves).unwrap_or(usize::MAX));
                Ok::<_, DataFusionError>(best)
            };
            let stream = futures::stream::once(fut)
                .map_ok(move |best| {
                    let (elapsed, output_rows, schema) =
                        (elapsed.clone(), output_rows.clone(), Arc::clone(&schema));
                    futures::stream::iter(best.into_iter().map(move |batch| {
                        let _timer = elapsed.timer();
                        let batch = to_table_batch(&batch, &schema).map_err(df)?;
                        output_rows.add(batch.num_rows());
                        Ok(batch)
                    }))
                })
                .try_flatten();
            return Ok(Box::pin(RecordBatchStreamAdapter::new(
                Arc::clone(&self.schema),
                stream,
            )));
        }
        // Partitions run at once: each builds its batches on its share of the cores.
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let threads = cores.div_ceil(self.partitions()).max(1);
        // Only a scan that rebuilds payload values builds its batches ahead: handing small,
        // cheap batches over a channel cost more than it saved (`real-data.md` § Payload scans).
        let payload = reads_payload(&self.files, &self.projection);
        type Batches = Box<dyn Iterator<Item = Result<RecordBatch>> + Send>;
        /// A partition's batches: built by one scan (and maybe ahead), or streamed from scans of
        /// its rows in pieces ([`pieces`]).
        enum Out {
            Built(Batches, bool),
            Pieces(futures::stream::BoxStream<'static, DfResult<RecordBatch>>),
        }
        let fut = async move {
            let Some(file) = file else {
                return Ok::<Out, DataFusionError>(Out::Built(Box::new(std::iter::empty()), false));
            };
            // A plain scan of many rows reads them in pieces: the partition holds one piece's
            // values at a time, and DataFusion gets the first batches while it reads the rest.
            // The next piece's reads run while the current one's batches are built, so on an
            // object store a partition does not wait a round trip between pieces.
            let plain =
                lookup.is_none() && keys.is_none() && limit.is_none() && top.is_none() && !payload;
            let ranges = if plain {
                pieces(row_ranges.as_deref(), file.row_count())
            } else {
                Vec::new()
            };
            if ranges.len() > 1 {
                let stream = futures::stream::iter(ranges)
                    .map(move |range| {
                        let file = Arc::clone(&file);
                        let request = ScanRequest {
                            projection: projection.clone(),
                            predicate: predicate.clone(),
                            row_ranges: Some(vec![range]),
                            limit: None,
                            kind: ScanKind::Scan,
                            views: true,
                            top: None,
                            row_filter: row_filter.clone(),
                        };
                        let (bytes, requests, waves) =
                            (bytes.clone(), requests.clone(), waves.clone());
                        async move {
                            let before = file.io_stats();
                            let scanned = file.scan_batches(&request, threads).await.map_err(df)?;
                            let after = file.io_stats();
                            let delta = |a: u64, b: u64| usize::try_from(a.saturating_sub(b));
                            bytes.add(delta(after.bytes, before.bytes).unwrap_or(usize::MAX));
                            requests
                                .add(delta(after.requests, before.requests).unwrap_or(usize::MAX));
                            waves.add(delta(after.waves, before.waves).unwrap_or(usize::MAX));
                            Ok::<_, DataFusionError>(futures::stream::iter(
                                scanned.map(|batch| batch.map_err(df)),
                            ))
                        }
                    })
                    .buffered(2)
                    .try_flatten()
                    .boxed();
                return Ok(Out::Pieces(stream));
            }
            let before = file.io_stats();
            // Whether the batches are built ahead: a payload scan with more than one step of
            // batches left; the others are built already, in one step, or cheap.
            let mut ahead = false;
            let batches: Batches = match lookup {
                Some(key) => Box::new(
                    file.lookup_scan(&key, &projection, true)
                        .await
                        .map_err(df)?
                        .into_iter()
                        .map(Ok),
                ),
                None => {
                    let scanned = file
                        .scan_batches(
                            &ScanRequest {
                                projection,
                                predicate,
                                row_ranges,
                                limit,
                                kind: ScanKind::Scan,
                                views: true,
                                top,
                                row_filter,
                            },
                            threads,
                        )
                        .await
                        .map_err(df)?;
                    match &keys {
                        // The partition's rows, sorted: at most `fetch` of them with a limit.
                        Some(keys) => {
                            let scan_schema = scanned.schema();
                            let rows = scanned.collect::<Result<Vec<_>>>().map_err(df)?;
                            let sorted = sort_rows(rows, &scan_schema, keys, fetch).map_err(df)?;
                            Box::new(sorted.into_iter().map(Ok))
                        }
                        None => {
                            ahead = payload && scanned.steps_left() > 1;
                            Box::new(scanned)
                        }
                    }
                }
            };
            let after = file.io_stats();
            bytes.add(
                usize::try_from(after.bytes.saturating_sub(before.bytes)).unwrap_or(usize::MAX),
            );
            requests.add(
                usize::try_from(after.requests.saturating_sub(before.requests))
                    .unwrap_or(usize::MAX),
            );
            waves.add(
                usize::try_from(after.waves.saturating_sub(before.waves)).unwrap_or(usize::MAX),
            );
            // The batches are built as DataFusion asks for them, so a partition holds a few
            // batches and the frames they share, not the whole file's values.
            Ok(Out::Built(batches, ahead))
        };
        let stream = futures::stream::once(fut)
            .map_ok(move |out| {
                // `map_ok` takes an `FnMut`; it runs once.
                let (elapsed, output_rows, schema) =
                    (elapsed.clone(), output_rows.clone(), Arc::clone(&schema));
                let batches = match out {
                    Out::Built(batches, true) => built_ahead(batches, threads)
                        .map(|batch| batch.map_err(df))
                        .boxed(),
                    Out::Built(batches, false) => {
                        futures::stream::iter(batches.map(|batch| batch.map_err(df))).boxed()
                    }
                    Out::Pieces(stream) => stream,
                };
                batches.map(move |batch| {
                    let _timer = elapsed.timer();
                    let batch = to_table_batch(&batch?, &schema).map_err(df)?;
                    output_rows.add(batch.num_rows());
                    Ok(batch)
                })
            })
            .try_flatten();
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            Arc::clone(&self.schema),
            stream,
        )))
    }

    fn metrics(&self) -> Option<MetricsSet> {
        Some(self.metrics.clone_inner())
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
    use arrow::array::{
        Array, FixedSizeBinaryArray, MapBuilder, RecordBatch, StringArray, StringBuilder,
        TimestampNanosecondArray,
    };
    use arrow::datatypes::{DataType, Field, Schema};
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

    use super::*;
    use crate::StrataWriter;
    use crate::testgen::{wide_batch, wide_schema};
    use ::datafusion::common::ScalarValue as Sv;
    use ::datafusion::functions::core::expr_fn::get_field;
    use ::datafusion::logical_expr::{cast, col, lit};

    /// Writes `batch` with the trace/start-time sort key, and opens it in memory.
    async fn open(batch: &RecordBatch, config: Config) -> Arc<StrataFile> {
        let mut writer =
            StrataWriter::new(batch.schema(), &["trace_id", "start_time"], config).unwrap();
        writer.write(batch).unwrap();
        let (bytes, _) = writer.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("file.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        Arc::new(
            StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap(),
        )
    }

    /// The main file: `wide_batch(3_000)` with the default config.
    async fn main_table() -> StrataTable {
        let file = open(&wide_batch(3_000), Config::default()).await;
        StrataTable::new(vec![file]).unwrap()
    }

    /// A batch with a payload `body` and a `Map` whose key `k` is present once per row, so that
    /// `Like` and `MapKeyEq` can be pushed. Its config makes `body` a payload column.
    fn payload_batch() -> RecordBatch {
        let n = 200usize;
        let trace: Vec<[u8; 16]> = (0..n)
            .map(|i| {
                let mut id = [0u8; 16];
                id[8..].copy_from_slice(&(i as u64).to_be_bytes());
                id
            })
            .collect();
        let trace_ids = FixedSizeBinaryArray::try_from_iter(trace.iter()).unwrap();
        let times = TimestampNanosecondArray::from_iter_values((0..n).map(|i| i as i64 * 1_000))
            .with_timezone("UTC");
        let body = StringArray::from_iter_values((0..n).map(|i| {
            if i % 3 == 0 {
                "some abc inside".to_string()
            } else {
                format!("other {i}")
            }
        }));
        let mut maps = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        for i in 0..n {
            maps.keys().append_value("k");
            maps.values().append_value(format!("v{}", i % 2));
            maps.append(true).unwrap();
        }
        let m = maps.finish();
        let schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("start_time", times.data_type().clone(), false),
            Field::new("body", DataType::Utf8, false),
            Field::new("m", m.data_type().clone(), false),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(trace_ids),
                Arc::new(times),
                Arc::new(body),
                Arc::new(m),
            ],
        )
        .unwrap()
    }

    /// The second file: [`payload_batch`] with `payload_min_avg: 1`.
    async fn payload_table() -> StrataTable {
        let config = Config {
            payload_min_avg: 1,
            ..Config::default()
        };
        StrataTable::new(vec![open(&payload_batch(), config).await]).unwrap()
    }

    fn index(table: &StrataTable, name: &str) -> usize {
        table.schema.index_of(name).unwrap()
    }

    fn utf8view(s: &str) -> Expr {
        lit(Sv::Utf8View(Some(s.to_string())))
    }

    #[tokio::test]
    async fn translate_comparisons() {
        let t = main_table().await;
        let status = index(&t, "status");
        let tokens = index(&t, "input_tokens");
        assert_eq!(
            translate(&col("status").eq(utf8view("ok")), &t),
            Pushed::Exact(Predicate::Cmp(status, CmpOp::Eq, Lit::Utf8("ok".into())))
        );
        assert_eq!(
            translate(&lit(5i64).lt(col("input_tokens")), &t),
            Pushed::Exact(Predicate::Cmp(tokens, CmpOp::Gt, Lit::Int(5)))
        );
        assert_eq!(
            translate(&col("input_tokens").gt(lit(5i64)), &t),
            Pushed::Exact(Predicate::Cmp(tokens, CmpOp::Gt, Lit::Int(5)))
        );
        assert_eq!(
            translate(&col("status").eq(lit(Sv::Utf8(None))), &t),
            Pushed::Exact(Predicate::Cmp(status, CmpOp::Eq, Lit::Null))
        );
    }

    #[tokio::test]
    async fn translate_in_list_and_nulls() {
        let t = main_table().await;
        let tokens = index(&t, "input_tokens");
        assert_eq!(
            translate(
                &col("input_tokens").in_list(vec![lit(1i64), lit(2i64)], true),
                &t
            ),
            Pushed::Exact(Predicate::Not(Box::new(Predicate::In(
                tokens,
                vec![Lit::Int(1), Lit::Int(2)]
            ))))
        );
        assert_eq!(
            translate(&col("input_tokens").is_null(), &t),
            Pushed::Exact(Predicate::IsNull(tokens))
        );
        assert_eq!(
            translate(&col("input_tokens").is_not_null(), &t),
            Pushed::Exact(Predicate::IsNotNull(tokens))
        );
    }

    #[tokio::test]
    async fn translate_refuses_what_strata_cannot_evaluate() {
        let t = main_table().await;
        // A Cast on the column.
        assert_eq!(
            translate(
                &cast(col("input_tokens"), DataType::Float64).gt(lit(1.0)),
                &t
            ),
            Pushed::No
        );
        assert_eq!(
            translate(
                &cast(col("input_tokens"), DataType::Int64).eq(lit(1i64)),
                &t
            ),
            Pushed::No
        );
        // A literal of another type than the column.
        assert_eq!(translate(&col("input_tokens").eq(lit("x")), &t), Pushed::No);
        // A List column.
        assert_eq!(translate(&col("tags").eq(lit(1i64)), &t), Pushed::No);
        // A LIKE on a narrow column.
        assert_eq!(translate(&col("uid").like(lit("%abc%")), &t), Pushed::No);
        // An OR with a term that is not pushed.
        assert_eq!(
            translate(
                &col("status").eq(lit("ok")).or(col("tags").eq(lit(1i64))),
                &t
            ),
            Pushed::No
        );
        // A map key that is not promoted: the residual holds it.
        assert_eq!(
            translate(&get_field(col("attrs"), "k0").eq(lit("v")), &t),
            Pushed::No
        );
        assert!(!t.exact_map_key("attrs", "k0"));
    }

    #[tokio::test]
    async fn translate_or_and_not() {
        let t = main_table().await;
        let status = index(&t, "status");
        let tokens = index(&t, "input_tokens");
        let a = col("status").eq(utf8view("ok"));
        let b = col("input_tokens").gt(lit(5i64));
        assert_eq!(
            translate(&a.clone().or(b.clone()), &t),
            Pushed::Exact(Predicate::Or(vec![
                Predicate::Cmp(status, CmpOp::Eq, Lit::Utf8("ok".into())),
                Predicate::Cmp(tokens, CmpOp::Gt, Lit::Int(5)),
            ]))
        );
        assert_eq!(
            translate(&!a, &t),
            Pushed::Exact(Predicate::Not(Box::new(Predicate::Cmp(
                status,
                CmpOp::Eq,
                Lit::Utf8("ok".into())
            ))))
        );
    }

    #[tokio::test]
    async fn translate_payload_like_and_map_key() {
        let t = payload_table().await;
        let body = index(&t, "body");
        let m = index(&t, "m");
        assert_eq!(
            translate(&col("body").like(utf8view("%abc%")), &t),
            Pushed::Exact(Predicate::Contains(body, b"abc".to_vec()))
        );
        // NOT of a `Contains` term is not pushed: on a null row `LIKE` is NULL.
        assert_eq!(
            translate(&!col("body").like(utf8view("%abc%")), &t),
            Pushed::No
        );
        // A pattern with an inner wildcard, or with `_`, is not pushed.
        assert_eq!(
            translate(&col("body").like(utf8view("a%c")), &t),
            Pushed::No
        );
        assert_eq!(
            translate(&col("body").like(utf8view("%a_c%")), &t),
            Pushed::No
        );
        assert_eq!(
            translate(&get_field(col("m"), "k").eq(lit("v0")), &t),
            Pushed::Exact(Predicate::MapKeyEq(m, "k".into(), Lit::Utf8("v0".into())))
        );
        // An AND of exact terms is exact. A null test on the payload column is not pushed,
        // because it is not narrow.
        assert_eq!(translate(&col("body").is_not_null(), &t), Pushed::No);
        assert!(matches!(
            translate(
                &col("body")
                    .like(utf8view("%abc%"))
                    .and(col("start_time").is_not_null()),
                &t
            ),
            Pushed::Exact(Predicate::And(_))
        ));
        // Nor is a NOT around an AND with a `Contains` term.
        assert_eq!(
            translate(
                &!(col("body")
                    .like(utf8view("%abc%"))
                    .and(col("start_time").is_not_null())),
                &t
            ),
            Pushed::No
        );
    }

    #[test]
    fn split_rows_covers_the_file_in_large_enough_ranges() {
        assert_eq!(split_rows(1_000, 8), vec![None]);
        assert_eq!(split_rows(2 * MIN_PARTITION_ROWS - 1, 8), vec![None]);
        let rows = 10 * MIN_PARTITION_ROWS + 7;
        for parts in [2u64, 3, 8, 100] {
            let ranges: Vec<Range<u32>> = split_rows(rows, parts)
                .into_iter()
                .map(Option::unwrap)
                .collect();
            assert_eq!(ranges.len() as u64, parts.min(10));
            assert_eq!(ranges.first().unwrap().start, 0);
            assert_eq!(u64::from(ranges.last().unwrap().end), rows);
            for pair in ranges.windows(2) {
                assert_eq!(pair[0].end, pair[1].start);
            }
            assert!(
                ranges
                    .iter()
                    .all(|r| u64::from(r.end - r.start) >= MIN_PARTITION_ROWS)
            );
        }
    }

    /// A plan's statistics as DataFusion's join selection computes them.
    fn stats_of(plan: &dyn ExecutionPlan) -> Arc<Statistics> {
        ::datafusion::physical_plan::statistics::StatisticsContext::new()
            .compute(plan, &StatisticsArgs::new())
            .unwrap()
    }

    #[tokio::test]
    async fn scans_report_the_bytes_of_fixed_width_projections() {
        let table = main_table().await;
        let state = ::datafusion::prelude::SessionContext::new().state();
        let bytes = |names: &[&str]| {
            let projection: Vec<usize> = names.iter().map(|n| index(&table, n)).collect();
            let state = state.clone();
            let table = &table;
            async move {
                let plan = table
                    .scan(&state, Some(&projection), &[], None)
                    .await
                    .unwrap();
                stats_of(plan.as_ref()).total_byte_size
            }
        };
        // 8 bytes of Int64 and 8 of a nanosecond timestamp per row, as a Parquet scan says.
        assert_eq!(
            bytes(&["input_tokens", "start_time"]).await,
            Precision::Exact(3_000 * 16)
        );
        assert_eq!(
            bytes(&["input_tokens", "cost", "f32"]).await,
            Precision::Exact(3_000 * 20)
        );
        // A variable-width column leaves the size unknown.
        assert_eq!(bytes(&["input_tokens", "uid"]).await, Precision::Absent);

        // With a pushed filter the size follows the estimated rows.
        let projection = vec![index(&table, "input_tokens")];
        let filter = col("input_tokens").gt(lit(100i64));
        let plan = table
            .scan(&state, Some(&projection), &[filter], None)
            .await
            .unwrap();
        let stats = stats_of(plan.as_ref());
        let (Precision::Inexact(rows), Precision::Inexact(size)) =
            (stats.num_rows, stats.total_byte_size)
        else {
            panic!(
                "estimates: {:?} {:?}",
                stats.num_rows, stats.total_byte_size
            )
        };
        assert_eq!(size, rows * 8);
    }

    #[tokio::test]
    async fn scans_report_inexact_zone_ranges_of_float_columns() {
        // `cost` has nulls, NaNs at rows 997 and 1,994, and else (i % 500) / 1,000; `f32` has
        // nulls and else i / 2.
        let table = main_table().await;
        let state = ::datafusion::prelude::SessionContext::new().state();
        let projection = vec![index(&table, "cost"), index(&table, "f32")];
        let plan = table
            .scan(&state, Some(&projection), &[], None)
            .await
            .unwrap();
        let stats = stats_of(plan.as_ref());
        let cost = &stats.column_statistics[0];
        // The bounds leave the NaNs out, so they are not exact: `max(cost)` is NaN.
        assert_eq!(cost.min_value, Precision::Inexact(Sv::Float64(Some(0.0))));
        assert_eq!(cost.max_value, Precision::Inexact(Sv::Float64(Some(0.499))));
        // Nulls make the null count unknown, as for an integer column.
        assert_eq!(cost.null_count, Precision::Absent);
        let f32 = &stats.column_statistics[1];
        assert_eq!(f32.min_value, Precision::Inexact(Sv::Float32(Some(0.5))));
        assert_eq!(
            f32.max_value,
            Precision::Inexact(Sv::Float32(Some(1_499.5)))
        );

        // `max` is computed, not answered from the statistics: it is the NaN.
        let ctx = ::datafusion::prelude::SessionContext::new();
        ctx.register_table("t", Arc::new(main_table().await))
            .unwrap();
        let batches = ctx
            .sql("SELECT max(cost) FROM t")
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let max = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Float64Array>()
            .unwrap()
            .value(0);
        assert!(max.is_nan(), "{max}");

        // A range filter on a float column is estimated from its bounds, as on Parquet: the
        // filter's output range is cut by the literal, and its rows are fewer than the scan's.
        let df = ctx
            .sql("SELECT cost FROM t WHERE cost > 0.25")
            .await
            .unwrap();
        let plan = df.create_physical_plan().await.unwrap();
        let stats = stats_of(plan.as_ref());
        let Precision::Inexact(rows) = stats.num_rows else {
            panic!("a filter's rows are an estimate")
        };
        assert!(rows < 3_000, "{rows}");
        let Some(Sv::Float64(Some(min))) = stats.column_statistics[0].min_value.get_value() else {
            panic!("{:?}", stats.column_statistics[0].min_value)
        };
        assert!(*min > 0.25, "{min}");
        assert_eq!(
            stats.column_statistics[0].max_value,
            Precision::Inexact(Sv::Float64(Some(0.499)))
        );
    }

    #[tokio::test]
    async fn scans_report_the_zone_range_of_integer_columns() {
        let table = payload_table().await;
        let start = index(&table, "start_time");
        let body = index(&table, "body");
        let ctx = ::datafusion::prelude::SessionContext::new();
        let state = ctx.state();
        let ns = |v: i64| Sv::TimestampNanosecond(Some(v), Some("UTC".into()));

        let plan = table
            .scan(&state, Some(&vec![start, body]), &[], None)
            .await
            .unwrap();
        let stats = stats_of(plan.as_ref());
        assert_eq!(stats.num_rows, Precision::Exact(200));
        let time = &stats.column_statistics[0];
        assert_eq!(time.min_value, Precision::Exact(ns(0)));
        assert_eq!(time.max_value, Precision::Exact(ns(199_000)));
        assert_eq!(time.null_count, Precision::Exact(0));
        // A string column has no zone range.
        assert_eq!(stats.column_statistics[1], ColumnStatistics::new_unknown());

        // With a pushed filter the rows are an estimate, and so are the bounds.
        let filter = col("start_time").gt(lit(ns(100_000)));
        let plan = table
            .scan(&state, Some(&vec![start]), &[filter], None)
            .await
            .unwrap();
        let stats = stats_of(plan.as_ref());
        // DataFusion's default selectivity, 20 %, of the rows the scan reads.
        let Precision::Inexact(kept) = stats.num_rows else {
            panic!(
                "a filtered scan's rows are an estimate: {:?}",
                stats.num_rows
            )
        };
        assert!((1..=40).contains(&kept), "{kept}");
        assert_eq!(
            stats.column_statistics[0].min_value,
            Precision::Inexact(ns(0))
        );
        assert_eq!(
            stats.column_statistics[0].max_value,
            Precision::Inexact(ns(199_000))
        );

        // The session's selectivity is the one applied.
        let mut config = ::datafusion::prelude::SessionConfig::new();
        config.options_mut().optimizer.default_filter_selectivity = 100;
        let all = ::datafusion::prelude::SessionContext::new_with_config(config).state();
        let filter = col("start_time").gt(lit(ns(100_000)));
        let plan = table
            .scan(&all, Some(&vec![start]), &[filter], None)
            .await
            .unwrap();
        let Precision::Inexact(read) = stats_of(plan.as_ref()).num_rows else {
            panic!("a filtered scan's rows are an estimate")
        };
        assert_eq!(kept, read.div_ceil(5).max(1));
    }

    #[test]
    fn files_split_into_a_multiple_of_the_partitions_when_they_can() {
        assert_eq!(ranges_per_file(1, 8), 8);
        assert_eq!(ranges_per_file(5, 8), 8); // 40 ranges
        assert_eq!(ranges_per_file(3, 8), 8); // 24 ranges
        assert_eq!(ranges_per_file(4, 8), 2);
        assert_eq!(ranges_per_file(16, 8), 1);
        assert_eq!(ranges_per_file(10, 8), 4); // 40 ranges
        assert_eq!(ranges_per_file(7, 1), 1);
        assert_eq!(ranges_per_file(0, 0), 1);
        // No multiple within four times the base: the most ranges.
        assert_eq!(ranges_per_file(7, 64), 40);
    }

    #[test]
    fn partitions_read_their_rows_in_pieces() {
        let p = PIECE_ROWS;
        assert_eq!(pieces(None, 0), Vec::<Range<u32>>::new());
        assert_eq!(pieces(None, 10), vec![0..10]);
        assert_eq!(pieces(None, u64::from(p) + 1), vec![0..p, p..p + 1]);
        assert_eq!(
            pieces(Some(&[5..5 + 2 * p, 9 * p..9 * p + 3]), 0),
            vec![5..5 + p, 5 + p..5 + 2 * p, 9 * p..9 * p + 3]
        );
        assert_eq!(
            pieces(Some(std::slice::from_ref(&(4..4))), 100),
            Vec::<Range<u32>>::new()
        );
        assert_eq!(pieces(None, u64::MAX), Vec::<Range<u32>>::new());
    }

    #[test]
    fn int_scalars_keep_the_column_type_and_reject_out_of_range_values() {
        assert_eq!(int_scalar(&DataType::Int32, -5), Some(Sv::Int32(Some(-5))));
        assert_eq!(int_scalar(&DataType::Int8, 300), None);
        assert_eq!(int_scalar(&DataType::Utf8, 1), None);
        assert_eq!(
            int_scalar(&DataType::Timestamp(TimeUnit::Millisecond, None), 7),
            Some(Sv::TimestampMillisecond(Some(7), None))
        );
    }

    /// Rows of one `count(*)`-style query and the bytes the table's files fetched for it.
    async fn rows_and_bytes(table: StrataTable, sql: &str) -> (i64, u64) {
        let files = table.files.clone();
        let ctx = ::datafusion::prelude::SessionContext::new();
        ctx.register_table("t", Arc::new(table)).unwrap();
        files.iter().for_each(|f| f.reset_io_stats());
        let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
        let count = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap()
            .value(0);
        (count, files.iter().map(|f| f.io_stats().bytes).sum())
    }

    #[tokio::test]
    async fn count_star_comes_from_the_row_count_without_a_read() {
        let (rows, bytes) = rows_and_bytes(main_table().await, "SELECT count(*) FROM t").await;
        assert_eq!((rows, bytes), (3_000, 0));
        // With a filter, the count is computed from the rows, which needs reads.
        let all = wide_batch(3_000);
        let status = all
            .column(all.schema().index_of("status").unwrap())
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        let ok = status.iter().filter(|s| *s == Some("ok")).count() as i64;
        let (rows, bytes) = rows_and_bytes(
            main_table().await,
            "SELECT count(*) FROM t WHERE status = 'ok'",
        )
        .await;
        assert_eq!(rows, ok);
        assert!(bytes > 0);
    }

    #[tokio::test]
    async fn a_string_filter_skips_the_varbin_chunks_its_zones_rule_out() {
        // 40,000 sorted, distinct series ids: too many for a dictionary, so `Varbin` chunks.
        let n = 40_000usize;
        let ids = StringArray::from_iter_values((0..n).map(|i| format!("ELEC.SERIES-{i:06}.M")));
        let values = arrow::array::Float64Array::from_iter_values((0..n).map(|i| i as f64));
        let schema = Arc::new(Schema::new(vec![
            Field::new("series_id", DataType::Utf8, false),
            Field::new("value", DataType::Float64, false),
        ]));
        let batch = RecordBatch::try_new(schema, vec![Arc::new(ids), Arc::new(values)]).unwrap();
        let config = Config {
            dict_max_values: 1_000,
            ..Config::default()
        };
        let mut writer = StrataWriter::new(batch.schema(), &[], config.clone()).unwrap();
        writer.write(&batch).unwrap();
        let (bytes, _) = writer.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("series.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = Arc::new(StrataFile::open(store, path, None, &config).await.unwrap());
        assert_eq!(file.directory()[0].dict_offset, 0);
        let table = || StrataTable::new(vec![Arc::clone(&file)]).unwrap();

        let (all, all_bytes) =
            rows_and_bytes(table(), "SELECT count(*) FROM t WHERE value >= 0").await;
        assert_eq!(all, n as i64);
        for (id, expected) in [
            ("ELEC.SERIES-012345.M", 1),
            ("ELEC.SERIES-039999.M", 1),
            ("ELEC.SERIES-040000.M", 0),
            ("ELEC.A", 0),
        ] {
            let sql = format!("SELECT count(*) FROM t WHERE series_id = '{id}'");
            let (rows, bytes) = rows_and_bytes(table(), &sql).await;
            assert_eq!(rows, expected, "{id}");
            // One chunk of ~20 chunks of `series_id` at most, and no `value` chunk outside it.
            assert!(bytes * 5 < all_bytes, "{id}: {bytes} of {all_bytes} bytes");
        }
    }

    #[tokio::test]
    async fn exec_partitions_and_ordering() {
        let t = main_table().await;
        let schema = Arc::clone(&t.schema);
        let with_key = StrataExec::new(
            t.files.clone(),
            vec![0, 1],
            None,
            None,
            None,
            project_schema(&schema, Some(&vec![0, 1])).unwrap(),
            8,
        );
        assert_eq!(with_key.partitions(), 1);
        assert_eq!(
            with_key.properties().output_ordering().map(|o| o.len()),
            Some(1)
        );
        // The sort key (column 0) is not projected: no ordering.
        let without_key = StrataExec::new(
            t.files.clone(),
            vec![1],
            None,
            None,
            None,
            project_schema(&schema, Some(&vec![1])).unwrap(),
            8,
        );
        assert!(without_key.properties().output_ordering().is_none());
    }

    #[tokio::test]
    async fn a_scan_that_reads_a_payload_column_reads_each_file_whole() {
        // 140,000 rows: two partitions of at least `MIN_PARTITION_ROWS` when split. The config
        // makes the string columns payload columns.
        let config = Config {
            payload_min_avg: 1,
            ..Config::default()
        };
        let file = open(&wide_batch(140_000), config).await;
        let table = StrataTable::new(vec![Arc::clone(&file)]).unwrap();
        let schema = Arc::clone(&table.schema);
        let exec = |projection: Vec<usize>| {
            StrataExec::new(
                table.files.clone(),
                projection.clone(),
                None,
                None,
                None,
                project_schema(&schema, Some(&projection)).unwrap(),
                8,
            )
        };
        let column_in = |layer: Layer| {
            (0..schema.fields().len())
                .find(|&c| file.directory()[c].layer == layer)
                .unwrap()
        };
        let (narrow, payload) = (column_in(Layer::Narrow), column_in(Layer::Payload));
        assert_eq!(exec(vec![narrow]).partitions(), 2);
        assert_eq!(exec(vec![payload]).partitions(), 1);
        assert_eq!(exec(vec![narrow, payload]).partitions(), 1);
    }

    #[tokio::test]
    async fn batches_built_ahead_keep_their_order_and_errors_and_stop_when_dropped() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let batch = |n: i64| {
            RecordBatch::try_from_iter([(
                "n",
                Arc::new(arrow::array::Int64Array::from(vec![n])) as arrow::array::ArrayRef,
            )])
            .unwrap()
        };
        let value = |b: &RecordBatch| {
            b.column(0)
                .as_any()
                .downcast_ref::<arrow::array::Int64Array>()
                .unwrap()
                .value(0)
        };
        // Every batch, in order, with an error in its place.
        let items: Vec<Result<RecordBatch>> = (0..20)
            .map(|n| {
                if n == 7 {
                    Err(StrataError::Corrupt {
                        what: "test",
                        offset: 7,
                    })
                } else {
                    Ok(batch(n))
                }
            })
            .collect();
        let got: Vec<Result<RecordBatch>> =
            built_ahead(Box::new(items.into_iter()), 3).collect().await;
        assert_eq!(got.len(), 20);
        for (n, item) in got.iter().enumerate() {
            match item {
                Ok(b) => assert_eq!(value(b), n as i64),
                Err(StrataError::Corrupt { offset: 7, .. }) => assert_eq!(n, 7),
                Err(e) => panic!("{e}"),
            }
        }
        // A consumer that stops after two batches: the building stops too, at most the channel's
        // capacity and the batch being sent ahead of it.
        let built = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&built);
        let endless = (0..).map(move |n| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(batch(n))
        });
        let mut stream = Box::pin(built_ahead(Box::new(endless), 3));
        for n in 0..2 {
            assert_eq!(value(&stream.next().await.unwrap().unwrap()), n);
        }
        drop(stream);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let after_drop = built.load(Ordering::SeqCst);
        assert!(after_drop <= 2 + 3 + 2, "{after_drop} batches built");
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(built.load(Ordering::SeqCst), after_drop);
    }

    /// The rows of `sql`, the segment bytes it fetched, and its physical plan, with DataFusion's sort
    /// pushdown on or off. `files` back `table`.
    async fn run_sorted(
        table: StrataTable,
        sql: &str,
        pushdown: bool,
    ) -> (RecordBatch, u64, String) {
        use ::datafusion::physical_plan::displayable;
        use ::datafusion::prelude::{SessionConfig, SessionContext};
        let files = table.files.clone();
        let mut config = SessionConfig::new().with_target_partitions(4);
        config.options_mut().optimizer.enable_sort_pushdown = pushdown;
        let ctx = SessionContext::new_with_config(config);
        ctx.register_table("t", Arc::new(table)).unwrap();
        let frame = ctx.sql(sql).await.unwrap();
        let plan = frame.clone().create_physical_plan().await.unwrap();
        let shown = displayable(plan.as_ref()).indent(true).to_string();
        files.iter().for_each(|f| f.reset_io_stats());
        let schema = Arc::new(frame.schema().as_arrow().clone());
        let batches = frame.collect().await.unwrap();
        let rows = concat_batches(&schema, &batches).unwrap();
        // Segment bytes, without the gaps merged into requests: the test file is smaller than
        // the scan's largest merged gap.
        let bytes = files
            .iter()
            .map(|f| f.io_stats().by_kind.iter().sum::<u64>())
            .sum();
        (rows, bytes, shown)
    }

    /// A sort with a limit is pushed into the scan: no `SortExec` is left, the rows are those
    /// DataFusion's own sort returns (ties, nulls, both directions, two partitions, and no
    /// limit), and the scan reads less when the top rows sit in a few chunks.
    #[tokio::test]
    async fn a_pushed_sort_returns_the_rows_of_datafusions_sort() {
        // 140,000 rows: two partitions of at least `MIN_PARTITION_ROWS`.
        let file = open(&wide_batch(140_000), Config::default()).await;
        let table = || StrataTable::new(vec![Arc::clone(&file)]).unwrap();
        for sql in [
            "SELECT trace_id, start_time, status, uid FROM t WHERE span_kind = 'llm' \
             ORDER BY start_time DESC, trace_id DESC LIMIT 50",
            "SELECT input_tokens, uid FROM t ORDER BY input_tokens DESC NULLS FIRST, uid LIMIT 30",
            "SELECT input_tokens, uid FROM t WHERE status = 'ok' \
             ORDER BY input_tokens, uid LIMIT 7000",
            "SELECT start_time, uid FROM t WHERE input_tokens < 100 ORDER BY start_time DESC",
        ] {
            let (pushed, pushed_bytes, plan) = run_sorted(table(), sql, true).await;
            let (sorted, sorted_bytes, plain) = run_sorted(table(), sql, false).await;
            assert!(plan.contains("order=["), "{sql}\n{plan}");
            assert!(!plan.contains("SortExec"), "{sql}\n{plan}");
            assert!(plain.contains("SortExec"), "{sql}\n{plain}");
            assert!(sorted.num_rows() > 0, "{sql}");
            assert_eq!(pushed, sorted, "{sql}");
            if sql.contains("ORDER BY start_time DESC, trace_id DESC LIMIT") {
                // The filter and sort columns are read whole either way; the others only in
                // the chunks of the top rows.
                assert!(
                    pushed_bytes < sorted_bytes,
                    "{pushed_bytes} of {sorted_bytes} bytes\n{plan}\n{plain}"
                );
            }
        }
    }

    /// File `f` of a table of time slices: 600 rows whose start times fall in slice `f` (ten
    /// rows share each time, so the sort has ties), a payload `body` in which every third row
    /// holds "abc", and row 17 of files 2 and 5 holds "zzrare".
    async fn slice_file(f: usize) -> Arc<StrataFile> {
        let n = 600usize;
        let trace: Vec<[u8; 16]> = (0..n)
            .map(|i| {
                let mut id = [0u8; 16];
                id[..8].copy_from_slice(&(f as u64).to_be_bytes());
                id[8..].copy_from_slice(&(i as u64).to_be_bytes());
                id
            })
            .collect();
        let trace_ids = FixedSizeBinaryArray::try_from_iter(trace.iter()).unwrap();
        // Time does not follow the sort key inside a file, as with real traces.
        // File 3 has a null start time in every 50th row: nulls sort first in a descending
        // sort, so they are always in its top rows.
        let times = TimestampNanosecondArray::from_iter((0..n).map(|i| {
            (f != 3 || i % 50 != 0).then_some((f * 1_000_000 + (i * 7919 % n) / 10) as i64)
        }))
        .with_timezone("UTC");
        let body = StringArray::from_iter_values((0..n).map(|i| {
            let rare = if i == 17 && (f == 2 || f == 5) {
                " zzrare"
            } else {
                ""
            };
            let common = if i % 3 == 0 { " abc" } else { "" };
            format!("file {f} row {i}{common}{rare} and some words to make it longer")
        }));
        let schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("start_time", times.data_type().clone(), true),
            Field::new("body", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(trace_ids), Arc::new(times), Arc::new(body)],
        )
        .unwrap();
        let config = Config {
            payload_min_avg: 1,
            ..Config::default()
        };
        // Sorted by trace id alone: a sort-key column holds no null.
        let mut writer = StrataWriter::new(batch.schema(), &["trace_id"], config).unwrap();
        writer.write(&batch).unwrap();
        let (bytes, _) = writer.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("slice.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        Arc::new(
            StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap(),
        )
    }

    /// A sort with a limit over a substring search reads the files in stages, best first: the
    /// rows are those of DataFusion's own sort (both directions, ties, fewer matches than the
    /// limit), and a common needle reads fewer bytes than the full sort.
    #[tokio::test]
    async fn a_staged_top_returns_the_rows_of_datafusions_sort() {
        let mut files = Vec::new();
        for f in 0..8 {
            files.push(slice_file(f).await);
        }
        let table = || StrataTable::new(files.clone()).unwrap();
        // Payload frame bytes of the last run: the files are small, so metadata would hide
        // what the stages save.
        let frame_bytes = || -> u64 {
            files
                .iter()
                .map(|f| f.io_stats().by_kind[crate::segment::Kind::PayloadFrame as usize])
                .sum()
        };
        for sql in [
            "SELECT start_time, trace_id, body FROM t WHERE body LIKE '%abc%' \
             ORDER BY start_time DESC, trace_id LIMIT 25",
            "SELECT start_time, trace_id FROM t WHERE body LIKE '%abc%' \
             ORDER BY start_time, trace_id DESC LIMIT 25",
            "SELECT start_time, trace_id, body FROM t WHERE body LIKE '%zzrare%' \
             ORDER BY start_time DESC, trace_id LIMIT 25",
            "SELECT start_time, trace_id FROM t WHERE body LIKE '%nowhere%' \
             ORDER BY start_time DESC LIMIT 5",
            "SELECT start_time, trace_id FROM t WHERE body LIKE '%abc%' \
             ORDER BY start_time DESC NULLS LAST, trace_id LIMIT 25",
        ] {
            let (pushed, _, plan) = run_sorted(table(), sql, true).await;
            let pushed_bytes = frame_bytes();
            let (sorted, _, plain) = run_sorted(table(), sql, false).await;
            let sorted_bytes = frame_bytes();
            assert!(plan.contains("staged"), "{sql}\n{plan}");
            assert!(!plan.contains("SortExec"), "{sql}\n{plan}");
            assert!(plain.contains("SortExec"), "{sql}\n{plain}");
            assert_eq!(pushed, sorted, "{sql}");
            if sql.contains("abc") {
                assert_eq!(sorted.num_rows(), 25, "{sql}");
                assert!(
                    pushed_bytes * 2 < sorted_bytes,
                    "{pushed_bytes} of {sorted_bytes} bytes\n{plan}"
                );
            }
            if sql.contains("zzrare") {
                assert_eq!(sorted.num_rows(), 2, "{sql}");
            }
        }
    }

    /// A sort by a payload column or by an expression stays in DataFusion.
    #[tokio::test]
    async fn a_sort_strata_cannot_bound_is_not_pushed() {
        for (table, sql) in [
            (
                payload_table().await,
                "SELECT trace_id, body FROM t ORDER BY body LIMIT 5",
            ),
            (
                main_table().await,
                "SELECT uid FROM t ORDER BY length(uid), uid LIMIT 5",
            ),
        ] {
            let (_, _, plan) = run_sorted(table, sql, true).await;
            assert!(plan.contains("SortExec"), "{sql}\n{plan}");
            assert!(!plan.contains("order=["), "{sql}\n{plan}");
        }
    }

    #[test]
    fn table_schema_views_top_level_strings_only() {
        let schema = table_schema(&wide_schema());
        let field = |name: &str| schema.field_with_name(name).unwrap().data_type().clone();
        assert_eq!(field("status"), DataType::Utf8View);
        assert_eq!(field("blob"), DataType::BinaryView);
        assert_eq!(field("trace_id"), DataType::FixedSizeBinary(16));
        assert!(matches!(field("attrs"), DataType::Map(_, _)));
    }

    #[tokio::test]
    async fn new_rejects_empty_and_mixed_tables() {
        assert!(matches!(
            StrataTable::new(Vec::new()),
            Err(StrataError::Schema { .. })
        ));
        let wide = open(&wide_batch(10), Config::default()).await;
        let payload = open(&payload_batch(), Config::default()).await;
        assert!(matches!(
            StrataTable::new(vec![wide, payload]),
            Err(StrataError::Schema { .. })
        ));
    }
}
