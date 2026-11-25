//! Typed expression IR and runtime evaluation context.

use std::fmt;

use cetra_column::{CancelToken, DataType, ScalarValue};

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
    pub fn fmt_plan(&self) -> String {
        match self {
            Self::Column { name, .. } => name.clone(),
            Self::Literal(v) => fmt_scalar_safe(v),
            Self::Unary { op, expr, .. } => match op {
                UnaryOp::Not => format!("NOT ({})", expr.fmt_plan()),
                UnaryOp::Neg => format!("-({})", expr.fmt_plan()),
                UnaryOp::IsNull => format!("{} IS NULL", expr.fmt_plan()),
                UnaryOp::IsNotNull => format!("{} IS NOT NULL", expr.fmt_plan()),
                UnaryOp::IsTrue => format!("{} IS TRUE", expr.fmt_plan()),
                UnaryOp::IsFalse => format!("{} IS FALSE", expr.fmt_plan()),
                UnaryOp::IsNotTrue => format!("{} IS NOT TRUE", expr.fmt_plan()),
                UnaryOp::IsNotFalse => format!("{} IS NOT FALSE", expr.fmt_plan()),
            },
            Self::Binary {
                left, op, right, ..
            } => format!("({} {} {})", left.fmt_plan(), op.symbol(), right.fmt_plan()),
            Self::Cast { expr, target, mode } => {
                let kw = match mode {
                    CastMode::Strict => "CAST",
                    CastMode::Try => "TRY_CAST",
                };
                format!("{kw}({} AS {})", expr.fmt_plan(), target.display_name())
            }
            Self::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let op = if *negated { "NOT BETWEEN" } else { "BETWEEN" };
                format!(
                    "{} {op} {} AND {}",
                    expr.fmt_plan(),
                    low.fmt_plan(),
                    high.fmt_plan()
                )
            }
            Self::InList {
                expr,
                list,
                negated,
                redacted,
            } => {
                let op = if *negated { "NOT IN" } else { "IN" };
                if *redacted {
                    format!("{} {op} (<{} redacted>)", expr.fmt_plan(), list.len())
                } else {
                    let items: Vec<String> = list.iter().map(Self::fmt_plan).collect();
                    format!("{} {op} ({})", expr.fmt_plan(), items.join(", "))
                }
            }
            Self::Like {
                expr,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => {
                let not = if *negated { "NOT " } else { "" };
                let kw = if *case_insensitive { "ILIKE" } else { "LIKE" };
                match escape {
                    Some(_) => format!(
                        "{} {not}{kw} {} ESCAPE <char>",
                        expr.fmt_plan(),
                        pattern.fmt_plan()
                    ),
                    None => format!("{} {not}{kw} {}", expr.fmt_plan(), pattern.fmt_plan()),
                }
            }
            Self::Case {
                operand,
                when_then,
                else_expr,
                ..
            } => {
                let mut out = String::from("CASE");
                if let Some(op) = operand {
                    out.push(' ');
                    out.push_str(&op.fmt_plan());
                }
                for (w, t) in when_then {
                    out.push_str(" WHEN ");
                    out.push_str(&w.fmt_plan());
                    out.push_str(" THEN ");
                    out.push_str(&t.fmt_plan());
                }
                if let Some(el) = else_expr {
                    out.push_str(" ELSE ");
                    out.push_str(&el.fmt_plan());
                }
                out.push_str(" END");
                out
            }
            Self::MapIndex { map, key } => {
                format!("{}[{}]", map.fmt_plan(), key.fmt_plan())
            }
            Self::MakeArray { elems, .. } => {
                let items: Vec<String> = elems.iter().map(Self::fmt_plan).collect();
                format!("[{}]", items.join(", "))
            }
            Self::Call { func, args, .. } => {
                let items: Vec<String> = args.iter().map(Self::fmt_plan).collect();
                format!("{}({})", func.name(), items.join(", "))
            }
        }
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
        match self {
            Self::Column {
                index,
                name,
                data_type,
            } => f
                .debug_struct("Column")
                .field("index", index)
                .field("name", name)
                .field("data_type", data_type)
                .finish(),
            Self::Literal(v) => f.debug_tuple("Literal").field(v).finish(),
            Self::Unary {
                op,
                expr,
                data_type,
            } => f
                .debug_struct("Unary")
                .field("op", op)
                .field("expr", expr)
                .field("data_type", data_type)
                .finish(),
            Self::Binary {
                left,
                op,
                right,
                data_type,
            } => f
                .debug_struct("Binary")
                .field("left", left)
                .field("op", op)
                .field("right", right)
                .field("data_type", data_type)
                .finish(),
            Self::Cast { expr, target, mode } => f
                .debug_struct("Cast")
                .field("expr", expr)
                .field("target", target)
                .field("mode", mode)
                .finish(),
            Self::Between {
                expr,
                low,
                high,
                negated,
            } => f
                .debug_struct("Between")
                .field("expr", expr)
                .field("low", low)
                .field("high", high)
                .field("negated", negated)
                .finish(),
            Self::InList {
                expr,
                list,
                negated,
                redacted,
            } => {
                let mut ds = f.debug_struct("InList");
                ds.field("expr", expr);
                if *redacted {
                    let placeholder = format!("<{} redacted>", list.len());
                    ds.field("list", &placeholder);
                } else {
                    ds.field("list", list);
                }
                ds.field("negated", negated)
                    .field("redacted", redacted)
                    .finish()
            }
            Self::Like {
                expr,
                pattern,
                escape,
                case_insensitive,
                negated,
            } => f
                .debug_struct("Like")
                .field("expr", expr)
                .field("pattern", pattern)
                .field("escape", &escape.is_some())
                .field("case_insensitive", case_insensitive)
                .field("negated", negated)
                .finish(),
            Self::Case {
                operand,
                when_then,
                else_expr,
                data_type,
            } => f
                .debug_struct("Case")
                .field("operand", operand)
                .field("when_then", when_then)
                .field("else_expr", else_expr)
                .field("data_type", data_type)
                .finish(),
            Self::MapIndex { map, key } => f
                .debug_struct("MapIndex")
                .field("map", map)
                .field("key", key)
                .finish(),
            Self::MakeArray { elems, item_type } => f
                .debug_struct("MakeArray")
                .field("elems", elems)
                .field("item_type", item_type)
                .finish(),
            Self::Call {
                func,
                args,
                data_type,
            } => f
                .debug_struct("Call")
                .field("func", func)
                .field("args", args)
                .field("data_type", data_type)
                .finish(),
        }
    }
}

/// Evaluation runtime context passed to [`crate::eval::eval_expr`] and [`crate::eval::eval_filter`].
#[derive(Debug, Clone)]
pub struct EvalContext {
    /// Query start instant in UTC nanoseconds since Unix epoch (used by `now()`).
    pub now_ns: i64,
    /// Cooperative cancellation flag.
    pub cancel: CancelToken,
}
