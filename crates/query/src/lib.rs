#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Pure-Rust vectorized expressions, Filter DSL, and tombstone predicates for Cetra.

pub mod bind;
pub mod error;
pub mod eval;
pub mod filter;
pub mod fn_misc;
pub mod fn_string;
pub mod fn_time;
pub mod ir;
pub mod text;
pub mod tombstone;

pub use bind::{CompiledFilter, bind_expr, compile_filter};
pub use error::{InvalidQuery, MESSAGE_CAP, Position, QueryError, cap_message};
pub use eval::{eval_expr, eval_filter};
pub use filter::{
    FilterView, MAX_SCORE_NAMES, ParsedFilter, filter_planning_schema, like_pattern,
    parse_filter_ast,
};
pub use ir::{BinaryOp, CastMode, EvalContext, Expr, ScalarFn, UnaryOp};
pub use text::{
    DENIED_FUNCTIONS, FILTER_LIMITS, PARSER_RECURSION_LIMIT, SQL_LIMITS, TextKind, TextLimits,
};
pub use tombstone::{
    TombstoneSpec, TombstoneTable, covered_predicate, filter_uncovered, key_columns,
};
