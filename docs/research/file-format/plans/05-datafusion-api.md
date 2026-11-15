# DataFusion 55.2 API reference for M5

These are the exact signatures and behaviours of DataFusion 55.2.0 that a custom `TableProvider` and `ExecutionPlan` rely on. They were read from the source under `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/datafusion-*-55.2.0/` and compile-checked in a probe crate. Every Expr shape in § 5 was observed by planning real SQL.

Path shorthand:

| Short name | Path |
|---|---|
| SESS | `datafusion-session-55.2.0/src` |
| PLAN | `datafusion-physical-plan-55.2.0/src` |
| EXPR | `datafusion-expr-55.2.0/src` |
| PEC | `datafusion-physical-expr-common-55.2.0/src` |
| PE | `datafusion-physical-expr-55.2.0/src` |
| OPT | `datafusion-optimizer-55.2.0/src` |

## 0. Differences from older DataFusion docs

- **No `as_any`.** `TableProvider` and `ExecutionPlan` have no `as_any` method; do not write one. Their declarations are `trait TableProvider: Any + Debug + Sync + Send` (SESS/table.rs:52) and `trait ExecutionPlan: Any + Debug + DisplayAs + Send + Sync` (PLAN/execution_plan.rs:102).
- **`with_new_children`** is deprecated since 55.0 but still **required**: it has no default body (PLAN/execution_plan.rs:435).
- **`apply_expressions`** is required too (PLAN/execution_plan.rs:349).
- **`Expr::Literal`** has two fields: `Literal(ScalarValue, Option<FieldMetadata>)` (EXPR/expr.rs:334).

## 1. `TableProvider`

```rust
use async_trait::async_trait;                        // the trait is #[async_trait] (SESS/table.rs:51)
use datafusion::catalog::Session;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result};
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown, TableType};
use datafusion::physical_plan::ExecutionPlan;

fn schema(&self) -> SchemaRef;                                                   // required
fn table_type(&self) -> TableType;                                               // required: TableType::Base
async fn scan(&self, state: &dyn Session, projection: Option<&Vec<usize>>,
              filters: &[Expr], limit: Option<usize>) -> Result<Arc<dyn ExecutionPlan>>;   // required
fn supports_filters_pushdown(&self, filters: &[&Expr])
    -> Result<Vec<TableProviderFilterPushDown>>;                                 // default: all Unsupported
```

**`TableProviderFilterPushDown` variants:**

| Variant | Effect |
|---|---|
| `Unsupported` | the filter is not pushed |
| `Inexact` | the filter is pushed, and DataFusion keeps its own `Filter` above the scan |
| `Exact` | the filter is pushed and removed from the plan, so the provider must apply it with exact SQL semantics |

**`supports_filters_pushdown`:**
- The returned `Vec` must have exactly `filters.len()` entries; otherwise planning fails with an internal error.
- It is called several times per query, so it must be pure.

**Column names.**
- In `supports_filters_pushdown`, columns are qualified: `Column { relation: Some(..), name }`.
- In `scan`, they are unqualified.
- Match on `Column.name` only.

## 2. `ExecutionPlan`

```rust
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::execution::TaskContext;
use datafusion::physical_expr::{EquivalenceProperties, PhysicalExpr, PhysicalSortExpr};
use datafusion::physical_expr::expressions::Column;          // Column::new(name: &str, index: usize)
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType, SchedulingType};
use datafusion::physical_plan::{ChildrenPropertiesMode, DisplayAs, DisplayFormatType, ExecutionPlan,
    Partitioning, PlanProperties, ReplaceChildrenOptions, SendableRecordBatchStream};

fn name(&self) -> &str;
fn properties(&self) -> &Arc<PlanProperties>;                // note the Arc
fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>>;          // leaf: vec![]
fn apply_expressions(&self, f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> Result<TreeNodeRecursion>)
    -> Result<TreeNodeRecursion>;                            // leaf: Ok(TreeNodeRecursion::Continue)
fn replace_children(self: Arc<Self>, children: Vec<Arc<dyn ExecutionPlan>>,
                    options: ReplaceChildrenOptions) -> Result<Arc<dyn ExecutionPlan>>;   // leaf: Ok(self)
fn with_new_children(self: Arc<Self>, children: Vec<Arc<dyn ExecutionPlan>>)
    -> Result<Arc<dyn ExecutionPlan>>;
    // leaf: self.replace_children(children, ReplaceChildrenOptions::new(ChildrenPropertiesMode::Recompute))
fn execute(&self, partition: usize, context: Arc<TaskContext>) -> Result<SendableRecordBatchStream>;
fn metrics(&self) -> Option<MetricsSet> { None }             // optional

// DisplayAs, a required supertrait:
fn fmt_as(&self, t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result;
```

**`PlanProperties`** (PLAN/execution_plan.rs:1503):

```rust
PlanProperties::new(eq_properties: EquivalenceProperties, partitioning: Partitioning,
                    emission_type: EmissionType, boundedness: Boundedness)
    .with_scheduling_type(SchedulingType::Cooperative)   // avoids a CooperativeExec wrapper
```

Use `Partitioning::UnknownPartitioning(n)`, `EmissionType::Incremental` and `Boundedness::Bounded`.

**Ordering.**
- Build the properties with `EquivalenceProperties::new(schema)`, then call `eq.add_ordering([PhysicalSortExpr::new(Arc::new(Column::new(name, idx)), SortOptions { descending: false, nulls_first: false }), ..])`.
- `idx` is the index in the exec's **output** (projected) schema.
- The ordering applies within each partition. DataFusion trusts it, so a wrong claim gives wrong `ORDER BY` results.
- `SortOptions` is `arrow::compute::SortOptions`.

## 3. Streams

```rust
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;   // RecordBatchStreamAdapter::new(schema, stream)
use futures::TryStreamExt;

let fut = async move { /* … */ Ok::<Vec<RecordBatch>, DataFusionError>(batches) };
let stream = futures::stream::once(fut)
    .map_ok(|v| futures::stream::iter(v.into_iter().map(Ok)))
    .try_flatten();
Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
```

The future must be `Send + 'static`: clone the `Arc`s and move them in.

## 4. Metrics

```rust
use datafusion::physical_plan::metrics::{Count, ExecutionPlanMetricsSet, MetricBuilder, MetricsSet, Time};

ExecutionPlanMetricsSet::new()                            // a field of the exec; derives Debug, Clone, Default
MetricBuilder::new(&set).output_rows(partition) -> Count
MetricBuilder::new(&set).counter("strata_waves", partition) -> Count   // name: impl Into<Cow<'static, str>>
MetricBuilder::new(&set).elapsed_compute(partition) -> Time
Count::add(&self, n: usize)                               // Count is Clone; create in execute(), move into the future
Time::timer(&self) -> ScopedTimerGuard<'_>                // records on drop
fn metrics(&self) -> Option<MetricsSet> { Some(self.metrics.clone_inner()) }
```

## 5. Filters the provider receives

**How filters arrive.**
- Type coercion runs first, then the expression simplifier, then `PushDownFilter`.
- The predicate is split on top-level `AND`: each conjunct is a separate filter.
- Volatile functions and subqueries are never offered.
- `scan` receives only the filters answered `Exact` or `Inexact`.

Observed shapes, with a table schema of `status Utf8View`, `input Utf8View`, `start_time Timestamp(ns, "UTC")`, `trace_id FixedSizeBinary(16)`, `n Int64` and `metadata Map<Utf8, Utf8>`:

| SQL | Expr |
|---|---|
| `status = 'error'` | `BinaryExpr { left: Column, op: Eq, right: Literal(ScalarValue::Utf8View(Some("error")), None) }`, with no Cast |
| `start_time >= to_timestamp_nanos(123)` | `BinaryExpr { Column, GtEq, Literal(ScalarValue::TimestampNanosecond(Some(123), Some("UTC")), None) }`; folded, no Cast. `TIMESTAMP '…Z'` and `now()` fold the same way |
| `trace_id = X'<16 bytes>'` | `BinaryExpr { Column, Eq, Literal(ScalarValue::FixedSizeBinary(16, Some(bytes)), None) }` |
| `trace_id = X'00ff'` (wrong length) | `BinaryExpr { Cast { expr: Column, .. }, Eq, Literal(Binary(..)) }`: a Cast on the column |
| `c IN (a, b, c)` (1–3 items) | `Or` of `Eq` `BinaryExpr`s |
| `c IN (…)` (≥ 4 items) | `Expr::InList(InList { expr: Box<Expr>, list: Vec<Expr>, negated: bool })` |
| `c IS NULL`, `c IS NOT NULL` | `Expr::IsNull(Box<Expr>)`, `Expr::IsNotNull(Box<Expr>)` |
| `n BETWEEN a AND b` | two filters, `n >= a` and `n <= b` |
| `NOT (status = 'x')` | `BinaryExpr { .., NotEq, .. }`; `Expr::Not` did not appear in practice, but handle it |
| `a OR b` | `BinaryExpr { left, op: Operator::Or, right }` |
| `input LIKE '%abc%'` | `Expr::Like(Like { negated: false, expr: Box<Column>, pattern: Box<Literal(Utf8View(Some("%abc%")))>, escape_char: None, case_insensitive: false })` |
| `metadata['k'] = '"v"'` | `BinaryExpr { left: Expr::ScalarFunction(ScalarFunction { func, args: [Column(metadata), Literal(Utf8(Some("k")), None)] }), op: Eq, right: Literal(ScalarValue::Utf8(Some("\"v\"")), None) }`, with `func.name() == "get_field"`. The value literal is `Utf8`, not `Utf8View` |
| `CAST(n AS DOUBLE) > 1`, `upper(status) = 'A'`, `status = n::varchar` | a `Cast` or `ScalarFunction` on the column, or a non-literal operand |

**Relevant `ScalarValue` variants:**
- `Boolean(Option<bool>)`;
- `Int8` to `Int64`, `UInt8` to `UInt64`;
- `Float32(Option<f32>)`, `Float64(Option<f64>)`;
- `Utf8(Option<String>)`, `Utf8View(Option<String>)`, `LargeUtf8(Option<String>)`;
- `Binary(Option<Vec<u8>>)`, `BinaryView(Option<Vec<u8>>)`;
- `FixedSizeBinary(i32, Option<Vec<u8>>)`;
- `TimestampNanosecond(Option<i64>, Option<Arc<str>>)`.

`None` is a SQL NULL.

**Other Expr items:**
- `Operator` is `datafusion::logical_expr::Operator`.
- `BinaryExpr { left: Box<Expr>, op, right: Box<Expr> }`.
- `Expr::Between` exists, but the simplifier rewrites it away before pushdown.
- A literal on the left is normally flipped to the right by the simplifier; still match both sides.

## 6. Registering

```rust
ctx.register_table("t", Arc::new(provider))?;   // SessionContext::register_table(impl Into<TableReference>, Arc<dyn TableProvider>)
```

## 7. Rules that make `Exact` correct

- **`Exact` removes DataFusion's own filter.** The provider must then apply the filter with DataFusion's semantics: three-valued logic, and NaN, null and case handling identical to Arrow's kernels.
- **Unhandled shapes.** Any shape the provider does not handle completely is `Unsupported`, or `Inexact` when it is only used to prune. That includes a `Cast` or `TryCast` on the column, a non-literal operand, or another function.
- **Filter columns.** A filter column may be absent from the projection when the filter is `Exact`. The provider reads it itself.
- **Projection.**
  - Projection indexes refer to `TableProvider::schema()`.
  - The output schema is `datafusion::common::utils::project_schema(&schema, projection)?`.
  - The batches must match it exactly, data types included (Utf8View arrays for Utf8View fields).
- **Empty projection.** `count(*)` gives `projection = Some([])`. Emit zero-column batches with a row count: `RecordBatch::try_new_with_options(schema, vec![], &RecordBatchOptions::new().with_row_count(Some(n)))`.
- **`limit`.**
  - It is passed only when every pushed filter is `Exact`.
  - Returning more rows is allowed: DataFusion keeps its own limit above the scan.
  - `LIMIT 3 OFFSET 2` gives `limit = Some(5)`.
- **Partitions.** With `UnknownPartitioning(n)`, `execute(partition)` must check `partition < n`.
