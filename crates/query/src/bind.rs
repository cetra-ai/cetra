//! SQL AST binder and Filter DSL compiler (`sqlparser::ast::Expr` -> typed [`Expr`] IR).

use std::borrow::Cow;
use std::fmt;

use cetra_column::{DataType, MAX_DECIMAL_PRECISION, MAX_NESTING_DEPTH, ScalarValue, Schema};
use sqlparser::ast::{
    AccessExpr, BinaryOperator, CastKind, CeilFloorKind, DataType as SqlDataType, DateTimeField,
    ExactNumberInfo, Expr as SqlExpr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    Ident, Interval, JsonPathElem, ObjectNamePart, Spanned, Subscript, TimezoneInfo,
    TrimWhereField, TypedString, UnaryOperator, Value,
};

use crate::filter::{FilterView, filter_planning_schema, parse_filter_ast};
use crate::fn_time::{extract_return_type, parse_date_days, parse_interval, parse_timestamp_ns};
use crate::text::{DENIED_FUNCTIONS, position_of};
use crate::{BinaryOp, CastMode, Expr, InvalidQuery, Position, ScalarFn, UnaryOp};

/// Maximum AST nesting depth accepted by [`bind_expr`] (left-deep binary spines are flattened iteratively).
const MAX_BIND_DEPTH: usize = 64;

/// Compiled Filter DSL ready for evaluation over batches matching `planning_schema`.
#[derive(Clone)]
pub struct CompiledFilter {
    /// Target view.
    pub view: FilterView,
    /// Bound boolean/null expression against `planning_schema`.
    pub expr: Expr,
    /// Schema extended with any `__score_{i}_*` columns referenced by `scores`.
    pub planning_schema: Schema,
    /// True iff any column or map root resolved to `"environment"`.
    pub references_environment: bool,
    /// Distinct score names in first-appearance order (`<= MAX_SCORE_NAMES`).
    pub scores: Vec<String>,
}

impl fmt::Debug for CompiledFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompiledFilter")
            .field("view", &self.view)
            .field("expr", &self.expr)
            .field("planning_schema", &self.planning_schema)
            .field("references_environment", &self.references_environment)
            .field("scores_len", &self.scores.len())
            .finish()
    }
}

/// Binds a `sqlparser::ast::Expr` against `schema` into a typed [`Expr`] IR.
pub fn bind_expr(ast: &SqlExpr, schema: &Schema) -> Result<Expr, InvalidQuery> {
    bind_node(ast, schema, 0)
}

/// Parses, validates, and binds a Filter DSL string end-to-end against `view`'s base `schema`.
///
/// Rejects expressions whose bound `data_type()` is neither `Bool` nor `Null` with exact message
/// `"the filter must be a true/false expression"` at `Some(Position { line: 1, column: 1 })`.
pub fn compile_filter(
    view: FilterView,
    schema: &Schema,
    text: &str,
) -> Result<CompiledFilter, InvalidQuery> {
    let parsed = parse_filter_ast(view, schema, text)?;
    let planning_schema = filter_planning_schema(schema, &parsed.scores)?;
    let expr = bind_expr(&parsed.ast, &planning_schema)?;
    if !matches!(expr.data_type(), DataType::Bool | DataType::Null) {
        return Err(InvalidQuery::new(
            "the filter must be a true/false expression",
            Some(Position { line: 1, column: 1 }),
        ));
    }
    Ok(CompiledFilter {
        view,
        expr,
        planning_schema,
        references_environment: parsed.references_environment,
        scores: parsed.scores,
    })
}

fn bind_node(ast: &SqlExpr, schema: &Schema, depth: usize) -> Result<Expr, InvalidQuery> {
    let pos = at(ast);
    if depth > MAX_BIND_DEPTH {
        return Err(InvalidQuery::new(
            "the expression is nested too deeply",
            pos,
        ));
    }
    match ast {
        SqlExpr::Identifier(ident) => bind_ident(ident, schema),
        SqlExpr::CompoundIdentifier(idents) => match idents.as_slice() {
            [ident] | [_, ident] => bind_ident(ident, schema),
            _ => Err(InvalidQuery::new(
                "qualified column names with more than two parts are not supported",
                pos,
            )),
        },
        SqlExpr::Value(vws) => bind_value(&vws.value, pos),
        SqlExpr::TypedString(ts) => bind_typed_string(ts, pos),
        SqlExpr::Interval(iv) => bind_interval(iv, pos),
        SqlExpr::Nested(inner) => bind_node(inner, schema, depth + 1),
        SqlExpr::UnaryOp { op, expr } => bind_unary(*op, expr, schema, pos, depth),
        SqlExpr::BinaryOp { .. } => bind_binary_spine(ast, schema, depth),
        SqlExpr::IsNull(inner) | SqlExpr::IsUnknown(inner) => {
            let e = bind_node(inner, schema, depth + 1)?;
            Ok(Expr::Unary {
                op: UnaryOp::IsNull,
                expr: Box::new(e),
                data_type: DataType::Bool,
            })
        }
        SqlExpr::IsNotNull(inner) | SqlExpr::IsNotUnknown(inner) => {
            let e = bind_node(inner, schema, depth + 1)?;
            Ok(Expr::Unary {
                op: UnaryOp::IsNotNull,
                expr: Box::new(e),
                data_type: DataType::Bool,
            })
        }
        SqlExpr::IsTrue(inner) => bind_is_bool(inner, UnaryOp::IsTrue, schema, pos, depth),
        SqlExpr::IsFalse(inner) => bind_is_bool(inner, UnaryOp::IsFalse, schema, pos, depth),
        SqlExpr::IsNotTrue(inner) => bind_is_bool(inner, UnaryOp::IsNotTrue, schema, pos, depth),
        SqlExpr::IsNotFalse(inner) => bind_is_bool(inner, UnaryOp::IsNotFalse, schema, pos, depth),
        SqlExpr::IsDistinctFrom(left, right) => {
            bind_distinct(left, right, BinaryOp::IsDistinctFrom, schema, pos, depth)
        }
        SqlExpr::IsNotDistinctFrom(left, right) => {
            bind_distinct(left, right, BinaryOp::IsNotDistinctFrom, schema, pos, depth)
        }
        SqlExpr::Between {
            expr,
            negated,
            low,
            high,
        } => bind_between(expr, low, high, *negated, schema, pos, depth),
        SqlExpr::InList {
            expr,
            list,
            negated,
        } => bind_in_list(expr, list, *negated, schema, pos, depth),
        SqlExpr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => bind_like(
            expr,
            pattern,
            escape_char.as_ref().map(|v| &v.value),
            false,
            *negated,
            *any,
            schema,
            pos,
            depth,
        ),
        SqlExpr::ILike {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => bind_like(
            expr,
            pattern,
            escape_char.as_ref().map(|v| &v.value),
            true,
            *negated,
            *any,
            schema,
            pos,
            depth,
        ),
        SqlExpr::Cast {
            kind,
            expr,
            data_type,
            array,
            format,
        } => {
            if *array {
                return Err(InvalidQuery::new("array casts are not supported", pos));
            }
            if format.is_some() {
                return Err(InvalidQuery::new("cast formats are not supported", pos));
            }
            let mode = match kind {
                CastKind::Cast | CastKind::DoubleColon => CastMode::Strict,
                CastKind::TryCast | CastKind::SafeCast => CastMode::Try,
            };
            let target = bind_sql_type(data_type, pos)?;
            let inner = bind_node(expr, schema, depth + 1)?;
            let src_dt = inner.data_type();
            if !can_cast(&src_dt, &target) {
                return Err(InvalidQuery::new(
                    format!(
                        "cannot cast `{}` to `{}`",
                        src_dt.display_name(),
                        target.display_name()
                    ),
                    pos,
                ));
            }
            Ok(Expr::Cast {
                expr: Box::new(inner),
                target,
                mode,
            })
        }
        SqlExpr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => bind_case(
            operand.as_deref(),
            conditions,
            else_result.as_deref(),
            schema,
            pos,
            depth,
        ),
        SqlExpr::Array(arr) => bind_array(&arr.elem, schema, pos, depth),
        SqlExpr::CompoundFieldAccess { root, access_chain } => {
            bind_compound_access(root, access_chain, schema, pos, depth)
        }
        SqlExpr::JsonAccess { value, path } => {
            bind_json_access(value, &path.path, schema, pos, depth)
        }
        SqlExpr::Ceil { expr, field } => {
            if !matches!(
                field,
                CeilFloorKind::DateTimeField(DateTimeField::NoDateTime)
            ) {
                return Err(InvalidQuery::new(
                    "CEIL with unit or scale is not supported",
                    pos,
                ));
            }
            let arg = bind_node(expr, schema, depth + 1)?;
            bind_floor_ceil(ScalarFn::Ceil, arg, pos)
        }
        SqlExpr::Floor { expr, field } => {
            if !matches!(
                field,
                CeilFloorKind::DateTimeField(DateTimeField::NoDateTime)
            ) {
                return Err(InvalidQuery::new(
                    "FLOOR with unit or scale is not supported",
                    pos,
                ));
            }
            let arg = bind_node(expr, schema, depth + 1)?;
            bind_floor_ceil(ScalarFn::Floor, arg, pos)
        }
        SqlExpr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            let s = coerce_to_utf8_from_utf8_or_null(bind_node(expr, schema, depth + 1)?, pos)?;
            let Some(from) = substring_from else {
                return Err(InvalidQuery::new(
                    "substring requires a start position",
                    pos,
                ));
            };
            let start = coerce_to_i64_from_i64_or_null(bind_node(from, schema, depth + 1)?, pos)?;
            let mut args = vec![s, start];
            if let Some(len_expr) = substring_for {
                args.push(coerce_to_i64_from_i64_or_null(
                    bind_node(len_expr, schema, depth + 1)?,
                    pos,
                )?);
            }
            Ok(Expr::Call {
                func: ScalarFn::Substr,
                args,
                data_type: DataType::Utf8,
            })
        }
        SqlExpr::Trim {
            expr,
            trim_where,
            trim_what,
            trim_characters,
        } => {
            let func = match trim_where {
                None | Some(TrimWhereField::Both) => ScalarFn::Trim,
                Some(TrimWhereField::Leading) => ScalarFn::Ltrim,
                Some(TrimWhereField::Trailing) => ScalarFn::Rtrim,
            };
            let s = coerce_to_utf8_from_utf8_or_null(bind_node(expr, schema, depth + 1)?, pos)?;
            let mut args = vec![s];
            if let Some(what) = trim_what {
                args.push(coerce_to_utf8_from_utf8_or_null(
                    bind_node(what, schema, depth + 1)?,
                    pos,
                )?);
            } else if let Some(chars) = trim_characters {
                match chars.as_slice() {
                    [] => {}
                    [ch] => args.push(coerce_to_utf8_from_utf8_or_null(
                        bind_node(ch, schema, depth + 1)?,
                        pos,
                    )?),
                    _ => {
                        return Err(InvalidQuery::new(
                            "TRIM accepts at most one characters argument",
                            pos,
                        ));
                    }
                }
            }
            Ok(Expr::Call {
                func,
                args,
                data_type: DataType::Utf8,
            })
        }
        SqlExpr::Position { expr: sub, r#in: s } => {
            let hay = coerce_to_utf8_from_utf8_or_null(bind_node(s, schema, depth + 1)?, pos)?;
            let needle = coerce_to_utf8_from_utf8_or_null(bind_node(sub, schema, depth + 1)?, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::Strpos,
                args: vec![hay, needle],
                data_type: DataType::I64,
            })
        }
        SqlExpr::Extract { field, expr, .. } => {
            let src = bind_node(expr, schema, depth + 1)?;
            bind_extract(field.to_string(), src, pos)
        }
        SqlExpr::Function(func) => bind_function(func, schema, pos, depth),
        _ => Err(InvalidQuery::new("unsupported SQL expression", pos)),
    }
}

fn bind_ident(ident: &Ident, schema: &Schema) -> Result<Expr, InvalidQuery> {
    let pos = at_ident(ident);
    let idx = if ident.quote_style.is_some() {
        schema.index_of(&ident.value)
    } else {
        let lower = ident.value.to_ascii_lowercase();
        schema.index_of(&lower).or_else(|| {
            schema
                .fields()
                .iter()
                .position(|f| f.name.eq_ignore_ascii_case(&ident.value))
        })
    };
    let Some(index) = idx else {
        let display = if ident.quote_style.is_some() {
            ident.value.clone()
        } else {
            ident.value.to_ascii_lowercase()
        };
        return Err(InvalidQuery::new(
            format!("unknown column `{display}`"),
            pos,
        ));
    };
    let field = &schema.fields()[index];
    Ok(Expr::Column {
        index,
        name: field.name.clone(),
        data_type: field.data_type.clone(),
    })
}

fn bind_value(value: &Value, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    match value {
        Value::Number(s, _) => {
            if let Ok(i) = s.parse::<i64>() {
                return Ok(Expr::Literal(ScalarValue::I64(i)));
            }
            if let Ok(f) = s.parse::<f64>()
                && f.is_finite()
            {
                return Ok(Expr::Literal(ScalarValue::F64(f)));
            }
            Err(InvalidQuery::new(
                format!("invalid numeric literal `{s}`"),
                pos,
            ))
        }
        Value::SingleQuotedString(s) => Ok(Expr::Literal(ScalarValue::Utf8(s.clone()))),
        Value::Boolean(b) => Ok(Expr::Literal(ScalarValue::Bool(*b))),
        Value::Null => Ok(Expr::Literal(ScalarValue::Null(DataType::Null))),
        Value::HexStringLiteral(h) => {
            let bytes =
                hex::decode(h).map_err(|_| InvalidQuery::new("invalid hex string literal", pos))?;
            Ok(Expr::Literal(ScalarValue::Binary(bytes)))
        }
        Value::DoubleQuotedString(_) => Err(InvalidQuery::new(
            "double-quoted strings are not allowed",
            pos,
        )),
        Value::Placeholder(_) => Err(InvalidQuery::new("placeholders are not allowed", pos)),
        Value::DollarQuotedString(_) => Err(InvalidQuery::new(
            "dollar-quoted strings are not allowed",
            pos,
        )),
        _ => Err(InvalidQuery::new("unsupported literal", pos)),
    }
}

fn bind_typed_string(ts: &TypedString, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let text = match &ts.value.value {
        Value::SingleQuotedString(s) => s.as_str(),
        _ => {
            return Err(InvalidQuery::new(
                "typed literal value must be a single-quoted string",
                pos,
            ));
        }
    };
    match &ts.data_type {
        SqlDataType::Timestamp(None | Some(9), tz) => {
            let utc = !matches!(tz, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone);
            let ns =
                parse_timestamp_ns(text, utc).map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
            Ok(Expr::Literal(ScalarValue::TimestampNs { value: ns, utc }))
        }
        SqlDataType::Date => {
            let days = parse_date_days(text).map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
            Ok(Expr::Literal(ScalarValue::Date(days)))
        }
        other => {
            let target = bind_sql_type(other, pos)?;
            if !can_cast(&DataType::Utf8, &target) {
                return Err(InvalidQuery::new(
                    format!("unsupported typed literal `{other}`"),
                    pos,
                ));
            }
            Ok(Expr::Cast {
                expr: Box::new(Expr::Literal(ScalarValue::Utf8(text.to_owned()))),
                target,
                mode: CastMode::Strict,
            })
        }
    }
}

fn bind_interval(iv: &Interval, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    if iv.last_field.is_some() {
        return Err(InvalidQuery::new(
            "compound interval qualifiers (TO ...) are not supported",
            pos,
        ));
    }
    let raw: Cow<'_, str> = match iv.value.as_ref() {
        SqlExpr::Value(vws) => match &vws.value {
            Value::SingleQuotedString(s) | Value::Number(s, _) => Cow::Borrowed(s.as_str()),
            _ => {
                return Err(InvalidQuery::new(
                    "interval values must be string literals",
                    pos,
                ));
            }
        },
        SqlExpr::UnaryOp { op, expr }
            if matches!(op, UnaryOperator::Minus | UnaryOperator::Plus) =>
        {
            match expr.as_ref() {
                SqlExpr::Value(vws) => match &vws.value {
                    Value::Number(s, _) => {
                        if matches!(op, UnaryOperator::Minus) {
                            Cow::Owned(format!("-{s}"))
                        } else {
                            Cow::Borrowed(s.as_str())
                        }
                    }
                    _ => {
                        return Err(InvalidQuery::new(
                            "interval values must be string literals",
                            pos,
                        ));
                    }
                },
                _ => {
                    return Err(InvalidQuery::new(
                        "interval values must be string literals",
                        pos,
                    ));
                }
            }
        }
        _ => {
            return Err(InvalidQuery::new(
                "interval values must be string literals",
                pos,
            ));
        }
    };
    let parsed = match &iv.leading_field {
        Some(field) => parse_interval(&format!("{raw} {field}")),
        None => parse_interval(&raw),
    }
    .map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
    Ok(Expr::Literal(ScalarValue::Interval(parsed)))
}

fn bind_unary(
    op: UnaryOperator,
    inner_ast: &SqlExpr,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    if matches!(op, UnaryOperator::Minus)
        && let SqlExpr::Value(vws) = inner_ast
        && let Value::Number(s, _) = &vws.value
        && s == "9223372036854775808"
    {
        return Ok(Expr::Literal(ScalarValue::I64(i64::MIN)));
    }
    let inner = bind_node(inner_ast, schema, depth + 1)?;
    let dt = inner.data_type();
    match op {
        UnaryOperator::Not => {
            if !matches!(dt, DataType::Bool | DataType::Null) {
                return Err(InvalidQuery::new(
                    format!("NOT requires boolean operand, got `{}`", dt.display_name()),
                    pos,
                ));
            }
            Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(inner),
                data_type: DataType::Bool,
            })
        }
        UnaryOperator::Minus => {
            if !matches!(
                dt,
                DataType::I64
                    | DataType::F64
                    | DataType::Decimal128 { .. }
                    | DataType::Interval
                    | DataType::Null
            ) {
                return Err(InvalidQuery::new(
                    format!(
                        "unary `-` requires numeric or interval operand, got `{}`",
                        dt.display_name()
                    ),
                    pos,
                ));
            }
            Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(inner),
                data_type: dt,
            })
        }
        UnaryOperator::Plus => {
            if !matches!(
                dt,
                DataType::I64
                    | DataType::F64
                    | DataType::Decimal128 { .. }
                    | DataType::Interval
                    | DataType::Null
            ) {
                return Err(InvalidQuery::new(
                    format!(
                        "unary `+` requires numeric or interval operand, got `{}`",
                        dt.display_name()
                    ),
                    pos,
                ));
            }
            Ok(inner)
        }
        _ => Err(InvalidQuery::new(
            format!("unsupported unary operator `{op}`"),
            pos,
        )),
    }
}

fn bind_is_bool(
    inner_ast: &SqlExpr,
    op: UnaryOp,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let inner = bind_node(inner_ast, schema, depth + 1)?;
    let dt = inner.data_type();
    if !matches!(dt, DataType::Bool | DataType::Null) {
        return Err(InvalidQuery::new(
            format!("expected boolean operand, got `{}`", dt.display_name()),
            pos,
        ));
    }
    Ok(Expr::Unary {
        op,
        expr: Box::new(inner),
        data_type: DataType::Bool,
    })
}

fn bind_distinct(
    left_ast: &SqlExpr,
    right_ast: &SqlExpr,
    op: BinaryOp,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let l = bind_node(left_ast, schema, depth + 1)?;
    let r = bind_node(right_ast, schema, depth + 1)?;
    let common = coerce_comparison_pair(&l, &r, pos)?;
    let left = coerce_expr(l, &common, pos)?;
    let right = coerce_expr(r, &common, pos)?;
    Ok(Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
        data_type: DataType::Bool,
    })
}

fn bind_binary_spine(ast: &SqlExpr, schema: &Schema, depth: usize) -> Result<Expr, InvalidQuery> {
    let mut spine: Vec<(&BinaryOperator, &SqlExpr, Option<Position>)> = Vec::new();
    let mut cur = ast;
    while let SqlExpr::BinaryOp { left, op, right } = cur {
        spine.push((op, right.as_ref(), at(cur)));
        cur = left.as_ref();
    }
    let mut acc = bind_node(cur, schema, depth + 1)?;
    for (op, right_ast, pos) in spine.into_iter().rev() {
        let r = bind_node(right_ast, schema, depth + 1)?;
        acc = bind_binary_op(acc, op, r, pos)?;
    }
    Ok(acc)
}

fn bind_binary_op(
    l: Expr,
    op: &BinaryOperator,
    r: Expr,
    pos: Option<Position>,
) -> Result<Expr, InvalidQuery> {
    match op {
        BinaryOperator::And | BinaryOperator::Or => {
            let bin_op = if matches!(op, BinaryOperator::And) {
                BinaryOp::And
            } else {
                BinaryOp::Or
            };
            let left = expect_bool_or_null(l, pos)?;
            let right = expect_bool_or_null(r, pos)?;
            Ok(Expr::Binary {
                left: Box::new(left),
                op: bin_op,
                right: Box::new(right),
                data_type: DataType::Bool,
            })
        }
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq => {
            let bin_op = match op {
                BinaryOperator::Eq => BinaryOp::Eq,
                BinaryOperator::NotEq => BinaryOp::NotEq,
                BinaryOperator::Lt => BinaryOp::Lt,
                BinaryOperator::LtEq => BinaryOp::LtEq,
                BinaryOperator::Gt => BinaryOp::Gt,
                BinaryOperator::GtEq => BinaryOp::GtEq,
                _ => BinaryOp::Eq,
            };
            let common = coerce_comparison_pair(&l, &r, pos)?;
            let left = coerce_expr(l, &common, pos)?;
            let right = coerce_expr(r, &common, pos)?;
            Ok(Expr::Binary {
                left: Box::new(left),
                op: bin_op,
                right: Box::new(right),
                data_type: DataType::Bool,
            })
        }
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo => bind_arith_binary(l, op, r, pos),
        BinaryOperator::StringConcat => bind_concat_op(l, r, pos),
        BinaryOperator::AtArrow => bind_at_arrow(l, r, pos),
        BinaryOperator::Arrow => bind_json_arrow(l, r, ScalarFn::JsonGet, pos),
        BinaryOperator::LongArrow => bind_json_arrow(l, r, ScalarFn::JsonGetText, pos),
        _ => Err(InvalidQuery::new(
            format!("unsupported binary operator `{op}`"),
            pos,
        )),
    }
}

fn bind_arith_binary(
    l: Expr,
    op: &BinaryOperator,
    r: Expr,
    pos: Option<Position>,
) -> Result<Expr, InvalidQuery> {
    let bin_op = match op {
        BinaryOperator::Plus => BinaryOp::Add,
        BinaryOperator::Minus => BinaryOp::Sub,
        BinaryOperator::Multiply => BinaryOp::Mul,
        BinaryOperator::Divide => BinaryOp::Div,
        BinaryOperator::Modulo => BinaryOp::Mod,
        _ => {
            return Err(InvalidQuery::new(
                format!("unsupported arithmetic operator `{op}`"),
                pos,
            ));
        }
    };
    let ldt = l.data_type();
    let rdt = r.data_type();
    match (&ldt, &rdt, bin_op) {
        (DataType::TimestampNs { utc }, DataType::Interval, BinaryOp::Add | BinaryOp::Sub) => {
            return Ok(Expr::Binary {
                left: Box::new(l),
                op: bin_op,
                right: Box::new(r),
                data_type: DataType::TimestampNs { utc: *utc },
            });
        }
        (DataType::Interval, DataType::TimestampNs { utc }, BinaryOp::Add) => {
            return Ok(Expr::Binary {
                left: Box::new(l),
                op: bin_op,
                right: Box::new(r),
                data_type: DataType::TimestampNs { utc: *utc },
            });
        }
        (DataType::TimestampNs { utc }, DataType::Null, BinaryOp::Add | BinaryOp::Sub) => {
            let right = coerce_expr(r, &DataType::Interval, pos)?;
            return Ok(Expr::Binary {
                left: Box::new(l),
                op: bin_op,
                right: Box::new(right),
                data_type: DataType::TimestampNs { utc: *utc },
            });
        }
        (DataType::Null, DataType::TimestampNs { utc }, BinaryOp::Add) => {
            let left = coerce_expr(l, &DataType::Interval, pos)?;
            return Ok(Expr::Binary {
                left: Box::new(left),
                op: bin_op,
                right: Box::new(r),
                data_type: DataType::TimestampNs { utc: *utc },
            });
        }
        (DataType::Interval, DataType::TimestampNs { .. }, BinaryOp::Sub) => {
            return Err(InvalidQuery::new(
                "cannot subtract timestamp from interval",
                pos,
            ));
        }
        _ => {}
    }

    let common = coerce_numeric_pair(&ldt, &rdt, pos)?;
    let left = coerce_expr(l, &common, pos)?;
    let right = coerce_expr(r, &common, pos)?;
    Ok(Expr::Binary {
        left: Box::new(left),
        op: bin_op,
        right: Box::new(right),
        data_type: common,
    })
}

fn bind_concat_op(l: Expr, r: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let ldt = l.data_type();
    let rdt = r.data_type();
    if matches!(ldt, DataType::List(_)) || matches!(rdt, DataType::List(_)) {
        let common = coerce_list_pair(&l, &r, pos)?;
        let left = coerce_expr(l, &common, pos)?;
        let right = coerce_expr(r, &common, pos)?;
        return Ok(Expr::Binary {
            left: Box::new(left),
            op: BinaryOp::Concat,
            right: Box::new(right),
            data_type: common,
        });
    }
    let left = coerce_to_utf8_general(l, pos)?;
    let right = coerce_to_utf8_general(r, pos)?;
    Ok(Expr::Binary {
        left: Box::new(left),
        op: BinaryOp::Concat,
        right: Box::new(right),
        data_type: DataType::Utf8,
    })
}

fn bind_at_arrow(l: Expr, r: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let ldt = l.data_type();
    let rdt = r.data_type();
    if !matches!(ldt, DataType::List(_) | DataType::Null)
        || !matches!(rdt, DataType::List(_) | DataType::Null)
    {
        return Err(InvalidQuery::new(
            "operator `@>` requires list operands",
            pos,
        ));
    }
    let common = coerce_list_pair(&l, &r, pos)?;
    let left = coerce_expr(l, &common, pos)?;
    let right = coerce_expr(r, &common, pos)?;
    Ok(Expr::Binary {
        left: Box::new(left),
        op: BinaryOp::ArrayContains,
        right: Box::new(right),
        data_type: DataType::Bool,
    })
}

fn bind_json_arrow(
    l: Expr,
    r: Expr,
    func: ScalarFn,
    pos: Option<Position>,
) -> Result<Expr, InvalidQuery> {
    let left = coerce_to_utf8_from_utf8_or_null(l, pos)?;
    let right = match r.data_type() {
        DataType::Utf8 | DataType::I64 => r,
        DataType::Null => coerce_expr(r, &DataType::Utf8, pos)?,
        other => {
            return Err(InvalidQuery::new(
                format!(
                    "JSON lookup key must be string or integer, got `{}`",
                    other.display_name()
                ),
                pos,
            ));
        }
    };
    Ok(Expr::Call {
        func,
        args: vec![left, right],
        data_type: DataType::Utf8,
    })
}

fn bind_between(
    expr_ast: &SqlExpr,
    low_ast: &SqlExpr,
    high_ast: &SqlExpr,
    negated: bool,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let e = bind_node(expr_ast, schema, depth + 1)?;
    let l = bind_node(low_ast, schema, depth + 1)?;
    let h = bind_node(high_ast, schema, depth + 1)?;
    let common = coerce_comparison_many(&[&e, &l, &h], pos)?;
    let expr = Box::new(coerce_expr(e, &common, pos)?);
    let low = Box::new(coerce_expr(l, &common, pos)?);
    let high = Box::new(coerce_expr(h, &common, pos)?);
    Ok(Expr::Between {
        expr,
        low,
        high,
        negated,
    })
}

fn bind_in_list(
    expr_ast: &SqlExpr,
    list_asts: &[SqlExpr],
    negated: bool,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let e = bind_node(expr_ast, schema, depth + 1)?;
    if list_asts.is_empty() {
        return Ok(Expr::InList {
            expr: Box::new(e),
            list: Vec::new(),
            negated,
            redacted: false,
        });
    }
    let mut items = Vec::with_capacity(list_asts.len());
    for item_ast in list_asts {
        items.push(bind_node(item_ast, schema, depth + 1)?);
    }
    let mut all_refs: Vec<&Expr> = Vec::with_capacity(items.len() + 1);
    all_refs.push(&e);
    for it in &items {
        all_refs.push(it);
    }
    let common = coerce_comparison_many(&all_refs, pos)?;
    let expr = Box::new(coerce_expr(e, &common, pos)?);
    let mut coerced_list = Vec::with_capacity(items.len());
    for it in items {
        coerced_list.push(coerce_expr(it, &common, pos)?);
    }
    Ok(Expr::InList {
        expr,
        list: coerced_list,
        negated,
        redacted: false,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal LIKE/ILIKE binder passes AST parts directly"
)]
fn bind_like(
    expr_ast: &SqlExpr,
    pat_ast: &SqlExpr,
    escape_val: Option<&Value>,
    case_insensitive: bool,
    negated: bool,
    any: bool,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    if any {
        return Err(InvalidQuery::new(
            "LIKE ANY / ILIKE ANY is not supported",
            pos,
        ));
    }
    let expr = coerce_to_utf8_from_utf8_or_null(bind_node(expr_ast, schema, depth + 1)?, pos)?;
    let pattern = coerce_to_utf8_from_utf8_or_null(bind_node(pat_ast, schema, depth + 1)?, pos)?;
    let escape = match escape_val {
        None => Some('\\'),
        Some(Value::SingleQuotedString(s)) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (None, _) => None,
                (Some(c), None) => Some(c),
                (Some(_), Some(_)) => {
                    return Err(InvalidQuery::new(
                        "ESCAPE must be a single character or empty string",
                        pos,
                    ));
                }
            }
        }
        Some(_) => {
            return Err(InvalidQuery::new("ESCAPE must be a string literal", pos));
        }
    };
    Ok(Expr::Like {
        expr: Box::new(expr),
        pattern: Box::new(pattern),
        escape,
        case_insensitive,
        negated,
    })
}

fn bind_case(
    operand_ast: Option<&SqlExpr>,
    conditions: &[sqlparser::ast::CaseWhen],
    else_ast: Option<&SqlExpr>,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let op_bound = match operand_ast {
        Some(op) => Some(bind_node(op, schema, depth + 1)?),
        None => None,
    };
    let mut whens = Vec::with_capacity(conditions.len());
    let mut thens = Vec::with_capacity(conditions.len());
    for cw in conditions {
        whens.push(bind_node(&cw.condition, schema, depth + 1)?);
        thens.push(bind_node(&cw.result, schema, depth + 1)?);
    }
    let else_bound = match else_ast {
        Some(el) => Some(bind_node(el, schema, depth + 1)?),
        None => None,
    };

    let (operand, coerced_whens) = match op_bound {
        None => {
            let mut cw = Vec::with_capacity(whens.len());
            for w in whens {
                cw.push(expect_bool_or_null(w, pos)?);
            }
            (None, cw)
        }
        Some(op_expr) => {
            let mut cmp_refs: Vec<&Expr> = Vec::with_capacity(whens.len() + 1);
            cmp_refs.push(&op_expr);
            for w in &whens {
                cmp_refs.push(w);
            }
            let cmp_dt = coerce_comparison_many(&cmp_refs, pos)?;
            let op_coerced = coerce_expr(op_expr, &cmp_dt, pos)?;
            let mut cw = Vec::with_capacity(whens.len());
            for w in whens {
                cw.push(coerce_expr(w, &cmp_dt, pos)?);
            }
            (Some(Box::new(op_coerced)), cw)
        }
    };

    let mut result_refs: Vec<&Expr> = Vec::with_capacity(thens.len() + 1);
    for t in &thens {
        result_refs.push(t);
    }
    if let Some(el) = &else_bound {
        result_refs.push(el);
    }
    let res_dt = coerce_common_many(&result_refs, pos)?;
    let mut when_then = Vec::with_capacity(coerced_whens.len());
    for (w, t) in coerced_whens.into_iter().zip(thens) {
        when_then.push((w, coerce_expr(t, &res_dt, pos)?));
    }
    let else_expr = match else_bound {
        Some(el) => Some(Box::new(coerce_expr(el, &res_dt, pos)?)),
        None => None,
    };
    Ok(Expr::Case {
        operand,
        when_then,
        else_expr,
        data_type: res_dt,
    })
}

fn bind_array(
    elems_ast: &[SqlExpr],
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let mut bound = Vec::with_capacity(elems_ast.len());
    for el in elems_ast {
        bound.push(bind_node(el, schema, depth + 1)?);
    }
    let refs: Vec<&Expr> = bound.iter().collect();
    let item_type = if refs.is_empty() {
        DataType::Null
    } else {
        coerce_common_many(&refs, pos)?
    };
    let mut coerced = Vec::with_capacity(bound.len());
    for el in bound {
        coerced.push(coerce_expr(el, &item_type, pos)?);
    }

    let all_literals = coerced.iter().all(|e| matches!(e, Expr::Literal(_)));
    if all_literals {
        let values: Vec<ScalarValue> = coerced
            .into_iter()
            .filter_map(|e| match e {
                Expr::Literal(sv) => Some(match sv {
                    ScalarValue::Null(_) => ScalarValue::Null(item_type.clone()),
                    other => other,
                }),
                _ => None,
            })
            .collect();
        let list_val = ScalarValue::List { item_type, values };
        list_val
            .validate(MAX_NESTING_DEPTH)
            .map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
        Ok(Expr::Literal(list_val))
    } else {
        let list_dt = DataType::List(Box::new(item_type.clone()));
        list_dt
            .validate(MAX_NESTING_DEPTH)
            .map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
        Ok(Expr::MakeArray {
            elems: coerced,
            item_type,
        })
    }
}

fn bind_compound_access(
    root: &SqlExpr,
    access_chain: &[AccessExpr],
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let (mut cur, subscripts) = if matches!(root, SqlExpr::Identifier(_))
        && let Some(AccessExpr::Dot(SqlExpr::Identifier(col))) = access_chain.first()
    {
        (bind_ident(col, schema)?, &access_chain[1..])
    } else {
        (bind_node(root, schema, depth + 1)?, access_chain)
    };
    for access in subscripts {
        match access {
            AccessExpr::Subscript(Subscript::Index { index }) => {
                let idx_expr = bind_node(index, schema, depth + 1)?;
                match cur.data_type() {
                    DataType::MapUtf8 => {
                        let key = coerce_to_utf8_from_utf8_or_null(idx_expr, pos)?;
                        cur = Expr::MapIndex {
                            map: Box::new(cur),
                            key: Box::new(key),
                        };
                    }
                    DataType::List(inner) => {
                        let idx = coerce_to_i64_from_i64_or_null(idx_expr, pos)?;
                        cur = Expr::Call {
                            func: ScalarFn::ArrayElement,
                            args: vec![cur, idx],
                            data_type: *inner,
                        };
                    }
                    other => {
                        return Err(InvalidQuery::new(
                            format!("`{}` cannot be subscripted", other.display_name()),
                            pos,
                        ));
                    }
                }
            }
            _ => {
                return Err(InvalidQuery::new(
                    "only single-index subscript access is supported",
                    pos,
                ));
            }
        }
    }
    Ok(cur)
}

fn bind_json_access(
    value: &SqlExpr,
    path: &[JsonPathElem],
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    let mut cur = coerce_to_utf8_from_utf8_or_null(bind_node(value, schema, depth + 1)?, pos)?;
    for elem in path {
        let key_expr = match elem {
            JsonPathElem::Dot { key, .. } => Expr::Literal(ScalarValue::Utf8(key.clone())),
            JsonPathElem::Bracket { key } | JsonPathElem::ColonBracket { key } => {
                let bound_key = bind_node(key, schema, depth + 1)?;
                match bound_key.data_type() {
                    DataType::Utf8 | DataType::I64 => bound_key,
                    DataType::Null => coerce_expr(bound_key, &DataType::Utf8, pos)?,
                    other => {
                        return Err(InvalidQuery::new(
                            format!(
                                "JSON path subscript must be string or integer, got `{}`",
                                other.display_name()
                            ),
                            pos,
                        ));
                    }
                }
            }
        };
        cur = Expr::Call {
            func: ScalarFn::JsonGet,
            args: vec![cur, key_expr],
            data_type: DataType::Utf8,
        };
    }
    Ok(cur)
}

fn bind_floor_ceil(func: ScalarFn, arg: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let coerced = match arg.data_type() {
        DataType::F64 | DataType::I64 | DataType::Null => arg,
        DataType::Decimal128 { .. } => coerce_expr(arg, &DataType::F64, pos)?,
        other => {
            return Err(InvalidQuery::new(
                format!(
                    "{}() requires numeric argument, got `{}`",
                    func.name(),
                    other.display_name()
                ),
                pos,
            ));
        }
    };
    let data_type = coerced.data_type();
    Ok(Expr::Call {
        func,
        args: vec![coerced],
        data_type,
    })
}

fn bind_function(
    function: &Function,
    schema: &Schema,
    pos: Option<Position>,
    depth: usize,
) -> Result<Expr, InvalidQuery> {
    validate_plain_function_call(function, pos)?;
    let raw_name = match function.name.0.as_slice() {
        [ObjectNamePart::Identifier(ident)] => ident.value.to_ascii_lowercase(),
        _ => {
            return Err(InvalidQuery::new(
                "qualified function names are not supported",
                pos,
            ));
        }
    };
    if DENIED_FUNCTIONS.contains(&raw_name.as_str()) {
        return Err(InvalidQuery::new(
            format!("function `{raw_name}` is not available"),
            pos,
        ));
    }

    let FunctionArguments::List(arg_list) = &function.args else {
        return Err(InvalidQuery::new("function calls require parentheses", pos));
    };
    let mut args = Vec::with_capacity(arg_list.args.len());
    for arg in &arg_list.args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else {
            return Err(InvalidQuery::new(
                "only positional expression arguments are supported",
                pos,
            ));
        };
        args.push(bind_node(expr, schema, depth + 1)?);
    }

    let canonical = match raw_name.as_str() {
        "char_length" | "character_length" => "length",
        "substring" => "substr",
        "position" => "strpos",
        "date_part" => "extract",
        "ceiling" => "ceil",
        "pow" => "power",
        "array_contains" => "array_has",
        other => other,
    };

    bind_scalar_fn(canonical, args, pos)
}

fn validate_plain_function_call(
    function: &Function,
    pos: Option<Position>,
) -> Result<(), InvalidQuery> {
    if function.uses_odbc_syntax
        || function.over.is_some()
        || function.filter.is_some()
        || !function.within_group.is_empty()
        || function.null_treatment.is_some()
    {
        return Err(InvalidQuery::new(
            "only plain scalar function calls are supported",
            pos,
        ));
    }
    let parameters_empty = match &function.parameters {
        FunctionArguments::None => true,
        FunctionArguments::List(l) => l.args.is_empty(),
        FunctionArguments::Subquery(_) => false,
    };
    if !parameters_empty {
        return Err(InvalidQuery::new(
            "function parameters are not supported",
            pos,
        ));
    }
    let list = match &function.args {
        FunctionArguments::List(list) => list,
        FunctionArguments::None => {
            return Err(InvalidQuery::new("function calls require parentheses", pos));
        }
        FunctionArguments::Subquery(_) => {
            return Err(InvalidQuery::new(
                "subquery function arguments are not supported",
                pos,
            ));
        }
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(InvalidQuery::new(
            "function modifiers and clauses are not supported",
            pos,
        ));
    }
    for arg in &list.args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(_)) => {}
            FunctionArg::Unnamed(_) => {
                return Err(InvalidQuery::new(
                    "wildcard arguments are not supported",
                    pos,
                ));
            }
            FunctionArg::Named { .. } | FunctionArg::ExprNamed { .. } => {
                return Err(InvalidQuery::new("named arguments are not supported", pos));
            }
        }
    }
    Ok(())
}

fn bind_scalar_fn(
    name: &str,
    mut args: Vec<Expr>,
    pos: Option<Position>,
) -> Result<Expr, InvalidQuery> {
    match name {
        // --- String & binary ---
        "lower" | "upper" => {
            let func = if name == "lower" {
                ScalarFn::Lower
            } else {
                ScalarFn::Upper
            };
            let [s] = take_one(args, name, pos)?;
            let s = coerce_to_utf8_from_utf8_or_null(s, pos)?;
            Ok(Expr::Call {
                func,
                args: vec![s],
                data_type: DataType::Utf8,
            })
        }
        "length" => {
            let [s] = take_one(args, name, pos)?;
            let s = coerce_to_utf8_from_utf8_or_null(s, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::Length,
                args: vec![s],
                data_type: DataType::I64,
            })
        }
        "substr" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(InvalidQuery::new("substr() takes 2 or 3 arguments", pos));
            }
            let s = coerce_to_utf8_from_utf8_or_null(args.remove(0), pos)?;
            let start = coerce_to_i64_from_i64_or_null(args.remove(0), pos)?;
            let mut out = vec![s, start];
            if let Some(count) = args.pop() {
                out.push(coerce_to_i64_from_i64_or_null(count, pos)?);
            }
            Ok(Expr::Call {
                func: ScalarFn::Substr,
                args: out,
                data_type: DataType::Utf8,
            })
        }
        "trim" | "ltrim" | "rtrim" => {
            let func = match name {
                "trim" => ScalarFn::Trim,
                "ltrim" => ScalarFn::Ltrim,
                _ => ScalarFn::Rtrim,
            };
            if args.is_empty() || args.len() > 2 {
                return Err(InvalidQuery::new(
                    format!("{name}() takes 1 or 2 arguments"),
                    pos,
                ));
            }
            let s = coerce_to_utf8_from_utf8_or_null(args.remove(0), pos)?;
            let mut out = vec![s];
            if let Some(chars) = args.pop() {
                out.push(coerce_to_utf8_from_utf8_or_null(chars, pos)?);
            }
            Ok(Expr::Call {
                func,
                args: out,
                data_type: DataType::Utf8,
            })
        }
        "left" | "right" => {
            let func = if name == "left" {
                ScalarFn::Left
            } else {
                ScalarFn::Right
            };
            let [s, n] = take_two(args, name, pos)?;
            let s = coerce_to_utf8_from_utf8_or_null(s, pos)?;
            let n = coerce_to_i64_from_i64_or_null(n, pos)?;
            Ok(Expr::Call {
                func,
                args: vec![s, n],
                data_type: DataType::Utf8,
            })
        }
        "split_part" => {
            let [s, delim, n] = take_three(args, name, pos)?;
            let s = coerce_to_utf8_from_utf8_or_null(s, pos)?;
            let delim = coerce_to_utf8_from_utf8_or_null(delim, pos)?;
            let n = coerce_to_i64_from_i64_or_null(n, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::SplitPart,
                args: vec![s, delim, n],
                data_type: DataType::Utf8,
            })
        }
        "concat" => {
            if args.is_empty() {
                return Err(InvalidQuery::new(
                    "concat() requires at least 1 argument",
                    pos,
                ));
            }
            let coerced: Vec<Expr> = args
                .into_iter()
                .map(|a| coerce_to_utf8_general(a, pos))
                .collect::<Result<_, _>>()?;
            Ok(Expr::Call {
                func: ScalarFn::Concat,
                args: coerced,
                data_type: DataType::Utf8,
            })
        }
        "concat_ws" => {
            if args.len() < 2 {
                return Err(InvalidQuery::new(
                    "concat_ws() requires at least 2 arguments",
                    pos,
                ));
            }
            let mut coerced = Vec::with_capacity(args.len());
            for (idx, a) in args.into_iter().enumerate() {
                if idx == 0 {
                    coerced.push(coerce_to_utf8_from_utf8_or_null(a, pos)?);
                } else {
                    coerced.push(coerce_to_utf8_general(a, pos)?);
                }
            }
            Ok(Expr::Call {
                func: ScalarFn::ConcatWs,
                args: coerced,
                data_type: DataType::Utf8,
            })
        }
        "starts_with" | "ends_with" | "contains" => {
            let func = match name {
                "starts_with" => ScalarFn::StartsWith,
                "ends_with" => ScalarFn::EndsWith,
                _ => ScalarFn::Contains,
            };
            let [s, sub] = take_two(args, name, pos)?;
            let s = coerce_to_utf8_from_utf8_or_null(s, pos)?;
            let sub = coerce_to_utf8_from_utf8_or_null(sub, pos)?;
            Ok(Expr::Call {
                func,
                args: vec![s, sub],
                data_type: DataType::Bool,
            })
        }
        "strpos" => {
            let [s, sub] = take_two(args, name, pos)?;
            let s = coerce_to_utf8_from_utf8_or_null(s, pos)?;
            let sub = coerce_to_utf8_from_utf8_or_null(sub, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::Strpos,
                args: vec![s, sub],
                data_type: DataType::I64,
            })
        }
        "regexp_like" | "regexp_extract" => {
            let (func, ret) = if name == "regexp_like" {
                (ScalarFn::RegexpLike, DataType::Bool)
            } else {
                (ScalarFn::RegexpExtract, DataType::Utf8)
            };
            if args.len() < 2 || args.len() > 3 {
                return Err(InvalidQuery::new(
                    format!("{name}() takes 2 or 3 arguments"),
                    pos,
                ));
            }
            let s = coerce_to_utf8_from_utf8_or_null(args.remove(0), pos)?;
            let pat = coerce_to_utf8_from_utf8_or_null(args.remove(0), pos)?;
            let mut out = vec![s, pat];
            if let Some(flags) = args.pop() {
                out.push(coerce_to_utf8_from_utf8_or_null(flags, pos)?);
            }
            Ok(Expr::Call {
                func,
                args: out,
                data_type: ret,
            })
        }
        "encode" => {
            let [val, enc] = take_two(args, name, pos)?;
            if !matches!(
                val.data_type(),
                DataType::Binary
                    | DataType::FixedBytes8
                    | DataType::FixedBytes16
                    | DataType::Utf8
                    | DataType::Null
            ) {
                return Err(InvalidQuery::new(
                    format!(
                        "encode() requires binary or string input, got `{}`",
                        val.data_type().display_name()
                    ),
                    pos,
                ));
            }
            let enc = coerce_to_utf8_from_utf8_or_null(enc, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::Encode,
                args: vec![val, enc],
                data_type: DataType::Utf8,
            })
        }

        // --- Time & interval ---
        "now" => {
            if !args.is_empty() {
                return Err(InvalidQuery::new("now() takes no arguments", pos));
            }
            Ok(Expr::Call {
                func: ScalarFn::Now,
                args: Vec::new(),
                data_type: DataType::TimestampNs { utc: true },
            })
        }
        "date_trunc" => {
            let [part, ts] = take_two(args, name, pos)?;
            let part = coerce_to_utf8_from_utf8_or_null(part, pos)?;
            let ts = coerce_to_timestamp_arg(ts, pos)?;
            let dt = ts.data_type();
            Ok(Expr::Call {
                func: ScalarFn::DateTrunc,
                args: vec![part, ts],
                data_type: dt,
            })
        }
        "date_bin" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(InvalidQuery::new("date_bin() takes 2 or 3 arguments", pos));
            }
            let stride = coerce_to_interval_or_null(args.remove(0), pos)?;
            let src = coerce_to_timestamp_arg(args.remove(0), pos)?;
            let src_dt = src.data_type();
            let mut out = vec![stride, src];
            if let Some(orig) = args.pop() {
                out.push(coerce_to_timestamp_arg(orig, pos)?);
            }
            Ok(Expr::Call {
                func: ScalarFn::DateBin,
                args: out,
                data_type: src_dt,
            })
        }
        "extract" => {
            let [field, src] = take_two(args, name, pos)?;
            let Expr::Literal(ScalarValue::Utf8(field_str)) = field else {
                return Err(InvalidQuery::new(
                    "date_part/extract field must be a string literal",
                    pos,
                ));
            };
            bind_extract(field_str, src, pos)
        }
        "to_timestamp" => {
            let [val] = take_one(args, name, pos)?;
            if !matches!(
                val.data_type(),
                DataType::I64
                    | DataType::F64
                    | DataType::Utf8
                    | DataType::TimestampNs { .. }
                    | DataType::Null
            ) {
                return Err(InvalidQuery::new(
                    format!(
                        "to_timestamp() unsupported type `{}`",
                        val.data_type().display_name()
                    ),
                    pos,
                ));
            }
            Ok(Expr::Call {
                func: ScalarFn::ToTimestamp,
                args: vec![val],
                data_type: DataType::TimestampNs { utc: true },
            })
        }

        // --- Math & null handling ---
        "abs" => {
            let [x] = take_one(args, name, pos)?;
            let dt = x.data_type();
            if !matches!(
                dt,
                DataType::I64 | DataType::F64 | DataType::Decimal128 { .. } | DataType::Null
            ) {
                return Err(InvalidQuery::new(
                    format!(
                        "abs() requires numeric argument, got `{}`",
                        dt.display_name()
                    ),
                    pos,
                ));
            }
            Ok(Expr::Call {
                func: ScalarFn::Abs,
                args: vec![x],
                data_type: dt,
            })
        }
        "round" => {
            if args.is_empty() || args.len() > 2 {
                return Err(InvalidQuery::new("round() takes 1 or 2 arguments", pos));
            }
            let raw_x = args.remove(0);
            let x = match raw_x.data_type() {
                DataType::F64 | DataType::I64 | DataType::Null => raw_x,
                DataType::Decimal128 { .. } => coerce_expr(raw_x, &DataType::F64, pos)?,
                other => {
                    return Err(InvalidQuery::new(
                        format!(
                            "round() requires numeric argument, got `{}`",
                            other.display_name()
                        ),
                        pos,
                    ));
                }
            };
            let ret_dt = x.data_type();
            let mut out = vec![x];
            if let Some(dp) = args.pop() {
                out.push(coerce_to_i64_from_i64_or_null(dp, pos)?);
            }
            Ok(Expr::Call {
                func: ScalarFn::Round,
                args: out,
                data_type: ret_dt,
            })
        }
        "floor" => {
            let [x] = take_one(args, name, pos)?;
            bind_floor_ceil(ScalarFn::Floor, x, pos)
        }
        "ceil" => {
            let [x] = take_one(args, name, pos)?;
            bind_floor_ceil(ScalarFn::Ceil, x, pos)
        }
        "sqrt" | "ln" | "log10" => {
            let func = match name {
                "sqrt" => ScalarFn::Sqrt,
                "ln" => ScalarFn::Ln,
                _ => ScalarFn::Log10,
            };
            let [x] = take_one(args, name, pos)?;
            let x = coerce_to_f64_numeric(x, name, pos)?;
            Ok(Expr::Call {
                func,
                args: vec![x],
                data_type: DataType::F64,
            })
        }
        "power" => {
            let [base, exp] = take_two(args, name, pos)?;
            let base = coerce_to_f64_numeric(base, name, pos)?;
            let exp = coerce_to_f64_numeric(exp, name, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::Power,
                args: vec![base, exp],
                data_type: DataType::F64,
            })
        }
        "isnan" | "isfinite" => {
            let func = if name == "isnan" {
                ScalarFn::IsNan
            } else {
                ScalarFn::IsFinite
            };
            let [x] = take_one(args, name, pos)?;
            let x = coerce_to_f64_numeric(x, name, pos)?;
            Ok(Expr::Call {
                func,
                args: vec![x],
                data_type: DataType::Bool,
            })
        }
        "coalesce" => {
            if args.is_empty() {
                return Err(InvalidQuery::new(
                    "coalesce() requires at least 1 argument",
                    pos,
                ));
            }
            let refs: Vec<&Expr> = args.iter().collect();
            let common = coerce_common_many(&refs, pos)?;
            let coerced: Vec<Expr> = args
                .into_iter()
                .map(|a| coerce_expr(a, &common, pos))
                .collect::<Result<_, _>>()?;
            Ok(Expr::Call {
                func: ScalarFn::Coalesce,
                args: coerced,
                data_type: common,
            })
        }
        "nullif" => {
            let [a, b] = take_two(args, name, pos)?;
            let common = coerce_comparison_pair(&a, &b, pos)?;
            let a = coerce_expr(a, &common, pos)?;
            let b = coerce_expr(b, &common, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::Nullif,
                args: vec![a, b],
                data_type: common,
            })
        }
        "greatest" | "least" => {
            let func = if name == "greatest" {
                ScalarFn::Greatest
            } else {
                ScalarFn::Least
            };
            if args.is_empty() {
                return Err(InvalidQuery::new(
                    format!("{name}() requires at least 1 argument"),
                    pos,
                ));
            }
            let refs: Vec<&Expr> = args.iter().collect();
            let common = coerce_comparison_many(&refs, pos)?;
            let coerced: Vec<Expr> = args
                .into_iter()
                .map(|a| coerce_expr(a, &common, pos))
                .collect::<Result<_, _>>()?;
            Ok(Expr::Call {
                func,
                args: coerced,
                data_type: common,
            })
        }

        // --- Array & map ---
        "array_has" => {
            let [arr, elem] = take_two(args, name, pos)?;
            let (arr_dt, elem_dt) = match (arr.data_type(), elem.data_type()) {
                (DataType::List(it), edt) => {
                    let item_common = coerce_common_types(&it, &edt, pos)?;
                    (DataType::List(Box::new(item_common.clone())), item_common)
                }
                (DataType::Null, edt) => (DataType::List(Box::new(edt.clone())), edt),
                (other, _) => {
                    return Err(InvalidQuery::new(
                        format!(
                            "array_has() requires list argument, got `{}`",
                            other.display_name()
                        ),
                        pos,
                    ));
                }
            };
            let arr = coerce_expr(arr, &arr_dt, pos)?;
            let elem = coerce_expr(elem, &elem_dt, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::ArrayHas,
                args: vec![arr, elem],
                data_type: DataType::Bool,
            })
        }
        "array_has_all" => {
            let [arr, sub] = take_two(args, name, pos)?;
            if !matches!(arr.data_type(), DataType::List(_) | DataType::Null)
                || !matches!(sub.data_type(), DataType::List(_) | DataType::Null)
            {
                return Err(InvalidQuery::new(
                    "array_has_all() requires list arguments",
                    pos,
                ));
            }
            let common = coerce_list_pair(&arr, &sub, pos)?;
            let arr = coerce_expr(arr, &common, pos)?;
            let sub = coerce_expr(sub, &common, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::ArrayHasAll,
                args: vec![arr, sub],
                data_type: DataType::Bool,
            })
        }
        "array_length" => {
            if args.is_empty() || args.len() > 2 {
                return Err(InvalidQuery::new(
                    "array_length() takes 1 or 2 arguments",
                    pos,
                ));
            }
            let arr = args.remove(0);
            if !matches!(arr.data_type(), DataType::List(_) | DataType::Null) {
                return Err(InvalidQuery::new(
                    format!(
                        "array_length() requires list argument, got `{}`",
                        arr.data_type().display_name()
                    ),
                    pos,
                ));
            }
            let mut out = vec![arr];
            if let Some(dim) = args.pop() {
                out.push(coerce_to_i64_from_i64_or_null(dim, pos)?);
            }
            Ok(Expr::Call {
                func: ScalarFn::ArrayLength,
                args: out,
                data_type: DataType::I64,
            })
        }
        "array_element" => {
            let [arr, idx] = take_two(args, name, pos)?;
            let item_dt = match arr.data_type() {
                DataType::List(it) => *it,
                DataType::Null => DataType::Null,
                other => {
                    return Err(InvalidQuery::new(
                        format!(
                            "array_element() requires list argument, got `{}`",
                            other.display_name()
                        ),
                        pos,
                    ));
                }
            };
            let idx = coerce_to_i64_from_i64_or_null(idx, pos)?;
            Ok(Expr::Call {
                func: ScalarFn::ArrayElement,
                args: vec![arr, idx],
                data_type: item_dt,
            })
        }
        "array_to_string" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(InvalidQuery::new(
                    "array_to_string() takes 2 or 3 arguments",
                    pos,
                ));
            }
            let arr = args.remove(0);
            if !matches!(arr.data_type(), DataType::List(_) | DataType::Null) {
                return Err(InvalidQuery::new(
                    format!(
                        "array_to_string() requires list argument, got `{}`",
                        arr.data_type().display_name()
                    ),
                    pos,
                ));
            }
            let delim = coerce_to_utf8_from_utf8_or_null(args.remove(0), pos)?;
            let mut out = vec![arr, delim];
            if let Some(null_str) = args.pop() {
                out.push(coerce_to_utf8_from_utf8_or_null(null_str, pos)?);
            }
            Ok(Expr::Call {
                func: ScalarFn::ArrayToString,
                args: out,
                data_type: DataType::Utf8,
            })
        }
        "array_distinct" => {
            let [arr] = take_one(args, name, pos)?;
            let dt = arr.data_type();
            if !matches!(dt, DataType::List(_) | DataType::Null) {
                return Err(InvalidQuery::new(
                    format!(
                        "array_distinct() requires list argument, got `{}`",
                        dt.display_name()
                    ),
                    pos,
                ));
            }
            Ok(Expr::Call {
                func: ScalarFn::ArrayDistinct,
                args: vec![arr],
                data_type: dt,
            })
        }
        "map_keys" | "map_values" => {
            let func = if name == "map_keys" {
                ScalarFn::MapKeys
            } else {
                ScalarFn::MapValues
            };
            let [m] = take_one(args, name, pos)?;
            if !matches!(m.data_type(), DataType::MapUtf8 | DataType::Null) {
                return Err(InvalidQuery::new(
                    format!(
                        "{name}() requires map argument, got `{}`",
                        m.data_type().display_name()
                    ),
                    pos,
                ));
            }
            Ok(Expr::Call {
                func,
                args: vec![m],
                data_type: DataType::List(Box::new(DataType::Utf8)),
            })
        }

        // --- JSON ---
        "json_get" | "json_get_text" => {
            let func = if name == "json_get" {
                ScalarFn::JsonGet
            } else {
                ScalarFn::JsonGetText
            };
            let [json, key] = take_two(args, name, pos)?;
            bind_json_arrow(json, key, func, pos)
        }
        "json_extract" | "json_extract_text" => {
            let func = if name == "json_extract" {
                ScalarFn::JsonExtract
            } else {
                ScalarFn::JsonExtractText
            };
            let [json, path] = take_two(args, name, pos)?;
            let json = coerce_to_utf8_from_utf8_or_null(json, pos)?;
            let path = coerce_to_utf8_from_utf8_or_null(path, pos)?;
            Ok(Expr::Call {
                func,
                args: vec![json, path],
                data_type: DataType::Utf8,
            })
        }

        _ => Err(InvalidQuery::new(format!("unknown function `{name}`"), pos)),
    }
}

fn take_one(args: Vec<Expr>, name: &str, pos: Option<Position>) -> Result<[Expr; 1], InvalidQuery> {
    <[Expr; 1]>::try_from(args)
        .map_err(|v| InvalidQuery::new(format!("{name}() takes 1 argument, got {}", v.len()), pos))
}

fn take_two(args: Vec<Expr>, name: &str, pos: Option<Position>) -> Result<[Expr; 2], InvalidQuery> {
    <[Expr; 2]>::try_from(args)
        .map_err(|v| InvalidQuery::new(format!("{name}() takes 2 arguments, got {}", v.len()), pos))
}

fn take_three(
    args: Vec<Expr>,
    name: &str,
    pos: Option<Position>,
) -> Result<[Expr; 3], InvalidQuery> {
    <[Expr; 3]>::try_from(args)
        .map_err(|v| InvalidQuery::new(format!("{name}() takes 3 arguments, got {}", v.len()), pos))
}

fn bind_extract(field_str: String, src: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let ret_dt =
        extract_return_type(&field_str).map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
    let src = if matches!(src.data_type(), DataType::Utf8) {
        coerce_to_timestamp_arg(src, pos)?
    } else {
        src
    };
    validate_extract_source(&field_str, &src, pos)?;
    Ok(Expr::Call {
        func: ScalarFn::Extract,
        args: vec![Expr::Literal(ScalarValue::Utf8(field_str)), src],
        data_type: ret_dt,
    })
}

fn validate_extract_source(
    field_str: &str,
    src: &Expr,
    pos: Option<Position>,
) -> Result<(), InvalidQuery> {
    let dt = src.data_type();
    if !matches!(
        dt,
        DataType::TimestampNs { .. } | DataType::Date | DataType::Interval | DataType::Null
    ) {
        return Err(InvalidQuery::new(
            format!(
                "extract() requires timestamp, date, or interval, got `{}`",
                dt.display_name()
            ),
            pos,
        ));
    }
    if matches!(dt, DataType::Interval) {
        let trimmed = field_str.trim_ascii();
        let stripped = trimmed
            .strip_prefix(|c| c == '\'' || c == '"')
            .and_then(|s| s.strip_suffix(|c| c == '\'' || c == '"'))
            .unwrap_or(trimmed);
        let lower = stripped.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "qtr"
                | "quarter"
                | "quarters"
                | "isoyear"
                | "dow"
                | "dayofweek"
                | "isodow"
                | "doy"
                | "dayofyear"
        ) {
            return Err(InvalidQuery::new(
                format!("extract({field_str}) is not supported on interval"),
                pos,
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Type coercion helpers
// ---------------------------------------------------------------------------

fn expect_bool_or_null(expr: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let dt = expr.data_type();
    if !matches!(dt, DataType::Bool | DataType::Null) {
        return Err(InvalidQuery::new(
            format!("expected boolean expression, got `{}`", dt.display_name()),
            pos,
        ));
    }
    coerce_expr(expr, &DataType::Bool, pos)
}

fn coerce_to_utf8_from_utf8_or_null(
    expr: Expr,
    pos: Option<Position>,
) -> Result<Expr, InvalidQuery> {
    match expr.data_type() {
        DataType::Utf8 => Ok(expr),
        DataType::Null => coerce_expr(expr, &DataType::Utf8, pos),
        other => Err(InvalidQuery::new(
            format!("expected string argument, got `{}`", other.display_name()),
            pos,
        )),
    }
}

fn coerce_to_utf8_general(expr: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let dt = expr.data_type();
    if dt == DataType::Utf8 {
        return Ok(expr);
    }
    if can_cast(&dt, &DataType::Utf8) {
        coerce_expr(expr, &DataType::Utf8, pos)
    } else {
        Err(InvalidQuery::new(
            format!("cannot coerce `{}` to string", dt.display_name()),
            pos,
        ))
    }
}

fn coerce_to_i64_from_i64_or_null(expr: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    match expr.data_type() {
        DataType::I64 => Ok(expr),
        DataType::Null => coerce_expr(expr, &DataType::I64, pos),
        other => Err(InvalidQuery::new(
            format!("expected integer argument, got `{}`", other.display_name()),
            pos,
        )),
    }
}

fn coerce_to_f64_numeric(
    expr: Expr,
    fn_name: &str,
    pos: Option<Position>,
) -> Result<Expr, InvalidQuery> {
    match expr.data_type() {
        DataType::F64 => Ok(expr),
        DataType::I64 | DataType::Decimal128 { .. } | DataType::Null => {
            coerce_expr(expr, &DataType::F64, pos)
        }
        other => Err(InvalidQuery::new(
            format!(
                "{fn_name}() requires numeric argument, got `{}`",
                other.display_name()
            ),
            pos,
        )),
    }
}

fn coerce_to_timestamp_arg(expr: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    match expr.data_type() {
        DataType::TimestampNs { .. } => Ok(expr),
        DataType::Utf8 | DataType::Date | DataType::Null => {
            coerce_expr(expr, &DataType::TimestampNs { utc: true }, pos)
        }
        other => Err(InvalidQuery::new(
            format!(
                "expected timestamp argument, got `{}`",
                other.display_name()
            ),
            pos,
        )),
    }
}

fn coerce_to_interval_or_null(expr: Expr, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    match expr.data_type() {
        DataType::Interval => Ok(expr),
        DataType::Null => coerce_expr(expr, &DataType::Interval, pos),
        other => Err(InvalidQuery::new(
            format!("expected interval argument, got `{}`", other.display_name()),
            pos,
        )),
    }
}

fn is_comparable_type(dt: &DataType) -> bool {
    match dt {
        DataType::MapUtf8 | DataType::Events | DataType::Links => false,
        DataType::List(inner) => is_comparable_type(inner),
        _ => true,
    }
}

fn coerce_comparison_pair(
    l: &Expr,
    r: &Expr,
    pos: Option<Position>,
) -> Result<DataType, InvalidQuery> {
    coerce_comparison_many(&[l, r], pos)
}

fn coerce_comparison_many(
    exprs: &[&Expr],
    pos: Option<Position>,
) -> Result<DataType, InvalidQuery> {
    let dt = coerce_common_many(exprs, pos)?;
    if !is_comparable_type(&dt) {
        return Err(InvalidQuery::new(
            format!("type `{}` cannot be compared", dt.display_name()),
            pos,
        ));
    }
    Ok(dt)
}

fn coerce_common_many(exprs: &[&Expr], pos: Option<Position>) -> Result<DataType, InvalidQuery> {
    let Some(first) = exprs.first() else {
        return Ok(DataType::Null);
    };
    let mut cur = first.data_type();
    for ex in &exprs[1..] {
        cur = coerce_common_types(&cur, &ex.data_type(), pos)?;
    }
    for ex in exprs {
        if !can_coerce_to(ex, &cur) {
            return Err(InvalidQuery::new(
                format!(
                    "incompatible types `{}` and `{}`",
                    ex.data_type().display_name(),
                    cur.display_name()
                ),
                pos,
            ));
        }
    }
    Ok(cur)
}

fn coerce_common_types(
    a: &DataType,
    b: &DataType,
    pos: Option<Position>,
) -> Result<DataType, InvalidQuery> {
    if a == b {
        return Ok(a.clone());
    }
    match (a, b) {
        (DataType::Null, t) | (t, DataType::Null) => Ok(t.clone()),
        (DataType::List(la), DataType::List(lb)) => {
            let item = coerce_common_types(la, lb, pos)?;
            Ok(DataType::List(Box::new(item)))
        }
        (DataType::I64, DataType::F64) | (DataType::F64, DataType::I64) => Ok(DataType::F64),
        (DataType::Decimal128 { precision, scale }, DataType::I64)
        | (DataType::I64, DataType::Decimal128 { precision, scale }) => Ok(DataType::Decimal128 {
            precision: *precision,
            scale: *scale,
        }),
        (DataType::Decimal128 { .. }, DataType::F64)
        | (DataType::F64, DataType::Decimal128 { .. }) => Ok(DataType::F64),
        (
            DataType::Decimal128 {
                precision: p1,
                scale: s1,
            },
            DataType::Decimal128 {
                precision: p2,
                scale: s2,
            },
        ) => Ok(DataType::Decimal128 {
            precision: (*p1).max(*p2).min(MAX_DECIMAL_PRECISION),
            scale: (*s1).max(*s2),
        }),
        (DataType::TimestampNs { utc: u1 }, DataType::TimestampNs { utc: u2 }) => {
            Ok(DataType::TimestampNs { utc: *u1 || *u2 })
        }
        (DataType::TimestampNs { utc }, DataType::Utf8)
        | (DataType::Utf8, DataType::TimestampNs { utc }) => {
            Ok(DataType::TimestampNs { utc: *utc })
        }
        (DataType::Date, DataType::Utf8) | (DataType::Utf8, DataType::Date) => Ok(DataType::Date),
        (DataType::Date, DataType::TimestampNs { utc })
        | (DataType::TimestampNs { utc }, DataType::Date) => {
            Ok(DataType::TimestampNs { utc: *utc })
        }
        (DataType::Binary, DataType::FixedBytes8 | DataType::FixedBytes16)
        | (DataType::FixedBytes8 | DataType::FixedBytes16, DataType::Binary) => {
            Ok(DataType::Binary)
        }
        _ => Err(InvalidQuery::new(
            format!(
                "incompatible types `{}` and `{}`",
                a.display_name(),
                b.display_name()
            ),
            pos,
        )),
    }
}

fn coerce_numeric_pair(
    a: &DataType,
    b: &DataType,
    pos: Option<Position>,
) -> Result<DataType, InvalidQuery> {
    match (a, b) {
        (DataType::Null, DataType::Null) => Ok(DataType::I64),
        (DataType::Null, t) | (t, DataType::Null)
            if matches!(
                t,
                DataType::I64 | DataType::F64 | DataType::Decimal128 { .. }
            ) =>
        {
            Ok(t.clone())
        }
        (DataType::I64, DataType::I64) => Ok(DataType::I64),
        (DataType::F64, DataType::F64)
        | (DataType::I64, DataType::F64)
        | (DataType::F64, DataType::I64) => Ok(DataType::F64),
        (DataType::Decimal128 { precision, scale }, DataType::I64)
        | (DataType::I64, DataType::Decimal128 { precision, scale }) => Ok(DataType::Decimal128 {
            precision: *precision,
            scale: *scale,
        }),
        (DataType::Decimal128 { .. }, DataType::F64)
        | (DataType::F64, DataType::Decimal128 { .. }) => Ok(DataType::F64),
        (
            DataType::Decimal128 {
                precision: p1,
                scale: s1,
            },
            DataType::Decimal128 {
                precision: p2,
                scale: s2,
            },
        ) => Ok(DataType::Decimal128 {
            precision: (*p1).max(*p2).min(MAX_DECIMAL_PRECISION),
            scale: (*s1).max(*s2),
        }),
        _ => Err(InvalidQuery::new(
            format!(
                "arithmetic requires numeric operands, got `{}` and `{}`",
                a.display_name(),
                b.display_name()
            ),
            pos,
        )),
    }
}

fn coerce_list_pair(a: &Expr, b: &Expr, pos: Option<Position>) -> Result<DataType, InvalidQuery> {
    let adt = a.data_type();
    let bdt = b.data_type();
    let out = match (&adt, &bdt) {
        (DataType::Null, DataType::Null) => DataType::List(Box::new(DataType::Null)),
        (DataType::List(la), DataType::Null) => DataType::List(la.clone()),
        (DataType::Null, DataType::List(lb)) => DataType::List(lb.clone()),
        (DataType::List(la), DataType::List(lb)) => {
            let item = coerce_common_types(la, lb, pos)?;
            DataType::List(Box::new(item))
        }
        _ => {
            return Err(InvalidQuery::new(
                format!(
                    "expected list types, got `{}` and `{}`",
                    adt.display_name(),
                    bdt.display_name()
                ),
                pos,
            ));
        }
    };
    if !can_coerce_to(a, &out) || !can_coerce_to(b, &out) {
        return Err(InvalidQuery::new(
            format!(
                "incompatible types `{}` and `{}`",
                adt.display_name(),
                bdt.display_name()
            ),
            pos,
        ));
    }
    Ok(out)
}

fn can_coerce_to(expr: &Expr, target: &DataType) -> bool {
    let src_dt = expr.data_type();
    if src_dt == *target {
        return true;
    }
    match (expr, target) {
        (Expr::Literal(ScalarValue::Null(_)), _) => true,
        (Expr::Literal(ScalarValue::I64(_)), DataType::F64) => true,
        (Expr::Literal(ScalarValue::TimestampNs { .. }), DataType::TimestampNs { .. }) => true,
        (Expr::Literal(ScalarValue::List { values, .. }), DataType::List(tgt_item)) => {
            values.iter().all(|v| can_coerce_scalar_to(v, tgt_item))
        }
        (Expr::MakeArray { elems, .. }, DataType::List(tgt_item)) => {
            elems.iter().all(|el| can_coerce_to(el, tgt_item))
        }
        _ => can_cast(&src_dt, target),
    }
}

fn can_coerce_scalar_to(sv: &ScalarValue, target: &DataType) -> bool {
    let src_dt = sv.data_type();
    if src_dt == *target {
        return true;
    }
    match (sv, target) {
        (ScalarValue::Null(_), _) => true,
        (ScalarValue::I64(_), DataType::F64) => true,
        (ScalarValue::TimestampNs { .. }, DataType::TimestampNs { .. }) => true,
        (ScalarValue::List { values, .. }, DataType::List(tgt_item)) => {
            values.iter().all(|v| can_coerce_scalar_to(v, tgt_item))
        }
        _ => can_cast(&src_dt, target),
    }
}

fn coerce_expr(expr: Expr, target: &DataType, pos: Option<Position>) -> Result<Expr, InvalidQuery> {
    let src_dt = expr.data_type();
    if src_dt == *target {
        return Ok(expr);
    }
    match (expr, target) {
        (Expr::Literal(ScalarValue::Null(_)), _) => {
            Ok(Expr::Literal(ScalarValue::Null(target.clone())))
        }
        (Expr::Literal(ScalarValue::I64(n)), DataType::F64) => {
            Ok(Expr::Literal(ScalarValue::F64(n as f64)))
        }
        (Expr::Literal(ScalarValue::TimestampNs { value, .. }), DataType::TimestampNs { utc }) => {
            Ok(Expr::Literal(ScalarValue::TimestampNs { value, utc: *utc }))
        }
        (Expr::Literal(ScalarValue::List { values, .. }), DataType::List(tgt_item)) => {
            let mut coerced = Vec::with_capacity(values.len());
            let mut all_literals = true;
            for v in values {
                let c = coerce_expr(Expr::Literal(v), tgt_item, pos)?;
                if !matches!(c, Expr::Literal(_)) {
                    all_literals = false;
                }
                coerced.push(c);
            }
            if all_literals {
                let new_vals: Vec<ScalarValue> = coerced
                    .into_iter()
                    .filter_map(|e| match e {
                        Expr::Literal(sv) => Some(sv),
                        _ => None,
                    })
                    .collect();
                let list_val = ScalarValue::List {
                    item_type: (**tgt_item).clone(),
                    values: new_vals,
                };
                list_val
                    .validate(MAX_NESTING_DEPTH)
                    .map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
                Ok(Expr::Literal(list_val))
            } else {
                let list_dt = DataType::List(Box::new((**tgt_item).clone()));
                list_dt
                    .validate(MAX_NESTING_DEPTH)
                    .map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
                Ok(Expr::MakeArray {
                    elems: coerced,
                    item_type: (**tgt_item).clone(),
                })
            }
        }
        (Expr::MakeArray { elems, .. }, DataType::List(tgt_item)) => {
            let new_elems = elems
                .into_iter()
                .map(|el| coerce_expr(el, tgt_item, pos))
                .collect::<Result<Vec<_>, _>>()?;
            let list_dt = DataType::List(Box::new((**tgt_item).clone()));
            list_dt
                .validate(MAX_NESTING_DEPTH)
                .map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
            Ok(Expr::MakeArray {
                elems: new_elems,
                item_type: (**tgt_item).clone(),
            })
        }
        (other, _) => {
            if !can_cast(&src_dt, target) {
                return Err(InvalidQuery::new(
                    format!(
                        "cannot coerce `{}` to `{}`",
                        src_dt.display_name(),
                        target.display_name()
                    ),
                    pos,
                ));
            }
            Ok(Expr::Cast {
                expr: Box::new(other),
                target: target.clone(),
                mode: CastMode::Strict,
            })
        }
    }
}

fn can_cast(src: &DataType, target: &DataType) -> bool {
    if src == target || matches!(src, DataType::Null) || matches!(target, DataType::Null) {
        return true;
    }
    match (src, target) {
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => true,
        (DataType::I64, DataType::F64 | DataType::Bool | DataType::Decimal128 { .. })
        | (DataType::F64, DataType::I64 | DataType::Decimal128 { .. })
        | (DataType::Bool, DataType::I64)
        | (
            DataType::Decimal128 { .. },
            DataType::I64 | DataType::F64 | DataType::Decimal128 { .. },
        ) => true,
        (
            DataType::Utf8,
            DataType::Bool
            | DataType::I64
            | DataType::F64
            | DataType::Decimal128 { .. }
            | DataType::TimestampNs { .. }
            | DataType::Date
            | DataType::Binary
            | DataType::FixedBytes8
            | DataType::FixedBytes16,
        ) => true,
        (
            DataType::Bool
            | DataType::I64
            | DataType::F64
            | DataType::Decimal128 { .. }
            | DataType::Binary
            | DataType::FixedBytes8
            | DataType::FixedBytes16
            | DataType::Date
            | DataType::TimestampNs { .. },
            DataType::Utf8,
        ) => true,
        (DataType::I64, DataType::TimestampNs { .. })
        | (DataType::TimestampNs { .. }, DataType::I64)
        | (DataType::Date, DataType::TimestampNs { .. })
        | (DataType::TimestampNs { .. }, DataType::Date)
        | (DataType::FixedBytes8 | DataType::FixedBytes16, DataType::Binary)
        | (DataType::Binary, DataType::FixedBytes8 | DataType::FixedBytes16) => true,
        (DataType::List(a), DataType::List(_)) if matches!(a.as_ref(), DataType::Null) => true,
        _ => false,
    }
}

fn bind_sql_type(dt: &SqlDataType, pos: Option<Position>) -> Result<DataType, InvalidQuery> {
    match dt {
        SqlDataType::Boolean | SqlDataType::Bool => Ok(DataType::Bool),
        SqlDataType::BigInt(None)
        | SqlDataType::Int64
        | SqlDataType::Integer(None)
        | SqlDataType::Int(None)
        | SqlDataType::SmallInt(None)
        | SqlDataType::TinyInt(None)
        | SqlDataType::Int2(None)
        | SqlDataType::Int4(None)
        | SqlDataType::Int8(None) => Ok(DataType::I64),
        SqlDataType::Double(ExactNumberInfo::None)
        | SqlDataType::DoublePrecision
        | SqlDataType::Float(ExactNumberInfo::None)
        | SqlDataType::Float64
        | SqlDataType::Float32
        | SqlDataType::Float4
        | SqlDataType::Float8
        | SqlDataType::Real => Ok(DataType::F64),
        SqlDataType::Decimal(info) | SqlDataType::Numeric(info) | SqlDataType::Dec(info) => {
            let (p, s) = match info {
                ExactNumberInfo::None => (38u8, 10i8),
                ExactNumberInfo::Precision(p) => (
                    u8::try_from(*p)
                        .map_err(|_| InvalidQuery::new("decimal precision out of range", pos))?,
                    0,
                ),
                ExactNumberInfo::PrecisionAndScale(p, s) => (
                    u8::try_from(*p)
                        .map_err(|_| InvalidQuery::new("decimal precision out of range", pos))?,
                    i8::try_from(*s)
                        .map_err(|_| InvalidQuery::new("decimal scale out of range", pos))?,
                ),
            };
            let out = DataType::Decimal128 {
                precision: p,
                scale: s,
            };
            out.validate(MAX_NESTING_DEPTH)
                .map_err(|e| InvalidQuery::new(e.to_string(), pos))?;
            Ok(out)
        }
        SqlDataType::Varchar(_)
        | SqlDataType::Char(_)
        | SqlDataType::Character(_)
        | SqlDataType::CharacterVarying(_)
        | SqlDataType::Text
        | SqlDataType::String(_) => Ok(DataType::Utf8),
        SqlDataType::Bytea
        | SqlDataType::Binary(_)
        | SqlDataType::Varbinary(_)
        | SqlDataType::Blob(_) => Ok(DataType::Binary),
        SqlDataType::Date => Ok(DataType::Date),
        SqlDataType::Timestamp(None | Some(9), tz) => {
            let utc = !matches!(tz, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone);
            Ok(DataType::TimestampNs { utc })
        }
        _ => Err(InvalidQuery::new(format!("unsupported type `{dt}`"), pos)),
    }
}

fn at(spanned: &impl Spanned) -> Option<Position> {
    let start = spanned.span().start;
    at_location(start.line, start.column)
}

fn at_ident(ident: &Ident) -> Option<Position> {
    at_location(ident.span.start.line, ident.span.start.column)
}

fn at_location(line: u64, column: u64) -> Option<Position> {
    if line == 0 {
        return None;
    }
    Some(position_of(line, column))
}

#[cfg(test)]
mod tests {
    use cetra_column::Field;

    use super::*;
    use crate::text::{SQL_LIMITS, TextKind, expect_end, parse_failure, parser, tokenize};

    fn test_schema() -> Schema {
        Schema::try_new(vec![
            Field::new("name", DataType::Utf8, true),
            Field::new("status", DataType::Utf8, false),
            Field::new("environment", DataType::Utf8, true),
            Field::new("duration_ns", DataType::I64, false),
            Field::new("cost_total", DataType::F64, true),
            Field::new("has_root", DataType::Bool, false),
            Field::new("start_time", DataType::timestamp_utc(), false),
            Field::new("day", DataType::Date, true),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), false),
            Field::new("nums", DataType::List(Box::new(DataType::I64)), true),
            Field::new("metadata", DataType::MapUtf8, false),
            Field::new("raw_bin", DataType::Binary, true),
            Field::new(
                "dec",
                DataType::Decimal128 {
                    precision: 18,
                    scale: 4,
                },
                true,
            ),
            Field::new("MixedCol", DataType::I64, true),
        ])
        .unwrap()
    }

    fn parse_and_bind(sql: &str, schema: &Schema) -> Result<Expr, InvalidQuery> {
        let tokens = tokenize(sql, &SQL_LIMITS, TextKind::Sql)?;
        let mut p = parser(tokens);
        let ast = p.parse_expr().map_err(|e| parse_failure(e, &p, sql))?;
        expect_end(&mut p, sql)?;
        bind_expr(&ast, schema)
    }

    #[test]
    fn all_50_scalar_functions_aliases_coercions_and_denied_functions() {
        let schema = test_schema();

        // All 50 curated ScalarFn forms + aliases + special SQL syntax forms
        let cases = [
            // String & binary (19 + aliases + special syntax)
            ("lower(name)", DataType::Utf8),
            ("upper(name)", DataType::Utf8),
            ("length(name)", DataType::I64),
            ("char_length(name)", DataType::I64),
            ("character_length(name)", DataType::I64),
            ("substr(name, 1, 3)", DataType::Utf8),
            ("substring(name, 2)", DataType::Utf8),
            ("SUBSTRING(name FROM 1 FOR 4)", DataType::Utf8),
            ("trim(name)", DataType::Utf8),
            ("trim(LEADING ' ' FROM name)", DataType::Utf8),
            ("trim(TRAILING 'x' FROM name)", DataType::Utf8),
            ("trim(BOTH 'x' FROM name)", DataType::Utf8),
            ("trim(name, 'xy')", DataType::Utf8),
            ("ltrim(name, 'x')", DataType::Utf8),
            ("rtrim(name)", DataType::Utf8),
            ("left(name, 2)", DataType::Utf8),
            ("right(name, -1)", DataType::Utf8),
            ("split_part(name, '/', 1)", DataType::Utf8),
            ("concat(name, ':', duration_ns, true)", DataType::Utf8),
            ("concat_ws(',', name, duration_ns, NULL)", DataType::Utf8),
            ("starts_with(name, 'a')", DataType::Bool),
            ("ends_with(name, 'z')", DataType::Bool),
            ("contains(name, 'mid')", DataType::Bool),
            ("strpos(name, 'x')", DataType::I64),
            ("POSITION('x' IN name)", DataType::I64),
            ("regexp_like(name, '^a', 'i')", DataType::Bool),
            ("regexp_extract(name, '([a-z]+)')", DataType::Utf8),
            ("encode(raw_bin, 'hex')", DataType::Utf8),
            // Time & interval (5 + alias + extract + typed string + interval)
            ("now()", DataType::TimestampNs { utc: true }),
            (
                "date_trunc('hour', start_time)",
                DataType::TimestampNs { utc: true },
            ),
            (
                "date_bin(INTERVAL '5 minutes', start_time, TIMESTAMP '1970-01-01T00:00:00Z')",
                DataType::TimestampNs { utc: true },
            ),
            ("EXTRACT(YEAR FROM start_time)", DataType::I64),
            ("EXTRACT(EPOCH FROM start_time)", DataType::F64),
            ("date_part('epoch', start_time)", DataType::F64),
            (
                "to_timestamp(duration_ns)",
                DataType::TimestampNs { utc: true },
            ),
            (
                "TIMESTAMP '2026-01-01T00:00:00Z'",
                DataType::TimestampNs { utc: false },
            ),
            ("DATE '2026-01-01'", DataType::Date),
            ("INTERVAL '5' MINUTE", DataType::Interval),
            (
                "start_time + INTERVAL '1 hour'",
                DataType::TimestampNs { utc: true },
            ),
            (
                "INTERVAL '1 day' + start_time",
                DataType::TimestampNs { utc: true },
            ),
            (
                "start_time - INTERVAL '1 hour'",
                DataType::TimestampNs { utc: true },
            ),
            // Math & null (14 + aliases + special ceil/floor)
            ("abs(duration_ns)", DataType::I64),
            ("round(cost_total, 2)", DataType::F64),
            ("round(duration_ns)", DataType::I64),
            ("floor(cost_total)", DataType::F64),
            ("ceil(cost_total)", DataType::F64),
            ("ceiling(cost_total)", DataType::F64),
            ("sqrt(duration_ns)", DataType::F64),
            ("ln(duration_ns)", DataType::F64),
            ("log10(duration_ns)", DataType::F64),
            ("power(2, 3)", DataType::F64),
            ("pow(duration_ns, 0.5)", DataType::F64),
            ("coalesce(name, 'fallback')", DataType::Utf8),
            ("coalesce(NULL, duration_ns, 1.5)", DataType::F64),
            ("nullif(duration_ns, 0)", DataType::I64),
            ("greatest(duration_ns, 10, 20.5)", DataType::F64),
            ("least(cost_total, 1)", DataType::F64),
            ("isnan(cost_total)", DataType::Bool),
            ("isfinite(duration_ns)", DataType::Bool),
            // Array & map (8 + alias + @> + || + subscript)
            ("array_has(tags, 'alpha')", DataType::Bool),
            ("array_contains(tags, 'alpha')", DataType::Bool),
            ("array_has_all(tags, ['alpha'])", DataType::Bool),
            ("tags @> []", DataType::Bool),
            ("tags @> [lower('BETA')]", DataType::Bool),
            ("tags || []", DataType::List(Box::new(DataType::Utf8))),
            ("array_length(tags)", DataType::I64),
            ("array_element(tags, -1)", DataType::Utf8),
            ("tags[1]", DataType::Utf8),
            ("array_to_string(tags, ',', 'none')", DataType::Utf8),
            (
                "array_distinct(tags)",
                DataType::List(Box::new(DataType::Utf8)),
            ),
            (
                "map_keys(metadata)",
                DataType::List(Box::new(DataType::Utf8)),
            ),
            (
                "map_values(metadata)",
                DataType::List(Box::new(DataType::Utf8)),
            ),
            ("metadata['k']", DataType::Utf8),
            // JSON (4 + -> + ->> + JsonAccess)
            ("name -> 'k'", DataType::Utf8),
            ("name ->> 0", DataType::Utf8),
            ("json_get(name, 'k')", DataType::Utf8),
            ("json_get_text(name, 0)", DataType::Utf8),
            ("json_extract(name, '$.a')", DataType::Utf8),
            ("json_extract_text(name, '$.a')", DataType::Utf8),
            ("name:foo[0]", DataType::Utf8),
            // Misc AST forms: Hex, Unary +, -, Case, IsTrue, IsUnknown, Between, InList, Coercions
            ("X'deadbeef'", DataType::Binary),
            ("+duration_ns", DataType::I64),
            ("-9223372036854775808", DataType::I64),
            ("has_root IS TRUE AND has_root IS NOT FALSE", DataType::Bool),
            ("name IS UNKNOWN OR name IS NOT UNKNOWN", DataType::Bool),
            ("duration_ns BETWEEN 1e8 AND 5e9", DataType::Bool),
            ("start_time >= '2026-01-01T10:05:00Z'", DataType::Bool),
            ("day >= '2026-01-01'", DataType::Bool),
            (
                "dec + 1",
                DataType::Decimal128 {
                    precision: 18,
                    scale: 4,
                },
            ),
            ("dec + 1.5", DataType::F64),
            (
                "CASE WHEN duration_ns > 0 THEN 1 ELSE 2.5 END",
                DataType::F64,
            ),
            ("mixedcol = 1 AND \"MixedCol\" = 2", DataType::Bool),
        ];

        for (sql, expected_dt) in cases {
            let expr = parse_and_bind(sql, &schema)
                .unwrap_or_else(|e| panic!("failed `{sql}`: {e} at {:?}", e.position));
            assert_eq!(expr.data_type(), expected_dt, "{sql}");
        }

        // Verify POSITION('x' IN name) swapped args -> Strpos(name, 'x')
        let pos_expr = parse_and_bind("POSITION('x' IN name)", &schema).unwrap();
        let Expr::Call { func, args, .. } = pos_expr else {
            panic!("expected Call");
        };
        assert_eq!(func, ScalarFn::Strpos);
        assert!(matches!(&args[0], Expr::Column { name, .. } if name == "name"));
        assert!(matches!(&args[1], Expr::Literal(ScalarValue::Utf8(s)) if s == "x"));

        // Verify constant array with NULL folds to Literal(List) with typed Null element
        let arr_lit = parse_and_bind("['a', NULL]", &schema).unwrap();
        assert!(matches!(
            arr_lit,
            Expr::Literal(ScalarValue::List {
                item_type: DataType::Utf8,
                ..
            })
        ));

        // Verify dynamic array emits MakeArray
        let arr_dyn = parse_and_bind("[lower('A'), name]", &schema).unwrap();
        assert!(matches!(
            arr_dyn,
            Expr::MakeArray {
                item_type: DataType::Utf8,
                ..
            }
        ));

        // All 20 DENIED_FUNCTIONS are rejected with exact message
        for name in DENIED_FUNCTIONS {
            let sql = format!("{name}(1)");
            let err = parse_and_bind(&sql, &schema).expect_err(name);
            assert_eq!(err.message, format!("function `{name}` is not available"));
        }

        // compile_filter rejects non-bool expression with exact message and position
        let non_bool = compile_filter(FilterView::Spans, &schema, "duration_ns + 1")
            .expect_err("non-bool filter");
        assert_eq!(
            non_bool.message,
            "the filter must be a true/false expression"
        );
        assert_eq!(non_bool.position, Some(Position { line: 1, column: 1 }));

        // Quoted vs unquoted case sensitivity
        assert!(parse_and_bind("\"mixedcol\" = 1", &schema).is_err());

        // Multi-char ESCAPE rejected
        assert!(parse_and_bind("name LIKE 'a' ESCAPE '!!'", &schema).is_err());
    }

    #[test]
    fn recursion_depth_bound_and_512_op_left_deep_spine() {
        let schema = test_schema();

        // (a) 200-deep nested SqlExpr::Nested returns Err(InvalidQuery) without stack overflow.
        let mut nested = SqlExpr::Identifier(Ident::new("duration_ns"));
        for _ in 0..200 {
            nested = SqlExpr::Nested(Box::new(nested));
        }
        let err = bind_expr(&nested, &schema).expect_err("200-deep nested expression");
        assert_eq!(err.message, "the expression is nested too deeply");

        // (b) 512-op left-deep `a + 1 + 1 + ...` chain binds cleanly without stack overflow.
        let mut sql = String::from("duration_ns");
        for _ in 0..SQL_LIMITS.max_operators {
            sql.push_str(" + 1");
        }
        let bound = parse_and_bind(&sql, &schema).expect("512-op left-deep chain");
        assert_eq!(bound.data_type(), DataType::I64);
    }

    #[test]
    fn compiled_filter_debug_redacts_score_names() {
        let schema = test_schema();
        let compiled = compile_filter(
            FilterView::Spans,
            &schema,
            "score('secret_score_name') > 0.5",
        )
        .expect("compile filter");
        let dbg = format!("{compiled:?}");
        assert!(!dbg.contains("secret_score_name"), "{dbg}");
        assert!(dbg.contains("scores_len: 1"), "{dbg}");
    }

    #[test]
    fn list_coercion_literals_make_array_and_column_restrictions() {
        use std::sync::Arc;

        use cetra_column::{Batch, CancelToken, Column, FlatColumn, FlatData, Selection};

        use crate::{EvalContext, eval_expr};

        let schema = test_schema();

        // All 6 list coercion expressions bind and evaluate cleanly.
        let batch_schema = Schema::try_new(vec![Field::new(
            "start_time",
            DataType::timestamp_utc(),
            false,
        )])
        .unwrap();
        let start_ns = parse_timestamp_ns("2026-06-01T12:00:00Z", true).unwrap();
        let start_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    utc: true,
                    values: vec![start_ns],
                },
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new(
            Arc::new(batch_schema.clone()),
            vec![start_col],
            1,
            Selection::All,
        )
        .unwrap();
        let ctx = EvalContext {
            now_ns: start_ns,
            cancel: CancelToken::new(),
        };

        let exprs = [
            ("[1, 2] @> [1.0]", DataType::Bool),
            ("array_has([1, 2], 1.5)", DataType::Bool),
            ("array_has([1, 2], 1.0)", DataType::Bool),
            ("[1, 2] || [3.0]", DataType::List(Box::new(DataType::F64))),
            (
                "[[1], [2.5]]",
                DataType::List(Box::new(DataType::List(Box::new(DataType::F64)))),
            ),
            (
                "[[NULL]] || [['a']]",
                DataType::List(Box::new(DataType::List(Box::new(DataType::Utf8)))),
            ),
            (
                "[TIMESTAMP '2026-01-01T00:00:00Z'] || [start_time]",
                DataType::List(Box::new(DataType::TimestampNs { utc: true })),
            ),
        ];

        let mut results = Vec::new();
        for (sql, expected_dt) in exprs {
            let bound =
                parse_and_bind(sql, &schema).unwrap_or_else(|e| panic!("failed `{sql}`: {e}"));
            assert_eq!(bound.data_type(), expected_dt, "{sql}");

            let eval_bound = parse_and_bind(sql, &batch_schema)
                .unwrap_or_else(|e| panic!("failed eval bind `{sql}`: {e}"));
            let out = eval_expr(&eval_bound, &batch, &ctx, None)
                .unwrap_or_else(|e| panic!("failed eval `{sql}`: {e}"));
            results.push(out.get_scalar(0).unwrap());
        }

        assert_eq!(results[0], ScalarValue::Bool(true));
        assert_eq!(results[1], ScalarValue::Bool(false));
        assert_eq!(results[2], ScalarValue::Bool(true));
        assert_eq!(
            results[3],
            ScalarValue::List {
                item_type: DataType::F64,
                values: vec![
                    ScalarValue::F64(1.0),
                    ScalarValue::F64(2.0),
                    ScalarValue::F64(3.0),
                ],
            }
        );
        assert_eq!(
            results[4],
            ScalarValue::List {
                item_type: DataType::List(Box::new(DataType::F64)),
                values: vec![
                    ScalarValue::List {
                        item_type: DataType::F64,
                        values: vec![ScalarValue::F64(1.0)],
                    },
                    ScalarValue::List {
                        item_type: DataType::F64,
                        values: vec![ScalarValue::F64(2.5)],
                    },
                ],
            }
        );
        assert_eq!(
            results[5],
            ScalarValue::List {
                item_type: DataType::List(Box::new(DataType::Utf8)),
                values: vec![
                    ScalarValue::List {
                        item_type: DataType::Utf8,
                        values: vec![ScalarValue::Null(DataType::Utf8)],
                    },
                    ScalarValue::List {
                        item_type: DataType::Utf8,
                        values: vec![ScalarValue::Utf8("a".to_owned())],
                    },
                ],
            }
        );
        let ts_2026 = parse_timestamp_ns("2026-01-01T00:00:00Z", true).unwrap();
        assert_eq!(
            results[6],
            ScalarValue::List {
                item_type: DataType::TimestampNs { utc: true },
                values: vec![
                    ScalarValue::TimestampNs {
                        value: ts_2026,
                        utc: true,
                    },
                    ScalarValue::TimestampNs {
                        value: start_ns,
                        utc: true,
                    },
                ],
            }
        );

        // Non-literal List(A) vs List(B) columns (where A != B, neither Null) are rejected.
        for bad_sql in [
            "nums @> [1.0]",
            "nums || [1.0]",
            "array_has(nums, 1.5)",
            "nums = [1.0]",
            "nums || tags",
        ] {
            assert!(
                parse_and_bind(bad_sql, &schema).is_err(),
                "expected `{bad_sql}` to be rejected"
            );
        }
    }

    #[test]
    fn qualified_subscripts_extract_interval_signs_typed_string_float8_and_quote_stripping() {
        let mut fields = test_schema().fields().to_vec();
        fields.push(Field::new("attributes", DataType::MapUtf8, true));
        let schema = Schema::try_new(fields).unwrap();

        // Major 4: qualified map/list subscripts
        let attr_sub = parse_and_bind("s.attributes['k']", &schema).unwrap();
        assert_eq!(attr_sub.data_type(), DataType::Utf8);
        assert!(matches!(attr_sub, Expr::MapIndex { .. }));

        let tag_sub = parse_and_bind("s.tags[1]", &schema).unwrap();
        assert_eq!(tag_sub.data_type(), DataType::Utf8);
        assert!(matches!(
            tag_sub,
            Expr::Call {
                func: ScalarFn::ArrayElement,
                ..
            }
        ));

        // Major 5: EXTRACT and date_part both coerce Utf8 via coerce_to_timestamp_arg
        let ext_str = parse_and_bind("EXTRACT(YEAR FROM '2026-01-01T00:00:00Z')", &schema).unwrap();
        assert_eq!(ext_str.data_type(), DataType::I64);
        let dp_str = parse_and_bind("date_part('year', '2026-01-01T00:00:00Z')", &schema).unwrap();
        assert_eq!(dp_str.data_type(), DataType::I64);

        // Major 6: signed numeric interval literals
        let neg_day = parse_and_bind("INTERVAL -1 DAY", &schema).unwrap();
        assert_eq!(
            neg_day,
            Expr::Literal(ScalarValue::Interval(cetra_column::Interval {
                months: 0,
                days: -1,
                nanos: 0,
            }))
        );
        let pos_hour = parse_and_bind("INTERVAL +1 HOUR", &schema).unwrap();
        assert_eq!(
            pos_hour,
            Expr::Literal(ScalarValue::Interval(cetra_column::Interval {
                months: 0,
                days: 0,
                nanos: 3_600_000_000_000,
            }))
        );

        // Minor 1: TIMESTAMP(9) folds to Literal and FLOAT8 binds to F64
        let ts9 = parse_and_bind("TIMESTAMP(9) '2026-01-01T00:00:00Z'", &schema).unwrap();
        assert!(matches!(
            ts9,
            Expr::Literal(ScalarValue::TimestampNs { utc: false, .. })
        ));
        let float8 = parse_and_bind("CAST(duration_ns AS FLOAT8)", &schema).unwrap();
        assert_eq!(float8.data_type(), DataType::F64);

        // Minor 2: quoted extract field on interval rejected at bind time
        let err = parse_and_bind("date_part('''quarter''', INTERVAL '1 day')", &schema)
            .expect_err("quoted quarter on interval");
        assert!(
            err.message.contains("not supported on interval"),
            "{}",
            err.message
        );
    }
}
