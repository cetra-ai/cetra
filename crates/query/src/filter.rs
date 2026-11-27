//! Filter DSL parser, validator, and AST rewriter.
//!
//! Matches `crates/storage/src/filter/` on grammar, error messages, `Position`s,
//! `TRY_CAST` rewrite, `references_environment`, and `score()`/`score_label()`/`has_score()`
//! metadata (`MAX_SCORE_NAMES = 8`).

use cetra_column::{DataType, Field, Schema};
use sqlparser::ast::{
    AccessExpr, BinaryOperator, CastKind, DataType as SqlDataType, ExactNumberInfo, Expr, Function,
    FunctionArg, FunctionArgExpr, FunctionArguments, Ident, InTarget, Interval, ObjectNamePart,
    Spanned, Subscript, TimezoneInfo, UnaryOperator, Value,
};

use crate::text::{
    FILTER_LIMITS, TextKind, expect_end, parse_failure, parser, position_of, tokenize,
};
use crate::{InvalidQuery, Position};

/// Maximum distinct score names referenced in one filter (D97).
pub const MAX_SCORE_NAMES: usize = 8;

/// Target view of a Filter DSL expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterView {
    /// `traces` view.
    Traces,
    /// `spans` view.
    Spans,
    /// `sessions` view.
    Sessions,
    /// `scores` view.
    Scores,
}

/// Validated & parsed Filter DSL ready for binding/planning.
#[derive(Clone)]
pub struct ParsedFilter {
    /// Target view.
    pub view: FilterView,
    /// Validated & rewritten `sqlparser` AST (casts rewritten to `TryCast`, score calls to `__score_{i}_*` / `IS TRUE`).
    pub ast: Expr,
    /// True iff any column or map root resolved to `"environment"`.
    pub references_environment: bool,
    /// Distinct score names in first-appearance order (`<= MAX_SCORE_NAMES`).
    pub scores: Vec<String>,
}

impl std::fmt::Debug for ParsedFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedFilter")
            .field("view", &self.view)
            .field("references_environment", &self.references_environment)
            .field("scores_len", &self.scores.len())
            .finish()
    }
}

/// Parses and validates a Filter DSL string against `view`'s base `schema`.
///
/// Empty or whitespace-only text is rejected. Callers that treat an empty
/// filter as "no filter" must check before calling.
pub fn parse_filter_ast(
    view: FilterView,
    schema: &Schema,
    text: &str,
) -> Result<ParsedFilter, InvalidQuery> {
    if text.chars().all(char::is_whitespace) {
        return Err(InvalidQuery::new("the filter is empty", None));
    }
    let tokens = tokenize(text, &FILTER_LIMITS, TextKind::Filter)?;
    let mut parsed = parser(tokens);
    let mut ast = parsed
        .parse_expr()
        .map_err(|error| parse_failure(error, &parsed, text))?;
    expect_end(&mut parsed, text)?;
    let mut walk = Walk {
        view,
        schema,
        references_environment: false,
        scores: Vec::new(),
    };
    walk.expr(&mut ast)?;
    Ok(ParsedFilter {
        view,
        ast,
        references_environment: walk.references_environment,
        scores: walk.scores,
    })
}

/// Returns `base` extended with the 3 nullable score columns (`__score_{i}_num: F64`,
/// `__score_{i}_str: Utf8`, `__score_{i}_present: Bool`) for each name in `scores`.
pub fn filter_planning_schema(base: &Schema, scores: &[String]) -> Result<Schema, InvalidQuery> {
    if scores.len() > MAX_SCORE_NAMES {
        return Err(InvalidQuery::new(
            "a filter can use at most 8 score names",
            None,
        ));
    }
    let mut fields = base.fields().to_vec();
    for index in 0..scores.len() {
        fields.push(Field::new(
            format!("__score_{index}_num"),
            DataType::F64,
            true,
        ));
        fields.push(Field::new(
            format!("__score_{index}_str"),
            DataType::Utf8,
            true,
        ));
        fields.push(Field::new(
            format!("__score_{index}_present"),
            DataType::Bool,
            true,
        ));
    }
    Schema::try_new(fields).map_err(|err| InvalidQuery::new(err.to_string(), None))
}

/// Escapes `\`, `%`, `_` with `\` and wraps `%<escaped>%` (spec 05 § Full-text search).
pub fn like_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for character in query.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    format!("%{escaped}%")
}

struct Walk<'a> {
    view: FilterView,
    schema: &'a Schema,
    references_environment: bool,
    scores: Vec<String>,
}

impl Walk<'_> {
    fn expr(&mut self, expr: &mut Expr) -> Result<(), InvalidQuery> {
        if matches!(expr, Expr::Cast { .. }) {
            return self.cast(expr);
        }
        if matches!(expr, Expr::Function(_)) {
            return self.function(expr);
        }
        let position = at(expr);
        match expr {
            Expr::Identifier(ident) => self.column(ident),
            Expr::CompoundIdentifier(_) => Err(forbid(position, "a qualified name")),
            Expr::CompoundFieldAccess { root, access_chain } => {
                self.map_access(position, root.as_ref(), access_chain.as_slice())
            }
            Expr::BinaryOp { left, op, right } => {
                if !binary_allowed(op) {
                    return Err(forbid(position, &format!("operator {op}")));
                }
                self.expr(left)?;
                self.expr(right)
            }
            Expr::UnaryOp { op, expr } => {
                if !unary_allowed(op) {
                    return Err(forbid(position, &format!("operator {op}")));
                }
                self.expr(expr)
            }
            Expr::IsNull(inner) | Expr::IsNotNull(inner) => self.expr(inner),
            Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right) => {
                self.expr(left)?;
                self.expr(right)
            }
            Expr::In {
                expr,
                target,
                global,
                ..
            } => {
                if *global {
                    return Err(forbid(position, "GLOBAL IN"));
                }
                match target {
                    InTarget::List(list) => {
                        self.expr(expr)?;
                        for item in list {
                            self.expr(item)?;
                        }
                        Ok(())
                    }
                    InTarget::Subquery(_) => Err(forbid(position, "a subquery")),
                    InTarget::Unnest(_) => Err(forbid(position, "IN UNNEST")),
                    InTarget::Table { .. } => Err(forbid(position, "this expression")),
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.expr(expr)?;
                self.expr(low)?;
                self.expr(high)
            }
            Expr::Like {
                any,
                expr,
                pattern,
                escape_char,
                ..
            } => {
                if *any {
                    return Err(forbid(position, "LIKE ANY"));
                }
                self.expr(expr)?;
                self.expr(pattern)?;
                if let Some(esc) = escape_char {
                    self.expr(esc)?;
                }
                Ok(())
            }
            Expr::ILike {
                any,
                expr,
                pattern,
                escape_char,
                ..
            } => {
                if *any {
                    return Err(forbid(position, "ILIKE ANY"));
                }
                self.expr(expr)?;
                self.expr(pattern)?;
                if let Some(esc) = escape_char {
                    self.expr(esc)?;
                }
                Ok(())
            }
            Expr::Nested(inner) => self.expr(inner),
            Expr::Value(value) => literal(position, &value.value),
            Expr::Interval(interval) => interval_value(position, interval),
            Expr::Array(array) => {
                if array.named {
                    return Err(forbid(position, "a named array"));
                }
                for element in &mut array.elem {
                    self.expr(element)?;
                }
                Ok(())
            }
            Expr::Exists { .. } | Expr::Subquery(_) => Err(forbid(position, "a subquery")),
            Expr::Case { .. } => Err(forbid(position, "CASE")),
            _ => Err(forbid(position, "this expression")),
        }
    }

    fn column(&mut self, ident: &Ident) -> Result<(), InvalidQuery> {
        let name = column_name(ident);
        if self.schema.index_of(&name).is_none() {
            return Err(InvalidQuery::new(
                format!("unknown column `{name}`"),
                at_ident(ident),
            ));
        }
        if name == "environment" {
            self.references_environment = true;
        }
        Ok(())
    }

    fn map_access(
        &mut self,
        position: Option<Position>,
        root: &Expr,
        access_chain: &[AccessExpr],
    ) -> Result<(), InvalidQuery> {
        let Expr::Identifier(ident) = root else {
            return Err(forbid(position, "map access"));
        };
        let name = column_name(ident);
        let is_map = match self.schema.index_of(&name) {
            Some(idx) => matches!(self.schema.fields()[idx].data_type, DataType::MapUtf8),
            None => {
                return Err(InvalidQuery::new(
                    format!("unknown column `{name}`"),
                    at_ident(ident),
                ));
            }
        };
        if !is_map {
            return Err(InvalidQuery::new(
                format!("`{name}` is not a map column"),
                position,
            ));
        }
        if name == "environment" {
            self.references_environment = true;
        }
        let [AccessExpr::Subscript(Subscript::Index { index })] = access_chain else {
            return Err(InvalidQuery::new(
                "map keys must be string literals",
                position,
            ));
        };
        if !is_single_quoted(index) {
            return Err(InvalidQuery::new(
                "map keys must be string literals",
                at(index),
            ));
        }
        Ok(())
    }

    fn cast(&mut self, expr: &mut Expr) -> Result<(), InvalidQuery> {
        let position = at(expr);
        let error = match &*expr {
            Expr::Cast {
                kind,
                data_type,
                format,
                ..
            } => {
                if format.is_some() {
                    Some(forbid(position, "a cast format"))
                } else {
                    match kind {
                        CastKind::TryCast => Some(forbid(position, "TRY_CAST")),
                        CastKind::SafeCast => Some(forbid(position, "SAFE_CAST")),
                        CastKind::Cast | CastKind::DoubleColon if !cast_allowed(data_type) => {
                            Some(InvalidQuery::new(
                                format!("casts to `{data_type}` are not allowed"),
                                position,
                            ))
                        }
                        CastKind::Cast | CastKind::DoubleColon => None,
                    }
                }
            }
            _ => None,
        };
        if let Some(error) = error {
            return Err(error);
        }
        if let Expr::Cast {
            kind, expr: inner, ..
        } = expr
        {
            *kind = CastKind::TryCast;
            self.expr(inner)?;
        }
        Ok(())
    }

    fn function(&mut self, expr: &mut Expr) -> Result<(), InvalidQuery> {
        let position = at(expr);
        if self.rewrite_score(position, expr)? {
            return Ok(());
        }
        let Expr::Function(function) = expr else {
            return Ok(());
        };
        function_shape(position, function)?;
        let FunctionArguments::List(list) = &mut function.args else {
            return Ok(());
        };
        for arg in &mut list.args {
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(inner)) = arg else {
                continue;
            };
            self.expr(inner)?;
        }
        Ok(())
    }

    /// Replace a score call with its internal column. User identifiers are
    /// checked against the view before this runs, so `__score_0_num` typed by
    /// a caller is still an unknown column.
    fn rewrite_score(
        &mut self,
        position: Option<Position>,
        expr: &mut Expr,
    ) -> Result<bool, InvalidQuery> {
        let rewritten = {
            let Expr::Function(function) = &*expr else {
                return Ok(false);
            };
            let Some(name) = bare_name(function) else {
                return Ok(false);
            };
            if !matches!(name.as_str(), "score" | "score_label" | "has_score") {
                return Ok(false);
            }
            ensure_plain_call(position, function)?;
            let Some(score_name) = one_string_literal(function) else {
                return Err(InvalidQuery::new(
                    format!("{name}() takes one string literal, the score name"),
                    position,
                ));
            };
            if self.view == FilterView::Scores {
                return Err(InvalidQuery::new(
                    "score functions are not available on the scores view",
                    position,
                ));
            }
            let index = self.score_index(position, score_name)?;
            let suffix = match name.as_str() {
                "score" => "num",
                "score_label" => "str",
                _ => "present",
            };
            let column = Expr::Identifier(Ident::new(format!("__score_{index}_{suffix}")));
            if name == "has_score" {
                Expr::IsTrue(Box::new(column))
            } else {
                column
            }
        };
        *expr = rewritten;
        Ok(true)
    }

    fn score_index(
        &mut self,
        position: Option<Position>,
        name: String,
    ) -> Result<usize, InvalidQuery> {
        if let Some(index) = self.scores.iter().position(|existing| existing == &name) {
            return Ok(index);
        }
        if self.scores.len() == MAX_SCORE_NAMES {
            return Err(InvalidQuery::new(
                "a filter can use at most 8 score names",
                position,
            ));
        }
        self.scores.push(name);
        Ok(self.scores.len() - 1)
    }
}

fn ensure_plain_call(position: Option<Position>, function: &Function) -> Result<(), InvalidQuery> {
    if function.uses_odbc_syntax {
        return Err(forbid(position, "an ODBC function"));
    }
    if function.over.is_some() {
        return Err(forbid(position, "a window function"));
    }
    if function.filter.is_some() {
        return Err(forbid(position, "FILTER"));
    }
    if !function.within_group.is_empty() {
        return Err(forbid(position, "WITHIN GROUP"));
    }
    if function.null_treatment.is_some() {
        return Err(forbid(position, "null treatment"));
    }
    if !arguments_empty(&function.parameters) {
        return Err(forbid(position, "function parameters"));
    }
    let list = match &function.args {
        FunctionArguments::Subquery(_) => return Err(forbid(position, "a subquery")),
        FunctionArguments::None => {
            return Err(forbid(position, "a function call without parentheses"));
        }
        FunctionArguments::List(list) => list,
    };
    if list.duplicate_treatment.is_some() {
        return Err(forbid(position, "DISTINCT"));
    }
    if !list.clauses.is_empty() {
        return Err(forbid(position, "a function clause"));
    }
    Ok(())
}

fn bare_name(function: &Function) -> Option<String> {
    match function.name.0.as_slice() {
        [ObjectNamePart::Identifier(ident)] => Some(ident.value.to_ascii_lowercase()),
        _ => None,
    }
}

fn one_string_literal(function: &Function) -> Option<String> {
    let FunctionArguments::List(list) = &function.args else {
        return None;
    };
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))] = list.args.as_slice() else {
        return None;
    };
    match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) => Some(text.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn function_shape(position: Option<Position>, function: &Function) -> Result<(), InvalidQuery> {
    ensure_plain_call(position, function)?;
    let FunctionArguments::List(list) = &function.args else {
        return Err(forbid(position, "a function call without parentheses"));
    };
    let name = match function.name.0.as_slice() {
        [ObjectNamePart::Identifier(ident)] => ident.value.to_ascii_lowercase(),
        _ => return Err(forbid(position, "a qualified name")),
    };
    if !allowed_function(&name) {
        return Err(InvalidQuery::new(
            format!("unknown function `{name}`"),
            position,
        ));
    }
    if name == "now" && !list.args.is_empty() {
        return Err(InvalidQuery::new("now() takes no arguments", position));
    }
    for arg in &list.args {
        match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(_)) => {}
            FunctionArg::Unnamed(_) => return Err(forbid(position, "a wildcard")),
            FunctionArg::Named { .. } | FunctionArg::ExprNamed { .. } => {
                return Err(forbid(position, "a named argument"));
            }
        }
    }
    Ok(())
}

fn arguments_empty(args: &FunctionArguments) -> bool {
    match args {
        FunctionArguments::None => true,
        FunctionArguments::List(list) => {
            list.args.is_empty() && list.duplicate_treatment.is_none() && list.clauses.is_empty()
        }
        FunctionArguments::Subquery(_) => false,
    }
}

fn allowed_function(name: &str) -> bool {
    matches!(
        name,
        "now"
            | "lower"
            | "upper"
            | "length"
            | "coalesce"
            | "array_has"
            | "array_length"
            | "date_trunc"
            | "abs"
    )
}

fn binary_allowed(op: &BinaryOperator) -> bool {
    matches!(
        op,
        BinaryOperator::And
            | BinaryOperator::Or
            | BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
            | BinaryOperator::Plus
            | BinaryOperator::Minus
            | BinaryOperator::Multiply
            | BinaryOperator::Divide
            | BinaryOperator::Modulo
            | BinaryOperator::AtArrow
    )
}

fn unary_allowed(op: &UnaryOperator) -> bool {
    matches!(
        op,
        UnaryOperator::Not | UnaryOperator::Minus | UnaryOperator::Plus
    )
}

/// `BIGINT`, `DOUBLE` / `DOUBLE PRECISION`, `VARCHAR` / `TEXT` / `STRING`,
/// `BOOLEAN` / `BOOL`, and `TIMESTAMP` with neither precision nor time zone.
fn cast_allowed(data_type: &SqlDataType) -> bool {
    matches!(
        data_type,
        SqlDataType::BigInt(None)
            | SqlDataType::Double(ExactNumberInfo::None)
            | SqlDataType::DoublePrecision
            | SqlDataType::Varchar(None)
            | SqlDataType::Text
            | SqlDataType::String(None)
            | SqlDataType::Boolean
            | SqlDataType::Bool
            | SqlDataType::Timestamp(None, TimezoneInfo::None | TimezoneInfo::WithoutTimeZone)
    )
}

fn literal(position: Option<Position>, value: &Value) -> Result<(), InvalidQuery> {
    match value {
        Value::Number(_, _) | Value::SingleQuotedString(_) | Value::Boolean(_) | Value::Null => {
            Ok(())
        }
        Value::HexStringLiteral(_) => Err(forbid(position, "a hex literal")),
        Value::DoubleQuotedString(_) => Err(forbid(position, "a double-quoted string")),
        Value::Placeholder(_) => Err(forbid(position, "a placeholder")),
        Value::DollarQuotedString(_) => Err(forbid(position, "a dollar-quoted string")),
        _ => Err(forbid(position, "this literal")),
    }
}

fn interval_value(position: Option<Position>, interval: &Interval) -> Result<(), InvalidQuery> {
    if is_single_quoted(&interval.value) {
        Ok(())
    } else {
        Err(InvalidQuery::new(
            "interval values must be string literals",
            position,
        ))
    }
}

fn is_single_quoted(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(value) if matches!(value.value, Value::SingleQuotedString(_))
    )
}

/// Unquoted names compare like DataFusion: lowercased. Quoted names stay as written.
fn column_name(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_ascii_lowercase()
    }
}

fn forbid(position: Option<Position>, what: &str) -> InvalidQuery {
    InvalidQuery::new(format!("{what} is not allowed in a filter"), position)
}

fn at(spanned: &impl Spanned) -> Option<Position> {
    let start = spanned.span().start;
    at_location(start.line, start.column)
}

/// `Ident` carries a span but does not implement [`Spanned`] in this sqlparser.
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
    use super::*;

    fn at_pos(line: u32, column: u32) -> Option<Position> {
        Some(Position { line, column })
    }

    fn base_schema() -> Schema {
        Schema::try_new(vec![
            Field::new("trace_id", DataType::Utf8, false),
            Field::new("span_id", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("status", DataType::Utf8, false),
            Field::new("environment", DataType::Utf8, true),
            Field::new("duration_ns", DataType::I64, false),
            Field::new("cost_total", DataType::F64, true),
            Field::new("has_root", DataType::Bool, false),
            Field::new("start_time", DataType::timestamp_utc(), false),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), false),
            Field::new("metadata", DataType::MapUtf8, false),
            Field::new("attributes", DataType::MapUtf8, false),
        ])
        .unwrap()
    }

    #[test]
    fn valid_and_invalid_filters_match_spec_05() {
        let schema = base_schema();

        // Allowed operators (binary, unary +, -, NOT, IS [NOT] NULL, IS [NOT] DISTINCT FROM,
        // IN, BETWEEN, LIKE/ILIKE, brackets array, map['k'], INTERVAL, parenthesized).
        let valid_cases = [
            "status = 'ok' AND duration_ns >= +10 AND -duration_ns < 0",
            "NOT has_root OR environment IS DISTINCT FROM 'eval'",
            "environment IS NOT DISTINCT FROM NULL AND status IS NOT NULL",
            "status IN ('ok', 'error') AND duration_ns BETWEEN 1 AND 100",
            "name LIKE 'llm%' AND name ILIKE '%chat%'",
            "tags @> ['alpha', 'beta'] AND metadata['channel'] = 'email'",
            "start_time > now() - INTERVAL '7 days'",
            "lower(name) = 'a' AND upper(status) = 'OK' AND length(name) > 0",
            "coalesce(environment, 'prod') = 'prod' AND abs(duration_ns) > 0",
            "array_has(tags, 'alpha') AND array_length(tags) >= 1",
            "date_trunc('hour', start_time) >= '2026-01-01T00:00:00Z'",
            "date_trunc('hour', start_time) >= CAST('2026-01-01T00:00:00Z' AS TIMESTAMP)",
            "CAST(duration_ns AS DOUBLE) > 0 AND metadata['k']::BIGINT > 0",
            "CAST(name AS DOUBLE PRECISION) IS NULL AND name::VARCHAR = 'a'",
            "name::TEXT = 'a' AND name::STRING = 'a' AND name::BOOLEAN IS NULL",
            "name::BOOL IS NULL AND name::TIMESTAMP IS NULL",
            "\"name\" = 'x' AND NAME = 'x'",
        ];
        for text in valid_cases {
            let parsed = parse_filter_ast(FilterView::Spans, &schema, text)
                .unwrap_or_else(|e| panic!("{text}: {e} at {:?}", e.position));
            assert_eq!(parsed.view, FilterView::Spans);
        }

        // Casts rewrite to TryCast.
        let cast_parsed = parse_filter_ast(
            FilterView::Spans,
            &schema,
            "CAST(duration_ns AS DOUBLE) > 0 AND metadata['k']::BIGINT > 0",
        )
        .unwrap();
        let Expr::BinaryOp { left, right, .. } = &cast_parsed.ast else {
            panic!("expected BinaryOp");
        };
        let Expr::BinaryOp {
            left: cast_left, ..
        } = left.as_ref()
        else {
            panic!("expected left BinaryOp");
        };
        assert!(matches!(
            cast_left.as_ref(),
            Expr::Cast {
                kind: CastKind::TryCast,
                ..
            }
        ));
        let Expr::BinaryOp {
            left: cast_right, ..
        } = right.as_ref()
        else {
            panic!("expected right BinaryOp");
        };
        assert!(matches!(
            cast_right.as_ref(),
            Expr::Cast {
                kind: CastKind::TryCast,
                ..
            }
        ));

        // Disallowed constructs with exact error messages and positions.
        let invalid_cases = [
            ("nme = 'a'", "unknown column `nme`", at_pos(1, 1)),
            (
                "spans.name = 'a'",
                "a qualified name is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "trace_id IN (SELECT trace_id FROM spans)",
                "a subquery is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "EXISTS (SELECT 1)",
                "a subquery is not allowed in a filter",
                at_pos(1, 9),
            ),
            (
                "CASE WHEN true THEN true END",
                "CASE is not allowed in a filter",
                at_pos(1, 1),
            ),
            ("sleep(1) > 0", "unknown function `sleep`", at_pos(1, 1)),
            ("count(*) > 1", "unknown function `count`", at_pos(1, 1)),
            (
                "lower(DISTINCT name) = 'a'",
                "DISTINCT is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "row_number() OVER () > 1",
                "a window function is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "name = 'a' extra",
                "unexpected text after the expression",
                at_pos(1, 12),
            ),
            (
                "name['k'] = 'a'",
                "`name` is not a map column",
                at_pos(1, 1),
            ),
            (
                "metadata[1] = 'a'",
                "map keys must be string literals",
                at_pos(1, 10),
            ),
            (
                "CAST(name AS INT) = 1",
                "casts to `INT` are not allowed",
                at_pos(1, 6),
            ),
            (
                "TRY_CAST(name AS BIGINT) = 1",
                "TRY_CAST is not allowed in a filter",
                at_pos(1, 10),
            ),
            (
                "tags @> ARRAY['beta']",
                "a named array is not allowed in a filter",
                at_pos(1, 15),
            ),
            (
                "now(1) > start_time",
                "now() takes no arguments",
                at_pos(1, 1),
            ),
            (
                "x'00' = name",
                "a hex literal is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "name || 'x' = 'ax'",
                "operator || is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "name LIKE ANY ('a%')",
                "LIKE ANY is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "name ILIKE ANY ('a%')",
                "ILIKE ANY is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "lower(*) = 'a'",
                "a wildcard is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "lower(x => name) = 'a'",
                "a named argument is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "ns.lower(name) = 'a'",
                "a qualified name is not allowed in a filter",
                at_pos(1, 1),
            ),
            (
                "score(name) > 1",
                "score() takes one string literal, the score name",
                at_pos(1, 1),
            ),
            (
                "score('a', 'b') > 1",
                "score() takes one string literal, the score name",
                at_pos(1, 1),
            ),
            (
                "INTERVAL 1 = start_time",
                "interval values must be string literals",
                at_pos(1, 10),
            ),
            ("", "the filter is empty", None),
            (" \n\t", "the filter is empty", None),
        ];
        for (text, message, position) in invalid_cases {
            let err = parse_filter_ast(FilterView::Spans, &schema, text).expect_err(text);
            assert_eq!(
                (err.message.as_str(), err.position),
                (message, position),
                "{text}"
            );
        }

        // Byte cap (4096) and operator cap (128).
        let over_bytes = format!("name = '{}'", "a".repeat(4097));
        let err_bytes = parse_filter_ast(FilterView::Spans, &schema, &over_bytes).unwrap_err();
        assert_eq!(err_bytes.message, "the filter is longer than 4096 bytes");
        assert_eq!(err_bytes.position, None);

        let ok_ops = format!("{}has_root", "has_root OR ".repeat(128));
        assert!(parse_filter_ast(FilterView::Spans, &schema, &ok_ops).is_ok());
        let over_ops = format!("{}has_root", "has_root OR ".repeat(129));
        let err_ops = parse_filter_ast(FilterView::Spans, &schema, &over_ops).unwrap_err();
        assert_eq!(err_ops.message, "the filter has more than 128 operators");
        assert!(err_ops.position.is_some());

        // `references_environment` detection (direct column and map root if map were named environment).
        let with_env = parse_filter_ast(
            FilterView::Spans,
            &schema,
            "environment = 'production' OR status = 'ok'",
        )
        .unwrap();
        assert!(with_env.references_environment);
        let without_env = parse_filter_ast(FilterView::Spans, &schema, "status = 'ok'").unwrap();
        assert!(!without_env.references_environment);

        // Score functions on Traces / Spans / Sessions vs rejected on Scores, max 8 distinct names.
        for view in [FilterView::Traces, FilterView::Spans, FilterView::Sessions] {
            let parsed = parse_filter_ast(
                view,
                &schema,
                "score('q') >= 0.5 AND score_label('tone') = 'good' AND has_score('q') AND has_score('ok')",
            )
            .unwrap();
            assert_eq!(parsed.scores, vec!["q", "tone", "ok"]);
            let planning = filter_planning_schema(&schema, &parsed.scores).unwrap();
            assert_eq!(planning.len(), schema.len() + 9);
            assert_eq!(
                planning.fields()[schema.len()],
                Field::new("__score_0_num", DataType::F64, true)
            );
            assert_eq!(
                planning.fields()[schema.len() + 1],
                Field::new("__score_0_str", DataType::Utf8, true)
            );
            assert_eq!(
                planning.fields()[schema.len() + 2],
                Field::new("__score_0_present", DataType::Bool, true)
            );
        }

        // Verify `has_score('x')` rewrites to `IsTrue(Identifier("__score_0_present"))`.
        let has_score_parsed =
            parse_filter_ast(FilterView::Traces, &schema, "has_score('x')").unwrap();
        assert_eq!(
            has_score_parsed.ast,
            Expr::IsTrue(Box::new(Expr::Identifier(Ident::new("__score_0_present"))))
        );

        let err_scores_view =
            parse_filter_ast(FilterView::Scores, &schema, "score('x') > 1").unwrap_err();
        assert_eq!(
            err_scores_view.message,
            "score functions are not available on the scores view"
        );
        assert_eq!(err_scores_view.position, at_pos(1, 1));

        let err_unknown_internal =
            parse_filter_ast(FilterView::Spans, &schema, "__score_0_num > 1").unwrap_err();
        assert_eq!(
            err_unknown_internal.message,
            "unknown column `__score_0_num`"
        );

        let eight_calls: Vec<String> = ('a'..='h')
            .map(|name| format!("score('{name}') > 0"))
            .collect();
        let eight_ok =
            parse_filter_ast(FilterView::Traces, &schema, &eight_calls.join(" OR ")).unwrap();
        assert_eq!(eight_ok.scores.len(), 8);

        let nine_calls: Vec<String> = ('a'..='i')
            .map(|name| format!("score('{name}') > 0"))
            .collect();
        let nine_text = nine_calls.join(" OR ");
        let ninth_col = nine_text.find("score('i')").unwrap() as u32 + 1;
        let nine_err = parse_filter_ast(FilterView::Traces, &schema, &nine_text).unwrap_err();
        assert_eq!(nine_err.message, "a filter can use at most 8 score names");
        assert_eq!(nine_err.position, at_pos(1, ninth_col));

        // like_pattern escaping
        assert_eq!(like_pattern("100% ok_now\\"), "%100\\% ok\\_now\\\\%");

        // Privacy-safe Debug on ParsedFilter omits AST literals and score names.
        let secret_parsed = parse_filter_ast(
            FilterView::Spans,
            &schema,
            "environment = 'secret_env_literal' AND score('secret_score_name') > 0",
        )
        .unwrap();
        let dbg = format!("{secret_parsed:?}");
        assert!(dbg.contains("ParsedFilter"));
        assert!(dbg.contains("Spans"));
        assert!(dbg.contains("references_environment: true"));
        assert!(dbg.contains("scores_len: 1"));
        assert!(!dbg.contains("secret_env_literal"));
        assert!(!dbg.contains("secret_score_name"));
    }

    /// The `filter_dsl` nightly's timeout input (87 bytes, issue #189): byte 0 picks the
    /// view, the rest is a dotted chain that never completes. sqlparser 0.62 re-parsed
    /// such a chain after every `.`, so each `.` doubled the time (about 2 min for this
    /// input); 0.64 parses it once (upstream PR 2344). Both inputs are rejected, quickly.
    #[test]
    fn a_dotted_chain_that_never_completes_is_rejected_quickly() {
        const REGRESSION: &[u8] = include_bytes!(
            "../../../fuzz/regressions/filter_dsl/timeout-4a90aaeb86596f02528e6a7db1eaaca1aa253f31"
        );
        let schema = base_schema();
        let text = std::str::from_utf8(&REGRESSION[1..]).unwrap();
        let long_chain = "a.".repeat(1000);
        for text in [text, long_chain.as_str()] {
            let started = std::time::Instant::now();
            parse_filter_ast(FilterView::Spans, &schema, text).expect_err(text);
            // About 1 ms in a debug build; the old parser took minutes.
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "{text}"
            );
        }
    }
}
