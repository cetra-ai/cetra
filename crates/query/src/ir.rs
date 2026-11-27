//! Typed expression IR and runtime evaluation context.

use std::borrow::Borrow;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use cetra_column::{CancelToken, DataType, ScalarValue};
use regex::Regex;
use serde_json_path::JsonPath;

use crate::error::QueryError;
use crate::eval::{LiteralSet, f64_canonical_bits};
use crate::fn_string::{LikeMatcher, REGEX_ESTIMATED_BYTES};

/// Binary operators in [`Expr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// Logical AND (3-valued).
    And,
    /// Logical OR (3-valued).
    Or,
    /// `=`
    Eq,
    /// `!=` / `<>`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `IS DISTINCT FROM` (null-safe `!=`, never returns NULL).
    IsDistinctFrom,
    /// `IS NOT DISTINCT FROM` (null-safe `=`, never returns NULL).
    IsNotDistinctFrom,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `||` (string or list concatenation).
    Concat,
    /// `@>` (`array_has_all`).
    ArrayContains,
}

impl BinaryOp {
    fn symbol(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::IsDistinctFrom => "IS DISTINCT FROM",
            Self::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
            Self::Concat => "||",
            Self::ArrayContains => "@>",
        }
    }
}

/// Unary operators in [`Expr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Logical `NOT` (3-valued).
    Not,
    /// Arithmetic negation `-x`.
    Neg,
    /// `IS NULL` (never returns NULL).
    IsNull,
    /// `IS NOT NULL` (never returns NULL).
    IsNotNull,
    /// `IS TRUE` (`true` -> `true`, `false`/`NULL` -> `false`).
    IsTrue,
    /// `IS FALSE` (`false` -> `true`, `true`/`NULL` -> `false`).
    IsFalse,
    /// `IS NOT TRUE` (`true` -> `false`, `false`/`NULL` -> `true`).
    IsNotTrue,
    /// `IS NOT FALSE` (`false` -> `false`, `true`/`NULL` -> `true`).
    IsNotFalse,
}

/// Cast failure handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastMode {
    /// Strict SQL `CAST`: invalid conversion returns `QueryError::Failed`.
    Strict,
    /// Safe `TRY_CAST` (Filter DSL, D100): invalid conversion produces `NULL`.
    Try,
}

/// Curated Shape B scalar function identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarFn {
    // String & binary (QE2.2)
    /// `lower(s)`
    Lower,
    /// `upper(s)`
    Upper,
    /// `length(s)` / `char_length(s)`
    Length,
    /// `substr(s, start [, count])`
    Substr,
    /// `trim(s [, chars])`
    Trim,
    /// `ltrim(s [, chars])`
    Ltrim,
    /// `rtrim(s [, chars])`
    Rtrim,
    /// `left(s, n)`
    Left,
    /// `right(s, n)`
    Right,
    /// `split_part(s, delim, n)`
    SplitPart,
    /// `concat(a1, ...)`
    Concat,
    /// `concat_ws(sep, a1, ...)`
    ConcatWs,
    /// `starts_with(s, prefix)`
    StartsWith,
    /// `ends_with(s, suffix)`
    EndsWith,
    /// `contains(s, sub)`
    Contains,
    /// `strpos(s, sub)` / `position(sub in s)`
    Strpos,
    /// `regexp_like(s, pat [, flags])`
    RegexpLike,
    /// `regexp_extract(s, pat [, flags])`
    RegexpExtract,
    /// `encode(val, encoding)`
    Encode,

    // Time & interval (QE2.3)
    /// `now()`
    Now,
    /// `date_trunc(part, ts)`
    DateTrunc,
    /// `date_bin(stride, ts [, origin])`
    DateBin,
    /// `extract(field from ts)` / `date_part(field, ts)`
    Extract,
    /// `to_timestamp(val)`
    ToTimestamp,

    // Math & null handling (QE2.4)
    /// `abs(x)`
    Abs,
    /// `round(x [, dp])`
    Round,
    /// `floor(x)`
    Floor,
    /// `ceil(x)` / `ceiling(x)`
    Ceil,
    /// `sqrt(x)`
    Sqrt,
    /// `ln(x)`
    Ln,
    /// `log10(x)`
    Log10,
    /// `power(x, y)` / `pow(x, y)`
    Power,
    /// `coalesce(a1, ...)`
    Coalesce,
    /// `nullif(a, b)`
    Nullif,
    /// `greatest(a1, ...)`
    Greatest,
    /// `least(a1, ...)`
    Least,
    /// `isnan(x)`
    IsNan,
    /// `isfinite(x)`
    IsFinite,

    // Array & map (QE2.4)
    /// `array_has(arr, elem)` / `array_contains(arr, elem)`
    ArrayHas,
    /// `array_has_all(a, b)`
    ArrayHasAll,
    /// `array_length(arr)`
    ArrayLength,
    /// `array_element(arr, idx)`
    ArrayElement,
    /// `array_to_string(arr, delim [, null_str])`
    ArrayToString,
    /// `array_distinct(arr)`
    ArrayDistinct,
    /// `map_keys(m)`
    MapKeys,
    /// `map_values(m)`
    MapValues,

    // JSON (QE2.4)
    /// `json -> key_or_idx`
    JsonGet,
    /// `json ->> key_or_idx`
    JsonGetText,
    /// `json_extract(json, path)`
    JsonExtract,
    /// `json_extract_text(json, path)`
    JsonExtractText,
}

impl ScalarFn {
    /// Canonical lowercase SQL function name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Lower => "lower",
            Self::Upper => "upper",
            Self::Length => "length",
            Self::Substr => "substr",
            Self::Trim => "trim",
            Self::Ltrim => "ltrim",
            Self::Rtrim => "rtrim",
            Self::Left => "left",
            Self::Right => "right",
            Self::SplitPart => "split_part",
            Self::Concat => "concat",
            Self::ConcatWs => "concat_ws",
            Self::StartsWith => "starts_with",
            Self::EndsWith => "ends_with",
            Self::Contains => "contains",
            Self::Strpos => "strpos",
            Self::RegexpLike => "regexp_like",
            Self::RegexpExtract => "regexp_extract",
            Self::Encode => "encode",
            Self::Now => "now",
            Self::DateTrunc => "date_trunc",
            Self::DateBin => "date_bin",
            Self::Extract => "extract",
            Self::ToTimestamp => "to_timestamp",
            Self::Abs => "abs",
            Self::Round => "round",
            Self::Floor => "floor",
            Self::Ceil => "ceil",
            Self::Sqrt => "sqrt",
            Self::Ln => "ln",
            Self::Log10 => "log10",
            Self::Power => "power",
            Self::Coalesce => "coalesce",
            Self::Nullif => "nullif",
            Self::Greatest => "greatest",
            Self::Least => "least",
            Self::IsNan => "isnan",
            Self::IsFinite => "isfinite",
            Self::ArrayHas => "array_has",
            Self::ArrayHasAll => "array_has_all",
            Self::ArrayLength => "array_length",
            Self::ArrayElement => "array_element",
            Self::ArrayToString => "array_to_string",
            Self::ArrayDistinct => "array_distinct",
            Self::MapKeys => "map_keys",
            Self::MapValues => "map_values",
            Self::JsonGet => "json_get",
            Self::JsonGetText => "json_get_text",
            Self::JsonExtract => "json_extract",
            Self::JsonExtractText => "json_extract_text",
        }
    }
}

/// Typed expression tree evaluated vectorized over a `cetra_column::Batch`.
///
/// Implements manual `fmt::Debug` and `fmt::Display` so `InList { redacted: true }`
/// never exposes erased user/session IDs in logs or `EXPLAIN` output.
#[derive(Clone, PartialEq)]
pub enum Expr {
    /// Reference to column at `index` in input batch schema.
    Column {
        /// Column index in the input `Schema`.
        index: usize,
        /// Column name (diagnostics / display only).
        name: String,
        /// Resolved data type.
        data_type: DataType,
    },
    /// Constant scalar value.
    Literal(ScalarValue),
    /// Unary operation.
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        expr: Box<Expr>,
        /// Result type.
        data_type: DataType,
    },
    /// Binary operation.
    Binary {
        /// Left operand.
        left: Box<Expr>,
        /// Operator.
        op: BinaryOp,
        /// Right operand.
        right: Box<Expr>,
        /// Result type.
        data_type: DataType,
    },
    /// `CAST` or `TRY_CAST`.
    Cast {
        /// Source expression.
        expr: Box<Expr>,
        /// Target type.
        target: DataType,
        /// Strict or try-cast mode.
        mode: CastMode,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        /// Tested value.
        expr: Box<Expr>,
        /// Lower bound (inclusive).
        low: Box<Expr>,
        /// Upper bound (inclusive).
        high: Box<Expr>,
        /// True for `NOT BETWEEN`.
        negated: bool,
    },
    /// `expr [NOT] IN (list...)`.
    InList {
        /// Tested value.
        expr: Box<Expr>,
        /// Candidate values.
        list: Vec<Expr>,
        /// True for `NOT IN`.
        negated: bool,
        /// When true, `Display`, `fmt_plan`, and `Debug` render `<col> IN (<{n} redacted>)` (GDPR redaction, D96).
        redacted: bool,
    },
    /// `expr [NOT] LIKE / ILIKE pattern [ESCAPE escape]`.
    Like {
        /// Input string expression.
        expr: Box<Expr>,
        /// Pattern string expression.
        pattern: Box<Expr>,
        /// Optional single-char escape (`Some(c)` escapes with `c`, `None` disables escaping; SQL default is `Some('\\')`).
        escape: Option<char>,
        /// True for `ILIKE` (Unicode case-insensitive).
        case_insensitive: bool,
        /// True for `NOT LIKE` / `NOT ILIKE`.
        negated: bool,
    },
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END`.
    Case {
        /// Optional base operand for simple `CASE x WHEN ...`.
        operand: Option<Box<Expr>>,
        /// `(condition_or_match, result)` branches.
        when_then: Vec<(Expr, Expr)>,
        /// Optional `ELSE` branch (defaults to `NULL` of `data_type`).
        else_expr: Option<Box<Expr>>,
        /// Resolved result type.
        data_type: DataType,
    },
    /// Map lookup `map_col['key']` (returns nullable `Utf8`).
    MapIndex {
        /// Map column expression (`DataType::MapUtf8`).
        map: Box<Expr>,
        /// Key expression (`DataType::Utf8`).
        key: Box<Expr>,
    },
    /// Dynamic array construction `[expr1, expr2, ...]` (constant arrays fold to `Literal(ScalarValue::List { .. })`).
    MakeArray {
        /// Element expressions (all coerced to `item_type`).
        elems: Vec<Expr>,
        /// Element `DataType` (the expression's own type is `DataType::List(Box::new(item_type))`).
        item_type: DataType,
    },
    /// Curated scalar function call.
    Call {
        /// Function identifier.
        func: ScalarFn,
        /// Evaluated arguments.
        args: Vec<Expr>,
        /// Resolved return type.
        data_type: DataType,
    },
}

impl Expr {
    /// Concrete [`DataType`] produced by evaluating this expression.
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Column { data_type, .. }
            | Self::Unary { data_type, .. }
            | Self::Binary { data_type, .. }
            | Self::Case { data_type, .. }
            | Self::Call { data_type, .. } => data_type.clone(),
            Self::Literal(v) => v.data_type(),
            Self::Cast { target, .. } => target.clone(),
            Self::Between { .. } | Self::InList { .. } | Self::Like { .. } => DataType::Bool,
            Self::MapIndex { .. } => DataType::Utf8,
            Self::MakeArray { item_type, .. } => DataType::List(Box::new(item_type.clone())),
        }
    }

    /// Formats a privacy-safe plan summary (respecting `redacted: true` on `InList`, formatting `"{expr} IN (<{n} redacted>)"`).
    ///
    /// Implemented iteratively so arbitrarily deep `Expr::Binary` spines never overflow the stack.
    pub fn fmt_plan(&self) -> String {
        enum FmtOp<'a> {
            Lit(&'static str),
            Owned(String),
            Node(&'a Expr),
            CommaSep(&'a [Expr]),
            WhenThen(&'a [(Expr, Expr)]),
        }

        let mut out = String::new();
        let mut stack = vec![FmtOp::Node(self)];
        while let Some(op) = stack.pop() {
            match op {
                FmtOp::Lit(s) => out.push_str(s),
                FmtOp::Owned(s) => out.push_str(&s),
                FmtOp::CommaSep(slice) => {
                    if let Some((first, rest)) = slice.split_first() {
                        if !rest.is_empty() {
                            stack.push(FmtOp::CommaSep(rest));
                            stack.push(FmtOp::Lit(", "));
                        }
                        stack.push(FmtOp::Node(first));
                    }
                }
                FmtOp::WhenThen(slice) => {
                    if let Some(((w, t), rest)) = slice.split_first() {
                        if !rest.is_empty() {
                            stack.push(FmtOp::WhenThen(rest));
                        }
                        stack.push(FmtOp::Node(t));
                        stack.push(FmtOp::Lit(" THEN "));
                        stack.push(FmtOp::Node(w));
                        stack.push(FmtOp::Lit(" WHEN "));
                    }
                }
                FmtOp::Node(node) => match node {
                    Self::Column { name, .. } => out.push_str(name),
                    Self::Literal(v) => out.push_str(&fmt_scalar_safe(v)),
                    Self::Unary { op, expr, .. } => match op {
                        UnaryOp::Not => {
                            stack.push(FmtOp::Lit(")"));
                            stack.push(FmtOp::Node(expr));
                            stack.push(FmtOp::Lit("NOT ("));
                        }
                        UnaryOp::Neg => {
                            stack.push(FmtOp::Lit(")"));
                            stack.push(FmtOp::Node(expr));
                            stack.push(FmtOp::Lit("-("));
                        }
                        UnaryOp::IsNull => {
                            stack.push(FmtOp::Lit(" IS NULL"));
                            stack.push(FmtOp::Node(expr));
                        }
                        UnaryOp::IsNotNull => {
                            stack.push(FmtOp::Lit(" IS NOT NULL"));
                            stack.push(FmtOp::Node(expr));
                        }
                        UnaryOp::IsTrue => {
                            stack.push(FmtOp::Lit(" IS TRUE"));
                            stack.push(FmtOp::Node(expr));
                        }
                        UnaryOp::IsFalse => {
                            stack.push(FmtOp::Lit(" IS FALSE"));
                            stack.push(FmtOp::Node(expr));
                        }
                        UnaryOp::IsNotTrue => {
                            stack.push(FmtOp::Lit(" IS NOT TRUE"));
                            stack.push(FmtOp::Node(expr));
                        }
                        UnaryOp::IsNotFalse => {
                            stack.push(FmtOp::Lit(" IS NOT FALSE"));
                            stack.push(FmtOp::Node(expr));
                        }
                    },
                    Self::Binary {
                        left, op, right, ..
                    } => {
                        stack.push(FmtOp::Lit(")"));
                        stack.push(FmtOp::Node(right));
                        stack.push(FmtOp::Lit(" "));
                        stack.push(FmtOp::Lit(op.symbol()));
                        stack.push(FmtOp::Lit(" "));
                        stack.push(FmtOp::Node(left));
                        stack.push(FmtOp::Lit("("));
                    }
                    Self::Cast { expr, target, mode } => {
                        let kw = match mode {
                            CastMode::Strict => "CAST(",
                            CastMode::Try => "TRY_CAST(",
                        };
                        stack.push(FmtOp::Lit(")"));
                        stack.push(FmtOp::Lit(target.display_name()));
                        stack.push(FmtOp::Lit(" AS "));
                        stack.push(FmtOp::Node(expr));
                        stack.push(FmtOp::Lit(kw));
                    }
                    Self::Between {
                        expr,
                        low,
                        high,
                        negated,
                    } => {
                        let op = if *negated {
                            " NOT BETWEEN "
                        } else {
                            " BETWEEN "
                        };
                        stack.push(FmtOp::Node(high));
                        stack.push(FmtOp::Lit(" AND "));
                        stack.push(FmtOp::Node(low));
                        stack.push(FmtOp::Lit(op));
                        stack.push(FmtOp::Node(expr));
                    }
                    Self::InList {
                        expr,
                        list,
                        negated,
                        redacted,
                    } => {
                        let op = if *negated { " NOT IN (" } else { " IN (" };
                        if *redacted {
                            stack.push(FmtOp::Lit(")"));
                            stack.push(FmtOp::Owned(format!("<{} redacted>", list.len())));
                            stack.push(FmtOp::Lit(op));
                            stack.push(FmtOp::Node(expr));
                        } else {
                            stack.push(FmtOp::Lit(")"));
                            stack.push(FmtOp::CommaSep(list));
                            stack.push(FmtOp::Lit(op));
                            stack.push(FmtOp::Node(expr));
                        }
                    }
                    Self::Like {
                        expr,
                        pattern,
                        escape,
                        case_insensitive,
                        negated,
                    } => {
                        let kw = match (*negated, *case_insensitive) {
                            (false, false) => " LIKE ",
                            (false, true) => " ILIKE ",
                            (true, false) => " NOT LIKE ",
                            (true, true) => " NOT ILIKE ",
                        };
                        if escape.is_some() {
                            stack.push(FmtOp::Lit(" ESCAPE <char>"));
                        }
                        stack.push(FmtOp::Node(pattern));
                        stack.push(FmtOp::Lit(kw));
                        stack.push(FmtOp::Node(expr));
                    }
                    Self::Case {
                        operand,
                        when_then,
                        else_expr,
                        ..
                    } => {
                        stack.push(FmtOp::Lit(" END"));
                        if let Some(el) = else_expr {
                            stack.push(FmtOp::Node(el));
                            stack.push(FmtOp::Lit(" ELSE "));
                        }
                        stack.push(FmtOp::WhenThen(when_then));
                        if let Some(op) = operand {
                            stack.push(FmtOp::Node(op));
                            stack.push(FmtOp::Lit(" "));
                        }
                        stack.push(FmtOp::Lit("CASE"));
                    }
                    Self::MapIndex { map, key } => {
                        stack.push(FmtOp::Lit("]"));
                        stack.push(FmtOp::Node(key));
                        stack.push(FmtOp::Lit("["));
                        stack.push(FmtOp::Node(map));
                    }
                    Self::MakeArray { elems, .. } => {
                        stack.push(FmtOp::Lit("]"));
                        stack.push(FmtOp::CommaSep(elems));
                        stack.push(FmtOp::Lit("["));
                    }
                    Self::Call { func, args, .. } => {
                        stack.push(FmtOp::Lit(")"));
                        stack.push(FmtOp::CommaSep(args));
                        stack.push(FmtOp::Lit("("));
                        stack.push(FmtOp::Lit(func.name()));
                    }
                },
            }
        }
        out
    }

    /// Collects every column index referenced anywhere in this expression into `out`.
    ///
    /// Implemented iteratively so arbitrarily deep `Expr::Binary` spines never overflow the stack.
    pub fn referenced_columns(&self, out: &mut BTreeSet<usize>) {
        let mut stack = vec![self];
        while let Some(node) = stack.pop() {
            match node {
                Self::Column { index, .. } => {
                    out.insert(*index);
                }
                Self::Literal(_) => {}
                Self::Unary { expr, .. } | Self::Cast { expr, .. } => {
                    stack.push(expr);
                }
                Self::Binary { left, right, .. } => {
                    stack.push(right);
                    stack.push(left);
                }
                Self::Between {
                    expr, low, high, ..
                } => {
                    stack.push(high);
                    stack.push(low);
                    stack.push(expr);
                }
                Self::InList { expr, list, .. } => {
                    for item in list {
                        stack.push(item);
                    }
                    stack.push(expr);
                }
                Self::Like { expr, pattern, .. } => {
                    stack.push(pattern);
                    stack.push(expr);
                }
                Self::Case {
                    operand,
                    when_then,
                    else_expr,
                    ..
                } => {
                    if let Some(el) = else_expr {
                        stack.push(el);
                    }
                    for (w, t) in when_then {
                        stack.push(t);
                        stack.push(w);
                    }
                    if let Some(op) = operand {
                        stack.push(op);
                    }
                }
                Self::MapIndex { map, key } => {
                    stack.push(key);
                    stack.push(map);
                }
                Self::MakeArray { elems, .. } => {
                    for elem in elems {
                        stack.push(elem);
                    }
                }
                Self::Call { args, .. } => {
                    for arg in args {
                        stack.push(arg);
                    }
                }
            }
        }
    }

    /// Rewrites every [`Expr::Column`] index according to `map`, preserving `name` and `data_type`.
    /// Returns [`QueryError::Internal`] if a referenced column index is out of bounds or mapped to `None`.
    ///
    /// Implemented iteratively so arbitrarily deep `Expr::Binary` spines never overflow the stack.
    pub fn remap_columns(&self, map: &[Option<usize>]) -> Result<Self, QueryError> {
        enum RemapTask<'a> {
            Visit(&'a Expr),
            BuildUnary(UnaryOp, DataType),
            BuildBinary(BinaryOp, DataType),
            BuildCast(DataType, CastMode),
            BuildBetween(bool),
            BuildInList(usize, bool, bool),
            BuildLike(Option<char>, bool, bool),
            BuildCase {
                has_operand: bool,
                when_then_len: usize,
                has_else: bool,
                data_type: DataType,
            },
            BuildMapIndex,
            BuildMakeArray(usize, DataType),
            BuildCall(ScalarFn, usize, DataType),
        }

        let mut tasks = vec![RemapTask::Visit(self)];
        let mut results: Vec<Self> = Vec::new();

        while let Some(task) = tasks.pop() {
            match task {
                RemapTask::Visit(node) => match node {
                    Self::Column {
                        index,
                        name,
                        data_type,
                    } => {
                        let new_index = map.get(*index).copied().flatten().ok_or_else(|| {
                            QueryError::internal(format!(
                                "column index {index} is not in remap table"
                            ))
                        })?;
                        results.push(Self::Column {
                            index: new_index,
                            name: name.clone(),
                            data_type: data_type.clone(),
                        });
                    }
                    Self::Literal(v) => results.push(Self::Literal(v.clone())),
                    Self::Unary {
                        op,
                        expr,
                        data_type,
                    } => {
                        tasks.push(RemapTask::BuildUnary(*op, data_type.clone()));
                        tasks.push(RemapTask::Visit(expr));
                    }
                    Self::Binary {
                        left,
                        op,
                        right,
                        data_type,
                    } => {
                        tasks.push(RemapTask::BuildBinary(*op, data_type.clone()));
                        tasks.push(RemapTask::Visit(right));
                        tasks.push(RemapTask::Visit(left));
                    }
                    Self::Cast { expr, target, mode } => {
                        tasks.push(RemapTask::BuildCast(target.clone(), *mode));
                        tasks.push(RemapTask::Visit(expr));
                    }
                    Self::Between {
                        expr,
                        low,
                        high,
                        negated,
                    } => {
                        tasks.push(RemapTask::BuildBetween(*negated));
                        tasks.push(RemapTask::Visit(high));
                        tasks.push(RemapTask::Visit(low));
                        tasks.push(RemapTask::Visit(expr));
                    }
                    Self::InList {
                        expr,
                        list,
                        negated,
                        redacted,
                    } => {
                        tasks.push(RemapTask::BuildInList(list.len(), *negated, *redacted));
                        for item in list.iter().rev() {
                            tasks.push(RemapTask::Visit(item));
                        }
                        tasks.push(RemapTask::Visit(expr));
                    }
                    Self::Like {
                        expr,
                        pattern,
                        escape,
                        case_insensitive,
                        negated,
                    } => {
                        tasks.push(RemapTask::BuildLike(*escape, *case_insensitive, *negated));
                        tasks.push(RemapTask::Visit(pattern));
                        tasks.push(RemapTask::Visit(expr));
                    }
                    Self::Case {
                        operand,
                        when_then,
                        else_expr,
                        data_type,
                    } => {
                        tasks.push(RemapTask::BuildCase {
                            has_operand: operand.is_some(),
                            when_then_len: when_then.len(),
                            has_else: else_expr.is_some(),
                            data_type: data_type.clone(),
                        });
                        if let Some(el) = else_expr {
                            tasks.push(RemapTask::Visit(el));
                        }
                        for (w, t) in when_then.iter().rev() {
                            tasks.push(RemapTask::Visit(t));
                            tasks.push(RemapTask::Visit(w));
                        }
                        if let Some(op) = operand {
                            tasks.push(RemapTask::Visit(op));
                        }
                    }
                    Self::MapIndex { map: m, key } => {
                        tasks.push(RemapTask::BuildMapIndex);
                        tasks.push(RemapTask::Visit(key));
                        tasks.push(RemapTask::Visit(m));
                    }
                    Self::MakeArray { elems, item_type } => {
                        tasks.push(RemapTask::BuildMakeArray(elems.len(), item_type.clone()));
                        for elem in elems.iter().rev() {
                            tasks.push(RemapTask::Visit(elem));
                        }
                    }
                    Self::Call {
                        func,
                        args,
                        data_type,
                    } => {
                        tasks.push(RemapTask::BuildCall(*func, args.len(), data_type.clone()));
                        for arg in args.iter().rev() {
                            tasks.push(RemapTask::Visit(arg));
                        }
                    }
                },
                RemapTask::BuildUnary(op, data_type) => {
                    let expr = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    results.push(Self::Unary {
                        op,
                        expr: Box::new(expr),
                        data_type,
                    });
                }
                RemapTask::BuildBinary(op, data_type) => {
                    let right = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let left = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    results.push(Self::Binary {
                        left: Box::new(left),
                        op,
                        right: Box::new(right),
                        data_type,
                    });
                }
                RemapTask::BuildCast(target, mode) => {
                    let expr = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    results.push(Self::Cast {
                        expr: Box::new(expr),
                        target,
                        mode,
                    });
                }
                RemapTask::BuildBetween(negated) => {
                    let high = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let low = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let expr = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    results.push(Self::Between {
                        expr: Box::new(expr),
                        low: Box::new(low),
                        high: Box::new(high),
                        negated,
                    });
                }
                RemapTask::BuildInList(len, negated, redacted) => {
                    let start = results
                        .len()
                        .checked_sub(len + 1)
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let mut drained = results.drain(start..);
                    let expr = drained
                        .next()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let list = drained.collect();
                    results.push(Self::InList {
                        expr: Box::new(expr),
                        list,
                        negated,
                        redacted,
                    });
                }
                RemapTask::BuildLike(escape, case_insensitive, negated) => {
                    let pattern = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let expr = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    results.push(Self::Like {
                        expr: Box::new(expr),
                        pattern: Box::new(pattern),
                        escape,
                        case_insensitive,
                        negated,
                    });
                }
                RemapTask::BuildCase {
                    has_operand,
                    when_then_len,
                    has_else,
                    data_type,
                } => {
                    let needed = usize::from(has_operand)
                        + when_then_len.saturating_mul(2)
                        + usize::from(has_else);
                    let start = results
                        .len()
                        .checked_sub(needed)
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let mut drained = results.drain(start..);
                    let operand = if has_operand {
                        Some(Box::new(drained.next().ok_or_else(|| {
                            QueryError::internal("remap stack underflow")
                        })?))
                    } else {
                        None
                    };
                    let mut when_then = Vec::with_capacity(when_then_len);
                    for _ in 0..when_then_len {
                        let w = drained
                            .next()
                            .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                        let t = drained
                            .next()
                            .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                        when_then.push((w, t));
                    }
                    let else_expr = if has_else {
                        Some(Box::new(drained.next().ok_or_else(|| {
                            QueryError::internal("remap stack underflow")
                        })?))
                    } else {
                        None
                    };
                    drop(drained);
                    results.push(Self::Case {
                        operand,
                        when_then,
                        else_expr,
                        data_type,
                    });
                }
                RemapTask::BuildMapIndex => {
                    let key = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let m = results
                        .pop()
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    results.push(Self::MapIndex {
                        map: Box::new(m),
                        key: Box::new(key),
                    });
                }
                RemapTask::BuildMakeArray(len, item_type) => {
                    let start = results
                        .len()
                        .checked_sub(len)
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let elems = results.drain(start..).collect();
                    results.push(Self::MakeArray { elems, item_type });
                }
                RemapTask::BuildCall(func, len, data_type) => {
                    let start = results
                        .len()
                        .checked_sub(len)
                        .ok_or_else(|| QueryError::internal("remap stack underflow"))?;
                    let args = results.drain(start..).collect();
                    results.push(Self::Call {
                        func,
                        args,
                        data_type,
                    });
                }
            }
        }

        if results.len() != 1 {
            return Err(QueryError::internal("remap stack did not produce one root"));
        }
        results
            .pop()
            .ok_or_else(|| QueryError::internal("remap stack empty"))
    }
}

fn fmt_scalar_safe(value: &ScalarValue) -> String {
    match value {
        ScalarValue::Null(_) => "NULL".to_owned(),
        ScalarValue::Bool(b) => b.to_string(),
        ScalarValue::I64(n) => n.to_string(),
        ScalarValue::F64(x) => x.to_string(),
        ScalarValue::Decimal128 {
            value,
            precision,
            scale,
        } => format!("Decimal128({value}, {precision}, {scale})"),
        ScalarValue::TimestampNs { value, utc } => format!("TimestampNs({value}, utc={utc})"),
        ScalarValue::Date(days) => format!("Date({days})"),
        ScalarValue::Interval(iv) => {
            format!("Interval({}m, {}d, {}ns)", iv.months, iv.days, iv.nanos)
        }
        ScalarValue::Utf8(s) => format!("<utf8:{}>", s.len()),
        ScalarValue::Binary(b) => format!("<binary:{}>", b.len()),
        ScalarValue::FixedBytes8(_) => "<bytes8>".to_owned(),
        ScalarValue::FixedBytes16(_) => "<bytes16>".to_owned(),
        ScalarValue::List { values, .. } => format!("<list:{}>", values.len()),
        ScalarValue::MapUtf8(m) => format!("<map:{}>", m.len()),
        ScalarValue::Events(e) => format!("<events:{}>", e.len()),
        ScalarValue::Links(l) => format!("<links:{}>", l.len()),
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.fmt_plan())
    }
}

impl fmt::Debug for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        enum DbgOp<'a> {
            Str(&'static str),
            Owned(String),
            Node(&'a Expr),
            NodeSlice(&'a [Expr]),
            WhenThen(&'a [(Expr, Expr)]),
            Debug(&'a dyn fmt::Debug),
            Bool(bool),
        }

        let mut stack = vec![DbgOp::Node(self)];
        while let Some(op) = stack.pop() {
            match op {
                DbgOp::Str(s) => f.write_str(s)?,
                DbgOp::Owned(s) => f.write_str(&s)?,
                DbgOp::Debug(d) => fmt::Debug::fmt(d, f)?,
                DbgOp::Bool(b) => fmt::Debug::fmt(&b, f)?,
                DbgOp::NodeSlice(slice) => {
                    if let Some((first, rest)) = slice.split_first() {
                        if !rest.is_empty() {
                            stack.push(DbgOp::NodeSlice(rest));
                            stack.push(DbgOp::Str(", "));
                        }
                        stack.push(DbgOp::Node(first));
                    }
                }
                DbgOp::WhenThen(slice) => {
                    if let Some(((w, t), rest)) = slice.split_first() {
                        if !rest.is_empty() {
                            stack.push(DbgOp::WhenThen(rest));
                            stack.push(DbgOp::Str(", "));
                        }
                        stack.push(DbgOp::Str(")"));
                        stack.push(DbgOp::Node(t));
                        stack.push(DbgOp::Str(", "));
                        stack.push(DbgOp::Node(w));
                        stack.push(DbgOp::Str("("));
                    }
                }
                DbgOp::Node(node) => match node {
                    Self::Column {
                        index,
                        name,
                        data_type,
                    } => f
                        .debug_struct("Column")
                        .field("index", index)
                        .field("name", name)
                        .field("data_type", data_type)
                        .finish()?,
                    Self::Literal(v) => f
                        .debug_tuple("Literal")
                        .field(&format_args!("{}", fmt_scalar_safe(v)))
                        .finish()?,
                    Self::Unary {
                        op,
                        expr,
                        data_type,
                    } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Debug(data_type));
                        stack.push(DbgOp::Str(", data_type: "));
                        stack.push(DbgOp::Node(expr));
                        stack.push(DbgOp::Str(", expr: "));
                        stack.push(DbgOp::Debug(op));
                        stack.push(DbgOp::Str("Unary { op: "));
                    }
                    Self::Binary {
                        left,
                        op,
                        right,
                        data_type,
                    } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Debug(data_type));
                        stack.push(DbgOp::Str(", data_type: "));
                        stack.push(DbgOp::Node(right));
                        stack.push(DbgOp::Str(", right: "));
                        stack.push(DbgOp::Debug(op));
                        stack.push(DbgOp::Str(", op: "));
                        stack.push(DbgOp::Node(left));
                        stack.push(DbgOp::Str("Binary { left: "));
                    }
                    Self::Cast { expr, target, mode } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Debug(mode));
                        stack.push(DbgOp::Str(", mode: "));
                        stack.push(DbgOp::Debug(target));
                        stack.push(DbgOp::Str(", target: "));
                        stack.push(DbgOp::Node(expr));
                        stack.push(DbgOp::Str("Cast { expr: "));
                    }
                    Self::Between {
                        expr,
                        low,
                        high,
                        negated,
                    } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Bool(*negated));
                        stack.push(DbgOp::Str(", negated: "));
                        stack.push(DbgOp::Node(high));
                        stack.push(DbgOp::Str(", high: "));
                        stack.push(DbgOp::Node(low));
                        stack.push(DbgOp::Str(", low: "));
                        stack.push(DbgOp::Node(expr));
                        stack.push(DbgOp::Str("Between { expr: "));
                    }
                    Self::InList {
                        expr,
                        list,
                        negated,
                        redacted,
                    } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Bool(*redacted));
                        stack.push(DbgOp::Str(", redacted: "));
                        stack.push(DbgOp::Bool(*negated));
                        stack.push(DbgOp::Str(", negated: "));
                        if *redacted {
                            stack.push(DbgOp::Owned(format!("\"<{} redacted>\"", list.len())));
                            stack.push(DbgOp::Str(", list: "));
                        } else {
                            stack.push(DbgOp::Str("]"));
                            stack.push(DbgOp::NodeSlice(list));
                            stack.push(DbgOp::Str(", list: ["));
                        }
                        stack.push(DbgOp::Node(expr));
                        stack.push(DbgOp::Str("InList { expr: "));
                    }
                    Self::Like {
                        expr,
                        pattern,
                        escape,
                        case_insensitive,
                        negated,
                    } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Bool(*negated));
                        stack.push(DbgOp::Str(", negated: "));
                        stack.push(DbgOp::Bool(*case_insensitive));
                        stack.push(DbgOp::Str(", case_insensitive: "));
                        stack.push(DbgOp::Bool(escape.is_some()));
                        stack.push(DbgOp::Str(", escape: "));
                        stack.push(DbgOp::Node(pattern));
                        stack.push(DbgOp::Str(", pattern: "));
                        stack.push(DbgOp::Node(expr));
                        stack.push(DbgOp::Str("Like { expr: "));
                    }
                    Self::Case {
                        operand,
                        when_then,
                        else_expr,
                        data_type,
                    } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Debug(data_type));
                        stack.push(DbgOp::Str(", data_type: "));
                        match else_expr {
                            Some(el) => {
                                stack.push(DbgOp::Str(")"));
                                stack.push(DbgOp::Node(el));
                                stack.push(DbgOp::Str(", else_expr: Some("));
                            }
                            None => stack.push(DbgOp::Str(", else_expr: None")),
                        }
                        stack.push(DbgOp::Str("]"));
                        stack.push(DbgOp::WhenThen(when_then));
                        stack.push(DbgOp::Str(", when_then: ["));
                        match operand {
                            Some(op) => {
                                stack.push(DbgOp::Str(")"));
                                stack.push(DbgOp::Node(op));
                                stack.push(DbgOp::Str("Case { operand: Some("));
                            }
                            None => stack.push(DbgOp::Str("Case { operand: None")),
                        }
                    }
                    Self::MapIndex { map, key } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Node(key));
                        stack.push(DbgOp::Str(", key: "));
                        stack.push(DbgOp::Node(map));
                        stack.push(DbgOp::Str("MapIndex { map: "));
                    }
                    Self::MakeArray { elems, item_type } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Debug(item_type));
                        stack.push(DbgOp::Str("], item_type: "));
                        stack.push(DbgOp::NodeSlice(elems));
                        stack.push(DbgOp::Str("MakeArray { elems: ["));
                    }
                    Self::Call {
                        func,
                        args,
                        data_type,
                    } => {
                        stack.push(DbgOp::Str(" }"));
                        stack.push(DbgOp::Debug(data_type));
                        stack.push(DbgOp::Str("], data_type: "));
                        stack.push(DbgOp::NodeSlice(args));
                        stack.push(DbgOp::Str(", args: ["));
                        stack.push(DbgOp::Debug(func));
                        stack.push(DbgOp::Str("Call { func: "));
                    }
                },
            }
        }
        Ok(())
    }
}

/// Maximum number of cached entries per sub-cache in [`EvalCache`].
pub const MAX_CACHE_ENTRIES: usize = 256;
/// Global byte cap across all 4 sub-caches in [`EvalCache`] (`4 MiB`).
pub const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;

#[derive(PartialEq, Eq)]
struct LikeCacheKey {
    pattern: String,
    escape: Option<char>,
    case_insensitive: bool,
}

trait LikeKey {
    fn as_tuple(&self) -> (&str, Option<char>, bool);
}

impl LikeKey for LikeCacheKey {
    fn as_tuple(&self) -> (&str, Option<char>, bool) {
        (&self.pattern, self.escape, self.case_insensitive)
    }
}

impl LikeKey for (&str, Option<char>, bool) {
    fn as_tuple(&self) -> (&str, Option<char>, bool) {
        *self
    }
}

impl Hash for LikeCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_tuple().hash(state);
    }
}

impl Hash for dyn LikeKey + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_tuple().hash(state);
    }
}

impl PartialEq for dyn LikeKey + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.as_tuple() == other.as_tuple()
    }
}

impl Eq for dyn LikeKey + '_ {}

impl<'a> Borrow<dyn LikeKey + 'a> for LikeCacheKey {
    fn borrow(&self) -> &(dyn LikeKey + 'a) {
        self
    }
}

#[derive(PartialEq, Eq)]
struct RegexCacheKey {
    pattern: String,
    flags: String,
}

trait RegexKey {
    fn as_pair(&self) -> (&str, &str);
}

impl RegexKey for RegexCacheKey {
    fn as_pair(&self) -> (&str, &str) {
        (&self.pattern, &self.flags)
    }
}

impl RegexKey for (&str, &str) {
    fn as_pair(&self) -> (&str, &str) {
        *self
    }
}

impl Hash for RegexCacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_pair().hash(state);
    }
}

impl Hash for dyn RegexKey + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_pair().hash(state);
    }
}

impl PartialEq for dyn RegexKey + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.as_pair() == other.as_pair()
    }
}

impl Eq for dyn RegexKey + '_ {}

impl<'a> Borrow<dyn RegexKey + 'a> for RegexCacheKey {
    fn borrow(&self) -> &(dyn RegexKey + 'a) {
        self
    }
}

struct InListCacheEntry {
    lhs_type: DataType,
    literals: Vec<ScalarValue>,
    set: Arc<(LiteralSet, bool)>,
}

#[derive(Default)]
struct InListCache {
    buckets: HashMap<u64, Vec<InListCacheEntry>>,
    entries: usize,
}

/// Thread-safe, per-query bounded cache for compiled LIKE patterns, regexes, JSONPath queries,
/// and `IN` literal sets (`list.len() >= 4`), capped at [`MAX_CACHE_ENTRIES`] per sub-cache
/// and [`MAX_CACHE_BYTES`] globally.
#[derive(Default)]
pub struct EvalCache {
    like: Mutex<HashMap<LikeCacheKey, Arc<LikeMatcher>>>,
    regex: Mutex<HashMap<RegexCacheKey, Arc<Regex>>>,
    json_path: Mutex<HashMap<String, Arc<JsonPath>>>,
    in_list: Mutex<InListCache>,
    total_bytes: AtomicUsize,
}

impl fmt::Debug for EvalCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let like_entries = self.like.lock().map_or(0, |m| m.len());
        let regex_entries = self.regex.lock().map_or(0, |m| m.len());
        let json_path_entries = self.json_path.lock().map_or(0, |m| m.len());
        let in_list_entries = self.in_list.lock().map_or(0, |m| m.entries);
        f.debug_struct("EvalCache")
            .field("like_entries", &like_entries)
            .field("regex_entries", &regex_entries)
            .field("json_path_entries", &json_path_entries)
            .field("in_list_entries", &in_list_entries)
            .field("total_bytes", &self.total_bytes.load(Ordering::Acquire))
            .finish()
    }
}

fn try_reserve_cache_bytes(counter: &AtomicUsize, bytes: usize) -> bool {
    let mut cur = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = cur.checked_add(bytes) else {
            return false;
        };
        if next > MAX_CACHE_BYTES {
            return false;
        }
        match counter.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

fn hash_scalar_canonical(h: &mut impl Hasher, sv: &ScalarValue) {
    std::mem::discriminant(sv).hash(h);
    match sv {
        ScalarValue::Null(dt) => dt.hash(h),
        ScalarValue::Bool(b) => b.hash(h),
        ScalarValue::I64(v) => v.hash(h),
        ScalarValue::F64(v) => f64_canonical_bits(*v).hash(h),
        ScalarValue::Decimal128 {
            value,
            precision,
            scale,
        } => {
            value.hash(h);
            precision.hash(h);
            scale.hash(h);
        }
        ScalarValue::TimestampNs { value, utc } => {
            value.hash(h);
            utc.hash(h);
        }
        ScalarValue::Date(d) => d.hash(h),
        ScalarValue::Interval(iv) => iv.canonical_key().hash(h),
        ScalarValue::Utf8(s) => s.hash(h),
        ScalarValue::Binary(b) => b.hash(h),
        ScalarValue::FixedBytes8(b) => b.hash(h),
        ScalarValue::FixedBytes16(b) => b.hash(h),
        ScalarValue::List { item_type, values } => {
            item_type.hash(h);
            values.len().hash(h);
            for v in values {
                hash_scalar_canonical(h, v);
            }
        }
        ScalarValue::MapUtf8(m) => m.hash(h),
        ScalarValue::Events(e) => {
            e.len().hash(h);
            for ev in e {
                ev.time.hash(h);
                ev.name.hash(h);
                ev.attributes.hash(h);
            }
        }
        ScalarValue::Links(l) => {
            l.len().hash(h);
            for lk in l {
                lk.trace_id.hash(h);
                lk.span_id.hash(h);
                lk.attributes.hash(h);
            }
        }
    }
}

fn hash_in_list(lhs_type: &DataType, list: &[Expr]) -> Option<u64> {
    let mut h = DefaultHasher::new();
    lhs_type.hash(&mut h);
    list.len().hash(&mut h);
    for item in list {
        let Expr::Literal(sv) = item else {
            return None;
        };
        hash_scalar_canonical(&mut h, sv);
    }
    Some(h.finish())
}

impl EvalCache {
    pub(crate) fn get_or_insert_like(
        &self,
        pattern: &str,
        escape: Option<char>,
        case_insensitive: bool,
        compile: impl FnOnce() -> Result<LikeMatcher, QueryError>,
    ) -> Result<Arc<LikeMatcher>, QueryError> {
        let lookup = (pattern, escape, case_insensitive);
        if let Some(hit) = self
            .like
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&lookup as &dyn LikeKey)
        {
            return Ok(Arc::clone(hit));
        }

        let compiled = Arc::new(compile()?);
        let entry_bytes = size_of::<LikeCacheKey>() + pattern.len() + compiled.estimated_bytes();

        let mut map = self.like.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = map.get(&lookup as &dyn LikeKey) {
            return Ok(Arc::clone(hit));
        }
        if map.len() < MAX_CACHE_ENTRIES && try_reserve_cache_bytes(&self.total_bytes, entry_bytes)
        {
            map.insert(
                LikeCacheKey {
                    pattern: pattern.to_owned(),
                    escape,
                    case_insensitive,
                },
                Arc::clone(&compiled),
            );
        }
        Ok(compiled)
    }

    pub(crate) fn get_or_insert_regex(
        &self,
        pat: &str,
        flags: &str,
        compile: impl FnOnce() -> Result<Regex, QueryError>,
    ) -> Result<Arc<Regex>, QueryError> {
        let lookup = (pat, flags);
        if let Some(hit) = self
            .regex
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&lookup as &dyn RegexKey)
        {
            return Ok(Arc::clone(hit));
        }

        let compiled = Arc::new(compile()?);
        let entry_bytes = size_of::<RegexCacheKey>()
            + pat.len()
            + flags.len()
            + size_of::<Regex>()
            + REGEX_ESTIMATED_BYTES;

        let mut map = self.regex.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = map.get(&lookup as &dyn RegexKey) {
            return Ok(Arc::clone(hit));
        }
        if map.len() < MAX_CACHE_ENTRIES && try_reserve_cache_bytes(&self.total_bytes, entry_bytes)
        {
            map.insert(
                RegexCacheKey {
                    pattern: pat.to_owned(),
                    flags: flags.to_owned(),
                },
                Arc::clone(&compiled),
            );
        }
        Ok(compiled)
    }

    pub(crate) fn get_or_insert_json_path(
        &self,
        path: &str,
        compile: impl FnOnce() -> Result<JsonPath, QueryError>,
    ) -> Result<Arc<JsonPath>, QueryError> {
        if let Some(hit) = self
            .json_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
        {
            return Ok(Arc::clone(hit));
        }

        let compiled = Arc::new(compile()?);
        let entry_bytes = size_of::<String>()
            + path.len()
            + size_of::<JsonPath>()
            + path.len().saturating_mul(8).max(256);

        let mut map = self
            .json_path
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(hit) = map.get(path) {
            return Ok(Arc::clone(hit));
        }
        if map.len() < MAX_CACHE_ENTRIES && try_reserve_cache_bytes(&self.total_bytes, entry_bytes)
        {
            map.insert(path.to_owned(), Arc::clone(&compiled));
        }
        Ok(compiled)
    }

    pub(crate) fn get_or_insert_in_list(
        &self,
        lhs_type: &DataType,
        list: &[Expr],
        build: impl FnOnce() -> Option<(LiteralSet, bool)>,
    ) -> Option<Arc<(LiteralSet, bool)>> {
        if list.len() < 4 {
            return build().map(Arc::new);
        }
        let hash = hash_in_list(lhs_type, list)?;
        {
            let guard = self.in_list.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(bucket) = guard.buckets.get(&hash)
                && let Some(hit) = bucket.iter().find(|entry| {
                    entry.lhs_type == *lhs_type
                        && entry.literals.len() == list.len()
                        && entry
                            .literals
                            .iter()
                            .zip(list)
                            .all(|(a, b)| matches!(b, Expr::Literal(sb) if a.semantic_eq(sb)))
                })
            {
                return Some(Arc::clone(&hit.set));
            }
        }

        let built = Arc::new(build()?);
        let literals: Vec<ScalarValue> = list
            .iter()
            .filter_map(|e| match e {
                Expr::Literal(sv) => Some(sv.clone()),
                _ => None,
            })
            .collect();
        if literals.len() != list.len() {
            return Some(built);
        }
        let literals_bytes: usize = literals
            .iter()
            .map(|sv| size_of::<ScalarValue>() + sv.heap_bytes())
            .sum();
        let entry_bytes =
            size_of::<InListCacheEntry>() + literals_bytes + built.0.estimated_bytes();

        let mut guard = self.in_list.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(bucket) = guard.buckets.get(&hash)
            && let Some(hit) = bucket.iter().find(|entry| {
                entry.lhs_type == *lhs_type
                    && entry.literals.len() == literals.len()
                    && entry
                        .literals
                        .iter()
                        .zip(&literals)
                        .all(|(a, b)| a.semantic_eq(b))
            })
        {
            return Some(Arc::clone(&hit.set));
        }
        if guard.entries < MAX_CACHE_ENTRIES
            && try_reserve_cache_bytes(&self.total_bytes, entry_bytes)
        {
            guard.entries += 1;
            guard
                .buckets
                .entry(hash)
                .or_default()
                .push(InListCacheEntry {
                    lhs_type: lhs_type.clone(),
                    literals,
                    set: Arc::clone(&built),
                });
        }
        Some(built)
    }

    #[cfg(test)]
    pub(crate) fn stats(&self) -> (usize, usize, usize, usize, usize) {
        let like_entries = self.like.lock().map_or(0, |m| m.len());
        let regex_entries = self.regex.lock().map_or(0, |m| m.len());
        let json_path_entries = self.json_path.lock().map_or(0, |m| m.len());
        let in_list_entries = self.in_list.lock().map_or(0, |m| m.entries);
        let total_bytes = self.total_bytes.load(Ordering::Acquire);
        (
            like_entries,
            regex_entries,
            json_path_entries,
            in_list_entries,
            total_bytes,
        )
    }
}

/// Evaluation runtime context passed to [`crate::eval::eval_expr`] and [`crate::eval::eval_filter`].
#[derive(Debug, Clone)]
pub struct EvalContext {
    /// Query start instant in UTC nanoseconds since Unix epoch (used by `now()`).
    pub now_ns: i64,
    /// Cooperative cancellation flag.
    pub cancel: CancelToken,
    /// Per-query bounded cache shared across worker partitions.
    pub cache: Arc<EvalCache>,
}

impl EvalContext {
    /// Creates a new [`EvalContext`] with a fresh per-query [`EvalCache`].
    pub fn new(now_ns: i64, cancel: CancelToken) -> Self {
        Self {
            now_ns,
            cancel,
            cache: Arc::new(EvalCache::default()),
        }
    }

    /// Returns a copy of `self` with `cancel`, sharing the same `now_ns` and `Arc<EvalCache>`.
    pub fn with_cancel(&self, cancel: CancelToken) -> Self {
        Self {
            now_ns: self.now_ns,
            cancel,
            cache: Arc::clone(&self.cache),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(index: usize, name: &str, data_type: DataType) -> Expr {
        Expr::Column {
            index,
            name: name.to_owned(),
            data_type,
        }
    }

    #[test]
    fn referenced_and_remap_cover_all_expr_variants() {
        let expr = Expr::Case {
            operand: Some(Box::new(col(1, "c1", DataType::I64))),
            when_then: vec![(
                Expr::Between {
                    expr: Box::new(col(2, "c2", DataType::I64)),
                    low: Box::new(Expr::Literal(ScalarValue::I64(0))),
                    high: Box::new(Expr::Unary {
                        op: UnaryOp::Neg,
                        expr: Box::new(col(3, "c3", DataType::I64)),
                        data_type: DataType::I64,
                    }),
                    negated: false,
                },
                Expr::MakeArray {
                    elems: vec![
                        Expr::Cast {
                            expr: Box::new(Expr::MapIndex {
                                map: Box::new(col(4, "c4", DataType::MapUtf8)),
                                key: Box::new(Expr::Literal(ScalarValue::Utf8("k".to_owned()))),
                            }),
                            target: DataType::Utf8,
                            mode: CastMode::Try,
                        },
                        Expr::Call {
                            func: ScalarFn::Coalesce,
                            args: vec![col(5, "c5", DataType::Utf8)],
                            data_type: DataType::Utf8,
                        },
                    ],
                    item_type: DataType::Utf8,
                },
            )],
            else_expr: Some(Box::new(Expr::Binary {
                left: Box::new(Expr::InList {
                    expr: Box::new(col(6, "c6", DataType::Utf8)),
                    list: vec![col(7, "c7", DataType::Utf8)],
                    negated: false,
                    redacted: true,
                }),
                op: BinaryOp::And,
                right: Box::new(Expr::Like {
                    expr: Box::new(col(8, "c8", DataType::Utf8)),
                    pattern: Box::new(Expr::Literal(ScalarValue::Utf8("%x%".to_owned()))),
                    escape: Some('\\'),
                    case_insensitive: true,
                    negated: false,
                }),
                data_type: DataType::Bool,
            })),
            data_type: DataType::List(Box::new(DataType::Utf8)),
        };

        let mut refs = BTreeSet::new();
        expr.referenced_columns(&mut refs);
        assert_eq!(
            refs.into_iter().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );

        let map = vec![
            None,
            Some(10),
            Some(20),
            Some(30),
            Some(40),
            Some(50),
            Some(60),
            Some(70),
            Some(80),
        ];
        let remapped = expr.remap_columns(&map).unwrap();
        let mut new_refs = BTreeSet::new();
        remapped.referenced_columns(&mut new_refs);
        assert_eq!(
            new_refs.into_iter().collect::<Vec<_>>(),
            vec![10, 20, 30, 40, 50, 60, 70, 80]
        );

        // Missing mapping returns Internal error
        let bad_map = vec![None; 9];
        assert!(matches!(
            expr.remap_columns(&bad_map),
            Err(QueryError::Internal(_))
        ));
    }

    #[test]
    fn deep_binary_spine_walkers_do_not_overflow_stack() {
        let depth = 5000usize;
        let mut expr = Expr::Binary {
            left: Box::new(Expr::Column {
                index: 0,
                name: "a".to_owned(),
                data_type: DataType::I64,
            }),
            op: BinaryOp::Add,
            right: Box::new(Expr::Column {
                index: 1,
                name: "b".to_owned(),
                data_type: DataType::I64,
            }),
            data_type: DataType::I64,
        };
        for i in 0..depth {
            let rhs = Expr::Literal(ScalarValue::I64(i as i64));
            expr = Expr::Binary {
                left: Box::new(expr),
                op: BinaryOp::Add,
                right: Box::new(rhs),
                data_type: DataType::I64,
            };
        }

        let plan_text = expr.fmt_plan();
        assert!(!plan_text.is_empty());
        let debug_text = format!("{expr:?}");
        assert!(debug_text.contains("Binary"));

        let mut refs = BTreeSet::new();
        expr.referenced_columns(&mut refs);
        assert_eq!(refs.into_iter().collect::<Vec<_>>(), vec![0, 1]);

        let remapped = expr.remap_columns(&[Some(10), Some(20)]).unwrap();
        let mut new_refs = BTreeSet::new();
        remapped.referenced_columns(&mut new_refs);
        assert_eq!(new_refs.into_iter().collect::<Vec<_>>(), vec![10, 20]);
    }

    #[test]
    fn eval_cache_charges_regex_budget_for_regex_and_ilike_entries() {
        let cache = EvalCache::default();
        for i in 0..32usize {
            let pat = format!("pat_{i}");
            let _ = cache
                .get_or_insert_regex(&pat, "", || {
                    Regex::new(&pat).map_err(|_| QueryError::failed("bad regex"))
                })
                .unwrap();
        }
        let (_, regex_entries, _, _, total_bytes) = cache.stats();
        assert!(regex_entries > 0);
        assert!(
            regex_entries <= MAX_CACHE_BYTES / REGEX_ESTIMATED_BYTES,
            "regex cache admitted {regex_entries} entries"
        );
        assert!(total_bytes <= MAX_CACHE_BYTES);

        let ilike_cache = EvalCache::default();
        for i in 0..32usize {
            let pat = format!("%needle_{i}%");
            let _ = ilike_cache
                .get_or_insert_like(&pat, None, true, || LikeMatcher::compile(&pat, None, true))
                .unwrap();
        }
        let (like_entries, _, _, _, ilike_bytes) = ilike_cache.stats();
        assert!(like_entries > 0);
        assert!(
            like_entries <= MAX_CACHE_BYTES / REGEX_ESTIMATED_BYTES,
            "ILIKE cache admitted {like_entries} entries"
        );
        assert!(ilike_bytes <= MAX_CACHE_BYTES);
    }
}
