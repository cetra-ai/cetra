//! SQL console parser and sandbox validator.
//!
//! Enforces single-statement read-only Shape B SQL over the registered views,
//! matching `crates/storage/src/sql/validate.rs` limits, error strings, and positions.

use std::collections::HashSet;
use std::ops::ControlFlow;

use sqlparser::ast::{
    DescribeAlias, Expr, GroupByExpr, Ident, ObjectName, ObjectNamePart, Query, Select, SetExpr,
    SetOperator, SetQuantifier, Spanned, Statement, TableFactor, Visit, Visitor, WindowFrameBound,
    WindowFrameUnits, WindowType,
};
use sqlparser::tokenizer::{Span, Token};

use crate::error::{InvalidQuery, Position};
use crate::text::{
    DENIED_FUNCTIONS, SQL_LIMITS, TextKind, expect_end, parse_failure, parser, position_of,
    tokenize,
};

use super::params::{Placeholder, bind_placeholders};

/// Maximum tables one statement may read (D-A2-1). Each `INTERSECT`, `EXCEPT`, or `MINUS`
/// counts as one more; `UNION` / `UNION ALL` do not.
pub const MAX_TABLES: usize = 32;

/// Tokenizes, rewrites `{name:Type}` placeholders, parses a single statement, and validates it
/// against the Shape B SQL sandbox and `registered` view names.
///
/// The non-`Send` `sqlparser::Parser` is dropped before returning.
pub fn parse_and_validate_sql(
    text: &str,
    registered: &HashSet<String>,
) -> Result<(Statement, Vec<Placeholder>), InvalidQuery> {
    let tokens = tokenize(text, &SQL_LIMITS, TextKind::Sql)?;
    let (tokens, placeholders) = bind_placeholders(tokens)?;
    if let Some(pipe) = tokens
        .iter()
        .find(|token| token.token == Token::VerticalBarRightAngleBracket)
    {
        return Err(InvalidQuery::new(
            "pipe syntax (`|>`) is not allowed",
            Some(position_of(pipe.span.start.line, pipe.span.start.column)),
        ));
    }

    let mut parsed = parser(tokens);
    let start = parsed.peek_token();
    let statement = parsed
        .parse_statement()
        .map_err(|error| parse_failure(error, &parsed, text))?;
    if parsed.consume_token(&Token::SemiColon) {
        let trailing = parsed.peek_token_ref();
        if !matches!(trailing.token, Token::EOF) {
            return Err(InvalidQuery::new(
                "only one statement is allowed",
                Some(position_of(
                    trailing.span.start.line,
                    trailing.span.start.column,
                )),
            ));
        }
    }
    expect_end(&mut parsed, text)?;
    drop(parsed);

    let start_pos = position_of(start.span.start.line, start.span.start.column);
    validate_statement(&statement, registered, text, Some(start_pos))?;
    Ok((statement, placeholders))
}

/// Validates an already-parsed `Statement` against the Shape B SQL sandbox and `registered` views.
pub fn validate_statement(
    statement: &Statement,
    registered: &HashSet<String>,
    text: &str,
    statement_start: Option<Position>,
) -> Result<(), InvalidQuery> {
    let query = match statement {
        Statement::Query(query) => query,
        Statement::Explain {
            describe_alias,
            analyze,
            verbose,
            query_plan,
            estimate,
            statement,
            format,
            options,
            ..
        } if *describe_alias == DescribeAlias::Explain
            && !analyze
            && !verbose
            && !query_plan
            && !estimate
            && format.is_none()
            && options.is_none() =>
        {
            let Statement::Query(query) = statement.as_ref() else {
                return Err(InvalidQuery::new(
                    "only plain EXPLAIN of a query is allowed",
                    statement_start,
                ));
            };
            query
        }
        Statement::Explain { .. } => {
            return Err(InvalidQuery::new(
                "only plain EXPLAIN of a query is allowed",
                statement_start,
            ));
        }
        _ => {
            return Err(InvalidQuery::new(
                "only SELECT queries are allowed",
                statement_start,
            ));
        }
    };

    let mut ctes = CteNames::default();
    let _ = query.visit(&mut CollectCtes { names: &mut ctes });
    let mut walk = Walk {
        registered,
        ctes,
        text,
        tables: 0,
    };
    match query.visit(&mut walk) {
        ControlFlow::Break(error) => Err(error),
        ControlFlow::Continue(()) => Ok(()),
    }
}

#[derive(Default)]
struct CteNames {
    folded: HashSet<String>,
    exact: HashSet<String>,
}

impl CteNames {
    fn insert(&mut self, ident: &Ident) {
        if ident.quote_style.is_some() {
            self.exact.insert(ident.value.clone());
        } else {
            self.folded.insert(ident.value.to_ascii_lowercase());
        }
    }

    fn contains(&self, ident: &Ident) -> bool {
        if ident.quote_style.is_some() {
            self.exact.contains(&ident.value)
        } else {
            self.folded.contains(&ident.value.to_ascii_lowercase())
        }
    }
}

struct CollectCtes<'a> {
    names: &'a mut CteNames,
}

impl Visitor for CollectCtes<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                self.names.insert(&cte.alias.name);
            }
        }
        ControlFlow::Continue(())
    }
}

struct Walk<'a> {
    registered: &'a HashSet<String>,
    ctes: CteNames,
    text: &'a str,
    tables: usize,
}

impl Walk<'_> {
    fn count_table(&mut self, span: Span) -> ControlFlow<InvalidQuery> {
        self.tables += 1;
        if self.tables > MAX_TABLES {
            return ControlFlow::Break(InvalidQuery::new(
                format!(
                    "the query reads more than {MAX_TABLES} tables (each INTERSECT or EXCEPT counts as one)"
                ),
                at(span),
            ));
        }
        ControlFlow::Continue(())
    }
}

impl Visitor for Walk<'_> {
    type Break = InvalidQuery;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        if query.with.as_ref().is_some_and(|with| with.recursive) {
            return ControlFlow::Break(InvalidQuery::new(
                "recursive queries are not allowed",
                at(query.span()),
            ));
        }
        // Set operations nest left-deep; walk iteratively so deep UNION chains never overflow stack.
        let mut pending = vec![query.body.as_ref()];
        while let Some(mut body) = pending.pop() {
            let mut chain = Vec::new();
            while let SetExpr::SetOperation {
                op,
                set_quantifier,
                left,
                right,
            } = body
            {
                chain.push((op, set_quantifier, right.as_ref()));
                body = left;
            }
            for (op, quantifier, right) in chain.into_iter().rev() {
                if matches!(
                    quantifier,
                    SetQuantifier::ByName
                        | SetQuantifier::AllByName
                        | SetQuantifier::DistinctByName
                ) {
                    return ControlFlow::Break(InvalidQuery::new(
                        "BY NAME set operations are not supported",
                        at(right.span()),
                    ));
                }
                if matches!(
                    op,
                    SetOperator::Intersect | SetOperator::Except | SetOperator::Minus
                ) {
                    if matches!(quantifier, SetQuantifier::All) {
                        return ControlFlow::Break(InvalidQuery::new(
                            "INTERSECT ALL and EXCEPT ALL are not supported",
                            at(right.span()),
                        ));
                    }
                    self.count_table(right.span())?;
                }
                pending.push(right);
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Self::Break> {
        if let Some(into) = &select.into {
            return ControlFlow::Break(InvalidQuery::new(
                "SELECT INTO is not allowed",
                at(into.name.span()),
            ));
        }
        match &select.group_by {
            GroupByExpr::All(_) => {
                return ControlFlow::Break(InvalidQuery::new(
                    "GROUP BY ALL is not supported",
                    at(select.span()),
                ));
            }
            GroupByExpr::Expressions(exprs, modifiers) => {
                if !modifiers.is_empty() {
                    return ControlFlow::Break(InvalidQuery::new(
                        "ROLLUP, CUBE, and GROUPING SETS are not supported",
                        at(select.span()),
                    ));
                }
                for expr in exprs {
                    if matches!(
                        expr,
                        Expr::Rollup(_) | Expr::Cube(_) | Expr::GroupingSets(_)
                    ) {
                        return ControlFlow::Break(InvalidQuery::new(
                            "ROLLUP, CUBE, and GROUPING SETS are not supported",
                            at(expr.span()),
                        ));
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        if !matches!(factor, TableFactor::NestedJoin { .. }) {
            self.count_table(factor.span())?;
        }
        match factor {
            TableFactor::Derived { lateral, .. } => {
                if *lateral {
                    ControlFlow::Break(InvalidQuery::new(
                        "LATERAL subqueries are not supported",
                        at(factor.span()),
                    ))
                } else {
                    ControlFlow::Continue(())
                }
            }
            TableFactor::NestedJoin { .. } | TableFactor::UNNEST { .. } => {
                ControlFlow::Continue(())
            }
            TableFactor::Table { name, args, .. } => {
                if args.is_some() {
                    return ControlFlow::Break(InvalidQuery::new(
                        "table functions are not allowed",
                        at(factor.span()),
                    ));
                }
                self.relation(name)
            }
            TableFactor::TableFunction { .. } => ControlFlow::Break(InvalidQuery::new(
                "`TABLE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::Function { .. } => ControlFlow::Break(InvalidQuery::new(
                "`FUNCTION` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::JsonTable { .. } => ControlFlow::Break(InvalidQuery::new(
                "`JSON_TABLE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::OpenJsonTable { .. } => ControlFlow::Break(InvalidQuery::new(
                "`OPENJSON` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::Pivot { .. } => ControlFlow::Break(InvalidQuery::new(
                "`PIVOT` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::Unpivot { .. } => ControlFlow::Break(InvalidQuery::new(
                "`UNPIVOT` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::MatchRecognize { .. } => ControlFlow::Break(InvalidQuery::new(
                "`MATCH_RECOGNIZE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::XmlTable { .. } => ControlFlow::Break(InvalidQuery::new(
                "`XMLTABLE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::SemanticView { .. } => ControlFlow::Break(InvalidQuery::new(
                "`SEMANTIC_VIEW` is not allowed in FROM",
                at(factor.span()),
            )),
        }
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<Self::Break> {
        if matches!(
            expr,
            Expr::Rollup(_) | Expr::Cube(_) | Expr::GroupingSets(_)
        ) {
            return ControlFlow::Break(InvalidQuery::new(
                "ROLLUP, CUBE, and GROUPING SETS are not supported",
                at(expr.span()),
            ));
        }
        let Expr::Function(function) = expr else {
            return ControlFlow::Continue(());
        };
        if let Some(name) = bare_name(&function.name) {
            if DENIED_FUNCTIONS.contains(&name.as_str()) {
                return ControlFlow::Break(InvalidQuery::new(
                    format!("function `{name}` is not available in the SQL console"),
                    at(expr.span()),
                ));
            }
            if matches!(name.as_str(), "score" | "score_label" | "has_score") {
                return ControlFlow::Break(InvalidQuery::new(
                    "score functions are only available in filters; join the scores view instead",
                    at(expr.span()),
                ));
            }
        }
        if let Some(WindowType::WindowSpec(spec)) = &function.over
            && let Some(frame) = &spec.window_frame
        {
            match frame.units {
                WindowFrameUnits::Rows => {}
                WindowFrameUnits::Groups => {
                    return ControlFlow::Break(InvalidQuery::new(
                        "GROUPS window frames are not supported",
                        at(expr.span()),
                    ));
                }
                WindowFrameUnits::Range => {
                    let is_default_range = matches!(
                        (&frame.start_bound, &frame.end_bound),
                        (
                            WindowFrameBound::Preceding(None),
                            None | Some(
                                WindowFrameBound::CurrentRow | WindowFrameBound::Following(None)
                            )
                        )
                    );
                    if !is_default_range {
                        return ControlFlow::Break(InvalidQuery::new(
                            "non-row RANGE window frames are not supported; use ROWS BETWEEN instead",
                            at(expr.span()),
                        ));
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }
}

impl Walk<'_> {
    fn relation(&self, name: &ObjectName) -> ControlFlow<InvalidQuery> {
        let [part] = name.0.as_slice() else {
            return ControlFlow::Break(InvalidQuery::new(
                "use the view name alone, without catalog or schema",
                at(name.span()),
            ));
        };
        let ObjectNamePart::Identifier(ident) = part else {
            return ControlFlow::Break(InvalidQuery::new(
                "table functions are not allowed",
                at(name.span()),
            ));
        };
        let known = if ident.quote_style.is_some() {
            self.registered.contains(&ident.value) || self.ctes.contains(ident)
        } else {
            let folded = ident.value.to_ascii_lowercase();
            self.registered.contains(&folded) || self.ctes.contains(ident)
        };
        if known {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(InvalidQuery::new(
                format!("unknown table `{}`", ident.value),
                self.located(ident),
            ))
        }
    }

    fn located(&self, ident: &Ident) -> Option<Position> {
        if ident.span.start.line != 0 {
            return at(ident.span);
        }
        let needle = match ident.quote_style {
            Some(quote) => format!("{quote}{}{quote}", ident.value),
            None => ident.value.clone(),
        };
        self.text
            .find(&needle)
            .map(|index| position_at(self.text, index))
    }
}

fn bare_name(name: &ObjectName) -> Option<String> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(ident)] => Some(ident.value.to_ascii_lowercase()),
        _ => None,
    }
}

fn position_at(text: &str, byte: usize) -> Position {
    let mut line = 1u32;
    let mut column = 1u32;
    for character in text[..byte].chars() {
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Position { line, column }
}

fn at(span: Span) -> Option<Position> {
    if span.start.line == 0 {
        None
    } else {
        Some(position_of(span.start.line, span.start.column))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered() -> HashSet<String> {
        ["spans", "traces", "scores", "sessions"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn pos(line: u32, column: u32) -> Option<Position> {
        Some(Position { line, column })
    }

    #[test]
    fn valid_queries_and_limits_match_parity() {
        let reg = registered();
        let ok = [
            "SELECT * FROM spans",
            "WITH s AS (SELECT span_id FROM spans) SELECT * FROM s UNION ALL SELECT * FROM s",
            "VALUES (1), (2)",
            "SELECT * FROM (SELECT span_id FROM spans)",
            "SELECT row_number() OVER (PARTITION BY trace_id ORDER BY start_time) FROM spans",
            "SELECT sum(duration_ns) OVER (ORDER BY start_time ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM spans",
            "SELECT sum(duration_ns) OVER (ORDER BY start_time RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM spans",
            "SELECT unnest(tags) FROM traces",
            "EXPLAIN SELECT name FROM spans",
        ];
        for sql in ok {
            parse_and_validate_sql(sql, &reg).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }

        // 512 operators allowed, 513 rejected
        let mut ops = String::from("SELECT name FROM spans WHERE true");
        for _ in 0..512 {
            ops.push_str(" OR true");
        }
        parse_and_validate_sql(&ops, &reg).unwrap();
        let col = ops.len() as u32 + 2;
        ops.push_str(" OR true");
        let err = parse_and_validate_sql(&ops, &reg).unwrap_err();
        assert_eq!(err.message, "the query has more than 512 operators");
        assert_eq!(err.position, pos(1, col));
    }

    #[test]
    fn sandbox_rejections_match_exact_messages_and_positions() {
        let reg = registered();
        let cases = [
            (
                "INSERT INTO spans VALUES (1)",
                "only SELECT queries are allowed",
                pos(1, 1),
            ),
            (
                "CREATE TABLE t (a INT)",
                "only SELECT queries are allowed",
                pos(1, 1),
            ),
            (
                "DROP TABLE spans",
                "only SELECT queries are allowed",
                pos(1, 1),
            ),
            (
                "EXPLAIN ANALYZE SELECT 1",
                "only plain EXPLAIN of a query is allowed",
                pos(1, 1),
            ),
            (
                "EXPLAIN VERBOSE SELECT 1",
                "only plain EXPLAIN of a query is allowed",
                pos(1, 1),
            ),
            (
                "SELECT 1; SELECT 2",
                "only one statement is allowed",
                pos(1, 11),
            ),
            (
                "SELECT 1 AS a |> WHERE true",
                "pipe syntax (`|>`) is not allowed",
                pos(1, 15),
            ),
            (
                "SELECT * FROM raw_spans",
                "unknown table `raw_spans`",
                pos(1, 15),
            ),
            (
                "SELECT * FROM datafusion.public.spans",
                "use the view name alone, without catalog or schema",
                pos(1, 15),
            ),
            (
                "SELECT * FROM generate_series(1, 3)",
                "table functions are not allowed",
                pos(1, 15),
            ),
            (
                "WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r",
                "recursive queries are not allowed",
                pos(1, 1),
            ),
            (
                "SELECT score('x') FROM traces",
                "score functions are only available in filters; join the scores view instead",
                pos(1, 8),
            ),
            ("SELECT 1 INTO t", "SELECT INTO is not allowed", pos(1, 15)),
            (
                "SELECT repeat('x', 3) AS a",
                "function `repeat` is not available in the SQL console",
                pos(1, 8),
            ),
            (
                "SELECT 1 AS a WHERE levenshtein('a', 'b') > 0",
                "function `levenshtein` is not available in the SQL console",
                pos(1, 21),
            ),
        ];

        for (sql, message, position) in cases {
            let err = parse_and_validate_sql(sql, &reg).expect_err(sql);
            assert_eq!(
                (err.message.as_str(), err.position),
                (message, position),
                "{sql}"
            );
        }

        // All 20 DENIED_FUNCTIONS are rejected.
        for name in DENIED_FUNCTIONS {
            let sql = format!("SELECT {name}(1)");
            let err = parse_and_validate_sql(&sql, &reg).expect_err(name);
            assert_eq!(
                err.message,
                format!("function `{name}` is not available in the SQL console")
            );
        }

        // Shape B out-of-scope constructs rejected cleanly.
        let shape_b_rejects = [
            "SELECT * FROM spans, LATERAL (SELECT 1) t",
            "SELECT name, count(*) FROM spans GROUP BY ROLLUP(name)",
            "SELECT name, count(*) FROM spans GROUP BY CUBE(name)",
            "SELECT name, count(*) FROM spans GROUP BY GROUPING SETS ((name), ())",
            "SELECT sum(duration_ns) OVER (ORDER BY start_time GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM spans",
            "SELECT sum(duration_ns) OVER (ORDER BY start_time RANGE BETWEEN 10 PRECEDING AND CURRENT ROW) FROM spans",
            "SELECT 1 INTERSECT ALL SELECT 1",
            "SELECT 1 EXCEPT ALL SELECT 1",
        ];
        for sql in shape_b_rejects {
            parse_and_validate_sql(sql, &reg).expect_err(sql);
        }
    }

    #[test]
    fn table_cap_counts_intersect_except_and_not_union() {
        let reg = registered();
        let tables_msg =
            "the query reads more than 32 tables (each INTERSECT or EXCEPT counts as one)";

        // 32 tables ok, 33 rejected
        let mut sql = String::from("SELECT 1 AS a FROM scores s0");
        for i in 1..32 {
            sql.push_str(&format!(" CROSS JOIN scores s{i}"));
        }
        parse_and_validate_sql(&sql, &reg).unwrap();
        let col = sql.len() as u32 + 13;
        sql.push_str(" CROSS JOIN scores s32");
        let err = parse_and_validate_sql(&sql, &reg).unwrap_err();
        assert_eq!(err.message, tables_msg);
        assert_eq!(err.position, pos(1, col));

        // 32 INTERSECT / EXCEPT ok, 33 rejected
        for op in ["INTERSECT", "EXCEPT"] {
            let mut s = String::from("SELECT 1 AS a");
            for _ in 0..32 {
                s.push_str(&format!(" {op} SELECT 1"));
            }
            parse_and_validate_sql(&s, &reg).unwrap();
            let c = (s.len() + op.len()) as u32 + 3;
            s.push_str(&format!(" {op} SELECT 1"));
            let e = parse_and_validate_sql(&s, &reg).unwrap_err();
            assert_eq!(e.message, tables_msg, "{op}");
            assert_eq!(e.position, pos(1, c), "{op}");
        }

        // 100 UNIONs allowed (do not count toward MAX_TABLES)
        let mut union_sql = String::from("SELECT 1 AS a");
        for _ in 0..100 {
            union_sql.push_str(" UNION SELECT 1");
        }
        parse_and_validate_sql(&union_sql, &reg).unwrap();
    }
}
