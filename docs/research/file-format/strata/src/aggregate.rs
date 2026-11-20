//! Aggregates grouped on dictionary codes (`real-data.md` § Narrow aggregates).
//!
//! DataFusion groups a `GROUP BY model` on strings: it hashes each row's `Utf8View` value, and
//! Strata first turns each row's dictionary code into that value. [`StrataAggregateRule`]
//! replaces an aggregate over a plain [`StrataExec`] by a [`StrataAggregateExec`], which reads
//! the group column as dictionary arrays and accumulates per code, so a string is looked at
//! once per distinct code and batch, not once per row.
//!
//! The rule applies when:
//! - the aggregate is `Single`, or a `Final` over a `Partial` (through repartitions and
//!   partition merges), over a `StrataExec` with no lookup, limit or pushed sort, so every filter is
//!   applied exactly by the scan;
//! - there is one group expression, a column, that is a narrow Utf8 column of every file;
//! - every aggregate is `count`, `sum`, `avg`, `min` or `max` of one narrow column of a signed
//!   integer type (`count`, `sum` and `avg` also take Float32 and Float64; `count` also takes
//!   a non-null literal, as `count(*)`), with no `DISTINCT`, `FILTER` or `ORDER BY`.
//!
//! The results are DataFusion's: `count` counts the non-null values (every row for a
//! literal), `sum` and `avg` of no value are NULL, an integer `sum` wraps on overflow, `avg`
//! sums as `f64`, and NULL is a group of its own. A float `sum` adds in another order than
//! DataFusion's, which already depends on the partitioning.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, AsArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Float64Type, Int64Type, SchemaRef, UInt32Type};
use futures::TryStreamExt;

use ::datafusion::common::config::ConfigOptions;
use ::datafusion::common::runtime::SpawnedTask;
use ::datafusion::common::tree_node::{
    Transformed, TransformedResult, TreeNode, TreeNodeRecursion,
};
use ::datafusion::common::{DataFusionError, ScalarValue};
use ::datafusion::error::Result as DfResult;
use ::datafusion::execution::TaskContext;
use ::datafusion::physical_expr::expressions::{CastExpr, Column as PhysicalColumn, Literal};
use ::datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr};
use ::datafusion::physical_optimizer::PhysicalOptimizerRule;
use ::datafusion::physical_plan::aggregates::{AggregateExec, AggregateMode};
use ::datafusion::physical_plan::coalesce_partitions::CoalescePartitionsExec;
use ::datafusion::physical_plan::execution_plan::{Boundedness, EmissionType, SchedulingType};
use ::datafusion::physical_plan::projection::ProjectionExec;
use ::datafusion::physical_plan::repartition::RepartitionExec;
use ::datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use ::datafusion::physical_plan::{
    ChildrenPropertiesMode, DisplayAs, DisplayFormatType, ExecutionPlan, Partitioning,
    PlanProperties, ReplaceChildrenOptions, SendableRecordBatchStream,
};

use crate::datafusion::{StrataExec, split_files};
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::predicate::Predicate;
use crate::scan::ScanRequest;

/// One aggregate, by kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// One aggregate: its kind, and the table column it reads (`None`: a `count` of a literal).
#[derive(Debug, Clone, Copy)]
struct Aggregate {
    kind: Kind,
    column: Option<usize>,
    /// Whether the column is a float column (`sum`, `avg` and `count` only).
    float: bool,
}

/// The running state of one aggregate in one group.
#[derive(Debug, Clone, Copy)]
enum Acc {
    Count(i64),
    /// An integer sum, wrapping, and whether a value was added.
    SumI(i64, bool),
    SumF(f64, bool),
    /// The sum as `f64` and the count of values.
    Avg(f64, i64),
    Min(Option<i64>),
    Max(Option<i64>),
}

impl Acc {
    fn new(agg: &Aggregate) -> Self {
        match (agg.kind, agg.float) {
            (Kind::Count, _) => Acc::Count(0),
            (Kind::Sum, false) => Acc::SumI(0, false),
            (Kind::Sum, true) => Acc::SumF(0.0, false),
            (Kind::Avg, _) => Acc::Avg(0.0, 0),
            (Kind::Min, _) => Acc::Min(None),
            (Kind::Max, _) => Acc::Max(None),
        }
    }

    /// Adds the state `other` of the same aggregate.
    fn merge(&mut self, other: &Acc) {
        match (self, other) {
            (Acc::Count(a), Acc::Count(b)) => *a += b,
            (Acc::SumI(a, x), Acc::SumI(b, y)) => {
                *a = a.wrapping_add(*b);
                *x |= y;
            }
            (Acc::SumF(a, x), Acc::SumF(b, y)) => {
                *a += b;
                *x |= y;
            }
            (Acc::Avg(a, n), Acc::Avg(b, m)) => {
                *a += b;
                *n += m;
            }
            (Acc::Min(a), Acc::Min(b)) => *a = min_opt(*a, *b),
            (Acc::Max(a), Acc::Max(b)) => *a = max_opt(*a, *b),
            _ => {}
        }
    }
}

fn min_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// The states of every group: the group value (`None`: NULL), and one state per aggregate.
type Groups = HashMap<Option<String>, Vec<Acc>>;

/// An aggregate column of a batch, as the type its state adds.
enum Values<'a> {
    /// A `count` of a literal: every row counts.
    Rows,
    Int(&'a Int64Array),
    Float(&'a Float64Array),
}

/// Adds row `row` of `values` to `acc`.
fn add(acc: &mut Acc, values: &Values<'_>, row: usize) {
    match values {
        Values::Rows => {
            if let Acc::Count(n) = acc {
                *n += 1;
            }
        }
        Values::Int(a) => {
            if a.is_null(row) {
                return;
            }
            let v = a.value(row);
            match acc {
                Acc::Count(n) => *n += 1,
                Acc::SumI(s, any) => {
                    *s = s.wrapping_add(v);
                    *any = true;
                }
                Acc::SumF(s, any) => {
                    *s += v as f64;
                    *any = true;
                }
                Acc::Avg(s, n) => {
                    *s += v as f64;
                    *n += 1;
                }
                Acc::Min(m) => *m = min_opt(*m, Some(v)),
                Acc::Max(m) => *m = max_opt(*m, Some(v)),
            }
        }
        Values::Float(a) => {
            if a.is_null(row) {
                return;
            }
            let v = a.value(row);
            match acc {
                Acc::Count(n) => *n += 1,
                Acc::SumF(s, any) => {
                    *s += v;
                    *any = true;
                }
                Acc::Avg(s, n) => {
                    *s += v;
                    *n += 1;
                }
                // The rule never pairs a float column with these.
                Acc::SumI(..) | Acc::Min(_) | Acc::Max(_) => {}
            }
        }
    }
}

/// Adds the rows of `batch` to `groups`. Column 0 is the group column; column `1 + i` is the
/// column of aggregate `i` that reads one.
fn add_batch(
    batch: &RecordBatch,
    aggs: &[Aggregate],
    inputs: &[Option<usize>],
    groups: &mut Groups,
) -> Result<()> {
    let cast_err = |what: &str| StrataError::Schema {
        reason: format!("internal: aggregate column is not {what}"),
    };
    // Each aggregate column as Int64 or Float64.
    let mut owned: Vec<Option<ArrayRef>> = Vec::with_capacity(aggs.len());
    for (agg, input) in aggs.iter().zip(inputs) {
        owned.push(match input {
            None => None,
            Some(i) => {
                let column = batch.columns().get(*i).ok_or_else(|| cast_err("present"))?;
                let to = if agg.float {
                    DataType::Float64
                } else {
                    DataType::Int64
                };
                Some(cast(column, &to)?)
            }
        });
    }
    let mut values: Vec<Values<'_>> = Vec::with_capacity(aggs.len());
    for (agg, array) in aggs.iter().zip(&owned) {
        values.push(match array {
            None => Values::Rows,
            Some(a) if agg.float => Values::Float(
                a.as_primitive_opt::<Float64Type>()
                    .ok_or_else(|| cast_err("Float64"))?,
            ),
            Some(a) => Values::Int(
                a.as_primitive_opt::<Int64Type>()
                    .ok_or_else(|| cast_err("Int64"))?,
            ),
        });
    }
    let fresh = || aggs.iter().map(Acc::new).collect::<Vec<_>>();
    let group = batch.columns().first().ok_or_else(|| cast_err("present"))?;
    match group.as_dictionary_opt::<UInt32Type>() {
        Some(dict) => {
            // The states per code of this batch, then merged by value: one string per code.
            let strings = dict
                .values()
                .as_string_opt::<i32>()
                .ok_or_else(|| cast_err("a string dictionary"))?;
            let keys = dict.keys();
            let n_codes = strings.len();
            let mut per_code: Vec<Option<Vec<Acc>>> = vec![None; n_codes + 1];
            for row in 0..keys.len() {
                let slot = if keys.is_null(row) {
                    n_codes
                } else {
                    keys.value(row) as usize
                };
                let Some(states) = per_code.get_mut(slot) else {
                    return Err(StrataError::Schema {
                        reason: "internal: dictionary key out of range".to_string(),
                    });
                };
                let states = states.get_or_insert_with(fresh);
                for (acc, v) in states.iter_mut().zip(&values) {
                    add(acc, v, row);
                }
            }
            for (code, states) in per_code.into_iter().enumerate() {
                let Some(states) = states else { continue };
                let key = (code < n_codes && strings.is_valid(code))
                    .then(|| strings.value(code).to_owned());
                let entry = groups.entry(key).or_insert_with(fresh);
                for (a, b) in entry.iter_mut().zip(&states) {
                    a.merge(b);
                }
            }
        }
        None => {
            // A file without a dictionary for the column: one hash per row.
            let strings = cast(group, &DataType::Utf8)?;
            let strings: &StringArray = strings
                .as_string_opt::<i32>()
                .ok_or_else(|| cast_err("Utf8"))?;
            for row in 0..strings.len() {
                let key = strings.is_valid(row).then(|| strings.value(row).to_owned());
                let states = groups.entry(key).or_insert_with(fresh);
                for (acc, v) in states.iter_mut().zip(&values) {
                    add(acc, v, row);
                }
            }
        }
    }
    Ok(())
}

/// Reads rows `rows` of `file` (all of them for `None`) and returns their groups.
async fn file_groups(
    file: Arc<StrataFile>,
    rows: Option<std::ops::Range<u32>>,
    group: usize,
    aggs: Vec<Aggregate>,
    predicate: Option<Predicate>,
    threads: usize,
) -> Result<Groups> {
    // The projection: the group column, then each distinct aggregate column once.
    let mut projection = vec![group];
    let mut inputs = Vec::with_capacity(aggs.len());
    for agg in &aggs {
        inputs.push(
            agg.column
                .map(|c| match projection.iter().position(|&p| p == c) {
                    Some(i) => i,
                    None => {
                        projection.push(c);
                        projection.len() - 1
                    }
                }),
        );
    }
    let batches = file
        .scan_batches(
            &ScanRequest {
                projection,
                predicate,
                row_ranges: rows.map(|r| vec![r]),
                limit: None,
                kind: ScanKind::Scan,
                views: false,
                top: None,
            },
            threads,
        )
        .await?
        .keep_dictionaries();
    let mut groups = Groups::new();
    for batch in batches {
        add_batch(&batch?, &aggs, &inputs, &mut groups)?;
    }
    Ok(groups)
}

/// The output batch of `groups`, of `schema`: the group column, then one column per aggregate.
fn output(groups: Groups, aggs: &[Aggregate], schema: &SchemaRef) -> Result<RecordBatch> {
    let mut keys: Vec<Option<String>> = Vec::with_capacity(groups.len());
    let mut states: Vec<Vec<Acc>> = Vec::with_capacity(groups.len());
    for (k, s) in groups {
        keys.push(k);
        states.push(s);
    }
    let mut columns: Vec<ArrayRef> = Vec::with_capacity(aggs.len() + 1);
    columns.push(Arc::new(StringArray::from(keys)));
    for (i, agg) in aggs.iter().enumerate() {
        let column = states.iter().map(|s| s.get(i).copied());
        let array: ArrayRef = match (agg.kind, agg.float) {
            (Kind::Sum, true) | (Kind::Avg, _) => {
                Arc::new(Float64Array::from_iter(column.map(|a| match a {
                    Some(Acc::SumF(s, true)) => Some(s),
                    Some(Acc::Avg(s, n)) if n > 0 => Some(s / n as f64),
                    _ => None,
                })))
            }
            _ => Arc::new(Int64Array::from_iter(column.map(|a| match a {
                Some(Acc::Count(n)) => Some(n),
                Some(Acc::SumI(s, true)) => Some(s),
                Some(Acc::Min(m) | Acc::Max(m)) => m,
                _ => None,
            }))),
        };
        columns.push(array);
    }
    let columns = columns
        .into_iter()
        .zip(schema.fields())
        .map(|(a, f)| {
            if a.data_type() == f.data_type() {
                Ok(a)
            } else {
                Ok(cast(&a, f.data_type())?)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(Arc::clone(schema), columns)?)
}

/// An aggregate grouped on one narrow Utf8 column, over Strata files: one partition, one batch.
#[derive(Debug)]
pub struct StrataAggregateExec {
    files: Vec<Arc<StrataFile>>,
    predicate: Option<Predicate>,
    /// The table column grouped on.
    group: usize,
    aggs: Vec<Aggregate>,
    schema: SchemaRef,
    props: Arc<PlanProperties>,
}

impl StrataAggregateExec {
    fn new(
        files: Vec<Arc<StrataFile>>,
        predicate: Option<Predicate>,
        group: usize,
        aggs: Vec<Aggregate>,
        schema: SchemaRef,
    ) -> Self {
        let props = PlanProperties::new(
            EquivalenceProperties::new(Arc::clone(&schema)),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Final,
            Boundedness::Bounded,
        )
        .with_scheduling_type(SchedulingType::Cooperative);
        Self {
            files,
            predicate,
            group,
            aggs,
            schema,
            props: Arc::new(props),
        }
    }
}

impl DisplayAs for StrataAggregateExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let aggs: Vec<String> = self
            .aggs
            .iter()
            .map(|a| match a.column {
                Some(c) => format!("{:?}({c})", a.kind),
                None => format!("{:?}(*)", a.kind),
            })
            .collect();
        write!(
            f,
            "StrataAggregateExec: files={}, group={}, aggs=[{}], predicate={}",
            self.files.len(),
            self.group,
            aggs.join(", "),
            self.predicate
                .as_ref()
                .map_or("none".to_string(), |p| format!("{p:?}"))
        )
    }
}

impl ExecutionPlan for StrataAggregateExec {
    fn name(&self) -> &str {
        "StrataAggregateExec"
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.props
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> DfResult<TreeNodeRecursion>,
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
        if partition != 0 {
            return Err(DataFusionError::Internal(format!(
                "StrataAggregateExec partition {partition} out of range"
            )));
        }
        let files = self.files.clone();
        let (group, aggs, predicate) = (self.group, self.aggs.clone(), self.predicate.clone());
        let schema = Arc::clone(&self.schema);
        let df = |e: StrataError| DataFusionError::External(Box::new(e));
        let fut = async move {
            // The files split into row ranges as a scan's partitions are, one task each on its
            // share of the cores: a large file is accumulated on several cores, not one.
            let cores = crate::parallel::cores();
            let parts = split_files(&files, cores);
            let threads = cores.div_ceil(parts.len().max(1));
            let tasks: Vec<_> = parts
                .into_iter()
                .filter_map(|(f, rows)| files.get(f).map(|file| (Arc::clone(file), rows)))
                .map(|(file, rows)| {
                    SpawnedTask::spawn(file_groups(
                        file,
                        rows,
                        group,
                        aggs.clone(),
                        predicate.clone(),
                        threads,
                    ))
                })
                .collect();
            let mut groups = Groups::new();
            for task in tasks {
                let part = task
                    .join_unwind()
                    .await
                    .map_err(|e| DataFusionError::ExecutionJoin(Box::new(e)))?
                    .map_err(df)?;
                for (key, states) in part {
                    match groups.get_mut(&key) {
                        Some(entry) => {
                            for (a, b) in entry.iter_mut().zip(&states) {
                                a.merge(b);
                            }
                        }
                        None => {
                            groups.insert(key, states);
                        }
                    }
                }
            }
            output(groups, &aggs, &schema).map_err(df)
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            Arc::clone(&self.schema),
            futures::stream::once(fut).map_ok(|b| b),
        )))
    }
}

/// Replaces an aggregate over a plain [`StrataExec`] by a [`StrataAggregateExec`] when it can
/// (module doc). Register it with `SessionStateBuilder::with_physical_optimizer_rule`.
#[derive(Debug, Default)]
pub struct StrataAggregateRule;

impl PhysicalOptimizerRule for StrataAggregateRule {
    fn optimize(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        _config: &ConfigOptions,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        plan.transform_down(|plan| {
            Ok(match rewrite(&plan) {
                Some(new) => Transformed::yes(new),
                None => Transformed::no(plan),
            })
        })
        .data()
    }

    fn name(&self) -> &str {
        "StrataAggregateRule"
    }

    fn schema_check(&self) -> bool {
        true
    }
}

/// `plan` without the repartitions and partition merges above its input.
fn skip_moves(plan: &Arc<dyn ExecutionPlan>) -> &Arc<dyn ExecutionPlan> {
    let mut plan = plan;
    loop {
        let moves = plan.downcast_ref::<RepartitionExec>().is_some()
            || plan.downcast_ref::<CoalescePartitionsExec>().is_some();
        match plan.children().first() {
            Some(child) if moves => plan = child,
            _ => return plan,
        }
    }
}

/// The [`StrataAggregateExec`] for `plan`, when the rule applies to it.
fn rewrite(plan: &Arc<dyn ExecutionPlan>) -> Option<Arc<dyn ExecutionPlan>> {
    let top = plan.downcast_ref::<AggregateExec>()?;
    let inner = match top.mode() {
        AggregateMode::Single | AggregateMode::SinglePartitioned => top,
        AggregateMode::Final | AggregateMode::FinalPartitioned => {
            let partial = skip_moves(top.input()).downcast_ref::<AggregateExec>()?;
            (*partial.mode() == AggregateMode::Partial).then_some(partial)?
        }
        _ => return None,
    };
    // A projection of columns and numeric casts may sit between them: DataFusion casts an
    // integer column to Float64 for `avg`, once for every aggregate that uses it.
    let below = skip_moves(inner.input());
    let (scan, between) = match below.downcast_ref::<ProjectionExec>() {
        Some(p) => (
            skip_moves(p.input()).downcast_ref::<StrataExec>()?,
            Some(p.expr()),
        ),
        None => (below.downcast_ref::<StrataExec>()?, None),
    };
    let (files, projection, predicate) = scan.plain_scan()?;
    let narrow = |column: usize| {
        files.iter().all(|f| {
            f.directory()
                .get(column)
                .is_some_and(|r| r.layer == Layer::Narrow)
        })
    };
    let file_type = |column: usize| {
        files
            .first()
            .map(|f| f.schema().field(column).data_type().clone())
    };
    // A column, or a numeric cast of one, and whether the cast is to a float type.
    fn uncast(e: &Arc<dyn PhysicalExpr>) -> Option<(&PhysicalColumn, bool)> {
        match e.downcast_ref::<CastExpr>() {
            Some(cast) => {
                let to_float = match cast.cast_type() {
                    DataType::Float32 | DataType::Float64 => true,
                    DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => false,
                    _ => return None,
                };
                Some((cast.expr().downcast_ref::<PhysicalColumn>()?, to_float))
            }
            None => Some((e.downcast_ref::<PhysicalColumn>()?, false)),
        }
    }
    // The table column an aggregate input reads, and whether it reaches the aggregate cast to
    // a float type, here or in the projection below.
    let input_column = |expr: &Arc<dyn PhysicalExpr>| -> Option<(usize, bool)> {
        let (c, outer) = uncast(expr)?;
        let (c, inner) = match between {
            None => (c, false),
            Some(exprs) => uncast(&exprs.get(c.index())?.expr)?,
        };
        Some((*projection.get(c.index())?, outer || inner))
    };

    let by = inner.group_expr();
    if by.has_grouping_set() || by.expr().len() != 1 || files.is_empty() {
        return None;
    }
    let (group, cast_group) = input_column(&by.expr().first()?.0)?;
    if cast_group || !narrow(group) || file_type(group)? != DataType::Utf8 {
        return None;
    }

    let mut aggs = Vec::with_capacity(inner.aggr_expr().len());
    for (expr, filter) in inner.aggr_expr().iter().zip(inner.filter_expr()) {
        if expr.is_distinct() || !expr.order_bys().is_empty() || filter.is_some() {
            return None;
        }
        let kind = match expr.fun().name() {
            "count" => Kind::Count,
            "sum" => Kind::Sum,
            "avg" => Kind::Avg,
            "min" => Kind::Min,
            "max" => Kind::Max,
            _ => return None,
        };
        let args = expr.expressions();
        let [arg] = args.as_slice() else { return None };
        if let Some(lit) = arg.downcast_ref::<Literal>() {
            if kind != Kind::Count
                || lit.value().is_null()
                || matches!(lit.value(), ScalarValue::Null)
            {
                return None;
            }
            aggs.push(Aggregate {
                kind,
                column: None,
                float: false,
            });
            continue;
        }
        let (column, to_float) = input_column(arg)?;
        if !narrow(column) {
            return None;
        }
        let float = match file_type(column)? {
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => to_float,
            DataType::Float32 | DataType::Float64 => true,
            _ => return None,
        };
        if float && (kind == Kind::Min || kind == Kind::Max) {
            return None;
        }
        aggs.push(Aggregate {
            kind,
            column: Some(column),
            float,
        });
    }
    Some(Arc::new(StrataAggregateExec::new(
        files.to_vec(),
        predicate.cloned(),
        group,
        aggs,
        top.schema(),
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::datafusion::StrataTable;
    use crate::testgen::wide_batch;
    use crate::writer::StrataWriter;
    use ::datafusion::execution::SessionStateBuilder;
    use ::datafusion::physical_plan::displayable;
    use ::datafusion::prelude::{SessionConfig, SessionContext};
    use arrow::compute::{SortColumn, lexsort_to_indices, take_record_batch};
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

    /// Rows `from..to` of `wide_batch`, written with `config` and opened.
    async fn file(rows: std::ops::Range<usize>, config: Config) -> Arc<StrataFile> {
        let all = wide_batch(rows.end);
        let batch = all.slice(rows.start, rows.end - rows.start);
        let mut w = StrataWriter::new(batch.schema(), &["trace_id", "start_time"], config).unwrap();
        w.write(&batch).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("agg.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        Arc::new(
            StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap(),
        )
    }

    /// The rows of `sql` sorted by every column, and the physical plan, with the rule or not.
    async fn run(files: &[Arc<StrataFile>], sql: &str, rule: bool) -> (RecordBatch, String) {
        let config = SessionConfig::new().with_target_partitions(4);
        let mut builder = SessionStateBuilder::new()
            .with_config(config)
            .with_default_features();
        if rule {
            builder = builder.with_physical_optimizer_rule(Arc::new(StrataAggregateRule));
        }
        let ctx = SessionContext::new_with_state(builder.build());
        ctx.register_table("t", Arc::new(StrataTable::new(files.to_vec()).unwrap()))
            .unwrap();
        let frame = ctx.sql(sql).await.unwrap();
        let plan = frame.clone().create_physical_plan().await.unwrap();
        let shown = displayable(plan.as_ref()).indent(true).to_string();
        let schema = Arc::new(frame.schema().as_arrow().clone());
        let batches = frame.collect().await.unwrap();
        let all = arrow::compute::concat_batches(&schema, &batches).unwrap();
        // Sorted by the group column, as text, so both runs compare row by row.
        let key = cast(all.column(0), &DataType::Utf8).unwrap();
        let order = lexsort_to_indices(
            &[SortColumn {
                values: key,
                options: None,
            }],
            None,
        )
        .unwrap();
        (take_record_batch(&all, &order).unwrap(), shown)
    }

    /// `a` and `b` hold the same values; floats within a relative 1e-9, NaN equal to NaN.
    fn assert_close(a: &RecordBatch, b: &RecordBatch, sql: &str) {
        assert_eq!(a.schema(), b.schema(), "{sql}");
        assert_eq!(a.num_rows(), b.num_rows(), "{sql}");
        for (x, y) in a.columns().iter().zip(b.columns()) {
            if let (Some(x), Some(y)) = (
                x.as_primitive_opt::<Float64Type>(),
                y.as_primitive_opt::<Float64Type>(),
            ) {
                for i in 0..x.len() {
                    assert_eq!(x.is_null(i), y.is_null(i), "{sql}");
                    let (u, v) = (x.value(i), y.value(i));
                    let same = (u.is_nan() && v.is_nan())
                        || (u - v).abs() <= 1e-9 * u.abs().max(v.abs()).max(1.0);
                    assert!(same, "{sql}: {u} against {v}");
                }
            } else {
                assert_eq!(x, y, "{sql}");
            }
        }
    }

    /// Every supported aggregate, grouped on dictionary columns (one file of three without a
    /// dictionary for `model`, so it groups by value), with NULL groups and values, filters,
    /// and several partitions: the rows are DataFusion's own.
    #[tokio::test]
    async fn grouping_on_codes_returns_datafusions_rows() {
        let no_dict = Config {
            dict_max_values: 4,
            ..Config::default()
        };
        let files = vec![
            file(0..3_000, Config::default()).await,
            file(3_000..6_000, no_dict).await,
            file(6_000..140_000, Config::default()).await,
        ];
        // The last query prunes every file: nothing is left to aggregate in Strata.
        for (sql, rewritten) in [
            (
                "SELECT model, count(*), count(input_tokens), sum(input_tokens), \
                 avg(input_tokens), min(input_tokens), max(input_tokens), sum(f32), avg(f32), \
                 count(cost), sum(cost), avg(cost) FROM t GROUP BY model",
                true,
            ),
            (
                "SELECT status, count(*), sum(input_tokens) FROM t WHERE span_kind = 'llm' \
                 AND input_tokens < 1000 GROUP BY status",
                true,
            ),
            (
                "SELECT span_kind, max(input_tokens), min(input_tokens) FROM t \
                 WHERE model IS NULL GROUP BY span_kind",
                true,
            ),
            (
                "SELECT model, count(*) FROM t WHERE status = 'none' GROUP BY model",
                false,
            ),
        ] {
            let (want, plain) = run(&files, sql, false).await;
            let (got, plan) = run(&files, sql, true).await;
            assert_eq!(
                plan.contains("StrataAggregateExec"),
                rewritten,
                "{sql}\n{plan}"
            );
            assert!(!plain.contains("StrataAggregateExec"), "{sql}\n{plain}");
            assert_close(&got, &want, sql);
        }
    }

    /// Shapes the rule leaves to DataFusion: two group columns, a float `min`, a `DISTINCT`, a
    /// payload or Map group column, an expression argument.
    #[tokio::test]
    async fn other_aggregates_stay_in_datafusion() {
        let files = vec![file(0..3_000, Config::default()).await];
        for sql in [
            "SELECT model, status, count(*) FROM t GROUP BY model, status",
            "SELECT model, min(cost) FROM t GROUP BY model",
            "SELECT model, count(DISTINCT status) FROM t GROUP BY model",
            "SELECT model, sum(input_tokens * 2) FROM t GROUP BY model",
            "SELECT model, count(*) FROM t WHERE uid LIKE '%7%' GROUP BY model",
        ] {
            let (want, _) = run(&files, sql, false).await;
            let (got, plan) = run(&files, sql, true).await;
            assert!(!plan.contains("StrataAggregateExec"), "{sql}\n{plan}");
            assert_close(&got, &want, sql);
        }
    }
}
