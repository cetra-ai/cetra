//! Shape B SQL frontend: validation, parameters, view/function catalogs, and logical IR.

pub mod binder;
pub mod catalog;
pub mod explain;
pub mod logical;
pub mod optimize;
pub mod params;
pub mod validate;

pub use binder::{BoundParams, bind_statement};

pub use catalog::{
    FunctionInfo, FunctionKind, SqlCatalog, SqlTableDef, SqlViewKind, TimeWindow, function_catalog,
    raw_scores_schema, raw_spans_schema, raw_traces_schema, scores_view_schema,
    sessions_view_schema, spans_view_schema, traces_view_schema,
};
pub use explain::{explain_batch, format_logical_plan};
pub use logical::{
    LogicalJoinKind, LogicalPlan, SqlColumn, columns_from_schema, explain_schema, unique_names,
};
pub use optimize::{TOPK_MAX_ROWS, optimize};
pub use params::{
    ParamKind, Placeholder, SqlParamValue, bind_placeholders, coerce_param_value,
    parse_param_values,
};
pub use validate::{MAX_TABLES, parse_and_validate_sql, validate_statement};
