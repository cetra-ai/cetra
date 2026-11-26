//! Typed Shape B SQL [`LogicalPlan`] IR, [`SqlColumn`], and output-name validation.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::{Arc, LazyLock};

use cetra_column::{DataType, Field, Schema};

use crate::error::InvalidQuery;
use crate::exec::hash_agg::AggExpr;
use crate::exec::sort_key::SortKey;
use crate::exec::sort_window::WindowSpec;
use crate::exec::source::{IdLookupHint, MapProjection, SubstringHint, TimeRangeHint};
use crate::ir::Expr;

use super::catalog::SqlViewKind;

/// One output column of a Shape B SQL query (`name` + `cetra_column::DataType`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlColumn {
    /// Output column name.
    pub name: String,
    /// Output column data type.
    pub data_type: DataType,
}

impl SqlColumn {
    /// Creates a [`SqlColumn`].
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    /// Unified cetra `columns[].type` display name (`DataType::display_name()`, D-QE-15).
    pub fn type_name(&self) -> &'static str {
        self.data_type.display_name()
    }
}

/// Converts a [`Schema`] into its ordered [`SqlColumn`] list.
pub fn columns_from_schema(schema: &Schema) -> Vec<SqlColumn> {
    schema
        .fields()
        .iter()
        .map(|f| SqlColumn::new(f.name.clone(), f.data_type.clone()))
        .collect()
}

/// Rejects duplicate output column names with DataFusion-matching error message.
pub fn unique_names(schema: &Schema) -> Result<(), InvalidQuery> {
    let mut seen = HashSet::with_capacity(schema.len());
    for field in schema.fields() {
        if !seen.insert(field.name.as_str()) {
            return Err(InvalidQuery::new(
                format!(
                    "column name `{}` appears twice; rename one with AS",
                    field.name
                ),
                None,
            ));
        }
    }
    Ok(())
}

static EXPLAIN_SCHEMA: LazyLock<Arc<Schema>> = LazyLock::new(|| {
    Schema::try_new_arc(vec![
        Field::new("plan_type", DataType::Utf8, false),
        Field::new("plan", DataType::Utf8, false),
    ])
    .unwrap_or_else(|_| unreachable!())
});

/// Two-column `["plan_type", "plan"]` schema returned by `EXPLAIN`.
pub fn explain_schema() -> Arc<Schema> {
    Arc::clone(&EXPLAIN_SCHEMA)
}

/// Join semantics in [`LogicalPlan::Join`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalJoinKind {
    /// `INNER JOIN`
    Inner,
    /// `LEFT [OUTER] JOIN`
    Left,
    /// `RIGHT [OUTER] JOIN`
    Right,
    /// `FULL [OUTER] JOIN`
    Full,
    /// Left semi-join (`IN (SELECT ...)` / `EXISTS` / `INTERSECT`)
    Semi,
    /// Left anti-join (`NOT EXISTS` / `EXCEPT`)
    Anti,
    /// Null-aware left anti-join (`NOT IN (SELECT ...)`)
    AntiNullAware,
    /// `CROSS JOIN`
    Cross,
}

/// Typed logical plan IR produced by the Shape B SQL binder and transformed by the optimizer.
///
/// Implements a manual privacy-safe [`fmt::Debug`] that never logs literal SQL values or `Values`
/// cell payloads (`AGENTS.md` § Privacy).
#[derive(Clone, PartialEq)]
pub enum LogicalPlan {
    /// Scan of a base storage table or registered `BatchSource`.
    Scan {
        /// Canonical table name in [`super::catalog::SqlCatalog`].
        table: String,
        /// View kind (`Spans`, `Traces`, `Scores`, `Batch`).
        kind: SqlViewKind,
        /// Full unprojected source schema (`raw_schema` for storage views, `schema` for `Batch`).
        full_schema: Arc<Schema>,
        /// Projected column indices in `full_schema` (`None` = all columns).
        projection: Option<Vec<usize>>,
        /// Conjunctive filter expressions bound against `full_schema`.
        filters: Vec<Expr>,
        /// Time range pruning hint.
        time_range: TimeRangeHint,
        /// Binary/string ID lookup hints.
        id_lookups: Vec<IdLookupHint>,
        /// CNF substring search clauses (outer AND, inner OR).
        substrings: Vec<Vec<SubstringHint>>,
        /// Map column projections keyed by column name for shredded readers.
        map_projections: BTreeMap<String, MapProjection>,
        /// Optional scan limit hint.
        limit: Option<usize>,
        /// Projected output schema of this `Scan`.
        schema: Arc<Schema>,
    },
    /// Constant `VALUES (...)` rows evaluated without a backing table.
    Values {
        /// Output schema.
        schema: Arc<Schema>,
        /// Row expressions (each row has `schema.len()` expressions).
        rows: Vec<Vec<Expr>>,
    },
    /// Single 0-column row used as input for `SELECT <exprs>` without a `FROM` clause.
    EmptyRow {
        /// Empty 0-field schema.
        schema: Arc<Schema>,
    },
    /// Row-level `WHERE` / `HAVING` filter.
    Filter {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Boolean filter predicate bound against `input.schema()`.
        predicate: Expr,
    },
    /// Column projection (`SELECT` expressions / view expansion).
    Project {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Output expressions bound against `input.schema()`.
        exprs: Vec<Expr>,
        /// Output schema.
        schema: Arc<Schema>,
    },
    /// Grouped or global hash aggregation.
    Aggregate {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Grouping `(output_name, expr)` expressions.
        group_by: Vec<(String, Expr)>,
        /// Aggregate output expressions (`exec::hash_agg::AggExpr`).
        aggs: Vec<AggExpr>,
        /// Output schema (`group_by` columns followed by `aggs`).
        schema: Arc<Schema>,
    },
    /// Window function evaluation sharing `(partition_by, order_by)`.
    Window {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Partition expressions.
        partition_by: Vec<Expr>,
        /// Ordering keys within each partition.
        order_by: Vec<SortKey>,
        /// Window output specifications appended to `input.schema()`.
        specs: Vec<WindowSpec>,
        /// Output schema (`input.schema()` + `specs`).
        schema: Arc<Schema>,
    },
    /// Full sort (`ORDER BY`).
    Sort {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Sort keys bound against `input.schema()`.
        keys: Vec<SortKey>,
    },
    /// `LIMIT` / `OFFSET`.
    Limit {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Number of leading rows to skip.
        skip: usize,
        /// Maximum rows to emit (`None` = unbounded).
        fetch: Option<usize>,
    },
    /// Bounded `TopK` (`ORDER BY ... LIMIT k [OFFSET n]` when `offset + limit <= 65_536`).
    TopK {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Sort keys.
        keys: Vec<SortKey>,
        /// Maximum rows to emit after `offset`.
        limit: usize,
        /// Leading rows to skip in sorted order.
        offset: usize,
    },
    /// Duplicate elimination (`SELECT DISTINCT` / set operation deduplication).
    Distinct {
        /// Child plan.
        input: Box<LogicalPlan>,
    },
    /// Relational join (inner/outer/semi/anti/null-aware-anti/cross).
    Join {
        /// Left (probe) child.
        left: Box<LogicalPlan>,
        /// Right (build) child.
        right: Box<LogicalPlan>,
        /// Join variant.
        kind: LogicalJoinKind,
        /// Left equi-join keys (empty for `Cross`).
        left_keys: Vec<Expr>,
        /// Right equi-join keys (empty for `Cross`).
        right_keys: Vec<Expr>,
        /// Whether `NULL = NULL` compares equal (`true` for `INTERSECT`/`EXCEPT`).
        null_equals_null: bool,
        /// Optional non-equi residual predicate bound against `(left_cols..., right_cols...)`.
        residual: Option<Expr>,
        /// Output schema.
        schema: Arc<Schema>,
    },
    /// `UNION` (`all: false`) or `UNION ALL` (`all: true`).
    Union {
        /// Input branches (non-empty, schemas coerced to `schema`).
        inputs: Vec<LogicalPlan>,
        /// True for `UNION ALL`, false for `UNION`.
        all: bool,
        /// Coerced output schema.
        schema: Arc<Schema>,
    },
    /// `UNNEST` over one or more `List` columns.
    Unnest {
        /// Child plan.
        input: Box<LogicalPlan>,
        /// Column indices in `input.schema()` to unnest.
        columns: Vec<usize>,
        /// Whether empty/null lists emit a `NULL`-padded row (`true`) or drop the row (`false`).
        preserve_nulls: bool,
        /// Output schema.
        schema: Arc<Schema>,
    },
    /// `EXPLAIN <query>` producing `["plan_type", "plan"]`.
    Explain {
        /// Inner query plan to optimize and explain.
        inner: Box<LogicalPlan>,
        /// Two-column `explain_schema()`.
        schema: Arc<Schema>,
    },
}

impl LogicalPlan {
    /// Output [`Schema`] of this plan node.
    pub fn schema(&self) -> &Arc<Schema> {
        match self {
            Self::Scan { schema, .. }
            | Self::Values { schema, .. }
            | Self::EmptyRow { schema }
            | Self::Project { schema, .. }
            | Self::Aggregate { schema, .. }
            | Self::Window { schema, .. }
            | Self::Join { schema, .. }
            | Self::Union { schema, .. }
            | Self::Unnest { schema, .. }
            | Self::Explain { schema, .. } => schema,
            Self::Filter { input, .. }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. }
            | Self::TopK { input, .. }
            | Self::Distinct { input } => input.schema(),
        }
    }
}

impl fmt::Debug for LogicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scan {
                table,
                kind,
                projection,
                filters,
                id_lookups,
                substrings,
                map_projections,
                limit,
                schema,
                ..
            } => f
                .debug_struct("Scan")
                .field("table", table)
                .field("kind", kind)
                .field("projection_len", &projection.as_ref().map(Vec::len))
                .field("filters_len", &filters.len())
                .field("id_lookups", id_lookups)
                .field("substrings", substrings)
                .field("map_projections_len", &map_projections.len())
                .field("limit", limit)
                .field("out_cols", &schema.len())
                .finish(),
            Self::Values { schema, rows } => f
                .debug_struct("Values")
                .field("cols", &schema.len())
                .field("rows_len", &rows.len())
                .finish(),
            Self::EmptyRow { .. } => f.debug_struct("EmptyRow").finish(),
            Self::Filter { input, .. } => f
                .debug_struct("Filter")
                .field("input", input)
                .finish_non_exhaustive(),
            Self::Project { input, schema, .. } => f
                .debug_struct("Project")
                .field("out_cols", &schema.len())
                .field("input", input)
                .finish(),
            Self::Aggregate {
                input,
                group_by,
                aggs,
                schema,
            } => f
                .debug_struct("Aggregate")
                .field("group_by_len", &group_by.len())
                .field("aggs_len", &aggs.len())
                .field("out_cols", &schema.len())
                .field("input", input)
                .finish(),
            Self::Window {
                input,
                partition_by,
                order_by,
                specs,
                schema,
            } => f
                .debug_struct("Window")
                .field("partition_by_len", &partition_by.len())
                .field("order_by_len", &order_by.len())
                .field("specs_len", &specs.len())
                .field("out_cols", &schema.len())
                .field("input", input)
                .finish(),
            Self::Sort { input, keys } => f
                .debug_struct("Sort")
                .field("keys_len", &keys.len())
                .field("input", input)
                .finish(),
            Self::Limit { input, skip, fetch } => f
                .debug_struct("Limit")
                .field("skip", skip)
                .field("fetch", fetch)
                .field("input", input)
                .finish(),
            Self::TopK {
                input,
                keys,
                limit,
                offset,
            } => f
                .debug_struct("TopK")
                .field("keys_len", &keys.len())
                .field("limit", limit)
                .field("offset", offset)
                .field("input", input)
                .finish(),
            Self::Distinct { input } => f.debug_struct("Distinct").field("input", input).finish(),
            Self::Join {
                left,
                right,
                kind,
                left_keys,
                null_equals_null,
                residual,
                schema,
                ..
            } => f
                .debug_struct("Join")
                .field("kind", kind)
                .field("keys_len", &left_keys.len())
                .field("null_equals_null", null_equals_null)
                .field("has_residual", &residual.is_some())
                .field("out_cols", &schema.len())
                .field("left", left)
                .field("right", right)
                .finish(),
            Self::Union {
                inputs,
                all,
                schema,
            } => f
                .debug_struct("Union")
                .field("all", all)
                .field("inputs_len", &inputs.len())
                .field("out_cols", &schema.len())
                .finish(),
            Self::Unnest {
                input,
                columns,
                preserve_nulls,
                schema,
            } => f
                .debug_struct("Unnest")
                .field("columns", columns)
                .field("preserve_nulls", preserve_nulls)
                .field("out_cols", &schema.len())
                .field("input", input)
                .finish(),
            Self::Explain { inner, .. } => f.debug_struct("Explain").field("inner", inner).finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::ScalarValue;

    use super::*;

    #[test]
    fn sql_column_type_names_and_unique_names() {
        let col_ts = SqlColumn::new("t", DataType::timestamp_utc());
        assert_eq!(col_ts.type_name(), "timestamp");
        let col_ev = SqlColumn::new("events", DataType::Events);
        assert_eq!(col_ev.type_name(), "json");
        let col_tags = SqlColumn::new("tags", DataType::List(Box::new(DataType::Utf8)));
        assert_eq!(col_tags.type_name(), "string_list");

        let ok_schema = Schema::try_new(vec![
            Field::new("a", DataType::I64, false),
            Field::new("b", DataType::Utf8, true),
        ])
        .unwrap();
        assert!(unique_names(&ok_schema).is_ok());

        let dup_schema = Schema::try_new(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("other", DataType::I64, false),
            Field::new("name", DataType::Utf8, true),
        ])
        .unwrap();
        let err = unique_names(&dup_schema).unwrap_err();
        assert_eq!(
            err.message,
            "column name `name` appears twice; rename one with AS"
        );
        assert_eq!(err.position, None);

        // Explain schema is ["plan_type", "plan"]
        let es = explain_schema();
        let es_cols = columns_from_schema(&es);
        assert_eq!(
            es_cols,
            vec![
                SqlColumn::new("plan_type", DataType::Utf8),
                SqlColumn::new("plan", DataType::Utf8),
            ]
        );

        // Privacy-safe Debug on LogicalPlan::Values omits literal strings.
        let plan = LogicalPlan::Values {
            schema: Arc::new(ok_schema),
            rows: vec![vec![
                Expr::Literal(ScalarValue::I64(1)),
                Expr::Literal(ScalarValue::Utf8("secret-user-payload".to_owned())),
            ]],
        };
        let dbg = format!("{plan:?}");
        assert!(!dbg.contains("secret-user-payload"), "leaked in {dbg}");
        assert!(dbg.contains("rows_len: 1"));
    }
}
