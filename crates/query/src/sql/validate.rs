//! SQL console parser and sandbox validator.
//!
//! Enforces single-statement read-only Shape B SQL over the registered views,
//! matching `crates/storage/src/sql/validate.rs` limits, error strings, and positions.

use std::collections::HashSet;

use sqlparser::ast::{
    AccessExpr, CaseWhen, ConnectByKind, DescribeAlias, Distinct, Expr, FunctionArg,
    FunctionArgExpr, FunctionArgumentClause, FunctionArguments, GroupByExpr, HavingBound, Ident,
    InTarget, Join, JoinConstraint, JoinOperator, JsonPathElem, LimitClause, ListAggOnOverflow,
    NamedWindowExpr, ObjectName, ObjectNamePart, OrderBy, OrderByExpr, OrderByKind, OrderBySort,
    Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, SetOperator,
    SetQuantifier, Spanned, Statement, Subscript, TableAlias, TableFactor, TopQuantity,
    WildcardAdditionalOptions, WindowFrame, WindowFrameBound, WindowFrameUnits, WindowSpec,
    WindowType,
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
    for token in &tokens {
        if let Token::Word(w) = &token.token {
            if w.quote_style.is_some() && w.value.is_empty() {
                return Err(InvalidQuery::new(
                    "empty identifier is not allowed",
                    Some(position_of(token.span.start.line, token.span.start.column)),
                ));
            }
            if w.value.contains('\0') {
                return Err(InvalidQuery::new(
                    "identifier must not contain NUL byte",
                    Some(position_of(token.span.start.line, token.span.start.column)),
                ));
            }
        }
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

    let ctes = collect_ctes(query);
    let mut walk = Walk {
        registered,
        ctes,
        text,
        tables: 0,
    };
    walk.run(query)
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

enum WorkItem<'a> {
    Query(&'a Query),
    SetExpr(&'a SetExpr),
    Select(&'a Select),
    TableFactor(&'a TableFactor),
    Join(&'a Join),
    Expr(&'a Expr),
    OrderBy(&'a OrderBy),
    OrderByExpr(&'a OrderByExpr),
    LimitClause(&'a LimitClause),
    WindowSpec(&'a WindowSpec),
}

fn collect_ctes(query: &Query) -> CteNames {
    let mut names = CteNames::default();
    let mut stack = vec![WorkItem::Query(query)];
    while let Some(item) = stack.pop() {
        match item {
            WorkItem::Query(q) => {
                if let Some(with) = &q.with {
                    for cte in &with.cte_tables {
                        names.insert(&cte.alias.name);
                    }
                }
                push_query_children(q, &mut stack);
            }
            WorkItem::SetExpr(mut body) => {
                let mut rhs = Vec::new();
                while let SetExpr::SetOperation { left, right, .. } = body {
                    rhs.push(right.as_ref());
                    body = left.as_ref();
                }
                for r in rhs {
                    stack.push(WorkItem::SetExpr(r));
                }
                push_set_expr_leaf_children(body, &mut stack);
            }
            WorkItem::Select(s) => push_select_children(s, &mut stack),
            WorkItem::TableFactor(tf) => push_table_factor_children(tf, &mut stack),
            WorkItem::Join(j) => push_join_children(j, &mut stack),
            WorkItem::Expr(e) => push_expr_children(e, &mut stack),
            WorkItem::OrderBy(ob) => push_order_by_children(ob, &mut stack),
            WorkItem::OrderByExpr(obe) => push_order_by_expr_children(obe, &mut stack),
            WorkItem::LimitClause(lc) => push_limit_clause_children(lc, &mut stack),
            WorkItem::WindowSpec(ws) => push_window_spec_children(ws, &mut stack),
        }
    }
    names
}

struct Walk<'a> {
    registered: &'a HashSet<String>,
    ctes: CteNames,
    text: &'a str,
    tables: usize,
}

impl<'a> Walk<'a> {
    fn count_table(&mut self, span: Span) -> Result<(), InvalidQuery> {
        self.tables += 1;
        if self.tables > MAX_TABLES {
            return Err(InvalidQuery::new(
                format!(
                    "the query reads more than {MAX_TABLES} tables (each INTERSECT or EXCEPT counts as one)"
                ),
                at(span),
            ));
        }
        Ok(())
    }

    fn run(&mut self, root: &'a Query) -> Result<(), InvalidQuery> {
        let mut stack = vec![WorkItem::Query(root)];
        while let Some(item) = stack.pop() {
            match item {
                WorkItem::Query(query) => self.visit_query(query, &mut stack)?,
                WorkItem::SetExpr(body) => self.visit_set_expr(body, &mut stack)?,
                WorkItem::Select(select) => self.visit_select(select, &mut stack)?,
                WorkItem::TableFactor(factor) => self.visit_table_factor(factor, &mut stack)?,
                WorkItem::Join(join) => self.visit_join(join, &mut stack)?,
                WorkItem::Expr(expr) => self.visit_expr(expr, &mut stack)?,
                WorkItem::OrderBy(ob) => push_order_by_children(ob, &mut stack),
                WorkItem::OrderByExpr(obe) => {
                    if matches!(obe.options.sort, Some(OrderBySort::Using(_))) {
                        return Err(InvalidQuery::new(
                            "ORDER BY ... USING is not supported",
                            at(obe.span()),
                        ));
                    }
                    push_order_by_expr_children(obe, &mut stack);
                }
                WorkItem::LimitClause(lc) => push_limit_clause_children(lc, &mut stack),
                WorkItem::WindowSpec(ws) => {
                    if let Some(wn) = &ws.window_name {
                        validate_ident(wn)?;
                    }
                    push_window_spec_children(ws, &mut stack);
                }
            }
        }
        Ok(())
    }

    fn visit_query(
        &mut self,
        query: &'a Query,
        stack: &mut Vec<WorkItem<'a>>,
    ) -> Result<(), InvalidQuery> {
        if let Some(with) = &query.with {
            if with.recursive {
                return Err(InvalidQuery::new(
                    "recursive queries are not allowed",
                    at(query.span()),
                ));
            }
            for cte in &with.cte_tables {
                validate_ident(&cte.alias.name)?;
                for col in &cte.alias.columns {
                    validate_ident(&col.name)?;
                }
            }
        }
        if !query.pipe_operators.is_empty() {
            return Err(InvalidQuery::new(
                "pipe syntax (`|>`) is not allowed",
                at(query.span()),
            ));
        }
        push_query_children(query, stack);
        Ok(())
    }

    fn visit_set_expr(
        &mut self,
        mut body: &'a SetExpr,
        stack: &mut Vec<WorkItem<'a>>,
    ) -> Result<(), InvalidQuery> {
        // Set operations nest left-deep; unwrap iteratively so deep UNION chains never overflow stack.
        let mut chain = Vec::new();
        while let SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } = body
        {
            chain.push((op, set_quantifier, right.as_ref()));
            body = left.as_ref();
        }
        // Validate in left-to-right order (matching original spine traversal).
        for &(op, quantifier, right) in chain.iter().rev() {
            if matches!(
                quantifier,
                SetQuantifier::ByName | SetQuantifier::AllByName | SetQuantifier::DistinctByName
            ) {
                return Err(InvalidQuery::new(
                    "BY NAME set operations are not supported",
                    at(right.span()),
                ));
            }
            if matches!(
                op,
                SetOperator::Intersect | SetOperator::Except | SetOperator::Minus
            ) {
                if matches!(quantifier, SetQuantifier::All) {
                    return Err(InvalidQuery::new(
                        "INTERSECT ALL and EXCEPT ALL are not supported",
                        at(right.span()),
                    ));
                }
                self.count_table(right.span())?;
            }
        }
        // Push rights in forward order (so reversed LIFO pops left-to-right), then leftmost leaf on top.
        for &(_, _, right) in &chain {
            stack.push(WorkItem::SetExpr(right));
        }
        if matches!(
            body,
            SetExpr::Insert(_)
                | SetExpr::Update(_)
                | SetExpr::Delete(_)
                | SetExpr::Merge(_)
                | SetExpr::Table(_)
        ) {
            return Err(InvalidQuery::new(
                "only SELECT queries are allowed",
                at(body.span()),
            ));
        }
        push_set_expr_leaf_children(body, stack);
        Ok(())
    }

    fn visit_select(
        &mut self,
        select: &'a Select,
        stack: &mut Vec<WorkItem<'a>>,
    ) -> Result<(), InvalidQuery> {
        if let Some(into) = &select.into {
            let pos = into
                .targets
                .first()
                .and_then(|e| at(e.span()))
                .or_else(|| at(select.span()));
            return Err(InvalidQuery::new("SELECT INTO is not allowed", pos));
        }
        match &select.group_by {
            GroupByExpr::All(_) => {
                return Err(InvalidQuery::new(
                    "GROUP BY ALL is not supported",
                    at(select.span()),
                ));
            }
            GroupByExpr::Expressions(exprs, modifiers) => {
                if !modifiers.is_empty() {
                    return Err(InvalidQuery::new(
                        "ROLLUP, CUBE, and GROUPING SETS are not supported",
                        at(select.span()),
                    ));
                }
                for expr in exprs {
                    if matches!(
                        expr,
                        Expr::Rollup(_) | Expr::Cube(_) | Expr::GroupingSets(_)
                    ) {
                        return Err(InvalidQuery::new(
                            "ROLLUP, CUBE, and GROUPING SETS are not supported",
                            at(expr.span()),
                        ));
                    }
                }
            }
        }
        for item in &select.projection {
            match item {
                SelectItem::ExprWithAlias { alias, .. } => validate_ident(alias)?,
                SelectItem::ExprWithAliases { aliases, .. } => {
                    for alias in aliases {
                        validate_ident(alias)?;
                    }
                }
                SelectItem::QualifiedWildcard(
                    SelectItemQualifiedWildcardKind::ObjectName(obj),
                    _,
                ) => validate_object_name_idents(obj)?,
                SelectItem::UnnamedExpr(_)
                | SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(_), _)
                | SelectItem::Wildcard(_) => {}
            }
        }
        for named in &select.named_window {
            validate_ident(&named.0)?;
            match &named.1 {
                NamedWindowExpr::NamedWindow(ref_ident) => validate_ident(ref_ident)?,
                NamedWindowExpr::WindowSpec(spec) => {
                    if let Some(wn) = &spec.window_name {
                        validate_ident(wn)?;
                    }
                    if let Some(frame) = &spec.window_frame {
                        validate_window_frame(frame, named.0.span)?;
                    }
                }
            }
        }
        push_select_children(select, stack);
        Ok(())
    }

    fn visit_join(
        &mut self,
        join: &'a Join,
        stack: &mut Vec<WorkItem<'a>>,
    ) -> Result<(), InvalidQuery> {
        let constraint = match &join.join_operator {
            JoinOperator::Join(c)
            | JoinOperator::Inner(c)
            | JoinOperator::Left(c)
            | JoinOperator::LeftOuter(c)
            | JoinOperator::Right(c)
            | JoinOperator::RightOuter(c)
            | JoinOperator::FullOuter(c)
            | JoinOperator::CrossJoin(c)
            | JoinOperator::Semi(c)
            | JoinOperator::LeftSemi(c)
            | JoinOperator::RightSemi(c)
            | JoinOperator::Anti(c)
            | JoinOperator::LeftAnti(c)
            | JoinOperator::RightAnti(c)
            | JoinOperator::StraightJoin(c)
            | JoinOperator::AsOf { constraint: c, .. } => Some(c),
            JoinOperator::CrossApply
            | JoinOperator::OuterApply
            | JoinOperator::ArrayJoin
            | JoinOperator::LeftArrayJoin
            | JoinOperator::InnerArrayJoin => None,
        };
        match constraint {
            Some(JoinConstraint::Using(cols)) => {
                for obj in cols {
                    validate_object_name_idents(obj)?;
                }
            }
            Some(JoinConstraint::UsingAlias(cols, alias)) => {
                for obj in cols {
                    validate_object_name_idents(obj)?;
                }
                validate_ident(alias)?;
            }
            _ => {}
        }
        push_join_children(join, stack);
        Ok(())
    }

    fn visit_table_factor(
        &mut self,
        factor: &'a TableFactor,
        stack: &mut Vec<WorkItem<'a>>,
    ) -> Result<(), InvalidQuery> {
        if !matches!(factor, TableFactor::NestedJoin { .. }) {
            self.count_table(factor.span())?;
        }
        match factor {
            TableFactor::Derived {
                lateral,
                subquery: _,
                alias,
                sample,
            } => {
                if *lateral {
                    return Err(InvalidQuery::new(
                        "LATERAL subqueries are not supported",
                        at(factor.span()),
                    ));
                }
                if sample.is_some() {
                    return Err(InvalidQuery::new(
                        "TABLESAMPLE is not supported",
                        at(factor.span()),
                    ));
                }
                validate_table_alias(alias.as_ref())?;
                push_table_factor_children(factor, stack);
                Ok(())
            }
            TableFactor::NestedJoin { alias, .. } | TableFactor::UNNEST { alias, .. } => {
                validate_table_alias(alias.as_ref())?;
                push_table_factor_children(factor, stack);
                Ok(())
            }
            TableFactor::Table {
                name,
                alias,
                args,
                with_hints,
                version,
                with_ordinality,
                partitions,
                json_path,
                sample,
                index_directives,
                inheritance,
            } => {
                if args.is_some() {
                    return Err(InvalidQuery::new(
                        "table functions are not allowed",
                        at(factor.span()),
                    ));
                }
                if !with_hints.is_empty() {
                    return Err(InvalidQuery::new(
                        "table hints are not supported",
                        at(factor.span()),
                    ));
                }
                if version.is_some() {
                    return Err(InvalidQuery::new(
                        "table time-travel qualifiers are not supported",
                        at(factor.span()),
                    ));
                }
                if *with_ordinality {
                    return Err(InvalidQuery::new(
                        "WITH ORDINALITY is not supported",
                        at(factor.span()),
                    ));
                }
                if !partitions.is_empty() {
                    return Err(InvalidQuery::new(
                        "table partition clauses are not supported",
                        at(factor.span()),
                    ));
                }
                if json_path.is_some() {
                    return Err(InvalidQuery::new(
                        "table JSON path qualifiers are not supported",
                        at(factor.span()),
                    ));
                }
                if sample.is_some() {
                    return Err(InvalidQuery::new(
                        "TABLESAMPLE is not supported",
                        at(factor.span()),
                    ));
                }
                if !index_directives.is_empty() {
                    return Err(InvalidQuery::new(
                        "index hints are not supported",
                        at(factor.span()),
                    ));
                }
                if inheritance.is_some() {
                    return Err(InvalidQuery::new(
                        "table inheritance qualifiers are not supported",
                        at(factor.span()),
                    ));
                }
                validate_table_alias(alias.as_ref())?;
                self.relation(name)
            }
            TableFactor::TableFunction { .. } => Err(InvalidQuery::new(
                "`TABLE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::Function { .. } => Err(InvalidQuery::new(
                "`FUNCTION` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::JsonTable { .. } => Err(InvalidQuery::new(
                "`JSON_TABLE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::OpenJsonTable { .. } => Err(InvalidQuery::new(
                "`OPENJSON` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::Pivot { .. } => Err(InvalidQuery::new(
                "`PIVOT` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::Unpivot { .. } | TableFactor::UnpivotExpr { .. } => Err(
                InvalidQuery::new("`UNPIVOT` is not allowed in FROM", at(factor.span())),
            ),
            TableFactor::MatchRecognize { .. } => Err(InvalidQuery::new(
                "`MATCH_RECOGNIZE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::XmlTable { .. } => Err(InvalidQuery::new(
                "`XMLTABLE` is not allowed in FROM",
                at(factor.span()),
            )),
            TableFactor::SemanticView { .. } => Err(InvalidQuery::new(
                "`SEMANTIC_VIEW` is not allowed in FROM",
                at(factor.span()),
            )),
        }
    }

    fn visit_expr(
        &mut self,
        expr: &'a Expr,
        stack: &mut Vec<WorkItem<'a>>,
    ) -> Result<(), InvalidQuery> {
        if matches!(
            expr,
            Expr::Rollup(_) | Expr::Cube(_) | Expr::GroupingSets(_)
        ) {
            return Err(InvalidQuery::new(
                "ROLLUP, CUBE, and GROUPING SETS are not supported",
                at(expr.span()),
            ));
        }
        match expr {
            Expr::Identifier(ident) => validate_ident(ident)?,
            Expr::CompoundIdentifier(parts) => {
                for ident in parts {
                    validate_ident(ident)?;
                }
            }
            Expr::QualifiedWildcard(obj, _) => validate_object_name_idents(obj)?,
            Expr::Function(function) => {
                validate_object_name_idents(&function.name)?;
                let Some(name) = bare_name(&function.name) else {
                    return Err(InvalidQuery::new(
                        "qualified function names are not supported",
                        at(expr.span()),
                    ));
                };
                if DENIED_FUNCTIONS.contains(&name.as_str()) {
                    return Err(InvalidQuery::new(
                        format!("function `{name}` is not available in the SQL console"),
                        at(expr.span()),
                    ));
                }
                if matches!(name.as_str(), "score" | "score_label" | "has_score") {
                    return Err(InvalidQuery::new(
                        "score functions are only available in filters; join the scores view instead",
                        at(expr.span()),
                    ));
                }
                match &function.over {
                    Some(WindowType::NamedWindow(ident)) => validate_ident(ident)?,
                    Some(WindowType::WindowSpec(spec)) => {
                        if let Some(wn) = &spec.window_name {
                            validate_ident(wn)?;
                        }
                        if let Some(frame) = &spec.window_frame {
                            validate_window_frame(frame, expr.span())?;
                        }
                    }
                    None => {}
                }
            }
            Expr::In { target, global, .. } => {
                if *global {
                    return Err(InvalidQuery::new(
                        "GLOBAL IN is not supported",
                        at(expr.span()),
                    ));
                }
                if matches!(target, InTarget::Table { .. }) {
                    return Err(InvalidQuery::new(
                        "table IN expressions are not supported",
                        at(expr.span()),
                    ));
                }
            }
            _ => {}
        }
        push_expr_children(expr, stack);
        Ok(())
    }
}

fn push_query_children<'a>(query: &'a Query, stack: &mut Vec<WorkItem<'a>>) {
    let Query {
        with,
        body,
        order_by,
        limit_clause,
        fetch,
        locks: _,
        for_clause: _,
        settings,
        format_clause: _,
        pipe_operators: _,
    } = query;

    if let Some(settings_list) = settings {
        for s in settings_list.iter().rev() {
            stack.push(WorkItem::Expr(&s.value));
        }
    }
    if let Some(f) = fetch
        && let Some(q) = &f.quantity
    {
        stack.push(WorkItem::Expr(q));
    }
    if let Some(lc) = limit_clause {
        stack.push(WorkItem::LimitClause(lc));
    }
    if let Some(ob) = order_by {
        stack.push(WorkItem::OrderBy(ob));
    }
    stack.push(WorkItem::SetExpr(body.as_ref()));
    if let Some(w) = with {
        for cte in w.cte_tables.iter().rev() {
            stack.push(WorkItem::Query(cte.query.as_ref()));
        }
    }
}

fn push_set_expr_leaf_children<'a>(body: &'a SetExpr, stack: &mut Vec<WorkItem<'a>>) {
    match body {
        SetExpr::Select(select) => stack.push(WorkItem::Select(select.as_ref())),
        SetExpr::Query(query) => stack.push(WorkItem::Query(query.as_ref())),
        SetExpr::SetOperation { .. } => {
            // Callers flatten `SetOperation` spines before calling `push_set_expr_leaf_children`.
            stack.push(WorkItem::SetExpr(body));
        }
        SetExpr::Values(values) => {
            for row in values.rows.iter().rev() {
                for expr in row.iter().rev() {
                    stack.push(WorkItem::Expr(expr));
                }
            }
        }
        SetExpr::Insert(_)
        | SetExpr::Update(_)
        | SetExpr::Delete(_)
        | SetExpr::Merge(_)
        | SetExpr::Table(_) => {}
    }
}

fn push_select_children<'a>(select: &'a Select, stack: &mut Vec<WorkItem<'a>>) {
    let Select {
        select_token: _,
        optimizer_hints: _,
        distinct,
        select_modifiers: _,
        top,
        top_before_distinct: _,
        projection,
        exclude: _,
        into: _,
        from,
        lateral_views,
        prewhere,
        selection,
        connect_by,
        group_by,
        cluster_by,
        distribute_by,
        sort_by,
        having,
        named_window,
        qualify,
        window_before_qualify: _,
        value_table_mode: _,
        flavor: _,
    } = select;

    if let Some(q) = qualify {
        stack.push(WorkItem::Expr(q));
    }
    for named in named_window.iter().rev() {
        if let NamedWindowExpr::WindowSpec(spec) = &named.1 {
            stack.push(WorkItem::WindowSpec(spec));
        }
    }
    if let Some(h) = having {
        stack.push(WorkItem::Expr(h));
    }
    for obe in sort_by.iter().rev() {
        stack.push(WorkItem::OrderByExpr(obe));
    }
    for e in distribute_by.iter().rev() {
        stack.push(WorkItem::Expr(e));
    }
    for e in cluster_by.iter().rev() {
        stack.push(WorkItem::Expr(e));
    }
    match group_by {
        GroupByExpr::All(_) => {}
        GroupByExpr::Expressions(exprs, _) => {
            for e in exprs.iter().rev() {
                stack.push(WorkItem::Expr(e));
            }
        }
    }
    for cb in connect_by.iter().rev() {
        match cb {
            ConnectByKind::ConnectBy {
                connect_token: _,
                nocycle: _,
                relationships,
            } => {
                for r in relationships.iter().rev() {
                    stack.push(WorkItem::Expr(r));
                }
            }
            ConnectByKind::StartWith {
                start_token: _,
                condition,
            } => {
                stack.push(WorkItem::Expr(condition.as_ref()));
            }
        }
    }
    if let Some(sel) = selection {
        stack.push(WorkItem::Expr(sel));
    }
    if let Some(pw) = prewhere {
        stack.push(WorkItem::Expr(pw));
    }
    for lv in lateral_views.iter().rev() {
        stack.push(WorkItem::Expr(&lv.lateral_view));
    }
    for twj in from.iter().rev() {
        for j in twj.joins.iter().rev() {
            stack.push(WorkItem::Join(j));
        }
        stack.push(WorkItem::TableFactor(&twj.relation));
    }
    for item in projection.iter().rev() {
        match item {
            SelectItem::UnnamedExpr(e)
            | SelectItem::ExprWithAlias { expr: e, .. }
            | SelectItem::ExprWithAliases { expr: e, .. } => {
                stack.push(WorkItem::Expr(e));
            }
            SelectItem::QualifiedWildcard(kind, opts) => {
                push_wildcard_opts_children(opts, stack);
                if let SelectItemQualifiedWildcardKind::Expr(e) = kind {
                    stack.push(WorkItem::Expr(e));
                }
            }
            SelectItem::Wildcard(opts) => {
                push_wildcard_opts_children(opts, stack);
            }
        }
    }
    if let Some(t) = top
        && let Some(TopQuantity::Expr(e)) = &t.quantity
    {
        stack.push(WorkItem::Expr(e));
    }
    if let Some(Distinct::On(exprs)) = distinct {
        for e in exprs.iter().rev() {
            stack.push(WorkItem::Expr(e));
        }
    }
}

fn push_wildcard_opts_children<'a>(
    opts: &'a WildcardAdditionalOptions,
    stack: &mut Vec<WorkItem<'a>>,
) {
    if let Some(replace) = &opts.opt_replace {
        for elem in replace.items.iter().rev() {
            stack.push(WorkItem::Expr(&elem.expr));
        }
    }
}

fn push_table_factor_children<'a>(factor: &'a TableFactor, stack: &mut Vec<WorkItem<'a>>) {
    match factor {
        TableFactor::Derived { subquery, .. } => {
            stack.push(WorkItem::Query(subquery.as_ref()));
        }
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            for j in table_with_joins.joins.iter().rev() {
                stack.push(WorkItem::Join(j));
            }
            stack.push(WorkItem::TableFactor(&table_with_joins.relation));
        }
        TableFactor::UNNEST { array_exprs, .. } => {
            for e in array_exprs.iter().rev() {
                stack.push(WorkItem::Expr(e));
            }
        }
        TableFactor::Table { .. }
        | TableFactor::TableFunction { .. }
        | TableFactor::Function { .. }
        | TableFactor::JsonTable { .. }
        | TableFactor::OpenJsonTable { .. }
        | TableFactor::Pivot { .. }
        | TableFactor::Unpivot { .. }
        | TableFactor::UnpivotExpr { .. }
        | TableFactor::MatchRecognize { .. }
        | TableFactor::XmlTable { .. }
        | TableFactor::SemanticView { .. } => {}
    }
}

fn push_join_children<'a>(join: &'a Join, stack: &mut Vec<WorkItem<'a>>) {
    let Join {
        relation,
        global: _,
        join_operator,
    } = join;
    match join_operator {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c)
        | JoinOperator::StraightJoin(c) => {
            push_join_constraint_children(c, stack);
        }
        JoinOperator::AsOf {
            match_condition,
            constraint,
        } => {
            push_join_constraint_children(constraint, stack);
            stack.push(WorkItem::Expr(match_condition));
        }
        JoinOperator::CrossApply
        | JoinOperator::OuterApply
        | JoinOperator::ArrayJoin
        | JoinOperator::LeftArrayJoin
        | JoinOperator::InnerArrayJoin => {}
    }
    stack.push(WorkItem::TableFactor(relation));
}

fn push_join_constraint_children<'a>(
    constraint: &'a JoinConstraint,
    stack: &mut Vec<WorkItem<'a>>,
) {
    match constraint {
        JoinConstraint::On(expr) => stack.push(WorkItem::Expr(expr)),
        JoinConstraint::Using(_)
        | JoinConstraint::UsingAlias(_, _)
        | JoinConstraint::Natural
        | JoinConstraint::None => {}
    }
}

fn push_order_by_children<'a>(order_by: &'a OrderBy, stack: &mut Vec<WorkItem<'a>>) {
    if let Some(interp) = &order_by.interpolate
        && let Some(exprs) = &interp.exprs
    {
        for ie in exprs.iter().rev() {
            if let Some(e) = &ie.expr {
                stack.push(WorkItem::Expr(e));
            }
        }
    }
    if let OrderByKind::Expressions(exprs) = &order_by.kind {
        for obe in exprs.iter().rev() {
            stack.push(WorkItem::OrderByExpr(obe));
        }
    }
}

fn push_order_by_expr_children<'a>(obe: &'a OrderByExpr, stack: &mut Vec<WorkItem<'a>>) {
    if let Some(wf) = &obe.with_fill {
        if let Some(step) = &wf.step {
            stack.push(WorkItem::Expr(step));
        }
        if let Some(to) = &wf.to {
            stack.push(WorkItem::Expr(to));
        }
        if let Some(from) = &wf.from {
            stack.push(WorkItem::Expr(from));
        }
    }
    stack.push(WorkItem::Expr(&obe.expr));
}

fn push_limit_clause_children<'a>(lc: &'a LimitClause, stack: &mut Vec<WorkItem<'a>>) {
    match lc {
        LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            for e in limit_by.iter().rev() {
                stack.push(WorkItem::Expr(e));
            }
            if let Some(off) = offset {
                stack.push(WorkItem::Expr(&off.value));
            }
            if let Some(lim) = limit {
                stack.push(WorkItem::Expr(lim));
            }
        }
        LimitClause::OffsetCommaLimit { offset, limit } => {
            stack.push(WorkItem::Expr(limit));
            stack.push(WorkItem::Expr(offset));
        }
    }
}

fn push_window_spec_children<'a>(spec: &'a WindowSpec, stack: &mut Vec<WorkItem<'a>>) {
    if let Some(frame) = &spec.window_frame {
        if let Some(WindowFrameBound::Preceding(Some(e)) | WindowFrameBound::Following(Some(e))) =
            &frame.end_bound
        {
            stack.push(WorkItem::Expr(e.as_ref()));
        }
        if let WindowFrameBound::Preceding(Some(e)) | WindowFrameBound::Following(Some(e)) =
            &frame.start_bound
        {
            stack.push(WorkItem::Expr(e.as_ref()));
        }
    }
    for obe in spec.order_by.iter().rev() {
        stack.push(WorkItem::OrderByExpr(obe));
    }
    for e in spec.partition_by.iter().rev() {
        stack.push(WorkItem::Expr(e));
    }
}

fn push_function_args_children<'a>(args: &'a FunctionArguments, stack: &mut Vec<WorkItem<'a>>) {
    match args {
        FunctionArguments::None => {}
        FunctionArguments::Subquery(q) => stack.push(WorkItem::Query(q.as_ref())),
        FunctionArguments::List(list) => {
            for clause in list.clauses.iter().rev() {
                match clause {
                    FunctionArgumentClause::OrderBy(obes) => {
                        for obe in obes.iter().rev() {
                            stack.push(WorkItem::OrderByExpr(obe));
                        }
                    }
                    FunctionArgumentClause::Where(e)
                    | FunctionArgumentClause::Limit(e)
                    | FunctionArgumentClause::Having(HavingBound(_, e)) => {
                        stack.push(WorkItem::Expr(e));
                    }
                    FunctionArgumentClause::OnOverflow(ListAggOnOverflow::Truncate {
                        filler: Some(e),
                        ..
                    }) => {
                        stack.push(WorkItem::Expr(e.as_ref()));
                    }
                    FunctionArgumentClause::IgnoreOrRespectNulls(_)
                    | FunctionArgumentClause::OnOverflow(_)
                    | FunctionArgumentClause::Separator(_)
                    | FunctionArgumentClause::JsonNullClause(_)
                    | FunctionArgumentClause::JsonReturningClause(_) => {}
                }
            }
            for arg in list.args.iter().rev() {
                match arg {
                    FunctionArg::Named { arg: inner, .. } | FunctionArg::Unnamed(inner) => {
                        push_function_arg_expr_children(inner, stack);
                    }
                    FunctionArg::ExprNamed {
                        name, arg: inner, ..
                    } => {
                        push_function_arg_expr_children(inner, stack);
                        stack.push(WorkItem::Expr(name));
                    }
                }
            }
        }
    }
}

fn push_function_arg_expr_children<'a>(arg: &'a FunctionArgExpr, stack: &mut Vec<WorkItem<'a>>) {
    match arg {
        FunctionArgExpr::Expr(e) => stack.push(WorkItem::Expr(e)),
        FunctionArgExpr::WildcardWithOptions(opts) => push_wildcard_opts_children(opts, stack),
        FunctionArgExpr::QualifiedWildcard(_) | FunctionArgExpr::Wildcard => {}
    }
}

fn push_expr_children<'a>(expr: &'a Expr, stack: &mut Vec<WorkItem<'a>>) {
    match expr {
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Value(_)
        | Expr::TypedString(_)
        | Expr::MatchAgainst { .. }
        | Expr::Wildcard(_)
        | Expr::QualifiedWildcard(_, _) => {}
        Expr::CompoundFieldAccess { root, access_chain } => {
            for access in access_chain.iter().rev() {
                match access {
                    AccessExpr::Dot(e) => stack.push(WorkItem::Expr(e)),
                    AccessExpr::Subscript(Subscript::Index { index }) => {
                        stack.push(WorkItem::Expr(index));
                    }
                    AccessExpr::Subscript(Subscript::Slice {
                        lower_bound,
                        upper_bound,
                        stride,
                    }) => {
                        if let Some(s) = stride {
                            stack.push(WorkItem::Expr(s));
                        }
                        if let Some(u) = upper_bound {
                            stack.push(WorkItem::Expr(u));
                        }
                        if let Some(l) = lower_bound {
                            stack.push(WorkItem::Expr(l));
                        }
                    }
                }
            }
            stack.push(WorkItem::Expr(root.as_ref()));
        }
        Expr::JsonAccess { value, path } => {
            for elem in path.path.iter().rev() {
                if let JsonPathElem::Bracket { key } | JsonPathElem::ColonBracket { key } = elem {
                    stack.push(WorkItem::Expr(key));
                }
            }
            stack.push(WorkItem::Expr(value.as_ref()));
        }
        Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner)
        | Expr::IsJson { expr: inner, .. }
        | Expr::IsNormalized { expr: inner, .. }
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::Cast { expr: inner, .. }
        | Expr::Extract { expr: inner, .. }
        | Expr::Ceil { expr: inner, .. }
        | Expr::Floor { expr: inner, .. }
        | Expr::Collate { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::Prefixed { value: inner, .. }
        | Expr::Named { expr: inner, .. }
        | Expr::OuterJoin(inner)
        | Expr::Prior(inner) => {
            stack.push(WorkItem::Expr(inner.as_ref()));
        }
        Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right)
        | Expr::BinaryOp { left, right, .. }
        | Expr::AnyOp { left, right, .. }
        | Expr::AllOp { left, right, .. }
        | Expr::AtTimeZone {
            timestamp: left,
            time_zone: right,
        }
        | Expr::Position {
            expr: left,
            r#in: right,
        } => {
            stack.push(WorkItem::Expr(right.as_ref()));
            stack.push(WorkItem::Expr(left.as_ref()));
        }
        Expr::In { expr, target, .. } => {
            match target {
                InTarget::List(list) => {
                    for item in list.iter().rev() {
                        stack.push(WorkItem::Expr(item));
                    }
                }
                InTarget::Subquery(subquery) => {
                    stack.push(WorkItem::Query(subquery.as_ref()));
                }
                InTarget::Unnest(array_expr) => {
                    stack.push(WorkItem::Expr(array_expr.as_ref()));
                }
                InTarget::Table { .. } => {}
            }
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            stack.push(WorkItem::Expr(high.as_ref()));
            stack.push(WorkItem::Expr(low.as_ref()));
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::Like {
            expr,
            pattern,
            escape_char,
            ..
        }
        | Expr::ILike {
            expr,
            pattern,
            escape_char,
            ..
        }
        | Expr::SimilarTo {
            expr,
            pattern,
            escape_char,
            ..
        } => {
            if let Some(esc) = escape_char {
                stack.push(WorkItem::Expr(esc.as_ref()));
            }
            stack.push(WorkItem::Expr(pattern.as_ref()));
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::RLike { expr, pattern, .. } => {
            stack.push(WorkItem::Expr(pattern.as_ref()));
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::Convert { expr, styles, .. } => {
            for s in styles.iter().rev() {
                stack.push(WorkItem::Expr(s));
            }
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            if let Some(f) = substring_for {
                stack.push(WorkItem::Expr(f.as_ref()));
            }
            if let Some(f) = substring_from {
                stack.push(WorkItem::Expr(f.as_ref()));
            }
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::Trim {
            trim_what,
            expr,
            trim_characters,
            ..
        } => {
            if let Some(chars) = trim_characters {
                for c in chars.iter().rev() {
                    stack.push(WorkItem::Expr(c));
                }
            }
            if let Some(tw) = trim_what {
                stack.push(WorkItem::Expr(tw.as_ref()));
            }
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            if let Some(f) = overlay_for {
                stack.push(WorkItem::Expr(f.as_ref()));
            }
            stack.push(WorkItem::Expr(overlay_from.as_ref()));
            stack.push(WorkItem::Expr(overlay_what.as_ref()));
            stack.push(WorkItem::Expr(expr.as_ref()));
        }
        Expr::Function(f) => {
            if let Some(WindowType::WindowSpec(spec)) = &f.over {
                stack.push(WorkItem::WindowSpec(spec));
            }
            for obe in f.within_group.iter().rev() {
                stack.push(WorkItem::OrderByExpr(obe));
            }
            if let Some(filter) = &f.filter {
                stack.push(WorkItem::Expr(filter.as_ref()));
            }
            push_function_args_children(&f.args, stack);
            push_function_args_children(&f.parameters, stack);
        }
        Expr::Case {
            case_token: _,
            end_token: _,
            operand,
            conditions,
            else_result,
        } => {
            if let Some(el) = else_result {
                stack.push(WorkItem::Expr(el.as_ref()));
            }
            for CaseWhen { condition, result } in conditions.iter().rev() {
                stack.push(WorkItem::Expr(result));
                stack.push(WorkItem::Expr(condition));
            }
            if let Some(op) = operand {
                stack.push(WorkItem::Expr(op.as_ref()));
            }
        }
        Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => {
            stack.push(WorkItem::Query(subquery.as_ref()));
        }
        Expr::GroupingSets(groups) | Expr::Cube(groups) | Expr::Rollup(groups) => {
            for g in groups.iter().rev() {
                for e in g.iter().rev() {
                    stack.push(WorkItem::Expr(e));
                }
            }
        }
        Expr::Tuple(exprs) | Expr::Struct { values: exprs, .. } => {
            for e in exprs.iter().rev() {
                stack.push(WorkItem::Expr(e));
            }
        }
        Expr::Dictionary(fields) => {
            for f in fields.iter().rev() {
                stack.push(WorkItem::Expr(f.value.as_ref()));
            }
        }
        Expr::Map(map) => {
            for entry in map.entries.iter().rev() {
                stack.push(WorkItem::Expr(entry.value.as_ref()));
                stack.push(WorkItem::Expr(entry.key.as_ref()));
            }
        }
        Expr::Array(arr) => {
            for e in arr.elem.iter().rev() {
                stack.push(WorkItem::Expr(e));
            }
        }
        Expr::Interval(iv) => {
            stack.push(WorkItem::Expr(iv.value.as_ref()));
        }
        Expr::Lambda(lf) => {
            stack.push(WorkItem::Expr(lf.body.as_ref()));
        }
        Expr::MemberOf(mo) => {
            stack.push(WorkItem::Expr(mo.array.as_ref()));
            stack.push(WorkItem::Expr(mo.value.as_ref()));
        }
    }
}

impl Walk<'_> {
    fn relation(&self, name: &ObjectName) -> Result<(), InvalidQuery> {
        let [part] = name.0.as_slice() else {
            return Err(InvalidQuery::new(
                "use the view name alone, without catalog or schema",
                at(name.span()),
            ));
        };
        let ObjectNamePart::Identifier(ident) = part else {
            return Err(InvalidQuery::new(
                "table functions are not allowed",
                at(name.span()),
            ));
        };
        validate_ident(ident)?;
        let known = if ident.quote_style.is_some() {
            self.registered.contains(&ident.value) || self.ctes.contains(ident)
        } else {
            let folded = ident.value.to_ascii_lowercase();
            self.registered.contains(&folded) || self.ctes.contains(ident)
        };
        if known {
            Ok(())
        } else {
            Err(InvalidQuery::new(
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

fn validate_ident(ident: &Ident) -> Result<(), InvalidQuery> {
    if ident.value.is_empty() {
        return Err(InvalidQuery::new(
            "empty identifier is not allowed",
            at(ident.span),
        ));
    }
    if ident.value.contains('\0') {
        return Err(InvalidQuery::new(
            "identifier must not contain NUL byte",
            at(ident.span),
        ));
    }
    Ok(())
}

fn validate_table_alias(alias: Option<&TableAlias>) -> Result<(), InvalidQuery> {
    if let Some(ta) = alias {
        validate_ident(&ta.name)?;
        for col in &ta.columns {
            validate_ident(&col.name)?;
        }
    }
    Ok(())
}

fn validate_object_name_idents(name: &ObjectName) -> Result<(), InvalidQuery> {
    for part in &name.0 {
        if let ObjectNamePart::Identifier(ident) = part {
            validate_ident(ident)?;
        }
    }
    Ok(())
}

fn validate_window_frame(frame: &WindowFrame, span: Span) -> Result<(), InvalidQuery> {
    match frame.units {
        WindowFrameUnits::Rows => Ok(()),
        WindowFrameUnits::Groups => Err(InvalidQuery::new(
            "GROUPS window frames are not supported",
            at(span),
        )),
        WindowFrameUnits::Range => {
            let is_default_range = matches!(
                (&frame.start_bound, &frame.end_bound),
                (
                    WindowFrameBound::Preceding(None),
                    None | Some(WindowFrameBound::CurrentRow | WindowFrameBound::Following(None))
                )
            );
            if !is_default_range {
                Err(InvalidQuery::new(
                    "non-row RANGE window frames are not supported; use ROWS BETWEEN instead",
                    at(span),
                ))
            } else {
                Ok(())
            }
        }
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
            "SELECT sum(duration_ns) OVER w FROM spans WINDOW w AS (ORDER BY start_time GROUPS BETWEEN 1 PRECEDING AND CURRENT ROW)",
            "SELECT sum(duration_ns) OVER w FROM spans WINDOW w AS (ORDER BY start_time RANGE BETWEEN 10 PRECEDING AND CURRENT ROW)",
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

    #[test]
    fn exhaustive_nested_containers_table_modifiers_and_qualified_fns_rejected() {
        let reg = registered();
        let nested_rejects = [
            // Unknown table inside CTE, subquery, CASE, FILTER, OVER
            "WITH bad AS (SELECT * FROM secret_tbl) SELECT * FROM spans",
            "SELECT (SELECT count(*) FROM secret_tbl) FROM spans",
            "SELECT * FROM spans WHERE trace_id IN (SELECT trace_id FROM secret_tbl)",
            "SELECT * FROM spans WHERE EXISTS (SELECT 1 FROM secret_tbl)",
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM secret_tbl) THEN 1 ELSE 0 END FROM spans",
            "SELECT count(*) FILTER (WHERE EXISTS (SELECT 1 FROM secret_tbl)) FROM spans",
            "SELECT sum(duration_ns) OVER (PARTITION BY (SELECT 1 FROM secret_tbl)) FROM spans",
            // Denied functions inside CTE, CASE, FILTER, OVER, JOIN ON
            "WITH c AS (SELECT repeat('a', 2) AS x FROM spans) SELECT x FROM c",
            "SELECT CASE WHEN true THEN levenshtein('a', 'b') ELSE 0 END FROM spans",
            "SELECT count(*) FILTER (WHERE repeat('x', 2) = 'y') FROM spans",
            "SELECT sum(duration_ns) OVER (ORDER BY length(repeat('a', 2))) FROM spans",
            "SELECT * FROM spans a JOIN spans b ON repeat(a.name, 2) = b.name",
            // score* functions inside nested positions
            "WITH c AS (SELECT score('x') AS s FROM traces) SELECT s FROM c",
            "SELECT CASE WHEN score('x') > 0.5 THEN 1 ELSE 0 END FROM traces",
            "SELECT count(*) FILTER (WHERE has_score('x')) FROM traces",
            "SELECT count(*) OVER (ORDER BY score_label('x')) FROM traces",
            // RANGE / GROUPS window frames in OVER and named WINDOW
            "SELECT sum(duration_ns) OVER (ORDER BY start_time RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM spans",
            "SELECT sum(duration_ns) OVER (ORDER BY start_time GROUPS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM spans",
            // Window frame bound expressions (subquery / denied function)
            "SELECT sum(duration_ns) OVER (ORDER BY start_time ROWS BETWEEN (SELECT count(*) FROM secret_tbl) PRECEDING AND CURRENT ROW) FROM spans",
            "SELECT sum(duration_ns) OVER (ORDER BY start_time ROWS BETWEEN repeat('a', 2) PRECEDING AND CURRENT ROW) FROM spans",
            // Non-SELECT SetExpr (TABLE / DML in CTE)
            "TABLE secret_tbl",
            "WITH c AS (DELETE FROM secret_tbl RETURNING *) SELECT 1",
            // MAX_TABLES exceeded across CTE + subqueries + joins
            &format!(
                "WITH c AS (SELECT 1 FROM {}) SELECT * FROM c CROSS JOIN spans",
                vec!["spans"; 32].join(" CROSS JOIN ")
            ),
            // Qualified function names
            "SELECT public.lower(name) FROM spans",
            "SELECT pg_catalog.count(*) FROM spans",
            "SELECT sum(pg_catalog.abs(duration_ns)) OVER () FROM spans",
            // TableFactor::Table and Derived modifiers
            "SELECT * FROM spans TABLESAMPLE BERNOULLI (10)",
            "SELECT * FROM (SELECT * FROM spans) t TABLESAMPLE SYSTEM (10)",
            "SELECT * FROM spans WITH (NOLOCK)",
            "SELECT * FROM spans PARTITION (p0)",
            "SELECT * FROM spans USE INDEX (idx_name)",
            "SELECT * FROM spans FOR SYSTEM_TIME AS OF '2026-01-01'",
        ];
        for sql in nested_rejects {
            parse_and_validate_sql(sql, &reg).expect_err(sql);
        }

        let err = parse_and_validate_sql("SELECT public.lower(name) FROM spans", &reg).unwrap_err();
        assert_eq!(err.message, "qualified function names are not supported");
    }

    #[test]
    fn validates_512_op_binary_and_512_way_union_all_on_2mib_stack() {
        let reg = registered();
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let expr_512 = format!("SELECT {} FROM spans", vec!["1"; 513].join(" + "));
                parse_and_validate_sql(&expr_512, &reg).unwrap();

                let union_512 = vec!["SELECT 1 AS x"; 512].join(" UNION ALL ");
                parse_and_validate_sql(&union_512, &reg).unwrap();
            })
            .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn rejects_empty_and_nul_identifiers_in_parser_and_ast_validator() {
        let reg = registered();
        let empty_cases = [
            r#"SELECT 1 AS """#,
            r#"WITH "" AS (SELECT 1 AS x) SELECT * FROM spans"#,
            r#"WITH c("") AS (SELECT 1 AS x) SELECT * FROM c"#,
            r#"SELECT * FROM spans AS """#,
            r#"SELECT * FROM (SELECT 1 AS x) AS t("")"#,
            r#"SELECT * FROM spans a JOIN traces b USING ("")"#,
            r#"SELECT row_number() OVER "" FROM spans WINDOW "" AS (ORDER BY start_time)"#,
            r#"SELECT "" FROM spans"#,
            r#"SELECT spans."" FROM spans"#,
            r#"SELECT ""(name) FROM spans"#,
        ];
        for sql in empty_cases {
            let err = parse_and_validate_sql(sql, &reg).expect_err(sql);
            assert!(
                err.message.contains("empty identifier"),
                "expected empty identifier error for `{sql}`, got: {}",
                err.message
            );
        }

        let nul_cases = [
            "SELECT 1 AS \"a\0b\"",
            "WITH \"a\0b\" AS (SELECT 1 AS x) SELECT * FROM spans",
            "WITH c(\"a\0b\") AS (SELECT 1 AS x) SELECT * FROM c",
            "SELECT * FROM spans AS \"a\0b\"",
            "SELECT * FROM (SELECT 1 AS x) AS t(\"a\0b\")",
            "SELECT * FROM spans a JOIN traces b USING (\"a\0b\")",
            "SELECT row_number() OVER \"a\0b\" FROM spans WINDOW \"a\0b\" AS (ORDER BY start_time)",
            "SELECT \"a\0b\" FROM spans",
            "SELECT spans.\"a\0b\" FROM spans",
            "SELECT \"a\0b\"(name) FROM spans",
        ];
        for sql in nul_cases {
            let err = parse_and_validate_sql(sql, &reg).expect_err(sql);
            assert!(
                err.message.contains("NUL byte"),
                "expected NUL byte error for `{sql}`, got: {}",
                err.message
            );
        }

        // Direct `validate_statement` path (bypassing token pre-check) also rejects empty & NUL idents.
        let mut parsed =
            parser(tokenize("SELECT 1 AS a FROM spans", &SQL_LIMITS, TextKind::Sql).unwrap());
        let mut stmt = parsed.parse_statement().unwrap();
        if let Statement::Query(ref mut q) = stmt
            && let SetExpr::Select(ref mut sel) = *q.body
            && let Some(SelectItem::ExprWithAlias { alias, .. }) = sel.projection.first_mut()
        {
            *alias = Ident::with_quote('"', String::new());
        }
        let err = validate_statement(&stmt, &reg, "", None).unwrap_err();
        assert_eq!(err.message, "empty identifier is not allowed");

        if let Statement::Query(ref mut q) = stmt
            && let SetExpr::Select(ref mut sel) = *q.body
            && let Some(SelectItem::ExprWithAlias { alias, .. }) = sel.projection.first_mut()
        {
            *alias = Ident::with_quote('"', "\0__cetra_agg_0".to_owned());
        }
        let err = validate_statement(&stmt, &reg, "", None).unwrap_err();
        assert_eq!(err.message, "identifier must not contain NUL byte");
    }

    #[test]
    fn enforces_sandbox_on_sqlparser_064_ast_constructs() {
        let reg = registered();
        let reject_cases = [
            // Denied function inside LIKE ESCAPE expression
            (
                "SELECT name FROM spans WHERE name LIKE '%a%' ESCAPE repeat('x', 1)",
                "not available",
            ),
            // Denied function inside inline aggregate WHERE clause
            (
                "SELECT count(name WHERE repeat('x', 1) = 'x') FROM spans",
                "not available",
            ),
            // Denied function inside IS JSON expression
            ("SELECT repeat('x', 1) IS JSON FROM spans", "not available"),
            // Table inheritance marker (ONLY)
            ("SELECT name FROM ONLY spans", "table inheritance"),
            // SQLite table IN expression (or rejected by GenericDialect parser)
            ("SELECT name FROM spans WHERE name IN traces", ""),
            // ClickHouse GLOBAL IN (or rejected by GenericDialect parser)
            ("SELECT name FROM spans WHERE name GLOBAL IN ('a')", ""),
            // ORDER BY ... USING <op> (rejected by GenericDialect parser)
            ("SELECT name FROM spans ORDER BY name USING <", ""),
        ];
        for (sql, expected) in reject_cases {
            let err = parse_and_validate_sql(sql, &reg).expect_err(sql);
            if !expected.is_empty() {
                assert!(
                    err.message.contains(expected),
                    "expected `{expected}` for `{sql}`, got: {}",
                    err.message
                );
            }
        }

        // Direct AST validation also rejects OrderBySort::Using even if constructed programmatically.
        let mut parsed = parser(
            tokenize(
                "SELECT name FROM spans ORDER BY name ASC",
                &SQL_LIMITS,
                TextKind::Sql,
            )
            .unwrap(),
        );
        let mut stmt = parsed.parse_statement().unwrap();
        if let Statement::Query(ref mut q) = stmt
            && let Some(ref mut ob) = q.order_by
            && let OrderByKind::Expressions(ref mut exprs) = ob.kind
            && let Some(first) = exprs.first_mut()
        {
            first.options.sort = Some(OrderBySort::Using(ObjectName(vec![
                ObjectNamePart::Identifier(Ident::new("<")),
            ])));
        }
        let err = validate_statement(&stmt, &reg, "", None).unwrap_err();
        assert!(
            err.message.contains("ORDER BY ... USING"),
            "expected ORDER BY ... USING error, got: {}",
            err.message
        );
    }

    /// A long dotted name that never completes (issue #189): sqlparser 0.62 took time
    /// doubling with each `.`; 0.64 rejects it at once.
    #[test]
    fn a_dotted_chain_that_never_completes_is_rejected_quickly() {
        let sql = format!("SELECT {}", "a.".repeat(4000));
        let started = std::time::Instant::now();
        parse_and_validate_sql(&sql, &registered()).unwrap_err();
        // A few ms in a debug build; the old parser never finished.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
