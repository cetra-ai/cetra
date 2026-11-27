#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Pure-Rust vectorized expressions, Filter DSL, and tombstone predicates for Cetra.

pub mod bind;
pub mod encode;
pub mod error;
pub mod eval;
pub mod exec;
pub mod filter;
pub mod fn_misc;
pub mod fn_string;
pub mod fn_time;
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzz_entry;
pub mod ir;
pub mod sql;
pub mod text;
pub mod tombstone;

pub use bind::{CompiledFilter, bind_expr, compile_filter};
pub use error::{InvalidQuery, MESSAGE_CAP, Position, QueryError, cap_message};
pub use eval::{eval_expr, eval_filter};
pub use filter::{
    FilterView, MAX_SCORE_NAMES, ParsedFilter, filter_planning_schema, like_pattern,
    parse_filter_ast,
};
pub use ir::{BinaryOp, CastMode, EvalCache, EvalContext, Expr, ScalarFn, UnaryOp};
pub use text::{
    DENIED_FUNCTIONS, FILTER_LIMITS, PARSER_RECURSION_LIMIT, SQL_LIMITS, TextKind, TextLimits,
};
pub use tombstone::{
    TombstoneSpec, TombstoneTable, covered_predicate, filter_uncovered, key_columns,
};
