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

use arrow::array::{RecordBatch, RecordBatchOptions};
use arrow::compute::{SortOptions, cast};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use async_trait::async_trait;
use futures::TryStreamExt;
use futures::future::try_join_all;
use object_store::ObjectStore;
use object_store::path::Path;

use ::datafusion::catalog::Session;
use ::datafusion::common::tree_node::TreeNodeRecursion;
use ::datafusion::common::utils::project_schema;
use ::datafusion::common::{DataFusionError, ScalarValue};
use ::datafusion::datasource::TableProvider;
use ::datafusion::error::Result as DfResult;
use ::datafusion::execution::TaskContext;
use ::datafusion::logical_expr::expr::{Between, BinaryExpr, InList, Like, ScalarFunction};
use ::datafusion::logical_expr::{Expr, Operator, TableProviderFilterPushDown, TableType};
use ::datafusion::physical_expr::expressions::Column as PhysicalColumn;
use ::datafusion::physical_expr::{EquivalenceProperties, PhysicalSortExpr};
use ::datafusion::physical_plan::execution_plan::{Boundedness, EmissionType, SchedulingType};
use ::datafusion::physical_plan::metrics::{ExecutionPlanMetricsSet, MetricBuilder, MetricsSet};
use ::datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use ::datafusion::physical_plan::{
    ChildrenPropertiesMode, DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning,
    PlanProperties, ReplaceChildrenOptions, SendableRecordBatchStream,
};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::predicate::{self, CmpOp, Lit, Predicate, layout_of};
use crate::scan::ScanRequest;

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

/// Fewest rows in one partition's range when a file is split.
const MIN_PARTITION_ROWS: u64 = 65_536;

/// Splits each file into about `target / files` contiguous row ranges of at least
/// `MIN_PARTITION_ROWS` rows. A file that is not split reads all its rows (`None`).
fn split_files(files: &[Arc<StrataFile>], target: usize) -> Vec<(usize, Option<Range<u32>>)> {
    let per_file = target.div_ceil(files.len().max(1)).max(1) as u64;
    let mut parts = Vec::new();
    for (index, file) in files.iter().enumerate() {
        for rows in split_rows(file.row_count(), per_file) {
            parts.push((index, rows));
        }
    }
    parts
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
        (DataType::Timestamp(TimeUnit::Nanosecond, _), ScalarValue::TimestampNanosecond(v, _)) => {
            opt(*v, Lit::Int)
        }
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
        let mut key = None;
        for filter in filters {
            let (p, exact) = match translate(filter, self) {
                Pushed::Exact(p) => (p, true),
                Pushed::Inexact(p) => (p, false),
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

        // Lookup mode: the key filter is the only kept filter and there is no limit.
        let lookup = match key {
            Some((_, bytes)) if kept.len() == 1 && limit.is_none() => Some(bytes),
            _ => None,
        };
        let predicate = match (lookup.is_some(), kept.len()) {
            (true, _) | (_, 0) => None,
            (false, 1) => kept.into_iter().next(),
            _ => Some(Predicate::And(kept)),
        };

        let indexes: Vec<usize> = projection
            .cloned()
            .unwrap_or_else(|| (0..self.schema.fields().len()).collect());
        let schema = project_schema(&self.schema, projection)?;
        let target = state.config().target_partitions();
        Ok(Arc::new(StrataExec::new(
            files, indexes, predicate, lookup, limit, schema, target,
        )))
    }
}

/// Reads Strata files for one table scan.
#[derive(Debug)]
pub struct StrataExec {
    files: Vec<Arc<StrataFile>>,
    /// One entry per partition: the file index and the rows it reads (`None`: all of them).
    parts: Vec<(usize, Option<Range<u32>>)>,
    projection: Vec<usize>,
    predicate: Option<Predicate>,
    lookup: Option<Vec<u8>>,
    limit: Option<usize>,
    schema: SchemaRef,
    props: Arc<PlanProperties>,
    metrics: ExecutionPlanMetricsSet,
}

impl StrataExec {
    /// An exec over `files`. `schema` is the output schema, the projection of the table schema.
    /// A scan splits each file into row ranges so that there are about `target_partitions`
    /// partitions; a lookup reads each file whole.
    fn new(
        files: Vec<Arc<StrataFile>>,
        projection: Vec<usize>,
        predicate: Option<Predicate>,
        lookup: Option<Vec<u8>>,
        limit: Option<usize>,
        schema: SchemaRef,
        target_partitions: usize,
    ) -> StrataExec {
        let parts = split_files(
            &files,
            if lookup.is_some() {
                1
            } else {
                target_partitions
            },
        );
        // Ordering: the sort key, when its column is projected. Each partition reads one
        // contiguous row range of one file, which is sorted by the key.
        let sort_key = files.first().and_then(|file| file.sort_key_column());
        let mut eq = EquivalenceProperties::new(Arc::clone(&schema));
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
        let partitions = parts.len().max(1);
        // Prototype shortcut: the batch stream does not consume DataFusion's cooperative
        // budget. `Cooperative` avoids the `CooperativeExec` wrapper.
        let props = PlanProperties::new(
            eq,
            Partitioning::UnknownPartitioning(partitions),
            EmissionType::Incremental,
            Boundedness::Bounded,
        )
        .with_scheduling_type(SchedulingType::Cooperative);
        StrataExec {
            files,
            parts,
            projection,
            predicate,
            lookup,
            limit,
            schema,
            props: Arc::new(props),
            metrics: ExecutionPlanMetricsSet::new(),
        }
    }

    /// The number of partitions: one per row range, and one when there is no file.
    fn partitions(&self) -> usize {
        self.parts.len().max(1)
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
        )
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
        let lookup = self.lookup.clone();
        let limit = self.limit;
        let schema = Arc::clone(&self.schema);
        // Partitions run at once: each builds its batches on its share of the cores.
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let threads = cores.div_ceil(self.partitions()).max(1);
        type Batches = Box<dyn Iterator<Item = Result<RecordBatch>> + Send>;
        let fut = async move {
            let Some(file) = file else {
                return Ok::<Batches, DataFusionError>(Box::new(std::iter::empty()));
            };
            let before = file.io_stats();
            let batches: Batches = match lookup {
                Some(key) => Box::new(
                    file.lookup_scan(&key, &projection, true)
                        .await
                        .map_err(df)?
                        .into_iter()
                        .map(Ok),
                ),
                None => Box::new(
                    file.scan_batches(&ScanRequest {
                        projection,
                        predicate,
                        row_ranges,
                        limit,
                        kind: ScanKind::Scan,
                        views: true,
                    })
                    .await
                    .map_err(df)?
                    .with_threads(threads),
                ),
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
            Ok(batches)
        };
        let stream = futures::stream::once(fut)
            .map_ok(move |batches| {
                // `map_ok` takes an `FnMut`; it runs once.
                let (elapsed, output_rows, schema) =
                    (elapsed.clone(), output_rows.clone(), Arc::clone(&schema));
                futures::stream::iter(batches.map(move |batch| {
                    let _timer = elapsed.timer();
                    let batch = batch
                        .and_then(|b| to_table_batch(&b, &schema))
                        .map_err(df)?;
                    output_rows.add(batch.num_rows());
                    Ok(batch)
                }))
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
